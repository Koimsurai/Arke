//! GPUI tests for quiet updates (`update_quiet.rs`): a found update downloads in the background
//! with nothing popping up, then goes in when Arke quits, or after a while with no window open;
//! never while file operations run. The network, files and processes are a fake [`Host`].

use std::sync::atomic::AtomicBool;

use super::*;
use crate::build_info::Channel;
use crate::op_center::OpCenter;
use crate::settings::SettingsStore;
use crate::shell::OpOutcome;
use crate::update::{Release, UpdateError, UpdateState};
use crate::update_quiet::{self, Applied, Host, Launch, QuitReason, Report};

/// A download the app asked for, which the test finishes.
struct Asked {
    release: Release,
    stop: Arc<AtomicBool>,
    tx: flume::Sender<Report>,
}

#[derive(Default)]
struct FakeHost {
    metered: AtomicBool,
    /// An all-users install: the installer would ask for approval.
    needs_approval: AtomicBool,
    shutting_down: AtomicBool,
    downloads: Mutex<Vec<Asked>>,
    applied: Mutex<Vec<(PathBuf, Launch)>>,
    discarded: Mutex<Vec<PathBuf>>,
}

impl Host for FakeHost {
    fn can_update(&self) -> bool {
        true
    }

    fn download(&self, release: Release, stop: Arc<AtomicBool>, tx: flume::Sender<Report>) {
        if self.metered.load(Ordering::Relaxed) {
            let _ = tx.send(Report::Metered);
            return;
        }
        self.downloads.lock().push(Asked { release, stop, tx });
    }

    fn installs_silently(&self) -> bool {
        !self.needs_approval.load(Ordering::Relaxed)
    }

    fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    fn apply(&self, file: &Path, launch: Launch) -> Result<Applied, UpdateError> {
        self.applied.lock().push((file.to_path_buf(), launch));
        Ok(Applied::Quit)
    }

    fn discard(&self, file: &Path) {
        self.discarded.lock().push(file.to_path_buf());
    }
}

impl FakeHost {
    fn asked(&self) -> Vec<String> {
        self.downloads
            .lock()
            .iter()
            .map(|a| a.release.version.clone())
            .collect()
    }

    /// Reports on the latest download.
    fn report(&self, report: Report) {
        let tx = self
            .downloads
            .lock()
            .last()
            .expect("a download was asked for")
            .tx
            .clone();
        tx.send(report).unwrap();
    }

    fn applied(&self) -> Vec<(PathBuf, Launch)> {
        self.applied.lock().clone()
    }
}

fn release(version: &str) -> Release {
    Release {
        version: version.into(),
        exe_url: format!("https://example.test/arke-{version}.exe"),
        exe_name: format!("arke-{version}.exe"),
        size: 100,
        sha_url: None,
        notes: "### Features\n\n- Something new\n".into(),
        page: String::new(),
    }
}

fn downloaded(version: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\fake\updates\arke-{version}.exe"))
}

fn fake_host(cx: &mut VisualTestContext) -> Arc<FakeHost> {
    let host = Arc::new(FakeHost::default());
    let dyn_host: Arc<dyn Host> = Arc::clone(&host) as Arc<dyn Host>;
    cx.update(|_, cx| update_quiet::set_host_for_test(dyn_host, cx));
    host
}

fn state(cx: &mut VisualTestContext) -> UpdateState {
    cx.update(|_, cx| UpdateState::get(cx))
}

fn channel(cx: &mut VisualTestContext) -> Channel {
    cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.update_channel())
}

/// What the check reports when it finds `version`.
fn check_finds(cx: &mut VisualTestContext, version: &str) {
    let channel = channel(cx);
    let found = release(version);
    cx.update(|_, cx| crate::update::checked(channel, Ok(Some(found)), cx));
    cx.run_until_parked();
}

/// Down to Ready through the fake: found, then downloaded.
fn download_ready(host: &FakeHost, cx: &mut VisualTestContext, version: &str) -> PathBuf {
    check_finds(cx, version);
    let file = downloaded(version);
    host.report(Report::Done(Ok(file.clone())));
    cx.run_until_parked();
    file
}

fn drawn(cx: &mut VisualTestContext, id: &'static str) -> bool {
    cx.debug_bounds(id).is_some()
}

fn quit_as(cx: &mut VisualTestContext, reason: Option<QuitReason>) {
    if let Some(reason) = reason {
        update_quiet::note_quit(reason);
    }
    cx.update(|_, cx| update_quiet::on_quit(cx));
}

/// Found by the check, the update downloads right away with no notice and no badge; once it is
/// ready only the sidebar badge says so, and clicking it restarts into it.
#[gpui::test]
fn a_found_update_downloads_quietly_and_the_badge_restarts(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);

    check_finds(cx, "99.0.0");
    assert_eq!(host.asked(), ["99.0.0"], "the download starts when found");
    assert!(!drawn(cx, "update-notice"), "no notice while it downloads");
    assert!(!cx.update(|_, cx| UpdateState::has_news(cx)), "nor a badge");

    host.report(Report::Progress(40, 100));
    cx.run_until_parked();
    assert!(matches!(state(cx), UpdateState::Downloading(_, 40, 100)));
    assert!(!drawn(cx, "update-notice"));
    assert!(!drawn(cx, "update-badge-restart"));

    let file = downloaded("99.0.0");
    host.report(Report::Done(Ok(file.clone())));
    cx.run_until_parked();
    assert!(matches!(state(cx), UpdateState::Ready(_, ref f) if *f == file));
    assert!(!drawn(cx, "update-notice"), "ready: still no notice");
    assert!(
        drawn(cx, "update-badge-restart"),
        "the badge offers the restart"
    );

    let settings_open = |cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.shown_page_for_test())
            .is_some_and(|p| p == t!("settings.title"))
    };
    let bounds = cx.debug_bounds("update-badge-restart").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(host.applied(), [(file, Launch::Window)]);
    assert!(
        !settings_open(cx),
        "the click restarts; it doesn't open Settings"
    );
}

/// Quitting (tray Exit, last window closed) puts the downloaded update in place; quitting for
/// another program (the installer's `--quit`) or for an update doesn't.
#[gpui::test]
fn quitting_puts_the_downloaded_update_in_place(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    let file = download_ready(&host, cx, "99.0.0");

    quit_as(cx, Some(QuitReason::Other));
    quit_as(cx, Some(QuitReason::Update));
    quit_as(cx, None);
    assert!(
        host.applied().is_empty(),
        "not when asked to by another program"
    );

    quit_as(cx, Some(QuitReason::User));
    assert_eq!(host.applied(), [(file, Launch::Nothing)]);
}

/// When Windows shuts down the single exe is swapped in (a moment's work); an installer could be
/// cut off halfway, so it waits for the next start. One that would ask for approval (installed
/// for all users) doesn't run when quitting either.
#[gpui::test]
fn an_installer_waits_through_a_shutdown(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    host.shutting_down.store(true, Ordering::Relaxed);

    let exe = download_ready(&host, cx, "99.0.0");
    quit_as(cx, None);
    assert_eq!(host.applied(), [(exe, Launch::Nothing)]);
    host.applied.lock().clear();

    check_finds(cx, "99.1.0");
    let setup = PathBuf::from(r"C:\fake\updates\arke-99.1.0-setup.exe");
    host.report(Report::Done(Ok(setup.clone())));
    cx.run_until_parked();
    quit_as(cx, None);
    assert!(host.applied().is_empty(), "the installer waits");

    host.shutting_down.store(false, Ordering::Relaxed);
    host.needs_approval.store(true, Ordering::Relaxed);
    quit_as(cx, Some(QuitReason::User));
    assert!(
        host.applied().is_empty(),
        "no approval prompt out of nowhere"
    );

    host.needs_approval.store(false, Ordering::Relaxed);
    quit_as(cx, Some(QuitReason::User));
    assert_eq!(host.applied(), [(setup, Launch::Nothing)]);
}

/// While a copy runs nothing goes in: not on quit, not by itself; "Restart now" waits for it to
/// finish, and says so on the badge.
#[gpui::test]
fn never_during_file_operations(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    let file = download_ready(&host, cx, "99.0.0");
    let (id, _progress) = cx.update(|_, cx| OpCenter::begin(cx, "copy", None));

    quit_as(cx, Some(QuitReason::User));
    assert!(host.applied().is_empty(), "not on quit");

    cx.update(|_, cx| update_quiet::restart_now(cx));
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert!(host.applied().is_empty(), "the restart waits");
    let label = cx.debug_bounds("update-badge-restart");
    assert!(label.is_some());
    assert!(cx.update(|_, cx| update_quiet::restart_waiting(cx)));

    cx.update(|_, cx| OpCenter::finish::<String>(cx, id, "copy", &Ok(OpOutcome::Done)));
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();
    assert_eq!(host.applied(), [(file, Launch::Window)]);
}

/// On a metered connection nothing downloads: the update is offered by hand as before, and
/// downloads by itself once the connection isn't metered.
#[gpui::test]
fn a_metered_connection_skips_the_download(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    host.metered.store(true, Ordering::Relaxed);

    check_finds(cx, "99.0.0");
    assert!(host.asked().is_empty(), "nothing downloaded");
    assert!(matches!(state(cx), UpdateState::Available(_)));
    assert!(drawn(cx, "update-notice-install"), "offered by hand");

    cx.update(|_, cx| update_quiet::resume(cx));
    cx.run_until_parked();
    assert!(host.asked().is_empty(), "still metered");
    assert!(drawn(cx, "update-notice"), "the offer stays up meanwhile");

    host.metered.store(false, Ordering::Relaxed);
    cx.update(|_, cx| update_quiet::resume(cx));
    cx.run_until_parked();
    assert_eq!(host.asked(), ["99.0.0"]);
}

/// A newer version found while one waits for a restart downloads, then replaces it: the older
/// download is deleted. Until then, quitting puts the older one in.
#[gpui::test]
fn a_newer_version_replaces_the_waiting_download(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    let old = download_ready(&host, cx, "99.0.0");

    check_finds(cx, "99.0.0");
    assert_eq!(
        host.asked(),
        ["99.0.0"],
        "the same one isn't downloaded again"
    );
    assert!(matches!(state(cx), UpdateState::Ready(..)));

    check_finds(cx, "99.1.0");
    assert_eq!(host.asked(), ["99.0.0", "99.1.0"]);
    quit_as(cx, Some(QuitReason::User));
    assert_eq!(host.applied(), [(old.clone(), Launch::Nothing)]);

    let new = downloaded("99.1.0");
    host.report(Report::Done(Ok(new.clone())));
    cx.run_until_parked();
    assert_eq!(*host.discarded.lock(), [old]);
    assert!(matches!(state(cx), UpdateState::Ready(r, f) if r.version == "99.1.0" && f == new));
}

/// A failed download tries again later, continuing what it got (the fake host continues; the
/// real one asks for the rest), and says nothing; after the last try it falls back to the manual
/// flow, the notice with "Download and update".
#[gpui::test]
fn failed_downloads_try_again_then_fall_back(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    check_finds(cx, "99.0.0");
    for try_number in 1..update_quiet::TRIES {
        let err = std::io::Error::other("network");
        host.report(Report::Done(Err(UpdateError::Io(err))));
        cx.run_until_parked();
        assert!(!drawn(cx, "update-notice"), "quiet after try {try_number}");
        assert_eq!(host.asked().len(), try_number as usize);
        cx.executor().advance_clock(Duration::from_mins(61));
        cx.run_until_parked();
        assert_eq!(
            host.asked().len(),
            try_number as usize + 1,
            "tried again after try {try_number}"
        );
    }
    host.report(Report::Done(Err(UpdateError::BadHash)));
    cx.run_until_parked();
    assert!(matches!(state(cx), UpdateState::Available(_)));
    assert!(drawn(cx, "update-notice-install"), "the manual flow");
    cx.executor().advance_clock(Duration::from_hours(2));
    cx.run_until_parked();
    assert_eq!(
        host.asked().len(),
        update_quiet::TRIES as usize,
        "no more tries"
    );
}

/// Switching to a channel that doesn't offer the downloaded version (an RC, then Stable) drops
/// it; one the new channel offers stays.
#[gpui::test]
fn switching_the_channel_drops_a_download_it_doesnt_offer(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.update_channel_choice = Some(Channel::Rc), cx);
        });
    });
    let rc = download_ready(&host, cx, "99.0.0-rc.1");

    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.update_channel_choice = Some(Channel::Nightly), cx);
        });
        crate::update::channel_changed(Channel::Nightly, cx);
    });
    cx.run_until_parked();
    assert!(host.discarded.lock().is_empty(), "nightly offers RCs too");
    assert!(matches!(state(cx), UpdateState::Ready(..)));

    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.update_channel_choice = Some(Channel::Stable), cx);
        });
        crate::update::channel_changed(Channel::Stable, cx);
    });
    cx.run_until_parked();
    assert_eq!(*host.discarded.lock(), [rc]);
    assert!(!matches!(state(cx), UpdateState::Ready(..)));
    quit_as(cx, Some(QuitReason::User));
    assert!(host.applied().is_empty());
}

/// A download left from before a restart waits again, unless the channel no longer offers it or
/// automatic updates were turned off.
#[gpui::test]
fn a_download_from_before_a_restart_waits_again(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.update_channel_choice = Some(Channel::Stable), cx);
        });
    });
    cx.update(|_, cx| update_quiet::restore(downloaded("99.0.0-rc.1"), cx));
    assert_eq!(
        *host.discarded.lock(),
        [downloaded("99.0.0-rc.1")],
        "not on stable"
    );

    let file = downloaded("99.0.0");
    cx.update(|_, cx| update_quiet::restore(file.clone(), cx));
    cx.run_until_parked();
    assert!(
        matches!(state(cx), UpdateState::Ready(ref r, ref f) if r.version == "99.0.0" && *f == file)
    );
    assert!(drawn(cx, "update-badge-restart"));
    quit_as(cx, Some(QuitReason::User));
    assert_eq!(host.applied(), [(file, Launch::Nothing)]);
}

/// Switching to a channel that doesn't offer the version being downloaded stops the download.
#[gpui::test]
fn switching_the_channel_stops_a_download_it_doesnt_offer(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.update_channel_choice = Some(Channel::Rc), cx);
        });
    });
    check_finds(cx, "99.0.0-rc.1");
    host.report(Report::Progress(10, 100));
    cx.run_until_parked();
    cx.update(|_, cx| crate::update::channel_changed(Channel::Stable, cx));
    cx.run_until_parked();
    assert!(host.downloads.lock()[0].stop.load(Ordering::Relaxed));
    assert!(matches!(state(cx), UpdateState::Idle));
    // What the stopped download still reports doesn't count.
    host.report(Report::Done(Ok(downloaded("99.0.0-rc.1"))));
    cx.run_until_parked();
    assert!(matches!(state(cx), UpdateState::Idle));
}

/// With automatic updates off nothing downloads by itself: the notice offers it, as before.
#[gpui::test]
fn with_automatic_updates_off_it_is_offered_by_hand(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let host = fake_host(cx);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.update_auto = false, cx));
    });
    check_finds(cx, "99.0.0");
    assert!(host.asked().is_empty());
    assert!(drawn(cx, "update-notice-install"));
}

fn fake_host_app(cx: &mut TestAppContext) -> Arc<FakeHost> {
    let host = Arc::new(FakeHost::default());
    let dyn_host: Arc<dyn Host> = Arc::clone(&host) as Arc<dyn Host>;
    cx.update(|cx| update_quiet::set_host_for_test(dyn_host, cx));
    host
}

fn ready_in_background(cx: &mut TestAppContext, host: &FakeHost) -> PathBuf {
    let channel = cx.update(|cx| SettingsStore::global(cx).read(cx).settings.update_channel());
    cx.update(|cx| crate::update::checked(channel, Ok(Some(release("99.0.0"))), cx));
    cx.run_until_parked();
    let file = downloaded("99.0.0");
    host.report(Report::Done(Ok(file.clone())));
    cx.run_until_parked();
    file
}

/// Running in the background with no window open, the update goes in by itself after
/// [`update_quiet::IDLE_MINUTES`] and Arke starts again in the background.
#[gpui::test]
fn in_the_background_it_goes_in_after_a_while_with_no_window(cx: &mut TestAppContext) {
    update_quiet::mark_resident(true, false);
    let host = fake_host_app(cx);
    let file = ready_in_background(cx, &host);
    cx.executor().advance_clock(Duration::from_secs(
        u64::from(update_quiet::IDLE_MINUTES - 1) * 60,
    ));
    cx.run_until_parked();
    assert!(host.applied().is_empty(), "not yet");
    cx.executor().advance_clock(Duration::from_secs(61));
    cx.run_until_parked();
    assert_eq!(host.applied(), [(file, Launch::Background)]);
}

/// An open window, or a file operation, restarts the clock.
#[gpui::test]
fn a_window_or_an_operation_keeps_it_waiting(cx: &mut TestAppContext) {
    update_quiet::mark_resident(true, false);
    let host = fake_host_app(cx);
    let file = ready_in_background(cx, &host);
    let (id, _progress) = cx.update(|cx| OpCenter::begin(cx, "copy", None));
    cx.executor().advance_clock(Duration::from_secs(
        u64::from(update_quiet::IDLE_MINUTES + 2) * 60,
    ));
    cx.run_until_parked();
    assert!(host.applied().is_empty(), "a copy is running");
    cx.update(|cx| OpCenter::finish::<String>(cx, id, "copy", &Ok(OpOutcome::Done)));

    let window = cx.add_empty_window();
    window.executor().advance_clock(Duration::from_secs(
        u64::from(update_quiet::IDLE_MINUTES + 2) * 60,
    ));
    window.run_until_parked();
    assert!(host.applied().is_empty(), "a window is open");
    window.update(|window, _| window.remove_window());
    cx.run_until_parked();

    cx.executor().advance_clock(Duration::from_secs(
        u64::from(update_quiet::IDLE_MINUTES) * 60 + 61,
    ));
    cx.run_until_parked();
    assert_eq!(host.applied(), [(file, Launch::Background)]);
}

/// Started in the background at sign-in (after Windows restarted), an update waiting from before
/// goes in about a minute later instead of half an hour.
#[gpui::test]
fn after_a_background_start_it_goes_in_soon(cx: &mut TestAppContext) {
    update_quiet::mark_resident(true, true);
    let host = fake_host_app(cx);
    let file = downloaded("99.0.0");
    cx.update(|cx| update_quiet::restore(file.clone(), cx));
    cx.executor().advance_clock(Duration::from_secs(61));
    cx.run_until_parked();
    assert_eq!(host.applied(), [(file, Launch::Background)]);
}

/// Not resident (closing the last window quits) or needing approval: it waits for a restart.
#[gpui::test]
fn no_background_update_without_residency_or_with_an_approval_prompt(cx: &mut TestAppContext) {
    update_quiet::mark_resident(false, false);
    let host = fake_host_app(cx);
    ready_in_background(cx, &host);
    cx.executor().advance_clock(Duration::from_secs(
        u64::from(update_quiet::IDLE_MINUTES + 2) * 60,
    ));
    cx.run_until_parked();
    assert!(host.applied().is_empty(), "not resident");

    update_quiet::mark_resident(true, false);
    host.needs_approval.store(true, Ordering::Relaxed);
    let channel = cx.update(|cx| SettingsStore::global(cx).read(cx).settings.update_channel());
    cx.update(|cx| crate::update::checked(channel, Ok(Some(release("99.1.0"))), cx));
    cx.run_until_parked();
    let setup = PathBuf::from(r"C:\fake\updates\arke-99.1.0-setup.exe");
    host.report(Report::Done(Ok(setup)));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(
        u64::from(update_quiet::IDLE_MINUTES + 2) * 60,
    ));
    cx.run_until_parked();
    assert!(
        host.applied().is_empty(),
        "the installer would ask for approval"
    );
}
