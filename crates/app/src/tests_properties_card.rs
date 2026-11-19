//! GPUI tests for the cards of the Properties window: where the space goes, the checksum, unblock,
//! the quick actions, the picture at the top, and the window's size and place.

use arke_core::dirsize::Category;
use gpui::{px, size};

use super::properties_window::{PLACEHOLDER, at, click, file_fs, props, ready, services, wait_for};
use super::*;
use crate::properties::HashState;
use crate::settings::{PropertiesPlace, SettingsStore};

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn wait_shell(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// What the space card draws: bytes per category (biggest first), the three biggest folders inside
/// and the bar itself; the summary under the name says what is in the folder.
#[gpui::test]
fn the_space_card_shows_where_the_space_goes(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\w",
        vec![
            dir("media"),
            dir("src"),
            dir("docs"),
            dir("tiny"),
            file("top.txt", 5),
        ],
    );
    fs.insert_dir(
        r"C:\w\media",
        vec![file("a.mp4", 9000), file("b.jpg", 1000)],
    );
    fs.insert_dir(r"C:\w\src", vec![file("a.rs", 300)]);
    fs.insert_dir(r"C:\w\docs", vec![file("a.pdf", 200)]);
    fs.insert_dir(r"C:\w\tiny", vec![file("x.bin", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\w", true)]);
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());

    let space = view
        .update(cx, |v, _| v.space_view())
        .expect("a folder has one");
    assert_eq!(space.total, 10_506);
    assert_eq!(
        space.categories,
        vec![
            (Category::Video, 9000),
            (Category::Images, 1000),
            (Category::Code, 300),
            (Category::Documents, 205),
            (Category::Other, 1),
        ]
    );
    assert_eq!(
        space.largest,
        vec![
            ("media".to_string(), 10_000),
            ("src".into(), 300),
            ("docs".into(), 200)
        ],
        "the three biggest folders inside"
    );
    assert!(cx.debug_bounds("props-space-bar").is_some());
    let summary = view.update(cx, |v, _| v.summary_for_test());
    assert!(summary[0].contains("10 KB"), "{summary:?}");
    assert!(summary[0].contains("6 files, 4 folders"), "{summary:?}");
}

/// A single file has no space card; several items have one, listing the folders picked.
#[gpui::test]
fn only_folders_and_several_items_get_a_space_card(cx: &mut TestAppContext) {
    let fs = file_fs();
    fs.insert_dir(r"C:\p\sub", vec![file("m.mp4", 4000)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());
    assert!(view.update(cx, |v, _| v.space_view()).is_none());
    assert!(cx.debug_bounds("props-space-bar").is_none());
}

/// Windows gives an uncompressed file's own size as what it stores: on disk it takes whole
/// clusters, as File Explorer shows (2,402 bytes take 4 KB); a compressed file its stored bytes,
/// rounded the same way.
#[test]
fn size_on_disk_counts_whole_clusters() {
    let on_disk = |size, stored| {
        crate::properties::on_disk_of(
            &file("a.txt", size),
            crate::shell::FileDetails {
                accessed: None,
                size_on_disk: stored,
            },
            4096,
        )
    };
    assert_eq!(on_disk(2402, Some(2402)), 4096);
    assert_eq!(on_disk(100_000, Some(5000)), 8192);
    assert_eq!(on_disk(8192, Some(8192)), 8192);
    assert_eq!(on_disk(2402, None), 4096);
}

/// An empty folder, once counted, has nothing to break down: no space card, rather than an empty
/// bar that looks as if it were still loading.
#[gpui::test]
fn an_empty_folder_gets_no_space_card(cx: &mut TestAppContext) {
    let fs = file_fs();
    fs.insert_dir(r"C:\p\empty", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\empty", true)]);
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());
    assert!(view.update(cx, |v, _| v.space_view()).is_none());
    assert!(cx.debug_bounds("props-space-bar").is_none());
}

#[gpui::test]
fn several_items_share_one_space_card(cx: &mut TestAppContext) {
    let fs = file_fs();
    fs.insert_dir(r"C:\p\sub", vec![file("m.mp4", 4000)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(
        cx,
        &fs,
        &shell,
        &[(r"C:\p\a.txt", false), (r"C:\p\sub", true)],
    );
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());
    let space = view.update(cx, |v, _| v.space_view()).unwrap();
    assert_eq!(space.total, 9000);
    assert_eq!(space.largest, vec![("sub".to_string(), 4000)]);
    let summary = view.update(cx, |v, _| v.summary_for_test());
    assert!(summary[0].starts_with("2 items"), "{summary:?}");
}

/// The checksum is only worked out when asked for, shown in full and copyable.
#[gpui::test]
fn the_checksum_is_calculated_on_click_and_copied(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\h", vec![file("abc.txt", 3)]);
    fs.set_content(r"C:\h\abc.txt", b"abc".to_vec());
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\h\abc.txt", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| v.hash_for_test().clone()),
        HashState::Idle,
        "nothing is read until asked"
    );

    click(cx, "props-hash");
    let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    wait_for(cx, &view, "the checksum", |v| {
        matches!(v.hash_for_test(), HashState::Done(_))
    });
    assert_eq!(
        view.update(cx, |v, _| v.hash_for_test().clone()),
        HashState::Done(want.into())
    );
    click(cx, "props-hash-copy");
    wait_shell("the copy", || shell.clipboard_text.lock().is_some());
    assert_eq!(shell.clipboard_text.lock().as_deref(), Some(want));
}

/// A checksum under way can be cancelled, and nothing arrives afterwards.
#[gpui::test]
fn a_checksum_can_be_cancelled(cx: &mut TestAppContext) {
    let big = 48 * 1024 * 1024;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\h", vec![file("big.bin", big as u64)]);
    fs.set_content(r"C:\h\big.bin", vec![7u8; big]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\h\big.bin", false)]);
    ready(cx, &view);
    click(cx, "props-hash");
    assert!(matches!(
        view.update(cx, |v, _| v.hash_for_test().clone()),
        HashState::Running(_)
    ));
    click(cx, "props-hash-cancel");
    assert_eq!(
        view.update(cx, |v, _| v.hash_for_test().clone()),
        HashState::Idle
    );
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| v.hash_for_test().clone()),
        HashState::Idle,
        "what the thread had left is dropped"
    );
    assert!(
        cx.debug_bounds("props-hash").is_some(),
        "can be asked again"
    );
}

/// A cloud file that is not on this device is never read for a checksum.
#[gpui::test]
fn a_cloud_file_has_no_checksum_button(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\h",
        vec![Entry::new("c.bin", EntryKind::File, 10, None, PLACEHOLDER)],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\h\c.bin", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    assert!(cx.debug_bounds("props-hash").is_none());
}

/// The internet mark is found on files that carry it, offered for removal and removed; files
/// without it, and cloud files, show nothing.
#[gpui::test]
fn a_downloaded_file_can_be_unblocked(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    shell.blocked.lock().push(PathBuf::from(r"C:\p\a.txt"));
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.blocked_for_test()));
    click(cx, "props-unblock");
    wait_for(cx, &view, "the mark to go", |v| !v.blocked_for_test());
    assert_eq!(*shell.unblocked.lock(), vec![PathBuf::from(r"C:\p\a.txt")]);
    cx.run_until_parked();
    assert!(cx.debug_bounds("props-unblock").is_none());
}

#[gpui::test]
fn a_file_without_the_mark_shows_no_unblock(cx: &mut TestAppContext) {
    let fs = file_fs();
    fs.add_entry(
        Path::new(r"C:\p"),
        Entry::new("cloud.txt", EntryKind::File, 10, None, PLACEHOLDER),
    );
    let shell = Arc::new(FakeShell::default());
    shell.blocked.lock().push(PathBuf::from(r"C:\p\cloud.txt"));
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    assert!(!view.update(cx, |v, _| v.blocked_for_test()));
    assert!(cx.debug_bounds("props-unblock").is_none());
}

/// A cloud file is never even looked at for the mark.
#[gpui::test]
fn a_cloud_file_is_not_checked_for_the_mark(cx: &mut TestAppContext) {
    let fs = file_fs();
    fs.add_entry(
        Path::new(r"C:\p"),
        Entry::new("cloud.txt", EntryKind::File, 10, None, PLACEHOLDER),
    );
    let shell = Arc::new(FakeShell::default());
    shell.blocked.lock().push(PathBuf::from(r"C:\p\cloud.txt"));
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\cloud.txt", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    assert!(!view.update(cx, |v, _| v.blocked_for_test()));
}

/// The quick row for a file: Open runs the program, Copy path puts the quoted path on the
/// clipboard.
#[gpui::test]
fn quick_actions_for_a_file(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    for id in [
        "props-act-open",
        "props-act-with",
        "props-act-copy",
        "props-act-show",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "{id}");
    }
    assert!(cx.debug_bounds("props-act-fav").is_none());
    assert!(cx.debug_bounds("props-act-tab").is_none());

    click(cx, "props-act-open");
    wait_shell("open", || !shell.opened.lock().is_empty());
    assert_eq!(*shell.opened.lock(), vec![PathBuf::from(r"C:\p\a.txt")]);
    click(cx, "props-act-copy");
    wait_shell("copy", || shell.clipboard_text.lock().is_some());
    assert_eq!(
        shell.clipboard_text.lock().as_deref(),
        Some("\"C:\\p\\a.txt\"")
    );
    click(cx, "props-act-with");
    wait_shell("choose app", || !shell.chose_app.lock().is_empty());
}

/// The quick row for a folder has Open in new tab and favorites (and takes it back); a drive
/// only gets Open and Copy path.
#[gpui::test]
fn quick_actions_for_a_folder_and_a_drive(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\sub", true)]);
    ready(cx, &view);
    for id in [
        "props-act-open",
        "props-act-tab",
        "props-act-copy",
        "props-act-fav",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "{id}");
    }
    assert!(cx.debug_bounds("props-act-show").is_none());
    let favorites = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            SettingsStore::global(cx)
                .read(cx)
                .settings
                .favorites
                .clone()
                .unwrap_or_default()
        })
    };
    click(cx, "props-act-fav");
    assert!(favorites(cx).contains(&PathBuf::from(r"C:\p\sub")));
    click(cx, "props-act-fav");
    assert!(
        !favorites(cx).contains(&PathBuf::from(r"C:\p\sub")),
        "and back"
    );
}

#[gpui::test]
fn a_drive_has_only_open_and_copy(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\", true)]);
    ready(cx, &view);
    assert!(cx.debug_bounds("props-act-open").is_some());
    assert!(cx.debug_bounds("props-act-copy").is_some());
    for id in [
        "props-act-tab",
        "props-act-fav",
        "props-act-with",
        "props-act-show",
    ] {
        assert!(cx.debug_bounds(id).is_none(), "{id}");
    }
}

/// Open, Open in new tab and Show in folder go to a window of Arke.
#[gpui::test]
fn quick_actions_show_things_in_arke(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    for dir in [r"C:\a", r"C:\b", r"C:\c"] {
        fs.insert_many(dir, 1);
    }
    fs.insert_dir(r"C:\root", vec![dir("b"), dir("c")]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\a");
    cx.run_until_parked();
    let services = services(&fs, &shell);
    let window = cx
        .update(|_, cx| crate::properties::open(services, vec![(PathBuf::from(r"C:\b"), true)], cx))
        .expect("the window opens");
    cx.run_until_parked();
    wait_for_tab(cx, &ws, r"C:\a");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !window.read_with(cx, |p, _| p.is_loaded_for_test()).unwrap() {
        cx.run_until_parked();
        assert!(Instant::now() < deadline, "the window never loaded");
        std::thread::sleep(Duration::from_millis(5));
    }
    window.update(cx, |p, _, cx| p.open_in_new_tab(cx)).unwrap();
    cx.run_until_parked();
    let tabs = ws.update(cx, |ws, cx| tab_paths(ws, cx));
    assert!(tabs.contains(&Some(PathBuf::from(r"C:\b"))), "{tabs:?}");

    window.update(cx, |p, _, cx| p.open_item(cx)).unwrap();
    cx.run_until_parked();
    let tabs = ws.update(cx, |ws, cx| tab_paths(ws, cx));
    assert_eq!(
        tabs.iter()
            .filter(|t| **t == Some(PathBuf::from(r"C:\b")))
            .count(),
        1,
        "the folder is already open: it comes forward"
    );
}

fn wait_for_tab(cx: &mut VisualTestContext, ws: &Entity<Workspace>, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if ws
            .update(cx, |ws, cx| tab_paths(ws, cx))
            .contains(&Some(PathBuf::from(path)))
        {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the tab");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A photo is the picture at the top, with its size in the summary; a video shows a frame; a
/// cloud file that is not on this device only gets its icon (nothing of it is opened).
#[gpui::test]
fn the_picture_at_the_top(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let bytes = png(8, 6);
    fs.insert_dir(
        r"C:\i",
        vec![
            Entry::new("p.png", EntryKind::File, bytes.len() as u64, Some(at(1)), 0),
            Entry::new("clip.mp4", EntryKind::File, 5000, Some(at(1)), 0),
            Entry::new("cloud.png", EntryKind::File, 5000, Some(at(1)), PLACEHOLDER),
            Entry::new("cloud.mp4", EntryKind::File, 5000, Some(at(1)), PLACEHOLDER),
        ],
    );
    fs.set_content(r"C:\i\p.png", bytes);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\i\p.png", false)]);
    ready(cx, &view);
    assert_eq!(view.update(cx, |v, _| v.picture_for_test()), (true, true));
    let summary = view.update(cx, |v, _| v.summary_for_test());
    assert!(summary[0].contains("8 × 6"), "{summary:?}");
    assert!(cx.debug_bounds("props-hero").is_some());
}

#[gpui::test]
fn a_video_shows_a_frame(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\i",
        vec![Entry::new(
            "clip.mp4",
            EntryKind::File,
            5000,
            Some(at(1)),
            0,
        )],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\i\clip.mp4", false)]);
    ready(cx, &view);
    assert_eq!(view.update(cx, |v, _| v.picture_for_test()), (true, true));
    assert_eq!(
        *shell.video_thumbs.lock(),
        vec![PathBuf::from(r"C:\i\clip.mp4")]
    );
}

#[gpui::test]
fn cloud_files_are_never_opened_for_the_picture(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\i",
        vec![
            Entry::new("cloud.png", EntryKind::File, 5000, Some(at(1)), PLACEHOLDER),
            Entry::new("cloud.mp4", EntryKind::File, 5000, Some(at(1)), PLACEHOLDER),
        ],
    );
    fs.set_content(r"C:\i\cloud.png", png(4, 4));
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\i\cloud.png", false)]);
    ready(cx, &view);
    assert_eq!(
        view.update(cx, |v, _| v.picture_for_test()),
        (false, false),
        "the big kind icon instead"
    );
}

#[gpui::test]
fn a_cloud_video_gets_no_frame(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\i",
        vec![Entry::new(
            "cloud.mp4",
            EntryKind::File,
            5000,
            Some(at(1)),
            PLACEHOLDER,
        )],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\i\cloud.mp4", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    assert!(shell.video_thumbs.lock().is_empty());
    assert_eq!(view.update(cx, |v, _| v.picture_for_test()), (false, false));
}

/// With "apply to items inside" on, an attribute reaches everything in a folder; without it only
/// the folder is touched.
#[gpui::test]
fn attributes_can_reach_the_items_inside(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\r", vec![dir("in"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\r\in", vec![file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\r", true)]);
    ready(cx, &view);
    click(cx, "props-hidden");
    wait_shell("the first change", || {
        !shell.attribute_calls.lock().is_empty()
    });
    assert_eq!(
        shell.attribute_calls.lock()[0].0,
        vec![PathBuf::from(r"C:\r")],
        "only the folder"
    );

    click(cx, "props-recursive");
    click(cx, "props-hidden");
    wait_shell("the second change", || {
        shell.attribute_calls.lock().len() == 2
    });
    let mut paths = shell.attribute_calls.lock()[1].0.clone();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            PathBuf::from(r"C:\r"),
            PathBuf::from(r"C:\r\a.txt"),
            PathBuf::from(r"C:\r\in"),
            PathBuf::from(r"C:\r\in\b.txt"),
        ]
    );
}

/// The window sizes itself to what it shows, and remembers where it was.
#[gpui::test]
fn the_window_fits_its_content_and_remembers_its_place(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    cx.run_until_parked();
    let (height, user) = view.update(cx, |v, _| v.height_for_test());
    // As tall as what it shows: no longer capped below the screen's height.
    assert!((440.0..=760.0).contains(&height), "fits: {height}");
    assert!(!user);

    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.properties_window = Some(PropertiesPlace {
                        x: 100.,
                        y: 80.,
                        width: 800.,
                        height: None,
                    });
                },
                cx,
            );
        });
    });
    let bounds = cx.update(|_, cx| crate::properties::start_bounds(cx));
    assert_eq!(
        (
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width)
        ),
        (100., 80., 800.)
    );
    // A width from the old, narrow window would give a single column: the new default is used.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.properties_window = Some(PropertiesPlace {
                        x: 100.,
                        y: 80.,
                        width: 500.,
                        height: None,
                    });
                },
                cx,
            );
        });
    });
    let bounds = cx.update(|_, cx| crate::properties::start_bounds(cx));
    assert_eq!(f32::from(bounds.size.width), 760.);
    assert_eq!(f32::from(bounds.size.height), 520.);
    // A place that is on no screen any more is not used.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.properties_window = Some(PropertiesPlace {
                        x: 90_000.,
                        y: 90_000.,
                        width: 800.,
                        height: Some(700.),
                    });
                },
                cx,
            );
        });
    });
    let bounds = cx.update(|_, cx| crate::properties::start_bounds(cx));
    assert!(f32::from(bounds.origin.x) < 5_000., "centered instead");
    assert_eq!(
        f32::from(bounds.size.height),
        700.,
        "a height the user chose is kept"
    );
}

/// A wide window puts the cards in two columns side by side, a narrow one in a single column.
#[gpui::test]
fn wide_windows_use_two_columns_and_narrow_ones_one(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\w", vec![file("a.txt", 5)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\w", true)]);
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());

    cx.simulate_resize(size(px(760.), px(520.)));
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.columns_for_test()), 2);
    let left = cx.debug_bounds("props-col-left").expect("left column");
    let right = cx.debug_bounds("props-col-right").expect("right column");
    assert!(left.right() <= right.left(), "side by side");
    assert_eq!(left.top(), right.top(), "cards start at a common top");
    assert!(
        (left.size.width - right.size.width).abs() < px(1.),
        "equal columns"
    );

    cx.simulate_resize(size(px(640.), px(520.)));
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.columns_for_test()), 1);
    let left = cx.debug_bounds("props-col-left").expect("left column");
    assert!(cx.debug_bounds("props-col-right").is_none());
    assert!(
        left.size.width > px(500.),
        "one column takes the width: {:?}",
        left.size.width
    );
    // The quick actions are part of the hero, not a row of their own below it.
    let hero = cx.debug_bounds("props-hero").expect("hero");
    let open = cx.debug_bounds("props-act-open").expect("open action");
    assert!(open.left() >= hero.right(), "next to the picture");
    assert!(open.top() < hero.bottom() + px(40.));
}

/// Without a space card (a file) the attributes sit under the times, so the columns stay level.
#[gpui::test]
fn the_attributes_sit_under_the_times_when_there_is_no_space_card(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    cx.simulate_resize(size(px(760.), px(520.)));
    cx.run_until_parked();
    let right = cx.debug_bounds("props-col-right").expect("right column");
    let modified = cx.debug_bounds("props-modified").expect("times");
    let readonly = cx.debug_bounds("props-readonly").expect("attributes");
    assert!(readonly.left() >= right.left(), "on the right");
    assert!(readonly.top() > modified.top(), "under the times");
}
