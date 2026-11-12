//! Settings > About > "Remove Arke from this PC": a danger button and a confirmation dialog that
//! lists what goes (from `uninstall::plan`, so it matches what runs), with "Keep my settings".
//!
//! Confirming starts `arke --uninstall --wait-pid <this process>` and quits; that process removes
//! everything once this one is gone and shows the summary (`uninstall.rs`).

use std::borrow::Cow;

use gpui::{
    AnimationExt, App, ClickEvent, Context, FocusHandle, KeyBinding, SharedString, Window, actions,
    div, prelude::*, px,
};
use rust_i18n::t;

use super::Workspace;
use super::settings_panel::card;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme::{self, UiFont as _};
use crate::uninstall::{self, Step};

actions!(workspace, [CancelUninstall]);

const CONTEXT: &str = "UninstallDialog";

pub(super) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CancelUninstall, Some(CONTEXT))]);
}

/// The open confirmation.
pub(super) struct UninstallAsk {
    keep_settings: bool,
    /// Whether the search service is installed; `None` until Windows answered.
    service: Option<bool>,
    /// Starting the removal failed.
    error: Option<String>,
    focus: FocusHandle,
}

/// The About card's button: outlined in the danger color, so it reads as destructive without
/// shouting next to the other cards.
fn danger_button(
    id: &'static str,
    label: Cow<'static, str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let label = SharedString::from(label.into_owned());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_button(label.clone())
        .h(theme::H_CTL_SM)
        .px(theme::S_12)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .border_color(theme::danger())
        .text_color(theme::danger())
        .cursor_pointer()
        .text_size(theme::FS_12)
        .hover(|s| s.bg(theme::danger_surface()))
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

/// Settings > About row.
pub(super) fn uninstall_card(cx: &mut Context<Workspace>) -> gpui::AnyElement {
    card(
        Icon::Trash,
        t!("uninstall.title"),
        Some(t!("uninstall.desc")),
        danger_button(
            "uninstall-open",
            t!("uninstall.button"),
            cx.listener(|this, _: &ClickEvent, window, cx| this.ask_uninstall(window, cx)),
        ),
    )
}

/// Dialog button: the primary one is filled in the danger color, the other outlined.
fn dialog_button(
    id: &'static str,
    label: Cow<'static, str>,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let label = SharedString::from(label.into_owned());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_button(label.clone())
        .h(theme::H_CTL)
        .px(theme::S_16)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(primary, |d| {
            d.bg(theme::danger())
                .text_color(theme::on_signal())
                .hover(|s| s.opacity(0.9))
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(label)
}

impl Workspace {
    /// Opens the confirmation and asks Windows in the background whether the search service is
    /// installed.
    pub(super) fn ask_uninstall(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &mut self.settings_panel else {
            return;
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        page.uninstall = Some(UninstallAsk {
            keep_settings: false,
            service: None,
            error: None,
            focus,
        });
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("uninstall-service-check", move || {
            let _ = tx.send(arke_indexd::client::installed());
        });
        cx.spawn(async move |this, cx| {
            if let Ok(installed) = rx.recv_async().await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(ask) = this
                        .settings_panel
                        .as_mut()
                        .and_then(|p| p.uninstall.as_mut())
                    {
                        ask.service = Some(installed);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn cancel_uninstall(
        &mut self,
        _: &CancelUninstall,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(page) = &mut self.settings_panel
            && page.uninstall.take().is_some()
        {
            window.focus(&page.focus, cx);
            cx.notify();
        }
    }

    fn toggle_keep_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(ask) = self
            .settings_panel
            .as_mut()
            .and_then(|p| p.uninstall.as_mut())
        {
            ask.keep_settings = !ask.keep_settings;
            cx.notify();
        }
    }

    /// "Remove Arke": hands over to `arke --uninstall` and quits.
    fn confirm_uninstall(&mut self, cx: &mut Context<Self>) {
        let Some(ask) = self
            .settings_panel
            .as_mut()
            .and_then(|p| p.uninstall.as_mut())
        else {
            return;
        };
        match uninstall::start_from_app(ask.keep_settings) {
            Ok(()) => {
                log::info!("uninstall: handed over, quitting");
                cx.quit();
            }
            Err(err) => {
                ask.error =
                    Some(t!("uninstall.start_failed", error = err.to_string()).into_owned());
                cx.notify();
            }
        }
    }

    /// Opens the confirmation as the About page's button does (tests).
    #[cfg(test)]
    pub(crate) fn open_uninstall_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_uninstall(window, cx);
    }

    /// Whether the confirmation is open, and with "Keep my settings" ticked (tests).
    #[cfg(test)]
    pub(crate) fn uninstall_ask_for_test(&self) -> Option<bool> {
        self.settings_panel
            .as_ref()
            .and_then(|p| p.uninstall.as_ref())
            .map(|a| a.keep_settings)
    }

    /// The confirmation over the settings page, or `None` when closed.
    // The dialog reads top to bottom: title, list, option, notes, buttons.
    #[allow(clippy::too_many_lines)]
    pub(super) fn render_uninstall_dialog(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ask = self.settings_panel.as_ref()?.uninstall.as_ref()?;
        let machine = uninstall::Machine::detect(ask.service == Some(true))?;
        let steps = uninstall::plan(&machine, ask.keep_settings);
        let items = steps
            .iter()
            .filter(|s| !matches!(s, Step::QuitArke))
            .map(|step| {
                let kept = matches!(step, Step::KeepSettings(_) | Step::KeepExe(_));
                div()
                    .flex()
                    .gap(theme::S_8)
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .text_color(theme::text_subtle())
                            .child(if kept { "–" } else { "•" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(kept, |d| d.text_color(theme::text_muted()))
                            .child(uninstall::label(step)),
                    )
            })
            .collect::<Vec<_>>();
        let has_service = steps.contains(&Step::RemoveService);
        let keep = ask.keep_settings;
        let panel = div()
            .id("uninstall-dialog")
            .a11y(gpui::Role::AlertDialog, t!("uninstall.confirm_title"))
            .key_context(CONTEXT)
            .track_focus(&ask.focus)
            .on_action(cx.listener(Self::cancel_uninstall))
            .w(px(520.))
            .max_w_full()
            .p(theme::S_20)
            .flex()
            .flex_col()
            .gap(theme::S_12)
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .ui_font()
            .text_size(theme::FS_13)
            .text_color(theme::text())
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(theme::FS_15)
                    .font_weight(theme::FW_STRONG)
                    .child(t!("uninstall.confirm_title").into_owned()),
            )
            .child(
                div()
                    .text_color(theme::text_muted())
                    .child(t!("uninstall.intro").into_owned()),
            )
            .child(div().flex().flex_col().gap(theme::S_6).children(items))
            .child(
                div()
                    .pt(theme::S_4)
                    .flex()
                    .flex_col()
                    .gap(theme::S_4)
                    .child(super::report_page::check_row(
                        "uninstall-keep",
                        t!("uninstall.keep"),
                        keep,
                        {
                            let workspace = cx.entity().downgrade();
                            move |cx| {
                                let _ = workspace.update(cx, Self::toggle_keep_settings);
                            }
                        },
                    ))
                    .child(
                        div()
                            .pl(theme::ICON + theme::S_8)
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle())
                            .child(t!("uninstall.keep_desc").into_owned()),
                    ),
            )
            .child(
                div()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_muted())
                    .when(has_service, |d| {
                        d.child(div().child(t!("uninstall.uac_note").into_owned()))
                    })
                    .child(div().child(t!("uninstall.closes_note").into_owned())),
            )
            .children(ask.error.clone().map(|error| {
                div()
                    .text_size(theme::FS_12)
                    .text_color(theme::danger())
                    .child(error)
            }))
            .child(
                div()
                    .pt(theme::S_8)
                    .flex()
                    .justify_end()
                    .gap(theme::S_8)
                    .child(
                        dialog_button("uninstall-cancel", t!("confirm.cancel"), false).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.cancel_uninstall(&CancelUninstall, window, cx);
                            }),
                        ),
                    )
                    .child(
                        dialog_button("uninstall-confirm", t!("uninstall.ok"), true).on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.confirm_uninstall(cx)),
                        ),
                    ),
            );
        Some(
            div()
                .id("uninstall-veil")
                .absolute()
                .inset_0()
                .p(theme::S_16)
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::veil())
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.cancel_uninstall(&CancelUninstall, window, cx);
                    }),
                )
                .child(panel)
                .with_animation("uninstall-appear", crate::motion::enter(), |el, t| {
                    crate::motion::slide_in(el, t, 0., 0.)
                })
                .into_any_element(),
        )
    }
}
