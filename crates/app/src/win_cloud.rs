//! Per-item cloud sync state from the Cloud Files API (OneDrive, Dropbox, iCloud and any other
//! provider built on it).
//!
//! - Sync root: `CfGetSyncRootInfoByPath` on the folder, which also finds roots that aren't listed
//!   in the sidebar (a provider that didn't register with `StorageProviderSyncRootManager`).
//! - In sync or not: `CfGetPlaceholderInfo` on a handle opened for attributes only, with
//!   `FILE_FLAG_OPEN_REPARSE_POINT`, so the cloud filter never hydrates anything. Only local
//!   placeholders are opened; online-only items are left alone.
//!
//! One `CreateFileW` and one call per item: a 10,000-item OneDrive folder takes a fraction of a
//! second on a background thread, and the explorer caches the result per folder.

use std::path::Path;
use std::sync::Arc;

use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA};
use windows::Win32::Storage::CloudFilters::{
    CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_PLACEHOLDER_BASIC_INFO, CF_PLACEHOLDER_INFO_BASIC,
    CF_SYNC_ROOT_BASIC_INFO, CF_SYNC_ROOT_INFO_BASIC, CfGetPlaceholderInfo,
    CfGetSyncRootInfoByPath,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::{HSTRING, PCWSTR};

use crate::shell::CloudScan;

/// Most items read per folder; past that the rest show their attribute state only.
const MAX_ITEMS: usize = 20_000;

/// Whether `dir` is inside a Cloud Files sync root.
pub(crate) fn in_sync_root(dir: &Path) -> bool {
    let wide = HSTRING::from(dir.as_os_str());
    let mut info = CF_SYNC_ROOT_BASIC_INFO::default();
    #[allow(clippy::cast_possible_truncation)]
    let size = size_of::<CF_SYNC_ROOT_BASIC_INFO>() as u32;
    // SAFETY: `info` is a valid buffer of `size` bytes for the duration of the call.
    let result = unsafe {
        CfGetSyncRootInfoByPath(
            &wide,
            CF_SYNC_ROOT_INFO_BASIC,
            std::ptr::from_mut(&mut info).cast(),
            size,
            None,
        )
    };
    result.is_ok() || result.is_err_and(|e| e.code() == ERROR_MORE_DATA.to_hresult())
}

/// Whether the placeholder at `path` is marked not in sync. `None` if it can't be opened or isn't
/// a placeholder.
fn not_in_sync(path: &Path, buffer: &mut [u64]) -> Option<bool> {
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: plain CreateFileW with a NUL-terminated path; the handle is closed below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .ok()?;
    #[allow(clippy::cast_possible_truncation)]
    let len = std::mem::size_of_val(buffer) as u32;
    // SAFETY: `buffer` is u64-aligned (enough for CF_PLACEHOLDER_BASIC_INFO) and `len` bytes long.
    let result = unsafe {
        CfGetPlaceholderInfo(
            handle,
            CF_PLACEHOLDER_INFO_BASIC,
            buffer.as_mut_ptr().cast(),
            len,
            None,
        )
    };
    // SAFETY: the handle came from CreateFileW above.
    let _ = unsafe { CloseHandle(handle) };
    // The fixed part is filled even when the file identity didn't fit.
    if result.is_err_and(|e| e.code() != ERROR_MORE_DATA.to_hresult()) {
        return None;
    }
    // SAFETY: the call succeeded, so the buffer starts with a CF_PLACEHOLDER_BASIC_INFO.
    let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_BASIC_INFO>() };
    Some(info.InSyncState == CF_IN_SYNC_STATE_NOT_IN_SYNC)
}

/// See [`crate::shell::Shell::cloud_scan`].
pub(crate) fn scan(dir: &Path, items: &[Arc<str>]) -> CloudScan {
    if !in_sync_root(dir) {
        return CloudScan::default();
    }
    // Room for the fixed part and the largest file identity (4 KB).
    let mut buffer = vec![0_u64; 4096 / 8 + 8];
    let syncing = items
        .iter()
        .take(MAX_ITEMS)
        .filter(|name| not_in_sync(&dir.join(&***name), &mut buffer) == Some(true))
        .cloned()
        .collect();
    CloudScan {
        sync_root: true,
        syncing,
        not_synced: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain temp folder is no sync root and nothing in it is read as syncing.
    #[test]
    fn temp_folders_are_not_cloud() {
        let dir = std::env::temp_dir().join(format!("arke-cloud-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        assert!(!in_sync_root(&dir));
        let mut buffer = vec![0_u64; 520];
        assert_eq!(not_in_sync(&dir.join("a.txt"), &mut buffer), None);
        assert_eq!(scan(&dir, &[Arc::from("a.txt")]), CloudScan::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
