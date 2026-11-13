//! GPUI tests for icon sizes (`explorer_icons.rs`) and the Details view tree
//! (`explorer_tree.rs`).

use gpui::{Modifiers, ScrollDelta, ScrollEvent, point, px};

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
        (ViewMode::Icons, IconSize::EXTRA_LARGE),
        "the largest"
    );
    ctrl_wheel(cx, &view, 1.);
    assert_eq!(
        icons_of(&view, cx),
        (ViewMode::Icons, IconSize::EXTRA_LARGE)
    );
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
    assert_eq!(half, IconSize::from_px(120.));
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

/// Expanded folders follow the sort and stay current through their own watcher; a folder that
/// disappears takes its expansion with it.
#[gpui::test]
fn tree_follows_sort_and_watcher(cx: &mut TestAppContext) {
    let fs = tree_fs();
    let (view, cx) = setup_tree(cx, fs.clone(), Arc::default());
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

    // Name descending: children reverse too (folders still first).
    view.update(cx, |v, cx| {
        v.set_sort(SortSpec::by(SortColumn::Name, false, false), cx);
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

    // The expanded folder is deleted: its rows and its expansion go away.
    wait_watcher(&fs, Path::new(r"C:\t"));
    fs.remove(Path::new(r"C:\t"), "alpha");
    fs.emit_changed(Path::new(r"C:\t"), &["alpha"]);
    wait_until(cx, &view, "alpha gone", |v| {
        view_names(v) == ["beta", "top.txt"] && v.tree_expanded().is_empty()
    });
}

/// Rename, selection and the context menu work on nested rows; turning the setting off brings
/// back the flat list.
#[gpui::test]
fn tree_rows_rename_and_setting_off(cx: &mut TestAppContext) {
    let fs = tree_fs();
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup_tree(cx, fs.clone(), shell.clone());
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
    let (view, cx) = setup_tree(cx, fs.clone(), shell.clone());
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
    let (view, cx) = setup_tree(cx, fs.clone(), Arc::default());
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
            #[allow(clippy::cast_precision_loss)]
            let y = row_h * ix as f32 + v.list_scroll_for_test().y;
            y
        })
    };
    // Scroll so f1 is a few rows below the top, inside the expanded rows of f0 and f1.
    let f1_row = view.update(cx, |v, _| {
        view_names(v).iter().position(|n| n == "f1").unwrap()
    });
    #[allow(clippy::cast_precision_loss)]
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
    let (view, cx) = setup_tree(cx, fs, shell.clone());
    cx.simulate_keystrokes("home right");
    wait_until(cx, &view, "alpha listed", |v| view_names(v).len() == 5);

    // alpha\inner, collapsed: the group sits right under Open, as at the top of VS Code's menu.
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(
        labels[..6],
        [
            tr("menu.open"),
            tr("menu.open_in_new_tab"),
            "—".into(),
            tr("menu.new_file_in"),
            tr("menu.new_folder_in"),
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
    let (view, cx) = setup_tree(cx, tree_fs(), shell.clone());
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
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\t");
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
        assert_eq!(shown.size.height, crate::theme::S_24, "{mode:?}: one line");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
    }
}
