//! User settings: pins, favorite tiles, recent folders, tab layout, density, and the Settings page toggles.
//!
//! Stored in `%APPDATA%\Arke\settings.json`, never the registry. Reads and writes happen on background
//! threads. Startup renders with defaults and updates once the file is read. Writes are debounced to
//! coalesce bursts, and go to a temp file that is then renamed so a crash cannot leave a broken file.
//!
//! One app-wide copy (`Global`) shared by all windows.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arke_core::sort::{SortColumn, SortSpec};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, Task};
use serde::{Deserialize, Serialize};

use crate::explorer::ViewMode;

/// Debounce for coalescing consecutive changes into one write.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// How long startup waits for the settings file (read in the background, usually 1-2 ms). If it arrives
/// in time the first window opens with the saved size, tabs and language; otherwise defaults are used
/// and the file is applied when it arrives (e.g. %APPDATA% on a slow network).
const STARTUP_WAIT: Duration = Duration::from_millis(60);
/// Most windows a restored session opens: each takes a moment to open, and many at once would
/// keep a second `arke.exe` waiting.
pub(crate) const RESTORED_WINDOWS_MAX: usize = 20;
/// Number of recent folders to remember.
pub(crate) const RECENT_MAX: usize = 50;
/// Max favorite tiles; further pins go to the list below.
pub(crate) const FAVORITES_MAX: usize = 12;

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

/// A sidebar section. Favorites, This PC and Network collapse to their heading; Home (its row and
/// tiles) never collapses, nor do the tabs. Home, Favorites and This PC can be hidden (#323).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SidebarSection {
    Home,
    Favorites,
    ThisPc,
    Network,
}

/// Reads the collapsed sections, skipping names this version doesn't know (a section added later):
/// one unknown name must not fail the whole file, which would fall back to the defaults.
fn known_sections<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SidebarSection>, D::Error> {
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    let mut out: Vec<SidebarSection> = Vec::new();
    for section in raw
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
    {
        if !out.contains(&section) {
            out.push(section);
        }
    }
    Ok(out)
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

/// What Ctrl+T and the New tab buttons do (#90).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NewTabAction {
    /// The "Go to folder" box.
    #[default]
    GoToFolder,
    /// A new tab with Home.
    Home,
    /// A new tab on the current folder, as in File Explorer.
    CurrentFolder,
}

/// What closing the last tab does (#253, #255).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LastTabAction {
    /// The window closes, as in File Explorer and browsers (#98).
    #[default]
    CloseWindow,
    /// The tab stays and shows Home.
    Home,
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

/// Background of the dark theme: near-black (since 0.5) or the gray of Arke 0.4 and earlier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DarkShade {
    #[default]
    Black,
    Gray,
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
    /// Every window open when Arke last quit, with all its tabs and splits, like a browser. If a
    /// window is already open, new windows open Home.
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
    /// Home was shown over this tab's folder, so it comes back on Home (#98).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) home: bool,
}

const fn half() -> f32 {
    0.5
}

/// The tabs of one window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Session {
    pub(crate) tabs: Vec<SessionTab>,
    pub(crate) active: usize,
    /// The saved workspace the window showed (#269), so its name comes back with the tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<SessionWorkspace>,
}

/// The saved workspace a window showed: its name and which of the session's tabs came from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionWorkspace {
    pub(crate) name: String,
    /// Indices into [`Session::tabs`].
    pub(crate) tabs: Vec<usize>,
}

/// One window of the last session: its tabs and where it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SessionWindow {
    #[serde(flatten)]
    pub(crate) session: Session,
    pub(crate) place: Option<WindowPlace>,
}

impl SessionWindow {
    /// The folder its first tab opens on; `None` for a window with nothing to restore.
    pub(crate) fn first_folder(&self) -> Option<&PathBuf> {
        self.session.tabs.first().and_then(|t| t.panes.first())
    }
}

/// Where the Properties window was last, in logical pixels. The height is only kept when the user
/// sized the window; otherwise it fits its content.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PropertiesPlace {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    #[serde(default)]
    pub(crate) height: Option<f32>,
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

/// What back and forward move through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NavScope {
    /// Every step in the order it happened, switching tabs when needed (`core::timeline`).
    #[default]
    AcrossTabs,
    /// Only the current tab's folders, as in File Explorer.
    WithinTab,
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

/// Column view widths the user dragged or fitted, in pixels (`None`: the default). One set for
/// every folder, so the columns don't jump while browsing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ColumnViewWidths {
    /// The parent folder's column.
    pub(crate) parent: Option<u16>,
    /// The current folder's column; the preview takes the rest. `None`: it fills what the
    /// preview leaves.
    pub(crate) current: Option<u16>,
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
    Ja,
    De,
    Es,
    PtBr,
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
            Self::Ja => "ja".into(),
            Self::De => "de".into(),
            Self::Es => "es".into(),
            Self::PtBr => "pt-BR".into(),
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

/// How long reading a drive's whole file table through the index service took (Properties).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TableTime {
    pub(crate) ms: u64,
    /// Read to the end. Otherwise it was stopped: a walk that had run `ms` (the read starting
    /// part way through) was done first, so the table isn't worth starting before `ms`. That is
    /// under twice the table's time: the read started no later than the table takes, and hadn't
    /// ended.
    pub(crate) done: bool,
}

impl TableTime {
    /// Keeps what is known best: a read to the end replaces anything, a stopped read only raises
    /// a lower bound.
    pub(crate) fn learn(
        times: &mut std::collections::BTreeMap<String, Self>,
        volume: String,
        new: Self,
    ) {
        let known = times.get(&volume);
        if new.done || known.is_none_or(|k| !k.done && new.ms > k.ms) {
            times.insert(volume, new);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one bool per on/off setting in settings.json"
)]
pub(crate) struct Settings {
    /// Pinned folders that do not fit on the tiles: the list under the tiles, in user order.
    pub(crate) pinned: Vec<PathBuf>,
    /// Pinned folders on the tiles (at most [`FAVORITES_MAX`]). `None` means the defaults
    /// (Desktop, Downloads, Documents, Pictures, Music, Videos).
    pub(crate) favorites: Option<Vec<PathBuf>>,
    /// Folders pinned to Quick Access in File Explorer as of the last sync into the favorites
    /// (#45). `None` until the first sync. Kept so a folder removed from the favorites here is not
    /// added back while it stays pinned in Windows.
    pub(crate) quick_access_seen: Option<Vec<PathBuf>>,
    /// Recycle Bin row at the end of the pinned list.
    pub(crate) recycle_pinned: bool,
    /// Recent folders, newest first (for Ctrl+T).
    pub(crate) recent: Vec<PathBuf>,
    pub(crate) tab_layout: TabLayout,
    /// Opening a folder in a new tab switches to a tab already showing it instead.
    pub(crate) one_tab_per_folder: bool,
    /// What Ctrl+T does (#90).
    pub(crate) new_tab: NewTabAction,
    /// What closing the last tab does (#253, #255).
    pub(crate) last_tab: LastTabAction,
    pub(crate) density: Density,
    /// Preview pane on the right.
    pub(crate) preview_pane: bool,
    /// Space hands the item to QuickLook (QL-Win) instead of Arke's Quick Look (offered only
    /// when it is installed).
    pub(crate) space_quicklook: bool,
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
    /// Origins (`http://user@host:port`) the user allowed to sign in over plain http, where the
    /// password crosses the network unencrypted. Per location and per user, never per network.
    pub(crate) insecure_http: Vec<String>,
    /// Names the user gave mapped drives, network places and saved remote locations, shown
    /// instead of the default ones (`place_names.rs`). Windows' own labels are never changed.
    pub(crate) place_names: crate::place_names::Names,
    /// Names the user gave favorites (#259), shown on their tile or row in the sidebar only.
    pub(crate) favorite_names: crate::place_names::Names,
    pub(crate) theme: ThemeMode,
    /// Background of the dark theme.
    pub(crate) dark_shade: DarkShade,
    /// The sidebar is collapsed to a column of icons (Ctrl+B).
    pub(crate) sidebar_collapsed: bool,
    /// The collapsed sidebar opens over the content while the pointer rests on it (#122).
    pub(crate) sidebar_peek: bool,
    /// This PC's tree opens down to the current folder and scrolls to it (#241); off, the tree
    /// only opens where the user opens it (#299).
    pub(crate) sidebar_follow: bool,
    /// Sidebar sections collapsed to their heading. Before 0.8 only This PC could collapse
    /// (`this_pc_collapsed`, see [`Settings::migrate_this_pc_collapsed`]).
    #[serde(deserialize_with = "known_sections")]
    pub(crate) collapsed_sections: Vec<SidebarSection>,
    /// Sidebar sections hidden from their heading's menu, shown again from the menu of the
    /// sidebar's empty space (#323).
    #[serde(deserialize_with = "known_sections")]
    pub(crate) hidden_sections: Vec<SidebarSection>,
    pub(crate) accent: Accent,
    pub(crate) tint: Tint,
    pub(crate) material: Material,
    /// UI font name; empty means default.
    pub(crate) font: String,
    /// Image drawn under the file list.
    pub(crate) background_image: Option<PathBuf>,
    /// Background image opacity in percent.
    pub(crate) background_opacity: u8,
    /// Interface scale in percent (80..=150), on top of the Windows DPI scale.
    pub(crate) ui_scale: u32,
    /// Dragged sidebar width at 100% scale; 0 is the default width.
    pub(crate) sidebar_width: f32,
    /// Height of the sidebar's places (favorites, This PC, Network) above the tabs, dragged on the
    /// line between them, at 100% scale; 0 shares the room by itself (the tabs give way first).
    pub(crate) sidebar_split: f32,
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
    /// With `open_folders_in_arke`, Win+E and File Explorer started from the taskbar or Start
    /// (the same verb) open Arke too. Off: File Explorer started on purpose stays File Explorer.
    pub(crate) win_e_opens_arke: bool,
    /// One window: folders from other programs (#11), Win+E and Arke started again open as a tab
    /// in the window used last instead of a window of their own. "Open in new window" still opens
    /// one. Before 0.7 `open_external_in_tab` (see [`Settings::migrate_single_window`]).
    pub(crate) single_window: bool,
    /// What back and forward move through.
    pub(crate) nav_scope: NavScope,
    /// A two-finger swipe left or right on a precision touchpad goes back or forward (#70).
    pub(crate) swipe_nav: bool,
    /// Same-drive Recycle Bin moves done directly instead of through the Windows
    /// engine (`win_fast_recycle.rs`).
    pub(crate) fast_recycle: bool,
    /// Copies of new items on local SSDs done by Arke, several files at once, instead of the
    /// Windows engine (`win_fast_copy.rs`).
    pub(crate) fast_copy: bool,
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
    /// With [`single_click`](Self::single_click): resting the pointer on an item selects it, as
    /// Windows' "Single-click to open an item (point to select)" (#46). Off: it only highlights,
    /// as File Explorer on Windows 11 does.
    pub(crate) point_to_select: bool,
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
    /// Share of the preview pane's height the dragged preview content takes above its details, in
    /// thousandths (`None`: automatic, from the content's shape).
    pub(crate) preview_split: Option<u16>,
    /// Column view: the parent and current folder columns' widths.
    pub(crate) column_view_widths: ColumnViewWidths,
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
    /// Arke context menu entries in the order dragged to in Settings › Context menu (keys from
    /// `menu_order::DEFAULT`), empty for the built-in order. Unknown keys are ignored and missing
    /// entries keep their built-in place, so files from other versions still load (#263).
    pub(crate) menu_order: Vec<String>,
    /// Windows commands pinned to the main menu from "More options" (`explorer_menu::pin_key` keys, in pin order).
    pub(crate) menu_pinned: Vec<String>,
    /// Where each pinned command comes from, learned from the menus seen
    /// (`win_menu_parts::MenuHandler::owner`): an extension's class id, `verb:<verb>` for a
    /// registry command, `command:{CLSID}` for a packaged app's, `-` for none of those (left to
    /// the whole Windows menu). Empty: learned by an older version, learned again. Lets a pinned
    /// command show before the whole Windows menu is read (#129, #278).
    pub(crate) menu_pin_owners: std::collections::BTreeMap<String, String>,
    /// Packaged apps' pinned commands (`command:{CLSID}` owners): the item kinds they are for,
    /// from the app's manifest and the menus they were seen in (`win_menu_parts::item_kind`).
    pub(crate) menu_pin_types: std::collections::BTreeMap<String, Vec<String>>,
    /// Extensions' pinned commands, by pin key: the item kinds (`win_menu_parts::item_kind`) the
    /// extension showed them for when last read. An extension decides for itself whether it adds
    /// an item, so its row is drawn ahead only where it showed last time (#278).
    pub(crate) menu_pin_seen: std::collections::BTreeMap<String, Vec<String>>,
    /// "More options" has shown, once, the line saying its pins keep a command in the main menu.
    pub(crate) pin_hint_seen: bool,
    /// How long reading each drive's whole file table through the index service took (`C:\`
    /// keys), as last measured by Properties: a folder walk that has taken longer than that is
    /// overtaken by a read of the table (`properties.rs`).
    pub(crate) mft_list: std::collections::BTreeMap<String, TableTime>,
    /// Shortcuts changed in Settings: command id (`commands.rs`) to its keys, empty when the
    /// command has none. Only changes are stored; a command that is absent keeps its defaults.
    pub(crate) shortcuts: std::collections::BTreeMap<String, Vec<String>>,
    /// Commands run from the command palette, newest first.
    pub(crate) recent_commands: Vec<String>,
    /// Auto update: channel (`None` follows this build's channel), whether to check automatically,
    /// and the last check time (Unix seconds).
    pub(crate) update_channel_choice: Option<crate::build_info::Channel>,
    pub(crate) update_auto: bool,
    pub(crate) update_checked: Option<u64>,
    /// The update whose notice bar was closed (`Some("0.4.0")`): it doesn't come back for that
    /// version, only for a newer one.
    pub(crate) update_dismissed: Option<String>,
    /// Download route for updates (`update_route.rs`): empty for GitHub directly, an https prefix
    /// or an HTTP proxy `host:port`. Only update checks and downloads use it.
    pub(crate) update_route: String,
    /// Windows' RedirectionGuard is on for every program on this PC (a policy): the copy Arke
    /// started again through the desktop shell still had it, so later starts don't try that
    /// (`redirection_guard.rs`). Cleared by a start without it. Per machine: not exported.
    pub(crate) redirection_guard_by_policy: bool,
    /// What's new (`whats_new.rs`): open it after an update, and the version it last saw (`None`
    /// before Arke 0.4 recorded it).
    pub(crate) whats_new_auto: bool,
    pub(crate) whats_new_seen: Option<String>,
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
    /// The tabs of the window used last (the first of `session_windows`), which versions before
    /// 0.7 read as the whole last session.
    pub(crate) session: Session,
    /// Every window open when Arke last quit, the one used last first, for "New windows open to:
    /// Last tabs" (`session.rs`).
    pub(crate) session_windows: Vec<SessionWindow>,
    /// Last window position and size.
    pub(crate) window: Option<WindowPlace>,
    /// Last position and size of the Properties window.
    pub(crate) properties_window: Option<PropertiesPlace>,
}

impl Default for Settings {
    #[allow(clippy::too_many_lines, reason = "one line per setting")]
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            favorites: None,
            quick_access_seen: None,
            recycle_pinned: true,
            recent: Vec::new(),
            tab_layout: TabLayout::default(),
            one_tab_per_folder: true,
            new_tab: NewTabAction::default(),
            last_tab: LastTabAction::default(),
            density: Density::default(),
            preview_pane: false,
            space_quicklook: false,
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
            insecure_http: Vec::new(),
            place_names: crate::place_names::Names::new(),
            favorite_names: crate::place_names::Names::new(),
            theme: ThemeMode::default(),
            accent: Accent::default(),
            tint: Tint::default(),
            material: Material::default(),
            dark_shade: DarkShade::default(),
            sidebar_collapsed: false,
            sidebar_peek: false,
            sidebar_follow: true,
            collapsed_sections: Vec::new(),
            hidden_sections: Vec::new(),
            font: String::new(),
            background_image: None,
            background_opacity: 20,
            ui_scale: 100,
            sidebar_width: 0.,
            sidebar_split: 0.,
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
            win_e_opens_arke: false,
            single_window: true,
            nav_scope: NavScope::AcrossTabs,
            swipe_nav: true,
            fast_recycle: true,
            fast_copy: true,
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
            point_to_select: true,
            full_dates: false,
            show_modified: true,
            show_created: false,
            column_widths: ColumnWidths::default(),
            preview_width: None,
            preview_split: None,
            column_view_widths: ColumnViewWidths::default(),
            show_type: true,
            show_size: true,
            show_extension: false,
            show_attributes: false,
            show_link_target: false,
            column_order: Vec::new(),
            decimal_sizes: false,
            tray_icon: true,
            menu_hidden: Vec::new(),
            menu_order: Vec::new(),
            menu_pinned: Vec::new(),
            menu_pin_owners: std::collections::BTreeMap::new(),
            menu_pin_types: std::collections::BTreeMap::new(),
            menu_pin_seen: std::collections::BTreeMap::new(),
            pin_hint_seen: false,
            mft_list: std::collections::BTreeMap::new(),
            shortcuts: std::collections::BTreeMap::new(),
            recent_commands: Vec::new(),
            update_channel_choice: None,
            update_auto: true,
            update_checked: None,
            update_dismissed: None,
            update_route: String::new(),
            redirection_guard_by_policy: false,
            whats_new_auto: true,
            whats_new_seen: None,
            open_with_extra: Vec::new(),
            open_with_hidden: Vec::new(),
            show_checkboxes: false,
            group_rows: false,
            details_tree: false,
            autostart: false,
            start_menu: false,
            start_menu_offered: false,
            session: Session::default(),
            session_windows: Vec::new(),
            window: None,
            properties_window: None,
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
        self.place_names.remove(&crate::place_names::key(share));
    }

    /// Adds or updates a saved remote location (same URL replaces), newest first.
    pub(crate) fn save_remote_place(&mut self, place: RemotePlace) {
        self.remote_places.retain(|p| p.url != place.url);
        self.remote_places.insert(0, place);
    }

    pub(crate) fn forget_remote_place(&mut self, url: &Path) {
        self.remote_places.retain(|p| p.url != url);
        self.place_names.remove(&crate::place_names::key(url));
    }

    /// Allows or takes back plain-http sign-in for a server (`origin`, see `RemoteUrl::origin`).
    pub(crate) fn set_insecure_http(&mut self, origin: &str, allowed: bool) {
        self.insecure_http.retain(|o| o != origin);
        if allowed {
            self.insecure_http.push(origin.to_owned());
        }
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

    /// Puts a folder in at `slot`, an insertion point (a tile or list index, past the end means
    /// last), whether or not it was pinned: one already on the tiles or in the list moves there.
    /// On full tiles the last other tile makes room, going to the top of the list. Returns where
    /// the folder ended up.
    pub(crate) fn insert_pin(
        &mut self,
        path: &Path,
        slot: PinSlot,
        places: &[crate::shell::Place],
    ) -> PinSlot {
        let mut tiles = self.tiles(places);
        let mut list = self.pinned.clone();
        // Takes the folder out of `items`; `at` counted with it still there.
        let take = |items: &mut Vec<PathBuf>, at: usize| -> usize {
            match items.iter().position(|p| p == path) {
                Some(from) => {
                    items.remove(from);
                    if from < at { at - 1 } else { at }
                }
                None => at,
            }
        };
        let landed = match slot {
            PinSlot::Tile(at) => {
                let at = take(&mut tiles, at).min(tiles.len());
                list.retain(|p| p != path);
                tiles.insert(at, path.to_path_buf());
                if tiles.len() > FAVORITES_MAX
                    && let Some(last) = tiles.iter().rposition(|p| p != path)
                {
                    list.insert(0, tiles.remove(last));
                }
                PinSlot::Tile(tiles.iter().position(|p| p == path).unwrap_or(at))
            }
            PinSlot::List(at) => {
                let at = take(&mut list, at).min(list.len());
                tiles.retain(|p| p != path);
                list.insert(at, path.to_path_buf());
                PinSlot::List(at)
            }
        };
        self.favorites = Some(tiles);
        self.pinned = list;
        landed
    }

    /// Unpins, from the tiles or the list. The name the user gave it goes with it.
    pub(crate) fn unpin(&mut self, path: &Path, places: &[crate::shell::Place]) {
        let tiles = self.tiles(places);
        if tiles.iter().any(|p| p == path) {
            self.favorites = Some(tiles.into_iter().filter(|p| p != path).collect());
        }
        self.pinned.retain(|p| p != path);
        self.favorite_names.remove(&crate::place_names::key(path));
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
    /// Changes made so far; each write says which change it holds (see [`save_change`]).
    changes: u64,
    /// The newest change written to the file so far (see [`save_change`]).
    written: Arc<parking_lot::Mutex<u64>>,
    _quit: Option<Subscription>,
}

pub(crate) struct GlobalSettings(pub(crate) Entity<SettingsStore>);

impl Global for GlobalSettings {}

impl SettingsStore {
    /// Creates the global settings and reads the file in the background. With `path` `None` it is memory only (tests).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = Self::unread(path.clone(), cx);
        // Tests compare whole menus: the one-time line in More options is left to its own test.
        #[cfg(test)]
        store.update(cx, |s, _| s.settings.pin_hint_seen = true);
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

    /// The global settings on `path`, at the defaults until the file is read.
    fn unread(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|cx| Self {
            settings: Settings::default(),
            loaded: false,
            last_session_folder: None,
            first_run: false,
            _quit: path
                .is_some()
                .then(|| cx.on_app_quit(Self::save_before_quit)),
            path,
            save_task: None,
            changes: 0,
            written: Arc::default(),
        });
        cx.set_global(GlobalSettings(store.clone()));
        store
    }

    /// Settings on `path` whose file is read only when the test calls [`Self::finish_reading`]
    /// (a slow disk).
    #[cfg(test)]
    pub(crate) fn reading_slowly(path: PathBuf, cx: &mut App) -> Entity<Self> {
        Self::unread(Some(path), cx)
    }

    /// The file read at last for [`Self::reading_slowly`].
    #[cfg(test)]
    pub(crate) fn finish_reading(&mut self, path: &Path, cx: &mut Context<Self>) {
        let loaded = load(path).unwrap_or_default();
        self.merge_loaded(loaded, cx);
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

    /// Applies the loaded file, keeping folders visited in the few milliseconds since startup, and
    /// writes it if that added any.
    fn merge_loaded(&mut self, mut loaded: Settings, cx: &mut Context<Self>) {
        self.loaded = true;
        self.last_session_folder = loaded.recent.first().cloned();
        let file = loaded.clone();
        // What this start found out about RedirectionGuard, written like any change.
        loaded.redirection_guard_by_policy =
            crate::redirection_guard::remembered_policy(loaded.redirection_guard_by_policy);
        for path in self.settings.recent.iter().rev() {
            loaded.remember(path);
        }
        let merged = loaded != file;
        self.settings = loaded;
        cx.notify();
        if merged {
            self.schedule_save(cx);
        }
    }

    /// Pretends the settings file was read (tests use memory-only settings).
    #[cfg(test)]
    pub(crate) fn mark_loaded(&mut self, cx: &mut Context<Self>) {
        self.loaded = true;
        cx.notify();
    }

    /// Changes settings: applies now, writes the file in the background shortly after. Until the
    /// file is read only memory changes: a write then would replace the user's settings with the
    /// defaults (the folders visited meanwhile are kept by [`Self::merge_loaded`]).
    pub(crate) fn update(&mut self, f: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        let before = self.settings.clone();
        f(&mut self.settings);
        if self.settings == before {
            return;
        }
        cx.notify();
        if self.loaded {
            self.schedule_save(cx);
        }
    }

    /// Writes the settings shortly, as one write with any change that follows.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.changes += 1;
        let written = Arc::clone(&self.written);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok((snapshot, change)) =
                this.read_with(cx, |store, _| (store.settings.clone(), store.changes))
            else {
                return;
            };
            let _ = std::thread::Builder::new()
                .name("settings-save".into())
                .spawn(move || save_change(&path, &snapshot, change, &written));
        });
        // A newer change replaces the pending write.
        self.save_task = Some(task);
    }

    /// Quitting (the notification area's Quit, the last window closing) within the write delay of
    /// a change, such as a window closed just before: write it now instead of losing it. Nothing
    /// before the file is read, which would write the defaults.
    fn save_before_quit(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        let pending = self
            .path
            .clone()
            .filter(|_| self.loaded && self.save_task.take().is_some())
            .map(|path| (path, self.settings.clone(), self.changes));
        let written = Arc::clone(&self.written);
        let background = cx.background_executor().clone();
        async move {
            if let Some((path, settings, change)) = pending {
                background
                    .spawn(async move { save_change(&path, &settings, change, &written) })
                    .await;
            }
        }
    }
}

/// Writes the settings as of change number `change`, unless a newer change is already written
/// (`written`): a write of an older one that finishes later is skipped, so the write at quit is
/// never overwritten by one that started before it.
fn save_change(path: &Path, settings: &Settings, change: u64, written: &parking_lot::Mutex<u64>) {
    let mut written = written.lock();
    if change <= *written {
        return;
    }
    match save(path, settings) {
        Ok(()) => *written = change,
        Err(err) => log::warn!("failed to write settings {}: {err}", path.display()),
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

/// The "Win+E and the File Explorer icon open Arke too" switch, read straight from the settings
/// file before anything else starts (the folder handler deciding where Home goes). Off when the
/// file is missing or unreadable; nothing is written.
pub(crate) fn home_opens_arke() -> bool {
    file_switch("win_e_opens_arke")
}

/// `redirection_guard_by_policy`, read straight from the settings file like [`home_opens_arke`]:
/// startup decides with it before the settings are loaded, and only when RedirectionGuard is on.
pub(crate) fn redirection_guard_by_policy() -> bool {
    file_switch("redirection_guard_by_policy")
}

/// One on/off setting from the settings file, without loading the rest. Off when the file is
/// missing or unreadable.
fn file_switch(key: &str) -> bool {
    default_path()
        .and_then(|path| std::fs::read(path).ok())
        .is_some_and(|bytes| switch(&bytes, key))
}

fn switch(bytes: &[u8], key: &str) -> bool {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get(key).and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
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
            settings.migrate_session();
            settings.migrate_single_window(bytes);
            settings.migrate_this_pc_collapsed(bytes);
            settings.migrate_pin_owners(bytes);
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
    arke_core::fs::replace_file(path, &tmp, json)
}

impl Settings {
    /// Settings from before 0.7 kept one window's tabs (`session`) and the last placement: they
    /// become the one window of the last session.
    fn migrate_session(&mut self) {
        if self.session_windows.is_empty() && !self.session.tabs.is_empty() {
            self.session_windows = vec![SessionWindow {
                session: self.session.clone(),
                place: self.window,
            }];
        }
    }

    /// "Open folders from other apps in a tab" (`open_external_in_tab`, before 0.7) became single
    /// window mode: a file that has only the old switch keeps its choice. The old key is read here
    /// rather than as an alias, which would fail the whole file (and so fall back to the defaults)
    /// when both keys are in it.
    fn migrate_single_window(&mut self, bytes: &[u8]) {
        const OLD: &str = "open_external_in_tab";
        if !bytes.windows(OLD.len()).any(|w| w == OLD.as_bytes()) {
            return;
        }
        let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(bytes) else {
            return;
        };
        if !map.contains_key("single_window")
            && let Some(old) = map.get(OLD).and_then(serde_json::Value::as_bool)
        {
            self.single_window = old;
        }
    }

    /// Before 0.8 only This PC could collapse (`this_pc_collapsed`): a file that has only the old
    /// switch keeps This PC collapsed. Read here like [`Self::migrate_single_window`]; the key is
    /// dropped on the next save.
    fn migrate_this_pc_collapsed(&mut self, bytes: &[u8]) {
        const OLD: &str = "this_pc_collapsed";
        if !bytes.windows(OLD.len()).any(|w| w == OLD.as_bytes()) {
            return;
        }
        let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(bytes) else {
            return;
        };
        if !map.contains_key("collapsed_sections")
            && map.get(OLD).and_then(serde_json::Value::as_bool) == Some(true)
            && !self.is_collapsed(SidebarSection::ThisPc)
        {
            self.collapsed_sections.push(SidebarSection::ThisPc);
        }
    }

    /// Before 0.8.0-rc.2 a pinned command in a cascading menu made in the registry was learned to
    /// have no source of its own (`-`, left to the whole Windows menu) and waited for it on every
    /// menu (#278). A file written before then (no `menu_pin_seen` yet) learns those again: an
    /// empty owner is learned anew at the next menu.
    fn migrate_pin_owners(&mut self, bytes: &[u8]) {
        const SINCE: &str = "menu_pin_seen";
        if bytes.windows(SINCE.len()).any(|w| w == SINCE.as_bytes()) {
            return;
        }
        for owner in self.menu_pin_owners.values_mut() {
            if owner == "-" {
                owner.clear();
            }
        }
    }

    /// Whether a sidebar section is collapsed to its heading.
    pub(crate) fn is_collapsed(&self, section: SidebarSection) -> bool {
        self.collapsed_sections.contains(&section)
    }

    /// Collapses a sidebar section, or expands it again.
    pub(crate) fn toggle_collapsed(&mut self, section: SidebarSection) {
        if let Some(ix) = self.collapsed_sections.iter().position(|s| *s == section) {
            self.collapsed_sections.remove(ix);
        } else {
            self.collapsed_sections.push(section);
        }
    }

    /// Whether a sidebar section is hidden (#323).
    pub(crate) fn is_hidden(&self, section: SidebarSection) -> bool {
        self.hidden_sections.contains(&section)
    }

    /// Hides a sidebar section, or shows it again.
    pub(crate) fn set_hidden(&mut self, section: SidebarSection, hidden: bool) {
        let at = self.hidden_sections.iter().position(|s| *s == section);
        match (hidden, at) {
            (true, None) => self.hidden_sections.push(section),
            (false, Some(ix)) => {
                self.hidden_sections.remove(ix);
            }
            _ => {}
        }
    }

    /// The windows of the last session that have something to restore, the one used last first;
    /// at most [`RESTORED_WINDOWS_MAX`].
    pub(crate) fn last_windows(&self) -> Vec<SessionWindow> {
        self.session_windows
            .iter()
            .filter(|w| w.first_folder().is_some())
            .take(RESTORED_WINDOWS_MAX)
            .cloned()
            .collect()
    }

    /// The pinned tabs of the last session's windows, in order: they come back at every start,
    /// whatever the start location is, as in a browser (#298). A tab pinned in two windows on the
    /// same folders comes back once, and none of them reopens Home over its folder.
    pub(crate) fn last_pinned_tabs(&self) -> Session {
        let mut tabs: Vec<SessionTab> = Vec::new();
        for tab in self
            .last_windows()
            .iter()
            .flat_map(|w| w.session.tabs.iter())
            .filter(|t| t.pinned)
        {
            if !tabs.iter().any(|t| t.panes == tab.panes) {
                tabs.push(SessionTab {
                    home: false,
                    ..tab.clone()
                });
            }
        }
        Session {
            tabs,
            active: 0,
            workspace: None,
        }
    }

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
        out.session_windows.clear();
        out.window = None;
        out.recent.clear();
        out.redirection_guard_by_policy = false;
        out
    }

    /// Applies imported settings while keeping this machine's tabs, window placement and recent folders.
    pub(crate) fn imported(&self, mut from: Self) -> Self {
        from.session.clone_from(&self.session);
        from.session_windows.clone_from(&self.session_windows);
        from.window = self.window;
        from.recent.clone_from(&self.recent);
        from.redirection_guard_by_policy = self.redirection_guard_by_policy;
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

    /// Changed shortcuts are written to the file and read back; files from before have none.
    #[test]
    fn changed_shortcuts_survive_a_save() {
        let mut s = Settings::default();
        s.shortcuts
            .insert("go_up".to_owned(), vec!["ctrl-alt-u".to_owned()]);
        s.shortcuts.insert("refresh".to_owned(), Vec::new());
        s.recent_commands = vec!["refresh".to_owned()];
        let bytes = serde_json::to_vec(&s).unwrap();
        let back = parse_settings(&bytes, Path::new("settings.json")).unwrap();
        assert_eq!(back.shortcuts, s.shortcuts);
        assert_eq!(back.recent_commands, s.recent_commands);
        let old = parse_settings(br#"{"show_hidden": true}"#, Path::new("settings.json")).unwrap();
        assert!(old.shortcuts.is_empty() && old.recent_commands.is_empty());
    }

    /// #278: pinned commands learned to have no source of their own before 0.8.0-rc.2 (a file
    /// without `menu_pin_seen`) are learned again; once it is written by this version, they keep it.
    #[test]
    fn ownerless_pins_are_learned_again_once() {
        let path = Path::new("settings.json");
        let old = parse_settings(
            br#"{"menu_pin_owners": {"Tools / 01_one": "-", "Edit": "verb:Edit"}}"#,
            path,
        )
        .unwrap();
        assert_eq!(old.menu_pin_owners["Tools / 01_one"], "", "learned again");
        assert_eq!(old.menu_pin_owners["Edit"], "verb:Edit", "kept");
        let saved = serde_json::to_vec(&old).unwrap();
        let mut back = parse_settings(&saved, path).unwrap();
        back.menu_pin_owners
            .insert("Tools / 01_one".into(), "-".into());
        let again = parse_settings(&serde_json::to_vec(&back).unwrap(), path).unwrap();
        assert_eq!(again.menu_pin_owners["Tools / 01_one"], "-", "only once");
    }

    /// Settings from before 0.5 get the new near-black dark theme; the gray one is a choice.
    #[test]
    fn dark_shade_defaults_to_black() {
        let old = parse_settings(br#"{"theme": "dark"}"#, Path::new("settings.json")).unwrap();
        assert_eq!(old.dark_shade, DarkShade::Black);
        let gray =
            parse_settings(br#"{"dark_shade": "gray"}"#, Path::new("settings.json")).unwrap();
        assert_eq!(gray.dark_shade, DarkShade::Gray);
    }

    /// Settings from before 0.7 kept one window's tabs and the last placement: they become the one
    /// window of the last session. A file with every window keeps them as they are.
    #[test]
    fn an_old_single_window_session_becomes_the_first_window() {
        let old = br#"{
            "session": {"tabs": [{"panes": ["C:\\a", "C:\\b"]}, {"panes": ["C:\\c"]}], "active": 1},
            "window": {"x": 10, "y": 20, "width": 900, "height": 700}
        }"#;
        let s = parse_settings(old, Path::new("settings.json")).unwrap();
        let windows = s.last_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].session, s.session);
        assert_eq!(windows[0].session.active, 1);
        assert_eq!(windows[0].place, s.window);
        assert_eq!(windows[0].first_folder(), Some(&PathBuf::from(r"C:\a")));

        let tab = |path: &str| SessionTab {
            panes: vec![PathBuf::from(path)],
            active_pane: 0,
            ratio: 0.5,
            pinned: false,
            synced: false,
            home: false,
        };
        let window = |path: &str| SessionWindow {
            session: Session {
                tabs: vec![tab(path)],
                active: 0,
                workspace: None,
            },
            place: None,
        };
        let new = Settings {
            session: window(r"C:\front").session,
            session_windows: vec![window(r"C:\front"), window(r"C:\behind")],
            ..Settings::default()
        };
        let bytes = serde_json::to_vec(&new).unwrap();
        let back = parse_settings(&bytes, Path::new("settings.json")).unwrap();
        assert_eq!(back.session_windows, new.session_windows);
        // Nothing to restore in an empty session.
        assert!(Settings::default().last_windows().is_empty());
    }

    /// "Open folders from other apps in a tab" turned off before 0.7 keeps single window mode off.
    #[test]
    fn the_old_tab_switch_carries_over_to_single_window() {
        let off = parse_settings(
            br#"{"open_external_in_tab": false}"#,
            Path::new("settings.json"),
        )
        .unwrap();
        assert!(!off.single_window);
        let default = parse_settings(b"{}", Path::new("settings.json")).unwrap();
        assert!(default.single_window);
        // The old key isn't written back.
        let json = serde_json::to_string(&off).unwrap();
        assert!(!json.contains("open_external_in_tab"), "{json}");
    }

    /// Before 0.8 only This PC collapsed: a file with `this_pc_collapsed: true` keeps it collapsed,
    /// the old key isn't written back, and a section name from a later version is skipped rather
    /// than failing the file.
    #[test]
    fn collapsed_this_pc_carries_over_to_the_sections() {
        let read = |json: &[u8]| parse_settings(json, Path::new("settings.json")).unwrap();
        let old = read(br#"{"this_pc_collapsed": true, "show_hidden": true}"#);
        assert_eq!(old.collapsed_sections, vec![SidebarSection::ThisPc]);
        assert!(old.show_hidden, "the rest of the file too");
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("this_pc_collapsed"), "{json}");
        assert!(
            json.contains(r#""collapsed_sections":["this_pc"]"#),
            "{json}"
        );

        assert!(
            read(br#"{"this_pc_collapsed": false}"#)
                .collapsed_sections
                .is_empty()
        );
        // The new key wins once it is there.
        let both = read(br#"{"this_pc_collapsed": true, "collapsed_sections": ["network"]}"#);
        assert_eq!(both.collapsed_sections, vec![SidebarSection::Network]);
        // The tabs collapsed for a while during 0.8's development; that's dropped.
        assert!(
            read(br#"{"collapsed_sections": ["tabs"]}"#)
                .collapsed_sections
                .is_empty()
        );
        let later = read(br#"{"collapsed_sections": ["network", "someday", "network"]}"#);
        assert_eq!(later.collapsed_sections, vec![SidebarSection::Network]);

        let mut s = Settings::default();
        s.toggle_collapsed(SidebarSection::Favorites);
        assert!(s.is_collapsed(SidebarSection::Favorites));
        assert!(!s.is_collapsed(SidebarSection::ThisPc));
        s.toggle_collapsed(SidebarSection::Favorites);
        assert!(s.collapsed_sections.is_empty());
    }

    /// Hidden sidebar sections are kept, once each; a name from a later version is skipped.
    #[test]
    fn hidden_sections_are_kept() {
        let read = |json: &[u8]| parse_settings(json, Path::new("settings.json")).unwrap();
        assert!(Settings::default().hidden_sections.is_empty());
        let later =
            read(br#"{"hidden_sections": ["home", "someday", "this_pc", "home", "network"]}"#);
        assert_eq!(
            later.hidden_sections,
            vec![
                SidebarSection::Home,
                SidebarSection::ThisPc,
                SidebarSection::Network
            ]
        );
        let json = serde_json::to_string(&later).unwrap();
        assert!(
            json.contains(r#""hidden_sections":["home","this_pc","network"]"#),
            "{json}"
        );

        let mut s = Settings::default();
        s.set_hidden(SidebarSection::Favorites, true);
        s.set_hidden(SidebarSection::Favorites, true);
        assert_eq!(s.hidden_sections, vec![SidebarSection::Favorites]);
        assert!(s.is_hidden(SidebarSection::Favorites));
        assert!(
            !s.is_collapsed(SidebarSection::Favorites),
            "hidden isn't collapsed"
        );
        s.set_hidden(SidebarSection::Favorites, false);
        assert!(s.hidden_sections.is_empty());
    }

    /// A file with both the old switch and single window mode (an older Arke ran after 0.7 and
    /// kept the key it knew) is read, not replaced by the defaults; the new key wins.
    #[test]
    fn both_tab_switches_in_one_file_still_read() {
        let both =
            br#"{"single_window": false, "open_external_in_tab": true, "show_hidden": true}"#;
        let s = parse_settings(both, Path::new("settings.json")).expect("read");
        assert!(!s.single_window);
        assert!(s.show_hidden, "the rest of the file too");
        let both = br#"{"open_external_in_tab": false, "single_window": true}"#;
        assert!(
            parse_settings(both, Path::new("settings.json"))
                .unwrap()
                .single_window
        );
    }

    /// Writes finishing out of order: a write of an older change that ends after a newer one
    /// leaves the newer one in the file.
    #[test]
    fn an_older_write_never_replaces_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("arke-save-order-{}", std::process::id()));
        let path = dir.join("settings.json");
        let written = parking_lot::Mutex::new(0);
        let with_recent = |p: &str| Settings {
            recent: vec![PathBuf::from(p)],
            ..Settings::default()
        };
        save_change(&path, &with_recent(r"C:\new"), 2, &written);
        save_change(&path, &with_recent(r"C:\old"), 1, &written);
        assert_eq!(load(&path).unwrap().recent, [PathBuf::from(r"C:\new")]);
        // The same change again is no newer either.
        save_change(&path, &with_recent(r"C:\again"), 2, &written);
        assert_eq!(load(&path).unwrap().recent, [PathBuf::from(r"C:\new")]);
        save_change(&path, &with_recent(r"C:\newer"), 3, &written);
        assert_eq!(load(&path).unwrap().recent, [PathBuf::from(r"C:\newer")]);
        assert_eq!(*written.lock(), 3);
        let _ = std::fs::remove_dir_all(&dir);
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
            home: false,
        });
        mine.session_windows.push(SessionWindow {
            session: mine.session.clone(),
            place: None,
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
        assert_eq!(value["session_windows"], serde_json::json!([]));
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

    /// Folders dragged onto the sidebar go in at an insertion point, whether pinned before or not.
    #[test]
    fn inserting_pins_adds_or_moves_them_to_the_insertion_point() {
        let p = |n: usize| PathBuf::from(format!(r"D:\p{n}"));
        let mut s = Settings {
            favorites: Some(vec![p(0), p(1), p(2)]),
            pinned: vec![p(10)],
            ..Settings::default()
        };
        // A new folder between the first two tiles.
        assert_eq!(s.insert_pin(&p(5), PinSlot::Tile(1), &[]), PinSlot::Tile(1));
        assert_eq!(s.favorites, Some(vec![p(0), p(5), p(1), p(2)]));
        // A tile moved forward: the point is counted with it still there.
        assert_eq!(s.insert_pin(&p(0), PinSlot::Tile(3), &[]), PinSlot::Tile(2));
        assert_eq!(s.favorites, Some(vec![p(5), p(1), p(0), p(2)]));
        // A list entry onto the tiles leaves the list; a tile into the list leaves the tiles.
        s.insert_pin(&p(10), PinSlot::Tile(0), &[]);
        assert_eq!(s.favorites, Some(vec![p(10), p(5), p(1), p(0), p(2)]));
        assert!(s.pinned.is_empty());
        s.insert_pin(&p(1), PinSlot::List(usize::MAX), &[]);
        assert_eq!(s.pinned, [p(1)]);

        // Full tiles: the last other tile makes room, to the top of the list.
        let mut full = Settings {
            favorites: Some((0..FAVORITES_MAX).map(p).collect()),
            pinned: vec![p(50)],
            ..Settings::default()
        };
        full.insert_pin(&p(99), PinSlot::Tile(usize::MAX), &[]);
        let tiles = full.favorites.clone().unwrap();
        assert_eq!(tiles.len(), FAVORITES_MAX);
        assert_eq!(tiles.last(), Some(&p(99)));
        assert_eq!(full.pinned, [p(FAVORITES_MAX - 1), p(50)]);
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
            old.show_extensions
                && old.show_thumbnails
                && old.keep_running
                && old.fast_recycle
                && old.fast_copy,
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
    fn plain_http_allowances_are_per_origin_and_persist() {
        let mut s = Settings::default();
        assert!(s.insecure_http.is_empty(), "nothing is allowed by default");
        s.set_insecure_http("http://u@nas", true);
        s.set_insecure_http("http://u@nas", true);
        s.set_insecure_http("http://u@other", true);
        assert_eq!(s.insecure_http, ["http://u@nas", "http://u@other"]);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.insecure_http, s.insecure_http);
        s.set_insecure_http("http://u@nas", false);
        assert_eq!(s.insecure_http, ["http://u@other"]);
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
    fn the_win_e_switch_is_read_without_loading_the_rest() {
        assert!(switch(
            br#"{"win_e_opens_arke":true,"pinned":[]}"#,
            "win_e_opens_arke"
        ));
        assert!(switch(
            b"\xEF\xBB\xBF{\"win_e_opens_arke\":true}",
            "win_e_opens_arke"
        ));
        assert!(!switch(
            br#"{"win_e_opens_arke":false}"#,
            "win_e_opens_arke"
        ));
        assert!(
            !switch(br#"{"open_folders_in_arke":true}"#, "win_e_opens_arke"),
            "off unless set"
        );
        assert!(!switch(b"{ broken", "win_e_opens_arke"));
    }

    #[test]
    fn the_redirection_guard_policy_is_off_by_default_and_per_machine() {
        let key = "redirection_guard_by_policy";
        assert!(!Settings::default().redirection_guard_by_policy);
        let old = parse_settings(br#"{"pinned":[]}"#, Path::new("settings.json")).unwrap();
        assert!(!old.redirection_guard_by_policy, "missing in older files");
        let on = Settings {
            redirection_guard_by_policy: true,
            ..Settings::default()
        };
        let json = serde_json::to_vec(&on).unwrap();
        assert!(switch(&json, key), "read early without loading the rest");
        assert!(!switch(br#"{"pinned":[]}"#, key));
        let back: Settings = serde_json::from_slice(&json).unwrap();
        assert!(back.redirection_guard_by_policy);
        // A policy of this PC: not exported, and an import keeps this PC's.
        let exported = export_json(&on).unwrap();
        assert!(!parse_export(&exported).unwrap().redirection_guard_by_policy);
        assert!(on.imported(Settings::default()).redirection_guard_by_policy);
        assert!(!Settings::default().imported(on).redirection_guard_by_policy);
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
