//! GPUI tests for the favorites at the top of the sidebar (`workspace_sidebar.rs`): the remove
//! target while one is dragged, long names listed as rows instead of cut off (#174), names of
//! their own (#259), and where the Recycle Bin goes when it leaves the sidebar (#31).

use gpui::{Modifiers, MouseButton, point, px};

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

/// A window on `C:\w`, whose folders `a` to `d` and [`LONG`] can be favorites.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    let names = ["a", "b", "c", "d", LONG];
    fs.insert_dir(r"C:\w", names.iter().map(|n| dir(n)).collect());
    for name in names {
        fs.insert_dir(p(name), Vec::new());
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    refresh(cx);
    (ws, cx)
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

/// While a favorite is dragged, a remove target shows over the bottom of the places; dropping it
/// there takes it off the favorites, and the target goes away with the drag.
#[gpui::test]
fn dropping_a_favorite_on_the_remove_target_removes_it(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b"), p("c"), p("d")]);
    });
    refresh(cx);
    assert!(!drawn(cx, "favorite-remove"), "only while dragging");

    let first = cx.debug_bounds("tile-slot-0").expect("the tile is drawn");
    cx.simulate_mouse_down(first.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        first.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    cx.update(|_, cx| assert!(cx.has_active_drag()));
    let target = cx
        .debug_bounds("favorite-remove")
        .expect("a remove target while dragging")
        .center();
    assert!(
        target.y > first.bottom(),
        "it doesn't cover the tiles being dragged among"
    );
    cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(!cx.has_active_drag()));
    assert_eq!(current(cx).favorites, Some(vec![p("b"), p("c"), p("d")]));
    refresh(cx);
    assert!(!drawn(cx, "favorite-remove"));
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
