//! Home page: a page title, then the favorites (the sidebar's, as large cards), drives with
//! capacity, network locations, and the most recent files (Windows' own recent list).
//!
//! Recent files load in the background from the shortcut targets without touching the targets, so
//! network files can't stall it.
//!
//! The keyboard (#322), as on File Explorer's Home: Tab brings it to the page and a ring shows
//! the picked card, or the first one. The arrow keys move between the cards of a section (Up and
//! Down go on into the next section at its edge), Home and End to the first and last card, Enter
//! opens it (whatever the click setting is) and Shift+F10 or the Menu key opens its menu. The card
//! under the ring is reported to screen readers as the focused item.

use std::cell::{Cell, RefCell};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, KeyBinding, MouseButton, MouseDownEvent, Pixels, Point, ScrollHandle,
    SharedString, Stateful, Window, actions, canvas, div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use arke_core::filetype::kind_for_extension;
use arke_core::format::format_size;

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{kind_icon, spawn_thread};
use crate::press_guard::is_double_click;
use crate::settings::{SettingsStore, SidebarSection};
use crate::shell::{Place, PlaceKind, Shell};
use crate::text_input::TextInput;
use crate::theme::{self, UiFont as _};

actions!(
    home,
    [
        /// Left: the card before, in the same section.
        HomeLeft,
        /// Right: the card after, in the same section.
        HomeRight,
        /// Up: the card above.
        HomeUp,
        /// Down: the card below.
        HomeDown,
        /// Home: the first card.
        HomeFirst,
        /// End: the last card.
        HomeLast,
        /// Enter: opens the card.
        HomeOpen,
        /// Shift+F10 or the Menu key: the card's menu.
        HomeMenu
    ]
);

/// The page's key context. Tab and Shift+Tab there are the window's (`workspace_sidebar_keys.rs`).
const CONTEXT: &str = "HomePage";

/// The keys of the cards.
pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", HomeLeft, ctx),
        KeyBinding::new("right", HomeRight, ctx),
        KeyBinding::new("up", HomeUp, ctx),
        KeyBinding::new("down", HomeDown, ctx),
        KeyBinding::new("home", HomeFirst, ctx),
        KeyBinding::new("end", HomeLast, ctx),
        KeyBinding::new("enter", HomeOpen, ctx),
        KeyBinding::new("shift-f10", HomeMenu, ctx),
        KeyBinding::new("menu", HomeMenu, ctx),
    ]);
}

/// A card or recent file the keyboard can be on.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Spot {
    /// A card, by its key (as `HomePage::selected_card` holds it).
    Card(String),
    Recent(PathBuf),
}

/// The part of the page a spot is in: Left and Right stay inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Favorites,
    Drives,
    Network,
    Recent,
}

/// What Enter (or a screen reader's click) does on a spot.
#[derive(Clone)]
enum Open {
    Event(HomeEvent),
    File(PathBuf),
}

/// The menu Shift+F10 opens on a spot: its right click's.
#[derive(Clone, Copy)]
enum MenuOf {
    Favorite,
    Place,
    File,
}

/// A spot drawn this frame.
struct KeySpot {
    spot: Spot,
    section: Section,
    open: Open,
    menu: Option<(MenuOf, PathBuf)>,
    /// Where it was drawn (set while prepainting).
    at: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl KeySpot {
    fn new(spot: Spot, section: Section, open: Open, menu: Option<(MenuOf, PathBuf)>) -> Self {
        Self {
            spot,
            section,
            open,
            menu,
            at: Rc::new(Cell::new(None)),
        }
    }
}

/// The focus ring, as of the last frame drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ring {
    /// Not shown: the page doesn't have the keyboard, or the mouse is in use.
    Off,
    /// Brought up by the key being handled now: the user hasn't seen where it is, so that key
    /// only shows it.
    Fresh,
    /// Shown since an earlier frame (the keyboard came to the page, or an earlier key).
    Shown,
}

/// An arrow key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Left,
    Right,
    Up,
    Down,
}

/// Where the keyboard goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    Step(Step),
    First,
    Last,
}

/// Where an arrow key takes the cursor from spot `at`, given each spot's section and where it
/// was drawn (in drawing order, which is reading order). Left and Right go to the card before or
/// after in the same section (not in the recent files, a single column); Up and Down to the card
/// nearest by its middle in the row above or below, going on into the next section at a
/// section's edge, so every card can be reached. A spot not drawn yet goes by the order.
fn step_to(spots: &[(Section, Option<Bounds<Pixels>>)], at: usize, step: Step) -> Option<usize> {
    let &(section, here) = spots.get(at)?;
    let (down, other) = match step {
        Step::Left | Step::Right => {
            if section == Section::Recent {
                return None;
            }
            let to = if step == Step::Left {
                at.checked_sub(1)?
            } else {
                at + 1
            };
            return (spots.get(to)?.0 == section).then_some(to);
        }
        Step::Up => (false, at.checked_sub(1)),
        Step::Down => (true, Some(at + 1).filter(|&ix| ix < spots.len())),
    };
    let Some(here) = here else {
        return other;
    };
    // How far past this spot's row a spot is, when it is in a row beyond it.
    let past = |b: &Bounds<Pixels>| -> Option<f32> {
        let gap = if down {
            f32::from(b.top()) - f32::from(here.bottom())
        } else {
            f32::from(here.top()) - f32::from(b.bottom())
        };
        (gap > -1.).then_some(gap)
    };
    // The spots drawn in a row beyond this one, with how far past it.
    let beyond: Vec<(usize, Bounds<Pixels>, f32)> = spots
        .iter()
        .enumerate()
        .filter_map(|(ix, &(_, b))| {
            let b = b?;
            Some((ix, b, past(&b)?))
        })
        .collect();
    let nearest = beyond.iter().map(|&(.., gap)| gap).min_by(f32::total_cmp)?;
    let off = |b: &Bounds<Pixels>| (f32::from(b.center().x) - f32::from(here.center().x)).abs();
    beyond
        .iter()
        .filter(|&&(.., gap)| gap - nearest < 1.)
        .min_by(|(_, a, _), (_, b, _)| off(a).total_cmp(&off(b)))
        .map(|&(ix, ..)| ix)
}

/// Scrolls the page just enough to show the card drawn at `card`.
fn reveal_card(scroll: &ScrollHandle, card: Bounds<Pixels>, window: &mut Window) {
    let shift = crate::keynav::shift_to_show(card, scroll.bounds(), theme::s_16());
    if shift == px(0.) {
        return;
    }
    let offset = scroll.offset();
    let y = (offset.y + shift).min(px(0.)).max(-scroll.max_offset().y);
    scroll.set_offset(point(offset.x, y));
    // This runs while the window draws: the next frame shows the page at its new place.
    window.on_next_frame(|window, _| window.refresh());
}

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

/// Whether a click on a recent file or a card opens it: always with "Single click to open",
/// otherwise on the second click of a double click.
const fn recent_click_opens(single_click: bool, click_count: usize) -> bool {
    single_click || is_double_click(click_count)
}

/// A card's background: the picked one wears the accent wash of a selected file row.
fn card_bg(selected: bool) -> gpui::Rgba {
    if selected {
        theme::list_selection(true)
    } else {
        theme::card()
    }
}

/// A drive's usage bar, `used` being the fraction in use.
fn usage_bar(used: f32) -> gpui::Div {
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
}

/// What a card is known by while it is the picked one.
fn card_key(id: (&str, usize)) -> String {
    format!("{}-{}", id.0, id.1)
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
    /// The recent file the last click launched: a later click of the same quick run on it must
    /// not launch it again (single-click mode, where every click opens).
    launched_recent: Option<PathBuf>,
    /// The card (favorite, drive, network location) picked with one click, by its id.
    selected_card: Option<String>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// The favorite card being dragged: its cell stays empty while the card is under the pointer.
    dragging: Option<PathBuf>,
    /// Where each favorite card's cell is, as last reported while dragging.
    card_cells: std::collections::HashMap<usize, Bounds<Pixels>>,
    /// Cards sliding into their new cell after a reorder.
    card_shift: std::collections::HashMap<PathBuf, crate::motion::Slide<(f32, f32)>>,
    /// The cards and recent files drawn this frame, in order; only recorded while the page has
    /// the keyboard or a screen reader is attached.
    spots: RefCell<Vec<KeySpot>>,
    /// The page has the keyboard (as of the last frame).
    focused: bool,
    /// The focus ring: whether it shows (the page has the keyboard and it is in use) and since
    /// when (as of the last frame).
    ring: Ring,
    /// Scroll the cursor's card into view when it is next drawn.
    reveal: Rc<Cell<bool>>,
    /// Shift+Tab brought the keyboard in: pick the first card of the last section once drawn.
    enter_last: bool,
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
            launched_recent: None,
            selected_card: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            dragging: None,
            card_cells: std::collections::HashMap::new(),
            card_shift: std::collections::HashMap::new(),
            spots: RefCell::new(Vec::new()),
            focused: false,
            ring: Ring::Off,
            enter_last: false,
            reveal: Rc::new(Cell::new(false)),
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
            let row = div()
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
                    let launched = this.launched_recent.take();
                    if recent_click_opens(crate::prefs::single_click(), e.click_count()) {
                        // The file is still under the cursor after it launched: the next click
                        // of the run (counts 2, 3...) would launch it again.
                        if e.click_count() > 1 && launched.as_ref() == Some(&open) {
                            this.launched_recent = launched;
                            return;
                        }
                        this.launched_recent = Some(open.clone());
                        this.open_file(open.clone(), cx);
                    } else {
                        this.selected_recent = Some(open.clone());
                        this.selected_card = None;
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
                );
            self.key_spot(
                row,
                || {
                    KeySpot::new(
                        Spot::Recent(file.path.clone()),
                        Section::Recent,
                        Open::File(file.path.clone()),
                        Some((MenuOf::File, file.path.clone())),
                    )
                },
                cx,
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

    /// Click opens (a double click, unless "Single click to open"; the first click of a double
    /// click only picks the card); middle click or Ctrl+click opens in a new tab (when there's a
    /// folder to open).
    fn card_actions(
        el: gpui::Stateful<gpui::Div>,
        key: String,
        event: HomeEvent,
        new_tab: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ctrl_tab = new_tab.clone();
        el.on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
            // Ctrl: a new tab, in front with Shift too, as a link in a browser (#291).
            match &ctrl_tab {
                Some(path) if e.modifiers().control => cx.emit(HomeEvent::OpenInNewTab {
                    path: path.clone(),
                    activate: e.modifiers().shift,
                }),
                _ if recent_click_opens(crate::prefs::single_click(), e.click_count()) => {
                    cx.emit(event.clone());
                }
                _ => {
                    this.selected_card = Some(key.clone());
                    this.selected_recent = None;
                    cx.notify();
                }
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

    /// Whether the card with this id is the one picked with a click.
    fn card_picked(&self, id: (&str, usize)) -> bool {
        self.selected_card.as_deref() == Some(card_key(id).as_str())
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
        let card_id = format!("home-quick-{}", path.display());
        let selected = self.selected_card.as_deref() == Some(card_id.as_str());
        // Known by its folder, not its place: after a reorder the card under the pointer is
        // another one, and its hover and tooltip must not carry over.
        let card = div()
            .id(SharedString::from(card_id.clone()))
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
            .bg(card_bg(selected))
            .border_1()
            .border_color(theme::line())
            .cursor_pointer()
            .when(!selected, |d| d.hover(|s| s.bg(theme::wash())))
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
            card_id.clone(),
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
        let card = self.key_spot(
            card,
            || {
                KeySpot::new(
                    Spot::Card(card_id),
                    Section::Favorites,
                    Open::Event(HomeEvent::OpenFolder(path.to_path_buf())),
                    Some((MenuOf::Favorite, path.to_path_buf())),
                )
            },
            cx,
        );
        Self::quick_cell(ix, card, drag, cx)
    }

    /// The cell a favorite card sits in, which is what is dragged to reorder the favorites.
    fn quick_cell(
        ix: usize,
        card: Stateful<Div>,
        drag: DraggedCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let home = cx.entity().downgrade();
        let key = drag.path.clone();
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
        selected: bool,
        icon: Icon,
        label: CardName,
        detail: Option<String>,
        space: Option<(u64, u64)>,
        event: HomeEvent,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
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
            .bg(card_bg(selected))
            .border_1()
            .border_color(theme::line())
            .cursor_pointer()
            .when(!selected, |d| d.hover(|s| s.bg(theme::wash())))
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
                    .children(used.map(usage_bar))
                    .children(detail.map(|d| {
                        div()
                            .truncate()
                            .text_size(theme::fs_12())
                            .text_color(theme::text_subtle())
                            .child(d)
                    })),
            );
        Self::card_actions(card, card_key(id), event, new_tab, cx).when_some(
            menu_path,
            |el, path| {
                el.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |_, e: &MouseDownEvent, _, cx| {
                        cx.emit(HomeEvent::PlaceMenu {
                            path: path.clone(),
                            position: e.position,
                        });
                    }),
                )
            },
        )
    }
}

/// The keyboard on the page (see the module docs).
impl HomePage {
    /// Whether a card or recent file is picked.
    const fn has_pick(&self) -> bool {
        self.selected_card.is_some() || self.selected_recent.is_some()
    }

    fn is_picked(&self, spot: &Spot) -> bool {
        match spot {
            Spot::Card(key) => self.selected_card.as_ref() == Some(key),
            Spot::Recent(path) => self.selected_recent.as_ref() == Some(path),
        }
    }

    /// Picks `spot`, as a click on it does.
    fn pick(&mut self, spot: &Spot) {
        match spot {
            Spot::Card(key) => {
                self.selected_card = Some(key.clone());
                self.selected_recent = None;
            }
            Spot::Recent(path) => {
                self.selected_recent = Some(path.clone());
                self.selected_card = None;
            }
        }
    }

    /// Gives a card or recent file (`el`) what the keyboard and screen readers need: its place
    /// in the keyboard's order, the focus ring while the cursor is on it, the focused-item
    /// report, and a screen reader's click. `spot` only runs while the page has the keyboard or a
    /// screen reader is attached; otherwise `el` is left as it is.
    fn key_spot(
        &self,
        el: Stateful<Div>,
        spot: impl FnOnce() -> KeySpot,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if !self.focused && !crate::a11y::active() {
            return el;
        }
        let spot = spot();
        // The picked one, or the first while nothing is.
        let cursor = if self.has_pick() {
            self.is_picked(&spot.spot)
        } else {
            self.spots.borrow().is_empty()
        };
        let home = cx.entity().downgrade();
        let target = spot.spot.clone();
        // Not where the card is drawn: one scrolled out of the page couldn't be opened.
        let el = el.a11y_on_click(move |_, cx| {
            let _ = home.update(cx, |home, cx| home.open_spot(&target, cx));
        });
        let at = Rc::clone(&spot.at);
        self.spots.borrow_mut().push(spot);
        if !self.focused {
            return el;
        }
        let reveal = cursor.then(|| (Rc::clone(&self.reveal), self.scroll.clone()));
        el.relative()
            .child(
                canvas(
                    move |bounds, window, _| {
                        at.set(Some(bounds));
                        if let Some((reveal, scroll)) = &reveal
                            && reveal.replace(false)
                        {
                            reveal_card(scroll, bounds, window);
                        }
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when(
                cursor,
                gpui::StatefulInteractiveElement::aria_active_descendant,
            )
            .when(cursor && self.ring != Ring::Off, |d| {
                d.child(
                    div()
                        .debug_selector(|| "home-focus-ring".to_owned())
                        .absolute()
                        .inset_0()
                        .rounded(theme::r_8())
                        .shadow(crate::keynav::ring()),
                )
            })
    }

    /// Opens `spot` (Enter, or a screen reader's click on it).
    fn open_spot(&mut self, spot: &Spot, cx: &mut Context<Self>) {
        let open = self
            .spots
            .borrow()
            .iter()
            .find(|s| s.spot == *spot)
            .map(|s| s.open.clone());
        match open {
            Some(Open::Event(event)) => cx.emit(event),
            Some(Open::File(path)) => self.open_file(path, cx),
            None => {}
        }
    }

    /// The spot the keys act on: the picked one, or the first while the ring shows on it.
    /// `None` while the ring isn't showing and nothing is picked: a key then only brings it up.
    fn acting_ix(&self) -> Option<usize> {
        let spots = self.spots.borrow();
        spots
            .iter()
            .position(|s| self.is_picked(&s.spot))
            .or_else(|| {
                (self.ring == Ring::Shown && !self.has_pick() && !spots.is_empty()).then_some(0)
            })
    }

    fn move_cursor(&mut self, to: Move, cx: &mut Context<Self>) {
        let shapes: Vec<(Section, Option<Bounds<Pixels>>)> = self
            .spots
            .borrow()
            .iter()
            .map(|s| (s.section, s.at.get()))
            .collect();
        let Some(last) = shapes.len().checked_sub(1) else {
            return;
        };
        let to = match (to, self.acting_ix()) {
            (Move::Last, _) => Some(last),
            (Move::Step(step), Some(at)) => step_to(&shapes, at, step),
            // Home, or the ring wasn't showing: the first key shows it, on the first card.
            (Move::First, _) | (Move::Step(_), None) => Some(0),
        };
        let Some(spot) = to.and_then(|ix| self.spots.borrow().get(ix).map(|s| s.spot.clone()))
        else {
            return;
        };
        self.pick(&spot);
        self.reveal.set(true);
        cx.notify();
    }

    /// Tab (`forward`) or Shift+Tab inside the page: the first card of the next or previous
    /// section (favorites, drives, network, recent files). `false` at the last or first section,
    /// where the keyboard goes on to the rest of the window.
    pub(crate) fn tab_section(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        // The ring isn't on screen (the mouse moved after Tab brought the keyboard here): the
        // first card was never seen as the stop, so Tab shows it instead of walking past it.
        // Any UIA client counts, not only screen readers: the ring is what a sighted user goes by.
        if forward && !self.has_pick() && self.ring != Ring::Shown {
            let first = self.spots.borrow().first().map(|s| s.spot.clone());
            if let Some(spot) = first {
                self.pick(&spot);
                self.reveal.set(true);
                cx.notify();
                return true;
            }
        }
        // Nothing picked, ring showing or not: the keyboard is on the first card (the one a
        // screen reader reports as focused), so Tab goes on from there.
        let at = self.acting_ix().unwrap_or(0);
        let to = {
            let spots = self.spots.borrow();
            let Some(here) = spots.get(at).map(|s| s.section) else {
                return false;
            };
            if forward {
                (at..spots.len()).find(|&ix| spots[ix].section != here)
            } else {
                let start = (0..=at)
                    .rev()
                    .take_while(|&ix| spots[ix].section == here)
                    .last();
                let before = start
                    .and_then(|ix| ix.checked_sub(1))
                    .and_then(|ix| spots.get(ix))
                    .map(|s| s.section);
                before.and_then(|before| spots.iter().position(|s| s.section == before))
            }
        };
        let Some(spot) = to.and_then(|ix| self.spots.borrow().get(ix).map(|s| s.spot.clone()))
        else {
            return false;
        };
        self.pick(&spot);
        self.reveal.set(true);
        cx.notify();
        true
    }

    /// Tab (`forward`) or Shift+Tab into the page from the rest of the window: the first card of
    /// its first or last section, so a round visits every section, whatever was picked before.
    /// The cards are only recorded while the page has the keyboard, so Shift+Tab picks the last
    /// section's card in the next frame (`enter_last`); Tab just forgets the pick, which puts the
    /// ring on the first card.
    pub(crate) fn enter(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.selected_card = None;
        self.selected_recent = None;
        self.enter_last = !forward;
        self.reveal.set(true);
        cx.notify();
    }

    fn home_left(&mut self, _: &HomeLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Move::Step(Step::Left), cx);
    }

    fn home_right(&mut self, _: &HomeRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Move::Step(Step::Right), cx);
    }

    fn home_up(&mut self, _: &HomeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Move::Step(Step::Up), cx);
    }

    fn home_down(&mut self, _: &HomeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Move::Step(Step::Down), cx);
    }

    fn home_first(&mut self, _: &HomeFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Move::First, cx);
    }

    fn home_last(&mut self, _: &HomeLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Move::Last, cx);
    }

    /// Enter opens the card, with "Single click to open" on or off: that is about the mouse.
    fn home_open(&mut self, _: &HomeOpen, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.acting_ix() else {
            self.move_cursor(Move::First, cx);
            return;
        };
        let spot = self.spots.borrow().get(ix).map(|s| s.spot.clone());
        if let Some(spot) = spot {
            self.open_spot(&spot, cx);
        }
    }

    /// Shift+F10: the card's menu, where the card is (the Recycle Bin has none).
    fn home_menu(&mut self, _: &HomeMenu, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.acting_ix() else {
            self.move_cursor(Move::First, cx);
            return;
        };
        let (spot, menu, at) = {
            let spots = self.spots.borrow();
            let Some(s) = spots.get(ix) else {
                return;
            };
            (s.spot.clone(), s.menu.clone(), s.at.get())
        };
        let (Some((of, path)), Some(at)) = (menu, at) else {
            return;
        };
        self.pick(&spot);
        cx.notify();
        let position = at.center();
        cx.emit(match of {
            MenuOf::Favorite => HomeEvent::FavoriteMenu { path, position },
            MenuOf::Place => HomeEvent::PlaceMenu { path, position },
            MenuOf::File => HomeEvent::FileMenu { path, position },
        });
    }

    /// The spot under the keyboard's cursor (tests): a card's key, or a recent file's path.
    #[cfg(test)]
    pub(crate) fn cursor_for_test(&self) -> Option<String> {
        let ix = self.acting_ix()?;
        self.spots.borrow().get(ix).map(|s| match &s.spot {
            Spot::Card(key) => key.clone(),
            Spot::Recent(path) => path.display().to_string(),
        })
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
        // The cards record themselves again as they are drawn.
        let (was_focused, was_ring) = (self.focused, self.ring);
        self.focused = self.focus.is_focused(window);
        self.ring = match (self.focused && window.last_input_was_keyboard(), was_ring) {
            (false, _) => Ring::Off,
            // The page already had the keyboard and the mouse was in use: a key was just pressed
            // (GPUI draws this frame right before handling it), so the ring wasn't seen yet.
            (true, Ring::Off) if was_focused => Ring::Fresh,
            (true, _) => Ring::Shown,
        };
        // The ring comes up (Tab to the page, or the first key): its card comes into view.
        if self.ring != Ring::Off && was_ring == Ring::Off {
            self.reveal.set(true);
        }
        self.spots.borrow_mut().clear();
        // Favorites hidden in the sidebar (#323) are hidden here too (#539); collapsed is not.
        let favorites_hidden = SettingsStore::global(cx)
            .read(cx)
            .settings
            .is_hidden(SidebarSection::Favorites);
        let quick: Vec<AnyElement> = if favorites_hidden {
            Vec::new()
        } else {
            self.favorites
                .iter()
                .enumerate()
                .map(|(ix, (path, label, icon))| self.quick_card(ix, path, label, *icon, cx))
                .collect()
        };
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
                let id = ("home-drive", ix);
                let path = p.path.clone();
                let card = Self::place_card(
                    id,
                    self.card_picked(id),
                    Icon::Drive,
                    CardName::Text(p.label),
                    None,
                    space,
                    HomeEvent::OpenFolder(p.path),
                    cx,
                );
                self.key_spot(
                    card,
                    || {
                        KeySpot::new(
                            Spot::Card(card_key(id)),
                            Section::Drives,
                            Open::Event(HomeEvent::OpenFolder(path.clone())),
                            Some((MenuOf::Place, path)),
                        )
                    },
                    cx,
                )
                .into_any_element()
            })
            .collect();
        if !SettingsStore::global(cx).read(cx).settings.recycle_pinned {
            let id = ("home-recycle", 0);
            let card = Self::place_card(
                id,
                self.card_picked(id),
                Icon::Trash,
                CardName::Text(t!("recycle.title").into_owned()),
                None,
                None,
                HomeEvent::OpenRecycle,
                cx,
            );
            let card = self.key_spot(
                card,
                || {
                    KeySpot::new(
                        Spot::Card(card_key(id)),
                        Section::Drives,
                        Open::Event(HomeEvent::OpenRecycle),
                        None,
                    )
                },
                cx,
            );
            drives.push(card.into_any_element());
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
                let id = ("home-net", ix);
                let card = Self::place_card(
                    id,
                    self.card_picked(id),
                    Icon::NetDrive,
                    self.card_name(&path, label),
                    detail,
                    None,
                    HomeEvent::OpenFolder(path.clone()),
                    cx,
                );
                self.key_spot(
                    card,
                    || {
                        KeySpot::new(
                            Spot::Card(card_key(id)),
                            Section::Network,
                            Open::Event(HomeEvent::OpenFolder(path.clone())),
                            Some((MenuOf::Place, path)),
                        )
                    },
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let cards =
            |items: Vec<AnyElement>| div().flex().flex_wrap().gap(theme::s_12()).children(items);
        let has_quick = !quick.is_empty();
        let has_net = !net.is_empty();
        let page = div()
            .id("home-scroll")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            // The focused node the cards are reported under (see `key_spot`).
            .a11y(gpui::Role::Group, t!("home.title"))
            .on_action(cx.listener(Self::home_left))
            .on_action(cx.listener(Self::home_right))
            .on_action(cx.listener(Self::home_up))
            .on_action(cx.listener(Self::home_down))
            .on_action(cx.listener(Self::home_first))
            .on_action(cx.listener(Self::home_last))
            .on_action(cx.listener(Self::home_open))
            .on_action(cx.listener(Self::home_menu))
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
            );
        // The picked card is gone (a favorite removed, a drive unplugged): the ring goes back to
        // the first card, on the next frame.
        if self.focused
            && self.has_pick()
            && !self.spots.borrow().iter().any(|s| self.is_picked(&s.spot))
        {
            self.selected_card = None;
            self.selected_recent = None;
            window.request_animation_frame();
        }
        if self.enter_last && self.focused {
            self.enter_last = false;
            let last = self.spots.borrow().last().map(|s| s.section);
            let spot = last.and_then(|last| {
                let spots = self.spots.borrow();
                spots
                    .iter()
                    .find(|s| s.section == last)
                    .map(|s| s.spot.clone())
            });
            if let Some(spot) = spot {
                self.pick(&spot);
                self.reveal.set(true);
                window.request_animation_frame();
            }
        }
        page
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

    /// Two rows of three favorites, a row of two wide drive cards, then two recent files.
    fn page() -> Vec<(Section, Option<Bounds<Pixels>>)> {
        let at = |x: f32, y: f32, w: f32, h: f32| {
            Some(Bounds::new(point(px(x), px(y)), gpui::size(px(w), px(h))))
        };
        let mut spots: Vec<_> = (0..6)
            .map(|i| {
                let (col, row) = ((i % 3) as f32, (i / 3) as f32);
                (Section::Favorites, at(col * 144., row * 100., 132., 88.))
            })
            .collect();
        spots.push((Section::Drives, at(0., 240., 260., 72.)));
        spots.push((Section::Drives, at(272., 240., 260., 72.)));
        spots.push((Section::Recent, at(0., 360., 800., 48.)));
        spots.push((Section::Recent, at(0., 408., 800., 48.)));
        spots
    }

    #[test]
    fn arrows_walk_the_grid_and_go_on_at_its_edges() {
        let page = page();
        let go = |at: usize, step: Step| step_to(&page, at, step);
        // Left and Right: along the section, row after row, never out of it.
        assert_eq!(go(0, Step::Right), Some(1));
        assert_eq!(go(2, Step::Right), Some(3), "on to the next row");
        assert_eq!(go(0, Step::Left), None);
        assert_eq!(go(5, Step::Right), None, "the end of the favorites");
        assert_eq!(go(6, Step::Left), None, "the start of the drives");
        // Up and Down: the nearest by its middle in the next row, into the next section.
        assert_eq!(go(1, Step::Down), Some(4));
        assert_eq!(
            go(4, Step::Down),
            Some(6),
            "middle 210: nearest is the first drive"
        );
        assert_eq!(go(5, Step::Down), Some(7));
        assert_eq!(
            go(7, Step::Up),
            Some(5),
            "the favorite above the second drive"
        );
        assert_eq!(go(4, Step::Up), Some(1));
        assert_eq!(go(0, Step::Up), None, "nothing above the first row");
        // The recent files are a column.
        assert_eq!(go(7, Step::Down), Some(8));
        assert_eq!(go(8, Step::Down), Some(9));
        assert_eq!(go(9, Step::Down), None);
        assert_eq!(go(9, Step::Right), None);
        assert_eq!(
            go(8, Step::Up),
            Some(7),
            "middle 400: nearer the second drive"
        );
    }

    #[test]
    fn spots_not_drawn_yet_go_by_their_order() {
        let mut page = page();
        page[4].1 = None;
        assert_eq!(step_to(&page, 4, Step::Down), Some(5));
        assert_eq!(step_to(&page, 4, Step::Up), Some(3));
        assert_eq!(step_to(&page, 0, Step::Up), None);
    }

    #[test]
    fn recent_files_open_by_double_click_unless_single_click_is_on() {
        assert!(!recent_click_opens(false, 1));
        assert!(recent_click_opens(false, 2));
        assert!(!recent_click_opens(false, 3));
        assert!(recent_click_opens(false, 4));
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
