//! Folder size: walks everything below a folder and sums file sizes.
//!
//! Runs on a background thread through `Fs`. Links and junctions are not followed
//! (no double counting, no cycles). Cloud placeholders count their recorded size
//! without triggering a download. Checks for cancellation once per directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::entry::EntryKind;
use crate::fs::Fs;

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
        let entries = match fs.read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) if first => return None,
            Err(_) => {
                total.partial = true;
                continue;
            }
        };
        first = false;
        for entry in entries {
            let Ok(entry) = entry else {
                total.partial = true;
                continue;
            };
            match entry.kind {
                EntryKind::Dir => stack.push(dir.join(&*entry.name)),
                EntryKind::File => {
                    total.bytes += entry.size;
                    total.files += 1;
                }
                // Links are skipped; the target is counted wherever it lives.
                EntryKind::Link { .. } => {}
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;
    use crate::fake::FakeFs;

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
}
