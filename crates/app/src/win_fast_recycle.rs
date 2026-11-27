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
//! - The drive's bin is not set to delete right away, and this item fits in its maximum size on
//!   its own (Windows would otherwise ask), as does everything this delete puts there.
//! - No Group Policy turns the bin off or caps its size (the engine applies those).
//! - The `$I` records already in the bin are all version 2, the layout written here.
//! - The rename succeeds: a file in use inside the folder makes it fail, nothing has moved, and
//!   the item goes through the engine instead. A folder holding read-only items can be refused
//!   by the rename (seen on a CI runner: access denied moving it into the bin, while renaming it
//!   in place works), and then goes to the engine too.
//!
//! Everything else goes to the Windows engine. An item bigger than its drive's bin may hold at
//! all is the exception: Windows would delete it permanently (asking behind Arke's window, or not
//! at all without confirmations), so the caller keeps it and Arke asks instead. The fast path
//! already has the size for that; for items it leaves to the engine before measuring them,
//! [`Limits`] sums the size, stopping as soon as it is past the limit. A bin that is merely full is
//! no reason: like Windows, the fast path makes room by deleting the oldest items for good
//! (`win_bin_evict.rs`). The size is summed from directory listings only, so no file is opened
//! (and scanned). What the bin already holds is kept in memory (`win_bin_usage.rs`). The `$I`
//! layout (version 2) was checked byte for byte against records Windows wrote (see the tests):
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

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::win_bin_evict::{Evicted, Evictor, delete_for_good};
use crate::win_bin_usage::{Stamp, tracker};

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
pub(crate) const VERSION: i64 = 2;
/// `$I` records looked at to check the bin's layout.
const PROBE_RECORDS: usize = 8;

/// Puts `path` in its drive's Recycle Bin directly. Returns where it went (the `$R` item), or
/// `None` when this path isn't eligible or anything failed before it moved, so the Windows
/// engine should handle it. One item as a batch of its own (Arke deletes through [`Batch`]).
#[cfg(test)]
pub(crate) fn recycle(path: &Path) -> Option<PathBuf> {
    let mut batch = Batch::default();
    let item = if let Outcome::Recycled(now) = batch.recycle(path) {
        Some(now)
    } else {
        None
    };
    batch.finish();
    item
}

/// What [`Batch::recycle`] did with an item.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// In the bin, as this `$R` item.
    Recycled(PathBuf),
    /// Bigger than the drive's bin may hold: Windows would delete it permanently. Not moved.
    TooBig,
    /// Not moved; the engine should take it. `measured`: its size was already summed (it fits,
    /// or the tree couldn't be listed), so checking it again against the bin's limit is no use.
    Engine { measured: bool },
}

impl Outcome {
    const UNMEASURED: Self = Self::Engine { measured: false };
    const MEASURED: Self = Self::Engine { measured: true };
}

/// Most items reported to Explorer one by one; past this a batch reports each folder they left
/// once instead.
const NOTIFY_EACH_MAX: usize = 64;

/// One delete of any number of items. What doesn't change from item to item is checked once:
/// Group Policy and the user for the batch; per drive the file system, the bin folder, its `$I`
/// layout and its settings. What the bin holds comes from memory ([`crate::win_bin_usage`]):
/// `SHQueryRecycleBinW` lists the whole bin, which took tens of seconds per delete on a big one
/// (#379). It is only asked while this version of Windows hasn't been seen counting an item moved
/// to a bin ([`Counted`]). The bytes in the bin are then kept up to date locally.
///
/// When the bin is full, the item goes in first and older items are then deleted for good until
/// it fits, oldest first and never items of this batch, as Windows does ([`crate::win_bin_evict`]).
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
    /// Bytes in the bin: what its records held at the start, plus what this batch added, minus
    /// what it deleted to make room.
    used: u64,
    /// Items Windows counted at the start, while this version of Windows hasn't been seen
    /// counting an item moved to a bin ([`Counted`]).
    unconfirmed: Option<u64>,
    /// `$I` names of the items this batch moved here: never deleted to make room.
    mine: HashSet<String>,
    /// Bytes this batch moved here.
    mine_bytes: u64,
    /// Older items to delete for good when the bin is full; made when first needed.
    evictor: Option<Evictor>,
    /// What that deleted, logged once at the end.
    evicted: Evicted,
}

impl Batch {
    /// One item of the batch: in the bin, too big for it, or for the engine.
    pub(crate) fn recycle(&mut self, path: &Path) -> Outcome {
        let Some(sid) = self
            .user
            .get_or_init(|| policy_allows().then(user_sid).flatten())
            .clone()
        else {
            declined(|| format!("{}: policy or user", path.display()));
            return Outcome::UNMEASURED;
        };
        // Windows was seen not counting an item moved to a bin: the engine, until Arke restarts.
        if !counted().allowed() {
            declined(|| format!("{}: Windows was seen not counting", path.display()));
            return Outcome::UNMEASURED;
        }
        let Some(root) = drive_root(path) else {
            declined(|| format!("{}: no drive letter", path.display()));
            return Outcome::UNMEASURED;
        };
        let Some(drive) = self
            .drives
            .entry(root.clone())
            .or_insert_with(|| Drive::check(&root, &sid))
            .as_mut()
        else {
            declined(|| format!("{}: the drive isn't taken (Drive::check)", path.display()));
            return Outcome::UNMEASURED;
        };
        let Ok(meta) = fs::symlink_metadata(path) else {
            declined(|| format!("{}: metadata unreadable", path.display()));
            return Outcome::UNMEASURED;
        };
        // Links, junctions and cloud placeholders (OneDrive and the like keep a reparse point on
        // every item they sync) stay with the engine, which knows what each of them needs.
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            declined(|| format!("{}: reparse point", path.display()));
            return Outcome::UNMEASURED;
        }
        let size = if meta.is_dir() {
            let Some(size) = tree_size(path) else {
                declined(|| format!("{}: a folder inside can't be listed", path.display()));
                return Outcome::MEASURED;
            };
            size
        } else {
            meta.len()
        };
        match room(drive.used, drive.mine_bytes, size, drive.max_mb) {
            Room::Fits => {}
            Room::Full { need } if drive.can_free(need) => {}
            // Deleting every older item wouldn't make room (some aren't version 2 records, or
            // they are this batch's own): nothing is deleted, and the engine decides.
            Room::Full { need } => {
                declined(|| format!("{}: bin full, can't free {need}", path.display()));
                return Outcome::MEASURED;
            }
            // Windows would delete it permanently: Arke asks instead.
            Room::TooBig => return Outcome::TooBig,
        }
        let Some(placed) = place(path, &drive.bin, size) else {
            declined(|| format!("{}: left to the engine by place()", path.display()));
            return Outcome::MEASURED;
        };
        // The first item moved to a bin on this version of Windows: Windows must count it. If it
        // doesn't, put it back, and this item and every later one go to the engine. Checked
        // before any room is made, so the count can only go up. (A crash between the record and
        // the move leaves the item where it was and an `$I` with no item, which Windows lists as
        // nothing to restore; no data moves without a record.)
        if let Some(count_before) = drive.unconfirmed.take()
            && !counted().confirmed()
        {
            if windows_counted(&root, count_before) {
                counted().confirm();
            } else {
                log::warn!(
                    "fast recycle: Windows didn't count {}; put back",
                    path.display()
                );
                declined(|| format!("{}: Windows didn't count it", path.display()));
                counted().fail();
                let back = move_no_replace(
                    &arke_core::path::verbatim(&placed.item),
                    &arke_core::path::verbatim(path),
                );
                if back.is_ok() {
                    let _ = fs::remove_file(&placed.info);
                    return Outcome::MEASURED;
                }
            }
        }
        drive.add(&placed, size);
        self.moved.push((path.to_path_buf(), meta.is_dir()));
        #[cfg(test)]
        TAKEN.fetch_add(1, Ordering::Relaxed);
        Outcome::Recycled(placed.item)
    }

    /// Tells Explorer windows and the desktop icon what changed. Call once, after the last item.
    pub(crate) fn finish(self) {
        let now = now_filetime();
        for drive in self.drives.values().flatten() {
            drive.evicted.log(&drive.bin, now);
        }
        if self.moved.is_empty() {
            return;
        }
        let bins: Vec<&Path> = self
            .drives
            .values()
            .flatten()
            .filter(|d| !d.mine.is_empty())
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
        let used = tracker().bytes(&bin)?;
        let unconfirmed = if counted().confirmed() {
            None
        } else {
            Some(bin_state(root)?.1)
        };
        Some(Self {
            bin,
            max_mb,
            used,
            unconfirmed,
            mine: HashSet::new(),
            mine_bytes: 0,
            evictor: None,
            evicted: Evicted::default(),
        })
    }

    /// Whether deleting older items (none of this batch's) could free `need` bytes.
    fn can_free(&mut self, need: u64) -> bool {
        let bin = &self.bin;
        self.evictor
            .get_or_insert_with(|| Evictor::new(tracker().candidates(bin)))
            .can_free(need, &self.mine)
    }

    /// The item [`place`] moved here (`size` bytes) is in the bin: counted, and if that took the
    /// bin over its maximum, older items are deleted for good until it fits.
    fn add(&mut self, placed: &Placed, size: u64) {
        let name = placed
            .info
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        let head = Head {
            version: VERSION,
            size,
            deleted: placed.deleted,
        };
        tracker().note_added(&self.bin, &name, head, placed.stamp);
        self.used = self.used.saturating_add(size);
        self.mine_bytes = self.mine_bytes.saturating_add(size);
        self.mine.insert(name);
        let need = self.used.saturating_sub(max_bytes(self.max_mb));
        if need == 0 {
            return;
        }
        let bin = &self.bin;
        let evicted = self
            .evictor
            .get_or_insert_with(|| Evictor::new(tracker().candidates(bin)))
            .evict(need, &self.mine, |c| delete_for_good(bin, c));
        self.used = self.used.saturating_sub(evicted.bytes);
        self.evicted.absorb(evicted);
    }
}

/// An item [`place`] moved into the bin.
struct Placed {
    /// Its `$I` record.
    info: PathBuf,
    /// Its `$R` data: the item itself.
    item: PathBuf,
    /// Deletion time in the record, FILETIME.
    deleted: i64,
    stamp: Stamp,
}

/// Writes the `$I` record of `path` (`size` bytes) and renames it into `bin`. `None` when nothing
/// moved; no record is left behind then.
fn place(path: &Path, bin: &Path, size: u64) -> Option<Placed> {
    let ext = bin_extension(path);
    let Some((info, item)) = free_names(bin, &ext) else {
        declined(|| format!("{}: no free name in the bin", path.display()));
        return None;
    };
    let deleted = now_filetime();
    let record = encode(VERSION, size, deleted, &arke_core::path::unverbatim(path));
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
    let in_place = fs::symlink_metadata(&info)
        .ok()
        .filter(|m| m.len() == record.len() as u64);
    let (Ok(()), Some(meta)) = (&written, &in_place) else {
        declined(|| {
            format!(
                "{}: record not written ({written:?}, in place: {})",
                path.display(),
                in_place.is_some()
            )
        });
        let _ = fs::remove_file(&info);
        return None;
    };
    if let Err(err) = move_no_replace(
        &arke_core::path::verbatim(path),
        &arke_core::path::verbatim(&item),
    ) {
        declined(|| format!("{}: rename failed: {err}", path.display()));
        log::info!(
            "fast recycle: {} stays with the engine: {err}",
            path.display()
        );
        let _ = fs::remove_file(&info);
        return None;
    }
    Some(Placed {
        info,
        item,
        deleted,
        stamp: Stamp::of(meta),
    })
}

/// Where an item stands with its drive's bin.
#[derive(Debug, PartialEq, Eq)]
enum Room {
    /// It fits with what is there.
    Fits,
    /// It fits once older items holding `need` bytes are deleted for good, as Windows does.
    Full { need: u64 },
    /// It can't go to the bin without it, or this batch's own items, being deleted for good.
    TooBig,
}

/// Where an item of `size` bytes stands with a bin of at most `max_mb` MB that holds `used`
/// bytes, `mine` of them put there by this batch.
fn room(used: u64, mine: u64, size: u64, max_mb: u32) -> Room {
    let max = max_bytes(max_mb);
    if size > max {
        return Room::TooBig;
    }
    // This batch alone is over the maximum: making room would mean deleting its own first items,
    // which is never done. What Windows does with a selection bigger than the bin isn't known for
    // sure (one report says it asks "These items are too big to recycle" about all of them); it
    // is to be checked on a VM. Until then the items past the maximum count as too big, like a
    // single item that doesn't fit, and Arke asks before deleting them for good.
    if mine.saturating_add(size) > max {
        return Room::TooBig;
    }
    match used.saturating_add(size).saturating_sub(max) {
        0 => Room::Fits,
        need => Room::Full { need },
    }
}

/// A bin's maximum size in bytes, from `MaxCapacity` (MB).
fn max_bytes(max_mb: u32) -> u64 {
    u64::from(max_mb).saturating_mul(1024 * 1024)
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
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    if !fixed_drive(root) {
        return false;
    }
    let root = windows::core::HSTRING::from(root.as_os_str());
    let mut fs_name = [0u16; 32];
    // SAFETY: root is NUL-terminated; the name buffer outlives the call.
    let named =
        unsafe { GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs_name)) }.is_ok();
    named
        && String::from_utf16_lossy(&fs_name)
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("NTFS")
}

/// A fixed drive (not a network, removable or optical one). Local mount information only.
fn fixed_drive(root: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_FIXED: u32 = 3;
    let root = windows::core::HSTRING::from(root.as_os_str());
    // SAFETY: root is NUL-terminated.
    unsafe { GetDriveTypeW(&root) == DRIVE_FIXED }
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

/// What the `$I` records already in a bin say about their layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// No records yet: nothing to compare with.
    Empty,
    /// Every record looked at is the version written here.
    Current,
    /// A record of another version (1 is Vista to 8.1, 2 is Windows 10 and later) is there.
    Other,
    /// A record couldn't be read or isn't well formed.
    Unreadable,
}

impl Layout {
    /// The verdict for the versions of the records looked at (`None`: not readable).
    /// A foreign version outweighs an unreadable record.
    fn judge(versions: impl IntoIterator<Item = Option<i64>>) -> Self {
        let mut verdict = Self::Empty;
        for version in versions {
            match version {
                Some(v) if v != VERSION => return Self::Other,
                Some(_) if verdict == Self::Empty => verdict = Self::Current,
                Some(_) => {}
                None => verdict = Self::Unreadable,
            }
        }
        verdict
    }

    /// Whether items may be written here directly: nothing seen says the layout differs.
    const fn allows(self) -> bool {
        matches!(self, Self::Empty | Self::Current)
    }

    /// Whether the verdict rests on records actually seen, so it can be remembered.
    const fn settled(self) -> bool {
        matches!(self, Self::Current | Self::Other)
    }
}

/// Verdicts per bin folder for this run of Arke.
static LAYOUTS: LazyLock<parking_lot::Mutex<std::collections::HashMap<PathBuf, bool>>> =
    LazyLock::new(Default::default);

/// The `$I` records already there are all the version written here (or there are none yet).
/// A verdict based on records is remembered per bin, so later deletes don't read any; an empty
/// bin or an unreadable record is looked at again next time.
fn layout_is_known(bin: &Path) -> bool {
    layout_allowed(&LAYOUTS, bin, || probe_bin(bin, |p| fs::read(p).ok()))
}

/// [`Layout::allows`] for `bin`, asking `probe` only when `cache` has no verdict for it.
fn layout_allowed(
    cache: &parking_lot::Mutex<std::collections::HashMap<PathBuf, bool>>,
    bin: &Path,
    probe: impl FnOnce() -> Layout,
) -> bool {
    if let Some(&known) = cache.lock().get(bin) {
        return known;
    }
    let layout = probe();
    if layout.settled() {
        cache.lock().insert(bin.to_path_buf(), layout.allows());
    }
    layout.allows()
}

/// Looks at the first [`PROBE_RECORDS`] `$I` records of `bin`, read with `read`.
fn probe_bin(bin: &Path, read: impl Fn(&Path) -> Option<Vec<u8>>) -> Layout {
    let Ok(entries) = fs::read_dir(bin) else {
        return Layout::Unreadable;
    };
    let versions = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("$I"))
        .take(PROBE_RECORDS)
        .map(|e| {
            read(&e.path())
                .and_then(|bytes| decode(&bytes))
                .map(|r| r.version)
        });
    Layout::judge(versions)
}

/// Total size of the files below `dir`, from directory listings (no file is opened). Links are
/// not followed. `None` if part of the tree can't be listed.
fn tree_size(dir: &Path) -> Option<u64> {
    tree_size_upto(dir, u64::MAX)
}

/// Whether the files below `dir` add up to more than `limit` bytes; stops listing as soon as they
/// do. `None` if part of the tree (listed before that) can't be listed.
fn tree_size_over(dir: &Path, limit: u64) -> Option<bool> {
    tree_size_upto(dir, limit).map(|total| total > limit)
}

/// Like [`tree_size`], but stops once the total is past `limit` and returns the total so far.
fn tree_size_upto(dir: &Path, limit: u64) -> Option<u64> {
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
                total = total.saturating_add(meta.file_size());
                if total > limit {
                    return Some(total);
                }
            }
        }
    }
    Some(total)
}

/// Per drive, the size above which Windows deletes an item instead of recycling it: for the items
/// [`Batch`] leaves to the engine without measuring them (fast recycle off, a drive or a policy it
/// leaves to the engine, links). Registry and volume information are read once per drive; then
/// a file costs its metadata and a folder the listings up to the limit.
#[derive(Default)]
pub(crate) struct Limits {
    /// "Maximum allowed Recycle Bin size" policy (percent of the drive), looked up when needed.
    policy: std::cell::OnceCell<Option<u32>>,
    /// Per drive root; `None` when it isn't known (no bin settings, not a fixed drive).
    drives: std::collections::HashMap<PathBuf, Option<u64>>,
}

impl Limits {
    /// `path` is bigger than its drive's Recycle Bin may hold, so Windows would delete it
    /// permanently. `false` whenever that isn't known for sure (the engine and its prompt decide).
    pub(crate) fn exceeds_bin(&mut self, path: &Path) -> bool {
        let Some(root) = drive_root(path) else {
            return false;
        };
        let policy = *self.policy.get_or_init(policy_size_percent);
        let Some(limit) = *self
            .drives
            .entry(root.clone())
            .or_insert_with(|| drive_limit(&root, policy))
        else {
            return false;
        };
        let Ok(meta) = fs::symlink_metadata(path) else {
            return false;
        };
        // Links and cloud placeholders: what the engine does with them doesn't depend on this.
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
        if meta.is_dir() {
            tree_size_over(path, limit) == Some(true)
        } else {
            meta.len() > limit
        }
    }
}

/// The bin limit of the drive at `root` (see [`bin_limit`]); `None` for a drive that isn't fixed
/// or whose bin deletes everything anyway. Registry and local volume information only.
fn drive_limit(root: &Path, policy_percent: Option<u32>) -> Option<u64> {
    if !fixed_drive(root) {
        return None;
    }
    let max_mb = match bin_settings(root) {
        Some((_, true)) => return None,
        Some((max_mb, false)) => Some(max_mb),
        None => None,
    };
    let volume = policy_percent.and_then(|_| volume_bytes(root));
    bin_limit(max_mb, policy_percent, volume)
}

/// Bytes above which an item can't go to the bin: the policy's share of the volume when the
/// "Maximum allowed Recycle Bin size" policy is set (it replaces each drive's own setting), else
/// the bin's own maximum (`MaxCapacity`, MB). `None` when that isn't known.
fn bin_limit(max_mb: Option<u32>, policy_percent: Option<u32>, volume: Option<u64>) -> Option<u64> {
    match policy_percent {
        Some(percent) => volume.map(|total| total / 100 * u64::from(percent.min(100))),
        None => max_mb.map(|mb| u64::from(mb).saturating_mul(1024 * 1024)),
    }
}

/// The "Maximum allowed Recycle Bin size" policy (`RecycleBinSize`, percent), the smaller of the
/// user's and the machine's. Read only.
fn policy_size_percent() -> Option<u32> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .filter_map(|hive| read_dword_in(hive, KEY, "RecycleBinSize"))
        .min()
}

/// Size of the volume at `root`, bytes. Local mount information (fixed drives only).
fn volume_bytes(root: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let root = windows::core::HSTRING::from(root.as_os_str());
    let mut total = 0u64;
    // SAFETY: root is NUL-terminated; the out pointer is valid for the call.
    unsafe { GetDiskFreeSpaceExW(&root, None, Some(&raw mut total), None) }.ok()?;
    (total > 0).then_some(total)
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

/// Deleting on the drive of `root` (`C:\`) never goes to the Recycle Bin: the user turned it off
/// for that drive ("Don't move files to the Recycle Bin"), or a policy did for every drive.
/// Registry reads only.
pub(crate) fn recycling_off(root: &Path) -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    let policy = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .any(|hive| read_dword_in(hive, KEY, "NoRecycleFiles").is_some_and(|v| v != 0));
    policy || bin_settings(root).is_some_and(|(_, nuke)| nuke)
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

/// Notes why an item is left to the engine; only recorded in tests, for their messages.
fn declined(why: impl FnOnce() -> String) {
    #[cfg(test)]
    DECLINES.lock().push(why());
    #[cfg(not(test))]
    let _ = why;
}

/// Every reason [`declined`] was given, for the messages of failing live tests.
#[cfg(test)]
static DECLINES: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

/// Whether Windows' item count for the bin at `root` went above `before` after an item was
/// moved in. The shell may take a moment to notice what was renamed into the bin, so the count
/// is asked again a few times (at most ~0.3 s, and only while a version of Windows is unconfirmed).
fn windows_counted(root: &Path, before: u64) -> bool {
    const TRIES: u32 = 6;
    const WAIT: std::time::Duration = std::time::Duration::from_millis(60);
    for attempt in 0..TRIES {
        if attempt > 0 {
            std::thread::sleep(WAIT);
        }
        let state = bin_state(root);
        #[cfg(test)]
        COUNT_CHECKS.lock().push(format!(
            "attempt {attempt}: before {before} items, now (bytes, items) {state:?}"
        ));
        if state.is_some_and(|(_, count)| count > before) {
            return true;
        }
    }
    false
}

/// What each count check saw, for the messages of failing live tests.
#[cfg(test)]
static COUNT_CHECKS: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

/// Names handed out so far: set apart from each other, whatever the clock says.
static NAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unused `$I`/`$R` names in `bin` for an item with extension `ext`.
fn free_names(bin: &Path, ext: &str) -> Option<(PathBuf, PathBuf)> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64
        ^ u64::from(std::process::id()).rotate_left(32)
        // The clock ticks every millisecond or so: threads asking within one tick (two deletes
        // at once) would get the same names and one of them would lose the race to create them.
        ^ NAMES.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
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

/// Tells the shell `event` happened at `path` (fast copy reports its new items with it too).
pub(crate) fn send(event: windows::Win32::UI::Shell::SHCNE_ID, path: &Path) {
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

/// The start of a `$I` record, the same in versions 1 (Vista to 8.1) and 2: all the bin's
/// size and age need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) version: i64,
    /// Bytes the item holds; a folder's is the sum of its files.
    pub(crate) size: u64,
    /// Deletion time, FILETIME (UTC).
    pub(crate) deleted: i64,
}

/// Bytes [`head`] reads.
pub(crate) const HEAD_LEN: usize = 24;

/// The start of a `$I` record of any version; `None` if it is too short.
pub(crate) fn head(bytes: &[u8]) -> Option<Head> {
    let (version, rest) = bytes.split_first_chunk::<8>()?;
    let (size, rest) = rest.split_first_chunk::<8>()?;
    let (deleted, _) = rest.split_first_chunk::<8>()?;
    Some(Head {
        version: i64::from_le_bytes(*version),
        size: u64::from_le_bytes(*size),
        deleted: i64::from_le_bytes(*deleted),
    })
}

/// The `$I` bytes for an item of `size` bytes deleted at `deleted` (FILETIME) from `path`.
pub(crate) fn encode(version: i64, size: u64, deleted: i64, path: &Path) -> Vec<u8> {
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

/// Whether Windows counts the items the fast path moves into a bin, checked once per version of
/// Windows instead of on every delete (#379).
///
/// On a version not seen before, the first item moved to a bin is checked: Windows' item count
/// for that drive (`SHQueryRecycleBinW`, which lists the whole bin, so it is slow on a big one)
/// must go up. The version is then noted in `%LOCALAPPDATA%\Arke\bin-usage\counted.txt` and never
/// checked again. If Windows didn't count it, the item is put back and every later one goes to
/// the engine until Arke restarts; that isn't noted, since it may have been a one-off (the bin
/// emptied at the same moment).
struct Counted {
    /// Where confirmed versions are noted; `None` notes nothing.
    file: Option<PathBuf>,
    /// This version of Windows, as noted there (`10.0.19045`).
    version: String,
    state: parking_lot::Mutex<Check>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// The file isn't read yet.
    Unread,
    Unchecked,
    Confirmed,
    /// Windows didn't count an item moved to a bin.
    Failed,
}

impl Counted {
    const fn new(file: Option<PathBuf>, version: String) -> Self {
        Self {
            file,
            version,
            state: parking_lot::Mutex::new(Check::Unread),
        }
    }

    fn check(&self) -> Check {
        let mut state = self.state.lock();
        if *state == Check::Unread {
            let noted = self
                .file
                .as_deref()
                .and_then(|file| fs::read_to_string(file).ok())
                .is_some_and(|text| lists(&text, &self.version));
            *state = if noted {
                Check::Confirmed
            } else {
                Check::Unchecked
            };
        }
        *state
    }

    /// Windows was seen counting an item moved to a bin on this version: no need to ask it.
    fn confirmed(&self) -> bool {
        self.check() == Check::Confirmed
    }

    /// The fast path may be used: Windows wasn't seen failing to count an item since Arke started.
    fn allowed(&self) -> bool {
        self.check() != Check::Failed
    }

    fn confirm(&self) {
        *self.state.lock() = Check::Confirmed;
        log::info!(
            "fast recycle: Windows {} counts items moved to the Recycle Bin; not checked again",
            self.version
        );
        let Some(file) = &self.file else {
            return;
        };
        let text = fs::read_to_string(file).unwrap_or_default();
        if lists(&text, &self.version) {
            return;
        }
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Err(e) = fs::write(file, with_version(&text, &self.version)) {
            log::warn!("fast recycle: {} not saved: {e}", file.display());
        }
    }

    fn fail(&self) {
        *self.state.lock() = Check::Failed;
    }
}

/// Whether a list of versions, one per line, has `version`.
fn lists(text: &str, version: &str) -> bool {
    text.lines().any(|line| line.trim() == version)
}

/// The list with `version` added at the end.
fn with_version(text: &str, version: &str) -> String {
    let mut out = String::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(version);
    out.push('\n');
    out
}

/// The check for this version of Windows. When the version can't be read, nothing is noted, so
/// it is checked again on each start.
fn counted() -> &'static Counted {
    static COUNTED: LazyLock<Counted> = LazyLock::new(|| {
        let version = crate::win::windows_version();
        let file = if cfg!(test) {
            None
        } else {
            version.and_then(|_| crate::paths::bin_usage_dir().map(|d| d.join("counted.txt")))
        };
        let version = version.map_or_else(
            || "unknown".to_owned(),
            |(major, minor, build)| format!("{major}.{minor}.{build}"),
        );
        Counted::new(file, version)
    });
    &COUNTED
}

/// Starts summing the system drive's bin in the background a little after Arke starts, so the
/// first delete there doesn't wait for it. Other drives start on their first delete (their disks
/// may be asleep). Nothing happens with the fast Recycle Bin off.
pub(crate) fn warm_up() {
    let _ = std::thread::Builder::new()
        .name("bin-usage-warm".into())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            if !enabled() || !policy_allows() {
                return;
            }
            let Some(sid) = user_sid() else {
                return;
            };
            let Some(drive) = std::env::var_os("SystemDrive") else {
                return;
            };
            let root = PathBuf::from(format!(r"{}\", drive.to_string_lossy()));
            if !local_ntfs(&root) {
                return;
            }
            let bin = root.join("$Recycle.Bin").join(sid);
            if bin.is_dir() {
                tracker().track(&bin);
            }
        });
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
    pub(crate) struct Sandbox(
        pub(crate) PathBuf,
        Vec<crate::shell::RecycledItem>,
        #[allow(dead_code, reason = "held for its Drop, which deletes the folder")]
        tempfile::TempDir,
    );

    impl Sandbox {
        pub(crate) fn new(tag: &str) -> Self {
            purge_leftovers(tag);
            let guard = crate::win::long_tempdir(&format!("arke-test-fastbin-{tag}-"));
            let primed = prime_bin();
            Self(guard.path().to_path_buf(), primed, guard)
        }
        pub(crate) fn file(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        }
    }

    /// Puts one throwaway file of its own in the bin through the engine and returns it, to be
    /// purged when the sandbox is dropped. The fast path leaves a drive to the engine until its
    /// bin has a folder for this user, an `$I` record to check the layout against, and a count
    /// Windows keeps up to date; on a PC that has deleted something that is so, but a fresh
    /// machine such as a CI runner has an empty bin. The file comes from a folder of its own, so
    /// it never shows among what a test finds from its sandbox.
    fn prime_bin() -> Vec<crate::shell::RecycledItem> {
        let dir = crate::win::long_temp_dir()
            .join(format!("arke-test-fastbin-prime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        if fs::create_dir_all(&dir).is_err() {
            return Vec::new();
        }
        let file = dir.join("prime-the-bin.tmp");
        let primed = fs::write(&file, "prime").is_ok()
            && crate::win_fileops::perform(
                &crate::shell::FileOp::Delete {
                    paths: vec![file],
                    recycle: true,
                },
                None,
                true,
            )
            .is_ok();
        let here = fs::canonicalize(&dir).ok();
        let items = if primed {
            crate::win_recycle::list()
                .unwrap_or_default()
                .into_iter()
                .filter(|item| fs::canonicalize(&item.from).ok() == here)
                .collect()
        } else {
            Vec::new()
        };
        let _ = fs::remove_dir_all(&dir);
        items
    }

    /// Waits until `folder` can be moved: right after a test fills it, antivirus may still hold
    /// a file inside open, and the rename into the bin is then refused (the real path leaves
    /// such a folder to the engine, which is right). Renames it to a sibling name and back until
    /// both succeed; panics if something outside the test keeps it open.
    pub(crate) fn settle(folder: &Path) {
        let mut aside = folder.as_os_str().to_owned();
        aside.push(".settle");
        let aside = PathBuf::from(aside);
        let started = std::time::Instant::now();
        let mut last = None;
        while started.elapsed() < std::time::Duration::from_secs(5) {
            match fs::rename(folder, &aside) {
                Ok(()) => {
                    // Back where it was; it can't be held now unless the check itself raced.
                    while let Err(e) = fs::rename(&aside, folder) {
                        assert!(
                            started.elapsed() < std::time::Duration::from_secs(10),
                            "{} couldn't be moved back: {e}",
                            aside.display()
                        );
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    return;
                }
                Err(e) => last = Some(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!(
            "something outside the test kept {} open for 5 s: {:?}",
            folder.display(),
            last
        );
    }

    /// Why items were left to the engine so far in this process (see `declined`).
    pub(crate) fn declines() -> Vec<String> {
        super::DECLINES.lock().clone()
    }

    /// Why the fast path would leave `path` to the engine, one line per condition `Drive::check`
    /// tests with its value. For the messages of failing live tests only.
    pub(crate) fn diagnose(path: &Path) -> String {
        use std::fmt::Write as _;
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let mut out = format!("\n  path: {}", path.display());
        let Some(root) = super::drive_root(path) else {
            return out + "\n  drive_root: none";
        };
        let host = windows::core::HSTRING::from(root.as_os_str());
        let mut fs_name = [0u16; 32];
        let mut flags = 0u32;
        // SAFETY: root is NUL-terminated; the out buffers outlive the call.
        let info = unsafe {
            GetVolumeInformationW(
                &host,
                None,
                None,
                None,
                Some(&raw mut flags),
                Some(&mut fs_name),
            )
        };
        let _ = write!(
            out,
            "\n  root: {}\n  temp dir: {}\n  file system: {:?} (query ok: {}, flags {flags:#x})\n  fixed drive: {}\n  local_ntfs: {}",
            root.display(),
            crate::win::long_temp_dir().display(),
            String::from_utf16_lossy(&fs_name).trim_end_matches('\0'),
            info.is_ok(),
            super::fixed_drive(&root),
            super::local_ntfs(&root),
        );
        let _ = write!(
            out,
            "\n  policy_allows: {}\n  counted().allowed: {}",
            super::policy_allows(),
            super::counted().allowed()
        );
        let Some(sid) = super::user_sid() else {
            return out + "\n  user_sid: none";
        };
        let bin = root.join("$Recycle.Bin").join(&sid);
        let _ = write!(
            out,
            "\n  sid: {sid}\n  bin: {} (is_dir: {})",
            bin.display(),
            bin.is_dir()
        );
        let _ = write!(
            out,
            "\n  bin entries: {:?}",
            fs::read_dir(&bin).map(|d| d
                .flatten()
                .map(|e| e.file_name())
                .take(12)
                .collect::<Vec<_>>())
        );
        let _ = write!(
            out,
            "\n  layout (probe): {:?}",
            super::probe_bin(&bin, |p| fs::read(p).ok())
        );
        let guid = {
            use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
            let mut name = [0u16; 64];
            // SAFETY: host is NUL-terminated; the buffer outlives the call.
            let got = unsafe { GetVolumeNameForVolumeMountPointW(&host, &mut name) };
            format!(
                "{:?}: {}",
                got,
                String::from_utf16_lossy(&name).trim_end_matches('\0')
            )
        };
        let _ = write!(
            out,
            "\n  volume name: {guid}\n  bin_settings (MaxCapacity MB, NukeOnDelete): {:?}",
            super::bin_settings(&root)
        );
        let _ = write!(out, "\n  declined: {:#?}", super::DECLINES.lock());
        let _ = write!(out, "\n  count checks: {:#?}", super::COUNT_CHECKS.lock());
        let _ = write!(
            out,
            "\n  tracker bytes: {:?}\n  bin_state (SHQueryRecycleBin bytes, items): {:?}\n  counted confirmed: {}",
            super::tracker().bytes(&bin),
            super::bin_state(&root),
            super::counted().confirmed()
        );
        out
    }

    /// Attributes and size of every item below `dir`, for a failing test's message.
    pub(crate) fn describe_tree(dir: &Path) -> String {
        use std::fmt::Write as _;
        use std::os::windows::fs::MetadataExt as _;
        let mut out = String::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(d) = pending.pop() {
            let Ok(entries) = fs::read_dir(&d) else {
                let _ = write!(out, "\n  {}: can't list", d.display());
                continue;
            };
            for e in entries.flatten() {
                match e.metadata() {
                    Ok(m) => {
                        let _ = write!(
                            out,
                            "\n  {} attributes {:#x}, {} bytes",
                            e.path().display(),
                            m.file_attributes(),
                            m.len()
                        );
                        if m.is_dir() {
                            pending.push(e.path());
                        }
                    }
                    Err(err) => {
                        let _ = write!(out, "\n  {}: {err}", e.path().display());
                    }
                }
            }
        }
        out
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
            if !self.1.is_empty() {
                let _ = crate::win_fileops::purge_recycled(
                    &self.1,
                    &crate::shell::OpProgress::default(),
                );
            }
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
    fn a_full_bin_makes_room_but_a_batch_never_pushes_out_its_own() {
        let mb = 1024 * 1024;
        assert_eq!(room(0, 0, 10 * mb, 10), Room::Fits, "exactly the maximum");
        assert_eq!(
            room(1, 0, 10 * mb, 10),
            Room::Full { need: 1 },
            "one byte over: older items make room"
        );
        assert_eq!(room(5 * mb, 0, 6 * mb, 10), Room::Full { need: mb });
        assert_eq!(room(0, 0, 10 * mb + 1, 10), Room::TooBig, "over on its own");
        assert_eq!(
            room(0, 0, 1, 0),
            Room::TooBig,
            "a bin of size 0 takes nothing"
        );
        assert_eq!(
            room(9 * mb, 4 * mb, 3 * mb, 10),
            Room::Full { need: 2 * mb },
            "room from items older than this batch"
        );
        assert_eq!(
            room(8 * mb, 8 * mb, 3 * mb, 10),
            Room::TooBig,
            "room would mean pushing out this batch's own items"
        );
        assert_eq!(
            room(u64::MAX, 0, 1, u32::MAX),
            Room::Full {
                need: u64::MAX - u64::from(u32::MAX) * mb
            },
            "no overflow"
        );
    }

    #[test]
    fn record_heads_read_any_version() {
        let path = Path::new(r"C:\x.txt");
        let bytes = encode(2, 10, 0x01DD_735E_EFCE_2ED0, path);
        assert_eq!(
            head(&bytes),
            Some(Head {
                version: 2,
                size: 10,
                deleted: 0x01DD_735E_EFCE_2ED0
            })
        );
        let mut v1 = bytes.clone();
        v1[..8].copy_from_slice(&1i64.to_le_bytes());
        assert_eq!(head(&v1).map(|h| (h.version, h.size)), Some((1, 10)));
        assert_eq!(head(&bytes[..HEAD_LEN]).map(|h| h.size), Some(10));
        assert_eq!(head(&bytes[..HEAD_LEN - 1]), None, "too short");
    }

    #[test]
    fn windows_counting_moved_items_is_checked_once_per_version() {
        let dir = Scratch::new("counted");
        let file = dir.0.join("counted.txt");
        let first = Counted::new(Some(file.clone()), "10.0.19045".into());
        assert!(!first.confirmed() && first.allowed(), "not checked yet");
        first.confirm();
        assert!(first.confirmed());
        let later = Counted::new(Some(file.clone()), "10.0.19045".into());
        assert!(later.confirmed(), "remembered: not checked again");
        let updated = Counted::new(Some(file.clone()), "10.0.26100".into());
        assert!(
            !updated.confirmed(),
            "a new version of Windows is checked again"
        );
        updated.confirm();
        let text = fs::read_to_string(&file).unwrap();
        assert!(lists(&text, "10.0.19045") && lists(&text, "10.0.26100"));

        let failing = Counted::new(Some(file.clone()), "10.0.22631".into());
        failing.fail();
        assert!(
            !failing.allowed() && !failing.confirmed(),
            "off until Arke restarts"
        );
        assert!(
            Counted::new(Some(file), "10.0.22631".into()).allowed(),
            "a failure isn't remembered"
        );
        assert!(!Counted::new(None, "10.0.19045".into()).confirmed());
    }

    #[test]
    fn version_lists_ignore_blank_lines_and_spaces() {
        assert!(lists(" 10.0.19045 \r\n", "10.0.19045"));
        assert!(!lists("10.0.190451", "10.0.19045"));
        assert!(!lists("", "10.0.19045"));
        assert_eq!(with_version("a\n\n b \n", "c"), "a\nb\nc\n");
        assert_eq!(with_version("", "c"), "c\n");
    }

    #[test]
    fn the_policy_share_replaces_the_bins_own_maximum() {
        let mb = 1024 * 1024;
        assert_eq!(bin_limit(None, None, None), None, "nothing known");
        assert_eq!(bin_limit(Some(10), None, None), Some(10 * mb));
        assert_eq!(bin_limit(Some(0), None, None), Some(0), "a bin of size 0");
        assert_eq!(
            bin_limit(Some(10), Some(5), None),
            None,
            "the policy decides, and the volume size is unknown"
        );
        assert_eq!(bin_limit(None, Some(5), None), None);
        assert_eq!(
            bin_limit(Some(10), Some(5), Some(100 * mb)),
            Some(5 * mb),
            "the policy's share, though smaller"
        );
        assert_eq!(
            bin_limit(Some(1), Some(5), Some(100 * mb)),
            Some(5 * mb),
            "the policy's share, though the bin's own maximum is smaller"
        );
        assert_eq!(
            bin_limit(None, Some(10), Some(1000)),
            Some(100),
            "policy alone"
        );
        assert_eq!(
            bin_limit(None, Some(250), Some(1000)),
            Some(1000),
            "at most the whole drive"
        );
        assert_eq!(
            bin_limit(Some(u32::MAX), None, None),
            Some(u64::from(u32::MAX) * mb),
            "no overflow"
        );
    }

    /// A folder under %TEMP% for tests that only write and list their own files; removed after.
    struct Scratch(
        PathBuf,
        #[allow(dead_code, reason = "held for its Drop, which deletes the folder")]
        tempfile::TempDir,
    );

    impl Scratch {
        fn new(tag: &str) -> Self {
            let guard = crate::win::long_tempdir(&format!("arke-test-binlimit-{tag}-"));
            Self(guard.path().to_path_buf(), guard)
        }
    }

    #[test]
    fn only_records_of_the_written_version_allow_direct_writes() {
        let judge = |v: &[Option<i64>]| Layout::judge(v.iter().copied());
        assert_eq!(judge(&[]), Layout::Empty);
        assert_eq!(judge(&[Some(2), Some(2)]), Layout::Current);
        assert_eq!(judge(&[Some(2), Some(1)]), Layout::Other);
        assert_eq!(judge(&[Some(3)]), Layout::Other);
        assert_eq!(judge(&[Some(2), None]), Layout::Unreadable);
        assert_eq!(judge(&[None, Some(1)]), Layout::Other, "foreign wins");
        let allowed: Vec<bool> = [
            Layout::Empty,
            Layout::Current,
            Layout::Other,
            Layout::Unreadable,
        ]
        .map(Layout::allows)
        .to_vec();
        assert_eq!(allowed, [true, true, false, false]);
    }

    #[test]
    fn records_are_probed_through_the_injected_read() {
        let sb = Scratch::new("layout-probe");
        let bin = &sb.0;
        for name in ["$IAAAAAA.txt", "$RAAAAAA.txt", "desktop.ini"] {
            fs::write(bin.join(name), "x").unwrap();
        }
        let record = |version: i64| encode(version, 1, 0, Path::new(r"C:\a.txt"));
        assert_eq!(probe_bin(bin, |_| Some(record(2))), Layout::Current);
        assert_eq!(probe_bin(bin, |_| Some(record(1))), Layout::Other);
        assert_eq!(probe_bin(bin, |_| None), Layout::Unreadable);
        assert_eq!(
            probe_bin(bin, |_| Some(vec![2, 0])),
            Layout::Unreadable,
            "too short"
        );
        assert_eq!(
            probe_bin(&bin.join("missing"), |_| Some(record(2))),
            Layout::Unreadable
        );
    }

    #[test]
    fn a_settled_verdict_is_remembered_and_others_are_asked_again() {
        let cache = parking_lot::Mutex::new(std::collections::HashMap::new());
        let asked = std::cell::Cell::new(0);
        let ask = |bin: &str, layout: Layout| {
            layout_allowed(&cache, Path::new(bin), || {
                asked.set(asked.get() + 1);
                layout
            })
        };
        // A foreign version: the engine, and no second read of the bin.
        assert!(!ask(r"C:\bin", Layout::Other));
        assert!(!ask(r"C:\bin", Layout::Current), "cached, not asked");
        assert_eq!(asked.get(), 1);
        // The version written here is remembered per bin.
        assert!(ask(r"D:\bin", Layout::Current));
        assert!(ask(r"D:\bin", Layout::Other), "cached, not asked");
        assert_eq!(asked.get(), 2);
        // Nothing to compare with, or unreadable: asked again each time.
        assert!(ask(r"E:\bin", Layout::Empty));
        assert!(ask(r"E:\bin", Layout::Empty));
        assert!(!ask(r"F:\bin", Layout::Unreadable));
        assert!(!ask(r"F:\bin", Layout::Unreadable));
        assert_eq!(asked.get(), 6);
    }

    /// Ten files of 100 bytes and a subfolder with 500 more.
    fn sized_tree(dir: &Path) {
        for i in 0..10 {
            fs::write(dir.join(format!("f{i}.bin")), [0u8; 100]).unwrap();
        }
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join(r"sub\deep.bin"), [0u8; 500]).unwrap();
    }

    #[test]
    fn folder_sizes_stop_listing_once_past_the_limit() {
        let sb = Scratch::new("treesize");
        sized_tree(&sb.0);
        assert_eq!(tree_size(&sb.0), Some(1500));
        assert_eq!(
            tree_size_over(&sb.0, 1500),
            Some(false),
            "exactly the limit fits"
        );
        assert_eq!(tree_size_over(&sb.0, 1499), Some(true));
        // Past the limit at the third file of the top folder: no more of its files are counted,
        // and the subfolder (listed after it) never is.
        assert_eq!(tree_size_upto(&sb.0, 250), Some(300));
        assert_eq!(tree_size_over(&sb.0, 250), Some(true));
        assert_eq!(
            tree_size_over(&sb.0.join("missing"), 1),
            None,
            "can't be listed"
        );
    }

    #[test]
    fn items_over_the_drive_limit_are_too_big_for_the_bin() {
        let sb = Scratch::new("exceeds");
        sized_tree(&sb.0);
        let root = drive_root(&sb.0).expect("%TEMP% is on a drive letter");
        let limits = |limit: Option<u64>| Limits {
            policy: std::cell::OnceCell::from(None),
            drives: std::iter::once((root.clone(), limit)).collect(),
        };
        let mut limits_1000 = limits(Some(1000));
        assert!(limits_1000.exceeds_bin(&sb.0), "the folder's total is over");
        assert!(!limits_1000.exceeds_bin(&sb.0.join("sub")), "500 fits");
        assert!(!limits_1000.exceeds_bin(&sb.0.join("f0.bin")));
        assert!(
            limits(Some(99)).exceeds_bin(&sb.0.join("f0.bin")),
            "a file over"
        );
        assert!(
            !limits(None).exceeds_bin(&sb.0),
            "an unknown limit leaves it to the engine"
        );
        assert!(!limits_1000.exceeds_bin(&sb.0.join("missing")), "gone");
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

    /// The fast path end to end: the folder (a file and a subfolder inside) is in the bin with
    /// the right name, origin and size, then comes back where it was with everything in it, and
    /// leaves no record behind.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn a_folder_goes_to_the_bin_and_comes_back() {
        let sb = Sandbox::new("roundtrip");
        sb.file(r"photos.v2\a.bin", "1234567");
        sb.file(r"photos.v2\sub\b.bin", "123");
        let folder = sb.0.join("photos.v2");
        test_support::settle(&folder);

        let Some(now) = recycle(&folder) else {
            panic!(
                "this drive should take the fast path (fixed NTFS, bin folder present){}",
                test_support::diagnose(&folder)
            );
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
        assert!(recycled_from(&sb.0).is_empty(), "no record left behind");
    }

    /// A folder holding a read-only file goes to the bin and comes back with the flag. The rename
    /// into the bin may be refused (then the engine takes it), so only the outcome is checked,
    /// not which of the two did it.
    #[test]
    #[ignore = "uses the real Recycle Bin (only this test's own temp items, purged at the end)"]
    fn a_folder_with_a_read_only_file_goes_to_the_bin_and_comes_back() {
        let sb = Sandbox::new("readonly");
        sb.file(r"docs.v2\a.bin", "1234567");
        sb.file(r"docs.v2\sub\b.bin", "123");
        let ro = sb.0.join(r"docs.v2\a.bin");
        let mut perms = fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ro, perms).unwrap();
        let folder = sb.0.join("docs.v2");
        test_support::settle(&folder);

        if recycle(&folder).is_none() {
            assert_eq!(
                engine(vec![folder.clone()]),
                Ok(OpOutcome::Done),
                "the fast path declined it ({:?}) and the engine failed too",
                test_support::declines()
            );
        }
        assert!(!folder.exists());
        let items = recycled_from(&sb.0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "docs.v2");
        assert!(items[0].is_dir);
        assert_eq!(items[0].size, 10);

        let back = Undo::PutBack(vec![PutBack {
            now: items[0].now.clone(),
            parent: sb.0.clone(),
            name: "docs.v2".into(),
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
        .map_err(|e| format!("{e} / {e:?}"))
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
            test_support::settle(&fast_item);
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
                crate::win_fileops::undo(&back, None, true).map_err(|e| format!("{e} / {e:?}")),
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
        test_support::settle(&folder);
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

    /// Sizes Windows records for files with an alternate data stream, and hidden and system
    /// ones (not read-only: the engine in silent mode refuses read-only system files, where a
    /// real delete asks), match what the fast path records.
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
                .args(["+h", "+s"])
                .arg(&flagged)
                .status()
                .unwrap()
                .success();
            assert!(ok, "attrib");
            folder
        });
        assert_eq!(
            engine(vec![engine_item.clone()]),
            Ok(OpOutcome::Done),
            "the engine refused the folder; what is in it:{}{}",
            test_support::describe_tree(&engine_item),
            test_support::diagnose(&engine_item)
        );
        test_support::settle(&fast_item);
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
        for item in &items {
            test_support::settle(item);
        }
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
        assert!(
            done.iter().all(|(_, now)| now.is_some()),
            "all taken ({} of {ITEMS} were){}",
            done.iter().filter(|(_, now)| now.is_some()).count(),
            test_support::diagnose(&sb.0)
        );
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
