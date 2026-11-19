//! GPUI tests for dragging tabs and folders around the sidebar: a tab dragged out of the window
//! becomes a window of its own or moves into another one (`workspace_tabmove.rs`), files dropped
//! on a tab go into its folder (#32), and tabs and folders dragged onto the favorites or the
//! pinned list are added there (`workspace_pindrop.rs`).

use gpui::{ExternalPaths, FileDropEvent, Modifiers, MouseButton, Point, point, px, size};

use super::*;
use crate::settings::{SettingsStore, TabLayout};

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn workspaces(cx: &VisualTestContext) -> Vec<gpui::WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .collect()
}

/// The folder of each tab of `window` (through the app: the test's own window may have closed).
fn tabs_in(
    window: gpui::WindowHandle<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<Option<PathBuf>> {
    cx.cx.update(|cx| tab_paths(window.read(cx).unwrap(), cx))
}

/// Settings read, with `f` applied (the session is only saved once they are).
fn with_settings(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings)) {
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            f(&mut store.settings);
            store.mark_loaded(cx);
        });
    });
    cx.run_until_parked();
}

/// Opens a tab on each of `paths` after the others and waits for it to list.
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

/// Presses at `at` and moves a little, so a drag starts.
fn start_drag_at(cx: &mut VisualTestContext, at: Point<gpui::Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        at + point(px(12.), px(1.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.update(|_, cx| assert!(cx.has_active_drag(), "the drag should have started"));
}

fn start_tab_drag(cx: &mut VisualTestContext, selector: &'static str) {
    let at = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"))
        .center();
    start_drag_at(cx, at);
}

/// Starts dragging list row `row` of `view`.
fn start_row_drag(cx: &mut VisualTestContext, view: &Entity<Explorer>, row: usize) {
    let (bounds, height) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = point(
        bounds.left() + px(60.),
        bounds.top() + height * (row as f32 + 0.5),
    );
    start_drag_at(cx, at);
}

fn move_to(cx: &mut VisualTestContext, at: Point<gpui::Pixels>) {
    cx.simulate_mouse_move(at, Some(MouseButton::Left), Modifiers::none());
    cx.run_until_parked();
}

fn release_at(cx: &mut VisualTestContext, at: Point<gpui::Pixels>, modifiers: Modifiers) {
    cx.simulate_mouse_move(at, Some(MouseButton::Left), modifiers);
    cx.run_until_parked();
    cx.simulate_mouse_up(at, MouseButton::Left, modifiers);
    cx.run_until_parked();
}

/// A tab dragged out of the window and released becomes a window of its own there, with its
/// history; the session then has both windows. Single window mode (on) doesn't turn it back into a
/// tab. The only tab of a window has nothing to tear off.
#[gpui::test]
fn dragging_a_tab_out_of_the_window_opens_it_in_a_new_window(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "a", loaded(r"C:\a"));
    with_settings(cx, |s| assert!(s.single_window, "on by default"));

    // The only tab: dropped outside, nothing happens.
    start_tab_drag(cx, "tab-0");
    release_at(cx, point(px(-40.), px(300.)), Modifiers::none());
    assert_eq!(workspaces(cx).len(), 1);
    assert_eq!(tabs_in(workspaces(cx)[0], cx), [Some(p(r"C:\a"))]);

    // A second tab that went from b to c, so Back leads to b.
    open_tabs(&ws, cx, &[r"C:\b"]);
    let second = ws.update(cx, |ws, _| ws.active_view().clone());
    second.update(cx, |e, cx| e.navigate(p(r"C:\c"), NavKind::New, cx));
    wait_until(cx, &second, "c", loaded(r"C:\c"));
    cx.run_until_parked();

    start_tab_drag(cx, "tab-1");
    release_at(cx, point(px(-40.), px(300.)), Modifiers::none());
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 2, "a window of its own");
    let source = windows[0];
    let torn = windows[1];
    assert_eq!(
        tabs_in(source, cx),
        [Some(p(r"C:\a"))],
        "it left the first window"
    );
    assert_eq!(tabs_in(torn, cx), [Some(p(r"C:\c"))]);
    let back = cx.update(|_, cx| {
        let ws = torn.read(cx).unwrap();
        ws.active_view()
            .read(cx)
            .history
            .back_target()
            .map(Path::to_path_buf)
    });
    assert_eq!(back, Some(p(r"C:\b")), "the history came along");

    // Both windows are in the session, each with its own tabs.
    cx.run_until_parked();
    let saved = cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .session_windows
            .clone()
    });
    let mut firsts: Vec<Vec<PathBuf>> = saved
        .iter()
        .map(|w| w.session.tabs.iter().map(|t| t.panes[0].clone()).collect())
        .collect();
    firsts.sort();
    assert_eq!(firsts, [vec![p(r"C:\a")], vec![p(r"C:\c")]]);
}

/// A tab drag whose release went to another program ends on the next move with no button down:
/// the tab stays in its window, and no window is torn off.
#[gpui::test]
fn a_tab_drag_whose_release_was_lost_tears_off_nothing(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "a", loaded(r"C:\a"));
    open_tabs(&ws, cx, &[r"C:\b"]);
    cx.run_until_parked();

    start_tab_drag(cx, "tab-1");
    move_to(cx, point(px(-40.), px(300.)));
    // Released over another program's window; the next move Arke hears has no button down.
    cx.simulate_mouse_move(point(px(300.), px(300.)), None, Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(!cx.has_active_drag(), "the drag is over"));
    assert_eq!(ws.update(cx, |ws, _| ws.tab_ghost_shown_for_test()), None);
    assert_eq!(workspaces(cx).len(), 1, "no window torn off");
    assert_eq!(
        tabs_in(workspaces(cx)[0], cx),
        [Some(p(r"C:\a")), Some(p(r"C:\b"))]
    );
}

/// Past the window's edge the drag image inside it stops; the tab is drawn beside the pointer in a
/// window of its own instead, saying what letting go does, and goes when the pointer comes back or
/// the tab is dropped. The only tab has nothing to tear off, so nothing shows for it.
#[gpui::test]
fn a_tab_dragged_past_the_edge_is_drawn_beside_the_pointer(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "a", loaded(r"C:\a"));
    let shown = |cx: &mut VisualTestContext| ws.update(cx, |ws, _| ws.tab_ghost_shown_for_test());
    let popups =
        |cx: &mut VisualTestContext| cx.update(|_, cx| crate::popup::open_popups(cx).len());

    start_tab_drag(cx, "tab-0");
    move_to(cx, point(px(-40.), px(300.)));
    assert_eq!(shown(cx), None, "the only tab stays where it is");
    release_at(cx, point(px(-40.), px(300.)), Modifiers::none());

    open_tabs(&ws, cx, &[r"C:\b"]);
    start_tab_drag(cx, "tab-1");
    assert_eq!(shown(cx), None, "inside the window GPUI draws the drag");
    move_to(cx, point(px(-40.), px(300.)));
    assert_eq!(
        shown(cx).as_deref(),
        Some(t!("tabs.drop_new_window").as_ref())
    );
    assert_eq!(popups(cx), 1, "in a window of its own");
    move_to(cx, point(px(-60.), px(320.)));
    assert_eq!(popups(cx), 1, "the same one follows the pointer");

    move_to(cx, point(px(300.), px(300.)));
    assert_eq!(shown(cx), None, "back over the window");
    assert_eq!(popups(cx), 0);

    move_to(cx, point(px(-40.), px(300.)));
    assert!(shown(cx).is_some());
    release_at(cx, point(px(-40.), px(300.)), Modifiers::none());
    assert_eq!(shown(cx), None, "gone once dropped");
    assert_eq!(popups(cx), 0);
    assert_eq!(
        workspaces(cx).len(),
        2,
        "and the tab has a window of its own"
    );
}

/// A tab dropped on another Arke window's tabs moves into it, between the tabs under the cursor;
/// its first window closes with its last tab.
#[gpui::test]
fn dropping_a_tab_on_another_windows_tabs_moves_it_there(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "a", loaded(r"C:\a"));
    with_settings(cx, |_| {});
    open_tabs(&ws, cx, &[r"C:\b", r"C:\c"]);
    // Narrow, so there is room for another window beside it.
    cx.simulate_resize(size(px(800.), px(1000.)));
    cx.run_until_parked();

    // c goes to a new window to the right.
    start_tab_drag(cx, "tab-2");
    release_at(cx, point(px(1100.), px(400.)), Modifiers::none());
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 2);
    let (source, other) = (windows[0], windows[1]);
    assert_eq!(tabs_in(other, cx), [Some(p(r"C:\c"))]);
    let (origin, slot) = other
        .update(cx, |ws, window, _| {
            (window.bounds().origin, ws.tab_slot_for_test(0))
        })
        .unwrap();
    let slot = slot.expect("the other window's tab should be drawn");
    assert!(origin.x > px(800.), "beside the first window: {origin:?}");

    // b dropped on the upper half of c over there: before it.
    start_tab_drag(cx, "tab-1");
    let over = origin + point(slot.center().x, slot.top() + px(3.));
    release_at(cx, over, Modifiers::none());
    assert_eq!(workspaces(cx).len(), 2);
    assert_eq!(tabs_in(source, cx), [Some(p(r"C:\a"))]);
    assert_eq!(tabs_in(other, cx), [Some(p(r"C:\b")), Some(p(r"C:\c"))]);
    assert_eq!(
        cx.update(|_, cx| other.read(cx).unwrap().active),
        0,
        "the moved tab is the active one"
    );

    // The last tab of the first window, dropped on the other one away from its tabs: at the end,
    // and the first window closes.
    start_tab_drag(cx, "tab-0");
    release_at(cx, origin + point(px(700.), px(500.)), Modifiers::none());
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 1, "the emptied window closed");
    assert_eq!(
        tabs_in(windows[0], cx),
        [Some(p(r"C:\b")), Some(p(r"C:\c")), Some(p(r"C:\a"))]
    );
}

/// Files dropped on another tab (#32) go into its folder, as on a folder: moved on the same
/// drive, copied with Ctrl. While they hover, a bar fills under the tab until it comes forward.
fn drop_files_on_a_tab(cx: &mut TestAppContext, layout: TabLayout) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1), file("b.txt", 1)]);
    fs.insert_dir(r"C:\t", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\s");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "s", loaded(r"C:\s"));
    with_settings(cx, |s| s.tab_layout = layout);
    open_tabs(&ws, cx, &[r"C:\t"]);
    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    cx.run_until_parked();

    // a.txt onto the second tab.
    start_row_drag(cx, &first, 0);
    let tab = cx
        .debug_bounds("tab-1")
        .expect("the second tab should be drawn");
    move_to(cx, tab.center());
    move_to(cx, tab.center() + point(px(2.), px(0.)));
    assert!(
        cx.debug_bounds("tab-spring-bar").is_some(),
        "a bar fills under the tab while files hover"
    );
    release_at(cx, tab.center(), Modifiers::none());
    wait_shell("move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops()[0],
        FileOp::Move {
            sources: vec![p(r"C:\s\a.txt")],
            dest: p(r"C:\t"),
        }
    );
    assert!(
        cx.debug_bounds("tab-spring-bar").is_none(),
        "gone after the drop"
    );

    // b.txt with Ctrl: copied.
    start_row_drag(cx, &first, 1);
    release_at(cx, tab.center(), Modifiers::control());
    wait_shell("copy", || shell.ops().len() > 1);
    assert_eq!(
        shell.ops()[1],
        FileOp::Copy {
            sources: vec![p(r"C:\s\b.txt")],
            dest: p(r"C:\t"),
        }
    );
}

#[gpui::test]
fn files_dropped_on_a_vertical_tab_go_into_its_folder(cx: &mut TestAppContext) {
    drop_files_on_a_tab(cx, TabLayout::Vertical);
}

#[gpui::test]
fn files_dropped_on_a_horizontal_tab_go_into_its_folder(cx: &mut TestAppContext) {
    drop_files_on_a_tab(cx, TabLayout::Horizontal);
}

fn favorites(cx: &mut VisualTestContext) -> Option<Vec<PathBuf>> {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .favorites
            .clone()
    })
}

/// The tab menu adds the tab's folder to the favorites (the tiles while there is room, then the
/// list below) and takes it off again. "Pin" is only about the tab itself.
#[gpui::test]
fn the_tab_menu_adds_and_removes_a_favorite(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\w"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "w", loaded(r"C:\w"));
    with_settings(cx, |s| s.favorites = Some(vec![p(r"C:\a"), p(r"C:\b")]));
    let add = t!("sidebar.add_favorite").into_owned();
    let remove = t!("sidebar.remove_favorite").into_owned();

    let labels = ws.update(cx, |ws, cx| ws.tab_menu_labels_for_test(0, cx));
    assert!(labels.contains(&add), "{labels:?}");
    let pins: Vec<&String> = labels
        .iter()
        .filter(|l| l.to_lowercase().contains("pin"))
        .collect();
    assert_eq!(pins, [&t!("sidebar.pin_tab").into_owned()], "{labels:?}");
    ws.update_in(cx, |ws, window, cx| {
        ws.run_tab_menu_for_test(0, &add, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        favorites(cx),
        Some(vec![p(r"C:\a"), p(r"C:\b"), p(r"C:\w")])
    );
    // It is on the tiles now: shown with its name.
    assert!(cx.debug_bounds("tile-slot-2").is_some());

    let labels = ws.update(cx, |ws, cx| ws.tab_menu_labels_for_test(0, cx));
    assert!(
        labels.contains(&remove) && !labels.contains(&add),
        "{labels:?}"
    );
    ws.update_in(cx, |ws, window, cx| {
        ws.run_tab_menu_for_test(0, &remove, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(favorites(cx), Some(vec![p(r"C:\a"), p(r"C:\b")]));

    // Full tiles: the list below takes it, and the menu still says it is a favorite.
    let full: Vec<PathBuf> = (0..crate::settings::FAVORITES_MAX)
        .map(|n| p(&format!(r"C:\t{n}")))
        .collect();
    with_settings(cx, |s| s.favorites = Some(full.clone()));
    ws.update_in(cx, |ws, window, cx| {
        ws.run_tab_menu_for_test(0, &add, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(favorites(cx), Some(full));
    let pinned = cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.pinned.clone());
    assert_eq!(pinned, [p(r"C:\w")]);
    let labels = ws.update(cx, |ws, cx| ws.tab_menu_labels_for_test(0, cx));
    assert!(labels.contains(&remove), "{labels:?}");
}

/// The left quarter of a tile: where a folder dragged there goes in before it.
fn left_edge(cx: &mut VisualTestContext, tile: &'static str) -> Point<gpui::Pixels> {
    let b = cx.debug_bounds(tile).expect("the tile should be drawn");
    point(b.left() + b.size.width * 0.1, b.center().y)
}

/// A folder dragged from the list onto the edge of a tile is added to the favorites there, with
/// a mark showing where; its middle still drops into the tile's folder. A tab dropped on a tile
/// adds its folder. Files are not added.
#[gpui::test]
fn folders_and_tabs_dragged_onto_the_favorites_are_added_where_the_mark_shows(
    cx: &mut TestAppContext,
) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\f", vec![dir("one"), dir("two"), file("x.txt", 1)]);
    for dir in [r"C:\a", r"C:\b", r"C:\c", r"C:\f\one", r"C:\f\two", r"C:\t"] {
        fs.insert_many(dir, 1);
    }
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\f");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "f", loaded(r"C:\f"));
    with_settings(cx, |s| {
        s.favorites = Some(vec![p(r"C:\a"), p(r"C:\b"), p(r"C:\c")]);
    });
    assert!(
        cx.debug_bounds("tile-add-favorite").is_none(),
        "no slot without a drag"
    );

    // "one" onto the left edge of b: a mark, the "Add to favorites" slot, and it goes before b.
    start_row_drag(cx, &view, 0);
    let edge = left_edge(cx, "tile-slot-1");
    move_to(cx, edge);
    move_to(cx, edge + point(px(1.), px(0.)));
    assert!(
        cx.debug_bounds("pin-insertion-mark").is_some(),
        "the mark shows where"
    );
    assert!(
        cx.debug_bounds("tile-add-favorite").is_some(),
        "and what the drop does"
    );
    release_at(cx, edge + point(px(1.), px(0.)), Modifiers::none());
    assert_eq!(
        favorites(cx),
        Some(vec![p(r"C:\a"), p(r"C:\f\one"), p(r"C:\b"), p(r"C:\c")])
    );
    assert!(shell.ops().is_empty(), "nothing moved into b");
    assert!(cx.debug_bounds("tile-add-favorite").is_none());

    // The middle of a tile still drops into its folder.
    start_row_drag(cx, &view, 1);
    let middle = cx.debug_bounds("tile-slot-0").unwrap().center();
    move_to(cx, middle);
    assert!(cx.debug_bounds("pin-insertion-mark").is_none());
    release_at(cx, middle, Modifiers::none());
    wait_shell("move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops()[0],
        FileOp::Move {
            sources: vec![p(r"C:\f\two")],
            dest: p(r"C:\a"),
        }
    );
    assert_eq!(favorites(cx).map(|f| f.len()), Some(4));

    // A file gets no mark and no slot; dropped beside the tiles it does nothing.
    start_row_drag(cx, &view, 2);
    let edge = left_edge(cx, "tile-slot-1");
    move_to(cx, edge);
    assert!(
        cx.debug_bounds("pin-insertion-mark").is_none(),
        "files are not added"
    );
    assert!(cx.debug_bounds("tile-add-favorite").is_none());
    let grid = cx.debug_bounds("pin-tiles").unwrap();
    let last = cx.debug_bounds("tile-slot-3").unwrap();
    let beside = point(grid.right() - px(4.), last.center().y);
    release_at(cx, beside, Modifiers::none());
    assert_eq!(shell.ops().len(), 1, "the file stayed");
    assert_eq!(favorites(cx).map(|f| f.len()), Some(4));

    // A tab (on C:\t) onto the right half of the first tile: after it.
    let at = ws.update(cx, |ws, _| ws.tab_views().count());
    ws.update_in(cx, |ws, window, cx| {
        ws.open_tab(p(r"C:\t"), at, false, window, cx);
    });
    cx.run_until_parked();
    start_tab_drag(cx, "tab-1");
    let tile = cx.debug_bounds("tile-slot-0").unwrap();
    let right = point(tile.right() - px(6.), tile.center().y);
    move_to(cx, right);
    assert!(cx.debug_bounds("pin-insertion-mark").is_some());
    release_at(cx, right, Modifiers::none());
    assert_eq!(
        favorites(cx),
        Some(vec![
            p(r"C:\a"),
            p(r"C:\t"),
            p(r"C:\f\one"),
            p(r"C:\b"),
            p(r"C:\c")
        ])
    );
    assert_eq!(
        ws.update(cx, |ws, cx| tab_paths(ws, cx)),
        [Some(p(r"C:\f")), Some(p(r"C:\t"))],
        "the tab stays where it was"
    );
}

/// Paths from Explorer dropped on the "Add to favorites" slot: the folders among them are added,
/// once checked in the background; files are not.
#[gpui::test]
fn folders_from_explorer_dropped_on_the_favorites_are_added(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\f", vec![dir("two"), file("x.txt", 1)]);
    for dir in [r"C:\a", r"C:\f\two"] {
        fs.insert_many(dir, 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\f");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "f", loaded(r"C:\f"));
    with_settings(cx, |s| s.favorites = Some(vec![p(r"C:\a")]));

    // From Explorer: a folder and a file onto the "Add to favorites" slot; only the folder is added.
    let grid = cx.debug_bounds("pin-tiles").unwrap();
    let paths = ExternalPaths(vec![p(r"C:\f\two"), p(r"C:\f\x.txt")].into());
    cx.simulate_event(FileDropEvent::Entered {
        position: grid.center(),
        paths,
    });
    cx.run_until_parked();
    let slot = cx
        .debug_bounds("tile-add-favorite")
        .expect("the slot shows for paths from Explorer too")
        .center();
    cx.simulate_event(FileDropEvent::Pending { position: slot });
    cx.run_until_parked();
    cx.simulate_event(FileDropEvent::Pending {
        position: slot + point(px(1.), px(0.)),
    });
    cx.run_until_parked();
    cx.simulate_event(FileDropEvent::Submit {
        position: slot + point(px(1.), px(0.)),
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while favorites(cx).map_or(0, |f| f.len()) < 2 {
        assert!(Instant::now() < deadline, "the folder should be added");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        favorites(cx).and_then(|f| f.last().cloned()),
        Some(p(r"C:\f\two"))
    );
    assert_eq!(favorites(cx).map(|f| f.len()), Some(2), "not the file");
}

/// A folder dragged onto the "Pinned" heading goes first in the list; onto the lower edge of a
/// pinned row, below it.
#[gpui::test]
fn folders_dragged_onto_the_pinned_list_are_pinned_there(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\f", vec![dir("one"), dir("two")]);
    for dir in [r"C:\a", r"C:\f\one", r"C:\f\two", r"C:\p"] {
        fs.insert_many(dir, 1);
    }
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\f");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "f", loaded(r"C:\f"));
    with_settings(cx, |s| {
        s.favorites = Some(vec![p(r"C:\a")]);
        s.pinned = vec![p(r"C:\p")];
    });
    let pinned = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.pinned.clone())
    };

    start_row_drag(cx, &view, 0);
    let head = cx.debug_bounds("pin-list-label").unwrap().center();
    move_to(cx, head);
    move_to(cx, head + point(px(1.), px(0.)));
    assert!(cx.debug_bounds("pin-insertion-mark").is_some());
    release_at(cx, head + point(px(1.), px(0.)), Modifiers::none());
    assert_eq!(pinned(cx), [p(r"C:\f\one"), p(r"C:\p")]);

    start_row_drag(cx, &view, 1);
    let row = cx.debug_bounds("pin-row-1").expect("the second pinned row");
    let low = point(row.center().x, row.bottom() - px(2.));
    move_to(cx, low);
    move_to(cx, low + point(px(1.), px(0.)));
    release_at(cx, low + point(px(1.), px(0.)), Modifiers::none());
    assert_eq!(pinned(cx), [p(r"C:\f\one"), p(r"C:\p"), p(r"C:\f\two")]);
    assert!(
        shell.ops().is_empty(),
        "nothing moved into the pinned folders"
    );
    assert_eq!(favorites(cx), Some(vec![p(r"C:\a")]));
}
