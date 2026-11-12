//! Arke's side: connects to the background service and implements [`arke_core::journal::Journal`].
//!
//! - If the service isn't running, asks the system to start it (install granted this user that
//!   right). Not installed means unsupported.
//! - Connects with `SECURITY_IDENTIFICATION`, so the service can't act as Arke's user.
//! - Checks that the pipe's server is the service itself (matching PID), not a squatter that
//!   created the same name first.
//! - Every step has a timeout: a stuck service only disables live updates, it never hangs Arke.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arke_core::journal::{Cursor, Journal, JournalCallback, JournalEvent, ListSink};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SERVICE_ALREADY_RUNNING, GENERIC_READ,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_MODE, FILE_WRITE_DATA, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::CancelIoEx;
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_STATUS_PROCESS, StartServiceW,
};
use windows::core::PCWSTR;

use crate::proto::{self, Reply, Request, VERSION};
use crate::sec::OwnedHandle;

/// Timeout for connecting, including asking the system to start the service.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Wait for the first listing batch. The service reads the whole MFT first, which takes a while
/// for large volumes on spinning disks.
const LIST_FIRST_TIMEOUT: Duration = Duration::from_secs(180);
/// Wait for each later batch.
const LIST_NEXT_TIMEOUT: Duration = Duration::from_secs(30);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// In development, connect to a service run with `arke-indexd console` (`ARKE_INDEXD_PIPE`).
/// Ignored in release so nobody can redirect Arke to another pipe; the server isn't verified then.
fn dev_pipe() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("ARKE_INDEXD_PIPE").ok()
    } else {
        None
    }
}

struct Service(SC_HANDLE);

impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: self.0 is a service handle owned by this struct and not yet closed.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_service(access: u32) -> Option<Service> {
    // SAFETY: asks for connect access only; the handle is owned by Service.
    let manager = Service(unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }.ok()?);
    let name = wide(proto::SERVICE_NAME);
    // SAFETY: name is NUL-terminated and valid for the call; manager is a valid handle.
    let service = unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), access) }.ok()?;
    Some(Service(service))
}

/// Whether the background service is installed (asks the system, doesn't connect).
pub fn installed() -> bool {
    open_service(SERVICE_QUERY_STATUS).is_some()
}

/// The service's PID, if it's running.
fn service_pid(service: &Service) -> Option<u32> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    // SAFETY: the out buffer is a SERVICE_STATUS_PROCESS of the right size, valid for the call.
    unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(std::slice::from_raw_parts_mut(
                (&raw mut status).cast::<u8>(),
                size_of::<SERVICE_STATUS_PROCESS>(),
            )),
            &raw mut needed,
        )
    }
    .ok()?;
    (status.dwCurrentState == SERVICE_RUNNING && status.dwProcessId != 0)
        .then_some(status.dwProcessId)
}

#[derive(Debug)]
enum ConnectError {
    /// The service isn't installed.
    NotInstalled,
    Failed,
}

fn try_open(name: &[u16]) -> windows::core::Result<OwnedHandle> {
    // SAFETY: name is NUL-terminated and valid for the call. Asks for read and write data only,
    // not the right to create pipe instances.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | FILE_WRITE_DATA.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    }?;
    Ok(OwnedHandle(handle))
}

/// Connects. `custom` is a development or test pipe name (no service, server not verified).
fn connect(custom: Option<&str>) -> Result<OwnedHandle, ConnectError> {
    let service = match custom {
        Some(_) => None,
        None => Some(
            open_service(SERVICE_QUERY_STATUS | SERVICE_START).ok_or(ConnectError::NotInstalled)?,
        ),
    };
    let name = wide(custom.unwrap_or(proto::PIPE_NAME));
    let started = Instant::now();
    let mut asked_to_start = false;
    let pipe = loop {
        match try_open(&name) {
            Ok(pipe) => break pipe,
            Err(err) if err.code() == ERROR_PIPE_BUSY.to_hresult() => {
                // SAFETY: name is NUL-terminated and valid for the call.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1000) };
            }
            Err(err) if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                // Service not running: ask the system to start it (once), then wait for the pipe.
                if let Some(service) = &service
                    && !asked_to_start
                {
                    asked_to_start = true;
                    // SAFETY: service is a valid handle with SERVICE_START access; no arguments.
                    if let Err(err) = unsafe { StartServiceW(service.0, None) }
                        && err.code() != ERROR_SERVICE_ALREADY_RUNNING.to_hresult()
                    {
                        log::warn!("could not start the Arke index service: {err}");
                        return Err(ConnectError::Failed);
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                log::warn!("could not connect to the Arke index service: {err}");
                return Err(ConnectError::Failed);
            }
        }
        if started.elapsed() > CONNECT_TIMEOUT {
            return Err(ConnectError::Failed);
        }
    };
    // The other end of the pipe must be the service.
    if let Some(service) = &service {
        let mut server = 0u32;
        // SAFETY: pipe is a valid pipe handle; server is a valid out pointer.
        let ok = unsafe { GetNamedPipeServerProcessId(pipe.0, &raw mut server) }.is_ok();
        if !ok || service_pid(service) != Some(server) {
            log::warn!("Arke index service pipe was not created by the service; not using it");
            return Err(ConnectError::Failed);
        }
    }
    Ok(pipe)
}

/// Pipe reader and writer.
struct PipeIo<'a>(&'a OwnedHandle);

impl Read for PipeIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: buf is valid and writable for the call; read is a valid out pointer; sync handle.
        unsafe { ReadFile(self.0.0, Some(buf), Some(&raw mut read), None) }
            .map_err(io::Error::other)?;
        Ok(read as usize)
    }
}

impl Write for PipeIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: buf is valid for the call; written is a valid out pointer; sync handle.
        unsafe { WriteFile(self.0.0, Some(buf), Some(&raw mut written), None) }
            .map_err(io::Error::other)?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn send(pipe: &OwnedHandle, req: &Request) -> io::Result<()> {
    proto::write_frame(&mut PipeIo(pipe), &req.encode())
}

fn receive(pipe: &OwnedHandle) -> io::Result<Reply> {
    let frame = proto::read_frame(&mut PipeIo(pipe))?;
    Reply::decode(&frame).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad reply"))
}

/// Connects and checks the protocol version.
fn session(custom: Option<&str>) -> Option<OwnedHandle> {
    let pipe = connect(custom).ok()?;
    send(&pipe, &Request::Hello { version: VERSION }).ok()?;
    match receive(&pipe).ok()? {
        Reply::Hello { version } if version == VERSION => Some(pipe),
        other => {
            log::warn!("Arke index service version mismatch: {other:?}");
            None
        }
    }
}

/// Runs `f` on a helper thread and waits at most `timeout`. If the service hangs, the helper is
/// left behind and the caller moves on.
fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = flume::bounded(1);
    std::thread::Builder::new()
        .name("arke-indexd-client".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(timeout).ok().flatten()
}

/// Reads the USN journal through the background service.
#[derive(Default)]
pub struct ServiceJournal {
    /// Development or test pipe; `None` means the installed service.
    custom: Option<String>,
}

impl ServiceJournal {
    /// Uses the installed service (or `ARKE_INDEXD_PIPE` in debug builds).
    pub fn new() -> Self {
        Self { custom: dev_pipe() }
    }

    /// Connects to the named pipe (tests only; no service, server not verified).
    #[cfg(debug_assertions)]
    pub fn at(pipe: &str) -> Self {
        Self {
            custom: Some(pipe.to_string()),
        }
    }
}

impl Journal for ServiceJournal {
    fn cursor(&self, root: &Path) -> Option<Cursor> {
        let root = root.to_path_buf();
        let custom = self.custom.clone();
        with_timeout(CONNECT_TIMEOUT * 2, move || {
            let pipe = session(custom.as_deref())?;
            send(&pipe, &Request::Cursor { root }).ok()?;
            match receive(&pipe).ok()? {
                Reply::Cursor(cursor) => cursor,
                _ => None,
            }
        })
    }

    fn list(&self, root: &Path, sink: ListSink) -> io::Result<()> {
        let root = root.to_path_buf();
        let custom = self.custom.clone();
        let pipe = with_timeout(CONNECT_TIMEOUT * 2, move || {
            let pipe = session(custom.as_deref())?;
            send(&pipe, &Request::List { root }).ok()?;
            Some(pipe)
        })
        .ok_or_else(|| io::Error::other("Arke index service unavailable"))?;
        // Receive on another thread so a stuck service times out here (falling back to a walk)
        // instead of stalling the index build.
        let (tx, rx) = flume::bounded::<io::Result<Reply>>(4);
        std::thread::Builder::new()
            .name("arke-indexd-list".into())
            .spawn(move || {
                loop {
                    let reply = receive(&pipe);
                    let last = !matches!(reply, Ok(Reply::Listing(_)));
                    // Receiver gone (cancelled, timed out): close the pipe, the service stops when
                    // it can't send.
                    if tx.send(reply).is_err() || last {
                        return;
                    }
                }
            })?;
        let mut timeout = LIST_FIRST_TIMEOUT;
        loop {
            let reply = rx.recv_timeout(timeout).map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "index service timed out")
            })??;
            timeout = LIST_NEXT_TIMEOUT;
            match reply {
                Reply::Listing(items) => {
                    if !sink(items) {
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                }
                Reply::ListDone => return Ok(()),
                Reply::ListFailed(why) => {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, why));
                }
                other => return Err(io::Error::other(format!("unexpected reply {other:?}"))),
            }
        }
    }

    fn follow(
        &self,
        root: &Path,
        from: Cursor,
        on_event: JournalCallback,
    ) -> io::Result<Box<dyn Send>> {
        let root = root.to_path_buf();
        let custom = self.custom.clone();
        let pipe = with_timeout(CONNECT_TIMEOUT * 2, move || {
            let pipe = session(custom.as_deref())?;
            send(&pipe, &Request::Follow { root, from }).ok()?;
            Some(pipe)
        })
        .ok_or_else(|| io::Error::other("Arke index service unavailable"))?;
        let follow = Arc::new(Follow {
            pipe,
            stopped: AtomicBool::new(false),
        });
        let reader = follow.clone();
        std::thread::Builder::new()
            .name("arke-indexd-follow".into())
            .spawn(move || {
                loop {
                    let reply = receive(&reader.pipe);
                    if reader.stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    let event = match reply {
                        Ok(Reply::Changes { changes, cursor }) => {
                            JournalEvent::Changed { changes, cursor }
                        }
                        Ok(Reply::CaughtUp) => JournalEvent::CaughtUp,
                        Ok(Reply::Ping) => continue,
                        // Disconnected, service stopped or unknown reply: changes may be missed.
                        Ok(_) | Err(_) => {
                            on_event(JournalEvent::Lost);
                            return;
                        }
                    };
                    let lost = event == JournalEvent::Lost;
                    on_event(event);
                    if lost {
                        return;
                    }
                }
            })?;
        Ok(Box::new(FollowGuard(follow)))
    }
}

struct Follow {
    pipe: OwnedHandle,
    stopped: AtomicBool,
}

/// Stops following on drop by cancelling the pending read, which ends the reader thread.
struct FollowGuard(Arc<Follow>);

impl Drop for FollowGuard {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Relaxed);
        // SAFETY: pipe is a valid handle kept alive by the Arc; cancels all pending I/O on it.
        let _ = unsafe { CancelIoEx(self.0.pipe.0, None) };
    }
}
