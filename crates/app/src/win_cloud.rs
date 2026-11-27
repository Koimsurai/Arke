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
    CF_SYNC_ROOT_BASIC_INFO, CF_SYNC_ROOT_INFO_BASIC, CF_SYNC_ROOT_INFO_STANDARD,
    CF_SYNC_ROOT_STANDARD_INFO, CfGetPlaceholderInfo, CfGetSyncRootInfoByPath,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::{HSTRING, PCWSTR};

use crate::shell::CloudScan;
use crate::win_dll::system_dll;

/// The Cloud Files API. A Windows without it (a stripped install) has no cloud status: every
/// function here answers as if nothing were in the cloud, before anything calls into the DLL.
const CLDAPI: &str = "cldapi.dll";

/// Whether the Cloud Files API is there to call.
pub(crate) fn available() -> bool {
    system_dll(CLDAPI)
}

/// Most items read per folder; past that the rest show their attribute state only.
const MAX_ITEMS: usize = 20_000;

/// Whether `dir` is inside a Cloud Files sync root.
pub(crate) fn in_sync_root(dir: &Path) -> bool {
    if !available() {
        return false;
    }
    let wide = HSTRING::from(dir.as_os_str());
    let mut info = CF_SYNC_ROOT_BASIC_INFO::default();
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

/// The name of the sync client whose root `dir` is in and the root's population policy
/// (`CF_POPULATION_POLICY_PRIMARY`: 2 the cloud filter asks the client when a folder is listed, 3
/// it never does), for the log. Reads the registration only.
pub(crate) fn sync_root_provider(dir: &Path) -> Option<(String, u16)> {
    if !available() {
        return None;
    }
    let wide = HSTRING::from(dir.as_os_str());
    // Room for the fixed part and the largest sync root identity (64 KB).
    let mut buffer = vec![0_u64; (size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + 65_536) / 8 + 1];
    let len = u32::try_from(std::mem::size_of_val(buffer.as_slice())).ok()?;
    // SAFETY: `buffer` is u64-aligned (enough for CF_SYNC_ROOT_STANDARD_INFO) and `len` bytes long.
    unsafe {
        CfGetSyncRootInfoByPath(
            &wide,
            CF_SYNC_ROOT_INFO_STANDARD,
            buffer.as_mut_ptr().cast(),
            len,
            None,
        )
    }
    .ok()?;
    // SAFETY: the call succeeded, so the buffer starts with a CF_SYNC_ROOT_STANDARD_INFO.
    let info = unsafe { &*buffer.as_ptr().cast::<CF_SYNC_ROOT_STANDARD_INFO>() };
    let name = &info.ProviderName;
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some((
        String::from_utf16_lossy(&name[..end]),
        info.PopulationPolicy.Primary.0,
    ))
}

/// Whether the placeholder at `path` is marked not in sync. `None` if it can't be opened or isn't
/// a placeholder.
fn not_in_sync(path: &Path, buffer: &mut [u64]) -> Option<bool> {
    if !available() {
        return None;
    }
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
    if !available() || !in_sync_root(dir) {
        return CloudScan::default();
    }
    log_placeholder_mode();
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

/// See [`crate::shell::Shell::cloud_verify`]: asks each item itself whether it is a placeholder
/// (`CfGetPlaceholderInfo` on a handle opened for attributes only, nothing is downloaded), since
/// the attributes a listing gave can say it isn't when it is (#319).
pub(crate) fn verify(dir: &Path, plain: Vec<Arc<str>>) -> (Vec<Arc<str>>, Vec<Arc<str>>) {
    if !available() {
        return (plain, Vec::new());
    }
    let mut buffer = vec![0_u64; 4096 / 8 + 8];
    let (mut not_synced, mut syncing) = (Vec::new(), Vec::new());
    for (i, name) in plain.into_iter().enumerate() {
        // Past the limit, as before: the attributes have it.
        let state = if i < MAX_ITEMS {
            not_in_sync(&dir.join(&*name), &mut buffer)
        } else {
            None
        };
        match state {
            // Not a placeholder (or it can't be asked): not synced, as the attributes said.
            None => not_synced.push(name),
            Some(true) => syncing.push(name),
            // A placeholder in sync: nothing to show but its cloud state.
            Some(false) => {}
        }
    }
    (not_synced, syncing)
}

/// Logs this process's placeholder compatibility mode once (0 the application default, 1
/// placeholders disguised as plain files, 2 exposed), the first time a sync root is scanned: a
/// disguised placeholder lists without its reparse point attribute (#319).
fn log_placeholder_mode() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        // SAFETY: ntdll is always loaded; the function takes nothing and returns a CHAR, and is
        // only called if this Windows has it (1803 and later).
        let mode = unsafe {
            GetModuleHandleW(windows::core::w!("ntdll.dll"))
                .ok()
                .and_then(|ntdll| {
                    GetProcAddress(
                        ntdll,
                        windows::core::s!("RtlQueryProcessPlaceholderCompatibilityMode"),
                    )
                })
                .map(|query| {
                    let query = std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn() -> i8,
                    >(query);
                    query()
                })
        };
        log::info!("placeholder compatibility mode of this process: {mode:?}");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain file in a plain folder is no placeholder: it stays not synced.
    #[test]
    fn plain_files_stay_not_synced_when_verified() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let (not_synced, syncing) = verify(&dir, vec![Arc::from("a.txt"), Arc::from("gone.txt")]);
        assert_eq!(
            not_synced,
            [Arc::<str>::from("a.txt"), Arc::from("gone.txt")]
        );
        assert!(syncing.is_empty());
    }

    /// A plain temp folder is no sync root and nothing in it is read as syncing.
    #[test]
    fn temp_folders_are_not_cloud() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        assert!(!in_sync_root(&dir));
        assert_eq!(sync_root_provider(&dir), None);
        let mut buffer = vec![0_u64; 520];
        assert_eq!(not_in_sync(&dir.join("a.txt"), &mut buffer), None);
        assert_eq!(scan(&dir, &[Arc::from("a.txt")]), CloudScan::default());
    }

    /// Without `cldapi.dll` nothing reaches the Cloud Files API: no sync root, no status, and the
    /// scan is empty.
    #[test]
    fn a_missing_cloud_api_means_no_cloud_status() {
        crate::win_dll::assert_not_loaded(&[CLDAPI], "before the test");
        let _missing = crate::win_dll::pretend_missing(&[CLDAPI]);
        let dir = std::env::temp_dir();
        assert!(!available());
        assert!(!in_sync_root(&dir));
        assert_eq!(sync_root_provider(&dir), None);
        let mut buffer = vec![0_u64; 520];
        assert_eq!(not_in_sync(&dir.join("a.txt"), &mut buffer), None);
        assert_eq!(scan(&dir, &[Arc::from("a.txt")]), CloudScan::default());
        crate::win_dll::assert_not_loaded(&[CLDAPI], "after the gated calls");
    }
}
