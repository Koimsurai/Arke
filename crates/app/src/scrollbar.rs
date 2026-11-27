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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use gpui::{
    Along as _, Axis, Bounds, DispatchPhase, Div, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels,
    ScrollEvent, ScrollHandle, Stateful, Styled, canvas, div, point, px,
};

use crate::theme;

/// Hover-sensitive width (the thumb is drawn at its right edge)
fn track_w() -> gpui::Pixels {
    crate::theme::scaled_px(12.)
}
/// Thumb width: thin and widened
const THUMB_THIN: f32 = 2.;
const THUMB_WIDE: f32 = 6.;
/// Shortest thumb, along the track.
fn thumb_min_h() -> gpui::Pixels {
    crate::theme::scaled_px(24.)
}
/// Grow and shrink duration (seconds)
const GROW_SECS: f32 = crate::motion::FAST.as_secs_f32();

/// Windows shows scrollbars all the time ("Always show scrollbars", and always on Windows 10):
/// the resting thumb is then drawn as wide as the hovered one.
static ALWAYS_SHOWN: AtomicBool = AtomicBool::new(false);

/// Re-read the Windows scrollbar setting. Called at startup and whenever a window is activated,
/// like the animation setting (see `motion::sync_app`). Tests keep the default.
pub(crate) fn refresh_system_setting() {
    if !cfg!(test) {
        ALWAYS_SHOWN.store(crate::win::always_show_scrollbars(), Ordering::Relaxed);
    }
}

/// Whether the setting makes scrollbars permanent: Windows 10 (build under 22000) has no dynamic
/// scrollbars; Windows 11 hides them unless `DynamicScrollbars` (HKCU\Control
/// Panel\Accessibility) is 0. An unreadable value means the default, dynamic.
pub(crate) const fn always_shown(build: Option<u32>, dynamic_scrollbars: Option<u32>) -> bool {
    matches!(build, Some(b) if b < 22000) || matches!(dynamic_scrollbars, Some(0))
}

/// How wide the thumb rests at the next step: 1 is fully widened, 0 the thin line.
const fn target_grow(active: bool, always_shown: bool) -> f32 {
    if active || always_shown { 1. } else { 0. }
}

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
        let target = target_grow(wide, ALWAYS_SHOWN.load(Ordering::Relaxed));
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
        .max(thumb_min_h())
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

/// Scrollbar overlaid on the right of a list. Place it in a `relative()` container after the list;
/// its edge can be moved (`right`) to sit in a gutter beside the rows.
pub(crate) fn scrollbar(handle: ScrollHandle, drag: ScrollbarDrag) -> gpui::Canvas<Bounds<Pixels>> {
    bar(handle, drag, Axis::Vertical)
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(track_w())
}

/// Sideways scrollbar along the bottom of a list whose rows are wider than it (Details columns),
/// stopping short of the vertical scrollbar's corner. Place it in a `relative()` container after
/// the list.
pub(crate) fn hscrollbar(handle: ScrollHandle, drag: ScrollbarDrag) -> impl IntoElement {
    bar(handle, drag, Axis::Horizontal)
        .absolute()
        .left_0()
        .right(track_w())
        .bottom_0()
        .h(track_w())
}

/// A scroll area inside a scrolling page (the report text in Settings), with its scrollbar.
///
/// GPUI hands a wheel event to every scroll area under the pointer, so the page would move along
/// with the inner area. Here the wheel stops at the inner area while it still moves, and reaches the
/// page only once the inner area is at its end, like a browser. `inner` must track `handle`.
pub(crate) fn nested(inner: Stateful<Div>, handle: &ScrollHandle, drag: ScrollbarDrag) -> Div {
    // The inner offset before this event, recorded in the capture phase, which runs before any
    // scroll area applies the event.
    let before = Rc::new(Cell::new(px(0.)));
    let clamped = |h: &ScrollHandle| h.offset().y.clamp(-h.max_offset().y, px(0.));
    let (h, b) = (handle.clone(), Rc::clone(&before));
    let record = canvas(
        |_, _, _| {},
        move |_, (), window, _| {
            let (h, b) = (h.clone(), Rc::clone(&b));
            window.on_mouse_event(move |_: &ScrollEvent, phase, _, _| {
                if phase == DispatchPhase::Capture {
                    b.set(clamped(&h));
                }
            });
        },
    )
    // No hitbox, so it doesn't take the pointer from the text.
    .absolute()
    .size_full();
    let h = handle.clone();
    // Runs after the area has applied the event: a div adds this listener before its own scroll
    // handler, and GPUI runs bubble listeners last-added first.
    let inner = inner.on_scroll(move |e: &ScrollEvent, _, cx| {
        // Sideways swipes stay with the window (back and forward).
        if e.delta.pixel_delta(px(1.)).y != px(0.) && clamped(&h) != before.get() {
            cx.stop_propagation();
        }
    });
    div()
        .relative()
        .child(inner)
        .child(record)
        .child(scrollbar(handle.clone(), drag))
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
                    gpui::size(
                        px(THUMB_WIDE) + crate::theme::scaled_px(4.),
                        track.size.height,
                    ),
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
                    gpui::size(
                        track.size.width,
                        px(THUMB_WIDE) + crate::theme::scaled_px(4.),
                    ),
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
            let (h, d) = (handle.clone(), Rc::clone(&drag));
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
            let (h, d) = (handle.clone(), Rc::clone(&drag));
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

#[cfg(test)]
mod tests {
    use super::{always_shown, target_grow};

    /// Windows 10 has no dynamic scrollbars; Windows 11 follows "Always show scrollbars"
    /// (`DynamicScrollbars` = 0), and a missing value keeps the dynamic default.
    #[test]
    fn the_windows_setting_decides_whether_scrollbars_rest_wide() {
        assert!(always_shown(Some(19045), None), "Windows 10");
        assert!(
            always_shown(Some(19045), Some(1)),
            "Windows 10, value ignored"
        );
        assert!(
            always_shown(Some(26100), Some(0)),
            "Windows 11, always show"
        );
        assert!(!always_shown(Some(26100), Some(1)), "Windows 11, dynamic");
        assert!(!always_shown(Some(26100), None), "Windows 11, no value");
        assert!(!always_shown(None, None), "unknown version, dynamic");
    }

    /// The thumb grows to full width on hover, and rests there when scrollbars are always shown.
    #[test]
    fn the_resting_thumb_is_wide_only_when_scrollbars_are_always_shown() {
        assert!((target_grow(false, false) - 0.).abs() < f32::EPSILON);
        assert!((target_grow(true, false) - 1.).abs() < f32::EPSILON);
        assert!((target_grow(false, true) - 1.).abs() < f32::EPSILON);
        assert!((target_grow(true, true) - 1.).abs() < f32::EPSILON);
    }
}
