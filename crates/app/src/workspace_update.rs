//! Settings > About > Updates: channel, auto-check, current state and the next action.
//! Also the "update available" badge on the sidebar Settings entry. Logic lives in `update.rs`.

use std::borrow::Cow;

use gpui::{
    AnimationExt as _, AnyElement, App, ClickEvent, Context, SharedString, Window, div, prelude::*,
    px, svg,
};
use rust_i18n::t;

use super::Workspace;
use super::settings_panel::{Ui, choice, expander, switch};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::build_info::{self, Channel};
use crate::settings::Settings;
use crate::theme;
use crate::update::{self, UpdateState};

/// Summary text on the right side of the card: the update status. The version itself is shown
/// once, in the About header.
fn summary(state: &UpdateState) -> String {
    match state {
        UpdateState::Idle => t!("update.never_checked").into_owned(),
        UpdateState::Checking => t!("update.checking").into_owned(),
        UpdateState::UpToDate => t!("update.up_to_date").into_owned(),
        UpdateState::Available(r) => t!("update.available", version = r.version).into_owned(),
        UpdateState::Downloading(..) => t!("update.downloading_short").into_owned(),
        UpdateState::Ready(..) => t!("update.ready_short").into_owned(),
        UpdateState::Failed(_) => t!("update.failed_short").into_owned(),
    }
}

pub(super) fn update_card(
    s: &Settings,
    open: bool,
    ui: &Ui,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let state = UpdateState::get(cx);
    let summary = div()
        .text_size(theme::FS_12)
        .text_color(if UpdateState::has_news(cx) {
            theme::signal()
        } else {
            theme::text_muted()
        })
        .child(summary(&state))
        .into_any_element();
    expander(
        "update",
        Icon::Update,
        t!("update.title"),
        t!("update.desc_short"),
        summary,
        open,
        body(s, &state, ui, cx.weak_entity()),
        cx,
    )
}

// One arm per update state, kept in order so they are easy to compare.
#[allow(clippy::too_many_lines)]
fn body(
    s: &Settings,
    state: &UpdateState,
    ui: &Ui,
    workspace: gpui::WeakEntity<Workspace>,
) -> AnyElement {
    // Scoop or winget owns the exe: channels and checks don't apply, point at its command instead.
    if let Some(manager) = update::package_manager() {
        return div()
            .child(status_line(
                Icon::Info,
                theme::text_muted(),
                t!(
                    "update.managed",
                    manager = manager.name(),
                    command = manager.update_command()
                )
                .into_owned(),
                None,
            ))
            .into_any_element();
    }
    let no_source = build_info::RELEASES_REPO.is_empty();
    let channel = choice(
        "update-channel",
        &[
            (Channel::Stable, Channel::Stable.label()),
            (Channel::Rc, Channel::Rc.label()),
            (Channel::Nightly, Channel::Nightly.label()),
        ],
        s.update_channel(),
        |st, v| st.update_channel_choice = Some(v),
        ui,
    );
    let auto = switch("update-auto", s.update_auto, |st| {
        st.update_auto = !st.update_auto;
    });
    let mut out = div()
        .flex()
        .flex_col()
        .gap(theme::S_12)
        .child(row(
            t!("update.channel"),
            Some(t!("update.channel_desc")),
            channel,
        ))
        .child(row(t!("update.auto"), Some(t!("update.auto_desc")), auto));
    if no_source {
        return out
            .child(status_line(
                Icon::Info,
                theme::text_muted(),
                t!("update.no_source").into_owned(),
                None,
            ))
            .into_any_element();
    }
    let channel = s.update_channel();
    let line = match state.clone() {
        UpdateState::Idle | UpdateState::UpToDate | UpdateState::Failed(_) => {
            let (icon, color, text) = match state {
                UpdateState::UpToDate => (
                    Icon::Check,
                    theme::signal(),
                    t!("update.up_to_date").into_owned(),
                ),
                UpdateState::Failed(e) => (
                    Icon::Close,
                    theme::danger(),
                    t!("update.failed", error = e).into_owned(),
                ),
                _ => (
                    Icon::Info,
                    theme::text_muted(),
                    t!("update.never_checked").into_owned(),
                ),
            };
            status_line(
                icon,
                color,
                text,
                Some(button(
                    "update-check",
                    t!("update.check"),
                    true,
                    move |_, _, cx| {
                        update::start_check(channel, cx);
                    },
                )),
            )
        }
        UpdateState::Checking => status_line(
            Icon::Refresh,
            theme::text_muted(),
            t!("update.checking").into_owned(),
            None,
        ),
        UpdateState::Available(release) => {
            let notes = release.clone();
            let actions = div()
                .flex()
                .gap(theme::S_8)
                .child(button(
                    "update-notes",
                    t!("update.notes"),
                    false,
                    move |_, window, cx| {
                        let notes = notes.clone();
                        let _ = workspace.update(cx, |ws, cx| {
                            ws.open_whats_new(
                                super::whats_new_page::Mode::Available(notes),
                                window,
                                cx,
                            );
                        });
                    },
                ))
                .child(button(
                    "update-install",
                    t!("update.install"),
                    true,
                    move |_, _, cx| {
                        update::start_download(release.clone(), cx);
                    },
                ))
                .into_any_element();
            status_line(
                Icon::Bolt,
                theme::signal(),
                t!("update.available", version = release_version(state)).into_owned(),
                Some(actions),
            )
        }
        UpdateState::Downloading(release, got, total) => status_line(
            Icon::Refresh,
            theme::text_muted(),
            downloading_text(&release.version, got, total),
            None,
        ),
        UpdateState::Ready(release, exe) => status_line(
            Icon::Check,
            theme::signal(),
            t!("update.ready", version = release.version).into_owned(),
            Some(button(
                "update-restart",
                t!("update.restart"),
                true,
                move |_, _, cx| {
                    update::restart(&exe, cx);
                },
            )),
        ),
    };
    out = out.child(line);
    out.into_any_element()
}

/// "Downloading 0.4.0: 12.3 / 35.2 MB".
pub(super) fn downloading_text(version: &str, got: u64, total: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = |b: u64| b as f64 / 1_048_576.0;
    t!(
        "update.downloading",
        version = version,
        got = format!("{:.1}", mb(got)),
        total = format!("{:.1}", mb(total))
    )
    .into_owned()
}

fn release_version(state: &UpdateState) -> String {
    match state {
        UpdateState::Available(r) | UpdateState::Downloading(r, ..) | UpdateState::Ready(r, _) => {
            r.version.clone()
        }
        _ => String::new(),
    }
}

/// Opens a URL in the default browser on a background thread.
pub(super) fn open_url(url: &str, cx: &mut App) {
    if url.is_empty() {
        return;
    }
    let _ = cx;
    let url = url.to_string();
    crate::explorer::spawn_thread("open-url", move || {
        let _ = std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", &url])
            .spawn();
    });
}

fn row(
    title: Cow<'static, str>,
    desc: Option<Cow<'static, str>>,
    control: AnyElement,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(theme::S_12)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::S_2)
                .child(div().text_size(theme::FS_13).child(title.into_owned()))
                .children(desc.map(|d| {
                    div()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_subtle())
                        .child(d.into_owned())
                })),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn status_line(
    icon: Icon,
    color: gpui::Rgba,
    text: String,
    action: Option<AnyElement>,
) -> AnyElement {
    div()
        .min_h(px(40.))
        .px(theme::S_12)
        .py(theme::S_8)
        .flex()
        .items_center()
        .gap(theme::S_8)
        .rounded(theme::R_8)
        .bg(theme::wash())
        .child(
            svg()
                .path(icon.path())
                .flex_none()
                .size(theme::ICON_SM)
                .text_color(color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme::FS_12)
                .child(SharedString::from(text)),
        )
        .children(action)
        .into_any_element()
}

pub(super) fn button(
    id: &'static str,
    label: Cow<'static, str>,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_lazy(gpui::Role::Button, || label.to_string())
        .h(theme::H_CTL_SM)
        .px(theme::S_12)
        .flex()
        .flex_none()
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
        .child(label.into_owned())
        .on_click(on_click)
        .into_any_element()
}

/// "Update available" badge next to the sidebar Settings entry.
pub(super) fn sidebar_badge(cx: &App) -> Option<AnyElement> {
    UpdateState::has_news(cx).then(|| {
        div()
            .ml_auto()
            .mr(theme::S_8)
            .px(theme::S_6)
            .h(theme::S_20)
            .flex()
            .items_center()
            .rounded(theme::R_FULL)
            .bg(theme::signal_wash())
            .text_size(theme::FS_12)
            .text_color(theme::text())
            .child(t!("update.badge").to_string())
            .into_any_element()
    })
}

/// Corner card for a new version found by the check (automatic or by hand), with What's new
/// and Download; then the download's progress and Restart. Closing it hides it until a newer
/// version comes (`update_dismissed`); the sidebar badge and Settings › About stay.
pub(super) fn update_notice(cx: &mut Context<Workspace>) -> Option<AnyElement> {
    if super::welcome::welcome_due(cx) || update::package_manager().is_some() {
        return None;
    }
    let state = UpdateState::get(cx);
    let release = match &state {
        UpdateState::Available(r) | UpdateState::Downloading(r, ..) | UpdateState::Ready(r, _) => {
            r.clone()
        }
        _ => return None,
    };
    let dismissed = crate::settings::SettingsStore::global(cx)
        .read(cx)
        .settings
        .update_dismissed
        .clone();
    if dismissed.as_deref() == Some(release.version.as_str()) {
        return None;
    }
    let body = match &state {
        UpdateState::Downloading(r, got, total) => downloading_text(&r.version, *got, *total),
        UpdateState::Ready(r, _) => t!("update.ready", version = r.version).into_owned(),
        _ => t!("update.notice_body").into_owned(),
    };
    let workspace = cx.weak_entity();
    let actions = match state {
        UpdateState::Available(release) => {
            let notes = release.clone();
            vec![
                button(
                    "update-notice-notes",
                    t!("update.notes"),
                    false,
                    move |_, window, cx| {
                        let notes = notes.clone();
                        let _ = workspace.update(cx, |ws, cx| {
                            ws.open_whats_new(
                                super::whats_new_page::Mode::Available(notes),
                                window,
                                cx,
                            );
                        });
                    },
                ),
                button(
                    "update-notice-install",
                    t!("update.install"),
                    true,
                    move |_, _, cx| update::start_download(release.clone(), cx),
                ),
            ]
        }
        UpdateState::Ready(_, exe) => vec![button(
            "update-notice-restart",
            t!("update.restart"),
            true,
            move |_, _, cx| update::restart(&exe, cx),
        )],
        _ => Vec::new(),
    };
    Some(notice_card(&release.version, body, actions))
}

/// The notice's card: icon, title and text, a close button, and the actions underneath.
fn notice_card(version: &str, body: String, actions: Vec<AnyElement>) -> AnyElement {
    let title = t!("update.available", version = version).into_owned();
    let dismiss = version.to_string();
    let close = div()
        .id("update-notice-close")
        .debug_selector(|| "update-notice-close".to_owned())
        .a11y(gpui::Role::Button, t!("palette.hint_close"))
        .flex_none()
        .size(theme::H_CTL_SM)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash()))
        .child(
            svg()
                .path(Icon::Close.path())
                .size(theme::ICON_SM)
                .text_color(theme::text_muted()),
        )
        .on_click(move |_, _, cx| {
            let version = dismiss.clone();
            crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
                s.update(|st| st.update_dismissed = Some(version), cx);
            });
        });
    crate::motion::corner_card(
        div()
            .id("update-notice")
            .debug_selector(|| "update-notice".to_owned())
            .a11y(gpui::Role::Dialog, title.clone())
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
                            .path(Icon::Bolt.path())
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
                                    .child(title),
                            )
                            .child(div().text_color(theme::text_muted()).child(body)),
                    )
                    .child(close),
            )
            .when(!actions.is_empty(), |d| {
                d.child(div().flex().justify_end().gap(theme::S_8).children(actions))
            })
            .with_animation("update-notice-appear", crate::motion::enter(), |el, t| {
                crate::motion::slide_in(el, t, 0., crate::motion::SLIDE)
            }),
    )
}
