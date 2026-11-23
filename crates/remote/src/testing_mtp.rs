//! A fake phone for tests (#292): devices and their files in memory, behind the same
//! [`Portable`] trait the WPD backend implements, so everything above it is tested without a
//! phone.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    missing_docs,
    reason = "test support: a failure should stop the test"
)]

use std::collections::BTreeMap;
use std::io::Write;
use std::time::SystemTime;

use parking_lot::Mutex;

use crate::error::{RemoteError, RemoteResult};
use crate::hub::TransferSink;
use crate::mtp::{Device, Object, Portable, segments};

/// A file or folder on a fake device.
#[derive(Clone, Debug)]
pub enum Node {
    File(Vec<u8>),
    Folder(BTreeMap<String, Self>),
}

impl Node {
    /// A folder holding `items`.
    pub fn folder<const N: usize>(items: [(&str, Self); N]) -> Self {
        Self::Folder(items.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    /// A file holding `bytes`.
    pub fn file(bytes: &[u8]) -> Self {
        Self::File(bytes.to_vec())
    }

    fn at(&self, path: &str) -> Option<&Self> {
        let mut node = self;
        for name in segments(path) {
            match node {
                Self::Folder(items) => node = items.get(name)?,
                Self::File(_) => return None,
            }
        }
        Some(node)
    }

    fn object(name: &str, node: &Self) -> Object {
        Object {
            name: name.to_owned(),
            is_folder: matches!(node, Self::Folder(_)),
            size: match node {
                Self::File(bytes) => bytes.len() as u64,
                Self::Folder(_) => 0,
            },
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        }
    }
}

/// Fake devices: plug them in and out, lock their screens.
#[derive(Default)]
pub struct FakeDevices {
    devices: Mutex<Vec<(Device, Node, bool)>>,
}

impl FakeDevices {
    /// Connects a device named `name` with key `key` holding `root` (its storages).
    pub fn plug(&self, key: &str, name: &str, root: Node) {
        self.devices.lock().push((
            Device {
                key: key.to_owned(),
                name: name.to_owned(),
            },
            root,
            false,
        ));
    }

    /// Disconnects the device `key`.
    pub fn unplug(&self, key: &str) {
        self.devices.lock().retain(|(d, _, _)| d.key != key);
    }

    /// Locks or unlocks the device's screen (locked devices refuse access).
    pub fn set_locked(&self, key: &str, locked: bool) {
        for (d, _, l) in self.devices.lock().iter_mut() {
            if d.key == key {
                *l = locked;
            }
        }
    }

    fn with<T>(&self, key: &str, f: impl FnOnce(&Node) -> RemoteResult<T>) -> RemoteResult<T> {
        let (root, locked) = self
            .devices
            .lock()
            .iter()
            .find(|(d, _, _)| d.key == key)
            .map(|(_, root, locked)| (root.clone(), *locked))
            .ok_or(RemoteError::DeviceGone)?;
        if locked {
            return Err(RemoteError::Locked);
        }
        f(&root)
    }
}

impl Portable for FakeDevices {
    fn devices(&self) -> RemoteResult<Vec<Device>> {
        Ok(self
            .devices
            .lock()
            .iter()
            .map(|(d, _, _)| d.clone())
            .collect())
    }

    fn list(&self, device: &str, path: &str) -> RemoteResult<Vec<Object>> {
        self.with(device, |root| match root.at(path) {
            Some(Node::Folder(items)) => {
                Ok(items.iter().map(|(k, v)| Node::object(k, v)).collect())
            }
            Some(Node::File(_)) => Err(RemoteError::Server("not a folder".into())),
            None => Err(RemoteError::NotFound),
        })
    }

    fn stat(&self, device: &str, path: &str) -> RemoteResult<Object> {
        self.with(device, |root| {
            let node = root.at(path).ok_or(RemoteError::NotFound)?;
            let name = segments(path).last().copied().unwrap_or_default();
            Ok(Node::object(name, node))
        })
    }

    fn read(
        &self,
        device: &str,
        path: &str,
        out: &mut dyn Write,
        max: u64,
        sink: &dyn TransferSink,
    ) -> RemoteResult<u64> {
        let bytes = self.with(device, |root| match root.at(path) {
            Some(Node::File(bytes)) => Ok(bytes.clone()),
            Some(Node::Folder(_)) => Err(RemoteError::Server("not a file".into())),
            None => Err(RemoteError::NotFound),
        })?;
        let n = bytes.len().min(usize::try_from(max).unwrap_or(usize::MAX));
        for chunk in bytes[..n].chunks(4) {
            if sink.cancelled() {
                return Err(RemoteError::Cancelled);
            }
            out.write_all(chunk)?;
            sink.add_bytes(chunk.len() as u64);
        }
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use arke_core::fs::Fs as _;
    use arke_core::remote::RemoteUrl;

    use super::*;
    use crate::hub::{NoProgress, Remotes, RemotesConfig};

    struct NoHttp;

    impl crate::dav::Transport for NoHttp {
        fn send(
            &self,
            _: crate::dav::Request<'_>,
            _: &mut dyn Write,
            _: crate::dav::Progress<'_>,
        ) -> RemoteResult<u16> {
            Err(RemoteError::Server("no http".into()))
        }
    }

    fn phone() -> (Arc<FakeDevices>, Arc<Remotes>) {
        let devices = Arc::new(FakeDevices::default());
        devices.plug(
            "pixel-7-3fa2c1",
            "Pixel 7",
            Node::folder([(
                "Internal shared storage",
                Node::folder([(
                    "DCIM",
                    Node::folder([("IMG_0001.jpg", Node::file(b"jpeg bytes"))]),
                )]),
            )]),
        );
        let remotes = Remotes::new(RemotesConfig {
            known_hosts: None,
            cache_dir: std::env::temp_dir().join(format!("arke-mtp-test-{}", std::process::id())),
            secrets: Arc::new(crate::creds::NoSecrets),
            transport: Arc::new(NoHttp),
            portable: Arc::clone(&devices) as Arc<dyn Portable>,
        });
        (devices, remotes)
    }

    fn url(s: &str) -> RemoteUrl {
        RemoteUrl::from_path(Path::new(s)).unwrap()
    }

    /// Browsing and copying off the phone go through the same calls as any remote location.
    #[test]
    fn a_phone_is_browsed_and_read_like_a_remote_folder() {
        let (_, remotes) = phone();
        assert_eq!(remotes.portable_devices().unwrap()[0].name, "Pixel 7");
        let fs = crate::RoutingFs::new(Arc::new(arke_core::RealFs), Arc::clone(&remotes));
        let root: Vec<String> = fs
            .read_dir(Path::new("mtp://pixel-7-3fa2c1"))
            .unwrap()
            .map(|e| e.unwrap().name.to_string())
            .collect();
        assert_eq!(root, ["Internal shared storage"]);
        let photo = url("mtp://pixel-7-3fa2c1/Internal shared storage/DCIM/IMG_0001.jpg");
        let entry = remotes.stat(&photo).unwrap();
        assert_eq!((entry.size, entry.is_dir_like()), (10, false));
        assert_eq!(remotes.read_head(&photo, 4).unwrap(), b"jpeg");
        let dest = std::env::temp_dir().join(format!("arke-mtp-copy-{}.jpg", std::process::id()));
        remotes.download_file(&photo, &dest, &NoProgress).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"jpeg bytes");
        let _ = std::fs::remove_file(dest);
    }

    /// Nothing on the phone changes in this version.
    #[test]
    fn nothing_on_a_phone_is_changed() {
        let (_, remotes) = phone();
        let dcim = url("mtp://pixel-7-3fa2c1/Internal shared storage/DCIM");
        let photo = dcim.join("IMG_0001.jpg");
        assert!(matches!(
            remotes.mkdir(&dcim.join("new")),
            Err(RemoteError::ReadOnly)
        ));
        assert!(matches!(
            remotes.delete(&photo, &NoProgress),
            Err(RemoteError::ReadOnly)
        ));
        assert!(matches!(
            remotes.rename(&photo, &dcim.join("b.jpg")),
            Err(RemoteError::ReadOnly)
        ));
        assert!(matches!(
            remotes.upload_file(
                Path::new("a"),
                &dcim.join("a"),
                &mut std::io::empty(),
                0,
                &NoProgress
            ),
            Err(RemoteError::ReadOnly)
        ));
        assert!(matches!(
            remotes.create_empty(&dcim.join("a")),
            Err(RemoteError::ReadOnly)
        ));
    }

    /// A locked phone and a pulled cable say what happened.
    #[test]
    fn a_locked_or_unplugged_phone_says_so() {
        let (devices, remotes) = phone();
        let root = url("mtp://pixel-7-3fa2c1");
        devices.set_locked("pixel-7-3fa2c1", true);
        assert!(matches!(remotes.list(&root), Err(RemoteError::Locked)));
        devices.set_locked("pixel-7-3fa2c1", false);
        assert!(remotes.list(&root).is_ok());
        devices.unplug("pixel-7-3fa2c1");
        assert!(matches!(remotes.list(&root), Err(RemoteError::DeviceGone)));
        assert!(remotes.portable_devices().unwrap().is_empty());
    }
}
