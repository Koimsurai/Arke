//! [`Remotes`]: every remote server Arke talks to, behind one blocking API for background
//! threads. Keeps one SFTP connection per server (reconnecting once if it dropped), the
//! passwords typed this session, and a download cache for previews and opened files.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arke_core::remote::{RemoteUrl, Scheme};
use arke_core::{Entry, EntryKind};
use parking_lot::Mutex;
use sha2::Digest as _;

use crate::creds::SecretStore;
use crate::dav::{DavClient, Transport};
use crate::error::{HostKey, RemoteError, RemoteResult};
use crate::known_hosts::KnownHosts;
use crate::sftp::{self, SftpConn, SshAuth};

/// One SFTP request (listing a folder may take a while on slow links).
const OP_TIMEOUT: Duration = Duration::from_secs(60);
/// One chunk of a transfer.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// Transfer chunk size.
const CHUNK: usize = 256 * 1024;
/// Largest file downloaded for a preview.
pub const PREVIEW_MAX: u64 = 32 * 1024 * 1024;

/// Receives transfer progress; called on the transferring thread.
pub trait TransferSink {
    /// Stop as soon as possible.
    fn cancelled(&self) -> bool;
    /// `n` more bytes moved.
    fn add_bytes(&self, n: u64);
}

/// Ignores progress, never cancels.
pub struct NoProgress;

impl TransferSink for NoProgress {
    fn cancelled(&self) -> bool {
        false
    }
    fn add_bytes(&self, _: u64) {}
}

/// Where Remotes keeps things.
pub struct RemotesConfig {
    /// Trusted SSH host keys; `None` keeps them in memory (tests).
    pub known_hosts: Option<PathBuf>,
    /// Download cache for previews and opened files (under `%TEMP%`).
    pub cache_dir: PathBuf,
    /// Remembered passwords.
    pub secrets: Arc<dyn SecretStore>,
    /// HTTP for WebDAV.
    pub transport: Arc<dyn Transport>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Arc<SftpConn>>>>;

/// All remote servers (see the module docs).
pub struct Remotes {
    /// Runs SSH; `None` only if the OS refused the worker threads (SFTP then reports an error).
    rt: Option<tokio::runtime::Runtime>,
    known: Arc<KnownHosts>,
    secrets: Arc<dyn SecretStore>,
    transport: Arc<dyn Transport>,
    cache_dir: PathBuf,
    /// Passwords typed this session, by origin.
    session: Mutex<HashMap<String, String>>,
    /// SFTP key files, by origin.
    key_files: Mutex<HashMap<String, PathBuf>>,
    conns: Mutex<HashMap<String, Slot>>,
}

async fn timed<T>(limit: Duration, fut: impl Future<Output = RemoteResult<T>>) -> RemoteResult<T> {
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or(Err(RemoteError::Timeout))
}

/// A remote name as a Windows file name (characters Windows refuses become `_`).
pub fn local_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    if out.is_empty() || arke_core::path::validate_file_name(&out).is_err() {
        out.insert(0, '_');
    }
    out
}

impl Remotes {
    /// Starts the runtime for SSH (two worker threads, idle until used). Touches no network.
    pub fn new(config: RemotesConfig) -> Arc<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("remote-io")
            .enable_all()
            .build()
            .inspect_err(|e| log::error!("cannot start the SSH runtime: {e}"))
            .ok();
        Arc::new(Self {
            rt,
            known: Arc::new(KnownHosts::new(config.known_hosts)),
            secrets: config.secrets,
            transport: config.transport,
            cache_dir: config.cache_dir,
            session: Mutex::default(),
            key_files: Mutex::default(),
            conns: Mutex::default(),
        })
    }

    /// Runs a future on the SSH runtime from a background thread.
    fn block_on<T>(&self, fut: impl Future<Output = RemoteResult<T>>) -> RemoteResult<T> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "remote I/O on the UI thread"
        );
        match &self.rt {
            Some(rt) => rt.block_on(fut),
            None => Err(RemoteError::Server("SSH is unavailable".into())),
        }
    }

    /// Uses this private key for the server (`None`: password or agent).
    pub fn set_key_file(&self, url: &RemoteUrl, key_file: Option<PathBuf>) {
        let mut keys = self.key_files.lock();
        match key_file {
            Some(path) => keys.insert(url.origin(), path),
            None => keys.remove(&url.origin()),
        };
    }

    fn auth(&self, url: &RemoteUrl) -> SshAuth {
        let origin = url.origin();
        let typed = self.session.lock().get(&origin).cloned();
        SshAuth {
            secret: typed.or_else(|| self.secrets.load(url)),
            key_file: self.key_files.lock().get(&origin).cloned(),
        }
    }

    /// Trusts a host key from now on (after the user confirmed it).
    pub fn trust(&self, key: &HostKey) -> std::io::Result<()> {
        self.known.trust(key)
    }

    /// Signs in with `secret` (password, or the key file's passphrase), checking it by opening
    /// `url`. On success the secret is kept for this session, and saved in Credential Manager
    /// when `remember` is set. On failure nothing changes.
    pub fn sign_in(
        &self,
        url: &RemoteUrl,
        secret: Option<String>,
        remember: bool,
    ) -> RemoteResult<Entry> {
        let origin = url.origin();
        let previous = {
            let mut session = self.session.lock();
            match &secret {
                Some(s) => session.insert(origin.clone(), s.clone()),
                None => session.remove(&origin),
            }
        };
        self.disconnect(url);
        let result = self.stat(url);
        match (&result, &secret) {
            (Ok(_), Some(secret)) if remember => self.secrets.save(url, secret),
            (Ok(_), _) => {}
            (Err(_), _) => {
                let mut session = self.session.lock();
                match previous {
                    Some(p) => session.insert(origin, p),
                    None => session.remove(&origin),
                };
            }
        }
        result
    }

    /// Forgets the server: session password, saved password and open connection.
    pub fn forget(&self, url: &RemoteUrl) {
        self.session.lock().remove(&url.origin());
        self.secrets.forget(url);
        self.disconnect(url);
    }

    /// Closes the SFTP connection to the server, if any.
    pub fn disconnect(&self, url: &RemoteUrl) {
        self.conns.lock().remove(&url.origin());
    }

    fn slot(&self, url: &RemoteUrl) -> Slot {
        self.conns.lock().entry(url.origin()).or_default().clone()
    }

    // The slot stays locked while connecting, so concurrent requests share one connection.
    #[allow(clippy::significant_drop_tightening)]
    async fn conn(&self, url: &RemoteUrl) -> RemoteResult<Arc<SftpConn>> {
        let slot = self.slot(url);
        let mut held = slot.lock().await;
        if let Some(conn) = held.as_ref().filter(|c| !c.is_closed()) {
            return Ok(conn.clone());
        }
        let auth = self.auth(url);
        log::info!("connecting to {}", url.origin());
        let conn = Arc::new(sftp::connect(url, &auth, self.known.clone()).await?);
        *held = Some(conn.clone());
        Ok(conn)
    }

    /// Runs one SFTP request, reconnecting once if the connection had dropped.
    fn with_sftp<T, F, Fut>(&self, url: &RemoteUrl, op: F) -> RemoteResult<T>
    where
        F: Fn(Arc<SftpConn>) -> Fut,
        Fut: Future<Output = RemoteResult<T>>,
    {
        self.block_on(async {
            for attempt in 0..2 {
                let conn = self.conn(url).await?;
                match timed(OP_TIMEOUT, op(conn)).await {
                    Err(e) if e.is_connection() && attempt == 0 => {
                        log::info!("{}: {e}, reconnecting", url.origin());
                        self.slot(url).lock().await.take();
                    }
                    other => return other,
                }
            }
            Err(RemoteError::Lost)
        })
    }

    fn dav(&self, url: &RemoteUrl) -> DavClient {
        DavClient::new(url, self.auth(url).secret, self.transport.clone())
    }

    /// Lists a folder.
    pub fn list(&self, url: &RemoteUrl) -> RemoteResult<Vec<Entry>> {
        if url.scheme.is_dav() {
            return self.dav(url).list(&url.path);
        }
        let path = url.path.clone();
        self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.list(&path).await }
        })
    }

    /// One item.
    pub fn stat(&self, url: &RemoteUrl) -> RemoteResult<Entry> {
        if url.scheme.is_dav() {
            return self.dav(url).stat(&url.path);
        }
        let path = url.path.clone();
        self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.stat(&path).await }
        })
    }

    /// Up to `len` bytes from the start of a file (text previews).
    pub fn read_head(&self, url: &RemoteUrl, len: u64) -> RemoteResult<Vec<u8>> {
        if url.scheme.is_dav() {
            return self.dav(url).read_head(&url.path, len);
        }
        let path = url.path.clone();
        self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.read_head(&path, len).await }
        })
    }

    /// Creates a folder.
    pub fn mkdir(&self, url: &RemoteUrl) -> RemoteResult<()> {
        if url.scheme.is_dav() {
            return self.dav(url).mkdir(&url.path);
        }
        let path = url.path.clone();
        self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.mkdir(&path).await }
        })
    }

    /// Creates an empty file; fails if the name is taken.
    pub fn create_empty(&self, url: &RemoteUrl) -> RemoteResult<()> {
        if self.stat(url).is_ok() {
            return Err(RemoteError::Exists);
        }
        self.upload_file(Path::new(""), url, &mut std::io::empty(), 0, &NoProgress)
    }

    /// Renames or moves on the same server; never replaces an existing item.
    pub fn rename(&self, from: &RemoteUrl, to: &RemoteUrl) -> RemoteResult<()> {
        if from.scheme.is_dav() {
            let is_dir = self.stat(from)?.is_dir_like();
            return self.dav(from).rename(&from.path, &to.path, is_dir);
        }
        let (a, b) = (from.path.clone(), to.path.clone());
        self.with_sftp(from, |c| {
            let (a, b) = (a.clone(), b.clone());
            async move { c.rename(&a, &b).await }
        })
    }

    /// Deletes permanently (folders with everything in them). There is no recycle bin on a
    /// server: the caller must have confirmed.
    pub fn delete(&self, url: &RemoteUrl, sink: &dyn TransferSink) -> RemoteResult<()> {
        let entry = self.stat(url)?;
        if url.scheme.is_dav() {
            return self
                .dav(url)
                .delete(&url.path, entry.kind == EntryKind::Dir);
        }
        self.delete_sftp(url, &entry, sink)
    }

    fn delete_sftp(
        &self,
        url: &RemoteUrl,
        entry: &Entry,
        sink: &dyn TransferSink,
    ) -> RemoteResult<()> {
        if sink.cancelled() {
            return Err(RemoteError::Cancelled);
        }
        let path = url.path.clone();
        // A link is removed itself, never followed into.
        if entry.kind == EntryKind::Dir {
            for child in self.list(url)? {
                self.delete_sftp(&url.join(&child.name), &child, sink)?;
            }
            return self.with_sftp(url, |c| {
                let path = path.clone();
                async move { c.remove_dir(&path).await }
            });
        }
        self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.remove_file(&path).await }
        })
    }

    /// Downloads one file to `dest` (created or replaced). On failure or cancel the partial
    /// file is removed.
    pub fn download_file(
        &self,
        url: &RemoteUrl,
        dest: &Path,
        sink: &dyn TransferSink,
    ) -> RemoteResult<()> {
        let mut out = std::fs::File::create(dest)?;
        let result = self.download_into(url, &mut out, sink);
        drop(out);
        if result.is_err() {
            let _ = std::fs::remove_file(dest);
        }
        result
    }

    fn download_into(
        &self,
        url: &RemoteUrl,
        out: &mut std::fs::File,
        sink: &dyn TransferSink,
    ) -> RemoteResult<()> {
        if url.scheme.is_dav() {
            let mut last = 0u64;
            return self.dav(url).download(&url.path, out, &mut |got| {
                sink.add_bytes(got - last);
                last = got;
                !sink.cancelled()
            });
        }
        let path = url.path.clone();
        let mut file = self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.open(&path).await }
        })?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if sink.cancelled() {
                return Err(RemoteError::Cancelled);
            }
            let n = self.block_on(timed(CHUNK_TIMEOUT, sftp::read_chunk(&mut file, &mut buf)))?;
            if n == 0 {
                return Ok(());
            }
            out.write_all(&buf[..n])?;
            sink.add_bytes(n as u64);
        }
    }

    /// Uploads `len` bytes from `reader` to `url`, replacing a file of that name (callers pick
    /// free names first). `source` is only for messages.
    pub fn upload_file(
        &self,
        source: &Path,
        url: &RemoteUrl,
        reader: &mut dyn std::io::Read,
        len: u64,
        sink: &dyn TransferSink,
    ) -> RemoteResult<()> {
        let _ = source;
        if url.scheme.is_dav() {
            let mut last = 0u64;
            return self.dav(url).upload(&url.path, reader, len, &mut |sent| {
                sink.add_bytes(sent - last);
                last = sent;
                !sink.cancelled()
            });
        }
        let path = url.path.clone();
        let mut file = self.with_sftp(url, |c| {
            let path = path.clone();
            async move { c.create(&path).await }
        })?;
        let mut buf = vec![0u8; CHUNK];
        let result = loop {
            if sink.cancelled() {
                break Err(RemoteError::Cancelled);
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break Ok(());
            }
            if let Err(e) = self.block_on(timed(
                CHUNK_TIMEOUT,
                sftp::write_chunk(&mut file, &buf[..n]),
            )) {
                break Err(e);
            }
            sink.add_bytes(n as u64);
        };
        let closed = self.block_on(timed(CHUNK_TIMEOUT, sftp::finish(file)));
        if result.is_err() {
            // Don't leave half a file behind.
            let _ = self.with_sftp(url, |c| {
                let path = path.clone();
                async move { c.remove_file(&path).await }
            });
        }
        result.and(closed)
    }

    /// A local copy of a remote file in the download cache, fetched only if the server's copy
    /// changed (size or time). For previews and for opening in other apps.
    pub fn fetch(&self, url: &RemoteUrl, max: u64) -> RemoteResult<PathBuf> {
        let entry = self.stat(url)?;
        if entry.is_dir_like() {
            return Err(RemoteError::Server("not a file".into()));
        }
        if entry.size > max {
            return Err(RemoteError::TooLarge);
        }
        let parent = url.parent().unwrap_or_else(|| url.clone());
        let digest = sha2::Sha256::digest(parent.to_string().as_bytes());
        let folder = digest[..8]
            .iter()
            .fold(String::with_capacity(16), |mut s, b| {
                use std::fmt::Write as _;
                let _ = write!(s, "{b:02x}");
                s
            });
        let dir = self.cache_dir.join(folder);
        std::fs::create_dir_all(&dir)?;
        let local = dir.join(local_name(&entry.name));
        if let Ok(meta) = std::fs::metadata(&local)
            && meta.len() == entry.size
            && same_time(meta.modified().ok(), entry.modified)
        {
            return Ok(local);
        }
        let part = local.with_extension("arke-part");
        self.download_file(url, &part, &NoProgress)?;
        if let Some(time) = entry.modified
            && let Ok(file) = std::fs::File::options().write(true).open(&part)
        {
            let _ = file.set_modified(time);
        }
        let _ = std::fs::remove_file(&local);
        std::fs::rename(&part, &local)?;
        Ok(local)
    }

    /// Whole file into memory, through the cache (image previews, archive listings).
    pub fn read(&self, url: &RemoteUrl, max: u64) -> RemoteResult<Vec<u8>> {
        let local = self.fetch(url, max.min(PREVIEW_MAX))?;
        Ok(std::fs::read(local)?)
    }

    /// The download cache folder.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Whether this server speaks WebDAV.
    pub const fn is_dav(url: &RemoteUrl) -> bool {
        matches!(url.scheme, Scheme::Dav | Scheme::Davs)
    }
}

/// File times match (servers and FAT keep whole seconds).
fn same_time(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let d = a.duration_since(b).unwrap_or_else(|e| e.duration());
            d < Duration::from_secs(2)
        }
        // Unknown on the server: always fetch again.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_become_valid_windows_names() {
        assert_eq!(local_name("report.txt"), "report.txt");
        assert_eq!(local_name("a:b?c"), "a_b_c");
        assert_eq!(local_name("trailing. "), "trailing");
        assert_eq!(local_name("CON"), "_CON");
        assert_eq!(local_name(""), "_");
    }

    #[test]
    fn times_match_within_a_second_or_two() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        assert!(same_time(Some(t), Some(t + Duration::from_millis(900))));
        assert!(!same_time(Some(t), Some(t + Duration::from_secs(5))));
        assert!(!same_time(Some(t), None));
    }
}
