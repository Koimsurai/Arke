//! GPUI tests for three small behaviors: closing the last tab (#253, #255), point to select in
//! single-click mode (#46) and Home following Windows' "Show recently used files" (#92).

use gpui::{Modifiers, MouseButton, point, px};

use super::*;
use crate::settings::{LastTabAction, Settings, SettingsStore};

fn single_click(on: bool) {
    crate::prefs::apply(&Settings {
        single_click: on,
        ..Settings::default()
    });
}

/// Single-click mode with pointing not selecting (#46: Windows 11's File Explorer).
fn single_click_without_pointing() {
    crate::prefs::apply(&Settings {
        single_click: true,
        point_to_select: false,
        ..Settings::default()
    });
}

/// Closing the only tab keeps the window and shows Home when the setting says so; the default
/// still closes the window.
#[gpui::test]
fn closing_the_last_tab_can_open_home(cx: &mut TestAppContext) {
    assert_eq!(Settings::default().last_tab, LastTabAction::CloseWindow);
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.last_tab = LastTabAction::Home, cx);
        });
    });
    ws.update_in(cx, |ws, window, cx| ws.close_tab(0, window, cx));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx)).len(), 1);
    assert_eq!(
        ws.update(cx, |ws, _| ws.shown_page_for_test()),
        Some(rust_i18n::t!("home.title").into_owned()),
        "the tab shows Home"
    );
    // Closing it again while Home shows leaves it be.
    ws.update_in(cx, |ws, window, cx| ws.close_tab(0, window, cx));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx)).len(), 1);
}

/// Single-click mode: resting the pointer on an item selects only it and moves the cursor there;
/// a held modifier or button, or the setting being off, selects nothing.
#[gpui::test]
fn resting_on_an_item_selects_it_in_single_click_mode(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\o",
        vec![file("a.txt", 1), file("b.txt", 1), file("c.txt", 1)],
    );
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\o");
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let over = |n: f32| point(bounds.left() + px(60.), bounds.top() + row * (n + 0.5));
    let rest = |cx: &mut VisualTestContext| {
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
    };
    let selected = |cx: &mut VisualTestContext| view.update(cx, |v, _| selected_names(v));

    // Off: nothing happens.
    single_click(false);
    cx.simulate_mouse_move(over(1.), None, Modifiers::none());
    rest(cx);
    assert!(selected(cx).is_empty());

    single_click(true);
    // Not yet: the hover time hasn't passed.
    cx.simulate_mouse_move(over(1.), None, Modifiers::none());
    cx.run_until_parked();
    assert!(selected(cx).is_empty(), "selects only after the hover time");
    rest(cx);
    assert_eq!(selected(cx), ["b.txt"]);
    assert_eq!(view.update(cx, |v, _| v.cursor), Some(1));

    // Moving on and resting again moves the selection.
    cx.simulate_mouse_move(over(2.), None, Modifiers::none());
    rest(cx);
    assert_eq!(selected(cx), ["c.txt"]);

    // With Ctrl down it only hovers.
    cx.simulate_mouse_move(over(0.), None, Modifiers::control());
    rest(cx);
    assert_eq!(selected(cx), ["c.txt"]);

    // A move with the button down (a drag or marquee) doesn't select either.
    cx.simulate_mouse_move(over(0.), Some(MouseButton::Left), Modifiers::none());
    rest(cx);
    assert_eq!(selected(cx), ["c.txt"]);

    // Pointing set not to select (#46): resting only hovers.
    single_click_without_pointing();
    cx.simulate_mouse_move(over(0.), None, Modifiers::none());
    rest(cx);
    assert_eq!(selected(cx), ["c.txt"]);
    single_click(false);
}

/// Home hides the recent files when Windows' "Show recently used files" is off, and doesn't read
/// the list at all.
#[gpui::test]
fn home_hides_recent_files_when_windows_does(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    shell.recent.lock().push(crate::home::RecentFile {
        path: PathBuf::from(r"C:\docs\x.txt"),
        used: SystemTime::now(),
    });
    shell
        .recent_off
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\t");
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if ws
            .update(cx, |ws, cx| ws.home_recent_for_test(cx))
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "home never loaded");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        ws.update(cx, |ws, cx| ws.home_recent_shown_for_test(cx)),
        Some(false)
    );
    assert_eq!(
        ws.update(cx, |ws, cx| ws.home_recent_for_test(cx)),
        Some(Vec::new())
    );
    assert_eq!(*shell.recent_limit.lock(), None, "the list wasn't read");
}
