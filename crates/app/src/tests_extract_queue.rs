//! GPUI tests for extracting several archives at once (#407): each goes into a folder of its own,
//! and a tab's extractions and compressions run one after another. The ones waiting are listed in
//! the operation center, where Cancel takes one out of the line before it starts.
//!
//! Extraction writes to a real disk, so the FakeFs folder is a real folder under %TEMP%.

use std::io::Write as _;

use super::*;
use crate::op_center::{Job, JobState, OpCenter};
use crate::shell::OpProgress;

/// `%TEMP%\arke-extract-queue-<tag>-<pid>`, empty.
fn sandbox(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("arke-extract-queue-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A zip at `path` holding `inner` with `body`.
fn make_zip(path: &Path, inner: &str, body: &str) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    zip.start_file(inner, zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(body.as_bytes()).unwrap();
    zip.finish().unwrap();
}

/// A tar at `path` holding `inner` with `body`, written by hand (one ustar header, the data,
/// and the two empty blocks that end it).
fn make_tar(path: &Path, inner: &str, body: &str) {
    let mut header = [0u8; 512];
    header[..inner.len()].copy_from_slice(inner.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    // The checksum counts its own field as spaces.
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(body.as_bytes());
    bytes.resize(bytes.len().div_ceil(512) * 512 + 1024, 0);
    std::fs::write(path, bytes).unwrap();
}

/// The folder opened in a view, listing `names`.
fn open<'a>(
    cx: &'a mut TestAppContext,
    dir: &Path,
    names: &[&str],
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(dir, names.iter().map(|n| file(n, 100)).collect());
    let dir_str = dir.to_string_lossy().into_owned();
    let (view, cx) = setup(cx, fs, Arc::default(), &dir_str);
    wait_until(cx, &view, "load", loaded(&dir_str));
    (view, cx)
}

/// Selects `names` and opens the context menu on the first of them.
fn menu_on(view: &Entity<Explorer>, cx: &mut VisualTestContext, names: &[&str]) {
    let row = view
        .update(cx, |v, _| {
            v.selection
                .select_names(names.iter().map(|n| Arc::<str>::from(*n)));
            view_names(v).iter().position(|n| n == names[0])
        })
        .expect("first item is listed");
    right_click(view, cx, Some(row));
    assert_eq!(view.update(cx, |v, _| v.selection.len()), names.len());
}

/// The extractions in the operation center, oldest first.
fn extractions(cx: &mut VisualTestContext) -> Vec<(String, JobState, bool)> {
    cx.update(|_, cx| {
        OpCenter::jobs(cx)
            .iter()
            .filter(|job| job.verb == "op.extract")
            .map(|job: &Job| (job.title(), job.state.clone(), job.queued()))
            .collect()
    })
}

/// Waits until the operation center's extractions match `pred`.
fn wait_extractions(
    cx: &mut VisualTestContext,
    what: &str,
    pred: impl Fn(&[(String, JobState, bool)]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if pred(&extractions(cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Several archives selected: the menu offers to extract each to its own folder, and each is
/// extracted into a folder named after it, one after another.
#[gpui::test]
fn several_archives_extract_each_into_its_own_folder(cx: &mut TestAppContext) {
    let dir = sandbox("each");
    make_zip(&dir.join("one.zip"), "one.txt", "first");
    make_zip(&dir.join("two.zip"), "inside/two.txt", "second");
    let (view, cx) = open(cx, &dir, &["one.zip", "two.zip"]);

    menu_on(&view, cx, &["one.zip", "two.zip"]);
    let labels = menu_labels(&view, cx).expect("menu open");
    assert!(
        labels.iter().any(|l| l.contains(&*t!("menu.extract_each"))),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains(&*t!("menu.extract_here"))),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    wait_until(cx, &view, "both extracted", |v| v.extract_jobs.is_empty());

    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    assert_eq!(
        std::fs::read_to_string(dir.join("one").join("one.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("two").join("inside").join("two.txt")).unwrap(),
        "second"
    );
    let done = extractions(cx);
    assert_eq!(done.len(), 2, "{done:?}");
    assert!(
        done.iter()
            .all(|(_, state, _)| matches!(state, JobState::Done { .. })),
        "{done:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two archives that would make the same folder: the second, queued before the first has made
/// it, still gets a folder of its own.
#[gpui::test]
fn queued_archives_named_alike_get_folders_of_their_own(cx: &mut TestAppContext) {
    let dir = sandbox("alike");
    make_zip(&dir.join("photos.zip"), "a.txt", "from zip");
    make_tar(&dir.join("photos.tar"), "b.txt", "from tar");
    let (view, cx) = open(cx, &dir, &["photos.tar", "photos.zip"]);

    menu_on(&view, cx, &["photos.tar", "photos.zip"]);
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    wait_until(cx, &view, "both extracted", |v| v.extract_jobs.is_empty());

    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    let first = dir.join("photos");
    let second = dir.join("photos (2)");
    let read = |folder: &Path, name: &str| std::fs::read_to_string(folder.join(name)).ok();
    let (tar_in, zip_in) = if read(&first, "b.txt").is_some() {
        (&first, &second)
    } else {
        (&second, &first)
    };
    assert_eq!(read(tar_in, "b.txt").as_deref(), Some("from tar"));
    assert_eq!(read(zip_in, "a.txt").as_deref(), Some("from zip"));
    assert!(
        read(tar_in, "a.txt").is_none(),
        "not extracted into one folder"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// While this tab is busy with an archive (its turn held here), a new extraction waits instead
/// of being refused, and one after it waits behind it. Both are listed as queued in the
/// operation center; Cancel there takes the second out of the line without extracting it, and
/// the first runs once the tab is free.
#[gpui::test]
fn a_second_extraction_waits_in_the_line_and_can_be_cancelled(cx: &mut TestAppContext) {
    let dir = sandbox("line");
    make_zip(&dir.join("one.zip"), "one.txt", "first");
    make_zip(&dir.join("two.zip"), "two.txt", "second");
    let (view, cx) = open(cx, &dir, &["one.zip", "two.zip"]);
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(800.)));
    let busy = OpProgress::default();
    let line = view.update(cx, |v, _| v.archive_line);
    let turn = crate::op_queue::enter_archive(line, &busy).expect("the line is free");

    menu_on(&view, cx, &["one.zip"]);
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    wait_extractions(cx, "one.zip waits", |jobs| {
        jobs.len() == 1 && jobs[0].2 && jobs[0].0.contains("one.zip")
    });
    menu_on(&view, cx, &["two.zip"]);
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    let jobs = extractions(cx);
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs[1].0.contains("two.zip") && jobs[1].2, "{jobs:?}");
    assert_eq!(view.update(cx, |v, _| v.extract_jobs.len()), 2);
    assert_eq!(
        view.update(cx, |v, _| v.notice.clone()),
        None,
        "not refused"
    );

    // The list shows both as queued, newest first, each with its Cancel.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let button = cx
        .debug_bounds("op-center")
        .expect("the operation center button is drawn");
    cx.simulate_click(button.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.ops_open), "the list is open");
    let details: Vec<String> =
        cx.update(|_, cx| OpCenter::jobs(cx).iter().map(Job::detail).collect());
    assert!(
        details.iter().all(|d| *d == t!("ops.state_queued")),
        "{details:?}"
    );
    let cancel = cx
        .debug_bounds("op-center-action-0")
        .expect("the newest row has Cancel");
    cx.simulate_click(cancel.center(), gpui::Modifiers::none());
    wait_extractions(cx, "two.zip leaves the line", |jobs| {
        jobs.len() == 2 && jobs[1].1 == JobState::Cancelled
    });
    wait_until(cx, &view, "one job left in the tab", |v| {
        v.extract_jobs.len() == 1
    });
    assert!(extractions(cx)[0].2, "one.zip still waits");

    drop(turn);
    wait_until(cx, &view, "one.zip extracted", |v| {
        v.extract_jobs.is_empty()
    });
    assert_eq!(
        std::fs::read_to_string(dir.join("one").join("one.txt")).unwrap(),
        "first"
    );
    assert!(!dir.join("two").exists(), "the cancelled one did nothing");
    assert!(matches!(extractions(cx)[0].1, JobState::Done { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A zip at `path` holding `count` small files, `f0.txt`, `f1.txt` and so on.
fn make_zip_many(path: &Path, count: usize) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for n in 0..count {
        zip.start_file(
            format!("f{n}.txt"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"x").unwrap();
    }
    zip.finish().unwrap();
}

/// An archive with many small files: the finished entry counts every one of them, not the last
/// progress report that got through.
#[gpui::test]
fn a_finished_extraction_counts_every_file(cx: &mut TestAppContext) {
    const FILES: usize = 200;
    let dir = sandbox("count");
    make_zip_many(&dir.join("many.zip"), FILES);
    let (view, cx) = open(cx, &dir, &["many.zip"]);

    menu_on(&view, cx, &["many.zip"]);
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    wait_until(cx, &view, "extracted", |v| v.extract_jobs.is_empty());

    assert_eq!(view.update(cx, |v, _| v.error.clone()), None);
    assert_eq!(std::fs::read_dir(dir.join("many")).unwrap().count(), FILES);
    let (done, detail) = cx.update(|_, cx| {
        let jobs = OpCenter::jobs(cx);
        let job = jobs.iter().find(|job| job.verb == "op.extract").unwrap();
        (matches!(job.state, JobState::Done { .. }), job.done_items())
    });
    assert!(done);
    assert_eq!(detail, FILES as u64);
    let _ = std::fs::remove_dir_all(&dir);
}
