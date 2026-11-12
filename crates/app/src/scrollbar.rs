//! List scrollbar: a thin line that widens and shows its track when the cursor approaches (or while
//! dragging), then shrinks back, like Files and Windows 11. Drag the thumb or click the track to
//! page.
//!
//! GPUI's `uniform_list` has no scrollbar, so a `canvas` is overlaid on the right (and along the
//! bottom for Details columns wider than the pane). It reads the
//! scroll position at paint time (so wheel scrolling moves it without rebuilding the view) and
//! registers mouse events during paint, the usual approach for custom GPUI elements. Not drawn when
//! content fits one page.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    Along as _, Axis, Bounds, DispatchPhase, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ScrollHandle, Styled, canvas, point, px,
};

use crate::theme;

/// Hover-sensitive width (the thumb is drawn at its right edge)
const TRACK_W: Pixels = px(12.);
/// Thumb width: thin and widened
const THUMB_THIN: f32 = 2.;
const THUMB_WIDE: f32 = 6.;
/// Shortest thumb, along the track.
const THUMB_MIN_H: Pixels = px(24.);
/// Grow and shrink duration (seconds)
const GROW_SECS: f32 = crate::motion::FAST.as_secs_f32();

/// Scrollbar state across frames, kept in the view that owns the list.
#[derive(Default)]
pub(crate) struct ScrollbarState {
    /// While dragging: cursor offset from the thumb top at press
    drag: Cell<Option<Pixels>>,
    /// Current width: 0 thin, 1 fully widened
    grow: Cell<f32>,
    /// Last paint time (to advance the animation)
    painted: Cell<Option<Instant>>,
}

impl ScrollbarState {
    /// Step toward wide or thin; returns the current width (0-1) and whether to keep painting.
    fn step(&self, wide: bool) -> (f32, bool) {
        let target = if wide { 1. } else { 0. };
        let now = Instant::now();
        let elapsed = self
            .painted
            .replace(Some(now))
            .map_or(0., |last| now.duration_since(last).as_secs_f32());
        let current = self.grow.get();
        let next = if !crate::motion::enabled() {
            target
        } else if current < target {
            (current + elapsed / GROW_SECS).min(target)
        } else {
            (current - elapsed / GROW_SECS).max(target)
        };
        self.grow.set(next);
        let moving = (next - target).abs() > f32::EPSILON;
        if !moving {
            // Settled: don't count the idle gap next time it moves.
            self.painted.set(None);
        }
        (next, moving)
    }
}

pub(crate) type ScrollbarDrag = Rc<ScrollbarState>;

/// Thumb start and length along `axis` in the track, and the scroll range; `None` when no
/// scrolling is needed.
fn thumb(
    track: Bounds<Pixels>,
    handle: &ScrollHandle,
    axis: Axis,
) -> Option<(Pixels, Pixels, Pixels)> {
    let max = handle.max_offset().along(axis);
    let viewport = handle.bounds().size.along(axis);
    if max <= px(0.) || viewport <= px(0.) {
        return None;
    }
    let room = track.size.along(axis);
    let length = (room * (viewport / (viewport + max)))
        .max(THUMB_MIN_H)
        .min(room);
    let progress = (-handle.offset().along(axis) / max).clamp(0., 1.);
    let start = track.origin.along(axis) + (room - length) * progress;
    Some((start, length, max))
}

/// Scroll offset that puts the thumb's start at `start` (window coordinates).
fn scroll_to(
    track: Bounds<Pixels>,
    handle: &ScrollHandle,
    axis: Axis,
    (length, max): (Pixels, Pixels),
    start: Pixels,
) {
    let room = track.size.along(axis) - length;
    let progress = if room > px(0.) {
        ((start - track.origin.along(axis)) / room).clamp(0., 1.)
    } else {
        0.
    };
    let offset = handle.offset();
    handle.set_offset(offset.apply_along(axis, |_| -(max * progress)));
}

/// Scrollbar overlaid on the right of a list. Place it in a `relative()` container after the list.
pub(crate) fn scrollbar(handle: ScrollHandle, drag: ScrollbarDrag) -> impl IntoElement {
    bar(handle, drag, Axis::Vertical)
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(TRACK_W)
}

/// Sideways scrollbar along the bottom of a list whose rows are wider than it (Details columns),
/// stopping short of the vertical scrollbar's corner. Place it in a `relative()` container after
/// the list.
pub(crate) fn hscrollbar(handle: ScrollHandle, drag: ScrollbarDrag) -> impl IntoElement {
    bar(handle, drag, Axis::Horizontal)
        .absolute()
        .left_0()
        .right(TRACK_W)
        .bottom_0()
        .h(TRACK_W)
}

/// The thumb as drawn `width` thick, its hit area across the track, and the rail behind it, for a
/// thumb at `start` and `length` long along `axis`.
fn parts(
    axis: Axis,
    track: Bounds<Pixels>,
    (start, length): (Pixels, Pixels),
    width: Pixels,
) -> (Bounds<Pixels>, Bounds<Pixels>, Bounds<Pixels>) {
    match axis {
        Axis::Vertical => {
            let right = track.origin.x + track.size.width - px(2.);
            (
                Bounds::new(point(right - width, start), gpui::size(width, length)),
                Bounds::new(
                    point(track.origin.x, start),
                    gpui::size(track.size.width, length),
                ),
                Bounds::new(
                    point(right - px(THUMB_WIDE) - px(2.), track.origin.y),
                    gpui::size(px(THUMB_WIDE) + px(4.), track.size.height),
                ),
            )
        }
        Axis::Horizontal => {
            let bottom = track.origin.y + track.size.height - px(2.);
            (
                Bounds::new(point(start, bottom - width), gpui::size(length, width)),
                Bounds::new(
                    point(start, track.origin.y),
                    gpui::size(length, track.size.height),
                ),
                Bounds::new(
                    point(track.origin.x, bottom - px(THUMB_WIDE) - px(2.)),
                    gpui::size(track.size.width, px(THUMB_WIDE) + px(4.)),
                ),
            )
        }
    }
}

fn bar(handle: ScrollHandle, drag: ScrollbarDrag, axis: Axis) -> gpui::Canvas<Bounds<Pixels>> {
    canvas(
        |bounds, _, _| bounds,
        move |_, track: Bounds<Pixels>, window, _| {
            let Some((start, length, max)) = thumb(track, &handle, axis) else {
                drag.drag.set(None);
                return;
            };
            let mouse = window.mouse_position();
            let active = drag.drag.get().is_some() || track.contains(&mouse);
            let (grow, moving) = drag.step(active);
            if moving {
                window.request_animation_frame();
            }
            // Thumb hugs the outer edge (right, or bottom when sideways) and grows from a thin
            // line; the hit area is always the full track.
            let width = px(THUMB_THIN + (THUMB_WIDE - THUMB_THIN) * grow);
            let (thumb_bounds, hit_bounds, rail) = parts(axis, track, (start, length), width);
            if grow > 0. {
                let rail_color: Hsla = theme::blend(theme::clear(), theme::wash(), grow).into();
                let thickness = rail.size.along(axis.invert());
                window.paint_quad(gpui::fill(rail, rail_color).corner_radii(thickness / 2.));
            }
            let color: Hsla = theme::blend(theme::line_strong(), theme::text_subtle(), grow).into();
            window.paint_quad(gpui::fill(thumb_bounds, color).corner_radii(width / 2.));

            // Press on the thumb starts a drag; on the track pages toward the cursor.
            let (h, d) = (handle.clone(), drag.clone());
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Left
                    || !track.contains(&e.position)
                {
                    return;
                }
                cx.stop_propagation();
                let at = e.position.along(axis);
                if hit_bounds.contains(&e.position) {
                    d.drag.set(Some(at - start));
                } else {
                    let page = h.bounds().size.along(axis);
                    let offset = h.offset();
                    let dir = if at < start { 1. } else { -1. };
                    let to = (offset.along(axis) + page * dir).clamp(-max, px(0.));
                    h.set_offset(offset.apply_along(axis, |_| to));
                }
                window.refresh();
            });
            let (h, d) = (handle.clone(), drag.clone());
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                match d.drag.get() {
                    Some(grab) if e.pressed_button == Some(MouseButton::Left) => {
                        scroll_to(
                            track,
                            &h,
                            axis,
                            (length, max),
                            e.position.along(axis) - grab,
                        );
                        cx.stop_propagation();
                        window.refresh();
                    }
                    Some(_) => d.drag.set(None),
                    // Darken the thumb while hovering the track.
                    None if track.contains(&e.position) != active => window.refresh(),
                    None => {}
                }
            });
            let d = drag;
            window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble && d.drag.take().is_some() {
                    cx.stop_propagation();
                    window.refresh();
                }
            });
        },
    )
}
