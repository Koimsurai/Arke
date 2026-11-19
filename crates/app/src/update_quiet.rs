//! Quiet updates: with "Update automatically" on, a new version found by the check downloads in
//! the background and goes in at the next natural restart, the way browsers do it.
//!
//! - The download runs at background priority and continues where a failed try stopped (`.part`
//!   and an HTTP range request); it is verified against `.sha256` like a manual one. A failed try
//!   waits and tries again; after [`TRIES`] it falls back to the manual flow ("Download and
//!   update"). On a metered connection (Windows' connection cost) nothing downloads: the update
//!   is offered by hand as before, and downloads by itself once the connection isn't metered.
//! - Nothing pops up while it downloads. Once ready, the sidebar badge says "Restart to update".
//! - It goes in when Arke quits (tray Exit, or the last window closed without background mode),
//!   when Windows shuts down (the single exe only: an installer could be cut off halfway, so it
//!   waits for the next start), and when Arke runs in the background with no window open for
//!   [`IDLE_MINUTES`] (a minute after a background start at sign-in, which is how an update
//!   waiting from before a restart goes in). Never while file operations run, and never where
//!   the installer would ask for administrator approval (an all-users install): those wait for
//!   "Restart now".
//! - "Restart now" and the after-update start work as before: the tabs that were open come back
//!   and What's new opens once.
//!
//! Network, files and processes go through [`Host`], so tests drive all of this with a fake.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use gpui::{App, Global};

use crate::build_info::{self, Channel};
use crate::update::{self, Release, UpdateError, UpdateState, Version};

/// Tries at downloading one release before falling back to the manual flow.
pub(crate) const TRIES: u32 = 4;
/// Waits before the second, third and fourth try.
const RETRY_WAITS: [Duration; 3] = [
    Duration::from_mins(1),
    Duration::from_mins(10),
    Duration::from_hours(1),
];
/// Minutes with no window open (in the background) before the update goes in by itself.
pub(crate) const IDLE_MINUTES: u32 = 30;
const MINUTE: Duration = Duration::from_mins(1);

// ───────────────────────── The outside world ─────────────────────────

/// What a background download reports.
pub(crate) enum Report {
    /// Bytes downloaded and in all (0 if unknown).
    Progress(u64, u64),
    /// The connection is metered: nothing was downloaded.
    Metered,
    /// Downloaded and verified, or why not.
    Done(Result<PathBuf, UpdateError>),
}

/// What starts once the update is in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Launch {
    /// Nothing: Arke is quitting, and its next start is the new version.
    Nothing,
    /// The new version in the background, no window.
    Background,
    /// The new version with the tabs that were open ("Restart now").
    Window,
}

/// What [`Host::apply`] did.
pub(crate) enum Applied {
    /// The update is in place, or the installer runs on its own: quit now.
    Quit,
    /// The installer runs and closes Arke itself; its exit code arrives here.
    Installer(update::SetupExit),
}

/// Network, files and processes. [`RealHost`] in the app, a fake in tests.
pub(crate) trait Host: Send + Sync {
    /// Whether this copy updates itself (it has a releases repo and no package manager owns it).
    fn can_update(&self) -> bool;
    /// Downloads `release` off the UI thread at low priority, continuing a partial download, and
    /// reports on `tx`; stops once `stop` is set.
    fn download(&self, release: Release, stop: Arc<AtomicBool>, tx: flume::Sender<Report>);
    /// Whether the installer can run without Windows asking for approval (Arke is installed for
    /// this user, in a folder it can write to).
    fn installs_silently(&self) -> bool;
    /// Whether Windows is shutting down or signing out.
    fn shutting_down(&self) -> bool;
    /// Puts the downloaded `file` in place and starts what `launch` says.
    fn apply(&self, file: &Path, launch: Launch) -> Result<Applied, UpdateError>;
    /// Deletes a download that is no longer wanted.
    fn discard(&self, file: &Path);
}

struct HostGlobal(Arc<dyn Host>);

impl Global for HostGlobal {}

fn host(cx: &App) -> Arc<dyn Host> {
    cx.try_global::<HostGlobal>()
        .map_or_else(|| Arc::new(RealHost) as Arc<dyn Host>, |h| Arc::clone(&h.0))
}

#[cfg(test)]
pub(crate) fn set_host_for_test(host: Arc<dyn Host>, cx: &mut App) {
    cx.set_global(HostGlobal(host));
}

// ───────────────────────── State ─────────────────────────

#[derive(Default)]
struct Quiet {
    /// Downloaded and verified, waiting for a restart.
    ready: Option<(Release, PathBuf)>,
    /// The release the background download is for, while it runs or waits to try again.
    wanted: Option<Release>,
    /// Stops the running download (none while it waits to try again).
    stop: Option<Arc<AtomicBool>>,
    /// Failed tries at `wanted`.
    tries: u32,
    /// The last try found the connection metered.
    metered: bool,
    /// Bumped when a scheduled retry should no longer happen.
    generation: u64,
    /// Whether the no-window clock is running.
    idle_watch: bool,
    idle_minutes: u32,
    /// "Restart now" was chosen while file operations ran: it waits for them.
    restart_waiting: bool,
}

impl Global for Quiet {}

fn state(cx: &mut App) -> &mut Quiet {
    cx.default_global::<Quiet>()
}

/// This process is the resident one: with no window it keeps running in the background.
static RESIDENT: AtomicBool = AtomicBool::new(false);
/// Started in the background (at sign-in): an update waiting from before goes in soon.
static HEAD_START: AtomicBool = AtomicBool::new(false);

/// Called once at startup: whether this process stays resident, and whether it started with no
/// window (`--background`).
pub(crate) fn mark_resident(resident: bool, background: bool) {
    RESIDENT.store(resident, Ordering::Relaxed);
    HEAD_START.store(resident && background, Ordering::Relaxed);
}

/// Automatic updates are on and this copy can update itself.
fn enabled(cx: &mut App) -> bool {
    crate::settings::SettingsStore::global(cx)
        .read(cx)
        .settings
        .update_auto
        && host(cx).can_update()
}

/// Whether the update in [`UpdateState`] is the background download's while it runs or waits
/// to try again: nothing to show yet.
pub(crate) fn hidden(cx: &App) -> bool {
    let Some(q) = cx.try_global::<Quiet>() else {
        return false;
    };
    matches!(
        UpdateState::get(cx),
        UpdateState::Available(_) | UpdateState::Downloading(..)
    ) && q.wanted.is_some()
        && !q.metered
}

/// Whether the update ready in [`UpdateState`] was downloaded in the background: the sidebar
/// badge offers the restart instead of a notice.
pub(crate) fn ready_quietly(cx: &App) -> bool {
    match UpdateState::get(cx) {
        UpdateState::Ready(_, file) => owns(&file, cx),
        _ => false,
    }
}

/// Whether `file` is the background download waiting for a restart.
pub(crate) fn owns(file: &Path, cx: &App) -> bool {
    cx.try_global::<Quiet>()
        .and_then(|q| q.ready.as_ref())
        .is_some_and(|(_, f)| f == file)
}

/// Whether a background download is running.
pub(crate) fn downloading(cx: &App) -> bool {
    cx.try_global::<Quiet>().is_some_and(|q| q.stop.is_some())
}

/// Whether "Restart now" waits for file operations to finish.
pub(crate) fn restart_waiting(cx: &App) -> bool {
    cx.try_global::<Quiet>().is_some_and(|q| q.restart_waiting)
}

fn ops_running(cx: &App) -> bool {
    crate::op_center::OpCenter::jobs(cx)
        .iter()
        .any(crate::op_center::Job::running)
}

fn visible_on(release: &Release, channel: Channel) -> bool {
    Version::parse(&release.version).is_some_and(|v| v.channel() <= channel)
}

// ───────────────────────── Downloading ─────────────────────────

/// The check found `release`: download it in the background, or offer it by hand as before when
/// automatic updates are off.
pub(crate) fn found(release: Release, cx: &mut App) {
    if !enabled(cx) {
        UpdateState::set(cx, UpdateState::Available(release));
        return;
    }
    let q = state(cx);
    let have = q
        .ready
        .as_ref()
        .filter(|(r, _)| r.version == release.version)
        .map(|(_, file)| file.clone());
    if let Some(file) = have {
        // Already here (restored at startup without its notes): keep it, with the notes now.
        q.ready = Some((release.clone(), file.clone()));
        UpdateState::set(cx, UpdateState::Ready(release, file));
        watch_idle(cx);
        return;
    }
    if q.stop.is_some()
        && q.wanted
            .as_ref()
            .is_some_and(|r| r.version == release.version)
    {
        return;
    }
    start(release, false, cx);
}

fn start(release: Release, retry: bool, cx: &mut App) {
    let host = host(cx);
    let stop = Arc::new(AtomicBool::new(false));
    let q = state(cx);
    if let Some(old) = q.stop.take() {
        old.store(true, Ordering::Relaxed);
    }
    if !retry {
        q.tries = 0;
    }
    q.generation += 1;
    q.wanted = Some(release.clone());
    q.stop = Some(Arc::clone(&stop));
    // Until it reports progress, show what was there: a version already waiting, or this one
    // (hidden, or offered by hand while the connection is metered).
    let shown = match &q.ready {
        Some((r, file)) => UpdateState::Ready(r.clone(), file.clone()),
        None => UpdateState::Available(release.clone()),
    };
    if !matches!(UpdateState::get(cx), UpdateState::Downloading(..)) {
        UpdateState::set(cx, shown);
    }
    let (tx, rx) = flume::unbounded();
    host.download(release.clone(), Arc::clone(&stop), tx);
    cx.spawn(async move |cx| {
        while let Ok(report) = rx.recv_async().await {
            if cx.update(|cx| on_report(&release, &stop, report, cx)) {
                break;
            }
        }
    })
    .detach();
}

/// Handles one report of the download `stop` belongs to; `true` once it is over.
fn on_report(release: &Release, stop: &Arc<AtomicBool>, report: Report, cx: &mut App) -> bool {
    let host = host(cx);
    let q = state(cx);
    // Replaced by a newer one, or stopped: its reports no longer count. A finished file it left
    // goes at the next start.
    if !q.stop.as_ref().is_some_and(|s| Arc::ptr_eq(s, stop)) {
        return true;
    }
    match report {
        Report::Progress(got, total) => {
            q.metered = false;
            UpdateState::set(cx, UpdateState::Downloading(release.clone(), got, total));
            false
        }
        Report::Metered => {
            log::info!(
                "update: metered connection, {} not downloaded",
                release.version
            );
            q.stop = None;
            q.wanted = None;
            q.metered = true;
            show_ready_or(UpdateState::Available(release.clone()), cx);
            true
        }
        Report::Done(Ok(file)) => {
            log::info!(
                "update: {} downloaded, goes in at the next restart",
                release.version
            );
            q.stop = None;
            q.wanted = None;
            q.tries = 0;
            q.metered = false;
            let old = q.ready.replace((release.clone(), file.clone()));
            if let Some((_, old)) = old
                && old != file
            {
                host.discard(&old);
            }
            UpdateState::set(cx, UpdateState::Ready(release.clone(), file));
            watch_idle(cx);
            true
        }
        Report::Done(Err(err)) => {
            q.stop = None;
            q.tries += 1;
            let tries = q.tries;
            let generation = q.generation;
            log::warn!(
                "update: downloading {} failed (try {tries} of {TRIES}): {err}",
                release.version
            );
            if tries >= TRIES {
                // Back to the manual flow: the notice offers "Download and update".
                q.wanted = None;
                q.tries = 0;
                show_ready_or(UpdateState::Available(release.clone()), cx);
                return true;
            }
            show_ready_or(UpdateState::Available(release.clone()), cx);
            let wait = RETRY_WAITS[(tries as usize - 1).min(RETRY_WAITS.len() - 1)];
            let release = release.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(wait).await;
                cx.update(|cx| {
                    let q = state(cx);
                    if q.generation == generation && q.wanted.is_some() && q.stop.is_none() {
                        start(release, true, cx);
                    }
                });
            })
            .detach();
            true
        }
    }
}

/// Shows the version waiting for a restart, if any, else `otherwise`.
fn show_ready_or(otherwise: UpdateState, cx: &mut App) {
    if !show_ready(cx) {
        UpdateState::set(cx, otherwise);
    }
}

/// Shows the version waiting for a restart again (after a check overwrote it); `false` if none.
pub(crate) fn show_ready(cx: &mut App) -> bool {
    let Some((release, file)) = cx.try_global::<Quiet>().and_then(|q| q.ready.clone()) else {
        return false;
    };
    UpdateState::set(cx, UpdateState::Ready(release, file));
    true
}

/// The hourly check: an update found on a metered connection downloads once it isn't.
pub(crate) fn resume(cx: &mut App) {
    if !enabled(cx) {
        return;
    }
    let q = state(cx);
    if q.wanted.is_some() || !q.metered {
        return;
    }
    if let UpdateState::Available(release) = UpdateState::get(cx) {
        start(release, false, cx);
    }
}

/// A download chosen by hand takes over: the background one, or its next try, stops.
pub(crate) fn stop(cx: &mut App) {
    let q = state(cx);
    if let Some(stop) = q.stop.take() {
        stop.store(true, Ordering::Relaxed);
    }
    q.wanted = None;
    q.metered = false;
    q.generation += 1;
}

/// The check says there's nothing newer (the release was taken back): the waiting download goes.
pub(crate) fn forget(cx: &mut App) {
    stop(cx);
    let host = host(cx);
    if let Some((_, file)) = state(cx).ready.take() {
        host.discard(&file);
    }
}

/// The channel changed to `channel`. A background download it doesn't offer (an RC after
/// switching to stable) is dropped; one it does stays. Returns whether there was one.
pub(crate) fn channel_changed(channel: Channel, cx: &mut App) -> bool {
    let Some(q) = cx.try_global::<Quiet>() else {
        return false;
    };
    if q.wanted.is_none() && q.ready.is_none() {
        return false;
    }
    if q.wanted.as_ref().is_some_and(|r| !visible_on(r, channel)) {
        stop(cx);
    }
    let host = host(cx);
    let q = state(cx);
    if q.ready
        .as_ref()
        .is_some_and(|(r, _)| !visible_on(r, channel))
        && let Some((_, file)) = q.ready.take()
    {
        host.discard(&file);
    }
    if !show_ready(cx) && !downloading(cx) {
        UpdateState::set(cx, UpdateState::Idle);
    }
    true
}

/// At startup: `file` was downloaded before Arke last quit and hasn't gone in yet (Windows shut
/// down, or the installer needed a start of its own). It waits for the next restart again, unless
/// automatic updates were turned off or the channel no longer offers it.
pub(crate) fn restore(file: PathBuf, cx: &mut App) {
    let host = host(cx);
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let channel = crate::settings::SettingsStore::global(cx)
        .read(cx)
        .settings
        .update_channel();
    let number = update::downloaded_number(&name, &update::asset_suffix())
        .filter(|n| Version::parse(n).is_some_and(|v| v.channel() <= channel))
        .map(str::to_owned);
    let Some(number) = number.filter(|_| enabled(cx)) else {
        host.discard(&file);
        return;
    };
    let release = Release {
        version: number,
        exe_url: String::new(),
        exe_name: name,
        size: 0,
        sha_url: None,
        notes: String::new(),
        page: String::new(),
    };
    state(cx).ready = Some((release.clone(), file.clone()));
    if matches!(
        UpdateState::get(cx),
        UpdateState::Idle | UpdateState::UpToDate | UpdateState::Failed(_)
    ) {
        UpdateState::set(cx, UpdateState::Ready(release, file));
    }
    watch_idle(cx);
}

// ───────────────────────── Applying ─────────────────────────

/// Why Arke quits, for [`on_quit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuitReason {
    /// Tray Exit, or the last window closed without background mode: the update goes in.
    User = 1,
    /// An update restarts Arke.
    Update = 2,
    /// Asked by another process (the installer's `--quit`, the uninstaller) or by Remove Arke.
    Other = 3,
}

static QUIT_REASON: AtomicU8 = AtomicU8::new(0);

/// Quits, noting why for [`on_quit`].
pub(crate) fn quit(reason: QuitReason, cx: &mut App) {
    note_quit(reason);
    cx.quit();
}

/// Notes why Arke is about to quit (for a quit that happens by itself).
pub(crate) fn note_quit(reason: QuitReason) {
    QUIT_REASON.store(reason as u8, Ordering::Relaxed);
}

/// The download to put in place when Arke quits.
fn to_apply(cx: &App) -> Option<PathBuf> {
    let ready = cx.try_global::<Quiet>().and_then(|q| q.ready.clone());
    match UpdateState::get(cx) {
        UpdateState::Ready(_, file) if owns(&file, cx) => Some(file),
        // An installer downloaded by hand and not run yet.
        UpdateState::Ready(_, file) if update::is_setup(&file) => Some(file),
        // The installer stopped (approval declined) or putting it in place failed: not again.
        UpdateState::Canceled(..) | UpdateState::Failed(_) => None,
        // A newer one is downloading: the one already here still goes in.
        _ => ready.map(|(_, file)| file),
    }
}

/// Arke is quitting (on every quit, also when Windows shuts down): put a downloaded update in
/// place so the next start is the new version.
pub(crate) fn on_quit(cx: &mut App) {
    let host = host(cx);
    let session_end = match QUIT_REASON.swap(0, Ordering::Relaxed) {
        1 => false,
        0 if host.shutting_down() => true,
        _ => return,
    };
    let Some(file) = to_apply(cx) else {
        return;
    };
    if owns(&file, cx) && !enabled(cx) {
        return;
    }
    if ops_running(cx) {
        log::info!("update: file operations are running, the update waits for the next restart");
        return;
    }
    if update::is_setup(&file) && (session_end || !host.installs_silently()) {
        // Windows may end the installer halfway through shutting down, and one that asks for
        // approval would ask out of nowhere: both wait for a start of their own.
        log::info!("update: the installer waits for the next start");
        return;
    }
    match host.apply(&file, Launch::Nothing) {
        Ok(_) => log::info!("update: {} goes in as Arke quits", file.display()),
        Err(err) => log::warn!("update: couldn't put {} in place: {err}", file.display()),
    }
}

/// "Restart now" for a background download: puts it in place and starts it with the tabs that
/// were open. While file operations run it waits for them.
pub(crate) fn restart_now(cx: &mut App) {
    if !ops_running(cx) {
        apply(Launch::Window, cx);
        return;
    }
    let q = state(cx);
    if q.restart_waiting {
        return;
    }
    q.restart_waiting = true;
    cx.refresh_windows();
    cx.spawn(async |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let done = cx.update(|cx| {
                if !state(cx).restart_waiting {
                    return true;
                }
                if ops_running(cx) {
                    return false;
                }
                state(cx).restart_waiting = false;
                apply(Launch::Window, cx);
                true
            });
            if done {
                break;
            }
        }
    })
    .detach();
}

fn apply(launch: Launch, cx: &mut App) {
    let Some((_, file)) = cx.try_global::<Quiet>().and_then(|q| q.ready.clone()) else {
        return;
    };
    let installer = update::is_setup(&file);
    if installer && !update::claim_setup() {
        return;
    }
    let host = host(cx);
    match host.apply(&file, launch) {
        Ok(Applied::Quit) => quit(QuitReason::Update, cx),
        Ok(Applied::Installer(exit)) => update::watch_installer(exit, cx),
        Err(err) => {
            if installer {
                update::release_setup();
            }
            log::warn!("update: couldn't put {} in place: {err}", file.display());
            state(cx).ready = None;
            host.discard(&file);
            UpdateState::set(cx, UpdateState::Failed(err.to_string()));
        }
    }
}

/// Counts minutes with no window open while Arke runs in the background with an update ready;
/// after [`IDLE_MINUTES`] (and no file operations) the update goes in and Arke starts again in
/// the background.
fn watch_idle(cx: &mut App) {
    let q = state(cx);
    if q.idle_watch {
        return;
    }
    q.idle_watch = true;
    q.idle_minutes = if HEAD_START.swap(false, Ordering::Relaxed) {
        IDLE_MINUTES - 1
    } else {
        0
    };
    cx.spawn(async |cx| {
        loop {
            cx.background_executor().timer(MINUTE).await;
            if !cx.update(idle_tick) {
                break;
            }
        }
    })
    .detach();
}

/// One minute of [`watch_idle`]; `false` once it is done.
fn idle_tick(cx: &mut App) -> bool {
    let waiting = ready_quietly(cx) && enabled(cx);
    let busy = !RESIDENT.load(Ordering::Relaxed) || !cx.windows().is_empty() || ops_running(cx);
    let silently = host(cx).installs_silently();
    let installer = cx
        .try_global::<Quiet>()
        .and_then(|q| q.ready.as_ref())
        .is_some_and(|(_, f)| update::is_setup(f));
    let q = state(cx);
    if !waiting {
        q.idle_watch = false;
        return false;
    }
    if busy {
        q.idle_minutes = 0;
        return true;
    }
    q.idle_minutes += 1;
    if q.idle_minutes < IDLE_MINUTES {
        return true;
    }
    q.idle_watch = false;
    if installer && !silently {
        return false;
    }
    log::info!("update: no window open for {IDLE_MINUTES} minutes, updating in the background");
    apply(Launch::Background, cx);
    false
}

// ───────────────────────── The real host ─────────────────────────

pub(crate) struct RealHost;

impl Host for RealHost {
    fn can_update(&self) -> bool {
        !build_info::RELEASES_REPO.is_empty() && update::package_manager().is_none()
    }

    fn download(&self, release: Release, stop: Arc<AtomicBool>, tx: flume::Sender<Report>) {
        crate::explorer::spawn_thread("update-background", move || {
            background_priority();
            if metered() {
                let _ = tx.send(Report::Metered);
                return;
            }
            let result = update::updates_dir()
                .ok_or(UpdateError::NoUpdatesDir)
                .and_then(|dir| {
                    let mut last: Option<std::time::Instant> = None;
                    update::download_to(&dir, &release, &mut |done, total| {
                        if stop.load(Ordering::Relaxed) {
                            return false;
                        }
                        // Once a second: each report redraws the windows.
                        if last.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                            last = Some(std::time::Instant::now());
                            let _ = tx.send(Report::Progress(done, total.unwrap_or(release.size)));
                        }
                        true
                    })
                });
            let _ = tx.send(Report::Done(result));
        });
    }

    fn installs_silently(&self) -> bool {
        static WRITABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *WRITABLE.get_or_init(|| {
            let Some(dir) = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf))
            else {
                return false;
            };
            // Program Files (an all-users install) can't be written without approval.
            let probe = dir.join(format!(".arke-update-{}", std::process::id()));
            let writable = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .is_ok();
            let _ = std::fs::remove_file(&probe);
            writable
        })
    }

    fn shutting_down(&self) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
        // SAFETY: no arguments to get wrong; it only reads a system flag.
        unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
    }

    fn apply(&self, file: &Path, launch: Launch) -> Result<Applied, UpdateError> {
        if update::is_setup(file) {
            let mut setup = std::process::Command::new(file);
            setup.args(update::SETUP_SILENT);
            match launch {
                Launch::Nothing => {}
                Launch::Background => {
                    setup.args([update::SETUP_LAUNCH, update::SETUP_BACKGROUND]);
                }
                Launch::Window => {
                    setup.arg(update::SETUP_LAUNCH);
                }
            }
            let child = setup.spawn()?;
            return Ok(if launch == Launch::Nothing {
                Applied::Quit
            } else {
                Applied::Installer(update::wait_for_setup(child))
            });
        }
        let exe = update::install(file)?;
        if launch == Launch::Nothing {
            return Ok(Applied::Quit);
        }
        let mut new = std::process::Command::new(&exe);
        if launch == Launch::Background {
            new.arg("--background");
        }
        match new
            .arg(update::AFTER_UPDATE_ARG)
            .arg(std::process::id().to_string())
            .spawn()
        {
            Ok(_) => Ok(Applied::Quit),
            Err(err) => {
                let _ = update::roll_back(&exe);
                Err(UpdateError::NotStarted(err))
            }
        }
    }

    fn discard(&self, file: &Path) {
        let file = file.to_path_buf();
        crate::explorer::spawn_thread("update-discard", move || {
            let _ = std::fs::remove_file(file);
        });
    }
}

/// This thread at background priority: CPU, disk and memory, so a download never slows Arke or
/// anything else down.
fn background_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    // SAFETY: the pseudo handle of the current thread needs no closing.
    if let Err(err) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }
    {
        log::debug!("update: background priority not set: {err}");
    }
}

/// Whether Windows says the connection costs by the byte, roams or is over its limit (Settings >
/// Network > "Metered connection").
fn metered() -> bool {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};
    let Ok(cost) = NetworkInformation::GetInternetConnectionProfile()
        .and_then(|profile| profile.GetConnectionCost())
    else {
        return false;
    };
    matches!(
        cost.NetworkCostType(),
        Ok(NetworkCostType::Fixed | NetworkCostType::Variable)
    ) || cost.Roaming().unwrap_or(false)
        || cost.OverDataLimit().unwrap_or(false)
}
