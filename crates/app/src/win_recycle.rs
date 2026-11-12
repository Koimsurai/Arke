//! Recycle Bin contents.
//!
//! Enumerated through the shell (`FOLDERID_RecycleBinFolder`), so name, original location,
//! deletion time and size come from shell32 itself. Read only; restore and purge live in
//! `win_fileops.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, PROPERTYKEY};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_RecycleBinFolder, IEnumShellItems, IShellItem, IShellItem2,
    KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN, SIGDN_DESKTOPABSOLUTEPARSING,
    SIGDN_NORMALDISPLAY,
};
use windows::core::{GUID, Interface};

use crate::shell::RecycledItem;
use crate::win_fileops::{Com, in_recycle_bin};

/// Recycle Bin properties `System.Recycle.DeletedFrom` and `System.Recycle.DateDeleted`.
const RECYCLE: GUID = GUID::from_u128(0x9B17_4B33_40FF_11D2_A27E_00C0_4FC3_0871);
const DELETED_FROM: PROPERTYKEY = PROPERTYKEY {
    fmtid: RECYCLE,
    pid: 2,
};
const DATE_DELETED: PROPERTYKEY = PROPERTYKEY {
    fmtid: RECYCLE,
    pid: 3,
};
/// `System.Size`
const SIZE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xB725_F130_47EF_101A_A5F1_0260_8C9E_EBAC),
    pid: 12,
};

/// Whether items deleted from `path` can go to the Recycle Bin: not on network shares (`\\server`,
/// mapped drives, WSL) nor on removable or optical drives, where Windows deletes permanently.
/// `GetDriveTypeW` reads local mount info only, so this never waits on the network.
pub(crate) fn has_recycle_bin(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    // GetDriveTypeW results (WinBase.h).
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;
    let letter = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => return false,
            // Device paths and the like: let the copy engine decide.
            _ => return true,
        },
        _ => return true,
    };
    let root = windows::core::HSTRING::from(format!(r"{}:\", char::from(letter)));
    // SAFETY: root is a NUL-terminated wide string that outlives the call.
    let kind = unsafe { GetDriveTypeW(&root) };
    !matches!(
        kind,
        DRIVE_REMOTE | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK
    )
}

/// Lists the Recycle Bin across all drives. Call from a background thread.
pub(crate) fn list() -> Result<Vec<RecycledItem>, crate::shell::ShellError> {
    let _com = Com::init()?;
    // SAFETY: COM is initialized on this thread; interfaces are freshly obtained and strings
    // are copied before being freed.
    unsafe {
        let bin: IShellItem =
            SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None)?;
        // An empty bin may fail to bind an enumerator rather than return an empty one.
        let Ok(items) = bin.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else {
            return Ok(Vec::new());
        };
        let mut found = Vec::new();
        loop {
            let mut slot = [None];
            let mut fetched = 0u32;
            if items.Next(&mut slot, Some(&raw mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = slot[0].take() else {
                break;
            };
            if let Some(entry) = describe(&item) {
                found.push(entry);
            }
        }
        Ok(found)
    }
}

/// Name, original location, deletion time and size of one item. Items without an original
/// location (not actually recycled) are skipped.
///
/// # Safety
/// COM must be initialized on this thread and `item` obtained on it.
unsafe fn describe(item: &IShellItem) -> Option<RecycledItem> {
    // SAFETY: guaranteed by the caller, see `# Safety`.
    unsafe {
        let now = name(item, SIGDN_DESKTOPABSOLUTEPARSING)?;
        if !in_recycle_bin(Path::new(&now)) {
            return None;
        }
        let display = name(item, SIGDN_NORMALDISPLAY)?;
        let details: IShellItem2 = item.cast().ok()?;
        let from = details.GetString(&DELETED_FROM).ok().and_then(|p| {
            let text = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            text
        })?;
        let deleted = details
            .GetFileTime(&DATE_DELETED)
            .ok()
            .and_then(system_time);
        let size = details.GetUInt64(&SIZE).unwrap_or(0);
        // Stat only the item itself, never walk into it.
        let is_dir = std::fs::symlink_metadata(&now).is_ok_and(|m| m.is_dir());
        // In the bin the display name is the full original path; keep the last component.
        let display = Path::new(&display)
            .file_name()
            .and_then(|n| n.to_str())
            .map_or_else(|| display.clone(), str::to_owned);
        Some(RecycledItem {
            name: full_name(display, Path::new(&now), is_dir),
            now,
            from: PathBuf::from(from),
            deleted,
            size,
            is_dir,
        })
    }
}

/// # Safety
/// Same as [`describe`].
unsafe fn name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    // SAFETY: the returned string is copied, then freed with CoTaskMemFree.
    unsafe {
        let p = item.GetDisplayName(kind).ok()?;
        let text = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        text
    }
}

/// With "Hide extensions" on, the display name lacks the extension. The `$R` file still has
/// it, so restore it to get the original file name.
fn full_name(display: String, now: &Path, is_dir: bool) -> String {
    if is_dir {
        return display;
    }
    let Some(ext) = now.extension().and_then(|e| e.to_str()) else {
        return display;
    };
    let suffix = format!(".{}", ext.to_lowercase());
    if display.to_lowercase().ends_with(&suffix) {
        display
    } else {
        format!("{display}.{ext}")
    }
}

/// FILETIME (100 ns ticks since 1601, UTC) to `SystemTime`.
fn system_time(ft: FILETIME) -> Option<SystemTime> {
    const UNIX_START: u64 = 116_444_736_000_000_000;
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let since_unix = ticks.checked_sub(UNIX_START)?;
    Some(UNIX_EPOCH + Duration::from_nanos(since_unix.checked_mul(100)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_paths_have_no_recycle_bin() {
        assert!(!has_recycle_bin(Path::new(r"\\localhost\C$\Users")));
        assert!(!has_recycle_bin(Path::new(r"\\wsl.localhost\Ubuntu\home")));
        assert!(!has_recycle_bin(Path::new(r"\\?\UNC\server\share\x")));
        // The system drive is a fixed disk.
        let temp = crate::win::long_temp_dir();
        assert!(has_recycle_bin(&temp), "{}", temp.display());
        assert!(has_recycle_bin(&arke_core::path::verbatim(&temp)));
        // Relative paths: the copy engine decides.
        assert!(has_recycle_bin(Path::new("relative.txt")));
    }

    #[test]
    fn hidden_extensions_are_put_back() {
        let now = Path::new("C:/$Recycle.Bin/S-1-5-21/$RAB12CD.txt");
        assert_eq!(full_name("notes".into(), now, false), "notes.txt");
        assert_eq!(full_name("notes.TXT".into(), now, false), "notes.TXT");
        // A dot in a folder name is not an extension.
        let dir = Path::new("C:/$Recycle.Bin/S-1-5-21/$RAB12CD.d");
        assert_eq!(full_name("conf.d".into(), dir, true), "conf.d");
        let bare = Path::new("C:/$Recycle.Bin/S-1-5-21/$RAB12CD");
        assert_eq!(full_name("Makefile".into(), bare, false), "Makefile");
    }

    #[test]
    fn filetime_conversion() {
        // 2023-11-14 22:13:20 UTC
        let ticks = 116_444_736_000_000_000u64 + 1_700_000_000 * 10_000_000;
        #[allow(clippy::cast_possible_truncation)]
        let ft = FILETIME {
            dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        assert_eq!(
            system_time(ft),
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        // Zero (1601) means no value.
        assert_eq!(system_time(FILETIME::default()), None);
    }
}
