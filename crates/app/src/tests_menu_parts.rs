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
        parent: None,
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
                parent: None,
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

fn set_owner(cx: &mut VisualTestContext, key: &str, owner: &str) {
    let (key, owner) = (key.to_owned(), owner.to_owned());
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| _ = st.menu_pin_owners.insert(key, owner), cx);
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
                parent: None,
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

/// A pinned app command whose app was uninstalled (its class no longer reads): the row drawn
/// ahead goes away when its source fails, instead of staying until the whole Windows menu is in.
#[gpui::test]
fn a_pinned_command_of_an_uninstalled_app_goes_away(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_commands();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &["Open in Terminal"]);
    right_click(&view, cx, Some(0));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !owners(cx).contains_key("Open in Terminal") || pin_types(cx).is_empty() {
        assert!(Instant::now() < deadline, "owner not learned");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Uninstalled: reading it alone fails. The whole Windows menu is slow.
    shell
        .extensions
        .lock()
        .retain(|(h, _)| h.kind != HandlerKind::Command);
    let (open_whole, whole_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    right_click(&view, cx, Some(0));
    for _ in 0..50 {
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
    let rows = rows_above_more_options(&view, cx);
    assert!(!rows.contains(&"Open in Terminal".to_owned()), "{rows:?}");
    let _ = open_whole.send(());
    *shell.menu_gate.lock() = None;
}

fn pin_seen(cx: &mut VisualTestContext) -> std::collections::BTreeMap<String, Vec<String>> {
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pin_seen
            .clone()
    })
}

/// Waits, running the app, until `done`.
fn wait_for(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(cx) {
        assert!(Instant::now() < deadline, "{what}");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// #278: an extension's pinned command shows as soon as the menu opens where the extension
/// showed it for this kind of item before, without waiting for the extension (a third-party
/// DLL loaded and asked, up to a second on a slow PC). When it doesn't add it this time, the row
/// goes once it answers, and isn't drawn ahead again.
#[gpui::test]
fn an_extension_row_shows_ahead_where_it_showed_before(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let key = "Compress with 7-Zip";
    pin(cx, &[key]);

    // First menu: learns 7-Zip has it, for a .txt file.
    right_click(&view, cx, Some(0));
    wait_for(cx, "learned", |cx| {
        pin_seen(cx).get(key).is_some_and(|k| k == &[".txt"])
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Next menu, 7-Zip and the whole menu slow: the row is there before either answers.
    let (open_whole, whole_gate) = flume::unbounded();
    let (open_parts, parts_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    *shell.part_gate.lock() = Some(parts_gate);
    shell.extensions_read.lock().clear();
    right_click(&view, cx, Some(0));
    wait_for(cx, "row drawn ahead", |cx| {
        rows_above_more_options(&view, cx).contains(&key.to_owned())
    });
    assert!(
        shell.extensions_read.lock().is_empty(),
        "7-Zip hasn't answered"
    );
    let _ = open_parts.send(());
    wait_for(cx, "7-Zip answered", |_| {
        !shell.extensions_read.lock().is_empty()
    });
    assert!(rows_above_more_options(&view, cx).contains(&key.to_owned()));
    let _ = open_whole.send(());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // 7-Zip no longer adds it here: drawn ahead, gone once 7-Zip answers, forgotten.
    shell.extensions.lock()[0].1.clear();
    shell.extensions_read.lock().clear();
    right_click(&view, cx, Some(0));
    wait_for(cx, "row drawn ahead again", |cx| {
        rows_above_more_options(&view, cx).contains(&key.to_owned())
    });
    let _ = open_parts.send(());
    wait_for(cx, "row gone", |cx| {
        !rows_above_more_options(&view, cx).contains(&key.to_owned())
    });
    wait_for(cx, "forgotten", |cx| !pin_seen(cx).contains_key(key));
    *shell.part_gate.lock() = None;
    let _ = open_whole.send(());
    *shell.menu_gate.lock() = None;
}

/// #278: a registry command in a cascading menu (`ExtendedSubCommandsKey`, as context menu
/// managers make them) is learned by its verb like a top-level one, and from then on its row is
/// drawn before any source answers and runs in its own one-command menu, which shows it under
/// the cascade's label so its pinned key matches.
#[gpui::test]
fn a_command_in_a_cascading_menu_shows_on_its_own(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let item = |id, label: &str| ShellMenuEntry::Item {
        id,
        verb: Some("01_one".into()),
        label: label.into(),
        enabled: true,
        checked: false,
        default: false,
        icon: None,
    };
    let tools = |child| ShellMenuEntry::Submenu {
        label: "Tools".into(),
        enabled: true,
        icon: None,
        children: vec![child],
        lazy: None,
    };
    *shell.extra_menu.lock() = vec![tools(item(30, "01_one"))];
    shell.extensions.lock().push((
        MenuHandler {
            key: "Tools.Menu".into(),
            clsid: String::new(),
            name: "01_one".into(),
            kind: HandlerKind::Verb { extended: false },
            parent: Some(r"*\shell\Tools".into()),
        },
        vec![tools(item(1, "01_one"))],
    ));
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let key = "Tools › 01_one";
    pin(cx, &[key]);

    right_click(&view, cx, Some(0));
    wait_for(cx, "learned", |cx| owners(cx).contains_key(key));
    assert_eq!(
        owners(cx)[key],
        "verb:01_one",
        "its verb, not left to the whole menu"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Next menu, every source slow: the row is there, and a click runs it once it answers.
    let (open_whole, whole_gate) = flume::unbounded();
    let (open_parts, parts_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    *shell.part_gate.lock() = Some(parts_gate);
    right_click(&view, cx, Some(0));
    wait_for(cx, "row drawn ahead", |cx| {
        rows_above_more_options(&view, cx).contains(&"01_one".to_owned())
    });
    let at = rows_above_more_options(&view, cx)
        .iter()
        .position(|r| r == "01_one")
        .unwrap();
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(at);
        }
        cx.notify();
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        shell.shell_invoked.lock().is_empty(),
        "not before it answers"
    );
    for _ in 0..4 {
        let _ = open_parts.send(());
    }
    *shell.part_gate.lock() = None;
    wait_for(cx, "ran", |_| !shell.shell_invoked.lock().is_empty());
    let ran = shell.shell_invoked.lock().clone();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].1, 1, "its own one-command menu");
    let _ = open_whole.send(());
    *shell.menu_gate.lock() = None;
}

/// A command pinned from the whole Windows menu learns its source right away, so the very next
/// menu shows it on its own instead of waiting for the whole menu once more.
#[gpui::test]
fn pinning_learns_the_source_at_once(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));

    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    assert!(owners(cx).is_empty(), "nothing pinned, nothing learned");
    view.update(cx, |v, cx| v.toggle_shell_pin("Compress with 7-Zip", cx));
    wait_for(cx, "learned on pinning", |cx| {
        owners(cx).contains_key("Compress with 7-Zip")
    });
    assert_eq!(owners(cx)["Compress with 7-Zip"], seven_zip().clsid);
}

/// #278: a source read only in part (a submenu still filling when the wait ran out) doesn't
/// count as not having a pinned command. Its row, drawn ahead, stays, and it is still drawn
/// ahead next time: taking a short read for "not here" made rows come and go.
#[gpui::test]
fn a_source_read_short_keeps_its_row(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let key = "Compress with 7-Zip";
    pin(cx, &[key]);
    right_click(&view, cx, Some(0));
    wait_for(cx, "learned", |cx| {
        pin_seen(cx).get(key).is_some_and(|k| k == &[".txt"])
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // 7-Zip comes back cut short and without the command; the whole menu too.
    shell.extensions.lock()[0].1.clear();
    shell.partial_extensions.lock().push("7-Zip".into());
    shell
        .partial_menu
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shell.extensions_read.lock().clear();
    right_click(&view, cx, Some(0));
    wait_for(cx, "7-Zip answered", |_| {
        !shell.extensions_read.lock().is_empty()
    });
    wait_until(cx, &view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    assert!(
        rows_above_more_options(&view, cx).contains(&key.to_owned()),
        "the row stays"
    );
    assert!(
        pin_seen(cx).get(key).is_some_and(|k| k == &[".txt"]),
        "still known to show for a .txt file"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Read in full and still without it: now it goes, and is forgotten (the whole menu, which
    // this shell gives with the command, held back).
    shell.partial_extensions.lock().clear();
    shell
        .partial_menu
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (open_whole, whole_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    shell.extensions_read.lock().clear();
    right_click(&view, cx, Some(0));
    wait_for(cx, "row gone", |cx| {
        !rows_above_more_options(&view, cx).contains(&key.to_owned())
    });
    wait_for(cx, "forgotten", |cx| !pin_seen(cx).contains_key(key));
    let _ = open_whole.send(());
    *shell.menu_gate.lock() = None;
}

/// #278: "Send to" under "Show more options" is the menu host's own list, read on its own,
/// in place of Windows' one, which fills slowly and was often cut short. Its rows run in that
/// list's own menu.
#[gpui::test]
fn send_to_comes_from_the_hosts_own_list(cx: &mut TestAppContext) {
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
    // Windows' own, as the whole menu has it: left to fill (it is never waited for).
    *shell.lazy_submenus.lock() = vec![("Send to".into(), vec![item(40, "Only the first")])];
    // The host's list.
    shell.extensions.lock().push((
        MenuHandler::send_to(),
        vec![ShellMenuEntry::Submenu {
            label: "Send to".into(),
            enabled: true,
            icon: None,
            children: vec![
                item(1, "Compressed folder"),
                item(2, "Desktop"),
                item(3, "Notes"),
            ],
            lazy: None,
        }],
    ));
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    right_click(&view, cx, Some(0));
    let want = (
        vec![
            "Compressed folder".to_owned(),
            "Desktop".to_owned(),
            "Notes".to_owned(),
        ],
        false,
    );
    wait_until(cx, &view, "the host's Send to", |v| {
        v.context_menu
            .as_ref()
            .and_then(|m| m.shell_submenu_for_test("Send to"))
            .as_ref()
            == Some(&want)
    });
    assert!(shell.extensions_read.lock().contains(&"SendTo".to_owned()));
}

/// A right press, as the list gets it on a row.
fn right_press() -> gpui::MouseDownEvent {
    gpui::MouseDownEvent {
        button: gpui::MouseButton::Right,
        position: gpui::point(gpui::px(300.), gpui::px(200.)),
        modifiers: gpui::Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    }
}

/// #325: a pinned command seen for this kind of item before is in the menu the moment it opens,
/// before anything has been looked up for it, a registry command's (as "Run as administrator")
/// as well as an extension's: the menu doesn't grow a row a moment later.
#[gpui::test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "ContextMenu lives in a private explorer submodule, so its methods can't be named by path"
)]
fn seen_rows_are_in_the_menu_as_it_opens(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_commands();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let (git, zip) = ("Open Git Bash here", "Compress with 7-Zip");
    pin(cx, &[git, zip]);

    // Where they come from, then (their sources read on their own) where they show.
    right_click(&view, cx, Some(0));
    wait_for(cx, "owners", |cx| owners(cx).len() >= 2);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    right_click(&view, cx, Some(0));
    wait_for(cx, "seen", |cx| {
        let seen = pin_seen(cx);
        [git, zip]
            .iter()
            .all(|key| seen.get(*key).is_some_and(|k| k == &[".txt"]))
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Nothing answers; the rows are there as it opens, before any event is handled.
    let (open_whole, whole_gate) = flume::unbounded();
    let (open_parts, parts_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    *shell.part_gate.lock() = Some(parts_gate);
    let event = right_press();
    let labels = cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.on_row_right_down(0, &event, window, cx);
            v.right_press_released(event.position, window, cx);
            v.context_menu
                .as_ref()
                .map_or_else(Vec::new, |m| m.labels())
        })
    });
    assert!(labels.iter().any(|l| l == git), "{labels:?}");
    assert!(labels.iter().any(|l| l == zip), "{labels:?}");
    *shell.part_gate.lock() = None;
    *shell.menu_gate.lock() = None;
    for _ in 0..4 {
        let _ = open_parts.send(());
        let _ = open_whole.send(());
    }
    cx.run_until_parked();
}

/// #278: an item's menu is made when the right button goes down, so the Windows menu is read
/// while it is held: what came back by the time it is let go is in the menu as it shows. A press
/// that goes away without a menu (a drag, a release elsewhere) lets go of what was read for it.
#[gpui::test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "ContextMenu lives in a private explorer submodule, so its methods can't be named by path"
)]
fn the_menu_is_read_while_the_button_is_down(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let event = right_press();
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.on_row_right_down(0, &event, window, cx));
    });
    wait_for(cx, "read while held", |_| {
        shell.system_menus.lock().len() == 1
    });
    cx.run_until_parked();
    assert!(
        view.update(cx, |v, _| v.context_menu.is_none()),
        "not shown while held"
    );
    // Let go: "Show more options" is in already.
    let loaded = cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.right_press_released(event.position, window, cx);
            v.context_menu.as_ref().map(|m| m.shell_state())
        })
    });
    assert!(loaded.is_some_and(|(done, session)| done && session.is_some()));
    assert_eq!(shell.system_menus.lock().len(), 1, "read once");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // A press that goes away: its menu isn't shown, and what was read is let go.
    shell.shell_released.lock().clear();
    let event = right_press();
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.on_row_right_down(1, &event, window, cx));
    });
    wait_for(cx, "read again", |_| shell.system_menus.lock().len() == 2);
    cx.run_until_parked();
    let moved = gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(320.), gpui::px(240.)),
        pressed_button: None,
        modifiers: gpui::Modifiers::default(),
    };
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.right_press_moved(&moved, window, cx));
    });
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.context_menu.is_none()
        && v.early_menu.is_none()));
    assert!(!shell.shell_released.lock().is_empty(), "let go");
}

/// #278: a Windows submenu still filling when the whole menu was read isn't waited for; it is
/// filled right after, without being opened, so it is usually in by the time it is.
#[gpui::test]
fn a_submenu_left_filling_is_filled_right_after(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
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
    *shell.lazy_submenus.lock() = vec![
        ("Give access to".into(), vec![item(30, "Specific people")]),
        ("Opened later".into(), vec![item(31, "Only when opened")]),
    ];
    // The first was still filling; the second, empty, is one filled when opened (as before).
    *shell.filling_menu.lock() = vec![(0, "Give access to".into())];
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "filled unopened", |v| {
        v.context_menu
            .as_ref()
            .and_then(|m| m.shell_submenu_for_test("Give access to"))
            .is_some_and(|(items, _)| items == ["Specific people"])
    });
    let filled: Vec<u32> = shell
        .submenus_filled
        .lock()
        .iter()
        .map(|(_, ix)| *ix)
        .collect();
    assert_eq!(filled, [0], "only the one left filling");
    assert!(
        view.update(cx, |v, _| v
            .context_menu
            .as_ref()
            .and_then(|m| m.shell_submenu_for_test("Opened later")))
            .is_some_and(|(items, lazy)| lazy && !items.iter().any(|i| i == "Only when opened"))
    );
}

/// #278: a submenu left filling that comes back with nothing Arke can show (owner-drawn items)
/// goes from "Show more options", rather than staying as an empty submenu.
#[gpui::test]
fn a_submenu_left_filling_with_nothing_goes(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.lazy_submenus.lock() = vec![("Drawn by its app".into(), Vec::new())];
    *shell.filling_menu.lock() = vec![(0, "Drawn by its app".into())];
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options in", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    wait_for(cx, "filled", |_| !shell.submenus_filled.lock().is_empty());
    wait_until(cx, &view, "gone", |v| {
        v.context_menu
            .as_ref()
            .is_some_and(|m| m.shell_submenu_for_test("Drawn by its app").is_none())
    });
}

/// #325: "Run as administrator" has no label in the registry (Windows names it), so read on its
/// own it comes back called by its verb. It is still the pinned command: its row stays when that
/// answer comes, and it is noted to show for the kind of item, to be drawn as the menu opens.
#[gpui::test]
fn a_command_windows_names_keeps_its_row(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let item = |id, label: &str| ShellMenuEntry::Item {
        id,
        verb: Some("runas".into()),
        label: label.into(),
        enabled: true,
        checked: false,
        default: false,
        icon: None,
    };
    let key = "Run as administrator";
    *shell.extra_menu.lock() = vec![item(22, key)];
    shell.extensions.lock().push((
        MenuHandler {
            key: "txtfile".into(),
            clsid: String::new(),
            name: "runas".into(),
            kind: HandlerKind::Verb { extended: false },
            parent: None,
        },
        vec![item(1, "runas")],
    ));
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &[key]);
    right_click(&view, cx, Some(0));
    wait_for(cx, "owner", |cx| {
        owners(cx).get(key).is_some_and(|o| o == "verb:runas")
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // The whole menu slow: the row comes from the verb read on its own, and stays.
    let (open_whole, whole_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    shell.extensions_read.lock().clear();
    right_click(&view, cx, Some(0));
    wait_for(cx, "verb answered", |_| {
        !shell.extensions_read.lock().is_empty()
    });
    wait_for(cx, "noted", |cx| {
        pin_seen(cx).get(key).is_some_and(|k| k == &[".txt"])
    });
    assert!(rows_above_more_options(&view, cx).contains(&key.to_owned()));
    *shell.menu_gate.lock() = None;
    for _ in 0..4 {
        let _ = open_whole.send(());
    }
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
}

/// #325: the registry lists "Run as administrator" for an item Windows doesn't show it for (a zip
/// here). Its row isn't drawn ahead of its source, to vanish when that answers: rows are drawn
/// ahead only where they showed before.
#[gpui::test]
fn a_command_left_out_here_is_not_drawn_ahead(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.zip", 1)]);
    let shell = Arc::new(FakeShell::default());
    let key = "Run as administrator";
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    pin(cx, &[key]);
    set_owner(cx, key, "verb:runas");
    // Listed in the registry, left out by Windows: read on its own it adds nothing.
    shell.extensions.lock().push((
        MenuHandler {
            key: "CompressedFolder".into(),
            clsid: String::new(),
            name: "runas".into(),
            kind: HandlerKind::Verb { extended: false },
            parent: None,
        },
        Vec::new(),
    ));
    // Every source slow, the whole menu too (once it is in, it decides the rows).
    let (open_whole, whole_gate) = flume::unbounded();
    *shell.menu_gate.lock() = Some(whole_gate);
    let (answer, gate) = flume::unbounded();
    *shell.part_gate.lock() = Some(gate);
    right_click(&view, cx, Some(0));
    // Long enough for the sources to be listed and their rows drawn ahead.
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!rows_above_more_options(&view, cx).contains(&key.to_owned()));
    let _ = answer.send(());
    wait_for(cx, "read", |_| !shell.extensions_read.lock().is_empty());
    cx.run_until_parked();
    assert!(!rows_above_more_options(&view, cx).contains(&key.to_owned()));
    *shell.part_gate.lock() = None;
    *shell.menu_gate.lock() = None;
    for _ in 0..4 {
        let _ = open_whole.send(());
    }
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
}

/// #344: "Pin to Start" only works when File Explorer itself runs it; from Arke Windows answers
/// "access denied". It stays out of "Show more options"; the rest of the Windows menu is kept.
#[gpui::test]
fn pin_to_start_stays_out_of_more_options(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.exe", 1)]);
    let shell = Arc::new(FakeShell::default());
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
        item(40, "Pin to Start", "PinToStartScreen"),
        item(41, "Share", "Windows.ModernShare"),
    ];
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options in", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    let items = view.update(cx, |v, _| {
        let Some(menu) = v.context_menu.as_ref() else {
            return Vec::new();
        };
        menu.shell_items_for_test()
    });
    assert!(items.iter().any(|i| i == "Share"), "{items:?}");
    assert!(!items.iter().any(|i| i == "Pin to Start"), "{items:?}");
}

/// "More options" says once what its pins do: the line is there until the submenu has been open,
/// then it doesn't come back.
#[gpui::test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "ContextMenu lives in a private explorer submodule, so its methods can't be named by path"
)]
fn more_options_explains_its_pins_once(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = shell_with_extensions();
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let hint = t!("menu.pin_hint").into_owned();
    // A new profile: the line hasn't been shown yet.
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.pin_hint_seen = false, cx);
        });
    });
    let items = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        view.update(cx, |v, _| {
            v.context_menu
                .as_ref()
                .map_or_default(|m| m.shell_items_for_test())
        })
    };
    let seen = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            crate::settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .pin_hint_seen
        })
    };

    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options in", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    assert!(items(&view, cx).contains(&hint), "{:?}", items(&view, cx));
    assert!(!seen(cx), "not until More options opens");

    let more = format!("{} ›", t!("menu.more_options"));
    let at = menu_labels(&view, cx)
        .unwrap_or_default()
        .iter()
        .position(|l| *l == more)
        .expect("More options in the menu");
    view.update(cx, |v, _| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(at);
        }
    });
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    assert!(seen(cx), "shown once");
    cx.simulate_keystrokes("escape escape");
    cx.run_until_parked();

    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options in", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    assert!(!items(&view, cx).contains(&hint), "{:?}", items(&view, cx));
}
