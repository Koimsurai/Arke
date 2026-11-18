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
