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

/// A window on `C:\w` with three files, drive `C:` in the sidebar, the tree open to the folder
/// (off by default since #416, these tests are about it), and a screen reader connected.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    window_following(cx, true)
}

/// The same window with the tree following the current folder or not from the first frame.
fn window_following(
    cx: &mut TestAppContext,
    follow: bool,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.sidebar_follow = follow, cx));
    });
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
    for _ in 0..5 {
        stops.push(focused(&tree(cx)));
        cx.simulate_keystrokes("f6");
    }
    stops.push(focused(&tree(cx)));
    let roles: Vec<&str> = stops.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        [
            "ListBox",
            "TreeItem",
            "TextInput",
            "Button",
            "TextInput",
            "ListBox"
        ],
        "{stops:?}"
    );
    assert!(stops.iter().all(|(_, l)| !l.is_empty()), "{stops:?}");
    assert_eq!(stops[1].1, "w", "the sidebar starts on the folder shown");
    assert!(
        stops[3].1.starts_with("Up"),
        "the pane row starts on its first button: {stops:?}"
    );
    assert_ne!(
        stops[2].1, stops[4].1,
        "path bar and search box are told apart"
    );
}

/// #416: the tree doesn't follow the folder by default, so it may not show the current one: F6
/// still reaches the sidebar, on the nearest row above the folder (File Explorer lands on its own
/// row), and the arrows move on to rows a screen reader can name.
#[gpui::test]
fn the_sidebar_is_usable_from_the_keyboard_when_the_tree_does_not_follow(cx: &mut TestAppContext) {
    let (_ws, cx) = window_following(cx, false);
    cx.simulate_keystrokes("f6");
    let start = focused(&tree(cx));
    assert_eq!(start.0, "TreeItem", "starts on a row: {start:?}");
    assert!(
        start.1.contains("C:"),
        "the drive above the folder: {start:?}"
    );
    cx.simulate_keystrokes("home");
    let first = focused(&tree(cx));
    assert!(!first.1.is_empty(), "the row is named: {first:?}");
    cx.simulate_keystrokes("end");
    let last = focused(&tree(cx));
    assert!(!last.1.is_empty(), "the row is named: {last:?}");
    cx.simulate_keystrokes("home");
    assert_eq!(focused(&tree(cx)), first, "Home goes back to the first row");
}

/// The pane row's buttons from the keyboard: F6 makes the row one stop, the arrows, Home and End
/// move between its buttons (each read as focused), Space and Enter press the one it is on, and
/// Escape goes back to the list (#322).
#[gpui::test]
fn the_pane_row_works_from_the_keyboard(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    // The list, the sidebar, the path bar, then the row.
    for _ in 0..3 {
        cx.simulate_keystrokes("f6");
        tree(cx);
    }
    let t = tree(cx);
    assert!(
        t["gpui_focus"]
            .as_str()
            .is_some_and(|key| role(&t["nodes"][key]) == "Toolbar"),
        "the row has the focus: {:?}",
        t["gpui_focus"]
    );
    let first = focused(&t);
    assert_eq!(first.0, "Button", "{first:?}");
    assert!(first.1.starts_with("Up"), "{first:?}");

    cx.simulate_keystrokes("right");
    let next = focused(&tree(cx));
    assert_ne!(next, first, "Right moves to the next button");
    cx.simulate_keystrokes("left left");
    assert_eq!(
        focused(&tree(cx)),
        first,
        "Left moves back and stops at the first"
    );

    let mut path = Vec::new();
    while !focused(&tree(cx)).1.starts_with("Preview pane") {
        assert!(path.len() < 30, "no preview pane button: {path:?}");
        cx.simulate_keystrokes("right");
        path.push(focused(&tree(cx)));
    }
    let before = settings_now(cx).preview_pane;
    press(cx, "space");
    assert_eq!(settings_now(cx).preview_pane, !before, "Space presses it");
    let after = focused(&tree(cx));
    assert!(
        after.1.starts_with("Preview pane"),
        "the focus stays: {after:?}"
    );
    press(cx, "enter");
    assert_eq!(settings_now(cx).preview_pane, before, "and so does Enter");

    cx.simulate_keystrokes("end");
    let last = focused(&tree(cx));
    assert_ne!(last, first, "{last:?}");
    cx.simulate_keystrokes("right");
    assert_eq!(
        focused(&tree(cx)),
        last,
        "End goes to the last, Right stops there"
    );
    cx.simulate_keystrokes("home");
    assert_eq!(focused(&tree(cx)), first, "Home goes to the first");

    cx.simulate_keystrokes("escape");
    let region = cx.update(|window, cx| ws.read(cx).region_for_test(window, cx));
    assert_eq!(region, "list", "Escape goes back to the list");
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

/// The operation center's list keeps its names and roles (#403): the list, the queue's two
/// header buttons (there even while nothing has finished) and a running job's Cancel.
#[gpui::test]
fn the_ops_list_is_named(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let _running = cx.update(|_, cx| {
        crate::op_center::OpCenter::begin(cx, "op.copy", Some(("a.txt".to_owned(), 4))).1
    });
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    view.update(cx, |v, cx| {
        v.ops_open = true;
        cx.notify();
    });
    let t = tree(cx);
    assert_eq!(unnamed(&t), Vec::<String>::new(), "operation center open");
    assert_eq!(role(named(&t, &t!("ops.center_title"))), "List");
    for button in [
        t!("ops.queue_pause"),
        t!("ops.center_clear"),
        t!("toast.cancel"),
    ] {
        assert_eq!(role(named(&t, &button)), "Button", "{button}");
    }
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

/// A sidebar tree row tells its depth (from 0, as AccessKit counts it; UI Automation adds one),
/// and only the current folder's row says it is selected, so a screen reader doesn't call every
/// row "not selected" (#322).
#[gpui::test]
fn sidebar_tree_rows_tell_their_level(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    let t = tree_with_here(cx);
    let level = |n: &Value| n["aria"]["level"].as_u64();
    let rows = || nodes(&t).filter(|n| role(n) == "TreeItem");
    let drive = rows()
        .filter(|n| label(n) == "Local Disk (C:)")
        .filter_map(level)
        .min();
    assert_eq!(drive, Some(0), "a top-level row is at depth 0");
    let w = rows()
        .find(|n| label(n) == "w")
        .expect("the current folder's row");
    assert!(level(w) > Some(0), "a nested row is deeper: {w}");
    assert_eq!(w["aria"]["selected"], Value::Bool(true));
    let drive_row = rows()
        .find(|n| label(n) == "Local Disk (C:)" && level(n) == Some(0))
        .expect("the drive's row");
    assert!(
        drive_row["aria"]["selected"].is_null(),
        "other rows are not marked not selected: {drive_row}"
    );
}

/// The tree once the sidebar shows the current folder's row ("w", under the drive).
fn tree_with_here(cx: &mut VisualTestContext) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let t = tree(cx);
        let here = nodes(&t).any(|n| role(n) == "TreeItem" && label(n) == "w");
        if here || Instant::now() > deadline {
            return t;
        }
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The key of the node that lists `key` among its children.
fn parent_key<'a>(tree: &'a Value, key: &str) -> Option<&'a str> {
    tree["nodes"]
        .as_object()?
        .iter()
        .find(|(_, n)| {
            n["children"]
                .as_array()
                .is_some_and(|c| c.iter().any(|c| c.as_str() == Some(key)))
        })
        .map(|(k, _)| k.as_str())
}

/// The sidebar's rows are drawn one after another, but a folder's row sits under its parent's
/// row in the tree: NVDA counts a row's level from the rows it is under, and read every row as
/// "level 1" (#322).
#[gpui::test]
fn sidebar_tree_rows_sit_under_their_parent(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    let t = tree_with_here(cx);
    let mut at: &str = t["nodes"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, n)| role(n) == "TreeItem" && label(n) == "w")
        .map(|(k, _)| k.as_str())
        .expect("the current folder's row");
    let mut above: Vec<(&str, &str)> = Vec::new();
    while let Some(up) = parent_key(&t, at) {
        let node = &t["nodes"][up];
        above.push((role(node), label(node)));
        assert!(above.len() < 100, "a loop in the tree: {above:?}");
        at = up;
    }
    let rows: Vec<&str> = above
        .iter()
        .filter(|(r, _)| *r == "TreeItem")
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(rows, ["Local Disk (C:)"], "the rows above w: {above:?}");
    assert!(
        above.iter().any(|(r, _)| *r == "Tree"),
        "in the sidebar's tree: {above:?}"
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

/// The context menu's rows are told from the window that has the keys, the highlighted one as
/// focused: its levels are popup windows that are never active, which a screen reader doesn't
/// follow (#322).
#[gpui::test]
fn the_context_menu_tells_its_highlighted_row(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.simulate_keystrokes("down shift-f10");
    let opened = focused(&tree(cx));
    assert!(
        matches!(opened.0.as_str(), "Menu" | "MenuItem"),
        "the menu has the focus: {opened:?}"
    );
    cx.simulate_keystrokes("down");
    let first = focused(&tree(cx));
    assert_eq!(first.0, "MenuItem", "{first:?}");
    assert!(!first.1.is_empty(), "{first:?}");
    cx.simulate_keystrokes("down");
    let second = focused(&tree(cx));
    assert_eq!(second.0, "MenuItem", "{second:?}");
    assert_ne!(first.1, second.1, "the next row is told");
    cx.simulate_keystrokes("escape");
    assert_eq!(focused(&tree(cx)).0, "ListBoxOption", "back on the file");
}

/// "Similar results" in search results (#396) is read like the item under the cursor once the
/// keyboard is on it, tells whether it is open, and opens by Enter and by a screen reader's
/// Expand.
#[gpui::test]
fn the_similar_results_row_reads_as_the_focused_item(cx: &mut TestAppContext) {
    use crate::explorer::similar::SimilarRow;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\k", vec![dir("sub"), file("notes.txt", 1)]);
    fs.insert_dir(r"C:\k\sub", vec![file("n_o_t_e_s.md", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), r"C:\k");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\k"));
    cx.simulate_a11y_activation();
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("notes");
    wait_until(cx, &view, "the exact hit", |v| {
        final_hits(v) == ["notes.txt"] && v.similar_row() == Some((1, SimilarRow::Closed))
    });
    let similar = t!("list.similar").into_owned();
    // Into the list (the hit), then the row.
    press(cx, "down");
    let hit = focused(&tree(cx));
    assert_eq!(hit.0, "ListBoxOption", "{hit:?}");
    assert!(hit.1.starts_with("notes.txt"), "{hit:?}");
    press(cx, "down");
    let t = tree(cx);
    assert_eq!(focused(&t), ("ListBoxOption".to_owned(), similar.clone()));
    assert_eq!(named(&t, &similar)["aria"]["selected"], Value::Bool(true));
    assert_eq!(named(&t, &similar)["aria"]["expanded"], Value::Bool(false));

    press(cx, "enter");
    wait_until(cx, &view, "open", |v| {
        v.similar_row() == Some((1, SimilarRow::Open)) && final_hits(v).len() == 2
    });
    let t = tree(cx);
    assert_eq!(focused(&t).1, similar, "still on the row");
    assert_eq!(named(&t, &similar)["aria"]["expanded"], Value::Bool(true));

    // A screen reader's Collapse and Expand work too.
    cx.simulate_a11y_action(node_id(&t, &similar), gpui::AccessibleAction::Collapse);
    wait_until(cx, &view, "closed", |v| {
        v.similar_row() == Some((1, SimilarRow::Closed))
    });
    let t = tree(cx);
    cx.simulate_a11y_action(node_id(&t, &similar), gpui::AccessibleAction::Expand);
    wait_until(cx, &view, "open again", |v| {
        v.similar_row() == Some((1, SimilarRow::Open))
    });
}

/// The node the focus is on as "Role label", for comparing across frames.
fn focus_desc(tree: &Value) -> Option<String> {
    let key = tree["active_descendant_focus"]
        .as_str()
        .or_else(|| tree["gpui_focus"].as_str())?;
    let node = &tree["nodes"][key];
    Some(format!("{} {}", role(node), label(node)))
}

/// Whether the keyboard focus is on the Settings categories (not just a page under them, whose
/// selected category is then read as focused).
fn on_categories(tree: &Value) -> bool {
    tree["gpui_focus"]
        .as_str()
        .is_some_and(|key| role(&tree["nodes"][key]) == "TabList")
}

/// Roles Tab stops at.
const KEYED: &[&str] = &[
    "Button",
    "CheckBox",
    "ComboBox",
    "Link",
    "RadioButton",
    "Slider",
    "Switch",
    "TextInput",
];

/// The controls on the Settings page as "Role label": what Tab must reach. The categories are one
/// stop of their own; the window's caption buttons and tabs aren't the page's.
fn settings_controls(tree: &Value) -> std::collections::BTreeSet<String> {
    let page = nodes(tree)
        .find(|n| role(n) == "Group" && label(n) == "Settings")
        .expect("the settings page");
    let mut out = std::collections::BTreeSet::new();
    let mut stack: Vec<&Value> = vec![page];
    while let Some(node) = stack.pop() {
        if matches!(role(node), "TabList") {
            continue;
        }
        if KEYED.contains(&role(node))
            && !matches!(
                label(node),
                "Minimize" | "Maximize" | "Restore" | "Close window"
            )
            && !(role(node) == "RadioButton"
                && node["aria"]["toggled"] != Value::String("True".into()))
        {
            out.insert(format!("{} {}", role(node), label(node)));
        }
        if let Some(children) = node["children"].as_array() {
            stack.extend(
                children
                    .iter()
                    .filter_map(|c| c.as_str())
                    .map(|k| &tree["nodes"][k]),
            );
        }
    }
    out
}

/// Tab reaches every control of Settings, on every page with every section open, and goes round
/// to the categories after the last one (#322).
#[gpui::test]
fn tab_reaches_every_settings_control(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.dispatch_action(crate::workspace::OpenSettings);
    let first = tree(cx);
    let pages: Vec<String> = nodes(&first)
        .filter(|n| role(n) == "Tab" && n["aria"]["selected"].is_boolean())
        .map(|n| label(n).to_owned())
        .filter(|l| l != "w" && l != "Settings")
        .collect();
    let mut missed = Vec::new();
    for page in &pages {
        let t = tree(cx);
        cx.simulate_a11y_action(node_id(&t, page), gpui::AccessibleAction::Click);
        let mut seen = std::collections::HashSet::new();
        loop {
            let t = tree(cx);
            let next = nodes(&t).find(|n| {
                role(n) == "Button"
                    && n["aria"]["expanded"] == Value::Bool(false)
                    && !seen.contains(label(n))
            });
            let Some(next) = next else { break };
            seen.insert(label(next).to_owned());
            let id = accesskit::NodeId(next["accesskit_id"].as_str().unwrap().parse().unwrap());
            cx.simulate_a11y_action(id, gpui::AccessibleAction::Expand);
        }
        let t = tree(cx);
        let want = settings_controls(&t);
        // From the categories round to them again.
        if !on_categories(&tree(cx)) {
            cx.simulate_keystrokes("f6");
        }
        assert!(
            on_categories(&tree(cx)),
            "{page}: F6 goes to the categories"
        );
        let mut got = std::collections::BTreeSet::new();
        let mut path = Vec::new();
        let mut round = false;
        for _ in 0..want.len() + 20 {
            cx.simulate_keystrokes("tab");
            let t = tree(cx);
            let at = focus_desc(&t).unwrap_or_else(|| format!("none ({:?})", t["gpui_focus"]));
            path.push(at.clone());
            if on_categories(&t) {
                round = true;
                break;
            }
            got.insert(at);
        }
        assert!(
            round,
            "{page}: Tab goes round to the categories; went {path:#?}"
        );
        missed.extend(want.difference(&got).map(|d| format!("{page} › {d}")));
    }
    assert_eq!(missed, Vec::<String>::new(), "not reached with Tab");
}

/// The settings as they are now.
fn settings_now(cx: &mut VisualTestContext) -> crate::settings::Settings {
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .clone()
    })
}

/// Presses and releases `key`, as a person does: GPUI presses the focused control on the release
/// of Space or Enter, which `simulate_keystrokes` doesn't send.
fn press(cx: &mut VisualTestContext, key: &'static str) {
    cx.simulate_keystrokes(key);
    let keystroke = gpui::Keystroke::parse(key).unwrap();
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }),
            cx,
        );
    });
    cx.run_until_parked();
}

/// Opens Settings on the category `down` steps below the first, with the focus on the categories.
fn settings_at(cx: &mut VisualTestContext, down: usize) {
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.simulate_keystrokes("f6");
    assert!(on_categories(&tree(cx)), "F6 goes to the categories");
    for _ in 0..down {
        cx.simulate_keystrokes("down");
    }
}

/// Up and Down on the categories show each page; Space presses a switch; Enter opens a dropdown,
/// whose arrows and Enter pick (#322).
#[gpui::test]
fn settings_work_from_the_keyboard(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    settings_at(cx, 1);
    let t = tree(cx);
    assert_eq!(focus_desc(&t).as_deref(), Some("Tab Files and folders"));

    cx.simulate_keystrokes("tab");
    assert_eq!(
        focus_desc(&tree(cx)).as_deref(),
        Some("Switch Show hidden items")
    );
    let before = settings_now(cx).show_hidden;
    press(cx, "space");
    assert_eq!(
        settings_now(cx).show_hidden,
        !before,
        "Space toggles the switch"
    );
    press(cx, "enter");
    assert_eq!(settings_now(cx).show_hidden, before, "and so does Enter");

    cx.simulate_keystrokes("shift-tab");
    assert!(
        on_categories(&tree(cx)),
        "Shift+Tab goes back to the categories"
    );
    cx.simulate_keystrokes("down tab");
    assert_eq!(focus_desc(&tree(cx)).as_deref(), Some("ComboBox View"));
    let before = settings_now(cx).default_view;
    press(cx, "enter");
    assert_eq!(
        named(&tree(cx), "View")["aria"]["expanded"],
        Value::Bool(true),
        "Enter opens the list"
    );
    cx.simulate_keystrokes("down");
    press(cx, "enter");
    assert_ne!(
        settings_now(cx).default_view,
        before,
        "Down and Enter pick the next view"
    );
    assert_eq!(
        focus_desc(&tree(cx)).as_deref(),
        Some("ComboBox View"),
        "the focus stays"
    );
}

/// The theme tiles are one stop: the arrows pick the next or previous tile, and the picked one is
/// read as focused (#322).
#[gpui::test]
fn arrows_pick_a_theme(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    settings_at(cx, 0);
    // The Theme section is open from the start: its header, then its tiles.
    cx.simulate_keystrokes("tab tab");
    let first = focus_desc(&tree(cx)).expect("a tile has the focus");
    assert!(first.starts_with("RadioButton "), "{first}");
    let before = settings_now(cx).theme;
    cx.simulate_keystrokes("right");
    let next = focus_desc(&tree(cx)).expect("a tile has the focus");
    assert!(
        next.starts_with("RadioButton ") && next != first,
        "{first} then {next}"
    );
    assert_ne!(settings_now(cx).theme, before, "Right picks the next theme");
    cx.simulate_keystrokes("left");
    assert_eq!(focus_desc(&tree(cx)), Some(first), "Left picks it back");
    cx.simulate_keystrokes("tab");
    assert!(
        !focus_desc(&tree(cx))
            .unwrap_or_default()
            .starts_with("RadioButton "),
        "Tab leaves the tiles"
    );
}

/// A control Tab moves to below the end of the view comes into view (#322).
#[gpui::test]
fn tab_brings_the_control_into_view(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(420.)));
    settings_at(cx, 1);
    let view = cx.update(|window, _| window.viewport_size());
    let out = cx.debug_bounds("fast-copy").expect("the switch is drawn");
    assert!(
        out.bottom() > view.height,
        "it starts below the view: {out:?}"
    );
    let mut path = Vec::new();
    while path.len() < 60 {
        cx.simulate_keystrokes("tab");
        let at = focus_desc(&tree(cx)).unwrap_or_default();
        path.push(at.clone());
        if at == "Switch Fast copy" {
            break;
        }
    }
    assert_eq!(
        path.last().map(String::as_str),
        Some("Switch Fast copy"),
        "{path:?}"
    );
    let now = cx.debug_bounds("fast-copy").expect("the switch is drawn");
    assert!(
        now.top() >= gpui::px(0.) && now.bottom() <= view.height,
        "in view: {now:?} in {view:?}"
    );
}

/// Tab and Shift+Tab stay in the "Remove Arke" dialog over Settings (#322).
#[gpui::test]
fn tab_stays_in_the_remove_dialog(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.dispatch_action(crate::workspace::OpenSettings);
    let t = tree(cx);
    cx.simulate_a11y_action(node_id(&t, "About"), gpui::AccessibleAction::Click);
    let t = tree(cx);
    cx.simulate_a11y_action(node_id(&t, "Remove…"), gpui::AccessibleAction::Click);
    let t = tree(cx);
    let dialog = nodes(&t)
        .find(|n| role(n) == "AlertDialog")
        .expect("the dialog opens");
    let mut inside = std::collections::BTreeSet::new();
    let mut stack = vec![dialog];
    while let Some(node) = stack.pop() {
        inside.insert(format!("{} {}", role(node), label(node)));
        if let Some(children) = node["children"].as_array() {
            stack.extend(
                children
                    .iter()
                    .filter_map(|c| c.as_str())
                    .map(|k| &t["nodes"][k]),
            );
        }
    }
    let mut went = Vec::new();
    for key in [
        "tab",
        "tab",
        "tab",
        "tab",
        "tab",
        "shift-tab",
        "shift-tab",
        "shift-tab",
    ] {
        cx.simulate_keystrokes(key);
        went.push(focus_desc(&tree(cx)).unwrap_or_default());
    }
    assert!(
        went.iter().all(|d| inside.contains(d)),
        "{went:?} in {inside:?}"
    );
    assert!(
        went.iter().collect::<std::collections::BTreeSet<_>>().len() > 1,
        "Tab moves between the dialog's controls: {went:?}"
    );
}

/// A text field Tab moves to below the end of the view comes into view too (#322).
#[gpui::test]
fn tab_brings_a_text_field_into_view(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(520.)));
    settings_at(cx, 0);
    let view = cx.update(|window, _| window.viewport_size());
    let out = cx.debug_bounds("accent-code").expect("the field is drawn");
    assert!(
        out.bottom() > view.height,
        "it starts below the view: {out:?}"
    );
    let mut path = Vec::new();
    while path.len() < 20 {
        cx.simulate_keystrokes("tab");
        let at = focus_desc(&tree(cx)).unwrap_or_default();
        path.push(at.clone());
        if at.starts_with("TextInput ") {
            break;
        }
    }
    assert!(
        path.last().is_some_and(|d| d.starts_with("TextInput ")),
        "{path:?}"
    );
    let now = cx.debug_bounds("accent-code").expect("the field is drawn");
    assert!(
        now.top() >= gpui::px(0.) && now.bottom() <= view.height,
        "in view: {now:?} in {view:?}"
    );
}

/// What's new works from the keyboard: Tab goes through the versions and the links at the
/// bottom, each named, and Enter on the GitHub line opens it (#322).
#[gpui::test]
fn whats_new_links_work_from_the_keyboard(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    ws.update_in(cx, |ws, window, cx| {
        ws.open_whats_new(crate::workspace::whats_new_page::Mode::Browse, window, cx);
    });
    let page = tree(cx);
    assert_eq!(unnamed(&page), Vec::<String>::new(), "what's new");
    let changelog = format!("Link {}", t!("whats_new.changelog"));
    let star = format!("Link {}", t!("whats_new.star"));
    let mut path = Vec::new();
    for _ in 0..100 {
        cx.simulate_keystrokes("tab");
        let at = focus_desc(&tree(cx)).unwrap_or_default();
        if at == star {
            break;
        }
        path.push(at);
    }
    assert_eq!(
        focus_desc(&tree(cx)).as_deref(),
        Some(star.as_str()),
        "Tab reaches the GitHub line; went {path:#?}"
    );
    assert!(
        path.contains(&changelog),
        "and the changelog before it; went {path:#?}"
    );
    press(cx, "enter");
    assert_eq!(
        cx.update(|_, cx| crate::workspace::update_page::opened_urls_for_test(cx)),
        [crate::build_info::SOURCE_URL]
    );
}

/// #424: the current folder is a favorite: its row is the one read as selected, not the same
/// folder's row under This PC.
#[gpui::test]
fn a_favorite_that_is_the_current_folder_is_the_selected_row(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.pinned = vec![PathBuf::from(r"C:\w")], cx);
        });
    });
    cx.run_until_parked();
    let t = tree_with_here(cx);
    let selected = nodes(&t)
        .filter(|n| role(n) == "TreeItem" && label(n) == "w")
        .filter(|n| n["aria"]["selected"] == Value::Bool(true))
        .count();
    assert_eq!(selected, 1, "one row of w is the selected one");
}

/// Settings search (#400): the box is named, its hits are a named list whose items say the
/// setting and its page, and nothing on the way is unnamed.
#[gpui::test]
fn the_settings_search_is_named(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    let page = tree(cx);
    let find = t!("settings.find_setting");
    assert!(
        nodes(&page).any(|n| role(n) == "TextInput" && label(n) == find),
        "the box is named after its placeholder"
    );
    cx.simulate_keystrokes("ctrl-f");
    cx.simulate_input("hidden");
    let found = tree(cx);
    assert_eq!(unnamed(&found), Vec::<String>::new(), "settings search");
    assert!(
        nodes(&found).any(|n| role(n) == "ListBox" && label(n) == t!("a11y.results")),
        "the hits are a named list"
    );
    let item = crate::a11y::item_label(&t!("settings.show_hidden"), &[t!("settings.files")]);
    assert!(
        nodes(&found).any(|n| role(n) == "ListBoxOption" && label(n) == item),
        "{item:?} is listed"
    );
    assert!(
        nodes(&found).any(|n| role(n) == "Button" && label(n) == t!("a11y.clear_filter")),
        "the clear button is named"
    );
}

/// Home's cards are links named like the places they open (#322), and the card under the
/// keyboard's ring is what a screen reader reads as focused as the arrow keys move it.
#[gpui::test]
fn home_cards_are_named_and_read_as_the_keyboard_moves(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.favorites = Some(vec![PathBuf::from(r"C:\w")]), cx);
        });
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    let home = tree(cx);
    assert_eq!(unnamed(&home), Vec::<String>::new(), "Home");
    let link = |t: &Value, name: &str| nodes(t).any(|n| role(n) == "Link" && label(n) == name);
    assert!(link(&home, "w"), "the favorite's card");
    assert!(link(&home, "Local Disk (C:)"), "the drive's card");

    // The first key brings the ring up on the first card.
    cx.simulate_keystrokes("home");
    let t = tree(cx);
    assert_eq!(focused(&t), ("Link".to_owned(), "w".to_owned()));
    cx.simulate_keystrokes("down");
    let t = tree(cx);
    assert_eq!(
        focused(&t),
        ("Link".to_owned(), "Local Disk (C:)".to_owned())
    );
}

/// What the UI Automation scan does on Home: nothing is touched, the page has the keyboard and
/// the first card is what a screen reader reports as focused (no key pressed, no ring). Tab then
/// walks the page's sections, reaches the sidebar and comes back round; it never alternates
/// between two elements (#322).
#[gpui::test]
fn tab_from_the_first_home_card_walks_on(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.favorites = Some(vec![PathBuf::from(r"C:\w")]), cx);
        });
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    let start = focused(&tree(cx));
    assert_eq!(start.0, "Link", "{start:?}");
    let mut went = vec![start.clone()];
    let mut sidebar = false;
    for _ in 0..8 {
        cx.simulate_keystrokes("tab");
        went.push(focused(&tree(cx)));
        sidebar |= cx.update(|window, cx| ws.read(cx).stop_for_test(window, cx)) == "sidebar";
    }
    let mut distinct = went.clone();
    distinct.sort();
    distinct.dedup();
    assert!(distinct.len() > 2, "{went:?}");
    // The ring wasn't showing: the first Tab shows it on the first card (favorites) instead of
    // walking past it to the drives.
    assert_eq!(
        went[1], start,
        "the first Tab stays on the first card: {went:?}"
    );
    assert!(went[2..].iter().any(|w| *w != start), "then it walks on");
    assert!(sidebar, "Tab reaches the sidebar: {went:?}");
    assert!(went[1..].contains(&start), "and comes back: {went:?}");
}
