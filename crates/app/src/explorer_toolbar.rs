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
//!
//! The row's flyouts, Sort and View options, work the same way once Space or Enter opened them
//! (see [`FlyoutKeys`]).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    App, ClickEvent, Context, Div, FocusHandle, KeyDownEvent, Stateful, WeakEntity, Window,
    prelude::*,
};

use super::{Explorer, ViewMode};
use crate::settings::{Density, Settings};

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

// ───────────────────────── The row's flyouts ─────────────────────────

/// The keyboard in the row's flyouts, Sort and View options (#322). Opened from the keyboard or
/// by a screen reader, a flyout takes the focus, on its checked choice: Up and Down (and Tab) move
/// between its choices, the one the keyboard is on is read as focused and shows the ring, Space
/// and Enter pick it, Left and Right move the icon size slider, and Escape closes the flyout and
/// goes back to its button. Like the row, the flyout is one stop: the focus is on the flyout, its
/// choices are read as focused in turn (`aria_active_descendant`). Opened with the mouse, it
/// leaves the focus where it was, as before.
pub(super) struct FlyoutKeys {
    focus: FocusHandle,
    /// The choice the keyboard is on, in [`Explorer::flyout_choices`].
    at: usize,
    /// Where the focus was when the flyout took it; `None`: the row.
    back: Option<FocusHandle>,
}

impl FlyoutKeys {
    /// Keys for flyouts focused with `focus`.
    pub(super) const fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            at: 0,
            back: None,
        }
    }
}

/// A choice in a flyout, in the order the keyboard goes through them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    /// Sort: a column.
    SortBy(arke_core::SortColumn),
    /// Sort: ascending (`true`) or descending.
    SortOrder(bool),
    /// View options: a layout.
    Layout(ViewMode),
    /// View options: the icon size slider.
    IconSize,
    /// View options: a row density.
    Density(Density),
    /// View options: a switch.
    Switch(ViewSwitch),
}

/// The view options flyout's switches (the same settings as in Settings).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ViewSwitch {
    /// Group rows (not in Columns).
    GroupRows,
    /// The Details tree (Details only).
    DetailsTree,
    /// Hidden items.
    ShowHidden,
    /// File name extensions.
    ShowExtensions,
}

impl ViewSwitch {
    /// Whether it is on.
    pub(super) const fn on(self, s: &Settings) -> bool {
        match self {
            Self::GroupRows => s.group_rows,
            Self::DetailsTree => s.details_tree,
            Self::ShowHidden => s.show_hidden,
            Self::ShowExtensions => s.show_extensions,
        }
    }

    /// Turns it the other way.
    pub(super) const fn flip(self, s: &mut Settings) {
        match self {
            Self::GroupRows => s.group_rows = !s.group_rows,
            Self::DetailsTree => s.details_tree = !s.details_tree,
            Self::ShowHidden => s.show_hidden = !s.show_hidden,
            Self::ShowExtensions => s.show_extensions = !s.show_extensions,
        }
    }
}

/// Whether a choice is the one the keyboard is on, and whether it shows the ring (by key).
pub(super) type Mark = (bool, bool);

impl Explorer {
    /// The open flyout's choices, in order; none while neither is open.
    pub(super) fn flyout_choices(&self) -> Vec<Choice> {
        if self.sort_options_open {
            super::sortopts::sort_choices()
        } else if self.view_options_open {
            self.view_choices()
        } else {
            Vec::new()
        }
    }

    /// Whether the open flyout has the keyboard.
    pub(super) fn flyout_held(&self, window: &Window) -> bool {
        self.flyout.focus.is_focused(window)
    }

    /// An open flyout's panel, which hears the keys while it has the keyboard. Only then does it
    /// track its focus: otherwise a click in it would take the focus from the list.
    pub(super) fn flyout_keys(
        &self,
        open: bool,
        panel: Stateful<Div>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if !open || !self.flyout_held(window) {
            return panel;
        }
        panel
            .track_focus(&self.flyout.focus)
            .on_key_down(cx.listener(Self::flyout_key))
            .on_click(cx.listener(Self::flyout_press))
    }

    /// How choice `ix` of the open flyout is drawn: read as focused while the flyout has the
    /// keyboard and it is the one the keyboard is on, with the ring when by key.
    pub(super) fn flyout_mark(&self, ix: Option<usize>, window: &Window) -> Mark {
        let on = ix.is_some_and(|ix| ix == self.flyout.at) && self.flyout_held(window);
        (on, on && window.last_input_was_keyboard())
    }

    /// A flyout was opened or closed from the keyboard or by a screen reader: open, it takes the
    /// focus on choice `at`; closed, the focus goes back to `button` in the row.
    pub(super) fn flyout_by_key(
        &mut self,
        open: bool,
        at: usize,
        button: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !open {
            self.flyout_return(button, window, cx);
            return;
        }
        self.flyout.back = window
            .focused(cx)
            .filter(|f| *f != self.flyout.focus && *f != self.toolbar.focus);
        self.flyout.at = at;
        window.focus(&self.flyout.focus, cx);
        cx.notify();
    }

    /// A flyout closed: if it had the keyboard, the focus goes back where it was, to the row on
    /// `button` unless it came from elsewhere.
    pub(super) fn flyout_return(
        &mut self,
        button: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.flyout_held(window) {
            return;
        }
        match self.flyout.back.take() {
            Some(back) => window.focus(&back, cx),
            None => {
                self.toolbar.at = Some(button);
                window.focus(&self.toolbar.focus, cx);
            }
        }
        cx.notify();
    }

    /// Keys in a flyout that has the keyboard (see [`FlyoutKeys`]).
    pub(super) fn flyout_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "escape" && !m.shift {
            let button = if self.sort_options_open {
                "sort-options"
            } else {
                "view-options"
            };
            self.close_view_options(cx);
            self.close_sort_options(cx);
            self.flyout_return(button, window, cx);
            cx.stop_propagation();
            return;
        }
        let choices = self.flyout_choices();
        let Some(last) = choices.len().checked_sub(1) else {
            return;
        };
        let at = self.flyout.at.min(last);
        let to = match (key, m.shift) {
            ("left" | "right", false) if choices[at] == Choice::IconSize => {
                let size = if key == "right" {
                    self.icon_size.larger()
                } else {
                    self.icon_size.smaller()
                };
                if let Some(size) = size {
                    self.pick_icon_size(size, cx);
                }
                at
            }
            ("up" | "left", false) | ("tab", true) => at.saturating_sub(1),
            ("down" | "right" | "tab", false) => (at + 1).min(last),
            ("home", false) => 0,
            ("end", false) => last,
            _ => return,
        };
        self.flyout.at = to;
        cx.stop_propagation();
        cx.notify();
    }

    /// Space or Enter in a flyout that has the keyboard (a keyboard click, which GPUI gives the
    /// focused element): picks the choice the keyboard is on, as a click on it does.
    pub(super) fn flyout_press(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, ClickEvent::Keyboard(_)) {
            return;
        }
        let choices = self.flyout_choices();
        let Some(&choice) = choices.get(self.flyout.at) else {
            return;
        };
        match choice {
            Choice::SortBy(column) => self.pick_sort_column(column, cx),
            Choice::SortOrder(ascending) => self.pick_sort_direction(ascending, cx),
            Choice::Layout(ViewMode::Details) => {
                self.view_details(&super::ViewDetails, window, cx);
            }
            Choice::Layout(ViewMode::Icons) => self.show_icons(cx),
            Choice::Layout(ViewMode::Columns) => {
                self.view_columns(&super::ViewColumns, window, cx);
            }
            // The arrows move it.
            Choice::IconSize => {}
            Choice::Density(density) => change(cx, |s| s.density = density),
            Choice::Switch(switch) => change(cx, |s| switch.flip(s)),
        }
    }
}

/// Changes the settings (every tab follows).
fn change(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    crate::settings::SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx));
}
