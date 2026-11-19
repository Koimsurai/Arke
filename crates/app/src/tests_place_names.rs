//! GPUI tests for renaming mapped network drives and network places (`place_names.rs`,
//! `workspace_placename.rs`): the menu entry, the box in the sidebar and on Home, F2, and the
//! name showing wherever the drive's name does.

use std::sync::Arc;

use super::*;
use crate::settings::SettingsStore;
use crate::shell::{Place, PlaceKind};

const DRIVE: &str = r"Z:\";
const SHARE: &str = r"\\nas\docs";

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings) + 'static) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

fn names(cx: &mut VisualTestContext) -> Vec<(String, String)> {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .place_names
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
}

/// A window on `C:\a`, with a mapped drive `Z:` (default name `docs (\\nas)`), a local drive and
/// the share `\\nas\docs` among the recent network places.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\a", 1);
    fs.insert_dir(DRIVE, Vec::new());
    fs.insert_dir(SHARE, Vec::new());
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().extend([
        Place {
            label: "Local Disk (C:)".into(),
            path: PathBuf::from(r"C:\"),
            kind: PlaceKind::Drive {
                remote: false,
                space: None,
            },
        },
        Place {
            label: r"docs (\\nas) (Z:)".into(),
            path: PathBuf::from(DRIVE),
            kind: PlaceKind::Drive {
                remote: true,
                space: None,
            },
        },
    ]);
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\a");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\a")(v) && v.places.len() > 2
    });
    change(cx, |st| st.net_places = vec![PathBuf::from(SHARE)]);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    refresh(cx);
    (ws, cx)
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

/// The sidebar draws a row with this label.
fn sidebar_shows(cx: &mut VisualTestContext, label: &str) -> bool {
    refresh(cx);
    let id: &'static str = Box::leak(format!("sidebar {label}").into_boxed_str());
    cx.debug_bounds(id).is_some()
}

/// Starts renaming from the place's menu, types `name` and presses Enter.
fn rename(ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: &str, name: &str) {
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_place_from_menu_for_test(Path::new(path), window, cx);
    });
    let input = ws
        .update(cx, |ws, _| ws.place_rename_for_test(Path::new(path)))
        .expect("the box is open");
    input.update(cx, |input, cx| input.set_text(name.to_owned(), cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
}

/// Mapped drives and network places have "Rename" in their menu; local drives and folders don't.
#[gpui::test]
fn only_mapped_drives_and_network_places_can_be_renamed(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let rename = t!("menu.rename").to_string();
    let has = |ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: &str| {
        let path = PathBuf::from(path);
        ws.update(cx, |ws, cx| ws.place_menu_labels_for_test(&path, cx))
            .contains(&rename)
    };
    assert!(has(&ws, cx, DRIVE), "mapped drive");
    assert!(has(&ws, cx, SHARE), "recent network place");
    assert!(!has(&ws, cx, r"C:\"), "local drive");
    assert!(!has(&ws, cx, r"C:\a"), "folder");
}

/// The box starts with the drive's own name (without its letter); the new name shows in the
/// sidebar with the letter, and Windows is not asked to change anything.
#[gpui::test]
fn a_mapped_drive_is_renamed_in_the_sidebar(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    assert!(sidebar_shows(cx, r"docs (\\nas) (Z:)"));
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_place_from_menu_for_test(Path::new(DRIVE), window, cx);
    });
    refresh(cx);
    let input = ws
        .update(cx, |ws, _| ws.place_rename_for_test(Path::new(DRIVE)))
        .expect("the box is open");
    assert_eq!(
        input.read_with(cx, |i, _| i.text().to_owned()),
        r"docs (\\nas)"
    );
    assert!(
        cx.debug_bounds("place-rename-box").is_some(),
        "the sidebar row turned into a box"
    );
    input.update(cx, |input, cx| input.set_text("Backup", cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        names(cx),
        vec![("z:".to_owned(), "Backup".to_owned())],
        "kept in Arke's settings"
    );
    assert!(ws.update(cx, |ws, _| {
        ws.place_rename_for_test(Path::new(DRIVE)).is_none()
    }));
    assert!(sidebar_shows(cx, "Backup (Z:)"));
    assert!(!sidebar_shows(cx, r"docs (\\nas) (Z:)"));
}

/// An empty name, or the default one, brings the default name back.
#[gpui::test]
fn an_empty_name_restores_the_default(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    rename(&ws, cx, DRIVE, "Backup");
    assert!(sidebar_shows(cx, "Backup (Z:)"));
    // The box starts with the custom name.
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_place_from_menu_for_test(Path::new(DRIVE), window, cx);
    });
    let input = ws
        .update(cx, |ws, _| ws.place_rename_for_test(Path::new(DRIVE)))
        .unwrap();
    assert_eq!(input.read_with(cx, |i, _| i.text().to_owned()), "Backup");
    input.update(cx, |input, cx| input.set_text("   ", cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(names(cx).is_empty());
    assert!(sidebar_shows(cx, r"docs (\\nas) (Z:)"));
    assert!(!sidebar_shows(cx, "Backup (Z:)"));

    rename(&ws, cx, SHARE, "Team");
    assert_eq!(names(cx).len(), 1);
    rename(&ws, cx, SHARE, "docs (nas)");
    assert!(names(cx).is_empty(), "typing the default name restores it");
}

/// Escape leaves the name as it was.
#[gpui::test]
fn escape_keeps_the_old_name(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_place_from_menu_for_test(Path::new(DRIVE), window, cx);
    });
    let input = ws
        .update(cx, |ws, _| ws.place_rename_for_test(Path::new(DRIVE)))
        .unwrap();
    input.update(cx, |input, cx| input.set_text("Nope", cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(names(cx).is_empty());
    assert!(ws.update(cx, |ws, _| {
        ws.place_rename_for_test(Path::new(DRIVE)).is_none()
    }));
    assert!(sidebar_shows(cx, r"docs (\\nas) (Z:)"));
}

/// A recent network place takes the name as it is; forgetting the place forgets the name.
#[gpui::test]
fn a_network_place_is_renamed_and_forgotten_with_its_name(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    assert!(sidebar_shows(cx, "docs (nas)"));
    rename(&ws, cx, SHARE, "Team files");
    assert_eq!(
        names(cx),
        vec![(r"\\nas\docs".to_owned(), "Team files".to_owned())]
    );
    assert!(sidebar_shows(cx, "Team files"));
    change(cx, |st| st.forget_share(Path::new(SHARE)));
    assert!(names(cx).is_empty());
}

/// The name is the drive's name everywhere: tab title, window title, breadcrumb root, and the
/// search scope chip.
#[gpui::test]
fn the_name_shows_wherever_the_drive_does(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    rename(&ws, cx, DRIVE, "Backup");
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(DRIVE), NavKind::New, cx);
    });
    wait_until(cx, &view, "drive", loaded(DRIVE));
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |v, _| v.tab_title().to_string()),
        "Backup (Z:)"
    );
    assert_eq!(cx.window_title().as_deref(), Some("Backup (Z:) - Arke"));
    let crumbs = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window));
    assert_eq!(crumbs, ["Backup (Z:)"]);
    view.update(cx, |v, _| v.search_in_for_test(PathBuf::from(DRIVE)));
    assert_eq!(
        view.read_with(cx, |v, _| v.search_in_name()).as_deref(),
        Some("Backup (Z:)")
    );

    // Back to the default everywhere.
    rename(&ws, cx, DRIVE, "");
    assert_eq!(view.read_with(cx, |v, _| v.tab_title().to_string()), r"Z:\");
    assert_eq!(cx.window_title().as_deref(), Some(r"Z:\ - Arke"));
    let crumbs = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window));
    assert_eq!(crumbs, ["Z:"]);
    assert_eq!(
        view.read_with(cx, |v, _| v.search_in_name()).as_deref(),
        Some(r"Z:\")
    );

    // A network place: its share name becomes the name, folders below it keep theirs.
    rename(&ws, cx, SHARE, "Team files");
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(SHARE), NavKind::New, cx);
    });
    wait_until(cx, &view, "share", loaded(SHARE));
    assert_eq!(
        view.read_with(cx, |v, _| v.tab_title().to_string()),
        "Team files"
    );
    let crumbs = view.update_in(cx, |v, window, _| v.crumb_labels_for_test(window));
    assert_eq!(crumbs, ["nas", "Team files"]);
}

/// F2 with nothing to rename in the list renames the mapped drive that is open; in an ordinary
/// folder it does nothing.
#[gpui::test]
fn f2_renames_the_open_mapped_drive(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    assert!(
        ws.update(cx, |ws, _| ws
            .place_rename_for_test(Path::new(r"C:\a"))
            .is_none()),
        "a plain folder has nothing to rename"
    );

    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(DRIVE), NavKind::New, cx);
    });
    wait_until(cx, &view, "drive", loaded(DRIVE));
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    let input = ws
        .update(cx, |ws, _| ws.place_rename_for_test(Path::new(DRIVE)))
        .expect("F2 opened the box");
    input.update(cx, |input, cx| input.set_text("Backup", cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(names(cx), vec![("z:".to_owned(), "Backup".to_owned())]);
}

/// On Home the drive's card is the box, and it shows the new name afterwards.
#[gpui::test]
fn a_mapped_drive_is_renamed_on_home(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    refresh(cx);
    assert!(cx.debug_bounds(r"home-card docs (\\nas) (Z:)").is_some());
    ws.update_in(cx, |ws, window, cx| {
        ws.rename_place_from_menu_for_test(Path::new(DRIVE), window, cx);
    });
    refresh(cx);
    assert!(
        cx.debug_bounds("place-rename-box").is_some(),
        "the card is the box"
    );
    let input = ws
        .update(cx, |ws, _| ws.place_rename_for_test(Path::new(DRIVE)))
        .unwrap();
    input.update(cx, |input, cx| input.set_text("Backup", cx));
    cx.simulate_keystrokes("enter");
    refresh(cx);
    assert!(cx.debug_bounds("home-card Backup (Z:)").is_some());
    assert!(cx.debug_bounds("place-rename-box").is_none());
}

/// The collapsed sidebar names the drive too.
#[gpui::test]
fn the_collapsed_sidebar_uses_the_name(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    rename(&ws, cx, DRIVE, "Backup");
    change(cx, |st| st.sidebar_collapsed = true);
    refresh(cx);
    assert!(cx.debug_bounds("rail Backup (Z:)").is_some());
}
