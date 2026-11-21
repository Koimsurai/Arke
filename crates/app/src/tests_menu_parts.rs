//! GPUI tests for pinned Windows commands that don't wait for the whole Windows menu
//! (`explorer_menu_parts.rs`, #129, #278).

use super::*;
use crate::shell::ShellMenuEntry;
use crate::win_menu_parts::{HandlerKind, MenuHandler};

fn seven_zip() -> MenuHandler {
    MenuHandler {
        key: "*".into(),
        clsid: "{23170F69-40C1-278A-1000-000100020000}".into(),
        name: "7-Zip".into(),
        kind: HandlerKind::Extension,
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
                kind: HandlerKind::Extension,
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
    // In the whole menu but from no source read on its own: left to the whole menu.
    assert_eq!(
        learned.get("Send to › Desktop").map(String::as_str),
        Some("-")
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

/// Shift held at the right click: the Windows menu is read with the commands registered to show
/// only then (#272), as in File Explorer; without Shift, not.
#[gpui::test]
fn shift_right_click_reads_the_extended_windows_menu(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\t");
    wait_until(cx, &view, "load", loaded(r"C:\t"));

    right_click(&view, cx, None);
    wait_shell("plain menu", || shell.menu_extended.lock().len() == 1);
    cx.simulate_keystrokes("escape");
    cx.simulate_modifiers_change(gpui::Modifiers::shift());
    right_click(&view, cx, None);
    wait_shell("extended menu", || shell.menu_extended.lock().len() == 2);
    assert_eq!(*shell.menu_extended.lock(), [false, true]);
}

const TERMINAL: &str = "{9F156763-7844-4DC4-B2B1-901F640F5155}";

/// A registry command (Git Bash) and a packaged app's command (Terminal), both in the whole
/// Windows menu with their canonical verbs, and each readable on its own.
fn shell_with_commands() -> Arc<FakeShell> {
    let shell = shell_with_extensions();
    let item = |id, label: &str, verb: &str| ShellMenuEntry::Item {
        id,
        verb: Some(verb.into()),
        label: label.into(),
        enabled: true,
        checked: false,
        default: false,
        icon: None,
    };
    *shell.extra_menu.lock() = vec![
        item(20, "Open Git Bash here", "git_shell"),
        item(21, "Open in Terminal", TERMINAL),
    ];
    shell.extensions.lock().extend([
        (
            MenuHandler {
                key: "Directory".into(),
                clsid: String::new(),
                name: "git_shell".into(),
                kind: HandlerKind::Verb { extended: false },
            },
            vec![item(1, "Open Git Bash here", "git_shell")],
        ),
        (
            MenuHandler::command(TERMINAL),
            vec![item(1, "Open in Terminal", TERMINAL)],
        ),
    ]);
    *shell.packaged_types.lock() = vec![(TERMINAL.into(), vec!["Directory".into()])];
    shell
}

fn pin_types(cx: &mut VisualTestContext) -> std::collections::BTreeMap<String, Vec<String>> {
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pin_types
            .clone()
    })
}

/// #278: pinned registry and packaged-app commands are recognized by their canonical verbs in
/// the first whole menu (no extension read), the packaged one with its manifest's item types and
/// the kind it showed for. From then on their rows are drawn as soon as they are known to show,
/// before any source answers, and a click on one then runs it once its source is in.
#[gpui::test]
fn registry_and_packaged_commands_show_and_run_on_their_own(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_commands();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &["Open Git Bash here", "Open in Terminal"]);

    // First menu: learned from the verbs alone.
    right_click(&view, cx, Some(0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while owners(cx).len() < 2 || pin_types(cx).is_empty() {
        assert!(Instant::now() < deadline, "owners not learned");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let learned = owners(cx);
    assert_eq!(
        learned.get("Open Git Bash here").map(String::as_str),
        Some("verb:git_shell")
    );
    let terminal = format!("command:{TERMINAL}");
    assert_eq!(
        learned.get("Open in Terminal").map(String::as_str),
        Some(terminal.as_str())
    );
    // The manifest's type, and the kind of item it showed for (the fake's files are `.txt`).
    let deadline = Instant::now() + Duration::from_secs(20);
    while pin_types(cx).get(&terminal).is_none_or(|t| t.len() < 2) {
        assert!(Instant::now() < deadline, "item kinds not learned");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(pin_types(cx)[&terminal], ["Directory", ".txt"]);
    assert!(
        shell.extensions_read.lock().is_empty(),
        "no extension read to learn"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Next menu, with the whole menu and every source slow: both rows are there already.
    let (open_whole, whole_gate) = flume::unbounded();
    let (open_parts, parts_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    *shell.part_gate.lock() = Some(parts_gate);
    right_click(&view, cx, Some(0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !["Open Git Bash here", "Open in Terminal"]
        .iter()
        .all(|l| rows_above_more_options(&view, cx).contains(&(*l).to_owned()))
    {
        assert!(Instant::now() < deadline, "pinned rows not drawn ahead");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let rows = rows_above_more_options(&view, cx);
    let git = rows.iter().position(|r| r == "Open Git Bash here").unwrap();
    assert!(rows.contains(&"Open in Terminal".to_owned()), "{rows:?}");
    // Chosen before its source answered: nothing runs yet.
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(git);
        }
        cx.notify();
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.shell_invoked.lock().is_empty());
    assert!(
        view.update(cx, |v, _| v.context_menu.is_some()),
        "menu stays"
    );
    // The sources answer: it runs, in Git Bash's own one-command menu (sessions from 100).
    let _ = open_parts.send(());
    let _ = open_parts.send(());
    *shell.part_gate.lock() = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while shell.shell_invoked.lock().is_empty() {
        assert!(Instant::now() < deadline, "pending click never ran");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let ran = shell.shell_invoked.lock().clone();
    assert_eq!(ran.len(), 1);
    assert!(ran[0].0 >= 100 && ran[0].1 == 1, "{ran:?}");
    let _ = open_whole.send(());
    *shell.menu_gate.lock() = None;
}

/// #283: a command in a packaged app's own submenu (NanaZip's "Extract here") has no verb in the
/// Windows menu. The first menu finds which packaged command holds it by reading those for this
/// kind of item; from then on the pinned row shows from that command alone, before the whole
/// Windows menu, and runs there.
#[gpui::test]
fn a_command_in_a_packaged_submenu_shows_on_its_own(cx: &mut TestAppContext) {
    const NANA: &str = "{CAE3F1D4-7765-4D98-A060-52CD14D56EAB}";
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let item = |id, label: &str| ShellMenuEntry::Item {
        id,
        verb: None,
        label: label.into(),
        enabled: true,
        checked: false,
        default: false,
        icon: None,
    };
    let nana_menu = || ShellMenuEntry::Submenu {
        label: "NanaZip".into(),
        enabled: true,
        icon: None,
        children: vec![item(2, "Open archive"), item(3, "Extract here")],
        lazy: None,
    };
    *shell.extra_menu.lock() = vec![nana_menu()];
    shell
        .extensions
        .lock()
        .push((MenuHandler::command(NANA), vec![nana_menu()]));
    *shell.packaged_types.lock() = vec![(NANA.into(), vec!["*".into()])];
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &["NanaZip › Extract here"]);

    right_click(&view, cx, Some(0));
    let owner = format!("command:{NANA}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while owners(cx).get("NanaZip › Extract here") != Some(&owner) {
        assert!(
            Instant::now() < deadline,
            "owner not learned: {:?}",
            owners(cx)
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // The whole menu slow: the row comes from NanaZip's command alone and runs there.
    let (open_gate, gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(gate);
    right_click(&view, cx, Some(0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !rows_above_more_options(&view, cx).contains(&"Extract here".to_owned()) {
        assert!(Instant::now() < deadline, "pinned row didn't show early");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let rows = rows_above_more_options(&view, cx);
    let at = rows.iter().position(|r| r == "Extract here").unwrap();
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(at);
        }
        cx.notify();
    });
    cx.simulate_keystrokes("enter");
    wait_shell("ran in NanaZip's menu", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(102, 3)]);
    let _ = open_gate.send(());
    *shell.menu_gate.lock() = None;
}
