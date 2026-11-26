//! Moving through a page's controls with the keyboard (#322): Tab and Shift+Tab go from control
//! to control in the order they are drawn, Space and Enter press the one with the focus, and a
//! ring shows which one it is while the keyboard is in use.
//!
//! A control joins with [`KeyStop::key_stop`]: it becomes a tab stop (GPUI keeps the order, see
//! `Window::focus_next`), and GPUI presses it on Space or Enter. The page's Tab action calls
//! [`next`] or [`prev`], which also asks for the control to be brought into view: the control
//! reports where it is when it is next drawn with the focus, and the page's scroll moves just
//! enough to show it ([`reveal`]). Clicking a control doesn't scroll anything.
//!
//! Only Settings uses this; elsewhere Arke's areas keep their own keys (arrows, F6).

use std::cell::{Cell, RefCell};

use gpui::{
    App, Bounds, BoxShadow, FocusHandle, Pixels, ScrollHandle, StatefulInteractiveElement,
    Styled as _, Window, point, px,
};

use crate::theme;

thread_local! {
    /// The focus moved by key: the control that has it scrolls into view when next drawn.
    static REVEAL: Cell<bool> = const { Cell::new(false) };
    /// The scrolling area of the page being drawn, set by the page each frame.
    static VIEW: RefCell<Option<ScrollHandle>> = const { RefCell::new(None) };
}

/// Room kept between a revealed control and the edge of the view.
const MARGIN: f32 = 24.;

pub(crate) trait KeyStop: StatefulInteractiveElement + Sized {
    /// A control Tab stops at: Space and Enter press it, and a ring shows it has the keyboard.
    #[must_use]
    fn key_stop(self) -> Self {
        self.tab_index(0)
            .focus_visible(|s| s.shadow(ring()))
            .on_focused_bounds(reveal)
    }

    /// [`Self::key_stop`] without the ring, for a control that shows the focus itself.
    #[must_use]
    fn key_stop_plain(self) -> Self {
        self.tab_index(0).on_focused_bounds(reveal)
    }
}

impl<E: StatefulInteractiveElement> KeyStop for E {}

/// The keyboard focus ring: a line in the text color just inside the control's edge, as Windows
/// draws it. The text color, not the accent, so it shows on what the accent fills (a switch that
/// is on, the picked tile). Inside, drawn over the control, so it moves nothing; a shadow outside
/// would show through the translucent fill of dropdowns and selected rows and tint them whole.
pub(crate) fn ring() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: theme::text().into(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(2.),
        inset: true,
    }]
}

/// The page whose controls are being drawn scrolls with `scroll`; `None` once it is gone.
pub(crate) fn set_view(scroll: Option<ScrollHandle>) {
    VIEW.with_borrow_mut(|view| *view = scroll);
}

/// Moves the focus to the next control (wrapping around), and brings it into view.
pub(crate) fn next(window: &mut Window, cx: &mut App) {
    REVEAL.set(true);
    window.focus_next(cx);
    window.refresh();
}

/// Moves the focus to the previous control (wrapping around), and brings it into view.
pub(crate) fn prev(window: &mut Window, cx: &mut App) {
    REVEAL.set(true);
    window.focus_prev(cx);
    window.refresh();
}

/// [`next`] kept inside `scope` (a dialog over the page): the controls outside it are passed over.
pub(crate) fn next_within(scope: &FocusHandle, window: &mut Window, cx: &mut App) {
    step_within(scope, true, window, cx);
}

/// [`prev`] kept inside `scope`.
pub(crate) fn prev_within(scope: &FocusHandle, window: &mut Window, cx: &mut App) {
    step_within(scope, false, window, cx);
}

fn step_within(scope: &FocusHandle, forward: bool, window: &mut Window, cx: &mut App) {
    let start = window.focused(cx);
    REVEAL.set(true);
    // Each step moves one stop along the window's order, so going round once is enough.
    for _ in 0..512 {
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        if scope.contains_focused(window, cx) || window.focused(cx) == start {
            break;
        }
    }
    if !scope.contains_focused(window, cx) {
        window.focus(scope, cx);
    }
    window.refresh();
}

/// The control at `bounds` has the focus: scrolls the page just enough to show it, if the focus
/// was moved by key.
fn reveal(bounds: Bounds<Pixels>, window: &mut Window, _: &mut App) {
    if !REVEAL.replace(false) {
        return;
    }
    show(bounds, window);
}

/// Scrolls the page being drawn just enough to show `bounds` (also a row a Settings search went
/// to, #400). Call while the page is drawn (prepaint).
pub(crate) fn show(bounds: Bounds<Pixels>, window: &mut Window) {
    let Some(scroll) = VIEW.with_borrow(Clone::clone) else {
        return;
    };
    let view = scroll.bounds();
    let offset = scroll.offset();
    let shift = shift_to_show(bounds, view, px(MARGIN));
    if shift != px(0.) {
        // Offsets are negative going down; never past the top or the end.
        let max = scroll.max_offset().y;
        let y = (offset.y + shift).min(px(0.)).max(-max);
        scroll.set_offset(point(offset.x, y));
        // This runs while the window draws, when a refresh is ignored: the next frame shows the
        // page at its new place.
        window.on_next_frame(|window, _| window.refresh());
    }
}

/// How far to move the content (positive: down) so `item` is inside `view`, `margin` from its
/// edges where there is room; 0 when it already is.
pub(crate) fn shift_to_show(item: Bounds<Pixels>, view: Bounds<Pixels>, margin: Pixels) -> Pixels {
    let margin = margin.min((view.size.height - item.size.height).max(px(0.)) / 2.);
    if item.top() < view.top() + margin {
        view.top() + margin - item.top()
    } else if item.bottom() > view.bottom() - margin {
        view.bottom() - margin - item.bottom()
    } else {
        px(0.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn at(top: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(top)), size(px(100.), px(height)))
    }

    #[test]
    fn a_control_in_view_doesnt_move() {
        assert_eq!(shift_to_show(at(100., 40.), at(0., 500.), px(24.)), px(0.));
    }

    #[test]
    fn a_control_below_comes_up_to_the_margin() {
        assert_eq!(
            shift_to_show(at(600., 40.), at(0., 500.), px(24.)),
            px(-164.)
        );
    }

    #[test]
    fn a_control_above_comes_down_to_the_margin() {
        assert_eq!(shift_to_show(at(-50., 40.), at(0., 500.), px(24.)), px(74.));
    }

    #[test]
    fn a_control_taller_than_the_view_keeps_its_top_in_view() {
        assert_eq!(
            shift_to_show(at(-100., 800.), at(0., 500.), px(24.)),
            px(100.)
        );
    }
}
