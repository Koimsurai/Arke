//! GPUI tests for the operation center's list (#403): the header lines up with the rows and
//! always offers the same buttons, a hairline separates the header and each job from the next,
//! a job's icon and button sit on the middle of its text, a long name gives way before the item
//! count, and the list stays inside a short, narrow window.

use gpui::{Bounds, Pixels, px};

use super::*;
use crate::op_center::OpCenter;
use crate::shell::{OpOutcome, OpProgress};

/// A name too long for the list, so the title has to be shortened.
const LONG_NAME: &str = "ClassifiedArchitectureReview-final-draft-2026-11-25.docx";

fn bounds(cx: &mut VisualTestContext, selector: &str) -> Bounds<Pixels> {
    let selector: &'static str = selector.to_owned().leak();
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"))
}

fn drawn(cx: &mut VisualTestContext, selector: &str) -> bool {
    let selector: &'static str = selector.to_owned().leak();
    cx.debug_bounds(selector).is_some()
}

/// The same position, give or take rounding to device pixels.
fn near(a: Pixels, b: Pixels) -> bool {
    (a - b).abs() <= px(0.5)
}

/// A pane on `C:\o` in a window of `width` × `height`.
fn pane(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\o");
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.simulate_resize(gpui::size(px(width), px(height)));
    (view, cx)
}

/// `finished` finished copies, then a running copy of [`LONG_NAME`] and three more items (the
/// newest, so the list's first row). Returns the running job's progress: holding it keeps the job
/// running.
fn add_jobs(cx: &mut VisualTestContext, finished: usize) -> Arc<OpProgress> {
    cx.update(|_, cx| {
        for i in 0..finished {
            let (id, _) = OpCenter::begin(cx, "op.copy", Some((format!("done-{i}.txt"), 1)));
            OpCenter::finish::<String>(cx, id, "op.copy", &Ok(OpOutcome::Done));
        }
        let (_, progress) = OpCenter::begin(cx, "op.copy", Some((LONG_NAME.to_owned(), 4)));
        progress
    })
}

/// Opens the list from its button in the title row.
fn open_list(cx: &mut VisualTestContext, view: &Entity<Explorer>) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let button = bounds(cx, "op-center");
    cx.simulate_click(button.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.ops_open), "the list is open");
}

/// The header spans the rows' width and its buttons end where the rows' buttons do, set apart
/// from them by a hairline; jobs are separated by hairlines (none after the last); a job's icon
/// and Cancel sit on the middle of its text; and the title keeps "(4 items)" whole while the
/// name gives way.
#[gpui::test]
fn the_ops_list_lines_up_and_keeps_the_item_count(cx: &mut TestAppContext) {
    let (view, cx) = pane(cx, 1200., 800.);
    let _running = add_jobs(cx, 2);
    open_list(cx, &view);

    let header = bounds(cx, "op-center-header");
    let row = bounds(cx, "op-center-row-0");
    assert!(
        near(header.left(), row.left()) && near(header.right(), row.right()),
        "the header lines up with the rows: {header:?} / {row:?}"
    );
    let actions = bounds(cx, "op-center-header-actions");
    let cancel = bounds(cx, "op-center-action-0");
    assert!(
        near(actions.right(), cancel.right()),
        "the header's buttons end where the rows' do: {actions:?} / {cancel:?}"
    );
    let pause = bounds(cx, "op-center-pause");
    let clear = bounds(cx, "op-center-clear");
    assert!(
        clear.left() - pause.right() >= px(3.5) && near(pause.center().y, clear.center().y),
        "Pause queue and Clear finished side by side, apart: {pause:?} / {clear:?}"
    );

    let header_rule = bounds(cx, "op-center-header-rule");
    assert!(
        header_rule.top() >= header.bottom() && header_rule.bottom() <= row.top(),
        "a hairline between the header and the first job: {header_rule:?}"
    );
    let rules = (0..6)
        .filter(|i| drawn(cx, &format!("op-center-rule-{i}")))
        .count();
    assert_eq!(rules, 2, "three jobs, a hairline between each two");
    let second = bounds(cx, "op-center-row-1");
    let rule = bounds(cx, "op-center-rule-1");
    assert!(
        rule.top() >= row.bottom() && rule.bottom() <= second.top(),
        "the hairline sits between the jobs: {row:?} / {rule:?} / {second:?}"
    );

    let icon = bounds(cx, "op-center-row-0-icon");
    let text = bounds(cx, "op-center-row-0-text");
    assert!(
        text.size.height > icon.size.height * 2.,
        "title, status line and progress bar: {text:?}"
    );
    assert!(
        near(icon.center().y, text.center().y),
        "the icon on the job's middle, not its first line: {icon:?} / {text:?}"
    );
    assert!(
        near(cancel.center().y, text.center().y),
        "Cancel on the job's middle: {cancel:?} / {text:?}"
    );

    let count = bounds(cx, "op-center-row-0-count");
    assert!(
        count.size.width > px(0.) && count.left() > text.left() && count.right() <= text.right(),
        "\"4 items)\" shows whole at the end of the title: {count:?} / {text:?}"
    );
    let name = bounds(cx, "op-center-row-0-name");
    assert!(
        name.size.width >= crate::theme::s_32() - px(0.5) && name.right() <= count.left(),
        "a few letters of the name are left, before the count: {name:?} / {count:?}"
    );
    let title = cx.update(|_, cx| OpCenter::jobs(cx).last().unwrap().title());
    assert!(title.ends_with("(4 items)"), "{title}");
}

/// The header offers Pause queue and Clear finished whatever the list holds: only a running job,
/// then only finished ones. Clear finished then empties the list and closes it.
#[gpui::test]
fn the_ops_list_always_offers_the_same_header_buttons(cx: &mut TestAppContext) {
    let (view, cx) = pane(cx, 1200., 800.);
    let running = add_jobs(cx, 0);
    open_list(cx, &view);
    assert!(
        drawn(cx, "op-center-pause") && drawn(cx, "op-center-clear"),
        "while a job runs"
    );

    cx.update(|_, cx| {
        let id = OpCenter::jobs(cx).last().unwrap().id;
        OpCenter::finish::<String>(cx, id, "op.copy", &Ok(OpOutcome::Done));
    });
    drop(running);
    cx.run_until_parked();
    assert!(
        drawn(cx, "op-center-pause") && drawn(cx, "op-center-clear"),
        "once it finished"
    );
    assert!(
        !drawn(cx, "op-center-rule-1"),
        "one job, no hairline after it"
    );

    let clear = bounds(cx, "op-center-clear");
    cx.simulate_click(clear.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.update(|_, cx| OpCenter::jobs(cx).is_empty()));
    assert!(!view.update(cx, |v, _| v.ops_open), "nothing left: closed");
}

/// In a short, narrow window the list ends inside the window and scrolls, under its button
/// rather than pushed up over it, and the header's buttons stay inside it.
#[gpui::test]
fn the_ops_list_fits_a_short_narrow_window(cx: &mut TestAppContext) {
    let (view, cx) = pane(cx, 480., 300.);
    let _running = add_jobs(cx, 12);
    open_list(cx, &view);

    let window = cx.update(|window, _| window.viewport_size());
    let button = bounds(cx, "op-center");
    let panel = bounds(cx, "op-center-panel");
    assert!(
        panel.top() >= button.bottom(),
        "under its button: {button:?} / {panel:?}"
    );
    assert!(
        panel.bottom() <= window.height && panel.left() >= px(0.) && panel.right() <= window.width,
        "inside the window ({window:?}): {panel:?}"
    );
    let actions = bounds(cx, "op-center-header-actions");
    assert!(
        actions.left() >= panel.left() && actions.right() <= panel.right(),
        "the header's buttons inside the list: {actions:?} / {panel:?}"
    );
}
