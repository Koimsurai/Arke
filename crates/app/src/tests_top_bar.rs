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

/// The filter chip says what the pane is filtered by (its tooltip has all of it); the pane row
/// carries no second description of the query, which would squeeze the path bar.
#[gpui::test]
fn the_pane_row_has_no_query_description(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1000.);
    let before = bounds(cx, "path-bar").size.width;
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("file");
    settle(cx);
    let parts = view.update(cx, |v, cx| v.info_parts(cx));
    assert!(
        parts.iter().all(|p| !p.contains("file")),
        "no description of the query: {parts:?}"
    );
    view.update_in(cx, Explorer::cancel_path_edit);
    settle(cx);
    assert!(cx.debug_bounds("filter-chip").is_some());
    let after = bounds(cx, "path-bar").size.width;
    assert!(
        after >= before - px(200.),
        "the path bar only gives way to the chip: {before:?} -> {after:?}"
    );
}

/// Split view: only the active pane has the accent edge, as a thin outline over the paper (never
/// a shadow, which tints the translucent paper through).
#[gpui::test]
fn the_active_pane_has_a_thin_outline_not_a_tint(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1600.);
    assert!(cx.debug_bounds("active-pane-outline").is_none(), "unsplit");
    split(&ws, &left, cx);
    let outline = bounds(cx, "active-pane-outline");
    assert!(outline.size.width < px(1600.) / 2.);
    assert!(outline.top() >= bounds(cx, "title-row").bottom());
    // Unsplit again: the one pane left is no longer marked.
    let right = ws.update(cx, |ws, _| ws.pane_views()[1].clone());
    right.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    settle(cx);
    assert!(cx.debug_bounds("active-pane-outline").is_none());
}

/// A search box too narrow for "Search <folder> (Ctrl+F)" shows the short placeholder.
#[gpui::test]
fn a_narrow_search_box_shows_the_short_placeholder(cx: &mut TestAppContext) {
    let placeholder = |cx: &mut VisualTestContext, view: &Entity<Explorer>| {
        view.update(cx, |v, cx| {
            v.filter_input_for_test().read(cx).placeholder_for_test()
        })
    };
    let (_ws, view, cx) = open_deep(cx, 1600.);
    let wide = placeholder(cx, &view);
    assert!(wide.contains("reports"), "{wide}");
    cx.simulate_resize(size(px(800.), px(650.)));
    settle(cx);
    let narrow = placeholder(cx, &view);
    assert_eq!(narrow, t!("filter.placeholder").into_owned());
    assert!(!narrow.contains("reports"));
    cx.simulate_resize(size(px(1600.), px(650.)));
    settle(cx);
    assert_eq!(placeholder(cx, &view), wide);
}

/// A click in the split tab's shared preview leaves the search box too.
#[gpui::test]
fn a_click_in_the_shared_preview_leaves_the_search_box(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1700.);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    let right = split(&ws, &left, cx);
    settle(cx);
    assert!(
        ws.update(cx, |ws, _| ws.shared_preview_for_test())
            .is_some()
    );
    cx.dispatch_action(FocusFilter);
    settle(cx);
    let active = ws.update(cx, |ws, _| ws.active_view().clone());
    assert_eq!(active, right);
    assert!(searching(cx, &active));
    let preview = bounds(cx, "shared-preview").center();
    press(cx, preview);
    assert!(!searching(cx, &active));
}

#[gpui::test]
fn a_narrow_window_keeps_everything_in_reach(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, 800.);
    let title = bounds(cx, "title-row");
    let search = bounds(cx, "filter-box");
    let caption = bounds(cx, "caption-lead");
    assert!(search.size.width >= px(200.), "{search:?}");
    assert!(search.left() >= title.left() && search.right() <= caption.left() + px(0.5));
    assert!(caption.right() + theme::h_ctl() * 3. <= px(800.));
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
    // At this width the buttons used to squeeze the path bar down to its 160 px minimum (#388).
    assert!(bounds(cx, "path-bar").size.width >= path_keep() - px(0.5));
    assert!(bounds(cx, "path-bar").right() <= actions.left());
}

/// Narrowest the selection's buttons let the path bar get before they fold into "⋯": the width
/// below which it keeps only the current folder (`explorer_topbar.rs` `path_compact_below`).
fn path_keep() -> Pixels {
    theme::scaled_px(200.)
}

/// #388: the selection's buttons fold into "⋯" before they squeeze the path bar, at whatever
/// width: with them in the row the path bar keeps [`path_keep`] and the current folder, and the
/// fold leaves the path bar as wide as with nothing selected, give or take the "● 3 ⋯".
#[gpui::test]
fn the_selection_never_squeezes_the_path_bar(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1600.);
    // `crumb`: the current folder's breadcrumb.
    let check =
        |cx: &mut VisualTestContext, view: &Entity<Explorer>, crumb: &'static str, at: String| {
            select(cx, view, false);
            let before = bounds(cx, "path-bar").size.width;
            select(cx, view, true);
            let path = bounds(cx, "path-bar");
            let actions = bounds(cx, "selection-actions");
            let folded = cx.debug_bounds("sel-more").is_some();
            let at = format!("{at}, folded {folded}: {path:?} {actions:?}");
            assert!(path.right() <= actions.left() + px(0.5), "{at}");
            if folded {
                assert!(
                    path.size.width + actions.size.width + theme::s_4() >= before - px(0.5),
                    "the fold takes only its own room: {at}, before {before:?}"
                );
            } else {
                assert!(path.size.width >= path_keep() - px(0.5), "{at}");
            }
            let current = bounds(cx, crumb);
            assert!(
                current.size.width > px(0.) && current.right() <= path.right() + px(0.5),
                "the current folder is drawn: {at}"
            );
        };
    for width in [1600., 1300., 1150., 1000., 900.] {
        cx.simulate_resize(size(px(width), px(650.)));
        settle(cx);
        // C: › top1 › projects › clients › acme › reports
        check(cx, &left, "crumb-5", format!("{width} px"));
    }
    // Split view: the right pane (on OTHER, drawn last) gets Copy to/Move to the other side.
    cx.simulate_resize(size(px(1600.), px(650.)));
    settle(cx);
    let right = split(&ws, &left, cx);
    for width in [2400., 2000., 1800., 1600., 1400.] {
        cx.simulate_resize(size(px(width), px(650.)));
        settle(cx);
        assert!(
            bounds(cx, "path-bar").left() > px(width / 2.),
            "the right pane's path bar is the one measured"
        );
        // C: › other
        check(cx, &right, "crumb-1", format!("{width} px, split"));
    }
}

/// Selects all of `view`'s items, or none.
fn select(cx: &mut VisualTestContext, view: &Entity<Explorer>, all: bool) {
    view.update(cx, |v, cx| {
        v.selection.clear();
        if all && let Some(entries) = v.view() {
            v.selection.select_all(&entries);
        }
        cx.notify();
    });
    settle(cx);
}

/// #388: folded into "⋯", the selection's buttons are still in reach from the keyboard: F6 to the
/// pane row, the arrows to "⋯", Enter opens the menu with Cut, Copy, Rename and Delete.
#[gpui::test]
fn the_folded_selection_opens_from_the_keyboard(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1000.);
    let view = split(&ws, &left, cx);
    select(cx, &view, true);
    assert!(
        cx.debug_bounds("sel-more").is_some(),
        "folded at this width"
    );
    view.update_in(cx, |v, window, cx| v.focus_toolbar(window, cx));
    settle(cx);
    for _ in 0..20 {
        if view.update(cx, |v, _| v.toolbar_at_for_test()) == Some("sel-more") {
            break;
        }
        cx.simulate_keystrokes("right");
        settle(cx);
    }
    assert_eq!(
        view.update(cx, |v, _| v.toolbar_at_for_test()),
        Some("sel-more")
    );
    // GPUI presses the focused control on the release of Enter.
    cx.simulate_keystrokes("enter");
    let keystroke = gpui::Keystroke::parse("enter").unwrap();
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }),
            cx,
        );
    });
    settle(cx);
    let labels = view
        .update(cx, |v, _| v.context_menu.as_ref().map(|m| m.labels()))
        .expect("Enter on ⋯ opens the selection's menu");
    let row = format!(
        "[{} {} {} {}]",
        t!("menu.cut"),
        t!("menu.copy"),
        t!("menu.rename"),
        t!("menu.delete")
    );
    assert_eq!(labels.last(), Some(&row), "{labels:?}");
}

/// A split pane in a small window has no room for the selection's buttons: they fold into "● 3"
/// and "⋯", which opens the selection's menu. A wide pane keeps the buttons.
#[gpui::test]
fn a_narrow_pane_folds_the_selection_actions_into_a_menu(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1200.);
    cx.simulate_keystrokes("ctrl-a");
    settle(cx);
    assert!(
        cx.debug_bounds("sel-more").is_none(),
        "a wide pane keeps its buttons"
    );

    cx.simulate_resize(size(px(1000.), px(650.)));
    settle(cx);
    let right = split(&ws, &left, cx);
    cx.simulate_keystrokes("ctrl-a");
    settle(cx);
    assert!(
        [&left, &right]
            .into_iter()
            .any(|v| v.update(cx, |v, _| !v.selection.is_empty())),
        "a pane with a selection"
    );
    assert!(cx.debug_bounds("sel-more").is_some(), "folded");
    let actions = bounds(cx, "selection-actions");
    assert!(
        actions.size.width < px(100.),
        "only the count and ⋯: {actions:?}"
    );
    assert!(bounds(cx, "sel-count").size.width > px(0.));
    // Both panes may hold a selection: whichever "⋯" is pressed opens its own pane's menu.
    let menus = |cx: &mut VisualTestContext| {
        [&left, &right]
            .into_iter()
            .filter(|v| v.update(cx, |v, _| v.context_menu.is_some()))
            .count()
    };
    assert_eq!(menus(cx), 0);
    let more = bounds(cx, "sel-more").center();
    press(cx, more);
    assert_eq!(menus(cx), 1, "⋯ opens the selection's menu");
}

/// #179: the Sort button sits in the pane row before the view switcher, and stays in split panes
/// (where the switcher folds into the view options button), opening its own pane's flyout.
#[gpui::test]
fn the_sort_button_sits_before_the_view_switcher(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1600.);
    let row = bounds(cx, "pane-row");
    let sort = bounds(cx, "sort-options");
    let options = bounds(cx, "view-options");
    assert!(sort.top() >= row.top() && sort.bottom() <= row.bottom());
    assert!(
        sort.right() <= options.left(),
        "{sort:?} before {options:?}"
    );

    let right = split(&ws, &left, cx);
    let sort = bounds(cx, "sort-options");
    press(cx, sort.center());
    let open: Vec<bool> = [&left, &right]
        .into_iter()
        .map(|v| v.update(cx, |v, _| v.sort_options_open_for_test()))
        .collect();
    assert_eq!(
        open.iter().filter(|o| **o).count(),
        1,
        "one pane's flyout: {open:?}"
    );
}

/// Tabs on top, as in VS Code: the title row (search, window buttons) stays on top, as with tabs
/// in the sidebar; the tab strip goes under it, over the panes.
#[gpui::test]
fn horizontal_tabs_go_under_the_title_row(cx: &mut TestAppContext) {
    let (ws, _view, cx) = open_deep(cx, 1000.);
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
    let strip = bounds(cx, "tab-strip");
    let search = bounds(cx, "filter-box");
    assert_eq!(title.top(), px(0.), "the title row is on top");
    assert!(strip.top() >= title.bottom() - px(0.5), "the tabs under it");
    assert!(
        strip.left() >= title.left() - px(0.5),
        "over the panes, not the sidebar"
    );
    assert!(search.right() <= title.right());
    assert!(
        cx.debug_bounds("caption-lead").is_some(),
        "the title row has the window buttons"
    );

    // More tabs than fit: the strip scrolls, and the active tab is scrolled into view.
    let paths: Vec<String> = (0..14).map(|i| format!(r"C:\tabs\tab{i}")).collect();
    for path in &paths {
        ws.update_in(cx, |ws, window, cx| {
            let at = ws.tab_views().count();
            ws.open_tab(PathBuf::from(path), at, true, window, cx);
        });
    }
    settle(cx);
    let strip = bounds(cx, "tab-strip-tabs");
    let last = ws.update(cx, |ws, _| ws.tab_views().count() - 1);
    let tab = cx
        .debug_bounds(Box::leak(format!("tab-{last}").into_boxed_str()))
        .expect("the active tab is drawn");
    assert!(
        tab.left() >= strip.left() - px(1.) && tab.right() <= strip.right() + px(1.),
        "the active (last) tab is in view: {tab:?} in {strip:?}"
    );
}

/// Rests the mouse at `at` long enough for gpui to show the tooltip there.
fn hover_for_tooltip(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_move(at, None, Modifiers::none());
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(800));
    settle(cx);
}

/// #246: a tooltip at the mouse starts below the pointer's arrow, not at its tip, so the arrow
/// doesn't cover it; the sidebar button's sits under the button like Back and Forward beside it.
#[gpui::test]
fn tooltips_clear_the_pointer(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, 1200.);
    // The path bar's blank part: a tooltip at the mouse.
    let blank = bounds(cx, "path-blank");
    let at = point(blank.center().x, blank.top() + px(4.));
    hover_for_tooltip(cx, at);
    let card = bounds(cx, "tooltip-card");
    assert!(
        card.top() >= at.y + px(18.),
        "below the arrow: mouse {:?}, tooltip {:?}",
        at.y,
        card.top()
    );
    cx.simulate_mouse_move(point(px(600.), px(500.)), None, Modifiers::none());
    settle(cx);

    // The sidebar button: under the button wherever the mouse is on it.
    let button = bounds(cx, "sidebar-toggle");
    for at in [
        point(button.center().x, button.top() + px(2.)),
        point(button.right() - px(2.), button.bottom() - px(2.)),
    ] {
        hover_for_tooltip(cx, at);
        let card = bounds(cx, "tooltip-card");
        assert!(
            near(card.top(), button.bottom() + theme::s_8(), 1.),
            "under the button: button {:?}, tooltip {:?}",
            button.bottom(),
            card.top()
        );
        cx.simulate_mouse_move(point(px(600.), px(500.)), None, Modifiers::none());
        settle(cx);
    }
}

/// The search box's placeholder always starts with the same verb, whatever the scope (#388): the
/// folder shown, the subfolders of it, the whole PC. The shortcut hint stays on the folder one.
#[gpui::test]
fn the_search_placeholder_follows_the_scope(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    rust_i18n::set_locale("en");
    let (_ws, view, cx) = open_deep(cx, 1600.);
    let shown = |cx: &mut VisualTestContext| {
        settle(cx);
        view.update(cx, |v, cx| {
            v.filter_input_for_test().read(cx).placeholder_for_test()
        })
    };
    assert_eq!(shown(cx), "Search reports (Ctrl+F)");
    view.update(cx, |v, cx| v.set_scope(Scope::Subfolders, cx));
    assert_eq!(shown(cx), "Search subfolders of reports");
    view.update(cx, |v, cx| v.set_scope(Scope::Computer, cx));
    assert_eq!(shown(cx), "Search this PC");
    view.update(cx, |v, cx| v.set_scope(Scope::Here, cx));
    assert_eq!(shown(cx), "Search reports (Ctrl+F)");

    // Other languages go through the same function; only the verb and the wording differ.
    let in_zh = |cx: &mut VisualTestContext, scope: Scope, locale: &str| {
        view.update(cx, |v, cx| {
            v.set_scope(scope, cx);
            v.filter_placeholder(locale, false)
        })
    };
    assert_eq!(in_zh(cx, Scope::Here, "zh-TW"), "搜尋「reports」（Ctrl+F）");
    assert_eq!(
        in_zh(cx, Scope::Subfolders, "zh-TW"),
        "搜尋「reports」的子資料夾"
    );
    assert_eq!(in_zh(cx, Scope::Computer, "zh-TW"), "搜尋整台電腦");
    assert_eq!(in_zh(cx, Scope::Here, "zh-CN"), "搜索“reports”（Ctrl+F）");
    assert_eq!(in_zh(cx, Scope::Computer, "zh-CN"), "搜索整台电脑");
}
