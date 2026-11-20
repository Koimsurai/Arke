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

/// Resolves `host` with `GetAddrInfoW`. The standard library's resolver hands the name to the
/// ANSI `getaddrinfo`, which reads it in the system code page: a computer named in Chinese never
/// resolved, and its shares were reported unreachable while Windows reached them fine (#262).
pub(crate) fn resolve_wide(host: &str) -> std::io::Result<Vec<std::net::IpAddr>> {
    use windows::Win32::Networking::WinSock::{
        ADDRINFOW, AF_INET, AF_INET6, FreeAddrInfoW, GetAddrInfoW, SOCKADDR_IN, SOCKADDR_IN6,
        WSADATA, WSAStartup,
    };
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let mut data = WSADATA::default();
        // SAFETY: data is a valid out pointer. WinSock stays started for the process.
        let _ = unsafe { WSAStartup(0x0202, &raw mut data) };
    });
    let name = wide(std::ffi::OsStr::new(host));
    let mut found: *mut ADDRINFOW = std::ptr::null_mut();
    // SAFETY: name is NUL-terminated; found receives a list freed below.
    let status =
        unsafe { GetAddrInfoW(PCWSTR(name.as_ptr()), PCWSTR::null(), None, &raw mut found) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    let mut ips = Vec::new();
    let mut at = found;
    // SAFETY: walks the list GetAddrInfoW returned; each ai_addr matches its ai_family.
    unsafe {
        while let Some(info) = at.as_ref() {
            if info.ai_family == i32::from(AF_INET.0) && !info.ai_addr.is_null() {
                let addr = info.ai_addr.cast::<SOCKADDR_IN>().read_unaligned();
                ips.push(std::net::IpAddr::from(
                    addr.sin_addr.S_un.S_addr.to_ne_bytes(),
                ));
            } else if info.ai_family == i32::from(AF_INET6.0) && !info.ai_addr.is_null() {
                let addr = info.ai_addr.cast::<SOCKADDR_IN6>().read_unaligned();
                ips.push(std::net::IpAddr::from(addr.sin6_addr.u.Byte));
            }
            at = info.ai_next;
        }
        FreeAddrInfoW(Some(found));
    }
    Ok(ips)
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

    fn reconnect(&self, path: &Path) -> Option<std::io::Result<()>> {
        unavailable_drive(path).map(reconnect_drive)
    }

    fn reach(&self, server: &str) -> Reach {
        arke_core::net::tcp_reach_with(server, resolve_wide, RESOLVE_BUDGET, CONNECT_TIMEOUT)
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
            #[allow(
                clippy::cast_ptr_alignment,
                reason = "NetApiBufferAllocate memory is aligned for any record type"
            )]
            // SAFETY: NetShareEnum returned `read` consecutive SHARE_INFO_1 records at buf.
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
/// A drive Windows remembers but hasn't connected yet counts too (see [`unavailable_drive`]).
fn mapped_drive(path: &Path) -> Option<PathBuf> {
    let (letter, rest) = drive_letter(path)?;
    let (mut unc, _) = drive_target(letter)?;
    if !rest.is_empty() {
        unc.push(rest);
    }
    Some(unc)
}

/// `Z:\a\b` -> (`Z`, `a\b`).
fn drive_letter(path: &Path) -> Option<(char, &str)> {
    let s = path.to_str()?;
    let letter = s.chars().next().filter(char::is_ascii_alphabetic)?;
    let rest = s.get(1..)?.strip_prefix(':')?;
    Some((letter, rest.trim_start_matches('\\')))
}

/// Where mapped drive `letter` points, and whether Windows has it connected. A remembered drive
/// that isn't connected (no drive yet, `ERROR_CONNECTION_UNAVAIL`) counts; local drives don't.
/// No network access.
fn drive_target(letter: char) -> Option<(PathBuf, bool)> {
    /// No such drive: an unconnected remembered drive has no root yet.
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const ERROR_CONNECTION_UNAVAIL: u32 = 1201;
    let root = wide(std::ffi::OsStr::new(&format!("{letter}:\\")));
    // SAFETY: root is NUL-terminated and outlives the call; only queries the drive type.
    let connected = match unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } {
        DRIVE_REMOTE => true,
        DRIVE_NO_ROOT_DIR => false,
        _ => return None,
    };
    let local = wide(std::ffi::OsStr::new(&format!("{letter}:")));
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
    let expected = if connected {
        0
    } else {
        ERROR_CONNECTION_UNAVAIL
    };
    if result != WIN32_ERROR(expected) {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some((PathBuf::from(OsString::from_wide(&buf[..end])), connected))
}

/// The drive letter of `path` if it's on a mapped drive Windows remembers but hasn't connected:
/// after a restart, a mapped drive stays "unavailable" until something reconnects it (File
/// Explorer does when it's opened). No network access.
fn unavailable_drive(path: &Path) -> Option<char> {
    let (letter, _) = drive_letter(path)?;
    let (_, connected) = drive_target(letter)?;
    (!connected).then_some(letter)
}

/// Reconnects a remembered mapped drive the way File Explorer does, with the credentials Windows
/// has for it. No prompt: a sign-in failure comes back as an error, and Arke's own sign-in
/// applies. Touches the network; call from a background thread.
fn reconnect_drive(letter: char) -> std::io::Result<()> {
    // winnetwk.h; not in the windows crate.
    windows_core::link!("mpr.dll" "system" fn WNetRestoreSingleConnectionW(
        hwnd: HWND, device: PCWSTR, use_ui: windows::core::BOOL) -> u32);
    debug_assert!(!arke_core::fs::is_ui_thread());
    let device = wide(std::ffi::OsStr::new(&format!("{letter}:")));
    // SAFETY: device is NUL-terminated and outlives the call; no owner window, no UI.
    let result = unsafe {
        WNetRestoreSingleConnectionW(HWND::default(), PCWSTR(device.as_ptr()), false.into())
    };
    if result == 0 {
        log::info!("reconnected mapped drive {letter}:");
        Ok(())
    } else {
        log::info!("reconnecting mapped drive {letter}: failed: error {result}");
        Err(std::io::Error::from_raw_os_error(
            i32::try_from(result).unwrap_or(i32::MAX),
        ))
    }
}

/// Name of a mapped network drive as File Explorer shows it (without the trailing `(X:)`):
/// the label the user set in Explorer, or `share (\\server)`.
///
/// Reads only Windows' stored mapping and the registry, never the network; a disconnected
/// drive (`ERROR_CONNECTION_UNAVAIL`) still gets its name. Call from a background thread.
/// `None` when unknown, so the caller falls back to the generic "Network Drive".
pub(crate) fn mapped_drive_label(letter: char) -> Option<String> {
    let (target, _) = drive_target(letter)?;
    let unc = target.to_string_lossy().into_owned();
    let custom = explorer_custom_label(&unc);
    format_mapped_label(&unc, custom.as_deref())
}

/// Drive name the user set in File Explorer
/// (`HKCU\...\Explorer\MountPoints2\##server#share\_LabelFromReg`). Read-only; `None` if unset.
fn explorer_custom_label(unc: &str) -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let key = format!(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\{}",
        unc.replace('\\', "#")
    );
    let key = wide(std::ffi::OsStr::new(&key));
    let value = wide(std::ffi::OsStr::new("_LabelFromReg"));
    let mut buf = vec![0u16; 256];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: key and value are NUL-terminated; buf has `bytes` writable bytes; bytes is a valid pointer. Read-only.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut bytes),
        )
    };
    if result != WIN32_ERROR(0) {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let label = String::from_utf16_lossy(&buf[..end]);
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_owned())
}

/// Builds the File Explorer drive name from `\\server\share[\sub]` and the user's custom label.
/// The custom label wins; otherwise `last-component (\\parent)`, so a share root gives
/// `share (\\server)`. `None` if `unc` is not a UNC path.
fn format_mapped_label(unc: &str, custom: Option<&str>) -> Option<String> {
    if let Some(custom) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        return Some(custom.to_owned());
    }
    let rest = unc.strip_prefix(r"\\")?.trim_end_matches('\\');
    let (head, name) = rest.rsplit_once('\\')?;
    if head.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!(r"{name} (\\{head})"))
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
    /// The wide-character resolver finds this computer by name, including one that isn't
    /// ASCII the way the probe would see it (#262).
    #[test]
    fn resolves_names_with_the_wide_api() {
        let local = super::resolve_wide("localhost").expect("localhost resolves");
        assert!(local.iter().any(std::net::IpAddr::is_loopback), "{local:?}");
        let me = std::env::var("COMPUTERNAME").expect("computer name");
        assert!(
            !super::resolve_wide(&me)
                .expect("this computer resolves")
                .is_empty()
        );
        assert!(super::resolve_wide("arke-test.invalid").is_err());
    }

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

    #[test]
    fn mapped_label_matches_file_explorer() {
        assert_eq!(
            format_mapped_label(r"\\10.16.20.142\dad", None).as_deref(),
            Some(r"dad (\\10.16.20.142)")
        );
        assert_eq!(
            format_mapped_label(r"\\nas\share\sub\", None).as_deref(),
            Some(r"sub (\\nas\share)")
        );
        assert_eq!(
            format_mapped_label(r"\\nas\share", Some("  Photos ")).as_deref(),
            Some("Photos")
        );
        // blank custom label falls back to the share name
        assert_eq!(
            format_mapped_label(r"\\nas\share", Some(" ")).as_deref(),
            Some(r"share (\\nas)")
        );
        assert_eq!(format_mapped_label(r"\\nas", None), None);
        assert_eq!(format_mapped_label("", None), None);
        assert_eq!(format_mapped_label(r"C:\x", None), None);
    }

    #[test]
    fn unmapped_drive_returns_none_quickly() {
        // Q: is almost never mapped; the call must not touch the network or hang.
        let start = std::time::Instant::now();
        let _ = mapped_drive_label('Q');
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
