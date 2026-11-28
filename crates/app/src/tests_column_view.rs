//! GPUI tests for the column view's columns (`explorer_columns.rs`): sizing them (dragging a
//! divider, the minimum width, fitting a column to its longest name, saving the widths), drops on
//! the parent column, the drives at a drive root, and the parent column's live listing.

use gpui::{Modifiers, MouseButton, Pixels, Point, point, px};

use super::*;
use crate::prefs::ViewColumn;
use crate::settings::{ColumnViewWidths, SettingsStore};

const LONG: &str = "a folder with a rather long name that never fits";

/// `C:\m\one` in the column view, in a window wide enough for the preview. In a workspace, so the
/// pane sits beside the sidebar as in the app (the widths add up to the window's).
fn open_columns(
    cx: &mut TestAppContext,
    current: Vec<Entry>,
) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("one"), dir(LONG), file("b.txt", 1)]);
    fs.insert_dir(r"C:\m\one", current);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\m\one");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\m\one"));
    cx.simulate_resize(gpui::size(px(1800.), px(800.)));
    cx.dispatch_action(crate::explorer::ViewColumns);
    wait_until(cx, &view, "parent column", |v| {
        v.parent_column_names().len() == 3
    });
    (view, cx)
}

fn saved(cx: &mut VisualTestContext) -> ColumnViewWidths {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .column_view_widths
    })
}

/// The middle of the parent column's divider (on its right border).
fn parent_divider(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Point<Pixels> {
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    let parent = crate::prefs::view_column_width(ViewColumn::Parent).unwrap_or(px(240.));
    point(list.left() + parent, list.top() + px(200.))
}

/// The preview's edge, which is the current column's divider: where the list ends, 3 px in.
fn preview_edge(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Point<Pixels> {
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    point(list.right() + px(3.), list.top() + px(200.))
}

/// Press at `at`, move `dx` sideways (past the drag threshold first), and stop there without
/// releasing.
fn drag_by(cx: &mut VisualTestContext, at: Point<Pixels>, dx: Pixels) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    let sign = if dx < px(0.) { -1. } else { 1. };
    cx.simulate_mouse_move(
        point(at.x + px(20.) * sign, at.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(point(at.x + dx, at.y), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn release(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn double_click(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_click(at, Modifiers::none());
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
    });
    cx.run_until_parked();
}

/// What the test text system draws a name as: every character 0.6 em wide at 14 px.
fn text_w(name: &str) -> Pixels {
    px(f32::from(u16::try_from(name.chars().count()).unwrap()) * 14. * 0.6)
}

/// Dragging the parent column's divider sizes the column, saved on release for every folder;
/// it never gets narrower than the minimum, and rows right next to it still take clicks.
#[gpui::test]
fn dragging_the_parent_divider_sizes_and_saves_it(cx: &mut TestAppContext) {
    let (view, cx) = open_columns(cx, vec![file("x.txt", 1), file("y.txt", 1)]);
    let grab = parent_divider(&view, cx);
    drag_by(cx, grab, px(100.));
    assert_eq!(
        crate::prefs::view_column_width(ViewColumn::Parent),
        Some(px(340.)),
        "the divider follows the cursor"
    );
    assert_eq!(
        saved(cx).parent,
        None,
        "saved on release, not on every move"
    );
    release(cx, point(grab.x + px(100.), grab.y));
    assert_eq!(saved(cx).parent, Some(340));
    assert_eq!(saved(cx).current, None, "the current column still fills");

    // A row starts just right of the divider and still takes the click.
    let (list, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let first_row = point(
        list.left() + px(340.) + crate::theme::list_inset() + px(8.),
        list.top() + crate::theme::s_4() + row / 2.,
    );
    cx.simulate_click(first_row, Modifiers::none());
    wait_until(cx, &view, "row selected", |v| {
        selected_names(v) == ["x.txt"]
    });

    // Dragged far to the left: the minimum.
    let grab = parent_divider(&view, cx);
    drag_by(cx, grab, px(-500.));
    release(cx, point(grab.x - px(500.), grab.y));
    assert_eq!(saved(cx).parent, Some(160));
    assert_eq!(
        crate::prefs::view_column_width(ViewColumn::Parent),
        Some(px(160.))
    );
}

/// In the column view the preview's edge is the current column's divider: dragging it sizes
/// that column (the preview takes the rest) without touching the preview pane's own width.
#[gpui::test]
fn dragging_the_preview_edge_sizes_the_current_column(cx: &mut TestAppContext) {
    let (view, cx) = open_columns(cx, vec![file("x.txt", 1)]);
    let width_before = view.update(cx, |v, _| v.list_bounds_for_test()).size.width - px(240.);
    let grab = preview_edge(&view, cx);
    drag_by(cx, grab, px(-150.));
    release(cx, point(grab.x - px(150.), grab.y));
    let want = width_before - px(150.);
    assert_eq!(
        crate::prefs::view_column_width(ViewColumn::Current),
        Some(want)
    );
    assert_eq!(saved(cx).current, Some(f32::from(want) as u16));
    let preview_width =
        cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.preview_width);
    assert_eq!(
        preview_width, None,
        "the preview pane's own width stays automatic"
    );
    // The column comes out exactly that wide; the preview fills the rest.
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    assert_eq!(list.size.width - px(240.), want);
    let (shown, preview) = view.update(cx, |v, _| v.fit_for_test());
    assert!(shown);
    let paper = view.update(cx, |v, _| v.widths_for_test()).0;
    assert_eq!(px(preview), paper - px(4.) - px(240.) - want);

    // Too narrow: the minimum.
    let grab = preview_edge(&view, cx);
    drag_by(cx, grab, px(-900.));
    release(cx, point(grab.x - px(900.), grab.y));
    assert_eq!(saved(cx).current, Some(160));
}

/// Double-clicking a divider fits its column to the longest name in it, between 160 and 600 px,
/// and saves it. Names cut off before that get a tooltip with the whole name.
#[gpui::test]
fn double_clicking_a_divider_fits_the_column(cx: &mut TestAppContext) {
    let longest = "x".repeat(120);
    let (view, cx) = open_columns(
        cx,
        vec![file("a.txt", 1), file(&format!("{longest}.txt"), 1)],
    );
    let tips = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.read(cx).current_name_tips_for_test(window))
    };
    assert_eq!(tips(&view, cx), [false, true], "only the cut-off name");

    // Parent: the border, the list's and the row's padding, the icon and its gap, the name, the
    // folder arrow with its gap, and a little air.
    let at = parent_divider(&view, cx);
    double_click(cx, at);
    let want = (px(1. + 32. + 16. + 8.) + text_w(LONG) + px(8. + 12. + 4.)).ceil();
    assert_eq!(
        crate::prefs::view_column_width(ViewColumn::Parent),
        Some(want)
    );
    assert_eq!(saved(cx).parent, Some(f32::from(want) as u16));

    // Current: the name is wider than the fit allows, so the column stops at 600 px.
    let at = preview_edge(&view, cx);
    double_click(cx, at);
    assert_eq!(
        crate::prefs::view_column_width(ViewColumn::Current),
        Some(px(600.))
    );
    assert_eq!(saved(cx).current, Some(600));
    assert_eq!(
        view.update(cx, |v, _| v.list_bounds_for_test()).size.width - want,
        px(600.)
    );
}

/// A name cut off by a narrow column has a tooltip until the column is fitted to it.
#[gpui::test]
fn a_fitted_column_shows_its_names_whole(cx: &mut TestAppContext) {
    let name = "a name of thirty characters ok";
    let (view, cx) = open_columns(cx, vec![file("a.txt", 1), file(name, 1)]);
    // Narrow the current column so the longer name is cut off.
    let grab = preview_edge(&view, cx);
    let width = view.update(cx, |v, _| v.list_bounds_for_test()).size.width - px(240.);
    let dx = px(160.) - width;
    drag_by(cx, grab, dx);
    release(cx, point(grab.x + dx, grab.y));
    assert_eq!(saved(cx).current, Some(160));
    let tips = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.read(cx).current_name_tips_for_test(window))
    };
    assert_eq!(tips(&view, cx), [true, false], "the long name sorts first");
    let at = preview_edge(&view, cx);
    double_click(cx, at);
    // The row's chrome: the list's left inset (#448) plus 112 px of right padding, gutter, row
    // padding, icon and folder arrow.
    let want = (crate::theme::list_inset() + px(112.) + text_w(name) + px(4.)).ceil();
    assert!(want > px(160.), "{want:?}");
    assert_eq!(
        crate::prefs::view_column_width(ViewColumn::Current),
        Some(want)
    );
    assert_eq!(tips(&view, cx), [false, false]);
}

/// Short names fit the column at its minimum.
#[gpui::test]
fn fitting_short_names_keeps_the_minimum(cx: &mut TestAppContext) {
    let (view, cx) = open_columns(cx, vec![file("a", 1), file("b", 1)]);
    let at = preview_edge(&view, cx);
    double_click(cx, at);
    assert_eq!(saved(cx).current, Some(160));
    assert_eq!(
        view.update(cx, |v, _| v.list_bounds_for_test()).size.width - px(240.),
        px(160.)
    );
}

/// The parent column's width is shared by every pane: one made wide in a big window doesn't
/// squeeze the current folder's column to nothing once the window is smaller.
#[gpui::test]
fn a_wide_parent_column_gives_way_in_a_smaller_window(cx: &mut TestAppContext) {
    let (view, cx) = open_columns(cx, vec![file("x.txt", 1)]);
    crate::prefs::set_view_column_width(ViewColumn::Parent, Some(px(900.)));
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    let name = cx
        .debug_bounds("name:x.txt")
        .expect("the current column's row is drawn");
    assert!(
        name.left() < list.right() - px(40.),
        "room left for the current column: {name:?} in {list:?}"
    );
    crate::prefs::set_view_column_width(ViewColumn::Parent, None);
}

/// Parent column drop setup: `C:\m` holds `aaa`, `one` (the current folder, with `x.txt` and
/// `y.txt`) and `b.txt`; the column view is open.
fn drop_columns(
    cx: &mut TestAppContext,
) -> (
    Arc<FakeShell>,
    Entity<Explorer>,
    Entity<Workspace>,
    &mut VisualTestContext,
) {
    let (_fs, shell, view, ws, cx) = drop_columns_fs(cx);
    (shell, view, ws, cx)
}

/// [`drop_columns`], with its file system.
fn drop_columns_fs(
    cx: &mut TestAppContext,
) -> (
    Arc<FakeFs>,
    Arc<FakeShell>,
    Entity<Explorer>,
    Entity<Workspace>,
    &mut VisualTestContext,
) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("aaa"), dir("one"), file("b.txt", 1)]);
    fs.insert_dir(r"C:\m\aaa", vec![]);
    fs.insert_dir(r"C:\m\one", vec![file("x.txt", 1), file("y.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\m\one");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\m\one"));
    cx.simulate_resize(gpui::size(px(1800.), px(800.)));
    cx.dispatch_action(crate::explorer::ViewColumns);
    wait_until(cx, &view, "parent column", |v| {
        v.parent_column_names().len() == 3
    });
    cx.run_until_parked();
    (fs, shell, view, ws, cx)
}

/// Centre of the current column's first row (`x.txt`).
fn x_txt_row(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Point<Pixels> {
    let (list, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let parent = crate::prefs::view_column_width(ViewColumn::Parent).unwrap_or(px(240.));
    point(
        list.left() + parent + px(60.),
        list.top() + px(4.) + row * 0.5,
    )
}

fn centre(cx: &mut VisualTestContext, selector: &'static str) -> Point<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} drawn"))
        .center()
}

/// Drags from `from` to `to` and releases there holding `modifiers`.
fn drag_and_drop(
    cx: &mut VisualTestContext,
    from: Point<Pixels>,
    to: Point<Pixels>,
    modifiers: Modifiers,
) {
    cx.simulate_mouse_down(from, MouseButton::Left, modifiers);
    cx.simulate_mouse_move(from + point(px(30.), px(4.)), MouseButton::Left, modifiers);
    cx.simulate_mouse_move(to, MouseButton::Left, modifiers);
    cx.run_until_parked();
    assert!(cx.update(|_, cx| cx.has_active_drag()), "the drag started");
    cx.simulate_mouse_up(to, MouseButton::Left, modifiers);
    cx.run_until_parked();
}

/// #430: a folder in the parent column takes drops like a folder row in the list: same volume
/// moves, Ctrl copies, and the current folder (where the items already are) takes none.
#[gpui::test]
fn dropping_on_a_parent_column_folder_moves_or_copies_into_it(cx: &mut TestAppContext) {
    let (shell, view, _ws, cx) = drop_columns(cx);
    let from = x_txt_row(&view, cx);
    let aaa = centre(cx, "parent-row-0");
    drag_and_drop(cx, from, aaa, Modifiers::none());
    wait_shell_in(cx, "the move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::Move {
            sources: vec![PathBuf::from(r"C:\m\one\x.txt")],
            dest: PathBuf::from(r"C:\m\aaa"),
        }]
    );
    let from = x_txt_row(&view, cx);
    let aaa = centre(cx, "parent-row-0");
    let ctrl = Modifiers {
        control: true,
        ..Modifiers::none()
    };
    drag_and_drop(cx, from, aaa, ctrl);
    wait_shell_in(cx, "the copy", || shell.ops().len() == 2);
    assert!(
        matches!(&shell.ops()[1], FileOp::Copy { dest, .. } if dest == Path::new(r"C:\m\aaa")),
        "{:?}",
        shell.ops()
    );
    // The current folder's own row: the items are there already, so nothing happens.
    let from = x_txt_row(&view, cx);
    let one = centre(cx, "parent-row-1");
    drag_and_drop(cx, from, one, Modifiers::none());
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    assert_eq!(shell.ops().len(), 2, "{:?}", shell.ops());
}

/// #430: the empty space of the parent column takes drops for the folder it shows, and so does a
/// file row in it (like the list's background under a file).
#[gpui::test]
fn dropping_on_parent_column_empty_space_goes_to_its_folder(cx: &mut TestAppContext) {
    let (shell, view, _ws, cx) = drop_columns(cx);
    let column = cx.debug_bounds("parent-column").expect("column drawn");
    let empty = point(column.left() + px(60.), column.bottom() - px(40.));
    let from = x_txt_row(&view, cx);
    drag_and_drop(cx, from, empty, Modifiers::none());
    wait_shell_in(cx, "the move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::Move {
            sources: vec![PathBuf::from(r"C:\m\one\x.txt")],
            dest: PathBuf::from(r"C:\m"),
        }]
    );
    let from = x_txt_row(&view, cx);
    let file_row = centre(cx, "parent-row-2");
    drag_and_drop(cx, from, file_row, Modifiers::none());
    wait_shell_in(cx, "the second move", || shell.ops().len() == 2);
    assert!(
        matches!(&shell.ops()[1], FileOp::Move { dest, .. } if dest == Path::new(r"C:\m")),
        "{:?}",
        shell.ops()
    );
}

/// Starts dragging `x.txt` (no drop yet).
fn start_drag(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Point<Pixels> {
    let from = x_txt_row(view, cx);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    let moved = from + point(px(30.), px(4.));
    cx.simulate_mouse_move(moved, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        moved + point(px(30.), px(0.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert!(cx.update(|_, cx| cx.has_active_drag()), "the drag started");
    moved + point(px(30.), px(0.))
}

/// The pointer rests on the path bar's "…" (as far as the spring is concerned): `action`'s
/// countdown starts, and runs out when `wait`.
fn hover_more(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    action: crate::spring::Action,
    wait: bool,
) {
    cx.update(|window, cx| {
        crate::spring::hover_at(
            "crumb-more".into(),
            action,
            crate::spring::Target::Explorer(view.downgrade()),
            true,
            point(px(10.), px(10.)),
            window,
            cx,
        );
    });
    if wait {
        cx.executor().advance_clock(crate::spring::DELAY);
    }
    cx.run_until_parked();
}

fn levels() -> crate::spring::Action {
    crate::spring::Action::Levels(vec![("aaa".to_owned(), PathBuf::from(r"C:\m\aaa"))])
}

fn menu_open(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> bool {
    view.update(cx, |v, _| v.path_popup_for_test().is_some())
}

/// #430: a drag held on the "…" lists the hidden levels in the pane after the spring delay (not
/// before), and a drop on a level moves into that folder like a drop on a breadcrumb.
#[gpui::test]
fn hovering_the_more_button_in_a_drag_lists_levels_to_drop_on(cx: &mut TestAppContext) {
    let (shell, view, _ws, cx) = drop_columns(cx);
    start_drag(&view, cx);
    hover_more(&view, cx, levels(), false);
    assert!(!menu_open(&view, cx), "not before the delay");
    cx.executor().advance_clock(crate::spring::DELAY);
    cx.run_until_parked();
    assert!(menu_open(&view, cx), "opens after the delay");
    let row = centre(cx, "path-drop-row-0");
    cx.simulate_mouse_move(row, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(row, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    wait_shell_in(cx, "the move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::Move {
            sources: vec![PathBuf::from(r"C:\m\one\x.txt")],
            dest: PathBuf::from(r"C:\m\aaa"),
        }]
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(!menu_open(&view, cx), "closed once the drag ended");
}

/// #430: the list closes when the drag is cancelled, with nothing moved.
#[gpui::test]
fn the_levels_list_closes_when_the_drag_is_cancelled(cx: &mut TestAppContext) {
    let (shell, view, _ws, cx) = drop_columns(cx);
    start_drag(&view, cx);
    hover_more(&view, cx, levels(), true);
    assert!(menu_open(&view, cx));
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| {
        assert!(!cx.has_active_drag(), "Esc ended the drag");
        window.refresh();
    });
    cx.run_until_parked();
    assert!(!menu_open(&view, cx), "closed with the drag");
    assert!(shell.ops().is_empty(), "{:?}", shell.ops());
}

/// #430: a pinned tab keeps its folder, so a drag over a segment starts no countdown there, but
/// the "…" list opens no folder and still comes up.
#[gpui::test]
fn a_pinned_tab_blocks_navigation_but_not_the_levels_list(cx: &mut TestAppContext) {
    let (_shell, view, ws, cx) = drop_columns(cx);
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    start_drag(&view, cx);
    hover_more(
        &view,
        cx,
        crate::spring::Action::Open(PathBuf::from(r"C:\m\aaa")),
        false,
    );
    assert_eq!(cx.update(|_, cx| crate::spring::pending_key(cx)), None);
    hover_more(&view, cx, levels(), true);
    assert!(menu_open(&view, cx), "the list opens on a pinned tab");
    assert_eq!(
        view.update(cx, |v, _| v.display_path().map(Path::to_path_buf)),
        Some(PathBuf::from(r"C:\m\one")),
        "and the folder stays"
    );
}

/// Draws the window again, so debug bounds follow the latest state.
fn redraw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn drive(label: &str, root: &str, remote: bool) -> Place {
    Place {
        label: label.into(),
        path: PathBuf::from(root),
        kind: PlaceKind::Drive {
            remote,
            space: None,
        },
    }
}

/// Copies and moves land on the fake file system too, and its watchers hear of them.
fn ops_on_fake_fs(fs: &Arc<FakeFs>, shell: &FakeShell) {
    let fs = Arc::clone(fs);
    *shell.on_op.lock() = Some(Box::new(move |op| {
        let (FileOp::Copy { sources, dest } | FileOp::Move { sources, dest }) = op else {
            return;
        };
        for source in sources {
            let name = source.file_name().unwrap().to_str().unwrap();
            if matches!(op, FileOp::Move { .. }) {
                let from = source.parent().unwrap();
                fs.remove(from, name);
                fs.emit_changed(from, &[name]);
            }
            fs.add_file(dest, name, 1);
            fs.emit_changed(dest, &[name]);
        }
    }));
}

/// #523: a drive root has no parent, so the parent column lists the computer's drives (mapped
/// ones too), the current one marked. Clicking another drive goes there and stays in the column
/// view.
#[gpui::test]
fn a_drive_root_lists_the_drives(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![file("c.txt", 1)]);
    fs.insert_dir(r"C:\Users", vec![file("u.txt", 1)]);
    fs.insert_dir(r"D:\", vec![file("d.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().extend([
        drive("Local Disk (C:)", r"C:\", false),
        drive("Data (D:)", r"D:\", false),
        drive("Share (Z:)", r"Z:\", true),
    ]);
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\")(v) && v.places.len() == 4
    });
    cx.simulate_resize(gpui::size(px(1800.), px(800.)));
    cx.dispatch_action(crate::explorer::ViewColumns);
    redraw(cx);
    let roots = vec![
        PathBuf::from(r"C:\"),
        PathBuf::from(r"D:\"),
        PathBuf::from(r"Z:\"),
    ];
    assert_eq!(
        view.update(cx, |v, _| v.parent_column_drives()),
        Some((roots.clone(), Some(0)))
    );
    assert_eq!(
        view.update(cx, |v, _| v.parent_column_lists()),
        None,
        "no folder listed or watched"
    );
    let d = centre(cx, "parent-drive-1");
    cx.simulate_click(d, Modifiers::none());
    wait_until(cx, &view, "on D:", loaded(r"D:\"));
    redraw(cx);
    view.update(cx, |v, _| {
        assert_eq!(
            v.view_mode,
            crate::explorer::ViewMode::Columns,
            "still the column view"
        );
        assert_eq!(v.parent_column_drives(), Some((roots, Some(1))));
    });
    // In a folder of the drive, the column lists the drive again.
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\"), NavKind::New, cx);
    });
    wait_until(cx, &view, "on C:", loaded(r"C:\"));
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\Users"), NavKind::New, cx);
    });
    wait_until(cx, &view, "in Users", loaded(r"C:\Users"));
    wait_until(cx, &view, "the drive listed", |v| {
        v.parent_column_drives().is_none() && v.parent_column_names() == ["Users", "c.txt"]
    });
}

/// A parent folder that can't be listed says why in the column instead of staying empty, and is
/// listed again on the next navigation.
#[gpui::test]
fn a_failed_parent_listing_shows_why_and_is_retried(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    // `C:\m\one` alone: its parent can't be listed.
    fs.add_entry(Path::new(r"C:\m\one"), file("x.txt", 1));
    let (ws, cx) = setup_workspace(cx, Arc::clone(&fs), r"C:\m\one");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\m\one"));
    cx.simulate_resize(gpui::size(px(1800.), px(800.)));
    cx.dispatch_action(crate::explorer::ViewColumns);
    wait_until(cx, &view, "the failure", |v| {
        v.parent_column_error().is_some()
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("parent-column-error").is_some(),
        "the column says why"
    );
    assert!(view.update(cx, |v, _| v.parent_column_names().is_empty()));

    // It can be listed now: going to another folder in it tries again.
    fs.insert_dir(r"C:\m", vec![dir("one"), dir("two")]);
    fs.add_entry(Path::new(r"C:\m\two"), file("y.txt", 1));
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\m\two"), NavKind::New, cx);
    });
    wait_until(cx, &view, "two", loaded(r"C:\m\two"));
    wait_until(cx, &view, "listed after the retry", |v| {
        v.parent_column_names() == ["one", "two"]
    });
    assert_eq!(view.update(cx, |v, _| v.parent_column_error()), None);
    redraw(cx);
    assert!(cx.debug_bounds("parent-column-error").is_none());
    assert!(
        cx.debug_bounds("parent-row-1").is_some(),
        "the rows are back"
    );
}

/// #524 (#430): an item moved into the parent column's folder shows up there.
#[gpui::test]
fn a_drop_into_the_parent_column_shows_up_in_it(cx: &mut TestAppContext) {
    let (fs, shell, view, _ws, cx) = drop_columns_fs(cx);
    ops_on_fake_fs(&fs, &shell);
    wait_watcher(&fs, Path::new(r"C:\m"));
    let column = cx.debug_bounds("parent-column").expect("column drawn");
    let empty = point(column.left() + px(60.), column.bottom() - px(40.));
    let from = x_txt_row(&view, cx);
    drag_and_drop(cx, from, empty, Modifiers::none());
    wait_until(cx, &view, "the moved file in the parent column", |v| {
        v.parent_column_names() == ["aaa", "one", "b.txt", "x.txt"]
    });
}

/// #524: the parent column follows changes other programs make in its folder; leaving the column
/// view stops watching it.
#[gpui::test]
fn the_parent_column_follows_its_watcher(cx: &mut TestAppContext) {
    let (fs, _shell, view, _ws, cx) = drop_columns_fs(cx);
    let parent = Path::new(r"C:\m");
    wait_watcher(&fs, parent);
    fs.add_file(parent, "new.txt", 1);
    fs.emit_changed(parent, &["new.txt"]);
    wait_until(cx, &view, "watcher delta", |v| {
        v.parent_column_names() == ["aaa", "one", "b.txt", "new.txt"]
    });
    fs.remove(parent, "b.txt");
    fs.emit_changed(parent, &["b.txt"]);
    wait_until(cx, &view, "watcher removal", |v| {
        v.parent_column_names() == ["aaa", "one", "new.txt"]
    });

    cx.dispatch_action(crate::explorer::ViewDetails);
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.parent_column_lists()), None);
    wait_shell_in(cx, "the parent's watch stopped", || {
        fs.watcher_count(parent) == 0
    });
}

/// #524: F5 lists the parent column's folder again, picking up what no watcher reported.
#[gpui::test]
fn f5_relists_the_parent_column(cx: &mut TestAppContext) {
    let (fs, _shell, view, _ws, cx) = drop_columns_fs(cx);
    fs.add_file(Path::new(r"C:\m"), "quiet.txt", 1);
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| v.parent_column_names()),
        ["aaa", "one", "b.txt"]
    );
    cx.simulate_keystrokes("f5");
    wait_until(cx, &view, "relisted", |v| {
        v.parent_column_names() == ["aaa", "one", "b.txt", "quiet.txt"]
    });
}

/// #524: showing or hiding hidden items, and another sort, apply to the parent column too.
#[gpui::test]
fn the_parent_column_follows_hidden_items_and_the_sort(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let secret = Entry::new("secret", EntryKind::Dir, 0, None, FILE_ATTRIBUTE_HIDDEN);
    fs.insert_dir(r"C:\m", vec![dir("one"), secret, file("b.txt", 1)]);
    fs.insert_dir(r"C:\m\one", vec![file("x.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\m\one");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\m\one"));
    cx.simulate_resize(gpui::size(px(1800.), px(800.)));
    cx.dispatch_action(crate::explorer::ViewColumns);
    wait_until(cx, &view, "parent column", |v| {
        v.parent_column_names() == ["one", "b.txt"]
    });
    cx.simulate_keystrokes("ctrl-h");
    wait_until(cx, &view, "hidden items shown", |v| {
        v.parent_column_names() == ["one", "secret", "b.txt"]
    });
    cx.simulate_keystrokes("ctrl-h");
    wait_until(cx, &view, "hidden items hidden again", |v| {
        v.parent_column_names() == ["one", "b.txt"]
    });
    // Name descending, folders before files (File Explorer): the files come first.
    view.update(cx, |v, cx| {
        v.set_sort(SortSpec::by(SortColumn::Name, false, false), cx);
    });
    wait_until(cx, &view, "resorted", |v| {
        v.parent_column_names() == ["b.txt", "one"]
    });
}

/// `FILE_ATTRIBUTE_HIDDEN`.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
