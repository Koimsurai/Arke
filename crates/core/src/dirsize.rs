//! Folder size: walks everything below a folder and sums file sizes.
//!
//! Runs on a background thread through `Fs` ([`Fs::scan_sizes`], which on the real file system
//! reads directories without building an entry per file). Links and junctions are not followed
//! (no double counting, no cycles). Cloud placeholders count their recorded size without
//! triggering a download. Checks for cancellation once per directory.

use std::ops::AddAssign;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::entry::{Entry, EntryKind};
use crate::fs::{Fs, SizeItem};

/// Size of one folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirSize {
    /// Total size of all files in bytes, recursively.
    pub bytes: u64,
    /// Number of files, recursively.
    pub files: u64,
    /// Some subfolder could not be read (access denied etc.), so `bytes` is an undercount.
    pub partial: bool,
}

impl AddAssign for DirSize {
    fn add_assign(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.files += other.files;
        self.partial |= other.partial;
    }
}

/// A subfolder's total found on the way by [`dir_size_tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubSize {
    /// The subfolder.
    pub path: PathBuf,
    /// Its modified time as its parent lists it (to tell later whether it changed).
    pub modified: Option<SystemTime>,
    /// Its total.
    pub size: DirSize,
}

type SubDir = (PathBuf, Option<SystemTime>);

/// Adds one directory's files to `total` and returns its subfolders. `None` if unreadable.
fn scan(fs: &dyn Fs, dir: &Path, total: &mut DirSize) -> Option<Vec<SubDir>> {
    let mut dirs = Vec::new();
    fs.scan_sizes(dir, &mut |item| match item {
        SizeItem::File(bytes) => {
            total.bytes += bytes;
            total.files += 1;
        }
        SizeItem::Dir { name, modified } => dirs.push((dir.join(name), modified)),
        SizeItem::Unreadable => total.partial = true,
        // Links are skipped; the target is counted wherever it lives.
        SizeItem::Link | SizeItem::Skip => {}
    })
    .ok()?;
    Some(dirs)
}

/// Total size of all files under `path`. `None` if cancelled or `path` itself is unreadable.
pub fn dir_size(fs: &dyn Fs, path: &Path, cancel: &AtomicBool) -> Option<DirSize> {
    crate::fs::debug_assert_not_ui_thread("dir_size", path);
    let mut total = DirSize::default();
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    let mut first = true;
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match scan(fs, &dir, &mut total) {
            Some(dirs) => stack.extend(dirs.into_iter().map(|(path, _)| path)),
            None if first => return None,
            None => total.partial = true,
        }
        first = false;
    }
    Some(total)
}

/// Like [`dir_size`], and also reports the totals of the subfolders up to `depth` levels below
/// `path` as each one finishes, so opening one of them later finds its subfolders already
/// computed. Walks exactly as much as [`dir_size`].
pub fn dir_size_tree(
    fs: &dyn Fs,
    path: &Path,
    depth: usize,
    cancel: &AtomicBool,
    found: &mut dyn FnMut(SubSize),
) -> Option<DirSize> {
    crate::fs::debug_assert_not_ui_thread("dir_size_tree", path);
    if depth == 0 {
        return dir_size(fs, path, cancel);
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let mut total = DirSize::default();
    let dirs = scan(fs, path, &mut total)?;
    for (dir, modified) in dirs {
        match dir_size_tree(fs, &dir, depth - 1, cancel, found) {
            Some(size) => {
                total += size;
                found(SubSize {
                    path: dir,
                    modified,
                    size,
                });
            }
            None if cancel.load(Ordering::Relaxed) => return None,
            None => total.partial = true,
        }
    }
    Some(total)
}

/// What a Properties window shows for folders: sizes and counts, found as the walk goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Total size of all files in bytes.
    pub bytes: u64,
    /// Space the files take on disk, estimated from the cluster size. Cloud placeholders that are
    /// not on this device take none.
    pub on_disk: u64,
    /// Files, recursively.
    pub files: u64,
    /// Folders below, recursively (not the folder itself).
    pub folders: u64,
    /// Something could not be read, so the numbers are an undercount.
    pub partial: bool,
    /// Bytes per [`Category`], in [`Category::ALL`] order.
    pub categories: [u64; CATEGORY_COUNT],
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.on_disk += other.on_disk;
        self.files += other.files;
        self.folders += other.folders;
        self.partial |= other.partial;
        for (mine, theirs) in self.categories.iter_mut().zip(other.categories) {
            *mine += theirs;
        }
    }
}

/// `size` rounded up to a whole number of `cluster`s (`size` itself when the cluster is unknown).
pub const fn round_to_clusters(size: u64, cluster: u64) -> u64 {
    if cluster == 0 {
        size
    } else {
        size.div_ceil(cluster).saturating_mul(cluster)
    }
}

/// Space one listed file takes on disk. Listings only; a cloud placeholder is never opened.
pub const fn file_on_disk(entry: &Entry, cluster: u64) -> u64 {
    if entry.is_cloud_placeholder() {
        0
    } else {
        round_to_clusters(entry.size, cluster)
    }
}

/// Where the space goes: what kind of thing a file is, by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Source code, project files and build output.
    Code,
    /// Video.
    Video,
    /// Pictures.
    Images,
    /// Documents, spreadsheets, slides, text.
    Documents,
    /// Audio.
    Audio,
    /// Archives and disk images.
    Archives,
    /// Everything else.
    Other,
}

/// Number of [`Category`] values.
pub const CATEGORY_COUNT: usize = 7;

impl Category {
    /// Every category, in the order [`Usage::categories`] lists them.
    pub const ALL: [Self; CATEGORY_COUNT] = [
        Self::Code,
        Self::Video,
        Self::Images,
        Self::Documents,
        Self::Audio,
        Self::Archives,
        Self::Other,
    ];

    /// Position in [`Category::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The category of a file with this extension (lowercase, no dot).
    pub fn of_extension(ext: &str) -> Self {
        use crate::filetype::{FileKind, kind_for_extension};
        // Build output and project files the file kinds do not know.
        if matches!(
            ext,
            "obj"
                | "o"
                | "pdb"
                | "class"
                | "jar"
                | "rlib"
                | "rmeta"
                | "lib"
                | "a"
                | "pyc"
                | "map"
                | "lock"
                | "ilk"
                | "exp"
                | "idb"
                | "d"
                | "gcno"
                | "gcda"
                | "wasm"
        ) {
            return Self::Code;
        }
        match kind_for_extension(ext) {
            FileKind::Code | FileKind::Executable => Self::Code,
            FileKind::Video => Self::Video,
            FileKind::Image => Self::Images,
            FileKind::Document
            | FileKind::Pdf
            | FileKind::Spreadsheet
            | FileKind::Presentation
            | FileKind::Text => Self::Documents,
            FileKind::Audio => Self::Audio,
            FileKind::Archive | FileKind::DiskImage => Self::Archives,
            _ => Self::Other,
        }
    }

    /// The category of a listed file.
    pub fn of(entry: &Entry) -> Self {
        Self::of_extension(entry.extension())
    }
}

/// What is known about a folder so far: the totals, and how much each folder directly inside it
/// holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// Totals, with the split by category.
    pub usage: Usage,
    /// The folders directly inside the one walked and what is in each, in listing order.
    pub children: Vec<(PathBuf, u64)>,
}

impl Space {
    /// The `n` biggest folders directly inside, biggest first; empty ones are left out.
    pub fn largest(&self, n: usize) -> Vec<(PathBuf, u64)> {
        let mut all: Vec<(PathBuf, u64)> = self
            .children
            .iter()
            .filter(|(_, bytes)| *bytes > 0)
            .cloned()
            .collect();
        all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(n);
        all
    }
}

/// Counts one listed file into `total`.
pub fn count_file(total: &mut Usage, entry: &Entry, cluster: u64) {
    total.bytes += entry.size;
    total.on_disk += file_on_disk(entry, cluster);
    total.files += 1;
    total.categories[Category::of(entry).index()] += entry.size;
}

/// Threads walking a folder at once for [`usage`]. Measured on a whole SSD (C:, 3.3 million
/// files): 182 s alone, 69 s with 4; more don't help (the file system and antivirus are the
/// limit then).
const WALKERS: usize = 4;

/// How often [`usage`] reports while its walkers work.
const PROGRESS_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// Walks everything below the folder `root` and counts it, calling `progress` now and then with
/// what is known so far. Only lists folders (names, sizes, attributes), so files are never opened
/// and cloud placeholders are not downloaded. Links are not followed. `None` when cancelled; an
/// unreadable folder makes the result `partial`.
///
/// [`WALKERS`] threads share the folders still to list; `progress` runs on the calling thread.
pub fn usage(
    fs: &dyn Fs,
    root: &Path,
    cluster: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Space),
) -> Option<Space> {
    crate::fs::debug_assert_not_ui_thread("usage", root);
    let mut space = Space::default();
    // The folder itself: its files count here, its folders are the children the rest is split by.
    let mut todo: Vec<(PathBuf, usize)> = Vec::new();
    match fs.read_dir(root) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(e) => match e.kind {
                        EntryKind::File => count_file(&mut space.usage, &e, cluster),
                        EntryKind::Dir => {
                            space.usage.folders += 1;
                            let path = root.join(&*e.name);
                            space.children.push((path.clone(), 0));
                            todo.push((path, space.children.len() - 1));
                        }
                        EntryKind::Link { .. } => {}
                    },
                    Err(_) => space.usage.partial = true,
                }
            }
        }
        Err(_) => space.usage.partial = true,
    }
    progress(&space);
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    if todo.is_empty() {
        return Some(space);
    }
    let walk = Walk {
        state: parking_lot::Mutex::new(WalkState {
            space,
            todo,
            busy: 0,
        }),
        work: parking_lot::Condvar::new(),
        finished: parking_lot::Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..WALKERS {
            scope.spawn(|| walker(fs, cluster, cancel, &walk));
        }
        let mut state = walk.state.lock();
        while !(cancel.load(Ordering::Relaxed) || (state.todo.is_empty() && state.busy == 0)) {
            walk.finished.wait_for(&mut state, PROGRESS_TICK);
            let snapshot = state.space.clone();
            drop(state);
            progress(&snapshot);
            state = walk.state.lock();
        }
        drop(state);
        walk.work.notify_all();
    });
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    Some(walk.state.into_inner().space)
}

/// What [`usage`]'s walkers share.
struct Walk {
    state: parking_lot::Mutex<WalkState>,
    /// More folders to list, or the walk is over.
    work: parking_lot::Condvar,
    /// Nothing left to list.
    finished: parking_lot::Condvar,
}

struct WalkState {
    space: Space,
    /// Folders still to list, each with the child of the root it is in.
    todo: Vec<(PathBuf, usize)>,
    /// Folders being listed right now.
    busy: usize,
}

/// One of [`usage`]'s threads: lists folders until there are none left (and none being listed
/// that could add more), adding what it finds to the shared totals.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the lock is taken back after each folder; waiting releases it"
)]
fn walker(fs: &dyn Fs, cluster: u64, cancel: &AtomicBool, walk: &Walk) {
    let mut state = walk.state.lock();
    loop {
        if cancel.load(Ordering::Relaxed) {
            walk.work.notify_all();
            walk.finished.notify_all();
            return;
        }
        let Some((dir, child)) = state.todo.pop() else {
            if state.busy == 0 {
                walk.work.notify_all();
                walk.finished.notify_all();
                return;
            }
            walk.work.wait(&mut state);
            continue;
        };
        state.busy += 1;
        drop(state);
        let mut usage = Usage::default();
        let mut bytes = 0;
        let mut found: Vec<(PathBuf, usize)> = Vec::new();
        match fs.read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(e) => match e.kind {
                            EntryKind::File => {
                                count_file(&mut usage, &e, cluster);
                                bytes += e.size;
                            }
                            EntryKind::Dir => {
                                usage.folders += 1;
                                found.push((dir.join(&*e.name), child));
                            }
                            EntryKind::Link { .. } => {}
                        },
                        Err(_) => usage.partial = true,
                    }
                }
            }
            Err(_) => usage.partial = true,
        }
        state = walk.state.lock();
        state.space.usage += usage;
        state.space.children[child].1 += bytes;
        let more = !found.is_empty();
        state.todo.extend(found);
        state.busy -= 1;
        if more {
            walk.work.notify_all();
        } else if state.todo.is_empty() && state.busy == 0 {
            walk.work.notify_all();
            walk.finished.notify_all();
        }
    }
}

/// Everything below `dir` counted as [`usage`] counts it, from a whole-volume listing of its
/// drive (the index service reads the NTFS MFT, as WizTree does: a whole C: drive in seconds
/// instead of a minute of walking). The listing is summed as it streams in, nothing kept but two
/// numbers per item and the names of folders. Errors when the listing can't be had (no service,
/// not NTFS) or `dir` isn't in it; `Interrupted` when cancelled. Background thread.
///
/// The listing is the search index's, which doesn't go into some folders
/// ([`crate::index::descends_into`]: hidden system folders such as `C:\Windows\Installer`, the
/// recycle bin): those are walked with `fs` afterwards (a few small folders). `Unsupported` when
/// `dir` is one of them or inside one: walk it instead.
///
/// The service leaves out what is inside other people's profile folders, which a walk can't read
/// either: a count over them is marked `partial`, as a walk's would be.
pub fn usage_from_listing(
    journal: &dyn crate::journal::Journal,
    fs: &dyn Fs,
    dir: &Path,
    cluster: u64,
    cancel: &AtomicBool,
) -> std::io::Result<Space> {
    use crate::journal::LISTED_ROOT;
    const OUT: u32 = u32::MAX;
    const TARGET: u32 = u32::MAX - 1;
    // How far down the way to `dir` each item is (its components matched), OUT once off it.
    const OFF: u16 = u16::MAX;
    let volume: PathBuf = dir.components().take(2).collect();
    let wanted: Vec<String> = dir
        .components()
        .skip(2)
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let mut level: Vec<u16> = Vec::new();
    // Per item: the child of `dir` it is in, TARGET for `dir` itself, OUT if not below it.
    let mut slot: Vec<u32> = Vec::new();
    let mut space = Space::default();
    let mut found = wanted.is_empty();
    // `dir` itself is a folder the listing doesn't go into.
    let mut target_unlisted = false;
    // Below `dir`: each folder's parent and name, for the paths of the folders to walk.
    let mut dirs: std::collections::HashMap<u32, (u32, String)> = std::collections::HashMap::new();
    let mut target_ix = LISTED_ROOT;
    // Folders below `dir` the listing doesn't go into: (listing index, slot).
    let mut unlisted: Vec<(u32, u32)> = Vec::new();
    let mut sink = |batch: Vec<crate::journal::Listed>| -> bool {
        for item in batch {
            let ix = u32::try_from(level.len()).unwrap_or(OUT);
            let (parent_level, parent_slot) = if item.parent == LISTED_ROOT {
                (0, if wanted.is_empty() { TARGET } else { OUT })
            } else {
                let p = item.parent as usize;
                (
                    level.get(p).copied().unwrap_or(OFF),
                    slot.get(p).copied().unwrap_or(OUT),
                )
            };
            let is_dir = item.attributes & 0x10 != 0 && !item.link;
            // On the way to `dir`: one more component matched, or off it.
            let mut here_level = OFF;
            let mut here_slot = OUT;
            if parent_slot == OUT {
                if parent_level != OFF
                    && let Some(name) = wanted.get(usize::from(parent_level))
                    && is_dir
                    && item.name.to_lowercase() == *name
                {
                    here_level = parent_level + 1;
                    if !crate::index::descends_into(&item.to_entry()) {
                        // The listing has nothing below: neither `dir` nor anything on the way.
                        target_unlisted = true;
                    }
                    if usize::from(here_level) == wanted.len() {
                        here_slot = TARGET;
                        target_ix = ix;
                        found = true;
                    }
                }
            } else if !item.link {
                // Below `dir`.
                if is_dir {
                    space.usage.folders += 1;
                    here_slot = if parent_slot == TARGET {
                        space.children.push((dir.join(&item.name), 0));
                        u32::try_from(space.children.len() - 1).unwrap_or(OUT)
                    } else {
                        parent_slot
                    };
                    if !crate::index::descends_into(&item.to_entry()) {
                        unlisted.push((ix, here_slot));
                    }
                    dirs.insert(ix, (item.parent, item.name));
                } else {
                    count_file(&mut space.usage, &item.to_entry(), cluster);
                    if parent_slot != TARGET
                        && let Some(child) = space.children.get_mut(parent_slot as usize)
                    {
                        child.1 += item.size;
                    }
                }
            }
            level.push(here_level);
            slot.push(here_slot);
        }
        !cancel.load(Ordering::Relaxed)
    };
    journal.list(&volume_root(&volume), &mut sink)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::ErrorKind::Interrupted.into());
    }
    if !found {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    if target_unlisted {
        return Err(std::io::ErrorKind::Unsupported.into());
    }
    let paths = unlisted
        .into_iter()
        .map(|(ix, slot)| (listed_path(dir, target_ix, &dirs, ix), slot));
    add_walks(fs, paths, cluster, cancel, &mut space)?;
    // Other people's profiles are listed without their contents.
    let profiles = volume_root(&volume).join("Users");
    if profiles.starts_with(dir) || dir == volume_root(&volume) {
        space.usage.partial = true;
    }
    Ok(space)
}

/// The path of listed folder `ix` below `dir` (listing index `dir_ix`), from each folder's parent
/// and name.
fn listed_path(
    dir: &Path,
    dir_ix: u32,
    dirs: &std::collections::HashMap<u32, (u32, String)>,
    ix: u32,
) -> PathBuf {
    let mut names = Vec::new();
    let mut at = ix;
    while at != dir_ix
        && let Some((parent, name)) = dirs.get(&at)
    {
        names.push(name.as_str());
        at = *parent;
    }
    names.iter().rev().fold(dir.to_path_buf(), |p, n| p.join(n))
}

/// Walks folders and adds them to `space`, each to its child of the folder counted (`slot`).
fn add_walks(
    fs: &dyn Fs,
    folders: impl Iterator<Item = (PathBuf, u32)>,
    cluster: u64,
    cancel: &AtomicBool,
    space: &mut Space,
) -> std::io::Result<()> {
    for (path, slot) in folders {
        let Some(walked) = usage(fs, &path, cluster, cancel, &mut |_| {}) else {
            return Err(std::io::ErrorKind::Interrupted.into());
        };
        if let Some(child) = space.children.get_mut(slot as usize) {
            child.1 += walked.usage.bytes;
        }
        space.usage += walked.usage;
    }
    Ok(())
}

/// `C:\` for `C:` (a drive prefix alone is the current folder on it, not its root).
fn volume_root(volume: &Path) -> PathBuf {
    let mut root = volume.to_path_buf();
    if root.components().count() == 1 {
        root.push(std::path::MAIN_SEPARATOR_STR);
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind};
    use crate::fake::FakeFs;

    /// A count from the drive's whole-volume listing (the file table) matches a walk: totals,
    /// categories, space on disk, folders, each folder inside, for the drive and a folder on it,
    /// whatever the letter case; links don't count, nor does anything outside the folder.
    #[test]
    fn usage_from_the_file_table_matches_a_walk() {
        use crate::journal::{FakeJournal, LISTED_ROOT, Listed};
        let item = |parent: u32, name: &str, dir: bool, size: u64| Listed {
            parent,
            name: name.into(),
            attributes: if dir { 0x10 } else { 0x20 },
            size,
            mtime: 0,
            link: false,
        };
        // C:\w\{a.mp4, docs\{x.pdf, old\y.txt}, src\main.rs, junction}, C:\other\z.bin
        let mut junction = item(0, "jl", true, 0);
        junction.link = true;
        // A hidden system folder (like C:\Windows\Installer): the listing doesn't go in.
        let mut hidden_system = item(4, "cache", true, 0);
        hidden_system.attributes |= 0x2 | 0x4;
        let listing = vec![
            item(LISTED_ROOT, "w", true, 0),     // 0
            item(LISTED_ROOT, "other", true, 0), // 1
            item(0, "a.mp4", false, 5000),       // 2
            item(0, "docs", true, 0),            // 3
            item(0, "src", true, 0),             // 4
            junction,                            // 5
            item(1, "z.bin", false, 77),         // 6
            item(3, "x.pdf", false, 300),        // 7
            item(3, "old", true, 0),             // 8
            item(4, "main.rs", false, 40),       // 9
            item(8, "y.txt", false, 9000),       // 10
            hidden_system,                       // 11
        ];
        let journal = FakeJournal::new(1);
        journal.set_listing(r"C:\", listing);
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\", vec![dir("w"), dir("other")]);
        fs.insert_dir(
            r"C:\w",
            vec![
                file("a.mp4", 5000),
                dir("docs"),
                dir("src"),
                Entry::new("jl", EntryKind::Link { is_dir: true }, 0, None, 0),
            ],
        );
        fs.insert_dir(r"C:\w\docs", vec![file("x.pdf", 300), dir("old")]);
        fs.insert_dir(r"C:\w\docs\old", vec![file("y.txt", 9000)]);
        fs.insert_dir(
            r"C:\w\src",
            vec![
                file("main.rs", 40),
                Entry::new("cache", EntryKind::Dir, 0, None, 0x2 | 0x4),
            ],
        );
        fs.insert_dir(r"C:\w\src\cache", vec![file("big.msi", 6000), dir("in")]);
        fs.insert_dir(r"C:\w\src\cache\in", vec![file("p.msp", 700)]);
        fs.insert_dir(r"C:\other", vec![file("z.bin", 77)]);
        let cancel = AtomicBool::new(false);
        for dir in [r"C:\w", r"C:\"] {
            let walked = usage(&*fs, Path::new(dir), 4096, &cancel, &mut |_| {}).unwrap();
            let mut listed =
                usage_from_listing(&*journal, &*fs, Path::new(dir), 4096, &cancel).unwrap();
            // The drive is marked partial for other people's profiles; the rest is the same.
            listed.usage.partial = walked.usage.partial;
            assert_eq!(listed, walked, "{dir}");
        }
        let upper =
            usage_from_listing(&*journal, &*fs, Path::new(r"C:\W\DOCS"), 4096, &cancel).unwrap();
        assert_eq!(upper.usage.bytes, 9300);
        assert_eq!(
            upper.children,
            vec![(PathBuf::from(r"C:\W\DOCS\old"), 9000)]
        );
        let inside =
            usage_from_listing(&*journal, &*fs, Path::new(r"C:\w\src\cache"), 4096, &cancel);
        assert_eq!(inside.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
        let missing = usage_from_listing(&*journal, &*fs, Path::new(r"C:\nope"), 4096, &cancel);
        assert_eq!(missing.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        cancel.store(true, Ordering::Relaxed);
        let stopped = usage_from_listing(&*journal, &*fs, Path::new(r"C:\w"), 4096, &cancel);
        assert_eq!(stopped.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn usage_counts_files_folders_and_space_on_disk() {
        let fs = FakeFs::new();
        let placeholder = 0x40_0000;
        fs.insert_dir(
            r"C:\u",
            vec![
                dir("a"),
                file("small", 1),
                Entry::new("cloud", EntryKind::File, 9000, None, placeholder),
            ],
        );
        fs.insert_dir(
            r"C:\u\a",
            vec![file("x", 4096), file("y", 4097), dir("gone")],
        );
        let cancel = AtomicBool::new(false);
        let mut steps = 0;
        let got = usage(&*fs, Path::new(r"C:\u"), 4096, &cancel, &mut |_| steps += 1)
            .unwrap()
            .usage;
        assert_eq!(got.bytes, 1 + 9000 + 4096 + 4097);
        // 1 -> 4096, placeholder -> 0, 4096 -> 4096, 4097 -> 8192
        assert_eq!(got.on_disk, 4096 + 4096 + 8192);
        assert_eq!((got.files, got.folders), (4, 2));
        assert!(got.partial, "the unreadable folder makes it an undercount");
        assert!(steps >= 1, "progress as it goes");
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            usage(&*fs, Path::new(r"C:\u"), 4096, &cancel, &mut |_| {}),
            None
        );
    }

    #[test]
    fn files_are_sorted_into_categories_by_extension() {
        let of = Category::of_extension;
        assert_eq!(of("rs"), Category::Code);
        assert_eq!(of("exe"), Category::Code);
        assert_eq!(of("pdb"), Category::Code, "build output counts as code");
        assert_eq!(of("mp4"), Category::Video);
        assert_eq!(of("jpg"), Category::Images);
        assert_eq!(of("docx"), Category::Documents);
        assert_eq!(of("pdf"), Category::Documents);
        assert_eq!(of("txt"), Category::Documents);
        assert_eq!(of("flac"), Category::Audio);
        assert_eq!(of("zip"), Category::Archives);
        assert_eq!(of("iso"), Category::Archives);
        assert_eq!(of("xyz"), Category::Other);
        assert_eq!(of(""), Category::Other);
        for (ix, category) in Category::ALL.into_iter().enumerate() {
            assert_eq!(category.index(), ix);
        }
    }

    /// The space bar's data: bytes per category, and the folders directly inside with everything
    /// below each (files directly in the root belong to none of them); the walk reports as it
    /// goes.
    #[test]
    fn space_splits_by_category_and_by_folder() {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\s",
            vec![dir("big"), dir("small"), dir("empty"), file("loose.txt", 7)],
        );
        fs.insert_dir(r"C:\s\big", vec![dir("deep"), file("a.mp4", 1000)]);
        fs.insert_dir(r"C:\s\big\deep", vec![file("b.jpg", 500), file("c.rs", 20)]);
        fs.insert_dir(r"C:\s\small", vec![file("d.zip", 30)]);
        fs.insert_dir(r"C:\s\empty", vec![]);
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let space = usage(&*fs, Path::new(r"C:\s"), 0, &cancel, &mut |s| {
            seen.push(s.usage.bytes);
        })
        .unwrap();
        let cat = |c: Category| space.usage.categories[c.index()];
        assert_eq!(cat(Category::Video), 1000);
        assert_eq!(cat(Category::Images), 500);
        assert_eq!(cat(Category::Code), 20);
        assert_eq!(cat(Category::Archives), 30);
        assert_eq!(cat(Category::Documents), 7);
        assert_eq!(
            space.usage.categories.iter().sum::<u64>(),
            space.usage.bytes
        );
        assert_eq!(
            space.largest(3),
            vec![
                (PathBuf::from(r"C:\s\big"), 1520),
                (PathBuf::from(r"C:\s\small"), 30),
            ],
            "folders with nothing in them are left out"
        );
        assert_eq!(space.largest(1).len(), 1);
        // Reported as it goes, never going back.
        assert!(seen.windows(2).all(|w| w[0] <= w[1]) && !seen.is_empty());
    }

    #[test]
    fn clusters_round_up() {
        assert_eq!(round_to_clusters(0, 4096), 0);
        assert_eq!(round_to_clusters(1, 4096), 4096);
        assert_eq!(round_to_clusters(8192, 4096), 8192);
        assert_eq!(round_to_clusters(5, 0), 5);
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry::new(name, EntryKind::File, size, None, 0)
    }

    fn dir(name: &str) -> Entry {
        Entry::new(name, EntryKind::Dir, 0, None, 0)
    }

    #[test]
    fn sums_nested_files_and_skips_links() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\s", vec![dir("a"), file("top.txt", 10)]);
        fs.insert_dir(r"C:\s\a", vec![dir("b"), file("x", 100)]);
        fs.insert_dir(r"C:\s\a\b", vec![file("y", 1000), file("z", 1)]);
        let cancel = AtomicBool::new(false);
        let size = dir_size(&*fs, Path::new(r"C:\s"), &cancel).unwrap();
        assert_eq!((size.bytes, size.files, size.partial), (1111, 4, false));

        // Unreadable subfolder: marked partial, the rest still counted.
        fs.insert_dir(r"C:\t", vec![dir("gone"), file("k", 5)]);
        let size = dir_size(&*fs, Path::new(r"C:\t"), &cancel).unwrap();
        assert_eq!((size.bytes, size.partial), (5, true));

        assert_eq!(dir_size(&*fs, Path::new(r"C:\nope"), &cancel), None);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(dir_size(&*fs, Path::new(r"C:\s"), &cancel), None, "cancel");
    }

    #[test]
    fn tree_walk_reports_subfolders_on_the_way() {
        let fs = FakeFs::new();
        let at = |s| Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s));
        fs.insert_dir(
            r"C:\s",
            vec![Entry::new("a", EntryKind::Dir, 0, at(1), 0), file("top", 1)],
        );
        fs.insert_dir(
            r"C:\s\a",
            vec![
                Entry::new("b", EntryKind::Dir, 0, at(2), 0),
                dir("gone"),
                file("x", 10),
            ],
        );
        fs.insert_dir(r"C:\s\a\b", vec![dir("c"), file("y", 100)]);
        fs.insert_dir(r"C:\s\a\b\c", vec![file("z", 1000)]);
        let cancel = AtomicBool::new(false);
        let mut found = Vec::new();
        let total =
            dir_size_tree(&*fs, Path::new(r"C:\s"), 2, &cancel, &mut |s| found.push(s)).unwrap();
        assert_eq!(total, dir_size(&*fs, Path::new(r"C:\s"), &cancel).unwrap());
        assert_eq!((total.bytes, total.partial), (1111, true));
        let sizes: Vec<(&Path, Option<SystemTime>, u64)> = found
            .iter()
            .map(|s| (s.path.as_path(), s.modified, s.size.bytes))
            .collect();
        // Two levels below C:\s ("c" is three down); "gone" can't be read.
        assert_eq!(
            sizes,
            [
                (Path::new(r"C:\s\a\b"), at(2), 1100),
                (Path::new(r"C:\s\a"), at(1), 1110),
            ]
        );
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            dir_size_tree(&*fs, Path::new(r"C:\s"), 2, &cancel, &mut |_| {}),
            None
        );
    }

    /// The real file system's own directory read counts the same as listing entries, and reports
    /// subfolder times the way a listing shows them.
    #[test]
    fn real_fs_walk_matches_listing() {
        let root = std::env::temp_dir().join(format!("arke-dirsize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"a\b")).unwrap();
        std::fs::write(root.join("one"), [0u8; 10]).unwrap();
        std::fs::write(root.join(r"a\two"), [0u8; 200]).unwrap();
        std::fs::write(root.join(r"a\b\three"), [0u8; 3000]).unwrap();
        let cancel = AtomicBool::new(false);
        let real = crate::fs::RealFs;
        let size = dir_size(&real, &root, &cancel).unwrap();
        assert_eq!((size.bytes, size.files, size.partial), (3210, 3, false));
        let mut found = Vec::new();
        dir_size_tree(&real, &root, 1, &cancel, &mut |s| found.push(s)).unwrap();
        let listed = real.stat(&root.join("a")).unwrap().modified;
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].size.bytes, found[0].modified), (3200, listed));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
