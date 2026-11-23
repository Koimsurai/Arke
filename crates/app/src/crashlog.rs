//! Log file and crash records at `%LOCALAPPDATA%\Arke\arke.log`.
//!
//! Release builds have no console, so logs go to a file written by a background
//! thread; the file restarts once it exceeds 512 KB. Panics are logged with the
//! thread name. Hard crashes (usually an access violation in a third-party DLL)
//! log the exception code, address and faulting module, then go on to Windows
//! as usual; no recovery is attempted.
//!
//! Before a panic or a hard crash ends the process, the writer thread gets a moment to write
//! what is still queued: the process used to end with the last lines (the panic itself among
//! them) still in the queue, and a report showed Arke vanish without a trace (#363).
//!
//! Debug builds log to stderr. `ARKE_LOG_STDERR` forces stderr in release too.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
const MAX_LOG: u64 = 512 * 1024;

/// Set in the child process of the log file test.
const TEST_CHILD: &str = "ARKE_TEST_LOG_CHILD";

/// The writer thread's name; it never waits for itself.
const WRITER: &str = "log";

/// How long a panic waits for the queued lines to be written.
const PANIC_FLUSH: Duration = Duration::from_millis(500);
/// The same for a hard crash, where the process may be too broken to wait long.
const CRASH_FLUSH: Duration = Duration::from_millis(200);

/// What the writer thread receives.
enum Message {
    /// Text to append.
    Line(Vec<u8>),
    /// Answer once everything before this is written.
    Flush(flume::Sender<()>),
}

/// The writer thread's queue, for [`flush`].
static QUEUE: OnceLock<flume::Sender<Message>> = OnceLock::new();

/// Forwards log output to the writer thread.
struct Lines(flume::Sender<Message>);

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !is_noise(buf) {
            let _ = self.0.send(Message::Line(buf.to_vec()));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// GPUI errors that only mean a window closed a moment before GPUI got to it: every menu (a
/// window of its own) that closes logs one. They aren't problems, but as ERROR lines they fill
/// the log and every problem report with what reads like a fault.
fn is_noise(line: &[u8]) -> bool {
    const NOISE: [&str; 2] = [
        "] scheduling follow-up frame for window ",
        "] handling active status change for window ",
    ];
    let Ok(line) = std::str::from_utf8(line) else {
        return false;
    };
    line.trim_end().ends_with(": window not found") && NOISE.iter().any(|n| line.contains(n))
}

/// Path of the log file, or `None` when logging to the console.
pub(crate) fn log_path() -> Option<&'static std::path::Path> {
    LOG_PATH.get().map(PathBuf::as_path)
}

/// Writes every message to `out` until the queue closes, answering each flush once what came
/// before it is written.
fn write_all(rx: &flume::Receiver<Message>, out: &mut impl Write) {
    for message in rx {
        match message {
            Message::Line(chunk) => {
                let _ = out.write_all(&chunk);
            }
            Message::Flush(done) => {
                let _ = out.flush();
                let _ = done.send(());
            }
        }
    }
}

/// Waits up to `wait` for the lines queued so far to reach `queue`'s writer; `false` when it
/// didn't answer in time (or there is none).
fn flush_queue(queue: &flume::Sender<Message>, wait: Duration) -> bool {
    if std::thread::current().name() == Some(WRITER) {
        return false;
    }
    let (done, answered) = flume::bounded(1);
    queue.send(Message::Flush(done)).is_ok() && answered.recv_timeout(wait).is_ok()
}

/// Waits up to `wait` for the lines logged so far to be in the log file.
pub(crate) fn flush(wait: Duration) -> bool {
    QUEUE.get().is_some_and(|queue| flush_queue(queue, wait))
}

pub(crate) fn init() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.format_timestamp_millis();
    // The PDF parser logs every oddity of the files a content search reads.
    if std::env::var_os("RUST_LOG").is_none() {
        builder.filter_module("lopdf", log::LevelFilter::Off);
    }
    // Tests check the file path in a child process of their own.
    let release = !cfg!(debug_assertions) || (cfg!(test) && std::env::var_os(TEST_CHILD).is_some());
    let to_file = release && std::env::var_os("ARKE_LOG_STDERR").is_none();
    let dir = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Arke"));
    if to_file && let Some(dir) = dir {
        let path = dir.join("arke.log");
        let (tx, rx) = flume::unbounded::<Message>();
        let target = path.clone();
        let spawned = std::thread::Builder::new()
            .name(WRITER.into())
            .spawn(move || {
                let _ = std::fs::create_dir_all(&dir);
                let too_big = std::fs::metadata(&target).is_ok_and(|m| m.len() > MAX_LOG);
                let opened = if too_big {
                    std::fs::File::create(&target)
                } else {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&target)
                };
                let Ok(mut file) = opened else {
                    return;
                };
                write_all(&rx, &mut file);
            });
        if spawned.is_ok() {
            let _ = LOG_PATH.set(path);
            let _ = QUEUE.set(tx.clone());
            builder.target(env_logger::Target::Pipe(Box::new(Lines(tx))));
        }
    }
    builder.init();
    log::info!(
        "main started ({} {})",
        crate::build_info::VERSION,
        std::env::current_exe().map_or_default(|p| p.display().to_string())
    );

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "panic (thread {}): {info}",
            std::thread::current().name().unwrap_or("?")
        );
        // A panic on the UI thread, or in a callback that can't unwind, ends the process next.
        flush(PANIC_FLUSH);
        default_hook(info);
    }));
    install_crash_filter();
}

/// Last-chance exception filter: log one line and let Windows continue.
fn install_crash_filter() {
    use windows::Win32::System::Diagnostics::Debug::{
        EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
    };

    unsafe extern "system" fn on_crash(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the pointer from Windows is valid for the duration of the callback; read only.
        let (code, address) = unsafe {
            info.as_ref()
                .and_then(|i| i.ExceptionRecord.as_ref())
                .map_or((0u32, 0usize), |r| {
                    (
                        r.ExceptionCode.0.cast_unsigned(),
                        r.ExceptionAddress as usize,
                    )
                })
        };
        let module = module_at(address);
        // What led up to it, before the crash line.
        flush(CRASH_FLUSH);
        if let Some(path) = LOG_PATH.get()
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(
                file,
                "[crash] exception 0x{code:08X} at 0x{address:X} in module {module} (thread {})",
                std::thread::current().name().unwrap_or("?")
            );
        }
        // EXCEPTION_CONTINUE_SEARCH: Windows proceeds with error reporting and termination.
        0
    }

    // SAFETY: registers a process-wide callback; the callback is a 'static fn.
    unsafe {
        SetUnhandledExceptionFilter(Some(on_crash));
    }
}

/// Path of the module (DLL or exe) containing `address`.
fn module_at(address: usize) -> String {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    let mut module = HMODULE::default();
    // SAFETY: with FROM_ADDRESS the second argument is any address inside the module, not a
    // string; `module` is the out parameter.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(address as *const u16),
            &raw mut module,
        )
    };
    if found.is_err() {
        return "?".into();
    }
    let mut buf = [0u16; 520];
    // SAFETY: `buf` is a correctly sized output buffer.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len.min(buf.len())])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flush answers only once every line before it is written, so a panic's line and what led
    /// up to it are in the file before the process ends (#363).
    #[test]
    fn a_flush_waits_for_the_lines_before_it() {
        let (tx, rx) = flume::unbounded::<Message>();
        let written = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = std::sync::Arc::clone(&written);
        let writer = std::thread::Builder::new()
            .name(WRITER.into())
            .spawn(move || {
                struct Slow(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl Write for Slow {
                    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                        std::thread::sleep(Duration::from_millis(5));
                        self.0.lock().extend_from_slice(buf);
                        Ok(buf.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                write_all(&rx, &mut Slow(sink));
            })
            .unwrap();
        let mut lines = Lines(tx.clone());
        for i in 0..20 {
            writeln!(lines, "line {i}").unwrap();
        }
        writeln!(lines, "panic (thread main): boom").unwrap();
        assert!(flush_queue(&tx, Duration::from_secs(5)));
        let text = String::from_utf8(written.lock().clone()).unwrap();
        assert!(text.starts_with("line 0\n"));
        assert!(text.ends_with("panic (thread main): boom\n"), "{text}");
        drop((lines, tx));
        writer.join().unwrap();
    }

    /// The child of the test below: logs, then panics where unwinding is impossible, which ends
    /// the process at once, like a panic in a window procedure.
    #[test]
    #[ignore = "started by a_panic_that_ends_the_process_leaves_its_lines_in_the_file"]
    fn child_ends_with_a_panic_in_a_callback() {
        extern "C" fn callback() {
            panic!("boom in a callback");
        }
        if std::env::var_os(TEST_CHILD).is_none() {
            return;
        }
        init();
        for i in 0..2000 {
            log::info!("before the panic {i}");
        }
        callback();
    }

    #[test]
    fn a_panic_that_ends_the_process_leaves_its_lines_in_the_file() {
        let dir = std::env::temp_dir().join(format!("arke-crashlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crashlog::tests::child_ends_with_a_panic_in_a_callback",
                "--ignored",
                "--nocapture",
            ])
            .env("LOCALAPPDATA", &dir)
            .env(TEST_CHILD, "1")
            .env_remove("ARKE_LOG_STDERR")
            .env_remove("RUST_LOG")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let text = std::fs::read_to_string(dir.join("Arke").join("arke.log")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!status.success(), "the child should end with the panic");
        assert!(
            text.contains("before the panic 1999"),
            "lines before the panic are lost"
        );
        assert!(
            text.contains("panic (thread ") && text.contains("boom in a callback"),
            "the panic line is lost"
        );
    }

    #[test]
    fn a_flush_gives_up_when_nothing_writes() {
        let (tx, rx) = flume::unbounded::<Message>();
        assert!(!flush_queue(&tx, Duration::from_millis(20)));
        drop(rx);
        assert!(!flush_queue(&tx, Duration::from_millis(20)));
    }

    #[test]
    fn closed_window_races_are_left_out_of_the_log() {
        assert!(is_noise(
            b"[2026-11-20T01:41:56.835Z ERROR] scheduling follow-up frame for window WindowId(2v1): window not found
"
        ));
        assert!(is_noise(
            b"[2026-11-20T01:41:56.835Z ERROR] handling active status change for window WindowId(2v3): window not found
"
        ));
        // Anything else about a window stays, and so do other errors.
        assert!(!is_noise(
            b"[2026-11-20T01:41:56.835Z ERROR] drawing and presenting frame for window WindowId(1v1): device lost
"
        ));
        assert!(!is_noise(
            b"[2026-11-20T02:05:01.969Z ERROR] scheduling follow-up frame for window WindowId(1v1): out of memory
"
        ));
    }
}
