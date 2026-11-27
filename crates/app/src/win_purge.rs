//! Permanent delete (Shift+Delete, and deleting from the Recycle Bin; already confirmed by
//! Arke), done with a custom walk instead of the Windows copy engine.
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
//! - Folders are listed once (large fetches) and their attributes used as listed, with no stat per
//!   item; the files and links of one folder are deleted on 4 to 8 threads, folders bottom-up.
//! - Cancel is checked before each item; progress goes to `OpProgress`.
//!
//! Call from a background thread.

use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use arke_core::fs::Fs as _;
use arke_core::{EntryKind, RealFs};
use rust_i18n::t;

use crate::shell::{ItemFailure, OpOutcome, OpProgress, ShellError};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
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

/// A Recycle Bin item to delete for good.
pub(crate) struct BinItem {
    /// Its `$R` data in the bin, a file or a folder.
    pub(crate) data: PathBuf,
    /// Where it was deleted from (original folder plus name): what messages call it.
    pub(crate) shown: PathBuf,
}

/// Empties Recycle Bin items: each one's `$R` data (a file or a whole folder) and then the `$I`
/// record next to it, the same walk as [`purge`] (`\\?\` paths, links removed as links,
/// read-only cleared). An item whose `$R` is already gone just loses its `$I`, without an error.
/// An item that can't be deleted completely keeps its `$I`, so it stays in the bin with what is
/// left, and is reported by its original name ("report.docx is in use by Word") while the rest
/// continues. When the drive holding the bin is gone (a USB drive pulled out), its items are
/// reported as such instead of counting as already deleted. Cancel stops before the next file;
/// failures so far and an item left half deleted are still reported ([`ShellError::Stopped`]).
/// `work_done` counts finished items out of `work_total`; `done` counts files and folders
/// removed.
///
/// Every item must be a `$R...` entry of a `$Recycle.Bin\<SID>` folder (or of `$Recycle.Bin`
/// itself on exFAT and FAT drives), or nothing is deleted.
pub(crate) fn purge_bin(items: &[BinItem], progress: &OpProgress) -> Result<OpOutcome, ShellError> {
    if let Some(stray) = items.iter().find(|item| !bin_entry(&item.data)) {
        return Err(ShellError::Missing {
            name: stray.data.display().to_string(),
        });
    }
    progress
        .work_total
        .store(items.len() as u64, Ordering::Relaxed);
    // By their real path for now, with the item each belongs to.
    let mut failed: Vec<(usize, ItemFailure)> = Vec::new();
    let mut cancelled = false;
    let mut partial = None;
    for (ix, item) in items.iter().enumerate() {
        let data = arke_core::path::verbatim(&item.data);
        let done_before = progress.done.load(Ordering::Relaxed);
        let mut here: Vec<(PathBuf, io::Error)> = Vec::new();
        cancelled = remove_tree(&data, progress, &mut here);
        let mut here: Vec<ItemFailure> = here
            .into_iter()
            .map(|(path, e)| ItemFailure::io(arke_core::path::unverbatim(&path), &e))
            .collect();
        if cancelled {
            // Started on but not finished: what is left stays in the bin.
            if progress.done.load(Ordering::Relaxed) > done_before {
                partial = Some(ix);
            }
        } else if here.is_empty() {
            if volume_gone(&data) {
                here.push(ItemFailure::new(
                    arke_core::path::unverbatim(&data),
                    arke_core::failure::FailReason::Other,
                    t!("fail.drive_gone").into_owned(),
                ));
            } else if let Some(info) = info_record(&data)
                && let Err(e) = remove_info(&info)
            {
                here.push(ItemFailure::io(arke_core::path::unverbatim(&info), &e));
            }
        }
        failed.extend(here.into_iter().map(|failure| (ix, failure)));
        if cancelled {
            break;
        }
        progress.work_done.fetch_add(1, Ordering::Relaxed);
    }
    // Programs holding files are looked up once for the whole batch (capped), by real path.
    let (owners, mut failed): (Vec<usize>, Vec<ItemFailure>) = failed.into_iter().unzip();
    crate::win_fileops::explain(&mut failed);
    for (failure, ix) in failed.iter_mut().zip(owners) {
        failure.path = shown_path(&failure.path, &items[ix]);
    }
    match (cancelled, partial) {
        (true, None) if failed.is_empty() => Ok(OpOutcome::Cancelled),
        (true, partial) => Err(ShellError::Stopped {
            partial: partial.map(|ix| shown_name(&items[ix])),
            failed,
        }),
        (false, _) if failed.is_empty() => Ok(OpOutcome::Done),
        (false, _) => Err(ShellError::Items { failed }),
    }
}

fn file_name(p: Option<&Path>) -> Option<&str> {
    p.and_then(Path::file_name).and_then(|n| n.to_str())
}

fn is_bin(p: Option<&Path>) -> bool {
    file_name(p).is_some_and(|n| n.eq_ignore_ascii_case("$Recycle.Bin"))
}

/// Whether `data` is shaped like a bin item: `...\$Recycle.Bin\<SID>\$R...` on NTFS, or
/// `...\$Recycle.Bin\$R...` on exFAT and FAT drives, which have no folder per user (#330).
fn bin_entry(data: &Path) -> bool {
    let parent = data.parent();
    file_name(Some(data)).is_some_and(|n| n.starts_with("$R") && n.len() > 2)
        && (is_bin(parent)
            || (file_name(parent).is_some_and(|n| n.starts_with("S-1-"))
                && is_bin(parent.and_then(Path::parent))))
}

/// The folder holding the item's `$Recycle.Bin` (the drive's root) is gone: the drive was
/// removed, so missing files mean nothing was deleted.
fn volume_gone(data: &Path) -> bool {
    data.ancestors()
        .find(|a| is_bin(Some(a)))
        .and_then(Path::parent)
        .is_some_and(|root| fs::metadata(root).is_err())
}

fn remove_info(info: &Path) -> io::Result<()> {
    match fs::symlink_metadata(info) {
        Ok(meta) => remove_writable(
            info,
            meta.file_attributes() & FILE_ATTRIBUTE_READONLY != 0,
            |p| fs::remove_file(p),
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// A failed path inside an item's `$R` data, as the user knows it: under where the item was
/// deleted from. The item itself and its `$I` record are the item's original path.
fn shown_path(real: &Path, item: &BinItem) -> PathBuf {
    let data = arke_core::path::unverbatim(&arke_core::path::verbatim(&item.data));
    let shown = arke_core::path::unverbatim(&item.shown);
    match real.strip_prefix(&data) {
        Ok(inside) if !inside.as_os_str().is_empty() => shown.join(inside),
        _ => shown,
    }
}

fn shown_name(item: &BinItem) -> String {
    item.shown.file_name().map_or_else(
        || item.shown.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The `$I...` record paired with a `$R...` item.
fn info_record(data: &Path) -> Option<PathBuf> {
    let name = data.file_name()?.to_str()?;
    let rest = name.strip_prefix("$R")?;
    Some(data.with_file_name(format!("$I{rest}")))
}

/// One step of [`remove_tree`]'s walk.
enum Step {
    /// List this folder, delete its files and links in parallel, then visit its subfolders.
    List { path: PathBuf, readonly: bool },
    /// Every child of this folder has been dealt with: delete the folder itself. `failed_before`
    /// is how many failures there were when it was listed, so a child's failure is told apart.
    Leave {
        path: PathBuf,
        readonly: bool,
        failed_before: usize,
    },
}

/// What a listed child is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A real folder: emptied, then removed.
    Dir,
    /// A file, or a link to a file.
    File,
    /// A junction, or a symlink to a folder: removed with `remove_dir`, never entered.
    DirLink,
}

/// A child of a listed folder, with what its listing says about it (no per-item stat).
struct Child {
    path: PathBuf,
    kind: Kind,
    /// Has the Read-only attribute.
    readonly: bool,
}

/// Lists `dir` with the attributes the directory listing already carries (large fetches, one
/// find record per item), so no item needs a stat of its own. Items that can't be read are left
/// out (the folder then can't be removed, which is reported).
fn list_dir(dir: &Path) -> io::Result<Vec<Child>> {
    let mut children = Vec::new();
    for entry in RealFs.read_dir(dir)?.flatten() {
        // The listing's names are lossy text; a name with unpaired surrogates would point at
        // another path, so such a folder is listed by std instead (exact names).
        if entry.name.contains('\u{FFFD}') {
            return list_dir_std(dir);
        }
        let kind = match entry.kind {
            EntryKind::Dir => Kind::Dir,
            EntryKind::Link { is_dir: true } => Kind::DirLink,
            EntryKind::File | EntryKind::Link { is_dir: false } => Kind::File,
        };
        children.push(Child {
            path: dir.join(&*entry.name),
            kind,
            readonly: entry.attributes & FILE_ATTRIBUTE_READONLY != 0,
        });
    }
    Ok(children)
}

/// [`list_dir`] with `std::fs::read_dir`, which keeps names exactly.
fn list_dir_std(dir: &Path) -> io::Result<Vec<Child>> {
    let mut children = Vec::new();
    for entry in fs::read_dir(dir)?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let attributes = meta.file_attributes();
        let kind = if meta.file_type().is_symlink() {
            if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                Kind::DirLink
            } else {
                Kind::File
            }
        } else if meta.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        };
        children.push(Child {
            path: entry.path(),
            kind,
            readonly: attributes & FILE_ATTRIBUTE_READONLY != 0,
        });
    }
    Ok(children)
}

/// Fewer files than this in a folder are deleted on the calling thread: starting threads costs
/// more than it saves.
const PARALLEL_MIN: usize = 64;

/// How many threads delete the files of one folder: the machine's, within 4 to 8. Deleting is
/// mostly waiting on the file system (and antivirus), so more than the cores still helps.
fn workers() -> usize {
    static WORKERS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WORKERS.get_or_init(|| std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 8)))
}

/// Counts a finished removal. `Ok` is one more item done; an item that is already gone counts as
/// deleted; anything else is handed back to be reported.
fn account(result: io::Result<()>, progress: &OpProgress) -> Option<io::Error> {
    match result {
        Ok(()) => {
            progress.done.fetch_add(1, Ordering::Relaxed);
            #[cfg(test)]
            tests::after_removal(progress);
            None
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => Some(e),
    }
}

/// Removes a file or a link (never what a link points to).
fn remove_child(child: &Child) -> io::Result<()> {
    if child.kind == Kind::DirLink {
        fs::remove_dir(&child.path)
    } else {
        remove_writable(&child.path, child.readonly, |p| fs::remove_file(p))
    }
}

/// Deletes the files and links of one folder, in parallel when there are many. Cancel is checked
/// before each item, so at most one item per thread is still being removed after it. Failures go
/// to `failed` in listing order. Returns true if cancelled.
fn remove_batch(
    children: &[Child],
    progress: &OpProgress,
    failed: &mut Vec<(PathBuf, io::Error)>,
) -> bool {
    if children.len() < PARALLEL_MIN {
        for child in children {
            if progress.cancel.load(Ordering::Relaxed) {
                return true;
            }
            if let Some(e) = account(remove_child(child), progress) {
                failed.push((child.path.clone(), e));
            }
        }
        return false;
    }
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let work = || {
        let mut errors: Vec<(usize, PathBuf, io::Error)> = Vec::new();
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                stopped.store(true, Ordering::Relaxed);
                break;
            }
            let ix = next.fetch_add(1, Ordering::Relaxed);
            let Some(child) = children.get(ix) else { break };
            if let Some(e) = account(remove_child(child), progress) {
                errors.push((ix, child.path.clone(), e));
            }
        }
        errors
    };
    let mut errors = std::thread::scope(|scope| {
        let mut helpers = Vec::new();
        for _ in 1..workers().min(children.len()) {
            helpers.push(scope.spawn(work));
        }
        let mut errors = work();
        for helper in helpers {
            match helper.join() {
                Ok(more) => errors.extend(more),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        errors
    });
    errors.sort_by_key(|(ix, ..)| *ix);
    failed.extend(errors.into_iter().map(|(_, path, e)| (path, e)));
    stopped.load(Ordering::Relaxed)
}

/// Deletes `path` and everything below it. Returns true if cancelled.
///
/// The walk keeps its own stack rather than recursing, so a folder nested thousands of levels deep
/// (which `\\?\` paths allow) can't overflow the thread's stack, and only one folder is open at a
/// time. Folders are removed after their contents (post-order); the files and links inside one
/// folder are removed in parallel ([`remove_batch`]).
fn remove_tree(path: &Path, progress: &OpProgress, failed: &mut Vec<(PathBuf, io::Error)>) -> bool {
    if progress.cancel.load(Ordering::Relaxed) {
        return true;
    }
    // The only stat of the walk: what was selected. Everything below comes with its listing.
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // Already gone (e.g. a link target deleted earlier): treat as deleted.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(e) => {
            failed.push((path.to_path_buf(), e));
            return false;
        }
    };
    let readonly = meta.file_attributes() & FILE_ATTRIBUTE_READONLY != 0;
    if meta.file_type().is_symlink() {
        // Symlink, junction or mount point: remove the link only, never what it points to
        // (directory links need remove_dir).
        let result = if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
        if let Some(e) = account(result, progress) {
            failed.push((path.to_path_buf(), e));
        }
        return false;
    }
    if !meta.is_dir() {
        let result = remove_writable(path, readonly, |p| fs::remove_file(p));
        if let Some(e) = account(result, progress) {
            failed.push((path.to_path_buf(), e));
        }
        return false;
    }
    let mut steps = vec![Step::List {
        path: path.to_path_buf(),
        readonly,
    }];
    while let Some(step) = steps.pop() {
        match step {
            Step::List { path, readonly } => {
                if progress.cancel.load(Ordering::Relaxed) {
                    return true;
                }
                // Listed in full and closed before going deeper.
                let children = match list_dir(&path) {
                    Ok(children) => children,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        failed.push((path, e));
                        continue;
                    }
                };
                steps.push(Step::Leave {
                    path,
                    readonly,
                    failed_before: failed.len(),
                });
                let (dirs, leaves): (Vec<Child>, Vec<Child>) =
                    children.into_iter().partition(|c| c.kind == Kind::Dir);
                if remove_batch(&leaves, progress, failed) {
                    return true;
                }
                steps.extend(dirs.into_iter().rev().map(|c| Step::List {
                    path: c.path,
                    readonly: c.readonly,
                }));
            }
            Step::Leave {
                path,
                readonly,
                failed_before,
            } => {
                let result = remove_writable(&path, readonly, |p| fs::remove_dir(p));
                // The folder failed because of a child already recorded: don't report it twice.
                if result.is_err() && failed.len() > failed_before {
                    continue;
                }
                if let Some(e) = account(result, progress) {
                    failed.push((path, e));
                }
            }
        }
    }
    false
}

/// Removes `path`, clearing the read-only attribute and retrying if it was set (`readonly`).
fn remove_writable(
    path: &Path,
    readonly: bool,
    remove: fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match remove(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && readonly => {
            let mut perms = fs::symlink_metadata(path)?.permissions();
            #[allow(
                clippy::permissions_set_readonly_false,
                reason = "Windows: clears the read-only attribute so the file can be deleted"
            )]
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

    struct Sandbox(
        PathBuf,
        #[allow(dead_code, reason = "held for its Drop, which deletes the folder")]
        tempfile::TempDir,
    );

    impl Sandbox {
        fn new(name: &str) -> Self {
            let guard = crate::win::long_tempdir(&format!("arke-purge-{name}-"));
            Self(guard.path().to_path_buf(), guard)
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

    /// Timing for #9, not a check. Run with
    /// `cargo test -p arke-app --bin arke purge_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: writes 20,000 files to %TEMP%"]
    #[allow(
        clippy::print_stderr,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn purge_timing() {
        let sb = Sandbox::new("timing");
        let dir = sb.0.join("many");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..20_000 {
            fs::write(dir.join(format!("f{i:05}.txt")), b"x").unwrap();
        }
        let progress = OpProgress::default();
        let started = std::time::Instant::now();
        let result = purge(std::slice::from_ref(&dir), &progress);
        eprintln!(
            "permanent delete of 20,000 files: {:?} ({:?}, {} items)",
            started.elapsed(),
            result.map_err(|e| e.to_string()),
            progress.done.load(Ordering::Relaxed)
        );
        assert!(!dir.exists());
    }

    /// #330: exFAT and FAT drives keep items right in `$Recycle.Bin`, with no folder per user;
    /// they can be deleted for good too. Nested items and similar names elsewhere still can't.
    #[test]
    fn bins_without_a_folder_per_user_count() {
        assert!(super::bin_entry(Path::new(
            r"C:\$Recycle.Bin\S-1-5-21-1\$RABC.txt"
        )));
        assert!(super::bin_entry(Path::new(r"E:\$RECYCLE.BIN\$RU1O20G.txt")));
        assert!(!super::bin_entry(Path::new(
            r"E:\$RECYCLE.BIN\$RABC\$RDEF.txt"
        )));
        assert!(!super::bin_entry(Path::new(r"E:\Data\$RABC.txt")));
        assert!(super::volume_gone(Path::new(
            r"Q:\$RECYCLE.BIN\$RU1O20G.txt"
        )));
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

    /// A Recycle Bin-like folder in the sandbox (`$Recycle.Bin\<SID>`); never the real bin.
    fn fake_bin(sb: &Sandbox) -> PathBuf {
        fake_bin_in(&sb.0)
    }

    /// The same, with `drive` standing for the drive's root.
    fn fake_bin_in(drive: &Path) -> PathBuf {
        let bin = drive.join(r"$Recycle.Bin\S-1-5-21-0-0-0-1001");
        fs::create_dir_all(&bin).unwrap();
        bin
    }

    /// A test callback given the operation's progress.
    type Hook = std::sync::Arc<dyn Fn(&OpProgress) + Send + Sync>;

    /// Hooks by the address of the `OpProgress` they watch, so tests running side by side don't
    /// see each other's removals and worker threads reach the hook too.
    static HOOKS: parking_lot::Mutex<Vec<(usize, Hook)>> = parking_lot::Mutex::new(Vec::new());

    pub(super) fn after_removal(progress: &OpProgress) {
        let key = std::ptr::from_ref(progress) as usize;
        let hook = HOOKS
            .lock()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, hook)| Hook::clone(hook));
        if let Some(hook) = hook {
            hook(progress);
        }
    }

    /// Runs `hook` after each file or folder removed under `progress`, until the guard drops.
    struct HookGuard(usize);

    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOKS.lock().retain(|(k, _)| *k != self.0);
        }
    }

    fn hook_removals(
        progress: &OpProgress,
        hook: impl Fn(&OpProgress) + Send + Sync + 'static,
    ) -> HookGuard {
        let key = std::ptr::from_ref(progress) as usize;
        HOOKS.lock().push((key, std::sync::Arc::new(hook)));
        HookGuard(key)
    }

    /// A folder nested 400 levels deep (only `\\?\` paths reach that far) is emptied on a thread
    /// with a small stack: the walk keeps its own stack, so it can't overflow the thread's.
    #[test]
    fn purge_bin_empties_a_very_deep_folder_on_a_small_stack() {
        // Still several times what a recursive walk fits in the small stack below (a frame of the
        // walk is hundreds of bytes, the stack 128 KiB). Each level costs more the deeper it is,
        // so 1,000 took 4 to 10 s alone and timed out under a loaded full run.
        const LEVELS: usize = 400;
        let sb = Sandbox::new("bin-deep");
        let bin = fake_bin(&sb);
        let item = record(&bin, "AAAAA1", "deep");
        // Built bottom up with short paths, each new level taking the chain so far as its "a",
        // so it is quick; only deleting it walks the long paths.
        let chain = sb.0.join("chain");
        let next = sb.0.join("next");
        fs::create_dir(&chain).unwrap();
        fs::write(chain.join("bottom.txt"), "x").unwrap();
        // A folder just changed can be held open for a moment (antivirus, indexing).
        let rename = |from: &Path, to: &Path| {
            for _ in 0..200 {
                match fs::rename(from, to) {
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    other => return other.unwrap(),
                }
            }
            panic!("{} stayed locked", from.display());
        };
        for _ in 0..LEVELS {
            fs::create_dir(&next).unwrap();
            rename(&chain, &next.join("a"));
            rename(&next, &chain);
        }
        rename(&chain, &item.data);
        let mut deepest = item.data.clone();
        for _ in 0..LEVELS {
            deepest.push("a");
        }
        assert!(exists(&deepest.join("bottom.txt")));
        let data = item.data.clone();
        let result = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let progress = OpProgress::default();
                let result = purge_bin(&[item], &progress).map_err(|e| e.to_string());
                (result, progress.done.load(Ordering::Relaxed))
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(result.0, Ok(OpOutcome::Done));
        assert_eq!(
            result.1,
            LEVELS as u64 + 2,
            "every level, the file, the item"
        );
        assert!(!exists(&data) && !exists(&info_record(&data).unwrap()));
    }

    /// Links are removed as links, wherever they are: a `$R` that is itself a junction or folder
    /// symlink, and a junction, folder symlink and file symlink inside a `$R` folder, all pointing
    /// outside the bin. What they point to keeps every file.
    #[test]
    fn purge_bin_removes_links_and_keeps_their_targets() {
        let sb = Sandbox::new("bin-links");
        let bin = fake_bin(&sb);
        let kept = sb.file(r"outside\dir\keep.txt", "keep");
        let target = kept.parent().unwrap().to_path_buf();
        let kept_file = sb.file(r"outside\lone.txt", "lone");
        let symlinks = std::os::windows::fs::symlink_dir(&target, sb.0.join("probe")).is_ok();
        if symlinks {
            fs::remove_dir(sb.0.join("probe")).unwrap();
        }

        let mut items = Vec::new();
        let as_junction = record(&bin, "AAAAA1", "junction");
        junction(&as_junction.data, &target);
        items.push(as_junction);
        if symlinks {
            let as_symlink = record(&bin, "BBBBB2", "symlink");
            std::os::windows::fs::symlink_dir(&target, &as_symlink.data).unwrap();
            items.push(as_symlink);
        }
        let holder = record(&bin, "CCCCC3", "holder");
        fs::create_dir_all(&holder.data).unwrap();
        fs::write(holder.data.join("own.txt"), "own").unwrap();
        junction(&holder.data.join("junction"), &target);
        if symlinks {
            std::os::windows::fs::symlink_dir(&target, holder.data.join("dir-link")).unwrap();
            std::os::windows::fs::symlink_file(&kept_file, holder.data.join("file-link")).unwrap();
        }
        items.push(holder);

        let progress = OpProgress::default();
        assert_eq!(
            purge_bin(&items, &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        for item in &items {
            assert!(!exists(&item.data), "{}", item.data.display());
            assert!(!exists(&info_record(&item.data).unwrap()));
        }
        assert_eq!(fs::read_to_string(&kept).unwrap(), "keep");
        assert_eq!(fs::read_to_string(&kept_file).unwrap(), "lone");
    }

    /// A `$R` without its `$I` record is deleted all the same.
    #[test]
    fn purge_bin_deletes_data_without_a_record() {
        let sb = Sandbox::new("bin-noinfo");
        let bin = fake_bin(&sb);
        let data = bin.join("$RAAAAA1");
        fs::create_dir_all(data.join("sub")).unwrap();
        fs::write(data.join(r"sub\f.txt"), "f").unwrap();
        let item = BinItem {
            data: data.clone(),
            shown: PathBuf::from(r"C:\orig\folder"),
        };
        assert_eq!(
            purge_bin(&[item], &OpProgress::default()).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        assert!(!exists(&data));
    }

    /// Cancelled halfway through a folder, after another item failed: both are reported, not a
    /// plain "cancelled". The half-deleted item keeps its `$I` (it stays in the bin with what is
    /// left) and the item after it isn't touched.
    #[test]
    fn purge_bin_cancelled_mid_folder_reports_what_happened() {
        let sb = Sandbox::new("bin-stop");
        let bin = fake_bin(&sb);
        let locked = record(&bin, "AAAAA1.txt", "locked.txt");
        fs::write(&locked.data, "l").unwrap();
        let folder = record(&bin, "BBBBB2", "photos");
        fs::create_dir_all(&folder.data).unwrap();
        for i in 0..10 {
            fs::write(folder.data.join(format!("p{i}.jpg")), "p").unwrap();
        }
        let after = record(&bin, "CCCCC3.txt", "after.txt");
        fs::write(&after.data, "a").unwrap();
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&locked.data)
                .unwrap()
        };
        // Cancel once three of the folder's files are gone.
        let items = [locked, folder, after];
        let progress = OpProgress::default();
        let guard = hook_removals(&progress, |p| {
            if p.done.load(Ordering::Relaxed) >= 3 {
                p.cancel.store(true, Ordering::Relaxed);
            }
        });
        let result = purge_bin(&items, &progress);
        drop(guard);
        drop(handle);

        let error = result.unwrap_err();
        let ShellError::Stopped { partial, failed } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(partial.as_deref(), Some("photos"));
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].path, Path::new(r"C:\orig\locked.txt"));
        let message = error.to_string();
        assert!(
            message.contains("photos") && message.contains("locked.txt"),
            "{message}"
        );
        let [_, folder, after] = &items;
        assert!(folder.data.exists() && info_record(&folder.data).unwrap().exists());
        assert_eq!(fs::read_dir(&folder.data).unwrap().count(), 7);
        assert!(after.data.exists() && info_record(&after.data).unwrap().exists());
        assert_eq!(progress.work_done.load(Ordering::Relaxed), 1);
    }

    /// Programs holding files are looked up once for the whole batch, not once per item.
    #[test]
    fn purge_bin_looks_up_holders_once() {
        use std::os::windows::fs::OpenOptionsExt;
        let sb = Sandbox::new("bin-explain");
        let bin = fake_bin(&sb);
        let mut items = Vec::new();
        let mut handles = Vec::new();
        for i in 0..4 {
            let item = record(&bin, &format!("AAAAA{i}.txt"), &format!("held{i}.txt"));
            fs::write(&item.data, "h").unwrap();
            handles.push(
                fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(&item.data)
                    .unwrap(),
            );
            items.push(item);
        }
        let before = crate::win_fileops::explain_calls();
        let result = purge_bin(&items, &OpProgress::default());
        drop(handles);
        assert_eq!(crate::win_fileops::explain_calls() - before, 1);
        let Err(ShellError::Items { failed }) = result else {
            panic!("{:?}", result.map_err(|e| e.to_string()));
        };
        assert_eq!(failed.len(), 4);
    }

    /// The drive holding the bin is gone (a USB drive pulled out): its items are reported, not
    /// counted as deleted because nothing was found.
    #[test]
    fn purge_bin_reports_items_of_a_drive_that_is_gone() {
        let sb = Sandbox::new("bin-unplugged");
        let drive = sb.0.join("drive");
        let bin = fake_bin_in(&drive);
        let item = record(&bin, "AAAAA1.txt", "notes.txt");
        fs::write(&item.data, "n").unwrap();
        fs::remove_dir_all(&drive).unwrap();
        let error = purge_bin(&[item], &OpProgress::default()).unwrap_err();
        let ShellError::Items { failed } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].path, Path::new(r"C:\orig\notes.txt"));
        let message = error.to_string();
        assert!(
            message.contains(&*t!("fail.drive_gone")) && message.contains("notes.txt"),
            "{message}"
        );
    }

    /// Anything that isn't a `$R` entry of a `$Recycle.Bin\<SID>` folder rejects the whole batch
    /// before anything is deleted.
    #[test]
    fn purge_bin_refuses_paths_outside_a_bin() {
        let sb = Sandbox::new("bin-stray");
        let bin = fake_bin(&sb);
        let fine = record(&bin, "AAAAA1.txt", "fine.txt");
        fs::write(&fine.data, "f").unwrap();
        let stray = sb.file(r"docs\$RNOTBIN.txt", "mine");
        let plain = sb.file(r"docs\report.txt", "mine");
        for bad in [stray, plain] {
            let items = [
                BinItem {
                    data: fine.data.clone(),
                    shown: fine.shown.clone(),
                },
                BinItem {
                    data: bad.clone(),
                    shown: PathBuf::from(r"C:\orig\x"),
                },
            ];
            assert!(purge_bin(&items, &OpProgress::default()).is_err());
            assert!(bad.exists() && fine.data.exists(), "{}", bad.display());
        }
    }

    /// Writes `$I<id>` and returns the matching item to purge, `$R<id>`, named `name` from
    /// `C:\orig`.
    fn record(bin: &Path, id: &str, name: &str) -> BinItem {
        fs::write(
            arke_core::path::verbatim(&bin.join(format!("$I{id}"))),
            "info",
        )
        .unwrap();
        BinItem {
            data: bin.join(format!("$R{id}")),
            shown: PathBuf::from(r"C:\orig").join(name),
        }
    }

    fn exists(path: &Path) -> bool {
        fs::symlink_metadata(arke_core::path::verbatim(path)).is_ok()
    }

    fn read_only(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms).unwrap();
    }

    /// Emptying deletes each item's `$R` data and its `$I` record: a file, a folder tree with
    /// read-only files and folders and names only `\\?\` paths reach (the "x." that made Windows
    /// say "Item Not Found"), a `$R` whose own name ends in a period, and an entry whose `$R` is
    /// already gone (just its `$I` goes, no error). Other bin items are not touched.
    #[test]
    fn purge_bin_empties_items_of_a_fake_bin() {
        let v = arke_core::path::verbatim;
        let sb = Sandbox::new("bin");
        let bin = fake_bin(&sb);
        let file = record(&bin, "AAAAA1.txt", "notes.txt");
        fs::write(&file.data, "notes").unwrap();

        let tree = record(&bin, "BBBBB2", "project");
        let deep = tree.data.join(r"src\deep");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("main.rs"), "fn main() {}").unwrap();
        for odd in ["x.", "trailing space ", "con", "nul.txt"] {
            fs::write(v(&tree.data.join(odd)), "odd").unwrap();
        }
        fs::create_dir_all(v(&tree.data.join(r"dots.\inner."))).unwrap();
        fs::write(v(&tree.data.join(r"dots.\inner.\y.")), "odd").unwrap();
        let ro_file = tree.data.join(r"src\locked-down.txt");
        fs::write(&ro_file, "ro").unwrap();
        read_only(&ro_file);
        let ro_dir = tree.data.join("frozen");
        fs::create_dir_all(&ro_dir).unwrap();
        fs::write(ro_dir.join("inside.txt"), "in").unwrap();
        read_only(&ro_dir.join("inside.txt"));
        read_only(&ro_dir);

        let dotted = record(&bin, "CCCCC3.", "y.");
        fs::write(v(&dotted.data), "y").unwrap();

        // `$I` without its `$R` (deleted behind the bin's back).
        let orphan = record(&bin, "DDDDD4.txt", "gone.txt");

        let bystander = record(&bin, "EEEEE5.txt", "keep.txt");
        fs::write(&bystander.data, "keep").unwrap();

        let items = [file, tree, dotted, orphan];
        let progress = OpProgress::default();
        assert_eq!(
            purge_bin(&items, &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        for item in &items {
            let info = info_record(&item.data).unwrap();
            assert!(
                !exists(&item.data),
                "{} is still there",
                item.data.display()
            );
            assert!(!exists(&info), "{} is still there", info.display());
        }
        assert_eq!(progress.work_total.load(Ordering::Relaxed), 4);
        assert_eq!(progress.work_done.load(Ordering::Relaxed), 4);
        assert!(progress.done.load(Ordering::Relaxed) >= 14);
        assert_eq!(fs::read_to_string(&bystander.data).unwrap(), "keep");
        assert!(info_record(&bystander.data).unwrap().exists());
    }

    /// A file in use is reported by its original name (inside the folder it was deleted with, or
    /// the item itself), never as `$R...`. That item keeps its `$I`, so it stays in the bin with
    /// what is left; the other items are still deleted.
    #[test]
    fn purge_bin_names_what_failed_and_keeps_going() {
        let sb = Sandbox::new("bin-locked");
        let bin = fake_bin(&sb);
        let folder = record(&bin, "AAAAA1", "project");
        fs::create_dir_all(&folder.data).unwrap();
        fs::write(folder.data.join("free.txt"), "f").unwrap();
        let held = folder.data.join("held.txt");
        fs::write(&held, "h").unwrap();
        let lone = record(&bin, "BBBBB2.docx", "report.docx");
        fs::write(&lone.data, "r").unwrap();
        let fine = record(&bin, "CCCCC3.txt", "fine.txt");
        fs::write(&fine.data, "ok").unwrap();
        // Opened without FILE_SHARE_DELETE, so they can't be deleted.
        let open = |path: &Path| {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(path)
                .unwrap()
        };
        let handles = (open(&held), open(&lone.data));

        let items = [folder, lone, fine];
        let progress = OpProgress::default();
        let result = purge_bin(&items, &progress);
        drop(handles);
        let error = result.unwrap_err();
        let ShellError::Items { failed } = &error else {
            panic!("{error:?}");
        };
        let paths: Vec<&Path> = failed.iter().map(|f| f.path.as_path()).collect();
        assert_eq!(
            paths,
            [
                Path::new(r"C:\orig\project\held.txt"),
                Path::new(r"C:\orig\report.docx")
            ]
        );
        assert!(
            failed
                .iter()
                .all(|f| f.reason == arke_core::failure::FailReason::InUse),
            "{failed:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("held.txt") && message.contains("report.docx"),
            "{message}"
        );
        assert!(!message.contains("$R"), "{message}");
        // What could go went; the failed items stay listed.
        let [folder, lone, fine] = &items;
        assert!(!folder.data.join("free.txt").exists());
        assert!(held.exists() && info_record(&folder.data).unwrap().exists());
        assert!(lone.data.exists() && info_record(&lone.data).unwrap().exists());
        assert!(!fine.data.exists() && !info_record(&fine.data).unwrap().exists());
        assert_eq!(progress.work_done.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn purge_bin_stops_when_cancelled() {
        let sb = Sandbox::new("bin-cancel");
        let bin = fake_bin(&sb);
        let item = record(&bin, "AAAAA1.txt", "a.txt");
        fs::write(&item.data, "a").unwrap();
        let progress = OpProgress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let items = [item];
        assert_eq!(
            purge_bin(&items, &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Cancelled)
        );
        assert!(items[0].data.exists() && info_record(&items[0].data).unwrap().exists());
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

    /// A folder with many files (the parallel path), nested folders, read-only files and a
    /// junction to a folder outside: all gone, the junction's target untouched, and every item
    /// counted once.
    #[test]
    fn purge_deletes_a_big_tree_in_parallel_and_keeps_link_targets() {
        let sb = Sandbox::new("parallel");
        let outside = sb.file(r"outside\keep.txt", "keep");
        let tree = sb.0.join("tree");
        let mut files = 0;
        for dir in ["tree", "tree/a", "tree/a/b", "tree/c"] {
            for i in 0..(PARALLEL_MIN * 3) {
                let rel = format!("{dir}/f{i}.txt");
                let path = sb.file(&rel, "x");
                if i % 7 == 0 {
                    read_only(&path);
                }
                files += 1;
            }
        }
        junction(&tree.join("a").join("link"), outside.parent().unwrap());
        // Folders: tree, a, a/b, c, and the junction itself.
        let items = files + 5;

        let progress = OpProgress::default();
        assert_eq!(
            purge(std::slice::from_ref(&tree), &progress).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        assert!(!exists(&tree));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(progress.done.load(Ordering::Relaxed), items);
    }

    /// Many files, some in use: the failures are reported in listing order with the right path
    /// and reason, the rest goes, and the folder (still holding them) is not reported again.
    #[test]
    fn purge_reports_locked_files_among_many() {
        use std::os::windows::fs::OpenOptionsExt;
        let sb = Sandbox::new("parallel-locked");
        let mut handles = Vec::new();
        let mut locked = Vec::new();
        for i in 0..PARALLEL_MIN * 2 {
            let name = format!("f{i:03}.txt");
            let path = sb.file(&format!("t/{name}"), "x");
            if i % 50 == 0 {
                handles.push(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(&path)
                        .unwrap(),
                );
                locked.push(sb.0.join("t").join(name));
            }
        }
        let result = purge(&[sb.0.join("t")], &OpProgress::default());
        drop(handles);
        let Err(ShellError::Items { failed }) = result else {
            panic!("{:?}", result.map_err(|e| e.to_string()));
        };
        let mut paths: Vec<PathBuf> = failed.iter().map(|f| f.path.clone()).collect();
        paths.sort();
        assert_eq!(paths, locked);
        let left = fs::read_dir(sb.0.join("t")).unwrap().count();
        assert_eq!(left, locked.len());
    }

    /// Cancelled while a big folder is being emptied: it stops within a few items (one per
    /// thread may still be in flight), reports cancelled, and leaves the rest.
    #[test]
    fn purge_stops_promptly_when_cancelled_mid_folder() {
        const FILES: usize = 600;
        let sb = Sandbox::new("parallel-cancel");
        let dir = sb.0.join("many");
        for i in 0..FILES {
            sb.file(&format!("many/f{i:03}.txt"), "x");
        }
        let progress = OpProgress::default();
        let guard = hook_removals(&progress, |p| {
            if p.done.load(Ordering::Relaxed) >= 20 {
                p.cancel.store(true, Ordering::Relaxed);
            }
        });
        let result = purge(std::slice::from_ref(&dir), &progress);
        drop(guard);
        assert_eq!(result.map_err(|e| e.to_string()), Ok(OpOutcome::Cancelled));
        let done = progress.done.load(Ordering::Relaxed);
        assert!(
            (20..20 + 8).contains(&done),
            "{done} removed after cancel at 20"
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count() as u64,
            FILES as u64 - done
        );
    }

    /// Names with unpaired surrogates (legal on NTFS, lossy in a listing) are still deleted.
    #[test]
    fn purge_deletes_names_that_are_not_valid_unicode() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let sb = Sandbox::new("surrogate");
        let dir = sb.0.join("t");
        fs::create_dir_all(&dir).unwrap();
        let odd = dir.join(OsString::from_wide(&[0x61, 0xD800, 0x62]));
        fs::write(&odd, "x").unwrap();
        sb.file(r"t\plain.txt", "p");
        assert_eq!(
            purge(std::slice::from_ref(&dir), &OpProgress::default()).map_err(|e| e.to_string()),
            Ok(OpOutcome::Done)
        );
        assert!(!dir.exists());
    }
}
