//! Fast Recycle Bin (experimental, off by default; #9).
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

/// `$I` layout version written and accepted.
const VERSION: i64 = 2;
/// `$I` records looked at to check the bin's layout.
const PROBE_RECORDS: usize = 8;

/// Puts `path` in its drive's Recycle Bin directly. Returns where it went (the `$R` item), or
/// `None` when this path isn't eligible or anything failed before it moved, so the Windows
/// engine should handle it.
pub(crate) fn recycle(path: &Path) -> Option<PathBuf> {
    let root = drive_root(path)?;
    if !local_ntfs(&root) {
        return None;
    }
    let bin = root.join("$Recycle.Bin").join(user_sid()?);
    if !bin.is_dir() || !layout_is_known(&bin) {
        return None;
    }
    let meta = fs::symlink_metadata(path).ok()?;
    let size = if meta.is_dir() && !meta.file_type().is_symlink() {
        tree_size(path)?
    } else {
        meta.len()
    };
    if !fits(&root, size) {
        return None;
    }
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let (info, item) = free_names(&bin, &ext)?;
    let record = encode(
        VERSION,
        size,
        now_filetime(),
        &arke_core::path::unverbatim(path),
    );
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&info)
        .and_then(|mut f| f.write_all(&record));
    if written.is_err() {
        let _ = fs::remove_file(&info);
        return None;
    }
    if let Err(err) = fs::rename(
        arke_core::path::verbatim(path),
        arke_core::path::verbatim(&item),
    ) {
        log::info!(
            "fast recycle: {} stays with the engine: {err}",
            path.display()
        );
        let _ = fs::remove_file(&info);
        return None;
    }
    notify(path, meta.is_dir(), &bin);
    Some(item)
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

/// The drive's bin keeps items (not "delete right away") and has room for `size` more bytes.
/// Read from the bin's own settings; anything unclear means no.
fn fits(root: &Path, size: u64) -> bool {
    let Some((max_mb, nuke)) = bin_settings(root) else {
        return false;
    };
    if nuke {
        return false;
    }
    let Some(used) = bin_usage(root) else {
        return false;
    };
    used.saturating_add(size) <= u64::from(max_mb) * 1024 * 1024
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
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key = windows::core::HSTRING::from(key);
    let value = windows::core::HSTRING::from(value);
    let mut data = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: a read of one DWORD into `data`; strings are NUL-terminated.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
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

/// Bytes already in the drive's bin.
fn bin_usage(root: &Path) -> Option<u64> {
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    let root = windows::core::HSTRING::from(root.as_os_str());
    let mut info = SHQUERYRBINFO {
        cbSize: size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: root is NUL-terminated; info is a properly sized out struct.
    unsafe { SHQueryRecycleBinW(&root, &raw mut info) }.ok()?;
    u64::try_from(info.i64Size).ok()
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

/// Lets Explorer windows and the desktop icon catch up: the item left its folder and the bin
/// changed.
fn notify(from: &Path, was_dir: bool, bin: &Path) {
    use windows::Win32::UI::Shell::{
        SHCNE_DELETE, SHCNE_RMDIR, SHCNE_UPDATEDIR, SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify,
    };
    // shellapi.h; not in the windows crate.
    windows_core::link!("shell32.dll" "system" fn SHUpdateRecycleBinIcon());
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let from = wide(from);
    let bin = wide(bin);
    let event = if was_dir { SHCNE_RMDIR } else { SHCNE_DELETE };
    // SAFETY: NUL-terminated paths that outlive the calls.
    unsafe {
        SHChangeNotify(
            event,
            SHCNF_PATHW | SHCNF_FLUSHNOWAIT,
            Some(from.as_ptr().cast()),
            None,
        );
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW | SHCNF_FLUSHNOWAIT,
            Some(bin.as_ptr().cast()),
            None,
        );
        SHUpdateRecycleBinIcon();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{FileOp, OpOutcome, PutBack, Undo};

    /// A folder under %TEMP% named for this test and process; only it is ever touched.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = crate::win::long_temp_dir()
                .join(format!("arke-test-fastbin-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, body: &str) -> PathBuf {
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

    /// Items in the bin that came from `dir`.
    fn recycled_from(dir: &Path) -> Vec<crate::shell::RecycledItem> {
        let here = fs::canonicalize(dir).unwrap();
        crate::win_recycle::list()
            .unwrap()
            .into_iter()
            .filter(|item| fs::canonicalize(&item.from).ok().as_ref() == Some(&here))
            .collect()
    }

    fn purge(items: &[crate::shell::RecycledItem]) {
        let places: Vec<&str> = items.iter().map(|i| i.now.as_str()).collect();
        assert_eq!(
            crate::win_fileops::purge_recycled(&places, None).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
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

    /// Timing against the engine on the same kind of folder. Run with
    /// `cargo test -p arke-app --bin arke fast_recycle_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: writes 20,000 files to %TEMP% and uses the Recycle Bin"]
    #[allow(clippy::print_stderr)]
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
}
