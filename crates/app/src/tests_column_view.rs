//! GPUI tests for sizing the column view's columns (`explorer_columns.rs`): dragging a divider,
//! the minimum width, fitting a column to its longest name, and saving the widths.

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
        list.left() + px(340.) + px(16.),
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
    assert_eq!(px(preview), paper - px(8.) - px(240.) - want);

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
    let want = (px(120.) + text_w(name) + px(4.)).ceil();
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
