//! GPUI tests for what the draggable dividers share (`splitter.rs`): the whole hit area lights the
//! line up, and a drag ends on any release, wherever it happens, and on a move with the button up
//! (released where the window didn't see it).

use gpui::{
    Context, CursorStyle, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement as _, Pixels, Point, Render, Styled as _, Window,
    div, point, px,
};

use super::*;
use crate::prefs::Col;
use crate::settings::SettingsStore;
use crate::splitter::{DragEnd, Look, Splitter};
use crate::theme;

/// Far outside the window: no element is under a release there, so none of them hears it.
const NOWHERE: Point<Pixels> = point(px(-5000.), px(-5000.));

/// The color the divider with element id `id` paints over its line once the pointer has moved to
/// `at` (`None`: its color at rest). Only the move itself gets the frame drawn.
fn color_at(cx: &mut VisualTestContext, id: &str, at: Point<Pixels>) -> Option<gpui::Rgba> {
    cx.simulate_mouse_move(at, None::<MouseButton>, Modifiers::none());
    cx.run_until_parked();
    crate::splitter::painted_for_test(id).expect("the divider is drawn")
}

/// Whether the divider with element id `id` shows the hover color of `look` with the pointer at
/// `at`.
fn lit_at(cx: &mut VisualTestContext, id: &str, look: Look, at: Point<Pixels>) -> bool {
    color_at(cx, id, at) == Some(crate::splitter::hover_color_for_test(look))
}

/// Every point across `hit`, at height `y`: both ends (half a pixel in) and the middle.
fn across(hit: gpui::Bounds<Pixels>, y: Pixels) -> [Point<Pixels>; 3] {
    [
        point(hit.left() + px(0.5), y),
        point(hit.center().x, y),
        point(hit.right() - px(0.5), y),
    ]
}

fn press(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
}

fn drag_to(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_move(at, MouseButton::Left, Modifiers::none());
}

/// A move that finds the button up: it was released where the window didn't see it.
fn move_released(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_move(at, None::<MouseButton>, Modifiers::none());
    cx.run_until_parked();
}

fn release(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

// ───────────────────────── The component on its own ─────────────────────────

/// A divider at x 100..108 and, right of it, an element that keeps the release to itself (it
/// covers what is under it and stops the event), like one acting on mouse up.
#[derive(Default)]
struct Harness {
    dragging: bool,
    moves: Vec<Pixels>,
    ends: usize,
    double_clicks: usize,
    /// The window's root heard a release (bubbling up from what is under the pointer).
    root_heard_up: bool,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hit = div().absolute().top_0().bottom_0().left(px(100.)).w(px(8.));
        let line = div().h_full().w(px(2.)).mx_auto();
        div()
            .id("harness")
            .relative()
            .size_full()
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, _| this.root_heard_up = true),
            )
            .child(
                Splitter::new("test-divider", CursorStyle::ResizeColumn)
                    .dragging(self.dragging)
                    .on_start(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.dragging = true;
                        cx.notify();
                    }))
                    .on_double_click(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.double_clicks += 1;
                        cx.notify();
                    }))
                    .on_drag(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                        this.moves.push(e.position.x);
                        cx.notify();
                    }))
                    .on_end(cx.listener(|this, _: &DragEnd, _, cx| {
                        if std::mem::take(&mut this.dragging) {
                            this.ends += 1;
                            cx.notify();
                        }
                    }))
                    .render(hit, line),
            )
            .child(
                div()
                    .id("keeper")
                    .absolute()
                    .top_0()
                    .left(px(200.))
                    .size(px(100.))
                    .occlude()
                    .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
            )
    }
}

fn harness(cx: &mut TestAppContext) -> (Entity<Harness>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|_, _| Harness::default());
    cx.simulate_resize(gpui::size(px(400.), px(300.)));
    cx.run_until_parked();
    (view, cx)
}

/// The line lights up wherever the pointer is in the hit area, not only over the line itself,
/// and not outside it.
#[gpui::test]
fn the_whole_hit_area_lights_the_line(cx: &mut TestAppContext) {
    let (_view, cx) = harness(cx);
    let hit = cx
        .debug_bounds("test-divider")
        .expect("the divider is drawn");
    for at in across(hit, px(50.)) {
        assert!(
            lit_at(cx, "test-divider", Look::Accent, at),
            "lit at {at:?}"
        );
    }
    assert!(!lit_at(
        cx,
        "test-divider",
        Look::Accent,
        point(hit.left() - px(2.), px(50.))
    ));
    assert!(!lit_at(
        cx,
        "test-divider",
        Look::Accent,
        point(hit.right() + px(2.), px(50.))
    ));
}

/// A drag released over an element that keeps the release to itself ends anyway, without the
/// element or anything else having to pass it on; moves report the pointer meanwhile, and the
/// line doesn't show the hover color while dragged.
#[gpui::test]
fn a_release_kept_by_another_element_ends_the_drag(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx);
    let on_divider = point(px(104.), px(50.));
    assert!(lit_at(cx, "test-divider", Look::Accent, on_divider));
    press(cx, on_divider);
    assert!(view.read_with(cx, |h, _| h.dragging));
    assert_eq!(
        crate::splitter::painted_for_test("test-divider"),
        Some(Some(theme::signal())),
        "the drag color, not the hover color, while dragged"
    );
    drag_to(cx, point(px(150.), px(50.)));
    drag_to(cx, point(px(250.), px(50.)));
    release(cx, point(px(250.), px(50.)));
    view.read_with(cx, |h, _| {
        assert!(!h.dragging, "the drag ended");
        assert_eq!(h.ends, 1, "once");
        assert_eq!(h.moves, [px(150.), px(250.)]);
        assert!(
            !h.root_heard_up,
            "nothing but the divider heard that release"
        );
    });
}

/// Released outside the window, where no element is: the drag ends. A move that finds the button
/// up ends it too (released where the window didn't see it), and nothing moves after that.
#[gpui::test]
fn a_drag_ends_on_any_release(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx);
    press(cx, point(px(104.), px(50.)));
    drag_to(cx, point(px(120.), px(50.)));
    release(cx, NOWHERE);
    view.read_with(cx, |h, _| assert!(!h.dragging && h.ends == 1));

    press(cx, point(px(104.), px(50.)));
    drag_to(cx, point(px(130.), px(50.)));
    move_released(cx, point(px(140.), px(50.)));
    view.read_with(cx, |h, _| {
        assert!(!h.dragging, "the drag ended");
        assert_eq!(h.ends, 2);
        assert_eq!(
            h.moves,
            [px(120.), px(130.)],
            "the move without the button isn't a drag"
        );
    });
    drag_to(cx, point(px(160.), px(50.)));
    view.read_with(cx, |h, _| assert_eq!(h.moves.len(), 2, "no drag, no moves"));
}

/// A click that doesn't move ends its drag at once; the second press of a double-click resets
/// instead of starting a drag.
#[gpui::test]
fn a_click_ends_and_a_double_click_resets(cx: &mut TestAppContext) {
    let (view, cx) = harness(cx);
    let at = point(px(104.), px(50.));
    cx.simulate_click(at, Modifiers::none());
    cx.run_until_parked();
    view.read_with(cx, |h, _| assert!(!h.dragging && h.ends == 1));
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    view.read_with(cx, |h, _| {
        assert!(!h.dragging, "no drag from the second press");
        assert_eq!(h.double_clicks, 1);
    });
    release(cx, at);
    view.read_with(cx, |h, _| assert_eq!(h.ends, 1, "nothing to end"));
}

// ───────────────────────── The sidebar's edge ─────────────────────────

fn sidebar_window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1), file("b.txt", 2)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::default(), r"C:\s");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.simulate_resize(gpui::size(px(1200.), px(700.)));
    cx.run_until_parked();
    (ws, cx)
}

fn saved_sidebar_width(cx: &mut VisualTestContext) -> f32 {
    cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.sidebar_width)
}

fn sidebar_resizing(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    ws.update(cx, |ws, _| ws.sidebar_resizing)
}

/// The sidebar's edge lights up anywhere in its 8 px, not only on the 2 px line drawn in it.
#[gpui::test]
fn the_sidebar_edge_lights_up_anywhere_in_its_hit_area(cx: &mut TestAppContext) {
    let (_ws, cx) = sidebar_window(cx);
    let hit = cx.debug_bounds("sidebar-edge").expect("the edge is drawn");
    let y = px(300.);
    for at in across(hit, y) {
        assert!(
            lit_at(cx, "sidebar-edge", Look::Accent, at),
            "lit at {at:?} in {hit:?}"
        );
    }
    assert!(!lit_at(
        cx,
        "sidebar-edge",
        Look::Accent,
        point(hit.left() - px(6.), y)
    ));
}

/// Released over a file row, outside the window, or found released on a move: the drag of the
/// sidebar's edge ends each time, and the width it ended on is saved.
#[gpui::test]
fn the_sidebar_edge_stops_dragging_wherever_it_is_released(cx: &mut TestAppContext) {
    let (ws, cx) = sidebar_window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    let edge = |cx: &mut VisualTestContext| {
        let hit = cx.debug_bounds("sidebar-edge").expect("the edge is drawn");
        point(hit.center().x, px(300.))
    };

    // Over the first file row (pressing it would select it; this release isn't its click).
    let selected = view.read_with(cx, |v, _| selected_names(v));
    let at = edge(cx);
    press(cx, at);
    drag_to(cx, point(px(300.), px(300.)));
    assert!(sidebar_resizing(&ws, cx));
    let (list, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let on_row = point(list.left() + px(60.), list.top() + row / 2.);
    release(cx, on_row);
    assert!(!sidebar_resizing(&ws, cx), "released over a row");
    assert!((saved_sidebar_width(cx) - 300.).abs() < 1e-3);
    assert_eq!(view.read_with(cx, |v, _| selected_names(v)), selected);

    // Outside the window.
    let at = edge(cx);
    press(cx, at);
    drag_to(cx, point(px(320.), px(300.)));
    release(cx, NOWHERE);
    assert!(!sidebar_resizing(&ws, cx), "released outside the window");
    assert!((saved_sidebar_width(cx) - 320.).abs() < 1e-3);

    // Found released on a move.
    let at = edge(cx);
    press(cx, at);
    drag_to(cx, point(px(340.), px(300.)));
    move_released(cx, point(px(360.), px(300.)));
    assert!(!sidebar_resizing(&ws, cx), "found released on a move");
    assert!((saved_sidebar_width(cx) - 340.).abs() < 1e-3);
    assert_eq!(
        theme::sidebar_full_width(),
        px(340.),
        "moves after that don't drag"
    );
}

/// What a person (or the test harness on a real machine) does, through the window's own event
/// dispatch: rest on the edge, press, 30 moves, let go with the pointer still over the edge (it
/// moved along) and no move after. The press starts the drag, the edge follows, the release ends
/// it at once, and the frame drawn for that shows the hover color, not the drag color.
#[gpui::test]
fn the_sidebar_edge_released_over_itself_shows_the_hover_color(cx: &mut TestAppContext) {
    let (ws, cx) = sidebar_window(cx);
    let hit = cx.debug_bounds("sidebar-edge").expect("the edge is drawn");
    let start = point(hit.center().x, px(300.));
    let before = theme::sidebar_full_width();
    // As a person does: the pointer rests on the edge (lit) before the press.
    assert!(
        lit_at(cx, "sidebar-edge", Look::Accent, start),
        "lit before the press"
    );
    press(cx, start);
    for step in 1..=30 {
        drag_to(cx, start + point(px(step as f32 * 1.6), px(0.)));
    }
    assert!(sidebar_resizing(&ws, cx));
    assert_eq!(
        crate::splitter::painted_for_test("sidebar-edge"),
        Some(Some(theme::signal())),
        "the drag color while dragged"
    );
    let at = start + point(px(48.), px(0.));
    release(cx, at);
    assert!(!sidebar_resizing(&ws, cx), "released over the edge");
    assert_ne!(
        theme::sidebar_full_width(),
        before,
        "the press started a drag"
    );
    assert_eq!(
        theme::sidebar_full_width(),
        at.x,
        "the edge followed the pointer"
    );
    assert!((saved_sidebar_width(cx) - f32::from(at.x)).abs() < 1e-3);
    assert_eq!(
        crate::splitter::painted_for_test("sidebar-edge"),
        Some(Some(theme::signal_line())),
        "the pointer is still over the edge: the hover color"
    );
}

// ───────────────────────── A Details column's divider ─────────────────────────

fn details(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\w", vec![file("a.txt", 1), file("b.txt", 2)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\w");
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.simulate_resize(gpui::size(px(1600.), px(800.)));
    cx.update(|_, cx| crate::prefs::apply(&SettingsStore::global(cx).read(cx).settings));
    cx.run_until_parked();
    (view, cx)
}

fn saved_column_width(cx: &mut VisualTestContext, col: Col) -> Option<u16> {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .column_widths
            .get(col)
    })
}

/// Modified's divider (in the gap right of its title) lights up anywhere in its hit area, and its
/// drag ends wherever it is released, the width saved: outside the window, or found released
/// on a move.
#[gpui::test]
fn a_column_divider_lights_up_and_stops_dragging_wherever_it_is_released(cx: &mut TestAppContext) {
    let (view, cx) = details(cx);
    let id: &'static str =
        Box::leak(format!("col-resize-{}", Col::Modified as usize).into_boxed_str());
    let hit = cx.debug_bounds(id).expect("Modified's divider is drawn");
    let y = hit.center().y;
    for at in across(hit, y) {
        assert!(lit_at(cx, id, Look::Subtle, at), "lit at {at:?} in {hit:?}");
    }
    assert!(
        !lit_at(cx, id, Look::Subtle, point(hit.left() - px(3.), y)),
        "not over the title"
    );

    let start = crate::prefs::width(Col::Modified);
    press(cx, hit.center());
    assert!(view.read_with(cx, |v, _| v.divider_drag_for_test()));
    drag_to(cx, hit.center() + point(px(40.), px(0.)));
    release(cx, NOWHERE);
    assert!(
        !view.read_with(cx, |v, _| v.divider_drag_for_test()),
        "released outside"
    );
    assert_eq!(crate::prefs::width(Col::Modified), start + px(40.));
    assert_eq!(
        saved_column_width(cx, Col::Modified),
        Some(f32::from(start + px(40.)) as u16)
    );

    let hit = cx.debug_bounds(id).expect("Modified's divider is drawn");
    press(cx, hit.center());
    drag_to(cx, hit.center() + point(px(20.), px(0.)));
    move_released(cx, hit.center() + point(px(30.), px(0.)));
    assert!(
        !view.read_with(cx, |v, _| v.divider_drag_for_test()),
        "found released"
    );
    assert_eq!(crate::prefs::width(Col::Modified), start + px(60.));
    assert_eq!(
        saved_column_width(cx, Col::Modified),
        Some(f32::from(start + px(60.)) as u16)
    );
}

// ───────────────────────── The preview pane's edge ─────────────────────────

fn with_preview(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.bin", 5)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1500.), px(800.)));
    crate::prefs::set_preview_width(Some(px(360.)));
    // In the settings too: saving the width syncs the pane from them.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    (view, cx)
}

fn saved_preview_width(cx: &mut VisualTestContext) -> Option<u16> {
    cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.preview_width)
}

/// The preview pane's edge lights up anywhere in its hit area, and its drag ends wherever it is
/// released, the width saved: outside the window, or found released on a move.
#[gpui::test]
fn the_preview_edge_lights_up_and_stops_dragging_wherever_it_is_released(cx: &mut TestAppContext) {
    let (view, cx) = with_preview(cx);
    let hit = cx
        .debug_bounds("preview-resize")
        .expect("the preview's edge is drawn");
    let y = hit.center().y;
    for at in across(hit, y) {
        assert!(
            lit_at(cx, "preview-resize", Look::Accent, at),
            "lit at {at:?} in {hit:?}"
        );
    }
    assert!(
        !lit_at(
            cx,
            "preview-resize",
            Look::Accent,
            point(hit.left() - px(3.), y)
        ),
        "not over the list"
    );

    press(cx, hit.center());
    drag_to(cx, hit.center() - point(px(50.), px(0.)));
    release(cx, NOWHERE);
    assert!(
        !view.read_with(cx, |v, _| v.divider_drag_for_test()),
        "released outside"
    );
    let dragged = crate::prefs::stored_preview_width();
    assert_ne!(dragged, Some(360), "the drag widened the pane");
    assert_eq!(saved_preview_width(cx), dragged);

    let hit = cx
        .debug_bounds("preview-resize")
        .expect("the preview's edge is drawn");
    press(cx, hit.center());
    drag_to(cx, hit.center() - point(px(20.), px(0.)));
    move_released(cx, hit.center() - point(px(30.), px(0.)));
    assert!(
        !view.read_with(cx, |v, _| v.divider_drag_for_test()),
        "found released"
    );
    let again = crate::prefs::stored_preview_width();
    assert_ne!(again, dragged, "the second drag widened it again");
    assert_eq!(saved_preview_width(cx), again);
}
