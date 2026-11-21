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
    let label = read("MUIVerb")
        .or_else(|| read(""))
        .map_or_else(|| handler.name.clone(), |s| indirect(&s));
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
    (
        menu,
        vec![item(1, &handler.name, &label, true, false, icon)],
    )
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

/// The one-item menu's object: what runs the command when the item is invoked.
#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    reason = "code generated by windows' implement macro"
)]
mod single {
    use super::{
        Action, CMINVOKECOMMANDINFO, E_NOTIMPL, HMENU, HRESULT, HWND, IContextMenu,
        IContextMenu_Impl, IObjectWithSite, PSTR, run_verb,
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
