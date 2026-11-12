//! Confirmation dialogs: before recycling (when enabled) and on extension changes.
//!
//! Covers the whole window like Quick Look. Enter confirms, Esc or clicking outside cancels.
//! Permanent delete always asks here, so the Windows copy engine never shows its own prompt.

use std::path::PathBuf;

use gpui::{
    AnimationExt, App, ClickEvent, Context, FocusHandle, KeyBinding, SharedString, Window, actions,
    deferred, div, prelude::*, px,
};
use rust_i18n::t;

use super::Explorer;
use super::ops::OpFollowUp;
use crate::a11y::A11yExt as _;
use crate::shell::FileOp;
use crate::theme::{self, UiFont as _};

actions!(explorer, [ConfirmOk, ConfirmCancel]);

const CONTEXT: &str = "ConfirmDialog";

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", ConfirmOk, Some(CONTEXT)),
        KeyBinding::new("escape", ConfirmCancel, Some(CONTEXT)),
    ]);
}

/// Operation to run on confirm.
pub(crate) enum ConfirmAction {
    /// File operation (delete, rename); `follow` decides the selection afterwards
    Op(FileOp, OpFollowUp),
    /// Recycle `recycle` and permanently delete `purge`, which are where there is no Recycle
    /// Bin.
    NoBin {
        recycle: Vec<PathBuf>,
        purge: Vec<PathBuf>,
        follow: OpFollowUp,
    },
}

pub(crate) struct Confirm {
    title: SharedString,
    body: SharedString,
    ok: SharedString,
    /// Use the danger color for the confirm button (delete)
    danger: bool,
    action: ConfirmAction,
    focus: FocusHandle,
}

impl Explorer {
    pub(super) fn ask(
        &mut self,
        title: String,
        body: String,
        ok: String,
        danger: bool,
        action: ConfirmAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.confirm = Some(Confirm {
            title: title.into(),
            body: body.into(),
            ok: ok.into(),
            danger,
            action,
            focus,
        });
        cx.notify();
    }

    /// Always ask before a permanent delete (replaces the Windows prompt).
    pub(super) fn ask_delete_permanently(
        &mut self,
        paths: Vec<PathBuf>,
        follow: OpFollowUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = match paths.as_slice() {
            [one] => t!(
                "recycle.confirm_purge_one",
                name = one
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            ),
            many => t!(
                "recycle.confirm_purge_many",
                count = arke_core::format::format_count(many.len())
            ),
        }
        .into_owned();
        self.ask(
            t!("recycle.confirm_purge_title").into_owned(),
            body,
            t!("recycle.purge").into_owned(),
            true,
            ConfirmAction::Op(
                FileOp::Delete {
                    paths,
                    recycle: false,
                },
                follow,
            ),
            window,
            cx,
        );
    }

    /// Some items to recycle are where Windows has no Recycle Bin (network share, USB stick):
    /// they would be deleted permanently, so ask like a permanent delete, naming why.
    pub(super) fn ask_no_recycle_bin(
        &mut self,
        recycle: Vec<PathBuf>,
        purge: Vec<PathBuf>,
        follow: OpFollowUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut body = match purge.as_slice() {
            [one] => t!(
                "confirm.no_bin_one",
                name = one.file_name().map_or_else(
                    || one.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
            ),
            many => t!(
                "confirm.no_bin_many",
                count = arke_core::format::format_count(many.len())
            ),
        }
        .into_owned();
        if !recycle.is_empty() {
            body.push_str(&t!("confirm.gap"));
            body.push_str(&tn!("confirm.no_bin_others", recycle.len()));
        }
        self.ask(
            t!("recycle.confirm_purge_title").into_owned(),
            body,
            t!("recycle.purge").into_owned(),
            true,
            ConfirmAction::NoBin {
                recycle,
                purge,
                follow,
            },
            window,
            cx,
        );
    }

    /// Ask once before recycling (when "Confirm before deleting" is on).
    pub(super) fn ask_recycle(
        &mut self,
        paths: Vec<PathBuf>,
        follow: OpFollowUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = match paths.as_slice() {
            [one] => t!(
                "confirm.recycle_one",
                name = one
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            ),
            many => t!("confirm.recycle_many", count = many.len()),
        }
        .into_owned();
        self.ask(
            t!("confirm.recycle_title").into_owned(),
            body,
            t!("confirm.recycle_ok").into_owned(),
            true,
            ConfirmAction::Op(
                FileOp::Delete {
                    paths,
                    recycle: true,
                },
                follow,
            ),
            window,
            cx,
        );
    }

    /// Extension changed: ask once, like Explorer, since the file may no longer open.
    pub(super) fn ask_extension_change(
        &mut self,
        path: PathBuf,
        new_name: String,
        follow: OpFollowUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ask(
            t!("confirm.ext_title").into_owned(),
            t!("confirm.ext_body", name = new_name.as_str()).into_owned(),
            t!("confirm.ext_ok").into_owned(),
            false,
            ConfirmAction::Op(FileOp::Rename { path, new_name }, follow),
            window,
            cx,
        );
    }

    fn confirm_ok(&mut self, _: &ConfirmOk, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        window.focus(&self.list_focus, cx);
        match confirm.action {
            ConfirmAction::Op(op, follow) => self.run_op(op, follow, cx),
            ConfirmAction::NoBin {
                recycle,
                purge,
                follow,
            } => {
                let purge_follow = super::ops::OpFollowUp::purge_part(&follow);
                if !recycle.is_empty() {
                    let follow = follow.recycled(recycle.len());
                    self.run_op(
                        FileOp::Delete {
                            paths: recycle,
                            recycle: true,
                        },
                        follow,
                        cx,
                    );
                }
                self.run_op(
                    FileOp::Delete {
                        paths: purge,
                        recycle: false,
                    },
                    purge_follow,
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Cancel (Esc, the button, clicking outside): the dialog fades out. Confirming closes it at
    /// once, since the operation starts right away.
    fn confirm_cancel(&mut self, _: &ConfirmCancel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(confirm) = self.confirm.take() {
            crate::motion::Leaving::start(self, confirm, |this| &mut this.closing_confirm, cx);
            window.focus(&self.list_focus, cx);
            cx.notify();
        }
    }

    /// Whether a confirmation is open (tests).
    #[cfg(test)]
    pub(crate) fn confirm_title(&self) -> Option<String> {
        self.confirm.as_ref().map(|c| c.title.to_string())
    }

    /// The open confirmation's text (tests).
    #[cfg(test)]
    pub(crate) fn confirm_body(&self) -> Option<String> {
        self.confirm.as_ref().map(|c| c.body.to_string())
    }

    pub(super) fn render_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if let Some(confirm) = &self.confirm {
            return Some(Self::confirm_dialog(confirm, None, cx));
        }
        // A cancelled dialog fading out. Its buttons and veil act on `self.confirm`, which is
        // already gone, so they do nothing meanwhile.
        let ghost = self.closing_confirm.as_ref()?;
        Some(Self::confirm_dialog(
            &ghost.value,
            Some(ghost.id("confirm-leave")),
            cx,
        ))
    }

    /// The veil and the dialog: entering, or (`leaving`: the ghost's animation id) fading out.
    #[allow(clippy::too_many_lines)]
    fn confirm_dialog(
        confirm: &Confirm,
        leaving: Option<gpui::ElementId>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let out = leaving.is_some();
        deferred(
            div()
                .id("confirm-veil")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::veil())
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.confirm_cancel(&ConfirmCancel, window, cx);
                }))
                .child(
                    div()
                        .id("confirm")
                        .a11y(gpui::Role::AlertDialog, confirm.title.clone())
                        .aria_description(confirm.body.clone())
                        .key_context(CONTEXT)
                        .track_focus(&confirm.focus)
                        .on_action(cx.listener(Self::confirm_ok))
                        .on_action(cx.listener(Self::confirm_cancel))
                        .w(px(420.))
                        .p(theme::S_20)
                        .flex()
                        .flex_col()
                        .gap(theme::S_12)
                        .rounded(theme::R_12)
                        .bg(theme::overlay())
                        .shadow(theme::elev_3())
                        .ui_font()
                        .text_size(theme::FS_14)
                        .text_color(theme::text())
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_size(theme::FS_15)
                                .font_weight(theme::FW_STRONG)
                                .child(confirm.title.clone()),
                        )
                        .child(
                            div()
                                .text_color(theme::text_muted())
                                .child(confirm.body.clone()),
                        )
                        .child(
                            div()
                                .pt(theme::S_8)
                                .flex()
                                .justify_end()
                                .gap(theme::S_8)
                                .child(
                                    dialog_button(
                                        "confirm-cancel",
                                        t!("confirm.cancel").into_owned().into(),
                                        false,
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, window, cx| {
                                            this.confirm_cancel(&ConfirmCancel, window, cx);
                                        },
                                    )),
                                )
                                .child(
                                    dialog_button(
                                        "confirm-ok",
                                        confirm.ok.clone(),
                                        true,
                                        confirm.danger,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, window, cx| {
                                            this.confirm_ok(&ConfirmOk, window, cx);
                                        },
                                    )),
                                ),
                        ),
                )
                .with_animation(
                    leaving.unwrap_or_else(|| gpui::ElementId::Name("confirm-appear".into())),
                    if out {
                        crate::motion::exit()
                    } else {
                        crate::motion::enter()
                    },
                    move |el, t| {
                        if out {
                            crate::motion::slide_out(el, t, 0., 0.)
                        } else {
                            crate::motion::slide_in(el, t, 0., 0.)
                        }
                    },
                ),
        )
        .with_priority(2)
        .into_any_element()
    }
}

/// Dialog button: primary uses the accent color (danger for delete), the other is outlined.
fn dialog_button(
    id: &'static str,
    label: SharedString,
    primary: bool,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_button(label.clone())
        .h(theme::H_CTL)
        .px(theme::S_16)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(primary && danger, |d| {
            d.bg(theme::danger()).text_color(theme::on_signal())
        })
        .when(primary && !danger, |d| {
            d.bg(theme::signal()).text_color(theme::on_signal())
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(label)
}

/// Whether a rename changes the extension (folders and case-only changes don't count).
pub(crate) fn extension_changed(old: &str, new: &str) -> bool {
    let ext = |name: &str| {
        name.rfind('.')
            .filter(|&dot| dot > 0)
            .map(|dot| name[dot + 1..].to_lowercase())
    };
    ext(old) != ext(new)
}

#[cfg(test)]
mod tests {
    use super::extension_changed;

    #[test]
    fn detects_extension_changes() {
        assert!(extension_changed("report.docx", "report.pdf"));
        assert!(extension_changed("report.docx", "report"));
        assert!(extension_changed("notes", "notes.txt"));
        assert!(!extension_changed("report.docx", "new report.docx"));
        assert!(!extension_changed("a.TXT", "b.txt"), "case doesn't count");
        assert!(
            !extension_changed(".gitignore", ".env"),
            "a leading dot is not an extension"
        );
    }
}
