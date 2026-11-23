//! What a screen reader is given (#322): the window's accessibility tree, as AccessKit builds it
//! once a screen reader connects (`simulate_a11y_activation`, from Arke's patch of gpui-pre's test
//! platform, see third_party/README.md).

use std::sync::Arc;

use serde_json::Value;

use super::*;

/// Roles a screen reader user moves to and acts on: each needs a name.
const CONTROLS: &[&str] = &[
    "Button",
    "CheckBox",
    "ColumnHeader",
    "ComboBox",
    "Link",
    "ListBox",
    "ListBoxOption",
    "Menu",
    "MenuItem",
    "MenuItemCheckBox",
    "MenuItemRadio",
    "RadioButton",
    "Slider",
    "Switch",
    "Tab",
    "TabList",
    "TextInput",
    "Toolbar",
    "Tree",
    "TreeItem",
];

/// The window's accessibility tree, as the debug JSON GPUI keeps of the last frame.
fn tree(cx: &mut VisualTestContext) -> Value {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
    let json = cx
        .update(|window, _| window.debug_a11y_tree_json())
        .expect("the tree is built once a screen reader is connected");
    serde_json::from_str(&json).unwrap()
}

fn nodes(tree: &Value) -> impl Iterator<Item = &Value> {
    tree["nodes"].as_object().unwrap().values()
}

fn role(node: &Value) -> &str {
    node["aria"]["role"].as_str().unwrap_or_default()
}

fn label(node: &Value) -> &str {
    node["aria"]["label"].as_str().unwrap_or_default()
}

fn actions(node: &Value) -> Vec<&str> {
    node["aria"]["on_action"]
        .as_array()
        .map_or_default(|a| a.iter().filter_map(Value::as_str).collect())
}

/// Controls with no name: what a screen reader would read as just "button" or nothing. Each
/// with its role and where it was made.
fn unnamed(tree: &Value) -> Vec<String> {
    nodes(tree)
        .filter(|n| {
            let acts = actions(n);
            let control = CONTROLS.contains(&role(n))
                || acts
                    .iter()
                    .any(|a| matches!(*a, "Click" | "Expand" | "Focus"));
            control && label(n).trim().is_empty()
        })
        .map(|n| {
            format!(
                "{} at {}",
                role(n),
                n["source_location"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

/// The node of the control named `name` (the first, in tree order).
fn node_id(tree: &Value, name: &str) -> accesskit::NodeId {
    let node = nodes(tree)
        .find(|n| label(n) == name)
        .unwrap_or_else(|| panic!("no control named {name:?}"));
    accesskit::NodeId(node["accesskit_id"].as_str().unwrap().parse().unwrap())
}

fn named<'a>(tree: &'a Value, name: &str) -> &'a Value {
    nodes(tree)
        .find(|n| label(n) == name)
        .unwrap_or_else(|| panic!("no control named {name:?}"))
}

/// The node a screen reader reads as focused: the active item of a list or tree, else the
/// focused element.
fn focused(tree: &Value) -> (String, String) {
    let key = tree["active_descendant_focus"]
        .as_str()
        .or_else(|| tree["gpui_focus"].as_str())
        .expect("something has the focus");
    let node = &tree["nodes"][key];
    (role(node).to_owned(), label(node).to_owned())
}

/// A window on `C:\w` with three files, drive `C:` in the sidebar, and a screen reader connected.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("w")]);
    fs.insert_many(r"C:\w", 3);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(crate::shell::Place {
        label: "Local Disk (C:)".to_owned(),
        path: PathBuf::from(r"C:\"),
        kind: crate::shell::PlaceKind::Drive {
            remote: false,
            space: None,
        },
    });
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\w");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", |v| {
        loaded(r"C:\w")(v) && v.places.len() > 1
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.simulate_a11y_activation();
    (ws, cx)
}

/// Every control of the main window, Settings (with every section opened the way a screen reader
/// does, through Expand), the command palette and Quick look has a name.
#[gpui::test]
fn every_control_is_named(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    let main = tree(cx);
    assert!(nodes(&main).count() > 30, "the window is in the tree");
    assert_eq!(unnamed(&main), Vec::<String>::new(), "main window");

    // Quick look over the first file.
    cx.simulate_keystrokes("down space");
    let look = tree(cx);
    assert_eq!(unnamed(&look), Vec::<String>::new(), "quick look");
    cx.simulate_keystrokes("escape");

    cx.simulate_keystrokes("ctrl-shift-p");
    let palette = tree(cx);
    assert!(nodes(&palette).any(|n| role(n) == "ListBox" && label(n) == "Results"));
    assert_eq!(unnamed(&palette), Vec::<String>::new(), "command palette");
    cx.simulate_keystrokes("escape");

    cx.dispatch_action(crate::workspace::OpenSettings);
    let first = tree(cx);
    let pages: Vec<String> = nodes(&first)
        .filter(|n| role(n) == "Tab" && n["aria"]["selected"].is_boolean())
        .map(|n| label(n).to_owned())
        .filter(|l| l != "w" && l != "Settings")
        .collect();
    assert!(pages.len() > 5, "settings pages {pages:?}");
    let mut opened = Vec::new();
    for page in &pages {
        let t = tree(cx);
        cx.simulate_a11y_action(node_id(&t, page), gpui::AccessibleAction::Click);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..30 {
            let settings = tree(cx);
            assert_eq!(
                unnamed(&settings),
                Vec::<String>::new(),
                "settings › {page}"
            );
            // Open the next closed section (not dropdowns: one would close the other).
            let next = nodes(&settings).find(|n| {
                role(n) == "Button"
                    && n["aria"]["expanded"] == Value::Bool(false)
                    && !seen.contains(label(n))
            });
            let Some(next) = next else { break };
            let name = label(next).to_owned();
            let id = accesskit::NodeId(next["accesskit_id"].as_str().unwrap().parse().unwrap());
            seen.insert(name.clone());
            cx.simulate_a11y_action(id, gpui::AccessibleAction::Expand);
            let after = tree(cx);
            assert_eq!(
                named(&after, &name)["aria"]["expanded"],
                Value::Bool(true),
                "{page} › {name} opens with Expand"
            );
            opened.push(format!("{page} › {name}"));
        }
    }
    assert!(opened.len() > 5, "opened {opened:?}");
}

/// F6 takes the focus round the areas, and a screen reader is told where it is each time.
#[gpui::test]
fn f6_names_each_area(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    let mut stops = Vec::new();
    for _ in 0..4 {
        stops.push(focused(&tree(cx)));
        cx.simulate_keystrokes("f6");
    }
    stops.push(focused(&tree(cx)));
    let roles: Vec<&str> = stops.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        ["ListBox", "TreeItem", "TextInput", "TextInput", "ListBox"],
        "{stops:?}"
    );
    assert!(stops.iter().all(|(_, l)| !l.is_empty()), "{stops:?}");
    assert_eq!(stops[1].1, "w", "the sidebar starts on the folder shown");
    assert_ne!(
        stops[2].1, stops[3].1,
        "path bar and search box are told apart"
    );
}

/// What a screen reader presses works: a toggle button by Click, a menu button by Expand (it
/// reports itself open), and the context menu it opens is named.
#[gpui::test]
fn a_screen_reader_can_press_and_open(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let t = tree(cx);
    cx.simulate_a11y_action(
        node_id(&t, "Split into two panes"),
        gpui::AccessibleAction::Click,
    );
    assert_eq!(
        ws.update(cx, |ws, cx| ws.all_paths_for_test(cx))[0].len(),
        2,
        "split by Click"
    );

    let t = tree(cx);
    cx.simulate_a11y_action(node_id(&t, "View options"), gpui::AccessibleAction::Expand);
    let t = tree(cx);
    assert_eq!(
        named(&t, "View options")["aria"]["expanded"],
        Value::Bool(true)
    );
    assert_eq!(unnamed(&t), Vec::<String>::new(), "view options open");
}

/// A screen reader's Click reaches a button scrolled out of view: it runs the button's click,
/// rather than a mouse click where the button would be, which hit nothing or another control.
#[gpui::test]
fn a_screen_reader_can_press_a_button_out_of_view(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    // Short enough that the Network section is below the end of the sidebar.
    cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(240.)));
    let t = tree(cx);
    cx.simulate_a11y_action(
        node_id(&t, "Add network location…"),
        gpui::AccessibleAction::Click,
    );
    assert!(
        ws.update(cx, |ws, _| ws.net_dialog_for_test()).is_some(),
        "the dialog opens"
    );
}

/// A drive in the pinned list is named as on a tile and in This PC, not by its path (#371).
#[gpui::test]
fn a_pinned_drive_is_named_like_the_drive(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.pinned = vec![PathBuf::from(r"C:\")], cx);
        });
    });
    let t = tree(cx);
    let rows: Vec<&str> = nodes(&t)
        .filter(|n| role(n) == "TreeItem")
        .map(label)
        .collect();
    assert!(!rows.contains(&r"C:\"), "{rows:?}");
    assert_eq!(
        rows.iter().filter(|l| **l == "Local Disk (C:)").count(),
        2,
        "This PC and the pinned list: {rows:?}"
    );
}

/// A folder named by its `desktop.ini`, as Windows names the user folder's Documents in other
/// languages, is shown (and read out) by that name; a folder without the read-only or system
/// attribute keeps its own, as in File Explorer (#370).
#[gpui::test]
fn a_folder_shows_the_name_its_desktop_ini_gives(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\u",
        vec![
            Entry::new("Documents", EntryKind::Dir, 0, None, 0x1),
            Entry::new("Plain", EntryKind::Dir, 0, None, 0),
        ],
    );
    let ini = |name: &str| {
        let text = format!("[.ShellClassInfo]\r\nLocalizedResourceName={name}\r\n");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    };
    fs.set_content(r"C:\u\Documents\desktop.ini", ini("Dokumente"));
    fs.set_content(r"C:\u\Plain\desktop.ini", ini("Not read"));
    let (ws, cx) = setup_workspace(cx, fs, r"C:\u");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\u"));
    cx.simulate_a11y_activation();
    let deadline = Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let t = tree(cx);
        let rows: Vec<String> = nodes(&t)
            .filter(|n| role(n) == "ListBoxOption")
            .map(|n| label(n).to_owned())
            .collect();
        if rows.iter().any(|r| r.starts_with("Dokumente,")) || Instant::now() > deadline {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(rows.iter().any(|r| r.starts_with("Dokumente,")), "{rows:?}");
    assert!(rows.iter().any(|r| r.starts_with("Plain,")), "{rows:?}");
    assert!(!rows.iter().any(|r| r.starts_with("Not read")), "{rows:?}");
}
