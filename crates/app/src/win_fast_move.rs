//! Same-volume move fast path.
//!
//! Moving inside one volume is only a rename, but the Windows engine (`IFileOperation`) still goes
//! through the shell for every item, which costs a millisecond or two each. Here an item is renamed
//! straight away with `MoveFileExW` (no flags: it fails rather than replaces, and never copies).
//! Everything else goes to the engine afterwards, exactly as before (`perform_tracked` in
//! `win_fileops.rs` merges the two and the undo). An item is renamed here when:
//!
//! - Source folder and destination are on local drive letters (fixed or removable; not network,
//!   subst, shell namespace or server paths) of the same volume, compared with links resolved.
//! - The item is not a link: no symlink, junction, cloud placeholder or other reparse point, which
//!   the engine knows how to treat.
//! - Nothing in the destination has the item's name (either case, either its 8.3 or its long
//!   name), and no earlier item of the same move takes it. So there is nothing to merge, replace or
//!   ask about; Windows' conflict dialog stays as it is for everything else.
//! - The destination is neither the item nor inside it, and not the folder the item is already in.
//! - No source holds another source.
//!
//! An item whose rename fails for any reason (in use, access denied, a name Windows refuses, a
//! path too long) is left to the engine, which reports or asks as it does today. Undo is the same
//! "put back" list the engine records. Explorer windows are told with `SHCNE_RENAMEITEM` and
//! `SHCNE_RENAMEFOLDER`, one by one up to [`NOTIFY_EACH_MAX`], else the folders involved once.
//! Same-volume renames move no bytes, so none are counted, as with the engine.

use std::collections::{HashMap, HashSet};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
use windows::core::HSTRING;

use crate::shell::{OpProgress, PutBack};

/// Most renames reported to Explorer one by one; past this the folders are reported once.
const NOTIFY_EACH_MAX: usize = 64;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;

/// What [`rename`] did.
#[derive(Debug, Default)]
pub(crate) struct Moved {
    /// How to put back what was renamed, in the order of the sources.
    pub(crate) put_back: Vec<PutBack>,
    /// Sources left to the Windows engine, in order.
    pub(crate) rest: Vec<PathBuf>,
    /// Cancel was pressed: nothing is left to the engine.
    pub(crate) cancelled: bool,
}

/// What is known about one item, for [`can_rename`].
#[derive(Clone, Copy, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a fact table for one yes/no decision; each fact is tested and named on its own"
)]
struct Facts {
    /// Source folder and destination are on the same volume.
    same_volume: bool,
    /// The item is a reparse point of any kind.
    link: bool,
    /// Nothing in the destination has the item's name.
    name_free: bool,
    /// No earlier item of this move has the name.
    name_unique: bool,
    /// The destination is the item or inside it.
    into_itself: bool,
    /// The item is already in the destination folder.
    same_folder: bool,
}

/// Whether the item is renamed here rather than left to the engine.
const fn can_rename(facts: Facts) -> bool {
    facts.same_volume
        && !facts.link
        && facts.name_free
        && facts.name_unique
        && !facts.into_itself
        && !facts.same_folder
}

/// A path as a set key: either slash, no trailing slash, lowercase (the way Windows compares).
fn key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Whether one of `sources` is the same as, or inside, another one.
fn nested(sources: &[PathBuf]) -> bool {
    let keys: HashSet<String> = sources.iter().map(|s| key(s)).collect();
    keys.len() != sources.len()
        || sources
            .iter()
            .any(|s| s.ancestors().skip(1).any(|up| keys.contains(&key(up))))
}

/// The destination folder, when renames may go there.
struct Target {
    /// `\\?\` with links resolved.
    real: PathBuf,
    /// Its volume, [`volume_key`].
    volume: String,
}

impl Target {
    fn find(dest: &Path) -> Option<Self> {
        let (real, volume) = place(dest)?;
        std::fs::metadata(&real)
            .is_ok_and(|m| m.is_dir())
            .then_some(Self { real, volume })
    }
}

/// A volume as a set key.
fn volume_key(root: &Path) -> String {
    key(root)
}

/// `folder` with links resolved and its volume, when it is on a local drive letter.
fn place(folder: &Path) -> Option<(PathBuf, String)> {
    use crate::win_fast_copy::{local_letter, real_path, volume_root};
    if !local_letter(folder) {
        return None;
    }
    let real = real_path(folder)?;
    let volume = volume_key(&volume_root(&real)?);
    Some((real, volume))
}

/// A top-level item renamed.
struct Renamed {
    /// Where it was and is, as the caller names them.
    from: PathBuf,
    now: PathBuf,
    is_dir: bool,
}

/// Renames what it can of `sources` into the folder `dest` (see the module docs); the rest is
/// returned for the engine. Background thread.
pub(crate) fn rename(sources: &[PathBuf], dest: &Path, progress: &OpProgress) -> Moved {
    let mut out = Moved::default();
    if progress.cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        return out;
    }
    let target = Target::find(dest).filter(|_| !nested(sources));
    let Some(target) = target else {
        out.rest = sources.to_vec();
        return out;
    };
    let mut parents: HashMap<PathBuf, Option<(PathBuf, String)>> = HashMap::new();
    let mut names = HashSet::new();
    let mut renamed = Vec::new();
    for source in sources {
        if progress.cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        match rename_one(source, dest, &target, &mut parents, &mut names) {
            Some(done) => {
                progress.done.fetch_add(1, Ordering::Relaxed);
                renamed.push(done);
            }
            None => out.rest.push(source.clone()),
        }
    }
    if !renamed.is_empty() {
        log::info!(
            "fast move: {} renamed, {} left to Windows",
            renamed.len(),
            out.rest.len()
        );
        notify(&renamed, dest);
    }
    out.put_back = renamed.iter().filter_map(put_back).collect();
    out
}

/// How to put `done` back.
fn put_back(done: &Renamed) -> Option<PutBack> {
    Some(PutBack {
        now: done.now.to_string_lossy().into_owned(),
        parent: done.from.parent()?.to_path_buf(),
        name: done.from.file_name()?.to_string_lossy().into_owned(),
    })
}

/// Renames `source` into `dest` when it may be done here; `None` leaves it to the engine.
fn rename_one(
    source: &Path,
    dest: &Path,
    target: &Target,
    parents: &mut HashMap<PathBuf, Option<(PathBuf, String)>>,
    names: &mut HashSet<String>,
) -> Option<Renamed> {
    use crate::win_fast_copy::{absent, entry_of};
    let name = source.file_name()?;
    let parent = source.parent()?;
    let name_unique = names.insert(name.to_string_lossy().to_lowercase());
    let (parent_real, parent_volume) = parents
        .entry(parent.to_path_buf())
        .or_insert_with_key(|parent| place(parent))
        .clone()?;
    let real = parent_real.join(name);
    let (stored, attributes) = entry_of(&real)?;
    // Given by its short (8.3) name: the item takes the stored one, which must be free too.
    let stored_unique =
        stored.eq_ignore_ascii_case(name) || names.insert(stored.to_string_lossy().to_lowercase());
    let facts = Facts {
        same_volume: parent_volume == target.volume,
        link: attributes & ATTR_REPARSE_POINT != 0,
        name_free: absent(&target.real.join(name)) && absent(&target.real.join(&stored)),
        name_unique: name_unique && stored_unique,
        into_itself: arke_core::path::is_same_or_inside(&target.real, &real),
        same_folder: key(&parent_real) == key(&target.real),
    };
    if !can_rename(facts) {
        return None;
    }
    let to = target.real.join(&stored);
    if let Err(error) = move_one(&real, &to) {
        log::debug!("fast move: {} left to Windows: {error}", source.display());
        return None;
    }
    Some(Renamed {
        from: parent.join(&stored),
        now: dest.join(&stored),
        is_dir: attributes & ATTR_DIRECTORY != 0,
    })
}

/// Renames `from` to `to` (both `\\?\`): fails if `to` exists, and never copies across volumes.
fn move_one(from: &Path, to: &Path) -> windows::core::Result<()> {
    let from = HSTRING::from(from.as_os_str());
    let to = HSTRING::from(to.as_os_str());
    // SAFETY: both strings are NUL-terminated and outlive the call; no flags means no replacing
    // and no copying.
    unsafe { MoveFileExW(&from, &to, MOVE_FILE_FLAGS(0)) }
}

/// Tells Explorer windows and the desktop about the renames: one by one up to
/// [`NOTIFY_EACH_MAX`], else the destination and the folders the items came from once each.
fn notify(renamed: &[Renamed], dest: &Path) {
    use windows::Win32::UI::Shell::{SHCNE_RENAMEFOLDER, SHCNE_RENAMEITEM, SHCNE_UPDATEDIR};
    if renamed.len() > NOTIFY_EACH_MAX {
        let mut seen = HashSet::new();
        send(SHCNE_UPDATEDIR, dest, None);
        for item in renamed {
            if let Some(parent) = item.from.parent()
                && seen.insert(key(parent))
            {
                send(SHCNE_UPDATEDIR, parent, None);
            }
        }
        return;
    }
    for item in renamed {
        let event = if item.is_dir {
            SHCNE_RENAMEFOLDER
        } else {
            SHCNE_RENAMEITEM
        };
        send(event, &item.from, Some(&item.now));
    }
}

/// `SHChangeNotify` for `event` on `path` (and the new path of a rename).
fn send(event: windows::Win32::UI::Shell::SHCNE_ID, path: &Path, new: Option<&Path>) {
    use windows::Win32::UI::Shell::{SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify};
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (first, second) = (wide(path), new.map(wide));
    // SAFETY: NUL-terminated paths that outlive the call.
    unsafe {
        SHChangeNotify(
            event,
            SHCNF_PATHW | SHCNF_FLUSHNOWAIT,
            Some(first.as_ptr().cast()),
            second.as_ref().map(|p| p.as_ptr().cast()),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::time::{Duration, SystemTime};

    use super::*;

    /// A folder under %TEMP% named for this test and process, with `src` and `dst` in it; only it
    /// is ever touched.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("arke-test-fastmove-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::create_dir_all(dir.join("dst")).unwrap();
            Self(dir)
        }

        fn src(&self, rel: &str) -> PathBuf {
            self.0.join("src").join(rel)
        }

        fn dst(&self, rel: &str) -> PathBuf {
            self.0.join("dst").join(rel)
        }

        fn file(&self, rel: &str, body: &[u8]) -> PathBuf {
            let path = self.src(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The names of the renamed items.
    fn names_of(done: &[PutBack]) -> Vec<OsString> {
        done.iter().map(|p| OsString::from(&p.name)).collect()
    }

    fn all_fine() -> Facts {
        Facts {
            same_volume: true,
            link: false,
            name_free: true,
            name_unique: true,
            into_itself: false,
            same_folder: false,
        }
    }

    fn run(sources: &[PathBuf], dest: &Path) -> (Moved, OpProgress) {
        let progress = OpProgress::default();
        let moved = rename(sources, dest, &progress);
        (moved, progress)
    }

    #[test]
    fn a_plain_same_volume_item_is_renamed() {
        assert!(can_rename(all_fine()));
    }

    #[test]
    fn every_unsafe_fact_leaves_the_item_to_windows() {
        let fine = all_fine();
        let cases = [
            (
                "another volume",
                Facts {
                    same_volume: false,
                    ..fine
                },
            ),
            ("a link", Facts { link: true, ..fine }),
            (
                "name taken",
                Facts {
                    name_free: false,
                    ..fine
                },
            ),
            (
                "name used twice",
                Facts {
                    name_unique: false,
                    ..fine
                },
            ),
            (
                "into itself",
                Facts {
                    into_itself: true,
                    ..fine
                },
            ),
            (
                "same folder",
                Facts {
                    same_folder: true,
                    ..fine
                },
            ),
        ];
        for (what, facts) in cases {
            assert!(!can_rename(facts), "{what}");
        }
    }

    #[test]
    fn sources_holding_each_other_are_found() {
        let p = |s: &str| PathBuf::from(s);
        assert!(!nested(&[p(r"C:\a\x"), p(r"C:\a\y"), p(r"C:\b")]));
        assert!(nested(&[p(r"C:\a"), p(r"C:\a\x")]));
        assert!(
            nested(&[p(r"C:\a\x"), p(r"C:\A")]),
            "either order, any case"
        );
        assert!(nested(&[p(r"C:\a\x"), p(r"c:/a/x/")]), "the same twice");
        assert!(
            !nested(&[p(r"C:\ab"), p(r"C:\a")]),
            "a longer name is not inside"
        );
    }

    #[test]
    fn a_file_and_a_folder_are_moved_with_their_content_and_times() {
        let sb = Scratch::new("tree");
        let file = sb.file("a.txt", b"hello");
        sb.file(r"folder\inner\b.txt", b"inner");
        let old = SystemTime::UNIX_EPOCH + Duration::from_hours(416_666);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let (moved, progress) = run(&[file.clone(), sb.src("folder")], &sb.dst(""));
        assert!(moved.rest.is_empty() && !moved.cancelled);
        assert_eq!(fs::read(sb.dst("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(sb.dst(r"folder\inner\b.txt")).unwrap(), b"inner");
        assert_eq!(
            fs::metadata(sb.dst("a.txt")).unwrap().modified().unwrap(),
            old
        );
        assert!(!file.exists() && !sb.src("folder").exists());
        assert_eq!(progress.done.load(Ordering::Relaxed), 2);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 0, "no bytes move");
        assert_eq!(
            moved.put_back,
            [
                PutBack {
                    now: sb.dst("a.txt").to_string_lossy().into_owned(),
                    parent: sb.0.join("src"),
                    name: "a.txt".into(),
                },
                PutBack {
                    now: sb.dst("folder").to_string_lossy().into_owned(),
                    parent: sb.0.join("src"),
                    name: "folder".into(),
                },
            ]
        );
    }

    #[test]
    fn undo_list_puts_the_items_back() {
        let sb = Scratch::new("undo");
        let file = sb.file("a.txt", b"x");
        let (moved, _) = run(std::slice::from_ref(&file), &sb.dst(""));
        assert_eq!(names_of(&moved.put_back), [OsString::from("a.txt")]);
        for back in &moved.put_back {
            fs::rename(&back.now, back.parent.join(&back.name)).unwrap();
        }
        assert_eq!(fs::read(&file).unwrap(), b"x");
    }

    #[test]
    fn a_name_in_the_destination_in_any_case_is_left_to_windows() {
        let sb = Scratch::new("conflict");
        let taken = sb.file("a.txt", b"new");
        let free = sb.file("b.txt", b"b");
        fs::write(sb.dst("A.TXT"), b"old").unwrap();
        let (moved, _) = run(&[taken.clone(), free], &sb.dst(""));
        assert_eq!(moved.rest, std::slice::from_ref(&taken));
        assert_eq!(moved.put_back.len(), 1);
        assert_eq!(fs::read(sb.dst("A.TXT")).unwrap(), b"old", "untouched");
        assert_eq!(fs::read(&taken).unwrap(), b"new", "still where it was");
        assert!(sb.dst("b.txt").exists());
    }

    #[test]
    fn a_folder_of_the_same_name_is_not_merged() {
        let sb = Scratch::new("merge");
        sb.file(r"docs\new.txt", b"n");
        fs::create_dir_all(sb.dst("Docs")).unwrap();
        fs::write(sb.dst(r"Docs\old.txt"), b"o").unwrap();
        let (moved, _) = run(&[sb.src("docs")], &sb.dst(""));
        assert_eq!(moved.rest, [sb.src("docs")]);
        assert!(sb.src(r"docs\new.txt").exists());
        assert!(!sb.dst(r"Docs\new.txt").exists());
    }

    #[test]
    fn a_move_into_the_folder_the_item_is_in_is_left_to_windows() {
        let sb = Scratch::new("samefolder");
        let file = sb.file("a.txt", b"a");
        let (moved, _) = run(std::slice::from_ref(&file), &sb.src(""));
        assert_eq!(moved.rest, std::slice::from_ref(&file));
        assert!(moved.put_back.is_empty());
        assert!(file.exists());
    }

    #[test]
    fn a_case_only_difference_in_the_same_folder_is_never_a_move() {
        let sb = Scratch::new("caseonly");
        let file = sb.file("a.txt", b"a");
        // The item "a.txt" given as "A.TXT", moved to the folder it is in.
        let given = sb.src("A.TXT");
        let (moved, _) = run(std::slice::from_ref(&given), &sb.src(""));
        assert_eq!(moved.rest, [given]);
        assert_eq!(fs::read(&file).unwrap(), b"a");
        assert!(moved.put_back.is_empty());
    }

    #[test]
    fn an_item_given_in_another_case_moves_under_its_stored_name() {
        let sb = Scratch::new("storedcase");
        sb.file("Mixed.TXT", b"m");
        let given = sb.src("mixed.txt");
        let (moved, _) = run(std::slice::from_ref(&given), &sb.dst(""));
        assert!(moved.rest.is_empty());
        let names: Vec<_> = fs::read_dir(sb.dst(""))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from("Mixed.TXT")]);
        assert_eq!(names_of(&moved.put_back), [OsString::from("Mixed.TXT")]);
    }

    #[test]
    fn a_folder_cannot_move_into_itself() {
        let sb = Scratch::new("itself");
        sb.file(r"outer\inner\a.txt", b"a");
        let (moved, _) = run(&[sb.src("outer")], &sb.src(r"outer\inner"));
        assert_eq!(moved.rest, [sb.src("outer")]);
        assert!(sb.src(r"outer\inner\a.txt").exists());
        let (moved, _) = run(&[sb.src("outer")], &sb.src("outer"));
        assert_eq!(moved.rest, [sb.src("outer")]);
    }

    #[test]
    fn two_sources_of_one_name_leave_the_second_to_windows() {
        let sb = Scratch::new("twice");
        let first = sb.file(r"one\x.txt", b"1");
        let second = sb.file(r"two\X.txt", b"2");
        let (moved, _) = run(&[first, second.clone()], &sb.dst(""));
        assert_eq!(moved.rest, std::slice::from_ref(&second));
        assert_eq!(moved.put_back.len(), 1);
        assert!(second.exists());
    }

    #[test]
    fn sources_inside_each_other_all_go_to_windows() {
        let sb = Scratch::new("nested");
        let inner = sb.file(r"outer\a.txt", b"a");
        let sources = [sb.src("outer"), inner];
        let (moved, _) = run(&sources, &sb.dst(""));
        assert_eq!(moved.rest, sources);
        assert!(moved.put_back.is_empty());
        assert!(sb.src(r"outer\a.txt").exists());
    }

    #[test]
    fn a_missing_source_goes_to_windows() {
        let sb = Scratch::new("missing");
        let gone = sb.src("gone.txt");
        let (moved, _) = run(std::slice::from_ref(&gone), &sb.dst(""));
        assert_eq!(moved.rest, [gone]);
    }

    #[test]
    fn a_missing_destination_sends_everything_to_windows() {
        let sb = Scratch::new("nodest");
        let file = sb.file("a.txt", b"a");
        let (moved, _) = run(std::slice::from_ref(&file), &sb.dst("nowhere"));
        assert_eq!(moved.rest, std::slice::from_ref(&file));
        assert!(file.exists());
    }

    #[test]
    fn a_folder_held_open_without_sharing_delete_is_left_to_windows() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        let sb = Scratch::new("inuse");
        let file = sb.file(r"locked\a.txt", b"a");
        // The folder itself held open without sharing delete: it can't be renamed.
        let _held = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .share_mode(1 | 2)
            .open(sb.src("locked"))
            .unwrap();
        let (moved, _) = run(&[sb.src("locked")], &sb.dst(""));
        assert_eq!(moved.rest, [sb.src("locked")]);
        assert!(moved.put_back.is_empty());
        assert!(file.exists());
    }

    #[test]
    fn a_junction_is_left_to_windows() {
        let sb = Scratch::new("junction");
        let target = sb.file(r"target\t.txt", b"t");
        let link = sb.src("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(target.parent().unwrap())
            .output();
        if !made.is_ok_and(|o| o.status.success()) {
            return;
        }
        let (moved, _) = run(std::slice::from_ref(&link), &sb.dst(""));
        assert_eq!(moved.rest, std::slice::from_ref(&link));
        assert!(!sb.dst("link").exists());
        assert!(link.exists());
        // Removing the junction itself, never what it points to.
        let _ = fs::remove_dir(&link);
        assert!(target.exists());
    }

    #[test]
    fn a_symlink_is_left_to_windows() {
        let sb = Scratch::new("symlink");
        let target = sb.file("t.txt", b"t");
        let link = sb.src("link.txt");
        // Symlinks need Developer Mode or elevation; without them there is nothing to test.
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }
        let (moved, _) = run(std::slice::from_ref(&link), &sb.dst(""));
        assert_eq!(moved.rest, [link]);
        assert!(!sb.dst("link.txt").exists());
    }

    #[test]
    fn cancel_before_the_start_moves_nothing() {
        let sb = Scratch::new("cancel");
        let file = sb.file("a.txt", b"a");
        let progress = OpProgress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let moved = rename(std::slice::from_ref(&file), &sb.dst(""), &progress);
        assert!(moved.cancelled);
        assert!(moved.rest.is_empty() && moved.put_back.is_empty());
        assert!(file.exists());
    }

    #[test]
    fn many_items_are_all_renamed() {
        let sb = Scratch::new("many");
        let sources: Vec<PathBuf> = (0..NOTIFY_EACH_MAX + 6)
            .map(|i| sb.file(&format!("f{i:03}.txt"), b"x"))
            .collect();
        let (moved, progress) = run(&sources, &sb.dst(""));
        assert!(moved.rest.is_empty());
        assert_eq!(moved.put_back.len(), sources.len());
        assert_eq!(progress.done.load(Ordering::Relaxed), sources.len() as u64);
        assert_eq!(fs::read_dir(sb.dst("")).unwrap().count(), sources.len());
        assert_eq!(fs::read_dir(sb.src("")).unwrap().count(), 0);
    }
}
