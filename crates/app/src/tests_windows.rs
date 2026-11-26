//! GPUI tests for windows and sessions: single window mode routing what other programs and a
//! second `arke.exe` ask for (`main.rs`), and the last session saving and restoring every window
//! (`session.rs`).

use super::*;
use crate::instance::OpenRequest;
use crate::settings::{
    Session, SessionTab, SessionWindow, SettingsStore, StartLocation, WindowPlace,
};

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn services(fs: &Arc<FakeFs>) -> Services {
    Services {
        fs: Arc::clone(fs) as Arc<dyn arke_core::fs::Fs>,
        shell: Arc::new(FakeShell::default()),
        remotes: crate::remote_shell::test_remotes(),
    }
}

fn fake_fs() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c", r"C:\d"] {
        fs.insert_many(dir, 1);
    }
    fs
}

/// Settings read, with `f` applied.
fn settings(cx: &mut TestAppContext, f: impl FnOnce(&mut crate::settings::Settings)) {
    cx.update(|cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            f(&mut store.settings);
            store.mark_loaded(cx);
        });
    });
}

fn saved_windows(cx: &mut TestAppContext) -> Vec<SessionWindow> {
    cx.update(|cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .session_windows
            .clone()
    })
}

/// The first folder of each saved window.
fn saved_firsts(cx: &mut TestAppContext) -> Vec<PathBuf> {
    saved_windows(cx)
        .iter()
        .filter_map(|w| w.first_folder().cloned())
        .collect()
}

fn workspaces(cx: &TestAppContext) -> Vec<gpui::WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .collect()
}

/// The folder of each tab of `window`.
fn tabs_of(window: gpui::WindowHandle<Workspace>, cx: &mut TestAppContext) -> Vec<Option<PathBuf>> {
    cx.update(|cx| {
        let ws = window.read(cx).unwrap();
        tab_paths(ws, cx)
    })
}

fn request(path: Option<&str>, new_window: bool) -> OpenRequest {
    OpenRequest {
        path: path.map(p),
        bench: None,
        bench_out: None,
        launched_unix_ms: 0,
        page: None,
        new_window,
    }
}

fn tab(path: &str) -> SessionTab {
    SessionTab {
        panes: vec![p(path)],
        active_pane: 0,
        ratio: 0.5,
        pinned: false,
        synced: false,
        home: false,
    }
}

/// The app with no window open yet.
fn no_window_yet(cx: &mut TestAppContext) {
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
    });
}

/// Past the moment the windows opened behind the front one are up and it came forward again.
fn settle(cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.executor()
        .advance_clock(crate::BEHIND_SETTLE + Duration::from_millis(50));
    cx.run_until_parked();
}

/// The window whose first tab shows `first`.
fn window_on(cx: &mut TestAppContext, first: &str) -> gpui::WindowHandle<Workspace> {
    workspaces(cx)
        .into_iter()
        .find(|w| tabs_of(*w, cx).first() == Some(&Some(p(first))))
        .unwrap_or_else(|| panic!("no window on {first}"))
}

fn last_active(cx: &mut TestAppContext) -> Option<gpui::AnyWindowHandle> {
    cx.update(|cx| {
        cx.try_global::<crate::workspace::LastActiveWindow>()
            .and_then(|l| l.0)
    })
}

/// "Last tabs" and a last session of two windows: one on `C:\a` and `C:\b` (used last), one on
/// `C:\c`.
fn two_saved_windows(cx: &mut TestAppContext, single_window: bool) {
    settings(cx, |s| {
        s.start_location = StartLocation::LastSession;
        s.single_window = single_window;
        s.session_windows = vec![
            saved_window(&[r"C:\a", r"C:\b"], None),
            saved_window(&[r"C:\c"], None),
        ];
    });
}

fn active_tab(window: gpui::WindowHandle<Workspace>, cx: &mut TestAppContext) -> Option<PathBuf> {
    cx.update(|cx| {
        let ws = window.read(cx).unwrap();
        ws.active_view()
            .read(cx)
            .display_path()
            .map(Path::to_path_buf)
    })
}

fn saved_window(paths: &[&str], place: Option<WindowPlace>) -> SessionWindow {
    SessionWindow {
        session: Session {
            workspace: None,
            tabs: paths.iter().map(|path| tab(path)).collect(),
            active: 0,
        },
        place,
    }
}

/// A folder from another program, a second `arke.exe <folder>` or Win+E while a window is open:
/// a tab in that window, no second window.
#[gpui::test]
fn single_window_mode_opens_what_comes_from_outside_as_tabs(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (ws, vcx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let first = ws.update(vcx, |ws, _| ws.active_view().clone());
    wait_until(vcx, &first, "a", loaded(r"C:\a"));
    let cx = &mut vcx.cx;
    settings(cx, |s| s.start_location = StartLocation::HomePage);
    let services = services(&fs);
    let window = workspaces(cx)[0];

    let handled = cx.update(|cx| {
        crate::handle_open(cx, &services, request(Some(r"C:\b"), false), Instant::now())
    });
    assert!(handled);
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 1, "no second window");
    assert_eq!(tabs_of(window, cx), [Some(p(r"C:\a")), Some(p(r"C:\b"))]);

    // Win+E or Arke started again: a tab on Home, where new windows open.
    let handled =
        cx.update(|cx| crate::handle_open(cx, &services, request(None, false), Instant::now()));
    assert!(handled);
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 1, "still one window");
    assert_eq!(tabs_of(window, cx).len(), 3);
    assert_eq!(
        cx.update(|cx| window.read(cx).unwrap().shown_page_for_test()),
        Some(t!("home.title").into_owned())
    );

    // "Open in new window" is a window all the same.
    let handled = cx.update(|cx| {
        crate::handle_open(cx, &services, request(Some(r"C:\c"), true), Instant::now())
    });
    assert!(handled);
    cx.run_until_parked();
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 2, "a window of its own");
    assert_eq!(tabs_of(windows[1], cx), [Some(p(r"C:\c"))]);
    assert_eq!(
        tabs_of(window, cx).len(),
        3,
        "the first window keeps its tabs"
    );
}

/// Single window mode off: as before, each folder from outside gets a window of its own.
#[gpui::test]
fn with_single_window_off_each_folder_gets_a_window(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (ws, vcx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let first = ws.update(vcx, |ws, _| ws.active_view().clone());
    wait_until(vcx, &first, "a", loaded(r"C:\a"));
    let cx = &mut vcx.cx;
    settings(cx, |s| s.single_window = false);
    let services = services(&fs);
    let handled = cx.update(|cx| {
        crate::handle_open(cx, &services, request(Some(r"C:\b"), false), Instant::now())
    });
    assert!(handled);
    cx.run_until_parked();
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 2);
    assert_eq!(tabs_of(windows[0], cx), [Some(p(r"C:\a"))]);
    assert_eq!(tabs_of(windows[1], cx), [Some(p(r"C:\b"))]);
}

/// What "Open folders in Arke" asks for: from the early path (File Explorer's launcher ended) or
/// from the watcher (a File Explorer window).
fn taken_over(path: &str, select: &[&str], early: bool) -> crate::win_takeover::OpenRequest {
    let (reply, _) = flume::bounded(1);
    crate::win_takeover::OpenRequest {
        target: arke_core::takeover::Target::Folder(p(path)),
        select: select.iter().map(|name| (*name).to_owned()).collect(),
        explorer_thread: 0,
        early,
        unchecked: false,
        reply,
    }
}

fn until(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The window `open` made, among those not in `before`.
fn opened_window(
    cx: &mut TestAppContext,
    before: &[gpui::WindowId],
    open: impl FnOnce(&mut TestAppContext) -> bool,
) -> gpui::WindowHandle<Workspace> {
    assert!(open(cx));
    cx.run_until_parked();
    workspaces(cx)
        .into_iter()
        .find(|w| !before.contains(&w.window_id()))
        .expect("a window of its own")
}

fn window_ids(cx: &TestAppContext) -> Vec<gpui::WindowId> {
    workspaces(cx).iter().map(|w| w.window_id()).collect()
}

/// The active list of `window`, once it shows `path`.
fn loaded_view(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<Workspace>,
    path: &str,
) -> Entity<Explorer> {
    let view = cx.update(|cx| window.read(cx).unwrap().active_view().clone());
    let done = loaded(path);
    until(cx, path, |cx| cx.update(|cx| done(view.read(cx))));
    view
}

fn selection(cx: &mut TestAppContext, view: &Entity<Explorer>) -> Vec<String> {
    cx.update(|cx| selected_names(view.read(cx)))
}

/// #285, single window mode off: the folder the early path opened gets a window. A File Explorer
/// window on it that comes anyway (its launcher had handed it over already) shows in that tab with
/// its selection instead of in a window of its own, once: the next one opens as any other. A tab
/// that went elsewhere keeps no mark, even once it is back.
#[gpui::test]
fn a_late_explorer_window_uses_the_early_tab_once(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (ws, vcx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let first = ws.update(vcx, |ws, _| ws.active_view().clone());
    wait_until(vcx, &first, "a", loaded(r"C:\a"));
    let cx = &mut vcx.cx;
    settings(cx, |s| s.single_window = false);
    let services = services(&fs);
    let open = |cx: &mut TestAppContext, request: crate::win_takeover::OpenRequest| {
        cx.update(|cx| crate::open_taken_over(cx, &services, &request))
    };

    let before = window_ids(cx);
    let early = opened_window(cx, &before, |cx| open(cx, taken_over(r"C:\b", &[], true)));
    let view = loaded_view(cx, early, r"C:\b");

    // The window comes anyway, with the item selected.
    assert!(open(cx, taken_over(r"C:\b", &["file_000000.txt"], false)));
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 2, "no window of its own");
    assert_eq!(tabs_of(early, cx), [Some(p(r"C:\b"))], "no second tab");
    until(cx, "the window's selection", |cx| {
        cx.update(|cx| selected_names(view.read(cx)) == ["file_000000.txt"])
    });
    // The mark is used up: the next window on it opens as usual.
    assert!(open(cx, taken_over(r"C:\b", &[], false)));
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 3, "a window as usual");

    // A tab that goes elsewhere loses its mark, even once it is back.
    let before = window_ids(cx);
    let early = opened_window(cx, &before, |cx| open(cx, taken_over(r"C:\c", &[], true)));
    let view = loaded_view(cx, early, r"C:\c");
    cx.update(|cx| {
        view.update(cx, |v, cx| {
            v.navigate(p(r"C:\d"), arke_core::NavKind::New, cx);
        });
    });
    let _ = loaded_view(cx, early, r"C:\d");
    cx.update(|cx| {
        view.update(cx, |v, cx| {
            v.navigate(p(r"C:\c"), arke_core::NavKind::New, cx);
        });
    });
    let _ = loaded_view(cx, early, r"C:\c");
    assert!(open(cx, taken_over(r"C:\c", &[], false)));
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 5, "not marked any more");
}

/// #285: two quick `explorer.exe C:\b`, both launchers ended, the second just too late, so its
/// window comes anyway. Each launch has a window, and the late window goes to the second's (the
/// newest mark), not the first's: no duplicate. Each mark is used once.
#[gpui::test]
fn two_quick_launches_each_get_their_own_merge(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (ws, vcx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let first = ws.update(vcx, |ws, _| ws.active_view().clone());
    wait_until(vcx, &first, "a", loaded(r"C:\a"));
    let cx = &mut vcx.cx;
    settings(cx, |s| s.single_window = false);
    let services = services(&fs);
    let open = |cx: &mut TestAppContext, request: crate::win_takeover::OpenRequest| {
        cx.update(|cx| crate::open_taken_over(cx, &services, &request))
    };

    let before = window_ids(cx);
    let one = opened_window(cx, &before, |cx| open(cx, taken_over(r"C:\b", &[], true)));
    let before = window_ids(cx);
    let two = opened_window(cx, &before, |cx| open(cx, taken_over(r"C:\b", &[], true)));
    let one_view = loaded_view(cx, one, r"C:\b");
    let two_view = loaded_view(cx, two, r"C:\b");
    assert_eq!(workspaces(cx).len(), 3);

    // The second launch's window, with its item selected: the newest mark.
    assert!(open(cx, taken_over(r"C:\b", &["file_000000.txt"], false)));
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 3, "no duplicate");
    until(cx, "the second tab's selection", |cx| {
        cx.update(|cx| selected_names(two_view.read(cx)) == ["file_000000.txt"])
    });
    assert!(
        selection(cx, &one_view).is_empty(),
        "the first tab untouched"
    );

    // One more on it goes to the first launch's tab, whose mark is left; then none are.
    assert!(open(cx, taken_over(r"C:\b", &["file_000000.txt"], false)));
    cx.run_until_parked();
    assert_eq!(workspaces(cx).len(), 3);
    until(cx, "the first tab's selection", |cx| {
        cx.update(|cx| selected_names(one_view.read(cx)) == ["file_000000.txt"])
    });
    assert!(open(cx, taken_over(r"C:\b", &[], false)));
    cx.run_until_parked();
    assert_eq!(
        workspaces(cx).len(),
        4,
        "every mark used: a window as usual"
    );
}

/// A network folder the early path took without looking (#285), with the receiver of its answer.
fn unchecked(path: &str) -> (crate::win_takeover::OpenRequest, flume::Receiver<bool>) {
    let (reply, answer) = flume::bounded(1);
    let request = crate::win_takeover::OpenRequest {
        target: arke_core::takeover::Target::Folder(p(path)),
        select: Vec::new(),
        explorer_thread: 0,
        early: true,
        unchecked: true,
        reply,
    };
    (request, answer)
}

/// Arke's answer to a request, once it comes.
fn answer(cx: &mut TestAppContext, reply: &flume::Receiver<bool>) -> bool {
    let mut got = None;
    until(cx, "Arke's answer", |_| {
        got = got.or_else(|| reply.try_recv().ok());
        got.is_some()
    });
    got.unwrap_or_default()
}

/// #285: a network folder the early path took without looking is checked first. A folder opens
/// as usual and is answered `true`; a missing folder or a file opens nothing and is answered
/// `false` (File Explorer gets it back); one nobody waits for any more opens nothing.
#[gpui::test]
fn an_unchecked_folder_opens_only_once_it_is_found(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (ws, vcx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let first = ws.update(vcx, |ws, _| ws.active_view().clone());
    wait_until(vcx, &first, "a", loaded(r"C:\a"));
    let cx = &mut vcx.cx;
    settings(cx, |s| s.single_window = false);
    let services = services(&fs);

    for missing in [r"C:\missing", r"C:\b\file_000000.txt"] {
        let (request, reply) = unchecked(missing);
        cx.update(|cx| crate::answer_taken_over(cx, &services, request));
        assert!(!answer(cx, &reply), "{missing}: given back");
        settle(cx);
        assert_eq!(workspaces(cx).len(), 1, "{missing}: nothing opened");
    }

    // Nobody waits any more: File Explorer has it already.
    let (request, reply) = unchecked(r"C:\c");
    drop(reply);
    cx.update(|cx| crate::answer_taken_over(cx, &services, request));
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(workspaces(cx).len(), 1, "not opened behind File Explorer");

    let before = window_ids(cx);
    let (request, reply) = unchecked(r"C:\b");
    cx.update(|cx| crate::answer_taken_over(cx, &services, request));
    assert!(answer(cx, &reply), "found and opened");
    let window = opened_window(cx, &before, |_| true);
    let _ = loaded_view(cx, window, r"C:\b");
    assert_eq!(workspaces(cx).len(), 2);
}

/// The forum report: with two windows open, the second one's tabs no longer replace the
/// first's. Each window is saved; closing one of several drops it, closing the last keeps it.
#[gpui::test]
fn every_open_window_is_saved_and_closing_one_drops_only_it(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (_, vcx) = setup_workspace(cx, Arc::clone(&fs), r"C:\a");
    let cx = &mut vcx.cx;
    settings(cx, |s| s.single_window = false);
    let services = services(&fs);
    cx.update(|cx| crate::handle_open(cx, &services, request(Some(r"C:\b"), true), Instant::now()));
    cx.run_until_parked();
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 2);
    // A tab more in the first window changes only its part of the session.
    windows[0]
        .update(cx, |ws, window, cx| {
            ws.open_tab(p(r"C:\c"), 1, true, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let saved = saved_windows(cx);
    assert_eq!(saved.len(), 2, "{saved:?}");
    let tabs = |w: &SessionWindow| -> Vec<PathBuf> {
        w.session.tabs.iter().map(|t| t.panes[0].clone()).collect()
    };
    let mut all: Vec<Vec<PathBuf>> = saved.iter().map(tabs).collect();
    all.sort();
    assert_eq!(all, [vec![p(r"C:\a"), p(r"C:\c")], vec![p(r"C:\b")]]);
    assert!(
        saved.iter().all(|w| w.place.is_some()),
        "each window's place"
    );

    // Closing one of the two: the other is the session.
    windows[1]
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_eq!(saved_firsts(cx), [p(r"C:\a")]);
    // Closing the last one keeps it for the next start.
    windows[0]
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert!(workspaces(cx).is_empty());
    assert_eq!(saved_firsts(cx), [p(r"C:\a")]);
    let session = cx.update(|cx| SettingsStore::global(cx).read(cx).settings.session.clone());
    assert_eq!(session.tabs.len(), 2, "what older versions read too");
}

/// "New windows open to: Last tabs" brings back every window of the last session, each with its
/// tabs, the one used last in front.
#[gpui::test]
fn last_tabs_restores_every_window(cx: &mut TestAppContext) {
    let fs = fake_fs();
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
    });
    let place = WindowPlace {
        x: 40.,
        y: 50.,
        width: 900.,
        height: 700.,
        maximized: false,
    };
    settings(cx, |s| {
        s.start_location = StartLocation::LastSession;
        s.session_windows = vec![
            saved_window(&[r"C:\a", r"C:\b"], None),
            saved_window(&[r"C:\c"], Some(place)),
            saved_window(&[r"C:\d"], None),
        ];
    });
    let services = services(&fs);
    let opened = cx.update(|cx| crate::open_explorer(cx, &services, None, None, Instant::now()));
    assert!(opened);
    settle(cx);
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 3, "every window comes back");
    let mut all: Vec<Vec<Option<PathBuf>>> = windows.iter().map(|w| tabs_of(*w, cx)).collect();
    all.sort();
    assert_eq!(
        all,
        [
            vec![Some(p(r"C:\a")), Some(p(r"C:\b"))],
            vec![Some(p(r"C:\c"))],
            vec![Some(p(r"C:\d"))],
        ]
    );
    // Saved again in the same order, the front window first.
    assert_eq!(saved_firsts(cx), [p(r"C:\a"), p(r"C:\c"), p(r"C:\d")]);
    let saved = saved_windows(cx);
    assert_eq!(saved[1].place.map(|p| (p.x, p.width)), Some((40., 900.)));
}

/// Settings read only after the first window opened (a slow disk): that window takes the window
/// used last and the others open beside it then.
#[gpui::test]
fn last_tabs_restores_every_window_when_settings_arrive_late(cx: &mut TestAppContext) {
    let fs = fake_fs();
    let (ws, vcx) = {
        arke_core::fs::mark_ui_thread();
        rust_i18n::set_locale("en");
        cx.executor().allow_parking();
        cx.update(|cx| {
            crate::text_input::bind_keys(cx);
            crate::explorer::bind_keys(cx);
            crate::workspace::bind_keys(cx);
        });
        let services = services(&fs);
        cx.add_window_view(move |window, cx| {
            Workspace::new(
                services,
                p(r"C:\a"),
                true,
                None,
                None,
                Instant::now(),
                window,
                cx,
            )
        })
    };
    let cx = &mut vcx.cx;
    settings(cx, |s| {
        s.start_location = StartLocation::LastSession;
        s.session_windows = vec![
            saved_window(&[r"C:\b", r"C:\c"], None),
            saved_window(&[r"C:\d"], None),
        ];
    });
    settle(cx);
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 2);
    assert_eq!(
        cx.update(|cx| windows[0].read(cx).unwrap().active_view().entity_id()),
        cx.update(|cx| ws.read(cx).active_view().entity_id()),
        "the window that was open took the window used last"
    );
    assert_eq!(
        tabs_of(windows[0], cx),
        [Some(p(r"C:\b")), Some(p(r"C:\c"))]
    );
    assert_eq!(tabs_of(windows[1], cx), [Some(p(r"C:\d"))]);
    assert_eq!(saved_firsts(cx), [p(r"C:\b"), p(r"C:\d")]);
}

/// "Show in folder" or "Open folder with Arke" while Arke isn't running: the first window opens
/// on a folder, and the last session comes back all the same, the folder a tab of the window used
/// last. It used to open alone and save itself over the session.
#[gpui::test]
fn a_folder_from_outside_with_no_window_brings_back_the_last_session(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    two_saved_windows(cx, true);
    let services = services(&fs);
    let handled = cx.update(|cx| {
        crate::handle_open(cx, &services, request(Some(r"C:\d"), false), Instant::now())
    });
    assert!(handled);
    settle(cx);
    assert_eq!(workspaces(cx).len(), 2);
    let front = window_on(cx, r"C:\a");
    assert_eq!(
        tabs_of(front, cx),
        [Some(p(r"C:\a")), Some(p(r"C:\b")), Some(p(r"C:\d"))]
    );
    assert_eq!(active_tab(front, cx), Some(p(r"C:\d")));
    assert_eq!(tabs_of(window_on(cx, r"C:\c"), cx), [Some(p(r"C:\c"))]);
    assert_eq!(last_active(cx), Some(front.into()));
    assert_eq!(saved_firsts(cx), [p(r"C:\a"), p(r"C:\c")]);
}

/// Single window mode off: the folder gets a window of its own, in front of the last session's.
#[gpui::test]
fn with_single_window_off_the_folder_opens_in_front_of_the_last_session(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    two_saved_windows(cx, false);
    let services = services(&fs);
    let opened =
        cx.update(|cx| crate::open_explorer(cx, &services, Some(p(r"C:\d")), None, Instant::now()));
    assert!(opened);
    settle(cx);
    assert_eq!(workspaces(cx).len(), 3);
    let asked = window_on(cx, r"C:\d");
    assert_eq!(tabs_of(asked, cx), [Some(p(r"C:\d"))]);
    assert_eq!(
        tabs_of(window_on(cx, r"C:\a"), cx),
        [Some(p(r"C:\a")), Some(p(r"C:\b"))]
    );
    assert_eq!(last_active(cx), Some(asked.into()));
    assert_eq!(saved_firsts(cx), [p(r"C:\d"), p(r"C:\a"), p(r"C:\c")]);
}

/// Win+E (File Explorer's Home) with no window open is Arke starting: the last session, without
/// a Home tab beside it.
#[gpui::test]
fn win_e_with_no_window_open_brings_back_the_last_session(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    two_saved_windows(cx, true);
    let services = services(&fs);
    let home = OpenRequest {
        page: Some(crate::instance::Page::Home),
        ..request(None, false)
    };
    let handled = cx.update(|cx| crate::handle_open(cx, &services, home, Instant::now()));
    assert!(handled);
    settle(cx);
    assert_eq!(workspaces(cx).len(), 2);
    let front = window_on(cx, r"C:\a");
    assert_eq!(tabs_of(front, cx), [Some(p(r"C:\a")), Some(p(r"C:\b"))]);
    assert_eq!(
        cx.update(|cx| front.read(cx).unwrap().shown_page_for_test()),
        None
    );
    assert_eq!(saved_firsts(cx), [p(r"C:\a"), p(r"C:\c")]);
}

/// The first window opened on a folder before the settings were read (a slow disk): once they
/// are, the last session comes back around it instead of being saved over.
#[gpui::test]
fn a_folder_opened_before_the_settings_are_read_keeps_the_last_session(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    let services = services(&fs);
    let opened =
        cx.update(|cx| crate::open_explorer(cx, &services, Some(p(r"C:\d")), None, Instant::now()));
    assert!(opened);
    cx.run_until_parked();
    let window = workspaces(cx)[0];
    two_saved_windows(cx, true);
    settle(cx);
    assert_eq!(workspaces(cx).len(), 2);
    assert_eq!(
        tabs_of(window, cx),
        [Some(p(r"C:\a")), Some(p(r"C:\b")), Some(p(r"C:\d"))]
    );
    assert_eq!(
        active_tab(window, cx),
        Some(p(r"C:\d")),
        "the folder in front"
    );
    assert_eq!(tabs_of(window_on(cx, r"C:\c"), cx), [Some(p(r"C:\c"))]);
    assert_eq!(saved_firsts(cx), [p(r"C:\a"), p(r"C:\c")]);
}

/// Windows activates a maximized window when it shows it, even one opened behind, a moment after
/// it opened. The window used last still ends up in front, and first in the session.
#[gpui::test]
fn the_front_window_stays_in_front_when_windows_behind_activate(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    // What `ShowWindowAsync(SW_MAXIMIZE)` does to every window: activates it a little later.
    cx.update(|cx| {
        cx.observe_new(|_: &mut Workspace, window, cx| {
            let Some(window) = window else {
                return;
            };
            cx.spawn_in(window, async move |_, cx| {
                let _ = cx.update(|window, _| window.activate_window());
            })
            .detach();
        })
        .detach();
    });
    settings(cx, |s| {
        s.start_location = StartLocation::LastSession;
        s.session_windows = vec![
            saved_window(&[r"C:\a"], None),
            saved_window(&[r"C:\b"], None),
            saved_window(&[r"C:\c"], None),
        ];
    });
    let services = services(&fs);
    cx.update(|cx| crate::open_explorer(cx, &services, None, None, Instant::now()));
    settle(cx);
    let front = window_on(cx, r"C:\a");
    assert_eq!(last_active(cx), Some(front.into()));
    assert_eq!(saved_firsts(cx), [p(r"C:\a"), p(r"C:\b"), p(r"C:\c")]);
}

/// A second `arke.exe` waits for its answer for 2 s, then opens a window of its own: the request
/// that restores the session is answered once the front window is up, the others open after, and
/// at most 20 come back.
#[gpui::test]
fn restoring_answers_after_the_front_window_and_opens_at_most_twenty(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    settings(cx, |s| {
        s.start_location = StartLocation::LastSession;
        s.session_windows = (0..25)
            .map(|i| saved_window(&[[r"C:\a", r"C:\b", r"C:\c"][i % 3]], None))
            .collect();
    });
    let services = services(&fs);
    let handled =
        cx.update(|cx| crate::handle_open(cx, &services, request(None, false), Instant::now()));
    assert!(handled);
    assert_eq!(
        workspaces(cx).len(),
        1,
        "answered with only the front window open"
    );
    settle(cx);
    assert_eq!(workspaces(cx).len(), crate::settings::RESTORED_WINDOWS_MAX);
}

fn settings_file(name: &str, json: &str) -> (PathBuf, tempfile::TempDir) {
    let guard = tempfile::Builder::new()
        .prefix(&format!("arke-{name}-"))
        .tempdir()
        .unwrap();
    let path = guard.path().join("settings.json");
    std::fs::write(&path, json).unwrap();
    (path, guard)
}

/// The settings file read slowly (%APPDATA% on a network share): a folder visited and Arke
/// quitting before it is read leave the file as it was, instead of the defaults.
#[gpui::test]
fn nothing_is_written_before_the_settings_file_is_read(cx: &mut TestAppContext) {
    let saved = r#"{"show_hidden": true, "recent": ["C:\\old"]}"#;
    let (path, _guard) = settings_file("unread", saved);
    let store = cx.update(|cx| SettingsStore::reading_slowly(path.clone(), cx));
    store.update(cx, |s, cx| {
        s.update(|st| st.remember(Path::new(r"C:\new")), cx);
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    // A write would run on a thread of its own.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    cx.quit();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
}

/// Folders visited before the file was read join it, and are written even when Arke quits right
/// after (the write at quit is for a change newer than any written).
#[gpui::test]
fn folders_visited_before_the_file_is_read_are_written_with_it(cx: &mut TestAppContext) {
    let (path, _guard) = settings_file("late", r#"{"show_hidden": true, "recent": ["C:\\old"]}"#);
    let store = cx.update(|cx| SettingsStore::reading_slowly(path.clone(), cx));
    store.update(cx, |s, cx| {
        s.update(|st| st.remember(Path::new(r"C:\new")), cx);
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(200));
    store.update(cx, |s, cx| s.finish_reading(&path, cx));
    cx.quit();
    let deadline = Instant::now() + Duration::from_secs(10);
    let written = loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.contains("new") {
            break text;
        }
        assert!(Instant::now() < deadline, "not written: {text}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["show_hidden"], true, "{written}");
    assert_eq!(
        written["recent"],
        serde_json::json!([r"C:\new", r"C:\old"]),
        "{written}"
    );
}

/// A saved window whose tabs are the given folders, the ones in `pinned` pinned.
fn saved_window_with_pins(paths: &[&str], pinned: &[&str]) -> SessionWindow {
    SessionWindow {
        session: Session {
            workspace: None,
            tabs: paths
                .iter()
                .map(|path| SessionTab {
                    pinned: pinned.contains(path),
                    ..tab(path)
                })
                .collect(),
            active: 0,
        },
        place: None,
    }
}

/// Pinned tabs come back like a browser's even when the start location is not "Last tabs" (#298):
/// pinned, in their order, in front of the usual start tab, which is the active one.
#[gpui::test]
fn pinned_tabs_come_back_when_last_tabs_is_off(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    settings(cx, |s| {
        s.start_location = StartLocation::Home;
        s.session_windows = vec![saved_window_with_pins(
            &[r"C:\a", r"C:\b", r"C:\c"],
            &[r"C:\a", r"C:\c"],
        )];
    });
    let services = services(&fs);
    assert!(cx.update(|cx| crate::open_explorer(cx, &services, None, None, Instant::now())));
    settle(cx);
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 1, "only the window of the start location");
    let window = windows[0];
    let tabs = tabs_of(window, cx);
    assert_eq!(tabs.len(), 3, "two pinned tabs and the start tab: {tabs:?}");
    assert_eq!(tabs[..2], [Some(p(r"C:\a")), Some(p(r"C:\c"))]);
    // The unpinned tab stays closed.
    assert!(!tabs.contains(&Some(p(r"C:\b"))));
    cx.update(|cx| {
        let ws = window.read(cx).unwrap();
        assert_eq!(ws.pinned_for_test(), [true, true, false]);
        assert_eq!(ws.active, 2, "the start tab is the active one");
    });
}

/// Opened on a folder (Win+E with Arke as the default, "Open in Arke"), the pinned tabs come back
/// in front of it too, and the folder asked for is the active tab (#298).
#[gpui::test]
fn pinned_tabs_come_back_around_a_folder_asked_for(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    settings(cx, |s| {
        s.start_location = StartLocation::Home;
        s.session_windows = vec![saved_window_with_pins(
            &[r"C:\a", r"C:\b", r"C:\c"],
            &[r"C:\a", r"C:\c"],
        )];
    });
    let services = services(&fs);
    let opened =
        cx.update(|cx| crate::open_explorer(cx, &services, Some(p(r"C:\d")), None, Instant::now()));
    assert!(opened);
    settle(cx);
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 1);
    let window = windows[0];
    let tabs = tabs_of(window, cx);
    assert_eq!(
        tabs,
        [Some(p(r"C:\a")), Some(p(r"C:\c")), Some(p(r"C:\d"))],
        "pinned tabs, then the folder asked for"
    );
    cx.update(|cx| {
        let ws = window.read(cx).unwrap();
        assert_eq!(ws.pinned_for_test(), [true, true, false]);
        assert_eq!(ws.active, 2, "the folder asked for is the active tab");
    });
}

/// Without a pinned tab in the last session, the start location alone decides, as before.
#[gpui::test]
fn no_pinned_tabs_leaves_the_start_alone(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    settings(cx, |s| {
        s.start_location = StartLocation::Home;
        s.session_windows = vec![saved_window(&[r"C:\a", r"C:\b"], None)];
    });
    let services = services(&fs);
    assert!(cx.update(|cx| crate::open_explorer(cx, &services, None, None, Instant::now())));
    settle(cx);
    let windows = workspaces(cx);
    assert_eq!(windows.len(), 1);
    assert_eq!(tabs_of(windows[0], cx).len(), 1);
}

/// A pinned tab is saved in the session whatever the start location is, so the next start can
/// bring it back.
#[gpui::test]
fn a_pinned_tab_is_saved_when_last_tabs_is_off(cx: &mut TestAppContext) {
    let fs = fake_fs();
    no_window_yet(cx);
    settings(cx, |s| s.start_location = StartLocation::Home);
    let services = services(&fs);
    assert!(cx.update(|cx| crate::open_explorer(cx, &services, None, None, Instant::now())));
    settle(cx);
    let window = workspaces(cx)[0];
    window
        .update(cx, |ws, window, cx| {
            ws.open_tab(p(r"C:\a"), 1, true, window, cx);
            ws.toggle_pin(1, cx);
        })
        .unwrap();
    settle(cx);
    // The next start brings that tab back.
    let pinned = cx.update(|cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .last_pinned_tabs()
    });
    assert_eq!(
        pinned
            .tabs
            .iter()
            .map(|t| t.panes.clone())
            .collect::<Vec<_>>(),
        [vec![p(r"C:\a")]]
    );
}
