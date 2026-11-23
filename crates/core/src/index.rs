//! Global search index ("This PC"): like Everything, but **without admin rights and without
//! bundling third-party programs**.
//!
//! Everything reads the NTFS MFT directly, which needs the volume handle (`\\.\C:`) and admin
//! rights. Instead:
//! - **Build**: several background threads walk directories (`Fs::read_dir` returns name, size,
//!   mtime and attributes without opening files). Symlinks and junctions are not followed
//!   (cycles), and a few system folders are skipped.
//! - **Storage**: each node holds its parent, name offset in a string pool and flags (12 bytes);
//!   size, mtime and a char mask live in separate columns (16 bytes). Names are one big string.
//! - **Query** (syntax in [`crate::index_query`]): threads take small chunks (faster cores do
//!   more), apply filters, then reject impossible names with the char mask (a single AND, no
//!   name read), and hand the rest to nucleo; per-thread top results are merged. Full paths
//!   are built only for the winners. Can search only within the previous matches (narrowing
//!   while typing).
//! - **Updates**: changes after the build go into an [`Overlay`] (added/removed paths) that
//!   queries merge in; too many triggers a rebuild.
//! - **Folder sizes** ([`Index::child_dir_sizes`]): one pass over the nodes sums every file under
//!   each subfolder, with the overlay applied, instead of walking the disk. Same rules as
//!   [`crate::dirsize`]: links are not counted, every hard link name counts.
//!
//! Background threads only.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};

use crate::entry::{Entry, EntryKind};
use crate::fs::Fs;
use crate::index_query::Query;
use crate::journal::{Cursor, LISTED_ROOT, ListSink, Listed};

/// No parent (root).
const NO_PARENT: u32 = u32::MAX;
const FLAG_DIR: u8 = 1;
const FLAG_HIDDEN: u8 = 2;
/// Symlink, junction or mount point (to a file or a folder).
const FLAG_LINK: u8 = 4;
/// A real folder whose contents are not in the index (build folders, exclusions, hidden system
/// folders, cloud placeholder folders).
const FLAG_PRUNED: u8 = 8;
/// Folders never entered: the system recycle bin, restore data and Windows Update staging, and on
/// NAS shares their snapshots (each a full copy of the share), recycle bins and thumbnail caches
/// (Synology, QNAP, NetApp, ZFS, snapper and others).
const SKIP_DIRS: [&str; 14] = [
    "$recycle.bin",
    "system volume information",
    "$windows.~bt",
    "$windows.~ws",
    "#snapshot",
    "#recycle",
    "@eadir",
    "@recycle",
    "@recently-snapshot",
    ".@__thumb",
    ".snapshot",
    ".snapshots",
    "~snapshot",
    ".zfs",
];

/// Whether a folder (lowercase name) is never entered: one of [`SKIP_DIRS`], or a single
/// snapshot of a share by its time stamp (`GMT+08-2026.10.13-08.00.02` on Synology,
/// `@GMT-2026.10.13-00.00.00` from Samba and Windows previous versions). Those also show up
/// outside a snapshot folder, through a link on the server or a share of the snapshot folder,
/// and each is a whole copy of the share.
pub fn is_skipped_dir(name_lower: &str) -> bool {
    SKIP_DIRS.contains(&name_lower) || is_snapshot_name(name_lower)
}

/// `[@]gmt[±hh[:mm]]-yyyy.mm.dd-hh.mm.ss` (lowercase).
fn is_snapshot_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('@').unwrap_or(name).strip_prefix("gmt") else {
        return false;
    };
    // `yyyy.mm.dd-hh.mm.ss`: digits with dots, one dash in the middle.
    let stamp = |s: &str| {
        s.len() == 19
            && s.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 | 13 | 16 => b == b'.',
                10 => b == b'-',
                _ => b.is_ascii_digit(),
            })
    };
    if rest.strip_prefix('-').is_some_and(stamp) {
        return true;
    }
    // With a zone offset: `+08`, `-0530`, `+05:30`.
    let Some(zone) = rest.strip_prefix(['+', '-']) else {
        return false;
    };
    let digits = zone
        .find(|c: char| !c.is_ascii_digit() && c != ':')
        .unwrap_or(zone.len());
    digits > 0 && zone[digits..].strip_prefix('-').is_some_and(stamp)
}
/// Build output and package cache folders (not entered with "skip build folders", but the folder
/// itself stays findable): often hundreds of thousands of churning files nobody searches for.
const BUILD_DIRS: [&str; 12] = [
    "node_modules",
    ".git",
    "__pycache__",
    ".venv",
    ".gradle",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    ".pnpm-store",
    ".yarn",
];

/// Whether a folder is build output or a package cache. `siblings` are the other names in the
/// same folder (lowercase), used to recognize projects: `target` next to `Cargo.toml`,
/// `bin`/`obj` next to a `.csproj` and the like.
pub fn is_build_dir<'a>(name_lower: &str, siblings: &mut impl Iterator<Item = &'a str>) -> bool {
    if is_named_build_dir(name_lower) {
        return true;
    }
    match name_lower {
        "target" => siblings.any(|s| s == "cargo.toml"),
        "bin" | "obj" => siblings
            .any(|s| s.ends_with(".csproj") || s.ends_with(".fsproj") || s.ends_with(".vbproj")),
        _ => false,
    }
}

/// Build or package cache folder by name alone (`node_modules`, `.git`...; not ones like
/// `target` that need their siblings).
pub fn is_named_build_dir(name_lower: &str) -> bool {
    BUILD_DIRS.contains(&name_lower)
}

// The saved file is used directly as memory, so little-endian only (x64 and ARM64 Windows both
// are).
#[cfg(target_endian = "big")]
compile_error!("index files are little-endian");
const _: () = assert!(size_of::<Node>() == 12 && align_of::<Node>() == 4);

/// Name hash (FNV-1a; only finds candidate duplicates, contents are compared afterwards).
fn name_hash(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// File format magic (a version mismatch fails to load and we just rebuild).
const MAGIC: &[u8; 8] = b"ARKEIDX6";
/// Items taken per chunk when querying.
const CHUNK: usize = 16 * 1024;

/// Nodes on disk match the in-memory layout (12 bytes, little-endian), so the file can be mapped
/// directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
struct Node {
    parent: u32,
    name_start: u32,
    name_len: u16,
    flags: u8,
}

/// One search hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexHit {
    /// Absolute path of the match.
    pub path: PathBuf,
    /// Folder (or link to a folder).
    pub is_dir: bool,
    /// File size (0 for folders).
    pub size: u64,
    /// Last write time; `None` if unknown.
    pub modified: Option<SystemTime>,
    /// Matched byte ranges in the name, for highlighting.
    pub highlights: Vec<std::ops::Range<usize>>,
    score: u32,
}

impl IndexHit {
    /// Match score (higher is better), used to sort when merging results from several indexes.
    pub const fn score(&self) -> u32 {
        self.score
    }

    /// A hit for an item found outside the index (see [`crate::rank::frecent_hits`]).
    pub(crate) const fn new(
        path: PathBuf,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
        highlights: Vec<std::ops::Range<usize>>,
        score: u32,
    ) -> Self {
        Self {
            path,
            is_dir,
            size,
            modified,
            highlights,
            score,
        }
    }
}

/// Matches paths against the name part (and folder part, `docs\report`) of a query the way the
/// index does. Built once per query and reused: a matcher is too costly to create per path.
pub(crate) struct PathMatcher {
    name: Pattern,
    dir: Option<Pattern>,
    matcher: Matcher,
    buf: Vec<char>,
    indices: Vec<u32>,
}

impl PathMatcher {
    /// `None` when the query has no name part.
    pub(crate) fn new(query: &Query) -> Option<Self> {
        let (dir_part, name_part) = split_query(query.text.trim());
        if name_part.is_empty() {
            return None;
        }
        Some(Self {
            name: name_pattern(name_part),
            dir: dir_part.map(|d| Pattern::parse(d, CaseMatching::Smart, Normalization::Smart)),
            matcher: Matcher::new(nucleo::Config::DEFAULT.match_paths()),
            buf: Vec::new(),
            indices: Vec::new(),
        })
    }

    /// The score and the highlighted byte ranges in the name; `None` when it doesn't match.
    pub(crate) fn matches(&mut self, path: &Path) -> Option<(u32, Vec<std::ops::Range<usize>>)> {
        let name = path.file_name()?.to_string_lossy();
        self.indices.clear();
        let score = self.name.indices(
            Utf32Str::new(&name, &mut self.buf),
            &mut self.matcher,
            &mut self.indices,
        )?;
        if let Some(dir) = &self.dir {
            let parent = path.parent()?.to_string_lossy();
            dir.score(Utf32Str::new(&parent, &mut self.buf), &mut self.matcher)?;
        }
        Some((
            score,
            crate::filter::char_ranges_to_bytes(&name, &mut self.indices),
        ))
    }
}

/// What to skip while building.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildRules {
    /// Don't enter build output and package cache folders (see [`is_build_dir`]).
    pub skip_build_dirs: bool,
    /// User-excluded folders: the folder itself is findable, its contents are not indexed.
    pub excluded: Vec<PathBuf>,
    /// Stop entering folders once the index holds this many items (network shares, which can be
    /// far larger than anyone searches); `None` for no limit.
    pub max_items: Option<usize>,
}

/// Path comparison key: lowercase, no trailing `\`.
fn path_key(path: &Path) -> String {
    let mut s = path.to_string_lossy().to_lowercase();
    while s.len() > 3 && (s.ends_with('\\') || s.ends_with('/')) {
        s.pop();
    }
    s
}

impl BuildRules {
    /// Fingerprint of the exclusion list, stored in the saved index so changing the list
    /// invalidates it. 0 when nothing is excluded (also what older saves have).
    pub fn fingerprint(&self) -> u32 {
        if self.excluded.is_empty() {
            return 0;
        }
        let mut keys: Vec<String> = self.excluded.iter().map(|p| path_key(p)).collect();
        keys.sort();
        keys.dedup();
        let hash = keys.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, k| {
            k.bytes()
                .chain(std::iter::once(0))
                .fold(h, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
        });
        // 31 bits (the lowest bit of the saved options field is "skip build folders"); never 0.
        ((hash ^ (hash >> 31) ^ (hash >> 62)) as u32 & 0x7FFF_FFFF).max(1)
    }

    /// Whether this folder is one the user excluded (not entered).
    fn excludes(&self, dir: &Path) -> bool {
        !self.excluded.is_empty() && {
            let key = path_key(dir);
            self.excluded.iter().any(|e| path_key(e) == key)
        }
    }

    /// Whether the path is inside an excluded folder (the folder itself doesn't count); decides
    /// whether watch or journal changes are recorded.
    pub fn is_inside_excluded(&self, path: &Path) -> bool {
        !self.excluded.is_empty() && {
            let key = path_key(path);
            self.excluded.iter().any(|e| {
                let e = path_key(e);
                key.len() > e.len()
                    && key.starts_with(&e)
                    && (e.ends_with('\\') || key.as_bytes()[e.len()] == b'\\')
            })
        }
    }
}

/// Lists a whole root in one go (see [`crate::journal::Journal::list`]).
pub type Lister<'a> = &'a (dyn Fn(&Path, ListSink) -> io::Result<()> + Sync);

/// A built index. Immutable once built; changes go into the [`Overlay`].
#[derive(Default, Debug)]
pub struct Index {
    roots: Vec<PathBuf>,
    nodes: Column<Node>,
    sizes: Column<u64>,
    /// Modified time in Unix seconds, 0 if unknown.
    mtimes: Column<u32>,
    /// Char mask of each name ([`char_mask`]).
    masks: Column<u32>,
    names: Names,
    /// Build start time (Unix seconds).
    built_at: u64,
    /// Journal position per root before walking (only with the background service); the next
    /// start replays changes from here.
    cursors: Vec<Option<Cursor>>,
    /// Whether build folders were skipped (a settings change invalidates the save).
    skipped_build_dirs: bool,
    /// Exclusions used for the build ([`BuildRules::fingerprint`]; 0 for none).
    excluded_hash: u32,
    /// Build-time only: one node per distinct name, found by [`name_hash`], so each name is
    /// stored once in the pool. Holds just the node id (4 bytes a name; a map keyed by the hash
    /// would take 16). Dropped after the build.
    dedup: Option<hashbrown::HashTable<u32>>,
}

/// Build progress: items listed so far.
#[derive(Default, Debug)]
pub struct BuildProgress {
    /// Items listed so far; updated from the walker threads.
    pub scanned: AtomicUsize,
    /// Items and name bytes of the index this build replaces (0 for a first build). Memory for
    /// about that much is set aside up front instead of growing into it.
    pub expected: (AtomicUsize, AtomicUsize),
    /// Folders a refresh took over from the old index without listing them (see
    /// [`Index::refresh`]).
    pub reused: AtomicUsize,
    /// Roots that couldn't be listed (see [`Index::roots_failed`]).
    pub failed_roots: AtomicUsize,
    /// Set by the caller when there's no index to search yet: the walk then puts a copy of what
    /// it has so far in `partial` every few seconds.
    pub want_partial: AtomicBool,
    /// What the walk has so far (see `want_partial`); emptied when it ends.
    pub partial: parking_lot::Mutex<Option<Arc<Index>>>,
}

/// One item from [`Index::items_under`].
#[derive(Clone, Debug)]
pub struct IndexedItem {
    /// Path relative to the folder asked about (`sub\file.txt`).
    pub rel: String,
    /// The item as a listing gives it; of the attributes only Hidden is known.
    pub entry: Entry,
}

/// What [`Index::items_under`] has below a folder.
#[derive(Clone, Debug, Default)]
pub struct IndexedTree {
    /// Every item the index has there.
    pub items: Vec<IndexedItem>,
    /// Folders there whose contents the index doesn't have (build folders with "skip build
    /// folders", exclusions, hidden system folders), relative like the items: a search walks
    /// these itself. Snapshots and recycle bins aren't among them.
    pub unindexed: Vec<String>,
}

impl IndexedItem {
    fn new(rel: String, name: &str, kind: EntryKind, size: u64, mtime: u32, hidden: bool) -> Self {
        /// `FILE_ATTRIBUTE_HIDDEN`.
        const HIDDEN: u32 = 0x2;
        Self {
            rel,
            entry: Entry::new(
                name,
                kind,
                size,
                to_system_time(mtime),
                if hidden { HIDDEN } else { 0 },
            ),
        }
    }
}

/// One folder's listing during a walk.
enum Listing {
    Complete(Vec<Entry>),
    /// The listing ended with an error partway (the connection dropped): what came before.
    Incomplete(Vec<Entry>),
    /// The folder couldn't be listed at all.
    Failed,
}

impl Listing {
    fn of(listed: io::Result<crate::fs::DirIter>) -> Self {
        let Ok(iter) = listed else {
            return Self::Failed;
        };
        let mut entries = Vec::new();
        let mut complete = true;
        for item in iter {
            match item {
                Ok(entry) => entries.push(entry),
                Err(_) => complete = false,
            }
        }
        if complete {
            Self::Complete(entries)
        } else {
            Self::Incomplete(entries)
        }
    }
}

/// The previous index during [`Index::refresh`]: which of its folders each new folder is, and
/// what they held.
struct Reuse<'a> {
    old: &'a Index,
    /// Children of old node `p`: `kids[offsets[p]..offsets[p + 1]]` (a merged index appends
    /// changed items at the end, so children aren't always next to each other).
    offsets: Vec<u32>,
    kids: Vec<u32>,
    /// New folder id -> the same folder in `old`.
    old_of: HashMap<u32, u32>,
    /// Folder times (Unix seconds) from this on aren't trusted: the folder may have changed in
    /// the same second `old` listed it, by the server's clock, which may be off from this one
    /// (see [`CLOCK_MARGIN`]).
    trusted_before: u32,
    /// Folders (lowercase full paths) listed again whatever their time says: changes were seen
    /// in them since `old` was built.
    relist: HashSet<String>,
}

/// How far a file server's clock may be off from this one: folders changed this close to the
/// last walk are listed again.
const CLOCK_MARGIN: u64 = 60 * 60;
/// `FILE_ATTRIBUTE_REPARSE_POINT`: such a folder (a DFS link, a mount point) may show another
/// folder's contents without its own time ever changing.
const ATTR_REPARSE_POINT: u32 = 0x400;

impl<'a> Reuse<'a> {
    fn new(old: &'a Index, relist: &[PathBuf]) -> Self {
        let n = old.nodes.len();
        let mut offsets = vec![0u32; n + 1];
        for node in old.nodes.iter() {
            if let Some(slot) = offsets.get_mut(node.parent as usize + 1) {
                *slot += 1;
            }
        }
        for i in 1..=n {
            offsets[i] += offsets[i - 1];
        }
        let mut fill = offsets.clone();
        let mut kids = vec![0u32; offsets[n] as usize];
        for (i, node) in old.nodes.iter().enumerate() {
            if let Some(at) = fill.get_mut(node.parent as usize) {
                kids[*at as usize] = u32::try_from(i).unwrap_or(NO_PARENT);
                *at += 1;
            }
        }
        Self {
            old,
            offsets,
            kids,
            old_of: HashMap::new(),
            trusted_before: u32::try_from(old.built_at.saturating_sub(CLOCK_MARGIN))
                .unwrap_or(u32::MAX),
            relist: relist.iter().map(|p| path_key(p)).collect(),
        }
    }

    fn children(&self, old: u32) -> &[u32] {
        let i = old as usize;
        &self.kids[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    /// New root `id` is `old`'s root of the same name.
    fn add_root(&mut self, id: u32, name: &str) {
        let old = self.old;
        if let Some(was) = old
            .nodes
            .iter()
            .position(|n| n.parent == NO_PARENT && old.name(n) == name)
        {
            self.old_of
                .insert(id, u32::try_from(was).unwrap_or(NO_PARENT));
        }
    }

    /// What the old copy of new folder `parent` held, by name (empty if it had none).
    fn children_by_name(&self, parent: u32) -> HashMap<&'a str, u32> {
        let old = self.old;
        self.old_of.get(&parent).map_or_default(|&was| {
            self.children(was)
                .iter()
                .map(|&c| (old.name(&old.nodes[c as usize]), c))
                .collect()
        })
    }

    /// New folders (id, old id) that are being listed again.
    fn listing(&mut self, listed: &[(u32, u32)]) {
        self.old_of.extend(listed.iter().copied());
    }

    /// Whether folder `entry`, just listed in its parent, still holds what old node `was` holds:
    /// a plain folder there too, its time unchanged (and from before `old` was walked), with no
    /// subfolders that were walked into, and not empty (a folder that couldn't be listed looks
    /// empty, so empty ones are always listed again).
    fn unchanged_leaf(&self, was: u32, entry: &Entry, path: &Path) -> bool {
        let old = self.old;
        let i = was as usize;
        let mtime = entry
            .modified
            .map_or(0, |t| u32::try_from(unix_secs(t)).unwrap_or(u32::MAX));
        let walked = |n: &Node| n.flags & (FLAG_DIR | FLAG_LINK | FLAG_PRUNED) == FLAG_DIR;
        let kids = self.children(was);
        entry.kind == EntryKind::Dir
            && entry.attributes & ATTR_REPARSE_POINT == 0
            && walked(&old.nodes[i])
            && mtime != 0
            && mtime < self.trusted_before
            && old.mtimes[i] == mtime
            && !kids.is_empty()
            && !kids.iter().any(|&c| walked(&old.nodes[c as usize]))
            && (self.relist.is_empty() || !self.relist.contains(&path_key(path)))
    }

    /// The old copy of new folder `id`, if there is one.
    fn old_of(&self, id: u32) -> Option<u32> {
        self.old_of.get(&id).copied()
    }

    /// Copies everything below old folder `was` (any depth) under new folder `id`, for a folder
    /// that couldn't be listed this time; returns how many.
    fn copy_subtree(&self, was: u32, id: u32, index: &mut Index) -> usize {
        let old = self.old;
        let mut queue = std::collections::VecDeque::from([(was, id)]);
        let mut copied = 0;
        while let Some((was, id)) = queue.pop_front() {
            for &c in self.children(was) {
                let node = old.nodes[c as usize];
                let i = c as usize;
                let Some(new) =
                    index.push(id, old.name(&node), node.flags, old.sizes[i], old.mtimes[i])
                else {
                    continue;
                };
                copied += 1;
                if node.flags & (FLAG_DIR | FLAG_LINK | FLAG_PRUNED) == FLAG_DIR {
                    queue.push_back((c, new));
                }
            }
        }
        copied
    }

    /// Copies old folder `was`'s contents under new folder `id`; returns how many.
    fn copy_children(&self, was: u32, id: u32, index: &mut Index) -> usize {
        let old = self.old;
        let kids = self.children(was);
        for &c in kids {
            let c = c as usize;
            let node = old.nodes[c];
            index.push(id, old.name(&node), node.flags, old.sizes[c], old.mtimes[c]);
        }
        kids.len()
    }
}

/// One query.
pub struct Search<'a> {
    /// Parsed query.
    pub query: &'a Query,
    /// Maximum number of hits returned (best scores first).
    pub limit: usize,
    /// Include hidden items.
    pub show_hidden: bool,
    /// Maximum scanning threads (at least one is used).
    pub threads: usize,
    /// Search only these items (the previous matches, see [`SearchResult::matched`]).
    pub within: Option<&'a [u32]>,
    /// Return every match (for narrowing the next query).
    pub collect_matched: bool,
    /// Set by the caller to give up (a newer query replaced this one); checked between chunks.
    pub cancel: Option<&'a AtomicBool>,
}

/// Output of [`Index::search`].
pub struct SearchResult {
    /// Best hits, highest score first, at most `limit`.
    pub hits: Vec<IndexHit>,
    /// All matching ids, ascending; only with `collect_matched`.
    pub matched: Option<Vec<u32>>,
    /// Stopped early through [`Search::cancel`]: `hits` and `matched` are incomplete.
    pub cancelled: bool,
}

/// Threads that scan the index for queries, started on the first query and kept: starting a
/// thread per core for every keystroke cost more than scanning millions of names.
fn scan_pool() -> Option<&'static rayon_core::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon_core::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        rayon_core::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("index-scan-{i}"))
            .build()
            .inspect_err(|err| log::warn!("no search thread pool, scanning on one thread: {err}"))
            .ok()
    })
    .as_ref()
}

thread_local! {
    /// Scan scratch kept per pool thread (the matcher allocates on creation).
    static WORK: std::cell::RefCell<Option<Work>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn to_system_time(secs: u32) -> Option<SystemTime> {
    (secs != 0).then(|| UNIX_EPOCH + Duration::from_secs(u64::from(secs)))
}

impl Index {
    /// Number of indexed items (files and folders).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// No items at all.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Bytes of the name pool (each distinct name once).
    pub fn name_bytes(&self) -> usize {
        self.names.len()
    }

    /// Root paths this index covers, in build order.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// When building this index started.
    pub fn built_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.built_at)
    }

    /// Whether build folders were skipped.
    pub const fn skipped_build_dirs(&self) -> bool {
        self.skipped_build_dirs
    }

    /// Exclusion list fingerprint at build time (see [`BuildRules::fingerprint`]).
    pub const fn excluded_hash(&self) -> u32 {
        self.excluded_hash
    }

    /// Journal position of root `i` before the build.
    pub fn cursor(&self, i: usize) -> Option<Cursor> {
        self.cursors.get(i).copied().flatten()
    }

    /// Records each root's journal position before the build (same order as `roots`).
    #[must_use]
    pub fn with_cursors(mut self, cursors: Vec<Option<Cursor>>) -> Self {
        self.cursors = cursors;
        self
    }

    /// Approximate heap usage in bytes (mapped parts not counted).
    pub const fn heap_bytes(&self) -> usize {
        self.names.heap_capacity()
            + self.nodes.heap_capacity() * size_of::<Node>()
            + self.sizes.heap_capacity() * 8
            + self.mtimes.heap_capacity() * 4
            + self.masks.heap_capacity() * 4
    }

    fn name(&self, node: &Node) -> &str {
        let start = node.name_start as usize;
        &self.names[start..start + node.name_len as usize]
    }

    fn push(&mut self, parent: u32, name: &str, flags: u8, size: u64, mtime: u32) -> Option<u32> {
        let id = u32::try_from(self.nodes.len())
            .ok()
            .filter(|&id| id != NO_PARENT)?;
        let name_len = u16::try_from(name.len()).ok()?;
        // Repeated names (`index.js`, `package.json`, `mod.rs`...) are stored once in the pool,
        // which saves more than half on a developer's disk.
        let name_start = if let Some(dedup) = self.dedup.as_mut() {
            let (nodes, names) = (&*self.nodes, &*self.names);
            let name_of = |id: u32| {
                let node = nodes[id as usize];
                let start = node.name_start as usize;
                &names[start..start + node.name_len as usize]
            };
            match dedup.entry(
                name_hash(name),
                |&id| name_of(id) == name,
                |&id| name_hash(name_of(id)),
            ) {
                hashbrown::hash_table::Entry::Occupied(known) => {
                    nodes[*known.get() as usize].name_start
                }
                hashbrown::hash_table::Entry::Vacant(slot) => {
                    let start = u32::try_from(names.len()).ok()?;
                    slot.insert(id);
                    self.names.push_str(name);
                    start
                }
            }
        } else {
            let start = u32::try_from(self.names.len()).ok()?;
            self.names.push_str(name);
            start
        };
        self.nodes.push(Node {
            parent,
            name_start,
            name_len,
            flags,
        });
        self.sizes.push(size);
        self.mtimes.push(mtime);
        self.masks.push(char_mask(name));
        Some(id)
    }

    /// Full path of item `id`.
    pub fn path_of(&self, id: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while let Some(node) = self.nodes.get(cur as usize) {
            parts.push(self.name(node));
            if node.parent == NO_PARENT {
                break;
            }
            cur = node.parent;
        }
        parts.iter().rev().collect()
    }

    /// Walks `roots` to build the index with `threads` listing in parallel. Setting `cancel` stops
    /// early and returns what was gathered. `thread_init` runs at the start of each walker thread
    /// (the app lowers I/O priority there).
    pub fn build(
        fs: &dyn Fs,
        roots: &[PathBuf],
        threads: usize,
        cancel: &AtomicBool,
        progress: &BuildProgress,
        thread_init: &(dyn Fn() + Sync),
        skip_build_dirs: bool,
    ) -> Self {
        Self::build_listed(
            fs,
            roots,
            threads,
            cancel,
            progress,
            thread_init,
            &BuildRules {
                skip_build_dirs,
                ..BuildRules::default()
            },
            &|_, _| Err(io::ErrorKind::Unsupported.into()),
        )
    }

    /// Like [`Index::build`], but each root is first offered to `list` (the background service
    /// reads the MFT and lists a whole drive in seconds). Roots it can't list (no service, not
    /// NTFS, failure midway) are walked.
    #[allow(
        clippy::too_many_arguments,
        reason = "one entry point for every build input"
    )]
    pub fn build_listed(
        fs: &dyn Fs,
        roots: &[PathBuf],
        threads: usize,
        cancel: &AtomicBool,
        progress: &BuildProgress,
        thread_init: &(dyn Fn() + Sync),
        rules: &BuildRules,
        list: Lister,
    ) -> Self {
        Self::walk(
            fs,
            roots,
            threads,
            cancel,
            progress,
            thread_init,
            rules,
            list,
            None,
        )
    }

    /// Whether the walk behind `progress` couldn't list one of its roots (an offline share):
    /// what it built is empty there and must not replace a good index.
    pub fn roots_failed(progress: &BuildProgress) -> bool {
        progress.failed_roots.load(Ordering::Relaxed) > 0
    }

    /// Walks `old`'s roots again, but takes a folder's contents from `old` instead of listing it
    /// when they can't have changed: a folder without subfolders whose modified time is the same
    /// as when `old` listed it (adding, removing or renaming an item inside a folder changes its
    /// time; that is all a folder's time says). Every other folder is listed: a change deep
    /// inside a folder doesn't change the folder's own time, so only the bottom level can be
    /// trusted. Taken over as they were, in folders not listed again: sizes and times of files
    /// changed in place, and changed attributes. Folders reused are counted in
    /// [`BuildProgress::reused`]. Folders in `relist` (changes were seen there) are listed again
    /// whatever their time; a folder that can't be listed keeps what `old` had below it.
    #[allow(
        clippy::too_many_arguments,
        reason = "the build's inputs plus the old index"
    )]
    pub fn refresh(
        fs: &dyn Fs,
        old: &Self,
        relist: &[PathBuf],
        threads: usize,
        cancel: &AtomicBool,
        progress: &BuildProgress,
        thread_init: &(dyn Fn() + Sync),
        rules: &BuildRules,
    ) -> Self {
        Self::walk(
            fs,
            &old.roots,
            threads,
            cancel,
            progress,
            thread_init,
            rules,
            &|_, _| Err(io::ErrorKind::Unsupported.into()),
            Some((old, relist)),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one entry point for every build input; the walk reads best in one place"
    )]
    fn walk(
        fs: &dyn Fs,
        roots: &[PathBuf],
        threads: usize,
        cancel: &AtomicBool,
        progress: &BuildProgress,
        thread_init: &(dyn Fn() + Sync),
        rules: &BuildRules,
        list: Lister,
        old: Option<(&Self, &[PathBuf])>,
    ) -> Self {
        let mut reuse = old.map(|(old, relist)| Reuse::new(old, relist));
        let mut index = Self {
            roots: roots.to_vec(),
            built_at: unix_secs(SystemTime::now()),
            skipped_build_dirs: rules.skip_build_dirs,
            excluded_hash: rules.fingerprint(),
            ..Self::default()
        };
        index.reserve(
            progress.expected.0.load(Ordering::Relaxed),
            progress.expected.1.load(Ordering::Relaxed),
        );
        // Work: folders to list (node id, path). Results: their contents.
        let (work_tx, work_rx) = flume::unbounded::<(u32, PathBuf)>();
        let (done_tx, done_rx) = flume::unbounded::<(u32, Listing)>();
        let mut pending = 0usize;
        for root in roots {
            let name = root.to_string_lossy();
            let Some(id) = index.push(NO_PARENT, &name, FLAG_DIR, 0, 0) else {
                continue;
            };
            if let Some(reuse) = reuse.as_mut() {
                reuse.add_root(id, &name);
            }
            if cancel.load(Ordering::Relaxed) {
                continue;
            }
            match index.add_listing(id, root, list, cancel, progress, rules) {
                Ok(n) => log::info!("global search: {} MFT listing, {n} items", root.display()),
                Err(err) => {
                    if err.kind() != io::ErrorKind::Unsupported {
                        log::warn!(
                            "global search: MFT listing failed for {}, walking instead: {err}",
                            root.display()
                        );
                    }
                    pending += 1;
                    let _ = work_tx.send((id, root.clone()));
                }
            }
        }
        // Set once the index reaches `rules.max_items`: queued folders are no longer listed.
        let full = AtomicBool::new(false);
        let mut published: Option<(std::time::Instant, usize)> = None;
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                let work_rx = work_rx.clone();
                let done_tx = done_tx.clone();
                let full = &full;
                scope.spawn(move || {
                    thread_init();
                    while let Ok((id, path)) = work_rx.recv() {
                        let listed =
                            if cancel.load(Ordering::Relaxed) || full.load(Ordering::Relaxed) {
                                Listing::Complete(Vec::new())
                            } else {
                                Listing::of(fs.read_dir(&path))
                            };
                        if done_tx.send((id, listed)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(done_tx);
            // Collector: this thread owns the index, assigns ids to subfolders and sends them back
            // to be listed.
            while pending > 0 {
                let Ok((parent, listed)) = done_rx.recv() else {
                    break;
                };
                pending -= 1;
                let entries = match listed {
                    Listing::Complete(entries) => entries,
                    Listing::Incomplete(entries) => {
                        // Not all of it: never trust this folder's time to skip it next time.
                        index.mtimes.owned()[parent as usize] = 0;
                        entries
                    }
                    Listing::Failed => {
                        if index.nodes[parent as usize].parent == NO_PARENT {
                            progress.failed_roots.fetch_add(1, Ordering::Relaxed);
                        } else if let Some(reuse) = reuse.as_ref()
                            && let Some(was) = reuse.old_of(parent)
                        {
                            // Unreadable now (the share dropped, access changed): keep what the
                            // last walk found there rather than nothing.
                            let kept = reuse.copy_subtree(was, parent, &mut index);
                            progress.scanned.fetch_add(kept, Ordering::Relaxed);
                        }
                        continue;
                    }
                };
                if full.load(Ordering::Relaxed) {
                    // Past the limit: only drain what was already queued.
                    continue;
                }
                if rules.max_items.is_some_and(|max| index.len() >= max) {
                    full.store(true, Ordering::Relaxed);
                    log::warn!(
                        "global search: {} has more than {} items, indexing stops there",
                        roots_label(roots),
                        index.len()
                    );
                    continue;
                }
                progress.scanned.fetch_add(entries.len(), Ordering::Relaxed);
                let parent_path = index.path_of(parent);
                // With an old index: what its copy of this folder held, by name.
                let before = reuse.as_mut().map(|r| r.children_by_name(parent));
                let mut unchanged: Vec<(u32, u32)> = Vec::new();
                let mut listed: Vec<(u32, u32)> = Vec::new();
                index.add_entries(parent, Some(&parent_path), &entries, rules, |id, entry| {
                    if cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    if let (Some(reuse), Some(before)) = (reuse.as_ref(), before.as_ref())
                        && let Some(&was) = before.get(&*entry.name)
                    {
                        if reuse.unchanged_leaf(was, entry, &parent_path.join(&*entry.name)) {
                            unchanged.push((id, was));
                            return true;
                        }
                        listed.push((id, was));
                    }
                    pending += 1;
                    let _ = work_tx.send((id, parent_path.join(&*entry.name)));
                    true
                });
                if let Some(reuse) = reuse.as_mut() {
                    reuse.listing(&listed);
                    for (id, was) in unchanged {
                        let copied = reuse.copy_children(was, id, &mut index);
                        progress.scanned.fetch_add(copied, Ordering::Relaxed);
                        progress.reused.fetch_add(1, Ordering::Relaxed);
                    }
                }
                index.publish_partial(progress, &mut published);
            }
            drop(work_tx);
        });
        progress.partial.lock().take();
        index.dedup = None;
        // Not shrunk here: it's usually saved and mapped right away, and shrinking copies every
        // column while the old one is still allocated (see [`Index::shrink_to_fit`]).
        index
    }

    /// While a first build runs (`progress.want_partial`), hands a copy of what's walked so far to
    /// `progress.partial` now and then, so searches find something before the walk ends. Spaced
    /// out by how long a copy takes, so copying never costs the walk more than a few percent, and
    /// by size: a new copy only once the walk has a quarter more than the last one, so the copies
    /// add up to a few times the final index at most. The previous copy is let go first.
    fn publish_partial(
        &self,
        progress: &BuildProgress,
        last: &mut Option<(std::time::Instant, usize)>,
    ) {
        const EVERY: Duration = if cfg!(test) {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(2)
        };
        if !progress.want_partial.load(Ordering::Relaxed)
            || last.is_some_and(|(at, len)| at.elapsed() < EVERY || self.len() < len + len / 4)
        {
            return;
        }
        let started = std::time::Instant::now();
        progress.partial.lock().take();
        let copy = Arc::new(self.snapshot());
        *progress.partial.lock() = Some(copy);
        // The next copy waits at least 20 times as long as this one took.
        *last = Some((
            std::time::Instant::now() + started.elapsed() * 20,
            self.len(),
        ));
    }

    /// An owned copy of the walk so far (see [`Self::publish_partial`]).
    fn snapshot(&self) -> Self {
        Self {
            roots: self.roots.clone(),
            nodes: Column::Owned(self.nodes.to_vec()),
            sizes: Column::Owned(self.sizes.to_vec()),
            mtimes: Column::Owned(self.mtimes.to_vec()),
            masks: Column::Owned(self.masks.to_vec()),
            names: Names::Owned(self.names.to_string()),
            built_at: self.built_at,
            cursors: Vec::new(),
            skipped_build_dirs: self.skipped_build_dirs,
            excluded_hash: self.excluded_hash,
            dedup: None,
        }
    }

    /// Everything the index has below `dir`, at any depth, with `overlay`'s changes applied: each
    /// item's path relative to `dir` and an entry like a listing gives (attributes: only Hidden).
    /// Without `show_hidden`, hidden items and everything inside hidden folders are left out, as
    /// a walk leaves them. Folders below that the index doesn't enter are listed in
    /// [`IndexedTree::unindexed`]. `None` when the index doesn't have `dir`'s contents (not under
    /// a root, a folder it doesn't enter, gone since the build) or when cancelled.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass over the index, then the changes it lacks"
    )]
    pub fn items_under(
        &self,
        dir: &Path,
        overlay: &Overlay,
        show_hidden: bool,
        cancel: &AtomicBool,
    ) -> Option<IndexedTree> {
        // The folder itself (or one above it) is gone since the build.
        if overlay.is_removed(dir) {
            return None;
        }
        let top = self.resolve(dir, cancel)?;
        let key = |rel: &str| rel.to_lowercase();
        // Changes since the build below `dir`, by relative path (lowercase).
        let mut added: HashMap<String, (String, Seen)> = HashMap::new();
        let mut removed: HashSet<String> = HashSet::new();
        for (path, (seen, _)) in &overlay.added {
            if let Some(rel) = rel_components(path, dir).filter(|r| !r.is_empty()) {
                let rel = rel.join("\\");
                added.insert(key(&rel), (rel, *seen));
            }
        }
        for path in overlay.removed.keys() {
            if let Some(rel) = rel_components(path, dir) {
                removed.insert(key(&rel.join("\\")));
            }
        }
        // Hidden folders left out (lowercase relative paths), so changes recorded inside them
        // are left out too.
        let mut hidden_folders: HashSet<String> = HashSet::new();
        let track_hidden = !show_hidden && !added.is_empty();
        // Folders inside `dir` that are walked into, with their relative path.
        let mut folders: HashMap<u32, String> = HashMap::from([(top, String::new())]);
        let mut tree = IndexedTree::default();
        for (i, node) in self.nodes.iter().enumerate().skip(top as usize + 1) {
            if i % (64 * 1024) == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Some(parent_rel) = folders.get(&node.parent) else {
                continue;
            };
            let name = self.name(node);
            let rel = if parent_rel.is_empty() {
                name.to_string()
            } else {
                format!("{parent_rel}\\{name}")
            };
            // Lowercase only when there are changes to look up (a path per item otherwise).
            let lower = if added.is_empty() && removed.is_empty() {
                String::new()
            } else {
                key(&rel)
            };
            if removed.contains(&lower) {
                continue;
            }
            let indexed_dir = node.flags & FLAG_DIR != 0;
            let mut kind = if node.flags & FLAG_LINK != 0 {
                EntryKind::Link {
                    is_dir: indexed_dir,
                }
            } else if indexed_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let changed = if added.is_empty() {
                None
            } else {
                added.remove(&lower)
            };
            let (size, mtime, hidden) = match changed {
                Some((_, seen)) => {
                    // A file replaced by a folder of the same name, or the other way round.
                    if seen.is_dir != indexed_dir {
                        kind = if seen.is_dir {
                            EntryKind::Dir
                        } else {
                            EntryKind::File
                        };
                    }
                    (
                        seen.size,
                        u32::try_from(seen.modified).unwrap_or(u32::MAX),
                        seen.hidden,
                    )
                }
                None => (self.sizes[i], self.mtimes[i], node.flags & FLAG_HIDDEN != 0),
            };
            if hidden && !show_hidden {
                if track_hidden && kind != EntryKind::File {
                    hidden_folders.insert(key(&rel));
                }
                continue;
            }
            let id = u32::try_from(i).ok()?;
            if kind == EntryKind::Dir {
                if node.flags & (FLAG_DIR | FLAG_LINK | FLAG_PRUNED) == FLAG_DIR {
                    folders.insert(id, rel.clone());
                } else if node.flags & FLAG_PRUNED != 0 && !is_skipped_dir(&name.to_lowercase()) {
                    // Not entered by the index (build folders, exclusions, hidden system
                    // folders), but a walk would find what's inside.
                    tree.unindexed.push(rel.clone());
                }
            }
            tree.items
                .push(IndexedItem::new(rel, name, kind, size, mtime, hidden));
        }
        // Recorded since the build and not in the index: new items.
        let mut fresh: Vec<(String, Seen)> = added.into_values().collect();
        fresh.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, seen) in fresh {
            let gone_or_hidden = Path::new(&rel).ancestors().skip(1).any(|a| {
                let a = key(&a.to_string_lossy());
                removed.contains(&a) || hidden_folders.contains(&a)
            });
            if (seen.hidden && !show_hidden) || gone_or_hidden {
                if seen.is_dir && seen.hidden && track_hidden {
                    hidden_folders.insert(key(&rel));
                }
                continue;
            }
            let name = Path::new(&rel)
                .file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned());
            let kind = if seen.is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let mtime = u32::try_from(seen.modified).unwrap_or(u32::MAX);
            tree.items.push(IndexedItem::new(
                rel,
                &name,
                kind,
                seen.size,
                mtime,
                seen.hidden,
            ));
        }
        Some(tree)
    }

    /// Sets aside memory for `items` items with `name_bytes` of names (a little over, so a
    /// rebuild that found a few more doesn't double everything), and starts the name dedup.
    fn reserve(&mut self, items: usize, name_bytes: usize) {
        let (items, name_bytes) = (items + items / 16, name_bytes + name_bytes / 16);
        self.nodes.owned().reserve_exact(items);
        self.sizes.owned().reserve_exact(items);
        self.mtimes.owned().reserve_exact(items);
        self.masks.owned().reserve_exact(items);
        self.names.reserve_exact(name_bytes);
        // About a third of the names on a disk are distinct.
        self.dedup = Some(hashbrown::HashTable::with_capacity(items / 3));
    }

    /// Gives back the memory a build or merge set aside beyond what it used. Only for an index
    /// kept in memory (not saved): copying every column briefly needs both copies.
    pub fn shrink_to_fit(&mut self) {
        self.names.shrink_to_fit();
        self.nodes.shrink_to_fit();
        self.sizes.shrink_to_fit();
        self.mtimes.shrink_to_fit();
        self.masks.shrink_to_fit();
    }

    /// Adds one folder's contents. Subfolders go through `descend` (true means it will be entered).
    /// Returns each entry's id; ones not entered (files, links, skipped folders, failures) get
    /// [`NO_PARENT`]. `parent_path` is only needed when there are excluded folders.
    fn add_entries(
        &mut self,
        parent: u32,
        parent_path: Option<&Path>,
        entries: &[Entry],
        rules: &BuildRules,
        mut descend: impl FnMut(u32, &Entry) -> bool,
    ) -> Vec<u32> {
        let skip_build_dirs = rules.skip_build_dirs;
        let siblings: Vec<&str> = if skip_build_dirs {
            entries.iter().map(|e| e.sort_key.as_ref()).collect()
        } else {
            Vec::new()
        };
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let build_dir = entry.kind == EntryKind::Dir
                && ((skip_build_dirs
                    && is_build_dir(&entry.sort_key, &mut siblings.iter().copied()))
                    || parent_path.is_some_and(|p| rules.excludes(&p.join(&*entry.name))));
            let mut flags = 0;
            if entry.is_dir_like() {
                flags |= FLAG_DIR;
            }
            if entry.is_hidden() {
                flags |= FLAG_HIDDEN;
            }
            if matches!(entry.kind, EntryKind::Link { .. }) {
                flags |= FLAG_LINK;
            }
            let size = if flags & FLAG_DIR == 0 { entry.size } else { 0 };
            let mtime = entry
                .modified
                .map_or(0, |t| u32::try_from(unix_secs(t)).unwrap_or(u32::MAX));
            let id = match self.push(parent, &entry.name, flags, size, mtime) {
                Some(id) if descends_into(entry) && !build_dir && descend(id, entry) => id,
                Some(id) => {
                    if entry.kind == EntryKind::Dir {
                        self.nodes.owned()[id as usize].flags |= FLAG_PRUNED;
                    }
                    NO_PARENT
                }
                None => NO_PARENT,
            };
            out.push(id);
        }
        out
    }

    /// Adds a whole-root listing from `list` under node `root_id` and returns how many were added.
    /// On failure (unsupported, disconnect, cancel) the partial additions are removed, leaving the
    /// index as before.
    fn add_listing(
        &mut self,
        root_id: u32,
        root: &Path,
        list: Lister,
        cancel: &AtomicBool,
        progress: &BuildProgress,
        rules: &BuildRules,
    ) -> io::Result<usize> {
        let (nodes_before, names_before) = (self.nodes.len(), self.names.len());
        // Listing position -> index id (NO_PARENT for folders not entered and files: nothing below
        // is added).
        let mut ids: Vec<u32> = Vec::new();
        // Collect a folder's children (contiguous in the listing) before adding: build folder
        // detection needs siblings.
        let mut group: Vec<Listed> = Vec::new();
        let flush = |index: &mut Self, group: &mut Vec<Listed>, ids: &mut Vec<u32>| {
            let Some(first) = group.first() else {
                return;
            };
            let parent = match first.parent {
                LISTED_ROOT => root_id,
                p => ids.get(p as usize).copied().unwrap_or(NO_PARENT),
            };
            if parent == NO_PARENT {
                ids.extend(std::iter::repeat_n(NO_PARENT, group.len()));
            } else {
                let entries: Vec<Entry> = group.iter().map(Listed::to_entry).collect();
                // The parent path is only needed with exclusions (computed once per folder).
                let parent_path = (!rules.excluded.is_empty()).then(|| index.path_of(parent));
                ids.extend(index.add_entries(
                    parent,
                    parent_path.as_deref(),
                    &entries,
                    rules,
                    |_, _| true,
                ));
            }
            group.clear();
        };
        let result = list(root, &mut |batch: Vec<Listed>| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            progress.scanned.fetch_add(batch.len(), Ordering::Relaxed);
            for item in batch {
                if group.last().is_some_and(|g| g.parent != item.parent) {
                    flush(self, &mut group, &mut ids);
                }
                group.push(item);
            }
            true
        });
        flush(self, &mut group, &mut ids);
        if let Err(err) = result {
            self.truncate(nodes_before, names_before);
            return Err(err);
        }
        Ok(self.nodes.len() - nodes_before)
    }

    /// Removes everything added after node `nodes` (and shrinks the name pool back to `names`
    /// bytes).
    fn truncate(&mut self, nodes: usize, names: usize) {
        self.nodes.owned().truncate(nodes);
        self.sizes.owned().truncate(nodes);
        self.mtimes.owned().truncate(nodes);
        self.masks.owned().truncate(nodes);
        self.names.truncate(names);
        // Dedup entries for the removed nodes are no longer valid.
        if let Some(dedup) = self.dedup.as_mut() {
            dedup.retain(|id| (*id as usize) < nodes);
        }
    }

    /// Node of `dir` (any letter case), if the index has its contents.
    fn resolve(&self, dir: &Path, cancel: &AtomicBool) -> Option<u32> {
        let (root, rel) = self
            .roots
            .iter()
            .find_map(|r| rel_components(dir, r).map(|rel| (r, rel)))?;
        let root_name = root.to_string_lossy();
        let mut cur = NO_PARENT;
        let mut depth = 0;
        for (i, node) in self.nodes.iter().enumerate() {
            if i % (64 * 1024) == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let hit = if cur == NO_PARENT {
                node.parent == NO_PARENT && self.name(node) == root_name
            } else {
                node.parent == cur
                    && depth < rel.len()
                    && eq_ignore_case(self.name(node), &rel[depth])
            };
            if hit {
                if cur != NO_PARENT {
                    depth += 1;
                }
                cur = u32::try_from(i).ok()?;
                if depth == rel.len() {
                    break;
                }
            }
        }
        let node = self.nodes.get(cur as usize)?;
        (depth == rel.len() && node.flags & (FLAG_PRUNED | FLAG_LINK) == 0).then_some(cur)
    }

    /// What the index holds directly inside each folder of `dirs`: (name, is a folder) per entry,
    /// for the folders the index has with their contents (others are left out). One pass over the
    /// index for all of them, so a batch of changed folders costs one pass.
    ///
    /// Relies on parents being added before their children, which both build paths do.
    pub fn children_of(
        &self,
        dirs: &[PathBuf],
        cancel: &AtomicBool,
    ) -> Vec<(PathBuf, Vec<(String, bool)>)> {
        #[derive(Default)]
        struct Step {
            next: HashMap<String, usize>,
            /// Index into `dirs` when this step is one of them.
            target: Option<usize>,
        }
        // The folders as paths of lowercase names under their root's step.
        let mut steps: Vec<Step> = Vec::new();
        let mut root_steps: Vec<(String, usize)> = Vec::new();
        for (target, dir) in dirs.iter().enumerate() {
            let Some((root, rel)) = self
                .roots
                .iter()
                .find_map(|r| rel_components(dir, r).map(|rel| (r, rel)))
            else {
                continue;
            };
            let root_name = root.to_string_lossy().into_owned();
            let mut cur = if let Some(&(_, step)) = root_steps.iter().find(|(n, _)| *n == root_name)
            {
                step
            } else {
                steps.push(Step::default());
                root_steps.push((root_name, steps.len() - 1));
                steps.len() - 1
            };
            for part in rel {
                let key = part.to_lowercase();
                cur = if let Some(&next) = steps[cur].next.get(&key) {
                    next
                } else {
                    steps.push(Step::default());
                    let next = steps.len() - 1;
                    steps[cur].next.insert(key, next);
                    next
                };
            }
            steps[cur].target = Some(target);
        }
        if steps.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<Option<Vec<(String, bool)>>> = vec![None; dirs.len()];
        // Nodes on the way to a folder: a bit per node id for the cheap "no" (almost every node),
        // their steps in id order for the rest.
        let mut on_bits = vec![0u64; self.nodes.len().div_ceil(64)];
        let mut on: Vec<(u32, usize)> = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if i % (64 * 1024) == 0 && cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let name = self.name(node);
            let step = if node.parent == NO_PARENT {
                root_steps.iter().find(|(n, _)| n == name).map(|&(_, s)| s)
            } else {
                let p = node.parent as usize;
                if on_bits.get(p / 64).is_none_or(|w| w & (1 << (p % 64)) == 0) {
                    continue;
                }
                let Ok(at) = on.binary_search_by_key(&node.parent, |&(id, _)| id) else {
                    continue;
                };
                let parent_step = on[at].1;
                if let Some(target) = steps[parent_step].target
                    && let Some(list) = found[target].as_mut()
                {
                    list.push((name.to_owned(), node.flags & FLAG_DIR != 0));
                }
                steps[parent_step].next.get(&name.to_lowercase()).copied()
            };
            let Some(step) = step else {
                continue;
            };
            if let Some(target) = steps[step].target
                && node.flags & FLAG_DIR != 0
                && node.flags & (FLAG_PRUNED | FLAG_LINK) == 0
            {
                found[target] = Some(Vec::new());
            }
            on_bits[i / 64] |= 1 << (i % 64);
            on.push((u32::try_from(i).unwrap_or(u32::MAX), step));
        }
        dirs.iter()
            .cloned()
            .zip(found)
            .filter_map(|(dir, children)| children.map(|c| (dir, c)))
            .collect()
    }

    /// Sizes of the subfolders of `dir`, summed from the index with `overlay` applied (see the
    /// module docs). Folders inside that the index has no contents for come back in
    /// [`IndexedDirSize::unindexed`]: walk those and add them for the same total as a full walk.
    /// `None` if the index doesn't have `dir`'s contents or `cancel` was set.
    ///
    /// Relies on parents being added before their children, which both build paths do.
    pub fn child_dir_sizes(
        &self,
        dir: &Path,
        overlay: &Overlay,
        cancel: &AtomicBool,
    ) -> Option<Vec<IndexedDirSize>> {
        self.sum_below(dir, overlay, None, cancel)
            .map(|(_, children)| children)
    }

    /// Everything below `dir` counted as [`crate::dirsize::usage`] counts a walk: the files
    /// directly in it and the folder count in the first value, each subfolder's
    /// [`IndexedDirSize::usage`] (categories, and space on disk for clusters of `cluster` bytes)
    /// in the second. Index entries carry no cloud state, so a cloud file that isn't on this
    /// device counts its rounded size on disk. Same `None` and unindexed folders as
    /// [`Self::child_dir_sizes`].
    pub fn usage_below(
        &self,
        dir: &Path,
        overlay: &Overlay,
        cluster: u64,
        cancel: &AtomicBool,
    ) -> Option<(crate::dirsize::Usage, Vec<IndexedDirSize>)> {
        self.sum_below(dir, overlay, Some(cluster), cancel)
    }

    /// One pass for [`Self::child_dir_sizes`] (`detail` `None`: sizes and counts only, as fast as
    /// before) and [`Self::usage_below`] (`detail`: the cluster size; categories and space on
    /// disk too, and the files directly in `dir`).
    fn sum_below(
        &self,
        dir: &Path,
        overlay: &Overlay,
        detail: Option<u64>,
        cancel: &AtomicBool,
    ) -> Option<(crate::dirsize::Usage, Vec<IndexedDirSize>)> {
        const NONE: u32 = u32::MAX;
        let mut direct = crate::dirsize::Usage::default();
        if overlay.is_removed(dir) {
            return None;
        }
        let target = self.resolve(dir, cancel)?;
        let trie = OverlayTrie::new(dir, overlay);
        let first = target as usize + 1;
        // Per node after `target`: which child of `target` it is under (NONE: not under one).
        let mut slots = vec![NONE; self.nodes.len().saturating_sub(first)];
        // Nodes matching an overlay path, by id (`target` itself is the trie root).
        let mut matched: HashMap<u32, usize> = HashMap::new();
        let mut out: Vec<IndexedDirSize> = Vec::new();
        for (i, node) in self.nodes.iter().enumerate().skip(first) {
            if i % (64 * 1024) == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let p = node.parent;
            let (mut slot, trie_parent) = if p == target {
                (NONE, Some(0))
            } else if p != NO_PARENT && p > target {
                let slot = slots[p as usize - first];
                if slot == NONE {
                    continue;
                }
                (slot, matched.get(&p).copied())
            } else {
                continue;
            };
            let id = u32::try_from(i).ok()?;
            let mut replaced = false;
            if let Some(t) = trie_parent
                && let Some(c) = trie.child(t, self.name(node))
            {
                if trie.nodes[c].removed {
                    // Deleted since the build: nothing below counts.
                    continue;
                }
                replaced = trie.nodes[c].added;
                matched.insert(id, c);
            }
            let flags = node.flags;
            let is_dir = flags & FLAG_DIR != 0;
            if flags & FLAG_LINK != 0 {
                continue;
            }
            if p == target {
                if !is_dir {
                    if let Some(cluster) = detail
                        && !replaced
                    {
                        count_indexed(&mut direct, self.name(node), self.sizes[i], cluster);
                    }
                    continue;
                }
                direct.folders += u64::from(detail.is_some());
                slot = u32::try_from(out.len()).ok()?;
                out.push(IndexedDirSize {
                    name: self.name(node).to_string(),
                    ..IndexedDirSize::default()
                });
            }
            let child = &mut out[slot as usize];
            if is_dir {
                if p != target {
                    child.usage.folders += u64::from(detail.is_some());
                }
                if flags & FLAG_PRUNED != 0 {
                    child.unindexed.push(self.path_of(id));
                } else {
                    slots[i - first] = slot;
                }
            } else if !replaced {
                child.size.bytes += self.sizes[i];
                child.size.files += 1;
                if let Some(cluster) = detail {
                    count_indexed(&mut child.usage, self.name(node), self.sizes[i], cluster);
                }
            }
        }
        overlay.add_files_under(dir, detail, &mut direct, &mut out);
        Some((direct, out))
    }

    /// A new index with `overlay` folded in, without walking or reading the MFT again: removed
    /// paths are dropped with everything below them, changed ones get their new size and time,
    /// and added ones are appended under their folder (parents stay before children). Paths
    /// whose folder isn't in the index come back in [`Merged::unplaced`] (keep them in the
    /// overlay). `None` if cancelled.
    ///
    /// The result keeps the walk time plus one second: it stays as stale as the walk for volumes
    /// that miss changes while Arke is closed, yet sorts after the index it came from, so the
    /// newer file is loaded and old checkpoints don't apply to it.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass over the index, then one over the adds"
    )]
    pub fn merged(
        &self,
        overlay: &Overlay,
        rules: &BuildRules,
        cancel: &AtomicBool,
    ) -> Option<Merged> {
        let tries: Vec<MergeTrie> = self
            .roots
            .iter()
            .map(|r| MergeTrie::new(r, overlay))
            .collect();
        let added = overlay.added.len();
        let mut out = Self {
            roots: self.roots.clone(),
            built_at: self.built_at + 1,
            cursors: self.cursors.clone(),
            skipped_build_dirs: self.skipped_build_dirs,
            excluded_hash: self.excluded_hash,
            names: Names::Owned(String::with_capacity(self.names.len() + added * 16)),
            ..Self::default()
        };
        out.names.push_str(&self.names);
        out.nodes.owned().reserve(self.nodes.len() + added);
        out.sizes.owned().reserve(self.nodes.len() + added);
        out.mtimes.owned().reserve(self.nodes.len() + added);
        out.masks.owned().reserve(self.nodes.len() + added);
        // Old id -> new id (NO_PARENT: dropped).
        let mut new_id = vec![NO_PARENT; self.nodes.len()];
        // Old ids on an overlay path -> (root, trie node).
        let mut on_trie: HashMap<u32, (usize, usize)> = HashMap::new();
        // Per root and trie node: the new id it got.
        let mut placed: Vec<Vec<u32>> = tries
            .iter()
            .map(|t| vec![NO_PARENT; t.nodes.len()])
            .collect();
        // Kept items whose contents go (a folder replaced by a file).
        let mut barren: HashSet<u32> = HashSet::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if i % (64 * 1024) == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let id = u32::try_from(i).ok()?;
            let name = self.name(node);
            let (parent, here) = if node.parent == NO_PARENT {
                let root = self
                    .roots
                    .iter()
                    .position(|r| *r.to_string_lossy() == *name);
                (NO_PARENT, root.map(|r| (r, 0)))
            } else {
                let parent = new_id[node.parent as usize];
                if parent == NO_PARENT || (!barren.is_empty() && barren.contains(&node.parent)) {
                    continue;
                }
                let here = on_trie
                    .get(&node.parent)
                    .and_then(|&(r, t)| tries[r].child(t, name).map(|c| (r, c)));
                (parent, here)
            };
            let (mut flags, mut size, mut mtime) = (node.flags, self.sizes[i], self.mtimes[i]);
            let mut name_start = node.name_start;
            let mut name_len = node.name_len;
            let mut mask = self.masks[i];
            let mut drop_children = false;
            if let Some((r, c)) = here.filter(|&(_, c)| c != 0) {
                let t = &tries[r].nodes[c];
                match &t.seen {
                    None if t.removed => continue,
                    None => {}
                    Some(seen) => {
                        let was_dir = flags & FLAG_DIR != 0;
                        flags = seen_flags(seen, flags);
                        size = seen.size;
                        mtime = u32::try_from(seen.modified).unwrap_or(u32::MAX);
                        drop_children = was_dir && !seen.is_dir;
                        if t.name != name {
                            // Renamed in letter case only.
                            name_start = u32::try_from(out.names.len()).ok()?;
                            name_len = u16::try_from(t.name.len()).ok()?;
                            out.names.push_str(&t.name);
                            mask = char_mask(&t.name);
                        }
                    }
                }
            }
            let new = u32::try_from(out.nodes.len()).ok()?;
            out.nodes.push(Node {
                parent,
                name_start,
                name_len,
                flags,
            });
            out.sizes.push(size);
            out.mtimes.push(mtime);
            out.masks.push(mask);
            new_id[i] = new;
            if drop_children {
                barren.insert(id);
            }
            if let Some((r, c)) = here {
                on_trie.insert(id, (r, c));
                placed[r][c] = new;
            }
        }
        // Added paths not in the index yet, in trie order (a folder before its contents).
        let mut unplaced = Vec::new();
        for (r, trie) in tries.iter().enumerate() {
            for (c, t) in trie.nodes.iter().enumerate().skip(1) {
                if c % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let Some(seen) = &t.seen else { continue };
                if placed[r][c] != NO_PARENT {
                    continue;
                }
                let parent = placed[r][t.parent];
                let parent_ok = parent != NO_PARENT
                    && out.nodes[parent as usize].flags & (FLAG_DIR | FLAG_PRUNED | FLAG_LINK)
                        == FLAG_DIR;
                if !parent_ok {
                    // Under a folder that's gone: gone too. Otherwise its folder isn't indexed
                    // (yet); it stays an overlay record.
                    if !trie.under_removed(c) {
                        unplaced.push(t.path.clone());
                    }
                    continue;
                }
                let mut flags = seen_flags(seen, 0);
                let lower = t.name.to_lowercase();
                if seen.is_dir
                    && (is_skipped_dir(&lower)
                        || (rules.skip_build_dirs && is_named_build_dir(&lower))
                        || rules.excludes(&t.path))
                {
                    flags |= FLAG_PRUNED;
                }
                let mtime = u32::try_from(seen.modified).unwrap_or(u32::MAX);
                let Some(new) = out.push(parent, &t.name, flags, seen.size, mtime) else {
                    unplaced.push(t.path.clone());
                    continue;
                };
                placed[r][c] = new;
            }
        }
        Some(Merged {
            index: out,
            unplaced,
        })
    }

    /// Convenience query (no narrowing).
    pub fn find(
        &self,
        query: &Query,
        limit: usize,
        show_hidden: bool,
        threads: usize,
    ) -> Vec<IndexHit> {
        self.search(&Search {
            query,
            limit,
            show_hidden,
            threads,
            within: None,
            collect_matched: false,
            cancel: None,
        })
        .hits
    }

    /// Query: apply filters, fuzzy-match names, return the top `limit` by score. With `\` in the
    /// name, the last segment matches the name and the rest the containing folder. Filters only,
    /// no name: newest first.
    pub fn search(&self, s: &Search) -> SearchResult {
        let empty = SearchResult {
            hits: Vec::new(),
            matched: None,
            cancelled: false,
        };
        let query = s.query;
        if query.is_empty() || self.nodes.is_empty() {
            return empty;
        }
        let (dir_part, name_part) = split_query(query.text.trim());
        if name_part.is_empty() && !query.has_filters() {
            return empty;
        }
        let pattern = (!name_part.is_empty()).then(|| name_pattern(name_part));
        // With a folder condition, keep more candidates and filter by path afterwards.
        let keep = if dir_part.is_some() {
            s.limit * 20
        } else {
            s.limit
        };
        let total = s.within.map_or(self.nodes.len(), <[u32]>::len);
        let threads = s.threads.max(1).min(total.div_ceil(CHUNK).max(1));
        let matching = Matching {
            pattern: pattern.as_ref(),
            needle: (!name_part.is_empty()).then(|| Needle::new(name_part)),
            keep,
        };
        // Small chunks: faster (performance) cores naturally do more, nobody waits on the slowest
        // thread.
        let next = AtomicUsize::new(0);
        let cancelled = || s.cancel.is_some_and(|c| c.load(Ordering::Relaxed));
        let worker = || {
            WORK.with_borrow_mut(|slot| {
                let work = slot.get_or_insert_with(Work::new);
                work.top.clear();
                let mut out = Chunk::default();
                loop {
                    let start = next.fetch_add(CHUNK, Ordering::Relaxed);
                    if start >= total || cancelled() {
                        break;
                    }
                    let range = start..(start + CHUNK).min(total);
                    self.scan(s, range, &matching, work, &mut out);
                }
                out.top = work.top.drain().map(|Reverse(x)| x).collect();
                out
            })
        };
        let results: parking_lot::Mutex<Vec<Chunk>> = parking_lot::Mutex::new(Vec::new());
        match scan_pool() {
            Some(pool) => pool.scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        let chunk = worker();
                        results.lock().push(chunk);
                    });
                }
            }),
            None => results.lock().push(worker()),
        }
        let results = results.into_inner();
        if cancelled() {
            return SearchResult {
                cancelled: true,
                ..empty
            };
        }
        let mut candidates = Vec::new();
        let mut matched = s.collect_matched.then(Vec::new);
        for chunk in results {
            candidates.extend(chunk.top);
            if let Some(m) = matched.as_mut() {
                m.extend(chunk.ids);
            }
        }
        if let Some(m) = matched.as_mut() {
            // Threads finish chunks in any order; sort back to ascending.
            m.sort_unstable();
        }
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let hits = self.finish(candidates, pattern.as_ref(), dir_part, s.limit);
        SearchResult {
            hits,
            matched,
            cancelled: false,
        }
    }

    /// Scans one chunk: `range` is a span of ids (positions in `within` when set).
    fn scan(
        &self,
        s: &Search,
        range: std::ops::Range<usize>,
        m: &Matching,
        work: &mut Work,
        out: &mut Chunk,
    ) {
        let query = s.query;
        for k in range {
            let id = match s.within {
                Some(within) => within[k],
                None => match u32::try_from(k) {
                    Ok(id) => id,
                    Err(_) => break,
                },
            };
            let Some(node) = self.nodes.get(id as usize) else {
                continue;
            };
            if !s.show_hidden && node.flags & FLAG_HIDDEN != 0 {
                continue;
            }
            // Char mask: a name missing a letter the query needs can't match, no need to read it.
            if let Some(needle) = m.needle
                && !needle.may_match(self.masks[id as usize])
            {
                continue;
            }
            let name = self.name(node);
            let mtime = self.mtimes[id as usize];
            let is_dir = node.flags & FLAG_DIR != 0;
            if query.has_filters()
                && !query.accepts(name, is_dir, self.sizes[id as usize], i64::from(mtime))
            {
                continue;
            }
            let score = match m.pattern {
                Some(pattern) => {
                    let Some(score) =
                        pattern.score(Utf32Str::new(name, &mut work.buf), &mut work.matcher)
                    else {
                        continue;
                    };
                    score
                }
                // Filters only: newest first.
                None => mtime,
            };
            if s.collect_matched {
                out.ids.push(id);
            }
            let rank = Rank::new(score, node.name_len, id);
            // Heap full and worse than the last: leave the heap alone (most matches stop here).
            if work.top.len() >= m.keep {
                match work.top.peek() {
                    Some(Reverse(last)) if rank > *last => {
                        work.top.pop();
                    }
                    _ => continue,
                }
            }
            work.top.push(Reverse(rank));
        }
    }

    /// Builds paths for the selected candidates, applies the folder condition, computes highlights.
    fn finish(
        &self,
        candidates: Vec<Rank>,
        pattern: Option<&Pattern>,
        dir_part: Option<&str>,
        limit: usize,
    ) -> Vec<IndexHit> {
        let dir_pattern =
            dir_part.map(|d| Pattern::parse(d, CaseMatching::Smart, Normalization::Smart));
        let mut matcher = Matcher::new(nucleo::Config::DEFAULT.match_paths());
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let mut hits = Vec::new();
        // Check each folder once (candidates often cluster in one folder).
        let mut dir_ok: HashMap<u32, bool> = HashMap::new();
        for rank in candidates {
            let (score, id) = (rank.0, rank.id());
            if hits.len() >= limit {
                break;
            }
            let node = &self.nodes[id as usize];
            if let Some(dp) = &dir_pattern {
                let parent = node.parent;
                let ok = *dir_ok.entry(parent).or_insert_with(|| {
                    parent != NO_PARENT && {
                        let dir = self.path_of(parent).to_string_lossy().into_owned();
                        dp.score(Utf32Str::new(&dir, &mut buf), &mut matcher)
                            .is_some()
                    }
                });
                if !ok {
                    continue;
                }
            }
            let path = self.path_of(id);
            let name = self.name(node);
            indices.clear();
            if let Some(pattern) = pattern {
                pattern.indices(Utf32Str::new(name, &mut buf), &mut matcher, &mut indices);
            }
            hits.push(IndexHit {
                highlights: crate::filter::char_ranges_to_bytes(name, &mut indices),
                is_dir: node.flags & FLAG_DIR != 0,
                size: self.sizes[id as usize],
                modified: to_system_time(self.mtimes[id as usize]),
                path,
                score,
            });
        }
        hits
    }

    // ───────────────────────── Persistence ─────────────────────────

    /// Writes to `w`. Format (little-endian): header (version, build time, roots, journal
    /// positions, name length, item count), then each column contiguous and 8-aligned: names, nodes
    /// (12 bytes, same as in-memory `Node`), sizes, mtimes, char masks. That lets [`Index::open`]
    /// map the file without copying.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut w = Counting { w, pos: 0 };
        w.put(MAGIC)?;
        w.put(&self.built_at.to_le_bytes())?;
        // Options: lowest bit is skip-build-folders, the rest is the exclusion fingerprint.
        w.put(&(u32::from(self.skipped_build_dirs) | (self.excluded_hash << 1)).to_le_bytes())?;
        w.put(
            &u32::try_from(self.roots.len())
                .map_err(io::Error::other)?
                .to_le_bytes(),
        )?;
        for root in &self.roots {
            let s = root.to_string_lossy();
            w.put(
                &u32::try_from(s.len())
                    .map_err(io::Error::other)?
                    .to_le_bytes(),
            )?;
            w.put(s.as_bytes())?;
        }
        for i in 0..self.roots.len() {
            match self.cursor(i) {
                Some(c) => {
                    w.put(&[1])?;
                    w.put(&c.journal.to_le_bytes())?;
                    w.put(&c.usn.to_le_bytes())?;
                }
                None => w.put(&[0; 17])?,
            }
        }
        w.put(&(self.names.len() as u64).to_le_bytes())?;
        w.put(&(self.nodes.len() as u64).to_le_bytes())?;
        w.align()?;
        w.put(self.names.as_bytes())?;
        w.align()?;
        // Write in pieces instead of allocating hundreds of MB at once.
        let mut buf = Vec::with_capacity(64 * 1024 * 12);
        let flush = |buf: &mut Vec<u8>, w: &mut Counting<_>, force: bool| -> io::Result<()> {
            if force || buf.len() >= 64 * 1024 * 12 {
                w.put(buf)?;
                buf.clear();
            }
            Ok(())
        };
        for n in self.nodes.iter() {
            buf.extend_from_slice(&n.parent.to_le_bytes());
            buf.extend_from_slice(&n.name_start.to_le_bytes());
            buf.extend_from_slice(&n.name_len.to_le_bytes());
            buf.push(n.flags);
            buf.push(0);
            flush(&mut buf, &mut w, false)?;
        }
        flush(&mut buf, &mut w, true)?;
        w.align()?;
        for v in self.sizes.iter() {
            buf.extend_from_slice(&v.to_le_bytes());
            flush(&mut buf, &mut w, false)?;
        }
        flush(&mut buf, &mut w, true)?;
        w.align()?;
        for v in self.mtimes.iter() {
            buf.extend_from_slice(&v.to_le_bytes());
            flush(&mut buf, &mut w, false)?;
        }
        flush(&mut buf, &mut w, true)?;
        w.align()?;
        for v in self.masks.iter() {
            buf.extend_from_slice(&v.to_le_bytes());
            flush(&mut buf, &mut w, false)?;
        }
        flush(&mut buf, &mut w, true)
    }

    /// Reads from `r` into memory (a copy; used by tests and when mapping fails).
    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes)?;
        let layout = Layout::parse(&bytes)?;
        let le32 = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let names = std::str::from_utf8(&bytes[layout.names..layout.names + layout.names_len])
            .map_err(|_not_utf8| bad_format())?
            .to_string();
        let count = layout.count;
        let mut nodes = Vec::with_capacity(count);
        let mut sizes = Vec::with_capacity(count);
        let mut mtimes = Vec::with_capacity(count);
        let mut masks = Vec::with_capacity(count);
        for i in 0..count {
            let at = layout.nodes + i * 12;
            nodes.push(Node {
                parent: le32(at),
                name_start: le32(at + 4),
                name_len: u16::from_le_bytes([bytes[at + 8], bytes[at + 9]]),
                flags: bytes[at + 10],
            });
            let at = layout.sizes + i * 8;
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[at..at + 8]);
            sizes.push(u64::from_le_bytes(b));
            mtimes.push(le32(layout.mtimes + i * 4));
            masks.push(le32(layout.masks + i * 4));
        }
        validate(&nodes, &names)?;
        Ok(Self {
            roots: layout.roots,
            nodes: Column::Owned(nodes),
            sizes: Column::Owned(sizes),
            mtimes: Column::Owned(mtimes),
            masks: Column::Owned(masks),
            names: Names::Owned(names),
            built_at: layout.built_at,
            cursors: layout.cursors,
            skipped_build_dirs: layout.skipped_build_dirs,
            excluded_hash: layout.excluded_hash,
            dedup: None,
        })
    }

    /// Maps the saved file read-only instead of copying it. Under memory pressure the OS can drop
    /// these pages and reread them from the file, with nothing going to the page file. The whole
    /// file is validated on open (ranges, encoding), so later reads stay in checked bounds.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: Read-only mapping. Only Arke writes this file, and a new index is always written
        // to the other file (see index_service), so the mapped one never changes underneath us.
        let map = Arc::new(unsafe { memmap2::Mmap::map(&file)? });
        let layout = Layout::parse(&map)?;
        // The mapping starts on a page boundary and columns are 8-aligned, so typed slices are
        // aligned.
        let names = Names::Mapped {
            map: Arc::clone(&map),
            offset: layout.names,
            len: layout.names_len,
        };
        std::str::from_utf8(&map[layout.names..layout.names + layout.names_len])
            .map_err(|_not_utf8| bad_format())?;
        let column = |offset: usize| (Arc::clone(&map), offset, layout.count);
        let (m, o, n) = column(layout.nodes);
        let nodes: Column<Node> = Column::Mapped {
            map: m,
            offset: o,
            len: n,
        };
        let (m, o, n) = column(layout.sizes);
        let sizes: Column<u64> = Column::Mapped {
            map: m,
            offset: o,
            len: n,
        };
        let (m, o, n) = column(layout.mtimes);
        let mtimes: Column<u32> = Column::Mapped {
            map: m,
            offset: o,
            len: n,
        };
        let (m, o, n) = column(layout.masks);
        let masks: Column<u32> = Column::Mapped {
            map: m,
            offset: o,
            len: n,
        };
        validate(&nodes, &names)?;
        Ok(Self {
            roots: layout.roots,
            nodes,
            sizes,
            mtimes,
            masks,
            names,
            built_at: layout.built_at,
            cursors: layout.cursors,
            skipped_build_dirs: layout.skipped_build_dirs,
            excluded_hash: layout.excluded_hash,
            dedup: None,
        })
    }
}

fn bad_format() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "index format")
}

/// Checks every node's parent and name range are in bounds (a corrupt file can't make queries go
/// out of range).
fn validate(nodes: &[Node], names: &str) -> io::Result<()> {
    let count = nodes.len();
    for node in nodes {
        let start = node.name_start as usize;
        let end = start + node.name_len as usize;
        let parent_ok = node.parent == NO_PARENT || (node.parent as usize) < count;
        let name_ok =
            end <= names.len() && names.is_char_boundary(start) && names.is_char_boundary(end);
        if !name_ok || !parent_ok {
            return Err(bad_format());
        }
    }
    Ok(())
}

/// Counts bytes written (for alignment).
struct Counting<'a, W: Write> {
    w: &'a mut W,
    pos: usize,
}

impl<W: Write> Counting<'_, W> {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.w.write_all(bytes)?;
        self.pos += bytes.len();
        Ok(())
    }

    /// Pads with zeros to a multiple of 8.
    fn align(&mut self) -> io::Result<()> {
        let pad = (8 - self.pos % 8) % 8;
        self.put(&[0; 8][..pad])
    }
}

/// Header and column offsets of a saved file (all checked to be within the file).
struct Layout {
    roots: Vec<PathBuf>,
    cursors: Vec<Option<Cursor>>,
    built_at: u64,
    skipped_build_dirs: bool,
    excluded_hash: u32,
    count: usize,
    names: usize,
    names_len: usize,
    nodes: usize,
    sizes: usize,
    mtimes: usize,
    masks: usize,
}

impl Layout {
    fn parse(b: &[u8]) -> io::Result<Self> {
        let mut pos = 0usize;
        let mut take = |len: usize| -> io::Result<&[u8]> {
            let end = pos
                .checked_add(len)
                .filter(|&e| e <= b.len())
                .ok_or_else(bad_format)?;
            let out = &b[pos..end];
            pos = end;
            Ok(out)
        };
        if take(8)? != MAGIC {
            return Err(bad_format());
        }
        let u64_at = |s: &[u8]| {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_le_bytes(a)
        };
        let u32_at = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize;
        let built_at = u64_at(take(8)?);
        let options = u32_at(take(4)?);
        let root_count = u32_at(take(4)?);
        let mut roots = Vec::new();
        for _ in 0..root_count {
            let len = u32_at(take(4)?);
            let s = std::str::from_utf8(take(len)?).map_err(|_not_utf8| bad_format())?;
            roots.push(PathBuf::from(s));
        }
        let mut cursors = Vec::with_capacity(roots.len());
        for _ in 0..roots.len() {
            let c = take(17)?;
            cursors.push((c[0] == 1).then(|| Cursor {
                journal: u64_at(&c[1..9]),
                usn: u64_at(&c[9..17]).cast_signed(),
            }));
        }
        let names_len = usize::try_from(u64_at(take(8)?)).map_err(|_too_big| bad_format())?;
        let count = usize::try_from(u64_at(take(8)?)).map_err(|_too_big| bad_format())?;
        let align = |pos: &mut usize| *pos = pos.div_ceil(8) * 8;
        let mut sections = [0usize; 5];
        let sizes_of = [
            Some(names_len),
            count.checked_mul(12),
            count.checked_mul(8),
            count.checked_mul(4),
            count.checked_mul(4),
        ];
        let mut cur = pos;
        for (slot, len) in sections.iter_mut().zip(sizes_of) {
            align(&mut cur);
            *slot = cur;
            cur = cur
                .checked_add(len.ok_or_else(bad_format)?)
                .filter(|&e| e <= b.len())
                .ok_or_else(bad_format)?;
        }
        Ok(Self {
            roots,
            cursors,
            built_at,
            skipped_build_dirs: options & 1 != 0,
            excluded_hash: u32::try_from(options >> 1).unwrap_or(0),
            count,
            names: sections[0],
            names_len,
            nodes: sections[1],
            sizes: sections[2],
            mtimes: sections[3],
            masks: sections[4],
        })
    }
}

/// How much a full column (or name pool) of `capacity` grows to fit `needed` more: half again,
/// not the standard doubling. A build of millions of items would otherwise end up to twice the
/// size it needs (a third more on average), on top of the old allocation while it's copied.
fn grow_by(capacity: usize, needed: usize) -> usize {
    (capacity / 2).max(needed).max(4096)
}

/// One index column: on the heap when built, mapped from the file when loaded ([`Index::open`]).
enum Column<T: Copy + 'static> {
    Owned(Vec<T>),
    /// A span of the saved file. `offset` is a multiple of 8 and the mapping starts on a page
    /// boundary, so it is aligned.
    Mapped {
        map: Arc<memmap2::Mmap>,
        offset: usize,
        len: usize,
    },
}

impl<T: Copy> Default for Column<T> {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl<T: Copy> std::fmt::Debug for Column<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Column({})", self.len())
    }
}

impl<T: Copy> std::ops::Deref for Column<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Self::Owned(v) => v,
            // SAFETY: Layout::parse checked that offset + len * size_of::<T>() is within the file
            // and offset is 8-aligned. T is only u32, u64 or Node (repr(C) integers), valid for any
            // bytes. The mapping lives as long as the Arc.
            Self::Mapped { map, offset, len } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().add(*offset).cast::<T>(), *len)
            },
        }
    }
}

impl<T: Copy> Column<T> {
    fn owned(&mut self) -> &mut Vec<T> {
        if let Self::Mapped { .. } = self {
            *self = Self::Owned(self.to_vec());
        }
        match self {
            Self::Owned(v) => v,
            Self::Mapped { .. } => unreachable!("replaced above"),
        }
    }

    fn push(&mut self, value: T) {
        let v = self.owned();
        if v.len() == v.capacity() {
            v.reserve_exact(grow_by(v.capacity(), 1));
        }
        v.push(value);
    }

    fn shrink_to_fit(&mut self) {
        if let Self::Owned(v) = self {
            v.shrink_to_fit();
        }
    }

    /// Elements allocated on the heap (mapped data not counted).
    const fn heap_capacity(&self) -> usize {
        match self {
            Self::Owned(v) => v.capacity(),
            Self::Mapped { .. } => 0,
        }
    }
}

/// All names concatenated; owned or mapped, like [`Column`].
enum Names {
    Owned(String),
    Mapped {
        map: Arc<memmap2::Mmap>,
        offset: usize,
        len: usize,
    },
}

impl Default for Names {
    fn default() -> Self {
        Self::Owned(String::new())
    }
}

impl std::fmt::Debug for Names {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Names({} bytes)", self.len())
    }
}

impl std::ops::Deref for Names {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Self::Owned(s) => s,
            // SAFETY: Index::open checked this span is within the file and valid UTF-8; the mapping
            // lives as long as the Arc.
            Self::Mapped { map, offset, len } => unsafe {
                std::str::from_utf8_unchecked(&map[*offset..*offset + *len])
            },
        }
    }
}

impl Names {
    fn push_str(&mut self, s: &str) {
        if let Self::Mapped { .. } = self {
            *self = Self::Owned(self.to_string());
        }
        if let Self::Owned(owned) = self {
            if owned.capacity() - owned.len() < s.len() {
                owned.reserve_exact(grow_by(owned.capacity(), s.len()));
            }
            owned.push_str(s);
        }
    }

    fn reserve_exact(&mut self, additional: usize) {
        if let Self::Owned(owned) = self {
            owned.reserve_exact(additional);
        }
    }

    fn shrink_to_fit(&mut self) {
        if let Self::Owned(s) = self {
            s.shrink_to_fit();
        }
    }

    /// Shrinks to `len` bytes (build time only; `len` is always on a name boundary).
    fn truncate(&mut self, len: usize) {
        if let Self::Owned(s) = self {
            s.truncate(len);
        }
    }

    const fn heap_capacity(&self) -> usize {
        match self {
            Self::Owned(s) => s.capacity(),
            Self::Mapped { .. } => 0,
        }
    }
}

/// Matching state shared by a query.
struct Matching<'a> {
    pattern: Option<&'a Pattern>,
    needle: Option<Needle>,
    /// Results kept per thread.
    keep: usize,
}

/// Ranking: higher score first; ties prefer shorter names (more likely the one wanted), then
/// walk order (shallower folders). Independent of how threads split work, so results are stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank(u32, u16, u32);

impl Rank {
    const fn new(score: u32, name_len: u16, id: u32) -> Self {
        Self(score, u16::MAX - name_len, u32::MAX - id)
    }

    const fn id(self) -> u32 {
        u32::MAX - self.2
    }
}

/// Per-thread scratch.
struct Work {
    matcher: Matcher,
    buf: Vec<char>,
    top: BinaryHeap<Reverse<Rank>>,
}

/// Scratch spaces created so far (tests check they are reused across queries).
#[cfg(test)]
static WORK_CREATED: AtomicUsize = AtomicUsize::new(0);

impl Work {
    fn new() -> Self {
        #[cfg(test)]
        WORK_CREATED.fetch_add(1, Ordering::Relaxed);
        Self {
            matcher: Matcher::new(nucleo::Config::DEFAULT.match_paths()),
            buf: Vec::new(),
            top: BinaryHeap::new(),
        }
    }
}

/// One thread's result: top (score, id) pairs and all matching ids.
#[derive(Default)]
struct Chunk {
    top: Vec<Rank>,
    ids: Vec<u32>,
}

const MASK_DIGIT: u32 = 1 << 26;
const MASK_NON_ASCII: u32 = 1 << 27;

const fn char_bit(b: u8) -> u32 {
    match b.to_ascii_lowercase() {
        c @ b'a'..=b'z' => 1 << (c - b'a'),
        b'0'..=b'9' => MASK_DIGIT,
        b'.' => 1 << 28,
        b'_' => 1 << 29,
        b'-' => 1 << 30,
        b' ' => 1 << 31,
        0x80..=0xFF => MASK_NON_ASCII,
        _ => 0,
    }
}

/// Char mask of a name: one bit each for a-z (case-insensitive), digits, `.` `_` `-` space, and
/// non-ASCII.
fn char_mask(name: &str) -> u32 {
    name.bytes().fold(0, |m, b| m | char_bit(b))
}

/// Chars the query requires. May let extra names through, never rejects a real match:
/// - Names with non-ASCII chars always pass (nucleo folds é to e, so a letter may come from there).
/// - Non-ASCII query chars with case (Greek, Cyrillic...) aren't checked; caseless ones (CJK) only
///   require the name to have non-ASCII.
/// - `!` exclusion terms aren't checked.
#[derive(Clone, Copy)]
struct Needle {
    need: u32,
    non_ascii: bool,
}

impl Needle {
    fn new(name_part: &str) -> Self {
        let mut need = 0;
        let mut non_ascii = false;
        for atom in name_part.split_whitespace().filter(|a| !a.starts_with('!')) {
            let atom = atom.trim_start_matches(['^', '\'']).trim_end_matches('$');
            for c in atom.chars() {
                if c.is_ascii() {
                    need |= char_bit(c as u8);
                } else if !c.is_lowercase() && !c.is_uppercase() {
                    non_ascii = true;
                }
            }
        }
        Self { need, non_ascii }
    }

    const fn may_match(self, mask: u32) -> bool {
        if mask & MASK_NON_ASCII != 0 {
            return true;
        }
        !self.non_ascii && mask & self.need == self.need
    }
}

/// Name pattern. Alphanumeric terms of 1-2 chars match as substrings (like Everything): fuzzy
/// matching that short hits almost every name (`ca` matches anything with c then a, over a
/// million names on C:), which is useless and slow. Fuzzy only from 3 chars.
fn name_pattern(name_part: &str) -> Pattern {
    let text: Vec<String> = name_part
        .split_whitespace()
        .map(|atom| {
            let plain = !atom.starts_with(['!', '^', '\'']) && !atom.ends_with('$');
            // ASCII only: two CJK chars are already specific, and the char mask already filtered
            // out pure-ASCII names.
            if plain && atom.is_ascii() && atom.len() <= 2 {
                format!("'{atom}")
            } else {
                atom.to_string()
            }
        })
        .collect();
    Pattern::parse(&text.join(" "), CaseMatching::Smart, Normalization::Smart)
}

/// `docs\report` -> (Some("docs"), "report"); without a separator the whole string is the name.
pub(crate) fn split_query(query: &str) -> (Option<&str>, &str) {
    match query.rfind(['\\', '/']) {
        Some(at) => {
            let dir = query[..at].trim();
            ((!dir.is_empty()).then_some(dir), query[at + 1..].trim())
        }
        None => (None, query),
    }
}

/// A watched change after the background stat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// Folder (or link to a folder).
    pub is_dir: bool,
    /// Has the Hidden attribute.
    pub hidden: bool,
    /// Size in bytes (0 for folders).
    pub size: u64,
    /// Unix seconds, 0 if unknown.
    pub modified: u64,
}

impl Seen {
    /// Captures the fields the index needs from a stat result.
    pub fn of(entry: &Entry) -> Self {
        let is_dir = entry.is_dir_like();
        Self {
            is_dir,
            hidden: entry.is_hidden(),
            size: if is_dir { 0 } else { entry.size },
            modified: entry.modified.map_or(0, unix_secs),
        }
    }
}

/// Changes since the index was built: added and removed paths. Merged into queries; a rebuild
/// happens when it grows too large.
///
/// Records carry a sequence number; once a rebuild finishes, records older than its start are
/// already in the new index and can be dropped.
#[derive(Clone, Default, Debug)]
pub struct Overlay {
    /// Paths that exist (added or modified).
    added: HashMap<PathBuf, (Seen, u64)>,
    /// Paths that are gone (a removed folder removes everything under it).
    removed: HashMap<PathBuf, u64>,
    seq: u64,
}

impl Overlay {
    /// Number of recorded paths (added plus removed).
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len()
    }

    /// No changes recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current sequence number (taken when a rebuild starts, then passed to
    /// [`Overlay::prune_before`]).
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Reports the result of a background stat: still there (`Some`) or gone (`None`).
    pub fn record(&mut self, path: PathBuf, seen: Option<Seen>) {
        self.seq += 1;
        if let Some(seen) = seen {
            self.removed.remove(&path);
            self.added.insert(path, (seen, self.seq));
        } else {
            self.added.remove(&path);
            self.removed.insert(path, self.seq);
        }
    }

    /// Folders changes were recorded in, and recorded folders themselves: an incremental rewalk
    /// lists these again whatever their times say (the times came from separate looks at the
    /// server and may not match a listing's).
    pub fn folders_touched(&self) -> Vec<PathBuf> {
        let mut out: HashSet<&Path> = HashSet::new();
        for (path, (seen, _)) in &self.added {
            out.extend(path.parent());
            if seen.is_dir {
                out.insert(path);
            }
        }
        for path in self.removed.keys() {
            out.extend(path.parent());
        }
        out.into_iter().map(Path::to_path_buf).collect()
    }

    /// Paths recorded as added directly inside `dir`.
    #[must_use]
    pub fn added_in(&self, dir: &Path) -> Vec<PathBuf> {
        self.added
            .keys()
            .filter(|p| p.parent() == Some(dir))
            .cloned()
            .collect()
    }

    /// Drops records before `seq` (the new index includes them).
    pub fn prune_before(&mut self, seq: u64) {
        self.added.retain(|_, (_, s)| *s > seq);
        self.removed.retain(|_, s| *s > seq);
        self.shrink();
    }

    /// Paths recorded as gone.
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }

    /// Removal records that change nothing if the index doesn't have their path: all but those
    /// of folders that hold an added path (a folder made after the index and removed again
    /// still hides what was recorded inside it).
    pub fn removed_hiding_nothing_added(&self) -> Vec<PathBuf> {
        let mut holding: HashSet<String> = HashSet::new();
        for path in self.added.keys() {
            let mut up = path.parent();
            while let Some(dir) = up {
                if !holding.insert(dir.to_string_lossy().to_lowercase()) {
                    break;
                }
                up = dir.parent();
            }
        }
        self.removed
            .keys()
            .filter(|p| !holding.contains(&p.to_string_lossy().to_lowercase()))
            .cloned()
            .collect()
    }

    /// Drops the removal records of `paths`, which the index doesn't have.
    pub fn forget_removed(&mut self, paths: &[PathBuf]) {
        for path in paths {
            self.removed.remove(path);
        }
        self.removed.shrink_to_fit();
    }

    /// Gives back the room of pruned records: up to 100,000 of them pile up between merges,
    /// and a map keeps its size when emptied.
    fn shrink(&mut self) {
        self.added.shrink_to_fit();
        self.removed.shrink_to_fit();
    }

    /// After [`Index::merged`]: drops records up to `seq` except `keep` (the ones it couldn't
    /// place).
    pub fn prune_merged(&mut self, seq: u64, keep: &[PathBuf]) {
        let keep: HashSet<&PathBuf> = keep.iter().collect();
        self.added.retain(|p, (_, s)| *s > seq || keep.contains(p));
        self.removed.retain(|_, s| *s > seq);
        self.shrink();
    }

    /// Saves to a small file (for journal checkpoints, see index_service): added paths with their
    /// info, and removed paths.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let path_bytes = |p: &Path| p.to_string_lossy().into_owned().into_bytes();
        w.write_all(
            &u32::try_from(self.added.len())
                .map_err(io::Error::other)?
                .to_le_bytes(),
        )?;
        for (path, (seen, _)) in &self.added {
            let bytes = path_bytes(path);
            w.write_all(
                &u32::try_from(bytes.len())
                    .map_err(io::Error::other)?
                    .to_le_bytes(),
            )?;
            w.write_all(&bytes)?;
            w.write_all(&[u8::from(seen.is_dir), u8::from(seen.hidden)])?;
            w.write_all(&seen.size.to_le_bytes())?;
            w.write_all(&seen.modified.to_le_bytes())?;
        }
        w.write_all(
            &u32::try_from(self.removed.len())
                .map_err(io::Error::other)?
                .to_le_bytes(),
        )?;
        for path in self.removed.keys() {
            let bytes = path_bytes(path);
            w.write_all(
                &u32::try_from(bytes.len())
                    .map_err(io::Error::other)?
                    .to_le_bytes(),
            )?;
            w.write_all(&bytes)?;
        }
        Ok(())
    }

    /// Reads what [`Overlay::write_to`] saved (sequence numbers are reassigned).
    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        fn u32_of(r: &mut impl Read) -> io::Result<usize> {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            Ok(u32::from_le_bytes(b) as usize)
        }
        fn u64_of(r: &mut impl Read) -> io::Result<u64> {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            Ok(u64::from_le_bytes(b))
        }
        fn path_of(r: &mut impl Read) -> io::Result<PathBuf> {
            let len = u32_of(r)?;
            if len > 64 * 1024 {
                return Err(bad_format());
            }
            let mut b = vec![0u8; len];
            r.read_exact(&mut b)?;
            Ok(PathBuf::from(
                String::from_utf8(b).map_err(|_not_utf8| bad_format())?,
            ))
        }
        let mut overlay = Self::default();
        for _ in 0..u32_of(r)? {
            let path = path_of(r)?;
            let mut flags = [0u8; 2];
            r.read_exact(&mut flags)?;
            let seen = Seen {
                is_dir: flags[0] != 0,
                hidden: flags[1] != 0,
                size: u64_of(r)?,
                modified: u64_of(r)?,
            };
            overlay.record(path, Some(seen));
        }
        for _ in 0..u32_of(r)? {
            let path = path_of(r)?;
            overlay.record(path, None);
        }
        Ok(overlay)
    }

    fn is_removed(&self, path: &Path) -> bool {
        !self.removed.is_empty() && path.ancestors().any(|p| self.removed.contains_key(p))
    }

    /// The records at or below `dir` (copied, so a long size pass doesn't hold the overlay's
    /// lock).
    #[must_use]
    pub fn under(&self, dir: &Path) -> Self {
        let removed_above = dir
            .ancestors()
            .filter_map(|p| self.removed.get_key_value(p))
            .map(|(p, s)| (p.clone(), *s));
        Self {
            added: self
                .added
                .iter()
                .filter(|(p, _)| rel_components(p, dir).is_some())
                .map(|(p, v)| (p.clone(), *v))
                .collect(),
            removed: self
                .removed
                .iter()
                .filter(|(p, _)| rel_components(p, dir).is_some())
                .map(|(p, s)| (p.clone(), *s))
                .chain(removed_above)
                .collect(),
            seq: self.seq,
        }
    }

    /// Adds files recorded since the build (new or changed) to the subfolders of `dir` they are
    /// in (new subfolders get an entry), and with `detail` those right in `dir` to `direct`.
    /// Files in folders the caller walks anyway are skipped.
    fn add_files_under(
        &self,
        dir: &Path,
        detail: Option<u64>,
        direct: &mut crate::dirsize::Usage,
        out: &mut Vec<IndexedDirSize>,
    ) {
        for (path, (seen, _)) in &self.added {
            if seen.is_dir {
                continue;
            }
            let Some(rel) = rel_components(path, dir).filter(|r| !r.is_empty()) else {
                continue;
            };
            if self.is_removed(path) {
                continue;
            }
            if rel.len() == 1 {
                // Right in `dir`: the index's own copy (if any) was left out as replaced.
                if let Some(cluster) = detail {
                    count_indexed(direct, &rel[0], seen.size, cluster);
                }
                continue;
            }
            let slot = if let Some(slot) = out.iter().position(|c| eq_ignore_case(&c.name, &rel[0]))
            {
                slot
            } else {
                out.push(IndexedDirSize {
                    name: rel[0].clone(),
                    ..IndexedDirSize::default()
                });
                out.len() - 1
            };
            let child = &mut out[slot];
            if child.unindexed.iter().any(|u| path.starts_with(u)) {
                continue;
            }
            child.size.bytes += seen.size;
            child.size.files += 1;
            if let Some(cluster) = detail {
                let name = path.file_name().map_or_default(|n| n.to_string_lossy());
                count_indexed(&mut child.usage, &name, seen.size, cluster);
            }
        }
    }

    /// Merges index results with added paths, dropping removed and duplicate ones. Modified
    /// items get the new size and time.
    pub fn apply(
        &self,
        hits: Vec<IndexHit>,
        query: &Query,
        limit: usize,
        show_hidden: bool,
    ) -> Vec<IndexHit> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut out: Vec<IndexHit> = hits
            .into_iter()
            .filter(|h| {
                !self.is_removed(&h.path)
                    && !self.added.contains_key(&h.path)
                    && seen.insert(h.path.clone())
            })
            .collect();
        let (dir_part, name_part) = split_query(query.text.trim());
        if self.added.is_empty() || (name_part.is_empty() && !query.has_filters()) {
            out.truncate(limit);
            return out;
        }
        let pattern = (!name_part.is_empty()).then(|| name_pattern(name_part));
        let dir_pattern =
            dir_part.map(|d| Pattern::parse(d, CaseMatching::Smart, Normalization::Smart));
        let mut matcher = Matcher::new(nucleo::Config::DEFAULT.match_paths());
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let mut fresh = Vec::new();
        for (path, (info, _)) in &self.added {
            if (!show_hidden && info.hidden) || seen.contains(path) {
                continue;
            }
            let name = path
                .file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned());
            let modified = i64::try_from(info.modified).unwrap_or(i64::MAX);
            if !query.accepts(&name, info.is_dir, info.size, modified) {
                continue;
            }
            indices.clear();
            let score = match &pattern {
                Some(p) => {
                    match p.indices(Utf32Str::new(&name, &mut buf), &mut matcher, &mut indices) {
                        Some(score) => score,
                        None => continue,
                    }
                }
                None => u32::try_from(info.modified).unwrap_or(u32::MAX),
            };
            if let Some(dp) = &dir_pattern {
                let parent = path
                    .parent()
                    .map_or_default(|p| p.to_string_lossy().into_owned());
                if dp
                    .score(Utf32Str::new(&parent, &mut buf), &mut matcher)
                    .is_none()
                {
                    continue;
                }
            }
            // Inside a folder removed since (a folder made after the build, removed with what
            // it held).
            if self.is_removed(path) {
                continue;
            }
            fresh.push((
                name.len(),
                IndexHit {
                    highlights: crate::filter::char_ranges_to_bytes(&name, &mut indices),
                    path: path.clone(),
                    is_dir: info.is_dir,
                    size: info.size,
                    modified: (info.modified != 0)
                        .then(|| UNIX_EPOCH + Duration::from_secs(info.modified)),
                    score,
                },
            ));
        }
        // `added` iterates in no fixed order. Break ties the way the index does (shorter name
        // first), then by path, so equal scores keep one order and the same ones survive the limit.
        fresh.sort_unstable_by(|(a_len, a), (b_len, b)| {
            b.score
                .cmp(&a.score)
                .then(a_len.cmp(b_len))
                .then_with(|| a.path.cmp(&b.path))
        });
        out.extend(fresh.into_iter().map(|(_, hit)| hit));
        // Stable: at equal scores, index hits keep their order ahead of the added ones.
        out.sort_by_key(|h| Reverse(h.score));
        out.truncate(limit);
        out
    }
}

/// Counts a file the index knows of (`name`, `size` bytes) into `usage`, as
/// [`crate::dirsize::count_file`] counts a listed one.
fn count_indexed(usage: &mut crate::dirsize::Usage, name: &str, size: u64, cluster: u64) {
    let ext = name
        .rsplit_once('.')
        .map_or_default(|(_, e)| e.to_ascii_lowercase());
    usage.bytes += size;
    usage.on_disk += crate::dirsize::round_to_clusters(size, cluster);
    usage.files += 1;
    usage.categories[crate::dirsize::Category::of_extension(&ext).index()] += size;
}

/// One subfolder's size from [`Index::child_dir_sizes`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexedDirSize {
    /// Folder name as indexed.
    pub name: String,
    /// Files below it that the index knows of (changes since the build applied).
    pub size: crate::dirsize::DirSize,
    /// The same with categories, space on disk and folders, from [`Index::usage_below`] only.
    pub usage: crate::dirsize::Usage,
    /// Folders below it whose contents aren't indexed; walk them and add.
    pub unindexed: Vec<PathBuf>,
}

/// Letter-case-insensitive name comparison (NTFS names are case-insensitive).
fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || (!a.is_ascii() && a.to_lowercase() == b.to_lowercase())
}

/// Components of `path` below `base` (any letter case), `None` if it isn't below it.
fn rel_components(path: &Path, base: &Path) -> Option<Vec<String>> {
    let mut path = path.components();
    for part in base.components() {
        let other = path.next()?;
        if !eq_ignore_case(
            &part.as_os_str().to_string_lossy(),
            &other.as_os_str().to_string_lossy(),
        ) {
            return None;
        }
    }
    Some(
        path.map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect(),
    )
}

/// Overlay paths below one folder as a tree of lowercase names, to match index nodes while
/// walking the node list.
struct OverlayTrie {
    nodes: Vec<TrieNode>,
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<String, usize>,
    /// Added or modified since the build (the indexed size is stale).
    added: bool,
    /// Removed since the build.
    removed: bool,
}

impl OverlayTrie {
    fn new(dir: &Path, overlay: &Overlay) -> Self {
        let mut trie = Self {
            nodes: vec![TrieNode::default()],
        };
        let added = overlay.added.keys().map(|p| (p, true));
        let removed = overlay.removed.keys().map(|p| (p, false));
        for (path, is_added) in added.chain(removed) {
            let Some(rel) = rel_components(path, dir).filter(|r| !r.is_empty()) else {
                continue;
            };
            let mut cur = 0;
            for part in rel {
                let key = part.to_lowercase();
                cur = if let Some(&next) = trie.nodes[cur].children.get(&key) {
                    next
                } else {
                    let next = trie.nodes.len();
                    trie.nodes.push(TrieNode::default());
                    trie.nodes[cur].children.insert(key, next);
                    next
                };
            }
            let node = &mut trie.nodes[cur];
            if is_added {
                node.added = true;
            } else {
                node.removed = true;
            }
        }
        trie
    }

    fn child(&self, parent: usize, name: &str) -> Option<usize> {
        let children = &self.nodes.get(parent)?.children;
        if children.is_empty() {
            return None;
        }
        children.get(&name.to_lowercase()).copied()
    }
}

/// Output of [`Index::merged`].
pub struct Merged {
    /// The index with the changes folded in.
    pub index: Index,
    /// Recorded paths whose folder isn't in the index; they stay overlay records.
    pub unplaced: Vec<PathBuf>,
}

/// Index flags for a stat result; link and pruned marks carry over from `old`.
const fn seen_flags(seen: &Seen, old: u8) -> u8 {
    let mut flags = old & (FLAG_LINK | FLAG_PRUNED);
    if seen.is_dir {
        flags |= FLAG_DIR;
    }
    if seen.hidden {
        flags |= FLAG_HIDDEN;
    }
    flags
}

/// Overlay paths below one root as a tree, for [`Index::merged`].
struct MergeTrie {
    nodes: Vec<MergeNode>,
}

#[derive(Default)]
struct MergeNode {
    /// Children by lowercase name.
    children: HashMap<String, usize>,
    parent: usize,
    /// Name as recorded (the added path's letter case when there is one).
    name: String,
    /// Full path, for added records.
    path: PathBuf,
    /// Added or changed since the build.
    seen: Option<Seen>,
    /// Removed since the build.
    removed: bool,
}

impl MergeTrie {
    fn new(root: &Path, overlay: &Overlay) -> Self {
        let mut trie = Self {
            nodes: vec![MergeNode::default()],
        };
        let added = overlay.added.iter().map(|(p, (seen, _))| (p, Some(*seen)));
        let removed = overlay.removed.keys().map(|p| (p, None));
        for (path, seen) in added.chain(removed) {
            let Some(rel) = rel_components(path, root).filter(|r| !r.is_empty()) else {
                continue;
            };
            let mut cur = 0;
            for part in rel {
                let key = part.to_lowercase();
                cur = if let Some(&next) = trie.nodes[cur].children.get(&key) {
                    next
                } else {
                    let next = trie.nodes.len();
                    trie.nodes.push(MergeNode {
                        parent: cur,
                        name: part,
                        ..MergeNode::default()
                    });
                    trie.nodes[cur].children.insert(key, next);
                    next
                };
            }
            let node = &mut trie.nodes[cur];
            match seen {
                Some(seen) => {
                    node.seen = Some(seen);
                    node.path.clone_from(path);
                    if let Some(name) = path.file_name() {
                        node.name = name.to_string_lossy().into_owned();
                    }
                }
                None => node.removed = true,
            }
        }
        trie
    }

    fn child(&self, parent: usize, name: &str) -> Option<usize> {
        let children = &self.nodes.get(parent)?.children;
        if children.is_empty() {
            return None;
        }
        children.get(&name.to_lowercase()).copied()
    }

    /// Whether a folder above node `c` was removed (and not added back).
    fn under_removed(&self, mut c: usize) -> bool {
        while c != 0 {
            c = self.nodes[c].parent;
            let node = &self.nodes[c];
            if node.removed && node.seen.is_none() {
                return true;
            }
        }
        false
    }
}

/// Roots for log lines (`C:\, D:\`).
pub fn roots_label(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the walk enters this folder: only plain folders. Not symlinks/junctions (cycles or
/// duplicates), not cloud placeholder folders (listing recalls from the cloud), not
/// hidden+system folders or the recycle bin.
pub fn descends_into(entry: &Entry) -> bool {
    entry.kind == EntryKind::Dir
        && !is_skipped_dir(&entry.sort_key)
        && !entry.is_cloud_placeholder()
        && (!entry.is_hidden() || !entry.is_system())
}

/// Whether a watched path is recorded: inside the indexed roots and not in a skipped system
/// folder (e.g. a file moved to the recycle bin).
pub fn is_indexable(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|r| path.starts_with(r))
        && !path.components().any(|c| {
            let c = c.as_os_str().to_string_lossy().to_lowercase();
            is_skipped_dir(&c)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeFs;
    use crate::index_query::Clock;
    use crate::journal::Journal;

    fn q(text: &str) -> Query {
        Query::parse(text, Clock::default())
    }

    /// Test helper: parse the query, then search.
    fn find(index: &Index, text: &str, limit: usize, show_hidden: bool) -> Vec<IndexHit> {
        index.find(&q(text), limit, show_hidden, 3)
    }

    fn dir(name: &str) -> Entry {
        Entry::new(name, EntryKind::Dir, 0, None, 0)
    }

    fn file(name: &str) -> Entry {
        Entry::new(name, EntryKind::File, 1, None, 0)
    }

    fn sample() -> Index {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\",
            vec![dir("Users"), dir("$Recycle.Bin"), file("pagefile.sys")],
        );
        fs.insert_dir(r"C:\Users", vec![dir("me")]);
        fs.insert_dir(
            r"C:\Users\me",
            vec![
                dir("Documents"),
                dir("Pictures"),
                file("notes.txt"),
                Entry::new("secret.txt", EntryKind::File, 1, None, 0x2),
                Entry::new("link", EntryKind::Link { is_dir: true }, 0, None, 0),
            ],
        );
        fs.insert_dir(
            r"C:\Users\me\Documents",
            vec![file("季報告.pdf"), file("report.docx")],
        );
        fs.insert_dir(r"C:\Users\me\Pictures", vec![file("report-cover.png")]);
        fs.insert_dir(r"C:\$Recycle.Bin", vec![file("deleted-report.txt")]);
        fs.insert_dir(r"C:\Users\me\link", vec![file("inside-link.txt")]);
        let progress = BuildProgress::default();
        Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\")],
            4,
            &AtomicBool::new(false),
            &progress,
            &|| {},
            false,
        )
    }

    fn paths(hits: &[IndexHit]) -> Vec<String> {
        hits.iter().map(|h| h.path.display().to_string()).collect()
    }

    fn all_paths(index: &Index) -> Vec<String> {
        let mut out: Vec<String> = (0..index.len())
            .map(|i| index.path_of(i as u32).display().to_string())
            .collect();
        out.sort();
        out
    }

    /// One listing item (`parent` is a position in the listing).
    fn listed(parent: u32, name: &str, attributes: u32, size: u64) -> Listed {
        Listed {
            parent,
            name: name.into(),
            attributes,
            size,
            mtime: 1_700_000_000,
            link: false,
        }
    }

    #[test]
    fn builds_from_a_listing_with_the_same_rules_and_walks_the_rest() {
        const DIR: u32 = 0x10;
        let listing = vec![
            listed(LISTED_ROOT, "Users", DIR, 0),              // 0
            listed(LISTED_ROOT, "$Recycle.Bin", DIR | 0x6, 0), // 1
            listed(LISTED_ROOT, "pagefile.sys", 0x26, 4096),   // 2
            listed(0, "me", DIR, 0),                           // 3
            listed(1, "deleted.txt", 0x20, 1),                 // 4: in recycle bin, skipped
            listed(3, "proj", DIR, 0),                         // 5
            Listed {
                link: true,
                ..listed(3, "link", DIR | 0x400, 0) // 6
            },
            listed(3, "OneDrive-placeholder", DIR | 0x0040_0000, 0), // 7: cloud, not entered
            // proj's children span several batches (3 per batch): recognizing target as a build
            // folder needs Cargo.toml.
            listed(5, "src", DIR, 0),              // 8
            listed(5, "target", DIR, 0),           // 9
            listed(5, "README.md", 0x20, 10),      // 10
            listed(5, "Cargo.toml", 0x20, 20),     // 11
            listed(6, "inside-link.txt", 0x20, 1), // 12
            listed(7, "cloud.txt", 0x20, 1),       // 13
            listed(8, "main.rs", 0x20, 30),        // 14
            listed(9, "debug", DIR, 0),            // 15
            listed(15, "arke.exe", 0x20, 1),       // 16
        ];
        let journal = crate::journal::FakeJournal::new(1);
        journal.set_listing(r"C:\", listing);
        let fs = FakeFs::new();
        // Only D:\ is walked; FakeFs has nothing for C:\, so anything there came from the listing.
        fs.insert_dir(r"D:\", vec![file("walked.txt")]);
        let build = |list: Lister| {
            Index::build_listed(
                fs.as_ref(),
                &[PathBuf::from(r"C:\"), PathBuf::from(r"D:\")],
                2,
                &AtomicBool::new(false),
                &BuildProgress::default(),
                &|| {},
                // Excluded folders are skipped in the listing too.
                &BuildRules {
                    skip_build_dirs: true,
                    excluded: vec![PathBuf::from(r"c:\users\me\proj\SRC")],
                    max_items: None,
                },
                list,
            )
        };
        let j = Arc::clone(&journal);
        let index = build(&move |root, sink| j.list(root, sink));
        assert_eq!(
            all_paths(&index),
            [
                r"C:\",
                r"C:\$Recycle.Bin",
                r"C:\Users",
                r"C:\Users\me",
                r"C:\Users\me\OneDrive-placeholder",
                r"C:\Users\me\link",
                r"C:\Users\me\proj",
                r"C:\Users\me\proj\Cargo.toml",
                r"C:\Users\me\proj\README.md",
                r"C:\Users\me\proj\src",
                r"C:\Users\me\proj\target",
                r"C:\pagefile.sys",
                r"D:\",
                r"D:\walked.txt",
            ]
        );
        let hits = find(&index, "README", 1, true);
        assert_eq!((hits[0].size, hits[0].is_dir), (10, false));
        assert!(hits[0].modified.is_some());
        // System+hidden files are still marked hidden.
        assert!(find(&index, "pagefile", 1, false).is_empty());

        // Failure midway: partial additions removed and the root is walked instead (nothing to
        // walk for C:\ here, so only the root remains).
        let j = journal;
        let index = build(&move |root, sink| {
            let mut n = 0;
            j.list(root, &mut |batch| {
                n += 1;
                n < 3 && sink(batch)
            })
        });
        assert_eq!(all_paths(&index), [r"C:\", r"D:\", r"D:\walked.txt"]);
        // The name pool shrank too, so dedup never points into the removed span.
        assert_eq!(&*index.names, r"C:\D:\walked.txt");
    }

    /// A NAS share's snapshots are each a full copy of the share: walking them never ends on a
    /// share with many snapshots. They, the NAS recycle bin and thumbnail folders aren't entered.
    #[test]
    fn skips_nas_snapshots_recycle_bins_and_thumbnails() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"\\nas\share",
            vec![
                dir("docs"),
                dir("#snapshot"),
                dir("#recycle"),
                dir("@eaDir"),
            ],
        );
        fs.insert_dir(r"\\nas\share\docs", vec![file("plan.xlsx")]);
        fs.insert_dir(r"\\nas\share\#snapshot", vec![dir("GMT+08-2026.11.16")]);
        fs.insert_dir(
            r"\\nas\share\#snapshot\GMT+08-2026.11.16",
            vec![file("plan.xlsx")],
        );
        fs.insert_dir(r"\\nas\share\#recycle", vec![file("old-plan.xlsx")]);
        fs.insert_dir(r"\\nas\share\@eaDir", vec![file("plan.xlsx@SynoEAStream")]);
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"\\nas\share")],
            1,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        assert_eq!(
            all_paths(&index),
            [
                r"\\nas\share",
                r"\\nas\share\#recycle",
                r"\\nas\share\#snapshot",
                r"\\nas\share\@eaDir",
                r"\\nas\share\docs",
                r"\\nas\share\docs\plan.xlsx",
            ]
        );
    }

    /// Records which folders are listed.
    struct Listings {
        fs: Arc<FakeFs>,
        listed: parking_lot::Mutex<Vec<String>>,
        /// Folders that can't be listed.
        failing: Vec<String>,
        /// Folders whose listing breaks off with an error after what it has.
        broken: Vec<String>,
    }

    impl Listings {
        fn new(fs: Arc<FakeFs>) -> Self {
            Self {
                fs,
                listed: parking_lot::Mutex::new(Vec::new()),
                failing: Vec::new(),
                broken: Vec::new(),
            }
        }
    }

    impl Fs for Listings {
        fn read_dir(&self, path: &Path) -> io::Result<crate::fs::DirIter> {
            let shown = path.display().to_string();
            self.listed.lock().push(shown.clone());
            if self.failing.contains(&shown) {
                return Err(io::ErrorKind::NotConnected.into());
            }
            let iter = self.fs.read_dir(path)?;
            if self.broken.contains(&shown) {
                let cut = std::iter::once(Err(io::Error::from(io::ErrorKind::ConnectionReset)));
                return Ok(Box::new(iter.chain(cut)));
            }
            Ok(iter)
        }
        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.fs.stat(path)
        }
        fn watch(
            &self,
            path: &Path,
            on_event: crate::fs::WatchCallback,
        ) -> io::Result<Box<dyn Send>> {
            self.fs.watch(path, on_event)
        }
        fn read(&self, path: &Path, max_len: u64) -> io::Result<Vec<u8>> {
            self.fs.read(path, max_len)
        }
        fn read_head(&self, path: &Path, len: u64) -> io::Result<Vec<u8>> {
            self.fs.read_head(path, len)
        }
    }

    /// A folder last changed at Unix second `secs`.
    fn dir_at(name: &str, secs: u64) -> Entry {
        Entry::new(
            name,
            EntryKind::Dir,
            0,
            Some(UNIX_EPOCH + Duration::from_secs(secs)),
            0,
        )
    }

    /// The share used by the refresh tests. `later`: a file added to `changed` and to
    /// `parent\deep` (their times move; `parent`'s own time doesn't), and a file in `same` grown
    /// in place, which no folder time shows.
    fn refresh_share(later: bool) -> Arc<FakeFs> {
        let share = REFRESH_SHARE;
        let fs = FakeFs::new();
        let moved = if later { 2_000 } else { 1_000 };
        fs.insert_dir(
            share,
            vec![
                dir_at("same", 1_000),
                dir_at("changed", moved),
                dir_at("parent", 1_000),
                dir_at("empty", 1_000),
            ],
        );
        fs.insert_dir(format!(r"{share}\parent"), vec![dir_at("deep", moved)]);
        let mut a = file("a.txt");
        a.size = if later { 99 } else { 1 };
        fs.insert_dir(format!(r"{share}\same"), vec![a]);
        let added = |name: &str| later.then(|| file(name));
        fs.insert_dir(
            format!(r"{share}\changed"),
            [Some(file("b.txt")), added("b2.txt")]
                .into_iter()
                .flatten()
                .collect(),
        );
        fs.insert_dir(
            format!(r"{share}\parent\deep"),
            [Some(file("d.txt")), added("d2.txt")]
                .into_iter()
                .flatten()
                .collect(),
        );
        fs.insert_dir(format!(r"{share}\empty"), vec![]);
        fs
    }

    const REFRESH_SHARE: &str = r"\\nas\s";

    /// Builds (or with `old`, refreshes) through `fs`: the index, the folders listed (sorted)
    /// and the progress.
    fn walk_recorded(
        fs: &Listings,
        old: Option<(&Index, &[PathBuf])>,
    ) -> (Index, Vec<String>, BuildProgress) {
        let progress = BuildProgress::default();
        let cancel = AtomicBool::new(false);
        let rules = BuildRules::default();
        let index = match old {
            Some((old, relist)) => {
                Index::refresh(fs, old, relist, 2, &cancel, &progress, &|| {}, &rules)
            }
            None => Index::build(
                fs,
                &[PathBuf::from(REFRESH_SHARE)],
                2,
                &cancel,
                &progress,
                &|| {},
                false,
            ),
        };
        let mut listed = std::mem::take(&mut *fs.listed.lock());
        listed.sort();
        (index, listed, progress)
    }

    fn at(rel: &str) -> String {
        format!(r"{REFRESH_SHARE}{rel}")
    }

    /// A rewalk lists again only what may have changed: every folder with subfolders, folders
    /// whose time changed, and empty ones (a folder that couldn't be listed looks empty). A
    /// bottom-level folder with the same time keeps what the last walk found.
    #[test]
    fn refresh_lists_only_folders_that_may_have_changed() {
        let (old, listed, _) = walk_recorded(&Listings::new(refresh_share(false)), None);
        assert_eq!(listed.len(), 6);
        let (new, listed, progress) =
            walk_recorded(&Listings::new(refresh_share(true)), Some((&old, &[])));
        assert_eq!(
            listed,
            [
                at(""),
                at(r"\changed"),
                at(r"\empty"),
                at(r"\parent"),
                at(r"\parent\deep"),
            ]
        );
        assert_eq!(progress.reused.load(Ordering::Relaxed), 1);
        let (fresh, _, _) = walk_recorded(&Listings::new(refresh_share(true)), None);
        assert_eq!(all_paths(&new), all_paths(&fresh));
        let size_of =
            |index: &Index, name: &str| find(index, name, 1, true).first().map(|hit| hit.size);
        assert_eq!(size_of(&new, "a.txt"), Some(1), "taken over as it was");
        assert_eq!(size_of(&fresh, "a.txt"), Some(99));
    }

    /// A folder whose listing broke off partway isn't trusted to be skipped next time; neither
    /// are folders changes were seen in, folders that are reparse points (DFS links, which may
    /// never change their time) or folders changed close to the last walk (the server's clock
    /// may be off).
    #[test]
    fn refresh_lists_again_what_it_cannot_trust() {
        let mut first = Listings::new(refresh_share(false));
        first.broken = vec![at(r"\same")];
        let (old, _, _) = walk_recorded(&first, None);
        let (_, listed, _) = walk_recorded(&Listings::new(refresh_share(false)), Some((&old, &[])));
        assert!(
            listed.contains(&at(r"\same")),
            "listing broke off last time"
        );

        let (old, _, _) = walk_recorded(&Listings::new(refresh_share(false)), None);
        let touched = [PathBuf::from(at(r"\same"))];
        let (_, listed, _) =
            walk_recorded(&Listings::new(refresh_share(false)), Some((&old, &touched)));
        assert!(listed.contains(&at(r"\same")), "changes were seen there");

        let one_folder = |attributes: u32| {
            let fs = FakeFs::new();
            let same = Entry {
                attributes,
                ..dir_at("same", 1_000)
            };
            fs.insert_dir(REFRESH_SHARE, vec![same]);
            fs.insert_dir(at(r"\same"), vec![file("a.txt")]);
            fs
        };
        let (old, _, _) = walk_recorded(&Listings::new(one_folder(0)), None);
        let (_, listed, _) = walk_recorded(&Listings::new(one_folder(0)), Some((&old, &[])));
        assert!(
            !listed.contains(&at(r"\same")),
            "a plain folder is taken over"
        );
        let (_, listed, _) = walk_recorded(&Listings::new(one_folder(0x400)), Some((&old, &[])));
        assert!(listed.contains(&at(r"\same")), "a reparse point");

        let recent = UNIX_EPOCH.elapsed().unwrap().as_secs() - 60;
        let near = FakeFs::new();
        near.insert_dir(REFRESH_SHARE, vec![dir_at("same", recent)]);
        near.insert_dir(at(r"\same"), vec![file("a.txt")]);
        let (old, _, _) = walk_recorded(&Listings::new(Arc::clone(&near)), None);
        let (_, listed, _) = walk_recorded(&Listings::new(near), Some((&old, &[])));
        assert!(
            listed.contains(&at(r"\same")),
            "changed just before the walk"
        );
    }

    /// A folder that can't be listed during a refresh keeps what the last walk found below it;
    /// a root that can't be listed (the share is offline) is reported, so the caller keeps the
    /// index it has.
    #[test]
    fn refresh_keeps_what_it_cannot_list() {
        let (old, _, _) = walk_recorded(&Listings::new(refresh_share(false)), None);
        let mut offline = Listings::new(refresh_share(true));
        offline.failing = vec![at(r"\parent")];
        let (new, _, progress) = walk_recorded(&offline, Some((&old, &[])));
        assert!(!Index::roots_failed(&progress));
        assert!(all_paths(&new).contains(&at(r"\parent\deep\d.txt")));

        let mut down = Listings::new(refresh_share(true));
        down.failing = vec![at("")];
        let (_, _, progress) = walk_recorded(&down, Some((&old, &[])));
        assert!(Index::roots_failed(&progress));
    }

    /// While a first build runs, a copy of it is published only once it has grown by a quarter,
    /// so copies don't pile up.
    #[test]
    fn partial_copies_wait_for_the_walk_to_grow() {
        let progress = BuildProgress::default();
        progress.want_partial.store(true, Ordering::Relaxed);
        let mut index = Index::default();
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        for i in 0..100 {
            index.push(root, &format!("f{i}"), 0, 1, 0);
        }
        let mut last = None;
        index.publish_partial(&progress, &mut last);
        assert_eq!(progress.partial.lock().as_ref().map(|p| p.len()), Some(101));
        for i in 0..10 {
            index.push(root, &format!("g{i}"), 0, 1, 0);
        }
        std::thread::sleep(Duration::from_millis(30));
        index.publish_partial(&progress, &mut last);
        assert_eq!(progress.partial.lock().as_ref().map(|p| p.len()), Some(101));
        for i in 0..20 {
            index.push(root, &format!("h{i}"), 0, 1, 0);
        }
        std::thread::sleep(Duration::from_millis(30));
        index.publish_partial(&progress, &mut last);
        assert_eq!(progress.partial.lock().as_ref().map(|p| p.len()), Some(131));
    }

    /// The items below a folder come with the changes seen since the build, and without hidden
    /// ones (or what's inside hidden folders) unless asked for.
    #[test]
    fn items_under_a_folder_include_changes_and_leave_out_hidden_ones() {
        let fs = FakeFs::new();
        let hidden = |e: Entry| Entry {
            attributes: 0x2,
            ..e
        };
        fs.insert_dir(r"\\nas\s", vec![dir("docs"), dir("other")]);
        fs.insert_dir(
            r"\\nas\s\docs",
            vec![file("a.txt"), file("gone.txt"), hidden(dir(".cache"))],
        );
        fs.insert_dir(r"\\nas\s\docs\.cache", vec![file("c.bin")]);
        fs.insert_dir(r"\\nas\s\other", vec![file("x.txt")]);
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"\\nas\s")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        let mut overlay = Overlay::default();
        overlay.record(PathBuf::from(r"\\nas\s\docs\gone.txt"), None);
        let seen = Seen {
            is_dir: false,
            hidden: false,
            size: 7,
            modified: 0,
        };
        overlay.record(PathBuf::from(r"\\nas\s\docs\new.txt"), Some(seen));
        overlay.record(PathBuf::from(r"\\nas\s\docs\a.txt"), Some(seen));
        let cancel = AtomicBool::new(false);
        let items = |show_hidden: bool| {
            let mut items: Vec<(String, u64)> = index
                .items_under(Path::new(r"\\nas\s\docs"), &overlay, show_hidden, &cancel)
                .unwrap()
                .items
                .into_iter()
                .map(|item| (item.rel, item.entry.size))
                .collect();
            items.sort();
            items
        };
        let own = |list: &[(&str, u64)]| -> Vec<(String, u64)> {
            list.iter().map(|(p, s)| ((*p).to_owned(), *s)).collect()
        };
        assert_eq!(items(false), own(&[("a.txt", 7), ("new.txt", 7)]));
        assert_eq!(
            items(true),
            own(&[
                (".cache", 0),
                (r".cache\c.bin", 1),
                ("a.txt", 7),
                ("new.txt", 7)
            ])
        );
        assert!(
            index
                .items_under(Path::new(r"\\nas\elsewhere"), &overlay, true, &cancel)
                .is_none()
        );
    }

    /// Changes go where they belong: an item inside a hidden folder stays hidden with it, a file
    /// replaced by a folder is a folder, and a folder gone since the build has nothing to list.
    /// Folders the index doesn't enter (build folders here) are handed back to be walked;
    /// snapshots aren't.
    #[test]
    fn items_under_follow_changes_and_name_what_is_not_indexed() {
        let fs = FakeFs::new();
        let hidden = Entry {
            attributes: 0x2,
            ..dir(".cache")
        };
        fs.insert_dir(
            r"\\nas\s",
            vec![
                dir("docs"),
                hidden,
                file("swap"),
                dir("node_modules"),
                dir("#snapshot"),
            ],
        );
        fs.insert_dir(r"\\nas\s\docs", vec![file("a.txt")]);
        fs.insert_dir(r"\\nas\s\.cache", vec![]);
        fs.insert_dir(r"\\nas\s\node_modules", vec![file("lib.js")]);
        fs.insert_dir(r"\\nas\s\#snapshot", vec![file("old.txt")]);
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"\\nas\s")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            true,
        );
        let seen = |is_dir: bool| Seen {
            is_dir,
            hidden: false,
            size: 0,
            modified: 0,
        };
        let mut overlay = Overlay::default();
        overlay.record(PathBuf::from(r"\\nas\s\.cache\new.bin"), Some(seen(false)));
        overlay.record(PathBuf::from(r"\\nas\s\swap"), Some(seen(true)));
        let cancel = AtomicBool::new(false);
        let tree = index
            .items_under(Path::new(r"\\nas\s"), &overlay, false, &cancel)
            .unwrap();
        let mut names: Vec<(String, EntryKind)> = tree
            .items
            .iter()
            .map(|i| (i.rel.clone(), i.entry.kind))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                ("#snapshot".to_owned(), EntryKind::Dir),
                ("docs".to_owned(), EntryKind::Dir),
                (r"docs\a.txt".to_owned(), EntryKind::File),
                ("node_modules".to_owned(), EntryKind::Dir),
                ("swap".to_owned(), EntryKind::Dir),
            ]
        );
        assert_eq!(tree.unindexed, ["node_modules"]);
        overlay.record(PathBuf::from(r"\\nas\s\docs"), None);
        assert!(
            index
                .items_under(Path::new(r"\\nas\s\docs"), &overlay, false, &cancel)
                .is_none()
        );
    }

    /// A single snapshot can show up outside the snapshot folder (a link on the server, which a
    /// share shows as a plain folder, or Windows previous versions), and ZFS and snapper keep
    /// theirs in folders of their own: none of them are entered. Folders that only look a bit like
    /// one still are.
    #[test]
    fn snapshots_are_skipped_wherever_they_appear() {
        let fs = FakeFs::new();
        let snapshots = [
            "GMT+08-2026.10.13-08.00.02",
            "@GMT-2026.10.13-00.00.00",
            "gmt+05:30-2026.10.13-08.00.02",
            ".zfs",
            ".snapshots",
            "#Snapshot",
        ];
        let lookalikes = ["GMT+0", "GMT-2026", "gmt+08-2026.10.13", "snapshot"];
        fs.insert_dir(
            r"\\nas\share",
            snapshots
                .iter()
                .chain(&lookalikes)
                .map(|name| dir(name))
                .collect(),
        );
        for name in snapshots.iter().chain(&lookalikes) {
            fs.insert_dir(format!(r"\\nas\share\{name}"), vec![file("plan.xlsx")]);
        }
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"\\nas\share")],
            4,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        let walked: Vec<String> = all_paths(&index)
            .into_iter()
            .filter_map(|p| p.strip_suffix(r"\plan.xlsx").map(str::to_owned))
            .collect();
        let mut expected: Vec<String> = lookalikes
            .iter()
            .map(|name| format!(r"\\nas\share\{name}"))
            .collect();
        expected.sort();
        assert_eq!(walked, expected);
        for name in snapshots {
            assert!(is_skipped_dir(&name.to_lowercase()), "{name}");
        }
    }

    /// With a limit, the walk stops entering folders once the index is that big, so a huge share
    /// can't keep growing the index for hours.
    #[test]
    fn stops_walking_at_the_item_limit() {
        let fs = FakeFs::new();
        let folders: Vec<Entry> = (0..20).map(|i| dir(&format!("d{i:02}"))).collect();
        fs.insert_dir(r"\\nas\big", folders);
        for i in 0..20 {
            fs.insert_dir(
                format!(r"\\nas\big\d{i:02}"),
                (0..10).map(|j| file(&format!("f{j}.txt"))).collect(),
            );
        }
        let build = |max_items| {
            Index::build_listed(
                fs.as_ref(),
                &[PathBuf::from(r"\\nas\big")],
                1,
                &AtomicBool::new(false),
                &BuildProgress::default(),
                &|| {},
                &BuildRules {
                    max_items,
                    ..BuildRules::default()
                },
                &|_, _| Err(io::ErrorKind::Unsupported.into()),
            )
        };
        assert_eq!(build(None).len(), 1 + 20 + 200);
        let capped = build(Some(50)).len();
        // One folder's contents may land after the limit is reached, nothing more.
        assert!((50..=50 + 10).contains(&capped), "{capped} items");
    }

    #[test]
    fn builds_without_entering_links_or_recycle_bin() {
        let index = sample();
        let all = find(&index, "report", 50, true);
        let found = paths(&all);
        assert!(
            found.contains(&r"C:\Users\me\Documents\report.docx".to_string()),
            "{found:?}"
        );
        assert!(found.contains(&r"C:\Users\me\Pictures\report-cover.png".to_string()));
        // Neither the recycle bin nor links are entered.
        assert!(!found.iter().any(|p| p.contains("deleted-report")));
        assert!(find(&index, "inside-link", 10, true).is_empty());
        // But the recycle bin and links are still items themselves.
        assert_eq!(
            paths(&find(&index, "link", 10, true)),
            [r"C:\Users\me\link"]
        );
    }

    #[test]
    fn ranks_highlights_hidden_and_folder_queries() {
        let index = sample();
        let hits = find(&index, "report", 10, false);
        // Shorter names first on equal score.
        assert_eq!(
            hits[0].path,
            PathBuf::from(r"C:\Users\me\Documents\report.docx")
        );
        let name = hits[0]
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let marked: String = hits[0]
            .highlights
            .iter()
            .map(|r| &name[r.clone()])
            .collect();
        assert_eq!(marked, "report");
        // Hidden files are excluded by default.
        assert!(find(&index, "secret", 10, false).is_empty());
        assert_eq!(find(&index, "secret", 10, true).len(), 1);
        // CJK: multi-byte names go through the same char mask and fuzzy match.
        assert_eq!(
            paths(&find(&index, "季告", 10, false)),
            [r"C:\Users\me\Documents\季報告.pdf"],
            "two CJK chars still match fuzzily"
        );
        assert_eq!(
            paths(&find(&index, "季報告", 10, false)),
            [r"C:\Users\me\Documents\季報告.pdf"]
        );
        // "folder\name": the last segment matches the name, the rest the containing folder.
        assert_eq!(
            paths(&find(&index, r"pict\report", 10, false)),
            [r"C:\Users\me\Pictures\report-cover.png"]
        );
        assert!(find(&index, "", 10, false).is_empty());
        // 1-2 chars must be contiguous ("co" matches only report-cover, not the spread-out c...o in
        // Documents).
        assert_eq!(
            paths(&find(&index, "co", 10, false)),
            [r"C:\Users\me\Pictures\report-cover.png"]
        );
    }

    /// Mapping the saved file: same results as reading it into memory, no heap use; a corrupt file
    /// fails to open.
    #[test]
    fn opens_the_saved_file_without_copying_it() {
        let dir = std::env::temp_dir().join(format!("arke-index-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");
        let cursor = Cursor {
            journal: 7,
            usn: -3,
        };
        let index = sample().with_cursors(vec![Some(cursor)]);
        let mut file = std::fs::File::create(&path).unwrap();
        index.write_to(&mut file).unwrap();
        drop(file);
        {
            let mapped = Index::open(&path).unwrap();
            assert_eq!(mapped.len(), index.len());
            assert_eq!(mapped.cursor(0), Some(cursor));
            assert_eq!(mapped.heap_bytes(), 0, "not copied into memory");
            for query in ["report", "季報告", "ext:pdf", r"me\documents\"] {
                assert_eq!(
                    paths(&find(&mapped, query, 10, false)),
                    paths(&find(&index, query, 10, false)),
                    "{query}"
                );
            }
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 5);
        std::fs::write(&path, &bytes).unwrap();
        assert!(Index::open(&path).is_err(), "truncated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Identical names are stored once in the pool and paths stay correct.
    #[test]
    fn identical_names_are_stored_once() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\", vec![dir("a"), dir("b")]);
        fs.insert_dir(r"C:\a", vec![file("index.js"), file("package.json")]);
        fs.insert_dir(
            r"C:\b",
            vec![file("index.js"), file("package.json"), file("other.txt")],
        );
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        let unique = [r"C:\", "a", "b", "index.js", "package.json", "other.txt"];
        assert_eq!(
            index.names.len(),
            unique.iter().map(|s| s.len()).sum::<usize>()
        );
        let mut hits = paths(&find(&index, "index.js", 10, false));
        hits.sort();
        assert_eq!(hits, [r"C:\a\index.js", r"C:\b\index.js"]);
    }

    /// Records folded into the index are dropped with the room they took.
    #[test]
    fn a_pruned_overlay_gives_its_room_back() {
        let mut overlay = Overlay::default();
        let seen = Seen {
            is_dir: false,
            hidden: false,
            size: 1,
            modified: 0,
        };
        for i in 0..20_000 {
            overlay.record(PathBuf::from(format!(r"C:\build\out{i}.o")), Some(seen));
            overlay.record(PathBuf::from(format!(r"C:\old\gone{i}.o")), None);
        }
        let seq = overlay.seq();
        overlay.record(PathBuf::from(r"C:\later.txt"), Some(seen));
        overlay.prune_merged(seq, &[]);
        assert_eq!(overlay.len(), 1);
        assert!(
            overlay.added.capacity() < 100 && overlay.removed.capacity() < 100,
            "{} / {}",
            overlay.added.capacity(),
            overlay.removed.capacity()
        );
    }

    /// A build grows its columns by half, not double; a rebuild sets aside about what the index
    /// it replaces used and never grows. Only an index kept in memory is shrunk.
    #[test]
    fn builds_set_aside_little_more_than_they_use() {
        let fs = FakeFs::new();
        let folders: Vec<_> = (0..40).map(|i| dir(&format!("folder {i}"))).collect();
        fs.insert_dir(r"C:\", folders);
        for i in 0..40 {
            let files: Vec<_> = (0..500).map(|j| file(&format!("f{i}-{j}.txt"))).collect();
            fs.insert_dir(format!(r"C:\folder {i}"), files);
        }
        let build = |progress: &BuildProgress| {
            Index::build(
                fs.as_ref(),
                &[PathBuf::from(r"C:\")],
                2,
                &AtomicBool::new(false),
                progress,
                &|| {},
                false,
            )
        };
        let first = build(&BuildProgress::default());
        let items = first.len();
        assert_eq!(items, 1 + 40 + 40 * 500);
        let capacity = first.nodes.heap_capacity();
        assert!(
            capacity >= items && capacity <= items + items / 2 + 4096,
            "{capacity} for {items}"
        );

        let progress = BuildProgress::default();
        progress.expected.0.store(items, Ordering::Relaxed);
        progress
            .expected
            .1
            .store(first.name_bytes(), Ordering::Relaxed);
        let mut again = build(&progress);
        assert_eq!(again.len(), items);
        assert_eq!(again.nodes.heap_capacity(), items + items / 16);
        let names = first.name_bytes();
        assert_eq!(again.names.heap_capacity(), names + names / 16);

        again.shrink_to_fit();
        assert_eq!(again.nodes.heap_capacity(), items);
        assert_eq!(again.names.heap_capacity(), names);
        assert_eq!(
            paths(&find(&again, "f39-499", 10, false)),
            [r"C:\folder 39\f39-499.txt"]
        );
    }

    #[test]
    fn round_trips_through_bytes_and_rejects_garbage() {
        let cursor = Cursor {
            journal: 0xABCD_0123_4567_89EF,
            usn: 1_234_567_890_123,
        };
        let index = sample().with_cursors(vec![Some(cursor)]);
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        let back = Index::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(back.len(), index.len());
        assert_eq!(back.roots(), index.roots());
        assert_eq!(back.cursor(0), Some(cursor), "journal position is saved");
        assert_eq!(back.cursor(1), None);
        assert_eq!(
            paths(&find(&back, "report", 10, false)),
            paths(&find(&index, "report", 10, false))
        );
        assert!(Index::read_from(&mut &b"garbage!"[..]).is_err());
        bytes.truncate(bytes.len() - 3);
        assert!(Index::read_from(&mut bytes.as_slice()).is_err());
    }

    /// Files added after the build that score the same come back in the same order every time,
    /// and the same ones survive the limit: shorter names first, then by path, like the index.
    #[test]
    fn overlay_ties_keep_a_fixed_order() {
        let file = Some(Seen {
            is_dir: false,
            hidden: false,
            size: 5,
            modified: 0,
        });
        let names: Vec<String> = (0..40)
            .map(|i| format!(r"C:\new\report-{i:02}.md"))
            .chain([r"C:\new\z\report.md".to_owned()])
            .collect();
        let build = |order: &mut dyn Iterator<Item = &String>| {
            let mut overlay = Overlay::default();
            for name in order {
                overlay.record(PathBuf::from(name), file);
            }
            paths(&overlay.apply(Vec::new(), &q("report"), 10, false))
        };
        let forward = build(&mut names.iter());
        let backward = build(&mut names.iter().rev());
        assert_eq!(forward, backward);
        // The shortest name wins the tie; the rest follow by path.
        assert_eq!(forward[0], r"C:\new\z\report.md");
        let mut expected = names[..9].to_vec();
        expected.sort();
        assert_eq!(forward[1..], expected[..]);
    }

    #[test]
    fn overlay_adds_and_removes_after_the_build() {
        let index = sample();
        let file = Some(Seen {
            is_dir: false,
            hidden: false,
            size: 5,
            modified: 0,
        });
        let mut overlay = Overlay::default();
        overlay.record(PathBuf::from(r"C:\Users\me\new-report.md"), file);
        overlay.record(PathBuf::from(r"C:\Users\me\Documents\report.docx"), None);
        let found =
            paths(&overlay.apply(find(&index, "report", 50, false), &q("report"), 50, false));
        assert!(found.contains(&r"C:\Users\me\new-report.md".to_string()));
        assert!(!found.contains(&r"C:\Users\me\Documents\report.docx".to_string()));
        // Back again: no longer removed, and not duplicated.
        overlay.record(PathBuf::from(r"C:\Users\me\Documents\report.docx"), file);
        let found =
            paths(&overlay.apply(find(&index, "report", 50, false), &q("report"), 50, false));
        assert_eq!(
            found
                .iter()
                .filter(|p| p.ends_with(r"Documents\report.docx"))
                .count(),
            1
        );
        // Folder gone: everything under it is gone.
        overlay.record(PathBuf::from(r"C:\Users\me\Pictures"), None);
        let found = paths(&overlay.apply(find(&index, "cover", 50, false), &q("cover"), 50, false));
        assert!(found.is_empty(), "{found:?}");
        // The folder condition applies to added items too.
        let found = paths(&overlay.apply(Vec::new(), &q(r"me\new"), 50, false));
        assert_eq!(found, [r"C:\Users\me\new-report.md"]);
        assert!(
            overlay
                .apply(Vec::new(), &q(r"docs\new"), 50, false)
                .is_empty()
        );
        // Rebuild finished: records from before it started are dropped.
        let at = overlay.seq();
        overlay.record(PathBuf::from(r"C:\later.txt"), file);
        overlay.prune_before(at);
        assert_eq!(overlay.len(), 1);
    }

    #[test]
    fn skips_watch_events_outside_or_in_system_folders() {
        let roots = [PathBuf::from(r"C:\")];
        assert!(is_indexable(&roots, Path::new(r"C:\Users\me\a.txt")));
        assert!(!is_indexable(
            &roots,
            Path::new(r"C:\$Recycle.Bin\S-1\$R1.txt")
        ));
        assert!(!is_indexable(&roots, Path::new(r"D:\a.txt")));
    }

    #[test]
    fn filters_by_extension_size_date_and_kind() {
        let fs = FakeFs::new();
        let day = |d: u64| Some(UNIX_EPOCH + Duration::from_secs(1_790_000_000 + d * 86_400));
        fs.insert_dir(
            r"C:\docs",
            vec![
                Entry::new("big.pdf", EntryKind::File, 20 << 20, day(10), 0),
                Entry::new("small.pdf", EntryKind::File, 100, day(1), 0),
                Entry::new("notes.txt", EntryKind::File, 5, day(12), 0),
                Entry::new("pdfs", EntryKind::Dir, 0, day(3), 0),
            ],
        );
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        let names = |text: &str| -> Vec<String> {
            find(&index, text, 10, false)
                .iter()
                .map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        // Filters only: newest first.
        assert_eq!(names("ext:pdf"), ["big.pdf", "small.pdf"]);
        assert_eq!(names("*.pdf size:>1mb"), ["big.pdf"]);
        assert_eq!(names("pdf folder:"), ["pdfs"]);
        assert_eq!(names("pdf file:"), ["big.pdf", "small.pdf"]);
        assert_eq!(names("size:<1kb"), ["notes.txt", "small.pdf"]);
        let hit = &find(&index, "big", 1, false)[0];
        assert_eq!(hit.size, 20 << 20);
        assert_eq!(hit.modified, day(10));
        // Size and time survive a save and reload.
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        let back = Index::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(find(&back, "*.pdf size:>1mb", 5, false)[0].size, 20 << 20);
        assert_eq!(back.built_at(), index.built_at());
    }

    /// The char mask may let extra names through but must never reject a name nucleo accepts.
    #[test]
    fn char_mask_never_rejects_a_real_match() {
        let names = [
            "Report-2026.PDF",
            "café menu.txt",
            "Ünïcödé.md",
            "季報告-第3季.xlsx",
            "ΑΒΓ notes",
            "main_test.rs",
            "a b c",
            "日本語のファイル.txt",
        ];
        let queries = [
            "rep",
            "REP",
            "r26",
            "cafe",
            "café",
            "unicode",
            "季報",
            "報3",
            "αβγ",
            "ΑΒΓ",
            "mts",
            "main test",
            "^main",
            "'test",
            "rs$",
            "!foo",
            "abc",
            "日本ファ",
            "zzz",
            "c m",
        ];
        let mut matcher = Matcher::new(nucleo::Config::DEFAULT.match_paths());
        let mut buf = Vec::new();
        for query in queries {
            let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
            let needle = Needle::new(query);
            for name in names {
                let real = pattern
                    .score(Utf32Str::new(name, &mut buf), &mut matcher)
                    .is_some();
                if real {
                    assert!(
                        needle.may_match(char_mask(name)),
                        "{query:?} wrongly rejected {name:?}"
                    );
                }
            }
        }
        // And it does reject things.
        assert!(!Needle::new("zzz").may_match(char_mask("main_test.rs")));
        assert!(!Needle::new("季報").may_match(char_mask("report.pdf")));
        assert!(Needle::new("report").may_match(char_mask("Report-2026.PDF")));
    }

    /// Searching only within the previous matches gives the same results as a full search.
    #[test]
    fn narrowing_gives_the_same_results() {
        let index = sample();
        let search = |text: &str, within: Option<&[u32]>| {
            let query = q(text);
            index.search(&Search {
                query: &query,
                limit: 50,
                show_hidden: true,
                threads: 2,
                within,
                collect_matched: true,
                cancel: None,
            })
        };
        let first = search("rep", None);
        let matched = first.matched.unwrap();
        assert!(matched.windows(2).all(|w| w[0] < w[1]), "ascending");
        let narrowed = search("repo", Some(&matched));
        let fresh = search("repo", None);
        assert_eq!(paths(&narrowed.hits), paths(&fresh.hits));
        assert_eq!(narrowed.matched, fresh.matched);
        assert!(!narrowed.hits.is_empty());
    }

    #[test]
    fn cancel_stops_early() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\", vec![dir("a")]);
        fs.insert_dir(r"C:\a", vec![file("x")]);
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\")],
            2,
            &AtomicBool::new(true),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        // Only the root itself.
        assert_eq!(index.len(), 1);
    }

    /// Folding the overlay into the index gives what a fresh walk would: new files and moved-in
    /// folders appear, deleted files and folders (and everything below) go, changed files get
    /// their new size, a case-only rename takes the new name. Records whose folder isn't indexed
    /// stay in the overlay.
    #[test]
    fn merging_the_overlay_matches_a_fresh_walk() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\docs",
            vec![file("a.txt"), file("b.txt"), file("Case.md")],
        );
        fs.insert_dir(r"C:\old\deep", vec![file("y.txt")]);
        fs.add_file(Path::new(r"C:\old"), "x.txt", 1);
        fs.insert_dir(r"C:\proj\node_modules", vec![file("lib.js")]);
        let walk = |fs: &FakeFs| {
            Index::build(
                fs,
                &[PathBuf::from(r"C:\")],
                2,
                &AtomicBool::new(false),
                &BuildProgress::default(),
                &|| {},
                true,
            )
        };
        let base = walk(&fs);
        let mut overlay = Overlay::default();
        // As the index service records a change: an item found under another spelling (the
        // case changed) leaves the old one and goes in under its own.
        let mut record = |path: &str, fs: &FakeFs| {
            let mut path = PathBuf::from(path);
            if let Some(listed) = fs.listed_name(&path) {
                overlay.record(path.clone(), None);
                path.set_file_name(listed);
            }
            let seen = fs.stat(&path).ok().map(|e| Seen::of(&e));
            overlay.record(path, seen);
        };
        // A file inside a folder recorded before the folder was deleted: gone with it.
        record(r"C:\old\deep\y.txt", &fs);
        fs.remove(Path::new(r"C:\"), "old");
        record(r"C:\old", &fs);
        fs.add_file(Path::new(r"C:\docs"), "c.txt", 7);
        record(r"C:\docs\c.txt", &fs);
        fs.remove(Path::new(r"C:\docs"), "a.txt");
        record(r"C:\docs\a.txt", &fs);
        fs.add_file(Path::new(r"C:\docs"), "b.txt", 99);
        record(r"C:\docs\b.txt", &fs);
        fs.remove(Path::new(r"C:\docs"), "Case.md");
        fs.add_file(Path::new(r"C:\docs"), "case.md", 1);
        record(r"C:\docs\Case.md", &fs);
        record(r"C:\docs\case.md", &fs);
        // Moved in: the folder and its contents (as record_tree does), deepest last.
        fs.insert_dir(r"C:\moved\sub", vec![file("deep-report.txt")]);
        fs.add_file(Path::new(r"C:\moved"), "report.txt", 3);
        for p in [
            r"C:\moved",
            r"C:\moved\report.txt",
            r"C:\moved\sub",
            r"C:\moved\sub\deep-report.txt",
        ] {
            record(p, &fs);
        }
        // Inside a folder the index skips: can't be placed.
        fs.add_file(Path::new(r"C:\proj\node_modules"), "new.js", 1);
        record(r"C:\proj\node_modules\new.js", &fs);

        let merged = base
            .merged(
                &overlay,
                &BuildRules {
                    skip_build_dirs: true,
                    ..BuildRules::default()
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            merged.unplaced,
            [PathBuf::from(r"C:\proj\node_modules\new.js")]
        );
        let fresh = walk(&fs);
        assert_eq!(all_paths(&merged.index), all_paths(&fresh));
        let sizes = |index: &Index| {
            let mut v: Vec<(String, u64)> = (0..index.len())
                .map(|i| {
                    (
                        index.path_of(i as u32).display().to_string(),
                        index.sizes[i],
                    )
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(sizes(&merged.index), sizes(&fresh));
        assert!(merged.index.built_at > base.built_at, "sorts as newer");
        // Still searchable, and parents still come before children (folder sizes rely on it).
        assert_eq!(
            paths(&find(&merged.index, "deep-report", 5, false)),
            [r"C:\moved\sub\deep-report.txt"]
        );
        for (i, node) in merged.index.nodes.iter().enumerate() {
            assert!(node.parent == NO_PARENT || (node.parent as usize) < i);
        }
        // Round trip through the saved format.
        let mut bytes = Vec::new();
        merged.index.write_to(&mut bytes).unwrap();
        let back = Index::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(all_paths(&back), all_paths(&fresh));
        // Records it couldn't place stay; the rest go.
        overlay.prune_merged(overlay.seq(), &merged.unplaced);
        assert_eq!(overlay.len(), 1);
    }

    /// An index of `n` files under one folder.
    fn flat(n: u32) -> Index {
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        for i in 0..n {
            index.push(root, &format!("report_{i:07}.txt"), 0, 1, 0);
        }
        index
    }

    /// Typing on: the caller cancels the previous query, which stops scanning and reports so
    /// (no partial results that look complete, nothing kept for narrowing).
    #[test]
    fn cancelled_queries_stop_scanning() {
        let index = flat(200_000);
        let query = q("rprt");
        let search = |cancel: &AtomicBool| {
            index.search(&Search {
                query: &query,
                limit: 50,
                show_hidden: false,
                threads: 4,
                within: None,
                collect_matched: true,
                cancel: Some(cancel),
            })
        };
        let done = search(&AtomicBool::new(false));
        assert!(!done.cancelled);
        assert_eq!(done.hits.len(), 50);
        assert_eq!(done.matched.map(|m| m.len()), Some(200_000));
        let gave_up = search(&AtomicBool::new(true));
        assert!(gave_up.cancelled);
        assert!(gave_up.hits.is_empty() && gave_up.matched.is_none());
    }

    /// Queries run on threads kept between them, each reusing its scratch space, instead of a
    /// fresh thread (and matcher) per core for every keystroke.
    #[test]
    fn queries_reuse_scan_threads() {
        let index = flat(100_000);
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        for text in ["r", "re", "rep", "repo", "repor", "report"].repeat(5) {
            assert!(!index.find(&q(text), 10, false, cpus).is_empty(), "{text}");
        }
        let created = WORK_CREATED.load(Ordering::Relaxed);
        assert!(
            created <= cpus,
            "30 queries on {cpus} threads made {created} scratch spaces"
        );
    }

    fn sized(name: &str, size: u64) -> Entry {
        Entry::new(name, EntryKind::File, size, None, 0)
    }

    /// A tree with every kind of thing the size rules care about.
    fn size_tree() -> Arc<FakeFs> {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\w",
            vec![
                dir("app"),
                dir("docs"),
                dir("$Recycle.Bin"),
                sized("top.txt", 5),
                Entry::new("jdocs", EntryKind::Link { is_dir: true }, 0, None, 0),
            ],
        );
        fs.insert_dir(
            r"C:\w\app",
            vec![
                sized("Cargo.toml", 10),
                dir("src"),
                dir("target"),
                dir("node_modules"),
                // A file symlink: links never count.
                Entry::new("link.txt", EntryKind::Link { is_dir: false }, 0, None, 0),
                // Hidden + system folder: not indexed, still counted by a walk.
                Entry::new("sys", EntryKind::Dir, 0, None, 0x6),
            ],
        );
        fs.insert_dir(
            r"C:\w\app\src",
            vec![sized("main.rs", 100), sized("lib.rs", 200)],
        );
        fs.insert_dir(r"C:\w\app\target", vec![sized("app.exe", 1000)]);
        fs.insert_dir(r"C:\w\app\node_modules", vec![sized("x.js", 3000)]);
        fs.insert_dir(r"C:\w\app\sys", vec![sized("s.bin", 7)]);
        fs.insert_dir(r"C:\w\docs", vec![sized("a.pdf", 50), dir("old")]);
        fs.insert_dir(r"C:\w\docs\old", vec![sized("b.pdf", 60)]);
        fs.insert_dir(r"C:\w\$Recycle.Bin", vec![sized("gone", 9)]);
        fs
    }

    fn walked(fs: &FakeFs, path: &str) -> u64 {
        crate::dirsize::dir_size(fs, Path::new(path), &AtomicBool::new(false))
            .unwrap()
            .bytes
    }

    /// Index sums plus walking what it reports unindexed: same as walking everything.
    fn from_index(
        index: &Index,
        fs: &FakeFs,
        dir: &str,
        overlay: &Overlay,
    ) -> Vec<(String, u64, u64)> {
        let cancel = AtomicBool::new(false);
        let mut out: Vec<(String, u64, u64)> = index
            .child_dir_sizes(Path::new(dir), overlay, &cancel)
            .unwrap()
            .into_iter()
            .map(|c| {
                let mut size = c.size;
                for u in &c.unindexed {
                    let w = crate::dirsize::dir_size(fs, u, &cancel).unwrap();
                    size.bytes += w.bytes;
                    size.files += w.files;
                }
                (c.name, size.bytes, size.files)
            })
            .collect();
        out.sort();
        out
    }

    /// The Properties count from the index (totals, categories, space on disk, folders, each
    /// folder inside), with the folders it skips walked: the same as walking everything.
    #[test]
    fn usage_from_the_index_matches_a_walk() {
        let fs = size_tree();
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\w")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            true,
        );
        let cancel = AtomicBool::new(false);
        let cluster = 4096;
        let (direct, children) = index
            .usage_below(Path::new(r"C:\w"), &Overlay::default(), cluster, &cancel)
            .unwrap();
        let mut total = direct;
        let mut sizes: Vec<(String, u64)> = Vec::new();
        for child in children {
            let mut usage = child.usage;
            for u in &child.unindexed {
                usage += crate::dirsize::usage(fs.as_ref(), u, cluster, &cancel, &mut |_| {})
                    .unwrap()
                    .usage;
            }
            total += usage;
            sizes.push((child.name, usage.bytes));
        }
        sizes.sort();
        let walk = crate::dirsize::usage(
            fs.as_ref(),
            Path::new(r"C:\w"),
            cluster,
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(total, walk.usage, "totals, categories, on disk and folders");
        let mut walked: Vec<(String, u64)> = walk
            .children
            .iter()
            .map(|(p, bytes)| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    *bytes,
                )
            })
            .collect();
        walked.sort();
        assert_eq!(sizes, walked, "each folder inside");
    }

    /// Files added or changed right in the folder after the build (a fresh download) count: the
    /// index's old copy of a changed one is left out, the new size goes in.
    #[test]
    fn usage_from_the_index_counts_files_changed_since_the_build() {
        let fs = size_tree();
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\w")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            true,
        );
        let seen = |size| Seen {
            is_dir: false,
            hidden: false,
            size,
            modified: 0,
        };
        let mut overlay = Overlay::default();
        overlay.record(PathBuf::from(r"C:\w\download.iso"), Some(seen(2_000)));
        overlay.record(PathBuf::from(r"C:\w\top.txt"), Some(seen(50)));
        let cancel = AtomicBool::new(false);
        let (direct, _) = index
            .usage_below(Path::new(r"C:\w"), &overlay, 4096, &cancel)
            .unwrap();
        assert_eq!(direct.files, 2, "top.txt once, plus the download");
        assert_eq!(direct.bytes, 2_050);
    }

    #[test]
    fn folder_sizes_from_the_index_match_a_walk() {
        let fs = size_tree();
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\w")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            true,
        );
        let none = Overlay::default();
        let sizes = from_index(&index, &fs, r"C:\w", &none);
        assert_eq!(
            sizes,
            [
                ("$Recycle.Bin".to_string(), 9, 1),
                ("app".to_string(), 4317, 6),
                ("docs".to_string(), 110, 2),
            ],
            "the junction is skipped, as by the walker"
        );
        for (name, bytes, _) in &sizes {
            assert_eq!(*bytes, walked(&fs, &format!(r"C:\w\{name}")), "{name}");
        }
        // Any letter case finds the folder; folders the index has no contents for don't resolve.
        assert_eq!(from_index(&index, &fs, r"c:\W\APP", &none).len(), 4);
        let cancel = AtomicBool::new(false);
        assert!(
            index
                .child_dir_sizes(Path::new(r"C:\w\app\node_modules"), &none, &cancel)
                .is_none()
        );
        assert!(
            index
                .child_dir_sizes(Path::new(r"C:\w\jdocs"), &none, &cancel)
                .is_none()
        );
        assert!(
            index
                .child_dir_sizes(Path::new(r"C:\other"), &none, &cancel)
                .is_none()
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(
            index
                .child_dir_sizes(Path::new(r"C:\w"), &none, &cancel)
                .is_none()
        );
    }

    #[test]
    fn folder_sizes_follow_the_overlay() {
        let fs = size_tree();
        let index = Index::build(
            fs.as_ref(),
            &[PathBuf::from(r"C:\w")],
            2,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            true,
        );
        let file = |size| {
            Some(Seen {
                is_dir: false,
                hidden: false,
                size,
                modified: 0,
            })
        };
        let mut overlay = Overlay::default();
        // Modified, deleted, a deleted folder, new files in an old and in a new folder, and a
        // change inside a folder that is walked anyway (must not count twice).
        overlay.record(PathBuf::from(r"C:\w\app\src\main.rs"), file(150));
        overlay.record(PathBuf::from(r"C:\w\app\src\lib.rs"), None);
        overlay.record(PathBuf::from(r"C:\w\docs\old"), None);
        overlay.record(PathBuf::from(r"C:\w\docs\new.pdf"), file(1));
        overlay.record(PathBuf::from(r"C:\w\fresh\a.txt"), file(2));
        overlay.record(PathBuf::from(r"C:\w\app\sys\s2.bin"), file(1000));
        overlay.record(PathBuf::from(r"C:\w\top2.txt"), file(1000));
        let sub = overlay.under(Path::new(r"C:\w"));
        assert_eq!(sub.len(), overlay.len());
        assert_eq!(
            from_index(&index, &fs, r"C:\w", &sub),
            [
                ("$Recycle.Bin".to_string(), 9, 1),
                ("app".to_string(), 4317 + 50 - 200, 5),
                ("docs".to_string(), 51, 2),
                ("fresh".to_string(), 2, 1),
            ]
        );
        // A removed folder (or one under it) has no size.
        assert!(
            index
                .child_dir_sizes(
                    Path::new(r"C:\w\docs\old"),
                    &overlay,
                    &AtomicBool::new(false)
                )
                .is_none()
        );
        assert!(overlay.under(Path::new(r"C:\w\docs")).len() < overlay.len());
    }

    /// Folder sizes from the index vs walking (`cargo test --release -p arke-core -- --ignored
    /// --nocapture folder_size_speed`). Builds a tree under `%TEMP%` and removes it afterwards.
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn folder_size_speed() {
        let root = std::env::temp_dir().join(format!("arke-dirsize-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for top in 0..20 {
            for sub in 0..50 {
                let dir = root
                    .join(format!("top{top:02}"))
                    .join(format!("sub{sub:02}"));
                std::fs::create_dir_all(&dir).unwrap();
                for f in 0..100 {
                    std::fs::write(dir.join(format!("f{f:03}.txt")), [0u8; 10]).unwrap();
                }
            }
        }
        let fs = crate::fs::RealFs;
        let cancel = AtomicBool::new(false);
        let t = std::time::Instant::now();
        let mut walked = 0;
        for top in 0..20 {
            walked += crate::dirsize::dir_size(&fs, &root.join(format!("top{top:02}")), &cancel)
                .unwrap()
                .bytes;
        }
        let walk_time = t.elapsed();
        let t = std::time::Instant::now();
        let index = Index::build(
            &fs,
            std::slice::from_ref(&root),
            8,
            &cancel,
            &BuildProgress::default(),
            &|| {},
            false,
        );
        let build_time = t.elapsed();
        let t = std::time::Instant::now();
        let sizes = index
            .child_dir_sizes(&root, &Overlay::default(), &cancel)
            .unwrap();
        let index_time = t.elapsed();
        let from_index: u64 = sizes.iter().map(|c| c.size.bytes).sum();
        assert_eq!(from_index, walked);
        println!(
            "{} files in 20 folders: walk {walk_time:?}, index pass {index_time:?} \
             (one-off build {build_time:?})",
            index.len()
        );
        // The pass over a large index, for a folder near the root.
        let mut big = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let top = big.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        let mut dirs = vec![top];
        for i in 0..20_000u32 {
            let parent = dirs[(i as usize) / 20];
            dirs.push(big.push(parent, &format!("d{i}"), FLAG_DIR, 0, 0).unwrap());
        }
        for i in 0..3_000_000u32 {
            big.push(dirs[i as usize % dirs.len()], &format!("f{i}"), 0, 10, 0);
        }
        let t = std::time::Instant::now();
        let sizes = big
            .child_dir_sizes(Path::new(r"C:\"), &Overlay::default(), &cancel)
            .unwrap();
        println!(
            "{} items: {} subfolders of C:\\ in {:?}",
            big.len(),
            sizes.len(),
            t.elapsed()
        );
        // What the Properties window asks for a whole drive: categories and space on disk too.
        let t = std::time::Instant::now();
        let (_, usage) = big
            .usage_below(Path::new(r"C:\"), &Overlay::default(), 4096, &cancel)
            .unwrap();
        println!(
            "{} items: Properties usage of C:\\ ({} folders inside) in {:?}",
            big.len(),
            usage.len(),
            t.elapsed()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Query speed over many items (`cargo test -p arke-core -- --ignored --nocapture
    /// index_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn index_speed() {
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        // A CJK word keeps the non-ASCII match path in the measurement.
        let words = [
            "report",
            "photo",
            "季報告",
            "main",
            "readme",
            "cargo",
            "index",
            "src",
        ];
        for i in 0..2_000_000u32 {
            let name = format!(
                "{}_{i:07}.{}",
                words[i as usize % words.len()],
                ["txt", "rs", "jpg"][i as usize % 3]
            );
            index.push(root + i / 1000, &name, 0, u64::from(i), 1_700_000_000 + i);
        }
        let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        // Median of many runs: one keystroke is one query, so this is the per-keystroke latency.
        let median = |text: &str| {
            let query = q(text);
            let mut times: Vec<Duration> = (0..41)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(index.find(&query, 50, false, threads));
                    t.elapsed()
                })
                .collect();
            times.sort_unstable();
            times[times.len() / 2]
        };
        for text in [
            "r",
            "rp",
            "rprt",
            "季報",
            "main_00012",
            "zzzz",
            "ext:rs size:>1000000",
        ] {
            let hits = index.find(&q(text), 50, false, threads).len();
            println!(
                "{text:>22}: {hits:>2} hits, median {:>9.2?} ({threads} threads, {} items)",
                median(text),
                index.len()
            );
        }
        // A small index, where getting the scan going weighs the most.
        let small = Index {
            nodes: Column::Owned(index.nodes[..40_000].to_vec()),
            sizes: Column::Owned(index.sizes[..40_000].to_vec()),
            mtimes: Column::Owned(index.mtimes[..40_000].to_vec()),
            masks: Column::Owned(index.masks[..40_000].to_vec()),
            names: Names::Owned(index.names.to_string()),
            roots: index.roots.clone(),
            ..Index::default()
        };
        let zzzz = q("zzzz");
        let mut times: Vec<Duration> = (0..201)
            .map(|_| {
                let t = std::time::Instant::now();
                std::hint::black_box(small.find(&zzzz, 50, false, threads));
                t.elapsed()
            })
            .collect();
        times.sort_unstable();
        println!(
            "{:>22}: median {:>9.2?} ({} items)",
            "zzzz (small)",
            times[times.len() / 2],
            small.len()
        );
    }

    /// The contents of several folders in one pass: names in any letter case find the folder,
    /// nested folders and the root work, and folders the index lacks (or holds only as a file or
    /// a link) are left out.
    #[test]
    fn children_of_finds_each_folder_once() {
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        let users = index.push(root, "Users", FLAG_DIR, 0, 0).unwrap();
        let build = index.push(users, "build", FLAG_DIR, 0, 0).unwrap();
        index.push(build, "a.txt", 0, 1, 0).unwrap();
        let sub = index.push(build, "Sub", FLAG_DIR, 0, 0).unwrap();
        index.push(sub, "b.txt", 0, 1, 0).unwrap();
        index.push(users, "notes.txt", 0, 1, 0).unwrap();
        index
            .push(users, "link", FLAG_DIR | FLAG_LINK, 0, 0)
            .unwrap();
        let dirs = [
            PathBuf::from(r"C:\users\BUILD"),
            PathBuf::from(r"C:\Users\build\sub"),
            PathBuf::from(r"C:\"),
            PathBuf::from(r"C:\Users\missing"),
            PathBuf::from(r"C:\Users\notes.txt"),
            PathBuf::from(r"C:\Users\link"),
            PathBuf::from(r"D:\elsewhere"),
        ];
        let got = index.children_of(&dirs, &AtomicBool::new(false));
        let names = |d: &str| -> Option<Vec<(String, bool)>> {
            got.iter()
                .find(|(p, _)| p == Path::new(d))
                .map(|(_, c)| c.clone())
        };
        assert_eq!(
            names(r"C:\users\BUILD"),
            Some(vec![("a.txt".to_owned(), false), ("Sub".to_owned(), true)])
        );
        assert_eq!(
            names(r"C:\Users\build\sub"),
            Some(vec![("b.txt".to_owned(), false)])
        );
        assert_eq!(names(r"C:\"), Some(vec![("Users".to_owned(), true)]));
        assert_eq!(got.len(), 3, "{got:?}");
    }

    /// Comparing the folders of one batch of changes with a 4M-item index: one pass for all of them
    /// (`cargo test --release -p arke-core -- --ignored --nocapture children_of_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn children_of_speed() {
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        let mut dirs = Vec::new();
        for d in 0..4000u32 {
            dirs.push(
                index
                    .push(root, &format!("dir{d:04}"), FLAG_DIR, 0, 0)
                    .unwrap(),
            );
        }
        for i in 0..4_000_000u32 {
            index.push(
                dirs[(i % 4000) as usize],
                &format!("file_{i:07}.txt"),
                0,
                1,
                0,
            );
        }
        for count in [1usize, 100, 2000] {
            let wanted: Vec<PathBuf> = (0..count)
                .map(|d| PathBuf::from(format!(r"C:\dir{:04}", d * 2 % 4000)))
                .collect();
            let t = std::time::Instant::now();
            let got = index.children_of(&wanted, &AtomicBool::new(false));
            println!(
                "children_of {count} folders in {} items: {:?} ({} children)",
                index.len(),
                t.elapsed(),
                got.iter().map(|(_, c)| c.len()).sum::<usize>()
            );
        }
    }

    /// Folding 100k changes into a 4M-item index, what replaces a full rebuild when the overlay
    /// fills up (`cargo test --release -p arke-core -- --ignored --nocapture merge_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn merge_speed() {
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        let mut dirs = Vec::new();
        for d in 0..4000u32 {
            dirs.push(
                index
                    .push(root, &format!("dir{d:04}"), FLAG_DIR, 0, 0)
                    .unwrap(),
            );
        }
        for i in 0..4_000_000u32 {
            index.push(
                dirs[(i % 4000) as usize],
                &format!("file_{i:07}.txt"),
                0,
                1,
                0,
            );
        }
        let mut overlay = Overlay::default();
        let seen = Seen {
            is_dir: false,
            hidden: false,
            size: 7,
            modified: 0,
        };
        for i in 0..100_000u32 {
            let path = PathBuf::from(format!(r"C:\dir{:04}\file_{i:07}.txt", i % 4000));
            // Half changed in place, a quarter deleted, a quarter new.
            let seen = match i % 4 {
                0 | 1 => Some(seen),
                2 => None,
                _ => {
                    overlay.record(
                        PathBuf::from(format!(r"C:\dir{:04}\new_{i:07}.txt", i % 4000)),
                        Some(seen),
                    );
                    continue;
                }
            };
            overlay.record(path, seen);
        }
        let t = std::time::Instant::now();
        let merged = index
            .merged(&overlay, &BuildRules::default(), &AtomicBool::new(false))
            .unwrap();
        println!(
            "merged {} changes into {} items: {:?} ({} items after, {} unplaced)",
            overlay.len(),
            index.len(),
            t.elapsed(),
            merged.index.len(),
            merged.unplaced.len()
        );
    }

    /// Fast typing: four queries 2 ms apart, each on its own thread like the app's. Latency of
    /// the last one, with the earlier ones left running or cancelled (`cargo test --release -p
    /// arke-core -- --ignored --nocapture typing_burst_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn typing_burst_speed() {
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        for i in 0..2_000_000u32 {
            let name = format!("{}_{i:07}.txt", ["report", "photo", "main"][i as usize % 3]);
            index.push(root + i / 1000, &name, 0, u64::from(i), 0);
        }
        let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let texts = ["r", "rp", "rpr", "rprt"].map(q);
        for cancel in [false, true] {
            let mut last: Vec<Duration> = (0..21)
                .map(|_| {
                    let flags: Vec<AtomicBool> =
                        texts.iter().map(|_| AtomicBool::new(false)).collect();
                    std::thread::scope(|scope| {
                        let mut handles = Vec::new();
                        for (k, query) in texts.iter().enumerate() {
                            if cancel && k > 0 {
                                flags[k - 1].store(true, Ordering::Relaxed);
                            }
                            let flag = &flags[k];
                            let index = &index;
                            let started = std::time::Instant::now();
                            handles.push(scope.spawn(move || {
                                index.search(&Search {
                                    query,
                                    limit: 50,
                                    show_hidden: false,
                                    threads,
                                    within: None,
                                    collect_matched: false,
                                    cancel: Some(flag),
                                });
                                started.elapsed()
                            }));
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        handles.pop().unwrap().join().unwrap()
                    })
                })
                .collect();
            last.sort_unstable();
            println!(
                "typing burst, earlier queries {}: last query median {:>9.2?}",
                if cancel { "cancelled" } else { "left running" },
                last[last.len() / 2]
            );
        }
    }

    /// Cost of the final ranking (`crate::rank`) on top of a query over 1M items, with a full
    /// visit history (`cargo test --release -p arke-core -- --ignored --nocapture rank_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn rank_speed() {
        use crate::frecency::Frecency;
        use crate::rank::{RankSignals, Stat, frecent_hits, merge};
        let mut index = Index {
            roots: vec![PathBuf::from(r"C:\")],
            ..Index::default()
        };
        let root = index.push(NO_PARENT, r"C:\", FLAG_DIR, 0, 0).unwrap();
        // 1000 folders of 1000 files; many identical names, like README.md on a developer disk.
        let words = [
            "report",
            "photo",
            "readme",
            "main",
            "index",
            "季報告",
            "cargo",
            "notes",
        ];
        let mut dirs = Vec::new();
        for d in 0..1000u32 {
            dirs.push(
                index
                    .push(root, &format!("dir{d:04}"), FLAG_DIR, 0, 0)
                    .unwrap(),
            );
        }
        for i in 0..1_000_000u32 {
            let w = words[i as usize % words.len()];
            let name = if i % 7 == 0 {
                format!("{w}.md")
            } else {
                format!("{w}_{i:07}.txt")
            };
            index.push(
                dirs[(i / 1000) as usize],
                &name,
                0,
                u64::from(i),
                1_700_000_000 + i,
            );
        }
        let now = 1_800_000_000;
        let mut history = Frecency::default();
        for i in 0..crate::frecency::MAX_ITEMS as u32 {
            let w = words[i as usize % words.len()];
            history.record(
                Path::new(&format!(r"C:\dir{:04}\{w}.md", i % 1000)),
                false,
                now + u64::from(i) * 60,
            );
        }
        let snapshot = Arc::new(history.snapshot(now + 86_400));
        let signals = RankSignals::new(
            Some(Arc::clone(&snapshot)),
            Some(Path::new(r"C:\Users\me")),
            &[PathBuf::from(r"C:\Windows")],
        );
        let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let stat = |_: &Path| {
            Some(Stat {
                is_dir: false,
                size: 1,
                modified: None,
                hidden: false,
            })
        };
        for text in [
            "readme",
            "rprt",
            "季報",
            "main_00012",
            "notes ext:md",
            "zzzz",
        ] {
            let query = q(text);
            let mut search = Duration::MAX;
            let mut ranked = Duration::MAX;
            let mut out = Vec::new();
            for _ in 0..5 {
                let t = std::time::Instant::now();
                let hits = index.find(&query, 1000, false, threads);
                search = search.min(t.elapsed());
                let t = std::time::Instant::now();
                let extra = frecent_hits(&snapshot, &query, false, 32, |_| true, stat);
                out = merge(hits, extra, &query, &signals, 1000);
                ranked = ranked.min(t.elapsed());
            }
            println!(
                "{text:>14}: {:>4} hits, search {search:>9.2?}, rank {ranked:>9.2?}  {}",
                out.len(),
                out.first().map_or_default(|h| h.path.display().to_string())
            );
        }
    }

    /// Real disk (lists directories only, opens no files): `cargo test --release -p arke-core --
    /// --ignored --nocapture real_drive_index`
    #[test]
    #[ignore = "benchmark; run manually (walks the whole local drive)"]
    #[allow(
        clippy::print_stdout,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn real_drive_index() {
        let roots: Vec<PathBuf> = std::env::var("ARKE_INDEX_ROOTS")
            .unwrap_or_else(|_| r"C:\".into())
            .split(';')
            .map(PathBuf::from)
            .collect();
        let threads = 8;
        let t = std::time::Instant::now();
        let index = Index::build(
            &crate::fs::RealFs,
            &roots,
            threads,
            &AtomicBool::new(false),
            &BuildProgress::default(),
            &|| {},
            false,
        );
        println!(
            "build {roots:?}: {} items, {:.1} s, {:.0} MB (names {:.0} MB)",
            index.len(),
            t.elapsed().as_secs_f32(),
            index.heap_bytes() as f64 / 1e6,
            index.names.len() as f64 / 1e6
        );
        let mut bytes = Vec::new();
        let t = std::time::Instant::now();
        index.write_to(&mut bytes).unwrap();
        let back = Index::read_from(&mut bytes.as_slice()).unwrap();
        println!(
            "save+load: {:.0} MB, {:?} ({} items)",
            bytes.len() as f64 / 1e6,
            t.elapsed(),
            back.len()
        );
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        for text in [
            "cargo.toml",
            "rprt",
            "notepad",
            r"system32\drivers\etc",
            "zzzzqq",
            "ext:pdf size:>10mb",
            "dm:7d",
            "c",
            "ca",
            // Non-ASCII query path.
            "季報",
        ] {
            // Take the best of several runs (the first is affected by caching).
            let mut best = Duration::MAX;
            let mut hits = Vec::new();
            for _ in 0..5 {
                let t = std::time::Instant::now();
                hits = index.find(&q(text), 50, false, cpus);
                best = best.min(t.elapsed());
            }
            println!(
                "{text:>24}: {:>2} hits, {:>8.2?}  {}",
                hits.len(),
                best,
                hits.first()
                    .map_or_default(|h| h.path.display().to_string())
            );
        }
        // Narrow while typing: c -> ca -> car -> carg -> cargo.
        let mut within: Option<Vec<u32>> = None;
        for text in ["c", "ca", "car", "carg", "cargo"] {
            let narrowable = text.len() >= 3;
            let t = std::time::Instant::now();
            let query = q(text);
            let result = index.search(&Search {
                query: &query,
                limit: 50,
                show_hidden: false,
                threads: cpus,
                within: within.as_deref(),
                collect_matched: narrowable,
                cancel: None,
            });
            println!(
                "{text:>24}: {:>8.2?}  narrowed to {}",
                t.elapsed(),
                result.matched.as_ref().map_or(0, Vec::len)
            );
            within = result.matched;
        }
    }
}
