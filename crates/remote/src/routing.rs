//! [`RoutingFs`]: the app's `Fs`. Remote URLs go to [`Remotes`], everything else to the local
//! file system. Remote folders are not watched (no change notifications over SFTP/WebDAV);
//! Arke relists them after its own changes and on F5.

use std::io;
use std::path::Path;
use std::sync::Arc;

use arke_core::Entry;
use arke_core::fs::{ChangeCallback, DirIter, Fs, WatchCallback};
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

    /// Remote listings carry the names as they are; only local items can be found under another
    /// spelling (#310).
    fn listed_name(&self, path: &Path) -> Option<String> {
        if RemoteUrl::from_path(path).is_some() {
            return None;
        }
        self.local.listed_name(path)
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

    fn watch_changes(&self, path: &Path, on_change: ChangeCallback) -> io::Result<Box<dyn Send>> {
        if RemoteUrl::from_path(path).is_some() {
            return Err(unsupported());
        }
        self.local.watch_changes(path, on_change)
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

    fn open_sequential(&self, path: &Path) -> io::Result<Box<dyn arke_core::fs::ReadSeek>> {
        match RemoteUrl::from_path(path) {
            Some(_) => self.open(path),
            None => self.local.open_sequential(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arke_core::EntryKind;
    use arke_core::fake::FakeFs;

    /// A transport that is never used: these tests touch no network.
    struct NoHttp;

    impl crate::dav::Transport for NoHttp {
        fn send(
            &self,
            _: crate::dav::Request<'_>,
            _: &mut dyn std::io::Write,
            _: crate::dav::Progress<'_>,
        ) -> crate::RemoteResult<u16> {
            Err(crate::RemoteError::Server("no http".into()))
        }
    }

    /// The name a local item is listed under reaches the folder watch through the router, so a
    /// change named in another spelling (a case-only rename, a short 8.3 name) leaves one item.
    #[test]
    fn listed_names_of_local_items_pass_through() {
        let local = FakeFs::new();
        let dir = Path::new(r"C:\w");
        local.insert_dir(
            dir,
            vec![Entry::new("Report.txt", EntryKind::File, 1, None, 0)],
        );
        let remotes = Remotes::new(crate::RemotesConfig {
            known_hosts: None,
            cache_dir: std::env::temp_dir().join(format!("arke-routing-{}", std::process::id())),
            secrets: Arc::new(crate::creds::NoSecrets),
            transport: Arc::new(NoHttp),
            portable: Arc::new(crate::mtp::NoDevices),
        });
        let fs = RoutingFs::new(local, remotes);
        assert_eq!(
            fs.listed_name(&dir.join("report.txt")).as_deref(),
            Some("Report.txt")
        );
        assert_eq!(fs.listed_name(&dir.join("Report.txt")), None);
        assert_eq!(
            fs.listed_name(Path::new("http://u@10.0.0.5/files/a.txt")),
            None
        );
    }
}
