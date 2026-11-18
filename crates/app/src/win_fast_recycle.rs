//! Fast Recycle Bin (on by default, can be turned off in settings; #9).
//!
//! Sending a big folder to the Recycle Bin through the Windows engine (`IFileOperation`, or the
//! older `SHFileOperation`, the same speed) first visits every file, and with real-time antivirus
//! each one is scanned: about 40 s for 20,000 small files. Yet on the same drive the bin only
//! needs the item renamed into `X:\$Recycle.Bin\<SID>\` plus a small `$I` record next to it,
//! which is what Windows writes too. This does exactly that, and only where it is safe:
//!
//! - A local fixed NTFS drive whose bin folder for this user already exists (Windows creates it
//!   with the right permissions on first use; until then the engine does the work).
//! - The drive's bin is not set to delete right away, and this item fits in its maximum size
//!   with what is already there (Windows would otherwise ask or drop older items).
//! - No Group Policy turns the bin off or caps its size (the engine applies those).
//! - The `$I` records already in the bin are all version 2, the layout written here.
//! - The rename succeeds: a file in use inside the folder makes it fail, nothing has moved, and
//!   the item goes through the engine instead.
//!
//! Everything else returns `None` and the caller uses the Windows engine. The size is summed from
//! directory listings only, so no file is opened (and scanned). The `$I` layout (version 2) was
//! checked byte for byte against records Windows wrote (see the tests):
//!
//! | offset | size | value |
//! |---|---|---|
//! | 0 | 8 | version, 2 |
//! | 8 | 8 | size in bytes; a folder's is the sum of its files |
//! | 16 | 8 | deletion time, FILETIME (UTC) |
//! | 24 | 4 | length of the path in UTF-16 units, terminator included |
//! | 28 | .. | original full path, UTF-16, NUL-terminated |
//!
//! Item names are `$I` / `$R`, six random letters or digits, then the original extension.

use std::fs;
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The setting, mirrored for the file operation threads.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// `$I` layout version written and accepted.
const VERSION: i64 = 2;
/// `$I` records looked at to check the bin's layout.
const PROBE_RECORDS: usize = 8;

/// Puts `path` in its drive's Recycle Bin directly. Returns where it went (the `$R` item), or
/// `None` when this path isn't eligible or anything failed before it moved, so the Windows
/// engine should handle it. One item as a batch of its own (Arke deletes through [`Batch`]).
#[cfg(test)]
pub(crate) fn recycle(path: &Path) -> Option<PathBuf> {
    let mut batch = Batch::default();
    let item = batch.recycle(path);
    batch.finish();
    item
}

/// Most items reported to Explorer one by one; past this a batch reports each folder they left
/// once instead.
const NOTIFY_EACH_MAX: usize = 64;

/// One delete of any number of items. What doesn't change from item to item is checked once:
/// Group Policy and the user for the batch; per drive the file system, the bin folder, its `$I`
/// layout, its settings and what it holds. `SHQueryRecycleBinW` lists the whole bin, so asking it
/// for every item made a delete of many loose files quadratic; here it is asked once per drive,
/// and once more to confirm Windows counted the first item moved there. The bytes in the bin are
/// then kept up to date locally.
///
/// What is left per item is creating its `$I` record and the rename; real-time antivirus sets
/// their cost (0.2 ms each with Defender, about 2 ms with another product, measured). Items go
/// one at a time: on 4 threads they were no faster with one and ten times slower with the other
/// (they all change the same bin folder).
#[derive(Default)]
pub(crate) struct Batch {
    /// This user's SID, if Group Policy leaves the bin to Arke; looked up when first needed.
    user: std::cell::OnceCell<Option<String>>,
    /// Per drive root; `None` for a drive whose bin stays with the engine.
    drives: std::collections::HashMap<PathBuf, Option<Drive>>,
    /// Items moved, and whether each was a folder, for Explorer's change notifications.
    moved: Vec<(PathBuf, bool)>,
}

struct Drive {
    bin: PathBuf,
    /// The bin's maximum size, MB.
    max_mb: u32,
    /// Bytes in the bin: what Windows reported at the start, plus what this batch added.
    used: u64,
    /// Items Windows counted at the start, until it has been seen counting one moved here.
    unconfirmed: Option<u64>,
}

impl Batch {
    /// Like [`recycle`], for one item of the batch.
    pub(crate) fn recycle(&mut self, path: &Path) -> Option<PathBuf> {
        let sid = self
            .user
            .get_or_init(|| policy_allows().then(user_sid).flatten())
            .clone()?;

        let root = drive_root(path)?;
        let drive = self
            .drives
            .entry(root.clone())
            .or_insert_with(|| Drive::check(&root, &sid))
            .as_mut()?;
        let meta = fs::symlink_metadata(path).ok()?;
        // Links, junctions and cloud placeholders (OneDrive and the like keep a reparse point on
        // every item they sync) stay with the engine, which knows what each of them needs.
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return None;
        }
        let size = if meta.is_dir() {
            tree_size(path)?
        } else {
            meta.len()
        };
        if !has_room(drive.used, size, drive.max_mb) {
            return None;
        }
        let ext = bin_extension(path);
        let (info, item) = free_names(&drive.bin, &ext)?;
        let record = encode(
            VERSION,
            size,
            now_filetime(),
            &arke_core::path::unverbatim(path),
        );
        // The whole record must be in place before anything moves: after it is closed (when
        // antivirus may still take it away) it is still there at full length. Only its metadata
        // is read: reading it back opened it again right after it was written, which real-time
        // antivirus scans before letting the read through (with Defender two thirds of each
        // item), and it only read the cache anyway.
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info)
            .and_then(|mut f| f.write_all(&record));
        let in_place = || fs::symlink_metadata(&info).is_ok_and(|m| m.len() == record.len() as u64);
        if written.is_err() || !in_place() {
            let _ = fs::remove_file(&info);
            return None;
        }
        if let Err(err) = move_no_replace(
            &arke_core::path::verbatim(path),
            &arke_core::path::verbatim(&item),
        ) {
            log::info!(
                "fast recycle: {} stays with the engine: {err}",
                path.display()
            );
            let _ = fs::remove_file(&info);
            return None;
        }
        // The first item moved to a drive's bin: Windows must count it. If it doesn't, put it
        // back, and this item and every later one on the drive go to the engine. (A crash
        // between the record and the move leaves the item where it was and an `$I` with no
        // item, which Windows lists as nothing to restore; no data moves without a record.)
        if let Some(count_before) = drive.unconfirmed {
            if bin_state(&root).is_none_or(|(_, count)| count <= count_before) {
                log::warn!(
                    "fast recycle: Windows didn't count {}; put back",
                    path.display()
                );
                let back = move_no_replace(
                    &arke_core::path::verbatim(&item),
                    &arke_core::path::verbatim(path),
                );
                if back.is_ok() {
                    let _ = fs::remove_file(&info);
                    self.drives.insert(root, None);
                    return None;
                }
            }
            drive.unconfirmed = None;
        }
        drive.used = drive.used.saturating_add(size);
        self.moved.push((path.to_path_buf(), meta.is_dir()));
        #[cfg(test)]
        TAKEN.fetch_add(1, Ordering::Relaxed);
        Some(item)
    }

    /// Tells Explorer windows and the desktop icon what changed. Call once, after the last item.
    pub(crate) fn finish(self) {
        if self.moved.is_empty() {
            return;
        }
        let bins: Vec<&Path> = self
            .drives
            .values()
            .flatten()
            .filter(|d| d.unconfirmed.is_none())
            .map(|d| d.bin.as_path())
            .collect();
        notify(&self.moved, &bins);
    }
}

impl Drive {
    /// The drive's bin, if it can take items directly; `None` leaves the drive to the engine.
    fn check(root: &Path, sid: &str) -> Option<Self> {
        if !local_ntfs(root) {
            return None;
        }
        let bin = root.join("$Recycle.Bin").join(sid);
        if !bin.is_dir() || !layout_is_known(&bin) {
            return None;
        }
        let (max_mb, nuke) = bin_settings(root)?;
        if nuke {
            return None;
        }
        let (used, count) = bin_state(root)?;
        Some(Self {
            bin,
            max_mb,
            used,
            unconfirmed: Some(count),
        })
    }
}

/// The part of the name the bin item keeps: from the last dot on, as Windows takes it, so a name
/// that only starts with a dot (`.gitignore`) keeps all of it.
fn bin_extension(path: &Path) -> String {
    let name = path
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned());
    name.rfind('.').map_or_default(|at| name[at..].to_owned())
}

/// Renames `from` to `to` on the same volume, never replacing anything at `to` (unlike
/// `fs::rename`) and never copying.
fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
    let from = windows::core::HSTRING::from(from.as_os_str());
    let to = windows::core::HSTRING::from(to.as_os_str());
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe { MoveFileExW(&from, &to, MOVE_FILE_FLAGS(0)) }
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

/// `X:\` of a drive-letter path; `None` for anything else (shares, device paths).
fn drive_root(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(PathBuf::from(format!(r"{}:\", char::from(letter))))
            }
            _ => None,
        },
        _ => None,
    }
}

/// A fixed drive formatted NTFS. Local mount information only, no network.
fn local_ntfs(root: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
    const DRIVE_FIXED: u32 = 3;
    let root = windows::core::HSTRING::from(root.as_os_str());
    let mut fs_name = [0u16; 32];
    // SAFETY: root is NUL-terminated; the name buffer outlives the calls.
    let named = unsafe {
        GetDriveTypeW(&root) == DRIVE_FIXED
            && GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs_name)).is_ok()
    };
    named
        && String::from_utf16_lossy(&fs_name)
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("NTFS")
}

/// This user's SID as text, the name of their bin folder.
fn user_sid() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: plain Win32 calls; the token handle is closed and the string freed below, and the
    // buffer holds the TOKEN_USER the second call writes.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &raw mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let got = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &raw mut len,
        );
        let _ = CloseHandle(token);
        got.ok()?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut text = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &raw mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

/// The `$I` records already there are all the version written here (or there are none yet).
fn layout_is_known(bin: &Path) -> bool {
    let Ok(entries) = fs::read_dir(bin) else {
        return false;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("$I"))
        .take(PROBE_RECORDS)
        .all(|e| {
            fs::read(e.path())
                .ok()
                .and_then(|bytes| decode(&bytes))
                .is_some_and(|record| record.version == VERSION)
        })
}

/// Total size of the files below `dir`, from directory listings (no file is opened). Links are
/// not followed. `None` if part of the tree can't be listed.
fn tree_size(dir: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut pending = vec![arke_core::path::verbatim(dir)];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let meta = entry.metadata().ok()?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                total += meta.file_size();
            }
        }
    }
    Some(total)
}

/// `size` more bytes fit in a bin holding `used` bytes of at most `max_mb` megabytes.
fn has_room(used: u64, size: u64, max_mb: u32) -> bool {
    used.saturating_add(size) <= u64::from(max_mb).saturating_mul(1024 * 1024)
}

/// `MaxCapacity` (MB) and `NukeOnDelete` of the drive's bin, from
/// `HKCU\...\Explorer\BitBucket\Volume\{volume GUID}`. Read only.
fn bin_settings(root: &Path) -> Option<(u32, bool)> {
    use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    let mount = windows::core::HSTRING::from(root.as_os_str());
    let mut name = [0u16; 64];
    // SAFETY: mount is NUL-terminated; the buffer outlives the call.
    unsafe { GetVolumeNameForVolumeMountPointW(&mount, &mut name) }.ok()?;
    let name = String::from_utf16_lossy(&name);
    // `\\?\Volume{GUID}\` → `{GUID}`
    let guid = &name[name.find('{')?..=name.find('}')?];
    let key =
        format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}");
    let max = read_dword(&key, "MaxCapacity")?;
    let nuke = read_dword(&key, "NukeOnDelete").unwrap_or(0) != 0;
    Some((max, nuke))
}

fn read_dword(key: &str, value: &str) -> Option<u32> {
    read_dword_in(
        windows::Win32::System::Registry::HKEY_CURRENT_USER,
        key,
        value,
    )
}

fn read_dword_in(
    hive: windows::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<u32> {
    use windows::Win32::System::Registry::{RRF_RT_REG_DWORD, RegGetValueW};
    let key = windows::core::HSTRING::from(key);
    let value = windows::core::HSTRING::from(value);
    let mut data = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: a read of one DWORD into `data`; strings are NUL-terminated.
    unsafe {
        RegGetValueW(
            hive,
            &key,
            &value,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&raw mut len),
        )
    }
    .ok()
    .ok()?;
    Some(data)
}

/// Group Policy over the bin, user and machine: "Do not move deleted files to the Recycle Bin"
/// (`NoRecycleFiles`) and "Maximum allowed Recycle Bin size" (`RecycleBinSize`, a share of the
/// drive), both under `...\CurrentVersion\Policies\Explorer`. Read only.
fn policy_allows() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .all(|hive| {
            !policy_blocks(
                read_dword_in(hive, KEY, "NoRecycleFiles"),
                read_dword_in(hive, KEY, "RecycleBinSize"),
            )
        })
}

/// Whether these policy values leave the bin to the engine: no bin at all, or a size cap the
/// engine applies (the bin's own maximum, which the fast path checks, may not reflect it).
fn policy_blocks(no_recycle_files: Option<u32>, recycle_bin_size: Option<u32>) -> bool {
    no_recycle_files.is_some_and(|v| v != 0) || recycle_bin_size.is_some()
}

/// Bytes and items in the drive's bin, as Windows counts them.
fn bin_state(root: &Path) -> Option<(u64, u64)> {
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    let root = windows::core::HSTRING::from(root.as_os_str());
    let mut info = SHQUERYRBINFO {
        cbSize: size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    #[cfg(test)]
    BIN_QUERIES.fetch_add(1, Ordering::Relaxed);
    // SAFETY: root is NUL-terminated; info is a properly sized out struct.
    unsafe { SHQueryRecycleBinW(&root, &raw mut info) }.ok()?;
    Some((
        u64::try_from(info.i64Size).ok()?,
        u64::try_from(info.i64NumItems).ok()?,
    ))
}

/// Unused `$I`/`$R` names in `bin` for an item with extension `ext`.
fn free_names(bin: &Path, ext: &str) -> Option<(PathBuf, PathBuf)> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64
        ^ u64::from(std::process::id()).rotate_left(32);
    for _ in 0..32 {
        let mut tag = String::with_capacity(6);
        for _ in 0..6 {
            // xorshift: names only need to differ, not be unpredictable.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            tag.push(char::from(CHARS[(seed % CHARS.len() as u64) as usize]));
        }
        let info = bin.join(format!("$I{tag}{ext}"));
        let item = bin.join(format!("$R{tag}{ext}"));
        if !info.exists() && !item.exists() {
            return Some((info, item));
        }
    }
    None
}

fn now_filetime() -> i64 {
    // SAFETY: no preconditions.
    let ft = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
    (i64::from(ft.dwHighDateTime) << 32) | i64::from(ft.dwLowDateTime)
}

/// Lets Explorer windows and the desktop icon catch up: the items left their folders and the bins
/// changed. Up to [`NOTIFY_EACH_MAX`] items are reported one by one; past that each folder they
/// left is reported once, as changed.
fn notify(moved: &[(PathBuf, bool)], bins: &[&Path]) {
    use windows::Win32::UI::Shell::{SHCNE_DELETE, SHCNE_RMDIR, SHCNE_UPDATEDIR};
    if moved.len() <= NOTIFY_EACH_MAX {
        for (from, was_dir) in moved {
            send(if *was_dir { SHCNE_RMDIR } else { SHCNE_DELETE }, from);
        }
    } else {
        let mut folders: Vec<&Path> = moved.iter().filter_map(|(p, _)| p.parent()).collect();
        folders.sort_unstable();
        folders.dedup();
        for folder in folders {
            send(SHCNE_UPDATEDIR, folder);
        }
    }
    notify_bins(bins);
}

/// Tells Explorer windows that these bin folders (`X:\$Recycle.Bin\<SID>`) changed and has the
/// desktop's Recycle Bin icon redrawn (full or empty).
pub(crate) fn notify_bins(bins: &[&Path]) {
    use windows::Win32::UI::Shell::SHCNE_UPDATEDIR;
    // shellapi.h; not in the windows crate.
    windows_core::link!("shell32.dll" "system" fn SHUpdateRecycleBinIcon());
    for bin in bins {
        send(SHCNE_UPDATEDIR, bin);
    }
    // SAFETY: no arguments.
    unsafe { SHUpdateRecycleBinIcon() };
}

fn send(event: windows::Win32::UI::Shell::SHCNE_ID, path: &Path) {
    use windows::Win32::UI::Shell::{SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: a NUL-terminated path that outlives the call.
    unsafe {
        SHChangeNotify(
            event,
            SHCNF_PATHW | SHCNF_FLUSHNOWAIT,
            Some(wide.as_ptr().cast()),
            None,
        );
    }
}

/// A `$I` record.
#[derive(Debug, PartialEq, Eq)]
struct Record {
    version: i64,
    size: u64,
    deleted: i64,
    path: String,
}

/// The `$I` bytes for an item of `size` bytes deleted at `deleted` (FILETIME) from `path`.
fn encode(version: i64, size: u64, deleted: i64, path: &Path) -> Vec<u8> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut out = Vec::with_capacity(28 + path.len() * 2);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&deleted.to_le_bytes());
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    for unit in path {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// Reads a version 2 `$I` record; `None` for anything else.
fn decode(bytes: &[u8]) -> Option<Record> {
    let int = |at: usize| -> Option<[u8; 8]> { bytes.get(at..at + 8)?.try_into().ok() };
    let version = i64::from_le_bytes(int(0)?);
    if version != VERSION {
        return Some(Record {
            version,
            size: 0,
            deleted: 0,
            path: String::new(),
        });
    }
    let len = u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?) as usize;
    let units: Vec<u16> = bytes
        .get(28..28 + len * 2)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(Record {
        version,
        size: u64::from_le_bytes(int(8)?),
        deleted: i64::from_le_bytes(int(16)?),
        path: String::from_utf16(units.strip_suffix(&[0])?).ok()?,
    })
}

/// Calls to `SHQueryRecycleBinW`, which lists the whole bin each time.
#[cfg(test)]
pub(crate) static BIN_QUERIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Items the fast path took (tests tell it from the engine with this).
#[cfg(test)]
pub(crate) static TAKEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Temp sandboxes and bin lookups for the live tests here and in `tests_live_recycle.rs`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::shell::OpOutcome;

    /// A folder under %TEMP% named for this test and process; only it is ever touched.
    pub(crate) struct Sandbox(pub(crate) PathBuf);

    impl Sandbox {
        pub(crate) fn new(tag: &str) -> Self {
            purge_leftovers(tag);
            let dir = crate::win::long_temp_dir()
                .join(format!("arke-test-fastbin-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        pub(crate) fn file(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Bin items an earlier, interrupted run of the test `tag` left: their origin is one of its
    /// sandboxes (which may be gone by now). Long paths come back from the bin with `\\?\`.
    pub(crate) fn purge_leftovers(tag: &str) {
        let prefix = format!("arke-test-fastbin-{tag}-");
        let Ok(items) = crate::win_recycle::list() else {
            return;
        };
        let stale: Vec<crate::shell::RecycledItem> = items
            .into_iter()
            .filter(|item| {
                let from = arke_core::path::unverbatim(&item.from);
                from.starts_with(crate::win::long_temp_dir())
                    && from
                        .components()
                        .any(|c| c.as_os_str().to_string_lossy().starts_with(&prefix))
            })
            .collect();
        if !stale.is_empty() {
            purge(&stale);
        }
    }

    /// Items in the bin that came from `dir`.
    pub(crate) fn recycled_from(dir: &Path) -> Vec<crate::shell::RecycledItem> {
        let here = fs::canonicalize(dir).unwrap();
        crate::win_recycle::list()
            .unwrap()
            .into_iter()
            .filter(|item| fs::canonicalize(&item.from).ok().as_ref() == Some(&here))
            .collect()
    }

    pub(crate) fn purge(items: &[crate::shell::RecycledItem]) {
        assert_eq!(
            crate::win_fileops::purge_recycled(items, &crate::shell::OpProgress::default())
                .map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Sandbox, purge, recycled_from};
    use super::*;
    use crate::shell::{FileOp, OpOutcome, PutBack, Undo};

    #[test]
    fn bin_names_keep_the_name_from_the_last_dot() {
        for (name, kept) in [
            ("a.b.c.txt", ".txt"),
            (".gitignore", ".gitignore"),
            ("noext", ""),
            ("中文 資料夾", ""),
            ("photos.v2", ".v2"),
        ] {
            assert_eq!(bin_extension(Path::new(name)), kept, "{name}");
        }
    }

    #[test]
    fn group_policy_over_the_bin_leaves_it_to_the_engine() {
        assert!(!policy_blocks(None, None), "no policy");
        assert!(!policy_blocks(Some(0), None), "explicitly allowed");
        assert!(policy_blocks(Some(1), None), "bin turned off");
        assert!(policy_blocks(None, Some(5)), "size capped");
        assert!(policy_blocks(None, Some(0)), "capped at nothing");
    }

    #[test]
    fn an_item_fits_only_with_what_is_already_in_the_bin() {
        let mb = 1024 * 1024;
        assert!(has_room(0, 10 * mb, 10));
        assert!(!has_room(1, 10 * mb, 10), "one byte over");
        assert!(!has_room(5 * mb, 6 * mb, 10));
        assert!(!has_room(0, 1, 0), "a bin of size 0 takes nothing");
        assert!(!has_room(u64::MAX, 1, u32::MAX), "no overflow");
    }

    #[test]
    fn records_read_back_as_written() {
        let path = Path::new(r"C:\Users\someone\Downloads\big folder.v1");
        let bytes = encode(2, 10, 0x01DD_735E_EFCE_2ED0, path);
        assert_eq!(bytes.len(), 28 + (path.as_os_str().len() + 1) * 2);
        assert_eq!(
            decode(&bytes),
            Some(Record {
                version: 2,
                size: 10,
                deleted: 0x01DD_735E_EFCE_2ED0,
                path: path.display().to_string(),
            })
        );
        assert_eq!(decode(&bytes[..20]), None, "too short");
    }

    /// The layout is checked against what Windows itself writes: a file and a folder recycled
    /// through the engine, their `$I` records decoded and written again, byte for byte.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn records_match_what_windows_writes() {
        let sb = Sandbox::new("layout");
        let file = sb.file("note.txt", "hello");
        sb.file(r"folder.v1\a.bin", "1234567");
        sb.file(r"folder.v1\sub\b.bin", "123");
        let folder = sb.0.join("folder.v1");
        assert_eq!(
            crate::win_fileops::perform(
                &FileOp::Delete {
                    paths: vec![file, folder],
                    recycle: true,
                },
                None,
                true,
            )
            .map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        let items = recycled_from(&sb.0);
        assert_eq!(items.len(), 2);
        for item in &items {
            let now = Path::new(&item.now);
            let name = now.file_name().unwrap().to_string_lossy();
            let info = now.with_file_name(format!("$I{}", &name[2..]));
            let bytes = fs::read(&info).unwrap();
            let record = decode(&bytes).expect("a version 2 record");
            assert_eq!(record.size, if item.is_dir { 10 } else { 5 });
            assert_eq!(
                encode(
                    record.version,
                    record.size,
                    record.deleted,
                    Path::new(&record.path)
                ),
                bytes,
                "{}",
                item.name
            );
        }
        purge(&items);
    }

    /// The fast path end to end: the folder (a read-only file and a subfolder inside) is in the
    /// bin with the right name, origin and size, then comes back where it was with everything in
    /// it, and leaves no record behind.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn a_folder_goes_to_the_bin_and_comes_back() {
        let sb = Sandbox::new("roundtrip");
        sb.file(r"photos.v2\a.bin", "1234567");
        sb.file(r"photos.v2\sub\b.bin", "123");
        let ro = sb.0.join(r"photos.v2\a.bin");
        let mut perms = fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ro, perms).unwrap();
        let folder = sb.0.join("photos.v2");

        let Some(now) = recycle(&folder) else {
            panic!("this drive should take the fast path (fixed NTFS, bin folder present)");
        };
        assert!(!folder.exists());
        assert!(now.file_name().unwrap().to_string_lossy().starts_with("$R"));
        assert!(now.to_string_lossy().ends_with(".v2"));
        let items = recycled_from(&sb.0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "photos.v2");
        assert!(items[0].is_dir);
        assert_eq!(items[0].size, 10);
        assert_eq!(Path::new(&items[0].now), now);

        let back = Undo::PutBack(vec![PutBack {
            now: items[0].now.clone(),
            parent: sb.0.clone(),
            name: "photos.v2".into(),
        }]);
        assert_eq!(
            crate::win_fileops::undo(&back, None, true).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        assert_eq!(
            fs::read_to_string(folder.join(r"sub\b.bin")).unwrap(),
            "123"
        );
        assert!(fs::metadata(&ro).unwrap().permissions().readonly());
        assert!(recycled_from(&sb.0).is_empty(), "no record left behind");
    }

    /// Recycles `path` through the Windows engine (silent), like Arke does without the fast path.
    fn engine(paths: Vec<PathBuf>) -> Result<OpOutcome, String> {
        crate::win_fileops::perform(
            &FileOp::Delete {
                paths,
                recycle: true,
            },
            None,
            true,
        )
        .map_err(|e| e.to_string())
    }

    /// The `$R` name past the six random characters: what Windows keeps of the original name.
    fn suffix(now: &str) -> String {
        Path::new(now).file_name().unwrap().to_string_lossy()[8..].to_owned()
    }

    fn record_of(now: &str) -> Record {
        let now = Path::new(now);
        let name = now.file_name().unwrap().to_string_lossy();
        decode(&fs::read(now.with_file_name(format!("$I{}", &name[2..]))).unwrap()).unwrap()
    }

    /// Builds the same item twice, under `engine` and `fast`, with `build(dir)`.
    fn twin(sb: &Sandbox, build: impl Fn(&Path) -> PathBuf) -> (PathBuf, PathBuf) {
        let engine_dir = sb.0.join("engine");
        let fast_dir = sb.0.join("fast");
        fs::create_dir_all(&engine_dir).unwrap();
        fs::create_dir_all(&fast_dir).unwrap();
        (build(&engine_dir), build(&fast_dir))
    }

    /// Odd names: the bin item keeps the same part of the name, the record the same size and
    /// path as when Windows recycles the same item, and each comes back intact.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn odd_names_end_up_like_windows_puts_them() {
        let sb = Sandbox::new("names");
        let names = [
            ".gitignore",
            "noext",
            "a.b.c.txt",
            "中文 資料夾",
            "emoji 😀.md",
            "UPPER.TXT",
        ];
        for name in names {
            let (engine_item, fast_item) = twin(&sb, |dir| {
                let path = dir.join(name);
                if name.contains('.') {
                    fs::write(&path, "12345").unwrap();
                } else {
                    fs::create_dir_all(path.join("inner")).unwrap();
                    fs::write(path.join("inner").join("x.bin"), "1234567").unwrap();
                }
                path
            });
            assert_eq!(
                engine(vec![engine_item.clone()]),
                Ok(OpOutcome::Done),
                "{name}"
            );
            let fast_now = recycle(&fast_item).unwrap_or_else(|| panic!("{name}: not taken"));
            let by_engine = recycled_from(engine_item.parent().unwrap());
            let by_fast = recycled_from(fast_item.parent().unwrap());
            assert_eq!((by_engine.len(), by_fast.len()), (1, 1), "{name}");
            let (e, f) = (&by_engine[0], &by_fast[0]);
            assert_eq!(suffix(&e.now), suffix(&f.now), "{name}: bin name");
            assert_eq!(
                (e.name.as_str(), e.is_dir, e.size),
                (f.name.as_str(), f.is_dir, f.size)
            );
            let (re, rf) = (record_of(&e.now), record_of(&f.now));
            assert_eq!(re.size, rf.size, "{name}: recorded size");
            assert_eq!(
                re.path.replace(r"\engine\", r"\fast\"),
                rf.path,
                "{name}: recorded path"
            );
            assert_eq!(Path::new(&f.now), fast_now);
            purge(&by_engine);
            let back = Undo::PutBack(vec![PutBack {
                now: f.now.clone(),
                parent: f.from.clone(),
                name: f.name.clone(),
            }]);
            assert_eq!(
                crate::win_fileops::undo(&back, None, true).map_err(|e| e.to_string()),
                Ok(OpOutcome::Done),
                "{name}: restore"
            );
            assert!(fast_item.exists(), "{name}: back in place");
            let _ = fs::remove_dir_all(&fast_item);
            let _ = fs::remove_file(&fast_item);
        }
    }

    /// Items something holds: whatever the engine does with them (move or leave), the fast path
    /// does too, and when it leaves one it leaves no record behind.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    #[allow(
        clippy::print_stderr,
        reason = "a live test run by hand reports what it saw"
    )]
    fn items_in_use_are_treated_like_the_engine_treats_them() {
        use crate::win_inuse::test_support::lock_in_other_process;
        type Build = fn(&Path) -> PathBuf;
        let sb = Sandbox::new("inuse");
        let cases: [(&str, Build); 2] = [
            ("open file", |dir| {
                let file = dir.join("open.txt");
                fs::write(&file, "x").unwrap();
                file
            }),
            ("folder with an open file", |dir| {
                let folder = dir.join("holder");
                fs::create_dir_all(&folder).unwrap();
                fs::write(folder.join("open.txt"), "x").unwrap();
                folder
            }),
        ];
        for (label, build) in cases {
            let (engine_item, fast_item) = twin(&sb, build);
            let lock = |item: &Path| {
                let target = if item.is_dir() {
                    item.join("open.txt")
                } else {
                    item.to_path_buf()
                };
                lock_in_other_process(&target)
            };
            let engine_lock = lock(&engine_item);
            let _ = engine(vec![engine_item.clone()]);
            let engine_moved = !engine_item.exists();
            drop(engine_lock);

            let fast_lock = lock(&fast_item);
            let records_before = recycled_from(fast_item.parent().unwrap()).len();
            let fast_moved = recycle(&fast_item).is_some();
            drop(fast_lock);
            eprintln!("{label}: engine moved it {engine_moved}, fast path moved it {fast_moved}");
            assert_eq!(
                fast_moved,
                !fast_item.exists(),
                "{label}: answer matches the disk"
            );
            assert_eq!(
                fast_moved, engine_moved,
                "{label}: same outcome as the engine"
            );
            if !fast_moved {
                assert_eq!(
                    recycled_from(fast_item.parent().unwrap()).len(),
                    records_before,
                    "{label}: no record left behind"
                );
            }
            purge(&recycled_from(engine_item.parent().unwrap()));
            purge(&recycled_from(fast_item.parent().unwrap()));
        }
    }

    /// A folder with a junction to somewhere else goes to the bin with the junction, and the
    /// place it points to keeps its files; a junction on its own is left to the engine.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn links_go_with_their_folder_and_their_targets_stay() {
        let sb = Sandbox::new("links");
        let outside = sb.file(r"outside\keep.txt", "keep");
        let folder = sb.0.join("project");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("a.txt"), "a").unwrap();
        let junction = |link: &Path, target: &Path| {
            let ok = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "junction");
        };
        junction(&folder.join("linked"), outside.parent().unwrap());
        let alone = sb.0.join("alone");
        junction(&alone, outside.parent().unwrap());

        assert_eq!(
            recycle(&alone),
            None,
            "a junction itself stays with the engine"
        );
        assert!(alone.exists());
        let now = recycle(&folder).expect("the folder is taken");
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "keep",
            "target untouched"
        );
        let items = recycled_from(&sb.0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].size, 1, "the link's target isn't counted");
        assert_eq!(Path::new(&items[0].now), now);
        let back = Undo::PutBack(vec![PutBack {
            now: items[0].now.clone(),
            parent: sb.0.clone(),
            name: "project".into(),
        }]);
        assert_eq!(
            crate::win_fileops::undo(&back, None, true).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        assert_eq!(
            fs::read_to_string(folder.join(r"linked\keep.txt")).unwrap(),
            "keep",
            "the link still points where it did"
        );
        let _ = fs::remove_dir(folder.join("linked"));
        let _ = fs::remove_dir(&alone);
    }

    /// Sizes Windows records for files with an alternate data stream, and hidden, system and
    /// read-only ones, match what the fast path records.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn recorded_sizes_match_windows_for_streams_and_attributes() {
        let sb = Sandbox::new("sizes");
        let (engine_item, fast_item) = twin(&sb, |dir| {
            let folder = dir.join("mixed");
            fs::create_dir_all(&folder).unwrap();
            let plain = folder.join("plain.txt");
            fs::write(&plain, "12345").unwrap();
            fs::write(folder.join("plain.txt:extra"), vec![b'x'; 1000]).unwrap();
            let flagged = folder.join("flagged.txt");
            fs::write(&flagged, "123").unwrap();
            let ok = std::process::Command::new("attrib")
                .args(["+h", "+s", "+r"])
                .arg(&flagged)
                .status()
                .unwrap()
                .success();
            assert!(ok, "attrib");
            folder
        });
        assert_eq!(engine(vec![engine_item.clone()]), Ok(OpOutcome::Done));
        recycle(&fast_item).expect("taken");
        let e = &recycled_from(engine_item.parent().unwrap())[0];
        let f = &recycled_from(fast_item.parent().unwrap())[0];
        assert_eq!(record_of(&e.now).size, record_of(&f.now).size);
        purge(&recycled_from(engine_item.parent().unwrap()));
        purge(&recycled_from(fast_item.parent().unwrap()));
    }

    /// A folder whose own path is longer than MAX_PATH: the fast path takes it only if the engine
    /// can too, and then it comes back.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    #[allow(
        clippy::print_stderr,
        reason = "a live test run by hand reports what it saw"
    )]
    fn long_paths_follow_the_engine() {
        let sb = Sandbox::new("long");
        let deep = |dir: &Path| -> PathBuf {
            let mut path = arke_core::path::verbatim(dir);
            for _ in 0..6 {
                path.push("a".repeat(45));
            }
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("x.txt"), "x").unwrap();
            path
        };
        let (engine_item, fast_item) = twin(&sb, deep);
        assert!(arke_core::path::unverbatim(&fast_item).as_os_str().len() > 260);
        let engine_result = engine(vec![arke_core::path::unverbatim(&engine_item)]);
        let engine_moved = !engine_item.exists();
        let fast_now = recycle(&arke_core::path::unverbatim(&fast_item));
        eprintln!("engine: {engine_result:?}, moved {engine_moved}; fast: {fast_now:?}");
        assert_eq!(
            fast_now.is_some(),
            engine_moved,
            "same outcome as the engine"
        );
        if let Some(now) = fast_now {
            let back = Undo::PutBack(vec![PutBack {
                now: now.to_string_lossy().into_owned(),
                parent: arke_core::path::unverbatim(fast_item.parent().unwrap()),
                name: "a".repeat(45),
            }]);
            assert_eq!(
                crate::win_fileops::undo(&back, None, true).map_err(|e| e.to_string()),
                Ok(OpOutcome::Done),
                "restores"
            );
            assert!(fast_item.join("x.txt").exists());
        }
        purge(&recycled_from(engine_item.parent().unwrap()));
        let _ = fs::remove_dir_all(arke_core::path::verbatim(&sb.0));
    }

    /// Many items at once from several threads (as with two windows deleting together): each gets
    /// its own names in the bin, nothing overwrites anything, and every one comes back.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn many_at_once_each_get_their_own_place() {
        const ITEMS: usize = 40;
        let sb = Sandbox::new("together");
        let items: Vec<PathBuf> = (0..ITEMS)
            .map(|i| {
                let dir = sb.0.join(format!("item{i:02}.dat"));
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("body.txt"), format!("item {i}")).unwrap();
                dir
            })
            .collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done = parking_lot::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let now = recycle(item);
                        done.lock().push((item.clone(), now));
                    }
                });
            }
        });
        let done = done.into_inner();
        assert!(done.iter().all(|(_, now)| now.is_some()), "all taken");
        let mut places: Vec<&PathBuf> = done.iter().filter_map(|(_, now)| now.as_ref()).collect();
        places.sort();
        places.dedup();
        assert_eq!(places.len(), ITEMS, "every item has its own place");
        assert_eq!(
            recycled_from(&sb.0).len(),
            ITEMS,
            "and Windows lists them all"
        );

        let back = Undo::PutBack(
            recycled_from(&sb.0)
                .into_iter()
                .map(|item| PutBack {
                    now: item.now,
                    parent: sb.0.clone(),
                    name: item.name,
                })
                .collect(),
        );
        assert_eq!(
            crate::win_fileops::undo(&back, None, true).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        for (i, item) in items.iter().enumerate() {
            assert_eq!(
                fs::read_to_string(item.join("body.txt")).unwrap(),
                format!("item {i}"),
                "each back with its own contents"
            );
        }
        assert!(recycled_from(&sb.0).is_empty());
    }

    /// Timing against the engine on the same kind of folder. Run with
    /// `cargo test -p arke-app --bin arke fast_recycle_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: writes 20,000 files to %TEMP% and uses the Recycle Bin"]
    #[allow(
        clippy::print_stderr,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn fast_recycle_timing() {
        let sb = Sandbox::new("timing");
        let dir = sb.0.join("many");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..20_000 {
            fs::write(dir.join(format!("f{i:05}.txt")), b"x").unwrap();
        }
        let started = std::time::Instant::now();
        let now = recycle(&dir);
        eprintln!(
            "fast recycle of 20,000 files: {:?} (to {now:?})",
            started.elapsed()
        );
        assert!(now.is_some());
        purge(&recycled_from(&sb.0));
    }

    /// `count` small files named `arke-test-<tag>-NNNNN.txt` directly in the sandbox.
    fn loose_files(sb: &Sandbox, tag: &str, count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|i| {
                let path = sb.0.join(format!("arke-test-{tag}-{i:05}.txt"));
                fs::write(&path, b"x").unwrap();
                path
            })
            .collect()
    }

    /// Deletes `paths` to the bin the way Arke's own progress does it.
    fn delete_tracked(
        paths: Vec<PathBuf>,
        progress: &std::sync::Arc<crate::shell::OpProgress>,
    ) -> (Result<OpOutcome, String>, Option<Undo>) {
        set_enabled(true);
        let (result, undo) = crate::win_fileops::perform_tracked(
            &FileOp::Delete {
                paths,
                recycle: true,
            },
            None,
            progress,
        );
        (result.map_err(|e| e.to_string()), undo)
    }

    /// Many loose files in one delete: what Windows reports about the bin (a call that lists the
    /// whole bin) is asked for once per drive, not three times per file.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn loose_files_ask_about_the_bin_once() {
        const FILES: usize = 30;
        let sb = Sandbox::new("batch");
        let files = loose_files(&sb, "batch", FILES);
        let progress = std::sync::Arc::default();
        let before = BIN_QUERIES.load(Ordering::Relaxed);
        let (result, undo) = delete_tracked(files.clone(), &progress);
        let queries = BIN_QUERIES.load(Ordering::Relaxed) - before;
        let binned = recycled_from(&sb.0);
        purge(&binned);
        assert_eq!(result, Ok(OpOutcome::Done));
        assert_eq!(binned.len(), FILES, "all of them in the bin");
        assert!(files.iter().all(|f| !f.exists()));
        assert!(matches!(undo, Some(Undo::PutBack(items)) if items.len() == FILES));
        assert!(queries <= 2, "{queries} bin queries for {FILES} files");
    }

    /// Cancel stops the fast path before the next item, like it stops the engine.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn cancel_stops_the_fast_path() {
        let sb = Sandbox::new("cancel");
        let files = loose_files(&sb, "cancel", 5);
        let progress = std::sync::Arc::<crate::shell::OpProgress>::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let (result, _) = delete_tracked(files.clone(), &progress);
        let binned = recycled_from(&sb.0);
        purge(&binned);
        assert_eq!(result, Ok(OpOutcome::Cancelled));
        assert!(binned.is_empty(), "{} went to the bin", binned.len());
        assert!(files.iter().all(|f| f.exists()));
    }

    /// Timing for 20,000 loose files in one delete. Run with
    /// `cargo test -p arke-app --bin arke loose_files_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: writes 20,000 files to %TEMP% and uses the Recycle Bin"]
    #[allow(
        clippy::print_stderr,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn loose_files_timing() {
        const FILES: usize = 20_000;
        let sb = Sandbox::new("loose");
        let files = loose_files(&sb, "loose", FILES);
        let progress = std::sync::Arc::default();
        let before = BIN_QUERIES.load(Ordering::Relaxed);
        let started = std::time::Instant::now();
        let (result, _) = delete_tracked(files, &progress);
        let took = started.elapsed();
        let queries = BIN_QUERIES.load(Ordering::Relaxed) - before;
        eprintln!(
            "fast recycle of {FILES} loose files: {took:?}, {queries} bin queries, {result:?}"
        );
        let binned = recycled_from(&sb.0);
        eprintln!("{} in the bin; purging them", binned.len());
        purge(&binned);
        assert_eq!(result, Ok(OpOutcome::Done));
    }

    /// The same kind of delete through the Windows engine, for comparison (fewer files: the
    /// engine is slower). Run alone, like `loose_files_timing`: it turns fast recycle off.
    #[test]
    #[ignore = "benchmark: uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    #[allow(
        clippy::print_stderr,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn engine_loose_files_timing() {
        const FILES: usize = 2_000;
        let sb = Sandbox::new("engine-loose");
        let files = loose_files(&sb, "engine-loose", FILES);
        let progress = std::sync::Arc::default();
        set_enabled(false);
        let started = std::time::Instant::now();
        let (result, _) = crate::win_fileops::perform_tracked(
            &FileOp::Delete {
                paths: files,
                recycle: true,
            },
            None,
            &progress,
        );
        let took = started.elapsed();
        eprintln!(
            "engine recycle of {FILES} loose files: {took:?}, {:?}",
            result.as_ref().map_err(ToString::to_string)
        );
        let binned = recycled_from(&sb.0);
        eprintln!("{} in the bin; purging them", binned.len());
        purge(&binned);
        assert!(result.is_ok());
    }
}
