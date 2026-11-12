//! Context menu, compression and extraction.
//!
//! The menu is plain GPUI and draws on the same frame as the right-click, without waiting for
//! the Shell. In parallel a separate process (`shell_host.rs`, the only place third-party shell
//! extensions get loaded) reads the Windows context menu. Its items land in the "Show more
//! options" submenu with icons and nested submenus, and a chosen item is sent back to that
//! process to run. No native Windows menu is ever shown.
//!
//! Extraction runs on a background thread (`arke_core::archive`) with cancellable progress in the
//! status bar. The extracted folder is selected when done.

use rust_i18n::t;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use arke_core::archive::{
    self, ExtractOptions, ExtractProgress, ExtractSummary, archive_kind, folder_name_for,
};
use gpui::{
    Anchor, AnimationExt, AnyElement, App, Bounds, ClickEvent, Context, ElementId, FocusHandle,
    InteractiveElement, IntoElement, KeyBinding, MouseDownEvent, ParentElement, Pixels, Point,
    RenderImage, SharedString, StatefulInteractiveElement, Styled, Task, Window, actions, anchored,
    canvas, deferred, div, img, point, prelude::FluentBuilder, px, svg,
};

use super::ops::{CopyFiles, CutFiles, DeleteFiles, NewFolder, PasteFiles, RenameFile};
use super::{AppEvent, Explorer, ExplorerEvent, Refresh, ViewDetails, ViewMode, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::settings::SettingsStore;
use crate::shell::{ShellMenu, ShellMenuEntry};
use crate::theme::{self, UiFont as _};

actions!(
    explorer,
    [
        OpenContextMenu,
        MenuUp,
        MenuDown,
        MenuLeft,
        MenuRight,
        MenuConfirm,
        MenuDismiss
    ]
);

pub(crate) fn bind_keys(cx: &mut App) {
    let menu = Some("ContextMenu");
    cx.bind_keys([
        KeyBinding::new("shift-f10", OpenContextMenu, Some("FileList")),
        KeyBinding::new("up", MenuUp, menu),
        KeyBinding::new("down", MenuDown, menu),
        KeyBinding::new("left", MenuLeft, menu),
        KeyBinding::new("right", MenuRight, menu),
        KeyBinding::new("enter", MenuConfirm, menu),
        KeyBinding::new("escape", MenuDismiss, menu),
    ]);
}

/// Hover delay before a submenu opens or switches, so moving diagonally into a submenu doesn't
/// close it.
const SUBMENU_DELAY: Duration = Duration::from_millis(180);
/// Row ids for submenu alignment are `level * LEVEL_STRIDE + row`.
const LEVEL_STRIDE: usize = 1000;
/// Menu entrance and exit: slide this far (see `motion.rs`).
const MENU_SLIDE: f32 = crate::motion::SLIDE;
static NEXT_MENU: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuCommand {
    /// Alt-drop menu: move, copy, create shortcut, cancel.
    DropMove,
    DropCopy,
    DropShortcut,
    DropCancel,
    Open,
    OpenInNewTab,
    ExtractToFolder,
    ExtractHere,
    Cut,
    Copy,
    Paste,
    Rename,
    Delete,
    CopyPath,
    /// Details tree: the item's path below the folder shown (`src\ui\view.rs`).
    CopyRelativePath,
    NewFolder,
    NewTextFile,
    /// Folder item: a new folder inside it.
    NewFolderIn,
    /// Folder item: a new file inside it, its whole name ready to type.
    NewFileIn,
    /// Folder item: a subfolder search rooted at it, like VS Code's "Find in Folder".
    SearchInFolder,
    /// Undo the last file operation (Ctrl+Z).
    Undo,
    /// Redo the last undone file operation (Ctrl+Y).
    Redo,
    /// Open in terminal (the selected folder, or the current one).
    OpenTerminal,
    /// Compress the selection into one zip.
    Compress,
    /// Move the selection into a new folder.
    MoveToNewFolder,
    /// Use this image as the Arke background.
    SetBackground,
    /// Import this `.json` as an Arke settings backup.
    ImportSettings,
    /// Path bar: copy the address as plain text, without quotes.
    CopyAddress,
    /// Path bar: switch to typing a path.
    EditAddress,
    /// Path bar: clear visited-folder history (used by Ctrl+T and path autocomplete).
    ClearHistory,
    Refresh,
    ViewDetails,
    /// Icons view at the folder's icon size.
    ViewIcons,
    ViewColumns,
    RevealInExplorer,
    MoreOptions,
    /// Pin or unpin (the current folder if nothing is selected). Goes to the tiles if one is free,
    /// else the list below.
    TogglePin,
    /// Open this folder in a new window.
    OpenInNewWindow,
    /// Windows Properties dialog, opened in the menu process so third-party property sheets stay
    /// out of Arke.
    Properties,
    /// Open with the nth app found on this PC (VS Code, Zed, ...; see [`set_open_with`]).
    OpenWith(usize),
    /// Windows menu item read for "Show more options".
    Shell(u32),
    /// Cloud files: toggle "Always keep on this device".
    KeepOnDevice,
    /// Windows "Open with" dialog.
    ChooseApp,
    /// Shortcut: go to its target and select it.
    OpenFileLocation,
    /// Symlink or junction: go to the folder its target is in and select the target.
    GoToLinkTarget,
    /// Column header menu: show or hide a Details column.
    ToggleColumn(crate::prefs::Col),
    /// Column header menu: put the columns back in their default order.
    ResetColumnOrder,
    /// Folder on the network: add to or remove from whole-PC search. Local folder: exclude or
    /// re-include.
    ToggleSearchNetwork,
    /// Cloud files: free up space (online-only).
    FreeUpSpace,
    /// Put the targets on the shelf.
    AddToShelf,
}

/// Apps offered under "Open with" (name, exe). Discovered once in the background at startup.
static OPEN_WITH: std::sync::OnceLock<Vec<(String, std::path::PathBuf)>> =
    std::sync::OnceLock::new();

/// Icons extracted from those exes, not yet converted to renderable images. Converted on the UI
/// thread on first use.
static OPEN_WITH_ICON_DATA: Mutex<Vec<Option<arke_core::thumbs::Thumbnail>>> =
    Mutex::new(Vec::new());

thread_local! {
    static OPEN_WITH_ICONS: std::cell::RefCell<Option<Vec<Option<Arc<RenderImage>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// `icons` matches `apps` by index (read from the exes on a background thread).
pub(crate) fn set_open_with(
    apps: Vec<(String, std::path::PathBuf)>,
    icons: Vec<Option<arke_core::thumbs::Thumbnail>>,
) {
    *OPEN_WITH_ICON_DATA.lock() = icons;
    let _ = OPEN_WITH.set(apps);
}

/// Icon of app `ix`.
pub(crate) fn open_with_icon(ix: usize) -> Option<Arc<RenderImage>> {
    // Don't convert until discovery is done; the conversion result is final.
    OPEN_WITH.get()?;
    OPEN_WITH_ICONS.with_borrow_mut(|cache| {
        let icons = cache.get_or_insert_with(|| {
            let data = std::mem::take(&mut *OPEN_WITH_ICON_DATA.lock());
            data.into_iter()
                .map(|t| t.and_then(crate::media::to_render_image))
                .collect()
        });
        icons.get(ix).cloned().flatten()
    })
}

pub(crate) fn open_with_apps() -> &'static [(String, std::path::PathBuf)] {
    OPEN_WITH.get().map_or(&[], Vec::as_slice)
}

/// Apps actually listed under "Open with": discovered editors not disabled in settings (with their
/// own icons), plus user-added ones.
pub(crate) struct OpenWithApp {
    pub(crate) name: String,
    pub(crate) exe: PathBuf,
    pub(crate) icon: Option<Arc<RenderImage>>,
}

pub(crate) fn open_with_list(settings: &crate::settings::Settings) -> Vec<OpenWithApp> {
    let found = open_with_apps()
        .iter()
        .enumerate()
        .filter(|(_, (_, exe))| !settings.open_with_hidden.contains(exe))
        .map(|(ix, (name, exe))| OpenWithApp {
            name: name.clone(),
            exe: exe.clone(),
            icon: open_with_icon(ix),
        });
    let extra = settings.open_with_extra.iter().map(|exe| OpenWithApp {
        name: app_name(exe),
        exe: exe.clone(),
        icon: None,
    });
    found.chain(extra).collect()
}

/// Editors found on this PC (name, exe, icon), listed in settings so each can be turned off.
pub(crate) fn menu_open_with_found() -> Vec<(String, PathBuf, Option<Arc<RenderImage>>)> {
    open_with_apps()
        .iter()
        .enumerate()
        .map(|(ix, (name, exe))| (name.clone(), exe.clone(), open_with_icon(ix)))
        .collect()
}

/// Display name for a user-added app: the exe file stem.
pub(crate) fn app_name(exe: &std::path::Path) -> String {
    exe.file_stem().map_or_else(
        || exe.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Icons for Arke's own commands (every item has one, like the Windows 11 menu).
const fn glyph_for(command: MenuCommand) -> Option<Icon> {
    use MenuCommand as C;
    Some(match command {
        C::Open => Icon::Open,
        C::OpenInNewTab => Icon::TabOpen,
        C::OpenInNewWindow | C::DropShortcut => Icon::External,
        C::Cut | C::DropMove => Icon::Cut,
        C::Copy | C::CopyAddress | C::DropCopy => Icon::Copy,
        C::Paste => Icon::Paste,
        C::Rename | C::EditAddress => Icon::Rename,
        C::Delete | C::ClearHistory => Icon::Trash,
        C::CopyPath | C::CopyRelativePath => Icon::CopyPath,
        C::NewFolder | C::NewFolderIn | C::MoveToNewFolder => Icon::FolderPlus,
        C::Undo => Icon::Undo,
        C::Redo => Icon::Redo,
        C::OpenTerminal => Icon::Terminal,
        C::Compress => Icon::Zip,
        C::Refresh => Icon::Refresh,
        C::ViewDetails => Icon::ViewList,
        C::ViewIcons => Icon::ViewGrid,
        C::ViewColumns => Icon::ViewColumns,
        C::RevealInExplorer => Icon::Explorer,
        C::TogglePin => Icon::Pin,
        C::NewTextFile | C::NewFileIn => Icon::Plus,
        C::Properties => Icon::Info,
        C::OpenWith(_) | C::ChooseApp => Icon::OpenWith,
        C::ImportSettings => Icon::Settings,
        C::SetBackground => Icon::Image,
        C::KeepOnDevice => Icon::Drive,
        C::OpenFileLocation | C::GoToLinkTarget => Icon::Folder,
        C::ToggleSearchNetwork | C::SearchInFolder => Icon::Search,
        C::FreeUpSpace => Icon::Cloud,
        C::AddToShelf => Icon::Shelf,
        _ => return None,
    })
}

/// Adds icons to Arke's own items; items read from Windows bring their own.
fn decorate(entries: &mut [MenuEntry]) {
    for entry in entries {
        match entry {
            MenuEntry::Item(item) if item.icon.is_none() => item.glyph = glyph_for(item.command),
            MenuEntry::Actions(items) => {
                for item in items {
                    item.glyph = glyph_for(item.command);
                }
            }
            _ => {}
        }
    }
}

// Each bool is an independent row style (enabled, checked, bold, danger); an enum would read worse.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone)]
pub(crate) struct MenuItem {
    pub(crate) label: SharedString,
    shortcut: Option<&'static str>,
    pub(crate) command: MenuCommand,
    pub(crate) enabled: bool,
    icon: Option<Arc<RenderImage>>,
    /// Arke's own icon (`icon` is the one carried by Windows menu items).
    glyph: Option<Icon>,
    checked: bool,
    /// Default command, drawn bold.
    bold: bool,
    /// Error message, drawn in the danger color rather than the faint disabled gray.
    danger: bool,
    /// Windows menu items: key used to pin it to the main menu (see [`pin_key`]).
    pin: Option<String>,
}

pub(crate) enum MenuEntry {
    Item(MenuItem),
    Submenu {
        label: SharedString,
        enabled: bool,
        icon: Option<Arc<RenderImage>>,
        children: Vec<Self>,
        /// "Show more options" and the submenus read from Windows inside it; `false` for Arke's
        /// own View submenu.
        shell: bool,
    },
    /// Row of icon buttons (cut, copy, rename, delete) at the bottom, as in the Windows 11 menu.
    Actions(Vec<MenuItem>),
    Separator,
}

impl MenuEntry {
    const fn selectable(&self) -> bool {
        match self {
            Self::Item(i) => i.enabled,
            Self::Submenu { enabled, .. } => *enabled,
            // Icon buttons are mouse-only; the keyboard has Ctrl+X, Ctrl+C, F2 and Delete.
            Self::Actions(_) | Self::Separator => false,
        }
    }

    const fn has_mark(&self) -> bool {
        match self {
            Self::Item(i) => i.icon.is_some() || i.glyph.is_some() || i.checked,
            Self::Submenu { icon, .. } => icon.is_some(),
            Self::Actions(_) | Self::Separator => false,
        }
    }

    /// Converts a Windows menu entry (icons become GPUI images). `parent` holds enclosing submenu
    /// names, for the pin key.
    fn from_shell(entry: ShellMenuEntry, parent: &[&str]) -> Self {
        let icon =
            |t: Option<arke_core::thumbs::Thumbnail>| t.and_then(crate::media::to_render_image);
        match entry {
            ShellMenuEntry::Item {
                id,
                label,
                enabled,
                checked,
                default,
                icon: i,
                ..
            } => {
                let mut path = parent.to_vec();
                path.push(&label);
                let pin = Some(pin_key(&path));
                Self::Item(MenuItem {
                    label: label.into(),
                    shortcut: None,
                    command: MenuCommand::Shell(id),
                    enabled,
                    icon: icon(i),
                    glyph: None,
                    checked,
                    bold: default,
                    danger: false,
                    pin,
                })
            }
            ShellMenuEntry::Submenu {
                label,
                enabled,
                icon: i,
                children,
            } => {
                let mut path = parent.to_vec();
                path.push(&label);
                let children = children
                    .into_iter()
                    .map(|c| Self::from_shell(c, &path))
                    .collect();
                Self::Submenu {
                    label: label.into(),
                    enabled,
                    icon: icon(i),
                    children,
                    shell: true,
                }
            }
            ShellMenuEntry::Separator => Self::Separator,
        }
    }
}

const fn plain(
    label: SharedString,
    shortcut: Option<&'static str>,
    command: MenuCommand,
    enabled: bool,
) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        label,
        shortcut,
        command,
        enabled,
        icon: None,
        glyph: None,
        checked: false,
        bold: false,
        danger: false,
        pin: None,
    })
}

/// One button in the icon row.
fn action(label: impl Into<SharedString>, command: MenuCommand, enabled: bool) -> MenuItem {
    MenuItem {
        label: label.into(),
        shortcut: None,
        command,
        enabled,
        icon: None,
        glyph: None,
        checked: false,
        bold: false,
        danger: false,
        pin: None,
    }
}

fn item(
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    command: MenuCommand,
) -> MenuEntry {
    plain(label.into(), shortcut, command, true)
}

/// Row with a check mark when `checked` (the current view, an option that is on).
fn checked_item(
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    command: MenuCommand,
    checked: bool,
) -> MenuEntry {
    match plain(label.into(), shortcut, command, true) {
        MenuEntry::Item(item) => MenuEntry::Item(MenuItem { checked, ..item }),
        other => other,
    }
}

/// Disabled text-only row ("Loading...", "No other options").
fn placeholder(label: impl Into<SharedString>) -> MenuEntry {
    plain(label.into(), None, MenuCommand::MoreOptions, false)
}

/// Error row, e.g. failed to load. Not clickable, but drawn in the danger color since disabled gray
/// is too faint.
fn error_line(label: impl Into<SharedString>) -> MenuEntry {
    match plain(label.into(), None, MenuCommand::MoreOptions, false) {
        MenuEntry::Item(item) => MenuEntry::Item(MenuItem {
            danger: true,
            ..item
        }),
        other => other,
    }
}

fn item_if(
    enabled: bool,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    command: MenuCommand,
) -> MenuEntry {
    plain(label.into(), shortcut, command, enabled)
}

/// Positions known only after paint: bounds of every row and menu level in window coordinates.
/// Used for submenu alignment and click-outside dismissal.
#[derive(Default)]
struct MenuGeometry {
    rows: HashMap<usize, Bounds<Pixels>>,
    panels: HashMap<usize, Bounds<Pixels>>,
    /// Levels whose submenu opens to the left (their chevron points left).
    opens_left: std::collections::HashSet<usize>,
}

pub(crate) struct ContextMenu {
    position: Point<Pixels>,
    pub(crate) entries: Vec<MenuEntry>,
    /// Open submenus: the parent row index at each level.
    open: Vec<usize>,
    /// Keyboard-highlighted row in the innermost level.
    highlighted: Option<usize>,
    focus: FocusHandle,
    /// Menu targets and whether each is a folder (empty means the folder background).
    targets: Vec<(PathBuf, bool)>,
    /// "Show more options": Windows menu still held by the menu process.
    shell_session: Option<u64>,
    /// "Show more options" still loading: id of this request.
    shell_request: Option<u64>,
    /// Top-level row index of the "Show more options" submenu.
    shell_slot: Option<usize>,
    /// Number of pinned Windows commands placed directly above "Show more options".
    pinned_rows: usize,
    /// Differs per menu open; used as the animation id so only a new menu replays the entrance.
    serial: u64,
    /// Last input was keyboard: when submenu items arrive late, keyboard users get the first item
    /// selected.
    keyboard: bool,
    geometry: Rc<RefCell<MenuGeometry>>,
    hover_task: Option<Task<()>>,
}

impl ContextMenu {
    fn new(
        position: Point<Pixels>,
        mut entries: Vec<MenuEntry>,
        focus: FocusHandle,
        targets: Vec<(PathBuf, bool)>,
    ) -> Self {
        // Every menu variant (single, multi, background) gets icons.
        decorate(&mut entries);
        Self {
            position,
            entries,
            open: Vec::new(),
            highlighted: None,
            focus,
            targets,
            shell_session: None,
            shell_request: None,
            shell_slot: None,
            pinned_rows: 0,
            serial: NEXT_MENU.fetch_add(1, Ordering::Relaxed),
            keyboard: false,
            geometry: Rc::default(),
            hover_task: None,
        }
    }

    /// Entries at `level` (0 is the top level).
    fn level(&self, level: usize) -> &[MenuEntry] {
        let mut entries = self.entries.as_slice();
        for &ix in self.open.iter().take(level) {
            match entries.get(ix) {
                Some(MenuEntry::Submenu { children, .. }) => entries = children,
                _ => return &[],
            }
        }
        entries
    }

    /// Places pinned Windows commands directly above "Show more options", in pin order, skipping
    /// ones
    /// not in this menu. Previously placed rows are removed first, so calling again after a pin
    /// change
    /// is enough. Open submenus and the highlighted row shift accordingly.
    fn sync_pinned(&mut self, pinned: &[String]) {
        let Some(slot) = self.shell_slot else {
            return;
        };
        let start = slot - self.pinned_rows;
        let mut found: Vec<MenuItem> = Vec::new();
        if let Some(MenuEntry::Submenu { children, .. }) = self.entries.get(slot) {
            for key in pinned {
                if let Some(item) = find_pinned(children, key) {
                    found.push(item.clone());
                }
            }
        }
        let added = found.len();
        self.entries
            .splice(start..slot, found.into_iter().map(MenuEntry::Item));
        let new_slot = start + added;
        let shift = |ix: usize| {
            if ix >= slot {
                ix + new_slot - slot
            } else if ix >= start {
                // Highlight was on a replaced pinned row: move it to the first pinned row (or "Show
                // more options").
                start
            } else {
                ix
            }
        };
        if let Some(first) = self.open.first_mut() {
            *first = shift(*first);
        }
        if self.open.is_empty() {
            self.highlighted = self.highlighted.map(shift);
        }
        self.shell_slot = Some(new_slot);
        self.pinned_rows = added;
    }

    /// Test helper: checked items at the top level.
    #[cfg(test)]
    pub(crate) fn checked_labels(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) if i.checked => Some(i.label.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Test helper: labels at the top level (separators as `—`).
    #[cfg(test)]
    pub(crate) fn labels(&self) -> Vec<String> {
        self.labels_at(0)
    }

    /// Test helper: labels at `level`.
    #[cfg(test)]
    pub(crate) fn labels_at(&self, level: usize) -> Vec<String> {
        self.level(level)
            .iter()
            .map(|e| match e {
                MenuEntry::Item(i) => i.label.to_string(),
                MenuEntry::Submenu { label, .. } => format!("{label} ›"),
                MenuEntry::Actions(items) => format!(
                    "[{}]",
                    items
                        .iter()
                        .map(|i| i.label.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                MenuEntry::Separator => "—".into(),
            })
            .collect()
    }

    /// Test helper: whether "Show more options" has loaded, and the menu process session.
    #[cfg(test)]
    pub(crate) const fn shell_state(&self) -> (bool, Option<u64>) {
        (self.shell_request.is_none(), self.shell_session)
    }

    /// Test helper: number of open submenu levels.
    #[cfg(test)]
    pub(crate) const fn depth(&self) -> usize {
        self.open.len()
    }
}

pub(crate) struct ExtractJob {
    pub(crate) name: String,
    /// Compressing rather than extracting; changes the banner text.
    pub(crate) compress: bool,
    /// Progress center job. Cancelling sets its `cancel`; the progress center and banner share one
    /// cancel button.
    job: u64,
    ops: Arc<crate::shell::OpProgress>,
    pub(crate) progress: ExtractProgress,
}

/// Writes background compress/extract progress (items, bytes) into the progress center entry.
fn report(ops: &crate::shell::OpProgress, p: ExtractProgress) {
    ops.done.store(p.files, Ordering::Relaxed);
    ops.work_done.store(p.done, Ordering::Relaxed);
    ops.work_total.store(p.total, Ordering::Relaxed);
}

/// Maps a compress/extract result to a progress center outcome.
fn outcome(
    result: Result<bool, &archive::ArchiveError>,
) -> Result<crate::shell::OpOutcome, &archive::ArchiveError> {
    result.map(|cancelled| {
        if cancelled {
            crate::shell::OpOutcome::Cancelled
        } else {
            crate::shell::OpOutcome::Done
        }
    })
}

impl Explorer {
    // ───────────────────────── Opening and building the menu ─────────────────────────

    /// Right-click on an item: if it isn't selected, select only it (like Explorer), then open the
    /// menu.
    pub(crate) fn on_row_right_down(
        &mut self,
        ix: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.renaming.is_some() {
            return;
        }
        window.focus(&self.list_focus, cx);
        if self.search_mode {
            self.search_cursor = Some(ix);
        } else if let Some(view) = self.view()
            && let Some(entry) = view.get(ix)
        {
            if !self.selection.contains(&entry.name) {
                self.selection.select_only(&view, ix);
            }
            self.cursor = Some(ix);
        }
        // The menu opens on release (`right_press_released`), so the press can still become a
        // right-button drag.
        self.note_right_press(event.position, cx);
    }

    /// Right-click on empty space: clear the selection and show the folder menu.
    pub(crate) fn on_list_right_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() || self.model.is_none() {
            return;
        }
        if !self.search_mode {
            self.selection.clear();
        }
        self.open_context_menu(event.position, window, cx);
        // The list is focusable. Without this GPUI hands focus back to the list afterwards and the
        // menu
        // never sees Esc or arrow keys.
        window.prevent_default();
    }

    /// Shift+F10: opens the menu from the keyboard, a third of the way down the list.
    pub(super) fn open_context_menu_action(
        &mut self,
        _: &OpenContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let size = window.viewport_size();
        self.open_context_menu(point(size.width / 2., size.height / 3.), window, cx);
    }

    /// For targets in cloud folders like OneDrive: whether all are "Always keep on this device".
    /// `None` if any isn't in a cloud folder, or for search results.
    fn cloud_state(&self, targets: &[(PathBuf, bool)]) -> Option<bool> {
        if self.search_mode || targets.is_empty() || !targets.iter().all(|(p, _)| self.in_cloud(p))
        {
            return None;
        }
        let view = self.view()?;
        Some(
            view.iter()
                .filter(|e| self.selection.contains(&e.name))
                .all(arke_core::Entry::is_cloud_pinned),
        )
    }

    fn menu_targets(&self) -> Vec<(PathBuf, bool)> {
        if self.search_mode {
            return self
                .search_hit(self.search_cursor)
                .zip(self.search_results.as_ref())
                .map(|(hit, r)| vec![(r.root.join(&*hit.rel), hit.entry.is_dir_like())])
                .unwrap_or_default();
        }
        let (Some(model), Some(view)) = (self.model.as_ref(), self.view()) else {
            return Vec::new();
        };
        view.iter()
            .filter(|e| self.selection.contains(&e.name))
            .map(|e| (model.path.join(&*e.name), e.is_dir_like()))
            .collect()
    }

    pub(crate) fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.menu_targets();
        let pinned = {
            let store = SettingsStore::global(cx);
            let settings = &store.read(cx).settings;
            match targets.first() {
                Some((path, _)) => settings.is_pinned(path, &self.places),
                None => self
                    .current_path()
                    .is_some_and(|p| settings.is_pinned(p, &self.places)),
            }
        };
        // Background menu: "Undo <operation>" names the operation it would undo.
        let undo = crate::explorer::UndoHistory::last(cx)
            .map(|e| t!("menu.undo_op", op = t!(e.verb)).into_owned());
        let redo = crate::explorer::RedoStack::last(cx)
            .map(|e| t!("menu.redo_op", op = t!(e.verb)).into_owned());
        let hidden = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_hidden
            .clone();
        let apps = open_with_list(&SettingsStore::global(cx).read(cx).settings);
        let cloud = self.cloud_state(&targets);
        // A single folder can be added to or removed from whole-PC search (network: add, local:
        // exclude).
        let net_search = match targets.as_slice() {
            [(path, true)] => {
                let st = &SettingsStore::global(cx).read(cx).settings;
                if crate::global_search::is_network_path(path) {
                    Some(crate::global_search::is_searched(st, path))
                } else {
                    crate::global_search::local_search_state(st, path)
                }
            }
            _ => None,
        };
        let mut entries = self.build_menu(&targets, pinned, (undo, redo), &apps, net_search);
        if let Some(kept) = cloud {
            add_cloud_items(&mut entries, kept);
        }
        let entries = hide_entries(entries, &hidden);
        let on_server = targets.first().map_or_else(
            || self.on_remote(),
            |(p, _)| arke_core::remote::is_remote(p),
        );
        let entries = if on_server {
            remote_menu(entries)
        } else {
            entries
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        let mut menu = ContextMenu::new(position, entries, focus, targets);
        menu.shell_slot = menu
            .entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Submenu { shell: true, .. }));
        if menu.shell_slot.is_some() && self.services.shell.shell_menu_prefetch() {
            self.request_shell_menu(&mut menu);
        }
        self.context_menu = Some(menu);
        cx.notify();
    }

    /// Background menu "View" submenu: the three layouts, the current one checked, as in
    /// Explorer's and Files' menus.
    fn view_submenu(&self) -> MenuEntry {
        use MenuCommand as C;
        let mode = self.view_mode;
        MenuEntry::Submenu {
            label: t!("menu.view").into(),
            enabled: true,
            icon: None,
            children: vec![
                checked_item(
                    t!("menu.view_details"),
                    Some("Ctrl+Shift+6"),
                    C::ViewDetails,
                    mode == ViewMode::Details,
                ),
                checked_item(
                    t!("view.icons"),
                    None,
                    C::ViewIcons,
                    mode == ViewMode::Icons,
                ),
                checked_item(
                    t!("settings.view_columns"),
                    None,
                    C::ViewColumns,
                    mode == ViewMode::Columns,
                ),
            ],
            shell: false,
        }
    }

    // One section per menu kind (background, single, multi), listed top to bottom as on screen.
    #[allow(clippy::too_many_lines)]
    fn build_menu(
        &self,
        targets: &[(PathBuf, bool)],
        pinned: bool,
        (undo, redo): (Option<String>, Option<String>),
        apps: &[OpenWithApp],
        net_search: Option<bool>,
    ) -> Vec<MenuEntry> {
        use MenuCommand as C;
        if targets.is_empty() {
            let mut entries = vec![
                item(t!("menu.new_folder"), Some("Ctrl+Shift+N"), C::NewFolder),
                item(t!("menu.new_text_file"), None, C::NewTextFile),
                item_if(
                    self.services.shell.clipboard_has_files(),
                    t!("menu.paste"),
                    Some("Ctrl+V"),
                    C::Paste,
                ),
                item_if(
                    undo.is_some(),
                    undo.unwrap_or_else(|| t!("menu.undo").into_owned()),
                    Some("Ctrl+Z"),
                    C::Undo,
                ),
                MenuEntry::Separator,
                // Icon size, grouping and the other view switches are in the view options flyout.
                self.view_submenu(),
                item(t!("menu.refresh"), Some("F5"), C::Refresh),
            ];
            entries.extend([
                MenuEntry::Separator,
                item(t!("menu.copy_path"), None, C::CopyPath),
                item(t!("menu.open_in_explorer"), None, C::RevealInExplorer),
                item(t!("menu.open_terminal"), None, C::OpenTerminal),
                MenuEntry::Separator,
                item(pin_label(pinned), None, C::TogglePin),
                MenuEntry::Separator,
                // Windows background menu: New (file types), Open in Terminal, third-party items.
                more_options(),
            ]);
            // Redo, right under Undo, only when there is something to redo.
            if let Some(label) = redo {
                let at = entries
                    .iter()
                    .position(|e| matches!(e, MenuEntry::Separator))
                    .unwrap_or(entries.len());
                entries.insert(at, item(label, Some("Ctrl+Y"), C::Redo));
            }
            return entries;
        }
        let single = targets.len() == 1;
        let (path, is_dir) = &targets[0];
        let mut entries = vec![item(t!("menu.open"), Some("Enter"), C::Open)];
        if single && *is_dir {
            entries.push(item(
                t!("menu.open_in_new_tab"),
                Some("Ctrl+Enter"),
                C::OpenInNewTab,
            ));
            // Inside this folder, as at the top of VS Code's explorer menu. A server's shares are
            // only listed, so nothing can be made in `\\server` itself.
            entries.push(MenuEntry::Separator);
            if arke_core::net::bare_server(path).is_none() {
                entries.push(item(t!("menu.new_file_in"), None, C::NewFileIn));
                entries.push(item(t!("menu.new_folder_in"), None, C::NewFolderIn));
            }
            entries.push(item(t!("menu.search_in_folder"), None, C::SearchInFolder));
            entries.push(MenuEntry::Separator);
            entries.push(item(pin_label(pinned), None, C::TogglePin));
            // Network folders can be added to whole-PC search on their own, so large shares needn't
            // be indexed whole.
            if let Some(searched) = net_search {
                entries.push(item(
                    if searched {
                        t!("sidebar.unsearch_network")
                    } else {
                        t!("sidebar.search_network")
                    },
                    None,
                    C::ToggleSearchNetwork,
                ));
            }
            entries.push(item(t!("menu.open_terminal"), None, C::OpenTerminal));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if single && !*is_dir && archive_kind(&name).is_some() {
            entries.push(MenuEntry::Separator);
            entries.push(item(
                t!("menu.extract_to", name = folder_name_for(&name)),
                None,
                C::ExtractToFolder,
            ));
            entries.push(item(t!("menu.extract_here"), None, C::ExtractHere));
        }
        if single && !*is_dir && is_backdrop_image(&name) {
            entries.push(MenuEntry::Separator);
            entries.push(item(t!("menu.set_background"), None, C::SetBackground));
        }
        // Settings backup. Whether it's really an Arke backup is checked in the background on
        // import.
        if single && !*is_dir && name.to_lowercase().ends_with(".json") {
            entries.push(MenuEntry::Separator);
            entries.push(item(t!("menu.import_settings"), None, C::ImportSettings));
        }
        // Search results may span folders, so compress is only offered in the normal list.
        if !self.search_mode {
            entries.push(MenuEntry::Separator);
            entries.push(item(
                t!(
                    "menu.compress",
                    name = format!("{}.zip", zip_stem_for(path, *is_dir))
                ),
                None,
                C::Compress,
            ));
            entries.push(item(
                t!("menu.move_to_new_folder"),
                None,
                C::MoveToNewFolder,
            ));
        }
        entries.push(MenuEntry::Separator);
        if single && *is_dir {
            entries.push(item(t!("menu.open_new_window"), None, C::OpenInNewWindow));
        }
        // Apps found on this PC (VS Code, Zed, ...), single selection only.
        if single {
            for (ix, app) in apps.iter().enumerate() {
                let mut entry = item(
                    t!("menu.open_with", name = app.name.as_str()),
                    None,
                    C::OpenWith(ix),
                );
                // Use the app's own icon, or a generic one if unreadable or user-added.
                if let MenuEntry::Item(row) = &mut entry {
                    row.icon.clone_from(&app.icon);
                }
                entries.push(entry);
            }
        }
        // Symlinks and junctions (files or folders): where they point.
        if single && self.target_is_link(path) {
            entries.push(item(t!("menu.go_to_link_target"), None, C::GoToLinkTarget));
        }
        // Single file: Windows "Open with"; shortcuts also get "Open file location".
        if single && !*is_dir {
            if name.to_lowercase().ends_with(".lnk") {
                entries.push(item(
                    t!("menu.open_file_location"),
                    None,
                    C::OpenFileLocation,
                ));
            }
            entries.push(item(t!("menu.choose_app"), None, C::ChooseApp));
        }
        entries.push(item(
            t!("menu.add_to_shelf"),
            Some("Ctrl+Shift+A"),
            C::AddToShelf,
        ));
        entries.push(item(t!("menu.copy_path"), None, C::CopyPath));
        // The tree names nested rows by their path below the folder shown.
        if self.tree_active() {
            entries.push(item(
                t!("menu.copy_relative_path"),
                None,
                C::CopyRelativePath,
            ));
        }
        entries.extend([
            item(t!("menu.reveal_in_explorer"), None, C::RevealInExplorer),
            item(t!("menu.properties"), None, C::Properties),
            MenuEntry::Separator,
            more_options(),
            MenuEntry::Separator,
            MenuEntry::Actions(vec![
                action(t!("menu.cut"), C::Cut, true),
                action(t!("menu.copy"), C::Copy, true),
                // Works on multiple items too: renames all selected to "name (1)", "name (2)", ...
                action(t!("menu.rename"), C::Rename, !self.search_mode),
                action(t!("menu.delete"), C::Delete, true),
            ]),
        ]);
        entries
    }

    // ───────────────────────── Keyboard ─────────────────────────

    fn move_highlight(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let selectable: Vec<usize> = menu
            .level(menu.open.len())
            .iter()
            .enumerate()
            .filter(|(_, e)| e.selectable())
            .map(|(ix, _)| ix)
            .collect();
        if selectable.is_empty() {
            return;
        }
        let pos = menu
            .highlighted
            .and_then(|h| selectable.iter().position(|&ix| ix == h));
        let next = match (pos, forward) {
            (None, true) => 0,
            (None, false) => selectable.len() - 1,
            (Some(p), true) => (p + 1) % selectable.len(),
            (Some(p), false) => (p + selectable.len() - 1) % selectable.len(),
        };
        menu.highlighted = Some(selectable[next]);
        menu.keyboard = true;
        cx.notify();
    }

    pub(super) fn menu_up(&mut self, _: &MenuUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(false, cx);
    }

    pub(super) fn menu_down(&mut self, _: &MenuDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(true, cx);
    }

    /// Right: opens the highlighted submenu and selects its first item.
    pub(super) fn menu_right(&mut self, _: &MenuRight, _: &mut Window, cx: &mut Context<Self>) {
        self.enter_submenu(cx);
    }

    /// Left: closes the innermost submenu and returns to the row that opened it.
    pub(super) fn menu_left(&mut self, _: &MenuLeft, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.context_menu.as_mut()
            && let Some(ix) = menu.open.pop()
        {
            menu.highlighted = Some(ix);
            menu.hover_task = None;
            cx.notify();
        }
    }

    fn enter_submenu(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.context_menu.as_mut() else {
            return false;
        };
        let Some(ix) = menu.highlighted else {
            return false;
        };
        let first = match menu.level(menu.open.len()).get(ix) {
            Some(MenuEntry::Submenu {
                enabled: true,
                children,
                ..
            }) => children.iter().position(MenuEntry::selectable),
            _ => return false,
        };
        menu.open.push(ix);
        menu.highlighted = first;
        menu.hover_task = None;
        menu.keyboard = true;
        self.ensure_shell_menu();
        cx.notify();
        true
    }

    pub(super) fn menu_confirm(
        &mut self,
        _: &MenuConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.enter_submenu(cx) {
            return;
        }
        let command = self.context_menu.as_ref().and_then(|m| {
            match m.level(m.open.len()).get(m.highlighted?)? {
                MenuEntry::Item(i) if i.enabled => Some(i.command),
                _ => None,
            }
        });
        match command {
            Some(command) => self.run_menu_command(command, window, cx),
            None => self.close_menu(window, cx),
        }
    }

    /// Esc: closes the open submenu first, otherwise the whole menu.
    pub(super) fn menu_dismiss(
        &mut self,
        _: &MenuDismiss,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .context_menu
            .as_ref()
            .is_some_and(|m| !m.open.is_empty())
        {
            self.menu_left(&MenuLeft, window, cx);
            return;
        }
        self.close_menu(window, cx);
    }

    /// Dismiss the menu (Escape, clicking outside): it fades out. Running a command closes it
    /// instantly instead (`run_menu_command`).
    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_shell_menu();
        if let Some(mut menu) = self.context_menu.take() {
            menu.hover_task = None;
            crate::motion::Leaving::start(self, menu, |this| &mut this.closing_menu, cx);
            window.focus(&self.list_focus, cx);
            cx.notify();
        }
    }

    /// Menu closed without a choice: release the background Windows menu too (fire and forget).
    fn release_shell_menu(&mut self) {
        if let Some(session) = self
            .context_menu
            .as_mut()
            .and_then(|m| m.shell_session.take())
        {
            self.services.shell.release_shell_menu(session);
        }
    }

    /// Hovering row `ix` at `level`: after a short delay, open its submenu (or close another one).
    fn hover_row(&mut self, level: usize, ix: usize, submenu: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let want: Vec<usize> = menu
            .open
            .iter()
            .take(level)
            .copied()
            .chain(submenu.then_some(ix))
            .collect();
        if menu.open == want {
            menu.hover_task = None;
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SUBMENU_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(menu) = this.context_menu.as_mut() {
                    menu.open = want;
                    menu.highlighted = None;
                    menu.keyboard = false;
                    this.ensure_shell_menu();
                    cx.notify();
                }
            });
        });
        menu.hover_task = Some(task);
    }

    /// Clicking a submenu row opens it immediately.
    fn click_submenu(&mut self, level: usize, ix: usize, cx: &mut Context<Self>) {
        if let Some(menu) = self.context_menu.as_mut() {
            menu.open.truncate(level);
            menu.open.push(ix);
            menu.highlighted = None;
            menu.hover_task = None;
            menu.keyboard = false;
            self.ensure_shell_menu();
            cx.notify();
        }
    }

    // ───────────────────────── Running commands ─────────────────────────

    // One arm per command; splitting it up would scatter them.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn run_menu_command(
        &mut self,
        command: MenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use MenuCommand as C;
        if !matches!(command, MenuCommand::Shell(_)) {
            self.release_shell_menu();
        }
        let Some(menu) = self.context_menu.take() else {
            return;
        };
        window.focus(&self.list_focus, cx);
        let targets = menu.targets;
        let pending_drop = self.pending_drop.take();
        match command {
            C::DropMove | C::DropCopy => {
                if let Some((sources, dest)) = pending_drop {
                    self.transfer_paths(sources, dest, command == C::DropMove, cx);
                }
            }
            C::DropShortcut => {
                if let Some((sources, dest)) = pending_drop {
                    self.create_shortcuts(sources, dest, cx);
                }
            }
            C::Open => self.open_targets(&targets, cx),
            C::OpenInNewTab => {
                if let Some((path, true)) = targets.first().cloned() {
                    cx.emit(ExplorerEvent::OpenInNewTab {
                        path,
                        activate: true,
                    });
                }
            }
            C::ExtractToFolder | C::ExtractHere => {
                if let Some((path, false)) = targets.first().cloned() {
                    self.start_extract(path, command == C::ExtractHere, cx);
                }
            }
            C::Cut => self.cut_files(&CutFiles, window, cx),
            C::Copy => self.copy_files(&CopyFiles, window, cx),
            C::Paste => self.paste_files(&PasteFiles, window, cx),
            C::Rename => self.rename_file(&RenameFile, window, cx),
            C::Delete => self.delete_files(&DeleteFiles, window, cx),
            C::NewFolder => self.new_folder(&NewFolder, window, cx),
            C::NewTextFile => self.new_text_file(cx),
            C::NewFolderIn | C::NewFileIn => {
                if let Some((path, true)) = targets.first() {
                    let kind = if command == C::NewFolderIn {
                        super::ops::NewItem::Folder
                    } else {
                        super::ops::NewItem::File
                    };
                    self.new_item_in(kind, path, cx);
                }
            }
            C::SearchInFolder => {
                if let Some((path, true)) = targets.first().cloned() {
                    self.search_in_folder(path, window, cx);
                }
            }
            C::Undo => self.undo_file_op(&crate::explorer::UndoFileOp, window, cx),
            C::Redo => self.redo_file_op(&crate::explorer::RedoFileOp, window, cx),
            C::Refresh => self.refresh(&Refresh, window, cx),
            C::ViewDetails => self.view_details(&ViewDetails, window, cx),
            C::ViewIcons => self.show_icons(cx),
            C::ViewColumns => self.view_columns(&crate::explorer::ViewColumns, window, cx),
            C::CopyPath => self.copy_paths(&targets),
            C::CopyRelativePath => self.copy_relative_paths(&targets),
            C::CopyAddress => self.copy_address(&targets),
            C::EditAddress => self.start_edit_path(window, cx),
            C::ClearHistory => {
                crate::settings::SettingsStore::global(cx)
                    .update(cx, |s, cx| s.update(|st| st.recent.clear(), cx));
                self.notice = Some(t!("notice.history_cleared").into_owned());
            }
            C::RevealInExplorer => self.reveal_in_explorer(&targets),
            C::KeepOnDevice | C::FreeUpSpace => {
                let pin = match (command, self.cloud_state(&targets)) {
                    (C::FreeUpSpace, _) => crate::shell::CloudPin::Free,
                    (_, Some(true)) => crate::shell::CloudPin::Default,
                    _ => crate::shell::CloudPin::Keep,
                };
                let paths: Vec<PathBuf> = targets.iter().map(|(p, _)| p.clone()).collect();
                let shell = self.services.shell.clone();
                let tx = self.events_tx.clone();
                spawn_thread("cloud-pin", move || {
                    if let Err(e) = shell.set_cloud_pin(&paths, pin) {
                        let _ = tx.send(AppEvent::Notice(Err(t!(
                            "error.cloud_pin_failed",
                            error = e
                        )
                        .into_owned())));
                    }
                });
            }
            C::OpenTerminal => self.open_terminal(&targets),
            C::Compress => self.start_compress(&targets, cx),
            C::MoveToNewFolder => self.move_into_new_folder(cx),
            C::OpenInNewWindow => {
                if let Some((path, true)) = targets.first().cloned() {
                    open_in_new_window(path);
                }
            }
            C::OpenWith(ix) => {
                let app = open_with_list(&SettingsStore::global(cx).read(cx).settings)
                    .into_iter()
                    .nth(ix)
                    .map(|a| a.exe);
                if let (Some((path, _)), Some(app)) = (targets.first().cloned(), app) {
                    crate::explorer::spawn_thread("open-with", move || {
                        if let Err(err) = std::process::Command::new(&app).arg(&path).spawn() {
                            log::warn!(
                                "failed to use {} to open {}: {err}",
                                app.display(),
                                path.display()
                            );
                        }
                    });
                }
            }
            C::Properties => self.run_shell_verb(&targets, "properties", window),
            // Windows' own "Open with" dialog, opened in the menu process.
            C::ChooseApp => {
                if let Some((path, false)) = targets.first().cloned() {
                    self.choose_app(path, window);
                }
            }
            C::ToggleSearchNetwork => {
                if let Some((path, true)) = targets.first().cloned() {
                    if crate::global_search::is_network_path(&path) {
                        crate::global_search::toggle_network(path, cx);
                    } else {
                        crate::global_search::toggle_local_exclusion(path, cx);
                    }
                }
            }
            C::AddToShelf => self.add_paths_to_shelf(targets.clone(), cx),
            C::OpenFileLocation => {
                if let Some((path, false)) = targets.first().cloned() {
                    let shell = self.services.shell.clone();
                    let tx = self.events_tx.clone();
                    spawn_thread("shortcut-target", move || {
                        let event = match shell.shortcut_target(&path) {
                            Some((target, _)) => AppEvent::RevealPath(target),
                            None => {
                                AppEvent::Notice(Err(t!("error.shortcut_no_target").into_owned()))
                            }
                        };
                        let _ = tx.send(event);
                    });
                }
            }
            C::GoToLinkTarget => {
                if let Some((path, _)) = targets.first().cloned() {
                    let fs = self.services.fs.clone();
                    let tx = self.events_tx.clone();
                    spawn_thread("link-target", move || {
                        let event = match fs.read_link(&path) {
                            Ok(target) => AppEvent::RevealPath(target),
                            Err(error) => {
                                AppEvent::Notice(Err(
                                    t!("error.link_no_target", error = error).into_owned()
                                ))
                            }
                        };
                        let _ = tx.send(event);
                    });
                }
            }
            C::ToggleColumn(col) => {
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(
                        |st| {
                            if let Some(on) = col.setting(st) {
                                *on = !*on;
                            }
                        },
                        cx,
                    );
                });
            }
            C::ResetColumnOrder => {
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(|st| st.column_order.clear(), cx);
                });
            }
            C::ImportSettings => {
                if let Some((path, false)) = targets.first().cloned() {
                    cx.emit(ExplorerEvent::ImportSettings(path));
                }
            }
            C::SetBackground => {
                if let Some((path, false)) = targets.first().cloned() {
                    crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
                        store.update(|s| s.background_image = Some(path), cx);
                    });
                }
            }
            // Disabled rows like "Loading..."; never executed.
            C::DropCancel | C::MoreOptions => {}
            C::Shell(id) => {
                if let Some(session) = menu.shell_session {
                    self.invoke_shell_menu(session, id, menu.position, window);
                }
            }
            C::TogglePin => {
                let path = match targets.first() {
                    Some((path, true)) => Some(path.clone()),
                    Some(_) => None,
                    None => self.current_path().map(PathBuf::from),
                };
                if let Some(path) = path {
                    let places = self.places.clone();
                    let mut went = None;
                    SettingsStore::global(cx).update(cx, |s, cx| {
                        s.update(|st| went = st.toggle_pin(&path, &places), cx);
                    });
                    if went == Some(crate::settings::PinPlace::List) {
                        self.notice = Some(t!("sidebar.pinned_to_list").into_owned());
                        crate::workspace::flash_pin(path);
                    }
                }
            }
        }
        cx.notify();
    }

    fn open_targets(&mut self, targets: &[(PathBuf, bool)], cx: &mut Context<Self>) {
        if let [(path, true)] = targets {
            self.navigate(path.clone(), arke_core::NavKind::New, cx);
            return;
        }
        // Multiple selection: open all files. Only one folder could be entered, so folders are
        // ignored.
        for (path, is_dir) in targets {
            if !is_dir {
                self.open_file(path.clone(), cx);
            }
        }
    }

    fn copy_paths(&self, targets: &[(PathBuf, bool)]) {
        // Same as Explorer's "Copy as path": each path quoted, one per line.
        let text = if targets.is_empty() {
            self.current_path()
                .map(|p| format!("\"{}\"", p.display()))
                .unwrap_or_default()
        } else {
            targets
                .iter()
                .map(|(p, _)| format!("\"{}\"", p.display()))
                .collect::<Vec<_>>()
                .join("\r\n")
        };
        let count = targets.len().max(1);
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("copy-path", move || {
            let result = shell
                .set_clipboard_text(&text)
                .map(|()| tn!("notice.paths_copied", count).into_owned())
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Notice(result));
        });
    }

    /// "Copy relative path" in the Details tree: each item's path below the folder shown
    /// (`src\ui\view.rs`), one per line and unquoted, like VS Code. Servers get `/`.
    fn copy_relative_paths(&self, targets: &[(PathBuf, bool)]) {
        let Some(here) = self.current_path() else {
            return;
        };
        let remote = arke_core::remote::is_remote(here);
        let lines: Vec<String> = targets
            .iter()
            .filter_map(|(path, _)| path.strip_prefix(here).ok())
            .map(|rel| {
                let rel = rel.display().to_string();
                if remote { rel.replace('\\', "/") } else { rel }
            })
            .collect();
        if lines.is_empty() {
            return;
        }
        let count = lines.len();
        let text = lines.join("\r\n");
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("copy-relative-path", move || {
            let result = shell
                .set_clipboard_text(&text)
                .map(|()| tn!("notice.paths_copied", count).into_owned())
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Notice(result));
        });
    }

    /// Path bar "Copy address": plain path without quotes; network locations as `\\server\share\…`.
    fn copy_address(&self, targets: &[(PathBuf, bool)]) {
        let Some(path) = targets
            .first()
            .map(|(p, _)| p.clone())
            .or_else(|| self.current_path().map(std::path::Path::to_path_buf))
        else {
            return;
        };
        let text = path.display().to_string();
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("copy-address", move || {
            let result = shell
                .set_clipboard_text(&text)
                .map(|()| t!("notice.address_copied").into_owned())
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Notice(result));
        });
    }

    /// Ctrl+Shift+C: same as the menu's "Copy as path" (quoted, one per line; current folder if
    /// nothing is selected).
    pub(super) fn copy_as_path(
        &mut self,
        _: &super::CopyAsPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.menu_targets();
        self.copy_paths(&targets);
        cx.notify();
    }

    /// Right-click on the path bar: copy address, copy as path, edit address, clear history (like
    /// Explorer's address bar).
    pub(crate) fn open_path_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use MenuCommand as C;
        let Some(here) = self.current_path().map(std::path::Path::to_path_buf) else {
            return;
        };
        let entries = vec![
            item(t!("menu.copy_address"), None, C::CopyAddress),
            item(t!("menu.copy_as_path"), Some("Ctrl+Shift+C"), C::CopyPath),
            item(t!("menu.edit_address"), Some("Ctrl+L"), C::EditAddress),
            MenuEntry::Separator,
            item(t!("menu.clear_history"), None, C::ClearHistory),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        self.context_menu = Some(ContextMenu::new(
            position,
            entries,
            focus,
            vec![(here, true)],
        ));
        cx.notify();
    }

    /// Whether the menu target `path` is a symlink or junction (from the listing, no I/O).
    fn target_is_link(&self, path: &std::path::Path) -> bool {
        if self.search_mode {
            return self
                .search_hit(self.search_cursor)
                .is_some_and(|hit| hit.entry.is_link());
        }
        let (Some(model), Some(name)) = (self.model.as_ref(), path.file_name()) else {
            return false;
        };
        model
            .index_of(&name.to_string_lossy())
            .and_then(|ix| model.entries.get(ix))
            .is_some_and(arke_core::Entry::is_link)
    }

    /// Right-click on the Details header: every optional column with a check mark when shown (the
    /// same switches as Settings), and putting the order back.
    pub(crate) fn open_header_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::prefs::Col;
        let mut entries: Vec<MenuEntry> = Col::ALL
            .into_iter()
            .filter(|&col| col != Col::Name)
            .map(|col| {
                MenuEntry::Item(MenuItem {
                    checked: col.shown(),
                    ..action(col.label(), MenuCommand::ToggleColumn(col), true)
                })
            })
            .collect();
        entries.push(MenuEntry::Separator);
        entries.push(item_if(
            crate::prefs::order() != Col::MOVABLE,
            t!("menu.reset_column_order"),
            None,
            MenuCommand::ResetColumnOrder,
        ));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        self.context_menu = Some(ContextMenu::new(position, entries, focus, Vec::new()));
        cx.notify();
    }

    /// Alt-drop: asks at the cursor whether to move, copy or create shortcuts (what right-drag does
    /// in
    /// Windows). The default action, what a plain drop would do, comes first.
    pub(crate) fn open_drop_menu(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use MenuCommand as C;
        if !crate::explorer::dnd::useful_drop(&sources, &dest) {
            return;
        }
        let name = dest.file_name().map_or_else(
            || dest.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let move_item = item(t!("dnd.menu_move", dest = name), None, C::DropMove);
        let copy_item = item(t!("dnd.menu_copy", dest = name), None, C::DropCopy);
        let default_move =
            crate::explorer::dnd::drop_is_move(gpui::Modifiers::default(), &sources, &dest);
        let mut entries = if default_move {
            vec![move_item, copy_item]
        } else {
            vec![copy_item, move_item]
        };
        entries.push(item(t!("dnd.menu_shortcut"), None, C::DropShortcut));
        entries.push(MenuEntry::Separator);
        entries.push(item(t!("dnd.menu_cancel"), Some("Esc"), C::DropCancel));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        self.context_menu = Some(ContextMenu::new(position, entries, focus, Vec::new()));
        self.pending_drop = Some((sources, dest));
        cx.notify();
    }

    /// Creates a shortcut in `dest` for each of `sources` in the background. The watcher updates
    /// the list.
    pub(crate) fn create_shortcuts(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        crate::explorer::spawn_thread("create-shortcuts", move || {
            if let Err(err) = shell.create_shortcuts(&sources, &dest) {
                let _ = tx.send(crate::explorer::AppEvent::Notice(Err(t!(
                    "dnd.shortcut_failed",
                    error = err
                )
                .into_owned())));
            }
        });
        cx.notify();
    }

    /// Opens a terminal in the selected folder, or the current one if nothing is selected.
    fn open_terminal(&self, targets: &[(PathBuf, bool)]) {
        let dir = match targets.first() {
            Some((p, true)) => p.clone(),
            Some((_, false)) => return,
            None => match self.current_path() {
                Some(p) => p.to_path_buf(),
                None => return,
            },
        };
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("terminal", move || {
            if let Err(e) = shell.open_terminal(&dir) {
                let _ = tx.send(AppEvent::Notice(Err(t!(
                    "error.terminal_failed",
                    error = e
                )
                .into_owned())));
            }
        });
    }

    /// Windows' own "Open with" dialog for `path`, owned by this window.
    pub(crate) fn choose_app(&self, path: PathBuf, window: &Window) {
        let shell = self.services.shell.clone();
        let owner = Self::owner_from(window);
        let tx = self.events_tx.clone();
        crate::explorer::spawn_thread("choose-app", move || {
            if let Err(err) = shell.choose_app(&path, owner) {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = tx.send(crate::explorer::AppEvent::Notice(Err(t!(
                    "error.open_failed",
                    name = name,
                    error = err
                )
                .into_owned())));
            }
        });
    }

    fn reveal_in_explorer(&self, targets: &[(PathBuf, bool)]) {
        let (path, select) = match targets.first() {
            Some((p, _)) => (p.clone(), true),
            None => match self.current_path() {
                Some(p) => (p.to_path_buf(), false),
                None => return,
            },
        };
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("reveal", move || {
            if let Err(e) = shell.reveal_in_explorer(&path, select) {
                let _ = tx.send(AppEvent::Notice(Err(
                    t!("error.reveal_failed", error = e).into_owned()
                )));
            }
        });
    }

    /// Asks the menu process for the Windows context menu as soon as the menu opens; "Show more
    /// options"
    /// shows "Loading..." until then. If prefetch is off, it's read when the submenu is opened.
    fn ensure_shell_menu(&mut self) {
        let Some(mut menu) = self.context_menu.take() else {
            return;
        };
        let opened = menu.shell_slot.is_some() && menu.open.first() == menu.shell_slot.as_ref();
        if opened && menu.shell_request.is_none() && menu.shell_session.is_none() {
            self.request_shell_menu(&mut menu);
        }
        self.context_menu = Some(menu);
    }

    /// Requests the menu for the selected items, or the folder background menu for empty space.
    fn request_shell_menu(&self, menu: &mut ContextMenu) {
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
        let background = menu.targets.is_empty();
        let paths: Vec<PathBuf> = if background {
            self.current_path().map(PathBuf::from).into_iter().collect()
        } else {
            menu.targets.iter().map(|(p, _)| p.clone()).collect()
        };
        if paths.is_empty() {
            return;
        }
        let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        menu.shell_request = Some(request);
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("more-options", move || {
            let result = shell.shell_menu(&paths, background);
            let _ = tx.send(AppEvent::ShellMenu { request, result });
        });
    }

    /// Windows menu is ready: if it's still the same menu (not closed or replaced), put it under
    /// "Show
    /// more options"; otherwise release it.
    pub(super) fn handle_shell_menu(
        &mut self,
        request: u64,
        result: Result<ShellMenu, crate::shell::ShellError>,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self
            .context_menu
            .as_mut()
            .filter(|m| m.shell_request == Some(request))
        else {
            if let Ok(shell_menu) = result {
                self.services.shell.release_shell_menu(shell_menu.session);
            }
            return;
        };
        menu.shell_request = None;
        let children = match result {
            Ok(shell_menu) => {
                menu.shell_session = Some(shell_menu.session);
                let entries: Vec<MenuEntry> = without_duplicates(shell_menu.entries, &menu.entries)
                    .into_iter()
                    .map(|e| MenuEntry::from_shell(e, &[]))
                    .collect();
                if entries.is_empty() {
                    vec![placeholder(t!("menu.no_more_options"))]
                } else {
                    entries
                }
            }
            Err(e) => vec![error_line(t!("menu.load_failed", error = e))],
        };
        if let Some(MenuEntry::Submenu { children: c, .. }) =
            menu.shell_slot.and_then(|ix| menu.entries.get_mut(ix))
        {
            *c = children;
        }
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        menu.sync_pinned(&pinned);
        let slot = menu.shell_slot;
        // Keyboard already entered "Show more options" while it held only the disabled "Loading..."
        // row:
        // select the first item.
        if menu.open.len() == 1
            && menu.open.first() == slot.as_ref()
            && menu.highlighted.is_none()
            && menu.keyboard
        {
            menu.highlighted = menu.level(1).iter().position(MenuEntry::selectable);
        }
        cx.notify();
    }

    /// Runs the item with `verb` (e.g. "properties", "openas") from the classic Windows menu. The
    /// menu
    /// process reads the menu, finds the item (including in submenus) and runs it, so third-party
    /// code
    /// stays in that process.
    fn run_shell_verb(&self, targets: &[(PathBuf, bool)], verb: &'static str, window: &Window) {
        fn find(entries: &[crate::shell::ShellMenuEntry], verb: &str) -> Option<u32> {
            entries.iter().find_map(|e| match e {
                crate::shell::ShellMenuEntry::Item {
                    id, verb: Some(v), ..
                } if v.eq_ignore_ascii_case(verb) => Some(*id),
                crate::shell::ShellMenuEntry::Submenu { children, .. } => find(children, verb),
                _ => None,
            })
        }
        let paths: Vec<PathBuf> = targets.iter().map(|(p, _)| p.clone()).collect();
        let shell = self.services.shell.clone();
        let owner = Self::owner_from(window);
        crate::explorer::spawn_thread("shell-verb", move || {
            let result = shell.shell_menu(&paths, false).and_then(|menu| {
                let Some(id) = find(&menu.entries, verb) else {
                    shell.release_shell_menu(menu.session);
                    log::warn!("failed to run Windows menu verb {verb}: no {verb} item");
                    return Ok(());
                };
                shell.invoke_shell_menu(menu.session, id, (0, 0), owner)
            });
            if let Err(err) = result {
                log::warn!("failed to run Windows menu verb {verb}: {err}");
            }
        });
    }

    /// Pins or unpins a Windows command (pin icon on rows in "Show more options"); an open menu
    /// updates immediately.
    pub(crate) fn toggle_shell_pin(&mut self, key: &str, cx: &mut Context<Self>) {
        let key = key.to_string();
        let pinned = SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    if let Some(at) = s.menu_pinned.iter().position(|k| *k == key) {
                        s.menu_pinned.remove(at);
                    } else {
                        s.menu_pinned.push(key);
                    }
                },
                cx,
            );
            store.settings.menu_pinned.clone()
        });
        if let Some(menu) = self.context_menu.as_mut() {
            menu.sync_pinned(&pinned);
        }
        cx.notify();
    }

    /// Runs a Windows menu item on a background STA thread; some commands open windows.
    fn invoke_shell_menu(&self, session: u64, id: u32, position: Point<Pixels>, window: &Window) {
        let scale = window.scale_factor();
        let client = (
            (f32::from(position.x) * scale).round() as i32,
            (f32::from(position.y) * scale).round() as i32,
        );
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        let owner = self.owner;
        spawn_thread("shell-command", move || {
            if let Err(e) = shell.invoke_shell_menu(session, id, client, owner) {
                let _ = tx.send(AppEvent::Notice(Err(t!(
                    "error.system_menu_failed",
                    error = e
                )
                .into_owned())));
            }
        });
    }

    // ───────────────────────── Extraction ─────────────────────────

    fn start_extract(&mut self, archive_path: PathBuf, here: bool, cx: &mut Context<Self>) {
        if self.extract_job.is_some() {
            self.notice = Some(t!("extract.busy").into_owned());
            return;
        }
        let name = archive_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (Some(kind), Some(model)) = (archive_kind(&name), self.model.as_ref()) else {
            return;
        };
        // Extracting into a new folder picks a unique name. Only listed items are checked; on a
        // collision
        // existing files still aren't overwritten.
        let (dest, folder) = if here {
            (model.path.clone(), None)
        } else {
            let existing: std::collections::HashSet<String> = model
                .entries
                .iter()
                .map(|e| e.name.to_lowercase())
                .collect();
            let folder =
                arke_core::path::unique_name(&folder_name_for(&name), |n| existing.contains(n));
            (model.path.join(&folder), Some(folder))
        };
        let (job, ops) =
            crate::op_center::OpCenter::begin(cx, "op.extract", Some((name.clone(), 1)));
        self.extract_job = Some(ExtractJob {
            name,
            compress: false,
            job,
            ops: ops.clone(),
            progress: ExtractProgress::default(),
        });
        self.notice = None;
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("extract", move || {
            // Banner progress is sent at most every 100 ms to avoid flooding the UI; the progress
            // center reads counters directly.
            let last = Mutex::new(Instant::now());
            let progress_tx = tx.clone();
            let counters = ops.clone();
            let progress = move |p: ExtractProgress| {
                report(&counters, p);
                let mut last = last.lock();
                if last.elapsed() >= Duration::from_millis(100) {
                    *last = Instant::now();
                    drop(last);
                    let _ = progress_tx.send(AppEvent::ExtractProgress(p));
                }
            };
            let decode = |bytes: &[u8]| shell.decode_ansi(bytes);
            let result = archive::extract(
                &archive_path,
                &dest,
                kind,
                &ExtractOptions {
                    cancel: &ops.cancel,
                    progress: &progress,
                    decode_legacy: &decode,
                },
            );
            let _ = tx.send(AppEvent::ExtractDone { result, folder });
        });
        cx.notify();
    }

    /// Zips into the same folder, named after the first item (made unique). Runs in the background
    /// with cancellable progress in the banner.
    fn start_compress(&mut self, targets: &[(PathBuf, bool)], cx: &mut Context<Self>) {
        if self.extract_job.is_some() {
            self.notice = Some(t!("extract.busy").into_owned());
            return;
        }
        let (Some((first, first_is_dir)), Some(model)) = (targets.first(), self.model.as_ref())
        else {
            return;
        };
        let existing: std::collections::HashSet<String> = model
            .entries
            .iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        let name =
            arke_core::path::unique_file_name(&zip_stem_for(first, *first_is_dir), "zip", |n| {
                existing.contains(n)
            });
        let dest = model.path.join(&name);
        let sources: Vec<PathBuf> = targets.iter().map(|(p, _)| p.clone()).collect();
        let first_name = first.file_name().map_or_else(
            || first.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let (job, ops) =
            crate::op_center::OpCenter::begin(cx, "op.compress", Some((first_name, sources.len())));
        self.extract_job = Some(ExtractJob {
            name: name.clone(),
            compress: true,
            job,
            ops: ops.clone(),
            progress: ExtractProgress::default(),
        });
        self.notice = None;
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("compress", move || {
            let last = Mutex::new(Instant::now());
            let progress_tx = tx.clone();
            let counters = ops.clone();
            let progress = move |p: ExtractProgress| {
                report(&counters, p);
                let mut last = last.lock();
                if last.elapsed() >= Duration::from_millis(100) {
                    *last = Instant::now();
                    drop(last);
                    let _ = progress_tx.send(AppEvent::ExtractProgress(p));
                }
            };
            let local_time = |t: std::time::SystemTime| shell.local_parts(t);
            let result = archive::compress(
                &sources,
                &dest,
                &archive::CompressOptions {
                    cancel: &ops.cancel,
                    progress: &progress,
                    local_time: &local_time,
                },
            );
            let _ = tx.send(AppEvent::CompressDone { result, name, dest });
        });
        cx.notify();
    }

    pub(super) fn handle_compress_done(
        &mut self,
        result: Result<archive::CompressSummary, archive::ArchiveError>,
        name: String,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if let Some(job) = self.extract_job.take() {
            let state = outcome(result.as_ref().map(|s| s.cancelled));
            crate::op_center::OpCenter::finish(cx, job.job, "op.compress", &state);
        }
        match result {
            Err(e) => {
                self.error = Some(t!("compress.failed", name = name, error = e).into_owned());
            }
            Ok(s) if s.cancelled => {
                self.notice = Some(t!("compress.cancelled").into_owned());
            }
            Ok(s) => {
                let mut msg = tn!(
                    "compress.done",
                    usize::try_from(s.files).unwrap_or(usize::MAX),
                    name = name.as_str()
                )
                .into_owned();
                if s.skipped > 0 {
                    msg.push_str(&t!("compress.skipped", count = s.skipped));
                }
                self.notice = Some(msg);
                // Ctrl+Z moves the new archive to the recycle bin.
                super::ops::UndoHistory::push(
                    cx,
                    "op.compress",
                    crate::shell::Undo::Remove(vec![dest.clone()]),
                    None,
                    true,
                );
                if dest.parent() == self.current_path() {
                    self.selection.select_name(Arc::from(name.as_str()));
                    self.scroll_to_cursor = Some(gpui::ScrollStrategy::Nearest);
                    self.resolve_cursor();
                }
            }
        }
        cx.notify();
    }

    pub(super) fn handle_extract_progress(&mut self, p: ExtractProgress, cx: &mut Context<Self>) {
        if let Some(job) = self.extract_job.as_mut() {
            job.progress = p;
            cx.notify();
        }
    }

    pub(super) fn handle_extract_done(
        &mut self,
        result: Result<ExtractSummary, archive::ArchiveError>,
        folder: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let name = match self.extract_job.take() {
            Some(job) => {
                let state = outcome(result.as_ref().map(|s| s.cancelled));
                crate::op_center::OpCenter::finish(cx, job.job, "op.extract", &state);
                job.name
            }
            None => String::new(),
        };
        match result {
            Err(e) => {
                self.error = Some(t!("extract.failed", name = name, error = e).into_owned());
            }
            Ok(s) => {
                let count = s.files as usize;
                let mut msg = if s.cancelled {
                    tn!("extract.cancelled", count).into_owned()
                } else {
                    tn!("extract.done", count).into_owned()
                };
                if s.skipped_existing > 0 {
                    msg.push_str(&tn!(
                        "extract.skipped_existing",
                        usize::try_from(s.skipped_existing).unwrap_or(usize::MAX)
                    ));
                }
                if s.skipped_unsafe > 0 {
                    msg.push_str(&tn!(
                        "extract.skipped_unsafe",
                        usize::try_from(s.skipped_unsafe).unwrap_or(usize::MAX)
                    ));
                }
                self.notice = Some(msg);
                if let Some(folder) = folder {
                    // Selected as soon as the new folder shows up in the list.
                    self.selection.select_name(Arc::from(folder.as_str()));
                    self.scroll_to_cursor = Some(gpui::ScrollStrategy::Nearest);
                    self.resolve_cursor();
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn cancel_extract(&mut self) {
        if let Some(job) = &self.extract_job {
            job.ops.cancel.store(true, Ordering::Relaxed);
        }
    }

    // ───────────────────────── Rendering ─────────────────────────

    pub(super) fn render_context_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(menu) = self.context_menu.as_ref() {
            return Some(Self::render_menu_levels(menu, false, window, cx));
        }
        // A dismissed menu fading out; a new menu replaces it at once.
        let ghost = self.closing_menu.as_ref()?;
        Some(Self::render_menu_levels(&ghost.value, true, window, cx))
    }

    /// Every open level of `menu`. `leaving`: the dismissed menu's ghost, drawn inert while it fades.
    // Positions and animation of the top level and each submenu are easier to follow side by side.
    #[allow(clippy::too_many_lines)]
    fn render_menu_levels(
        menu: &ContextMenu,
        leaving: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let geometry = menu.geometry.clone();
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        let mut panels: Vec<AnyElement> = Vec::new();
        // Top level sits at the right-click position; clicking outside every level closes it.
        let outer = Self::render_menu_panel(menu, 0, &pinned, cx);
        let outer = if leaving {
            crate::motion::leave(
                outer,
                ElementId::Name(format!("menu-{}-leave", menu.serial).into()),
                0.,
                -MENU_SLIDE / 2.,
            )
        } else {
            outer
            .key_context("ContextMenu")
            .track_focus(&menu.focus)
            .on_action(cx.listener(Self::menu_up))
            .on_action(cx.listener(Self::menu_down))
            .on_action(cx.listener(Self::menu_left))
            .on_action(cx.listener(Self::menu_right))
            .on_action(cx.listener(Self::menu_confirm))
            .on_action(cx.listener(Self::menu_dismiss))
            .on_mouse_down_out(cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                let inside = geometry
                    .borrow()
                    .panels
                    .iter()
                    .any(|(&level, b)| level > 0 && b.contains(&e.position));
                if !inside {
                    this.close_menu(window, cx);
                }
            }))
            // Slide down and fade in.
            .with_animation(
                ElementId::Name(format!("menu-{}", menu.serial).into()),
                crate::motion::enter(),
                |panel, t| crate::motion::slide_in(panel, t, 0., -MENU_SLIDE),
            )
            .into_any_element()
        };
        panels.push(
            deferred(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(theme::S_8)
                    .child(outer),
            )
            .with_priority(1)
            .into_any_element(),
        );
        // Submenus sit next to the row that opened them and follow the parent's direction. If that
        // doesn't
        // fit, they go past all open menus on the other side rather than covering an ancestor (see
        // `place_submenu`).
        let viewport = window.viewport_size();
        let mut going_right = true;
        for level in 1..=menu.open.len() {
            let geometry = menu.geometry.borrow();
            let Some(row) = geometry
                .rows
                .get(&((level - 1) * LEVEL_STRIDE + menu.open[level - 1]))
                .copied()
            else {
                break;
            };
            let parent = geometry.panels.get(&(level - 1)).copied().unwrap_or(row);
            let width = geometry
                .panels
                .get(&level)
                .map_or(px(240.), |b| b.size.width);
            let ancestors: Vec<(f32, f32)> = (0..level)
                .filter_map(|l| geometry.panels.get(&l))
                .map(|b| (f32::from(b.left()), f32::from(b.right())))
                .collect();
            drop(geometry);
            let top = row.origin.y - theme::S_4;
            let (left, right_side) = place_submenu(
                (f32::from(parent.left()), f32::from(parent.right())),
                &ancestors,
                f32::from(width),
                f32::from(viewport.width),
                going_right,
            );
            going_right = right_side;
            {
                let mut geometry = menu.geometry.borrow_mut();
                if right_side {
                    geometry.opens_left.remove(&(level - 1));
                } else {
                    geometry.opens_left.insert(level - 1);
                }
            }
            let (anchor, x) = (Anchor::TopLeft, px(left));
            // Slide out from the opening side and fade in; replays when switching submenus.
            let slide = if right_side { -MENU_SLIDE } else { MENU_SLIDE };
            let path = menu.open[..level]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(".");
            let panel = Self::render_menu_panel(menu, level, &pinned, cx);
            let panel = if leaving {
                crate::motion::leave(
                    panel,
                    ElementId::Name(format!("menu-{}-{path}-leave", menu.serial).into()),
                    0.,
                    -MENU_SLIDE / 2.,
                )
            } else {
                panel
                    .with_animation(
                        ElementId::Name(format!("menu-{}-{path}", menu.serial).into()),
                        crate::motion::enter(),
                        move |panel, t| crate::motion::slide_in(panel, t, slide, 0.),
                    )
                    .into_any_element()
            };
            panels.push(
                deferred(
                    anchored()
                        .anchor(anchor)
                        .position(point(x, top))
                        .snap_to_window_with_margin(theme::S_8)
                        .child(panel),
                )
                .with_priority(1 + level)
                .into_any_element(),
            );
        }
        div().children(panels).into_any_element()
    }

    /// One menu level: frame and rows. Row and frame bounds are recorded in `geometry` after paint
    /// for submenu alignment.
    fn render_menu_panel(
        menu: &ContextMenu,
        level: usize,
        pinned: &[String],
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let entries = menu.level(level);
        let current = level == menu.open.len();
        let open_here = menu.open.get(level).copied();
        let marks = entries.iter().any(MenuEntry::has_mark);
        let rows: Vec<AnyElement> = entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                let highlighted =
                    (current && menu.highlighted == Some(ix)) || open_here == Some(ix);
                Self::render_menu_row(menu, level, ix, entry, highlighted, marks, pinned, cx)
            })
            .collect();
        let geometry = menu.geometry.clone();
        div()
            .id(("context-menu", level))
            .role(gpui::Role::Menu)
            .occlude()
            .min_w(px(240.))
            .max_w(px(420.))
            .py(theme::S_4)
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .relative()
            .child(
                canvas(
                    move |bounds, _, _| {
                        geometry.borrow_mut().panels.insert(level, bounds);
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(rows)
    }

    /// Bottom row of icon buttons: icon above label, equal widths.
    fn render_action_row(items: &[MenuItem], cx: &mut Context<Self>) -> AnyElement {
        div()
            .mx(theme::S_4)
            .flex()
            .gap(theme::S_2)
            .children(items.iter().enumerate().map(|(ix, item)| {
                let command = item.command;
                let enabled = item.enabled;
                div()
                    .id(("menu-action", ix))
                    .a11y_menu_item(item.label.clone(), enabled, false, false)
                    .flex_1()
                    .py(theme::S_6)
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(theme::S_4)
                    .rounded(theme::R_8)
                    .text_size(theme::FS_12)
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .text_color(theme::text_muted())
                            .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.run_menu_command(command, window, cx);
                            }))
                    })
                    .when(!enabled, |d| d.text_color(theme::text_disabled()))
                    .children(item.glyph.map(|glyph| {
                        svg()
                            .path(glyph.path())
                            .size(theme::ICON)
                            .text_color(if enabled {
                                theme::text()
                            } else {
                                theme::text_disabled()
                            })
                    }))
                    .child(item.label.clone())
            }))
            .into_any_element()
    }

    /// Pin on Windows menu rows: always visible (accent) when pinned, otherwise only on row hover.
    fn pin_button(
        key: usize,
        pin: String,
        pinned: &[String],
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let on = pinned.contains(&pin);
        div()
            .id(("menu-pin", key))
            .a11y_button(if on {
                t!("menu.unpin_command")
            } else {
                t!("menu.pin_command")
            })
            .flex_none()
            .size(px(22.))
            .mr(-theme::S_4)
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .cursor_pointer()
            .when(!on, |d| d.invisible().group_hover(group.clone(), gpui::Styled::visible))
            .hover(|s| s.bg(theme::wash_strong()))
            .tooltip(crate::workspace::tooltip(if on {
                t!("menu.unpin_command")
            } else {
                t!("menu.pin_command")
            }))
            .child(svg().path(Icon::Pin.path()).size(theme::ICON_SM).text_color(if on {
                theme::signal()
            } else {
                theme::text_muted()
            }))
            // Handled here so the row doesn't run its command or close the menu.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle_shell_pin(&pin, cx);
                }),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_menu_row(
        menu: &ContextMenu,
        level: usize,
        ix: usize,
        entry: &MenuEntry,
        highlighted: bool,
        marks: bool,
        pinned: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let MenuEntry::Actions(items) = entry {
            return Self::render_action_row(items, cx);
        }
        let Some(RowParts {
            label,
            danger,
            enabled,
            icon,
            checked,
            bold,
            shortcut,
            submenu,
            command,
            glyph,
            pin,
        }) = RowParts::of(entry)
        else {
            return div()
                .h(px(1.))
                .my(theme::S_4)
                .mx(theme::S_8)
                .bg(theme::line())
                .into_any_element();
        };
        let key = level * LEVEL_STRIDE + ix;
        let geometry = menu.geometry.clone();
        let mark = marks.then(|| row_mark(icon, glyph, checked));
        let group: SharedString = format!("menu-row-{key}").into();
        let pin_button = pin.map(|pin| Self::pin_button(key, pin, pinned, group.clone(), cx));
        div()
            .id(("menu-item", key))
            .a11y_menu_item(label.clone(), enabled, checked, highlighted)
            .when(submenu, |d| d.aria_expanded(menu.open.get(level) == Some(&ix)))
            .when_some(shortcut, gpui::StatefulInteractiveElement::aria_keyshortcuts)
            .group(group)
            .relative()
            .h(theme::H_CTL_SM + theme::S_4)
            .mx(theme::S_4)
            .px(theme::S_8)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_8)
            .when(submenu, |d| {
                d.child(
                    canvas(
                        move |bounds, window, _| {
                            // The row moved (e.g. "Show more options" shifted down after a pin):
                            // redraw the submenu too.
                            let old = geometry.borrow_mut().rows.insert(key, bounds);
                            if old != Some(bounds) {
                                window.refresh();
                            }
                        },
                        |_, (), _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .when(!enabled && !danger, |d| {
                d.text_color(theme::text_disabled())
            })
            .when(danger, |d| d.text_color(theme::danger()))
            .when(enabled, |d| {
                d.cursor_pointer()
                    .when(highlighted, |d| d.bg(theme::wash_strong()))
                    .hover(|s| s.bg(theme::wash()))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| match command {
                            Some(command) => this.run_menu_command(command, window, cx),
                            None => this.click_submenu(level, ix, cx),
                        }),
                    )
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hover_row(level, ix, submenu && enabled, cx);
                }
            }))
            .children(mark)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(bold, |d| d.font_weight(theme::FW_STRONG))
                    .child(label),
            )
            .children(shortcut.map(|s| {
                div()
                    .flex_none()
                    .pl(theme::S_16)
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(s)
            }))
            .children(pin_button)
            // Chevron points left when the submenu opens to the left.
            .when(submenu, |d| {
                d.child(submenu_chevron(menu.geometry.borrow().opens_left.contains(&level)))
            })
            .into_any_element()
    }
}

/// What a row draws (separators have none).
#[allow(clippy::struct_excessive_bools)]
struct RowParts {
    label: SharedString,
    enabled: bool,
    icon: Option<Arc<RenderImage>>,
    checked: bool,
    bold: bool,
    danger: bool,
    shortcut: Option<&'static str>,
    submenu: bool,
    /// Submenu rows have no command; clicking opens the submenu.
    command: Option<MenuCommand>,
    glyph: Option<Icon>,
    /// Windows menu items: pin key (row shows a pin on the right).
    pin: Option<String>,
}

impl RowParts {
    fn of(entry: &MenuEntry) -> Option<Self> {
        match entry {
            MenuEntry::Separator | MenuEntry::Actions(_) => None,
            MenuEntry::Item(it) => Some(Self {
                label: it.label.clone(),
                enabled: it.enabled,
                icon: it.icon.clone(),
                checked: it.checked,
                bold: it.bold,
                danger: it.danger,
                shortcut: it.shortcut,
                submenu: false,
                command: Some(it.command),
                glyph: it.glyph,
                pin: it.pin.clone(),
            }),
            MenuEntry::Submenu {
                label,
                enabled,
                icon,
                ..
            } => Some(Self {
                label: label.clone(),
                enabled: *enabled,
                icon: icon.clone(),
                checked: false,
                bold: false,
                danger: false,
                shortcut: None,
                submenu: true,
                command: None,
                glyph: None,
                pin: None,
            }),
        }
    }
}

/// Icon or check mark on the left of a row. If any row in a level has one, the whole level reserves
/// the space.
fn row_mark(icon: Option<Arc<RenderImage>>, glyph: Option<Icon>, checked: bool) -> gpui::Div {
    div()
        .size(theme::ICON)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .children(icon.map(|image| img(image).size(theme::ICON)))
        .children(glyph.filter(|_| !checked).map(|glyph| {
            svg()
                .path(glyph.path())
                .size(theme::ICON)
                .text_color(theme::text_muted())
        }))
        .when(checked, |d| {
            d.child(
                svg()
                    .path(Icon::Check.path())
                    .size(theme::ICON)
                    .text_color(theme::text_muted()),
            )
        })
}

/// Drops items from "Show more options" that Arke's menu already has (open, cut, copy, delete,
/// ...),
/// matched by standard Windows verbs like Files does. Only the top level is filtered.
fn without_duplicates(shell: Vec<ShellMenuEntry>, ours: &[MenuEntry]) -> Vec<ShellMenuEntry> {
    use MenuCommand as C;
    let verbs: Vec<&str> = ours
        .iter()
        // The icon button row holds Arke commands too.
        .flat_map(|e| match e {
            MenuEntry::Item(i) => vec![i.command],
            MenuEntry::Actions(items) => items.iter().map(|i| i.command).collect(),
            _ => Vec::new(),
        })
        .flat_map(|command| match command {
            C::Open => &["open", "explore"][..],
            C::OpenInNewTab => &["opennewtab"][..],
            C::Cut => &["cut"][..],
            C::Copy => &["copy"][..],
            C::Paste => &["paste"][..],
            C::Refresh => &["refresh"][..],
            C::Rename => &["rename"][..],
            C::Delete => &["delete"][..],
            C::CopyPath => &["copyaspath"][..],
            C::Properties => &["properties"][..],
            C::OpenInNewWindow => &["opennewwindow"][..],
            _ => &[][..],
        })
        .copied()
        .collect();
    let kept = shell
        .into_iter()
        .filter(|e| match e {
            ShellMenuEntry::Item {
                verb: Some(verb), ..
            } => !verbs.iter().any(|v| v.eq_ignore_ascii_case(verb)),
            _ => true,
        })
        .collect();
    crate::shell::tidy_separators(kept)
}

/// Which "Context menu" setting toggles this entry (`workspace_menuprefs.rs`). Basic entries have
/// none and can't be hidden.
const fn pref_key(entry: &MenuEntry) -> Option<&'static str> {
    use MenuCommand as C;
    match entry {
        MenuEntry::Item(item) => match item.command {
            C::OpenInNewTab => Some("new_tab"),
            C::OpenInNewWindow => Some("new_window"),
            C::TogglePin => Some("pin"),
            C::OpenTerminal => Some("terminal"),
            C::Compress | C::ExtractToFolder | C::ExtractHere => Some("archive"),
            C::OpenWith(_) | C::ChooseApp => Some("open_with"),
            C::CopyPath => Some("copy_path"),
            C::CopyRelativePath => Some("relative_path"),
            C::SearchInFolder => Some("search_folder"),
            C::RevealInExplorer => Some("reveal"),
            C::Properties => Some("properties"),
            C::SetBackground => Some("set_background"),
            C::ImportSettings => Some("import_settings"),
            C::ViewDetails | C::ViewIcons | C::ViewColumns | C::Refresh => Some("view"),
            C::KeepOnDevice | C::FreeUpSpace => Some("cloud"),
            C::AddToShelf => Some("shelf"),
            _ => None,
        },
        MenuEntry::Submenu { shell: true, .. } => Some("more_options"),
        MenuEntry::Submenu { shell: false, .. } => Some("view"),
        MenuEntry::Actions(_) | MenuEntry::Separator => None,
    }
}

/// Removes entries disabled in settings, then tidies separators (none at the ends, no repeats).
/// Commands that need a file on this PC (Windows shell, other apps, archives, search index).
const fn local_only(command: &MenuCommand) -> bool {
    use MenuCommand as C;
    matches!(
        command,
        C::OpenTerminal
            | C::Compress
            | C::ExtractToFolder
            | C::ExtractHere
            | C::SetBackground
            | C::ImportSettings
            | C::RevealInExplorer
            | C::OpenInNewWindow
            | C::Properties
            | C::OpenWith(_)
            | C::Shell(_)
            | C::KeepOnDevice
            | C::FreeUpSpace
            | C::ChooseApp
            | C::OpenFileLocation
            | C::ToggleSearchNetwork
    )
}

/// The menu for items on a server: without local-only commands and "Show more options" (the
/// Windows menu needs local files).
pub(crate) fn remote_menu(entries: Vec<MenuEntry>) -> Vec<MenuEntry> {
    let kept = entries
        .into_iter()
        .filter(|e| match e {
            MenuEntry::Item(item) => !local_only(&item.command),
            MenuEntry::Submenu { shell, .. } => !shell,
            MenuEntry::Actions(_) | MenuEntry::Separator => true,
        })
        .collect();
    hide_entries(kept, &[])
}

fn hide_entries(entries: Vec<MenuEntry>, hidden: &[String]) -> Vec<MenuEntry> {
    let mut out: Vec<MenuEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if pref_key(&entry).is_some_and(|key| hidden.iter().any(|h| h == key)) {
            continue;
        }
        let separator = matches!(entry, MenuEntry::Separator);
        if separator && out.last().is_none_or(|e| matches!(e, MenuEntry::Separator)) {
            continue;
        }
        out.push(entry);
    }
    while out
        .last()
        .is_some_and(|e| matches!(e, MenuEntry::Separator))
    {
        out.pop();
    }
    out
}

/// Chevron at the right of submenu rows; points left when the submenu opens to the left.
fn submenu_chevron(left: bool) -> impl IntoElement {
    svg()
        .path(Icon::ChevronRight.path())
        .size(theme::ICON)
        .flex_none()
        .text_color(theme::text_subtle())
        .when(left, |svg| {
            svg.with_transformation(gpui::Transformation::rotate(gpui::radians(
                std::f32::consts::PI,
            )))
        })
}

/// Horizontal placement of a submenu: returns the left x and whether it opens to the right.
/// Prefers the parent's direction, flush with the parent. If that doesn't fit, goes past all
/// open menus on the other side. Only if neither fits, sits flush on the parent's other side,
/// which may cover an ancestor.
fn place_submenu(
    parent: (f32, f32),
    ancestors: &[(f32, f32)],
    width: f32,
    viewport: f32,
    prefer_right: bool,
) -> (f32, bool) {
    const MARGIN: f32 = 8.;
    const OVERLAP: f32 = 2.;
    let fits = |left: f32| left >= MARGIN && left + width <= viewport - MARGIN;
    let right_of_parent = parent.1 - OVERLAP;
    let left_of_parent = parent.0 + OVERLAP - width;
    let leftmost = ancestors.iter().map(|a| a.0).fold(parent.0, f32::min);
    let rightmost = ancestors.iter().map(|a| a.1).fold(parent.1, f32::max);
    let (same, beyond, back, right) = if prefer_right {
        (right_of_parent, leftmost - width, left_of_parent, true)
    } else {
        (left_of_parent, rightmost, right_of_parent, false)
    };
    if fits(same) {
        (same, right)
    } else if fits(beyond) {
        (beyond, !right)
    } else {
        (back, !right)
    }
}

/// Pin key of a Windows menu item: enclosing submenu names and its own, joined with " › ".
/// Quoted parts of a label (usually a file name, as in `Add to "photos.zip"`) become `…` so the
/// key still matches for other files.
pub(crate) fn pin_key(path: &[&str]) -> String {
    path.iter()
        .map(|label| {
            let mut out = String::with_capacity(label.len());
            let mut closing: Option<char> = None;
            for c in label.chars() {
                match closing {
                    Some(end) if c == end => {
                        out.push(c);
                        closing = None;
                    }
                    Some(_) => {}
                    None => {
                        out.push(c);
                        let end = match c {
                            '"' => Some('"'),
                            '“' => Some('”'),
                            // CJK corner brackets, used in localized labels on zh/ja systems.
                            '「' => Some('」'),
                            '『' => Some('』'),
                            _ => None,
                        };
                        if end.is_some() {
                            out.push('…');
                            closing = end;
                        }
                    }
                }
            }
            out.trim().to_string()
        })
        .collect::<Vec<_>>()
        .join(" › ")
}

/// Finds the item with pin key `key` in a Windows menu, including submenus.
fn find_pinned<'a>(entries: &'a [MenuEntry], key: &str) -> Option<&'a MenuItem> {
    entries.iter().find_map(|entry| match entry {
        MenuEntry::Item(item) if item.pin.as_deref() == Some(key) => Some(item),
        MenuEntry::Submenu { children, .. } => find_pinned(children, key),
        _ => None,
    })
}

/// Items for cloud folders: "Always keep on this device" (checked when all are, click again to
/// undo) and "Free up space", placed before the "Copy as path" section near the top like Windows
/// 11.
fn add_cloud_items(entries: &mut Vec<MenuEntry>, kept: bool) {
    let at = entries
        .iter()
        .position(|e| matches!(e, MenuEntry::Item(i) if i.command == MenuCommand::CopyPath))
        .unwrap_or(entries.len());
    let mut keep = item(t!("menu.keep_on_device"), None, MenuCommand::KeepOnDevice);
    if let MenuEntry::Item(row) = &mut keep {
        row.checked = kept;
        row.glyph = glyph_for(MenuCommand::KeepOnDevice);
    }
    let mut free = item(t!("menu.free_up_space"), None, MenuCommand::FreeUpSpace);
    if let MenuEntry::Item(row) = &mut free {
        row.glyph = glyph_for(MenuCommand::FreeUpSpace);
    }
    entries.splice(at..at, [keep, free, MenuEntry::Separator]);
}

fn more_options() -> MenuEntry {
    MenuEntry::Submenu {
        label: t!("menu.more_options").into(),
        enabled: true,
        icon: None,
        children: vec![placeholder(t!("menu.loading"))],
        shell: true,
    }
}

/// Zip file stem: a folder's name, or a file's name without extension (`report.docx` becomes
/// `report.zip`).
fn zip_stem_for(path: &std::path::Path, is_dir: bool) -> String {
    let stem = if is_dir {
        path.file_name()
    } else {
        path.file_stem().or_else(|| path.file_name())
    };
    stem.map_or_else(|| "archive".into(), |s| s.to_string_lossy().into_owned())
}

fn pin_label(pinned: bool) -> std::borrow::Cow<'static, str> {
    if pinned {
        t!("sidebar.unpin")
    } else {
        t!("sidebar.pin")
    }
}

/// Images usable as a background (formats gpui can draw).
fn is_backdrop_image(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ["png", "jpg", "jpeg", "bmp", "gif", "webp"]
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// Opens the folder in a new window by launching Arke again, on a background thread. In
/// background mode the running process takes it over, so it's fast.
fn open_in_new_window(path: PathBuf) {
    crate::explorer::spawn_thread("new-window", move || {
        let spawned = std::env::current_exe()
            .and_then(|exe| std::process::Command::new(exe).arg(&path).spawn());
        if let Err(err) = spawned {
            log::warn!("failed to open {} in a new window: {err}", path.display());
        }
    });
}

#[cfg(test)]
mod submenu_tests {
    use super::place_submenu;

    #[test]
    fn pin_keys_ignore_quoted_file_names() {
        use super::pin_key;
        assert_eq!(
            pin_key(&["7-Zip", "Add to \"photos.zip\""]),
            "7-Zip › Add to \"…\""
        );
        assert_eq!(
            pin_key(&["7-Zip", "Add to \"report.zip\""]),
            pin_key(&["7-Zip", "Add to \"photos.zip\""])
        );
        // zh-TW "Open with Notepad": CJK corner brackets quote the app name.
        assert_eq!(pin_key(&["以「記事本」開啟 "]), "以「…」開啟");
        // zh-CN quotes with curly double quotes: 7-Zip's "Add to photos.zip".
        assert_eq!(
            pin_key(&["7-Zip", "添加到“photos.zip”"]),
            pin_key(&["7-Zip", "添加到“report.zip”"])
        );
        assert_eq!(pin_key(&["添加到“photos.zip”"]), "添加到“…”");
        assert_eq!(pin_key(&["Send to", "Desktop"]), "Send to › Desktop");
    }

    #[test]
    fn submenus_never_fold_back_over_the_menus_above() {
        // Root menu spans 300..540, level 1 opens right at 538..778; window width 1000.
        let root = (300., 540.);
        assert_eq!(place_submenu(root, &[], 240., 1000., true), (538., true));
        // Level 2 doesn't fit on the right. Flush left of level 1 would cover the root, so it goes
        // left of all menus.
        let level1 = (538., 778.);
        assert_eq!(
            place_submenu(level1, &[root, level1], 240., 1000., true),
            (60., false)
        );
        // Doesn't fit on the left either: flush with the parent's left side.
        let tight = place_submenu(level1, &[(100., 340.), level1], 240., 1000., true);
        assert_eq!(tight, (300., false));
        // A chain opening left keeps going left.
        assert_eq!(
            place_submenu((400., 640.), &[(640., 880.)], 240., 1000., false),
            (162., false)
        );
    }
}
