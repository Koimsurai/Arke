//! File system abstraction. All directory I/O goes through `Fs`, so tests can swap in
//! `FakeFs` with delays or permanent hangs.
//!
//! Also a UI thread guard: the UI thread calls [`mark_ui_thread`] at startup, after which
//! any `Fs` call on that thread panics (always in `FakeFs`, in debug builds for `RealFs`).
//! That turns "the UI thread never waits on I/O" into something tests can catch.

use rust_i18n::t;
use std::cell::Cell;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::entry::{Entry, EntryKind};

/// Directory listing stream; each item is one entry or a per-entry error.
pub type DirIter = Box<dyn Iterator<Item = io::Result<Entry>> + Send>;

/// File with random access (archive previews read only the central directory).
pub trait ReadSeek: io::Read + io::Seek + Send {}
impl<T: io::Read + io::Seek + Send> ReadSeek for T {}

/// Largest file the default [`Fs::open`] reads fully into memory.
const OPEN_IN_MEMORY_MAX: u64 = 64 * 1024 * 1024;
/// Watch event handler; called on the watcher's background thread.
pub type WatchCallback = Box<dyn Fn(WatchEvent) + Send + Sync>;

/// Change notification from [`Fs::watch`] / [`Fs::watch_tree`].
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// These paths may have changed (created, deleted, modified, renamed).
    Changed(Vec<PathBuf>),
    /// Events were lost (buffer overflow etc.); relist the directory.
    Rescan,
}

/// One entry seen by a folder size walk ([`Fs::scan_sizes`]).
#[derive(Debug)]
pub enum SizeItem {
    /// A file and its size.
    File(u64),
    /// A subfolder, to walk next, with its modified time as listed here.
    Dir {
        /// Folder name.
        name: OsString,
        /// Last modified, as [`Entry::modified`] has it.
        modified: Option<SystemTime>,
    },
    /// A link or junction: not followed, so nothing is counted twice and there are no cycles.
    Link,
    /// An entry that could not be read; the total is an undercount.
    Unreadable,
    /// Nothing to count (`.` and `..`).
    Skip,
}

/// File system operations used by the directory service, search and previews.
pub trait Fs: Send + Sync + 'static {
    /// Starts listing. The iterator is consumed on a background thread.
    fn read_dir(&self, path: &Path) -> io::Result<DirIter>;
    /// Stats a single entry without following symlinks.
    fn stat(&self, path: &Path) -> io::Result<Entry>;
    /// The name `path`'s item has in its folder, when it differs from the last part of `path`: a
    /// change notification can name an item in another case than it now has (renamed to only
    /// change the case) or by its short 8.3 name, and the item is found either way (#310).
    /// `None` when it is the same, or unknown.
    fn listed_name(&self, path: &Path) -> Option<String> {
        let _ = path;
        None
    }
    /// Watches a directory (non-recursive). Dropping the returned value stops watching.
    fn watch(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>>;
    /// Reads a whole file (for thumbnails). Errors without reading if larger than `max_len`.
    fn read(&self, path: &Path, max_len: u64) -> io::Result<Vec<u8>>;
    /// Reads up to `len` bytes from the start of a file (for text previews).
    fn read_head(&self, path: &Path, len: u64) -> io::Result<Vec<u8>>;
    /// Opens for random access. The default reads the whole file into memory (up to 64 MB);
    /// the real file system opens the file directly.
    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        let bytes = self.read(path, OPEN_IN_MEMORY_MAX)?;
        Ok(Box::new(io::Cursor::new(bytes)))
    }
    /// Where the symlink or junction at `path` points, as an absolute path (see
    /// [`crate::link::link_target`]). Never follows the link any further.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("read_link({})", path.display()),
        ))
    }
    /// Lists `dir` for a folder size walk: calls `each` once per entry. Same entries as
    /// [`Fs::read_dir`]; the real file system skips building an [`Entry`] for each file.
    fn scan_sizes(&self, dir: &Path, each: &mut dyn FnMut(SizeItem)) -> io::Result<()> {
        for entry in self.read_dir(dir)? {
            each(match entry {
                Ok(e) => match e.kind {
                    EntryKind::File => SizeItem::File(e.size),
                    EntryKind::Dir => SizeItem::Dir {
                        name: OsString::from(&*e.name),
                        modified: e.modified,
                    },
                    EntryKind::Link { .. } => SizeItem::Link,
                },
                Err(_) => SizeItem::Unreadable,
            });
        }
        Ok(())
    }
    /// Watches a whole tree (recursive, used by the global search index). The default only
    /// watches one level, like [`Fs::watch`].
    fn watch_tree(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>> {
        self.watch(path, on_event)
    }
}

thread_local! {
    static IS_UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as the UI thread.
pub fn mark_ui_thread() {
    IS_UI_THREAD.with(|c| c.set(true));
}

/// Whether the current thread was marked with [`mark_ui_thread`].
pub fn is_ui_thread() -> bool {
    IS_UI_THREAD.with(Cell::get)
}

/// Whether `path` is on a file server (`\\server\share\…` or a mapped drive), where every listing
/// waits on the server: walks there list several folders at once. No network access.
pub fn is_network_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        crate::winfs::is_network(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// Panics (debug builds) on file system calls from the UI thread.
#[track_caller]
pub(crate) fn debug_assert_not_ui_thread(op: &str, path: &Path) {
    debug_assert!(
        !is_ui_thread(),
        "file system call on the UI thread: {op}({})",
        path.display()
    );
}

/// Real file system: std::fs + notify.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl Fs for RealFs {
    fn read_dir(&self, path: &Path) -> io::Result<DirIter> {
        debug_assert_not_ui_thread("read_dir", path);
        // Large fetches: far fewer round trips to a file server (see `winfs::read_dir`).
        #[cfg(windows)]
        if let Some(listed) = crate::winfs::read_dir(path) {
            return listed;
        }
        // On Windows std::fs::read_dir uses FindFirstFileExW and DirEntry::metadata() reuses
        // the find data, so there is no extra syscall.
        let rd = std::fs::read_dir(path)?;
        Ok(Box::new(rd.filter_map(|item| match item {
            Ok(de) => Entry::from_dir_entry(&de).map(Ok),
            Err(err) => Some(Err(err)),
        })))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        debug_assert_not_ui_thread("read_link", path);
        crate::link::link_target(path)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        debug_assert_not_ui_thread("stat", path);
        let meta = std::fs::symlink_metadata(path)?;
        let name = path
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        Ok(Entry::from_metadata(&name, &meta))
    }

    #[cfg(windows)]
    fn listed_name(&self, path: &Path) -> Option<String> {
        debug_assert_not_ui_thread("listed_name", path);
        let listed = crate::winfs::listed_name(path)?;
        (path.file_name() != Some(listed.as_os_str()))
            .then(|| listed.to_string_lossy().into_owned())
    }

    fn read(&self, path: &Path, max_len: u64) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        debug_assert_not_ui_thread("read", path);
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() > max_len {
            return Err(io::Error::other(t!("io.too_large").into_owned()));
        }
        let mut bytes = Vec::new();
        file.take(max_len).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn read_head(&self, path: &Path, len: u64) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        debug_assert_not_ui_thread("read_head", path);
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(len)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        debug_assert_not_ui_thread("open", path);
        Ok(Box::new(io::BufReader::new(std::fs::File::open(path)?)))
    }

    fn watch(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>> {
        debug_assert_not_ui_thread("watch", path);
        watch_with(path, notify::RecursiveMode::NonRecursive, on_event)
    }

    #[cfg(windows)]
    fn scan_sizes(&self, dir: &Path, each: &mut dyn FnMut(SizeItem)) -> io::Result<()> {
        debug_assert_not_ui_thread("scan_sizes", dir);
        crate::winfs::scan_sizes(dir, each)
    }

    fn watch_tree(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>> {
        debug_assert_not_ui_thread("watch_tree", path);
        watch_with(path, notify::RecursiveMode::Recursive, on_event)
    }
}

/// Watches through the volume registry ([`crate::watches`]), so the watcher can be closed when
/// its drive is ejected or locked and reopened if that is cancelled.
fn watch_with(
    path: &Path,
    mode: notify::RecursiveMode,
    on_event: WatchCallback,
) -> io::Result<Box<dyn Send>> {
    let on_event: Arc<WatchCallback> = Arc::new(on_event);
    let target = path.to_path_buf();
    let events = Arc::clone(&on_event);
    let guard = crate::watches::registry().watch(
        path,
        Box::new(move || arm_watcher(&target, mode, Arc::clone(&events))),
        Box::new(move || on_event(WatchEvent::Rescan)),
    )?;
    Ok(Box::new(guard))
}

fn forward(on_event: &WatchCallback, res: notify::Result<notify::Event>) {
    match res {
        Ok(event) if event.need_rescan() => on_event(WatchEvent::Rescan),
        Ok(event) => on_event(WatchEvent::Changed(event.paths)),
        Err(_) => on_event(WatchEvent::Rescan),
    }
}

/// `ReadDirectoryChangesW` watcher that can report when its directory handle is closed.
#[cfg(windows)]
struct DirWatch {
    watcher: Option<notify::ReadDirectoryChangesWatcher>,
    /// The watcher's thread reports here once the handle is closed.
    closed: std::sync::mpsc::Receiver<notify::windows::MetaEvent>,
}

#[cfg(windows)]
impl crate::watches::Armed for DirWatch {
    fn close(mut self: Box<Self>) {
        const WAIT: std::time::Duration = std::time::Duration::from_secs(2);
        drop(self.watcher.take());
        let deadline = std::time::Instant::now() + WAIT;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match self.closed.recv_timeout(left) {
                Ok(notify::windows::MetaEvent::SingleWatchComplete) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}

#[cfg(windows)]
impl crate::watches::Armed for crate::winfs::DirWatcher {
    fn close(self: Box<Self>) {
        (*self).close();
    }
}

/// Folders shown in a tab use Arke's own watcher ([`crate::winfs::DirWatcher`]: no polling, and
/// a full change buffer asks for a relist). Whole trees for the search index stay on notify.
#[cfg(windows)]
fn arm_watcher(
    path: &Path,
    mode: notify::RecursiveMode,
    on_event: Arc<WatchCallback>,
) -> io::Result<Box<dyn crate::watches::Armed>> {
    use notify::Watcher;
    if mode == notify::RecursiveMode::NonRecursive {
        return Ok(Box::new(crate::winfs::DirWatcher::start(
            path, false, on_event,
        )?));
    }
    let (closed_tx, closed) = std::sync::mpsc::channel();
    #[allow(
        clippy::disallowed_types,
        reason = "notify's own API takes a std mutex here"
    )]
    let handler: Arc<std::sync::Mutex<dyn notify::EventHandler>> = Arc::new(std::sync::Mutex::new(
        move |res: notify::Result<notify::Event>| forward(&on_event, res),
    ));
    let mut watcher = notify::ReadDirectoryChangesWatcher::create(handler, closed_tx)
        .map_err(io::Error::other)?;
    // Can block on network paths, but this only runs on background threads.
    watcher.watch(path, mode).map_err(io::Error::other)?;
    Ok(Box::new(DirWatch {
        watcher: Some(watcher),
        closed,
    }))
}

#[cfg(not(windows))]
struct DirWatch(#[allow(dead_code)] notify::RecommendedWatcher);

#[cfg(not(windows))]
impl crate::watches::Armed for DirWatch {
    fn close(self: Box<Self>) {}
}

#[cfg(not(windows))]
fn arm_watcher(
    path: &Path,
    mode: notify::RecursiveMode,
    on_event: Arc<WatchCallback>,
) -> io::Result<Box<dyn crate::watches::Armed>> {
    use notify::Watcher;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        forward(&on_event, res);
    })
    .map_err(io::Error::other)?;
    watcher.watch(path, mode).map_err(io::Error::other)?;
    Ok(Box::new(DirWatch(watcher)))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Thousands of new items at once (an extracted archive) while the consumer is busy overflow
    /// the change buffer. That must ask for a relist, and watching must go on afterwards.
    /// Folders rather than files: they make the same change records, and an antivirus doesn't
    /// scan them, so creating thousands takes seconds instead of minutes.
    #[test]
    fn overflowing_watch_asks_for_a_rescan_and_keeps_watching() {
        const FILES: usize = 3000;
        let dir = std::env::temp_dir().join(format!("arke-watch-overflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let seen: Arc<Mutex<HashSet<String>>> = Arc::default();
        let rescans = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Instant::now()));
        let (seen2, rescans2, last2) = (Arc::clone(&seen), Arc::clone(&rescans), Arc::clone(&last));
        // The consumer is busy (as the UI or an antivirus scan can be) until everything exists.
        let created = Arc::new(AtomicBool::new(false));
        let busy = Arc::clone(&created);
        let watch = RealFs
            .watch(
                &dir,
                Box::new(move |event| {
                    while !busy.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    match event {
                        WatchEvent::Changed(paths) => seen2.lock().extend(
                            paths
                                .iter()
                                .filter_map(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned()),
                        ),
                        WatchEvent::Rescan => {
                            rescans2.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    *last2.lock() = Instant::now();
                }),
            )
            .unwrap();
        std::fs::write(dir.join("first.txt"), b"x").unwrap();
        for i in 0..FILES {
            std::fs::create_dir(dir.join(format!("extracted-{i:05}"))).unwrap();
        }
        created.store(true, Ordering::Relaxed);
        *last.lock() = Instant::now();
        // Until no event came for a while.
        let start = Instant::now();
        while last.lock().elapsed() < Duration::from_millis(800)
            && start.elapsed() < Duration::from_secs(20)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let got = seen
            .lock()
            .iter()
            .filter(|n| n.starts_with("extracted-"))
            .count();
        let rescanned = rescans.load(Ordering::Relaxed) > 0;
        assert!(
            rescanned || got == FILES,
            "changes were lost without asking for a rescan: saw {got} of {FILES}"
        );

        // Still watching.
        std::fs::write(dir.join("after.txt"), b"y").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !seen.lock().contains("after.txt") {
            assert!(
                Instant::now() < deadline,
                "the watch stopped after the overflow"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(watch);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
