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
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.on_disk += other.on_disk;
        self.files += other.files;
        self.folders += other.folders;
        self.partial |= other.partial;
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

/// Walks everything below the folder `root` and counts it, calling `progress` after each folder
/// with what is known so far. Only lists folders (names, sizes, attributes), so files are never
/// opened and cloud placeholders are not downloaded. Links are not followed. `None` when
/// cancelled; an unreadable folder makes the result `partial`.
pub fn usage(
    fs: &dyn Fs,
    root: &Path,
    cluster: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Usage),
) -> Option<Usage> {
    crate::fs::debug_assert_not_ui_thread("usage", root);
    let mut total = Usage::default();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match fs.read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(e) => match e.kind {
                            EntryKind::File => {
                                total.bytes += e.size;
                                total.on_disk += file_on_disk(&e, cluster);
                                total.files += 1;
                            }
                            EntryKind::Dir => {
                                total.folders += 1;
                                stack.push(dir.join(&*e.name));
                            }
                            EntryKind::Link { .. } => {}
                        },
                        Err(_) => total.partial = true,
                    }
                }
            }
            Err(_) => total.partial = true,
        }
        progress(&total);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind};
    use crate::fake::FakeFs;

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
        let got = usage(&*fs, Path::new(r"C:\u"), 4096, &cancel, &mut |_| steps += 1).unwrap();
        assert_eq!(got.bytes, 1 + 9000 + 4096 + 4097);
        // 1 -> 4096, placeholder -> 0, 4096 -> 4096, 4097 -> 8192
        assert_eq!(got.on_disk, 4096 + 4096 + 8192);
        assert_eq!((got.files, got.folders), (4, 2));
        assert!(got.partial, "the unreadable folder makes it an undercount");
        assert_eq!(steps, 3, "progress after every folder");
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            usage(&*fs, Path::new(r"C:\u"), 4096, &cancel, &mut |_| {}),
            None
        );
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
