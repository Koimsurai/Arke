//! GPUI tests for tabs and dual pane: tab cycling, pinned tabs, saved workspaces, synchronized
//! browsing and folder compare (`workspace.rs`, `workspace_saved.rs`, `explorer_compare.rs`).

use arke_core::compare::Mark;

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
                .map(|m| m.iter().map(|(k, v)| (k.to_string(), *v)).collect())
                .unwrap_or_default();
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
        let fs = fs.clone();
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
    let content = px(1700.) - crate::theme::SIDEBAR_W;
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
        ws.open_external(Target::Folder(p(r"C:\dl")), vec![one.clone()], window, cx);
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
        ws.open_external(Target::Folder(p(r"c:\DL\")), vec![two.clone()], window, cx);
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
fn closing_the_last_tab_leaves_the_window_on_home(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\a"));

    ws.update_in(cx, |ws, window, cx| ws.close_tab(0, window, cx));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 1);
    assert_eq!(
        ws.update(cx, |ws, _| ws.shown_page_for_test()),
        Some(rust_i18n::t!("home.title").into_owned()),
        "the window stays, showing Home"
    );

    // Home has the keyboard: Ctrl+W there closes the window.
    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| cx.windows().len()),
        0,
        "Ctrl+W on Home closes it"
    );
}
