//! Taskbar jump list: pinned and recent folders that open in Arke, plus a "New window" task.
//!
//! Uses `ICustomDestinationList`; Windows stores the list in the Recent folder, no registry
//! writes. Each entry is a shortcut to arke.exe with the folder as argument. Entries the user
//! removed from the list are never added back.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, ShellLink,
};
use windows::core::{Interface, PCWSTR, PWSTR};

/// Max entries per category; Windows shows only about ten by default anyway.
const MAX_PER_CATEGORY: usize = 8;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Shortcut arguments: the quoted folder path.
fn args_for(folder: &Path) -> String {
    format!("\"{}\"", folder.display())
}

/// Display title: folder name, or the full path for drive roots.
fn title_for(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// VT_LPWSTR PROPVARIANT. The string is CoTaskMemAlloc'd and freed by PropVariantClear on drop.
fn string_variant(text: &str) -> Option<PROPVARIANT> {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * 2;
    // SAFETY: allocates `bytes`, fully written below and then owned by the PROPVARIANT.
    let ptr = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
    if ptr.is_null() {
        return None;
    }
    // SAFETY: ptr points to freshly allocated memory of sufficient size.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len()) };
    Some(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    pwszVal: PWSTR(ptr),
                },
            }),
        },
    })
}

/// Builds one jump list shortcut to `exe` with the given arguments, title, tooltip and icon.
///
/// # Safety
/// COM must be initialized on this thread.
unsafe fn link(
    exe: &Path,
    args: &str,
    title: &str,
    tip: &str,
    icon: (&str, i32),
) -> Option<IShellLinkW> {
    // SAFETY: COM is initialized per the contract; wide strings outlive each call.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let exe_w = wide(exe.as_os_str());
        link.SetPath(PCWSTR(exe_w.as_ptr())).ok()?;
        let args_w = wide(std::ffi::OsStr::new(args));
        link.SetArguments(PCWSTR(args_w.as_ptr())).ok()?;
        let tip_w = wide(std::ffi::OsStr::new(tip));
        let _ = link.SetDescription(PCWSTR(tip_w.as_ptr()));
        let icon_w = wide(std::ffi::OsStr::new(icon.0));
        let _ = link.SetIconLocation(PCWSTR(icon_w.as_ptr()), icon.1);
        let store: IPropertyStore = link.cast().ok()?;
        let value = string_variant(title)?;
        let key = PKEY_Title;
        store.SetValue(&raw const key, &raw const value).ok()?;
        store.Commit().ok()?;
        Some(link)
    }
}

/// Arguments of entries the user removed; these must not be added back.
///
/// # Safety
/// COM must be initialized on this thread.
unsafe fn removed_args(removed: &IObjectArray) -> HashSet<String> {
    let mut out = HashSet::new();
    // SAFETY: COM is initialized per the contract; `removed` is a valid interface.
    unsafe {
        let count = removed.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(link) = removed.GetAt::<IShellLinkW>(i) else {
                continue;
            };
            let mut buf = [0u16; 1024];
            if link.GetArguments(&mut buf).is_ok() {
                let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
                out.insert(String::from_utf16_lossy(&buf[..end]));
            }
        }
    }
    out
}

/// Replaces the jump list. Call from a background thread; initializes COM.
pub(crate) fn update(
    pinned: &[PathBuf],
    recent: &[PathBuf],
) -> Result<(), crate::shell::ShellError> {
    let exe = std::env::current_exe()?;
    let _com = crate::win_fileops::Com::init()?;
    let folder_icon = (r"%SystemRoot%\System32\imageres.dll", 3);
    // SAFETY: COM is initialized; all interfaces are created and used on this thread.
    unsafe {
        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        let mut slots = 0u32;
        let removed: IObjectArray = list.BeginList(&raw mut slots)?;
        let skip = removed_args(&removed);
        let category = |items: &[PathBuf], exclude: &[PathBuf]| -> Option<IObjectArray> {
            let col: IObjectCollection =
                CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER).ok()?;
            let mut added = 0;
            for folder in items {
                if added >= MAX_PER_CATEGORY {
                    break;
                }
                let args = args_for(folder);
                if skip.contains(&args) || exclude.contains(folder) {
                    continue;
                }
                let tip = folder.display().to_string();
                if let Some(link) = link(&exe, &args, &title_for(folder), &tip, folder_icon) {
                    let _ = col.AddObject(&link);
                    added += 1;
                }
            }
            (added > 0).then(|| col.cast().ok()).flatten()
        };
        if let Some(arr) = category(pinned, &[]) {
            let name = wide(std::ffi::OsStr::new(&*rust_i18n::t!("jumplist.pinned")));
            // A removed entry makes the whole category fail. They are skipped above; if it
            // still fails, the category is left out.
            let _ = list.AppendCategory(PCWSTR(name.as_ptr()), &arr);
        }
        if let Some(arr) = category(recent, pinned) {
            let name = wide(std::ffi::OsStr::new(&*rust_i18n::t!("jumplist.recent")));
            let _ = list.AppendCategory(PCWSTR(name.as_ptr()), &arr);
        }
        // Task: new window (handed to the running instance when Arke stays resident).
        let tasks: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        let exe_icon = exe.display().to_string();
        let title = rust_i18n::t!("tray.open").into_owned();
        if let Some(new_window) = link(&exe, "", &title, &title, (&exe_icon, 0)) {
            let _ = tasks.AddObject(&new_window);
        }
        let tasks: IObjectArray = tasks.cast()?;
        let _ = list.AddUserTasks(&tasks);
        Ok(list.CommitList()?)
    }
}

/// Removes Arke's jump list (uninstall). Call from a background thread; initializes COM.
pub(crate) fn clear() -> Result<(), crate::shell::ShellError> {
    let _com = crate::win_fileops::Com::init()?;
    // SAFETY: COM is initialized; the list is created and used on this thread. `None` means this
    // process's own app id, which for Arke is derived from its exe path.
    unsafe {
        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        Ok(list.DeleteList(PCWSTR::null())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_and_titles() {
        assert_eq!(
            args_for(Path::new(r"C:\Users\me\My Photos")),
            r#""C:\Users\me\My Photos""#
        );
        assert_eq!(title_for(Path::new(r"C:\Users\me\My Photos")), "My Photos");
        assert_eq!(title_for(Path::new(r"D:\")), r"D:\");
    }

    #[test]
    fn makes_a_titled_link() {
        let _com = crate::win_fileops::Com::init().unwrap();
        // SAFETY: COM is initialized above.
        let made = unsafe {
            link(
                Path::new(r"C:\Windows\notepad.exe"),
                r#""C:\Temp""#,
                "Temp",
                r"C:\Temp",
                (r"%SystemRoot%\System32\imageres.dll", 3),
            )
        };
        assert!(made.is_some());
    }
}
