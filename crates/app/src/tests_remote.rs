//! GPUI tests for remote locations, end to end against a real SFTP server on 127.0.0.1
//! (`arke_remote::testing`): the add dialog with host key confirmation, browsing, new folder,
//! permanent delete with confirmation, and upload by copying in.

use super::*;
use crate::net_dialog::{NetDialogKind, Protocol};
use crate::settings::SettingsStore;
use arke_remote::testing::{PASSWORD, USER, serve_sftp};
use std::sync::Arc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("arke-app-remote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup_remote<'a>(
    cx: &'a mut TestAppContext,
    fs: Arc<FakeFs>,
    start: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::commands::init(&crate::settings::SettingsStore::global(cx), cx);
    });
    let remotes = crate::remote_shell::test_remotes();
    let services = Services {
        fs: Arc::new(arke_remote::RoutingFs::new(fs, Arc::clone(&remotes))),
        shell: Arc::new(crate::remote_shell::RemoteShell::new(
            Arc::new(FakeShell::default()),
            Arc::clone(&remotes),
        )),
        remotes,
    };
    let start = PathBuf::from(start);
    cx.add_window_view(move |window, cx| {
        Workspace::new(
            services,
            start,
            false,
            None,
            None,
            Instant::now(),
            window,
            cx,
        )
    })
}

fn wait(cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The app's shell is the local one wrapped for remote locations: what Windows says about Quick
/// Access pins and recent files must come through it, not the trait's defaults.
#[test]
fn the_wrapped_shell_answers_like_the_local_one() {
    let local = Arc::new(FakeShell::default());
    *local.quick_access.lock() = Some(vec![PathBuf::from(r"C:\w\pinned")]);
    local.recent_off.store(true, Ordering::Relaxed);
    let shell = crate::remote_shell::RemoteShell::new(local, crate::remote_shell::test_remotes());
    let (pins, recent) =
        std::thread::spawn(move || (shell.quick_access_pinned(), shell.recent_files_enabled()))
            .join()
            .unwrap();
    assert_eq!(pins, Some(vec![PathBuf::from(r"C:\w\pinned")]));
    assert!(!recent);
}

#[gpui::test]
fn adds_an_sftp_location_and_works_in_it(cx: &mut TestAppContext) {
    let root = temp_dir("root");
    std::fs::write(root.join("hello.txt"), b"hi").unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    let port = serve_sftp(root.clone(), 3);
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_remote(cx, fs, r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));

    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_net_dialog(NetDialogKind::Map { letters: vec!['Z'] }, None, window, cx);
        });
    });
    let dialog = ws.update(cx, |ws, _| ws.net_dialog_for_test()).unwrap();
    let port_text = port.to_string();
    dialog.update(cx, |d, cx| {
        d.fill_remote_for_test(Protocol::Sftp, "127.0.0.1", &port_text, USER, PASSWORD, cx);
    });

    // First connection: the fingerprint is shown, nothing connects yet.
    wait(cx, "host key prompt", &|cx| {
        dialog.update(cx, |d, _| d.pending_key_for_test()).is_some()
    });
    let (fingerprint, changed) = dialog.update(cx, |d, _| d.pending_key_for_test()).unwrap();
    assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
    assert!(!changed);
    assert_eq!(
        view.update(cx, |v, _| v.current_path().map(Path::to_path_buf)),
        Some(PathBuf::from(r"C:\o"))
    );

    // Trust and connect: the dialog closes, the location is saved and the tab opens it.
    dialog.update(cx, crate::net_dialog::NetDialog::submit_for_test);
    wait(cx, "dialog closes", &|cx| {
        ws.update(cx, |ws, _| ws.net_dialog_for_test()).is_none()
    });
    let url = format!("sftp://{USER}@127.0.0.1:{port}");
    wait_until(cx, &view, "server root listed", loaded(&url));
    let mut listed = view.update(cx, |v, _| names(v));
    listed.sort();
    assert_eq!(listed, ["docs", "hello.txt"]);
    let saved = cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .remote_places
            .clone()
    });
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].url, PathBuf::from(&url));

    // New folder is created on the server and shows up (relisted: no watcher on servers).
    cx.simulate_keystrokes("ctrl-shift-n");
    let new_folder = t!("ops.new_folder_name").into_owned();
    wait(cx, "new folder on the server", &|_| {
        root.join(&new_folder).is_dir()
    });
    wait_until(cx, &view, "new folder listed", |v| {
        names(v).contains(&new_folder)
    });

    // Delete on a server is permanent: it asks first, then removes it.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        let ix = v
            .view()
            .unwrap()
            .iter()
            .position(|e| e.name.as_ref() == "hello.txt")
            .unwrap();
        v.select_index(ix, cx);
    });
    cx.dispatch_action(DeleteFiles);
    cx.run_until_parked();
    assert!(
        view.update(cx, |v, _| v.confirm_title()).is_some(),
        "no confirmation before a permanent delete"
    );
    assert!(root.join("hello.txt").exists());
    cx.simulate_keystrokes("enter");
    wait(cx, "deleted on the server", &|_| {
        !root.join("hello.txt").exists()
    });
    wait_until(cx, &view, "gone from the list", |v| {
        !names(v).contains(&"hello.txt".to_owned())
    });

    let _ = std::fs::remove_dir_all(&root);
}

/// Plain http with a password is refused until the user allows it for that location: the dialog
/// warns first, and allowing lets the sign-in go on. The test hub has no HTTP, so the attempt
/// after allowing fails with its own message, which also takes the consent back (it was not used).
#[gpui::test]
fn plain_http_sign_in_asks_before_sending_the_password(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_remote(cx, fs, r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_net_dialog(NetDialogKind::Map { letters: vec!['Z'] }, None, window, cx);
        });
    });
    let dialog = ws.update(cx, |ws, _| ws.net_dialog_for_test()).unwrap();
    dialog.update(cx, |d, cx| {
        d.fill_remote_for_test(Protocol::WebDav, "http://nas.invalid/", "", "u", "pw", cx);
    });
    wait(cx, "plain http warning", &|cx| {
        dialog.update(cx, |d, _| d.pending_insecure_for_test())
    });
    assert_eq!(dialog.update(cx, |d, _| d.error_for_test()), None);

    // Allow for this location: the sign-in goes on (and fails here for lack of HTTP).
    dialog.update(cx, crate::net_dialog::NetDialog::submit_for_test);
    wait(cx, "the sign-in was tried", &|cx| {
        dialog.update(cx, |d, _| d.error_for_test()).is_some()
    });
    assert!(!dialog.update(cx, |d, _| d.pending_insecure_for_test()));
    assert_ne!(
        dialog.update(cx, |d, _| d.error_for_test()),
        Some(arke_remote::RemoteError::Insecure.to_string())
    );
    // Nothing is remembered from a sign-in that did not work.
    let allowed = cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .insecure_http
            .clone()
    });
    assert!(allowed.is_empty(), "{allowed:?}");
}

#[gpui::test]
fn unreachable_server_fails_fast_with_retry(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_remote(cx, fs, r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let started = Instant::now();
    view.update(cx, |v, cx| {
        v.navigate(
            PathBuf::from(format!("sftp://u@127.0.0.1:{port}/x")),
            NavKind::New,
            cx,
        );
    });
    wait_until(cx, &view, "error shown", |v| {
        v.error.is_some() || v.failed.is_some()
    });
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(
        view.update(cx, |v, _| v.error_action.clone()),
        Some(crate::explorer::ErrorAction::Retry(_))
    ));
    // The toast gives the reason only: with the path in front it ran past its four lines.
    if let Some(error) = view.update(cx, |v, _| v.error.clone()) {
        assert!(!error.contains("sftp://"), "{error}");
    }
}

/// The routing file system passes link targets (the Link target column) through to the local file
/// system and refuses them on servers.
#[test]
fn routing_fs_reads_local_link_targets_only() {
    use arke_core::fs::Fs as _;
    let local = FakeFs::new();
    local.insert_link(r"C:\l\docs", r"D:\docs");
    let fs = arke_remote::RoutingFs::new(local, crate::remote_shell::test_remotes());
    assert_eq!(
        fs.read_link(Path::new(r"C:\l\docs")).ok(),
        Some(PathBuf::from(r"D:\docs"))
    );
    assert!(fs.read_link(Path::new("sftp://u@h/docs")).is_err());
}

/// A whole URL in the Server field wins over the Port and folder fields: they show the URL's port
/// and path instead (dimmed), keep what was typed for when the URL goes, and the location is the
/// same as before.
#[gpui::test]
fn a_url_in_the_server_field_shows_its_port_and_path(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_remote(cx, fs, r"C:\o");
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_net_dialog(NetDialogKind::Map { letters: vec!['Z'] }, None, window, cx);
        });
    });
    let dialog = ws.update(cx, |ws, _| ws.net_dialog_for_test()).unwrap();
    dialog.update(cx, |d, cx| {
        d.type_remote_for_test(Protocol::WebDav, ["nas", "8443", "/mine"], cx);
    });
    assert_eq!(dialog.update(cx, |d, cx| d.url_values_for_test(cx)), None);

    dialog.update(cx, |d, cx| {
        d.type_remote_for_test(
            Protocol::WebDav,
            ["http://nas:8080/dav", "8443", "/mine"],
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        dialog.update(cx, |d, cx| d.url_values_for_test(cx)),
        Some(("8080".to_owned(), "/dav".to_owned()))
    );
    let url = dialog
        .update(cx, |d, cx| d.remote_url_for_test(cx))
        .unwrap();
    assert_eq!((url.port, url.path.as_str()), (8080, "/dav"));
    assert_eq!(dialog.update(cx, |d, cx| d.port_text_for_test(cx)), "8443");

    // No port in the URL: the scheme's own.
    dialog.update(cx, |d, cx| {
        d.type_remote_for_test(Protocol::WebDav, ["http://nas", "8443", "/mine"], cx);
    });
    assert_eq!(
        dialog.update(cx, |d, cx| d.url_values_for_test(cx)),
        Some(("80".to_owned(), "/".to_owned()))
    );

    // The "://" goes: the typed values apply again.
    dialog.update(cx, |d, cx| {
        d.type_remote_for_test(Protocol::WebDav, ["nas", "8443", "/mine"], cx);
    });
    assert_eq!(dialog.update(cx, |d, cx| d.url_values_for_test(cx)), None);
    let url = dialog
        .update(cx, |d, cx| d.remote_url_for_test(cx))
        .unwrap();
    assert_eq!((url.port, url.path.as_str()), (8443, "/mine"));
}
