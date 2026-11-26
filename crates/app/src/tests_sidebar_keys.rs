//! GPUI tests for the keyboard in the sidebar, F6 between the parts of the window and Tab and
//! Shift+Tab between its stops (`workspace_sidebar_keys.rs`, #322): a screen reader user couldn't
//! reach the sidebar at all, not even to switch drives.

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
    // The tree opens to the folder: off by default (#416), these tests are about it.
    cx.update(|cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.sidebar_follow = true, cx));
    });
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

/// F6 goes from the list to the sidebar, the path bar, the pane row's buttons, the search box and
/// back; Shift+F6 the other way. In the sidebar the cursor starts on the current folder.
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
    assert_eq!(region(&ws, cx), "toolbar");
    key(cx, "f6");
    assert_eq!(region(&ws, cx), "search");
    key(cx, "f6");
    assert_eq!(region(&ws, cx), "list");
    key(cx, "shift-f6");
    assert_eq!(region(&ws, cx), "search");
    key(cx, "shift-f6");
    assert_eq!(region(&ws, cx), "toolbar");
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

// ─── Tab and Shift+Tab walk the stops (#322) ───

/// Where Tab sees the keyboard (`workspace_sidebar_keys.rs`): "list0", "sidebar", "path",
/// "toolbar:<button>", "search" or "none".
fn stop(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    cx.update(|window, cx| ws.read(cx).stop_for_test(window, cx))
}

/// Presses `keys` until the keyboard is back where it started; the stops visited on the way,
/// the last one (the start) left out.
fn round(ws: &Entity<Workspace>, cx: &mut VisualTestContext, keys: &str) -> Vec<String> {
    let start = stop(ws, cx);
    round_to(ws, cx, keys, &start)
}

/// Presses `keys` until the keyboard is on `end`; the stops it was on before that.
fn round_to(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    keys: &str,
    end: &str,
) -> Vec<String> {
    let mut went = Vec::new();
    for _ in 0..80 {
        key(cx, keys);
        let at = stop(ws, cx);
        if at == end {
            return went;
        }
        went.push(at);
    }
    panic!("{keys} never reached {end}: {went:?}");
}

/// The stops with the pane row's buttons (and the box with its panel) counted once.
fn parts(went: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for at in went {
        let part = if at.starts_with("toolbar:") {
            "toolbar"
        } else {
            at.as_str()
        };
        if out.last() != Some(&part) {
            out.push(part);
        }
    }
    out
}

/// The row's buttons in the order they were visited.
fn buttons(went: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for at in went {
        if let Some(button) = at.strip_prefix("toolbar:")
            && out.last() != Some(&button)
        {
            out.push(button);
        }
    }
    out
}

/// Tab goes list, sidebar, path bar, each button of the pane row, the search box and its panel,
/// and round to the list; Shift+Tab goes the other way, through the same buttons.
#[gpui::test]
fn tab_walks_every_stop_and_shift_tab_walks_back(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    assert_eq!(stop(&ws, cx), "list0");
    let forward = round(&ws, cx, "tab");
    assert_eq!(
        parts(&forward),
        ["sidebar", "path", "toolbar", "search"],
        "{forward:?}"
    );
    let row = buttons(&forward);
    assert!(row.len() >= 2, "each button is a stop: {forward:?}");
    assert!(
        row.iter().all(|b| *b != "?"),
        "the row is on a button: {forward:?}"
    );
    let distinct: std::collections::BTreeSet<_> = row.iter().collect();
    assert_eq!(distinct.len(), row.len(), "no button twice: {forward:?}");
    assert_eq!(stop(&ws, cx), "list0");

    let back = round(&ws, cx, "shift-tab");
    assert_eq!(
        parts(&back),
        ["search", "toolbar", "path", "sidebar"],
        "{back:?}"
    );
    let mut reversed = buttons(&back);
    reversed.reverse();
    assert_eq!(reversed, row, "the same buttons, the other way round");
}

/// Tab and Shift+Tab from the search box, the path bar and the sidebar tree.
#[gpui::test]
fn tab_goes_on_from_the_search_box_the_path_bar_and_the_tree(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    key(cx, "ctrl-f");
    assert_eq!(stop(&ws, cx), "search");
    key(cx, "shift-tab");
    let last = stop(&ws, cx);
    assert!(last.starts_with("toolbar:"), "{last}");
    key(cx, "tab");
    assert_eq!(stop(&ws, cx), "search", "and forward again");
    // Into the panel (scope, filters, contents), and past it to the list.
    let after = round_to(&ws, cx, "tab", "list0");
    assert!(after.iter().all(|at| at == "search"), "{after:?}");

    key(cx, "ctrl-l");
    assert_eq!(stop(&ws, cx), "path");
    key(cx, "tab");
    assert!(stop(&ws, cx).starts_with("toolbar:"));
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "path");
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "sidebar");

    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "list0");
    key(cx, "f6");
    assert_eq!(stop(&ws, cx), "sidebar");
    key(cx, "tab");
    assert_eq!(stop(&ws, cx), "path");
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "sidebar");
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "list0");
}

/// Tab in the list leaves the list: it never moves between items.
#[gpui::test]
fn tab_in_the_list_does_not_move_between_items(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    key(cx, "home");
    let before = view.update(cx, |v, _| v.cursor);
    key(cx, "tab");
    assert_eq!(stop(&ws, cx), "sidebar");
    assert_eq!(view.update(cx, |v, _| v.cursor), before);
}

/// Dual pane: the other pane's list is the next stop after this one's, and Shift+Tab comes back.
#[gpui::test]
fn tab_reaches_the_other_pane_and_shift_tab_comes_back(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let left = ws.update(cx, |ws, _| ws.active_view().clone());
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    let right = ws.update(cx, |ws, _| ws.pane_views()[1].clone());
    wait_until(cx, &right, "the new side's first folder", |v| {
        v.model.as_ref().is_some_and(|m| m.complete)
    });
    refresh(cx);
    assert_eq!(stop(&ws, cx), "list1", "the new side has the keyboard");
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);

    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "list0");
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);
    key(cx, "tab");
    assert_eq!(
        stop(&ws, cx),
        "list1",
        "Tab from the left list reaches the right one"
    );
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);
    key(cx, "tab");
    assert_eq!(stop(&ws, cx), "sidebar", "and on from the right one");
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "list1");

    // The whole round: back to the right list through the left one.
    let forward = round(&ws, cx, "tab");
    assert_eq!(
        parts(&forward),
        ["sidebar", "path", "toolbar", "search", "list0"],
        "{forward:?}"
    );
}

/// The Home page is a stop of its own, and the rest of the round is the same.
#[gpui::test]
fn tab_walks_the_stops_from_the_home_page(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    refresh(cx);
    for _ in 0..6 {
        if stop(&ws, cx) == "list0" {
            break;
        }
        key(cx, "f6");
    }
    assert_eq!(stop(&ws, cx), "list0", "the page has the keyboard");
    // The pane row and the search box aren't drawn under a page: the page and the sidebar are
    // the only stops, and Tab never lands on a control that isn't there.
    let forward = round(&ws, cx, "tab");
    assert_eq!(parts(&forward), ["sidebar"], "{forward:?}");
    let back = round(&ws, cx, "shift-tab");
    assert_eq!(parts(&back), ["sidebar"], "{back:?}");
    key(cx, "tab");
    key(cx, "tab");
    assert_eq!(stop(&ws, cx), "list0");
}

/// With a context menu open, or while renaming, Tab doesn't walk to another region.
#[gpui::test]
fn tab_stays_put_in_a_menu_and_in_a_rename_box(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    key(cx, "home");
    key(cx, "shift-f10");
    settle(cx, "the menu opens", |cx| {
        view.update(cx, |v, _| v.context_menu.is_some())
    });
    for keys in ["tab", "shift-tab"] {
        key(cx, keys);
        assert_eq!(stop(&ws, cx), "none", "{keys} in the menu");
        assert!(view.update(cx, |v, _| v.context_menu.is_some()), "{keys}");
    }
    key(cx, "escape");
    settle(cx, "the menu closes", |cx| {
        view.update(cx, |v, _| v.context_menu.is_none())
    });

    key(cx, "home");
    key(cx, "f2");
    assert!(view.update(cx, |v, _| v.renaming.is_some()));
    // Tab in the rename box commits and goes on to the next item: still renaming, no stop.
    key(cx, "tab");
    assert!(view.update(cx, |v, _| v.renaming.is_some()));
    assert_eq!(stop(&ws, cx), "none");
    key(cx, "escape");
}

/// Tab in the path bar completes only what was typed, and moves on once there is nothing more
/// to complete: it never traps the keyboard.
#[gpui::test]
fn tab_in_the_path_bar_completes_typed_text_then_moves_on(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    // Untouched text: Tab moves on at once.
    key(cx, "ctrl-l");
    assert_eq!(stop(&ws, cx), "path");
    key(cx, "tab");
    assert!(stop(&ws, cx).starts_with("toolbar:"), "{}", stop(&ws, cx));

    key(cx, "ctrl-l");
    cx.simulate_input(r"C:\work\re");
    settle(cx, "suggestions", |cx| {
        view.update(cx, |v, _| v.suggestion_names() == ["reports"])
    });
    key(cx, "tab");
    assert_eq!(
        view.update(cx, |v, cx| v.path_text(cx)),
        r"C:\work\reports\"
    );
    assert_eq!(stop(&ws, cx), "path", "Tab completed it");
    key(cx, "tab");
    assert!(stop(&ws, cx).starts_with("toolbar:"), "{}", stop(&ws, cx));
}

/// The sidebar's rows follow the density like the file list (#438): compact rows in compact
/// density, the standard height otherwise (comfortable keeps it, as before).
#[gpui::test]
fn sidebar_rows_follow_the_density(cx: &mut TestAppContext) {
    use crate::settings::Density;
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    let mut tiles = Vec::new();
    for (density, side, list) in [
        (Density::Compact, 24., 21.),
        (Density::Standard, 32., 32.),
        (Density::Comfortable, 32., 40.),
    ] {
        cx.update(|_, cx| {
            crate::settings::SettingsStore::global(cx)
                .update(cx, |s, cx| s.update(|st| st.density = density, cx));
        });
        refresh(cx);
        let tree = cx.debug_bounds("sidebar work").expect("work in the tree");
        let tab = cx.debug_bounds("tab-0").expect("the tab in the sidebar");
        assert_eq!(tree.size.height, gpui::px(side), "{density:?}: tree row");
        assert_eq!(tab.size.height, gpui::px(side), "{density:?}: tab row");
        assert_eq!(crate::theme::h_row_side(), gpui::px(side), "{density:?}");
        assert_eq!(
            view.update(cx, |v, _| v.row_height()),
            gpui::px(list),
            "{density:?}: list row"
        );
        tiles.push(view.update(cx, |v, _| v.tile().h));
    }
    // Icons: compact rows of tiles sit 4 px closer; the others keep the tile's height.
    assert_eq!(tiles[0], tiles[1] - gpui::px(4.));
    assert_eq!(tiles[1], tiles[2]);
}
