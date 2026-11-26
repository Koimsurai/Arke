//! The pane row's buttons from the keyboard (#322): F6 stops at the row (after the path bar,
//! before the search box), Left and Right move between its buttons, Home and End go to the first
//! and last, Space and Enter press the one it is on, and Escape goes back to the list.
//!
//! The row is one stop: the row itself has the focus, the button it is on is read as focused
//! (`aria_active_descendant`) and shows the keyboard ring. Space and Enter press the row (GPUI
//! does that for the focused element), which runs that button's click.
//!
//! Tab and Shift+Tab walk the row's buttons one by one (each is a stop, as in File Explorer's
//! command bar) and leave the row after the last or before the first (`workspace_sidebar_keys.rs`).
//!
//! The row takes the keyboard only through F6 or Tab: it tracks its focus handle only while that
//! handle is focused, so a click on a button leaves the keyboard where it was (in the list).
//! Buttons are recorded only while the row has the focus, so it costs nothing more otherwise.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    App, ClickEvent, Context, Div, FocusHandle, KeyDownEvent, Stateful, WeakEntity, Window,
    prelude::*,
};

use super::Explorer;

/// What pressing a button does: its click.
type Press = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

thread_local! {
    /// The row being drawn while it has the focus (see [`ToolbarKeys::begin`]).
    static ROW: RefCell<Option<Row>> = const { RefCell::new(None) };
}

/// The buttons of the row being drawn.
struct Row {
    /// The button the keyboard is on; `None`: the first.
    at: Option<&'static str>,
    /// The ring shows on it (the keyboard is in use).
    ring: bool,
    buttons: Vec<(&'static str, Press)>,
}

/// The row's keyboard state, kept between frames.
pub(super) struct ToolbarKeys {
    pub(super) focus: FocusHandle,
    /// The button the keyboard is on, by id.
    at: Option<&'static str>,
    /// The row was entered backwards (Shift+Tab): it starts on its last button.
    start_last: bool,
    /// The buttons drawn last frame, in order, while the row had the focus.
    buttons: Vec<(&'static str, Press)>,
}

impl ToolbarKeys {
    pub(super) fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            at: None,
            start_last: false,
            buttons: Vec::new(),
        }
    }

    /// The row's buttons are about to be built; `held`: the row has the focus, `ring`: by key.
    pub(super) fn begin(&self, held: bool, ring: bool) {
        let at = self.at;
        ROW.with_borrow_mut(|row| {
            *row = held.then(|| Row {
                at,
                ring,
                buttons: Vec::new(),
            });
        });
    }

    /// The row's buttons are built. Returns `true` when another frame is needed: the button the
    /// keyboard was on is gone (the first takes its place), or the row was entered from behind
    /// and the keyboard moved to its last button.
    pub(super) fn end(&mut self) -> bool {
        let Some(row) = ROW.with_borrow_mut(Option::take) else {
            self.buttons.clear();
            return false;
        };
        self.buttons = row.buttons;
        match self.at {
            Some(at) if !self.buttons.iter().any(|(id, _)| *id == at) => {
                self.at = None;
                true
            }
            Some(_) => false,
            None => {
                let back = std::mem::take(&mut self.start_last);
                let pick = if back {
                    self.buttons.last()
                } else {
                    self.buttons.first()
                };
                self.at = pick.map(|(id, _)| *id);
                // Entered from behind: the first button was drawn as the current one.
                back
            }
        }
    }

    /// Tab (`forward`) or Shift+Tab: onto the next or previous button. `false` at the last or
    /// first one, or while no buttons are known: Tab then leaves the row.
    fn walk(&mut self, forward: bool) -> bool {
        let Some(last) = self.buttons.len().checked_sub(1) else {
            return false;
        };
        let at = self
            .at
            .and_then(|at| self.buttons.iter().position(|(id, _)| *id == at))
            .unwrap_or(0);
        let to = match (forward, at) {
            (true, at) if at < last => at + 1,
            (false, at) if at > 0 => at - 1,
            _ => return false,
        };
        self.at = Some(self.buttons[to].0);
        true
    }

    /// Moves along the row for `key` (Left, Right, Home, End); `false` for any other key. The
    /// ends don't wrap round.
    fn step(&mut self, key: &str) -> bool {
        let Some(last) = self.buttons.len().checked_sub(1) else {
            return false;
        };
        let at = self
            .at
            .and_then(|at| self.buttons.iter().position(|(id, _)| *id == at))
            .unwrap_or(0);
        let to = match key {
            "left" => at.saturating_sub(1),
            "right" => (at + 1).min(last),
            "home" => 0,
            "end" => last,
            _ => return false,
        };
        self.at = Some(self.buttons[to].0);
        true
    }

    /// The click of the button the keyboard is on.
    fn current(&self) -> Option<Press> {
        let at = self.at?;
        self.buttons
            .iter()
            .find(|(id, _)| *id == at)
            .map(|(_, press)| Rc::clone(press))
    }
}

/// Whether the row being drawn has the focus.
fn drawing() -> bool {
    ROW.with_borrow(Option::is_some)
}

/// Records `el` as the row's button `id` pressed by `press`; the one the keyboard is on is read
/// as focused and shows the ring.
fn stop(el: Stateful<Div>, id: &'static str, press: Press) -> Stateful<Div> {
    let mark = ROW.with_borrow_mut(|row| {
        let row = row.as_mut()?;
        let current = match row.at {
            Some(at) => at == id,
            None => row.buttons.is_empty(),
        };
        row.buttons.push((id, press));
        Some((current, current && row.ring))
    });
    let Some((current, ring)) = mark else {
        return el;
    };
    let el = if current {
        el.aria_active_descendant()
    } else {
        el
    };
    if ring {
        el.shadow(crate::keynav::ring())
    } else {
        el
    }
}

/// Buttons of the pane row. Outside the row, or while it doesn't have the focus, these are just
/// `on_click` and nothing.
pub(super) trait ToolbarStop: Sized {
    /// `on_click`, and the row's button `id` (unique in the row, the same in every state).
    fn toolbar_button(
        self,
        id: &'static str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self;

    /// The row's button `id` for a button that acts on the mouse press (a menu): `press` is what
    /// Space and Enter do.
    fn toolbar_menu(
        self,
        id: &'static str,
        press: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self;
}

impl ToolbarStop for Stateful<Div> {
    fn toolbar_button(
        self,
        id: &'static str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        if !drawing() {
            return self.on_click(on_click);
        }
        let click: Press = Rc::new(on_click);
        let el = self.on_click({
            let click = Rc::clone(&click);
            move |event, window, cx| click(event, window, cx)
        });
        stop(el, id, click)
    }

    fn toolbar_menu(
        self,
        id: &'static str,
        press: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        if !drawing() {
            return self;
        }
        let click: Press = Rc::new(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            press(window, cx);
        });
        stop(self, id, click)
    }
}

impl Explorer {
    /// Whether the keyboard is in the pane row (F6, `workspace_sidebar_keys.rs`).
    pub(crate) fn toolbar_held(&self, window: &Window) -> bool {
        self.toolbar.focus.is_focused(window)
    }

    /// F6: the keyboard goes to the pane row, on its first button.
    pub(crate) fn focus_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toolbar.at = None;
        self.toolbar.start_last = false;
        window.focus(&self.toolbar.focus, cx);
        cx.notify();
    }

    /// Shift+Tab into the row from the search box: the keyboard goes to its last button.
    pub(crate) fn focus_toolbar_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toolbar.at = None;
        self.toolbar.start_last = true;
        window.focus(&self.toolbar.focus, cx);
        cx.notify();
    }

    /// Tab and Shift+Tab in the row: each button is a stop. `false` when the keyboard is on the
    /// last (first) button, so the key goes on to the next region.
    pub(crate) fn toolbar_walk(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let moved = self.toolbar.walk(forward);
        if moved {
            cx.notify();
        }
        moved
    }

    /// Tests: the button the keyboard is on.
    #[cfg(test)]
    pub(crate) const fn toolbar_at_for_test(&self) -> Option<&'static str> {
        self.toolbar.at
    }

    /// The arrows, Home and End in the row; Escape closes an open flyout, else goes back to the
    /// list.
    pub(super) fn toolbar_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                if !self.close_view_options(cx) && !self.close_sort_options(cx) {
                    window.focus(&self.list_focus, cx);
                }
            }
            key => {
                if !self.toolbar.step(key) {
                    return;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Space or Enter on the row (a keyboard click, which GPUI gives the focused element): the
    /// click of the button it is on. Run outside the pane's update, as a click on the button is.
    pub(super) fn toolbar_press(
        this: &WeakEntity<Self>,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !matches!(event, ClickEvent::Keyboard(_)) {
            return;
        }
        let press = this
            .upgrade()
            .and_then(|this| this.read(cx).toolbar.current());
        if let Some(press) = press {
            press(event, window, cx);
        }
    }
}
