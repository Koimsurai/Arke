//! Running in the background with no window (tray only, or autostart at sign-in): Arke gives back
//! what it doesn't need until a window opens again.
//!
//! - Frees its own caches: decoded thumbnails and icons, and the in-memory compressed thumbnails.
//!   The disk cache stays, so the next window gets its thumbnails back without decoding again.
//! - Lowers the process's memory priority, so Windows trims its pages first under pressure, and
//!   turns on EcoQoS (power throttling), so what still runs (index updates) uses efficiency
//!   cores at low clocks.
//!
//! It doesn't force the working set out (`EmptyWorkingSet`): that only pages it out, and the next
//! window would page it back in. The GPU device stays too: making it again costs 0.2–0.6 s, and
//! with no window the vsync thread already sleeps (third_party/README.md).

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::App;

use crate::workspace::Workspace;

static RESIDENT: AtomicBool = AtomicBool::new(false);

/// Enters background mode when the last window closes, and leaves it when one opens.
pub(crate) fn watch(cx: &mut App) {
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            enter(cx);
        }
    })
    .detach();
    cx.observe_new(|_: &mut Workspace, _, _| leave()).detach();
}

/// No window is open: free caches and lower the process's priority.
pub(crate) fn enter(cx: &mut App) {
    if RESIDENT.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::media::MediaStore::release_all(cx);
    set_background_priority(true);
    log::info!("no window open: freed caches, lowered memory priority, EcoQoS on");
}

/// A window opens: back to normal priority. Caches refill as the window asks for them.
pub(crate) fn leave() {
    if RESIDENT.swap(false, Ordering::AcqRel) {
        set_background_priority(false);
        log::info!("window open: normal priority");
    }
}

/// Whether Arke is in background mode (tests).
#[cfg(test)]
pub(crate) fn is_resident() -> bool {
    RESIDENT.load(Ordering::Acquire)
}

/// Memory priority low (or normal) and EcoQoS on (or left to Windows) for the whole process.
pub(crate) fn set_background_priority(background: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };
    set_memory_priority_low(background);
    // Control 0 with state 0 hands the decision back to Windows (its default).
    let throttle = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: if background {
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED
        } else {
            0
        },
        StateMask: if background {
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED
        } else {
            0
        },
    };
    // SAFETY: GetCurrentProcess is a pseudo handle that needs no closing; the structure is the
    // one its information class takes, passed with its size.
    unsafe {
        if let Err(err) = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const throttle).cast(),
            u32::try_from(size_of::<PROCESS_POWER_THROTTLING_STATE>()).unwrap_or(12),
        ) {
            log::debug!("could not set power throttling: {err}");
        }
    }
}

/// Memory priority low (or normal) for the whole process: under memory pressure Windows takes
/// low-priority pages first. Costs nothing while memory is plentiful.
pub(crate) fn set_memory_priority_low(low: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW,
        MEMORY_PRIORITY_NORMAL, ProcessMemoryPriority, SetProcessInformation,
    };
    let memory = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: if low {
            MEMORY_PRIORITY_LOW
        } else {
            MEMORY_PRIORITY_NORMAL
        },
    };
    // SAFETY: GetCurrentProcess is a pseudo handle that needs no closing; the structure is the
    // one its information class takes, passed with its size.
    if let Err(err) = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            (&raw const memory).cast(),
            u32::try_from(size_of::<MEMORY_PRIORITY_INFORMATION>()).unwrap_or(4),
        )
    } {
        log::debug!("could not set memory priority: {err}");
    }
}
