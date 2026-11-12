//! In-memory file system for tests. It can:
//! - delay a directory (slow network drive)
//! - hang a directory until `release` (disconnected SMB share)
//! - fire watch events by hand
//!
//! Any call on the UI thread (see [`crate::fs::mark_ui_thread`]) panics.

use parking_lot::{Condvar, Mutex};
use rust_i18n::t;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use crate::entry::{Entry, EntryKind};
use crate::fs::{DirIter, Fs, WatchCallback, WatchEvent, is_ui_thread};

#[derive(Default)]
struct State {
    dirs: HashMap<PathBuf, Vec<Entry>>,
    delays: HashMap<PathBuf, Duration>,
    hung: HashSet<PathBuf>,
    contents: HashMap<PathBuf, Vec<u8>>,
    links: HashMap<PathBuf, PathBuf>,
    watchers: Vec<(PathBuf, Weak<WatchCallback>)>,
}

/// In-memory [`Fs`] with configurable delays, hangs and manual watch events.
#[derive(Default)]
pub struct FakeFs {
    state: Mutex<State>,
    released: Condvar,
    read_dir_calls: AtomicUsize,
}

impl FakeFs {
    /// Creates an empty file system (no directories).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[track_caller]
    fn guard(op: &str, path: &Path) {
        assert!(
            !is_ui_thread(),
            "file system call on the UI thread: {op}({})",
            path.display()
        );
    }

    /// Creates a directory (and all parents) with the given entries.
    pub fn insert_dir(&self, path: impl Into<PathBuf>, entries: Vec<Entry>) {
        let path = path.into();
        let mut state = self.state.lock();
        let mut cur = path.clone();
        while let Some(parent) = cur.parent().map(Path::to_path_buf) {
            let name = cur.file_name().map(|n| n.to_string_lossy().into_owned());
            let siblings = state.dirs.entry(parent.clone()).or_default();
            if let Some(name) = name
                && !siblings.iter().any(|e| *e.name == *name)
            {
                siblings.push(Entry::new(name, EntryKind::Dir, 0, None, 0));
            }
            cur = parent;
        }
        state.dirs.entry(path).or_default().extend(entries);
    }

    /// Makes `link` read back as a link to `target` ([`Fs::read_link`]). The listing entry itself
    /// comes from [`Self::insert_dir`] (with [`EntryKind::Link`]).
    pub fn insert_link(&self, link: impl Into<PathBuf>, target: impl Into<PathBuf>) {
        self.state.lock().links.insert(link.into(), target.into());
    }

    /// Creates a directory with `count` files (file_000000.txt ...).
    pub fn insert_many(&self, path: impl Into<PathBuf>, count: usize) {
        let entries = (0..count)
            .map(|i| {
                Entry::new(
                    format!("file_{i:06}.txt"),
                    EntryKind::File,
                    i as u64,
                    Some(SystemTime::UNIX_EPOCH),
                    0,
                )
            })
            .collect();
        self.insert_dir(path, entries);
    }

    /// Adds or replaces a file entry of `size` bytes in `dir`; no watch event.
    pub fn add_file(&self, dir: &Path, name: &str, size: u64) {
        self.add_entry(dir, Entry::new(name, EntryKind::File, size, None, 0));
    }

    /// Adds or replaces an entry. Emits no watch event; call `emit_changed` if needed.
    // False positive: `list` borrows from the guard until the last line.
    #[allow(clippy::significant_drop_tightening)]
    pub fn add_entry(&self, dir: &Path, entry: Entry) {
        let mut state = self.state.lock();
        let list = state.dirs.entry(dir.to_path_buf()).or_default();
        list.retain(|e| e.name != entry.name);
        list.push(entry);
    }

    /// Removes the entry `name` from `dir`; no watch event.
    pub fn remove(&self, dir: &Path, name: &str) {
        let mut state = self.state.lock();
        if let Some(list) = state.dirs.get_mut(dir) {
            list.retain(|e| &*e.name != name);
        }
    }

    /// Sets file content for `read`. `hang` works on file paths too.
    pub fn set_content(&self, path: impl Into<PathBuf>, bytes: Vec<u8>) {
        self.state.lock().contents.insert(path.into(), bytes);
    }

    /// Makes every read of `path` sleep for `delay` first.
    pub fn set_delay(&self, path: impl Into<PathBuf>, delay: Duration) {
        self.state.lock().delays.insert(path.into(), delay);
    }

    /// Makes `read_dir` on `path` block until [`Self::release`].
    pub fn hang(&self, path: impl Into<PathBuf>) {
        self.state.lock().hung.insert(path.into());
    }

    /// Unblocks reads hung on `path` by [`Self::hang`].
    pub fn release(&self, path: &Path) {
        self.state.lock().hung.remove(path);
        self.released.notify_all();
    }

    /// Unblocks every hung path.
    pub fn release_all(&self) {
        self.state.lock().hung.clear();
        self.released.notify_all();
    }

    /// Number of `read_dir` calls so far, including ones still blocked.
    pub fn read_dir_calls(&self) -> usize {
        self.read_dir_calls.load(Ordering::SeqCst)
    }

    /// Sends a change event to watchers of `dir`.
    pub fn emit_changed(&self, dir: &Path, names: &[&str]) {
        let paths: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
        self.emit(dir, &WatchEvent::Changed(paths));
    }

    /// Delivers `event` to every live watcher registered on exactly `dir`.
    pub fn emit(&self, dir: &Path, event: &WatchEvent) {
        let callbacks: Vec<Arc<WatchCallback>> = {
            let mut state = self.state.lock();
            state.watchers.retain(|(_, w)| w.strong_count() > 0);
            state
                .watchers
                .iter()
                .filter(|(p, _)| p == dir)
                .filter_map(|(_, w)| w.upgrade())
                .collect()
        };
        for cb in callbacks {
            cb(event.clone());
        }
    }

    /// Number of live (not yet dropped) watchers on `dir`.
    pub fn watcher_count(&self, dir: &Path) -> usize {
        self.state
            .lock()
            .watchers
            .iter()
            .filter(|(p, w)| p == dir && w.strong_count() > 0)
            .count()
    }
}

impl FakeFs {
    /// Blocks or delays as configured by `hang` / `set_delay`.
    fn block_on(&self, path: &Path) {
        let delay = {
            let mut state = self.state.lock();
            while state.hung.contains(path) {
                self.released.wait(&mut state);
            }
            state.delays.get(path).copied()
        };
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
    }
}

impl Fs for FakeFs {
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        Self::guard("read_link", path);
        self.block_on(path);
        self.state
            .lock()
            .links
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn read(&self, path: &Path, max_len: u64) -> io::Result<Vec<u8>> {
        Self::guard("read", path);
        self.block_on(path);
        let bytes = self
            .state
            .lock()
            .contents
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if bytes.len() as u64 > max_len {
            return Err(io::Error::other(t!("io.too_large").into_owned()));
        }
        Ok(bytes)
    }

    fn read_head(&self, path: &Path, len: u64) -> io::Result<Vec<u8>> {
        Self::guard("read_head", path);
        self.block_on(path);
        let mut bytes = self
            .state
            .lock()
            .contents
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        bytes.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        Ok(bytes)
    }

    fn read_dir(&self, path: &Path) -> io::Result<DirIter> {
        Self::guard("read_dir", path);
        self.read_dir_calls.fetch_add(1, Ordering::SeqCst);
        self.block_on(path);
        let listed = self.state.lock().dirs.get(path).cloned();
        if let Some(entries) = listed {
            return Ok(Box::new(entries.into_iter().map(Ok)));
        }
        // Listing a file fails like on Windows (ERROR_DIRECTORY).
        let is_file = self.stat(path).is_ok_and(|e| !e.is_dir_like());
        Err(io::Error::from(if is_file {
            io::ErrorKind::NotADirectory
        } else {
            io::ErrorKind::NotFound
        }))
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        Self::guard("stat", path);
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::ErrorKind::NotFound.into());
        };
        let name = name.to_string_lossy();
        self.state
            .lock()
            .dirs
            .get(dir)
            .and_then(|list| list.iter().find(|e| *e.name == *name).cloned())
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    fn watch(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>> {
        Self::guard("watch", path);
        let cb = Arc::new(on_event);
        self.state
            .lock()
            .watchers
            .push((path.to_path_buf(), Arc::downgrade(&cb)));
        Ok(Box::new(cb))
    }
}
