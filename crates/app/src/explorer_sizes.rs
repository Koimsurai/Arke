//! Folder sizes (the Size column and selection totals).
//!
//! - Computed one by one on a background thread at lowest CPU and I/O priority, only after the
//!   listing completes and only on local disks (network locations are skipped, like thumbnails).
//!   Canceled when the folder changes. At most [`WALKS_AT_ONCE`] walks run at a time across all
//!   windows and tabs.
//! - Only subfolders that are needed are computed: the ones drawn on screen (scrolling computes
//!   more), the selected ones (for the selection total), or all of them when sorting by size. A
//!   home folder whose big subfolders are further down no longer walks them all on opening.
//! - Results go in an app-wide cache ([`SharedSizes`]). A folder computed in the last
//!   [`REUSE_FOR`] whose modified time is unchanged is not walked again: going back to a folder,
//!   or opening the same one in another tab, shows its sizes right away. Older values are shown
//!   while recomputing. F5 recomputes everything.
//! - A walk also records the totals of the folders up to [`SEED_DEPTH`] levels below each
//!   subfolder, so opening one of those finds its subfolders already computed (same amount of
//!   walking).
//! - A change seen in a folder (a tab's watcher) marks the cached sizes of that folder and every
//!   folder above it out of date, so going up recomputes them (showing the old values meanwhile). Changes deeper down that no tab watches show
//!   after [`REUSE_FOR`] or F5.
//! - New subfolders, or ones whose mtime changed (direct children added or removed), are recomputed
//!   without F5. This only runs when the listing version changes, not on every render.
//! - Volumes the search index follows live through the change journal (background service) are
//!   answered from the index in one pass instead of a walk (`IndexService::child_dir_sizes`);
//!   folders the index skips are still walked, so totals are the same. Others are walked.
//! - Can be turned off with the "Calculate folder sizes" setting.
//! - Sorting by size uses these values for folders (`Explorer::ensure_sorted`); any change bumps
//!   `rev` so the list re-sorts.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use arke_core::EntryKind;
use gpui::{App, Context, Global};
use parking_lot::{Condvar, Mutex};

use super::{AppEvent, Explorer, spawn_thread};

/// How long a computed size is used as is when its folder's modified time hasn't changed.
pub(crate) const REUSE_FOR: Duration = Duration::from_mins(10);
/// Levels below each subfolder whose totals a walk also keeps.
const SEED_DEPTH: usize = 2;
/// Folder walks running at once, app-wide (each is one low-priority thread).
const WALKS_AT_ONCE: usize = 2;
/// Cached sizes kept; beyond this the expired ones go, then all.
const CACHE_MAX: usize = 100_000;

/// A computed folder size.
#[derive(Clone, Copy, Debug)]
struct Known {
    bytes: u64,
    /// The folder's modified time when computed, as its parent lists it.
    modified: Option<SystemTime>,
    at: Instant,
    /// Something inside changed since: walk again, but show this meanwhile.
    outdated: bool,
}

impl Known {
    fn reusable(&self, modified: Option<SystemTime>) -> bool {
        !self.outdated && self.modified == modified && self.at.elapsed() < REUSE_FOR
    }
}

/// Folder sizes shared by all windows, tabs and their background walks.
#[derive(Default)]
pub(crate) struct SharedSizes {
    known: Mutex<HashMap<PathBuf, Known>>,
    /// Walks running right now.
    walking: Mutex<usize>,
    walk_done: Condvar,
}

/// One of the [`WALKS_AT_ONCE`] walk slots; frees it when dropped.
struct WalkSlot<'a>(&'a SharedSizes);

impl Drop for WalkSlot<'_> {
    fn drop(&mut self) {
        *self.0.walking.lock() -= 1;
        self.0.walk_done.notify_one();
    }
}

impl SharedSizes {
    fn get(&self, path: &Path) -> Option<Known> {
        self.known.lock().get(path).copied()
    }

    /// The size of `path` if it can be used without walking again.
    fn reusable(&self, path: &Path, modified: Option<SystemTime>) -> Option<u64> {
        self.get(path)
            .filter(|k| k.reusable(modified))
            .map(|k| k.bytes)
    }

    fn put(&self, path: PathBuf, bytes: u64, modified: Option<SystemTime>) {
        let mut known = self.known.lock();
        if known.len() >= CACHE_MAX && !known.contains_key(&path) {
            known.retain(|_, k| k.at.elapsed() < REUSE_FOR);
            if known.len() >= CACHE_MAX {
                known.clear();
            }
        }
        known.insert(
            path,
            Known {
                bytes,
                modified,
                at: Instant::now(),
                outdated: false,
            },
        );
    }

    /// Something in `dir` changed: its size and those of the folders above it are out of date.
    /// They stay as the last known values (shown, and sorted by) until walked again, so a folder
    /// keeps its place in a list sorted by size instead of dropping to zero meanwhile (#161).
    fn outdate_with_ancestors(&self, dir: &Path) {
        let mut known = self.known.lock();
        for path in dir.ancestors() {
            if let Some(k) = known.get_mut(path) {
                k.outdated = true;
            }
        }
    }

    /// Waits for a walk slot; `None` if cancelled meanwhile.
    fn walk_slot(&self, cancel: &AtomicBool) -> Option<WalkSlot<'_>> {
        let mut walking = self.walking.lock();
        while *walking >= WALKS_AT_ONCE {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            self.walk_done
                .wait_for(&mut walking, Duration::from_millis(250));
        }
        *walking += 1;
        drop(walking);
        Some(WalkSlot(self))
    }
}

#[derive(Default)]
struct SizeCache(Arc<SharedSizes>);

impl Global for SizeCache {}

fn shared(cx: &mut App) -> Arc<SharedSizes> {
    Arc::clone(&cx.default_global::<SizeCache>().0)
}

/// Sizes of subfolders in this tab's current folder.
#[derive(Default)]
pub(crate) struct FolderSizes {
    /// Folder that `sizes` belongs to (computing or done)
    dir: Option<PathBuf>,
    /// Shared as `Arc` so background sorting doesn't copy it
    sizes: Arc<HashMap<Arc<str>, u64>>,
    /// Handed to the worker, not computed yet
    pending: HashSet<Arc<str>>,
    /// Not handed to the worker yet: off screen, not selected, not sorting by size
    waiting: HashMap<Arc<str>, Option<SystemTime>>,
    /// The worker's queue for `dir`
    queue: Option<Arc<SizeQueue>>,
    /// Bumped whenever `sizes` changes
    rev: u64,
    /// F5: recompute everything after the listing completes (old values stay visible)
    stale: bool,
    /// Each subfolder's mtime when computed (recompute if it changes)
    seen: HashMap<Arc<str>, Option<SystemTime>>,
    /// Listing version at the last recompute check
    checked_version: u64,
}

/// Subfolders for one tab's worker thread to compute, in order. The thread runs while there is
/// work and quits when the queue is empty; adding more starts it again.
struct SizeQueue {
    dir: PathBuf,
    /// Take sizes another tab computed meanwhile (not after F5)
    reuse: bool,
    cancel: AtomicBool,
    /// The queue, and whether a thread is working on it.
    todo: Mutex<(Todo, bool)>,
}

type Todo = VecDeque<(Arc<str>, Option<SystemTime>)>;

/// A tab's size worker. If it stops without finishing its queue, the list must not wait for
/// those folders forever before sorting by size: a folder it panicked on is reported unknown
/// and the rest are left for a new worker ([`Explorer::handle_folder_size`] starts one); if its
/// thread couldn't start at all, they are all reported unknown.
struct SizeWorker {
    queue: Arc<SizeQueue>,
    tx: flume::Sender<AppEvent>,
    /// The thread started.
    started: bool,
    /// The folder being computed.
    current: Option<Arc<str>>,
    /// Stopped as it should: the queue ran out or the folder was left.
    finished: bool,
}

impl SizeWorker {
    fn send(&self, name: Arc<str>, bytes: Option<u64>) {
        let _ = self.tx.send(AppEvent::FolderSize {
            dir: self.queue.dir.clone(),
            name,
            bytes,
        });
    }

    const fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for SizeWorker {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let lost: Vec<Arc<str>> = {
            let mut todo = self.queue.todo.lock();
            // No thread works on the queue anymore: the next request starts one.
            todo.1 = false;
            match self.current.take() {
                Some(name) if self.started => vec![name],
                current => current
                    .into_iter()
                    .chain(todo.0.drain(..).map(|(name, _)| name))
                    .collect(),
            }
        };
        log::warn!(
            "folder sizes stopped in {}: {} left unknown",
            self.queue.dir.display(),
            lost.len()
        );
        for name in lost {
            self.send(name, None);
        }
    }
}

impl FolderSizes {
    fn stop(&mut self) {
        if let Some(queue) = self.queue.take() {
            queue.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Recompute after the listing reloads (F5).
    pub(crate) fn invalidate(&mut self) {
        self.stop();
        self.stale = true;
    }

    /// Drop everything (folder changed or setting turned off).
    fn clear(&mut self) {
        self.stop();
        self.dir = None;
        self.stale = false;
        self.pending.clear();
        self.waiting.clear();
        self.seen.clear();
        if !self.sizes.is_empty() {
            self.sizes = Arc::default();
            self.rev += 1;
        }
    }

    /// Some folders are still being computed.
    pub(crate) fn computing(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Revision of the computed sizes, if any (sorting uses it to decide whether to re-sort).
    pub(crate) fn revision(&self) -> Option<u64> {
        (!self.sizes.is_empty()).then_some(self.rev)
    }

    /// Current sizes (name -> bytes) for sorting.
    pub(crate) fn snapshot(&self) -> Arc<HashMap<Arc<str>, u64>> {
        Arc::clone(&self.sizes)
    }
}

impl Drop for FolderSizes {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the Size column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FolderSize {
    /// Not computed (setting off, network location)
    Unknown,
    /// Done; `fresh` means computed this time (or recently enough to reuse) rather than an old
    /// cached value
    Known { bytes: u64, fresh: bool },
    /// Still computing (or waiting to be shown) with no old value
    Computing,
}

fn enabled(cx: &App) -> bool {
    cx.try_global::<crate::settings::GlobalSettings>()
        .is_some_and(|g| g.0.read(cx).settings.folder_sizes)
}

/// Subfolder names with their modified times, as listed.
type Subfolders = Vec<(Arc<str>, Option<SystemTime>)>;

impl Explorer {
    /// Before each render: once the current folder is listed, look up what is known and hand
    /// what isn't to the worker (the ones on screen, selected, or all when sorting by size).
    pub(super) fn sync_folder_sizes(&mut self, cx: &mut Context<Self>) {
        let on = enabled(cx) && !self.search_mode;
        let shown = self.model.as_ref().map(|m| m.path.as_path());
        // Folder changed (or turned off): drop the previous folder's sizes, or same-named
        // subfolders would show wrong values.
        if self.folder_sizes.dir.is_some() && (!on || self.folder_sizes.dir.as_deref() != shown) {
            self.folder_sizes.clear();
            self.ensure_sorted(cx);
        }
        if !on {
            return;
        }
        let Some(model) = self.model.as_ref().filter(|m| m.complete) else {
            return;
        };
        if self.folder_sizes.dir.as_deref() == Some(model.path.as_path())
            && !self.folder_sizes.stale
        {
            self.refresh_changed_sizes();
            self.request_needed_sizes(cx);
            return;
        }
        let path = model.path.clone();
        // Skip network locations; while the local check is still running, wait (it redraws when
        // done).
        if !self.media.update(cx, |m, _| m.media_allowed(&path)) {
            return;
        }
        let dirs: Subfolders = model
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .map(|e| (Arc::clone(&e.name), e.modified))
            .collect();
        self.folder_sizes.checked_version = model.version;
        self.folder_sizes.stop();
        // F5 walks everything again; otherwise recent sizes are used as they are.
        let reuse = !std::mem::take(&mut self.folder_sizes.stale);
        let shared = shared(cx);
        let mut sizes = HashMap::new();
        let mut waiting = HashMap::new();
        for (name, modified) in &dirs {
            let known = shared.get(&path.join(&**name));
            if let Some(known) = known {
                sizes.insert(Arc::clone(name), known.bytes);
            }
            if !(reuse && known.is_some_and(|k| k.reusable(*modified))) {
                waiting.insert(Arc::clone(name), *modified);
            }
        }
        self.folder_sizes.sizes = Arc::new(sizes);
        self.folder_sizes.rev += 1;
        self.folder_sizes.pending.clear();
        self.folder_sizes.waiting = waiting;
        self.folder_sizes.seen = dirs.into_iter().collect();
        self.folder_sizes.dir = Some(path.clone());
        self.folder_sizes.queue = Some(Arc::new(SizeQueue {
            dir: path,
            reuse,
            cancel: AtomicBool::new(false),
            todo: Mutex::default(),
        }));
        // With previous values, sort by them first when sorting by size.
        self.ensure_sorted(cx);
        self.request_needed_sizes(cx);
    }

    /// Hands waiting subfolders to the worker when something needs them: sorting by size (all),
    /// or a selection total (the selected ones). Those on screen are asked for by the list itself
    /// ([`Self::folder_sizes_shown`]).
    fn request_needed_sizes(&mut self, cx: &mut App) {
        if self.folder_sizes.waiting.is_empty() {
            return;
        }
        let names: Vec<Arc<str>> = if self.sort.uses(arke_core::SortColumn::Size) {
            // In the list's order (the worker takes one at a time): a map's order was random, so
            // a slow folder could come first and hold up the rest at random.
            let in_list: Vec<Arc<str>> = self.model.as_ref().map_or_default(|m| {
                m.entries
                    .iter()
                    .filter(|e| self.folder_sizes.waiting.contains_key(&e.name))
                    .map(|e| Arc::clone(&e.name))
                    .collect()
            });
            // Anything waiting that the listing no longer has goes last.
            let listed: std::collections::HashSet<&Arc<str>> = in_list.iter().collect();
            let rest: Vec<Arc<str>> = self
                .folder_sizes
                .waiting
                .keys()
                .filter(|n| !listed.contains(n))
                .cloned()
                .collect();
            drop(listed);
            let mut names = in_list;
            names.extend(rest);
            names
        } else if self.selection.len() > super::view::SIZE_SUM_MAX {
            // No total is shown for that many.
            return;
        } else {
            self.selection
                .names()
                .filter(|n| self.folder_sizes.waiting.contains_key(*n))
                .cloned()
                .collect()
        };
        self.request_sizes(names, cx);
    }

    /// The list is drawing rows `range` of `entries`: subfolders among them that are still
    /// waiting get computed next.
    pub(super) fn folder_sizes_shown(
        &mut self,
        entries: &[arke_core::Entry],
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.folder_sizes.waiting.is_empty() {
            return;
        }
        // The list may ask for rows past the end.
        let end = range.end.min(entries.len());
        let shown: Vec<Arc<str>> = entries
            .get(range.start.min(end)..end)
            .unwrap_or_default()
            .iter()
            .filter(|e| self.folder_sizes.waiting.contains_key(&e.name))
            .map(|e| Arc::clone(&e.name))
            .collect();
        if shown.is_empty() {
            return;
        }
        // Not while drawing: the worker's results come back as events anyway.
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| this.request_sizes(shown, cx));
        });
    }

    /// Moves `names` from waiting to the worker's queue, starting the worker if it is idle.
    fn request_sizes(&mut self, names: Vec<Arc<str>>, cx: &mut App) {
        let mut todo: Subfolders = Vec::new();
        for name in names {
            if let Some(modified) = self.folder_sizes.waiting.remove(&name) {
                self.folder_sizes.pending.insert(Arc::clone(&name));
                todo.push((name, modified));
            }
        }
        if todo.is_empty() {
            return;
        }
        self.enqueue_sizes(todo, cx);
    }

    fn enqueue_sizes(&mut self, todo: Subfolders, cx: &mut App) {
        let Some(queue) = self.folder_sizes.queue.clone() else {
            return;
        };
        let start = {
            let mut guard = queue.todo.lock();
            guard.0.extend(todo);
            !std::mem::replace(&mut guard.1, true)
        };
        if start {
            self.start_size_worker(queue, cx);
        }
    }

    /// Listing changed (watcher saw adds or edits): recompute new subfolders and ones whose mtime
    /// changed.
    fn refresh_changed_sizes(&mut self) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        if model.version == self.folder_sizes.checked_version || self.folder_sizes.computing() {
            return;
        }
        self.folder_sizes.checked_version = model.version;
        let changed: Subfolders = model
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .filter(|e| self.folder_sizes.seen.get(&e.name) != Some(&e.modified))
            .map(|e| (Arc::clone(&e.name), e.modified))
            .collect();
        if changed.is_empty() {
            return;
        }
        self.folder_sizes.seen.extend(changed.iter().cloned());
        // Like the rest: computed once on screen (or needed by the sort or the selection).
        self.folder_sizes.waiting.extend(changed);
    }

    /// Computes the queued subfolders on a background thread, from the index when it covers the
    /// folder, otherwise one by one by walking. Quits when the queue is empty.
    fn start_size_worker(&self, queue: Arc<SizeQueue>, cx: &mut App) {
        let fs = Arc::clone(&self.services.fs);
        let index = crate::global_search::GlobalSearch::local_service(cx);
        let shared = shared(cx);
        // Made out here so that a thread that can't start reports its folders too.
        let mut worker = SizeWorker {
            queue,
            tx: self.events_tx.clone(),
            started: false,
            current: None,
            finished: false,
        };
        spawn_thread("folder-sizes", move || {
            worker.started = true;
            crate::win::lower_io_priority();
            let queue = Arc::clone(&worker.queue);
            let path = &queue.dir;
            let cancel = &queue.cancel;
            // The index answers the whole folder in one pass (subfolders it lacks are walked).
            let indexed: HashMap<String, u64> = index
                .and_then(|svc| svc.child_dir_sizes(path, cancel))
                .map_or_default(|sizes| {
                    sizes
                        .into_iter()
                        .map(|(name, size)| (name.to_lowercase(), size.bytes))
                        .collect()
                });
            loop {
                let next = {
                    let mut guard = queue.todo.lock();
                    let next = guard.0.pop_front();
                    if next.is_none() {
                        guard.1 = false;
                    }
                    next
                };
                let Some((name, modified)) = next else {
                    worker.finish();
                    return;
                };
                if cancel.load(Ordering::Relaxed) {
                    worker.finish();
                    return;
                }
                worker.current = Some(Arc::clone(&name));
                let full = path.join(&*name);
                if let Some(&bytes) = indexed.get(&name.to_lowercase()) {
                    shared.put(full, bytes, modified);
                    worker.current = None;
                    worker.send(name, Some(bytes));
                    continue;
                }
                let Some(slot) = shared.walk_slot(cancel) else {
                    worker.finish();
                    return;
                };
                // Another tab may have just computed it.
                let reused = queue
                    .reuse
                    .then(|| shared.reusable(&full, modified))
                    .flatten();
                let bytes = if reused.is_some() {
                    reused
                } else {
                    let size = arke_core::dirsize::dir_size_tree(
                        &*fs,
                        &full,
                        SEED_DEPTH,
                        cancel,
                        &mut |sub| shared.put(sub.path, sub.size.bytes, sub.modified),
                    );
                    if cancel.load(Ordering::Relaxed) {
                        worker.finish();
                        return;
                    }
                    if let Some(size) = size {
                        shared.put(full, size.bytes, modified);
                    }
                    size.map(|s| s.bytes)
                };
                drop(slot);
                worker.current = None;
                worker.send(name, bytes);
            }
        });
    }

    pub(super) fn handle_folder_size(
        &mut self,
        dir: &Path,
        name: Arc<str>,
        bytes: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self.folder_sizes.dir.as_deref() != Some(dir) {
            return;
        }
        self.folder_sizes.pending.remove(&name);
        let before = self.folder_sizes.sizes.get(&name).copied();
        if before != bytes {
            // Same as the last value: leave it so no re-sort is needed.
            let sizes = Arc::make_mut(&mut self.folder_sizes.sizes);
            match bytes {
                Some(bytes) => sizes.insert(Arc::clone(&name), bytes),
                None => sizes.remove(&name),
            };
            self.folder_sizes.rev += 1;
        }
        // A worker that stopped on a folder (see `SizeWorker`) left the rest queued: go on.
        if let Some(queue) = self.folder_sizes.queue.clone() {
            let restart = {
                let mut todo = queue.todo.lock();
                let idle = !todo.0.is_empty() && !todo.1;
                todo.1 |= idle;
                idle
            };
            if restart {
                self.start_size_worker(queue, cx);
            }
        }
        // Re-sort when sorting by size (always once when everything is done).
        self.ensure_sorted(cx);
        cx.notify();
    }

    /// The watcher saw a change in the folder shown: cached sizes of it and the folders above it
    /// are out of date.
    pub(super) fn folder_contents_changed(&self, cx: &mut App) {
        if let Some(path) = self.model.as_ref().map(|m| m.path.as_path()) {
            shared(cx).outdate_with_ancestors(path);
        }
    }

    #[cfg(test)]
    pub(crate) fn folder_sizes_computing(&self) -> bool {
        self.folder_sizes.computing()
    }

    /// The folder sizes' bookkeeping, for a test that timed out waiting on them.
    #[cfg(test)]
    pub(crate) fn folder_sizes_state(&self) -> String {
        let f = &self.folder_sizes;
        let queue = f.queue.as_ref().map(|q| {
            let (todo, running) = {
                let t = q.todo.lock();
                (
                    t.0.iter().map(|x| Arc::clone(&x.0)).collect::<Vec<_>>(),
                    t.1,
                )
            };
            let cancel = q.cancel.load(Ordering::Relaxed);
            format!("todo={todo:?} running={running} cancel={cancel}")
        });
        format!(
            "dir={:?} sizes={:?} pending={:?} waiting={:?} stale={} queue={queue:?} rev={}",
            f.dir,
            f.sizes,
            f.pending,
            f.waiting.keys().collect::<Vec<_>>(),
            f.stale,
            f.rev
        )
    }

    /// What the Size column shows for subfolder `name`.
    pub(crate) fn folder_size(&self, name: &str) -> FolderSize {
        if self.folder_sizes.dir.is_none() {
            return FolderSize::Unknown;
        }
        let pending = self.folder_sizes.pending.contains(name)
            || self.folder_sizes.waiting.contains_key(name);
        match self.folder_sizes.sizes.get(name) {
            Some(&bytes) => FolderSize::Known {
                bytes,
                fresh: !pending,
            },
            None if pending => FolderSize::Computing,
            None => FolderSize::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn walks_are_limited_app_wide() {
        let shared = Arc::new(SharedSizes::default());
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (shared, running, most) =
                    (Arc::clone(&shared), Arc::clone(&running), Arc::clone(&most));
                std::thread::spawn(move || {
                    let never = AtomicBool::new(false);
                    let _slot = shared.walk_slot(&never).unwrap();
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), WALKS_AT_ONCE);

        // A waiting walk gives up once its folder is left.
        let held: Vec<_> = (0..WALKS_AT_ONCE)
            .map(|_| shared.walk_slot(&AtomicBool::new(false)).unwrap())
            .collect();
        assert!(shared.walk_slot(&AtomicBool::new(true)).is_none());
        drop(held);
    }

    #[test]
    fn changes_outdate_the_folder_and_its_ancestors() {
        let shared = SharedSizes::default();
        for p in [r"C:\u", r"C:\u\a", r"C:\u\a\b", r"C:\u\c"] {
            shared.put(PathBuf::from(p), 1, None);
        }
        shared.outdate_with_ancestors(Path::new(r"C:\u\a\b"));
        // Walked again, but the last value is still there to show.
        assert_eq!(shared.get(Path::new(r"C:\u\a")).map(|k| k.bytes), Some(1));
        let left: Vec<bool> = [r"C:\u", r"C:\u\a", r"C:\u\a\b", r"C:\u\c"]
            .iter()
            .map(|p| shared.reusable(Path::new(p), None).is_some())
            .collect();
        assert_eq!(left, [false, false, false, true]);
        // A changed modified time means changed direct contents.
        let at = Some(SystemTime::now());
        assert_eq!(shared.reusable(Path::new(r"C:\u\c"), at), None);
    }
}
