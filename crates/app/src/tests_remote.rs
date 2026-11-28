//! GPUI tests for remote locations, end to end against a real SFTP server on 127.0.0.1
//! (`arke_remote::testing`): the add dialog with host key confirmation, browsing, new folder,
//! permanent delete with confirmation, and upload by copying in.

use super::*;
use crate::net_dialog::{NetDialogKind, Protocol};
use crate::settings::SettingsStore;
use arke_remote::mtp::Portable;
use arke_remote::testing::{PASSWORD, USER, serve_sftp};
use std::sync::Arc;

/// A folder of its own, and the guard that deletes it when dropped.
fn temp_dir(tag: &str) -> (PathBuf, tempfile::TempDir) {
    let guard = tempfile::Builder::new()
        .prefix(&format!("arke-app-remote-{tag}-"))
        .tempdir()
        .unwrap();
    (guard.path().to_path_buf(), guard)
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
    let (root, _root) = temp_dir("root");
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
}

/// An address typed without a user asks to sign in; after signing in with a user the pane
/// reopens that folder as that user (the password is saved for the user, not for the bare
/// address), instead of asking again.
#[gpui::test]
fn signing_in_to_an_address_without_a_user_opens_the_folder(cx: &mut TestAppContext) {
    let (root, _root) = temp_dir("signin");
    std::fs::write(root.join("hello.txt"), b"hi").unwrap();
    let port = serve_sftp(root, 3);
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_remote(cx, fs, r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));

    view.update(cx, |v, cx| {
        v.navigate(
            PathBuf::from(format!("sftp://127.0.0.1:{port}")),
            NavKind::New,
            cx,
        );
    });
    wait(cx, "sign-in is offered", &|cx| {
        view.update(cx, |v, _| {
            matches!(v.error_action, Some(crate::explorer::ErrorAction::Login(_)))
        })
    });
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.run_error_action(window, cx));
    });
    let dialog = ws.update(cx, |ws, _| ws.net_dialog_for_test()).unwrap();
    dialog.update(cx, |d, cx| {
        d.fill_remote_for_test(
            Protocol::Sftp,
            "127.0.0.1",
            &port.to_string(),
            USER,
            PASSWORD,
            cx,
        );
    });
    wait(cx, "host key prompt", &|cx| {
        dialog.update(cx, |d, _| d.pending_key_for_test()).is_some()
    });
    dialog.update(cx, crate::net_dialog::NetDialog::submit_for_test);
    wait(cx, "dialog closes", &|cx| {
        ws.update(cx, |ws, _| ws.net_dialog_for_test()).is_none()
    });

    wait_until(
        cx,
        &view,
        "folder listed",
        loaded(&format!("sftp://{USER}@127.0.0.1:{port}")),
    );
    assert_eq!(view.update(cx, |v, _| names(v)), ["hello.txt"]);
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

/// A fake phone whose reads are counted.
struct CountingPhone {
    devices: arke_remote::testing_mtp::FakeDevices,
    reads: std::sync::atomic::AtomicUsize,
}

impl Portable for CountingPhone {
    fn devices(&self) -> arke_remote::RemoteResult<Vec<arke_remote::mtp::Device>> {
        self.devices.devices()
    }
    fn list(
        &self,
        device: &str,
        path: &str,
    ) -> arke_remote::RemoteResult<Vec<arke_remote::mtp::Object>> {
        self.devices.list(device, path)
    }
    fn stat(
        &self,
        device: &str,
        path: &str,
    ) -> arke_remote::RemoteResult<arke_remote::mtp::Object> {
        self.devices.stat(device, path)
    }
    fn read(
        &self,
        device: &str,
        path: &str,
        out: &mut dyn std::io::Write,
        max: u64,
        sink: &dyn arke_remote::TransferSink,
    ) -> arke_remote::RemoteResult<u64> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.devices.read(device, path, out, max, sink)
    }
}

/// Copying a phone's folder puts it on the clipboard (as virtual files, for other programs)
/// without reading a byte from the phone or staging anything (#529); pasting it in Arke copies
/// the originals straight from the phone, with progress in bytes.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one copy followed from the clipboard to the pasted tree"
)]
fn copying_from_a_phone_downloads_nothing_and_pasting_downloads_with_progress() {
    use arke_remote::testing_mtp::Node;
    let devices = arke_remote::testing_mtp::FakeDevices::default();
    devices.plug(
        "phone",
        "Phone",
        Node::folder([(
            "Internal",
            Node::folder([(
                "DCIM",
                Node::folder([
                    ("a.jpg", Node::file(b"jpeg bytes")),
                    (
                        "Camera",
                        Node::folder([("c.jpg", Node::file(b"camera bytes"))]),
                    ),
                ]),
            )]),
        )]),
    );
    let phone = Arc::new(CountingPhone {
        devices,
        reads: std::sync::atomic::AtomicUsize::new(0),
    });
    let (cache, _cache) = temp_dir("cache");
    let (dest, _dest) = temp_dir("paste");
    let remotes = crate::remote_shell::test_remotes_with(
        Arc::clone(&phone) as Arc<dyn Portable>,
        cache.clone(),
    );
    let local = Arc::new(FakeShell::default());
    let shell =
        crate::remote_shell::RemoteShell::new(Arc::clone(&local) as Arc<dyn Shell>, remotes);
    let dcim = PathBuf::from("mtp://phone/Internal/DCIM");

    let seq = shell
        .set_clipboard_files(&ClipboardFiles {
            paths: vec![dcim.clone()],
            cut: false,
        })
        .unwrap();
    assert_eq!(
        phone.reads.load(Ordering::Relaxed),
        0,
        "nothing was downloaded"
    );
    assert!(!cache.join("staged").exists(), "nothing was staged");
    assert!(
        local.clipboard.lock().is_none(),
        "no local copies on the clipboard"
    );
    let (items, _) = local.remote_clipboard.lock().clone().unwrap();
    assert_eq!(items.sources, std::slice::from_ref(&dcim));
    assert!(!items.cut);
    assert!(items.listed().is_none(), "nor listed, until a program asks");
    assert!(shell.clipboard_has_files());

    // Paste, as `paste_files` does it: what is on the clipboard, then a tracked copy into the
    // folder (the job the progress center draws).
    let (files, got) = shell.clipboard_files().unwrap().unwrap();
    assert_eq!(got, seq);
    assert_eq!(files.paths, std::slice::from_ref(&dcim));
    assert!(!files.cut);
    let progress = Arc::new(crate::shell::OpProgress::default());
    let op = FileOp::Copy {
        sources: files.paths,
        dest: dest.clone(),
    };
    let (result, undo) = shell.file_op_tracked(&op, None, &progress);
    assert!(matches!(result, Ok(OpOutcome::Done)), "{result:?}");
    assert!(undo.is_none());
    assert_eq!(
        std::fs::read(dest.join(r"DCIM\a.jpg")).unwrap(),
        b"jpeg bytes"
    );
    assert_eq!(
        std::fs::read(dest.join(r"DCIM\Camera\c.jpg")).unwrap(),
        b"camera bytes"
    );
    assert_eq!(
        phone.reads.load(Ordering::Relaxed),
        2,
        "each file read once"
    );
    assert!(progress.work_in_bytes.load(Ordering::Relaxed));
    assert_eq!(progress.bytes_total.load(Ordering::Relaxed), 22);
    assert_eq!(progress.bytes.load(Ordering::Relaxed), 22);

    // A cut is pasted as a move of the originals.
    shell
        .set_clipboard_files(&ClipboardFiles {
            paths: vec![dcim.join("a.jpg")],
            cut: true,
        })
        .unwrap();
    let (files, _) = shell.clipboard_files().unwrap().unwrap();
    assert_eq!(files.paths, [dcim.join("a.jpg")]);
    assert!(files.cut);
    assert_eq!(phone.reads.load(Ordering::Relaxed), 2);

    // Something else copied: that is what a paste takes.
    let other = PathBuf::from(r"C:\o\a.txt");
    shell
        .set_clipboard_files(&ClipboardFiles {
            paths: vec![other.clone()],
            cut: false,
        })
        .unwrap();
    let (files, _) = shell.clipboard_files().unwrap().unwrap();
    assert_eq!(files.paths, [other]);
}

/// A program moved cut remote items out of a folder (#529): every pane showing that folder
/// relists, the way panes do after Arke's own operations on a server (which sends no change
/// notifications). The folder is a fake local one here; the relist only compares paths.
#[gpui::test]
fn a_folder_changed_outside_arke_is_relisted_where_it_shows(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![dir("moved"), file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, Arc::clone(&fs), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    assert!(
        view.update(cx, |v, _| names(v))
            .contains(&"moved".to_owned())
    );

    // Gone without a change notification, as on a server.
    fs.remove(Path::new(r"C:\o"), "moved");
    cx.cx
        .update(|cx| crate::workspace::reload_remote_everywhere(Path::new(r"C:\elsewhere"), cx));
    cx.run_until_parked();
    assert!(
        view.update(cx, |v, _| names(v))
            .contains(&"moved".to_owned()),
        "a pane showing another folder is left alone"
    );
    cx.cx
        .update(|cx| crate::workspace::reload_remote_everywhere(Path::new(r"C:\o"), cx));
    wait_until(cx, &view, "relisted", |v| {
        !names(v).contains(&"moved".to_owned())
    });
}
