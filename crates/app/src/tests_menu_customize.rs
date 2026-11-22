//! GPUI tests for the context menu as customized in Settings › Context menu: the shorter New
//! entries (#190), the order dragged to (#263) and whole Windows submenus pinned to the main
//! menu (#88).

use gpui::{Bounds, Modifiers, MouseButton, Pixels, point, px, size};

use super::*;
use crate::menu_order::{DEFAULT, moved};
use crate::settings::{Settings, SettingsStore};
use crate::shell::ShellMenuEntry;

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
    assert!(
        above(cx, "menu-preview", "open", "new_tab"),
        "the rest stay"
    );

    // Dragged back down onto the row it came after: the built-in order again, so nothing is
    // stored.
    let (from, to) = (
        drawn(cx, "menu-row properties"),
        drawn(cx, "menu-row reveal"),
    );
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
    assert!(
        labels.last().is_some_and(|l| l.starts_with('[')),
        "{labels:?}"
    );
    cx.simulate_keystrokes("escape");

    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(
        labels[..3],
        [properties.clone(), open.clone(), "—".to_owned()]
    );
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

fn wait_more_options(view: &Entity<Explorer>, cx: &mut VisualTestContext) {
    wait_until(cx, view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
}

/// Index of the top-level row labeled `label`.
fn row_of(view: &Entity<Explorer>, cx: &mut VisualTestContext, label: &str) -> usize {
    let labels = menu_labels(view, cx).unwrap_or_default();
    labels
        .iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("{label} in {labels:?}"))
}

fn wait_level(view: &Entity<Explorer>, cx: &mut VisualTestContext, level: usize, want: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while labels_at(view, cx, level) != want {
        assert!(
            Instant::now() < deadline,
            "level {level}: {:?}",
            labels_at(view, cx, level)
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A whole Windows submenu under "More options" has a pin button, as its commands do; pinned, it
/// shows right above "More options" as a submenu, and its rows run in the Windows menu. It is
/// there again in the next menu, and unpinning takes it away (#88).
#[gpui::test]
fn a_whole_windows_submenu_can_be_pinned(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let more = format!("{} ›", t!("menu.more_options"));

    right_click(&view, cx, Some(0));
    wait_more_options(&view, cx);
    let slot = row_of(&view, cx, &more);
    highlight(&view, cx, slot);
    cx.simulate_keystrokes("right");
    let rows = labels_at(&view, cx, 1);
    let send_to = rows.iter().position(|l| l == "Send to ›").unwrap();
    let pin = view.update(cx, |v, _| {
        v.context_menu.as_ref().and_then(|m| m.pin_at(1, send_to))
    });
    assert_eq!(pin.as_deref(), Some("Send to"));

    // Its pin button.
    view.update(cx, |v, cx| v.toggle_shell_pin("Send to", cx));
    cx.simulate_keystrokes("escape");
    let at = row_of(&view, cx, &more) - 1;
    assert_eq!(menu_labels(&view, cx).unwrap()[at], "Send to ›");
    highlight(&view, cx, at);
    cx.simulate_keystrokes("right");
    assert_eq!(labels_at(&view, cx, 1), ["Desktop", "Mail recipient"]);
    let session = view
        .update(cx, |v, _| v.context_menu.as_ref().unwrap().shell_state().1)
        .unwrap();
    cx.simulate_keystrokes("down enter");
    wait_shell("ran the pinned submenu's command", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(session, 3)]);
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    right_click(&view, cx, Some(0));
    wait_more_options(&view, cx);
    let at = row_of(&view, cx, &more) - 1;
    assert_eq!(menu_labels(&view, cx).unwrap()[at], "Send to ›");
    view.update(cx, |v, cx| v.toggle_shell_pin("Send to", cx));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(!labels.contains(&"Send to ›".to_owned()), "{labels:?}");
    assert!(settings(cx).menu_pinned.is_empty());
    cx.simulate_keystrokes("escape");
}

/// A pinned submenu that Windows fills only when it is opened fills as it opens up top, once: the
/// same submenu under "More options" isn't asked again. The keyboard, already in it, gets its
/// first row as it comes in (#88).
#[gpui::test]
fn a_pinned_submenu_filled_on_opening_fills_once(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![(
        "Give access to".into(),
        vec![ShellMenuEntry::Item {
            id: 30,
            verb: None,
            label: "Remove access".into(),
            enabled: true,
            checked: false,
            default: false,
            icon: None,
        }],
    )];
    let (open_gate, gate) = flume::unbounded();
    *shell.fill_gate.lock() = Some(gate);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    change(cx, |st| st.menu_pinned = vec!["Give access to".into()]);
    let more = format!("{} ›", t!("menu.more_options"));

    right_click(&view, cx, None);
    wait_more_options(&view, cx);
    let at = row_of(&view, cx, "Give access to ›");
    assert_eq!(at + 1, row_of(&view, cx, &more));
    assert!(
        shell.submenus_filled.lock().is_empty(),
        "nothing fills before it opens"
    );
    highlight(&view, cx, at);
    cx.simulate_keystrokes("right");
    assert_eq!(labels_at(&view, cx, 1), [t!("menu.loading").into_owned()]);
    let _ = open_gate.send(());
    wait_level(&view, cx, 1, &["Remove access"]);
    let session = view
        .update(cx, |v, _| v.context_menu.as_ref().unwrap().shell_state().1)
        .unwrap();
    assert_eq!(*shell.submenus_filled.lock(), [(session, 0)]);

    // Under "More options" it is filled already.
    cx.simulate_keystrokes("left");
    let more_at = row_of(&view, cx, &more);
    highlight(&view, cx, more_at);
    cx.simulate_keystrokes("right");
    let give = labels_at(&view, cx, 1)
        .iter()
        .position(|l| l == "Give access to ›")
        .unwrap();
    highlight(&view, cx, give);
    cx.simulate_keystrokes("right");
    assert_eq!(labels_at(&view, cx, 2), ["Remove access"]);
    cx.run_until_parked();
    assert_eq!(shell.submenus_filled.lock().len(), 1);
    cx.simulate_keystrokes("left left");

    // Back up top: the keyboard goes in and the first row runs in the Windows menu.
    highlight(&view, cx, at);
    cx.simulate_keystrokes("right enter");
    wait_shell("ran the pinned submenu's command", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(session, 30)]);
}
