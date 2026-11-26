//! Fuzzy search in subfolders.
//!
//! - Walker thread: breadth-first `Fs::read_dir` from the root, feeding items to nucleo as
//!   it goes. Does not enter symlinks or junctions (cycles), nor a NAS's snapshots and recycle
//!   bin ([`crate::index::is_skipped_dir`]); skips hidden items by default. On a file server
//!   several folders are listed (and files read) at once, since each waits on a round trip.
//! - Coordinator thread: owns `nucleo::Nucleo`, takes queries, calls `tick` and sends the top
//!   [`MAX_HITS`] as a snapshot to the UI. `tick` may briefly wait on workers when the query
//!   changes, so it never runs on the UI thread.
//! - Every `start` / `set_query` bumps the generation; the UI only accepts the latest one.
//! - The query uses the search box syntax ([`crate::index_query`]): filters are applied while
//!   walking (changing them rewalks), the name text goes to nucleo. With `content:`, files that
//!   pass the filters are read by a few worker threads ([`crate::content`]) and only matches are
//!   added, each with its first matching line.
//! - The name text matches as [`Query::names`] says (#396): exact searches use substring terms;
//!   [`NameMatch::Similar`] matches fuzzily and lists the exact matches first, the others after
//!   them ([`SearchResults::similar`]). Switching between them only reparses, no rewalk.

use std::borrow::Cow;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Nucleo};

use crate::content::ContentMatcher;
use crate::entry::{Entry, EntryKind};
use crate::filter::entry_accepted;
use crate::fs::Fs;
use crate::index::IndexedTree;
use crate::index_query::{Clock, NameMatch, Query, exact_text, fuzzy_text};
use crate::service::Gen;

/// Maximum hits sent to the UI (top by score).
pub const MAX_HITS: usize = 5_000;
/// Cap on items walked per search, so a whole drive is not walked forever.
pub const MAX_ITEMS: u64 = 2_000_000;
/// Minimum interval between result sends.
const SEND_INTERVAL: Duration = Duration::from_millis(60);
/// Threads reading files for a content search (disk bound; more mostly adds seeking).
const CONTENT_THREADS: usize = 4;
/// Folders listed, and files read for a content search, at once on a file server: each waits on
/// a round trip, not on a disk here. Enough to hide the latency without crowding the server.
const NET_THREADS: usize = 8;

/// One fuzzy search match.
#[derive(Debug)]
pub struct SearchHit {
    /// Path relative to the search root, e.g. `src\main.rs`.
    pub rel: Arc<str>,
    /// The matched entry as listed.
    pub entry: Entry,
    /// First matching line, for a content search.
    pub snippet: Option<Arc<str>>,
}

/// Snapshot of the current matches, sent to the [`SearchSink`] whenever they change.
#[derive(Debug, Clone)]
pub struct SearchResults {
    /// Generation of the `start` / `set_query` call this belongs to.
    pub generation: Gen,
    /// Folder the search walks.
    pub root: PathBuf,
    /// Top matches by score, at most [`MAX_HITS`].
    pub hits: Arc<Vec<Arc<SearchHit>>>,
    /// Total matches (may exceed `hits.len()`).
    pub matched: u32,
    /// Items walked so far.
    pub scanned: u64,
    /// Folders that could not be read (access denied etc.).
    pub errors: u32,
    /// Still walking or matching; false means this is the final result for the query.
    pub searching: bool,
    /// Walk stopped early at the [`MAX_ITEMS`] cap.
    pub truncated: bool,
    /// Searching file contents (`content:` with text).
    pub contents: bool,
    /// Files whose contents were read so far.
    pub read: u64,
    /// Hits at the end of `hits` that only match fuzzily, after the exact ones (with
    /// [`NameMatch::Similar`]; 0 otherwise).
    pub similar: usize,
}

/// Receives result snapshots on the coordinator thread.
pub type SearchSink = Arc<dyn Fn(SearchResults) + Send + Sync>;

/// Everything below a folder (any depth) from an index: `(folder, show hidden, cancel)`, `None`
/// when no index covers the folder (see [`crate::index_service::IndexService::items_under`]).
pub type TreeSource =
    Arc<dyn Fn(&std::path::Path, bool, &AtomicBool) -> Option<IndexedTree> + Send + Sync>;

enum Cmd {
    Start {
        generation: Gen,
        root: PathBuf,
        show_hidden: bool,
        query: Query,
    },
    Query {
        generation: Gen,
        query: Query,
    },
    Stop,
    Wake,
}

/// Background fuzzy search. Dropping it stops all background work.
pub struct SearchService {
    cmd: flume::Sender<Cmd>,
    generation: AtomicU64,
    /// What the coordinator thread takes, until the first search starts it: each tab has a
    /// search, and most tabs never search.
    unstarted: parking_lot::Mutex<Option<Unstarted>>,
}

struct Unstarted {
    fs: Arc<dyn Fs>,
    sink: SearchSink,
    index: Option<TreeSource>,
    rx: flume::Receiver<Cmd>,
}

impl SearchService {
    /// A search with no thread yet; the first [`Self::start`] starts it.
    pub fn new(fs: Arc<dyn Fs>, sink: SearchSink) -> Self {
        Self::with_index(fs, sink, None)
    }

    /// Like [`Self::new`], but a search below a network folder that `index` covers takes the
    /// folder's items from it instead of walking the share.
    pub fn with_index(fs: Arc<dyn Fs>, sink: SearchSink, index: Option<TreeSource>) -> Self {
        let (cmd, rx) = flume::unbounded();
        Self {
            cmd,
            generation: AtomicU64::new(0),
            unstarted: parking_lot::Mutex::new(Some(Unstarted {
                fs,
                sink,
                index,
                rx,
            })),
        }
    }

    /// Starts the coordinator thread, the first time. Commands sent before wait in its channel.
    fn run(&self) {
        let Some(Unstarted {
            fs,
            sink,
            index,
            rx,
        }) = self.unstarted.lock().take()
        else {
            return;
        };
        let wake_tx = self.cmd.downgrade();
        let spawned = std::thread::Builder::new()
            .name("search".into())
            .spawn(move || coordinator(&fs, index, &sink, &rx, &wake_tx));
        if let Err(err) = spawned {
            log::error!("failed to spawn search thread: {err}");
        }
    }

    fn next_gen(&self) -> Gen {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Generation of the latest `start` / `set_query` / `stop`; older results are stale.
    pub fn current_gen(&self) -> Gen {
        self.generation.load(Ordering::SeqCst)
    }

    /// Starts a new search from `root` (fresh walk). `clock` is for date filters. Returns
    /// immediately.
    pub fn start(&self, root: PathBuf, show_hidden: bool, query: &str, clock: Clock) -> Gen {
        self.start_parsed(root, show_hidden, Query::parse(query, clock))
    }

    /// [`Self::start`] with a parsed query (to choose how names match, [`Query::names`]).
    pub fn start_parsed(&self, root: PathBuf, show_hidden: bool, query: Query) -> Gen {
        self.run();
        let generation = self.next_gen();
        let _ = self.cmd.send(Cmd::Start {
            generation,
            root,
            show_hidden,
            query,
        });
        generation
    }

    /// Changes the query. Reuses walked items when only the name text changed; new filters
    /// rewalk. Returns immediately.
    pub fn set_query(&self, query: &str, clock: Clock) -> Gen {
        self.set_parsed(Query::parse(query, clock))
    }

    /// [`Self::set_query`] with a parsed query. A new [`Query::names`] alone only reparses.
    pub fn set_parsed(&self, query: Query) -> Gen {
        let generation = self.next_gen();
        if self.unstarted.lock().is_some() {
            // No search ran yet: nothing to change.
            return generation;
        }
        let _ = self.cmd.send(Cmd::Query { generation, query });
        generation
    }

    /// Stops walking and matching; results already sent become stale.
    pub fn stop(&self) {
        self.next_gen();
        if self.unstarted.lock().is_some() {
            // No search ran yet: nothing to stop.
            return;
        }
        let _ = self.cmd.send(Cmd::Stop);
    }
}

struct Walk {
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
}

impl Drop for Walk {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Progress {
    scanned: AtomicU64,
    read: AtomicU64,
    errors: AtomicU32,
    done: AtomicBool,
    truncated: AtomicBool,
}

struct Active {
    generation: Gen,
    root: PathBuf,
    show_hidden: bool,
    query: Query,
    walk: Walk,
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// Coordinator thread state.
struct Coordinator {
    fs: Arc<dyn Fs>,
    index: Option<TreeSource>,
    notify: Notify,
    wake_pending: Arc<AtomicBool>,
    nucleo: Option<Nucleo<Arc<SearchHit>>>,
    active: Option<Active>,
    /// Changed but not sent yet.
    dirty: bool,
    last_sent: Option<Instant>,
    /// Walk and match done and final result sent: block for the next command.
    idle: bool,
}

fn coordinator(
    fs: &Arc<dyn Fs>,
    index: Option<TreeSource>,
    sink: &SearchSink,
    rx: &flume::Receiver<Cmd>,
    wake_tx: &flume::WeakSender<Cmd>,
) {
    // nucleo calls notify for every pushed item; a flag coalesces them into at most one pending
    // Wake.
    let wake_pending = Arc::new(AtomicBool::new(false));
    let notify: Notify = {
        let wake_tx = wake_tx.clone();
        let pending = Arc::clone(&wake_pending);
        Arc::new(move || {
            if !pending.swap(true, Ordering::AcqRel)
                && let Some(tx) = wake_tx.upgrade()
            {
                let _ = tx.send(Cmd::Wake);
            }
        })
    };
    let mut c = Coordinator {
        fs: Arc::clone(fs),
        index,
        notify,
        wake_pending,
        nucleo: None,
        active: None,
        dirty: false,
        last_sent: None,
        idle: true,
    };
    loop {
        let cmd = if c.active.is_some() && !c.idle {
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(cmd) => Some(cmd),
                Err(flume::RecvTimeoutError::Timeout) => None,
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return,
            }
        };
        // Drain all queued commands so fast typing only matches the last query.
        let mut next = cmd;
        while let Some(cmd) = next.take() {
            c.handle(cmd);
            next = rx.try_recv().ok();
        }
        c.tick(sink);
    }
}

impl Coordinator {
    fn handle(&mut self, cmd: Cmd) {
        self.idle = false;
        match cmd {
            Cmd::Start {
                generation,
                root,
                show_hidden,
                query,
            } => self.restart(generation, root, show_hidden, query),
            Cmd::Query { generation, query } => {
                let Some(a) = self.active.as_mut() else {
                    return;
                };
                if !a.query.same_filters(&query) {
                    // Filters decide what the walk adds: walk again.
                    let (root, show_hidden) = (a.root.clone(), a.show_hidden);
                    self.restart(generation, root, show_hidden, query);
                    return;
                }
                if let Some(nucleo) = self.nucleo.as_mut() {
                    // Only appended to the previous text: nucleo can refine the last results.
                    {
                        let (old, new) = (pattern_text(&a.query), pattern_text(&query));
                        let append = !old.is_empty()
                            && a.query.names == query.names
                            && new.starts_with(&*old)
                            && is_path_query(&new) == is_path_query(&old);
                        reparse(nucleo, &new, append);
                    }
                    a.query = query;
                    a.generation = generation;
                    self.dirty = true;
                    self.last_sent = None;
                }
            }
            Cmd::Stop => {
                self.active = None;
                if let Some(nucleo) = self.nucleo.as_mut() {
                    nucleo.restart(true);
                }
            }
            Cmd::Wake => {
                self.wake_pending.store(false, Ordering::Release);
                self.dirty = true;
            }
        }
    }

    /// Fresh walk of `root` for `query`.
    fn restart(&mut self, generation: Gen, root: PathBuf, show_hidden: bool, query: Query) {
        // Stop the old walk first (dropping Walk cancels it), then clear nucleo.
        drop(self.active.take());
        let notify = Arc::clone(&self.notify);
        let nucleo = self.nucleo.get_or_insert_with(|| {
            Nucleo::new(Config::DEFAULT.match_paths(), notify, None, COLUMNS)
        });
        nucleo.restart(true);
        reparse(nucleo, &pattern_text(&query), false);
        let walk = spawn_walk(
            Arc::clone(&self.fs),
            WalkSpec {
                root: root.clone(),
                show_hidden,
                query: query.clone(),
                index: self.index.clone(),
            },
            nucleo.injector(),
            Arc::clone(&self.notify),
        );
        self.active = Some(Active {
            generation,
            root,
            show_hidden,
            query,
            walk,
        });
        self.dirty = true;
        self.last_sent = None;
    }

    fn tick(&mut self, sink: &SearchSink) {
        let (Some(a), Some(nucleo)) = (self.active.as_ref(), self.nucleo.as_mut()) else {
            return;
        };
        let status = nucleo.tick(10);
        let walking = !a.walk.progress.done.load(Ordering::Acquire);
        self.dirty |= status.changed || walking;
        let settled = !walking && !status.running;
        let due = self.last_sent.is_none_or(|t| t.elapsed() >= SEND_INTERVAL);
        if self.dirty && (due || settled) {
            sink(snapshot(nucleo, a, !settled));
            self.last_sent = Some(Instant::now());
            self.dirty = false;
        }
        if settled && !self.dirty {
            // Keep nucleo's items (a new query needn't rewalk) and wait for the next command.
            self.idle = true;
        }
    }
}

/// Match columns: 0 = file name, 1 = relative path.
const COL_NAME: usize = 0;
const COL_PATH: usize = 1;
const COLUMNS: u32 = 2;

/// A query with a path separator (e.g. `src\main`) matches the relative path, otherwise only the
/// file name. Name-only matching keeps out deep files whose path happens to contain the letters in
/// order.
fn is_path_query(query: &str) -> bool {
    query.contains(['\\', '/'])
}

/// What nucleo matches for the query's name text, as [`Query::names`] says.
fn pattern_text(query: &Query) -> Cow<'_, str> {
    match query.names {
        NameMatch::Any => Cow::Borrowed(&query.text),
        NameMatch::Exact => Cow::Owned(exact_text(&query.text)),
        NameMatch::Similar => Cow::Owned(fuzzy_text(&query.text)),
    }
}

/// `hits` (in score order) with the exact matches of `text` first, then the others, and how many
/// the others are.
fn exact_first(hits: Vec<Arc<SearchHit>>, text: &str) -> (Vec<Arc<SearchHit>>, usize) {
    let path = is_path_query(text);
    let pattern = nucleo::pattern::Pattern::parse(
        &exact_text(text),
        CaseMatching::Smart,
        Normalization::Smart,
    );
    let mut matcher = nucleo::Matcher::new(Config::DEFAULT.match_paths());
    let mut buf = Vec::new();
    let (mut exact, similar): (Vec<_>, Vec<_>) = hits.into_iter().partition(|hit| {
        let haystack: &str = if path { &hit.rel } else { &hit.entry.name };
        pattern
            .score(nucleo::Utf32Str::new(haystack, &mut buf), &mut matcher)
            .is_some()
    });
    let count = similar.len();
    exact.extend(similar);
    (exact, count)
}

fn reparse(nucleo: &mut Nucleo<Arc<SearchHit>>, query: &str, append: bool) {
    let (active, inactive) = if is_path_query(query) {
        (COL_PATH, COL_NAME)
    } else {
        (COL_NAME, COL_PATH)
    };
    let pattern = &mut nucleo.pattern;
    pattern.reparse(
        active,
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        append,
    );
    pattern.reparse(
        inactive,
        "",
        CaseMatching::Smart,
        Normalization::Smart,
        false,
    );
}

fn snapshot(nucleo: &Nucleo<Arc<SearchHit>>, a: &Active, searching: bool) -> SearchResults {
    let snap = nucleo.snapshot();
    // Blank query: nucleo would match everything; show nothing instead. Filters alone list what
    // passed them.
    let (hits, matched) = if a.query.is_empty() {
        (Vec::new(), 0)
    } else {
        let matched = snap.matched_item_count();
        let take = matched.min(MAX_HITS as u32);
        let mut hits: Vec<Arc<SearchHit>> = snap
            .matched_items(0..take)
            .map(|item| Arc::clone(item.data))
            .collect();
        // No name to rank by: newest first, like "This PC". Content hits stay in the order
        // found so the list doesn't jump while files are read.
        if a.query.text.trim().is_empty() && a.query.content_needle().is_none() {
            hits.sort_by_key(|h| std::cmp::Reverse(h.entry.modified));
        }
        (hits, matched)
    };
    let similar_wanted = a.query.names == NameMatch::Similar && !a.query.text.trim().is_empty();
    let (hits, similar) = if similar_wanted {
        exact_first(hits, &a.query.text)
    } else {
        (hits, 0)
    };
    let p = &a.walk.progress;
    SearchResults {
        generation: a.generation,
        root: a.root.clone(),
        hits: Arc::new(hits),
        matched,
        scanned: p.scanned.load(Ordering::Relaxed),
        errors: p.errors.load(Ordering::Relaxed),
        searching,
        truncated: p.truncated.load(Ordering::Relaxed),
        contents: a.query.content_needle().is_some(),
        read: p.read.load(Ordering::Relaxed),
        similar,
    }
}

/// What one walk looks for.
struct WalkSpec {
    root: PathBuf,
    show_hidden: bool,
    query: Query,
    /// Takes the place of the walk where it covers the root.
    index: Option<TreeSource>,
}

fn spawn_walk(
    fs: Arc<dyn Fs>,
    spec: WalkSpec,
    injector: Injector<Arc<SearchHit>>,
    notify: Notify,
) -> Walk {
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Progress::default());
    let walk = Walk {
        cancel: Arc::clone(&cancel),
        progress: Arc::clone(&progress),
    };
    let spawned = std::thread::Builder::new()
        .name("search-walk".into())
        .spawn(move || {
            walk_tree(&fs, &spec, &injector, &cancel, &progress);
            progress.done.store(true, Ordering::Release);
            notify();
        });
    if let Err(err) = spawned {
        log::error!("failed to spawn search walker thread: {err}");
        walk.progress.done.store(true, Ordering::Release);
    }
    walk
}

fn push_hit(injector: &Injector<Arc<SearchHit>>, hit: SearchHit) {
    injector.push(Arc::new(hit), |hit, cols| {
        cols[COL_NAME] = hit.entry.name.as_ref().into();
        cols[COL_PATH] = hit.rel.as_ref().into();
    });
}

/// Walks the tree; plain searches add every item passing the filters, content searches hand
/// those files to reader threads, which add the matches.
fn walk_tree(
    fs: &Arc<dyn Fs>,
    spec: &WalkSpec,
    injector: &Injector<Arc<SearchHit>>,
    cancel: &Arc<AtomicBool>,
    progress: &Progress,
) {
    // Reading file contents on an SFTP or WebDAV server would download every file.
    if spec.query.content_needle().is_some() && crate::remote::is_remote(&spec.root) {
        return;
    }
    // On a file server each listing and each file waits on a round trip: ask for several at once.
    let network = crate::fs::is_network_path(&spec.root);
    let (list_threads, read_threads) = if network {
        (NET_THREADS, NET_THREADS)
    } else {
        (1, CONTENT_THREADS)
    };
    // On a file server an index that covers the folder stands in for the walk: listing the
    // share folder by folder is what takes the time there.
    let mut indexed = if network {
        spec.index
            .as_ref()
            .and_then(|source| source(&spec.root, spec.show_hidden, cancel))
    } else {
        None
    };
    let Some(matcher) = spec.query.content_needle().and_then(ContentMatcher::new) else {
        gather(
            fs,
            spec,
            indexed.take(),
            list_threads,
            cancel,
            progress,
            &mut |hit| push_hit(injector, hit),
        );
        return;
    };
    // Name text with a content search: only open files whose name matches too.
    let name_pattern = (!spec.query.text.trim().is_empty() && !is_path_query(&spec.query.text))
        .then(|| {
            nucleo::pattern::Pattern::parse(
                &spec.query.text,
                CaseMatching::Smart,
                Normalization::Smart,
            )
        });
    let mut name_matcher = nucleo::Matcher::default();
    let mut buf = Vec::new();
    std::thread::scope(|scope| {
        let (tx, rx) = flume::bounded::<SearchHit>(1024);
        for _ in 0..read_threads {
            let (rx, matcher) = (rx.clone(), &matcher);
            scope.spawn(move || {
                while let Ok(mut hit) = rx.recv() {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let path = spec.root.join(&*hit.rel);
                    let found = matcher.find_in(&**fs, &path, cancel);
                    progress.read.fetch_add(1, Ordering::Relaxed);
                    if let Some(line) = found {
                        hit.snippet = Some(Arc::from(line));
                        push_hit(injector, hit);
                    }
                }
            });
        }
        drop(rx);
        gather(
            fs,
            spec,
            indexed.take(),
            list_threads,
            cancel,
            progress,
            &mut |hit| {
                if !crate::content::worth_reading(&hit.entry) {
                    return;
                }
                if let Some(p) = &name_pattern
                    && p.score(
                        nucleo::Utf32Str::new(&hit.entry.name, &mut buf),
                        &mut name_matcher,
                    )
                    .is_none()
                {
                    return;
                }
                let _ = tx.send(hit);
            },
        );
    });
}

/// Hands every item below the root to `found`: from `indexed` (an index's items, plus a walk of
/// the folders it doesn't enter) when given, otherwise by walking.
fn gather(
    fs: &Arc<dyn Fs>,
    spec: &WalkSpec,
    indexed: Option<IndexedTree>,
    threads: usize,
    cancel: &Arc<AtomicBool>,
    progress: &Progress,
    found: &mut dyn FnMut(SearchHit),
) {
    let Some(tree) = indexed else {
        walk_items(
            fs,
            spec,
            vec![PathBuf::new()],
            threads,
            cancel,
            progress,
            found,
        );
        return;
    };
    let filtered = spec.query.has_filters() || spec.query.content.is_some();
    for item in tree.items {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if !filtered || entry_accepted(&spec.query, &item.entry) {
            found(SearchHit {
                rel: Arc::from(item.rel),
                entry: item.entry,
                snippet: None,
            });
        }
        if progress.scanned.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_ITEMS {
            progress.truncated.store(true, Ordering::Relaxed);
            return;
        }
    }
    if !tree.unindexed.is_empty() {
        let start = tree.unindexed.into_iter().map(PathBuf::from).collect();
        walk_items(fs, spec, start, threads, cancel, progress, found);
    }
}

/// How long the walk waits for a listing before looking whether it was cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Walks breadth first from the folders `start` (relative to the root; their contents, not
/// them): `threads` list folders (one keeps the order folders were found in), this thread takes
/// their contents in turn and calls `found`. The listers aren't waited for: once cancelled or
/// done, this returns even while one is stuck on a folder that never answers (a dead share);
/// it ends when that listing does.
fn walk_items(
    fs: &Arc<dyn Fs>,
    spec: &WalkSpec,
    start: Vec<PathBuf>,
    threads: usize,
    cancel: &Arc<AtomicBool>,
    progress: &Progress,
    found: &mut dyn FnMut(SearchHit),
) {
    let filtered = spec.query.has_filters() || spec.query.content.is_some();
    // A NAS's snapshots and recycle bin aren't entered, unless the search is inside one.
    let skip_snapshots = !spec
        .root
        .components()
        .any(|c| crate::index::is_skipped_dir(&c.as_os_str().to_string_lossy().to_lowercase()));
    // Set when this walk ends (cap reached, cancelled): queued folders aren't listed.
    let stop = Arc::new(AtomicBool::new(false));
    let (work_tx, work_rx) = flume::unbounded::<PathBuf>();
    let (done_tx, done_rx) = flume::unbounded::<(PathBuf, Option<Vec<io::Result<Entry>>>)>();
    for _ in 0..threads.max(1) {
        let (work_rx, done_tx) = (work_rx.clone(), done_tx.clone());
        let (fs, root, stop, cancel) = (
            Arc::clone(fs),
            spec.root.clone(),
            Arc::clone(&stop),
            Arc::clone(cancel),
        );
        let spawned = std::thread::Builder::new()
            .name("search-list".into())
            .spawn(move || {
                while let Ok(rel_dir) = work_rx.recv() {
                    let listed = if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                        Some(Vec::new())
                    } else {
                        fs.read_dir(&root.join(&rel_dir))
                            .ok()
                            .map(Iterator::collect)
                    };
                    if done_tx.send((rel_dir, listed)).is_err() {
                        return;
                    }
                }
            });
        if let Err(err) = spawned {
            log::error!("failed to spawn search lister thread: {err}");
        }
    }
    drop((work_rx, done_tx));
    let mut pending = start.len();
    for dir in start {
        let _ = work_tx.send(dir);
    }
    'walk: while pending > 0 {
        let (rel_dir, listed) = match done_rx.recv_timeout(CANCEL_POLL) {
            Ok(done) => done,
            Err(flume::RecvTimeoutError::Timeout) if !cancel.load(Ordering::Relaxed) => continue,
            Err(_) => break,
        };
        pending -= 1;
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(listed) = listed else {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        for item in listed {
            if cancel.load(Ordering::Relaxed) {
                break 'walk;
            }
            let Ok(entry) = item else {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if !spec.show_hidden && entry.is_hidden() {
                continue;
            }
            let rel_path = rel_dir.join(&*entry.name);
            // Only enter real folders (links and junctions can form cycles), and not a NAS's
            // snapshots or recycle bin: each snapshot is a whole copy of the share.
            if entry.kind == EntryKind::Dir
                && !(skip_snapshots && crate::index::is_skipped_dir(&entry.sort_key))
            {
                pending += 1;
                let _ = work_tx.send(rel_path.clone());
            }
            if !filtered || entry_accepted(&spec.query, &entry) {
                let rel: Arc<str> = Arc::from(rel_path.to_string_lossy().as_ref());
                found(SearchHit {
                    rel,
                    entry,
                    snippet: None,
                });
            }
            if progress.scanned.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_ITEMS {
                progress.truncated.store(true, Ordering::Relaxed);
                break 'walk;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeFs;

    fn file(name: &str) -> Entry {
        Entry::new(name, EntryKind::File, 1, None, 0)
    }

    fn dir(name: &str) -> Entry {
        Entry::new(name, EntryKind::Dir, 0, None, 0)
    }

    /// What an index would hand over: items, and folders it doesn't enter.
    fn tree(items: Vec<(&str, Entry)>, unindexed: &[&str]) -> IndexedTree {
        IndexedTree {
            items: items
                .into_iter()
                .map(|(rel, entry)| crate::index::IndexedItem {
                    rel: rel.to_owned(),
                    entry,
                })
                .collect(),
            unindexed: unindexed.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn collect() -> (SearchSink, flume::Receiver<SearchResults>) {
        let (tx, rx) = flume::unbounded();
        (
            Arc::new(move |r| {
                let _ = tx.send(r);
            }),
            rx,
        )
    }

    /// Waits for the final result (walk finished) of the given generation.
    fn settle(rx: &flume::Receiver<SearchResults>, generation: Gen) -> SearchResults {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let r = rx.recv_timeout(left).expect("no search result");
            if r.generation == generation && !r.searching {
                return r;
            }
        }
    }

    fn rels(r: &SearchResults) -> Vec<String> {
        r.hits.iter().map(|h| h.rel.to_string()).collect()
    }

    #[test]
    fn walks_subfolders_and_ranks_by_fuzzy_score() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\p", vec![dir("src"), dir("docs"), file("Cargo.toml")]);
        fs.insert_dir(
            r"C:\p\src",
            vec![file("main.rs"), file("explorer.rs"), dir("deep")],
        );
        fs.insert_dir(r"C:\p\src\deep", vec![file("explorer_test.rs")]);
        fs.insert_dir(r"C:\p\docs", vec![file("notes.md")]);
        let (sink, rx) = collect();
        let svc = SearchService::new(fs, sink);

        let g = svc.start(PathBuf::from(r"C:\p"), false, "explorer", Clock::default());
        let r = settle(&rx, g);
        assert_eq!(r.scanned, 8);
        assert_eq!(rels(&r).len(), 2);
        // Exact file name matches come first.
        assert!(rels(&r)[0].ends_with("explorer.rs"), "{:?}", rels(&r));

        // A new query does not rewalk.
        let g = svc.set_query("notes", Clock::default());
        let r = settle(&rx, g);
        assert_eq!(rels(&r), [r"docs\notes.md"]);

        // Name only: a file whose path happens to contain s-r-c does not count.
        let g = svc.set_query("src", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), ["src"]);
        // Path matching only with a separator.
        let g = svc.set_query(r"src\exp", Clock::default());
        let mut found = rels(&settle(&rx, g));
        found.sort();
        assert_eq!(found, [r"src\deep\explorer_test.rs", r"src\explorer.rs"]);

        // Blank query shows nothing.
        let g = svc.set_query("", Clock::default());
        assert!(settle(&rx, g).hits.is_empty());
    }

    /// #396: an exact search lists the names as typed; a similar one lists them first and the
    /// fuzzy matches after them, counted in `similar`. Switching only reparses, no rewalk.
    #[test]
    fn exact_search_then_similar_ones_after_it() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\v", vec![dir("tools"), file("vacation_list_drive.txt")]);
        fs.insert_dir(
            r"C:\v\tools",
            vec![file("validrive.exe"), file("other.txt")],
        );
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::clone(&fs) as Arc<dyn Fs>, sink);
        let query = |text: &str, names: NameMatch| Query {
            names,
            ..Query::parse(text, Clock::default())
        };
        let g = svc.start_parsed(
            PathBuf::from(r"C:\v"),
            false,
            query("validrive", NameMatch::Exact),
        );
        let r = settle(&rx, g);
        assert_eq!(rels(&r), [r"tools\validrive.exe"]);
        assert_eq!((r.similar, r.matched), (0, 1));
        let listed = fs.read_dir_calls();

        let g = svc.set_parsed(query("validrive", NameMatch::Similar));
        let r = settle(&rx, g);
        assert_eq!(
            rels(&r),
            [r"tools\validrive.exe", "vacation_list_drive.txt"]
        );
        assert_eq!(r.similar, 1);
        // Back to exact.
        let g = svc.set_parsed(query("validrive", NameMatch::Exact));
        assert_eq!(rels(&settle(&rx, g)), [r"tools\validrive.exe"]);
        assert_eq!(fs.read_dir_calls(), listed, "no rewalk");

        // Short terms are substrings for both: nothing similar. A path query matches the path.
        let g = svc.set_parsed(query("va", NameMatch::Similar));
        let r = settle(&rx, g);
        assert_eq!(r.similar, 0, "{:?}", rels(&r));
        let g = svc.set_parsed(query(r"tls\valid", NameMatch::Exact));
        assert!(settle(&rx, g).hits.is_empty());
        let g = svc.set_parsed(query(r"tls\valid", NameMatch::Similar));
        let r = settle(&rx, g);
        assert_eq!(
            (rels(&r), r.similar),
            (vec![r"tools\validrive.exe".to_owned()], 1)
        );
    }

    /// A tab that never searches has no search thread, and what it is told before its first search
    /// (stop on leaving a folder, a query) queues nothing.
    #[test]
    fn no_thread_until_the_first_search() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\p", vec![file("notes.md")]);
        let (sink, rx) = collect();
        let svc = SearchService::new(fs, sink);
        svc.stop();
        svc.set_query("notes", Clock::default());
        assert!(svc.unstarted.lock().is_some(), "no thread yet");
        assert!(svc.cmd.is_empty(), "nothing queued");
        let g = svc.start(PathBuf::from(r"C:\p"), false, "notes", Clock::default());
        assert!(svc.unstarted.lock().is_none());
        assert_eq!(rels(&settle(&rx, g)), ["notes.md"]);
    }

    /// The point of the design: a permanently hung folder under the root (dead network drive)
    /// does not block the caller, and searching another root still returns results.
    #[test]
    fn hung_folder_does_not_block_new_search() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\slow", vec![file("a.txt")]);
        fs.hang(r"C:\slow");
        fs.insert_dir(r"C:\fast", vec![file("target.txt")]);
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::clone(&fs) as Arc<dyn Fs>, sink);

        let started = Instant::now();
        let _stuck = svc.start(PathBuf::from(r"C:\slow"), false, "a", Clock::default());
        let g = svc.start(PathBuf::from(r"C:\fast"), false, "target", Clock::default());
        assert!(started.elapsed() < Duration::from_millis(50));
        let r = settle(&rx, g);
        assert_eq!(rels(&r), ["target.txt"]);
        fs.release_all();
    }

    /// A NAS keeps a whole copy of the share for every snapshot: searching the share finds the
    /// snapshot folder itself, but walks and reads nothing inside one.
    #[test]
    fn snapshots_on_a_share_are_not_walked() {
        let fs = FakeFs::new();
        let snapshot = "GMT+08-2026.10.13-08.00.02";
        fs.insert_dir(
            r"\\nas\dad",
            vec![
                dir("#snapshot"),
                dir("docs"),
                dir("@GMT-2026.10.13-00.00.00"),
            ],
        );
        fs.insert_dir(r"\\nas\dad\#snapshot", vec![dir(snapshot)]);
        for folder in [
            format!(r"\\nas\dad\#snapshot\{snapshot}"),
            r"\\nas\dad\@GMT-2026.10.13-00.00.00".to_owned(),
            r"\\nas\dad\docs".to_owned(),
        ] {
            fs.insert_dir(&folder, vec![file("plan.txt")]);
            fs.set_content(format!(r"{folder}\plan.txt"), b"budget".to_vec());
        }
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::clone(&fs) as Arc<dyn Fs>, sink);
        let g = svc.start(PathBuf::from(r"\\nas\dad"), false, "plan", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), [r"docs\plan.txt"]);
        let g = svc.set_query("#snapshot", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), ["#snapshot"]);
        let g = svc.start(
            PathBuf::from(r"\\nas\dad"),
            false,
            "content:budget",
            Clock::default(),
        );
        let r = settle(&rx, g);
        assert_eq!((rels(&r), r.read), (vec![r"docs\plan.txt".to_owned()], 1));
    }

    /// Below a network folder an index covers, a search takes the items from the index: nothing
    /// is listed, and a content search opens only the files that qualify. Elsewhere it walks.
    #[test]
    fn indexed_network_folders_are_not_walked() {
        let fs = FakeFs::new();
        fs.insert_dir(r"\\nas\s", vec![dir("docs")]);
        fs.insert_dir(r"\\nas\s\docs", vec![file("plan.txt"), file("pic.jpg")]);
        fs.set_content(r"\\nas\s\docs\plan.txt", b"budget".to_vec());
        fs.insert_dir(r"C:\local", vec![file("plan.txt")]);
        fs.set_content(r"C:\local\plan.txt", b"budget".to_vec());
        let source: TreeSource = Arc::new(|root, _, _| {
            (root == std::path::Path::new(r"\\nas\s")).then(|| {
                tree(
                    vec![
                        ("docs", dir("docs")),
                        (r"docs\plan.txt", file("plan.txt")),
                        (r"docs\pic.jpg", file("pic.jpg")),
                    ],
                    &[],
                )
            })
        });
        let (sink, rx) = collect();
        let svc = SearchService::with_index(Arc::clone(&fs) as Arc<dyn Fs>, sink, Some(source));
        let g = svc.start(PathBuf::from(r"\\nas\s"), false, "plan", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), [r"docs\plan.txt"]);
        let g = svc.start(
            PathBuf::from(r"\\nas\s"),
            false,
            "content:budget",
            Clock::default(),
        );
        let r = settle(&rx, g);
        assert_eq!((rels(&r), r.read), (vec![r"docs\plan.txt".to_owned()], 1));
        assert_eq!(fs.read_dir_calls(), 0, "the share was never listed");
        let g = svc.start(PathBuf::from(r"C:\local"), false, "plan", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), ["plan.txt"]);
        assert_eq!(fs.read_dir_calls(), 1, "local folders are walked");
    }

    /// Folders the index doesn't enter (build folders, exclusions) are walked on top of what the
    /// index has, so a search there still finds what's inside them.
    #[test]
    fn folders_the_index_skips_are_walked() {
        let fs = FakeFs::new();
        fs.insert_dir(r"\\nas\s\node_modules", vec![file("plan-lib.js")]);
        let source: TreeSource = Arc::new(|_, _, _| {
            Some(tree(
                vec![
                    ("plan.txt", file("plan.txt")),
                    ("node_modules", dir("node_modules")),
                ],
                &["node_modules"],
            ))
        });
        let (sink, rx) = collect();
        let svc = SearchService::with_index(Arc::clone(&fs) as Arc<dyn Fs>, sink, Some(source));
        let g = svc.start(PathBuf::from(r"\\nas\s"), false, "plan", Clock::default());
        let mut found = rels(&settle(&rx, g));
        found.sort();
        assert_eq!(found, [r"node_modules\plan-lib.js", "plan.txt"]);
        assert_eq!(fs.read_dir_calls(), 1, "only the skipped folder is listed");
    }

    /// Searching inside a snapshot folder on purpose finds what's in it.
    #[test]
    fn a_search_inside_a_snapshot_folder_walks_it() {
        let fs = FakeFs::new();
        let snapshot = "GMT+08-2026.10.13-08.00.02";
        fs.insert_dir(
            format!(r"\\nas\dad\#snapshot\{snapshot}"),
            vec![file("plan.txt")],
        );
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::clone(&fs) as Arc<dyn Fs>, sink);
        let g = svc.start(
            PathBuf::from(r"\\nas\dad\#snapshot"),
            false,
            "plan",
            Clock::default(),
        );
        assert_eq!(rels(&settle(&rx, g)), [format!(r"{snapshot}\plan.txt")]);
    }

    /// A cancelled walk returns even while every lister is stuck on a share that never answers.
    #[test]
    fn a_cancelled_walk_does_not_wait_for_stuck_listings() {
        let fs = FakeFs::new();
        fs.insert_dir(r"\\nas\s", vec![dir("a")]);
        fs.hang(r"\\nas\s");
        let spec = WalkSpec {
            root: PathBuf::from(r"\\nas\s"),
            show_hidden: false,
            query: Query::parse("x", Clock::default()),
            index: None,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = flume::bounded(1);
        {
            let fs = Arc::clone(&fs) as Arc<dyn Fs>;
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                walk_items(
                    &fs,
                    &spec,
                    vec![PathBuf::new()],
                    NET_THREADS,
                    &cancel,
                    &Progress::default(),
                    &mut |_| {},
                );
                let _ = tx.send(());
            });
        }
        std::thread::sleep(Duration::from_millis(50));
        cancel.store(true, Ordering::Relaxed);
        assert!(
            rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "the walk waited for the stuck listing"
        );
        fs.release_all();
    }

    /// On a file server several folders are listed at once: a slow one doesn't hold up the rest.
    #[test]
    fn network_folders_are_listed_several_at_once() {
        let fs = FakeFs::new();
        fs.insert_dir(r"\\nas\s", vec![dir("slow"), dir("a"), dir("b")]);
        fs.insert_dir(r"\\nas\s\slow", vec![file("late.txt")]);
        fs.insert_dir(r"\\nas\s\a", vec![file("one.txt")]);
        fs.insert_dir(r"\\nas\s\b", vec![file("two.txt")]);
        fs.hang(r"\\nas\s\slow");
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::clone(&fs) as Arc<dyn Fs>, sink);
        let g = svc.start(PathBuf::from(r"\\nas\s"), false, "txt", Clock::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs.read_dir_calls() < 4 {
            assert!(
                Instant::now() < deadline,
                "the other folders wait for the slow one"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        fs.release_all();
        let mut found = rels(&settle(&rx, g));
        found.sort();
        assert_eq!(found, [r"a\one.txt", r"b\two.txt", r"slow\late.txt"]);
    }

    #[test]
    fn filters_apply_while_walking_and_rewalk_when_changed() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\f",
            vec![dir("pics"), file("notes.txt"), file("song.mp3")],
        );
        fs.insert_dir(r"C:\f\pics", vec![file("cat.jpg"), file("cat.txt")]);
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::clone(&fs) as Arc<dyn Fs>, sink);
        let g = svc.start(
            PathBuf::from(r"C:\f"),
            false,
            "cat type:image",
            Clock::default(),
        );
        assert_eq!(rels(&settle(&rx, g)), [r"pics\cat.jpg"]);
        let walks = fs.read_dir_calls();
        // Filters alone list everything that passes them.
        let g = svc.set_query("type:image", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), [r"pics\cat.jpg"]);
        assert_eq!(fs.read_dir_calls(), walks, "same filters: no rewalk");
        let g = svc.set_query("cat ext:txt", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), [r"pics\cat.txt"]);
        assert!(fs.read_dir_calls() > walks, "new filters walk again");
    }

    #[test]
    fn content_search_reads_files_and_keeps_the_line() {
        let fs = FakeFs::new();
        let mut big = file("big.log");
        big.size = crate::content::MAX_CONTENT_FILE + 1;
        let cloud = Entry::new("cloud.txt", EntryKind::File, 10, None, 0x40_0000);
        fs.insert_dir(
            r"C:\c",
            vec![dir("sub"), file("a.txt"), file("b.txt"), big, cloud],
        );
        fs.insert_dir(r"C:\c\sub", vec![file("deep.md")]);
        fs.set_content(r"C:\c\a.txt", b"nothing here".to_vec());
        fs.set_content(r"C:\c\b.txt", b"one\n  Invoice 42 due\n".to_vec());
        fs.set_content(r"C:\c\sub\deep.md", b"see INVOICE list".to_vec());
        fs.set_content(r"C:\c\big.log", b"invoice".to_vec());
        fs.set_content(r"C:\c\cloud.txt", b"invoice".to_vec());
        let (sink, rx) = collect();
        let svc = SearchService::new(fs, sink);
        let g = svc.start(
            PathBuf::from(r"C:\c"),
            false,
            r#"content:"invoice""#,
            Clock::default(),
        );
        let r = settle(&rx, g);
        assert!(r.contents);
        let mut found: Vec<(String, String)> = r
            .hits
            .iter()
            .map(|h| (h.rel.to_string(), h.snippet.as_deref().unwrap().to_string()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("b.txt".to_string(), "Invoice 42 due".to_string()),
                (r"sub\deep.md".to_string(), "see INVOICE list".to_string()),
            ],
            "the big file and the cloud placeholder are never read"
        );
        assert_eq!(r.read, 3);
        // Name text narrows which files are opened.
        let g = svc.set_query("deep content:invoice", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), [r"sub\deep.md"]);
    }

    #[test]
    fn content_search_finds_text_inside_documents_only_when_asked() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\d",
            vec![
                file("letter.docx"),
                file("scan.pdf"),
                file("invoice-list.txt"),
                file("old.doc"),
            ],
        );
        fs.set_content(
            r"C:\d\letter.docx",
            crate::office::fixtures::docx("<w:p><w:r><w:t>Please pay invoice 7</w:t></w:r></w:p>"),
        );
        fs.set_content(
            r"C:\d\scan.pdf",
            crate::pdf_text::fixtures::pdf(&[&["page one"], &["發票 invoice 8"]], true),
        );
        fs.set_content(r"C:\d\invoice-list.txt", b"nothing".to_vec());
        fs.set_content(r"C:\d\old.doc", b"invoice".to_vec());
        let (sink, rx) = collect();
        let svc = SearchService::new(fs, sink);
        let g = svc.start(
            PathBuf::from(r"C:\d"),
            false,
            "content:invoice",
            Clock::default(),
        );
        let r = settle(&rx, g);
        let mut found: Vec<(String, String)> = r
            .hits
            .iter()
            .map(|h| (h.rel.to_string(), h.snippet.as_deref().unwrap().to_string()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                (
                    "letter.docx".to_string(),
                    "Please pay invoice 7".to_string()
                ),
                ("scan.pdf".to_string(), "發票 invoice 8".to_string()),
            ],
            "the old binary format isn't read"
        );
        assert_eq!(r.read, 3);
        // A name search never opens files: only the name matches, without a snippet.
        let g = svc.set_query("invoice", Clock::default());
        let r = settle(&rx, g);
        assert_eq!(rels(&r), ["invoice-list.txt"]);
        assert!(r.hits.iter().all(|h| h.snippet.is_none()));
    }

    #[test]
    fn skips_hidden_and_links() {
        let fs = FakeFs::new();
        let hidden = Entry::new("secret", EntryKind::Dir, 0, None, 0x2);
        let link = Entry::new("loop", EntryKind::Link { is_dir: true }, 0, None, 0);
        fs.insert_dir(r"C:\r", vec![hidden, link, file("keep.txt")]);
        fs.insert_dir(r"C:\r\secret", vec![file("keep_hidden.txt")]);
        fs.insert_dir(r"C:\r\loop", vec![file("keep_loop.txt")]);
        let (sink, rx) = collect();
        let svc = SearchService::new(fs, sink);
        let g = svc.start(PathBuf::from(r"C:\r"), false, "keep", Clock::default());
        assert_eq!(rels(&settle(&rx, g)), ["keep.txt"]);
        let g = svc.start(PathBuf::from(r"C:\r"), true, "keep", Clock::default());
        let mut found = rels(&settle(&rx, g));
        found.sort();
        assert_eq!(found, ["keep.txt", r"secret\keep_hidden.txt"]);
    }

    /// Content search over a few hundred generated documents on disk, timed. Run with
    /// `cargo nextest run -p arke-core --run-ignored only --no-capture content_search_benchmark`.
    #[test]
    #[ignore = "benchmark"]
    #[allow(
        clippy::print_stderr,
        clippy::too_many_lines,
        clippy::format_collect,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn content_search_benchmark() {
        use crate::office::fixtures as office;
        use crate::pdf_text::fixtures::pdf;
        use std::fmt::Write as _;
        let tmp_dir = tempfile::tempdir().unwrap();
        let root = tmp_dir.path().to_path_buf();
        let words = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let started = Instant::now();
        let mut bytes = 0_usize;
        let mut write = |name: String, data: Vec<u8>| {
            bytes += data.len();
            std::fs::write(root.join(name), data).unwrap();
        };
        for i in 0..100 {
            // The needle sits at the end of every tenth file, so the rest are read whole.
            let needle = if i % 10 == 0 { "needle" } else { "" };
            let mut body = String::new();
            for p in 0..200 {
                let _ = write!(body, "<w:p><w:r><w:t>{words} {p}</w:t></w:r></w:p>");
            }
            let _ = write!(body, "<w:p><w:r><w:t>end {needle}</w:t></w:r></w:p>");
            write(format!("doc{i}.docx"), office::docx(&body));
            let rows: String = (1..=300)
                .map(|r| format!(r#"<row r="{r}"><c r="A{r}" t="s"><v>{}</v></c><c r="B{r}"><v>{r}</v></c></row>"#, r % 50))
                .collect();
            let mut shared: Vec<String> = (0..50).map(|n| format!("{words} {n}")).collect();
            shared.push(format!("last {needle}"));
            let shared: Vec<&str> = shared.iter().map(String::as_str).collect();
            write(
                format!("book{i}.xlsx"),
                office::xlsx(&[("One", &rows), ("Two", &rows)], &shared, ""),
            );
            let line = format!("{words} on a slide");
            let mut slides: Vec<(String, Vec<&str>)> = (0..20)
                .map(|n| (format!("Slide {n}"), vec![line.as_str(); 6]))
                .collect();
            slides.push((format!("Final {needle}"), vec![]));
            let slides: Vec<(&str, &[&str])> = slides
                .iter()
                .map(|(t, l)| (t.as_str(), l.as_slice()))
                .collect();
            write(format!("deck{i}.pptx"), office::pptx(&slides, None));
            let page: Vec<String> = (0..40)
                .map(|n| format!("{words} line {n} 中文 {n}"))
                .collect();
            let page: Vec<&str> = page.iter().map(String::as_str).collect();
            let last = [format!("the end {needle}")];
            let last: Vec<&str> = last.iter().map(String::as_str).collect();
            let mut pages: Vec<&[&str]> = vec![page.as_slice(); 10];
            pages.push(&last);
            write(format!("paper{i}.pdf"), pdf(&pages, true));
            write(
                format!("notes{i}.txt"),
                format!("{words}\n{needle}\n").into_bytes(),
            );
        }
        // One long PDF: 300 pages of text, the needle past the page cap.
        let page: Vec<String> = (0..50).map(|n| format!("{words} {n}")).collect();
        let page: Vec<&str> = page.iter().map(String::as_str).collect();
        let mut pages: Vec<&[&str]> = vec![page.as_slice(); crate::pdf_text::MAX_PAGES];
        let tail: &[&str] = &["needle"];
        pages.push(tail);
        let long = pdf(&pages, true);
        let long_len = long.len();
        write("long.pdf".into(), long);
        // A big uncompressed PDF: the whole file is parsed before the first page is read.
        let pages: Vec<&[&str]> = vec![page.as_slice(); 3000];
        let huge = pdf(&pages, false);
        let huge_len = huge.len();
        write("huge.pdf".into(), huge);
        let generated = started.elapsed();
        let (sink, rx) = collect();
        let svc = SearchService::new(Arc::new(crate::fs::RealFs), sink);
        for (label, query) in [("hits", "content:needle"), ("no hits", "content:absent")] {
            let started = Instant::now();
            let g = svc.start(root.clone(), false, query, Clock::default());
            let mut first_hit = None;
            let r = loop {
                let r = rx.recv_timeout(Duration::from_secs(120)).unwrap();
                if r.generation != g {
                    continue;
                }
                if first_hit.is_none() && !r.hits.is_empty() {
                    first_hit = Some(started.elapsed());
                }
                if !r.searching {
                    break r;
                }
            };
            eprintln!(
                "{label}: {} matches, {} files read in {:?} (first hit after {first_hit:?})",
                r.hits.len(),
                r.read,
                started.elapsed()
            );
        }
        // One file at a time, to see what each kind costs.
        let matcher = crate::content::ContentMatcher::new("absent").unwrap();
        let cancel = AtomicBool::new(false);
        for name in [
            "doc1.docx",
            "book1.xlsx",
            "deck1.pptx",
            "paper1.pdf",
            "notes1.txt",
            "long.pdf",
            "huge.pdf",
        ] {
            let path = root.join(name);
            let size = std::fs::metadata(&path).unwrap().len();
            let started = Instant::now();
            let runs = 20;
            for _ in 0..runs {
                assert!(
                    matcher
                        .find_in(&crate::fs::RealFs, &path, &cancel)
                        .is_none()
                );
            }
            eprintln!(
                "{name} ({size} bytes): {:?} per file",
                started.elapsed() / runs
            );
        }
        eprintln!(
            "corpus: 502 files, {} KB (long.pdf {} KB, huge.pdf {} KB), generated in {generated:?}",
            bytes / 1024,
            long_len / 1024,
            huge_len / 1024
        );
    }
}
