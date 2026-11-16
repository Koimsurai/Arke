//! Marks the service process as background work.
//!
//! The service spends nearly all its life waiting on change journals, and its MFT reads are
//! one-off bulk work nobody watches. So, process-wide:
//! - EcoQoS: the scheduler prefers efficiency cores and lower clocks.
//! - Timer resolution requests are ignored (a library asking for 1 ms ticks can't keep the CPU
//!   out of deep idle).
//! - Low memory priority: pages it touches, like a multi-GB MFT read, are the first to leave
//!   memory instead of the user's.

use windows::Win32::System::Threading::{
    GetCurrentProcess, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    ProcessMemoryPriority, ProcessPowerThrottling, SetProcessInformation,
};

/// Power throttling bits the service turns on.
const THROTTLE: u32 =
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;

/// Applies the hints to the current process. Failures (older Windows without EcoQoS) are logged
/// and otherwise ignored: the service works the same, just less frugally.
pub(crate) fn run_as_background() {
    let throttling = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THROTTLE,
        StateMask: THROTTLE,
    };
    // SAFETY: GetCurrentProcess is a pseudo handle that needs no closing; the struct is valid and
    // its exact size is passed.
    let set = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const throttling).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if let Err(err) = set {
        log::info!("EcoQoS not available: {err}");
    }
    let memory = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: MEMORY_PRIORITY_LOW,
    };
    // SAFETY: as above.
    let set = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            (&raw const memory).cast(),
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    };
    if let Err(err) = set {
        log::info!("cannot lower memory priority: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::GetProcessInformation;

    /// Each test runs in its own process under nextest, so changing this one is harmless.
    #[test]
    fn the_process_runs_as_background_work() {
        run_as_background();
        let mut memory = MEMORY_PRIORITY_INFORMATION::default();
        // SAFETY: pseudo handle; the out struct is valid and its exact size is passed.
        unsafe {
            GetProcessInformation(
                GetCurrentProcess(),
                ProcessMemoryPriority,
                (&raw mut memory).cast(),
                size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            )
        }
        .unwrap();
        assert_eq!(memory.MemoryPriority, MEMORY_PRIORITY_LOW);
        let mut throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ..Default::default()
        };
        // SAFETY: as above.
        unsafe {
            GetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw mut throttling).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        }
        .unwrap();
        assert_eq!(throttling.ControlMask & THROTTLE, THROTTLE);
        assert_eq!(throttling.StateMask & THROTTLE, THROTTLE);
    }
}
