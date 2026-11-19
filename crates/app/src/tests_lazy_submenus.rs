//! GPUI tests for Windows submenus filled only when opened (`explorer_menu_lazy.rs`).

use super::*;
use crate::shell::ShellMenuEntry;

fn item(id: u32, label: &str) -> ShellMenuEntry {
    ShellMenuEntry::Item {
        id,
        verb: None,
        label: label.into(),
        enabled: true,
        checked: false,
        default: false,
        icon: None,
    }
}

/// Labels at `level` of the open menu, and how many submenus are open.
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "ContextMenu lives in a private explorer submodule"
)]
fn level(view: &Entity<Explorer>, cx: &mut VisualTestContext, at: usize) -> (usize, Vec<String>) {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .map_or_else(Default::default, |m| (m.depth(), m.labels_at(at)))
    })
}

/// Opens the empty-space menu and goes into "Show more options" with the keyboard, once the
/// Windows menu is in. Returns its session.
fn into_more_options(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> u64 {
    right_click(view, cx, None);
    wait_until(cx, view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    let more = format!("{} ›", t!("menu.more_options"));
    let slot = menu_labels(view, cx)
        .unwrap()
        .iter()
        .position(|l| *l == more)
        .unwrap();
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(slot);
        }
        cx.notify();
    });
    cx.simulate_keystrokes("right");
    view.update(cx, |v, _| v.context_menu.as_ref().unwrap().shell_state().1)
        .unwrap()
}

fn wait_level(view: &Entity<Explorer>, cx: &mut VisualTestContext, at: usize, want: &[String]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while level(view, cx, at).1 != want {
        assert!(
            Instant::now() < deadline,
            "level {at}: {:?}",
            level(view, cx, at).1
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A Windows submenu that is empty until it is opened ("Give access to") is kept, shows
/// "Loading…" until the menu host fills it, then lists its items, which run in the same Windows
/// menu. The rows around it stay put.
#[gpui::test]
fn a_submenu_filled_when_opened_lists_and_runs_its_items(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![
        (
            "Give access to".into(),
            vec![
                item(30, "Remove access"),
                ShellMenuEntry::Separator,
                item(31, "Specific people..."),
            ],
        ),
        ("Nothing inside".into(), Vec::new()),
    ];
    let (open_gate, gate) = flume::unbounded();
    *shell.fill_gate.lock() = Some(gate);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));

    let session = into_more_options(&view, cx);
    // Empty space: Arke's menu has no Open or Copy, so Windows' stay.
    let more_options = vec![
        "Open".to_owned(),
        "—".to_owned(),
        "Compress with 7-Zip".to_owned(),
        "—".to_owned(),
        "Send to ›".to_owned(),
        "—".to_owned(),
        "Copy".to_owned(),
        "Give access to ›".to_owned(),
        "Nothing inside ›".to_owned(),
    ];
    assert_eq!(level(&view, cx, 1), (1, more_options.clone()));
    let top = level(&view, cx, 0).1;
    // Nothing is filled before it is opened.
    assert!(shell.submenus_filled.lock().is_empty());

    // Up wraps to the last row, so up twice is "Give access to".
    cx.simulate_keystrokes("up up right");
    assert_eq!(
        level(&view, cx, 2),
        (2, vec![t!("menu.loading").into_owned()])
    );
    let _ = open_gate.send(());
    wait_level(
        &view,
        cx,
        2,
        &[
            "Remove access".to_owned(),
            "—".to_owned(),
            "Specific people...".to_owned(),
        ],
    );
    assert_eq!(*shell.submenus_filled.lock(), [(session, 0)]);
    // The menus above it didn't move.
    assert_eq!(level(&view, cx, 0).1, top);
    assert_eq!(level(&view, cx, 1).1, more_options);

    // The keyboard was already in it: its first item is selected, and runs in the Windows menu.
    cx.simulate_keystrokes("down enter");
    wait_shell("ran the filled submenu's item", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(session, 31)]);
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // A submenu that is still empty once filled says so.
    *shell.fill_gate.lock() = None;
    let session = into_more_options(&view, cx);
    cx.simulate_keystrokes("up right");
    wait_level(&view, cx, 2, &[t!("menu.submenu_empty").into_owned()]);
    assert!(shell.submenus_filled.lock().contains(&(session, 1)));
    // Opening it again doesn't ask again.
    cx.simulate_keystrokes("left right");
    cx.run_until_parked();
    assert_eq!(
        shell
            .submenus_filled
            .lock()
            .iter()
            .filter(|f| **f == (session, 1))
            .count(),
        1
    );
    cx.simulate_keystrokes("escape escape escape");
}

/// A submenu filled after its menu closed is dropped quietly.
#[gpui::test]
fn a_submenu_filled_after_the_menu_closed_is_dropped(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![("Give access to".into(), vec![item(30, "Remove access")])];
    let (open_gate, gate) = flume::unbounded();
    *shell.fill_gate.lock() = Some(gate);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let session = into_more_options(&view, cx);
    cx.simulate_keystrokes("up right");
    cx.simulate_keystrokes("escape escape escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(shell.shell_released.lock().contains(&session));
    let _ = open_gate.send(());
    wait_shell("filled", || !shell.submenus_filled.lock().is_empty());
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
}

fn lazy(label: &str, index: u32) -> ShellMenuEntry {
    ShellMenuEntry::Submenu {
        label: label.into(),
        enabled: true,
        icon: None,
        children: Vec::new(),
        lazy: Some(index),
    }
}

/// "Outer" holds a submenu filled on opening too ("Inner", lazy 1), which holds a command.
fn nested_shell() -> Arc<FakeShell> {
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![
        ("Outer".into(), vec![item(40, "Before"), lazy("Inner", 1)]),
        ("Inner".into(), vec![item(41, "Deep command")]),
    ];
    shell
}

fn pin(cx: &mut VisualTestContext, keys: &[&str]) {
    let keys: Vec<String> = keys.iter().map(ToString::to_string).collect();
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.menu_pinned = keys, cx);
        });
    });
}

fn rows_above_more_options(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<String> {
    let more_options = format!("{} ›", t!("menu.more_options"));
    let labels = menu_labels(view, cx).unwrap_or_default();
    let more = labels.iter().position(|l| *l == more_options).unwrap_or(0);
    labels[..more].to_vec()
}

fn wait_pinned(view: &Entity<Explorer>, cx: &mut VisualTestContext, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !rows_above_more_options(view, cx).contains(&label.to_owned()) {
        assert!(
            Instant::now() < deadline,
            "{label} not pinned: {:?}",
            menu_labels(view, cx)
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A submenu filled on opening that holds another one: both fill as they open, one inside the
/// other, and the inner one's command runs in the Windows menu.
#[gpui::test]
fn a_submenu_filled_on_opening_inside_another_fills_too(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = nested_shell();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let session = into_more_options(&view, cx);
    // Up twice from the top wraps to "Outer" (the last two rows are Outer, Inner).
    cx.simulate_keystrokes("up up right");
    wait_level(&view, cx, 2, &["Before".to_owned(), "Inner ›".to_owned()]);
    cx.simulate_keystrokes("down right");
    wait_level(&view, cx, 3, &["Deep command".to_owned()]);
    assert_eq!(*shell.submenus_filled.lock(), [(session, 0), (session, 1)]);
    cx.simulate_keystrokes("enter");
    wait_shell("ran the inner command", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(session, 41)]);
}

/// The pointer passing over submenu rows fills none of them; the one it rests on fills as it
/// opens (after the hover delay).
#[gpui::test]
fn hovering_fills_only_the_submenu_that_opens(cx: &mut TestAppContext) {
    use gpui::Modifiers;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![
        ("Give access to".into(), vec![item(30, "Remove access")]),
        ("Nothing inside".into(), Vec::new()),
    ];
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let session = into_more_options(&view, cx);
    cx.run_until_parked();
    let rows = level(&view, cx, 1).1;
    let at = |label: &str| rows.iter().position(|l| l == label).unwrap();
    let (give, nothing) = (at("Give access to ›"), at("Nothing inside ›"));
    let row = |view: &Entity<Explorer>, cx: &mut VisualTestContext, ix| {
        view.update(cx, |v, _| {
            v.context_menu
                .as_ref()
                .and_then(|m| m.submenu_row_for_test(1, ix))
        })
        .expect("row drawn")
        .center()
    };
    let (over_give, over_nothing) = (row(&view, cx, give), row(&view, cx, nothing));
    // Across "Give access to" on the way to "Nothing inside", without resting on it (in the
    // window "Show more options" is drawn in).
    let (more, _) = view
        .update(cx, |v, _| {
            v.context_menu.as_ref().unwrap().popup_for_test(1)
        })
        .expect("level 1 has a window");
    let mut more = VisualTestContext::from_window(more, &cx.cx);
    more.simulate_mouse_move(over_give, None, Modifiers::none());
    more.simulate_mouse_move(over_nothing, None, Modifiers::none());
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        shell.submenus_filled.lock().is_empty(),
        "nothing fills before it opens"
    );
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    wait_level(&view, cx, 2, &[t!("menu.submenu_empty").into_owned()]);
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    assert_eq!(*shell.submenus_filled.lock(), [(session, 1)]);
}

/// A command pinned from inside a submenu filled on opening still shows up top the next time
/// the menu opens: that submenu is filled as soon as the menu is read, without being opened. It
/// works the same on an item's menu, whose submenus used to be waited for while it was read,
/// and through a submenu filled inside another one.
#[gpui::test]
fn commands_pinned_inside_submenus_filled_on_opening_show_up_top(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = nested_shell();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));

    // Pinned from the filled submenu, as with the pin button.
    let session = into_more_options(&view, cx);
    cx.simulate_keystrokes("up up right");
    wait_level(&view, cx, 2, &["Before".to_owned(), "Inner ›".to_owned()]);
    view.update(cx, |v, cx| v.toggle_shell_pin("Outer › Before", cx));
    assert!(rows_above_more_options(&view, cx).contains(&"Before".to_owned()));
    cx.simulate_keystrokes("escape escape escape");
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    shell.submenus_filled.lock().clear();

    // Next time, on empty space: there without opening anything.
    right_click(&view, cx, None);
    wait_pinned(&view, cx, "Before");
    assert_eq!(Some(level(&view, cx, 0).0), Some(0));
    assert!(
        shell.submenus_filled.lock().iter().all(|(_, ix)| *ix == 0),
        "only the submenu with the pin: {:?}",
        shell.submenus_filled.lock()
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // An item's menu, and a command two submenus down.
    pin(cx, &["Outer › Inner › Deep command"]);
    shell.submenus_filled.lock().clear();
    right_click(&view, cx, Some(0));
    wait_pinned(&view, cx, "Deep command");
    let filled = shell.submenus_filled.lock().clone();
    assert_eq!(filled.iter().map(|(_, ix)| *ix).collect::<Vec<_>>(), [0, 1]);
    // It runs in the Windows menu, like any pinned command.
    let pinned = rows_above_more_options(&view, cx)
        .iter()
        .position(|l| l == "Deep command")
        .unwrap();
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(pinned);
        }
        cx.notify();
    });
    cx.simulate_keystrokes("enter");
    wait_shell("ran the pinned command", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(shell.shell_invoked.lock()[0].1, 41);
    let _ = session;
}

/// A command picked while a submenu is still being filled runs at once, without waiting for
/// the fill.
#[gpui::test]
fn a_command_runs_while_a_submenu_is_still_filling(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![("Give access to".into(), vec![item(30, "Remove access")])];
    let (open_gate, gate) = flume::unbounded();
    *shell.fill_gate.lock() = Some(gate);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let session = into_more_options(&view, cx);
    // "Give access to" opens and stays "Loading…" (its fill is held up).
    cx.simulate_keystrokes("up right");
    assert_eq!(
        level(&view, cx, 2),
        (2, vec![t!("menu.loading").into_owned()])
    );
    // Back out and run "Compress with 7-Zip", a few rows up.
    cx.simulate_keystrokes("left");
    let rows = level(&view, cx, 1).1;
    let at = |label: &str| rows.iter().position(|l| l == label).unwrap();
    let ups = rows[at("Compress with 7-Zip")..at("Give access to ›")]
        .iter()
        .filter(|l| *l != "—")
        .count();
    cx.simulate_keystrokes(&vec!["up"; ups].join(" "));
    cx.simulate_keystrokes("enter");
    wait_shell("ran the command", || !shell.shell_invoked.lock().is_empty());
    assert_eq!(*shell.shell_invoked.lock(), [(session, 1)]);
    assert!(
        shell.submenus_filled.lock().is_empty(),
        "the fill still waits"
    );
    let _ = open_gate.send(());
}
