//! GPUI tests for text previews of large files: only the lines on screen are laid out, the rest
//! of the file is read once it is scrolled to, and the last line can be reached, wrapped or not.

use std::fmt::Write as _;

use super::*;
use crate::explorer::{PreviewBody, take_built_rows_for_test};
use gpui::{point, px};

/// Most line rows a frame of the pane may build: the lines on screen and a few around them. The
/// head alone holds thousands of these lines; all of them were laid out before.
const ON_SCREEN_MAX: usize = 120;

fn show_preview_pane(cx: &mut VisualTestContext) {
    cx.simulate_resize(gpui::size(px(1500.), px(800.)));
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.preview_pane = true, cx));
    });
    cx.run_until_parked();
}

/// `count` lines `line 0000000` up, joined by line breaks (no break after the last).
fn numbered(count: usize) -> String {
    (0..count)
        .map(|n| format!("line {n:07}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lines in the text preview, and whether the whole file is indexed.
fn text_state(v: &Explorer) -> Option<(usize, bool)> {
    match &v.preview.as_ref()?.body {
        PreviewBody::Text { doc, .. } => Some((doc.count(), doc.indexed_for_test())),
        _ => None,
    }
}

/// Line `ix` of the text preview, once read.
fn line_text(v: &Explorer, ix: usize) -> Option<String> {
    match &v.preview.as_ref()?.body {
        PreviewBody::Text { doc, .. } => doc.line_for_test(ix),
        _ => None,
    }
}

/// `name` with `body` as the only file in `dir`, previewed in the pane.
fn previewed<'a>(
    cx: &'a mut TestAppContext,
    dir: &'static str,
    name: &str,
    body: String,
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(dir, vec![file(name, body.len() as u64)]);
    fs.set_content(Path::new(dir).join(name), body.into_bytes());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), dir);
    wait_until(cx, &view, "load", loaded(dir));
    show_preview_pane(cx);
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "the head", |v| text_state(v).is_some());
    cx.run_until_parked();
    (view, cx)
}

/// Rows built by one more frame.
fn rows_of_a_frame(cx: &mut VisualTestContext, view: &Entity<Explorer>) -> usize {
    take_built_rows_for_test();
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    take_built_rows_for_test()
}

/// Scrolls the pane's text to its end.
fn scroll_to_end(cx: &mut VisualTestContext, view: &Entity<Explorer>) {
    view.update(cx, |v, cx| {
        let max = v.preview_scroll.max_offset();
        v.preview_scroll.set_offset(point(px(0.), -max.y));
        cx.notify();
    });
    cx.run_until_parked();
}

/// Scrolls past the head, waits for the index, scrolls to the very end and waits for the last
/// line `last` to be read.
fn reach_the_end(cx: &mut VisualTestContext, view: &Entity<Explorer>, count: usize, last: &str) {
    scroll_to_end(cx, view);
    wait_until(cx, view, "the index", |v| {
        text_state(v) == Some((count, true))
    });
    // The frame after the index has the whole file's height to scroll to.
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    scroll_to_end(cx, view);
    let want = last.to_owned();
    wait_until(cx, view, "the last line", move |v| {
        line_text(v, count - 1).as_deref() == Some(want.as_str())
    });
    cx.run_until_parked();
}

/// Where line `ix`'s row was drawn in the last frame, if it was.
fn line_bounds(cx: &mut VisualTestContext, ix: usize) -> Option<gpui::Bounds<gpui::Pixels>> {
    // The test context looks selectors up by `&'static str`.
    let selector: &'static str = Box::leak(format!("preview-line-{ix}").into_boxed_str());
    cx.debug_bounds(selector)
}

/// Line `ix` is drawn in the text's frame with its end in view: all of it, or for a wrapped line
/// taller than the frame its last rows, at the bottom.
fn assert_drawn_inside(cx: &mut VisualTestContext, ix: usize) -> gpui::Bounds<gpui::Pixels> {
    let frame = cx.debug_bounds("preview-text").expect("the text is drawn");
    let line = line_bounds(cx, ix).unwrap_or_else(|| panic!("line {ix} is drawn"));
    let tall = line.size.height > frame.size.height;
    assert!(
        line.bottom() <= frame.bottom() + px(1.)
            && line.bottom() > frame.top()
            && (tall || line.top() >= frame.top() - px(1.)),
        "line {ix} at {line:?} ends in {frame:?}"
    );
    line
}

/// A 200,000-line log: only the lines on screen are laid out; scrolled past the head, the file is
/// indexed and its last line can be reached, still laying out only a screenful.
#[gpui::test]
fn a_large_log_lays_out_only_the_lines_on_screen(cx: &mut TestAppContext) {
    const LINES: usize = 200_000;
    let (view, cx) = previewed(cx, r"C:\big", "app.log", numbered(LINES));
    // Only the head is read until the view comes near its end.
    let (head, indexed) = view.update(cx, |v, _| text_state(v)).expect("text");
    assert!(!indexed);
    assert!(head > 1000 && head < LINES, "{head} lines in the head");
    let built = rows_of_a_frame(cx, &view);
    assert!(
        built > 0 && built < ON_SCREEN_MAX,
        "{built} rows laid out for one frame"
    );
    assert!(line_bounds(cx, 0).is_some());
    assert!(line_bounds(cx, head - 2).is_none());

    reach_the_end(cx, &view, LINES, "line 0199999");
    assert_drawn_inside(cx, LINES - 1);
    let built = rows_of_a_frame(cx, &view);
    assert!(
        built > 0 && built < ON_SCREEN_MAX,
        "{built} rows laid out at the end"
    );
    // A line from the middle of the file, read when scrolled to.
    view.update(cx, |v, cx| {
        let max = v.preview_scroll.max_offset();
        v.preview_scroll.set_offset(point(px(0.), -max.y / 2.));
        cx.notify();
    });
    wait_until(cx, &view, "the middle", |v| {
        line_text(v, LINES / 2).as_deref() == Some("line 0100000")
    });
}

/// A table (CSV) doesn't wrap: every row is one line high, even a very long one, and the view
/// scrolls sideways to it. Its last row can be reached like a log's.
#[gpui::test]
fn a_large_table_reaches_its_last_row_unwrapped(cx: &mut TestAppContext) {
    const ROWS: usize = 100_000;
    let mut body = (0..ROWS - 1).fold(String::new(), |mut body, n| {
        let _ = writeln!(body, "{n},a,b");
        body
    });
    let long = "x".repeat(2000);
    body.push_str(&long);
    let (view, cx) = previewed(cx, r"C:\tab", "data.csv", body);
    let built = rows_of_a_frame(cx, &view);
    assert!(built > 0 && built < ON_SCREEN_MAX, "{built} rows");

    reach_the_end(cx, &view, ROWS, &long);
    let last = assert_drawn_inside(cx, ROWS - 1);
    let before = line_bounds(cx, ROWS - 2).expect("the row before is drawn");
    assert_eq!(
        last.size.height, before.size.height,
        "the long row is one line"
    );
    let (_, max) = view.update(cx, |v, _| v.preview_scroll_for_test());
    assert!(max.x > px(0.), "the long row scrolls sideways: {max:?}");
}

/// Text that isn't a table wraps: a long line takes several rows and nothing scrolls sideways.
/// Its last line is still reached when the lines wrap.
#[gpui::test]
fn long_lines_wrap_in_a_large_text(cx: &mut TestAppContext) {
    const LINES: usize = 20_000;
    let long = "word ".repeat(80);
    let body = (0..LINES)
        .map(|n| {
            if n % 2 == 0 {
                format!("short {n}")
            } else {
                format!("{n} {long}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (view, cx) = previewed(cx, r"C:\wrap", "notes.txt", body);
    let short = line_bounds(cx, 0).expect("line 1 is drawn");
    let wrapped = line_bounds(cx, 1).expect("line 2 is drawn");
    assert!(
        wrapped.size.height > short.size.height * 2.5,
        "the long line wraps: {wrapped:?} vs {short:?}"
    );
    let (_, max) = view.update(cx, |v, _| v.preview_scroll_for_test());
    assert_eq!(max.x, px(0.), "nothing scrolls sideways");
    let built = rows_of_a_frame(cx, &view);
    assert!(built > 0 && built < ON_SCREEN_MAX, "{built} rows");

    let last = format!("{} {long}", LINES - 1);
    reach_the_end(cx, &view, LINES, &last);
    // The heights the model learned while scrolling may move the end a little: scroll there again
    // once the lines near it have been laid out.
    scroll_to_end(cx, &view);
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    scroll_to_end(cx, &view);
    assert_drawn_inside(cx, LINES - 1);
}
