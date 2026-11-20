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
