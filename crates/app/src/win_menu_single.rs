//! A pinned Windows command read on its own when it isn't an extension (#278): a command in the
//! registry (`<class>\shell\<verb>`) or a packaged app's command (an `IExplorerCommand`, from
//! `desktop5:ItemType` in its manifest: Windows Terminal, VS Code, Zed, Notepad...).
//!
//! Each becomes a one-item menu in this thread's session and runs like any other item:
//! - a registry command through `ShellExecuteEx` with its class key, so Windows runs that verb
//!   alone (its command line, or its `DelegateExecute` object) without loading any extension;
//!   %V and %W come out as the folder, as from File Explorer's menu;
//! - a packaged command (or a registry one with an `ExplorerCommandHandler`) through
//!   `IExplorerCommand`: created by its class id alone, it gives its title, icon and state, and
//!   `Invoke` runs it. One with commands of its own (NanaZip's "Extract here", ...) becomes its
//!   submenu, each command in it run the same way, so a pinned one shows as quickly (#283).
//!
//! Measured: a registry command 9 to 20 ms, a packaged one 2 to 140 ms, against 0.25 to 1.8 s
//! for the whole Windows menu, which waits for its slowest extension.
//!
//! "Send to" is listed here too, from its folder, instead of through Windows' own Send To
//! extension (#278). That extension fills its submenu in the background a few items at a time,
//! with no sign when it is done, taking 0.5 to 1.6 s: read too early it gave only its first
//! items, and pinned ones went missing. Its folder holds every target (shortcuts, and Windows'
//! own "Compressed folder", "Desktop", "Documents" files), listed in a few ms; a target runs the
//! way File Explorer runs it, the selection dropped on that item's drop target. Removable drives
//! follow, as in File Explorer.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{E_NOTIMPL, HWND};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CLSIDFromString, CoCreateInstance, CoTaskMemFree,
};
use windows::Win32::System::Ole::IObjectWithSite;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMINVOKECOMMANDINFO, ECF_HASSUBCOMMANDS, ECF_ISSEPARATOR, ECS_CHECKED, ECS_DISABLED,
    ECS_HIDDEN, IContextMenu, IContextMenu_Impl, IExplorerCommand, IShellItemArray,
    SEE_MASK_CLASSKEY, SEE_MASK_NOASYNC, SHCreateShellItemArrayFromIDLists, SHDefExtractIconW,
    SHELLEXECUTEINFOW, SHLoadIndirectString, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, GetSystemMetrics, HICON, HMENU, ICONINFO, SM_CXSMICON, SW_SHOWNORMAL,
};
use windows::core::{HRESULT, Interface, PCWSTR, PSTR, PWSTR};

use super::{Pidls, clean_label, fill_single, menu_directory, parse_pidls};
use crate::shell::{ShellError, ShellMenu, ShellMenuEntry};
use crate::win_fileops::wide;
use crate::win_menu_parts::{HandlerKind, MenuHandler};
use windows::Win32::Foundation::{E_FAIL, POINTL};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, GetDriveTypeW, GetLogicalDrives,
};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropTarget,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::WindowsProgramming::{DRIVE_REMOTE, DRIVE_REMOVABLE};
use windows::Win32::UI::Shell::Common::STRRET;
use windows::Win32::UI::Shell::{
    BHID_DataObject, CMF_NORMAL, FOLDERID_SendTo, IEnumIDList, ILFindLastID, ILFree, IShellFolder,
    KF_FLAG_DEFAULT, SHBindToParent, SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, SHFILEINFOW,
    SHGDN_FORPARSING, SHGDN_NORMAL, SHGDNF, SHGFI_DISPLAYNAME, SHGFI_ICON, SHGFI_SMALLICON,
    SHGetDesktopFolder, SHGetFileInfoW, SHGetKnownFolderPath, SHParseDisplayName, StrCmpLogicalW,
    StrRetToStrW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuStringW, MF_BYPOSITION,
};

/// Reads `handler` (a registry or packaged command) for `paths` as a menu of its own: one item,
/// or none when it doesn't show for them. Runs on the part's own thread.
pub(super) fn build(
    paths: &[PathBuf],
    background: bool,
    handler: &MenuHandler,
    window: HWND,
) -> Result<ShellMenu, ShellError> {
    let Some(first) = paths.first() else {
        return Ok(ShellMenu::default());
    };
    let directory = menu_directory(paths, background);
    // A registry verb runs through ShellExecuteEx and needs no PIDLs, which on a dead mapped
    // drive can take long to parse.
    let (pidls, (menu, entries)) = match handler.kind {
        HandlerKind::Verb { .. } if handler.clsid.is_empty() => {
            (Pidls(Vec::new()), verb(first, background, handler))
        }
        HandlerKind::Verb { .. } | HandlerKind::Command => {
            let pidls = parse_pidls(paths)?;
            let built = command(&pidls, first, background, handler)?;
            (pidls, built)
        }
        HandlerKind::Extension => return Err(ShellError::Unsupported),
    };
    fill_single(menu, window, pidls, directory, entries)
}

/// A registry command: its label (`MUIVerb`, else the key's value, else the verb) and `Icon`.
fn verb(
    target: &Path,
    background: bool,
    handler: &MenuHandler,
) -> (IContextMenu, Vec<ShellMenuEntry>) {
    let at = format!(r"{}\shell\{}", handler.key, handler.name);
    let read = |name: &str| {
        crate::win::read_reg_string(HKEY_CLASSES_ROOT, &at, name).filter(|s| !s.trim().is_empty())
    };
    let label = verb_label(&at).unwrap_or_else(|| handler.name.clone());
    let icon = read("Icon").and_then(|spec| icon_of(&spec));
    let folder = if background {
        target.to_path_buf()
    } else {
        target
            .parent()
            .map_or_else(|| target.to_path_buf(), Path::to_path_buf)
    };
    let menu: IContextMenu = Single(
        vec![Action::Verb {
            class: handler.key.clone(),
            verb: handler.name.clone(),
            file: target.to_path_buf(),
            folder,
        }],
        None,
    )
    .into();
    let entry = item(1, &handler.name, &label, true, false, icon);
    // In a cascading menu: under that menu's label, as in the whole Windows menu, so the pinned
    // command's key (`Tools › Command`) is found (#278).
    let entry = match handler.parent.as_deref() {
        Some(parent) => ShellMenuEntry::Submenu {
            label: clean_label(
                &verb_label(parent)
                    .unwrap_or_else(|| parent.rsplit('\\').next().unwrap_or(parent).to_string()),
            ),
            enabled: true,
            icon: None,
            children: vec![entry],
            lazy: None,
        },
        None => entry,
    };
    (menu, vec![entry])
}

/// A registry verb's label (at registry path `at`): its `MUIVerb`, else the key's value.
fn verb_label(at: &str) -> Option<String> {
    let read = |name: &str| {
        crate::win::read_reg_string(HKEY_CLASSES_ROOT, at, name).filter(|s| !s.trim().is_empty())
    };
    read("MUIVerb").or_else(|| read("")).map(|s| indirect(&s))
}

/// An `IExplorerCommand` (a packaged app's command, or a registry command's
/// `ExplorerCommandHandler`), created by its class id alone.
fn command(
    pidls: &Pidls,
    target: &Path,
    background: bool,
    handler: &MenuHandler,
) -> Result<(IContextMenu, Vec<ShellMenuEntry>), ShellError> {
    let text = wide(OsStr::new(&handler.clsid));
    // SAFETY: NUL-terminated.
    let clsid = unsafe { CLSIDFromString(PCWSTR(text.as_ptr())) }?;
    // SAFETY: plain COM creation on this STA thread. Packaged classes are found by COM itself.
    let command: IExplorerCommand = unsafe {
        CoCreateInstance(
            &raw const clsid,
            None,
            CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER,
        )
    }?;
    let ids: Vec<*const ITEMIDLIST> = pidls.0.iter().map(|p| p.cast_const()).collect();
    // SAFETY: the PIDLs are valid absolute ones, owned by `pidls` for the call.
    let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromIDLists(&ids) }?;
    // The command holding the view site; the menu clears it when it is dropped, so the two
    // never keep each other alive.
    let mut sited: Option<IObjectWithSite> = None;
    if background {
        // Commands for a folder's empty space may look at the view (Windows Terminal does when
        // it gets no item), as from File Explorer's menu.
        if let (Some(site), Ok(with_site)) = (
            crate::win_viewsite::background_site(target),
            command.cast::<IObjectWithSite>(),
        ) {
            // SAFETY: plain COM call on this STA thread.
            if unsafe { with_site.SetSite(&site) }.is_ok() {
                sited = Some(with_site);
            }
        }
    }
    // SAFETY: plain COM calls on this STA thread.
    let (flags, state) = unsafe {
        (
            command.GetFlags().unwrap_or(0),
            command.GetState(&items, true).unwrap_or(0),
        )
    };
    let verb = match handler.kind {
        HandlerKind::Command => &handler.clsid,
        _ => &handler.name,
    };
    let mut actions = Vec::new();
    let entries = shown(&command, &items, verb, &mut actions, flags, state)
        .into_iter()
        .collect();
    let menu: IContextMenu = Single(actions, sited).into();
    Ok((menu, entries))
}

/// `command`'s row, its action added to `actions` (the row's id is its place there plus one);
/// `None` when it is hidden for these items or has no title. One with commands of its own is
/// their submenu (one level: deeper ones are left to the whole menu).
fn shown(
    command: &IExplorerCommand,
    items: &IShellItemArray,
    verb: &str,
    actions: &mut Vec<Action>,
    flags: u32,
    state: u32,
) -> Option<ShellMenuEntry> {
    let has = |bits: u32, flag: i32| bits & flag.cast_unsigned() != 0;
    if has(state, ECS_HIDDEN.0) {
        return None;
    }
    // SAFETY: plain COM calls; the strings are ours to free.
    let title = unsafe { command.GetTitle(items) }.map_or_default(take);
    if title.trim().is_empty() {
        return None;
    }
    // SAFETY: as above.
    let icon = unsafe { command.GetIcon(items) }
        .map(take)
        .ok()
        .and_then(|spec| icon_of(&spec));
    let enabled = !has(state, ECS_DISABLED.0);
    if has(flags, ECF_HASSUBCOMMANDS.0) {
        // Only the top command opens a submenu here.
        if !actions.is_empty() {
            return None;
        }
        let children = sub_commands(command, items, actions);
        return (!children.is_empty()).then(|| ShellMenuEntry::Submenu {
            label: clean_label(&title),
            enabled,
            icon,
            children,
            lazy: None,
        });
    }
    actions.push(Action::Command {
        command: command.clone(),
        items: items.clone(),
    });
    let id = u32::try_from(actions.len()).unwrap_or(u32::MAX);
    Some(item(
        id,
        verb,
        &title,
        enabled,
        has(state, ECS_CHECKED.0),
        icon,
    ))
}

/// Most rows read from one command's own commands.
const MAX_SUB_COMMANDS: usize = 200;

/// The rows of a command's own commands (`EnumSubCommands`), separators included.
fn sub_commands(
    command: &IExplorerCommand,
    items: &IShellItemArray,
    actions: &mut Vec<Action>,
) -> Vec<ShellMenuEntry> {
    // Holds the submenu's place, so the rows below are never taken for the top command.
    actions.push(Action::None);
    let mut rows = Vec::new();
    // SAFETY: plain COM call on this STA thread.
    let Ok(list) = (unsafe { command.EnumSubCommands() }) else {
        return rows;
    };
    // A command listing more than this is misbehaving; the whole menu still shows it.
    for _ in 0..MAX_SUB_COMMANDS {
        let mut next = [None];
        let mut fetched = 0u32;
        // SAFETY: one slot to fill and a count, both ours.
        let got = unsafe { list.Next(&mut next, Some(&raw mut fetched)) };
        if got.is_err() || fetched == 0 {
            break;
        }
        let Some(sub) = next[0].take() else { break };
        // SAFETY: plain COM calls on this STA thread.
        let (flags, state) = unsafe {
            (
                sub.GetFlags().unwrap_or(0),
                sub.GetState(items, true).unwrap_or(0),
            )
        };
        if flags & ECF_ISSEPARATOR.0.cast_unsigned() != 0 {
            rows.push(ShellMenuEntry::Separator);
            continue;
        }
        // Deeper submenus are left to the whole menu: skip them before reading anything.
        if flags & ECF_HASSUBCOMMANDS.0.cast_unsigned() != 0 {
            continue;
        }
        // SAFETY: as above; the canonical name is a GUID, the row's verb.
        let verb = unsafe { sub.GetCanonicalName() }.map_or_default(|g| format!("{{{g:?}}}"));
        if let Some(row) = shown(&sub, items, &verb, actions, flags, state) {
            rows.push(row);
        }
    }
    crate::shell::tidy_separators(rows)
}

fn item(
    id: u32,
    verb: &str,
    label: &str,
    enabled: bool,
    checked: bool,
    icon: Option<arke_core::thumbs::Thumbnail>,
) -> ShellMenuEntry {
    ShellMenuEntry::Item {
        id,
        // A packaged command's own commands may have no canonical name.
        verb: (!verb.is_empty()).then(|| verb.to_string()),
        label: clean_label(label),
        enabled,
        checked,
        default: false,
        icon,
    }
}

/// A string from a COM call, freed.
fn take(text: PWSTR) -> String {
    // Some commands answer S_OK with no string (no icon, no title).
    if text.0.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated string allocated with CoTaskMemAlloc, freed once here.
    unsafe {
        let s = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0.cast_const().cast()));
        s
    }
}

/// `@file.dll,-123` resource strings resolved (read from the file as data); others as they are.
fn indirect(text: &str) -> String {
    if !text.starts_with('@') {
        return text.to_string();
    }
    let source = wide(OsStr::new(text));
    let mut out = vec![0u16; 512];
    // SAFETY: source is NUL-terminated; the output buffer is ours.
    match unsafe { SHLoadIndirectString(PCWSTR(source.as_ptr()), &mut out, None) } {
        Ok(()) => {
            let len = out.iter().position(|&c| c == 0).unwrap_or(out.len());
            String::from_utf16_lossy(&out[..len])
        }
        Err(_) => text.to_string(),
    }
}

/// `path,index` (a negative index is a resource id), `"path"`, `%Var%` expanded; the small icon.
fn icon_of(spec: &str) -> Option<arke_core::thumbs::Thumbnail> {
    let (path, index) = split_icon_spec(spec)?;
    let path = expand_env(&path);
    let text = wide(OsStr::new(&path));
    // SAFETY: plain metric query.
    let side = u32::try_from(unsafe { GetSystemMetrics(SM_CXSMICON) })
        .unwrap_or(16)
        .max(16);
    let mut icon = HICON::default();
    // SAFETY: text is NUL-terminated; icon is a valid out pointer, destroyed below.
    unsafe {
        SHDefExtractIconW(
            PCWSTR(text.as_ptr()),
            index,
            0,
            Some(&raw mut icon),
            None,
            side,
        )
    }
    .ok()
    .ok()?;
    icon_pixels_of(icon)
}

/// `icon`'s pixels; the icon is destroyed.
fn icon_pixels_of(icon: HICON) -> Option<arke_core::thumbs::Thumbnail> {
    // SAFETY: icon is ours; both bitmaps from GetIconInfo are freed, then the icon.
    unsafe {
        let mut parts = ICONINFO::default();
        let got = GetIconInfo(icon, &raw mut parts);
        let pixels = got
            .is_ok()
            .then(|| crate::win_icons::icon_pixels(parts.hbmColor, parts.hbmMask))
            .flatten();
        if got.is_ok() {
            let _ = DeleteObject(HGDIOBJ(parts.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(parts.hbmMask.0));
        }
        let _ = DestroyIcon(icon);
        pixels
    }
}

/// `C:\app.exe,-101` → (`C:\app.exe`, -101); `"C:\a b.ico"` → (`C:\a b.ico`, 0).
fn split_icon_spec(spec: &str) -> Option<(String, i32)> {
    let spec = spec.trim();
    let (path, index) = match spec.rsplit_once(',') {
        Some((path, index)) if index.trim().parse::<i32>().is_ok() => {
            (path, index.trim().parse().ok()?)
        }
        _ => (spec, 0),
    };
    let path = path.trim().trim_matches('"').trim();
    (!path.is_empty()).then(|| (path.to_string(), index))
}

/// `%SystemRoot%\x` with the variables filled in (unknown ones kept).
fn expand_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        match std::env::var(name) {
            Ok(value) if !name.is_empty() => out.push_str(&value),
            _ => {
                out.push('%');
                out.push_str(name);
                out.push('%');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

enum Action {
    Verb {
        /// Class key under `HKEY_CLASSES_ROOT` holding the verb.
        class: String,
        verb: String,
        /// The item, or the folder for its empty space.
        file: PathBuf,
        /// %W: the folder.
        folder: PathBuf,
    },
    Command {
        command: IExplorerCommand,
        items: IShellItemArray,
    },
    /// A "Send to" target: the items dropped on it.
    Drop {
        target: PathBuf,
        items: IShellItemArray,
    },
    /// No row: a submenu's own place, so ids and places stay in step.
    None,
}

/// Runs a registry verb from `class` alone: Windows reads that class's verb and runs it (its
/// command line or its `DelegateExecute` object), no menu extension involved.
fn run_verb(
    class: &str,
    verb: &str,
    file: &Path,
    folder: &Path,
    owner: HWND,
) -> windows::core::Result<()> {
    let class = wide(OsStr::new(class));
    let mut key = HKEY::default();
    // SAFETY: class is NUL-terminated; the key is closed below.
    unsafe {
        RegOpenKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(class.as_ptr()),
            None,
            KEY_READ,
            &raw mut key,
        )
    }
    .ok()?;
    let (verb, file, folder) = (
        wide(OsStr::new(verb)),
        wide(file.as_os_str()),
        wide(folder.as_os_str()),
    );
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or_default(),
        // NOASYNC: the menu thread may end right after.
        fMask: SEE_MASK_CLASSKEY | SEE_MASK_NOASYNC,
        hwnd: owner,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpDirectory: PCWSTR(folder.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        hkeyClass: key,
        ..Default::default()
    };
    // SAFETY: every string outlives the call; the key is open.
    let result = unsafe { ShellExecuteExW(&raw mut info) };
    // SAFETY: opened above.
    let _ = unsafe { RegCloseKey(key) };
    result
}

/// The "Send to" label once read: it only changes with the display language, which takes a new
/// session (so a new menu host).
static SEND_TO_LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The targets as last listed, with their icons: kept while the folder is unchanged (adding,
/// removing or renaming a target changes its time), so a menu doesn't read them again.
type Targets = Vec<(String, PathBuf, Option<arke_core::thumbs::Thumbnail>)>;
static SEND_TO_TARGETS: parking_lot::Mutex<Option<(PathBuf, std::time::SystemTime, Targets)>> =
    parking_lot::Mutex::new(None);

/// The "Send to" label read before, without creating Windows' Send To extension.
pub(super) fn cached_send_to_label() -> Option<String> {
    SEND_TO_LABEL.get().cloned()
}

/// The "Send to" submenu's label as Windows shows it: its extension, initialized for the items
/// (`menu`), adds the submenu but fills it only when it opens. Read once.
pub(super) fn send_to_label(menu: &IContextMenu) -> Option<String> {
    if let Some(label) = SEND_TO_LABEL.get() {
        return Some(label.clone());
    }
    let label = read_send_to_label(menu)?;
    Some(SEND_TO_LABEL.get_or_init(|| label).clone())
}

fn read_send_to_label(menu: &IContextMenu) -> Option<String> {
    // SAFETY: a popup menu of our own, destroyed below; the extension adds its submenu to it.
    unsafe {
        let hmenu = CreatePopupMenu().ok()?;
        let added = menu
            .QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL)
            .is_ok();
        let mut text = [0u16; 260];
        let len = if added {
            GetMenuStringW(hmenu, 0, Some(&mut text), MF_BYPOSITION)
        } else {
            0
        };
        let _ = DestroyMenu(hmenu);
        let len = usize::try_from(len).unwrap_or(0).min(text.len());
        let label = clean_label(&String::from_utf16_lossy(&text[..len]));
        (!label.is_empty()).then_some(label)
    }
}

/// "Send to" for `paths` as a menu of its own: the submenu `label` with every target.
pub(super) fn send_to(
    paths: &[PathBuf],
    label: String,
    window: HWND,
) -> Result<ShellMenu, ShellError> {
    let pidls = parse_pidls(paths)?;
    let ids: Vec<*const ITEMIDLIST> = pidls.0.iter().map(|p| p.cast_const()).collect();
    // SAFETY: the PIDLs are valid absolute ones, owned by `pidls` for the call.
    let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromIDLists(&ids) }?;
    let mut actions = Vec::new();
    let mut children = Vec::new();
    let drives = removable_drives().into_iter().map(|(name, root)| {
        let icon = file_icon(&root);
        (name, root, icon)
    });
    for (name, target, icon) in cached_targets().into_iter().chain(drives) {
        actions.push(Action::Drop {
            target,
            items: items.clone(),
        });
        let id = u32::try_from(actions.len()).unwrap_or(u32::MAX);
        children.push(item(id, "", &name, true, false, icon));
    }
    let entries = if children.is_empty() {
        Vec::new()
    } else {
        vec![ShellMenuEntry::Submenu {
            label,
            enabled: true,
            icon: None,
            children,
            lazy: None,
        }]
    };
    let menu: IContextMenu = Single(actions, None).into();
    fill_single(menu, window, pidls, menu_directory(paths, false), entries)
}

/// The targets in the SendTo folder with their icons, from the cache while the folder is
/// unchanged.
fn cached_targets() -> Targets {
    let Some(folder) = send_to_folder() else {
        return Vec::new();
    };
    let changed = std::fs::metadata(&folder).and_then(|m| m.modified()).ok();
    if let Some(changed) = changed
        && let Some((at, time, targets)) = SEND_TO_TARGETS.lock().as_ref()
        && *at == folder
        && *time == changed
    {
        return targets.clone();
    }
    let targets: Targets = send_to_targets(&folder)
        .into_iter()
        .map(|(name, path)| {
            let icon = file_icon(&path);
            (name, path, icon)
        })
        .collect();
    if let Some(changed) = changed {
        *SEND_TO_TARGETS.lock() = Some((folder, changed, targets.clone()));
    }
    targets
}

/// The user's SendTo folder.
fn send_to_folder() -> Option<PathBuf> {
    // SAFETY: the returned string is freed by `take`.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_SendTo, KF_FLAG_DEFAULT, None) }.ok()?;
    Some(PathBuf::from(take(path)))
}

/// The targets in `folder` as File Explorer names them (no ".lnk", the localized names of
/// Windows' own) and orders them; hidden files (desktop.ini) left out.
fn send_to_targets(folder: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(pidls) = parse_pidls(&[folder.to_path_buf()]) else {
        return out;
    };
    // SAFETY: plain COM calls on this STA thread; every child PIDL is freed after use.
    unsafe {
        let Ok(desktop) = SHGetDesktopFolder() else {
            return out;
        };
        let Ok(shell_folder) = desktop.BindToObject::<_, IShellFolder>(pidls.0[0], None) else {
            return out;
        };
        let mut list: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0).cast_unsigned();
        if shell_folder
            .EnumObjects(HWND::default(), flags, &raw mut list)
            .is_err()
        {
            return out;
        }
        let Some(list) = list else {
            return out;
        };
        loop {
            let mut child = [std::ptr::null_mut::<ITEMIDLIST>()];
            let mut got = 0u32;
            if list.Next(&mut child, Some(&raw mut got)).is_err() || got == 0 {
                break;
            }
            let name = display_name(&shell_folder, child[0], SHGDN_NORMAL);
            let path = display_name(&shell_folder, child[0], SHGDN_FORPARSING);
            CoTaskMemFree(Some(child[0].cast_const().cast()));
            if let (Some(name), Some(path)) = (name, path) {
                out.push((name, PathBuf::from(path)));
            }
        }
    }
    // In the display language's order, as File Explorer sorts them (by strokes on zh-TW).
    out.sort_by(|(a, _), (b, _)| {
        let (a, b) = (wide(OsStr::new(a)), wide(OsStr::new(b)));
        // SAFETY: both NUL-terminated.
        unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) }.cmp(&0)
    });
    out
}

/// A child's name in `folder` (`how`: as shown, or its full path).
///
/// # Safety
/// `child` is a valid child PIDL of `folder`.
unsafe fn display_name(
    folder: &IShellFolder,
    child: *mut ITEMIDLIST,
    how: SHGDNF,
) -> Option<String> {
    // SAFETY: per the contract; StrRetToStrW frees the STRRET's own string.
    unsafe {
        let mut name = STRRET::default();
        folder.GetDisplayNameOf(child, how, &raw mut name).ok()?;
        let mut text = PWSTR::null();
        StrRetToStrW(&raw mut name, Some(child.cast_const()), &raw mut text).ok()?;
        let text = take(text);
        (!text.is_empty()).then_some(text)
    }
}

/// Removable and mapped network drives, as File Explorer adds them to "Send to" (not when policy
/// turns that off).
fn removable_drives() -> Vec<(String, PathBuf)> {
    let policy = |root| {
        crate::win::read_reg_dword(
            root,
            r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer",
            "NoDrivesInSendToMenu",
        )
        .is_some_and(|v| v != 0)
    };
    if policy(HKEY_CURRENT_USER) || policy(HKEY_LOCAL_MACHINE) {
        return Vec::new();
    }
    // SAFETY: plain query.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let root = PathBuf::from(format!("{}:\\", char::from(b'A' + bit)));
            let text = wide(root.as_os_str());
            // SAFETY: NUL-terminated.
            let kind = unsafe { GetDriveTypeW(PCWSTR(text.as_ptr())) };
            if kind != DRIVE_REMOVABLE && kind != DRIVE_REMOTE {
                return None;
            }
            let mut info = SHFILEINFOW::default();
            // SAFETY: info is sized for the call; asks for the display name only.
            let ok = unsafe {
                SHGetFileInfoW(
                    PCWSTR(text.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    Some(&raw mut info),
                    size_of::<SHFILEINFOW>() as u32,
                    SHGFI_DISPLAYNAME,
                )
            } != 0;
            let len = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&info.szDisplayName[..len]);
            (ok && !name.is_empty()).then_some((name, root))
        })
        .collect()
}

/// A target's small icon, as the shell draws it.
fn file_icon(path: &Path) -> Option<arke_core::thumbs::Thumbnail> {
    let text = wide(path.as_os_str());
    let mut info = SHFILEINFOW::default();
    // SAFETY: info is sized for the call; the icon it returns is destroyed by icon_pixels_of.
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(text.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&raw mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        )
    } != 0;
    (ok && !info.hIcon.is_invalid())
        .then(|| icon_pixels_of(info.hIcon))
        .flatten()
}

/// Runs a "Send to" target: `items` dropped on `target`'s drop target, as File Explorer's Send
/// To does. A copy where the target offers one (a folder on the same drive would otherwise take a
/// move); a program's shortcut gets the files as arguments, the "Compressed folder" target zips
/// them (in the background: the menu host lingers after a command).
fn drop_on(target: &Path, items: &IShellItemArray, owner: HWND) -> windows::core::Result<()> {
    let name = wide(target.as_os_str());
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: plain COM calls on this STA thread; name is NUL-terminated; the PIDL is freed below
    // and `child` points into it.
    unsafe {
        let data: IDataObject = items.BindToHandler(None, &BHID_DataObject)?;
        SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut pidl, 0, None)?;
        let result = (|| {
            let folder: IShellFolder = SHBindToParent(pidl, None)?;
            let child = [ILFindLastID(pidl).cast_const()];
            let target: IDropTarget = folder.GetUIObjectOf(owner, &child, None)?;
            let keys = MODIFIERKEYS_FLAGS(MK_LBUTTON.0);
            let mut effect = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;
            target.DragEnter(&data, keys, POINTL::default(), &raw mut effect)?;
            if effect == DROPEFFECT_NONE {
                let _ = target.DragLeave();
                return Err(E_FAIL.into());
            }
            let mut effect = if effect.0 & DROPEFFECT_COPY.0 == 0 {
                effect
            } else {
                DROPEFFECT_COPY
            };
            target.Drop(&data, keys, POINTL::default(), &raw mut effect)
        })();
        ILFree(Some(pidl.cast_const()));
        result
    }
}

/// The one-item menu's object: what runs the command when the item is invoked.
#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    reason = "code generated by windows' implement macro"
)]
mod single {
    use super::{
        Action, CMINVOKECOMMANDINFO, E_NOTIMPL, HMENU, HRESULT, HWND, IContextMenu,
        IContextMenu_Impl, IObjectWithSite, PSTR, drop_on, run_verb,
    };

    /// The rows' actions: row id `n` at place `n - 1`, and the command given a view site, whose
    /// site is cleared with the menu.
    #[windows::core::implement(IContextMenu)]
    pub(super) struct Single(pub(super) Vec<Action>, pub(super) Option<IObjectWithSite>);

    impl Drop for Single {
        fn drop(&mut self) {
            if let Some(command) = &self.1 {
                // SAFETY: plain COM call; the menu is dropped on the thread that built it.
                let _ = unsafe { command.SetSite(None::<&windows::core::IUnknown>) };
            }
        }
    }

    impl IContextMenu_Impl for Single_Impl {
        fn QueryContextMenu(&self, _: HMENU, _: u32, _: u32, _: u32, _: u32) -> HRESULT {
            HRESULT(0)
        }

        fn InvokeCommand(&self, info: *const CMINVOKECOMMANDINFO) -> windows::core::Result<()> {
            // SAFETY: the caller (`win_menu::invoke_in`) passes a valid structure for the call.
            let info = unsafe { info.as_ref() };
            let owner = info.map_or_else(HWND::default, |i| i.hwnd);
            // MAKEINTRESOURCE: the row's place (`win_menu::invoke_in`); a text verb isn't one.
            let place = info.map_or(0, |i| i.lpVerb.0 as usize);
            let Some(action) = self.0.get(place).filter(|_| place >> 16 == 0) else {
                return Err(E_NOTIMPL.into());
            };
            match action {
                Action::Verb {
                    class,
                    verb,
                    file,
                    folder,
                } => run_verb(class, verb, file, folder, owner),
                // SAFETY: plain COM call on the thread that created it.
                Action::Command { command, items } => unsafe { command.Invoke(items, None) },
                Action::Drop { target, items } => drop_on(target, items, owner),
                Action::None => Err(E_NOTIMPL.into()),
            }
        }

        fn GetCommandString(
            &self,
            _: usize,
            _: u32,
            _: *const u32,
            _: PSTR,
            _: u32,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
    }
}
use single::Single;

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine's own Send To, read the way a pinned command's source is: the submenu with
    /// Windows' label and every target, quickly. Prints what it found.
    #[test]
    #[ignore = "reads this machine's Send To; run by hand"]
    #[allow(
        clippy::print_stderr,
        reason = "a by-hand measurement prints what it found"
    )]
    fn this_machines_send_to_reads_quickly() {
        let _com = crate::win_fileops::Com::init().unwrap();
        let file = crate::win::long_temp_dir()
            .join(format!("arke-sendto-real-{}.txt", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        let handler = MenuHandler::send_to();
        let read = || {
            let started = std::time::Instant::now();
            let menu = super::super::build_handler(
                std::slice::from_ref(&file),
                false,
                false,
                &handler,
                HWND::default(),
            )
            .unwrap();
            (menu, started.elapsed())
        };
        let (_, first) = read();
        let (menu, took) = read();
        eprintln!("first {first:?}, again {took:?}");
        let _ = std::fs::remove_file(&file);
        let Some(ShellMenuEntry::Submenu {
            label, children, ..
        }) = menu.entries.first()
        else {
            panic!("no submenu: {:?}", menu.entries.len());
        };
        let names: Vec<String> = children
            .iter()
            .filter_map(|c| match c {
                ShellMenuEntry::Item { label, icon, .. } => Some(format!(
                    "{label}{}",
                    if icon.is_some() { "" } else { " (no icon)" }
                )),
                _ => None,
            })
            .collect();
        eprintln!("{label}: {names:?} in {took:?}");
        assert!(!names.is_empty());
        assert!(took < std::time::Duration::from_millis(50), "{took:?}");
    }

    /// A Send To folder stand-in: a program's shortcut (named without ".lnk", as File Explorer
    /// shows it) and a hidden desktop.ini, which stays out (#278).
    fn send_to_folder_with_shortcut(dir: &Path) -> PathBuf {
        let _ = std::fs::remove_dir_all(dir);
        let send_to = dir.join("SendTo");
        std::fs::create_dir_all(&send_to).unwrap();
        let script = dir.join("args.cmd");
        std::fs::write(&script, "@echo %*> \"%~dp0args.txt\"\r\n").unwrap();
        let ini = send_to.join("desktop.ini");
        std::fs::write(&ini, "[.ShellClassInfo]\r\n").unwrap();
        let ini_w = wide(ini.as_os_str());
        // SAFETY: NUL-terminated path; hidden and system, as Windows keeps desktop.ini.
        unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                PCWSTR(ini_w.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
                    | windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM,
            )
            .unwrap();
        }
        let shortcut = send_to.join("Args tool.lnk");
        // SAFETY: a ShellLink object saved through IPersistFile; strings are NUL-terminated.
        unsafe {
            use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, IPersistFile};
            use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let target = wide(script.as_os_str());
            link.SetPath(PCWSTR(target.as_ptr())).unwrap();
            let save = wide(shortcut.as_os_str());
            link.cast::<IPersistFile>()
                .unwrap()
                .Save(PCWSTR(save.as_ptr()), true)
                .unwrap();
        }
        send_to
    }

    #[test]
    fn send_to_lists_its_folder_as_explorer_names_it() {
        let dir = crate::win::long_temp_dir().join(format!("arke-sendto-{}", std::process::id()));
        let _com = crate::win_fileops::Com::init().unwrap();
        let send_to = send_to_folder_with_shortcut(&dir);
        let names: Vec<String> = send_to_targets(&send_to)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(names, ["Args tool"]);
    }

    /// The real thing: a file sent to a program's shortcut reaches the program as an argument,
    /// through the drop the way File Explorer's Send To runs it.
    #[test]
    #[ignore = "starts cmd.exe through the shell; run by hand"]
    fn a_file_sent_to_a_shortcut_reaches_its_program() {
        let dir =
            crate::win::long_temp_dir().join(format!("arke-sendto-run-{}", std::process::id()));
        let _com = crate::win_fileops::Com::init().unwrap();
        let send_to = send_to_folder_with_shortcut(&dir);
        let file = dir.join("sent.txt");
        std::fs::write(&file, "x").unwrap();
        let pidls = parse_pidls(std::slice::from_ref(&file)).unwrap();
        let ids: Vec<*const ITEMIDLIST> = pidls.0.iter().map(|p| p.cast_const()).collect();
        // SAFETY: the PIDLs are valid for the call.
        let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromIDLists(&ids) }.unwrap();
        let (_, target) = send_to_targets(&send_to).remove(0);
        drop_on(&target, &items, HWND::default()).unwrap();
        let out = dir.join("args.txt");
        let mut got = None;
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(&out) {
                got = Some(text);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let got = got.expect("the program never ran");
        assert!(got.contains("sent.txt"), "{got:?}");
    }

    #[test]
    fn icon_specs_split_into_path_and_index() {
        assert_eq!(
            split_icon_spec(r"C:\app\app.exe,-101"),
            Some((r"C:\app\app.exe".into(), -101))
        );
        assert_eq!(
            split_icon_spec(r#""C:\a b\x.ico""#),
            Some((r"C:\a b\x.ico".into(), 0))
        );
        assert_eq!(
            split_icon_spec(r#""C:\a,b\x.exe",2"#),
            Some((r"C:\a,b\x.exe".into(), 2))
        );
        assert_eq!(split_icon_spec("  "), None);
    }

    #[test]
    fn environment_variables_expand() {
        let root = std::env::var("SystemRoot").unwrap_or_default();
        assert_eq!(
            expand_env(r"%SystemRoot%\system32\x.dll"),
            format!(r"{root}\system32\x.dll")
        );
        assert_eq!(expand_env("%NO_SUCH_VAR_ARKE%"), "%NO_SUCH_VAR_ARKE%");
        assert_eq!(expand_env("100%"), "100%");
    }
}
