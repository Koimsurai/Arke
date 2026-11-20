//! GPUI tests for items too big for the Recycle Bin: the recycle keeps them back, and Arke asks
//! whether to delete them permanently, instead of Windows asking behind the window.

use super::*;

const DIR: &str = r"C:\tb";

fn big() -> PathBuf {
    PathBuf::from(r"C:\tb\big.iso")
}

/// Lists `C:\tb` (`a.txt`, `big.iso`, `locked.docx`) and recycles all of it; `big.iso` is too big
/// for the Recycle Bin. Returns once the recycle is over, `left` is what is still listed, and Arke
/// asks.
fn recycle_all<'a>(
    cx: &'a mut TestAppContext,
    shell: &Arc<FakeShell>,
    left: &'static [&'static str],
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let fs = FakeFs::new();
    let dir = Path::new(DIR);
    fs.insert_dir(
        dir,
        vec![file("a.txt", 1), file("big.iso", 1), file("locked.docx", 1)],
    );
    apply_to_fake_fs(&fs, shell);
    shell
        .fail_delete
        .lock()
        .push((big(), FailReason::TooBigForBin, Vec::new()));
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(shell), DIR);
    wait_until(cx, &view, "load", loaded(DIR));
    wait_watcher(&fs, dir);

    cx.simulate_keystrokes("ctrl-a delete");
    wait_until(cx, &view, "recycled, and Arke asks", |v| {
        v.ops_running == 0 && v.confirm_title().is_some() && names(v) == left
    });
    let ops = shell.ops();
    assert!(
        matches!(ops.as_slice(), [FileOp::Delete { recycle: true, .. }]),
        "{ops:?}"
    );
    (view, cx)
}

/// What Arke deletes permanently once the user agrees: exactly the item that was too big.
fn purge_big() -> FileOp {
    FileOp::Delete {
        paths: vec![big()],
        recycle: false,
    }
}

/// The rest goes to the Recycle Bin right away; the big item stays, and instead of an error
/// toast Arke asks once. Delete removes just that item, permanently, with no second question.
#[gpui::test]
fn too_big_for_the_bin_asks_then_deletes_just_that_permanently(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = recycle_all(cx, &shell, &["big.iso"]);
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("recycle.confirm_purge_title"))
        );
        let body = v.confirm_body().unwrap();
        assert!(
            body.contains(&*t!("confirm.too_big_one", name = "big.iso")),
            "{body}"
        );
        assert_eq!(v.error, None, "no error toast for it");
    });

    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "deleted permanently", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    let ops = shell.ops();
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(ops.last(), Some(&purge_big()));
    view.update(cx, |v, _| {
        assert_eq!(v.confirm_title(), None, "asked once");
        assert_eq!(v.error, None);
    });
}

/// Cancel leaves the item where it is and runs nothing.
#[gpui::test]
fn cancelling_the_too_big_question_keeps_the_item(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = recycle_all(cx, &shell, &["big.iso"]);

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(shell.ops().len(), 1, "nothing else runs");
    view.update(cx, |v, _| {
        assert_eq!(v.confirm_title(), None);
        assert!(v.too_big_ask.is_none(), "not asked again");
        assert_eq!(names(v), ["big.iso"]);
        assert_eq!(v.error, None);
    });
}

/// Too big and in use in the same recycle: Arke asks about the big one, and the in-use one still
/// gets its error toast with Retry (for it alone).
#[gpui::test]
fn too_big_and_in_use_ask_for_one_and_report_the_other(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let locked = PathBuf::from(r"C:\tb\locked.docx");
    shell
        .fail_delete
        .lock()
        .push((locked.clone(), FailReason::InUse, Vec::new()));
    let (view, cx) = recycle_all(cx, &shell, &["big.iso", "locked.docx"]);
    view.update(cx, |v, _| {
        let body = v.confirm_body().unwrap();
        assert!(
            body.contains(&*t!("confirm.too_big_one", name = "big.iso")),
            "{body}"
        );
        let error = v.error.clone().expect("the in-use item's toast");
        assert!(
            error.contains(&*t!("fail.in_use", name = "locked.docx")),
            "{error}"
        );
        assert!(!error.contains("big.iso"), "{error}");
        assert_eq!(
            v.error_action,
            Some(crate::explorer::ErrorAction::RetryOp(Box::new(
                FileOp::Delete {
                    paths: vec![locked],
                    recycle: true,
                }
            )))
        );
    });

    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "big one deleted permanently", |v| {
        v.ops_running == 0 && names(v) == ["locked.docx"]
    });
    let ops = shell.ops();
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(ops.last(), Some(&purge_big()));
}
