//! GPUI tests for pinned Windows commands that don't wait for the whole Windows menu
//! (`explorer_menu_parts.rs`, #129).

use super::*;
use crate::shell::ShellMenuEntry;
use crate::win_menu_parts::MenuHandler;

fn seven_zip() -> MenuHandler {
    MenuHandler {
        key: "*".into(),
        clsid: "{23170F69-40C1-278A-1000-000100020000}".into(),
        name: "7-Zip".into(),
    }
}

/// A fake shell with two extensions: 7-Zip (whose menu has the pinned command, under its own id
/// 9) and one with nothing pinned.
fn shell_with_extensions() -> Arc<FakeShell> {
    let shell = Arc::new(FakeShell::default());
    let item = |id, label: &str| ShellMenuEntry::Item {
        id,
        verb: None,
        label: label.into(),
        enabled: true,
        checked: false,
        default: false,
        icon: None,
    };
    *shell.extensions.lock() = vec![
        (seven_zip(), vec![item(9, "Compress with 7-Zip")]),
        (
            MenuHandler {
                key: "*".into(),
                clsid: "{00000000-0000-0000-0000-000000000001}".into(),
                name: "Other".into(),
            },
            vec![item(3, "Something else")],
        ),
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

fn owners(cx: &mut VisualTestContext) -> std::collections::BTreeMap<String, String> {
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pin_owners
            .clone()
    })
}

fn rows_above_more_options(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<String> {
    let more_options = format!("{} ›", t!("menu.more_options"));
    let labels = menu_labels(view, cx).unwrap_or_default();
    let more = labels.iter().position(|l| *l == more_options).unwrap_or(0);
    labels[..more].to_vec()
}

/// The first menu with a pinned command learns, in the background, which extension it comes
/// from (and that a command Windows registers itself comes from none). From then on the pinned
/// command shows while the whole Windows menu is still being read, and runs in its extension's
/// own menu.
#[gpui::test]
fn a_pinned_command_shows_before_the_whole_windows_menu(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &["Compress with 7-Zip", "Send to › Desktop", "Missing"]);

    // First menu: nothing known yet, so it waits for the whole menu, then learns.
    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while owners(cx).len() < 2 {
        assert!(Instant::now() < deadline, "owners not learned");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let learned = owners(cx);
    assert_eq!(
        learned.get("Compress with 7-Zip").map(String::as_str),
        Some(seven_zip().clsid.as_str())
    );
    // In the whole menu but from no extension: Windows' own.
    assert_eq!(
        learned.get("Send to › Desktop").map(String::as_str),
        Some("")
    );
    // Not in this menu at all: left to learn when it shows.
    assert!(!learned.contains_key("Missing"));
    // The extension menus read to learn were released at once.
    wait_shell("learning released", || {
        let released = shell.shell_released.lock();
        released.contains(&100) && released.contains(&101)
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Next menu, with the whole Windows menu slow: the pinned command shows from 7-Zip alone.
    let (open_gate, gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(gate);
    shell.extensions_read.lock().clear();
    right_click(&view, cx, Some(0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !rows_above_more_options(&view, cx).contains(&"Compress with 7-Zip".to_owned()) {
        assert!(Instant::now() < deadline, "pinned row didn't show early");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        view.update(cx, |v, _| v
            .context_menu
            .as_ref()
            .is_some_and(|m| !m.shell_state().0)),
        "the whole menu is still loading"
    );
    // Only the extension with a pinned command was read.
    assert_eq!(*shell.extensions_read.lock(), ["7-Zip"]);
    let rows = rows_above_more_options(&view, cx);
    assert!(!rows.contains(&"Desktop".to_owned()), "{rows:?}");

    // Choosing it runs it in 7-Zip's own menu; the whole menu is released when it comes.
    let at = rows
        .iter()
        .position(|r| r == "Compress with 7-Zip")
        .unwrap();
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(at);
        }
        cx.notify();
    });
    cx.simulate_keystrokes("enter");
    wait_shell("ran in 7-Zip's menu", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(100, 9)]);
    let _ = open_gate.send(());
    *shell.menu_gate.lock() = None;
    wait_shell("whole menu released", || {
        shell
            .shell_released
            .lock()
            .iter()
            .any(|&s| (7..100).contains(&s))
    });
}

/// Unpinning forgets which extension the command came from.
#[gpui::test]
fn unpinning_forgets_the_extension(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &["Compress with 7-Zip"]);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.menu_pin_owners
                        .insert("Compress with 7-Zip".into(), seven_zip().clsid);
                },
                cx,
            );
        });
    });
    view.update(cx, |v, cx| v.toggle_shell_pin("Compress with 7-Zip", cx));
    assert!(owners(cx).is_empty());
}
