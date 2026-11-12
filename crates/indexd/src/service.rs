//! Service control manager glue: start up, report status, stop on request.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::server::{Config, Server};

/// Exit after this long without connections.
const IDLE: Duration = Duration::from_mins(5);

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static SERVER: OnceLock<Arc<Server>> = OnceLock::new();
static STATUS: OnceLock<StatusHandle> = OnceLock::new();

struct StatusHandle(SERVICE_STATUS_HANDLE);

// SAFETY: SERVICE_STATUS_HANDLE is an SCM token; SetServiceStatus may be called from any thread.
unsafe impl Send for StatusHandle {}
// SAFETY: as above.
unsafe impl Sync for StatusHandle {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn report(state: SERVICE_STATUS_CURRENT_STATE, exit_code: u32) {
    let Some(handle) = STATUS.get() else {
        return;
    };
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED {
            0
        } else {
            3000
        },
    };
    // SAFETY: handle came from RegisterServiceCtrlHandlerExW; status is valid for the call.
    let _ = unsafe { SetServiceStatus(handle.0, &raw const status) };
}

unsafe extern "system" fn handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, 0);
            if let Some(server) = SERVER.get() {
                server.stop();
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide(crate::proto::SERVICE_NAME);
    // SAFETY: name is NUL-terminated and valid for the call; handler is a 'static function.
    let Ok(handle) =
        (unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(handler), None) })
    else {
        return;
    };
    let _ = STATUS.set(StatusHandle(handle));
    report(SERVICE_START_PENDING, 0);
    let config = CONFIG.lock().take();
    let Some(mut config) = config else {
        report(SERVICE_STOPPED, 1);
        return;
    };
    config.idle = Some(IDLE);
    let server = Server::new(config);
    let _ = SERVER.set(server.clone());
    report(SERVICE_RUNNING, 0);
    let code = match server.run() {
        Ok(()) => 0,
        Err(err) => {
            log::error!("server stopped: {err}");
            1
        }
    };
    report(SERVICE_STOPPED, code);
}

/// Runs as a service; called when started by the SCM.
pub(crate) fn run(config: Config) -> windows::core::Result<()> {
    *CONFIG.lock() = Some(config);
    let mut name = wide(crate::proto::SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: table ends with an all-zero entry and outlives the call, which blocks until the
    // service stops.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
}
