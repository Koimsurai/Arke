//! GPUI tests for Quick Look's wheel and zoom.

use super::*;
use crate::explorer::PreviewBody;
use gpui::{Modifiers, ScrollDelta, ScrollEvent, point, px};

fn previewed(v: &Explorer) -> bool {
    v.preview
        .as_ref()
        .is_some_and(|p| !matches!(p.body, PreviewBody::Loading(_)))
}

/// A folder with a long text file and enough other items for the list to scroll.
fn long_text_folder() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    let mut entries = vec![file("a.txt", 4000)];
    entries.extend((0..200).map(|i| file(&format!("z{i:03}.bin"), 1)));
    fs.insert_dir(r"C:\q", entries);
    let text = (0..400)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs.set_content(Path::new(r"C:\q\a.txt"), text.into_bytes());
    fs
}

/// Quick Look open on the long text file.
fn long_text(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let (view, cx) = setup(
        cx,
        long_text_folder(),
        Arc::new(FakeShell::default()),
        r"C:\q",
    );
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    cx.simulate_keystrokes("down space");
    wait_until(cx, &view, "text previewed", previewed);
    cx.run_until_parked();
    (view, cx)
}

/// The wheel over Quick Look's stage: `lines` (negative = down), with `modifiers`.
fn wheel(cx: &mut VisualTestContext, lines: f32, modifiers: Modifiers) {
    let center = cx
        .debug_bounds("ql-stage")
        .expect("Quick Look should be drawn")
        .center();
    cx.simulate_event(ScrollEvent {
        position: center,
        delta: ScrollDelta::Lines(point(0., lines)),
        modifiers,
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
}

fn ctrl() -> Modifiers {
    Modifiers {
        control: true,
        ..Modifiers::default()
    }
}

/// The wheel over a text file scrolls it, and only it: the list behind stays put.
#[gpui::test]
fn wheel_scrolls_text_in_quick_look(cx: &mut TestAppContext) {
    let (view, cx) = long_text(cx);
    let list_before = view.update(cx, |v, _| v.list_scroll_for_test().y);
    wheel(cx, -5., Modifiers::none());
    let offset = view.update(cx, |v, _| v.look_scroll.offset().y);
    assert!(offset < px(0.), "the text should scroll, offset {offset:?}");
    assert_eq!(
        view.update(cx, |v, _| v.list_scroll_for_test().y),
        list_before,
        "the list scrolled behind Quick Look"
    );
}

/// Ctrl+wheel makes the text larger (it doesn't scroll it, and the list's icons stay as they
/// are); Ctrl+- and Ctrl+0 step back, and the zoom is dropped when Quick Look closes.
#[gpui::test]
fn ctrl_wheel_zooms_text_in_quick_look(cx: &mut TestAppContext) {
    let (view, cx) = long_text(cx);
    let tall = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.look_scroll.max_offset().y);
    let before = tall(cx);
    let icons = view.update(cx, |v, _| v.icon_size);
    assert!((view.update(cx, |v, _| v.doc_zoom()) - 1.).abs() < f32::EPSILON);

    wheel(cx, 3., ctrl());
    wheel(cx, 3., ctrl());
    let zoom = view.update(cx, |v, _| v.doc_zoom());
    assert!(zoom > 1.2, "two notches in, zoom {zoom}");
    assert_eq!(view.update(cx, |v, _| v.look_scroll.offset().y), px(0.));
    assert!(tall(cx) > before, "larger text makes the page longer");
    assert_eq!(view.update(cx, |v, _| v.icon_size), icons);
    assert!(cx.debug_bounds("ql-doc-zoom").is_some(), "zoom chip shown");

    cx.simulate_keystrokes("ctrl--");
    assert!(view.update(cx, |v, _| v.doc_zoom()) < zoom);
    cx.simulate_keystrokes("ctrl-0");
    assert!((view.update(cx, |v, _| v.doc_zoom()) - 1.).abs() < f32::EPSILON);
    cx.run_until_parked();
    assert!(cx.debug_bounds("ql-doc-zoom").is_none(), "no chip at 100%");
    cx.simulate_keystrokes("ctrl-=");
    assert!(view.update(cx, |v, _| v.doc_zoom()) > 1.);

    // Esc and Space still close it; reopened, it starts at 100%.
    cx.simulate_keystrokes("escape");
    assert!(!view.update(cx, |v, _| v.quick_look));
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    cx.simulate_keystrokes("space");
    assert!(view.update(cx, |v, _| v.quick_look));
    assert!((view.update(cx, |v, _| v.doc_zoom()) - 1.).abs() < f32::EPSILON);
    cx.simulate_keystrokes("space");
    assert!(!view.update(cx, |v, _| v.quick_look));
}

/// Page and page count of the PDF on screen.
fn pdf_page(v: &Explorer) -> Option<u32> {
    match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Pdf(view)) if view.render.pages == 5 => Some(view.page),
        _ => None,
    }
}

/// On a PDF the wheel turns pages; zoomed in, it first scrolls the page, then turns to the next
/// one at its top.
#[gpui::test]
fn wheel_turns_pdf_pages_after_scrolling_them(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.pdf", 100)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    cx.simulate_keystrokes("down space");
    wait_until(cx, &view, "page 0", |v| pdf_page(v) == Some(0));
    cx.run_until_parked();

    wheel(cx, -3., Modifiers::none());
    wait_until(cx, &view, "page 1", |v| pdf_page(v) == Some(1));
    wheel(cx, 3., Modifiers::none());
    wait_until(cx, &view, "back to page 0", |v| pdf_page(v) == Some(0));

    // Zoomed in, the page is taller than the stage and is rendered for the larger box.
    let fitted = shell
        .pdf_renders
        .lock()
        .last()
        .map(|r| r.2)
        .expect("rendered");
    wheel(cx, 3., ctrl());
    wheel(cx, 3., ctrl());
    wheel(cx, 3., ctrl());
    // The larger render runs in the background.
    let rendered_larger = || {
        shell
            .pdf_renders
            .lock()
            .last()
            .is_some_and(|r| r.2.1 > fitted.1)
    };
    wait_until(cx, &view, "zoomed page rendered larger", |_| {
        rendered_larger()
    });
    assert_eq!(view.update(cx, |v, _| pdf_page(v)), Some(0));
    assert!(view.update(cx, |v, _| v.look_scroll.max_offset().y) > px(0.));

    wheel(cx, -1., Modifiers::none());
    assert!(
        view.update(cx, |v, _| v.look_scroll.offset().y) < px(0.),
        "the first notch scrolls the page"
    );
    assert_eq!(view.update(cx, |v, _| pdf_page(v)), Some(0));
    // Stop once the turn is asked for: the page renders in the background, and more notches
    // before it lands would scroll the new page.
    let turning_to = |v: &Explorer| match v.preview.as_ref().map(|p| &p.body) {
        Some(PreviewBody::Pdf(view)) => Some(view.turning_to()),
        _ => None,
    };
    for _ in 0..60 {
        if view.update(cx, |v, _| turning_to(v)) == Some(1) {
            break;
        }
        wheel(cx, -3., Modifiers::none());
    }
    wait_until(cx, &view, "page 1 after scrolling", |v| {
        pdf_page(v) == Some(1)
    });
    assert_eq!(
        view.update(cx, |v, _| v.look_scroll.offset().y),
        px(0.),
        "the next page starts at its top"
    );
}
