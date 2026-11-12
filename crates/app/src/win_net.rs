//! Windows side of network (SMB) locations: mapped drive targets, server reachability, sign-in.
//!
//! - Credentials come from Arke's own dialog (`net_dialog.rs`) and go to `WNetAddConnection2W`.
//!   The password is used for that one call, then zeroed; never logged or stored. Only
//!   "Remember" hands it to Windows Credential Manager. No `CONNECT_UPDATE_PROFILE`, which
//!   would persist the connection in the registry.
//! - The network is touched only when a network folder is opened or sign-in is requested.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rust_i18n::t;
use std::time::Duration;

use arke_core::net::{NetProbe, Reach, Share};
use windows::Win32::Foundation::{HWND, WIN32_ERROR};
use windows::Win32::NetworkManagement::WNet::{
    NETRESOURCEW, RESOURCETYPE_DISK, WNetConnectionDialog, WNetDisconnectDialog, WNetGetConnectionW,
};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::core::{PCWSTR, PWSTR};

const DRIVE_REMOTE: u32 = 4;

/// Why connecting to, disconnecting or mapping a network share failed. Display is the
/// localized reason shown in Arke's dialog and notices.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NetError {
    /// Access denied, wrong password, logon failure or unknown user name.
    #[error("{}", t!("net.bad_login"))]
    BadLogin,
    /// Already connected to this server with different credentials.
    #[error("{}", t!("net.conflict"))]
    Conflict,
    /// The network path or share name was not found.
    #[error("{}", t!("net.not_found"))]
    NotFound,
    /// The drive letter is already in use.
    #[error("{}", t!("net.letter_used"))]
    LetterUsed,
    /// Any other Windows networking error; shows the system message.
    #[error("{}", .0.message())]
    Windows(#[source] windows::core::Error),
}

impl NetError {
    /// Maps a WNet result code to an error.
    fn from_code(code: WIN32_ERROR) -> Self {
        match code.0 {
            // Access denied, bad password, logon failure, bad user name.
            5 | 86 | 1326 | 2202 => Self::BadLogin,
            // Already connected to this server with different credentials.
            1219 => Self::Conflict,
            // Network path or share name not found.
            53 | 67 => Self::NotFound,
            // Drive letter already in use.
            85 => Self::LetterUsed,
            _ => Self::Windows(windows::core::Error::from_hresult(code.to_hresult())),
        }
    }
}
/// Name resolution budget; past this, fall through to the normal directory listing wait.
const RESOLVE_BUDGET: Duration = Duration::from_secs(3);
/// Connect timeout per address.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) struct WinNetProbe;

impl NetProbe for WinNetProbe {
    fn unc(&self, path: &Path) -> Option<PathBuf> {
        let s = path.as_os_str().to_string_lossy();
        if s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\")) {
            return Some(path.to_path_buf());
        }
        mapped_drive(path)
    }

    fn reach(&self, server: &str) -> Reach {
        arke_core::net::tcp_reach(server, RESOLVE_BUDGET, CONNECT_TIMEOUT)
    }

    fn shares(&self, server: &str) -> std::io::Result<Vec<Share>> {
        server_shares(server)
    }
}

/// The shares `server` offers (`NetShareEnum`, level 1: name and type). Runs as the current user,
/// so it fails with access denied until signed in to a server that wants credentials.
/// Touches the network; call from a background thread.
fn server_shares(server: &str) -> std::io::Result<Vec<Share>> {
    use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
    use windows::Win32::Storage::FileSystem::{NetShareEnum, SHARE_INFO_1};
    /// "Return all available data" for `prefmaxlen`.
    const MAX_PREFERRED_LENGTH: u32 = u32::MAX;
    const NERR_SUCCESS: u32 = 0;
    const ERROR_MORE_DATA: u32 = 234;
    debug_assert!(!arke_core::fs::is_ui_thread());
    let name = wide(std::ffi::OsStr::new(&format!(r"\\{server}")));
    let mut shares = Vec::new();
    let mut resume = 0u32;
    loop {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut read = 0u32;
        let mut total = 0u32;
        // SAFETY: name is NUL-terminated and outlives the call; the out pointers are valid. On
        // success (or ERROR_MORE_DATA) buf holds `read` SHARE_INFO_1 records, freed below.
        let status = unsafe {
            NetShareEnum(
                PCWSTR(name.as_ptr()),
                1,
                &raw mut buf,
                MAX_PREFERRED_LENGTH,
                &raw mut read,
                &raw mut total,
                Some(&raw mut resume),
            )
        };
        if status != NERR_SUCCESS && status != ERROR_MORE_DATA {
            if !buf.is_null() {
                // SAFETY: buf was allocated by NetShareEnum.
                unsafe { NetApiBufferFree(Some(buf.cast())) };
            }
            log::info!("listing the shares of {server} failed: error {status}");
            return Err(std::io::Error::from_raw_os_error(
                i32::try_from(status).unwrap_or(i32::MAX),
            ));
        }
        if !buf.is_null() {
            // SAFETY: NetShareEnum returned `read` consecutive SHARE_INFO_1 records at buf.
            // NetApiBufferAllocate memory is aligned for any record type.
            #[allow(clippy::cast_ptr_alignment)]
            let records =
                unsafe { std::slice::from_raw_parts(buf.cast::<SHARE_INFO_1>(), read as usize) };
            for record in records {
                if record.shi1_netname.is_null() {
                    continue;
                }
                // SAFETY: shi1_netname is a NUL-terminated string inside buf.
                let name = unsafe { record.shi1_netname.as_wide() };
                shares.push(Share {
                    name: String::from_utf16_lossy(name),
                    kind: record.shi1_type.0,
                });
            }
            // SAFETY: buf was allocated by NetShareEnum and is not used after this.
            unsafe { NetApiBufferFree(Some(buf.cast())) };
        }
        if status == NERR_SUCCESS || read == 0 {
            return Ok(shares);
        }
    }
}

/// Mapped drive path (`Z:\a`) -> `\\server\share\a`. Reads Windows' mapping only; no network access.
fn mapped_drive(path: &Path) -> Option<PathBuf> {
    let s = path.to_str()?;
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let root = wide(std::ffi::OsStr::new(&format!("{}:\\", &s[..1])));
    // SAFETY: root is NUL-terminated and outlives the call; only queries the drive type.
    if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != DRIVE_REMOTE {
        return None;
    }
    let local = wide(std::ffi::OsStr::new(&s[..2]));
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: local is NUL-terminated; buf holds at least len u16s; len is a valid pointer.
    let result = unsafe {
        WNetGetConnectionW(
            PCWSTR(local.as_ptr()),
            Some(PWSTR(buf.as_mut_ptr())),
            &raw mut len,
        )
    };
    if result != WIN32_ERROR(0) {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let mut unc = PathBuf::from(OsString::from_wide(&buf[..end]));
    let rest = s[2..].trim_start_matches('\\');
    if !rest.is_empty() {
        unc.push(rest);
    }
    Some(unc)
}

/// Connects to a share, optionally mapped to a drive letter. Without credentials the current
/// identity is used. No `CONNECT_UPDATE_PROFILE`, so the connection lasts only for this
/// logon session. Call from a background thread.
pub(crate) fn connect(request: &crate::shell::NetConnect) -> Result<(), NetError> {
    use windows::Win32::NetworkManagement::WNet::{NET_CONNECT_FLAGS, WNetAddConnection2W};
    // Signing in to a server itself (to list its shares) goes through its IPC$ share, like
    // Explorer does.
    let server = arke_core::net::bare_server(&request.share);
    let target = server.map_or_else(
        || request.share.clone(),
        |server| PathBuf::from(format!(r"\\{server}\IPC$")),
    );
    let mut remote = wide(target.as_os_str());
    let mut local = request
        .drive
        .map(|letter| wide(std::ffi::OsStr::new(&format!("{letter}:"))));
    let resource = NETRESOURCEW {
        // IPC$ is not a disk share.
        dwType: if server.is_some() {
            windows::Win32::NetworkManagement::WNet::RESOURCETYPE_ANY
        } else {
            RESOURCETYPE_DISK
        },
        lpRemoteName: PWSTR(remote.as_mut_ptr()),
        lpLocalName: local
            .as_mut()
            .map_or(PWSTR::null(), |l| PWSTR(l.as_mut_ptr())),
        ..Default::default()
    };
    let user = request
        .credentials
        .as_ref()
        .map(|(user, _)| wide(std::ffi::OsStr::new(user)));
    let mut password = request
        .credentials
        .as_ref()
        .map(|(_, password)| wide(std::ffi::OsStr::new(password)));
    // SAFETY: resource and all strings are valid for the call; null user/password means the
    // current identity.
    let result = unsafe {
        WNetAddConnection2W(
            &raw const resource,
            password
                .as_ref()
                .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            user.as_ref().map_or(PCWSTR::null(), |u| PCWSTR(u.as_ptr())),
            NET_CONNECT_FLAGS(0),
        )
    };
    // Zero the password right away instead of leaving it for the allocator.
    if let Some(password) = password.as_mut() {
        password.fill(0);
    }
    if result != WIN32_ERROR(0) {
        log::info!(
            "connecting to {} failed: error {}",
            request.share.display(),
            result.0
        );
        return Err(NetError::from_code(result));
    }
    if request.remember
        && let Some((user, password)) = &request.credentials
    {
        remember(&request.share, user, password);
    }
    Ok(())
}

/// Saves credentials in Windows Credential Manager, like "Remember my credentials" in
/// Windows' own prompt. Failure only gets logged; the connection still counts as successful.
fn remember(share: &Path, user: &str, password: &str) {
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_ENTERPRISE, CRED_TYPE_DOMAIN_PASSWORD, CREDENTIALW, CredWriteW,
    };
    let share = share.to_string_lossy();
    let Some(server) = share
        .trim_start_matches('\\')
        .split('\\')
        .next()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let mut target = wide(std::ffi::OsStr::new(server));
    let mut name = wide(std::ffi::OsStr::new(user));
    let mut blob: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_DOMAIN_PASSWORD,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: u32::try_from(blob.len()).unwrap_or(0),
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_ENTERPRISE,
        UserName: PWSTR(name.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: the buffers credential points to are valid for the call.
    if let Err(err) = unsafe { CredWriteW(&raw const credential, 0) } {
        log::warn!("Credential Manager did not save credentials for {server}: {err}");
    }
    blob.fill(0);
}

/// Disconnects a mapped drive (`Z:\`). Windows refuses while files are open; not forced.
/// Call from a background thread.
pub(crate) fn disconnect(drive: &Path) -> Result<(), NetError> {
    use windows::Win32::NetworkManagement::WNet::{NET_CONNECT_FLAGS, WNetCancelConnection2W};
    let text = drive.to_string_lossy();
    let name = wide(std::ffi::OsStr::new(text.trim_end_matches('\\')));
    // SAFETY: name is a NUL-terminated wide string. Flags 0 leave the reconnect-at-logon
    // setting alone; force is false.
    let result =
        unsafe { WNetCancelConnection2W(PCWSTR(name.as_ptr()), NET_CONNECT_FLAGS(0), false) };
    if result == WIN32_ERROR(0) {
        Ok(())
    } else {
        Err(NetError::Windows(windows::core::Error::from_hresult(
            result.to_hresult(),
        )))
    }
}

/// Shows Windows' "Map network drive" (`map`) or "Disconnect network drive" dialog; options
/// like reconnect at logon are chosen there. Returns `true` if a drive was mapped or
/// disconnected, `false` on cancel. Call from a background thread.
pub(crate) fn drive_dialog(owner: Option<isize>, map: bool) -> Result<bool, NetError> {
    debug_assert!(!arke_core::fs::is_ui_thread());
    let owner = owner.map_or_else(HWND::default, |h| HWND(h as *mut _));
    // SAFETY: owner is the Arke window or null; the call returns once the dialog closes.
    let result = unsafe {
        if map {
            WNetConnectionDialog(owner, RESOURCETYPE_DISK.0)
        } else {
            WNetDisconnectDialog(Some(owner), RESOURCETYPE_DISK.0)
        }
    };
    match result.0 {
        0 => Ok(true),
        // Cancelled (-1).
        u32::MAX => Ok(false),
        _ => Err(NetError::Windows(windows::core::Error::from_hresult(
            result.to_hresult(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_are_network_and_local_drives_are_not() {
        let probe = WinNetProbe;
        assert_eq!(
            probe.unc(Path::new(r"\\nas\share\a")),
            Some(PathBuf::from(r"\\nas\share\a"))
        );
        assert!(probe.unc(Path::new(r"\\?\UNC\nas\share")).is_some());
        assert_eq!(probe.unc(Path::new(r"\\?\C:\Windows")), None);
        assert_eq!(probe.unc(Path::new(r"C:\Windows")), None);
        assert_eq!(probe.unc(Path::new("relative")), None);
    }
}
