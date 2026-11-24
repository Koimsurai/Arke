//! GPUI tests for extracting a password-protected archive (#346): the dialog it asks with, a
//! wrong password, cancelling, and the folder and operation center left behind.

use super::*;

/// A real folder under %TEMP% (extraction writes to disk) with `secret.zip`: AES-256, password
/// "secret", holding `a.txt`. Made by 7-Zip: tests never encrypt anything, since anti-ransomware
/// tools quarantine a test process that does.
fn sandbox_with_secret(tag: &str) -> PathBuf {
    let sandbox = std::env::temp_dir().join(format!(
        "arke-archive-password-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).unwrap();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\core\src\archive\tests\fixtures\aes.zip");
    std::fs::copy(fixture, sandbox.join("secret.zip")).unwrap();
    sandbox
}

fn open<'a>(
    cx: &'a mut TestAppContext,
    sandbox: &Path,
) -> (
    Entity<Workspace>,
    Entity<Explorer>,
    &'a mut VisualTestContext,
) {
    let fs = FakeFs::new();
    fs.insert_dir(sandbox, vec![file("secret.zip", 300)]);
    let dir = sandbox.to_string_lossy().into_owned();
    let (ws, cx) = setup_workspace(cx, fs, &dir);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(&dir));
    (ws, view, cx)
}

fn dialog(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<crate::archive_password::ArchivePasswordDialog>> {
    ws.update(cx, |ws, _| ws.archive_password_for_test())
}

/// Waits for the extraction to end and the dialog to be open or closed (`open`).
fn wait_dialog(
    ws: &Entity<Workspace>,
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    open: bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        let idle = view.update(cx, |v, _| v.extract_job.is_none());
        if idle && dialog(ws, cx).is_some() == open {
            return;
        }
        assert!(Instant::now() < deadline, "dialog open: {open}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn enter(ws: &Entity<Workspace>, cx: &mut VisualTestContext, password: &str) {
    let dialog = dialog(ws, cx).expect("dialog open");
    dialog.update(cx, |d, cx| {
        d.type_for_test(password, cx);
        d.submit_for_test(cx);
    });
}

#[gpui::test]
fn a_protected_zip_asks_for_its_password(cx: &mut TestAppContext) {
    let sandbox = sandbox_with_secret("asks");
    let (ws, view, cx) = open(cx, &sandbox);

    right_click(&view, cx, Some(0));
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    wait_dialog(&ws, &view, cx, true);
    // Asking isn't an error, and the folder made for it is gone until it has something in it.
    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    assert!(!sandbox.join("secret").exists());
    assert!(
        !dialog(&ws, cx)
            .unwrap()
            .update(cx, |d, _| d.wrong_for_test())
    );

    // A wrong password: asked again, saying so, with nothing written.
    enter(&ws, cx, "guess");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dialog(&ws, cx).is_some_and(|d| d.update(cx, |d, _| d.wrong_for_test())) {
        assert!(
            Instant::now() < deadline,
            "asked again, saying it was wrong"
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!sandbox.join("secret").exists());

    // The right one extracts into the folder, selected, as one job in the operation center.
    enter(&ws, cx, "secret");
    wait_until(cx, &view, "extracted", |v| {
        v.extract_job.is_none() && v.selection.contains("secret")
    });
    assert!(dialog(&ws, cx).is_none());
    let text = std::fs::read_to_string(sandbox.join("secret").join("a.txt")).unwrap();
    assert!(text.starts_with("Arke reads this back unchanged."));
    let jobs = finished_jobs(cx);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert!(jobs[0].1, "{jobs:?}");
    let _ = std::fs::remove_dir_all(&sandbox);
}

#[gpui::test]
fn cancelling_the_password_extracts_nothing(cx: &mut TestAppContext) {
    let sandbox = sandbox_with_secret("cancel");
    let (ws, view, cx) = open(cx, &sandbox);

    right_click(&view, cx, Some(0));
    run_command(&view, cx, MenuCommand::ExtractHere);
    wait_dialog(&ws, &view, cx, true);
    cx.dispatch_action(crate::archive_password::ArchivePasswordCancel);
    wait_dialog(&ws, &view, cx, false);
    assert!(!sandbox.join("a.txt").exists());
    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    assert!(finished_jobs(cx).is_empty());
    let _ = std::fs::remove_dir_all(&sandbox);
}
