//! Interface scale and the resizable sidebar.

use super::*;
use crate::settings::{Settings, SettingsStore};
use crate::theme;
use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, point, px};

/// The interface scale multiplies every themed size; 100% leaves them alone and
/// out-of-range values are clamped.
#[test]
fn interface_scale_multiplies_themed_sizes() {
    assert!(!theme::set_scale(100), "100% is already the default");
    assert_eq!(theme::h_ctl(), px(32.));
    assert_eq!(theme::sidebar_full_width(), px(248.));
    assert!(theme::set_scale(150));
    assert_eq!(theme::h_ctl(), px(48.));
    assert_eq!(theme::fs_12(), px(18.));
    assert_eq!(theme::icon(), px(24.));
    assert_eq!(theme::sidebar_full_width(), px(372.));
    assert!(theme::set_scale(80));
    assert_eq!(theme::h_row(), px(25.6));
    theme::set_scale(1000);
    assert_eq!(theme::scale_pct(), 150);
    theme::set_scale(1);
    assert_eq!(theme::scale_pct(), 80);
}

/// The scale and the dragged sidebar width are saved, and old files without them
/// load with the defaults.
#[test]
fn interface_scale_and_sidebar_width_persist() {
    let s = Settings {
        ui_scale: 125,
        sidebar_width: 300.,
        ..Settings::default()
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(back.ui_scale, 125);
    assert!((back.sidebar_width - 300.).abs() < 1e-3);
    let old: Settings = serde_json::from_str(r#"{"density":"compact"}"#).unwrap();
    assert_eq!(old.ui_scale, 100);
    assert!(old.sidebar_width.abs() < 1e-3);
}

/// Changing the scale in the settings store resizes the open window right away.
#[gpui::test]
fn changing_the_interface_scale_applies_live(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::default(), r"C:\s");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.run_until_parked();
    let row_before = view.update(cx, |v, _| v.row_height());
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.ui_scale = 150, cx));
    });
    cx.run_until_parked();
    assert_eq!(theme::scale_pct(), 150);
    let row_after = view.update(cx, |v, _| v.row_height());
    assert_eq!(row_after, row_before * 1.5);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.ui_scale = 80, cx));
    });
    cx.run_until_parked();
    assert_eq!(theme::scale_pct(), 80);
}

/// Dragging the sidebar's right edge resizes it within the limits and saves the
/// width; a double-click on the edge brings the default back.
#[gpui::test]
fn sidebar_edge_drag_resizes_and_double_click_resets(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::default(), r"C:\s");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.run_until_parked();
    let saved = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.sidebar_width)
    };
    let drag_to = |cx: &mut VisualTestContext, x: f32| {
        let edge = point(theme::sidebar_full_width(), px(300.));
        cx.simulate_mouse_down(edge, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(px(x), px(300.)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.simulate_mouse_up(point(px(x), px(300.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    };
    assert_eq!(theme::sidebar_full_width(), px(248.));
    drag_to(cx, 300.);
    assert_eq!(theme::sidebar_full_width(), px(300.));
    assert!((saved(cx) - 300.).abs() < 1e-3);
    drag_to(cx, 50.);
    assert_eq!(
        theme::sidebar_full_width(),
        px(180.),
        "stops at the minimum"
    );
    drag_to(cx, 900.);
    assert_eq!(
        theme::sidebar_full_width(),
        px(420.),
        "stops at the maximum"
    );
    assert!((saved(cx) - 420.).abs() < 1e-3);
    let edge = point(theme::sidebar_full_width(), px(300.));
    for click_count in [1, 2] {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: edge,
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: edge,
            modifiers: Modifiers::none(),
            click_count,
        });
    }
    cx.run_until_parked();
    assert_eq!(theme::sidebar_full_width(), px(248.));
    assert!(saved(cx).abs() < 1e-3);
}

/// The collapsed rail has no drag handle and keeps its width.
#[gpui::test]
fn collapsed_rail_has_no_edge_handle(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::default(), r"C:\s");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    assert!(cx.debug_bounds("sidebar-edge").is_some());
    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert!(cx.debug_bounds("sidebar-edge").is_none());
    assert_eq!(theme::sidebar_width(), theme::rail_w());
}

/// The paper's gap to the window's right and bottom edges and its corner radius follow the
/// density (#429): about Edge's by default, thinner when compact, the old roomy look when
/// comfortable.
#[gpui::test]
fn density_sets_the_gaps_around_the_paper(cx: &mut TestAppContext) {
    use crate::settings::Density;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\g", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::default(), r"C:\g");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\g"));
    cx.simulate_resize(gpui::size(px(1200.), px(700.)));
    cx.run_until_parked();
    for (density, gap, radius) in [
        (Density::Standard, 4., 8.),
        (Density::Compact, 2., 6.),
        (Density::Comfortable, 8., 12.),
    ] {
        cx.update(|_, cx| {
            SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.density = density, cx));
        });
        cx.run_until_parked();
        let paper = cx.debug_bounds("paper").expect("paper drawn");
        let window = cx.update(|window, _| window.viewport_size());
        let right = window.width - paper.right();
        let bottom = window.height - paper.bottom();
        assert!(
            (right - px(gap)).abs() < px(0.5),
            "{density:?}: right {right:?}"
        );
        assert!(
            (bottom - px(gap)).abs() < px(0.5),
            "{density:?}: bottom {bottom:?}"
        );
        assert_eq!(theme::paper_gap(), px(gap), "{density:?}");
        assert_eq!(theme::r_paper(), px(radius), "{density:?}");
    }
}
