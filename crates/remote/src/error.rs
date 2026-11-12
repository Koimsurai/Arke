//! Remote failures and how they look to the user.

use std::io;

use arke_core::net::{FailKind, LocationError};
use rust_i18n::t;

/// A server's host key as seen during connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKey {
    /// `host` or `[host]:port`, as written in known_hosts.
    pub host: String,
    /// Key type, e.g. `ssh-ed25519`.
    pub algorithm: String,
    /// `SHA256:...`, as OpenSSH prints it.
    pub fingerprint: String,
    /// Base64 key data (the second field of a known_hosts line).
    pub key: String,
}

/// Why a remote operation failed.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The host name does not resolve.
    #[error("{}", t!("remote.no_host", host = .0))]
    NoSuchHost(String),
    /// Connecting failed (refused, unreachable).
    #[error("{}", t!("remote.unreachable", host = .0))]
    Unreachable(String),
    /// No answer in time.
    #[error("{}", t!("remote.timeout"))]
    Timeout,
    /// The connection dropped mid-way.
    #[error("{}", t!("remote.lost"))]
    Lost,
    /// The server wants credentials and none are known.
    #[error("{}", t!("remote.needs_login"))]
    NeedsLogin,
    /// The server rejected the credentials.
    #[error("{}", t!("remote.login_failed"))]
    LoginFailed,
    /// First connection to this server: its key must be confirmed.
    #[error("{}", t!("remote.host_unknown", fingerprint = .0.fingerprint))]
    HostKeyUnknown(HostKey),
    /// The server's key is not the one seen before: possibly an attack.
    #[error("{}", t!("remote.host_changed", host = .0.host))]
    HostKeyChanged(HostKey),
    /// Credentials would go out unencrypted (Basic auth over plain HTTP).
    #[error("{}", t!("remote.insecure"))]
    Insecure,
    /// The HTTPS certificate is not trusted by Windows.
    #[error("{}", t!("remote.cert"))]
    Certificate,
    /// The key file could not be read or is not a supported private key.
    #[error("{}", t!("remote.bad_key", reason = .0))]
    BadKey(String),
    /// No such file or folder.
    #[error("{}", t!("remote.not_found"))]
    NotFound,
    /// No permission.
    #[error("{}", t!("remote.denied"))]
    Denied,
    /// An item with that name already exists.
    #[error("{}", t!("remote.exists"))]
    Exists,
    /// Too large for a preview.
    #[error("{}", t!("remote.too_large"))]
    TooLarge,
    /// The user cancelled.
    #[error("{}", t!("remote.cancelled"))]
    Cancelled,
    /// The server answered with an error of its own.
    #[error("{}", t!("remote.server", reason = .0))]
    Server(String),
    /// Reading or writing the local side failed.
    #[error("{0}")]
    Local(#[from] io::Error),
}

/// Result of a remote operation.
pub type RemoteResult<T> = Result<T, RemoteError>;

impl RemoteError {
    /// The button the UI offers for this failure.
    pub const fn fail_kind(&self) -> FailKind {
        match self {
            Self::NoSuchHost(_) | Self::Unreachable(_) | Self::Timeout | Self::Lost => {
                FailKind::Offline
            }
            Self::NeedsLogin
            | Self::LoginFailed
            | Self::HostKeyUnknown(_)
            | Self::HostKeyChanged(_)
            | Self::BadKey(_) => FailKind::NeedsLogin,
            _ => FailKind::Other,
        }
    }

    /// Whether the connection is likely dead (worth reconnecting once).
    pub(crate) const fn is_connection(&self) -> bool {
        matches!(self, Self::Lost | Self::Timeout)
    }

    /// As an `io::Error` for the `Fs` boundary; the message and button survive the trip
    /// (see [`LocationError`]).
    pub fn into_io(self) -> io::Error {
        let kind = match &self {
            Self::NotFound => io::ErrorKind::NotFound,
            Self::Denied | Self::NeedsLogin | Self::LoginFailed => io::ErrorKind::PermissionDenied,
            Self::Exists => io::ErrorKind::AlreadyExists,
            Self::Timeout => io::ErrorKind::TimedOut,
            Self::Cancelled => io::ErrorKind::Interrupted,
            Self::Local(err) => err.kind(),
            _ => io::ErrorKind::Other,
        };
        LocationError {
            message: self.to_string(),
            kind: self.fail_kind(),
        }
        .into_io(kind)
    }
}

impl From<RemoteError> for io::Error {
    fn from(err: RemoteError) -> Self {
        err.into_io()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_their_message_and_button_through_io() {
        let io = RemoteError::NotFound.into_io();
        assert_eq!(io.kind(), io::ErrorKind::NotFound);
        let (message, kind) = arke_core::net::describe(&io, true);
        assert_eq!(message, RemoteError::NotFound.to_string());
        assert_eq!(kind, FailKind::Other);
        let (_, kind) = arke_core::net::describe(&RemoteError::Timeout.into_io(), true);
        assert_eq!(kind, FailKind::Offline);
        let (_, kind) = arke_core::net::describe(&RemoteError::NeedsLogin.into_io(), true);
        assert_eq!(kind, FailKind::NeedsLogin);
    }
}
