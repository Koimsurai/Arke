//! GPUI tests for virtual files dropped from other programs (#206): Outlook attachments,
//! browser images, files in zip folders.

use super::*;
use crate::shell::Undo;
use crate::win_virtual_drop::tests::{FakeItem, FakeSource, inject, pattern};
use gpui::{ExternalPaths, FileDropEvent, point, px};

/// A moved item's name and its (first) file's bytes.
type Moved = (String, Vec<u8>);

/// Drops `data` on the list's empty space, as the platform does for virtual files.
fn drop_virtual(
    cx: &mut VisualTestContext,
    view: &Entity<Explorer>,
    data: ::windows::Win32::System::Com::IDataObject,
) {
    inject(data);
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let at = point(bounds.left() + px(200.), bounds.top() + row * 6.);
    let marker = vec![PathBuf::from(gpui_windows::VIRTUAL_DROP_ROOT)];
    cx.simulate_event(FileDropEvent::Entered {
        position: at,
        paths: ExternalPaths(marker.into()),
    });
    cx.simulate_event(FileDropEvent::Submit { position: at });
}

/// Dropped virtual files are written to a folder of Arke's own, then moved into the folder
/// they were dropped on (so conflicts ask like any copy); undo removes them like a copy.
#[gpui::test]
fn virtual_files_dropped_on_a_folder_land_there(cx: &mut TestAppContext) {
    let _com = crate::win_virtual_drop::ComThread::init();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![]);
    let shell = Arc::new(FakeShell::default());
    // What the move found where the drop was held.
    let moved: Arc<Mutex<Vec<Moved>>> = Arc::default();
    {
        let moved = Arc::clone(&moved);
        *shell.on_op.lock() = Some(Box::new(move |op| {
            if let FileOp::Move { sources, .. } = op {
                for source in sources {
                    let file = if source.is_dir() {
                        source.join("cat.jpg")
                    } else {
                        source.clone()
                    };
                    let name = source.file_name().unwrap().to_string_lossy().into_owned();
                    moved.lock().push((name, std::fs::read(file).unwrap()));
                }
            }
        }));
    }
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    cx.run_until_parked();

    let source = FakeSource::new(vec![
        FakeItem::memory("report.txt", b"hello"),
        FakeItem::folder("photos"),
        FakeItem::stream("photos\\cat.jpg", 2000),
    ]);
    drop_virtual(cx, &view, source.data());
    wait_until(cx, &view, "drop done", |v| {
        v.ops_running == 0 && !shell.ops().is_empty()
    });

    let ops = shell.ops();
    let [FileOp::Move { sources, dest }] = ops.as_slice() else {
        panic!("one move expected: {:?}", shell.ops());
    };
    assert_eq!(dest, Path::new(r"C:\a"));
    let names: Vec<_> = sources
        .iter()
        .map(|s| s.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["report.txt", "photos"]);
    let held = sources[0].parent().unwrap();
    assert!(
        held.starts_with(std::env::temp_dir().join("Arke drops")),
        "{held:?}"
    );
    assert_eq!(
        *moved.lock(),
        [
            ("report.txt".to_owned(), b"hello".to_vec()),
            ("photos".to_owned(), (0..2000).map(pattern).collect()),
        ]
    );
    assert!(!held.exists(), "the folder holding the drop is removed");
    let (verb, undo) = cx.update(|_, cx| {
        let last = crate::explorer::UndoHistory::last(cx).unwrap();
        (last.verb, last.undo.clone())
    });
    assert_eq!(verb, "op.copy");
    assert_eq!(
        undo,
        Undo::Remove(vec![
            PathBuf::from(r"C:\a\report.txt"),
            PathBuf::from(r"C:\a\photos"),
        ])
    );
    view.update(cx, |v, _| assert_eq!(v.error, None));
}

/// One file dropped is one item done, though it is written to Arke's folder first and then
/// moved, and the move counts it too (#456).
#[gpui::test]
fn one_virtual_file_dropped_is_one_item_done(cx: &mut TestAppContext) {
    use crate::op_center::{JobState, OpCenter};
    let _com = crate::win_virtual_drop::ComThread::init();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![]);
    let shell = Arc::new(FakeShell::default());
    shell.counts_done.store(true, Ordering::Relaxed);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    cx.run_until_parked();

    let source = FakeSource::new(vec![FakeItem::stream("big.iso", 3000)]);
    drop_virtual(cx, &view, source.data());
    wait_until(cx, &view, "drop done", |v| {
        v.ops_running == 0 && !shell.ops().is_empty()
    });
    let (finished, done) = cx.update(|_, cx| {
        let job = OpCenter::jobs(cx)
            .iter()
            .rfind(|job| job.verb == "op.copy")
            .expect("the drop is listed as a copy");
        (matches!(job.state, JobState::Done { .. }), job.done_items())
    });
    assert!(finished);
    assert_eq!(done, 1);
}

/// A source that describes nothing it can hand over: said so, and nothing runs.
#[gpui::test]
fn a_virtual_drop_with_nothing_in_it_says_so(cx: &mut TestAppContext) {
    let _com = crate::win_virtual_drop::ComThread::init();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    cx.run_until_parked();

    drop_virtual(cx, &view, FakeSource::new(Vec::new()).data());
    wait_until(cx, &view, "error", |v| v.error.is_some());
    view.update(cx, |v, _| {
        assert_eq!(v.error.as_deref(), Some(&*t!("ops.virtual_none")));
        assert_eq!(v.ops_running, 0);
    });
    assert!(shell.ops().is_empty());
}

/// Virtual files on the clipboard (a Remote Desktop copy, an Outlook attachment) paste like a
/// drop: written to a folder of Arke's own, moved into the current folder, listed as a copy (#221).
#[gpui::test]
fn virtual_files_on_the_clipboard_are_pasted(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![]);
    let shell = Arc::new(FakeShell::default());
    let moved: Arc<Mutex<Vec<Moved>>> = Arc::default();
    {
        let moved = Arc::clone(&moved);
        *shell.on_op.lock() = Some(Box::new(move |op| {
            if let FileOp::Move { sources, .. } = op {
                for source in sources {
                    let name = source.file_name().unwrap().to_string_lossy().into_owned();
                    moved.lock().push((name, std::fs::read(source).unwrap()));
                }
            }
        }));
    }
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    cx.run_until_parked();

    let source = FakeSource::new(vec![
        FakeItem::memory("report.txt", b"hello"),
        FakeItem::stream("big.bin", 3000),
    ]);
    *shell.virtual_clipboard.lock() = Some(FakeClipboardData(source.data()));
    assert!(shell.clipboard_has_files(), "Paste is offered");
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, &view, "paste done", |v| {
        v.ops_running == 0 && !shell.ops().is_empty()
    });

    let ops = shell.ops();
    let [FileOp::Move { sources, dest }] = ops.as_slice() else {
        panic!("one move expected: {:?}", shell.ops());
    };
    assert_eq!(dest, Path::new(r"C:\a"));
    assert_eq!(
        *moved.lock(),
        [
            ("report.txt".to_owned(), b"hello".to_vec()),
            ("big.bin".to_owned(), (0..3000).map(pattern).collect()),
        ]
    );
    assert!(!sources[0].parent().unwrap().exists());
    let (verb, undo) = cx.update(|_, cx| {
        let last = crate::explorer::UndoHistory::last(cx).unwrap();
        (last.verb, last.undo.clone())
    });
    assert_eq!(verb, "op.copy");
    assert_eq!(
        undo,
        Undo::Remove(vec![
            PathBuf::from(r"C:\a\report.txt"),
            PathBuf::from(r"C:\a\big.bin"),
        ])
    );
    view.update(cx, |v, _| assert_eq!(v.error, None));
}

/// Files listed on disk (`CF_HDROP`) paste the way they always did, even when virtual files
/// are offered too.
#[gpui::test]
fn files_on_disk_are_pasted_before_virtual_ones(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    cx.run_until_parked();

    *shell.clipboard.lock() = Some((
        ClipboardFiles {
            paths: vec![PathBuf::from(r"C:\other\x.txt")],
            cut: false,
        },
        1,
    ));
    *shell.virtual_clipboard.lock() = Some(FakeClipboardData(
        FakeSource::new(vec![FakeItem::memory("v.txt", b"v")]).data(),
    ));
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, &view, "paste done", |v| {
        v.ops_running == 0 && !shell.ops().is_empty()
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\other\x.txt")],
            dest: PathBuf::from(r"C:\a"),
        }]
    );
}

/// Virtual files that can't be read say why, not that the clipboard has no files.
#[gpui::test]
fn virtual_files_on_the_clipboard_that_cant_be_read_show_the_error(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));
    cx.run_until_parked();

    *shell.virtual_clipboard.lock() = Some(FakeClipboardData(FakeSource::new(Vec::new()).data()));
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, &view, "error", |v| v.error.is_some());
    view.update(cx, |v, _| {
        let error = v.error.as_deref().unwrap();
        assert!(error.contains(&*t!("ops.virtual_none")), "{error}");
        assert!(!error.contains(&*t!("clip.empty")), "{error}");
        assert_eq!(v.ops_running, 0);
    });
    assert!(shell.ops().is_empty());
}
