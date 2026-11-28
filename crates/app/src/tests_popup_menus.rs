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

/// A settings dropdown's list closes on a press beside it, even when nothing else made the
/// window draw after the list opened (it stayed open on screen).
#[gpui::test]
fn a_settings_dropdown_closes_on_a_press_beside_it(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.simulate_resize(size(px(1400.), px(3000.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.about"), cx);
    });
    cx.run_until_parked();
    for id in ["update", "update-channel"] {
        let at = cx.debug_bounds(id).expect(id).center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
    }
    assert_eq!(popups(cx).len(), 1, "the list is open in its own window");
    press(cx, point(px(40.), px(2900.)));
    assert!(
        popups(cx).is_empty(),
        "a press beside it closes the list's window"
    );
}

/// Opens Settings › Appearance in a short window, scrolls it to the bottom (where Language is) and
/// opens the Language dropdown. Returns the button and the list as placed, on the screen.
fn open_language_dropdown_scrolled(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (ScreenRect, ScreenRect) {
    // A monitor around the window, at its scale, so the list has room under the button.
    let scale = cx.update(|window, _| window.scale_factor());
    crate::popup::set_test_monitors(vec![Monitor {
        bounds: ScreenRect::new(0, 0, 8000, 6000),
        work: ScreenRect::new(0, 0, 8000, 6000),
        scale,
    }]);
    cx.simulate_resize(size(px(1200.), px(640.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    cx.simulate_event(gpui::ScrollEvent {
        position: point(px(700.), px(400.)),
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-5000.))),
        ..gpui::ScrollEvent::default()
    });
    cx.run_until_parked();
    let scrolled = ws
        .update(cx, |ws, _| ws.settings_scroll_for_test())
        .unwrap()
        .0;
    assert!(scrolled < px(-100.), "the page scrolled: {scrolled:?}");
    let button = cx.debug_bounds("language").expect("Language drawn");
    cx.simulate_click(button.center(), Modifiers::none());
    cx.run_until_parked();
    let control = cx.update(|window, _| crate::popup::rect_to_screen(window, button));
    let placed = ws
        .update(cx, |ws, _| ws.settings_dropdown_placed_for_test())
        .expect("the list is placed");
    (control, placed)
}

/// A settings dropdown on a scrolled page opens right under its button, its right edge lined up
/// with the button's, not where the button would be without the scroll.
#[gpui::test]
fn a_settings_dropdown_on_a_scrolled_page_opens_under_its_button(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let (control, placed) = open_language_dropdown_scrolled(&ws, cx);
    let gap = cx.update(|window, _| (4. * window.scale_factor()).round() as i32);
    assert_eq!(placed.top, control.bottom + gap, "{control:?} {placed:?}");
    assert_eq!(placed.right, control.right, "{control:?} {placed:?}");
    assert!(placed.width() >= control.width(), "{control:?} {placed:?}");
}

/// A dropdown list that slides in comes to rest where it was placed: its window is fitted to the
/// panel's resting place, not to the first frame of the slide (which left it a few pixels low).
#[gpui::test]
fn a_dropdown_that_slides_in_rests_where_it_was_placed(cx: &mut TestAppContext) {
    crate::motion::force_for_test(true);
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let (control, placed) = open_language_dropdown_scrolled(&ws, cx);
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let popup = popups(cx)[0];
    // Draw the popup again at the end of its slide.
    cx.update(|_, cx| {
        let _ = popup.update(cx, |_, window, _| window.refresh());
    });
    cx.run_until_parked();
    let drawn = ws
        .update(cx, |ws, _| ws.settings_dropdown_drawn_for_test())
        .expect("the list is drawn");
    crate::motion::force_for_test(false);
    assert_eq!(drawn, placed, "{control:?}");
}

/// Opened with the mouse, a settings dropdown lights no row but the hovered one (its own hover):
/// the keyboard cursor shows once an arrow key moves it, from the current choice.
#[gpui::test]
fn a_settings_dropdown_opened_with_the_mouse_shows_no_keyboard_cursor(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let _ = open_language_dropdown_scrolled(&ws, cx);
    let cursor = |cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.settings_dropdown_cursor_for_test())
            .expect("open")
    };
    // The current choice is "Follow system", the first row.
    assert_eq!(cursor(cx), (0, false), "no keyboard cursor shown");
    let popup = popups(cx)[0];
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let row = popup_cx
        .debug_bounds("language-2")
        .expect("third row drawn in the list's window");
    popup_cx.simulate_mouse_move(row.center(), None, Modifiers::none());
    popup_cx.run_until_parked();
    assert_eq!(
        cursor(cx),
        (0, false),
        "hovering lights only the hovered row"
    );
    cx.simulate_keystrokes("down");
    assert_eq!(
        cursor(cx),
        (1, true),
        "arrow keys start from the current choice"
    );
}

/// The › menu in the path bar opens under the bar at the › pressed, and above the bar when there
/// is no room below, not over it. Opened with the mouse, it lights no row (the current folder is
/// in bold); an arrow key lights the next one.
#[gpui::test]
fn the_path_bar_folder_menu_opens_under_the_bar_or_above_it(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![dir("b"), dir("c"), dir("d")]);
    fs.insert_dir(r"C:\a\b", vec![file("x.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\a\b");
    wait_until(cx, &view, "load", loaded(r"C:\a\b"));
    let scale = cx.update(|window, _| window.scale_factor());
    let gap = (4. * scale).round() as i32;
    let bar = view.update(cx, |v, _| v.path_bar_bounds_for_test());
    let bar = cx.update(|window, _| crate::popup::rect_to_screen(window, bar));
    let open = |cx: &mut VisualTestContext| {
        view.update_in(cx, |v, window, cx| {
            v.show_folders(
                std::path::PathBuf::from(r"C:\a"),
                Some(std::path::PathBuf::from(r"C:\a\b")),
                px(200.),
                false,
                window,
                cx,
            );
        });
        wait_until(cx, &view, "listed", |v| {
            v.path_popup_for_test()
                .is_some_and(|(_, at, _)| at.is_some())
        });
        cx.run_until_parked();
        view.update(cx, |v, _| v.path_popup_for_test()).unwrap()
    };
    // A monitor ending at `bottom`, with plenty of room above the bar.
    let monitor = |bottom: i32| {
        vec![Monitor {
            bounds: ScreenRect::new(0, -4000, 8000, bottom + 4000),
            work: ScreenRect::new(0, -4000, 8000, bottom + 4000),
            scale,
        }]
    };

    crate::popup::set_test_monitors(monitor(6000));
    let (placed, at, lit) = open(cx);
    let placed = placed.expect("placed");
    assert_eq!(
        placed.top,
        bar.bottom + gap,
        "under the bar: {bar:?} {placed:?}"
    );
    assert_eq!(at, Some(0), "the keyboard row starts on the current folder");
    assert!(!lit, "but is not lit when opened with the mouse");
    cx.simulate_keystrokes("down");
    let (_, at, lit) = view.update(cx, |v, _| v.path_popup_for_test()).unwrap();
    assert_eq!(
        (at, lit),
        (Some(1), true),
        "an arrow key lights the next row"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(popups(cx).is_empty());

    // The screen ends just under the bar: above it instead.
    crate::popup::set_test_monitors(monitor(bar.bottom + 40));
    let (placed, ..) = open(cx);
    let placed = placed.expect("placed");
    assert_eq!(
        placed.bottom,
        bar.top - gap,
        "above the bar: {bar:?} {placed:?}"
    );
}

/// A filter's menu opened with the mouse lights no row until an arrow key is used; opened with
/// the keys, its keyboard row shows at once.
#[gpui::test]
fn a_filter_menu_opened_with_the_mouse_lights_no_row(cx: &mut TestAppContext) {
    use arke_core::index_query::Facet;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\k", vec![file("a.jpg", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\k");
    wait_until(cx, &view, "load", loaded(r"C:\k"));
    cx.dispatch_action(crate::explorer::FocusFilter);
    cx.run_until_parked();
    let (_, button) = view.update(cx, |v, _| v.panel_geometry_for_test(Facet::Size));
    cx.simulate_click(button.expect("drawn").center(), Modifiers::none());
    cx.run_until_parked();
    let state = |cx: &mut VisualTestContext| {
        let menu = cx
            .update(|window, cx| view.update(cx, |v, _| v.panel_state_for_test(window).2))
            .expect("menu open");
        (
            menu.1,
            view.update(cx, |v, _| v.filter_menu_keys_for_test()),
        )
    };
    assert_eq!(state(cx), (0, false), "on the current choice, not lit");
    cx.simulate_keystrokes("down");
    assert_eq!(state(cx), (1, true), "an arrow key lights the next row");
    // Right opens the next filter's menu from the keys: lit at once.
    cx.simulate_keystrokes("right");
    assert!(state(cx).1);
}

/// A dropdown under a control across two monitors goes on the monitor of the edge it lines up
/// with (a settings list's right edge), not the one its left edge is on.
#[gpui::test]
fn a_dropdown_across_two_monitors_takes_the_monitor_of_its_aligned_edge(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let (control, _) = open_language_dropdown_scrolled(&ws, cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(popups(cx).is_empty());
    let scale = cx.update(|window, _| window.scale_factor());
    // The seam runs through the button, near its left edge.
    let seam = control.left + 10;
    crate::popup::set_test_monitors(vec![
        Monitor {
            bounds: ScreenRect::new(0, 0, seam, 6000),
            work: ScreenRect::new(0, 0, seam, 6000),
            scale,
        },
        Monitor {
            bounds: ScreenRect::new(seam, 0, 8000, 6000),
            work: ScreenRect::new(seam, 0, 8000, 6000),
            scale,
        },
    ]);
    let button = cx.debug_bounds("language").expect("Language drawn");
    cx.simulate_click(button.center(), Modifiers::none());
    cx.run_until_parked();
    let placed = ws
        .update(cx, |ws, _| ws.settings_dropdown_placed_for_test())
        .expect("the list is placed");
    assert!(
        placed.left >= seam,
        "on the right-hand monitor: {control:?} {placed:?}"
    );
}

/// Pressing the › that opened the path bar's folder menu again closes it, as a menu's button does,
/// rather than closing it on the press and opening it again on the release.
#[gpui::test]
fn the_path_bar_folder_menu_closes_when_its_chevron_is_pressed_again(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![dir("b"), dir("c")]);
    fs.insert_dir(r"C:\a\b", vec![file("x.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\a\b");
    wait_until(cx, &view, "load", loaded(r"C:\a\b"));
    // "C:", "a", "b": the › before "b" starts its crumb.
    let crumb = cx.debug_bounds("crumb-2").expect("crumb drawn");
    let chevron = gpui::point(crumb.left() + px(10.), crumb.center().y);
    let open =
        |cx: &mut VisualTestContext| view.update(cx, |v, _| v.path_popup_for_test().is_some());
    cx.simulate_click(chevron, Modifiers::none());
    cx.run_until_parked();
    assert!(open(cx), "opens");
    cx.simulate_click(chevron, Modifiers::none());
    cx.run_until_parked();
    assert!(!open(cx), "the same › closes it");
    cx.simulate_click(chevron, Modifiers::none());
    cx.run_until_parked();
    assert!(open(cx), "and opens it again after that");
}

/// The › before the drive (#354) lists Home, Documents, every drive with its free space (as the
/// sidebar has it, nothing is read) and the network shares opened before. The drive the path is
/// on is in bold and the keys start there; Enter opens the row, Home opens the Home page.
#[gpui::test]
fn the_path_bar_lists_the_drives_before_the_first_segment(cx: &mut TestAppContext) {
    use arke_core::format::format_size;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![file("x.txt", 1)]);
    fs.insert_dir(r"D:\e", vec![file("y.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().extend([
        Place {
            label: "Documents".into(),
            path: std::path::PathBuf::from(r"C:\home\Documents"),
            kind: PlaceKind::Folder(KnownFolder::Documents),
        },
        Place {
            label: "Local Disk (C:)".into(),
            path: std::path::PathBuf::from(r"C:\"),
            kind: PlaceKind::Drive {
                remote: false,
                space: Some((10 << 30, 100 << 30)),
            },
        },
        Place {
            label: "Data (D:)".into(),
            path: std::path::PathBuf::from(r"D:\"),
            kind: PlaceKind::Drive {
                remote: false,
                space: None,
            },
        },
    ]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\a");
    cx.simulate_resize(size(px(1200.), px(800.)));
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\a")(v) && v.places.len() > 3
    });
    let open = |cx: &mut VisualTestContext| {
        let lead = cx
            .debug_bounds("crumb-roots")
            .expect("the › before the drive is drawn");
        cx.simulate_click(lead.center(), Modifiers::none());
        cx.run_until_parked();
    };

    open(cx);
    let space = t!(
        "home.space",
        free = format_size(10 << 30),
        total = format_size(100 << 30)
    )
    .into_owned();
    assert_eq!(
        view.update(cx, |v, _| v.path_popup_rows_for_test()),
        Some(vec![
            (t!("home.title").into_owned(), None, false),
            ("Documents".to_owned(), None, false),
            ("Local Disk (C:)".to_owned(), Some(space), true),
            ("Data (D:)".to_owned(), None, false),
        ])
    );
    let (_, at, lit) = view.update(cx, |v, _| v.path_popup_for_test()).unwrap();
    assert_eq!((at, lit), (Some(2), false), "on the drive, not lit");

    cx.simulate_keystrokes("down enter");
    wait_until(cx, &view, "the other drive", loaded(r"D:\"));
    assert!(view.update(cx, |v, _| v.path_popup_for_test().is_none()));

    // From D:, three rows up is Home.
    open(cx);
    cx.simulate_keystrokes("up up up enter");
    cx.run_until_parked();
    assert!(
        ws.update(cx, |ws, cx| ws.home_favorites_for_test(cx).is_some()),
        "Home opens"
    );
}

/// Whether the menu's access keys are underlined, and how many submenus are open.
fn cues_and_depth(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Option<(bool, usize)> {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .map(|m| (m.cues_shown_for_test(), m.depth()))
    })
}

/// A menu opened with the mouse has no underlines until a key is pressed in it. A letter nothing
/// has does nothing else; the letter of a submenu opens it (#422).
#[gpui::test]
fn a_menu_opened_with_the_mouse_shows_its_access_keys_once_a_key_is_pressed(
    cx: &mut TestAppContext,
) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    open_menu_at(&view, cx, point(px(300.), px(200.)));
    assert_eq!(cues_and_depth(&view, cx), Some((false, 0)));
    cx.simulate_keystrokes("z");
    assert_eq!(cues_and_depth(&view, cx), Some((true, 0)));
    // "New" is the one with W.
    cx.simulate_keystrokes("w");
    assert_eq!(cues_and_depth(&view, cx), Some((true, 1)));
}

/// A menu opened from the keyboard (Shift+F10) has them from the start.
#[gpui::test]
fn a_menu_opened_from_the_keyboard_shows_its_access_keys(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_context_menu_by_keyboard_for_test(window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(cues_and_depth(&view, cx), Some((true, 0)));
}

/// Open in Terminal is picked with T, as in File Explorer (#422): the menu runs it and closes.
#[gpui::test]
fn open_in_terminal_is_picked_with_t(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    open_menu_at(&view, cx, point(px(300.), px(200.)));
    assert!(cues_and_depth(&view, cx).is_some());
    cx.simulate_keystrokes("t");
    assert_eq!(cues_and_depth(&view, cx), None);
}

/// Shift with Delete's key in a file's menu is Shift+Delete: it asks before deleting for good and
/// leaves the Recycle Bin alone. A fake shell stands in for it, so nothing real is touched.
#[gpui::test]
fn shift_with_deletes_key_asks_to_delete_permanently(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_context_menu_by_keyboard_for_test(window, cx);
        });
    });
    cx.run_until_parked();
    assert!(cues_and_depth(&view, cx).is_some());
    cx.simulate_keystrokes("shift-d");
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("recycle.confirm_purge_title"))
        );
    });
    assert!(shell.ops().is_empty(), "nothing is deleted before the yes");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(shell.ops().is_empty());
}

/// Clicks the menu's Delete button, with `modifiers` held, in a menu opened on `a.txt`.
fn click_menu_delete(view: &Entity<Explorer>, cx: &mut VisualTestContext, modifiers: Modifiers) {
    right_click(view, cx, Some(0));
    let (popup, _) = level_popup(view, cx, 0).unwrap();
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let at = popup_cx
        .debug_bounds("menu-action-Delete")
        .expect("Delete drawn in the menu window")
        .center();
    popup_cx.simulate_mouse_down(at, MouseButton::Left, modifiers);
    popup_cx.simulate_mouse_up(at, MouseButton::Left, modifiers);
    popup_cx.run_until_parked();
    cx.run_until_parked();
}

/// Shift+click on the menu's Delete button deletes permanently after asking, as in File Explorer:
/// nothing is recycled. Space then confirms and the file is purged.
#[gpui::test]
fn shift_click_on_the_menus_delete_button_asks_to_delete_permanently(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    click_menu_delete(&view, cx, Modifiers::shift());
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("recycle.confirm_purge_title"))
        );
    });
    assert!(shell.ops().is_empty(), "nothing is deleted before the yes");

    // Escape still cancels.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    view.update(cx, |v, _| assert!(v.confirm_title().is_none()));
    assert!(shell.ops().is_empty());

    // Space confirms.
    click_menu_delete(&view, cx, Modifiers::shift());
    cx.simulate_keystrokes("space");
    wait_until(cx, &view, "purged", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Delete {
            paths: vec![PathBuf::from(r"C:\m\a.txt")],
            recycle: false,
        }]
    );
}

/// A plain click on the menu's Delete button still recycles, without asking.
#[gpui::test]
fn a_plain_click_on_the_menus_delete_button_recycles(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    click_menu_delete(&view, cx, Modifiers::none());
    wait_until(cx, &view, "recycled", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Delete {
            paths: vec![PathBuf::from(r"C:\m\a.txt")],
            recycle: true,
        }]
    );
}

/// Shift+click on the selection bar's Delete button asks to delete permanently too.
#[gpui::test]
fn shift_click_on_the_selection_bars_delete_button_asks_to_delete_permanently(
    cx: &mut TestAppContext,
) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    let at = cx
        .debug_bounds("sel-delete")
        .expect("Delete drawn in the selection bar")
        .center();
    cx.simulate_click(at, Modifiers::shift());
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("recycle.confirm_purge_title"))
        );
    });
    assert!(shell.ops().is_empty());
}

/// In a folder's menu, Cut's button takes no letter, so T is Open in Terminal's alone: it runs
/// at once, and nothing is cut (#422).
#[gpui::test]
fn in_a_folders_menu_t_opens_the_terminal(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("sub")]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_context_menu_by_keyboard_for_test(window, cx);
        });
    });
    cx.run_until_parked();
    assert!(cues_and_depth(&view, cx).is_some());
    cx.simulate_keystrokes("t");
    cx.run_until_parked();
    assert_eq!(cues_and_depth(&view, cx), None);
    assert!(shell.clipboard().is_none(), "T did not cut");
}

/// The icon buttons of the menu, each with the text it draws and the key beside its icon.
fn icon_row(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<(String, Option<char>)> {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .expect("the menu is open")
            .icon_row_for_test()
    })
}

/// Where the menu's window last drew `selector`, drawn afresh.
fn drawn_in(
    popup_cx: &mut VisualTestContext,
    selector: &'static str,
) -> Option<Bounds<gpui::Pixels>> {
    popup_cx.update(|window, _| window.refresh());
    popup_cx.run_until_parked();
    popup_cx.debug_bounds(selector)
}

/// In Chinese the icon buttons' text is the label alone, never "剪下(T)", which crowds them. The
/// key shows beside the top of the icon once the underlines do, with a gap, without moving the
/// button, and works whether it is shown or not (#422).
#[gpui::test]
fn icon_buttons_show_their_key_beside_the_icon_only_with_the_underlines(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    rust_i18n::set_locale("zh-TW");
    let labels: Vec<String> = ["menu.cut", "menu.copy", "menu.rename", "menu.delete"]
        .iter()
        .map(|key| t!(*key).to_string())
        .collect();
    let with = |keys: [Option<char>; 4]| -> Vec<(String, Option<char>)> {
        labels.iter().cloned().zip(keys).collect()
    };

    // Opened with the mouse: the labels alone, and no key drawn.
    right_click(&view, cx, Some(0));
    assert_eq!(icon_row(&view, cx), with([None; 4]));
    assert!(
        icon_row(&view, cx)
            .iter()
            .all(|(text, _)| !text.contains('('))
    );
    let (popup, _) = level_popup(&view, cx, 0).expect("the menu has a window");
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let cut = drawn_in(&mut popup_cx, "menu-action-Cut").expect("Cut drawn");
    assert!(drawn_in(&mut popup_cx, "menu-action-key-Cut").is_none());

    // A key pressed shows the underlines: the keys come beside the icons, the text stays the label
    // and the button doesn't move or change size.
    cx.simulate_keystrokes("z");
    assert_eq!(
        icon_row(&view, cx),
        with([Some('T'), Some('C'), Some('M'), Some('D')])
    );
    let key = drawn_in(&mut popup_cx, "menu-action-key-Cut").expect("Cut's key drawn");
    assert_eq!(drawn_in(&mut popup_cx, "menu-action-Cut"), Some(cut));
    assert!(
        key.is_contained_within(&cut),
        "the key is in the button: {key:?} {cut:?}"
    );
    // The icon is centered at the button's top: the key starts past its right edge, with a gap,
    // and sits by its upper half.
    let icon = crate::theme::icon();
    let icon_top = cut.top() + crate::theme::s_6();
    assert!(
        key.left() >= cut.center().x + icon / 2. + crate::theme::s_2(),
        "the key is right of the icon: {key:?} {cut:?}"
    );
    assert!(
        key.top() < icon_top + icon / 2.,
        "the key is by the icon's top: {key:?} {cut:?}"
    );
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // The key works while it isn't shown: C copies at once.
    right_click(&view, cx, Some(0));
    assert_eq!(icon_row(&view, cx), with([None; 4]));
    cx.simulate_keystrokes("c");
    wait_shell_in(cx, "copied", || shell.clipboard().is_some_and(|c| !c.cut));
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // And while it is: opened from the keyboard, T cuts.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_context_menu_by_keyboard_for_test(window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(
        icon_row(&view, cx),
        with([Some('T'), Some('C'), Some('M'), Some('D')])
    );
    cx.simulate_keystrokes("t");
    wait_shell_in(cx, "cut", || shell.clipboard().is_some_and(|c| c.cut));
    rust_i18n::set_locale("en");
}

/// How far the rows of menu level `level` are scrolled, how far they can be, and the box they
/// scroll in (in that level's window).
fn rows_scroll(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    level: usize,
) -> (gpui::Pixels, gpui::Pixels, Bounds<gpui::Pixels>) {
    view.update(cx, |v, _| {
        v.context_menu.as_ref().unwrap().rows_scroll_for_test(level)
    })
}

/// Where row `ix` of level `level` is drawn now, as scrolled.
fn row_drawn(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    level: usize,
    ix: usize,
) -> Bounds<gpui::Pixels> {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .unwrap()
            .row_drawn_for_test(level, ix)
    })
    .expect("row drawn")
}

/// The highlighted row of the innermost menu level.
fn highlighted(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Option<usize> {
    view.update(cx, |v, _| {
        v.context_menu.as_ref().unwrap().highlighted_for_test()
    })
}

fn assert_row_in_view(rows: Bounds<gpui::Pixels>, row: Bounds<gpui::Pixels>, what: &str) {
    assert!(
        row.top() >= rows.top() - px(0.5) && row.bottom() <= rows.bottom() + px(0.5),
        "{what}: {row:?} in {rows:?}"
    );
}

/// One notch of the mouse wheel down (Windows scrolls three lines a notch) over `at`.
fn wheel_down(popup_cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    popup_cx.simulate_event(gpui::ScrollEvent {
        position: at,
        delta: gpui::ScrollDelta::Lines(point(0., -3.)),
        ..gpui::ScrollEvent::default()
    });
    popup_cx.run_until_parked();
}

/// A menu taller than the screen (here a short one, under a short window) is capped to it and
/// kept on it; its rows scroll by the wheel a notch at a time and by the keys, every row can be
/// reached, and the buttons at its bottom stay in view below them.
#[gpui::test]
#[allow(
    clippy::too_many_lines,
    reason = "one menu, reached by the wheel and by each key in turn"
)]
fn a_menu_taller_than_the_screen_fits_on_it_and_every_row_can_be_reached(cx: &mut TestAppContext) {
    let work = ScreenRect::new(0, 0, 1600, 300);
    crate::popup::set_test_monitors(one_monitor(work));
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_resize(size(px(700.), px(280.)));
    cx.run_until_parked();
    right_click(&view, cx, Some(0));
    let (popup, placed) = level_popup(&view, cx, 0).expect("placed");
    assert!(
        placed.top >= work.top && placed.bottom <= work.bottom,
        "on the screen: {placed:?}"
    );
    let (offset, max, rows) = rows_scroll(&view, cx, 0);
    assert_eq!(offset, px(0.));
    assert!(max > px(0.), "taller than the screen: its rows scroll");
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let viewport = popup_cx.update(|window, _| window.viewport_size());
    let panel = view
        .update(cx, |v, _| {
            v.context_menu.as_ref().unwrap().panel_for_test(0).0
        })
        .expect("drawn");
    assert!(panel.bottom() <= viewport.height, "{panel:?} {viewport:?}");
    let buttons = |popup_cx: &mut VisualTestContext| {
        popup_cx.debug_bounds("menu-action-Cut").expect("Cut drawn")
    };
    let cut = buttons(&mut popup_cx);
    assert!(
        cut.top() >= rows.bottom() && cut.bottom() <= panel.bottom(),
        "the buttons are below the rows, in the panel: {cut:?} {rows:?} {panel:?}"
    );

    // The wheel: a notch moves by more than a few pixels, and notches reach the end.
    wheel_down(&mut popup_cx, rows.center());
    let (one, max, _) = rows_scroll(&view, cx, 0);
    assert!(
        one <= px(-20.) || one == -max,
        "a notch scrolls a normal amount: {one:?}"
    );
    for _ in 0..40 {
        wheel_down(&mut popup_cx, rows.center());
    }
    let (end, max, rows) = rows_scroll(&view, cx, 0);
    assert_eq!(end, -max, "scrolled to the end");
    let (selectable, footer) =
        view.update(cx, |v, _| v.context_menu.as_ref().unwrap().rows_for_test(0));
    assert_row_in_view(rows, row_drawn(&view, cx, 0, footer - 1), "the last row");
    assert_eq!(buttons(&mut popup_cx), cut, "the buttons didn't move");

    // Home goes back to the first row, at the top.
    let first = *selectable.first().unwrap();
    let last = *selectable.last().unwrap();
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    assert_eq!(highlighted(&view, cx), Some(first));
    let (_, _, rows) = rows_scroll(&view, cx, 0);
    assert_row_in_view(
        rows,
        row_drawn(&view, cx, 0, first),
        "the first row, by Home",
    );
    // Down, row by row, reaches the last one, which is in view when highlighted.
    for _ in 1..selectable.len() {
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
    }
    assert_eq!(highlighted(&view, cx), Some(last));
    let (offset, _, rows) = rows_scroll(&view, cx, 0);
    assert!(offset < px(0.), "scrolled along: {offset:?}");
    assert_row_in_view(rows, row_drawn(&view, cx, 0, last), "the last row, by Down");
    // Back with Home, then End: the last row again.
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    assert_eq!(highlighted(&view, cx), Some(last));
    let (_, _, rows) = rows_scroll(&view, cx, 0);
    assert_row_in_view(rows, row_drawn(&view, cx, 0, last), "the last row, by End");
    // Home, then Up wraps around to it.
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    assert_eq!(highlighted(&view, cx), Some(last));
    let (_, _, rows) = rows_scroll(&view, cx, 0);
    assert_row_in_view(rows, row_drawn(&view, cx, 0, last), "the last row, by Up");

    // Opened from the keyboard (Shift+F10) too: on the screen, the buttons whole in the panel.
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_context_menu_by_keyboard_for_test(window, cx);
        });
    });
    cx.run_until_parked();
    let (popup, placed) = level_popup(&view, cx, 0).expect("placed");
    assert!(
        placed.top >= work.top && placed.bottom <= work.bottom,
        "on the screen: {placed:?}"
    );
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let panel = view
        .update(cx, |v, _| {
            v.context_menu.as_ref().unwrap().panel_for_test(0).0
        })
        .expect("drawn");
    let cut = buttons(&mut popup_cx);
    assert!(cut.bottom() <= panel.bottom(), "{cut:?} {panel:?}");
}

/// Right-clicks the first file and opens "Show more options", once it has loaded: level 1.
fn open_more_options(view: &Entity<Explorer>, cx: &mut VisualTestContext) {
    right_click(view, cx, Some(0));
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
    assert_eq!(
        view.update(cx, |v, _| v.context_menu.as_ref().unwrap().depth()),
        1
    );
}

/// A shell whose "Show more options" has `extra` commands of its own after the usual ones.
fn shell_with_extra_commands(extra: u32) -> Arc<FakeShell> {
    let shell = Arc::new(FakeShell::default());
    *shell.extra_menu.lock() = (0..extra)
        .map(|n| crate::shell::ShellMenuEntry::Item {
            id: 100 + n,
            verb: None,
            label: format!("Command {n}"),
            access: None,
            enabled: true,
            checked: false,
            default: false,
            icon: None,
        })
        .collect();
    shell
}

/// "Show more options" with more commands than the screen has room for: kept on the screen,
/// capped to it, and End reaches its last command, in view.
#[gpui::test]
fn a_submenu_taller_than_the_screen_is_capped_and_scrolls(cx: &mut TestAppContext) {
    let work = ScreenRect::new(0, 0, 1600, 500);
    crate::popup::set_test_monitors(one_monitor(work));
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, shell_with_extra_commands(40), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_resize(size(px(800.), px(450.)));
    cx.run_until_parked();
    open_more_options(&view, cx);
    let (popup, placed) = level_popup(&view, cx, 1).expect("level 1 placed");
    assert!(
        placed.top >= work.top && placed.bottom <= work.bottom,
        "on the screen: {placed:?}"
    );
    let (offset, max, _) = rows_scroll(&view, cx, 1);
    assert_eq!(offset, px(0.));
    assert!(max > px(0.), "capped: its rows scroll");
    let mut popup_cx = VisualTestContext::from_window(popup, &cx.cx);
    let viewport = popup_cx.update(|window, _| window.viewport_size());
    let panel = view
        .update(cx, |v, _| {
            v.context_menu.as_ref().unwrap().panel_for_test(1).0
        })
        .expect("drawn");
    assert!(panel.bottom() <= viewport.height, "{panel:?} {viewport:?}");

    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    let (selectable, _) = view.update(cx, |v, _| v.context_menu.as_ref().unwrap().rows_for_test(1));
    let last = *selectable.last().unwrap();
    assert_eq!(highlighted(&view, cx), Some(last));
    let (offset, _, rows) = rows_scroll(&view, cx, 1);
    assert!(offset < px(0.), "scrolled: {offset:?}");
    assert_row_in_view(rows, row_drawn(&view, cx, 1, last), "the last command");
}

/// "Show more options" that fits the screen but would run past its bottom where its row is: it
/// moves up whole, as a Windows submenu does, not squeezed and not scrolling.
#[gpui::test]
fn a_submenu_that_would_run_past_the_bottom_moves_up(cx: &mut TestAppContext) {
    crate::popup::set_test_monitors(one_monitor(ScreenRect::new(0, 0, 1600, 3000)));
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, shell_with_extra_commands(4), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    open_more_options(&view, cx);
    let (_, roomy) = level_popup(&view, cx, 1).expect("level 1 placed");
    let (_, roomy_parent) = level_popup(&view, cx, 0).expect("level 0 placed");
    cx.simulate_keystrokes("escape escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // The screen ends halfway down the submenu, still below the top level.
    let bottom = (roomy.top + roomy.height() / 2).max(roomy_parent.bottom + 1);
    assert!(bottom < roomy.bottom, "the submenu runs past it");
    let work = ScreenRect::new(0, 0, 1600, bottom);
    crate::popup::set_test_monitors(one_monitor(work));
    open_more_options(&view, cx);
    let (_, placed) = level_popup(&view, cx, 1).expect("level 1 placed");
    assert!(
        placed.top >= work.top && placed.bottom <= work.bottom,
        "on the screen: {placed:?}"
    );
    assert!(placed.top < roomy.top, "moved up: {placed:?} {roomy:?}");
    assert_eq!(placed.height(), roomy.height(), "whole, not squeezed");
    assert_eq!(rows_scroll(&view, cx, 1).1, px(0.), "nothing to scroll");
}

/// A menu that fits is laid out as before: the same size on a screen just big enough for it as on
/// a large one, at the pointer, and nothing to scroll (not even by its padding).
#[gpui::test]
fn a_menu_that_fits_is_not_capped_and_does_not_scroll(cx: &mut TestAppContext) {
    crate::popup::set_test_monitors(one_monitor(ScreenRect::new(0, 0, 1600, 3000)));
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    right_click(&view, cx, Some(0));
    let (_, roomy) = level_popup(&view, cx, 0).expect("placed");
    let at = cx.update(|window, _| crate::popup::to_screen(window, point(px(300.), px(200.))));
    assert_eq!((roomy.left, roomy.top), at, "at the pointer");
    assert_eq!(rows_scroll(&view, cx, 0).1, px(0.), "nothing to scroll");
    cx.simulate_keystrokes("escape");

    // A screen with just room for it below the pointer, and its shadow.
    let work = ScreenRect::new(0, 0, 1600, roomy.bottom + 60);
    crate::popup::set_test_monitors(one_monitor(work));
    right_click(&view, cx, Some(0));
    let (_, placed) = level_popup(&view, cx, 0).expect("placed");
    assert_eq!(placed, roomy, "same place and size");
    let (offset, max, rows) = rows_scroll(&view, cx, 0);
    assert_eq!((offset, max), (px(0.), px(0.)), "nothing to scroll");
    let (_, footer) = view.update(cx, |v, _| v.context_menu.as_ref().unwrap().rows_for_test(0));
    assert_row_in_view(rows, row_drawn(&view, cx, 0, footer - 1), "the last row");
}
