//! GPUI tests for "Compress" (#409): the archive of several items is named after their folder,
//! and a finished archive is selected and renamed unless the user went elsewhere.
//!
//! Compressing reads and writes a real disk, so the FakeFs folder is a real folder under %TEMP%.
//! The archive is made on disk by the app; the test adds it to the FakeFs listing and sends the
//! watcher event, as the file system would.

use super::*;

/// `%TEMP%\arke-compress-<tag>-<random>\Report` with `a.txt`, `b.txt` and `c.txt`. The guard
/// deletes the folder when dropped.
fn sandbox(tag: &str) -> (PathBuf, tempfile::TempDir) {
    let guard = tempfile::Builder::new()
        .prefix(&format!("arke-compress-{tag}-"))
        .tempdir()
        .unwrap();
    let dir = guard.path().join("Report");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), name).unwrap();
    }
    (dir, guard)
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
    wait_until(cx, view, "compressed", |v| v.extract_jobs.is_empty());
}

/// The archive shows up in the folder listing.
fn list(fs: &FakeFs, view: &Entity<Explorer>, cx: &mut VisualTestContext, dir: &Path, name: &str) {
    fs.add_file(dir, name, 100);
    fs.emit_changed(dir, &[name]);
    wait_until(cx, view, "archive listed", |v| {
        view_names(v).iter().any(|n| n == name)
    });
    frames(cx);
}

/// A few frames, so anything that waits for the listing has had its turn.
fn frames(cx: &mut VisualTestContext) {
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
    let (dir, _guard) = sandbox("folder-name");
    let (_fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    assert!(dir.join("Report.zip").is_file());
    assert!(!dir.join("a.zip").exists());
    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
}

#[gpui::test]
fn a_taken_folder_name_is_numbered(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("taken");
    std::fs::write(dir.join("Report.zip"), "old").unwrap();
    let (_fs, view, cx) = open(cx, &dir, vec![file("Report.zip", 3)]);
    compress(&view, cx, &["a.txt", "c.txt"]);
    assert!(dir.join("Report (2).zip").is_file());
    assert_eq!(
        std::fs::read_to_string(dir.join("Report.zip")).unwrap(),
        "old"
    );
    assert_eq!(selected(&view, cx), ["Report (2).zip"]);
}

#[gpui::test]
fn one_item_keeps_its_own_name(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("single");
    let (_fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["b.txt"]);
    assert!(dir.join("b.zip").is_file());
    assert!(!dir.join("Report.zip").exists());
}

#[gpui::test]
fn the_new_archive_is_selected_and_renamed(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("rename");
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
}

#[gpui::test]
fn typing_in_the_search_box_is_left_alone(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("search-box");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    cx.dispatch_action(FocusFilter);
    cx.run_until_parked();
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    assert!(cx.update(|window, cx| view.read(cx).filter_focused_for_test(window, cx)));
}

#[gpui::test]
fn typing_in_the_path_bar_is_left_alone(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("path-bar");
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    compress(&view, cx, &["a.txt", "b.txt"]);
    cx.dispatch_action(FocusPathBar);
    cx.run_until_parked();
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    assert!(view.update(cx, |v, _| v.editing_path));
}

#[gpui::test]
fn another_folder_is_left_alone(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("away");
    let other = dir.parent().unwrap().join("Other");
    std::fs::create_dir_all(&other).unwrap();
    let (fs, view, cx) = open(cx, &dir, Vec::new());
    fs.insert_dir(&other, vec![file("z.txt", 1)]);
    let other_str = other.to_string_lossy().into_owned();
    // The view leaves before the archive is done.
    start_compress(&view, cx, &["a.txt", "b.txt"]);
    view.update(cx, |v, cx| v.navigate(other.clone(), NavKind::New, cx));
    wait_until(cx, &view, "other folder", loaded(&other_str));
    wait_until(cx, &view, "compressed", |v| v.extract_jobs.is_empty());
    assert!(dir.join("Report.zip").is_file());
    // The archive lands in the folder left behind; this view isn't showing it.
    fs.add_file(&dir, "Report.zip", 100);
    fs.emit_changed(&dir, &["Report.zip"]);
    frames(cx);
    assert_eq!(renaming(&view, cx), None);
    assert!(selected(&view, cx).is_empty());
    assert_eq!(view.update(cx, |v, _| view_names(v)), ["z.txt"]);
    // Back in the folder: the archive is listed, and nothing waited to rename it.
    let dir_str = dir.to_string_lossy().into_owned();
    view.update(cx, |v, cx| v.navigate(dir.clone(), NavKind::New, cx));
    wait_until(cx, &view, "back", loaded(&dir_str));
    wait_until(cx, &view, "archive listed", |v| {
        view_names(v).iter().any(|n| n == "Report.zip")
    });
    frames(cx);
    assert_eq!(renaming(&view, cx), None);
}

#[gpui::test]
fn a_cancelled_compression_changes_nothing(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("cancelled");
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
}

#[gpui::test]
fn a_failed_compression_changes_nothing(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("failed");
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
}

#[gpui::test]
fn the_sidebar_keeps_the_keyboard(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("sidebar");
    let fs = FakeFs::new();
    fs.insert_dir(
        &dir,
        vec![file("a.txt", 5), file("b.txt", 5), file("c.txt", 5)],
    );
    let dir_str = dir.to_string_lossy().into_owned();
    let (ws, cx) = setup_workspace(cx, Arc::clone(&fs), &dir_str);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(&dir_str));
    wait_watcher(&fs, &dir);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    compress(&view, cx, &["a.txt", "b.txt"]);
    // F6 moves the keyboard from the list to the sidebar while the archive is being listed.
    cx.simulate_keystrokes("f6");
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
    let region = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| ws.read(cx).region_for_test(window, cx))
    };
    assert_eq!(region(cx), "sidebar");
    list(&fs, &view, cx, &dir, "Report.zip");
    assert_eq!(renaming(&view, cx), None);
    assert_eq!(selected(&view, cx), ["Report.zip"]);
    assert_eq!(region(cx), "sidebar", "focus stays where the user put it");
}

/// The menu says the name the command will use (#409).
#[gpui::test]
fn the_menu_names_the_archive_as_the_command_will(cx: &mut TestAppContext) {
    let (dir, _guard) = sandbox("label");
    let (_fs, view, cx) = open(cx, &dir, Vec::new());
    let label = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        menu_labels(view, cx)
            .unwrap_or_default()
            .into_iter()
            .find(|l| l.contains(".zip"))
            .expect("a Compress row")
    };

    view.update(cx, |v, _| {
        v.selection
            .select_names(["a.txt", "b.txt"].map(Arc::<str>::from));
    });
    right_click(&view, cx, Some(0));
    assert!(label(&view, cx).contains("Report.zip"));

    view.update(cx, |v, _| v.selection.select_name(Arc::from("b.txt")));
    right_click(&view, cx, Some(1));
    assert!(label(&view, cx).contains("b.zip"));
}
