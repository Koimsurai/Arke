//! GPUI tests for the folder tree in the sidebar (`workspace_sidebar.rs`): double-clicking a row
//! expands it, as in File Explorer's navigation pane; the tree follows the current folder (#241);
//! This PC collapses to its heading.

use std::sync::Arc;

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};

use super::*;
use crate::settings::SettingsStore;
use crate::shell::{Place, PlaceKind};

/// The file system the tree is drawn from: `C:\` with `music` and `work\reports\2026`.
fn fs() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("music"), dir("work")]);
    fs.insert_dir(r"C:\work", vec![dir("reports"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\work\reports", vec![dir("2026")]);
    fs.insert_dir(r"C:\work\reports\2026", vec![file("q4.pdf", 1)]);
    fs.insert_dir(r"C:\music", Vec::new());
    fs
}

/// A window on `start`, with the drive `C:` under This PC.
fn window_at<'a>(
    cx: &'a mut TestAppContext,
    fs: Arc<FakeFs>,
    start: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(Place {
        label: "Local Disk (C:)".into(),
        path: PathBuf::from(r"C:\"),
        kind: PlaceKind::Drive {
            remote: false,
            space: None,
        },
    });
    let (ws, cx) = setup_workspace_with(cx, fs, shell, start);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(start)(v) && v.places.len() > 1
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    refresh(cx);
    (ws, cx)
}

/// A window on `C:\work`.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    window_at(cx, fs(), r"C:\work")
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

/// Draws frames until `done`: the tree follows the folder after a frame and lists folders in the
/// background.
fn settle(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(cx) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        refresh(cx);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Where the sidebar draws the row with this label.
fn row(cx: &mut VisualTestContext, label: &str) -> gpui::Point<gpui::Pixels> {
    refresh(cx);
    let id: &'static str = Box::leak(format!("sidebar {label}").into_boxed_str());
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("no sidebar row {label}"))
        .center()
}

/// The sidebar draws a row with this label.
fn shows(cx: &mut VisualTestContext, label: &str) -> bool {
    let id: &'static str = Box::leak(format!("sidebar {label}").into_boxed_str());
    cx.debug_bounds(id).is_some()
}

fn expanded(ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: &str) -> bool {
    ws.update(cx, |ws, _| ws.tree_expanded_for_test(Path::new(path)))
}

/// Opens `path` in the active pane, as going there in any way does.
fn open(ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: &str) {
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(path), NavKind::New, cx);
    });
    wait_until(cx, &view, path, loaded(path));
}

fn click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>, click_count: usize) {
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count,
    });
    cx.run_until_parked();
}

fn double_click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    click(cx, at, 1);
    click(cx, at, 2);
}

fn this_pc_collapsed(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .is_collapsed(crate::settings::SidebarSection::ThisPc)
    })
}

/// #242: a double-click on a tree row expands it (and a second one collapses it); the first click
/// still opens the folder, and a single click doesn't expand anything.
#[gpui::test]
fn double_clicking_a_tree_row_expands_it(cx: &mut TestAppContext) {
    // On the drive itself, so following the folder (#241) has nothing to open.
    let (ws, cx) = window_at(cx, fs(), r"C:\music");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    let drive = PathBuf::from(r"C:\");

    // Following `C:\music` opened the drive; the drive itself needs nothing open.
    let at = row(cx, "Local Disk (C:)");
    click(cx, at, 1);
    wait_until(cx, &view, "the drive opens", loaded(r"C:\"));
    settle(cx, "the drive closes again", |cx| {
        !expanded(&ws, cx, r"C:\")
    });
    refresh(cx);
    assert!(
        !expanded(&ws, cx, r"C:\"),
        "a single click only opens the drive"
    );

    let at = row(cx, "Local Disk (C:)");
    double_click(cx, at);
    refresh(cx);
    assert!(expanded(&ws, cx, r"C:\"), "a double-click expands it");
    let deadline = Instant::now() + Duration::from_secs(20);
    while ws
        .update(cx, |ws, _| ws.tree_children_for_test(&drive))
        .is_none()
    {
        assert!(Instant::now() < deadline, "subfolders not listed");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    // Its subfolders are rows now.
    let _ = row(cx, "work");

    let at = row(cx, "Local Disk (C:)");
    double_click(cx, at);
    refresh(cx);
    assert!(
        !expanded(&ws, cx, r"C:\"),
        "another double-click collapses it"
    );
}

/// #241: the tree opens down to the folder the active pane shows and marks it; what it opened
/// closes again once the folder is elsewhere, while folders the user opened stay open.
#[gpui::test]
fn the_tree_follows_the_current_folder(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    settle(cx, "the drive opens to work", |cx| {
        expanded(&ws, cx, r"C:\") && shows(cx, "work")
    });
    assert!(
        !expanded(&ws, cx, r"C:\work"),
        "the current folder itself stays closed"
    );
    assert_eq!(
        ws.update(cx, |ws, cx| ws.sidebar_here_for_test(cx)),
        Some(PathBuf::from(r"C:\work")),
        "and is the one marked"
    );

    open(&ws, cx, r"C:\work\reports\2026");
    settle(cx, "the tree opens down to 2026", |cx| {
        expanded(&ws, cx, r"C:\work") && expanded(&ws, cx, r"C:\work\reports") && shows(cx, "2026")
    });

    // A folder the user opens is theirs.
    ws.update(cx, |ws, cx| {
        ws.toggle_expand_for_test(PathBuf::from(r"C:\music"), cx);
    });
    assert!(expanded(&ws, cx, r"C:\music"));

    // Back on a folder right under the drive: what the tree opened for 2026 closes.
    open(&ws, cx, r"C:\music");
    settle(cx, "the folders opened for 2026 close", |cx| {
        !expanded(&ws, cx, r"C:\work")
    });
    assert!(!expanded(&ws, cx, r"C:\work\reports"));
    assert!(expanded(&ws, cx, r"C:\"), "music is under the drive");
    assert!(
        expanded(&ws, cx, r"C:\music"),
        "the user's folder stays open"
    );
    refresh(cx);
    assert!(!shows(cx, "2026"));
}

/// A branch the tree opened by itself and the user then closed stays closed while the folder
/// doesn't change, and is the user's from then on.
#[gpui::test]
fn closing_a_followed_branch_is_left_alone(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    settle(cx, "the drive opens", |cx| expanded(&ws, cx, r"C:\"));
    ws.update(cx, |ws, cx| {
        ws.toggle_expand_for_test(PathBuf::from(r"C:\"), cx);
    });
    for _ in 0..5 {
        refresh(cx);
    }
    assert!(
        !expanded(&ws, cx, r"C:\"),
        "not opened again behind the user"
    );
}

/// The This PC heading collapses the section (remembered); while it is collapsed the tree doesn't
/// follow, and expanded again it catches up.
#[gpui::test]
fn this_pc_collapses_and_stops_following(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    settle(cx, "the drive opens", |cx| expanded(&ws, cx, r"C:\"));
    assert!(shows(cx, "Local Disk (C:)"));

    let header = cx
        .debug_bounds("this-pc-header")
        .expect("This PC has a heading")
        .center();
    click(cx, header, 1);
    refresh(cx);
    assert!(this_pc_collapsed(cx), "kept in the settings");
    assert!(!shows(cx, "Local Disk (C:)"), "the drives are hidden");

    open(&ws, cx, r"C:\work\reports");
    for _ in 0..5 {
        refresh(cx);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !expanded(&ws, cx, r"C:\work"),
        "nothing is opened while This PC is collapsed"
    );

    let header = cx.debug_bounds("this-pc-header").unwrap().center();
    click(cx, header, 1);
    assert!(!this_pc_collapsed(cx));
    settle(cx, "the tree catches up with reports", |cx| {
        expanded(&ws, cx, r"C:\work") && shows(cx, "reports")
    });
    assert!(shows(cx, "Local Disk (C:)"));
}

/// The row of a folder deep in a long list is scrolled into view when the tree opens to it.
#[gpui::test]
fn the_current_folder_is_scrolled_into_view(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("work")]);
    let mut many: Vec<Entry> = (1..=40).map(|i| dir(&format!("d{i:02}"))).collect();
    many.push(dir("zeta"));
    fs.insert_dir(r"C:\work", many);
    fs.insert_dir(r"C:\work\zeta", vec![dir("deep")]);
    fs.insert_dir(r"C:\work\zeta\deep", Vec::new());
    let (ws, cx) = window_at(cx, fs, r"C:\");
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(480.)));
    refresh(cx);

    open(&ws, cx, r"C:\work\zeta\deep");
    settle(cx, "the row of deep is in view", |cx| {
        let (Some(frame), Some(row)) = (
            cx.debug_bounds("sidebar-places-frame"),
            cx.debug_bounds("sidebar deep"),
        ) else {
            return false;
        };
        row.top() >= frame.top() - gpui::px(1.) && row.bottom() <= frame.bottom() + gpui::px(1.)
    });
    assert!(
        ws.update(cx, |ws, _| ws.places_overflow_for_test()) > gpui::px(0.),
        "the places had to scroll"
    );
}

/// A long `C:\work` with `zeta\deep` at its end, the window short: following `deep` has to scroll.
fn deep_window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("work")]);
    let mut many: Vec<Entry> = (1..=40).map(|i| dir(&format!("d{i:02}"))).collect();
    many.push(dir("zeta"));
    fs.insert_dir(r"C:\work", many);
    fs.insert_dir(r"C:\work\zeta", vec![dir("deep")]);
    fs.insert_dir(r"C:\work\zeta\deep", Vec::new());
    let (ws, cx) = window_at(cx, fs, r"C:\");
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(480.)));
    refresh(cx);
    (ws, cx)
}

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings) + 'static) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

/// #307: working in a folder inside a favorite, the sidebar stays on the favorite: the tree
/// opens below, out of sight, but doesn't scroll away to the folder.
#[gpui::test]
fn inside_a_favorite_the_sidebar_stays_on_it(cx: &mut TestAppContext) {
    let (ws, cx) = deep_window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![PathBuf::from(r"C:\work")]);
    });
    open(&ws, cx, r"C:\work\zeta\deep");
    settle(cx, "the tree opens to deep", |cx| {
        expanded(&ws, cx, r"C:\work\zeta")
    });
    for _ in 0..5 {
        refresh(cx);
    }
    assert!(
        ws.update(cx, |ws, _| ws.places_overflow_for_test()) > gpui::px(0.),
        "deep is out of sight"
    );
    assert_eq!(
        ws.update(cx, |ws, _| ws.places_offset_for_test()),
        gpui::px(0.),
        "the sidebar stays on the favorite"
    );
}

/// #299: following the folder turned off, the tree opens only where the user opens it.
#[gpui::test]
fn following_the_folder_can_be_turned_off(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    settle(cx, "the drive opens to work", |cx| {
        expanded(&ws, cx, r"C:\")
    });
    change(cx, |st| st.sidebar_follow = false);
    open(&ws, cx, r"C:\work\reports\2026");
    for _ in 0..5 {
        refresh(cx);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!expanded(&ws, cx, r"C:\work"), "nothing more is opened");
    assert!(expanded(&ws, cx, r"C:\"), "what was open stays open");
}

/// The favorite tiles are drawn whole under Home while the tree opens to the current folder on
/// start: following the folder doesn't scroll them away or leave them cut.
#[gpui::test]
fn the_tiles_stay_in_view_while_the_tree_follows(cx: &mut TestAppContext) {
    let (_ws, cx) = window_at(cx, fs(), r"C:\work\reports\2026");
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.favorites =
                        Some(vec![PathBuf::from(r"C:\music"), PathBuf::from(r"C:\work")]);
                },
                cx,
            );
        });
    });
    settle(cx, "the tree opened to 2026", |cx| shows(cx, "2026"));
    refresh(cx);
    let frame = cx
        .debug_bounds("sidebar-places-frame")
        .expect("the places are drawn");
    let tiles = cx.debug_bounds("pin-tiles").expect("the tiles are drawn");
    assert!(tiles.size.height > gpui::px(40.), "{tiles:?}");
    for slot in ["tile-slot-0", "tile-slot-1"] {
        let tile = cx.debug_bounds(slot).expect("a tile");
        assert!(
            tile.top() >= frame.top() - gpui::px(1.)
                && tile.bottom() <= frame.bottom() + gpui::px(1.),
            "{slot} {tile:?} in {frame:?}"
        );
    }
}
