//! GPUI tests for the command palette (Ctrl+Shift+P) and the Keyboard shortcuts settings
//! (`commands.rs`, `palette.rs`, `workspace_shortcuts.rs`).

use std::collections::BTreeSet;

use super::*;
use crate::commands::{COMMANDS, Overrides};
use crate::palette::LocationPalette;
use crate::settings::SettingsStore;

/// A window on `C:\a\sub`, which has a parent to go up to.
fn start(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![dir("sub")]);
    fs.insert_many(r"C:\a\sub", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\a\sub");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a\sub"));
    (ws, cx)
}

fn palette(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<LocationPalette> {
    ws.update(cx, |ws, _| ws.palette.as_ref().map(|(p, _)| p.clone()))
        .expect("the palette should be open")
}

fn labels(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    let palette = palette(ws, cx);
    palette.update(cx, |p, _| p.row_labels())
}

fn shortcuts(cx: &mut VisualTestContext) -> Overrides {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .shortcuts
            .clone()
    })
}

fn current(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> PathBuf {
    ws.update(cx, |ws, cx| {
        ws.active_view()
            .read(cx)
            .current_path()
            .map_or_default(Path::to_path_buf)
    })
}

/// Settings › Keyboard shortcuts, narrowed to the rows matching `query`.
fn open_shortcuts(ws: &Entity<Workspace>, cx: &mut VisualTestContext, query: &str) {
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.shortcuts"), cx);
    });
    cx.run_until_parked();
    let search = ws
        .update(cx, |ws, _| ws.shortcuts_search_for_test())
        .expect("the search box should be drawn");
    let focus = search.update(cx, |s, cx| gpui::Focusable::focus_handle(s, cx));
    cx.update(|window, cx| window.focus(&focus, cx));
    cx.simulate_input(query);
    cx.run_until_parked();
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    click_named(cx, selector.to_owned());
}

fn click_named(cx: &mut VisualTestContext, selector: String) {
    // Selectors are static strings; a few leaked ones in a test are harmless.
    let selector: &'static str = Box::leak(selector.into_boxed_str());
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn recording(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<(&'static str, bool)> {
    ws.update(cx, |ws, _| ws.shortcut_recording_for_test())
}

#[gpui::test]
fn the_list_agrees_with_the_keys_bound_by_default(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    cx.update(|_, cx| {
        let map = cx.key_bindings();
        let map = map.borrow();
        let mut seen = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for command in COMMANDS {
            assert!(ids.insert(command.id), "{} is listed twice", command.id);
            let bound: BTreeSet<String> = map
                .bindings_for_action(&*command.action())
                .map(|b| b.keystrokes()[0].unparse(gpui::OperatingSystem::Windows))
                .collect();
            let listed: BTreeSet<String> = command
                .default_keys()
                .iter()
                .chain(command.also_keys())
                .map(|k| (*k).to_owned())
                .collect();
            assert_eq!(bound, listed, "keys of {}", command.id);
            for key in command.default_keys() {
                assert_eq!(
                    crate::commands::canonical(key).as_deref(),
                    Some(*key),
                    "{key} is not written the way keys are stored"
                );
                assert!(seen.insert(*key), "{key} is the default of two commands");
            }
        }
    });
}

#[test]
fn every_command_is_named_in_every_language() {
    for command in COMMANDS {
        let key = format!("cmd.{}", command.id);
        for locale in ["en", "zh-TW", "zh-CN"] {
            let name = t!(&key, locale = locale).into_owned();
            assert_ne!(name, key, "{key} has no {locale} name");
        }
    }
}

#[gpui::test]
fn ctrl_shift_p_searches_commands_and_enter_runs_one(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    let palette_entity = palette(&ws, cx);
    assert!(palette_entity.update(cx, |p, _| p.is_commands()));
    assert_eq!(labels(&ws, cx).len(), 10, "all commands, the first page");

    // Fuzzy search by name, with each command's key.
    cx.simulate_input("clse tab");
    cx.run_until_parked();
    assert_eq!(
        labels(&ws, cx).first(),
        Some(&t!("cmd.close_tab").into_owned())
    );
    let shown = palette_entity.update(cx, |p, _| p.row_shortcuts());
    assert_eq!(shown.first(), Some(&Some("Ctrl+W".to_owned())));

    // Enter runs it in the window: "New tab" opens a tab beside this one.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(ws.update(cx, |ws, _| ws.palette.is_none()));
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    cx.simulate_input("new tab");
    cx.run_until_parked();
    assert_eq!(
        labels(&ws, cx).first(),
        Some(&t!("cmd.new_tab").into_owned())
    );
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(ws.update(cx, |ws, _| ws.palette.is_none()));
    assert_eq!(ws.update(cx, |ws, _| ws.tab_views().count()), 2);
}

#[gpui::test]
fn commands_run_in_the_active_pane(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    assert!(!cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.show_hidden));
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    cx.simulate_input("hidden files");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.show_hidden));

    // Up one level goes up in the pane that was active.
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    cx.simulate_input("up one level");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a"));
}

#[gpui::test]
fn recent_commands_come_first_and_a_typed_greater_than_switches(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    cx.simulate_input("refresh");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let recent = cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .recent_commands
            .clone()
    });
    assert_eq!(recent, ["refresh"]);

    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    assert_eq!(
        labels(&ws, cx).first(),
        Some(&t!("cmd.refresh").into_owned())
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // Ctrl+T is the folder box; ">" in it turns it into the command box, and deleting the ">"
    // turns it back.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    assert!(!palette(&ws, cx).update(cx, |p, _| p.is_commands()));
    cx.simulate_input(">");
    cx.run_until_parked();
    assert!(palette(&ws, cx).update(cx, |p, _| p.is_commands()));
    assert_eq!(
        labels(&ws, cx).first(),
        Some(&t!("cmd.refresh").into_owned())
    );
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert!(!palette(&ws, cx).update(cx, |p, _| p.is_commands()));

    // Ctrl+Shift+P in the open folder box switches it too.
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    assert!(palette(&ws, cx).update(cx, |p, _| p.is_commands()));
}

#[gpui::test]
fn a_recorded_key_is_stored_and_works_at_once(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    open_shortcuts(&ws, cx, "up one");
    click(cx, "shortcut-key-go_up");
    assert_eq!(recording(&ws, cx), Some(("go_up", false)));

    // A bare letter would be typing; it is not taken, and recording goes on.
    cx.simulate_keystrokes("u");
    assert_eq!(recording(&ws, cx), Some(("go_up", false)));
    assert!(shortcuts(cx).is_empty());

    cx.simulate_keystrokes("ctrl-shift-u");
    cx.run_until_parked();
    assert_eq!(recording(&ws, cx), None);
    assert_eq!(
        shortcuts(cx),
        Overrides::from([("go_up".to_owned(), vec!["ctrl-shift-u".to_owned()])])
    );

    // The folder keys work over Settings: the new key goes up, the old one no longer does.
    cx.simulate_keystrokes("alt-up");
    cx.run_until_parked();
    assert_eq!(current(&ws, cx), PathBuf::from(r"C:\a\sub"));
    cx.simulate_keystrokes("ctrl-shift-u");
    // The folder loads on its own thread.
    let deadline = Instant::now() + Duration::from_secs(20);
    while current(&ws, cx) != Path::new(r"C:\a") && Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let page = ws.update(cx, |ws, _| ws.shown_page_for_test());
    let up = cx.update(|_, cx| {
        let map = cx.key_bindings();
        map.borrow()
            .bindings_for_input(
                &[gpui::Keystroke::parse("ctrl-shift-u").unwrap()],
                &[
                    gpui::KeyContext::parse("Workspace").unwrap(),
                    gpui::KeyContext::parse("SettingsPage").unwrap(),
                ],
            )
            .0
            .len()
    });
    assert_eq!(
        (current(&ws, cx), page, up),
        (PathBuf::from(r"C:\a"), None, 1)
    );
    // Settings is a tab of its own: it closed, and the folder tab under it went up.
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a"));

    // The palette shows the new key.
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.run_until_parked();
    cx.simulate_input("up one level");
    cx.run_until_parked();
    let shown = palette(&ws, cx).update(cx, |p, _| p.row_shortcuts());
    assert_eq!(shown.first(), Some(&Some("Ctrl+Shift+U".to_owned())));
}

#[gpui::test]
fn escape_stops_recording_and_changes_nothing(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    open_shortcuts(&ws, cx, "up one");
    click(cx, "shortcut-key-go_up");
    assert!(recording(&ws, cx).is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(recording(&ws, cx), None);
    assert!(shortcuts(cx).is_empty());
    // Settings is still open: Esc only ended the recording.
    assert!(
        ws.update(cx, |ws, _| ws.shortcuts_search_for_test())
            .is_some()
    );
}

#[gpui::test]
fn a_key_another_command_has_can_be_swapped(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    open_shortcuts(&ws, cx, "up one");
    click(cx, "shortcut-key-go_up");
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    // Nothing changes until the choice is made.
    assert_eq!(recording(&ws, cx), Some(("go_up", true)));
    assert!(shortcuts(cx).is_empty());

    // Cancel keeps both as they were.
    click_named(cx, "shortcut-cancel-go_up".to_owned());
    assert_eq!(recording(&ws, cx), None);
    assert!(shortcuts(cx).is_empty());

    // Swap: Go up takes Ctrl+T and "Go to folder" takes Alt+Up.
    click(cx, "shortcut-key-go_up");
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    click_named(cx, "shortcut-swap-go_up".to_owned());
    assert_eq!(recording(&ws, cx), None);
    assert_eq!(
        shortcuts(cx),
        Overrides::from([
            ("go_up".to_owned(), vec!["ctrl-t".to_owned()]),
            ("go_to_folder".to_owned(), vec!["alt-up".to_owned()]),
        ])
    );

    // Both work in their new place.
    cx.simulate_keystrokes("alt-up");
    cx.run_until_parked();
    assert!(
        ws.update(cx, |ws, _| ws.palette.is_some()),
        "Alt+Up opens the folder box now"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\a"));
}

#[gpui::test]
fn shortcuts_reset_one_by_one_or_all_together(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.shortcuts
                        .insert("go_up".to_owned(), vec!["ctrl-shift-u".to_owned()]);
                    s.shortcuts.insert("refresh".to_owned(), Vec::new());
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    // The changes are live: Refresh has no key, Go up has the new one.
    cx.update(|_, cx| {
        let map = cx.key_bindings();
        let refresh = map
            .borrow()
            .bindings_for_action(&crate::explorer::Refresh)
            .count();
        assert_eq!(refresh, 0);
    });

    open_shortcuts(&ws, cx, "up one");
    click(cx, "shortcut-reset-go_up");
    assert_eq!(
        shortcuts(cx),
        Overrides::from([("refresh".to_owned(), Vec::new())])
    );
    cx.update(|_, cx| {
        let map = cx.key_bindings();
        assert!(
            map.borrow()
                .bindings_for_action(&crate::explorer::GoUp)
                .count()
                > 0
        );
    });

    click(cx, "shortcuts-reset-all");
    assert!(shortcuts(cx).is_empty());
    cx.update(|_, cx| {
        let map = cx.key_bindings();
        let keys: BTreeSet<String> = map
            .borrow()
            .bindings_for_action(&crate::explorer::Refresh)
            .map(|b| b.keystrokes()[0].unparse(gpui::OperatingSystem::Windows))
            .collect();
        assert_eq!(keys, BTreeSet::from(["f5".to_owned(), "ctrl-r".to_owned()]));
    });
}

#[gpui::test]
fn a_new_key_replaces_the_default_but_not_the_list_only_ones(cx: &mut TestAppContext) {
    let (_ws, cx) = start(cx);
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.shortcuts
                        .insert("go_up".to_owned(), vec!["ctrl-shift-u".to_owned()]);
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let map = cx.key_bindings();
        let keys: BTreeSet<String> = map
            .borrow()
            .bindings_for_action(&crate::explorer::GoUp)
            .map(|b| b.keystrokes()[0].unparse(gpui::OperatingSystem::Windows))
            .collect();
        // Backspace goes up in the list only; it is not one of the keys that can be changed.
        assert_eq!(
            keys,
            BTreeSet::from(["ctrl-shift-u".to_owned(), "backspace".to_owned()])
        );
    });
}

#[gpui::test]
fn keys_of_text_boxes_keep_working_whatever_is_assigned(cx: &mut TestAppContext) {
    let (ws, cx) = start(cx);
    // Ctrl+A is "select all" in a text box and in the list; give it to Refresh as well.
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.shortcuts.insert("select_all".to_owned(), Vec::new());
                    s.shortcuts
                        .insert("refresh".to_owned(), vec!["ctrl-a".to_owned()]);
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    cx.simulate_keystrokes("ctrl-f");
    cx.run_until_parked();
    cx.simulate_input("abc");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("x");
    cx.run_until_parked();
    let filter = view.update(cx, |v, _| v.filter_input_for_test());
    assert_eq!(filter.update(cx, |f, _| f.committed_text()), "x");
}

#[test]
fn only_changes_are_stored_and_a_default_is_not_a_change() {
    let mut overrides = Overrides::new();
    crate::commands::assign(&mut overrides, "go_up", "ctrl-shift-u", false);
    assert_eq!(overrides.len(), 1);
    // Back to the default: nothing left to store.
    crate::commands::assign(&mut overrides, "go_up", "alt-up", false);
    assert!(overrides.is_empty());

    // A swap changes both commands; swapping back leaves nothing.
    crate::commands::assign(&mut overrides, "go_up", "ctrl-t", true);
    assert_eq!(overrides["go_up"], ["ctrl-t"]);
    assert_eq!(overrides["go_to_folder"], ["alt-up"]);
    crate::commands::assign(&mut overrides, "go_up", "alt-up", true);
    assert!(overrides.is_empty(), "{overrides:?}");

    // A command with two keys loses only the one that was taken.
    crate::commands::assign(&mut overrides, "go_up", "ctrl-w", true);
    assert_eq!(overrides["close_tab"], ["ctrl-f4", "alt-up"]);
}

#[test]
fn recent_commands_are_kept_newest_first_and_bounded() {
    let mut recent = Vec::new();
    for id in ["a", "b", "c", "a"] {
        crate::commands::remember(&mut recent, id);
    }
    assert_eq!(recent, ["a", "c", "b"]);
    for n in 0..20 {
        crate::commands::remember(&mut recent, &format!("c{n}"));
    }
    assert_eq!(recent.len(), 8);
    assert_eq!(recent[0], "c19");
}
