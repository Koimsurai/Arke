//! The title row (#120): the path bar fills it up to the search box, the search box grows with
//! long text, every gap in the row drags the window, and a press dragged on the
//! path bar's blank end moves the window instead of editing the path.

use std::sync::Arc;

use gpui::{Bounds, Focusable as _, Modifiers, MouseButton, Pixels, Point, point, px, size};

use super::*;
use crate::theme;

const DEEP: &str = r"C:\top1\projects\clients\acme\reports";

fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn open_deep(
    cx: &mut TestAppContext,
    shell: Arc<FakeShell>,
    width: f32,
) -> (Entity<Workspace>, Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_many(DEEP, 3);
    let (ws, cx) = setup_workspace_with(cx, fs, shell, DEEP);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(DEEP));
    cx.simulate_resize(size(px(width), px(650.)));
    settle(cx);
    (ws, view, cx)
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} drawn"))
}

fn near(a: Pixels, b: Pixels) -> bool {
    (a - b).abs() < px(1.)
}

const LONG_QUERY: &str = "quarterly budget review summary for the acme corporation account \
     with every attachment, draft and note from this year";

/// Every level of [`DEEP`] is drawn.
fn whole_path_shows(cx: &mut VisualTestContext) -> bool {
    [
        "crumb-0", "crumb-1", "crumb-2", "crumb-3", "crumb-4", "crumb-5",
    ]
    .into_iter()
    .all(|level| cx.debug_bounds(level).is_some())
}

#[gpui::test]
fn the_search_box_grows_with_its_text_and_steps_aside_for_the_path(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 1600.);
    let gap = bounds(cx, "caption-lead");
    let idle = |cx: &mut VisualTestContext| {
        let path = bounds(cx, "path-bar");
        let search = bounds(cx, "filter-box");
        assert!(near(search.size.width, theme::FILTER_W), "usual width");
        // The path bar runs up to the search box, which ends where the space before the window buttons starts.
        assert!(search.left() - path.right() <= theme::S_8 + px(1.));
        assert!(search.right() <= gap.left() + px(0.5));
        path
    };
    assert!(
        near(gap.size.width, theme::S_8),
        "just the usual spacing before the window buttons"
    );
    let wide_path = idle(cx);
    assert!(whole_path_shows(cx));

    // Focused, empty or with short text: nothing moves.
    cx.dispatch_action(FocusFilter);
    settle(cx);
    assert!(near(idle(cx).size.width, wide_path.size.width));
    assert!(whole_path_shows(cx), "focus alone keeps the path");
    cx.simulate_input("qu");
    settle(cx);
    assert!(near(idle(cx).size.width, wide_path.size.width));
    assert!(whole_path_shows(cx), "short text keeps the path");

    // Text that doesn't fit: the box grows up to its cap, the path bar gives up just that.
    cx.simulate_input(LONG_QUERY);
    settle(cx);
    let path = bounds(cx, "path-bar");
    let search = bounds(cx, "filter-box");
    assert!(near(search.size.width, px(560.)), "{search:?}");
    assert!(near(
        path.size.width,
        wide_path.size.width - (px(560.) - theme::FILTER_W)
    ));
    assert!(
        cx.debug_bounds("crumb-5").is_some(),
        "the current folder shows"
    );
    assert_eq!(
        bounds(cx, "caption-lead"),
        gap,
        "the window buttons never move"
    );

    // Not in use but holding text: back to its usual width, the path gets its room back.
    view.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);
    assert!(view.update(cx, |v, _| v.query.len()) > 100);
    let path = idle(cx);
    assert!(near(path.size.width, wide_path.size.width));
    assert_eq!(bounds(cx, "caption-lead"), gap);

    // Editing the path in a wide row: the search box stays as it is.
    cx.dispatch_action(FocusPathBar);
    settle(cx);
    assert!(view.update(cx, |v, _| v.editing_path));
    let path = idle(cx);
    assert!(near(path.size.width, wide_path.size.width));
    assert!(path.size.width >= px(360.));
    assert_eq!(bounds(cx, "caption-lead"), gap);
}

/// In a narrow row the search box steps aside to its magnifier while the path is edited. A press
/// on the magnifier ends the edit and focuses the search box on that same press, so a key typed
/// right away (even before the button is released) lands in the search text.
#[gpui::test]
fn the_magnifier_focuses_search_on_the_press(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 800.);
    cx.dispatch_action(FocusPathBar);
    settle(cx);
    let magnifier = bounds(cx, "filter-box");
    assert!(near(magnifier.size.width, theme::H_CTL), "{magnifier:?}");

    let at = magnifier.center();
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    let (editing, searching) = cx.update(|window, cx| {
        let v = view.read(cx);
        let focused = v
            .filter_input_for_test()
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        (v.editing_path, focused)
    });
    assert!(!editing, "the press ends the path edit");
    assert!(searching, "and focuses the search box at once");
    cx.simulate_input("q");
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.simulate_input("u");
    settle(cx);
    assert_eq!(view.update(cx, |v, _| v.query.clone()), "qu");
    assert!(!view.update(cx, |v, _| v.editing_path));
}

#[gpui::test]
fn a_narrow_window_keeps_every_minimum(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 800.);
    let path = bounds(cx, "path-bar");
    let search = bounds(cx, "filter-box");
    let gap = bounds(cx, "caption-lead");
    assert!(path.size.width >= px(110.), "path bar {path:?}");
    assert!(search.size.width >= px(120.), "search box {search:?}");
    assert!(path.right() <= search.left());
    assert!(search.right() <= gap.left() + px(0.5));
    assert!(near(gap.size.width, theme::S_8));
    // The narrow bar gives its room to "… › reports", not to the trailing › and blank end.
    let current = bounds(cx, "crumb-5");
    assert!(current.size.width >= px(60.), "current folder {current:?}");
    // Three window buttons fit after that space.
    assert!(gap.right() + theme::H_CTL * 3. <= px(800.));
}

#[gpui::test]
fn horizontal_tabs_put_the_search_box_after_the_path_bar(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 1000.);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| st.tab_layout = crate::settings::TabLayout::Horizontal,
                cx,
            );
        });
    });
    settle(cx);
    let path = bounds(cx, "path-bar");
    let search = bounds(cx, "filter-box");
    assert!(near(search.size.width, theme::FILTER_W));
    assert!(path.right() <= search.left());
    assert!(search.right() <= px(1000.));
    // The tab strip has the window buttons and its own drag space.
    assert!(cx.debug_bounds("caption-lead").is_none());
}

#[gpui::test]
fn a_click_on_the_path_bar_edits_and_a_drag_moves_the_window(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    // A made-up drag distance, wider than Windows' default.
    *shell.drag_threshold.lock() = Some((20, 20));
    let (_ws, view, cx) = open_deep(cx, Arc::clone(&shell), 1000.);
    let scale = cx.update(|window, _| window.scale_factor());
    let blank = bounds(cx, "path-blank");
    let at = blank.center();
    let editing = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.editing_path);
    let stop = |cx: &mut VisualTestContext| {
        view.update_in(cx, Explorer::cancel_path_edit);
        settle(cx);
    };
    let press_and_move = |cx: &mut VisualTestContext, to: Point<Pixels>| {
        cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        cx.run_until_parked();
    };
    let release = |cx: &mut VisualTestContext, to: Point<Pixels>| {
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        settle(cx);
    };

    // Moving less than the drag distance is still a click: edit.
    let small = px(20. / scale - 1.);
    press_and_move(cx, at + point(small, small));
    release(cx, at + point(small, small));
    assert!(editing(cx));
    assert!(shell.window_moves.lock().is_empty());
    stop(cx);

    // Past it: Windows moves the window from the press point; the release is no click.
    let far = px(20. / scale + 2.);
    press_and_move(cx, at + point(far, px(0.)));
    release(cx, at + point(far, px(0.)));
    assert!(!editing(cx));
    let expected = (
        (f32::from(at.x) * scale).round() as i32,
        (f32::from(at.y) * scale).round() as i32,
    );
    assert_eq!(*shell.window_moves.lock(), [expected]);

    // As Windows does it: the move replays the press as a title bar press, and the move loop
    // keeps the release. A drag that leaves the bar upward moves the window too.
    press_and_move(cx, at - point(px(0.), far + px(30.)));
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    settle(cx);
    assert!(!editing(cx));
    assert_eq!(shell.window_moves.lock().len(), 2);

    // The next plain click edits again.
    cx.simulate_click(at, Modifiers::none());
    settle(cx);
    assert!(editing(cx));
    stop(cx);

    // A press on a folder segment never moves the window.
    let crumb = bounds(cx, "crumb-5").center();
    cx.simulate_mouse_down(crumb, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        crumb + point(far, far),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.simulate_mouse_up(
        crumb + point(far, far),
        MouseButton::Left,
        Modifiers::none(),
    );
    settle(cx);
    assert_eq!(shell.window_moves.lock().len(), 2);
}

/// A tap on the path bar edits it even when the finger moves a little past the mouse's drag
/// distance; a real drag with the finger still moves the window.
#[gpui::test]
fn a_tap_on_the_path_bar_edits_despite_a_wobble(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    *shell.drag_threshold.lock() = Some((20, 20));
    shell
        .touch
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (_ws, view, cx) = open_deep(cx, Arc::clone(&shell), 1000.);
    let scale = cx.update(|window, _| window.scale_factor());
    let at = bounds(cx, "path-blank").center();
    let tap = |cx: &mut VisualTestContext, to: Point<Pixels>| {
        cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        settle(cx);
    };
    // Twice the mouse's drag distance: still a tap.
    let wobble = px(40. / scale);
    tap(cx, at + point(wobble, px(0.)));
    assert!(view.update(cx, |v, _| v.editing_path));
    assert!(shell.window_moves.lock().is_empty());
    view.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);
    // Well past three times: the window moves.
    let far = px(60. / scale + 4.);
    tap(cx, at + point(far, px(0.)));
    assert!(!view.update(cx, |v, _| v.editing_path));
    assert_eq!(shell.window_moves.lock().len(), 1);
}

#[gpui::test]
fn the_results_header_names_the_whole_query(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 1000.);
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("file");
    settle(cx);
    let parts = view.update(cx, |v, cx| v.info_parts(cx));
    assert_eq!(
        parts.first().map(String::as_str),
        Some("In \"reports\": \"file\"")
    );
}

/// Long text widens the search box to at most half the row (560 px in a wide one).
#[gpui::test]
fn long_text_widens_the_search_box_to_half_the_row_at_most(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 1000.);
    cx.dispatch_action(FocusFilter);
    cx.simulate_input(LONG_QUERY);
    settle(cx);
    let half = view.update(cx, |v, _| v.bar_width) * 0.5;
    assert!(half < px(560.));
    let search = bounds(cx, "filter-box");
    assert!(
        near(search.size.width, half),
        "{search:?}, half the row {half:?}"
    );
    assert!(bounds(cx, "path-bar").size.width >= px(110.));
}

/// A double click on the path bar's blank part maximizes (or restores) the window like the title
/// bar, and leaves the breadcrumbs as they were. A slow second click doesn't, and a double click
/// on the path being edited selects as usual.
#[gpui::test]
fn a_double_click_on_the_path_bar_maximizes(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let (_ws, view, cx) = open_deep(cx, Arc::clone(&shell), 1000.);
    let blank = bounds(cx, "path-blank").center();
    let editing = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.editing_path);
    let toggles = || shell.maximize_toggles.load(Ordering::Relaxed);
    let click = |cx: &mut VisualTestContext, at: Point<Pixels>| {
        cx.simulate_click(at, Modifiers::none());
        settle(cx);
    };

    click(cx, blank);
    assert!(editing(cx), "the first click edits");
    click(cx, blank + point(px(1.), px(1.)));
    assert!(!editing(cx), "the second one ends editing");
    assert!(
        bounds(cx, "path-blank").size.width > px(0.),
        "breadcrumbs back"
    );
    assert_eq!(toggles(), 1);

    // Clicks on the path being edited (not the one that started it) stay in the text.
    click(cx, blank);
    let input = bounds(cx, "path-bar").center();
    click(cx, input + point(px(80.), px(0.)));
    click(cx, input);
    assert!(editing(cx));
    assert_eq!(toggles(), 1);
    view.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);

    // Too slow for the system's double-click time: just editing.
    *shell.double_click.lock() = Some((Duration::ZERO, (4, 4)));
    click(cx, blank);
    click(cx, blank);
    assert!(editing(cx));
    assert_eq!(toggles(), 1);
}

/// A press anywhere outside the path input ends editing, also on parts that don't take focus:
/// the toolbar row, the column headers, the search box (just its magnifier while editing).
#[gpui::test]
fn a_press_outside_the_path_input_ends_editing(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 1000.);
    let path = bounds(cx, "path-bar");
    let left = path.left() + px(40.);
    let toolbar = point(left, path.bottom() + px(30.));
    let headers = point(left, path.bottom() + px(65.));
    let editing = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.editing_path);
    let press = |cx: &mut VisualTestContext, at: Point<Pixels>| {
        cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        settle(cx);
    };
    for target in ["toolbar", "headers", "search"] {
        cx.dispatch_action(FocusPathBar);
        settle(cx);
        assert!(editing(cx), "editing before pressing the {target}");
        let at = match target {
            "toolbar" => toolbar,
            "headers" => headers,
            _ => bounds(cx, "filter-box").center(),
        };
        press(cx, at);
        assert!(!editing(cx), "a press on the {target} ends editing");
    }
    // A press inside the input keeps editing.
    cx.dispatch_action(FocusPathBar);
    settle(cx);
    let input = bounds(cx, "path-bar").center();
    press(cx, input);
    assert!(editing(cx));
}

/// With the preview pane open, the gap between the path bar and the search box sits right over
/// the divider between the list and the preview, follows that divider at once when it's dragged,
/// and makes way for the usual spacing when the preview closes. A preview wider than the search
/// box's cap keeps the box at the cap and the path bar takes the rest.
#[gpui::test]
fn the_search_box_lines_up_with_the_preview_divider(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, Arc::new(FakeShell::default()), 1600.);
    let preview = |cx: &mut VisualTestContext, width: Option<f32>| {
        crate::prefs::set_preview_width(width.map(px));
        view.update(cx, |v, cx| {
            v.preview_pane = width.is_some();
            cx.notify();
        });
        cx.run_until_parked();
    };
    let lined_up = |cx: &mut VisualTestContext| {
        let divider = bounds(cx, "preview-pane").left();
        let gap = bounds(cx, "path-search-gap");
        let search = bounds(cx, "filter-box");
        assert!(
            (gap.center().x - divider).abs() < px(3.),
            "gap {gap:?} over the divider at {divider:?}"
        );
        assert!(bounds(cx, "path-bar").right() <= gap.left() + px(0.5));
        assert!(near(search.left(), gap.right()));
        assert!(search.size.width >= px(180.) && search.size.width <= px(560.));
        divider
    };

    preview(cx, Some(360.));
    let first = lined_up(cx);
    // Dragging the divider: the next frame already has the gap over it.
    preview(cx, Some(440.));
    let second = lined_up(cx);
    assert!(second < first - px(60.), "the divider moved");

    // A preview wider than the search box's cap: the box stays at the cap, at the right.
    preview(cx, Some(720.));
    let search = bounds(cx, "filter-box");
    assert!(near(search.size.width, px(560.)), "{search:?}");
    let divider = bounds(cx, "preview-pane").left();
    assert!(
        bounds(cx, "path-bar").right() > divider + px(20.),
        "the path bar runs on past the divider at {divider:?}"
    );

    // No preview: the usual spacing.
    preview(cx, None);
    settle(cx);
    assert!(cx.debug_bounds("preview-pane").is_none());
    assert!(near(bounds(cx, "filter-box").size.width, theme::FILTER_W));
    assert!(near(bounds(cx, "path-search-gap").size.width, theme::S_8));
}
