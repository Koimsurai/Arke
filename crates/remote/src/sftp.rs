//! SFTP over SSH (russh). Connects with a timeout at every step, checks the host key against
//! Arke's known_hosts, and signs in with a key file, a password or a running SSH agent
//! (Windows OpenSSH agent or Pageant).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arke_core::remote::RemoteUrl;
use arke_core::{Entry, EntryKind};
use parking_lot::Mutex;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::error::{HostKey, RemoteError, RemoteResult};
use crate::known_hosts::{KnownHosts, Verdict, host_pattern};

/// Name resolution and TCP connect.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// SSH handshake and sign-in.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// One SFTP request.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// How to sign in.
#[derive(Clone, Default)]
pub(crate) struct SshAuth {
    /// Password, or the key file's passphrase when `key_file` is set.
    pub(crate) secret: Option<String>,
    /// Private key in OpenSSH or PEM format.
    pub(crate) key_file: Option<PathBuf>,
}

// The secret never appears in logs.
impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshAuth")
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .field("key_file", &self.key_file)
            .finish()
    }
}

/// `HostKey` for a server's public key.
pub(crate) fn host_key(pattern: &str, key: &PublicKey) -> HostKey {
    let openssh = key.to_openssh().unwrap_or_default();
    let mut fields = openssh.split_whitespace();
    HostKey {
        host: pattern.to_owned(),
        algorithm: fields.next().unwrap_or_default().to_owned(),
        key: fields.next().unwrap_or_default().to_owned(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
    }
}

struct Client {
    known: Arc<KnownHosts>,
    pattern: String,
    /// Why the host key was refused, read after the handshake fails.
    refused: Arc<Mutex<Option<RemoteError>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    // The trait method is async; this check needs no awaiting.
    #[allow(clippy::unused_async_trait_impl)]
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.refused.lock() = Some(RemoteError::Server(
                "host certificates are not supported".into(),
            ));
            return Ok(false);
        };
        let host = host_key(&self.pattern, key);
        let refusal = match self.known.verdict(&host) {
            Verdict::Trusted => return Ok(true),
            Verdict::Unknown => RemoteError::HostKeyUnknown(host),
            Verdict::Changed => {
                log::warn!("host key of {} changed: {}", host.host, host.fingerprint);
                RemoteError::HostKeyChanged(host)
            }
        };
        *self.refused.lock() = Some(refusal);
        Ok(false)
    }
}

/// An open SFTP session.
pub(crate) struct SftpConn {
    handle: Handle<Client>,
    pub(crate) sftp: SftpSession,
}

impl SftpConn {
    /// Whether the SSH connection is gone (reconnect before the next request).
    pub(crate) fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
}

async fn timed<T>(limit: Duration, fut: impl Future<Output = RemoteResult<T>>) -> RemoteResult<T> {
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or(Err(RemoteError::Timeout))
}

/// Connects and signs in.
pub(crate) async fn connect(
    url: &RemoteUrl,
    auth: &SshAuth,
    known: Arc<KnownHosts>,
) -> RemoteResult<SftpConn> {
    let user = url.user.clone().ok_or(RemoteError::NeedsLogin)?;
    let host = url.host.clone();
    let addrs: Vec<std::net::SocketAddr> = timed(CONNECT_TIMEOUT, async {
        tokio::net::lookup_host((host.as_str(), url.port))
            .await
            .map(Iterator::collect)
            .map_err(|_| RemoteError::NoSuchHost(host.clone()))
    })
    .await?;
    let stream = timed(CONNECT_TIMEOUT, async {
        tokio::net::TcpStream::connect(&addrs[..])
            .await
            .map_err(|_| RemoteError::Unreachable(url.host_port()))
    })
    .await?;
    let _ = stream.set_nodelay(true);
    let refused = Arc::new(Mutex::new(None));
    let handler = Client {
        known,
        pattern: host_pattern(&url.host, url.port),
        refused: refused.clone(),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..client::Config::default()
    });
    timed(HANDSHAKE_TIMEOUT, async {
        let mut handle = match client::connect_stream(config, stream, handler).await {
            Ok(handle) => handle,
            Err(err) => {
                return Err(refused
                    .lock()
                    .take()
                    .unwrap_or_else(|| RemoteError::Server(err.to_string())));
            }
        };
        sign_in(&mut handle, &user, auth).await?;
        let channel = handle.channel_open_session().await.map_err(ssh_error)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(ssh_error)?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(sftp_error)?;
        sftp.set_timeout(REQUEST_TIMEOUT_SECS);
        Ok(SftpConn { handle, sftp })
    })
    .await
}

fn ssh_error(err: russh::Error) -> RemoteError {
    match err {
        russh::Error::Disconnect | russh::Error::HUP | russh::Error::SendError => RemoteError::Lost,
        other => RemoteError::Server(other.to_string()),
    }
}

/// SFTP status -> error.
pub(crate) fn sftp_error(err: SftpError) -> RemoteError {
    match err {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => RemoteError::NotFound,
            StatusCode::PermissionDenied => RemoteError::Denied,
            StatusCode::ConnectionLost | StatusCode::NoConnection => RemoteError::Lost,
            _ if status.error_message.to_ascii_lowercase().contains("exist") => RemoteError::Exists,
            _ if status.error_message.is_empty() => {
                RemoteError::Server(status.status_code.to_string())
            }
            _ => RemoteError::Server(status.error_message),
        },
        SftpError::Timeout => RemoteError::Timeout,
        SftpError::IO(_) => RemoteError::Lost,
        other => RemoteError::Server(other.to_string()),
    }
}

async fn sign_in(handle: &mut Handle<Client>, user: &str, auth: &SshAuth) -> RemoteResult<()> {
    if let Some(path) = &auth.key_file {
        let key = match russh::keys::load_secret_key(path, auth.secret.as_deref()) {
            Ok(key) => key,
            Err(russh::keys::Error::KeyIsEncrypted) if auth.secret.is_none() => {
                return Err(RemoteError::NeedsLogin);
            }
            Err(russh::keys::Error::KeyIsEncrypted) => return Err(RemoteError::LoginFailed),
            Err(err) => return Err(RemoteError::BadKey(err.to_string())),
        };
        let hash = handle_rsa_hash(handle).await;
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        let result = handle
            .authenticate_publickey(user, key)
            .await
            .map_err(ssh_error)?;
        return if result.success() {
            Ok(())
        } else {
            Err(RemoteError::LoginFailed)
        };
    }
    if let Some(password) = &auth.secret {
        let result = handle
            .authenticate_password(user, password)
            .await
            .map_err(ssh_error)?;
        return if result.success() {
            Ok(())
        } else {
            Err(RemoteError::LoginFailed)
        };
    }
    if agent_sign_in(handle, user).await {
        return Ok(());
    }
    Err(RemoteError::NeedsLogin)
}

async fn handle_rsa_hash(handle: &Handle<Client>) -> Option<HashAlg> {
    handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten()
}

/// Most keys tried from an agent (servers drop the connection after a few failures).
const AGENT_MAX_KEYS: usize = 5;

/// Tries the keys of a running SSH agent: Windows' OpenSSH agent, then Pageant.
#[cfg(windows)]
async fn agent_sign_in(handle: &mut Handle<Client>, user: &str) -> bool {
    use russh::keys::agent::client::AgentClient;
    let pipe =
        tokio::net::windows::named_pipe::ClientOptions::new().open(r"\\.\pipe\openssh-ssh-agent");
    if let Ok(pipe) = pipe {
        let mut agent = AgentClient::connect(pipe);
        if try_agent(handle, user, &mut agent).await {
            return true;
        }
    }
    if let Ok(mut agent) = AgentClient::connect_pageant().await {
        return try_agent(handle, user, &mut agent).await;
    }
    false
}

#[cfg(not(windows))]
async fn agent_sign_in(_: &mut Handle<Client>, _: &str) -> bool {
    false
}

#[cfg(windows)]
async fn try_agent<S>(
    handle: &mut Handle<Client>,
    user: &str,
    agent: &mut russh::keys::agent::client::AgentClient<S>,
) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use russh::keys::agent::AgentIdentity;
    let Ok(Ok(identities)) =
        tokio::time::timeout(Duration::from_secs(3), agent.request_identities()).await
    else {
        return false;
    };
    let hash = handle_rsa_hash(handle).await;
    for identity in identities.into_iter().take(AGENT_MAX_KEYS) {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        if let Ok(result) = handle
            .authenticate_publickey_with(user, key, hash, agent)
            .await
            && result.success()
        {
            return true;
        }
    }
    false
}

fn entry_of(
    name: &str,
    meta: &russh_sftp::client::fs::Metadata,
    link_is_dir: Option<bool>,
) -> Entry {
    let modified = meta.modified().ok();
    let ft = meta.file_type();
    if ft.is_symlink() {
        Entry::new(
            name,
            EntryKind::Link {
                is_dir: link_is_dir.unwrap_or(false),
            },
            0,
            modified,
            0,
        )
    } else if ft.is_dir() {
        Entry::new(name, EntryKind::Dir, 0, modified, 0)
    } else {
        Entry::new(name, EntryKind::File, meta.len(), modified, 0)
    }
}

impl SftpConn {
    /// Lists a folder. Symlinks are followed once to tell folders from files.
    pub(crate) async fn list(&self, path: &str) -> RemoteResult<Vec<Entry>> {
        let dir = self.sftp.read_dir(path).await.map_err(sftp_error)?;
        let mut out = Vec::new();
        for item in dir {
            let name = item.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let meta = item.metadata();
            let link_is_dir = if meta.file_type().is_symlink() {
                self.sftp
                    .metadata(item.path())
                    .await
                    .ok()
                    .map(|m| m.file_type().is_dir())
            } else {
                None
            };
            out.push(entry_of(&name, &meta, link_is_dir));
        }
        Ok(out)
    }

    /// One item, without following a final symlink.
    pub(crate) async fn stat(&self, path: &str) -> RemoteResult<Entry> {
        let meta = self.sftp.symlink_metadata(path).await.map_err(sftp_error)?;
        let name = path.rsplit('/').next().unwrap_or_default();
        let link_is_dir = if meta.file_type().is_symlink() {
            self.sftp
                .metadata(path)
                .await
                .ok()
                .map(|m| m.file_type().is_dir())
        } else {
            None
        };
        Ok(entry_of(name, &meta, link_is_dir))
    }

    /// Opens for reading.
    pub(crate) async fn open(&self, path: &str) -> RemoteResult<russh_sftp::client::fs::File> {
        self.sftp.open(path).await.map_err(sftp_error)
    }

    /// Creates (or truncates) for writing.
    pub(crate) async fn create(&self, path: &str) -> RemoteResult<russh_sftp::client::fs::File> {
        self.sftp.create(path).await.map_err(sftp_error)
    }

    /// Up to `len` bytes from the start.
    pub(crate) async fn read_head(&self, path: &str, len: u64) -> RemoteResult<Vec<u8>> {
        let file = self.open(path).await?;
        let mut bytes = Vec::new();
        file.take(len)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| RemoteError::Lost)?;
        Ok(bytes)
    }

    pub(crate) async fn mkdir(&self, path: &str) -> RemoteResult<()> {
        self.sftp.create_dir(path).await.map_err(sftp_error)
    }

    pub(crate) async fn rename(&self, from: &str, to: &str) -> RemoteResult<()> {
        if self.sftp.try_exists(to).await.unwrap_or(false) {
            return Err(RemoteError::Exists);
        }
        self.sftp.rename(from, to).await.map_err(sftp_error)
    }

    pub(crate) async fn remove_file(&self, path: &str) -> RemoteResult<()> {
        self.sftp.remove_file(path).await.map_err(sftp_error)
    }

    pub(crate) async fn remove_dir(&self, path: &str) -> RemoteResult<()> {
        self.sftp.remove_dir(path).await.map_err(sftp_error)
    }
}

/// Writes all of `bytes` to a remote file.
pub(crate) async fn write_chunk(
    file: &mut russh_sftp::client::fs::File,
    bytes: &[u8],
) -> RemoteResult<()> {
    file.write_all(bytes).await.map_err(|_| RemoteError::Lost)
}

/// Flushes and closes a remote file being written.
pub(crate) async fn finish(mut file: russh_sftp::client::fs::File) -> RemoteResult<()> {
    file.shutdown().await.map_err(|_| RemoteError::Lost)
}

/// Reads the next chunk (empty at the end).
pub(crate) async fn read_chunk(
    file: &mut russh_sftp::client::fs::File,
    buf: &mut [u8],
) -> RemoteResult<usize> {
    file.read(buf).await.map_err(|_| RemoteError::Lost)
}
