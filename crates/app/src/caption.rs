//! Caption buttons that reach the screen edges when the window is maximized.
//!
//! A maximized window's top and right edges are the screen's, and Windows gives the pixels above
//! the caption buttons, and the top-right corner, to those buttons (Fitts's law: a flick into the
//! corner closes the window). Arke's buttons sit inset in its own title bar, so those pixels used
//! to fall through to the bar. Each button therefore adds an invisible hit area that runs up to
//! the top edge (and for Close, out to the right edge), registered as the same window control, so
//! the system hit test (`WM_NCHITTEST`) answers `HTCLOSE` there.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    Bounds, DispatchPhase, HitboxBehavior, IntoElement, MouseMoveEvent, Pixels, Size, Styled,
    WindowControlArea, canvas, fill, point, px,
};

/// The pointer is in Close's extra area (outside the button), so its icon turns light as on hover.
static CLOSE_EDGE_HOVERED: AtomicBool = AtomicBool::new(false);

/// Whether Close shows its hover look because the pointer is in its extra area.
pub(crate) fn close_edge_hovered() -> bool {
    CLOSE_EDGE_HOVERED.load(Ordering::Relaxed)
}

/// How far from an edge a button may sit and still be stretched to it: the title bar's padding.
/// A button further in (not in the window's top-right corner) keeps its own size.
fn edge_reach() -> gpui::Pixels {
    crate::theme::s_16()
}

/// The extra hit area of a caption button drawn at `button` when the window is maximized: from
/// the top edge down to the button's bottom, widened by `gap` to the right so the strip above the
/// space between two buttons isn't dead, and for the `last` button out to the right edge. None
/// when not maximized or when the button isn't next to the top edge.
pub(crate) fn edge_hit_bounds(
    button: Bounds<Pixels>,
    viewport: Size<Pixels>,
    maximized: bool,
    last: bool,
    gap: Pixels,
) -> Option<Bounds<Pixels>> {
    if !maximized || button.top() > edge_reach() {
        return None;
    }
    let right = if !last {
        button.right() + gap
    } else if viewport.width - button.right() <= edge_reach() {
        viewport.width
    } else {
        button.right()
    };
    Some(Bounds::from_corners(
        point(button.left(), px(0.)),
        point(right, button.bottom()),
    ))
}

/// Child of a caption button that adds its [`edge_hit_bounds`] as `area`. While the pointer is in
/// that extra area (outside the button itself, where the button's own hover look doesn't apply)
/// it paints the hover background, so a flick into the corner lights Close up as it would in
/// Windows. Put it before the icon so the icon draws on top.
pub(crate) fn edge_hit(area: WindowControlArea, last: bool, gap: Pixels) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            edge_hit_bounds(
                bounds,
                window.viewport_size(),
                window.is_maximized(),
                last,
                gap,
            )
            .map(|b| window.insert_hitbox(b, HitboxBehavior::Normal))
        },
        move |button, hitbox, window, _| {
            let Some(hitbox) = hitbox else {
                if last && CLOSE_EDGE_HOVERED.swap(false, Ordering::Relaxed) {
                    window.refresh();
                }
                return;
            };
            let edge = hitbox.clone();
            let in_edge = move |window: &gpui::Window| {
                edge.is_hovered(window) && !button.contains(&window.mouse_position())
            };
            let lit = in_edge(window);
            // Close's icon color is chosen at render: draw once more when this changes it.
            if last && CLOSE_EDGE_HOVERED.swap(lit, Ordering::Relaxed) != lit {
                window.refresh();
            }
            if lit {
                let color = if last {
                    crate::theme::danger()
                } else {
                    crate::theme::wash_strong()
                };
                window.paint_quad(fill(button, color).corner_radii(crate::theme::r_8()));
            }
            window.insert_window_control_hitbox(area, hitbox);
            // Draw again when the pointer enters or leaves the extra area.
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble && in_edge(window) != lit {
                    window.refresh();
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    use gpui::{Point, size};

    use super::*;
    use crate::theme::{h_bar, h_ctl, s_2, s_8};

    const SCREEN: Size<Pixels> = Size {
        width: px(1920.),
        height: px(1080.),
    };

    /// Buttons laid out as in the title bar: centered in it, the last one `s_8()` from the right.
    fn buttons() -> [Bounds<Pixels>; 3] {
        let top = (h_bar() - h_ctl()) / 2.;
        let close_left = SCREEN.width - s_8() - h_ctl();
        [2., 1., 0.].map(|n| {
            Bounds::new(
                point(close_left - (h_ctl() + s_2()) * n, top),
                size(h_ctl(), h_ctl()),
            )
        })
    }

    /// The area (Min, Max, Close) a click at `at` lands on.
    fn target(at: Point<Pixels>, maximized: bool) -> Option<usize> {
        let buttons = buttons();
        buttons.iter().enumerate().position(|(ix, b)| {
            b.contains(&at)
                || edge_hit_bounds(*b, SCREEN, maximized, ix == 2, s_2())
                    .is_some_and(|e| e.contains(&at))
        })
    }

    #[test]
    fn the_maximized_corner_pixel_closes() {
        // The last pixel row and column of the screen.
        let corner = point(SCREEN.width - px(1.), px(0.));
        assert_eq!(target(corner, true), Some(2));
        assert_eq!(target(corner, false), None, "restored: the corner resizes");
    }

    #[test]
    fn the_maximized_top_edge_above_each_button_hits_it() {
        for (ix, b) in buttons().iter().enumerate() {
            let above = point(b.center().x, px(0.));
            assert_eq!(target(above, true), Some(ix));
            assert_eq!(target(above, false), None);
            // Above the gap after a button: still that button, no dead pixel.
            if ix < 2 {
                let gap = point(b.right() + s_2() / 2., px(0.));
                assert_eq!(target(gap, true), Some(ix));
            }
        }
        // Right of Close, level with it.
        let right = point(SCREEN.width - px(1.), buttons()[2].center().y);
        assert_eq!(target(right, true), Some(2));
    }

    #[test]
    fn nothing_is_stretched_below_the_buttons_or_away_from_the_edges() {
        let [min, ..] = buttons();
        let below = point(SCREEN.width - px(1.), min.bottom() + px(1.));
        assert_eq!(target(below, true), None);
        let left_of_min = point(min.left() - px(1.), px(0.));
        assert_eq!(target(left_of_min, true), None);
        // A button lower down (not in the title bar) keeps its own bounds.
        let low = Bounds::new(
            point(crate::theme::scaled_px(100.), crate::theme::scaled_px(200.)),
            size(h_ctl(), h_ctl()),
        );
        assert_eq!(edge_hit_bounds(low, SCREEN, true, true, s_2()), None);
        // Close far from the right edge reaches the top, not across the window.
        let inner = Bounds::new(
            point(crate::theme::scaled_px(100.), crate::theme::scaled_px(8.)),
            size(h_ctl(), h_ctl()),
        );
        let hit = edge_hit_bounds(inner, SCREEN, true, true, s_2()).expect("near the top");
        assert_eq!(hit.right(), inner.right());
        assert_eq!(hit.top(), px(0.));
    }
}
