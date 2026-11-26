//! The commands of Arke: one list that the command palette (Ctrl+Shift+P) searches and the
//! Keyboard shortcuts settings edit.
//!
//! Each command is an action that already exists, plus its name (a locale key), where its keys are
//! bound and what they are by default. The defaults are still bound where the actions are declared
//! (`explorer.rs`, `workspace.rs`, ...); a test checks that this list agrees with them.
//!
//! Changed shortcuts are stored in `Settings::shortcuts` (only the changes). Whenever they change
//! the key map is built again: the defaults of a changed command are dropped and its new keys are
//! bound in the same places. Keys of text boxes are bound deeper than any command, so they keep
//! working whatever is assigned.

use std::collections::BTreeMap;

use gpui::{
    Action, App, Entity, Global, KeyBinding, KeyBindingContextPredicate, Keystroke, OperatingSystem,
};
use rust_i18n::t;

use crate::settings::SettingsStore;

/// Where each kind of command is bound.
const WORKSPACE: &[&str] = &["Workspace"];
const EXPLORER: &[&str] = &["Explorer"];
/// Also bound on the pages that cover a tab (`workspace.rs`).
const EXPLORER_PAGES: &[&str] = &[
    "Explorer",
    "HomePage",
    "SettingsPage",
    "RecycleBin",
    "WhatsNewPage",
];
const LIST: &[&str] = &["FileList"];
/// Where Tab walks the window's stops (`workspace_sidebar_keys.rs`).
const TAB_STOPS: &[&str] = &[
    "FileList",
    "Sidebar",
    "HomePage",
    "Toolbar",
    "PathBar",
    "FilterBar",
];

/// Command palette rows for recently run commands.
const RECENT_MAX: usize = 8;

/// The system the keys are spelled for (Arke runs on Windows only).
const OS: OperatingSystem = OperatingSystem::Windows;

pub(crate) struct Command {
    /// Stable name, stored in settings.
    pub(crate) id: &'static str,
    /// Locale key of the name.
    name: &'static str,
    /// Key contexts the keys are bound in.
    contexts: &'static [&'static str],
    /// Default keys, the first one is shown.
    keys: &'static [&'static str],
    /// Keys bound only in the list, left as they are when `keys` change.
    #[cfg_attr(not(test), allow(dead_code, reason = "only the list test reads it"))]
    also: &'static [&'static str],
    /// Changes files (paste, delete, new folder, undo...): never run through the integration API
    /// (`api.rs`), which only lists and runs the others. Mark new commands that change files too.
    pub(crate) changes_files: bool,
    make: fn() -> Box<dyn Action>,
}

impl Command {
    pub(crate) fn action(&self) -> Box<dyn Action> {
        (self.make)()
    }

    /// The name in the current language.
    pub(crate) fn label(&self) -> String {
        t!(self.name).into_owned()
    }

    #[cfg(test)]
    pub(crate) const fn default_keys(&self) -> &'static [&'static str] {
        self.keys
    }

    #[cfg(test)]
    pub(crate) const fn also_keys(&self) -> &'static [&'static str] {
        self.also
    }
}

macro_rules! command {
    (@make $files:expr, $id:literal, $contexts:expr, [$($key:literal),*], also [$($also:literal),*], $action:expr) => {
        Command {
            id: $id,
            // Braces: the locale key check reads `t!(` at the end of `concat!(` as a lookup.
            name: concat! { "cmd.", $id },
            contexts: $contexts,
            keys: &[$($key),*],
            also: &[$($also),*],
            changes_files: $files,
            make: || Box::new($action),
        }
    };
    // A command that changes files (`Command::changes_files`).
    (files $id:literal, $contexts:expr, [$($key:literal),*], $action:expr) => {
        command!(@make true, $id, $contexts, [$($key),*], also [], $action)
    };
    ($id:literal, $contexts:expr, [$($key:literal),*], also [$($also:literal),*], $action:expr) => {
        command!(@make false, $id, $contexts, [$($key),*], also [$($also),*], $action)
    };
    ($id:literal, $contexts:expr, [$($key:literal),*], $action:expr) => {
        command!(@make false, $id, $contexts, [$($key),*], also [], $action)
    };
}

pub(crate) static COMMANDS: &[Command] = &[
    command!(
        "command_palette",
        WORKSPACE,
        ["ctrl-shift-p"],
        crate::workspace::OpenCommandPalette
    ),
    command!(
        "go_to_folder",
        WORKSPACE,
        ["ctrl-t"],
        crate::workspace::OpenPalette
    ),
    command!("new_tab", WORKSPACE, [], crate::workspace::NewTab),
    command!(
        "new_window",
        WORKSPACE,
        ["ctrl-n"],
        crate::workspace::NewWindow
    ),
    command!(
        "close_tab",
        WORKSPACE,
        ["ctrl-w", "ctrl-f4"],
        crate::workspace::CloseTab
    ),
    command!(
        "reopen_closed_tab",
        WORKSPACE,
        ["ctrl-shift-t"],
        crate::workspace::ReopenClosedTab
    ),
    command!(
        "next_tab",
        WORKSPACE,
        ["ctrl-tab", "ctrl-pagedown"],
        crate::workspace::NextTab
    ),
    command!(
        "prev_tab",
        WORKSPACE,
        ["ctrl-shift-tab", "ctrl-pageup"],
        crate::workspace::PrevTab
    ),
    command!(
        "toggle_sync_browsing",
        WORKSPACE,
        ["ctrl-shift-s"],
        crate::workspace::ToggleSyncBrowsing
    ),
    command!(
        "compare_folders",
        WORKSPACE,
        ["ctrl-shift-d"],
        crate::workspace::CompareFolders
    ),
    command!(
        "toggle_sidebar",
        WORKSPACE,
        ["ctrl-b"],
        crate::workspace::ToggleSidebar
    ),
    command!(
        "focus_next_region",
        WORKSPACE,
        ["f6"],
        crate::workspace::sidebar_keys::FocusNextRegion
    ),
    command!(
        "focus_prev_region",
        WORKSPACE,
        ["shift-f6"],
        crate::workspace::sidebar_keys::FocusPrevRegion
    ),
    command!(
        "open_workspaces",
        WORKSPACE,
        [],
        crate::workspace::OpenWorkspaces
    ),
    command!(
        "open_settings",
        WORKSPACE,
        ["ctrl-,"],
        crate::workspace::OpenSettings
    ),
    command!(
        "open_setup_guide",
        WORKSPACE,
        [],
        crate::workspace::OpenSetupGuide
    ),
    command!(
        "go_up",
        EXPLORER_PAGES,
        ["alt-up"],
        also["backspace"],
        crate::explorer::GoUp
    ),
    command!(
        "go_back",
        EXPLORER_PAGES,
        ["alt-left"],
        crate::explorer::GoBack
    ),
    command!(
        "go_forward",
        EXPLORER_PAGES,
        ["alt-right"],
        crate::explorer::GoForward
    ),
    command!(
        "focus_path_bar",
        EXPLORER_PAGES,
        ["ctrl-l", "alt-d"],
        crate::explorer::FocusPathBar
    ),
    command!(
        "focus_filter",
        EXPLORER_PAGES,
        ["ctrl-f"],
        crate::explorer::FocusFilter
    ),
    command!(
        "focus_search",
        EXPLORER_PAGES,
        ["ctrl-shift-f"],
        crate::explorer::FocusSearch
    ),
    command!(
        "focus_computer_search",
        EXPLORER_PAGES,
        ["ctrl-k"],
        crate::explorer::FocusComputerSearch
    ),
    command!(
        "refresh",
        EXPLORER,
        ["f5", "ctrl-r"],
        crate::explorer::Refresh
    ),
    command!(
        "toggle_search_mode",
        EXPLORER,
        ["alt-s"],
        crate::explorer::ToggleSearchMode
    ),
    command!(
        "toggle_hidden",
        EXPLORER,
        ["ctrl-h"],
        crate::explorer::ToggleHidden
    ),
    command!(
        "copy_as_path",
        EXPLORER,
        ["ctrl-shift-c"],
        crate::explorer::CopyAsPath
    ),
    command!(
        "add_to_shelf",
        EXPLORER,
        ["ctrl-shift-a"],
        crate::explorer::AddToShelf
    ),
    command!(
        "copy",
        EXPLORER,
        ["ctrl-c"],
        crate::explorer::ops::CopyFiles
    ),
    command!("cut", EXPLORER, ["ctrl-x"], crate::explorer::ops::CutFiles),
    command!(
        files "paste",
        EXPLORER,
        ["ctrl-v"],
        crate::explorer::ops::PasteFiles
    ),
    command!(
        files "delete",
        EXPLORER,
        ["delete"],
        crate::explorer::ops::DeleteFiles
    ),
    command!(
        files "delete_permanently",
        EXPLORER,
        ["shift-delete"],
        crate::explorer::ops::DeletePermanently
    ),
    command!("rename", EXPLORER, ["f2"], crate::explorer::ops::RenameFile),
    command!(
        files "new_folder",
        EXPLORER,
        ["ctrl-shift-n"],
        crate::explorer::ops::NewFolder
    ),
    command!(
        files "undo",
        EXPLORER,
        ["ctrl-z"],
        crate::explorer::ops::UndoFileOp
    ),
    command!(
        files "redo",
        EXPLORER,
        ["ctrl-y", "ctrl-shift-z"],
        crate::explorer::ops::RedoFileOp
    ),
    command!(
        "view_details",
        EXPLORER,
        ["ctrl-shift-6"],
        crate::explorer::ViewDetails
    ),
    command!(
        "view_icons_extra_large",
        EXPLORER,
        ["ctrl-shift-1"],
        crate::explorer::ViewIconsExtraLarge
    ),
    command!(
        "view_icons",
        EXPLORER,
        ["ctrl-shift-2"],
        crate::explorer::ViewIcons
    ),
    command!(
        "view_icons_medium",
        EXPLORER,
        ["ctrl-shift-3"],
        crate::explorer::ViewIconsMedium
    ),
    command!(
        "view_icons_small",
        EXPLORER,
        ["ctrl-shift-4"],
        crate::explorer::ViewIconsSmall
    ),
    command!("view_columns", EXPLORER, [], crate::explorer::ViewColumns),
    command!("toggle_tree", EXPLORER, [], crate::explorer::ToggleTree),
    command!("select_all", LIST, ["ctrl-a"], crate::explorer::SelectAll),
    command!(
        "open_in_new_tab",
        LIST,
        ["ctrl-enter"],
        crate::explorer::OpenInNewTab
    ),
    command!(
        "open_location",
        LIST,
        ["shift-enter"],
        crate::explorer::OpenLocation
    ),
    command!(
        "focus_other_pane",
        LIST,
        [],
        crate::explorer::FocusOtherPane
    ),
    command!(
        "focus_next_stop",
        TAB_STOPS,
        ["tab"],
        crate::workspace::sidebar_keys::FocusNextStop
    ),
    command!(
        "focus_prev_stop",
        TAB_STOPS,
        ["shift-tab"],
        crate::workspace::sidebar_keys::FocusPrevStop
    ),
    command!(
        files "copy_to_peer",
        LIST,
        ["alt-c"],
        crate::explorer::CopyToPeer
    ),
    command!(
        files "move_to_peer",
        LIST,
        ["alt-m"],
        crate::explorer::MoveToPeer
    ),
];

pub(crate) fn find(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// Commands that move keyboard focus into Arke's window (a box, the palette, a page). When one is
/// run through the integration API the window comes to the front, so typing goes where the
/// caller expects; the others leave the foreground alone.
const MOVES_FOCUS: &[&str] = &[
    "command_palette",
    "go_to_folder",
    "open_workspaces",
    "open_settings",
    "focus_next_region",
    "focus_prev_region",
    "focus_path_bar",
    "focus_filter",
    "focus_search",
    "focus_computer_search",
    "focus_other_pane",
];

/// Whether running command `id` moves keyboard focus into Arke's window.
pub(crate) fn moves_focus(id: &str) -> bool {
    MOVES_FOCUS.contains(&id)
}

/// The commands the integration API may run (`api.rs`): all but the ones that change files.
pub(crate) fn runnable() -> impl Iterator<Item = &'static Command> {
    COMMANDS.iter().filter(|c| !c.changes_files)
}

/// Stored shortcut changes: command id to keys.
pub(crate) type Overrides = BTreeMap<String, Vec<String>>;

/// The keys of `command`, as changed in `overrides` or else its defaults.
pub(crate) fn keys_of(command: &Command, overrides: &Overrides) -> Vec<String> {
    match overrides.get(command.id) {
        Some(keys) => keys.clone(),
        None => command.keys.iter().map(|k| (*k).to_owned()).collect(),
    }
}

/// The key shown for `command`: its first one.
pub(crate) fn shown_key(command: &Command, overrides: &Overrides) -> Option<String> {
    keys_of(command, overrides).first().map(|k| display(k))
}

/// `ctrl-shift-p` as `Ctrl+Shift+P`.
pub(crate) fn display(chord: &str) -> String {
    let Ok(key) = Keystroke::parse_with_os(chord, OS) else {
        return chord.to_owned();
    };
    let mut out = String::new();
    let m = key.modifiers;
    for (on, name) in [
        (m.control, "Ctrl+"),
        (m.alt, "Alt+"),
        (m.platform, "Win+"),
        (m.shift, "Shift+"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    let mut chars = key.key.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}

/// A key written the one way every key is stored in (modifier order, spelling), or `None` when it
/// is not a single keystroke.
pub(crate) fn canonical(chord: &str) -> Option<String> {
    let mut parts = chord.split_whitespace();
    let (Some(one), None) = (parts.next(), parts.next()) else {
        return None;
    };
    Keystroke::parse_with_os(one, OS)
        .ok()
        .map(|k| k.unparse(OS))
}

/// A pressed keystroke in the form shortcuts are stored in.
pub(crate) fn chord_of(key: &Keystroke) -> Option<String> {
    canonical(&key.unparse(OS))
}

/// Whether a recorded keystroke can be a shortcut: not a bare key that a text box would take as
/// typing. Control, Alt or Win with a key, a function key, or Delete.
pub(crate) fn assignable(key: &Keystroke) -> bool {
    if key.is_modifier_key() {
        return false;
    }
    let m = key.modifiers;
    let function_key = key
        .key
        .strip_prefix('f')
        .is_some_and(|n| n.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)));
    m.control || m.alt || m.platform || function_key || key.key == "delete"
}

/// The command whose keys include `chord`, other than `except`.
pub(crate) fn holder(chord: &str, except: &str, overrides: &Overrides) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| {
        c.id != except
            && keys_of(c, overrides)
                .iter()
                .any(|k| canonical(k).as_deref() == Some(chord))
    })
}

/// Gives `chord` to `id` and nothing else; with `swap` the command that had it takes the key
/// `id` gave up. Stores only what differs from the defaults.
pub(crate) fn assign(overrides: &mut Overrides, id: &str, chord: &str, swap: bool) {
    let Some(command) = find(id) else {
        return;
    };
    let before = keys_of(command, overrides);
    if swap && let Some(other) = holder(chord, id, overrides) {
        let mut keys: Vec<String> = keys_of(other, overrides)
            .into_iter()
            .filter(|k| canonical(k).as_deref() != Some(chord))
            .collect();
        keys.extend(before.first().cloned());
        set(overrides, other, keys);
    }
    set(overrides, command, vec![chord.to_owned()]);
}

/// Keeps `keys` for `command` unless they are its defaults.
fn set(overrides: &mut Overrides, command: &Command, keys: Vec<String>) {
    if keys
        .iter()
        .map(String::as_str)
        .eq(command.keys.iter().copied())
    {
        overrides.remove(command.id);
    } else {
        overrides.insert(command.id.to_owned(), keys);
    }
}

/// Remembers that `id` was just run.
pub(crate) fn remember(recent: &mut Vec<String>, id: &str) {
    recent.retain(|r| r != id);
    recent.insert(0, id.to_owned());
    recent.truncate(RECENT_MAX);
}

/// Commands for the palette: the ones run lately first, then the rest in list order.
pub(crate) fn palette_order(recent: &[String]) -> Vec<&'static Command> {
    let mut out: Vec<&Command> = recent.iter().filter_map(|id| find(id)).collect();
    for command in COMMANDS {
        if !out.iter().any(|c| c.id == command.id) {
            out.push(command);
        }
    }
    out
}

// --- Key map ---

/// Every default binding.
pub(crate) fn bind_defaults(cx: &mut App) {
    crate::text_input::bind_keys(cx);
    crate::explorer::bind_keys(cx);
    crate::workspace::bind_keys(cx);
    crate::palette::bind_keys(cx);
}

/// Whether `binding` is one of the default keys of `command`, which new keys replace.
fn replaces(command: &Command, binding: &KeyBinding) -> bool {
    let [key] = binding.keystrokes() else {
        return false;
    };
    binding.action().partial_eq(&*command.action())
        && command
            .keys
            .iter()
            .any(|k| canonical(k).is_some_and(|k| k == key.unparse(OS)))
}

/// The key map with `overrides` applied: bindings of changed commands are replaced.
fn rebuild(overrides: &Overrides, cx: &mut App) {
    cx.clear_key_bindings();
    bind_defaults(cx);
    if overrides.is_empty() {
        return;
    }
    let changed: Vec<&Command> = COMMANDS
        .iter()
        .filter(|c| overrides.contains_key(c.id))
        .collect();
    let map = cx.key_bindings();
    let kept: Vec<KeyBinding> = map
        .borrow()
        .bindings()
        .filter(|b| !changed.iter().any(|c| replaces(c, b)))
        .cloned()
        .collect();
    let mut bindings = Vec::new();
    for command in &changed {
        for context in command.contexts {
            let Ok(predicate) = KeyBindingContextPredicate::parse(context) else {
                continue;
            };
            let predicate = std::rc::Rc::new(predicate);
            for key in keys_of(command, overrides) {
                // `load` maps keys like Shift+2 the way the keyboard produces them.
                if let Ok(binding) = KeyBinding::load(
                    &key,
                    command.action(),
                    Some(std::rc::Rc::clone(&predicate)),
                    false,
                    None,
                    cx,
                ) {
                    bindings.push(binding);
                }
            }
        }
    }
    {
        let mut map = map.borrow_mut();
        map.clear();
        map.add_bindings(kept);
    }
    cx.bind_keys(bindings);
}

/// The shortcuts last applied.
struct Applied(Overrides);

impl Global for Applied {}

/// Binds the keys and keeps them in step with the settings: a change of shortcuts rebinds at once.
pub(crate) fn init(store: &Entity<SettingsStore>, cx: &mut App) {
    apply(store, cx);
    cx.observe(store, |store, cx| apply(&store, cx)).detach();
}

fn apply(store: &Entity<SettingsStore>, cx: &mut App) {
    let overrides = store.read(cx).settings.shortcuts.clone();
    if cx.try_global::<Applied>().is_some_and(|a| a.0 == overrides) {
        return;
    }
    rebuild(&overrides, cx);
    cx.set_global(Applied(overrides));
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    #[test]
    fn focus_moving_commands_are_real_and_runnable() {
        for id in MOVES_FOCUS {
            let command = find(id).unwrap_or_else(|| panic!("no command {id}"));
            assert!(!command.changes_files, "{id} changes files");
        }
        assert!(moves_focus("focus_search"));
        assert!(!moves_focus("refresh"));
        assert!(!moves_focus("copy_as_path"));
        assert!(!moves_focus("toggle_hidden"));
    }
}
