//! The Windows context menu ("More options") is read and invoked in a separate
//! process, `arke.exe --shell-host`.
//!
//! Arke never loads third-party shell extensions in its own process, so a crashing or
//! hung extension can't take it down. Reading the menu always loads them, hence:
//!
//! - A standby host starts in the background and reads one menu up front, so slow
//!   first-time parts like "Send to" are already warm.
//! - The host is asked to read as soon as the context menu opens (`win_menu.rs` runs
//!   inside it), so the menu is usually ready by the time "More options" is reached.
//! - A read past the timeout means the host is stuck: kill it and start a new one later.
//!   A crash looks the same to Arke: the read just fails.
//! - An invoked item runs in the host that read it (some commands open dialogs or keep
//!   working in the background). That host then retires: it takes no new requests and
//!   exits once its windows have been closed for a while. A fresh standby replaces it.
//!
//! Transport is one JSON message per line over stdin/stdout. Arke reads and writes on
//! dedicated threads; the UI thread never waits on the host.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

use crate::shell::{ShellError, ShellMenu};

/// Starts arke.exe as a menu host only: no window, no tray, no settings.
pub(crate) const HOST_ARG: &str = "--shell-host";
/// A menu read longer than this counts as hung (a cold "Send to" takes about 0.7 s).
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// A retired host exits after its windows have been closed this long; some extensions
/// keep working after the command returns.
const LINGER: Duration = Duration::from_secs(20);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Reply for a query skipped because a later one replaced it.
const SUPERSEDED: &str = "superseded";

#[derive(Debug, Serialize, Deserialize)]
enum HostRequest {
    Query {
        id: u64,
        paths: Vec<PathBuf>,
        /// Menu for the folder background.
        #[serde(default)]
        background: bool,
    },
    Invoke {
        id: u64,
        session: u64,
        item: u32,
        client_point: (i32, i32),
        owner: Option<isize>,
    },
    Release {
        session: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum Reply {
    Menu(ShellMenu),
    Invoked,
}

/// One reply on the wire. Errors cross the process boundary as their display text and come
/// back as [`ShellError::Host`].
#[derive(Debug, Serialize, Deserialize)]
struct HostReply {
    id: u64,
    result: Result<Reply, String>,
}

// ───────────────────────── Host side ─────────────────────────

/// Entry point of `arke.exe --shell-host`. Serves requests from stdin until Arke closes it.
pub(crate) fn run() {
    serve(
        BufReader::new(std::io::stdin()),
        std::io::stdout(),
        true,
        LINGER,
    );
}

/// Serves requests until input ends, then returns once this process has had no
/// visible window for `linger`.
fn serve<R, W>(input: R, mut output: W, warm: bool, linger: Duration)
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let (tx, rx) = flume::unbounded::<HostRequest>();
    let _ = std::thread::Builder::new()
        .name("shell-host-in".into())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                if let Ok(request) = serde_json::from_str(&line)
                    && tx.send(request).is_err()
                {
                    break;
                }
            }
        });
    if warm {
        warm_up();
    }
    while let Ok(first) = rx.recv() {
        // Of queued queries only the last is read; earlier menus were already closed.
        let mut batch = vec![first];
        batch.extend(rx.drain());
        let last_query = batch
            .iter()
            .rposition(|r| matches!(r, HostRequest::Query { .. }));
        for (i, request) in batch.into_iter().enumerate() {
            let reply = match request {
                HostRequest::Query {
                    id,
                    paths,
                    background,
                } => HostReply {
                    id,
                    result: if Some(i) == last_query {
                        crate::win_menu::query(&paths, background)
                            .map(Reply::Menu)
                            .map_err(|e| e.to_string())
                    } else {
                        Err(SUPERSEDED.into())
                    },
                },
                HostRequest::Invoke {
                    id,
                    session,
                    item,
                    client_point,
                    owner,
                } => HostReply {
                    id,
                    result: crate::win_menu::invoke(session, item, client_point, owner)
                        .map(|()| Reply::Invoked)
                        .map_err(|e| e.to_string()),
                },
                HostRequest::Release { session } => {
                    crate::win_menu::release(session);
                    continue;
                }
            };
            let Ok(mut line) = serde_json::to_string(&reply) else {
                continue;
            };
            line.push('\n');
            let _ = output
                .write_all(line.as_bytes())
                .and_then(|()| output.flush());
        }
    }
    let step = Duration::from_millis(500);
    let mut idle = Duration::ZERO;
    while idle < linger {
        std::thread::sleep(step);
        idle = if has_visible_window() {
            Duration::ZERO
        } else {
            idle + step
        };
    }
    // After a command like Copy the clipboard data lives in this process; hand it to
    // the system before exiting.
    crate::win_menu::flush_clipboard();
}

/// Reads the menu for arke.exe once so extensions load and "Send to" builds its list.
fn warm_up() {
    if let Ok(exe) = std::env::current_exe()
        && let Ok(menu) = crate::win_menu::query(&[exe], false)
    {
        crate::win_menu::release(menu.session);
    }
}

/// Whether this process still has a visible window (extension dialogs, progress windows).
fn has_visible_window() -> bool {
    unsafe extern "system" fn check(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut pid = 0u32;
        // SAFETY: hwnd comes from EnumWindows; pid is a valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        // SAFETY: hwnd comes from EnumWindows.
        if pid == std::process::id() && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            // SAFETY: lparam is the &mut bool passed below, valid until EnumWindows returns.
            unsafe { *(lparam.0 as *mut bool) = true };
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut found = false;
    // SAFETY: the callback only uses the address of `found` while EnumWindows runs.
    unsafe {
        let _ = EnumWindows(Some(check), LPARAM((&raw mut found) as isize));
    }
    found
}

// ───────────────────────── Arke side ─────────────────────────

/// Requests awaiting a reply. `None` once the host has exited, so later requests fail
/// instead of waiting forever.
type Pending = Arc<Mutex<Option<HashMap<u64, flume::Sender<Result<Reply, ShellError>>>>>>;

struct Host {
    /// Host instance number, stored in the high 32 bits of each menu session so menus
    /// from an old host are recognized as stale.
    generation: u64,
    child: Child,
    /// Lines for the writer thread. Dropping it closes the host's stdin.
    tx: flume::Sender<String>,
    pending: Pending,
}

/// The current standby host.
static HOST: Mutex<Option<Host>> = Mutex::new(None);
/// When the host last failed to start (antivirus or group policy blocking child
/// processes). Retries are suppressed for a while and [`ShellError::HostBlocked`] is reported.
///
/// There is deliberately no fallback to reading in Arke's own process: that loads
/// third-party shell extensions into Arke and has crashed it in practice.
static SPAWN_FAILED: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const SPAWN_RETRY_AFTER: Duration = Duration::from_secs(30);
/// High 32 bits of the session for menus read inside Arke's process.
const IN_PROCESS: u64 = 0;

/// Whether menus are read in the host process. Only `ARKE_NO_SHELL_HOST` turns this off,
/// for development and debugging; extensions then load into Arke and "More options" is
/// read only when opened.
pub(crate) fn isolated() -> bool {
    static FORCED_OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    !*FORCED_OFF.get_or_init(|| std::env::var_os("ARKE_NO_SHELL_HOST").is_some())
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Why the menu host could not be started. Only logged; callers see
/// [`ShellError::HostBlocked`].
#[derive(Debug, thiserror::Error)]
enum SpawnError {
    /// `ARKE_TEST_HOST_BLOCKED` is set (simulates a machine that blocks child processes).
    #[error("{}", t!("menu.host_start_failed", error = "ARKE_TEST_HOST_BLOCKED"))]
    TestBlocked,
    /// Arke's own exe path is unknown.
    #[error("{0}")]
    Exe(#[source] std::io::Error),
    /// The child process could not be started.
    #[error("{}", t!("menu.host_start_failed", error = .0))]
    Spawn(#[source] std::io::Error),
    /// The child started without piped stdin/stdout.
    #[error("{}", t!("menu.host_start_failed", error = "stdio"))]
    Stdio,
    /// The pipe reader or writer thread could not start.
    #[error("{}", t!("menu.host_start_failed", error = .0))]
    Thread(#[source] std::io::Error),
}

fn spawn_host() -> Result<Host, SpawnError> {
    // ARKE_TEST_HOST_BLOCKED simulates a machine that blocks child processes.
    if std::env::var_os("ARKE_TEST_HOST_BLOCKED").is_some() {
        return Err(SpawnError::TestBlocked);
    }
    let exe = std::env::current_exe().map_err(SpawnError::Exe)?;
    let mut child = Command::new(exe)
        .arg(HOST_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(SpawnError::Spawn)?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err(SpawnError::Stdio);
    };
    let (tx, rx) = flume::unbounded::<String>();
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
    let spawned = std::thread::Builder::new()
        .name("shell-host-write".into())
        .spawn(move || {
            for line in rx {
                if stdin
                    .write_all(line.as_bytes())
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        })
        .and_then(|_| {
            let pending = pending.clone();
            std::thread::Builder::new()
                .name("shell-host-read".into())
                .spawn(move || {
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else { break };
                        let Ok(reply) = serde_json::from_str::<HostReply>(&line) else {
                            continue;
                        };
                        let waiter = pending.lock().as_mut().and_then(|m| m.remove(&reply.id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(reply.result.map_err(ShellError::Host));
                        }
                    }
                    // Host exited: fail everything still waiting.
                    pending.lock().take();
                })
        });
    if let Err(e) = spawned {
        let _ = child.kill();
        return Err(SpawnError::Thread(e));
    }
    Ok(Host {
        generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
        child,
        tx,
        pending,
    })
}

/// Runs `f` on the current host, starting one if none is alive.
fn with_host<T>(f: impl FnOnce(&mut Host) -> Result<T, ShellError>) -> Result<T, ShellError> {
    let mut slot = HOST.lock();
    if slot
        .as_mut()
        .is_some_and(|h| !matches!(h.child.try_wait(), Ok(None)))
    {
        *slot = None;
    }
    if slot.is_none() {
        // Check-then-set on SPAWN_FAILED is safe without holding its lock: `HOST` is held.
        if SPAWN_FAILED
            .lock()
            .is_some_and(|at| at.elapsed() < SPAWN_RETRY_AFTER)
        {
            return Err(ShellError::HostBlocked);
        }
        match spawn_host() {
            Ok(host) => {
                *SPAWN_FAILED.lock() = None;
                *slot = Some(host);
            }
            Err(e) => {
                log::warn!("menu host failed to start: {e}");
                // The menu has room for one line; details go to the log.
                *SPAWN_FAILED.lock() = Some(std::time::Instant::now());
                return Err(ShellError::HostBlocked);
            }
        }
    }
    match slot.as_mut() {
        Some(host) => f(host),
        None => Err(ShellError::HostGone),
    }
}

fn submit(
    host: &Host,
    make: impl FnOnce(u64) -> HostRequest,
) -> Result<flume::Receiver<Result<Reply, ShellError>>, ShellError> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (done, wait) = flume::bounded(1);
    host.pending
        .lock()
        .as_mut()
        .ok_or(ShellError::HostGone)?
        .insert(id, done);
    let mut line = serde_json::to_string(&make(id)).map_err(ShellError::Encode)?;
    line.push('\n');
    host.tx.send(line).map_err(|_| ShellError::HostGone)?;
    Ok(wait)
}

/// Starts the standby host, which warms itself up. Call from a background thread.
pub(crate) fn prewarm() {
    if !isolated() {
        return;
    }
    if let Err(e) = with_host(|_| Ok(())) {
        log::warn!("menu host did not start: {e}");
    }
}

/// Reads the Windows context menu for `paths`. Call from a background thread; waits at
/// most [`QUERY_TIMEOUT`].
pub(crate) fn query(paths: &[PathBuf], background: bool) -> Result<ShellMenu, ShellError> {
    if !isolated() {
        return crate::win_menu::query(paths, background);
    }
    query_host(paths, background)
}

fn query_host(paths: &[PathBuf], background: bool) -> Result<ShellMenu, ShellError> {
    let (generation, wait) = with_host(|h| {
        let wait = submit(h, |id| HostRequest::Query {
            id,
            paths: paths.to_vec(),
            background,
        })?;
        Ok((h.generation, wait))
    })?;
    match wait.recv_timeout(QUERY_TIMEOUT) {
        Ok(Ok(Reply::Menu(mut menu))) => {
            menu.session = (generation << 32) | (menu.session & 0xFFFF_FFFF);
            Ok(menu)
        }
        Ok(Err(e)) => Err(e),
        Err(flume::RecvTimeoutError::Timeout) => {
            // Hung: kill it; the next request starts a new one.
            let stuck = HOST.lock().take_if(|h| h.generation == generation);
            if let Some(mut host) = stuck {
                let _ = host.child.kill();
            }
            Err(ShellError::HostTimeout)
        }
        Ok(Ok(Reply::Invoked)) | Err(flume::RecvTimeoutError::Disconnected) => {
            Err(ShellError::HostGone)
        }
    }
}

/// Invokes a menu item in the host that read it. That host then retires and a new
/// standby starts. Call from a background thread.
pub(crate) fn invoke(
    session: u64,
    item: u32,
    client_point: (i32, i32),
    owner: Option<isize>,
) -> Result<(), ShellError> {
    let generation = session >> 32;
    if generation == IN_PROCESS {
        return crate::win_menu::invoke(session, item, client_point, owner);
    }
    let host = HOST
        .lock()
        .take_if(|h| h.generation == generation)
        .ok_or(ShellError::MenuExpired)?;
    // Arke owns the foreground; let the host bring any dialog the command opens to the front.
    // SAFETY: plain Win32 call taking the host's PID.
    let _ = unsafe { AllowSetForegroundWindow(host.child.id()) };
    let wait = submit(&host, |id| HostRequest::Invoke {
        id,
        session: session & 0xFFFF_FFFF,
        item,
        client_point,
        owner,
    })?;
    prewarm();
    let result = match wait.recv() {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ShellError::HostGone),
    };
    // Closing stdin retires the host; it exits once its windows are gone.
    drop(host);
    result
}

/// Releases a menu that was closed without a choice. Non-blocking; safe on the UI thread.
pub(crate) fn release(session: u64) {
    let generation = session >> 32;
    if generation == IN_PROCESS {
        crate::win_menu::release(session);
        return;
    }
    let request = HostRequest::Release {
        session: session & 0xFFFF_FFFF,
    };
    // Spawning a host holds the lock briefly; keep that off the UI thread.
    let _ = std::thread::Builder::new()
        .name("shell-host-release".into())
        .spawn(move || {
            let slot = HOST.lock();
            if let Some(host) = slot.as_ref().filter(|h| h.generation == generation)
                && let Ok(mut line) = serde_json::to_string(&request)
            {
                line.push('\n');
                let _ = host.tx.send(line);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::ShellMenuEntry;

    fn count(entries: &[ShellMenuEntry]) -> usize {
        entries
            .iter()
            .map(|e| match e {
                ShellMenuEntry::Submenu { children, .. } => 1 + count(children),
                _ => 1,
            })
            .sum()
    }

    /// Host side reads a real menu and only serves the last queued query.
    #[test]
    fn host_serves_real_menus() {
        let notepad = PathBuf::from(r"C:\Windows\notepad.exe");
        if !notepad.exists() {
            return;
        }
        let mut input = String::new();
        for id in [1, 2] {
            input += &serde_json::to_string(&HostRequest::Query {
                id,
                paths: vec![notepad.clone()],
                background: false,
            })
            .unwrap();
            input.push('\n');
        }
        let mut output = Vec::new();
        serve(
            std::io::Cursor::new(input.into_bytes()),
            &mut output,
            false,
            Duration::ZERO,
        );
        let replies: Vec<HostReply> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        // The first query is either served or superseded, depending on whether the reader
        // thread queued both lines before the loop drained them.
        let first = replies.iter().find(|r| r.id == 1).unwrap();
        assert!(
            matches!(&first.result, Ok(Reply::Menu(_)))
                || matches!(&first.result, Err(e) if e == SUPERSEDED)
        );
        let second = replies.iter().find(|r| r.id == 2).unwrap();
        match &second.result {
            Ok(Reply::Menu(menu)) => {
                assert!(count(&menu.entries) > 3, "{menu:?}");
                // Built-in items carry canonical verbs, which Arke uses to drop duplicates.
                assert!(
                    menu.entries.iter().any(|e| matches!(
                        e,
                        ShellMenuEntry::Item { verb: Some(v), .. } if v.eq_ignore_ascii_case("copy")
                    )),
                    "{menu:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn menu_icons_cross_the_wire() {
        let menu = ShellMenu {
            session: 3,
            entries: vec![ShellMenuEntry::Item {
                id: 9,
                verb: Some("copy".into()),
                label: "Compress".into(),
                enabled: true,
                checked: false,
                default: true,
                icon: Some(arke_core::thumbs::Thumbnail {
                    width: 1,
                    height: 2,
                    rgba: vec![0, 15, 16, 255, 1, 2, 3, 4],
                }),
            }],
        };
        let json = serde_json::to_string(&menu).unwrap();
        let back: ShellMenu = serde_json::from_str(&json).unwrap();
        match &back.entries[0] {
            ShellMenuEntry::Item { icon: Some(t), .. } => {
                assert_eq!((t.width, t.height), (1, 2));
                assert_eq!(t.rgba, [0, 15, 16, 255, 1, 2, 3, 4]);
            }
            other => panic!("{other:?}"),
        }
        // Icons whose size doesn't match the data are rejected.
        let bad = json.replace("\"000f10ff01020304\"", "\"000f\"");
        assert_ne!(bad, json);
        assert!(serde_json::from_str::<ShellMenu>(&bad).is_err());
    }
}
