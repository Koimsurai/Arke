//! GPUI tests for the drag threshold: a press becomes a drag only after the pointer leaves the
//! platform's drag rectangle (`SM_CXDRAG` and `SM_CYDRAG` on Windows), per axis.

use gpui::{
    Context, Empty, IntoElement, Modifiers, MouseButton, Point, Render, TestAppContext,
    VisualTestContext, Window, div, point, prelude::*, px, size,
};

#[derive(Clone)]
struct Payload;

struct Source;

impl Render for Source {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("source")
            .size(px(200.))
            .on_drag(Payload, |_, _, _, cx| cx.new(|_| Empty))
    }
}

/// Presses at (50, 50), moves by `by`, and says whether a drag started.
fn drag_started(cx: &mut TestAppContext, threshold: (f32, f32), by: (f32, f32)) -> bool {
    cx.set_drag_threshold(size(px(threshold.0), px(threshold.1)));
    let (_, cx) = cx.add_window_view(|_, _| Source);
    let start: Point<gpui::Pixels> = point(px(50.), px(50.));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(by.0), px(by.1)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    started(cx)
}

fn started(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| cx.has_active_drag())
}

/// A 3 px move stays a click when the system threshold is 4 px.
#[gpui::test]
fn three_pixels_is_no_drag_with_a_four_pixel_threshold(cx: &mut TestAppContext) {
    assert!(!drag_started(cx, (4., 4.), (3., 0.)));
}

/// Past the threshold on either axis it is a drag.
#[gpui::test]
fn five_pixels_is_a_drag_with_a_four_pixel_threshold(cx: &mut TestAppContext) {
    assert!(drag_started(cx, (4., 4.), (5., 0.)));
}

/// X and Y are separate: with a threshold of 10 wide and 2 tall, 3 px down starts a drag but
/// 3 px sideways does not, and neither does a diagonal inside a 4 px square.
#[gpui::test]
fn the_threshold_is_a_rectangle(cx: &mut TestAppContext) {
    assert!(drag_started(cx, (10., 2.), (0., 3.)));
}

#[gpui::test]
fn sideways_inside_the_wide_axis_is_no_drag(cx: &mut TestAppContext) {
    assert!(!drag_started(cx, (10., 2.), (3., 0.)));
}

#[gpui::test]
fn a_diagonal_inside_the_rectangle_is_no_drag(cx: &mut TestAppContext) {
    assert!(!drag_started(cx, (4., 4.), (3., 3.)));
}
