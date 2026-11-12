//! Sidebar: favorite tiles, pins, This PC (an expandable folder tree), and tabs.
//!
//! Tree children are listed on a background thread (`Fs::read_dir`); the UI thread only renders results.
//! The context menu is the sidebar's own small menu (open in new tab, pin, favorites, ...).

use std::path::{Path, PathBuf};

use arke_core::filetype::FileKind;
use gpui::{
    AnimationExt, AnyElement, App, ClickEvent, Context, Entity, ExternalPaths, MouseButton,
    MouseDownEvent, Pixels, Point, SharedString, Window, anchored, deferred, div, prelude::*, px,
    relative, svg,
};
use rust_i18n::t;

use super::{DraggedTab, Tab, Workspace};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{DraggedFiles, Explorer, drop_external_into, drop_into, kind_icon};
use crate::settings::{FAVORITES_LARGE, FAVORITES_MAX, SettingsStore};
use crate::shell::{KnownFolder, Place, PlaceKind};
use crate::theme::{self, UiFont as _};
use crate::workspace::{Tooltip, TooltipBelowExt as _};

/// One tree indent level.
const INDENT: Pixels = px(16.);
/// Drive usage bar: thin, under the label, inside the row (the row keeps its height).
const USAGE_BAR_H: Pixels = px(3.);
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
    /// Remove from the sidebar Network section.
    ForgetShare(PathBuf),
    /// Remove a saved SFTP/WebDAV location (and its saved password).
    ForgetRemote(PathBuf),
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
    /// Copy or move the shelf's items into the active tab's folder.
    ShelfToHere {
        is_move: bool,
    },
    ShelfClear,
    ShelfRemove(PathBuf),
    /// Go to a shelf item's folder and select it.
    ShelfReveal(PathBuf),
}

/// A pinned folder being dragged, from the tiles or the list.
#[derive(Clone)]
pub(super) struct DraggedPin {
    pub(super) path: PathBuf,
    label: SharedString,
    icon: Icon,
    /// Dragged from the tiles: reorders them live while over other tiles (`workspace_tabdrag.rs`).
    pub(super) from_tile: bool,
}

impl Render for DraggedPin {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(theme::H_ROW)
            .px(theme::S_8)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_8)
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .child(crate::assets::tinted(
                self.icon,
                theme::ICON,
                theme::text_muted(),
            ))
            .child(self.label.clone())
    }
}

/// Starts a pin drag: records it (an empty list needs a drop target) and returns the drag preview.
fn start_pin_drag(
    workspace: &gpui::WeakEntity<Workspace>,
    dragged: &DraggedPin,
    cx: &mut App,
) -> Entity<DraggedPin> {
    let _ = workspace.update(cx, |ws, cx| {
        ws.dragging_pin = true;
        ws.tab_drag
            .start_tile(dragged.from_tile.then(|| dragged.path.clone()));
        cx.notify();
    });
    cx.new(|_| dragged.clone())
}

pub(super) struct SidebarMenu {
    position: Point<Pixels>,
    items: Vec<(SharedString, SidebarAction)>,
}

/// Whether `dir` has at least one non-hidden subfolder. Stops at the first one.
fn has_subfolder(fs: &dyn arke_core::Fs, dir: &Path) -> bool {
    fs.read_dir(dir).is_ok_and(|iter| {
        iter.filter_map(Result::ok)
            .any(|e| e.is_dir_like() && !e.is_hidden())
    })
}

/// Display name: the folder name, or the full path for a drive root (C:\).
fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
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

fn section_label(text: impl Into<SharedString>) -> impl IntoElement {
    let text: SharedString = text.into();
    div()
        .flex_none()
        .h(theme::H_ROW)
        .flex()
        .items_end()
        .px(theme::S_8)
        .pb(theme::S_6)
        // Section heading (like Files): item size, semibold and full ink, so the groups read as
        // headings rather than captions; the weight alone sets them apart from the items.
        .text_size(theme::FS_14)
        .font_weight(theme::FW_STRONG)
        .text_color(theme::text())
        .child(text)
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
                t!("sidebar.unpin")
            } else {
                t!("sidebar.pin")
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
        .size(theme::S_24)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(if horizontal {
                    Icon::TabsSide.path()
                } else {
                    Icon::TabsTop.path()
                })
                .size(theme::ICON)
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

/// Button next to the Network header: opens Arke's "Add network location" dialog.
fn map_drive_button(cx: &mut Context<Workspace>) -> impl IntoElement + use<> {
    div()
        .id("map-network-drive")
        .a11y_button(t!("sidebar.add_location"))
        .size(theme::S_24)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(Icon::Plus.path())
                .size(theme::ICON)
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
        .h(theme::H_ROW)
        .flex()
        .items_center()
        .pl(indent)
        .text_size(theme::FS_12)
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
        self.relative()
            .children(spring_bar)
            .can_drop(move |value, _, cx| crate::explorer::accepts_drop(value, &check, cx))
            .on_drag_move(move |e: &gpui::DragMoveEvent<DraggedFiles>, window, cx| {
                crate::explorer::report_drop_target(&hovered, e, cx);
                crate::spring::hover(
                    spring_key.clone(),
                    crate::spring::Action::Open(hovered.clone()),
                    crate::spring::Target::Workspace(workspace.clone()),
                    e,
                    window,
                    cx,
                );
            })
            .drag_over::<DraggedFiles>(|s, _, _, _| {
                s.bg(theme::signal_wash())
                    .border_color(theme::signal_line())
            })
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(theme::signal_wash()))
            .on_drop(move |dragged: &DraggedFiles, window, cx| {
                drop_into(dragged, dest.clone(), window, cx);
            })
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                let target = this.active_view().clone();
                drop_external_into(paths, external.clone(), &target, window, cx);
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
        self.move_pin(path, slot, cx);
    }

    /// A pin dropped in a gap of the tile grid. A list entry goes in the last slot; a tile keeps the
    /// slot it reached while dragging.
    fn drop_pin_between_tiles(
        &mut self,
        dragged: &DraggedPin,
        last: usize,
        cx: &mut Context<Self>,
    ) {
        if dragged.from_tile {
            self.dragging_pin = false;
            self.tab_drag.end_tile();
            cx.notify();
        } else {
            self.place_pin(&dragged.path, crate::settings::PinSlot::Tile(last), cx);
        }
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

    /// Pinned list row: draggable, flashes when just added.
    fn pin_list_row(ix: usize, path: &Path, row: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let dragged = DraggedPin {
            path: path.to_path_buf(),
            label: display_name(path).into(),
            icon: Icon::Folder,
            from_tile: false,
        };
        let row = div()
            .id(SharedString::from(format!("pin-row-{}", path.display())))
            .rounded(theme::R_8)
            .child(row)
            .on_drag(dragged, move |dragged, _, _, cx| {
                start_pin_drag(&workspace, dragged, cx)
            })
            .drag_over::<DraggedPin>(|s, _, _, _| s.bg(theme::signal_wash()))
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, _, cx| {
                this.place_pin(&dragged.path, crate::settings::PinSlot::List(ix), cx);
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
        let icon_for = |path: &Path| {
            match places.iter().find(|p| p.path == path) {
                Some(p) => place_icon(p.kind),
                // Neither a known folder nor a listed drive: drive roots (e.g. an unplugged USB
                // stick) get the drive icon, the rest a folder.
                None if path.parent().is_none() => Icon::Drive,
                None => Icon::Folder,
            }
        };
        let label_for = |path: &Path| {
            places
                .iter()
                .find(|p| p.path == path)
                .map_or_else(|| display_name(path), |p| p.label.clone())
        };
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .tiles(&places)
            .into_iter()
            .map(|p| {
                let (label, icon) = (label_for(&p), icon_for(&p));
                (p, label, icon)
            })
            .collect()
    }

    /// Pins section: draggable tiles, the "Pinned" header, and the overflow list below.
    fn render_pins(&self, current: Option<&Path>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let pinned = settings.pinned.clone();
        let recycle_pinned = settings.recycle_pinned;
        let favorites = self.favorites(cx);
        // Up to 6 favorites use 3 large tiles per row; more shrink to 4 per row, like Arc.
        let large = favorites.len() <= FAVORITES_LARGE;
        let tile_count = favorites.len();
        let tiles: Vec<AnyElement> = favorites
            .into_iter()
            .enumerate()
            .map(|(ix, (path, label, icon))| {
                let here = current == Some(path.as_path());
                let tile = Self::render_tile(ix, &path, label, icon, here, large, cx);
                self.tile_drag_shell(ix, &path, tile, cx)
            })
            .collect();
        let mut pinned_rows = Vec::new();
        for (ix, path) in pinned.iter().enumerate() {
            let row_at = pinned_rows.len();
            self.render_tree(
                path,
                display_name(path),
                None,
                None,
                0,
                current,
                &mut pinned_rows,
                cx,
            );
            // The list row itself (not its expanded children) is draggable: reorder or drop onto tiles.
            let row = pinned_rows.remove(row_at);
            pinned_rows.insert(row_at, Self::pin_list_row(ix, path, row, cx));
        }
        // With an empty list, a dragged tile still needs somewhere to drop.
        let list_drop = (pinned.is_empty() && self.dragging_pin).then(|| {
            div()
                .id("pin-list-drop")
                .h(theme::H_ROW)
                .mx(theme::S_4)
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::R_8)
                .border_1()
                .border_dashed()
                .border_color(theme::line_strong())
                .text_size(theme::FS_12)
                .text_color(theme::text_muted())
                .child(t!("sidebar.drop_to_list").into_owned())
                .drag_over::<DraggedPin>(|s, _, _, _| {
                    s.bg(theme::signal_wash())
                        .border_color(theme::signal_line())
                })
                .on_drop(cx.listener(|this, dragged: &DraggedPin, _, cx| {
                    this.place_pin(&dragged.path, crate::settings::PinSlot::List(0), cx);
                }))
        });
        let mut out = vec![
            div()
                .id("pin-tiles")
                .grid()
                .grid_cols(if large { 3 } else { 4 })
                .gap(theme::S_4)
                .children(tiles)
                .on_drop(cx.listener(move |this, dragged: &DraggedPin, _, cx| {
                    this.drop_pin_between_tiles(dragged, tile_count, cx);
                }))
                .into_any_element(),
        ];
        // The Recycle Bin is pinned by default and always sits last; it cannot be dragged.
        if recycle_pinned {
            pinned_rows.push(self.render_recycle_row(cx));
        }
        // No pins, no section. "Pin to sidebar" makes it appear; no placeholder text.
        if !pinned_rows.is_empty() || list_drop.is_some() {
            out.push(
                div()
                    .id("pin-list-label")
                    .child(section_label(t!("sidebar.pinned")))
                    // Dropping on the "Pinned" header puts it first in the list.
                    .on_drop(cx.listener(|this, dragged: &DraggedPin, _, cx| {
                        this.place_pin(&dragged.path, crate::settings::PinSlot::List(0), cx);
                    }))
                    .into_any_element(),
            );
        }
        out.extend(list_drop.map(IntoElement::into_any_element));
        // One parent for the pinned rows, so a folder pinned and also shown under This PC doesn't
        // repeat an element id (the screen reader tree needs them unique).
        if !pinned_rows.is_empty() {
            out.push(
                div()
                    .id("pinned")
                    .flex()
                    .flex_col()
                    .gap(theme::S_2)
                    .children(pinned_rows)
                    .into_any_element(),
            );
        }
        out
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let explorer = self.active_view().read(cx);
        let current = explorer.display_path().map(Path::to_path_buf);
        let places = explorer.places.clone();
        let pins = self.render_pins(current.as_deref(), cx);
        // This PC: user folders and all drives, all expandable.
        let mut devices = Vec::new();
        // Network drives go under Network instead.
        for place in places.iter().filter(|p| {
            matches!(
                p.kind,
                PlaceKind::Folder(KnownFolder::Profile)
                    | PlaceKind::Cloud
                    | PlaceKind::Drive { remote: false, .. }
                    | PlaceKind::Wsl
            )
        }) {
            self.render_device(place, current.as_deref(), &mut devices, cx);
        }
        let network = self.render_network(&places, current.as_deref(), cx);
        let tabs: Vec<AnyElement> = (0..self.tabs.len())
            .map(|ix| self.render_tab(ix, cx))
            .collect();

        div()
            .id("sidebar")
            .a11y(gpui::Role::Tree, t!("a11y.sidebar"))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .pl(theme::S_12)
            .pr(theme::S_8)
            .pb(theme::S_8)
            .gap(theme::S_2)
            .overflow_y_scroll()
            .children(pins)
            .children(self.render_shelf(cx))
            .child(section_label(t!("sidebar.this_pc")))
            .children(devices)
            .children(network)
            .when(!Self::horizontal_tabs(cx), |d| {
                d.child(
                    div()
                        .flex()
                        .items_end()
                        .child(div().flex_1().child(section_label(t!("sidebar.tabs"))))
                        .child(Self::workspaces_button(cx))
                        .child(tab_layout_button(false, cx)),
                )
                // Dragging an unpinned tab turns the New tab row into the pin zone (same
                // height, so the tabs under the cursor don't move).
                .child(match self.pin_drop_zone(cx) {
                    Some(zone) => zone.into_any_element(),
                    None => Self::render_new_tab_row(cx).into_any_element(),
                })
                .child(
                    div()
                        .id("tab-list")
                        .a11y(gpui::Role::TabList, t!("sidebar.tabs"))
                        .flex()
                        .flex_col()
                        .gap(theme::S_2)
                        .children(tabs)
                        // Dropping in the gaps between tabs moves the tab to the end.
                        .on_drop(cx.listener(Self::drop_tab_at_end)),
                )
                // Dropping in the empty space below the last tab moves it to the end.
                .child(
                    div()
                        .id("tab-list-end")
                        .flex_1()
                        .min_h(theme::S_32)
                        .on_drop(cx.listener(Self::drop_tab_at_end)),
                )
            })
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
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = vec![
            div()
                .flex()
                .items_end()
                .child(div().flex_1().child(section_label(t!("sidebar.network"))))
                .child(map_drive_button(cx))
                .into_any_element(),
        ];
        for place in places
            .iter()
            .filter(|p| matches!(p.kind, PlaceKind::Drive { remote: true, .. }))
        {
            self.render_device(place, current, &mut out, cx);
        }
        // Stored newest first (for eviction) but listed by name, so rows don't jump around as
        // shares are visited.
        let mut shares = SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        shares.sort_by_cached_key(|s| s.to_string_lossy().to_lowercase());
        for share in &shares {
            self.render_tree(
                share,
                share_label(share),
                None,
                Some(Icon::NetDrive),
                0,
                current,
                &mut out,
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
                remote_label(url),
                None,
                Some(Icon::NetDrive),
                0,
                current,
                &mut out,
                cx,
            );
        }
        out
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
            .h(theme::H_ROW)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_8)
            .px(theme::S_8)
            .rounded(theme::R_8)
            .cursor_pointer()
            .text_color(theme::text_subtle())
            .hover(|s| s.bg(theme::wash()).text_color(theme::text_muted()))
            .child(
                svg()
                    .path(Icon::TabNew.path())
                    .size(theme::ICON)
                    .text_color(theme::text_subtle()),
            )
            .child(div().flex_1().child(t!("sidebar.new_tab").to_string()))
            // The shortcut only appears on hover so it takes no space otherwise.
            .child(
                div()
                    .invisible()
                    .group_hover("new-tab", gpui::Styled::visible)
                    .px(theme::S_4)
                    .rounded(theme::R_4)
                    .border_1()
                    .border_color(theme::line_strong())
                    .text_size(theme::FS_11)
                    .child("Ctrl T"),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_palette(&super::OpenPalette, window, cx);
            }))
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tile(
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
        let menu_path = path.to_path_buf();
        let workspace = cx.entity().downgrade();
        let dragged = DraggedPin {
            path: path.to_path_buf(),
            label: label.clone().into(),
            icon,
            from_tile: true,
        };
        div()
            .id(SharedString::from(format!("tile-{}", path.display())))
            .a11y_lazy(gpui::Role::TreeItem, || label.clone())
            .aria_selected(here)
            .h(if large { px(56.) } else { px(48.) })
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(if large { theme::S_4 } else { theme::S_2 })
            .rounded(theme::R_8)
            .cursor_pointer()
            .text_size(theme::FS_12)
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
                if large { theme::S_20 } else { px(18.) },
                if here {
                    theme::signal()
                } else {
                    theme::text_muted()
                },
            ))
            .child(div().max_w_full().px(theme::S_4).truncate().child(label))
            .with_file_drop(path.to_path_buf(), cx)
            // Drag to reorder (live, see `tile_drag_shell`) or onto the list below.
            .on_drag(dragged, move |dragged, _, _, cx| {
                start_pin_drag(&workspace, dragged, cx)
            })
            // Only a list entry highlights its target; tiles make room instead.
            .drag_over::<DraggedPin>(|s, dragged, _, _| {
                if dragged.from_tile {
                    s
                } else {
                    s.bg(theme::signal_wash())
                }
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, _, cx| {
                cx.stop_propagation();
                this.place_pin(&dragged.path, crate::settings::PinSlot::Tile(ix), cx);
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
                    let p = menu_path.clone();
                    this.open_sidebar_menu(
                        e.position,
                        vec![
                            (
                                t!("sidebar.open_in_new_tab").to_string().into(),
                                SidebarAction::OpenInNewTab(p.clone()),
                            ),
                            (
                                t!("sidebar.move_to_list").to_string().into(),
                                SidebarAction::MoveToList(p.clone()),
                            ),
                            (
                                t!("sidebar.unpin").to_string().into(),
                                SidebarAction::TogglePin(p),
                            ),
                        ],
                        cx,
                    );
                }),
            )
            .tooltip(tooltip_text(
                t!("sidebar.tile_tooltip", path = path.display()).to_string(),
            ))
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
            #[allow(clippy::cast_precision_loss)]
            let used = 1. - free as f32 / total as f32;
            DriveSpace {
                free: arke_core::format::format_size(free),
                used: used.clamp(0., 1.),
            }
        });
        self.render_tree(
            &place.path,
            place.label.clone(),
            space,
            Some(icon),
            0,
            current,
            out,
            cx,
        );
    }

    /// An expandable folder; renders subfolders recursively when open.
    #[allow(clippy::too_many_arguments)]
    fn render_tree(
        &self,
        path: &Path,
        label: String,
        space: Option<DriveSpace>,
        icon: Option<Icon>,
        depth: usize,
        current: Option<&Path>,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let expanded = self.expanded.contains(path);
        // Just toggled: animate the chevron and the rows that appear.
        let cue = (self.tree_cue.0.as_deref() == Some(path)
            && self.tree_cue.1.live_at(std::time::Instant::now()))
        .then(|| self.tree_cue.1.serial());
        let here = current == Some(path);
        let children = self.tree_children.get(path);
        // No chevron when the node was listed with no children, or a background check found no subfolders,
        // so users do not expand a folder only to find it empty.
        let leaf = matches!(children, Some(Some(list)) if list.is_empty())
            || self.tree_leaf.contains(path);
        let open = path.to_path_buf();
        let tab_path = path.to_path_buf();
        let menu = self.tree_menu(path, depth, cx);
        #[allow(clippy::cast_precision_loss)]
        let indent = INDENT * depth as f32;
        out.push(
            div()
                .id(SharedString::from(format!(
                    "tree-{depth}-{}",
                    path.display()
                )))
                .a11y_lazy(gpui::Role::TreeItem, || label.clone())
                .aria_level(depth)
                .aria_selected(here)
                .when(!leaf, |d| d.aria_expanded(expanded))
                .relative()
                .h(theme::H_ROW)
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::S_4)
                .pl(indent + theme::S_4)
                .pr(theme::S_8)
                .rounded(theme::R_8)
                .cursor_pointer()
                // Current location: neutral wash.
                .when(here, |d| d.bg(theme::selection()))
                .when(!here, |d| d.hover(|s| s.bg(theme::wash())))
                .child(Self::tree_chevron(path, depth, expanded, leaf, cue, cx))
                .child(match icon {
                    Some(icon) => crate::assets::tinted(icon, theme::ICON, theme::text_muted()),
                    None => kind_icon(FileKind::Folder, theme::ICON),
                })
                .child(tree_label(label, space))
                .with_file_drop(path.to_path_buf(), cx)
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
                        out,
                        cx,
                    );
                }
            }
            _ => out.push(loading_row(indent + INDENT + theme::S_32)),
        }
        // Just expanded: the new rows drop in under their parent.
        if let Some(serial) = cue {
            let rows: Vec<AnyElement> = out.drain(start..).collect();
            out.extend(rows.into_iter().enumerate().map(|(ix, row)| {
                div()
                    .child(row)
                    .with_animation(
                        gpui::ElementId::Name(format!("tree-in-{serial}-{ix}").into()),
                        crate::motion::enter(),
                        |el, t| crate::motion::slide_in(el, t, 0., -crate::motion::NUDGE),
                    )
                    .into_any_element()
            }));
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
            .size(theme::ICON_SM)
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
            .size(theme::S_20)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .when(!leaf, |d| {
                d.hover(|s| s.bg(theme::wash_strong()))
                    .child(glyph)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_expand(toggle.clone(), cx);
                    }))
            })
    }

    /// Expands or collapses. Expanding lists subfolders on a background thread.
    pub(super) fn toggle_expand(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.tree_cue.0 = Some(path.clone());
        self.tree_cue.1.fire();
        if self.expanded.remove(&path) {
            cx.notify();
            return;
        }
        self.expanded.insert(path.clone());
        // Relist on every expand since the folder may have changed; show the old result meanwhile.
        self.tree_children.entry(path.clone()).or_insert(None);
        let fs = self.services.fs.clone();
        let shell = self.services.shell.clone();
        let (tx, rx) = flume::bounded::<(Vec<PathBuf>, Vec<PathBuf>)>(1);
        let dir = path.clone();
        let spawned = std::thread::Builder::new()
            .name("tree".into())
            .spawn(move || {
                let mut dirs: Vec<PathBuf> = match fs.read_dir(&dir) {
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
                // Check each subfolder for nested folders and hide the chevron when there are none.
                // Local disks only; network paths can be slow.
                let leaves = if shell.is_local_disk(&dir) {
                    dirs.iter()
                        .filter(|d| !has_subfolder(fs.as_ref(), d))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                let _ = tx.send((dirs, leaves));
            });
        if spawned.is_err() {
            return;
        }
        cx.spawn(async move |this, cx| {
            if let Ok((dirs, leaves)) = rx.recv_async().await {
                let _ = this.update(cx, |this, cx| {
                    for dir in &dirs {
                        this.tree_leaf.remove(dir);
                    }
                    this.tree_leaf.extend(leaves);
                    this.tree_children.insert(path, Some(dirs));
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    // --- Context menu ---

    pub(super) fn open_sidebar_menu(
        &mut self,
        position: Point<Pixels>,
        items: Vec<(SharedString, SidebarAction)>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_menu = Some(SidebarMenu { position, items });
        cx.stop_propagation();
        cx.notify();
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
            let remotes = self.services.remotes.clone();
            crate::explorer::spawn_thread("remote-forget", move || remotes.forget(&gone));
        }
    }

    fn run_sidebar_action(
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
            SidebarAction::SetRecyclePinned(pinned) => Self::set_recycle_pinned(pinned, cx),
            SidebarAction::OpenInNewTab(path) => {
                let at = self.active + 1;
                self.open_tab(path, at, true, window, cx);
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
            SidebarAction::ForgetShare(share) => {
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(|st| st.forget_share(&share), cx);
                });
            }
            SidebarAction::ForgetRemote(url) => self.forget_remote(&url, cx),
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
                view.update(cx, |v, cx| v.navigate(home, arke_core::NavKind::New, cx));
            }
        }
    }

    /// Moves tabs on this drive to the user folder first (Arke's own watchers must not block the eject),
    /// then asks Windows to eject. Reports who holds the drive if it is in use.
    fn eject_drive(&mut self, drive: PathBuf, cx: &mut Context<Self>) {
        self.leave_drive(&drive, cx);
        let shell = self.active_view().read(cx).services.shell.clone();
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
        let shell = self.active_view().read(cx).services.shell.clone();
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
        let shell = self.active_view().read(cx).services.shell.clone();
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

    pub(super) fn render_sidebar_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.sidebar_menu.as_ref()?;
        let rows: Vec<AnyElement> = menu
            .items
            .iter()
            .enumerate()
            .map(|(ix, (label, action))| {
                let action = action.clone();
                div()
                    .id(("sidebar-menu-item", ix))
                    .h(theme::H_CTL_SM + theme::S_4)
                    .mx(theme::S_4)
                    .px(theme::S_8)
                    .flex()
                    .items_center()
                    .rounded(theme::R_8)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::wash()))
                    .child(label.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run_sidebar_action(action.clone(), window, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            deferred(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(theme::S_8)
                    .child(
                        div()
                            .id("sidebar-menu")
                            // Occlude items below; otherwise the covered item thinks it is hovered and its tooltip covers the menu.
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                this.sidebar_menu = None;
                                cx.notify();
                            }))
                            .min_w(px(200.))
                            .py(theme::S_4)
                            .rounded(theme::R_12)
                            .bg(theme::overlay())
                            .shadow(theme::elev_float())
                            .ui_font()
                            .text_size(theme::FS_14)
                            .text_color(theme::text())
                            .children(rows)
                            // Like the list's context menu: slides down and fades in, again for
                            // each new right-click position.
                            .with_animation(
                                gpui::ElementId::Name(
                                    format!("sidebar-menu-{:?}", menu.position).into(),
                                ),
                                crate::motion::enter(),
                                |el, t| crate::motion::slide_in(el, t, 0., -crate::motion::SLIDE),
                            ),
                    ),
            )
            .with_priority(1)
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

    fn tab_menu(&self, ix: usize) -> Vec<(SharedString, SidebarAction)> {
        let pinned = self.tabs.get(ix).is_some_and(|t| t.pinned);
        let split = self.tabs.get(ix).is_some_and(Tab::is_split);
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
            (
                // Ctrl+W closes the tab only when it isn't split (else the active side).
                if split {
                    t!("sidebar.close_tab")
                } else {
                    t!("sidebar.close_tab_key")
                }
                .to_string()
                .into(),
                SidebarAction::CloseTab(ix),
            ),
        ];
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
                    .h(theme::H_CTL_SM)
                    .px(theme::S_6)
                    .flex()
                    .items_center()
                    .gap(theme::S_6)
                    .rounded(theme::R_4)
                    .when(active_pane == Some(pane), |d| d.bg(theme::signal_wash()))
                    .child(kind_icon(FileKind::Folder, theme::ICON))
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
            .size(theme::S_20)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Pin.path())
                    .size(theme::ICON_SM)
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
            .size(theme::S_20)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme::ICON_SM)
                    .text_color(theme::text_subtle()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.close_tab(ix, window, cx);
            }))
    }

    /// One tab. `horizontal` uses the top tab strip style (wider, joined to the paper at the bottom).
    #[allow(clippy::too_many_lines)]
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
                theme::H_ROW + theme::S_8
            } else if tab.pinned {
                // Pinned tabs are a tighter group at the top.
                theme::H_ROW_COMPACT
            } else {
                theme::H_ROW
            })
            .when(tab.pinned, |d| d.text_size(theme::FS_13))
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_4)
            .pl(if split { theme::S_4 } else { theme::S_8 })
            .pr(theme::S_6)
            .rounded(theme::R_8)
            .cursor_pointer()
            // Slides from the old position after a reorder (the offset is relative).
            .relative()
            // The active tab shares the paper surface with the list.
            .when(active, |d| d.child(here_layer()).text_color(theme::text()))
            .when(!active, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
            })
            // Dragging another tab over this one reorders live (workspace_tabdrag.rs); dropping on the right half
            // of the content area opens it side by side. Files dropped on a tab go into its current folder.
            .when_some(
                tab.view().read(cx).current_path().map(Path::to_path_buf),
                |d, dest| d.with_file_drop(dest, cx),
            )
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
                    let items = this.tab_menu(ix);
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
                    .size(theme::ICON)
                    .flex_none()
                    .text_color(theme::text_muted())
                    .into_any_element(),
                None => kind_icon(FileKind::Folder, theme::ICON),
            };
            row.gap(theme::S_8)
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
                .gap(theme::S_2)
                .child(shell)
                .child(
                    div()
                        .h(px(1.))
                        .mx(theme::S_8)
                        .my(theme::S_2)
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

    pub(crate) fn tree_children_for_test(&self, path: &Path) -> Option<Vec<PathBuf>> {
        self.tree_children.get(path).cloned().flatten()
    }

    pub(crate) fn tree_is_leaf_for_test(&self, path: &Path) -> bool {
        self.tree_leaf.contains(path)
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
            .pl(theme::S_4)
            .child(label);
    };
    div()
        .flex_1()
        .min_w_0()
        .pl(theme::S_4)
        .flex()
        .flex_col()
        .gap(theme::S_2)
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme::S_8)
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(
                    div()
                        .flex_none()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_subtle())
                        .child(space.free),
                ),
        )
        .child(
            div()
                .h(USAGE_BAR_H)
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
        .rounded(theme::R_8)
        .bg(theme::paper())
        .shadow(theme::elev_1())
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
