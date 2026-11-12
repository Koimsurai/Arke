//! Why a file operation failed on one item, in words the user can act on.
//!
//! Windows reports failures as Win32 error codes (`std::io::Error::raw_os_error`) or as HRESULTs
//! from the shell's copy engine (`COPYENGINE_E_*`, or a Win32 code wrapped as `0x8007xxxx`). Both
//! map to the same few reasons, so a locked file reads the same whether it was recycled or
//! deleted permanently.

use std::path::PathBuf;

use crate::path::is_same_or_inside;

/// The kind of failure, from the user's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailReason {
    /// Another program has the item open.
    InUse,
    /// No permission (the item's or its folder's security settings).
    AccessDenied,
    /// The item, or the medium it is on, is read-only.
    ReadOnly,
    /// The path is too long (for the item, or for its place in the Recycle Bin).
    PathTooLong,
    /// The item can't go to the Recycle Bin (none on that drive, or too big for it).
    NoRecycleBin,
    /// The item's name (trailing dot or space, a device name like `con`) is one the shell can't
    /// handle, so it can't go to the Recycle Bin. Deleting it permanently works.
    BadName,
    /// A folder holding items with such names; the shell gives up on the whole folder.
    BadNameInside,
    /// Not enough free space.
    DiskFull,
    /// The network location can't be reached.
    Offline,
    /// The item no longer exists. Deleting it counts as done.
    Gone,
    /// Anything else; the system message explains it.
    Other,
}

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_WRITE_PROTECT: u32 = 19;
const ERROR_SHARING_VIOLATION: u32 = 32;
const ERROR_LOCK_VIOLATION: u32 = 33;
const ERROR_HANDLE_DISK_FULL: u32 = 39;
const ERROR_BAD_NETPATH: u32 = 53;
const ERROR_NETNAME_DELETED: u32 = 64;
const ERROR_BAD_NET_NAME: u32 = 67;
const ERROR_DISK_FULL: u32 = 112;
const ERROR_BUFFER_OVERFLOW: u32 = 111;
const ERROR_BUSY: u32 = 170;
/// The file name or path is too long (Win32 error 206).
const ERROR_FILENAME_TOO_LONG: u32 = 206;
const ERROR_USER_MAPPED_FILE: u32 = 1224;
const ERROR_NETWORK_UNREACHABLE: u32 = 1231;
const ERROR_FILE_READ_ONLY: u32 = 6009;
const ERROR_CANT_ACCESS_FILE: u32 = 1920;

/// Reason for a Win32 error code (`GetLastError`, `io::Error::raw_os_error`).
#[must_use]
pub const fn from_win32(code: u32) -> FailReason {
    match code {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => FailReason::Gone,
        ERROR_ACCESS_DENIED | ERROR_CANT_ACCESS_FILE => FailReason::AccessDenied,
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_USER_MAPPED_FILE | ERROR_BUSY => {
            FailReason::InUse
        }
        ERROR_WRITE_PROTECT | ERROR_FILE_READ_ONLY => FailReason::ReadOnly,
        ERROR_FILENAME_TOO_LONG | ERROR_BUFFER_OVERFLOW => FailReason::PathTooLong,
        ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL => FailReason::DiskFull,
        ERROR_BAD_NETPATH
        | ERROR_NETNAME_DELETED
        | ERROR_BAD_NET_NAME
        | ERROR_NETWORK_UNREACHABLE => FailReason::Offline,
        _ => FailReason::Other,
    }
}

/// Reason for an HRESULT, as the copy engine reports per item.
#[must_use]
pub const fn from_hresult(hr: i32) -> FailReason {
    let hr = hr.cast_unsigned();
    // HRESULT_FROM_WIN32: facility 7.
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        return from_win32(hr & 0xFFFF);
    }
    match hr {
        // COPYENGINE_E_SHARING_VIOLATION_SRC / _DEST
        0x8027_0027 | 0x8027_0028 => FailReason::InUse,
        // COPYENGINE_E_REQUIRES_ELEVATION, _ACCESS_DENIED_SRC / _DEST
        0x8027_0002 | 0x8027_0021 | 0x8027_0022 => FailReason::AccessDenied,
        // COPYENGINE_E_ACCESSDENIED_READONLY, _DEST_IS_RO_CD / _DVD
        0x8027_003F | 0x8027_000F | 0x8027_0012 => FailReason::ReadOnly,
        // COPYENGINE_E_PATH_TOO_DEEP_SRC / _DEST, _RECYCLE_PATH_TOO_LONG, _NEWFILE / _NEWFOLDER_NAME_TOO_LONG
        0x8027_001D | 0x8027_001E | 0x8027_0038 | 0x8027_003B | 0x8027_003C => {
            FailReason::PathTooLong
        }
        // COPYENGINE_E_RECYCLE_FORCE_NUKE, _RECYCLE_SIZE_TOO_BIG, _RECYCLE_BIN_NOT_FOUND
        0x8027_0036 | 0x8027_0037 | 0x8027_003A => FailReason::NoRecycleBin,
        // COPYENGINE_E_DISK_FULL, _DISK_FULL_CLEAN, _REMOVABLE_FULL
        0x8027_0032 | 0x8027_0033 | 0x8027_000E => FailReason::DiskFull,
        // COPYENGINE_E_NET_DISCONNECT_SRC / _DEST
        0x8027_0025 | 0x8027_0026 => FailReason::Offline,
        // COPYENGINE_E_PATH_NOT_FOUND_SRC
        0x8027_0023 => FailReason::Gone,
        _ => FailReason::Other,
    }
}

/// Reason for an I/O error.
#[must_use]
pub fn from_io(e: &std::io::Error) -> FailReason {
    if let Some(code) = e.raw_os_error() {
        return from_win32(code.cast_unsigned());
    }
    match e.kind() {
        std::io::ErrorKind::NotFound => FailReason::Gone,
        std::io::ErrorKind::PermissionDenied => FailReason::AccessDenied,
        std::io::ErrorKind::ReadOnlyFilesystem => FailReason::ReadOnly,
        std::io::ErrorKind::StorageFull => FailReason::DiskFull,
        _ => FailReason::Other,
    }
}

/// The selected items to try again after some failed: each one that is, or contains, a failed
/// path, in selection order. A failed path inside a selected folder retries that folder (its other
/// contents may already be gone, which is fine). Failed paths outside the selection are retried as
/// themselves.
#[must_use]
pub fn retry_targets(selected: &[PathBuf], failed: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = selected
        .iter()
        .filter(|s| failed.iter().any(|f| is_same_or_inside(f, s)))
        .cloned()
        .collect();
    for f in failed {
        if !selected.iter().any(|s| is_same_or_inside(f, s)) && !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_codes_and_wrapped_hresults_agree() {
        for (code, reason) in [
            (32, FailReason::InUse),
            (33, FailReason::InUse),
            (1224, FailReason::InUse),
            (5, FailReason::AccessDenied),
            (2, FailReason::Gone),
            (3, FailReason::Gone),
            (206, FailReason::PathTooLong),
            (19, FailReason::ReadOnly),
            (112, FailReason::DiskFull),
            (53, FailReason::Offline),
            (87, FailReason::Other),
        ] {
            assert_eq!(from_win32(code), reason, "code {code}");
            let hr = (0x8007_0000 | code).cast_signed();
            assert_eq!(from_hresult(hr), reason, "HRESULT {hr:#x}");
        }
    }

    #[test]
    fn copy_engine_errors_have_reasons() {
        let hr = |v: u32| from_hresult(v.cast_signed());
        assert_eq!(hr(0x8027_0027), FailReason::InUse);
        assert_eq!(hr(0x8027_0021), FailReason::AccessDenied);
        assert_eq!(hr(0x8027_0002), FailReason::AccessDenied);
        assert_eq!(hr(0x8027_003F), FailReason::ReadOnly);
        assert_eq!(hr(0x8027_001D), FailReason::PathTooLong);
        assert_eq!(hr(0x8027_0038), FailReason::PathTooLong);
        assert_eq!(hr(0x8027_0036), FailReason::NoRecycleBin);
        assert_eq!(hr(0x8027_0037), FailReason::NoRecycleBin);
        assert_eq!(hr(0x8027_0023), FailReason::Gone);
        // E_FAIL and unknown copy engine codes.
        assert_eq!(hr(0x8000_4005), FailReason::Other);
        assert_eq!(hr(0x8027_0099), FailReason::Other);
    }

    #[test]
    fn io_errors_use_the_os_code_first() {
        assert_eq!(
            from_io(&std::io::Error::from_raw_os_error(32)),
            FailReason::InUse
        );
        assert_eq!(
            from_io(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            FailReason::Gone
        );
        assert_eq!(from_io(&std::io::Error::other("x")), FailReason::Other);
    }

    #[test]
    fn retry_picks_the_selected_items_that_hold_failures() {
        let sel = [
            PathBuf::from(r"C:\t\a.txt"),
            PathBuf::from(r"C:\t\Folder"),
            PathBuf::from(r"C:\t\ok.txt"),
            PathBuf::from(r"C:\t\Folder2"),
        ];
        let failed = [
            PathBuf::from(r"c:\t\folder\inner\locked.txt"),
            PathBuf::from(r"C:\t\a.txt"),
            PathBuf::from(r"C:\elsewhere\x.txt"),
        ];
        assert_eq!(
            retry_targets(&sel, &failed),
            [
                PathBuf::from(r"C:\t\a.txt"),
                PathBuf::from(r"C:\t\Folder"),
                PathBuf::from(r"C:\elsewhere\x.txt"),
            ]
        );
        // "Folder2" is not inside "Folder" just because the name starts the same.
        assert_eq!(
            retry_targets(&sel, &[PathBuf::from(r"C:\t\Folder2\x")]),
            [PathBuf::from(r"C:\t\Folder2")]
        );
        assert!(retry_targets(&sel, &[]).is_empty());
    }
}
