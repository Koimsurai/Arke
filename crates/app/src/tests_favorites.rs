//! GPUI tests for the favorites at the top of the sidebar (`workspace_sidebar.rs`): a tile dragged
//! like an app icon (lifted under the pointer, an empty slot the others slide around, settling
//! back when dropped), removal by dragging one out of the sidebar (with Undo), long names listed
//! as rows instead of cut off (#174), names of their own (#259), and where the Recycle Bin goes
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

/// #174: a name that doesn't fit on a tile is listed as a row under the tiles instead of being cut
/// off. It stays where it was among the favorites, so a shorter name puts it back on its tile.
#[gpui::test]
fn long_names_are_listed_instead_of_cut_off(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| st.favorites = Some(vec![p("a"), p(LONG), p("b")]));
    refresh(cx);
    assert!(drawn(cx, "tile-slot-0"));
    assert!(!drawn(cx, "tile-slot-1"), "no tile with a cut-off name");
    assert!(drawn(cx, "tile-slot-2"), "the others keep their slots");
    assert!(drawn(cx, "favorite-row-1"), "listed as a row instead");
    assert!(drawn(cx, &format!("sidebar {LONG}")));
    assert!(drawn(cx, "pin-list-label"), "under the Favorites heading");
    let settings = current(cx);
    assert_eq!(
        settings.favorites,
        Some(vec![p("a"), p(LONG), p("b")]),
        "the saved order doesn't change"
    );
    assert!(settings.pinned.is_empty());

    rename(&ws, cx, &p(LONG), "Board");
    assert!(drawn(cx, "tile-slot-1"), "back on its own tile");
    assert!(drawn(cx, "tile Board"));
    assert!(!drawn(cx, "favorite-row-1"));
}

/// With a favorite listed as a row, dragging a tile onto another still reorders them live and the
/// tiles slide over from their old slots; the row keeps its place in the saved order.
#[gpui::test]
fn tiles_slide_when_a_favorite_is_a_row(cx: &mut TestAppContext) {
    crate::motion::force_for_test(true);
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p(LONG), p("b"), p("c")]);
    });
    refresh(cx);
    assert!(drawn(cx, "favorite-row-1"), "the long name is a row");
    let first = cx.debug_bounds("tile-slot-0").expect("a is a tile");
    let last = cx.debug_bounds("tile-slot-3").expect("c is a tile");

    cx.simulate_mouse_down(first.center(), MouseButton::Left, Modifiers::none());
    // Two moves within the first tile: the drag starts, then every tile reports where it is.
    for nudge in [6., 8.] {
        cx.simulate_mouse_move(
            first.center() + point(px(nudge), px(4.)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        refresh(cx);
    }
    cx.update(|_, cx| assert!(cx.has_active_drag()));

    cx.simulate_mouse_move(last.center(), Some(MouseButton::Left), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        current(cx).favorites,
        Some(vec![p(LONG), p("b"), p("c"), p("a")]),
        "a takes c's slot; the row stays ahead of them"
    );
    assert_eq!(
        ws.update(cx, |ws, _| ws.tiles_sliding_for_test()),
        vec![p("a"), p("b"), p("c")],
        "every tile slides from its old slot"
    );
    cx.simulate_mouse_up(last.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    crate::motion::force_for_test(false);
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
