//! Sidebar: favorite tiles, pins, This PC (an expandable folder tree), and tabs.
//!
//! Tree children are listed on a background thread (`Fs::read_dir`); the UI thread only renders results.
//! The tree under This PC follows the active pane's folder (#241): the folders down to it expand
//! by themselves and close again once it moves elsewhere.
//! Favorites, This PC, Network and Tabs each collapse to their heading (remembered); Home and its
//! tiles don't. Tiles drag like app icons on a phone (`workspace_tabdrag.rs`). A favorite (tile
//! or list entry) dragged out of the sidebar and let go there leaves the favorites, like an app
//! dragged off the macOS Dock, with Undo on a toast.
//! The context menu is the sidebar's own small menu (open in new tab, pin, favorites, ...).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arke_core::filetype::FileKind;
use gpui::{
    AnimationExt, AnyElement, App, Bounds, ClickEvent, Context, DispatchPhase, Div, DragMoveEvent,
    Entity, ExternalPaths, Focusable as _, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, SharedString, Stateful, Subscription, Window,
    canvas, div, prelude::*, px, relative, svg,
};
use rust_i18n::t;

use super::pindrop::{self, PinArea, PinDrops as _};
use super::{DraggedTab, Tab, Workspace};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{DraggedFiles, Explorer, drop_external_into, drop_into, kind_icon};
use crate::settings::{FAVORITES_LARGE, FAVORITES_MAX, PinSlot, SettingsStore, SidebarSection};
use crate::shell::{KnownFolder, Place, PlaceKind};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};
use crate::workspace::{Tooltip, TooltipBelowExt as _};

/// One tree indent level.
fn tree_indent() -> gpui::Pixels {
    crate::theme::scaled_px(16.)
}
/// Drive usage bar: thin, under the label, inside the row (the row keeps its height).
fn usage_bar_h() -> gpui::Pixels {
    crate::theme::scaled_px(3.)
}
/// Used share above which the usage bar turns red.
const USAGE_DANGER: f32 = 0.9;

/// Free space shown on a drive row: the text on the right and the used share for the bar.
struct DriveSpace {
    free: String,
    used: f32,
}
/// Max subfolders listed per folder; past that a tree is not useful.
const TREE_MAX_CHILDREN: usize = 500;

/// Subfolders of a tree node; `None` while listing.
pub(super) type TreeChildren = Option<Vec<PathBuf>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarAction {
    OpenInNewTab(PathBuf),
    TogglePin(PathBuf),
    /// Move a tile down to the pinned list.
    MoveToList(PathBuf),
    /// Move a pinned list entry up to the tiles.
    MoveToTiles(PathBuf),
    /// A folder picked in the back or forward button's menu: `steps` down that pane's history,
    /// where `path` was when the menu opened.
    HistoryJump {
        pane: gpui::EntityId,
        back: bool,
        steps: usize,
        path: PathBuf,
    },
    /// The top row of a back or forward menu when the button goes to another tab first: one
    /// step across tabs, to pane `to` (showing `path`).
    TimelineStep {
        back: bool,
        to: gpui::EntityId,
        path: PathBuf,
    },
    CloseTab(usize),
    DuplicateTab(usize),
    CloseOtherTabs(usize),
    CloseAllTabs,
    TogglePinTab(usize),
    ReopenClosedTab,
    Unsplit(usize),
    /// Close a split tab's active side (Ctrl+W).
    CloseSide(usize),
    /// Windows "Map network drive" dialog.
    MapDrive,
    /// Windows "Disconnect network drive" dialog.
    DisconnectDrive(PathBuf),
    /// Eject a removable drive.
    EjectDrive(PathBuf),
    /// Rename a mapped drive or network place in place (the name is Arke's own).
    RenamePlace(PathBuf),
    /// Give a favorite a name of its own, shown on its tile or row (#259).
    RenameFavorite(PathBuf),
    /// Remove from the sidebar Network section.
    ForgetShare(PathBuf),
    /// Remove a saved SFTP/WebDAV location (and its saved password).
    ForgetRemote(PathBuf),
    /// Take back the plain-http sign-in allowed for a WebDAV location (#252).
    RequireSecure(PathBuf),
    /// Add to or remove from global search's network locations.
    ToggleSearchNetwork(PathBuf),
    /// Window menu (right click on the title bar).
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    WindowClose,
    OpenSettings,
    OpenRecycleInNewTab,
    SetRecyclePinned(bool),
    /// The collapsed sidebar opens over the content on hover (#122).
    SetSidebarPeek(bool),
    /// Copy or move the shelf's items into the active tab's folder.
    ShelfToHere {
        is_move: bool,
    },
    ShelfClear,
    ShelfRemove(PathBuf),
    /// Go to a shelf item's folder and select it.
    ShelfReveal(PathBuf),
}

/// How much bigger a dragged tile is drawn under the pointer than in its slot: lifted off the grid.
pub(super) const TILE_LIFT: f32 = 1.1;

/// A favorite tile's height: the large ones (three a row) are taller.
fn tile_height(large: bool) -> Pixels {
    theme::scaled_px(if large { 56. } else { 48. })
}

/// How a dragged tile looks in its slot, for its copy under the pointer.
#[derive(Clone, Copy)]
struct TileLook {
    size: gpui::Size<Pixels>,
    large: bool,
    /// It is the current location (its icon in the accent color).
    here: bool,
}

/// A pinned folder being dragged, from the tiles or the list.
#[derive(Clone)]
pub(super) struct DraggedPin {
    pub(super) path: PathBuf,
    label: SharedString,
    icon: Icon,
    /// Dragged from the tiles: reorders them live while over other tiles (`workspace_tabdrag.rs`).
    pub(super) from_tile: bool,
    /// From the tiles: how the tile looks, so the copy under the pointer is the tile itself.
    look: Option<TileLook>,
}

/// What follows the pointer while a favorite is dragged. A tile is lifted off the grid like an app
/// icon on a phone: the tile itself, a little bigger, on the hover surface with a soft shadow,
/// held where it was taken hold of so it doesn't jump. A list entry is a chip with its name.
/// Outside the sidebar, where letting go removes it, it fades a little and wears a trash badge.
struct PinPreview {
    pin: DraggedPin,
    /// Where the pointer took hold of it, from its top left corner.
    grab: Point<Pixels>,
    workspace: gpui::WeakEntity<Workspace>,
}

/// How opaque a dragged favorite is drawn outside the sidebar, where letting go removes it.
const PIN_OUT_OPACITY: f32 = 0.6;

/// The trash badge on the corner of a dragged favorite that would be removed if let go.
fn remove_badge() -> AnyElement {
    div()
        .debug_selector(|| "pin-remove-cue".to_owned())
        .absolute()
        .top(-theme::s_6())
        .right(-theme::s_6())
        .size(theme::s_20())
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_FULL)
        .bg(theme::overlay())
        .border_1()
        .border_color(theme::danger())
        .shadow(theme::elev_tooltip())
        .child(crate::assets::tinted(
            Icon::Trash,
            theme::icon_sm(),
            theme::danger(),
        ))
        .into_any_element()
}

impl Render for PinPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let out = self
            .workspace
            .upgrade()
            .is_some_and(|ws| ws.read(cx).tab_drag.pin_out());
        let pin = &self.pin;
        let Some(look) = pin.look else {
            let chip = div()
                .h(theme::h_row())
                .px(theme::s_8())
                .flex()
                .items_center()
                .gap(theme::s_8())
                .rounded(theme::r_8())
                .bg(theme::overlay())
                .shadow(theme::elev_float())
                .ui_font()
                .text_size(theme::fs_14())
                .text_color(theme::text())
                .child(crate::assets::tinted(
                    pin.icon,
                    theme::icon(),
                    theme::text_muted(),
                ))
                .child(pin.label.clone())
                .when(out, |d| d.opacity(PIN_OUT_OPACITY));
            return div()
                .relative()
                .child(chip)
                .children(out.then(remove_badge))
                .into_any_element();
        };
        let lift = TILE_LIFT;
        let (gap, icon) = if look.large {
            (theme::s_4(), theme::s_20())
        } else {
            (theme::s_2(), theme::scaled_px(18.))
        };
        // The preview is placed at the tile's top left corner as it was taken hold of; the
        // bigger copy grows around the grab point, which stays under the pointer.
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(gap * lift)
            .rounded(theme::r_8() * lift)
            // Opaque under the hover wash: it floats over rows, and a shadow under a see-through
            // surface would darken it.
            .bg(theme::frame_solid())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::fs_12() * lift)
            .text_color(theme::text())
            .debug_selector(|| "tile-lifted".to_owned())
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .rounded(theme::r_8() * lift)
                    .bg(theme::wash_strong()),
            )
            .child(crate::assets::tinted(
                pin.icon,
                icon * lift,
                if look.here {
                    theme::signal()
                } else {
                    theme::text_muted()
                },
            ))
            .child(
                div()
                    .max_w_full()
                    .px(theme::s_4() * lift)
                    .truncate()
                    .child(pin.label.clone()),
            )
            .when(out, |d| d.opacity(PIN_OUT_OPACITY));
        let card = div()
            .absolute()
            .left(-(self.grab.x * (lift - 1.)))
            .top(-(self.grab.y * (lift - 1.)))
            .w(look.size.width * lift)
            .h(look.size.height * lift)
            .child(body)
            .children(out.then(remove_badge));
        div().relative().child(card).into_any_element()
    }
}

/// Starts a pin drag, taken hold of at `grab`: records it (an empty list needs a drop target, and
/// the sidebar watches for it leaving) and returns the drag preview.
fn start_pin_drag(
    workspace: &gpui::WeakEntity<Workspace>,
    dragged: &DraggedPin,
    grab: Point<Pixels>,
    cx: &mut App,
) -> Entity<PinPreview> {
    let _ = workspace.update(cx, |ws, cx| {
        ws.dragging_pin = true;
        ws.tab_drag
            .start_tile(dragged.path.clone(), dragged.from_tile, grab);
        cx.notify();
    });
    cx.new(|_| PinPreview {
        pin: dragged.clone(),
        grab,
        workspace: workspace.clone(),
    })
}

/// Whether a dragged favorite at `at` is outside the sidebar drawn at `sidebar` (left or right of
/// its column, or out of the window), where letting go removes it.
fn pin_outside(sidebar: Bounds<Pixels>, at: Point<Pixels>, window: gpui::Size<Pixels>) -> bool {
    at.x < sidebar.left() || at.x > sidebar.right() || at.y < px(0.) || at.y > window.height
}

/// A favorite dragged out of the sidebar and let go: what Undo puts back.
#[derive(Clone)]
pub(super) struct RemovedFavorite {
    path: PathBuf,
    /// Its slot on the tiles or in the list.
    slot: PinSlot,
    /// The name the user gave it (#259), which removing forgets.
    name: Option<String>,
}

pub(super) struct SidebarMenu {
    position: Point<Pixels>,
    items: Vec<(SharedString, SidebarAction)>,
    /// The tabs when it opened: rows name tabs by index, so a tab opened or closed meanwhile
    /// would make them act on the wrong one; the menu closes instead.
    tabs: Vec<u64>,
    /// The window it is drawn in (`popup.rs`).
    popup: crate::popup::Single,
}

/// Whether `dir` has at least one non-hidden subfolder. Stops at the first one.
fn has_subfolder(fs: &dyn arke_core::Fs, dir: &Path) -> bool {
    fs.read_dir(dir).is_ok_and(|iter| {
        iter.filter_map(Result::ok)
            .any(|e| e.is_dir_like() && !e.is_hidden())
    })
}

/// Subfolders of `dir` in the tree's order, and which of them have no subfolder of their own (only
/// checked when `local`: network paths can be slow). Runs on a background thread.
fn list_subfolders(
    fs: &dyn arke_core::Fs,
    dir: &Path,
    local: bool,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs: Vec<PathBuf> = match fs.read_dir(dir) {
        Ok(iter) => iter
            .filter_map(Result::ok)
            .filter(|e| e.is_dir_like() && !e.is_hidden())
            .map(|e| arke_core::remote::canonical(dir.join(&*e.name)))
            .take(TREE_MAX_CHILDREN)
            .collect(),
        Err(_) => Vec::new(),
    };
    // Same case-insensitive natural order as the file list.
    dirs.sort_by(|a, b| {
        arke_core::sort::natural_cmp(
            &display_name(a).to_lowercase(),
            &display_name(b).to_lowercase(),
        )
    });
    // Subfolders without nested folders get no chevron.
    let leaves = if local {
        dirs.iter()
            .filter(|d| !has_subfolder(fs, d))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    (dirs, leaves)
}

/// The places listed under This PC (network drives go under Network).
const fn in_this_pc(kind: PlaceKind) -> bool {
    matches!(
        kind,
        PlaceKind::Folder(KnownFolder::Profile)
            | PlaceKind::Cloud
            | PlaceKind::Drive { remote: false, .. }
            | PlaceKind::Wsl
    )
}

/// How many places This PC lists.
fn this_pc_count(places: &[Place]) -> usize {
    places.iter().filter(|p| in_this_pc(p.kind)).count()
}

/// The folders the tree opens to show `here` (#241): from the This PC root that holds it (the
/// innermost one, so the user folder wins over its drive) down to its parent. Empty when `here`
/// is a root itself or isn't under This PC.
fn follow_chain(places: &[Place], here: &Path) -> Vec<PathBuf> {
    let Some(root) = places
        .iter()
        .filter(|p| in_this_pc(p.kind) && here.starts_with(&p.path))
        .map(|p| p.path.as_path())
        .max_by_key(|root| root.components().count())
    else {
        return Vec::new();
    };
    let mut chain: Vec<PathBuf> = here
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();
    chain
}

/// Which favorites get a tile, and whether the tiles are the large ones (three a row).
#[derive(Debug, PartialEq, Eq)]
struct TileLayout {
    large: bool,
    on_tile: Vec<bool>,
}

/// Lays out the favorites (#174): a name that doesn't fit on a large tile is listed as a row below
/// the tiles instead of being cut off. More than six tiles shrink to four a row, but only when that
/// cuts none of their names off; otherwise they stay three a row and take more rows. The saved
/// order is untouched, so a name that fits again (a wider sidebar, a shorter name) goes back to
/// its own slot. `fits(label, per_row)`: the label fits on a tile that many to a row.
fn split_tiles(labels: &[&str], fits: &mut impl FnMut(&str, usize) -> bool) -> TileLayout {
    let on_tile: Vec<bool> = labels.iter().map(|label| fits(label, 3)).collect();
    let shown = on_tile.iter().filter(|&&tile| tile).count();
    let large = shown <= FAVORITES_LARGE
        || labels
            .iter()
            .zip(&on_tile)
            .any(|(label, &tile)| tile && !fits(label, 4));
    TileLayout { large, on_tile }
}

/// Room for a tile's name when `per_row` tiles share a row `list` wide.
fn tile_text_width(list: Pixels, per_row: usize) -> Pixels {
    let per_row = per_row as f32;
    let gap = f32::from(theme::s_4());
    let tile = (f32::from(list) - gap * (per_row - 1.)) / per_row;
    // The name's own padding, on both sides.
    px(tile - gap * 2.)
}

/// What a favorite on the tiles is called without a name of its own: the place's name (a mapped
/// drive's custom one included), or the folder's.
fn favorite_default(names: &crate::place_names::Names, places: &[Place], path: &Path) -> String {
    places.iter().find(|p| p.path == path).map_or_else(
        || display_name(path),
        |p| crate::place_names::place_label(names, p),
    )
}

/// A favorite being renamed in place, and the box it is typed in.
pub(super) struct FavoriteRename {
    path: PathBuf,
    /// Its name without one of its own; typing it (or nothing) brings it back.
    default: String,
    input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

/// The favorites with their names and icons, in order: the sidebar's tiles and Home's cards
/// (#173) show the same list.
pub(super) fn favorite_tiles(places: &[Place], cx: &mut App) -> Vec<(PathBuf, String, Icon)> {
    let settings = &SettingsStore::global(cx).read(cx).settings;
    settings
        .tiles(places)
        .into_iter()
        .map(|path| {
            // A name the user gave the favorite (#259) comes first.
            let label = settings
                .favorite_names
                .get(&crate::place_names::key(&path))
                .cloned()
                .unwrap_or_else(|| favorite_default(&settings.place_names, places, &path));
            let icon = match places.iter().find(|p| p.path == path) {
                Some(p) => place_icon(p.kind),
                // Neither a known folder nor a listed drive: drive roots (e.g. an unplugged USB
                // stick) get the drive icon, the rest a folder.
                None if path.parent().is_none() => Icon::Drive,
                None => Icon::Folder,
            };
            (path, label, icon)
        })
        .collect()
}

/// Display name: the folder name, or the full path for a drive root (C:\).
pub(super) fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The second, dimmer text of a menu row: the folder a history entry is in.
fn menu_detail(action: &SidebarAction) -> Option<SharedString> {
    match action {
        SidebarAction::HistoryJump { path, .. } | SidebarAction::TimelineStep { path, .. } => {
            arke_core::path::parent(path).map(|p| p.display().to_string().into())
        }
        _ => None,
    }
}

/// Icon for known folders and drives, shared by the sidebar and Ctrl+T.
pub(crate) const fn place_icon(kind: PlaceKind) -> Icon {
    match kind {
        PlaceKind::Folder(kf) => known_icon(kf),
        PlaceKind::Drive { remote: true, .. } => Icon::NetDrive,
        PlaceKind::Drive { .. } => Icon::Drive,
        PlaceKind::Wsl => Icon::Terminal,
        PlaceKind::Cloud => Icon::Cloud,
    }
}

pub(super) const fn known_icon(kf: KnownFolder) -> Icon {
    match kf {
        // A person, not the house: the house is the Home page.
        KnownFolder::Profile => Icon::KfUser,
        KnownFolder::Desktop => Icon::KfDesktop,
        KnownFolder::Downloads => Icon::KfDownload,
        KnownFolder::Documents => Icon::KfDocuments,
        KnownFolder::Pictures => Icon::KfPictures,
        KnownFolder::Music => Icon::KfMusic,
        KnownFolder::Videos => Icon::KfVideos,
    }
}

/// How much more readily the tab list shrinks than the places above it when the sidebar runs out
/// of room (flex shrink factors): it gives way to its minimum before the places shrink.
pub(super) const TABS_GIVE_WAY: f32 = 1000.;

/// `items` in `list`, scrolling on its own inside `frame` (which sets its size). A row cut off by the
/// bottom edge is dimmed, the more of it is hidden the more, so it reads as going on below rather
/// than broken. (A gradient over it can't match the Mica background.)
pub(super) fn scrolling(
    list: Stateful<Div>,
    items: Vec<AnyElement>,
    scroll: &ScrollHandle,
    frame: Div,
) -> Div {
    // Row positions are laid out unscrolled (from the last frame); the bottom edge in those terms
    // follows the current scroll.
    let edge = scroll.bounds().bottom() - scroll.offset().y;
    let items = items.into_iter().enumerate().map(|(ix, item)| {
        match scroll
            .bounds_for_item(ix)
            .filter(|b| b.top() < edge && b.bottom() > edge + px(0.5))
        {
            Some(row) => {
                let hidden = ((row.bottom() - edge) / row.size.height).clamp(0., 1.);
                div()
                    .flex()
                    .flex_col()
                    .opacity(CUT_ROW_OPACITY.0 + CUT_ROW_OPACITY.1 * (1. - hidden))
                    .child(item)
                    .into_any_element()
            }
            None => item,
        }
    });
    frame.flex().flex_col().child(
        list.flex_initial()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .children(items),
    )
}

/// Opacity of a row cut off at the bottom of a scrolling list: base plus this much times the
/// visible fraction.
const CUT_ROW_OPACITY: (f32, f32) = (0.2, 0.5);

fn section_label(text: impl Into<SharedString>) -> impl IntoElement {
    let text: SharedString = text.into();
    div()
        .flex_none()
        .h(theme::h_row())
        .flex()
        .items_end()
        .px(theme::s_8())
        .pb(theme::s_6())
        // Section heading (like Files): item size, semibold and full ink, so the groups read as
        // headings rather than captions; the weight alone sets them apart from the items.
        .text_size(theme::fs_14())
        .font_weight(theme::FW_STRONG)
        .text_color(theme::text())
        .child(text)
}

/// A section heading's element id (also its debug selector and hover group) and title.
fn section_id(section: SidebarSection) -> (&'static str, SharedString) {
    let (id, title) = match section {
        // Kept from when the heading only labelled the pinned list.
        SidebarSection::Favorites => ("pin-list-label", t!("sidebar.favorites")),
        SidebarSection::ThisPc => ("this-pc-header", t!("sidebar.this_pc")),
        SidebarSection::Network => ("network-header", t!("sidebar.network")),
    };
    (id, title.into_owned().into())
}

/// Under the sticky Home row while the places are scrolled down: a hairline with a soft shadow
/// below it, so the tiles and rows read as going on under it rather than starting cut off (on a
/// dark theme the shadow alone hardly shows). It takes no clicks.
fn scroll_shadow() -> AnyElement {
    let shade: gpui::Hsla = theme::scrim().into();
    div()
        .debug_selector(|| "sidebar-scroll-shadow".to_owned())
        .absolute()
        .top_0()
        // The sidebar's whole width, past its padding.
        .left(-theme::s_12())
        .right(-theme::s_8())
        .h(theme::s_8())
        .border_t_1()
        .border_color(theme::line())
        .bg(gpui::linear_gradient(
            180.,
            gpui::linear_color_stop(shade, 0.),
            gpui::linear_color_stop(gpui::Hsla { a: 0., ..shade }, 1.),
        ))
        .into_any_element()
}

/// Context menu for folders in the tree or pinned list. `in_list` adds "Move to tiles"
/// unless the tiles are full.
fn folder_menu(
    path: &Path,
    pinned: bool,
    in_list: bool,
    tiles_full: bool,
) -> Vec<(SharedString, SidebarAction)> {
    let mut items = vec![
        (
            t!("sidebar.open_in_new_tab").to_string().into(),
            SidebarAction::OpenInNewTab(path.to_path_buf()),
        ),
        (
            if pinned {
                t!("sidebar.remove_favorite")
            } else {
                t!("sidebar.add_favorite")
            }
            .to_string()
            .into(),
            SidebarAction::TogglePin(path.to_path_buf()),
        ),
    ];
    if in_list && !tiles_full {
        items.push((
            t!("sidebar.move_to_tiles").to_string().into(),
            SidebarAction::MoveToTiles(path.to_path_buf()),
        ));
    }
    items
}

/// Toggles tabs between the sidebar and the top. `horizontal` is the current layout.
pub(super) fn tab_layout_button(
    horizontal: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    use crate::settings::TabLayout;
    div()
        .id("toggle-tab-layout")
        .a11y_button(if horizontal {
            t!("sidebar.tabs_to_side")
        } else {
            t!("sidebar.tabs_to_top")
        })
        .size(theme::s_24())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(if horizontal {
                    Icon::TabsSide.path()
                } else {
                    Icon::TabsTop.path()
                })
                .size(theme::icon())
                .text_color(theme::text_subtle()),
        )
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            let layout = if horizontal {
                TabLayout::Vertical
            } else {
                TabLayout::Horizontal
            };
            SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.tab_layout = layout, cx));
        }))
        .tooltip_below(
            if horizontal {
                t!("sidebar.tabs_to_side")
            } else {
                t!("sidebar.tabs_to_top")
            }
            .to_string(),
        )
}

/// `\nas\docs` -> "docs (nas)"
fn share_label(share: &Path) -> String {
    let s = share.to_string_lossy();
    let mut parts = s.trim_start_matches('\\').splitn(2, '\\');
    match (parts.next(), parts.next()) {
        (Some(server), Some(name)) => {
            t!("sidebar.share_label", share = name, server = server).into_owned()
        }
        _ => s.into_owned(),
    }
}

/// Sidebar label of a saved remote location: the folder and its server, or just the server.
fn remote_label(url: &Path) -> String {
    let Some(url) = arke_core::remote::RemoteUrl::from_path(url) else {
        return url.display().to_string();
    };
    match url.name() {
        Some(name) => t!(
            "sidebar.share_label",
            share = name,
            server = url.host_port()
        )
        .into_owned(),
        None => url.host_port(),
    }
}

/// What a place that can be renamed is called without a custom name: a mapped drive (its name
/// without the letter), a recently opened share or a saved remote location. `None` for the rest.
pub(super) fn place_default_name(
    settings: &crate::settings::Settings,
    places: &[Place],
    path: &Path,
) -> Option<String> {
    use crate::place_names::{drive_default_name, key};
    let same = |other: &Path| key(other) == key(path);
    if let Some(drive) = places
        .iter()
        .find(|p| matches!(p.kind, PlaceKind::Drive { remote: true, .. }) && same(&p.path))
    {
        return Some(drive_default_name(drive));
    }
    if settings.net_places.iter().any(|p| same(p)) {
        return Some(share_label(path));
    }
    settings
        .remote_places
        .iter()
        .any(|p| same(&p.url))
        .then(|| remote_label(path))
}

/// Button next to the Network header: opens Arke's "Add network location" dialog.
fn map_drive_button(cx: &mut Context<Workspace>) -> impl IntoElement + use<> {
    div()
        .id("map-network-drive")
        .a11y_button(t!("sidebar.add_location"))
        .size(theme::s_24())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(Icon::Plus.path())
                .size(theme::icon())
                .text_color(theme::text_subtle()),
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.run_sidebar_action(SidebarAction::MapDrive, window, cx);
        }))
        .tooltip_below(t!("sidebar.add_location"))
}

/// Placeholder row while a tree node is listing its children.
fn loading_row(indent: Pixels) -> AnyElement {
    div()
        .flex_none()
        .h(theme::h_row())
        .flex()
        .items_center()
        .pl(indent)
        .text_size(theme::fs_12())
        .text_color(theme::text_subtle())
        .child(t!("info.loading").to_string())
        .into_any_element()
}

/// Lets sidebar items accept dropped files, from Arke or from Windows Explorer.
trait FileDropTarget: Sized {
    fn with_file_drop(self, dest: PathBuf, cx: &mut Context<Workspace>) -> Self;
}

impl FileDropTarget for gpui::Stateful<gpui::Div> {
    fn with_file_drop(self, dest: PathBuf, cx: &mut Context<Workspace>) -> Self {
        let external = dest.clone();
        let check = dest.clone();
        let hovered = dest.clone();
        // Hovering this folder during a drag opens it in the current tab after a countdown (see `spring.rs`).
        let spring_key = format!("side:{}", dest.display());
        let spring_bar = crate::spring::bar(&spring_key, cx);
        let workspace = cx.entity().downgrade();
        // Near the edge of a favorite or pinned folder, dragged folders are added to the sidebar
        // there instead (`workspace_pindrop.rs`), not dropped into this one. That is handled here:
        // GPUI ends the drag at the first target of its kind, even one that turns it down.
        self.relative()
            .children(spring_bar)
            .can_drop(move |value, window, cx| {
                pindrop::inserting(window, cx) || crate::explorer::accepts_drop(value, &check, cx)
            })
            .on_drag_move(move |e: &gpui::DragMoveEvent<DraggedFiles>, window, cx| {
                let inserting = pindrop::inserting(window, cx);
                if !inserting {
                    crate::explorer::report_drop_target(&hovered, e, cx);
                }
                crate::spring::hover_at(
                    spring_key.clone(),
                    crate::spring::Action::Open(hovered.clone()),
                    crate::spring::Target::Workspace(workspace.clone()),
                    !inserting && e.bounds.contains(&e.event.position),
                    e.event.position,
                    window,
                    cx,
                );
            })
            .drag_over::<DraggedFiles>(|s, _, window, cx| {
                if pindrop::inserting(window, cx) {
                    return s;
                }
                s.bg(theme::signal_wash())
                    .border_color(theme::signal_line())
            })
            .drag_over::<ExternalPaths>(|s, _, window, cx| {
                if pindrop::inserting(window, cx) {
                    return s;
                }
                s.bg(theme::signal_wash())
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedFiles, window, cx| {
                    if !this.drop_files_to_pin(dragged, window, cx) {
                        drop_into(dragged, dest.clone(), window, cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                if !this.drop_external_to_pin(paths, window, cx) {
                    let target = this.active_view().clone();
                    drop_external_into(paths, external.clone(), &target, window, cx);
                }
            }))
    }
}

pub(super) fn tooltip_text(
    text: String,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    move |_, cx| cx.new(|_| Tooltip(text.clone().into())).into()
}

impl Workspace {
    /// Drops a dragged pin: reorders or moves it between tiles and list depending on the slot.
    fn place_pin(&mut self, path: &Path, slot: crate::settings::PinSlot, cx: &mut Context<Self>) {
        self.dragging_pin = false;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        self.move_pin(path, slot, cx);
    }

    /// A pin dropped in a gap of the tile grid. A list entry goes in the last slot; a tile keeps the
    /// slot it reached while dragging and settles into it.
    fn drop_pin_between_tiles(
        &mut self,
        dragged: &DraggedPin,
        last: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if dragged.from_tile {
            self.drop_tile(&dragged.path, window, cx);
        } else {
            self.place_pin(&dragged.path, crate::settings::PinSlot::Tile(last), cx);
        }
    }

    /// A favorite dropped on the list (a row or the Favorites heading): it goes in where the
    /// insertion mark shows, or at `ix` once the pointer is past the mark's reach.
    fn drop_pin_in_list(
        &mut self,
        dragged: &DraggedPin,
        ix: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let at = match pindrop::current(window, cx) {
            Some(PinSlot::List(at)) => at,
            _ => ix,
        };
        self.dragging_pin = false;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        let places = self.active_view().read(cx).places.clone();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.insert_pin(&dragged.path, PinSlot::List(at), &places);
                },
                cx,
            );
        });
        cx.notify();
    }

    /// Moves a pin to `slot` and saves the new order, without ending the drag.
    pub(super) fn move_pin(
        &mut self,
        path: &Path,
        slot: crate::settings::PinSlot,
        cx: &mut Context<Self>,
    ) {
        let places = self.active_view().read(cx).places.clone();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.place_pin(path, slot, &places), cx);
        });
        cx.notify();
    }

    /// Pinned list row: draggable, flashes when just added. A folder or tab dragged over its top or
    /// bottom edge goes in above or below it (`workspace_pindrop.rs`).
    fn pin_list_row(
        &self,
        ix: usize,
        path: &Path,
        label: String,
        row: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let dragged = DraggedPin {
            path: path.to_path_buf(),
            label: label.into(),
            icon: Icon::Folder,
            from_tile: false,
            look: None,
        };
        // Over this row, or over the "Pinned" heading (first in the list).
        let mark = match (self.pin_shown, self.pin_mark) {
            (Some(_), Some(m)) if m.area == PinArea::List && m.ix == ix => {
                Some(pindrop::insertion_mark(false, m.before))
            }
            (Some(PinSlot::List(0)), None) if ix == 0 => Some(pindrop::insertion_mark(false, true)),
            _ => None,
        };
        let row = div()
            .id(SharedString::from(format!("pin-row-{}", path.display())))
            .debug_selector(move || format!("pin-row-{ix}"))
            .relative()
            .rounded(theme::r_8())
            .child(row)
            .children(mark)
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                    this.pin_hover(PinArea::List, ix, e, cx);
                }),
            )
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<DraggedFiles>, _, cx| {
                    this.pin_hover(PinArea::List, ix, e, cx);
                }),
            )
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                    this.pin_hover(PinArea::List, ix, e, cx);
                }),
            )
            // A favorite dragged over the list (a tile taken out of the grid, or another entry)
            // gets the insertion mark too.
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<DraggedPin>, _, cx| {
                    this.pin_hover(PinArea::List, ix, e, cx);
                }),
            )
            .with_pin_drops(cx)
            .on_drag(dragged, move |dragged, grab, _, cx| {
                start_pin_drag(&workspace, dragged, grab, cx)
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, window, cx| {
                this.drop_pin_in_list(dragged, ix, window, cx);
            }));
        match crate::workspace::pin_flash(path) {
            // Just pinned here: background fades out from the accent color.
            Some(serial) => row
                .with_animation(
                    ("pin-flash", serial as usize),
                    gpui::Animation::new(crate::motion::FLASH),
                    |el, t| el.bg(theme::blend(theme::signal_wash(), theme::clear(), t)),
                )
                .into_any_element(),
            None => row.into_any_element(),
        }
    }

    /// Favorite tiles: the user's list, or six common folders by default.
    pub(super) fn favorites(&self, cx: &mut Context<Self>) -> Vec<(PathBuf, String, Icon)> {
        let places = self.active_view().read(cx).places.clone();
        favorite_tiles(&places, cx)
    }

    /// Pins section: draggable tiles, the "Favorites" header, and the list below: favorites whose
    /// name doesn't fit on a tile (#174), then the pinned folders.
    #[allow(
        clippy::too_many_lines,
        reason = "the tiles, the heading and the list, each with its drops, read top to bottom"
    )]
    fn render_pins(&self, current: Option<&Path>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let pinned = settings.pinned.clone();
        let recycle_pinned = settings.recycle_pinned;
        let own_names = settings.favorite_names.clone();
        let collapsed = settings.is_collapsed(SidebarSection::Favorites);
        let favorites = self.favorites(cx);
        let layout = self.tile_layout(&favorites, cx);
        let large = layout.large;
        // Slots count every favorite, tiles or rows: drops and the saved order use them.
        let tile_count = favorites.len();
        let mut tiles: Vec<AnyElement> = Vec::new();
        let mut long_rows: Vec<AnyElement> = Vec::new();
        for (ix, (path, label, icon)) in favorites.into_iter().enumerate() {
            if layout.on_tile.get(ix).copied().unwrap_or(true) {
                let here = current == Some(path.as_path());
                let tile = self.render_tile(ix, &path, label, icon, here, large, cx);
                let cell = tiles.len();
                tiles.push(self.tile_drag_shell(ix, cell, &path, tile, cx));
            } else {
                let at = long_rows.len();
                // A plain folder keeps the tree's folder icon, like the pinned rows.
                let row_icon = (icon != Icon::Folder).then_some(icon);
                self.render_tree(
                    &path,
                    label.clone(),
                    None,
                    row_icon,
                    0,
                    current,
                    true,
                    &mut long_rows,
                    cx,
                );
                let row = long_rows.remove(at);
                long_rows.insert(at, Self::favorite_row(ix, &path, label, icon, row, cx));
            }
        }
        let mut pinned_rows = Vec::new();
        for (ix, path) in pinned.iter().enumerate() {
            let row_at = pinned_rows.len();
            let label = own_names
                .get(&crate::place_names::key(path))
                .cloned()
                .unwrap_or_else(|| display_name(path));
            self.render_tree(
                path,
                label.clone(),
                None,
                None,
                0,
                current,
                true,
                &mut pinned_rows,
                cx,
            );
            // The list row itself (not its expanded children) is draggable: reorder or drop onto tiles.
            let row = pinned_rows.remove(row_at);
            pinned_rows.insert(row_at, self.pin_list_row(ix, path, label, row, cx));
        }
        // With an empty list, a dragged tile still needs somewhere to drop (the heading, while
        // the list is collapsed).
        let wants_list_drop = pinned.is_empty() && self.dragging_pin;
        let list_drop = (wants_list_drop && !collapsed).then(|| {
            div()
                .id("pin-list-drop")
                .h(theme::h_row())
                .mx(theme::s_4())
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::r_8())
                .border_1()
                .border_dashed()
                .border_color(theme::line_strong())
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(t!("sidebar.drop_to_list").into_owned())
                .drag_over::<DraggedPin>(|s, _, _, _| {
                    s.bg(theme::signal_wash())
                        .border_color(theme::signal_line())
                })
                .on_drop(cx.listener(|this, dragged: &DraggedPin, window, cx| {
                    this.drop_pin_in_list(dragged, 0, window, cx);
                }))
        });
        // A folder or tab over the tiles: they wash over, and an "Add to favorites" slot follows
        // the last one (`workspace_pindrop.rs`).
        let add_slot = self.render_add_favorite_slot(tile_count, large, cx);
        let mut out = vec![
            div()
                .id("pin-tiles")
                .debug_selector(|| "pin-tiles".to_owned())
                .grid()
                .grid_cols(if large { 3 } else { 4 })
                .gap(theme::s_4())
                .rounded(theme::r_8())
                .when(add_slot.is_some(), |d| d.bg(theme::signal_wash()))
                .children(tiles)
                .children(add_slot)
                .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                    this.tiles_hover(e, cx);
                }))
                .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedFiles>, _, cx| {
                    this.tiles_hover(e, cx);
                }))
                .on_drag_move(
                    cx.listener(|this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                        this.tiles_hover(e, cx);
                    }),
                )
                .with_pin_drops(cx)
                .on_drop(cx.listener(move |this, dragged: &DraggedPin, window, cx| {
                    this.drop_pin_between_tiles(dragged, tile_count, window, cx);
                }))
                .into_any_element(),
        ];
        // The Recycle Bin is pinned by default and always sits last; it cannot be dragged.
        if recycle_pinned {
            pinned_rows.push(self.render_recycle_row(cx));
        }
        // No pins, no section. "Pin to sidebar" makes it appear; no placeholder text.
        let heading = !long_rows.is_empty() || !pinned_rows.is_empty() || wants_list_drop;
        if heading {
            out.push(
                self.section_header(SidebarSection::Favorites, collapsed, Vec::new(), cx)
                    // A folder or tab goes first in the list; with no folder pinned yet, or the
                    // list collapsed, the heading itself shows it.
                    .when(
                        (pinned.is_empty() || collapsed)
                            && self.pin_shown == Some(PinSlot::List(0)),
                        |d| d.child(pindrop::insertion_mark(false, false)),
                    )
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                        this.pin_hover_at(PinSlot::List(0), e, cx);
                    }))
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedFiles>, _, cx| {
                        this.pin_hover_at(PinSlot::List(0), e, cx);
                    }))
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                        this.pin_hover_at(PinSlot::List(0), e, cx);
                    }))
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedPin>, _, cx| {
                        this.pin_hover_at(PinSlot::List(0), e, cx);
                    }))
                    .with_pin_drops(cx)
                    // Dropping on the heading puts it first in the list.
                    .on_drop(cx.listener(|this, dragged: &DraggedPin, window, cx| {
                        this.drop_pin_in_list(dragged, 0, window, cx);
                    }))
                    .into_any_element(),
            );
        }
        // Collapsed to its heading: the list goes, the Shelf under it too (the tiles stay, they
        // belong to Home).
        if heading && collapsed {
            return out;
        }
        let start = out.len();
        // Favorites too long for a tile come first: they are ahead of the list in the saved order.
        // Each in a parent of its own, so a favorite also shown under This PC doesn't repeat an
        // element id.
        out.extend(long_rows.into_iter().enumerate().map(|(ix, row)| {
            div()
                .id(("favorite", ix))
                .flex()
                .flex_col()
                .child(row)
                .into_any_element()
        }));
        out.extend(list_drop.map(IntoElement::into_any_element));
        // A parent for each pinned row, so a folder pinned and also shown under This PC doesn't
        // repeat an element id (the screen reader tree needs them unique). Each row stays its own
        // item of the scrolling list, so one cut off at its bottom is dimmed alone.
        out.extend(pinned_rows.into_iter().enumerate().map(|(ix, row)| {
            div()
                .id(("pinned", ix))
                .flex()
                .flex_col()
                .child(row)
                .into_any_element()
        }));
        out.extend(self.render_shelf(cx));
        self.drop_in(SidebarSection::Favorites, &mut out, start);
        out
    }

    /// The width of the places list last frame (an estimate from the sidebar's width before the
    /// first): the tiles' grid is as wide.
    fn places_width(&self) -> Pixels {
        let drawn = self.sidebar_ui.places_scroll.bounds().size.width;
        if drawn > px(0.) {
            drawn
        } else {
            theme::sidebar_full_width() - theme::s_12() - theme::s_8()
        }
    }

    /// A tile's size in the grid, `large` ones three a row, the others four.
    fn tile_size(&self, large: bool) -> gpui::Size<Pixels> {
        let per_row: f32 = if large { 3. } else { 4. };
        let gap = f32::from(theme::s_4());
        let width = (f32::from(self.places_width()) - gap * (per_row - 1.)) / per_row;
        gpui::size(px(width), tile_height(large))
    }

    /// Which favorites fit on a tile (`split_tiles`), measured with the tiles' font in the width
    /// the places list had last frame (an estimate from the sidebar's width before the first).
    fn tile_layout(&self, favorites: &[(PathBuf, String, Icon)], cx: &App) -> TileLayout {
        let list = self.places_width();
        let mut wrapper = cx
            .text_system()
            .line_wrapper(theme::ui_font_at(gpui::FontWeight::NORMAL), theme::fs_12());
        let mut fits = |label: &str, per_row: usize| {
            wrapper
                .should_truncate_line(
                    label,
                    tile_text_width(list, per_row),
                    "",
                    gpui::TruncateFrom::End,
                )
                .is_none()
        };
        let labels: Vec<&str> = favorites
            .iter()
            .map(|(_, label, _)| label.as_str())
            .collect();
        split_tiles(&labels, &mut fits)
    }

    /// The favorites drawn as tiles, in order: the grid the tiles reorder and slide on (the others
    /// are rows, #174).
    pub(super) fn tile_favorites(&self, cx: &mut Context<Self>) -> Vec<PathBuf> {
        let favorites = self.favorites(cx);
        let layout = self.tile_layout(&favorites, cx);
        favorites
            .into_iter()
            .zip(layout.on_tile)
            .filter_map(|((path, ..), tile)| tile.then_some(path))
            .collect()
    }

    /// A favorite listed as a row because its name doesn't fit on a tile (#174). It still is one of
    /// the favorites at slot `ix`: dragged, it moves like a list entry; a pin dropped on it takes
    /// its slot.
    fn favorite_row(
        ix: usize,
        path: &Path,
        label: String,
        icon: Icon,
        row: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let dragged = DraggedPin {
            path: path.to_path_buf(),
            label: label.into(),
            icon,
            from_tile: false,
            look: None,
        };
        div()
            .id(SharedString::from(format!(
                "favorite-row-{}",
                path.display()
            )))
            .debug_selector(move || format!("favorite-row-{ix}"))
            .relative()
            .rounded(theme::r_8())
            .child(row)
            .on_drag(dragged, move |dragged, grab, _, cx| {
                start_pin_drag(&workspace, dragged, grab, cx)
            })
            .drag_over::<DraggedPin>(|s, _, _, _| s.bg(theme::signal_wash()))
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, _, cx| {
                this.place_pin(&dragged.path, crate::settings::PinSlot::Tile(ix), cx);
            }))
            .into_any_element()
    }

    /// The menu of a favorite, on a tile or in the list: the usual entries and "Rename" (#259),
    /// which names the favorite rather than the place.
    fn favorite_menu(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, SidebarAction)> {
        let places = self.active_view().read(cx).places.clone();
        let on_tiles = SettingsStore::global(cx)
            .read(cx)
            .settings
            .tiles(&places)
            .iter()
            .any(|p| p == path);
        let mut items = if on_tiles {
            vec![
                (
                    t!("sidebar.open_in_new_tab").to_string().into(),
                    SidebarAction::OpenInNewTab(path.to_path_buf()),
                ),
                (
                    t!("sidebar.move_to_list").to_string().into(),
                    SidebarAction::MoveToList(path.to_path_buf()),
                ),
                (
                    t!("sidebar.remove_favorite").to_string().into(),
                    SidebarAction::TogglePin(path.to_path_buf()),
                ),
            ]
        } else {
            // A pinned network place keeps its other entries; its row shows the favorite's name,
            // so that is the one "Rename" changes.
            self.tree_menu(path, 0, cx)
                .into_iter()
                .filter(|(_, action)| !matches!(action, SidebarAction::RenamePlace(_)))
                .collect()
        };
        items.push((
            t!("menu.rename").to_string().into(),
            SidebarAction::RenameFavorite(path.to_path_buf()),
        ));
        items
    }

    /// The sidebar's bottom row, Settings, and the "Removed from favorites" toast beside the
    /// sidebar's bottom while it shows.
    pub(super) fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .child(self.render_settings_button(cx))
            .into_any_element()
    }

    /// While a favorite is dragged: watches the pointer anywhere in the window. Outside the
    /// sidebar (`bounds`, this element's) the dragged copy shows it would be removed, and letting
    /// go there removes it. Nothing else takes a dragged favorite, so that is all a drop outside
    /// does.
    fn pin_out_watch(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.dragging_pin || !cx.has_active_drag() {
            return None;
        }
        let workspace = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| (),
                move |sidebar, (), window, _| {
                    // The mouse stays captured by the window during a drag: moves and the release
                    // past its edge arrive here too. Heard in the capture phase, before anything
                    // under the pointer could keep the event to itself.
                    let mover = workspace.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture {
                            return;
                        }
                        let out = pin_outside(sidebar, e.position, window.viewport_size());
                        let _ = mover.update(cx, |ws, cx| {
                            if ws.tab_drag.set_pin_out(out) {
                                cx.notify();
                            }
                        });
                    });
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                        // A cancelled press (`press_guard.rs`) was released nowhere.
                        if phase != DispatchPhase::Capture
                            || e.button != MouseButton::Left
                            || crate::press_guard::is_cancel(e)
                            || !pin_outside(sidebar, e.position, window.viewport_size())
                        {
                            return;
                        }
                        let _ = workspace.update(cx, Self::drop_pin_outside);
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element(),
        )
    }

    /// The dragged favorite was let go outside the sidebar: it leaves the favorites.
    fn drop_pin_outside(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.tab_drag.dragged_pin().map(Path::to_path_buf) else {
            return;
        };
        self.dragging_pin = false;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        self.remove_favorite(&path, cx);
    }

    /// Takes `path` off the favorites (tiles or list) and offers Undo on a toast for a while.
    fn remove_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        let places = self.active_view().read(cx).places.clone();
        let (slot, name) = {
            let settings = &SettingsStore::global(cx).read(cx).settings;
            let slot = settings
                .tiles(&places)
                .iter()
                .position(|p| p == path)
                .map(PinSlot::Tile)
                .or_else(|| {
                    settings
                        .pinned
                        .iter()
                        .position(|p| p == path)
                        .map(PinSlot::List)
                });
            let name = settings.favorite_name(path).map(str::to_owned);
            (slot, name)
        };
        let Some(slot) = slot else {
            return;
        };
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.unpin(path, &places), cx);
        });
        // The toast shows where all the others do, with Undo (the file list's notice).
        let removed = RemovedFavorite {
            path: path.to_path_buf(),
            slot,
            name,
        };
        let workspace = cx.entity().downgrade();
        let text = t!("sidebar.favorite_removed").into_owned();
        self.active_view().update(cx, |view, cx| {
            view.notice = Some(text.clone());
            view.notice_action = Some(crate::explorer::NoticeAction {
                text,
                label: t!("toast.undo").into_owned().into(),
                run: std::rc::Rc::new(move |cx| {
                    let _ = workspace.update(cx, |ws, cx| ws.undo_favorite_removal(&removed, cx));
                }),
            });
            cx.notify();
        });
        cx.notify();
    }

    /// Undo on the toast: the favorite goes back to its slot, with its name.
    fn undo_favorite_removal(&mut self, removed: &RemovedFavorite, cx: &mut Context<Self>) {
        let places = self.active_view().read(cx).places.clone();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.insert_pin(&removed.path, removed.slot, &places);
                    if let Some(name) = removed.name.clone() {
                        st.favorite_names
                            .insert(crate::place_names::key(&removed.path), name);
                    }
                },
                cx,
            );
        });
        cx.notify();
    }

    /// The folder the sidebar marks as the current location. None while a page (Home, Settings...)
    /// covers the active tab: the folder under it isn't where the user is (#98).
    fn sidebar_here(&self, cx: &App) -> Option<PathBuf> {
        if self.page_shown() {
            return None;
        }
        self.active_view()
            .read(cx)
            .display_path()
            .map(Path::to_path_buf)
    }

    /// The tree follows the folder (#241). Drawing can't change the tree, so following runs right
    /// after this frame, once per change of folder (or of This PC's places, which load later).
    fn schedule_follow(&self, current: Option<&PathBuf>, places: &[Place], cx: &mut Context<Self>) {
        let Some(here) = current else {
            return;
        };
        let count = this_pc_count(places);
        let followed = self
            .sidebar_ui
            .followed
            .as_ref()
            .is_some_and(|(path, n)| path == here && *n == count);
        if followed || self.sidebar_ui.follow_pending.replace(true) {
            return;
        }
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| {
                this.sidebar_ui.follow_pending.set(false);
                let here = this.sidebar_here(cx);
                this.follow_tree(here, cx);
            });
        });
    }

    /// This PC: user folders and all drives, all expandable (network drives go under Network).
    /// Drawn straight into the places, so the current folder's row knows its index there.
    fn render_this_pc(
        &self,
        places: &[Place],
        current: Option<&Path>,
        items: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_ui.here_row.set(None);
        for place in places.iter().filter(|p| in_this_pc(p.kind)) {
            self.render_device(place, current, items, cx);
        }
        // The tree just opened to the current folder: scroll its row into view if needed.
        if self.sidebar_ui.reveal.replace(false)
            && let Some(ix) = self.sidebar_ui.here_row.get()
        {
            self.sidebar_ui.places_scroll.scroll_to_item(ix);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the places, then the tabs with their heading, read top to bottom"
    )]
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.sidebar_here(cx);
        let places = self.active_view().read(cx).places.clone();
        let (this_pc_collapsed, network_collapsed) = {
            let settings = &SettingsStore::global(cx).read(cx).settings;
            (
                settings.is_collapsed(SidebarSection::ThisPc),
                settings.is_collapsed(SidebarSection::Network),
            )
        };
        if !this_pc_collapsed {
            self.schedule_follow(current.as_ref(), &places, cx);
        }
        // Places (pins, This PC, Network) and the vertical tab list each scroll on their own. When
        // they don't both fit, the tab list gives way first, down to three tabs, so opening many
        // tabs doesn't squeeze the places; only then do the places shrink too. The tabs never
        // fall below the fold, where nothing showed they were there. With room to spare, nothing
        // changes: the places take their height and the tabs follow right under them.
        let horizontal = Self::horizontal_tabs(cx);
        // The favorites, their list and the Shelf.
        let mut items = self.render_pins(current.as_deref(), cx);
        items.push(
            self.section_header(SidebarSection::ThisPc, this_pc_collapsed, Vec::new(), cx)
                .into_any_element(),
        );
        if !this_pc_collapsed {
            let start = items.len();
            self.render_this_pc(&places, current.as_deref(), &mut items, cx);
            self.drop_in(SidebarSection::ThisPc, &mut items, start);
        }
        self.render_network(
            &places,
            current.as_deref(),
            network_collapsed,
            &mut items,
            cx,
        );
        // The tiles and rows went under the Home row: a shadow there says so.
        let scrolled = self.sidebar_ui.places_scroll.offset().y < px(-0.5);
        let places = div()
            .id("sidebar-places")
            .flex()
            .flex_col()
            .gap(theme::s_2());
        let places_area = scrolling(
            places,
            items,
            &self.sidebar_ui.places_scroll,
            div()
                .debug_selector(|| "sidebar-places-frame".to_owned())
                // With tabs on top there is nothing below, so the places fill the sidebar.
                .when(horizontal, gpui::Styled::flex_1)
                .when(!horizontal, gpui::Styled::flex_initial)
                // At least two rows stay visible, so it's clear this part scrolls too.
                .min_h(theme::h_row() * 2.),
        )
        .relative()
        .when(scrolled, |d| d.child(scroll_shadow()));
        let sidebar = div()
            .id("sidebar")
            .a11y(gpui::Role::Tree, t!("a11y.sidebar"))
            .relative()
            .children(self.pin_out_watch(cx))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .pl(theme::s_12())
            .pr(theme::s_8())
            .pb(theme::s_8())
            .gap(theme::s_2())
            .child(places_area);
        if horizontal {
            return sidebar;
        }
        // The tabs don't collapse: they are what's open (hiding the whole sidebar makes room).
        let header = div()
            .debug_selector(|| "tabs-header".to_owned())
            .relative()
            .flex()
            .flex_none()
            .items_end()
            .child(self.tab_area_recorder(super::tabmove::AreaPart::ListHead))
            .child(div().flex_1().child(section_label(t!("sidebar.tabs"))))
            .child(Self::workspaces_button(cx))
            .child(tab_layout_button(false, cx));
        let sidebar = sidebar.child(header);
        let sidebar = {
            let tabs: Vec<AnyElement> = (0..self.tabs.len())
                .map(|ix| self.render_tab(ix, cx))
                .collect();
            let tab_count = tabs.len();
            sidebar
                // Dragging an unpinned tab turns the New tab row into the pin zone (same
                // height, so the tabs under the cursor don't move).
                .child(
                    div().flex_none().child(match self.pin_drop_zone(cx) {
                        Some(zone) => zone.into_any_element(),
                        None => Self::render_new_tab_row(cx).into_any_element(),
                    }),
                )
                .child(scrolling(
                    div()
                        .id("tab-list")
                        .a11y(gpui::Role::TabList, t!("sidebar.tabs"))
                        .flex()
                        .flex_col()
                        .gap(theme::s_2())
                        // Dropping in the gaps between tabs moves the tab to the end.
                        .on_drop(cx.listener(Self::drop_tab_at_end)),
                    tabs,
                    &self.sidebar_ui.tabs_scroll,
                    // Keeps up to three tabs in view however short the window gets.
                    div()
                        .debug_selector(|| "sidebar-tabs-frame".to_owned())
                        .flex_initial()
                        .flex_shrink(TABS_GIVE_WAY)
                        .min_h(Self::tab_list_min_height(tab_count)),
                ))
        };
        // Dropping in the empty space below the last tab moves it to the end.
        sidebar.child(
            div()
                .id("tab-list-end")
                .relative()
                .child(self.tab_area_recorder(super::tabmove::AreaPart::ListEnd))
                .flex_1()
                .min_h(theme::s_32())
                .on_drop(cx.listener(Self::drop_tab_at_end)),
        )
    }

    /// How far the sidebar's places scroll past what is shown (tests).
    #[cfg(test)]
    pub(crate) fn places_overflow_for_test(&self) -> Pixels {
        self.sidebar_ui.places_scroll.max_offset().y
    }

    /// Height the vertical tab list keeps when the sidebar runs out of room: up to three tabs.
    fn tab_list_min_height(tabs: usize) -> Pixels {
        let shown = tabs.min(3) as f32;
        (theme::h_row() + theme::s_2()) * shown
    }

    /// Drops a tab onto empty space in the tab strip or list: moves it to the end.
    pub(super) fn drop_tab_at_end(
        &mut self,
        dragged: &DraggedTab,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The tab may already have moved during the drag; use its current index.
        let from = self.dragging_tab.take().unwrap_or(dragged.ix);
        let last = self.tabs.len().saturating_sub(1);
        self.move_tab(from, last, cx);
    }

    /// Network section: header (with map drive button), network drives, and recently opened shares.
    /// Shares are not contacted until clicked.
    fn render_network(
        &self,
        places: &[Place],
        current: Option<&Path>,
        collapsed: bool,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        out.push(
            self.section_header(
                SidebarSection::Network,
                collapsed,
                vec![map_drive_button(cx).into_any_element()],
                cx,
            )
            .into_any_element(),
        );
        // Collapsed to its heading; adding a location stays one click away.
        if collapsed {
            return;
        }
        let start = out.len();
        for place in places
            .iter()
            .filter(|p| matches!(p.kind, PlaceKind::Drive { remote: true, .. }))
        {
            self.render_device(place, current, out, cx);
        }
        // Stored newest first (for eviction) but listed by name, so rows don't jump around as
        // shares are visited.
        let mut shares = SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        shares.sort_by_cached_key(|s| s.to_string_lossy().to_lowercase());
        let names = SettingsStore::global(cx)
            .read(cx)
            .settings
            .place_names
            .clone();
        for share in &shares {
            self.render_tree(
                share,
                crate::place_names::shown_name(&names, share).unwrap_or_else(|| share_label(share)),
                None,
                Some(Icon::NetDrive),
                0,
                current,
                false,
                out,
                cx,
            );
        }
        // Saved SFTP and WebDAV locations; not contacted until expanded or opened.
        let remotes: Vec<PathBuf> = SettingsStore::global(cx)
            .read(cx)
            .settings
            .remote_places
            .iter()
            .map(|p| p.url.clone())
            .collect();
        for url in &remotes {
            self.render_tree(
                url,
                crate::place_names::shown_name(&names, url).unwrap_or_else(|| remote_label(url)),
                None,
                Some(Icon::NetDrive),
                0,
                current,
                false,
                out,
                cx,
            );
        }
        self.drop_in(SidebarSection::Network, out, start);
    }

    /// Tree node context menu: the common folder entries, plus "Disconnect" for network drives
    /// and "Remove from list" for shares.
    fn tree_menu(
        &self,
        path: &Path,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, SidebarAction)> {
        let places = self.active_view().read(cx).places.clone();
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let in_list = depth == 0 && settings.pinned.iter().any(|p| p == path);
        let tiles_full = settings.tiles(&places).len() >= FAVORITES_MAX;
        let mut items = folder_menu(path, settings.is_pinned(path, &places), in_list, tiles_full);
        if depth > 0 {
            return items;
        }
        let net_place = settings.net_places.iter().any(|p| p == path);
        let mapped = self
            .active_view()
            .read(cx)
            .places
            .iter()
            .any(|p| p.path == path && matches!(p.kind, PlaceKind::Drive { remote: true, .. }));
        if place_default_name(settings, &places, path).is_some() {
            items.push((
                t!("menu.rename").to_string().into(),
                SidebarAction::RenamePlace(path.to_path_buf()),
            ));
        }
        if mapped {
            items.push((
                t!("sidebar.disconnect_drive").to_string().into(),
                SidebarAction::DisconnectDrive(path.to_path_buf()),
            ));
        }
        let is_drive = places
            .iter()
            .any(|p| p.path == path && matches!(p.kind, PlaceKind::Drive { remote: false, .. }));
        if is_drive
            && self
                .active_view()
                .read(cx)
                .services
                .shell
                .is_ejectable(path)
        {
            items.push((
                t!("sidebar.eject").to_string().into(),
                SidebarAction::EjectDrive(path.to_path_buf()),
            ));
        }
        if mapped || net_place {
            let searched = crate::global_search::is_searched(settings, path);
            items.push((
                if searched {
                    t!("sidebar.unsearch_network")
                } else {
                    t!("sidebar.search_network")
                }
                .to_string()
                .into(),
                SidebarAction::ToggleSearchNetwork(path.to_path_buf()),
            ));
        }
        if net_place {
            items.push((
                t!("sidebar.forget_share").to_string().into(),
                SidebarAction::ForgetShare(path.to_path_buf()),
            ));
        }
        if settings.remote_places.iter().any(|p| p.url == path) {
            // Only offered once plain http was allowed for it, so the choice stays visible and
            // can be taken back (#252).
            if crate::remote_shell::insecure_http_allowed(&self.services.remotes, path) {
                items.push((
                    t!("sidebar.require_secure").to_string().into(),
                    SidebarAction::RequireSecure(path.to_path_buf()),
                ));
            }
            items.push((
                t!("sidebar.forget_share").to_string().into(),
                SidebarAction::ForgetRemote(path.to_path_buf()),
            ));
        }
        items
    }

    fn render_new_tab_row(cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("new-tab")
            .a11y_button(t!("sidebar.new_tab"))
            .aria_keyshortcuts("Ctrl+T")
            .group("new-tab")
            .h(theme::h_row())
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::s_8())
            .px(theme::s_8())
            .rounded(theme::r_8())
            .cursor_pointer()
            .text_color(theme::text_subtle())
            .hover(|s| s.bg(theme::wash()).text_color(theme::text_muted()))
            .child(
                svg()
                    .path(Icon::TabNew.path())
                    .size(theme::icon())
                    .text_color(theme::text_subtle()),
            )
            .child(div().flex_1().child(t!("sidebar.new_tab").to_string()))
            // The shortcut only appears on hover so it takes no space otherwise.
            .child(
                div()
                    .invisible()
                    .group_hover("new-tab", gpui::Styled::visible)
                    .px(theme::s_4())
                    .rounded(theme::r_4())
                    .border_1()
                    .border_color(theme::line_strong())
                    .text_size(theme::fs_11())
                    .child("Ctrl T"),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_palette(&super::OpenPalette, window, cx);
            }))
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "a tile needs its place, label, icon and state, drawn top to bottom"
    )]
    fn render_tile(
        &self,
        ix: usize,
        path: &Path,
        label: String,
        icon: Icon,
        here: bool,
        large: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = path.to_path_buf();
        let tab_path = path.to_path_buf();
        let menu = self.favorite_menu(path, cx);
        let renaming = self.favorite_rename_box(path);
        let renamed = renaming.is_some();
        // Renaming (#259): the name turns into the box, inside the tile.
        let name = match renaming {
            Some(input) => div()
                .w_full()
                .px(theme::s_2())
                .flex()
                .child(crate::place_names::rename_box(input))
                .into_any_element(),
            None => div()
                .max_w_full()
                .px(theme::s_4())
                .truncate()
                .child(label.clone())
                .into_any_element(),
        };
        let workspace = cx.entity().downgrade();
        let dragged = DraggedPin {
            path: path.to_path_buf(),
            label: label.clone().into(),
            icon,
            from_tile: true,
            look: Some(TileLook {
                size: self.tile_size(large),
                large,
                here,
            }),
        };
        div()
            .id(SharedString::from(format!("tile-{}", path.display())))
            .a11y_lazy(gpui::Role::TreeItem, || label.clone())
            .debug_selector(move || format!("tile {label}"))
            .aria_selected(here)
            .h(tile_height(large))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(if large { theme::s_4() } else { theme::s_2() })
            .rounded(theme::r_8())
            .cursor_pointer()
            .text_size(theme::fs_12())
            .when(here, |d| {
                d.relative().child(here_layer()).text_color(theme::text())
            })
            // No background by default (six gray tiles look heavy), only on hover. The current location gets the paper surface.
            .when(!here, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
            })
            .child(crate::assets::tinted(
                icon,
                if large { theme::s_20() } else { crate::theme::scaled_px(18.) },
                if here {
                    theme::signal()
                } else {
                    theme::text_muted()
                },
            ))
            .child(name)
            .with_file_drop(path.to_path_buf(), cx)
            // Drag to reorder (live, see `tile_drag_shell`) or onto the list below.
            .on_drag(dragged, move |dragged, grab, _, cx| {
                start_pin_drag(&workspace, dragged, grab, cx)
            })
            // Only a list entry highlights its target; tiles make room instead.
            .drag_over::<DraggedPin>(|s, dragged, _, _| {
                if dragged.from_tile {
                    s
                } else {
                    s.bg(theme::signal_wash())
                }
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, window, cx| {
                cx.stop_propagation();
                let slot = crate::settings::PinSlot::Tile(ix);
                if dragged.from_tile {
                    // The tiles already reordered live while it moved: the drop only settles it.
                    this.drop_tile(&dragged.path, window, cx);
                } else {
                    this.place_pin(&dragged.path, slot, cx);
                }
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.navigate_active(open.clone(), window, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.open_in_background(tab_path.clone(), window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.open_sidebar_menu(e.position, menu.clone(), cx);
                }),
            )
            // Not over its own rename box.
            .when(!renamed, |d| {
                d.tooltip(tooltip_text(
                    t!("sidebar.tile_tooltip", path = path.display()).to_string(),
                ))
            })
    }

    fn render_device(
        &self,
        place: &Place,
        current: Option<&Path>,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let (icon, space) = match place.kind {
            PlaceKind::Drive { remote, space } => {
                (if remote { Icon::NetDrive } else { Icon::Drive }, space)
            }
            PlaceKind::Folder(kf) => (known_icon(kf), None),
            PlaceKind::Wsl => (Icon::Terminal, None),
            PlaceKind::Cloud => (Icon::Cloud, None),
        };
        // Drives get free space on the right and a thin usage bar under the label.
        let space = space.filter(|&(_, total)| total > 0).map(|(free, total)| {
            let used = 1. - free as f32 / total as f32;
            DriveSpace {
                free: arke_core::format::format_size(free),
                used: used.clamp(0., 1.),
            }
        });
        let label = crate::place_names::place_label(
            &SettingsStore::global(cx).read(cx).settings.place_names,
            place,
        );
        self.render_tree(
            &place.path,
            label,
            space,
            Some(icon),
            0,
            current,
            false,
            out,
            cx,
        );
    }

    /// An expandable folder; renders subfolders recursively when open. `favorite`: the row is a
    /// favorite's (in the list under the tiles), so its menu and "Rename" are the favorite's.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "a tree row needs its place, depth and state, and draws its children"
    )]
    fn render_tree(
        &self,
        path: &Path,
        label: String,
        space: Option<DriveSpace>,
        icon: Option<Icon>,
        depth: usize,
        current: Option<&Path>,
        favorite: bool,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let expanded = self.expanded.contains(path);
        // Just toggled: animate the chevron and the rows that appear.
        let cue = (self.tree_cue.0.as_deref() == Some(path)
            && self.tree_cue.1.live_at(std::time::Instant::now()))
        .then(|| self.tree_cue.1.serial());
        let here = current == Some(path);
        if here {
            // Where the current folder's row is, for scrolling it into view (#241).
            self.sidebar_ui.here_row.set(Some(out.len()));
        }
        let children = self.tree_children.get(path);
        // No chevron when the node was listed with no children, or a background check found no subfolders,
        // so users do not expand a folder only to find it empty.
        let leaf = matches!(children, Some(Some(list)) if list.is_empty())
            || self.tree_leaf.contains(path);
        let open = path.to_path_buf();
        let tab_path = path.to_path_buf();
        let (menu, renaming) = if favorite {
            (self.favorite_menu(path, cx), self.favorite_rename_box(path))
        } else {
            (
                self.tree_menu(path, depth, cx),
                self.sidebar_rename_box(path),
            )
        };
        let indent = tree_indent() * depth as f32;
        out.push(
            div()
                .id(SharedString::from(format!(
                    "tree-{depth}-{}",
                    path.display()
                )))
                .a11y_lazy(gpui::Role::TreeItem, || label.clone())
                .debug_selector({
                    let label = label.clone();
                    move || format!("sidebar {label}")
                })
                .aria_level(depth)
                .aria_selected(here)
                .when(!leaf, |d| d.aria_expanded(expanded))
                .relative()
                .h(theme::h_row())
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::s_4())
                .pl(indent + theme::s_4())
                .pr(theme::s_8())
                .rounded(theme::r_8())
                .cursor_pointer()
                // Current location: neutral wash.
                .when(here, |d| d.bg(theme::selection()))
                .when(!here, |d| d.hover(|s| s.bg(theme::wash())))
                .child(Self::tree_chevron(path, depth, expanded, leaf, cue, cx))
                .child(match icon {
                    Some(icon) => crate::assets::tinted(icon, theme::icon(), theme::text_muted()),
                    None => kind_icon(FileKind::Folder, theme::icon()),
                })
                .child(match renaming {
                    Some(input) => crate::place_names::rename_box(input),
                    None => tree_label(label, space).into_any_element(),
                })
                .with_file_drop(path.to_path_buf(), cx)
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    // Like File Explorer's navigation pane: the first click opens the folder,
                    // a double-click also expands or collapses it (#242). The clicks after the
                    // first don't open it again (that would reload it).
                    match e.click_count() {
                        0 | 1 => this.navigate_active(open.clone(), window, cx),
                        2 if !leaf => this.toggle_expand(open.clone(), cx),
                        _ => {}
                    }
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        this.open_in_background(tab_path.clone(), window, cx);
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        this.open_sidebar_menu(e.position, menu.clone(), cx);
                    }),
                )
                .into_any_element(),
        );
        if !expanded {
            return;
        }
        let start = out.len();
        match children {
            Some(Some(list)) => {
                for child in list {
                    self.render_tree(
                        child,
                        display_name(child),
                        None,
                        None,
                        depth + 1,
                        current,
                        false,
                        out,
                        cx,
                    );
                }
            }
            _ => out.push(loading_row(indent + tree_indent() + theme::s_32())),
        }
        // Just expanded: the new rows drop in under their parent.
        if self.tree_cue.0.as_deref() == Some(path)
            && let Some(t) = self.cue_progress(&self.tree_cue.1)
        {
            let rows: Vec<AnyElement> = out.drain(start..).collect();
            out.extend(crate::motion::drop_in(rows, t));
        }
    }

    /// Expand chevron: right when collapsed, down when expanded. `cue`: just toggled, so it turns.
    fn tree_chevron(
        path: &Path,
        depth: usize,
        expanded: bool,
        leaf: bool,
        cue: Option<u64>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let toggle = path.to_path_buf();
        let turn = move |t: f32| {
            gpui::Transformation::rotate(gpui::percentage(if expanded {
                0.25 * t
            } else {
                0.25 * (1. - t)
            }))
        };
        let glyph = svg()
            .path(Icon::ChevronRight.path())
            .size(theme::icon_sm())
            .text_color(theme::text_subtle());
        let glyph = match cue {
            Some(serial) => glyph
                .with_animation(
                    ("chev-turn", serial),
                    crate::motion::enter(),
                    move |s, t| s.with_transformation(turn(t)),
                )
                .into_any_element(),
            None => glyph.with_transformation(turn(1.)).into_any_element(),
        };
        div()
            .id(SharedString::from(format!(
                "chev-{depth}-{}",
                path.display()
            )))
            .size(theme::s_20())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_4())
            .when(!leaf, |d| {
                d.hover(|s| s.bg(theme::wash_strong()))
                    .child(glyph)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_expand(toggle.clone(), cx);
                    }))
            })
    }

    /// Expands or collapses. Expanding lists subfolders on a background thread. Either way the
    /// folder is the user's now: following the current folder (#241) no longer closes it.
    pub(super) fn toggle_expand(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.sidebar_ui.tree_auto.remove(&path);
        self.tree_cue.0 = Some(path.clone());
        self.tree_cue.1.fire();
        if self.expanded.remove(&path) {
            cx.notify();
            return;
        }
        self.expanded.insert(path.clone());
        // Relist on every expand since the folder may have changed; show the old result meanwhile.
        self.tree_children.entry(path.clone()).or_insert(None);
        let fs = Arc::clone(&self.services.fs);
        let shell = Arc::clone(&self.services.shell);
        let (tx, rx) = flume::bounded::<(Vec<PathBuf>, Vec<PathBuf>)>(1);
        let dir = path.clone();
        let spawned = std::thread::Builder::new()
            .name("tree".into())
            .spawn(move || {
                let local = shell.is_local_disk(&dir);
                let _ = tx.send(list_subfolders(fs.as_ref(), &dir, local));
            });
        if spawned.is_err() {
            return;
        }
        cx.spawn(async move |this, cx| {
            if let Ok((dirs, leaves)) = rx.recv_async().await {
                let _ = this.update(cx, |this, cx| this.tree_listed(path, dirs, leaves, cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// A folder's subfolders arrived from the background.
    fn tree_listed(
        &mut self,
        dir: PathBuf,
        dirs: Vec<PathBuf>,
        leaves: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        for child in &dirs {
            self.tree_leaf.remove(child);
        }
        self.tree_leaf.extend(leaves);
        self.tree_children.insert(dir, Some(dirs));
        cx.notify();
    }

    /// Opens the tree under This PC down to `here`, the active pane's folder, and marks it there
    /// (#241). Folders opened for an earlier folder close again unless the new one needs them;
    /// folders the user opened stay. Only the folders on the way are listed, one after another on
    /// a background thread, and a newer folder stops an older listing.
    fn follow_tree(&mut self, here: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(here) = here else {
            // A page covers the folder: the tree stays as it is.
            return;
        };
        let places = self.active_view().read(cx).places.clone();
        // Followed again when This PC's places change too: they load after the first frame.
        let key = (here.clone(), this_pc_count(&places));
        if SettingsStore::global(cx)
            .read(cx)
            .settings
            .is_collapsed(SidebarSection::ThisPc)
            || self.sidebar_ui.followed.as_ref() == Some(&key)
        {
            return;
        }
        self.sidebar_ui.followed = Some(key);
        if let Some(cancel) = self.sidebar_ui.follow_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let chain = follow_chain(&places, &here);
        let stale: Vec<PathBuf> = self
            .sidebar_ui
            .tree_auto
            .iter()
            .filter(|dir| !chain.contains(*dir))
            .cloned()
            .collect();
        for dir in &stale {
            self.sidebar_ui.tree_auto.remove(dir);
            self.expanded.remove(dir);
        }
        let mut to_list = Vec::new();
        for (ix, dir) in chain.iter().enumerate() {
            if self.expanded.insert(dir.clone()) {
                self.sidebar_ui.tree_auto.insert(dir.clone());
            }
            // Listed before, and the next folder on the way is there: nothing to read.
            let next = chain.get(ix + 1).unwrap_or(&here);
            let listed =
                matches!(self.tree_children.get(dir), Some(Some(list)) if list.contains(next));
            if !listed {
                self.tree_children.entry(dir.clone()).or_insert(None);
                to_list.push(dir.clone());
            }
        }
        self.sidebar_ui.reveal.set(to_list.is_empty());
        cx.notify();
        if to_list.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.sidebar_ui.follow_cancel = Some(Arc::clone(&cancel));
        let fs = Arc::clone(&self.services.fs);
        let shell = Arc::clone(&self.services.shell);
        let (tx, rx) = flume::unbounded::<(PathBuf, Vec<PathBuf>, Vec<PathBuf>)>();
        let spawned = std::thread::Builder::new()
            .name("tree-follow".into())
            .spawn(move || {
                for dir in to_list {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let local = shell.is_local_disk(&dir);
                    let (dirs, leaves) = list_subfolders(fs.as_ref(), &dir, local);
                    if tx.send((dir, dirs, leaves)).is_err() {
                        return;
                    }
                }
            });
        if spawned.is_err() {
            return;
        }
        cx.spawn(async move |this, cx| {
            while let Ok((dir, dirs, leaves)) = rx.recv_async().await {
                if this
                    .update(cx, |this, cx| this.tree_listed(dir, dirs, leaves, cx))
                    .is_err()
                {
                    return;
                }
            }
            // Every folder on the way is listed: the current folder's row is drawn now.
            let _ = this.update(cx, |this, cx| {
                this.sidebar_ui.reveal.set(true);
                cx.notify();
            });
        })
        .detach();
    }

    /// A section's heading. A click collapses the section to it or expands it again
    /// (remembered), like File Explorer's groups. The chevron at its right shows on hover, and
    /// always while collapsed so the section doesn't seem to be gone. `buttons` sit before the
    /// chevron and keep their clicks to themselves. Callers add the drops the heading takes.
    fn section_header(
        &self,
        section: SidebarSection,
        collapsed: bool,
        buttons: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (id, title) = section_id(section);
        let (cued, cue) = &self.sidebar_ui.section_cue;
        let turning = (*cued == Some(section) && cue.live_at(std::time::Instant::now()))
            .then(|| cue.serial());
        let turn = move |t: f32| {
            gpui::Transformation::rotate(gpui::percentage(if collapsed {
                0.25 * (1. - t)
            } else {
                0.25 * t
            }))
        };
        let glyph = svg()
            .path(Icon::ChevronRight.path())
            .size(theme::icon_sm())
            .text_color(theme::text_subtle());
        let glyph = match turning {
            Some(serial) => glyph
                .with_animation(
                    gpui::ElementId::Name(format!("{id}-chev-{serial}").into()),
                    crate::motion::enter(),
                    move |s, t| s.with_transformation(turn(t)),
                )
                .into_any_element(),
            None => glyph.with_transformation(turn(1.)).into_any_element(),
        };
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .a11y_button(title.clone())
            .aria_expanded(!collapsed)
            .group(id)
            .relative()
            .flex()
            .flex_none()
            .items_end()
            .rounded(theme::r_8())
            .cursor_pointer()
            // The chevron follows the title, so the heading's buttons keep the right edge and
            // line up across sections whether or not the chevron shows.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_end()
                    .child(section_label(title))
                    .child(
                        div()
                            .h(theme::h_row())
                            .ml(-theme::s_4())
                            .pb(px(9.))
                            .flex()
                            .flex_none()
                            .items_end()
                            .when(!collapsed, |d| {
                                d.invisible().group_hover(id, gpui::Styled::visible)
                            })
                            .child(glyph),
                    ),
            )
            .when(!buttons.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        // A click on a button doesn't collapse the section too.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .children(buttons),
                )
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_section(section, cx);
            }))
    }

    /// Collapses a section or expands it again. This PC expanded again catches up with the
    /// current folder.
    fn toggle_section(&mut self, section: SidebarSection, cx: &mut Context<Self>) {
        self.sidebar_ui.section_cue.0 = Some(section);
        self.sidebar_ui.section_cue.1.fire();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.toggle_collapsed(section), cx);
        });
        cx.notify();
    }

    /// The rows of `section` from `start` on: just expanded by its heading, they drop in under it
    /// like a folder's in the tree.
    fn drop_in(&self, section: SidebarSection, items: &mut Vec<AnyElement>, start: usize) {
        let (cued, cue) = &self.sidebar_ui.section_cue;
        if *cued != Some(section) {
            return;
        }
        let Some(t) = self.cue_progress(cue) else {
            return;
        };
        let rows: Vec<AnyElement> = items.drain(start..).collect();
        items.extend(crate::motion::drop_in(rows, t));
    }

    // --- Renaming favorites (#259) ---

    /// Starts renaming a favorite in its tile or row. The name is Arke's own and shows only for
    /// the favorite; the folder keeps its name everywhere else.
    fn start_favorite_rename(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let places = self.active_view().read(cx).places.clone();
        let settings = &SettingsStore::global(cx).read(cx).settings;
        // The box is drawn in the full sidebar, so it has to show.
        if settings.sidebar_collapsed && !self.rail_peek {
            return;
        }
        let on_tiles = settings.tiles(&places).iter().any(|p| p == path);
        let default = if on_tiles {
            favorite_default(&settings.place_names, &places, path)
        } else if settings.pinned.iter().any(|p| p == path) {
            display_name(path)
        } else {
            return;
        };
        let text = settings
            .favorite_name(path)
            .map_or_else(|| default.clone(), str::to_owned);
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text(text, cx);
            input.select_all_text(cx);
            input
        });
        let focus = input.read(cx).focus_handle(cx);
        let subscriptions =
            [
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Confirm => this.commit_favorite_rename(window, cx),
                        InputEvent::Cancel => this.cancel_favorite_rename(window, cx),
                        InputEvent::Changed => {}
                    }
                }),
                // Clicking elsewhere keeps what was typed, like renaming a file.
                cx.on_blur(&focus, window, |this, window, cx| {
                    this.commit_favorite_rename(window, cx);
                }),
            ];
        self.sidebar_ui.favorite_rename = Some(FavoriteRename {
            path: path.to_path_buf(),
            default,
            input,
            _subscriptions: subscriptions,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn commit_favorite_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.sidebar_ui.favorite_rename.take() else {
            return;
        };
        let typed = rename.input.read(cx).committed_text();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| st.rename_favorite(&rename.path, &typed, &rename.default),
                cx,
            );
        });
        self.focus_active(window, cx);
        cx.notify();
    }

    fn cancel_favorite_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_ui.favorite_rename.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// The box to draw for `path` while it is the favorite being renamed.
    fn favorite_rename_box(&self, path: &Path) -> Option<Entity<TextInput>> {
        self.sidebar_ui
            .favorite_rename
            .as_ref()
            .filter(|r| r.path == path)
            .map(|r| r.input.clone())
    }

    // --- Context menu ---

    /// Tests: the entries of the menu a right click on the place's row opens.
    #[cfg(test)]
    pub(crate) fn place_menu_labels_for_test(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        self.tree_menu(path, 0, cx)
            .into_iter()
            .map(|(label, _)| label.to_string())
            .collect()
    }

    /// Tests: "Rename" picked from the place's menu.
    #[cfg(test)]
    pub(crate) fn rename_place_from_menu_for_test(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_sidebar_action(SidebarAction::RenamePlace(path.to_path_buf()), window, cx);
    }

    #[cfg(test)]
    pub(super) fn sidebar_menu_labels_for_test(&self) -> Option<Vec<String>> {
        let menu = self.sidebar_menu.as_ref()?;
        Some(menu.items.iter().map(|(l, _)| l.to_string()).collect())
    }

    #[cfg(test)]
    pub(super) fn sidebar_menu_action_for_test(&self, ix: usize) -> Option<SidebarAction> {
        self.sidebar_menu
            .as_ref()
            .filter(|m| m.tabs == self.tab_ids())?
            .items
            .get(ix)
            .map(|(_, a)| a.clone())
    }

    /// The second line of each row of the open menu ("" for none).
    #[cfg(test)]
    pub(super) fn sidebar_menu_details_for_test(&self) -> Option<Vec<String>> {
        let menu = self.sidebar_menu.as_ref()?;
        Some(
            menu.items
                .iter()
                .map(|(_, a)| menu_detail(a).map_or_default(|d| d.to_string()))
                .collect(),
        )
    }

    pub(super) fn open_sidebar_menu(
        &mut self,
        position: Point<Pixels>,
        items: Vec<(SharedString, SidebarAction)>,
        cx: &mut Context<Self>,
    ) {
        let tabs = self.tab_ids();
        self.sidebar_menu = Some(SidebarMenu {
            position,
            items,
            tabs,
            popup: crate::popup::Single::new(),
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn tab_ids(&self) -> Vec<u64> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    /// Closes the open menu if tabs were opened or closed since it opened (before paint, and
    /// before running a row).
    pub(super) fn drop_stale_menu(&mut self) {
        if self
            .sidebar_menu
            .as_ref()
            .is_some_and(|m| m.tabs != self.tab_ids())
        {
            self.sidebar_menu = None;
        }
    }

    /// The open menu is a back or forward menu with `pane`'s folders.
    pub(super) fn menu_lists_history_of(&self, pane: gpui::EntityId) -> bool {
        self.sidebar_menu.as_ref().is_some_and(|m| {
            m.items
                .iter()
                .any(|(_, a)| matches!(a, SidebarAction::HistoryJump { pane: p, .. } if *p == pane))
        })
    }

    /// Removes a saved remote location, and its saved password unless another saved location
    /// uses the same account.
    fn forget_remote(&self, url: &Path, cx: &mut Context<Self>) {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.forget_remote_place(url), cx);
        });
        let Some(gone) = arke_core::remote::RemoteUrl::from_path(url) else {
            return;
        };
        let still_used = SettingsStore::global(cx)
            .read(cx)
            .settings
            .remote_places
            .iter()
            .filter_map(|p| arke_core::remote::RemoteUrl::from_path(&p.url))
            .any(|u| u.same_origin(&gone));
        if !still_used {
            let remotes = Arc::clone(&self.services.remotes);
            crate::explorer::spawn_thread("remote-forget", move || remotes.forget(&gone));
        }
    }

    #[allow(clippy::too_many_lines, reason = "One arm per sidebar action")]
    pub(super) fn run_sidebar_action(
        &mut self,
        action: SidebarAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_menu = None;
        match action {
            SidebarAction::ShelfToHere { .. }
            | SidebarAction::ShelfClear
            | SidebarAction::ShelfRemove(_)
            | SidebarAction::ShelfReveal(_) => self.run_shelf_action(&action, window, cx),
            SidebarAction::OpenRecycleInNewTab => self.open_recycle(true, window, cx),
            SidebarAction::SetRecyclePinned(pinned) => {
                Self::set_recycle_pinned(pinned, cx);
                // Off the sidebar it is still a click away, on Home among the drives (#31).
                if !pinned {
                    self.active_view().update(cx, |e, cx| {
                        e.notice = Some(t!("sidebar.recycle_on_home").into_owned());
                        cx.notify();
                    });
                }
            }
            SidebarAction::SetSidebarPeek(on) => Self::set_sidebar_peek(on, cx),
            SidebarAction::OpenInNewTab(path) => {
                let at = self.active + 1;
                self.open_folder_tab(path, at, true, window, cx);
            }
            SidebarAction::TogglePin(path) => {
                let places = self.active_view().read(cx).places.clone();
                let mut went = None;
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(|st| went = st.toggle_pin(&path, &places), cx);
                });
                if went == Some(crate::settings::PinPlace::List) {
                    crate::workspace::flash_pin(path);
                    self.active_view().update(cx, |e, cx| {
                        e.notice = Some(t!("sidebar.pinned_to_list").into_owned());
                        cx.notify();
                    });
                }
            }
            SidebarAction::MoveToList(path) => {
                let places = self.active_view().read(cx).places.clone();
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(|st| st.move_to_list(&path, &places), cx);
                });
            }
            SidebarAction::MoveToTiles(path) => {
                let places = self.active_view().read(cx).places.clone();
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(
                        |st| {
                            st.move_to_tiles(&path, &places);
                        },
                        cx,
                    );
                });
            }
            SidebarAction::HistoryJump {
                pane,
                back,
                steps,
                path,
            } => {
                self.jump_in_history(pane, back, steps, &path, window, cx);
            }
            SidebarAction::TimelineStep { back, to, .. } => {
                self.timeline_step_to(back, to, window, cx);
            }
            SidebarAction::CloseTab(ix) => self.close_tab(ix, window, cx),
            SidebarAction::DuplicateTab(ix) => self.duplicate_tab(ix, window, cx),
            SidebarAction::CloseOtherTabs(ix) => self.close_other_tabs(ix, window, cx),
            SidebarAction::CloseAllTabs => self.close_all_tabs(window, cx),
            SidebarAction::TogglePinTab(ix) => self.toggle_pin(ix, cx),
            SidebarAction::ReopenClosedTab => {
                self.reopen_closed_tab(&super::ReopenClosedTab, window, cx);
            }
            SidebarAction::Unsplit(ix) => {
                let keep = self.tabs.get(ix).map_or(0, |t| t.active_pane);
                self.unsplit(ix, keep, window, cx);
            }
            SidebarAction::CloseSide(ix) => self.close_side(ix, window, cx),
            SidebarAction::MapDrive => {
                // Free drive letters (D to Z), derived from the drives already listed instead of asking the system.
                let used: Vec<char> = self
                    .active_view()
                    .read(cx)
                    .places
                    .iter()
                    .filter(|p| matches!(p.kind, PlaceKind::Drive { .. }))
                    .filter_map(|p| p.path.to_string_lossy().chars().next())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                let letters = ('D'..='Z').filter(|c| !used.contains(c)).collect();
                self.open_net_dialog(
                    crate::net_dialog::NetDialogKind::Map { letters },
                    None,
                    window,
                    cx,
                );
            }
            SidebarAction::DisconnectDrive(drive) => self.disconnect_drive(drive, cx),
            SidebarAction::EjectDrive(drive) => self.eject_drive(drive, cx),
            SidebarAction::ToggleSearchNetwork(path) => {
                crate::global_search::toggle_network(path, cx);
            }
            SidebarAction::RenamePlace(path) => self.start_place_rename(&path, window, cx),
            SidebarAction::RenameFavorite(path) => self.start_favorite_rename(&path, window, cx),
            SidebarAction::ForgetShare(share) => {
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(|st| st.forget_share(&share), cx);
                });
            }
            SidebarAction::ForgetRemote(url) => self.forget_remote(&url, cx),
            SidebarAction::RequireSecure(url) => {
                crate::remote_shell::revoke_insecure_http(&self.services.remotes, &url, cx);
            }
            SidebarAction::WindowMinimize => window.minimize_window(),
            SidebarAction::WindowMaximize => window.zoom_window(),
            SidebarAction::WindowRestore => {
                if let Some(hwnd) = Explorer::owner_from(window) {
                    crate::win::restore_window(hwnd);
                }
            }
            SidebarAction::WindowClose => window.remove_window(),
            SidebarAction::OpenSettings => {
                self.open_settings(&super::OpenSettings, window, cx);
            }
        }
        cx.notify();
    }

    /// Moves every tab showing a folder on `drive` to the user folder, so nothing in Arke keeps
    /// the drive busy. Used by Arke's own eject and when Windows asks to remove the drive.
    pub(crate) fn leave_drive(&mut self, drive: &Path, cx: &mut Context<Self>) {
        let home = super::home();
        let views: Vec<_> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter().map(|p| p.view.clone()))
            .collect();
        for view in views {
            let on_drive = view
                .read(cx)
                .display_path()
                .is_some_and(|p| on_drive(p, drive));
            if on_drive {
                let home = home.clone();
                // Pinned tabs too: nothing may keep the drive busy.
                view.update(cx, |v, cx| {
                    v.navigate_unlocked(home, arke_core::NavKind::New, cx);
                });
            }
        }
    }

    /// Moves tabs on this drive to the user folder first (Arke's own watchers must not block the eject),
    /// then asks Windows to eject. Reports who holds the drive if it is in use.
    fn eject_drive(&mut self, drive: PathBuf, cx: &mut Context<Self>) {
        self.leave_drive(&drive, cx);
        let shell = Arc::clone(&self.active_view().read(cx).services.shell);
        let view = self.active_view().clone();
        let (tx, rx) = flume::bounded(1);
        let target = drive.clone();
        crate::explorer::spawn_thread("eject", move || {
            // Give tabs time to drop their watchers on this drive.
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = tx.send(shell.eject_drive(&target));
        });
        cx.spawn(async move |this, cx| {
            // Shown in the notice as is; no reply (thread gone) has no reason.
            let result = match rx.recv_async().await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err(String::new()),
            };
            let name = drive.display().to_string();
            let _ = this.update(cx, |this, cx| {
                let notice = match result {
                    Ok(()) => {
                        this.reload_all_places(cx);
                        t!("eject.done", name = name).into_owned()
                    }
                    Err(error) => {
                        log::info!("eject {name} failed: {error}");
                        t!("eject.failed", name = name, error = error).into_owned()
                    }
                };
                view.update(cx, |v, cx| {
                    v.notice = Some(notice);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn disconnect_drive(&mut self, drive: PathBuf, cx: &mut Context<Self>) {
        let shell = Arc::clone(&self.active_view().read(cx).services.shell);
        let view = self.active_view().clone();
        let (tx, rx) = flume::bounded(1);
        let target = drive.clone();
        crate::explorer::spawn_thread("net-disconnect", move || {
            let _ = tx.send(shell.net_disconnect(&target));
        });
        cx.spawn(async move |this, cx| {
            let result = match rx.recv_async().await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err(String::new()),
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.reload_all_places(cx),
                Err(error) => {
                    log::info!("disconnect {} failed: {error}", drive.display());
                    view.update(cx, |v, cx| {
                        v.notice = Some(
                            t!(
                                "net.disconnect_failed",
                                name = drive.display(),
                                error = error
                            )
                            .into_owned(),
                        );
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    /// Windows' own map/disconnect network drive dialog, used when chosen from Arke's dialog.
    /// It has "Reconnect at sign-in", which is left for the user to decide there.
    pub(super) fn system_drive_dialog(
        &mut self,
        map: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shell = Arc::clone(&self.active_view().read(cx).services.shell);
        let owner = Explorer::owner_from(window);
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("net-drive-dialog", move || {
            let result = if map {
                shell.map_drive_dialog(owner)
            } else {
                shell.disconnect_drive_dialog(owner)
            };
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| match rx.recv_async().await {
            Ok(Ok(true)) => {
                let _ = this.update(cx, |this, cx| {
                    for tab in &this.tabs {
                        for pane in &tab.panes {
                            pane.view.read(cx).reload_places();
                        }
                    }
                });
            }
            Ok(Err(err)) => log::warn!("network drive dialog failed: {err}"),
            _ => {}
        })
        .detach();
    }

    /// While the tab, sidebar or history menu is open, what stays in the window: closing it on a
    /// press anywhere in the window. The menu is drawn in a window of its own (`popup.rs`).
    pub(super) fn render_sidebar_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.sidebar_menu.as_mut()?;
        let at = crate::popup::to_screen(window, menu.position);
        menu.popup.ensure(
            || crate::popup::Anchor::At(at.0, at.1),
            |this: &mut Self| this.sidebar_menu.as_mut().map(|m| &mut m.popup),
            Self::sidebar_menu_panel,
            |this, _, cx| {
                this.sidebar_menu = None;
                cx.notify();
            },
            window,
            cx,
        );
        Some(
            div()
                .absolute()
                .size_0()
                .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.sidebar_menu = None;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// The tab, sidebar or history menu's panel, drawn in its own window.
    fn sidebar_menu_panel(
        &mut self,
        max_h: Pixels,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.sidebar_menu.as_ref()?;
        let owner = menu.popup.owner();
        let rows: Vec<AnyElement> = menu
            .items
            .iter()
            .enumerate()
            .map(|(ix, (label, action))| {
                let action = action.clone();
                let folder = matches!(
                    action,
                    SidebarAction::HistoryJump { .. } | SidebarAction::TimelineStep { .. }
                );
                // Where a history folder is, so two folders of the same name tell apart.
                let detail = menu_detail(&action);
                div()
                    .id(("sidebar-menu-item", ix))
                    .h(theme::h_ctl_sm() + theme::s_4())
                    .mx(theme::s_4())
                    .px(theme::s_8())
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .when(folder, |d| {
                        d.child(kind_icon(FileKind::Folder, theme::icon()))
                    })
                    .rounded(theme::r_8())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::wash()))
                    .child(div().flex_none().child(label.clone()))
                    .when_some(detail, |d, detail| {
                        d.child(
                            div()
                                .min_w_0()
                                .max_w(crate::theme::scaled_px(320.))
                                .truncate()
                                .text_size(theme::fs_12())
                                .text_color(theme::text_muted())
                                .child(detail),
                        )
                    })
                    .on_click(crate::popup::listener_in(
                        owner,
                        cx,
                        move |this, _: &ClickEvent, window, cx| {
                            this.drop_stale_menu();
                            if this.sidebar_menu.is_some() {
                                this.run_sidebar_action(action.clone(), window, cx);
                            }
                            cx.notify();
                        },
                    ))
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("sidebar-menu")
                .occlude()
                .min_w(crate::theme::scaled_px(200.))
                .max_h(max_h)
                .overflow_y_scroll()
                .py(theme::s_4())
                .rounded(theme::r_12())
                .bg(theme::overlay())
                .shadow(theme::elev_float())
                .ui_font()
                .text_size(theme::fs_14())
                .text_color(theme::text())
                .children(rows)
                // Like the list's context menu: fades in, again for each new menu.
                .with_animation(
                    gpui::ElementId::Name(format!("sidebar-menu-{:?}", menu.position).into()),
                    crate::motion::menu_enter(),
                    crate::motion::fade_in,
                )
                .into_any_element(),
        )
    }

    /// Right click on the draggable title bar area: Arke's window menu instead of the Windows system menu.
    pub(crate) fn open_window_menu(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let size = if window.is_maximized() {
            (t!("window.restore"), SidebarAction::WindowRestore)
        } else {
            (t!("window.maximize"), SidebarAction::WindowMaximize)
        };
        let items = vec![
            (
                t!("window.minimize").to_string().into(),
                SidebarAction::WindowMinimize,
            ),
            (size.0.to_string().into(), size.1),
            (
                t!("window.close").to_string().into(),
                SidebarAction::WindowClose,
            ),
            (
                t!("settings.menu").to_string().into(),
                SidebarAction::OpenSettings,
            ),
        ];
        self.open_sidebar_menu(position, items, cx);
    }

    // --- Tabs ---

    pub(super) fn tab_menu(&self, ix: usize, cx: &App) -> Vec<(SharedString, SidebarAction)> {
        let pinned = self.tabs.get(ix).is_some_and(|t| t.pinned);
        let split = self.tabs.get(ix).is_some_and(Tab::is_split);
        // The tab's folder can go in the sidebar's favorites (the same as a folder's own menu:
        // the tiles, then the list below once they are full).
        let folder = self
            .tabs
            .get(ix)
            .map(Tab::view)
            .filter(|view| self.page_title(view.entity_id()).is_none())
            .and_then(|view| view.read(cx).display_path().map(Path::to_path_buf))
            .map(|path| {
                let places = &self.active_view().read(cx).places;
                let favorite = SettingsStore::try_global(cx)
                    .is_some_and(|s| s.read(cx).settings.is_pinned(&path, places));
                (
                    if favorite {
                        t!("sidebar.remove_favorite")
                    } else {
                        t!("sidebar.add_favorite")
                    }
                    .to_string()
                    .into(),
                    SidebarAction::TogglePin(path),
                )
            });
        let mut items = vec![
            (
                if pinned {
                    t!("sidebar.unpin_tab")
                } else {
                    t!("sidebar.pin_tab")
                }
                .to_string()
                .into(),
                SidebarAction::TogglePinTab(ix),
            ),
            (
                t!("sidebar.duplicate_tab").to_string().into(),
                SidebarAction::DuplicateTab(ix),
            ),
        ];
        items.extend(folder);
        items.push((
            // Ctrl+W closes the tab only when it isn't split (else the active side).
            if split {
                t!("sidebar.close_tab")
            } else {
                t!("sidebar.close_tab_key")
            }
            .to_string()
            .into(),
            SidebarAction::CloseTab(ix),
        ));
        // Pinned tabs are spared by both, so offer them only when they would close something.
        let others = self
            .tabs
            .iter()
            .enumerate()
            .any(|(i, t)| i != ix && !t.pinned);
        if others {
            items.push((
                t!("sidebar.close_other_tabs").to_string().into(),
                SidebarAction::CloseOtherTabs(ix),
            ));
        }
        if self.tabs.iter().any(|t| !t.pinned) && self.tabs.len() > 1 {
            items.push((
                t!("sidebar.close_all_tabs").to_string().into(),
                SidebarAction::CloseAllTabs,
            ));
        }
        if !self.closed.is_empty() {
            items.push((
                t!("sidebar.reopen_closed_tab").to_string().into(),
                SidebarAction::ReopenClosedTab,
            ));
        }
        if split {
            items.push((
                t!("sidebar.unsplit").to_string().into(),
                SidebarAction::Unsplit(ix),
            ));
            items.push((
                t!("sidebar.close_side").to_string().into(),
                SidebarAction::CloseSide(ix),
            ));
        }
        items
    }

    /// Dual-pane tab: one row split in half, the active half tinted; clicking a half focuses that pane.
    fn tab_halves(
        ix: usize,
        active_pane: Option<usize>,
        titles: Vec<SharedString>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        titles
            .into_iter()
            .enumerate()
            .map(|(pane, title)| {
                div()
                    .id(("tab-half", ix * 2 + pane))
                    .flex_1()
                    .min_w_0()
                    .h(theme::h_ctl_sm())
                    .px(theme::s_6())
                    .flex()
                    .items_center()
                    .gap(theme::s_6())
                    .rounded(theme::r_4())
                    .when(active_pane == Some(pane), |d| d.bg(theme::signal_wash()))
                    .child(kind_icon(FileKind::Folder, theme::icon()))
                    .child(div().min_w_0().truncate().child(title))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.activate_pane(ix, pane, window, cx);
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn tab_pin_button(ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id(("tab-pin", ix))
            .a11y_button(t!("sidebar.unpin_tab"))
            .size(theme::s_20())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_4())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Pin.path())
                    .size(theme::icon_sm())
                    .text_color(theme::text_subtle()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.toggle_pin(ix, cx);
            }))
            .tooltip(crate::workspace::tooltip(
                t!("sidebar.unpin_tab").to_string(),
            ))
    }

    fn tab_close_button(ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id(("tab-close", ix))
            .a11y_button(t!("sidebar.close_tab"))
            .size(theme::s_20())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_4())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme::icon_sm())
                    .text_color(theme::text_subtle()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.close_tab(ix, window, cx);
            }))
    }

    /// One tab. `horizontal` uses the top tab strip style (wider, joined to the paper at the bottom).
    #[allow(
        clippy::too_many_lines,
        reason = "one tab's look, drag and menu together"
    )]
    pub(super) fn render_tab(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let tab: &Tab = &self.tabs[ix];
        let active = ix == self.active;
        let split = tab.is_split();
        let group = SharedString::from(format!("tab-{ix}"));
        // Tabs showing Home, Settings or the Recycle Bin use that page's name.
        let titles: Vec<SharedString> = tab
            .panes
            .iter()
            .map(|p| {
                self.page_title(p.view.entity_id())
                    .map_or_else(|| p.view.read(cx).tab_title(), |(title, _)| title.into())
            })
            .collect();
        let close_button = Self::tab_close_button(ix, cx);
        let row = div()
            .id(("tab", ix))
            .debug_selector(|| format!("tab-{ix}"))
            .group(group.clone())
            .h(if split {
                theme::h_row() + theme::s_8()
            } else if tab.pinned {
                // Pinned tabs are a tighter group at the top.
                theme::h_row_compact()
            } else {
                theme::h_row()
            })
            .when(tab.pinned, |d| d.text_size(theme::fs_13()))
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_4())
            .pl(if split { theme::s_4() } else { theme::s_8() })
            .pr(theme::s_6())
            .rounded(theme::r_8())
            .cursor_pointer()
            // Slides from the old position after a reorder (the offset is relative).
            .relative()
            // The active tab shares the paper surface with the list.
            .when(active, |d| d.child(here_layer()).text_color(theme::text()))
            // Asked for again in the background (middle click): fades out from the accent color.
            .when_some(self.tab_flash(ix), |d, serial| {
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(theme::r_8())
                        .with_animation(
                            ("tab-flash", serial as usize),
                            gpui::Animation::new(crate::motion::FLASH),
                            |el, t| el.bg(theme::blend(theme::signal_wash(), theme::clear(), t)),
                        ),
                )
            })
            .when(!active, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
            })
            // Dragging another tab over this one reorders live (workspace_tabdrag.rs); dropping on the right half
            // of the content area opens it side by side. Files dropped on a tab go into its current folder (#32).
            .when_some(self.tab_drop_folder(ix, cx), |d, dest| {
                self.with_tab_file_drop(ix, dest, d)
            })
            // Position was already updated during the drag.
            .on_drop(cx.listener(move |this, _: &DraggedTab, _, cx| {
                this.dragging_tab = None;
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.activate(ix, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    // Pinned tabs don't close by accident; unpin first.
                    if !this.tabs.get(ix).is_some_and(|t| t.pinned) {
                        this.close_tab(ix, window, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    let items = this.tab_menu(ix, cx);
                    this.open_sidebar_menu(e.position, items, cx);
                }),
            );
        // Title on the drag preview; both sides for dual pane.
        let drag_title: SharedString = titles
            .iter()
            .map(SharedString::as_ref)
            .collect::<Vec<_>>()
            .join(" | ")
            .into();
        let workspace = cx.entity().downgrade();
        let row = row
            .a11y(gpui::Role::Tab, drag_title.clone())
            .aria_selected(active);
        let row = row.on_drag(
            DraggedTab {
                ix,
                title: drag_title,
            },
            move |dragged, _, _, cx| {
                let _ = workspace.update(cx, |ws, cx| {
                    ws.dragging_tab = Some(dragged.ix);
                    cx.notify();
                });
                cx.new(|_| dragged.clone())
            },
        );
        let row = if split {
            row.children(Self::tab_halves(
                ix,
                active.then_some(tab.active_pane),
                titles,
                cx,
            ))
        } else {
            let title = titles.into_iter().next().unwrap_or_default();
            // Home, Settings and Recycle Bin use their own page icon.
            let icon = match self.page_title(tab.view().entity_id()) {
                Some((_, icon)) => svg()
                    .path(icon.path())
                    .size(theme::icon())
                    .flex_none()
                    .text_color(theme::text_muted())
                    .into_any_element(),
                None => kind_icon(FileKind::Folder, theme::icon()),
            };
            row.gap(theme::s_8())
                .child(icon)
                .child(div().flex_1().min_w_0().truncate().child(title))
        };
        let row = if tab.pinned {
            // Pinned: a pin instead of the close button, always shown; clicking it unpins.
            row.child(Self::tab_pin_button(ix, cx))
        } else {
            row.child(
                // Close button only on the active tab or on hover.
                div()
                    .flex_none()
                    .when(!active, |d| {
                        d.invisible().group_hover(group, gpui::Styled::visible)
                    })
                    .child(close_button),
            )
        };
        let last_pinned = tab.pinned && self.tabs.get(ix + 1).is_none_or(|t| !t.pinned);
        let shell = self.tab_drag_shell(ix, row, cx);
        if last_pinned && !Self::horizontal_tabs(cx) && ix + 1 < self.tabs.len() {
            // A hairline under the pinned group sets it apart from the other tabs.
            div()
                .flex()
                .flex_col()
                .gap(theme::s_2())
                .child(shell)
                .child(
                    div()
                        .h(px(1.))
                        .mx(theme::s_8())
                        .my(theme::s_2())
                        .bg(theme::line()),
                )
                .into_any_element()
        } else {
            shell
        }
    }
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn toggle_expand_for_test(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.toggle_expand(path, cx);
    }

    pub(crate) fn tree_expanded_for_test(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub(crate) fn tree_children_for_test(&self, path: &Path) -> Option<Vec<PathBuf>> {
        self.tree_children.get(path).cloned().flatten()
    }

    pub(crate) fn sidebar_here_for_test(&self, cx: &App) -> Option<PathBuf> {
        self.sidebar_here(cx)
    }

    /// A quick access tile or tree row was clicked.
    pub(crate) fn sidebar_open_for_test(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_active(path, window, cx);
    }

    /// A sidebar folder's "Open in new tab" (also Ctrl+click on it).
    pub(crate) fn sidebar_open_in_new_tab_for_test(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_sidebar_action(SidebarAction::OpenInNewTab(path), window, cx);
    }

    /// Labels of tab `ix`'s context menu.
    /// Opens tab `ix`'s context menu, as a right click on the tab does.
    pub(crate) fn open_tab_menu_for_test(&mut self, ix: usize, cx: &mut Context<Self>) {
        let items = self.tab_menu(ix, cx);
        self.open_sidebar_menu(
            gpui::point(crate::theme::scaled_px(100.), crate::theme::scaled_px(100.)),
            items,
            cx,
        );
    }

    /// The window the open tab, sidebar or history menu is drawn in.
    pub(crate) fn sidebar_menu_window_for_test(&self) -> Option<gpui::AnyWindowHandle> {
        self.sidebar_menu.as_ref()?.popup.window()
    }

    pub(crate) fn tab_menu_labels_for_test(&self, ix: usize, cx: &App) -> Vec<String> {
        self.tab_menu(ix, cx)
            .into_iter()
            .map(|(label, _)| label.to_string())
            .collect()
    }

    /// Picks the entry labeled `label` in tab `ix`'s context menu.
    pub(crate) fn run_tab_menu_for_test(
        &mut self,
        ix: usize,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, action)) = self
            .tab_menu(ix, cx)
            .into_iter()
            .find(|(l, _)| l.as_ref() == label)
        else {
            panic!("no {label:?} in the tab menu");
        };
        self.run_sidebar_action(action, window, cx);
    }

    pub(crate) fn tree_is_leaf_for_test(&self, path: &Path) -> bool {
        self.tree_leaf.contains(path)
    }

    /// The entries of the menu a right click on a favorite (tile or list row) opens.
    pub(crate) fn favorite_menu_labels_for_test(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        self.favorite_menu(path, cx)
            .into_iter()
            .map(|(label, _)| label.to_string())
            .collect()
    }

    /// "Rename" picked from a favorite's menu.
    pub(crate) fn rename_favorite_from_menu_for_test(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_sidebar_action(
            SidebarAction::RenameFavorite(path.to_path_buf()),
            window,
            cx,
        );
    }

    /// The box a favorite is being renamed in.
    pub(crate) fn favorite_rename_for_test(&self, path: &Path) -> Option<Entity<TextInput>> {
        self.favorite_rename_box(path)
    }

    /// "Remove from sidebar" picked from the Recycle Bin row's menu.
    pub(crate) fn unpin_recycle_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_sidebar_action(SidebarAction::SetRecyclePinned(false), window, cx);
    }

    /// How much bigger a dragged tile is drawn under the pointer.
    pub(crate) const fn tile_lift_for_test() -> f32 {
        TILE_LIFT
    }

    /// Scrolls the sidebar's places `y` down from the top.
    pub(crate) fn scroll_places_for_test(&mut self, y: Pixels, cx: &mut Context<Self>) {
        self.sidebar_ui
            .places_scroll
            .set_offset(gpui::point(px(0.), -y));
        cx.notify();
    }
}

/// Tree row label. Drives add their free space on the right and a usage bar under both, which
/// starts at the label's left edge and fits inside the row (its height matches the other rows).
fn tree_label(label: String, space: Option<DriveSpace>) -> impl IntoElement {
    let Some(space) = space else {
        return div()
            .flex_1()
            .min_w_0()
            .truncate()
            .pl(theme::s_4())
            .child(label);
    };
    div()
        .flex_1()
        .min_w_0()
        .pl(theme::s_4())
        .flex()
        .flex_col()
        .gap(theme::s_2())
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(
                    div()
                        .flex_none()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(space.free),
                ),
        )
        .child(
            div()
                .h(usage_bar_h())
                .rounded(theme::R_FULL)
                .bg(theme::line())
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(relative(space.used))
                        .rounded(theme::R_FULL)
                        .bg(if space.used > USAGE_DANGER {
                            theme::danger()
                        } else {
                            theme::text_subtle()
                        }),
                ),
        )
}

/// Highlight for the current location (favorite tile, tab): a paper surface. Background and border
/// are drawn on this layer, text above it. It moves at once when the tab or folder changes, like a
/// selection (no fade: a tab or folder switch never waits on an animation).
fn here_layer() -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .rounded(theme::r_8())
        .bg(theme::paper())
        // The outline goes inside: a shadow outside is cut off where a tile or a row touches the
        // edge of the scrolling sidebar (#220).
        .border_1()
        .border_color(theme::line())
        .into_any_element()
}

/// Whether `path` is on the drive whose root is `drive` (letters in any case).
fn on_drive(path: &Path, drive: &Path) -> bool {
    arke_core::watches::volume_key(path)
        .is_some_and(|v| arke_core::watches::volume_key(drive) == Some(v))
}

/// Windows is removing or dismounting `drive` (from the taskbar or another app): every window moves
/// its tabs off it.
pub(crate) fn leave_drive_everywhere(drive: &Path, cx: &mut App) {
    for window in cx.windows() {
        if let Some(workspace) = window.downcast::<Workspace>() {
            let _ = workspace.update(cx, |ws, _, cx| ws.leave_drive(drive, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(path: &str, kind: PlaceKind) -> Place {
        Place {
            label: path.to_owned(),
            path: PathBuf::from(path),
            kind,
        }
    }

    fn drive(path: &str, remote: bool) -> Place {
        place(
            path,
            PlaceKind::Drive {
                remote,
                space: None,
            },
        )
    }

    #[test]
    fn the_tree_opens_from_the_innermost_root_down_to_the_parent() {
        let places = [
            drive(r"C:\", false),
            place(r"C:\Users\me", PlaceKind::Folder(KnownFolder::Profile)),
            place(
                r"C:\Users\me\Desktop",
                PlaceKind::Folder(KnownFolder::Desktop),
            ),
            drive(r"Z:\", true),
        ];
        let paths = |chain: Vec<PathBuf>| -> Vec<String> {
            chain.iter().map(|p| p.display().to_string()).collect()
        };
        assert_eq!(
            paths(follow_chain(&places, Path::new(r"C:\work\reports\2026"))),
            [r"C:\", r"C:\work", r"C:\work\reports"]
        );
        // The user folder is under This PC as well, and nearer.
        assert_eq!(
            paths(follow_chain(&places, Path::new(r"C:\Users\me\Desktop\a"))),
            [r"C:\Users\me", r"C:\Users\me\Desktop"]
        );
        assert!(
            follow_chain(&places, Path::new(r"C:\")).is_empty(),
            "a root"
        );
        assert!(
            follow_chain(&places, Path::new(r"Z:\docs")).is_empty(),
            "network drives are under Network"
        );
        assert!(follow_chain(&places, Path::new(r"\nas\docs\a")).is_empty());
    }

    #[test]
    fn a_favorite_is_out_once_past_the_sidebars_column() {
        let sidebar = Bounds::new(gpui::point(px(0.), px(80.)), gpui::size(px(240.), px(700.)));
        let window = gpui::size(px(1200.), px(900.));
        let at = |x: f32, y: f32| gpui::point(px(x), px(y));
        assert!(
            !pin_outside(sidebar, at(120., 400.), window),
            "over the sidebar"
        );
        assert!(
            !pin_outside(sidebar, at(120., 20.), window),
            "over the rows above it (Home), in its column"
        );
        assert!(
            pin_outside(sidebar, at(241., 400.), window),
            "over the content"
        );
        assert!(
            pin_outside(sidebar, at(-5., 400.), window),
            "left of the window"
        );
        assert!(
            pin_outside(sidebar, at(120., 910.), window),
            "below the window"
        );
    }

    #[test]
    fn long_names_leave_the_tiles_without_moving_the_others() {
        // A name fits when it is at most 8 characters three a row, 5 four a row.
        let mut fits = |label: &str, per_row: usize| {
            let room = if per_row == 3 { 8 } else { 5 };
            label.len() <= room
        };
        let layout = split_tiles(&["Desktop", "Quarterly reports", "Music"], &mut fits);
        assert_eq!(
            layout,
            TileLayout {
                large: true,
                on_tile: vec![true, false, true],
            }
        );
        // Seven short names shrink to four a row.
        let seven = ["a", "b", "c", "d", "e", "f", "g"];
        assert!(!split_tiles(&seven, &mut fits).large);
        // But not when that would cut one off: they stay three a row instead.
        let seven = ["a", "b", "c", "d", "e", "f", "Desktop"];
        let layout = split_tiles(&seven, &mut fits);
        assert!(layout.large);
        assert!(layout.on_tile.iter().all(|&tile| tile));
    }
}
