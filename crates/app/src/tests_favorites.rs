//! GPUI tests for the favorites at the top of the sidebar (`workspace_sidebar.rs`): a tile dragged
//! like an app icon (lifted under the pointer, an empty slot the others slide around, settling
//! back when dropped), removal by dragging one out of the sidebar (with Undo), long names kept on
//! their tiles, names of their own (#259), and where the Recycle Bin goes
//! when it leaves the sidebar (#31).

use gpui::{Bounds, Modifiers, MouseButton, Pixels, Point, point, px};

use super::*;
use crate::settings::{Settings, SettingsStore};

/// Too long for a tile.
const LONG: &str = "Quarterly reports for the board";

fn p(name: &str) -> PathBuf {
    PathBuf::from(r"C:\w").join(name)
}

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut Settings) + 'static) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

fn current(cx: &mut VisualTestContext) -> Settings {
    cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.clone())
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn drawn(cx: &mut VisualTestContext, selector: &str) -> bool {
    let id: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    cx.debug_bounds(id).is_some()
}

/// A window on `C:\w`, whose folders `a` to `d` and [`LONG`] can be favorites, and the shell that
/// would run any file operation.
fn window_and_shell(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Arc<FakeShell>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    let names = ["a", "b", "c", "d", LONG];
    fs.insert_dir(r"C:\w", names.iter().map(|n| dir(n)).collect());
    for name in names {
        fs.insert_dir(p(name), Vec::new());
    }
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    refresh(cx);
    (ws, shell, cx)
}

/// A window on `C:\w`, whose folders `a` to `d` and [`LONG`] can be favorites.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (ws, _, cx) = window_and_shell(cx);
    (ws, cx)
}

/// Over the file list, outside the sidebar.
fn over_the_content() -> Point<Pixels> {
    point(px(700.), px(160.))
}

fn click(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
}

/// Drags what was picked up out over the content and lets go there: it shows it would be removed
/// first.
fn drop_outside(cx: &mut VisualTestContext) {
    assert!(
        !drawn(cx, "pin-remove-cue"),
        "over the sidebar: nothing yet"
    );
    let out = over_the_content();
    cx.simulate_mouse_move(out, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert!(drawn(cx, "pin-remove-cue"), "outside: it shows it would go");
    cx.simulate_mouse_up(out, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    cx.update(|_, cx| assert!(!cx.has_active_drag()));
}

fn bounds(cx: &mut VisualTestContext, selector: &str) -> Bounds<Pixels> {
    let id: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
}

/// The tiles show a favorite dragged from the list can go up there: the "Move to the tiles" slot
/// where it fits beside the last tiles, else their wash.
fn invited(cx: &mut VisualTestContext) -> bool {
    drawn(cx, "tile-add-favorite") || drawn(cx, "tiles-invite")
}

fn near(a: Pixels, b: Pixels) -> bool {
    (a - b).abs() <= px(1.)
}

/// Takes hold of the tile drawn at `cell` and starts dragging it: a first move starts the drag
/// (6, 4 from the center, where the tile is held), a second one has every tile report where it is.
fn pick_up(cx: &mut VisualTestContext, cell: Bounds<Pixels>) {
    cx.simulate_mouse_down(cell.center(), MouseButton::Left, Modifiers::none());
    for nudge in [6., 8.] {
        cx.simulate_mouse_move(
            cell.center() + point(px(nudge), px(4.)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        refresh(cx);
    }
    cx.update(|_, cx| assert!(cx.has_active_drag()));
}

/// Rests the mouse at `at` long enough for gpui to show the tooltip there.
fn hover_for_tooltip(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_move(at, None, Modifiers::none());
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(800));
    refresh(cx);
}

/// Starts renaming the favorite from its menu, types `name` and presses Enter.
fn rename(ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: &Path, name: &str) {
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_favorite_from_menu_for_test(path, window, cx);
    });
    refresh(cx);
    let input = ws
        .update(cx, |ws, _| ws.favorite_rename_for_test(path))
        .expect("the box is open");
    input.update(cx, |input, cx| input.set_text(name.to_owned(), cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    refresh(cx);
}

/// Like an app dragged off the macOS Dock: a tile dragged out of the sidebar and let go there
/// leaves the favorites (its name with it), and nothing else happens to the folder. The toast's
/// Undo puts it back in its slot, name and all.
#[gpui::test]
fn a_tile_dropped_outside_the_sidebar_is_removed_until_undone(cx: &mut TestAppContext) {
    let (_ws, shell, cx) = window_and_shell(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b"), p("c"), p("d")]);
        st.favorite_names
            .insert(crate::place_names::key(&p("b")), "Bee".to_owned());
    });
    refresh(cx);
    let at = bounds(cx, "tile-slot-1");
    pick_up(cx, at);
    drop_outside(cx);
    let settings = current(cx);
    assert_eq!(settings.favorites, Some(vec![p("a"), p("c"), p("d")]));
    assert!(settings.favorite_names.is_empty());
    assert!(
        shell.ops().is_empty(),
        "nothing moved or copied into the list's folder"
    );
    assert!(drawn(cx, "toast-notice-action-0"), "a toast says so");

    let at = bounds(cx, "toast-notice-action-0").center();

    click(cx, at);
    let settings = current(cx);
    assert_eq!(
        settings.favorites,
        Some(vec![p("a"), p("b"), p("c"), p("d")]),
        "back in its slot"
    );
    assert_eq!(settings.favorite_name(&p("b")), Some("Bee"));
    assert!(!drawn(cx, "toast-notice-action-0"));
}

/// The same for an entry of the Favorites list.
#[gpui::test]
fn a_list_entry_dropped_outside_the_sidebar_is_removed_until_undone(cx: &mut TestAppContext) {
    let (_ws, shell, cx) = window_and_shell(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("c"), p("d")];
    });
    refresh(cx);
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        row.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    cx.update(|_, cx| assert!(cx.has_active_drag()));
    drop_outside(cx);
    assert_eq!(current(cx).pinned, vec![p("d")]);
    assert!(shell.ops().is_empty(), "nothing moved or copied");
    let at = bounds(cx, "toast-notice-action-0").center();
    click(cx, at);
    assert_eq!(
        current(cx).pinned,
        vec![p("c"), p("d")],
        "back in its place"
    );
}

/// Dragged out and back over the sidebar, a favorite stops showing it would go and stays.
#[gpui::test]
fn a_favorite_dragged_out_and_back_stays(cx: &mut TestAppContext) {
    let (_ws, shell, cx) = window_and_shell(cx);
    change(cx, |st| st.favorites = Some(vec![p("a"), p("b"), p("c")]));
    refresh(cx);
    let first = bounds(cx, "tile-slot-0");
    pick_up(cx, first);
    cx.simulate_mouse_move(
        over_the_content(),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    assert!(drawn(cx, "pin-remove-cue"));
    // Back over its own (empty) slot.
    let back = first.center() + point(px(8.), px(4.));
    cx.simulate_mouse_move(back, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert!(!drawn(cx, "pin-remove-cue"), "back over the sidebar");
    cx.simulate_mouse_up(back, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("a"), p("b"), p("c")]));
    assert!(!drawn(cx, "toast-notice-action-0"), "nothing was removed");
    assert!(shell.ops().is_empty());
}

/// A dragged tile is lifted under the pointer, a little bigger, held where it was taken; its slot
/// is left empty and moves with it, the other tiles sliding around it (across rows too). Let go,
/// it keeps that slot and glides in from where its lifted copy was.
#[gpui::test]
fn a_dragged_tile_leaves_a_gap_and_settles_into_it(cx: &mut TestAppContext) {
    crate::motion::force_for_test(true);
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b"), p("c"), p("d")]);
    });
    refresh(cx);
    let cells: Vec<Bounds<Pixels>> = (0..4)
        .map(|ix| bounds(cx, &format!("tile-slot-{ix}")))
        .collect();
    assert!(
        cells[3].top() > cells[0].bottom(),
        "three a row: d is on the next"
    );
    let lift = Workspace::tile_lift_for_test();

    pick_up(cx, cells[0]);
    let gap = bounds(cx, "tile-gap");
    assert!(near(gap.left(), cells[0].left()) && near(gap.top(), cells[0].top()));
    let lifted = bounds(cx, "tile-lifted");
    let ratio = f32::from(lifted.size.width) / f32::from(cells[0].size.width);
    assert!(
        (ratio - lift).abs() < 0.02,
        "the tile itself, a little bigger: {ratio}"
    );
    assert!(
        lifted.contains(&(cells[0].center() + point(px(8.), px(4.)))),
        "under the pointer"
    );

    // Over d, on the next row: a takes its slot, the gap with it, and the rest slide over.
    cx.simulate_mouse_move(
        cells[3].center(),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("b"), p("c"), p("d"), p("a")])
    );
    assert_eq!(
        ws.update(cx, |ws, _| ws.tiles_sliding_for_test()),
        vec![p("a"), p("b"), p("c"), p("d")],
        "every tile that moved slides, d up to the first row too"
    );
    let gap = bounds(cx, "tile-gap");
    assert!(near(gap.left(), cells[3].left()) && near(gap.top(), cells[3].top()));

    cx.simulate_mouse_up(cells[3].center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(!cx.has_active_drag()));
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("b"), p("c"), p("d"), p("a")]),
        "the order is kept"
    );
    // Held 6, 4 right of and below its center, the lifted copy's center was that much (lifted)
    // up and left of the pointer: the tile glides in from there.
    let (dx, dy) = ws
        .update(cx, |ws, _| ws.tile_slide_from_for_test(&p("a")))
        .expect("it settles into its slot");
    assert!(
        (dx + 6. * lift).abs() < 0.5 && (dy + 4. * lift).abs() < 0.5,
        "{dx} {dy}"
    );
    refresh(cx);
    assert!(!drawn(cx, "tile-gap"));
    assert!(!drawn(cx, "tile-lifted"));
    crate::motion::force_for_test(false);
}

/// A tile being renamed shows no tooltip: the full path would cover the box.
#[gpui::test]
fn a_tile_being_renamed_has_no_tooltip(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| st.favorites = Some(vec![p("a"), p("b")]));
    refresh(cx);
    let tile = bounds(cx, "tile-slot-0");
    // Over the icon, clear of the name and its box.
    let at = point(tile.center().x, tile.top() + px(6.));
    hover_for_tooltip(cx, at);
    assert!(drawn(cx, "tooltip-card"), "the path, on hover");
    cx.simulate_mouse_move(point(px(900.), px(600.)), None, Modifiers::none());
    refresh(cx);

    ws.update_in(cx, |ws, window, cx| {
        ws.rename_favorite_from_menu_for_test(&p("a"), window, cx);
    });
    refresh(cx);
    assert!(drawn(cx, "place-rename-box"));
    hover_for_tooltip(cx, at);
    assert!(!drawn(cx, "tooltip-card"), "not over the box");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
}

/// A favorite with a name too long for its tile is still a tile (cut off, its tooltip has the
/// path): where a folder goes, the tiles or the list, is the user's. Up to 0.8 it was drawn as a
/// row (#174), which looked like the list but wasn't: dragging it to the tiles did nothing.
#[gpui::test]
fn a_long_name_stays_on_its_tile(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| st.favorites = Some(vec![p("a"), p(LONG), p("b")]));
    refresh(cx);
    for slot in ["tile-slot-0", "tile-slot-1", "tile-slot-2"] {
        assert!(drawn(cx, slot), "{slot} is a tile");
    }
    assert!(!drawn(cx, "favorite-row-1"), "not a row");
}

/// #259: "Rename" in a favorite's menu gives it a name of its own on its tile or row; the folder
/// keeps its name everywhere else, an empty name brings the folder's back, and removing the
/// favorite forgets it.
#[gpui::test]
fn a_favorite_is_renamed_where_it_shows(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    for path in [p("a"), p("c")] {
        let labels = ws.update(cx, |ws, cx| ws.favorite_menu_labels_for_test(&path, cx));
        assert!(
            labels.contains(&t!("menu.rename").into_owned()),
            "{path:?}: {labels:?}"
        );
        assert!(
            labels.contains(&t!("sidebar.remove_favorite").into_owned()),
            "{path:?}: {labels:?}"
        );
    }

    // The box takes the tile's name, which it starts with.
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_favorite_from_menu_for_test(&p("a"), window, cx);
    });
    refresh(cx);
    let input = ws
        .update(cx, |ws, _| ws.favorite_rename_for_test(&p("a")))
        .expect("the box is open");
    assert_eq!(input.read_with(cx, |i, _| i.text().to_owned()), "a");
    assert!(drawn(cx, "place-rename-box"), "in the tile");
    input.update(cx, |input, cx| input.set_text("Alpha", cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    refresh(cx);
    let settings = current(cx);
    assert_eq!(
        settings.favorite_names.into_iter().collect::<Vec<_>>(),
        vec![(r"c:\w\a".to_owned(), "Alpha".to_owned())]
    );
    assert!(
        settings.place_names.is_empty(),
        "tab titles and the path bar keep the folder's name"
    );
    assert!(drawn(cx, "tile Alpha"));
    assert!(!drawn(cx, "place-rename-box"));

    // The folder's own tab still says `a`.
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    view.update(cx, |v, cx| v.navigate(p("a"), NavKind::New, cx));
    wait_until(cx, &view, "a", loaded(r"C:\w\a"));
    assert_eq!(view.read_with(cx, |v, _| v.tab_title().to_string()), "a");

    // A list entry too.
    rename(&ws, cx, &p("c"), "Gamma");
    assert!(drawn(cx, "sidebar Gamma"));
    assert!(!drawn(cx, "sidebar c"));

    // Empty brings the folder's name back.
    rename(&ws, cx, &p("a"), "");
    assert!(drawn(cx, "tile a"));
    assert_eq!(current(cx).favorite_names.len(), 1);

    // Taken off the favorites, it forgets its name.
    change(cx, |st| st.unpin(&p("c"), &[]));
    assert!(current(cx).favorite_names.is_empty());
}

/// Escape leaves a favorite's name as it was.
#[gpui::test]
fn escape_keeps_the_favorites_name(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| st.favorites = Some(vec![p("a")]));
    refresh(cx);
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_favorite_from_menu_for_test(&p("a"), window, cx);
    });
    refresh(cx);
    let input = ws
        .update(cx, |ws, _| ws.favorite_rename_for_test(&p("a")))
        .unwrap();
    input.update(cx, |input, cx| input.set_text("Nope", cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(current(cx).favorite_names.is_empty());
    assert!(ws.update(cx, |ws, _| ws.favorite_rename_for_test(&p("a")).is_none()));
    refresh(cx);
    assert!(drawn(cx, "tile a"));
}

/// #31: taking the Recycle Bin off the sidebar says where it is now.
#[gpui::test]
fn unpinning_the_recycle_bin_says_it_is_on_home(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    assert!(current(cx).recycle_pinned);
    ws.update_in(cx, Workspace::unpin_recycle_for_test);
    cx.run_until_parked();
    assert!(!current(cx).recycle_pinned);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    assert_eq!(
        view.read_with(cx, |v, _| v.notice.clone()),
        Some(t!("sidebar.recycle_on_home").into_owned())
    );
}

/// A pinned list entry dragged up over the tiles becomes a tile on the way, its cell left empty
/// under the pointer as a dragged tile's, and let go it stays there.
#[gpui::test]
fn a_list_entry_dragged_over_the_tiles_becomes_one(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let row = bounds(cx, "pin-row-0");
    let first = bounds(cx, "tile-slot-0");
    cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        row.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    let over = first.center() + point(px(2.), px(2.));
    cx.simulate_mouse_move(over, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("c"), p("a"), p("b")]));
    assert!(current(cx).pinned.is_empty(), "out of the list");
    assert!(
        drawn(cx, "tile-gap"),
        "its cell is the gap under the pointer"
    );
    cx.simulate_mouse_up(over, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("c"), p("a"), p("b")]),
        "stays where it was let go"
    );
}

/// Dragging a pinned list entry, the tiles show from the start that it can go up there: the
/// "Add to favorites" slot and its wash, before the pointer gets to them.
#[gpui::test]
fn the_tiles_invite_a_list_entry_being_dragged(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    assert!(!drawn(cx, "tile-add-favorite"));
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        row.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    assert!(invited(cx), "shown while it is still over the list");
    cx.simulate_mouse_up(row.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert!(!invited(cx), "gone with the drag");
}

/// A tile dragged down: the list shows a block where it can go; over a list row it becomes a list
/// entry at once (its row the gap, no insertion line), and back over the tiles a tile again, the
/// tiles inviting it once more whenever it is a list entry.
#[gpui::test]
fn a_favorite_takes_the_shape_of_where_it_is(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b"), p("c")]);
        st.pinned = vec![p("d")];
    });
    refresh(cx);
    let first = bounds(cx, "tile-slot-0");
    pick_up(cx, first);
    assert!(drawn(cx, "pin-list-drop"), "the list shows it can go there");
    assert!(
        !drawn(cx, "tile-add-favorite"),
        "a tile needs no invitation to the tiles"
    );

    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_move(row.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(current(cx).pinned, vec![p("a"), p("d")], "a list entry now");
    assert_eq!(current(cx).favorites, Some(vec![p("b"), p("c")]));
    assert!(drawn(cx, "pin-row-gap"), "its row is the gap");
    assert!(!drawn(cx, "pin-insertion-mark"), "no line");
    assert!(!drawn(cx, "pin-list-drop"), "the block made way for it");
    assert!(
        !drawn(cx, "tile-add-favorite"),
        "the tiles it left just close its gap"
    );

    let tile = bounds(cx, "tile-slot-0");
    cx.simulate_mouse_move(tile.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("a"), p("b"), p("c")]));
    assert_eq!(current(cx).pinned, vec![p("d")]);
    assert!(!drawn(cx, "tile-add-favorite"));
    assert!(drawn(cx, "pin-list-drop"), "and the list invites it again");

    cx.simulate_mouse_up(tile.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert!(!drawn(cx, "pin-list-drop"), "nothing left behind");
    assert!(!drawn(cx, "tile-add-favorite"));

    // The other way: a list entry shows the tiles take it, and once a tile, the list it left
    // shows nothing.
    let row = bounds(cx, "pin-row-0");
    pick_up(cx, row);
    assert!(invited(cx), "the tiles show it can go there");
    let tile = bounds(cx, "tile-slot-0");
    cx.simulate_mouse_move(tile.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert!(
        !drawn(cx, "pin-list-drop"),
        "the list it left just closes its gap"
    );
    cx.simulate_mouse_up(tile.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

/// Rows of the list make room for a favorite dragged over them as the tiles do: they slide to
/// their new places, a row at a time, and close up sliding when it leaves for the tiles.
#[gpui::test]
fn list_rows_slide_to_make_room(cx: &mut TestAppContext) {
    crate::motion::force_for_test(true);
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("b"), p("c"), p("d")];
    });
    refresh(cx);
    let rows: Vec<Bounds<Pixels>> = (0..3)
        .map(|ix| bounds(cx, &format!("pin-row-{ix}")))
        .collect();
    let pitch = f32::from(rows[1].top() - rows[0].top());
    let from = |ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: Option<&str>| {
        ws.update(cx, |ws, _| {
            ws.list_row_slide_from_for_test(path.map(p).as_deref())
        })
    };
    let close = |got: Option<f32>, want: f32| got.is_some_and(|got| (got - want).abs() < 1.);

    pick_up(cx, rows[0]);
    assert!(
        near(bounds(cx, "pin-row-1").top(), rows[1].top()),
        "its gap is a row high: the rows under it stay put"
    );
    cx.simulate_mouse_move(
        rows[2].center() + point(px(0.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    assert_eq!(current(cx).pinned, vec![p("c"), p("d"), p("b")]);
    assert!(
        close(from(&ws, cx, Some("c")), pitch),
        "c slides up from a row down"
    );
    assert!(close(from(&ws, cx, Some("d")), pitch), "d too");
    assert!(
        close(from(&ws, cx, Some("b")), -2. * pitch),
        "its gap from where it was"
    );
    assert_eq!(from(&ws, cx, None), None, "the Recycle Bin stays put");

    // Up onto the tiles: the rows under its gap close up, sliding.
    let tile = bounds(cx, "tile-slot-0");
    cx.simulate_mouse_move(tile.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(current(cx).pinned, vec![p("c"), p("d")]);
    assert!(
        close(from(&ws, cx, None), pitch),
        "the Recycle Bin closes up"
    );
    cx.simulate_mouse_up(tile.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    crate::motion::force_for_test(false);
}

/// A list row picked up with the tiles scrolled away above: the tiles open a row for it up there,
/// and the rows in view stay where they are, the one picked up under the pointer.
#[gpui::test]
fn a_row_picked_up_under_tiles_scrolled_away_stays_put(cx: &mut TestAppContext) {
    use gpui::{ScrollDelta, ScrollEvent};
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b"), p("c")]);
        st.pinned = vec![p("d"), p(LONG)];
    });
    cx.simulate_resize(gpui::size(px(1200.), px(360.)));
    refresh(cx);
    let grid = bounds(cx, "pin-tiles");
    let over = bounds(cx, "sidebar-places-frame").center();
    cx.simulate_event(ScrollEvent {
        position: over,
        delta: ScrollDelta::Pixels(point(px(0.), -(grid.size.height + px(12.)))),
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
    refresh(cx);
    let frame = bounds(cx, "sidebar-places-frame");
    assert!(
        bounds(cx, "pin-tiles").bottom() <= frame.top(),
        "the tiles are out of sight"
    );
    let row = bounds(cx, "pin-row-0");
    pick_up(cx, row);
    assert!(invited(cx), "the tiles show they take it");
    assert!(
        near(bounds(cx, "pin-row-gap").top(), row.top()),
        "the row stays under the pointer"
    );
    let at = bounds(cx, "pin-row-gap").center();
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert!(
        near(bounds(cx, "pin-row-0").top(), row.top()),
        "and once let go, the row closing up above leaves it there"
    );
}

/// A list entry dragged over the next row takes its place only once past the row's middle: a
/// pointer resting near the line between two rows doesn't make them trade places back and forth.
#[gpui::test]
fn list_rows_swap_past_the_middle_only(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("b"), p("c"), p("d")];
    });
    refresh(cx);
    let rows: Vec<Bounds<Pixels>> = (0..3)
        .map(|ix| bounds(cx, &format!("pin-row-{ix}")))
        .collect();
    pick_up(cx, rows[0]);
    let at = |cx: &mut VisualTestContext, y: Pixels| {
        cx.simulate_mouse_move(
            point(rows[1].center().x, y),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        refresh(cx);
        current(cx).pinned
    };
    // Into the next row but short of its middle: nothing moves.
    assert_eq!(at(cx, rows[1].top() + px(3.)), vec![p("b"), p("c"), p("d")]);
    // Past it: they trade places once.
    assert_eq!(
        at(cx, rows[1].center().y + px(3.)),
        vec![p("c"), p("b"), p("d")]
    );
    // Wobbling around that line now stays put.
    for y in [
        rows[1].top() + px(2.),
        rows[1].center().y - px(2.),
        rows[1].top() - px(1.),
    ] {
        assert_eq!(at(cx, y), vec![p("c"), p("b"), p("d")], "at {y:?}");
    }
    let end = rows[1].center();
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

/// A tile dragged down into the list and back up over the tiles' empty space (where it was)
/// becomes a tile again, the last one, instead of staying a list entry over the tiles.
#[gpui::test]
fn a_tile_back_over_the_empty_cell_is_a_tile_again(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b"), p("c"), p("d")]);
        st.pinned = vec![p(LONG)];
    });
    refresh(cx);
    let cells: Vec<Bounds<Pixels>> = (0..4)
        .map(|ix| bounds(cx, &format!("tile-slot-{ix}")))
        .collect();
    let last = cells[3];
    pick_up(cx, last);
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_move(row.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("a"), p("b"), p("c")]));
    assert!(current(cx).pinned.contains(&p("d")), "a list entry now");
    // Back where it was: that cell is empty now (no tile there to take it).
    cx.simulate_mouse_move(last.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("a"), p("b"), p("c"), p("d")]),
        "a tile again, in its place"
    );
    assert_eq!(current(cx).pinned, vec![p(LONG)]);
    assert!(
        drawn(cx, "tile-gap"),
        "its cell is the gap under the pointer"
    );
    cx.simulate_mouse_up(last.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("a"), p("b"), p("c"), p("d")])
    );
}

/// The line between the places and the tabs: dragged down, the places take that much more room
/// (saved, at 100% scale); double-clicked, the room is shared by itself again.
#[gpui::test]
fn the_line_above_the_tabs_sets_the_places_height(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.pinned = vec![p("a"), p("b"), p("c"), p("d")];
    });
    cx.simulate_resize(gpui::size(px(1200.), px(700.)));
    refresh(cx);
    let before = bounds(cx, "sidebar-places-frame");
    let line = bounds(cx, "sidebar-split");
    cx.simulate_mouse_down(line.center(), MouseButton::Left, Modifiers::none());
    let down = line.center() + point(px(0.), px(-60.));
    cx.simulate_mouse_move(down, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    let during = bounds(cx, "sidebar-places-frame");
    assert!(
        near(during.size.height, before.size.height - px(60.)),
        "{before:?} -> {during:?}"
    );
    cx.simulate_mouse_up(down, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    let saved = current(cx).sidebar_split;
    assert!(
        (saved - f32::from(during.size.height)).abs() < 1.,
        "saved {saved}"
    );
    assert!(near(
        bounds(cx, "sidebar-places-frame").size.height,
        during.size.height
    ));
    // Double-click: shared by itself again.
    let line = bounds(cx, "sidebar-split").center();
    cx.simulate_mouse_down(line, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(line, MouseButton::Left, Modifiers::none());
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: line,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_mouse_up(line, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert_eq!(current(cx).sidebar_split, 0.);
    assert!(near(
        bounds(cx, "sidebar-places-frame").size.height,
        before.size.height
    ));
}

/// Let go over the sidebar where nothing takes it (This PC), a list entry stays where the drag
/// left it and nothing the drag showed stays behind.
#[gpui::test]
fn a_pin_drag_let_go_elsewhere_in_the_sidebar_leaves_nothing_behind(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        row.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    assert!(invited(cx));
    let below = row.center() + point(px(0.), px(200.));
    cx.simulate_mouse_move(below, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert!(
        !drawn(cx, "tile-add-favorite"),
        "the invitation went with the drag"
    );
    assert_eq!(current(cx).pinned, vec![p("c")]);
    // A new drag starts clean and is invited again.
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        row.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    assert!(drawn(cx, "tile-add-favorite"));
    cx.simulate_mouse_up(row.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

/// A tile that turned into a list entry on its way out of the sidebar is put back on the tiles by
/// Undo, where the drag started; one let go where nothing takes it goes back there too.
#[gpui::test]
fn a_favorite_goes_back_where_its_drag_started(cx: &mut TestAppContext) {
    let (_ws, _shell, cx) = window_and_shell(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let tile = bounds(cx, "tile-slot-0");
    pick_up(cx, tile);
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_move(row.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(
        current(cx).pinned,
        vec![p("a"), p("c")],
        "a list entry on the way"
    );
    drop_outside(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("b")]));
    assert_eq!(current(cx).pinned, vec![p("c")]);
    let at = bounds(cx, "toast-notice-action-0").center();
    click(cx, at);
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("a"), p("b")]),
        "back on the tiles"
    );
    assert_eq!(current(cx).pinned, vec![p("c")]);

    // Over the list, then let go below it (This PC): back on the tiles.
    refresh(cx);
    let tile = bounds(cx, "tile-slot-0");
    pick_up(cx, tile);
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_move(row.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    let below = row.center() + point(px(0.), px(240.));
    cx.simulate_mouse_move(below, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    refresh(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("a"), p("b")]));
    assert_eq!(current(cx).pinned, vec![p("c")]);
}

/// Esc during a favorite drag (no drop at all): the favorites go back the way they were,
/// including where the drag moved them on the way.
#[gpui::test]
fn esc_puts_dragged_favorites_back(cx: &mut TestAppContext) {
    let (_ws, _shell, cx) = window_and_shell(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let tile = bounds(cx, "tile-slot-0");
    pick_up(cx, tile);
    let row = bounds(cx, "pin-row-0");
    cx.simulate_mouse_move(row.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    assert_eq!(current(cx).pinned, vec![p("a"), p("c")], "moved on the way");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    refresh(cx);
    assert_eq!(current(cx).favorites, Some(vec![p("a"), p("b")]));
    assert_eq!(current(cx).pinned, vec![p("c")]);
    cx.simulate_mouse_up(row.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        current(cx).favorites,
        Some(vec![p("a"), p("b")]),
        "nothing on release"
    );
    assert_eq!(current(cx).pinned, vec![p("c")]);
}

/// A folder's menu as drawn, from translation keys: `key` for a row, `key|shortcut` for a row
/// with its key on the right (`shortcut` a translation key too), `-` for a line between groups.
fn drawn_rows(keys: &[&str]) -> Vec<String> {
    keys.iter()
        .map(|k| match k.split_once('|') {
            _ if *k == "-" => "-".to_owned(),
            Some((label, key)) => format!("{} [{}]", t!(label), t!(key)),
            None => t!(*k).into_owned(),
        })
        .collect()
}

/// The folder actions after the favorites' rows, the same for every folder on this PC.
const FOLDER_TAIL: [&str; 5] = [
    "-",
    "menu.open_terminal",
    "menu.open_in_explorer",
    "menu.copy_path",
    "menu.properties",
];

/// A favorite has the same menu wherever it is, in the same order: a tile and a list row differ
/// only in where "Move" sends it, Home words that row for its own page, a folder that is no
/// favorite has the same rows without it, and Ctrl+click shows on "Open in new tab".
#[gpui::test]
fn folder_menus_match_wherever_the_folder_is(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let menu = |ws: &Entity<Workspace>,
                cx: &mut VisualTestContext,
                path: PathBuf,
                depth: Option<usize>,
                home: bool| {
        ws.update(cx, |ws, cx| {
            ws.folder_menu_drawn_for_test(&path, depth, home, cx)
        })
    };
    let open = ["sidebar.open_in_new_tab|menu.ctrl_click", "-"];
    let with = |rows: &[&str]| {
        let all: Vec<&str> = open
            .iter()
            .chain(rows)
            .chain(FOLDER_TAIL.iter())
            .copied()
            .collect();
        drawn_rows(&all)
    };
    let tile = with(&[
        "sidebar.remove_favorite",
        "sidebar.move_to_list",
        "menu.rename",
    ]);
    assert_eq!(menu(&ws, cx, p("a"), None, false), tile, "a tile");
    assert_eq!(
        menu(&ws, cx, p("c"), None, false),
        with(&[
            "sidebar.remove_favorite",
            "sidebar.move_to_tiles",
            "menu.rename",
        ]),
        "a list row"
    );
    assert_eq!(
        menu(&ws, cx, p("a"), None, true),
        with(&[
            "sidebar.remove_favorite",
            "home.move_to_sidebar_list",
            "menu.rename",
        ]),
        "a card on Home, which has no list below"
    );
    assert_eq!(
        menu(&ws, cx, p("d"), Some(1), false),
        with(&["sidebar.add_favorite"]),
        "a folder in the tree"
    );

    // A right click on the tile opens that menu (built then, not when the tile is drawn).
    let at = bounds(cx, "tile-slot-0").center();
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        ws.update(cx, |ws, cx| ws.open_menu_drawn_for_test(cx)),
        Some(tile)
    );
}

/// The tab menu shows its keys on the right, as the file list's menu does, instead of in the
/// label: Ctrl+W on the active tab only, where it works.
#[gpui::test]
fn the_tab_menu_shows_its_keys_on_the_right(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    // Home in a tab of its own, the active one.
    ws.update_in(cx, |ws, window, cx| ws.open_home_for_test(true, window, cx));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active), 1);
    let close = t!("sidebar.close_tab").into_owned();
    let active = ws.update(cx, |ws, cx| ws.tab_menu_drawn_for_test(1, cx));
    assert!(active.contains(&format!("{close} [Ctrl+W]")), "{active:?}");
    let other = ws.update(cx, |ws, cx| ws.tab_menu_drawn_for_test(0, cx));
    assert!(other.contains(&close), "{other:?}");
    assert!(!other.iter().any(|row| row.contains("(Ctrl")), "{other:?}");
}
