//! Live directory watchers, grouped by volume.
//!
//! A watcher keeps a directory handle open on its drive, and an open handle makes Windows refuse
//! to eject or lock the drive ("This device is currently in use"). The app listens for removal
//! and lock requests (`win_devnotify.rs`) and calls [`release_volume`] before answering, which
//! closes every watcher on that drive and waits until their handles are gone. If the request is
//! cancelled, [`rearm_volume`] opens them again and asks each owner to rescan, since changes made
//! in between were missed. The owners keep their [`WatchGuard`] the whole time and never notice.

use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, Prefix};
use std::sync::{Arc, LazyLock, Weak};

use parking_lot::Mutex;

/// The OS side of one watch. Dropping it stops watching without waiting.
pub trait Armed: Send {
    /// Stops watching and returns once the directory handle is closed (or a short timeout passed).
    fn close(self: Box<Self>);
}

/// Opens the OS side of a watch.
pub type ArmFn = Box<dyn Fn() -> io::Result<Box<dyn Armed>> + Send + Sync>;
/// Told after a watch was reopened: changes may have been missed.
pub type RescanFn = Box<dyn Fn() + Send + Sync>;

struct Slot {
    volume: Option<String>,
    arm: ArmFn,
    rescan: RescanFn,
    armed: Mutex<Option<Box<dyn Armed>>>,
}

/// Keeps a watch registered; dropping it stops the watch.
pub struct WatchGuard {
    slot: Arc<Slot>,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        // Plain drop: the owner may be the UI thread, which must not wait for the handle.
        drop(self.slot.armed.lock().take());
    }
}

/// Watches by volume. One per process ([`registry`]); tests make their own.
#[derive(Default)]
pub struct Registry {
    slots: Mutex<Vec<Weak<Slot>>>,
    /// Volumes whose watchers are closed until [`Registry::rearm`] or [`Registry::forget`].
    released: Mutex<HashSet<String>>,
}

impl Registry {
    /// Registers a watch on `path` and opens it, unless its volume is released right now (then it
    /// opens when the volume is rearmed).
    ///
    /// # Errors
    /// Whatever `arm` returns when opening fails.
    pub fn watch(&self, path: &Path, arm: ArmFn, rescan: RescanFn) -> io::Result<WatchGuard> {
        let volume = volume_key(path);
        let held = volume
            .as_ref()
            .is_some_and(|v| self.released.lock().contains(v));
        let armed = if held { None } else { Some(arm()?) };
        let slot = Arc::new(Slot {
            volume,
            arm,
            rescan,
            armed: Mutex::new(armed),
        });
        let mut slots = self.slots.lock();
        slots.retain(|s| s.strong_count() > 0);
        slots.push(Arc::downgrade(&slot));
        drop(slots);
        Ok(WatchGuard { slot })
    }

    fn on_volume(&self, volume: &str) -> Vec<Arc<Slot>> {
        self.slots
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|s| s.volume.as_deref() == Some(volume))
            .collect()
    }

    /// Closes every watch on `volume` (a [`volume_key`]) and waits until their handles are
    /// released. New watches on it stay closed until [`Registry::rearm`]. Returns how many closed.
    pub fn release(&self, volume: &str) -> usize {
        self.released.lock().insert(volume.to_owned());
        let mut closed = 0;
        for slot in self.on_volume(volume) {
            let armed = slot.armed.lock().take();
            if let Some(armed) = armed {
                armed.close();
                closed += 1;
            }
        }
        closed
    }

    /// The removal or lock was cancelled: reopens the watches on `volume` and asks their owners to
    /// rescan. Watches that can't be reopened stay closed. Returns how many reopened.
    pub fn rearm(&self, volume: &str) -> usize {
        if !self.released.lock().remove(volume) {
            return 0;
        }
        let mut opened = 0;
        for slot in self.on_volume(volume) {
            let mut armed = slot.armed.lock();
            if armed.is_some() {
                continue;
            }
            match (slot.arm)() {
                Ok(watch) => {
                    *armed = Some(watch);
                    drop(armed);
                    (slot.rescan)();
                    opened += 1;
                }
                Err(err) => log::info!("watch on {volume} not reopened: {err}"),
            }
        }
        opened
    }

    /// The volume is gone: its watches stay closed (their owners move elsewhere), and a new drive
    /// that later gets the same letter is watched normally.
    pub fn forget(&self, volume: &str) {
        self.released.lock().remove(volume);
    }

    /// Whether `volume` is released right now.
    pub fn is_released(&self, volume: &str) -> bool {
        self.released.lock().contains(volume)
    }

    /// Open watches on `volume`.
    pub fn open_on(&self, volume: &str) -> usize {
        self.on_volume(volume)
            .iter()
            .filter(|s| s.armed.lock().is_some())
            .count()
    }
}

/// The registry every [`crate::fs::RealFs`] watch goes through.
pub fn registry() -> &'static Registry {
    static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::default);
    &REGISTRY
}

/// [`Registry::release`] on the process registry, for the drive holding `root`.
pub fn release_volume(root: &Path) -> usize {
    volume_key(root).map_or(0, |v| registry().release(&v))
}

/// [`Registry::rearm`] on the process registry, for the drive holding `root`.
pub fn rearm_volume(root: &Path) -> usize {
    volume_key(root).map_or(0, |v| registry().rearm(&v))
}

/// [`Registry::forget`] on the process registry, for the drive holding `root`.
pub fn forget_volume(root: &Path) {
    if let Some(v) = volume_key(root) {
        registry().forget(&v);
    }
}

/// Which volume a path is on: `E:` for drive letters (any case, `\\?\` too), `\\server\share`
/// (lowercase) for network shares. `None` for relative paths.
pub fn volume_key(path: &Path) -> Option<String> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    Some(match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            format!("{}:", char::from(letter).to_ascii_uppercase())
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            r"\\{}\{}",
            server.to_string_lossy().to_lowercase(),
            share.to_string_lossy().to_lowercase()
        ),
        Prefix::DeviceNS(name) => name.to_string_lossy().to_uppercase(),
        Prefix::Verbatim(name) => name.to_string_lossy().to_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counts {
        opened: AtomicUsize,
        closed: AtomicUsize,
        rescans: AtomicUsize,
    }

    struct Fake(Arc<Counts>);

    impl Armed for Fake {
        fn close(self: Box<Self>) {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn watch(registry: &Registry, path: &str, counts: &Arc<Counts>) -> WatchGuard {
        let (arm, rescan) = (counts.clone(), counts.clone());
        registry
            .watch(
                Path::new(path),
                Box::new(move || {
                    arm.opened.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(Fake(arm.clone())) as Box<dyn Armed>)
                }),
                Box::new(move || {
                    rescan.rescans.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap()
    }

    #[test]
    fn volume_keys_ignore_case_and_verbatim_prefixes() {
        assert_eq!(volume_key(Path::new(r"e:\photos")).as_deref(), Some("E:"));
        assert_eq!(
            volume_key(Path::new(r"\\?\E:\photos")).as_deref(),
            Some("E:")
        );
        assert_eq!(volume_key(Path::new(r"E:\")).as_deref(), Some("E:"));
        assert_eq!(
            volume_key(Path::new(r"\\NAS\Share\docs")).as_deref(),
            Some(r"\\nas\share")
        );
        assert_eq!(volume_key(Path::new(r"relative\path")), None);
    }

    #[test]
    fn releasing_a_volume_closes_only_its_watches_and_rearming_reopens_them() {
        let registry = Registry::default();
        let usb = Arc::new(Counts::default());
        let system = Arc::new(Counts::default());
        let _a = watch(&registry, r"E:\photos", &usb);
        let _b = watch(&registry, r"e:\music", &usb);
        let _c = watch(&registry, r"C:\Users", &system);
        assert_eq!(usb.opened.load(Ordering::SeqCst), 2);

        assert_eq!(registry.release("E:"), 2);
        assert_eq!(usb.closed.load(Ordering::SeqCst), 2);
        assert_eq!(registry.open_on("E:"), 0);
        assert_eq!(
            system.closed.load(Ordering::SeqCst),
            0,
            "other drives keep theirs"
        );
        assert_eq!(registry.open_on("C:"), 1);

        // Released twice (lock, then removal): nothing left to close.
        assert_eq!(registry.release("E:"), 0);

        assert_eq!(registry.rearm("E:"), 2);
        assert_eq!(usb.opened.load(Ordering::SeqCst), 4);
        assert_eq!(
            usb.rescans.load(Ordering::SeqCst),
            2,
            "missed changes are rescanned"
        );
        assert_eq!(registry.open_on("E:"), 2);
        assert_eq!(registry.rearm("E:"), 0, "already armed");
    }

    #[test]
    fn watches_started_while_released_wait_for_rearm() {
        let registry = Registry::default();
        let counts = Arc::new(Counts::default());
        registry.release("F:");
        let _w = watch(&registry, r"F:\new", &counts);
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        registry.rearm("F:");
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_removed_volume_is_not_reopened_and_dropped_watches_are_gone() {
        let registry = Registry::default();
        let counts = Arc::new(Counts::default());
        let kept = watch(&registry, r"G:\a", &counts);
        let dropped = watch(&registry, r"G:\b", &counts);
        drop(dropped);
        assert_eq!(
            registry.release("G:"),
            1,
            "the dropped watch is not tracked"
        );
        registry.forget("G:");
        assert!(!registry.is_released("G:"));
        assert_eq!(
            registry.rearm("G:"),
            0,
            "removal completed: nothing to reopen"
        );
        assert_eq!(registry.open_on("G:"), 0);
        // A new drive with the same letter is watched normally.
        let _fresh = watch(&registry, r"G:\c", &counts);
        assert_eq!(registry.open_on("G:"), 1);
        drop(kept);
    }
}
