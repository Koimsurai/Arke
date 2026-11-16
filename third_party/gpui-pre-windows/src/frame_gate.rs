//! Arke: lets the vsync thread sleep while no window needs frames (see third_party/README.md).
//!
//! Upstream, the `VSyncProvider` thread invalidates every window on every vblank, so an idle app
//! wakes its UI thread at about twice the refresh rate, minimized or with no window at all. GPUI's
//! core already supports frame sources that stop for idle windows: it calls the window's frame
//! waker (and `schedule_frame`) whenever it needs a frame. This is the Windows side of that,
//! modeled on the macOS display link:
//!
//! - A window goes idle after a vsync tick in which GPUI neither presented nor asked for another
//!   frame. The vsync thread skips idle, hidden and minimized windows.
//! - When no window is left to tick, the vsync thread waits on [`VSyncWake`] (or on the device's
//!   removed event) instead of `DwmFlush`.
//! - Any frame demand ([`WindowFrames::wake`]) marks the window busy again and wakes the thread.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, Result};
use gpui_util::ResultExt as _;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Graphics::Direct3D11::{ID3D11Device, ID3D11Device4},
        System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects},
    },
    core::Interface as _,
};

/// An auto-reset event the vsync thread sleeps on while every window is idle. One per platform,
/// shared with each window; it outlives vsync threads, which come and go with the GPU.
pub(crate) struct VSyncWake {
    event: HANDLE,
}

// SAFETY: an event handle may be signaled and waited on from any thread.
unsafe impl Send for VSyncWake {}
unsafe impl Sync for VSyncWake {}

impl VSyncWake {
    pub(crate) fn new() -> Result<Arc<Self>> {
        let event = unsafe { CreateEventW(None, false, false, None) }.context("CreateEventW")?;
        Ok(Arc::new(Self { event }))
    }

    /// Wakes the vsync thread if it is waiting, or makes its next wait return at once.
    pub(crate) fn notify(&self) {
        unsafe { SetEvent(self.event) }.log_err();
    }

    /// Clears a pending wake. The vsync thread calls this before it looks at the windows, so a
    /// wake that comes after the look still ends the wait that follows.
    pub(crate) fn reset(&self) {
        unsafe { ResetEvent(self.event) }.log_err();
    }

    /// Sleeps until [`Self::notify`], or until `device_removed` (if any) is signaled.
    pub(crate) fn wait(&self, device_removed: Option<&DeviceRemovedEvent>) -> WakeReason {
        let mut handles = vec![self.event];
        handles.extend(device_removed.map(|d| d.event));
        let result = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if result.0 == WAIT_OBJECT_0.0 + 1 {
            WakeReason::DeviceRemoved
        } else {
            WakeReason::Notified
        }
    }
}

impl Drop for VSyncWake {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.event) }.log_err();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeReason {
    Notified,
    DeviceRemoved,
}

/// Signaled by the driver when a D3D11 device is removed, so a sleeping vsync thread still
/// notices a lost device (it checks for that only when it runs).
pub(crate) struct DeviceRemovedEvent {
    device: ID3D11Device4,
    event: HANDLE,
    cookie: u32,
}

impl DeviceRemovedEvent {
    /// `None` when the driver doesn't support it (before Windows 10 1703): the vsync thread then
    /// notices device loss on its next frame, as upstream does.
    pub(crate) fn register(device: &ID3D11Device) -> Option<Self> {
        let device = device.cast::<ID3D11Device4>().ok()?;
        let event = unsafe { CreateEventW(None, false, false, None) }.log_err()?;
        match unsafe { device.RegisterDeviceRemovedEvent(event) } {
            Ok(cookie) => Some(Self {
                device,
                event,
                cookie,
            }),
            Err(error) => {
                log::warn!("RegisterDeviceRemovedEvent: {error}");
                unsafe { CloseHandle(event) }.log_err();
                None
            }
        }
    }
}

impl Drop for DeviceRemovedEvent {
    fn drop(&mut self) {
        unsafe {
            self.device.UnregisterDeviceRemoved(self.cookie);
            CloseHandle(self.event).log_err();
        }
    }
}

/// One window's frame demand, written on the UI thread and read by the vsync thread.
pub(crate) struct WindowFrames {
    /// Nothing asked for a frame during the last tick: the vsync thread leaves the window alone.
    idle: AtomicBool,
    /// GPUI asked for a frame, or the window presented, since the current tick began.
    woken: AtomicBool,
    vsync: Arc<VSyncWake>,
}

impl WindowFrames {
    /// A new window starts busy, so its first frames are drawn.
    pub(crate) fn new(vsync: Arc<VSyncWake>) -> Self {
        Self {
            idle: AtomicBool::new(false),
            woken: AtomicBool::new(false),
            vsync,
        }
    }

    /// Frame demand: keep (or start) ticking this window.
    pub(crate) fn wake(&self) {
        self.woken.store(true, Ordering::Relaxed);
        self.idle.store(false, Ordering::Release);
        self.vsync.notify();
    }

    /// Called before GPUI handles a frame request.
    pub(crate) fn begin_frame(&self) {
        self.woken.store(false, Ordering::Relaxed);
    }

    /// Called after GPUI handled a frame request: goes idle unless GPUI presented or asked for
    /// another frame meanwhile. Returns whether it went idle.
    pub(crate) fn end_frame(&self) -> bool {
        let idle = !self.woken.load(Ordering::Relaxed);
        if idle {
            self.idle.store(true, Ordering::Release);
        }
        idle
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.idle.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn frames() -> WindowFrames {
        WindowFrames::new(VSyncWake::new().unwrap())
    }

    #[test]
    fn a_new_window_ticks_until_a_frame_without_demand() {
        let frames = frames();
        assert!(!frames.is_idle());
        frames.begin_frame();
        assert!(frames.end_frame());
        assert!(frames.is_idle());
    }

    #[test]
    fn presenting_or_asking_for_a_frame_keeps_the_window_ticking() {
        let frames = frames();
        frames.begin_frame();
        frames.wake();
        assert!(!frames.end_frame());
        assert!(!frames.is_idle());
        // The tick after that has no demand: idle.
        frames.begin_frame();
        assert!(frames.end_frame());
    }

    #[test]
    fn demand_between_ticks_wakes_an_idle_window() {
        let frames = frames();
        frames.begin_frame();
        frames.end_frame();
        assert!(frames.is_idle());
        frames.wake();
        assert!(!frames.is_idle());
    }

    #[test]
    fn a_wake_after_reset_ends_the_wait() {
        let vsync = VSyncWake::new().unwrap();
        vsync.notify();
        vsync.reset();
        let frames = WindowFrames::new(vsync.clone());
        let waiter = std::thread::spawn({
            let vsync = vsync.clone();
            move || {
                let started = Instant::now();
                (vsync.wait(None), started.elapsed())
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        frames.wake();
        let (reason, waited) = waiter.join().unwrap();
        assert_eq!(reason, WakeReason::Notified);
        assert!(waited >= Duration::from_millis(40), "{waited:?}");
    }

    #[test]
    fn a_wake_before_the_wait_is_not_lost() {
        let vsync = VSyncWake::new().unwrap();
        vsync.reset();
        vsync.notify();
        assert_eq!(vsync.wait(None), WakeReason::Notified);
    }
}
