//! Lets drives be ejected, locked or dismounted from outside Arke (the taskbar's "Safely Remove
//! Hardware", another app, VeraCrypt, chkdsk) while Arke has folders on them open.
//!
//! Arke's directory watchers keep a handle on their folder, and Windows refuses to remove or lock
//! a volume with open handles. A hidden window on its own thread holds a query-only handle to the
//! root of every local drive and registers it for device notifications. Before a removal
//! (`DBT_DEVICEQUERYREMOVE`) or a lock (`GUID_IO_VOLUME_LOCK`) goes ahead, Windows asks that
//! window first: it closes its own handle and every watcher on the drive
//! (`arke_core::watches::release_volume`, which waits until the handles are gone) before
//! answering, and tells the UI to move tabs on that drive to the home folder like Arke's own
//! eject does. If the removal or lock is cancelled, everything is reopened and rescanned.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDriveTypeW, GetLogicalDrives, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DBT_CUSTOMEVENT, DBT_DEVICEARRIVAL, DBT_DEVICEQUERYREMOVE,
    DBT_DEVICEQUERYREMOVEFAILED, DBT_DEVICEREMOVECOMPLETE, DBT_DEVICEREMOVEPENDING,
    DBT_DEVTYP_HANDLE, DBT_DEVTYP_VOLUME, DEV_BROADCAST_HANDLE, DEV_BROADCAST_HDR,
    DEV_BROADCAST_VOLUME, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DispatchMessageW,
    GUID_IO_VOLUME_DISMOUNT, GUID_IO_VOLUME_DISMOUNT_FAILED, GUID_IO_VOLUME_LOCK,
    GUID_IO_VOLUME_LOCK_FAILED, GUID_IO_VOLUME_MOUNT, GUID_IO_VOLUME_UNLOCK, GetMessageW,
    HDEVNOTIFY, MSG, RegisterClassW, RegisterDeviceNotificationW, UnregisterDeviceNotification,
    WM_DEVICECHANGE, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_CDROM: u32 = 5;

/// Sent to the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DriveEvent {
    /// The drive is being removed or dismounted: tabs showing it should move away.
    Leaving(PathBuf),
}

/// A device message about one volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DevEvent {
    QueryRemove,
    QueryRemoveFailed,
    /// Removal pending or complete: the device is gone.
    Removed,
    Lock,
    LockFailed,
    Unlock,
    Dismount,
    DismountFailed,
    Mount,
}

/// Where a watched volume stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VolState {
    /// Handles open, watching.
    Open,
    /// Someone is locking it (chkdsk, format, VeraCrypt): handles closed until unlocked.
    Locked,
    /// Windows asked to remove it: handles closed until it is gone or the removal fails.
    Removing,
    /// Dismounted: handles closed; reopening now would mount it again, so wait for a mount.
    Dismounted,
    /// The device is gone.
    Gone,
}

/// What to do for an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Close the notification handle and every watcher on the volume.
    Release,
    /// Move tabs on the volume to the home folder.
    LeaveTabs,
    /// Reopen the notification handle and the watchers.
    Rearm,
    /// Unregister and forget the volume.
    Forget,
}

/// The state machine behind the window procedure, kept pure so it can be tested.
pub(crate) const fn next(state: VolState, event: DevEvent) -> (VolState, &'static [Step]) {
    use DevEvent as E;
    use Step::{Forget, LeaveTabs, Rearm, Release};
    use VolState as S;
    match (state, event) {
        (S::Gone, _) => (S::Gone, &[]),
        (_, E::Removed) => (S::Gone, &[Forget]),
        (S::Open, E::QueryRemove) => (S::Removing, &[Release, LeaveTabs]),
        (S::Locked | S::Dismounted, E::QueryRemove) => (S::Removing, &[LeaveTabs]),
        (S::Open, E::Lock) => (S::Locked, &[Release]),
        (S::Open, E::Dismount) => (S::Dismounted, &[Release, LeaveTabs]),
        (S::Locked, E::Dismount) => (S::Dismounted, &[LeaveTabs]),
        (S::Locked, E::LockFailed | E::Unlock)
        | (S::Removing, E::QueryRemoveFailed)
        | (S::Dismounted, E::Mount | E::DismountFailed) => (S::Open, &[Rearm]),
        (state, _) => (state, &[]),
    }
}

/// Drive letters in a `GetLogicalDrives` / `dbcv_unitmask` bit mask.
pub(crate) fn letters(mask: u32) -> impl Iterator<Item = char> {
    (0u8..26)
        .filter(move |bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit))
}

/// Maps a custom volume event to a [`DevEvent`].
fn custom_event(guid: windows::core::GUID) -> Option<DevEvent> {
    Some(match guid {
        g if g == GUID_IO_VOLUME_LOCK => DevEvent::Lock,
        g if g == GUID_IO_VOLUME_LOCK_FAILED => DevEvent::LockFailed,
        g if g == GUID_IO_VOLUME_UNLOCK => DevEvent::Unlock,
        g if g == GUID_IO_VOLUME_DISMOUNT => DevEvent::Dismount,
        g if g == GUID_IO_VOLUME_DISMOUNT_FAILED => DevEvent::DismountFailed,
        g if g == GUID_IO_VOLUME_MOUNT => DevEvent::Mount,
        _ => return None,
    })
}

fn root(letter: char) -> PathBuf {
    PathBuf::from(format!("{letter}:\\"))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Volume {
    handle: Option<HANDLE>,
    notify: Option<HDEVNOTIFY>,
    state: VolState,
}

struct Watcher {
    hwnd: HWND,
    volumes: HashMap<char, Volume>,
    events: flume::Sender<DriveEvent>,
}

thread_local! {
    static WATCHER: RefCell<Option<Watcher>> = const { RefCell::new(None) };
}

impl Watcher {
    /// Whether this drive is worth watching: local, not the system drive.
    fn wanted(letter: char) -> bool {
        let system = std::env::var("SystemDrive")
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or('C');
        if letter.eq_ignore_ascii_case(&system) {
            return false;
        }
        let path = wide(&format!("{letter}:\\"));
        // SAFETY: path is NUL-terminated; only reads mount information.
        let kind = unsafe { GetDriveTypeW(PCWSTR(path.as_ptr())) };
        matches!(kind, DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_CDROM)
    }

    /// Opens a query-only handle to the drive root and registers it. `None` when the drive has
    /// no medium or can't be opened.
    fn open(&self, letter: char) -> Option<(HANDLE, HDEVNOTIFY)> {
        let path = wide(&format!("{letter}:\\"));
        // SAFETY: path is NUL-terminated; no access rights are asked for, so this neither needs
        // admin rights nor reads anything.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .ok()?;
        let filter = DEV_BROADCAST_HANDLE {
            dbch_size: u32::try_from(size_of::<DEV_BROADCAST_HANDLE>()).unwrap_or(0),
            dbch_devicetype: DBT_DEVTYP_HANDLE.0,
            dbch_handle: handle,
            ..DEV_BROADCAST_HANDLE::default()
        };
        // SAFETY: hwnd is this thread's window; filter is a valid DEV_BROADCAST_HANDLE that lives
        // through the call.
        let notify = unsafe {
            RegisterDeviceNotificationW(
                HANDLE(self.hwnd.0),
                (&raw const filter).cast(),
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        };
        match notify {
            Ok(notify) => Some((handle, notify)),
            Err(err) => {
                log::info!("{letter}: not registered for removal notices: {err}");
                // SAFETY: handle was opened above and is not used elsewhere.
                let _ = unsafe { CloseHandle(handle) };
                None
            }
        }
    }

    fn watch(&mut self, letter: char) {
        if self.volumes.contains_key(&letter) || !Self::wanted(letter) {
            return;
        }
        if let Some((handle, notify)) = self.open(letter) {
            self.volumes.insert(
                letter,
                Volume {
                    handle: Some(handle),
                    notify: Some(notify),
                    state: VolState::Open,
                },
            );
        }
    }

    fn letter_of(&self, notify: HDEVNOTIFY) -> Option<char> {
        self.volumes
            .iter()
            .find(|(_, v)| v.notify == Some(notify))
            .map(|(letter, _)| *letter)
    }

    fn apply(&mut self, letter: char, event: DevEvent) {
        let Some(volume) = self.volumes.get_mut(&letter) else {
            return;
        };
        let (state, steps) = next(volume.state, event);
        volume.state = state;
        log::info!("{letter}: {event:?} -> {state:?}");
        for step in steps {
            match step {
                Step::Release => {
                    if let Some(handle) = volume.handle.take() {
                        // SAFETY: our own handle, closed once.
                        let _ = unsafe { CloseHandle(handle) };
                    }
                    let closed = arke_core::watches::release_volume(&root(letter));
                    log::info!("{letter}: closed {closed} watchers");
                }
                Step::LeaveTabs => {
                    let _ = self.events.send(DriveEvent::Leaving(root(letter)));
                }
                Step::Rearm => {
                    if let Some(notify) = volume.notify.take() {
                        // SAFETY: registered by `open` and not yet unregistered.
                        let _ = unsafe { UnregisterDeviceNotification(notify) };
                    }
                    let opened = arke_core::watches::rearm_volume(&root(letter));
                    log::info!("{letter}: reopened {opened} watchers");
                }
                Step::Forget => {}
            }
        }
        // Reopening needs `self` (the window), so it happens after the volume borrow ends.
        if steps.contains(&Step::Rearm) {
            match self.open(letter) {
                Some((handle, notify)) => {
                    if let Some(volume) = self.volumes.get_mut(&letter) {
                        volume.handle = Some(handle);
                        volume.notify = Some(notify);
                    }
                }
                None => {
                    self.volumes.remove(&letter);
                }
            }
        }
        if steps.contains(&Step::Forget) {
            self.forget(letter);
        }
    }

    fn forget(&mut self, letter: char) {
        if let Some(volume) = self.volumes.remove(&letter) {
            if let Some(notify) = volume.notify {
                // SAFETY: registered by `open` and not yet unregistered.
                let _ = unsafe { UnregisterDeviceNotification(notify) };
            }
            if let Some(handle) = volume.handle {
                // SAFETY: our own handle, closed once.
                let _ = unsafe { CloseHandle(handle) };
            }
        }
        arke_core::watches::forget_volume(&root(letter));
    }
}

/// `WM_DEVICECHANGE`: broadcasts about drive letters, and notices on our volume handles.
fn on_device_change(wparam: WPARAM, lparam: LPARAM) {
    if lparam.0 == 0 {
        return;
    }
    let Ok(kind) = u32::try_from(wparam.0) else {
        return;
    };
    // SAFETY: for these events lparam points at a DEV_BROADCAST_HDR from Windows, valid for the
    // duration of the message.
    let header = unsafe { &*(lparam.0 as *const DEV_BROADCAST_HDR) };
    WATCHER.with(|cell| {
        // A notice sent while one is being handled (shouldn't happen) is skipped, not a panic.
        let Ok(mut watcher) = cell.try_borrow_mut() else {
            return;
        };
        let Some(watcher) = watcher.as_mut() else {
            return;
        };
        if header.dbch_devicetype == DBT_DEVTYP_VOLUME {
            // SAFETY: the header says this is a DEV_BROADCAST_VOLUME.
            let volume = unsafe { &*(lparam.0 as *const DEV_BROADCAST_VOLUME) };
            for letter in letters(volume.dbcv_unitmask) {
                match kind {
                    DBT_DEVICEARRIVAL => watcher.watch(letter),
                    DBT_DEVICEREMOVECOMPLETE => watcher.apply(letter, DevEvent::Removed),
                    _ => {}
                }
            }
        } else if header.dbch_devicetype == DBT_DEVTYP_HANDLE {
            // SAFETY: the header says this is a DEV_BROADCAST_HANDLE.
            let notice = unsafe { &*(lparam.0 as *const DEV_BROADCAST_HANDLE) };
            let event = match kind {
                DBT_DEVICEQUERYREMOVE => Some(DevEvent::QueryRemove),
                DBT_DEVICEQUERYREMOVEFAILED => Some(DevEvent::QueryRemoveFailed),
                DBT_DEVICEREMOVEPENDING | DBT_DEVICEREMOVECOMPLETE => Some(DevEvent::Removed),
                DBT_CUSTOMEVENT => custom_event(notice.dbch_eventguid),
                _ => None,
            };
            if let (Some(event), Some(letter)) = (event, watcher.letter_of(notice.dbch_hdevnotify))
            {
                watcher.apply(letter, event);
            }
        }
    });
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DEVICECHANGE {
        on_device_change(wparam, lparam);
        // TRUE: never deny a removal (BROADCAST_QUERY_DENY); Arke only gets out of the way.
        return LRESULT(1);
    }
    // SAFETY: default handling for every other message of our own window.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Starts the device notification thread. Drive events arrive on the returned channel.
pub(crate) fn start() -> flume::Receiver<DriveEvent> {
    let (tx, rx) = flume::unbounded();
    crate::explorer::spawn_thread("device-notify", move || {
        if let Err(err) = run(tx) {
            log::warn!("removal notices unavailable: {err}");
        }
    });
    rx
}

fn run(events: flume::Sender<DriveEvent>) -> windows::core::Result<()> {
    let class = w!("ArkeDeviceNotify");
    // SAFETY: module handle of this exe; the class struct and its strings outlive the call.
    let instance = unsafe { GetModuleHandleW(None) }?;
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..WNDCLASSW::default()
    };
    // SAFETY: wc is fully initialized.
    if unsafe { RegisterClassW(&raw const wc) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    // A hidden top-level window (not message-only): drive arrival broadcasts only reach top-level
    // windows. Never shown, and a tool window, so it stays out of the taskbar and Alt+Tab.
    // SAFETY: the class was registered above on this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("Arke device notifications"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;
    let mut watcher = Watcher {
        hwnd,
        volumes: HashMap::new(),
        events,
    };
    // SAFETY: only reads which drive letters exist.
    for letter in letters(unsafe { GetLogicalDrives() }) {
        watcher.watch(letter);
    }
    log::info!("watching {} drives for removal", watcher.volumes.len());
    WATCHER.set(Some(watcher));
    let mut msg = MSG::default();
    // SAFETY: standard message loop for this thread's window.
    while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
        // SAFETY: msg was filled in by GetMessageW.
        unsafe { DispatchMessageW(&raw const msg) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use DevEvent as E;
    use VolState as S;

    /// Runs events from `Open`, collecting the steps taken.
    fn run(events: &[DevEvent]) -> (VolState, Vec<Step>) {
        let mut state = S::Open;
        let mut steps = Vec::new();
        for event in events {
            let (to, taken) = next(state, *event);
            state = to;
            steps.extend_from_slice(taken);
        }
        (state, steps)
    }

    #[test]
    fn removal_closes_handles_and_moves_tabs_then_forgets() {
        assert_eq!(
            run(&[E::QueryRemove, E::Removed]),
            (S::Gone, vec![Step::Release, Step::LeaveTabs, Step::Forget])
        );
        // Nothing more after the device is gone.
        assert_eq!(run(&[E::Removed, E::Mount]), (S::Gone, vec![Step::Forget]));
    }

    #[test]
    fn a_cancelled_removal_rearms() {
        assert_eq!(
            run(&[E::QueryRemove, E::QueryRemoveFailed]),
            (S::Open, vec![Step::Release, Step::LeaveTabs, Step::Rearm])
        );
    }

    #[test]
    fn a_lock_only_closes_handles_until_unlocked() {
        assert_eq!(
            run(&[E::Lock, E::Unlock]),
            (S::Open, vec![Step::Release, Step::Rearm])
        );
        assert_eq!(
            run(&[E::Lock, E::LockFailed]),
            (S::Open, vec![Step::Release, Step::Rearm])
        );
        // Locked and then removed: handles are already closed, tabs still move.
        assert_eq!(
            run(&[E::Lock, E::QueryRemove]),
            (S::Removing, vec![Step::Release, Step::LeaveTabs])
        );
    }

    /// VeraCrypt and format: lock, dismount, unlock. Reopening after a dismount would mount the
    /// volume again, so only a mount (or a failed dismount) rearms.
    #[test]
    fn nothing_reopens_a_dismounted_volume_until_it_mounts() {
        assert_eq!(
            run(&[E::Lock, E::Dismount, E::Unlock]),
            (S::Dismounted, vec![Step::Release, Step::LeaveTabs])
        );
        assert_eq!(
            run(&[E::Dismount, E::Mount]),
            (S::Open, vec![Step::Release, Step::LeaveTabs, Step::Rearm])
        );
        assert_eq!(
            run(&[E::Dismount, E::DismountFailed]).1.last(),
            Some(&Step::Rearm)
        );
    }

    #[test]
    fn stray_events_change_nothing() {
        assert_eq!(
            run(&[E::Unlock, E::QueryRemoveFailed, E::Mount]),
            (S::Open, vec![])
        );
    }

    #[test]
    fn unit_masks_name_drive_letters() {
        assert_eq!(letters(0b1_0001).collect::<String>(), "AE");
        assert_eq!(letters(1 << 25).collect::<String>(), "Z");
        assert_eq!(letters(0).count(), 0);
    }
}
