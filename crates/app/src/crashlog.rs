//! Log file and crash records at `%LOCALAPPDATA%\Arke\arke.log`.
//!
//! Release builds have no console, so logs go to a file written by a background
//! thread; the file restarts once it exceeds 512 KB. Panics are logged with the
//! thread name. Hard crashes (usually an access violation in a third-party DLL)
//! log the exception code, address and faulting module, then go on to Windows
//! as usual; no recovery is attempted.
//!
//! Debug builds log to stderr. `ARKE_LOG_STDERR` forces stderr in release too.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
const MAX_LOG: u64 = 512 * 1024;

/// Forwards log output to the writer thread.
struct Lines(flume::Sender<Vec<u8>>);

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Path of the log file, or `None` when logging to the console.
pub(crate) fn log_path() -> Option<&'static std::path::Path> {
    LOG_PATH.get().map(PathBuf::as_path)
}

pub(crate) fn init() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.format_timestamp_millis();
    // The PDF parser logs every oddity of the files a content search reads.
    if std::env::var_os("RUST_LOG").is_none() {
        builder.filter_module("lopdf", log::LevelFilter::Off);
    }
    let to_file = !cfg!(debug_assertions) && std::env::var_os("ARKE_LOG_STDERR").is_none();
    let dir = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Arke"));
    if to_file && let Some(dir) = dir {
        let path = dir.join("arke.log");
        let (tx, rx) = flume::unbounded::<Vec<u8>>();
        let target = path.clone();
        let spawned = std::thread::Builder::new()
            .name("log".into())
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
                for chunk in rx {
                    let _ = file.write_all(&chunk);
                }
            });
        if spawned.is_ok() {
            let _ = LOG_PATH.set(path);
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
