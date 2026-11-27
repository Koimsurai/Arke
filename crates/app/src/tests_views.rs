//! GPUI tests for icon sizes (`explorer_icons.rs`) and the Details view tree
//! (`explorer_tree.rs`).

use gpui::{Modifiers, ScrollDelta, ScrollEvent, point, px};
use std::sync::Arc;

use super::*;
use crate::explorer::{IconSize, ViewMode};
use crate::folder_views::FolderViewStore;
use crate::media::MediaKey;
use crate::settings::SettingsStore;

fn sep(parts: &[&str]) -> String {
    parts.join(&arke_core::tree::SEP.to_string())
}

fn icons_of(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> (ViewMode, IconSize) {
    view.update(cx, |v, _| (v.view_mode, v.icon_size))
}

fn ctrl_wheel(cx: &mut VisualTestContext, view: &Entity<Explorer>, notches: f32) {
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    cx.simulate_event(ScrollEvent {
        position: list.origin + point(px(80.), px(60.)),
        delta: ScrollDelta::Lines(point(0., notches)),
        modifiers: Modifiers::control(),
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
}

/// Ctrl+Shift+1..4 pick Explorer's four sizes, Ctrl+wheel steps through them and on into Details
/// (and back), and the size is remembered for the folder.
#[gpui::test]
fn icon_sizes_by_keys_and_ctrl_wheel(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\many", 300);
    fs.insert_dir(r"C:\other", vec![file("x.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\many");
    wait_until(cx, &view, "load", loaded(r"C:\many"));

    cx.simulate_keystrokes("ctrl-shift-1");
    assert_eq!(
        icons_of(&view, cx),
        (ViewMode::Icons, IconSize::EXTRA_LARGE)
    );
    cx.simulate_keystrokes("ctrl-shift-3");
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::MEDIUM));
    cx.simulate_keystrokes("ctrl-shift-4");
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::SMALL));
    cx.simulate_keystrokes("ctrl-shift-2");
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::LARGE));
    // Wider tiles mean fewer columns.
    cx.run_until_parked();
    let large_cols = view.update(cx, |v, _| v.grid_cols);

    // Wheel up: one stop larger, and the list doesn't scroll.
    let step_up = IconSize::LADDER[IconSize::LADDER
        .iter()
        .position(|s| *s == IconSize::LARGE)
        .unwrap()
        + 1];
    ctrl_wheel(cx, &view, 1.);
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, step_up));
    assert!(view.update(cx, |v, _| v.grid_cols) < large_cols);
    assert_eq!(view.update(cx, |v, _| v.list_scroll_for_test().y), px(0.));
    // A fast flick goes all the way, and no further.
    ctrl_wheel(cx, &view, 20.);
    assert_eq!(
        icons_of(&view, cx),
        (ViewMode::Icons, IconSize::LARGEST),
        "the largest"
    );
    ctrl_wheel(cx, &view, 1.);
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::LARGEST));
    // Wheel down past small icons lands in Details, and up again comes back to small icons.
    cx.simulate_keystrokes("ctrl-shift-3");
    ctrl_wheel(cx, &view, -2.);
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::SMALL));
    ctrl_wheel(cx, &view, -1.);
    assert_eq!(view.update(cx, |v, _| v.view_mode), ViewMode::Details);
    ctrl_wheel(cx, &view, 1.);
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::SMALL));
    // Touchpads send small precise amounts; they add up to a step.
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    for _ in 0..3 {
        cx.simulate_event(ScrollEvent {
            position: list.origin + point(px(80.), px(60.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(20.))),
            modifiers: Modifiers::control(),
            ..ScrollEvent::default()
        });
    }
    cx.run_until_parked();
    let between = IconSize::LADDER[1];
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, between));

    // Remembered for this folder, sizes between Explorer's too: another folder opens with the
    // defaults, coming back restores.
    let remembered = cx.update(|_, cx| {
        FolderViewStore::global(cx)
            .read(cx)
            .peek(Path::new(r"C:\many"))
    });
    assert_eq!(remembered.map(|v| v.icons), Some(between));
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\other"), NavKind::New, cx);
    });
    wait_until(cx, &view, "other", loaded(r"C:\other"));
    assert_eq!(icons_of(&view, cx), (ViewMode::Details, IconSize::LARGE));
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\many"), NavKind::New, cx);
    });
    wait_until(cx, &view, "back", loaded(r"C:\many"));
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, between));
}

/// The view options flyout: opens from the view switcher, switches the layout, its slider sets any
/// icon size (remembered for the folder), its switches change the settings, and Esc closes it.
#[gpui::test]
fn view_options_flyout(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\many", 50);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\many");
    wait_until(cx, &view, "load", loaded(r"C:\many"));
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("{id} should be drawn"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    };
    click(cx, "view-options");
    assert!(view.update(cx, |v, _| v.view_options_open_for_test()));
    // The tree switch is there in Details, and it is the setting.
    click(cx, "vo-details-tree");
    assert!(cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.details_tree));
    // Icons from the flyout, then a slider position between two of Explorer's sizes.
    click(cx, "vo-icons");
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::LARGE));
    assert!(
        view.update(cx, |v, _| v.view_options_open_for_test()),
        "stays open"
    );
    view.update(cx, |v, cx| v.slide_icon_size_for_test(0.5, cx));
    let half = IconSize::from_fraction(0.5);
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, half));
    assert_eq!(half, IconSize::from_px(152.));
    let remembered = cx.update(|_, cx| {
        FolderViewStore::global(cx)
            .read(cx)
            .peek(Path::new(r"C:\many"))
    });
    assert_eq!(remembered.map(|v| v.icons), Some(half));
    // The keys still land on Explorer's sizes.
    cx.simulate_keystrokes("ctrl-shift-4");
    assert_eq!(icons_of(&view, cx), (ViewMode::Icons, IconSize::SMALL));
    // Esc closes it first, without clearing the selection.
    cx.simulate_keystrokes("home escape");
    assert!(!view.update(cx, |v, _| v.view_options_open_for_test()));
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 1);
}

/// Thumbnails are decoded at a size that matches the icon size.
#[gpui::test]
fn thumbnails_follow_the_icon_size(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\pics", vec![file("wide.png", 100)]);
    fs.set_content(r"C:\pics\wide.png", png_bytes(800, 400));
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\pics");
    wait_until(cx, &view, "load", loaded(r"C:\pics"));
    let width_at = |view: &Entity<Explorer>, cx: &mut VisualTestContext, keys: &str| {
        cx.simulate_keystrokes(keys);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let width = view.update(cx, |v, cx| {
                let px = v.thumbnail_px();
                let entry = v.view()?.first()?.clone();
                let key = MediaKey::sized(Path::new(r"C:\pics"), &entry, Some(px))?;
                v.media.read(cx).get(&key).map(|img| img.size(0).width.0)
            });
            if let Some(width) = width {
                return width;
            }
            assert!(Instant::now() < deadline, "no thumbnail for {keys}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let scale = cx.update(|window, _| window.scale_factor());
    let expect =
        |logical: f32| i32::try_from(arke_core::thumbs::source_size(logical, scale)).unwrap();
    assert_eq!(width_at(&view, cx, "ctrl-shift-4"), expect(48.));
    assert_eq!(width_at(&view, cx, "ctrl-shift-1"), expect(192.));
    assert!(expect(192.) > expect(48.));
}

fn tree_fs() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("alpha"), dir("beta"), file("top.txt", 1)]);
    fs.insert_dir(r"C:\t\alpha", vec![dir("inner"), file("a1.txt", 1)]);
    fs.insert_dir(r"C:\t\alpha\inner", vec![file("deep.md", 1)]);
    fs.insert_dir(r"C:\t\beta", vec![file("b1.txt", 1)]);
    fs
}

fn setup_tree(
    cx: &mut TestAppContext,
    fs: Arc<FakeFs>,
    shell: Arc<FakeShell>,
) -> (Entity<Explorer>, &mut VisualTestContext) {
    let (view, cx) = setup(cx, fs, shell, r"C:\t");
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.details_tree = true, cx));
    });
    cx.run_until_parked();
    (view, cx)
}

/// → expands the cursor folder (listed off the UI thread), → again steps into it, ← goes back to
/// the folder and collapses it. Nested rows carry their path relative to the folder shown.
#[gpui::test]
fn tree_expands_and_collapses_with_arrows(cx: &mut TestAppContext) {
    let (view, cx) = setup_tree(cx, tree_fs(), Arc::default());
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        ["alpha", "beta", "top.txt"]
    );
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        [
            "alpha".to_string(),
            sep(&["alpha", "inner"]),
            sep(&["alpha", "a1.txt"]),
            "beta".into(),
            "top.txt".into(),
        ]
    );
    // Right again: onto the first child; right on it expands it too.
    cx.simulate_keystrokes("right");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)),
        Some(sep(&["alpha", "inner"]))
    );
    cx.simulate_keystrokes("right");
    wait_until(cx, &view, "inner listed", |v| {
        view_names(v).contains(&sep(&["alpha", "inner", "deep.md"]))
    });
    // The nested file has its own full path for opening, menus and the preview.
    cx.simulate_keystrokes("down");
    let path = view.update(cx, |v, _| Some(v.current_path()?.join(selected_name(v)?)));
    assert_eq!(path, Some(PathBuf::from(r"C:\t\alpha\inner\deep.md")));
    // Left on a nested file goes to its folder; left there collapses it.
    cx.simulate_keystrokes("left");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)),
        Some(sep(&["alpha", "inner"]))
    );
    cx.simulate_keystrokes("left");
    assert!(
        !view
            .update(cx, |v, _| view_names(v))
            .contains(&sep(&["alpha", "inner", "deep.md"]))
    );
    // Collapsing alpha with a child selected moves the selection to alpha.
    cx.simulate_keystrokes("down");
    view.update(cx, |v, cx| v.collapse_for_test("alpha", cx));
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["alpha"]);
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        ["alpha", "beta", "top.txt"]
    );
    assert!(view.update(cx, |v, _| v.tree_expanded().is_empty()));
}

/// #260: a file inside an expanded folder passes drops to that folder (where it sits), not to the
/// folder shown; only folders draw themselves as the target. A file of the folder shown takes no
/// drop of its own (the list's folder does).
#[gpui::test]
fn files_in_an_expanded_folder_drop_into_it(cx: &mut TestAppContext) {
    let (view, cx) = setup_tree(cx, tree_fs(), Arc::default());
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);
    // alpha, alpha\inner, alpha\a1.txt, beta, top.txt
    let mut drop = |ix| view.update(cx, |v, _| v.drop_dir_for_test(ix));
    assert_eq!(drop(0), Some((PathBuf::from(r"C:\t\alpha"), true)));
    assert_eq!(drop(1), Some((PathBuf::from(r"C:\t\alpha\inner"), true)));
    assert_eq!(drop(2), Some((PathBuf::from(r"C:\t\alpha"), false)));
    assert_eq!(drop(4), None);
}

/// Expanded folders follow the sort and stay current through their own watcher; a folder that
/// disappears takes its expansion with it.
#[gpui::test]
fn tree_follows_sort_and_watcher(cx: &mut TestAppContext) {
    let fs = tree_fs();
    let (view, cx) = setup_tree(cx, Arc::clone(&fs), Arc::default());
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);
    wait_watcher(&fs, Path::new(r"C:\t\alpha"));

    // A new file in the expanded folder shows up in place.
    fs.add_file(Path::new(r"C:\t\alpha"), "a0.txt", 1);
    fs.emit_changed(Path::new(r"C:\t\alpha"), &["a0.txt"]);
    wait_until(cx, &view, "watcher delta", |v| {
        view_names(v).contains(&sep(&["alpha", "a0.txt"]))
    });
    assert_eq!(
        view.update(cx, |v, _| view_names(v))[..4],
        [
            "alpha".to_string(),
            sep(&["alpha", "inner"]),
            sep(&["alpha", "a0.txt"]),
            sep(&["alpha", "a1.txt"]),
        ]
    );

    // Name descending, folders always first (#499): children reverse too, and folders lead the
    // files at every level.
    view.update(cx, |v, cx| {
        v.set_sort(
            SortSpec::by(SortColumn::Name, false, false).with_always_first(true),
            cx,
        );
    });
    wait_until(cx, &view, "resorted", |v| {
        view_names(v).first().map(String::as_str) == Some("beta")
            && view_names(v).get(4) == Some(&sep(&["alpha", "a0.txt"]))
    });
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        [
            "beta".to_string(),
            "alpha".into(),
            sep(&["alpha", "inner"]),
            sep(&["alpha", "a1.txt"]),
            sep(&["alpha", "a0.txt"]),
            "top.txt".into(),
        ]
    );

    // Name descending, folders before files (File Explorer): they trail the files at every level.
    view.update(cx, |v, cx| {
        v.set_sort(SortSpec::by(SortColumn::Name, false, false), cx);
    });
    wait_until(cx, &view, "resorted again", |v| {
        view_names(v).first().map(String::as_str) == Some("top.txt")
            && view_names(v).get(4) == Some(&sep(&["alpha", "a0.txt"]))
    });
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        [
            "top.txt".to_string(),
            "beta".into(),
            "alpha".into(),
            sep(&["alpha", "a1.txt"]),
            sep(&["alpha", "a0.txt"]),
            sep(&["alpha", "inner"]),
        ]
    );

    // The expanded folder is deleted: its rows and its expansion go away (still descending, so
    // the folder left trails the file).
    wait_watcher(&fs, Path::new(r"C:\t"));
    fs.remove(Path::new(r"C:\t"), "alpha");
    fs.emit_changed(Path::new(r"C:\t"), &["alpha"]);
    wait_until(cx, &view, "alpha gone", |v| {
        view_names(v) == ["top.txt", "beta"] && v.tree_expanded().is_empty()
    });
}

/// Rename, selection and the context menu work on nested rows; turning the setting off brings
/// back the flat list.
#[gpui::test]
fn tree_rows_rename_and_setting_off(cx: &mut TestAppContext) {
    let fs = tree_fs();
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup_tree(cx, Arc::clone(&fs), Arc::clone(&shell));
    // beta: expand, step onto b1.txt, rename it.
    cx.simulate_keystrokes("home down right");
    wait_until(cx, &view, "beta listed", |v| view_names(v).len() == 4);
    wait_watcher(&fs, Path::new(r"C:\t\beta"));
    cx.simulate_keystrokes("right f2");
    let text = view.update(cx, |v, cx| {
        v.renaming
            .as_ref()
            .map(|r| r.input.read(cx).text().to_owned())
    });
    assert_eq!(
        text.as_deref(),
        Some("b1.txt"),
        "only the file name is edited"
    );
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("renamed.txt");
    cx.simulate_keystrokes("enter");
    // The rename is handed to the shell asynchronously; slow CI runners need a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while shell.ops().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "rename never reached the shell"
        );
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        shell.ops(),
        [FileOp::Rename {
            path: PathBuf::from(r"C:\t\beta\b1.txt"),
            new_name: "renamed.txt".into(),
        }]
    );
    wait_until(cx, &view, "renamed in place", |v| {
        selected_name(v) == Some(sep(&["beta", "renamed.txt"]))
    });
    // Ctrl+A covers nested rows; the selection summary counts them.
    cx.simulate_keystrokes("ctrl-a");
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 4);

    // Setting off: flat list, no nested selection left behind.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.details_tree = false, cx));
    });
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        ["alpha", "beta", "top.txt"]
    );
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 3);
    assert!(!view.update(cx, |v, _| v.tree_lists(Path::new(r"C:\t\beta"))));
}

/// A folder that can't be listed folds back up with a message; the list keeps working.
#[gpui::test]
fn tree_folder_that_cannot_be_listed(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("locked"), file("x.txt", 1)]);
    let (view, cx) = setup_tree(cx, fs, Arc::default());
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "failure reported", |v| v.error.is_some());
    assert!(view.update(cx, |v, _| v.tree_expanded().is_empty()));
    assert_eq!(view.update(cx, |v, _| view_names(v)), ["locked", "x.txt"]);
}

/// Nested tree rows are full paths everywhere a path is built from the folder shown plus a row
/// name: the shelf (Ctrl+Shift+A), drags onto it, and redo of an operation on a nested item.
#[gpui::test]
fn tree_rows_feed_the_shelf_drags_and_redo(cx: &mut TestAppContext) {
    let fs = tree_fs();
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup_tree(cx, Arc::clone(&fs), Arc::clone(&shell));
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);
    wait_watcher(&fs, Path::new(r"C:\t\alpha"));
    // Onto alpha\inner, a nested folder.
    cx.simulate_keystrokes("right");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)),
        Some(sep(&["alpha", "inner"]))
    );
    let inner = (PathBuf::from(r"C:\t\alpha\inner"), true);
    cx.simulate_keystrokes("ctrl-shift-a");
    let on_shelf: Vec<(PathBuf, bool)> = cx.update(|_, cx| {
        crate::shelf::ShelfStore::global(cx)
            .read(cx)
            .shelf()
            .items()
            .iter()
            .map(|i| (i.path.clone(), i.is_dir))
            .collect()
    });
    assert_eq!(on_shelf, std::slice::from_ref(&inner));
    assert_eq!(view.update(cx, |v, _| v.drag_items(1)), [inner]);

    // Recycle alpha\a1.txt, undo, redo: the redo recycles the same nested file.
    cx.simulate_keystrokes("down delete");
    wait_until(cx, &view, "recycled", |v| v.ops_running == 0);
    let recycle = FileOp::Delete {
        paths: vec![PathBuf::from(r"C:\t\alpha\a1.txt")],
        recycle: true,
    };
    assert_eq!(shell.ops(), std::slice::from_ref(&recycle));
    cx.simulate_keystrokes("ctrl-z");
    wait_shell("undo", || shell.undone.lock().len() == 1);
    wait_until(cx, &view, "undo done", |v| v.ops_running == 0);
    cx.simulate_keystrokes("ctrl-y");
    wait_shell("redo", || shell.ops().len() == 2);
    wait_until(cx, &view, "redo done", |v| v.ops_running == 0);
    assert_eq!(shell.ops(), [recycle.clone(), recycle]);
}

/// Long tree with the cursor far below: clicking a chevron keeps its row where it was on screen
/// instead of jumping to the cursor, and rows added above the screen by the watcher don't push
/// the view down. Only keyboard moves scroll to the cursor.
#[gpui::test]
fn tree_keeps_rows_in_place_while_expanding_and_collapsing(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let folders: Vec<String> = (0..6).map(|n| format!("f{n}")).collect();
    let mut top: Vec<Entry> = folders.iter().map(|f| dir(f)).collect();
    top.push(file("zz-last.txt", 1));
    fs.insert_dir(r"C:\t", top);
    for f in &folders {
        let files = (0..60)
            .map(|n| file(&format!("item{n:02}.txt"), 1))
            .collect();
        fs.insert_dir(format!(r"C:\t\{f}"), files);
    }
    let (view, cx) = setup_tree(cx, Arc::clone(&fs), Arc::default());
    let row_h = view.update(cx, |v, _| v.row_height());
    let paint = |cx: &mut VisualTestContext| {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    };
    // Cursor on the last item, at the very bottom.
    cx.simulate_keystrokes("end");
    paint(cx);
    for f in ["f0", "f1", "f2"] {
        view.update(cx, |v, cx| v.click_chevron_for_test(f, cx));
        wait_until(cx, &view, "listed", |v| {
            view_names(v).contains(&sep(&[f, "item59.txt"]))
        });
    }
    paint(cx);
    // Where a row is on screen: its top relative to the top of the list's viewport.
    let screen_y = |view: &Entity<Explorer>, cx: &mut VisualTestContext, name: &str| {
        view.update(cx, |v, _| {
            let ix = view_names(v).iter().position(|n| n == name).unwrap();
            row_h * ix as f32 + v.list_scroll_for_test().y
        })
    };
    // Scroll so f1 is a few rows below the top, inside the expanded rows of f0 and f1.
    let f1_row = view.update(cx, |v, _| {
        view_names(v).iter().position(|n| n == "f1").unwrap()
    });
    let scrolled = row_h * (f1_row as f32 - 3.);
    view.update(cx, |v, _| v.set_list_scroll_for_test(scrolled));
    paint(cx);
    let before = screen_y(&view, cx, "f1");
    assert_eq!(before, row_h * 3.);

    // Collapsing f1 by its chevron: f1 stays put, the list doesn't jump to the cursor at the end.
    view.update(cx, |v, cx| v.click_chevron_for_test("f1", cx));
    paint(cx);
    assert_eq!(
        screen_y(&view, cx, "f1"),
        before,
        "collapsed row stays in place"
    );
    // Expanding it again: same.
    view.update(cx, |v, cx| v.click_chevron_for_test("f1", cx));
    wait_until(cx, &view, "f1 again", |v| {
        view_names(v).contains(&sep(&["f1", "item59.txt"]))
    });
    paint(cx);
    assert_eq!(
        screen_y(&view, cx, "f1"),
        before,
        "expanded row stays in place"
    );

    // Files appearing in f0, above what's on screen: the rows on screen don't move.
    wait_watcher(&fs, Path::new(r"C:\t\f0"));
    let names: Vec<String> = (0..5).map(|n| format!("item00-{n}.txt")).collect();
    for name in &names {
        fs.add_file(Path::new(r"C:\t\f0"), name, 1);
    }
    let added: Vec<&str> = names.iter().map(String::as_str).collect();
    fs.emit_changed(Path::new(r"C:\t\f0"), &added);
    wait_until(cx, &view, "watcher delta", |v| {
        view_names(v).contains(&sep(&["f0", "item00-4.txt"]))
    });
    paint(cx);
    assert_eq!(
        screen_y(&view, cx, "f1"),
        before,
        "rows added above stay off screen"
    );

    // The keyboard still brings the cursor into view.
    cx.simulate_keystrokes("end");
    paint(cx);
    let last = screen_y(&view, cx, "zz-last.txt");
    let height = view.update(cx, |v, _| v.list_bounds_for_test().size.height);
    assert!(
        last >= px(0.) && last + row_h <= height,
        "cursor scrolled into view"
    );
}

fn tr(key: &str) -> String {
    t!(key).into_owned()
}

/// "New folder…" / "New file…" on a folder row of the tree create inside that folder: a folder
/// that isn't expanded opens in place (listed first, so the name is free there) and the new row is
/// renamed where it appears. "New file…" selects the whole name, so a typed extension needs no
/// confirmation. Both are undoable like the background menu's.
#[gpui::test]
fn tree_folder_menu_creates_inside_nested_folders(cx: &mut TestAppContext) {
    let new_folder = tr("ops.new_folder_name");
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("alpha"), dir("beta"), file("top.txt", 1)]);
    fs.insert_dir(r"C:\t\alpha", vec![dir("inner"), file("a1.txt", 1)]);
    fs.insert_dir(
        r"C:\t\alpha\inner",
        vec![dir(&new_folder), file("deep.md", 1)],
    );
    fs.insert_dir(r"C:\t\beta", vec![file("b1.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup_tree(cx, fs, Arc::clone(&shell));
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);

    // alpha\inner, collapsed: the group sits right under Open, as at the top of VS Code's menu,
    // with "New file…" and "New folder…" in "New" (#190).
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(
        labels[..5],
        [
            tr("menu.open"),
            tr("menu.open_in_new_tab"),
            "—".into(),
            format!("{} ›", tr("menu.new")),
            tr("menu.search_in_folder"),
        ]
    );
    run_command(&view, cx, MenuCommand::NewFolderIn);
    let second = sep(&["alpha", "inner", &format!("{new_folder} (2)")]);
    wait_until(cx, &view, "renaming the new nested folder", |v| {
        v.renaming.as_ref().is_some_and(|r| *r.name == *second)
    });
    assert_eq!(
        view.update(cx, |v, _| v.tree_expanded()),
        ["alpha".to_string(), sep(&["alpha", "inner"])],
        "the folder opened in place"
    );
    assert_eq!(
        shell.ops(),
        [FileOp::NewFolder {
            parent: PathBuf::from(r"C:\t\alpha\inner"),
            name: format!("{new_folder} (2)"),
        }]
    );
    cx.simulate_keystrokes("escape");

    // beta, a top-level folder: "New file…" with the whole name selected.
    let beta = view.update(cx, |v, _| view_names(v).iter().position(|n| n == "beta"));
    right_click(&view, cx, beta);
    run_command(&view, cx, MenuCommand::NewFileIn);
    let text_doc = format!("{}.txt", tr("ops.new_text_file_name"));
    wait_until(cx, &view, "renaming the new file", |v| {
        v.renaming
            .as_ref()
            .is_some_and(|r| *r.name == *sep(&["beta", &text_doc]))
    });
    cx.simulate_input("notes.md");
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "renamed", |v| v.ops_running == 0);
    assert_eq!(
        shell.ops()[1..],
        [
            FileOp::NewFile {
                parent: PathBuf::from(r"C:\t\beta"),
                name: text_doc.clone(),
            },
            FileOp::Rename {
                path: PathBuf::from(r"C:\t\beta").join(&text_doc),
                new_name: "notes.md".into(),
            },
        ]
    );
    wait_until(cx, &view, "renamed row selected", |v| {
        selected_name(v) == Some(sep(&["beta", "notes.md"]))
    });
}

/// "Copy relative path" is offered in the tree and copies each item's path below the folder
/// shown, unquoted, one per line; the plain list doesn't offer it. The context menu setting
/// hides it.
#[gpui::test]
fn tree_copy_relative_path(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup_tree(cx, tree_fs(), Arc::clone(&shell));
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);
    cx.simulate_keystrokes("right right");
    wait_until(cx, &view, "inner listed", |v| {
        view_names(v).contains(&sep(&["alpha", "inner", "deep.md"]))
    });

    // A nested file.
    let deep = view.update(cx, |v, _| {
        view_names(v)
            .iter()
            .position(|n| *n == sep(&["alpha", "inner", "deep.md"]))
    });
    right_click(&view, cx, deep);
    let labels = menu_labels(&view, cx).unwrap();
    let copy = labels
        .iter()
        .position(|l| *l == tr("menu.copy_path"))
        .unwrap();
    assert_eq!(labels[copy + 1], tr("menu.copy_relative_path"));
    run_command(&view, cx, MenuCommand::CopyRelativePath);
    wait_shell("relative path", || shell.clipboard_text.lock().is_some());
    assert_eq!(
        shell.clipboard_text.lock().take().as_deref(),
        Some(r"alpha\inner\deep.md")
    );

    // Two items, one at the top: one per line, the top one by its name.
    cx.simulate_keystrokes("end shift-up");
    right_click(&view, cx, Some(4));
    run_command(&view, cx, MenuCommand::CopyRelativePath);
    wait_shell("relative paths", || shell.clipboard_text.lock().is_some());
    assert_eq!(
        shell.clipboard_text.lock().take().as_deref(),
        Some("beta\r\ntop.txt")
    );

    // Hidden by its switch in the context menu settings.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.menu_hidden = vec!["relative_path".into()], cx);
        });
    });
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        !labels.contains(&tr("menu.copy_relative_path")),
        "{labels:?}"
    );
    cx.simulate_keystrokes("escape");

    // The plain list doesn't offer it.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.menu_hidden.clear();
                    st.details_tree = false;
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(labels.contains(&tr("menu.copy_path")));
    assert!(
        !labels.contains(&tr("menu.copy_relative_path")),
        "{labels:?}"
    );
}

/// "Search in this folder" starts a subfolder search rooted at that folder with the cursor in the
/// box, and the folder shown stays. "Subfolders" again widens it to the whole folder shown; leaving
/// search forgets the folder.
#[gpui::test]
fn search_in_this_folder_from_the_menu(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    let (view, cx) = setup_tree(cx, tree_fs(), Arc::default());
    right_click(&view, cx, Some(0));
    run_command(&view, cx, MenuCommand::SearchInFolder);
    let walk = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.read(cx).search_walk_for_test(window, cx))
    };
    assert_eq!(view.update(cx, |v, _| v.scope()), Scope::Subfolders);
    assert!(walk(&view, cx).1, "the cursor is in the search box");
    cx.simulate_input("deep");
    wait_until(cx, &view, "results below alpha", |v| {
        final_hits(v) == [sep(&["inner", "deep.md"])]
    });
    assert_eq!(walk(&view, cx).0, Some(PathBuf::from(r"C:\t\alpha")));
    assert_eq!(
        view.update(cx, |v, _| v.current_path().map(Path::to_path_buf)),
        Some(PathBuf::from(r"C:\t")),
        "the folder shown stays"
    );

    // Subfolders again: the whole folder shown.
    view.update(cx, |v, cx| v.set_scope(Scope::Subfolders, cx));
    wait_until(cx, &view, "results below the folder shown", |v| {
        final_hits(v) == [sep(&["alpha", "inner", "deep.md"])]
    });
    assert_eq!(walk(&view, cx).0, Some(PathBuf::from(r"C:\t")));

    // Leaving search forgets the folder: searching again starts from the folder shown.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.search_in_folder(PathBuf::from(r"C:\t\beta"), window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(walk(&view, cx).0, Some(PathBuf::from(r"C:\t\beta")));
    view.update(cx, |v, cx| v.set_scope(Scope::Here, cx));
    view.update(cx, |v, cx| v.set_scope(Scope::Subfolders, cx));
    wait_until(cx, &view, "whole folder again", |v| {
        final_hits(v) == [sep(&["alpha", "inner", "deep.md"])]
    });
}

/// In the plain list, "New folder…" on a folder opens it and renames the new folder there, like
/// Explorer's new items; Back returns to where it was.
#[gpui::test]
fn folder_menu_new_folder_in_the_plain_list_opens_the_folder(cx: &mut TestAppContext) {
    let fs = tree_fs();
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\t");
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    right_click(&view, cx, Some(1));
    run_command(&view, cx, MenuCommand::NewFolderIn);
    let new_folder = tr("ops.new_folder_name");
    wait_until(cx, &view, "renaming inside beta", |v| {
        v.current_path() == Some(Path::new(r"C:\t\beta"))
            && v.renaming.as_ref().is_some_and(|r| *r.name == *new_folder)
    });
    assert_eq!(
        shell.ops(),
        [FileOp::NewFolder {
            parent: PathBuf::from(r"C:\t\beta"),
            name: new_folder,
        }]
    );
}

/// The rename box shows the name in every view: as wide as the name column in Details, and as
/// wide as the tile at every icon size (it once shrank to its border there, #38).
#[gpui::test]
fn rename_box_shows_the_name_in_every_view(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\r",
        vec![
            file("a fairly long file name for renaming.txt", 1),
            file("b.txt", 1),
        ],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    for keys in [
        "",
        "ctrl-shift-1",
        "ctrl-shift-2",
        "ctrl-shift-3",
        "ctrl-shift-4",
    ] {
        if !keys.is_empty() {
            cx.simulate_keystrokes(keys);
        }
        cx.simulate_keystrokes("home f2");
        cx.run_until_parked();
        let mode = icons_of(&view, cx);
        let shown = cx.debug_bounds("rename-box").expect("rename box drawn");
        let tile = view.update(cx, |v, _| v.tile().w);
        let least = if mode.0 == ViewMode::Details {
            px(200.)
        } else {
            tile - px(16.)
        };
        assert!(
            shown.size.width >= least,
            "{mode:?}: box {:?}, wanted at least {least:?}",
            shown.size
        );
        assert_eq!(
            shown.size.height,
            crate::theme::s_24(),
            "{mode:?}: one line"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
    }
}

/// Clicking inside the rename box moves the caret and keeps renaming. The press used to reach the
/// row underneath, which took focus back to the list, and losing focus commits the rename (#69).
/// The test window doesn't send blur events, so this checks where focus is.
#[gpui::test]
fn clicking_inside_the_rename_box_keeps_renaming(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\r", vec![file("report.txt", 1), file("b.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    for keys in ["", "ctrl-shift-2"] {
        if !keys.is_empty() {
            cx.simulate_keystrokes(keys);
        }
        cx.simulate_keystrokes("home f2");
        cx.run_until_parked();
        let shown = cx.debug_bounds("rename-box").expect("rename box drawn");
        // The text, then the box's padding at its left edge.
        for at in [
            shown.center(),
            gpui::point(shown.left() + px(2.), shown.center().y),
        ] {
            cx.simulate_click(at, gpui::Modifiers::none());
            cx.run_until_parked();
            let input_focused = cx.update(|window, cx| {
                view.update(cx, |v, cx| {
                    v.renaming.as_ref().is_some_and(|r| {
                        gpui::Focusable::focus_handle(r.input.read(cx), cx).is_focused(window)
                    })
                })
            });
            assert!(
                input_focused,
                "{keys:?}: a click at {at:?} took focus from the rename box"
            );
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
    }
}

/// Grouped Details with a folder expanded in the list (#279): the sections stay, the folder's
/// items sit under it in its section, and its header selects them too.
#[gpui::test]
fn expanding_a_folder_keeps_the_sections(cx: &mut TestAppContext) {
    use crate::explorer::groups::Row;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\t",
        vec![dir("docs"), file("apple.txt", 1), file("banana.txt", 1)],
    );
    fs.insert_dir(
        r"C:\t\docs",
        vec![file("zeta.txt", 1), file("alpha.txt", 1)],
    );
    let (view, cx) = setup_tree(cx, Arc::clone(&fs), Arc::default());
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.group_rows = true, cx));
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| v.click_chevron_for_test("docs", cx));
    wait_until(cx, &view, "listed", |v| {
        view_names(v).contains(&sep(&["docs", "zeta.txt"]))
    });
    let headers = view.update(cx, |v, _| {
        v.groups()
            .expect("still grouped")
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Header {
                    start, count, span, ..
                } => Some((*start, *count, *span)),
                Row::Item(_) => None,
            })
            .collect::<Vec<_>>()
    });
    // Folders (docs and its two items), A, B: no section for the nested "alpha" or "zeta".
    assert_eq!(headers, [(0, 1, 3), (3, 1, 1), (4, 1, 1)]);
    let rows = view.update(cx, |v, _| v.groups().map(|g| g.rows.len()));
    assert_eq!(rows, Some(8), "5 items plus 3 headers");
}

/// Collapsing a group (#287): the header stays with its count, the items lose their rows, and the
/// keyboard, Ctrl+A and Shift ranges skip them.
#[gpui::test]
fn collapsed_groups_hide_their_items(cx: &mut TestAppContext) {
    use crate::explorer::groups::Row;
    use gpui::SharedString;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\g",
        vec![
            dir("docs"),
            file("apple.txt", 1),
            file("avocado.txt", 1),
            file("banana.txt", 1),
            file("cherry.txt", 1),
        ],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\g");
    wait_until(cx, &view, "load", loaded(r"C:\g"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.group_rows = true, cx));
    });
    cx.run_until_parked();
    let rows =
        |cx: &mut gpui::VisualTestContext| view.update(cx, |v, _| v.groups().map(|g| g.rows.len()));
    let toggle = |cx: &mut gpui::VisualTestContext, label: &str| {
        view.update(cx, |v, cx| {
            v.toggle_group(&SharedString::from(label.to_owned()), cx);
        });
    };
    assert_eq!(rows(cx), Some(9), "5 items plus 4 headers");
    toggle(cx, "A");
    assert_eq!(rows(cx), Some(7), "A's two items lose their rows");
    let header = view.update(cx, |v, _| {
        v.groups().and_then(|g| {
            g.rows.iter().find_map(|r| match r {
                Row::Header {
                    label,
                    count,
                    collapsed,
                    ..
                } if label.as_ref() == "A" => Some((*count, *collapsed)),
                _ => None,
            })
        })
    });
    assert_eq!(header, Some((2, true)), "the header keeps its count");

    // Down from docs jumps over A, and up comes back.
    cx.update(|_, cx| view.update(cx, |v, cx| v.select_index(0, cx)));
    cx.simulate_keystrokes("down");
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["banana.txt"]);
    cx.simulate_keystrokes("up");
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["docs"]);
    // Shift+Down selects across the collapsed section without taking its items.
    cx.simulate_keystrokes("shift-down");
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["docs", "banana.txt"]
    );
    // Ctrl+A leaves them out too.
    cx.simulate_keystrokes("ctrl-a");
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["docs", "banana.txt", "cherry.txt"]
    );
    // End lands on the last visible item when the last section is collapsed.
    toggle(cx, "C");
    cx.simulate_keystrokes("end");
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["banana.txt"]);
    toggle(cx, "C");

    // Selecting a hidden item by index (type to select, a pasted file) opens its section.
    cx.update(|_, cx| view.update(cx, |v, cx| v.select_index(2, cx)));
    assert_eq!(rows(cx), Some(9));
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["avocado.txt"]);
    // Collapsing deselects what it hides.
    toggle(cx, "A");
    assert_eq!(rows(cx), Some(7));
    assert!(view.update(cx, |v, _| selected_names(v)).is_empty());

    // A new folder starts with every section open.
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\g"), NavKind::New, cx);
    });
    wait_until(cx, &view, "reload", loaded(r"C:\g"));
    assert_eq!(rows(cx), Some(9));
}

/// The chevron in a group header collapses it without selecting the section (#287).
#[gpui::test]
fn the_group_chevron_collapses_without_selecting(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\g",
        vec![dir("docs"), file("apple.txt", 1), file("banana.txt", 1)],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\g");
    wait_until(cx, &view, "load", loaded(r"C:\g"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.group_rows = true, cx));
    });
    cx.run_until_parked();
    let rows =
        |cx: &mut gpui::VisualTestContext| view.update(cx, |v, _| v.groups().map(|g| g.rows.len()));
    assert_eq!(rows(cx), Some(6), "3 items plus 3 headers");
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    // Rows: Folders header, docs, "A" header. The chevron sits after the list's and the header's
    // padding.
    let chevron = point(
        bounds.left() + crate::theme::list_inset() + crate::theme::s_12() + px(8.),
        bounds.top() + row * 2.5,
    );
    cx.simulate_click(chevron, Modifiers::none());
    assert_eq!(rows(cx), Some(5), "apple.txt is hidden");
    assert!(
        view.update(cx, |v, _| selected_names(v)).is_empty(),
        "the chevron doesn't select the section"
    );
    cx.simulate_click(chevron, Modifiers::none());
    assert_eq!(rows(cx), Some(6));
}

/// Search results (here the subfolder search; the whole-computer one shares the view) sort by a
/// clicked header without searching again, and a third click goes back to relevance (#55).
#[gpui::test]
fn search_results_sort_by_header_clicks(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\sr",
        vec![dir("docs"), dir("src"), file("report_mid.txt", 20)],
    );
    fs.insert_dir(r"C:\sr\docs", vec![file("report_big.txt", 900)]);
    fs.insert_dir(r"C:\sr\src", vec![file("report_small.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\sr");
    wait_until(cx, &view, "load", loaded(r"C:\sr"));
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("report");
    wait_until(cx, &view, "results", |v| final_hits(v).len() == 3);
    let relevance = view.update(cx, |v, _| final_hits(v));
    let hits = |cx: &mut gpui::VisualTestContext| view.update(cx, |v, _| final_hits(v));

    // Size starts largest first, then flips.
    click_drawn(cx, "search-header-size");
    assert_eq!(
        hits(cx),
        [
            r"docs\report_big.txt",
            "report_mid.txt",
            r"src\report_small.txt"
        ]
    );
    click_drawn(cx, "search-header-size");
    assert_eq!(
        hits(cx),
        [
            r"src\report_small.txt",
            "report_mid.txt",
            r"docs\report_big.txt"
        ]
    );
    // A third click is back to the order the search gave.
    click_drawn(cx, "search-header-size");
    assert_eq!(hits(cx), relevance);

    // Name: report_big, report_mid, report_small.
    click_drawn(cx, "search-header-name");
    assert_eq!(
        hits(cx),
        [
            r"docs\report_big.txt",
            "report_mid.txt",
            r"src\report_small.txt"
        ]
    );
    // Location: the folder searched itself first, then docs, then src.
    click_drawn(cx, "search-header-location");
    assert_eq!(
        hits(cx),
        [
            "report_mid.txt",
            r"docs\report_big.txt",
            r"src\report_small.txt"
        ]
    );
    // The sort ends with the search: a new one starts in relevance order again.
    cx.simulate_keystrokes("escape");
    view.update(cx, |v, _| assert!(v.search_sort.is_none()));
}

/// #286, #499: grouped by a date, folders go into the section of their date. Newest first they
/// trail the files of it (before files) or lead them (always first); oldest first they lead. Not
/// grouped, they stay a block of their own; column view never groups.
#[gpui::test]
fn grouped_by_date_folders_join_their_sections(cx: &mut TestAppContext) {
    use std::time::{Duration, SystemTime};
    let now = SystemTime::now();
    let old = now - Duration::from_hours(400 * 24);
    let at = |name: &str, kind, t| Entry::new(name, kind, 1, Some(t), 0);
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\d",
        vec![
            at("new.txt", EntryKind::File, now),
            at("new-dir", EntryKind::Dir, now),
            at("old.txt", EntryKind::File, old),
            at("old-dir", EntryKind::Dir, old),
        ],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\d");
    wait_until(cx, &view, "load", loaded(r"C:\d"));
    let sort = |cx: &mut gpui::VisualTestContext, ascending: bool| {
        view.update(cx, |v, cx| {
            v.set_sort(SortSpec::by(SortColumn::Modified, ascending, false), cx);
        });
        cx.run_until_parked();
    };
    let group = |cx: &mut gpui::VisualTestContext, on: bool| {
        cx.update(|_, cx| {
            SettingsStore::global(cx)
                .update(cx, |s, cx| s.update(move |st| st.group_rows = on, cx));
        });
        cx.run_until_parked();
    };
    let headers = |cx: &mut gpui::VisualTestContext| {
        view.update(cx, |v, _| {
            v.groups().map_or_default(|g| {
                g.rows
                    .iter()
                    .filter_map(|r| match r {
                        crate::explorer::groups::Row::Header { count, .. } => Some(*count),
                        crate::explorer::groups::Row::Item(_) => None,
                    })
                    .collect::<Vec<_>>()
            })
        })
    };

    sort(cx, false);
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["new.txt", "old.txt", "new-dir", "old-dir"]
    );
    group(cx, true);
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["new.txt", "new-dir", "old.txt", "old-dir"]
    );
    assert_eq!(headers(cx), [2, 2], "today and the year, two items each");
    // Always first: the folders lead each section newest first too.
    view.update(cx, |v, cx| {
        v.set_sort(
            SortSpec::by(SortColumn::Modified, false, false).with_always_first(true),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["new-dir", "new.txt", "old-dir", "old.txt"]
    );
    assert_eq!(headers(cx), [2, 2]);
    group(cx, false);
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["new-dir", "old-dir", "new.txt", "old.txt"]
    );
    group(cx, true);
    sort(cx, true);
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["old-dir", "old.txt", "new-dir", "new.txt"]
    );
    assert_eq!(headers(cx), [2, 2]);

    // Column view doesn't group: the folders go back to a block of their own.
    view.update(cx, |v, cx| v.pick_view_mode(ViewMode::Columns, cx));
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["old-dir", "new-dir", "old.txt", "new.txt"]
    );
    view.update(cx, |v, cx| v.pick_view_mode(ViewMode::Details, cx));
    cx.run_until_parked();
    group(cx, false);
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["old-dir", "new-dir", "old.txt", "new.txt"]
    );
}

/// #335: a rename changes the list in place, at the same length. The sections are made again
/// from the new list, not drawn from the one before it.
#[gpui::test]
fn sections_follow_a_rename(cx: &mut TestAppContext) {
    use crate::explorer::groups::Row;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\g",
        vec![
            file("apple.txt", 1),
            file("banana.zip", 1),
            file("zebra.part", 1),
        ],
    );
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::default(), r"C:\g");
    wait_until(cx, &view, "load", loaded(r"C:\g"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.group_rows = true, cx));
    });
    cx.run_until_parked();
    let headers = |cx: &mut gpui::VisualTestContext| {
        view.update(cx, |v, _| {
            v.groups()
                .expect("grouped")
                .rows
                .iter()
                .filter_map(|r| match r {
                    Row::Header { label, count, .. } => Some((label.to_string(), *count)),
                    Row::Item(_) => None,
                })
                .collect::<Vec<_>>()
        })
    };
    let names = |list: &[(&str, usize)]| {
        list.iter()
            .map(|(l, n)| ((*l).to_owned(), *n))
            .collect::<Vec<_>>()
    };
    assert_eq!(headers(cx), names(&[("A", 1), ("B", 1), ("Z", 1)]));
    // A download finishing: "zebra.part" becomes "banana2.zip".
    let dir = std::path::Path::new(r"C:\g");
    fs.remove(dir, "zebra.part");
    fs.add_file(dir, "banana2.zip", 1);
    fs.emit_changed(dir, &["zebra.part", "banana2.zip"]);
    wait_until(cx, &view, "renamed", |v| {
        view_names(v).iter().any(|n| n == "banana2.zip")
    });
    assert_eq!(headers(cx), names(&[("A", 1), ("B", 2)]));
}
