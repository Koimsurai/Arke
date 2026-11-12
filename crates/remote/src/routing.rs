//! [`RoutingFs`]: the app's `Fs`. Remote URLs go to [`Remotes`], everything else to the local
//! file system. Remote folders are not watched (no change notifications over SFTP/WebDAV);
//! Arke relists them after its own changes and on F5.

use std::io;
use std::path::Path;
use std::sync::Arc;

use arke_core::Entry;
use arke_core::fs::{DirIter, Fs, WatchCallback};
use arke_core::remote::RemoteUrl;

use crate::hub::Remotes;

/// Local or remote, by path.
pub struct RoutingFs {
    local: Arc<dyn Fs>,
    remotes: Arc<Remotes>,
}

impl RoutingFs {
    /// Routes remote URLs to `remotes` and the rest to `local`.
    pub fn new(local: Arc<dyn Fs>, remotes: Arc<Remotes>) -> Self {
        Self { local, remotes }
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "not available on remote locations",
    )
}

impl Fs for RoutingFs {
    fn read_dir(&self, path: &Path) -> io::Result<DirIter> {
        match RemoteUrl::from_path(path) {
            Some(url) => {
                let entries = self.remotes.list(&url)?;
                Ok(Box::new(entries.into_iter().map(Ok)))
            }
            None => self.local.read_dir(path),
        }
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        match RemoteUrl::from_path(path) {
            Some(url) => Ok(self.remotes.stat(&url)?),
            None => self.local.stat(path),
        }
    }

    fn watch(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>> {
        if RemoteUrl::from_path(path).is_some() {
            return Err(unsupported());
        }
        self.local.watch(path, on_event)
    }

    fn watch_tree(&self, path: &Path, on_event: WatchCallback) -> io::Result<Box<dyn Send>> {
        if RemoteUrl::from_path(path).is_some() {
            return Err(unsupported());
        }
        self.local.watch_tree(path, on_event)
    }

    fn read(&self, path: &Path, max_len: u64) -> io::Result<Vec<u8>> {
        match RemoteUrl::from_path(path) {
            Some(url) => Ok(self.remotes.read(&url, max_len)?),
            None => self.local.read(path, max_len),
        }
    }

    fn read_head(&self, path: &Path, len: u64) -> io::Result<Vec<u8>> {
        match RemoteUrl::from_path(path) {
            Some(url) => Ok(self.remotes.read_head(&url, len)?),
            None => self.local.read_head(path, len),
        }
    }

    fn read_link(&self, path: &Path) -> io::Result<std::path::PathBuf> {
        if RemoteUrl::from_path(path).is_some() {
            return Err(unsupported());
        }
        self.local.read_link(path)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn arke_core::fs::ReadSeek>> {
        match RemoteUrl::from_path(path) {
            Some(url) => {
                let local = self.remotes.fetch(&url, crate::hub::PREVIEW_MAX)?;
                Ok(Box::new(io::BufReader::new(std::fs::File::open(local)?)))
            }
            None => self.local.open(path),
        }
    }
}
