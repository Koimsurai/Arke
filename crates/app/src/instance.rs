//! Single instance and background residency.
//!
//! The first Arke creates a named pipe and stays resident after its last window closes. Later
//! launches of `arke.exe` just pass the path to open to the resident process and exit, skipping GPU
//! init (about 260 ms).
//!
//! Guards against hangs:
//! - The sender's whole exchange has a timeout; if the resident process doesn't confirm a window in
//!   time, the sender opens one itself.
//! - The pipe accepts local clients only (`PIPE_REJECT_REMOTE_CLIENTS`), and its name includes the
//!   user and session plus debug/release, so instances never cross.
//! - No autostart of its own; residency only exists after the user launched Arke.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
    FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
    PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, SetNamedPipeHandleState, WaitNamedPipeW,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::PCWSTR;

use crate::bench::BenchMode;

/// Max message size: paths up to 32767 UTF-16 units plus the other fields.
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
/// How long the resident process waits for the window to open before replying failure so the sender
/// opens its own.
pub(crate) const SERVER_ACK_TIMEOUT: Duration = Duration::from_millis(2000);
/// Timeout for the sender's whole exchange (connect plus waiting for the ack).
pub(crate) const CLIENT_TIMEOUT: Duration = Duration::from_millis(3000);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenRequest {
    pub(crate) path: Option<PathBuf>,
    pub(crate) bench: Option<BenchMode>,
    pub(crate) bench_out: Option<PathBuf>,
    /// Sender start time in Unix ms, so measurements start from the moment of launch
    pub(crate) launched_unix_ms: u64,
    /// Open a page instead of a folder (`arke --home`, the Win+E registration, the Recycle Bin)
    pub(crate) page: Option<Page>,
}

/// A page Arke shows instead of a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Home,
    RecycleBin,
}

impl Page {
    pub(crate) const fn target(self) -> arke_core::takeover::Target {
        match self {
            Self::Home => arke_core::takeover::Target::Home,
            Self::RecycleBin => arke_core::takeover::Target::RecycleBin,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Open(OpenRequest),
    /// Select these items in the window showing their folder ("Show in folder" arrives after the
    /// folder was opened; see `win_openinfolder.rs`)
    Select {
        folder: PathBuf,
        items: Vec<PathBuf>,
    },
    Quit,
}

// ─── Protocol: NUL-separated UTF-16 fields (Windows paths may not be valid UTF-8, so no string
// conversion)

fn push_field(out: &mut Vec<u16>, field: &[u16]) {
    out.extend_from_slice(field);
    out.push(0);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub(crate) fn encode(req: &Request) -> Vec<u8> {
    let mut w = Vec::new();
    match req {
        Request::Quit => push_field(&mut w, &wide("quit")),
        Request::Select { folder, items } => {
            push_field(&mut w, &wide("select"));
            push_field(
                &mut w,
                &folder.as_os_str().encode_wide().collect::<Vec<u16>>(),
            );
            for item in items {
                push_field(
                    &mut w,
                    &item.as_os_str().encode_wide().collect::<Vec<u16>>(),
                );
            }
        }
        Request::Open(open) => {
            push_field(&mut w, &wide("open"));
            let path: Vec<u16> = open
                .path
                .as_ref()
                .map(|p| p.as_os_str().encode_wide().collect())
                .unwrap_or_default();
            push_field(&mut w, &path);
            let bench = match open.bench {
                None => "",
                Some(BenchMode::Startup) => "startup",
                Some(BenchMode::Scroll) => "scroll",
                Some(BenchMode::ScrollGrid) => "scroll-grid",
            };
            push_field(&mut w, &wide(bench));
            let out: Vec<u16> = open
                .bench_out
                .as_ref()
                .map(|p| p.as_os_str().encode_wide().collect())
                .unwrap_or_default();
            push_field(&mut w, &out);
            push_field(&mut w, &wide(&open.launched_unix_ms.to_string()));
            let page = match open.page {
                None => "",
                Some(Page::Home) => "home",
                Some(Page::RecycleBin) => "recycle",
            };
            push_field(&mut w, &wide(page));
        }
    }
    w.iter().flat_map(|u| u.to_le_bytes()).collect()
}

pub(crate) fn decode(bytes: &[u8]) -> Option<Request> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs.iter().map(|&c| u16::from_le_bytes(c)).collect();
    let mut fields = units.split(|&u| u == 0);
    let as_string = |f: &[u16]| String::from_utf16(f).ok();
    let as_path = |f: &[u16]| (!f.is_empty()).then(|| PathBuf::from(OsString::from_wide(f)));
    match as_string(fields.next()?)?.as_str() {
        "quit" => Some(Request::Quit),
        "open" => {
            let path = as_path(fields.next()?);
            let bench = match as_string(fields.next()?)?.as_str() {
                "" => None,
                "scroll" => Some(BenchMode::Scroll),
                "scroll-grid" => Some(BenchMode::ScrollGrid),
                _ => Some(BenchMode::Startup),
            };
            let bench_out = as_path(fields.next()?);
            let launched_unix_ms = as_string(fields.next()?)?.parse().ok()?;
            // Absent when an older Arke sent it.
            let page = match fields.next().and_then(as_string).as_deref() {
                Some("home") => Some(Page::Home),
                Some("recycle") => Some(Page::RecycleBin),
                _ => None,
            };
            Some(Request::Open(OpenRequest {
                path,
                bench,
                bench_out,
                launched_unix_ms,
                page,
            }))
        }
        "select" => {
            let folder = as_path(fields.next()?)?;
            // The last split piece is the empty tail after the final terminator.
            let items: Vec<PathBuf> = fields.filter_map(as_path).collect();
            Some(Request::Select { folder, items })
        }
        _ => None,
    }
}

// ─── Pipe

fn pipe_name() -> Vec<u16> {
    let mut session = 0u32;
    // SAFETY: valid out pointer; on failure session stays 0, which only affects the name.
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) };
    let user = std::env::var("USERNAME").unwrap_or_default();
    let profile = if cfg!(debug_assertions) { "-dbg" } else { "" };
    let name = format!(r"\\.\pipe\arke{profile}.{session}.{user}");
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns a pipe HANDLE.
struct Pipe(HANDLE);

// SAFETY: a HANDLE is a kernel object reference usable from any thread; only the owner uses it at a
// time.
unsafe impl Send for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: self.0 is a valid HANDLE owned by this struct and not yet closed.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The first pipe instance; holding it means this process is the resident one.
pub(crate) struct Listener {
    first: Pipe,
    name: Vec<u16>,
}

fn create_instance(name: &[u16], first: bool) -> Option<Pipe> {
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            open_mode,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            MAX_MESSAGE_BYTES as u32,
            0,
            None,
        )
    };
    (!handle.is_invalid() && handle != INVALID_HANDLE_VALUE).then_some(Pipe(handle))
}

/// Try to become the resident process. Returns `None` if another one already is.
pub(crate) fn listen() -> Option<Listener> {
    listen_at(pipe_name())
}

fn listen_at(name: Vec<u16>) -> Option<Listener> {
    let first = create_instance(&name, true)?;
    Some(Listener { first, name })
}

/// Handle requests on a background thread. `handler` returns `true` when handled (e.g. the window
/// opened).
pub(crate) fn serve(listener: Listener, handler: impl Fn(Request) -> bool + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("arke-instance".into())
        .spawn(move || {
            let Listener { first, name } = listener;
            let mut current = first;
            loop {
                // SAFETY: current.0 is a valid pipe HANDLE; synchronous mode needs no OVERLAPPED.
                let connected = unsafe { ConnectNamedPipe(current.0, None) };
                if let Err(err) = connected
                    && err.code() != ERROR_PIPE_CONNECTED.to_hresult()
                {
                    log::warn!("resident pipe connect failed: {err}");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                // Create the next instance before handling this connection so the pipe name always
                // exists; otherwise an Arke launched in the gap would see no server and become a
                // second resident.
                let Some(next) = create_instance(&name, false) else {
                    log::error!("cannot create next pipe instance, stopping resident service");
                    return;
                };
                handle_connection(&current, &handler);
                // SAFETY: current.0 is a valid, connected pipe HANDLE.
                unsafe {
                    let _ = FlushFileBuffers(current.0);
                    let _ = DisconnectNamedPipe(current.0);
                }
                current = next;
            }
        });
    if let Err(err) = spawned {
        log::error!("cannot start resident service thread: {err}");
    }
}

fn handle_connection(pipe: &Pipe, handler: &impl Fn(Request) -> bool) {
    let mut buf = vec![0u8; MAX_MESSAGE_BYTES];
    let mut read = 0u32;
    // SAFETY: buf is valid and writable for the call; read is a valid out pointer.
    let ok = unsafe { ReadFile(pipe.0, Some(&mut buf), Some(&raw mut read), None) };
    let status = if let Err(err) = ok {
        log::warn!("resident pipe read failed: {err}");
        false
    } else if let Some(req) = decode(&buf[..read as usize]) {
        handler(req)
    } else {
        log::warn!("resident pipe got an unparsable message ({read} bytes)");
        false
    };
    let reply = [u8::from(status)];
    let mut written = 0u32;
    // SAFETY: reply is valid for the call; written is a valid out pointer.
    let _ = unsafe { WriteFile(pipe.0, Some(&reply), Some(&raw mut written), None) };
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    /// Handled by the resident process
    Delivered,
    /// No resident process
    NoServer,
    /// Resident process exists but didn't handle it in time (or reported failure)
    Failed,
}

/// Hand the request to the resident process. The exchange runs on a helper thread and the main
/// thread waits at most `timeout`; if the resident process hangs, the helper is abandoned and this
/// process opens its own window.
pub(crate) fn send(req: &Request, timeout: Duration) -> SendOutcome {
    send_to(pipe_name(), req, timeout)
}

fn send_to(name: Vec<u16>, req: &Request, timeout: Duration) -> SendOutcome {
    let message = encode(req);
    let (tx, rx) = flume::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("arke-instance-client".into())
        .spawn(move || {
            let _ = tx.send(exchange(&name, &message));
        });
    if spawned.is_err() {
        return SendOutcome::Failed;
    }
    rx.recv_timeout(timeout).unwrap_or(SendOutcome::Failed)
}

fn exchange(name: &[u16], message: &[u8]) -> SendOutcome {
    let pipe = loop {
        // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        match opened {
            Ok(handle) => break Pipe(handle),
            Err(err) if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                return SendOutcome::NoServer;
            }
            Err(err) if err.code() == ERROR_PIPE_BUSY.to_hresult() => {
                // SAFETY: same as above.
                if unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1000) }.as_bool() {
                    continue;
                }
                return SendOutcome::Failed;
            }
            Err(err) => {
                log::warn!("resident pipe connect failed: {err}");
                return SendOutcome::Failed;
            }
        }
    };
    let mode = PIPE_READMODE_MESSAGE;
    // SAFETY: pipe.0 is a valid pipe HANDLE; mode is valid for the call.
    let _ = unsafe { SetNamedPipeHandleState(pipe.0, Some(&raw const mode), None, None) };

    // This process was just launched by the user and holds foreground rights; pass them to the
    // resident process so its new window can come to front.
    let mut server_pid = 0u32;
    // SAFETY: pipe.0 is a valid pipe HANDLE; server_pid is a valid out pointer.
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &raw mut server_pid) }.is_ok() {
        // SAFETY: plain Win32 call with no pointer arguments.
        let _ = unsafe { AllowSetForegroundWindow(server_pid) };
    }

    let mut written = 0u32;
    // SAFETY: message is valid for the call; written is a valid out pointer.
    if unsafe { WriteFile(pipe.0, Some(message), Some(&raw mut written), None) }.is_err() {
        return SendOutcome::Failed;
    }
    let mut reply = [0u8; 1];
    let mut read = 0u32;
    // SAFETY: reply is valid and writable for the call; read is a valid out pointer.
    match unsafe { ReadFile(pipe.0, Some(&mut reply), Some(&raw mut read), None) } {
        Ok(()) if read == 1 && reply[0] == 1 => SendOutcome::Delivered,
        _ => SendOutcome::Failed,
    }
}

/// Whether a resident process's pipe exists. Only waits on the name, never connects, so the
/// resident process sees nothing.
fn pipe_exists(name: &[u16]) -> bool {
    use windows::Win32::Foundation::{ERROR_SEM_TIMEOUT, GetLastError};
    // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
    let free = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) }.as_bool();
    // SAFETY: reads this thread's last error, set by the call above.
    free || unsafe { GetLastError() } == ERROR_SEM_TIMEOUT
}

/// Asks the resident process to quit and waits up to `timeout` for it to be gone (uninstall).
/// Returns `false` if it is still there.
pub(crate) fn quit_resident(timeout: Duration) -> bool {
    quit_at(&pipe_name(), timeout)
}

fn quit_at(name: &[u16], timeout: Duration) -> bool {
    if !pipe_exists(name) {
        return true;
    }
    let _ = send_to(name.to_vec(), &Request::Quit, CLIENT_TIMEOUT);
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !pipe_exists(name) {
            // The pipe closes as the process exits; give it a moment to finish.
            std::thread::sleep(Duration::from_millis(300));
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Each test uses its own pipe name so it never touches a running Arke.
    fn test_pipe(tag: &str) -> Vec<u16> {
        format!(r"\\.\pipe\arke-test.{tag}.{}", std::process::id())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    fn open_req(ms: u64) -> Request {
        Request::Open(OpenRequest {
            // Non-ASCII on purpose: paths must survive the pipe intact.
            path: Some(PathBuf::from(r"C:\測試")),
            bench: None,
            bench_out: None,
            launched_unix_ms: ms,
            page: None,
        })
    }

    #[test]
    fn pipe_single_instance_and_delivery() {
        let name = test_pipe("deliver");
        assert_eq!(
            send_to(name.clone(), &open_req(1), Duration::from_secs(2)),
            SendOutcome::NoServer
        );
        let listener = listen_at(name.clone()).unwrap();
        // With a resident process present, a second one can't become resident.
        assert!(listen_at(name.clone()).is_none());

        let (tx, rx) = flume::unbounded();
        serve(listener, move |req| {
            let _ = tx.send(req);
            true
        });
        // Send several times: the pipe name must persist after each handled connection.
        for ms in 1..=5 {
            assert_eq!(
                send_to(name.clone(), &open_req(ms), Duration::from_secs(2)),
                SendOutcome::Delivered
            );
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                open_req(ms)
            );
        }
        assert!(listen_at(name).is_none());
    }

    #[test]
    fn pipe_rejection_and_hang_fall_back() {
        let name = test_pipe("hang");
        let listener = listen_at(name.clone()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls;
        serve(listener, move |_| {
            // First reports failure, second simulates a hung resident process.
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                false
            } else {
                std::thread::sleep(Duration::from_secs(5));
                true
            }
        });
        assert_eq!(
            send_to(name.clone(), &open_req(1), Duration::from_secs(2)),
            SendOutcome::Failed
        );
        let started = std::time::Instant::now();
        assert_eq!(
            send_to(name, &open_req(2), Duration::from_millis(300)),
            SendOutcome::Failed
        );
        // The sender doesn't hang with it.
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn protocol_round_trip() {
        let reqs = [
            Request::Quit,
            Request::Open(OpenRequest {
                // Non-ASCII and a space, to cover the encoding.
                path: Some(PathBuf::from(r"C:\Users\測試\報告 2026")),
                bench: Some(BenchMode::Scroll),
                bench_out: Some(PathBuf::from(r"C:\Temp\out.txt")),
                launched_unix_ms: 1_762_000_000_123,
                page: Some(Page::RecycleBin),
            }),
            Request::Open(OpenRequest {
                path: None,
                bench: None,
                bench_out: None,
                launched_unix_ms: 0,
                page: None,
            }),
        ];
        for req in reqs {
            assert_eq!(decode(&encode(&req)), Some(req));
        }
    }

    #[test]
    fn select_requests_carry_every_item() {
        let req = Request::Select {
            folder: PathBuf::from(r"C:\Users\測試"),
            items: vec![
                PathBuf::from(r"C:\Users\測試\a b.txt"),
                PathBuf::from(r"C:\Users\測試\c.txt"),
            ],
        };
        assert_eq!(decode(&encode(&req)), Some(req));
        let none = Request::Select {
            folder: PathBuf::from(r"C:\x"),
            items: Vec::new(),
        };
        assert_eq!(decode(&encode(&none)), Some(none));
    }

    #[test]
    fn protocol_keeps_non_utf8_paths() {
        // A lone surrogate is a valid Windows file name but not valid UTF-8.
        let raw = OsString::from_wide(&[0x0041, 0xD800, 0x0042]);
        let req = Request::Open(OpenRequest {
            path: Some(PathBuf::from(raw)),
            bench: None,
            bench_out: None,
            launched_unix_ms: 1,
            page: None,
        });
        assert_eq!(decode(&encode(&req)), Some(req));
    }

    #[test]
    fn protocol_rejects_garbage() {
        assert_eq!(decode(&[1]), None);
        assert_eq!(decode(&encode(&Request::Quit)[..1]), None);
        assert_eq!(
            decode(
                &wide("nope\0")
                    .iter()
                    .flat_map(|u| u.to_le_bytes())
                    .collect::<Vec<_>>()
            ),
            None
        );
    }

    #[test]
    fn sees_a_pipe_without_connecting() {
        let name = test_pipe("exists");
        assert!(!pipe_exists(&name));
        assert!(quit_at(&name, Duration::from_millis(10)), "nothing to quit");
        let listener = listen_at(name.clone()).unwrap();
        assert!(pipe_exists(&name));
        drop(listener);
        assert!(!pipe_exists(&name));
    }
}
