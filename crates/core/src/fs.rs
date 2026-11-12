//! File system abstraction. All directory I/O goes through `Fs`, so tests can swap in
//! `FakeFs` with delays or permanent hangs.
//!
//! Also a UI thread guard: the UI thread calls [`mark_ui_thread`] at startup, after which
//! any `Fs` call on that thread panics (always in `FakeFs`, in debug builds for `RealFs`).
//! That turns "the UI thread never waits on I/O" into something tests can catch.

use rust_i18n::t;
use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::entry::Entry;

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

/// File system operations used by the directory service, search and previews.
pub trait Fs: Send + Sync + 'static {
    /// Starts listing. The iterator is consumed on a background thread.
    fn read_dir(&self, path: &Path) -> io::Result<DirIter>;
    /// Stats a single entry without following symlinks.
    fn stat(&self, path: &Path) -> io::Result<Entry>;
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
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Entry::from_metadata(&name, &meta))
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
    let events = on_event.clone();
    let guard = crate::watches::registry().watch(
        path,
        Box::new(move || arm_watcher(&target, mode, events.clone())),
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
fn arm_watcher(
    path: &Path,
    mode: notify::RecursiveMode,
    on_event: Arc<WatchCallback>,
) -> io::Result<Box<dyn crate::watches::Armed>> {
    use notify::Watcher;
    let (closed_tx, closed) = std::sync::mpsc::channel();
    // notify's own API takes a std mutex here.
    #[allow(clippy::disallowed_types)]
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
