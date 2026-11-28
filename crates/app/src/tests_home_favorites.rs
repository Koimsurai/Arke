//! GPUI tests for the favorites on the Home page (#173): the same list, order and names as the
//! sidebar tiles, following edits made there.

use super::*;
use crate::settings::{Settings, SettingsStore};

fn p(name: &str) -> PathBuf {
    PathBuf::from(r"C:\w").join(name)
}

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut Settings) + 'static) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn listed(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<(PathBuf, String)> {
    ws.update(cx, |ws, cx| ws.home_favorites_for_test(cx))
        .expect("Home is open")
}

/// Home lists the sidebar's favorites in its order with the names given to them, and follows
/// edits made in the sidebar without reopening.
#[gpui::test]
fn home_lists_the_sidebar_favorites(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let names = ["a", "b", "c"];
    fs.insert_dir(r"C:\w", names.iter().map(|n| dir(n)).collect());
    for name in names {
        fs.insert_dir(p(name), Vec::new());
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    change(cx, |st| {
        st.favorites = Some(vec![p("c"), p("a")]);
        st.favorite_names
            .insert(crate::place_names::key(&p("a")), "Alpha".to_owned());
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    refresh(cx);
    assert_eq!(
        listed(&ws, cx),
        [(p("c"), "c".to_owned()), (p("a"), "Alpha".to_owned())],
        "same order, same custom names"
    );

    // Editing the favorites (as the sidebar does) shows on the open page.
    change(cx, |st| st.favorites = Some(vec![p("b")]));
    refresh(cx);
    assert_eq!(listed(&ws, cx), [(p("b"), "b".to_owned())]);

    change(cx, |st| st.favorites = Some(Vec::new()));
    refresh(cx);
    assert!(listed(&ws, cx).is_empty(), "no favorites, no cards");
}

/// A favorite card dragged over another on Home takes its place, saved as the sidebar's order.
#[gpui::test]
fn home_favorites_reorder_by_drag(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    let names = ["a", "b", "c"];
    fs.insert_dir(r"C:\w", names.iter().map(|n| dir(n)).collect());
    for name in names {
        fs.insert_dir(p(name), Vec::new());
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    change(cx, |st| st.favorites = Some(vec![p("a"), p("b"), p("c")]));
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    refresh(cx);
    let card = |cx: &mut VisualTestContext, name: &str| {
        let selector: &'static str = format!("home-favorite {name}").leak();
        cx.debug_bounds(selector).expect("card drawn")
    };
    let a = card(cx, "a");
    let c = card(cx, "c");
    cx.simulate_mouse_down(a.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        a.center() + point(px(8.), px(2.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    cx.simulate_mouse_move(c.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    cx.simulate_mouse_up(c.center(), MouseButton::Left, Modifiers::none());
    refresh(cx);
    let order = cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .favorites
            .clone()
    });
    assert_eq!(order, Some(vec![p("b"), p("c"), p("a")]));
    let shown: Vec<PathBuf> = listed(&ws, cx).into_iter().map(|(p, _)| p).collect();
    assert_eq!(shown, [p("b"), p("c"), p("a")], "Home shows the new order");
}

/// A click on a Home card follows "Single click to open" like the recent files: with it off, one
/// click only picks the card and a double click opens it; with it on, one click opens (#426).
#[gpui::test]
fn home_cards_follow_the_single_click_setting(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\w", vec![dir("a")]);
    fs.insert_dir(p("a"), Vec::new());
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    change(cx, |st| st.favorites = Some(vec![p("a")]));
    let click = |cx: &mut VisualTestContext, click_count: usize| {
        let at = cx
            .debug_bounds("home-favorite a")
            .expect("card drawn")
            .center();
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count,
        });
        cx.run_until_parked();
    };
    let home_open = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update(cx, |ws, cx| ws.home_favorites_for_test(cx))
            .is_some()
    };
    let open_home = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update_in(cx, |ws, window, cx| {
            ws.open_home_for_test(false, window, cx);
        });
        refresh(cx);
        assert!(home_open(ws, cx), "Home is open");
    };

    crate::prefs::apply(&Settings::default());
    open_home(&ws, cx);
    click(cx, 1);
    assert!(
        home_open(&ws, cx),
        "double-click mode: one click doesn't open"
    );
    refresh(cx);
    click(cx, 1);
    click(cx, 2);
    assert!(
        !home_open(&ws, cx),
        "double-click mode: a double click opens"
    );
    wait_until(cx, &view, "folder", loaded(r"C:\w\a"));

    crate::prefs::apply(&Settings {
        single_click: true,
        ..Settings::default()
    });
    open_home(&ws, cx);
    click(cx, 1);
    assert!(!home_open(&ws, cx), "single-click mode: one click opens");
    wait_until(cx, &view, "folder", loaded(r"C:\w\a"));
    crate::prefs::apply(&Settings::default());
}

/// Hiding the sidebar's Favorites section takes Home's favorite cards with it (#539), showing it
/// again brings them back, and collapsing it keeps them.
#[gpui::test]
fn home_follows_hidden_favorites(cx: &mut TestAppContext) {
    use crate::settings::SidebarSection;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\w", vec![dir("a")]);
    fs.insert_dir(p("a"), Vec::new());
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    change(cx, |st| st.favorites = Some(vec![p("a")]));
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    refresh(cx);
    let card_drawn = |cx: &mut VisualTestContext| cx.debug_bounds("home-favorite a").is_some();
    assert!(card_drawn(cx), "the card is shown");

    change(cx, |st| st.set_hidden(SidebarSection::Favorites, true));
    refresh(cx);
    assert!(!card_drawn(cx), "hidden in the sidebar: no card on Home");

    change(cx, |st| st.set_hidden(SidebarSection::Favorites, false));
    refresh(cx);
    assert!(card_drawn(cx), "shown again: the card is back");

    change(cx, |st| st.toggle_collapsed(SidebarSection::Favorites));
    refresh(cx);
    assert!(card_drawn(cx), "collapsed is not hidden: the card stays");
}
