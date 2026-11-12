//! User settings: pins, favorite tiles, recent folders, tab layout, density, and the Settings page toggles.
//!
//! Stored in `%APPDATA%\Arke\settings.json`, never the registry. Reads and writes happen on background
//! threads. Startup renders with defaults and updates once the file is read. Writes are debounced to
//! coalesce bursts, and go to a temp file that is then renamed so a crash cannot leave a broken file.
//!
//! One app-wide copy (`Global`) shared by all windows.

use std::path::{Path, PathBuf};
use std::time::Duration;

use arke_core::sort::{SortColumn, SortSpec};
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::explorer::ViewMode;

/// Debounce for coalescing consecutive changes into one write.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// How long startup waits for the settings file (read in the background, usually 1-2 ms). If it arrives
/// in time the first window opens with the saved size, tabs and language; otherwise defaults are used
/// and the file is applied when it arrives (e.g. %APPDATA% on a slow network).
const STARTUP_WAIT: Duration = Duration::from_millis(60);
/// Number of recent folders to remember.
pub(crate) const RECENT_MAX: usize = 50;
/// Max favorite tiles; further pins go to the list below.
pub(crate) const FAVORITES_MAX: usize = 12;
/// Above this many tiles they shrink to 4 per row.
pub(crate) const FAVORITES_LARGE: usize = 6;

/// Where a pin ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinPlace {
    Tiles,
    /// Tiles were full, so it went to the list below.
    List,
    /// Already pinned; nothing changed.
    Already,
}

/// Drop target for a dragged pin: an index in the tiles or the list (past the end means last).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinSlot {
    Tile(usize),
    List(usize),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TabLayout {
    /// Tabs in the sidebar (Arc).
    #[default]
    Vertical,
    /// Tabs on top (browsers, Windows 11 Explorer).
    Horizontal,
}

/// Dark, light, or follow the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Accent color for selection, focus and buttons. Stored as a string: `"arke"` (default orange),
/// `"windows"` (system accent) or `"#rrggbb"`. Anything else falls back to the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub(crate) enum Accent {
    #[default]
    Arke,
    Windows,
    /// 0xRRGGBB
    Color(u32),
}

impl Accent {
    /// Parses `#rrggbb`, `rrggbb` or `#rgb`, case-insensitive.
    pub(crate) fn parse_hex(text: &str) -> Option<u32> {
        let digits = text.trim().trim_start_matches('#');
        if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok()?;
        match digits.len() {
            6 => Some(value),
            3 => {
                let (r, g, b) = ((value >> 8) & 0xf, (value >> 4) & 0xf, value & 0xf);
                Some(((r * 17) << 16) | ((g * 17) << 8) | (b * 17))
            }
            _ => None,
        }
    }

    pub(crate) fn hex(color: u32) -> String {
        format!("#{:06X}", color & 0x00ff_ffff)
    }
}

impl From<String> for Accent {
    fn from(text: String) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "windows" => Self::Windows,
            other => Self::parse_hex(other).map_or(Self::Arke, Self::Color),
        }
    }
}

impl From<Accent> for String {
    fn from(accent: Accent) -> Self {
        match accent {
            Accent::Arke => "arke".into(),
            Accent::Windows => "windows".into(),
            Accent::Color(color) => Accent::hex(color).to_ascii_lowercase(),
        }
    }
}

/// Tint applied to the frame (sidebar, top bar). Stored as a string: `"none"` (neutral gray),
/// `"accent"` (follow accent) or `"#rrggbb"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub(crate) enum Tint {
    #[default]
    None,
    Accent,
    Color(u32),
}

impl From<String> for Tint {
    fn from(text: String) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "accent" => Self::Accent,
            other => Accent::parse_hex(other).map_or(Self::None, Self::Color),
        }
    }
}

impl From<Tint> for String {
    fn from(tint: Tint) -> Self {
        match tint {
            Tint::None => "none".into(),
            Tint::Accent => "accent".into(),
            Tint::Color(color) => Accent::hex(color).to_ascii_lowercase(),
        }
    }
}

/// Window background material. Mica needs Windows 11; everything else is solid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Material {
    #[default]
    Mica,
    MicaAlt,
    Solid,
}

/// Where a new window opens when no path is given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StartLocation {
    /// User profile folder.
    #[default]
    Home,
    /// The last folder visited.
    LastFolder,
    /// All tabs from last time, including splits. If a window is already open, new windows use the profile folder.
    LastSession,
    /// Home page (recent files, drives, network locations).
    HomePage,
}

/// A saved tab: the folder of each of its one or two panes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionTab {
    pub(crate) panes: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) active_pane: usize,
    #[serde(default = "half")]
    pub(crate) ratio: f32,
    /// Pinned tabs stay at the top of the list and are spared by "Close other tabs".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) pinned: bool,
    /// Dual pane: browsing in one side repeats in the other.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) synced: bool,
}

const fn half() -> f32 {
    0.5
}

/// Last session, from the window that changed most recently.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Session {
    pub(crate) tabs: Vec<SessionTab>,
    pub(crate) active: usize,
}

/// Last window position and size in logical pixels. When maximized this is the restored size.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowPlace {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    #[serde(default)]
    pub(crate) maximized: bool,
}

/// Default sort column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SortBy {
    #[default]
    Name,
    Modified,
    Type,
    Size,
    Created,
    Extension,
    Attributes,
}

impl SortBy {
    /// The list column this sorts by.
    pub(crate) const fn column(self) -> SortColumn {
        match self {
            Self::Name => SortColumn::Name,
            Self::Modified => SortColumn::Modified,
            Self::Type => SortColumn::Type,
            Self::Size => SortColumn::Size,
            Self::Created => SortColumn::Created,
            Self::Extension => SortColumn::Extension,
            Self::Attributes => SortColumn::Attributes,
        }
    }

    /// The stored name for a list column (`SortColumn` lives in core and has no serde).
    pub(crate) const fn from_column(column: SortColumn) -> Self {
        match column {
            SortColumn::Name => Self::Name,
            SortColumn::Modified => Self::Modified,
            SortColumn::Type => Self::Type,
            SortColumn::Size => Self::Size,
            SortColumn::Created => Self::Created,
            SortColumn::Extension => Self::Extension,
            SortColumn::Attributes => Self::Attributes,
        }
    }
}

/// A saved SFTP or WebDAV location.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct RemotePlace {
    /// Canonical URL (`sftp://user@host:port/path`, `https://host/path`).
    pub(crate) url: PathBuf,
    /// SFTP private key file; `None` signs in with a password or an SSH agent.
    pub(crate) key_file: Option<PathBuf>,
}

/// User-resized column widths in pixels (`None` is the default). For Name this is a cap; it still shrinks in narrow windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ColumnWidths {
    pub(crate) name: Option<u16>,
    pub(crate) modified: Option<u16>,
    pub(crate) created: Option<u16>,
    #[serde(rename = "type")]
    pub(crate) kind: Option<u16>,
    pub(crate) size: Option<u16>,
    pub(crate) extension: Option<u16>,
    pub(crate) attributes: Option<u16>,
    pub(crate) link_target: Option<u16>,
}

impl ColumnWidths {
    pub(crate) const fn get(&self, col: crate::prefs::Col) -> Option<u16> {
        use crate::prefs::Col;
        match col {
            Col::Name => self.name,
            Col::Modified => self.modified,
            Col::Created => self.created,
            Col::Type => self.kind,
            Col::Size => self.size,
            Col::Extension => self.extension,
            Col::Attributes => self.attributes,
            Col::LinkTarget => self.link_target,
        }
    }

    pub(crate) const fn set(&mut self, col: crate::prefs::Col, width: Option<u16>) {
        use crate::prefs::Col;
        let slot = match col {
            Col::Name => &mut self.name,
            Col::Modified => &mut self.modified,
            Col::Created => &mut self.created,
            Col::Type => &mut self.kind,
            Col::Size => &mut self.size,
            Col::Extension => &mut self.extension,
            Col::Attributes => &mut self.attributes,
            Col::LinkTarget => &mut self.link_target,
        };
        *slot = width;
    }
}

/// Where folders sort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FolderOrder {
    /// Folders always first, as in Windows Explorer.
    #[default]
    First,
    /// Mixed with files.
    Mixed,
}

/// UI language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Language {
    #[default]
    System,
    ZhTw,
    ZhCn,
    En,
}

impl Language {
    /// Locale for this option. `ARKE_LANG` always wins (tests, debugging).
    pub(crate) fn locale(self) -> String {
        if std::env::var_os("ARKE_LANG").is_some() {
            return crate::win::ui_locale();
        }
        match self {
            Self::System => crate::win::ui_locale(),
            Self::ZhTw => "zh-TW".into(),
            Self::ZhCn => "zh-CN".into(),
            Self::En => "en".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Density {
    Compact,
    #[default]
    Standard,
    Comfortable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Settings {
    /// Pinned folders that do not fit on the tiles: the list under the tiles, in user order.
    pub(crate) pinned: Vec<PathBuf>,
    /// Pinned folders on the tiles (at most [`FAVORITES_MAX`]). `None` means the defaults
    /// (Desktop, Downloads, Documents, Pictures, Music, Videos).
    pub(crate) favorites: Option<Vec<PathBuf>>,
    /// Recycle Bin row at the end of the pinned list.
    pub(crate) recycle_pinned: bool,
    /// Recent folders, newest first (for Ctrl+T).
    pub(crate) recent: Vec<PathBuf>,
    pub(crate) tab_layout: TabLayout,
    pub(crate) density: Density,
    /// Preview pane on the right.
    pub(crate) preview_pane: bool,
    /// Ctrl+T searches the whole PC. Needs an index, off by default.
    pub(crate) global_search: bool,
    /// Global search skips build and package cache folders (`node_modules`, Rust `target`, ...).
    pub(crate) search_skip_build_dirs: bool,
    /// Remember opened files and visited folders to rank "This PC" results
    /// (`search_history.rs`). Local only.
    pub(crate) search_history: bool,
    /// Network locations added to global search by the user. One index each; the last index is used when offline.
    pub(crate) search_network: Vec<PathBuf>,
    /// Also add network drives and recently opened shares to global search automatically.
    pub(crate) search_network_auto: bool,
    /// Auto-added network locations the user removed.
    pub(crate) search_network_excluded: Vec<PathBuf>,
    /// Local drives global search does not index (`D:\`).
    pub(crate) search_skipped_drives: Vec<PathBuf>,
    /// Folders global search does not index. The folder itself is still found, its contents are not.
    pub(crate) search_excluded: Vec<PathBuf>,
    /// Recently opened network shares (`\\server\share`), newest first (sidebar Network section).
    pub(crate) net_places: Vec<PathBuf>,
    /// Saved SFTP and WebDAV locations (sidebar Network section). Never holds a password: those
    /// stay in Windows Credential Manager.
    pub(crate) remote_places: Vec<RemotePlace>,
    pub(crate) theme: ThemeMode,
    pub(crate) accent: Accent,
    pub(crate) tint: Tint,
    pub(crate) material: Material,
    /// UI font name; empty means default.
    pub(crate) font: String,
    /// Image drawn under the file list.
    pub(crate) background_image: Option<PathBuf>,
    /// Background image opacity in percent.
    pub(crate) background_opacity: u8,
    /// Show hidden items (Ctrl+H, applies to all tabs).
    pub(crate) show_hidden: bool,
    pub(crate) start_location: StartLocation,
    /// Show file extensions. When off the list shows only the stem; rename still edits the full name.
    pub(crate) show_extensions: bool,
    /// Thumbnails for images in grid view; type icons when off.
    pub(crate) show_thumbnails: bool,
    /// Thumbnails on network locations too. Reads images from the server, off by default.
    pub(crate) network_thumbnails: bool,
    /// Default sort for new tabs; changes also apply to tabs that have not picked their own sort.
    pub(crate) sort_by: SortBy,
    pub(crate) sort_descending: bool,
    pub(crate) folders: FolderOrder,
    /// View mode for new tabs.
    pub(crate) default_view: ViewMode,
    /// Remember the view and sort for each folder (`folder_views.rs`), like Windows File Explorer.
    /// Off: every tab keeps its own view and sort, following the defaults until changed.
    pub(crate) remember_folder_views: bool,
    /// Keep running in the background after the last window closes, for faster reopening.
    pub(crate) keep_running: bool,
    /// Show folders opened from anywhere in Arke instead of File Explorer (`win_takeover.rs`).
    /// Only while Arke runs in the background; needs `keep_running`.
    pub(crate) open_folders_in_arke: bool,
    pub(crate) language: Language,
    /// Ask before moving to the Recycle Bin. Permanent deletes are always confirmed by Windows.
    pub(crate) confirm_recycle: bool,
    /// Ask when a rename changes the extension.
    pub(crate) warn_extension_change: bool,
    /// Copies, moves and deletes on the same physical drive run one after another.
    pub(crate) queue_same_drive: bool,
    /// Read copies back and compare them with their sources.
    pub(crate) verify_copies: bool,
    /// Hand copies and moves to TeraCopy (offered only when it is installed).
    pub(crate) use_teracopy: bool,
    /// Double click on empty list space goes up one level.
    pub(crate) double_click_up: bool,
    /// Compute folder sizes in the background (local disks only).
    pub(crate) folder_sizes: bool,
    /// Treat dot-prefixed items (`.git`, `.vscode`) as hidden.
    pub(crate) hide_dotfiles: bool,
    /// Show protected system files (hidden + system). Hidden by default even when hidden items are shown.
    pub(crate) show_protected: bool,
    /// Open with a single click instead of double.
    pub(crate) single_click: bool,
    /// Selection checkboxes to the left of items.
    pub(crate) show_checkboxes: bool,
    /// Group rows in details view by the sort column.
    pub(crate) group_rows: bool,
    /// Details view: folders get an expand arrow and list their contents indented below them.
    pub(crate) details_tree: bool,
    /// Full date and time for modified dates instead of the short form ("Today 07:49").
    pub(crate) full_dates: bool,
    /// Visible columns in details view.
    pub(crate) show_modified: bool,
    /// Created column in details view, hidden by default.
    pub(crate) show_created: bool,
    /// Resized column widths, shared app-wide.
    pub(crate) column_widths: ColumnWidths,
    /// Width the preview pane was dragged to, app-wide (`None`: follows the window).
    pub(crate) preview_width: Option<u16>,
    pub(crate) show_type: bool,
    pub(crate) show_size: bool,
    /// Extension, Attributes (R/H/S/A) and Link target columns, hidden by default.
    pub(crate) show_extension: bool,
    pub(crate) show_attributes: bool,
    pub(crate) show_link_target: bool,
    /// Details columns after Name, left to right, as dragged in the header (`prefs::Col` keys).
    /// Unknown keys are ignored and missing columns keep their default place, so files from
    /// other versions still load.
    pub(crate) column_order: Vec<String>,
    /// Sizes in base 1000 instead of 1024 (the Windows default).
    pub(crate) decimal_sizes: bool,
    /// Notification area icon while running in the background.
    pub(crate) tray_icon: bool,
    /// Arke context menu entries turned off (keys from `workspace_menuprefs.rs`).
    pub(crate) menu_hidden: Vec<String>,
    /// Windows commands pinned to the main menu from "More options" (`explorer_menu::pin_key` keys, in pin order).
    pub(crate) menu_pinned: Vec<String>,
    /// Auto update: channel (`None` follows this build's channel), whether to check automatically,
    /// and the last check time (Unix seconds).
    pub(crate) update_channel_choice: Option<crate::build_info::Channel>,
    pub(crate) update_auto: bool,
    pub(crate) update_checked: Option<u64>,
    /// "Open with": custom apps (exe paths) and detected editors to hide.
    pub(crate) open_with_extra: Vec<PathBuf>,
    pub(crate) open_with_hidden: Vec<PathBuf>,
    /// Start in the background at Windows sign-in, via a shortcut in the Startup folder (no registry).
    pub(crate) autostart: bool,
    /// A shortcut in the Start menu (`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Arke.lnk`),
    /// rewritten at every start so it follows a moved exe. No registry.
    pub(crate) start_menu: bool,
    /// The first-run card offering the Start menu shortcut was answered or dismissed.
    pub(crate) start_menu_offered: bool,
    /// Last session, for "Open new windows at: last session".
    pub(crate) session: Session,
    /// Last window position and size.
    pub(crate) window: Option<WindowPlace>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            favorites: None,
            recycle_pinned: true,
            recent: Vec::new(),
            tab_layout: TabLayout::default(),
            density: Density::default(),
            preview_pane: false,
            global_search: false,
            search_network: Vec::new(),
            search_network_auto: true,
            search_network_excluded: Vec::new(),
            search_skipped_drives: Vec::new(),
            search_excluded: Vec::new(),
            search_skip_build_dirs: false,
            search_history: true,
            net_places: Vec::new(),
            remote_places: Vec::new(),
            theme: ThemeMode::default(),
            accent: Accent::default(),
            tint: Tint::default(),
            material: Material::default(),
            font: String::new(),
            background_image: None,
            background_opacity: 20,
            show_hidden: false,
            start_location: StartLocation::default(),
            show_extensions: true,
            show_thumbnails: true,
            network_thumbnails: false,
            sort_by: SortBy::default(),
            sort_descending: false,
            folders: FolderOrder::default(),
            default_view: ViewMode::default(),
            remember_folder_views: true,
            keep_running: true,
            open_folders_in_arke: false,
            language: Language::default(),
            confirm_recycle: false,
            warn_extension_change: true,
            queue_same_drive: true,
            verify_copies: false,
            use_teracopy: false,
            double_click_up: false,
            folder_sizes: true,
            hide_dotfiles: false,
            show_protected: false,
            single_click: false,
            full_dates: false,
            show_modified: true,
            show_created: false,
            column_widths: ColumnWidths::default(),
            preview_width: None,
            show_type: true,
            show_size: true,
            show_extension: false,
            show_attributes: false,
            show_link_target: false,
            column_order: Vec::new(),
            decimal_sizes: false,
            tray_icon: true,
            menu_hidden: Vec::new(),
            menu_pinned: Vec::new(),
            update_channel_choice: None,
            update_auto: true,
            update_checked: None,
            open_with_extra: Vec::new(),
            open_with_hidden: Vec::new(),
            show_checkboxes: false,
            group_rows: false,
            details_tree: false,
            autostart: false,
            start_menu: false,
            start_menu_offered: false,
            session: Session::default(),
            window: None,
        }
    }
}

/// Max entries in the sidebar Network section.
const NET_PLACES_MAX: usize = 6;

impl Settings {
    /// Default sort from settings.
    pub(crate) fn default_sort(&self) -> SortSpec {
        SortSpec::by(
            self.sort_by.column(),
            !self.sort_descending,
            self.folders == FolderOrder::Mixed,
        )
    }

    pub(crate) fn remember(&mut self, path: &Path) {
        if self.recent.first().map(PathBuf::as_path) == Some(path) {
            return;
        }
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(RECENT_MAX);
    }

    /// Records the share root of a network location that was opened. Only real SMB servers: WSL
    /// distros and localhost already have their own place under This PC.
    pub(crate) fn remember_share(&mut self, path: &Path) {
        if arke_core::net::server_of(path).is_none() {
            return;
        }
        let Some(share) = arke_core::net::share_root(path) else {
            return;
        };
        if self.net_places.first() == Some(&share) {
            return;
        }
        self.net_places
            .retain(|p| !p.as_os_str().eq_ignore_ascii_case(share.as_os_str()));
        self.net_places.insert(0, share);
        self.net_places.truncate(NET_PLACES_MAX);
    }

    pub(crate) fn forget_share(&mut self, share: &Path) {
        self.net_places.retain(|p| p != share);
    }

    /// Adds or updates a saved remote location (same URL replaces), newest first.
    pub(crate) fn save_remote_place(&mut self, place: RemotePlace) {
        self.remote_places.retain(|p| p.url != place.url);
        self.remote_places.insert(0, place);
    }

    pub(crate) fn forget_remote_place(&mut self, url: &Path) {
        self.remote_places.retain(|p| p.url != url);
    }

    /// Folders on the tiles: the custom list, or the known folders (excluding the profile folder itself).
    pub(crate) fn tiles(&self, places: &[crate::shell::Place]) -> Vec<PathBuf> {
        use crate::shell::{KnownFolder, PlaceKind};
        match &self.favorites {
            Some(list) => list.clone(),
            None => places
                .iter()
                .filter(|p| matches!(p.kind, PlaceKind::Folder(kf) if kf != KnownFolder::Profile))
                .map(|p| p.path.clone())
                .take(FAVORITES_MAX)
                .collect(),
        }
    }

    /// Whether the folder is pinned, on the tiles or in the list.
    pub(crate) fn is_pinned(&self, path: &Path, places: &[crate::shell::Place]) -> bool {
        self.pinned.iter().any(|p| p == path) || self.tiles(places).iter().any(|p| p == path)
    }

    /// Pins to the tiles if there is room, otherwise to the list. Already pinned folders stay put. Returns where it went.
    pub(crate) fn pin(&mut self, path: &Path, places: &[crate::shell::Place]) -> PinPlace {
        if self.is_pinned(path, places) {
            return PinPlace::Already;
        }
        let mut tiles = self.tiles(places);
        if tiles.len() < FAVORITES_MAX {
            tiles.push(path.to_path_buf());
            self.favorites = Some(tiles);
            PinPlace::Tiles
        } else {
            self.pinned.push(path.to_path_buf());
            PinPlace::List
        }
    }

    /// Drags a pinned folder to `slot`: reorders within an area, moves across areas.
    /// Dropping a list entry on full tiles swaps it with that tile (the displaced tile takes its list slot).
    pub(crate) fn place_pin(&mut self, path: &Path, slot: PinSlot, places: &[crate::shell::Place]) {
        let mut tiles = self.tiles(places);
        let mut list = self.pinned.clone();
        let in_tiles = tiles.iter().position(|p| p == path);
        let in_list = list.iter().position(|p| p == path);
        match (slot, in_tiles, in_list) {
            (PinSlot::Tile(to), Some(from), _) => {
                let moved = tiles.remove(from);
                tiles.insert(to.min(tiles.len()), moved);
            }
            (PinSlot::Tile(to), None, Some(from)) => {
                let moved = list.remove(from);
                if tiles.len() >= FAVORITES_MAX {
                    let to = to.min(tiles.len() - 1);
                    let bumped = std::mem::replace(&mut tiles[to], moved);
                    list.insert(from, bumped);
                } else {
                    tiles.insert(to.min(tiles.len()), moved);
                }
            }
            (PinSlot::List(to), _, Some(from)) => {
                let moved = list.remove(from);
                list.insert(to.min(list.len()), moved);
            }
            (PinSlot::List(to), Some(from), None) => {
                let moved = tiles.remove(from);
                list.insert(to.min(list.len()), moved);
            }
            // Unpinned folders cannot be added by dragging.
            _ => return,
        }
        self.favorites = Some(tiles);
        self.pinned = list;
    }

    /// Unpins, from the tiles or the list.
    pub(crate) fn unpin(&mut self, path: &Path, places: &[crate::shell::Place]) {
        let tiles = self.tiles(places);
        if tiles.iter().any(|p| p == path) {
            self.favorites = Some(tiles.into_iter().filter(|p| p != path).collect());
        }
        self.pinned.retain(|p| p != path);
    }

    /// Pins or unpins. Returns where the pin went, or `None` when unpinned.
    pub(crate) fn toggle_pin(
        &mut self,
        path: &Path,
        places: &[crate::shell::Place],
    ) -> Option<PinPlace> {
        if self.is_pinned(path, places) {
            self.unpin(path, places);
            None
        } else {
            Some(self.pin(path, places))
        }
    }

    /// Moves a tile to the front of the list below.
    pub(crate) fn move_to_list(&mut self, path: &Path, places: &[crate::shell::Place]) {
        let tiles = self.tiles(places);
        if !tiles.iter().any(|p| p == path) {
            return;
        }
        self.favorites = Some(tiles.into_iter().filter(|p| p != path).collect());
        self.pinned.retain(|p| p != path);
        self.pinned.insert(0, path.to_path_buf());
    }

    /// Moves a list entry to the last tile. Returns `false` and does nothing if the tiles are full.
    pub(crate) fn move_to_tiles(&mut self, path: &Path, places: &[crate::shell::Place]) -> bool {
        let mut tiles = self.tiles(places);
        if tiles.iter().any(|p| p == path) {
            return true;
        }
        if tiles.len() >= FAVORITES_MAX {
            return false;
        }
        self.pinned.retain(|p| p != path);
        tiles.push(path.to_path_buf());
        self.favorites = Some(tiles);
        true
    }
}

pub(crate) struct SettingsStore {
    pub(crate) settings: Settings,
    /// The settings file has been read; defaults are used until then.
    pub(crate) loaded: bool,
    /// Last folder before the previous exit (first recent entry at load time), for "Open at last location".
    pub(crate) last_session_folder: Option<PathBuf>,
    /// There was no settings file at startup: Arke runs for the first time with this profile.
    pub(crate) first_run: bool,
    path: Option<PathBuf>,
    save_task: Option<Task<()>>,
}

pub(crate) struct GlobalSettings(pub(crate) Entity<SettingsStore>);

impl Global for GlobalSettings {}

impl SettingsStore {
    /// Creates the global settings and reads the file in the background. With `path` `None` it is memory only (tests).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            settings: Settings::default(),
            loaded: false,
            last_session_folder: None,
            first_run: false,
            path: path.clone(),
            save_task: None,
        });
        cx.set_global(GlobalSettings(store.clone()));
        if let Some(path) = path {
            let (tx, rx) = flume::bounded::<(Settings, bool)>(1);
            let spawned = std::thread::Builder::new()
                .name("settings-load".into())
                .spawn(move || {
                    let loaded = load(&path);
                    // A broken file is not a first run: it is kept, and defaults are used.
                    let first_run = loaded.is_none() && !path.exists();
                    // No file (first run) still has to report "loaded" with defaults, otherwise nothing is saved until the next launch.
                    let _ = tx.send((loaded.unwrap_or_default(), first_run));
                });
            if spawned.is_ok() {
                // Wait briefly. The UI thread does not touch the file, it only waits for the background result.
                if let Ok((loaded, first_run)) = rx.recv_timeout(STARTUP_WAIT) {
                    store.update(cx, |store, cx| {
                        store.first_run = first_run;
                        store.merge_loaded(loaded, cx);
                    });
                    return store;
                }
                let weak = store.downgrade();
                cx.spawn(async move |cx| {
                    if let Ok((loaded, first_run)) = rx.recv_async().await {
                        let _ = weak.update(cx, |store, cx| {
                            store.first_run = first_run;
                            store.merge_loaded(loaded, cx);
                        });
                    }
                })
                .detach();
            }
        }
        store
    }

    /// Global settings. Creates a memory-only copy when not initialized (unit tests).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalSettings>() {
            Some(g) => g.0.clone(),
            None => Self::init(None, cx),
        }
    }

    /// Global settings if they were set up, for callers that can't create them.
    pub(crate) fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSettings>().map(|g| g.0.clone())
    }

    /// Applies the loaded file, keeping folders visited in the few milliseconds since startup.
    fn merge_loaded(&mut self, mut loaded: Settings, cx: &mut Context<Self>) {
        self.loaded = true;
        self.last_session_folder = loaded.recent.first().cloned();
        for path in self.settings.recent.iter().rev() {
            loaded.remember(path);
        }
        self.settings = loaded;
        cx.notify();
    }

    /// Pretends the settings file was read (tests use memory-only settings).
    #[cfg(test)]
    pub(crate) fn mark_loaded(&mut self, cx: &mut Context<Self>) {
        self.loaded = true;
        cx.notify();
    }

    /// Changes settings: applies now, writes the file in the background shortly after.
    pub(crate) fn update(&mut self, f: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        let before = self.settings.clone();
        f(&mut self.settings);
        if self.settings == before {
            return;
        }
        cx.notify();
        let Some(path) = self.path.clone() else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(snapshot) = this.read_with(cx, |store, _| store.settings.clone()) else {
                return;
            };
            let _ = std::thread::Builder::new()
                .name("settings-save".into())
                .spawn(move || {
                    if let Err(err) = save(&path, &snapshot) {
                        log::warn!("failed to write settings {}: {err}", path.display());
                    }
                });
        });
        // A newer change replaces the pending write.
        self.save_task = Some(task);
    }
}

/// `%APPDATA%\Arke\settings.json` (string handling only).
pub(crate) fn default_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Arke").join("settings.json"))
}

/// Turns "Open folders in Arke" off in the settings file (for `--restore-explorer`, which runs
/// without the UI). A missing file counts as off; an unreadable one is left untouched.
pub(crate) fn turn_off_takeover(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let Some(mut settings) = load(path) else {
        return false;
    };
    if !settings.open_folders_in_arke {
        return true;
    }
    settings.open_folders_in_arke = false;
    save(path, &settings).is_ok()
}

fn load(path: &Path) -> Option<Settings> {
    let bytes = std::fs::read(path).ok()?;
    parse_settings(&bytes, path)
}

/// Parses the settings file. Strips a leading BOM first: Notepad and PowerShell add one for UTF-8 and
/// serde_json rejects it, which would fall back to defaults and overwrite the user's settings on next save.
fn parse_settings(bytes: &[u8], path: &Path) -> Option<Settings> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match serde_json::from_slice::<Settings>(bytes) {
        Ok(mut settings) => {
            // Older versions also remembered WSL paths as network shares.
            settings
                .net_places
                .retain(|p| arke_core::net::server_of(p).is_some());
            Some(settings)
        }
        Err(err) => {
            // A broken file must not stop startup: keep it for the user and use defaults.
            log::warn!(
                "invalid settings file, using defaults {}: {err}",
                path.display()
            );
            None
        }
    }
}

fn save(path: &Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

impl Settings {
    /// Network locations global search actually indexes: user-added ones plus, when auto is on, network
    /// drives `drives` and recent shares, minus exclusions.
    pub(crate) fn searched_network(&self, drives: &[PathBuf]) -> Vec<PathBuf> {
        let mut out = self.search_network.clone();
        if self.search_network_auto {
            for p in drives.iter().chain(&self.net_places) {
                if !out.contains(p) {
                    out.push(p.clone());
                }
            }
        }
        out.retain(|p| !self.search_network_excluded.contains(p));
        out
    }

    pub(crate) fn update_channel(&self) -> crate::build_info::Channel {
        self.update_channel_choice
            .or_else(crate::build_info::built_channel)
            .unwrap_or_default()
    }
}

// --- Export and import ---

/// Why exporting or importing a settings backup failed. Display is the reason shown in the
/// Backup card.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BackupError {
    /// Settings could not be serialized.
    #[error("{0}")]
    Serialize(#[source] serde_json::Error),
    /// The file to import does not exist.
    #[error("{}", rust_i18n::t!("backup.missing"))]
    Missing {
        /// The file that was asked for.
        path: PathBuf,
    },
    /// The file to import could not be read.
    #[error("{source}")]
    Read {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The export could not be written.
    #[error("{source}")]
    Write {
        /// The file being written.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not JSON.
    #[error("{}", rust_i18n::t!("backup.not_json"))]
    NotJson(#[source] serde_json::Error),
    /// The file is JSON but not Arke settings.
    #[error("{}", rust_i18n::t!("backup.not_arke"))]
    NotArke,
    /// The file looks like Arke settings but a field does not fit.
    #[error("{}", rust_i18n::t!("backup.broken", error = .0))]
    Broken(#[source] serde_json::Error),
}

/// Marker in exported files so import only accepts Arke settings, not arbitrary JSON.
const EXPORT_MARK: &str = "arke_settings_export";

impl Settings {
    /// Strips per-machine, per-session state for export (tabs, window placement, recent folders).
    fn for_export(&self) -> Self {
        let mut out = self.clone();
        out.session = Session::default();
        out.window = None;
        out.recent.clear();
        out
    }

    /// Applies imported settings while keeping this machine's tabs, window placement and recent folders.
    pub(crate) fn imported(&self, mut from: Self) -> Self {
        from.session.clone_from(&self.session);
        from.window = self.window;
        from.recent.clone_from(&self.recent);
        from
    }
}

/// Serializes settings for export (pure).
pub(crate) fn export_json(settings: &Settings) -> Result<Vec<u8>, BackupError> {
    let mut value = serde_json::to_value(settings.for_export()).map_err(BackupError::Serialize)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(EXPORT_MARK.into(), serde_json::Value::from(1));
    }
    serde_json::to_vec_pretty(&value).map_err(BackupError::Serialize)
}

/// Parses an exported file (pure). Also accepts Arke's own `settings.json` (marker present, or settings-only fields).
pub(crate) fn parse_export(bytes: &[u8]) -> Result<Settings, BackupError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(BackupError::NotJson)?;
    let arke = value.get(EXPORT_MARK).is_some()
        || (value.get("tab_layout").is_some() && value.get("density").is_some());
    if !value.is_object() || !arke {
        return Err(BackupError::NotArke);
    }
    serde_json::from_value(value).map_err(BackupError::Broken)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Global search network locations: user-added plus, when auto is on, network drives and recent shares,
    /// minus exclusions, without duplicates.
    #[test]
    fn searched_network_merges_auto_and_exclusions() {
        let p = PathBuf::from;
        let mut s = Settings {
            search_network: vec![p(r"\\nas\manual")],
            net_places: vec![p(r"\\nas\recent"), p(r"\\nas\manual")],
            search_network_excluded: vec![p(r"\\nas\recent")],
            ..Settings::default()
        };
        assert_eq!(
            s.searched_network(&[p(r"X:\"), p(r"Y:\")]),
            [p(r"\\nas\manual"), p(r"X:\"), p(r"Y:\")]
        );
        s.search_network_auto = false;
        assert_eq!(s.searched_network(&[p(r"X:\")]), [p(r"\\nas\manual")]);
    }

    /// A UTF-8 BOM from Notepad or PowerShell still loads instead of falling back to defaults,
    /// which would overwrite the settings on the next save.
    #[test]
    fn settings_with_a_byte_order_mark_still_load() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"global_search": true, "show_hidden": true}"#);
        let s = parse_settings(&bytes, Path::new("settings.json")).unwrap();
        assert!(s.global_search && s.show_hidden);
    }

    /// Settings saved before the Recycle Bin moved into the pinned list keep it pinned.
    #[test]
    fn recycle_bin_is_pinned_unless_unpinned() {
        let old = parse_settings(br#"{"pinned": []}"#, Path::new("settings.json")).unwrap();
        assert!(old.recycle_pinned);
        let unpinned =
            parse_settings(br#"{"recycle_pinned": false}"#, Path::new("settings.json")).unwrap();
        assert!(!unpinned.recycle_pinned);
    }

    #[test]
    fn export_and_import_round_trip() {
        let mut mine = Settings {
            density: Density::Compact,
            show_hidden: true,
            recent: vec![PathBuf::from(r"C:\work")],
            ..Settings::default()
        };
        mine.session.tabs.push(SessionTab {
            panes: vec![PathBuf::from(r"C:\tab")],
            active_pane: 0,
            ratio: 0.5,
            pinned: false,
            synced: false,
        });
        let bytes = export_json(&mine).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains(EXPORT_MARK));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["recent"],
            serde_json::json!([]),
            "recent folders are not exported"
        );
        assert_eq!(
            value["session"]["tabs"],
            serde_json::json!([]),
            "tabs are not exported"
        );
        let back = parse_export(&bytes).unwrap();
        assert_eq!(back.density, Density::Compact);
        assert!(back.show_hidden);
        assert!(back.session.tabs.is_empty() && back.recent.is_empty());

        let other = Settings {
            recent: vec![PathBuf::from(r"D:\other")],
            ..Settings::default()
        };
        let applied = other.imported(back);
        assert_eq!(applied.density, Density::Compact);
        assert_eq!(applied.recent, vec![PathBuf::from(r"D:\other")]);

        assert!(matches!(
            parse_export(b"{\"name\": \"package\"}"),
            Err(BackupError::NotArke)
        ));
        assert!(matches!(
            parse_export(b"not json"),
            Err(BackupError::NotJson(_))
        ));
        assert!(matches!(parse_export(b"[1, 2]"), Err(BackupError::NotArke)));
    }

    /// Pinning fills the tiles first, then the list. Unpinning works from either. Entries can move
    /// between the two, but not onto full tiles.
    #[test]
    fn pin_fills_tiles_then_the_list() {
        use crate::shell::{KnownFolder, Place, PlaceKind};
        let places = vec![
            Place {
                label: "User".into(),
                path: PathBuf::from(r"C:\home"),
                kind: PlaceKind::Folder(KnownFolder::Profile),
            },
            Place {
                label: "Desktop".into(),
                path: PathBuf::from(r"C:\home\Desktop"),
                kind: PlaceKind::Folder(KnownFolder::Desktop),
            },
        ];
        let p = |n: usize| PathBuf::from(format!(r"D:\p{n}"));
        let mut s = Settings::default();
        // Default tiles (Desktop) count as pinned; the profile folder is not a tile.
        assert!(s.is_pinned(Path::new(r"C:\home\Desktop"), &places));
        assert!(!s.is_pinned(Path::new(r"C:\home"), &places));

        for n in 1..FAVORITES_MAX {
            s.pin(&p(n), &places);
        }
        assert_eq!(s.tiles(&places).len(), FAVORITES_MAX);
        assert!(s.pinned.is_empty());
        // Tiles full: further pins go to the list; pinning twice changes nothing.
        s.pin(&p(100), &places);
        s.pin(&p(100), &places);
        s.pin(&p(1), &places);
        assert_eq!(s.pinned, [p(100)]);
        assert_eq!(s.tiles(&places).len(), FAVORITES_MAX);
        assert!(s.is_pinned(&p(100), &places));

        // Tiles full, so a list entry cannot move up.
        assert!(!s.move_to_tiles(&p(100), &places));
        assert_eq!(s.pinned, [p(100)]);
        // Moving a tile down (to the front of the list) frees a slot, then the list entry can move up.
        s.move_to_list(&p(3), &places);
        assert_eq!(s.pinned, [p(3), p(100)]);
        assert!(s.move_to_tiles(&p(100), &places));
        assert_eq!(s.pinned, [p(3)]);
        assert_eq!(s.tiles(&places).last(), Some(&p(100)));

        // Unpinning works for tiles, list entries and defaults.
        s.toggle_pin(&p(3), &places);
        s.toggle_pin(&p(100), &places);
        s.unpin(Path::new(r"C:\home\Desktop"), &places);
        assert!(s.pinned.is_empty());
        assert!(!s.is_pinned(&p(100), &places));
        assert!(!s.is_pinned(Path::new(r"C:\home\Desktop"), &places));
        // Removed from the tiles: p100 and Desktop (p3 was in the list at that point).
        assert_eq!(s.tiles(&places).len(), FAVORITES_MAX - 2);
    }

    /// Dragging reorders within an area and moves across areas; onto full tiles a list entry swaps.
    #[test]
    fn dragging_pins_reorders_and_moves_between_areas() {
        let p = |n: usize| PathBuf::from(format!(r"D:\p{n}"));
        let mut s = Settings {
            favorites: Some(vec![p(0), p(1), p(2)]),
            pinned: vec![p(10), p(11)],
            ..Settings::default()
        };
        // Dragging forward within the tiles lands on that slot.
        s.place_pin(&p(0), PinSlot::Tile(2), &[]);
        assert_eq!(s.favorites, Some(vec![p(1), p(2), p(0)]));
        // List entry onto tiles with room: inserted before that slot.
        s.place_pin(&p(11), PinSlot::Tile(0), &[]);
        assert_eq!(s.favorites, Some(vec![p(11), p(1), p(2), p(0)]));
        assert_eq!(s.pinned, [p(10)]);
        // Tile to the end of the list.
        s.place_pin(&p(1), PinSlot::List(usize::MAX), &[]);
        assert_eq!(s.favorites, Some(vec![p(11), p(2), p(0)]));
        assert_eq!(s.pinned, [p(10), p(1)]);
        // Reorder within the list.
        s.place_pin(&p(1), PinSlot::List(0), &[]);
        assert_eq!(s.pinned, [p(1), p(10)]);
        // Dragging does not add unpinned folders.
        s.place_pin(&p(99), PinSlot::Tile(0), &[]);
        assert!(!s.is_pinned(&p(99), &[]));

        // Tiles full: a list entry dragged onto them swaps with that tile.
        let mut full = Settings {
            favorites: Some((0..FAVORITES_MAX).map(p).collect()),
            pinned: vec![p(50), p(51)],
            ..Settings::default()
        };
        full.place_pin(&p(51), PinSlot::Tile(3), &[]);
        let tiles = full.favorites.clone().unwrap();
        assert_eq!(tiles.len(), FAVORITES_MAX);
        assert_eq!(tiles[3], p(51));
        assert_eq!(full.pinned, [p(50), p(3)]);
    }

    #[test]
    fn round_trips_and_tolerates_missing_fields() {
        let dir = std::env::temp_dir().join(format!("arke-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let mut s = Settings::default();
        s.toggle_pin(Path::new(r"C:\a"), &[]);
        s.remember(Path::new(r"C:\b"));
        s.remember(Path::new(r"C:\c"));
        s.remember(Path::new(r"C:\b"));
        s.density = Density::Compact;
        save(&path, &s).unwrap();
        assert_eq!(load(&path), Some(s.clone()));
        assert_eq!(
            s.recent,
            [PathBuf::from(r"C:\b"), PathBuf::from(r"C:\c")],
            "most recent first, no duplicates"
        );
        // Old settings files with missing fields get defaults.
        std::fs::write(&path, br#"{"pinned":["D:\\x"]}"#).unwrap();
        let old = load(&path).unwrap();
        assert_eq!(old.pinned, [PathBuf::from(r"D:\x")]);
        assert_eq!(old.tab_layout, TabLayout::Vertical);
        assert!(
            old.show_extensions && old.show_thumbnails && old.keep_running,
            "new switches default to on"
        );
        assert!(
            !old.open_folders_in_arke,
            "taking over Explorer is opt-in, never on by default"
        );
        // `--restore-explorer` flips only that switch in the file.
        std::fs::write(
            &path,
            br#"{"pinned":["D:\\x"],"open_folders_in_arke":true}"#,
        )
        .unwrap();
        assert!(turn_off_takeover(&path));
        let off = load(&path).unwrap();
        assert!(!off.open_folders_in_arke);
        assert_eq!(off.pinned, [PathBuf::from(r"D:\x")], "other settings stay");
        std::fs::write(&path, b"{ broken").unwrap();
        assert!(
            !turn_off_takeover(&path),
            "an unreadable file is left alone"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{ broken");
        // Accent is a string; unknown values fall back to the default and other settings still load.
        std::fs::write(&path, br##"{"accent":"#0078D4","density":"compact"}"##).unwrap();
        let blue = load(&path).unwrap();
        assert_eq!(blue.accent, Accent::Color(0x0078d4));
        assert_eq!(blue.density, Density::Compact);
        std::fs::write(&path, br#"{"accent":"sparkly","density":"compact"}"#).unwrap();
        let odd = load(&path).unwrap();
        assert_eq!(odd.accent, Accent::Arke);
        assert_eq!(odd.density, Density::Compact);
        let windows = Settings {
            accent: Accent::Windows,
            ..Settings::default()
        };
        save(&path, &windows).unwrap();
        assert_eq!(load(&path).unwrap().accent, Accent::Windows);
        std::fs::write(&path, br#"{"tint":"accent","material":"mica_alt"}"#).unwrap();
        let look = load(&path).unwrap();
        assert_eq!(
            (look.tint, look.material),
            (Tint::Accent, Material::MicaAlt)
        );
        std::fs::write(&path, br##"{"tint":"#102030"}"##).unwrap();
        assert_eq!(load(&path).unwrap().tint, Tint::Color(0x10_2030));
        assert_eq!(Accent::parse_hex("#0af"), Some(0x00aaff));
        assert_eq!(Accent::parse_hex("0078d4"), Some(0x0078d4));
        assert_eq!(Accent::parse_hex("#0078d"), None);
        assert_eq!(Accent::parse_hex("zzzzzz"), None);
        // A corrupt file does not panic.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saved_remote_places_replace_by_url_and_hold_no_password() {
        let mut s = Settings::default();
        let place = |url: &str, key: Option<&str>| RemotePlace {
            url: PathBuf::from(url),
            key_file: key.map(PathBuf::from),
        };
        s.save_remote_place(place("sftp://a@nas", None));
        s.save_remote_place(place("https://dav.example.com/files", None));
        s.save_remote_place(place("sftp://a@nas", Some(r"C:\k\id_ed25519")));
        assert_eq!(s.remote_places.len(), 2, "same URL replaces");
        assert_eq!(
            s.remote_places[0],
            place("sftp://a@nas", Some(r"C:\k\id_ed25519"))
        );
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("remote_places") && !json.contains("password"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.remote_places, s.remote_places);
        s.forget_remote_place(Path::new("sftp://a@nas"));
        assert_eq!(
            s.remote_places,
            [place("https://dav.example.com/files", None)]
        );
    }

    #[test]
    fn remembers_the_shares_of_network_folders() {
        let mut s = Settings::default();
        s.remember_share(Path::new(r"C:\Users"));
        s.remember_share(Path::new(r"\\?\C:\x"));
        s.remember_share(Path::new(r"\\wsl.localhost\Ubuntu\home"));
        s.remember_share(Path::new(r"\\wsl$\Ubuntu"));
        assert!(
            s.net_places.is_empty(),
            "local and WSL paths are not remembered"
        );
        s.remember_share(Path::new(r"\\nas\docs\2026\report"));
        s.remember_share(Path::new(r"\\nas\media"));
        s.remember_share(Path::new(r"\\NAS\DOCS\other"));
        assert_eq!(
            s.net_places,
            [PathBuf::from(r"\\NAS\DOCS"), PathBuf::from(r"\\nas\media")],
            "the share itself, case-insensitive, newest first"
        );
        for i in 0..10 {
            s.remember_share(Path::new(&format!(r"\\server{i}\share")));
        }
        assert_eq!(s.net_places.len(), NET_PLACES_MAX);
        assert!(s.net_places.contains(&PathBuf::from(r"\\server9\share")));
        s.forget_share(Path::new(r"\\server9\share"));
        assert!(!s.net_places.contains(&PathBuf::from(r"\\server9\share")));
    }

    #[test]
    fn uninstall_keeps_settings_but_turns_takeover_off() {
        let dir = std::env::temp_dir().join(format!("arke-test-takeover-{}", std::process::id()));
        let path = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(turn_off_takeover(&path), "no file, nothing to do");
        let settings = Settings {
            open_folders_in_arke: true,
            show_hidden: true,
            ..Settings::default()
        };
        save(&path, &settings).unwrap();
        assert!(turn_off_takeover(&path));
        let kept = load(&path).unwrap();
        assert!(!kept.open_folders_in_arke);
        assert!(kept.show_hidden, "other settings stay");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
