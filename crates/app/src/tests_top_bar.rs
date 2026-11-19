//! The title row and the pane rows: the search box sits centered in the window's title row and
//! searches the active pane (each pane keeps its own query); every pane's first row holds Up, its
//! path bar, its filter chip, the item count or selection actions, and the view buttons.

use std::sync::Arc;

use gpui::{Bounds, Focusable as _, Modifiers, MouseButton, Pixels, Point, point, px, size};

use super::*;
use crate::theme;

const DEEP: &str = r"C:\top1\projects\clients\acme\reports";
const OTHER: &str = r"C:\other";

fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn open_deep(
    cx: &mut TestAppContext,
    width: f32,
) -> (Entity<Workspace>, Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_many(DEEP, 3);
    fs.insert_many(OTHER, 2);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), DEEP);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(DEEP));
    cx.simulate_resize(size(px(width), px(650.)));
    settle(cx);
    (ws, view, cx)
}

/// Splits the tab; the right pane shows [`OTHER`].
fn split(
    ws: &Entity<Workspace>,
    left: &Entity<Explorer>,
    cx: &mut VisualTestContext,
) -> Entity<Explorer> {
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    let right = ws.update(cx, |ws, _| ws.pane_views()[1].clone());
    right.update(cx, |e, cx| {
        e.navigate(PathBuf::from(OTHER), NavKind::New, cx);
    });
    wait_until(cx, &right, "right pane", loaded(OTHER));
    settle(cx);
    right
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} drawn"))
}

fn near(a: Pixels, b: Pixels, within: f32) -> bool {
    (a - b).abs() <= px(within)
}

/// Whether `view`'s search box has keyboard focus.
fn searching(cx: &mut VisualTestContext, view: &Entity<Explorer>) -> bool {
    cx.update(|window, cx| {
        view.read(cx)
            .filter_input_for_test()
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
    })
}

fn press(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    settle(cx);
}

const LONG_QUERY: &str = "quarterly budget review summary for the acme corporation account \
     with every attachment, draft and note from this year, and then some more words";

#[gpui::test]
fn the_search_box_sits_centered_in_the_title_row(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, 1600.);
    let title = bounds(cx, "title-row");
    let search = bounds(cx, "filter-box");
    assert!(
        near(search.center().x, title.center().x, 2.),
        "{search:?} centered in {title:?}"
    );
    let share = search.size.width / title.size.width;
    assert!((0.4..=0.5).contains(&share), "rests at {share} of the row");
    // The pane's own row, below: Up, the path bar, then the view buttons.
    let row = bounds(cx, "pane-row");
    let path = bounds(cx, "path-bar");
    assert!(row.top() >= title.bottom());
    assert!(path.top() >= row.top() && path.bottom() <= row.bottom());
    assert!(
        path.size.width > px(600.),
        "the path bar fills the pane row"
    );
}

#[gpui::test]
fn long_text_grows_the_search_box_but_never_over_the_buttons(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1000.);
    let rest = bounds(cx, "filter-box");
    cx.dispatch_action(FocusFilter);
    cx.simulate_input(&LONG_QUERY.repeat(2));
    settle(cx);
    assert!(view.update(cx, |v, _| v.query.len()) > 200);
    let search = bounds(cx, "filter-box");
    let title = bounds(cx, "title-row");
    let caption = bounds(cx, "caption-lead");
    assert!(search.size.width > rest.size.width + px(100.), "{search:?}");
    assert!(search.left() >= title.left());
    assert!(
        search.right() <= caption.left() + px(0.5),
        "{search:?} {caption:?}"
    );
    // Not in use: back to its resting width.
    view.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);
    assert!(near(
        bounds(cx, "filter-box").size.width,
        rest.size.width,
        1.
    ));
}

#[gpui::test]
fn splitting_the_tab_does_not_move_the_title_row(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1600.);
    let title = bounds(cx, "title-row");
    let search = bounds(cx, "filter-box");
    split(&ws, &left, cx);
    assert_eq!(bounds(cx, "title-row"), title);
    assert_eq!(bounds(cx, "filter-box"), search);
}

/// Each pane keeps its own query; the search box shows the active pane's. A filtered pane shows
/// its query as a chip, which brings it back to the search box.
#[gpui::test]
fn each_pane_keeps_its_own_filter(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1600.);
    let right = split(&ws, &left, cx);
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::Activated));
    settle(cx);
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("qu");
    settle(cx);
    assert!(searching(cx, &left));

    // The right pane becomes active: the search box is its own, empty one.
    right.update(cx, |_, cx| cx.emit(ExplorerEvent::Activated));
    settle(cx);
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);
    assert_eq!(left.update(cx, |v, _| v.query.clone()), "qu");
    assert!(right.update(cx, |v, _| v.query.is_empty()));
    cx.dispatch_action(FocusFilter);
    settle(cx);
    assert!(searching(cx, &right), "Ctrl+F searches the active pane");
    cx.simulate_input("x");
    settle(cx);
    assert_eq!(right.update(cx, |v, _| v.query.clone()), "x");
    assert_eq!(left.update(cx, |v, _| v.query.clone()), "qu");

    // Escape clears only the right pane's filter.
    cx.simulate_keystrokes("escape");
    settle(cx);
    assert!(right.update(cx, |v, _| v.query.is_empty()));
    assert_eq!(left.update(cx, |v, _| v.query.clone()), "qu");

    // The left pane's chip: activates it and puts its query back in the search box.
    let chip = bounds(cx, "filter-chip").center();
    press(cx, chip);
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);
    assert!(searching(cx, &left));

    // × on the chip clears that pane's filter.
    left.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);
    let clear = bounds(cx, "filter-chip-clear").center();
    press(cx, clear);
    assert!(left.update(cx, |v, _| v.query.is_empty()));
    assert!(cx.debug_bounds("filter-chip").is_none());
}

#[gpui::test]
fn the_searched_pane_is_outlined_while_typing(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1600.);
    assert!(cx.debug_bounds("search-target").is_none());
    cx.dispatch_action(FocusFilter);
    settle(cx);
    let outline = bounds(cx, "search-target");
    assert!(outline.top() >= bounds(cx, "title-row").bottom());
    view.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);
    assert!(cx.debug_bounds("search-target").is_none());
}

/// A click in the preview pane leaves the search box, as one in the list does.
#[gpui::test]
fn a_click_in_the_preview_pane_leaves_the_search_box(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1600.);
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    settle(cx);
    cx.dispatch_action(FocusFilter);
    settle(cx);
    assert!(searching(cx, &view));
    let preview = bounds(cx, "preview-pane").center();
    press(cx, preview);
    assert!(!searching(cx, &view));
}

/// A press anywhere outside the path input ends editing, also on parts that don't take focus:
/// the column headers, the title row, the search box.
#[gpui::test]
fn a_press_outside_the_path_input_ends_editing(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1000.);
    let editing = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.editing_path);
    for target in ["headers", "title", "search"] {
        cx.dispatch_action(FocusPathBar);
        settle(cx);
        assert!(editing(cx), "editing before pressing the {target}");
        let path = bounds(cx, "path-bar");
        let at = match target {
            "headers" => point(path.left() + px(40.), path.bottom() + px(25.)),
            "title" => bounds(cx, "title-row").origin + point(px(4.), px(24.)),
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

#[gpui::test]
fn the_results_header_names_the_whole_query(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1000.);
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("file");
    settle(cx);
    let parts = view.update(cx, |v, cx| v.info_parts(cx));
    assert_eq!(
        parts.first().map(String::as_str),
        Some("In \"reports\": \"file\"")
    );
}

#[gpui::test]
fn a_narrow_window_keeps_everything_in_reach(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, 800.);
    let title = bounds(cx, "title-row");
    let search = bounds(cx, "filter-box");
    let caption = bounds(cx, "caption-lead");
    assert!(search.size.width >= px(200.), "{search:?}");
    assert!(search.left() >= title.left() && search.right() <= caption.left() + px(0.5));
    assert!(caption.right() + theme::H_CTL * 3. <= px(800.));
    assert!(bounds(cx, "path-bar").size.width >= px(150.));
}

/// With items selected the pane row shows their actions; the path bar keeps its room.
#[gpui::test]
fn the_selection_actions_share_the_pane_row(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1000.);
    assert!(
        cx.debug_bounds("pane-info").is_some(),
        "the item count shows"
    );
    cx.simulate_keystrokes("ctrl-a");
    settle(cx);
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 3);
    let row = bounds(cx, "pane-row");
    let actions = bounds(cx, "selection-actions");
    assert!(actions.top() >= row.top() && actions.bottom() <= row.bottom());
    assert!(bounds(cx, "path-bar").size.width >= px(160.));
    assert!(bounds(cx, "path-bar").right() <= actions.left());
}

#[gpui::test]
fn horizontal_tabs_keep_the_title_row_below_the_tabs(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, 1000.);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| st.tab_layout = crate::settings::TabLayout::Horizontal,
                cx,
            );
        });
    });
    settle(cx);
    let title = bounds(cx, "title-row");
    let search = bounds(cx, "filter-box");
    assert!(title.top() >= theme::H_BAR, "below the tab strip");
    assert!(search.right() <= title.right());
    // The tab strip has the window buttons.
    assert!(cx.debug_bounds("caption-lead").is_none());
}
