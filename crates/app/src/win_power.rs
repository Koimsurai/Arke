//! Whether Windows asks programs to save energy (battery saver, or Energy Saver on Windows 11
//! 24H2 and later, which also turns on while plugged in).
//!
//! Windows 10 1809 and later report an effective power mode
//! (`PowerRegisterForEffectivePowerModeNotifications`). Arke registers once and Windows calls back
//! on every change, so a check is one atomic load and always current. Older Windows, without that
//! export in powrprof.dll, fall back to the battery saver flag of `GetSystemPowerStatus`, read on
//! each check.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

/// `EFFECTIVE_POWER_MODE_V2` (Windows 10 1903 and later) and `_V1`.
const VERSIONS: [u32; 2] = [2, 1];
/// `EffectivePowerModeBatterySaver`.
const BATTERY_SAVER: i32 = 0;
/// No mode reported yet.
const UNKNOWN: i32 = -1;

/// The last mode Windows reported.
static MODE: AtomicI32 = AtomicI32::new(UNKNOWN);

/// Whether Windows is saving energy now. Cheap; any thread.
pub(crate) fn saving_energy() -> bool {
    listen();
    decide(reported(), legacy_saver_on)
}

fn reported() -> Option<i32> {
    let mode = MODE.load(Ordering::Relaxed);
    (mode != UNKNOWN).then_some(mode)
}

/// The effective mode once Windows has reported one; until then (or on Windows without it) the
/// battery saver flag.
fn decide(mode: Option<i32>, legacy: impl FnOnce() -> bool) -> bool {
    mode.map_or_else(legacy, |mode| mode == BATTERY_SAVER)
}

/// Called by Windows right after registering and on every change, on a thread of its own.
unsafe extern "system" fn on_mode(mode: i32, _context: *const c_void) {
    if MODE.swap(mode, Ordering::Relaxed) != mode {
        log::info!("effective power mode: {mode}");
    }
}

/// Registers for mode changes once; false where Windows doesn't offer it.
fn listen() -> bool {
    static LISTENING: OnceLock<bool> = OnceLock::new();
    *LISTENING.get_or_init(register)
}

fn register() -> bool {
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    use windows::core::{HRESULT, s, w};
    type Callback = unsafe extern "system" fn(i32, *const c_void);
    type Register =
        unsafe extern "system" fn(u32, Callback, *const c_void, *mut *mut c_void) -> HRESULT;
    // SAFETY: a DLL load restricted to System32; the module is never freed, since the
    // registration (and so the callback) lives for the rest of the process.
    let Ok(module) =
        (unsafe { LoadLibraryExW(w!("powrprof.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
    else {
        return false;
    };
    // SAFETY: the name is a NUL-terminated literal and the module is loaded.
    let Some(proc) = (unsafe {
        GetProcAddress(
            module,
            s!("PowerRegisterForEffectivePowerModeNotifications"),
        )
    }) else {
        log::info!("no effective power mode on this Windows; using the battery saver flag");
        return false;
    };
    // SAFETY: the signature is the one in powersetting.h.
    let register =
        unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Register>(proc) };
    for version in VERSIONS {
        let mut handle = std::ptr::null_mut();
        // SAFETY: `on_mode` matches EFFECTIVE_POWER_MODE_CALLBACK and lives forever; the handle is
        // kept registered (never unregistered) for the life of the process.
        if unsafe { register(version, on_mode, std::ptr::null(), &raw mut handle) }.is_ok() {
            return true;
        }
    }
    log::info!("cannot follow the effective power mode; using the battery saver flag");
    false
}

/// Battery saver, as `GetSystemPowerStatus` reports it.
fn legacy_saver_on() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut power = SYSTEM_POWER_STATUS::default();
    // SAFETY: power is a correctly sized out param.
    unsafe { GetSystemPowerStatus(&raw mut power) }.is_ok() && power.SystemStatusFlag == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_effective_mode_decides_once_reported() {
        assert!(
            decide(Some(BATTERY_SAVER), || false),
            "Energy Saver while plugged in"
        );
        assert!(
            !decide(Some(2), || true),
            "balanced: the old flag no longer decides"
        );
        assert!(
            !decide(Some(1), || false),
            "best power efficiency is not saver"
        );
        assert!(decide(None, || true), "nothing reported: the old flag");
        assert!(!decide(None, || false));
    }

    /// Each change Windows reports is what the next check sees, with no new query.
    #[test]
    fn checks_follow_reported_changes() {
        // SAFETY: called the way Windows calls it.
        unsafe { on_mode(BATTERY_SAVER, std::ptr::null()) };
        assert!(decide(reported(), || false));
        // SAFETY: as above.
        unsafe { on_mode(2, std::ptr::null()) };
        assert!(!decide(reported(), || true));
    }

    /// On this Windows the registration works and reports the current mode right away.
    #[test]
    fn registers_with_windows() {
        assert!(listen(), "PowerRegisterForEffectivePowerModeNotifications");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while reported().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(reported().is_some(), "a mode was reported");
    }
}
