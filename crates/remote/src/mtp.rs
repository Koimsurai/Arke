//! Phones and other portable devices (#292), as remote locations `mtp://<device>/<path>`.
//!
//! Windows talks to them through Windows Portable Devices (WPD), its own MTP driver stack and
//! API (`wpd.rs`); nothing third-party. A device has no file paths: every object has an id the
//! device hands out, and folders have to be listed to learn their children's ids. The backend
//! keeps the ids it has seen by path, so a path resolves without listing every folder above it
//! each time.
//!
//! This first version only reads: browsing, copying to the PC, opening a file (downloaded to the
//! cache first, like any remote file). Nothing on the phone is changed ([`RemoteError::ReadOnly`]),
//! so a mistake here can't cost anyone their photos.
//!
//! MTP does one thing at a time and is slow; a phone with its screen locked refuses access
//! ([`RemoteError::Locked`]), and an Android phone shares its files only in "File transfer" mode.
//! An iPhone shows its photos (DCIM) only, read-only.

use std::io::Write;
use std::time::SystemTime;

use arke_core::{Entry, EntryKind};

use crate::error::{RemoteError, RemoteResult};
use crate::hub::TransferSink;

/// A device connected now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Its key: the host of its `mtp://` locations (see [`device_key`]).
    pub key: String,
    /// The name it gives itself ("Pixel 7").
    pub name: String,
}

/// One file or folder on a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// File name (what the phone shows).
    pub name: String,
    /// A folder (or a storage, which the device root lists).
    pub is_folder: bool,
    /// Size in bytes; 0 for folders.
    pub size: u64,
    /// Last change, when the device says.
    pub modified: Option<SystemTime>,
}

impl Object {
    /// As a directory entry.
    pub fn entry(&self) -> Entry {
        Entry::new(
            self.name.as_str(),
            if self.is_folder {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            if self.is_folder { 0 } else { self.size },
            self.modified,
            0,
        )
    }
}

/// What Arke needs from the devices: the WPD backend, or a fake one in tests.
pub trait Portable: Send + Sync {
    /// The devices connected now.
    fn devices(&self) -> RemoteResult<Vec<Device>>;
    /// The contents of `path` (`/`-separated; `/` lists the device's storages).
    fn list(&self, device: &str, path: &str) -> RemoteResult<Vec<Object>>;
    /// One item.
    fn stat(&self, device: &str, path: &str) -> RemoteResult<Object>;
    /// Copies the file at `path` into `out`, at most `max` bytes, reporting to `sink`; returns
    /// the bytes copied.
    fn read(
        &self,
        device: &str,
        path: &str,
        out: &mut dyn Write,
        max: u64,
        sink: &dyn TransferSink,
    ) -> RemoteResult<u64>;
}

/// No devices (other platforms; tests that don't need any).
pub struct NoDevices;

impl Portable for NoDevices {
    fn devices(&self) -> RemoteResult<Vec<Device>> {
        Ok(Vec::new())
    }
    fn list(&self, _: &str, _: &str) -> RemoteResult<Vec<Object>> {
        Err(RemoteError::NotFound)
    }
    fn stat(&self, _: &str, _: &str) -> RemoteResult<Object> {
        Err(RemoteError::NotFound)
    }
    fn read(
        &self,
        _: &str,
        _: &str,
        _: &mut dyn Write,
        _: u64,
        _: &dyn TransferSink,
    ) -> RemoteResult<u64> {
        Err(RemoteError::NotFound)
    }
}

/// The key of the device with PnP id `pnp_id` and name `name`: the name in lower case letters,
/// digits and dashes, then six hex digits of a hash of the id, so two phones of the same model
/// differ and a location keeps pointing at its phone (`pixel-7-3fa2c1`).
pub fn device_key(name: &str, pnp_id: &str) -> String {
    use sha2::Digest as _;
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let hash = sha2::Sha256::digest(pnp_id.to_ascii_lowercase().as_bytes());
    let tail = hash[..3].iter().fold(String::with_capacity(6), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    if slug.is_empty() {
        format!("device-{tail}")
    } else {
        format!("{slug}-{tail}")
    }
}

/// What the device list knows of a device before anything is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Its PnP device-interface path (what WPD calls the device id).
    pub pnp_id: String,
}

/// Whether a device belongs under the portable devices (#437). Windows also shows every disk
/// volume that has a drive letter (USB sticks, external disks, card-reader media, even floppy
/// slots) to WPD, through its bus enumerator (`SWD#WPDBUSENUM...`, the WPD mass-storage
/// driver); those are drives and already listed as such, which is how File Explorer treats them.
/// Phones and cameras (MTP/PTP) come from the USB stack instead (`USB#VID_...`). So the device
/// itself decides: no names, and nothing is opened.
pub fn is_portable(info: &DeviceInfo) -> bool {
    let id = info.pnp_id.to_ascii_lowercase();
    let id = id.trim_start_matches(['\\', '?']);
    !id.starts_with("swd#wpdbusenum")
}

/// `/a/b` -> `["a", "b"]`; `/` -> `[]`.
pub fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_keys_are_readable_and_tell_twin_phones_apart() {
        let a = device_key("Pixel 7", r"\\?\usb#vid_18d1&pid_4ee1#ABC#{6ac27878}");
        let b = device_key("Pixel 7", r"\\?\usb#vid_18d1&pid_4ee1#XYZ#{6ac27878}");
        assert!(
            a.starts_with("pixel-7-") && a.len() == "pixel-7-".len() + 6,
            "{a}"
        );
        assert_ne!(a, b);
        assert_eq!(
            a,
            device_key("Pixel 7", r"\\?\USB#VID_18D1&PID_4EE1#abc#{6AC27878}"),
            "the id's case doesn't matter"
        );
        assert!(device_key("小米 13", "x").starts_with("13-"));
        assert!(device_key("相機", "x").starts_with("device-"));
        // A valid host for a location.
        let url = format!("mtp://{a}/DCIM");
        assert!(arke_core::remote::RemoteUrl::parse_input(&url).is_some());
    }

    fn info(id: &str) -> DeviceInfo {
        DeviceInfo { pnp_id: id.into() }
    }

    #[test]
    fn disk_volumes_are_not_portable_devices() {
        let iface = "{6ac27878-a6fa-4155-ba85-f98f491d4f33}";
        // An MTP phone.
        assert!(is_portable(&info(&format!(
            r"\\?\usb#vid_18d1&pid_4ee1#0a1b2c#{iface}"
        ))));
        // A camera (PTP) is the same kind of USB device.
        assert!(is_portable(&info(&format!(
            r"\\?\USB#VID_04A9&PID_32B4#5&1f2e3d#{iface}"
        ))));
        // A mass-storage stick with a drive letter, and card-reader media in a slot.
        assert!(!is_portable(&info(&format!(
            r"\\?\swd#wpdbusenum#{{a1b2c3d4-0000-0000-0000-806e6f6e6963}}#0000000000100000#{iface}"
        ))));
        assert!(!is_portable(&info(&format!(
            r"\\?\SWD#WPDBUSENUMROOT#UMB#2&37C186B&0&STORAGE#VOLUME#_??_USBSTOR#{iface}"
        ))));
        // A reader with no media has no volume, hence no WPD device: nothing is listed for it.
        let listed: Vec<_> = [
            info(&format!(r"\\?\usb#vid_18d1&pid_4ee1#0a1b2c#{iface}")),
            info(&format!(
                r"\\?\swd#wpdbusenum#{{b1}}#0000000000100000#{iface}"
            )),
        ]
        .into_iter()
        .filter(is_portable)
        .collect();
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn paths_split_into_names() {
        assert_eq!(segments("/"), Vec::<&str>::new());
        assert_eq!(
            segments("/Internal shared storage/DCIM"),
            ["Internal shared storage", "DCIM"]
        );
    }
}
