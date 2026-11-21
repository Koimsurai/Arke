//! GPUI tests for quiet toasts (#266): finished operations say nothing (the operation center
//! lists them), a progress toast shows only for operations that take a while, and the toasts that
//! tell the user something they can't see otherwise stay: the Recycle Bin Undo, failures and
//! "nothing happened" warnings.

use std::time::Duration;

use super::*;
use crate::op_center::TOAST_DELAY;

/// Whether the bottom-right progress toast (with its Cancel button) is drawn.
fn progress_toast(cx: &mut VisualTestContext) -> bool {
    cx.run_until_parked();
    cx.debug_bounds("toast-ops-action-0").is_some()
}

/// Whether a notice toast (with its Close or Undo button) is drawn.
fn notice_toast(cx: &mut VisualTestContext) -> bool {
    cx.run_until_parked();
    cx.debug_bounds("toast-notice-action-0").is_some()
}

/// Makes the fake shell's file operations wait for a message, like a big copy. Returns the sender
/// that lets them finish.
fn hold_operations(shell: &FakeShell) -> flume::Sender<()> {
    let (tx, gate) = flume::unbounded::<()>();
    *shell.on_op.lock() = Some(Box::new(move |_| {
        let _ = gate.recv_timeout(Duration::from_secs(20));
    }));
    tx
}

/// Copies `a.txt` from `C:\q` and pastes it into `C:\q2`; returns once the paste is running.
fn start_paste<'a>(
    cx: &'a mut TestAppContext,
    shell: &Arc<FakeShell>,
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a.txt", 1)]);
    fs.insert_dir(r"C:\q2", vec![]);
    let (view, cx) = setup(cx, fs, Arc::clone(shell), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    cx.simulate_keystrokes("down ctrl-c");
    wait_shell("put on clipboard", || shell.clipboard().is_some());
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\q2"), NavKind::New, cx);
    });
    wait_until(cx, &view, "load q2", loaded(r"C:\q2"));
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, &view, "paste running", |v| v.ops_running == 1);
    wait_shell("copy started", || !shell.ops().is_empty());
    (view, cx)
}

/// A copy that finishes within the delay never shows a toast: not while it runs, not after.
#[gpui::test]
fn a_quick_copy_shows_no_toast_at_any_point(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let release = hold_operations(&shell);
    let (view, cx) = start_paste(cx, &shell);

    assert!(!progress_toast(cx), "just started");
    cx.executor().advance_clock(TOAST_DELAY / 2);
    assert!(!progress_toast(cx), "still within the delay");

    release.send(()).unwrap();
    wait_until(cx, &view, "copy done", |v| v.ops_running == 0);
    // Past the mark: the timer must not bring the toast back for a finished copy.
    cx.executor().advance_clock(TOAST_DELAY * 2);
    assert!(!progress_toast(cx));
    assert!(!notice_toast(cx));
    view.update(cx, |v, _| {
        assert_eq!(v.notice, None);
        assert_eq!(v.error, None);
    });
    // It is in the operation center.
    let jobs = finished_jobs(cx);
    assert!(
        jobs.iter()
            .any(|(title, done)| title.contains("a.txt") && *done),
        "{jobs:?}"
    );
}

/// An operation still running after the delay gets the progress toast, with Cancel, which goes
/// when it ends. Finishing says nothing.
#[gpui::test]
fn a_long_operation_shows_the_progress_toast_after_two_seconds(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let release = hold_operations(&shell);
    let (view, cx) = start_paste(cx, &shell);

    cx.executor().advance_clock(TOAST_DELAY / 2);
    assert!(!progress_toast(cx), "not yet");
    // Nothing else redraws: the timer alone brings the toast up.
    cx.executor()
        .advance_clock(TOAST_DELAY / 2 + Duration::from_millis(50));
    assert!(progress_toast(cx), "shown at the mark");

    release.send(()).unwrap();
    wait_until(cx, &view, "copy done", |v| v.ops_running == 0);
    assert!(!progress_toast(cx), "gone with the operation");
    assert!(!notice_toast(cx), "no 'done' toast");
    view.update(cx, |v, _| assert_eq!(v.notice, None));
}

/// Moving to the Recycle Bin keeps its toast with Undo, and Undo works from it.
#[gpui::test]
fn recycling_still_offers_undo(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\qr");
    fs.insert_dir(dir_path, vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\qr");
    wait_until(cx, &view, "load", loaded(r"C:\qr"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down delete");
    wait_until(cx, &view, "recycled", |v| {
        v.ops_running == 0 && names(v).is_empty() && v.notice.is_some()
    });
    view.update(cx, |v, _| {
        assert_eq!(v.notice.as_deref(), Some(&*tn!("notice.recycled", 1)));
    });
    assert!(notice_toast(cx), "the toast is drawn");
    assert!(
        cx.debug_bounds("toast-notice-action-1").is_some(),
        "Undo and Close"
    );

    click_drawn(cx, "toast-notice-action-0");
    wait_until(cx, &view, "undone", |v| {
        v.ops_running == 0 && !shell.undone.lock().is_empty()
    });
    // The undo itself is quiet.
    assert!(!notice_toast(cx));
    view.update(cx, |v, _| assert_eq!(v.notice, None));
}

/// A permanent delete is quiet when it works.
#[gpui::test]
fn a_permanent_delete_shows_no_toast(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\qp");
    fs.insert_dir(dir_path, vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\qp");
    wait_until(cx, &view, "load", loaded(r"C:\qp"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down shift-delete enter");
    wait_until(cx, &view, "deleted", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    assert!(!notice_toast(cx));
    view.update(cx, |v, _| assert_eq!(v.notice, None));
}

/// A failure keeps its error toast, even for an operation that was quick.
#[gpui::test]
fn a_failed_operation_still_shows_its_error(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\qf");
    fs.insert_dir(dir_path, vec![file("locked.docx", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    shell.fail_delete.lock().push((
        PathBuf::from(r"C:\qf\locked.docx"),
        FailReason::InUse,
        Vec::new(),
    ));
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\qf");
    wait_until(cx, &view, "load", loaded(r"C:\qf"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down delete");
    wait_until(cx, &view, "failed", |v| {
        v.ops_running == 0 && v.error.is_some()
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("toast-error-action-0").is_some(),
        "the error toast is drawn"
    );
    assert!(!progress_toast(cx));
}

/// Undo and redo confirm nothing when they work, and still say so when there is nothing to do.
#[gpui::test]
fn undo_and_redo_are_quiet_but_say_when_nothing_happens(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\qu");
    fs.insert_dir(dir_path, vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\qu");
    wait_until(cx, &view, "load", loaded(r"C:\qu"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down delete");
    wait_until(cx, &view, "recycled", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    cx.simulate_keystrokes("ctrl-z");
    wait_until(cx, &view, "undone", |v| {
        v.ops_running == 0 && !shell.undone.lock().is_empty()
    });
    assert!(!notice_toast(cx), "no 'undone' toast");
    view.update(cx, |v, _| assert_eq!(v.notice, None));

    cx.simulate_keystrokes("ctrl-y");
    wait_shell_in(cx, "redo ran", || shell.ops().len() == 2);
    wait_until(cx, &view, "redone", |v| v.ops_running == 0);
    assert!(!notice_toast(cx), "no 'redone' toast");
    view.update(cx, |v, _| assert_eq!(v.notice, None));

    // Nothing left to redo: the user is told why nothing happened.
    cx.simulate_keystrokes("ctrl-y");
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert_eq!(v.notice.as_deref(), Some(&*t!("redo.nothing")));
    });
    assert!(notice_toast(cx), "the warning is drawn");
}

/// Copying to the clipboard is quiet.
#[gpui::test]
fn copying_to_the_clipboard_shows_no_toast(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\qc", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\qc");
    wait_until(cx, &view, "load", loaded(r"C:\qc"));
    cx.simulate_keystrokes("down ctrl-x");
    wait_shell("put on clipboard", || shell.clipboard().is_some());
    cx.run_until_parked();
    assert!(!notice_toast(cx));
    view.update(cx, |v, _| assert_eq!(v.notice, None));
}
