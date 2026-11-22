//! "More options", one extension at a time.
//!
//! Windows builds the classic context menu by calling every registered extension in turn, so the
//! menu takes as long as all of them together, and one slow extension (a version control status
//! check, a sandbox, a cloud client) holds up everything. File Explorer and most file managers
//! have that delay; Directory Opus queries extensions one by one instead.
//!
//! Arke still reads the whole Windows menu (`win_menu::query`), which stays the reference for
//! "Show more options". Next to it, the menu host can find the extensions that apply to an item
//! ([`handlers`]) and load any of them on its own thread (`win_menu::query_handler`), all at the
//! same time. Items the user pinned to the top of Arke's menu then show as soon as their own
//! extension answers, instead of after the slowest one. How long each extension took is logged,
//! so a slow one can be named.
//!
//! Which extensions apply follows the keys File Explorer reads them from: the file's class
//! (the user's choice of app first), its extension, `SystemFileAssociations` for the extension and
//! its kind, then `*` and `AllFilesystemObjects`; `Directory`, `Folder` and `AllFilesystemObjects`
//! for a folder; `Drive` and `Folder` for a drive; `Directory\Background` for empty space.
//! Extensions an administrator blocked (`Shell Extensions\Blocked`) are left out.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One source of Windows menu commands that applies to an item, read on its own (#129, #278).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MenuHandler {
    /// Key under `HKEY_CLASSES_ROOT` it is registered under; handed to an extension, which
    /// may read its settings there. For a registry command, the class key holding its `shell`.
    pub(crate) key: String,
    /// Its COM class, `{...}` (an extension, or the object that runs a command); empty for a
    /// registry command run by its command line.
    pub(crate) clsid: String,
    /// Registered name, for logs (a registry command's verb).
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) kind: HandlerKind,
    /// A registry command in a cascading menu (`ExtendedSubCommandsKey`, as context menu
    /// managers make them): the registry path of the verb that opens that menu
    /// (`*\shell\Tools`), under whose label it is shown, so its pinned key matches (#278).
    #[serde(default)]
    pub(crate) parent: Option<String>,
}

/// Where a menu command comes from. Each item of the whole Windows menu says which with its
/// canonical verb (`GCS_VERBW`): a registry command's verb, a packaged app's command's `{CLSID}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HandlerKind {
    /// A `shellex\ContextMenuHandlers` extension (a DLL that adds any number of items).
    #[default]
    Extension,
    /// A command in the registry, `<class>\shell\<verb>`: run by Windows on its own
    /// (`ShellExecuteEx` with the class key), or by its `ExplorerCommandHandler` (`clsid`).
    Verb {
        /// Shown only with Shift held (`Extended`).
        extended: bool,
    },
    /// A packaged app's command (`desktop5:ItemType` in its manifest): an `IExplorerCommand`.
    Command,
}

impl MenuHandler {
    /// How a pinned command remembers its source (`Settings::menu_pin_owners`): the extension's
    /// class id, `verb:<verb>` or `command:{CLSID}`.
    pub(crate) fn owner(&self) -> String {
        match self.kind {
            HandlerKind::Extension => self.clsid.clone(),
            HandlerKind::Verb { .. } => format!("{VERB_OWNER}{}", self.name),
            HandlerKind::Command => format!("{COMMAND_OWNER}{}", self.clsid),
        }
    }

    /// Windows' Send To extension, which the menu host lists from its folder itself (#278).
    pub(crate) fn send_to() -> Self {
        Self {
            key: "AllFilesystemObjects".into(),
            clsid: SEND_TO.into(),
            name: "SendTo".into(),
            kind: HandlerKind::Extension,
            parent: None,
        }
    }

    /// Whether this is Windows' Send To extension.
    pub(crate) fn is_send_to(&self) -> bool {
        self.kind == HandlerKind::Extension && self.clsid.eq_ignore_ascii_case(SEND_TO)
    }

    /// A packaged app's command, `clsid` `{...}`.
    pub(crate) fn command(clsid: &str) -> Self {
        Self {
            key: String::new(),
            clsid: clsid.to_string(),
            name: clsid.to_string(),
            kind: HandlerKind::Command,
            parent: None,
        }
    }
}

/// Windows' Send To extension's class.
pub(crate) const SEND_TO: &str = "{7BA4C740-9E81-11CF-99D3-00AA004AE837}";

/// Owner prefixes (see [`MenuHandler::owner`]).
pub(crate) const VERB_OWNER: &str = "verb:";
pub(crate) const COMMAND_OWNER: &str = "command:";

/// The item kind a menu is for, as packaged apps register commands for it
/// (`desktop5:ItemType Type`): `Directory\Background`, `Drive`, `Directory`, or a file's
/// extension (`.txt`, lowercased; `*` without one). The first item decides.
pub(crate) fn item_kind(paths: &[PathBuf], background: bool) -> Option<String> {
    let first = paths.first()?;
    Some(if background {
        r"Directory\Background".to_string()
    } else {
        kind_of(&target_of(first))
    })
}

/// [`item_kind`] from what the list already knows of the first item (its path, whether it is a
/// folder), reading nothing: for drawing pinned rows the moment the menu opens. `None` on a
/// server, where the Windows menu isn't read.
pub(crate) fn item_kind_known(first: Option<&(PathBuf, bool)>, background: bool) -> Option<String> {
    if background {
        return Some(r"Directory\Background".to_string());
    }
    let (path, folder) = first?;
    if arke_core::remote::is_remote(path) {
        return None;
    }
    Some(if path.parent().is_none() {
        "Drive".into()
    } else if *folder {
        "Directory".into()
    } else {
        file_ext(path).unwrap_or_else(|| "*".into())
    })
}

/// `.txt`, lowercased.
fn file_ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
}

fn kind_of(target: &Target) -> String {
    match target {
        Target::Background => r"Directory\Background".into(),
        Target::Drive => "Drive".into(),
        Target::Folder => "Directory".into(),
        Target::File { ext, .. } => ext.clone().unwrap_or_else(|| "*".into()),
    }
}

/// Whether a command registered for `types` (item types from a manifest, or the kinds it was
/// seen on) applies to an item of `kind`: `*` covers every file.
pub(crate) fn kind_matches(types: &[String], kind: &str) -> bool {
    let file = kind.starts_with('.') || kind == "*";
    types
        .iter()
        .any(|t| t.eq_ignore_ascii_case(kind) || (file && t == "*"))
}

/// What an item is, as far as choosing extensions goes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Empty space in a folder.
    Background,
    Drive,
    Folder,
    File {
        /// `.txt`, lowercased; `None` without an extension.
        ext: Option<String>,
        /// Its classes: the user's choice of app first, then the registered default.
        classes: Vec<String>,
        /// `PerceivedType` of the extension (`image`, `text`...).
        perceived: Option<String>,
    },
}

/// Keys under `HKEY_CLASSES_ROOT` whose `shellex\ContextMenuHandlers` apply, in File Explorer's
/// order.
pub(crate) fn class_keys(target: &Target) -> Vec<String> {
    let fixed = |keys: &[&str]| keys.iter().map(ToString::to_string).collect();
    match target {
        Target::Background => fixed(&[r"Directory\Background"]),
        Target::Drive => fixed(&["Drive", "Folder"]),
        Target::Folder => fixed(&["Directory", "Folder", "AllFilesystemObjects"]),
        Target::File {
            ext,
            classes,
            perceived,
        } => {
            let mut keys: Vec<String> = Vec::new();
            let mut add = |key: String| {
                if !key.is_empty() && !keys.iter().any(|k| k.eq_ignore_ascii_case(&key)) {
                    keys.push(key);
                }
            };
            for class in classes {
                add(class.clone());
            }
            if let Some(ext) = ext {
                add(ext.clone());
                add(format!(r"SystemFileAssociations\{ext}"));
            }
            if let Some(kind) = perceived {
                add(format!(r"SystemFileAssociations\{kind}"));
            }
            add("*".into());
            add("AllFilesystemObjects".into());
            keys
        }
    }
}

/// A `{...}` class id.
fn is_clsid(s: &str) -> bool {
    s.len() == 38 && s.starts_with('{') && s.ends_with('}')
}

/// The extensions that apply to `paths` (Arke's selection is in one folder; the first item
/// decides, as in File Explorer), or to the folder's empty space. Reads the registry only.
pub(crate) fn handlers(paths: &[PathBuf], background: bool) -> Vec<MenuHandler> {
    let Some(first) = paths.first() else {
        return Vec::new();
    };
    let target = if background {
        Target::Background
    } else {
        target_of(first)
    };
    let mut out: Vec<MenuHandler> = Vec::new();
    for key in class_keys(&target) {
        let list = format!(r"{key}\shellex\ContextMenuHandlers");
        for entry in reg::subkeys(&list) {
            let clsid = if is_clsid(&entry) {
                entry.clone()
            } else {
                match reg::default_value(&format!(r"{list}\{entry}")) {
                    Some(value) if is_clsid(value.trim()) => value.trim().to_string(),
                    _ => continue,
                }
            };
            if out.iter().any(|h| h.clsid.eq_ignore_ascii_case(&clsid)) || reg::blocked(&clsid) {
                continue;
            }
            let name = if is_clsid(&entry) {
                reg::default_value(&format!(r"CLSID\{clsid}")).unwrap_or_else(|| clsid.clone())
            } else {
                entry.trim().to_string()
            };
            out.push(MenuHandler {
                key: key.clone(),
                clsid,
                name,
                kind: HandlerKind::Extension,
                parent: None,
            });
        }
    }
    out.extend(verbs(&target, paths.len()));
    out
}

/// Registry commands (`<class>\shell\<verb>`) for `target`, each verb once, the first class
/// that has it winning, as File Explorer does, and the commands of a cascading menu one level
/// down (`ExtendedSubCommandsKey`: the key whose `shell` holds them, run with that key as their
/// class). Left out: ones Windows decides on by itself (`AppliesTo`, `CommandStateHandler`),
/// Windows' own cascades (`SubCommands`), hidden ones (`LegacyDisable`,
/// `ProgrammaticAccessOnly`), and, for several items, ones run by command line (Windows runs
/// those once per item, by rules of their own).
fn verbs(target: &Target, items: usize) -> Vec<MenuHandler> {
    let mut out: Vec<MenuHandler> = Vec::new();
    for key in class_keys(target) {
        let shell = format!(r"{key}\shell");
        for verb in reg::subkeys(&shell) {
            if out
                .iter()
                .any(|h| h.parent.is_none() && h.name.eq_ignore_ascii_case(&verb))
            {
                continue;
            }
            let at = format!(r"{shell}\{verb}");
            if !shown_alone(&at) {
                continue;
            }
            let extended = reg::value_exists(&at, "Extended");
            if let Some(sub) = reg::value(&at, "ExtendedSubCommandsKey")
                .map(|k| k.trim().trim_matches('\\').to_string())
                .filter(|k| !k.is_empty())
            {
                let sub_shell = format!(r"{sub}\shell");
                for child in reg::subkeys(&sub_shell) {
                    let child_at = format!(r"{sub_shell}\{child}");
                    // Each child once; a top-level verb of the same name is still its own.
                    if out
                        .iter()
                        .any(|h| h.parent.is_some() && h.name.eq_ignore_ascii_case(&child))
                        || !shown_alone(&child_at)
                        || reg::value_exists(&child_at, "ExtendedSubCommandsKey")
                    {
                        continue;
                    }
                    if let Some(handler) = verb_handler(&sub, &child, &child_at, items) {
                        out.push(MenuHandler {
                            kind: HandlerKind::Verb {
                                extended: extended || reg::value_exists(&child_at, "Extended"),
                            },
                            parent: Some(at.clone()),
                            ..handler
                        });
                    }
                }
                continue;
            }
            if let Some(handler) = verb_handler(&key, &verb, &at, items) {
                out.push(MenuHandler {
                    kind: HandlerKind::Verb { extended },
                    ..handler
                });
            }
        }
    }
    out
}

/// Whether the verb at registry path `at` can be read on its own: not one Windows decides on by
/// itself, not Windows' own cascade, not hidden.
fn shown_alone(at: &str) -> bool {
    ![
        "AppliesTo",
        "CommandStateHandler",
        "SubCommands",
        "LegacyDisable",
        "ProgrammaticAccessOnly",
    ]
    .into_iter()
    .any(|name| reg::value_exists(at, name))
}

/// The verb `verb` of class `key` (at registry path `at`) as a command run on its own: by its
/// `ExplorerCommandHandler`, or its command line for one item. `None` when neither applies.
fn verb_handler(key: &str, verb: &str, at: &str, items: usize) -> Option<MenuHandler> {
    let handler = reg::value(at, "ExplorerCommandHandler")
        .map(|c| c.trim().to_string())
        .filter(|c| is_clsid(c));
    let by_line = reg::subkeys(at)
        .iter()
        .any(|k| k.eq_ignore_ascii_case("command"));
    if handler.is_none() && (!by_line || items > 1) {
        return None;
    }
    Some(MenuHandler {
        key: key.to_string(),
        clsid: handler.unwrap_or_default(),
        name: verb.to_string(),
        kind: HandlerKind::Verb { extended: false },
        parent: None,
    })
}

fn target_of(path: &Path) -> Target {
    if path.parent().is_none() {
        return Target::Drive;
    }
    if path.is_dir() {
        return Target::Folder;
    }
    let ext = file_ext(path);
    let mut classes = Vec::new();
    let mut perceived = None;
    if let Some(ext) = &ext {
        classes.extend(reg::user_choice(ext));
        classes.extend(reg::default_value(ext));
        perceived = reg::value(ext, "PerceivedType");
    }
    Target::File {
        ext,
        classes,
        perceived,
    }
}

mod reg {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_ANY, RegGetValueW,
    };
    use windows::core::PCWSTR;

    use crate::win_fileops::wide;

    pub(super) fn subkeys(key: &str) -> Vec<String> {
        crate::win::reg_subkeys(HKEY_CLASSES_ROOT, key)
    }

    pub(super) fn default_value(key: &str) -> Option<String> {
        value(key, "")
    }

    pub(super) fn value(key: &str, name: &str) -> Option<String> {
        crate::win::read_reg_string(HKEY_CLASSES_ROOT, key, name)
    }

    /// Whether `key` has a value `name`, of any type (flags like `Extended` are often empty).
    pub(super) fn value_exists(key: &str, name: &str) -> bool {
        exists(HKEY_CLASSES_ROOT, key, name)
    }

    /// The app the user chose for `ext` ("Open with" › "Always").
    pub(super) fn user_choice(ext: &str) -> Option<String> {
        crate::win::read_reg_string(
            HKEY_CURRENT_USER,
            &format!(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}\UserChoice"
            ),
            "ProgId",
        )
    }

    /// Whether an administrator blocked the extension (the value's presence is what counts).
    pub(super) fn blocked(clsid: &str) -> bool {
        const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked";
        [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER]
            .into_iter()
            .any(|root| exists(root, KEY, clsid))
    }

    fn exists(root: HKEY, key: &str, name: &str) -> bool {
        let (key, name) = (
            wide(std::ffi::OsStr::new(key)),
            wide(std::ffi::OsStr::new(name)),
        );
        // SAFETY: both names are NUL-terminated; only the value's presence is asked.
        unsafe {
            RegGetValueW(
                root,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_ANY,
                None,
                None,
                None,
            )
        }
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_reads_its_classes_then_the_shared_keys() {
        let keys = class_keys(&Target::File {
            ext: Some(".txt".into()),
            classes: vec!["txtfile".into(), "TXTFILE".into()],
            perceived: Some("text".into()),
        });
        assert_eq!(
            keys,
            [
                "txtfile",
                ".txt",
                r"SystemFileAssociations\.txt",
                r"SystemFileAssociations\text",
                "*",
                "AllFilesystemObjects"
            ]
        );
        let bare = class_keys(&Target::File {
            ext: None,
            classes: Vec::new(),
            perceived: None,
        });
        assert_eq!(bare, ["*", "AllFilesystemObjects"]);
        assert_eq!(
            class_keys(&Target::Folder),
            ["Directory", "Folder", "AllFilesystemObjects"]
        );
        assert_eq!(class_keys(&Target::Background), [r"Directory\Background"]);
    }

    /// On this machine: the extensions for a text file include the ones registered for every
    /// file, each class once.
    #[test]
    fn real_handlers_for_a_file() {
        let file = std::env::temp_dir().join(format!("arke-menu-parts-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let found = handlers(std::slice::from_ref(&file), false);
        let _ = std::fs::remove_file(&file);
        let star = crate::win::reg_subkeys(
            windows::Win32::System::Registry::HKEY_CLASSES_ROOT,
            r"*\shellex\ContextMenuHandlers",
        );
        if !star.is_empty() {
            assert!(!found.is_empty(), "{star:?}");
        }
        for (i, h) in found
            .iter()
            .filter(|h| h.kind == HandlerKind::Extension)
            .enumerate()
        {
            assert!(is_clsid(&h.clsid), "{h:?}");
            assert!(
                !found[..i]
                    .iter()
                    .any(|o| o.clsid.eq_ignore_ascii_case(&h.clsid)),
                "{h:?} twice"
            );
        }
    }
}

/// By hand: `cargo test -p arke-app menu_extension_timing -- --ignored --nocapture`. Loads the
/// extensions into the test process. Prints the whole menu's time next to each extension's alone
/// and all of them at once.
#[cfg(test)]
#[allow(clippy::print_stdout, reason = "a report printed for a run by hand")]
mod timing {
    use std::time::Instant;

    use super::*;

    fn time(paths: &[PathBuf], background: bool, label: &str) {
        // One group per menu, as Arke asks: a newer group releases the older ones' menus, so a
        // group per extension would expire all but the last.
        static GROUP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000);
        let started = Instant::now();
        let whole = crate::win_menu::query(paths, background, false);
        let whole_ms = started.elapsed().as_millis();
        if let Ok(menu) = &whole {
            crate::win_menu::release(menu.session);
        }
        let only = std::env::var("ARKE_TIMING_ONLY").ok();
        let found: Vec<MenuHandler> = handlers(paths, background)
            .into_iter()
            .filter(|h| only.as_deref().is_none_or(|o| h.name.contains(o)))
            .collect();
        println!(
            "{label}: whole menu {whole_ms} ms ({}), {} extensions",
            if whole.is_ok() { "ok" } else { "failed" },
            found.len()
        );
        let group = GROUP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let all = Instant::now();
        let threads: Vec<_> = found
            .into_iter()
            .map(|h| {
                let paths = paths.to_vec();
                std::thread::spawn(move || {
                    let started = Instant::now();
                    let menu = crate::win_menu::query_handler(&paths, background, false, &h, group);
                    let ms = started.elapsed().as_millis();
                    let items = menu.as_ref().map_or(0, |m| m.entries.len());
                    if let Ok(m) = &menu
                        && std::env::var_os("ARKE_TIMING_ONLY").is_some()
                    {
                        println!(
                            "{:#?}",
                            m.entries.iter().map(label_tree).collect::<Vec<_>>()
                        );
                        // Submenus filled when opened, as if opened now (nothing is run).
                        for (label, index) in m.entries.iter().filter_map(|e| match e {
                            crate::shell::ShellMenuEntry::Submenu {
                                label,
                                lazy: Some(index),
                                ..
                            } => Some((label, *index)),
                            _ => None,
                        }) {
                            let opened = Instant::now();
                            let filled = crate::win_menu::fill_submenu(m.session, index);
                            println!(
                                "  {label} opened in {} ms: {:?}",
                                opened.elapsed().as_millis(),
                                filled.map(|c| c.iter().map(label_tree).collect::<Vec<_>>())
                            );
                        }
                    }
                    if let Ok(m) = &menu {
                        crate::win_menu::release(m.session);
                    }
                    (h, ms, items, menu.err().map(|e| e.to_string()))
                })
            })
            .collect();
        for t in threads {
            let (h, ms, items, err) = t.join().unwrap();
            println!(
                "  {ms:>5} ms  {items:>2} items  {} {} [{}]{}",
                h.name,
                h.clsid,
                h.key,
                err.map_or_else(String::new, |e| format!("  ERROR {e}"))
            );
        }
        println!("  all at once: {} ms", all.elapsed().as_millis());
    }

    fn label_tree(e: &crate::shell::ShellMenuEntry) -> String {
        match e {
            crate::shell::ShellMenuEntry::Item { label, .. } => label.clone(),
            crate::shell::ShellMenuEntry::Submenu {
                label,
                children,
                lazy,
                ..
            } => format!(
                "{label} ›{} [{}]",
                if lazy.is_some() {
                    " (filled when opened)"
                } else {
                    ""
                },
                children
                    .iter()
                    .map(label_tree)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            crate::shell::ShellMenuEntry::Separator => "—".into(),
        }
    }

    #[test]
    #[ignore = "loads this machine's shell extensions; run by hand"]
    fn menu_extension_timing() {
        let dir = std::env::temp_dir().join("arke-menu-timing");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.zip", "c.png", "d.pdf"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        for name in ["a.txt", "b.zip", "c.png", "d.pdf"] {
            time(&[dir.join(name)], false, name);
        }
        time(std::slice::from_ref(&dir), false, "folder");
        time(std::slice::from_ref(&dir), true, "background");
        // Second round: warm.
        time(&[dir.join("a.txt")], false, "a.txt again");
    }
}

/// By hand: `cargo test -p arke-app menu_thread_leftovers -- --ignored --nocapture`. Reads menus
/// many times in this process and reports the threads left behind, by the module they started in.
#[cfg(test)]
#[allow(clippy::print_stdout, reason = "a report printed for a run by hand")]
mod leftovers {
    use std::collections::{BTreeMap, HashSet};
    use std::ffi::c_void;

    use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    use windows::Win32::System::Threading::{OpenThread, THREAD_QUERY_INFORMATION};

    use super::*;

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationThread(
            thread: HANDLE,
            class: u32,
            info: *mut c_void,
            len: u32,
            returned: *mut u32,
        ) -> i32;
    }

    fn threads() -> HashSet<u32> {
        let me = std::process::id();
        let mut out = HashSet::new();
        // SAFETY: snapshot of all threads; closed below.
        let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
            return out;
        };
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: entry.dwSize is set.
        let mut ok = unsafe { Thread32First(snap, &raw mut entry) }.is_ok();
        while ok {
            if entry.th32OwnerProcessID == me {
                out.insert(entry.th32ThreadID);
            }
            // SAFETY: as above.
            ok = unsafe { Thread32Next(snap, &raw mut entry) }.is_ok();
        }
        // SAFETY: opened above.
        let _ = unsafe { CloseHandle(snap) };
        out
    }

    /// The module a thread started in (its Win32 start address).
    fn start_module(id: u32) -> String {
        // SAFETY: plain calls; the handle is closed below.
        let Ok(thread) = (unsafe { OpenThread(THREAD_QUERY_INFORMATION, false, id) }) else {
            return "?".into();
        };
        let mut start: usize = 0;
        // SAFETY: class 9 (ThreadQuerySetWin32StartAddress) writes one pointer.
        let status = unsafe {
            NtQueryInformationThread(
                thread,
                9,
                (&raw mut start).cast(),
                size_of::<usize>() as u32,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: opened above.
        let _ = unsafe { CloseHandle(thread) };
        if status != 0 {
            return "?".into();
        }
        let mut module = HMODULE::default();
        // SAFETY: looks up the module containing the address without changing its refcount.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                windows::core::PCWSTR(start as *const u16),
                &raw mut module,
            )
        };
        if found.is_err() {
            return format!("{start:#x}");
        }
        let mut buf = [0u16; 260];
        // SAFETY: buf is writable.
        let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
        let path = String::from_utf16_lossy(&buf[..len]);
        let base = path.rsplit('\\').next().unwrap_or(&path).to_string();
        format!("{base}+{:#x}", start - module.0 as usize)
    }

    fn report(label: &str, before: &HashSet<u32>) -> HashSet<u32> {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let now = threads();
        let mut by: BTreeMap<String, usize> = BTreeMap::new();
        for id in now.difference(before) {
            *by.entry(start_module(*id)).or_default() += 1;
        }
        println!(
            "{label}: {} -> {} threads, new: {by:?}",
            before.len(),
            now.len()
        );
        now
    }

    #[test]
    #[ignore = "loads this machine's shell extensions; run by hand"]
    fn menu_thread_leftovers() {
        const ROUNDS: usize = 10;
        let dir = std::env::temp_dir().join("arke-menu-leftovers");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        // Warm up once so one-time threads don't count.
        for (paths, background) in [(vec![file.clone()], false), (vec![dir.clone()], true)] {
            if let Ok(m) = crate::win_menu::query(&paths, background, false) {
                crate::win_menu::release(m.session);
            }
        }
        let mut seen = report("warm-up", &threads());
        for (label, paths, background) in [
            ("file menu", vec![file], false),
            ("background menu", vec![dir], true),
        ] {
            for _ in 0..ROUNDS {
                if let Ok(m) = crate::win_menu::query(&paths, background, false) {
                    crate::win_menu::release(m.session);
                }
            }
            seen = report(&format!("{label} x{ROUNDS}"), &seen);
            for handler in handlers(&paths, background) {
                for round in 0..ROUNDS {
                    if let Ok(m) = crate::win_menu::query_handler(
                        &paths,
                        background,
                        false,
                        &handler,
                        10_000 + round as u64,
                    ) {
                        crate::win_menu::release(m.session);
                    }
                }
                seen = report(&format!("  {} alone x{ROUNDS}", handler.name), &seen);
            }
        }
    }
}
