//! Where [`crate::fs::RealFs`] calls Win32 directly instead of going through std and notify:
//!
//! - [`DirWatcher`]: one `ReadDirectoryChangesW` per watched folder, on its own thread, waiting
//!   with no timeout. notify 8.2 wakes every 100 ms per watch, and when the change buffer
//!   overflows it either logs the error and stops watching or hands back an empty buffer, so a
//!   folder that got thousands of new files at once (an extracted archive) kept showing the old
//!   list. Here an overflow is reported as [`WatchEvent::Rescan`] and watching goes on.
//! - [`scan_sizes`]: one directory read for the folder size walk, with `FindFirstFileExW` (basic
//!   info, large fetch) and no allocation per file.
//! - [`read_dir`]: folder listings, the same entries as std's but fetched in large batches (one
//!   round trip to a file server per few hundred entries instead of a few dozen).
//! - [`is_network`]: whether a path is on a file server, from the drive type Windows records.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetDriveTypeW, OPEN_EXISTING, ReadDirectoryChangesW, WIN32_FIND_DATAW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::PCWSTR;

use crate::fs::{SizeItem, WatchCallback, WatchEvent};

/// Change buffer of one watch. The kernel keeps a buffer of this size for changes made while no
/// read is pending; 64 KB (about 1,500 changes with short names) is the most a network share
/// accepts.
const BUF_BYTES: usize = 64 * 1024;
/// How long [`DirWatcher::close`] waits for the handle to be closed.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Name surrogate bit of a reparse tag: symlinks and junctions (std treats these as links).
const NAME_SURROGATE: u32 = 0x2000_0000;

/// An owned kernel handle.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: kernel handles may be used from any thread; the owner decides when to close.
unsafe impl Send for Handle {}
// SAFETY: as above; SetEvent / waits on a shared event handle are thread safe.
unsafe impl Sync for Handle {}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn event() -> io::Result<Handle> {
    // SAFETY: plain manual-reset event without name or security attributes.
    let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
    Ok(Handle(handle))
}

/// Watches one folder (or a tree) until dropped. Dropping only signals the thread to stop, so it
/// never blocks; [`Self::close`] also waits until the directory handle is closed.
pub(crate) struct DirWatcher {
    stop: Arc<Handle>,
    done: flume::Receiver<()>,
}

impl DirWatcher {
    /// Opens `path` and starts watching on a new thread; returns once the first read is pending,
    /// so changes made after this returns are seen. Can block on network paths; background
    /// threads only.
    pub(crate) fn start(
        path: &Path,
        recursive: bool,
        on_event: Arc<WatchCallback>,
    ) -> io::Result<Self> {
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let dir = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }?;
        let dir = Handle(dir);
        let stop = Arc::new(event()?);
        let io_done = event()?;
        let (done_tx, done) = flume::bounded(1);
        let (ready_tx, ready) = flume::bounded(1);
        let thread_stop = Arc::clone(&stop);
        let root = path.to_path_buf();
        std::thread::Builder::new()
            .name("dir-watch".into())
            .spawn(move || {
                let watch = Watch {
                    dir: &dir,
                    io_done: &io_done,
                    stop: &thread_stop,
                    root: &root,
                    recursive,
                };
                watch.run(&on_event, &ready_tx);
                drop(dir);
                let _ = done_tx.send(());
            })?;
        let watcher = Self { stop, done };
        match ready.recv() {
            Ok(Ok(())) => Ok(watcher),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(io::Error::other("watch thread ended")),
        }
    }

    /// Stops watching and returns once the directory handle is closed (or after 2 s).
    pub(crate) fn close(self) {
        self.signal();
        let _ = self.done.recv_timeout(CLOSE_WAIT);
    }

    fn signal(&self) {
        // SAFETY: the event handle is alive while `self.stop` is.
        let _ = unsafe { SetEvent(self.stop.0) };
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.signal();
    }
}

/// One watch, as its thread sees it.
struct Watch<'a> {
    dir: &'a Handle,
    io_done: &'a Handle,
    stop: &'a Handle,
    root: &'a Path,
    recursive: bool,
}

impl Watch<'_> {
    /// Reads changes until stopped or the folder can no longer be watched (deleted, drive gone).
    /// Reports on `ready` whether the first read could be started.
    fn run(&self, on_event: &WatchCallback, ready: &flume::Sender<io::Result<()>>) {
        let Self {
            dir,
            io_done,
            stop,
            root,
            recursive,
        } = *self;
        let mut first = true;
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_ATTRIBUTES
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_CREATION
            | FILE_NOTIFY_CHANGE_SECURITY;
        // u32s: ReadDirectoryChangesW wants a DWORD-aligned buffer.
        let mut buf = vec![0u32; BUF_BYTES / 4];
        loop {
            let mut overlapped = OVERLAPPED {
                hEvent: io_done.0,
                ..OVERLAPPED::default()
            };
            // SAFETY: `buf` and `overlapped` outlive the request: it either completes before we read
            // them, or is cancelled and waited for below before they go out of scope.
            let issued = unsafe {
                ReadDirectoryChangesW(
                    dir.0,
                    buf.as_mut_ptr().cast(),
                    BUF_BYTES as u32,
                    recursive,
                    filter,
                    None,
                    Some(&raw mut overlapped),
                    None,
                )
            };
            if first {
                first = false;
                let _ = ready.send(issued.clone().map_err(io::Error::from));
            }
            if let Err(err) = issued {
                log::debug!("stopped watching {}: {err}", root.display());
                return;
            }
            // SAFETY: both handles are alive for the whole call.
            let woke = unsafe { WaitForMultipleObjects(&[io_done.0, stop.0], false, INFINITE) };
            let mut bytes = 0u32;
            if woke != WAIT_OBJECT_0 {
                // Stop (or a failed wait): cancel the read and wait until the kernel lets go of `buf`.
                // SAFETY: `overlapped` is the pending request on `dir`.
                unsafe {
                    let _ = CancelIoEx(dir.0, Some(&raw const overlapped));
                    let _ = GetOverlappedResult(dir.0, &raw const overlapped, &raw mut bytes, true);
                }
                return;
            }
            // SAFETY: the request completed (its event is set).
            let result =
                unsafe { GetOverlappedResult(dir.0, &raw const overlapped, &raw mut bytes, false) };
            match result {
                // The buffer overflowed: the changes are lost, the folder has to be listed again.
                Ok(()) if bytes == 0 => on_event(WatchEvent::Rescan),
                Ok(()) => {
                    // SAFETY: u32 to u8 view of the same initialized memory.
                    let raw: &[u8] =
                        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 4) };
                    let len = (bytes as usize).min(raw.len());
                    on_event(WatchEvent::Changed(changed_paths(&raw[..len], root)));
                }
                Err(err) if err.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                    on_event(WatchEvent::Rescan);
                }
                Err(err) if err.code() == ERROR_OPERATION_ABORTED.to_hresult() => return,
                Err(err) => {
                    log::debug!("stopped watching {}: {err}", root.display());
                    return;
                }
            }
        }
    }
}

/// Paths named in a buffer of `FILE_NOTIFY_INFORMATION` records.
fn changed_paths(raw: &[u8], root: &Path) -> Vec<PathBuf> {
    const HEADER: usize = 12; // NextEntryOffset, Action, FileNameLength
    let u32_at = |at: usize| {
        raw.get(at..at + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let mut paths = Vec::new();
    let mut at = 0usize;
    while let (Some(next), Some(name_bytes)) = (u32_at(at), u32_at(at + 8)) {
        let start = at + HEADER;
        let Some(name) = raw.get(start..start + name_bytes as usize) else {
            break;
        };
        let units: Vec<u16> = name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        paths.push(root.join(OsString::from_wide(&units)));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    paths
}

/// Converts a FILETIME (100 ns intervals since 1601) the way std does, so the result compares
/// equal to `Metadata::modified()` of the same entry.
fn filetime(low: u32, high: u32) -> Option<SystemTime> {
    const UNIX_EPOCH_INTERVALS: u64 = 116_444_736_000_000_000;
    let intervals = (u64::from(high) << 32) | u64::from(low);
    let to_duration = |n: u64| Duration::new(n / 10_000_000, ((n % 10_000_000) * 100) as u32);
    if intervals >= UNIX_EPOCH_INTERVALS {
        SystemTime::UNIX_EPOCH.checked_add(to_duration(intervals - UNIX_EPOCH_INTERVALS))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(to_duration(UNIX_EPOCH_INTERVALS - intervals))
    }
}

/// Owned `FindFirstFileExW` handle.
struct Find(HANDLE);

impl Drop for Find {
    fn drop(&mut self) {
        // SAFETY: owned find handle, closed once.
        let _ = unsafe { FindClose(self.0) };
    }
}

/// Longest `dir\*` pattern (in UTF-16 units) passed to `FindFirstFileExW` as is; longer ones
/// need the `\\?\` form, which std builds (see [`read_dir`]).
const FIND_PATTERN_MAX: usize = 247;

/// Lists `dir` with `FindFirstFileExW` asking for large fetches, the same entries as
/// `std::fs::read_dir` (which asks for small ones). On a network share each fetch is a round
/// trip to the server, so a folder of a few hundred files comes back in one instead of a dozen.
/// `None` for paths too long for a plain pattern: std lists those.
pub(crate) fn read_dir(dir: &Path) -> Option<io::Result<crate::fs::DirIter>> {
    // std refuses a path with a NUL in it; passed on, Windows would read it only up to the NUL.
    if dir.as_os_str().encode_wide().any(|c| c == 0) {
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        )));
    }
    let pattern = wide(&dir.join("*"));
    if pattern.len() > FIND_PATTERN_MAX + 1 {
        return None;
    }
    let volume = volume_of(dir);
    if no_large_fetch().lock().contains(&volume) {
        return None;
    }
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated; `data` is a WIN32_FIND_DATAW as FindExInfoBasic wants.
    let find = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    match find {
        Ok(find) => Some(Ok(Box::new(FindEntries {
            find: Some(Find(find)),
            first: Some(data),
        }))),
        Err(err) => match find_failure(win32_code(&err)) {
            // Nothing matched, as for the root of an empty drive (which has no `.`): std lists
            // that as empty too.
            FindFailure::Empty => Some(Ok(Box::new(std::iter::empty()))),
            FindFailure::Error => Some(Err(err.into())),
            FindFailure::Unsupported => {
                log::info!(
                    "{volume} doesn't take large directory fetches ({err}), listing it like std"
                );
                no_large_fetch().lock().insert(volume);
                None
            }
            FindFailure::Other => None,
        },
    }
}

/// What a failed `FindFirstFileExW` means for [`read_dir`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FindFailure {
    /// Nothing matched: an empty listing.
    Empty,
    /// The folder can't be listed (missing, access denied, the network is down): std would fail
    /// the same way, and trying again only waits on the network twice.
    Error,
    /// The file system or redirector rejects the large-fetch or basic-info flags: list it (and
    /// the rest of that volume) the way std does.
    Unsupported,
    /// Anything else: let std try.
    Other,
}

fn find_failure(code: u32) -> FindFailure {
    /// `ERROR_INVALID_FUNCTION`, `ERROR_NOT_SUPPORTED`, `ERROR_INVALID_PARAMETER`,
    /// `ERROR_INVALID_LEVEL`.
    const UNSUPPORTED: [u32; 4] = [1, 50, 87, 124];
    /// Path not found, access denied, a file not a folder, and network failures: bad network
    /// path, unexpected network error, name deleted, bad network name, semaphore timeout,
    /// network unreachable, host unreachable, no network provider, logon failure.
    const ERRORS: [u32; 13] = [3, 5, 267, 53, 59, 64, 67, 121, 1231, 1232, 1203, 1326, 1222];
    match code {
        2 => FindFailure::Empty,
        c if ERRORS.contains(&c) => FindFailure::Error,
        c if UNSUPPORTED.contains(&c) => FindFailure::Unsupported,
        _ => FindFailure::Other,
    }
}

/// The Win32 error code inside an HRESULT from windows-rs (`0x8007xxxx`).
const fn win32_code(err: &windows::core::Error) -> u32 {
    let hr = err.code().0.cast_unsigned();
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        hr
    }
}

/// Volumes (`x:` or `\\server\share`, lowercase) whose listings go through std: they rejected
/// a large fetch.
fn no_large_fetch() -> &'static parking_lot::Mutex<std::collections::HashSet<String>> {
    static VOLUMES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    VOLUMES.get_or_init(Default::default)
}

/// The volume part of a path (`x:` or `\\server\share`), lowercase.
fn volume_of(path: &Path) -> String {
    path.components()
        .next()
        .map_or_default(|c| c.as_os_str().to_string_lossy().to_lowercase())
}

/// Whether `path` is on a file server: `\\server\share\…` or a mapped drive. Reads the drive
/// type Windows records, no network access.
pub(crate) fn is_network(path: &Path) -> bool {
    /// `GetDriveTypeW`: a network drive.
    const DRIVE_REMOTE: u32 = 4;
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\UNC\") {
        return true;
    }
    if text.starts_with(r"\\") {
        return !text.starts_with(r"\\?\") && !text.starts_with(r"\\.\");
    }
    let mut chars = text.chars();
    let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
        return false;
    };
    if !letter.is_ascii_alphabetic() {
        return false;
    }
    let root = wide(Path::new(&format!("{letter}:\\")));
    // SAFETY: `root` is a NUL-terminated `X:\`; only reads the recorded drive type.
    (unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }) == DRIVE_REMOTE
}

/// Entries of one `FindFirstFileExW` listing, read as they're asked for.
struct FindEntries {
    /// `None` once the listing has ended (or failed).
    find: Option<Find>,
    /// What `FindFirstFileExW` returned, not handed out yet.
    first: Option<WIN32_FIND_DATAW>,
}

// SAFETY: a find handle may be used from any thread, one at a time (the iterator is not Sync).
unsafe impl Send for FindEntries {}

impl Iterator for FindEntries {
    type Item = io::Result<crate::entry::Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let find = self.find.as_ref()?;
            let data = if let Some(first) = self.first.take() {
                first
            } else {
                let mut data = WIN32_FIND_DATAW::default();
                // SAFETY: valid find handle and output buffer.
                if let Err(err) = unsafe { FindNextFileW(find.0, &raw mut data) } {
                    self.find = None;
                    return (err.code() != ERROR_NO_MORE_FILES.to_hresult())
                        .then(|| Err(err.into()));
                }
                data
            };
            if let Some(entry) = entry_of(&data) {
                return Some(Ok(entry));
            }
        }
    }
}

/// The entry `std::fs::DirEntry` would give for this find record; `None` for `.` and `..`.
fn entry_of(data: &WIN32_FIND_DATAW) -> Option<crate::entry::Entry> {
    use crate::entry::{Entry, EntryKind};
    let len = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.cFileName.len());
    let name = &data.cFileName[..len];
    if name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')] {
        return None;
    }
    let attributes = data.dwFileAttributes;
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    // dwReserved0 holds the reparse tag when the entry is a reparse point; std counts name
    // surrogates (symlinks, junctions) as links.
    let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        && data.dwReserved0 & NAME_SURROGATE != 0
    {
        EntryKind::Link { is_dir }
    } else if is_dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    let size = if kind == EntryKind::File {
        (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow)
    } else {
        0
    };
    let time =
        |t: windows::Win32::Foundation::FILETIME| filetime(t.dwLowDateTime, t.dwHighDateTime);
    let name = OsString::from_wide(name);
    Some(
        Entry::new(
            name.to_string_lossy(),
            kind,
            size,
            time(data.ftLastWriteTime),
            attributes,
        )
        .with_created(time(data.ftCreationTime)),
    )
}

/// Lists `dir` for a folder size walk (see [`crate::fs::Fs::scan_sizes`]).
pub(crate) fn scan_sizes(dir: &Path, each: &mut dyn FnMut(SizeItem)) -> io::Result<()> {
    let pattern = wide(&dir.join("*"));
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated; `data` is a WIN32_FIND_DATAW as FindExInfoBasic wants.
    let find = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }?;
    let find = Find(find);
    loop {
        each(size_item(&data));
        // SAFETY: valid find handle and output buffer.
        if let Err(err) = unsafe { FindNextFileW(find.0, &raw mut data) } {
            return if err.code() == ERROR_NO_MORE_FILES.to_hresult() {
                Ok(())
            } else {
                Err(err.into())
            };
        }
    }
}

fn size_item(data: &WIN32_FIND_DATAW) -> SizeItem {
    let attributes = data.dwFileAttributes;
    // dwReserved0 holds the reparse tag when the entry is a reparse point.
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 && data.dwReserved0 & NAME_SURROGATE != 0 {
        return SizeItem::Link;
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return SizeItem::File(
            (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        );
    }
    let len = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.cFileName.len());
    let name = &data.cFileName[..len];
    if name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')] {
        return SizeItem::Skip;
    }
    SizeItem::Dir {
        name: OsString::from_wide(name),
        modified: filetime(
            data.ftLastWriteTime.dwLowDateTime,
            data.ftLastWriteTime.dwHighDateTime,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_change_records() {
        let mut raw = Vec::new();
        let record = |next: u32, name: &str, raw: &mut Vec<u8>| {
            let units: Vec<u16> = name.encode_utf16().collect();
            raw.extend(next.to_le_bytes());
            raw.extend(1u32.to_le_bytes());
            raw.extend((units.len() as u32 * 2).to_le_bytes());
            raw.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        };
        record(24, "ab.txt", &mut raw); // 12 + 12 bytes
        record(0, "新しい", &mut raw);
        let paths = changed_paths(&raw, Path::new(r"C:\w"));
        assert_eq!(
            paths,
            [PathBuf::from(r"C:\w\ab.txt"), PathBuf::from(r"C:\w\新しい")]
        );
        // A truncated record ends the list instead of reading past the buffer.
        assert_eq!(changed_paths(&raw[..30], Path::new(r"C:\w")).len(), 1);
    }

    /// The large-fetch listing gives exactly what std's listing does: names, kinds (a junction is
    /// a link to a folder), sizes, times and attributes; an empty folder is empty and a missing one
    /// is an error.
    #[test]
    fn listing_matches_std() {
        let dir = std::env::temp_dir().join(format!("arke-winfs-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("\u{5831}\u{544a}.md"), vec![0u8; 70_000]).unwrap();
        let linked = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(dir.join("link"))
            .arg(dir.join("sub"))
            .output()
            .unwrap()
            .status
            .success();
        assert!(linked, "failed to create junction");

        let key = |e: &crate::entry::Entry| {
            (
                e.name.to_string(),
                e.kind,
                e.size,
                e.modified,
                e.created,
                e.attributes,
            )
        };
        let mut ours: Vec<_> = read_dir(&dir)
            .unwrap()
            .unwrap()
            .map(|e| key(&e.unwrap()))
            .collect();
        let mut std_list: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| key(&crate::entry::Entry::from_dir_entry(&e.unwrap()).unwrap()))
            .collect();
        ours.sort();
        std_list.sort();
        assert_eq!(ours.len(), 5);
        assert_eq!(ours, std_list);
        assert!(
            ours.iter()
                .any(|e| e.0 == "link" && e.1 == crate::entry::EntryKind::Link { is_dir: true })
        );
        assert_eq!(read_dir(&dir.join("empty")).unwrap().unwrap().count(), 0);
        assert!(read_dir(&dir.join("missing")).unwrap().is_err());
        let _ = std::fs::remove_dir(dir.join("link"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path with a NUL in it is refused, as std refuses it, instead of listing what comes
    /// before the NUL.
    #[test]
    fn a_path_with_a_nul_is_refused() {
        let dir = std::env::temp_dir();
        let mut bad = dir.into_os_string();
        bad.push("\0x");
        let err = read_dir(Path::new(&bad)).unwrap().err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Errors that std would hit too are returned; a redirector rejecting the large-fetch flags
    /// sends that volume's listings through std from then on.
    #[test]
    fn unsupported_large_fetches_fall_back_to_std() {
        assert_eq!(find_failure(2), FindFailure::Empty);
        for code in [3, 5, 53, 67, 1231] {
            assert_eq!(find_failure(code), FindFailure::Error, "{code}");
        }
        for code in [1, 50, 87, 124] {
            assert_eq!(find_failure(code), FindFailure::Unsupported, "{code}");
        }
        assert_eq!(find_failure(1117), FindFailure::Other);
        let err = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(87));
        assert_eq!(win32_code(&err), 87);

        let dir = std::env::temp_dir();
        assert!(read_dir(&dir).is_some());
        let volume = volume_of(&dir);
        no_large_fetch().lock().insert(volume.clone());
        assert!(read_dir(&dir).is_none(), "listed like std");
        assert!(crate::fs::Fs::read_dir(&crate::fs::RealFs, &dir).is_ok());
        no_large_fetch().lock().remove(&volume);
        assert_eq!(volume_of(Path::new(r"\\NAS\Share\docs")), r"\\nas\share");
        assert_eq!(volume_of(Path::new(r"X:\docs")), "x:");
    }

    #[test]
    fn network_paths_are_told_by_the_path_or_the_drive_type() {
        assert!(is_network(Path::new(r"\\nas\share\docs")));
        assert!(is_network(Path::new(r"\\?\UNC\nas\share")));
        assert!(!is_network(Path::new(r"\\?\C:\Windows")));
        assert!(!is_network(Path::new(r"\\.\pipe\x")));
        assert!(!is_network(Path::new("relative")));
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        assert!(!is_network(&Path::new(&system).join("Windows")));
    }

    #[test]
    fn file_times_match_std() {
        let dir = std::env::temp_dir();
        let listed = std::fs::metadata(&dir).unwrap().modified().unwrap();
        let since = listed.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let intervals = since.as_secs() * 10_000_000
            + u64::from(since.subsec_nanos()) / 100
            + 116_444_736_000_000_000;
        assert_eq!(
            filetime(intervals as u32, (intervals >> 32) as u32),
            Some(listed)
        );
        assert!(filetime(0, 0).is_some(), "1601 is before 1970 but valid");
    }
}
