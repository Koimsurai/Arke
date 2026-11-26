//! GPUI tests for the Settings search box (#400, `workspace_settings_search.rs`).

use super::*;

/// A window on `C:\s`, a little shorter than the Files page, with Settings open.
fn settings_window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\s");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(700.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    (ws, cx)
}

fn search_box(
    ws: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<crate::text_input::TextInput> {
    ws.update(cx, |ws, _| ws.settings_search_for_test())
        .expect("Settings is open")
}

fn in_box(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    let input = search_box(ws, cx);
    cx.update(|window, cx| gpui::Focusable::focus_handle(&input, cx).is_focused(window))
}

fn typed(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let input = search_box(ws, cx);
    input.update(cx, |i, _| i.text().to_owned())
}

/// Draws a few frames (the lit row scrolls into view on the frame after it is first drawn).
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

/// Ctrl+F, then `query` typed into the box.
fn search(ws: &Entity<Workspace>, cx: &mut VisualTestContext, query: &str) -> Vec<(usize, String)> {
    cx.simulate_keystrokes("ctrl-f");
    cx.run_until_parked();
    assert!(in_box(ws, cx), "Ctrl+F puts the cursor in the search box");
    cx.simulate_input(query);
    settle(cx);
    hits(ws, cx)
}

fn hits(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<(usize, String)> {
    ws.update(cx, |ws, _| ws.settings_search_hits_for_test())
}

fn target(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> (usize, Option<String>) {
    ws.update(cx, |ws, _| ws.settings_search_target_for_test())
        .expect("Settings is open")
}

/// The names of the pages, in order.
fn page_names() -> Vec<String> {
    [
        "settings.appearance",
        "settings.files",
        "settings.sorting",
        "settings.startup",
        "settings.search",
        "settings.context_menu",
        "settings.shortcuts",
        "settings.about",
    ]
    .iter()
    .map(|key| t!(*key).into_owned())
    .collect()
}

/// The same setting is found by its name in Traditional Chinese and by its English name, so an
/// English "search X in Settings" works in a Chinese UI.
#[gpui::test]
fn chinese_and_english_find_the_same_setting(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\s");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    rust_i18n::set_locale("zh-TW");
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    let shown = t!("settings.show_hidden").into_owned();
    assert_eq!(shown, "顯示隱藏的項目", "the UI is in Traditional Chinese");
    let want = (1, shown.clone());

    // Part of the Chinese name: the words after "show".
    let part: String = shown.chars().skip(2).collect();
    let chinese = search(&ws, cx, &part);
    assert!(chinese.contains(&want), "found in Chinese: {chinese:?}");

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(typed(&ws, cx), "");
    cx.simulate_input("hidden ITEMS");
    settle(cx);
    let english = hits(&ws, cx);
    assert!(english.contains(&want), "found in English: {english:?}");
    assert_eq!(
        english.first(),
        chinese.first(),
        "both put the same setting first"
    );
    rust_i18n::set_locale("en");
}

/// Every page's rows are found on that page (each page notes its rows under its own index).
#[gpui::test]
fn every_page_is_searched(cx: &mut TestAppContext) {
    let (ws, cx) = settings_window(cx);
    let cases = [
        (0, t!("settings.theme")),
        (1, t!("settings.show_hidden")),
        (1, t!("settings.fast_copy")),
        (2, t!("settings.sort_by")),
        (3, t!("settings.autostart")),
        (4, t!("settings.global_search")),
        (5, t!("openwith.title")),
        (6, t!("settings.shortcuts")),
        (7, t!("whats_new.auto")),
    ];
    let input = search_box(&ws, cx);
    for (page, title) in cases {
        input.update(cx, |i, cx| i.set_text(title.to_string(), cx));
        settle(cx);
        let found = hits(&ws, cx);
        assert!(
            found.contains(&(page, title.to_string())),
            "{title} on page {page}: {found:?}"
        );
    }
}

/// Enter goes to the hit: its page shows, the box empties, and the row is lit and scrolled into
/// view.
#[gpui::test]
fn enter_shows_the_page_and_lights_the_setting(cx: &mut TestAppContext) {
    let (ws, cx) = settings_window(cx);
    assert_eq!(target(&ws, cx), (0, None), "Appearance first");
    let title = t!("settings.fast_recycle").into_owned();
    let found = search(&ws, cx, &title.to_lowercase());
    assert_eq!(found.first(), Some(&(1, title.clone())), "{found:?}");
    assert!(cx.debug_bounds("settings-search-results").is_some());
    assert!(cx.debug_bounds("settings-search-target").is_none());

    cx.simulate_keystrokes("enter");
    settle(cx);
    assert_eq!(target(&ws, cx), (1, Some(title)), "Files, with the row lit");
    assert_eq!(typed(&ws, cx), "", "the box empties");
    assert!(cx.debug_bounds("settings-search-results").is_none());
    let lit = cx
        .debug_bounds("settings-search-target")
        .expect("the row is lit");
    let viewport = cx.update(|window, _| window.viewport_size());
    assert!(
        lit.top() >= gpui::px(0.) && lit.bottom() <= viewport.height,
        "the row is scrolled into view: {lit:?} in {viewport:?}"
    );
    let (scrolled, _) = ws
        .update(cx, |ws, _| ws.settings_scroll_for_test())
        .unwrap();
    assert!(scrolled < gpui::px(0.), "the page scrolled down to it");

    // Another page from the categories: nothing is lit there.
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.sorting"), cx);
    });
    settle(cx);
    assert!(cx.debug_bounds("settings-search-target").is_none());
}

/// Up and Down move through the hits; Enter goes to the one picked. Clicking a hit goes there too.
#[gpui::test]
fn arrows_pick_a_hit_and_a_click_goes_there(cx: &mut TestAppContext) {
    let (ws, cx) = settings_window(cx);
    let found = search(&ws, cx, "sort");
    assert!(found.len() >= 2, "{found:?}");
    cx.simulate_keystrokes("down down up");
    cx.run_until_parked();
    assert!(in_box(&ws, cx), "the arrows leave the cursor in the box");
    cx.simulate_keystrokes("enter");
    settle(cx);
    let (page, title) = found[1].clone();
    let lit = (!page_names().contains(&title)).then_some(title);
    assert_eq!(target(&ws, cx), (page, lit));

    let found = search(&ws, cx, &t!("settings.autostart").to_lowercase());
    assert_eq!(found.first().map(|h| h.0), Some(3), "{found:?}");
    let bounds = cx
        .debug_bounds("settings-search-hit 0")
        .expect("the first hit is drawn");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    settle(cx);
    assert_eq!(
        target(&ws, cx),
        (3, Some(t!("settings.autostart").into_owned()))
    );
}

/// Esc empties the box and the page shows again; Esc in the empty box gives the focus back to the
/// page, and the next Esc closes Settings as before.
#[gpui::test]
fn escape_clears_then_leaves(cx: &mut TestAppContext) {
    let (ws, cx) = settings_window(cx);
    let found = search(&ws, cx, "theme");
    assert!(!found.is_empty());
    assert!(cx.debug_bounds("settings-search-results").is_some());

    cx.simulate_keystrokes("escape");
    settle(cx);
    assert_eq!(typed(&ws, cx), "");
    assert!(cx.debug_bounds("settings-search-results").is_none());
    assert!(in_box(&ws, cx), "the cursor stays in the box");
    assert!(ws.update(cx, |ws, _| ws.shown_page_for_test()).is_some());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!in_box(&ws, cx));
    assert!(
        ws.update(cx, |ws, _| ws.shown_page_for_test()).is_some(),
        "Settings stays open"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.shown_page_for_test()), None);
}

/// Picking a category while searching shows that page (the box empties); nothing matching shows
/// a line saying so.
#[gpui::test]
fn a_category_ends_the_search(cx: &mut TestAppContext) {
    let (ws, cx) = settings_window(cx);
    assert!(search(&ws, cx, "zzzz no such setting").is_empty());
    assert!(cx.debug_bounds("settings-search-results").is_some());
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.files"), cx);
    });
    settle(cx);
    // The test hook only picks the page; pressing Down on the categories does what a click does.
    cx.simulate_keystrokes("f6");
    cx.run_until_parked();
    assert!(!in_box(&ws, cx));
    cx.simulate_keystrokes("down");
    settle(cx);
    assert_eq!(typed(&ws, cx), "");
    assert!(cx.debug_bounds("settings-search-results").is_none());
    assert_eq!(target(&ws, cx), (2, None));
}
