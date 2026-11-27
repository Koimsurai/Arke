//! Files dropped from the temp folder are taken before the drop returns (#168).
//!
//! An archive manager (7-Zip, NanaZip and others built on 7-Zip; WinRAR) can't know where a drop
//! lands, so it extracts what was dragged to a temp folder and lists those files. 7-Zip deletes
//! that folder as soon as the drop returns; it doesn't support a drop finishing later
//! (`IDataObjectAsyncCapability`). Arke's transfers run in the background after the drop, so
//! they found the files gone.
//!
//! So dropped files under the temp folder are hard-linked into a folder of Arke's own,
//! `%TEMP%\Arke drops\<n>`, before the drop returns: the same drive, so nothing is copied and
//! it's instant whatever the size. The transfer then moves them from there, and the source can
//! delete its own copies whenever it likes. Where links aren't possible (a drive without them),
//! the file is copied instead. Files elsewhere are left as they are.
//!
//! Folders emptied by finished transfers are removed on the next such drop.
//!
//! Virtual files (#206, `win_virtual_drop.rs`) are written to such a folder too, then moved.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use arke_core::path::is_same_or_inside;

/// Numbers the held-drop folders within this process.
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Takes dropped files that are in the temp folder (see the module docs). Returns the paths to
/// use instead, and whether any were taken (they then should be moved, not copied, from where
/// they are now).
pub(crate) fn hold_temp_drop(paths: Vec<PathBuf>) -> (Vec<PathBuf>, bool) {
    let temp = std::env::temp_dir();
    hold_in(paths, &temp, &drops_root(&temp))
}

/// `%TEMP%\Arke drops`.
fn drops_root(temp: &Path) -> PathBuf {
    temp.join("Arke drops")
}

/// A new, empty folder of Arke's own in the temp folder to hold a drop in. Clears the ones
/// finished transfers emptied.
pub(crate) fn new_drop_folder() -> std::io::Result<PathBuf> {
    let root = drops_root(&std::env::temp_dir());
    prune(&root);
    let dir = next_folder(&root);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn next_folder(root: &Path) -> PathBuf {
    root.join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn hold_in(paths: Vec<PathBuf>, temp: &Path, root: &Path) -> (Vec<PathBuf>, bool) {
    let volatile = |p: &Path| is_same_or_inside(p, temp) && !is_same_or_inside(p, root);
    if !paths.iter().any(|p| volatile(p)) {
        return (paths, false);
    }
    prune(root);
    let dir = next_folder(root);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("can't hold dropped temp files in {}: {e}", dir.display());
        return (paths, false);
    }
    let mut held_any = false;
    let paths = paths
        .into_iter()
        .map(|path| {
            let (true, Some(name)) = (volatile(&path), path.file_name()) else {
                return path;
            };
            let held = dir.join(name);
            let taken = link_tree(&path, &held).or_else(|_| {
                let _ = remove_tree(&held);
                copy_tree(&path, &held)
            });
            match taken {
                Ok(()) => {
                    held_any = true;
                    held
                }
                Err(e) => {
                    log::warn!("can't hold dropped {}: {e}", path.display());
                    path
                }
            }
        })
        .collect();
    (paths, held_any)
}

/// `from` (a file or a folder tree) hard-linked at `to`.
fn link_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(from)?.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            link_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::hard_link(from, to)
    }
}

/// `from` (a file or a folder tree) copied to `to`.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(from)?.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// How long another process's empty held-drop folder is left alone: it may be about to be
/// filled (a drop in another Arke, or a test run beside this one).
const OTHERS_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// Removes held-drop folders that finished transfers left empty (with empty folders inside).
/// Anything still holding files stays: its transfer may still run, or have failed. Another
/// process's folders go only once they are old, since an empty one may be a drop in progress.
fn prune(root: &Path) {
    fn remove_if_empty(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut empty = true;
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if !(is_dir && remove_if_empty(&path)) {
                empty = false;
            }
        }
        empty && std::fs::remove_dir(dir).is_ok()
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let own = format!("{}-", std::process::id());
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let ours = entry.file_name().to_string_lossy().starts_with(&own);
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > OTHERS_GRACE));
        if ours || old {
            remove_if_empty(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> (PathBuf, tempfile::TempDir) {
        let guard = tempfile::Builder::new()
            .prefix(&format!("arke-hold-test-{name}-"))
            .tempdir()
            .unwrap();
        (guard.path().to_path_buf(), guard)
    }

    /// Files and folders in the temp folder survive the source deleting them; files elsewhere
    /// are left alone.
    #[test]
    fn temp_files_are_held_and_others_left() {
        let (base, _guard) = scratch("held");
        let temp = base.join("temp");
        let root = temp.join("Arke drops");
        let extracted = temp.join("7zE1234");
        std::fs::create_dir_all(extracted.join("folder").join("deep")).unwrap();
        std::fs::write(extracted.join("a.txt"), b"alpha").unwrap();
        std::fs::write(extracted.join("folder").join("deep").join("b.txt"), b"beta").unwrap();
        let elsewhere = base.join("docs").join("c.txt");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, b"gamma").unwrap();

        let dropped = vec![
            extracted.join("a.txt"),
            extracted.join("folder"),
            elsewhere.clone(),
        ];
        let (held, any) = hold_in(dropped, &temp, &root);
        assert!(any);
        assert!(
            held[0].starts_with(&root) && held[1].starts_with(&root),
            "{held:?}"
        );
        assert_eq!(held[2], elsewhere, "not in the temp folder: as it was");
        // The source cleans up its temp folder as soon as the drop returns.
        std::fs::remove_dir_all(&extracted).unwrap();
        assert_eq!(std::fs::read(&held[0]).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(held[1].join("deep").join("b.txt")).unwrap(),
            b"beta"
        );

        // A transfer moved them away: the next held drop clears the empty folder.
        std::fs::remove_file(&held[0]).unwrap();
        std::fs::remove_dir_all(&held[1]).unwrap();
        let first = held[0].parent().unwrap().to_path_buf();
        std::fs::write(temp.join("d.txt"), b"delta").unwrap();
        let (again, _) = hold_in(vec![temp.join("d.txt")], &temp, &root);
        assert!(!first.exists(), "emptied folder removed");
        assert!(again[0].exists());
    }

    /// Nothing in the temp folder: nothing happens, not even the folder of Arke's own.
    #[test]
    fn drops_from_elsewhere_are_untouched() {
        let (base, _guard) = scratch("elsewhere");
        let temp = base.join("temp");
        let root = temp.join("Arke drops");
        let file = base.join("e.txt");
        std::fs::write(&file, b"x").unwrap();
        let (paths, any) = hold_in(vec![file.clone()], &temp, &root);
        assert_eq!(paths, [file]);
        assert!(!any);
        assert!(!root.exists());
    }
}
