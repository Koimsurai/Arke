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
/// Folders never entered: the system recycle bin, restore data and Windows Update staging.
const SKIP_DIRS: [&str; 4] = [
    "$recycle.bin",
    "system volume information",
    "$windows.~bt",
    "$windows.~ws",
];
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
    /// Build-time only: name hash -> existing span in the string pool (each name stored once).
    /// Dropped after the build.
    dedup: Option<HashMap<u64, (u32, u16)>>,
}

/// Build progress: items listed so far.
#[derive(Default, Debug)]
pub struct BuildProgress {
    /// Items listed so far; updated from the walker threads.
    pub scanned: AtomicUsize,
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
}

/// Output of [`Index::search`].
pub struct SearchResult {
    /// Best hits, highest score first, at most `limit`.
    pub hits: Vec<IndexHit>,
    /// All matching ids, ascending; only with `collect_matched`.
    pub matched: Option<Vec<u32>>,
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
        let key = self.dedup.as_ref().map(|_| name_hash(name));
        let known = key.and_then(|k| {
            let &(start, len) = self.dedup.as_ref()?.get(&k)?;
            let start_ix = start as usize;
            (len == name_len && self.names.get(start_ix..start_ix + len as usize) == Some(name))
                .then_some(start)
        });
        let name_start = if let Some(start) = known {
            start
        } else {
            let start = u32::try_from(self.names.len()).ok()?;
            self.names.push_str(name);
            if let (Some(k), Some(map)) = (key, self.dedup.as_mut()) {
                map.entry(k).or_insert((start, name_len));
            }
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
                excluded: Vec::new(),
            },
            &|_, _| Err(io::ErrorKind::Unsupported.into()),
        )
    }

    /// Like [`Index::build`], but each root is first offered to `list` (the background service
    /// reads the MFT and lists a whole drive in seconds). Roots it can't list (no service, not
    /// NTFS, failure midway) are walked.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
        let mut index = Self {
            roots: roots.to_vec(),
            built_at: unix_secs(SystemTime::now()),
            skipped_build_dirs: rules.skip_build_dirs,
            excluded_hash: rules.fingerprint(),
            dedup: Some(HashMap::new()),
            ..Self::default()
        };
        // Work: folders to list (node id, path). Results: their contents.
        let (work_tx, work_rx) = flume::unbounded::<(u32, PathBuf)>();
        let (done_tx, done_rx) = flume::unbounded::<(u32, Vec<Entry>)>();
        let mut pending = 0usize;
        for root in roots {
            let name = root.to_string_lossy();
            let Some(id) = index.push(NO_PARENT, &name, FLAG_DIR, 0, 0) else {
                continue;
            };
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
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                let work_rx = work_rx.clone();
                let done_tx = done_tx.clone();
                scope.spawn(move || {
                    thread_init();
                    while let Ok((id, path)) = work_rx.recv() {
                        let entries: Vec<Entry> = if cancel.load(Ordering::Relaxed) {
                            Vec::new()
                        } else {
                            fs.read_dir(&path)
                                .map(|iter| iter.filter_map(Result::ok).collect())
                                .unwrap_or_default()
                        };
                        if done_tx.send((id, entries)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(done_tx);
            // Collector: this thread owns the index, assigns ids to subfolders and sends them back
            // to be listed.
            while pending > 0 {
                let Ok((parent, entries)) = done_rx.recv() else {
                    break;
                };
                pending -= 1;
                progress.scanned.fetch_add(entries.len(), Ordering::Relaxed);
                let parent_path = index.path_of(parent);
                index.add_entries(parent, Some(&parent_path), &entries, rules, |id, entry| {
                    if cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    pending += 1;
                    let _ = work_tx.send((id, parent_path.join(&*entry.name)));
                    true
                });
            }
            drop(work_tx);
        });
        index.dedup = None;
        index.names.shrink_to_fit();
        index.nodes.shrink_to_fit();
        index.sizes.shrink_to_fit();
        index.mtimes.shrink_to_fit();
        index.masks.shrink_to_fit();
        index
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
        // Dedup entries pointing into the removed names are no longer valid.
        if let Some(map) = self.dedup.as_mut() {
            map.retain(|_, &mut (start, len)| start as usize + len as usize <= names);
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
        const NONE: u32 = u32::MAX;
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
                    continue;
                }
                slot = u32::try_from(out.len()).ok()?;
                out.push(IndexedDirSize {
                    name: self.name(node).to_string(),
                    ..IndexedDirSize::default()
                });
            }
            let child = &mut out[slot as usize];
            if is_dir {
                if flags & FLAG_PRUNED != 0 {
                    child.unindexed.push(self.path_of(id));
                } else {
                    slots[i - first] = slot;
                }
            } else if !replaced {
                child.size.bytes += self.sizes[i];
                child.size.files += 1;
            }
        }
        overlay.add_files_under(dir, &mut out);
        Some(out)
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
        let results: Vec<Chunk> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (matching, next) = (&matching, &next);
                    scope.spawn(move || {
                        let mut out = Chunk::default();
                        let mut work = Work::new();
                        loop {
                            let start = next.fetch_add(CHUNK, Ordering::Relaxed);
                            if start >= total {
                                break;
                            }
                            let range = start..(start + CHUNK).min(total);
                            self.scan(s, range, matching, &mut work, &mut out);
                        }
                        out.top = work.top.into_iter().map(|Reverse(x)| x).collect();
                        out
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
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
        SearchResult { hits, matched }
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
            .map_err(|_| bad_format())?
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
            map: map.clone(),
            offset: layout.names,
            len: layout.names_len,
        };
        std::str::from_utf8(&map[layout.names..layout.names + layout.names_len])
            .map_err(|_| bad_format())?;
        let column = |offset: usize| (map.clone(), offset, layout.count);
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
            let s = std::str::from_utf8(take(len)?).map_err(|_| bad_format())?;
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
        let names_len = usize::try_from(u64_at(take(8)?)).map_err(|_| bad_format())?;
        let count = usize::try_from(u64_at(take(8)?)).map_err(|_| bad_format())?;
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
        self.owned().push(value);
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
            owned.push_str(s);
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

impl Work {
    fn new() -> Self {
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
#[derive(Default, Debug)]
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

    /// Drops records before `seq` (the new index includes them).
    pub fn prune_before(&mut self, seq: u64) {
        self.added.retain(|_, (_, s)| *s > seq);
        self.removed.retain(|_, s| *s > seq);
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
                String::from_utf8(b).map_err(|_| bad_format())?,
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

    /// Adds files recorded since the build to the subfolders of `dir` they are in (new
    /// subfolders get an entry). Files in folders the caller walks anyway are skipped.
    fn add_files_under(&self, dir: &Path, out: &mut Vec<IndexedDirSize>) {
        for (path, (seen, _)) in &self.added {
            if seen.is_dir {
                continue;
            }
            let Some(rel) = rel_components(path, dir).filter(|r| r.len() >= 2) else {
                continue;
            };
            if self.is_removed(path) {
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
        for (path, (info, _)) in &self.added {
            if (!show_hidden && info.hidden) || seen.contains(path) {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
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
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if dp
                    .score(Utf32Str::new(&parent, &mut buf), &mut matcher)
                    .is_none()
                {
                    continue;
                }
            }
            out.push(IndexHit {
                highlights: crate::filter::char_ranges_to_bytes(&name, &mut indices),
                path: path.clone(),
                is_dir: info.is_dir,
                size: info.size,
                modified: (info.modified != 0)
                    .then(|| UNIX_EPOCH + Duration::from_secs(info.modified)),
                score,
            });
        }
        out.sort_by_key(|h| Reverse(h.score));
        out.truncate(limit);
        out
    }
}

/// One subfolder's size from [`Index::child_dir_sizes`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexedDirSize {
    /// Folder name as indexed.
    pub name: String,
    /// Files below it that the index knows of (changes since the build applied).
    pub size: crate::dirsize::DirSize,
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

/// Whether the walk enters this folder: only plain folders. Not symlinks/junctions (cycles or
/// duplicates), not cloud placeholder folders (listing recalls from the cloud), not
/// hidden+system folders or the recycle bin.
pub fn descends_into(entry: &Entry) -> bool {
    entry.kind == EntryKind::Dir
        && !SKIP_DIRS.contains(&entry.sort_key.as_ref())
        && !entry.is_cloud_placeholder()
        && (!entry.is_hidden() || !entry.is_system())
}

/// Whether a watched path is recorded: inside the indexed roots and not in a skipped system
/// folder (e.g. a file moved to the recycle bin).
pub fn is_indexable(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|r| path.starts_with(r))
        && !path.components().any(|c| {
            let c = c.as_os_str().to_string_lossy().to_lowercase();
            SKIP_DIRS.contains(&c.as_str())
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
                },
                list,
            )
        };
        let j = journal.clone();
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
    #[allow(clippy::print_stdout)]
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
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Query speed over many items (`cargo test -p arke-core -- --ignored --nocapture
    /// index_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(clippy::print_stdout)]
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
        for text in ["rprt", "季報", "main_00012", "zzzz", "ext:rs size:>1000000"] {
            let t = std::time::Instant::now();
            let hits = index.find(&q(text), 50, false, threads);
            println!(
                "{text}: {} hits, {:?} ({threads} threads, {} items)",
                hits.len(),
                t.elapsed(),
                index.len()
            );
        }
    }

    /// Cost of the final ranking (`crate::rank`) on top of a query over 1M items, with a full
    /// visit history (`cargo test --release -p arke-core -- --ignored --nocapture rank_speed`).
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(clippy::print_stdout)]
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
            Some(snapshot.clone()),
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
                out.first()
                    .map(|h| h.path.display().to_string())
                    .unwrap_or_default()
            );
        }
    }

    /// Real disk (lists directories only, opens no files): `cargo test --release -p arke-core --
    /// --ignored --nocapture real_drive_index`
    #[test]
    #[ignore = "benchmark; run manually (walks the whole local drive)"]
    #[allow(clippy::print_stdout)]
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
                    .map(|h| h.path.display().to_string())
                    .unwrap_or_default()
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
