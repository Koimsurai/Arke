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

use gpui::{App, Context, SharedString, div, prelude::*, svg};
use rust_i18n::t;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use super::Workspace;
use super::settings_panel::expander;
use super::update_page::{button, open_url};
use crate::assets::Icon;
use crate::build_info;
use crate::keynav::KeyStop as _;
use crate::settings::{Settings, SettingsStore};
use crate::theme;

/// Number of recent log lines to include.
const LOG_LINES: usize = 120;

/// Where a report can also be sent, for people without a GitHub account.
const SUPPORT_EMAIL: &str = "service@koimsurai.com";

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
    arke_core::scrub::scrub(&compose(s, with_log, true), &identity(), mask_names)
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
         - view {:?}, sort {:?}{}, folders {:?}, groups {}, per-folder views {}\n\
         - hidden {}, protected {}, extensions {}, thumbnails {} (network {}), folder sizes {}\n\
         - single click {} (point to select {}), check boxes {}, preview {}, search this PC {}\n\
         - keep running {}, start {:?}\n",
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
        s.remember_folder_views,
        s.show_hidden,
        s.show_protected,
        s.show_extensions,
        s.show_thumbnails,
        s.network_thumbnails,
        s.folder_sizes,
        s.single_click,
        s.point_to_select,
        s.show_checkboxes,
        s.preview_pane,
        s.global_search,
        s.keep_running,
        s.start_location,
    )
}

/// "Open folders in Arke" and Win+E: the settings, and who the registry says handles folders and
/// Win+E now (read only). `with_registry` is false on the UI thread (the issue URL).
fn shell_integration(s: &Settings, with_registry: bool) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "- open folders in Arke {}, Win+E too {}, single window {}\n",
        s.open_folders_in_arke, s.win_e_opens_arke, s.single_window
    );
    if with_registry && let Ok(exe) = std::env::current_exe() {
        for line in arke_core::folder_handler::describe(&crate::win_registry::UserRegistry, &exe) {
            out.push_str(&line);
            out.push('\n');
        }
        // Where this is on, Win+E skips the registration and Arke takes it with a hook (#34).
        let _ = writeln!(
            out,
            "- Windows sends Win+E straight to File Explorer: {}",
            crate::win_e_hook::direct_launch_active()
        );
    }
    out
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

/// Full report text. Runs on a background thread, except for the issue URL, which leaves out what
/// needs reading (`with_registry`).
fn compose(s: &Settings, with_log: bool, with_registry: bool) -> String {
    let mut out = format!(
        "## {}\n\n{}\n## {}\n\n{}\n## {}\n\n{}",
        t!("report.environment"),
        environment(),
        t!("report.settings"),
        settings_summary(s),
        t!("report.shell"),
        shell_integration(s, with_registry)
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

/// Everything but letters, digits and `-_.~` is escaped in a URL query value.
const QUERY_VALUE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encodes a URL query value.
fn encode(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, QUERY_VALUE).to_string()
}

/// GitHub turns away longer new-issue URLs.
const MAX_URL: usize = 7_500;

/// New-issue URL on the bug report form, filled in: version, Windows and build in their fields,
/// and in Log the report itself (already scrubbed) with as many of the latest log lines as the URL
/// can hold. The whole report is on the clipboard as well.
pub(super) fn issue_url(repo: &str, report: &str) -> String {
    let windows = crate::win::windows_version()
        .map_or_else(String::new, |(a, b, c)| format!("{a}.{b} (build {c})"));
    let arch = if cfg!(target_arch = "aarch64") {
        "ARM64"
    } else {
        "x64"
    };
    let url = |log: &str| {
        format!(
            "https://github.com/{repo}/issues/new?template=bug_report.yml&title={}&version={}&windows={}&arch={}&log={}",
            encode(&t!("report.default_title")),
            encode(build_info::VERSION),
            encode(&windows),
            arch,
            encode(log)
        )
    };
    // Drop the oldest log lines (right after the log heading and its fence) until it fits; the
    // newest stay.
    let lines: Vec<&str> = report.lines().collect();
    let heading = format!("## {}", t!("report.log"));
    let log_start = lines
        .iter()
        .position(|l| *l == heading)
        .map_or(lines.len(), |i| i + 3);
    let mut skip = 0;
    loop {
        let kept = if skip == 0 {
            report.to_owned()
        } else {
            let mut kept: Vec<&str> = lines[..log_start.min(lines.len())].to_vec();
            kept.push("…");
            kept.extend(&lines[(log_start + skip).min(lines.len())..]);
            kept.join("\n")
        };
        let candidate = url(&kept);
        if candidate.len() <= MAX_URL || log_start + skip >= lines.len() {
            return candidate;
        }
        skip += 5;
    }
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
    /// Copies the full report to the clipboard, then, with `repo`, opens a new issue there filled
    /// in with it ([`issue_url`]).
    fn copy_report(&mut self, repo: Option<&'static str>, cx: &mut Context<Self>) {
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let shell = Arc::clone(&self.active_view().read(cx).services.shell);
        let (shown, with_log, mask) = text_to_send(cx);
        let (tx, rx) = flume::bounded::<(Result<String, ReportError>, Option<String>)>(1);
        crate::explorer::spawn_thread("report-copy", move || {
            let text = shown.unwrap_or_else(|| prepare(&settings, with_log, mask));
            let result = shell
                .set_clipboard_text(&text)
                .map(|()| t!("report.copied").into_owned())
                .map_err(ReportError::from);
            let url = repo.map(|repo| issue_url(repo, &text));
            let _ = tx.send((result, url));
        });
        cx.spawn(async move |_, cx| {
            if let Ok((result, url)) = rx.recv_async().await {
                cx.update(|cx| {
                    let ok = result.is_ok();
                    cx.set_global(ReportNote(Some(result.map_err(|e| e.to_string()))));
                    if ok && let Some(url) = &url {
                        open_url(url, cx);
                    }
                    cx.refresh_windows();
                });
            }
        })
        .detach();
    }

    /// Copies [`SUPPORT_EMAIL`] to the clipboard.
    fn copy_support_email(&mut self, cx: &mut Context<Self>) {
        let shell = Arc::clone(&self.active_view().read(cx).services.shell);
        let (tx, rx) = flume::bounded::<Result<String, ReportError>>(1);
        crate::explorer::spawn_thread("report-email", move || {
            let result = shell
                .set_clipboard_text(SUPPORT_EMAIL)
                .map(|()| t!("report.email_copied").into_owned())
                .map_err(ReportError::from);
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

    /// Closes Settings and shows the log file, selected, in the active tab.
    fn reveal_log(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(path) = crate::crashlog::log_path() else {
            return;
        };
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        self.close_settings(&super::settings_panel::CloseSettings, window, cx);
        let name = Arc::from(name.to_string_lossy().as_ref());
        let dir = dir.to_path_buf();
        self.active_view().clone().update(cx, |v, cx| {
            v.navigate_selecting(dir, arke_core::NavKind::New, Some(name), cx);
        });
    }

    /// Saves a timestamped report `.txt` to Downloads and selects it in the file list (in Arke,
    /// not File Explorer).
    fn save_report(&mut self, cx: &mut Context<Self>) {
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let explorer = self.active_view().read(cx);
        let shell = Arc::clone(&explorer.services.shell);
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
        let (tx, rx) = flume::bounded::<Result<PathBuf, ReportError>>(1);
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
                Ok(path)
            })();
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = rx.recv_async().await {
                cx.update(|cx| {
                    // The saved report is waiting, selected, in the active tab once Settings
                    // closes; the note says where it went meanwhile.
                    if let Ok(path) = &result
                        && let (Some(dir), Some(name)) = (path.parent(), path.file_name())
                    {
                        let name = Arc::from(name.to_string_lossy().as_ref());
                        let dir = dir.to_path_buf();
                        let _ = this.update(cx, |ws, cx| {
                            ws.active_view().clone().update(cx, |v, cx| {
                                v.navigate_selecting(dir, arke_core::NavKind::New, Some(name), cx);
                            });
                        });
                    }
                    let note = result
                        .map(|path| {
                            t!("report.saved", path = path.display().to_string()).into_owned()
                        })
                        .map_err(|e| e.to_string());
                    cx.set_global(ReportNote(Some(note)));
                    cx.refresh_windows();
                });
            }
        })
        .detach();
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "The card reads top to bottom: what is sent, the options, the exact text, the actions"
)]
pub(super) fn report_card(
    open: bool,
    scroll: &gpui::ScrollHandle,
    scrollbar: &crate::scrollbar::ScrollbarDrag,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let repo = build_info::RELEASES_REPO;
    let note = cx.try_global::<ReportNote>().cloned().unwrap_or_default().0;
    let mut actions = div().flex().flex_wrap().gap(theme::s_8());
    if !repo.is_empty() {
        let workspace = cx.entity().downgrade();
        actions = actions.child(button(
            "report-github",
            t!("report.github"),
            true,
            move |_, _, cx: &mut App| {
                let _ = workspace.update(cx, |ws, cx| ws.copy_report(Some(repo), cx));
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
    // Only when logging to a file (not in a debug build, which logs to the console).
    if crate::crashlog::log_path().is_some() {
        let workspace = cx.entity().downgrade();
        actions = actions.child(button(
            "report-log-file",
            t!("report.open_log"),
            false,
            move |_, window, cx: &mut App| {
                let _ = workspace.update(cx, |ws, cx| ws.reveal_log(window, cx));
            },
        ));
    }
    let workspace = cx.entity().downgrade();
    let email = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(theme::s_8())
        .child(
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(t!("report.email", address = SUPPORT_EMAIL).into_owned()),
        )
        .child(button(
            "report-email-copy",
            t!("menu.copy"),
            false,
            move |_, _, cx: &mut App| {
                let _ = workspace.update(cx, Workspace::copy_support_email);
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
        .gap(theme::s_16())
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
        .gap(theme::s_12())
        .child(
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(t!("report.what").to_string()),
        )
        .child(options)
        .child(crate::scrollbar::nested(
            div()
                .id("report-preview")
                .debug_selector(|| "report-preview".to_owned())
                .max_h(crate::theme::scaled_px(280.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .px(theme::s_12())
                .py(theme::s_8())
                .rounded(theme::r_8())
                .bg(theme::wash())
                .font_family("Cascadia Mono")
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(SharedString::from(preview)),
            scroll,
            Rc::clone(scrollbar),
        ))
        .child(actions)
        .child(email)
        .children(note.map(|note| {
            let (icon, color, text) = match note {
                Ok(text) => (Icon::Check, theme::signal(), text),
                Err(e) => (Icon::Close, theme::danger(), e),
            };
            div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .min_h(crate::theme::scaled_px(28.))
                .child(
                    svg()
                        .path(icon.path())
                        .size(theme::icon_sm())
                        .text_color(color),
                )
                .child(div().text_size(theme::fs_12()).child(text))
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

#[cfg(test)]
impl Workspace {
    /// Shows `text` as the prepared report, so a test needn't wait for the log.
    pub(crate) fn set_report_text_for_test(text: String, cx: &mut App) {
        let mut d = draft(cx);
        // Newer than any preparation still running.
        d.generation += 1;
        d.text = Some(text);
        d.loading = false;
        cx.set_global(d);
        cx.refresh_windows();
    }
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
        .key_stop()
        .aria_toggled(crate::a11y::toggled(on))
        .flex()
        .items_center()
        .gap(theme::s_8())
        .cursor_pointer()
        .text_size(theme::fs_13())
        .child(
            div()
                .size(theme::icon())
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(theme::r_4())
                .border_1()
                .when(on, |d| {
                    d.bg(theme::signal()).border_color(theme::signal()).child(
                        svg()
                            .path(Icon::Check.path())
                            .size(theme::icon_sm())
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
    use super::{MAX_URL, compose, encode, issue_url};
    use rust_i18n::t;

    #[test]
    fn issue_urls_are_encoded_and_short_enough() {
        // Multi-byte UTF-8 is percent-encoded byte by byte.
        assert_eq!(encode("a b/中"), "a%20b%2F%E4%B8%AD");
        let report = compose(&crate::settings::Settings::default(), false, false);
        assert!(
            report.contains(", start Home\n"),
            "the start location (#116)"
        );
        assert!(
            report.contains(", per-folder views true\n"),
            "whether folders remember their views (#124)"
        );
        let url = issue_url("me/arke-releases", &report);
        assert!(url.starts_with(
            "https://github.com/me/arke-releases/issues/new?template=bug_report.yml&title="
        ));
        assert!(url.contains(&format!("&version={}", encode(crate::build_info::VERSION))));
        assert!(url.contains("&arch=x64") || url.contains("&arch=ARM64"));
        assert!(
            url.contains(&format!("&log={}", &encode(&report)[..40])),
            "the report goes in Log"
        );
        assert!(!url.contains(' '));
    }

    /// A long log is cut from the oldest lines: the report's sections and the newest lines stay.
    #[test]
    fn a_long_log_keeps_its_newest_lines() {
        let lines: Vec<String> = (0..400)
            .map(|i| format!("[2026-11-16T00:00:00Z INFO  arke] line number {i} with some words"))
            .collect();
        let report = format!(
            "## Environment\n\n- Arke: x\n\n## {}\n\n```\n{}\n```\n",
            t!("report.log"),
            lines.join("\n")
        );
        let url = issue_url("me/arke", &report);
        assert!(url.len() <= MAX_URL, "{}", url.len());
        assert!(
            url.contains(&encode("line number 399 ")),
            "newest line kept"
        );
        assert!(
            !url.contains(&encode("line number 0 ")),
            "oldest line dropped"
        );
        assert!(url.contains(&encode("## Environment")), "sections kept");
    }
}
