//! GPUI tests for transitions (`motion.rs`): dismissed surfaces fade out as inert ghosts for
//! `motion::EXIT` and then go away, choices close at once, and cues play only right after the user
//! started them. Animations are off in tests, so each test turns them on for itself.

use std::time::Duration;

use super::*;
use crate::explorer::ViewMode;

/// Animations on for the test's lifetime.
struct Motion;

impl Motion {
    fn on() -> Self {
        crate::motion::force_for_test(true);
        Self
    }
}

impl Drop for Motion {
    fn drop(&mut self) {
        crate::motion::force_for_test(false);
    }
}

/// Past the exit fade (and the ghost's frame of slack).
fn after_exit(cx: &mut VisualTestContext) {
    cx.executor()
        .advance_clock(crate::motion::EXIT + Duration::from_millis(60));
    cx.run_until_parked();
}

fn menu_state(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> (bool, bool) {
    view.update(cx, |v, _| {
        (v.context_menu.is_some(), v.closing_menu.is_some())
    })
}

/// Escape fades the context menu out (it is closed for everything else at once); a new menu
/// replaces the ghost; running a command closes it without one.
#[gpui::test]
fn dismissed_menu_fades_out_and_a_command_closes_it_at_once(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![file("a.txt", 1), file("b.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));

    cx.simulate_keystrokes("home shift-f10");
    assert_eq!(menu_state(&view, cx), (true, false));
    cx.simulate_keystrokes("escape");
    assert_eq!(
        menu_state(&view, cx),
        (false, true),
        "closed for keys and clicks at once, still fading out"
    );
    // Keys reach the list right away.
    cx.simulate_keystrokes("down");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)),
        Some("b.txt".into())
    );
    after_exit(cx);
    assert_eq!(menu_state(&view, cx), (false, false), "gone after the fade");

    // Reopening while one fades: the new menu is what stays.
    cx.simulate_keystrokes("shift-f10 escape shift-f10");
    assert!(menu_state(&view, cx).0);
    after_exit(cx);
    assert!(menu_state(&view, cx).0, "the new menu stays open");

    // A command: no ghost.
    view.update(cx, |v, _| v.closing_menu = None);
    run_command(&view, cx, MenuCommand::Open);
    assert_eq!(
        menu_state(&view, cx),
        (false, false),
        "a choice closes it instantly"
    );
}

/// Closing Quick Look fades it out; the item stays loaded until the fade is over, then goes (the
/// preview pane is off). Opening it again during the fade drops the ghost.
#[gpui::test]
fn quick_look_fades_out_and_keeps_its_item_until_then(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a.txt", 1), file("b.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    assert!(!view.update(cx, |v, _| v.preview_pane));

    cx.simulate_keystrokes("home space");
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.quick_look && v.preview.is_some()));
    cx.simulate_keystrokes("escape");
    let (open, ghost, item) = view.update(cx, |v, _| {
        (
            v.quick_look,
            v.closing_quick_look.is_some(),
            v.preview.is_some(),
        )
    });
    assert!(!open && ghost, "closed, fading out");
    assert!(item, "the ghost still has its item");
    after_exit(cx);
    let (ghost, item) = view.update(cx, |v, _| {
        (v.closing_quick_look.is_some(), v.preview.is_some())
    });
    assert!(!ghost && !item, "both gone after the fade");

    cx.simulate_keystrokes("space escape space");
    let (open, ghost) = view.update(cx, |v, _| (v.quick_look, v.closing_quick_look.is_some()));
    assert!(open && !ghost, "reopened: no ghost under it");
    after_exit(cx);
    assert!(
        view.update(cx, |v, _| v.quick_look && v.preview.is_some()),
        "the late cleanup leaves an open Quick Look alone"
    );
}

/// The view options flyout fades out when closed by its button, and reopening during the fade
/// shows the live flyout only.
#[gpui::test]
fn view_options_flyout_fades_out(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_many(r"C:\v", 10);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\v");
    wait_until(cx, &view, "load", loaded(r"C:\v"));
    let toggle = |cx: &mut VisualTestContext| {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("view-options")
            .expect("the view options button is drawn");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };
    let state = |cx: &mut VisualTestContext| {
        view.update(cx, |v, _| {
            (
                v.view_options_open_for_test(),
                v.closing_view_options.is_some(),
            )
        })
    };
    toggle(cx);
    assert_eq!(state(cx), (true, false));
    toggle(cx);
    assert_eq!(state(cx), (false, true));
    toggle(cx);
    assert_eq!(state(cx), (true, false), "reopened: the ghost is dropped");
    toggle(cx);
    after_exit(cx);
    assert_eq!(state(cx), (false, false));
}

/// A notice that goes away fades out where it was, unless another notice takes its place.
#[gpui::test]
fn notices_fade_out_when_they_go_away(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\n", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\n");
    wait_until(cx, &view, "load", loaded(r"C:\n"));
    let set = |cx: &mut VisualTestContext, notice: Option<&str>| {
        view.update(cx, |v, cx| {
            v.notice = notice.map(str::to_owned);
            cx.notify();
        });
        cx.run_until_parked();
    };
    let ghost = |cx: &mut VisualTestContext| {
        view.update(cx, |v, _| {
            v.closing_notice.as_ref().map(|g| g.value.0.clone())
        })
    };
    set(cx, Some("Copied 3 items"));
    assert_eq!(ghost(cx), None);
    set(cx, None);
    assert_eq!(ghost(cx).as_deref(), Some("Copied 3 items"));
    set(cx, Some("Moved 1 item"));
    assert_eq!(ghost(cx), None, "a new notice replaces the ghost");
    set(cx, None);
    after_exit(cx);
    assert_eq!(ghost(cx), None, "gone after the fade");
}

/// Picking another layout plays the list's crossfade; a size change within Icons (direct
/// manipulation) and a folder's remembered view don't.
#[gpui::test]
fn only_a_chosen_layout_change_animates(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_many(r"C:\l", 20);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\l");
    wait_until(cx, &view, "load", loaded(r"C:\l"));
    let serial = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.view_cue.serial());
    let before = serial(cx);
    cx.simulate_keystrokes("ctrl-shift-2");
    assert_eq!(view.update(cx, |v, _| v.view_mode), ViewMode::Icons);
    let picked = serial(cx);
    assert_ne!(picked, before, "Details to Icons plays");
    cx.simulate_keystrokes("ctrl-shift-3");
    assert_eq!(serial(cx), picked, "another icon size doesn't");
    cx.simulate_keystrokes("ctrl-shift-6");
    assert_ne!(serial(cx), picked, "back to Details plays");
}

/// Cancelling a confirmation fades it out; confirming closes it at once and runs the operation.
#[gpui::test]
fn cancelled_confirmation_fades_out(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\c", vec![file("a.txt", 1), file("b.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\c");
    wait_until(cx, &view, "load", loaded(r"C:\c"));
    let state = |cx: &mut VisualTestContext| {
        view.update(cx, |v, _| {
            (v.confirm_title().is_some(), v.closing_confirm.is_some())
        })
    };
    cx.simulate_keystrokes("home shift-delete");
    assert_eq!(state(cx), (true, false));
    cx.simulate_keystrokes("escape");
    assert_eq!(state(cx), (false, true), "cancelled, fading out");
    after_exit(cx);
    assert_eq!(state(cx), (false, false));
    cx.simulate_keystrokes("shift-delete enter");
    assert_eq!(state(cx), (false, false), "confirming leaves no ghost");
}

/// With animations off (Windows "Animation effects" off, and tests by default) nothing lingers:
/// dismissed surfaces vanish at once and no cue plays.
#[gpui::test]
fn reduced_motion_leaves_no_ghosts(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\r", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    cx.simulate_keystrokes("home shift-f10 escape");
    assert_eq!(menu_state(&view, cx), (false, false));
    cx.simulate_keystrokes("space escape");
    assert!(view.update(cx, |v, _| v.closing_quick_look.is_none()));
    cx.simulate_keystrokes("ctrl-shift-2");
    assert!(!view.update(cx, |v, _| v.view_cue.live_at(std::time::Instant::now())));
}
