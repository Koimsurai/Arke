//! Background service for global search: load cache, rebuild in the background, watch recursively.
//!
//! - **Startup**: load the saved index first (a few hundred ms) so queries work right away. If it
//!   is fresh enough ([`IndexOptions::refresh_after`]) we skip the walk and rely on watching; if
//!   too old we rebuild in the background and swap it in. Also rechecked periodically.
//! - **Changes**: one recursive watch per root. Changed paths accumulate briefly, then get `stat`ed
//!   in the background (attributes only, no open, so cloud placeholders aren't downloaded) and
//!   recorded in the [`Overlay`].
//! - **Rebuild**: when the overlay grows too large or watch events are lost (buffer overflow).
//!   Records already reflected in the new index are dropped afterwards.
//! - **Queries**: [`IndexService::search`] is fast but still belongs on a background thread. When a
//!   query extends the previous one (`car` -> `carg`), only the previous matches are searched.
//!
//! - **Journal** (with the background service, see [`crate::journal`]): NTFS volumes use the USN
//!   journal instead of watching. The journal position is saved before building and replayed on the
//!   next start, so no periodic rebuilds; only a journal gap (cleared, wrapped) forces one. Volumes
//!   without the service keep using watching.
//!
//! - **Folder sizes**: on a journaled volume that is caught up, [`IndexService::child_dir_sizes`]
//!   answers from the index instead of walking (folders the index skips are still walked).
//!
//! Dropping the service stops it: walks end early and watches are removed.

use parking_lot::{Mutex, RwLock};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::frecency::FrecencySnapshot;
use crate::fs::{Fs, WatchEvent};
use crate::index::{
    BuildProgress, BuildRules, Index, IndexHit, Overlay, Search, Seen, descends_into, is_indexable,
};
use crate::index_query::{Clock, Query};
use crate::journal::{Change, Cursor, Journal, JournalEvent};
use crate::rank::Stat;

/// Rebuild once the overlay exceeds this many records.
const OVERLAY_MAX: usize = 100_000;
/// List at most this many items in a moved-in folder; more means rebuild.
const MOVED_IN_MAX: usize = 50_000;
/// After lost watch events, minimum time since the last rebuild before rebuilding again (busy
/// system drives overflow often, and a rebuild takes tens of seconds).
const RESCAN_MIN_GAP: Duration = Duration::from_hours(1);
/// How often to check whether the index is stale while running.
const REFRESH_CHECK: Duration = Duration::from_mins(10);
/// How often to save a checkpoint with a journal (latest position + changes since the build).
#[cfg(not(test))]
const CHECKPOINT_EVERY: Duration = Duration::from_mins(2);
#[cfg(test)]
const CHECKPOINT_EVERY: Duration = Duration::from_millis(40);
/// Checkpoint file magic.
const CHECKPOINT_MAGIC: &[u8; 8] = b"ARKECKP1";
/// How often to ask again when a rebuild is deferred (low memory, power saving).
#[cfg(not(test))]
const DEFERRED_RETRY: Duration = Duration::from_secs(30);
#[cfg(test)]
const DEFERRED_RETRY: Duration = Duration::from_millis(20);
/// Progress notification interval while building ("N items found" in the UI).
const PROGRESS_TICK: Duration = Duration::from_millis(400);
/// Let changes settle before processing (saving a file is often create, write, rename).
#[cfg(not(test))]
const SETTLE: Duration = Duration::from_millis(300);
#[cfg(test)]
const SETTLE: Duration = Duration::from_millis(10);

/// Hook run at the start of each background thread.
pub type ThreadInit = Arc<dyn Fn() + Send + Sync>;

/// Configuration for [`IndexService::start`].
pub struct IndexOptions {
    /// Roots to index (e.g. `C:\`, `D:\`).
    pub roots: Vec<PathBuf>,
    /// Where to save the index; `None` to not save.
    pub cache: Option<PathBuf>,
    /// Number of threads listing directories in parallel.
    pub threads: usize,
    /// Called at the start of every background thread (the app lowers I/O priority there).
    pub thread_init: ThreadInit,
    /// Rewalk once the index is older than this (at startup or on the periodic check). Changes
    /// made while closed only show up after a rebuild on volumes without a USN journal; journaled
    /// volumes ignore this.
    pub refresh_after: Duration,
    /// Change journal (background service); `None` means watch everything.
    pub journal: Option<Arc<dyn Journal>>,
    /// Whether walking is allowed now (in the app: no memory pressure, no power saving). If not,
    /// it is deferred and asked again later; the existing index stays queryable.
    pub can_build: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Skip build output and package cache folders (`node_modules`, `target` in Rust projects,
    /// see [`crate::index::is_build_dir`]).
    pub skip_build_dirs: bool,
    /// Read the saved index only: no watching, no walking (avoids waking a NAS that hasn't been
    /// opened yet).
    pub passive: bool,
    /// User-excluded folders (the folder itself is findable, its contents are not indexed).
    pub excluded: Vec<PathBuf>,
}

impl IndexOptions {
    fn rules(&self) -> BuildRules {
        BuildRules {
            skip_build_dirs: self.skip_build_dirs,
            excluded: self.excluded.clone(),
        }
    }
}

/// Status for display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexStatus {
    /// Items currently searchable; `None` before the first index exists.
    pub count: Option<usize>,
    /// Items listed so far while walking.
    pub scanning: Option<usize>,
    /// Some volume is updated live from the journal (changes made while closed already applied).
    pub live: bool,
    /// A rebuild is due but deferred because resources are tight (queries use the previous index).
    pub waiting: bool,
}

/// Global search index kept up to date in the background (see the module docs).
pub struct IndexService {
    shared: Arc<Shared>,
}

struct Shared {
    fs: Arc<dyn Fs>,
    opts: IndexOptions,
    /// Build rules (from `opts`).
    rules: BuildRules,
    index: RwLock<Option<Arc<Index>>>,
    overlay: Mutex<Overlay>,
    progress: BuildProgress,
    building: AtomicBool,
    stopped: AtomicBool,
    /// Rebuild deferred ([`IndexOptions::can_build`] said no).
    deferred: AtomicBool,
    /// Saved file currently mapped (only the other one may be written).
    cache_file: Mutex<Option<PathBuf>>,
    /// Journal position per root (after the last processed batch; same order as `opts.roots`).
    latest: Mutex<Vec<Option<Cursor>>>,
    /// Positions last written to the checkpoint (skip saving if unchanged).
    checkpointed: Mutex<Vec<Option<Cursor>>>,
    /// Positions read from the checkpoint at startup (newer than the ones saved at build time).
    resumed_from: Mutex<Vec<Option<Cursor>>>,
    /// Cache of which folders are Rust projects (their `target` is build output).
    cargo_targets: Mutex<std::collections::HashMap<PathBuf, bool>>,
    /// Changed paths waiting for stat (`true`: moved-in folder, list its contents too).
    pending: Mutex<Vec<(PathBuf, bool)>>,
    draining: AtomicBool,
    watchers: Mutex<Vec<Box<dyn Send>>>,
    /// Whether each root uses the journal (same order as `opts.roots`).
    journaled: Mutex<Vec<bool>>,
    /// Active journal followers (restarted from the new position after a rebuild).
    followers: Mutex<Vec<Box<dyn Send>>>,
    /// Number of roots caught up with the journal.
    live: std::sync::atomic::AtomicUsize,
    /// Which roots are caught up with the journal (same order as `opts.roots`).
    caught_up: Mutex<Vec<bool>>,
    /// Matches of the previous query, used to narrow the next one.
    narrow: Mutex<Option<Narrow>>,
    on_change: Box<dyn Fn() + Send + Sync>,
}

struct Narrow {
    /// Which index these belong to (invalid once the index is replaced).
    index: Weak<Index>,
    input: String,
    show_hidden: bool,
    ids: Arc<Vec<u32>>,
}

/// Plain name query (no filters, folders, exclusions or other syntax): can search within the
/// previous results.
fn can_narrow(input: &str) -> bool {
    !input.trim().is_empty() && !input.contains(['!', '^', '$', '\'', '\\', '/', ':', '*', '|'])
}

/// Whether results can be kept for narrowing: every alphanumeric term has at least 3 chars
/// (CJK terms are always fuzzy).
///
/// Terms of 1-2 chars match as substrings, 3+ fuzzily; `re` (substring) -> `rep` (fuzzy) can
/// match things `re` didn't, so it can't narrow. With all terms fuzzy, typing more only
/// tightens. It also skips short queries with millions of matches, where storing is slower.
fn can_store(input: &str) -> bool {
    can_narrow(input)
        && input
            .split_whitespace()
            .all(|w| !w.is_ascii() || w.len() >= 3)
}

/// `next` extends `prev`: anything matching `next` also matches `prev` (fuzzy is subsequence
/// matching, so more input only tightens).
fn refines(prev: &str, next: &str) -> bool {
    let prev = prev.trim();
    !prev.is_empty() && next.trim_start().starts_with(prev)
}

impl IndexService {
    /// Starts the service (cache loading, watching and rebuilding all happen in the background).
    /// Calls `on_change` on a background thread when status changes.
    pub fn start(
        fs: Arc<dyn Fs>,
        opts: IndexOptions,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            fs,
            rules: opts.rules(),
            opts,
            index: RwLock::new(None),
            overlay: Mutex::new(Overlay::default()),
            progress: BuildProgress::default(),
            building: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
            deferred: AtomicBool::new(false),
            cache_file: Mutex::new(None),
            latest: Mutex::new(Vec::new()),
            checkpointed: Mutex::new(Vec::new()),
            resumed_from: Mutex::new(Vec::new()),
            cargo_targets: Mutex::new(std::collections::HashMap::new()),
            pending: Mutex::new(Vec::new()),
            draining: AtomicBool::new(false),
            watchers: Mutex::new(Vec::new()),
            journaled: Mutex::new(Vec::new()),
            followers: Mutex::new(Vec::new()),
            live: std::sync::atomic::AtomicUsize::new(0),
            caught_up: Mutex::new(Vec::new()),
            narrow: Mutex::new(None),
            on_change: Box::new(on_change),
        });
        let weak = Arc::downgrade(&shared);
        spawn("index-start", &shared.opts.thread_init.clone(), move || {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.load_cache();
            shared.load_checkpoint();
            if shared.opts.passive {
                shared.building.store(false, Ordering::Release);
                (shared.on_change)();
                return;
            }
            let resume = shared.choose_sources();
            shared.watch_roots(&weak);
            match resume {
                // Journal available and the saved position still connects: replay changes from
                // while we were closed, no rewalk.
                Start::Resume(cursors) if !(shared.has_unjournaled() && shared.is_stale()) => {
                    shared.follow_all(&cursors);
                    shared.building.store(false, Ordering::Release);
                    (shared.on_change)();
                }
                Start::Resume(_) | Start::Build => shared.build(),
                Start::NoJournal if shared.is_stale() => shared.build(),
                Start::NoJournal => {
                    // Saved index is fresh enough: no rewalk.
                    shared.building.store(false, Ordering::Release);
                    (shared.on_change)();
                }
            }
            drop(shared);
            // Periodic staleness check while running; deferred rebuilds are retried.
            let mut since_check = Duration::ZERO;
            let mut since_checkpoint = Duration::ZERO;
            let tick = DEFERRED_RETRY.min(CHECKPOINT_EVERY);
            loop {
                std::thread::sleep(tick);
                since_check += tick;
                since_checkpoint += tick;
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                if shared.stopped.load(Ordering::Relaxed) {
                    return;
                }
                if since_checkpoint >= CHECKPOINT_EVERY {
                    since_checkpoint = Duration::ZERO;
                    shared.save_checkpoint();
                }
                if shared.deferred.load(Ordering::Relaxed) && (shared.opts.can_build)() {
                    shared.request_build();
                }
                if since_check >= REFRESH_CHECK {
                    since_check = Duration::ZERO;
                    // No periodic rebuild needed if every root is journaled.
                    if shared.has_unjournaled() && shared.is_stale() {
                        shared.request_build();
                    }
                }
            }
        });
        Self { shared }
    }

    /// Current status for display; cheap, fine on the UI thread.
    pub fn status(&self) -> IndexStatus {
        let count = self.shared.index.read().as_ref().map(|i| i.len());
        let scanning = self
            .shared
            .building
            .load(Ordering::Relaxed)
            .then(|| self.shared.progress.scanned.load(Ordering::Relaxed));
        let live = self.shared.live.load(Ordering::Relaxed) > 0;
        IndexStatus {
            count,
            scanning,
            live,
            waiting: self.shared.deferred.load(Ordering::Relaxed),
        }
    }

    /// Runs a query (on a background thread). `clock` is for date filters like `dm:today`.
    pub fn search(
        &self,
        input: &str,
        clock: Clock,
        limit: usize,
        show_hidden: bool,
    ) -> Vec<IndexHit> {
        let index = self.shared.index.read().clone();
        let Some(index) = index else {
            return Vec::new();
        };
        let query = Query::parse(input, clock);
        let narrowable = can_narrow(input);
        let within = if narrowable {
            self.shared.narrow.lock().as_ref().and_then(|n| {
                (Weak::ptr_eq(&n.index, &Arc::downgrade(&index))
                    && n.show_hidden == show_hidden
                    && refines(&n.input, input))
                .then(|| n.ids.clone())
            })
        } else {
            None
        };
        let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let result = index.search(&Search {
            query: &query,
            limit,
            show_hidden,
            threads,
            within: within.as_deref().map(Vec::as_slice),
            collect_matched: can_store(input),
        });
        if let Some(ids) = result.matched {
            *self.shared.narrow.lock() = Some(Narrow {
                index: Arc::downgrade(&index),
                input: input.trim().to_string(),
                show_hidden,
                ids: Arc::new(ids),
            });
        }
        self.shared
            .overlay
            .lock()
            .apply(result.hits, &query, limit, show_hidden)
    }

    /// Visited items matching `input` that this index covers, read from disk: the index's top hits
    /// may not include the copy the user opens (see [`crate::rank::frecent_hits`]). Background
    /// threads only.
    pub fn frecent_hits(
        &self,
        input: &str,
        clock: Clock,
        show_hidden: bool,
        snapshot: &FrecencySnapshot,
        max: usize,
    ) -> Vec<IndexHit> {
        let query = Query::parse(input, clock);
        let shared = &self.shared;
        crate::rank::frecent_hits(
            snapshot,
            &query,
            show_hidden,
            max,
            |path| is_indexable(&shared.opts.roots, path) && !shared.rules.is_inside_excluded(path),
            |path| {
                shared.fs.stat(path).ok().map(|e| Stat {
                    is_dir: e.is_dir_like(),
                    size: e.size,
                    modified: e.modified,
                    hidden: e.is_hidden(),
                })
            },
        )
    }

    /// Sizes of the subfolders of `dir` (name, size), when the index follows `dir`'s volume
    /// through the change journal and is caught up, so it is complete and current. Folders the
    /// index has no contents for (build folders, exclusions, hidden system folders) are walked
    /// through `fs` and added, so totals match [`crate::dirsize::dir_size`]. `None`: not covered
    /// (no journal, not caught up, folder not indexed) or cancelled; walk instead. Background
    /// threads only.
    pub fn child_dir_sizes(
        &self,
        dir: &Path,
        cancel: &AtomicBool,
    ) -> Option<Vec<(String, crate::dirsize::DirSize)>> {
        let shared = &self.shared;
        let root = shared.opts.roots.iter().position(|r| {
            dir.components().count() >= r.components().count()
                && dir
                    .components()
                    .zip(r.components())
                    .all(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str()))
        })?;
        let live = shared.journaled.lock().get(root).copied().unwrap_or(false)
            && shared.caught_up.lock().get(root).copied().unwrap_or(false);
        if !live {
            return None;
        }
        let index = shared.index.read().clone()?;
        let overlay = shared.overlay.lock().under(dir);
        let sizes = index.child_dir_sizes(dir, &overlay, cancel)?;
        let mut out = Vec::with_capacity(sizes.len());
        for child in sizes {
            let mut size = child.size;
            for path in &child.unindexed {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                match crate::dirsize::dir_size(shared.fs.as_ref(), path, cancel) {
                    Some(walked) => {
                        size.bytes += walked.bytes;
                        size.files += walked.files;
                        size.partial |= walked.partial;
                    }
                    None => size.partial = true,
                }
            }
            out.push((child.name, size));
        }
        Some(out)
    }

    /// Rewalks (no-op if a build is already running).
    pub fn rebuild(&self) {
        self.shared.request_build();
    }
}

impl Drop for IndexService {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        self.shared.watchers.lock().clear();
        self.shared.followers.lock().clear();
    }
}

/// How to start up.
enum Start {
    /// No journaled volumes: decide by the saved index's age as before.
    NoJournal,
    /// Journal available but the saved position doesn't connect (no save, journal replaced):
    /// rebuild.
    Build,
    /// Resume from saved positions (same order as `roots`, `None` for unjournaled ones).
    Resume(Vec<Option<Cursor>>),
}

impl Shared {
    /// Age of the current index; `None` if there is none yet.
    fn age(&self) -> Option<Duration> {
        let built = self.index.read().as_ref()?.built_at();
        Some(SystemTime::now().duration_since(built).unwrap_or_default())
    }

    fn is_stale(&self) -> bool {
        self.age().is_none_or(|age| age >= self.opts.refresh_after)
    }

    /// Loads the saved index: picks the newer of `cache` and its alternate (see [`alt_path`]),
    /// mapped directly rather than copied into memory. The other one is deleted.
    fn load_cache(&self) {
        let Some(path) = &self.opts.cache else {
            return;
        };
        let mut best: Option<(Index, PathBuf)> = None;
        for candidate in [path.clone(), alt_path(path)] {
            match Index::open(&candidate) {
                // Roots or the skip-build-dirs setting changed: the old index is unusable.
                Ok(index)
                    if index.roots() == self.opts.roots.as_slice()
                        && index.skipped_build_dirs() == self.opts.skip_build_dirs
                        && index.excluded_hash() == self.rules.fingerprint() =>
                {
                    if best
                        .as_ref()
                        .is_none_or(|(b, _)| index.built_at() > b.built_at())
                    {
                        best = Some((index, candidate));
                    }
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    log::info!(
                        "global search index unusable, rebuilding {}: {err}",
                        candidate.display()
                    );
                }
            }
        }
        let Some((index, from)) = best else {
            return;
        };
        log::info!("loaded index: {} items ({})", index.len(), from.display());
        for other in [path.clone(), alt_path(path)] {
            if other != from {
                let _ = std::fs::remove_file(&other);
            }
        }
        *self.cache_file.lock() = Some(from);
        self.set_index(index);
    }

    fn set_index(&self, index: Index) {
        *self.index.write() = Some(Arc::new(index));
        (self.on_change)();
    }

    /// Asks the background service which roots can use the journal and decides how to start.
    fn choose_sources(&self) -> Start {
        let now: Vec<Option<Cursor>> = self
            .opts
            .roots
            .iter()
            .map(|root| self.opts.journal.as_ref().and_then(|j| j.cursor(root)))
            .collect();
        *self.journaled.lock() = now.iter().map(Option::is_some).collect();
        if now.iter().all(Option::is_none) {
            return Start::NoJournal;
        }
        let index = self.index.read().clone();
        let Some(index) = index else {
            return Start::Build;
        };
        let mut saved = Vec::with_capacity(now.len());
        let resumed = self.resumed_from.lock().clone();
        for (i, current) in now.iter().enumerate() {
            // The checkpoint position is newer than the build-time one (still connects after a
            // reboot when the journal has overwritten the build-time position).
            let from = resumed
                .get(i)
                .copied()
                .flatten()
                .or_else(|| index.cursor(i));
            match (current, from) {
                (None, _) => saved.push(None),
                // Same journal and the saved position isn't ahead of now (otherwise, e.g. clock
                // went back, rebuild).
                (Some(cur), Some(old)) if cur.journal == old.journal && old.usn <= cur.usn => {
                    saved.push(Some(old));
                }
                (Some(_), _) => {
                    log::info!(
                        "global search: journal for {} doesn't match the index, rebuilding",
                        self.opts.roots[i].display()
                    );
                    return Start::Build;
                }
            }
        }
        Start::Resume(saved)
    }

    fn has_unjournaled(&self) -> bool {
        let journaled = self.journaled.lock();
        journaled.len() < self.opts.roots.len() || journaled.iter().any(|j| !j)
    }

    /// Follows the journal from `cursors` (replacing current followers). Journaled roots with no
    /// position (service stopped) or that fail to follow fall back to watching.
    fn follow_all(self: &Arc<Self>, cursors: &[Option<Cursor>]) {
        let Some(journal) = self.opts.journal.clone() else {
            return;
        };
        let journaled = self.journaled.lock().clone();
        *self.latest.lock() = cursors.to_vec();
        let mut followers = self.followers.lock();
        followers.clear();
        self.live.store(0, Ordering::Relaxed);
        *self.caught_up.lock() = vec![false; self.opts.roots.len()];
        let mut fallback = Vec::new();
        for (i, root) in self.opts.roots.iter().enumerate() {
            if !journaled.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(cursor) = cursors.get(i).copied().flatten() else {
                fallback.push(i);
                continue;
            };
            let weak = Arc::downgrade(self);
            let caught_up = AtomicBool::new(false);
            let followed = journal.follow(
                root,
                cursor,
                Box::new(move |event| {
                    if let Some(shared) = weak.upgrade() {
                        shared.on_journal(i, event, &caught_up);
                    }
                }),
            );
            match followed {
                Ok(f) => followers.push(f),
                Err(err) => {
                    log::warn!("can't follow journal {}: {err}", root.display());
                    fallback.push(i);
                }
            }
        }
        drop(followers);
        if !fallback.is_empty() {
            {
                let mut journaled = self.journaled.lock();
                for &i in &fallback {
                    journaled[i] = false;
                }
            }
            self.watch_roots_at(&Arc::downgrade(self), &fallback);
        }
    }

    fn on_journal(self: &Arc<Self>, root: usize, event: JournalEvent, caught_up: &AtomicBool) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        match event {
            JournalEvent::Changed { changes, cursor } => {
                self.enqueue(
                    changes
                        .into_iter()
                        .map(|Change { path, moved_in }| (path, moved_in)),
                );
                // Queue first, then record the position, so a checkpoint sees pending work and
                // never skips it.
                if let Some(slot) = self.latest.lock().get_mut(root) {
                    *slot = Some(cursor);
                }
            }
            JournalEvent::CaughtUp => {
                if let Some(slot) = self.caught_up.lock().get_mut(root) {
                    *slot = true;
                }
                if !caught_up.swap(true, Ordering::Relaxed) {
                    self.live.fetch_add(1, Ordering::Relaxed);
                    (self.on_change)();
                }
            }
            JournalEvent::Lost => {
                if let Some(slot) = self.caught_up.lock().get_mut(root) {
                    *slot = false;
                }
                if caught_up.swap(false, Ordering::Relaxed) {
                    self.live.fetch_sub(1, Ordering::Relaxed);
                }
                log::info!("global search: journal gap, rebuilding");
                self.request_build();
            }
        }
    }

    /// Saves a checkpoint: the latest journal position per root plus changes recorded since the
    /// build. After a reboot the journal has usually overwritten the build-time position; with this
    /// we resume from the latest one instead of rebuilding. Skipped while changes are pending or a
    /// rebuild runs.
    fn save_checkpoint(&self) {
        let Some(cache) = &self.opts.cache else {
            return;
        };
        if !self.journaled.lock().iter().any(|j| *j) {
            return;
        }
        // Take positions before checking the queue: anything arriving later has a newer position
        // and is replayed on restart, never lost.
        let cursors = self.latest.lock().clone();
        if cursors.iter().all(Option::is_none) || cursors == *self.checkpointed.lock() {
            return;
        }
        if !self.pending.lock().is_empty()
            || self.draining.load(Ordering::Acquire)
            || self.building.load(Ordering::Acquire)
        {
            return;
        }
        let Some(index) = self.index.read().clone() else {
            return;
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CHECKPOINT_MAGIC);
        bytes.extend_from_slice(&crate::index::unix_secs(index.built_at()).to_le_bytes());
        bytes.extend_from_slice(&(cursors.len() as u32).to_le_bytes());
        for cursor in &cursors {
            match cursor {
                Some(c) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&c.journal.to_le_bytes());
                    bytes.extend_from_slice(&c.usn.to_le_bytes());
                }
                None => bytes.extend_from_slice(&[0; 17]),
            }
        }
        if self.overlay.lock().write_to(&mut bytes).is_err() {
            return;
        }
        let path = checkpoint_path(cache);
        let tmp = path.with_extension("checkpoint.tmp");
        let written = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path));
        match written {
            Ok(()) => *self.checkpointed.lock() = cursors,
            Err(err) => log::warn!("failed to save index checkpoint {}: {err}", path.display()),
        }
    }

    /// Loads the checkpoint (after the saved index): if it belongs to the same index, puts its
    /// changes back in the overlay and records the latest positions for `choose_sources`.
    fn load_checkpoint(&self) {
        let Some(cache) = &self.opts.cache else {
            return;
        };
        let Ok(bytes) = std::fs::read(checkpoint_path(cache)) else {
            return;
        };
        let Some(index) = self.index.read().clone() else {
            return;
        };
        let parsed = (|| -> Option<(Vec<Option<Cursor>>, Overlay)> {
            let mut r = bytes.as_slice();
            let mut take = |n: usize| -> Option<&[u8]> {
                let (head, rest) = r.split_at_checked(n)?;
                r = rest;
                Some(head)
            };
            if take(8)? != CHECKPOINT_MAGIC {
                return None;
            }
            let built = u64::from_le_bytes(take(8)?.try_into().ok()?);
            if built != crate::index::unix_secs(index.built_at()) {
                return None;
            }
            let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            if count != self.opts.roots.len() {
                return None;
            }
            let mut cursors = Vec::with_capacity(count);
            for _ in 0..count {
                let c = take(17)?;
                cursors.push((c[0] == 1).then(|| Cursor {
                    journal: u64::from_le_bytes(c[1..9].try_into().unwrap_or_default()),
                    usn: i64::from_le_bytes(c[9..17].try_into().unwrap_or_default()),
                }));
            }
            let overlay = Overlay::read_from(&mut r).ok()?;
            Some((cursors, overlay))
        })();
        match parsed {
            Some((cursors, overlay)) => {
                log::info!("resuming index from checkpoint ({} changes)", overlay.len());
                *self.overlay.lock() = overlay;
                self.checkpointed.lock().clone_from(&cursors);
                *self.resumed_from.lock() = cursors;
            }
            None => log::info!("global search: checkpoint doesn't match index, ignored"),
        }
    }

    /// Watches the roots that don't use the journal.
    fn watch_roots(&self, weak: &Weak<Self>) {
        let journaled = self.journaled.lock().clone();
        let which: Vec<usize> = (0..self.opts.roots.len())
            .filter(|&i| !journaled.get(i).copied().unwrap_or(false))
            .collect();
        self.watch_roots_at(weak, &which);
    }

    fn watch_roots_at(&self, weak: &Weak<Self>, which: &[usize]) {
        for root in which.iter().filter_map(|&i| self.opts.roots.get(i)) {
            let weak = weak.clone();
            let watched = self.fs.watch_tree(
                root,
                Box::new(move |event| {
                    if let Some(shared) = weak.upgrade() {
                        shared.on_event(event);
                    }
                }),
            );
            match watched {
                Ok(w) => self.watchers.lock().push(w),
                Err(err) => log::warn!("cannot watch {}: {err}", root.display()),
            }
        }
    }

    fn on_event(self: &Arc<Self>, event: WatchEvent) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        match event {
            WatchEvent::Changed(paths) => self.enqueue(paths.into_iter().map(|p| (p, false))),
            WatchEvent::Rescan => {
                if self.age().is_none_or(|age| age >= RESCAN_MIN_GAP) {
                    self.request_build();
                }
            }
        }
    }

    /// With skip-build-dirs: whether the path is inside `node_modules`, a Rust project's `target`
    /// and the like (the last segment itself is still recorded). `target` only counts next to a
    /// `Cargo.toml`; results are cached.
    fn in_build_dir(&self, path: &Path) -> bool {
        if !self.opts.skip_build_dirs {
            return false;
        }
        let parts: Vec<_> = path.components().collect();
        let mut prefix = PathBuf::new();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            let name = part.as_os_str().to_string_lossy().to_lowercase();
            if crate::index::is_named_build_dir(&name) {
                return true;
            }
            if name == "target" {
                let known = self.cargo_targets.lock().get(&prefix).copied();
                let cargo = known.unwrap_or_else(|| {
                    let found = self.fs.stat(&prefix.join("Cargo.toml")).is_ok();
                    self.cargo_targets.lock().insert(prefix.clone(), found);
                    found
                });
                if cargo {
                    return true;
                }
            }
            prefix.push(part);
        }
        false
    }

    /// Takes possibly changed paths and stats them in the background.
    fn enqueue(self: &Arc<Self>, paths: impl Iterator<Item = (PathBuf, bool)>) {
        let roots = &self.opts.roots;
        let too_many = {
            let mut pending = self.pending.lock();
            pending.extend(paths.filter(|(p, _)| {
                is_indexable(roots, p) && !self.in_build_dir(p) && !self.rules.is_inside_excluded(p)
            }));
            // Too many at once (e.g. replaying a long absence): a rebuild is faster.
            let too_many = pending.len() > OVERLAY_MAX;
            if too_many {
                pending.clear();
            }
            too_many
        };
        if too_many {
            self.request_build();
            return;
        }
        if !self.draining.swap(true, Ordering::AcqRel) {
            let weak = Arc::downgrade(self);
            spawn("index-update", &self.opts.thread_init, move || {
                if let Some(shared) = weak.upgrade() {
                    shared.drain();
                }
            });
        }
    }

    /// Stats accumulated paths one by one into the overlay.
    fn drain(self: &Arc<Self>) {
        loop {
            std::thread::sleep(SETTLE);
            let mut paths = std::mem::take(&mut *self.pending.lock());
            if paths.is_empty() {
                self.draining.store(false, Ordering::Release);
                // New paths arrived just before letting go: run another round.
                if self.pending.lock().is_empty() || self.draining.swap(true, Ordering::AcqRel) {
                    return;
                }
                continue;
            }
            paths.sort_unstable();
            // One record per path; if any was a move-in it counts as one (after sorting, true comes
            // last).
            paths.dedup_by(|later, earlier| {
                let same = later.0 == earlier.0;
                if same {
                    earlier.1 |= later.1;
                }
                same
            });
            for (path, moved_in) in paths {
                if self.stopped.load(Ordering::Relaxed) {
                    return;
                }
                let entry = match self.fs.stat(&path) {
                    Ok(entry) => entry,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        self.overlay.lock().record(path, None);
                        continue;
                    }
                    // Access denied etc.: unknown, don't record.
                    Err(_) => continue,
                };
                let walk = moved_in && descends_into(&entry);
                self.overlay
                    .lock()
                    .record(path.clone(), Some(Seen::of(&entry)));
                if walk && !self.record_tree(&path) {
                    self.request_build();
                }
            }
            if self.overlay.lock().len() > OVERLAY_MAX {
                self.request_build();
            }
        }
    }

    /// Moved-in (or renamed) folder: record its contents in the overlay too (the index has the old
    /// paths). Returns `false` if there are too many (left to a rebuild).
    fn record_tree(&self, dir: &Path) -> bool {
        let mut stack = vec![dir.to_path_buf()];
        let mut count = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(iter) = self.fs.read_dir(&dir) else {
                continue;
            };
            for entry in iter.filter_map(Result::ok) {
                if self.stopped.load(Ordering::Relaxed) {
                    return true;
                }
                count += 1;
                if count > MOVED_IN_MAX {
                    return false;
                }
                let path = dir.join(&*entry.name);
                if descends_into(&entry) {
                    stack.push(path.clone());
                }
                self.overlay.lock().record(path, Some(Seen::of(&entry)));
            }
        }
        true
    }

    /// Rebuilds in the background (no-op if already building).
    fn request_build(self: &Arc<Self>) {
        if self.stopped.load(Ordering::Relaxed) || self.building.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        spawn("index-build", &self.opts.thread_init, move || {
            if let Some(shared) = weak.upgrade() {
                shared.build();
            }
        });
    }

    /// Walks all roots, swaps in and saves the result. Deferred while resources are tight (the
    /// current index keeps serving).
    #[allow(clippy::too_many_lines)]
    fn build(self: &Arc<Self>) {
        if !(self.opts.can_build)() {
            if !self.deferred.swap(true, Ordering::Relaxed) {
                log::info!("global search: low memory or power saving, deferring index update");
            }
            self.building.store(false, Ordering::Release);
            (self.on_change)();
            return;
        }
        self.deferred.store(false, Ordering::Relaxed);
        self.building.store(true, Ordering::Release);
        self.progress.scanned.store(0, Ordering::Relaxed);
        (self.on_change)();
        // Periodic progress while building (ends when done or stopped).
        let weak = Arc::downgrade(self);
        spawn("index-progress", &self.opts.thread_init, move || {
            loop {
                std::thread::sleep(PROGRESS_TICK);
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                if !shared.building.load(Ordering::Acquire)
                    || shared.stopped.load(Ordering::Relaxed)
                {
                    return;
                }
                (shared.on_change)();
            }
        });
        let started = Instant::now();
        let seq = self.overlay.lock().seq();
        // Record journal positions before walking: changes during the walk are replayed from here
        // (duplicates are harmless).
        let cursors: Vec<Option<Cursor>> = {
            let journaled = self.journaled.lock().clone();
            self.opts
                .roots
                .iter()
                .enumerate()
                .map(|(i, root)| {
                    if !journaled.get(i).copied().unwrap_or(false) {
                        return None;
                    }
                    self.opts.journal.as_ref().and_then(|j| j.cursor(root))
                })
                .collect()
        };
        let init = self.opts.thread_init.clone();
        // Roots with the background service: have it read the NTFS MFT directly (seconds); walk
        // otherwise.
        let journal = self.opts.journal.clone();
        let listable: Vec<PathBuf> = self
            .opts
            .roots
            .iter()
            .zip(&cursors)
            .filter(|(_, c)| c.is_some())
            .map(|(r, _)| r.clone())
            .collect();
        let lister = move |root: &Path, sink: crate::journal::ListSink| -> io::Result<()> {
            match &journal {
                Some(journal) if listable.iter().any(|r| r == root) => journal.list(root, sink),
                _ => Err(io::ErrorKind::Unsupported.into()),
            }
        };
        let index = Index::build_listed(
            self.fs.as_ref(),
            &self.opts.roots,
            self.opts.threads,
            &self.stopped,
            &self.progress,
            &move || init(),
            &self.rules,
            &lister,
        )
        .with_cursors(cursors.clone());
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        log::info!(
            "global search index built: {} items, {:.1} s",
            index.len(),
            started.elapsed().as_secs_f32()
        );
        let mut index = index;
        let mut replaced = None;
        if let Some(path) = &self.opts.cache {
            // The currently mapped file can't be overwritten: write the other one and map that
            // instead (the in-memory copy can then be released).
            let current = self.cache_file.lock().clone();
            let target = if current.as_deref() == Some(path.as_path()) {
                alt_path(path)
            } else {
                path.clone()
            };
            match save(&target, &index) {
                Ok(()) => match Index::open(&target) {
                    Ok(mapped) if mapped.len() == index.len() => {
                        index = mapped;
                        replaced = self.cache_file.lock().replace(target);
                    }
                    Ok(_) => log::warn!("index file read back differently: {}", target.display()),
                    Err(err) => log::warn!("cannot reread index file {}: {err}", target.display()),
                },
                Err(err) => log::warn!("failed to save index file {}: {err}", target.display()),
            }
        }
        self.overlay.lock().prune_before(seq);
        self.building.store(false, Ordering::Release);
        self.set_index(index);
        // Old file: can't be deleted while queries still use it; leave it (overwritten next save,
        // cleaned at startup).
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(old);
        }
        self.follow_all(&cursors);
    }
}

/// Journal checkpoint file name (`index.bin` -> `index.checkpoint`). Shared by both saved
/// files; it records which index it belongs to.
fn checkpoint_path(cache: &Path) -> PathBuf {
    cache.with_extension("checkpoint")
}

/// Alternate save name (`index.bin` -> `index.alt.bin`). The mapped file can't be
/// overwritten, so saves alternate between the two names.
fn alt_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.alt.{}", ext.to_string_lossy()),
        None => format!("{stem}.alt"),
    };
    path.with_file_name(name)
}

fn save(path: &Path, index: &Index) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut w = io::BufWriter::new(std::fs::File::create(&tmp)?);
        index.write_to(&mut w)?;
        io::Write::flush(&mut w)?;
    }
    std::fs::rename(&tmp, path)
}

fn spawn(name: &str, init: &ThreadInit, f: impl FnOnce() + Send + 'static) {
    let init = init.clone();
    let spawned = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            init();
            f();
        });
    if let Err(err) = spawned {
        log::warn!("failed to spawn thread {name}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind};
    use crate::fake::FakeFs;
    use crate::journal::FakeJournal;

    fn wait(what: &str, ok: impl Fn() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(5), "timeout: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn opts(cache: Option<PathBuf>) -> IndexOptions {
        IndexOptions {
            roots: vec![PathBuf::from(r"C:\")],
            cache,
            threads: 2,
            thread_init: Arc::new(|| {}),
            refresh_after: Duration::ZERO,
            journal: None,
            can_build: Arc::new(|| true),
            skip_build_dirs: false,
            passive: false,
            excluded: Vec::new(),
        }
    }

    fn found(svc: &IndexService, query: &str) -> Vec<String> {
        svc.search(query, Clock::default(), 10, false)
            .iter()
            .map(|h| h.path.display().to_string())
            .collect()
    }

    fn ready(svc: &IndexService) -> bool {
        let s = svc.status();
        s.count.is_some() && s.scanning.is_none()
    }

    /// Skip build folders: `node_modules` and Rust `target` are not entered (the folder itself is
    /// findable); a `target` without `Cargo.toml` is indexed normally. Watch events inside are
    /// ignored.
    #[test]
    fn skips_build_folders_when_asked() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\proj",
            vec![
                Entry::new("node_modules", EntryKind::Dir, 0, None, 0),
                Entry::new("target", EntryKind::Dir, 0, None, 0),
                Entry::new("Cargo.toml", EntryKind::File, 1, None, 0),
                Entry::new("main-report.rs", EntryKind::File, 1, None, 0),
            ],
        );
        fs.insert_dir(
            r"C:\proj\node_modules",
            vec![Entry::new("lib-report.js", EntryKind::File, 1, None, 0)],
        );
        fs.insert_dir(
            r"C:\proj\target",
            vec![Entry::new("build-report.o", EntryKind::File, 1, None, 0)],
        );
        fs.insert_dir(
            r"C:\photos",
            vec![Entry::new("target", EntryKind::Dir, 0, None, 0)],
        );
        fs.insert_dir(
            r"C:\photos\target",
            vec![Entry::new("trip-report.jpg", EntryKind::File, 1, None, 0)],
        );
        let svc = IndexService::start(
            fs.clone(),
            IndexOptions {
                skip_build_dirs: true,
                ..opts(None)
            },
            || {},
        );
        wait("index built", || ready(&svc));
        let mut hits = found(&svc, "report");
        hits.sort();
        assert_eq!(
            hits,
            [
                r"C:\photos\target\trip-report.jpg",
                r"C:\proj\main-report.rs"
            ]
        );
        assert_eq!(found(&svc, "node_modules"), [r"C:\proj\node_modules"]);

        fs.add_file(Path::new(r"C:\proj\target"), "new-report.o", 1);
        fs.add_file(Path::new(r"C:\proj"), "new-report.md", 1);
        fs.emit(
            Path::new(r"C:\"),
            &WatchEvent::Changed(vec![
                PathBuf::from(r"C:\proj\target\new-report.o"),
                PathBuf::from(r"C:\proj\new-report.md"),
            ]),
        );
        wait("new file in project", || {
            found(&svc, "new-report").len() == 1
        });
        assert_eq!(found(&svc, "new-report"), [r"C:\proj\new-report.md"]);
    }

    /// Excluded folders: findable themselves, contents not indexed (case-insensitive), watch events
    /// inside ignored. Changing the exclusions invalidates the saved index.
    #[test]
    fn excluded_folders_are_not_indexed_and_invalidate_the_cache() {
        let (dir, cache) = temp_cache("excluded");
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\",
            vec![
                Entry::new("Games", EntryKind::Dir, 0, None, 0),
                Entry::new("Work", EntryKind::Dir, 0, None, 0),
            ],
        );
        fs.insert_dir(
            r"C:\Games",
            vec![Entry::new("level-report.dat", EntryKind::File, 1, None, 0)],
        );
        fs.insert_dir(
            r"C:\Work",
            vec![Entry::new("q3-report.docx", EntryKind::File, 1, None, 0)],
        );
        let excluded = |list: &[&str]| IndexOptions {
            excluded: list.iter().map(PathBuf::from).collect(),
            ..opts(Some(cache.clone()))
        };
        {
            let svc = IndexService::start(fs.clone(), excluded(&[r"c:\games\"]), || {});
            wait("index built", || ready(&svc));
            assert_eq!(found(&svc, "report"), [r"C:\Work\q3-report.docx"]);
            assert_eq!(found(&svc, "games"), [r"C:\Games"]);
            fs.add_file(Path::new(r"C:\Games"), "new-report.sav", 1);
            fs.add_file(Path::new(r"C:\Work"), "new-report.md", 1);
            fs.emit(
                Path::new(r"C:\"),
                &WatchEvent::Changed(vec![
                    PathBuf::from(r"C:\Games\new-report.sav"),
                    PathBuf::from(r"C:\Work\new-report.md"),
                ]),
            );
            wait("new file", || !found(&svc, "new-report").is_empty());
            assert_eq!(found(&svc, "new-report"), [r"C:\Work\new-report.md"]);
        }
        // Removing the exclusion invalidates the save; found after the rebuild.
        let svc = IndexService::start(fs, excluded(&[]), || {});
        wait("rebuild", || found(&svc, "level-report").len() == 1);
        drop(svc);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exclusion_rules_compare_paths_loosely() {
        let rules = BuildRules {
            skip_build_dirs: false,
            excluded: vec![PathBuf::from(r"D:\Media\"), PathBuf::from(r"E:\")],
        };
        assert!(rules.is_inside_excluded(Path::new(r"d:\media\a.mp4")));
        assert!(!rules.is_inside_excluded(Path::new(r"D:\Media")));
        assert!(!rules.is_inside_excluded(Path::new(r"D:\Media2\a.mp4")));
        assert!(rules.is_inside_excluded(Path::new(r"E:\x")));
        assert_eq!(BuildRules::default().fingerprint(), 0);
        let same = BuildRules {
            skip_build_dirs: true,
            excluded: vec![PathBuf::from(r"e:\"), PathBuf::from(r"d:\media")],
        };
        assert_eq!(rules.fingerprint(), same.fingerprint());
        assert_ne!(rules.fingerprint(), 0);
    }

    /// Passive mode (network location not opened yet): loads the saved results but never walks or
    /// watches.
    #[test]
    fn passive_mode_only_reads_the_cache() {
        let (dir, cache) = temp_cache("passive");
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\nas",
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        {
            let svc = IndexService::start(fs, opts(Some(cache.clone())), || {});
            wait("first build", || ready(&svc));
        }
        let fs = FakeFs::new();
        let svc = IndexService::start(
            fs.clone(),
            IndexOptions {
                passive: true,
                ..opts(Some(cache))
            },
            || {},
        );
        wait("cache loaded", || ready(&svc));
        assert_eq!(found(&svc, "report"), [r"C:\nas\old-report.txt"]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(fs.read_dir_calls(), 0, "no walk");
        assert_eq!(fs.watcher_count(Path::new(r"C:\")), 0, "no watch");
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tight resources (`can_build` says no): no walk, status shows deferred; catches up once
    /// allowed.
    #[test]
    fn defers_building_while_resources_are_tight() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\a",
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let ok = Arc::new(AtomicBool::new(false));
        let gate = ok.clone();
        let svc = IndexService::start(
            fs,
            IndexOptions {
                can_build: Arc::new(move || gate.load(Ordering::Relaxed)),
                ..opts(None)
            },
            || {},
        );
        wait("deferred", || svc.status().waiting);
        assert_eq!(svc.status().count, None, "no walk");
        ok.store(true, Ordering::Relaxed);
        wait("builds once resources allow", || {
            ready(&svc) && !svc.status().waiting
        });
        assert_eq!(found(&svc, "report"), [r"C:\a\report.docx"]);
    }

    #[test]
    fn builds_in_background_then_follows_changes() {
        let fs = FakeFs::new();
        let me = Path::new(r"C:\Users\me");
        fs.insert_dir(
            me,
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let svc = IndexService::start(fs.clone(), opts(None), || {});
        wait("index built", || ready(&svc));
        assert_eq!(found(&svc, "report"), [r"C:\Users\me\report.docx"]);

        // Created: watch event -> background stat -> searchable.
        fs.add_file(me, "new-report.md", 1);
        fs.emit(
            Path::new(r"C:\"),
            &WatchEvent::Changed(vec![me.join("new-report.md")]),
        );
        wait("new file appears", || found(&svc, "new-report").len() == 1);

        // Deleted: no longer found.
        fs.remove(me, "report.docx");
        fs.emit(
            Path::new(r"C:\"),
            &WatchEvent::Changed(vec![me.join("report.docx")]),
        );
        wait("deleted file gone", || {
            !found(&svc, "report")
                .iter()
                .any(|p| p.ends_with("report.docx"))
        });

        // Changes inside the recycle bin are ignored.
        fs.emit(
            Path::new(r"C:\"),
            &WatchEvent::Changed(vec![PathBuf::from(r"C:\$Recycle.Bin\S-1\$R1.docx")]),
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(found(&svc, "$R1").is_empty());
    }

    #[test]
    fn serves_the_cached_index_while_rebuilding() {
        let dir = std::env::temp_dir().join(format!("arke-index-test-{}", std::process::id()));
        let cache = dir.join("index.bin");
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\old",
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        {
            let svc = IndexService::start(fs, opts(Some(cache.clone())), || {});
            wait("first build", || ready(&svc));
        }
        assert!(cache.exists());

        // Second start: disk changed and the walk hangs, so the saved index answers with old data.
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\new",
            vec![Entry::new("new-report.txt", EntryKind::File, 1, None, 0)],
        );
        fs.hang(r"C:\");
        let svc = IndexService::start(fs.clone(), opts(Some(cache)), || {});
        wait("cache loaded", || svc.status().count.is_some());
        assert!(svc.status().scanning.is_some(), "rebuilding meanwhile");
        assert_eq!(found(&svc, "report"), [r"C:\old\old-report.txt"]);

        // Rebuild done: swapped for the new one.
        fs.release_all();
        wait("rebuild done", || ready(&svc));
        assert_eq!(found(&svc, "report"), [r"C:\new\new-report.txt"]);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_cache_is_used_without_rescanning() {
        let dir = std::env::temp_dir().join(format!("arke-index-fresh-{}", std::process::id()));
        let cache = dir.join("index.bin");
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\old",
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        {
            let svc = IndexService::start(fs, opts(Some(cache.clone())), || {});
            wait("first build", || ready(&svc));
        }
        // Saved index still fresh: no rewalk (stale contents until a watch event or periodic
        // rebuild).
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\new",
            vec![Entry::new("new-report.txt", EntryKind::File, 1, None, 0)],
        );
        let mut options = opts(Some(cache.clone()));
        options.refresh_after = Duration::from_hours(24);
        let svc = IndexService::start(fs.clone(), options, || {});
        wait("cache loaded", || ready(&svc));
        assert_eq!(found(&svc, "report"), [r"C:\old\old-report.txt"]);
        assert_eq!(fs.read_dir_calls(), 0, "no walk");
        // Manual rebuild: the mapped file can't be overwritten, so write the alternate, switch,
        // delete the old.
        svc.rebuild();
        wait("rebuild done", || {
            found(&svc, "report") == [r"C:\new\new-report.txt"]
        });
        wait("switched to alt file", || {
            dir.join("index.alt.bin").exists() && !cache.exists()
        });
        drop(svc);
        // Next start loads the newer file.
        let mut options = opts(Some(cache));
        options.refresh_after = Duration::from_hours(24);
        let svc = IndexService::start(FakeFs::new(), options, || {});
        wait("new cache loaded", || ready(&svc));
        assert_eq!(found(&svc, "report"), [r"C:\new\new-report.txt"]);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn journal_opts(cache: &Path, journal: &Arc<FakeJournal>) -> IndexOptions {
        let mut o = opts(Some(cache.to_path_buf()));
        // With a journal the saved index's age doesn't matter.
        o.refresh_after = Duration::ZERO;
        o.journal = Some(journal.clone());
        o
    }

    fn temp_cache(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("arke-journal-{tag}-{}", std::process::id()));
        let cache = dir.join("index.bin");
        (dir, cache)
    }

    #[test]
    fn journal_catches_up_on_changes_made_while_closed() {
        let (dir, cache) = temp_cache("catchup");
        let me = Path::new(r"C:\Users\me");
        let fs = FakeFs::new();
        fs.insert_dir(
            me,
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let journal = FakeJournal::new(7);
        {
            let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
            wait("first build", || ready(&svc));
            wait("journal caught up", || svc.status().live);
            assert_eq!(fs.watcher_count(Path::new(r"C:\")), 0, "journal, no watch");
        }
        // While Arke is closed: add one, delete one, move a folder in.
        fs.add_file(me, "new-report.md", 1);
        journal.push(me.join("new-report.md"), false);
        fs.remove(me, "report.docx");
        journal.push(me.join("report.docx"), false);
        fs.insert_dir(
            me.join("moved"),
            vec![Entry::new("inner-report.txt", EntryKind::File, 1, None, 0)],
        );
        journal.push(me.join("moved"), true);

        let calls = fs.read_dir_calls();
        let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
        wait("offline changes replayed", || {
            let f = found(&svc, "report");
            f.iter().any(|p| p.ends_with("new-report.md"))
                && f.iter().any(|p| p.ends_with("inner-report.txt"))
                && !f.iter().any(|p| p.ends_with("report.docx"))
        });
        assert!(svc.status().live);
        assert_eq!(
            fs.read_dir_calls() - calls,
            1,
            "only the moved-in folder was listed, no rewalk"
        );

        // While running: the journal delivers live.
        fs.add_file(me, "live-report.txt", 1);
        journal.push(me.join("live-report.txt"), false);
        wait("live change", || found(&svc, "live-report").len() == 1);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_sizes_come_from_the_index_only_when_journaled_and_live() {
        let (dir, cache) = temp_cache("sizes");
        let me = Path::new(r"C:\Users\me");
        let fs = FakeFs::new();
        fs.insert_dir(
            me.join("docs"),
            vec![Entry::new("a.txt", EntryKind::File, 10, None, 0)],
        );
        let cancel = AtomicBool::new(false);
        let sizes = |svc: &IndexService| {
            svc.child_dir_sizes(me, &cancel)
                .map(|v| v.into_iter().map(|(n, s)| (n, s.bytes)).collect::<Vec<_>>())
        };
        {
            // Watching only: changes made while closed may be missing, so walk instead.
            let svc = IndexService::start(fs.clone(), opts(None), || {});
            wait("built", || ready(&svc));
            assert_eq!(sizes(&svc), None);
        }
        let journal = FakeJournal::new(3);
        let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
        wait("journal caught up", || ready(&svc) && svc.status().live);
        assert_eq!(sizes(&svc), Some(vec![("docs".to_string(), 10)]));
        // A live change shows up without a walk.
        fs.add_file(&me.join("docs"), "b.txt", 5);
        journal.push(me.join("docs").join("b.txt"), false);
        let calls = fs.read_dir_calls();
        wait("live change", || {
            sizes(&svc) == Some(vec![("docs".to_string(), 15)])
        });
        assert_eq!(fs.read_dir_calls(), calls, "answered from the index");
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Checkpoints are saved periodically while running. If the journal overwrites the build-time
    /// position while closed, the next start resumes from the checkpoint instead of rebuilding,
    /// and earlier recorded changes are kept.
    #[test]
    fn checkpoints_avoid_rebuilding_after_the_journal_wraps() {
        let (dir, cache) = temp_cache("checkpoint");
        let fs = FakeFs::new();
        let a = Path::new(r"C:\a");
        fs.insert_dir(
            a,
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        let journal = FakeJournal::new(1);
        {
            let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
            wait("first build", || ready(&svc) && svc.status().live);
            fs.add_file(a, "new-report.txt", 1);
            journal.push(a.join("new-report.txt"), false);
            wait("followed new file", || found(&svc, "new-report").len() == 1);
            wait("checkpoint saved", || dir.join("index.checkpoint").exists());
            // Wait for it to write the latest position.
            std::thread::sleep(Duration::from_millis(200));
        }
        // While closed: the journal wraps and the build-time position is gone.
        journal.forget_all();
        let walked = fs.read_dir_calls();
        let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
        wait("cache loaded, journal resumed", || {
            ready(&svc) && svc.status().live
        });
        assert_eq!(fs.read_dir_calls(), walked, "no rewalk");
        assert_eq!(
            found(&svc, "new-report"),
            [r"C:\a\new-report.txt"],
            "checkpoint changes survive"
        );
        // Later changes are followed as usual.
        fs.add_file(a, "later-report.txt", 1);
        journal.push(a.join("later-report.txt"), false);
        wait("later changes", || found(&svc, "later-report").len() == 1);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_gaps_trigger_a_rebuild() {
        let (dir, cache) = temp_cache("gap");
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\a",
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        let journal = FakeJournal::new(1);
        {
            let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
            wait("first build", || ready(&svc));
        }
        // Journal recreated while closed (new ID): saved position doesn't connect, so rebuild.
        journal.recreate(2);
        fs.add_file(Path::new(r"C:\a"), "unlogged-report.txt", 1);
        let svc = IndexService::start(fs.clone(), journal_opts(&cache, &journal), || {});
        wait("found after rebuild", || found(&svc, "unlogged").len() == 1);
        wait("following new journal", || svc.status().live);

        // Journal cleared while running: rebuild too.
        let calls = journal.follow_calls();
        fs.add_file(Path::new(r"C:\a"), "second-report.txt", 1);
        journal.recreate(3);
        wait("second rebuild", || found(&svc, "second-report").len() == 1);
        wait("journal refollowed", || journal.follow_calls() > calls);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn narrowing_only_follows_plain_extensions() {
        assert!(refines("car", "carg"));
        assert!(refines("car ", "car go"));
        assert!(!refines("carg", "car"));
        assert!(!refines("", "a"));
        assert!(!can_store("ca"), "too short");
        assert!(!can_store("cargo t"), "last term too short");
        assert!(can_store("cargo toml"));
        assert!(can_narrow("cargo t"), "can search within cargo results");
        assert!(!can_narrow("ext:rs"));
        assert!(!can_narrow(r"src\main"));
        assert!(!can_narrow("!test"));
    }
}
