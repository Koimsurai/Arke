//! Context menu, compression and extraction.
//!
//! The menu is plain GPUI and draws on the same frame as the right-click, without waiting for
//! the Shell. In parallel a separate process (`shell_host.rs`, the only place third-party shell
//! extensions get loaded) reads the Windows context menu. Its items land in the "Show more
//! options" submenu with icons and nested submenus, and a chosen item is sent back to that
//! process to run. No native Windows menu is ever shown.
//!
//! Commands pinned from "Show more options" to the top (or whole submenus, #88) don't wait for
//! the whole Windows menu: their extensions are read on their own beside it
//! (`explorer_menu_parts.rs`). Windows submenus that are filled only when opened are filled then
//! (`explorer_menu_lazy.rs`).
//!
//! Extraction runs on a background thread (`arke_core::archive`) with cancellable progress in the
//! status bar. The extracted folder is selected when done.

use rust_i18n::t;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use arke_core::archive::{
    self, ExtractOptions, ExtractProgress, ExtractSummary, archive_kind, folder_name_for,
};
use gpui::{
    AnimationExt, AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, Context, ElementId,
    FocusHandle, HighlightStyle, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent,
    MouseDownEvent, ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled,
    StyledText, Task, UnderlineStyle, WeakEntity, Window, actions, canvas, div, img, point,
    prelude::FluentBuilder, px, svg,
};

use super::ops::{
    CopyFiles, CutFiles, DeleteFiles, DeletePermanently, NewFolder, PasteFiles, RenameFile,
};
use super::{AppEvent, Explorer, ExplorerEvent, Refresh, ViewDetails, ViewMode, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::media::MenuIcon;
use crate::settings::SettingsStore;
use crate::shell::{OpProgress, ShellMenu, ShellMenuEntry};
use crate::win_menu::Access;

#[path = "explorer_menu_lazy.rs"]
mod lazy;
#[path = "explorer_menu_parts.rs"]
mod parts;
use crate::theme::{self, UiFont as _};
pub(crate) use parts::ShellMenuEvent;

actions!(
    explorer,
    [
        OpenContextMenu,
        MenuUp,
        MenuDown,
        MenuHome,
        MenuEnd,
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
        // The Menu key, which is what most keyboard and screen reader users press (#322).
        KeyBinding::new("menu", OpenContextMenu, Some("FileList")),
        KeyBinding::new("up", MenuUp, menu),
        KeyBinding::new("down", MenuDown, menu),
        KeyBinding::new("home", MenuHome, menu),
        KeyBinding::new("end", MenuEnd, menu),
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
    /// Delete's key with Shift: no Recycle Bin, after asking (Shift+Delete).
    DeleteForever,
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
    /// Split view: copy the selection to the other pane's folder (Alt+C).
    CopyToPeer,
    /// Split view: move the selection to the other pane's folder (Alt+M).
    MoveToPeer,
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
    /// Background menu "Sort by": sort by this column, as the Sort button's flyout does.
    SortBy(arke_core::SortColumn),
    /// Background menu "Sort by": ascending (`true`) or descending.
    SortDirection(bool),
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
    /// Open with the nth app the user added (see [`open_with_list`]).
    OpenWith(usize),
    /// Windows menu item read for "Show more options".
    Shell(u32),
    /// Item of one extension's menu, read on its own for a pinned command (its own session).
    ShellPart {
        session: u64,
        id: u32,
    },
    /// A pinned Windows command known to show here, drawn before its source has answered
    /// (#278), by index in the menu's `coming` list. Clicked, it runs once the source is in.
    PinPending(usize),
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
    /// Split view, from the pane row's "⋯": synchronized browsing on or off.
    ToggleSync,
    /// Split view, from the pane row's "⋯": compare the two folders, or stop.
    ToggleCompare,
}

/// An app listed under "Open with": one the user added in Settings › Context menu.
pub(crate) struct OpenWithApp {
    pub(crate) name: String,
    pub(crate) exe: PathBuf,
}

/// The apps listed under "Open with", in the order they were added.
pub(crate) fn open_with_list(settings: &crate::settings::Settings) -> Vec<OpenWithApp> {
    settings
        .open_with_extra
        .iter()
        .map(|exe| OpenWithApp {
            name: app_name(exe),
            exe: exe.clone(),
        })
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
        C::Cut | C::DropMove | C::MoveToPeer => Icon::Cut,
        C::Copy | C::CopyAddress | C::DropCopy | C::CopyToPeer => Icon::Copy,
        C::Paste => Icon::Paste,
        C::Rename | C::EditAddress => Icon::Rename,
        C::Delete | C::DeleteForever | C::ClearHistory => Icon::Trash,
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
        C::ToggleSync => Icon::Link,
        C::ToggleCompare => Icon::Compare,
        _ => return None,
    })
}

/// Adds icons to Arke's own items; items read from Windows bring their own.
/// A top-row button whose letter an enabled row of the same menu has keeps no letter: the key only
/// highlights the row (`access_hit`), so underlining it on the button would promise too much.
fn drop_shadowed_button_keys(entries: &mut [MenuEntry]) {
    let rows: Vec<Access> = entries
        .iter()
        .filter_map(|entry| match entry {
            MenuEntry::Item(i) if i.enabled => i.access,
            MenuEntry::Submenu {
                enabled: true,
                access,
                ..
            } => *access,
            _ => None,
        })
        .collect();
    for entry in entries {
        if let MenuEntry::Actions(items) = entry {
            for item in items {
                if item
                    .access
                    .is_some_and(|a| rows.iter().any(|r| r.matches(a.key)))
                {
                    item.access = None;
                }
            }
        }
    }
}

fn decorate(entries: &mut [MenuEntry]) {
    drop_shadowed_button_keys(entries);
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

#[allow(
    clippy::struct_excessive_bools,
    reason = "Each bool is an independent row style (enabled, checked, bold, danger); an enum would read worse"
)]
#[derive(Clone)]
pub(crate) struct MenuItem {
    pub(crate) label: SharedString,
    /// The key that picks it while the menu is open (`&O` in its text).
    access: Option<Access>,
    shortcut: Option<&'static str>,
    pub(crate) command: MenuCommand,
    pub(crate) enabled: bool,
    icon: Option<MenuIcon>,
    /// Arke's own icon (`icon` is the one carried by Windows menu items).
    glyph: Option<Icon>,
    checked: bool,
    /// Default command, drawn bold.
    bold: bool,
    /// Error message, drawn in the danger color rather than the faint disabled gray.
    danger: bool,
    /// Windows menu items: key used to pin it to the main menu (see [`pin_key`]).
    pin: Option<String>,
    /// Windows menu items: the canonical verb, which says where it comes from (a registry
    /// command's verb, a packaged app's command's `{CLSID}`; most extensions' items have none).
    verb: Option<String>,
}

#[derive(Clone)]
pub(crate) enum MenuEntry {
    Item(MenuItem),
    Submenu {
        label: SharedString,
        access: Option<Access>,
        enabled: bool,
        icon: Option<MenuIcon>,
        /// Arke's own icon for its own submenus (`icon` is the one Windows gives its submenus).
        glyph: Option<Icon>,
        children: Vec<Self>,
        /// "Show more options" and the submenus read from Windows inside it; `false` for Arke's
        /// own View and New submenus.
        shell: bool,
        /// A Windows submenu filled only when it is opened (`explorer_menu_lazy.rs`).
        lazy: Option<lazy::LazySubmenu>,
        /// Arke's own submenus: which entry of Settings › Context menu it is (`view`, `new`).
        key: Option<&'static str>,
        /// Windows submenus: key used to pin the whole submenu to the main menu (see
        /// [`pin_key`]), as for an item (#88).
        pin: Option<String>,
    },
    /// Row of icon buttons (a folder's new folder, cut, copy, rename, delete) at the bottom, as in
    /// the Windows 11 menu.
    Actions(Vec<MenuItem>),
    Separator,
}

impl MenuEntry {
    const fn selectable(&self) -> bool {
        match self {
            Self::Item(i) => i.enabled,
            Self::Submenu { enabled, .. } => *enabled,
            // Icon buttons are mouse-only; the keyboard has Ctrl+X, Ctrl+C, F2, Delete and "New".
            Self::Actions(_) | Self::Separator => false,
        }
    }

    const fn has_mark(&self) -> bool {
        match self {
            Self::Item(i) => i.icon.is_some() || i.glyph.is_some() || i.checked,
            Self::Submenu { icon, glyph, .. } => icon.is_some() || glyph.is_some(),
            Self::Actions(_) | Self::Separator => false,
        }
    }

    /// Converts a Windows menu entry (icons become GPUI images). `parent` holds enclosing submenu
    /// names, for the pin key.
    /// `session` is set for an item from one extension's menu (read on its own), which runs in
    /// that menu rather than the whole Windows menu.
    fn from_shell_in(entry: ShellMenuEntry, parent: &[&str], session: Option<u64>) -> Self {
        let icon = |t: Option<arke_core::thumbs::Thumbnail>| {
            t.and_then(crate::media::to_render_image).map(MenuIcon::new)
        };
        match entry {
            ShellMenuEntry::Item {
                id,
                verb,
                label,
                access,
                enabled,
                checked,
                default,
                icon: i,
            } => {
                let mut path = parent.to_vec();
                path.push(&label);
                let pin = Some(pin_key(&path));
                Self::Item(MenuItem {
                    label: label.into(),
                    access,
                    shortcut: None,
                    command: match session {
                        Some(session) => MenuCommand::ShellPart { session, id },
                        None => MenuCommand::Shell(id),
                    },
                    enabled,
                    icon: icon(i),
                    glyph: None,
                    checked,
                    bold: default,
                    danger: false,
                    pin,
                    verb,
                })
            }
            ShellMenuEntry::Submenu {
                label,
                access,
                enabled,
                icon: i,
                children,
                lazy: index,
            } => {
                let mut path = parent.to_vec();
                path.push(&label);
                let pin = Some(pin_key(&path));
                let lazy = index.map(|index| lazy::LazySubmenu::new(session, index));
                let children = if lazy.is_some() && children.is_empty() {
                    vec![placeholder(t!("menu.loading"))]
                } else {
                    children
                        .into_iter()
                        .map(|c| Self::from_shell_in(c, &path, session))
                        .collect()
                };
                Self::Submenu {
                    label: label.into(),
                    access,
                    enabled,
                    icon: icon(i),
                    glyph: None,
                    children,
                    shell: true,
                    lazy,
                    key: None,
                    pin,
                }
            }
            ShellMenuEntry::Separator => Self::Separator,
        }
    }
}

/// The text of one of Arke's own menu rows, and which `access.*` letter it takes (none for text that
/// isn't ours to key, like "Loading…" or an app's name).
pub(crate) struct Label {
    text: SharedString,
    name: Option<&'static str>,
    /// Text put into the label from outside (a file name): not searched for the key.
    skip: Option<String>,
}

impl<T: Into<SharedString>> From<T> for Label {
    fn from(text: T) -> Self {
        Self {
            text: text.into(),
            name: None,
            skip: None,
        }
    }
}

impl Label {
    /// Its access key, if it takes one.
    fn access(&self) -> Option<Access> {
        let name = self.name?;
        own_access(name, &self.text, self.skip.as_deref())
    }
}

/// `text` as the row `name` of the `access.*` letters.
fn lab(name: &'static str, text: impl Into<SharedString>) -> Label {
    Label {
        text: text.into(),
        name: Some(name),
        skip: None,
    }
}

/// [`lab`] for a label with `put_in` (a name) inside it, which is not searched for the key.
fn lab_with(name: &'static str, text: impl Into<SharedString>, put_in: &str) -> Label {
    Label {
        skip: Some(put_in.to_owned()),
        ..lab(name, text)
    }
}

/// Whether `locale` writes the key after the text, as `(C)`, not under a letter of it.
fn writes_key_after(locale: &str) -> bool {
    locale.starts_with("zh") || locale.starts_with("ja")
}

/// The access key for `letter` in `label`: its first occurrence there, compared without case and
/// outside `skip`, which is where it is underlined. When the letter isn't in the label (or the
/// language writes the key after the text), it goes after it as `(C)`, which is still right.
fn derive_access(letter: char, label: &str, skip: Option<&str>, after: bool) -> Access {
    if !after {
        let hole = skip.and_then(|s| label.find(s).map(|at| at..at + s.len()));
        let found = label.char_indices().find(|(at, c)| {
            c.to_lowercase().eq(letter.to_lowercase())
                && !hole.as_ref().is_some_and(|h| h.contains(at))
        });
        if let Some((at, key)) = found {
            return Access { key, at: Some(at) };
        }
    }
    Access {
        key: letter.to_uppercase().next().unwrap_or(letter),
        at: None,
    }
}

/// The access key of row `name` with `label`, under `locale`'s `access.*` letter; `None` where it
/// has none.
fn access_in(locale: &str, name: &str, label: &str, skip: Option<&str>) -> Option<Access> {
    let key = format!("access.{name}");
    let letter = t!(key.as_str(), locale = locale).chars().next()?;
    letter
        .is_alphanumeric()
        .then(|| derive_access(letter, label, skip, writes_key_after(locale)))
}

fn own_access(name: &str, label: &str, skip: Option<&str>) -> Option<Access> {
    access_in(&rust_i18n::locale(), name, label, skip)
}

fn plain(
    label: Label,
    shortcut: Option<&'static str>,
    command: MenuCommand,
    enabled: bool,
) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        access: label.access(),
        ..text_row(label.text, shortcut, command, enabled)
    })
}

/// A row that is only text, with no access key (a placeholder, an error).
const fn text_row(
    label: SharedString,
    shortcut: Option<&'static str>,
    command: MenuCommand,
    enabled: bool,
) -> MenuItem {
    MenuItem {
        label,
        access: None,
        shortcut,
        command,
        enabled,
        icon: None,
        glyph: None,
        checked: false,
        bold: false,
        danger: false,
        pin: None,
        verb: None,
    }
}

/// One button in the icon row.
fn action(label: impl Into<Label>, command: MenuCommand, enabled: bool) -> MenuItem {
    let label = label.into();
    MenuItem {
        access: label.access(),
        ..text_row(label.text, None, command, enabled)
    }
}

fn item(
    label: impl Into<Label>,
    shortcut: Option<&'static str>,
    command: MenuCommand,
) -> MenuEntry {
    plain(label.into(), shortcut, command, true)
}

/// Row with a check mark when `checked` (the current view, an option that is on).
fn checked_item(
    label: impl Into<Label>,
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
    MenuEntry::Item(text_row(
        label.into(),
        None,
        MenuCommand::MoreOptions,
        false,
    ))
}

/// Error row, e.g. failed to load. Not clickable, but drawn in the danger color since disabled gray
/// is too faint.
fn error_line(label: impl Into<SharedString>) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        danger: true,
        ..text_row(label.into(), None, MenuCommand::MoreOptions, false)
    })
}

fn item_if(
    enabled: bool,
    label: impl Into<Label>,
    shortcut: Option<&'static str>,
    command: MenuCommand,
) -> MenuEntry {
    plain(label.into(), shortcut, command, enabled)
}

/// Positions known only after paint: bounds of every row and menu level, each in the window of
/// its own level. Submenus are placed beside their row.
#[derive(Default)]
struct MenuGeometry {
    rows: HashMap<usize, Bounds<Pixels>>,
    panels: HashMap<usize, Bounds<Pixels>>,
}

/// Whether access keys are drawn underlined, by the Windows rule: not in a menu opened with the
/// mouse, until a key is pressed in it; from the start in one opened from the keyboard, or when
/// Windows is set to always underline them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cues(bool);

impl Cues {
    /// A menu just opened, from the keyboard or not, with Windows' "always underline" as it is.
    pub(crate) const fn opened(from_keyboard: bool, always: bool) -> Self {
        Self(from_keyboard || always)
    }

    pub(crate) const fn shown(self) -> bool {
        self.0
    }

    /// The keyboard is in use in the menu.
    pub(crate) const fn pressed(&mut self) {
        self.0 = true;
    }
}

/// What a letter typed in a menu level comes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessHit {
    /// No enabled item has that key.
    Nothing,
    /// One item or icon button has it: run it.
    Run(MenuCommand),
    /// One submenu has it: open it (its row).
    Open(usize),
    /// Several rows have it: the highlight goes to this one, the next after the highlighted row.
    Move(usize),
}

/// Looks `typed` up among the access keys of `entries` (a menu level), whose row `highlighted` is
/// highlighted. Disabled rows are skipped. Icon buttons count too, but only a lone match runs
/// them: the highlight can't rest on a button.
pub(crate) fn access_hit(
    entries: &[MenuEntry],
    highlighted: Option<usize>,
    typed: char,
) -> AccessHit {
    let has = |access: Option<Access>| access.is_some_and(|a| a.matches(typed));
    let mut rows: Vec<usize> = Vec::new();
    let mut buttons: Vec<MenuCommand> = Vec::new();
    for (ix, entry) in entries.iter().enumerate() {
        match entry {
            MenuEntry::Item(i) if i.enabled && has(i.access) => rows.push(ix),
            MenuEntry::Submenu {
                enabled: true,
                access,
                ..
            } if has(*access) => rows.push(ix),
            MenuEntry::Actions(items) => buttons.extend(
                items
                    .iter()
                    .filter(|i| i.enabled && has(i.access))
                    .map(|i| i.command),
            ),
            _ => {}
        }
    }
    match (rows.as_slice(), buttons.as_slice()) {
        ([], [command]) => AccessHit::Run(*command),
        ([ix], []) => match &entries[*ix] {
            MenuEntry::Item(i) => AccessHit::Run(i.command),
            _ => AccessHit::Open(*ix),
        },
        ([first, ..], _) => {
            let next = highlighted
                .and_then(|h| rows.iter().copied().find(|&ix| ix > h))
                .unwrap_or(*first);
            AccessHit::Move(next)
        }
        _ => AccessHit::Nothing,
    }
}

/// `hit` with Shift held: Delete's key then deletes permanently, as Shift+Delete does. Every other
/// key is the same with or without it.
const fn shift_hit(hit: AccessHit, shift: bool) -> AccessHit {
    match hit {
        AccessHit::Run(MenuCommand::Delete) if shift => AccessHit::Run(MenuCommand::DeleteForever),
        other => other,
    }
}

/// The command a click runs: Shift+click on Delete deletes permanently, as in File Explorer.
const fn click_command(command: MenuCommand, shift: bool) -> MenuCommand {
    match command {
        MenuCommand::Delete if shift => MenuCommand::DeleteForever,
        other => other,
    }
}

/// A row's text with its access key underlined, when the underlines are shown. The CJK form
/// writes the key after the text, as `(O)`, which is there whether underlined or not.
fn access_text(label: SharedString, access: Option<Access>, cues: Cues) -> AnyElement {
    let Some(access) = access else {
        return label.into_any_element();
    };
    let (text, at) = drawn_text(label, access);
    let end = at + access.key.len_utf8();
    if !cues.shown() || text.get(at..end).is_none() {
        return text.into_any_element();
    }
    let underline = HighlightStyle {
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        }),
        ..HighlightStyle::default()
    };
    StyledText::new(text)
        .with_highlights([(at..end, underline)])
        .into_any_element()
}

/// The text a row with `access` draws, and where the key is in it: the label, with the key
/// after it as `(O)` in the CJK form.
fn drawn_text(label: SharedString, access: Access) -> (SharedString, usize) {
    if let Some(at) = access.at {
        (label, at)
    } else {
        let at = label.len() + 1;
        (format!("{label}({})", access.key).into(), at)
    }
}

/// The key an icon button underlines in its label: only one that is a letter of it. The CJK form,
/// `(N)` after the text, would crowd the small buttons, so [`button_key`] shows that one instead
/// (#422).
const fn button_text_access(access: Option<Access>) -> Option<Access> {
    match access {
        Some(Access { at: Some(_), .. }) => access,
        _ => None,
    }
}

/// The key an icon button shows beside its icon: one that isn't a letter of its label, and only
/// while the underlines are shown.
const fn button_key(access: Option<Access>, cues: Cues) -> Option<char> {
    match access {
        Some(Access { key, at: None }) if cues.shown() => Some(key),
        _ => None,
    }
}

/// [`button_key`] drawn as a small muted letter beside the top of the icon button's icon, like a
/// superscript, with no box. It is laid over the button, so the button keeps its size and nothing
/// moves when it comes and goes. Its place is set against the icon (a box the icon's size): the
/// gap after it plus the widest letter (about 10 px) fit in the space beside the icon even in the
/// narrowest button, about 45 px with five in a 240 px menu, so it never reaches the next button.
fn key_badge(key: char, enabled: bool, command: MenuCommand) -> AnyElement {
    div()
        .debug_selector(move || format!("menu-action-key-{command:?}"))
        .absolute()
        .top(-theme::s_2())
        .left(theme::icon() + theme::s_4())
        .text_size(theme::fs_11())
        .line_height(gpui::relative(theme::LH_TIGHT))
        .text_color(if enabled {
            theme::text_muted()
        } else {
            theme::text_disabled()
        })
        .child(key.to_string())
        .into_any_element()
}

/// The window a menu level is drawn in (`popup.rs`), and the open submenus that lead to it.
struct LevelPopup {
    path: Vec<usize>,
    state: Rc<crate::popup::PopupState>,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about one open menu"
)]
pub(crate) struct ContextMenu {
    position: Point<Pixels>,
    /// The window the menu belongs to; its levels are popups owned by it.
    owner: Option<AnyWindowHandle>,
    /// `position` on the screen (physical pixels), where the top level opens.
    at: (i32, i32),
    /// The levels' windows, top level first.
    popups: Vec<LevelPopup>,
    group: crate::popup::Group,
    /// Levels are being opened (after this update).
    opening: bool,
    /// A level's window could not be made: not tried again for this menu.
    popup_failed: bool,
    /// Closes the menu when its window stops being the active one, or moves.
    watch: Vec<gpui::Subscription>,
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
    /// The request the whole Windows menu came in for, once it is in: a command pinned from it
    /// learns its source at once (`toggle_shell_pin`).
    whole_request: Option<u64>,
    /// The whole Windows menu came in only partly read ([`ShellMenu::partial`]): a pinned command
    /// missing from it may still be there, so it doesn't decide which pinned rows go (#278).
    whole_partial: bool,
    /// "Send to" as the menu host lists it (label, targets), put in place of Windows' own under
    /// "Show more options", which fills slowly and was often cut short (#278).
    send_to: Option<(SharedString, Vec<MenuEntry>)>,
    /// Submenus of the whole Windows menu left still filling ([`ShellMenu::filling`]), by number:
    /// one that comes back with nothing to show goes (#278).
    left_filling: Vec<u32>,
    /// Top-level row index of the "Show more options" submenu.
    shell_slot: Option<usize>,
    /// "More options" ends with the one-time line about its pins; set once it has been shown.
    pin_hint: bool,
    pin_hint_shown: std::cell::Cell<bool>,
    /// Number of pinned Windows commands placed directly above "Show more options".
    pinned_rows: usize,
    /// Single extensions' menus read for pinned commands, by session, while they are held.
    parts: Vec<(u64, Vec<MenuEntry>)>,
    /// Id those were asked for with (the whole menu's request).
    parts_request: Option<u64>,
    /// Opened with Shift held: the Windows menu includes its Shift-only commands (#272).
    pub(super) extended: bool,
    /// Pinned commands known to show here (registry and packaged-app commands), drawn as soon
    /// as that is known, before their sources answer, so the menu doesn't move (#278).
    pub(super) coming: Vec<String>,
    /// A pinned row clicked before its source answered: run when it is in.
    pending: Option<String>,
    /// Differs per menu open; used as the animation id so only a new menu replays the entrance.
    serial: u64,
    /// Last input was keyboard: when submenu items arrive late, keyboard users get the first item
    /// selected.
    keyboard: bool,
    /// Whether the access keys are drawn underlined.
    cues: Cues,
    geometry: Rc<RefCell<MenuGeometry>>,
    /// Scroll position of each level: a level taller than the window scrolls (#114).
    scrolls: RefCell<Vec<gpui::ScrollHandle>>,
    hover_task: Option<Task<()>>,
}

/// A menu made on a right press, before it shows on release: what comes back for it meanwhile
/// waits here (#278).
pub(crate) struct EarlyMenu {
    menu: ContextMenu,
    events: Vec<ShellMenuEvent>,
    made: std::time::Instant,
}

/// An early menu older than this isn't shown: the press was let go somewhere unheard of.
const EARLY_MENU_FOR: Duration = Duration::from_secs(3);

impl EarlyMenu {
    /// Whether `event` is for this menu's Windows menu or its sources.
    fn wants(&self, event: &ShellMenuEvent) -> bool {
        match event {
            ShellMenuEvent::Whole { request, .. } => self.menu.shell_request == Some(*request),
            ShellMenuEvent::Coming { request, .. } | ShellMenuEvent::Part { request, .. } => {
                self.menu.parts_request == Some(*request)
            }
            _ => false,
        }
    }
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
            owner: None,
            at: (0, 0),
            popups: Vec::new(),
            group: crate::popup::Group::default(),
            opening: false,
            popup_failed: false,
            watch: Vec::new(),
            entries,
            open: Vec::new(),
            highlighted: None,
            focus,
            targets,
            shell_session: None,
            shell_request: None,
            whole_partial: false,
            send_to: None,
            left_filling: Vec::new(),
            shell_slot: None,
            pin_hint: false,
            pin_hint_shown: std::cell::Cell::new(false),
            pinned_rows: 0,
            parts: Vec::new(),
            parts_request: None,
            whole_request: None,
            extended: false,
            coming: Vec::new(),
            pending: None,
            serial: NEXT_MENU.fetch_add(1, Ordering::Relaxed),
            keyboard: false,
            cues: Cues::default(),
            geometry: Rc::default(),
            scrolls: RefCell::default(),
            hover_task: None,
        }
    }

    /// The scroll handle of `level`, made on first use.
    fn scroll(&self, level: usize) -> gpui::ScrollHandle {
        let mut scrolls = self.scrolls.borrow_mut();
        if scrolls.len() <= level {
            scrolls.resize_with(level + 1, gpui::ScrollHandle::new);
        }
        scrolls[level].clone()
    }

    /// Scrolls row `ix` of `level` into view when next drawn. The rows of the pinned footer
    /// ([`footer_start`]) are always in view; the others are the children of the level's scroll
    /// area, one per entry.
    fn scroll_row_into_view(&self, level: usize, ix: usize) {
        if ix < footer_start(self.level(level)) {
            self.scroll(level).scroll_to_item(ix);
        }
    }

    /// Row `ix` of the innermost level highlighted from the keyboard, scrolled into view.
    fn highlight_by_key(&mut self, ix: usize) {
        self.highlighted = Some(ix);
        self.keyboard = true;
        self.cues.pressed();
        self.scroll_row_into_view(self.open.len(), ix);
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
    /// Pinned rows drawn ahead (`coming`) whose source turned out not to have them: no longer
    /// drawn, and a click waiting on one is dropped.
    pub(super) fn drop_coming(&mut self, keys: &[String]) {
        self.coming.retain(|k| !keys.contains(k));
        if self.pending.as_ref().is_some_and(|p| keys.contains(p)) {
            self.pending = None;
        }
    }

    /// Puts the host's own "Send to" under "Show more options" in place of Windows', once both
    /// are in. Its rows run in their own menu (their session), like a pinned command's.
    pub(super) fn apply_send_to(&mut self) {
        let Some((label, targets)) = &self.send_to else {
            return;
        };
        let Some(MenuEntry::Submenu { children, .. }) =
            self.shell_slot.and_then(|ix| self.entries.get_mut(ix))
        else {
            return;
        };
        for entry in children.iter_mut() {
            if let MenuEntry::Submenu {
                label: l,
                children: c,
                lazy,
                ..
            } = entry
                && l == label
            {
                c.clone_from(targets);
                *lazy = None;
            }
        }
    }

    pub(super) fn sync_pinned(&mut self, pinned: &[String]) {
        let Some(slot) = self.shell_slot else {
            return;
        };
        let start = slot - self.pinned_rows;
        let whole_in = self.shell_session.is_some() && !self.whole_partial;
        let mut found: Vec<MenuEntry> = Vec::new();
        if let Some(MenuEntry::Submenu { children, .. }) = self.entries.get(slot) {
            // From the whole Windows menu once it's in, until then from the sources read on
            // their own, and for the ones known to be coming, their row ahead of them. A pinned
            // submenu comes whole, its rows filled the same way as under "More options" (#88).
            for key in pinned {
                if let Some(entry) = find_pinned(children, key).or_else(|| {
                    self.parts
                        .iter()
                        .find_map(|(_, entries)| find_pinned(entries, key))
                }) {
                    remember_pin(key, entry);
                    found.push(entry.clone());
                } else if !whole_in
                    && let Some(ix) = self.coming.iter().position(|k| k == key)
                    && let Some(row) = coming_row(key, ix)
                {
                    found.push(row);
                }
            }
        }
        // The pinned rows before, by key, so an open pinned submenu or the highlight stays on
        // its row.
        let before: Vec<Option<String>> = self.entries[start..slot]
            .iter()
            .map(|e| entry_pin(e).map(str::to_owned))
            .collect();
        let after: Vec<Option<String>> = found
            .iter()
            .map(|e| entry_pin(e).map(str::to_owned))
            .collect();
        let added = found.len();
        self.entries.splice(start..slot, found);
        let new_slot = start + added;
        // Where a row now is; `None` for a pinned row that went away.
        let shift = |ix: usize| {
            if ix >= slot {
                Some(ix + new_slot - slot)
            } else if ix >= start {
                let key = before.get(ix - start).cloned().flatten();
                key.and_then(|key| after.iter().position(|k| k.as_ref() == Some(&key)))
                    .map(|at| start + at)
            } else {
                Some(ix)
            }
        };
        if let Some(&first) = self.open.first() {
            if let Some(first) = shift(first) {
                self.open[0] = first;
            } else {
                // Its pinned submenu went away: closed.
                self.open.clear();
                self.highlighted = Some(start);
            }
        } else {
            // Highlight was on a pinned row that went away: on the first pinned row (or "Show
            // more options").
            self.highlighted = self.highlighted.map(|ix| shift(ix).unwrap_or(start));
        }
        self.shell_slot = Some(new_slot);
        self.pinned_rows = added;
        // The keyboard went into a pinned submenu while it still showed "Loading…": its first
        // row once it has one.
        if self.keyboard
            && self.highlighted.is_none()
            && self.open.len() == 1
            && (start..new_slot).contains(&self.open[0])
        {
            self.highlighted = self.level(1).iter().position(MenuEntry::selectable);
        }
    }

    /// Test helper: highlight top-level row `ix`, as the keyboard would.
    #[cfg(test)]
    pub(crate) const fn highlight_for_test(&mut self, ix: usize) {
        self.highlighted = Some(ix);
        self.keyboard = true;
    }

    /// Test helper: the shortcut shown on the top-level item labeled `label`.
    #[cfg(test)]
    pub(crate) fn shortcut_of(&self, label: &str) -> Option<&'static str> {
        self.entries.iter().find_map(|e| match e {
            MenuEntry::Item(i) if i.label.as_ref() == label => i.shortcut,
            _ => None,
        })
    }

    /// Test helper: the shortcut shown on row `ix` of `level`.
    #[cfg(test)]
    pub(crate) fn shortcut_at(&self, level: usize, ix: usize) -> Option<&'static str> {
        match self.level(level).get(ix)? {
            MenuEntry::Item(i) => i.shortcut,
            _ => None,
        }
    }

    /// Test helper: the pin key of row `ix` of `level`, an item's or a whole submenu's (the row
    /// has a pin button).
    #[cfg(test)]
    pub(crate) fn pin_at(&self, level: usize, ix: usize) -> Option<String> {
        self.level(level)
            .get(ix)
            .and_then(entry_pin)
            .map(str::to_owned)
    }

    /// Test helper: checked items at the top level.
    #[cfg(test)]
    pub(crate) fn checked_labels(&self) -> Vec<String> {
        self.checked_labels_at(0)
    }

    /// Test helper: checked items at `level`.
    #[cfg(test)]
    pub(crate) fn checked_labels_at(&self, level: usize) -> Vec<String> {
        self.level(level)
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) if i.checked => Some(i.label.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Test helper: the access key of the top-level row `label`, and its text as drawn (the CJK
    /// form with the key after it, as `(N)`).
    #[cfg(test)]
    pub(crate) fn key_of(&self, label: &str) -> Option<(char, String)> {
        self.level(0).iter().find_map(|entry| match entry {
            MenuEntry::Item(row) if row.label.as_ref() == label => row
                .access
                .map(|access| (access.key, drawn_text(row.label.clone(), access).0.to_string())),
            _ => None,
        })
    }

    /// Test helper: labels at the top level (separators as `—`).
    #[cfg(test)]
    pub(crate) fn labels(&self) -> Vec<String> {
        self.labels_at(0)
    }

    /// Test helper: the items of Windows submenu `label` under "Show more options", and whether
    /// it is still left to fill when opened.
    #[cfg(test)]
    pub(crate) fn shell_submenu_for_test(&self, label: &str) -> Option<(Vec<String>, bool)> {
        let MenuEntry::Submenu { children, .. } =
            self.shell_slot.and_then(|ix| self.entries.get(ix))?
        else {
            return None;
        };
        children.iter().find_map(|e| match e {
            MenuEntry::Submenu {
                label: l,
                children,
                lazy,
                ..
            } if l.as_ref() == label => Some((
                children
                    .iter()
                    .filter_map(|c| match c {
                        MenuEntry::Item(i) => Some(i.label.to_string()),
                        _ => None,
                    })
                    .collect(),
                lazy.is_some(),
            )),
            _ => None,
        })
    }

    /// Test helper: the labels of the items right under "Show more options".
    #[cfg(test)]
    pub(crate) fn shell_items_for_test(&self) -> Vec<String> {
        let Some(MenuEntry::Submenu { children, .. }) =
            self.shell_slot.and_then(|ix| self.entries.get(ix))
        else {
            return Vec::new();
        };
        children
            .iter()
            .filter_map(|c| match c {
                MenuEntry::Item(i) => Some(i.label.to_string()),
                _ => None,
            })
            .collect()
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

    /// Test helper: whether the access keys are drawn underlined.
    #[cfg(test)]
    pub(crate) const fn cues_shown_for_test(&self) -> bool {
        self.cues.shown()
    }

    /// Test helper: the icon buttons of the innermost level, each with the text it draws and the
    /// key shown beside its icon.
    #[cfg(test)]
    pub(crate) fn icon_row_for_test(&self) -> Vec<(String, Option<char>)> {
        self.level(self.open.len())
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Actions(items) => Some(items),
                _ => None,
            })
            .flatten()
            .map(|i| {
                let text = button_text_access(i.access)
                    .map_or_else(|| i.label.clone(), |a| drawn_text(i.label.clone(), a).0);
                (text.to_string(), button_key(i.access, self.cues))
            })
            .collect()
    }

    /// Test helper: where `level` was drawn, and how far it is scrolled.
    #[cfg(test)]
    pub(crate) fn panel_for_test(&self, level: usize) -> (Option<Bounds<Pixels>>, Pixels) {
        (
            self.geometry.borrow().panels.get(&level).copied(),
            self.scroll(level).offset().y,
        )
    }

    /// Test helper: how far the rows of `level` are scrolled, how far they can be, and the box
    /// they scroll in (in that level's window).
    #[cfg(test)]
    pub(crate) fn rows_scroll_for_test(&self, level: usize) -> (Pixels, Pixels, Bounds<Pixels>) {
        let scroll = self.scroll(level);
        (scroll.offset().y, scroll.max_offset().y, scroll.bounds())
    }

    /// Test helper: where row `ix` of `level` is drawn now, as scrolled (in that level's window).
    /// `None` for the buttons kept below the rows, or before it is drawn.
    #[cfg(test)]
    pub(crate) fn row_drawn_for_test(&self, level: usize, ix: usize) -> Option<Bounds<Pixels>> {
        let scroll = self.scroll(level);
        let mut bounds = scroll.bounds_for_item(ix)?;
        bounds.origin.y += scroll.offset().y;
        Some(bounds)
    }

    /// Test helper: the rows of `level` the keys can highlight, and the first entry of the part
    /// kept in view below the rows.
    #[cfg(test)]
    pub(crate) fn rows_for_test(&self, level: usize) -> (Vec<usize>, usize) {
        let entries = self.level(level);
        let selectable = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.selectable())
            .map(|(ix, _)| ix)
            .collect();
        (selectable, footer_start(entries))
    }

    /// Test helper: the highlighted row of the innermost level.
    #[cfg(test)]
    pub(crate) const fn highlighted_for_test(&self) -> Option<usize> {
        self.highlighted
    }

    /// Test helper: where submenu row `ix` of `level` was drawn (in that level's window).
    #[cfg(test)]
    pub(crate) fn submenu_row_for_test(&self, level: usize, ix: usize) -> Option<Bounds<Pixels>> {
        self.geometry
            .borrow()
            .rows
            .get(&(level * LEVEL_STRIDE + ix))
            .copied()
    }

    /// Test helper: whether `level` opened to the left of the level before it.
    #[cfg(test)]
    pub(crate) fn opens_left_for_test(&self, level: usize) -> bool {
        self.popups.get(level).is_some_and(|p| p.state.opens_left())
    }

    /// Test helper: the window `level` is drawn in, and where its panel was placed on the screen.
    #[cfg(test)]
    pub(crate) fn popup_for_test(
        &self,
        level: usize,
    ) -> Option<(AnyWindowHandle, Option<crate::popup::ScreenRect>)> {
        let popup = self.popups.get(level)?;
        Some((popup.state.window()?, popup.state.placed()))
    }
}

pub(crate) struct ExtractJob {
    pub(crate) name: String,
    /// What is extracted, and whether into the folder it is in: asked again for its password.
    archive: Option<(PathBuf, bool)>,
    /// Compressing rather than extracting; changes the banner text.
    pub(crate) compress: bool,
    /// Progress center job. Cancelling sets its `cancel`; the progress center and banner share one
    /// cancel button.
    job: u64,
    ops: Arc<crate::shell::OpProgress>,
    pub(crate) progress: ExtractProgress,
    /// The folder it makes its new folder or zip in, and that name (lowercase): taken for the jobs
    /// queued after it, before it exists.
    claim: Option<(PathBuf, String)>,
}

/// The file's name says it is an archive Arke extracts.
fn is_archive(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| archive_kind(&name.to_string_lossy()).is_some())
}

/// A new tab's key in the archive queue: unique in the process (entity ids start over in every
/// test app, and those share the queue).
pub(crate) fn next_archive_line() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The progress callback an archive job's thread passes to `arke_core::archive`.
type ArchiveProgress<'a> = dyn Fn(ExtractProgress) + Sync + 'a;

/// How many threads to extract `archive` into `folder` with: several only when both are on
/// solid-state drives, where writing many small files at once is faster; one on a hard disk, a
/// network share, or when that can't be told. Background thread.
fn archive_workers(shell: &dyn crate::shell::Shell, archive: &Path, folder: &Path) -> usize {
    if shell.solid_state(archive) && shell.solid_state(folder) {
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
    } else {
        1
    }
}

/// Writes background compress/extract progress (items, bytes) into the progress center entry.
fn report(ops: &crate::shell::OpProgress, p: ExtractProgress) {
    ops.done.store(p.files, Ordering::Relaxed);
    ops.work_in_bytes.store(true, Ordering::Relaxed);
    ops.work_done.store(p.done, Ordering::Relaxed);
    ops.work_total.store(p.total, Ordering::Relaxed);
}

/// The archive is fine but compressed in a way Arke has no reader for.
const fn can_not_read(e: &archive::ArchiveError) -> bool {
    matches!(
        e,
        archive::ArchiveError::UnsupportedMethod { .. }
            | archive::ArchiveError::SystemUnsupported { .. }
            | archive::ArchiveError::SystemReaderMissing { .. }
    )
}

/// The extraction stopped for a password: none was given, or it was wrong.
const fn needs_password(e: &archive::ArchiveError) -> bool {
    matches!(
        e,
        archive::ArchiveError::Encrypted | archive::ArchiveError::WrongPassword
    )
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
            self.search_right_press(ix);
        } else if let Some(view) = self.view()
            && let Some(entry) = view.get(ix)
        {
            if !self.selection.contains(&entry.name) {
                self.selection.select_only(&view, ix);
            }
            self.cursor = Some(ix);
        }
        // The menu opens on release (`right_press_released`), so the press can still become a
        // right-button drag. It is made now, so what it reads from Windows is under way (#278).
        self.note_right_press(event.position, cx);
        if !self.search_mode {
            self.make_early_menu(event.modifiers.shift, cx);
        }
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
        // Opened from the keyboard: the access keys are underlined from the start.
        if let Some(menu) = self.context_menu.as_mut() {
            menu.cues = Cues::opened(true, menu.cues.shown());
        }
    }

    /// Test helper: Shift+F10.
    #[cfg(test)]
    pub(crate) fn open_context_menu_by_keyboard_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_context_menu_action(&OpenContextMenu, window, cx);
    }

    /// For targets in cloud folders like OneDrive: whether all are "Always keep on this device".
    /// `None` if any isn't in a cloud folder, for search results, or on a Windows without the
    /// Cloud Files API (`cldapi.dll`).
    fn cloud_state(&self, targets: &[(PathBuf, bool)]) -> Option<bool> {
        if self.search_mode
            || targets.is_empty()
            || !crate::win_cloud::available()
            || !targets.iter().all(|(p, _)| self.in_cloud(p))
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

    pub(super) fn menu_targets(&self) -> Vec<(PathBuf, bool)> {
        if self.search_mode {
            return self.selected_hit_items();
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
        let extended = window.modifiers().shift;
        let targets = self.menu_targets();
        let (mut menu, early_events) = match self.early_menu.take() {
            Some(early)
                if early.menu.targets == targets
                    && early.menu.extended == extended
                    && early.made.elapsed() < EARLY_MENU_FOR =>
            {
                (early.menu, early.events)
            }
            other => {
                if let Some(early) = other {
                    self.drop_early_menu(early);
                }
                (self.make_context_menu(position, extended, cx), Vec::new())
            }
        };
        menu.position = position;
        window.focus(&menu.focus, cx);
        self.release_shell_menu();
        self.show_menu(menu, window, cx);
        // What came back while the button was down.
        for event in early_events {
            self.handle_shell_menu_event(event, cx);
        }
    }

    /// A right press on items: makes their menu now, reading the Windows menu while the button
    /// is down, to show on release (`open_context_menu`). Replaces one made before.
    pub(super) fn make_early_menu(&mut self, shift: bool, cx: &mut Context<Self>) {
        if let Some(early) = self.early_menu.take() {
            self.drop_early_menu(early);
        }
        if !self.services.shell.shell_menu_prefetch() {
            return;
        }
        let menu = self.make_context_menu(Point::default(), shift, cx);
        if menu.shell_request.is_some() {
            self.early_menu = Some(EarlyMenu {
                menu,
                events: Vec::new(),
                made: std::time::Instant::now(),
            });
        }
    }

    /// The right press became a drag, or went away: its early menu isn't shown, and the Windows
    /// menus read for it are let go.
    pub(super) fn cancel_early_menu(&mut self) {
        if let Some(early) = self.early_menu.take() {
            self.drop_early_menu(early);
        }
    }

    fn drop_early_menu(&self, early: EarlyMenu) {
        let shell = &self.services.shell;
        for event in early.events {
            match event {
                ShellMenuEvent::Whole {
                    result: Ok(menu), ..
                }
                | ShellMenuEvent::Part {
                    result: Ok((menu, _)),
                    ..
                } => shell.release_shell_menu(menu.session),
                _ => {}
            }
        }
    }

    /// What comes back for an early menu (`make_early_menu`) waits for it to show.
    pub(super) fn keep_for_early_menu(&mut self, event: ShellMenuEvent) -> Option<ShellMenuEvent> {
        match self.early_menu.as_mut() {
            Some(early) if early.wants(&event) => {
                early.events.push(event);
                None
            }
            _ => Some(event),
        }
    }

    /// The menu for the selected items (or the folder background), with its Windows menu and
    /// pinned commands' sources asked for.
    fn make_context_menu(
        &mut self,
        position: Point<Pixels>,
        extended: bool,
        cx: &mut Context<Self>,
    ) -> ContextMenu {
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
        let (hidden, order) = {
            let settings = &SettingsStore::global(cx).read(cx).settings;
            (
                settings.menu_hidden.clone(),
                crate::menu_order::ranks(&settings.menu_order),
            )
        };
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
        // The order dragged to in Settings › Context menu (#263); the built-in one as built.
        if let Some(ranks) = order {
            entries =
                crate::menu_order::arrange(entries, ranks, order_slot, || MenuEntry::Separator);
        }
        let entries = hide_entries(entries, &hidden);
        let on_server = targets.first().map_or_else(
            || self.on_remote(),
            |(p, _)| arke_core::remote::is_remote(p),
        );
        let mut entries = if on_server {
            remote_menu(entries)
        } else {
            entries
        };
        // Opened from the pane row's "⋯" with Sort folded into it (#474): its choices come along.
        // The background menu has them anyway.
        // The background menu has "Sort by" anyway, and while searching there is no Sort.
        let from_more = std::mem::take(&mut self.row_fold.menu_from_more);
        let row = self.row_fold.row.filter(|_| from_more);
        if row.is_some_and(|row| row.sort_folded()) && !self.search_mode && !targets.is_empty() {
            entries.extend([MenuEntry::Separator, self.sort_submenu()]);
        }
        // With synchronized browsing and compare folded into it: they come along, checked while
        // on, as the buttons show them (#474).
        if row.is_some_and(|row| row.toggles_folded()) && self.pane.pair_toggles {
            entries.extend(self.dual_toggle_items());
        }
        let mut menu = ContextMenu::new(position, entries, cx.focus_handle(), targets);
        // Shift held, as File Explorer: the Windows menu includes commands registered to show
        // only then (#272).
        menu.extended = extended;
        menu.shell_slot = menu
            .entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Submenu { shell: true, .. }));
        if menu.shell_slot.is_some() && self.services.shell.shell_menu_prefetch() {
            self.request_shell_menu(&mut menu);
            if let Some(request) = menu.shell_request {
                self.request_menu_parts(&mut menu, request, cx);
            }
        }
        menu
    }

    /// Shows `menu`, opened in `window`: its levels open in windows of their own owned by it
    /// (`render_context_menu`). Like a Windows menu, it closes when `window` stops being the active
    /// window (a click in another program, on the taskbar, Alt+Tab) or moves.
    fn show_menu(&mut self, mut menu: ContextMenu, window: &mut Window, cx: &mut Context<Self>) {
        menu.owner = Some(window.window_handle());
        // Asked once, as the menu opens.
        if crate::win_menu::keyboard_cues_always() {
            menu.cues.pressed();
        }
        menu.at = crate::popup::to_screen(window, menu.position);
        menu.watch = crate::popup::close_with_window(window, cx, Self::close_menu);
        self.context_menu = Some(menu);
        cx.notify();
    }

    /// Background menu "View" submenu: the three layouts, the current one checked, as in
    /// Explorer's and Files' menus.
    fn view_submenu(&self) -> MenuEntry {
        use MenuCommand as C;
        let mode = self.view_mode;
        let label = t!("menu.view");
        let access = own_access("view", &label, None);
        MenuEntry::Submenu {
            label: label.into(),
            access,
            enabled: true,
            icon: None,
            glyph: None,
            children: vec![
                checked_item(
                    lab("view_details", t!("menu.view_details")),
                    Some("Ctrl+Shift+6"),
                    C::ViewDetails,
                    mode == ViewMode::Details,
                ),
                checked_item(
                    lab("view_icons", t!("view.icons")),
                    None,
                    C::ViewIcons,
                    mode == ViewMode::Icons,
                ),
                checked_item(
                    lab("view_columns", t!("settings.view_columns")),
                    None,
                    C::ViewColumns,
                    mode == ViewMode::Columns,
                ),
            ],
            shell: false,
            lazy: None,
            key: Some("view"),
            pin: None,
        }
    }

    /// Split view's synchronized browsing and compare as menu rows, checked while on, after a
    /// separator. A pinned tab has no synchronized browsing.
    fn dual_toggle_items(&self) -> Vec<MenuEntry> {
        use MenuCommand as C;
        let mut items = vec![MenuEntry::Separator];
        if !self.locked {
            items.push(checked_item(
                lab("sync", t!("compare.sync_tip")),
                Some("Ctrl+Shift+S"),
                C::ToggleSync,
                self.pane.synced,
            ));
        }
        items.push(checked_item(
            lab("compare", t!("compare.compare_tip")),
            Some("Ctrl+Shift+D"),
            C::ToggleCompare,
            self.pane.comparing,
        ));
        decorate(&mut items);
        items
    }

    /// "New": a folder and a file, as in Windows' own New submenu, instead of two rows of their
    /// own (#190). Its rows keep their shortcuts and the keyboard goes in with Right.
    fn new_submenu(mut children: Vec<MenuEntry>) -> MenuEntry {
        decorate(&mut children);
        let label = t!("menu.new");
        let access = own_access("new", &label, None);
        MenuEntry::Submenu {
            label: label.into(),
            access,
            enabled: true,
            icon: None,
            glyph: Some(Icon::Plus),
            children,
            shell: false,
            lazy: None,
            key: Some("new"),
            pin: None,
        }
    }

    /// Background menu "Sort by": the Sort button's choices (#179), so Icons and Columns, which
    /// have no headers, can be sorted from here too.
    fn sort_submenu(&self) -> MenuEntry {
        use MenuCommand as C;
        let sort = self.sort;
        let mut children: Vec<MenuEntry> = super::sortopts::SORT_COLUMNS
            .into_iter()
            .map(|column| {
                checked_item(
                    super::sortopts::column_label(column),
                    None,
                    C::SortBy(column),
                    sort.column == column,
                )
            })
            .collect();
        children.extend([
            MenuEntry::Separator,
            checked_item(
                lab("asc", t!("settings.ascending")),
                None,
                C::SortDirection(true),
                sort.ascending,
            ),
            checked_item(
                lab("desc", t!("settings.descending")),
                None,
                C::SortDirection(false),
                !sort.ascending,
            ),
        ]);
        let label = t!("settings.sort_by");
        let access = own_access("sort_by", &label, None);
        MenuEntry::Submenu {
            label: label.into(),
            access,
            enabled: true,
            icon: None,
            glyph: Some(Icon::Sort),
            children,
            shell: false,
            lazy: None,
            // No entry of its own in Settings › Context menu: it goes with View.
            key: None,
            pin: None,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One section per menu kind (background, single, multi), listed top to bottom as on screen"
    )]
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
                Self::new_submenu(vec![
                    item(
                        lab("new_folder", t!("menu.new_folder")),
                        Some("Ctrl+Shift+N"),
                        C::NewFolder,
                    ),
                    item(
                        lab("new_text", t!("menu.new_text_file")),
                        None,
                        C::NewTextFile,
                    ),
                ]),
                item_if(
                    self.services.shell.clipboard_has_files(),
                    lab("paste", t!("menu.paste")),
                    Some("Ctrl+V"),
                    C::Paste,
                ),
                item_if(
                    undo.is_some(),
                    lab(
                        if undo.is_some() { "undo_op" } else { "undo" },
                        undo.unwrap_or_else(|| t!("menu.undo").into_owned()),
                    ),
                    Some("Ctrl+Z"),
                    C::Undo,
                ),
                MenuEntry::Separator,
                // Icon size, grouping and the other view switches are in the view options flyout.
                self.view_submenu(),
            ];
            // Search results keep their own order (relevance, or their headers).
            if !self.search_mode {
                entries.push(self.sort_submenu());
            }
            entries.extend([
                item(lab("refresh", t!("menu.refresh")), Some("F5"), C::Refresh),
                MenuEntry::Separator,
                item(lab("copy_path", t!("menu.copy_path")), None, C::CopyPath),
                item(
                    lab("open_explorer", t!("menu.open_in_explorer")),
                    None,
                    C::RevealInExplorer,
                ),
                item(
                    lab("terminal", t!("menu.open_terminal")),
                    None,
                    C::OpenTerminal,
                ),
                MenuEntry::Separator,
                item(pin_label(pinned), None, C::TogglePin),
                // The folder's own, as Alt+Enter with nothing selected; Windows' would open
                // Explorer's dialog from "Show more options" otherwise.
                item(
                    lab("props", t!("menu.properties")),
                    Some("Alt+Enter"),
                    C::Properties,
                ),
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
                entries.insert(at, item(lab("redo_op", label), Some("Ctrl+Y"), C::Redo));
            }
            return entries;
        }
        let single = targets.len() == 1;
        let (path, is_dir) = &targets[0];
        // Inside this folder, as at the top of VS Code's explorer menu. A server's shares are
        // only listed, so nothing can be made in `\\server` itself.
        let new_in = single && *is_dir && arke_core::net::bare_server(path).is_none();
        let mut entries = vec![item(lab("open", t!("menu.open")), Some("Enter"), C::Open)];
        if single && *is_dir {
            entries.push(item(
                lab("open_tab", t!("menu.open_in_new_tab")),
                Some("Ctrl+Enter"),
                C::OpenInNewTab,
            ));
            entries.push(MenuEntry::Separator);
            if new_in {
                entries.push(Self::new_submenu(vec![
                    item(
                        lab("new_file_in", t!("menu.new_file_in")),
                        None,
                        C::NewFileIn,
                    ),
                    item(
                        lab("new_folder_in", t!("menu.new_folder_in")),
                        None,
                        C::NewFolderIn,
                    ),
                ]));
            }
            entries.push(item(
                lab("search_folder", t!("menu.search_in_folder")),
                None,
                C::SearchInFolder,
            ));
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
            entries.push(item(
                lab("terminal", t!("menu.open_terminal")),
                None,
                C::OpenTerminal,
            ));
        }
        let name = path
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        if single && !*is_dir && archive_kind(&name).is_some() {
            entries.push(MenuEntry::Separator);
            let folder = menu_name(&folder_name_for(&name));
            entries.push(item(
                lab_with("extract_to", t!("menu.extract_to", name = folder), &folder),
                None,
                C::ExtractToFolder,
            ));
            entries.push(item(
                lab("extract_here", t!("menu.extract_here")),
                None,
                C::ExtractHere,
            ));
        } else if !single && targets.iter().all(|(path, dir)| !dir && is_archive(path)) {
            // Several archives: each into a folder of its own, one after another (#407).
            entries.push(MenuEntry::Separator);
            entries.push(item(
                lab("extract_each", t!("menu.extract_each")),
                None,
                C::ExtractToFolder,
            ));
            entries.push(item(
                lab("extract_here", t!("menu.extract_here")),
                None,
                C::ExtractHere,
            ));
        }
        if single && !*is_dir && is_backdrop_image(&name) {
            entries.push(MenuEntry::Separator);
            entries.push(item(
                lab("set_bg", t!("menu.set_background")),
                None,
                C::SetBackground,
            ));
        }
        // Settings backup. Whether it's really an Arke backup is checked in the background on
        // import.
        if single && !*is_dir && name.to_lowercase().ends_with(".json") {
            entries.push(MenuEntry::Separator);
            entries.push(item(
                lab("import", t!("menu.import_settings")),
                None,
                C::ImportSettings,
            ));
        }
        // Search results may span folders, so compress is only offered in the normal list.
        if !self.search_mode {
            entries.push(MenuEntry::Separator);
            let zip = menu_name(&format!(
                "{}.zip",
                zip_stem_for_targets(self.current_path(), targets)
            ));
            entries.push(item(
                lab_with("compress", t!("menu.compress", name = zip), &zip),
                None,
                C::Compress,
            ));
            entries.push(item(
                lab("move_new", t!("menu.move_to_new_folder")),
                None,
                C::MoveToNewFolder,
            ));
        }
        // Split view: Alt+C / Alt+M are hard to find from the keyboard alone.
        if self.pane.peer.is_some() {
            entries.push(MenuEntry::Separator);
            entries.push(item(
                t!("menu.copy_to_other_pane"),
                Some("Alt+C"),
                C::CopyToPeer,
            ));
            entries.push(item(
                t!("menu.move_to_other_pane"),
                Some("Alt+M"),
                C::MoveToPeer,
            ));
        }
        entries.push(MenuEntry::Separator);
        if single && *is_dir {
            entries.push(item(
                lab("new_window", t!("menu.open_new_window")),
                None,
                C::OpenInNewWindow,
            ));
        }
        // Apps the user added, single selection only.
        if single {
            for (ix, app) in apps.iter().enumerate() {
                entries.push(item(
                    t!("menu.open_with", name = app.name.as_str()),
                    None,
                    C::OpenWith(ix),
                ));
            }
        }
        // Symlinks and junctions (files or folders): where they point.
        if single && self.target_is_link(path) {
            entries.push(item(
                lab("link", t!("menu.go_to_link_target")),
                None,
                C::GoToLinkTarget,
            ));
        }
        // Single file: Windows "Open with"; shortcuts also get "Open file location".
        if single && !*is_dir {
            if name.to_lowercase().ends_with(".lnk") {
                entries.push(item(
                    lab("file_location", t!("menu.open_file_location")),
                    None,
                    C::OpenFileLocation,
                ));
            }
            entries.push(item(
                lab("choose_app", t!("menu.choose_app")),
                None,
                C::ChooseApp,
            ));
        }
        entries.push(item(
            lab("shelf", t!("menu.add_to_shelf")),
            Some("Ctrl+Shift+A"),
            C::AddToShelf,
        ));
        entries.push(item(
            lab("copy_path", t!("menu.copy_path")),
            None,
            C::CopyPath,
        ));
        // The tree names nested rows by their path below the folder shown.
        if self.tree_active() {
            entries.push(item(
                lab("copy_rel", t!("menu.copy_relative_path")),
                None,
                C::CopyRelativePath,
            ));
        }
        // A folder's "New folder…" is a button here too, a click away (#190); the keyboard has it
        // in "New".
        let new_folder = new_in.then(|| {
            action(
                lab("new_folder", t!("menu.new_folder")),
                C::NewFolderIn,
                true,
            )
        });
        entries.extend([
            item(
                lab("reveal", t!("menu.reveal_in_explorer")),
                None,
                C::RevealInExplorer,
            ),
            item(
                lab("props", t!("menu.properties")),
                Some("Alt+Enter"),
                C::Properties,
            ),
            MenuEntry::Separator,
            more_options(),
            MenuEntry::Separator,
            MenuEntry::Actions(
                new_folder
                    .into_iter()
                    .chain([
                        action(lab("cut", t!("menu.cut")), C::Cut, true),
                        action(lab("copy", t!("menu.copy")), C::Copy, true),
                        // Works on multiple items too: renames all selected to "name (1)",
                        // "name (2)", ...
                        action(
                            lab("rename", t!("menu.rename")),
                            C::Rename,
                            !self.search_mode,
                        ),
                        action(lab("delete", t!("menu.delete")), C::Delete, true),
                    ])
                    .collect(),
            ),
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
        menu.highlight_by_key(selectable[next]);
        cx.notify();
    }

    /// Home and End: the first or last row that can be picked, as in a Windows menu.
    fn highlight_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let entries = menu.level(menu.open.len());
        let found = if last {
            entries.iter().rposition(MenuEntry::selectable)
        } else {
            entries.iter().position(MenuEntry::selectable)
        };
        let Some(ix) = found else {
            return;
        };
        menu.highlight_by_key(ix);
        cx.notify();
    }

    pub(super) fn menu_up(&mut self, _: &MenuUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(false, cx);
    }

    pub(super) fn menu_down(&mut self, _: &MenuDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(true, cx);
    }

    pub(super) fn menu_home(&mut self, _: &MenuHome, _: &mut Window, cx: &mut Context<Self>) {
        self.highlight_edge(false, cx);
    }

    pub(super) fn menu_end(&mut self, _: &MenuEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.highlight_edge(true, cx);
    }

    /// A letter or digit: picks the row with that access key (`access_hit`), as Windows does.
    /// Shift counts as the letter; Ctrl and Alt make it a shortcut of something else.
    pub(super) fn menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let mods = keystroke.modifiers;
        if mods.control || mods.alt || mods.platform || mods.function {
            return;
        }
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        // Any key shows the underlines.
        menu.cues.pressed();
        cx.notify();
        let mut chars = keystroke.key_char.as_deref().unwrap_or_default().chars();
        let (Some(typed), None) = (chars.next(), chars.next()) else {
            return;
        };
        if !typed.is_alphanumeric() {
            return;
        }
        cx.stop_propagation();
        let hit = shift_hit(
            access_hit(menu.level(menu.open.len()), menu.highlighted, typed),
            mods.shift,
        );
        match hit {
            AccessHit::Nothing => {}
            AccessHit::Run(command) => self.run_menu_command(command, window, cx),
            AccessHit::Open(ix) => {
                menu.highlighted = Some(ix);
                menu.scroll_row_into_view(menu.open.len(), ix);
                self.enter_submenu(cx);
            }
            AccessHit::Move(ix) => menu.highlight_by_key(ix),
        }
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

    /// A screen reader opened or closed the submenu of row `ix` in menu `level` (#322): opens it
    /// as Right does from that row, or closes it and goes back to the row.
    fn a11y_toggle_submenu(&mut self, level: usize, ix: usize, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let was_open = menu.open.get(level) == Some(&ix);
        menu.open.truncate(level);
        menu.highlighted = Some(ix);
        menu.hover_task = None;
        if was_open {
            cx.notify();
        } else {
            self.enter_submenu(cx);
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
        let level = menu.open.len();
        menu.open.push(ix);
        menu.highlighted = first;
        menu.hover_task = None;
        menu.keyboard = true;
        menu.cues.pressed();
        self.ensure_shell_menu();
        self.fill_lazy_submenu(level, ix);
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
            menu.watch.clear();
            crate::motion::Leaving::start(self, menu, |this| &mut this.closing_menu, cx);
            window.focus(&self.list_focus, cx);
            cx.notify();
        }
    }

    /// Menu closed without a choice: release the background Windows menus too (fire and forget).
    fn release_shell_menu(&mut self) {
        self.release_shell_menus_but(None);
    }

    /// Releases the menu's Windows menus (whole and single extensions) except session `keep`,
    /// which is about to run a command.
    fn release_shell_menus_but(&mut self, keep: Option<u64>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let mut sessions: Vec<u64> = menu.parts.drain(..).map(|(s, _)| s).collect();
        if menu.shell_session.is_some_and(|s| Some(s) != keep) {
            sessions.extend(menu.shell_session.take());
        }
        for session in sessions.into_iter().filter(|s| Some(*s) != keep) {
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
                    // A submenu filled only when opened is asked for now, as it opens, not for
                    // every row the pointer passes over on its way: each fill holds up the
                    // Windows menu for a while.
                    if submenu {
                        this.fill_lazy_submenu(level, ix);
                    }
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
            self.fill_lazy_submenu(level, ix);
            cx.notify();
        }
    }

    // ───────────────────────── Running commands ─────────────────────────

    #[allow(
        clippy::too_many_lines,
        reason = "One arm per command; splitting it up would scatter them"
    )]
    pub(crate) fn run_menu_command(
        &mut self,
        command: MenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use MenuCommand as C;
        if let C::PinPending(ix) = command {
            // Its source is still being read: it runs as soon as it is in (`run_pending_pin`).
            if let Some(menu) = self.context_menu.as_mut() {
                menu.pending = menu.coming.get(ix).cloned();
            }
            return;
        }
        let keep = match command {
            MenuCommand::Shell(_) => self.context_menu.as_ref().and_then(|m| m.shell_session),
            MenuCommand::ShellPart { session, .. } => Some(session),
            _ => None,
        };
        self.release_shell_menus_but(keep);
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
                // Several selected: each in turn, in the order of the selection (#407).
                for (path, dir) in &targets {
                    if !dir && is_archive(path) {
                        self.extract_archive(path.clone(), command == C::ExtractHere, None, cx);
                    }
                }
            }
            C::Cut => self.cut_files(&CutFiles, window, cx),
            C::Copy => self.copy_files(&CopyFiles, window, cx),
            C::Paste => self.paste_files(&PasteFiles, window, cx),
            C::Rename => self.rename_file(&RenameFile, window, cx),
            C::Delete => self.delete_files(&DeleteFiles, window, cx),
            C::DeleteForever => self.delete_permanently(&DeletePermanently, window, cx),
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
            C::SortBy(column) => self.pick_sort_column(column, cx),
            C::SortDirection(ascending) => self.pick_sort_direction(ascending, cx),
            C::CopyPath => self.copy_paths(&targets),
            C::CopyRelativePath => self.copy_relative_paths(&targets),
            C::CopyAddress => self.copy_address(&targets),
            C::EditAddress => self.start_edit_path(window, cx),
            C::ClearHistory => {
                crate::settings::SettingsStore::global(cx)
                    .update(cx, |s, cx| s.update(|st| st.recent.clear(), cx));
            }
            C::RevealInExplorer => self.reveal_in_explorer(&targets),
            C::KeepOnDevice | C::FreeUpSpace => {
                let pin = match (command, self.cloud_state(&targets)) {
                    (C::FreeUpSpace, _) => crate::shell::CloudPin::Free,
                    (_, Some(true)) => crate::shell::CloudPin::Default,
                    _ => crate::shell::CloudPin::Keep,
                };
                let paths: Vec<PathBuf> = targets.iter().map(|(p, _)| p.clone()).collect();
                let shell = Arc::clone(&self.services.shell);
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
            C::CopyToPeer | C::MoveToPeer => self.transfer_to_peer(command == C::MoveToPeer, cx),
            C::OpenInNewWindow => {
                if let Some((path, true)) = targets.first().cloned() {
                    crate::instance::open_new_window(Some(path));
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
            C::Properties => {
                let mut targets = targets;
                // The background menu: the folder shown.
                if targets.is_empty()
                    && let Some(path) = self.current_path()
                {
                    targets.push((path.to_path_buf(), true));
                }
                self.open_properties(targets, cx);
            }
            // Windows' "Choose another app" dialog, with "Always", as Explorer opens it.
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
                    let shell = Arc::clone(&self.services.shell);
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
                    let fs = Arc::clone(&self.services.fs);
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
            // The same as the buttons in the pane row.
            C::ToggleSync => cx.emit(ExplorerEvent::ToggleSync),
            C::ToggleCompare => cx.emit(ExplorerEvent::ToggleCompare),
            C::SetBackground => {
                if let Some((path, false)) = targets.first().cloned() {
                    crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
                        store.update(|s| s.background_image = Some(path), cx);
                    });
                }
            }
            // Disabled rows like "Loading..."; never executed.
            C::DropCancel | C::MoreOptions | C::PinPending(_) => {}
            C::Shell(id) => {
                if let Some(session) = menu.shell_session {
                    self.invoke_shell_menu(session, id, menu.position, window);
                }
            }
            C::ShellPart { session, id } => {
                self.invoke_shell_menu(session, id, menu.position, window);
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

    pub(super) fn copy_paths(&self, targets: &[(PathBuf, bool)]) {
        // Same as Explorer's "Copy as path": each path quoted, one per line.
        let text = if targets.is_empty() {
            self.current_path()
                .map_or_default(|p| format!("\"{}\"", p.display()))
        } else {
            targets
                .iter()
                .map(|(p, _)| format!("\"{}\"", p.display()))
                .collect::<Vec<_>>()
                .join("\r\n")
        };
        let count = targets.len().max(1);
        let shell = Arc::clone(&self.services.shell);
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
        let shell = Arc::clone(&self.services.shell);
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
        let shell = Arc::clone(&self.services.shell);
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
            item(
                lab("copy_address", t!("menu.copy_address")),
                None,
                C::CopyAddress,
            ),
            item(
                lab("copy_as_path", t!("menu.copy_as_path")),
                Some("Ctrl+Shift+C"),
                C::CopyPath,
            ),
            item(
                lab("edit_address", t!("menu.edit_address")),
                Some("Ctrl+L"),
                C::EditAddress,
            ),
            MenuEntry::Separator,
            item(
                lab("clear_history", t!("menu.clear_history")),
                None,
                C::ClearHistory,
            ),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        let menu = ContextMenu::new(position, entries, focus, vec![(here, true)]);
        self.show_menu(menu, window, cx);
    }

    /// Whether the menu target `path` is a symlink or junction (from the listing, no I/O).
    fn target_is_link(&self, path: &std::path::Path) -> bool {
        if self.search_mode {
            return self
                .selected_hits()
                .iter()
                .any(|(at, hit)| at == path && hit.entry.is_link());
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
            lab("reset_cols", t!("menu.reset_column_order")),
            None,
            MenuCommand::ResetColumnOrder,
        ));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        let menu = ContextMenu::new(position, entries, focus, Vec::new());
        self.show_menu(menu, window, cx);
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
        let move_item = item(
            lab_with("drop_move", t!("dnd.menu_move", dest = name), &name),
            None,
            C::DropMove,
        );
        let copy_item = item(
            lab_with("drop_copy", t!("dnd.menu_copy", dest = name), &name),
            None,
            C::DropCopy,
        );
        let default_move =
            crate::explorer::dnd::drop_is_move(gpui::Modifiers::default(), &sources, &dest);
        let mut entries = if default_move {
            vec![move_item, copy_item]
        } else {
            vec![copy_item, move_item]
        };
        entries.push(item(
            lab("drop_shortcut", t!("dnd.menu_shortcut")),
            None,
            C::DropShortcut,
        ));
        entries.push(MenuEntry::Separator);
        entries.push(item(
            lab("drop_cancel", t!("dnd.menu_cancel")),
            Some("Esc"),
            C::DropCancel,
        ));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.release_shell_menu();
        let menu = ContextMenu::new(position, entries, focus, Vec::new());
        self.show_menu(menu, window, cx);
        self.pending_drop = Some((sources, dest));
    }

    /// Creates a shortcut in `dest` for each of `sources` in the background. The watcher updates
    /// the list.
    pub(crate) fn create_shortcuts(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let shell = Arc::clone(&self.services.shell);
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
        let shell = Arc::clone(&self.services.shell);
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
        let shell = Arc::clone(&self.services.shell);
        let owner = Self::owner_from(window);
        let tx = self.events_tx.clone();
        crate::explorer::spawn_thread("choose-app", move || {
            if let Err(err) = shell.choose_app(&path, owner) {
                let name = path
                    .file_name()
                    .map_or_default(|n| n.to_string_lossy().into_owned());
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
        let shell = Arc::clone(&self.services.shell);
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
        let Some((paths, background)) = self.menu_paths(menu) else {
            return;
        };
        let extended = menu.extended;
        let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        menu.shell_request = Some(request);
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("more-options", move || {
            let started = Instant::now();
            let result = shell.shell_menu(&paths, background, extended);
            // What a slow menu was waiting on can be named from the log (#278).
            if let Ok(menu) = &result {
                let names: Vec<&str> = menu.filling.iter().map(|(_, l)| l.as_str()).collect();
                log::info!(
                    "more options: {} ms{}{}",
                    started.elapsed().as_millis(),
                    if menu.partial { ", partly read" } else { "" },
                    if names.is_empty() {
                        String::new()
                    } else {
                        format!(", filling after: {}", names.join(", "))
                    }
                );
            }
            let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::Whole {
                request,
                result,
            }));
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
        menu.whole_request = Some(request);
        let hint = !SettingsStore::global(cx).read(cx).settings.pin_hint_seen;
        let mut filling = Vec::new();
        let children = match result {
            Ok(shell_menu) => {
                filling = shell_menu.filling.iter().map(|(ix, _)| *ix).collect();
                menu.left_filling.clone_from(&filling);
                menu.shell_session = Some(shell_menu.session);
                menu.whole_partial = shell_menu.partial;
                let entries: Vec<MenuEntry> = without_duplicates(shell_menu.entries, &menu.entries)
                    .into_iter()
                    .map(|e| MenuEntry::from_shell_in(e, &[], None))
                    .collect();
                if entries.is_empty() {
                    vec![placeholder(t!("menu.no_more_options"))]
                } else if hint {
                    // Once: what the pins next to the commands do.
                    menu.pin_hint = true;
                    let mut entries = entries;
                    entries.extend([MenuEntry::Separator, placeholder(t!("menu.pin_hint"))]);
                    entries
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
        menu.apply_send_to();
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        menu.sync_pinned(&pinned);
        let slot = menu.shell_slot;
        self.run_pending_pin(cx);
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
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
        if let Some(menu) = self.context_menu.as_ref() {
            self.learn_pin_owners(menu, request, cx);
        }
        self.fill_lazy_for_pins(cx);
        self.fill_left_filling(&filling);
    }

    /// A pinned row clicked before its source answered: runs it now that it is in, or forgets it
    /// once the whole Windows menu has come without it.
    pub(super) fn run_pending_pin(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let Some(key) = menu.pending.clone() else {
            return;
        };
        let real = menu.entries.iter().find_map(|e| match e {
            MenuEntry::Item(i)
                if i.pin.as_deref() == Some(key.as_str())
                    && !matches!(i.command, MenuCommand::PinPending(_)) =>
            {
                Some((i.command, i.enabled))
            }
            _ => None,
        });
        match real {
            Some((command, enabled)) => {
                menu.pending = None;
                let Some(owner) = menu.owner.filter(|_| enabled) else {
                    return;
                };
                let this = cx.entity();
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| this.run_menu_command(command, window, cx));
                    });
                });
            }
            None if menu.shell_request.is_none() && !menu.whole_partial => menu.pending = None,
            None => {}
        }
    }

    /// Arke's own Properties window for `targets` (full path, whether it is a folder).
    pub(crate) fn open_properties(&self, targets: Vec<(PathBuf, bool)>, cx: &mut Context<Self>) {
        let services = self.services.clone();
        // Opened once the menu or key press that asked for it is done with this window.
        cx.defer(move |cx| {
            crate::properties::open(services, targets, cx);
        });
    }

    /// Pins or unpins a Windows command (pin icon on rows in "Show more options"); an open menu
    /// updates immediately.
    pub(crate) fn toggle_shell_pin(&mut self, key: &str, cx: &mut Context<Self>) {
        let key = key.to_string();
        let mut added = true;
        let pinned = SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    if let Some(at) = s.menu_pinned.iter().position(|k| *k == key) {
                        added = false;
                        s.menu_pinned.remove(at);
                        s.menu_pin_seen.remove(&key);
                        if let Some(owner) = s.menu_pin_owners.remove(&key)
                            && !s.menu_pin_owners.values().any(|o| *o == owner)
                        {
                            s.menu_pin_types.remove(&owner);
                        }
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
        // Pinned from the whole Windows menu, which is in: where it comes from is learned now,
        // so it shows on its own from the very next menu, not the one after.
        if added
            && let Some(menu) = self.context_menu.as_ref()
            && let Some(request) = menu.whole_request
        {
            self.learn_pin_owners(menu, request, cx);
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
        let shell = Arc::clone(&self.services.shell);
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

    /// Extracts `archive_path` into a new folder next to it, or `here`; `password` for a
    /// protected zip or 7z (#346), asked for after a first try without one. Waits for this tab's
    /// extraction or compression running now, if any (#407).
    pub(crate) fn extract_archive(
        &mut self,
        archive_path: PathBuf,
        here: bool,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let name = archive_path
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        let (Some(kind), Some(model)) = (archive_kind(&name), self.model.as_ref()) else {
            return;
        };
        let parent = model.path.clone();
        // Extracting into a new folder picks a unique name, among the items listed and the folders
        // the jobs before it will make. On a collision existing files still aren't overwritten.
        let (dest, folder) = if here {
            (parent.clone(), None)
        } else {
            let taken = self.taken_names(&parent);
            let folder =
                arke_core::path::unique_name(&folder_name_for(&name), |n| taken.contains(n));
            (parent.join(&folder), Some(folder))
        };
        let (job, ops) =
            crate::op_center::OpCenter::begin(cx, "op.extract", Some((name.clone(), 1)));
        let claim = folder.as_ref().map(|f| (parent.clone(), f.to_lowercase()));
        self.queue_archive_job(
            ExtractJob {
                name,
                archive: Some((archive_path.clone(), here)),
                compress: false,
                job,
                ops,
                progress: ExtractProgress::default(),
                claim,
            },
            "extract",
            move |shell, progress, ops| {
                let decode = |bytes: &[u8]| shell.decode_ansi(bytes);
                let result = archive::extract(
                    &archive_path,
                    &dest,
                    kind,
                    &ExtractOptions {
                        cancel: &ops.cancel,
                        progress,
                        decode_legacy: &decode,
                        password: password.as_deref(),
                        workers: archive_workers(shell, &archive_path, &parent),
                    },
                );
                drop(password);
                // Stopped before writing anything (a password to ask for, a method Arke can't
                // read): the new folder goes. Only while it is empty, so nothing extracted is lost.
                if folder.is_some() && result.is_err() {
                    let _ = std::fs::remove_dir(&dest);
                }
                AppEvent::ExtractDone {
                    job,
                    result,
                    folder,
                }
            },
            cx,
        );
    }

    /// Zips into the same folder, named after the first item, or after the folder for several
    /// items (made unique). Runs in the background with cancellable progress in the banner, after
    /// this tab's extraction or compression running now, if any (#407).
    fn start_compress(&mut self, targets: &[(PathBuf, bool)], cx: &mut Context<Self>) {
        let (Some((first, _)), Some(model)) = (targets.first(), self.model.as_ref()) else {
            return;
        };
        let stem = zip_stem_for_targets(Some(&model.path), targets);
        let parent = model.path.clone();
        let taken = self.taken_names(&parent);
        let name = arke_core::path::unique_file_name(&stem, "zip", |n| taken.contains(n));
        let dest = parent.join(&name);
        let sources: Vec<PathBuf> = targets.iter().map(|(p, _)| p.clone()).collect();
        let first_name = first.file_name().map_or_else(
            || first.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let (job, ops) =
            crate::op_center::OpCenter::begin(cx, "op.compress", Some((first_name, sources.len())));
        let claim = Some((parent, name.to_lowercase()));
        self.queue_archive_job(
            ExtractJob {
                name: name.clone(),
                archive: None,
                compress: true,
                job,
                ops,
                progress: ExtractProgress::default(),
                claim,
            },
            "compress",
            move |shell, progress, ops| {
                let local_time = |t: std::time::SystemTime| shell.local_parts(t);
                let result = archive::compress(
                    &sources,
                    &dest,
                    &archive::CompressOptions {
                        cancel: &ops.cancel,
                        progress,
                        local_time: &local_time,
                    },
                );
                AppEvent::CompressDone {
                    job,
                    result,
                    name,
                    dest,
                }
            },
            cx,
        );
    }

    /// Names in `folder` (lowercase) that a new folder or zip can't take: the items listed, and
    /// what this tab's queued and running jobs will make there before it shows up.
    fn taken_names(&self, folder: &Path) -> std::collections::HashSet<String> {
        let listed = self
            .model
            .as_ref()
            .filter(|model| model.path == folder)
            .into_iter()
            .flat_map(|model| model.entries.iter().map(|e| e.name.to_lowercase()));
        let claimed = self
            .extract_jobs
            .iter()
            .filter_map(|job| job.claim.as_ref())
            .filter(|(parent, _)| parent == folder)
            .map(|(_, name)| name.clone());
        listed.chain(claimed).collect()
    }

    /// Puts `job` in this tab's line (#407) and starts its thread, which waits for the jobs before
    /// it (Cancel while waiting takes it out of the line), then runs `work` with the progress
    /// callback and sends back the event `work` returns.
    fn queue_archive_job(
        &mut self,
        job: ExtractJob,
        thread: &'static str,
        work: impl FnOnce(&dyn crate::shell::Shell, &ArchiveProgress<'_>, &OpProgress) -> AppEvent
        + Send
        + 'static,
        cx: &mut Context<Self>,
    ) {
        let (id, ops) = (job.job, Arc::clone(&job.ops));
        // Behind another job: "Queued" in the progress center from the start.
        if !self.extract_jobs.is_empty() {
            ops.queued.store(true, Ordering::Relaxed);
        }
        self.extract_jobs.push(job);
        self.notice = None;
        let tab = self.archive_line;
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread(thread, move || {
            let Some(_turn) = crate::op_queue::enter_archive(tab, &ops) else {
                let _ = tx.send(AppEvent::ArchiveJobDropped(id));
                return;
            };
            // Banner progress is sent at most every 100 ms to avoid flooding the UI; the progress
            // center reads counters directly.
            let last = Mutex::new(Instant::now());
            let progress_tx = tx.clone();
            let counters = Arc::clone(&ops);
            let progress = move |p: ExtractProgress| {
                report(&counters, p);
                let mut last = last.lock();
                if last.elapsed() >= Duration::from_millis(100) {
                    *last = Instant::now();
                    drop(last);
                    let _ = progress_tx.send(AppEvent::ExtractProgress {
                        job: id,
                        progress: p,
                    });
                }
            };
            let done = work(shell.as_ref(), &progress, ops.as_ref());
            let _ = tx.send(done);
        });
        cx.notify();
    }

    /// Takes the job with this progress center id out of the line.
    fn take_extract_job(&mut self, id: u64) -> Option<ExtractJob> {
        let at = self.extract_jobs.iter().position(|job| job.job == id)?;
        Some(self.extract_jobs.remove(at))
    }

    /// A job cancelled before its turn came: it did nothing, so it only leaves the line.
    pub(super) fn handle_archive_job_dropped(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(job) = self.take_extract_job(id) {
            let verb = if job.compress {
                "op.compress"
            } else {
                "op.extract"
            };
            crate::op_center::OpCenter::finish(cx, job.job, verb, &outcome(Ok(true)));
        }
        cx.notify();
    }

    pub(super) fn handle_compress_done(
        &mut self,
        id: u64,
        result: Result<archive::CompressSummary, archive::ArchiveError>,
        name: String,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if let Some(job) = self.take_extract_job(id) {
            // The count the job ends with, not the last progress tick the writer got to.
            if let Ok(s) = &result {
                job.ops.done.store(s.files, Ordering::Relaxed);
            }
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
                // A clean run is in the operation center only (#266); skipped files are worth a toast.
                if s.skipped > 0 {
                    msg.push_str(&t!("compress.skipped", count = s.skipped));
                    self.notice = Some(msg);
                }
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
                    // Named in the render, where focus can be checked once it is listed (#409).
                    if !self.search_mode {
                        self.rename_after_compress = Some(Arc::from(name.as_str()));
                    }
                }
            }
        }
        cx.notify();
    }

    pub(super) fn handle_extract_progress(
        &mut self,
        id: u64,
        p: ExtractProgress,
        cx: &mut Context<Self>,
    ) {
        if let Some(job) = self.extract_jobs.iter_mut().find(|job| job.job == id) {
            job.progress = p;
            cx.notify();
        }
    }

    pub(super) fn handle_extract_done(
        &mut self,
        id: u64,
        result: Result<ExtractSummary, archive::ArchiveError>,
        folder: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let job = self.take_extract_job(id);
        // A protected archive: ask for its password (again, if this one was wrong) instead of
        // failing. The try that stopped to ask isn't a job in the operation center.
        if let (Err(e), Some(job)) = (&result, &job)
            && needs_password(e)
            && let Some((archive, here)) = job.archive.clone()
        {
            crate::op_center::OpCenter::forget(cx, job.job);
            let wrong = matches!(e, archive::ArchiveError::WrongPassword);
            cx.emit(ExplorerEvent::ArchivePassword(
                crate::archive_password::PasswordAsk {
                    archive,
                    here,
                    wrong,
                },
            ));
            cx.notify();
            return;
        }
        let archive = job
            .as_ref()
            .and_then(|j| j.archive.as_ref().map(|(path, _)| path.clone()));
        let name = match job {
            Some(job) => {
                // The count the job ends with, not the last progress tick the writer got to.
                if let Ok(s) = &result {
                    job.ops.done.store(s.files, Ordering::Relaxed);
                }
                let state = outcome(result.as_ref().map(|s| s.cancelled));
                crate::op_center::OpCenter::finish(cx, job.job, "op.extract", &state);
                job.name
            }
            None => String::new(),
        };
        match result {
            Err(e) => {
                self.error = Some(t!("extract.failed", name = name, error = e).into_owned());
                // Compressed in a way Arke can't read: the app Windows has for it may (#341).
                if can_not_read(&e) {
                    self.error_action = archive.map(super::ErrorAction::OpenDefault);
                }
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
                // A clean, complete run is in the operation center only (#266).
                if s.cancelled || s.skipped_existing > 0 || s.skipped_unsafe > 0 {
                    self.notice = Some(msg);
                }
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

    /// Delivers a compression's end without running one, so a test decides how it ended.
    #[cfg(test)]
    pub(crate) fn compress_done_for_test(
        &mut self,
        result: Result<archive::CompressSummary, archive::ArchiveError>,
        name: &str,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        // Not a job of this tab's line: only the end is delivered.
        self.handle_compress_done(0, result, name.to_owned(), dest, cx);
    }

    /// Cancels the job the banner shows: the first in line, the one running.
    pub(crate) fn cancel_extract(&mut self) {
        if let Some(job) = self.extract_jobs.first() {
            job.ops.cancel.store(true, Ordering::Relaxed);
        }
    }

    // ───────────────────────── Rendering ─────────────────────────

    /// While a menu is open, what stays in its own window: its keys and focus, and closing on a
    /// press anywhere in the window. Its levels are drawn in windows of their own
    /// (`render_menu_popup`), opened from here as levels open.
    pub(super) fn render_context_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.sync_menu_popups(cx);
        let menu = self.context_menu.as_ref()?;
        Some(
            div()
                .id("context-menu-keys")
                .role(gpui::Role::Menu)
                .aria_label(menu_a11y_label(menu))
                .children(menu_a11y_items(menu))
                .absolute()
                .size_0()
                .key_context("ContextMenu")
                .track_focus(&menu.focus)
                .on_action(cx.listener(Self::menu_up))
                .on_action(cx.listener(Self::menu_down))
                .on_action(cx.listener(Self::menu_home))
                .on_action(cx.listener(Self::menu_end))
                .on_action(cx.listener(Self::menu_left))
                .on_action(cx.listener(Self::menu_right))
                .on_action(cx.listener(Self::menu_confirm))
                .on_action(cx.listener(Self::menu_dismiss))
                .on_key_down(cx.listener(Self::menu_key))
                .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_menu(window, cx);
                }))
                .into_any_element(),
        )
    }

    /// Opens the windows of the levels that aren't open yet. The windows of levels closed since
    /// close themselves: their content is gone (`render_menu_popup`).
    fn sync_menu_popups(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let open = &menu.open;
        let still = menu
            .popups
            .iter()
            .enumerate()
            .take_while(|(level, popup)| *level <= open.len() && popup.path[..] == open[..*level])
            .count();
        menu.popups.truncate(still);
        if menu.popups.len() > menu.open.len() || menu.opening || menu.popup_failed {
            return;
        }
        menu.opening = true;
        let this = cx.weak_entity();
        // Not now: a popup draws its level as it opens, which reads this view.
        App::defer(cx, move |cx| Self::open_menu_popups(&this, cx));
    }

    /// Opens the missing levels' windows, outer first: a submenu is placed beside its row in the
    /// window of the level before it.
    fn open_menu_popups(this: &WeakEntity<Self>, cx: &mut App) {
        while let Ok(Some((serial, path, owner, anchor, group))) =
            this.update(cx, |this, _| this.next_menu_popup())
        {
            let content: crate::popup::Content = {
                let (this, path) = (this.clone(), path.clone());
                Rc::new(move |max_h, window, cx| {
                    this.update(cx, |this, cx| {
                        this.render_menu_popup(serial, &path, max_h, window, cx)
                    })
                    .ok()
                    .flatten()
                })
            };
            let dismiss: crate::popup::Dismiss = {
                let this = this.clone();
                Rc::new(move |cx| {
                    let _ = owner.update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| this.close_menu(window, cx))
                    });
                })
            };
            let state = crate::popup::open(owner, anchor, &group, content, dismiss, true, cx);
            let kept = this
                .update(cx, |this, _| this.keep_menu_popup(serial, path, state))
                .unwrap_or(false);
            if !kept {
                break;
            }
        }
        let _ = this.update(cx, |this, _| {
            if let Some(menu) = this.context_menu.as_mut() {
                menu.opening = false;
            }
        });
    }

    /// The next level to open a window for: its menu, path, owner window, place and group.
    #[allow(
        clippy::type_complexity,
        reason = "read once, right where it is taken apart"
    )]
    fn next_menu_popup(
        &self,
    ) -> Option<(
        u64,
        Vec<usize>,
        AnyWindowHandle,
        crate::popup::Anchor,
        crate::popup::Group,
    )> {
        let menu = self.context_menu.as_ref()?;
        let level = menu.popups.len();
        let path = menu.open.get(..level)?.to_vec();
        let anchor = match level.checked_sub(1) {
            None => crate::popup::Anchor::At(menu.at.0, menu.at.1),
            Some(parent) => {
                let key = parent * LEVEL_STRIDE + path[parent];
                let geometry = Rc::downgrade(&menu.geometry);
                crate::popup::Anchor::Beside {
                    parent: Rc::downgrade(&menu.popups[parent].state),
                    row: Rc::new(move || geometry.upgrade()?.borrow().rows.get(&key).copied()),
                    pad: theme::s_4(),
                }
            }
        };
        Some((menu.serial, path, menu.owner?, anchor, menu.group.clone()))
    }

    /// Keeps the window just opened for the level at `path`, if the menu still shows that level.
    fn keep_menu_popup(
        &mut self,
        serial: u64,
        path: Vec<usize>,
        state: Option<Rc<crate::popup::PopupState>>,
    ) -> bool {
        let Some(menu) = self.context_menu.as_mut().filter(|m| m.serial == serial) else {
            return false;
        };
        let Some(state) = state else {
            menu.popup_failed = true;
            return false;
        };
        if menu.popups.len() != path.len() || !menu.open.starts_with(&path) {
            return false;
        }
        menu.popups.push(LevelPopup { path, state });
        true
    }

    /// What the window of the level at `path` of menu `serial` draws: the level, or the level
    /// fading out once the menu was dismissed. `None` once it is gone, and the window closes.
    fn render_menu_popup(
        &self,
        serial: u64,
        path: &[usize],
        max_h: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let level = path.len();
        let shows = |m: &&ContextMenu| m.serial == serial && m.open.starts_with(path);
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        let id = path
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(".");
        if let Some(menu) = self.context_menu.as_ref().filter(shows) {
            // Fades in; replays when switching submenus.
            let panel = Self::render_menu_panel(menu, level, &pinned, max_h, cx);
            return Some(
                panel
                    .with_animation(
                        ElementId::Name(format!("menu-{serial}-{id}").into()),
                        crate::motion::menu_enter(),
                        crate::motion::fade_in,
                    )
                    .into_any_element(),
            );
        }
        let ghost = self.closing_menu.as_ref().map(|g| &g.value).filter(shows)?;
        // Dismissed: presses already go to what is below while it fades.
        crate::popup::pass_mouse_through(window);
        let panel = Self::render_menu_panel(ghost, level, &pinned, max_h, cx);
        Some(crate::motion::leave(
            panel,
            ElementId::Name(format!("menu-{serial}-{id}-leave").into()),
            0.,
            0.,
        ))
    }

    /// One menu level: frame and rows. Row and frame bounds are recorded in `geometry` after paint
    /// for submenu alignment.
    fn render_menu_panel(
        menu: &ContextMenu,
        level: usize,
        pinned: &[String],
        max_h: Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let entries = menu.level(level);
        // "More options" showing its one-time line: it doesn't come back.
        if level == 1
            && menu.pin_hint
            && menu.open.first() == menu.shell_slot.as_ref()
            && !menu.pin_hint_shown.replace(true)
        {
            cx.defer(|cx| {
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(|st| st.pin_hint_seen = true, cx);
                });
            });
        }
        let current = level == menu.open.len();
        let open_here = menu.open.get(level).copied();
        let marks = entries.iter().any(MenuEntry::has_mark);
        let mut rows: Vec<AnyElement> = entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                let highlighted =
                    (current && menu.highlighted == Some(ix)) || open_here == Some(ix);
                Self::render_menu_row(menu, level, ix, entry, highlighted, marks, pinned, cx)
            })
            .collect();
        let footer = rows.split_off(footer_start(entries));
        let geometry = Rc::clone(&menu.geometry);
        // A level taller than the screen is capped at `max_h`: its rows scroll, and the buttons
        // at its bottom stay in view under them. One that fits lays out as a plain stack.
        div()
            .id(("context-menu", level))
            .role(gpui::Role::Menu)
            .when(level == 0, |d| d.aria_label(t!("a11y.context_menu")))
            .occlude()
            .min_w(crate::theme::scaled_px(240.))
            .max_w(crate::theme::scaled_px(420.))
            .max_h(max_h)
            .flex()
            .flex_col()
            .py(theme::s_4())
            .rounded(theme::r_12())
            .bg(theme::overlay())
            .shadow(theme::elev_menu())
            .ui_font()
            .text_size(theme::fs_14())
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
                .top_0()
                .left_0()
                .size_full(),
            )
            // Only rows in here, one child per entry, so `scroll_to_item` takes an entry's index.
            // The measuring canvas stays outside: inside, set off by the padding, it made a level
            // that fits scroll by that much.
            .child(
                div()
                    .id(("context-menu-rows", level))
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&menu.scroll(level))
                    .children(rows),
            )
            .when(!footer.is_empty(), |d| d.child(div().flex_none().children(footer)))
    }

    /// Bottom row of icon buttons: icon above label, equal widths.
    fn render_action_row(
        menu: &ContextMenu,
        items: &[MenuItem],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .mx(theme::s_4())
            .flex()
            .gap(theme::s_2())
            .children(items.iter().enumerate().map(|(ix, item)| {
                let command = item.command;
                let enabled = item.enabled;
                div()
                    .id(("menu-action", ix))
                    .debug_selector(move || format!("menu-action-{command:?}"))
                    .a11y_menu_item(item.label.clone(), enabled, false, false)
                    .flex_1()
                    .py(theme::s_6())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(theme::s_4())
                    .rounded(theme::r_8())
                    .text_size(theme::fs_12())
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .text_color(theme::text_muted())
                            .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
                            .on_click(crate::popup::listener_in(
                                menu.owner,
                                cx,
                                move |this, event: &ClickEvent, window, cx| {
                                    this.run_menu_command(
                                        click_command(command, event.modifiers().shift),
                                        window,
                                        cx,
                                    );
                                },
                            ))
                    })
                    .when(!enabled, |d| d.text_color(theme::text_disabled()))
                    // The icon in a box of its size, which the key is placed against. Every
                    // icon button has one (`decorate`).
                    .children(item.glyph.map(|glyph| {
                        div()
                            .relative()
                            .flex_none()
                            .size(theme::icon())
                            .child(svg().path(glyph.path()).size(theme::icon()).text_color(
                                if enabled {
                                    theme::text()
                                } else {
                                    theme::text_disabled()
                                },
                            ))
                            .children(
                                button_key(item.access, menu.cues)
                                    .map(|key| key_badge(key, enabled, command)),
                            )
                    }))
                    .child(access_text(
                        item.label.clone(),
                        button_text_access(item.access),
                        menu.cues,
                    ))
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
            .size(crate::theme::scaled_px(22.))
            .mr(-theme::s_4())
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::r_4())
            .cursor_pointer()
            .when(!on, |d| d.invisible().group_hover(group.clone(), gpui::Styled::visible))
            .hover(|s| s.bg(theme::wash_strong()))
            .tooltip(crate::workspace::tooltip(if on {
                t!("menu.unpin_command")
            } else {
                t!("menu.pin_command")
            }))
            .child(svg().path(Icon::Pin.path()).size(theme::icon_sm()).text_color(if on {
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

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "a row needs its menu, position, entry and highlight state, and draws its parts \
                  in order"
    )]
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
            return Self::render_action_row(menu, items, cx);
        }
        let Some(RowParts {
            label,
            access,
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
                .my(theme::s_4())
                .mx(theme::s_8())
                .bg(theme::line())
                .into_any_element();
        };
        let key = level * LEVEL_STRIDE + ix;
        let geometry = Rc::clone(&menu.geometry);
        let popups = menu.group.clone();
        let mark = marks.then(|| row_mark(icon, glyph, checked));
        // Chevrons point left when this level's submenu opens to the left, and in a level that
        // itself opened to the left.
        let chevron_left = menu
            .popups
            .get(level + 1)
            .or_else(|| menu.popups.get(level))
            .is_some_and(|p| p.state.opens_left());
        let group: SharedString = format!("menu-row-{key}").into();
        let pin_button = pin.map(|pin| Self::pin_button(key, pin, pinned, group.clone(), cx));
        div()
            .id(("menu-item", key))
            .a11y_menu_item(label.clone(), enabled, checked, highlighted)
            .when(submenu, |d| {
                d.a11y_open_close(
                    menu.open.get(level) == Some(&ix),
                    cx.entity().downgrade(),
                    move |this, _, cx| this.a11y_toggle_submenu(level, ix, cx),
                )
            })
            .when_some(shortcut, gpui::StatefulInteractiveElement::aria_keyshortcuts)
            .group(group)
            .relative()
            .h(theme::h_ctl_sm() + theme::s_4())
            .mx(theme::s_4())
            .px(theme::s_8())
            .flex()
            .items_center()
            .gap(theme::s_8())
            .rounded(theme::r_8())
            .when(submenu, |d| {
                d.child(
                    canvas(
                        move |bounds, _, cx| {
                            // The row moved (e.g. "Show more options" shifted down after a pin):
                            // its submenu moves along.
                            let old = geometry.borrow_mut().rows.insert(key, bounds);
                            if old != Some(bounds) {
                                popups.refresh(cx);
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
                    .on_click(crate::popup::listener_in(
                        menu.owner,
                        cx,
                        move |this, event: &ClickEvent, window, cx| match command {
                            Some(command) => this.run_menu_command(
                                click_command(command, event.modifiers().shift),
                                window,
                                cx,
                            ),
                            None => this.click_submenu(level, ix, cx),
                        },
                    ))
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
                    .child(access_text(label, access, menu.cues)),
            )
            .children(shortcut.map(|s| {
                div()
                    .flex_none()
                    .pl(theme::s_16())
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(s)
            }))
            .children(pin_button)
            // Chevron points left when the submenu opens to the left.
            .when(submenu, |d| d.child(submenu_chevron(chevron_left)))
            .into_any_element()
    }
}

/// Where the part of a level that stays in view while its rows scroll begins: the row of icon
/// buttons at its end and the separator above it. `entries.len()` when it has none.
const fn footer_start(entries: &[MenuEntry]) -> usize {
    match entries {
        [.., MenuEntry::Separator, MenuEntry::Actions(_)] => entries.len() - 2,
        [.., MenuEntry::Actions(_)] => entries.len() - 1,
        _ => entries.len(),
    }
}

/// What a row draws (separators have none).
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent parts a row may draw"
)]
struct RowParts {
    label: SharedString,
    access: Option<Access>,
    enabled: bool,
    icon: Option<MenuIcon>,
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

/// Name of the menu level the keys move in: the context menu, or the submenu open innermost.
fn menu_a11y_label(menu: &ContextMenu) -> SharedString {
    let Some(&row) = menu.open.last() else {
        return t!("a11y.context_menu").into();
    };
    menu.level(menu.open.len() - 1)
        .get(row)
        .and_then(RowParts::of)
        .map_or_else(|| t!("a11y.context_menu").into(), |parts| parts.label)
}

/// The rows of the level the keys move in, as a screen reader is told them: items that aren't
/// drawn, under the element that has the keys, the highlighted one reported as focused.
///
/// The levels are drawn in popup windows that are never activated, and a screen reader follows
/// the focus of the active window only; the keys stay in the menu's own window, so its rows are
/// told from there too (#322).
fn menu_a11y_items(menu: &ContextMenu) -> Vec<AnyElement> {
    let mut items = Vec::new();
    for (ix, entry) in menu.level(menu.open.len()).iter().enumerate() {
        if let MenuEntry::Actions(actions) = entry {
            for (ax, item) in actions.iter().enumerate() {
                items.push(
                    div()
                        .id(("menu-a11y-action", ax))
                        .a11y_menu_item(item.label.clone(), item.enabled, false, false)
                        .into_any_element(),
                );
            }
        } else if let Some(parts) = RowParts::of(entry) {
            items.push(
                div()
                    .id(("menu-a11y-item", ix))
                    .a11y_menu_item(
                        parts.label,
                        parts.enabled,
                        parts.checked,
                        menu.highlighted == Some(ix),
                    )
                    .when(parts.submenu, |d| d.aria_expanded(false))
                    .into_any_element(),
            );
        }
    }
    items
}

impl RowParts {
    fn of(entry: &MenuEntry) -> Option<Self> {
        match entry {
            MenuEntry::Separator | MenuEntry::Actions(_) => None,
            MenuEntry::Item(it) => Some(Self {
                label: it.label.clone(),
                access: it.access,
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
                access,
                enabled,
                icon,
                glyph,
                pin,
                ..
            } => Some(Self {
                label: label.clone(),
                access: *access,
                enabled: *enabled,
                icon: icon.clone(),
                checked: false,
                bold: false,
                danger: false,
                shortcut: None,
                submenu: true,
                command: None,
                glyph: *glyph,
                pin: pin.clone(),
            }),
        }
    }
}

/// Icon or check mark on the left of a row. If any row in a level has one, the whole level reserves
/// the space.
fn row_mark(icon: Option<MenuIcon>, glyph: Option<Icon>, checked: bool) -> gpui::Div {
    div()
        .size(theme::icon())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .children(icon.map(|icon| img(icon.shown()).size(theme::icon())))
        .children(glyph.filter(|_| !checked).map(|glyph| {
            svg()
                .path(glyph.path())
                .size(theme::icon())
                .text_color(theme::text_muted())
        }))
        .when(checked, |d| {
            d.child(
                svg()
                    .path(Icon::Check.path())
                    .size(theme::icon())
                    .text_color(theme::text_muted()),
            )
        })
}

/// Windows menu items that only File Explorer itself may run: from any other program Windows
/// answers "access denied" (#344). Files hides them too.
const EXPLORER_ONLY_VERBS: &[&str] = &["pintostartscreen", "unpinfromstartscreen"];

/// Drops items from "Show more options" that Arke's menu already has (open, cut, copy, delete,
/// ...), matched by standard Windows verbs like Files does, and the ones only File Explorer may
/// run. Only the top level is filtered.
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
        .chain(EXPLORER_ONLY_VERBS.iter().copied())
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
fn pref_key(entry: &MenuEntry) -> Option<&'static str> {
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
            C::ViewDetails
            | C::ViewIcons
            | C::ViewColumns
            | C::SortBy(_)
            | C::SortDirection(_)
            | C::Refresh => Some("view"),
            C::KeepOnDevice | C::FreeUpSpace => Some("cloud"),
            C::AddToShelf => Some("shelf"),
            C::MoveToNewFolder => Some("move_new_folder"),
            _ => None,
        },
        MenuEntry::Submenu { shell: true, .. } => Some("more_options"),
        // "New" is a basic entry: always shown.
        MenuEntry::Submenu { key, .. } => (*key).filter(|k| *k != "new"),
        MenuEntry::Actions(_) | MenuEntry::Separator => None,
    }
}

/// Where an entry goes in the order set in Settings › Context menu (`menu_order.rs`): its key
/// there, or staying with the entry before it.
fn order_slot(entry: &MenuEntry) -> crate::menu_order::Slot {
    use crate::menu_order::Slot;
    match entry {
        MenuEntry::Separator => Slot::Separator,
        MenuEntry::Actions(_) => Slot::Last,
        MenuEntry::Submenu { shell: true, .. } => Slot::Key("more_options"),
        MenuEntry::Submenu { key: Some(key), .. } => Slot::Key(key),
        MenuEntry::Item(item) if item.command == MenuCommand::Open => Slot::Key("open"),
        entry => pref_key(entry).map_or(Slot::Follow, Slot::Key),
    }
}

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

/// Removes entries disabled in settings, then tidies separators (none at the ends, no repeats).
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
        .size(theme::icon())
        .flex_none()
        .text_color(theme::text_subtle())
        .when(left, |svg| {
            svg.with_transformation(gpui::Transformation::rotate(gpui::radians(
                std::f32::consts::PI,
            )))
        })
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

/// Remembers how pinned row `key` looked, for drawing it ahead next time (`menu_pin_looks`).
fn remember_pin(key: &str, entry: &MenuEntry) {
    let (icon, submenu) = match entry {
        MenuEntry::Item(item) => (item.icon.as_ref().map(MenuIcon::original), false),
        MenuEntry::Submenu { icon, .. } => (icon.as_ref().map(MenuIcon::original), true),
        MenuEntry::Actions(_) | MenuEntry::Separator => return,
    };
    crate::menu_pin_looks::remember(key, icon, submenu);
}

/// Whether pinned row `key` was last seen as a whole submenu: drawn ahead as one
/// (`coming_row`), and in the settings preview.
pub(crate) fn pin_is_submenu(key: &str) -> bool {
    crate::menu_pin_looks::is_submenu(key)
}

/// The row of a pinned command known to show, before its source answers: its label from the
/// pin key (left out when the key shortened a quoted part), its icon as last seen. A pinned
/// submenu shows "Loading…" inside until it is in.
fn coming_row(key: &str, ix: usize) -> Option<MenuEntry> {
    let label = key.rsplit(" › ").next()?.trim();
    if label.is_empty() || label.contains('…') {
        return None;
    }
    let icon = crate::menu_pin_looks::icon(key).map(MenuIcon::new);
    if pin_is_submenu(key) {
        return Some(MenuEntry::Submenu {
            label: label.to_string().into(),
            access: None,
            enabled: true,
            icon,
            glyph: None,
            children: vec![placeholder(t!("menu.loading"))],
            shell: true,
            lazy: None,
            key: None,
            pin: Some(key.to_string()),
        });
    }
    Some(MenuEntry::Item(MenuItem {
        label: label.to_string().into(),
        access: None,
        shortcut: None,
        command: MenuCommand::PinPending(ix),
        enabled: true,
        icon,
        glyph: None,
        checked: false,
        bold: false,
        danger: false,
        pin: Some(key.to_string()),
        verb: None,
    }))
}

/// Pin key of a Windows menu row, an item's or a whole submenu's.
fn entry_pin(entry: &MenuEntry) -> Option<&str> {
    match entry {
        MenuEntry::Item(item) => item.pin.as_deref(),
        MenuEntry::Submenu { pin, .. } => pin.as_deref(),
        MenuEntry::Actions(_) | MenuEntry::Separator => None,
    }
}

/// Finds the row with pin key `key` in a Windows menu, an item or a whole submenu, including
/// inside submenus.
fn find_pinned<'a>(entries: &'a [MenuEntry], key: &str) -> Option<&'a MenuEntry> {
    entries.iter().find_map(|entry| {
        if entry_pin(entry) == Some(key) {
            return Some(entry);
        }
        match entry {
            MenuEntry::Submenu { children, .. } => find_pinned(children, key),
            _ => None,
        }
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
    let mut keep = item(
        lab("keep", t!("menu.keep_on_device")),
        None,
        MenuCommand::KeepOnDevice,
    );
    if let MenuEntry::Item(row) = &mut keep {
        row.checked = kept;
        row.glyph = glyph_for(MenuCommand::KeepOnDevice);
    }
    let mut free = item(
        lab("free", t!("menu.free_up_space")),
        None,
        MenuCommand::FreeUpSpace,
    );
    if let MenuEntry::Item(row) = &mut free {
        row.glyph = glyph_for(MenuCommand::FreeUpSpace);
    }
    entries.splice(at..at, [keep, free, MenuEntry::Separator]);
}

fn more_options() -> MenuEntry {
    let label = t!("menu.more_options");
    let access = own_access("more", &label, None);
    MenuEntry::Submenu {
        label: label.into(),
        access,
        enabled: true,
        icon: None,
        glyph: None,
        children: vec![placeholder(t!("menu.loading"))],
        shell: true,
        lazy: None,
        key: None,
        pin: None,
    }
}

/// Zip file stem: a folder's name, or a file's name without extension (`report.docx` becomes
/// `report.zip`).
/// A file name inside a menu label: a long one keeps its start and end (the extension) around an
/// ellipsis, so the menu keeps its usual width (#87).
fn menu_name(name: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation as _;
    const MAX: usize = 28;
    const HEAD: usize = 14;
    const TAIL: usize = 10;
    // By grapheme, so an emoji sequence or a letter with its accents is never split.
    let graphemes: Vec<&str> = name.graphemes(true).collect();
    if graphemes.len() <= MAX {
        return name.to_owned();
    }
    let head = graphemes[..HEAD].concat();
    let tail = graphemes[graphemes.len() - TAIL..].concat();
    format!("{head}…{tail}")
}

fn zip_stem_for(path: &std::path::Path, is_dir: bool) -> String {
    let stem = if is_dir {
        path.file_name()
    } else {
        path.file_stem().or_else(|| path.file_name())
    };
    stem.map_or_else(|| "archive".into(), |s| s.to_string_lossy().into_owned())
}

/// The archive name for several items that all sit directly in `dir`: the folder's name, as
/// WinRAR, 7-Zip and Bandizip do (#409). `None` for one item, for items from other folders (a
/// nested tree view), and for a folder without a name (a drive or share root); the first item's
/// name is used then.
fn zip_stem_for_selection(dir: &std::path::Path, targets: &[(PathBuf, bool)]) -> Option<String> {
    if targets.len() < 2 || targets.iter().any(|(p, _)| p.parent() != Some(dir)) {
        return None;
    }
    let name = dir.file_name()?.to_string_lossy();
    (!name.trim().is_empty()).then(|| name.into_owned())
}

/// The archive name (without `.zip`) for `targets` compressed in `dir`: what the menu label says
/// and what [`Explorer::start_compress`] uses. `targets` is not empty.
fn zip_stem_for_targets(dir: Option<&std::path::Path>, targets: &[(PathBuf, bool)]) -> String {
    dir.and_then(|dir| zip_stem_for_selection(dir, targets))
        .unwrap_or_else(|| zip_stem_for(&targets[0].0, targets[0].1))
}

fn pin_label(pinned: bool) -> Label {
    if pinned {
        lab("fav_remove", t!("sidebar.remove_favorite"))
    } else {
        lab("fav_add", t!("sidebar.add_favorite"))
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

#[cfg(test)]
mod submenu_tests {
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
}

#[cfg(test)]
mod menu_name_tests {
    use super::menu_name;

    #[test]
    fn long_names_keep_their_start_and_extension() {
        assert_eq!(menu_name("short.zip"), "short.zip");
        let hash = "c4b7b6d1f5e8ce048749e09b4dae08795432903519a7e8c4f1d2b3a4c5d6e7f8.zip";
        assert_eq!(menu_name(hash), "c4b7b6d1f5e8ce…d6e7f8.zip");
        // Characters, not bytes: CJK names shorten without splitting a character.
        let cjk = "這是一個非常非常長的中文檔案名稱用來測試右鍵選單會不會變得很寬.zip";
        let short = menu_name(cjk);
        assert_eq!(short.chars().count(), 25);
        assert!(short.ends_with("變得很寬.zip"));
        // A family emoji (one grapheme of seven characters) stays whole at either end.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
        let emoji = format!("{family}{}{family}.zip", "x".repeat(40));
        let short = menu_name(&emoji);
        assert!(short.starts_with(family), "{short}");
        assert!(short.ends_with(&format!("x{family}.zip")), "{short}");
    }
}

#[cfg(test)]
mod compress_name_tests {
    use super::zip_stem_for_selection;
    use std::path::{Path, PathBuf};

    fn items(paths: &[&str]) -> Vec<(PathBuf, bool)> {
        paths.iter().map(|p| (PathBuf::from(p), false)).collect()
    }

    #[test]
    fn several_items_take_the_folder_name() {
        let targets = items(&[r"C:\Work\Report\a.txt", r"C:\Work\Report\b.txt"]);
        assert_eq!(
            zip_stem_for_selection(Path::new(r"C:\Work\Report"), &targets).as_deref(),
            Some("Report")
        );
    }

    #[test]
    fn one_item_keeps_its_own_name() {
        let targets = items(&[r"C:\Work\Report\a.txt"]);
        assert_eq!(
            zip_stem_for_selection(Path::new(r"C:\Work\Report"), &targets),
            None
        );
    }

    #[test]
    fn a_root_has_no_folder_name() {
        let targets = items(&[r"C:\a.txt", r"C:\b.txt"]);
        assert_eq!(zip_stem_for_selection(Path::new(r"C:\"), &targets), None);
        let share = items(&[r"\\srv\share\a.txt", r"\\srv\share\b.txt"]);
        assert_eq!(
            zip_stem_for_selection(Path::new(r"\\srv\share"), &share),
            None
        );
    }

    #[test]
    fn items_from_other_folders_keep_the_first_name() {
        let targets = items(&[r"C:\Work\Report\a.txt", r"C:\Work\Report\sub\b.txt"]);
        assert_eq!(
            zip_stem_for_selection(Path::new(r"C:\Work\Report"), &targets),
            None
        );
    }
}

/// Access keys in Arke's own menus (#422).
#[cfg(test)]
mod access_tests {
    use super::*;

    const LOCALES: [&str; 7] = ["en", "zh-TW", "zh-CN", "de", "es", "pt-BR", "ja"];

    /// No key for these: every letter of their words was taken by something they share a menu
    /// with.
    const NO_KEY: [(&str, &str); 6] = [
        ("es", "copy_rel"),
        ("es", "link"),
        ("es", "keep"),
        ("es", "free"),
        ("es", "compare"),
        ("pt-BR", "compare"),
    ];

    /// The own items each menu can show at once, by their `access.*` key. An item that never shows
    /// with another (a folder's "Add to favorites" and "Remove from favorites") is in its own
    /// copy of the menu. "Open with <app>", the pane items and the PC search ones have no key.
    const BUTTONS: &[&str] = &["new_folder", "cut", "copy", "rename", "delete"];

    const MENUS: &[(&str, &str)] = &[
        (
            "folder",
            "open open_tab new search_folder fav_add terminal compress move_new new_window shelf \
         copy_path reveal props more new_folder cut copy rename delete copy_rel link keep \
         free",
        ),
        (
            "folder_rm",
            "open open_tab new search_folder fav_remove terminal compress move_new new_window \
         shelf copy_path reveal props more new_folder cut copy rename delete copy_rel link \
         keep free",
        ),
        (
            "file_archive",
            "open compress move_new choose_app shelf copy_path reveal props more cut copy rename \
         delete extract_to extract_here copy_rel link keep free",
        ),
        (
            "files_archives",
            "open compress move_new shelf copy_path reveal props more cut copy rename delete \
         extract_each extract_here copy_rel keep free",
        ),
        (
            "file_image",
            "open compress move_new choose_app shelf copy_path reveal props more cut copy rename \
         delete set_bg copy_rel link keep free",
        ),
        (
            "file_json",
            "open compress move_new choose_app shelf copy_path reveal props more cut copy rename \
         delete import copy_rel link keep free",
        ),
        (
            "file_lnk",
            "open compress move_new choose_app shelf copy_path reveal props more cut copy rename \
         delete file_location copy_rel link keep free",
        ),
        (
            "background",
            "new paste redo_op view sort_by refresh copy_path open_explorer terminal fav_add \
         props more undo",
        ),
        (
            "background_rm",
            "new paste redo_op view sort_by refresh copy_path open_explorer terminal fav_remove \
         props more undo",
        ),
        ("view_sub", "view_details view_icons view_columns"),
        ("new_bg", "new_folder new_text"),
        ("new_in", "new_file_in new_folder_in"),
        ("sort_sub", "asc desc"),
        (
            "path",
            "copy_address copy_as_path edit_address clear_history",
        ),
        ("drop", "drop_move drop_copy drop_shortcut drop_cancel"),
        ("header", "reset_cols"),
        (
            "background_op",
            "new paste redo_op view sort_by refresh copy_path open_explorer terminal fav_add \
         props more undo_op",
        ),
        (
            "background_rm_op",
            "new paste redo_op view sort_by refresh copy_path open_explorer terminal fav_remove \
         props more undo_op",
        ),
    ];

    /// The `access.*` key of each row and the `menu.*`-style key of the label it comes with.
    const LABELS: &str = "open=menu.open open_tab=menu.open_in_new_tab new=menu.new new_folder=menu.new_folder \
         new_text=menu.new_text_file new_file_in=menu.new_file_in \
         new_folder_in=menu.new_folder_in search_folder=menu.search_in_folder \
         fav_add=sidebar.add_favorite fav_remove=sidebar.remove_favorite \
         terminal=menu.open_terminal extract_to=menu.extract_to extract_each=menu.extract_each \
         extract_here=menu.extract_here set_bg=menu.set_background \
         import=menu.import_settings compress=menu.compress move_new=menu.move_to_new_folder \
         new_window=menu.open_new_window link=menu.go_to_link_target \
         file_location=menu.open_file_location choose_app=menu.choose_app \
         shelf=menu.add_to_shelf copy_path=menu.copy_path copy_rel=menu.copy_relative_path \
         reveal=menu.reveal_in_explorer props=menu.properties more=menu.more_options \
         keep=menu.keep_on_device free=menu.free_up_space cut=menu.cut copy=menu.copy \
         rename=menu.rename delete=menu.delete paste=menu.paste undo=menu.undo \
         undo_op=menu.undo_op redo_op=menu.redo_op refresh=menu.refresh \
         open_explorer=menu.open_in_explorer view=menu.view view_details=menu.view_details \
         view_icons=view.icons view_columns=settings.view_columns sort_by=settings.sort_by \
         asc=settings.ascending desc=settings.descending copy_address=menu.copy_address \
         copy_as_path=menu.copy_as_path edit_address=menu.edit_address \
         clear_history=menu.clear_history reset_cols=menu.reset_column_order \
         drop_move=dnd.menu_move drop_copy=dnd.menu_copy drop_shortcut=dnd.menu_shortcut \
         drop_cancel=dnd.menu_cancel sync=compare.sync_tip compare=compare.compare_tip";

    /// The pane row's "⋯" adds to a menu (#474): "Sort by" to the selection's, and in a split
    /// tab synchronized browsing and compare to it or to the folder's.
    fn more_menus() -> Vec<(String, String)> {
        const SELECTION: [&str; 7] = [
            "folder",
            "folder_rm",
            "file_archive",
            "files_archives",
            "file_image",
            "file_json",
            "file_lnk",
        ];
        MENUS
            .iter()
            .filter_map(|(menu, names)| {
                let added = if menu.starts_with("background") {
                    "sync compare"
                } else if SELECTION.contains(menu) {
                    "sort_by sync compare"
                } else {
                    return None;
                };
                Some((format!("{menu}, from ⋯"), format!("{names} {added}")))
            })
            .collect()
    }

    fn letter(locale: &str, name: &str) -> Option<char> {
        let key = format!("access.{name}");
        let text = t!(key.as_str(), locale = locale);
        assert!(text != key.as_str(), "{locale}: {key} is not translated");
        text.chars().next().filter(|c| c.is_alphanumeric())
    }

    #[test]
    fn no_two_items_of_a_menu_share_an_access_key() {
        let menus: Vec<(String, String)> = MENUS
            .iter()
            .map(|(menu, names)| ((*menu).to_owned(), (*names).to_owned()))
            .chain(more_menus())
            .collect();
        for locale in LOCALES {
            for (menu, names) in &menus {
                let mut seen: Vec<(char, &str)> = Vec::new();
                for name in names.split_whitespace() {
                    let Some(letter) = letter(locale, name) else {
                        assert!(
                            NO_KEY.iter().any(|&(l, n)| l == locale && n == name),
                            "{locale}: access.{name} has no letter"
                        );
                        continue;
                    };
                    let letter = letter.to_ascii_lowercase();
                    // A row may share its letter with a top-row button: the letter only
                    // highlights the row (`access_hit`). Two rows or two buttons may not.
                    // ("New folder" is a row in the "New" menu.)
                    let is_button = |n: &str| BUTTONS.contains(&n) && menu != "new_bg";
                    let button = is_button(name);
                    if let Some((_, other)) = seen
                        .iter()
                        .find(|(l, o)| *l == letter && is_button(o) == button)
                    {
                        panic!("{locale}, {menu}: {other} and {name} share the key {letter}");
                    }
                    seen.push((letter, name));
                }
            }
        }
    }

    /// A letter that is not in its label would fall back to `(K)` after the text, which is right
    /// but not what our own languages are meant to do: every Western letter is in its word.
    #[test]
    fn every_letter_of_a_western_language_is_in_its_label() {
        for locale in ["en", "de", "es", "pt-BR"] {
            for pair in LABELS.split_whitespace() {
                let (name, label_key) = pair.split_once('=').expect("name=key");
                let Some(letter) = letter(locale, name) else {
                    continue;
                };
                let label = t!(label_key, locale = locale, name = "", dest = "", op = "");
                let access = access_in(locale, name, &label, None).expect("a letter");
                assert!(
                    access
                        .at
                        .is_some_and(|at| label[at..].starts_with(access.key)),
                    "{locale}: {letter} is not in {label:?} ({name})"
                );
            }
        }
    }

    #[test]
    fn chinese_and_japanese_write_the_key_after_the_text() {
        for locale in ["zh-TW", "zh-CN", "ja"] {
            for pair in LABELS.split_whitespace() {
                let (name, label_key) = pair.split_once('=').expect("name=key");
                let label = t!(label_key, locale = locale, name = "", dest = "", op = "");
                let access = access_in(locale, name, &label, None).expect("a letter");
                assert_eq!(access.at, None, "{locale}: {name}");
                assert!(access.key.is_ascii_uppercase(), "{locale}: {name}");
            }
        }
    }

    #[test]
    fn the_key_is_the_first_occurrence_of_the_letter_outside_what_was_put_in() {
        let at = |letter, label, skip| derive_access(letter, label, skip, false);
        assert_eq!(
            at('c', "Copy", None),
            Access {
                key: 'C',
                at: Some(0)
            }
        );
        assert_eq!(
            at('P', "Copy path", None),
            Access {
                key: 'p',
                at: Some(2)
            }
        );
        // The name put in is passed over: "Move to a" has its own `o` first.
        assert_eq!(at('o', "Move to Zoo", Some("Zoo")).at, Some(1));
        assert_eq!(at('z', "Move to Zoo", Some("Zoo")).at, None);
        // A letter the label doesn't have, and a language that writes it after the text.
        assert_eq!(at('q', "Copy", None), Access { key: 'Q', at: None });
        assert_eq!(
            derive_access('c', "複製", None, true),
            Access { key: 'C', at: None }
        );
    }

    fn keyed(label: &str, letter: char, command: MenuCommand) -> MenuEntry {
        match item(label.to_owned(), None, command) {
            MenuEntry::Item(row) => MenuEntry::Item(MenuItem {
                access: Some(derive_access(letter, label, None, false)),
                ..row
            }),
            other => other,
        }
    }

    fn off(label: &str, letter: char, command: MenuCommand) -> MenuEntry {
        match keyed(label, letter, command) {
            MenuEntry::Item(row) => MenuEntry::Item(MenuItem {
                enabled: false,
                ..row
            }),
            other => other,
        }
    }

    fn button(label: &str, letter: char, command: MenuCommand, enabled: bool) -> MenuItem {
        MenuItem {
            access: Some(derive_access(letter, label, None, false)),
            ..action(label.to_owned(), command, enabled)
        }
    }

    fn submenu(label: &str, letter: char, enabled: bool) -> MenuEntry {
        MenuEntry::Submenu {
            label: label.to_owned().into(),
            access: Some(derive_access(letter, label, None, false)),
            enabled,
            icon: None,
            glyph: None,
            children: Vec::new(),
            shell: false,
            lazy: None,
            key: None,
            pin: None,
        }
    }

    #[test]
    fn a_letter_with_one_match_runs_it_shift_or_not() {
        use MenuCommand as C;
        let entries = [
            keyed("Open", 'o', C::Open),
            keyed("Cut", 't', C::Cut),
            keyed("Copy", 'c', C::Copy),
        ];
        assert_eq!(access_hit(&entries, None, 'c'), AccessHit::Run(C::Copy));
        assert_eq!(access_hit(&entries, Some(0), 'C'), AccessHit::Run(C::Copy));
        assert_eq!(access_hit(&entries, None, 'T'), AccessHit::Run(C::Cut));
    }

    #[test]
    fn open_in_terminal_takes_t_and_shares_it_with_cut() {
        use MenuCommand as C;
        let label = t!("menu.open_terminal", locale = "en");
        let access = access_in("en", "terminal", &label, None).expect("a letter");
        assert_eq!(access.key, 'T');
        let entries = [
            keyed("Cut", 't', C::Cut),
            keyed("Open in Terminal", 't', C::OpenTerminal),
        ];
        // The first T lights one, the next moves to the other, and a third wraps.
        assert_eq!(access_hit(&entries, None, 't'), AccessHit::Move(0));
        assert_eq!(access_hit(&entries, Some(0), 't'), AccessHit::Move(1));
        assert_eq!(access_hit(&entries, Some(1), 't'), AccessHit::Move(0));
        // Alone, it runs at once.
        assert_eq!(
            access_hit(&entries[1..], None, 't'),
            AccessHit::Run(C::OpenTerminal)
        );
    }

    #[test]
    fn shift_with_deletes_key_deletes_for_good_and_nothing_else_changes() {
        use MenuCommand as C;
        let buttons = MenuEntry::Actions(vec![
            button("Cut", 't', C::Cut, true),
            button("Delete", 'd', C::Delete, true),
        ]);
        let entries = [keyed("Open", 'o', C::Open), buttons];
        let hit = |typed, shift| shift_hit(access_hit(&entries, None, typed), shift);
        assert_eq!(hit('D', true), AccessHit::Run(C::DeleteForever));
        assert_eq!(hit('d', false), AccessHit::Run(C::Delete));
        assert_eq!(hit('T', true), AccessHit::Run(C::Cut));
        assert_eq!(hit('O', true), AccessHit::Run(C::Open));
        assert_eq!(hit('Z', true), AccessHit::Nothing);
    }

    #[test]
    fn a_button_sharing_a_rows_key_is_not_underlined() {
        use MenuCommand as C;
        let mut entries = [
            keyed("Open in Terminal", 't', C::OpenTerminal),
            MenuEntry::Actions(vec![
                button("Cut", 't', C::Cut, true),
                button("Copy", 'c', C::Copy, true),
            ]),
        ];
        decorate(&mut entries);
        let MenuEntry::Actions(items) = &entries[1] else {
            return;
        };
        assert_eq!(items[0].access, None);
        assert!(items[1].access.is_some());
        // The row is now the only T, so it runs at once.
        assert_eq!(
            access_hit(&entries, None, 't'),
            AccessHit::Run(C::OpenTerminal)
        );
    }

    #[test]
    fn a_submenu_with_the_key_is_opened() {
        use MenuCommand as C;
        let entries = [keyed("Open", 'o', C::Open), submenu("New", 'w', true)];
        assert_eq!(access_hit(&entries, None, 'w'), AccessHit::Open(1));
    }

    #[test]
    fn rows_sharing_a_key_take_turns_and_wrap() {
        use MenuCommand as C;
        let entries = [
            keyed("Copy", 'c', C::Copy),
            keyed("Open", 'o', C::Open),
            keyed("Copy path", 'c', C::CopyPath),
            keyed("Cut", 't', C::Cut),
        ];
        assert_eq!(access_hit(&entries, None, 'c'), AccessHit::Move(0));
        assert_eq!(access_hit(&entries, Some(0), 'c'), AccessHit::Move(2));
        assert_eq!(access_hit(&entries, Some(2), 'c'), AccessHit::Move(0));
        // From a row that doesn't have the key: the next one after it.
        assert_eq!(access_hit(&entries, Some(1), 'c'), AccessHit::Move(2));
    }

    #[test]
    fn a_key_nothing_has_does_nothing() {
        use MenuCommand as C;
        let entries = [
            keyed("Open", 'o', C::Open),
            keyed("Cut", 't', C::Cut),
            MenuEntry::Separator,
        ];
        assert_eq!(access_hit(&entries, Some(1), 'z'), AccessHit::Nothing);
        assert_eq!(access_hit(&[], None, 'o'), AccessHit::Nothing);
    }

    #[test]
    fn disabled_rows_are_skipped() {
        use MenuCommand as C;
        let entries = [
            off("Copy", 'c', C::Copy),
            keyed("Copy path", 'c', C::CopyPath),
        ];
        assert_eq!(access_hit(&entries, None, 'c'), AccessHit::Run(C::CopyPath));
        let entries = [off("Copy", 'c', C::Copy), off("Cut", 't', C::Cut)];
        assert_eq!(access_hit(&entries, None, 'c'), AccessHit::Nothing);
        assert_eq!(
            access_hit(&[submenu("View", 'v', false)], None, 'v'),
            AccessHit::Nothing
        );
    }

    #[test]
    fn the_icon_row_takes_part_with_its_own_letters() {
        use MenuCommand as C;
        let buttons = MenuEntry::Actions(vec![
            button("Cut", 't', C::Cut, true),
            button("Delete", 'd', C::Delete, false),
            button("Rename", 'n', C::Rename, true),
        ]);
        let entries = [keyed("Open", 'o', C::Open), buttons.clone()];
        assert_eq!(access_hit(&entries, None, 't'), AccessHit::Run(C::Cut));
        assert_eq!(access_hit(&entries, None, 'n'), AccessHit::Run(C::Rename));
        // Disabled, like the rows.
        assert_eq!(access_hit(&entries, None, 'd'), AccessHit::Nothing);
        // A row and a button with the same key: the row takes the highlight, nothing runs.
        let entries = [keyed("Cut all", 't', C::Open), buttons];
        assert_eq!(access_hit(&entries, None, 't'), AccessHit::Move(0));
    }

    #[test]
    fn the_underlines_follow_how_the_menu_opened() {
        // Opened with the mouse: hidden, until a key is pressed in it.
        let mut cues = Cues::opened(false, false);
        assert!(!cues.shown());
        cues.pressed();
        assert!(cues.shown());
        // Opened from the keyboard, or with Windows set to always underline them: shown.
        assert!(Cues::opened(true, false).shown());
        assert!(Cues::opened(false, true).shown());
        assert!(!Cues::default().shown());
    }

    #[test]
    fn icon_buttons_never_write_their_key_after_the_text() {
        let off = Cues::opened(false, false);
        let on = Cues::opened(true, false);
        // The CJK form: no "(N)" in the text; N shows beside the icon, with the underlines only.
        let cjk = Some(derive_access('n', "新增資料夾", None, true));
        assert_eq!(button_text_access(cjk), None);
        assert_eq!(
            (button_key(cjk, off), button_key(cjk, on)),
            (None, Some('N'))
        );
        // A letter of the label: underlined in it, as in the rows, and nothing beside the icon.
        let latin = Some(derive_access('n', "New folder", None, false));
        assert_eq!(button_text_access(latin), latin);
        assert_eq!(button_key(latin, on), None);
        // A Latin label without the letter shows it beside the icon too.
        let missing = Some(derive_access('x', "Delete", None, false));
        assert_eq!(button_text_access(missing), None);
        assert_eq!(button_key(missing, on), Some('X'));
        assert_eq!(
            (button_text_access(None), button_key(None, on)),
            (None, None)
        );
    }
}
