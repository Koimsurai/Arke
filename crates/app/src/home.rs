//! Home page: a page title, then the favorites (the sidebar's, as large cards), drives with
//! capacity, network locations, and the most recent files (Windows' own recent list).
//!
//! Recent files load in the background from the shortcut targets without touching the targets, so
//! network files can't stall it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, Pixels, Point, ScrollHandle, SharedString, Window, div,
    prelude::*, px, svg,
};
use rust_i18n::t;

use arke_core::filetype::kind_for_extension;
use arke_core::format::format_size;

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{kind_icon, spawn_thread};
use crate::settings::SettingsStore;
use crate::shell::{Place, PlaceKind, Shell};
use crate::text_input::TextInput;
use crate::theme::{self, UiFont as _};

/// Recent files listed: enough to find today's work, few enough to stay a list and not a log.
pub(crate) const RECENT_MAX: usize = 10;
/// Content column width; centered on wide windows like Settings.
fn page_max_w() -> gpui::Pixels {
    crate::theme::scaled_px(1000.)
}
/// Quick access card size.
fn quick_w() -> gpui::Pixels {
    crate::theme::scaled_px(132.)
}
fn quick_h() -> gpui::Pixels {
    crate::theme::scaled_px(88.)
}
/// Drive and network card size.
fn place_w() -> gpui::Pixels {
    crate::theme::scaled_px(260.)
}
fn place_h() -> gpui::Pixels {
    crate::theme::scaled_px(72.)
}
/// Large icons on the cards.
fn card_icon() -> gpui::Pixels {
    crate::theme::scaled_px(32.)
}
/// Drive usage bar thickness.
fn usage_h() -> gpui::Pixels {
    crate::theme::scaled_px(6.)
}
/// Above this fraction used, the usage bar turns to the danger color (same as the sidebar).
const NEARLY_FULL: f32 = 0.9;
/// Recent file row height (name over folder).
fn recent_row_h() -> gpui::Pixels {
    crate::theme::scaled_px(48.)
}
/// A favorite as the sidebar shows it: folder, its name (a name of its own first) and icon.
pub(crate) type Favorite = (PathBuf, String, Icon);

/// Whether a click on a recent file opens it: always with "Single click to open", otherwise
/// on the second click of a double click.
const fn recent_click_opens(single_click: bool, click_count: usize) -> bool {
    single_click || click_count >= 2
}

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
    /// Open the folder in a new tab (middle click or Ctrl+click), in the background unless
    /// `activate` (Ctrl+Shift+click); Home stays then.
    OpenInNewTab { path: PathBuf, activate: bool },
    /// Go to the file's folder and select it
    Reveal(PathBuf),
    /// Open the Recycle Bin page (listed here only while it is unpinned from the sidebar).
    OpenRecycle,
    /// Right click on a drive or network card: the workspace opens its menu, as in the sidebar.
    PlaceMenu {
        path: PathBuf,
        position: gpui::Point<gpui::Pixels>,
    },
    /// Right click on a favorite: the same menu as its tile in the sidebar.
    FavoriteMenu {
        path: PathBuf,
        position: gpui::Point<gpui::Pixels>,
    },
    /// Right click on a recent file: open it, go to it, copy its path, its properties.
    FileMenu {
        path: PathBuf,
        position: gpui::Point<gpui::Pixels>,
    },
    /// A favorite card dragged over another: the favorite goes to slot `to` of the favorites, as
    /// a tile dragged in the sidebar (Home shows the same favorites).
    MoveFavorite { path: PathBuf, to: usize },
}

/// How much bigger a dragged card is drawn under the pointer: lifted off the grid, as the
/// sidebar's tiles.
const CARD_LIFT: f32 = 1.1;

/// A favorite card being dragged on Home to reorder the favorites.
#[derive(Clone)]
pub(crate) struct DraggedCard {
    path: PathBuf,
    label: String,
    icon: Icon,
}

/// What follows the pointer: the card itself, a little bigger, with a soft shadow, held where it
/// was taken hold of.
struct CardPreview {
    card: DraggedCard,
    grab: Point<Pixels>,
}

impl Render for CardPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let lift = CARD_LIFT;
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::s_8() * lift)
            .rounded(theme::r_8() * lift)
            .bg(theme::frame_solid())
            .border_1()
            .border_color(theme::line())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::fs_13() * lift)
            .text_color(theme::text())
            .child(crate::assets::tinted(
                self.card.icon,
                card_icon() * lift,
                theme::text_muted(),
            ))
            .child(
                div()
                    .max_w_full()
                    .px(theme::s_8() * lift)
                    .truncate()
                    .child(self.card.label.clone()),
            );
        div().relative().child(
            div()
                .absolute()
                .left(-(self.grab.x * (lift - 1.)))
                .top(-(self.grab.y * (lift - 1.)))
                .w(quick_w() * lift)
                .h(quick_h() * lift)
                .child(body),
        )
    }
}

/// Offset each card slides back from after a reorder: a card now in cell j came from cell i.
fn card_offsets(
    cells: &[(f32, f32)],
    old: &[PathBuf],
    new: &[PathBuf],
) -> Vec<(PathBuf, (f32, f32))> {
    new.iter()
        .zip(cells)
        .filter_map(|(item, &(x, y))| {
            let was = old.iter().position(|o| o == item)?;
            let &(ox, oy) = cells.get(was)?;
            let delta = (ox - x, oy - y);
            (delta.0.abs() > 0.5 || delta.1.abs() > 0.5).then(|| (item.clone(), delta))
        })
        .collect()
}

/// The name on a drive or network card: text, or the box it is being renamed in.
enum CardName {
    Text(String),
    Editing(Entity<TextInput>),
}

fn card_label(label: CardName) -> AnyElement {
    match label {
        CardName::Text(text) => div()
            .debug_selector({
                let text = text.clone();
                move || format!("home-card {text}")
            })
            .child(crate::place_names::drive_name(text))
            .into_any_element(),
        CardName::Editing(input) => crate::place_names::rename_box(input),
    }
}

pub(crate) struct HomePage {
    shell: Arc<dyn Shell>,
    /// `None` while loading
    recent: Option<Vec<RecentFile>>,
    /// File Explorer's "Show recently used files" is on; off hides the recent section.
    show_recent: bool,
    places: Vec<Place>,
    /// The sidebar's favorites, in its order with its names (edited there, not here).
    favorites: Vec<Favorite>,
    /// Recently opened shares (`\\server\share`)
    shares: Vec<PathBuf>,
    /// Names given to mapped drives and network places (settings).
    names: crate::place_names::Names,
    /// The place being renamed here and its box.
    rename: Option<(PathBuf, Entity<TextInput>)>,
    /// The recent file picked with one click (a double click opens it, or one click with
    /// "Single click to open").
    selected_recent: Option<PathBuf>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// The favorite card being dragged: its cell stays empty while the card is under the pointer.
    dragging: Option<PathBuf>,
    /// Where each favorite card's cell is, as last reported while dragging.
    card_cells: std::collections::HashMap<usize, Bounds<Pixels>>,
    /// Cards sliding into their new cell after a reorder.
    card_shift: std::collections::HashMap<PathBuf, crate::motion::Slide<(f32, f32)>>,
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
        favorites: Vec<Favorite>,
        shares: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            shell,
            recent: None,
            show_recent: true,
            places,
            favorites,
            shares,
            names: crate::place_names::Names::new(),
            rename: None,
            selected_recent: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            dragging: None,
            card_cells: std::collections::HashMap::new(),
            card_shift: std::collections::HashMap::new(),
        };
        this.reload(cx);
        this
    }

    /// Reload recent files (when the home page opens or Arke regains focus).
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let shell = Arc::clone(&self.shell);
        let (tx, rx) = flume::bounded::<(bool, Vec<RecentFile>)>(1);
        spawn_thread("home-recent", move || {
            // Off in Windows: the list isn't even read.
            let enabled = shell.recent_files_enabled();
            let files = if enabled {
                shell.recent_files(RECENT_MAX)
            } else {
                Vec::new()
            };
            let _ = tx.send((enabled, files));
        });
        cx.spawn(async move |this, cx| {
            if let Ok((enabled, mut recent)) = rx.recv_async().await {
                recent.truncate(RECENT_MAX);
                let _ = this.update(cx, |this, cx| {
                    this.show_recent = enabled;
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

    /// The sidebar's favorites changed (reordered, renamed, added, removed).
    pub(crate) fn set_favorites(&mut self, favorites: Vec<Favorite>) {
        self.favorites = favorites;
    }

    /// The favorites the page lists, as `(path, name)` (tests).
    #[cfg(test)]
    pub(crate) fn favorites_for_test(&self) -> Vec<(PathBuf, String)> {
        self.favorites
            .iter()
            .map(|(path, name, _)| (path.clone(), name.clone()))
            .collect()
    }

    /// Custom place names changed, or a place is (no longer) being renamed.
    pub(crate) fn set_names(
        &mut self,
        names: crate::place_names::Names,
        rename: Option<(PathBuf, Entity<TextInput>)>,
    ) {
        self.names = names;
        self.rename = rename;
    }

    /// The places with their custom names, for labels.
    fn named_places(&self) -> Vec<Place> {
        self.places
            .iter()
            .map(|p| Place {
                label: crate::place_names::place_label(&self.names, p),
                ..p.clone()
            })
            .collect()
    }

    /// The box the place is being renamed in, when it is this one.
    fn card_name(&self, path: &Path, label: String) -> CardName {
        match &self.rename {
            Some((renaming, input))
                if crate::place_names::key(renaming) == crate::place_names::key(path) =>
            {
                CardName::Editing(input.clone())
            }
            _ => CardName::Text(label),
        }
    }

    /// Whether the recent files section is drawn.
    #[cfg(test)]
    pub(crate) const fn recent_shown_for_test(&self) -> bool {
        self.show_recent
    }

    #[cfg(test)]
    pub(crate) fn recent_for_test(&self) -> Option<Vec<PathBuf>> {
        self.recent
            .as_ref()
            .map(|r| r.iter().map(|f| f.path.clone()).collect())
    }

    fn open_file(&self, path: PathBuf, cx: &mut App) {
        crate::search_history::SearchHistory::record(&path, false, cx);
        let shell = Arc::clone(&self.shell);
        spawn_thread("home-open", move || {
            if let Err(err) = shell.open(&path, None) {
                log::info!("home cannot open {}: {err}", path.display());
            }
        });
    }

    /// Section heading, the same for every section.
    fn section(title: impl Into<SharedString>) -> impl IntoElement {
        div()
            .pt(theme::s_24())
            .pb(theme::s_8())
            .px(theme::s_4())
            .text_size(theme::fs_15())
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
    #[allow(
        clippy::too_many_lines,
        reason = "Row layout kept together for easy comparison"
    )]
    fn render_recent(&self, cx: &mut Context<Self>) -> AnyElement {
        let note = |text: String| {
            div()
                .px(theme::s_4())
                .text_size(theme::fs_12())
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
                .map_or_default(|p| short_folder(p, &self.named_places()));
            let ext = file
                .path
                .extension()
                .map_or_default(|e| e.to_string_lossy().to_lowercase());
            let open = file.path.clone();
            let reveal = file.path.clone();
            let menu_path = file.path.clone();
            let parent = file.path.parent().map(Path::to_path_buf);
            let group = SharedString::from(format!("home-recent-{ix}"));
            div()
                .id(("home-recent", ix))
                .a11y_lazy(gpui::Role::Link, || {
                    crate::a11y::item_label(&name, &[folder.as_str()])
                })
                .group(group.clone())
                .h(recent_row_h())
                .px(theme::s_12())
                .flex()
                .items_center()
                .gap(theme::s_12())
                .when(ix > 0, |d| d.border_t_1().border_color(theme::line()))
                .cursor_pointer()
                .when(self.selected_recent.as_ref() == Some(&file.path), |d| {
                    d.bg(theme::selection())
                })
                .hover(|s| s.bg(theme::wash()))
                .child(kind_icon(kind_for_extension(&ext), theme::icon()))
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
                                .text_size(theme::fs_12())
                                .text_color(theme::text_subtle())
                                .child(folder),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme::text_subtle())
                        .text_size(theme::fs_12())
                        .child(self.time_label(file.used)),
                )
                // Shown on hover: go to the file's folder.
                .child(
                    div()
                        .id(("home-reveal", ix))
                        .a11y_button(t!("home.reveal"))
                        .size(theme::h_ctl_sm())
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(theme::r_8())
                        .invisible()
                        .group_hover(group, gpui::Styled::visible)
                        .hover(|s| s.bg(theme::wash_strong()))
                        .child(
                            svg()
                                .path(Icon::Folder.path())
                                .size(theme::icon_sm())
                                .text_color(theme::text_muted()),
                        )
                        .tooltip(crate::explorer::tooltip(t!("home.reveal").into_owned()))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            cx.emit(HomeEvent::Reveal(reveal.clone()));
                        })),
                )
                .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    if recent_click_opens(crate::prefs::single_click(), e.click_count()) {
                        this.open_file(open.clone(), cx);
                    } else {
                        this.selected_recent = Some(open.clone());
                        cx.notify();
                    }
                }))
                // Middle click opens the file's folder in a new tab.
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                        if let Some(parent) = parent.clone() {
                            cx.emit(HomeEvent::OpenInNewTab {
                                path: parent,
                                activate: false,
                            });
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |_, e: &MouseDownEvent, _, cx| {
                        cx.emit(HomeEvent::FileMenu {
                            path: menu_path.clone(),
                            position: e.position,
                        });
                    }),
                )
                .into_any_element()
        });
        div()
            .flex()
            .flex_col()
            .rounded(theme::r_8())
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .overflow_hidden()
            .children(rows)
            .into_any_element()
    }

    /// Click opens; middle click or Ctrl+click opens in a new tab (when there's a folder to open).
    fn card_actions(
        el: gpui::Stateful<gpui::Div>,
        event: HomeEvent,
        new_tab: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ctrl_tab = new_tab.clone();
        el.on_click(cx.listener(move |_, e: &ClickEvent, _, cx| {
            // Ctrl: a new tab, in front with Shift too, as a link in a browser (#291).
            match &ctrl_tab {
                Some(path) if e.modifiers().control => cx.emit(HomeEvent::OpenInNewTab {
                    path: path.clone(),
                    activate: e.modifiers().shift,
                }),
                _ => cx.emit(event.clone()),
            }
        }))
        .when_some(new_tab, |el, path| {
            el.on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                    cx.emit(HomeEvent::OpenInNewTab {
                        path: path.clone(),
                        activate: false,
                    });
                }),
            )
        })
    }

    /// A favorite as a large card, icon over name; a long name is cut and shown whole in the tooltip.
    /// A favorite card reports its cell while a card is dragged; the dragged card entering it
    /// takes its place, and the cards in between slide over (#173).
    fn card_drag_move(
        &mut self,
        cell: usize,
        path: &Path,
        e: &DragMoveEvent<DraggedCard>,
        cx: &mut Context<Self>,
    ) {
        self.card_cells.insert(cell, e.bounds);
        let dragged = e.drag(cx).path.clone();
        if dragged == path || !e.bounds.contains(&e.event.position) {
            return;
        }
        let old: Vec<PathBuf> = self.favorites.iter().map(|(p, ..)| p.clone()).collect();
        let (Some(from), Some(to)) = (
            old.iter().position(|p| *p == dragged),
            old.iter().position(|p| p == path),
        ) else {
            return;
        };
        let moving = self.favorites.remove(from);
        self.favorites.insert(to, moving);
        let new: Vec<PathBuf> = self.favorites.iter().map(|(p, ..)| p.clone()).collect();
        let cells: Option<Vec<(f32, f32)>> = (0..old.len())
            .map(|c| {
                self.card_cells
                    .get(&c)
                    .map(|b| (f32::from(b.origin.x), f32::from(b.origin.y)))
            })
            .collect();
        if let Some(cells) = cells.filter(|_| crate::motion::enabled()) {
            let now = std::time::Instant::now();
            for (p, (dx, dy)) in card_offsets(&cells, &old, &new) {
                let (lx, ly) = self
                    .card_shift
                    .get(&p)
                    .filter(|s| s.live(now))
                    .map_or((0., 0.), |s| s.left(now));
                self.card_shift.insert(
                    p,
                    crate::motion::Slide {
                        from: (dx + lx, dy + ly),
                        started: now,
                    },
                );
            }
        }
        cx.emit(HomeEvent::MoveFavorite { path: dragged, to });
        cx.notify();
    }

    fn quick_card(
        &self,
        ix: usize,
        path: &Path,
        label: &str,
        icon: Icon,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dragged = self.dragging.as_deref() == Some(path) && cx.has_active_drag();
        let now = std::time::Instant::now();
        let shift = self
            .card_shift
            .get(path)
            .filter(|s| s.live(now))
            .map(|s| s.left(now));
        let drag = DraggedCard {
            path: path.to_path_buf(),
            label: label.to_owned(),
            icon,
        };
        let home = cx.entity().downgrade();
        let key = path.to_path_buf();
        // Known by its folder, not its place: after a reorder the card under the pointer is
        // another one, and its hover and tooltip must not carry over.
        let card = div()
            .id(SharedString::from(format!("home-quick-{}", path.display())))
            .relative()
            .when_some(shift, |d, (dx, dy)| d.left(px(dx)).top(px(dy)))
            .when(dragged, gpui::Styled::invisible)
            .a11y_lazy(gpui::Role::Link, || label.to_owned())
            .w(quick_w())
            .h(quick_h())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::s_8())
            .rounded(theme::r_8())
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(crate::assets::tinted(
                icon,
                card_icon(),
                theme::text_muted(),
            ))
            .child(
                div()
                    .max_w_full()
                    .px(theme::s_8())
                    .truncate()
                    .text_size(theme::fs_13())
                    .debug_selector({
                        let label = label.to_owned();
                        move || format!("home-favorite {label}")
                    })
                    .child(label.to_owned()),
            )
            .tooltip(crate::explorer::tooltip(label.to_owned()));
        let menu_path = path.to_path_buf();
        let card = Self::card_actions(
            card,
            HomeEvent::OpenFolder(path.to_path_buf()),
            Some(path.to_path_buf()),
            cx,
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |_, e: &MouseDownEvent, _, cx| {
                cx.emit(HomeEvent::FavoriteMenu {
                    path: menu_path.clone(),
                    position: e.position,
                });
            }),
        );
        // The cell holds still while its card slides: drag moves are read against where the
        // cells are, never against a card midway through a slide (which threw reorders off).
        div()
            .id(("home-quick-cell", ix))
            .w(quick_w())
            .h(quick_h())
            .child(card)
            .on_drag(drag, move |card, grab, _, cx| {
                let _ = home.update(cx, |home, cx| {
                    home.dragging = Some(card.path.clone());
                    home.card_cells.clear();
                    cx.notify();
                });
                cx.new(|_| CardPreview {
                    card: card.clone(),
                    grab,
                })
            })
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DraggedCard>, _: &mut Window, cx| {
                    this.card_drag_move(ix, &key, e, cx);
                },
            ))
            .on_drop(cx.listener(|this, _: &DraggedCard, _, cx| {
                this.dragging = None;
                cx.notify();
            }))
            .into_any_element()
    }

    /// Drive, network location or Recycle Bin card: large icon, name, and for drives a usage bar
    /// with free space.
    fn place_card(
        id: (&'static str, usize),
        icon: Icon,
        label: CardName,
        detail: Option<String>,
        space: Option<(u64, u64)>,
        event: HomeEvent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text = match &label {
            CardName::Text(text) => text.clone(),
            CardName::Editing(_) => String::new(),
        };
        let menu_path = match &event {
            HomeEvent::OpenFolder(path) => Some(path.clone()),
            _ => None,
        };
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
                crate::a11y::item_label(&text, &[detail.as_deref().unwrap_or_default()])
            })
            .w(place_w())
            .h(place_h())
            .px(theme::s_16())
            .flex()
            .items_center()
            .gap(theme::s_12())
            .rounded(theme::r_8())
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(crate::assets::tinted(
                icon,
                card_icon(),
                theme::text_muted(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(theme::s_4())
                    .child(card_label(label))
                    .children(used.map(|used| {
                        let color = if used > NEARLY_FULL {
                            theme::danger()
                        } else {
                            theme::signal()
                        };
                        div()
                            .h(usage_h())
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
                            .text_size(theme::fs_12())
                            .text_color(theme::text_subtle())
                            .child(d)
                    })),
            );
        Self::card_actions(card, event, new_tab, cx)
            .when_some(menu_path, |el, path| {
                el.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |_, e: &MouseDownEvent, _, cx| {
                        cx.emit(HomeEvent::PlaceMenu {
                            path: path.clone(),
                            position: e.position,
                        });
                    }),
                )
            })
            .into_any_element()
    }
}

impl Render for HomePage {
    #[allow(
        clippy::too_many_lines,
        reason = "Sections in reading order: title, quick access, drives, network, recent"
    )]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A card drag that ended without a drop on a card (empty space, Esc): forgotten, so a
        // later drag of something else doesn't hide that card.
        if !cx.has_active_drag() {
            self.dragging = None;
        }
        let quick: Vec<AnyElement> = self
            .favorites
            .iter()
            .enumerate()
            .map(|(ix, (path, label, icon))| self.quick_card(ix, path, label, *icon, cx))
            .collect();
        // Cards still sliding into their cells after a reorder.
        let now = std::time::Instant::now();
        self.card_shift.retain(|_, s| s.live(now));
        if !self.card_shift.is_empty() {
            window.request_animation_frame();
        }
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
                    CardName::Text(p.label),
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
                CardName::Text(t!("recycle.title").into_owned()),
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
            .map(|p| {
                (
                    crate::place_names::place_label(&self.names, p),
                    p.path.clone(),
                )
            })
            .collect();
        for share in &self.shares {
            if !net.iter().any(|(_, p)| p == share) {
                let label = crate::place_names::shown_name(&self.names, share)
                    .unwrap_or_else(|| share.display().to_string());
                net.push((label, share.clone()));
            }
        }
        let net: Vec<AnyElement> = net
            .into_iter()
            .enumerate()
            .map(|(ix, (label, path))| {
                // The path under the name, unless the name already is the path, or it is a
                // mapped drive's `Z:\`, whose letter the name already ends with.
                let detail = (label != path.display().to_string()
                    && crate::place_names::drive_letter(&path).is_none())
                .then(|| path.display().to_string());
                Self::place_card(
                    ("home-net", ix),
                    Icon::NetDrive,
                    self.card_name(&path, label),
                    detail,
                    None,
                    HomeEvent::OpenFolder(path),
                    cx,
                )
            })
            .collect();
        let cards =
            |items: Vec<AnyElement>| div().flex().flex_wrap().gap(theme::s_12()).children(items);
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
                    .px(theme::s_32())
                    .pt(theme::s_24())
                    .pb(theme::s_32())
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(page_max_w())
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px(theme::s_4())
                                    .text_size(theme::fs_24())
                                    .font_weight(theme::FW_STRONG)
                                    .child(t!("home.title").into_owned()),
                            )
                            .when(has_quick, |d| {
                                d.child(Self::section(t!("sidebar.favorites")))
                                    .child(cards(quick))
                            })
                            .child(Self::section(t!("home.drives")))
                            .child(cards(drives))
                            .when(has_net, |d| {
                                d.child(Self::section(t!("home.network"))).child(cards(net))
                            })
                            .when(self.show_recent, |d| {
                                d.child(Self::section(t!("home.recent")))
                                    .child(self.render_recent(cx))
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::shell::KnownFolder;

    fn place(label: &str, path: &str, kind: PlaceKind) -> Place {
        Place {
            label: label.into(),
            path: PathBuf::from(path),
            kind,
        }
    }

    #[test]
    fn recent_files_open_by_double_click_unless_single_click_is_on() {
        assert!(!recent_click_opens(false, 1));
        assert!(recent_click_opens(false, 2));
        assert!(recent_click_opens(true, 1));
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
