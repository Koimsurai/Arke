//! "Space opens QuickLook": hands Space to QuickLook (QL-Win) when it is running or installed.
//!
//! QuickLook listens on a named pipe, `\\.\pipe\QuickLook.App.Pipe.<user SID>`, and reads one
//! UTF-8 line per connection, `<message>|<path>`. `QuickLook.App.PipeMessages.Toggle` toggles its
//! preview of the path, so pressing Space again on the same item closes it, as Space does in File
//! Explorer with QuickLook. It's what QuickLook's own second instance sends for its command line,
//! and what other file managers (Files) send, so it works whatever the install (installer,
//! portable, Scoop, Microsoft Store) as long as QuickLook is running. The path is the last field:
//! QuickLook 3.x takes everything after the first `|` as the path, 4.x reads options from a third
//! field, so no trailing `|` is sent (Windows paths can't contain one). QuickLook runs in its own
//! process; nothing of it is loaded into Arke.
//!
//! When the pipe isn't there (QuickLook isn't running), Arke starts `QuickLook.exe "<path>"`
//! instead, if it found the exe: that starts QuickLook, which then shows the path. The exe is looked
//! for, most likely first:
//! - in the default install folders: `%LOCALAPPDATA%\Programs\QuickLook` (the MSI is per user,
//!   `ALLUSERS=2` + `MSIINSTALLPERUSER=1`, so its `ProgramFilesFolder` lands there; the setup exe
//!   prefers it too), Program Files, and Scoop's `apps\quicklook\current`;
//! - in the setup exe's uninstall entry (`InstallLocation` under `...\Uninstall\QuickLook`, read
//!   only; the MSI sets no `InstallLocation`);
//! - through QuickLook's shortcuts, which follow a folder chosen in the installer or a portable
//!   copy: the Start menu one the installers make and the Startup one QuickLook makes for "Run at
//!   startup". Only a target named `QuickLook.exe` that exists counts.
//!
//! Settings offers the option when either is found. The pipe is checked at startup, whenever an
//! Arke window is activated or Settings opens (no polling), and on each hand-off, so a QuickLook
//! started or quit after Arke is noticed. The check only waits on the pipe's name (`WaitNamedPipeW`,
//! 1 ms at most) and never connects, so QuickLook sees nothing. The exe is searched for on the same
//! occasions while it isn't found (at most every [`SEARCH_EVERY`]; a resident Arke can outlive a
//! QuickLook install), and checked again if it disappears.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rust_i18n::t;
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, GetLastError};
use windows::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

/// QuickLook's pipe, before the user's SID (`PipeServerManager.PipeName` in QuickLook).
const PIPE_PREFIX: &str = r"\\.\pipe\QuickLook.App.Pipe.";
/// Toggles the preview of the path (`PipeMessages.Toggle` in QuickLook).
const TOGGLE: &str = "QuickLook.App.PipeMessages.Toggle";
/// How long a hand-off waits while QuickLook's pipe is busy with another message. QuickLook takes
/// one connection at a time and is listening again right after reading a line.
const SEND_TIMEOUT: Duration = Duration::from_millis(500);

/// While the exe isn't found, how often a re-check may search for it again.
const SEARCH_EVERY: Duration = Duration::from_secs(30);
/// QuickLook's uninstall entry, written by its setup exe (MicaSetup, `KeyName` "QuickLook").
const UNINSTALL_KEYS: [&str; 2] = [
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\QuickLook",
    r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\QuickLook",
];

/// The exe found by the last search, and when that search ran (`None`: not yet).
static EXE: Mutex<(Option<PathBuf>, Option<Instant>)> = Mutex::new((None, None));
/// Whether QuickLook's pipe was there at the last check or hand-off.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Looks for QuickLook in the background, so Settings can offer the option.
pub(crate) fn detect_in_background() {
    crate::explorer::spawn_thread("quicklook-detect", || {
        recheck();
        if let Some(exe) = exe() {
            log::info!("QuickLook found at {}", exe.display());
        }
        if RUNNING.load(Ordering::Relaxed) {
            log::info!("QuickLook is running");
        }
    });
}

/// Looks again: for QuickLook's pipe every time, for the exe while it isn't found (or is gone).
/// Returns whether [`available`] changed. Background thread with no COM apartment of its own
/// (reading shortcuts sets one up); touches the disk and reads the registry.
pub(crate) fn recheck() -> bool {
    // Tests don't see this machine's QuickLook; they use `pretend_installed`.
    if cfg!(test) {
        return false;
    }
    let before = available();
    let (known, searched) = EXE.lock().clone();
    let search = match &known {
        Some(exe) => !exe.is_file(),
        None => searched.is_none_or(|at| at.elapsed() >= SEARCH_EVERY),
    };
    if search {
        let found = find();
        *EXE.lock() = (found, Some(Instant::now()));
    }
    RUNNING.store(pipe_name().is_some_and(pipe_exists), Ordering::Relaxed);
    available() != before
}

/// Whether Space can go to QuickLook: it's running, or its exe was found (`false` also while
/// still looking).
pub(crate) fn available() -> bool {
    RUNNING.load(Ordering::Relaxed) || EXE.lock().0.is_some()
}

/// QuickLook's exe, if it was found (`None` also while still looking).
pub(crate) fn exe() -> Option<PathBuf> {
    EXE.lock().0.clone()
}

/// Tests: act as if QuickLook were installed at `exe`.
#[cfg(test)]
pub(crate) fn pretend_installed(exe: PathBuf) {
    *EXE.lock() = (Some(exe), Some(Instant::now()));
}

/// Tests: act as if QuickLook were running from somewhere it isn't looked for.
#[cfg(test)]
pub(crate) fn pretend_running() {
    RUNNING.store(true, Ordering::Relaxed);
}

/// Toggles QuickLook's preview of `item`: over its pipe when QuickLook is running, otherwise by
/// starting `exe` with it. Returns once the message is sent or QuickLook has started. Background
/// thread.
pub(crate) fn toggle(item: &Path, exe: Option<&Path>) -> io::Result<()> {
    debug_assert!(
        !arke_core::fs::is_ui_thread(),
        "QuickLook must not be called on the UI thread"
    );
    toggle_at(pipe_name(), item, exe, SEND_TIMEOUT, |exe, item| {
        // A plain argument: Rust quotes it the way .NET reads it back.
        std::process::Command::new(exe)
            .arg(item)
            .spawn()
            .map(|_| ())
    })
}

/// [`toggle`] with the pipe and the way the exe is started given (tests use their own).
fn toggle_at(
    pipe: Option<&str>,
    item: &Path,
    exe: Option<&Path>,
    timeout: Duration,
    start: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let start_exe = || match exe {
        Some(exe) => start(exe, item),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            t!("error.quicklook_not_running").into_owned(),
        )),
    };
    // A path that isn't valid Unicode can't be written as UTF-8; the command line can carry it.
    let (Some(pipe), Some(message)) = (pipe, toggle_message(item)) else {
        return start_exe();
    };
    match send(pipe, &message, timeout) {
        Ok(()) => {
            RUNNING.store(true, Ordering::Relaxed);
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            RUNNING.store(false, Ordering::Relaxed);
            start_exe()
        }
        // Running but not answering: a second QuickLook would only wait on the same pipe.
        Err(err) => Err(err),
    }
}

/// The line that toggles QuickLook's preview of `item`, or `None` if the path isn't valid Unicode.
/// `\r\n` ends it, as .NET's `WriteLine` does.
fn toggle_message(item: &Path) -> Option<String> {
    item.to_str().map(|path| format!("{TOGGLE}|{path}\r\n"))
}

/// This user's QuickLook pipe, or `None` if the SID can't be read.
fn pipe_name() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| arke_indexd::sec::current_user_sid().map(|sid| pipe_path(&sid)))
        .as_deref()
}

/// QuickLook's pipe for the user with this SID.
fn pipe_path(sid: &str) -> String {
    format!("{PIPE_PREFIX}{sid}")
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Whether the pipe exists. Only waits on the name, never connects, so the server sees nothing.
fn pipe_exists(pipe: &str) -> bool {
    let name = wide(pipe);
    // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
    let free = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) }.as_bool();
    // SAFETY: reads this thread's last error, set by the call above.
    free || unsafe { GetLastError() } == ERROR_SEM_TIMEOUT
}

/// Connects to `pipe` and writes `message`, waiting up to `timeout` while the pipe is busy with
/// another client. Fails with `NotFound` when the pipe isn't there.
fn send(pipe: &str, message: &str, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut file = loop {
        let opened = std::fs::OpenOptions::new()
            .write(true)
            // QuickLook only reads; it has no reason to act as this user.
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(pipe);
        match opened {
            Ok(file) => break file,
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY.0.cast_signed()) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "QuickLook's pipe stayed busy",
                    ));
                }
                let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
                let name = wide(pipe);
                // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), ms) };
            }
            Err(err) => return Err(err),
        }
    };
    // Closing the handle after the write is fine: the server still reads what was written.
    file.write_all(message.as_bytes())
}

/// Looks in the install folders, the uninstall entry and the shortcuts, in that order. Background
/// thread (touches the disk, reads the registry, loads shortcuts with COM).
fn find() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let installed = || {
        UNINSTALL_KEYS.iter().find_map(|key| {
            [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
                .into_iter()
                .find_map(|root| crate::win::read_reg_string(root, key, "InstallLocation"))
                .filter(|dir| !dir.trim().is_empty())
        })
    };
    find_with(
        |name| std::env::var_os(name),
        installed,
        |link| crate::win::read_shortcut(link).map(|(target, _)| target),
    )
}

/// [`find`] with the environment, the uninstall entry's folder and shortcut reading given.
fn find_with(
    env: impl Fn(&str) -> Option<OsString>,
    install_location: impl FnOnce() -> Option<String>,
    shortcut_target: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let is_exe = |path: &Path| {
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("QuickLook.exe"))
            && path.is_file()
    };
    if let Some(exe) = candidates(&env).into_iter().find(|exe| exe.is_file()) {
        return Some(exe);
    }
    if let Some(exe) = install_location()
        .map(|dir| PathBuf::from(dir.trim().trim_matches('"')).join("QuickLook.exe"))
        .filter(|exe| is_exe(exe))
    {
        return Some(exe);
    }
    shortcuts(&env)
        .into_iter()
        .filter(|link| link.is_file())
        .filter_map(|link| shortcut_target(&link))
        .find(|target| is_exe(target))
}

/// QuickLook's shortcuts: the Start menu one its installers make (per user for the MSI, for all
/// users for a per-machine setup) and the Startup one it makes itself for "Run at startup".
fn shortcuts(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let programs = |root: OsString| {
        PathBuf::from(root)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    };
    let mut out = Vec::new();
    if let Some(roaming) = env("APPDATA") {
        let programs = programs(roaming);
        out.push(programs.join("QuickLook.lnk"));
        out.push(programs.join("Startup").join("QuickLook.lnk"));
    }
    if let Some(common) = env("ProgramData") {
        out.push(programs(common).join("QuickLook.lnk"));
    }
    out
}

/// Where an installed QuickLook would be, from environment variables, most likely first.
fn candidates(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let exe = |dir: PathBuf| dir.join("QuickLook.exe");
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(local) = env("LOCALAPPDATA") {
        push(exe(PathBuf::from(local).join("Programs").join("QuickLook")));
    }
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = env(var) {
            push(exe(PathBuf::from(dir).join("QuickLook")));
        }
    }
    let scoop = env("SCOOP")
        .map(PathBuf::from)
        .or_else(|| env("USERPROFILE").map(|home| PathBuf::from(home).join("scoop")));
    if let Some(scoop) = scoop {
        push(exe(scoop.join("apps").join("quicklook").join("current")));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read as _;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::sync::atomic::AtomicU32;
    use std::thread::JoinHandle;

    use windows::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use super::*;

    /// A pipe name of this test run's own, never QuickLook's.
    fn test_pipe() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\arke-test-quicklook-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A server shaped like QuickLook's: one inbound byte-mode instance that takes `connections`
    /// clients one after another and returns what each wrote before closing. The pipe exists once
    /// this returns.
    fn serve(pipe: &str, connections: usize) -> JoinHandle<Vec<String>> {
        let name = wide(pipe);
        // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                4096,
                0,
                None,
            )
        };
        assert!(!handle.is_invalid(), "test pipe not created");
        // SAFETY: handle is a new, valid pipe handle that nothing else owns.
        let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        std::thread::spawn(move || {
            let mut pipe = File::from(owned);
            let mut got = Vec::new();
            for _ in 0..connections {
                let raw = HANDLE(pipe.as_raw_handle());
                // SAFETY: raw is the valid pipe handle owned by `pipe`.
                // A client may have connected (and even written and closed) before this call;
                // what it wrote is still there to read.
                if let Err(err) = unsafe { ConnectNamedPipe(raw, None) } {
                    assert!(
                        [ERROR_PIPE_CONNECTED, ERROR_NO_DATA]
                            .iter()
                            .any(|e| err.code() == e.to_hresult()),
                        "connect: {err}"
                    );
                }
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes)
                    .expect("read the client's message");
                got.push(String::from_utf8(bytes).expect("UTF-8 message"));
                // SAFETY: raw is the valid pipe handle owned by `pipe`.
                unsafe { DisconnectNamedPipe(raw) }.expect("disconnect");
            }
            got
        })
    }

    fn never_started(_: &Path, _: &Path) -> io::Result<()> {
        panic!("the exe must not be started while the pipe answers");
    }

    #[test]
    fn the_pipe_is_named_after_the_user() {
        assert_eq!(
            pipe_path("S-1-5-21-1-2-3-1001"),
            r"\\.\pipe\QuickLook.App.Pipe.S-1-5-21-1-2-3-1001"
        );
        let sid = arke_indexd::sec::current_user_sid().expect("this user's SID");
        assert_eq!(pipe_name(), Some(pipe_path(&sid).as_str()));
    }

    #[test]
    fn toggle_message_is_one_line_with_the_path_last() {
        assert_eq!(
            toggle_message(Path::new(r"C:\Users\u\資料\a b.txt")).as_deref(),
            Some("QuickLook.App.PipeMessages.Toggle|C:\\Users\\u\\資料\\a b.txt\r\n")
        );
        assert_eq!(
            toggle_message(Path::new(r"D:\")).as_deref(),
            Some("QuickLook.App.PipeMessages.Toggle|D:\\\r\n")
        );
        // An unpaired surrogate can't be written as UTF-8.
        let odd = PathBuf::from(OsString::from_wide(&[
            u16::from(b'C'),
            u16::from(b':'),
            u16::from(b'\\'),
            0xD800,
        ]));
        assert_eq!(toggle_message(&odd), None);
    }

    #[test]
    fn checking_for_the_pipe_does_not_connect() {
        let pipe = test_pipe();
        assert!(!pipe_exists(&pipe), "no server yet");
        let server = serve(&pipe, 1);
        assert!(pipe_exists(&pipe));
        assert!(pipe_exists(&pipe), "still free after a check");
        send(&pipe, "hello\r\n", Duration::from_secs(5)).expect("send");
        assert_eq!(
            server.join().expect("server"),
            ["hello\r\n"],
            "the checks took no connection"
        );
    }

    #[test]
    fn toggle_writes_the_message_to_the_pipe() {
        let pipe = test_pipe();
        let server = serve(&pipe, 2);
        let item = Path::new(r"C:\q\a.txt");
        let exe = Path::new(r"C:\QuickLook\QuickLook.exe");
        for _ in 0..2 {
            toggle_at(
                Some(&pipe),
                item,
                Some(exe),
                Duration::from_secs(5),
                never_started,
            )
            .expect("toggle");
        }
        let line = "QuickLook.App.PipeMessages.Toggle|C:\\q\\a.txt\r\n";
        assert_eq!(server.join().expect("server"), [line, line]);
        assert!(RUNNING.load(Ordering::Relaxed));
    }

    #[test]
    fn a_busy_pipe_is_waited_for() {
        let pipe = test_pipe();
        let server = serve(&pipe, 2);
        let mut first = std::fs::OpenOptions::new()
            .write(true)
            .open(&pipe)
            .expect("first client");
        let sender = std::thread::spawn(move || send(&pipe, "second\r\n", Duration::from_secs(10)));
        // Let the second client find the pipe busy before the first one is done.
        std::thread::sleep(Duration::from_millis(200));
        first.write_all(b"first\r\n").expect("write");
        drop(first);
        sender.join().expect("sender").expect("second send");
        assert_eq!(server.join().expect("server"), ["first\r\n", "second\r\n"]);
    }

    #[test]
    fn a_pipe_that_stays_busy_times_out() {
        let pipe = test_pipe();
        let server = serve(&pipe, 1);
        let mut holder = std::fs::OpenOptions::new()
            .write(true)
            .open(&pipe)
            .expect("holding client");
        let err = send(&pipe, "late\r\n", Duration::from_millis(100)).expect_err("busy");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Busy but there: QuickLook isn't started a second time.
        let err = toggle_at(
            Some(&pipe),
            Path::new(r"C:\q\a.txt"),
            Some(Path::new(r"C:\QuickLook\QuickLook.exe")),
            Duration::from_millis(100),
            never_started,
        )
        .expect_err("busy");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        holder.write_all(b"done\r\n").expect("write");
        drop(holder);
        assert_eq!(server.join().expect("server"), ["done\r\n"]);
    }

    #[test]
    fn without_the_pipe_the_exe_is_started_with_the_item() {
        let pipe = test_pipe();
        let item = Path::new(r"C:\q\a.txt");
        let exe = Path::new(r"C:\QuickLook\QuickLook.exe");
        assert_eq!(
            send(&pipe, "x\r\n", Duration::from_secs(1))
                .expect_err("no server")
                .kind(),
            io::ErrorKind::NotFound
        );
        RUNNING.store(true, Ordering::Relaxed);
        let started = Mutex::new(Vec::new());
        toggle_at(
            Some(&pipe),
            item,
            Some(exe),
            Duration::from_secs(1),
            |e, i| {
                started.lock().push((e.to_path_buf(), i.to_path_buf()));
                Ok(())
            },
        )
        .expect("started");
        assert_eq!(*started.lock(), [(exe.to_path_buf(), item.to_path_buf())]);
        assert!(!RUNNING.load(Ordering::Relaxed), "noticed it quit");

        // Neither running nor installed where it's looked for.
        let err = toggle_at(
            Some(&pipe),
            item,
            None,
            Duration::from_secs(1),
            never_started,
        )
        .expect_err("nothing to hand to");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn shortcuts_are_the_start_menu_and_startup_ones() {
        let env = |name: &str| match name {
            "APPDATA" => Some(OsString::from(r"C:\Users\u\AppData\Roaming")),
            "ProgramData" => Some(OsString::from(r"C:\ProgramData")),
            _ => None,
        };
        let menu = r"Microsoft\Windows\Start Menu\Programs";
        assert_eq!(
            shortcuts(env),
            [
                PathBuf::from(format!(r"C:\Users\u\AppData\Roaming\{menu}\QuickLook.lnk")),
                PathBuf::from(format!(
                    r"C:\Users\u\AppData\Roaming\{menu}\Startup\QuickLook.lnk"
                )),
                PathBuf::from(format!(r"C:\ProgramData\{menu}\QuickLook.lnk")),
            ]
        );
    }

    /// A sandbox folder under %TEMP%, removed on drop.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("arke-test-quicklook-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("sandbox");
            Self(dir)
        }

        /// An empty file at `rel`, with its folders.
        fn file(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("folders");
            std::fs::write(&path, b"").expect("file");
            path
        }

        /// The environment of a user whose profile folders are all in the sandbox.
        fn env(&self) -> impl Fn(&str) -> Option<OsString> + '_ {
            |name: &str| {
                let sub = match name {
                    "LOCALAPPDATA" => "local",
                    "APPDATA" => "roaming",
                    "ProgramData" => "programdata",
                    "ProgramW6432" | "ProgramFiles" | "ProgramFiles(x86)" => "programfiles",
                    "USERPROFILE" => "home",
                    _ => return None,
                };
                Some(self.0.join(sub).into_os_string())
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MENU_LINK: &str = r"roaming\Microsoft\Windows\Start Menu\Programs\QuickLook.lnk";

    #[test]
    fn an_install_folder_chosen_in_the_installer_is_found_through_its_shortcut() {
        let sb = Sandbox::new("menu");
        let none = || None;
        assert_eq!(
            find_with(sb.env(), none, |_| panic!("no shortcut yet")),
            None
        );

        let exe = sb.file(r"D-Apps\QuickLook\QuickLook.exe");
        let link = sb.file(MENU_LINK);
        let target = |l: &Path| (l == link).then(|| exe.clone());
        assert_eq!(find_with(sb.env(), none, target), Some(exe.clone()));

        // A shortcut to something else, or to an exe that's gone, doesn't count.
        let other = sb.file(r"D-Apps\Other\Other.exe");
        assert_eq!(find_with(sb.env(), none, |_| Some(other.clone())), None);
        let gone = sb.0.join(r"Gone\QuickLook.exe");
        assert_eq!(find_with(sb.env(), none, |_| Some(gone.clone())), None);

        // The default folder comes first.
        let default = sb.file(r"local\Programs\QuickLook\QuickLook.exe");
        assert_eq!(find_with(sb.env(), none, target), Some(default));
    }

    #[test]
    fn the_setup_exes_install_location_is_used() {
        let sb = Sandbox::new("arp");
        let exe = sb.file(r"Tools\QuickLook\QuickLook.exe");
        let dir = exe.parent().expect("dir").display().to_string();
        let no_link = |_: &Path| -> Option<PathBuf> { panic!("the uninstall entry was enough") };
        assert_eq!(
            find_with(sb.env(), || Some(format!("\"{dir}\\\"")), no_link),
            Some(exe)
        );
        assert_eq!(
            find_with(
                sb.env(),
                || Some(sb.0.join("Empty").display().to_string()),
                |_| None
            ),
            None
        );
    }

    #[test]
    fn a_real_start_menu_shortcut_is_read() {
        use crate::shell::Shell as _;
        let sb = Sandbox::new("lnk");
        let exe = sb.file(r"Portable\QuickLook\QuickLook.exe");
        let link = sb.0.join(MENU_LINK);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("menu folder");
        let made = crate::win::WinShell
            .create_shortcuts(std::slice::from_ref(&exe), link.parent().expect("parent"))
            .expect("shortcut");
        std::fs::rename(&made[0], &link).expect("rename");
        let read = |l: &Path| crate::win::read_shortcut(l).map(|(target, _)| target);
        // The same file either way: a shortcut may give the temp folder's short (8.3) name.
        let canonical = |p: &Path| std::fs::canonicalize(p).expect("exists");
        let found = find_with(sb.env(), || None, read);
        assert_eq!(found.as_deref().map(canonical), Some(canonical(&exe)));
    }

    #[test]
    fn looks_in_the_install_folders_and_scoop() {
        let env = |name: &str| match name {
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\u\AppData\Local")),
            "ProgramW6432" | "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
            "ProgramFiles(x86)" => Some(OsString::from(r"C:\Program Files (x86)")),
            "USERPROFILE" => Some(OsString::from(r"C:\Users\u")),
            _ => None,
        };
        assert_eq!(
            candidates(env),
            [
                PathBuf::from(r"C:\Users\u\AppData\Local\Programs\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Program Files\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Program Files (x86)\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Users\u\scoop\apps\quicklook\current\QuickLook.exe"),
            ]
        );
        let scoop = |name: &str| (name == "SCOOP").then(|| OsString::from(r"D:\scoop"));
        assert_eq!(
            candidates(scoop),
            [PathBuf::from(
                r"D:\scoop\apps\quicklook\current\QuickLook.exe"
            )]
        );
    }
}
