//! Settings > About > Report a problem: collects version, Windows build, main settings and recent log
//! lines into text that can be copied, saved, or used to open a prefilled GitHub issue.
//!
//! - Opening an issue needs a GitHub account; without one, copy or save and send it another way.
//! - Personal details are taken out first (`arke_core::scrub`): the profile folder becomes
//!   `%USERPROFILE%`, the account and computer names `<user>` and `<pc>`, servers `<server>`, and
//!   by default the names of files and folders `***`. The card shows the exact text that is copied
//!   or saved; saving it as a file is the way to edit it before sending.
//! - The URL only carries environment and settings (GitHub limits URL length). The full report goes
//!   to the clipboard for the user to paste.
//! - Log reading, file writes and clipboard access run on background threads.

use gpui::{App, Context, SharedString, div, prelude::*, px, svg};
use rust_i18n::t;

use super::Workspace;
use super::settings_panel::expander;
use super::update_page::{button, open_url};
use crate::assets::Icon;
use crate::build_info;
use crate::settings::{Settings, SettingsStore};
use crate::theme;

/// Number of recent log lines to include.
const LOG_LINES: usize = 120;

/// Result of the last action, shown in the card: a message or the error text.
#[derive(Clone, Default)]
pub(super) struct ReportNote(pub(super) Option<Result<String, String>>);

/// The report as it will be sent, prepared in the background and shown in the card.
#[derive(Clone)]
struct ReportDraft {
    /// Replace file and folder names with `***`.
    mask_names: bool,
    include_log: bool,
    /// `None` before the card was first opened.
    text: Option<String>,
    /// Bumped by every refresh so an older preparation can't overwrite a newer one.
    generation: u64,
    loading: bool,
}

impl Default for ReportDraft {
    fn default() -> Self {
        Self {
            mask_names: true,
            include_log: true,
            text: None,
            generation: 0,
            loading: false,
        }
    }
}

impl gpui::Global for ReportDraft {}

fn draft(cx: &App) -> ReportDraft {
    cx.try_global::<ReportDraft>().cloned().unwrap_or_default()
}

/// Who the report must not name: from the environment.
fn identity() -> arke_core::scrub::Identity {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    arke_core::scrub::Identity {
        profile: var("USERPROFILE"),
        user: var("USERNAME"),
        computer: var("COMPUTERNAME"),
    }
}

/// The report as sent: composed, then scrubbed. Runs on a background thread.
fn prepare(s: &Settings, with_log: bool, mask_names: bool) -> String {
    arke_core::scrub::scrub(&compose(s, with_log), &identity(), mask_names)
}

/// Why copying or saving the report failed.
#[derive(Debug, thiserror::Error)]
enum ReportError {
    /// There is no Downloads folder to save into.
    #[error("{}", t!("backup.no_folder"))]
    NoFolder,
    /// The report file could not be written.
    #[error("{0}")]
    Write(#[source] std::io::Error),
    /// The clipboard could not be set.
    #[error(transparent)]
    Clipboard(#[from] crate::shell::ShellError),
}

impl gpui::Global for ReportNote {}

/// Version, Windows build and language.
fn environment() -> String {
    let windows = crate::win::windows_version()
        .map_or_else(String::new, |(a, b, c)| format!("{a}.{b} (build {c})"));
    format!(
        "- Arke: {}\n- Windows: {windows}\n- Locale: {}\n",
        build_info::describe(),
        &*rust_i18n::locale()
    )
}

/// Main settings, excluding paths, recent folders and accounts.
fn settings_summary(s: &Settings) -> String {
    format!(
        "- theme {:?}, material {:?}, density {:?}, tabs {:?}\n\
         - view {:?}, sort {:?}{}, folders {:?}, groups {}\n\
         - hidden {}, protected {}, extensions {}, thumbnails {} (network {}), folder sizes {}\n\
         - single click {}, check boxes {}, preview {}, search this PC {}, keep running {}\n",
        s.theme,
        s.material,
        s.density,
        s.tab_layout,
        s.default_view,
        s.sort_by,
        if s.sort_descending {
            " (descending)"
        } else {
            ""
        },
        s.folders,
        s.group_rows,
        s.show_hidden,
        s.show_protected,
        s.show_extensions,
        s.show_thumbnails,
        s.network_thumbnails,
        s.folder_sizes,
        s.single_click,
        s.show_checkboxes,
        s.preview_pane,
        s.global_search,
        s.keep_running,
    )
}

/// Last lines of the log file. Runs on a background thread.
fn recent_log() -> String {
    let Some(path) = crate::crashlog::log_path() else {
        return t!("report.no_log").into_owned();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return t!("report.no_log").into_owned();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_LINES)..].join("\n")
}

/// Full report text. Runs on a background thread.
fn compose(s: &Settings, with_log: bool) -> String {
    let mut out = format!(
        "## {}\n\n{}\n## {}\n\n{}",
        t!("report.environment"),
        environment(),
        t!("report.settings"),
        settings_summary(s)
    );
    if with_log {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "\n## {}\n\n```\n{}\n```\n",
            t!("report.log"),
            recent_log()
        );
    }
    out
}

/// Percent-encodes a URL query value.
fn encode(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

/// New-issue URL with title, environment and settings. The log stays on the clipboard for the user to paste.
pub(super) fn issue_url(repo: &str, s: &Settings) -> String {
    let body = format!(
        "{}\n\n## {}\n\n{}\n\n{}",
        t!("report.describe_here"),
        t!("report.steps"),
        t!("report.steps_hint"),
        compose(s, false)
    ) + &format!("\n## {}\n\n{}\n", t!("report.log"), t!("report.paste_log"));
    let body = arke_core::scrub::scrub(&body, &identity(), true);
    format!(
        "https://github.com/{repo}/issues/new?title={}&body={}",
        encode(&t!("report.default_title")),
        encode(&body)
    )
}

/// Prepares the report again in the background (card opened, an option changed, Refresh).
fn refresh_draft(cx: &mut App) {
    let mut d = draft(cx);
    d.generation += 1;
    d.loading = true;
    let (generation, with_log, mask) = (d.generation, d.include_log, d.mask_names);
    cx.set_global(d);
    let settings = SettingsStore::global(cx).read(cx).settings.clone();
    let (tx, rx) = flume::bounded::<String>(1);
    crate::explorer::spawn_thread("report-prepare", move || {
        let _ = tx.send(prepare(&settings, with_log, mask));
    });
    cx.spawn(async move |cx| {
        if let Ok(text) = rx.recv_async().await {
            cx.update(|cx| {
                let mut d = draft(cx);
                if d.generation == generation {
                    d.text = Some(text);
                    d.loading = false;
                    cx.set_global(d);
                    cx.refresh_windows();
                }
            });
        }
    })
    .detach();
    cx.refresh_windows();
}

/// What to copy or save: the text the card shows when it is ready, plus the options to prepare
/// the same text if it isn't.
fn text_to_send(cx: &App) -> (Option<String>, bool, bool) {
    let d = draft(cx);
    (d.text.filter(|_| !d.loading), d.include_log, d.mask_names)
}

impl Workspace {
    /// Copies the full report to the clipboard, then opens `then_open` (the issue URL) if given.
    fn copy_report(&mut self, then_open: Option<String>, cx: &mut Context<Self>) {
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let shell = self.active_view().read(cx).services.shell.clone();
        let (shown, with_log, mask) = text_to_send(cx);
        let (tx, rx) = flume::bounded::<Result<String, ReportError>>(1);
        crate::explorer::spawn_thread("report-copy", move || {
            let text = shown.unwrap_or_else(|| prepare(&settings, with_log, mask));
            let result = shell
                .set_clipboard_text(&text)
                .map(|()| t!("report.copied").into_owned())
                .map_err(ReportError::from);
            let _ = tx.send(result);
        });
        cx.spawn(async move |_, cx| {
            if let Ok(result) = rx.recv_async().await {
                cx.update(|cx| {
                    let ok = result.is_ok();
                    cx.set_global(ReportNote(Some(result.map_err(|e| e.to_string()))));
                    if ok && let Some(url) = &then_open {
                        open_url(url, cx);
                    }
                    cx.refresh_windows();
                });
            }
        })
        .detach();
    }

    /// Saves a timestamped report `.txt` to Downloads and selects it in the file list.
    fn save_report(&mut self, cx: &mut Context<Self>) {
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let explorer = self.active_view().read(cx);
        let shell = explorer.services.shell.clone();
        let folder = explorer
            .places
            .iter()
            .find(|p| {
                matches!(
                    p.kind,
                    crate::shell::PlaceKind::Folder(crate::shell::KnownFolder::Downloads)
                )
            })
            .map(|p| p.path.clone());
        let (shown, with_log, mask) = text_to_send(cx);
        let (tx, rx) = flume::bounded::<Result<String, ReportError>>(1);
        crate::explorer::spawn_thread("report-save", move || {
            let result = (|| {
                let folder = folder.ok_or(ReportError::NoFolder)?;
                let stamp = shell
                    .local_parts(std::time::SystemTime::now())
                    .map_or_else(String::new, |[y, m, d, h, mi, _]| {
                        format!(" {y}-{m:02}-{d:02} {h:02}{mi:02}")
                    });
                let path = folder.join(format!("{}{stamp}.txt", t!("report.file_stem")));
                let text = shown.unwrap_or_else(|| prepare(&settings, with_log, mask));
                std::fs::write(&path, text).map_err(ReportError::Write)?;
                let _ = shell.reveal_in_explorer(&path, true);
                Ok(t!("report.saved", path = path.display().to_string()).into_owned())
            })();
            let _ = tx.send(result);
        });
        cx.spawn(async move |_, cx| {
            if let Ok(result) = rx.recv_async().await {
                cx.update(|cx| {
                    cx.set_global(ReportNote(Some(result.map_err(|e| e.to_string()))));
                    cx.refresh_windows();
                });
            }
        })
        .detach();
    }
}

// The card reads top to bottom: what is sent, the options, the exact text, the actions.
#[allow(clippy::too_many_lines)]
pub(super) fn report_card(open: bool, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let repo = build_info::RELEASES_REPO;
    let note = cx.try_global::<ReportNote>().cloned().unwrap_or_default().0;
    let mut actions = div().flex().flex_wrap().gap(theme::S_8);
    if !repo.is_empty() {
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let url = issue_url(repo, &settings);
        let workspace = cx.entity().downgrade();
        actions = actions.child(button(
            "report-github",
            t!("report.github"),
            true,
            move |_, _, cx: &mut App| {
                let url = url.clone();
                let _ = workspace.update(cx, |ws, cx| ws.copy_report(Some(url), cx));
            },
        ));
    }
    let workspace = cx.entity().downgrade();
    actions = actions.child(button(
        "report-copy",
        t!("report.copy"),
        repo.is_empty(),
        move |_, _, cx: &mut App| {
            let _ = workspace.update(cx, |ws, cx| ws.copy_report(None, cx));
        },
    ));
    let workspace = cx.entity().downgrade();
    actions = actions.child(button(
        "report-save",
        t!("report.save"),
        false,
        move |_, _, cx: &mut App| {
            let _ = workspace.update(cx, Workspace::save_report);
        },
    ));
    let d = draft(cx);
    // Prepared when the card is first opened; Refresh picks up newer log lines.
    if open && d.text.is_none() && !d.loading {
        cx.defer(refresh_draft);
    }
    let options = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(theme::S_16)
        .child(check_row(
            "report-mask",
            t!("report.mask_names"),
            d.mask_names,
            |cx| {
                let mut d = draft(cx);
                d.mask_names = !d.mask_names;
                cx.set_global(d);
                refresh_draft(cx);
            },
        ))
        .child(check_row(
            "report-log",
            t!("report.include_log"),
            d.include_log,
            |cx| {
                let mut d = draft(cx);
                d.include_log = !d.include_log;
                cx.set_global(d);
                refresh_draft(cx);
            },
        ))
        .child(div().flex_1())
        .child(button(
            "report-refresh",
            t!("report.refresh"),
            false,
            |_, _, cx: &mut App| refresh_draft(cx),
        ));
    let preview = d
        .text
        .clone()
        .filter(|_| !d.loading)
        .unwrap_or_else(|| t!("report.preview_loading").into_owned());
    let body = div()
        .flex()
        .flex_col()
        .gap(theme::S_12)
        .child(
            div()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(t!("report.what").to_string()),
        )
        .child(options)
        .child(
            div()
                .id("report-preview")
                .max_h(px(280.))
                .overflow_y_scroll()
                .px(theme::S_12)
                .py(theme::S_8)
                .rounded(theme::R_8)
                .bg(theme::wash())
                .font_family("Cascadia Mono")
                .text_size(theme::FS_12)
                .text_color(theme::text_muted())
                .child(SharedString::from(preview)),
        )
        .child(actions)
        .children(note.map(|note| {
            let (icon, color, text) = match note {
                Ok(text) => (Icon::Check, theme::signal(), text),
                Err(e) => (Icon::Close, theme::danger(), e),
            };
            div()
                .flex()
                .items_center()
                .gap(theme::S_8)
                .min_h(px(28.))
                .child(
                    svg()
                        .path(icon.path())
                        .size(theme::ICON_SM)
                        .text_color(color),
                )
                .child(div().text_size(theme::FS_12).child(text))
        }))
        .into_any_element();
    expander(
        "report",
        Icon::Feedback,
        t!("report.title"),
        t!("report.desc"),
        div().into_any_element(),
        open,
        body,
        cx,
    )
}

/// A check box with its label, like the "Remember" box in the network dialog.
pub(super) fn check_row(
    id: &'static str,
    label: std::borrow::Cow<'static, str>,
    on: bool,
    toggle: impl Fn(&mut App) + 'static,
) -> gpui::AnyElement {
    use crate::a11y::A11yExt as _;
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y(gpui::Role::CheckBox, label.to_string())
        .aria_toggled(crate::a11y::toggled(on))
        .flex()
        .items_center()
        .gap(theme::S_8)
        .cursor_pointer()
        .text_size(theme::FS_13)
        .child(
            div()
                .size(theme::ICON)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(theme::R_4)
                .border_1()
                .when(on, |d| {
                    d.bg(theme::signal()).border_color(theme::signal()).child(
                        svg()
                            .path(Icon::Check.path())
                            .size(theme::ICON_SM)
                            .text_color(theme::on_signal()),
                    )
                })
                .when(!on, |d| d.border_color(theme::line_strong())),
        )
        .child(label.into_owned())
        .on_click(move |_, _, cx| toggle(cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{encode, issue_url};

    #[test]
    fn issue_urls_are_encoded_and_short_enough() {
        // Multi-byte UTF-8 is percent-encoded byte by byte.
        assert_eq!(encode("a b/中"), "a%20b%2F%E4%B8%AD");
        let url = issue_url("me/arke-releases", &crate::settings::Settings::default());
        assert!(url.starts_with("https://github.com/me/arke-releases/issues/new?title="));
        assert!(
            url.len() < 8000,
            "GitHub rejects URLs this long: {}",
            url.len()
        );
        assert!(!url.contains(' '));
    }
}
