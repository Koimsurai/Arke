//! GPUI tests for presses whose release went to another window (`press_guard.rs`): a program
//! opened by a double-click that comes up under the cursor, a release outside an inactive window.
//! The press must not stay armed, so no later move starts a drag and no later click drops one.

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, point, px};

use super::*;

/// A folder with `sub` above `app.json`, listed in a workspace; returns the centre of each row.
fn obsidian_folder(
    cx: &mut TestAppContext,
) -> (
    Arc<FakeShell>,
    Entity<Explorer>,
    &mut VisualTestContext,
    Point<Pixels>,
    Point<Pixels>,
) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\vault\.obsidian", vec![dir("sub"), file("app.json", 1)]);
    fs.insert_dir(r"C:\vault\.obsidian\sub", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\vault\.obsidian");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\vault\.obsidian"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let on_sub = point(bounds.left() + px(60.), bounds.top() + row * 0.5);
    let on_file = point(bounds.left() + px(60.), bounds.top() + row * 1.5);
    (shell, view, cx, on_sub, on_file)
}

fn press(cx: &mut VisualTestContext, at: Point<Pixels>, button: MouseButton, click_count: usize) {
    cx.simulate_event(MouseDownEvent {
        position: at,
        modifiers: Modifiers::none(),
        button,
        click_count,
        first_mouse: false,
    });
    cx.run_until_parked();
}

fn release(cx: &mut VisualTestContext, at: Point<Pixels>, button: MouseButton, click_count: usize) {
    cx.simulate_event(MouseUpEvent {
        position: at,
        modifiers: Modifiers::none(),
        button,
        click_count,
    });
    cx.run_until_parked();
}

fn hover(cx: &mut VisualTestContext, at: Point<Pixels>, button: Option<MouseButton>) {
    cx.simulate_mouse_move(at, button, Modifiers::none());
    cx.run_until_parked();
}

fn dragging(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| cx.has_active_drag())
}

/// Waits a little for anything the shell might still be asked to do.
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..10 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The forum report: a double-click opened `app.json` in an editor that came up under the
/// cursor, so the second release went to the editor. Back in Arke, moving the mouse with no
/// button down started a drag of the file, and the next click anywhere moved it there.
#[gpui::test]
fn a_double_click_whose_release_went_to_the_opened_app_starts_no_drag(cx: &mut TestAppContext) {
    let (shell, _view, cx, on_sub, on_file) = obsidian_folder(cx);
    press(cx, on_file, MouseButton::Left, 1);
    release(cx, on_file, MouseButton::Left, 1);
    press(cx, on_file, MouseButton::Left, 2);
    wait_shell_in(cx, "the double-click opens the file", || {
        !shell.opened.lock().is_empty()
    });
    // The release went to the editor. Back over Arke, no button is down.
    hover(cx, on_file + point(px(20.), px(3.)), None);
    hover(cx, on_sub, None);
    assert!(!dragging(cx), "a move with no button down starts no drag");
    // A plain click on the folder only selects it.
    press(cx, on_sub, MouseButton::Left, 1);
    release(cx, on_sub, MouseButton::Left, 1);
    settle(cx);
    assert!(!dragging(cx));
    assert!(shell.ops().is_empty(), "nothing moved: {:?}", shell.ops());
}

/// A drag whose release never arrived ends on the next move that reports no button, with
/// nothing dropped; later clicks over a folder drop nothing.
#[gpui::test]
fn a_drag_whose_release_was_lost_ends_on_the_next_move(cx: &mut TestAppContext) {
    let (shell, _view, cx, on_sub, on_file) = obsidian_folder(cx);
    press(cx, on_file, MouseButton::Left, 1);
    hover(
        cx,
        on_file + point(px(30.), px(4.)),
        Some(MouseButton::Left),
    );
    hover(cx, on_sub, Some(MouseButton::Left));
    assert!(dragging(cx), "the drag started");
    // Released over another program's window: Arke never hears of it.
    hover(cx, on_sub + point(px(5.), px(2.)), None);
    assert!(!dragging(cx), "no button down: the drag is over");
    for button in [MouseButton::Left, MouseButton::Right] {
        press(cx, on_sub, button, 1);
        release(cx, on_sub, button, 1);
    }
    settle(cx);
    assert!(shell.ops().is_empty(), "nothing moved: {:?}", shell.ops());
}

/// Pressed again with no move in between (the release went elsewhere and the cursor came back
/// where it was): the new press is a press of its own, not the drop of the old drag.
#[gpui::test]
fn a_new_press_ends_a_drag_whose_release_was_lost(cx: &mut TestAppContext) {
    let (shell, _view, cx, on_sub, on_file) = obsidian_folder(cx);
    press(cx, on_file, MouseButton::Left, 1);
    hover(
        cx,
        on_file + point(px(30.), px(4.)),
        Some(MouseButton::Left),
    );
    hover(cx, on_sub, Some(MouseButton::Left));
    assert!(dragging(cx), "the drag started");
    press(cx, on_sub, MouseButton::Left, 1);
    assert!(!dragging(cx), "the old drag ended at the new press");
    release(cx, on_sub, MouseButton::Left, 1);
    settle(cx);
    assert!(shell.ops().is_empty(), "nothing moved: {:?}", shell.ops());
}

/// Esc ends a right-button drag completely: the next left drag moves as usual rather than
/// asking what to do.
#[gpui::test]
fn escape_ends_a_right_drag_and_the_next_drag_does_not_ask(cx: &mut TestAppContext) {
    let (shell, view, cx, on_sub, on_file) = obsidian_folder(cx);
    press(cx, on_file, MouseButton::Right, 1);
    hover(
        cx,
        on_file + point(px(30.), px(4.)),
        Some(MouseButton::Right),
    );
    cx.update(|_, cx| {
        assert!(cx.has_active_drag(), "the right button started a drag");
        assert!(cx.has_global::<crate::explorer::RightDrag>());
    });
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| {
        assert!(!cx.has_active_drag());
        assert!(
            !cx.has_global::<crate::explorer::RightDrag>(),
            "nothing left of the right drag"
        );
    });
    release(cx, on_file + point(px(30.), px(4.)), MouseButton::Right, 1);
    assert!(
        view.update(cx, |v, _| v.context_menu.is_none()),
        "the release after Esc opens no menu"
    );

    press(cx, on_file, MouseButton::Left, 1);
    hover(
        cx,
        on_file + point(px(30.), px(4.)),
        Some(MouseButton::Left),
    );
    hover(cx, on_sub, Some(MouseButton::Left));
    release(cx, on_sub, MouseButton::Left, 1);
    wait_shell_in(cx, "the drop moves", || !shell.ops().is_empty());
    assert!(view.update(cx, |v, _| v.context_menu.is_none()), "no menu");
    assert_eq!(
        shell.ops(),
        [FileOp::Move {
            sources: vec![PathBuf::from(r"C:\vault\.obsidian\app.json")],
            dest: PathBuf::from(r"C:\vault\.obsidian\sub"),
        }]
    );
}

/// A right press whose release went elsewhere opens no menu later and leaves nothing armed.
#[gpui::test]
fn a_right_press_whose_release_was_lost_opens_no_menu(cx: &mut TestAppContext) {
    let (shell, view, cx, on_sub, on_file) = obsidian_folder(cx);
    press(cx, on_file, MouseButton::Right, 1);
    hover(cx, on_sub, None);
    assert!(view.update(cx, |v, _| v.right_press.is_none()));
    assert!(view.update(cx, |v, _| v.context_menu.is_none()), "no menu");
    // A left click on the item neither drags nor opens its menu.
    press(cx, on_file, MouseButton::Left, 1);
    release(cx, on_file, MouseButton::Left, 1);
    hover(cx, on_sub, None);
    settle(cx);
    assert!(!dragging(cx));
    assert!(view.update(cx, |v, _| v.context_menu.is_none()), "no menu");
    assert!(shell.ops().is_empty());
}
