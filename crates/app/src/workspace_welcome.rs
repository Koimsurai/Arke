//! First run (no settings file yet): a small card in the corner offering a Start menu shortcut,
//! and optionally starting with Windows. Arke is a single exe that may live anywhere, so without
//! this it can't be found from Start.
//!
//! Either button answers it for good (`start_menu_offered`); the same choices stay in
//! Settings > Startup.

use gpui::{AnimationExt, App, SharedString, div, prelude::*, svg};
use rust_i18n::t;

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::settings::SettingsStore;
use crate::theme;

/// The card's "Also start when I sign in" box (shared by every window).
#[derive(Clone, Copy, Default)]
struct WelcomeChoice {
    autostart: bool,
}

impl gpui::Global for WelcomeChoice {}

/// Whether the first-run card is due: first run, not answered, no shortcut yet.
pub(crate) fn welcome_due(cx: &App) -> bool {
    SettingsStore::try_global(cx).is_some_and(|store| {
        let store = store.read(cx);
        store.loaded
            && store.first_run
            && !store.settings.start_menu_offered
            && !store.settings.start_menu
    })
}

/// Answers the card: `add` turns on the Start menu shortcut (and starting with Windows if ticked).
pub(crate) fn answer_welcome(add: bool, cx: &mut App) {
    let autostart = cx
        .try_global::<WelcomeChoice>()
        .is_some_and(|c| c.autostart);
    SettingsStore::global(cx).update(cx, |store, cx| {
        store.update(
            |st| {
                st.start_menu_offered = true;
                if add {
                    st.start_menu = true;
                    st.autostart |= autostart;
                }
            },
            cx,
        );
    });
}

fn welcome_button(
    id: &'static str,
    label: std::borrow::Cow<'static, str>,
    primary: bool,
    add: bool,
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
        .cursor_pointer()
        .text_size(theme::FS_12)
        .when(primary, |d| {
            d.bg(theme::signal())
                .text_color(theme::on_signal())
                .hover(|s| s.opacity(0.9))
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(label)
        .on_click(move |_, _, cx| answer_welcome(add, cx))
        .into_any_element()
}

/// The first-run card, bottom right, or `None` when not due.
// One card, top to bottom.
#[allow(clippy::too_many_lines)]
pub(super) fn welcome_card(cx: &App) -> Option<gpui::AnyElement> {
    if !welcome_due(cx) {
        return None;
    }
    let autostart = cx
        .try_global::<WelcomeChoice>()
        .is_some_and(|c| c.autostart);
    Some(crate::motion::corner_card(
        div()
            .id("welcome")
            .debug_selector(|| "welcome".to_owned())
            .a11y(gpui::Role::Dialog, t!("welcome.title"))
            .occlude()
            .w_full()
            .p(theme::S_16)
            .flex()
            .flex_col()
            .gap(theme::S_12)
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .text_size(theme::FS_13)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(theme::S_12)
                    .child(
                        svg()
                            .path(Icon::WindowStart.path())
                            .flex_none()
                            .mt(theme::S_2)
                            .size(theme::ICON)
                            .text_color(theme::signal()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(theme::S_4)
                            .child(
                                div()
                                    .text_size(theme::FS_14)
                                    .font_weight(theme::FW_STRONG)
                                    .child(t!("welcome.title").into_owned()),
                            )
                            .child(
                                div()
                                    .text_color(theme::text_muted())
                                    .child(t!("welcome.body").into_owned()),
                            ),
                    ),
            )
            .child(
                div()
                    .pl(theme::ICON + theme::S_12)
                    .child(super::report_page::check_row(
                        "welcome-autostart",
                        t!("welcome.autostart"),
                        autostart,
                        |cx| {
                            let mut choice = cx
                                .try_global::<WelcomeChoice>()
                                .copied()
                                .unwrap_or_default();
                            choice.autostart = !choice.autostart;
                            cx.set_global(choice);
                            cx.refresh_windows();
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle())
                            .child(t!("welcome.later_hint").into_owned()),
                    )
                    .child(welcome_button(
                        "welcome-not-now",
                        t!("welcome.not_now"),
                        false,
                        false,
                    ))
                    .child(welcome_button("welcome-add", t!("welcome.add"), true, true)),
            )
            .with_animation("welcome-appear", crate::motion::enter(), |el, t| {
                crate::motion::slide_in(el, t, 0., crate::motion::SLIDE)
            }),
    ))
}

#[cfg(test)]
impl SettingsStore {
    /// Pretends this is a first run (tests).
    pub(crate) fn set_first_run_for_test(&mut self, first_run: bool) {
        self.first_run = first_run;
        self.loaded = true;
    }
}
