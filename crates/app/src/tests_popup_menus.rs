//! GPUI tests for menus in windows of their own (`popup.rs`): each level opens in a popup window
//! owned by the main window and placed against the monitor's work area, not the window; keys stay
//! in the main window; the menu closes on a press outside it, when the window goes to the
//! background, and when it moves.

use gpui::{AnyWindowHandle, Bounds, Modifiers, MouseButton, point, px, size};

use super::*;
use crate::popup::{Monitor, ScreenRect};

/// The popup windows open now.
fn popups(cx: &mut VisualTestContext) -> Vec<AnyWindowHandle> {
    cx.update(|_, cx| {
        crate::popup::open_popups(cx)
            .into_iter()
            .map(Into::into)
            .collect()
    })
}

/// The window of menu level `level` and where its panel was placed on the screen.
fn level_popup(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    level: usize,
) -> Option<(AnyWindowHandle, ScreenRect)> {
    let (window, placed) =
        view.update(cx, |v, _| v.context_menu.as_ref()?.popup_for_test(level))?;
    Some((window, placed?))
}

/// The main window's client area on the screen.
fn window_on_screen(cx: &mut VisualTestContext) -> ScreenRect {
    cx.update(|window, _| {
        crate::popup::rect_to_screen(
            window,
            Bounds::new(point(px(0.), px(0.)), window.viewport_size()),
        )
    })
}

fn open_menu_at(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    at: gpui::Point<gpui::Pixels>,
) {
    cx.update(|window, cx| view.update(cx, |v, cx| v.open_context_menu(at, window, cx)));
    cx.run_until_parked();
}

fn press(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn one_monitor(work: ScreenRect) -> Vec<Monitor> {
    vec![Monitor {
        bounds: work,
        work,
        scale: 1.,
    }]
}

/// The menu opens in a window of its own, at the pointer; keys still reach it from the main
/// window, and a press in the main window closes it and its window.
#[gpui::test]
fn the_context_menu_opens_in_a_window_of_its_own(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1), file("b.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    let main = cx.update(|window, _| window.window_handle());
    assert!(popups(cx).is_empty());

    open_menu_at(&view, cx, point(px(300.), px(200.)));
    let (popup, placed) = level_popup(&view, cx, 0).expect("the menu has a window, placed");
    assert_ne!(popup, main);
    assert_eq!(popups(cx), [popup]);
    let at = cx.update(|window, _| crate::popup::to_screen(window, point(px(300.), px(200.))));
    assert_eq!(
        (placed.left, placed.top),
        at,
        "top-left corner at the pointer"
    );
    // The panel is drawn in the popup, not in the main window.
    let panel = view.update(cx, |v, _| {
        v.context_menu.as_ref().unwrap().panel_for_test(0).0
    });
    let drawn = panel.expect("level drawn");
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let viewport = popup_cx.update(|window, _| window.viewport_size());
    assert!(drawn.right() <= viewport.width && drawn.bottom() <= viewport.height);
    // The window is fitted to the panel (the test monitor is at 100%).
    assert_eq!(
        (placed.width(), placed.height()),
        (
            f32::from(drawn.size.width).ceil() as i32,
            f32::from(drawn.size.height).ceil() as i32
        ),
        "{placed:?} {drawn:?}"
    );

    // Keys go to the main window, which keeps the menu's focus: Esc closes it.
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(popups(cx).is_empty(), "its window closed with it");

    // A press anywhere in the main window closes it too.
    open_menu_at(&view, cx, point(px(300.), px(200.)));
    assert_eq!(popups(cx).len(), 1);
    press(cx, point(px(20.), px(20.)));
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(popups(cx).is_empty());
}

/// Clicking a row in the menu's window runs it in the main window.
#[gpui::test]
fn a_row_clicked_in_the_menu_window_runs_in_the_main_window(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    right_click(&view, cx, Some(0));
    let (popup, _) = level_popup(&view, cx, 0).unwrap();
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let rename = popup_cx
        .debug_bounds("menu-action-Rename")
        .expect("Rename drawn in the menu window");
    press(&mut popup_cx, rename.center());
    assert!(
        view.update(cx, |v, _| v.context_menu.is_none()),
        "ran and closed"
    );
    // The rename box is in the main window, and has the keyboard there.
    assert!(view.update(cx, |v, _| v.renaming.is_some()));
    cx.simulate_input("x");
    let input = view.update(cx, |v, _| v.renaming.as_ref().unwrap().input.clone());
    assert_eq!(
        input.read_with(cx, |input, _| input.text().to_owned()),
        "x.txt"
    );
}

/// The menu goes when the window stops being the active one (a click in another program, on the
/// taskbar, Alt+Tab) and when the window moves.
#[gpui::test]
fn the_menu_closes_when_the_window_goes_to_the_background_or_moves(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();

    open_menu_at(&view, cx, point(px(300.), px(200.)));
    assert_eq!(popups(cx).len(), 1);
    cx.deactivate_window();
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(popups(cx).is_empty());

    open_menu_at(&view, cx, point(px(300.), px(200.)));
    assert_eq!(popups(cx).len(), 1);
    cx.simulate_resize(size(px(900.), px(600.)));
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(popups(cx).is_empty());
}

/// Near the bottom right corner of a small window the menu isn't squeezed into it: it opens at the
/// pointer and reaches past the window, kept inside the monitor's work area.
#[gpui::test]
fn a_menu_near_the_window_corner_reaches_past_it(cx: &mut TestAppContext) {
    crate::popup::set_test_monitors(one_monitor(ScreenRect::new(0, 0, 4000, 3000)));
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_resize(size(px(500.), px(400.)));
    cx.run_until_parked();
    let window = window_on_screen(cx);

    open_menu_at(&view, cx, point(px(480.), px(380.)));
    let (_, placed) = level_popup(&view, cx, 0).unwrap();
    let at = cx.update(|window, _| crate::popup::to_screen(window, point(px(480.), px(380.))));
    assert_eq!(
        (placed.left, placed.top),
        at,
        "opens at the pointer: {placed:?}"
    );
    assert!(
        placed.right > window.right && placed.bottom > window.bottom,
        "{placed:?} {window:?}"
    );

    // At the monitor's corner it flips up and left instead, staying on the screen.
    crate::popup::set_test_monitors(one_monitor(ScreenRect::new(0, 0, at.0 + 10, at.1 + 10)));
    cx.simulate_keystrokes("escape");
    open_menu_at(&view, cx, point(px(480.), px(380.)));
    let (_, placed) = level_popup(&view, cx, 0).unwrap();
    assert_eq!((placed.right, placed.bottom), at, "flipped: {placed:?}");
}

/// The levels "Show more options" and "Send to", as placed after opening them from the empty
/// space menu at `at`.
fn open_send_to(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    at: gpui::Point<gpui::Pixels>,
) -> Vec<ScreenRect> {
    open_menu_at(view, cx, at);
    wait_until(cx, view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    let more = format!("{} ›", t!("menu.more_options"));
    let slot = menu_labels(view, cx)
        .unwrap()
        .iter()
        .position(|l| *l == more)
        .unwrap();
    view.update(cx, |v, cx| {
        v.context_menu.as_mut().unwrap().highlight_for_test(slot);
        cx.notify();
    });
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    // "Open", then "Compress with 7-Zip", then "Send to".
    cx.simulate_keystrokes("down down right");
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| v.context_menu.as_ref().unwrap().depth()),
        2
    );
    (0..3)
        .map(|level| level_popup(view, cx, level).expect("level placed").1)
        .collect()
}

/// A submenu that doesn't fit on the right opens left of its own level, covering the level
/// before it like Windows, instead of skipping past every open level; the next one keeps going
/// left. Its row's chevron then points left.
#[gpui::test]
fn a_deep_submenu_near_the_screen_edge_opens_left_of_its_own_level(cx: &mut TestAppContext) {
    crate::popup::set_test_monitors(one_monitor(ScreenRect::new(0, 0, 6000, 3000)));
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    let at = point(px(100.), px(100.));
    // With room everywhere, each level opens right of the one before.
    let wide = open_send_to(&view, cx, at);
    assert!(
        wide[1].left > wide[0].left && wide[2].left > wide[1].left,
        "{wide:?}"
    );
    cx.simulate_keystrokes("escape escape escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // The screen ends just past "Show more options": "Send to" has no room on the right.
    let edge = wide[1].right + 20;
    crate::popup::set_test_monitors(one_monitor(ScreenRect::new(0, 0, edge, 3000)));
    let tight = open_send_to(&view, cx, at);
    assert_eq!(&tight[..2], &wide[..2], "the outer levels stay");
    let overlap = 2;
    assert_eq!(
        tight[2].right,
        tight[1].left + overlap,
        "left of its own level: {tight:?}"
    );
    assert!(tight[2].left < tight[0].right, "covers the level before it");
    assert!(view.update(cx, |v, _| {
        v.context_menu.as_ref().unwrap().opens_left_for_test(2)
    }));
}

/// The tab, sidebar and history menus open in a window of their own too.
#[gpui::test]
fn the_tab_menu_opens_in_a_window_of_its_own(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| ws.open_tab_menu_for_test(0, cx));
    cx.run_until_parked();
    let popup = ws
        .update(cx, |ws, _| ws.sidebar_menu_window_for_test())
        .expect("the menu has a window");
    assert_eq!(popups(cx), [popup]);
    press(cx, point(px(600.), px(400.)));
    assert!(popups(cx).is_empty(), "a press outside closes it");
}
