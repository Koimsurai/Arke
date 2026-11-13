//! Home page: a page title, then quick access (the known folders as large cards), drives with
//! capacity, network locations, and the most recent files (Windows' own recent list).
//!
//! Recent files load in the background from the shortcut targets without touching the targets, so
//! network files can't stall it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, ScrollHandle, SharedString, Window, div, prelude::*, px, svg,
};
use rust_i18n::t;

use arke_core::filetype::kind_for_extension;
use arke_core::format::format_size;

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{kind_icon, spawn_thread};
use crate::settings::SettingsStore;
use crate::shell::{KnownFolder, Place, PlaceKind, Shell};
use crate::theme;

/// Recent files listed: enough to find today's work, few enough to stay a list and not a log.
pub(crate) const RECENT_MAX: usize = 10;
/// Content column width; centered on wide windows like Settings.
const PAGE_MAX_W: gpui::Pixels = px(1100.);
/// Quick access card size.
const QUICK_W: gpui::Pixels = px(132.);
const QUICK_H: gpui::Pixels = px(88.);
/// Drive and network card size.
const PLACE_W: gpui::Pixels = px(260.);
const PLACE_H: gpui::Pixels = px(72.);
/// Large icons on the cards.
const CARD_ICON: gpui::Pixels = px(32.);
/// Drive usage bar thickness.
const USAGE_H: gpui::Pixels = px(6.);
/// Above this fraction used, the usage bar turns to the danger color (same as the sidebar).
const NEARLY_FULL: f32 = 0.9;
/// Recent file row height (name over folder).
const RECENT_ROW_H: gpui::Pixels = px(48.);
/// Known folders on the quick access row, in this order.
const QUICK_FOLDERS: [KnownFolder; 6] = [
    KnownFolder::Desktop,
    KnownFolder::Downloads,
    KnownFolder::Documents,
    KnownFolder::Pictures,
    KnownFolder::Music,
    KnownFolder::Videos,
];

/// One recent file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecentFile {
    pub(crate) path: PathBuf,
    /// Last used (time of its shortcut in the recent list)
    pub(crate) used: SystemTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeEvent {
    /// Open the folder in the current tab (drive, network location)
    OpenFolder(PathBuf),
    /// Open the folder in a new background tab (middle click); Home stays.
    OpenInNewTab(PathBuf),
    /// Go to the file's folder and select it
    Reveal(PathBuf),
    /// Open the Recycle Bin page (listed here only while it is unpinned from the sidebar).
    OpenRecycle,
}

pub(crate) struct HomePage {
    shell: Arc<dyn Shell>,
    /// `None` while loading
    recent: Option<Vec<RecentFile>>,
    places: Vec<Place>,
    /// Recently opened shares (`\\server\share`)
    shares: Vec<PathBuf>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl EventEmitter<HomeEvent> for HomePage {}

impl Focusable for HomePage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// How long ago something was used, for the recent list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ago {
    JustNow,
    Minutes(usize),
    Hours(usize),
    Yesterday,
    Days(usize),
    /// A week or more (or in the future, clock skew): show the date.
    Date,
}

/// `then` relative to `now`. "Yesterday" means 24 to 48 hours, not the calendar day, so it needs no
/// time zone.
pub(crate) fn ago(now: SystemTime, then: SystemTime) -> Ago {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let Ok(age) = now.duration_since(then) else {
        return Ago::Date;
    };
    let secs = age.as_secs();
    let count = |unit: u64| usize::try_from(secs / unit).unwrap_or(usize::MAX);
    match secs {
        s if s < MINUTE => Ago::JustNow,
        s if s < HOUR => Ago::Minutes(count(MINUTE)),
        s if s < DAY => Ago::Hours(count(HOUR)),
        s if s < 2 * DAY => Ago::Yesterday,
        s if s < 7 * DAY => Ago::Days(count(DAY)),
        _ => Ago::Date,
    }
}

/// Short folder for a recent file: the closest known place's name plus at most the last two
/// folders under it ("Pictures › Screenshots"); otherwise the drive or share with the last two
/// folders ("C:\ › … › garden-planner › docs").
pub(crate) fn short_folder(folder: &Path, places: &[Place]) -> String {
    const SEP: &str = " › ";
    let tail = |rest: Vec<String>| -> Vec<String> {
        if rest.len() > 2 {
            let mut kept = vec!["…".to_owned()];
            kept.extend(rest[rest.len() - 2..].iter().cloned());
            kept
        } else {
            rest
        }
    };
    let names = |path: &Path| -> Vec<String> {
        path.components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    // The deepest place that contains the folder (Pictures before the profile, the profile
    // before C:\).
    let best = places
        .iter()
        .filter(|p| folder.starts_with(&p.path))
        .max_by_key(|p| p.path.components().count());
    if let Some(place) = best {
        let rest = folder.strip_prefix(&place.path).map_or_default(names);
        let mut parts = vec![place.label.clone()];
        parts.extend(tail(rest));
        return parts.join(SEP);
    }
    let mut root: String = folder
        .components()
        .take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // `\\server\share` reads better without the trailing backslash; `D:\` keeps it.
    if root.starts_with(r"\\") {
        root.truncate(root.trim_end_matches('\\').len());
    }
    let mut parts = Vec::new();
    if !root.is_empty() {
        parts.push(root);
    }
    parts.extend(tail(names(folder)));
    parts.join(SEP)
}

impl HomePage {
    pub(crate) fn new(
        shell: Arc<dyn Shell>,
        places: Vec<Place>,
        shares: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            shell,
            recent: None,
            places,
            shares,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        };
        this.reload(cx);
        this
    }

    /// Reload recent files (when the home page opens or Arke regains focus).
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let shell = self.shell.clone();
        let (tx, rx) = flume::bounded::<Vec<RecentFile>>(1);
        spawn_thread("home-recent", move || {
            let _ = tx.send(shell.recent_files(RECENT_MAX));
        });
        cx.spawn(async move |this, cx| {
            if let Ok(mut recent) = rx.recv_async().await {
                recent.truncate(RECENT_MAX);
                let _ = this.update(cx, |this, cx| {
                    this.recent = Some(recent);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Drives or network locations changed (plugged, connected): swap in the new list.
    pub(crate) fn set_locations(&mut self, places: Vec<Place>, shares: Vec<PathBuf>) {
        self.places = places;
        self.shares = shares;
    }

    #[cfg(test)]
    pub(crate) fn recent_for_test(&self) -> Option<Vec<PathBuf>> {
        self.recent
            .as_ref()
            .map(|r| r.iter().map(|f| f.path.clone()).collect())
    }

    fn open_file(&self, path: PathBuf, cx: &mut App) {
        crate::search_history::SearchHistory::record(&path, false, cx);
        let shell = self.shell.clone();
        spawn_thread("home-open", move || {
            if let Err(err) = shell.open(&path) {
                log::info!("home cannot open {}: {err}", path.display());
            }
        });
    }

    /// Section heading, the same for every section.
    fn section(title: impl Into<SharedString>) -> impl IntoElement {
        div()
            .pt(theme::S_24)
            .pb(theme::S_8)
            .px(theme::S_4)
            .text_size(theme::FS_15)
            .font_weight(theme::FW_STRONG)
            .child(title.into())
    }

    fn time_label(&self, used: SystemTime) -> String {
        match ago(SystemTime::now(), used) {
            Ago::JustNow => t!("home.just_now").into_owned(),
            Ago::Minutes(n) => tn!("home.minutes_ago", n).into_owned(),
            Ago::Hours(n) => tn!("home.hours_ago", n).into_owned(),
            Ago::Yesterday => t!("home.yesterday").into_owned(),
            Ago::Days(n) => tn!("home.days_ago", n).into_owned(),
            Ago::Date => self.shell.format_time_short(used),
        }
    }

    /// Recent files: icon, name over a short folder, how long ago, and a reveal button on hover.
    // Row layout kept together for easy comparison.
    #[allow(clippy::too_many_lines)]
    fn render_recent(&self, cx: &mut Context<Self>) -> AnyElement {
        let note = |text: String| {
            div()
                .px(theme::S_4)
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(text)
                .into_any_element()
        };
        let Some(recent) = &self.recent else {
            return note(t!("home.loading").into_owned());
        };
        if recent.is_empty() {
            return note(t!("home.no_recent").into_owned());
        }
        let rows = recent.iter().enumerate().map(|(ix, file)| {
            let name = file
                .path
                .file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned());
            let folder = file
                .path
                .parent()
                .map_or_default(|p| short_folder(p, &self.places));
            let ext = file
                .path
                .extension()
                .map_or_default(|e| e.to_string_lossy().to_lowercase());
            let open = file.path.clone();
            let reveal = file.path.clone();
            let parent = file.path.parent().map(Path::to_path_buf);
            let group = SharedString::from(format!("home-recent-{ix}"));
            div()
                .id(("home-recent", ix))
                .a11y_lazy(gpui::Role::Link, || {
                    crate::a11y::item_label(&name, &[folder.as_str()])
                })
                .group(group.clone())
                .h(RECENT_ROW_H)
                .px(theme::S_12)
                .flex()
                .items_center()
                .gap(theme::S_12)
                .when(ix > 0, |d| d.border_t_1().border_color(theme::line()))
                .cursor_pointer()
                .hover(|s| s.bg(theme::wash()))
                .child(kind_icon(kind_for_extension(&ext), theme::ICON))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().child(name))
                        .child(
                            div()
                                .truncate()
                                .text_size(theme::FS_12)
                                .text_color(theme::text_subtle())
                                .child(folder),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme::text_subtle())
                        .text_size(theme::FS_12)
                        .child(self.time_label(file.used)),
                )
                // Shown on hover: go to the file's folder.
                .child(
                    div()
                        .id(("home-reveal", ix))
                        .a11y_button(t!("home.reveal"))
                        .size(theme::H_CTL_SM)
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(theme::R_8)
                        .invisible()
                        .group_hover(group, gpui::Styled::visible)
                        .hover(|s| s.bg(theme::wash_strong()))
                        .child(
                            svg()
                                .path(Icon::Folder.path())
                                .size(theme::ICON_SM)
                                .text_color(theme::text_muted()),
                        )
                        .tooltip(crate::explorer::tooltip(t!("home.reveal").into_owned()))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            cx.emit(HomeEvent::Reveal(reveal.clone()));
                        })),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.open_file(open.clone(), cx);
                }))
                // Middle click opens the file's folder in a new tab.
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                        if let Some(parent) = parent.clone() {
                            cx.emit(HomeEvent::OpenInNewTab(parent));
                        }
                    }),
                )
                .into_any_element()
        });
        div()
            .flex()
            .flex_col()
            .rounded(theme::R_8)
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .overflow_hidden()
            .children(rows)
            .into_any_element()
    }

    /// Click opens, middle click opens in a new tab (when there's a folder to open).
    fn card_actions(
        el: gpui::Stateful<gpui::Div>,
        event: HomeEvent,
        new_tab: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            cx.emit(event.clone());
        }))
        .when_some(new_tab, |el, path| {
            el.on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                    cx.emit(HomeEvent::OpenInNewTab(path.clone()));
                }),
            )
        })
    }

    /// Quick access: a known folder as a large card, icon over name.
    fn quick_card(ix: usize, place: &Place, cx: &mut Context<Self>) -> AnyElement {
        let card = div()
            .id(("home-quick", ix))
            .a11y_lazy(gpui::Role::Link, || place.label.clone())
            .w(QUICK_W)
            .h(QUICK_H)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::S_8)
            .rounded(theme::R_8)
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(crate::assets::tinted(
                crate::workspace::place_icon(place.kind),
                CARD_ICON,
                theme::text_muted(),
            ))
            .child(
                div()
                    .max_w_full()
                    .px(theme::S_8)
                    .truncate()
                    .text_size(theme::FS_13)
                    .child(place.label.clone()),
            );
        Self::card_actions(
            card,
            HomeEvent::OpenFolder(place.path.clone()),
            Some(place.path.clone()),
            cx,
        )
        .into_any_element()
    }

    /// Drive, network location or Recycle Bin card: large icon, name, and for drives a usage bar
    /// with free space.
    fn place_card(
        id: (&'static str, usize),
        icon: Icon,
        label: String,
        detail: Option<String>,
        space: Option<(u64, u64)>,
        event: HomeEvent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let new_tab = match &event {
            HomeEvent::OpenFolder(path) => Some(path.clone()),
            _ => None,
        };
        let detail = space
            .map(|(free, total)| {
                t!(
                    "home.space",
                    free = format_size(free),
                    total = format_size(total)
                )
                .into_owned()
            })
            .or(detail);
        let used = space.map(|(free, total)| {
            if total == 0 {
                0.
            } else {
                1. - free as f32 / total as f32
            }
        });
        let card = div()
            .id(id)
            .a11y_lazy(gpui::Role::Link, || {
                crate::a11y::item_label(&label, &[detail.as_deref().unwrap_or_default()])
            })
            .w(PLACE_W)
            .h(PLACE_H)
            .px(theme::S_16)
            .flex()
            .items_center()
            .gap(theme::S_12)
            .rounded(theme::R_8)
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(crate::assets::tinted(icon, CARD_ICON, theme::text_muted()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(theme::S_4)
                    .child(div().truncate().child(label))
                    .children(used.map(|used| {
                        let color = if used > NEARLY_FULL {
                            theme::danger()
                        } else {
                            theme::signal()
                        };
                        div()
                            .h(USAGE_H)
                            .w_full()
                            .rounded(theme::R_FULL)
                            .bg(theme::wash_strong())
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(used.clamp(0., 1.)))
                                    .rounded(theme::R_FULL)
                                    .bg(color),
                            )
                    }))
                    .children(detail.map(|d| {
                        div()
                            .truncate()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle())
                            .child(d)
                    })),
            );
        Self::card_actions(card, event, new_tab, cx).into_any_element()
    }
}

impl Render for HomePage {
    // Sections in reading order: title, quick access, drives, network, recent.
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let known: Vec<Place> = QUICK_FOLDERS
            .iter()
            .filter_map(|kf| {
                self.places
                    .iter()
                    .find(|p| p.kind == PlaceKind::Folder(*kf))
                    .cloned()
            })
            .collect();
        let quick: Vec<AnyElement> = known
            .iter()
            .enumerate()
            .map(|(ix, place)| Self::quick_card(ix, place, cx))
            .collect();
        let mut drives: Vec<AnyElement> = self
            .places
            .clone()
            .into_iter()
            .filter_map(|p| match p.kind {
                PlaceKind::Drive {
                    remote: false,
                    space,
                } => Some((p, space)),
                _ => None,
            })
            .enumerate()
            .map(|(ix, (p, space))| {
                Self::place_card(
                    ("home-drive", ix),
                    Icon::Drive,
                    p.label,
                    None,
                    space,
                    HomeEvent::OpenFolder(p.path),
                    cx,
                )
            })
            .collect();
        if !SettingsStore::global(cx).read(cx).settings.recycle_pinned {
            drives.push(Self::place_card(
                ("home-recycle", 0),
                Icon::Trash,
                t!("recycle.title").into_owned(),
                None,
                None,
                HomeEvent::OpenRecycle,
                cx,
            ));
        }
        // Network: mapped drives and recently opened shares
        let mut net: Vec<(String, PathBuf)> = self
            .places
            .iter()
            .filter(|p| matches!(p.kind, PlaceKind::Drive { remote: true, .. }))
            .map(|p| (p.label.clone(), p.path.clone()))
            .collect();
        for share in &self.shares {
            if !net.iter().any(|(_, p)| p == share) {
                net.push((share.display().to_string(), share.clone()));
            }
        }
        let net: Vec<AnyElement> = net
            .into_iter()
            .enumerate()
            .map(|(ix, (label, path))| {
                // The path under the name, unless the name already is the path.
                let detail =
                    (label != path.display().to_string()).then(|| path.display().to_string());
                Self::place_card(
                    ("home-net", ix),
                    Icon::NetDrive,
                    label,
                    detail,
                    None,
                    HomeEvent::OpenFolder(path),
                    cx,
                )
            })
            .collect();
        let cards =
            |items: Vec<AnyElement>| div().flex().flex_wrap().gap(theme::S_12).children(items);
        let has_quick = !quick.is_empty();
        let has_net = !net.is_empty();
        div()
            .id("home-scroll")
            .key_context("HomePage")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .w_full()
                    .px(theme::S_32)
                    .pt(theme::S_24)
                    .pb(theme::S_32)
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(PAGE_MAX_W)
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px(theme::S_4)
                                    .text_size(theme::FS_24)
                                    .font_weight(theme::FW_STRONG)
                                    .child(t!("home.title").into_owned()),
                            )
                            .when(has_quick, |d| {
                                d.child(Self::section(t!("home.quick_access")))
                                    .child(cards(quick))
                            })
                            .child(Self::section(t!("home.drives")))
                            .child(cards(drives))
                            .when(has_net, |d| {
                                d.child(Self::section(t!("home.network"))).child(cards(net))
                            })
                            .child(Self::section(t!("home.recent")))
                            .child(self.render_recent(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn place(label: &str, path: &str, kind: PlaceKind) -> Place {
        Place {
            label: label.into(),
            path: PathBuf::from(path),
            kind,
        }
    }

    #[test]
    fn recent_times_read_as_how_long_ago() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let before = |secs: u64| now - Duration::from_secs(secs);
        assert_eq!(ago(now, before(5)), Ago::JustNow);
        assert_eq!(ago(now, before(5 * 60 + 30)), Ago::Minutes(5));
        assert_eq!(ago(now, before(3 * 3600)), Ago::Hours(3));
        assert_eq!(ago(now, before(30 * 3600)), Ago::Yesterday);
        assert_eq!(ago(now, before(3 * 86_400)), Ago::Days(3));
        assert_eq!(ago(now, before(8 * 86_400)), Ago::Date);
        // Clock skew: a time in the future shows the date instead of "just now".
        assert_eq!(ago(now, now + Duration::from_secs(60)), Ago::Date);
    }

    #[test]
    fn recent_folders_are_shortened_to_places() {
        let places = vec![
            place(
                "Me",
                r"C:\Users\me",
                PlaceKind::Folder(KnownFolder::Profile),
            ),
            place(
                "Pictures",
                r"C:\Users\me\Pictures",
                PlaceKind::Folder(KnownFolder::Pictures),
            ),
            place(
                "Local Disk (C:)",
                r"C:\",
                PlaceKind::Drive {
                    remote: false,
                    space: None,
                },
            ),
        ];
        let short = |p: &str| short_folder(Path::new(p), &places);
        // The deepest place wins.
        assert_eq!(short(r"C:\Users\me\Pictures"), "Pictures");
        assert_eq!(
            short(r"C:\Users\me\Pictures\Screenshots"),
            "Pictures › Screenshots"
        );
        assert_eq!(
            short(r"C:\Users\me\Projects\garden-planner\docs"),
            "Me › … › garden-planner › docs"
        );
        assert_eq!(short(r"C:\tools"), "Local Disk (C:) › tools");
        // No place: the share root and the last two folders.
        assert_eq!(
            short(r"\\nas\team\Weekly\2026\Meeting"),
            r"\\nas\team › … › 2026 › Meeting"
        );
        assert_eq!(short(r"D:\a"), r"D:\ › a");
    }
}
