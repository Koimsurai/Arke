//! Contents of one tab: toolbar (back/forward/up/refresh, path bar, filter box), sidebar,
//! virtualized file list (or subfolder search results) and status bar. The tab strip lives in
//! `workspace.rs`.
//!
//! Everything here runs on the UI thread and must not do file system or Shell I/O.
//! Directory I/O goes through `DirService`, search through `SearchService`, and Shell calls run
//! on background threads. Results come back over flume channels and are handled in `handle_event`.

use rust_i18n::t;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::filter::filter_by_query;
use arke_core::index_query::{Clock, Facet, Query};
use arke_core::net::FailKind;
use arke_core::service::resort;
use arke_core::{
    DeltaOutcome, DirModel, DirService, Entry, Fs, Gen, History, IoEvent, ListOptions, ListTiming,
    NavKind, SearchHit, SearchResults, SearchService, Selection, SortColumn, SortSpec, TypeAhead,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, KeyDownEvent,
    MouseDownEvent, Pixels, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, actions, prelude::*, px,
};

#[path = "explorer_menu.rs"]
mod menu;
#[cfg(test)]
pub(crate) use menu::{
    ContextMenu as ContextMenuForTest, MenuCommand, MenuEntry as MenuEntryForTest,
};
#[path = "explorer_checks.rs"]
mod checks;
#[path = "explorer_cloud.rs"]
mod cloud;
#[path = "explorer_columns.rs"]
mod columns;
#[path = "explorer_compare.rs"]
mod compare;
#[path = "explorer_confirm.rs"]
mod confirm;
#[path = "explorer_details.rs"]
mod details;
#[path = "explorer_dnd.rs"]
mod dnd;
#[path = "explorer_filters.rs"]
mod filters;
#[path = "explorer_folder_view.rs"]
mod folder_view;
#[path = "explorer_groups.rs"]
pub(crate) mod groups;
#[path = "explorer_icons.rs"]
mod icons;
#[path = "explorer_layout.rs"]
mod layout;
#[path = "explorer_marquee.rs"]
mod marquee;
#[path = "explorer_opcenter.rs"]
mod op_view;
#[path = "explorer_ops.rs"]
mod ops;
#[path = "explorer_path.rs"]
mod path_bar;
#[path = "explorer_preview.rs"]
mod preview;
#[path = "explorer_scope.rs"]
pub(crate) mod scope;
#[path = "explorer_shelf.rs"]
mod shelf_ops;
#[path = "explorer_sizes.rs"]
mod sizes;
#[path = "explorer_tree.rs"]
mod tree;
#[cfg(test)]
pub(crate) use sizes::FolderSize;
#[path = "explorer_view.rs"]
mod view;
#[path = "explorer_viewopts.rs"]
mod viewopts;

pub(crate) use dnd::{
    DragPreview, DraggedFiles, DraggingOutside, PendingDragOut, RightDrag, ShelfDrag, accepts_drop,
    drop_external_into, drop_into, report_drop_target,
};
pub(crate) use icons::IconSize;
pub(crate) use layout::{SharedPreview, shared_preview};
pub(crate) use menu::{app_name, menu_open_with_found, open_with_list, set_open_with};
#[cfg(test)]
pub(crate) use preview::PreviewBody;

pub(crate) use ops::{
    CopyFiles, CutFiles, DeleteFiles, NewFolder, PasteFiles, RedoFileOp, RedoStack, RenameFile,
    UndoFileOp, UndoHistory,
};
#[cfg(test)]
pub(crate) use ops::{changes_share_list, commit_problem, typing_problem};
pub(crate) use view::{caption_buttons, ghost_button, kind_icon, tooltip};

use crate::bench::{Bench, BenchStep};
use crate::media::MediaStore;
use crate::settings::{Density, SettingsStore};
use crate::shell::{Place, Shell};
use crate::text_input::{InputEvent, TextInput};
use crate::theme;

actions!(
    explorer,
    [
        SelectPrev,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        ExtendPrev,
        ExtendNext,
        ExtendPageUp,
        ExtendPageDown,
        ExtendFirst,
        ExtendLast,
        SelectAll,
        ToggleCursor,
        Escape,
        OpenSelected,
        OpenInNewTab,
        OpenLocation,
        GoUp,
        GoBack,
        GoForward,
        Refresh,
        FocusPathBar,
        FocusFilter,
        FocusSearch,
        ToggleSearchMode,
        ToggleHidden,
        /// Ctrl+Shift+C: quoted paths of the selection, one per line, or the current folder if
        /// nothing is selected.
        CopyAsPath,
        MoveLeft,
        MoveRight,
        ViewDetails,
        /// Large icons (Ctrl+Shift+2).
        ViewIcons,
        /// Extra large icons (Ctrl+Shift+1).
        ViewIconsExtraLarge,
        /// Medium icons (Ctrl+Shift+3).
        ViewIconsMedium,
        /// Small icons (Ctrl+Shift+4).
        ViewIconsSmall,
        ViewColumns,
        /// Details view: folders expand in the list (setting, also in the view options flyout).
        ToggleTree,
        CopyToPeer,
        MoveToPeer,
        FocusOtherPane,
        /// Ctrl+Shift+A: put the selection on the shelf.
        AddToShelf,
    ]
);

pub(crate) fn bind_keys(cx: &mut App) {
    path_bar::bind_keys(cx);
    let list = Some("FileList");
    let root = Some("Explorer");
    let filter = Some("FilterBar");
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, list),
        KeyBinding::new("down", SelectNext, list),
        KeyBinding::new("pageup", SelectPageUp, list),
        KeyBinding::new("pagedown", SelectPageDown, list),
        KeyBinding::new("home", SelectFirst, list),
        KeyBinding::new("end", SelectLast, list),
        KeyBinding::new("shift-up", ExtendPrev, list),
        KeyBinding::new("shift-down", ExtendNext, list),
        KeyBinding::new("shift-pageup", ExtendPageUp, list),
        KeyBinding::new("shift-pagedown", ExtendPageDown, list),
        KeyBinding::new("shift-home", ExtendFirst, list),
        KeyBinding::new("shift-end", ExtendLast, list),
        KeyBinding::new("ctrl-a", SelectAll, list),
        KeyBinding::new("ctrl-space", ToggleCursor, list),
        KeyBinding::new("escape", Escape, list),
        KeyBinding::new("enter", OpenSelected, list),
        KeyBinding::new("ctrl-enter", OpenInNewTab, list),
        KeyBinding::new("shift-enter", OpenLocation, list),
        KeyBinding::new("backspace", GoUp, list),
        // Grid view: move one cell left or right (no-op in details view).
        // Details view with the folder tree on: expand/collapse.
        KeyBinding::new("left", MoveLeft, list),
        KeyBinding::new("right", MoveRight, list),
        // Dual pane: Alt+C copies and Alt+M moves to the other side (as in OneCommander), Tab
        // and Shift+Tab switch (there are only two sides, so both go to the other one).
        // Total Commander's F5/F6 are avoided on purpose: F5 is refresh on Windows, and a slip
        // would copy files.
        // Without a split these keys fall through to other handlers.
        KeyBinding::new("alt-c", CopyToPeer, list),
        KeyBinding::new("alt-m", MoveToPeer, list),
        KeyBinding::new("tab", FocusOtherPane, list),
        KeyBinding::new("shift-tab", FocusOtherPane, list),
        // Down in the filter box moves focus to the list.
        KeyBinding::new("down", SelectNext, filter),
        KeyBinding::new("alt-up", GoUp, root),
        KeyBinding::new("alt-left", GoBack, root),
        KeyBinding::new("alt-right", GoForward, root),
        KeyBinding::new("f5", Refresh, root),
        KeyBinding::new("ctrl-r", Refresh, root),
        KeyBinding::new("ctrl-l", FocusPathBar, root),
        KeyBinding::new("alt-d", FocusPathBar, root),
        KeyBinding::new("ctrl-f", FocusFilter, root),
        KeyBinding::new("ctrl-shift-f", FocusSearch, root),
        KeyBinding::new("alt-s", ToggleSearchMode, root),
        KeyBinding::new("ctrl-h", ToggleHidden, root),
        KeyBinding::new("ctrl-shift-c", CopyAsPath, root),
        KeyBinding::new("ctrl-shift-a", AddToShelf, root),
    ]);
    // Same as Explorer: Ctrl+Shift+6 details, Ctrl+Shift+1/2/3/4 extra large, large, medium and
    // small icons.
    // On Windows Shift+digit produces a symbol (Shift+2 = @). KeyBinding::new doesn't map that, so
    // KeyBinding::load is needed to apply the platform keyboard mapping or the keys never match.
    let platform_bindings: Vec<KeyBinding> = [
        (
            "ctrl-shift-6",
            Box::new(ViewDetails) as Box<dyn gpui::Action>,
        ),
        ("ctrl-shift-1", Box::new(ViewIconsExtraLarge)),
        ("ctrl-shift-2", Box::new(ViewIcons)),
        ("ctrl-shift-3", Box::new(ViewIconsMedium)),
        ("ctrl-shift-4", Box::new(ViewIconsSmall)),
    ]
    .into_iter()
    .filter_map(|(keys, action)| {
        let context = gpui::KeyBindingContextPredicate::parse("Explorer")
            .ok()
            .map(std::rc::Rc::new);
        KeyBinding::load(keys, action, context, false, None, cx).ok()
    })
    .collect();
    cx.bind_keys(platform_bindings);
    scope::bind_keys(cx);
    ops::bind_keys(cx);
    confirm::bind_keys(cx);
    menu::bind_keys(cx);
}

/// Delay before showing "loading" for a pending listing, so fast folders don't flash blank.
const LOADING_DELAY: Duration = Duration::from_millis(150);
/// Up to this many items, filter inline on the UI thread (microseconds) to avoid an extra frame of
/// flicker.
const INLINE_FILTER_MAX: usize = 3_000;

/// Button on the error banner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ErrorAction {
    /// Try opening this location again.
    Retry(PathBuf),
    /// Log in to the share containing this location, then open it.
    Login(PathBuf),
    /// Run this file operation again: the items of a failed one that are worth retrying (a
    /// program had them open, say).
    RetryOp(Box<crate::shell::FileOp>),
}

#[derive(Clone)]
pub(crate) struct Services {
    pub(crate) fs: Arc<dyn Fs>,
    pub(crate) shell: Arc<dyn Shell>,
    /// SFTP and WebDAV servers (sign-in, host keys); `fs` and `shell` already route remote
    /// paths there.
    pub(crate) remotes: Arc<arke_remote::Remotes>,
}

pub(crate) enum AppEvent {
    Io(IoEvent),
    Search(SearchResults),
    Places(Vec<Place>),
    /// Refreshed disk capacity: (root, (free, total)).
    DiskSpace(Vec<(PathBuf, (u64, u64))>),
    OpenFailed {
        path: PathBuf,
        error: crate::shell::ShellError,
    },
    ClipboardSet {
        result: Result<u32, crate::shell::ShellError>,
        cut: bool,
        count: usize,
        parent: PathBuf,
        names: std::collections::HashSet<Arc<str>>,
    },
    OpFinished {
        verb: &'static str,
        result: Result<crate::shell::OpOutcome, crate::shell::ShellError>,
        /// Boxed: the follow-up is large and events stay small.
        follow: Box<ops::OpFollowUp>,
    },
    /// Status bar message (Ok) or error (Err), already as display text.
    Notice(Result<String, String>),
    /// Open this folder in Arke (e.g. a shortcut to a folder).
    OpenFolder(PathBuf),
    /// Go to the item's folder and select it ("Open file location" on a shortcut).
    RevealPath(PathBuf),
    ExtractProgress(arke_core::archive::ExtractProgress),
    /// One subfolder's size finished (None if unreadable).
    FolderSize {
        dir: PathBuf,
        name: Arc<str>,
        bytes: Option<u64>,
    },
    CompressDone {
        result: Result<arke_core::archive::CompressSummary, arke_core::archive::ArchiveError>,
        name: String,
        dest: PathBuf,
    },
    ExtractDone {
        result: Result<arke_core::archive::ExtractSummary, arke_core::archive::ArchiveError>,
        folder: Option<String>,
    },
    Preview {
        generation: u64,
        data: preview::PreviewData,
    },
    /// High-resolution image for Quick Look's actual-size mode.
    ZoomImage {
        generation: u64,
        image: Option<Arc<gpui::RenderImage>>,
    },
    ParentColumn {
        dir: PathBuf,
        entries: Arc<Vec<Entry>>,
    },
    /// Link target column: where these links in the folder listed as `generation` point (`None`
    /// when unreadable).
    LinkTargets {
        generation: Gen,
        found: Vec<(Arc<str>, Option<PathBuf>)>,
    },
    /// Path bar: folders for a breadcrumb dropdown or autocomplete.
    PathFolders {
        request: u64,
        dir: PathBuf,
        /// Every folder, hidden ones included; consumers apply the hidden rules.
        folders: Vec<Entry>,
    },
    /// Listing or change for a folder expanded in the Details view tree (`explorer_tree.rs`).
    Tree {
        node: u64,
        rel: Arc<str>,
        event: IoEvent,
    },
    /// Cloud sync states of a folder's items (`explorer_cloud.rs`).
    CloudScan {
        dir: PathBuf,
        version: u64,
        scan: crate::shell::CloudScan,
    },
    /// "Show more options" menu is ready.
    ShellMenu {
        request: u64,
        result: Result<crate::shell::ShellMenu, crate::shell::ShellError>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ViewMode {
    /// Details: one row per item.
    #[default]
    Details,
    /// Large icons: grid with thumbnails for images.
    Icons,
    /// Columns: parent | current folder | preview.
    Columns,
}

/// Events handled by the tab strip (`Workspace`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplorerEvent {
    OpenInNewTab {
        path: PathBuf,
        activate: bool,
    },
    /// Split or unsplit the current tab.
    ToggleSplit,
    /// This pane was clicked (becomes the active side in dual pane).
    Activated,
    /// Dual pane: switch to the other side.
    FocusOtherPane,
    /// A copy or move from this pane finished in `dir`, which this pane isn't showing: the other
    /// side selects `names` if it shows `dir` (Alt+C/Alt+M, drags between the panes).
    Arrived {
        dir: PathBuf,
        names: Vec<Arc<str>>,
    },
    /// Needs a login for this network location (Workspace opens the dialog).
    NetLogin(PathBuf),
    /// Moved to another folder (recorded in Recent).
    Navigated(PathBuf),
    /// An operation changed this remote folder (servers send no change notifications; panes
    /// showing it relist).
    RemoteChanged(PathBuf),
    /// Right-click on the empty part of the top bar (window drag area): window menu, owned by
    /// Workspace.
    WindowMenu(gpui::Point<gpui::Pixels>),
    /// "Import Arke settings" on a `.json` (the settings page is owned by Workspace).
    ImportSettings(PathBuf),
    /// Dual pane: turn synchronized browsing on or off for this tab.
    ToggleSync,
    /// Dual pane: compare the two folders, or stop comparing.
    ToggleCompare,
    /// Dual pane: select what the comparison marked, in both panes.
    SelectDifferences,
}

/// Where this `Explorer` sits in its tab. Set by `Workspace` before every render.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PaneInfo {
    /// The tab is split into two panes.
    pub(crate) split: bool,
    /// This is the active side.
    pub(crate) active: bool,
    /// Window caption buttons go in this pane's top bar (the rightmost pane).
    pub(crate) show_caption: bool,
    /// The operation center button goes in this pane's top bar. It shows jobs from the whole app,
    /// so a split tab draws it once (the rightmost pane), not once per side.
    pub(crate) show_ops: bool,
    /// Current folder of the other pane (copy/move destination).
    pub(crate) peer: Option<PathBuf>,
    /// Share of the list area width.
    pub(crate) share: f32,
    /// Split tab: room at the tab's right end taken by the shared preview (both panes get it).
    pub(crate) reserve: Pixels,
    /// Split tab: this pane's box runs on under the shared preview (the rightmost pane), so its
    /// top bar spans the whole tab and keeps the caption buttons in the corner.
    pub(crate) trailing: bool,
    /// Split tab: this is the active side and the tab's shared preview shows its item.
    pub(crate) preview_host: Option<SharedPreview>,
    /// Back, forward and refresh are drawn in the sidebar header (vertical tabs); the top bar keeps
    /// only up and the path.
    pub(crate) nav_in_sidebar: bool,
    /// Dual pane: synchronized browsing is on for this tab.
    pub(crate) synced: bool,
    /// Dual pane: the folders are being compared.
    pub(crate) comparing: bool,
    /// Dual pane: the sync and compare toggles go in this pane's context bar. They act on the
    /// pair, so a split tab draws them once (the rightmost pane), not once per side.
    pub(crate) pair_toggles: bool,
}

impl Default for PaneInfo {
    fn default() -> Self {
        Self {
            split: false,
            active: true,
            show_caption: true,
            show_ops: true,
            peer: None,
            share: 1.,
            reserve: px(0.),
            trailing: false,
            preview_host: None,
            nav_in_sidebar: false,
            synced: false,
            comparing: false,
            pair_toggles: false,
        }
    }
}

impl EventEmitter<ExplorerEvent> for Explorer {}

/// A location that failed on first open with nothing to go back to. The list shows an error page
/// and the path bar keeps the path.
pub(crate) struct FailedNav {
    pub(crate) path: PathBuf,
    /// Reason text from the system, e.g. folder not found.
    pub(crate) reason: String,
}

struct PendingNav {
    generation: Gen,
    path: PathBuf,
    kind: NavKind,
    /// Names to select once listed (e.g. "Open file location", or the items another program
    /// asked to show selected).
    select: Vec<Arc<str>>,
    /// Per-folder view state to save or put back when this finishes.
    folder_view: folder_view::PendingView,
}

#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct Metrics {
    pub(crate) first_frame: Option<Duration>,
    pub(crate) first_listing: Option<Duration>,
    listing_ready: bool,
}

/// Inputs a filter result depends on: directory, content version, sort, query. Any change means
/// recompute.
type FilterKey = (Gen, u64, SortSpec, String);

/// When only folder sizes changed and the list is this small, resort inline (about 10k comparisons,
/// well under 1 ms).
const INLINE_SORT_MAX: usize = 1_000;
/// Minimum interval between resorts by size while folder sizes are still being computed.
const SIZE_RESORT_EVERY: Duration = Duration::from_millis(300);

/// Returns a sorted copy. When sorting by size, `dir_sizes` supplies folder sizes.
fn sort_entries(
    entries: &[Entry],
    spec: SortSpec,
    dir_sizes: Option<&std::collections::HashMap<Arc<str>, u64>>,
) -> Vec<Entry> {
    match dir_sizes {
        Some(sizes) => spec.sorted_with_dir_sizes(entries, sizes),
        None => resort(entries, spec),
    }
}

struct Filtered {
    key: FilterKey,
    entries: Arc<Vec<Entry>>,
}

// UI flags are independent (editing path, loading, show hidden, ...) and don't fit a state machine.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Explorer {
    pub(crate) services: Services,
    events_tx: flume::Sender<AppEvent>,
    dir_service: DirService,
    search: SearchService,
    pub(crate) model: Option<DirModel>,
    pending: Option<PendingNav>,
    pub(crate) history: History,
    pub(crate) sort: SortSpec,
    /// Last seen default sort and view. Tabs still on the old defaults follow when settings change.
    seen_defaults: (SortSpec, ViewMode),
    /// Per-folder view memory as this tab sees it (`explorer_folder_view.rs`).
    folder_view: folder_view::State,
    /// Show extensions; show thumbnails in grid (settings).
    pub(crate) show_extensions: bool,
    /// Checkboxes next to items (settings).
    pub(crate) show_checkboxes: bool,
    /// Column widths were dragged but not saved yet (saved on release).
    pub(crate) column_resized: bool,
    /// The preview pane's edge was dragged (or reset) but the width is not saved yet.
    pub(crate) preview_resized: bool,
    /// Pressed on the preview pane's edge: where (x), and the pane's width then.
    preview_drag_start: Option<(Pixels, Pixels)>,
    /// Where the symlinks and junctions in this folder point (Link target column).
    link_targets: details::LinkTargets,
    /// Alt-drop with the menu still open: files to drop and destination.
    pub(crate) pending_drop: Option<(Vec<PathBuf>, PathBuf)>,
    /// Right button down on an item, not yet a drag (see `explorer_dnd.rs`).
    pub(crate) right_press: Option<dnd::RightPress>,
    /// Group details view by sort column (settings), plus the computed sections.
    pub(crate) group_rows: bool,
    group_cache: groups::GroupCache,
    pub(crate) show_thumbnails: bool,
    /// Read images and exe icons on network locations too (settings, off by default).
    pub(crate) network_thumbnails: bool,
    /// Language currently used for input placeholders.
    language: crate::settings::Language,
    sort_inflight: Option<(Gen, u64, SortSpec, Option<u64>)>,
    /// Folder size version the current order was sorted with (dir, content version, size version).
    /// `None` if sizes weren't used.
    size_order: Option<(Gen, u64, u64)>,
    /// Just resorted by folder size; wait before the next resort so the list doesn't jump on every
    /// finished folder.
    size_resort_cooling: bool,
    size_resort_task: Option<Task<()>>,
    show_hidden: bool,
    /// Hidden-rules version the list was built with.
    hidden_rules: u32,
    hidden_count: usize,
    pub(crate) selection: Selection,
    /// Cursor index in the displayed list (cache of `selection.cursor()`).
    pub(crate) cursor: Option<usize>,
    /// Scroll to the cursor once it resolves (a folder opened with an item to select, an operation
    /// that names its result), and how.
    scroll_to_cursor: Option<ScrollStrategy>,
    scroll: UniformListScrollHandle,
    list_focus: FocusHandle,
    path_input: Entity<TextInput>,
    pub(crate) editing_path: bool,
    /// Marquee selection in progress.
    marquee: Option<marquee::Marquee>,
    /// The left press landed on an item, so the list's own handler doesn't start a marquee.
    row_pressed: bool,
    list_bounds: marquee::ListBounds,
    /// Folder dropdown opened from a breadcrumb chevron or the overflow button.
    path_popup: Option<path_bar::PathPopup>,
    /// Autocomplete while typing a path.
    suggest: path_bar::Suggest,
    path_bounds: path_bar::PathBounds,
    /// Width of the title bar slot the path bar sits in (it sizes to its content inside it).
    path_slot: path_bar::SlotWidth,
    filter_input: Entity<TextInput>,
    /// Filter box text, excluding uncommitted IME composition.
    pub(crate) query: String,
    filtered: Option<Filtered>,
    filter_inflight: Option<FilterKey>,
    filter_task: Option<Task<()>>,
    /// Content search (`content:`) running over this folder's files.
    content_filter: Option<filters::ContentFilter>,
    /// Filter whose menu is open in the search panel.
    filter_menu: Option<Facet>,
    /// Highlighted row of that menu.
    menu_cursor: usize,
    /// Keyboard focus inside the search panel (Tab from the box). Mouse use keeps focus in the
    /// box so typing goes on there.
    panel_focus: FocusHandle,
    /// Keyboard position in the search panel.
    panel_cursor: scope::PanelItem,
    /// Where the panel and its filter buttons were drawn (to place a filter's menu).
    panel_geometry: std::rc::Rc<std::cell::RefCell<filters::PanelGeometry>>,
    /// Esc closed the panel; it stays closed until the box is focused again or Tab.
    panel_dismissed: bool,
    /// Filter box is in subfolder search mode.
    pub(crate) search_mode: bool,
    /// Root and include-hidden flag of the running walk. If neither changes, only the query is
    /// updated.
    search_root: Option<(PathBuf, bool)>,
    /// "Search in this folder": the subfolder search walks this folder (below the one shown)
    /// instead of the folder shown. Cleared when search mode ends.
    search_in: Option<PathBuf>,
    pub(crate) search_results: Option<SearchResults>,
    /// Deleted or moved away since the walk started: the walk doesn't watch for changes, so
    /// hits at or below these are left out of every result until the next fresh walk.
    search_gone: Vec<PathBuf>,
    pub(crate) search_cursor: Option<usize>,
    /// Search scope is the whole PC (`search_mode` is also true; results come from the index).
    search_global: bool,
    /// Whole-PC query: serial (stale results are dropped), background task, and index size at the
    /// last query.
    global_serial: u64,
    global_task: Option<Task<()>>,
    global_count: Option<usize>,
    global_watch: Option<Subscription>,
    /// The scope strip collapses once typing starts so it doesn't cover the list. Tab or the scope
    /// chip reopens it.
    scope_strip_open: bool,
    search_scroll: UniformListScrollHandle,
    typeahead: TypeAhead,
    pub(crate) view_mode: ViewMode,
    /// Icon size of the Icons view (`explorer_icons.rs`), remembered per folder with the view.
    pub(crate) icon_size: IconSize,
    /// The grid's column count is about to change (new icon size or view): scroll the cursor back
    /// into view once the new count is known.
    recenter_grid: bool,
    /// Ctrl+wheel movement not yet worth a whole step (touchpads send small amounts).
    zoom_accum: f32,
    /// The view options flyout is open (`explorer_viewopts.rs`).
    view_options_open: bool,
    /// The view options flyout was just closed and is fading out.
    pub(crate) closing_view_options: Option<crate::motion::Leaving<()>>,
    /// Its icon size slider is being dragged.
    icon_slider_drag: std::rc::Rc<std::cell::Cell<bool>>,
    /// Display scale at the last render, for thumbnail decode sizes.
    ui_scale: f32,
    /// Details view tree: expanded folders and their listings (`explorer_tree.rs`).
    tree: tree::Tree,
    /// Current grid column count, computed from the width during render.
    pub(crate) grid_cols: usize,
    pub(crate) media: Entity<MediaStore>,
    /// Parent window (HWND) for file operation dialogs.
    owner: Option<isize>,
    pub(crate) renaming: Option<ops::Renaming>,
    pub(crate) context_menu: Option<menu::ContextMenu>,
    /// A dismissed context menu fading out (drawn inert, see `motion::Leaving`).
    pub(crate) closing_menu: Option<crate::motion::Leaving<menu::ContextMenu>>,
    pub(crate) extract_job: Option<menu::ExtractJob>,
    /// After creating a folder, start renaming it once it shows up in the list (`true`: whole name
    /// selected, see [`ops::NewItem::File`]).
    rename_when_appears: Option<(Arc<str>, bool)>,
    /// "New folder…" / "New file…" on a folder item, waiting for that folder's listing.
    pending_create: Option<ops::PendingCreate>,
    /// After delete, wait for the items to disappear, then select the item at the old position.
    pending_reselect: Option<(Vec<Arc<str>>, usize)>,
    pub(crate) ops_running: usize,
    /// Progress of the running file operation (drawn by Arke) and which operation it is.
    pub(crate) op_progress: Option<(Arc<crate::shell::OpProgress>, &'static str)>,
    /// Progress center dropdown in the top bar is open.
    pub(crate) ops_open: bool,
    /// Status bar message, e.g. "Copied 3 items".
    pub(crate) notice: Option<String>,
    /// Notice counting down to auto-dismiss.
    notice_timer: Option<(String, Task<()>)>,
    /// The notice toast drawn last frame (text, offers Undo), to notice it going away.
    notice_shown: Option<(String, bool)>,
    /// A notice toast that just went away, fading out.
    pub(crate) closing_notice: Option<crate::motion::Leaving<(String, bool)>>,
    /// Undo offered on the notice: the notice text it belongs to and the undo id. Ignored once
    /// the notice shows something else.
    pub(crate) notice_undo: Option<(String, u64)>,
    pub(crate) places: Vec<Place>,
    pub(crate) pane: PaneInfo,
    /// Dual pane folder compare: how this pane's items differ from the other side's, by name.
    /// Dropped on navigation.
    pub(crate) compare: Option<Arc<std::collections::HashMap<Arc<str>, arke_core::compare::Mark>>>,
    /// Press inside a multi-selection: keep the selection (it may be dragged) and narrow to this
    /// item on release if no drag happened.
    click_select: Option<usize>,
    /// List density (settings, shared by all tabs).
    pub(crate) density: Density,
    /// Preview pane on the right is open (settings).
    pub(crate) preview_pane: bool,
    /// The preview pane was just turned on: it slides in (only then, not on every redraw).
    pub(crate) preview_cue: crate::motion::Cue,
    /// The user just picked another layout: the list brightens in.
    pub(crate) view_cue: crate::motion::Cue,
    /// Space-bar Quick Look is open.
    pub(crate) quick_look: bool,
    /// Quick Look was just dismissed and is fading out (see `motion::Leaving`).
    pub(crate) closing_quick_look: Option<crate::motion::Leaving<()>>,
    /// Open confirmation dialog (before delete, extension change).
    pub(crate) confirm: Option<confirm::Confirm>,
    /// A cancelled confirmation fading out.
    pub(crate) closing_confirm: Option<crate::motion::Leaving<confirm::Confirm>>,
    /// Subfolder sizes, computed in the background.
    folder_sizes: sizes::FolderSizes,
    pub(crate) preview: Option<preview::Preview>,
    /// Left column of the column view (parent folder).
    parent_column: Option<columns::ParentColumn>,
    preview_gen: u64,
    preview_task: Option<Task<()>>,
    /// Quick Look actual-size mode.
    pub(crate) zoom: preview::Zoom,
    /// Recently rendered PDF pages, so paging back and moving between files is instant.
    pdf_cache: preview::PdfCache,
    /// Window size and scale factor at the last render (PDF pages render in device pixels).
    screen: (gpui::Size<Pixels>, f32),
    /// Cloud sync states of the current folder (`explorer_cloud.rs`).
    pub(crate) cloud: cloud::CloudStates,
    /// Pane width at the last render; decides whether columns and the filter box collapse.
    pane_width: Pixels,
    /// Whether and how wide the preview pane fits at the last render's width.
    fit: layout::Fit,
    /// Details (or search results) column geometry at the last render; `None` in the grid and
    /// the column view.
    cols: Option<layout::Columns>,
    /// Sideways scroll the header was drawn at in this render (`None`: no header), to redraw it
    /// if the list clamps its own.
    header_x: std::cell::Cell<Option<Pixels>>,
    /// Full pane width including the preview pane. The top bar and context bar span it, so it
    /// decides which buttons collapse.
    pub(crate) bar_width: Pixels,
    /// The paper's box: [`Self::bar_width`] without a split tab's shared preview, which the
    /// rightmost pane's top bar spans but its paper doesn't.
    paper_width: Pixels,
    pub(crate) show_loading: bool,
    pub(crate) error: Option<String>,
    /// Failed on first open: the list shows an error page instead of a toast.
    pub(crate) failed: Option<FailedNav>,
    /// Scrollbar drag state.
    pub(crate) scrollbar_drag: crate::scrollbar::ScrollbarDrag,
    /// The same for the sideways scrollbar under Details columns wider than the pane.
    pub(crate) hscrollbar_drag: crate::scrollbar::ScrollbarDrag,
    /// Scroll position and scrollbar for scrollable content in the preview pane and Quick Look
    /// (text, folders, archives).
    pub(crate) preview_scroll: gpui::ScrollHandle,
    /// Where the progress ring is drawn now; eases toward the real progress.
    pub(crate) ring_shown: std::rc::Rc<std::cell::Cell<f32>>,
    pub(crate) preview_scrollbar: crate::scrollbar::ScrollbarDrag,
    pub(crate) look_scroll: gpui::ScrollHandle,
    pub(crate) look_scrollbar: crate::scrollbar::ScrollbarDrag,
    /// Error banner button (server unreachable: retry; login needed: log in).
    pub(crate) error_action: Option<ErrorAction>,
    /// Showing the last cached contents of a network folder (fresh listing pending, or the server
    /// is unreachable).
    pub(crate) showing_cached: bool,
    /// Changes on every navigation; auto-retry only reopens if the user hasn't gone elsewhere.
    retry_token: u64,
    last_timing: Option<ListTiming>,
    window_title: String,
    started_at: Instant,
    pub(crate) metrics: Metrics,
    bench: Option<Bench>,
    loading_task: Option<Task<()>>,
    sort_task: Option<Task<()>>,
    _event_loop: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// Path for messages: only the last two components (`…\Projects\Reports`), since a full path crowds
/// out the reason.
pub(crate) fn short_path(path: &Path) -> String {
    let parts: Vec<_> = path.components().collect();
    if parts.len() <= 3 {
        return path.display().to_string();
    }
    let tail: PathBuf = parts[parts.len() - 2..].iter().collect();
    format!("…{}{}", std::path::MAIN_SEPARATOR, tail.display())
}

pub(crate) fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(err) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log::error!("failed to spawn thread {name}: {err}");
    }
}

impl Explorer {
    // Many fields, each needing an initial value; splitting this up would read worse.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn new(
        services: Services,
        start_path: PathBuf,
        bench: Option<Bench>,
        started_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events_tx, dir_service, search, event_loop) = Self::start_services(&services, cx);

        let path_input = cx.new(|cx| TextInput::new(t!("path.placeholder").to_string(), cx));
        let input_focus = path_input.read(cx).focus_handle(cx);
        let filter_input = cx.new(|cx| TextInput::new(t!("filter.placeholder").to_string(), cx));
        // Thumbnail and system icon cache is shared app-wide; repaint when new images load.
        let media = MediaStore::global(&services, cx);
        let settings = SettingsStore::global(cx);
        let folder_views = crate::folder_views::FolderViewStore::global(cx);
        let subscriptions = vec![
            cx.observe(&media, |_, _, cx| cx.notify()),
            cx.observe(&settings, Self::on_settings_changed),
            cx.observe(&folder_views, Self::on_folder_views_changed),
            // Progress center changes (start, finish, periodic poll): repaint the top bar ring and
            // overlay.
            cx.observe_global::<crate::op_center::OpCenter>(|_, cx| cx.notify()),
            cx.subscribe_in(&path_input, window, Self::on_path_event),
            cx.on_blur(&input_focus, window, |this, _window, cx| {
                if this.editing_path {
                    this.editing_path = false;
                    this.clear_suggestions();
                    cx.notify();
                }
            }),
            cx.subscribe_in(&filter_input, window, Self::on_filter_event),
            // Quick Look is a transient overlay: switching to another window closes it, so it's
            // never found still open (and taking over clicks) on return.
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.close_quick_look(cx);
                }
            }),
        ];

        let list_focus = cx.focus_handle();
        window.focus(&list_focus, cx);
        let grid_bench = bench
            .as_ref()
            .is_some_and(|b| b.mode() == crate::bench::BenchMode::ScrollGrid);
        let defaults = {
            let s = &settings.read(cx).settings;
            (s.default_sort(), s.default_view)
        };

        let mut this = Self {
            services,
            events_tx,
            dir_service,
            search,
            model: None,
            pending: None,
            history: History::default(),
            sort: defaults.0,
            seen_defaults: defaults,
            // A benchmark measures a fixed view, never one saved by the user
            folder_view: folder_view::State::new(
                settings.read(cx).settings.remember_folder_views && bench.is_none(),
                folder_views.read(cx),
            ),
            show_extensions: settings.read(cx).settings.show_extensions,
            show_checkboxes: settings.read(cx).settings.show_checkboxes,
            column_resized: false,
            preview_resized: false,
            preview_drag_start: None,
            link_targets: details::LinkTargets::default(),
            pending_drop: None,
            right_press: None,
            group_rows: settings.read(cx).settings.group_rows,
            group_cache: groups::GroupCache::default(),
            show_thumbnails: settings.read(cx).settings.show_thumbnails,
            network_thumbnails: settings.read(cx).settings.network_thumbnails,
            language: crate::settings::Language::System,
            sort_inflight: None,
            size_order: None,
            size_resort_cooling: false,
            size_resort_task: None,
            show_hidden: settings.read(cx).settings.show_hidden,
            hidden_rules: arke_core::entry::hidden_rules_version(),
            hidden_count: 0,
            selection: Selection::default(),
            cursor: None,
            scroll_to_cursor: None,
            scroll: UniformListScrollHandle::new(),
            list_focus,
            path_input,
            editing_path: false,
            marquee: None,
            row_pressed: false,
            list_bounds: marquee::ListBounds::default(),
            path_popup: None,
            suggest: path_bar::Suggest::default(),
            path_bounds: path_bar::PathBounds::default(),
            path_slot: path_bar::SlotWidth::default(),
            filter_input,
            query: String::new(),
            filtered: None,
            filter_inflight: None,
            filter_task: None,
            content_filter: None,
            filter_menu: None,
            menu_cursor: 0,
            panel_focus: cx.focus_handle(),
            panel_cursor: scope::PanelItem::Scope(scope::Scope::Here),
            panel_geometry: std::rc::Rc::default(),
            panel_dismissed: false,
            search_mode: false,
            search_root: None,
            search_in: None,
            search_results: None,
            search_gone: Vec::new(),
            search_cursor: None,
            search_global: false,
            global_serial: 0,
            global_task: None,
            global_count: None,
            global_watch: None,
            scope_strip_open: false,
            search_scroll: UniformListScrollHandle::new(),
            typeahead: TypeAhead::default(),
            // The grid scroll benchmark starts in large icons.
            view_mode: if grid_bench {
                ViewMode::Icons
            } else {
                defaults.1
            },
            icon_size: IconSize::default(),
            recenter_grid: false,
            zoom_accum: 0.,
            view_options_open: false,
            closing_view_options: None,
            icon_slider_drag: std::rc::Rc::default(),
            ui_scale: 1.,
            tree: tree::Tree::new(settings.read(cx).settings.details_tree),
            grid_cols: 1,
            media,
            owner: Self::owner_from(window),
            renaming: None,
            context_menu: None,
            closing_menu: None,
            extract_job: None,
            rename_when_appears: None,
            pending_create: None,
            pending_reselect: None,
            ops_running: 0,
            op_progress: None,
            ops_open: false,
            notice: None,
            notice_timer: None,
            notice_shown: None,
            closing_notice: None,
            notice_undo: None,
            places: Vec::new(),
            pane: PaneInfo::default(),
            compare: None,
            click_select: None,
            density: settings.read(cx).settings.density,
            preview_pane: settings.read(cx).settings.preview_pane,
            preview_cue: crate::motion::Cue::default(),
            view_cue: crate::motion::Cue::default(),
            quick_look: false,
            closing_quick_look: None,
            confirm: None,
            closing_confirm: None,
            folder_sizes: sizes::FolderSizes::default(),
            preview: None,
            parent_column: None,
            preview_gen: 0,
            zoom: preview::Zoom::default(),
            pdf_cache: preview::PdfCache::default(),
            screen: (gpui::size(px(1280.), px(800.)), 1.),
            cloud: cloud::CloudStates::default(),
            preview_task: None,
            pane_width: px(1000.),
            fit: layout::Fit::default(),
            cols: None,
            header_x: std::cell::Cell::new(None),
            bar_width: px(1000.),
            paper_width: px(1000.),
            show_loading: false,
            error: None,
            failed: None,
            scrollbar_drag: crate::scrollbar::ScrollbarDrag::default(),
            hscrollbar_drag: crate::scrollbar::ScrollbarDrag::default(),
            preview_scroll: gpui::ScrollHandle::new(),
            ring_shown: std::rc::Rc::default(),
            preview_scrollbar: crate::scrollbar::ScrollbarDrag::default(),
            look_scroll: gpui::ScrollHandle::new(),
            look_scrollbar: crate::scrollbar::ScrollbarDrag::default(),
            error_action: None,
            showing_cached: false,
            retry_token: 0,
            last_timing: None,
            window_title: String::new(),
            started_at,
            metrics: Metrics::default(),
            bench,
            loading_task: None,
            sort_task: None,
            _event_loop: event_loop,
            _subscriptions: subscriptions,
        };
        this.navigate(start_path, NavKind::New, cx);
        this
    }

    /// Starts background services and the foreground task that receives their results.
    fn start_services(
        services: &Services,
        cx: &mut Context<Self>,
    ) -> (flume::Sender<AppEvent>, DirService, SearchService, Task<()>) {
        let (events_tx, events_rx) = flume::unbounded::<AppEvent>();
        let io_tx = events_tx.clone();
        let mut dir_service = DirService::new(
            services.fs.clone(),
            Arc::new(move |event| {
                let _ = io_tx.send(AppEvent::Io(event));
            }),
        );
        if let Some(probe) = services.shell.net_probe() {
            dir_service = dir_service.with_probe(probe);
        }
        let search_tx = events_tx.clone();
        let search = SearchService::new(
            services.fs.clone(),
            Arc::new(move |results| {
                let _ = search_tx.send(AppEvent::Search(results));
            }),
        );
        // Background results are handled in order on this foreground task; awaiting doesn't block
        // the UI thread.
        let event_loop = cx.spawn(async move |this, cx| {
            while let Ok(event) = events_rx.recv_async().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        {
            let shell = services.shell.clone();
            let tx = events_tx.clone();
            spawn_thread("places", move || {
                let _ = tx.send(AppEvent::Places(shell.load_places()));
            });
        }

        (events_tx, dir_service, search, event_loop)
    }

    // ───────────────────────── Navigation ─────────────────────────

    /// Path to show right now: the target while navigating, so feedback is immediate.
    pub(crate) fn display_path(&self) -> Option<&Path> {
        self.pending
            .as_ref()
            .map(|p| p.path.as_path())
            .or_else(|| self.model.as_ref().map(|m| m.path.as_path()))
            .or_else(|| self.failed.as_ref().map(|f| f.path.as_path()))
    }

    pub(crate) fn current_path(&self) -> Option<&Path> {
        self.model.as_ref().map(|m| m.path.as_path())
    }

    /// Relists if this folder is shown (after another pane changed it on a server).
    pub(crate) fn reload_if_showing(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.current_path() == Some(path) {
            self.reload(cx);
        }
    }

    /// The folder shown is on an SFTP or WebDAV server.
    pub(crate) fn on_remote(&self) -> bool {
        self.display_path()
            .is_some_and(arke_core::remote::is_remote)
    }

    /// Tab title: the folder name (`C:\` for drive roots, the share name for `\server\share`).
    pub(crate) fn tab_title(&self) -> SharedString {
        match self.display_path() {
            Some(p) => tab_title_for(p).into(),
            None => "Arke".into(),
        }
    }

    /// Sends a navigation request and returns immediately. The switch happens when the first result
    /// arrives.
    pub(crate) fn navigate(&mut self, path: PathBuf, kind: NavKind, cx: &mut Context<Self>) {
        self.navigate_selecting(path, kind, None, cx);
    }

    pub(crate) fn navigate_selecting(
        &mut self,
        path: PathBuf,
        kind: NavKind,
        select: Option<Arc<str>>,
        cx: &mut Context<Self>,
    ) {
        self.navigate_selecting_all(path, kind, select.into_iter().collect(), cx);
    }

    /// Like [`Self::navigate`], selecting every name in `select` once the folder is listed.
    pub(crate) fn navigate_selecting_all(
        &mut self,
        path: PathBuf,
        kind: NavKind,
        select: Vec<Arc<str>>,
        cx: &mut Context<Self>,
    ) {
        // Remote URLs in one spelling (`Path::join` adds backslashes), so history and the
        // sidebar compare them right.
        let path = arke_core::remote::canonical(path);
        // A network location was opened, so the NAS is awake and global search can refresh its
        // index.
        crate::global_search::GlobalSearch::wake(&path, cx);
        // Before the request: the listing is sorted the way the target folder is shown.
        let folder_view = self.folder_view_on_navigate(&path, kind, cx);
        let generation = self.dir_service.navigate(
            path.clone(),
            ListOptions {
                sort: self.sort,
                show_hidden: self.show_hidden,
                watch: true,
            },
        );
        self.retry_token += 1;
        if kind != NavKind::Refresh {
            // Navigated away via the sidebar or a path bar dropdown: drop the path input.
            self.editing_path = false;
            self.clear_suggestions();
            self.error = None;
            self.failed = None;
            self.error_action = None;
            self.notice = None;
            self.renaming = None;
            self.rename_when_appears = None;
            self.pending_create = None;
            self.pending_reselect = None;
            // Filter and search belong to the old folder, and so does a folder comparison.
            self.reset_filter(cx);
            self.compare = None;

            // So does Quick Look: left open, it would pop up again over the new folder.
            self.close_quick_look(cx);
        }
        self.pending = Some(PendingNav {
            generation,
            path,
            kind,
            select,
            folder_view,
        });
        self.typeahead.reset();
        self.loading_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOADING_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.generation == generation && p.kind != NavKind::Refresh)
                {
                    this.show_loading = true;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// Error banner button: retry, or log in with credentials and then open.
    pub(crate) fn run_error_action(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(action) = self.error_action.take() else {
            return;
        };
        self.error = None;
        match action {
            ErrorAction::Retry(path) => self.retry(path, cx),
            ErrorAction::RetryOp(op) => self.retry_op(*op, cx),
            ErrorAction::Login(path) => {
                // Arke's own dialog (opened by Workspace) asks for credentials and reopens this tab
                // on success.
                // Keep the message and button so they can be used again after a cancel or failure.
                let _ = window;
                self.error = Some(t!("error.login_failed", name = path.display()).into_owned());
                self.error_action = Some(ErrorAction::Login(path.clone()));
                cx.emit(ExplorerEvent::NetLogin(path));
            }
        }
        cx.notify();
    }

    /// Opens again (refreshes if it's the same location, without adding a history entry).
    fn retry(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.error = None;
        self.error_action = None;
        let kind = if self.current_path() == Some(path.as_path()) {
            NavKind::Refresh
        } else {
            NavKind::New
        };
        self.navigate(path, kind, cx);
    }

    /// While the server is unreachable, probe in the background with backoff (5, 10, 20, 30 s, up
    /// to
    /// 10 min). Reopens automatically if the server is back and the user hasn't navigated away.
    /// Only probes; doesn't relist or repeat the error.
    fn schedule_auto_retry(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // SFTP and WebDAV servers aren't probed; Retry is one click.
        if arke_core::remote::is_remote(&path) {
            return;
        }
        let Some(probe) = self.services.shell.net_probe() else {
            return;
        };
        let token = self.retry_token;
        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let mut delay = Duration::from_secs(5);
            while started.elapsed() < Duration::from_mins(10) {
                cx.background_executor().timer(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
                let waiting = this
                    .update(cx, |this, _| {
                        this.retry_token == token
                            && this.error_action == Some(ErrorAction::Retry(path.clone()))
                    })
                    .unwrap_or(false);
                if !waiting {
                    return;
                }
                let (tx, rx) = flume::bounded(1);
                let probe = probe.clone();
                let target = path.clone();
                spawn_thread("net-retry-probe", move || {
                    let up = probe
                        .unc(&target)
                        .and_then(|unc| arke_core::net::server_of(&unc))
                        .is_some_and(|server| probe.reach(&server) == arke_core::net::Reach::Up);
                    let _ = tx.send(up);
                });
                if rx.recv_async().await.unwrap_or(false) {
                    let _ = this.update(cx, |this, cx| {
                        if this.retry_token == token
                            && this.error_action == Some(ErrorAction::Retry(path.clone()))
                        {
                            log::info!("server is back, reopening {}", path.display());
                            this.retry(path.clone(), cx);
                        }
                    });
                    return;
                }
            }
        })
        .detach();
    }

    /// On returning to Arke, recheck network folders since SMB change notifications are sometimes
    /// missed. No-op for local folders.
    pub(crate) fn recheck_network(&self) {
        self.dir_service.recheck();
    }

    /// Whether the path is inside a cloud-synced folder such as OneDrive: a sync root listed in the
    /// sidebar, or one found by asking the Cloud Files API about a folder opened in this tab.
    pub(crate) fn in_cloud(&self, path: &Path) -> bool {
        self.places
            .iter()
            .any(|p| matches!(p.kind, crate::shell::PlaceKind::Cloud) && path.starts_with(&p.path))
            || self.cloud.detected(path)
    }

    /// Refreshes sidebar disk capacity (after delete or copy, on returning to Arke, after emptying
    /// the
    /// recycle bin). Only drives with capacity bars (local fixed and removable); never network
    /// drives.
    pub(crate) fn refresh_disk_space(&self) {
        let roots: Vec<PathBuf> = self
            .places
            .iter()
            .filter(|p| {
                matches!(
                    p.kind,
                    crate::shell::PlaceKind::Drive {
                        remote: false,
                        space: Some(_)
                    }
                )
            })
            .map(|p| p.path.clone())
            .collect();
        if roots.is_empty() {
            return;
        }
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("disk-space", move || {
            let _ = tx.send(AppEvent::DiskSpace(shell.disk_spaces(&roots)));
        });
    }

    pub(crate) fn reload_places(&self) {
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("places", move || {
            let _ = tx.send(AppEvent::Places(shell.load_places()));
        });
    }

    /// State for the sidebar nav buttons: (can go back, can go forward, can refresh).
    pub(crate) const fn nav_state(&self) -> (bool, bool, bool) {
        (
            self.history.can_go_back(),
            self.history.can_go_forward(),
            self.model.is_some(),
        )
    }

    pub(crate) fn go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.history.back_target() {
            self.navigate(target.to_path_buf(), NavKind::Back, cx);
        }
    }

    pub(crate) fn go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.history.forward_target() {
            self.navigate(target.to_path_buf(), NavKind::Forward, cx);
        }
    }

    fn go_up(&mut self, _: &GoUp, _: &mut Window, cx: &mut Context<Self>) {
        let Some(here) = self.display_path().map(Path::to_path_buf) else {
            return;
        };
        if let Some(parent) = arke_core::path::parent(&here) {
            // From a share root (`\\server\docs`) up to the server: select the share.
            let came_from = arke_core::path::leaf_name(&here).map(Arc::from);
            self.navigate_selecting(parent, NavKind::New, came_from, cx);
        }
    }

    pub(crate) fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.folder_sizes.invalidate();
        self.reload(cx);
        if self.search_mode {
            // Refresh also rewalks subfolders.
            self.search_root = None;
            self.update_search(cx);
        }
    }

    /// Selects these names when the navigation in flight finishes (a window opened for a folder
    /// with items to show selected).
    pub(crate) fn select_on_arrival(&mut self, names: Vec<Arc<str>>) {
        if let Some(pending) = &mut self.pending {
            pending.select = names;
        }
    }

    /// Selects `names` when this view shows (or is loading) `folder` ("Show in folder" arrives
    /// after the folder was opened). Returns whether it was this view's folder.
    pub(crate) fn select_in_folder(
        &mut self,
        folder: &Path,
        names: Vec<Arc<str>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let same = |a: &Path| {
            arke_core::takeover::within(a, folder) && arke_core::takeover::within(folder, a)
        };
        if let Some(pending) = &self.pending {
            if !same(&pending.path) {
                return false;
            }
            self.select_on_arrival(names);
            return true;
        }
        if !self.current_path().is_some_and(same) || names.is_empty() {
            return false;
        }
        let first = names[0].clone();
        self.selection = Selection::default();
        self.selection.select_names(names);
        self.selection.set_cursor_name(Some(first));
        self.cursor = None;
        self.scroll_to_cursor = Some(ScrollStrategy::Center);
        // The folder is already listed, so nothing else will place the cursor: do it now, which
        // also scrolls the item to the middle.
        self.resolve_cursor();
        cx.notify();
        true
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        // Navigation in flight: resend it (e.g. when toggling hidden files) instead of refreshing
        // the old folder.
        if let Some((path, kind, select)) = self
            .pending
            .as_ref()
            .map(|p| (p.path.clone(), p.kind, p.select.clone()))
        {
            self.navigate_selecting_all(path, kind, select, cx);
        } else if let Some(path) = self.current_path().map(Path::to_path_buf) {
            self.navigate(path, NavKind::Refresh, cx);
            self.tree_relist();
        }
    }

    /// Ctrl+H: changes the setting for all tabs (`on_settings_changed` relists).
    fn toggle_hidden(&mut self, _: &ToggleHidden, _: &mut Window, cx: &mut Context<Self>) {
        let show = !self.show_hidden;
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.show_hidden = show, cx));
    }

    // ───────────────────────── Background events ─────────────────────────

    fn handle_event(&mut self, event: AppEvent, cx: &mut Context<Self>) {
        match event {
            AppEvent::Places(places) => {
                self.places = places;
                cx.notify();
            }
            AppEvent::DiskSpace(spaces) => {
                let mut changed = false;
                for place in &mut self.places {
                    if let crate::shell::PlaceKind::Drive { space, .. } = &mut place.kind
                        && let Some((_, now)) = spaces.iter().find(|(root, _)| *root == place.path)
                        && *space != Some(*now)
                    {
                        *space = Some(*now);
                        changed = true;
                    }
                }
                if changed {
                    cx.notify();
                }
            }
            AppEvent::OpenFailed { path, error } => {
                let name = path.file_name().unwrap_or(path.as_os_str());
                self.error = Some(
                    t!(
                        "error.open_failed",
                        name = name.to_string_lossy(),
                        error = error
                    )
                    .into_owned(),
                );
                cx.notify();
            }
            AppEvent::OpenFolder(path) => self.navigate(path, NavKind::New, cx),
            AppEvent::RevealPath(path) => match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => self.navigate_selecting(
                    parent.to_path_buf(),
                    NavKind::New,
                    Some(name.to_string_lossy().into()),
                    cx,
                ),
                // Drive root: open it directly.
                _ => self.navigate(path, NavKind::New, cx),
            },
            AppEvent::Io(event) => self.handle_io(event, cx),
            AppEvent::CloudScan { dir, version, scan } => {
                self.handle_cloud_scan(dir, version, scan, cx);
            }
            AppEvent::Search(results) => self.handle_search(results, cx),
            AppEvent::ClipboardSet {
                result,
                cut,
                count,
                parent,
                names,
            } => self.handle_clipboard_set(result, cut, count, parent, names, cx),
            AppEvent::OpFinished {
                verb,
                result,
                follow,
            } => self.handle_op_finished(verb, result, *follow, cx),
            AppEvent::Notice(Ok(msg)) => {
                self.notice = Some(msg);
                cx.notify();
            }
            AppEvent::Notice(Err(msg)) => {
                self.error = Some(msg);
                cx.notify();
            }
            AppEvent::ExtractProgress(p) => self.handle_extract_progress(p, cx),
            AppEvent::FolderSize { dir, name, bytes } => {
                self.handle_folder_size(&dir, name, bytes, cx);
            }
            AppEvent::CompressDone { result, name, dest } => {
                self.handle_compress_done(result, name, dest, cx);
            }
            AppEvent::ExtractDone { result, folder } => {
                self.handle_extract_done(result, folder, cx);
            }
            AppEvent::Preview { generation, data } => self.handle_preview(generation, data, cx),
            AppEvent::ZoomImage { generation, image } => {
                self.handle_zoom_image(generation, image, cx);
            }
            AppEvent::PathFolders {
                request,
                dir,
                folders,
            } => self.handle_path_folders(request, &dir, folders, cx),
            AppEvent::ParentColumn { dir, entries } => {
                self.handle_parent_column(dir, entries, cx);
            }
            AppEvent::LinkTargets { generation, found } => {
                self.handle_link_targets(generation, found, cx);
            }
            AppEvent::ShellMenu { request, result } => self.handle_shell_menu(request, result, cx),
            AppEvent::Tree { node, rel, event } => self.handle_tree_io(node, &rel, event, cx),
        }
    }

    fn handle_io(&mut self, event: IoEvent, cx: &mut Context<Self>) {
        let current = self.dir_service.current_gen();
        match event {
            IoEvent::Listing {
                generation,
                path,
                entries,
                sorted_by,
                complete,
                hidden_count,
                timing,
                cached,
            } if generation == current => {
                if let Some(pending) = self.pending.take_if(|p| p.generation == generation) {
                    let folder_view = pending.folder_view;
                    self.commit_navigation(pending, &path, sorted_by);
                    self.folder_view_on_commit(folder_view, cx);
                    cx.emit(ExplorerEvent::Navigated(path.clone()));
                }
                let Some(model) = self.model.as_mut().filter(|m| m.generation == generation) else {
                    return;
                };
                let outcome = model.set_snapshot(entries, sorted_by, complete);
                self.hidden_count = hidden_count;
                self.showing_cached = cached;
                if complete {
                    self.last_timing = Some(timing);
                    if self.metrics.first_listing.is_none() {
                        self.metrics.listing_ready = true;
                    }
                    // Full content replaced: drop selections that no longer exist (nested tree
                    // rows included, so the whole list).
                    if let Some(view) = self.view() {
                        self.selection.retain_existing(&view);
                    }
                }
                if outcome == DeltaOutcome::NeedsRescan {
                    self.reload(cx);
                    return;
                }
                self.after_content_change(cx);
            }
            IoEvent::ListFailed {
                generation,
                path,
                error,
                kind,
            } if generation == current => {
                self.list_failed(path, error, kind, cx);
            }
            IoEvent::Delta { generation, delta } if generation == current => {
                self.apply_folder_delta(generation, delta, cx);
            }
            IoEvent::IsFile { generation, path } if generation == current => {
                self.arrived_at_file(path, cx);
            }
            IoEvent::NeedsRescan { generation } if generation == current => self.reload(cx),
            // Stale results are dropped.
            _ => {}
        }
    }

    /// Changes reported by the watcher of the folder shown.
    fn apply_folder_delta(
        &mut self,
        generation: Gen,
        delta: arke_core::Delta,
        cx: &mut Context<Self>,
    ) {
        // With folders expanded, rows above the screen may come and go: keep the view.
        let anchor = self.scroll_anchor(None);
        let Some(model) = self.model.as_mut().filter(|m| m.generation == generation) else {
            return;
        };
        let removed = delta.removed.clone();
        if model.apply_delta(delta) == DeltaOutcome::NeedsRescan {
            self.reload(cx);
            return;
        }
        self.selection.remove_names(&removed);
        self.after_content_change(cx);
        self.restore_scroll_anchor(anchor);
    }

    /// The folder asked for can't be listed: stay put and say why (or show the error page).
    fn list_failed(
        &mut self,
        path: PathBuf,
        error: String,
        kind: FailKind,
        cx: &mut Context<Self>,
    ) {
        let failed_nav = self.pending.take();
        let restore = failed_nav
            .as_ref()
            .is_some_and(|p| p.kind != NavKind::Refresh);
        if let Some(pending) = failed_nav {
            // Staying put: the folder still on screen gets its own view back
            self.folder_view_on_fail(pending.folder_view, cx);
        }
        self.show_loading = false;
        self.loading_task = None;
        if self.model.is_none() {
            // Nothing to go back to: the list shows an error page and the path bar keeps
            // this location.
            self.failed = Some(FailedNav {
                path: path.clone(),
                reason: error,
            });
        } else {
            self.error =
                Some(t!("error.open_failed", name = short_path(&path), error = error).into_owned());
        }
        self.error_action = match kind {
            FailKind::Offline => Some(ErrorAction::Retry(path.clone())),
            FailKind::NeedsLogin => Some(ErrorAction::Login(path.clone())),
            FailKind::Other => None,
        };
        let offline = kind == FailKind::Offline;
        if offline && let Some(failed) = self.failed.as_mut() {
            failed.reason.push_str(&t!("error.auto_retry"));
        } else if offline && let Some(error) = self.error.as_mut() {
            error.push_str(&t!("error.auto_retry"));
        }
        // Navigation failed: relist the previous folder to restore watching (the old
        // watcher stopped when
        // the generation changed).
        if restore && let Some(back) = self.current_path().map(Path::to_path_buf) {
            self.navigate(back, NavKind::Refresh, cx);
        }
        // Server unreachable: reopen automatically when it comes back (e.g. VPN connects).
        if offline {
            self.schedule_auto_retry(path, cx);
        }
        cx.notify();
    }

    /// The location opened was a file (a full path typed or pasted in the path bar, a command line
    /// argument...): open it with its default app and show its folder with it selected, like
    /// Explorer.
    fn arrived_at_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let pending = self.pending.take();
        let kind = pending.as_ref().map_or(NavKind::New, |p| p.kind);
        if let Some(pending) = pending {
            self.folder_view_on_fail(pending.folder_view, cx);
        }
        self.show_loading = false;
        self.loading_task = None;
        // A refresh of a folder that has since become a file only shows the folder around it.
        let kind = if kind == NavKind::Refresh {
            NavKind::New
        } else {
            self.open_file(path.clone(), cx);
            kind
        };
        match (arke_core::path::parent(&path), path.file_name()) {
            (Some(parent), Some(name)) => {
                let name: Arc<str> = name.to_string_lossy().into();
                self.navigate_selecting(parent, kind, Some(name), cx);
            }
            // No folder to show (can't happen for a file); restore watching the current one.
            _ => {
                if let Some(back) = self.current_path().map(Path::to_path_buf) {
                    self.navigate(back, NavKind::Refresh, cx);
                }
            }
        }
        cx.notify();
    }

    fn after_content_change(&mut self, cx: &mut Context<Self>) {
        self.tree_prune();
        self.ensure_sorted(cx);
        self.ensure_filtered(cx);
        self.try_reselect_after_delete();
        self.resolve_cursor();
        cx.notify();
    }

    fn commit_navigation(&mut self, pending: PendingNav, path: &Path, sorted_by: SortSpec) {
        self.failed = None;
        let previous = self.model.as_ref().map(|m| m.path.clone());
        self.history.commit(pending.kind, previous.as_deref(), path);
        if pending.kind != NavKind::Refresh {
            // Going up selects the folder we came from; "Open file location" selects the given
            // item.
            let mut keep = pending.select;
            if keep.is_empty()
                && let Some(came_from) = previous
                    .as_deref()
                    .filter(|prev| prev.parent() == Some(path))
                    .and_then(Path::file_name)
            {
                keep.push(Arc::from(came_from.to_string_lossy().as_ref()));
            }
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            // The folder we came from goes to the middle, as in Explorer.
            self.scroll_to_cursor = (!keep.is_empty()).then_some(ScrollStrategy::Center);
            self.selection = Selection::default();
            if !keep.is_empty() {
                let first = keep[0].clone();
                self.selection.select_names(keep);
                self.selection.set_cursor_name(Some(first));
            }
            self.cursor = None;
            self.tree_clear();
        }
        // Refresh keeps the selection.
        self.model = Some(DirModel::new(
            path.to_path_buf(),
            pending.generation,
            sorted_by,
        ));
        self.filtered = None;
        self.show_loading = false;
        self.loading_task = None;
    }

    /// Restores the cursor by name after the contents or filter change.
    fn resolve_cursor(&mut self) {
        let Some(view) = self.view() else {
            self.cursor = None;
            return;
        };
        self.cursor = self.selection.cursor_index(&view, self.cursor);
        if let Some(strategy) = self.scroll_to_cursor
            && let Some(ix) = self.cursor
        {
            self.scroll_to_index(ix, strategy);
            self.scroll_to_cursor = None;
        }
    }

    // ───────────────────────── Sorting ─────────────────────────

    /// The user picked a sort (column header): show it and remember it for this folder.
    pub(crate) fn set_sort(&mut self, sort: SortSpec, cx: &mut Context<Self>) {
        self.apply_sort(sort, cx);
        self.remember_folder_view(cx);
    }

    /// Shows the list in `sort` without recording it anywhere (defaults, remembered views).
    fn apply_sort(&mut self, sort: SortSpec, cx: &mut Context<Self>) {
        self.sort = sort;
        self.tree_resort();
        self.ensure_sorted(cx);
        cx.notify();
    }

    /// Resorts on the background executor if the list order doesn't match the chosen sort
    /// (100k items take tens of ms, too slow for the UI thread).
    ///
    /// Sorting by size uses the background-computed folder sizes (`explorer_sizes.rs`). Resort when
    /// sizes change or when the list changes, since listings from the service carry no folder
    /// sizes.
    pub(super) fn ensure_sorted(&mut self, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let want = self
            .sort
            .uses(SortColumn::Size)
            .then(|| self.folder_sizes.revision())
            .flatten();
        let have = self
            .size_order
            .filter(|o| (o.0, o.1) == (model.generation, model.version))
            .map(|o| o.2);
        let key = (model.generation, model.version, self.sort, want);
        // Column and direction already match; only folder sizes are out of date.
        let only_sizes = model.sorted_by == self.sort;
        if (only_sizes && have == want) || self.sort_inflight == Some(key) {
            return;
        }
        if only_sizes
            && have.is_some()
            && want.is_some()
            && self.size_resort_cooling
            && self.folder_sizes.computing()
        {
            // More folders finished; resort together once the cooldown timer fires.
            return;
        }
        let spec = self.sort;
        let sizes = want.map(|_| self.folder_sizes.snapshot());
        let (generation, version) = (model.generation, model.version);
        if only_sizes && model.entries.len() <= INLINE_SORT_MAX {
            // Small lists sort inline (under 1 ms) so a refresh doesn't briefly show name order.
            let sorted = sort_entries(&model.entries, spec, sizes.as_deref());
            self.apply_sorted(sorted, spec, want, cx);
            return;
        }
        self.sort_inflight = Some(key);
        let entries = model.entries.clone();
        self.sort_task = Some(cx.spawn(async move |this, cx| {
            let sorted = cx
                .background_executor()
                .spawn(async move { sort_entries(&entries, spec, sizes.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sort_inflight = None;
                if let Some(model) = this.model.as_ref()
                    && model.generation == generation
                    && model.version == version
                    && this.sort == spec
                {
                    this.apply_sorted(sorted, spec, want, cx);
                    // The user changed the sort: keep the selected item on screen. Don't scroll
                    // when only folder sizes arrived.
                    if !only_sizes && let Some(ix) = this.cursor {
                        this.scroll_to_index(ix, ScrollStrategy::Nearest);
                    }
                }
                // Resort if anything changed or the sort was switched again meanwhile.
                this.ensure_sorted(cx);
            });
        }));
    }

    /// Swaps in the sorted list. `sizes_rev` is the folder size version used (`None` if unused).
    fn apply_sorted(
        &mut self,
        sorted: Vec<Entry>,
        spec: SortSpec,
        sizes_rev: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        model.replace_sorted(Arc::new(sorted), spec);
        self.size_order = sizes_rev.map(|rev| (model.generation, model.version, rev));
        if sizes_rev.is_some() && self.folder_sizes.computing() {
            self.size_resort_cooling = true;
            self.size_resort_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SIZE_RESORT_EVERY).await;
                let _ = this.update(cx, |this, cx| {
                    this.size_resort_cooling = false;
                    this.ensure_sorted(cx);
                });
            }));
        }
        self.ensure_filtered(cx);
        self.resolve_cursor();
        cx.notify();
    }

    // ───────────────────────── Filter and search ─────────────────────────

    /// The displayed list: filter results when there's a query, otherwise the whole folder.
    pub(crate) fn view(&self) -> Option<Arc<Vec<Entry>>> {
        let model = self.model.as_ref()?;
        if self.is_filtering()
            && let Some(f) = self
                .filtered
                .as_ref()
                .filter(|f| f.key.0 == model.generation)
        {
            // Keep the previous result while the filter recomputes so the full list doesn't flash
            // back.
            return Some(f.entries.clone());
        }
        if let Some(tree) = self.tree_view() {
            return Some(tree);
        }
        Some(model.entries.clone())
    }

    /// No name and no filters to search for (blank, or `content:""` still being typed).
    pub(crate) fn nothing_to_search(&self) -> bool {
        // The clock only shifts date ranges, which doesn't change whether there are any.
        Query::parse(&self.query, Clock::default()).is_empty()
    }

    fn is_filtering(&self) -> bool {
        !self.search_mode && !self.query.trim().is_empty()
    }

    fn ensure_filtered(&mut self, cx: &mut Context<Self>) {
        if !self.is_filtering() {
            self.filtered = None;
            self.filter_inflight = None;
            self.filter_task = None;
            self.content_filter = None;
            return;
        }
        let Some(model) = &self.model else { return };
        let key: FilterKey = (
            model.generation,
            model.version,
            model.sorted_by,
            self.query.clone(),
        );
        if self.filtered.as_ref().is_some_and(|f| f.key == key)
            || self.filter_inflight.as_ref() == Some(&key)
        {
            return;
        }
        let entries = model.entries.clone();
        let query = Query::parse(&self.query, crate::win::clock());
        if query.content_needle().is_some() {
            self.start_content_filter(key, entries, query, cx);
            return;
        }
        self.content_filter = None;
        if entries.len() <= INLINE_FILTER_MAX {
            let result = filter_by_query(&entries, &query).unwrap_or_default();
            self.filtered = Some(Filtered {
                key,
                entries: Arc::new(result),
            });
            self.filter_inflight = None;
            self.filter_task = None;
            self.drop_filtered_out();
            return;
        }
        self.filter_inflight = Some(key.clone());
        self.filter_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { filter_by_query(&entries, &query).unwrap_or_default() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.filter_inflight = None;
                let current = this
                    .model
                    .as_ref()
                    .map(|m| (m.generation, m.version, m.sorted_by, this.query.clone()));
                if this.is_filtering() && current.as_ref() == Some(&key) {
                    this.filtered = Some(Filtered {
                        key,
                        entries: Arc::new(result),
                    });
                    this.drop_filtered_out();
                    this.resolve_cursor();
                    cx.notify();
                }
                // Recompute if anything changed or the query was edited meanwhile.
                this.ensure_filtered(cx);
            });
        }));
    }

    /// A new filter result is on screen: items it hides leave the selection (and the cursor), so
    /// the selection bar, the preview and every action (delete, cut, drag, ...) only ever see
    /// visible items. They don't come back when the filter is cleared.
    fn drop_filtered_out(&mut self) {
        if !self.is_filtering() {
            return;
        }
        if let Some(view) = self.view() {
            self.selection.retain_visible(&view);
            self.click_select = None;
        }
    }

    fn on_filter_event(
        &mut self,
        _: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Changed => {
                let query = self.filter_input.read(cx).committed_text();
                if query != self.query {
                    self.query = query;
                    self.scope_strip_open = false;
                    self.filter_menu = None;
                    self.on_query_changed(cx);
                }
            }
            InputEvent::Confirm => {
                // Enter opens the item under the cursor, or the first one if there's no cursor.
                let ix = if self.search_mode {
                    self.search_cursor
                } else {
                    self.cursor
                }
                .unwrap_or(0);
                if ix < self.list_len() {
                    self.open_index(ix, cx);
                }
            }
            InputEvent::Cancel => {
                self.reset_filter(cx);
                window.focus(&self.list_focus, cx);
                cx.notify();
            }
        }
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        if self.search_mode {
            self.update_search(cx);
        } else {
            self.ensure_filtered(cx);
            self.resolve_cursor();
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// Clears the filter text and leaves search mode.
    fn reset_filter(&mut self, cx: &mut Context<Self>) {
        if !self.query.is_empty() {
            self.filter_input
                .update(cx, |input, cx| input.set_text("", cx));
        }
        self.query.clear();
        self.set_search_mode(false, cx);
        self.ensure_filtered(cx);
    }

    /// Input placeholders. They're set at creation, so reset them when the language or search mode
    /// changes.
    fn refresh_placeholders(&mut self, locale: &str, cx: &mut Context<Self>) {
        let filter = match self.scope() {
            scope::Scope::Here => t!("filter.placeholder", locale = locale),
            scope::Scope::Subfolders => self.search_in_name().map_or_else(
                || t!("filter.search_placeholder", locale = locale),
                |name| t!("filter.search_in_placeholder", name = name, locale = locale),
            ),
            scope::Scope::Computer => t!("filter.computer_placeholder", locale = locale),
        }
        .to_string();
        let path = t!("path.placeholder", locale = locale).to_string();
        self.filter_input
            .update(cx, |input, cx| input.set_placeholder(filter, cx));
        self.path_input
            .update(cx, |input, cx| input.set_placeholder(path, cx));
    }

    fn set_search_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.search_mode == on {
            return;
        }
        self.search_mode = on;
        self.refresh_placeholders(&rust_i18n::locale(), cx);
        if on {
            self.update_search(cx);
        } else {
            self.search.stop();
            self.search_root = None;
            self.search_in = None;
            self.search_results = None;
            self.search_gone.clear();
            self.search_cursor = None;
            // Back to "this folder": the next search starts with subfolders.
            self.search_global = false;
            self.global_task = None;
        }
        self.ensure_filtered(cx);
        self.resolve_cursor();
        cx.notify();
    }

    /// Updates the subfolder search for the current query and folder. Rewalks only when the root or
    /// hidden setting changes.
    fn update_search(&mut self, cx: &mut Context<Self>) {
        if self.search_global {
            self.run_global(cx);
            return;
        }
        if self.nothing_to_search() {
            // No query means no walk; keep an existing walk so typing again doesn't restart it.
            self.search_results = None;
            self.search_cursor = None;
            if self.search_root.is_some() {
                self.search.set_query("", crate::win::clock());
            }
            cx.notify();
            return;
        }
        let Some(root) = self
            .search_in
            .clone()
            .or_else(|| self.current_path().map(Path::to_path_buf))
        else {
            return;
        };
        let key = (root.clone(), self.show_hidden);
        if self.search_root.as_ref() == Some(&key) {
            self.search.set_query(&self.query, crate::win::clock());
        } else {
            self.search
                .start(root, self.show_hidden, &self.query, crate::win::clock());
            self.search_root = Some(key);
            self.search_gone.clear();
        }
        self.search_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.search_cursor = None;
    }

    fn handle_search(&mut self, results: SearchResults, cx: &mut Context<Self>) {
        if !self.search_mode
            || self.search_global
            || results.generation != self.search.current_gen()
        {
            return;
        }
        let results = self.without_gone(results);
        let len = results.hits.len();
        self.search_cursor = match self.search_cursor {
            _ if len == 0 => None,
            Some(ix) => Some(ix.min(len - 1)),
            None => Some(0),
        };
        self.search_results = Some(results);
        cx.notify();
    }

    /// `results` without hits that were deleted or moved away (see `search_gone`).
    pub(crate) fn without_gone(&self, mut results: SearchResults) -> SearchResults {
        if self.search_gone.is_empty() {
            return results;
        }
        let gone = |hit: &arke_core::SearchHit| {
            let path = results.root.join(&*hit.rel);
            self.search_gone
                .iter()
                .any(|g| arke_core::path::is_same_or_inside(&path, g))
        };
        if results.hits.iter().any(|hit| gone(hit)) {
            let kept: Vec<_> = results
                .hits
                .iter()
                .filter(|hit| !gone(hit))
                .cloned()
                .collect();
            let dropped = results.hits.len() - kept.len();
            results.matched = results
                .matched
                .saturating_sub(u32::try_from(dropped).unwrap_or(u32::MAX));
            results.hits = Arc::new(kept);
        }
        results
    }

    /// Items an operation deleted or moved away leave the search results now, and stay out of
    /// later results of the same walk.
    pub(crate) fn forget_search_hits(&mut self, removed: &[PathBuf], cx: &mut Context<Self>) {
        if removed.is_empty() || !self.search_mode {
            return;
        }
        self.search_gone.extend(removed.iter().cloned());
        if let Some(results) = self.search_results.take() {
            let results = self.without_gone(results);
            let len = results.hits.len();
            self.search_cursor = self
                .search_cursor
                .filter(|_| len > 0)
                .map(|ix| ix.min(len.saturating_sub(1)));
            self.search_results = Some(results);
        }
        cx.notify();
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_filter_input(window, cx);
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.set_scope(scope::Scope::Subfolders, cx);
        self.focus_filter_input(window, cx);
    }

    fn toggle_search_mode(
        &mut self,
        _: &ToggleSearchMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = if self.search_mode {
            scope::Scope::Here
        } else {
            scope::Scope::Subfolders
        };
        self.set_scope(next, cx);
        self.focus_filter_input(window, cx);
    }

    fn focus_filter_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel_dismissed = false;
        self.filter_input.update(cx, TextInput::select_all_text);
        let focus = self.filter_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Down in the filter box: focus the list and select the first item (or keep the cursor).
    fn filter_to_list(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.list_focus, cx);
        let ix = if self.search_mode {
            self.search_cursor
        } else {
            self.cursor
        }
        .unwrap_or(0);
        self.select_index(ix, cx);
    }

    // ───────────────────────── Selection and opening ─────────────────────────

    fn entry_count(&self) -> usize {
        self.model.as_ref().map_or(0, |m| m.entries.len())
    }

    /// Length of the displayed list (filter or search results).
    fn list_len(&self) -> usize {
        if self.search_mode {
            self.search_results.as_ref().map_or(0, |r| r.hits.len())
        } else {
            self.view().map_or(0, |v| v.len())
        }
    }

    const fn current_cursor(&self) -> Option<usize> {
        if self.search_mode {
            self.search_cursor
        } else {
            self.cursor
        }
    }

    pub(crate) fn select_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        // While renaming, arrow keys belong to the input.
        if self.renaming.is_some() {
            return;
        }
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let ix = ix.min(len - 1);
        if self.search_mode {
            self.search_cursor = Some(ix);
            self.search_scroll
                .scroll_to_item(ix, ScrollStrategy::Nearest);
        } else if let Some(view) = self.view() {
            self.selection.select_only(&view, ix);
            self.cursor = Some(ix);
            self.scroll_to_cursor = None;
            self.scroll_to_index(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Shift+arrow: select from the anchor to `ix`. Search results only support single selection.
    fn extend_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            return;
        }
        if self.search_mode {
            self.select_index(ix, cx);
            return;
        }
        let Some(view) = self.view() else { return };
        if view.is_empty() {
            return;
        }
        let ix = ix.min(view.len() - 1);
        self.selection.extend_to(&view, ix, false);
        self.cursor = Some(ix);
        self.scroll_to_index(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn grid_active(&self) -> bool {
        self.view_mode == ViewMode::Icons && !self.search_mode
    }

    /// Scrolls to item `ix` (converted to a row in grid view).
    pub(super) fn scroll_to_index(&self, ix: usize, strategy: ScrollStrategy) {
        let row = if self.grid_active() {
            // Grouped grid scrolls by slot.
            self.grid_groups()
                .map_or_else(|| ix / self.grid_cols.max(1), |g| g.slot_of(ix))
        } else {
            // Headers take rows when grouped.
            self.display_row(ix)
        };
        self.scroll.scroll_to_item(row, strategy);
    }

    /// The user picked a view mode: show it and remember it for this folder.
    fn pick_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_cue.fire();
        self.set_view_mode(mode, cx);
        self.remember_folder_view(cx);
    }

    /// Shows `mode` without recording it anywhere (defaults, remembered views).
    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        self.recenter_grid |= mode == ViewMode::Icons;
        if mode != ViewMode::Details {
            self.tree_clear();
        }
        if let Some(ix) = self.cursor {
            self.scroll_to_index(ix, ScrollStrategy::Center);
        } else {
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn view_details(&mut self, _: &ViewDetails, _: &mut Window, cx: &mut Context<Self>) {
        self.pick_view_mode(ViewMode::Details, cx);
    }

    fn move_left(&mut self, _: &MoveLeft, w: &mut Window, cx: &mut Context<Self>) {
        // Quick Look on a PDF turns pages first; otherwise the Details tree collapses.
        if self.turn_pdf_page(preview::PageTurn::Prev, cx) || self.tree_left(cx) {
            return;
        }
        if self.grid_active() {
            self.select_index(self.target(Step::Left, w), cx);
        } else if self.columns_active() {
            // Column view: Left goes up.
            self.go_up(&GoUp, w, cx);
        }
    }

    fn move_right(&mut self, _: &MoveRight, w: &mut Window, cx: &mut Context<Self>) {
        if self.turn_pdf_page(preview::PageTurn::Next, cx) || self.tree_right(cx) {
            return;
        }
        if self.grid_active() {
            self.select_index(self.target(Step::Right, w), cx);
        } else if self.columns_active()
            && let Some(ix) = self.current_cursor()
            && self.path_at(ix).is_some_and(|(_, is_dir)| is_dir)
        {
            // Column view: Right enters the folder.
            self.open_index(ix, cx);
        }
    }

    fn view_columns(&mut self, _: &ViewColumns, _: &mut Window, cx: &mut Context<Self>) {
        self.pick_view_mode(ViewMode::Columns, cx);
    }

    /// Height taken outside the list: top bar, context bar, column header, bottom margin and list
    /// padding.
    fn list_chrome() -> Pixels {
        theme::H_BAR + theme::H_CTX + theme::H_HEADER + theme::S_8 + theme::S_8
    }

    fn on_path_event(
        &mut self,
        _: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Confirm => self.confirm_path(window, cx),
            InputEvent::Cancel => self.stop_edit_path(window, cx),
            InputEvent::Changed => self.refresh_suggestions(cx),
        }
    }

    fn on_settings_changed(&mut self, settings: Entity<SettingsStore>, cx: &mut Context<Self>) {
        let settings = settings.read(cx).settings.clone();
        // Saved SFTP locations may sign in with a key file (settings load after startup).
        for place in &settings.remote_places {
            if let Some(url) = arke_core::remote::RemoteUrl::from_path(&place.url) {
                self.services
                    .remotes
                    .set_key_file(&url, place.key_file.clone());
            }
        }
        self.density = settings.density;
        if settings.preview_pane && !self.preview_pane {
            self.preview_cue.fire();
        }
        self.preview_pane = settings.preview_pane;
        self.show_extensions = settings.show_extensions;
        self.show_checkboxes = settings.show_checkboxes;
        self.group_rows = settings.group_rows;
        self.show_thumbnails = settings.show_thumbnails;
        self.network_thumbnails = settings.network_thumbnails;
        self.tree_setting(settings.details_tree);
        if settings.language != self.language {
            self.language = settings.language;
            // Use the language from settings rather than the global locale; main's observer may not
            // have run yet.
            self.refresh_placeholders(&settings.language.locale(), cx);
        }
        // New default sort/view: with per-folder memory on, folders without a remembered view
        // show the defaults and remembered ones keep theirs. With it off, tabs still on the old
        // defaults follow and tabs changed by hand keep theirs.
        // Folder placement always follows the setting.
        let policy = self.folder_view_settings(settings.remember_folder_views);
        let defaults = (settings.default_sort(), settings.default_view);
        if defaults != self.seen_defaults {
            use folder_view::DefaultsPolicy;
            let (old_sort, old_view) = std::mem::replace(&mut self.seen_defaults, defaults);
            let follow_sort = match policy {
                DefaultsPolicy::Follow => true,
                DefaultsPolicy::FollowUnchanged => self.sort.same_order(&old_sort),
                DefaultsPolicy::Keep => false,
            };
            let follow_view = match policy {
                DefaultsPolicy::Follow => true,
                DefaultsPolicy::FollowUnchanged => self.view_mode == old_view,
                DefaultsPolicy::Keep => false,
            };
            let mut sort = if follow_sort { defaults.0 } else { self.sort };
            sort.mixed = defaults.0.mixed;
            if sort != self.sort {
                self.apply_sort(sort, cx);
            }
            if follow_view && defaults.1 != self.view_mode {
                self.set_view_mode(defaults.1, cx);
            }
        }
        let show_hidden = settings.show_hidden;
        // Hidden rules (dotfiles, protected system files) live in core; set them and check whether
        // they changed.
        arke_core::entry::set_hidden_rules(settings.hide_dotfiles, settings.show_protected);
        let rules = arke_core::entry::hidden_rules_version();
        // Showing hidden items applies to all tabs (global, like Explorer).
        if show_hidden != self.show_hidden || rules != self.hidden_rules {
            self.hidden_rules = rules;
            self.show_hidden = show_hidden;
            self.reload(cx);
            if self.search_mode {
                self.update_search(cx);
            }
        }
        cx.notify();
    }

    /// Split panes are half width, so they use compact density.
    pub(crate) const fn row_height(&self) -> Pixels {
        if self.pane.split {
            return theme::H_ROW_COMPACT;
        }
        match self.density {
            Density::Compact => theme::H_ROW_COMPACT,
            Density::Standard => theme::H_ROW,
            Density::Comfortable => theme::H_ROW_COMFORTABLE,
        }
    }

    /// Too narrow for the Date modified column.
    pub(crate) fn narrow(&self) -> bool {
        self.pane_width < px(560.)
    }

    fn page_rows(&self, window: &Window) -> usize {
        let height: Pixels = window.viewport_size().height - Self::list_chrome();
        ((height / self.row_height()).floor() as usize).max(1)
    }

    fn target(&self, step: Step, window: &Window) -> usize {
        let cur = self.current_cursor();
        // Grid view: up/down moves a whole row; page moves a screen of rows.
        let (stride, page) = if self.grid_active() {
            let cols = self.grid_cols.max(1);
            let rows =
                ((window.viewport_size().height - Self::list_chrome()) / self.tile().h).floor();
            (cols, (rows as usize).max(1) * cols)
        } else {
            (1, self.page_rows(window))
        };
        // Grouped grid: up/down goes to the same column of the adjacent row (a section's last row
        // may be short).
        if self.grid_active()
            && let (Some(groups), Some(i)) = (self.grid_groups(), cur)
        {
            let rows = isize::try_from((page / stride).max(1)).unwrap_or(1);
            match step {
                Step::Prev => return groups.step(i, -1),
                Step::Next => return groups.step(i, 1),
                Step::PageUp => return groups.step(i, -rows),
                Step::PageDown => return groups.step(i, rows),
                _ => {}
            }
        }
        match step {
            Step::Prev => cur.map_or(0, |i| i.saturating_sub(stride)),
            Step::Next => cur.map_or(0, |i| (i + stride).min(self.list_len().saturating_sub(1))),
            Step::Left => cur.map_or(0, |i| i.saturating_sub(1)),
            Step::Right => cur.map_or(0, |i| i + 1),
            Step::PageUp => cur.map_or(0, |i| i.saturating_sub(page)),
            Step::PageDown => cur.map_or(0, |i| i + page),
            Step::First => 0,
            Step::Last => self.list_len().saturating_sub(1),
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, w: &mut Window, cx: &mut Context<Self>) {
        self.select_index(self.target(Step::Prev, w), cx);
    }
    fn select_next(&mut self, _: &SelectNext, w: &mut Window, cx: &mut Context<Self>) {
        self.select_index(self.target(Step::Next, w), cx);
    }
    fn select_page_up(&mut self, _: &SelectPageUp, w: &mut Window, cx: &mut Context<Self>) {
        if self.turn_pdf_page(preview::PageTurn::Prev, cx) {
            return;
        }
        self.select_index(self.target(Step::PageUp, w), cx);
    }
    fn select_page_down(&mut self, _: &SelectPageDown, w: &mut Window, cx: &mut Context<Self>) {
        if self.turn_pdf_page(preview::PageTurn::Next, cx) {
            return;
        }
        self.select_index(self.target(Step::PageDown, w), cx);
    }
    fn select_first(&mut self, _: &SelectFirst, w: &mut Window, cx: &mut Context<Self>) {
        if self.turn_pdf_page(preview::PageTurn::First, cx) {
            return;
        }
        self.select_index(self.target(Step::First, w), cx);
    }
    fn select_last(&mut self, _: &SelectLast, w: &mut Window, cx: &mut Context<Self>) {
        if self.turn_pdf_page(preview::PageTurn::Last, cx) {
            return;
        }
        self.select_index(self.target(Step::Last, w), cx);
    }
    fn extend_prev(&mut self, _: &ExtendPrev, w: &mut Window, cx: &mut Context<Self>) {
        self.extend_index(self.target(Step::Prev, w), cx);
    }
    fn extend_next(&mut self, _: &ExtendNext, w: &mut Window, cx: &mut Context<Self>) {
        self.extend_index(self.target(Step::Next, w), cx);
    }
    fn extend_page_up(&mut self, _: &ExtendPageUp, w: &mut Window, cx: &mut Context<Self>) {
        self.extend_index(self.target(Step::PageUp, w), cx);
    }
    fn extend_page_down(&mut self, _: &ExtendPageDown, w: &mut Window, cx: &mut Context<Self>) {
        self.extend_index(self.target(Step::PageDown, w), cx);
    }
    fn extend_first(&mut self, _: &ExtendFirst, w: &mut Window, cx: &mut Context<Self>) {
        self.extend_index(self.target(Step::First, w), cx);
    }
    fn extend_last(&mut self, _: &ExtendLast, w: &mut Window, cx: &mut Context<Self>) {
        self.extend_index(self.target(Step::Last, w), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.search_mode {
            return;
        }
        if let Some(view) = self.view() {
            self.selection.select_all(&view);
            cx.notify();
        }
    }

    fn toggle_cursor(&mut self, _: &ToggleCursor, _: &mut Window, cx: &mut Context<Self>) {
        if let (false, Some(ix), Some(view)) = (self.search_mode, self.cursor, self.view()) {
            self.selection.toggle(&view, ix);
            cx.notify();
        }
    }

    /// Esc: clears the filter first, then the selection.
    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.close_view_options(cx) {
            return;
        }
        if self.quick_look {
            self.toggle_quick_look(cx);
            return;
        }
        if !self.query.is_empty() || self.search_mode {
            self.reset_filter(cx);
            self.resolve_cursor();
        } else {
            self.selection.clear();
        }
        cx.notify();
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.current_cursor() {
            self.open_index(ix, cx);
        }
    }

    /// Ctrl+Enter: opens the folder under the cursor in a new tab.
    fn open_in_new_tab(&mut self, _: &OpenInNewTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.current_cursor()
            && let Some((path, true)) = self.path_at(ix)
        {
            cx.emit(ExplorerEvent::OpenInNewTab {
                path,
                activate: true,
            });
        }
    }

    /// Shift+Enter (search results): goes to the item's folder and selects it.
    fn open_location(&mut self, _: &OpenLocation, _: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.search_hit(self.search_cursor) else {
            return;
        };
        let Some(root) = self.search_results.as_ref().map(|r| r.root.clone()) else {
            return;
        };
        let path = root.join(&*hit.rel);
        if let Some(parent) = path.parent() {
            self.navigate_selecting(
                parent.to_path_buf(),
                NavKind::New,
                Some(hit.entry.name.clone()),
                cx,
            );
        }
    }

    fn search_hit(&self, ix: Option<usize>) -> Option<Arc<SearchHit>> {
        let results = self.search_results.as_ref()?;
        results.hits.get(ix?).cloned()
    }

    /// Full path of list item `ix` and whether it's a folder.
    fn path_at(&self, ix: usize) -> Option<(PathBuf, bool)> {
        if self.search_mode {
            let results = self.search_results.as_ref()?;
            let hit = results.hits.get(ix)?;
            Some((results.root.join(&*hit.rel), hit.entry.is_dir_like()))
        } else {
            let model = self.model.as_ref()?;
            let view = self.view()?;
            let entry = view.get(ix)?;
            Some((model.path.join(&*entry.name), entry.is_dir_like()))
        }
    }

    fn open_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some((path, is_dir)) = self.path_at(ix) else {
            return;
        };
        if is_dir {
            self.navigate(path, NavKind::New, cx);
        } else {
            self.open_file(path, cx);
        }
    }

    /// Opens with the default app. ShellExecuteExW loads third-party shell extensions, so it runs
    /// on a
    /// dedicated STA thread. Shortcuts (`.lnk`) to folders open in Arke instead of Explorer.
    fn open_file(&self, path: PathBuf, cx: &mut App) {
        // Opened files rank first among same-name results in "This PC" search.
        crate::search_history::SearchHistory::record(&path, false, cx);
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        let shortcut = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
        spawn_thread("shell-open", move || {
            if shortcut && let Some((target, true)) = shell.shortcut_target(&path) {
                let _ = tx.send(AppEvent::OpenFolder(target));
                return;
            }
            if let Err(error) = shell.open(&path) {
                let _ = tx.send(AppEvent::OpenFailed { path, error });
            }
        });
    }

    fn on_row_mouse_down(
        &mut self,
        ix: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.list_focus, cx);
        self.row_pressed = true;
        let m = event.modifiers;
        if self.search_mode {
            self.search_cursor = Some(ix);
        } else if let Some(view) = self.view() {
            let in_selection = view
                .get(ix)
                .is_some_and(|e| self.selection.contains(&e.name));
            self.click_select = None;
            if m.shift {
                self.selection.extend_to(&view, ix, m.control);
            } else if m.control {
                self.selection.toggle(&view, ix);
            } else if in_selection && self.selection.len() > 1 {
                self.click_select = Some(ix);
            } else {
                self.selection.select_only(&view, ix);
            }
            self.cursor = Some(ix);
            self.scroll_to_cursor = None;
        }
        // Double-click opens. In single-click mode, opening happens on release (`on_row_click`), so
        // don't
        // open again here: the second click would land on whatever is under the cursor in the new
        // folder.
        if event.click_count >= 2 && !m.control && !m.shift && !crate::prefs::single_click() {
            self.open_index(ix, cx);
        }
        cx.notify();
    }

    /// Single-click mode: opens on release if there was no drag and no Ctrl/Shift. Returns whether
    /// it opened.
    pub(super) fn on_row_click(
        &mut self,
        ix: usize,
        event: &gpui::ClickEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let m = event.modifiers();
        if !crate::prefs::single_click()
            || event.click_count() != 1
            || m.control
            || m.shift
            || m.alt
            || self.renaming.is_some()
        {
            return false;
        }
        self.click_select = None;
        self.open_index(ix, cx);
        true
    }

    fn on_row_middle_down(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some((path, true)) = self.path_at(ix) {
            cx.emit(ExplorerEvent::OpenInNewTab {
                path,
                activate: false,
            });
        }
    }

    /// Type-to-select in the list. Disabled while filtering or searching, where keys go to the
    /// filter box.
    fn on_list_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only when the list itself has focus; the rename input in the list takes its own keys.
        if !self.list_focus.is_focused(window) {
            return;
        }
        let ks = &event.keystroke;
        if ks.modifiers.control || ks.modifiers.alt || ks.modifiers.platform {
            return;
        }
        let Some(text) = ks.key_char.as_deref() else {
            return;
        };
        if text.is_empty() || text.chars().any(char::is_control) {
            return;
        }
        if text == " " && !self.typeahead.is_typing(Instant::now()) {
            self.toggle_quick_look(cx);
            cx.stop_propagation();
            return;
        }
        if self.search_mode || self.is_filtering() {
            return;
        }
        // The list on screen, so a match inside an expanded folder lands on its own row.
        let Some(list) = self.view() else { return };
        let found = self
            .typeahead
            .input(text, Instant::now(), &list, self.cursor);
        if let Some(ix) = found {
            self.select_index(ix, cx);
        }
        cx.stop_propagation();
    }

    // ───────────────────────── Path bar ─────────────────────────

    fn focus_path_bar(&mut self, _: &FocusPathBar, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit_path(window, cx);
    }

    fn start_edit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .display_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.editing_path = true;
        self.path_input.update(cx, |input, cx| {
            input.set_text(text, cx);
            input.select_all_text(cx);
        });
        let focus = self.path_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Click elsewhere (empty title bar, sidebar): end path editing without navigating, and release
    /// filter box focus back to the list, same as clicking the content. The filter text stays.
    pub(crate) fn cancel_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_path {
            self.stop_edit_path(window, cx);
        }
        if self
            .filter_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
        {
            window.focus(&self.list_focus, cx);
            cx.notify();
        }
    }

    fn stop_edit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_path = false;
        self.clear_suggestions();
        window.focus(&self.list_focus, cx);
        cx.notify();
    }

    fn confirm_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // An autocomplete item was picked with the arrow keys: go there.
        if let Some(target) = self.chosen_suggestion() {
            self.stop_edit_path(window, cx);
            self.navigate(target, NavKind::New, cx);
            return;
        }
        let text = self.path_input.read(cx).text().to_owned();
        self.stop_edit_path(window, cx);
        // Not a path (cmd, powershell, wt, `code .`, ...): run it in the current folder, like
        // Explorer.
        if let Some(command) = self.command_in_path_bar(&text) {
            self.run_in_current_folder(command, cx);
            return;
        }
        let expanded = self.services.shell.expand_env(&text);
        let base = self
            .display_path()
            .map_or_else(|| PathBuf::from("C:\\"), Path::to_path_buf);
        if let Some(target) = arke_core::path::resolve_input(&base, &expanded) {
            self.navigate(target, NavKind::New, cx);
        }
    }

    /// The path bar holds a command rather than a path: the first word doesn't look like a path (no
    /// `\`, `/`, `:`, `%`, `~`, no leading `.`) and no subfolder here has that name.
    fn command_in_path_bar(&self, text: &str) -> Option<String> {
        // A path pasted from "Copy as path" comes in quotes; it's still a path.
        let text = arke_core::path::unquote(text);
        let first = text.split_whitespace().next()?;
        // A quoted first word is a full program path with spaces; run it as a command.
        let pathy = !first.starts_with('"')
            && (first.contains(['\\', '/', ':', '%', '~']) || first.starts_with('.'));
        if pathy {
            return None;
        }
        let folder_named = self.model.as_ref().is_some_and(|m| {
            m.entries
                .iter()
                .any(|e| e.is_dir_like() && e.name.eq_ignore_ascii_case(text))
        });
        (!folder_named).then(|| text.to_string())
    }

    fn run_in_current_folder(&mut self, command: String, cx: &mut Context<Self>) {
        let Some(dir) = self.current_path().map(Path::to_path_buf) else {
            return;
        };
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("run-command", move || {
            if let Err(e) = shell.run_in(&command, &dir) {
                let program = command
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let _ = tx.send(AppEvent::Notice(Err(t!(
                    "error.command_failed",
                    name = program,
                    error = e
                )
                .into_owned())));
            }
        });
        cx.notify();
    }

    /// Whether items in `dir` may be read for thumbnails and exe icons: yes on local disks, on
    /// network
    /// locations only if the setting is on. Folder sizes ignore this and are never computed on
    /// network locations.
    pub(super) fn media_ok(&self, dir: &Path, cx: &mut Context<Self>) -> bool {
        // Thumbnails and file icons would download every file from the server.
        if arke_core::remote::is_remote(dir) {
            return false;
        }
        let network = self.network_thumbnails;
        self.media.update(cx, |m, _| {
            m.media_allowed(dir) || (network && m.known_remote(dir))
        })
    }

    // ───────────────────────── Per frame ─────────────────────────

    fn on_frame(&mut self, window: &mut Window, cx: &App) {
        let title = match self.display_path() {
            Some(_) => format!("{} - Arke", self.tab_title()),
            None => "Arke".into(),
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let since_start = self.started_at.elapsed();
        if self.metrics.first_frame.is_none() {
            self.metrics.first_frame = Some(since_start);
            log::info!(
                "cold start: first frame {:.1} ms",
                since_start.as_secs_f64() * 1e3
            );
        }
        let listing_ready = self.metrics.listing_ready && self.metrics.first_listing.is_none();
        if listing_ready {
            self.metrics.first_listing = Some(since_start);
            log::info!(
                "cold start: first full listing {:.1} ms ({} items)",
                since_start.as_secs_f64() * 1e3,
                self.entry_count()
            );
        }

        // Scroll metrics count rows; a grid row holds several items.
        let count = if self.grid_active() {
            self.entry_count().div_ceil(self.grid_cols.max(1))
        } else {
            self.entry_count()
        };
        let ready = self.metrics.first_listing.is_some();
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        match bench.on_frame(ready, count) {
            BenchStep::Continue => {}
            BenchStep::ScrollTo(row) => self.scroll.scroll_to_item_strict(row, ScrollStrategy::Top),
            BenchStep::Finish => {
                let header = format!(
                    "{}
media_loaded: {}",
                    self.bench_header(),
                    self.media.read(cx).loaded_count()
                );
                if let Some(bench) = &self.bench {
                    bench.write_report(&header);
                }
                // Close only this window. In background mode the process stays; otherwise closing
                // the last window exits.
                window.remove_window();
            }
        }
        window.request_animation_frame();
    }

    fn bench_header(&self) -> String {
        let ms = |d: Option<Duration>| d.map_or(-1.0, |d| d.as_secs_f64() * 1e3);
        // Wall clock (Unix ms) so external scripts can compare Arke and other file managers on the
        // same scale.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let elapsed = self.started_at.elapsed();
        let unix_ms = |d: Option<Duration>| {
            d.map_or(-1.0, |d| {
                (now_unix.saturating_sub(elapsed.saturating_sub(d))).as_secs_f64() * 1e3
            })
        };
        let timing = self.last_timing.unwrap_or_default();
        format!(
            "== arke bench {}\npath: {}\nentries: {}\nfirst_frame_ms: {:.1}\nfirst_listing_ms: {:.1}\nlist_enumerate_ms: {:.1}\nlist_sort_ms: {:.1}\nlist_total_ms: {:.1}\nfirst_frame_unix_ms: {:.1}\nfirst_listing_unix_ms: {:.1}",
            crate::build_info::VERSION,
            self.current_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self.entry_count(),
            ms(self.metrics.first_frame),
            ms(self.metrics.first_listing),
            timing.enumerate.as_secs_f64() * 1e3,
            timing.sort.as_secs_f64() * 1e3,
            timing.total.as_secs_f64() * 1e3,
            unix_ms(self.metrics.first_frame),
            unix_ms(self.metrics.first_listing),
        )
    }
}

#[derive(Clone, Copy)]
enum Step {
    Prev,
    Next,
    Left,
    Right,
    PageUp,
    PageDown,
    First,
    Last,
}

impl Focusable for Explorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.list_focus.clone()
    }
}

impl Drop for Explorer {
    fn drop(&mut self) {
        self.dir_service.cancel();
        self.search.stop();
    }
}

/// Tab title for a path: its last component, the share name of a UNC root
/// (`\\wsl.localhost\Ubuntu` -> `Ubuntu`), or the whole path for drive roots.
fn tab_title_for(path: &Path) -> String {
    if let Some(name) = path.file_name() {
        return name.to_string_lossy().into_owned();
    }
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next()
        && let std::path::Prefix::UNC(_, share) | std::path::Prefix::VerbatimUNC(_, share) =
            prefix.kind()
    {
        return share.to_string_lossy().into_owned();
    }
    path.display().to_string()
}

#[cfg(test)]
mod tab_title_tests {
    use super::tab_title_for;
    use std::path::Path;

    #[test]
    fn tab_titles_name_the_folder_share_or_drive() {
        assert_eq!(tab_title_for(Path::new(r"C:\Users\me")), "me");
        assert_eq!(tab_title_for(Path::new(r"C:\")), r"C:\");
        assert_eq!(
            tab_title_for(Path::new(r"\\wsl.localhost\Ubuntu-24.04")),
            "Ubuntu-24.04"
        );
        assert_eq!(tab_title_for(Path::new(r"\\nas\docs\")), "docs");
        assert_eq!(tab_title_for(Path::new(r"\\nas\docs\2026")), "2026");
    }
}
