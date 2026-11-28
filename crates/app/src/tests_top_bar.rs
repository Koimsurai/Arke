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
    let (_ws, view, cx) = open_deep(cx, 1600.);
    assert!(
        cx.debug_bounds("pane-info").is_some(),
        "the item count shows"
    );
    // Narrow enough for the item count to go (#474), and the selection to give way.
    cx.simulate_resize(size(px(1000.), px(650.)));
    settle(cx);
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
/// width: with any of them in the row the path bar keeps [`path_keep`] and the current folder,
/// and the full fold leaves the path bar as wide as with nothing selected, give or take the
/// "● 3 ⋯".
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
            let folded = folded(cx);
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
    // Only the right pane holds a selection, so the buttons found are its own.
    select(cx, &left, false);
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

/// Every button in "⋯": only "● 3 ⋯" is drawn (Delete is the last to go).
fn folded(cx: &mut VisualTestContext) -> bool {
    cx.debug_bounds("sel-more").is_some() && cx.debug_bounds("sel-delete").is_none()
}

/// Selects `view`'s first item alone.
fn select_one(cx: &mut VisualTestContext, view: &Entity<Explorer>) {
    view.update(cx, |v, cx| {
        v.selection.clear();
        if let Some(entry) = v.view().and_then(|entries| entries.first().cloned()) {
            v.selection.select_name(entry.name);
        }
        cx.notify();
    });
    settle(cx);
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 1);
}

/// One item's buttons, in the order they move into "⋯".
const ONE_ITEM_BUTTONS: [&str; 5] = [
    "sel-open",
    "sel-rename",
    "sel-cut",
    "sel-copy",
    "sel-delete",
];

/// #388, #474, #528: with one item selected (one more button, Rename) and the preview pane open,
/// the row gives way in steps as the window narrows: the view switcher, then Sort into "⋯", then
/// the summary shortens to "● 1", the buttons move into "⋯" one at a time (the least used first,
/// Delete last), and only then does the path bar lose a parent folder.
#[gpui::test]
fn the_buttons_fold_before_the_path_bar_collapses(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 2200.);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    let levels = arke_core::path::breadcrumbs(std::path::Path::new(DEEP)).len();
    // With the whole path: everything drawn; some buttons in "⋯"; only "● 1 ⋯".
    let mut seen = [false; 3];
    // Steps smaller than the room any stage spans.
    for width in (500..=2200_u16).rev().step_by(20) {
        cx.simulate_resize(size(px(f32::from(width)), px(650.)));
        settle(cx);
        select_one(cx, &view);
        let drawn = ONE_ITEM_BUTTONS.map(|id| cx.debug_bounds(id).is_some());
        let more = cx.debug_bounds("sel-more").is_some();
        let pill = cx.debug_bounds("sel-count").is_some();
        let sort = cx.debug_bounds("sort-options").is_some();
        let switcher = cx.debug_bounds("view-switcher").is_some();
        let shown = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window).len());
        let row = view.update(cx, |v, _| v.row_debug_for_test());
        let at = format!(
            "{width} px: {drawn:?}, ⋯ {more}, pill {pill}, sort {sort}, switcher {switcher}, \
             {shown} of {levels}; {row}"
        );
        // The least used go first: a button in "⋯" never has a less used one still drawn.
        assert!(drawn.windows(2).all(|w| !w[0] || w[1]), "{at}");
        assert!(
            !switcher || sort,
            "the view switcher goes before Sort: {at}"
        );
        assert_eq!(
            more,
            drawn.contains(&false) || !sort,
            "⋯ holds what isn't drawn: {at}"
        );
        assert!(
            !pill || !sort,
            "the view tools go before the summary shortens: {at}"
        );
        assert!(
            !drawn.contains(&false) || pill,
            "the summary shortens before any button goes: {at}"
        );
        assert!(
            shown == levels || (pill && !sort && drawn.iter().all(|d| !d)),
            "the path bar gives way last: {at}"
        );
        if shown == levels {
            if !more && !pill {
                seen[0] = true;
            } else if drawn.contains(&false) && drawn[4] {
                seen[1] = true;
            } else if !drawn[4] {
                seen[2] = true;
            }
        }
    }
    assert_eq!(
        seen, [true; 3],
        "every stage keeps the whole path at some width"
    );
    // Too narrow for the path even with the buttons folded: it collapses.
    cx.simulate_resize(size(px(500.), px(650.)));
    settle(cx);
    select_one(cx, &view);
    assert!(folded(cx), "folded when narrow");
    let shown = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window).len());
    assert!(
        shown < levels,
        "the path bar collapses: {shown} of {levels}"
    );
}

/// #528: a five-level path in a moderately narrow window keeps every button and every level,
/// the summary shrinking to "● 1" and Sort in "⋯" (#474). At these widths 0.10.1 folded every
/// button into "⋯": the buttons with the summary "1 selected" didn't leave the path bar its whole
/// width, and "● 1 ⋯" did.
#[gpui::test]
fn a_moderately_narrow_pane_keeps_the_buttons_and_the_path(cx: &mut TestAppContext) {
    const FIVE: &str = r"C:\top1\projects\clients\acme";
    let (_ws, view, cx) = open_deep(cx, 2000.);
    view.update(cx, |e, cx| {
        e.navigate(PathBuf::from(FIVE), NavKind::New, cx);
    });
    wait_until(cx, &view, "five levels", loaded(FIVE));
    settle(cx);
    let levels = arke_core::path::breadcrumbs(std::path::Path::new(FIVE)).len();
    assert_eq!(levels, 5);
    let mut found = None;
    for width in (600..=2000_u16).rev().step_by(10) {
        cx.simulate_resize(size(px(f32::from(width)), px(650.)));
        settle(cx);
        select_one(cx, &view);
        let all = ONE_ITEM_BUTTONS
            .into_iter()
            .all(|id| cx.debug_bounds(id).is_some());
        let pill = cx.debug_bounds("sel-count").is_some();
        let more = cx.debug_bounds("sel-more").is_some();
        let shown = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window).len());
        if all && pill && more && shown == levels {
            found = Some(width);
            break;
        }
    }
    let width = found.expect("a width with \"● 1\", every button and every level");
    let path = bounds(cx, "path-bar");
    let actions = bounds(cx, "selection-actions");
    assert!(
        path.right() <= actions.left() + px(0.5),
        "{width} px: {path:?} {actions:?}"
    );
    // Drawn whole, not cut off by the row.
    let delete = bounds(cx, "sel-delete");
    assert!(
        delete.right() <= actions.right() + px(0.5),
        "{width} px: {delete:?} {actions:?}"
    );
}

/// #474: with nothing selected and a deep path, the view tools give way as the window narrows,
/// the view switcher first (to the view options button alone), then Sort into a "⋯" of its own;
/// the path bar loses a level only after both.
#[gpui::test]
fn the_view_tools_fold_before_the_path_bar_collapses(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 2000.);
    let levels = arke_core::path::breadcrumbs(std::path::Path::new(DEEP)).len();
    // With the whole path: the switcher and Sort; the view button and Sort; the view button
    // and "⋯".
    let mut seen = [false; 3];
    let mut before: Option<(u16, (bool, bool))> = None;
    for width in (400..=2000_u16).rev().step_by(20) {
        let stage = view_tools_at(cx, &view, width, levels, &mut seen);
        // "⋯" for Sort saves only 16 px: walk back over a step that changed 2 px at a time, so
        // a stage narrower than a step is seen too.
        if let Some((wider, was)) = before
            && was != stage
        {
            for between in (width + 2..wider).rev().step_by(2) {
                view_tools_at(cx, &view, between, levels, &mut seen);
            }
        }
        before = Some((width, stage));
    }
    assert_eq!(
        seen, [true; 3],
        "every stage keeps the whole path at some width"
    );
}

/// The window `width` wide with nothing selected: checks the pane row's view tools against the
/// path bar, notes in `seen` which stage kept the whole path, and returns the switcher's and
/// Sort's.
fn view_tools_at(
    cx: &mut VisualTestContext,
    view: &Entity<Explorer>,
    width: u16,
    levels: usize,
    seen: &mut [bool; 3],
) -> (bool, bool) {
    cx.simulate_resize(size(px(f32::from(width)), px(650.)));
    settle(cx);
    let switcher = cx.debug_bounds("view-switcher").is_some();
    let sort = cx.debug_bounds("sort-options").is_some();
    let options = cx.debug_bounds("view-options").is_some();
    let more = cx.debug_bounds("row-more").is_some();
    let shown = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window).len());
    let row = view.update(cx, |v, _| v.row_debug_for_test());
    let at = format!(
        "{width} px: switcher {switcher}, sort {sort}, more {more}, {shown} of {levels}; {row}"
    );
    assert!(options, "the view options button never folds: {at}");
    assert!(!switcher || sort, "the switcher goes before Sort: {at}");
    assert_eq!(more, !sort, "⋯ holds Sort: {at}");
    assert!(
        shown == levels || (!switcher && !sort),
        "the path bar gives way last: {at}"
    );
    if shown == levels {
        seen[0] |= switcher && sort;
        seen[1] |= !switcher && sort;
        seen[2] |= more;
    }
    (switcher, sort)
}

/// #474: Sort folded into "⋯" is in its menu, with the current column and direction checked,
/// and in reach from the keyboard: F6 to the pane row, the arrows to "⋯", Enter.
#[gpui::test]
fn the_folded_sort_is_in_the_menu(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 2000.);
    let mut found = None;
    for width in (400..=2000_u16).rev().step_by(20) {
        cx.simulate_resize(size(px(f32::from(width)), px(650.)));
        settle(cx);
        if cx.debug_bounds("row-more").is_some() {
            found = Some(width);
            break;
        }
    }
    found.expect("a width with Sort in ⋯");
    view.update_in(cx, Explorer::focus_toolbar);
    settle(cx);
    for _ in 0..20 {
        if view.update(cx, |v, _| v.toolbar_at_for_test()) == Some("row-more") {
            break;
        }
        cx.simulate_keystrokes("right");
        settle(cx);
    }
    assert_eq!(
        view.update(cx, |v, _| v.toolbar_at_for_test()),
        Some("row-more")
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
        .update(cx, |v, _| {
            v.context_menu
                .as_ref()
                .map(crate::explorer::ContextMenuForTest::labels)
        })
        .expect("Enter on ⋯ opens the folder's menu");
    let sort_by = format!("{} ›", t!("settings.sort_by"));
    assert!(labels.contains(&sort_by), "{labels:?}");
}

/// #474: each pane of a split tab folds its own row: the one on the deep path gives Sort up
/// while the one on a short path keeps it.
#[gpui::test]
fn each_pane_folds_its_own_row(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 2400.);
    let right = split(&ws, &left, cx);
    let sort_folded = |cx: &mut VisualTestContext, view: &Entity<Explorer>| {
        view.update(cx, |v, _| v.row_fold.sort_in_more())
    };
    let mut found = None;
    for width in (800..=2400_u16).rev().step_by(20) {
        cx.simulate_resize(size(px(f32::from(width)), px(650.)));
        settle(cx);
        if sort_folded(cx, &left) && !sort_folded(cx, &right) {
            found = Some(width);
            break;
        }
    }
    let width = found.expect("a width where the deep pane folds Sort and the other doesn't");
    let levels = arke_core::path::breadcrumbs(std::path::Path::new(DEEP)).len();
    let shown = left.update_in(cx, |v, window, _| v.crumb_labels_for_test(window).len());
    assert_eq!(
        shown, levels,
        "{width} px: Sort went first, the path keeps its levels"
    );
    // The right pane, drawn last, still draws its Sort button.
    let sort = bounds(cx, "sort-options");
    assert!(
        sort.left() > px(f32::from(width) / 2.),
        "{width} px: {sort:?}"
    );
}

/// Narrows the window until the right pane of a split tab folds synchronized browsing and
/// compare into "⋯" with its path whole, checking on the way that its path keeps its levels while
/// they are drawn and that Sort went first. Returns the width.
fn fold_the_right_toggles(cx: &mut VisualTestContext, right: &Entity<Explorer>) -> u16 {
    let levels = arke_core::path::breadcrumbs(std::path::Path::new(OTHER)).len();
    let state =
        |cx: &mut VisualTestContext| right.update(cx, |v, _| (v.row_fold, v.row_debug_for_test()));
    let mut found = None;
    for width in (600..=1800_u16).rev().step_by(10) {
        cx.simulate_resize(size(px(f32::from(width)), px(650.)));
        settle(cx);
        let (fold, row) = state(cx);
        let shown = right.update_in(cx, |v, window, _| v.crumb_labels_for_test(window).len());
        let at = format!("{width} px: {fold:?}, {shown} of {levels}; {row}");
        // While the toggles are drawn, the path keeps its levels; they go after Sort.
        assert!(
            fold.toggles_in_more() || shown == levels,
            "the path bar gives way last: {at}"
        );
        assert!(
            !fold.toggles_in_more() || fold.sort_in_more(),
            "Sort goes first: {at}"
        );
        if fold.toggles_in_more() && shown == levels {
            found = Some(width);
            break;
        }
    }
    found.expect("a width where the right pane folds its toggles, path whole")
}

/// #474: the dual pane toggles folded into "⋯" take access keys like every other row: N for
/// synchronized browsing and M for compare in English, written after the text in Chinese, and
/// the key toggles them.
#[gpui::test]
fn the_folded_toggles_take_access_keys(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1800.);
    let right = split(&ws, &left, cx);
    let width = fold_the_right_toggles(cx, &right) - 60;
    // Further in, so the Chinese texts' widths can't bring the toggles back out.
    cx.simulate_resize(size(px(f32::from(width)), px(650.)));
    settle(cx);
    let menu = |cx: &mut VisualTestContext| {
        right.update_in(cx, Explorer::open_more_menu_for_test);
        settle(cx);
        let sync = t!("compare.sync_tip").into_owned();
        let compare = t!("compare.compare_tip").into_owned();
        right
            .update(cx, |v, _| {
                v.context_menu
                    .as_ref()
                    .map(|m| (m.key_of(&sync), m.key_of(&compare)))
            })
            .expect("⋯ opens the folder's menu")
    };

    rust_i18n::set_locale("en");
    let (sync, compare) = menu(cx);
    assert_eq!(
        sync,
        Some(('N', t!("compare.sync_tip").into_owned())),
        "{width} px"
    );
    assert_eq!(compare, Some(('M', t!("compare.compare_tip").into_owned())));
    cx.simulate_keystrokes("n");
    settle(cx);
    assert!(
        right.update(cx, |v, _| v.context_menu.is_none()),
        "N runs it"
    );
    assert!(right.update(cx, |v, _| v.pane.synced), "synchronized");

    rust_i18n::set_locale("zh-TW");
    let (sync, compare) = menu(cx);
    assert_eq!(sync, Some(('N', "同步瀏覽(N)".to_owned())));
    assert_eq!(compare, Some(('M', "比較資料夾(M)".to_owned())));
    cx.simulate_keystrokes("m");
    settle(cx);
    rust_i18n::set_locale("en");
    assert!(
        right.update(cx, |v, _| v.context_menu.is_none()),
        "M runs it"
    );
    assert!(right.update(cx, |v, _| v.pane.comparing), "comparing");
}

/// #474: in a split tab, the right pane (on a short path) folds Sort, then its item count, then
/// synchronized browsing and compare into "⋯", before its path loses a level. The menu shows
/// them checked while on, and they toggle as the buttons do.
#[gpui::test]
fn the_dual_pane_toggles_fold_into_the_menu(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1800.);
    let right = split(&ws, &left, cx);
    let width = fold_the_right_toggles(cx, &right);
    assert!(
        cx.debug_bounds("row-more").is_some(),
        "{width} px: ⋯ holds them"
    );
    let sync = t!("compare.sync_tip").into_owned();
    let compare = t!("compare.compare_tip").into_owned();
    let menu = |cx: &mut VisualTestContext| {
        right.update_in(cx, Explorer::open_more_menu_for_test);
        settle(cx);
        right
            .update(cx, |v, _| {
                v.context_menu
                    .as_ref()
                    .map(|m| (m.labels(), m.checked_labels()))
            })
            .expect("⋯ opens the folder's menu")
    };
    let (labels, checked) = menu(cx);
    assert!(
        labels.contains(&sync) && labels.contains(&compare),
        "{labels:?}"
    );
    assert!(!checked.contains(&sync), "off: {checked:?}");
    // Enter on synchronized browsing turns it on, as its button does, and closes the menu.
    let slot = labels.iter().position(|l| *l == sync).expect("sync row");
    right.update(cx, |v, cx| {
        v.context_menu
            .as_mut()
            .expect("menu")
            .highlight_for_test(slot);
        cx.notify();
    });
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert!(
        right.update(cx, |v, _| v.context_menu.is_none()),
        "the menu closes"
    );
    assert!(right.update(cx, |v, _| v.pane.synced), "synchronized");
    let (_, checked) = menu(cx);
    assert!(checked.contains(&sync), "on: {checked:?}");
}

/// #538: while the window narrows a frame at a time (as a resize does, or the preview pane sliding
/// in), the path bar's levels fold into "…" and the drive stays whole in every frame. The levels
/// are decided from the bar's width on the frame before, so they come out a little over; the
/// middle levels and the current folder take that, never the drive's short name.
#[gpui::test]
fn the_drive_stays_whole_while_the_path_folds(cx: &mut TestAppContext) {
    let (_ws, _view, cx) = open_deep(cx, 1600.);
    let drive = bounds(cx, "crumb-0").size.width;
    let mut folded = false;
    for width in (600..=1600_u16).rev().step_by(23) {
        cx.simulate_resize(size(px(f32::from(width)), px(650.)));
        // One frame per size, as an animation draws: no settling in between.
        cx.run_until_parked();
        if bounds(cx, "path-bar").size.width < px(260.) {
            break;
        }
        let crumb = cx
            .debug_bounds("crumb-0")
            .unwrap_or_else(|| panic!("{width} px: the drive is drawn"));
        assert!(
            near(crumb.size.width, drive, 0.5),
            "{width} px: the drive is {crumb:?}, whole {drive:?}"
        );
        folded |= cx.debug_bounds("crumb-more").is_some();
    }
    assert!(folded, "the path folded on the way");
}

/// Resizes the window and draws the way the platform does: a frame at the new size, then one
/// for each next-frame request, and no other. [`settle`] refreshes on its own, which hid a row
/// that asked for its second frame while drawing, where the request is dropped (#528).
fn resize_like_the_platform(cx: &mut VisualTestContext, width: f32) {
    cx.simulate_resize(size(px(width), px(650.)));
    cx.run_until_parked();
    for _ in 0..8 {
        if cx.update(gpui::Window::simulate_next_frame) == 0 {
            break;
        }
        cx.run_until_parked();
    }
}

/// What the pane row drew last: which of one item's buttons, "⋯", the pill, and whether the
/// path bar collapsed levels into "…".
fn row_state(cx: &mut VisualTestContext) -> ([bool; 5], bool, bool, bool) {
    let drawn = ONE_ITEM_BUTTONS.map(|id| cx.debug_bounds(id).is_some());
    let more = cx.debug_bounds("sel-more").is_some();
    let pill = cx.debug_bounds("sel-count").is_some();
    let elided = cx.debug_bounds("crumb-more").is_some();
    (drawn, more, pill, elided)
}

/// #528: as the window narrows step by step, each step's row is laid out for its own width at
/// once, not the width before: the path bar keeps every level while any selection button is
/// drawn, and drawing more frames changes nothing. Before, the path bar and the selection were
/// laid out a resize step behind, eliding the path next to buttons.
#[gpui::test]
fn the_pane_row_keeps_up_with_a_resize(cx: &mut TestAppContext) {
    let (_ws, view, cx) = open_deep(cx, 1600.);
    select_one(cx, &view);
    for width in (600..=1600_u16).rev().step_by(25) {
        resize_like_the_platform(cx, f32::from(width));
        let now = row_state(cx);
        let (drawn, _, _, elided) = now;
        assert!(
            !elided || !drawn.contains(&true),
            "{width} px: buttons {drawn:?} next to a collapsed path bar"
        );
        settle(cx);
        assert_eq!(
            row_state(cx),
            now,
            "{width} px: settled on the first frames"
        );
    }
}

/// #388, #528: the selection's buttons moved into "⋯" are still in reach from the keyboard: F6 to
/// the pane row, the arrows to "⋯", Enter opens the menu with Cut, Copy, Rename and Delete.
#[gpui::test]
fn the_folded_selection_opens_from_the_keyboard(cx: &mut TestAppContext) {
    let (ws, left, cx) = open_deep(cx, 1000.);
    let view = split(&ws, &left, cx);
    select(cx, &view, true);
    assert!(
        cx.debug_bounds("sel-more").is_some(),
        "buttons in ⋯ at this width"
    );
    view.update_in(cx, Explorer::focus_toolbar);
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
        .update(cx, |v, _| {
            v.context_menu
                .as_ref()
                .map(crate::explorer::ContextMenuForTest::labels)
        })
        .expect("Enter on ⋯ opens the selection's menu");
    let row = format!(
        "[{} {} {} {}]",
        t!("menu.cut"),
        t!("menu.copy"),
        t!("menu.rename"),
        t!("menu.delete")
    );
    assert!(labels.contains(&row), "{labels:?}");
}

/// A split pane in a small window has no room for any of the selection's buttons: they all fold
/// into "● 3" and "⋯", which opens the selection's menu. A wide pane keeps the buttons.
#[gpui::test]
fn a_narrow_pane_folds_the_selection_actions_into_a_menu(cx: &mut TestAppContext) {
    // Wide enough for the deep path to show every level with the buttons and the view tools
    // out.
    let (ws, left, cx) = open_deep(cx, 2000.);
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
    assert!(folded(cx), "folded");
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
