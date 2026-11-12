//! Permanent delete (Shift+Delete, already confirmed by Arke), done with a custom walk
//! instead of the Windows copy engine.
//!
//! `IFileOperation` fails on trees full of links, such as pnpm's `node_modules` junctions:
//! it shows "Could not find this item" for dangling links, and after Skip the parent folder
//! can't be removed and the whole operation counts as cancelled. Here:
//! - Symlinks and junctions are removed as links; their targets are never followed.
//! - Missing items count as deleted.
//! - Items that can't be deleted (in use, access denied) are collected and reported at the
//!   end while the rest continues.
//! - Read-only files are made writable first, as Explorer does.
//! - Paths are used in their `\\?\` form, so long paths and names Windows normally mangles (a
//!   trailing period or space, `con`, `nul`) are deleted too instead of "not found".
//! - Cancel is checked before each item; progress goes to `OpProgress`.
//!
//! Call from a background thread.

use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::shell::{ItemFailure, OpOutcome, OpProgress, ShellError};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Deletes the trees at `paths`. Failures are reported as an error listing the items and why
/// (with the programs holding in-use files); everything else is still deleted.
pub(crate) fn purge(paths: &[PathBuf], progress: &OpProgress) -> Result<OpOutcome, ShellError> {
    let mut failed: Vec<(PathBuf, io::Error)> = Vec::new();
    for path in paths {
        if remove_tree(&arke_core::path::verbatim(path), progress, &mut failed) {
            return Ok(OpOutcome::Cancelled);
        }
    }
    if failed.is_empty() {
        return Ok(OpOutcome::Done);
    }
    let mut failed: Vec<ItemFailure> = failed
        .into_iter()
        .map(|(path, e)| ItemFailure::io(arke_core::path::unverbatim(&path), &e))
        .collect();
    crate::win_fileops::explain(&mut failed);
    Err(ShellError::Items { failed })
}

/// Deletes `path` and everything below it. Returns true if cancelled.
fn remove_tree(path: &Path, progress: &OpProgress, failed: &mut Vec<(PathBuf, io::Error)>) -> bool {
    if progress.cancel.load(Ordering::Relaxed) {
        return true;
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // Already gone (e.g. a link target deleted earlier): treat as deleted.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(e) => {
            failed.push((path.to_path_buf(), e));
            return false;
        }
    };
    let result = if meta.file_type().is_symlink() {
        // Symlink or junction: remove the link only (directory links need remove_dir).
        if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    } else if meta.is_dir() {
        match fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if remove_tree(&entry.path(), progress, failed) {
                        return true;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
            Err(e) => {
                failed.push((path.to_path_buf(), e));
                return false;
            }
        }
        remove_writable(path, &meta, |p| fs::remove_dir(p))
    } else {
        remove_writable(path, &meta, |p| fs::remove_file(p))
    };
    match result {
        Ok(()) => {
            progress.done.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        // A folder that failed because of a child already recorded: don't report it twice.
        Err(_) if meta.is_dir() && failed.iter().any(|(p, _)| p.starts_with(path)) => {}
        Err(e) => failed.push((path.to_path_buf(), e)),
    }
    false
}

/// Removes `path`, clearing the read-only attribute and retrying if needed.
fn remove_writable(
    path: &Path,
    meta: &fs::Metadata,
    remove: fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match remove(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && meta.permissions().readonly() => {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(path, perms)?;
            remove(path)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = crate::win::long_temp_dir()
                .join(format!("arke-purge-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            // Verbatim, in case a failed test left names only `\\?\` paths reach.
            let _ = fs::remove_dir_all(arke_core::path::verbatim(&self.0));
        }
    }

    /// Creates a junction (no admin rights needed).
    fn junction(link: &Path, target: &Path) {
        let ok = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "failed to create junction");
    }

    /// A pnpm-like node_modules with a junction pointing outside, a dangling junction and a
    /// read-only file. Everything is deleted and link targets stay untouched.
    #[test]
    fn purge_removes_links_without_following_them() {
        let sb = Sandbox::new("links");
        let outside = sb.file(r"outside\keep.txt", "keep");
        let tree = sb.0.join("project");
        sb.file(r"project\a.txt", "a");
        sb.file(r"project\node_modules\.pnpm\pkg\index.js", "x");
        let ro = sb.file(r"project\readonly.txt", "r");
        let mut perms = fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ro, perms).unwrap();
        junction(
            &tree.join(r"node_modules\linked"),
            outside.parent().unwrap(),
        );
        // Delete the target first to get a dangling link.
        let gone = sb.0.join("gone");
        fs::create_dir_all(&gone).unwrap();
        junction(&tree.join(r"node_modules\prettier"), &gone);
        fs::remove_dir(&gone).unwrap();

        let progress = OpProgress::default();
        assert_eq!(
            purge(std::slice::from_ref(&tree), &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        assert!(!tree.exists());
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "keep",
            "link targets must not be deleted"
        );
        assert!(progress.done.load(Ordering::Relaxed) >= 6);
    }

    /// Deleting a selected link itself (a junction, and a folder symlink where Windows allows
    /// creating one) removes only the link; the folder it points to keeps its files.
    #[test]
    fn purging_a_selected_link_keeps_its_target() {
        let sb = Sandbox::new("selected-link");
        let kept = sb.file(r"target\keep.txt", "keep");
        let target = kept.parent().unwrap().to_path_buf();
        let junction_path = sb.0.join("junction");
        junction(&junction_path, &target);
        let mut links = vec![junction_path];
        let symlink = sb.0.join("symlink");
        // Folder symlinks need Developer Mode or admin rights; without them only the junction
        // is checked.
        if std::os::windows::fs::symlink_dir(&target, &symlink).is_ok() {
            links.push(symlink);
        }
        for link in links {
            let progress = OpProgress::default();
            assert_eq!(
                purge(std::slice::from_ref(&link), &progress).map_err(|e| e.to_string()),
                Ok(OpOutcome::Done)
            );
            assert!(
                fs::symlink_metadata(&link).is_err(),
                "{} removed",
                link.display()
            );
            assert_eq!(
                fs::read_to_string(&kept).unwrap(),
                "keep",
                "{}",
                link.display()
            );
        }
    }

    #[test]
    fn purge_stops_when_cancelled_and_ignores_missing_items() {
        let sb = Sandbox::new("cancel");
        let a = sb.file(r"a\x.txt", "x");
        let progress = OpProgress::default();
        assert_eq!(
            purge(&[sb.0.join("nope")], &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        progress.cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            purge(&[sb.0.join("a")], &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Cancelled)
        );
        assert!(a.exists(), "nothing is deleted after cancel");
    }

    /// A locked file is reported while everything else is deleted.
    #[test]
    fn purge_reports_locked_files_and_keeps_going() {
        let sb = Sandbox::new("locked");
        sb.file(r"t\free.txt", "f");
        let locked = sb.file(r"t\locked.txt", "l");
        // Opened without FILE_SHARE_DELETE, so it can't be deleted.
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&locked)
                .unwrap()
        };
        let progress = OpProgress::default();
        let result = purge(&[sb.0.join("t")], &progress);
        drop(handle);
        let error = result.unwrap_err();
        let ShellError::Items { failed } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].path, locked, "reported by its plain path");
        assert_eq!(failed[0].reason, arke_core::failure::FailReason::InUse);
        let message = error.to_string();
        assert!(message.contains("locked.txt"), "{message}");
        assert!(!sb.0.join(r"t\free.txt").exists());
        assert!(locked.exists());
    }

    /// Names Windows normally strips or turns into devices (trailing period or space, `con`,
    /// `nul`) and paths over 260 characters are deleted, not skipped as "not found".
    #[test]
    fn purge_deletes_mangled_names_and_long_paths() {
        let v = arke_core::path::verbatim;
        let sb = Sandbox::new("names");
        let top = sb.0.join("tree");
        fs::create_dir_all(&top).unwrap();
        for name in ["trailing dot.", "trailing space ", "con", "nul", "aux.txt"] {
            fs::write(v(&top.join(name)), "x").unwrap();
        }
        let mut deep = top.join("deep");
        while deep.as_os_str().len() < 300 {
            deep = deep.join("a_fairly_long_folder_name_0123456789");
        }
        fs::create_dir_all(v(&deep)).unwrap();
        fs::write(v(&deep.join("f.txt")), "x").unwrap();
        // Selected on their own, too.
        let lone_dot = sb.0.join("lone.");
        let lone_nul = sb.0.join("nul");
        fs::write(v(&lone_dot), "x").unwrap();
        fs::write(v(&lone_nul), "x").unwrap();

        let progress = OpProgress::default();
        assert_eq!(
            purge(
                &[top.clone(), lone_dot.clone(), lone_nul.clone()],
                &progress
            )
            .map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        for gone in [&top, &lone_dot, &lone_nul] {
            assert!(
                fs::symlink_metadata(v(gone)).is_err(),
                "{} is still there",
                gone.display()
            );
        }
    }
}
