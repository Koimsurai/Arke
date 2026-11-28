//! Tabs on top when there are many (#533, #541): titles stay readable, the strip scrolls with
//! buttons at its ends, and an inactive tab's hidden close button takes no room from its title
//! (`workspace_tabstrip.rs`).

use std::sync::Arc;

use gpui::{Bounds, Modifiers, MouseButton, Pixels, Point, px, size};

use super::*;
use crate::theme;

/// Every tab shows this folder name, the title measured in #533.
const NAME: &str = "Quarterly Reports 2026";

fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn folder(ix: usize) -> String {
    format!(r"C:\q{ix}\{NAME}")
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} drawn"))
}

/// `debug_bounds` takes static names.
fn tab_part(part: &str, ix: usize) -> &'static str {
    Box::leak(format!("{part}-{ix}").into_boxed_str())
}

/// Opens tabs on folders `from..to` after the ones already open, each waited on to list.
fn open_more(ws: &Entity<Workspace>, cx: &mut VisualTestContext, from: usize, to: usize) {
    for ix in from..to {
        let path = folder(ix);
        ws.update_in(cx, |ws, window, cx| {
            let at = ws.tab_views().count();
            ws.open_tab(PathBuf::from(&path), at, true, window, cx);
        });
        let view = ws.update(cx, |ws, _| ws.active_view().clone());
        wait_until(cx, &view, "load", loaded(&path));
    }
}

/// Tabs on top with `tabs` tabs open (of `folders` folders named [`NAME`]) in a window `width`
/// wide; the first tab is active.
fn strip_with_tabs(
    cx: &mut TestAppContext,
    folders: usize,
    tabs: usize,
    width: f32,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    for ix in 0..folders {
        fs.insert_many(folder(ix), 1);
    }
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), &folder(0));
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(&folder(0)));
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| st.tab_layout = crate::settings::TabLayout::Horizontal,
                cx,
            );
        });
    });
    open_more(&ws, cx, 1, tabs);
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    cx.simulate_resize(size(px(width), px(700.)));
    settle(cx);
    (ws, cx)
}

/// How wide `text` is in the UI font at the tabs' size.
fn text_width(cx: &mut VisualTestContext, text: &str) -> Pixels {
    cx.update(|window, _| {
        let run = gpui::TextRun {
            len: text.len(),
            font: theme::ui_font_at(gpui::FontWeight::NORMAL),
            color: gpui::Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(
                gpui::SharedString::from(text.to_owned()),
                theme::fs_14(),
                &[run],
                None,
            )
            .width
    })
}

/// The first `chars` characters of the title and the ellipsis after them.
fn title_start(cx: &mut VisualTestContext, chars: usize) -> Pixels {
    let start: String = NAME.chars().take(chars).collect();
    text_width(cx, &format!("{start}\u{2026}"))
}

fn offset(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Pixels {
    ws.update(cx, |ws, _| ws.strip_offset_for_test())
}

fn press(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    settle(cx);
}

/// #533: twelve tabs in a 1200 px window used to shrink to "Qu…". They now stop at the minimum
/// width and the strip scrolls, so each title keeps a dozen characters on an inactive tab and
/// most of that with the close button showing.
#[gpui::test]
fn many_tabs_keep_readable_titles(cx: &mut TestAppContext) {
    let (ws, cx) = strip_with_tabs(cx, 12, 12, 1200.);
    assert!(
        ws.update(cx, |ws, _| ws.strip_max_offset_for_test()) > px(0.),
        "twelve tabs overflow the strip"
    );
    let inactive_room = title_start(cx, 12);
    let active_room = title_start(cx, 10);
    for ix in 0..12 {
        let tab = bounds(cx, tab_part("tab", ix));
        assert!(
            tab.size.width >= theme::scaled_px(160.) - px(0.5),
            "tab {ix} stops at the minimum width: {tab:?}"
        );
        let title = bounds(cx, tab_part("tab-title", ix)).size.width;
        let room = if ix == 0 { active_room } else { inactive_room };
        assert!(
            title >= room,
            "tab {ix}'s title has {title:?}, wanted at least {room:?}"
        );
    }
}

/// An inactive tab's close button is hidden and leaves the title its room; it takes it only
/// while the pointer rests on the tab (#533).
#[gpui::test]
fn an_inactive_tab_keeps_the_close_buttons_room(cx: &mut TestAppContext) {
    let (_ws, cx) = strip_with_tabs(cx, 3, 3, 1200.);
    let active = bounds(cx, "tab-title-0").size.width;
    let inactive = bounds(cx, "tab-title-1").size.width;
    let tab_active = bounds(cx, "tab-0").size.width;
    let tab_inactive = bounds(cx, "tab-1").size.width;
    assert!(
        (tab_active - tab_inactive).abs() < px(0.5),
        "the tabs are as wide: {tab_active:?} and {tab_inactive:?}"
    );
    assert!(
        inactive >= active + theme::s_20(),
        "the inactive title ({inactive:?}) has the close button's room the active one ({active:?}) gives up"
    );

    // The pointer on the inactive tab: its close button shows and the title gives way.
    let tab = bounds(cx, "tab-1");
    let at = Point::new(tab.left() + px(20.), tab.center().y);
    cx.simulate_mouse_move(at, None, Modifiers::none());
    settle(cx);
    let hovered = bounds(cx, "tab-title-1").size.width;
    assert!(
        (hovered - active).abs() < px(0.5),
        "under the pointer the title is as wide as the active one's: {hovered:?}, {active:?}"
    );

    // And back once the pointer leaves the strip.
    cx.simulate_mouse_move(Point::new(px(600.), px(500.)), None, Modifiers::none());
    settle(cx);
    let left = bounds(cx, "tab-title-1").size.width;
    assert!(
        (left - inactive).abs() < px(0.5),
        "the room comes back: {left:?}, {inactive:?}"
    );
}

/// Whether the strip can scroll back and forward, as its buttons show it.
fn edges(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> (bool, bool) {
    ws.update(cx, |ws, _| ws.strip_edges_for_test())
}

/// #541: the scroll buttons show only while the tabs overflow, sit at both ends of the tabs, are
/// dimmed at the ends, and step the strip about a tab at a time.
#[gpui::test]
fn scroll_buttons_show_when_the_tabs_overflow(cx: &mut TestAppContext) {
    let (ws, cx) = strip_with_tabs(cx, 14, 3, 1200.);
    assert!(
        cx.debug_bounds("strip-scroll-left").is_none()
            && cx.debug_bounds("strip-scroll-right").is_none(),
        "three tabs fit: no buttons"
    );

    open_more(&ws, cx, 3, 14);
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    settle(cx);
    let tabs = bounds(cx, "tab-strip-tabs");
    let left = bounds(cx, "strip-scroll-left");
    let right = bounds(cx, "strip-scroll-right");
    assert!(
        left.right() <= tabs.left() + px(0.5) && right.left() >= tabs.right() - px(0.5),
        "the buttons at both ends of the tabs: {left:?} {tabs:?} {right:?}"
    );
    assert_eq!(offset(&ws, cx), px(0.), "the first tab, active, is in view");
    assert_eq!(edges(&ws, cx), (false, true), "only forward at the start");

    // Back at the start does nothing.
    press(cx, left.center());
    assert_eq!(offset(&ws, cx), px(0.));

    // Forward: about one tab.
    let step = bounds(cx, "tab-1").size.width + theme::s_4();
    press(cx, right.center());
    let moved = -offset(&ws, cx);
    assert!(
        moved > px(0.) && moved <= step + px(1.),
        "one press moves about a tab: {moved:?} of {step:?}"
    );
    assert_eq!(edges(&ws, cx), (true, true));

    // On to the end, where forward stops.
    let max = ws.update(cx, |ws, _| ws.strip_max_offset_for_test());
    for _ in 0..20 {
        if offset(&ws, cx) <= -max + px(0.5) {
            break;
        }
        let at = bounds(cx, "strip-scroll-right").center();
        press(cx, at);
    }
    assert!(offset(&ws, cx) <= -max + px(0.5), "the end is reached");
    assert_eq!(edges(&ws, cx), (true, false), "only back at the end");
    let end = offset(&ws, cx);
    let at = bounds(cx, "strip-scroll-right").center();
    press(cx, at);
    assert_eq!(offset(&ws, cx), end, "forward at the end does nothing");
    let last = tab_part("tab", 13);
    let last_tab = bounds(cx, last);
    assert!(
        last_tab.right() <= tabs.right() + px(1.),
        "the last tab shows: {last_tab:?} in {tabs:?}"
    );

    // Back steps toward the start again.
    let at = bounds(cx, "strip-scroll-left").center();
    press(cx, at);
    assert!(offset(&ws, cx) > end, "back moves toward the start");

    // Switching tabs still brings the active one into view.
    for ix in [5, 0] {
        ws.update_in(cx, |ws, window, cx| ws.activate(ix, window, cx));
        settle(cx);
    }
    let first = bounds(cx, "tab-0");
    assert!(
        first.left() >= tabs.left() - px(1.),
        "the active tab is scrolled into view: {first:?} in {tabs:?}"
    );

    // Fewer tabs again: the buttons go.
    for _ in 0..11 {
        ws.update_in(cx, |ws, window, cx| {
            let last = ws.tab_views().count() - 1;
            ws.close_tab(last, window, cx);
        });
    }
    settle(cx);
    assert!(
        cx.debug_bounds("strip-scroll-left").is_none()
            && cx.debug_bounds("strip-scroll-right").is_none(),
        "three tabs fit again: no buttons"
    );
}
