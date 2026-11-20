//! GPUI tests for the folder tree in the sidebar (`workspace_sidebar.rs`): double-clicking a row
//! expands it, as in File Explorer's navigation pane.

use std::sync::Arc;

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};

use super::*;
use crate::shell::{Place, PlaceKind};

/// A window on `C:\work`, with the drive `C:` under This PC.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("music"), dir("work")]);
    fs.insert_dir(r"C:\work", vec![dir("reports"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\work\reports", vec![dir("2026")]);
    fs.insert_dir(r"C:\work\reports\2026", vec![file("q4.pdf", 1)]);
    fs.insert_dir(r"C:\music", Vec::new());
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(Place {
        label: "Local Disk (C:)".into(),
        path: PathBuf::from(r"C:\"),
        kind: PlaceKind::Drive {
            remote: false,
            space: None,
        },
    });
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\work");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\work")(v) && v.places.len() > 1
    });
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

/// Where the sidebar draws the row with this label.
fn row(cx: &mut VisualTestContext, label: &str) -> gpui::Point<gpui::Pixels> {
    refresh(cx);
    let id: &'static str = Box::leak(format!("sidebar {label}").into_boxed_str());
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("no sidebar row {label}"))
        .center()
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

/// #242: a double-click on a tree row expands it (and a second one collapses it); the first click
/// still opens the folder, and a single click doesn't expand anything.
#[gpui::test]
fn double_clicking_a_tree_row_expands_it(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    let drive = PathBuf::from(r"C:\");
    let expanded = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.tree_expanded_for_test(Path::new(r"C:\")))
    };

    let at = row(cx, "Local Disk (C:)");
    click(cx, at, 1);
    wait_until(cx, &view, "the drive opens", loaded(r"C:\"));
    assert!(!expanded(&ws, cx), "a single click only opens the drive");

    let at = row(cx, "Local Disk (C:)");
    double_click(cx, at);
    assert!(expanded(&ws, cx), "a double-click expands it");
    let deadline = Instant::now() + Duration::from_secs(20);
    while ws.update(cx, |ws, _| ws.tree_children_for_test(&drive)).is_none() {
        assert!(Instant::now() < deadline, "subfolders not listed");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    // Its subfolders are rows now.
    let _ = row(cx, "work");

    let at = row(cx, "Local Disk (C:)");
    double_click(cx, at);
    assert!(!expanded(&ws, cx), "another double-click collapses it");
}
