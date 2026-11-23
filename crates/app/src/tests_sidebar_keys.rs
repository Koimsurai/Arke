//! GPUI tests for the keyboard in the sidebar and F6 between the parts of the window
//! (`workspace_sidebar_keys.rs`, #322): a screen reader user couldn't reach the sidebar at all,
//! not even to switch drives.

use std::sync::Arc;

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};

use super::*;
use crate::shell::{Place, PlaceKind};

/// `C:\` with `music` and `work`, and `D:\` with `games`.
fn fs() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("music"), dir("work")]);
    fs.insert_dir(r"C:\work", vec![dir("reports"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\work\reports", Vec::new());
    fs.insert_dir(r"C:\music", Vec::new());
    fs.insert_dir(r"D:\", vec![dir("games")]);
    fs.insert_dir(r"D:\games", Vec::new());
    fs
}

fn drive(letter: char) -> Place {
    Place {
        label: format!("Local Disk ({letter}:)"),
        path: PathBuf::from(format!(r"{letter}:\")),
        kind: PlaceKind::Drive {
            remote: false,
            space: None,
        },
    }
}

/// A window on `C:\work`, with drives `C:` and `D:` under This PC.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().extend([drive('C'), drive('D')]);
    let (ws, cx) = setup_workspace_with(cx, fs(), shell, r"C:\work");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\work")(v) && v.places.len() > 2
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    settle(cx, "the tree shows the folder", |cx| {
        cx.debug_bounds("sidebar work").is_some()
    });
    (ws, cx)
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn settle(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(cx) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        refresh(cx);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn key(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    refresh(cx);
}

fn region(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> &'static str {
    cx.update(|window, cx| ws.read(cx).region_for_test(window, cx))
}

fn cursor(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    ws.update(cx, |ws, _| ws.sidebar_cursor_for_test())
}

/// F6 goes from the list to the sidebar, the path bar, the search box and back; Shift+F6 the
/// other way. In the sidebar the cursor starts on the current folder.
#[gpui::test]
fn f6_goes_round_the_window(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    assert_eq!(region(&ws, cx), "list");
    key(cx, "f6");
    assert_eq!(region(&ws, cx), "sidebar");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(r"tree-1-C:\work"));
    key(cx, "f6");
    assert_eq!(region(&ws, cx), "path");
    key(cx, "f6");
    assert_eq!(region(&ws, cx), "search");
    key(cx, "f6");
    assert_eq!(region(&ws, cx), "list");
    key(cx, "shift-f6");
    assert_eq!(region(&ws, cx), "search");
    key(cx, "shift-f6");
    assert_eq!(region(&ws, cx), "path");
    key(cx, "shift-f6");
    assert_eq!(region(&ws, cx), "sidebar");
    key(cx, "escape");
    assert_eq!(region(&ws, cx), "list", "Escape goes back to the list");
}

/// The arrow keys walk the tree: Left goes up to the folder above, collapses it, Down reaches
/// the next drive, Right expands it and steps in, Enter opens a folder (the keyboard goes to the
/// list, as after a click).
#[gpui::test]
fn the_arrow_keys_switch_drives(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    key(cx, "f6");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(r"tree-1-C:\work"));
    key(cx, "left");
    assert_eq!(
        cursor(&ws, cx).as_deref(),
        Some(r"tree-0-C:\"),
        "up to the drive"
    );
    key(cx, "left");
    assert!(
        !ws.update(cx, |ws, _| ws.tree_expanded_for_test(Path::new(r"C:\"))),
        "the drive collapses"
    );
    key(cx, "down");
    assert_eq!(
        cursor(&ws, cx).as_deref(),
        Some(r"tree-0-D:\"),
        "the next drive"
    );
    key(cx, "right");
    settle(cx, "D: lists its folders", |cx| {
        cx.debug_bounds("sidebar games").is_some()
    });
    key(cx, "right");
    assert_eq!(
        cursor(&ws, cx).as_deref(),
        Some(r"tree-1-D:\games"),
        "into D:"
    );
    key(cx, "up");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(r"tree-0-D:\"));
    key(cx, "enter");
    wait_until(cx, &view, "D: open", loaded(r"D:\"));
    refresh(cx);
    assert_eq!(region(&ws, cx), "list");
    // Back in the sidebar, the cursor is where it was.
    key(cx, "f6");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(r"tree-0-D:\"));
    key(cx, "home");
    let first = cursor(&ws, cx);
    key(cx, "end");
    assert_ne!(cursor(&ws, cx), first, "Home and End reach both ends");
}

/// A click in the sidebar leaves the keyboard in the list, as before: the sidebar takes it only
/// through F6.
#[gpui::test]
fn a_click_in_the_sidebar_leaves_the_keyboard_in_the_list(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let heading = cx
        .debug_bounds("this-pc-header")
        .expect("This PC's heading")
        .center();
    for (count, at) in [(1, heading), (2, heading)] {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: count,
        });
        refresh(cx);
        assert_eq!(region(&ws, cx), "list");
    }
}

/// The path bar's folders have the same menu as the sidebar's: a right-click on `C:` shows the
/// drive's (Disk Cleanup, Properties), on `work` the folder's; the empty end of the bar keeps the
/// bar's own menu (#257).
#[gpui::test]
fn the_path_bar_folders_have_the_sidebar_menu(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let labels = |cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test())
            .unwrap_or_default()
    };
    let right_click = |cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>| {
        cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
        refresh(cx);
    };
    let drive = cx
        .debug_bounds("crumb-0")
        .expect("C: in the path bar")
        .center();
    right_click(cx, drive);
    let menu = labels(cx);
    assert!(
        menu.contains(&t!("menu.disk_cleanup").into_owned()),
        "{menu:?}"
    );
    assert!(
        menu.contains(&t!("menu.properties").into_owned()),
        "{menu:?}"
    );
    key(cx, "escape");

    let folder = cx
        .debug_bounds("crumb-1")
        .expect("work in the path bar")
        .center();
    right_click(cx, folder);
    let menu = labels(cx);
    assert!(
        menu.contains(&t!("sidebar.open_in_new_tab").into_owned()),
        "{menu:?}"
    );
    assert!(
        !menu.contains(&t!("menu.disk_cleanup").into_owned()),
        "{menu:?}"
    );
}
