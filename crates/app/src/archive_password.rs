//! The password for a protected zip or 7z (#346), asked when extracting it.
//!
//! The password lives only in this dialog's input and the extraction it starts: never logged,
//! never saved, not copyable. A wrong one brings the dialog back at once (the password is checked
//! before anything is extracted) with that said and the input empty, without the opening motion,
//! so it reads as the same dialog staying open.

use gpui::{
    AnimationExt, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyBinding, Subscription, Window, actions, div, prelude::*,
};
use rust_i18n::t;

use crate::a11y::A11yExt as _;
use crate::net_dialog::{button, field, label};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

actions!(archive_password, [ArchivePasswordCancel]);

const CONTEXT: &str = "ArchivePassword";

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        ArchivePasswordCancel,
        Some(CONTEXT),
    )]);
}

/// An extraction that stopped for a password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PasswordAsk {
    pub(crate) archive: std::path::PathBuf,
    /// Into the folder the archive is in, not a new one.
    pub(crate) here: bool,
    /// The last password didn't open it.
    pub(crate) wrong: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArchivePasswordEvent {
    /// Extract with this password.
    Submit(String),
    /// Cancelled or closed.
    Closed,
}

pub(crate) struct ArchivePasswordDialog {
    /// The archive's file name.
    name: String,
    /// The last password didn't open it.
    wrong: bool,
    /// Slides in: a first ask, not one after a wrong password.
    appear: bool,
    focus: FocusHandle,
    password: Entity<TextInput>,
    _input: Subscription,
}

impl EventEmitter<ArchivePasswordEvent> for ArchivePasswordDialog {}

impl Focusable for ArchivePasswordDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ArchivePasswordDialog {
    pub(crate) fn new(
        name: String,
        wrong: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let password = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_masked(true, cx);
            input
        });
        let input = cx.subscribe_in(&password, window, Self::on_input);
        window.focus(&password.focus_handle(cx), cx);
        Self {
            name,
            wrong,
            appear: !wrong,
            focus: cx.focus_handle(),
            password,
            _input: input,
        }
    }

    /// Enter extracts and Esc cancels in the input; typing clears "wrong password".
    fn on_input(
        &mut self,
        _: &Entity<TextInput>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Confirm => self.submit(cx),
            InputEvent::Cancel => self.cancel(cx),
            InputEvent::Changed => {
                if std::mem::take(&mut self.wrong) {
                    cx.notify();
                }
            }
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let password = self.password.read(cx).text().to_owned();
        if password.is_empty() {
            return;
        }
        self.password.update(cx, |input, cx| input.set_text("", cx));
        cx.emit(ArchivePasswordEvent::Submit(password));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.password.update(cx, |input, cx| input.set_text("", cx));
        cx.emit(ArchivePasswordEvent::Closed);
    }

    #[cfg(test)]
    pub(crate) fn type_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.password
            .update(cx, |input, cx| input.set_text(text.to_owned(), cx));
    }

    #[cfg(test)]
    pub(crate) fn submit_for_test(&mut self, cx: &mut Context<Self>) {
        self.submit(cx);
    }

    #[cfg(test)]
    pub(crate) const fn wrong_for_test(&self) -> bool {
        self.wrong
    }

    #[cfg(test)]
    pub(crate) const fn appears_for_test(&self) -> bool {
        self.appear
    }
}

impl Render for ArchivePasswordDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = t!("archive_password.title").into_owned();
        let panel = div()
            .id("archive-password")
            .a11y(gpui::Role::Dialog, title.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ArchivePasswordCancel, _, cx| this.cancel(cx)))
            // Clicks on the panel don't close it.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(theme::scaled_px(380.))
            .p(theme::s_20())
            .flex()
            .flex_col()
            .gap(theme::s_12())
            .rounded(theme::r_12())
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .ui_font()
            .text_size(theme::fs_14())
            .text_color(theme::text())
            .child(
                div()
                    .text_size(theme::fs_15())
                    .font_weight(theme::FW_STRONG)
                    .child(title),
            )
            .child(
                div()
                    .text_color(theme::text_muted())
                    .child(t!("archive_password.intro", name = self.name).into_owned()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme::s_4())
                    .child(label(t!("net.password").into_owned()))
                    .child(field(&self.password, window, cx)),
            )
            .when(crate::a11y::active(), |d| {
                d.child(crate::a11y::end_label_scope())
            })
            .when(self.wrong, |d| {
                d.child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::danger())
                        .child(t!("archive_password.wrong").into_owned()),
                )
            })
            .child(
                div()
                    .pt(theme::s_4())
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(theme::s_8())
                    .child(
                        button("archive-password-cancel", t!("confirm.cancel").into_owned(), false)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx))),
                    )
                    .child(
                        button("archive-password-ok", t!("archive_password.ok").into_owned(), true)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit(cx))),
                    ),
            );
        // Backdrop: clicking outside cancels.
        let veil = div()
            .id("archive-password-veil")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::veil())
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel(cx)),
            )
            .child(panel);
        if !self.appear {
            return veil.into_any_element();
        }
        veil.with_animation(
            "archive-password-appear",
            crate::motion::enter(),
            |el, t| crate::motion::slide_in(el, t, 0., 0.),
        )
        .into_any_element()
    }
}
