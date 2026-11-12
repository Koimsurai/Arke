//! Horizontal slider: a thin track, filled in the accent color up to a round knob. Press anywhere on
//! it to jump there, drag to adjust; the value follows while dragging. Used by the background
//! opacity setting and the icon size in the view options flyout.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, DispatchPhase, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Window, canvas, point, prelude::*, px, size,
};

use crate::theme;

const TRACK_H: f32 = 4.;
const KNOB: f32 = 18.;

/// Called with the new position, 0 (left) to 1 (right), on press and while dragging.
pub(crate) type OnSlide = Rc<dyn Fn(f32, &mut Window, &mut App)>;

/// Slider at `fraction` (0 to 1). `dragging` lives with the owner so a drag survives redraws.
/// Height is fixed; the caller sets the width.
pub(crate) fn slider(
    fraction: f32,
    dragging: Rc<Cell<bool>>,
    on_slide: OnSlide,
) -> impl IntoElement + Styled {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0., 1.)
    } else {
        0.
    };
    canvas(
        |bounds, _, _| bounds,
        move |_, bounds: Bounds<Pixels>, window, _| {
            let mid = bounds.origin.y + bounds.size.height / 2.;
            let left = bounds.origin.x + px(KNOB / 2.);
            let width = (bounds.size.width - px(KNOB)).max(px(1.));
            let track = Bounds::new(
                point(left, mid - px(TRACK_H / 2.)),
                size(width, px(TRACK_H)),
            );
            window
                .paint_quad(gpui::fill(track, theme::wash_strong()).corner_radii(px(TRACK_H / 2.)));
            let filled = Bounds::new(track.origin, size(width * fraction, px(TRACK_H)));
            window.paint_quad(gpui::fill(filled, theme::signal()).corner_radii(px(TRACK_H / 2.)));
            let x = left + width * fraction;
            let knob = Bounds::new(
                point(x - px(KNOB / 2.), mid - px(KNOB / 2.)),
                size(px(KNOB), px(KNOB)),
            );
            window.paint_quad(
                gpui::fill(knob, theme::overlay())
                    .corner_radii(px(KNOB / 2.))
                    .border_widths(px(1.))
                    .border_color(theme::line_strong()),
            );
            let dot = px(if dragging.get() { 8. } else { 10. });
            let center = Bounds::new(point(x - dot / 2., mid - dot / 2.), size(dot, dot));
            window.paint_quad(gpui::fill(center, theme::signal()).corner_radii(dot / 2.));

            let at = move |x: Pixels| ((x - left) / width).clamp(0., 1.);
            let (d, slide) = (dragging.clone(), on_slide.clone());
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Left
                    || !bounds.contains(&e.position)
                {
                    return;
                }
                cx.stop_propagation();
                d.set(true);
                slide(at(e.position.x), window, cx);
                window.refresh();
            });
            let (d, slide) = (dragging.clone(), on_slide);
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !d.get() {
                    return;
                }
                if e.pressed_button == Some(MouseButton::Left) {
                    cx.stop_propagation();
                    slide(at(e.position.x), window, cx);
                } else {
                    d.set(false);
                }
                window.refresh();
            });
            let d = dragging;
            window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble && d.replace(false) {
                    cx.stop_propagation();
                    window.refresh();
                }
            });
        },
    )
    .h(px(24.))
    .cursor_pointer()
}
