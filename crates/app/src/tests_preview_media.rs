//! GPUI tests for the preview and thumbnail additions of 0.8: PDF first pages as thumbnails, SVG
//! and plain `.gz` previews, and the draggable boundary between the preview and its details.

use gpui::{Modifiers, MouseButton, point, px};

use super::*;
use crate::explorer::PreviewBody;
use crate::settings::SettingsStore;

fn previewed(v: &Explorer) -> bool {
    v.preview
        .as_ref()
        .is_some_and(|p| !matches!(p.body, PreviewBody::Loading(_)))
}

/// Shows the side preview pane at a fixed width in a window of the given size.
fn open_pane(view: &Entity<Explorer>, cx: &mut VisualTestContext, window: (f32, f32)) {
    cx.simulate_resize(gpui::size(px(window.0), px(window.1)));
    crate::prefs::set_preview_width(Some(px(360.)));
    // In the settings too: saving anything there (the boundary, say) syncs the pane from them.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
}

/// Redraws until `done` holds, for results that arrive from background threads.
fn settle(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(cx) {
        assert!(Instant::now() < deadline, "{what}");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Grid view: a PDF's first page is its thumbnail, rendered off the UI thread by the same
/// renderer as the preview. Online-only, empty and non-PDF files are never rendered (a render
/// would download the cloud file).
#[gpui::test]
fn grid_view_shows_the_first_page_of_pdfs(cx: &mut TestAppContext) {
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\docs",
        vec![
            file("a.pdf", 100),
            Entry::new(
                "cloud.pdf",
                EntryKind::File,
                100,
                None,
                RECALL_ON_DATA_ACCESS,
            ),
            file("empty.pdf", 0),
            file("notes.txt", 5),
        ],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\docs");
    wait_until(cx, &view, "load", loaded(r"C:\docs"));
    cx.simulate_keystrokes("ctrl-shift-2");
    let expected = vec![(PathBuf::from(r"C:\docs\a.pdf"), MediaKind::Pdf)];
    settle(cx, "the PDF's first page never loaded", |cx| {
        loaded_media(&view, cx) == expected
    });
    // Let anything wrongly queued behind it run too.
    for _ in 0..20 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let renders = shell.pdf_renders.lock().clone();
    assert!(!renders.is_empty());
    assert!(
        renders.iter().all(|(path, page, (w, h))| {
            path == Path::new(r"C:\docs\a.pdf") && *page == 0 && w == h && *w > 0
        }),
        "only the first page of the local PDF: {renders:?}"
    );
}

fn image_size(v: &Explorer) -> Option<(u32, u32)> {
    match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Image { width, height, .. }) => Some((*width, *height)),
        _ => None,
    }
}

/// An SVG is drawn like an image, 1600 px on its long side whatever its own size, in its
/// proportions, and lists no pixel size (a drawing has none).
#[gpui::test]
fn svg_files_are_rendered_in_the_preview(cx: &mut TestAppContext) {
    let wide = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
    let tall = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="blue"/></svg>"#;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\art",
        vec![
            file("broken.svg", 30),
            file("logo.svg", 120),
            file("tall.svg", 120),
        ],
    );
    fs.set_content(Path::new(r"C:\art\logo.svg"), wide.to_vec());
    fs.set_content(Path::new(r"C:\art\tall.svg"), tall.to_vec());
    fs.set_content(Path::new(r"C:\art\broken.svg"), b"<svg".to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\art");
    wait_until(cx, &view, "load", loaded(r"C:\art"));
    open_pane(&view, cx, (1200., 900.));

    // broken.svg: nothing to draw, but the details still list.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "broken", |v| {
        previewed(v)
            && matches!(
                v.preview.as_ref().map(|p| &p.body),
                Some(PreviewBody::Nothing)
            )
    });

    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "wide svg", |v| {
        image_size(v) == Some((1600, 800))
    });
    let rows = view.update(cx, |v, _| v.preview_info_for_test());
    assert!(
        rows.iter()
            .all(|(label, _)| *label != t!("preview.dimensions")),
        "no pixel size for a drawing: {rows:?}"
    );

    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "tall svg", |v| {
        image_size(v) == Some((800, 1600))
    });
}

/// A picture wider than the frame is shown in a frame as tall as the picture at the pane's width,
/// not in a taller one with the picture centered; the boundary handle sits at its bottom. A
/// tall picture keeps the full frame.
#[gpui::test]
fn the_preview_frame_hugs_a_wide_picture(cx: &mut TestAppContext) {
    let wide = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
    let tall = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="blue"/></svg>"#;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\art",
        vec![file("logo.svg", 120), file("tall.svg", 120)],
    );
    fs.set_content(Path::new(r"C:\art\logo.svg"), wide.to_vec());
    fs.set_content(Path::new(r"C:\art\tall.svg"), tall.to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\art");
    wait_until(cx, &view, "load", loaded(r"C:\art"));
    open_pane(&view, cx, (1200., 900.));
    let frame_and_handle = |cx: &mut VisualTestContext| {
        settle(cx, "the frame and handle are drawn", |cx| {
            cx.debug_bounds("preview-frame").is_some() && cx.debug_bounds("preview-split").is_some()
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        (
            cx.debug_bounds("preview-frame").expect("frame"),
            cx.debug_bounds("preview-split").expect("handle"),
        )
    };

    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "wide svg", |v| {
        image_size(v) == Some((1600, 800))
    });
    let (frame, handle) = frame_and_handle(cx);
    assert!(
        (frame.size.height - frame.size.width / 2.).abs() < px(2.),
        "as tall as the picture at this width: {frame:?}"
    );
    assert!(
        (handle.top() - frame.bottom()).abs() < px(2.),
        "the handle is under the picture: {frame:?} {handle:?}"
    );

    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "tall svg", |v| {
        image_size(v) == Some((800, 1600))
    });
    let (tall_frame, _) = frame_and_handle(cx);
    assert!(
        tall_frame.size.height > frame.size.height,
        "a tall picture keeps the full frame: {tall_frame:?}"
    );

    // Dragging the boundary far down stops at the picture's height; the share is still saved.
    cx.simulate_keystrokes("up");
    wait_until(cx, &view, "wide svg again", |v| {
        image_size(v) == Some((1600, 800))
    });
    let (_, handle) = frame_and_handle(cx);
    let at = handle.center();
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        at + point(px(0.), px(100.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(
        at + point(px(0.), px(200.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_up(
        at + point(px(0.), px(200.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.run_until_parked();
    let (dragged, _) = frame_and_handle(cx);
    assert!(
        (dragged.size.height - frame.size.height).abs() < px(2.),
        "no taller than the picture: {dragged:?}"
    );
    assert!(saved_split(cx).is_some(), "the dragged share is saved");
}

/// A gzip file with one stored (uncompressed) deflate block: valid, and built by hand so the
/// test needs no compressor.
fn gzip(name: Option<&str>, content: &[u8]) -> Vec<u8> {
    let crc = {
        let mut crc = !0u32;
        for &byte in content {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    };
    let mut out = vec![
        0x1f,
        0x8b,
        8,
        if name.is_some() { 8 } else { 0 },
        0,
        0,
        0,
        0,
        0,
        0xff,
    ];
    if let Some(name) = name {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    let len = u16::try_from(content.len()).unwrap();
    out.push(1);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(content);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&u32::try_from(content.len()).unwrap().to_le_bytes());
    out
}

/// A `.gz` lists the name inside it and its uncompressed size; small text shows as text, anything
/// else keeps the icon; a name-less one is named after the file.
#[gpui::test]
fn gz_files_show_what_they_hold(cx: &mut TestAppContext) {
    let text = b"id,name\n1,Ann";
    let binary = [0u8, 1, 2, 3, 0, 255];
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\logs",
        vec![
            file("a.csv.gz", 100),
            file("b.bin.gz", 100),
            file("c.gz", 10),
        ],
    );
    fs.set_content(
        Path::new(r"C:\logs\a.csv.gz"),
        gzip(Some("inner.csv"), text),
    );
    fs.set_content(Path::new(r"C:\logs\b.bin.gz"), gzip(None, &binary));
    fs.set_content(Path::new(r"C:\logs\c.gz"), b"not gzip, just bytes".to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\logs");
    wait_until(cx, &view, "load", loaded(r"C:\logs"));
    open_pane(&view, cx, (1200., 900.));
    let row = |view: &Entity<Explorer>, cx: &mut VisualTestContext, label: String| {
        view.update(cx, |v, _| v.preview_info_for_test())
            .into_iter()
            .find(|(l, _)| *l == label)
            .map(|(_, value)| value)
    };

    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "gz text", |v| {
        v.preview.as_ref().is_some_and(|p| p.body.text().is_some())
    });
    assert_eq!(
        view.update(cx, |v, _| v.preview.as_ref().and_then(|p| p.body.text())),
        Some("id,name\n1,Ann".to_string())
    );
    assert_eq!(
        row(&view, cx, t!("preview.gzip_name").to_string()).as_deref(),
        Some("inner.csv")
    );
    assert_eq!(
        row(&view, cx, t!("preview.gzip_size").to_string()),
        Some(arke_core::format::format_size(text.len() as u64))
    );

    // Not text: the icon, with the name taken from the file, as the header has none.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "gz binary", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Gzip(_))
        )
    });
    assert_eq!(
        row(&view, cx, t!("preview.gzip_name").to_string()).as_deref(),
        Some("b.bin")
    );
    assert_eq!(
        row(&view, cx, t!("preview.gzip_size").to_string()),
        Some(arke_core::format::format_size(binary.len() as u64))
    );

    // Not gzip at all: nothing to show beyond the usual details.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "damaged gz", |v| {
        previewed(v)
            && matches!(
                v.preview.as_ref().map(|p| &p.body),
                Some(PreviewBody::Nothing)
            )
    });
    assert_eq!(row(&view, cx, t!("preview.gzip_name").to_string()), None);
}

fn saved_split(cx: &mut VisualTestContext) -> Option<u16> {
    cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.preview_split)
}

/// The boundary between the preview content and the details below it follows a drag, within
/// limits, is saved as a share of the pane's height, and goes back to automatic on a double
/// click.
#[gpui::test]
fn the_preview_boundary_can_be_dragged(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.bin", 5)]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    open_pane(&view, cx, (1200., 900.));
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", previewed);
    // The preview arrived after the last frame: draw until the boundary is on screen.
    settle(cx, "the boundary handle is drawn", |cx| {
        cx.debug_bounds("preview-split").is_some()
    });
    assert_eq!(crate::prefs::preview_split(), None);

    // Drawn again first: the last frame may predate the drag that just ended.
    let handle = |cx: &mut VisualTestContext| {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.debug_bounds("preview-split")
            .expect("the boundary handle is drawn")
    };
    let drag = |cx: &mut VisualTestContext, dy: f32| {
        let grab = handle(cx);
        let at = point(grab.center().x, grab.center().y);
        cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            at + point(px(0.), px(dy / 2.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            at + point(px(0.), px(dy)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(
            at + point(px(0.), px(dy)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
    };

    // Up by 40 px: the content gets that much shorter.
    let before = handle(cx).bottom();
    drag(cx, -40.);
    let after = handle(cx).bottom();
    assert!(
        (before - after - px(40.)).abs() < px(2.),
        "the boundary followed the cursor: {before:?} -> {after:?}"
    );
    let share = saved_split(cx).expect("saved on release");
    assert!((150..=800).contains(&share), "{share}");

    // Dragged to the very top: held at the smallest share, not collapsed.
    drag(cx, -2000.);
    let smallest = saved_split(cx).expect("saved");
    assert_eq!(smallest, 150);
    assert!(handle(cx).bottom() > px(100.), "content keeps some height");

    // Double click: automatic again, and saved so.
    let at = handle(cx).center();
    cx.simulate_click(at, Modifiers::none());
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
    });
    cx.run_until_parked();
    assert_eq!(crate::prefs::preview_split(), None);
    assert_eq!(saved_split(cx), None);
    assert!(
        (handle(cx).bottom() - before).abs() < px(2.),
        "back to the first height"
    );
}

/// Moving on from a previewed picture frees its copy in the window's GPU atlas. Dropped while the
/// window was being updated, it stayed there: the pane kept every picture shown, about 7 MB each.
#[gpui::test]
fn the_previous_picture_leaves_the_gpu_atlas(cx: &mut TestAppContext) {
    let wide = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
    let tall = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="blue"/></svg>"#;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\art", vec![file("a.svg", 120), file("b.svg", 120)]);
    fs.set_content(Path::new(r"C:\art\a.svg"), wide.to_vec());
    fs.set_content(Path::new(r"C:\art\b.svg"), tall.to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\art");
    wait_until(cx, &view, "load", loaded(r"C:\art"));
    open_pane(&view, cx, (1200., 900.));
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "a.svg", |v| image_size(v) == Some((1600, 800)));
    let first = view.update(cx, |v, _| match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Image { image, .. }) => Arc::clone(image),
        _ => panic!("a picture"),
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(
        cx.update(|window, _| window.has_image_atlas_entry(&first)),
        "drawn, so in the atlas"
    );
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "b.svg", |v| image_size(v) == Some((800, 1600)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(
        !cx.update(|window, _| window.has_image_atlas_entry(&first)),
        "the picture before is gone from the atlas"
    );
}
