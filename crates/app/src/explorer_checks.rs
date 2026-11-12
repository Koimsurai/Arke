//! Selection checkboxes (Files and folders > Show checkboxes, off by default).
//!
//! Same as Explorer's item checkboxes: a box appears left of the icon on hover or while something
//! is selected. Clicking it toggles that item (like Ctrl+click) and never opens or starts a drag.
//! Details view has a select-all box in the header (all, some, none); grid view puts it top-left.

use gpui::{
    AnyElement, Context, MouseButton, MouseDownEvent, SharedString, Window, div, prelude::*, px,
    svg,
};

use super::Explorer;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;

/// Checkbox state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Off,
    /// Header select-all: partially selected
    Some,
    On,
}

/// Draw one checkbox. With `visible` false it keeps its space but stays hidden until hover.
fn check_box(state: Check, visible: bool) -> gpui::Div {
    let on = state != Check::Off;
    div()
        .size(theme::ICON)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .when(on, |d| d.bg(theme::signal()).border_color(theme::signal()))
        .when(!on, |d| {
            d.bg(theme::surface()).border_color(theme::text_subtle())
        })
        .when(!visible && !on, gpui::Styled::invisible)
        .children(match state {
            Check::On => Some(
                svg()
                    .path(Icon::Check.path())
                    .size(px(12.))
                    .text_color(theme::on_signal())
                    .into_any_element(),
            ),
            Check::Some => Some(
                div()
                    .w(px(8.))
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(theme::on_signal())
                    .into_any_element(),
            ),
            Check::Off => None,
        })
}

impl Explorer {
    /// Group name for the row (or cell) so the box shows on hover.
    pub(super) fn check_group(ix: usize) -> SharedString {
        SharedString::from(format!("item-{ix}"))
    }

    /// Per-item checkbox. Always visible while something is selected, otherwise only on hover.
    pub(super) fn item_checkbox(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let picking = !self.selection.is_empty();
        let state = if selected { Check::On } else { Check::Off };
        div()
            .id(("item-check", ix))
            .a11y(gpui::Role::CheckBox, rust_i18n::t!("a11y.select_item"))
            .aria_toggled(crate::a11y::toggled(selected))
            .flex_none()
            .p(px(2.))
            .m(px(-2.))
            .cursor_pointer()
            .child(
                check_box(state, picking)
                    .group_hover(Self::check_group(ix), gpui::Styled::visible),
            )
            // Handle it here so the row never sees it: no select-only, no drag, no open on double
            // click.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.list_focus, cx);
                    this.toggle_checked(ix, cx);
                }),
            )
            .into_any_element()
    }

    fn toggle_checked(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(view) = self.view() {
            self.selection.toggle(&view, ix);
            self.cursor = Some(ix);
            self.click_select = None;
            cx.notify();
        }
    }

    /// Header select-all: clears when everything is selected, otherwise selects all.
    pub(super) fn header_checkbox(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.view().map_or(0, |v| v.len());
        let chosen = self.selection.len();
        let state = match chosen {
            0 => Check::Off,
            n if n >= total && total > 0 => Check::On,
            _ => Check::Some,
        };
        div()
            .id("header-check")
            .a11y(gpui::Role::CheckBox, rust_i18n::t!("checks.select_all"))
            .aria_toggled(match state {
                Check::Off => gpui::Toggled::False,
                Check::On => gpui::Toggled::True,
                Check::Some => gpui::Toggled::Mixed,
            })
            .flex_none()
            .cursor_pointer()
            .child(check_box(state, true))
            .tooltip(crate::workspace::tooltip(rust_i18n::t!(
                "checks.select_all"
            )))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window: &mut Window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.list_focus, cx);
                    if let Some(view) = this.view() {
                        if state == Check::On {
                            this.selection.clear();
                            this.cursor = None;
                        } else {
                            this.selection.select_all(&view);
                        }
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }
}
