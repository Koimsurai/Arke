//! GPUI tests for the integration API (`api.rs`, `docs/integration.md`): the state a window
//! publishes, and `open` and `run` as the UI thread applies them. The `WM_COPYDATA` and Z-order
//! parts need real windows and are left thin (`third_party/gpui-pre-windows`, `api::z_order`).

use serde_json::{Value, json};

use super::*;
use crate::api::{self, Action, Probe, Snapshot, Target};

/// `C:\work` (a.txt, b.txt, c.txt), `C:\other` and `E:\backup`.
fn folders() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\work",
        vec![
            dir("sub"),
            file("a.txt", 1),
            file("b.txt", 2),
            file("c.txt", 3),
        ],
    );
    fs.insert_dir(r"C:\work\sub", vec![file("deep.txt", 1)]);
    fs.insert_dir(r"C:\other", vec![file("x.txt", 1)]);
    fs.insert_dir(r"E:\backup", vec![file("old.zip", 1)]);
    fs
}

fn start(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (ws, cx) = setup_workspace(cx, folders(), r"C:\work");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\work"));
    (ws, cx)
}

/// The `state` reply for this window alone.
fn state(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Value {
    cx.run_until_parked();
    let window = ws.update(cx, |ws, cx| ws.api_state_for_test(cx));
    api::state_json(&Snapshot {
        windows: vec![Arc::new(window)],
    })
}

/// Exactly one `active: true` among these.
fn one_active(items: &Value) -> usize {
    let items = items.as_array().unwrap();
    let active: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| i["active"] == true)
        .map(|(ix, _)| ix)
        .collect();
    assert_eq!(active.len(), 1, "one active item: {items:?}");
    active[0]
}

/// Answers a request with the files and folders of the test, and no windows.
struct TestEnv;

impl api::Env for TestEnv {
    fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }

    fn probe(&self, path: &Path) -> Probe {
        match path.to_str() {
            Some(r"C:\work" | r"C:\other" | r"E:\backup") => Probe::Folder,
            Some(r"C:\work\a.txt") => Probe::File,
            _ => Probe::Missing,
        }
    }
}

fn answer(request: &str) -> (Value, Option<Action>) {
    let answer = api::answer(request.as_bytes(), Target::LastUsed, &TestEnv);
    (
        serde_json::from_slice(&answer.reply).unwrap(),
        answer.action,
    )
}

#[gpui::test]
fn state_lists_tabs_and_both_sides_of_a_split(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    let left = ws.update(cx, |ws, _| ws.active_view().clone());
    left.update(cx, |e, cx| {
        e.select_in_folder(
            Path::new(r"C:\work"),
            vec![Arc::from("c.txt"), Arc::from("a.txt")],
            cx,
        );
    });
    // Split, with the other side on E:\backup.
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    let right = ws.update(cx, |ws, _| ws.pane_views()[1].clone());
    right.update(cx, |e, cx| {
        e.navigate(PathBuf::from(r"E:\backup"), NavKind::New, cx);
    });
    wait_until(cx, &right, "backup", loaded(r"E:\backup"));
    // A second tab, which is the active one.
    ws.update_in(cx, |ws, window, cx| {
        let at = ws.tab_views().count();
        ws.open_tab(PathBuf::from(r"C:\other"), at, true, window, cx);
    });
    let other = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &other, "other", loaded(r"C:\other"));

    let value = state(&ws, cx);
    assert_eq!(value["v"], 1);
    let window = &value["windows"][0];
    assert_eq!(window["active"], true);
    assert!(window["hwnd"].as_str().unwrap().starts_with("0x"));
    let tabs = &window["tabs"];
    assert_eq!(tabs.as_array().unwrap().len(), 2);
    assert_eq!(one_active(tabs), 1);

    let split = &tabs[0]["panes"];
    assert_eq!(split.as_array().unwrap().len(), 2);
    one_active(split);
    assert_eq!(
        split[0]["location"],
        json!({ "kind": "folder", "path": r"C:\work" })
    );
    // Full paths, in list order.
    assert_eq!(
        split[0]["selection"],
        json!([r"C:\work\a.txt", r"C:\work\c.txt"])
    );
    assert_eq!(
        split[1]["location"],
        json!({ "kind": "folder", "path": r"E:\backup" })
    );
    assert_eq!(split[1]["selection"], json!([]));

    let single = &tabs[1]["panes"];
    assert_eq!(single.as_array().unwrap().len(), 1);
    assert_eq!(single[0]["active"], true);
    assert_eq!(
        single[0]["location"],
        json!({ "kind": "folder", "path": r"C:\other" })
    );
}

#[gpui::test]
fn home_shows_as_its_own_kind(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    let value = state(&ws, cx);
    let pane = &value["windows"][0]["tabs"][0]["panes"][0];
    assert_eq!(pane["location"], json!({ "kind": "home" }));
    assert_eq!(pane["selection"], json!([]));
}

/// What a window publishes is what the API answers with, and it follows changes.
#[gpui::test]
fn the_published_state_follows_the_window(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    // A made-up window id and HWND, so windows of tests running alongside don't show.
    let (id, hwnd): (u64, isize) = (u64::MAX - 0x7E57, 0x7E57_A410);
    let selection_now = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        ws.update(cx, |ws, cx| ws.api_publish_for_test(id, hwnd, cx));
        let value = api::state_json(&api::snapshot(Some(&[hwnd][..])));
        assert_eq!(value["windows"].as_array().unwrap().len(), 1);
        value["windows"][0]["tabs"][0]["panes"][0]["selection"].clone()
    };
    assert_eq!(selection_now(cx), json!([]));
    assert_eq!(api::window_of(hwnd), Some(id));
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    view.update(cx, |e, cx| {
        e.select_in_folder(Path::new(r"C:\work"), vec![Arc::from("b.txt")], cx);
    });
    assert_eq!(selection_now(cx), json!([r"C:\work\b.txt"]));
    // Another item, as many selected as before.
    view.update(cx, |e, cx| {
        e.select_in_folder(Path::new(r"C:\work"), vec![Arc::from("c.txt")], cx);
    });
    assert_eq!(selection_now(cx), json!([r"C:\work\c.txt"]));
    // A window that closed is gone.
    api::forget(id);
    assert_eq!(api::window_of(hwnd), None);
    assert!(api::snapshot(Some(&[hwnd][..])).windows.is_empty());
}

#[gpui::test]
fn open_with_select_selects_once_listed(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    let (reply, action) = answer(r#"{"v":1,"cmd":"open","path":"C:\\other","select":["x.txt"]}"#);
    assert_eq!(reply, json!({ "v": 1, "ok": true }));
    let Some(Action::Open {
        path,
        new_tab,
        select,
        ..
    }) = action
    else {
        panic!("open queues an open");
    };
    let select: Vec<Arc<str>> = select.iter().map(|n| Arc::from(n.as_str())).collect();
    ws.update_in(cx, |ws, window, cx| {
        ws.api_open(path, new_tab, select, window, cx);
    });
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "other", loaded(r"C:\other"));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| selected_names(v)), vec!["x.txt"]);
    // In the same tab.
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 1);

    // A file: its folder, with it selected, here in a new tab.
    let (_, action) = answer(r#"{"v":1,"cmd":"open","path":"C:\\work\\a.txt","new_tab":true}"#);
    let Some(Action::Open {
        path,
        new_tab,
        select,
        ..
    }) = action
    else {
        panic!("open queues an open");
    };
    assert_eq!(path, PathBuf::from(r"C:\work"));
    let select: Vec<Arc<str>> = select.iter().map(|n| Arc::from(n.as_str())).collect();
    ws.update_in(cx, |ws, window, cx| {
        ws.api_open(path, new_tab, select, window, cx);
    });
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "work", loaded(r"C:\work"));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| selected_names(v)), vec!["a.txt"]);
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);

    // Selecting in the folder already shown.
    ws.update_in(cx, |ws, window, cx| {
        ws.api_open(
            PathBuf::from(r"C:\work"),
            false,
            vec![Arc::from("b.txt")],
            window,
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| selected_names(v)), vec!["b.txt"]);

    let (reply, action) = answer(r#"{"v":1,"cmd":"open","path":"C:\\nowhere"}"#);
    assert_eq!(reply["error"]["code"], "not_found");
    assert!(action.is_none());
}

#[gpui::test]
fn run_dispatches_allowed_commands_and_refuses_the_others(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    let (reply, action) = answer(r#"{"v":1,"cmd":"run","id":"new_tab"}"#);
    assert_eq!(reply["ok"], true);
    let Some(Action::Run { id, .. }) = action else {
        panic!("run queues a run");
    };
    let command = crate::commands::find(id).unwrap();
    ws.update_in(cx, |ws, window, cx| ws.api_run(command, window, cx));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);

    for blocked in [
        "paste",
        "delete",
        "delete_permanently",
        "new_folder",
        "undo",
    ] {
        let (reply, action) = answer(&format!(r#"{{"v":1,"cmd":"run","id":"{blocked}"}}"#));
        assert_eq!(reply["error"]["code"], "not_allowed", "{blocked}");
        assert!(action.is_none());
    }
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);
}

#[gpui::test]
fn commands_lists_what_run_takes(cx: &mut TestAppContext) {
    let (_ws, _cx) = start(cx);
    let (reply, _) = answer(r#"{"v":1,"cmd":"commands"}"#);
    let commands = reply["commands"].as_array().unwrap();
    let ids: Vec<&str> = commands.iter().map(|c| c["id"].as_str().unwrap()).collect();
    for command in crate::commands::COMMANDS {
        assert_eq!(
            ids.contains(&command.id),
            !command.changes_files,
            "{}",
            command.id
        );
    }
    let refresh = commands.iter().find(|c| c["id"] == "refresh").unwrap();
    assert_eq!(refresh["name"], &*t!("cmd.refresh"));
}
