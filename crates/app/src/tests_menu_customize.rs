//! GPUI tests for the context menu as customized in Settings › Context menu: the shorter New
//! entries (#190) and the order dragged to (#263).

use gpui::{Bounds, Modifiers, MouseButton, Pixels, point, px, size};

use super::*;
use crate::menu_order::{DEFAULT, moved};
use crate::settings::{Settings, SettingsStore};

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

/// Where the element with debug selector `selector` was drawn.
fn drawn(cx: &mut VisualTestContext, selector: &str) -> Bounds<Pixels> {
    let selector: &'static str = selector.to_owned().leak();
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} drawn"))
}

/// Drags the element drawn at `from` to the middle of `to`, as the mouse does.
fn drag(cx: &mut VisualTestContext, from: Bounds<Pixels>, to: Bounds<Pixels>) {
    cx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        from.center() + point(px(8.), px(-2.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    cx.simulate_mouse_move(to.center(), Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    cx.simulate_mouse_up(to.center(), MouseButton::Left, Modifiers::none());
    refresh(cx);
}

/// `DEFAULT` with `key` moved to the top, as stored.
fn first(key: &str) -> Vec<String> {
    let at = DEFAULT.iter().position(|k| *k == key).unwrap();
    moved(&DEFAULT, at, 0)
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

/// Highlights top-level row `ix` of the open menu, as the keyboard would.
fn highlight(view: &Entity<Explorer>, cx: &mut VisualTestContext, ix: usize) {
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(ix);
        }
        cx.notify();
    });
}

fn labels_at(view: &Entity<Explorer>, cx: &mut VisualTestContext, level: usize) -> Vec<String> {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .map_or_else(Vec::new, |m| m.labels_at(level))
    })
}

/// A folder's menu has "New file…" and "New folder…" in "New", reached with the keyboard too,
/// and New folder as a button in the icon row; a file's menu has neither. "Move to new folder"
/// can be turned off (#190).
#[gpui::test]
fn the_new_entries_are_grouped_and_move_to_new_folder_can_be_hidden(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("docs"), file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    let new = format!("{} ›", t!("menu.new"));
    let move_to = t!("menu.move_to_new_folder").into_owned();
    let icons = |first: Option<String>| {
        let mut names: Vec<String> = first.into_iter().collect();
        names.extend(
            ["menu.cut", "menu.copy", "menu.rename", "menu.delete"].map(|k| t!(k).into_owned()),
        );
        format!("[{}]", names.join(" "))
    };

    // docs, a folder.
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(labels.contains(&new), "{labels:?}");
    assert!(
        !labels.contains(&t!("menu.new_folder_in").into_owned()),
        "{labels:?}"
    );
    assert!(labels.contains(&move_to), "{labels:?}");
    assert_eq!(
        labels.last(),
        Some(&icons(Some(t!("menu.new_folder").into_owned())))
    );
    let at = labels.iter().position(|l| *l == new).unwrap();
    highlight(&view, cx, at);
    cx.simulate_keystrokes("right");
    assert_eq!(
        labels_at(&view, cx, 1),
        [
            t!("menu.new_file_in").into_owned(),
            t!("menu.new_folder_in").into_owned()
        ]
    );
    cx.simulate_keystrokes("escape escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // a.txt, a file.
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(!labels.contains(&new), "{labels:?}");
    assert!(labels.contains(&move_to), "{labels:?}");
    assert_eq!(labels.last(), Some(&icons(None)));
    cx.simulate_keystrokes("escape");

    change(cx, |st| st.menu_hidden = vec!["move_new_folder".into()]);
    for row in [0, 1] {
        right_click(&view, cx, Some(row));
        let labels = menu_labels(&view, cx).unwrap();
        assert!(!labels.contains(&move_to), "{labels:?}");
        cx.simulate_keystrokes("escape");
    }
}

/// Settings › Context menu: a row dragged onto another takes its place, the list and the
/// preview follow at once, and only a changed order is stored. Reset puts it back (#263).
#[gpui::test]
fn menu_entries_reorder_by_drag_in_settings(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.simulate_resize(size(px(1400.), px(3000.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.context_menu"), cx);
    });
    refresh(cx);
    let above = |cx: &mut VisualTestContext, what: &str, a: &str, b: &str| {
        drawn(cx, &format!("{what} {a}")).top() < drawn(cx, &format!("{what} {b}")).top()
    };
    assert!(above(cx, "menu-row", "open", "properties"));
    assert!(above(cx, "menu-preview", "open", "properties"));

    // Properties dragged up onto Open.
    let (from, to) = (drawn(cx, "menu-row properties"), drawn(cx, "menu-row open"));
    drag(cx, from, to);
    assert_eq!(settings(cx).menu_order, first("properties"));
    assert!(above(cx, "menu-row", "properties", "open"));
    assert!(above(cx, "menu-preview", "properties", "open"));
    assert!(above(cx, "menu-preview", "open", "new_tab"), "the rest stay");

    // Dragged back down onto the row it came after: the built-in order again, so nothing is
    // stored.
    let (from, to) = (drawn(cx, "menu-row properties"), drawn(cx, "menu-row reveal"));
    drag(cx, from, to);
    assert!(settings(cx).menu_order.is_empty());

    // Pinned Windows commands are reordered among themselves the same way.
    change(cx, |st| {
        st.menu_pinned = vec!["Send to › Desktop".into(), "Compress with 7-Zip".into()];
    });
    refresh(cx);
    let (from, to) = (
        drawn(cx, "menu-pinned Compress with 7-Zip"),
        drawn(cx, "menu-pinned Send to › Desktop"),
    );
    drag(cx, from, to);
    assert_eq!(
        settings(cx).menu_pinned,
        ["Compress with 7-Zip", "Send to › Desktop"]
    );
    assert!(settings(cx).menu_order.is_empty(), "the entries stay put");

    // Reset brings back the order and the entries turned off.
    change(cx, |st| {
        st.menu_order = first("more_options");
        st.menu_hidden = vec!["shelf".into()];
    });
    refresh(cx);
    assert!(above(cx, "menu-preview", "more_options", "open"));
    let reset = drawn(cx, "menu-pref-reset");
    cx.simulate_click(reset.center(), Modifiers::none());
    refresh(cx);
    let st = settings(cx);
    assert!(st.menu_order.is_empty() && st.menu_hidden.is_empty());
    assert!(above(cx, "menu-preview", "open", "more_options"));
}

/// The menus follow the order set in Settings: an entry moved to the top comes first in a
/// folder's and a file's menu, joining Open's group. Unknown keys are ignored and entries
/// turned off stay off (#263).
#[gpui::test]
fn the_menu_follows_the_order_set_in_settings(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("docs"), file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    let properties = t!("menu.properties").into_owned();
    let open = t!("menu.open").into_owned();
    change(cx, |st| st.menu_order = first("properties"));

    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(
        labels[..4],
        [
            properties.clone(),
            open.clone(),
            t!("menu.open_in_new_tab").into_owned(),
            "—".to_owned()
        ]
    );
    assert_eq!(
        labels.iter().filter(|l| **l == properties).count(),
        1,
        "{labels:?}"
    );
    assert!(labels.last().is_some_and(|l| l.starts_with('[')), "{labels:?}");
    cx.simulate_keystrokes("escape");

    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(labels[..3], [properties.clone(), open.clone(), "—".to_owned()]);
    cx.simulate_keystrokes("escape");

    change(cx, |st| {
        st.menu_order.insert(0, "from a later version".into());
        st.menu_hidden = vec!["properties".into()];
    });
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(labels.first(), Some(&open));
    assert!(!labels.contains(&properties), "{labels:?}");
    cx.simulate_keystrokes("escape");
}
