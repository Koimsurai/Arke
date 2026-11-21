//! GPUI tests for the right-click edit menu of text boxes (rename box, address bar editor).

use gpui::{Focusable as _, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, point, px};

use super::*;
use crate::text_input::{EditCommand, TextInput};

fn rename_setup(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        Path::new(r"C:\r"),
        vec![file("report.docx", 1), file("a.txt", 1)],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(700.)));
    cx.run_until_parked();
    (view, cx)
}

fn rename_input(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Entity<TextInput> {
    view.update(cx, |v, _| {
        v.renaming.as_ref().expect("renaming").input.clone()
    })
}

fn path_input(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Entity<TextInput> {
    view.update(cx, |v, _| v.path_input_for_test())
}

fn right_click_at(
    input: &Entity<TextInput>,
    offset: usize,
    cx: &mut VisualTestContext,
) -> gpui::Point<gpui::Pixels> {
    let at = input
        .read_with(cx, |input, _| input.point_at(offset))
        .expect("drawn");
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    at
}

fn rows(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> Option<Vec<(EditCommand, bool)>> {
    input.read_with(cx, |input, _| input.menu_rows())
}

/// The edit menu's window and where `command`'s row is drawn in it.
fn menu_item(
    command: EditCommand,
    cx: &mut VisualTestContext,
) -> (VisualTestContext, gpui::Bounds<gpui::Pixels>) {
    // Leaked: the lookup wants a `'static` name, and a test process is short-lived.
    let selector: &'static str = Box::leak(format!("text-input-menu-{command:?}").into_boxed_str());
    let popups = cx.update(|_, cx| crate::popup::open_popups(cx));
    assert_eq!(popups.len(), 1, "the edit menu is in a window of its own");
    let mut menu = VisualTestContext::from_window(popups[0].into(), &cx.cx);
    let bounds = menu.debug_bounds(selector).expect("menu item drawn");
    (menu, bounds)
}

fn click_item(command: EditCommand, cx: &mut VisualTestContext) {
    let (mut menu, bounds) = menu_item(command, cx);
    menu.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

fn text(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> String {
    input.read_with(cx, |input, _| input.text().to_owned())
}

fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

fn enabled(rows: &[(EditCommand, bool)]) -> Vec<EditCommand> {
    rows.iter()
        .filter(|(_, on)| *on)
        .map(|(command, _)| *command)
        .collect()
}

/// Right-click in the rename box opens the edit menu (not the file's menu), keeps the selection
/// when it lands on it, and every command acts on the text without ending the rename.
#[gpui::test]
fn rename_box_edit_menu(cx: &mut TestAppContext) {
    use EditCommand as E;
    let (view, cx) = rename_setup(cx);
    cx.simulate_keystrokes("end f2");
    let input = rename_input(&view, cx);
    cx.run_until_parked();
    // The stem "report" is selected.
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..6);

    // Inside the selection: it stays. Nothing to undo yet, nothing on the clipboard.
    right_click_at(&input, 3, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..6);
    assert!(menu_labels(&view, cx).is_none(), "not the file's menu");
    let open = rows(&input, cx).expect("edit menu open");
    assert_eq!(
        open.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        [E::Undo, E::Cut, E::Copy, E::Paste, E::Delete, E::SelectAll]
    );
    assert_eq!(
        enabled(&open),
        [E::Cut, E::Copy, E::Delete, E::SelectAll],
        "no undo, no clipboard text"
    );

    // Copy puts the selection on the clipboard and closes the menu; the rename goes on.
    click_item(E::Copy, cx);
    assert_eq!(clipboard(cx).as_deref(), Some("report"));
    assert!(rows(&input, cx).is_none());
    assert!(view.update(cx, |v, _| v.renaming.is_some()));
    assert_eq!(text(&input, cx), "report.docx");

    // Outside the selection: the caret goes there, so nothing is selected.
    right_click_at(&input, 9, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 9..9);
    let open = rows(&input, cx).expect("edit menu open");
    assert_eq!(enabled(&open), [E::Paste, E::SelectAll]);

    // Paste inserts at the caret; Undo is then available and takes it back.
    click_item(E::Paste, cx);
    assert_eq!(text(&input, cx), "report.doreportcx");
    right_click_at(&input, 1, cx);
    assert!(enabled(&rows(&input, cx).unwrap()).contains(&E::Undo));
    click_item(E::Undo, cx);
    assert_eq!(text(&input, cx), "report.docx");
    assert!(view.update(cx, |v, _| v.renaming.is_some()));

    // Select all, then cut it: the whole name goes to the clipboard.
    right_click_at(&input, 1, cx);
    click_item(E::SelectAll, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..11);
    right_click_at(&input, 1, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..11);
    click_item(E::Cut, cx);
    assert_eq!(clipboard(cx).as_deref(), Some("report.docx"));
    assert_eq!(text(&input, cx), "");

    // Delete removes only the selection.
    click_item_after_undo_and_select(&input, cx);
    assert_eq!(text(&input, cx), ".docx");
    assert!(view.update(cx, |v, _| v.renaming.is_some()));
}

/// Undo the cut, select "report", then Delete from the menu.
fn click_item_after_undo_and_select(input: &Entity<TextInput>, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(input, cx), "report.docx");
    input.update(cx, |input, cx| input.select_range(0..6, cx));
    cx.run_until_parked();
    right_click_at(input, 2, cx);
    click_item(EditCommand::Delete, cx);
}

/// Esc closes the menu and leaves the rename alone; a second Esc cancels it.
#[gpui::test]
fn escape_closes_the_menu_before_the_rename(cx: &mut TestAppContext) {
    let (view, cx) = rename_setup(cx);
    cx.simulate_keystrokes("end f2");
    let input = rename_input(&view, cx);
    cx.run_until_parked();
    right_click_at(&input, 3, cx);
    assert!(rows(&input, cx).is_some());
    cx.simulate_keystrokes("escape");
    assert!(rows(&input, cx).is_none());
    assert!(view.update(cx, |v, _| v.renaming.is_some()));
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.renaming.is_none()));
}

/// Clicking elsewhere only closes the menu.
#[gpui::test]
fn clicking_away_closes_the_menu(cx: &mut TestAppContext) {
    let (view, cx) = rename_setup(cx);
    cx.simulate_keystrokes("end f2");
    let input = rename_input(&view, cx);
    cx.run_until_parked();
    right_click_at(&input, 3, cx);
    let away = gpui::point(gpui::px(900.), gpui::px(600.));
    cx.simulate_click(away, Modifiers::none());
    cx.run_until_parked();
    assert!(rows(&input, cx).is_none());
    assert!(
        view.update(cx, |v, _| v.renaming.is_some()),
        "still renaming"
    );
}

/// The address bar: its own menu while showing the breadcrumbs, the edit menu while typing.
#[gpui::test]
fn address_bar_edit_menu_only_while_editing(cx: &mut TestAppContext) {
    use EditCommand as E;
    let (view, cx) = rename_setup(cx);

    // Not editing: right-click on the bar is still the copy address menu.
    let bar = cx.debug_bounds("path-bar").expect("path bar drawn");
    cx.simulate_mouse_down(bar.center(), MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(bar.center(), MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    let labels = menu_labels(&view, cx).expect("address menu");
    assert!(labels.contains(&t!("menu.copy_address").to_string()));
    cx.simulate_keystrokes("escape");

    // Editing: the edit menu, with the typed text to work on.
    cx.simulate_keystrokes("ctrl-l");
    assert!(view.update(cx, |v, _| v.editing_path));
    cx.simulate_input(r"D:\x");
    cx.run_until_parked();
    let input = path_input(&view, cx);
    right_click_at(&input, 2, cx);
    assert!(menu_labels(&view, cx).is_none());
    let open = rows(&input, cx).expect("edit menu open");
    assert!(enabled(&open).contains(&E::Undo), "typing can be undone");
    assert!(view.update(cx, |v, _| v.editing_path), "still editing");

    // Select all then copy: the address is on the clipboard and editing goes on.
    click_item(E::SelectAll, cx);
    right_click_at(&input, 2, cx);
    click_item(E::Copy, cx);
    assert_eq!(clipboard(cx).as_deref(), Some(r"D:\x"));
    assert!(view.update(cx, |v, _| v.editing_path), "still editing");

    // Undo takes the typing back.
    right_click_at(&input, 2, cx);
    click_item(E::Undo, cx);
    assert_eq!(
        text(&input, cx),
        r"C:\r",
        "back to the address it started with"
    );
    assert!(view.update(cx, |v, _| v.editing_path), "still editing");
}

/// A password box can't be cut or copied, and Ctrl+Z undoes typing one step at a time (a word typed
/// in a row is one step).
#[gpui::test]
fn password_and_typing_undo(cx: &mut TestAppContext) {
    use EditCommand as E;
    cx.update(crate::text_input::bind_keys);
    let (input, cx) = cx.add_window_view(|_, cx| {
        let mut input = TextInput::new("", cx);
        input.set_masked(true, cx);
        input
    });
    cx.update(|window, cx| {
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    });
    cx.simulate_input("abc");
    cx.simulate_keystrokes("left");
    cx.simulate_input("X");
    assert_eq!(text(&input, cx), "abXc");
    // Select all: still no cut or copy for a password.
    cx.simulate_keystrokes("ctrl-a");
    right_click_at(&input, 1, cx);
    let open = rows(&input, cx).expect("menu");
    assert_eq!(enabled(&open), [E::Undo, E::Delete, E::SelectAll]);
    cx.simulate_keystrokes("escape");
    // Two steps: the X, then the word.
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "abc");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "");
}

/// A text box alone in a window, focused, holding `text`.
fn lone_input<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (Entity<TextInput>, &'a mut VisualTestContext) {
    cx.update(crate::text_input::bind_keys);
    let text = text.to_owned();
    let (input, cx) = cx.add_window_view(move |_, cx| {
        let mut input = TextInput::new("", cx);
        input.set_text(text, cx);
        input
    });
    cx.update(|window, cx| {
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    });
    cx.run_until_parked();
    (input, cx)
}

fn choice(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> Option<EditCommand> {
    input.read_with(cx, |input, _| input.menu_choice())
}

/// Right-click on the empty search box: focusing it would open its scope panel, which used to
/// be drawn over the edit menu, so the menu's rows couldn't be clicked. The menu is on top and
/// works.
#[gpui::test]
fn right_click_on_the_empty_search_box_gives_a_working_edit_menu(cx: &mut TestAppContext) {
    use EditCommand as E;
    let (view, cx) = rename_setup(cx);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("report".into()));
    let input = view.update(cx, |v, _| v.filter_input_for_test());
    // Where the panel shows when the box is focused.
    cx.dispatch_action(FocusFilter);
    cx.run_until_parked();
    let panel = view
        .update(cx, |v, _| {
            v.panel_geometry_for_test(arke_core::index_query::Facet::Type)
                .0
        })
        .expect("search panel drawn");
    cx.simulate_keystrokes("escape escape");
    cx.run_until_parked();
    let filter_box = cx.debug_bounds("filter-box").expect("search box drawn");
    // Just past the magnifier, on the text.
    let at = gpui::point(filter_box.left() + gpui::px(50.), filter_box.center().y);
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(cx.update(|window, cx| view.read(cx).filter_focused_for_test(window, cx)));
    let open = rows(&input, cx).expect("edit menu open");
    assert_eq!(enabled(&open), [E::Paste]);
    // The menu is in a window of its own, over the panel and everything else in the window.
    let (_, paste) = menu_item(E::Paste, cx);
    assert!(!paste.is_empty(), "paste row drawn over {panel:?}");
    click_item(E::Paste, cx);
    assert_eq!(text(&input, cx), "report");
    assert!(rows(&input, cx).is_none());
}

/// A settings field: the edit menu from the keyboard (Shift+F10), the arrows choose a row
/// (screen readers hear it as focused) and Enter runs it.
#[gpui::test]
fn settings_field_edit_menu_from_the_keyboard(cx: &mut TestAppContext) {
    use EditCommand as E;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    let input = ws
        .update(cx, |ws, _| ws.accent_input_for_test())
        .expect("hex field");
    let hex = text(&input, cx);
    cx.update(|window, cx| {
        let handle = input.focus_handle(cx);
        window.focus(&handle, cx);
    });
    cx.simulate_keystrokes("ctrl-a shift-f10");
    // The first row that can be used: Cut (nothing to undo).
    assert_eq!(choice(&input, cx), Some(E::Cut));
    cx.simulate_keystrokes("down");
    assert_eq!(choice(&input, cx), Some(E::Copy));
    cx.simulate_keystrokes("enter");
    assert!(rows(&input, cx).is_none());
    assert_eq!(clipboard(cx).as_deref(), Some(hex.as_str()));
    assert_eq!(text(&input, cx), hex, "only copied");

    // And with the mouse: a click on a row runs it.
    right_click_at(&input, 1, cx);
    click_item(E::SelectAll, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..hex.len());
}

/// A box in a floating panel, next to another floating panel drawn after it at the same level:
/// its edit menu is over both.
#[gpui::test]
fn the_edit_menu_of_a_box_in_a_floating_panel_is_on_top(cx: &mut TestAppContext) {
    use EditCommand as E;
    use gpui::{Render, Window, anchored, deferred, div, point, prelude::*, px};
    struct Panels {
        input: Entity<TextInput>,
    }
    impl Render for Panels {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            let at = |x: f32, y: f32| point(px(x), px(y));
            div()
                .size_full()
                .child(
                    deferred(
                        anchored()
                            .position(at(20., 20.))
                            .child(div().w(px(300.)).h(px(30.)).child(self.input.clone())),
                    )
                    .with_priority(2),
                )
                .child(
                    deferred(
                        anchored().position(at(0., 40.)).child(
                            div()
                                .id("cover")
                                .occlude()
                                .w(px(800.))
                                .h(px(500.))
                                .bg(gpui::black()),
                        ),
                    )
                    .with_priority(2),
                )
        }
    }
    cx.update(crate::text_input::bind_keys);
    let (panels, cx) = cx.add_window_view(|_, cx| Panels {
        input: cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text("name", cx);
            input
        }),
    });
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    cx.run_until_parked();
    let input = panels.read_with(cx, |p, _| p.input.clone());
    right_click_at(&input, 1, cx);
    assert!(rows(&input, cx).is_some());
    click_item(E::SelectAll, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..4);
}

/// Shift+F10 and the menu key open the edit menu at the caret; the arrows go round its usable
/// rows; Esc closes it; without it, the arrows are left to whatever holds the box.
#[gpui::test]
fn the_edit_menu_works_from_the_keyboard(cx: &mut TestAppContext) {
    use EditCommand as E;
    let (input, cx) = lone_input(cx, "");
    cx.simulate_input("abc");
    cx.simulate_keystrokes("menu");
    // Undo (typing) and Select all; nothing selected, nothing on the clipboard.
    assert_eq!(choice(&input, cx), Some(E::Undo));
    cx.simulate_keystrokes("down");
    assert_eq!(choice(&input, cx), Some(E::SelectAll));
    cx.simulate_keystrokes("down");
    assert_eq!(choice(&input, cx), Some(E::Undo), "wraps around");
    cx.simulate_keystrokes("up");
    assert_eq!(choice(&input, cx), Some(E::SelectAll));
    cx.simulate_keystrokes("escape");
    assert!(rows(&input, cx).is_none());
    assert_eq!(text(&input, cx), "abc");
    cx.simulate_keystrokes("shift-f10 enter");
    assert_eq!(text(&input, cx), "", "Enter ran Undo");
    assert!(rows(&input, cx).is_none());
}

/// Going to another window closes the edit menu, as Windows' menus do.
#[gpui::test]
fn the_edit_menu_closes_when_the_window_goes_to_the_background(cx: &mut TestAppContext) {
    let (input, cx) = lone_input(cx, "abc");
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    cx.simulate_keystrokes("shift-f10");
    assert!(rows(&input, cx).is_some());
    cx.deactivate_window();
    cx.run_until_parked();
    assert!(rows(&input, cx).is_none());
}

/// A paste is an Undo step of its own: it doesn't join the word typed before it, and what is
/// typed after it doesn't join the paste.
#[gpui::test]
fn a_paste_is_its_own_undo_step(cx: &mut TestAppContext) {
    let (input, cx) = lone_input(cx, "");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".into()));
    cx.simulate_input("ab");
    cx.simulate_keystrokes("ctrl-v");
    cx.simulate_input("cd");
    assert_eq!(text(&input, cx), "abXYcd");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "abXY");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "ab");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "");
}

/// Backspace or Delete held down (or pressed in a row) undoes in one step, like typing.
#[gpui::test]
fn a_run_of_backspaces_undoes_at_once(cx: &mut TestAppContext) {
    let (input, cx) = lone_input(cx, "");
    cx.simulate_input("hello world");
    cx.simulate_keystrokes("backspace backspace backspace backspace backspace");
    assert_eq!(text(&input, cx), "hello ");
    cx.simulate_keystrokes("home delete delete");
    assert_eq!(text(&input, cx), "llo ");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "hello ");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "hello world");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "");
}

/// Ctrl+Z waits while an IME composition is under way (undoing under it broke the text), and a
/// composition given up leaves no Undo step that does nothing.
#[gpui::test]
fn undo_and_ime_composition(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler as _;
    let (input, cx) = lone_input(cx, "");
    cx.simulate_input("ab");
    let compose = |text: &str, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, text, None, window, cx);
            });
        });
    };
    let commit = |text: &str, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, text, window, cx);
            });
        });
    };
    let steps = |cx: &mut VisualTestContext| input.read_with(cx, |i, _| i.undo_steps());
    // The caret moved: what is composed next is a step of its own, not more of the typing.
    cx.simulate_keystrokes("end");
    assert_eq!(steps(cx), 1);

    // Composing: Ctrl+Z leaves it alone.
    compose("\u{3113}", cx);
    compose("\u{3113}\u{3128}", cx);
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "ab\u{3113}\u{3128}");
    commit("\u{4e2d}", cx);
    assert_eq!(text(&input, cx), "ab\u{4e2d}");
    assert_eq!(steps(cx), 2);
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(text(&input, cx), "ab");
    assert_eq!(steps(cx), 1);

    // Given up: no step is left behind.
    compose("\u{3113}", cx);
    compose("", cx);
    assert_eq!(text(&input, cx), "ab");
    assert_eq!(steps(cx), 1);
    compose("\u{3113}", cx);
    commit("", cx);
    assert_eq!(text(&input, cx), "ab");
    assert_eq!(steps(cx), 1);
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(
        text(&input, cx),
        "",
        "the typing, not a step that does nothing"
    );
}

/// The search box showing its query while not in use: a click puts the caret where it lands
/// (not everything selected), and a right-click gives the edit menu.
#[gpui::test]
fn the_search_box_holding_a_query_takes_clicks_like_a_text_box(cx: &mut TestAppContext) {
    use EditCommand as E;
    let (view, cx) = rename_setup(cx);
    let input = view.update(cx, |v, _| v.filter_input_for_test());
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("abcdefghijklmnop");
    cx.run_until_parked();
    // Down goes to the list: the box shows its query.
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    let focused = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.read(cx).filter_focused_for_test(window, cx))
    };
    assert!(!focused(cx));
    let summary = view.update(cx, |v, _| v.search_summary_for_test());
    assert!(summary.size.width > gpui::px(0.), "query shown as text");

    let mid = gpui::point(summary.left() + gpui::px(30.), summary.center().y);
    cx.simulate_click(mid, Modifiers::none());
    cx.run_until_parked();
    assert!(focused(cx));
    let at = input.read_with(cx, |i, _| i.selection());
    assert!(at.is_empty() && at.start > 0 && at.start < 16, "{at:?}");

    // Right-click on it, once it shows its query again.
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert!(!focused(cx));
    let start = gpui::point(summary.left() + gpui::px(1.), summary.center().y);
    cx.simulate_mouse_down(start, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(start, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(focused(cx));
    assert!(menu_labels(&view, cx).is_none(), "not the list's menu");
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..0);
    click_item(E::SelectAll, cx);
    assert_eq!(input.read_with(cx, |i, _| i.selection()), 0..16);
}

/// Tab completing a typed path can be taken back with Ctrl+Z (it used to wipe Undo).
#[gpui::test]
fn a_tab_completed_path_can_be_undone(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("Docs"), dir("Downloads")]);
    fs.insert_many(r"C:\p\Docs", 1);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_keystrokes("ctrl-l");
    cx.simulate_input(r"C:\p\do");
    wait_until(cx, &view, "suggestions", |v| {
        v.suggestion_names() == ["Docs", "Downloads"]
    });
    cx.simulate_keystrokes("down tab");
    assert_eq!(view.update(cx, |v, cx| v.path_text(cx)), r"C:\p\Docs\");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(view.update(cx, |v, cx| v.path_text(cx)), r"C:\p\do");
    assert!(view.update(cx, |v, _| v.editing_path));
}

/// Ctrl with the arrows, Backspace and Delete works by word, as in a Windows edit box; in a path
/// each folder name and each part of a file name is a word (#219).
#[gpui::test]
fn ctrl_moves_selects_and_erases_by_word(cx: &mut TestAppContext) {
    let (view, cx) = rename_setup(cx);
    cx.simulate_keystrokes("ctrl-l");
    let input = path_input(&view, cx);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input(r"C:\Users\me\report.final.docx");
    cx.run_until_parked();

    cx.simulate_keystrokes("ctrl-backspace");
    assert_eq!(text(&input, cx), r"C:\Users\me\report.final.");
    cx.simulate_keystrokes("ctrl-backspace ctrl-backspace");
    assert_eq!(text(&input, cx), r"C:\Users\me\");

    // Ctrl+Right goes to the start of the next word; with Shift it selects up to there.
    cx.simulate_keystrokes("home ctrl-right ctrl-shift-right");
    cx.simulate_input(r"Docs\");
    cx.run_until_parked();
    assert_eq!(text(&input, cx), r"C:\Docs\me\");

    cx.simulate_keystrokes("end ctrl-left ctrl-shift-left");
    cx.simulate_input("x");
    cx.run_until_parked();
    assert_eq!(text(&input, cx), r"C:\xme\");

    cx.simulate_keystrokes("home ctrl-delete");
    assert_eq!(text(&input, cx), r"xme\");
    cx.simulate_keystrokes("end shift-home backspace");
    assert_eq!(text(&input, cx), "");
    assert!(view.update(cx, |v, _| v.editing_path), "still editing");
}

/// Double-click selects the word under the pointer, triple-click the whole text (#284).
#[gpui::test]
fn double_click_selects_a_word(cx: &mut TestAppContext) {
    let (view, cx) = rename_setup(cx);
    cx.simulate_keystrokes("ctrl-l");
    let input = path_input(&view, cx);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input(r"C:\Users\me\report.docx");
    cx.run_until_parked();
    let bounds = input.read_with(cx, |i, _| i.bounds_for_test()).unwrap();
    // Past the end of the text: the last word.
    let at = point(bounds.right() - px(2.), bounds.center().y);
    let clicks = |count: usize, cx: &mut VisualTestContext| {
        for click_count in 1..=count {
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
    };
    clicks(2, cx);
    assert_eq!(
        input.read_with(cx, |i, _| i.selected_text_for_test()),
        "docx"
    );
    clicks(3, cx);
    assert_eq!(
        input.read_with(cx, |i, _| i.selected_text_for_test()),
        r"C:\Users\me\report.docx"
    );
}

#[test]
fn word_at_a_point() {
    use crate::text_input::word_at;
    let s = "report final.docx";
    assert_eq!(&s[word_at(s, 2)], "report");
    assert_eq!(&s[word_at(s, 0)], "report");
    assert_eq!(&s[word_at(s, 6)], "report", "just after a word");
    assert_eq!(&s[word_at(s, 9)], "final");
    assert_eq!(&s[word_at(s, s.len())], "docx");
    assert_eq!(&s[word_at(s, 12)], "final", "the dot right after it");
    assert_eq!(word_at("", 0), 0..0);
    assert_eq!(
        &"a  b"[word_at("a  b", 2)],
        "  ",
        "between words: the spaces"
    );
    let cjk = "\u{8CC7}\u{6599} x";
    assert_eq!(&cjk[word_at(cjk, 3)], "\u{8CC7}\u{6599}");
}

#[test]
fn word_boundaries() {
    use crate::text_input::{next_word, previous_word};
    let s = "hello  world";
    assert_eq!(next_word(s, 0), 7);
    assert_eq!(next_word(s, 3), 7);
    assert_eq!(next_word(s, 7), s.len());
    assert_eq!(previous_word(s, s.len()), 7);
    assert_eq!(previous_word(s, 7), 0);
    assert_eq!(previous_word(s, 6), 0);
    assert_eq!(previous_word("", 0), 0);
    assert_eq!(next_word("", 0), 0);
    // A run of CJK is one word.
    let cjk = "\u{8CC7}\u{6599}\u{593E} a";
    assert_eq!(previous_word(cjk, 9), 0);
    assert_eq!(next_word(cjk, 0), 10);
}
