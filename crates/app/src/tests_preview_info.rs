//! GPUI tests for the details under a preview: the side pane's Info list and Quick Look's.

use super::*;
use crate::explorer::PreviewBody;
use gpui::{Bounds, Pixels, px};

/// A file with a long name, a type label longer than a narrow value column, a size and both
/// dates.
fn long_file() -> Entry {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    Entry::new(
        "quarterly-schema-migration-2026.backup-archive",
        EntryKind::File,
        1536,
        Some(at),
        0,
    )
    .with_created(Some(at))
}

fn previewed(v: &Explorer) -> bool {
    v.preview
        .as_ref()
        .is_some_and(|p| !matches!(p.body, PreviewBody::Loading(_)))
}

fn bounds(cx: &mut VisualTestContext, id: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} should be drawn"))
}

/// The labels of the Info list, in order.
fn labels(rows: &[(String, String)]) -> Vec<&str> {
    rows.iter().map(|(label, _)| label.as_str()).collect()
}

/// A file lists its type, size and both dates, and a narrow pane (its narrowest, 220 px) shows
/// all of each: the name takes a line of its own above the actions and wraps, and values wrap
/// instead of ending in "…".
#[gpui::test]
fn narrow_preview_pane_shows_every_detail_in_full(cx: &mut TestAppContext) {
    let entry = long_file();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![entry.clone()]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    crate::prefs::set_preview_width(Some(px(220.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", previewed);
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.fit_for_test()), (true, 220.));

    let rows = view.update(cx, |v, _| v.preview_info_for_test());
    assert_eq!(
        labels(&rows),
        [
            t!("col.type"),
            t!("col.size"),
            t!("col.created"),
            t!("col.modified"),
            t!("col.location"),
        ]
    );
    assert_eq!(rows[0].1, arke_core::format::type_label(&entry));
    assert_eq!(rows[1].1, arke_core::format::format_size(1536));
    assert_eq!(rows[2].1, "2026-01-01 00:00");
    assert_eq!(rows[4].1, r"C:\p");

    // The name is wider than the pane: it wraps over several lines, and the actions, which no
    // longer fit beside it, sit on the line below.
    let name = bounds(cx, "pane-name");
    assert!(
        name.size.height >= px(30.),
        "the name wraps instead of being cut: {name:?}"
    );
    let actions = bounds(cx, "pane-actions");
    assert!(
        actions.top() >= name.bottom(),
        "actions below the name: {actions:?} / {name:?}"
    );
    // The type label is wider than the value column: it takes two lines where the size takes one.
    let kind = bounds(cx, "pane-info-value-0");
    let size = bounds(cx, "pane-info-value-1");
    assert!(
        kind.size.height > size.size.height * 1.5,
        "the type wraps: {kind:?} / {size:?}"
    );
    assert!(kind.right() <= name.right() + px(1.), "inside the pane");
}

/// A short window: the preview gives up height so the name and every detail below it stay inside
/// the window (they were pushed off its bottom edge).
#[gpui::test]
fn a_short_window_keeps_the_details_on_screen(cx: &mut TestAppContext) {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    let entry = Entry::new(
        "meeting-notes-20260930.docx",
        EntryKind::File,
        49_152,
        Some(at),
        0,
    )
    .with_created(Some(at));
    let dir = r"C:\shares\team\progress\weekly\2026\applied-ai-project";
    let fs = FakeFs::new();
    fs.insert_dir(dir, vec![entry]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), dir);
    wait_until(cx, &view, "load", loaded(dir));
    let height = px(560.);
    cx.simulate_resize(gpui::size(px(1400.), height));
    crate::prefs::set_preview_width(Some(px(360.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", previewed);
    cx.run_until_parked();
    let rows = view.update(cx, |v, _| v.preview_info_for_test());
    let last = rows.len() - 1;
    let id: &'static str = Box::leak(format!("pane-info-value-{last}").into_boxed_str());
    let location = bounds(cx, id);
    assert!(
        location.bottom() <= height,
        "the last detail ends inside the window: {location:?}"
    );
    let name = bounds(cx, "pane-name");
    assert!(name.top() >= px(0.) && name.bottom() <= height, "{name:?}");

    // With room again, the preview takes its full height back.
    cx.simulate_resize(gpui::size(px(1400.), px(1000.)));
    cx.run_until_parked();
    let tall = bounds(cx, "pane-name");
    assert!(
        tall.top() > name.top(),
        "the preview grew: {tall:?} / {name:?}"
    );
}

/// With room for a wide pane, the name and the actions share one line again.
#[gpui::test]
fn wide_preview_pane_keeps_the_name_beside_the_actions(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 5)]);
    fs.set_content(Path::new(r"C:\p\a.txt"), b"hello".to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1600.), px(900.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", previewed);
    cx.run_until_parked();
    let name = bounds(cx, "pane-name");
    let actions = bounds(cx, "pane-actions");
    assert!(
        actions.top() < name.bottom() && actions.left() > name.right(),
        "one line: {name:?} / {actions:?}"
    );
    // No creation time known (the fake listing has none): no empty row for it.
    let rows = view.update(cx, |v, _| v.preview_info_for_test());
    assert!(!labels(&rows).contains(&t!("col.created").as_ref()));
}

/// A search hit that came without a creation time gets it from a stat off the UI thread (the
/// fake file system asserts that), so its Info list has the row too.
#[gpui::test]
fn a_search_hit_reads_its_creation_time(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\proj", vec![dir("docs")]);
    fs.insert_dir(r"C:\proj\docs", vec![file("report.txt", 5)]);
    fs.set_content(Path::new(r"C:\proj\docs\report.txt"), b"hello".to_vec());
    let (view, cx) = setup(
        cx,
        Arc::clone(&fs),
        Arc::new(FakeShell::default()),
        r"C:\proj",
    );
    wait_until(cx, &view, "load", loaded(r"C:\proj"));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("report");
    wait_until(cx, &view, "search results", |v| final_hits(v).len() == 1);
    // Only the stat knows the creation time now; the hit keeps what the walk found.
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    fs.add_entry(
        Path::new(r"C:\proj\docs"),
        file("report.txt", 5).with_created(Some(at)),
    );
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "creation time read", |v| {
        v.preview
            .as_ref()
            .is_some_and(|p| p.entry.created == Some(at))
    });
    let rows = view.update(cx, |v, _| v.preview_info_for_test());
    assert!(labels(&rows).contains(&t!("col.created").as_ref()));
}

/// Quick Look keeps the whole card for the preview: no details list beside the stage; the title
/// bar's line under the name carries type, size and modified.
#[gpui::test]
fn quick_look_keeps_the_whole_card_for_the_preview(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![long_file()]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1600.), px(1000.)));
    cx.simulate_keystrokes("down space");
    assert!(view.update(cx, |v, _| v.quick_look));
    wait_until(cx, &view, "previewed", previewed);
    cx.run_until_parked();
    assert!(cx.debug_bounds("ql-info").is_none(), "no details list");
    assert!(cx.debug_bounds("pane-info-value-0").is_none());
}

/// A video shows a frame in the preview pane, as in the icons view, without the frame's size as
/// its dimensions.
#[gpui::test]
fn a_video_previews_a_frame(cx: &mut TestAppContext) {
    let dir = r"C:\clips";
    let fs = FakeFs::new();
    fs.insert_dir(
        dir,
        vec![Entry::new("trip.mp4", EntryKind::File, 4096, None, 0)],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), dir);
    wait_until(cx, &view, "load", loaded(dir));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", previewed);
    assert!(view.update(cx, |v, _| matches!(
        v.preview.as_ref().map(|p| &p.body),
        Some(PreviewBody::Image { .. })
    )));
    assert!(
        shell
            .video_thumbs
            .lock()
            .contains(&PathBuf::from(r"C:\clips\trip.mp4"))
    );
    let rows = view.update(cx, |v, _| v.preview_info_for_test());
    let dimensions = t!("preview.dimensions").to_string();
    assert!(rows.iter().all(|(key, _)| *key != dimensions), "{rows:?}");
}

/// Put a video into the loading state (past the grace period) and keep the real load from
/// replacing it.
fn video_loading(view: &Entity<Explorer>, cx: &mut VisualTestContext) {
    view.update(cx, |v, cx| {
        let since = Instant::now()
            .checked_sub(Duration::from_secs(5))
            .expect("an instant in the past");
        v.show_loading_for_test(
            PathBuf::from(r"C:\clips\trip.mp4"),
            Entry::new("trip.mp4", EntryKind::File, 4096, None, 0),
            since,
            cx,
        );
    });
    cx.run_until_parked();
}

fn video_folder(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let dir = r"C:\clips";
    let fs = FakeFs::new();
    fs.insert_dir(
        dir,
        vec![Entry::new("trip.mp4", EntryKind::File, 4096, None, 0)],
    );
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), dir);
    wait_until(cx, &view, "load", loaded(dir));
    cx.simulate_resize(gpui::size(px(1400.), px(900.)));
    (view, cx)
}

/// While a video's frame loads, the side pane shows the picture-style placeholder filling the
/// media frame, not the text-line skeleton.
#[gpui::test]
fn a_loading_video_shows_the_picture_placeholder_in_the_pane(cx: &mut TestAppContext) {
    let (view, cx) = video_folder(cx);
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", previewed);
    video_loading(&view, cx);
    let frame = cx.debug_bounds("media-placeholder").expect("placeholder");
    let icon = cx.debug_bounds("media-placeholder-icon").expect("icon");
    assert!(frame.contains(&icon.center()), "the icon sits in the frame");
    assert!(icon.size.width <= px(48.) && icon.size.width >= px(32.));
    assert!(cx.debug_bounds("preview-skeleton").is_none());
}

/// The same in Quick Look.
#[gpui::test]
fn a_loading_video_shows_the_picture_placeholder_in_quick_look(cx: &mut TestAppContext) {
    let (view, cx) = video_folder(cx);
    cx.simulate_keystrokes("down space");
    assert!(view.update(cx, |v, _| v.quick_look));
    wait_until(cx, &view, "previewed", previewed);
    video_loading(&view, cx);
    let stage = cx.debug_bounds("ql-stage").expect("Quick Look is drawn");
    let frame = cx
        .debug_bounds("media-placeholder")
        .expect("the placeholder is drawn");
    assert!(cx.debug_bounds("preview-skeleton").is_none());
    let icon = cx.debug_bounds("media-placeholder-icon").expect("icon");
    assert!(
        (icon.center().x - frame.center().x).abs() < px(2.),
        "centered"
    );
    assert!(frame.size.width > stage.size.width * 0.8, "fills the stage");
    assert!(frame.size.height > stage.size.height * 0.8);
}

/// A folder's details stay in the side pane while its contents load and after they come in, in
/// the same place: they used to show only until the contents replaced them (#290).
#[gpui::test]
fn a_folder_keeps_its_details_beside_its_contents(cx: &mut TestAppContext) {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\p",
        vec![Entry::new("docs", EntryKind::Dir, 0, Some(at), 0x10).with_created(Some(at))],
    );
    fs.insert_dir(
        r"C:\p\docs",
        vec![Entry::new("a.txt", EntryKind::File, 5, None, 0)],
    );
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::new(FakeShell::default()), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    fs.hang(r"C:\p\docs");
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| !previewed(v)), "still loading");
    let loading = bounds(cx, "pane-info-value-0");
    fs.release(Path::new(r"C:\p\docs"));
    wait_until(cx, &view, "previewed", previewed);
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| matches!(
        v.preview.as_ref().map(|p| &p.body),
        Some(PreviewBody::Folder { .. })
    )));
    assert_eq!(bounds(cx, "pane-info-value-0"), loading, "details stay put");
}
