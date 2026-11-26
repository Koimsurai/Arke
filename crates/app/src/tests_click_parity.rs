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
