//! Network locations (SMB): is the server reachable, and what kind of error was it.
//!
//! The hard part is slow and disconnected servers, not I/O. When a server is asleep or the
//! VPN drops, listing only fails after the SMB timeout (often 20-60s). So while listing a
//! network folder we also try TCP 445 on the server: if it is unreachable we say so right
//! away. If it connects, or we can't tell (slow name resolution), we wait for the listing.
//!
//! Only connects when a network folder is opened; nothing touches the network at startup.

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::Duration;

use rust_i18n::t;

use crate::entry::{Entry, EntryKind};

/// Whether the server is reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Reachable.
    Up,
    /// Name does not resolve.
    NoSuchHost,
    /// Name resolves but connecting fails.
    Down,
    /// Can't tell (name resolution too slow etc.): wait for the listing as usual.
    Unknown,
}

/// Probes network locations. Implemented in the app, since mapped drives need Windows to
/// tell which share they point to.
pub trait NetProbe: Send + Sync + 'static {
    /// UNC path (`\\server\share\...`) of a network location; `None` for local paths.
    /// No network access, safe on a background thread.
    fn unc(&self, path: &Path) -> Option<PathBuf>;
    /// Whether the server is reachable (touches the network, up to a few seconds).
    fn reach(&self, server: &str) -> Reach;
    /// The shares `server` offers, as it reports them (`\\server` is listed with
    /// [`share_entries`]). Touches the network; call from a background thread.
    fn shares(&self, server: &str) -> io::Result<Vec<Share>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("listing the shares of {server}"),
        ))
    }
}

/// One share a server offers, as `NetShareEnum` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// Share name (`docs`, `C$`).
    pub name: String,
    /// `STYPE_*` bits: the low byte is the type, the high bits flag special (administrative) and
    /// temporary shares.
    pub kind: u32,
}

/// Disk share (the type in the low byte of [`Share::kind`]).
pub const STYPE_DISKTREE: u32 = 0;
/// Mask for the type part of [`Share::kind`].
const STYPE_MASK: u32 = 0xFF;
/// Folder attribute bit.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// Hidden attribute bit.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

/// The server of a bare server path, `\\server` or `\\server\` (no share). `None` for anything
/// else, including device paths (`\\?\`, `\\.\`). String handling only.
pub fn bare_server(path: &Path) -> Option<&str> {
    let rest = path.to_str()?.strip_prefix(r"\\")?;
    let server = rest.strip_suffix('\\').unwrap_or(rest);
    let valid =
        !server.is_empty() && !server.contains(['\\', '/']) && server != "?" && server != ".";
    valid.then_some(server)
}

/// `\\server\share` itself (with or without the trailing backslash), not a folder inside it.
pub fn is_share_root(path: &Path) -> bool {
    let mut components = path.components();
    let unc = matches!(
        components.next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::UNC(..))
    );
    unc && components.all(|c| c == Component::RootDir)
}

/// The server of a bare server path whose shares can be listed with `NetShareEnum`: not WSL
/// (Windows lists its distros like a folder) and not WebDAV (`server@SSL`).
pub fn share_server(path: &Path) -> Option<&str> {
    let server = bare_server(path)?;
    let lower = server.to_ascii_lowercase();
    let listed_as_folder = lower == "wsl$" || lower == "wsl.localhost" || lower.contains('@');
    (!listed_as_folder).then_some(server)
}

/// The shares of a server shown as folders, like Explorer: disk shares only (no printers, IPC or
/// devices). Administrative and other `$` shares (`C$`, `ADMIN$`) are marked hidden, so they only
/// show with "Hidden items" on.
pub fn share_entries(shares: &[Share]) -> Vec<Entry> {
    shares
        .iter()
        .filter(|s| s.kind & STYPE_MASK == STYPE_DISKTREE && !s.name.is_empty())
        .map(|s| {
            let hidden = if s.name.ends_with('$') {
                FILE_ATTRIBUTE_HIDDEN
            } else {
                0
            };
            Entry::new(
                s.name.as_str(),
                EntryKind::Dir,
                0,
                None,
                FILE_ATTRIBUTE_DIRECTORY | hidden,
            )
        })
        .collect()
}

/// Kind of listing failure (decides which button the UI offers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// Any other error (not found, access denied...): no special action.
    Other,
    /// Server unreachable: offer "Retry".
    Offline,
    /// Needs sign-in: offer "Sign in..." (Windows credential dialog).
    NeedsLogin,
}

/// An error that already knows how to present itself: remote locations (SFTP, WebDAV) wrap their
/// failures in this so the directory service shows the right message and button without knowing
/// the protocol. Travels inside an `io::Error`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LocationError {
    /// Localized message for display.
    pub message: String,
    /// Which action the UI offers.
    pub kind: FailKind,
}

impl LocationError {
    /// Wraps into an `io::Error` with the given kind (callers matching on `ErrorKind` still work).
    pub fn into_io(self, kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, self)
    }

    /// The `LocationError` inside an `io::Error`, if any.
    pub fn of(err: &io::Error) -> Option<&Self> {
        err.get_ref()?.downcast_ref::<Self>()
    }
}

/// Server name of `\\server\share\...` (or a bare `\\server`). WSL, WebDAV and local paths are not
/// SMB servers and are not probed.
pub fn server_of(unc: &Path) -> Option<String> {
    let server = if let Some(server) = bare_server(unc) {
        server
    } else {
        let Some(Component::Prefix(prefix)) = unc.components().next() else {
            return None;
        };
        match prefix.kind() {
            Prefix::UNC(server, _) | Prefix::VerbatimUNC(server, _) => server.to_str()?,
            _ => return None,
        }
    };
    let lower = server.to_ascii_lowercase();
    let special = [
        "wsl$",
        "wsl.localhost",
        "localhost",
        ".",
        "?",
        "127.0.0.1",
        "::1",
    ];
    if special.contains(&lower.as_str()) || lower.contains('@') || lower.is_empty() {
        return None;
    }
    Some(server.to_string())
}

/// `\\server\share\a\b` -> `\\server\share` (sign-in targets the share itself).
pub fn share_root(unc: &Path) -> Option<PathBuf> {
    let Some(Component::Prefix(prefix)) = unc.components().next() else {
        return None;
    };
    match prefix.kind() {
        Prefix::UNC(server, share) => {
            let mut s = std::ffi::OsString::from(r"\\");
            s.push(server);
            s.push(r"\");
            s.push(share);
            Some(PathBuf::from(s))
        }
        _ => None,
    }
}

/// Tries the SMB port on `server`. Name resolution gets at most `resolve_budget`, each
/// address at most `connect_timeout`.
pub fn tcp_reach(server: &str, resolve_budget: Duration, connect_timeout: Duration) -> Reach {
    // Name resolution can hang (NetBIOS/LLMNR), so it runs on another thread; past the budget we
    // report Unknown.
    let (tx, rx) = flume::bounded(1);
    let host = server.to_string();
    let spawned = std::thread::Builder::new()
        .name("net-resolve".into())
        .spawn(move || {
            let addrs: io::Result<Vec<SocketAddr>> = (host.as_str(), 445)
                .to_socket_addrs()
                .map(Iterator::collect);
            let _ = tx.send(addrs);
        });
    if spawned.is_err() {
        return Reach::Unknown;
    }
    let addrs = match rx.recv_timeout(resolve_budget) {
        Ok(Ok(addrs)) if !addrs.is_empty() => addrs,
        Ok(_) => return Reach::NoSuchHost,
        Err(_) => return Reach::Unknown,
    };
    // Try 445 and 139 on every address at once (old servers may only have NetBIOS); any
    // success counts. If all fail we wait at most one `connect_timeout`.
    let targets: Vec<SocketAddr> = addrs
        .iter()
        .take(4)
        .flat_map(|a| [445, 139].map(|port| SocketAddr::new(a.ip(), port)))
        .collect();
    let (tx, rx) = flume::unbounded::<bool>();
    let mut started = 0;
    for target in targets {
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("net-probe".into())
            .spawn(move || {
                let _ = tx.send(TcpStream::connect_timeout(&target, connect_timeout).is_ok());
            });
        if spawned.is_ok() {
            started += 1;
        }
    }
    drop(tx);
    let deadline = std::time::Instant::now() + connect_timeout + Duration::from_millis(200);
    for _ in 0..started {
        match rx.recv_deadline(deadline) {
            Ok(true) => return Reach::Up,
            Ok(false) => {}
            Err(_) => break,
        }
    }
    Reach::Down
}

/// Turns a listing error into a user-facing message and the button to offer.
/// `network`: whether the location is remote (access denied there usually means sign-in).
pub fn describe(err: &io::Error, network: bool) -> (String, FailKind) {
    // Windows error codes (https://learn.microsoft.com/windows/win32/debug/system-error-codes)
    const ACCESS_DENIED: i32 = 5;
    const BAD_NETPATH: i32 = 53;
    const UNEXP_NET_ERR: i32 = 59;
    const NETNAME_DELETED: i32 = 64;
    const BAD_NET_NAME: i32 = 67;
    const INVALID_PASSWORD: i32 = 86;
    const SEM_TIMEOUT: i32 = 121;
    const NO_NET_OR_BAD_PATH: i32 = 1203;
    const SESSION_CREDENTIAL_CONFLICT: i32 = 1219;
    const NO_NETWORK: i32 = 1222;
    const NETWORK_UNREACHABLE: i32 = 1231;
    const HOST_UNREACHABLE: i32 = 1232;
    const NOT_AUTHENTICATED: i32 = 1244;
    const LOGON_FAILURE: i32 = 1326;
    const ACCOUNT_RESTRICTION: i32 = 1327;
    const PASSWORD_EXPIRED: i32 = 1330;
    const ACCOUNT_DISABLED: i32 = 1331;
    const ACCOUNT_LOCKED_OUT: i32 = 1909;
    // Listing a server's shares goes over RPC (`NetShareEnum`).
    const RPC_SERVER_UNAVAILABLE: i32 = 1722;
    const RPC_CALL_FAILED: i32 = 1726;
    // Remote locations (SFTP, WebDAV) already know their message and button.
    if let Some(located) = LocationError::of(err) {
        return (located.message.clone(), located.kind);
    }
    match err.raw_os_error() {
        Some(BAD_NETPATH) => (t!("io.net_not_found").into_owned(), FailKind::Offline),
        Some(
            UNEXP_NET_ERR
            | NETNAME_DELETED
            | SEM_TIMEOUT
            | NO_NET_OR_BAD_PATH
            | NO_NETWORK
            | NETWORK_UNREACHABLE
            | HOST_UNREACHABLE
            | RPC_SERVER_UNAVAILABLE
            | RPC_CALL_FAILED,
        ) => (t!("io.net_offline").into_owned(), FailKind::Offline),
        Some(BAD_NET_NAME) => (t!("io.net_no_share").into_owned(), FailKind::Other),
        Some(SESSION_CREDENTIAL_CONFLICT) => {
            (t!("io.net_other_account").into_owned(), FailKind::Other)
        }
        Some(LOGON_FAILURE | INVALID_PASSWORD | NOT_AUTHENTICATED) => {
            (t!("io.net_login").into_owned(), FailKind::NeedsLogin)
        }
        Some(ACCOUNT_RESTRICTION | PASSWORD_EXPIRED | ACCOUNT_DISABLED | ACCOUNT_LOCKED_OUT) => {
            (t!("io.net_account").into_owned(), FailKind::NeedsLogin)
        }
        Some(ACCESS_DENIED) if network => (t!("io.net_denied").into_owned(), FailKind::NeedsLogin),
        _ => (crate::service::describe_io_error(err), FailKind::Other),
    }
}

/// Probe result -> error message (only when definitely unreachable).
pub fn describe_reach(reach: Reach, server: &str) -> Option<String> {
    match reach {
        Reach::NoSuchHost => Some(t!("io.net_no_host", server = server).into_owned()),
        Reach::Down => Some(t!("io.net_down", server = server).into_owned()),
        Reach::Up | Reach::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_server_of_unc_paths() {
        assert_eq!(
            server_of(Path::new(r"\\nas\share\docs")),
            Some("nas".into())
        );
        assert_eq!(
            server_of(Path::new(r"\\10.0.0.5\x")),
            Some("10.0.0.5".into())
        );
        assert_eq!(
            server_of(Path::new(r"\\?\UNC\nas\share\a")),
            Some("nas".into())
        );
        assert_eq!(server_of(Path::new(r"C:\Users")), None);
        assert_eq!(
            server_of(Path::new(r"\\wsl.localhost\Ubuntu\home")),
            None,
            "WSL is not SMB"
        );
        assert_eq!(server_of(Path::new(r"\\wsl$\Ubuntu")), None);
        assert_eq!(server_of(Path::new(r"\\localhost\c$")), None);
        assert_eq!(
            server_of(Path::new(r"\\server@SSL\DavWWWRoot")),
            None,
            "WebDAV"
        );
        assert_eq!(
            share_root(Path::new(r"\\nas\share\a\b")),
            Some(PathBuf::from(r"\\nas\share"))
        );
        assert_eq!(share_root(Path::new(r"C:\a")), None);
    }

    #[test]
    fn bare_server_paths_are_recognized() {
        assert_eq!(bare_server(Path::new(r"\\192.0.2.10")), Some("192.0.2.10"));
        assert_eq!(bare_server(Path::new(r"\\nas\")), Some("nas"));
        assert_eq!(bare_server(Path::new(r"\\nas\share")), None);
        assert_eq!(bare_server(Path::new(r"\\nas\share\")), None);
        assert_eq!(bare_server(Path::new(r"\\")), None);
        assert_eq!(bare_server(Path::new(r"\\?\C:\x")), None);
        assert_eq!(bare_server(Path::new(r"\\?")), None, "device namespace");
        assert_eq!(bare_server(Path::new(r"\\.")), None, "device namespace");
        assert_eq!(bare_server(Path::new(r"C:\nas")), None);
        assert_eq!(bare_server(Path::new("sftp://h")), None);
        assert_eq!(share_server(Path::new(r"\\nas")), Some("nas"));
        assert_eq!(share_server(Path::new(r"\\localhost")), Some("localhost"));
        assert_eq!(
            share_server(Path::new(r"\\wsl$")),
            None,
            "WSL lists its distros"
        );
        assert_eq!(share_server(Path::new(r"\\wsl.localhost\")), None);
        assert_eq!(share_server(Path::new(r"\\nas@SSL")), None, "WebDAV");
        assert_eq!(server_of(Path::new(r"\\nas")), Some("nas".into()));
        assert_eq!(server_of(Path::new(r"\\localhost")), None, "not probed");
        assert!(is_share_root(Path::new(r"\\nas\team")));
        assert!(is_share_root(Path::new(r"\\nas\team\")));
        assert!(is_share_root(&PathBuf::from(r"\\nas").join("team")));
        assert!(!is_share_root(Path::new(r"\\nas\team\docs")));
        assert!(!is_share_root(Path::new(r"\\nas")));
        assert!(!is_share_root(Path::new(r"C:\")));
    }

    #[test]
    fn only_disk_shares_are_listed_and_dollar_shares_are_hidden() {
        const STYPE_PRINTQ: u32 = 1;
        const STYPE_IPC: u32 = 3;
        const STYPE_SPECIAL: u32 = 0x8000_0000;
        let share = |name: &str, kind: u32| Share {
            name: name.into(),
            kind,
        };
        let entries = share_entries(&[
            share("team", STYPE_DISKTREE),
            share("報告 — 2026", STYPE_DISKTREE),
            share("C$", STYPE_DISKTREE | STYPE_SPECIAL),
            share("ADMIN$", STYPE_DISKTREE | STYPE_SPECIAL),
            share("IPC$", STYPE_IPC | STYPE_SPECIAL),
            share("Printer", STYPE_PRINTQ),
            share("", STYPE_DISKTREE),
        ]);
        let names: Vec<&str> = entries.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["team", "報告 — 2026", "C$", "ADMIN$"]);
        assert!(entries.iter().all(|e| e.kind == EntryKind::Dir));
        let shown = |show_hidden: bool| -> Vec<&str> {
            entries
                .iter()
                .filter(|e| !e.concealed_with(show_hidden, false, false))
                .map(|e| &*e.name)
                .collect()
        };
        assert_eq!(shown(false), ["team", "報告 — 2026"]);
        assert_eq!(
            shown(true),
            ["team", "報告 — 2026", "C$", "ADMIN$"],
            "administrative shares show with hidden items"
        );
    }

    #[test]
    fn network_errors_get_the_right_button() {
        let kind =
            |code: i32, network: bool| describe(&io::Error::from_raw_os_error(code), network).1;
        assert_eq!(kind(53, true), FailKind::Offline);
        assert_eq!(kind(64, true), FailKind::Offline, "connection dropped");
        assert_eq!(kind(121, true), FailKind::Offline, "timeout");
        assert_eq!(kind(1326, true), FailKind::NeedsLogin, "bad credentials");
        assert_eq!(kind(5, true), FailKind::NeedsLogin, "remote access denied");
        assert_eq!(kind(5, false), FailKind::Other, "local: no permission");
        assert_eq!(kind(67, true), FailKind::Other, "no such share");
        assert_eq!(kind(1219, true), FailKind::Other, "other user connected");
        assert_eq!(kind(1722, true), FailKind::Offline, "share list: no RPC");
        assert_eq!(kind(2, false), FailKind::Other);
        let remote = LocationError {
            message: "host key changed".into(),
            kind: FailKind::NeedsLogin,
        }
        .into_io(io::ErrorKind::PermissionDenied);
        assert_eq!(
            describe(&remote, true),
            ("host key changed".into(), FailKind::NeedsLogin)
        );
    }

    #[test]
    fn unreachable_hosts_are_reported_quickly() {
        // Nothing listens on this local port: refused immediately.
        let started = std::time::Instant::now();
        let reach = tcp_reach(
            "127.0.0.2",
            Duration::from_secs(2),
            Duration::from_millis(500),
        );
        // 127.0.0.2 may have a service in some environments; only require no hang and not
        // NoSuchHost.
        assert_ne!(reach, Reach::NoSuchHost);
        assert!(started.elapsed() < Duration::from_secs(3));
        // `.invalid` is reserved to never resolve.
        assert_eq!(
            tcp_reach(
                "arke-test.invalid",
                Duration::from_secs(5),
                Duration::from_millis(500)
            ),
            Reach::NoSuchHost
        );
    }
}
