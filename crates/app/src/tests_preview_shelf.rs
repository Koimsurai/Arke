//! GPUI tests for PDF and Office previews, cloud sync states and the shelf.

use super::*;
use crate::explorer::{PreviewBody, ShelfDrag};
use crate::shelf::ShelfStore;
use arke_core::entry::CloudSync;
use arke_core::filetype::FileKind;
use std::sync::Arc;

/// Page and page count of the PDF on screen.
fn pdf_page(v: &Explorer) -> Option<(u32, u32)> {
    match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Pdf(view)) => Some((view.page, view.render.pages)),
        _ => None,
    }
}

fn wait_page(cx: &mut VisualTestContext, view: &Entity<Explorer>, page: u32) {
    wait_until(cx, view, &format!("page {page}"), |v| {
        pdf_page(v) == Some((page, 5))
    });
}

#[gpui::test]
fn quick_look_turns_pdf_pages(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.pdf", 100), file("b.txt", 5)]);
    fs.set_content(Path::new(r"C:\p\b.txt"), b"hello".to_vec());
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_keystrokes("down space");
    assert!(view.update(cx, |v, _| v.quick_look));
    wait_page(cx, &view, 0);
    // Left/Right, Page Up/Down, Home/End turn pages and stop at the ends.
    cx.simulate_keystrokes("right");
    wait_page(cx, &view, 1);
    cx.simulate_keystrokes("pagedown");
    wait_page(cx, &view, 2);
    cx.simulate_keystrokes("end");
    wait_page(cx, &view, 4);
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| pdf_page(v)), Some((4, 5)));
    cx.simulate_keystrokes("home");
    wait_page(cx, &view, 0);
    cx.simulate_keystrokes("left pageup");
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| pdf_page(v)), Some((0, 5)));
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["a.pdf"]);
    // Every page was rendered off the UI thread for Quick Look's box; page 0 came from the cache
    // the second time.
    let renders = shell.pdf_renders.lock().clone();
    let pages: Vec<u32> = renders.iter().map(|(_, page, _)| *page).collect();
    assert_eq!(pages, [0, 1, 2, 4]);
    assert!(
        renders
            .iter()
            .all(|(path, _, (w, h))| { path == Path::new(r"C:\p\a.pdf") && *w > 100 && *h > 100 })
    );
    // Up and Down still move between files.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "text preview", |v| {
        v.preview
            .as_ref()
            .is_some_and(|p| p.entry.name.as_ref() == "b.txt")
    });
    cx.simulate_keystrokes("right");
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["b.txt"]);
    cx.simulate_keystrokes("up");
    wait_page(cx, &view, 0);
    assert_eq!(
        shell.pdf_renders.lock().len(),
        4,
        "the first page was cached"
    );
}

/// An online-only PDF is never rendered (that would download it).
#[gpui::test]
fn online_only_pdfs_are_not_rendered(cx: &mut TestAppContext) {
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\p",
        vec![Entry::new(
            "cloud.pdf",
            EntryKind::File,
            100,
            None,
            RECALL_ON_DATA_ACCESS,
        )],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_keystrokes("down space");
    wait_until(cx, &view, "cloud body", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Cloud)
        )
    });
    assert!(shell.pdf_renders.lock().is_empty());
}

/// Sync states come from the shell's scan (placeholders not in sync) and from attributes (plain
/// files inside a sync root). The folder counts as a cloud folder even though no sidebar place
/// lists it.
#[gpui::test]
fn cloud_states_come_from_the_scan(cx: &mut TestAppContext) {
    const REPARSE: u32 = 0x400;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\sync",
        vec![
            Entry::new("busy.txt", EntryKind::File, 1, None, REPARSE),
            Entry::new("done.txt", EntryKind::File, 1, None, REPARSE),
            Entry::new(
                "online.txt",
                EntryKind::File,
                1,
                None,
                REPARSE | RECALL_ON_DATA_ACCESS,
            ),
            Entry::new("fresh.txt", EntryKind::File, 1, None, 0),
        ],
    );
    fs.insert_dir(r"C:\plain", vec![file("x.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    *shell.cloud.lock() = crate::shell::CloudScan {
        sync_root: true,
        syncing: vec![Arc::from("busy.txt")],
        not_synced: Vec::new(),
    };
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\sync");
    wait_until(cx, &view, "load", loaded(r"C:\sync"));
    wait_until(cx, &view, "states", |v| {
        v.cloud.sync_of("busy.txt") == Some(CloudSync::Syncing)
    });
    view.update(cx, |v, _| {
        assert_eq!(v.cloud.sync_of("fresh.txt"), Some(CloudSync::NotSynced));
        assert_eq!(v.cloud.sync_of("done.txt"), None);
        assert_eq!(v.cloud.sync_of("online.txt"), None);
        assert!(v.in_cloud(Path::new(r"C:\sync\done.txt")));
        assert!(!v.in_cloud(Path::new(r"C:\plain\x.txt")));
    });
    // Only local placeholders were opened; the online-only one was left alone.
    let scans = shell.cloud_scans.lock().clone();
    assert_eq!(scans[0].0, PathBuf::from(r"C:\sync"));
    let mut items: Vec<String> = scans[0].1.iter().map(ToString::to_string).collect();
    items.sort();
    assert_eq!(items, ["busy.txt", "done.txt"]);

    // Another folder outside any sync root: no states.
    *shell.cloud.lock() = crate::shell::CloudScan::default();
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\plain"), NavKind::New, cx);
    });
    wait_until(cx, &view, "plain", loaded(r"C:\plain"));
    wait_shell("second scan", || shell.cloud_scans.lock().len() == 2);
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.cloud.sync_of("x.txt")), None);
}

fn shelf_paths(cx: &mut VisualTestContext) -> Vec<(PathBuf, bool, bool)> {
    cx.update(|_, cx| {
        ShelfStore::global(cx)
            .read(cx)
            .shelf()
            .items()
            .iter()
            .map(|i| (i.path.clone(), i.is_dir, i.missing))
            .collect()
    })
}

#[gpui::test]
fn shelf_collects_items_and_copies_or_moves_them(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\one", vec![file("a.txt", 1), file("b.txt", 1)]);
    fs.insert_dir(r"C:\two", vec![dir("sub"), file("c.txt", 1)]);
    fs.insert_dir(r"C:\two\sub", vec![]);
    fs.insert_dir(r"C:\dest", vec![]);
    let shell = Arc::new(FakeShell::default());
    // A move takes the items away from their folders.
    let moved_fs = Arc::clone(&fs);
    *shell.on_op.lock() = Some(Box::new(move |op| {
        if let FileOp::Move { sources, .. } = op {
            for p in sources {
                moved_fs.remove(
                    p.parent().unwrap(),
                    p.file_name().unwrap().to_str().unwrap(),
                );
            }
        }
    }));
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\one");
    wait_until(cx, &view, "load", loaded(r"C:\one"));

    // Ctrl+Shift+A adds the selection; adding again says it's already there.
    cx.simulate_keystrokes("down ctrl-shift-a");
    assert_eq!(
        shelf_paths(cx),
        [(PathBuf::from(r"C:\one\a.txt"), false, false)]
    );
    assert_eq!(
        view.update(cx, |v, _| v.notice.clone()),
        Some(tn!("shelf.added", 1).into_owned())
    );
    cx.simulate_keystrokes("ctrl-shift-a");
    assert_eq!(
        view.update(cx, |v, _| v.notice.clone()),
        Some(tn!("shelf.already", 1).into_owned())
    );

    // Items from another folder join, folders included.
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\two"), NavKind::New, cx);
    });
    wait_until(cx, &view, "two", loaded(r"C:\two"));
    cx.simulate_keystrokes("ctrl-a ctrl-shift-a");
    assert_eq!(
        shelf_paths(cx),
        [
            (PathBuf::from(r"C:\one\a.txt"), false, false),
            (PathBuf::from(r"C:\two\sub"), true, false),
            (PathBuf::from(r"C:\two\c.txt"), false, false),
        ]
    );

    // "Copy here" copies them all into the current folder and keeps them on the shelf.
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\dest"), NavKind::New, cx);
    });
    wait_until(cx, &view, "dest", loaded(r"C:\dest"));
    view.update(cx, |v, cx| v.shelf_to_here(false, cx));
    wait_shell("copy", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops()[0],
        FileOp::Copy {
            sources: vec![
                PathBuf::from(r"C:\one\a.txt"),
                PathBuf::from(r"C:\two\sub"),
                PathBuf::from(r"C:\two\c.txt"),
            ],
            dest: PathBuf::from(r"C:\dest"),
        }
    );
    cx.run_until_parked();
    assert_eq!(shelf_paths(cx).len(), 3);

    // Dragging a shelf item out copies even on the same drive; Shift would move.
    let dragged = crate::explorer::DraggedFiles {
        source: view.downgrade(),
        ix: 0,
        shelf: Some(ShelfDrag {
            paths: Arc::new(vec![PathBuf::from(r"C:\one\a.txt")]),
            kind: FileKind::Text,
        }),
    };
    cx.update(|window, cx| {
        crate::explorer::drop_into(&dragged, PathBuf::from(r"C:\two\sub"), window, cx);
    });
    wait_shell("drag copy", || shell.ops().len() == 2);
    assert_eq!(
        shell.ops()[1],
        FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\one\a.txt")],
            dest: PathBuf::from(r"C:\two\sub"),
        }
    );

    // "Move here" moves them; once they're gone from their folders they leave the shelf.
    view.update(cx, |v, cx| v.shelf_to_here(true, cx));
    wait_shell("move", || shell.ops().len() == 3);
    assert!(matches!(shell.ops()[2], FileOp::Move { .. }));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !shelf_paths(cx).is_empty() {
        assert!(Instant::now() < deadline, "moved items still on the shelf");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Items that disappear stay on the shelf, marked missing, and aren't copied.
#[gpui::test]
fn missing_shelf_items_are_marked_and_skipped(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\one", vec![file("a.txt", 1), file("b.txt", 1)]);
    fs.insert_dir(r"C:\dest", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\one");
    wait_until(cx, &view, "load", loaded(r"C:\one"));
    cx.simulate_keystrokes("ctrl-a ctrl-shift-a");
    assert_eq!(shelf_paths(cx).len(), 2);
    fs.remove(Path::new(r"C:\one"), "b.txt");
    view.update(cx, |v, cx| {
        let fs: Arc<dyn arke_core::Fs> = Arc::clone(&v.services.fs);
        ShelfStore::global(cx).update(cx, |s, cx| s.recheck(fs, cx));
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !shelf_paths(cx).iter().any(|(_, _, missing)| *missing) {
        assert!(Instant::now() < deadline, "not marked missing");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        shelf_paths(cx),
        [
            (PathBuf::from(r"C:\one\a.txt"), false, false),
            (PathBuf::from(r"C:\one\b.txt"), false, true),
        ]
    );
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\dest"), NavKind::New, cx);
    });
    wait_until(cx, &view, "dest", loaded(r"C:\dest"));
    view.update(cx, |v, cx| v.shelf_to_here(false, cx));
    wait_shell("copy", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops()[0],
        FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\one\a.txt")],
            dest: PathBuf::from(r"C:\dest"),
        }
    );
}

/// Dragging the preview pane's left edge resizes it (and its PDF page) and saves the width for
/// every window; double clicking the edge goes back to the automatic width.
#[gpui::test]
fn dragging_the_preview_edge_resizes_and_saves_it(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.pdf", 100)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1500.), px(800.)));
    // Through settings, like the toolbar button: saving the width writes settings too, which
    // would turn a preview switched on only in the view back off.
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("down");
    wait_page(cx, &view, 0);
    // The pane's width, checking it is on screen.
    let width = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        let (shown, width) = view.update(cx, |v, _| v.fit_for_test());
        assert!(shown, "the preview pane is shown");
        px(width)
    };
    let saved = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            crate::settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .preview_width
        })
    };
    // Automatic: 22 % of the window.
    assert_eq!(width(&view, cx), px(330.));
    let first = shell.pdf_renders.lock().last().map(|(_, _, size)| *size);

    // The edge is where the list ends. Grab it 3 px in and drag 100 px to the left.
    let edge = view.update(cx, |v, _| v.list_bounds_for_test()).right();
    let y = px(400.);
    let grab = point(edge + px(3.), y);
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(grab.x - px(20.), y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(
        point(grab.x - px(100.), y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert_eq!(width(&view, cx), px(430.), "the edge follows the cursor");
    assert_eq!(saved(cx), None, "saved on release, not on every move");
    cx.simulate_mouse_up(
        point(grab.x - px(100.), y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert_eq!(saved(cx), Some(430));
    assert_eq!(crate::prefs::preview_width(), Some(px(430.)));
    // The PDF page is rendered again for the wider frame.
    wait_until(cx, &view, "pdf re-rendered", |_| {
        shell.pdf_renders.lock().last().map(|(_, _, size)| *size) != first
    });
    let wider = shell.pdf_renders.lock().last().map(|(_, _, size)| size.0);
    assert!(wider > first.map(|size| size.0), "{first:?} → {wider:?}");

    // Way past the list's minimum: the list keeps its room.
    let edge = view.update(cx, |v, _| v.list_bounds_for_test()).right();
    let grab = point(edge + px(3.), y);
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(grab.x - px(20.), y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(point(px(10.), y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(px(10.), y), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let room = view.update(cx, |v, _| v.preview_room_for_test());
    assert_eq!(width(&view, cx), px(room));
    assert!(room < 960. && room > 600., "{room}");

    // Double click on the edge: automatic again.
    let edge = view.update(cx, |v, _| v.list_bounds_for_test()).right();
    let at = point(edge + px(3.), y);
    for click_count in [1, 2] {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count,
        });
    }
    cx.run_until_parked();
    assert_eq!(saved(cx), None);
    assert_eq!(width(&view, cx), px(330.));
}

/// Slide on screen and slide count in Quick Look.
fn slide(v: &Explorer) -> Option<(usize, usize)> {
    match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Slides(view)) => Some((view.page, view.deck.slides.len())),
        _ => None,
    }
}

/// Office files are parsed off the UI thread: slides turn with the page keys, workbooks switch
/// sheets, documents show their blocks, and protected or old binary files don't pretend.
#[gpui::test]
fn quick_look_previews_office_files(cx: &mut TestAppContext) {
    use arke_core::office::fixtures;
    let fs = FakeFs::new();
    let deck = fixtures::pptx(&[("One", &["a"]), ("Two", &[]), ("Three", &["b"])], None);
    let book = fixtures::xlsx(
        &[
            ("Data", r#"<row r="1"><c r="A1"><v>1</v></c></row>"#),
            ("More", r#"<row r="2"><c r="B2"><v>2</v></c></row>"#),
        ],
        &[],
        "",
    );
    let doc = fixtures::docx(
        r#"<w:p><w:pPr><w:pStyle w:val="h1"/></w:pPr><w:r><w:t>Hi</w:t></w:r></w:p>"#,
    );
    let mut locked = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    locked.resize(1024, 0);
    let files = [
        ("a.pptx", deck),
        ("b.xlsx", book),
        ("c.docx", doc),
        ("d.docx", locked),
        ("e.doc", b"old".to_vec()),
    ];
    fs.insert_dir(
        r"C:\o",
        files
            .iter()
            .map(|(name, bytes)| file(name, bytes.len() as u64))
            .collect(),
    );
    for (name, bytes) in files {
        fs.set_content(Path::new(r"C:\o").join(name), bytes);
    }
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell, r"C:\o");
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.simulate_keystrokes("down space");
    wait_until(cx, &view, "slides", |v| slide(v) == Some((0, 3)));
    cx.simulate_keystrokes("right");
    wait_until(cx, &view, "slide 2", |v| slide(v) == Some((1, 3)));
    cx.simulate_keystrokes("end");
    wait_until(cx, &view, "last slide", |v| slide(v) == Some((2, 3)));
    cx.simulate_keystrokes("right home");
    wait_until(cx, &view, "first slide", |v| slide(v) == Some((0, 3)));
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["a.pptx"]);

    cx.simulate_keystrokes("down");
    let sheet = |v: &Explorer| match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Workbook(w)) => Some((w.book.index, w.book.sheet.rows.len())),
        _ => None,
    };
    wait_until(cx, &view, "first sheet", |v| sheet(v) == Some((0, 1)));
    view.update(cx, |v, cx| v.select_sheet(1, cx));
    wait_until(cx, &view, "second sheet", |v| sheet(v) == Some((1, 2)));

    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "document", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Document(doc)) if matches!(
                doc.blocks.first(),
                Some(arke_core::office::Block::Heading { level: 1, .. })
            )
        )
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "protected", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Unreadable(reason)) if reason.contains("password")
        )
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "old binary format", |v| {
        matches!(
            v.preview.as_ref().map(|p| (p.entry.name.as_ref(), &p.body)),
            Some(("e.doc", PreviewBody::Nothing))
        )
    });
}

/// Turns the preview pane on (through settings, like the toolbar button) in a window wide enough
/// for it.
fn show_preview_pane(cx: &mut VisualTestContext) {
    cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(800.)));
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    cx.run_until_parked();
}

/// The preview pane scrolls through every page of a PDF, not just the first: pages render as
/// they come near the view, at the pane's width, and the ones scrolled away are freed.
#[gpui::test]
fn the_pane_scrolls_through_every_pdf_page(cx: &mut TestAppContext) {
    use gpui::{ScrollDelta, ScrollEvent, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.pdf", 100)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    show_preview_pane(cx);
    cx.simulate_keystrokes("down");
    wait_page(cx, &view, 0);
    // The first page and the one after it, for the slot the pane draws them in.
    wait_until(cx, &view, "first pages", |v| {
        v.pane_pdf_pages_for_test()
            .is_some_and(|p| p.contains(&0) && p.contains(&1))
    });
    let pane_w = shell.pdf_renders.lock()[0].2.0;
    let list = cx
        .debug_bounds("pane-pdf-pages")
        .expect("the page list is drawn");
    let wheel = |cx: &mut VisualTestContext, dy: f32| {
        cx.simulate_event(ScrollEvent {
            position: list.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            ..ScrollEvent::default()
        });
        cx.run_until_parked();
    };

    // Down to the end: the last page renders, the first ones are freed.
    wheel(cx, -5000.);
    wait_until(cx, &view, "last page", |v| {
        v.pane_pdf_pages_for_test()
            .is_some_and(|p| p.contains(&4) && p.iter().all(|&page| page >= 2))
    });
    assert!(cx.debug_bounds("pane-pdf-page-5").is_some());
    assert!(cx.debug_bounds("pane-pdf-page-1").is_none());
    let renders = shell.pdf_renders.lock().clone();
    assert!(
        renders.iter().any(|(path, page, (w, _))| {
            path == Path::new(r"C:\p\a.pdf") && *page == 4 && *w == pane_w
        }),
        "{renders:?}"
    );
    // Quick Look's page stays where it was.
    assert_eq!(view.update(cx, |v, _| pdf_page(v)), Some((0, 5)));

    // Back to the top: the first page again, the last ones freed.
    wheel(cx, 5000.);
    wait_until(cx, &view, "first page again", |v| {
        v.pane_pdf_pages_for_test()
            .is_some_and(|p| p.contains(&0) && p.iter().all(|&page| page <= 2))
    });
    assert!(cx.debug_bounds("pane-pdf-page-1").is_some());
}

/// A presentation in the preview pane lists every slide: the embedded thumbnail as the first, the
/// others as their text.
#[gpui::test]
fn the_pane_lists_every_slide(cx: &mut TestAppContext) {
    use arke_core::office::fixtures;
    let thumb = png_bytes(32, 18);
    let decks = [
        (
            "a.pptx",
            fixtures::pptx(
                &[("One", &["a"]), ("Two", &["b"]), ("Three", &["c"])],
                Some(&thumb),
            ),
        ),
        (
            "b.pptx",
            fixtures::pptx(&[("Uno", &[]), ("Dos", &["x"])], None),
        ),
    ];
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\o",
        decks
            .iter()
            .map(|(name, bytes)| file(name, bytes.len() as u64))
            .collect(),
    );
    for (name, bytes) in decks {
        fs.set_content(Path::new(r"C:\o").join(name), bytes);
    }
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\o");
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    show_preview_pane(cx);
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "slides", |v| slide(v) == Some((0, 3)));
    cx.run_until_parked();
    for selector in ["pane-slide-1", "pane-slide-2", "pane-slide-3"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector} is drawn");
    }
    // The thumbnail stands for the first slide; the second is a card under it.
    let first = cx.debug_bounds("pane-slide-1").expect("drawn");
    let second = cx.debug_bounds("pane-slide-2").expect("drawn");
    assert!(second.top() >= first.bottom());
    assert!(view.update(cx, |v, _| matches!(
        v.preview.as_ref().map(|p| &p.body),
        Some(PreviewBody::Slides(s)) if s.has_thumb()
    )));

    // Without a thumbnail every slide is a card.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "second deck", |v| slide(v) == Some((0, 2)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("pane-slide-2").is_some());
    assert!(cx.debug_bounds("pane-slide-3").is_none());
}

/// The pane's scroll position (x, y) and how far it can scroll (x, y).
fn pane_scroll(cx: &mut VisualTestContext, view: &Entity<Explorer>) -> ((f32, f32), (f32, f32)) {
    view.update(cx, |v, _| {
        let (at, max) = v.preview_scroll_for_test();
        (
            (at.x.as_f32(), at.y.as_f32()),
            (max.x.as_f32(), max.y.as_f32()),
        )
    })
}

/// A sheet of `rows` rows and `cols` numeric columns, as `<sheetData>` content.
fn wide_sheet(rows: u32, cols: u32) -> String {
    use std::fmt::Write as _;
    let mut data = String::new();
    for r in 1..=rows {
        let _ = write!(data, r#"<row r="{r}">"#);
        for c in 0..cols {
            let name = arke_core::office::column_name(c);
            let _ = write!(data, r#"<c r="{name}{r}"><v>{}</v></c>"#, r * 1000 + c);
        }
        data.push_str("</row>");
    }
    data
}

/// Shift+wheel (what Windows sends as a horizontal wheel) over `at`.
fn shift_wheel(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>, dx: f32) {
    use gpui::{Modifiers, ScrollDelta, ScrollEvent, point, px};
    cx.simulate_event(ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(dx), px(0.))),
        modifiers: Modifiers::shift(),
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
}

fn plain_wheel(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>, dy: f32) {
    use gpui::{ScrollDelta, ScrollEvent, point, px};
    cx.simulate_event(ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
}

/// A spreadsheet wider than the pane scrolls sideways with Shift+wheel and a sideways swipe,
/// the plain wheel still scrolls it up and down, and its bottom scrollbar can be dragged.
#[gpui::test]
fn a_wide_spreadsheet_scrolls_sideways_in_the_pane(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, ScrollDelta, ScrollEvent, point, px};
    let fs = FakeFs::new();
    let book = arke_core::office::fixtures::xlsx(&[("Data", &wide_sheet(80, 30))], &[], "");
    fs.insert_dir(r"C:\w", vec![file("wide.xlsx", book.len() as u64)]);
    fs.set_content(Path::new(r"C:\w\wide.xlsx"), book);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\w");
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    show_preview_pane(cx);
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "sheet", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Workbook(_))
        )
    });
    cx.run_until_parked();
    let sheet = cx.debug_bounds("preview-sheet").expect("the grid is drawn");
    let at = sheet.center();
    let (start, max) = pane_scroll(cx, &view);
    assert_eq!(start, (0., 0.));
    assert!(
        max.0 > 0. && max.1 > 0.,
        "wider and taller than the pane: {max:?}"
    );

    // The plain wheel goes down, not sideways.
    plain_wheel(cx, at, -100.);
    let (now, _) = pane_scroll(cx, &view);
    assert!(now.1 < 0. && now.0 == 0., "{now:?}");
    // Shift+wheel goes along, not down.
    let down = now.1;
    shift_wheel(cx, at, -100.);
    let (now, _) = pane_scroll(cx, &view);
    assert!(now.0 < 0. && (now.1 - down).abs() < 0.5, "{now:?}");
    // A touchpad's sideways swipe (no modifier) does the same, and swiping back returns.
    let along = now.0;
    cx.simulate_event(ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(-60.), px(0.))),
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
    let (now, _) = pane_scroll(cx, &view);
    assert!(now.0 < along && (now.1 - down).abs() < 0.5, "{now:?}");
    shift_wheel(cx, at, 5000.);
    assert_eq!(pane_scroll(cx, &view).0.0, 0.);

    // Dragging the thumb of the bottom bar moves the columns.
    let grab = point(sheet.left() + px(20.), sheet.bottom() - px(4.));
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        grab + point(px(60.), px(0.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_up(
        grab + point(px(60.), px(0.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.run_until_parked();
    let (now, _) = pane_scroll(cx, &view);
    assert!(
        now.0 < -50.,
        "the thumb dragged right scrolls the columns: {now:?}"
    );
}

/// CSV and TSV keep each row on one line and scroll sideways; other text still wraps.
#[gpui::test]
fn a_wide_csv_scrolls_sideways_while_code_still_wraps(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let row = (0..60)
        .map(|c| format!("column_{c}"))
        .collect::<Vec<_>>()
        .join(",");
    let body = vec![row; 100].join("\n");
    fs.insert_dir(
        r"C:\c",
        vec![
            file("a.csv", body.len() as u64),
            file("b.txt", body.len() as u64),
        ],
    );
    fs.set_content(Path::new(r"C:\c\a.csv"), body.clone().into_bytes());
    fs.set_content(Path::new(r"C:\c\b.txt"), body.into_bytes());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\c");
    wait_until(cx, &view, "load", loaded(r"C:\c"));
    show_preview_pane(cx);
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "csv", |v| {
        v.preview.as_ref().is_some_and(|p| {
            p.entry.name.as_ref() == "a.csv" && matches!(p.body, PreviewBody::Text { .. })
        })
    });
    cx.run_until_parked();
    let at = cx
        .debug_bounds("preview-text")
        .expect("text is drawn")
        .center();
    let (_, max) = pane_scroll(cx, &view);
    assert!(max.0 > 0. && max.1 > 0., "{max:?}");
    plain_wheel(cx, at, -100.);
    let (now, _) = pane_scroll(cx, &view);
    assert!(now.1 < 0. && now.0 == 0., "{now:?}");
    shift_wheel(cx, at, -100.);
    let (now, _) = pane_scroll(cx, &view);
    assert!(now.0 < 0., "{now:?}");

    // The same lines in a .txt wrap: nothing to scroll sideways.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "txt", |v| {
        v.preview.as_ref().is_some_and(|p| {
            p.entry.name.as_ref() == "b.txt" && matches!(p.body, PreviewBody::Text { .. })
        })
    });
    cx.run_until_parked();
    let (_, max) = pane_scroll(cx, &view);
    assert_eq!(max.0, 0., "{max:?}");
    assert!(max.1 > 0.);
}
