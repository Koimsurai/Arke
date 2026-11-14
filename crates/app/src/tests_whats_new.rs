//! GPUI tests for What's new (`workspace_whats_new.rs`) and the update notice: when the page
//! opens by itself, what it lists, and the notice's buttons.

use gpui::Modifiers;

use super::*;
use crate::build_info::Channel;
use crate::settings::SettingsStore;
use crate::update::{Release, UpdateState};

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings)) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

/// Starts over as a release build of `version` (what a restart after an update does), with the
/// settings the previous version left.
fn restart_as(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    version: &str,
    channel: Option<Channel>,
    first_run: bool,
) {
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, _| s.set_first_run_for_test(first_run));
        Workspace::pretend_build_for_test(version, channel, cx);
    });
    ws.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
}

fn page_versions(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Vec<(String, bool)>> {
    let page = ws.update(cx, |ws, _| ws.whats_new_for_test())?;
    Some(page.update(cx, |p, _| p.versions_for_test()))
}

fn shown_page(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    ws.update(cx, |ws, _| ws.shown_page_for_test())
}

fn tab_count(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx).len())
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} should be drawn"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// After an update the page opens once, in a tab of its own after the restored ones, listing
/// what changed since the version that ran before; the next start doesn't open it again.
#[gpui::test]
fn opens_once_after_an_update(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.run_until_parked();
    assert_eq!(shown_page(&ws, cx), None, "a dev build never opens it");
    change(cx, |s| s.whats_new_seen = Some("0.2.0".into()));

    restart_as(&ws, cx, "0.3.0", Some(Channel::Stable), false);
    assert_eq!(
        shown_page(&ws, cx),
        Some(t!("whats_new.title").into_owned())
    );
    assert_eq!(tab_count(&ws, cx), 2, "a tab of its own");
    assert_eq!(
        page_versions(&ws, cx),
        Some(vec![("0.3.0".to_string(), true)]),
        "only what changed since 0.2.0, open"
    );
    assert_eq!(settings(cx).whats_new_seen.as_deref(), Some("0.3.0"));

    // Esc closes it with its tab.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(shown_page(&ws, cx), None);
    assert_eq!(tab_count(&ws, cx), 1);

    restart_as(&ws, cx, "0.3.0", Some(Channel::Stable), false);
    assert_eq!(shown_page(&ws, cx), None, "once per version");
}

/// Skipped versions are all listed, newest first and open; older ones folded until clicked.
#[gpui::test]
fn lists_every_version_since_the_last_one_run(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    change(cx, |s| s.whats_new_seen = Some("0.1.1".into()));
    restart_as(&ws, cx, "0.3.0", Some(Channel::Stable), false);
    assert_eq!(
        page_versions(&ws, cx),
        Some(vec![
            ("0.3.0".to_string(), true),
            ("0.2.0".to_string(), false)
        ])
    );
    click(cx, "whats-new-section-1");
    assert_eq!(
        page_versions(&ws, cx),
        Some(vec![
            ("0.3.0".to_string(), true),
            ("0.2.0".to_string(), true)
        ])
    );
}

/// Not on a first install, not for nightly builds, and not when turned off; the version is
/// remembered all the same, so turning it back on doesn't show old news.
#[gpui::test]
fn stays_closed_for_a_new_install_nightly_or_when_off(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");

    restart_as(&ws, cx, "0.3.0", Some(Channel::Stable), true);
    assert_eq!(shown_page(&ws, cx), None, "first install");
    assert_eq!(settings(cx).whats_new_seen.as_deref(), Some("0.3.0"));

    restart_as(
        &ws,
        cx,
        "0.4.0-nightly.20261120",
        Some(Channel::Nightly),
        false,
    );
    assert_eq!(shown_page(&ws, cx), None, "nightly");

    change(cx, |s| {
        s.whats_new_seen = Some("0.2.0".into());
        s.whats_new_auto = false;
    });
    restart_as(&ws, cx, "0.3.0", Some(Channel::Stable), false);
    assert_eq!(shown_page(&ws, cx), None, "turned off");
    assert_eq!(settings(cx).whats_new_seen.as_deref(), Some("0.3.0"));
}

/// Settings › About: "Show" opens every version, the newest open; the switch turns the
/// automatic page off.
#[gpui::test]
fn settings_about_shows_it_and_turns_it_off(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(3000.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.about"), cx);
    });
    cx.run_until_parked();

    assert!(settings(cx).whats_new_auto, "on by default");
    click(cx, "whats-new-auto");
    assert!(!settings(cx).whats_new_auto);

    click(cx, "whats-new-show");
    assert_eq!(
        shown_page(&ws, cx),
        Some(t!("whats_new.title").into_owned())
    );
    let versions = page_versions(&ws, cx).unwrap();
    let names: Vec<&str> = versions.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(names, ["0.3.0", "0.2.0", "0.1.1", "0.1.0"]);
    assert!(
        versions[0].1 && !versions[1].1,
        "newest open, the rest folded"
    );
}

fn release(version: &str) -> Release {
    Release {
        version: version.into(),
        exe_url: format!("https://example.test/arke-{version}.exe"),
        exe_name: format!("arke-{version}.exe"),
        size: 1,
        sha_url: None,
        notes: "### Features\n\n- Something new\n\nVirusTotal scans of this release:\n[arke.exe](https://www.virustotal.com/gui/file/x)".into(),
        page: format!("https://example.test/v{version}"),
    }
}

/// A found update shows a corner notice; What's new opens its notes in Arke (the update first,
/// then this version), and closing the notice hides it until a newer version comes.
#[gpui::test]
fn update_notice_shows_the_notes_and_can_be_closed(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.update(|_, cx| Workspace::pretend_build_for_test("0.3.0", Some(Channel::Stable), cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("update-notice").is_none(),
        "nothing found yet"
    );

    cx.update(|_, cx| UpdateState::set_for_test(cx, UpdateState::Available(release("0.4.0"))));
    cx.run_until_parked();
    assert!(cx.debug_bounds("update-notice").is_some());

    click(cx, "update-notice-notes");
    assert_eq!(
        shown_page(&ws, cx),
        Some(t!("whats_new.title").into_owned())
    );
    assert_eq!(
        page_versions(&ws, cx),
        Some(vec![
            ("0.4.0".to_string(), true),
            ("0.3.0".to_string(), false)
        ])
    );
    let page = ws.update(cx, |ws, _| ws.whats_new_for_test()).unwrap();
    assert!(
        cx.debug_bounds("whats-new-install").is_some(),
        "the page offers the download too"
    );
    drop(page);

    click(cx, "update-notice-close");
    assert!(cx.debug_bounds("update-notice").is_none(), "closed");
    assert_eq!(settings(cx).update_dismissed.as_deref(), Some("0.4.0"));

    cx.update(|_, cx| UpdateState::set_for_test(cx, UpdateState::Available(release("0.5.0"))));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("update-notice").is_some(),
        "a newer version shows it again"
    );
}

/// The corner cards stay in the bottom right while they slide in: the slide sets `left` and `top`,
/// which on the placed box itself pinned it to the top left, as tall as the window.
#[gpui::test]
fn corner_cards_stay_in_the_corner_with_motion_on(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    crate::motion::force_for_test(true);
    cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1000.)));
    let in_corner = |cx: &mut VisualTestContext, id: &'static str| {
        for wait in [0, 50, 400] {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(wait));
            cx.run_until_parked();
            let b = cx
                .debug_bounds(id)
                .unwrap_or_else(|| panic!("{id} should be drawn"));
            assert!(
                b.origin.x > gpui::px(1000.) && b.size.height < gpui::px(400.),
                "{id} after {wait} ms: {b:?}"
            );
        }
    };
    cx.update(|_, cx| UpdateState::set_for_test(cx, UpdateState::Available(release("0.4.0"))));
    in_corner(cx, "update-notice");
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.set_first_run_for_test(true);
            cx.notify();
        });
    });
    in_corner(cx, "welcome");
    crate::motion::force_for_test(false);
}

/// Picking another channel in Settings drops the old channel's answer right away; before, the
/// page kept saying "up to date" until the next check, so switching seemed to do nothing.
/// (Test builds have no releases repo, so this takes the no-check path.)
#[gpui::test]
fn switching_the_channel_drops_the_old_answer(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(3000.)));
    change(cx, |st| st.update_channel_choice = Some(Channel::Stable));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.about"), cx);
    });
    cx.run_until_parked();
    click(cx, "update");
    cx.update(|_, cx| UpdateState::set_for_test(cx, UpdateState::UpToDate));
    cx.run_until_parked();

    click(cx, "update-channel");
    click(cx, "update-channel-0");
    assert!(
        matches!(
            cx.update(|_, cx| UpdateState::get(cx)),
            UpdateState::UpToDate
        ),
        "picking the same channel keeps the answer"
    );

    click(cx, "update-channel");
    click(cx, "update-channel-1");
    assert_eq!(settings(cx).update_channel(), Channel::Rc);
    assert!(matches!(
        cx.update(|_, cx| UpdateState::get(cx)),
        UpdateState::Idle
    ));

    // A download in progress isn't thrown away by a change of mind.
    cx.update(|_, cx| {
        UpdateState::set_for_test(cx, UpdateState::Downloading(release("0.4.0"), 1, 2));
    });
    click(cx, "update-channel");
    click(cx, "update-channel-0");
    assert!(matches!(
        cx.update(|_, cx| UpdateState::get(cx)),
        UpdateState::Downloading(..)
    ));
}
