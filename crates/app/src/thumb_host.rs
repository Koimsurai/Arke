//! Video thumbnails come from Windows (`IShellItemImageFactory`, the thumbnails File Explorer
//! shows) and are asked for in a separate process, `arke.exe --thumb-host`.
//!
//! Video decoders crash and hang on damaged or unusual files, whoever made them, so they stay out
//! of Arke's process twice over: Windows already runs thumbnail providers in its own surrogate
//! process, and the call that waits on it runs in this host.
//!
//! - A request past [`TIMEOUT`] means the host is stuck on that file: it is killed and the file
//!   counts as having no thumbnail. The next request starts a new host.
//! - A host that exits mid-request (a crash, or its idle exit racing the request) gets the request
//!   once more in a new host; failing again, the file counts as having none.
//! - A host that can't start (blocked by policy or antivirus) isn't tried again for
//!   [`SPAWN_COOLDOWN`], and the files asked for meanwhile aren't marked as having none.
//! - Hosts belong to a job object that ends them when Arke ends, even one stuck in a decoder.
//! - A host with nothing to do for [`IDLE`] exits; the next request starts another.
//!
//! Transport is one JSON message per line over stdin/stdout; the picture comes back compressed
//! ([`arke_core::thumb_cache::encode`]) as hex. Only Arke's background loader thread talks to it.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use arke_core::thumbs::Thumbnail;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Starts arke.exe as a thumbnail host only: no window, no tray, no settings.
pub(crate) const HOST_ARG: &str = "--thumb-host";
/// Longest a thumbnail may take; a cold one of a large 8K video takes a few seconds.
#[cfg(not(test))]
const TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(test)]
const TIMEOUT: Duration = Duration::from_millis(1500);
/// The host exits after this long without a request.
#[cfg(not(test))]
const IDLE: Duration = Duration::from_secs(60);
#[cfg(test)]
const IDLE: Duration = Duration::from_millis(400);
/// After the host failed to start, how long before trying again.
#[cfg(not(test))]
const SPAWN_COOLDOWN: Duration = Duration::from_secs(60);
#[cfg(test)]
const SPAWN_COOLDOWN: Duration = Duration::from_millis(300);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// What a request asks the host to make.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
enum Kind {
    /// Windows' thumbnail of a video.
    #[default]
    Video,
    /// First page of a PDF, from the Windows PDF renderer.
    Pdf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    id: u64,
    #[serde(default)]
    kind: Kind,
    path: PathBuf,
    max_side: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    id: u64,
    /// The compressed picture as hex, `None` when Windows has no thumbnail for the file.
    picture: Option<String>,
}

/// Why no answer came about the file itself.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HostError {
    /// The host process couldn't be started (or recently couldn't).
    #[error("thumbnail host can't start: {0}")]
    Unavailable(String),
}

// ───────────────────────── Host side ─────────────────────────

/// Entry point of `arke.exe --thumb-host`: answers requests until Arke closes its input or none
/// comes for [`IDLE`].
pub(crate) fn run() {
    let (tx, rx) = flume::unbounded::<Request>();
    let _ = std::thread::Builder::new()
        .name("thumb-host-in".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if let Ok(request) = serde_json::from_str(&line)
                    && tx.send(request).is_err()
                {
                    break;
                }
            }
        });
    let mut out = std::io::stdout().lock();
    while let Ok(request) = rx.recv_timeout(IDLE) {
        let picture = make(request.kind, &request.path, request.max_side)
            .and_then(|t| arke_core::thumb_cache::encode(&t))
            .map(|bytes| to_hex(&bytes));
        let reply = Reply {
            id: request.id,
            picture,
        };
        let Ok(line) = serde_json::to_string(&reply) else {
            continue;
        };
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            break;
        }
    }
}

#[cfg(not(test))]
fn make(kind: Kind, path: &Path, max_side: u32) -> Option<Thumbnail> {
    match kind {
        Kind::Video => crate::win_icons::video_thumbnail(path, max_side),
        Kind::Pdf => crate::win_pdf::render_page(path, 0, (max_side, max_side))
            .ok()
            .map(|r| r.image),
    }
}

/// In tests the host is the test binary itself, and file names choose what happens: `hang*` never
/// answers, `crash*` ends the process, `none*` has no thumbnail, `slow*` takes a moment.
#[cfg(test)]
fn make(_kind: Kind, path: &Path, max_side: u32) -> Option<Thumbnail> {
    let name = path
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned());
    if name.starts_with("hang") {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    if name.starts_with("crash") {
        std::process::exit(3);
    }
    if name.starts_with("none") {
        return None;
    }
    if name.starts_with("slow") {
        std::thread::sleep(Duration::from_millis(300));
    }
    let side = max_side.clamp(1, 32);
    Some(Thumbnail {
        width: side,
        height: side,
        rgba: vec![200; (side * side * 4) as usize],
    })
}

// ───────────────────────── Arke side ─────────────────────────

struct Host {
    child: Child,
    stdin: ChildStdin,
    replies: flume::Receiver<Reply>,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct State {
    host: Option<Host>,
    next_id: u64,
    /// Until when not to try starting a host again.
    blocked_until: Option<Instant>,
}

/// The running video host and its bookkeeping.
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
/// The PDF hosts, one per PDF loader thread so PDFs render two at a time as they did in-process;
/// separate from the video host, so a slow video never holds up PDFs and a stuck PDF never holds
/// up videos.
static PDF_STATES: [OnceLock<Mutex<State>>; 2] = [const { OnceLock::new() }; 2];
/// Which PDF host to wait for when both are busy.
static PDF_NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Windows' thumbnail for a video, fitted within `max_side`, from the host process. `Ok(None)`
/// when the file has none (unsupported format, no codec, or decoding it hung or crashed twice).
pub(crate) fn video_thumbnail(path: &Path, max_side: u32) -> Result<Option<Thumbnail>, HostError> {
    // One request at a time: the lock is held until the answer is in.
    ask(
        &mut STATE.get_or_init(Mutex::default).lock(),
        Kind::Video,
        path,
        max_side,
    )
}

/// First page of a PDF fitted within `max_side`, rendered in a host process: a malformed file
/// that stalls or crashes the renderer costs only that file. `Ok(None)` when it has none.
pub(crate) fn pdf_thumbnail(path: &Path, max_side: u32) -> Result<Option<Thumbnail>, HostError> {
    // A free host if there is one, else wait for one of them in turn.
    for state in &PDF_STATES {
        if let Some(mut state) = state.get_or_init(Mutex::default).try_lock() {
            return ask(&mut state, Kind::Pdf, path, max_side);
        }
    }
    let n = PDF_NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % PDF_STATES.len();
    ask(
        &mut PDF_STATES[n].get_or_init(Mutex::default).lock(),
        Kind::Pdf,
        path,
        max_side,
    )
}

fn ask(
    state: &mut State,
    kind: Kind,
    path: &Path,
    max_side: u32,
) -> Result<Option<Thumbnail>, HostError> {
    for attempt in 0..2 {
        if state.host.is_none() {
            if state.blocked_until.is_some_and(|t| Instant::now() < t) {
                return Err(HostError::Unavailable("started recently and failed".into()));
            }
            match spawn() {
                Ok(host) => {
                    state.blocked_until = None;
                    state.host = Some(host);
                }
                Err(err) => {
                    log::warn!("thumbnail host can't start: {err}");
                    state.blocked_until = Some(Instant::now() + SPAWN_COOLDOWN);
                    return Err(HostError::Unavailable(err.to_string()));
                }
            }
        }
        state.next_id += 1;
        let id = state.next_id;
        let line = serde_json::to_string(&Request {
            id,
            kind,
            path: path.to_path_buf(),
            max_side,
        })
        .map_err(|e| HostError::Unavailable(e.to_string()))?;
        let Some(host) = state.host.as_mut() else {
            continue;
        };
        // A host that exited while idle fails the write: start a new one and send once more.
        if writeln!(host.stdin, "{line}")
            .and_then(|()| host.stdin.flush())
            .is_err()
        {
            state.host = None;
            continue;
        }
        loop {
            match host.replies.recv_timeout(TIMEOUT) {
                Ok(reply) if reply.id == id => {
                    return Ok(reply
                        .picture
                        .and_then(|hex| from_hex(&hex))
                        .and_then(|bytes| arke_core::thumb_cache::decode(&bytes)));
                }
                // A late answer to a request given up on before.
                Ok(_) => {}
                Err(flume::RecvTimeoutError::Timeout) => {
                    log::warn!(
                        "thumbnail host: no thumbnail of {} after {} ms, ending it",
                        path.display(),
                        TIMEOUT.as_millis()
                    );
                    state.host = None;
                    return Ok(None);
                }
                Err(flume::RecvTimeoutError::Disconnected) => {
                    // Crashed on this file, or its idle exit raced the request: once more.
                    log::info!(
                        "thumbnail host exited reading {} (try {})",
                        path.display(),
                        attempt + 1
                    );
                    state.host = None;
                    break;
                }
            }
        }
    }
    Ok(None)
}

/// The command that starts a host: this exe with [`HOST_ARG`]; in tests the test binary running
/// only `tests::host_child`.
fn host_command() -> std::io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    if cfg!(test) {
        command
            .args([
                "thumb_host::tests::host_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("ARKE_THUMB_HOST_CHILD", "1");
    } else {
        command.arg(HOST_ARG);
    }
    Ok(command)
}

/// Tests: makes starting a host fail, as on a machine that blocks child processes.
#[cfg(test)]
static BLOCK_SPAWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn spawn() -> std::io::Result<Host> {
    // ARKE_TEST_HOST_BLOCKED simulates a machine that blocks child processes.
    if std::env::var_os("ARKE_TEST_HOST_BLOCKED").is_some() {
        return Err(std::io::Error::other("blocked (test)"));
    }
    #[cfg(test)]
    if BLOCK_SPAWN.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(std::io::Error::other("blocked (test)"));
    }
    let mut child = host_command()?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .spawn()?;
    end_with_arke(&child);
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err(std::io::Error::other("no pipes to the thumbnail host"));
    };
    let (tx, replies) = flume::unbounded::<Reply>();
    std::thread::Builder::new()
        .name("thumb-host-read".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Anything else on the line (the test harness's own output) is skipped.
                if let Ok(reply) = serde_json::from_str::<Reply>(&line)
                    && tx.send(reply).is_err()
                {
                    break;
                }
            }
        })?;
    Ok(Host {
        child,
        stdin,
        replies,
    })
}

/// The job hosts run in, created once and kept open until Arke exits (as a raw value: handles
/// aren't Send).
static JOB: OnceLock<Option<isize>> = OnceLock::new();

/// Puts the host in a job object that ends its processes when Arke's last handle to it closes,
/// which happens when Arke exits however it exits. Without it a host stuck in a decoder would
/// outlive Arke.
fn end_with_arke(child: &Child) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    let job = JOB.get_or_init(|| {
        // SAFETY: plain Win32 calls; the job handle is deliberately kept open for the process's
        // lifetime, and the limit struct is fully initialized.
        unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(job.0 as isize)
        }
    });
    if let Some(job) = *job {
        // SAFETY: both handles are valid: the job is kept open, the child is alive here.
        let assigned = unsafe {
            AssignProcessToJobObject(HANDLE(job as *mut _), HANDLE(child.as_raw_handle().cast()))
        };
        if let Err(err) = assigned {
            log::info!("thumbnail host: not in a job ({err}); it may outlive Arke if stuck");
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The host process in tests: the test binary started by [`host_command`] runs only this.
    #[test]
    #[ignore = "started by the thumbnail host tests as their host process"]
    #[allow(
        clippy::print_stdout,
        reason = "ends the harness's line so replies start on their own"
    )]
    fn host_child() {
        if std::env::var_os("ARKE_THUMB_HOST_CHILD").is_some() {
            // The harness has printed "test ... " without a line break: replies start on a line of
            // their own.
            println!();
            run();
            std::process::exit(0);
        }
    }

    /// The tests share one host (as Arke does); they run one after another.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn ask_for(name: &str) -> Result<Option<Thumbnail>, HostError> {
        video_thumbnail(&PathBuf::from(format!(r"C:\clips\{name}")), 16)
    }

    fn host_pid() -> Option<u32> {
        STATE
            .get()
            .and_then(|s| s.lock().host.as_ref().map(|h| h.child.id()))
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 15, 16, 127, 128, 254, 255];
        assert_eq!(to_hex(&bytes), "00010f107f80feff");
        assert_eq!(from_hex(&to_hex(&bytes)).as_deref(), Some(&bytes[..]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }

    /// A real host process answers; a file without a thumbnail is `Ok(None)`.
    #[test]
    fn a_host_process_answers() {
        let _serial = SERIAL.lock();
        let thumb = ask_for("trip.mp4").unwrap().expect("a thumbnail");
        assert_eq!((thumb.width, thumb.height), (16, 16));
        assert!(ask_for("none.mp4").unwrap().is_none());
        assert!(ask_for("slow.mp4").unwrap().is_some());
    }

    /// A file the decoder hangs on: given up after the timeout, its host ended, and the next file
    /// gets a new host.
    #[test]
    fn a_hung_decoder_is_ended_and_the_next_file_still_works() {
        let _serial = SERIAL.lock();
        assert!(ask_for("trip.mp4").unwrap().is_some());
        let first = host_pid().expect("a host");
        let started = Instant::now();
        assert!(ask_for("hang.mp4").unwrap().is_none());
        assert!(started.elapsed() >= TIMEOUT, "waited for the timeout");
        assert!(started.elapsed() < TIMEOUT * 3, "no second try of a hang");
        assert!(host_pid().is_none(), "the stuck host was ended");
        assert!(!process_alive(first), "and is gone");
        assert!(ask_for("holiday.mkv").unwrap().is_some());
        assert_ne!(host_pid(), Some(first));
    }

    /// PDFs have their own host: a hung PDF is ended after the timeout and videos are unaffected.
    #[test]
    fn a_hung_pdf_does_not_block_videos() {
        let _serial = SERIAL.lock();
        let pdf = |name: &str| pdf_thumbnail(&PathBuf::from(format!(r"C:\docs\{name}")), 16);
        assert!(pdf("a.pdf").unwrap().is_some());
        assert!(ask_for("trip.mp4").unwrap().is_some());
        assert!(pdf("hang.pdf").unwrap().is_none());
        assert!(ask_for("holiday.mkv").unwrap().is_some());
        assert!(pdf("b.pdf").unwrap().is_some(), "a new PDF host starts");
    }

    /// A file the decoder crashes on: tried once more in a new host, then it has none; other files
    /// still work.
    #[test]
    fn a_crashing_decoder_costs_only_that_file() {
        let _serial = SERIAL.lock();
        assert!(ask_for("crash.mp4").unwrap().is_none());
        assert!(ask_for("trip.mp4").unwrap().is_some());
    }

    /// The host exits when idle; the next request starts another without losing the answer.
    #[test]
    fn an_idle_host_exits_and_comes_back() {
        let _serial = SERIAL.lock();
        assert!(ask_for("trip.mp4").unwrap().is_some());
        let first = host_pid().expect("a host");
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_alive(first) {
            assert!(Instant::now() < deadline, "the idle host didn't exit");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ask_for("holiday.mkv").unwrap().is_some());
    }

    /// A host that can't start: the files asked for aren't counted as having no thumbnail (it's
    /// not their fault), starting isn't retried for a while, and works again after that.
    #[test]
    fn a_host_that_cant_start_is_not_retried_at_once() {
        use std::sync::atomic::Ordering;
        let _serial = SERIAL.lock();
        if let Some(state) = STATE.get() {
            state.lock().host = None;
        }
        BLOCK_SPAWN.store(true, Ordering::Relaxed);
        assert!(matches!(
            ask_for("trip.mp4"),
            Err(HostError::Unavailable(_))
        ));
        BLOCK_SPAWN.store(false, Ordering::Relaxed);
        assert!(
            matches!(ask_for("trip.mp4"), Err(HostError::Unavailable(_))),
            "not retried right away"
        );
        std::thread::sleep(SPAWN_COOLDOWN + Duration::from_millis(50));
        assert!(
            ask_for("trip.mp4").unwrap().is_some(),
            "retried after the cooldown"
        );
    }

    /// The host is in Arke's job, so it ends with Arke even when stuck in a decoder.
    #[test]
    fn the_host_ends_with_arke() {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::IsProcessInJob;
        let _serial = SERIAL.lock();
        assert!(ask_for("trip.mp4").unwrap().is_some());
        // The host stays alive (these tests run one at a time), so its handle stays valid.
        let process = STATE
            .get()
            .expect("a host was started")
            .lock()
            .host
            .as_ref()
            .map(|h| h.child.as_raw_handle() as isize)
            .expect("a host");
        let job = JOB.get().copied().flatten().expect("a job");
        let mut inside = windows::core::BOOL::default();
        // SAFETY: both handles are valid while the host is alive and the job is kept open.
        unsafe {
            IsProcessInJob(
                HANDLE(process as *mut _),
                Some(HANDLE(job as *mut _)),
                &raw mut inside,
            )
        }
        .expect("IsProcessInJob");
        assert!(inside.as_bool());
    }

    fn process_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: the handle is closed below; the exit code is a valid out pointer.
        unsafe {
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut code = 0u32;
            let alive =
                GetExitCodeProcess(process, &raw mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
            let _ = CloseHandle(process);
            alive
        }
    }
}
