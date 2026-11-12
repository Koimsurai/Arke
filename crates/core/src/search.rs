//! Fuzzy search in subfolders.
//!
//! - Walker thread: breadth-first `Fs::read_dir` from the root, feeding items to nucleo as
//!   it goes. Does not enter symlinks or junctions (cycles); skips hidden items by default.
//! - Coordinator thread: owns `nucleo::Nucleo`, takes queries, calls `tick` and sends the top
//!   [`MAX_HITS`] as a snapshot to the UI. `tick` may briefly wait on workers when the query
//!   changes, so it never runs on the UI thread.
//! - Every `start` / `set_query` bumps the generation; the UI only accepts the latest one.
//! - The query uses the search box syntax ([`crate::index_query`]): filters are applied while
//!   walking (changing them rewalks), the name text goes to nucleo. With `content:`, files that
//!   pass the filters are read by a few worker threads ([`crate::content`]) and only matches are
//!   added, each with its first matching line.

use std::collections::VecDeque;
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
use crate::index_query::{Clock, Query};
use crate::service::Gen;

/// Maximum hits sent to the UI (top by score).
pub const MAX_HITS: usize = 5_000;
/// Cap on items walked per search, so a whole drive is not walked forever.
pub const MAX_ITEMS: u64 = 2_000_000;
/// Minimum interval between result sends.
const SEND_INTERVAL: Duration = Duration::from_millis(60);
/// Threads reading files for a content search (disk bound; more mostly adds seeking).
const CONTENT_THREADS: usize = 4;

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
}

/// Receives result snapshots on the coordinator thread.
pub type SearchSink = Arc<dyn Fn(SearchResults) + Send + Sync>;

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
}

impl SearchService {
    /// Spawns the coordinator thread; nothing is walked until [`Self::start`].
    pub fn new(fs: Arc<dyn Fs>, sink: SearchSink) -> Self {
        let (cmd, rx) = flume::unbounded();
        let wake_tx = cmd.downgrade();
        let spawned = std::thread::Builder::new()
            .name("search".into())
            .spawn(move || coordinator(&fs, &sink, &rx, &wake_tx));
        if let Err(err) = spawned {
            log::error!("failed to spawn search thread: {err}");
        }
        Self {
            cmd,
            generation: AtomicU64::new(0),
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
        let generation = self.next_gen();
        let _ = self.cmd.send(Cmd::Start {
            generation,
            root,
            show_hidden,
            query: Query::parse(query, clock),
        });
        generation
    }

    /// Changes the query. Reuses walked items when only the name text changed; new filters
    /// rewalk. Returns immediately.
    pub fn set_query(&self, query: &str, clock: Clock) -> Gen {
        let generation = self.next_gen();
        let _ = self.cmd.send(Cmd::Query {
            generation,
            query: Query::parse(query, clock),
        });
        generation
    }

    /// Stops walking and matching; results already sent become stale.
    pub fn stop(&self) {
        self.next_gen();
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
    sink: &SearchSink,
    rx: &flume::Receiver<Cmd>,
    wake_tx: &flume::WeakSender<Cmd>,
) {
    // nucleo calls notify for every pushed item; a flag coalesces them into at most one pending
    // Wake.
    let wake_pending = Arc::new(AtomicBool::new(false));
    let notify: Notify = {
        let wake_tx = wake_tx.clone();
        let pending = wake_pending.clone();
        Arc::new(move || {
            if !pending.swap(true, Ordering::AcqRel)
                && let Some(tx) = wake_tx.upgrade()
            {
                let _ = tx.send(Cmd::Wake);
            }
        })
    };
    let mut c = Coordinator {
        fs: fs.clone(),
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
                    let (old, new) = (&a.query.text, &query.text);
                    let append = !old.is_empty()
                        && new.starts_with(old.as_str())
                        && is_path_query(new) == is_path_query(old);
                    reparse(nucleo, new, append);
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
        let notify = self.notify.clone();
        let nucleo = self.nucleo.get_or_insert_with(|| {
            Nucleo::new(Config::DEFAULT.match_paths(), notify, None, COLUMNS)
        });
        nucleo.restart(true);
        reparse(nucleo, &query.text, false);
        let walk = spawn_walk(
            self.fs.clone(),
            WalkSpec {
                root: root.clone(),
                show_hidden,
                query: query.clone(),
            },
            nucleo.injector(),
            self.notify.clone(),
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
            .map(|item| item.data.clone())
            .collect();
        // No name to rank by: newest first, like "This PC". Content hits stay in the order
        // found so the list doesn't jump while files are read.
        if a.query.text.trim().is_empty() && a.query.content_needle().is_none() {
            hits.sort_by_key(|h| std::cmp::Reverse(h.entry.modified));
        }
        (hits, matched)
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
    }
}

/// What one walk looks for.
struct WalkSpec {
    root: PathBuf,
    show_hidden: bool,
    query: Query,
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
        cancel: cancel.clone(),
        progress: progress.clone(),
    };
    let spawned = std::thread::Builder::new()
        .name("search-walk".into())
        .spawn(move || {
            walk_tree(&*fs, &spec, &injector, &cancel, &progress);
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
    fs: &dyn Fs,
    spec: &WalkSpec,
    injector: &Injector<Arc<SearchHit>>,
    cancel: &AtomicBool,
    progress: &Progress,
) {
    // Reading file contents on an SFTP or WebDAV server would download every file.
    if spec.query.content_needle().is_some() && crate::remote::is_remote(&spec.root) {
        return;
    }
    let Some(matcher) = spec.query.content_needle().and_then(ContentMatcher::new) else {
        walk_items(fs, spec, cancel, progress, &mut |hit| {
            push_hit(injector, hit);
        });
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
        for _ in 0..CONTENT_THREADS {
            let (rx, matcher) = (rx.clone(), &matcher);
            scope.spawn(move || {
                while let Ok(mut hit) = rx.recv() {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let path = spec.root.join(&*hit.rel);
                    let found = matcher.find_in(fs, &path, cancel);
                    progress.read.fetch_add(1, Ordering::Relaxed);
                    if let Some(line) = found {
                        hit.snippet = Some(Arc::from(line));
                        push_hit(injector, hit);
                    }
                }
            });
        }
        drop(rx);
        walk_items(fs, spec, cancel, progress, &mut |hit| {
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
        });
    });
}

fn walk_items(
    fs: &dyn Fs,
    spec: &WalkSpec,
    cancel: &AtomicBool,
    progress: &Progress,
    found: &mut dyn FnMut(SearchHit),
) {
    let mut queue: VecDeque<PathBuf> = VecDeque::from([PathBuf::new()]);
    let filtered = spec.query.has_filters() || spec.query.content.is_some();
    while let Some(rel_dir) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(iter) = fs.read_dir(&spec.root.join(&rel_dir)) else {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        for item in iter {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Ok(entry) = item else {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if !spec.show_hidden && entry.is_hidden() {
                continue;
            }
            let rel_path = rel_dir.join(&*entry.name);
            // Only enter real folders; links and junctions can form cycles.
            if entry.kind == EntryKind::Dir {
                queue.push_back(rel_path.clone());
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
                return;
            }
        }
    }
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

    /// The point of the design: a permanently hung folder under the root (dead network drive)
    /// does not block the caller, and searching another root still returns results.
    #[test]
    fn hung_folder_does_not_block_new_search() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\slow", vec![file("a.txt")]);
        fs.hang(r"C:\slow");
        fs.insert_dir(r"C:\fast", vec![file("target.txt")]);
        let (sink, rx) = collect();
        let svc = SearchService::new(fs.clone(), sink);

        let started = Instant::now();
        let _stuck = svc.start(PathBuf::from(r"C:\slow"), false, "a", Clock::default());
        let g = svc.start(PathBuf::from(r"C:\fast"), false, "target", Clock::default());
        assert!(started.elapsed() < Duration::from_millis(50));
        let r = settle(&rx, g);
        assert_eq!(rels(&r), ["target.txt"]);
        fs.release_all();
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
        let svc = SearchService::new(fs.clone(), sink);
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
    #[allow(clippy::print_stderr, clippy::too_many_lines, clippy::format_collect)]
    fn content_search_benchmark() {
        use crate::office::fixtures as office;
        use crate::pdf_text::fixtures::pdf;
        use std::fmt::Write as _;
        let root = std::env::temp_dir().join(format!("arke-content-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
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
        let _ = std::fs::remove_dir_all(&root);
    }
}
