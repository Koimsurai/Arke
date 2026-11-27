//! GPUI tests for the setup guide (`workspace_welcome.rs`, `onboarding.rs`): skipping it, each
//! preset setting exactly its settings when its step is confirmed, the look applying as it is
//! picked, the folder handling held until Next, Quick access pins, opening it again from the
//! command palette and Settings, and the What's new entry after an update.

use std::time::{Duration, Instant};

use gpui::Modifiers;

use super::*;
use crate::build_info::Channel;
use crate::onboarding::Source;
use crate::settings::{Density, Settings, SettingsStore, ThemeMode};

fn start(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    start_with(cx, Arc::new(FakeShell::default()))
}

fn start_with(
    cx: &mut TestAppContext,
    shell: Arc<FakeShell>,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\o");
    // Room for the whole card, so nothing in it scrolls.
    cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(1000.)));
    cx.run_until_parked();
    (ws, cx)
}

fn first_run(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.set_first_run_for_test(true);
            cx.notify();
        });
    });
    cx.run_until_parked();
}

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut Settings)) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} should be drawn"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// The settings without what the window itself records as it goes (its tabs, place and
/// recent folders), which no step of the guide sets.
fn chosen(cx: &mut VisualTestContext) -> Settings {
    let mut s = settings(cx);
    s.session = crate::settings::Session::default();
    s.session_windows.clear();
    s.window = None;
    s.recent.clear();
    s
}

fn step(cx: &mut VisualTestContext) -> Option<usize> {
    cx.update(|_, cx| Workspace::guide_step_for_test(cx))
}

fn shown(cx: &mut VisualTestContext) -> bool {
    cx.debug_bounds("welcome").is_some()
}

/// The guide shows on a first run only, and not again once it was skipped.
#[gpui::test]
fn shows_on_a_first_run_until_skipped(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    assert!(!shown(cx), "not a first run");
    first_run(cx);
    assert!(shown(cx));
    assert_eq!(step(cx), Some(0));
    let before = chosen(cx);
    click(cx, "welcome-skip");
    assert!(!shown(cx));
    let after = chosen(cx);
    assert!(after.onboarding_done && after.start_menu_offered);
    let mut expected = before;
    expected.onboarding_done = true;
    expected.start_menu_offered = true;
    assert_eq!(after, expected, "skipping changes nothing else");
}

/// Picking a program changes nothing; its Next sets exactly that preset's settings.
#[gpui::test]
fn each_preset_applies_exactly_its_settings_on_next(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    let tiles = [
        "welcome-source-0",
        "welcome-source-1",
        "welcome-source-2",
        "welcome-source-3",
    ];
    for (tile, source) in tiles.into_iter().zip(Source::ALL) {
        // Back to the defaults and the first step for each program.
        change(cx, |s| *s = Settings::default());
        cx.dispatch_action(crate::workspace::OpenSetupGuide);
        cx.run_until_parked();
        assert_eq!(step(cx), Some(0));
        let before = chosen(cx);
        click(cx, tile);
        assert_eq!(chosen(cx), before, "{source:?}: nothing until Next");
        assert!(
            source.changes(&before).is_empty() || cx.debug_bounds("welcome-changes").is_some(),
            "{source:?}: what it changes is listed"
        );
        click(cx, "welcome-next");
        assert_eq!(step(cx), Some(1));
        let mut expected = before;
        source.apply(&mut expected);
        assert_eq!(chosen(cx), expected, "{source:?}");
    }
}

/// Next without a program goes on and changes nothing.
#[gpui::test]
fn no_program_picked_changes_nothing(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    first_run(cx);
    let before = chosen(cx);
    click(cx, "welcome-next");
    assert_eq!(step(cx), Some(1));
    assert_eq!(chosen(cx), before);
}

/// The look applies as it is picked (the window behind shows it), as in Settings; Back keeps it.
#[gpui::test]
fn the_look_applies_as_it_is_picked(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    first_run(cx);
    click(cx, "welcome-next");
    click(cx, "welcome-density-0");
    assert_eq!(settings(cx).density, Density::Compact);
    click(cx, "theme-1");
    let s = settings(cx);
    assert_eq!(s.theme, ThemeMode::Dark);
    click(cx, "welcome-back");
    assert_eq!(step(cx), Some(0));
    assert_eq!(settings(cx).density, Density::Compact);
}

/// Folder handling and the Start menu shortcut are only written by the step's Next, by the
/// same rules as Settings › Startup.
#[gpui::test]
fn folder_handling_waits_for_next(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    first_run(cx);
    click(cx, "welcome-next");
    click(cx, "welcome-next");
    assert_eq!(step(cx), Some(2));
    let before = chosen(cx);
    click(cx, "welcome-takeover-2");
    click(cx, "welcome-autostart");
    assert_eq!(chosen(cx), before, "nothing until Next");
    assert!(!before.open_folders_in_arke && !before.win_e_opens_arke);

    click(cx, "welcome-next");
    let s = settings(cx);
    assert!(s.open_folders_in_arke && s.win_e_opens_arke && s.keep_running);
    assert!(s.autostart);
    assert!(
        s.start_menu && s.start_menu_offered,
        "the shortcut is proposed"
    );
    assert_eq!(step(cx), Some(3));
}

/// Keep File Explorer: nothing is registered, and keeping Arke in the background can be turned
/// off; it can't while Arke opens folders.
#[gpui::test]
fn keep_running_can_only_go_off_without_taking_over(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    first_run(cx);
    click(cx, "welcome-next");
    click(cx, "welcome-next");
    click(cx, "welcome-takeover-1");
    click(cx, "welcome-keep-running");
    click(cx, "welcome-takeover-0");
    click(cx, "welcome-keep-running");
    click(cx, "welcome-start-menu");
    click(cx, "welcome-next");
    let s = settings(cx);
    assert!(!s.open_folders_in_arke && !s.win_e_opens_arke && !s.keep_running);
    assert!(!s.start_menu && s.start_menu_offered);
}

fn wait_for(cx: &mut VisualTestContext, what: &str, done: impl Fn(&mut VisualTestContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The last step lists Quick access pins that aren't favorites; Done adds the ticked ones and
/// closes the guide.
#[gpui::test]
fn quick_access_pins_are_added_on_done(cx: &mut TestAppContext) {
    let a = PathBuf::from(r"C:\w\a");
    let b = PathBuf::from(r"C:\w\b");
    let shell = Arc::new(FakeShell::default());
    *shell.quick_access.lock() = Some(vec![a.clone(), b.clone()]);
    let (_ws, cx) = start_with(cx, shell);
    // Arranged favorites that already know these pins, so the background sync adds nothing.
    change(cx, |s| {
        s.favorites = Some(Vec::new());
        s.quick_access_seen = Some(vec![a.clone(), b.clone()]);
    });
    first_run(cx);
    for _ in 0..3 {
        click(cx, "welcome-next");
    }
    assert_eq!(step(cx), Some(3));
    wait_for(cx, "the pins", |cx| {
        cx.debug_bounds("welcome-pin-1").is_some()
    });
    assert!(settings(cx).favorites == Some(Vec::new()), "nothing yet");
    click(cx, "welcome-pin-1");
    click(cx, "welcome-next");
    let s = settings(cx);
    assert_eq!(s.favorites, Some(vec![a]));
    assert!(!s.pinned.contains(&b));
    assert!(s.onboarding_done);
    assert!(!shown(cx));
}

/// Pins already in the favorites aren't offered again.
#[gpui::test]
fn favorites_already_pinned_are_not_offered(cx: &mut TestAppContext) {
    let a = PathBuf::from(r"C:\w\a");
    let shell = Arc::new(FakeShell::default());
    *shell.quick_access.lock() = Some(vec![a.clone()]);
    let (_ws, cx) = start_with(cx, shell);
    change(cx, |s| {
        s.favorites = Some(vec![a.clone()]);
        s.quick_access_seen = Some(vec![a.clone()]);
    });
    first_run(cx);
    for _ in 0..3 {
        click(cx, "welcome-next");
    }
    wait_for(cx, "the read", |cx| {
        cx.debug_bounds("welcome-pins-none").is_some()
    });
    assert!(cx.debug_bounds("welcome-pin-0").is_none());
    click(cx, "welcome-next");
    assert_eq!(settings(cx).favorites, Some(vec![a]), "unchanged");
}

/// Ctrl+Shift+P › Setup guide opens it again at the first step.
#[gpui::test]
fn opens_again_from_the_palette(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    change(cx, |s| s.onboarding_done = true);
    assert!(!shown(cx));
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    cx.simulate_input("setup guide");
    cx.run_until_parked();
    let first = ws.update(cx, |ws, cx| {
        ws.palette
            .as_ref()
            .map(|(p, _)| p.read(cx).row_labels())
            .and_then(|rows| rows.first().cloned())
    });
    assert_eq!(first, Some(t!("cmd.open_setup_guide").into_owned()));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shown(cx));
    assert_eq!(step(cx), Some(0));
    click(cx, "welcome-skip");
    assert!(!shown(cx));
}

/// Settings › About › Setup guide opens it again at the first step.
#[gpui::test]
fn opens_again_from_settings_about(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(3000.)));
    change(cx, |s| s.onboarding_done = true);
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.about"), cx);
    });
    cx.run_until_parked();
    assert!(!shown(cx));
    click(cx, "setup-guide-open");
    assert!(shown(cx));
    assert_eq!(step(cx), Some(0));
}

/// Someone who updates gets an entry in What's new, not the guide itself; the entry opens it,
/// and is gone once the guide was done.
#[gpui::test]
fn an_update_offers_the_guide_in_whats_new(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    change(cx, |s| s.whats_new_seen = Some("0.2.0".into()));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, _| s.set_first_run_for_test(false));
        Workspace::pretend_build_for_test("0.3.0", Some(Channel::Stable), cx);
    });
    ws.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert!(ws.update(cx, |ws, _| ws.whats_new_for_test().is_some()));
    assert!(!shown(cx), "not the guide itself");
    click(cx, "whats-new-guide-open");
    assert!(shown(cx));
    click(cx, "welcome-skip");
    assert!(cx.debug_bounds("whats-new-guide").is_none(), "done");
}

/// The entry shows after the first update that has the guide only, even if it wasn't used.
#[gpui::test]
fn the_whats_new_entry_shows_once(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    change(cx, |s| s.whats_new_seen = Some("0.2.0".into()));
    for (version, entry) in [("0.3.0", true), ("0.4.0", false)] {
        cx.update(|_, cx| {
            SettingsStore::global(cx).update(cx, |s, _| s.set_first_run_for_test(false));
            Workspace::pretend_build_for_test(version, Some(Channel::Stable), cx);
        });
        ws.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(
            ws.update(cx, |ws, _| ws.whats_new_for_test().is_some()),
            "{version}: What's new opens"
        );
        assert_eq!(
            cx.debug_bounds("whats-new-guide").is_some(),
            entry,
            "{version}"
        );
    }
    let s = settings(cx);
    assert!(s.onboarding_offered && !s.onboarding_done);
}
