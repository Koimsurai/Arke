//! GPUI tests for how a toast lays out its message and buttons: a message too long to sit beside
//! its buttons gets the toast's whole width with the buttons on a line below, and a short notice
//! keeps its buttons beside it on one line; a progress toast keeps its text on one line.

use gpui::{Bounds, Pixels, px};

use super::*;

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"))
}

/// The error for an archive Arke can't read has two buttons, which used to share the message's
/// row and leave it a narrow column clamped at four lines ("…WavPack, which Ar"). The buttons now
/// move below and the message spans the toast. "Copied" keeps Close beside it, one line tall.
#[gpui::test]
fn a_long_error_gets_the_whole_width_and_a_short_notice_stays_on_one_line(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\tl", vec![file("wavpack.zip", 1)]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\tl");
    wait_until(cx, &view, "load", loaded(r"C:\tl"));
    view.update(cx, |v, cx| {
        v.notice = Some("Copied".to_owned());
        v.error = Some(
            "Couldn't extract wavpack.zip: Compressed with WavPack, which Arke can't read"
                .to_owned(),
        );
        v.error_action = Some(crate::explorer::ErrorAction::OpenDefault(PathBuf::from(
            r"C:\tl\wavpack.zip",
        )));
        cx.notify();
    });
    cx.run_until_parked();

    let notice = bounds(cx, "toast-notice-text");
    let notice_close = bounds(cx, "toast-notice-action-0");
    assert!(
        notice_close.left() >= notice.right() && notice_close.top() < notice.bottom(),
        "Close beside the notice: {notice:?} / {notice_close:?}"
    );
    assert!(
        notice.size.height < notice_close.size.height * 2.,
        "one line: {notice:?}"
    );

    let text = bounds(cx, "toast-error-text");
    let open = bounds(cx, "toast-error-action-0");
    let close = bounds(cx, "toast-error-action-1");
    assert!(
        open.top() >= text.bottom() && close.top() >= text.bottom(),
        "the buttons are below the message: {text:?} / {open:?} / {close:?}"
    );
    assert!(
        text.left() < open.left() && text.right() >= close.right() - px(1.),
        "the message spans the toast above them: {text:?} / {open:?} / {close:?}"
    );
    assert!(close.left() > open.right(), "Close last, at the right");
    assert!(
        text.size.height > notice.size.height * 1.5,
        "the message wraps rather than running off: {text:?}"
    );
}

/// A progress toast keeps its text on one line, ending in "…" when it is too long to sit beside
/// Cancel, so the icon, the text and Cancel share one middle (#403). It used to wrap to two lines
/// with the icon and Cancel off the text.
#[gpui::test]
fn the_progress_toast_keeps_its_text_on_one_line(cx: &mut TestAppContext) {
    use std::sync::atomic::Ordering;
    let shell = Arc::new(FakeShell::default());
    let release = super::quiet_toasts::hold_operations(&shell);
    let (view, cx) = super::quiet_toasts::start_paste(cx, &shell);
    // A count this long doesn't fit beside Cancel.
    view.update(cx, |v, cx| {
        let (progress, _) = v.op_progress.as_ref().expect("the paste reports progress");
        progress.done.store(10u64.pow(18), Ordering::Relaxed);
        cx.notify();
    });
    cx.executor()
        .advance_clock(crate::op_center::TOAST_DELAY + std::time::Duration::from_millis(50));
    cx.run_until_parked();

    let text = bounds(cx, "toast-ops-text");
    let cancel = bounds(cx, "toast-ops-action-0");
    assert!(
        text.size.height < cancel.size.height * 2.,
        "one line: {text:?} / {cancel:?}"
    );
    assert!(
        text.right() <= cancel.left(),
        "Cancel beside the text: {text:?} / {cancel:?}"
    );
    assert!(
        (text.center().y - cancel.center().y).abs() <= px(0.5),
        "the text and Cancel on one middle: {text:?} / {cancel:?}"
    );

    release.send(()).unwrap();
    wait_until(cx, &view, "copy done", |v| v.ops_running == 0);
}
