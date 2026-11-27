//! GPUI tests for how a quick run of clicks counts (#425): GPUI keeps counting 3, 4, 5... while
//! the clicks stay close in time and place, but only the even ones are double-clicks on Windows.

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, point, px};

use super::*;
use crate::press_guard::is_double_click;
use crate::settings::Settings;

fn single_click(on: bool) {
    crate::prefs::apply(&Settings {
        single_click: on,
        ..Settings::default()
    });
}

/// One press and release of the left button with this click count.
fn click(cx: &mut VisualTestContext, at: Point<Pixels>, modifiers: Modifiers, click_count: usize) {
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers,
        click_count,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers,
        click_count,
    });
    cx.run_until_parked();
}

#[test]
fn only_even_counts_from_two_are_double_clicks() {
    let table = [
        (0, false),
        (1, false),
        (2, true),
        (3, false),
        (4, true),
        (5, false),
        (6, true),
        (7, false),
    ];
    for (count, expected) in table {
        assert_eq!(is_double_click(count), expected, "count {count}");
    }
}

/// Clicking again and again in one spot: the 2nd click opens the folder, the 3rd (a plain click
/// on whatever is under the cursor in the new folder) opens nothing, the 4th is a double-click.
#[gpui::test]
fn a_run_of_clicks_opens_on_the_even_ones_only(cx: &mut TestAppContext) {
    single_click(false);
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one")]);
    fs.insert_dir(r"C:\t\one", vec![dir("inner")]);
    fs.insert_many(r"C:\t\one\inner", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = point(bounds.left() + px(60.), bounds.top() + row * 0.5);

    click(cx, at, Modifiers::none(), 1);
    assert!(
        view.update(cx, |v, _| loaded(r"C:\t")(v)),
        "1: only selects"
    );
    click(cx, at, Modifiers::none(), 2);
    wait_until(cx, &view, "2: opens", loaded(r"C:\t\one"));
    click(cx, at, Modifiers::none(), 3);
    assert!(
        view.update(cx, |v, _| loaded(r"C:\t\one")(v)),
        "3: a plain click, nothing opens"
    );
    click(cx, at, Modifiers::none(), 4);
    wait_until(cx, &view, "4: opens", loaded(r"C:\t\one\inner"));
}

/// Ctrl+double-click opens a new tab on the 2nd and the 4th click of a run, not on the 3rd.
#[gpui::test]
fn ctrl_double_click_opens_a_tab_on_even_counts_only(cx: &mut TestAppContext) {
    single_click(false);
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("a"), dir("b"), dir("c")]);
    for name in ["a", "b", "c"] {
        fs.insert_many(format!(r"C:\t\{name}"), 1);
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = |n: f32| point(bounds.left() + px(60.), bounds.top() + row * (n + 0.5));
    let ctrl = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    let tabs = |cx: &mut VisualTestContext| ws.update(cx, |ws, cx| tab_paths(ws, cx).len());
    assert_eq!(tabs(cx), 1);

    click(cx, at(0.), ctrl, 1);
    click(cx, at(0.), ctrl, 2);
    assert_eq!(tabs(cx), 2, "count 2");
    click(cx, at(1.), ctrl, 3);
    assert_eq!(tabs(cx), 2, "count 3 is a plain Ctrl+click");
    click(cx, at(2.), ctrl, 4);
    assert_eq!(tabs(cx), 3, "count 4");
    let mut paths = ws.update(cx, |ws, cx| tab_paths(ws, cx));
    paths.sort();
    assert_eq!(
        paths,
        [
            Some(PathBuf::from(r"C:\t")),
            Some(PathBuf::from(r"C:\t\a")),
            Some(PathBuf::from(r"C:\t\c")),
        ]
    );
}

/// Turns "Open items with a single click" on in the settings store, not only in the list prefs:
/// the workspace applies the stored settings again whenever they change (navigating can change
/// them), which would switch a prefs-only setting back off in the middle of a test.
fn store_single_click(cx: &mut VisualTestContext) {
    use crate::settings::SettingsStore;
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.single_click = true, cx));
        crate::prefs::apply(&SettingsStore::global(cx).read(cx).settings);
    });
}

/// A folder tree where each level has one subfolder: `C:\t\a\b\c\d`.
fn nested_fs() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("a")]);
    fs.insert_dir(r"C:\t\a", vec![dir("b")]);
    fs.insert_dir(r"C:\t\a\b", vec![dir("c")]);
    fs.insert_dir(r"C:\t\a\b\c", vec![dir("d")]);
    fs.insert_dir(r"C:\t\a\b\c\d", Vec::new());
    fs
}

/// "Open items with a single click": every click of a quick run opens, so three clicks at one spot
/// go down three levels, as in File Explorer (#425).
#[gpui::test]
fn single_click_mode_opens_on_every_click_of_a_run(cx: &mut TestAppContext) {
    let (ws, cx) = setup_workspace(cx, nested_fs(), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    store_single_click(cx);
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = point(bounds.left() + px(60.), bounds.top() + row * 0.5);

    click(cx, at, Modifiers::none(), 1);
    wait_until(cx, &view, "1: a", loaded(r"C:\t\a"));
    click(cx, at, Modifiers::none(), 2);
    wait_until(cx, &view, "2: b", loaded(r"C:\t\a\b"));
    click(cx, at, Modifiers::none(), 3);
    wait_until(cx, &view, "3: c", loaded(r"C:\t\a\b\c"));
    single_click(false);
}

/// Double-click mode: of a run of four clicks, the 2nd and the 4th open, two levels (#425).
#[gpui::test]
fn double_click_mode_opens_on_even_counts_in_nested_folders(cx: &mut TestAppContext) {
    single_click(false);
    let (ws, cx) = setup_workspace(cx, nested_fs(), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = point(bounds.left() + px(60.), bounds.top() + row * 0.5);

    click(cx, at, Modifiers::none(), 1);
    assert!(
        view.update(cx, |v, _| loaded(r"C:\t")(v)),
        "1: only selects"
    );
    click(cx, at, Modifiers::none(), 2);
    wait_until(cx, &view, "2: a", loaded(r"C:\t\a"));
    click(cx, at, Modifiers::none(), 3);
    assert!(view.update(cx, |v, _| loaded(r"C:\t\a")(v)), "3: nothing");
    click(cx, at, Modifiers::none(), 4);
    wait_until(cx, &view, "4: b", loaded(r"C:\t\a\b"));
}

/// Single-click mode: the 2nd click of a run lands on the file the 1st one launched, which stays
/// under the cursor. It must not launch it again.
#[gpui::test]
fn single_click_mode_launches_a_file_once_per_run(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![file("f.txt", 1), file("g.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    store_single_click(cx);
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = |n: f32| point(bounds.left() + px(60.), bounds.top() + row * (n + 0.5));

    click(cx, at(0.), Modifiers::none(), 1);
    click(cx, at(0.), Modifiers::none(), 2);
    wait_shell_in(cx, "f.txt launched", || !shell.opened.lock().is_empty());
    // A later run on another file proves nothing else was launched in between.
    click(cx, at(1.), Modifiers::none(), 1);
    wait_shell_in(cx, "g.txt launched", || shell.opened.lock().len() >= 2);
    let mut opened = shell.opened.lock().clone();
    opened.sort();
    assert_eq!(
        opened,
        [PathBuf::from(r"C:\t\f.txt"), PathBuf::from(r"C:\t\g.txt")]
    );
    single_click(false);
}
