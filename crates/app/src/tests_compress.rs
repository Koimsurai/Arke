//! GPUI tests for "Compress" (#409): the archive of several items is named after their folder,
//! and a finished archive is selected and renamed unless the user went elsewhere.
//!
//! Compressing reads and writes a real disk, so the FakeFs folder is a real folder under %TEMP%.
//! The archive is made on disk by the app; the test adds it to the FakeFs listing and sends the
//! watcher event, as the file system would.

use super::*;

/// `%TEMP%\arke-compress-<tag>-<pid>\Report` with `a.txt`, `b.txt` and `c.txt`.
fn sandbox(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("arke-compress-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("Report");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), name).unwrap();
    }
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

/// The folder opened in a view, listing the three files and `extra`.
fn open<'a>(
    cx: &'a mut TestAppContext,
    dir: &Path,
    extra: Vec<Entry>,
) -> (Arc<FakeFs>, Entity<Explorer>, &'a mut VisualTestContext) {
    let fs = FakeFs::new();
    let mut entries = vec![file("a.txt", 5), file("b.txt", 5), file("c.txt", 5)];
    entries.extend(extra);
    fs.insert_dir(dir, entries);
    let dir_str = dir.to_string_lossy().into_owned();
    let (view, cx) = setup(
        cx,
        Arc::clone(&fs),
        Arc::new(FakeShell::default()),
        &dir_str,
    );
    wait_until(cx, &view, "load", loaded(&dir_str));
    wait_watcher(&fs, dir);
    (fs, view, cx)
}

/// Selects `names` and starts compressing them from the context menu of the first one.
fn start_compress(view: &Entity<Explorer>, cx: &mut VisualTestContext, names: &[&str]) {
    let row = view
        .update(cx, |v, _| {
            v.selection
                .select_names(names.iter().map(|n| Arc::<str>::from(*n)));
            view_names(v).iter().position(|n| n == names[0])
        })
        .expect("first item is listed");
    right_click(view, cx, Some(row));
    assert_eq!(view.update(cx, |v, _| v.selection.len()), names.len());
    run_command(view, cx, MenuCommand::Compress);
}

/// [`start_compress`], then waits until the job is over.
fn compress(view: &Entity<Explorer>, cx: &mut VisualTestContext, names: &[&str]) {
    start_compress(view, cx, names);
    wait_until(cx, view, "compressed", |v| v.extract_job.is_none());
}

/// The archive shows up in the folder listing.
fn list(fs: &FakeFs, view: &Entity<Explorer>, cx: &mut VisualTestContext, dir: &Path, name: &str) {
    fs.add_file(dir, name, 100);
    fs.emit_changed(dir, &[name]);
    wait_until(cx, view, "archive listed", |v| {
        view_names(v).iter().any(|n| n == name)
    });
    // A few more frames, so anything that waits for the listing has had its turn.
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn renaming(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Option<String> {
    view.update(cx, |v, _| v.renaming.as_ref().map(|r| r.name.to_string()))
}

fn selected(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<String> {
    view.update(cx, |v, _| {
        let mut names: Vec<String> = v.selection.names().map(ToString::to_string).collect();
        names.sort();
        names
    })
}

#[gpui::test]
fn several_items_are_named_after_their_folder(cx: &mut TestAppContext) {
    let dir = sandbox("folder-name");
    let (_fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    assert!(dir.join("Report.zip").is_file());
    assert!(!dir.join("a.zip").exists());
    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    cleanup(&dir);
}

#[gpui::test]
fn a_taken_folder_name_is_numbered(cx: &mut TestAppContext) {
    let dir = sandbox("taken");
    std::fs::write(dir.join("Report.zip"), "old").unwrap();
    let (_fs, view, cx) = open(cx, &dir, vec![file("Report.zip", 3)]);
    compress(&view, cx, &["a.txt", "c.txt"]);
    assert!(dir.join("Report (2).zip").is_file());
    assert_eq!(
        std::fs::read_to_string(dir.join("Report.zip")).unwrap(),
        "old"
    );
    assert_eq!(selected(&view, cx), ["Report (2).zip"]);
    cleanup(&dir);
}

#[gpui::test]
fn one_item_keeps_its_own_name(cx: &mut TestAppContext) {
    let dir = sandbox("single");
    let (_fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["b.txt"]);
    assert!(dir.join("b.zip").is_file());
    assert!(!dir.join("Report.zip").exists());
    cleanup(&dir);
}

#[gpui::test]
fn the_new_archive_is_selected_and_renamed(cx: &mut TestAppContext) {
    let dir = sandbox("rename");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    // Not listed yet: nothing to rename.
    assert_eq!(renaming(&view, cx), None);
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx).as_deref(), Some("Report.zip"));
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    // The rename box has the keyboard and takes typing.
    cx.simulate_input("X");
    assert!(view.update(cx, |v, cx| {
        v.renaming
            .as_ref()
            .is_some_and(|r| r.input.read(cx).text().contains('X'))
    }));
    cleanup(&dir);
}

#[gpui::test]
fn typing_in_the_search_box_is_left_alone(cx: &mut TestAppContext) {
    let dir = sandbox("search-box");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    cx.dispatch_action(FocusFilter);
    cx.run_until_parked();
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    assert!(cx.update(|window, cx| view.read(cx).filter_focused_for_test(window, cx)));
    cleanup(&dir);
}

#[gpui::test]
fn typing_in_the_path_bar_is_left_alone(cx: &mut TestAppContext) {
    let dir = sandbox("path-bar");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    cx.dispatch_action(FocusPathBar);
    cx.run_until_parked();
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    assert!(view.update(cx, |v, _| v.editing_path));
    cleanup(&dir);
}

#[gpui::test]
fn another_folder_is_left_alone(cx: &mut TestAppContext) {
    let dir = sandbox("away");
    let other = dir.parent().unwrap().join("Other");
    std::fs::create_dir_all(&other).unwrap();
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    fs.insert_dir(&other, vec![file("z.txt", 1)]);
    let other_str = other.to_string_lossy().into_owned();
    // The view leaves before the archive is done.
    start_compress(&view, cx, &["a.txt", "b.txt"]);
    view.update(cx, |v, cx| v.navigate(other.clone(), NavKind::New, cx));
    wait_until(cx, &view, "other folder", loaded(&other_str));
    wait_until(cx, &view, "compressed", |v| v.extract_job.is_none());
    assert!(dir.join("Report.zip").is_file());
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert!(selected(&view, cx).is_empty());
    assert_eq!(view.update(cx, |v, _| view_names(v)), ["z.txt"]);
    cleanup(&dir);
}

#[gpui::test]
fn a_cancelled_compression_changes_nothing(cx: &mut TestAppContext) {
    let dir = sandbox("cancelled");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    view.update(cx, |v, _| v.selection.select_name(Arc::from("a.txt")));
    let summary = arke_core::archive::CompressSummary {
        cancelled: true,
        ..Default::default()
    };
    view.update(cx, |v, cx| {
        v.compress_done_for_test(Ok(summary), "Report.zip", dir.join("Report.zip"), cx);
    });
    cx.run_until_parked();
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["a.txt"]);
    assert_eq!(
        view.update(cx, |v, _| v.notice.clone()).as_deref(),
        Some(&*t!("compress.cancelled"))
    );
    cleanup(&dir);
}

#[gpui::test]
fn a_failed_compression_changes_nothing(cx: &mut TestAppContext) {
    let dir = sandbox("failed");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    view.update(cx, |v, _| v.selection.select_name(Arc::from("a.txt")));
    let error = arke_core::archive::ArchiveError::Exists {
        path: dir.join("Report.zip"),
    };
    view.update(cx, |v, cx| {
        v.compress_done_for_test(Err(error), "Report.zip", dir.join("Report.zip"), cx);
    });
    cx.run_until_parked();
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["a.txt"]);
    assert!(view.update(cx, |v, _| v.error.is_some()));
    cleanup(&dir);
}
