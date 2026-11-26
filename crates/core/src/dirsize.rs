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

/// `FILE_ATTRIBUTE_SPARSE_FILE` and `FILE_ATTRIBUTE_COMPRESSED`: files whose size on disk is not
/// their size rounded up to clusters.
const ATTR_SPARSE_OR_COMPRESSED: u32 = 0x200 | 0x800;

/// Counts one listed file into `total`.
pub fn count_file(total: &mut Usage, entry: &Entry, cluster: u64) {
    count_with(total, entry, file_on_disk(entry, cluster));
}

/// [`count_file`] for a file listed in `dir`. Sparse and NTFS-compressed files take less than
/// their size rounded up to clusters, so their real allocation is asked from the file system (one
/// call each, and only those few files); every other file keeps the cluster estimate.
pub fn count_file_in(total: &mut Usage, entry: &Entry, dir: &Path, cluster: u64) {
    let mut on_disk = file_on_disk(entry, cluster);
    if entry.attributes & ATTR_SPARSE_OR_COMPRESSED != 0 && !entry.is_cloud_placeholder() {
        on_disk = real_on_disk(&dir.join(&*entry.name)).unwrap_or(on_disk);
    }
    count_with(total, entry, on_disk);
}

#[cfg(windows)]
fn real_on_disk(path: &Path) -> Option<u64> {
    crate::winfs::allocated_size(path)
}

#[cfg(not(windows))]
fn real_on_disk(_path: &Path) -> Option<u64> {
    None
}

fn count_with(total: &mut Usage, entry: &Entry, on_disk: u64) {
    total.bytes += entry.size;
    total.on_disk += on_disk;
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
                        EntryKind::File => count_file_in(&mut space.usage, &e, root, cluster),
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
            walkers: WALKERS,
        }),
        work: parking_lot::Condvar::new(),
        finished: parking_lot::Condvar::new(),
    };
    std::thread::scope(|scope| {
        let mut walkers = Vec::with_capacity(WALKERS);
        for _ in 0..WALKERS {
            walkers.push(scope.spawn(|| walker(fs, cluster, cancel, &walk)));
        }
        let mut state = walk.state.lock();
        while !(cancel.load(Ordering::Relaxed)
            || (state.todo.is_empty() && state.busy == 0)
            || state.walkers == 0)
        {
            walk.finished.wait_for(&mut state, PROGRESS_TICK);
            let snapshot = state.space.clone();
            drop(state);
            progress(&snapshot);
            state = walk.state.lock();
        }
        // No walker left with folders still to list (each one panicked): those weren't counted.
        if !state.todo.is_empty() {
            state.space.usage.partial = true;
        }
        drop(state);
        walk.work.notify_all();
        for walker in walkers {
            // Joined here so a walker's panic doesn't take the count down with it; its folder
            // is already counted as unread (`Leaving`).
            if walker.join().is_err() {
                log::warn!("a folder size walker panicked");
            }
        }
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
    /// Walkers still running.
    walkers: usize,
}

/// Keeps [`WalkState`] true when a walker ends, also by a panic (a bug, a file system driver's):
/// the folder it was listing is no longer busy and counts as unread, and once no walker is left
/// [`usage`] stops waiting for them.
struct Leaving<'a> {
    walk: &'a Walk,
    /// Between taking a folder and adding what was in it.
    listing: bool,
}

impl Drop for Leaving<'_> {
    fn drop(&mut self) {
        let mut state = self.walk.state.lock();
        if self.listing {
            state.busy -= 1;
            state.space.usage.partial = true;
        }
        state.walkers -= 1;
        drop(state);
        self.walk.work.notify_all();
        self.walk.finished.notify_all();
    }
}

/// One of [`usage`]'s threads: lists folders until there are none left (and none being listed
/// that could add more), adding what it finds to the shared totals.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the lock is taken back after each folder; waiting releases it"
)]
fn walker(fs: &dyn Fs, cluster: u64, cancel: &AtomicBool, walk: &Walk) {
    let mut leaving = Leaving {
        walk,
        listing: false,
    };
    let mut state = walk.state.lock();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some((dir, child)) = state.todo.pop() else {
            if state.busy == 0 {
                return;
            }
            walk.work.wait(&mut state);
            continue;
        };
        state.busy += 1;
        leaving.listing = true;
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
                                count_file_in(&mut usage, &e, &dir, cluster);
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
        leaving.listing = false;
        if more {
            walk.work.notify_all();
        } else if state.todo.is_empty() && state.busy == 0 {
            walk.work.notify_all();
            walk.finished.notify_all();
        }
    }
}

/// Where user profiles live and which one is the user's. The index service lists the other
/// profile folders without their contents (privacy), `C:\Users\Default` too, which anyone can
/// read: counts from its listing walk those instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profiles {
    /// Where all profiles live (`C:\Users`).
    pub dir: PathBuf,
    /// The user's own profile.
    pub mine: Option<PathBuf>,
}

impl Profiles {
    /// Whether the service lists the profile folder `name` (directly in [`Self::dir`]) without
    /// its contents: every one but the user's own and Public.
    pub fn hides(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        name != "public"
            && !self.mine.as_deref().is_some_and(|mine| {
                mine.file_name()
                    .is_some_and(|m| m.to_string_lossy().to_lowercase() == name)
                    && mine
                        .parent()
                        .and_then(|p| below(p, &self.dir))
                        .is_some_and(|rest| rest.is_empty())
            })
    }

    /// Whether `path` is a profile folder the service lists without contents, or is inside one.
    pub fn hidden(&self, path: &Path) -> bool {
        below(path, &self.dir)
            .and_then(|rest| rest.first().map(|owner| self.hides(owner)))
            .unwrap_or(false)
    }

    /// The names on the way from `path` down to [`Self::dir`] (lowercase; none when it is
    /// `path`), when `path` is it or above it: a count there holds the hidden profiles.
    pub fn way_down(&self, path: &Path) -> Option<Vec<String>> {
        below(&self.dir, path)
    }
}

/// The components of `path` below `base` (lowercase), whatever the letter case of either;
/// `None` when it isn't `base` or below it.
fn below(path: &Path, base: &Path) -> Option<Vec<String>> {
    let mut parts = path.components();
    for want in base.components() {
        let got = parts.next()?;
        let (got, want) = (got.as_os_str(), want.as_os_str());
        if !got.eq_ignore_ascii_case(want)
            && got.to_string_lossy().to_lowercase() != want.to_string_lossy().to_lowercase()
        {
            return None;
        }
    }
    Some(
        parts
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect(),
    )
}

/// Why [`usage_from_listing`] couldn't count a folder after reading the whole listing.
#[derive(Debug)]
struct ReadToEnd(&'static str);

impl std::fmt::Display for ReadToEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ReadToEnd {}

/// Whether [`usage_from_listing`] failed only after reading the whole listing (`dir` isn't in
/// it, or the listing doesn't go into it), so it took as long as reading the file table takes.
pub fn listed_to_end(err: &std::io::Error) -> bool {
    matches!(err.get_ref(), Some(e) if e.is::<ReadToEnd>())
}

/// Everything below `dir` counted as [`usage`] counts it, from a whole-volume listing of its
/// drive (the index service reads the NTFS MFT, as WizTree does: a whole C: drive in seconds
/// instead of a minute of walking). The listing is summed as it streams in, nothing kept but two
/// numbers per item and the names of folders. Errors when the listing can't be had (no service,
/// not NTFS) or `dir` isn't in it (`NotFound`, as through a junction); `Interrupted` when
/// cancelled. Background thread.
///
/// The listing is the search index's, which doesn't go into some folders
/// ([`crate::index::descends_into`]: hidden system folders such as `C:\Windows\Installer`, the
/// recycle bin): those are walked with `fs` afterwards (a few small folders). `Unsupported` when
/// `dir` is one of them or inside one: walk it instead. Both after the whole listing
/// ([`listed_to_end`]).
///
/// The service lists other people's profile folders (`profiles`) without their contents: those
/// below `dir` are walked too, so what the user can read counts (`C:\Users\Default`) and the rest
/// makes the count `partial`, as a walk would. `PermissionDenied` at once when `dir` is in one
/// of them: walk it instead.
#[allow(
    clippy::too_many_lines,
    reason = "one pass over the streamed listing, its state shared by the steps"
)]
pub fn usage_from_listing(
    journal: &dyn crate::journal::Journal,
    fs: &dyn Fs,
    dir: &Path,
    cluster: u64,
    profiles: Option<&Profiles>,
    cancel: &AtomicBool,
) -> std::io::Result<Space> {
    use crate::journal::LISTED_ROOT;
    const OUT: u32 = u32::MAX;
    const TARGET: u32 = u32::MAX - 1;
    // How far down the way to `dir` each item is (its components matched), OFF once off it.
    // Below `dir` the same for the way from `dir` to the profiles folder.
    const OFF: u16 = u16::MAX;
    if profiles.is_some_and(|p| p.hidden(dir)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the file table is listed without what is in other people's profiles",
        ));
    }
    let way_down = profiles.and_then(|p| p.way_down(dir).map(|way| (p, way)));
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
                        // Below it, the way to the profiles folder starts here.
                        here_level = 0;
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
                    let descends = crate::index::descends_into(&item.to_entry());
                    if !descends {
                        unlisted.push((ix, here_slot));
                    }
                    // On the way to the profiles folder, or a profile in it listed without
                    // contents: walked like the folders the listing doesn't go into.
                    if let Some((profiles, way)) = &way_down
                        && parent_level != OFF
                    {
                        if let Some(next) = way.get(usize::from(parent_level)) {
                            if item.name.to_lowercase() == *next {
                                here_level = parent_level + 1;
                            }
                        } else if descends && profiles.hides(&item.name) {
                            unlisted.push((ix, here_slot));
                        }
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            ReadToEnd("not in the file table listing"),
        ));
    }
    if target_unlisted {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            ReadToEnd("the file table listing doesn't go into it"),
        ));
    }
    let paths = unlisted
        .into_iter()
        .map(|(ix, slot)| (listed_path(dir, target_ix, &dirs, ix), slot));
    add_walks(fs, paths, cluster, cancel, &mut space)?;
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
            let listed =
                usage_from_listing(&*journal, &*fs, Path::new(dir), 4096, None, &cancel).unwrap();
            assert_eq!(listed, walked, "{dir}");
        }
        let upper = usage_from_listing(
            &*journal,
            &*fs,
            Path::new(r"C:\W\DOCS"),
            4096,
            None,
            &cancel,
        )
        .unwrap();
        assert_eq!(upper.usage.bytes, 9300);
        assert_eq!(
            upper.children,
            vec![(PathBuf::from(r"C:\W\DOCS\old"), 9000)]
        );
        // Both known only once the whole listing is read: it took as long as the table takes.
        let inside = usage_from_listing(
            &*journal,
            &*fs,
            Path::new(r"C:\w\src\cache"),
            4096,
            None,
            &cancel,
        )
        .unwrap_err();
        assert_eq!(inside.kind(), std::io::ErrorKind::Unsupported);
        assert!(listed_to_end(&inside));
        let missing =
            usage_from_listing(&*journal, &*fs, Path::new(r"C:\nope"), 4096, None, &cancel)
                .unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        assert!(listed_to_end(&missing));
        cancel.store(true, Ordering::Relaxed);
        let stopped = usage_from_listing(&*journal, &*fs, Path::new(r"C:\w"), 4096, None, &cancel)
            .unwrap_err();
        assert_eq!(stopped.kind(), std::io::ErrorKind::Interrupted);
        assert!(!listed_to_end(&stopped));
    }

    /// The service lists other people's profile folders without contents (`Default` too): a
    /// count of the profiles folder or the drive walks them, so what can be read counts and the
    /// rest makes it "at least"; a count inside one is left to a walk. Any letter case.
    #[test]
    fn hidden_profiles_are_walked_from_the_file_table() {
        use crate::journal::{FakeJournal, LISTED_ROOT, Listed};
        let item = |parent: u32, name: &str, dir: bool, size: u64| Listed {
            parent,
            name: name.into(),
            attributes: if dir { 0x10 } else { 0x20 },
            size,
            mtime: 0,
            link: false,
        };
        let journal = FakeJournal::new(1);
        journal.set_listing(
            r"C:\",
            vec![
                item(LISTED_ROOT, "Users", true, 0), // 0
                item(0, "me", true, 0),              // 1
                item(0, "Public", true, 0),          // 2
                item(0, "Default", true, 0),         // 3
                item(0, "Bob", true, 0),             // 4
                item(1, "mine.txt", false, 10),      // 5
                item(2, "shared.txt", false, 20),    // 6
            ],
        );
        let fs = FakeFs::new();
        // Default can be read; Bob's folder can't (not in the fake: listing it fails).
        fs.insert_dir(
            r"C:\Users\Default",
            vec![dir("AppData"), file("NTUSER.DAT", 300)],
        );
        fs.insert_dir(r"C:\Users\Default\AppData", vec![file("x", 4000)]);
        // Walks go below `dir` as it is spelled (Windows doesn't mind the case; the fake does).
        fs.insert_dir(
            r"c:\users\Default",
            vec![dir("AppData"), file("NTUSER.DAT", 300)],
        );
        fs.insert_dir(r"c:\users\Default\AppData", vec![file("x", 4000)]);
        let profiles = Profiles {
            dir: PathBuf::from(r"C:\Users"),
            mine: Some(PathBuf::from(r"C:\Users\me")),
        };
        let cancel = AtomicBool::new(false);
        let count = |dir: &str| {
            usage_from_listing(&*journal, &*fs, Path::new(dir), 0, Some(&profiles), &cancel)
        };
        for dir in [r"C:\", r"C:\Users", r"c:\users"] {
            let space = count(dir).unwrap();
            assert_eq!(space.usage.bytes, 10 + 20 + 300 + 4000, "{dir}");
            assert!(space.usage.partial, "{dir}: Bob's can't be read");
        }
        let users = count(r"C:\Users").unwrap();
        assert!(
            users
                .children
                .contains(&(PathBuf::from(r"C:\Users\Default"), 4300)),
            "{:?}",
            users.children
        );
        let mine = count(r"C:\Users\me").unwrap();
        assert_eq!((mine.usage.bytes, mine.usage.partial), (10, false));
        let lists = journal.list_calls();
        for dir in [r"C:\Users\Default", r"C:\users\bob\x"] {
            let err = count(dir).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{dir}");
            assert!(!listed_to_end(&err));
        }
        assert_eq!(journal.list_calls(), lists, "not read for those");
        assert!(!profiles.hides("PUBLIC") && !profiles.hides("Me") && profiles.hides("default"));
        assert!(!profiles.hidden(Path::new(r"C:\Users")));
        assert!(profiles.hidden(Path::new(r"C:\USERS\Bob")));
        assert_eq!(
            profiles.way_down(Path::new(r"c:\")),
            Some(vec!["users".to_owned()])
        );
        assert_eq!(profiles.way_down(Path::new(r"C:\Users\me")), None);
        assert_eq!(profiles.way_down(Path::new(r"D:\")), None);
    }

    /// A walker that panics (a bug, a driver's) doesn't hang the count nor take it down: what
    /// it was listing counts as unread, the rest is counted; even when every walker panics.
    #[test]
    fn a_panicking_walker_leaves_a_partial_count() {
        let fs = FakeFs::new();
        fs.insert_dir(r"C:\q", vec![dir("bad"), dir("good")]);
        fs.insert_dir(r"C:\q\good", vec![file("g", 5)]);
        fs.crash(r"C:\q\bad");
        let cancel = AtomicBool::new(false);
        let space = usage(&*fs, Path::new(r"C:\q"), 0, &cancel, &mut |_| {}).unwrap();
        assert_eq!((space.usage.bytes, space.usage.partial), (5, true));

        let names: Vec<String> = (0..WALKERS + 2).map(|i| format!("d{i}")).collect();
        fs.insert_dir(r"C:\all", names.iter().map(|n| dir(n.as_str())).collect());
        for name in &names {
            fs.crash(Path::new(r"C:\all").join(name));
        }
        let space = usage(&*fs, Path::new(r"C:\all"), 0, &cancel, &mut |_| {}).unwrap();
        assert!(space.usage.partial);
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

    #[test]
    fn sparse_and_compressed_files_fall_back_to_the_estimate() {
        let missing = Path::new(r"Z:\arke-no-such-folder");
        let mut usage = Usage::default();
        // Flagged, but the real size can't be read: rounded up to clusters as before.
        let sparse = Entry::new("a", EntryKind::File, 5000, None, 0x200);
        count_file_in(&mut usage, &sparse, missing, 4096);
        assert_eq!(usage.on_disk, 8192);
        // Not flagged: never asks the file system.
        count_file_in(&mut usage, &file("b", 10), missing, 4096);
        assert_eq!(usage.on_disk, 8192 + 4096);
        assert_eq!(usage.bytes, 5010);
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
        let tmp_dir = tempfile::tempdir().unwrap();
        let root = tmp_dir.path().to_path_buf();
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
    }
}
