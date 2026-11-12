//! Background directory service: listing and watching run on background threads and
//! report through `Sink`.
//!
//! Each navigation bumps the generation; threads check it in their loops and quit when
//! stale, without sending results. Every request gets a fresh thread, so an old request
//! stuck in network I/O never blocks a new one (the stuck thread exits after the OS timeout).
//!
//! Network locations (when a [`NetProbe`] is set):
//! - The server is probed while listing; if it is definitely unreachable we fail right away
//!   instead of waiting for the SMB timeout.
//! - Recently listed network folders are remembered and shown first (`cached`) on revisit,
//!   then replaced by fresh contents, so they stay visible if the server goes away.
//! - SMB watching sometimes misses events, but we don't poll (it keeps NAS disks awake and
//!   generates traffic). [`DirService::recheck`] (Arke regains focus) relists once, diffs
//!   against the last listing and sends only the differences.

use parking_lot::Mutex;
use rust_i18n::t;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::entry::Entry;
use crate::fs::{Fs, WatchEvent};
use crate::model::Delta;
use crate::net::{FailKind, NetProbe};
use crate::sort::SortSpec;

/// Request generation; bumped by every navigation so stale results can be dropped.
pub type Gen = u64;

/// Options for [`DirService::navigate`].
#[derive(Clone, Copy, Debug)]
pub struct ListOptions {
    /// Order of the entries in each snapshot.
    pub sort: SortSpec,
    /// Include hidden items (see [`Entry::concealed`]).
    pub show_hidden: bool,
    /// Watch the directory for changes.
    pub watch: bool,
}

/// Result of a directory request, delivered to the [`Sink`].
#[derive(Debug)]
pub enum IoEvent {
    /// Listing snapshot (sorted and filtered). `complete == false` means still streaming.
    Listing {
        /// Request this snapshot answers.
        generation: Gen,
        /// Directory listed.
        path: PathBuf,
        /// Visible entries so far, sorted by `sorted_by`.
        entries: Arc<Vec<Entry>>,
        /// Order of `entries`.
        sorted_by: SortSpec,
        /// Final snapshot for this generation; later changes arrive as `Delta`.
        complete: bool,
        /// Entries left out as hidden.
        hidden_count: usize,
        /// How long listing and sorting took.
        timing: ListTiming,
        /// Last known contents of a network folder (fresh ones still on the way).
        cached: bool,
    },
    /// Listing failed (folder gone, access denied, server unreachable...).
    ListFailed {
        /// Request that failed.
        generation: Gen,
        /// Directory that could not be listed.
        path: PathBuf,
        /// Localized message for display.
        error: String,
        /// Decides which action the UI offers.
        kind: FailKind,
    },
    /// The path is a file, not a folder (typed or pasted in the path bar, passed on the command
    /// line...). The UI opens it and shows its folder instead, like Explorer.
    IsFile {
        /// Request this answers.
        generation: Gen,
        /// The file.
        path: PathBuf,
    },
    /// Changes in the listed directory, found by watching.
    Delta {
        /// Listing these changes apply to.
        generation: Gen,
        /// The changes, to apply with [`crate::DirModel::apply_delta`].
        delta: Delta,
    },
    /// The watcher overflowed or saw too many changes; relist.
    NeedsRescan {
        /// Listing that must be redone.
        generation: Gen,
    },
}

/// Timing of one listing snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct ListTiming {
    /// Time spent enumerating (including per-entry metadata).
    pub enumerate: Duration,
    /// Time spent sorting.
    pub sort: Duration,
    /// Time from the request to this snapshot.
    pub total: Duration,
}

/// Receives [`IoEvent`]s on background threads.
pub type Sink = Arc<dyn Fn(IoEvent) + Send + Sync>;

/// Lists and watches directories on background threads (see the module docs).
pub struct DirService {
    fs: Arc<dyn Fs>,
    current: Arc<AtomicU64>,
    sink: Sink,
    probe: Option<Arc<dyn NetProbe>>,
    cache: Arc<Mutex<NetCache>>,
    /// Recheck trigger for the current network folder ([`DirService::recheck`]).
    recheck: Arc<Mutex<Option<flume::Sender<()>>>>,
}

/// Number of network folders remembered.
const NET_CACHE_DIRS: usize = 32;
/// Folders with more entries are not remembered (memory, and they are slow to list anyway).
const NET_CACHE_MAX_ENTRIES: usize = 50_000;
/// Network folders with more entries are not rechecked (every recheck transfers the whole listing).
const NET_RECHECK_MAX_ENTRIES: usize = 10_000;

/// Recently listed network folders (memory only).
#[derive(Default)]
struct NetCache {
    dirs: HashMap<String, Cached>,
    /// Oldest first.
    order: VecDeque<String>,
}

struct Cached {
    entries: Arc<Vec<Entry>>,
    sorted_by: SortSpec,
    show_hidden: bool,
    /// Hiding rules version used when filtering.
    rules: u32,
    hidden_count: usize,
}

fn cache_key(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

impl NetCache {
    fn get(&self, path: &Path, show_hidden: bool) -> Option<(Arc<Vec<Entry>>, SortSpec, usize)> {
        let c = self.dirs.get(&cache_key(path))?;
        (c.show_hidden == show_hidden && c.rules == crate::entry::hidden_rules_version())
            .then(|| (c.entries.clone(), c.sorted_by, c.hidden_count))
    }

    fn put(&mut self, path: &Path, cached: Cached) {
        let key = cache_key(path);
        self.order.retain(|k| *k != key);
        self.order.push_back(key.clone());
        self.dirs.insert(key, cached);
        while self.order.len() > NET_CACHE_DIRS {
            if let Some(old) = self.order.pop_front() {
                self.dirs.remove(&old);
            }
        }
    }
}

/// Which of probe and listing reached a verdict first.
const OUTCOME_PENDING: u8 = 0;
const OUTCOME_LISTED: u8 = 1;
const OUTCOME_FAILED: u8 = 2;

/// Streaming snapshot interval: slow (network) directories send what they have so far this often.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Debounce for coalescing watch events.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(120);
/// Maximum delay while events keep arriving.
const WATCH_MAX_LATENCY: Duration = Duration::from_millis(500);
/// A coalesced batch with more changes than this requests a relist instead.
const WATCH_MAX_BATCH: usize = 512;

impl DirService {
    /// Creates the service without network handling (see [`Self::with_probe`]); spawns nothing yet.
    pub fn new(fs: Arc<dyn Fs>, sink: Sink) -> Self {
        Self {
            fs,
            current: Arc::new(AtomicU64::new(0)),
            sink,
            probe: None,
            cache: Arc::default(),
            recheck: Arc::default(),
        }
    }

    /// If the current folder is on the network, relists it and sends differences (call when
    /// Arke regains focus). Returns without touching the network; listing runs in the background.
    pub fn recheck(&self) {
        if let Some(tx) = self.recheck.lock().as_ref() {
            let _ = tx.try_send(());
        }
    }

    /// Enables network handling (server probing, remembering network folders).
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn NetProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Generation of the latest [`Self::navigate`]; events with other generations are stale.
    pub fn current_gen(&self) -> Gen {
        self.current.load(Ordering::Acquire)
    }

    /// Starts listing `path`. Returns the new generation immediately and does no I/O itself.
    pub fn navigate(&self, path: PathBuf, opts: ListOptions) -> Gen {
        let generation = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        *self.recheck.lock() = None;
        let job = Job {
            generation,
            fs: self.fs.clone(),
            current: self.current.clone(),
            sink: self.sink.clone(),
            probe: self.probe.clone(),
            cache: self.cache.clone(),
            recheck: self.recheck.clone(),
            outcome: Arc::new(AtomicU8::new(OUTCOME_PENDING)),
        };
        let requested_at = Instant::now();
        let spawn = std::thread::Builder::new()
            .name(format!("list-{generation}"))
            .spawn(move || {
                // A bug in background work must not take down the app: a panic becomes a listing
                // failure.
                let failed = job.clone();
                let shown = path.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job.list_dir(path, opts, requested_at);
                }));
                if result.is_err() {
                    failed.send(IoEvent::ListFailed {
                        generation,
                        path: shown,
                        error: t!("io.internal").into_owned(),
                        kind: FailKind::Other,
                    });
                }
            });
        if let Err(err) = spawn {
            log::error!("failed to spawn listing thread: {err}");
        }
        generation
    }

    /// Invalidates all current background work (e.g. when closing the window).
    pub fn cancel(&self) {
        self.current.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for DirService {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone)]
struct Job {
    generation: Gen,
    fs: Arc<dyn Fs>,
    current: Arc<AtomicU64>,
    sink: Sink,
    probe: Option<Arc<dyn NetProbe>>,
    cache: Arc<Mutex<NetCache>>,
    recheck: Arc<Mutex<Option<flume::Sender<()>>>>,
    /// Whether probe or listing decided first; if the probe found the server down, the listing is
    /// not sent.
    outcome: Arc<AtomicU8>,
}

impl Job {
    fn is_stale(&self) -> bool {
        self.current.load(Ordering::Acquire) != self.generation
    }

    fn send(&self, event: IoEvent) {
        if !self.is_stale() {
            (self.sink)(event);
        }
    }

    /// Whether the listing may still be sent (not if the probe already failed). The first send
    /// marks it listed.
    fn claim_listing(&self) -> bool {
        match self.outcome.compare_exchange(
            OUTCOME_PENDING,
            OUTCOME_LISTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(now) => now == OUTCOME_LISTED,
        }
    }

    /// Reports failure (from probe or listing; only the first verdict is sent).
    fn fail(&self, path: PathBuf, error: String, kind: FailKind) {
        if self
            .outcome
            .compare_exchange(
                OUTCOME_PENDING,
                OUTCOME_FAILED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.send(IoEvent::ListFailed {
                generation: self.generation,
                path,
                error,
                kind,
            });
        }
    }

    /// Whether `path` is a file (or a link to one) rather than a folder.
    fn is_file(&self, path: &Path) -> bool {
        !self.is_stale() && self.fs.stat(path).is_ok_and(|e| !e.is_dir_like())
    }

    /// Reports that the path is a file (unless the probe already failed it).
    fn found_file(&self, path: PathBuf) {
        if self
            .outcome
            .compare_exchange(
                OUTCOME_PENDING,
                OUTCOME_FAILED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.send(IoEvent::IsFile {
                generation: self.generation,
                path,
            });
        }
    }

    /// Network folder: sends the last contents first while probing the server, failing fast if
    /// it is unreachable. Returns whether cached contents were sent.
    fn start_network(
        &self,
        path: &Path,
        unc: &Path,
        opts: ListOptions,
        requested_at: Instant,
    ) -> bool {
        let cached = self.cache.lock().get(path, opts.show_hidden);
        let shown = if let Some((entries, sorted_by, hidden_count)) = cached {
            let entries = if sorted_by == opts.sort {
                entries
            } else {
                Arc::new(resort(&entries, opts.sort))
            };
            self.send(IoEvent::Listing {
                generation: self.generation,
                path: path.to_path_buf(),
                entries,
                sorted_by: opts.sort,
                complete: false,
                hidden_count,
                timing: ListTiming {
                    total: requested_at.elapsed(),
                    ..ListTiming::default()
                },
                cached: true,
            });
            true
        } else {
            false
        };
        if let (Some(probe), Some(server)) = (self.probe.clone(), crate::net::server_of(unc)) {
            let job = self.clone();
            let path = path.to_path_buf();
            let _ = std::thread::Builder::new()
                .name(format!("probe-{}", self.generation))
                .spawn(move || {
                    let reach = probe.reach(&server);
                    if let Some(error) = crate::net::describe_reach(reach, &server) {
                        log::info!("{server} unreachable ({reach:?}), not waiting for listing");
                        job.fail(path, error, FailKind::Offline);
                    }
                });
        }
        shown
    }

    /// Starts listing `path`: its entries, or for `\\server` its shares (there is no folder to
    /// list; the bool says so). On failure reports it (or that `path` is a file) and returns
    /// `None`.
    fn open_listing(&self, path: &Path, network: bool) -> Option<(crate::fs::DirIter, bool)> {
        let server = self.probe.as_ref().zip(crate::net::share_server(path));
        let listed = match server {
            Some((probe, server)) => probe.shares(server).map(|shares| -> crate::fs::DirIter {
                Box::new(crate::net::share_entries(&shares).into_iter().map(Ok))
            }),
            None => self.fs.read_dir(path),
        };
        match listed {
            Ok(iter) => Some((iter, server.is_some())),
            Err(err) => {
                let (error, kind) = crate::net::describe(&err, network);
                // A file (a path typed or pasted in full): the UI opens it instead.
                if kind == FailKind::Other && server.is_none() && self.is_file(path) {
                    self.found_file(path.to_path_buf());
                } else {
                    self.fail(path.to_path_buf(), error, kind);
                }
                None
            }
        }
    }

    fn list_dir(self, path: PathBuf, opts: ListOptions, requested_at: Instant) {
        let generation = self.generation;
        let unc = self.probe.as_ref().and_then(|p| p.unc(&path));
        let network = unc.is_some();
        let showed_cache = unc
            .as_deref()
            .is_some_and(|unc| self.start_network(&path, unc, opts, requested_at));
        let start = Instant::now();
        let Some((iter, shares)) = self.open_listing(&path, network) else {
            return;
        };
        if self.is_stale() || !self.claim_listing() {
            return;
        }
        // A server's share list can't be watched (F5 relists it).
        let opts = ListOptions {
            watch: opts.watch && !shares,
            ..opts
        };

        // Start watching during the listing so changes made meanwhile aren't missed
        // (the UI queues them until the full snapshot arrives).
        if opts.watch {
            let job = self.clone();
            let dir = path.clone();
            let _ = std::thread::Builder::new()
                .name(format!("watch-{generation}"))
                .spawn(move || job.watch_dir(&dir, opts.show_hidden));
        }

        let mut entries: Vec<Entry> = Vec::new();
        let mut hidden_count = 0usize;
        let mut last_snapshot = start;
        for (i, item) in iter.enumerate() {
            if i % 512 == 0 && self.is_stale() {
                return;
            }
            let Ok(entry) = item else { continue };
            if entry.concealed(opts.show_hidden) {
                hidden_count += 1;
                continue;
            }
            entries.push(entry);

            // Cached contents already shown: skip partial snapshots (the list would shrink and grow
            // again) and wait for the full listing.
            if !showed_cache && last_snapshot.elapsed() >= SNAPSHOT_INTERVAL {
                last_snapshot = Instant::now();
                let mut partial = entries.clone();
                let sort_start = Instant::now();
                opts.sort.sort(&mut partial);
                self.send(IoEvent::Listing {
                    generation,
                    path: path.clone(),
                    entries: Arc::new(partial),
                    sorted_by: opts.sort,
                    complete: false,
                    hidden_count,
                    timing: ListTiming {
                        enumerate: start.elapsed(),
                        sort: sort_start.elapsed(),
                        total: requested_at.elapsed(),
                    },
                    cached: false,
                });
            }
        }
        let enumerate = start.elapsed();
        let sort_start = Instant::now();
        opts.sort.sort(&mut entries);
        let sort = sort_start.elapsed();
        log::info!(
            "listed {} entries ({hidden_count} hidden): enumerate {:.1} ms, sort {:.1} ms, {}",
            entries.len(),
            enumerate.as_secs_f64() * 1e3,
            sort.as_secs_f64() * 1e3,
            path.display()
        );
        let entries = Arc::new(entries);
        if network {
            self.remember_network(&path, &entries, opts, hidden_count);
        }
        self.send(IoEvent::Listing {
            generation,
            path,
            entries,
            sorted_by: opts.sort,
            complete: true,
            hidden_count,
            timing: ListTiming {
                enumerate,
                sort,
                total: requested_at.elapsed(),
            },
            cached: false,
        });
    }

    fn watch_dir(self, dir: &Path, show_hidden: bool) {
        let (tx, rx) = mpsc::channel::<WatchEvent>();
        let tx = Mutex::new(tx);
        let _guard = match self.fs.watch(
            dir,
            Box::new(move |event| {
                let _ = tx.lock().send(event);
            }),
        ) {
            Ok(guard) => guard,
            Err(err) => {
                log::warn!("cannot watch {}: {err}", dir.display());
                return;
            }
        };

        let mut pending: HashSet<OsString> = HashSet::new();
        let mut first_pending: Option<Instant> = None;
        let mut last_event = Instant::now();
        let mut rescan = false;

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WatchEvent::Changed(paths)) => {
                    for p in &paths {
                        if p.parent() == Some(dir)
                            && let Some(name) = p.file_name()
                        {
                            pending.insert(name.to_os_string());
                        }
                    }
                    last_event = Instant::now();
                    first_pending.get_or_insert(last_event);
                }
                Ok(WatchEvent::Rescan) => {
                    rescan = true;
                    last_event = Instant::now();
                    first_pending.get_or_insert(last_event);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if self.is_stale() {
                return; // dropping _guard stops the watch
            }
            let Some(first) = first_pending else { continue };
            let quiet = last_event.elapsed() >= WATCH_DEBOUNCE;
            if !(quiet || first.elapsed() >= WATCH_MAX_LATENCY) {
                continue;
            }
            first_pending = None;
            if rescan || pending.len() > WATCH_MAX_BATCH {
                pending.clear();
                rescan = false;
                self.send(IoEvent::NeedsRescan {
                    generation: self.generation,
                });
                continue;
            }
            let delta = self.stat_names(dir, pending.drain(), show_hidden);
            if !delta.is_empty() {
                self.send(IoEvent::Delta {
                    generation: self.generation,
                    delta,
                });
            }
        }
    }

    /// Network folder listed: remember it (shown first next time) and set up the recheck.
    fn remember_network(
        &self,
        path: &Path,
        entries: &Arc<Vec<Entry>>,
        opts: ListOptions,
        hidden_count: usize,
    ) {
        if opts.watch && entries.len() <= NET_RECHECK_MAX_ENTRIES {
            let (tx, rx) = flume::bounded(1);
            *self.recheck.lock() = Some(tx);
            let job = self.clone();
            let dir = path.to_path_buf();
            let seen = entries.clone();
            let _ = std::thread::Builder::new()
                .name(format!("recheck-{}", self.generation))
                .spawn(move || job.recheck_dir(&dir, &seen, opts.show_hidden, &rx));
        }
        if entries.len() <= NET_CACHE_MAX_ENTRIES {
            self.cache.lock().put(
                path,
                Cached {
                    entries: entries.clone(),
                    sorted_by: opts.sort,
                    show_hidden: opts.show_hidden,
                    rules: crate::entry::hidden_rules_version(),
                    hidden_count,
                },
            );
        }
    }

    /// Network folder recheck: on each signal, relist once and send only differences.
    /// Skipped while the server is unreachable (the view is not cleared).
    fn recheck_dir(
        self,
        dir: &Path,
        first: &[Entry],
        show_hidden: bool,
        wake: &flume::Receiver<()>,
    ) {
        type Fingerprint = (EntryKindKey, u64, Option<std::time::SystemTime>, u32);
        type EntryKindKey = u8;
        let fingerprint = |e: &Entry| -> Fingerprint {
            let kind = match e.kind {
                crate::entry::EntryKind::Dir => 0,
                crate::entry::EntryKind::File => 1,
                crate::entry::EntryKind::Link { .. } => 2,
            };
            (kind, e.size, e.modified, e.attributes)
        };
        let mut last: HashMap<Arc<str>, Fingerprint> = first
            .iter()
            .map(|e| (e.name.clone(), fingerprint(e)))
            .collect();
        loop {
            // Wait for a signal; quit once the folder changes.
            match wake.recv_timeout(Duration::from_millis(200)) {
                Ok(()) => {}
                Err(flume::RecvTimeoutError::Timeout) => {
                    if self.is_stale() {
                        return;
                    }
                    continue;
                }
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
            if self.is_stale() {
                return;
            }
            let Ok(iter) = self.fs.read_dir(dir) else {
                continue;
            };
            let mut now: HashMap<Arc<str>, Fingerprint> = HashMap::with_capacity(last.len());
            let mut upserts = Vec::new();
            for entry in iter.filter_map(Result::ok) {
                if self.is_stale() {
                    return;
                }
                if entry.concealed(show_hidden) {
                    continue;
                }
                let fp = fingerprint(&entry);
                if last.get(&entry.name) != Some(&fp) {
                    upserts.push(entry.clone());
                }
                now.insert(entry.name.clone(), fp);
                if now.len() > NET_RECHECK_MAX_ENTRIES {
                    return;
                }
            }
            let removed: Vec<Arc<str>> = last
                .keys()
                .filter(|name| !now.contains_key(*name))
                .cloned()
                .collect();
            last = now;
            if upserts.is_empty() && removed.is_empty() {
                continue;
            }
            self.send(IoEvent::Delta {
                generation: self.generation,
                delta: Delta { upserts, removed },
            });
        }
    }

    fn stat_names(
        &self,
        dir: &Path,
        names: impl Iterator<Item = OsString>,
        show_hidden: bool,
    ) -> Delta {
        let mut delta = Delta {
            upserts: Vec::new(),
            removed: Vec::new(),
        };
        for name in names {
            let full = dir.join(&name);
            let name = name.to_string_lossy();
            match self.fs.stat(&full) {
                Ok(entry) if !entry.concealed(show_hidden) => delta.upserts.push(entry),
                // Gone, or now hidden: remove from the list.
                _ => delta.removed.push(name.into()),
            }
        }
        delta
    }
}

/// Localized message for an I/O error (common kinds get friendlier text).
pub(crate) fn describe_io_error(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => t!("io.not_found").into_owned(),
        ErrorKind::PermissionDenied => t!("io.denied").into_owned(),
        ErrorKind::NotADirectory => t!("io.not_dir").into_owned(),
        _ => err.to_string(),
    }
}

/// Re-sorts an existing snapshot (the UI calls this on a background executor).
pub fn resort(entries: &[Entry], spec: SortSpec) -> Vec<Entry> {
    let mut v = entries.to_vec();
    spec.sort(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeFs;
    use crate::fs::RealFs;

    type Events = Arc<Mutex<Vec<IoEvent>>>;

    fn service(fs: Arc<dyn Fs>) -> (DirService, Events) {
        let events: Events = Arc::default();
        let ev2 = events.clone();
        let svc = DirService::new(fs, Arc::new(move |e| ev2.lock().push(e)));
        (svc, events)
    }

    fn opts(watch: bool) -> ListOptions {
        ListOptions {
            sort: SortSpec::default(),
            show_hidden: true,
            watch,
        }
    }

    fn wait_for(events: &Events, timeout: Duration, pred: impl Fn(&IoEvent) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if events.lock().iter().any(&pred) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn complete_listing_of(events: &Events, path: &Path) -> Option<usize> {
        events.lock().iter().find_map(|e| match e {
            IoEvent::Listing {
                path: p,
                complete: true,
                entries,
                ..
            } if p == path => Some(entries.len()),
            _ => None,
        })
    }

    /// Core guarantee: with the file system hung forever, navigate still returns immediately,
    /// later navigations get results, and the stale result is never sent after release.
    #[test]
    fn hung_directory_never_blocks_caller_or_later_navigation() {
        let fs = FakeFs::new();
        let hung = PathBuf::from(r"\\dead-server\share");
        let fast = PathBuf::from(r"C:\fast");
        fs.insert_many(&hung, 10);
        fs.insert_many(&fast, 3);
        fs.hang(&hung);
        let (svc, events) = service(fs.clone());

        let t = Instant::now();
        svc.navigate(hung.clone(), opts(true));
        let gen_fast = svc.navigate(fast.clone(), opts(true));
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "navigate must not wait on I/O"
        );

        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { generation, complete: true, .. } if *generation == gen_fast
        )));
        assert_eq!(complete_listing_of(&events, &fast), Some(3));

        // Release the hung directory: the result is stale and must be dropped.
        fs.release(&hung);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(complete_listing_of(&events, &hung), None);
        assert!(
            !events
                .lock()
                .iter()
                .any(|e| matches!(e, IoEvent::ListFailed { .. })),
        );
    }

    /// Test probe: paths starting with `\\` are network locations; the test decides reachability.
    /// The shares answer is the share list or a Windows error code.
    struct FakeProbe(
        Mutex<crate::net::Reach>,
        Mutex<Result<Vec<crate::net::Share>, i32>>,
    );

    impl NetProbe for FakeProbe {
        fn unc(&self, path: &Path) -> Option<PathBuf> {
            path.to_string_lossy()
                .starts_with(r"\\")
                .then(|| path.to_path_buf())
        }
        fn reach(&self, _server: &str) -> crate::net::Reach {
            *self.0.lock()
        }
        fn shares(&self, _server: &str) -> std::io::Result<Vec<crate::net::Share>> {
            self.1
                .lock()
                .clone()
                .map_err(std::io::Error::from_raw_os_error)
        }
    }

    fn network_service(
        fs: Arc<dyn Fs>,
        reach: crate::net::Reach,
    ) -> (DirService, Events, Arc<FakeProbe>) {
        let probe = Arc::new(FakeProbe(Mutex::new(reach), Mutex::new(Ok(Vec::new()))));
        let (svc, events) = service(fs);
        (svc.with_probe(probe.clone()), events, probe)
    }

    /// Unreachable server: don't wait for the hung listing (OS timeout is tens of seconds);
    /// fail right away and allow retry.
    #[test]
    fn unreachable_server_fails_fast() {
        let fs = FakeFs::new();
        let share = PathBuf::from(r"\\sleeping-nas\share");
        fs.insert_many(&share, 3);
        fs.hang(&share);
        let (svc, events, _) = network_service(fs.clone(), crate::net::Reach::Down);
        let started = Instant::now();
        let generation = svc.navigate(share.clone(), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { generation: g, kind: FailKind::Offline, .. } if *g == generation
        )));
        assert!(started.elapsed() < Duration::from_secs(1));
        // The hung listing comes back later: failure was already reported, nothing more is sent.
        fs.release(&share);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(complete_listing_of(&events, &share), None);
    }

    /// Local folders are neither probed nor remembered.
    #[test]
    fn local_folders_are_not_probed() {
        let fs = FakeFs::new();
        let local = PathBuf::from(r"C:\local");
        fs.insert_many(&local, 2);
        let (svc, events, _) = network_service(fs, crate::net::Reach::Down);
        svc.navigate(local.clone(), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { complete: true, .. }
        )));
        assert_eq!(complete_listing_of(&events, &local), Some(2));
    }

    /// Visited network folders show their last contents first, even if the server hangs or is gone.
    #[test]
    fn network_folders_show_their_last_contents_first() {
        let fs = FakeFs::new();
        let share = PathBuf::from(r"\\nas\share\docs");
        fs.insert_many(&share, 4);
        let (svc, events, probe) = network_service(fs.clone(), crate::net::Reach::Up);
        svc.navigate(share.clone(), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { complete: true, .. }
        )));
        // No cached contents the first time.
        assert!(
            !events
                .lock()
                .iter()
                .any(|e| matches!(e, IoEvent::Listing { cached: true, .. }))
        );

        // Revisit with the server hung: cached contents show up immediately.
        events.lock().clear();
        fs.hang(&share);
        let started = Instant::now();
        let generation = svc.navigate(share.clone(), opts(false));
        assert!(wait_for(&events, Duration::from_secs(1), |e| matches!(
            e,
            IoEvent::Listing { generation: g, cached: true, complete: false, entries, .. }
                if *g == generation && entries.len() == 4
        )));
        assert!(started.elapsed() < Duration::from_millis(500));

        // Server down: failure reported (cached contents stay on screen).
        *probe.0.lock() = crate::net::Reach::Down;
        let generation = svc.navigate(share, opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { generation: g, kind: FailKind::Offline, .. } if *g == generation
        )));
        assert!(events.lock().iter().any(|e| matches!(
            e,
            IoEvent::Listing { generation: g, cached: true, .. } if *g == generation
        )));
        fs.release_all();
    }

    /// No watch events for network folders and no polling: recheck (Arke regains focus) relists
    /// and sends both additions and removals.
    #[test]
    fn network_folders_are_rechecked_on_request() {
        let fs = FakeFs::new();
        let share = PathBuf::from(r"\\nas\share");
        fs.insert_dir(
            &share,
            vec![
                Entry::new("old.txt", crate::entry::EntryKind::File, 1, None, 0),
                Entry::new("keep.txt", crate::entry::EntryKind::File, 1, None, 0),
            ],
        );
        let (svc, events, _) = network_service(fs.clone(), crate::net::Reach::Up);
        let generation = svc.navigate(share.clone(), opts(true));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { complete: true, .. }
        )));
        // No watch event, only the server-side contents changed.
        fs.remove(&share, "old.txt");
        fs.add_file(&share, "new.txt", 5);
        let calls = fs.read_dir_calls();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(fs.read_dir_calls(), calls, "no polling");
        svc.recheck();
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Delta { generation: g, delta }
                if *g == generation
                    && delta.upserts.iter().any(|u| &*u.name == "new.txt")
                    && delta.removed.iter().any(|r| &**r == "old.txt")
                    && !delta.upserts.iter().any(|u| &*u.name == "keep.txt")
        )));
        // After switching to a local folder the recheck no longer touches the network folder.
        svc.navigate(PathBuf::from(r"C:\elsewhere"), opts(false));
        std::thread::sleep(Duration::from_millis(300));
        let calls = fs.read_dir_calls();
        svc.recheck();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(fs.read_dir_calls(), calls);
    }

    /// Slow directories send partial snapshots instead of making the user wait for the full read.
    #[test]
    fn slow_directory_streams_partial_snapshots() {
        struct SlowIterFs(Arc<FakeFs>);
        impl Fs for SlowIterFs {
            fn read(&self, _: &Path, _: u64) -> std::io::Result<Vec<u8>> {
                Err(std::io::ErrorKind::Unsupported.into())
            }
            fn read_head(&self, _: &Path, _: u64) -> std::io::Result<Vec<u8>> {
                Err(std::io::ErrorKind::Unsupported.into())
            }

            fn read_dir(&self, path: &Path) -> std::io::Result<crate::fs::DirIter> {
                let inner = self.0.read_dir(path)?;
                Ok(Box::new(inner.inspect(|_| {
                    std::thread::sleep(Duration::from_millis(5));
                })))
            }
            fn stat(&self, path: &Path) -> std::io::Result<Entry> {
                self.0.stat(path)
            }
            fn watch(
                &self,
                path: &Path,
                cb: crate::fs::WatchCallback,
            ) -> std::io::Result<Box<dyn Send>> {
                self.0.watch(path, cb)
            }
        }
        let fake = FakeFs::new();
        let dir = PathBuf::from(r"\\slow\share");
        fake.insert_many(&dir, 100); // ~500ms
        let (svc, events) = service(Arc::new(SlowIterFs(fake)));
        svc.navigate(dir, opts(false));
        assert!(wait_for(&events, Duration::from_secs(3), |e| matches!(
            e,
            IoEvent::Listing { complete: false, entries, .. } if !entries.is_empty()
        )));
        assert!(wait_for(&events, Duration::from_secs(3), |e| matches!(
            e,
            IoEvent::Listing { complete: true, entries, .. } if entries.len() == 100
        )));
    }

    #[test]
    fn watch_events_become_deltas_and_stop_when_stale() {
        let fs = FakeFs::new();
        let dir = PathBuf::from(r"C:\w");
        fs.insert_many(&dir, 2);
        let (svc, events) = service(fs.clone());
        svc.navigate(dir.clone(), opts(true));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { complete: true, .. }
        )));
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs.watcher_count(&dir) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        fs.add_file(&dir, "new.txt", 1);
        fs.remove(&dir, "file_000000.txt");
        fs.emit_changed(&dir, &["new.txt", "file_000000.txt"]);
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Delta { delta, .. }
                if delta.upserts.iter().any(|u| &*u.name == "new.txt")
                && delta.removed.iter().any(|r| &**r == "file_000000.txt")
        )));

        // After navigating away the watcher must stop (drop guard).
        svc.cancel();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs.watcher_count(&dir) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs.watcher_count(&dir), 0);
    }

    /// A panic in background work fails only that listing; it must not crash the app or leave
    /// the UI loading forever.
    #[test]
    fn panic_in_listing_becomes_failure_event() {
        struct PanicFs;
        impl Fs for PanicFs {
            fn read(&self, _: &Path, _: u64) -> std::io::Result<Vec<u8>> {
                Err(std::io::ErrorKind::Unsupported.into())
            }
            fn read_head(&self, _: &Path, _: u64) -> std::io::Result<Vec<u8>> {
                Err(std::io::ErrorKind::Unsupported.into())
            }

            fn read_dir(&self, _: &Path) -> std::io::Result<crate::fs::DirIter> {
                Ok(Box::new(std::iter::once_with(
                    || -> std::io::Result<Entry> { panic!("simulated background bug") },
                )))
            }
            fn stat(&self, _: &Path) -> std::io::Result<Entry> {
                Err(std::io::ErrorKind::NotFound.into())
            }
            fn watch(
                &self,
                _: &Path,
                _: crate::fs::WatchCallback,
            ) -> std::io::Result<Box<dyn Send>> {
                Err(std::io::ErrorKind::Unsupported.into())
            }
        }
        let (svc, events) = service(Arc::new(PanicFs));
        svc.navigate(PathBuf::from(r"C:\x"), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { .. }
        )));
    }

    #[test]
    fn missing_dir_reports_failure() {
        let (svc, events) = service(FakeFs::new());
        svc.navigate(PathBuf::from(r"C:\nope"), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { .. }
        )));
    }

    /// A path to a file (typed in full, from the command line) is reported as a file, not as a
    /// listing failure; a missing path still fails.
    #[test]
    fn file_paths_are_reported_as_files() {
        let fs = FakeFs::new();
        let dir = PathBuf::from(r"\\nas\team\報告\Monthly\202611");
        fs.insert_dir(
            &dir,
            vec![Entry::new(
                "Quarterly Review — 11月專案.docx",
                crate::entry::EntryKind::File,
                10,
                None,
                0,
            )],
        );
        let file = dir.join("Quarterly Review — 11月專案.docx");
        let (svc, events, _) = network_service(fs, crate::net::Reach::Up);
        let generation = svc.navigate(file.clone(), opts(true));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::IsFile { generation: g, path } if *g == generation && *path == file
        )));
        assert!(
            !events
                .lock()
                .iter()
                .any(|e| matches!(e, IoEvent::ListFailed { .. }))
        );

        let missing = dir.join("gone.docx");
        let generation = svc.navigate(missing, opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { generation: g, .. } if *g == generation
        )));
    }

    /// `\\server` lists the server's disk shares as folders; `$` shares count as hidden.
    #[test]
    fn servers_list_their_shares() {
        use crate::net::{STYPE_DISKTREE, Share};
        let fs = FakeFs::new();
        let (svc, events, probe) = network_service(fs.clone(), crate::net::Reach::Up);
        *probe.1.lock() = Ok(vec![
            Share {
                name: "team".into(),
                kind: STYPE_DISKTREE,
            },
            Share {
                name: "C$".into(),
                kind: STYPE_DISKTREE | 0x8000_0000,
            },
            Share {
                name: "IPC$".into(),
                kind: 3 | 0x8000_0000,
            },
        ]);
        let server = PathBuf::from(r"\\nas");
        let hidden_off = ListOptions {
            show_hidden: false,
            ..opts(true)
        };
        let generation = svc.navigate(server.clone(), hidden_off);
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { generation: g, complete: true, entries, hidden_count: 1, .. }
                if *g == generation && entries.len() == 1 && &*entries[0].name == "team"
                    && entries[0].is_dir_like()
        )));
        assert_eq!(fs.watcher_count(&server), 0, "a share list isn't watched");

        let generation = svc.navigate(server.clone(), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::Listing { generation: g, complete: true, entries, .. }
                if *g == generation && entries.len() == 2
        )));

        // Not signed in: the "Sign in" button.
        *probe.1.lock() = Err(5);
        let generation = svc.navigate(server, opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { generation: g, kind: FailKind::NeedsLogin, .. }
                if *g == generation
        )));
    }

    /// An unreachable server fails fast while its share list hangs.
    #[test]
    fn unreachable_server_share_list_fails_fast() {
        struct HungProbe;
        impl NetProbe for HungProbe {
            fn unc(&self, path: &Path) -> Option<PathBuf> {
                Some(path.to_path_buf())
            }
            fn reach(&self, _server: &str) -> crate::net::Reach {
                crate::net::Reach::Down
            }
            fn shares(&self, _server: &str) -> std::io::Result<Vec<crate::net::Share>> {
                std::thread::sleep(Duration::from_secs(3));
                Ok(Vec::new())
            }
        }
        let (svc, events) = service(FakeFs::new());
        let svc = svc.with_probe(Arc::new(HungProbe));
        let started = Instant::now();
        let generation = svc.navigate(PathBuf::from(r"\\sleeping-nas"), opts(false));
        assert!(wait_for(&events, Duration::from_secs(2), |e| matches!(
            e,
            IoEvent::ListFailed { generation: g, kind: FailKind::Offline, .. } if *g == generation
        )));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[should_panic(expected = "file system call on the UI thread")]
    fn fake_fs_panics_on_ui_thread() {
        crate::fs::mark_ui_thread();
        let fs = FakeFs::new();
        let _ = fs.read_dir(Path::new(r"C:\"));
    }

    /// Real file system: only touches the temp directory.
    #[test]
    fn real_fs_lists_and_watches_temp_dir() {
        let dir = std::env::temp_dir().join(format!("arke-core-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..20 {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }
        std::fs::create_dir(dir.join("sub")).unwrap();

        let (svc, events) = service(Arc::new(RealFs));
        svc.navigate(dir.clone(), opts(true));
        assert!(wait_for(&events, Duration::from_secs(5), |e| matches!(
            e,
            IoEvent::Listing { complete: true, entries, .. }
                if entries.len() == 21 && entries[0].name.as_ref() == "sub"
        )));
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.join("new.txt"), b"y").unwrap();
        assert!(wait_for(&events, Duration::from_secs(5), |e| matches!(
            e,
            IoEvent::Delta { delta, .. } if delta.upserts.iter().any(|u| &*u.name == "new.txt")
        )));
        svc.cancel();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
