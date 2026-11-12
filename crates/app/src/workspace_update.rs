//! Settings > About > Updates: channel, auto-check, current state and the next action.
//! Also the "update available" badge on the sidebar Settings entry. Logic lives in `update.rs`.

use std::borrow::Cow;

use gpui::{AnyElement, App, ClickEvent, Context, SharedString, Window, div, prelude::*, px, svg};
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
        body(s, &state, ui),
        cx,
    )
}

// One arm per update state, kept in order so they are easy to compare.
#[allow(clippy::too_many_lines)]
fn body(s: &Settings, state: &UpdateState, ui: &Ui) -> AnyElement {
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
            let page = release.page.clone();
            let actions = div()
                .flex()
                .gap(theme::S_8)
                .child(button(
                    "update-notes",
                    t!("update.notes"),
                    false,
                    move |_, _, cx| {
                        open_url(&page, cx);
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
        UpdateState::Downloading(release, got, total) => {
            #[allow(clippy::cast_precision_loss)]
            let mb = |b: u64| b as f64 / 1_048_576.0;
            status_line(
                Icon::Refresh,
                theme::text_muted(),
                t!(
                    "update.downloading",
                    version = release.version,
                    got = format!("{:.1}", mb(got)),
                    total = format!("{:.1}", mb(total))
                )
                .into_owned(),
                None,
            )
        }
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
