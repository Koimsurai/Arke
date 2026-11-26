//! Presses whose release went to another window.
//!
//! GPUI arms a press on mouse down (a click on release, or a drag once the mouse moves 2 px) and
//! disarms it only on a mouse up; an in-app drag also runs until a mouse up, and any button's up
//! drops it on the target under the cursor. Nothing there looks at whether the button is still
//! down. On Windows the release can go to another window: a program opened by a double-click
//! comes up under the cursor before the second release, a menu or another program takes the
//! mouse, a press on an inactive window is released outside it. The press then stayed armed, so
//! the next move with no button down started a drag of the item, and the next click anywhere
//! dropped it there: files were moved by plain clicks.
//!
//! So each Arke window keeps the buttons it saw pressed. An event that shows one of them is up
//! (a move reporting no such button, or a second press of it) is held back; a release is
//! dispatched for it first, outside the window so no element takes it as a click or a drop, and
//! then the event itself. [`is_cancel`] tells Arke's own window-wide listeners that release apart
//! from a real one. The Windows platform layer sends the same release when the window loses the
//! mouse capture to another program in the middle of a press.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, DispatchPhase, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, NavigationDirection, PlatformInput, Point, Styled, Window, canvas, point, px,
};

/// Whether a press with this click count is a double-click. GPUI keeps counting while clicks
/// stay inside the double-click time and distance (3, 4, 5...), but Windows sends
/// `WM_LBUTTONDBLCLK` only for the 2nd, 4th, 6th... click of such a run; the 3rd, 5th... are
/// plain clicks, as in File Explorer.
pub(crate) const fn is_double_click(count: usize) -> bool {
    count >= 2 && count.is_multiple_of(2)
}

/// Buttons a window saw pressed and not yet released.
#[derive(Clone, Default)]
pub(crate) struct HeldButtons(Rc<Cell<u8>>);

/// One bit per button, in the order Windows checks them for the button a move reports.
const fn bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        MouseButton::Navigate(NavigationDirection::Back) => 8,
        MouseButton::Navigate(NavigationDirection::Forward) => 16,
    }
}

const BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Navigate(NavigationDirection::Back),
    MouseButton::Navigate(NavigationDirection::Forward),
];

/// Of the `held` buttons, the ones a move reporting `pressed` shows are up. A move reports one
/// button, the first of left, right, middle, back and forward that is down: the ones before it
/// are up, the ones after it may still be down.
const fn released(held: u8, pressed: Option<MouseButton>) -> u8 {
    let before = match pressed {
        Some(button) => bit(button) - 1,
        None => u8::MAX,
    };
    held & before
}

/// Where a cancelling release is dispatched: far outside the window, so no element is under it.
const NOWHERE: Point<gpui::Pixels> = point(px(-100_000.), px(-100_000.));

/// Whether this release ends a press whose real release went elsewhere (see the module docs):
/// it must not act like one. Real releases always count at least one click.
pub(crate) const fn is_cancel(event: &MouseUpEvent) -> bool {
    event.click_count == 0
}

/// Ends the press of `button` without a click or a drop. Call it outside event dispatch (from
/// `Window::defer`): GPUI's mouse listeners aren't available while it dispatches an event.
pub(crate) fn cancel_press(button: MouseButton, window: &mut Window, cx: &mut App) {
    let modifiers = window.modifiers();
    let _ = window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button,
            position: NOWHERE,
            modifiers,
            click_count: 0,
        }),
        cx,
    );
}

/// Cancels the presses in `lost` after this event, then dispatches `event` again.
fn replay_after_cancel(lost: u8, event: PlatformInput, window: &Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        for button in BUTTONS.into_iter().filter(|b| lost & bit(*b) != 0) {
            log::info!("{button:?} release went to another window; press cancelled");
            cancel_press(button, window, cx);
        }
        let _ = window.dispatch_event(event, cx);
        window.refresh();
    });
}

impl HeldButtons {
    /// Window-wide listeners that keep track of the presses and cancel the ones whose release
    /// went elsewhere. Paint it before anything else in the window so it hears events first.
    pub(crate) fn watch(&self) -> AnyElement {
        let held = Rc::clone(&self.0);
        canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let on_down = Rc::clone(&held);
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let b = bit(e.button);
                    if on_down.get() & b != 0 {
                        // Pressed again without a release in between: the release went
                        // elsewhere, and this press must not drop or click what the old one armed.
                        cx.stop_propagation();
                        replay_after_cancel(b, PlatformInput::MouseDown(e.clone()), window, cx);
                        return;
                    }
                    // A right-button drag replays its press as a left one while the right button
                    // is the one down (`explorer_dnd.rs`); the right release ends it.
                    if e.button == MouseButton::Left
                        && cx.has_global::<crate::explorer::RightDrag>()
                    {
                        return;
                    }
                    on_down.set(on_down.get() | b);
                });
                let on_up = Rc::clone(&held);
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, _| {
                    if phase == DispatchPhase::Capture {
                        on_up.set(on_up.get() & !bit(e.button));
                    }
                });
                let on_move = held;
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let lost = released(on_move.get(), e.pressed_button);
                    if lost == 0 {
                        return;
                    }
                    on_move.set(on_move.get() & !lost);
                    // Held back: GPUI would start a drag from the armed press on this very move.
                    cx.stop_propagation();
                    replay_after_cancel(lost, PlatformInput::MouseMove(e.clone()), window, cx);
                });
            },
        )
        .absolute()
        .size_0()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_shows_which_held_buttons_are_up() {
        let left = bit(MouseButton::Left);
        let right = bit(MouseButton::Right);
        let middle = bit(MouseButton::Middle);
        assert_eq!(released(left, Some(MouseButton::Left)), 0);
        assert_eq!(released(left, None), left, "no button: all are up");
        assert_eq!(
            released(left, Some(MouseButton::Right)),
            left,
            "left would have been reported first"
        );
        assert_eq!(
            released(left | right, Some(MouseButton::Left)),
            0,
            "right may still be down under left"
        );
        assert_eq!(released(right | middle, Some(MouseButton::Middle)), right);
        assert_eq!(released(0, None), 0);
    }

    #[test]
    fn only_dispatched_cancels_count_as_cancels() {
        let up = |click_count| MouseUpEvent {
            button: MouseButton::Left,
            position: point(px(1.), px(1.)),
            modifiers: gpui::Modifiers::default(),
            click_count,
        };
        assert!(is_cancel(&up(0)));
        assert!(!is_cancel(&up(1)));
        assert!(!is_cancel(&up(2)));
    }
}
