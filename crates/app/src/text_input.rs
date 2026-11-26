//! Single-line text input with IME composition (Zhuyin, Cangjie, etc.) and candidate window
//! positioning.
//!
//! Adapted from GPUI's `examples/input.rs` (gpui-pre 0.3.11 / zed@f816e1f). Original license:
//! Apache License 2.0, Copyright Zed Industries, Inc. Full text in `THIRD_PARTY_LICENSES.md`
//! (under gpui-pre); `THIRD_PARTY_NOTICES.md` describes the adaptation.
//! Changes: theme colors, Windows shortcuts, Confirm/Cancel events, APIs such as `set_text` /
//! `select_all`.
//!
//! The key to IME is `EntityInputHandler::bounds_for_range`: it returns the on-screen position of
//! the composition range, which the platform layer (IMM32 / TSF on Windows) uses to place the
//! candidate window.

use std::ops::Range;

use gpui::{
    AnimationExt as _, AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle,
    DispatchPhase, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, anchored, deferred, div, fill, point,
    prelude::*, px, relative, size, svg,
};
use rust_i18n::t;
use unicode_segmentation::UnicodeSegmentation;

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme::{self, UiFont as _};

actions!(
    text_input,
    [
        Undo,
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
        SelectHome,
        SelectEnd,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Confirm,
        Cancel,
        OpenMenu,
        MenuUp,
        MenuDown,
    ]
);

const CONTEXT: &str = "TextInput";

pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        // By word, as in a Windows edit box (#219).
        KeyBinding::new("ctrl-left", WordLeft, ctx),
        KeyBinding::new("ctrl-right", WordRight, ctx),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, ctx),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, ctx),
        KeyBinding::new("ctrl-delete", DeleteWordRight, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        // The same plain-text paste, for a mouse button mapped to it (Xshell style, #428).
        KeyBinding::new("ctrl-shift-v", Paste, ctx),
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-x", Cut, ctx),
        KeyBinding::new("ctrl-z", Undo, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("enter", Confirm, ctx),
        KeyBinding::new("escape", Cancel, ctx),
        // The edit menu from the keyboard, as in a Windows edit box.
        KeyBinding::new("shift-f10", OpenMenu, ctx),
        KeyBinding::new("menu", OpenMenu, ctx),
        // Move in the open edit menu; without one they pass on (a list or popup below the box).
        KeyBinding::new("up", MenuUp, ctx),
        KeyBinding::new("down", MenuDown, ctx),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputEvent {
    Confirm,
    Cancel,
    /// Content changed (typing, delete, paste, IME composition). Not fired by `set_text`.
    Changed,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about one field: masked, being selected, a tab stop"
)]
pub(crate) struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the text is scrolled left so the caret stays in view when it is wider than the field.
    scroll_x: Pixels,
    is_selecting: bool,
    /// Password field: drawn as dots, copy and cut disabled
    masked: bool,
    /// Tab stops here and the page scrolls it into view ([`Self::key_stop`]).
    key_stop: bool,
    /// Earlier states (content and selection), newest last, for Undo.
    undo: Vec<(SharedString, Range<usize>)>,
    /// What the newest Undo step is made of and where the caret sits after it, so the next
    /// character typed (or erased) there joins the same step.
    step: Option<(Step, usize)>,
    /// The edit under way is Backspace or Delete at the caret (set just before it).
    erasing: Option<Step>,
    /// Edit menu, while open.
    menu: Option<EditMenu>,
    /// Closes the edit menu when the window goes to the background.
    activation: Option<gpui::Subscription>,
}

/// Steps kept for Undo.
const UNDO_LIMIT: usize = 100;

/// A run of edits that undo together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Typing,
    Backspace,
    Delete,
}

/// The edit menu opened by right-click, Shift+F10 or the menu key.
struct EditMenu {
    position: Point<Pixels>,
    /// The clipboard held text when the menu opened.
    can_paste: bool,
    /// Row chosen with the arrow keys (an index into [`MENU_ROWS`]); Enter runs it.
    highlighted: Option<usize>,
    /// The window it is drawn in (`popup.rs`).
    popup: crate::popup::Single,
}

/// The edit menu's rows, in Windows order.
const MENU_ROWS: [EditCommand; 6] = [
    EditCommand::Undo,
    EditCommand::Cut,
    EditCommand::Copy,
    EditCommand::Paste,
    EditCommand::Delete,
    EditCommand::SelectAll,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditCommand {
    Undo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
}

/// Glyph drawn for each character in a password field
const MASK: &str = "•";

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    /// A field Tab stops at, on a page that moves through its controls with Tab (Settings, #322;
    /// see `keynav.rs`).
    #[must_use]
    pub(crate) fn key_stop(mut self) -> Self {
        // A tracked handle is a tab stop by its own setting, not the element's.
        self.focus_handle = self.focus_handle.tab_index(0).tab_stop(true);
        self.key_stop = true;
        self
    }

    pub(crate) fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: Pixels::ZERO,
            is_selecting: false,
            masked: false,
            key_stop: false,
            undo: Vec::new(),
            step: None,
            erasing: None,
            menu: None,
            activation: None,
        }
    }

    /// Make this a password field: content drawn as dots, copy and cut do nothing (passwords stay
    /// off the clipboard).
    pub(crate) fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    /// Content byte offset -> offset in the drawn line (password dots differ in length from the
    /// characters).
    fn display_offset(&self, offset: usize) -> usize {
        if !self.masked {
            return offset;
        }
        let offset = offset.min(self.content.len());
        self.content
            .char_indices()
            .take_while(|(at, _)| *at < offset)
            .count()
            * MASK.len()
    }

    /// Offset in the drawn line -> content byte offset.
    fn content_offset(&self, display: usize) -> usize {
        if !self.masked {
            return display;
        }
        self.content
            .char_indices()
            .nth(display / MASK.len())
            .map_or(self.content.len(), |(at, _)| at)
    }

    pub(crate) fn text(&self) -> &str {
        &self.content
    }

    /// Whether the right-click edit menu is open.
    pub(crate) const fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Text without the uncommitted IME composition. Live filtering uses this so phonetic symbols
    /// aren't matched.
    pub(crate) fn committed_text(&self) -> String {
        match &self.marked_range {
            Some(r) => format!("{}{}", &self.content[..r.start], &self.content[r.end..]),
            None => self.content.to_string(),
        }
    }

    pub(crate) fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        // Setting the text from outside starts over: Undo only covers what was typed since.
        self.undo.clear();
        self.step = None;
        self.menu = None;
        cx.notify();
    }

    /// Replaces the whole text on the user's behalf (Tab completing a typed path, a search filter
    /// written into the box): one Undo step takes it back to what was there. Like `set_text`, it
    /// fires no `Changed` and puts the caret at the end.
    pub(crate) fn replace_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        if text == self.content && self.marked_range.is_none() {
            self.selected_range = text.len()..text.len();
            self.selection_reversed = false;
            cx.notify();
            return;
        }
        self.push_undo();
        self.content = text;
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.step = None;
        self.menu = None;
        cx.notify();
    }

    /// Tests: the selected text.
    #[cfg(test)]
    pub(crate) fn selected_text_for_test(&self) -> String {
        self.content[self.selected_range.clone()].to_string()
    }

    /// Tests: where the text was last drawn.
    #[cfg(test)]
    pub(crate) const fn bounds_for_test(&self) -> Option<Bounds<Pixels>> {
        self.last_bounds
    }

    /// Tests: the text shown while the input is empty.
    #[cfg(test)]
    pub(crate) fn placeholder_for_test(&self) -> String {
        self.placeholder.to_string()
    }

    pub(crate) fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Select a byte range (clamped to character boundaries).
    pub(crate) fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let end = range.end.min(self.content.len());
        let start = range.start.min(end);
        if self.content.is_char_boundary(start) && self.content.is_char_boundary(end) {
            self.selected_range = start..end;
            self.selection_reversed = false;
            cx.notify();
        }
    }

    pub(crate) fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), window, cx);
        } else {
            self.move_to(self.selected_range.start, window, cx);
        }
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), window, cx);
        } else {
            self.move_to(self.selected_range.end, window, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), window, cx);
    }

    fn word_left(&mut self, _: &WordLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word(self.cursor_offset()), window, cx);
    }

    fn word_right(&mut self, _: &WordRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word(self.cursor_offset()), window, cx);
    }

    fn select_word_left(
        &mut self,
        _: &SelectWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.previous_word(self.cursor_offset()), window, cx);
    }

    fn select_word_right(
        &mut self,
        _: &SelectWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.next_word(self.cursor_offset()), window, cx);
    }

    fn select_home(&mut self, _: &SelectHome, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, window, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), window, cx);
    }

    /// Ctrl+Backspace: erases back to the start of the word (a selection is erased instead).
    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let to = self.previous_word(self.cursor_offset());
            if to == self.cursor_offset() {
                return;
            }
            self.select_to(to, window, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Ctrl+Delete: erases up to the start of the next word.
    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let to = self.next_word(self.cursor_offset());
            if to == self.cursor_offset() {
                return;
            }
            self.select_to(to, window, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, window, cx);
        self.select_to(self.content.len(), window, cx);
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, window, cx);
            self.erasing = Some(Step::Backspace);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, window, cx);
            self.erasing = Some(Step::Delete);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Enter: runs the row chosen in the edit menu (or only closes the menu), otherwise confirms.
    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.menu.as_ref() {
            match menu.highlighted.map(|ix| MENU_ROWS[ix]) {
                Some(command) if self.can(command) => self.run_edit(command, window, cx),
                _ => self.close_menu(cx),
            }
            return;
        }
        cx.emit(InputEvent::Confirm);
    }

    /// Shift+F10 or the menu key: the edit menu under the caret, its first usable row chosen.
    fn open_menu_from_keyboard(&mut self, _: &OpenMenu, _: &mut Window, cx: &mut Context<Self>) {
        let position = match (self.last_bounds, self.last_layout.as_ref()) {
            (Some(bounds), Some(line)) => point(
                bounds.left() + line.x_for_index(self.display_offset(self.cursor_offset()))
                    - self.scroll_x,
                bounds.bottom(),
            ),
            (Some(bounds), None) => bounds.bottom_left(),
            _ => point(px(0.), px(0.)),
        };
        self.show_menu(position, cx);
        self.move_in_menu(true, cx);
    }

    fn menu_up(&mut self, _: &MenuUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_none() {
            cx.propagate();
            return;
        }
        self.move_in_menu(false, cx);
    }

    fn menu_down(&mut self, _: &MenuDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_none() {
            cx.propagate();
            return;
        }
        self.move_in_menu(true, cx);
    }

    /// Chooses the next (or previous) usable row of the open menu, wrapping around.
    fn move_in_menu(&mut self, forward: bool, cx: &mut Context<Self>) {
        let usable: Vec<usize> = (0..MENU_ROWS.len())
            .filter(|&ix| self.can(MENU_ROWS[ix]))
            .collect();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let next = match (menu.highlighted, forward) {
            (None, true) => usable.first().copied(),
            (None, false) => usable.last().copied(),
            (Some(at), true) => usable
                .iter()
                .copied()
                .find(|&ix| ix > at)
                .or_else(|| usable.first().copied()),
            (Some(at), false) => usable
                .iter()
                .rev()
                .copied()
                .find(|&ix| ix < at)
                .or_else(|| usable.last().copied()),
        };
        menu.highlighted = next;
        cx.notify();
    }

    /// Esc closes the edit menu first; only without one does it cancel the edit.
    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
            return;
        }
        cx.emit(InputEvent::Cancel);
    }

    /// Records the current state before an edit, unless the edit continues the last step (typing
    /// at the caret, Backspace or Delete again at the caret, an IME composition in progress).
    /// `erasing` is the edit when it is Backspace or Delete at the caret.
    fn remember(&mut self, range: &Range<usize>, new_text: &str, erasing: Option<Step>) {
        let continues = self.marked_range.is_some()
            || match (erasing, self.step) {
                (None, Some((Step::Typing, at))) => {
                    range.is_empty() && !new_text.is_empty() && at == range.start
                }
                (Some(Step::Backspace), Some((Step::Backspace, at))) => at == range.end,
                (Some(Step::Delete), Some((Step::Delete, at))) => at == range.start,
                _ => false,
            };
        if !continues {
            self.push_undo();
        }
    }

    fn push_undo(&mut self) {
        if self.undo.len() >= UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo
            .push((self.content.clone(), self.selected_range.clone()));
    }

    /// An edit left the text as the newest Undo step has it (an IME composition given up, a paste
    /// of the same text): that step would undo nothing, so it goes.
    fn drop_empty_step(&mut self) {
        if self.marked_range.is_none()
            && self
                .undo
                .last()
                .is_some_and(|(content, _)| *content == self.content)
        {
            self.undo.pop();
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        // Mid-composition the IME owns the text being composed; undoing under it would leave the
        // composition pointing at text that is gone.
        if self.marked_range.is_some() {
            return;
        }
        let Some((content, selected)) = self.undo.pop() else {
            return;
        };
        self.content = content;
        self.selected_range = selected;
        self.selection_reversed = false;
        self.marked_range = None;
        self.step = None;
        self.menu = None;
        window.notify_text_input_changed(&self.focus_handle);
        window.notify_text_selection_changed(&self.focus_handle);
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    /// Whether `position` is over the selected text.
    fn over_selection(&self, position: Point<Pixels>) -> bool {
        if self.selected_range.is_empty() {
            return false;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return false;
        };
        if position.y < bounds.top() || position.y > bounds.bottom() {
            return false;
        }
        let x = position.x - bounds.left() + self.scroll_x;
        let from = line.x_for_index(self.display_offset(self.selected_range.start));
        let to = line.x_for_index(self.display_offset(self.selected_range.end));
        x >= from && x <= to
    }

    /// Right-click: keeps the selection when it lands on it, otherwise puts the caret there (like
    /// a Windows edit box), then opens the edit menu.
    fn on_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The row or bar around the box has its own right-click menu.
        cx.stop_propagation();
        window.focus(&self.focus_handle, cx);
        self.is_selecting = false;
        if !self.over_selection(event.position) {
            self.move_to(self.index_for_mouse_position(event.position), window, cx);
        }
        self.show_menu(event.position, cx);
    }

    /// Opens the edit menu at `position` (window coordinates), focusing the box. For a box drawn
    /// as plain text until it is used (the search box holding a query), which places the caret
    /// itself first.
    pub(crate) fn open_menu_at(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.show_menu(position, cx);
    }

    fn show_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let can_paste = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .is_some_and(|text| !text.is_empty());
        self.menu = Some(EditMenu {
            position,
            can_paste,
            highlighted: None,
            popup: crate::popup::Single::new(),
        });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Whether `command` applies now (the menu's rows are greyed out otherwise).
    fn can(&self, command: EditCommand) -> bool {
        let selected = !self.selected_range.is_empty();
        match command {
            EditCommand::Undo => !self.undo.is_empty(),
            // Passwords stay off the clipboard.
            EditCommand::Cut | EditCommand::Copy => selected && !self.masked,
            EditCommand::Paste => self.menu.as_ref().is_some_and(|menu| menu.can_paste),
            EditCommand::Delete => selected,
            EditCommand::SelectAll => !self.content.is_empty(),
        }
    }

    fn run_edit(&mut self, command: EditCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        match command {
            EditCommand::Undo => self.undo(&Undo, window, cx),
            EditCommand::Cut => self.cut(&Cut, window, cx),
            EditCommand::Copy => self.copy(&Copy, window, cx),
            EditCommand::Paste => self.paste(&Paste, window, cx),
            EditCommand::Delete => {
                if !self.selected_range.is_empty() {
                    self.replace_text_in_range(None, "", window, cx);
                }
            }
            EditCommand::SelectAll => self.select_all(&SelectAll, window, cx),
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let at = self.index_for_mouse_position(event.position);
        // Double-click selects the word, triple-click everything, as in a Windows edit box (#284).
        if event.click_count >= 2 && !event.modifiers.shift {
            self.is_selecting = false;
            let range = if event.click_count == 2 && !self.masked {
                word_at(&self.content, at)
            } else {
                0..self.content.len()
            };
            self.selected_range = range;
            self.selection_reversed = false;
            self.step = None;
            window.notify_text_selection_changed(&self.focus_handle);
            cx.notify();
            return;
        }
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(at, window, cx);
        } else {
            self.move_to(at, window, cx);
        }
    }

    const fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // A paste is a step of its own: it doesn't join the word typed before it, and what is
            // typed after it doesn't join the paste.
            self.step = None;
            self.replace_text_in_range(None, &text.replace(['\r', '\n'], " "), window, cx);
            self.step = None;
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn move_to(&mut self, offset: usize, window: &Window, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.step = None;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    const fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let offset = self
            .content_offset(line.closest_index_for_x(position.x - bounds.left() + self.scroll_x));
        // `line` is from the last frame. If the text changed since (Tab completing the path during
        // a drag), the offset may be past the end or inside a character of the new text.
        self.content.floor_char_boundary(offset)
    }

    fn select_to(&mut self, offset: usize, window: &Window, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    /// Start of the word before `offset`, skipping spaces and separators first (`\`, `.`, `-` and
    /// the like end a word, so in a path it stops at each folder name). A password field has no
    /// words: its start.
    fn previous_word(&self, offset: usize) -> usize {
        if self.masked {
            return 0;
        }
        previous_word(&self.content, offset)
    }

    /// Start of the next word after `offset`, as Ctrl+Right in a Windows edit box.
    fn next_word(&self, offset: usize) -> usize {
        if self.masked {
            return self.content.len();
        }
        next_word(&self.content, offset)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_range = self.range_from_utf16(&range_utf16);
        self.selection_reversed = false;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.content.encode_utf16().count())
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());

        let erasing = self.erasing.take();
        self.remember(&range, new_text, erasing);
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        // Typing over a selection starts a step too, and what follows at the caret joins it; so
        // does a run of Backspace or Delete.
        self.step = if new_text.is_empty() {
            erasing.map(|kind| (kind, range.start))
        } else {
            Some((Step::Typing, self.selected_range.end))
        };
        self.menu = None;
        self.marked_range.take();
        self.drop_empty_step();
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());

        self.remember(&range, new_text, None);
        self.erasing = None;
        self.step = None;
        self.menu = None;
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if new_text.is_empty() {
            self.marked_range = None;
            // The composition was given up.
            self.drop_empty_step();
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        let insertion_start_utf16 = self.offset_to_utf16(range.start);
        let inserted_length_utf16 = new_text.encode_utf16().count();
        let caret = range.start + new_text.len();
        self.selected_range = new_selected_range_utf16.map_or(caret..caret, |selected| {
            self.range_from_utf16(
                &(insertion_start_utf16 + selected.start.min(inserted_length_utf16)
                    ..insertion_start_utf16 + selected.end.min(inserted_length_utf16)),
            )
        });
        self.selection_reversed = false;
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        if last_layout.text != self.content {
            return None;
        }
        let range = self.range_from_utf16(&range_utf16);
        let left = bounds.left() - self.scroll_x;
        Some(Bounds::from_corners(
            point(left + last_layout.x_for_index(range.start), bounds.top()),
            point(left + last_layout.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        self.bounds_for_range(range_utf16, bounds, window, cx)
            .into_iter()
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        if last_layout.text != self.content {
            return None;
        }
        let utf8_index = last_layout.closest_index_for_x(line_point.x + self.scroll_x);
        Some(self.offset_to_utf16(utf8_index))
    }
}

fn caret_width() -> gpui::Pixels {
    crate::theme::scaled_px(1.5)
}

/// Scroll offset after a change: unchanged while the caret is visible, otherwise just enough to
/// bring it back in, and never past the end of the text.
fn follow_caret(scroll: Pixels, caret: Pixels, text_width: Pixels, field_width: Pixels) -> Pixels {
    let view = (field_width - caret_width()).max(Pixels::ZERO);
    let mut scroll = scroll;
    if caret - scroll > view {
        scroll = caret - view;
    }
    if caret < scroll {
        scroll = caret;
    }
    scroll.clamp(Pixels::ZERO, (text_width - view).max(Pixels::ZERO))
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll_x: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        // Password fields draw dots: map cursor, selection and composition to offsets in the drawn
        // line.
        let selected_range = input.display_offset(input.selected_range.start)
            ..input.display_offset(input.selected_range.end);
        let cursor = input.display_offset(input.cursor_offset());
        let marked_range = input
            .marked_range
            .as_ref()
            .map(|r| input.display_offset(r.start)..input.display_offset(r.end));
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), theme::placeholder())
        } else if input.masked {
            (
                SharedString::from(MASK.repeat(content.chars().count())),
                style.color,
            )
        } else {
            (content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let scroll_x = if input.content.is_empty() {
            Pixels::ZERO
        } else {
            follow_caret(input.scroll_x, cursor_pos, line.width(), bounds.size.width)
        };
        let left = bounds.left() - scroll_x;
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(left + cursor_pos, bounds.top()),
                        size(caret_width(), bounds.bottom() - bounds.top()),
                    ),
                    theme::signal(),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(left + line.x_for_index(selected_range.start), bounds.top()),
                        point(left + line.x_for_index(selected_range.end), bounds.bottom()),
                    ),
                    theme::text_selection(),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
            scroll_x,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        // While a drag-selection is in progress the field keeps following the pointer outside its
        // own bounds (#410). Registered only then, so an idle field costs nothing.
        if self.input.read(cx).is_selecting {
            let input = self.input.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture
                    && event.pressed_button == Some(MouseButton::Left)
                {
                    input.update(cx, |input, cx| input.on_mouse_move(event, window, cx));
                }
            });
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        let scroll_x = prepaint.scroll_x;
        // Text scrolled out of the field is clipped; the caret may sit on the right edge.
        let mask = Bounds::new(
            bounds.origin,
            size(bounds.size.width + caret_width(), bounds.size.height),
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds: mask }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection);
            }
            if let Err(err) = line.paint(
                point(bounds.left() - scroll_x, bounds.top()),
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            ) {
                log::warn!("text paint failed: {err}");
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_x = scroll_x;
        });
    }
}

impl TextInput {
    /// Screen reader view of the field: one text run holding the text (dots for a password) and
    /// the caret or selection in it, so the reader can echo typing and move by character.
    fn a11y_text(&self) -> impl FnOnce(&mut gpui::A11ySubtreeBuilder) + 'static {
        let at = |offset: usize| {
            self.content
                .get(..offset.min(self.content.len()))
                .map_or(0, |s| s.chars().count())
        };
        let selection = (at(self.selected_range.start), at(self.selected_range.end));
        let text = if self.masked {
            MASK.repeat(self.content.chars().count())
        } else {
            self.content.to_string()
        };
        let reversed = self.selection_reversed;
        let placeholder = self.placeholder.clone();
        move |builder| {
            // A field under a form label (see `a11y::label_scope`) is named by it, others by
            // their placeholder.
            let label = crate::a11y::scope_label().unwrap_or(placeholder);
            builder.parent_node().set_label(label.to_string());
            let mut run = gpui::accesskit::Node::new(gpui::Role::TextRun);
            run.set_character_lengths(
                text.chars()
                    .map(|c| u8::try_from(c.len_utf8()).unwrap_or(1))
                    .collect::<Vec<_>>(),
            );
            run.set_value(text);
            let id = builder.synthetic_node_id(0);
            builder.push_child(id, run);
            let pos = |character_index| gpui::accesskit::TextPosition {
                node: id,
                character_index,
            };
            let (start, end) = selection;
            let (anchor, focus) = if reversed { (end, start) } else { (start, end) };
            builder
                .parent_node()
                .set_text_selection(gpui::accesskit::TextSelection {
                    anchor: pos(anchor),
                    focus: pos(focus),
                });
        }
    }
}

#[cfg(test)]
impl TextInput {
    /// Each row of the open edit menu and whether it can be used; `None` while closed.
    pub(crate) fn menu_rows(&self) -> Option<Vec<(EditCommand, bool)>> {
        self.menu.as_ref()?;
        Some(
            MENU_ROWS
                .into_iter()
                .map(|command| (command, self.can(command)))
                .collect(),
        )
    }

    /// The row of the open edit menu chosen with the keyboard.
    pub(crate) fn menu_choice(&self) -> Option<EditCommand> {
        self.menu.as_ref()?.highlighted.map(|ix| MENU_ROWS[ix])
    }

    /// How many steps Undo can take back.
    pub(crate) const fn undo_steps(&self) -> usize {
        self.undo.len()
    }

    pub(crate) fn selection(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    /// Window position of the left edge of the character at byte `offset`, as drawn last, in the
    /// middle of the line.
    pub(crate) fn point_at(&self, offset: usize) -> Option<Point<Pixels>> {
        let (bounds, line) = (self.last_bounds?, self.last_layout.as_ref()?);
        Some(point(
            bounds.left() + line.x_for_index(self.display_offset(offset)) - self.scroll_x,
            bounds.center().y,
        ))
    }
}

/// Draw order of the edit menu's scrim (the menu is one above): over every other floating panel
/// (a search box's own panel, a dialog the box sits in, context menus and their submenus), which
/// would otherwise cover it where they overlap.
const MENU_PRIORITY: usize = 10;

impl TextInput {
    /// The edit menu over the page: Windows order (undo, cut, copy, paste, delete, select all),
    /// rows that don't apply greyed out. It never takes focus, so the box (and a rename or address
    /// edit that ends when focus leaves) stays as it is; the row chosen with the keyboard is
    /// reported to screen readers as focused instead. The menu is drawn in a window of its own
    /// (`popup.rs`); what stays here is the scrim that closes it.
    fn render_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_mut()?;
        let at = crate::popup::to_screen(window, menu.position);
        menu.popup.ensure(
            || crate::popup::Anchor::At(at.0, at.1),
            |this: &mut Self| this.menu.as_mut().map(|m| &mut m.popup),
            |this, _, _, cx| {
                Some(
                    this.menu_panel(cx)
                        .with_animation(
                            "text-input-menu-appear",
                            crate::motion::menu_enter(),
                            crate::motion::fade_in,
                        )
                        .into_any_element(),
                )
            },
            |this, _, cx| this.close_menu(cx),
            window,
            cx,
        );
        let viewport = window.viewport_size();
        // A click anywhere else only closes the menu.
        let scrim = deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("text-input-menu-scrim")
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.close_menu(cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.close_menu(cx);
                        }),
                    ),
            ),
        )
        .with_priority(MENU_PRIORITY);
        Some(scrim.into_any_element())
    }

    /// The rows of the edit menu.
    #[allow(
        clippy::too_many_lines,
        reason = "one table of rows and the one way each is drawn; split up they'd scatter"
    )]
    fn menu_panel(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let rows = [
            (
                EditCommand::Undo,
                t!("menu.undo"),
                "Ctrl+Z",
                Some(Icon::Undo),
            ),
            (EditCommand::Cut, t!("menu.cut"), "Ctrl+X", Some(Icon::Cut)),
            (
                EditCommand::Copy,
                t!("menu.copy"),
                "Ctrl+C",
                Some(Icon::Copy),
            ),
            (
                EditCommand::Paste,
                t!("menu.paste"),
                "Ctrl+V",
                Some(Icon::Paste),
            ),
            (
                EditCommand::Delete,
                t!("menu.delete"),
                "Del",
                Some(Icon::Trash),
            ),
            (
                EditCommand::SelectAll,
                t!("menu.select_all"),
                "Ctrl+A",
                None,
            ),
        ];
        let mut panel = div()
            .id("text-input-menu")
            .role(gpui::Role::Menu)
            .occlude()
            .min_w(crate::theme::scaled_px(220.))
            .py(theme::s_4())
            .rounded(theme::r_12())
            .bg(theme::overlay())
            .shadow(theme::elev_menu())
            .ui_font()
            .text_size(theme::fs_14())
            .text_color(theme::text());
        let chosen = self.menu.as_ref().and_then(|menu| menu.highlighted);
        let owner = self.menu.as_ref().and_then(|menu| menu.popup.owner());
        for (ix, (command, label, shortcut, glyph)) in rows.into_iter().enumerate() {
            let enabled = self.can(command);
            let highlighted = chosen == Some(ix);
            let label: SharedString = label.into_owned().into();
            if ix == 1 || ix == 5 {
                panel = panel.child(
                    div()
                        .h(px(1.))
                        .my(theme::s_4())
                        .mx(theme::s_8())
                        .bg(theme::line()),
                );
            }
            let color = if enabled {
                theme::text_muted()
            } else {
                theme::text_disabled()
            };
            panel = panel.child(
                div()
                    .id(("text-input-menu-item", ix))
                    .debug_selector(move || format!("text-input-menu-{command:?}"))
                    .a11y_menu_item(label.clone(), enabled, false, highlighted)
                    .aria_keyshortcuts(shortcut)
                    .h(theme::h_ctl_sm() + theme::s_4())
                    .mx(theme::s_4())
                    .px(theme::s_8())
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .rounded(theme::r_8())
                    .when(!enabled, |d| d.text_color(theme::text_disabled()))
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .when(highlighted, |d| d.bg(theme::wash_strong()))
                            .hover(|s| s.bg(theme::wash()))
                            .on_click(crate::popup::listener_in(
                                owner,
                                cx,
                                move |this, _: &ClickEvent, window, cx| {
                                    this.run_edit(command, window, cx);
                                },
                            ))
                    })
                    .child(
                        div()
                            .size(theme::icon())
                            .flex_none()
                            .children(glyph.map(|glyph| {
                                svg()
                                    .path(glyph.path())
                                    .size(theme::icon())
                                    .text_color(color)
                            })),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .pl(theme::s_16())
                            .text_size(theme::fs_12())
                            .text_color(theme::text_subtle())
                            .child(shortcut),
                    ),
            );
        }
        panel
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus moved elsewhere (Tab, another window): the menu goes with it.
        if self.menu.is_some() && !self.focus_handle.is_focused(window) {
            self.menu = None;
        }
        // So does going to another window (Alt+Tab, a click on another app): Windows' own menus
        // close then too.
        if self.activation.is_none() {
            self.activation = Some(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.close_menu(cx);
                }
            }));
        }
        let menu = self.render_menu(window, cx);
        let a11y = crate::a11y::active().then(|| self.a11y_text());
        div()
            .id("text-input")
            .role(if self.masked {
                gpui::Role::PasswordInput
            } else {
                gpui::Role::TextInput
            })
            .when_some(
                a11y,
                gpui::StatefulInteractiveElement::a11y_synthetic_children,
            )
            .flex()
            .flex_1()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .when(self.key_stop, crate::keynav::KeyStop::key_stop_plain)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::open_menu_from_keyboard))
            .on_action(cx.listener(Self::menu_up))
            .on_action(cx.listener(Self::menu_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
            .children(menu)
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;

    #[gpui::test]
    fn masked_offsets_follow_characters(cx: &mut gpui::TestAppContext) {
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text("aé中!", cx);
            input.set_masked(true, cx);
            input
        });
        input.read_with(cx, |input, _| {
            // a(1) é(2) CJK(3) !(1): content byte offsets 0, 1, 3, 6, 7 map to dots 0-4
            let dot = MASK.len();
            for (content, shown) in [(0, 0), (1, 1), (3, 2), (6, 3), (7, 4)] {
                assert_eq!(input.display_offset(content), shown * dot);
                assert_eq!(input.content_offset(shown * dot), content);
            }
            // A click inside a dot still lands on a character boundary, never mid-character.
            assert_eq!(input.content_offset(dot + 1), 1);
            assert_eq!(input.content_offset(99), 7);
        });
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    #[test]
    fn long_text_scrolls_to_keep_the_caret_in_view() {
        let (field, text) = (crate::theme::scaled_px(100.), crate::theme::scaled_px(300.));
        let view = crate::theme::scaled_px(100.) - caret_width();
        // Fits: never scrolls.
        assert_eq!(
            follow_caret(
                px(0.),
                crate::theme::scaled_px(80.),
                crate::theme::scaled_px(80.),
                field
            ),
            px(0.)
        );
        // Caret past the right edge (typing or pasting at the end): just enough to show it.
        assert_eq!(follow_caret(px(0.), text, text, field), text - view);
        // Moving left inside the visible part leaves the scroll alone.
        assert_eq!(
            follow_caret(text - view, crate::theme::scaled_px(250.), text, field),
            text - view
        );
        // Home: back to the start.
        assert_eq!(follow_caret(text - view, px(0.), text, field), px(0.));
        // Text got shorter (deleted at the end): no empty space after it.
        assert_eq!(
            follow_caret(
                crate::theme::scaled_px(200.),
                crate::theme::scaled_px(120.),
                crate::theme::scaled_px(120.),
                field
            ),
            crate::theme::scaled_px(120.) - view
        );
    }
}

#[cfg(test)]
mod stale_layout_tests {
    use super::*;

    /// The text changes while the mouse is selecting (Tab completes the path during a drag), and a
    /// mouse event arrives before the next frame: its position is measured on the old line. It
    /// used to land inside a character of the new text, and the next key pressed panicked.
    #[gpui::test]
    fn a_mouse_event_on_the_previous_frame_lands_on_a_character(cx: &mut gpui::TestAppContext) {
        // New text, and the offset on the old line: inside a character, and past the end.
        for (new_text, old_offset) in [("a中中", 3), ("a", 9)] {
            let (input, cx) = cx.add_window_view(|_, cx| {
                let mut input = TextInput::new("", cx);
                input.set_text("中中中", cx);
                input
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                input.update(cx, |input, cx| {
                    let line = input.last_layout.clone().expect("drawn");
                    let bounds = input.last_bounds.expect("drawn");
                    let at = point(
                        bounds.left() + line.x_for_index(old_offset),
                        bounds.center().y,
                    );
                    input.set_text(new_text, cx);
                    input.is_selecting = true;
                    let event = MouseMoveEvent {
                        position: at,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: gpui::Modifiers::none(),
                    };
                    input.on_mouse_move(&event, window, cx);
                    let range = input.selected_range.clone();
                    assert!(
                        input.content.get(range.clone()).is_some(),
                        "{range:?} splits a character of {:?} or runs past it",
                        input.content
                    );
                    input.replace_text_in_range(None, "x", window, cx);
                });
            });
        }
    }
    /// Dragging a selection out of the field keeps extending it to the nearest end; a move
    /// without a press does nothing (#410).
    #[gpui::test]
    fn drag_selection_follows_the_pointer_outside_the_field(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| {
            let mut input = TextInput::new("", cx);
            input.set_text("hello world", cx);
            input
        });
        cx.run_until_parked();
        let (bounds, line) = input.update(cx, |input, _| {
            (
                input.last_bounds.expect("drawn"),
                input.last_layout.clone().expect("drawn"),
            )
        });
        let mid = point(bounds.left() + line.x_for_index(5), bounds.center().y);
        let right = point(bounds.right() + px(300.), bounds.center().y);
        let left = point(bounds.left() - px(300.), bounds.center().y);
        let range = |cx: &mut gpui::VisualTestContext| {
            input.update(cx, |input, _| input.selected_range.clone())
        };

        // No press: moving outside changes nothing.
        cx.simulate_mouse_move(right, None, gpui::Modifiers::none());
        assert_eq!(range(cx), 11..11);

        cx.simulate_mouse_down(mid, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(right, Some(MouseButton::Left), gpui::Modifiers::none());
        assert_eq!(range(cx), 5..11);
        cx.simulate_mouse_move(left, Some(MouseButton::Left), gpui::Modifiers::none());
        assert_eq!(range(cx), 0..5);
        cx.simulate_mouse_move(right, Some(MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(right, MouseButton::Left, gpui::Modifiers::none());
        assert_eq!(range(cx), 5..11);

        // After the release, moving does not extend the selection any more.
        cx.simulate_mouse_move(left, None, gpui::Modifiers::none());
        assert_eq!(range(cx), 5..11);
    }
}

/// Whether a character (the first of a grapheme) belongs to a word: letters, digits and `_`.
/// Everything else (spaces, `\`, `/`, `.`, `-`, ...) separates words, so in a path or a file
/// name each part is a word of its own.
fn is_word(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// The word at `offset` in `text` (the one ending there at its end), or the run of separators
/// there, for a double-click.
pub(crate) fn word_at(text: &str, offset: usize) -> Range<usize> {
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let Some(ix) = graphemes
        .iter()
        .rposition(|(start, _)| *start <= offset)
        .map(|ix| {
            // At the very end, or just after a word: that word.
            let at_end = graphemes[ix].0 == offset && ix > 0 && !is_word(graphemes[ix].1);
            if (offset >= text.len() || at_end) && ix > 0 && is_word(graphemes[ix - 1].1) {
                ix - 1
            } else {
                ix
            }
        })
    else {
        return 0..0;
    };
    let word = is_word(graphemes[ix].1);
    let same = |g: &(usize, &str)| is_word(g.1) == word;
    let first = graphemes[..ix]
        .iter()
        .rposition(|g| !same(g))
        .map_or(0, |i| i + 1);
    let last = graphemes[ix..]
        .iter()
        .position(|g| !same(g))
        .map_or(graphemes.len(), |i| ix + i);
    let end = graphemes.get(last).map_or(text.len(), |g| g.0);
    graphemes[first].0..end
}

/// Start of the word before `offset` in `text`: separators and spaces right before it are skipped.
pub(crate) fn previous_word(text: &str, offset: usize) -> usize {
    let mut seen_word = false;
    let mut at = 0;
    for (start, g) in text.grapheme_indices(true).rev() {
        if start >= offset {
            continue;
        }
        if is_word(g) {
            at = start;
            seen_word = true;
        } else if seen_word {
            return at;
        }
    }
    if seen_word { at } else { 0 }
}

/// Start of the word after `offset` in `text` (the end when there is none), as Ctrl+Right in a
/// Windows edit box.
pub(crate) fn next_word(text: &str, offset: usize) -> usize {
    let mut passed_separator = false;
    for (start, g) in text.grapheme_indices(true) {
        if start < offset {
            continue;
        }
        if !is_word(g) {
            passed_separator = true;
        } else if passed_separator {
            return start;
        }
    }
    text.len()
}
