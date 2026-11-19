//! GPUI tests for tabs and dual pane: tab cycling, pinned tabs, saved workspaces, synchronized
//! browsing and folder compare (`workspace.rs`, `workspace_saved.rs`, `explorer_compare.rs`).

use arke_core::compare::Mark;
use std::sync::Arc;

use super::*;
use crate::saved_workspaces::WorkspaceStore;
use crate::workspace::{CompareFolders, OpenWorkspaces, SelectDifferences, ToggleSyncBrowsing};

fn active(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.update(cx, |ws, _| ws.active)
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

/// Opens tabs on `paths` after the first one and waits for each to list.
fn open_tabs(ws: &Entity<Workspace>, cx: &mut VisualTestContext, paths: &[&str]) {
    for path in paths {
        ws.update_in(cx, |ws, window, cx| {
            let at = ws.tab_views().count();
            ws.open_tab(p(path), at, true, window, cx);
        });
        let view = ws.update(cx, |ws, _| ws.active_view().clone());
        wait_until(cx, &view, path, loaded(path));
    }
}

/// Splits the active tab and sends the right pane to `right`; returns both panes.
fn split_to(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    right: &str,
) -> (Entity<Explorer>, Entity<Explorer>) {
    let left = ws.update(cx, |ws, _| ws.active_view().clone());
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    let right_view = ws.update(cx, |ws, _| ws.pane_views()[1].clone());
    right_view.update(cx, |e, cx| e.navigate(p(right), NavKind::New, cx));
    wait_until(cx, &right_view, right, loaded(right));
    cx.run_until_parked();
    (left, right_view)
}

#[gpui::test]
fn ctrl_tab_and_ctrl_shift_tab_cycle_in_tab_order_and_wrap(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b", r"C:\c"]);
    cx.simulate_keystrokes("ctrl-1");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 0);

    // Back from the first tab wraps to the last, like a browser.
    cx.simulate_keystrokes("ctrl-shift-tab");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 2);
    cx.simulate_keystrokes("ctrl-shift-tab");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 1);
    cx.simulate_keystrokes("ctrl-tab");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 2);
    cx.simulate_keystrokes("ctrl-tab");
    cx.run_until_parked();
    assert_eq!(
        active(&ws, cx),
        0,
        "forward from the last wraps to the first"
    );
    cx.simulate_keystrokes("ctrl-pageup");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 2);
    cx.simulate_keystrokes("ctrl-pagedown");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 0);

    // Order is the tab list's, also after a reorder.
    ws.update(cx, |ws, cx| ws.move_tab_for_test(2, 0, cx));
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.active, 1, "the active tab moved one down");
        assert_eq!(tab_paths(ws, cx)[0], Some(p(r"C:\c")));
    });
    cx.simulate_keystrokes("ctrl-shift-tab");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx)[ws.active], Some(p(r"C:\c")));
    });
}

#[gpui::test]
fn pinned_tabs_stay_first_and_survive_bulk_closes(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c", r"C:\d"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b", r"C:\c"]);

    // Pinning c moves it to the top.
    ws.update(cx, |ws, cx| ws.toggle_pin(2, cx));
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pinned_for_test(), [true, false, false]);
        assert_eq!(
            tab_paths(ws, cx),
            [Some(p(r"C:\c")), Some(p(r"C:\a")), Some(p(r"C:\b"))]
        );
        assert_eq!(ws.active, 0, "the active tab is still c");
    });

    // A new tab asked for at the very top goes below the pinned one; drags can't cross either.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_tab(p(r"C:\d"), 0, false, window, cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx)[1], Some(p(r"C:\d")));
        ws.move_tab_for_test(3, 0, cx);
        assert_eq!(
            tab_paths(ws, cx)[1],
            Some(p(r"C:\b")),
            "stops below the pin"
        );
        ws.move_tab_for_test(0, 3, cx);
        assert_eq!(
            tab_paths(ws, cx)[0],
            Some(p(r"C:\c")),
            "the pin stays on top"
        );
    });

    // Close other tabs keeps the pinned tab too; close all keeps only the pinned tab.
    ws.update_in(cx, |ws, window, cx| ws.close_other_tabs(2, window, cx));
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\c")), Some(p(r"C:\d"))]);
    });
    ws.update_in(cx, Workspace::close_all_tabs);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\c"))]);
        assert!(ws.session_snapshot(cx).tabs[0].pinned);
    });

    // Reopening a closed pinned tab brings it back pinned.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_tab(p(r"C:\a"), 1, true, window, cx);
        ws.toggle_pin(1, cx);
        ws.close_tab(1, window, cx);
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pinned_for_test(), [true, true]);
        assert_eq!(tab_paths(ws, cx)[1], Some(p(r"C:\a")));
        // Unpinning moves it just below the pinned group.
        ws.toggle_pin(0, cx);
        assert_eq!(ws.pinned_for_test(), [true, false]);
        assert_eq!(tab_paths(ws, cx)[1], Some(p(r"C:\c")));
    });
}

/// Presses on tab `ix` and moves a little so the tab drag starts; returns where it was pressed.
fn start_tab_drag(cx: &mut VisualTestContext, ix: usize) -> gpui::Point<gpui::Pixels> {
    use gpui::{Modifiers, MouseButton, point, px};
    let start = cx
        .debug_bounds(["tab-0", "tab-1", "tab-2"][ix])
        .expect("the tab should be drawn")
        .center();
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(12.), px(1.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    start
}

/// Moves the dragged tab to `to` and releases it there.
fn drop_tab_at(cx: &mut VisualTestContext, to: gpui::Point<gpui::Pixels>) {
    use gpui::{Modifiers, MouseButton};
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn dragging_a_tab_onto_the_pin_zone_pins_it(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b", r"C:\c"]);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("tab-pin-drop").is_none(),
        "no pin zone without a drag"
    );

    // Dragging c: the New tab row becomes the pin zone; dropping there pins c at the top.
    start_tab_drag(cx, 2);
    let zone = cx
        .debug_bounds("tab-pin-drop")
        .expect("dragging an unpinned tab should show the pin zone");
    drop_tab_at(cx, zone.center());
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pinned_for_test(), [true, false, false]);
        assert_eq!(tab_paths(ws, cx)[0], Some(p(r"C:\c")));
    });
    assert!(
        cx.debug_bounds("tab-pin-drop").is_none(),
        "the zone goes away after the drop"
    );

    // A pinned tab gets no pin zone; dragged onto the tab below the pinned group, it is unpinned
    // there.
    start_tab_drag(cx, 0);
    assert!(
        cx.debug_bounds("tab-pin-drop").is_none(),
        "a pinned tab can't be pinned again"
    );
    let below = cx.debug_bounds("tab-1").expect("tab a should be drawn");
    let lower_half = gpui::point(below.center().x, below.bottom() - gpui::px(4.));
    drop_tab_at(cx, lower_half);
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pinned_for_test(), [false, false, false]);
        assert_eq!(
            tab_paths(ws, cx),
            [Some(p(r"C:\a")), Some(p(r"C:\c")), Some(p(r"C:\b"))]
        );
    });
}

#[gpui::test]
fn horizontal_tabs_pin_by_drag_too(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b"]);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| st.tab_layout = crate::settings::TabLayout::Horizontal,
                cx,
            );
        });
    });
    cx.run_until_parked();

    start_tab_drag(cx, 1);
    let zone = cx
        .debug_bounds("tab-pin-drop")
        .expect("the pin zone shows under the tab strip");
    drop_tab_at(cx, zone.center());
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pinned_for_test(), [true, false]);
        assert_eq!(tab_paths(ws, cx)[0], Some(p(r"C:\b")));
    });
}

#[gpui::test]
fn saved_workspaces_save_open_and_show_in_ctrl_t(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 1);
    fs.insert_many(r"C:\t\two", 2);
    fs.insert_many(r"C:\x", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\t"));
    open_tabs(&ws, cx, &[r"C:\t\one"]);
    split_to(&ws, cx, r"C:\t\two");
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));

    // Save through the panel.
    cx.dispatch_action(OpenWorkspaces);
    cx.run_until_parked();
    let panel = ws
        .update(cx, |ws, _| ws.workspaces_panel_for_test())
        .expect("panel open");
    panel.update(cx, |panel, cx| panel.type_name_for_test("  ", cx));
    assert_eq!(
        panel.update(cx, |panel, _| panel.error_for_test()),
        Some(t!("workspaces.name_empty").into_owned())
    );
    panel.update(cx, |panel, cx| panel.type_name_for_test("Work", cx));
    cx.run_until_parked();
    assert!(
        ws.update(cx, |ws, _| ws.workspaces_panel_for_test())
            .is_none(),
        "saving closes the panel"
    );
    let saved = cx.update(|_, cx| {
        WorkspaceStore::global(cx)
            .read(cx)
            .workspaces()
            .get("work")
            .cloned()
    });
    let saved = saved.expect("saved");
    assert_eq!(saved.tabs.len(), 2);
    assert!(saved.tabs[0].pinned);
    assert_eq!(saved.tabs[1].panes, [p(r"C:\t\one"), p(r"C:\t\two")]);

    // Somewhere else entirely, then open it again: the pinned tab isn't doubled.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_tab(p(r"C:\x"), 2, true, window, cx);
        ws.close_tab(1, window, cx);
    });
    cx.run_until_parked();
    ws.update_in(cx, |ws, window, cx| {
        ws.open_saved_workspace("Work", window, cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(
            ws.all_paths_for_test(cx),
            [vec![p(r"C:\t")], vec![p(r"C:\t\one"), p(r"C:\t\two")]]
        );
        assert_eq!(ws.pinned_for_test(), [true, false]);
        assert_eq!(ws.active, 1, "the tab that was active when saved");
    });
    // The replaced tab comes back with Ctrl+Shift+T.
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert!(ws.all_paths_for_test(cx).contains(&vec![p(r"C:\x")]));
    });

    // Ctrl+T lists it by name.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    let palette = ws.update(cx, |ws, _| ws.palette.as_ref().map(|(p, _)| p.clone()));
    let palette = palette.expect("palette open");
    cx.simulate_input("Work");
    cx.run_until_parked();
    let labels = palette.update(cx, |p, _| p.row_labels());
    assert_eq!(
        labels.first().map(String::as_str),
        Some("Work"),
        "{labels:?}"
    );
}

#[gpui::test]
fn synchronized_browsing_repeats_moves_in_the_other_pane(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\l", vec![dir("sub"), dir("only")]);
    fs.insert_many(r"C:\l\sub", 1);
    fs.insert_many(r"C:\l\only", 1);
    fs.insert_dir(r"C:\r", vec![dir("SUB")]);
    fs.insert_many(r"C:\r\SUB", 2);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\l");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\l"));
    let (left, right) = split_to(&ws, cx, r"C:\r");
    cx.dispatch_action(ToggleSyncBrowsing);
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.synced_for_test()), [true]);
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);

    // Into a subfolder on the right: the left follows (names match in any case).
    right.update(cx, |e, cx| e.navigate(p(r"C:\r\SUB"), NavKind::New, cx));
    wait_until(cx, &right, "right sub", loaded(r"C:\r\SUB"));
    wait_until(cx, &left, "left follows down", loaded(r"C:\l\sub"));

    // Up on the right: the left goes up too.
    right.update(cx, |e, cx| e.navigate(p(r"C:\r"), NavKind::New, cx));
    wait_until(cx, &right, "right up", loaded(r"C:\r"));
    wait_until(cx, &left, "left follows up", loaded(r"C:\l"));

    // From the left into a folder the right doesn't have: the right stays put.
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::Activated));
    cx.run_until_parked();
    left.update(cx, |e, cx| e.navigate(p(r"C:\l\only"), NavKind::New, cx));
    wait_until(cx, &left, "left only", loaded(r"C:\l\only"));
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    assert_eq!(
        right.update(cx, |e, _| e.current_path().map(Path::to_path_buf)),
        Some(p(r"C:\r"))
    );

    // Off: moves no longer repeat, and the flag is saved with the session.
    assert!(ws.update(cx, |ws, cx| ws.session_snapshot(cx).tabs[0].synced));
    cx.dispatch_action(ToggleSyncBrowsing);
    left.update(cx, |e, cx| e.navigate(p(r"C:\l"), NavKind::New, cx));
    wait_until(cx, &left, "left up", loaded(r"C:\l"));
    cx.run_until_parked();
    assert_eq!(
        right.update(cx, |e, _| e.current_path().map(Path::to_path_buf)),
        Some(p(r"C:\r"))
    );
}

#[gpui::test]
fn compare_folders_marks_and_selects_the_differences(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\a",
        vec![
            file("same.txt", 1),
            file("changed.txt", 1),
            file("left.txt", 1),
            dir("sub"),
        ],
    );
    fs.insert_dir(
        r"C:\b",
        vec![
            file("same.txt", 1),
            file("Changed.txt", 2),
            file("right.txt", 1),
        ],
    );
    fs.insert_many(r"C:\a\sub", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    let (left, right) = split_to(&ws, cx, r"C:\b");

    cx.dispatch_action(CompareFolders);
    cx.update(|window, _| window.refresh());
    wait_until(cx, &left, "left marks", |e| e.compare.is_some());
    wait_until(cx, &right, "right marks", |e| e.compare.is_some());
    let marks = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        view.update(cx, |e, _| {
            let mut m: Vec<(String, Mark)> = e
                .compare
                .as_ref()
                .map_or_default(|m| m.iter().map(|(k, v)| (k.to_string(), *v)).collect());
            m.sort_by(|a, b| a.0.cmp(&b.0));
            m
        })
    };
    assert_eq!(
        marks(&left, cx),
        [
            ("changed.txt".to_owned(), Mark::Differs),
            ("left.txt".to_owned(), Mark::Unique),
            ("sub".to_owned(), Mark::Unique),
        ]
    );
    assert_eq!(
        marks(&right, cx),
        [
            ("Changed.txt".to_owned(), Mark::Differs),
            ("right.txt".to_owned(), Mark::Unique),
        ]
    );

    cx.dispatch_action(SelectDifferences);
    cx.run_until_parked();
    assert_eq!(
        left.update(cx, |e, _| sorted(selected_names(e))),
        ["changed.txt", "left.txt", "sub"]
    );
    assert_eq!(
        right.update(cx, |e, _| sorted(selected_names(e))),
        ["Changed.txt", "right.txt"]
    );

    // Going into a folder ends the comparison on both sides.
    left.update(cx, |e, cx| e.navigate(p(r"C:\a\sub"), NavKind::New, cx));
    wait_until(cx, &left, "sub", loaded(r"C:\a\sub"));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(left.update(cx, |e, _| e.compare.is_none()));
    assert!(right.update(cx, |e, _| e.compare.is_none()));
    assert!(!left.update(cx, |e, _| e.pane.comparing));
}

/// Alt+C and Alt+M select what arrived in the other pane (like a paste), Shift+Tab switches sides
/// like Tab, and the sync and compare toggles are drawn once, in the right pane.
#[gpui::test]
fn copies_to_the_other_pane_select_what_arrived(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\l", vec![file("a.txt", 1), file("b.txt", 1)]);
    fs.insert_dir(r"C:\r", vec![file("z.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    {
        // Copies and moves land in the fake file system, as the real ones would.
        let fs = Arc::clone(&fs);
        *shell.on_op.lock() = Some(Box::new(move |op| {
            let (FileOp::Copy { sources, dest } | FileOp::Move { sources, dest }) = op else {
                return;
            };
            let names: Vec<&str> = sources
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect();
            for name in &names {
                fs.add_file(dest, name, 1);
            }
            fs.emit_changed(dest, &names);
        }));
    }
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\l");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\l"));
    let (left, right) = split_to(&ws, cx, r"C:\r");
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        [&left, &right].map(|v| v.update(cx, |e, _| e.pane.pair_toggles)),
        [false, true]
    );

    // Shift+Tab from the right goes to the left (Tab is covered elsewhere).
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);
    cx.simulate_keystrokes("shift-tab");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);

    left.update(cx, |e, cx| e.select_index(1, cx));
    assert_eq!(left.update(cx, |e, _| selected_names(e)), ["b.txt"]);
    cx.simulate_keystrokes("alt-c");
    wait_until(cx, &right, "the copy is selected on the right", |e| {
        selected_names(e) == ["b.txt"]
    });
    assert_eq!(
        ws.update(cx, |ws, _| ws.active_pane()),
        0,
        "the left side stays active"
    );
    assert_eq!(left.update(cx, |e, _| selected_names(e)), ["b.txt"]);
}

/// The item menu offers "Copy to/Move to other pane" (Alt+C / Alt+M) only in a split tab, and
/// they copy like the shortcuts do.
#[gpui::test]
fn item_menu_copies_to_the_other_pane_only_when_split(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\l", vec![file("a.txt", 1), file("b.txt", 1)]);
    fs.insert_dir(r"C:\r", vec![file("z.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    {
        let fs = Arc::clone(&fs);
        *shell.on_op.lock() = Some(Box::new(move |op| {
            let FileOp::Copy { sources, dest } = op else {
                return;
            };
            let names: Vec<&str> = sources
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect();
            for name in &names {
                fs.add_file(dest, name, 1);
            }
            fs.emit_changed(dest, &names);
        }));
    }
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\l");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\l"));
    let copy = t!("menu.copy_to_other_pane").into_owned();
    let mv = t!("menu.move_to_other_pane").into_owned();

    right_click(&first, cx, Some(1));
    let labels = menu_labels(&first, cx).expect("a menu opens");
    assert!(
        !labels.contains(&copy) && !labels.contains(&mv),
        "one pane: nothing to copy to, {labels:?}"
    );

    let (left, right) = split_to(&ws, cx, r"C:\r");
    right_click(&left, cx, Some(1));
    let labels = menu_labels(&left, cx).expect("a menu opens");
    let at = labels.iter().position(|l| *l == copy).expect("copy item");
    assert_eq!(labels.get(at + 1), Some(&mv), "move right under copy");
    let shortcuts = left.update(cx, |e, _| {
        let menu = e.context_menu.as_ref().expect("still open");
        [menu.shortcut_of(&copy), menu.shortcut_of(&mv)]
    });
    assert_eq!(shortcuts, [Some("Alt+C"), Some("Alt+M")]);

    run_command(&left, cx, MenuCommand::CopyToPeer);
    wait_until(cx, &right, "the copy is selected on the right", |e| {
        selected_names(e) == ["b.txt"]
    });
    assert!(
        matches!(shell.ops.lock().as_slice(), [FileOp::Copy { dest, .. }] if dest == Path::new(r"C:\r")),
        "copied into the other pane's folder"
    );
}

/// Closing a split from the right side keeps the two folders in their on-screen order: the left
/// one becomes the tab before.
#[gpui::test]
fn unsplitting_from_the_right_keeps_the_left_side_first(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\l", 1);
    fs.insert_many(r"C:\r", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\l");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\l"));
    let (_, right) = split_to(&ws, cx, r"C:\r");
    right.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pane_views().len(), 1);
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\l")), Some(p(r"C:\r"))]);
        assert_eq!(ws.active, 1, "the side that was clicked stays in front");
    });
}

/// The item the preview shows.
fn previewing(v: &Explorer) -> Option<String> {
    v.preview.as_ref().map(|p| p.entry.name.to_string())
}

/// A split tab has one preview, at its right end, showing the active side's item: switching
/// sides switches it over, and the side left behind drops its own copy. Unsplit, the pane draws
/// its own preview again.
#[gpui::test]
fn split_tab_shares_one_preview_that_follows_the_active_side(cx: &mut TestAppContext) {
    use gpui::px;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![file("left.txt", 4)]);
    fs.set_content(Path::new(r"C:\a\left.txt"), b"left".to_vec());
    fs.insert_dir(r"C:\b", vec![file("right.txt", 5)]);
    fs.set_content(Path::new(r"C:\b\right.txt"), b"right".to_vec());
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    cx.simulate_resize(gpui::size(px(1700.), px(900.)));
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    assert!(first.update(cx, |v, _| v.fit_for_test().0), "own preview");
    assert!(
        ws.update(cx, |ws, _| ws.shared_preview_for_test())
            .is_none()
    );

    let (left, right) = split_to(&ws, cx, r"C:\b");
    let shared = ws
        .update(cx, |ws, _| ws.shared_preview_for_test())
        .expect("the split tab has a shared preview");
    for view in [&left, &right] {
        assert!(
            !view.update(cx, |v, _| v.fit_for_test().0),
            "no side draws its own preview"
        );
    }
    // The panes share what the preview leaves; the right one's top bar runs on above it.
    let (left_paper, left_bar) = left.update(cx, |v, _| v.widths_for_test());
    let (right_paper, right_bar) = right.update(cx, |v, _| v.widths_for_test());
    assert_eq!(left_paper, left_bar);
    assert_eq!(right_bar - right_paper, shared.reserve());
    let content = px(1700.) - crate::theme::sidebar_full_width();
    assert!(
        (f32::from(left_paper + right_paper + shared.reserve()) - f32::from(content)).abs() < 0.5,
        "{left_paper:?} + {right_paper:?} + {shared:?} fill {content:?}"
    );

    // The split opened on the right: its selection is previewed.
    right.update(cx, |e, cx| e.select_index(0, cx));
    wait_until(cx, &right, "right previewed", |v| {
        previewing(v).as_deref() == Some("right.txt")
    });
    assert!(right.update(cx, |v, _| v.pane.preview_host.is_some()));
    assert!(left.update(cx, |v, _| v.pane.preview_host.is_none()));

    // Over to the left side: the preview follows it, the right side lets go of its item.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);
    left.update(cx, |e, cx| e.select_index(0, cx));
    wait_until(cx, &left, "left previewed", |v| {
        previewing(v).as_deref() == Some("left.txt")
    });
    assert!(left.update(cx, |v, _| v.pane.preview_host.is_some()));
    assert!(right.update(cx, |v, _| v.pane.preview_host.is_none()));
    assert_eq!(right.update(cx, |v, _| previewing(v)), None);

    // And back by clicking into the right side.
    right.update(cx, |_, cx| cx.emit(ExplorerEvent::Activated));
    wait_until(cx, &right, "right previewed again", |v| {
        previewing(v).as_deref() == Some("right.txt")
    });
    assert_eq!(left.update(cx, |v, _| previewing(v)), None);

    // Unsplit: the kept pane draws its own preview again.
    right.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    assert!(
        ws.update(cx, |ws, _| ws.shared_preview_for_test())
            .is_none()
    );
    assert!(
        right.update(cx, |v, _| v.fit_for_test().0),
        "own preview again"
    );
    assert_eq!(
        right.update(cx, |v, _| previewing(v)).as_deref(),
        Some("right.txt")
    );
}

/// Ctrl+W in split view closes only the active side: the other one stays as the tab's pane and
/// takes focus. Pressed again, it closes the tab. Ctrl+Shift+T brings the closed side back.
#[gpui::test]
fn ctrl_w_closes_the_active_side_then_the_tab(cx: &mut TestAppContext) {
    use gpui::Focusable as _;
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\c");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\c"));
    open_tabs(&ws, cx, &[r"C:\a"]);
    let (_, right) = split_to(&ws, cx, r"C:\b");
    // Make the left side the active one.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);

    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(
            ws.pane_views(),
            std::slice::from_ref(&right),
            "the right side stays"
        );
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\c")), Some(p(r"C:\b"))]);
        assert_eq!(ws.active, 1);
    });
    assert!(
        cx.update(|window, cx| right.read(cx).focus_handle(cx).is_focused(window)),
        "the side left takes focus"
    );

    // Not split any more: Ctrl+W closes the tab.
    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\c"))]);
    });

    // Reopening brings back the tab, then the closed side as a tab of its own.
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(
            tab_paths(ws, cx),
            [Some(p(r"C:\c")), Some(p(r"C:\a")), Some(p(r"C:\b"))]
        );
    });
}

#[gpui::test]
fn a_folder_from_another_app_opens_as_a_tab_and_reuses_one_showing_it(cx: &mut TestAppContext) {
    use arke_core::takeover::Target;
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    fs.insert_many(r"C:\dl", 3);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));

    let one: Arc<str> = "file_000001.txt".into();
    ws.update_in(cx, |ws, window, cx| {
        ws.open_external(
            Target::Folder(p(r"C:\dl")),
            vec![Arc::clone(&one)],
            window,
            cx,
        );
    });
    let dl = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &dl, "dl", loaded(r"C:\dl"));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);
    assert_eq!(active(&ws, cx), 1);
    assert_eq!(
        dl.update(cx, |e, _| selected_names(e)),
        vec!["file_000001.txt"]
    );

    // Again for the same folder (other case, trailing backslash) while on the first tab: the tab
    // already showing it comes back with the new item selected, no third tab.
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    let two: Arc<str> = "file_000002.txt".into();
    ws.update_in(cx, |ws, window, cx| {
        ws.open_external(
            Target::Folder(p(r"c:\DL\")),
            vec![Arc::clone(&two)],
            window,
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);
    assert_eq!(active(&ws, cx), 1);
    assert_eq!(
        dl.update(cx, |e, _| selected_names(e)),
        vec!["file_000002.txt"]
    );
    // The folder was already listed: the item also gets the cursor (focus), not just the selection.
    let at_cursor = dl.update(cx, |e, _| {
        e.cursor.and_then(|ix| {
            e.view()
                .and_then(|v| v.get(ix).map(|entry| entry.name.to_string()))
        })
    });
    assert_eq!(at_cursor.as_deref(), Some("file_000002.txt"));
}

#[gpui::test]
fn closing_the_last_tab_closes_the_window(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));

    // Like File Explorer and browsers, not via Home first (#98).
    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    assert_eq!(cx.read(|cx| cx.windows().len()), 0);
}

#[gpui::test]
fn mouse_back_and_forward_work_anywhere_in_the_window(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, NavigationDirection, point, px};
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    for path in [r"C:\b", r"C:\c"] {
        view.update(cx, |e, cx| e.navigate(p(path), NavKind::New, cx));
        wait_until(cx, &view, path, loaded(path));
    }
    cx.run_until_parked();
    let press = |cx: &mut VisualTestContext, direction, position| {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Navigate(direction),
            position,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
    };
    let at = |cx: &mut VisualTestContext| {
        view.update(cx, |e, _| e.current_path().map(Path::to_path_buf))
    };

    // Over the list: the pane handles it.
    let list = view.update(cx, |e, _| e.list_bounds_for_test());
    let over_list = point(list.left() + px(40.), list.top() + px(10.));
    press(cx, NavigationDirection::Back, over_list);
    wait_until(cx, &view, "back to b", loaded(r"C:\b"));
    assert_eq!(at(cx), Some(p(r"C:\b")));

    // Left of the list (the sidebar), outside any pane: still the active tab.
    let beside = point(px(8.), list.top() + px(10.));
    assert!(beside.x < list.left(), "the test point is outside the list");
    press(cx, NavigationDirection::Back, beside);
    wait_until(cx, &view, "back to a", loaded(r"C:\a"));
    assert_eq!(at(cx), Some(p(r"C:\a")));
    press(cx, NavigationDirection::Forward, beside);
    wait_until(cx, &view, "forward to b", loaded(r"C:\b"));
    assert_eq!(at(cx), Some(p(r"C:\b")));
}

/// #70: a two-finger swipe on a touchpad (sideways wheel messages in fractions of a notch) goes
/// back or forward once per swipe; a mouse wheel tilted sideways (whole notches) and a short swipe
/// don't, and neither does a swipe with the setting off.
#[gpui::test]
fn touchpad_swipes_go_back_and_forward(cx: &mut TestAppContext) {
    use gpui::{ScrollDelta, ScrollEvent, point, px};
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    view.update(cx, |e, cx| e.navigate(p(r"C:\b"), NavKind::New, cx));
    wait_until(cx, &view, "b", loaded(r"C:\b"));
    let at = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        view.update(cx, |e, _| e.display_path().map(Path::to_path_buf))
    };
    // A swipe is a burst of messages; bursts are kept apart by waiting out the gap.
    let swipe = |cx: &mut VisualTestContext, lines: f32, count: usize| {
        for _ in 0..count {
            cx.simulate_event(ScrollEvent {
                position: point(px(400.), px(300.)),
                delta: ScrollDelta::Lines(point(lines, 0.)),
                ..ScrollEvent::default()
            });
        }
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    swipe(cx, 0.75, 4);
    assert_eq!(at(cx), Some(p(r"C:\b")), "a short swipe doesn't count");
    swipe(cx, 0.75, 12);
    assert_eq!(
        at(cx),
        Some(p(r"C:\a")),
        "fingers right: back, one step only"
    );
    swipe(cx, -0.75, 12);
    assert_eq!(at(cx), Some(p(r"C:\b")), "fingers left: forward");
    swipe(cx, 3., 4);
    assert_eq!(
        at(cx),
        Some(p(r"C:\b")),
        "a tilted mouse wheel sends whole notches"
    );

    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, _| s.settings.swipe_nav = false);
    });
    swipe(cx, 0.75, 12);
    assert_eq!(at(cx), Some(p(r"C:\b")), "the setting is off");
}

/// #70: a folder opened from Home goes back to Home, like a browser. Home where the tab started
/// is as far back as it goes; Home opened over a folder goes back to that folder.
#[gpui::test]
fn back_from_a_folder_opened_from_home_returns_to_home(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, NavigationDirection, point, px};
    let fs = FakeFs::new();
    for dir in [r"C:\Users\me", r"C:\Users\me\Desktop", r"C:\work"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\Users\me");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "profile", loaded(r"C:\Users\me"));
    let page = |cx: &mut VisualTestContext| ws.update(cx, |ws, _| ws.shown_page_for_test());
    let at = |cx: &mut VisualTestContext| {
        view.update(cx, |e, _| e.current_path().map(Path::to_path_buf))
    };
    let back = |cx: &mut VisualTestContext| {
        // Over the sidebar, as with Home showing there is no list to press over.
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Navigate(NavigationDirection::Back),
            position: point(px(8.), px(300.)),
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
    };
    let home = t!("home.title").into_owned();

    // The reporter's steps: Home at startup, Desktop from Home, then back twice.
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_start_home_for_test(window, cx)));
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.home_open_folder_for_test(p(r"C:\Users\me\Desktop"), window, cx);
        });
    });
    wait_until(cx, &view, "desktop", loaded(r"C:\Users\me\Desktop"));
    assert_eq!(page(cx), None);
    back(cx);
    assert_eq!(
        page(cx).as_deref(),
        Some(home.as_str()),
        "back from Desktop shows Home"
    );
    back(cx);
    assert_eq!(
        page(cx).as_deref(),
        Some(home.as_str()),
        "Home is where the tab started"
    );

    // Home opened over a folder: back from the folder opened from it shows Home, and back
    // again closes Home on the folder it was opened from.
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.home_open_folder_for_test(p(r"C:\work"), window, cx);
        });
    });
    wait_until(cx, &view, "work", loaded(r"C:\work"));
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_home_for_test(false, window, cx)));
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.home_open_folder_for_test(p(r"C:\Users\me\Desktop"), window, cx);
        });
    });
    wait_until(cx, &view, "desktop again", loaded(r"C:\Users\me\Desktop"));
    back(cx);
    wait_until(cx, &view, "under Home: work", loaded(r"C:\work"));
    assert_eq!(page(cx).as_deref(), Some(home.as_str()));
    back(cx);
    assert_eq!(
        page(cx),
        None,
        "back from Home shows the folder it was opened from"
    );
    assert_eq!(at(cx), Some(p(r"C:\work")));
}

/// Tab A goes a → b; a new tab B opens at c and goes to d. Returns the two list views.
fn two_tabs_with_history(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<Explorer>, Entity<Explorer>) {
    let a = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &a, "a", loaded(r"C:\a"));
    a.update(cx, |e, cx| e.navigate(p(r"C:\b"), NavKind::New, cx));
    wait_until(cx, &a, "b", loaded(r"C:\b"));
    open_tabs(ws, cx, &[r"C:\c"]);
    let b = ws.update(cx, |ws, _| ws.active_view().clone());
    b.update(cx, |e, cx| e.navigate(p(r"C:\d"), NavKind::New, cx));
    wait_until(cx, &b, "d", loaded(r"C:\d"));
    cx.run_until_parked();
    (a, b)
}

fn four_folders() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c", r"C:\d"] {
        fs.insert_many(dir, 1);
    }
    fs
}

#[gpui::test]
fn back_crosses_tabs_in_the_order_you_went_and_forward_retraces_it(cx: &mut TestAppContext) {
    let (ws, cx) = setup_workspace(cx, four_folders(), r"C:\a");
    let (a, b) = two_tabs_with_history(&ws, cx);
    let step = |cx: &mut VisualTestContext, keys: &str| {
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
    };

    step(cx, "alt-left");
    wait_until(cx, &b, "B back to c", loaded(r"C:\c"));
    assert_eq!(active(&ws, cx), 1, "still in tab B");
    // B is at its first folder: back returns to tab A as it was left.
    step(cx, "alt-left");
    assert_eq!(active(&ws, cx), 0);
    assert_eq!(
        a.update(cx, |e, _| e.current_path().map(Path::to_path_buf)),
        Some(p(r"C:\b"))
    );
    step(cx, "alt-left");
    wait_until(cx, &a, "A back to a", loaded(r"C:\a"));

    step(cx, "alt-right");
    wait_until(cx, &a, "A forward to b", loaded(r"C:\b"));
    step(cx, "alt-right");
    assert_eq!(active(&ws, cx), 1, "forward goes back into tab B");
    step(cx, "alt-right");
    wait_until(cx, &b, "B forward to d", loaded(r"C:\d"));
}

#[gpui::test]
fn within_the_current_tab_back_stays_in_the_tab(cx: &mut TestAppContext) {
    let (ws, cx) = setup_workspace(cx, four_folders(), r"C:\a");
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.nav_scope = crate::settings::NavScope::WithinTab, cx);
        });
    });
    let (_, b) = two_tabs_with_history(&ws, cx);
    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &b, "B back to c", loaded(r"C:\c"));
    cx.simulate_keystrokes("alt-left");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 1, "nothing left in this tab: stays");
    assert_eq!(
        b.update(cx, |e, _| e.current_path().map(Path::to_path_buf)),
        Some(p(r"C:\c"))
    );
}

/// Folder keys on Home (#77): Home yields to the folder under it, in the same tab, and the key
/// acts there.
#[gpui::test]
fn folder_keys_work_on_home(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    let fs = FakeFs::new();
    fs.insert_many(r"C:\t", 1);
    fs.insert_many(r"C:\t\sub", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t\sub");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t\sub"));
    let home = Some(rust_i18n::t!("home.title").into_owned());
    let open_home = |cx: &mut VisualTestContext| {
        ws.update_in(cx, |ws, window, cx| {
            ws.open_home_for_test(false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), home);
    };

    open_home(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), None);
    assert_eq!(
        ws.update(cx, |ws, _| ws.tab_views().count()),
        1,
        "the tab stays"
    );
    view.update(cx, |v, _| assert_eq!(v.scope(), Scope::Computer));
    assert!(cx.update(|window, cx| view.read(cx).filter_focused_for_test(window, cx)));

    cx.simulate_keystrokes("escape");
    open_home(cx);
    cx.simulate_keystrokes("alt-up");
    wait_until(cx, &view, "up", loaded(r"C:\t"));
    assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), None);

    // The other pages over a tab do the same. Settings opens in a tab of its own: Ctrl+K closes
    // that tab and searches from the folder tab it was opened from.
    cx.simulate_keystrokes("escape");
    ws.update_in(cx, crate::workspace::Workspace::open_settings_for_test);
    cx.run_until_parked();
    assert!(ws.update(cx, |ws, _| ws.shown_page_for_test()).is_some());
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);
    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), None);
    assert_eq!(
        ws.update(cx, |ws, _| ws.tab_views().count()),
        1,
        "no stray tab"
    );
    let shown = ws.update(cx, |ws, _| ws.active_view().clone());
    assert_eq!(shown.entity_id(), view.entity_id());
    shown.update(cx, |v, _| assert_eq!(v.scope(), Scope::Computer));
}

/// Enter on a folder found from the search box opens it with the cursor in the list, as a click
/// does, not in the search box with its options open (#76).
#[gpui::test]
fn enter_on_a_found_folder_moves_to_the_list(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("needle"), file("other.txt", 1)]);
    fs.insert_many(r"C:\t\needle", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.simulate_keystrokes("ctrl-f");
    cx.simulate_input("needle");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "open", loaded(r"C:\t\needle"));
    assert!(!cx.update(|window, cx| view.read(cx).filter_focused_for_test(window, cx)));
}

/// #98: Home (or Settings) over a folder is where the user is, so the sidebar doesn't also mark
/// the folder under it.
#[gpui::test]
fn a_page_over_a_folder_does_not_mark_the_folder_in_the_sidebar(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\Users\me\Downloads", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\Users\me\Downloads");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "downloads", loaded(r"C:\Users\me\Downloads"));
    let here = |cx: &mut VisualTestContext| ws.update(cx, |ws, cx| ws.sidebar_here_for_test(cx));
    assert_eq!(here(cx), Some(p(r"C:\Users\me\Downloads")));

    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_home_for_test(false, window, cx)));
    assert_eq!(here(cx), None, "Home covers Downloads");
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_settings_for_test(window, cx)));
    assert_eq!(here(cx), None, "Settings covers Downloads");
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.close_settings_for_test(window, cx)));
    let home = t!("home.title").into_owned();
    assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), Some(home));
    assert_eq!(here(cx), None, "back on Home");
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.close_home_for_test(window, cx)));
    assert_eq!(
        here(cx),
        Some(p(r"C:\Users\me\Downloads")),
        "the folder is marked again once nothing covers it"
    );

    // Clicking the folder under a page in the sidebar shows it: the page gives way, the tab
    // stays.
    for settings in [false, true] {
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                if settings {
                    ws.open_settings_for_test(window, cx);
                } else {
                    ws.open_home_for_test(false, window, cx);
                }
            });
        });
        assert!(ws.update(cx, |ws, _| ws.shown_page_for_test()).is_some());
        let tabs = ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx));
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.sidebar_open_for_test(p(r"C:\Users\me\Downloads"), window, cx);
            });
        });
        wait_until(
            cx,
            &view,
            "downloads again",
            loaded(r"C:\Users\me\Downloads"),
        );
        assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), None);
        assert_eq!(here(cx), Some(p(r"C:\Users\me\Downloads")));
        assert_eq!(
            ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx)).len(),
            tabs.len(),
            "the tab stays"
        );
    }
}

/// A page that opened its own tab (Settings) closes back to the tab it was opened from, not to
/// whichever tab is next to it.
#[gpui::test]
fn closing_a_page_tab_returns_to_the_tab_it_came_from(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "a", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b"]);
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));

    // Settings opens right after a, before b.
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_settings_for_test(window, cx)));
    assert_eq!(active(&ws, cx), 1);
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.close_settings_for_test(window, cx)));
    assert_eq!(active(&ws, cx), 0, "back on a, not b");
    assert_eq!(ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx)).len(), 2);

    // The tab it came from was closed meanwhile: the next tab shows, as before.
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_settings_for_test(window, cx)));
    ws.update_in(cx, |ws, window, cx| ws.close_tab(0, window, cx));
    let settings_tab = active(&ws, cx);
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.close_settings_for_test(window, cx)));
    assert_eq!(ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx)), ["b"]);
    assert_eq!(active(&ws, cx), 0, "settings was tab {settings_tab}");
}

/// #116: a window opened with no path after the settings were already read (a resident Arke, or
/// the read finishing before the window listens for it) still goes where Startup says, without
/// waiting for some later settings change to notice.
#[gpui::test]
fn a_new_window_follows_startup_when_settings_are_already_read(cx: &mut TestAppContext) {
    use crate::settings::{SettingsStore, StartLocation};
    let fs = FakeFs::new();
    for dir in [r"C:\Users\me", r"C:\last"] {
        fs.insert_many(dir, 1);
    }
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
    });
    let home = t!("home.title").into_owned();
    for (start, expect_page, expect_folder) in [
        (StartLocation::HomePage, Some(home.clone()), r"C:\Users\me"),
        (StartLocation::LastFolder, None, r"C:\last"),
        // Last tabs, with other windows open: Home, not the tabs again or the profile (#178).
        (StartLocation::LastSession, Some(home), r"C:\Users\me"),
    ] {
        cx.update(|cx| {
            SettingsStore::global(cx).update(cx, |s, cx| {
                s.settings.start_location = start;
                s.last_session_folder = Some(p(r"C:\last"));
                s.mark_loaded(cx);
            });
        });
        let services = Services {
            fs: Arc::clone(&fs) as Arc<dyn arke_core::fs::Fs>,
            shell: Arc::new(FakeShell::default()),
            remotes: crate::remote_shell::test_remotes(),
        };
        let (ws, vcx) = cx.add_window_view(move |window, cx| {
            Workspace::new(
                services,
                p(r"C:\Users\me"),
                true,
                None,
                None,
                Instant::now(),
                window,
                cx,
            )
        });
        vcx.run_until_parked();
        let view = ws.update(vcx, |ws, _| ws.active_view().clone());
        wait_until(vcx, &view, expect_folder, loaded(expect_folder));
        assert_eq!(
            ws.update(vcx, |ws, _| ws.shown_page_for_test()),
            expect_page,
            "{start:?}"
        );
    }
}

/// Ctrl+B collapses the sidebar to a column of icons and back (#91); the setting keeps it.
#[gpui::test]
fn ctrl_b_collapses_the_sidebar(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    assert!(cx.debug_bounds("sidebar-rail").is_none());

    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert!(settings(cx).sidebar_collapsed);
    assert_eq!(crate::theme::sidebar_width(), crate::theme::rail_w());
    assert!(
        cx.debug_bounds("sidebar-rail").is_some(),
        "the column of icons shows"
    );

    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert!(!settings(cx).sidebar_collapsed);
    assert_eq!(
        crate::theme::sidebar_width(),
        crate::theme::sidebar_full_width()
    );
    assert!(cx.debug_bounds("sidebar-rail").is_none());
}

/// Ctrl+B slides the sidebar's width instead of switching at once. While it slides, the full
/// sidebar and the column of icons crossfade (their icons sit in different places, so swapping
/// at the end would jump); afterwards only one is drawn.
#[gpui::test]
fn ctrl_b_slides_the_sidebar(cx: &mut TestAppContext) {
    crate::motion::force_for_test(true);
    let fs = FakeFs::new();
    fs.insert_many(r"C:", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:"));
    let both = |cx: &mut VisualTestContext| {
        (
            cx.debug_bounds("sidebar-slide-full").is_some(),
            cx.debug_bounds("sidebar-rail").is_some(),
        )
    };

    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert!(settings(cx).sidebar_collapsed);
    let sliding = cx.debug_bounds("sidebar-column").unwrap();
    assert!(
        sliding.size.width > crate::theme::rail_w(),
        "still wider than the column of icons: {sliding:?}"
    );
    assert_eq!(both(cx), (true, true), "crossfading");
    cx.executor()
        .advance_clock(crate::motion::BASE + Duration::from_millis(60));
    cx.run_until_parked();
    assert_eq!(both(cx), (false, true), "slide over");
    assert_eq!(
        cx.debug_bounds("sidebar-column").unwrap().size.width,
        crate::theme::rail_w()
    );

    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert_eq!(both(cx), (true, true), "crossfading back");
    cx.executor()
        .advance_clock(crate::motion::BASE + Duration::from_millis(60));
    cx.run_until_parked();
    assert_eq!(both(cx), (false, false), "full sidebar only");
    assert_eq!(
        cx.debug_bounds("sidebar-column").unwrap().size.width,
        crate::theme::sidebar_full_width()
    );
    crate::motion::force_for_test(false);
}

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings) + 'static) {
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx));
    });
    cx.run_until_parked();
}

/// #122: collapsed, the column also has the pinned folders, the Recycle Bin, the Shelf and the
/// drives (as their letters).
#[gpui::test]
fn the_collapsed_sidebar_has_pins_and_drives(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    fs.insert_many(r"C:\work", 1);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(crate::shell::Place {
        label: "Local Disk (C:)".into(),
        path: p(r"C:\"),
        kind: crate::shell::PlaceKind::Drive {
            remote: false,
            space: None,
        },
    });
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    change(cx, |st| {
        st.pinned = vec![p(r"C:\work")];
        st.recycle_pinned = true;
        st.sidebar_collapsed = true;
    });
    // debug_bounds takes static names.
    let wanted: Vec<&'static str> = [
        "rail work".to_owned(),
        format!("rail {}", t!("recycle.title")),
        format!("rail {}", t!("shelf.title")),
        "rail Local Disk (C:)".to_owned(),
    ]
    .into_iter()
    .map(|s| &*Box::leak(s.into_boxed_str()))
    .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while wanted.iter().any(|s| cx.debug_bounds(s).is_none()) {
        assert!(Instant::now() < deadline, "missing in the rail");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    // The pinned folder opens here.
    let at = cx.debug_bounds("rail work").unwrap().center();
    cx.simulate_click(at, gpui::Modifiers::none());
    wait_until(cx, &view, "work", loaded(r"C:\work"));
}

/// #122: resting the pointer on the collapsed column opens the full sidebar over the content; it
/// closes once the pointer moves past it, and a pass straight across opens nothing. Right click
/// turns it off.
#[gpui::test]
fn the_collapsed_sidebar_opens_while_the_pointer_rests_on_it(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    change(cx, |st| st.sidebar_collapsed = true);
    let peek = |cx: &mut VisualTestContext| ws.update(cx, |ws, _| ws.peek_for_test());
    let rail = cx.debug_bounds("sidebar-rail").unwrap().center();
    let content = gpui::point(crate::theme::sidebar_full_width() + gpui::px(200.), rail.y);
    let none = gpui::Modifiers::none();

    // Crossing the column without resting opens nothing.
    cx.simulate_mouse_move(rail, None, none);
    cx.executor().advance_clock(Duration::from_millis(100));
    cx.simulate_mouse_move(content, None, none);
    cx.executor().advance_clock(Duration::from_millis(500));
    cx.run_until_parked();
    assert!(!peek(cx));

    // Resting opens it over the content.
    cx.simulate_mouse_move(rail, None, none);
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    assert!(peek(cx));
    assert!(cx.debug_bounds("sidebar-peek").is_some());
    assert!(settings(cx).sidebar_collapsed, "only shown, not expanded");

    // Moving within it keeps it; leaving closes it.
    cx.simulate_mouse_move(
        gpui::point(crate::theme::sidebar_full_width() - gpui::px(20.), rail.y),
        None,
        none,
    );
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    assert!(peek(cx));
    // Out and straight back over it: stays open.
    cx.simulate_mouse_move(content, None, none);
    cx.executor().advance_clock(Duration::from_millis(100));
    cx.simulate_mouse_move(
        gpui::point(crate::theme::sidebar_full_width() - gpui::px(20.), rail.y),
        None,
        none,
    );
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    assert!(peek(cx), "came back before it closed");
    cx.simulate_mouse_move(content, None, none);
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    assert!(!peek(cx));
    assert!(cx.debug_bounds("sidebar-peek").is_none());

    // Off: resting does nothing.
    change(cx, |st| st.sidebar_peek = false);
    cx.simulate_mouse_move(rail, None, none);
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    assert!(!peek(cx));
}

/// A tab in the background keeps following its folder (its watcher sleeps until a change instead
/// of polling): switching back shows what changed meanwhile, without a relist.
#[gpui::test]
fn background_tab_is_current_when_switched_back(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 2);
    fs.insert_many(r"C:\b", 1);
    let (ws, cx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b"]);
    assert_eq!(active(&ws, cx), 1);
    let a = Path::new(r"C:\a");
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs.watcher_count(a) == 0 {
        assert!(Instant::now() < deadline, "watcher did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let reads = fs.read_dir_calls();
    fs.add_file(a, "while-away.txt", 1);
    fs.remove(a, "file_000000.txt");
    fs.emit_changed(a, &["while-away.txt", "file_000000.txt"]);
    wait_until(cx, &first, "background tab updated", |v| {
        names(v) == ["file_000001.txt", "while-away.txt"]
    });
    cx.simulate_keystrokes("ctrl-1");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 0);
    assert_eq!(
        first.update(cx, |v, _| names(v)),
        ["file_000001.txt", "while-away.txt"]
    );
    assert_eq!(fs.read_dir_calls(), reads, "no relist needed");
}

/// A sidebar with eight drives and `tabs` tabs, in a window `height` tall.
fn sidebar_with_tabs(
    cx: &mut TestAppContext,
    tabs: usize,
    height: f32,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    let paths: Vec<String> = (0..tabs).map(|i| format!(r"C:\t{i}")).collect();
    for path in &paths {
        fs.insert_many(path, 1);
    }
    let shell = Arc::new(FakeShell::default());
    shell
        .extra_places
        .lock()
        .extend((b'D'..=b'K').map(|letter| crate::shell::Place {
            label: format!("Disk ({}:)", letter as char),
            path: PathBuf::from(format!(r"{}:\", letter as char)),
            kind: crate::shell::PlaceKind::Drive {
                remote: false,
                space: None,
            },
        }));
    let (ws, cx) = setup_workspace_with(cx, fs, shell, &paths[0]);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(&paths[0]));
    let rest: Vec<&str> = paths[1..].iter().map(String::as_str).collect();
    open_tabs(&ws, cx, &rest);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(height)));
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
    (ws, cx)
}

/// Shown in the vertical tab list: tab `ix` is drawn and its bottom inside the list.
fn tab_shown(cx: &mut VisualTestContext, ix: usize) -> bool {
    let id: &'static str = Box::leak(format!("tab-{ix}").into_boxed_str());
    let list = cx.debug_bounds("sidebar-tabs-frame").unwrap();
    cx.debug_bounds(id)
        .is_some_and(|b| b.top() >= list.top() && b.bottom() <= list.bottom() + gpui::px(1.))
}

/// Many tabs don't squeeze the places (This PC, drives) above them: the tab list gives way
/// first and scrolls.
#[gpui::test]
fn many_tabs_leave_the_places_alone(cx: &mut TestAppContext) {
    let (ws, cx) = sidebar_with_tabs(cx, 20, 1000.);
    // Less than a pixel: rounding, not squeezed.
    assert!(ws.update(cx, |ws, _| ws.places_overflow_for_test()) < gpui::px(1.));
    assert!(tab_shown(cx, 0) && tab_shown(cx, 2), "the first tabs show");
    assert!(!tab_shown(cx, 19), "the rest scroll");
}

/// A short window: the tabs give way down to three, then the places shrink too and scroll.
#[gpui::test]
fn a_short_window_keeps_three_tabs_then_shrinks_the_places(cx: &mut TestAppContext) {
    let (ws, cx) = sidebar_with_tabs(cx, 20, 520.);
    assert!(ws.update(cx, |ws, _| ws.places_overflow_for_test()) > gpui::px(0.));
    for ix in 0..3 {
        assert!(tab_shown(cx, ix), "tab {ix} shows");
    }
}

/// Few tabs: nothing scrolls and nothing is cut.
#[gpui::test]
fn few_tabs_and_room_to_spare_scroll_nothing(cx: &mut TestAppContext) {
    let (ws, cx) = sidebar_with_tabs(cx, 3, 1000.);
    // Less than a pixel: rounding, not squeezed.
    assert!(ws.update(cx, |ws, _| ws.places_overflow_for_test()) < gpui::px(1.));
    for ix in 0..3 {
        assert!(tab_shown(cx, ix), "tab {ix} shows");
    }
}

/// A folder whose name alone is wider than the path bar (a narrow, not maximized window):
/// the name truncates inside the bar instead of running over the title bar next to it, with
/// tabs on the left or on top.
#[gpui::test]
fn a_very_long_folder_name_stays_inside_the_path_bar(cx: &mut TestAppContext) {
    let long = format!(
        r"C:\top1\{}",
        // Wide characters too (written escaped: no UI text in source).
        "[ABC-41][2015] Vol.41 \u{5929}\u{4f7f}\u{3068}\u{60aa}\u{9b54} a very long folder name "
            .repeat(4)
    );
    let fs = FakeFs::new();
    fs.insert_many(&long, 1);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), &long);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(&long));
    for horizontal in [false, true] {
        if horizontal {
            cx.update(|_, cx| {
                crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(
                        |st| st.tab_layout = crate::settings::TabLayout::Horizontal,
                        cx,
                    );
                });
            });
        }
        cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(600.)));
        for _ in 0..3 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }
        let bar = cx.debug_bounds("path-bar").expect("path bar drawn");
        let current = cx.debug_bounds("crumb-2").expect("current folder drawn");
        assert!(
            current.right() <= bar.right() + gpui::px(0.5),
            "horizontal tabs {horizontal}: current folder ends at {:?}, past the bar at {:?}",
            current.right(),
            bar.right()
        );
        assert!(
            bar.right() <= gpui::px(900.),
            "horizontal tabs {horizontal}: the bar stays in the window"
        );
    }
}

/// Opens the back (or forward) button's menu on the active pane, the way its right click does.
fn open_history_menu(
    ws: &Entity<Workspace>,
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    back: bool,
) -> Option<Vec<String>> {
    ws.update(cx, |ws, cx| {
        ws.open_history_menu_for_test(view, back, gpui::point(gpui::px(100.), gpui::px(40.)), cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, _| ws.history_menu_labels_for_test())
}

fn pick_menu_row(ws: &Entity<Workspace>, cx: &mut VisualTestContext, ix: usize) {
    ws.update_in(cx, |ws, window, cx| {
        ws.pick_menu_row_for_test(ix, window, cx);
    });
    cx.run_until_parked();
}

/// #152: right click on Back lists the folders behind, nearest first; choosing one goes there at
/// once and Forward then lists the ones passed over, like repeated Back.
#[gpui::test]
fn history_menu_jumps_back_and_keeps_forward_consistent(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in ["a", "b", "c", "d", "e"] {
        fs.insert_many(format!(r"C:\{dir}"), 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    for dir in ["b", "c", "d", "e"] {
        let path = format!(r"C:\{dir}");
        view.update(cx, |e, cx| e.navigate(p(&path), NavKind::New, cx));
        wait_until(cx, &view, &path, loaded(&path));
    }
    let at = |cx: &mut VisualTestContext| {
        view.update(cx, |e, _| e.current_path().map(Path::to_path_buf))
    };

    assert_eq!(
        open_history_menu(&ws, &view, cx, true),
        Some(vec!["d".into(), "c".into(), "b".into(), "a".into()])
    );
    assert_eq!(open_history_menu(&ws, &view, cx, false), None);

    // Third entry: b, in one step.
    assert_eq!(
        open_history_menu(&ws, &view, cx, true).map(|m| m[2].clone()),
        Some("b".into())
    );
    pick_menu_row(&ws, cx, 2);
    wait_until(cx, &view, "b", loaded(r"C:\b"));
    assert_eq!(at(cx), Some(p(r"C:\b")));
    assert_eq!(
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test()),
        None,
        "choosing closes the menu"
    );
    assert_eq!(
        open_history_menu(&ws, &view, cx, true),
        Some(vec!["a".into()])
    );
    assert_eq!(
        open_history_menu(&ws, &view, cx, false),
        Some(vec!["c".into(), "d".into(), "e".into()]),
        "the folders passed over wait on the forward side"
    );

    // Back and Forward carry on from there (the timeline agrees).
    ws.update_in(cx, |ws, window, cx| ws.go_in_history(true, window, cx));
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    ws.update_in(cx, |ws, window, cx| ws.go_in_history(false, window, cx));
    wait_until(cx, &view, "b again", loaded(r"C:\b"));
    ws.update_in(cx, |ws, window, cx| ws.go_in_history(false, window, cx));
    wait_until(cx, &view, "c", loaded(r"C:\c"));

    // Forward menu: jump to the last entry.
    assert_eq!(
        open_history_menu(&ws, &view, cx, false),
        Some(vec!["d".into(), "e".into()])
    );
    pick_menu_row(&ws, cx, 1);
    wait_until(cx, &view, "e", loaded(r"C:\e"));
    assert_eq!(
        open_history_menu(&ws, &view, cx, true),
        Some(vec!["d".into(), "c".into(), "b".into(), "a".into()])
    );
    assert_eq!(open_history_menu(&ws, &view, cx, false), None);
    ws.update_in(cx, |ws, window, cx| ws.go_in_history(true, window, cx));
    wait_until(cx, &view, "d", loaded(r"C:\d"));
}

/// #152: nothing to list, no menu; and a long history lists only the nearest 15.
#[gpui::test]
fn history_menu_is_empty_without_history_and_capped(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for n in 0..20 {
        fs.insert_many(format!(r"C:\d{n}"), 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\d0");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "d0", loaded(r"C:\d0"));
    assert_eq!(open_history_menu(&ws, &view, cx, true), None);
    assert_eq!(open_history_menu(&ws, &view, cx, false), None);

    for n in 1..20 {
        let path = format!(r"C:\d{n}");
        view.update(cx, |e, cx| e.navigate(p(&path), NavKind::New, cx));
        wait_until(cx, &view, &path, loaded(&path));
    }
    let hidden = |cx: &mut VisualTestContext| {
        ws.update_in(cx, |_, window, _| crate::workspace::tooltips_hidden(window))
    };
    assert!(!hidden(cx));
    let menu = open_history_menu(&ws, &view, cx, true).expect("a menu");
    assert_eq!(menu.len(), 15);
    assert_eq!(menu[0], "d18");
    assert_eq!(menu[14], "d4");
    // The button's tooltip doesn't show over the menu its right click opened.
    assert!(hidden(cx));
}

// --- Tab behaviors: pinned tabs keep their folder, one tab per folder, Ctrl+T (#90) ---

fn tab_count(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.update(cx, |ws, _| ws.tab_views().count())
}

#[gpui::test]
fn a_pinned_tab_opens_other_folders_in_a_new_tab(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("sub")]);
    fs.insert_many(r"C:\p\sub", 2);
    fs.insert_many(r"C:\other", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\other");
    let pinned = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &pinned, "load", loaded(r"C:\other"));
    pinned.update(cx, |e, cx| e.navigate(p(r"C:\p"), NavKind::New, cx));
    wait_until(cx, &pinned, "p", loaded(r"C:\p"));
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();

    // Enter on a subfolder: it opens in a new tab beside the pinned one, which stays.
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    let sub = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &sub, "sub", loaded(r"C:\p\sub"));
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\p")), Some(p(r"C:\p\sub"))]);
        assert_eq!(ws.pinned_for_test(), [true, false]);
        assert_eq!(ws.active, 1);
    });

    // Back from there returns to the pinned tab; its own history stays put (no tab per step).
    cx.simulate_keystrokes("alt-left");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 0);
    assert!(
        !pinned.update(cx, |e, _| e.can_go(true)),
        "back is off in a pinned tab"
    );
    assert_eq!(
        open_history_menu(&ws, &pinned, cx, true),
        None,
        "and so is its history menu"
    );
    cx.simulate_keystrokes("alt-left");
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 2);
    assert_eq!(
        pinned.update(cx, |e, _| e.current_path().map(Path::to_path_buf)),
        Some(p(r"C:\p"))
    );

    // The sidebar too: a new tab the first time, the tab already on it the second.
    ws.update_in(cx, |ws, window, cx| {
        ws.sidebar_open_for_test(p(r"C:\other"), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 3);
    ws.update_in(cx, |ws, window, cx| {
        ws.activate(0, window, cx);
        ws.sidebar_open_for_test(p(r"C:\Other\"), window, cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx).len(), 3);
        assert_eq!(tab_paths(ws, cx)[ws.active], Some(p(r"C:\other")));
        assert_eq!(
            tab_paths(ws, cx)[0],
            Some(p(r"C:\p")),
            "the pinned tab stayed"
        );
    });

    // Unpinned, the tab browses again.
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    pinned.update(cx, |e, cx| e.navigate(p(r"C:\p\sub"), NavKind::New, cx));
    wait_until(cx, &pinned, "sub in place", loaded(r"C:\p\sub"));
    assert_eq!(tab_count(&ws, cx), 3);
}

#[gpui::test]
fn opening_a_folder_already_in_a_tab_switches_to_it(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    fs.insert_many(r"C:\b", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b"]);

    // Open in new tab from the list, in another spelling: the tab on b comes forward.
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    first.update(cx, |_, cx| {
        cx.emit(ExplorerEvent::OpenInNewTab {
            path: p(r"c:\B\"),
            activate: true,
        });
    });
    cx.run_until_parked();
    assert_eq!((tab_count(&ws, cx), active(&ws, cx)), (2, 1));

    // In the background (middle click) it's left where it is.
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    first.update(cx, |_, cx| {
        cx.emit(ExplorerEvent::OpenInNewTab {
            path: p(r"C:\b"),
            activate: false,
        });
    });
    cx.run_until_parked();
    assert_eq!((tab_count(&ws, cx), active(&ws, cx)), (2, 0));

    // The sidebar's Open in new tab, and the Go to folder box.
    ws.update_in(cx, |ws, window, cx| {
        ws.sidebar_open_in_new_tab_for_test(p(r"C:\b"), window, cx);
    });
    cx.run_until_parked();
    assert_eq!((tab_count(&ws, cx), active(&ws, cx)), (2, 1));
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    cx.simulate_input(r"C:\b");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!((tab_count(&ws, cx), active(&ws, cx)), (2, 1));

    // Home in a tab of its own: a folder already open elsewhere closes it and switches there.
    ws.update_in(cx, |ws, window, cx| ws.open_home_for_test(true, window, cx));
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 3);
    ws.update_in(cx, |ws, window, cx| {
        ws.home_open_folder_for_test(p(r"C:\a"), window, cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\a")), Some(p(r"C:\b"))]);
        assert_eq!(ws.active, 0);
        assert_eq!(ws.shown_page_for_test(), None);
    });

    // Duplicate tab still duplicates.
    ws.update_in(cx, |ws, window, cx| {
        let label = t!("sidebar.duplicate_tab").into_owned();
        ws.run_tab_menu_for_test(1, &label, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 3);

    // Turned off, a second tab opens.
    change(cx, |s| s.one_tab_per_folder = false);
    first.update(cx, |_, cx| {
        cx.emit(ExplorerEvent::OpenInNewTab {
            path: p(r"C:\b"),
            activate: true,
        });
    });
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 4);
}

#[gpui::test]
fn the_tab_menu_adds_its_folder_to_the_favorites(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\work", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\work");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\work"));
    let pin = t!("sidebar.add_favorite").into_owned();
    let unpin = t!("sidebar.remove_favorite").into_owned();
    let on_sidebar = |cx: &mut VisualTestContext| {
        ws.update(cx, |ws, cx| {
            let places = ws.active_view().read(cx).places.clone();
            crate::settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .is_pinned(Path::new(r"C:\work"), &places)
        })
    };
    assert!(!on_sidebar(cx));
    assert!(ws.update(cx, |ws, cx| {
        ws.tab_menu_labels_for_test(0, cx).contains(&pin)
    }));
    ws.update_in(cx, |ws, window, cx| {
        ws.run_tab_menu_for_test(0, &pin, window, cx);
    });
    cx.run_until_parked();
    assert!(on_sidebar(cx));
    ws.update(cx, |ws, cx| {
        let labels = ws.tab_menu_labels_for_test(0, cx);
        assert!(labels.contains(&unpin) && !labels.contains(&pin));
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.run_tab_menu_for_test(0, &unpin, window, cx);
    });
    cx.run_until_parked();
    assert!(!on_sidebar(cx));

    // A page has no folder to pin.
    ws.update_in(cx, |ws, window, cx| ws.open_home_for_test(true, window, cx));
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        let labels = ws.tab_menu_labels_for_test(1, cx);
        assert!(!labels.contains(&pin) && !labels.contains(&unpin));
    });
}

#[gpui::test]
fn ctrl_t_does_what_settings_say(cx: &mut TestAppContext) {
    use crate::settings::NewTabAction;
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));

    // Default: the Go to folder box.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    assert!(ws.update(cx, |ws, _| ws.palette.is_some()));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 1);

    // A new tab on the current folder, straight away.
    change(cx, |s| s.new_tab = NewTabAction::CurrentFolder);
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert!(ws.palette.is_none());
        assert_eq!(tab_paths(ws, cx), [Some(p(r"C:\a")), Some(p(r"C:\a"))]);
        assert_eq!(ws.active, 1);
    });

    // A new tab with Home.
    change(cx, |s| s.new_tab = NewTabAction::Home);
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    let home = t!("home.title").into_owned();
    ws.update(cx, |ws, _| {
        assert!(ws.palette.is_none());
        assert_eq!(ws.tab_views().count(), 3);
        assert_eq!(ws.active, 2);
        assert_eq!(ws.shown_page_for_test(), Some(home));
    });
}

/// The window's title follows the active tab, also back to a tab that had the title before
/// (each pane used to remember what it last set, so switching back left the other tab's name).
#[gpui::test]
fn window_title_follows_the_active_tab(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in ["alpha", "beta"] {
        fs.insert_many(format!(r"C:\{dir}"), 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\alpha");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "alpha", loaded(r"C:\alpha"));
    cx.run_until_parked();
    assert_eq!(cx.window_title().as_deref(), Some("alpha - Arke"));
    open_tabs(&ws, cx, &[r"C:\beta"]);
    cx.run_until_parked();
    assert_eq!(cx.window_title().as_deref(), Some("beta - Arke"));
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    cx.run_until_parked();
    assert_eq!(cx.window_title().as_deref(), Some("alpha - Arke"));
}

/// In a split tab the title follows the pane in use; a page names the window after the page.
#[gpui::test]
fn window_title_follows_the_active_pane_and_names_pages(cx: &mut TestAppContext) {
    use arke_core::takeover::Target;
    let fs = FakeFs::new();
    for dir in ["alpha", "beta"] {
        fs.insert_many(format!(r"C:\{dir}"), 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\alpha");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "alpha", loaded(r"C:\alpha"));
    let title = |cx: &mut VisualTestContext| cx.window_title().unwrap_or_default();
    let page = |key: &str| format!("{} - Arke", t!(key));

    let (left, right) = split_to(&ws, cx, r"C:\beta");
    right.update(cx, |_, cx| cx.emit(ExplorerEvent::Activated));
    cx.run_until_parked();
    assert_eq!(title(cx), "beta - Arke");
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::Activated));
    cx.run_until_parked();
    assert_eq!(title(cx), "alpha - Arke");

    ws.update_in(cx, |ws, window, cx| {
        ws.open_external(Target::RecycleBin, Vec::new(), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(title(cx), page("recycle.title"));
    ws.update_in(cx, |ws, window, cx| {
        ws.open_external(Target::Home, Vec::new(), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(title(cx), page("home.title"));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    assert_eq!(title(cx), page("settings.title"));
    // Back to the split tab.
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    cx.run_until_parked();
    assert_eq!(title(cx), "alpha - Arke");
}

// --- History menus that go stale, the timeline, pinned tabs (#152) ---

fn at(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Option<PathBuf> {
    view.update(cx, |e, _| e.current_path().map(Path::to_path_buf))
}

fn go(view: &Entity<Explorer>, cx: &mut VisualTestContext, path: &str) {
    view.update(cx, |e, cx| e.navigate(p(path), NavKind::New, cx));
    wait_until(cx, view, path, loaded(path));
}

fn folders(paths: &[&str]) -> Arc<FakeFs> {
    let fs = FakeFs::new();
    for path in paths {
        fs.insert_many(*path, 1);
    }
    fs
}

/// A row chosen after the pane's history moved under the open menu (a slow folder finished
/// loading) still goes to the folder it named; one whose folder left that side does nothing.
/// Rows tell two folders of the same name apart by where they are.
#[gpui::test]
fn a_history_menu_row_goes_to_its_folder_after_the_history_moved(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\one\src", r"C:\two\src", r"C:\b", r"C:\c", r"C:\e"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\one\src");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "src", loaded(r"C:\one\src"));
    for path in [r"C:\two\src", r"C:\b", r"C:\c"] {
        go(&view, cx, path);
    }
    assert_eq!(
        open_history_menu(&ws, &view, cx, true),
        Some(vec!["b".into(), "src".into(), "src".into()])
    );
    assert_eq!(
        ws.update(cx, |ws, _| ws.menu_details_for_test()),
        Some(vec![r"C:\".into(), r"C:\two".into(), r"C:\one".into()])
    );
    // C:\two\src, two steps back when the menu opened.
    let row = ws.update(cx, |ws, _| ws.menu_row_for_test(1)).expect("row");

    // Meanwhile the pane gets somewhere (a folder that was loading): its menu closes.
    go(&view, cx, r"C:\e");
    assert_eq!(
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test()),
        None
    );
    ws.update_in(cx, |ws, window, cx| {
        ws.run_menu_row_for_test(row, window, cx);
    });
    wait_until(cx, &view, r"C:\two\src", loaded(r"C:\two\src"));
    assert_eq!(
        open_history_menu(&ws, &view, cx, false),
        Some(vec!["b".into(), "c".into(), "e".into()])
    );

    // A forward row whose folder is gone from the forward side by the time it is chosen.
    let row = ws.update(cx, |ws, _| ws.menu_row_for_test(2)).expect("row");
    go(&view, cx, r"C:\b");
    ws.update_in(cx, |ws, window, cx| {
        ws.run_menu_row_for_test(row, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(at(&view, cx), Some(p(r"C:\b")));
}

/// A jump to a folder that can't be opened (deleted, offline) leaves the timeline where it was:
/// Back then still goes back in this tab instead of skipping to the other one.
#[gpui::test]
fn a_history_jump_that_fails_leaves_the_timeline_alone(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\a", r"C:\b", r"C:\c", r"C:\d", r"C:\e"]);
    let (ws, cx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let (_, b) = two_tabs_with_history(&ws, cx);
    go(&b, cx, r"C:\e");
    assert_eq!(
        open_history_menu(&ws, &b, cx, true),
        Some(vec!["d".into(), "c".into()])
    );
    fs.remove_dir(Path::new(r"C:\c"));
    pick_menu_row(&ws, cx, 1);
    wait_until(cx, &b, "the error", |e| e.error.is_some());
    assert_eq!(at(&b, cx), Some(p(r"C:\e")));

    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &b, "back in this tab", loaded(r"C:\d"));
    assert_eq!(active(&ws, cx), 1);
}

/// Back and forward across two tabs with a history jump in between: the jump takes the steps it
/// passed over, and the round trip comes back the way it went. With back and forward across
/// tabs, the menu's first row is where the button goes.
#[gpui::test]
fn history_jumps_and_the_timeline_round_trip_across_tabs(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\a", r"C:\b", r"C:\c", r"C:\d", r"C:\e", r"C:\f"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let (a, b) = two_tabs_with_history(&ws, cx);
    go(&b, cx, r"C:\e");
    cx.simulate_keystrokes("ctrl-1");
    cx.run_until_parked();
    go(&a, cx, r"C:\f");
    cx.simulate_keystrokes("ctrl-2");
    cx.run_until_parked();
    assert_eq!(active(&ws, cx), 1);

    // Back would first return to tab A (on f): that row comes first, then B's own folders.
    assert_eq!(
        open_history_menu(&ws, &b, cx, true),
        Some(vec!["f".into(), "d".into(), "c".into()])
    );
    pick_menu_row(&ws, cx, 2);
    wait_until(cx, &b, "B jumps to c", loaded(r"C:\c"));

    let here = |cx: &mut VisualTestContext| {
        let view = ws.update(cx, |ws, _| ws.active_view().clone());
        wait_until(cx, &view, "listed", |e| {
            e.model.as_ref().is_some_and(|m| m.complete) && e.display_path() == e.current_path()
        });
        (active(&ws, cx), at(&view, cx).expect("a folder"))
    };
    let mut trip = Vec::new();
    for keys in ["alt-left"; 5].into_iter().chain(["alt-right"; 7]) {
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
        trip.push(here(cx));
    }
    assert_eq!(
        trip,
        [
            // Back: tab A as it was left, A's own step, B where the jump left it, then A.
            (0, p(r"C:\f")),
            (0, p(r"C:\b")),
            (1, p(r"C:\c")),
            (0, p(r"C:\b")),
            (0, p(r"C:\a")),
            // Forward retraces it, then redoes the folders the jump passed over.
            (0, p(r"C:\b")),
            (1, p(r"C:\c")),
            (0, p(r"C:\b")),
            (0, p(r"C:\f")),
            (1, p(r"C:\c")),
            (1, p(r"C:\d")),
            (1, p(r"C:\e")),
        ]
    );

    // Back over the folders the jump passed: then the first row is what the button does.
    for _ in 0..2 {
        cx.simulate_keystrokes("alt-left");
        cx.run_until_parked();
        here(cx);
    }
    assert_eq!(here(cx), (1, p(r"C:\c")));
    assert_eq!(
        open_history_menu(&ws, &b, cx, true).map(|m| m[0].clone()),
        Some("f".into())
    );
    pick_menu_row(&ws, cx, 0);
    assert_eq!(here(cx), (0, p(r"C:\f")));
}

/// A history menu on the side of a split tab that isn't active: the jump happens there and
/// that side becomes the active one.
#[gpui::test]
fn a_history_jump_on_the_other_side_of_a_split(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\a", r"C:\b", r"C:\c"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let left = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &left, "a", loaded(r"C:\a"));
    go(&left, cx, r"C:\b");
    let (left, right) = split_to(&ws, cx, r"C:\c");
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);
    let menu = open_history_menu(&ws, &left, cx, true).expect("a menu");
    assert_eq!(menu.last().map(String::as_str), Some("a"));
    pick_menu_row(&ws, cx, menu.len() - 1);
    wait_until(cx, &left, "left back to a", loaded(r"C:\a"));
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);
    assert_eq!(at(&right, cx), Some(p(r"C:\c")));

    // The keys go to the pane the jump focused; a busy machine may take a moment to get there.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cx.update(|window, cx| {
        gpui::Focusable::focus_handle(left.read(cx), cx).contains_focused(window, cx)
    }) {
        assert!(
            Instant::now() < deadline,
            "the jump left the focus on the pane"
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    // The steps stay in line: forward on the left comes back after the right's own step.
    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &right, "right back to b", loaded(r"C:\b"));
    cx.simulate_keystrokes("alt-right");
    wait_until(cx, &right, "right forward to c", loaded(r"C:\c"));
    cx.simulate_keystrokes("alt-right");
    wait_until(cx, &left, "left forward to b", loaded(r"C:\b"));
}

/// Jumping back past a folder opened from Home shows Home, as pressing Back that many times
/// does; with Home where the tab started, the folders under it aren't offered.
#[gpui::test]
fn a_history_jump_past_a_folder_opened_from_home_shows_home(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\a", r"C:\me", r"C:\me\Desktop", r"C:\x", r"C:\y"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    go(&view, cx, r"C:\me");
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_start_home_for_test(window, cx)));
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.home_open_folder_for_test(p(r"C:\me\Desktop"), window, cx);
        });
    });
    wait_until(cx, &view, "desktop", loaded(r"C:\me\Desktop"));
    go(&view, cx, r"C:\x");
    go(&view, cx, r"C:\y");
    assert_eq!(
        open_history_menu(&ws, &view, cx, true),
        Some(vec!["x".into(), "Desktop".into(), "me".into()])
    );
    pick_menu_row(&ws, cx, 2);
    wait_until(cx, &view, "me", loaded(r"C:\me"));
    assert_eq!(
        ws.update(cx, |ws, _| ws.shown_page_for_test()),
        Some(t!("home.title").into_owned())
    );
}

/// A key closes the open menu even with nothing focused (the key then never reaches the
/// workspace's elements).
#[gpui::test]
fn a_key_closes_the_menu_with_nothing_focused(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\a", r"C:\b"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    go(&view, cx, r"C:\b");
    assert!(open_history_menu(&ws, &view, cx, true).is_some());
    cx.update(gpui::Window::blur);
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    assert_eq!(
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test()),
        None
    );
}

/// Keys close the open menu (its rows name tabs and folders a key may move), and so does a tab
/// opening or closing behind it: the tab menu's Close then can't hit another tab.
#[gpui::test]
fn menus_close_on_keys_and_when_the_tabs_change(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\a", r"C:\b", r"C:\c"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    go(&view, cx, r"C:\b");
    assert!(open_history_menu(&ws, &view, cx, true).is_some());
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test()),
        None
    );

    open_tabs(&ws, cx, &[r"C:\c"]);
    // The tab menu of the second tab, then a tab closes before a row is chosen.
    ws.update(cx, |ws, cx| ws.open_tab_menu_for_test(1, cx));
    assert!(ws.update(cx, |ws, _| ws.menu_row_for_test(0)).is_some());
    ws.update_in(cx, |ws, window, cx| ws.close_tab(0, window, cx));
    cx.run_until_parked();
    assert_eq!(
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test()),
        None
    );
    assert!(
        ws.update(cx, |ws, _| ws.menu_row_for_test(0)).is_none(),
        "no stale row to choose"
    );
    assert_eq!(tab_count(&ws, cx), 1);
}

/// Ctrl+T set to Home opens a Home tab every time, also when Home is already open.
#[gpui::test]
fn ctrl_t_opens_home_again_and_again(cx: &mut TestAppContext) {
    use crate::settings::NewTabAction;
    let (ws, cx) = setup_workspace(cx, folders(&[r"C:\a"]), r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));
    change(cx, |s| s.new_tab = NewTabAction::Home);
    let home = t!("home.title").into_owned();
    for (tabs, active_tab) in [(2, 1), (3, 2)] {
        cx.simulate_keystrokes("ctrl-t");
        cx.run_until_parked();
        ws.update(cx, |ws, _| {
            assert_eq!((ws.tab_views().count(), ws.active), (tabs, active_tab));
            assert_eq!(ws.shown_page_for_test(), Some(home.clone()));
        });
    }
    // From another tab too: a new one, not a switch to the tab with Home.
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!((ws.tab_views().count(), ws.active), (4, 1));
        assert_eq!(ws.shown_page_for_test(), Some(home.clone()));
        assert_eq!(
            ws.tab_titles_for_test(cx)
                .iter()
                .filter(|t| **t == home)
                .count(),
            1,
            "one Home page; the others show their folder"
        );
    });
}

/// A pinned tab whose folder can't be shown (gone, unplugged) can be sent somewhere else.
#[gpui::test]
fn a_pinned_tab_on_a_missing_folder_can_be_pointed_elsewhere(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\other", r"C:\third"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\gone");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "error page", |e| e.failed.is_some());
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    ws.update_in(cx, |ws, window, cx| {
        ws.sidebar_open_for_test(p(r"C:\other"), window, cx);
    });
    wait_until(cx, &view, "in place", loaded(r"C:\other"));
    assert_eq!(tab_count(&ws, cx), 1);
    assert_eq!(ws.update(cx, |ws, _| ws.pinned_for_test()), [true]);

    // Shown again, it keeps its folder.
    view.update(cx, |e, cx| e.navigate(p(r"C:\third"), NavKind::New, cx));
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 2);
    assert_eq!(at(&view, cx), Some(p(r"C:\other")));
}

/// "New folder in" on a subfolder of a pinned tab makes it in a tab on that subfolder.
#[gpui::test]
fn new_folder_in_a_subfolder_of_a_pinned_tab(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("sub")]);
    fs.insert_many(r"C:\p\sub", 1);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\p");
    let pinned = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &pinned, "p", loaded(r"C:\p"));
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    right_click(&pinned, cx, Some(0));
    run_command(&pinned, cx, MenuCommand::NewFolderIn);
    let sub = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &sub, "sub", loaded(r"C:\p\sub"));
    wait_shell("created", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::NewFolder {
            parent: p(r"C:\p\sub"),
            name: t!("ops.new_folder_name").into_owned(),
        }]
    );
    assert_eq!(at(&pinned, cx), Some(p(r"C:\p")));
    assert_eq!(tab_count(&ws, cx), 2);
}

/// A file typed in a pinned tab's path bar opens, and shows selected in its folder: in the
/// pinned tab when that is its folder, else in a tab on the folder (not on the file).
#[gpui::test]
fn a_file_typed_in_a_pinned_tab_opens_without_a_duplicate_tab(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1), file("notes.txt", 1)]);
    fs.insert_dir(r"C:\q", vec![file("x.txt", 1), file("y.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\p");
    let pinned = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &pinned, "p", loaded(r"C:\p"));
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();

    cx.simulate_keystrokes("ctrl-l");
    cx.simulate_input(r"C:\p\notes.txt");
    cx.simulate_keystrokes("enter");
    wait_shell_in(cx, "opened", || !shell.opened.lock().is_empty());
    wait_until(cx, &pinned, "selected", |e| {
        selected_name(e).as_deref() == Some("notes.txt")
    });
    assert_eq!(tab_count(&ws, cx), 1);
    assert_eq!(shell.opened.lock().as_slice(), [p(r"C:\p\notes.txt")]);

    cx.simulate_keystrokes("ctrl-l");
    cx.simulate_input(r"C:\q\y.txt");
    cx.simulate_keystrokes("enter");
    wait_shell_in(cx, "opened", || shell.opened.lock().len() == 2);
    let deadline = Instant::now() + Duration::from_secs(20);
    while tab_count(&ws, cx) < 2 {
        assert!(Instant::now() < deadline, "no tab opened");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let q = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &q, "q with y selected", |e| {
        loaded(r"C:\q")(e) && selected_name(e).as_deref() == Some("y.txt")
    });
    assert_eq!(at(&pinned, cx), Some(p(r"C:\p")));
}

/// Dragging over a pinned tab's targets starts no countdown (there's nowhere it could go).
#[gpui::test]
fn no_spring_countdown_over_a_pinned_tab(cx: &mut TestAppContext) {
    let (ws, cx) = setup_workspace(cx, folders(&[r"C:\p", r"C:\p\sub"]), r"C:\p");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "p", loaded(r"C:\p"));
    let hover = |cx: &mut VisualTestContext, inside: bool| {
        let target = crate::spring::Target::Explorer(view.downgrade());
        cx.update(|window, cx| {
            crate::spring::hover_at(
                "crumb:test".into(),
                crate::spring::Action::Open(p(r"C:\p\sub")),
                target,
                inside,
                gpui::point(gpui::px(10.), gpui::px(10.)),
                window,
                cx,
            );
            crate::spring::pending_key(cx)
        })
    };
    assert_eq!(hover(cx, true).as_deref(), Some("crumb:test"));
    assert_eq!(hover(cx, false), None);
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    assert_eq!(hover(cx, true), None);
}

/// Middle click on a folder already open in a tab: that tab flashes instead of nothing
/// happening.
#[gpui::test]
fn a_folder_already_open_flashes_its_tab(cx: &mut TestAppContext) {
    let (ws, cx) = setup_workspace(cx, folders(&[r"C:\a", r"C:\b"]), r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "a", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b"]);
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    assert_eq!(ws.update(cx, |ws, _| ws.tab_flash_for_test(1)), None);
    let middle_click_b = |cx: &mut VisualTestContext| {
        first.update(cx, |_, cx| {
            cx.emit(ExplorerEvent::OpenInNewTab {
                path: p(r"C:\B"),
                activate: false,
            });
        });
        cx.run_until_parked();
    };
    middle_click_b(cx);
    assert_eq!((tab_count(&ws, cx), active(&ws, cx)), (2, 0));
    let flash = ws.update(cx, |ws, _| ws.tab_flash_for_test(1));
    assert!(flash.is_some());
    assert_eq!(ws.update(cx, |ws, _| ws.tab_flash_for_test(0)), None);
    // Again: a new flash.
    middle_click_b(cx);
    assert_ne!(ws.update(cx, |ws, _| ws.tab_flash_for_test(1)), flash);
}

/// A saved workspace's pinned tab in another spelling of an open pinned tab's folder isn't
/// opened a second time.
#[gpui::test]
fn saved_workspaces_match_pinned_tabs_in_any_spelling(cx: &mut TestAppContext) {
    use crate::settings::SessionTab;
    let (ws, cx) = setup_workspace(cx, folders(&[r"C:\Work", r"C:\x"]), r"C:\Work");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "work", loaded(r"C:\Work"));
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    let tab = |path: &str, pinned: bool| SessionTab {
        panes: vec![p(path)],
        active_pane: 0,
        ratio: 0.5,
        pinned,
        synced: false,
        home: false,
    };
    let tabs = vec![tab(r"c:\work\", true), tab(r"C:\x", false)];
    cx.update(|_, cx| {
        WorkspaceStore::global(cx).update(cx, |s, cx| {
            s.update(|w| w.save("Spelled", tabs, 1), cx).expect("saved");
        });
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.open_saved_workspace("Spelled", window, cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(
            ws.all_paths_for_test(cx),
            [vec![p(r"C:\Work")], vec![p(r"C:\x")]]
        );
        assert_eq!(ws.pinned_for_test(), [true, false]);
    });
}

/// A pinned split tab keeps both folders: a move on either side opens a new tab, and
/// synchronized browsing can't be turned on there.
#[gpui::test]
fn a_pinned_split_tab_keeps_both_sides(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\l", r"C:\l\sub", r"C:\r", r"C:\r\sub"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\l");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "l", loaded(r"C:\l"));
    let (left, right) = split_to(&ws, cx, r"C:\r");
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    cx.dispatch_action(ToggleSyncBrowsing);
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.synced_for_test()), [false]);

    right.update(cx, |e, cx| e.navigate(p(r"C:\r\sub"), NavKind::New, cx));
    cx.run_until_parked();
    let new = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &new, r"r\sub in a new tab", loaded(r"C:\r\sub"));
    assert_eq!(tab_count(&ws, cx), 2);
    assert_eq!(at(&left, cx), Some(p(r"C:\l")));
    assert_eq!(at(&right, cx), Some(p(r"C:\r")));
}

/// Home or Settings over a pinned tab: a folder chosen from Home or the sidebar opens in a new
/// tab and the pinned one keeps its folder.
#[gpui::test]
fn a_page_over_a_pinned_tab_opens_folders_beside_it(cx: &mut TestAppContext) {
    let fs = folders(&[r"C:\p", r"C:\other", r"C:\third"]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\p");
    let pinned = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &pinned, "p", loaded(r"C:\p"));
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();

    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.home_open_folder_for_test(p(r"C:\other"), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(tab_count(&ws, cx), 2);
    assert_eq!(at(&pinned, cx), Some(p(r"C:\p")));

    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    ws.update_in(cx, Workspace::open_settings_for_test);
    ws.update_in(cx, |ws, window, cx| {
        ws.sidebar_open_for_test(p(r"C:\third"), window, cx);
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(
            tab_paths(ws, cx),
            [Some(p(r"C:\p")), Some(p(r"C:\third")), Some(p(r"C:\other"))]
        );
    });
}

/// Up from a pinned tab opens the parent in a new tab with the folder it came from selected.
#[gpui::test]
fn up_from_a_pinned_tab_selects_where_it_came_from(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("a"), dir("sub"), dir("z")]);
    fs.insert_many(r"C:\p\sub", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\p\sub");
    let pinned = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &pinned, "sub", loaded(r"C:\p\sub"));
    ws.update(cx, |ws, cx| ws.toggle_pin(0, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("alt-up");
    cx.run_until_parked();
    let up = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &up, "p with sub selected", |e| {
        loaded(r"C:\p")(e) && selected_name(e).as_deref() == Some("sub")
    });
    assert_eq!(at(&pinned, cx), Some(p(r"C:\p\sub")));
}

/// Pins brought back by a session or by Ctrl+Shift+T keep their folders too.
#[gpui::test]
fn restored_pinned_tabs_are_locked(cx: &mut TestAppContext) {
    use crate::settings::{Session, SessionTab};
    let fs = folders(&[r"C:\p", r"C:\q"]);
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
    });
    let tab = |path: &str, pinned: bool| SessionTab {
        panes: vec![p(path)],
        active_pane: 0,
        ratio: 0.5,
        pinned,
        synced: false,
        home: false,
    };
    let session = Session {
        tabs: vec![tab(r"C:\p", true), tab(r"C:\q", false)],
        active: 0,
    };
    let services = Services {
        fs,
        shell: Arc::new(FakeShell::default()),
        remotes: crate::remote_shell::test_remotes(),
    };
    let (ws, cx) = cx.add_window_view(move |window, cx| {
        Workspace::new(
            services,
            p(r"C:\p"),
            false,
            Some(session),
            None,
            Instant::now(),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    let locks = |cx: &mut VisualTestContext| {
        ws.update(cx, |ws, cx| {
            ws.tab_views()
                .map(|v| v.read(cx).locked)
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(locks(cx), [true, false]);

    ws.update_in(cx, |ws, window, cx| ws.close_tab(0, window, cx));
    cx.run_until_parked();
    assert_eq!(locks(cx), [false]);
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.pinned_for_test()), [true, false]);
    assert_eq!(locks(cx), [true, false]);
}
