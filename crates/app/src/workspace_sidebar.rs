//! Sidebar: favorite tiles, pins, This PC (an expandable folder tree), and tabs.
//!
//! Tree children are listed on a background thread (`Fs::read_dir`); the UI thread only renders results.
//! The tree under This PC follows the active pane's folder (#241): the folders down to it expand
//! by themselves and close again once it moves elsewhere.
//! Favorites, This PC, Network and Tabs each collapse to their heading (remembered); Home and its
//! tiles don't. Home, Favorites and This PC can be hidden from their heading's menu (Home's from
//! its row's) and shown again from the menu of the sidebar's empty space (#323). Tiles drag like
//! app icons on a phone (`workspace_tabdrag.rs`). A favorite (tile or list entry) dragged out of
//! the sidebar and let go there leaves the favorites, like an app dragged off the macOS Dock, with
//! Undo on a toast.
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
use super::sidebar_keys::{Expand, NavRow};
use super::tabdrag::PlaceRow;
use super::{DraggedTab, Tab, Workspace};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{DraggedFiles, Explorer, drop_external_into, drop_into, kind_icon};
use crate::place_names::share_label;
use crate::settings::{FAVORITES_MAX, PinSlot, SettingsStore, SidebarSection};
use crate::shell::{KnownFolder, Place, PlaceKind};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};
use crate::workspace::{Tooltip, TooltipBelowExt as _};
use std::rc::Rc;

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
    /// Free and total bytes, for the row's tooltip.
    bytes: (u64, u64),
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
    /// Hide a section of the sidebar (its heading's menu, #323).
    HideSection(SidebarSection),
    /// A section's row in the menu of the sidebar's empty space, checked while it is shown
    /// (`true`): picked, it is hidden or shown again (#323).
    ShowSection(SidebarSection, bool),
    /// Copy or move the shelf's items into the active tab's folder.
    ShelfToHere {
        is_move: bool,
    },
    ShelfClear,
    ShelfRemove(PathBuf),
    /// Go to a shelf item's folder and select it.
    ShelfReveal(PathBuf),
    /// A file (a recent file on Home): open it, go to it in its folder, copy its path, show its
    /// properties.
    OpenFile(PathBuf),
    RevealFile(PathBuf),
    CopyPath(PathBuf),
    /// Properties of a folder (`true`: no need to ask the disk) or a file.
    Properties(PathBuf, bool),
    /// A drive (or any folder): Windows Terminal there, File Explorer there.
    OpenTerminal(PathBuf),
    OpenInExplorer(PathBuf),
    /// Windows' Format dialog for a drive.
    FormatDrive(PathBuf),
    /// Windows' Disk Cleanup for a drive.
    DiskCleanup(PathBuf),
}

/// One row of a sidebar or Home menu.
pub(super) type SidebarRow = (SharedString, SidebarAction);

fn row(label: std::borrow::Cow<'_, str>, action: SidebarAction) -> SidebarRow {
    (label.into_owned().into(), action)
}

/// Where a folder's menu was opened, for [`Workspace::folder_menu`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuFor {
    /// A row of the tree under This PC or Network, or a drive or network card on Home; `depth` 0
    /// is the place itself.
    Tree { depth: usize },
    /// A favorite in the sidebar: a tile, a row of a name too long for one, a pinned list row.
    Favorite,
    /// A favorite card on Home (Home shows the tiles only).
    HomeFavorite,
}

/// The icon on the left of a menu row, as the file list's menu has one on each of Arke's own
/// rows. `None`: the row keeps the icon's room empty.
const fn menu_glyph(action: &SidebarAction) -> Option<Icon> {
    use SidebarAction as A;
    Some(match action {
        A::OpenInNewTab(_) | A::OpenRecycleInNewTab => Icon::TabOpen,
        A::TogglePin(_) | A::SetRecyclePinned(_) | A::TogglePinTab(_) => Icon::Pin,
        A::MoveToList(_) => Icon::ViewList,
        A::MoveToTiles(_) => Icon::ViewGrid,
        A::RenamePlace(_) | A::RenameFavorite(_) => Icon::Rename,
        A::OpenTerminal(_) => Icon::Terminal,
        A::OpenInExplorer(_) => Icon::Explorer,
        A::CopyPath(_) => Icon::CopyPath,
        A::Properties(..) => Icon::Info,
        A::OpenFile(_) => Icon::Open,
        A::RevealFile(_) | A::ShelfReveal(_) | A::HistoryJump { .. } | A::TimelineStep { .. } => {
            Icon::Folder
        }
        A::EjectDrive(_) => Icon::ArrowUp,
        A::FormatDrive(_) => Icon::Drive,
        A::DiskCleanup(_) => Icon::Trash,
        A::DisconnectDrive(_) => Icon::Power,
        A::MapDrive => Icon::NetDrive,
        A::ToggleSearchNetwork(_) => Icon::Search,
        A::RequireSecure(_) => Icon::Globe,
        A::ForgetShare(_)
        | A::ForgetRemote(_)
        | A::ShelfRemove(_)
        | A::ShelfClear
        | A::CloseTab(_)
        | A::CloseSide(_) => Icon::Close,
        A::DuplicateTab(_) => Icon::TabNew,
        A::ReopenClosedTab => Icon::Restore,
        A::Unsplit(_) => Icon::Split,
        A::ShelfToHere { is_move: false } => Icon::Copy,
        A::ShelfToHere { is_move: true } => Icon::Cut,
        A::WindowMinimize => Icon::WinMin,
        A::WindowMaximize => Icon::WinMax,
        A::WindowRestore => Icon::WinRestore,
        A::WindowClose => Icon::WinClose,
        A::OpenSettings => Icon::Settings,
        A::SetSidebarPeek(_) => Icon::Sidebar,
        A::HideSection(_) => Icon::EyeOff,
        A::ShowSection(_, true) => Icon::Check,
        A::ShowSection(_, false) | A::CloseOtherTabs(_) | A::CloseAllTabs => return None,
    })
}

/// Where the places' top edge goes so it doesn't cut a row of favorite tiles in half, all in the
/// list's unscrolled terms: the edge at `top` now; the tiles from `tiles.0` to `tiles.1` in
/// `rows` rows `gap` apart; the current folder's row from `here.0` to `here.1`, which must stay
/// in a view `view_h` high; the edge going no further than `max_top`. Back to the start of the cut
/// row first, else on to the next row (past the tiles when it is the last one: scrolling up shows
/// them, and the shadow under Home says there is more above). Never a half row. `None`: leave it
/// as it is.
fn tile_snap(
    top: f32,
    tiles: (f32, f32),
    rows: usize,
    gap: f32,
    here: (f32, f32),
    view_h: f32,
    max_top: f32,
) -> Option<f32> {
    if rows == 0 || top <= tiles.0 || top >= tiles.1 {
        return None;
    }
    let period = (tiles.1 - tiles.0 + gap) / rows as f32;
    let into = (top - tiles.0).rem_euclid(period);
    // In the gap between two rows, or a hair into one: nothing is cut.
    if into < 1. || into > period - gap - 1. {
        return None;
    }
    let shows_here = |t: f32| t <= max_top + 0.5 && here.0 >= t - 0.5 && here.1 <= t + view_h + 0.5;
    [top - into, (top + period - into).min(tiles.1)]
        .into_iter()
        .find(|&t| shows_here(t))
}

/// A line goes above row `ix`: it starts another group than the row above.
fn line_before(items: &[SidebarRow], ix: usize) -> bool {
    ix.checked_sub(1)
        .and_then(|above| items.get(above))
        .zip(items.get(ix))
        .is_some_and(|((_, a), (_, b))| menu_group(a) != menu_group(b))
}

/// Rows of one group stay together and a line parts the groups, as in the file list's menu: a
/// menu is built in this order, so the lines fall where the kind of action changes.
const fn menu_group(action: &SidebarAction) -> u8 {
    use SidebarAction as A;
    match action {
        A::OpenInNewTab(_)
        | A::OpenRecycleInNewTab
        | A::OpenFile(_)
        | A::RevealFile(_)
        | A::ShelfReveal(_) => 0,
        A::TogglePin(_)
        | A::SetRecyclePinned(_)
        | A::MoveToList(_)
        | A::MoveToTiles(_)
        | A::RenamePlace(_)
        | A::RenameFavorite(_) => 1,
        A::DisconnectDrive(_)
        | A::EjectDrive(_)
        | A::FormatDrive(_)
        | A::DiskCleanup(_)
        | A::ToggleSearchNetwork(_)
        | A::ForgetShare(_)
        | A::ForgetRemote(_)
        | A::RequireSecure(_)
        | A::MapDrive => 2,
        A::OpenTerminal(_) | A::OpenInExplorer(_) | A::CopyPath(_) | A::Properties(..) => 3,
        A::TogglePinTab(_) | A::DuplicateTab(_) => 4,
        A::CloseTab(_) | A::CloseOtherTabs(_) | A::CloseAllTabs | A::ReopenClosedTab => 5,
        A::Unsplit(_) | A::CloseSide(_) => 6,
        A::WindowMinimize | A::WindowMaximize | A::WindowRestore | A::WindowClose => 7,
        A::OpenSettings => 8,
        A::ShelfToHere { .. } => 9,
        A::ShelfClear | A::ShelfRemove(_) => 10,
        A::HistoryJump { .. } | A::TimelineStep { .. } => 11,
        A::SetSidebarPeek(_) => 12,
        A::HideSection(_) | A::ShowSection(..) => 13,
    }
}

/// Whether `path` is the drive Windows runs from (`%SystemDrive%`).
fn is_system_drive(path: &Path) -> bool {
    std::env::var("SystemDrive").is_ok_and(|d| {
        path.to_string_lossy()
            .trim_end_matches('\\')
            .eq_ignore_ascii_case(d.trim_end_matches('\\'))
    })
}

/// The menu of a recent file on Home. "Open file location" goes to it in Arke, as the same row
/// does for a shortcut in the file list (File Explorer's rows say "Windows File Explorer").
pub(super) fn file_menu(path: &Path) -> Vec<SidebarRow> {
    let path = path.to_path_buf();
    vec![
        row(t!("menu.open"), SidebarAction::OpenFile(path.clone())),
        row(
            t!("menu.open_file_location"),
            SidebarAction::RevealFile(path.clone()),
        ),
        row(t!("menu.copy_path"), SidebarAction::CopyPath(path.clone())),
        row(
            t!("menu.properties"),
            SidebarAction::Properties(path, false),
        ),
    ]
}

/// How much bigger a dragged tile is drawn under the pointer than in its slot: lifted off the grid.
pub(super) const TILE_LIFT: f32 = 1.1;

/// A favorite tile's height: the large ones (three a row) are taller.
pub(super) fn tile_height(large: bool) -> Pixels {
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
    #[allow(
        clippy::too_many_lines,
        reason = "one card in two shapes, a tile and a row, and the change between them"
    )]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (out, tile_now, morph, cell) =
            self.workspace
                .upgrade()
                .map_or((false, self.pin.from_tile, 0, None), |ws| {
                    let drag = &ws.read(cx).tab_drag;
                    (
                        drag.pin_out(),
                        drag.is_tile_now(),
                        drag.morph(),
                        drag.tile_size(),
                    )
                });
        let pin = &self.pin;
        // The favorite is a tile while over the tiles, a row elsewhere: its copy takes that shape
        // (#278 follow-up). A list entry over the tiles takes a tile cell's size.
        let look = pin.look.or_else(|| {
            cell.map(|size| TileLook {
                size,
                large: size.height >= tile_height(true) - px(1.),
                here: false,
            })
        });
        let lift = TILE_LIFT;
        // The row: as tall as a list row, as wide as its icon and name; taken as a tile, placed
        // with the pointer on its icon.
        let label_w = {
            let run = gpui::TextRun {
                len: pin.label.len(),
                font: theme::ui_font_at(gpui::FontWeight::NORMAL),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(pin.label.clone(), theme::fs_14(), &[run], None)
                .width
        };
        let row_w = (theme::s_8() * 2. + theme::icon() + theme::s_8() + label_w).min(px(280.));
        let row_h = theme::h_row();
        let (row_x, row_y) = if pin.from_tile {
            (
                self.grab.x - theme::s_16(),
                self.grab.y - theme::h_row() / 2.,
            )
        } else {
            (px(0.), px(0.))
        };
        // The tile: lifted a little bigger; taken as a tile, placed as it was taken hold of
        // (growing around the grab point), a row turned tile centered on the pointer.
        let tile = look.map(|look| {
            let (w, h) = (look.size.width * lift, look.size.height * lift);
            let (x, y) = if pin.from_tile {
                (-(self.grab.x * (lift - 1.)), -(self.grab.y * (lift - 1.)))
            } else {
                (self.grab.x - w / 2., self.grab.y - h / 2.)
            };
            (look, x, y, w, h)
        });
        // How much of a tile it is: 1 a tile, 0 a row. Changing shape since it was taken, it
        // moves from the other one over [`MORPH`]; a shape it starts in shows at once.
        let target = if tile_now && tile.is_some() { 1. } else { 0. };
        let animate = morph > 0 && crate::motion::enabled() && tile.is_some();
        let (tx, ty, tw, th) =
            tile.map_or((row_x, row_y, row_w, row_h), |(_, x, y, w, h)| (x, y, w, h));
        let large = tile.is_some_and(|(look, ..)| look.large);
        let here = tile.is_some_and(|(look, ..)| look.here);
        let (gap, icon) = if large {
            (theme::s_4(), theme::s_20())
        } else {
            (theme::s_2(), theme::scaled_px(18.))
        };
        let icon_color = if here {
            theme::signal()
        } else {
            theme::text_muted()
        };
        let lerp = |a: Pixels, b: Pixels, k: f32| a + (b - a) * k;
        let radius = (theme::r_8() * lift, theme::r_8());
        // One card, the same surface in both shapes; what is inside fades from one layout to
        // the other while the card changes size.
        // Both fill the card as it changes size, so what fades out stays centered in it.
        let tile_face = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(gap * lift)
            .text_size(theme::fs_12() * lift)
            .debug_selector(|| "tile-lifted".to_owned())
            .child(crate::assets::tinted(pin.icon, icon * lift, icon_color))
            .child(
                div()
                    .max_w_full()
                    .px(theme::s_4() * lift)
                    .truncate()
                    .child(pin.label.clone()),
            );
        let row_face = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .px(theme::s_8())
            .flex()
            .items_center()
            .gap(theme::s_8())
            .text_size(theme::fs_14())
            .debug_selector(|| "pin-chip".to_owned())
            .child(crate::assets::tinted(pin.icon, theme::icon(), icon_color))
            .child(div().min_w_0().truncate().child(pin.label.clone()));
        let card = |k: f32| {
            div()
                .absolute()
                .left(lerp(row_x, tx, k))
                .top(lerp(row_y, ty, k))
                .w(lerp(row_w, tw, k))
                .h(lerp(row_h, th, k))
                .overflow_hidden()
                .rounded(lerp(radius.1, radius.0, k))
                // Opaque under the hover wash: it floats over rows, and a shadow under a
                // see-through surface would darken it.
                .bg(theme::frame_solid())
                .shadow(theme::elev_float())
                .ui_font()
                .text_color(theme::text())
                .child(div().absolute().top_0().left_0().size_full().bg(theme::wash_strong()))
                .when(out, |d| d.opacity(PIN_OUT_OPACITY))
        };
        let id = usize::try_from(morph).unwrap_or_default();
        let shape = if animate {
            // How much of a tile it is at `t`: the faces fade evenly, the card springs into its
            // new shape like jelly, its height a moment ahead of its width (squashed, then
            // stretched) and both passing the shape a little before they settle. Kept around the
            // same center, so it wobbles in place under the pointer.
            let toward = move |k: f32| if target > 0.5 { k } else { 1. - k };
            let fade = move |t: f32| toward(crate::motion::ease_out(t));
            let lag = |t: f32| ((t - 0.08) / 0.92).max(0.);
            let ease = || gpui::Animation::new(MORPH).with_easing(crate::motion::ease_out);
            let from_c = (row_x + row_w / 2., row_y + row_h / 2.);
            let to_c = (tx + tw / 2., ty + th / 2.);
            card(1. - target)
                .child(
                    tile_face.with_animation(("pin-morph-tile", id), ease(), move |el, t| {
                        el.opacity(fade(t))
                    }),
                )
                .child(
                    row_face.with_animation(("pin-morph-row", id), ease(), move |el, t| {
                        el.opacity(1. - fade(t))
                    }),
                )
                .with_animation(
                    ("pin-morph", id),
                    gpui::Animation::new(MORPH),
                    move |el, t| {
                        let kh = toward(crate::motion::spring(t));
                        let kw = toward(crate::motion::spring(lag(t)));
                        let place = toward(crate::motion::ease_out(t));
                        let w = lerp(row_w, tw, kw).max(px(8.));
                        let h = lerp(row_h, th, kh).max(px(8.));
                        let center_x = lerp(from_c.0, to_c.0, place);
                        let center_y = lerp(from_c.1, to_c.1, place);
                        el.left(center_x - w / 2.)
                            .top(center_y - h / 2.)
                            .w(w)
                            .h(h)
                            .rounded(lerp(radius.1, radius.0, kh.clamp(0., 1.)))
                    },
                )
                .into_any_element()
        } else if target > 0.5 {
            card(1.).child(tile_face).into_any_element()
        } else {
            card(0.).child(row_face).into_any_element()
        };
        // The trash badge sits on the corner of the shape it ends in.
        let (bx, by, bw) = if target > 0.5 {
            (tx, ty, tw)
        } else {
            (row_x, row_y, row_w)
        };
        div()
            .relative()
            .child(shape)
            .children(out.then(|| {
                div()
                    .absolute()
                    .left(bx)
                    .top(by)
                    .w(bw)
                    .child(remove_badge())
            }))
            .into_any_element()
    }
}

/// How long a dragged favorite takes to change shape between a row and a tile, wobble included
/// (`motion::spring` has settled well before the end).
const MORPH: std::time::Duration = std::time::Duration::from_millis(340);

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
        ws.tab_drag.grid_hold = dragged
            .from_tile
            .then(|| ws.sidebar_ui.grid_height.get())
            .flatten();
        let settings = &SettingsStore::global(cx).read(cx).settings;
        ws.tab_drag.origin = Some(super::tabdrag::PinSnapshot {
            favorites: settings.favorites.clone(),
            pinned: settings.pinned.clone(),
        });
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

/// The places listed under This PC. Network drives go under Network, unless Network is hidden
/// (`mapped`): then they stay in reach under This PC, as File Explorer lists them (#387).
const fn in_this_pc(kind: PlaceKind, mapped: bool) -> bool {
    match kind {
        PlaceKind::Drive { remote: true, .. } => mapped,
        _ => matches!(
            kind,
            PlaceKind::Folder(KnownFolder::Profile)
                | PlaceKind::Cloud
                | PlaceKind::Drive { remote: false, .. }
                | PlaceKind::Wsl
                | PlaceKind::Phone
        ),
    }
}

/// How many places This PC lists.
fn this_pc_count(places: &[Place], mapped: bool) -> usize {
    places.iter().filter(|p| in_this_pc(p.kind, mapped)).count()
}

/// The folders the tree opens to show `here` (#241): from the This PC root that holds it (the
/// innermost one, so the user folder wins over its drive) down to its parent. Empty when `here`
/// is a root itself or isn't under This PC.
fn follow_chain(places: &[Place], here: &Path, mapped: bool) -> Vec<PathBuf> {
    let Some(root) = places
        .iter()
        .filter(|p| in_this_pc(p.kind, mapped) && here.starts_with(&p.path))
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

/// How many favorite tiles share a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TileLayout {
    per_row: usize,
}

impl TileLayout {
    /// Three or fewer a row: the taller tiles with the larger icon.
    pub(super) const fn large(self) -> bool {
        self.per_row <= 3
    }

    pub(super) const fn per_row(self) -> usize {
        self.per_row
    }
}

/// The narrowest a tile's name gets before it is cut to a few letters ("Descargas", "EVO1 (C:)",
/// four or five CJK characters), at 100%.
const TILE_NAME_MIN: f32 = 56.;

/// Lays out `count` favorite tiles in a places list `list` wide: never more a row than leave each
/// tile room for a readable name, four at most, two at least (#371: eight favorites went four a
/// row in the default sidebar, their names cut to "Des…"). Where both four and three fit, the one
/// whose last row is fuller: six go three and three, eight four and four. Every favorite is a
/// tile: where a folder goes (the tiles or the list below) is the user's choice, by dragging it or
/// "Move to the list". A name too long for its tile is cut off, and the tile's tooltip shows the
/// whole path. (Up to 0.8 such a favorite was drawn as a row instead, #174, which looked like the
/// list but wasn't.)
fn split_tiles(list: Pixels, count: usize) -> TileLayout {
    let min = theme::scaled_px(TILE_NAME_MIN);
    let per_row = match [4, 3]
        .into_iter()
        .find(|&n| tile_text_width(list, n) >= min)
    {
        Some(4) if last_row_fill(count, 3) > last_row_fill(count, 4) => 3,
        Some(n) => n,
        None => 2,
    };
    TileLayout { per_row }
}

/// How full the last of the rows `count` tiles take `per_row` a row is, in twelfths (exact for
/// three and four a row).
const fn last_row_fill(count: usize, per_row: usize) -> usize {
    match count % per_row {
        0 => 12,
        left => left * 12 / per_row,
    }
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
        PlaceKind::Phone => Icon::Phone,
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
        // Home has its row, not a heading.
        SidebarSection::Home => ("open-home", t!("home.title")),
        // Kept from when the heading only labelled the pinned list.
        SidebarSection::Favorites => ("pin-list-label", t!("sidebar.favorites")),
        SidebarSection::ThisPc => ("this-pc-header", t!("sidebar.this_pc")),
        SidebarSection::Network => ("network-header", t!("sidebar.network")),
    };
    (id, title.into_owned().into())
}

/// The sections that can be hidden (#323, #387), in the order the menu of the sidebar's empty
/// space lists them.
const HIDEABLE: [SidebarSection; 4] = [
    SidebarSection::Home,
    SidebarSection::Favorites,
    SidebarSection::ThisPc,
    SidebarSection::Network,
];

/// The menu of a hideable section's heading (or Home's row): Hide.
pub(super) fn section_menu(section: SidebarSection) -> Vec<SidebarRow> {
    vec![row(
        t!("sidebar.hide_section"),
        SidebarAction::HideSection(section),
    )]
}

/// Whether a section of the sidebar is hidden (#323).
pub(super) fn section_hidden(section: SidebarSection, cx: &App) -> bool {
    SettingsStore::try_global(cx).is_some_and(|s| s.read(cx).settings.is_hidden(section))
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
        self.tab_drag.origin = None;
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
        // A tile, or a list entry that became one on the way (`on_tile_drag_move`).
        if self.tile_favorites(cx).contains(&dragged.path) {
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
        self.tab_drag.origin = None;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        // Moved into the list on the way (`pin_over_list`): it is where it was let go.
        if SettingsStore::global(cx)
            .read(cx)
            .settings
            .pinned
            .contains(&dragged.path)
        {
            cx.notify();
            return;
        }
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
        // The favorite being dragged, a list entry now: its row is the gap where it would land.
        let gap = self.dragging_pin
            && cx.has_active_drag()
            && !self.tab_drag.is_tile_now()
            && self.tab_drag.dragged_pin() == Some(path);
        let row = div()
            .id(SharedString::from(format!("pin-row-{}", path.display())))
            .debug_selector(move || format!("pin-row-{ix}"))
            .relative()
            .rounded(theme::r_8())
            .when(gap, |d| {
                // Where it lands, drawn like the block that showed the list takes it.
                // A row's height with its border, so the rows under it don't move when it opens.
                d.debug_selector(|| "pin-row-gap".to_owned())
                    .h(theme::h_row())
                    .overflow_hidden()
                    .mx(theme::s_4())
                    .border_1()
                    .border_dashed()
                    .border_color(theme::signal_line())
                    .bg(theme::signal_wash())
            })
            .child(if gap {
                div().invisible().child(row).into_any_element()
            } else {
                row
            })
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
            // takes this row's place at once, the rows making room: no insertion line.
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<DraggedPin>, _, cx| {
                    this.pin_over_row(ix, e, cx);
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
        // Hidden (#323): Home takes its tiles with it, Favorites its list (with the favorites too
        // long for a tile, listed there) and the Recycle Bin. The Shelf stays.
        let home_hidden = settings.is_hidden(SidebarSection::Home);
        let list_hidden = settings.is_hidden(SidebarSection::Favorites);
        let favorites = if home_hidden && list_hidden {
            Vec::new()
        } else {
            self.favorites(cx)
        };
        // Slots count every favorite, tiles or rows: drops and the saved order use them.
        let tile_count = favorites.len();
        let layout = self.tile_layout(tile_count);
        let mut tiles: Vec<AnyElement> = Vec::new();
        for (ix, (path, label, icon)) in favorites.into_iter().enumerate() {
            if home_hidden {
                continue;
            }
            let here = current == Some(path.as_path());
            let tile = self.render_tile(ix, &path, label, icon, here, layout, cx);
            let cell = tiles.len();
            tiles.push(self.tile_drag_shell(ix, cell, &path, tile, cx));
        }
        let mut pinned_rows = Vec::new();
        let mut pinned_keys: Vec<PlaceRow> = Vec::new();
        let listed: &[PathBuf] = if list_hidden { &[] } else { &pinned };
        // Named and drawn as on a tile: a drive is "Local Disk (C:)" with its icon, not "C:\"
        // with a folder's (#371).
        let places = self.active_view().read(cx).places.clone();
        let place_names = SettingsStore::global(cx)
            .read(cx)
            .settings
            .place_names
            .clone();
        for (ix, path) in listed.iter().enumerate() {
            let row_at = pinned_rows.len();
            let label = own_names
                .get(&crate::place_names::key(path))
                .cloned()
                .unwrap_or_else(|| favorite_default(&place_names, &places, path));
            let icon = places
                .iter()
                .find(|p| p.path == *path)
                .map(|p| place_icon(p.kind));
            self.render_tree(
                path,
                label.clone(),
                None,
                icon,
                0,
                None,
                current,
                true,
                &mut pinned_rows,
                cx,
            );
            // The list row itself (not its expanded children) is draggable: reorder or drop onto tiles.
            let row = pinned_rows.remove(row_at);
            pinned_rows.insert(row_at, self.pin_list_row(ix, path, label, row, cx));
            pinned_keys
                .extend((0..pinned_rows.len() - row_at).map(|n| PlaceRow::Pinned(path.clone(), n)));
        }
        // A tile being dragged: the list shows it can go down there (a block under the heading,
        // the heading shown for it even with nothing listed). Over the list it becomes a row, its
        // gap drawn like the block. A list entry dragged up onto the tiles shows nothing here:
        // the list only closes its gap.
        let tile_dragged = !list_hidden
            && self.dragging_pin
            && cx.has_active_drag()
            && self.tab_drag.is_tile_now()
            && self.tab_drag.started_on_tile();
        let wants_list_drop = tile_dragged;
        let mut out = Vec::new();
        if home_hidden {
            // No grid to keep still or snap to; drawn again, it starts afresh.
            self.sidebar_ui.tile_rows.set(0);
            self.sidebar_ui.grid_height.set(None);
        } else {
            out.push(self.render_tile_grid(tiles, tile_count, layout, cx));
        }
        // The Recycle Bin is pinned by default and always sits last; it cannot be dragged. A
        // favorite or folder dragged over it goes in last in the list, just above it.
        if recycle_pinned && !list_hidden {
            let end = pinned.len();
            let marked = self.pin_shown == Some(PinSlot::List(end)) && self.pin_mark.is_none();
            pinned_rows.push(
                div()
                    .id("recycle-drop")
                    .relative()
                    .child(self.render_recycle_row(cx))
                    .when(marked, |d| d.child(pindrop::insertion_mark(false, true)))
                    .on_drag_move(
                        cx.listener(move |this, e: &DragMoveEvent<DraggedPin>, _, cx| {
                            this.pin_over_list(end, e, cx);
                        }),
                    )
                    .on_drag_move(
                        cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                            this.pin_hover_at(PinSlot::List(end), e, cx);
                        }),
                    )
                    .on_drag_move(cx.listener(
                        move |this, e: &DragMoveEvent<DraggedFiles>, _, cx| {
                            this.pin_hover_at(PinSlot::List(end), e, cx);
                        },
                    ))
                    .on_drag_move(cx.listener(
                        move |this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                            this.pin_hover_at(PinSlot::List(end), e, cx);
                        },
                    ))
                    .with_pin_drops(cx)
                    .on_drop(cx.listener(move |this, dragged: &DraggedPin, window, cx| {
                        this.drop_pin_in_list(dragged, end, window, cx);
                    }))
                    .into_any_element(),
            );
            pinned_keys.push(PlaceRow::Recycle);
        }
        // No pins, no section. "Pin to sidebar" makes it appear; no placeholder text.
        let heading = !pinned_rows.is_empty() || wants_list_drop;
        if heading {
            out.push(
                self.section_header(SidebarSection::Favorites, collapsed, Vec::new(), cx)
                    // A folder or tab goes first in the list; with the list collapsed, or no
                    // folder pinned and no Recycle Bin row to show the spot above, the heading
                    // itself shows it (one mark, where it lands).
                    .when(
                        (collapsed || (pinned.is_empty() && !recycle_pinned))
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
                        this.pin_over_list(0, e, cx);
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
        // belong to Home). The block stays, under the heading.
        if heading && collapsed {
            if tile_dragged {
                out.push(Self::list_drop_block(cx));
            }
            return out;
        }
        let start = out.len();
        let flip = &self.tab_drag.rows;
        // Where a tile dropped into the list lands (first of the pinned folders), so the block
        // turns into its gap in place when it gets there.
        if tile_dragged {
            out.push(Self::list_drop_block(cx));
        }
        // A parent for each pinned row, so a folder pinned and also shown under This PC doesn't
        // repeat an element id (the screen reader tree needs them unique). Each row stays its own
        // item of the scrolling list, so one cut off at its bottom is dimmed alone.
        out.extend(
            pinned_rows
                .into_iter()
                .zip(pinned_keys)
                .enumerate()
                .map(|(ix, (row, key))| {
                    flip.row(
                        key,
                        div()
                            .id(("pinned", ix))
                            .flex()
                            .flex_col()
                            .child(row)
                            .into_any_element(),
                    )
                }),
        );
        out.extend(
            self.render_shelf(cx)
                .into_iter()
                .enumerate()
                .map(|(ix, row)| flip.row(PlaceRow::Shelf(ix), row)),
        );
        self.drop_in(SidebarSection::Favorites, &mut out, start);
        out
    }

    /// The favorite tiles' grid, Home's. A folder or tab over it washes it over, and an "Add to
    /// favorites" slot follows the last tile (`workspace_pindrop.rs`).
    fn render_tile_grid(
        &self,
        tiles: Vec<AnyElement>,
        tile_count: usize,
        layout: TileLayout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let add_slot = self.render_add_favorite_slot(tile_count, tiles.len(), layout, cx);
        let tile_rows = tiles.len().div_ceil(layout.per_row());
        self.sidebar_ui.tile_rows.set(tile_rows);
        let slot_row = add_slot.as_ref().is_some_and(|(_, own_row)| *own_row);
        self.keep_rows_under_tiles_still(tile_rows + usize::from(slot_row), layout.large());
        let add_slot = add_slot.map(|(slot, _)| slot);
        div()
            .id("pin-tiles")
            .debug_selector(|| "pin-tiles".to_owned())
            .when_some(
                self.tab_drag
                    .grid_hold
                    .filter(|_| self.dragging_pin && cx.has_active_drag()),
                |d, height| d.min_h(px(height)),
            )
            .grid()
            .grid_cols(u16::try_from(layout.per_row()).unwrap_or(3))
            .gap(theme::s_4())
            .rounded(theme::r_8())
            .when(add_slot.is_some() || self.list_invites_tiles(cx), |d| {
                d.bg(theme::signal_wash())
                    .child(div().absolute().debug_selector(|| "tiles-invite".to_owned()))
            })
            .children(tiles)
            .children(add_slot)
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                this.tiles_hover(e, cx);
            }))
            // A favorite from the list over the space after the last tile becomes one again.
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedPin>, _, cx| {
                this.pin_over_tiles(e, cx);
            }))
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedFiles>, _, cx| {
                this.tiles_hover(e, cx);
            }))
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                this.tiles_hover(e, cx);
            }))
            .with_pin_drops(cx)
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, window, cx| {
                this.drop_pin_between_tiles(dragged, tile_count, window, cx);
            }))
            .into_any_element()
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

    /// The tile grid is `rows` rows high now. Scrolled out of sight above, a change in its height
    /// (the "Move to the tiles" slot opening a row while a list entry is dragged, a tile coming or
    /// going) scrolls with it, so the rows in view stay where they are, under the pointer.
    fn keep_rows_under_tiles_still(&self, rows: usize, large: bool) {
        let height = if rows == 0 {
            0.
        } else {
            let rows = rows as f32;
            rows * f32::from(tile_height(large)) + (rows - 1.) * f32::from(theme::s_4())
        };
        let Some(was) = self.sidebar_ui.grid_height.replace(Some(height)) else {
            return;
        };
        let grew = height - was;
        if grew.abs() < 0.5 {
            return;
        }
        let scroll = &self.sidebar_ui.places_scroll;
        // Laid out unscrolled: the top of the view in those terms follows the scroll.
        let view_top = scroll.bounds().top() - scroll.offset().y;
        if scroll
            .bounds_for_item(0)
            .is_none_or(|grid| grid.bottom() > view_top + px(0.5))
        {
            return;
        }
        let offset = scroll.offset();
        scroll.set_offset(gpui::point(offset.x, (offset.y - px(grew)).min(px(0.))));
    }

    /// A tile's size in the grid.
    pub(super) fn tile_size(&self, layout: TileLayout) -> gpui::Size<Pixels> {
        let per_row = layout.per_row() as f32;
        let gap = f32::from(theme::s_4());
        let width = (f32::from(self.places_width()) - gap * (per_row - 1.)) / per_row;
        gpui::size(px(width), tile_height(layout.large()))
    }

    /// How many of `count` tiles a row (`split_tiles`), in the width the places list had last
    /// frame (an estimate from the sidebar's width before the first).
    pub(super) fn tile_layout(&self, count: usize) -> TileLayout {
        split_tiles(self.places_width(), count)
    }

    /// The favorites drawn as tiles, in order: the grid the tiles reorder and slide on.
    pub(super) fn tile_favorites(&self, cx: &mut Context<Self>) -> Vec<PathBuf> {
        self.favorites(cx)
            .into_iter()
            .map(|(path, ..)| path)
            .collect()
    }

    /// The menu of a favorite in the sidebar, on a tile or in the list.
    pub(super) fn favorite_menu(&self, path: &Path, cx: &mut Context<Self>) -> Vec<SidebarRow> {
        self.folder_menu(path, MenuFor::Favorite, cx)
    }

    /// The menu of a folder, wherever it is: the tree, a favorite (tile, list row or card on
    /// Home), a drive or network card on Home. One builder, so the same folder has the same rows
    /// in the same order everywhere, and drives only add their own:
    /// open in a new tab | favorites (add or remove, move, rename) | drive or network place
    /// (disconnect, eject, format, cleanup, search, forget) | Terminal, File Explorer, copy path,
    /// properties.
    /// Built when the menu opens, not each time the row is drawn.
    #[allow(clippy::too_many_lines, reason = "the groups of rows, top to bottom")]
    pub(super) fn folder_menu(
        &self,
        path: &Path,
        at: MenuFor,
        cx: &mut Context<Self>,
    ) -> Vec<SidebarRow> {
        let place = path.to_path_buf();
        let store = SettingsStore::global(cx);
        let view = self.active_view().read(cx);
        let places = &view.places;
        let settings = &store.read(cx).settings;
        let favorite = matches!(at, MenuFor::Favorite | MenuFor::HomeFavorite);
        // The place itself (a drive, a network place, a favorite), not a folder in its tree.
        let top = !matches!(at, MenuFor::Tree { depth } if depth > 0);
        let mut items = vec![row(
            t!("sidebar.open_in_new_tab"),
            SidebarAction::OpenInNewTab(place.clone()),
        )];

        // Favorites. "Add" and "Remove" take the same row, so it stays put either way.
        items.push(row(
            if settings.is_pinned(path, places) {
                t!("sidebar.remove_favorite")
            } else {
                t!("sidebar.add_favorite")
            },
            SidebarAction::TogglePin(place.clone()),
        ));
        let on_tiles = settings.tiles(places).iter().any(|p| p == path);
        let in_list = settings.pinned.iter().any(|p| p == path);
        if on_tiles && favorite {
            items.push(row(
                // Home shows the tiles only: there is no list below them there.
                if at == MenuFor::HomeFavorite {
                    t!("home.move_to_sidebar_list")
                } else {
                    t!("sidebar.move_to_list")
                },
                SidebarAction::MoveToList(place.clone()),
            ));
        } else if in_list && top && settings.tiles(places).len() < FAVORITES_MAX {
            items.push(row(
                t!("sidebar.move_to_tiles"),
                SidebarAction::MoveToTiles(place.clone()),
            ));
        }
        if favorite {
            // The name the favorite shows (#259), not the place's.
            items.push(row(
                t!("menu.rename"),
                SidebarAction::RenameFavorite(place.clone()),
            ));
        } else if top && place_default_name(settings, places, path).is_some() {
            items.push(row(
                t!("menu.rename"),
                SidebarAction::RenamePlace(place.clone()),
            ));
        }

        // A drive's or network place's own rows.
        if top {
            let kind = places.iter().find(|p| p.path == path).map(|p| p.kind);
            let mapped = matches!(kind, Some(PlaceKind::Drive { remote: true, .. }));
            let drive = matches!(kind, Some(PlaceKind::Drive { remote: false, .. }));
            let net_place = settings.net_places.iter().any(|p| p == path);
            if mapped {
                items.push(row(
                    t!("sidebar.disconnect_drive"),
                    SidebarAction::DisconnectDrive(place.clone()),
                ));
            }
            if drive && view.services.shell.is_ejectable(path) {
                items.push(row(
                    t!("sidebar.eject"),
                    SidebarAction::EjectDrive(place.clone()),
                ));
            }
            if drive {
                if !is_system_drive(path) {
                    items.push(row(
                        t!("menu.format_drive"),
                        SidebarAction::FormatDrive(place.clone()),
                    ));
                }
                items.push(row(
                    t!("menu.disk_cleanup"),
                    SidebarAction::DiskCleanup(place.clone()),
                ));
            }
            if mapped || net_place {
                items.push(row(
                    if crate::global_search::is_searched(settings, path) {
                        t!("sidebar.unsearch_network")
                    } else {
                        t!("sidebar.search_network")
                    },
                    SidebarAction::ToggleSearchNetwork(place.clone()),
                ));
            }
            if net_place {
                items.push(row(
                    t!("sidebar.forget_share"),
                    SidebarAction::ForgetShare(place.clone()),
                ));
            }
            if settings.remote_places.iter().any(|p| p.url == path) {
                // Only offered once plain http was allowed for it, so the choice stays visible
                // and can be taken back (#252).
                if crate::remote_shell::insecure_http_allowed(&self.services.remotes, path) {
                    items.push(row(
                        t!("sidebar.require_secure"),
                        SidebarAction::RequireSecure(place.clone()),
                    ));
                }
                items.push(row(
                    t!("sidebar.forget_share"),
                    SidebarAction::ForgetRemote(place.clone()),
                ));
            }
        }

        // What the file list's menu has for a folder. An SFTP or WebDAV location is no folder
        // Windows can open; a bare `\\server` only lists its shares (no Terminal or Properties).
        let remote = arke_core::remote::is_remote(path);
        let server = arke_core::net::bare_server(path).is_some();
        if !remote && !server {
            items.push(row(
                t!("menu.open_terminal"),
                SidebarAction::OpenTerminal(place.clone()),
            ));
        }
        if !remote {
            items.push(row(
                t!("menu.open_in_explorer"),
                SidebarAction::OpenInExplorer(place.clone()),
            ));
        }
        items.push(row(
            t!("menu.copy_path"),
            SidebarAction::CopyPath(place.clone()),
        ));
        if !remote && !server {
            items.push(row(
                t!("menu.properties"),
                SidebarAction::Properties(place, true),
            ));
        }
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
                    let ender = workspace.clone();
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
                    // Let go anywhere else (in the sidebar, not on a place that takes it): once
                    // the drops have run, the favorite goes back where it was and nothing the drag
                    // showed stays behind.
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                        if phase != DispatchPhase::Capture || e.button != MouseButton::Left {
                            return;
                        }
                        let ender = ender.clone();
                        cx.defer(move |cx| {
                            let _ = ender.update(cx, Self::end_pin_drag_nowhere);
                        });
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

    /// A favorite dragged over the list at slot `at` (a row's place, the heading's first slot, the
    /// Recycle Bin's last): it goes there at once and its row is the gap where it would land, the
    /// rows making room, as tiles do among the tiles. A tile turns into a list entry on the way,
    /// its copy under the pointer turning into a row.
    fn pin_over_list(&mut self, at: usize, e: &DragMoveEvent<DraggedPin>, cx: &mut Context<Self>) {
        if !e.bounds.contains(&e.event.position) || self.tab_drag.moved_now() {
            return;
        }
        let path = e.drag(cx).path.clone();
        let pinned = &SettingsStore::global(cx).read(cx).settings.pinned;
        let already =
            pinned.get(at) == Some(&path) || (at >= pinned.len() && pinned.last() == Some(&path));
        if already {
            return;
        }
        self.tab_drag.rows.moved();
        self.move_pin(&path, PinSlot::List(at), cx);
        self.tab_drag.mark_moved();
        self.tab_drag.set_tile(None);
        pindrop::clear(cx);
    }

    /// A favorite dragged over the list row of slot `ix`. One already in the list takes its place
    /// only once the pointer is past the row's middle, coming from where it is, as tabs do:
    /// swapped at the row's edge, the rows would trade places back and forth under a pointer
    /// resting between them.
    fn pin_over_row(&mut self, ix: usize, e: &DragMoveEvent<DraggedPin>, cx: &mut Context<Self>) {
        let at = e.event.position.y;
        if !e.bounds.contains(&e.event.position) {
            return;
        }
        let path = e.drag(cx).path.clone();
        let from = SettingsStore::global(cx)
            .read(cx)
            .settings
            .pinned
            .iter()
            .position(|p| *p == path);
        if let Some(from) = from {
            let mid = e.bounds.center().y;
            if !((from < ix && at > mid) || (from > ix && at < mid)) {
                return;
            }
        }
        self.pin_over_list(ix, e, cx);
    }

    /// While a tile is dragged: a block under the Favorites heading, where it goes into the list.
    fn list_drop_block(cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("pin-list-drop")
            .debug_selector(|| "pin-list-drop".to_owned())
            // A row's height, as the gap it turns into: not squeezed by the list.
            .flex_none()
            .h(theme::h_row())
            .mx(theme::s_4())
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::r_8())
            .border_1()
            .border_dashed()
            .border_color(theme::signal_line())
            .bg(theme::signal_wash())
            .text_size(theme::fs_12())
            .text_color(theme::text())
            .child(t!("sidebar.drop_to_list").into_owned())
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedPin>, _, cx| {
                this.pin_over_list(0, e, cx);
            }))
            .on_drop(cx.listener(|this, dragged: &DraggedPin, window, cx| {
                this.drop_pin_in_list(dragged, 0, window, cx);
            }))
            .into_any_element()
    }

    /// The dragged favorite was let go outside the sidebar: it leaves the favorites.
    fn drop_pin_outside(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.tab_drag.dragged_pin().map(Path::to_path_buf) else {
            return;
        };
        self.dragging_pin = false;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        // It may have become a list entry or a tile on its way out, moving others: the order
        // goes back to how it was, so Undo puts it back where the drag started.
        self.restore_pins(cx);
        self.remove_favorite(&path, cx);
    }

    /// The drag ended where nothing took it (This PC, the tabs): the favorite goes back where it
    /// was when the drag started.
    fn end_pin_drag_nowhere(&mut self, cx: &mut Context<Self>) {
        if !self.dragging_pin {
            return;
        }
        self.dragging_pin = false;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        self.restore_pins(cx);
        cx.notify();
    }

    /// Puts the favorites back the way they were when the drag started (`TabDrag::origin`).
    pub(super) fn restore_pins(&mut self, cx: &mut Context<Self>) {
        let Some(origin) = self.tab_drag.origin.take() else {
            return;
        };
        // The rows slide back with it.
        self.tab_drag.rows.moved();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.favorites = origin.favorites;
                    st.pinned = origin.pinned;
                },
                cx,
            );
        });
        cx.notify();
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
        let mapped = SettingsStore::global(cx)
            .read(cx)
            .settings
            .is_hidden(SidebarSection::Network);
        let count = this_pc_count(places, mapped);
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
        mapped: bool,
        items: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_ui.here_row.set(None);
        for place in places.iter().filter(|p| in_this_pc(p.kind, mapped)) {
            self.render_device(place, current, items, cx);
        }
        // The tree just opened to the current folder: scroll its row into view if needed.
        if self.sidebar_ui.reveal.replace(false)
            && let Some(ix) = self.sidebar_ui.here_row.get()
        {
            self.sidebar_ui.places_scroll.scroll_to_item(ix);
            // Once it has scrolled: not leaving the tiles cut in half at the top.
            self.sidebar_ui.snap_tiles.set(true);
            self.sidebar_ui.frame_wanted.set(true);
        }
    }

    /// Right after the places scrolled to the current folder's row (the frame before): when
    /// their top edge cuts through a row of favorite tiles, they scroll to a whole row
    /// ([`tile_snap`]) if the current folder's row stays in view. Scrolling to a row only ever
    /// moves as little as it needs, which often left the tiles under Home cut in half on start.
    fn snap_tile_rows(&self) {
        if !self.sidebar_ui.snap_tiles.replace(false) {
            return;
        }
        let scroll = &self.sidebar_ui.places_scroll;
        // The tiles are the first item; rows are laid out unscrolled, as in `scrolling`.
        let (Some(tiles), Some(here)) = (
            scroll.bounds_for_item(0),
            self.sidebar_ui
                .here_row
                .get()
                .and_then(|ix| scroll.bounds_for_item(ix)),
        ) else {
            return;
        };
        let view = scroll.bounds();
        let view_top = f32::from(view.top());
        let Some(top) = tile_snap(
            view_top - f32::from(scroll.offset().y),
            (f32::from(tiles.top()), f32::from(tiles.bottom())),
            self.sidebar_ui.tile_rows.get(),
            f32::from(theme::s_4()),
            (f32::from(here.top()), f32::from(here.bottom())),
            f32::from(view.size.height),
            view_top + f32::from(scroll.max_offset().y),
        ) else {
            return;
        };
        scroll.set_offset(gpui::point(scroll.offset().x, px(view_top - top)));
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the places, then the tabs with their heading, read top to bottom"
    )]
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.snap_tile_rows();
        if let Some(row) = self.sidebar_ui.keys.start_frame() {
            self.reveal_sidebar_row(row);
        }
        let current = self.sidebar_here(cx);
        let places = self.active_view().read(cx).places.clone();
        let (this_pc_collapsed, this_pc_hidden, network_collapsed, network_hidden) = {
            let settings = &SettingsStore::global(cx).read(cx).settings;
            (
                settings.is_collapsed(SidebarSection::ThisPc),
                settings.is_hidden(SidebarSection::ThisPc),
                settings.is_collapsed(SidebarSection::Network),
                settings.is_hidden(SidebarSection::Network),
            )
        };
        if !this_pc_collapsed && !this_pc_hidden {
            self.schedule_follow(current.as_ref(), &places, cx);
        }
        // Places (pins, This PC, Network) and the vertical tab list each scroll on their own. When
        // they don't both fit, the tab list gives way first, down to three tabs, so opening many
        // tabs doesn't squeeze the places; only then do the places shrink too. The tabs never
        // fall below the fold, where nothing showed they were there. With room to spare, nothing
        // changes: the places take their height and the tabs follow right under them.
        let horizontal = Self::horizontal_tabs(cx);
        // The favorites, their list and the Shelf.
        self.tab_drag.rows.frame();
        let mut items = self.render_pins(current.as_deref(), cx);
        let pins_end = items.len();
        if this_pc_hidden {
            // Hidden (#323): no row of the current folder to scroll to.
            self.sidebar_ui.here_row.set(None);
        } else {
            items.push(
                self.section_header(SidebarSection::ThisPc, this_pc_collapsed, Vec::new(), cx)
                    .into_any_element(),
            );
            if !this_pc_collapsed {
                let start = items.len();
                self.render_this_pc(&places, current.as_deref(), network_hidden, &mut items, cx);
                self.drop_in(SidebarSection::ThisPc, &mut items, start);
            }
        }
        // Hidden (#387): no heading, rows or add button.
        if !network_hidden {
            self.render_network(
                &places,
                current.as_deref(),
                network_collapsed,
                &mut items,
                cx,
            );
        }
        // Under the favorites, rows move with them: a row more or less in the list moves them all.
        if self.tab_drag.rows.wanted() {
            let after: Vec<AnyElement> = items.drain(pins_end..).collect();
            items.extend(
                after
                    .into_iter()
                    .enumerate()
                    .map(|(ix, row)| self.tab_drag.rows.row(PlaceRow::After(ix), row)),
            );
        }
        // The tiles and rows went under the Home row: a shadow there says so.
        let scrolled = self.sidebar_ui.places_scroll.offset().y < px(-0.5);
        let places = div()
            .id("sidebar-places")
            .flex()
            .flex_col()
            .gap(theme::s_2());
        // The line between the places and the tabs dragged (now, or before): the places take
        // that height, as far as the tabs' three rows leave room.
        let split = (!horizontal).then(|| self.places_split(cx)).flatten();
        let places_area = scrolling(
            places,
            items,
            &self.sidebar_ui.places_scroll,
            div()
                .debug_selector(|| "sidebar-places-frame".to_owned())
                // With tabs on top there is nothing below, so the places fill the sidebar.
                .when(horizontal, gpui::Styled::flex_1)
                .when(!horizontal && split.is_none(), gpui::Styled::flex_initial)
                .when_some(split, |d, height| {
                    d.flex_basis(height).flex_grow(0.).flex_shrink(1.)
                })
                // At least two rows stay visible, so it's clear this part scrolls too.
                .min_h(theme::h_row() * 2.),
        )
        .relative()
        .when(scrolled, |d| d.child(scroll_shadow()));
        let keys_here = self.sidebar_ui.keys.focused.get();
        let sidebar = div()
            .id("sidebar")
            .a11y(gpui::Role::Tree, t!("a11y.sidebar"))
            .key_context("Sidebar")
            // Only while the keyboard is here (F6): a click in the sidebar leaves it in the list.
            .when(keys_here, |d| d.track_focus(&self.sidebar_focus))
            .on_action(cx.listener(Self::sidebar_up))
            .on_action(cx.listener(Self::sidebar_down))
            .on_action(cx.listener(Self::sidebar_first))
            .on_action(cx.listener(Self::sidebar_last))
            .on_action(cx.listener(Self::sidebar_in))
            .on_action(cx.listener(Self::sidebar_out))
            .on_action(cx.listener(Self::sidebar_open))
            .on_action(cx.listener(Self::sidebar_leave))
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
            // A right click that no row, tab or heading takes (the space below the tabs, between
            // rows, the Network and Tabs headings): shows hidden sections again (#323).
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    let items = Self::sidebar_space_menu(cx);
                    this.open_sidebar_menu(e.position, items, cx);
                }),
            )
            .child(places_area);
        if horizontal {
            return sidebar;
        }
        let sidebar = sidebar.child(self.render_places_split(cx));
        // The tabs don't collapse: they are what's open (hiding the whole sidebar makes room).
        let header = div()
            .debug_selector(|| "tabs-header".to_owned())
            .relative()
            .flex()
            .flex_none()
            .items_end()
            .child(self.tab_area_recorder(super::tabmove::AreaPart::ListHead))
            // The saved workspace this window shows, muted after the heading (#269).
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_end()
                    .child(section_label(t!("sidebar.tabs")))
                    .children(
                        self.workspace_label(cx)
                            .map(|label| label.pb(theme::s_6()).pr(theme::s_8())),
                    ),
            )
            .child(Self::workspaces_button(cx))
            .child(tab_layout_button(false, cx));
        let sidebar = sidebar.child(header);
        let sidebar = {
            let tabs: Vec<AnyElement> = (0..self.tabs.len())
                .map(|ix| self.render_tab(ix, cx))
                .collect();
            let tab_count = tabs.len();
            self.reveal_active_tab();
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
                        .a11y(gpui::Role::TabList, self.tabs_label(cx))
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
                )
                .relative()
                // The file list's scrollbar (#323), only while the tabs don't fit. It sits in the
                // gutter between the tabs and the sidebar's edge, so it doesn't cover their close
                // buttons.
                .child(
                    crate::scrollbar::scrollbar(
                        self.sidebar_ui.tabs_scroll.clone(),
                        std::rc::Rc::clone(&self.sidebar_ui.tabs_bar),
                    )
                    .right(px(2.) - theme::s_8()),
                ))
        };
        // Dropping in the empty space below the last tab moves it to the end.
        sidebar.child(
            div()
                .id("tab-list-end")
                .debug_selector(|| "tab-list-end".to_owned())
                .relative()
                .child(self.tab_area_recorder(super::tabmove::AreaPart::ListEnd))
                .flex_1()
                .min_h(theme::s_32())
                .on_drop(cx.listener(Self::drop_tab_at_end)),
        )
    }

    /// A favorite taken from the list is dragged: the tiles show they take it.
    fn list_invites_tiles(&self, cx: &App) -> bool {
        self.dragging_pin
            && cx.has_active_drag()
            && self.tab_drag.pin_from_list()
            && !self.tab_drag.started_on_tile()
    }

    /// The places' height set by dragging the line above the tabs: while dragged, else as saved.
    /// `None`: the room is shared by itself.
    fn places_split(&self, cx: &mut Context<Self>) -> Option<Pixels> {
        if let Some((_, _, height)) = self.split_drag {
            return Some(height);
        }
        let saved = SettingsStore::global(cx).read(cx).settings.sidebar_split;
        (saved > 0.).then(|| px(saved * theme::scale_pct() as f32 / 100.))
    }

    /// The line between the places and the tabs (vertical tabs): dragged, it sets how tall the
    /// places are, as the pinned tabs' line in Firefox does; double-clicked, the room is shared
    /// by itself again. Always a faint line, so it can be found; brighter under the mouse.
    fn render_places_split(&self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.split_drag.is_some();
        div()
            .id("sidebar-split")
            .debug_selector(|| "sidebar-split".to_owned())
            .group("sidebar-split")
            .flex_none()
            // Easy to take hold of: the whole strip, not only the line drawn in it.
            .h(px(9.))
            .flex()
            .items_center()
            .cursor_row_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        this.split_drag = None;
                        SettingsStore::global(cx)
                            .update(cx, |s, cx| s.update(|st| st.sidebar_split = 0., cx));
                    } else {
                        let height = this.sidebar_ui.places_scroll.bounds().size.height;
                        this.split_drag = Some((event.position.y, height, height));
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w_full()
                    .mx(theme::s_8())
                    .h(px(2.))
                    .rounded(theme::R_FULL)
                    .when(!active, |d| d.bg(theme::line()))
                    .when(active, |d| d.bg(theme::signal()))
                    .group_hover("sidebar-split", |s| s.bg(theme::signal_line())),
            )
            .into_any_element()
    }

    /// How far the sidebar's places scroll past what is shown (tests).
    #[cfg(test)]
    pub(crate) fn places_overflow_for_test(&self) -> Pixels {
        self.sidebar_ui.places_scroll.max_offset().y
    }

    /// How far the sidebar's places are scrolled (tests).
    #[cfg(test)]
    pub(crate) fn places_offset_for_test(&self) -> Pixels {
        self.sidebar_ui.places_scroll.offset().y
    }

    /// The active tab is scrolled into view in the vertical tab list each time it changes (opened,
    /// switched to with keys), and again when the list changes height. Only as far as needed: a
    /// tab already in view stays where it is. Uses last frame's layout, like the tab strip on top.
    fn reveal_active_tab(&self) {
        let scroll = &self.sidebar_ui.tabs_scroll;
        let reveal = (self.active, scroll.bounds().size.height);
        if self.sidebar_ui.tabs_revealed.get() == Some(reveal) || reveal.1 <= px(0.) {
            return;
        }
        if scroll.bounds_for_item(self.active).is_some() {
            self.sidebar_ui.tabs_revealed.set(Some(reveal));
            scroll.scroll_to_item(self.active);
        } else {
            // Just opened, so not laid out yet: once more next frame (the list is drawn, so it
            // will be, and this doesn't repeat).
            self.sidebar_ui.frame_wanted.set(true);
        }
    }

    /// How far the vertical tab list scrolls past what is shown, and how far it is scrolled
    /// (tests).
    #[cfg(test)]
    pub(crate) fn tabs_scroll_for_test(&self) -> (Pixels, Pixels) {
        let scroll = &self.sidebar_ui.tabs_scroll;
        (scroll.max_offset().y, scroll.offset().y)
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
                None,
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
                None,
                current,
                false,
                out,
                cx,
            );
        }
        self.drop_in(SidebarSection::Network, out, start);
    }

    /// The menu of a place's row in the tree (`depth` 0) or of a folder in its tree, and of a
    /// drive or network card on Home.
    pub(super) fn tree_menu(
        &self,
        path: &Path,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<SidebarRow> {
        self.folder_menu(path, MenuFor::Tree { depth }, cx)
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
        layout: TileLayout,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let large = layout.large();
        let open = path.to_path_buf();
        let tab_path = path.to_path_buf();
        let menu_path = path.to_path_buf();
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
                size: self.tile_size(layout),
                large,
                here,
            }),
        };
        let tile = div()
            .id(SharedString::from(format!("tile-{}", path.display())))
            .a11y_lazy(gpui::Role::TreeItem, || label.clone())
            .debug_selector(move || format!("tile {label}"))
            // "Selected" only on the current place, as on tree rows.
            .when(here, |d| d.aria_selected(true))
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
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                this.open_clicked(open.clone(), e.modifiers(), window, cx);
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
                    let menu = this.favorite_menu(&menu_path, cx);
                    this.open_sidebar_menu(e.position, menu, cx);
                }),
            )
            // Not over its own rename box.
            .when(!renamed, |d| {
                d.tooltip(tooltip_text(
                    t!("sidebar.tile_tooltip", path = path.display()).to_string(),
                ))
            });
        let open = path.to_path_buf();
        self.nav_row(
            tile,
            SharedString::from(format!("tile-{}", path.display())),
            here,
            move || NavRow {
                depth: 1,
                expanded: None,
                open: Rc::new(move |this, window, cx| {
                    this.open_clicked(open.clone(), gpui::Modifiers::default(), window, cx);
                }),
                expand: None,
            },
            cx,
        )
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
            PlaceKind::Phone => (Icon::Phone, None),
            PlaceKind::Cloud => (Icon::Cloud, None),
        };
        // Drives get free space on the right and a thin usage bar under the label.
        let space = space.filter(|&(_, total)| total > 0).map(|(free, total)| {
            let used = 1. - free as f32 / total as f32;
            DriveSpace {
                free: arke_core::format::format_size(free),
                used: used.clamp(0., 1.),
                bytes: (free, total),
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
            None,
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
        parent: Option<&SharedString>,
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
        let menu_path = path.to_path_buf();
        let renaming = if favorite {
            self.favorite_rename_box(path)
        } else {
            self.sidebar_rename_box(path)
        };
        // A drive: what the number on the right is, as File Explorer's navigation pane tells.
        let space_tip = space.as_ref().map(|s| s.bytes);
        let indent = tree_indent() * depth as f32;
        let key = SharedString::from(format!(
            "{}tree-{depth}-{}",
            if favorite { "f" } else { "" },
            path.display()
        ));
        let row = div()
                .id(SharedString::from(format!(
                    "tree-{depth}-{}",
                    path.display()
                )))
                .a11y_lazy(gpui::Role::TreeItem, || label.clone())
                .debug_selector({
                    let label = label.clone();
                    move || format!("sidebar {label}")
                })
                // Nesting level, from 0 as AccessKit counts it (UI Automation gets it plus one);
                // "selected" only on the current row.
                .aria_level(depth)
                // Rows are drawn one after another; each is read as under its parent's row.
                .a11y_tree_row(expanded.then_some(&key), parent)
                .when(here, |d| d.aria_selected(true))
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
                        0 | 1 => this.open_clicked(open.clone(), e.modifiers(), window, cx),
                        2 if !leaf && !e.modifiers().control => {
                            this.toggle_expand(open.clone(), cx);
                        }
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
                        let menu = if favorite {
                            this.favorite_menu(&menu_path, cx)
                        } else {
                            this.tree_menu(&menu_path, depth, cx)
                        };
                        this.open_sidebar_menu(e.position, menu, cx);
                    }),
                )
                .when_some(space_tip, |d, (free, total)| {
                    d.tooltip(move |_, cx| {
                        let text = t!(
                            "home.space",
                            free = arke_core::format::format_size(free),
                            total = arke_core::format::format_size(total)
                        );
                        cx.new(|_| Tooltip(text.into_owned().into())).into()
                    })
                });
        let (open, at) = (path.to_path_buf(), path.to_path_buf());
        let row = self.nav_row(
            row,
            key.clone(),
            here,
            move || NavRow {
                depth: depth + 1,
                expanded: (!leaf).then_some(expanded),
                open: Rc::new(move |this, window, cx| {
                    this.open_clicked(open.clone(), gpui::Modifiers::default(), window, cx);
                }),
                expand: (!leaf).then(|| -> Expand {
                    Rc::new(move |this, want, cx| {
                        if this.expanded.contains(&at) != want {
                            this.toggle_expand(at.clone(), cx);
                        }
                    })
                }),
            },
            cx,
        );
        out.push(row.into_any_element());
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
                        Some(&key),
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
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let mapped = settings.is_hidden(SidebarSection::Network);
        let key = (here.clone(), this_pc_count(&places, mapped));
        // Turned off (#299), the tree is the user's alone.
        if settings.is_collapsed(SidebarSection::ThisPc)
            || settings.is_hidden(SidebarSection::ThisPc)
            || !settings.sidebar_follow
            || self.sidebar_ui.followed.as_ref() == Some(&key)
        {
            return;
        }
        self.sidebar_ui.followed = Some(key);
        if let Some(cancel) = self.sidebar_ui.follow_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let chain = follow_chain(&places, &here, mapped);
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
        // On a favorite or a pinned folder, or in one (#307): the tree opens under it but the
        // sidebar doesn't scroll away from it, the favorite being where the user works.
        let shown = self
            .favorites(cx)
            .iter()
            .map(|(p, ..)| p)
            .chain(SettingsStore::global(cx).read(cx).settings.pinned.iter())
            .any(|p| arke_core::takeover::within(&here, p));
        self.sidebar_ui.reveal_wanted.set(!shown);
        self.sidebar_ui.reveal.set(to_list.is_empty() && !shown);
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
                this.sidebar_ui
                    .reveal
                    .set(this.sidebar_ui.reveal_wanted.get());
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
        let header = div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .a11y_button(title.clone())
            .a11y_open_close(!collapsed, cx.entity().downgrade(), move |this, _, cx| {
                this.toggle_section(section, cx);
            })
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
            // Home, Favorites, This PC and Network can be hidden (#323, #387); a right click elsewhere on the
            // sidebar brings them back.
            .when(HIDEABLE.contains(&section), |d| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        this.open_sidebar_menu(e.position, section_menu(section), cx);
                    }),
                )
            });
        self.nav_row(
            header,
            SharedString::from(format!("section-{id}")),
            false,
            move || NavRow {
                depth: 0,
                expanded: Some(!collapsed),
                open: Rc::new(move |this, _, cx| this.toggle_section(section, cx)),
                expand: Some(Rc::new(move |this, want, cx| {
                    if want == collapsed {
                        this.toggle_section(section, cx);
                    }
                })),
            },
            cx,
        )
    }

    /// Hides a section of the sidebar or shows it again (#323). Its room goes to the tabs below.
    fn show_section(section: SidebarSection, shown: bool, cx: &mut App) {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.set_hidden(section, !shown), cx);
        });
    }

    /// The menu of the sidebar's empty space (#323): the sections that can be hidden, checked
    /// while shown, then the Recycle Bin once it was removed from the favorites, which had no
    /// way back there but its page's pin button (#308).
    fn sidebar_space_menu(cx: &App) -> Vec<SidebarRow> {
        let mut items: Vec<SidebarRow> = HIDEABLE
            .into_iter()
            .map(|section| {
                (
                    section_id(section).1,
                    SidebarAction::ShowSection(section, !section_hidden(section, cx)),
                )
            })
            .collect();
        if SettingsStore::try_global(cx).is_some_and(|s| !s.read(cx).settings.recycle_pinned) {
            items.push(row(
                t!("sidebar.add_recycle"),
                SidebarAction::SetRecyclePinned(true),
            ));
        }
        items
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

    /// Tests: `items` as their menu draws them: each label, its key on the right in brackets,
    /// and `-` for a line between two groups.
    #[cfg(test)]
    fn drawn_for_test(&self, items: &[SidebarRow], cx: &App) -> Vec<String> {
        let mut out = Vec::new();
        for (ix, (label, action)) in items.iter().enumerate() {
            if line_before(items, ix) {
                out.push("-".to_owned());
            }
            out.push(match self.menu_shortcut(action, cx) {
                Some(key) => format!("{label} [{key}]"),
                None => label.to_string(),
            });
        }
        out
    }

    /// Tests: the menu of folder `path` as drawn (`drawn_for_test`), opened from the tree at
    /// `depth` (`Some`), from a favorite in the sidebar (`None`), or from a card on Home (`home`).
    #[cfg(test)]
    pub(crate) fn folder_menu_drawn_for_test(
        &self,
        path: &Path,
        depth: Option<usize>,
        home: bool,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let at = match (depth, home) {
            (_, true) => MenuFor::HomeFavorite,
            (None, false) => MenuFor::Favorite,
            (Some(depth), false) => MenuFor::Tree { depth },
        };
        let items = self.folder_menu(path, at, cx);
        self.drawn_for_test(&items, cx)
    }

    /// Tests: tab `ix`'s menu as drawn (`drawn_for_test`).
    #[cfg(test)]
    pub(crate) fn tab_menu_drawn_for_test(&self, ix: usize, cx: &App) -> Vec<String> {
        self.drawn_for_test(&self.tab_menu(ix, cx), cx)
    }

    /// Tests: the open menu's rows as drawn (`drawn_for_test`).
    #[cfg(test)]
    pub(crate) fn open_menu_drawn_for_test(&self, cx: &App) -> Option<Vec<String>> {
        let menu = self.sidebar_menu.as_ref()?;
        Some(self.drawn_for_test(&menu.items, cx))
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
                if pinned {
                    // Back among the favorites: they show, so it does (#308).
                    Self::show_section(SidebarSection::Favorites, true, cx);
                } else {
                    // Off the sidebar it is still a click away, on Home among the drives (#31).
                    self.active_view().update(cx, |e, cx| {
                        e.notice = Some(t!("sidebar.recycle_on_home").into_owned());
                        cx.notify();
                    });
                }
            }
            SidebarAction::SetSidebarPeek(on) => Self::set_sidebar_peek(on, cx),
            SidebarAction::HideSection(section) => Self::show_section(section, false, cx),
            SidebarAction::ShowSection(section, shown) => Self::show_section(section, !shown, cx),
            SidebarAction::OpenFile(path) => {
                crate::search_history::SearchHistory::record(&path, false, cx);
                let shell = Arc::clone(&self.services.shell);
                crate::explorer::spawn_thread("home-open", move || {
                    if let Err(err) = shell.open(&path, None) {
                        log::info!("cannot open {}: {err}", path.display());
                    }
                });
            }
            SidebarAction::RevealFile(path) => {
                self.run_home_event(&crate::home::HomeEvent::Reveal(path), window, cx);
            }
            SidebarAction::CopyPath(path) => {
                let shell = Arc::clone(&self.services.shell);
                crate::explorer::spawn_thread("copy-path", move || {
                    let _ = shell.set_clipboard_text(&path.display().to_string());
                });
            }
            SidebarAction::Properties(path, folder) => {
                self.active_view()
                    .update(cx, |e, cx| e.open_properties(vec![(path, folder)], cx));
            }
            SidebarAction::OpenTerminal(path) => {
                let shell = Arc::clone(&self.services.shell);
                crate::explorer::spawn_thread("terminal", move || {
                    if let Err(e) = shell.open_terminal(&path) {
                        log::info!("terminal at {}: {e}", path.display());
                    }
                });
            }
            SidebarAction::OpenInExplorer(path) => {
                let shell = Arc::clone(&self.services.shell);
                crate::explorer::spawn_thread("reveal", move || {
                    let _ = shell.reveal_in_explorer(&path, false);
                });
            }
            SidebarAction::FormatDrive(path) => {
                let shell = Arc::clone(&self.services.shell);
                let owner = crate::explorer::Explorer::owner_from(window);
                crate::explorer::spawn_thread("format", move || {
                    if let Err(e) = shell.format_drive(&path, owner) {
                        log::info!("format {}: {e}", path.display());
                    }
                });
            }
            SidebarAction::DiskCleanup(path) => {
                let shell = Arc::clone(&self.services.shell);
                crate::explorer::spawn_thread("cleanup", move || {
                    if let Err(e) = shell.disk_cleanup(&path) {
                        log::info!("disk cleanup {}: {e}", path.display());
                    }
                });
            }
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
                // A page over the tab (the Recycle Bin, Home) stays where it is, and the folder
                // it returns to is the user folder with nothing of the drive left.
                let covered = self.keep_pages_over(view.entity_id(), &home);
                let home = home.clone();
                // Pinned tabs too: nothing may keep the drive busy.
                view.update(cx, |v, cx| {
                    if covered {
                        v.leave_removed_drive(home, cx);
                    } else {
                        v.navigate_unlocked(home, arke_core::NavKind::New, cx);
                    }
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

    /// The key or click shown on the right of a menu row, as the file list's menu shows its
    /// keys: Ctrl+click for "Open in new tab" (nothing else tells it), and the tab and window
    /// keys as they are set in Settings.
    fn menu_shortcut(&self, action: &SidebarAction, cx: &App) -> Option<SharedString> {
        use SidebarAction as A;
        let store = SettingsStore::try_global(cx);
        let key = |id: &str| {
            let overrides = store
                .as_ref()
                .map_or_else(Default::default, |s| s.read(cx).settings.shortcuts.clone());
            crate::commands::find(id)
                .and_then(|c| crate::commands::shown_key(c, &overrides))
                .map(SharedString::from)
        };
        match action {
            A::OpenInNewTab(_) | A::OpenRecycleInNewTab => {
                Some(t!("menu.ctrl_click").into_owned().into())
            }
            // The key works on the active tab: the whole tab unless it is split, else its active
            // side.
            A::CloseTab(ix)
                if *ix == self.active && !self.tabs.get(*ix).is_some_and(Tab::is_split) =>
            {
                key("close_tab")
            }
            A::CloseSide(ix) if *ix == self.active => key("close_tab"),
            A::ReopenClosedTab => key("reopen_closed_tab"),
            A::OpenSettings => key("open_settings"),
            A::WindowClose => Some("Alt+F4".into()),
            _ => None,
        }
    }

    /// The tab, sidebar or history menu's panel, drawn in its own window. It looks like the file
    /// list's menu: an icon on each row, the key on the right, lines between groups of rows.
    #[allow(
        clippy::too_many_lines,
        reason = "one row builder, shared by every sidebar menu"
    )]
    fn sidebar_menu_panel(
        &mut self,
        max_h: Pixels,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.sidebar_menu.as_ref()?;
        let owner = menu.popup.owner();
        let mut rows: Vec<AnyElement> = Vec::with_capacity(menu.items.len() * 2);
        for (ix, (label, action)) in menu.items.iter().enumerate() {
            if line_before(&menu.items, ix) {
                rows.push(
                    div()
                        .h(px(1.))
                        .my(theme::s_4())
                        .mx(theme::s_8())
                        .bg(theme::line())
                        .into_any_element(),
                );
            }
            let action = action.clone();
            let folder = matches!(
                action,
                SidebarAction::HistoryJump { .. } | SidebarAction::TimelineStep { .. }
            );
            // Where a history folder is, so two folders of the same name tell apart.
            let detail = menu_detail(&action);
            let shortcut = self.menu_shortcut(&action, cx);
            // A shown section's row in the menu of the sidebar's empty space has its check.
            let checked = matches!(action, SidebarAction::ShowSection(_, true));
            // A history folder is drawn as a folder; the other rows get their command's icon.
            let glyph = if folder { None } else { menu_glyph(&action) };
            let mark = div()
                .size(theme::icon())
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .when(folder, |d| {
                    d.child(kind_icon(FileKind::Folder, theme::icon()))
                })
                .children(glyph.map(|glyph| {
                    svg()
                        .path(glyph.path())
                        .size(theme::icon())
                        .text_color(theme::text_muted())
                }));
            rows.push(
                div()
                    .id(("sidebar-menu-item", ix))
                    .a11y_menu_item(label.clone(), true, checked, false)
                    .when_some(
                        shortcut.clone(),
                        gpui::StatefulInteractiveElement::aria_keyshortcuts,
                    )
                    .h(theme::h_ctl_sm() + theme::s_4())
                    .mx(theme::s_4())
                    .px(theme::s_8())
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .rounded(theme::r_8())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::wash()))
                    .child(mark)
                    .child(
                        div()
                            .when(folder, gpui::Styled::flex_none)
                            .when(!folder, |d| d.flex_1().min_w_0().truncate())
                            .child(label.clone()),
                    )
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
                    .when_some(shortcut, |d, s| {
                        d.child(
                            div()
                                .flex_none()
                                .pl(theme::s_16())
                                .text_size(theme::fs_12())
                                .text_color(theme::text_subtle())
                                .child(s),
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
                    .into_any_element(),
            );
        }
        Some(
            div()
                .id("sidebar-menu")
                .role(gpui::Role::Menu)
                .occlude()
                // As wide as the file list's menu at least.
                .min_w(crate::theme::scaled_px(240.))
                .max_h(max_h)
                .overflow_y_scroll()
                .py(theme::s_4())
                .rounded(theme::r_12())
                .bg(theme::overlay())
                .shadow(theme::elev_menu())
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
        // Its key (Ctrl+W) is on the right of the row (`menu_shortcut`).
        items.push(row(t!("sidebar.close_tab"), SidebarAction::CloseTab(ix)));
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

    /// Picks the row labeled `label` in the open sidebar menu.
    pub(crate) fn run_open_menu_for_test(
        &mut self,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action = self
            .sidebar_menu
            .as_ref()
            .and_then(|m| m.items.iter().find(|(l, _)| l.as_ref() == label))
            .map_or_else(
                || panic!("no {label:?} in the open menu"),
                |(_, action)| action.clone(),
            );
        self.run_sidebar_action(action, window, cx);
    }

    /// Which rows of the open sidebar menu have a check.
    pub(crate) fn open_menu_checks_for_test(&self) -> Option<Vec<bool>> {
        let menu = self.sidebar_menu.as_ref()?;
        Some(
            menu.items
                .iter()
                .map(|(_, a)| matches!(a, SidebarAction::ShowSection(_, true)))
                .collect(),
        )
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
        // Most rows are folders: one text box, as before. A drive keeps its letter in view.
        if crate::place_names::letter_suffix(&label).is_none() {
            return div()
                .flex_1()
                .min_w_0()
                .truncate()
                .pl(theme::s_4())
                .child(label);
        }
        return div()
            .flex_1()
            .min_w_0()
            .pl(theme::s_4())
            .flex()
            .child(crate::place_names::drive_name(label));
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
                .child(crate::place_names::drive_name(label).flex_1())
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
/// A drive came or went: every window's sidebar reads its drives again (#330).
pub(crate) fn reload_places_everywhere(cx: &mut App) {
    for window in cx.windows() {
        if let Some(workspace) = window.downcast::<Workspace>() {
            let _ = workspace.update(cx, |ws, _, cx| ws.reload_all_places(cx));
        }
    }
}

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
            paths(follow_chain(
                &places,
                Path::new(r"C:\work\reports\2026"),
                false
            )),
            [r"C:\", r"C:\work", r"C:\work\reports"]
        );
        // The user folder is under This PC as well, and nearer.
        assert_eq!(
            paths(follow_chain(
                &places,
                Path::new(r"C:\Users\me\Desktop\a"),
                false
            )),
            [r"C:\Users\me", r"C:\Users\me\Desktop"]
        );
        assert!(
            follow_chain(&places, Path::new(r"C:\"), false).is_empty(),
            "a root"
        );
        assert!(
            follow_chain(&places, Path::new(r"Z:\docs"), false).is_empty(),
            "network drives are under Network"
        );
        // With Network hidden they are listed under This PC, and the tree opens to them (#387).
        assert_eq!(
            paths(follow_chain(&places, Path::new(r"Z:\docs\a"), true)),
            [r"Z:\", r"Z:\docs"]
        );
        assert!(follow_chain(&places, Path::new(r"\nas\docs\a"), false).is_empty());
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
    fn a_tile_row_cut_by_the_top_edge_is_shown_whole_or_scrolled_past() {
        // Two rows of 56 px tiles 4 px apart (0 to 116), a view 652 px high that can start at
        // most at 118.
        let tiles = (0., 116.);
        let snap = |top: f32, rows: usize, here: (f32, f32)| {
            tile_snap(top, tiles, rows, 4., here, 652., 118.)
        };
        // The second row cut 18 px in: back to its start while the current row stays in view.
        assert_eq!(snap(78., 2, (500., 532.)), Some(60.));
        // The current row would drop out at the bottom (the start in German at S:\Studio): on
        // past the last row, no half row left at the top.
        assert_eq!(snap(78., 2, (698., 730.)), Some(116.));
        // Three rows (0 to 176), the first cut: on to the second.
        let three = tile_snap(30., (0., 176.), 3, 4., (658., 690.), 652., 200.);
        assert_eq!(three, Some(60.));
        // At a row's start, in the gap between rows, above or past the tiles: nothing to do.
        assert_eq!(snap(60., 2, (500., 532.)), None);
        assert_eq!(snap(57., 2, (500., 532.)), None);
        assert_eq!(snap(0., 2, (500., 532.)), None);
        assert_eq!(snap(116., 2, (500., 532.)), None);
        assert_eq!(snap(78., 0, (500., 532.)), None);
    }

    /// The sidebar's width caps how many tiles a row (#371): four where each still has room for a
    /// readable name, else three, else two. Where four fit, three when that fills the last row
    /// more.
    #[test]
    fn the_width_decides_how_many_tiles_a_row() {
        let per_row = |list: f32, count: usize| split_tiles(px(list), count).per_row();
        // The default sidebar (a places list about 232 wide): three, not four cut to "Des…".
        assert_eq!(per_row(232., 8), 3);
        assert_eq!(per_row(232., 12), 3);
        // Wide enough for four: whichever fills the last row more, four on a tie.
        assert_eq!(per_row(284., 8), 4);
        assert_eq!(per_row(284., 6), 3);
        assert_eq!(per_row(364., 6), 3);
        assert_eq!(per_row(364., 7), 4);
        assert_eq!(per_row(364., 9), 3);
        assert_eq!(per_row(364., 12), 4);
        // Dragged narrow: two.
        assert_eq!(per_row(170., 8), 2);
        assert!(split_tiles(px(284.), 6).large());
        assert!(!split_tiles(px(284.), 8).large());
    }
}
