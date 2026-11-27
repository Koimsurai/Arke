//! The named pipe transport of the external preview program (`external.rs`): one line written to
//! `\\.\pipe\<name>`, then the pipe is closed. The program on the other end reads one line per
//! connection, so it needs no new process for each preview.
//!
//! Connecting never waits for a pipe that isn't there (`ERROR_FILE_NOT_FOUND` comes back at once,
//! reported as [`io::ErrorKind::NotFound`]); a pipe that is busy with another client is waited for
//! at most the time given. The pipe is opened for writing only, with the lowest impersonation
//! level, so the server learns nothing about Arke and can't act as this user.

use std::io::{self, Write as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_PIPE_BUSY;
use windows::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

/// How long a hand-off waits while the pipe is busy with another message. A server takes one
/// connection at a time and is listening again right after it has read its line.
pub(crate) const BUSY_WAIT: Duration = Duration::from_millis(200);

/// Writes `line` to the pipe at `pipe` (`\\.\pipe\...`) and closes it. `NotFound` when the pipe
/// doesn't exist, `TimedOut` when it stayed busy for `busy_wait`. Background thread.
pub(crate) fn send_line(pipe: &str, line: &str, busy_wait: Duration) -> io::Result<()> {
    let deadline = Instant::now() + busy_wait;
    let mut file = loop {
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(pipe);
        match opened {
            Ok(file) => break file,
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY.0.cast_signed()) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the pipe stayed busy",
                    ));
                }
                let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
                let name: Vec<u16> = pipe.encode_utf16().chain(Some(0)).collect();
                // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), ms) };
            }
            Err(err) => return Err(err),
        }
    };
    // Closing the handle after the write is fine: the server still reads what was written.
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_INBOUND, ReadFile};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use super::*;

    /// A pipe name no other test or process uses.
    fn unique_pipe() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\arke-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A server that takes one connection on `pipe` and returns every byte it reads. It has
    /// created the pipe when this returns.
    fn serve_once(pipe: &str) -> std::thread::JoinHandle<Vec<u8>> {
        let name: Vec<u16> = pipe.encode_utf16().chain(Some(0)).collect();
        let (ready_tx, ready_rx) = flume::bounded(1);
        let server = std::thread::spawn(move || {
            // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            assert!(handle != INVALID_HANDLE_VALUE, "pipe created");
            ready_tx.send(()).expect("the test is waiting");
            // A client that connected first makes this fail with ERROR_PIPE_CONNECTED: fine.
            // SAFETY: handle is a valid pipe HANDLE; synchronous mode needs no OVERLAPPED.
            let _ = unsafe { ConnectNamedPipe(handle, None) };
            let mut got = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                let mut read = 0u32;
                // SAFETY: buf is writable for the call; read is a valid out pointer.
                let ok = unsafe { ReadFile(handle, Some(&mut buf), Some(&raw mut read), None) };
                if ok.is_err() || read == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..read as usize]);
            }
            // SAFETY: handle is open and not used again.
            let _ = unsafe { CloseHandle(handle) };
            got
        });
        ready_rx.recv().expect("the server is up");
        server
    }

    #[test]
    fn a_line_reaches_the_server_exactly_as_written() {
        let pipe = unique_pipe();
        let server = serve_once(&pipe);
        let line = "Toggle|C:\\q\\資料 夾\\a b.txt\r\n";
        send_line(&pipe, line, BUSY_WAIT).expect("sent");
        let got = server.join().expect("server ends");
        assert_eq!(String::from_utf8(got).expect("UTF-8"), line);
    }

    #[test]
    fn a_pipe_that_is_not_there_fails_at_once() {
        let pipe = unique_pipe();
        let started = Instant::now();
        let err = send_line(&pipe, "x\r\n", BUSY_WAIT).expect_err("no server");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "took {:?}",
            started.elapsed()
        );
    }
}
