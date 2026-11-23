//! Background service for global search: load cache, rebuild in the background, watch recursively.
//!
//! - **Startup**: load the saved index first (a few hundred ms) so queries work right away. If it
//!   is fresh enough ([`IndexOptions::refresh_after`]) we skip the walk and rely on watching; if
//!   too old we rebuild in the background and swap it in. Also rechecked periodically.
//! - **Changes**: one recursive watch per root. Changed paths accumulate briefly, then get `stat`ed
//!   in the background (attributes only, no open, so cloud placeholders aren't downloaded) and
//!   recorded in the [`Overlay`].
//! - **Merge**: when the overlay grows too large, it is folded into a new saved index
//!   ([`Index::merged`]) without walking again.
//! - **Rebuild**: when too many changes arrive at once or watch events are lost (buffer overflow);
//!   each one logs its [`Rebuild`] reason. Records already reflected in the new index are dropped
//!   afterwards.
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
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
#[cfg(not(test))]
const OVERLAY_MAX: usize = 100_000;
/// Removal records looked over again once this many more have piled up ([`Shared::drop_needless_removals`]).
#[cfg(not(test))]
const NEEDLESS_REMOVALS_EVERY: usize = 4096;
#[cfg(test)]
const OVERLAY_MAX: usize = 20;
#[cfg(test)]
const NEEDLESS_REMOVALS_EVERY: usize = 4;
/// List at most this many items in a moved-in folder; more means rebuild.
#[cfg(not(test))]
const MOVED_IN_MAX: usize = 50_000;
#[cfg(test)]
const MOVED_IN_MAX: usize = 10;
/// After lost watch events, minimum time since the last rebuild before rebuilding again (busy
/// system drives overflow often, and a rebuild takes tens of seconds). Sooner than that, the
/// rebuild is scheduled for when it's up.
#[cfg(not(test))]
const RESCAN_MIN_GAP: Duration = Duration::from_hours(1);
// Above the index's build time resolution (whole seconds), so "soon after" is really soon.
#[cfg(test)]
const RESCAN_MIN_GAP: Duration = Duration::from_secs(2);
/// After a journal gap, how long to wait before rebuilding: when the journal was deleted, the
/// system drive gets a new one within seconds, and with it the rebuild can read the MFT instead
/// of walking every folder.
#[cfg(not(test))]
const GAP_FIRST_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const GAP_FIRST_DELAY: Duration = Duration::from_millis(10);
/// Gaps coming back within this long count as repeats (something keeps deleting the journal, such
/// as a cleaner): each repeat doubles the wait before the next rebuild, from [`GAP_BACKOFF`] up
/// to [`GAP_BACKOFF_MAX`].
#[cfg(not(test))]
const GAP_REPEAT_WINDOW: Duration = Duration::from_hours(1);
#[cfg(test)]
const GAP_REPEAT_WINDOW: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const GAP_BACKOFF: Duration = Duration::from_mins(2);
#[cfg(test)]
const GAP_BACKOFF: Duration = Duration::from_millis(300);
#[cfg(not(test))]
const GAP_BACKOFF_MAX: Duration = Duration::from_mins(30);
#[cfg(test)]
const GAP_BACKOFF_MAX: Duration = Duration::from_millis(1200);
/// After the index service went away, when to try following again (it restarts within seconds
/// when updated); about a minute in all before treating it as a journal gap.
#[cfg(not(test))]
const REFOLLOW_WAITS: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
];
#[cfg(test)]
const REFOLLOW_WAITS: &[Duration] = &[Duration::from_millis(10), Duration::from_millis(20)];
/// How often to check whether the index is stale while running.
const REFRESH_CHECK: Duration = Duration::from_mins(10);
/// A root that couldn't be listed (offline share) is tried again after this.
#[cfg(not(test))]
const ROOT_RETRY: Duration = Duration::from_mins(10);
#[cfg(test)]
const ROOT_RETRY: Duration = Duration::from_millis(50);
/// With [`IndexOptions::incremental`], a full walk at least this often: a refresh doesn't see
/// files changed in place, or attribute changes, in folders it takes over.
const FULL_WALK_EVERY: Duration = Duration::from_hours(24 * 7);
/// How often to consider saving a checkpoint with a journal (latest position + changes since the
/// build); [`checkpoint_due`] decides whether one is written.
#[cfg(not(test))]
const CHECKPOINT_EVERY: Duration = Duration::from_mins(2);
#[cfg(test)]
const CHECKPOINT_EVERY: Duration = Duration::from_millis(40);
/// When checkpoints are written (see [`CheckpointPolicy::due`]).
const CHECKPOINTS: CheckpointPolicy = if cfg!(test) {
    CheckpointPolicy {
        records: 1_000,
        max_age: Duration::from_millis(400),
        position_age: Duration::from_millis(1200),
    }
} else {
    CheckpointPolicy::DEFAULT
};
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

/// Wait before rebuilding after a journal gap that came back `repeats` times lately.
fn gap_wait(repeats: usize) -> Duration {
    if repeats == 0 {
        return GAP_FIRST_DELAY;
    }
    let doublings = u32::try_from(repeats - 1).unwrap_or(u32::MAX).min(16);
    GAP_BACKOFF
        .saturating_mul(1 << doublings)
        .min(GAP_BACKOFF_MAX)
}

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
    /// Stop walking past this many items (network locations); `None` for no limit.
    pub max_items: Option<usize>,
    /// Rewalks list again only the folders that may have changed since the last one (see
    /// [`Index::refresh`]); network locations, where every listing is a round trip.
    pub incremental: bool,
}

impl IndexOptions {
    fn rules(&self) -> BuildRules {
        BuildRules {
            skip_build_dirs: self.skip_build_dirs,
            excluded: self.excluded.clone(),
            max_items: self.max_items,
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

/// Why a whole root is walked (or listed from the MFT) again; logged with every rebuild so a
/// report shows what keeps triggering them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rebuild {
    /// No saved index (first run, or the file is gone).
    NoIndex,
    /// The saved index can't be read (damaged, older format).
    Unreadable,
    /// The saved index was built with other roots, build-folder or exclusion settings.
    SettingsChanged,
    /// Older than [`IndexOptions::refresh_after`] (volumes without the journal miss changes
    /// made while Arke was closed).
    Stale,
    /// The saved journal position doesn't connect to the journal on disk (recreated, wrapped).
    JournalMismatch,
    /// The journal lost records while following.
    JournalGap,
    /// The folder watcher lost events (its buffer overflowed).
    WatchOverflow,
    /// This many changes recorded since the build.
    OverlayFull(usize),
    /// This many changes arrived at once (e.g. replaying a long absence).
    TooManyChanges(usize),
    /// A folder moved in holds more than this many items.
    LargeMoveIn(usize),
    /// Asked for ("Rebuild" in settings).
    Requested,
    /// Started again with a saved index of a location without a journal: catch up on changes
    /// made meanwhile (an incremental rewalk, see [`IndexOptions::incremental`]).
    Resumed,
}

impl std::fmt::Display for Rebuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::NoIndex => f.write_str("no saved index"),
            Self::Unreadable => f.write_str("saved index unreadable"),
            Self::SettingsChanged => f.write_str("index settings changed"),
            Self::Stale => f.write_str("saved index too old"),
            Self::JournalMismatch => f.write_str("journal doesn't match the saved index"),
            Self::JournalGap => f.write_str("journal gap"),
            Self::WatchOverflow => f.write_str("folder watch lost events"),
            Self::OverlayFull(n) => write!(f, "{n} changes since the last build"),
            Self::TooManyChanges(n) => write!(f, "{n} changes at once"),
            Self::LargeMoveIn(n) => write!(f, "moved-in folder with over {n} items"),
            Self::Requested => f.write_str("requested"),
            Self::Resumed => f.write_str("catching up on changes made while closed"),
        }
    }
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
    /// When the last checkpoint was written in this run and the overlay sequence it covered.
    last_checkpoint: Mutex<Option<(Instant, u64)>>,
    /// Checkpoints written and their total bytes (to keep SSD writes in check; tests read them).
    checkpoint_writes: std::sync::atomic::AtomicU64,
    checkpoint_bytes: std::sync::atomic::AtomicU64,
    /// Why there is no usable saved index (set by `load_cache`).
    no_index: Mutex<Rebuild>,
    /// Reason of a rebuild waiting for resources ([`IndexOptions::can_build`]).
    deferred_reason: Mutex<Option<Rebuild>>,
    /// A rebuild asked for while one was running, or held back on purpose (journal gaps coming
    /// back, lost watch events soon after a rebuild): started by the periodic check once due.
    scheduled: Mutex<Option<(Rebuild, Instant)>>,
    /// When rebuilds for journal gaps started lately (to space repeats out).
    gap_rebuilds: Mutex<Vec<Instant>>,
    /// Reason of the last rebuild started.
    last_rebuild: Mutex<Option<Rebuild>>,
    /// Folding the overlay into the index ([`Shared::merge`]): not a walk, no progress to show.
    merging: AtomicBool,
    /// Merges done so far.
    merges: std::sync::atomic::AtomicU64,
    /// Removal records left after [`Shared::drop_needless_removals`] last looked them over.
    removals_looked_at: AtomicUsize,
    /// Rebuilds (walks or MFT listings) started so far.
    rebuilds: std::sync::atomic::AtomicU64,
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
    /// The folder watcher works: attached to every watched root, and no events lost since.
    watch_alive: AtomicBool,
    /// Bumped each time the watcher is attached to every watched root.
    watch_epoch: std::sync::atomic::AtomicU64,
    /// `watch_epoch` when the current index's walk started with the watcher working (0: the
    /// index wasn't walked while watched in this run, so changes since may be missing).
    fresh_epoch: std::sync::atomic::AtomicU64,
    /// When the last full walk (not a refresh) of these roots ended, Unix seconds.
    last_full_walk: Mutex<Option<u64>>,
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
    #[allow(clippy::too_many_lines, reason = "the startup steps, in order")]
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
            last_checkpoint: Mutex::new(None),
            checkpoint_writes: std::sync::atomic::AtomicU64::new(0),
            checkpoint_bytes: std::sync::atomic::AtomicU64::new(0),
            no_index: Mutex::new(Rebuild::NoIndex),
            deferred_reason: Mutex::new(None),
            scheduled: Mutex::new(None),
            gap_rebuilds: Mutex::new(Vec::new()),
            last_rebuild: Mutex::new(None),
            merging: AtomicBool::new(false),
            merges: std::sync::atomic::AtomicU64::new(0),
            removals_looked_at: AtomicUsize::new(0),
            rebuilds: std::sync::atomic::AtomicU64::new(0),
            cargo_targets: Mutex::new(std::collections::HashMap::new()),
            pending: Mutex::new(Vec::new()),
            draining: AtomicBool::new(false),
            watchers: Mutex::new(Vec::new()),
            journaled: Mutex::new(Vec::new()),
            followers: Mutex::new(Vec::new()),
            live: std::sync::atomic::AtomicUsize::new(0),
            caught_up: Mutex::new(Vec::new()),
            narrow: Mutex::new(None),
            watch_alive: AtomicBool::new(false),
            watch_epoch: std::sync::atomic::AtomicU64::new(0),
            fresh_epoch: std::sync::atomic::AtomicU64::new(0),
            last_full_walk: Mutex::new(None),
            on_change: Box::new(on_change),
        });
        let weak = Arc::downgrade(&shared);
        spawn(
            "index-start",
            &Arc::clone(&shared.opts.thread_init),
            move || {
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
                    Start::Resume(_) => shared.build(Rebuild::Stale),
                    Start::Build(reason) => shared.build(reason),
                    Start::NoJournal if shared.is_stale() => shared.build(shared.stale_reason()),
                    // Cheap to rewalk (only changed folders): catch up on what changed while Arke
                    // was closed, so the index is current again.
                    Start::NoJournal
                        if shared.opts.incremental && shared.index.read().is_some() =>
                    {
                        shared.build(Rebuild::Resumed);
                    }
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
                        let reason = shared.deferred_reason.lock().unwrap_or(Rebuild::Stale);
                        shared.request_build(reason);
                    }
                    shared.start_scheduled();
                    if since_check >= REFRESH_CHECK {
                        since_check = Duration::ZERO;
                        // No periodic rebuild needed if every root is journaled.
                        if shared.has_unjournaled() && shared.is_stale() {
                            shared.request_build(shared.stale_reason());
                        }
                    }
                }
            },
        );
        Self { shared }
    }

    /// Current status for display; cheap, fine on the UI thread.
    pub fn status(&self) -> IndexStatus {
        let count = self.shared.index.read().as_ref().map(|i| i.len());
        let scanning = (self.shared.building.load(Ordering::Relaxed)
            && !self.shared.merging.load(Ordering::Relaxed))
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
        self.search_until(input, clock, limit, show_hidden, &AtomicBool::new(false))
            .unwrap_or_default()
    }

    /// [`IndexService::search`] that gives up once `cancel` is set (the user typed on and a newer
    /// query replaced this one): `None`, and the scan stops within one chunk.
    pub fn search_until(
        &self,
        input: &str,
        clock: Clock,
        limit: usize,
        show_hidden: bool,
        cancel: &AtomicBool,
    ) -> Option<Vec<IndexHit>> {
        // Before the first build ends: what it has walked so far.
        let index = self.shared.index.read().clone();
        let Some(index) = index.or_else(|| self.shared.progress.partial.lock().clone()) else {
            return Some(Vec::new());
        };
        let query = Query::parse(input, clock);
        let narrowable = can_narrow(input);
        let within = if narrowable {
            self.shared.narrow.lock().as_ref().and_then(|n| {
                (Weak::ptr_eq(&n.index, &Arc::downgrade(&index))
                    && n.show_hidden == show_hidden
                    && refines(&n.input, input))
                .then(|| Arc::clone(&n.ids))
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
            cancel: Some(cancel),
        });
        if result.cancelled {
            return None;
        }
        if let Some(ids) = result.matched {
            *self.shared.narrow.lock() = Some(Narrow {
                index: Arc::downgrade(&index),
                input: input.trim().to_string(),
                show_hidden,
                ids: Arc::new(ids),
            });
        }
        Some(
            self.shared
                .overlay
                .lock()
                .apply(result.hits, &query, limit, show_hidden),
        )
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

    /// The index snapshot and overlay for `dir`, when the index follows its volume through the
    /// change journal and is caught up (complete and current). `None` otherwise.
    fn live_index_for(&self, dir: &Path) -> Option<(Arc<Index>, Overlay)> {
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
        Some((index, shared.overlay.lock().under(dir)))
    }

    /// Everything below `dir` as [`crate::dirsize::usage`] would walk it (totals, categories,
    /// space on disk for `cluster`-byte clusters, and the size of each folder directly inside),
    /// answered from the index in one pass when it covers `dir` live, as
    /// [`Self::child_dir_sizes`] does. Folders the index has no contents for are walked and
    /// added, other people's profiles too (`profiles`: the service lists them without contents).
    /// `None`: not covered, inside one of those profiles, or cancelled; walk instead. Background
    /// threads only.
    pub fn usage_below(
        &self,
        dir: &Path,
        cluster: u64,
        profiles: Option<&crate::dirsize::Profiles>,
        cancel: &AtomicBool,
    ) -> Option<crate::dirsize::Space> {
        if profiles.is_some_and(|p| p.hidden(dir)) {
            return None;
        }
        let (index, overlay) = self.live_index_for(dir)?;
        let (direct, children) = index.usage_below(dir, &overlay, cluster, cancel)?;
        let mut space = crate::dirsize::Space {
            usage: direct,
            children: Vec::with_capacity(children.len()),
        };
        for child in children {
            let mut usage = child.usage;
            for path in &child.unindexed {
                let walked = crate::dirsize::usage(
                    self.shared.fs.as_ref(),
                    path,
                    cluster,
                    cancel,
                    &mut |_| {},
                );
                match walked {
                    // The folder itself is counted already, with the index's folders.
                    Some(more) => usage += more.usage,
                    None if cancel.load(Ordering::Relaxed) => return None,
                    None => usage.partial = true,
                }
            }
            space.usage += usage;
            space.children.push((dir.join(&child.name), usage.bytes));
        }
        if let Some(profiles) = profiles
            && let Some(way) = profiles.way_down(dir)
        {
            self.add_hidden_profiles(&index, profiles, &way, cluster, cancel, &mut space)?;
        }
        Some(space)
    }

    /// Walks the profile folders below a count (`way`: from it down to the profiles folder) that
    /// the index holds nothing in, as the service lists other people's: what can be read counts
    /// (`Default`), the rest makes it partial, as a walk would. Only for a count of the profiles
    /// folder or above it (a drive), so others pay nothing. `None` when cancelled.
    fn add_hidden_profiles(
        &self,
        index: &Index,
        profiles: &crate::dirsize::Profiles,
        way: &[String],
        cluster: u64,
        cancel: &AtomicBool,
        space: &mut crate::dirsize::Space,
    ) -> Option<()> {
        let fs = self.shared.fs.as_ref();
        let Ok(listed) = fs.read_dir(&profiles.dir) else {
            return Some(());
        };
        let hidden: Vec<PathBuf> = listed
            .filter_map(Result::ok)
            .filter(|e| e.kind == crate::entry::EntryKind::Dir && profiles.hides(&e.name))
            .map(|e| profiles.dir.join(&*e.name))
            .collect();
        if hidden.is_empty() {
            return Some(());
        }
        // In the index with nothing inside: listed without contents. (A folder that is really
        // empty walks to nothing; one built by a walk has its contents and is left alone.)
        for (path, inside) in index.children_of(&hidden, cancel) {
            if !inside.is_empty() {
                continue;
            }
            let walked = crate::dirsize::usage(fs, &path, cluster, cancel, &mut |_| {})?;
            let child = way
                .first()
                .cloned()
                .or_else(|| path.file_name().map(|n| n.to_string_lossy().to_lowercase()));
            if let Some(child) = child
                && let Some(slot) = space.children.iter_mut().find(|(p, _)| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().to_lowercase() == child)
                })
            {
                slot.1 += walked.usage.bytes;
            }
            space.usage += walked.usage;
        }
        (!cancel.load(Ordering::Relaxed)).then_some(())
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

    /// Every item below `dir` this index has, with the changes seen since its build (see
    /// [`Index::items_under`]): a search below `dir` can take these instead of walking it. `None`
    /// when the index can't stand in for a walk: not known to be current (only the saved index
    /// so far, the watcher broke or lost events, a rewalk to catch up still running), cut off at
    /// the item limit, no index yet, `dir` not covered, or cancelled.
    pub fn items_under(
        &self,
        dir: &Path,
        show_hidden: bool,
        cancel: &AtomicBool,
    ) -> Option<crate::index::IndexedTree> {
        let shared = &self.shared;
        if shared.opts.passive || !shared.is_current() {
            return None;
        }
        let index = shared.index.read().clone()?;
        // Folders past the limit were never listed: they'd look empty.
        if shared.rules.max_items.is_some_and(|max| index.len() >= max) {
            return None;
        }
        let overlay = shared.overlay.lock().under(dir);
        index.items_under(dir, &overlay, show_hidden, cancel)
    }

    /// Rewalks (no-op if a build is already running).
    pub fn rebuild(&self) {
        self.shared.request_build(Rebuild::Requested);
    }

    /// Times the recorded changes were folded into the index instead of a rebuild.
    pub fn merges(&self) -> u64 {
        self.shared.merges.load(Ordering::Relaxed)
    }

    /// Rebuilds (walks or MFT listings, not merges) started so far.
    pub fn rebuilds(&self) -> u64 {
        self.shared.rebuilds.load(Ordering::Relaxed)
    }

    /// Why the last rebuild started; `None` if none ran (the saved index was used as is).
    pub fn last_rebuild(&self) -> Option<Rebuild> {
        *self.shared.last_rebuild.lock()
    }

    /// Checkpoints written by this service so far, and their total size in bytes.
    pub fn checkpoint_writes(&self) -> (u64, u64) {
        (
            self.shared.checkpoint_writes.load(Ordering::Relaxed),
            self.shared.checkpoint_bytes.load(Ordering::Relaxed),
        )
    }

    /// The last checkpoint holds every recorded change and the latest journal position.
    #[cfg(test)]
    fn checkpoint_is_current(&self) -> bool {
        let shared = &self.shared;
        let seq = shared.overlay.lock().seq();
        shared.last_checkpoint.lock().is_some_and(|(_, s)| s == seq)
            && *shared.checkpointed.lock() == *shared.latest.lock()
    }
}

/// When to write a checkpoint.
///
/// Every checkpoint rewrites all changes since the build (a few MB on a busy disk), so they are
/// spaced out. A restart replays the journal from the saved position anyway, so an older one
/// only means a few more records to replay, never lost changes; only if the journal wrapped past
/// it would a restart rebuild, and a journal holds far more than `max_age` of changes.
struct CheckpointPolicy {
    /// Write once this many changes piled up since the last one...
    records: u64,
    /// ...or once the last one is this old and anything changed.
    max_age: Duration,
    /// Only the journal position moved (changes outside the index, like temp files): keep it
    /// fresh enough that a restart still finds it in the journal.
    position_age: Duration,
}

impl CheckpointPolicy {
    const DEFAULT: Self = Self {
        records: 20_000,
        max_age: Duration::from_mins(10),
        position_age: Duration::from_mins(30),
    };

    /// Whether to write one now. `changes`: records since the last one; `moved`: the journal
    /// position moved; `age`: time since the last one this run (`None`: none yet, so the
    /// position on disk may be from an earlier run and is refreshed right away).
    const fn due(&self, changes: u64, moved: bool, age: Option<Duration>) -> bool {
        let Some(age) = age else {
            return changes > 0 || moved;
        };
        if changes > 0 {
            changes >= self.records || age.as_nanos() >= self.max_age.as_nanos()
        } else {
            moved && age.as_nanos() >= self.position_age.as_nanos()
        }
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
    Build(Rebuild),
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

    /// Reason for a rebuild because [`Shared::is_stale`]: too old, or no usable index at all.
    fn stale_reason(&self) -> Rebuild {
        if self.index.read().is_some() {
            Rebuild::Stale
        } else {
            *self.no_index.lock()
        }
    }

    /// Loads the saved index: picks the newer of `cache` and its alternate (see [`alt_path`]),
    /// mapped directly rather than copied into memory. The other one is deleted.
    fn load_cache(&self) {
        let Some(path) = &self.opts.cache else {
            return;
        };
        let mut best: Option<(Index, PathBuf)> = None;
        let mut missing = Rebuild::NoIndex;
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
                Ok(_) => missing = Rebuild::SettingsChanged,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    if missing == Rebuild::NoIndex {
                        missing = Rebuild::Unreadable;
                    }
                    log::info!(
                        "global search index unusable, rebuilding {}: {err}",
                        candidate.display()
                    );
                }
            }
        }
        let Some((index, from)) = best else {
            *self.no_index.lock() = missing;
            return;
        };
        log::info!("loaded index: {} items ({})", index.len(), from.display());
        for other in [path.clone(), alt_path(path)] {
            if other != from {
                let _ = std::fs::remove_file(&other);
            }
        }
        *self.cache_file.lock() = Some(from);
        *self.last_full_walk.lock() = std::fs::read_to_string(walked_path(path))
            .ok()
            .and_then(|s| s.trim().parse().ok());
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
            return Start::Build(*self.no_index.lock());
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
                        "global search: journal for {} doesn't match the index",
                        self.opts.roots[i].display()
                    );
                    return Start::Build(Rebuild::JournalMismatch);
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
            match self.follow_root(journal.as_ref(), i, cursor) {
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

    /// Follows the journal of root `i` from `cursor`; its events go to [`Self::on_journal`].
    fn follow_root(
        self: &Arc<Self>,
        journal: &dyn Journal,
        i: usize,
        cursor: Cursor,
    ) -> std::io::Result<Box<dyn Send>> {
        let root = self.opts.roots.get(i).cloned().unwrap_or_default();
        let weak = Arc::downgrade(self);
        let caught_up = AtomicBool::new(false);
        journal.follow(
            &root,
            cursor,
            Box::new(move |event| {
                if let Some(shared) = weak.upgrade() {
                    shared.on_journal(i, event, &caught_up);
                }
            }),
        )
    }

    /// The index service went away (stopped, or being updated): once it's back, root `i` is
    /// followed again from its last position, which misses nothing, instead of rebuilding the
    /// whole index (#343). Only if it doesn't come back is it a journal gap. A rebuild that
    /// starts meanwhile follows every root itself.
    fn follow_again(self: &Arc<Self>, i: usize) {
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("arke-index-refollow".into())
            .spawn(move || {
                for wait in REFOLLOW_WAITS {
                    std::thread::sleep(*wait);
                    let Some(shared) = weak.upgrade() else { return };
                    if shared.stopped.load(Ordering::Relaxed)
                        || shared.building.load(Ordering::Acquire)
                    {
                        return;
                    }
                    let (Some(journal), Some(cursor)) = (
                        shared.opts.journal.clone(),
                        shared.latest.lock().get(i).copied().flatten(),
                    ) else {
                        break;
                    };
                    match shared.follow_root(journal.as_ref(), i, cursor) {
                        Ok(follower) => {
                            shared.followers.lock().push(follower);
                            return;
                        }
                        Err(err) => log::info!("global search: index service not back yet: {err}"),
                    }
                }
                if let Some(shared) = weak.upgrade() {
                    shared.journal_gap();
                }
            });
        if spawned.is_err() {
            self.journal_gap();
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
                log::info!(
                    "global search: journal gap on {}",
                    self.opts
                        .roots
                        .get(root)
                        .map_or_default(|r| r.display().to_string())
                );
                self.journal_gap();
            }
            JournalEvent::Disconnected => {
                if let Some(slot) = self.caught_up.lock().get_mut(root) {
                    *slot = false;
                }
                if caught_up.swap(false, Ordering::Relaxed) {
                    self.live.fetch_sub(1, Ordering::Relaxed);
                }
                log::info!(
                    "global search: index service gone while following {}, following again once it's back",
                    self.opts
                        .roots
                        .get(root)
                        .map_or_default(|r| r.display().to_string())
                );
                self.follow_again(root);
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
        if cursors.iter().all(Option::is_none) {
            return;
        }
        let moved = cursors != *self.checkpointed.lock();
        let last = *self.last_checkpoint.lock();
        let changes = self
            .overlay
            .lock()
            .seq()
            .saturating_sub(last.map_or(0, |(_, seq)| seq));
        if !CHECKPOINTS.due(changes, moved, last.map(|(at, _)| at.elapsed())) {
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
        let seq = {
            let overlay = self.overlay.lock();
            if overlay.write_to(&mut bytes).is_err() {
                return;
            }
            overlay.seq()
        };
        let path = checkpoint_path(cache);
        let tmp = path.with_extension("checkpoint.tmp");
        let written = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path));
        match written {
            Ok(()) => {
                *self.checkpointed.lock() = cursors;
                *self.last_checkpoint.lock() = Some((Instant::now(), seq));
                self.checkpoint_writes.fetch_add(1, Ordering::Relaxed);
                self.checkpoint_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
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
        let mut all = true;
        for root in which.iter().filter_map(|&i| self.opts.roots.get(i)) {
            let weak = Weak::clone(weak);
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
                Err(err) => {
                    all = false;
                    log::warn!("cannot watch {}: {err}", root.display());
                }
            }
        }
        if all {
            self.watch_epoch.fetch_add(1, Ordering::AcqRel);
        }
        self.watch_alive.store(all, Ordering::Release);
    }

    /// Whether the index is known to hold what's on disk now: every journaled root is caught
    /// up, and the others were walked in this run while the folder watcher was working, which
    /// it still is. Not so for a saved index of a share loaded at startup (changes made while
    /// Arke was closed aren't in it), or once the watcher failed or lost events (until a walk
    /// after watching again).
    fn is_current(&self) -> bool {
        let journaled = self.journaled.lock().clone();
        let caught_up = self.caught_up.lock().clone();
        let watched =
            (0..self.opts.roots.len()).any(|i| !journaled.get(i).copied().unwrap_or(false));
        let journals_ok = (0..self.opts.roots.len()).all(|i| {
            !journaled.get(i).copied().unwrap_or(false)
                || caught_up.get(i).copied().unwrap_or(false)
        });
        let epoch = self.watch_epoch.load(Ordering::Acquire);
        journals_ok
            && (!watched
                || (self.watch_alive.load(Ordering::Acquire)
                    && epoch != 0
                    && self.fresh_epoch.load(Ordering::Acquire) == epoch))
    }

    fn on_event(self: &Arc<Self>, event: WatchEvent) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        if matches!(event, WatchEvent::Rescan) {
            // Events lost, or the watch broke (a share went away): the index can't be trusted to
            // be current until a walk after watching again.
            self.watch_alive.store(false, Ordering::Release);
        }
        match event {
            WatchEvent::Changed(paths) => self.enqueue(paths.into_iter().map(|p| (p, false))),
            WatchEvent::Rescan => match self.age() {
                Some(age) if age < RESCAN_MIN_GAP => {
                    let wait = RESCAN_MIN_GAP.saturating_sub(age);
                    log::info!(
                        "global search: folder watch lost events, rebuilding in {} s",
                        wait.as_secs()
                    );
                    self.schedule(Rebuild::WatchOverflow, Instant::now() + wait);
                }
                _ => self.request_build(Rebuild::WatchOverflow),
            },
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
            let too_many = (pending.len() > OVERLAY_MAX).then_some(pending.len());
            if too_many.is_some() {
                pending.clear();
            }
            too_many
        };
        if let Some(count) = too_many {
            self.request_build(Rebuild::TooManyChanges(count));
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
            // Folders that changed: the old contents of a folder replaced at the same path aren't
            // reported (their records name a parent that's gone), so these are compared with the
            // index once the batch is in.
            let mut dirs: Vec<PathBuf> = Vec::new();
            // What the folders walked just now hold (lowercase names), so they aren't listed again.
            let mut listed: HashMap<PathBuf, HashSet<String>> = HashMap::new();
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
                // Found under another spelling (the case changed, or a short 8.3 name): nothing is
                // left under this one, and the item is recorded under the name it has (#310).
                let path = match self.fs.listed_name(&path) {
                    Some(listed) => {
                        self.overlay.lock().record(path.clone(), None);
                        path.with_file_name(listed)
                    }
                    None => path,
                };
                let walk = moved_in && descends_into(&entry);
                self.overlay
                    .lock()
                    .record(path.clone(), Some(Seen::of(&entry)));
                if descends_into(&entry) {
                    dirs.push(path.clone());
                }
                if walk && !self.record_tree(&path, &mut dirs, &mut listed) {
                    self.request_build(Rebuild::LargeMoveIn(MOVED_IN_MAX));
                }
            }
            self.forget_what_is_gone(&dirs, &listed);
            self.drop_needless_removals();
            let recorded = self.overlay.lock().len();
            if recorded > OVERLAY_MAX {
                self.request_build(Rebuild::OverlayFull(recorded));
            }
        }
    }

    /// Each temporary file made and deleted since the index was built leaves a removal record that
    /// changes nothing, and they pile up until the next merge (tens of thousands, over 10 MB). Once
    /// [`NEEDLESS_REMOVALS_EVERY`] more are in, drops those whose path the index doesn't have, in
    /// one pass over the index. Not while a walk or a merge is making a new index: that one may
    /// still have them.
    fn drop_needless_removals(&self) {
        let count = self.overlay.lock().removed_count();
        let looked_at = self.removals_looked_at.load(Ordering::Relaxed);
        if count < looked_at {
            // A merge or build took records away since.
            self.removals_looked_at.store(count, Ordering::Relaxed);
            return;
        }
        if count < looked_at + NEEDLESS_REMOVALS_EVERY {
            return;
        }
        let state = || {
            (
                self.building.load(Ordering::Acquire) || self.merging.load(Ordering::Acquire),
                self.rebuilds.load(Ordering::Acquire),
                self.merges.load(Ordering::Acquire),
            )
        };
        let before = state();
        if before.0 {
            return;
        }
        let Some(index) = self.index.read().clone() else {
            return;
        };
        let candidates = self.overlay.lock().removed_hiding_nothing_added();
        let mut parents: Vec<PathBuf> = candidates
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        parents.sort_unstable();
        parents.dedup();
        let indexed: HashMap<PathBuf, HashSet<String>> = index
            .children_of(&parents, &self.stopped)
            .into_iter()
            .map(|(dir, names)| {
                (
                    dir,
                    names.into_iter().map(|(n, _)| n.to_lowercase()).collect(),
                )
            })
            .collect();
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        let needless: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|path| {
                let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
                    return false;
                };
                !indexed
                    .get(dir)
                    .is_some_and(|names| names.contains(&name.to_string_lossy().to_lowercase()))
            })
            .collect();
        let mut overlay = self.overlay.lock();
        // A new index came, or is being made, meanwhile: it was looked at for the old one.
        if state() == before {
            overlay.forget_removed(&needless);
        }
        self.removals_looked_at
            .store(overlay.removed_count(), Ordering::Relaxed);
    }

    /// Moved-in (or renamed) folder: record its contents in the overlay too (the index has the old
    /// paths), add the folders inside to `dirs` and what each folder holds to `listed`. Returns
    /// `false` if there are too many (left to a rebuild).
    fn record_tree(
        &self,
        dir: &Path,
        dirs: &mut Vec<PathBuf>,
        listed: &mut HashMap<PathBuf, HashSet<String>>,
    ) -> bool {
        let mut stack = vec![dir.to_path_buf()];
        let mut count = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(iter) = self.fs.read_dir(&dir) else {
                continue;
            };
            let names = listed.entry(dir.clone()).or_default();
            for entry in iter.filter_map(Result::ok) {
                names.insert(entry.name.to_lowercase());
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
                    dirs.push(path.clone());
                }
                self.overlay.lock().record(path, Some(Seen::of(&entry)));
            }
        }
        true
    }

    /// For folders that changed: whatever the index or the overlay still has inside one of them
    /// that's no longer there is recorded as removed. A folder deleted and made again (or replaced
    /// by one moved in) is reported only as the folder itself, so this is where its old contents
    /// go. Folders that can't be listed are left as they are.
    fn forget_what_is_gone(&self, dirs: &[PathBuf], listed: &HashMap<PathBuf, HashSet<String>>) {
        if dirs.is_empty() {
            return;
        }
        let Some(index) = self.index.read().clone() else {
            return;
        };
        let indexed = index.children_of(dirs, &self.stopped);
        let mut known: HashMap<&Path, Vec<String>> = HashMap::new();
        for (dir, children) in &indexed {
            known
                .entry(dir.as_path())
                .or_default()
                .extend(children.iter().map(|(name, _)| name.clone()));
        }
        for dir in dirs {
            let added = self.overlay.lock().added_in(dir);
            known.entry(dir.as_path()).or_default().extend(
                added
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned()),
            );
        }
        for (dir, names) in known {
            if names.is_empty() || self.stopped.load(Ordering::Relaxed) {
                continue;
            }
            let here: HashSet<String> = if let Some(here) = listed.get(dir) {
                here.clone()
            } else {
                let Ok(iter) = self.fs.read_dir(dir) else {
                    continue;
                };
                iter.filter_map(Result::ok)
                    .map(|e| e.name.to_lowercase())
                    .collect()
            };
            let mut overlay = self.overlay.lock();
            for name in names {
                if !here.contains(&name.to_lowercase()) {
                    overlay.record(dir.join(name), None);
                }
            }
        }
    }

    /// The journal can't be followed any more: rebuild, after a short wait, and spaced further
    /// apart while gaps keep coming back. Until then the current index keeps serving searches.
    fn journal_gap(self: &Arc<Self>) {
        let now = Instant::now();
        let repeats = {
            let mut recent = self.gap_rebuilds.lock();
            recent.retain(|t| now.duration_since(*t) < GAP_REPEAT_WINDOW);
            recent.push(now);
            recent.len() - 1
        };
        let wait = gap_wait(repeats);
        if repeats > 0 {
            log::info!(
                "global search: journal gap again ({repeats} in the last hour), rebuilding in {} s",
                wait.as_secs()
            );
        }
        self.schedule(Rebuild::JournalGap, now + wait);
    }

    /// Starts `reason` at `at` at the earliest (from the periodic check). Of two scheduled
    /// rebuilds the earlier one stays: one rebuild covers both.
    fn schedule(&self, reason: Rebuild, at: Instant) {
        let mut scheduled = self.scheduled.lock();
        if scheduled.is_none_or(|(_, when)| at < when) {
            *scheduled = Some((reason, at));
        }
    }

    /// Starts the scheduled rebuild if it's due and none is running.
    fn start_scheduled(self: &Arc<Self>) {
        if self.building.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        let due = self
            .scheduled
            .lock()
            .take_if(|(_, at)| *at <= now)
            .map(|(reason, _)| reason);
        if let Some(reason) = due {
            self.request_build(reason);
        }
    }

    /// Rebuilds in the background. While one runs, this one is scheduled to follow it instead (a
    /// journal gap during a merge, say, still gets its rebuild).
    fn request_build(self: &Arc<Self>, reason: Rebuild) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        if self.building.swap(true, Ordering::AcqRel) {
            log::debug!("global search: already rebuilding, {reason} follows");
            self.schedule(reason, Instant::now());
            return;
        }
        let weak = Arc::downgrade(self);
        spawn("index-build", &self.opts.thread_init, move || {
            if let Some(shared) = weak.upgrade() {
                shared.build(reason);
            }
        });
    }

    /// Walks all roots, swaps in and saves the result. Deferred while resources are tight (the
    /// current index keeps serving).
    #[allow(clippy::too_many_lines, reason = "the build's steps, in order")]
    fn build(self: &Arc<Self>, reason: Rebuild) {
        if !(self.opts.can_build)() {
            *self.deferred_reason.lock() = Some(reason);
            if !self.deferred.swap(true, Ordering::Relaxed) {
                log::info!(
                    "global search: low memory or power saving, deferring index update ({reason})"
                );
            }
            self.building.store(false, Ordering::Release);
            (self.on_change)();
            return;
        }
        self.deferred.store(false, Ordering::Relaxed);
        *self.deferred_reason.lock() = None;
        // Too many changes since the build: fold them in, no need to walk again.
        if matches!(reason, Rebuild::OverlayFull(_)) && self.merge() {
            return;
        }
        *self.last_rebuild.lock() = Some(reason);
        self.rebuilds.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "global search: rebuilding {}: {reason}",
            crate::index::roots_label(&self.opts.roots)
        );
        self.building.store(true, Ordering::Release);
        self.progress.scanned.store(0, Ordering::Relaxed);
        // The index it replaces, if it covers the same roots, tells how big this one will be.
        let expected = self
            .index
            .read()
            .as_ref()
            .filter(|old| old.roots() == self.opts.roots.as_slice())
            .map_or((0, 0), |old| (old.len(), old.name_bytes()));
        self.progress
            .expected
            .0
            .store(expected.0, Ordering::Relaxed);
        self.progress
            .expected
            .1
            .store(expected.1, Ordering::Relaxed);
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
        let init = Arc::clone(&self.opts.thread_init);
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
        // Watch again first if the watcher broke: changes during the walk are then caught, and
        // the index this walk makes counts as current only if it was watched all along.
        if !self.opts.passive && !self.watch_alive.load(Ordering::Acquire) && self.has_unjournaled()
        {
            self.watchers.lock().clear();
            self.watch_roots(&Arc::downgrade(self));
        }
        let watched_from = if self.watch_alive.load(Ordering::Acquire) {
            self.watch_epoch.load(Ordering::Acquire)
        } else {
            0
        };
        let current = self.index.read().clone();
        let base = self.refresh_base(current.as_ref(), reason);
        self.progress.reused.store(0, Ordering::Relaxed);
        self.progress.failed_roots.store(0, Ordering::Relaxed);
        // Nothing to search yet: the walk shares what it has so far.
        self.progress
            .want_partial
            .store(current.is_none(), Ordering::Relaxed);
        drop(current);
        let full = base.is_none();
        let index = match &base {
            Some((old, relist)) => Index::refresh(
                self.fs.as_ref(),
                old,
                relist,
                self.opts.threads,
                &self.stopped,
                &self.progress,
                &move || init(),
                &self.rules,
            ),
            None => Index::build_listed(
                self.fs.as_ref(),
                &self.opts.roots,
                self.opts.threads,
                &self.stopped,
                &self.progress,
                &move || init(),
                &self.rules,
                &lister,
            ),
        }
        .with_cursors(cursors.clone());
        drop(base);
        self.progress.want_partial.store(false, Ordering::Relaxed);
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        if Index::roots_failed(&self.progress) {
            // An offline share lists as nothing: keep what we have and try again later.
            log::warn!(
                "global search: can't list {}, keeping the current index ({reason})",
                crate::index::roots_label(&self.opts.roots)
            );
            self.schedule(reason, Instant::now() + ROOT_RETRY);
            self.building.store(false, Ordering::Release);
            (self.on_change)();
            return;
        }
        log::info!(
            "global search index built: {}, {} items, {:.1} s{}",
            crate::index::roots_label(&self.opts.roots),
            index.len(),
            started.elapsed().as_secs_f32(),
            match self.progress.reused.load(Ordering::Relaxed) {
                0 => String::new(),
                n => format!(", {n} unchanged folders not listed again"),
            }
        );
        if full {
            let now = crate::index::unix_secs(SystemTime::now());
            *self.last_full_walk.lock() = Some(now);
            if let Some(cache) = &self.opts.cache {
                let _ = std::fs::write(walked_path(cache), now.to_string());
            }
        }
        let (index, replaced) = self.save_and_map(index);
        self.overlay.lock().prune_before(seq);
        self.fresh_epoch.store(watched_from, Ordering::Release);
        self.building.store(false, Ordering::Release);
        self.set_index(index);
        // Old file: can't be deleted while queries still use it; leave it (overwritten next save,
        // cleaned at startup).
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(old);
        }
        self.follow_all(&cursors);
    }

    /// The index an incremental rewalk starts from ([`IndexOptions::incremental`]): the current
    /// one with the changes recorded since folded in, so folders taken over unchanged carry
    /// them, and the folders those changes were in (listed again whatever their time says).
    /// `None` for a full walk: not incremental, asked for by the user, nothing to start from,
    /// built with other rules, cut off at the item limit (folders it never got to would look
    /// empty and unchanged), or no full walk for [`FULL_WALK_EVERY`] (a refresh never sees files
    /// changed in place in folders it takes over).
    fn refresh_base(
        &self,
        current: Option<&Arc<Index>>,
        reason: Rebuild,
    ) -> Option<(Arc<Index>, Vec<PathBuf>)> {
        let now = crate::index::unix_secs(SystemTime::now());
        let walked_lately = self
            .last_full_walk
            .lock()
            .is_some_and(|at| now.saturating_sub(at) < FULL_WALK_EVERY.as_secs());
        let old = current.filter(|old| {
            self.opts.incremental
                && walked_lately
                && !matches!(reason, Rebuild::Requested)
                && old.roots() == self.opts.roots.as_slice()
                && old.skipped_build_dirs() == self.rules.skip_build_dirs
                && old.excluded_hash() == self.rules.fingerprint()
                && self.rules.max_items.is_none_or(|max| old.len() < max)
        })?;
        let overlay = self.overlay.lock().clone();
        if overlay.is_empty() {
            return Some((Arc::clone(old), Vec::new()));
        }
        let relist = overlay.folders_touched();
        old.merged(&overlay, &self.rules, &self.stopped)
            .map(|merged| (Arc::new(merged.index), relist))
    }

    /// Writes `index` to the cache file not currently mapped and maps it instead (the in-memory
    /// copy can then go). Returns the index to use and the file it replaces.
    fn save_and_map(&self, index: Index) -> (Index, Option<PathBuf>) {
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
        if index.heap_bytes() > 0 {
            // Not saved (or not read back): it stays in memory, so give back the room set aside.
            index.shrink_to_fit();
        }
        (index, replaced)
    }

    /// Folds the overlay into a new index instead of walking again ([`Index::merged`]); on a
    /// developer disk this replaces a full rebuild every hour or so. `false` if there is nothing
    /// to merge into or it was stopped: rebuild then.
    fn merge(self: &Arc<Self>) -> bool {
        let Some(index) = self.index.read().clone() else {
            return false;
        };
        let started = Instant::now();
        // Positions first, then check that every change up to them is applied (like checkpoints).
        // Not quiet: the merged index keeps the old positions and the journal replays a little
        // more after a restart.
        let cursors = self.latest.lock().clone();
        let quiet = self.pending.lock().is_empty() && !self.draining.load(Ordering::Acquire);
        let (overlay, seq) = {
            let overlay = self.overlay.lock();
            (overlay.clone(), overlay.seq())
        };
        self.merging.store(true, Ordering::Release);
        (self.on_change)();
        let merged = index.merged(&overlay, &self.rules, &self.stopped);
        let recorded = overlay.len();
        drop(overlay);
        let Some(crate::index::Merged {
            index: mut merged,
            unplaced,
        }) = merged
        else {
            self.merging.store(false, Ordering::Release);
            return false;
        };
        if quiet && unplaced.is_empty() && cursors.iter().any(Option::is_some) {
            merged = merged.with_cursors(cursors);
        }
        let items = merged.len();
        let (merged, replaced) = self.save_and_map(merged);
        self.overlay.lock().prune_merged(seq, &unplaced);
        // The checkpoint on disk belongs to the old index: write one for this one soon.
        *self.last_checkpoint.lock() = None;
        self.checkpointed.lock().clear();
        self.merges.fetch_add(1, Ordering::Relaxed);
        self.merging.store(false, Ordering::Release);
        self.building.store(false, Ordering::Release);
        self.set_index(merged);
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(old);
        }
        log::info!(
            "global search: folded {} changes into the index of {}, no rewalk ({} items, {} \
             left over, {:.1} s)",
            recorded - unplaced.len(),
            crate::index::roots_label(&self.opts.roots),
            items,
            unplaced.len(),
            started.elapsed().as_secs_f32()
        );
        true
    }
}

/// Journal checkpoint file name (`index.bin` -> `index.checkpoint`). Shared by both saved
/// files; it records which index it belongs to.
fn checkpoint_path(cache: &Path) -> PathBuf {
    cache.with_extension("checkpoint")
}

/// When the saved index was last walked in full (not refreshed), in Unix seconds as text.
fn walked_path(cache: &Path) -> PathBuf {
    cache.with_extension("walked")
}

/// Alternate save name (`index.bin` -> `index.alt.bin`). The mapped file can't be
/// overwritten, so saves alternate between the two names.
fn alt_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map_or_default(|s| s.to_string_lossy().into_owned());
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
    let init = Arc::clone(init);
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
            max_items: None,
            incremental: false,
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

    /// Before the first index exists, searches see what the walk has found so far.
    #[test]
    fn a_first_build_is_searchable_before_it_ends() {
        let fs = FakeFs::new();
        let dir = |name: &str| Entry::new(name, EntryKind::Dir, 0, None, 0);
        let file = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        fs.insert_dir(r"C:\", vec![dir("a"), dir("b"), dir("slow")]);
        fs.insert_dir(r"C:\a", vec![file("found.txt")]);
        fs.insert_dir(r"C:\b", vec![file("other.txt")]);
        fs.set_delay(r"C:\b", Duration::from_millis(50));
        fs.insert_dir(r"C:\slow", vec![file("late.txt")]);
        fs.hang(r"C:\slow");
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            IndexOptions {
                threads: 3,
                ..opts(None)
            },
            || {},
        );
        wait("partial results", || {
            found(&svc, "found") == [r"C:\a\found.txt"]
        });
        assert!(svc.status().scanning.is_some(), "still walking");
        assert!(found(&svc, "late").is_empty());
        fs.release_all();
        wait("built", || ready(&svc));
        assert_eq!(found(&svc, "late"), [r"C:\slow\late.txt"]);
    }

    /// With `incremental`, a rewalk lists again only folders that may have changed; one the user
    /// asks for lists everything.
    #[test]
    fn incremental_rebuilds_list_only_changed_folders() {
        let fs = FakeFs::new();
        let then = Some(std::time::UNIX_EPOCH + Duration::from_secs(1_000));
        let dir = |name: &str| Entry::new(name, EntryKind::Dir, 0, then, 0);
        let file = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        fs.insert_dir(r"C:\", vec![dir("a"), dir("b")]);
        fs.insert_dir(r"C:\a", vec![file("a.txt")]);
        fs.insert_dir(r"C:\b", vec![file("b.txt")]);
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            IndexOptions {
                incremental: true,
                refresh_after: Duration::from_hours(1),
                ..opts(None)
            },
            || {},
        );
        wait("built", || ready(&svc));
        let calls = fs.read_dir_calls();
        svc.shared.request_build(Rebuild::Stale);
        wait("rewalked", || svc.rebuilds() == 2 && ready(&svc));
        assert_eq!(
            fs.read_dir_calls() - calls,
            1,
            "only the root is listed again"
        );
        assert_eq!(found(&svc, "b.txt"), [r"C:\b\b.txt"]);
        svc.rebuild();
        wait("rebuilt", || svc.rebuilds() == 3 && ready(&svc));
        assert_eq!(fs.read_dir_calls() - calls, 4);
    }

    fn share_opts(cache: Option<PathBuf>) -> IndexOptions {
        IndexOptions {
            roots: vec![PathBuf::from(r"\\nas\s")],
            incremental: true,
            refresh_after: Duration::from_hours(1),
            ..opts(cache)
        }
    }

    fn share_fs() -> Arc<FakeFs> {
        let fs = FakeFs::new();
        let then = Some(std::time::UNIX_EPOCH + Duration::from_secs(1_000));
        fs.insert_dir(
            r"\\nas\s",
            vec![Entry::new("docs", EntryKind::Dir, 0, then, 0)],
        );
        fs.insert_dir(
            r"\\nas\s\docs",
            vec![Entry::new("plan.txt", EntryKind::File, 1, None, 0)],
        );
        fs
    }

    fn items(svc: &IndexService) -> Option<usize> {
        svc.items_under(Path::new(r"\\nas\s"), false, &AtomicBool::new(false))
            .map(|tree| tree.items.len())
    }

    /// A search may take a folder's items from the index only while the index is known to be
    /// current: walked in this run with the watcher working. Not once the watcher lost events
    /// (until a rewalk after watching again), and not for a saved index loaded at startup
    /// before the rewalk that catches up on changes made meanwhile.
    #[test]
    fn items_come_from_the_index_only_while_it_is_current() {
        let (dir, cache) = temp_cache("current");
        let fs = share_fs();
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            share_opts(Some(cache.clone())),
            || {},
        );
        wait("built", || ready(&svc));
        assert_eq!(items(&svc), Some(2));
        fs.emit(Path::new(r"\\nas\s"), &WatchEvent::Rescan);
        assert_eq!(items(&svc), None, "the watcher lost events");
        wait("rewalked", || svc.rebuilds() == 2 && ready(&svc));
        assert_eq!(items(&svc), Some(2), "watched again and walked");
        drop(svc);

        // Started again with the saved index: not current until the catch-up rewalk ends.
        fs.hang(r"\\nas\s");
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            share_opts(Some(cache)),
            || {},
        );
        wait("loaded", || svc.status().count.is_some());
        assert!(svc.status().scanning.is_some(), "catching up");
        assert_eq!(items(&svc), None);
        fs.release_all();
        wait("caught up", || ready(&svc));
        assert_eq!(items(&svc), Some(2));
        assert_eq!(svc.last_rebuild(), Some(Rebuild::Resumed));
        drop(svc);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An index cut off at the item limit never stands in for a walk: folders it didn't get to
    /// would look empty.
    #[test]
    fn a_truncated_index_is_not_used_for_items() {
        let svc = IndexService::start(
            share_fs() as Arc<dyn Fs>,
            IndexOptions {
                max_items: Some(2),
                ..share_opts(None)
            },
            || {},
        );
        wait("built", || ready(&svc));
        assert_eq!(items(&svc), None);
    }

    /// A rewalk while the share is offline keeps the index there is instead of an empty one.
    #[test]
    fn an_offline_share_keeps_its_index() {
        let fs = share_fs();
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, share_opts(None), || {});
        wait("built", || ready(&svc));
        fs.remove_dir(Path::new(r"\\nas\s"));
        svc.rebuild();
        wait("rewalked", || {
            svc.rebuilds() >= 2 && !svc.shared.building.load(Ordering::Acquire)
        });
        assert_eq!(svc.status().count, Some(3));
        assert_eq!(found(&svc, "plan"), [r"\\nas\s\docs\plan.txt"]);
    }

    /// Rewalks are incremental, but at least once a week a full walk catches what they can't
    /// see (files changed in place in folders taken over).
    #[test]
    fn incremental_rewalks_still_walk_fully_every_week() {
        let fs = share_fs();
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, share_opts(None), || {});
        wait("built", || ready(&svc));
        let calls = fs.read_dir_calls();
        let week_ago = crate::index::unix_secs(SystemTime::now()) - FULL_WALK_EVERY.as_secs();
        *svc.shared.last_full_walk.lock() = Some(week_ago);
        svc.shared.request_build(Rebuild::Stale);
        wait("rewalked", || svc.rebuilds() == 2 && ready(&svc));
        assert_eq!(fs.read_dir_calls() - calls, 2, "everything listed again");
        let calls = fs.read_dir_calls();
        svc.shared.request_build(Rebuild::Stale);
        wait("refreshed", || svc.rebuilds() == 3 && ready(&svc));
        assert_eq!(fs.read_dir_calls() - calls, 1, "then incremental again");
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
            Arc::clone(&fs) as Arc<dyn Fs>,
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
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                excluded(&[r"c:\games\"]),
                || {},
            );
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
            max_items: None,
        };
        assert!(rules.is_inside_excluded(Path::new(r"d:\media\a.mp4")));
        assert!(!rules.is_inside_excluded(Path::new(r"D:\Media")));
        assert!(!rules.is_inside_excluded(Path::new(r"D:\Media2\a.mp4")));
        assert!(rules.is_inside_excluded(Path::new(r"E:\x")));
        assert_eq!(BuildRules::default().fingerprint(), 0);
        let same = BuildRules {
            skip_build_dirs: true,
            excluded: vec![PathBuf::from(r"e:\"), PathBuf::from(r"d:\media")],
            max_items: None,
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
            Arc::clone(&fs) as Arc<dyn Fs>,
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
        let gate = Arc::clone(&ok);
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
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, opts(None), || {});
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

    /// Temporary files made and deleted since the build leave no removal records behind; a file
    /// the index has stays gone, and a new folder removed with what it held leaves nothing in
    /// search.
    #[test]
    fn removals_of_what_the_index_never_had_are_dropped() {
        let fs = FakeFs::new();
        let me = Path::new(r"C:\Users\me");
        fs.insert_dir(
            me,
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, opts(None), || {});
        wait("index built", || ready(&svc));
        let removed = || svc.shared.overlay.lock().removed_count();
        let emit = |paths: Vec<PathBuf>| fs.emit(Path::new(r"C:\"), &WatchEvent::Changed(paths));

        // A folder made after the build, with a file in it, then removed: its record stays, so the
        // file inside isn't found.
        let made = me.join("made");
        fs.add_entry(me, Entry::new("made", EntryKind::Dir, 0, None, 0));
        fs.insert_dir(
            &made,
            vec![Entry::new("inside.txt", EntryKind::File, 1, None, 0)],
        );
        emit(vec![made.clone(), made.join("inside.txt")]);
        wait("inside found", || found(&svc, "inside").len() == 1);
        fs.remove(me, "made");
        fs.remove_dir(&made);
        emit(vec![made]);
        wait("the removed folder's file gone", || {
            found(&svc, "inside").is_empty()
        });
        // The indexed file is deleted too.
        fs.remove(me, "report.docx");
        emit(vec![me.join("report.docx")]);
        wait("the indexed file gone", || found(&svc, "report").is_empty());

        // Temporary files: made, seen, deleted.
        for i in 0..6 {
            let name = format!("~tmp{i}.tmp");
            fs.add_file(me, &name, 1);
            emit(vec![me.join(&name)]);
            wait("seen", || found(&svc, &format!("~tmp{i}")).len() == 1);
            fs.remove(me, &name);
            emit(vec![me.join(&name)]);
            wait("gone", || found(&svc, &format!("~tmp{i}")).is_empty());
        }
        wait("the temporary ones forgotten", || removed() <= 2);
        assert!(
            found(&svc, "report").is_empty(),
            "the indexed file stays gone"
        );
        assert!(
            found(&svc, "inside").is_empty(),
            "nothing of the removed folder"
        );
        assert!(found(&svc, "~tmp").is_empty());
    }

    /// #310: renamed to change only the case, reported under the old spelling (which Windows
    /// still finds). Search shows the name it has now, once.
    #[test]
    fn a_case_only_rename_is_found_under_its_new_name() {
        let fs = FakeFs::new();
        let me = Path::new(r"C:\Users\me");
        fs.insert_dir(
            me,
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, opts(None), || {});
        wait("index built", || ready(&svc));
        fs.remove(me, "report.docx");
        fs.add_file(me, "Report.docx", 1);
        fs.emit(
            Path::new(r"C:\"),
            &WatchEvent::Changed(vec![me.join("report.docx")]),
        );
        wait("the new spelling", || {
            found(&svc, "report") == [r"C:\Users\me\Report.docx"]
        });
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
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, opts(Some(cache)), || {});
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
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, options, || {});
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
        o.journal = Some(Arc::clone(journal) as Arc<dyn Journal>);
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
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
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
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
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

    /// A folder replaced by a new one at the same path (`rm -rf build && mkdir build`, or an
    /// updater moving a new `app` in place of the old one): the journal reports the folder itself,
    /// but not the old contents, whose records name a parent that no longer exists. None of the
    /// old contents may come back, after catching up or live.
    #[test]
    fn a_folder_replaced_at_the_same_path_leaves_nothing_behind() {
        let (dir, cache) = temp_cache("replaced");
        let me = Path::new(r"C:\Users\me");
        let build = me.join("build");
        let app = me.join("app");
        let file = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        let fs = FakeFs::new();
        fs.insert_dir(
            &build,
            vec![
                file("old-report.txt"),
                Entry::new("sub", EntryKind::Dir, 0, None, 0),
            ],
        );
        fs.insert_dir(build.join("sub"), vec![file("old-nested-report.txt")]);
        fs.insert_dir(&app, vec![file("old-app-report.txt")]);
        let journal = FakeJournal::new(5);
        {
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
            wait("first build", || ready(&svc));
            wait("journal caught up", || svc.status().live);
            assert_eq!(found(&svc, "report").len(), 3);
        }
        // While Arke is closed: `build` is deleted and made again, `app` is swapped for a new
        // folder moved in under the same name.
        fs.remove_dir(&build);
        journal.push(&build, false);
        fs.insert_dir(&build, vec![file("new-report.txt")]);
        journal.push(&build, false);
        journal.push(build.join("new-report.txt"), false);
        fs.remove_dir(&app);
        fs.insert_dir(&app, vec![file("new-app-report.txt")]);
        journal.push(&app, true);

        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
        wait("offline changes replayed", || {
            let f = found(&svc, "report");
            f.iter().any(|p| p.ends_with("new-report.txt"))
                && f.iter().any(|p| p.ends_with("new-app-report.txt"))
        });
        // Give any leftover a chance to show (the drain runs after a short settle).
        std::thread::sleep(Duration::from_millis(400));
        let mut f = found(&svc, "report");
        f.sort();
        assert_eq!(
            f,
            [
                app.join("new-app-report.txt").display().to_string(),
                build.join("new-report.txt").display().to_string(),
            ],
            "the old contents are gone after catching up"
        );

        // While running: replaced once more.
        fs.remove_dir(&build);
        journal.push(&build, false);
        fs.insert_dir(&build, vec![file("third-report.txt")]);
        journal.push(&build, false);
        journal.push(build.join("third-report.txt"), false);
        wait("live replacement", || {
            found(&svc, "report")
                .iter()
                .any(|p| p.ends_with("third-report.txt"))
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !found(&svc, "report")
                .iter()
                .any(|p| p.ends_with(r"build\new-report.txt")),
            "the previous contents are gone live too"
        );
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
            let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, opts(None), || {});
            wait("built", || ready(&svc));
            assert_eq!(sizes(&svc), None);
        }
        let journal = FakeJournal::new(3);
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
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

    /// The service lists other people's profile folders without contents, so an index built from
    /// its listing has nothing in them: a count of the profiles folder walks them (`Default` can
    /// be read, Bob's can't: "at least"), one inside them is walked instead. An index built by a
    /// walk has what could be read already and counts it once.
    #[test]
    fn hidden_profiles_count_from_the_index() {
        use crate::dirsize::Profiles;
        use crate::journal::{LISTED_ROOT, Listed};
        let (dir, cache) = temp_cache("profiles");
        let fs = FakeFs::new();
        let file = |name: &str, size| Entry::new(name, EntryKind::File, size, None, 0);
        let folder = |name: &str| Entry::new(name, EntryKind::Dir, 0, None, 0x10);
        fs.insert_dir(
            r"C:\Users",
            vec![folder("me"), folder("Default"), folder("Bob")],
        );
        fs.insert_dir(r"C:\Users\me", vec![file("mine.txt", 10)]);
        // Bob's folder can't be listed (not in the fake).
        fs.insert_dir(r"C:\Users\Default", vec![file("NTUSER.DAT", 300)]);
        let profiles = Profiles {
            dir: PathBuf::from(r"C:\Users"),
            mine: Some(PathBuf::from(r"C:\Users\me")),
        };
        let cancel = AtomicBool::new(false);
        let item = |parent, name: &str, dir: bool, size| Listed {
            parent,
            name: name.into(),
            attributes: if dir { 0x10 } else { 0x20 },
            size,
            mtime: 0,
            link: false,
        };
        for listed in [true, false] {
            let _ = std::fs::remove_dir_all(&dir);
            let journal = FakeJournal::new(3);
            if listed {
                journal.set_listing(
                    r"C:\",
                    vec![
                        item(LISTED_ROOT, "Users", true, 0),
                        item(0, "me", true, 0),
                        item(0, "Default", true, 0),
                        item(0, "Bob", true, 0),
                        item(1, "mine.txt", false, 10),
                    ],
                );
            }
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
            wait("journal caught up", || ready(&svc) && svc.status().live);
            let users = svc
                .usage_below(Path::new(r"C:\users"), 0, Some(&profiles), &cancel)
                .expect("from the index");
            assert_eq!(users.usage.bytes, 310, "listed: {listed}");
            if listed {
                assert!(users.usage.partial, "Bob's can't be read");
            }
            let mine = svc
                .usage_below(Path::new(r"C:\Users\me"), 0, Some(&profiles), &cancel)
                .expect("from the index");
            assert_eq!((mine.usage.bytes, mine.usage.partial), (10, false));
            let inside =
                svc.usage_below(Path::new(r"C:\Users\Default"), 0, Some(&profiles), &cancel);
            assert_eq!(inside, None, "walked instead");
            drop(svc);
        }
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
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
            wait("first build", || ready(&svc) && svc.status().live);
            fs.add_file(a, "new-report.txt", 1);
            journal.push(a.join("new-report.txt"), false);
            wait("followed new file", || found(&svc, "new-report").len() == 1);
            wait("checkpoint saved", || dir.join("index.checkpoint").exists());
            // Wait for it to write the latest position (checkpoints are spaced out by
            // CHECKPOINT_MAX_AGE).
            wait("checkpoint with the change", || svc.checkpoint_is_current());
        }
        // While closed: the journal wraps and the build-time position is gone.
        journal.forget_all();
        let walked = fs.read_dir_calls();
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
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
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
            wait("first build", || ready(&svc));
        }
        // Journal recreated while closed (new ID): saved position doesn't connect, so rebuild.
        journal.recreate(2);
        fs.add_file(Path::new(r"C:\a"), "unlogged-report.txt", 1);
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
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

    /// #343: the index service going away (stopped, updated) isn't a journal gap. Following
    /// starts again from the last position, which still gets the changes made meanwhile, and
    /// nothing is walked again.
    #[test]
    fn the_service_going_away_is_followed_again_not_rebuilt() {
        let (dir, cache) = temp_cache("refollow");
        let fs = FakeFs::new();
        let a = Path::new(r"C:\a");
        fs.insert_dir(
            a,
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        let journal = FakeJournal::new(1);
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
        wait("first build", || ready(&svc) && svc.status().live);
        let walked = fs.read_dir_calls();
        let calls = journal.follow_calls();
        let rebuilds = svc.rebuilds();

        journal.disconnect();
        // Changed while the service is away: still in the journal.
        fs.add_file(a, "meanwhile-report.txt", 1);
        journal.push(a.join("meanwhile-report.txt"), false);
        wait("followed again", || journal.follow_calls() > calls);
        wait("the change made meanwhile", || {
            found(&svc, "meanwhile-report").len() == 1
        });
        wait("live again", || svc.status().live);
        assert_eq!(fs.read_dir_calls(), walked, "nothing walked again");
        assert_eq!(svc.rebuilds(), rebuilds, "no rebuild");
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rebuild asked for while one runs isn't dropped: it follows the running one (a journal gap
    /// during a merge used to leave the drive with no change source until restart).
    #[test]
    fn a_rebuild_asked_for_during_one_follows_it() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\a",
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, opts(None), || {});
        wait("first build", || ready(&svc));
        let before = svc.rebuilds();
        fs.set_delay(r"C:\a", Duration::from_millis(200));
        svc.rebuild();
        wait("rebuilding", || svc.status().scanning.is_some());
        svc.rebuild();
        wait("both rebuilds ran", || svc.rebuilds() == before + 2);
        wait("done", || ready(&svc));
    }

    /// Lost watch events soon after a rebuild schedule one for later instead of being ignored
    /// (until the next daily refresh, changes they hid stayed missing).
    #[test]
    fn lost_watch_events_soon_after_a_rebuild_are_caught_up_later() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\a",
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let mut o = opts(None);
        o.refresh_after = Duration::from_hours(24);
        let svc = IndexService::start(Arc::clone(&fs) as Arc<dyn Fs>, o, || {});
        wait("first build", || ready(&svc));
        // A change the watcher missed, then its overflow, right after the build.
        fs.add_file(Path::new(r"C:\a"), "missed-report.txt", 1);
        fs.emit(Path::new(r"C:\"), &WatchEvent::Rescan);
        wait("caught up once the gap is up", || {
            found(&svc, "missed-report").len() == 1
        });
        assert_eq!(svc.last_rebuild(), Some(Rebuild::WatchOverflow));
    }

    /// Journal gaps that keep coming back wait longer each time, up to a limit.
    #[test]
    fn repeated_journal_gaps_wait_longer() {
        assert_eq!(gap_wait(0), GAP_FIRST_DELAY);
        assert_eq!(gap_wait(1), GAP_BACKOFF);
        assert_eq!(gap_wait(2), GAP_BACKOFF * 2);
        assert_eq!(gap_wait(3), GAP_BACKOFF * 4);
        assert_eq!(gap_wait(50), GAP_BACKOFF_MAX);
    }

    /// A cancelled query returns nothing instead of partial results.
    #[test]
    fn cancelled_searches_return_nothing() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\a",
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        let svc = IndexService::start(fs, opts(None), || {});
        wait("built", || ready(&svc));
        let run = |cancel: bool| {
            svc.search_until(
                "report",
                Clock::default(),
                10,
                false,
                &AtomicBool::new(cancel),
            )
            .map(|hits| hits.len())
        };
        assert_eq!(run(false), Some(1));
        assert_eq!(run(true), None);
    }

    /// Every rebuild carries its reason (it goes into the log line): first build, too many
    /// changes at once, asked for, settings changed. Too many changes since the build no longer
    /// rebuild: they are folded into the index without walking.
    #[test]
    fn rebuilds_say_why() {
        let (dir, cache) = temp_cache("reasons");
        let me = Path::new(r"C:\Users\me");
        let fs = FakeFs::new();
        fs.insert_dir(
            me,
            vec![Entry::new("report.docx", EntryKind::File, 1, None, 0)],
        );
        {
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                opts(Some(cache.clone())),
                || {},
            );
            wait("first build", || ready(&svc));
            assert_eq!(svc.last_rebuild(), Some(Rebuild::NoIndex));

            // Changes trickle in until the overlay holds more than OVERLAY_MAX records: merged,
            // not walked again.
            let walked = fs.read_dir_calls();
            for i in 0..=OVERLAY_MAX {
                let name = format!("note-{i}.txt");
                fs.add_file(me, &name, 1);
                fs.emit(
                    Path::new(r"C:\"),
                    &WatchEvent::Changed(vec![me.join(&name)]),
                );
                wait("change recorded", || {
                    svc.merges() > 0 || found(&svc, &format!("note-{i}.txt")).len() == 1
                });
            }
            wait("merged", || svc.merges() == 1 && ready(&svc));
            assert_eq!(fs.read_dir_calls(), walked, "no rewalk");
            assert_eq!(svc.last_rebuild(), Some(Rebuild::NoIndex));
            assert_eq!(svc.shared.overlay.lock().len(), 0);
            assert_eq!(found(&svc, "note-20.txt"), [r"C:\Users\me\note-20.txt"]);

            // More than OVERLAY_MAX at once: rebuilt.
            let burst: Vec<PathBuf> = (0..=OVERLAY_MAX)
                .map(|i| {
                    let name = format!("burst-{i}.txt");
                    fs.add_file(me, &name, 1);
                    me.join(name)
                })
                .collect();
            fs.emit(Path::new(r"C:\"), &WatchEvent::Changed(burst));
            wait("burst rebuild", || {
                svc.last_rebuild() == Some(Rebuild::TooManyChanges(OVERLAY_MAX + 1))
            });
            wait("rebuilt", || {
                ready(&svc)
                    && found(&svc, "burst-3.txt").contains(&r"C:\Users\me\burst-3.txt".into())
            });
            svc.rebuild();
            wait("asked", || svc.last_rebuild() == Some(Rebuild::Requested));
            wait("rebuilt", || ready(&svc));
        }
        // Saved with build folders indexed; now they are skipped: the save doesn't fit.
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            IndexOptions {
                skip_build_dirs: true,
                ..opts(Some(cache))
            },
            || {},
        );
        wait("settings rebuild", || ready(&svc));
        assert_eq!(svc.last_rebuild(), Some(Rebuild::SettingsChanged));
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            Rebuild::OverlayFull(100_001).to_string(),
            "100001 changes since the last build"
        );
        assert_eq!(Rebuild::JournalGap.to_string(), "journal gap");
    }

    /// With the journal: a merge saves an index that already holds the changes, a checkpoint for
    /// it follows, and a restart after the journal wrapped picks both up without a rewalk.
    #[test]
    fn merged_changes_survive_a_restart() {
        let (dir, cache) = temp_cache("merge");
        let a = Path::new(r"C:\a");
        let fs = FakeFs::new();
        fs.insert_dir(
            a,
            vec![Entry::new("gone-report.txt", EntryKind::File, 1, None, 0)],
        );
        let journal = FakeJournal::new(9);
        {
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
            wait("first build", || ready(&svc) && svc.status().live);
            fs.remove(a, "gone-report.txt");
            journal.push(a.join("gone-report.txt"), false);
            for i in 0..=OVERLAY_MAX {
                let name = format!("new-report-{i}.txt");
                fs.add_file(a, &name, 1);
                journal.push(a.join(&name), false);
                wait("applied", || {
                    svc.merges() > 0 || found(&svc, &name).len() == 1
                });
            }
            wait("merged", || svc.merges() == 1 && ready(&svc));
            wait("checkpoint for the merged index", || {
                svc.checkpoint_is_current()
            });
        }
        journal.forget_all();
        let walked = fs.read_dir_calls();
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
        wait("resumed", || ready(&svc) && svc.status().live);
        assert_eq!(fs.read_dir_calls(), walked, "no rewalk");
        assert_eq!(svc.last_rebuild(), None);
        assert!(found(&svc, "gone-report").is_empty());
        assert_eq!(
            svc.search("new-report", Clock::default(), 100, false).len(),
            OVERLAY_MAX + 1
        );
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Steady churn (a change every few ms, like a build running) no longer rewrites the
    /// checkpoint at every tick: it is written once enough changed or the last one is old
    /// enough, and the final state still makes it to disk.
    #[test]
    fn checkpoints_are_spaced_out_under_steady_churn() {
        let (dir, cache) = temp_cache("churn");
        let a = Path::new(r"C:\a");
        let fs = FakeFs::new();
        fs.insert_dir(
            a,
            vec![Entry::new("old-report.txt", EntryKind::File, 1, None, 0)],
        );
        let journal = FakeJournal::new(4);
        let started;
        {
            let svc = IndexService::start(
                Arc::clone(&fs) as Arc<dyn Fs>,
                journal_opts(&cache, &journal),
                || {},
            );
            wait("first build", || ready(&svc) && svc.status().live);
            started = Instant::now();
            let mut i = 0;
            while started.elapsed() < Duration::from_millis(1600) {
                let name = format!("tmp-{}.obj", i % 15);
                fs.add_file(a, &name, i);
                journal.push(a.join(&name), false);
                i += 1;
                // Long enough for each change to be applied (checkpoints wait for that).
                std::thread::sleep(Duration::from_millis(30));
            }
            let ticks = started.elapsed().as_millis() / CHECKPOINT_EVERY.as_millis();
            let (writes, _) = svc.checkpoint_writes();
            // Before: one per tick that found the queue empty (over 10 here). Now: the first,
            // then one per `max_age` (about 4).
            assert!(
                writes <= 7,
                "{writes} checkpoints in {ticks} ticks under steady churn"
            );
            // The last changes still reach the disk soon after things calm down.
            fs.add_file(a, "last-report.txt", 1);
            journal.push(a.join("last-report.txt"), false);
            wait("final checkpoint", || svc.checkpoint_is_current());
        }
        // A restart after the journal wrapped resumes from that checkpoint: nothing rewalked,
        // nothing lost.
        journal.forget_all();
        let walked = fs.read_dir_calls();
        let svc = IndexService::start(
            Arc::clone(&fs) as Arc<dyn Fs>,
            journal_opts(&cache, &journal),
            || {},
        );
        wait("resumed", || ready(&svc) && svc.status().live);
        assert_eq!(fs.read_dir_calls(), walked, "no rewalk");
        assert_eq!(found(&svc, "last-report"), [r"C:\a\last-report.txt"]);
        assert_eq!(svc.last_rebuild(), None);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write volume of checkpoints over a day on a busy developer disk, old rule against new.
    /// Model from the field log: about 37 changes/s, rebuilt at 100k changes (every ~45 min), a
    /// checkpoint costing ~100 bytes per recorded change.
    #[test]
    fn checkpoint_policy_cuts_daily_writes() {
        let tick = Duration::from_mins(2);
        let per_tick = 37 * 120;
        let simulate = |due: &dyn Fn(u64, Option<Duration>) -> bool| {
            let (mut overlay, mut bytes, mut since, mut changed) = (0u64, 0u64, None, 0u64);
            for _ in 0..(24 * 30) {
                overlay += per_tick;
                changed += per_tick;
                since = since.map(|s: Duration| s + tick);
                if overlay > 100_000 {
                    // Rebuild: the overlay starts over.
                    overlay = 0;
                    changed = 0;
                    continue;
                }
                if due(changed, since) {
                    bytes += overlay * 100;
                    changed = 0;
                    since = Some(Duration::ZERO);
                }
            }
            bytes
        };
        let policy = CheckpointPolicy::DEFAULT;
        let before = simulate(&|_, _| true);
        let after = simulate(&|changes, age| policy.due(changes, true, age));
        // About 3.6 GB a day before, 0.8 GB after.
        assert!(
            after * 4 <= before,
            "before {} MB/day, after {} MB/day",
            before >> 20,
            after >> 20
        );
        assert!(policy.due(1, false, None), "first one in a run right away");
        assert!(!policy.due(0, false, Some(Duration::from_hours(5))));
        assert!(policy.due(policy.records, true, Some(Duration::ZERO)));
        assert!(!policy.due(1, true, Some(Duration::ZERO)));
        assert!(policy.due(1, true, Some(policy.max_age)));
        assert!(!policy.due(0, true, Some(policy.max_age)));
        assert!(policy.due(0, true, Some(policy.position_age)));
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
