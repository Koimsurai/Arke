//! GPUI tests for the keyboard on the Home page (#322): Tab brings it there with a ring on a
//! card, the arrow keys move between the cards (and on into the next section at an edge), Enter
//! opens the card and Shift+F10 opens its menu, while Tab keeps walking the window's stops.

use std::sync::Arc;

use super::*;
use crate::home::RecentFile;
use crate::settings::SettingsStore;
use crate::shell::{Place, PlaceKind};

fn drive(letter: char) -> Place {
    Place {
        label: format!("Local Disk ({letter}:)"),
        path: PathBuf::from(format!(r"{letter}:\")),
        kind: PlaceKind::Drive {
            remote: false,
            space: None,
        },
    }
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn settle(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(cx) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        refresh(cx);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn key(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    refresh(cx);
}

fn stop(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    cx.update(|window, cx| ws.read(cx).stop_for_test(window, cx))
}

fn cursor(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    ws.update(cx, |ws, cx| ws.home_cursor_for_test(cx))
}

fn bounds(cx: &mut VisualTestContext, selector: &str) -> gpui::Bounds<gpui::Pixels> {
    let id: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
}

/// The ring is drawn around the card whose text is drawn at `selector`.
fn ring_on(cx: &mut VisualTestContext, selector: &str) -> bool {
    let card = bounds(cx, selector);
    cx.debug_bounds("home-focus-ring")
        .is_some_and(|ring| ring.contains(&card.center()))
}

/// Home over `C:\work`: favorites `C:\music`, `C:\work` and `D:\games` (one row of cards), drives
/// `C:` and `D:`, and two recent files.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, Arc<FakeShell>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\", vec![dir("music"), dir("work")]);
    fs.insert_dir(r"C:\music", Vec::new());
    fs.insert_dir(r"C:\work", vec![file("a.txt", 1), file("b.txt", 1)]);
    fs.insert_dir(r"D:\", vec![dir("games")]);
    fs.insert_dir(r"D:\games", Vec::new());
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().extend([drive('C'), drive('D')]);
    let now = SystemTime::now();
    *shell.recent.lock() = ["a.txt", "b.txt"]
        .iter()
        .map(|name| RecentFile {
            path: PathBuf::from(r"C:\work").join(name),
            used: now,
        })
        .collect();
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::clone(&shell), r"C:\work");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\work")(v) && v.places.len() > 2
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.favorites = Some(vec![
                        PathBuf::from(r"C:\music"),
                        PathBuf::from(r"C:\work"),
                        PathBuf::from(r"D:\games"),
                    ]);
                },
                cx,
            );
        });
    });
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    settle(cx, "the recent files", |cx| {
        ws.update(cx, |ws, cx| ws.home_recent_for_test(cx))
            .is_some_and(|r| r.len() == 2)
    });
    (ws, shell, cx)
}

const MUSIC: &str = r"home-quick-C:\music";
const WORK: &str = r"home-quick-C:\work";
const GAMES: &str = r"home-quick-D:\games";
const DRIVE_C: &str = "home-drive-0";
const DRIVE_D: &str = "home-drive-1";
const RECENT_A: &str = r"C:\work\a.txt";
const RECENT_B: &str = r"C:\work\b.txt";

/// Tab brings the keyboard to Home with a ring on the first card; the arrows walk the grid of
/// cards (Left and Right inside a section, Up and Down to the nearest card above or below, into
/// the next section at an edge) and the recent files as a column; Tab still goes to the sidebar
/// and back, to the card it was on.
#[gpui::test]
fn arrows_move_between_the_home_cards(cx: &mut TestAppContext) {
    let (ws, _shell, cx) = window(cx);
    assert_eq!(stop(&ws, cx), "list0", "the page has the keyboard");
    assert!(
        cx.debug_bounds("home-focus-ring").is_none(),
        "opened with the mouse: no ring yet"
    );

    // The keyboard is on the first section: Shift+Tab leaves the page.
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "sidebar");
    assert!(cx.debug_bounds("home-focus-ring").is_none());
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "list0", "back on the page");
    // Shift+Tab comes in at the last section.
    assert_eq!(cursor(&ws, cx).as_deref(), Some(RECENT_A));
    assert!(cx.debug_bounds("home-focus-ring").is_some(), "a ring");
    key(cx, "home");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(MUSIC));
    assert!(
        ring_on(cx, "home-favorite music"),
        "a ring on the first card"
    );

    let walk = [
        ("right", WORK),
        ("right", GAMES),
        // The end of the section: Right goes no further.
        ("right", GAMES),
        ("left", WORK),
        // Down: the nearest drive under it.
        ("down", DRIVE_C),
        ("right", DRIVE_D),
        ("right", DRIVE_D),
        // Up from D: the favorite above it, not the first one.
        ("up", GAMES),
        ("down", DRIVE_D),
        // On into the recent files, a column: Left and Right do nothing there.
        ("down", RECENT_A),
        ("down", RECENT_B),
        ("down", RECENT_B),
        ("right", RECENT_B),
        ("up", RECENT_A),
        ("up", DRIVE_D),
        ("home", MUSIC),
        ("end", RECENT_B),
    ];
    for (keys, to) in walk {
        key(cx, keys);
        assert_eq!(cursor(&ws, cx).as_deref(), Some(to), "{keys}");
        assert!(
            cx.debug_bounds("home-focus-ring").is_some(),
            "{keys}: the ring follows"
        );
    }

    // Tab and Shift+Tab still walk the window's stops; the page keeps its card.
    key(cx, "tab");
    assert_eq!(stop(&ws, cx), "sidebar");
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "list0");
    assert_eq!(
        cursor(&ws, cx).as_deref(),
        Some(RECENT_A),
        "the last section"
    );
    key(cx, "up");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(DRIVE_D));
    assert!(ring_on(cx, "home-card Local Disk (D:)"));
}

/// Enter opens the card under the ring, in double-click mode too (the click setting is about
/// the mouse): a recent file opens, a favorite opens its folder in the tab.
#[gpui::test]
fn enter_opens_the_home_card(cx: &mut TestAppContext) {
    let (ws, shell, cx) = window(cx);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    crate::prefs::apply(&crate::settings::Settings::default());
    key(cx, "end");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(RECENT_B));
    key(cx, "enter");
    settle(cx, "the file opens", |_| {
        shell.opened.lock().contains(&PathBuf::from(RECENT_B))
    });
    assert!(cursor(&ws, cx).is_some(), "Home stays");

    key(cx, "home right");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(WORK));
    key(cx, "home");
    key(cx, "enter");
    wait_until(cx, &view, "the folder", loaded(r"C:\music"));
    assert!(
        ws.update(cx, |ws, cx| ws.home_favorites_for_test(cx))
            .is_none(),
        "Home gave way to the folder"
    );
}

/// With nothing picked and no ring showing (Home opened with the mouse), the first key only
/// brings the ring up on the first card; Enter doesn't open a card the user couldn't see picked.
#[gpui::test]
fn the_first_key_shows_the_ring(cx: &mut TestAppContext) {
    let (ws, _shell, cx) = window(cx);
    key(cx, "enter");
    assert!(
        ws.update(cx, |ws, cx| ws.home_favorites_for_test(cx))
            .is_some(),
        "Home is still open"
    );
    assert_eq!(cursor(&ws, cx).as_deref(), Some(MUSIC));
    assert!(ring_on(cx, "home-favorite music"));
    key(cx, "down");
    assert_eq!(cursor(&ws, cx).as_deref(), Some(DRIVE_C), "now it moves");
}

/// Shift+F10 (or the Menu key) opens the card's menu, the one its right click opens.
#[gpui::test]
fn shift_f10_opens_the_home_cards_menu(cx: &mut TestAppContext) {
    let (ws, _shell, cx) = window(cx);
    let menu = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.history_menu_labels_for_test())
    };
    key(cx, "home");
    key(cx, "right");
    assert_eq!(menu(&ws, cx), None);
    key(cx, "shift-f10");
    let labels = menu(&ws, cx).expect("the favorite's menu");
    assert!(!labels.is_empty());
    assert_eq!(cursor(&ws, cx).as_deref(), Some(WORK), "on the same card");
}

/// Where the keyboard is: the page with its card, or the stop.
fn at(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    match stop(ws, cx).as_str() {
        "list0" => format!("page {}", cursor(ws, cx).unwrap_or_default()),
        other => other.to_owned(),
    }
}

/// Tab from the sidebar enters the page on the first section, favorites, and walks on in the
/// order the sections are drawn: drives, recent files, then out of the page. Shift+Tab from the
/// first section goes back to the sidebar. When the mouse moved after Tab came in, so the ring
/// isn't on screen, the next Tab shows the first card instead of walking past it (#322).
#[gpui::test]
fn tab_enters_the_home_page_on_favorites(cx: &mut TestAppContext) {
    let (ws, _shell, cx) = window(cx);
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "sidebar");

    let page = |card: &str| format!("page {card}");
    let mut went = Vec::new();
    for _ in 0..4 {
        key(cx, "tab");
        went.push(at(&ws, cx));
    }
    assert_eq!(
        went,
        [
            page(MUSIC),
            page(DRIVE_C),
            page(RECENT_A),
            "sidebar".to_owned()
        ]
    );

    // Shift+Tab from the first section leaves the page.
    key(cx, "tab");
    assert_eq!(at(&ws, cx), page(MUSIC));
    key(cx, "shift-tab");
    assert_eq!(stop(&ws, cx), "sidebar");

    // The mouse moves after Tab came in: the ring is gone, and Tab brings it back on favorites.
    key(cx, "tab");
    assert_eq!(at(&ws, cx), page(MUSIC));
    cx.simulate_mouse_move(
        gpui::point(gpui::px(900.), gpui::px(800.)),
        None,
        gpui::Modifiers::none(),
    );
    refresh(cx);
    assert!(cx.debug_bounds("home-focus-ring").is_none(), "no ring");
    key(cx, "tab");
    assert_eq!(at(&ws, cx), page(MUSIC), "the first card, not the drives");
    assert!(ring_on(cx, "home-favorite music"));
    key(cx, "tab");
    assert_eq!(at(&ws, cx), page(DRIVE_C));
}

/// Tab walks the page's sections (favorites, drives, recent files), then leaves it for the
/// sidebar, and Shift+Tab walks them back: a round visits more than two stops, and the arrow keys
/// still move inside a section (#322).
#[gpui::test]
fn tab_walks_the_home_sections_then_leaves_the_page(cx: &mut TestAppContext) {
    let (ws, _shell, cx) = window(cx);
    // The ring is up on the first card.
    key(cx, "home");
    assert_eq!(at(&ws, cx), format!("page {MUSIC}"));
    let mut forward = Vec::new();
    for _ in 0..4 {
        key(cx, "tab");
        forward.push(at(&ws, cx));
    }
    let page = |card: &str| format!("page {card}");
    assert_eq!(
        forward,
        [
            page(DRIVE_C),
            page(RECENT_A),
            "sidebar".to_owned(),
            page(MUSIC)
        ]
    );
    // The arrow keys stay inside the section.
    key(cx, "right");
    assert_eq!(at(&ws, cx), page(WORK));

    let mut back = Vec::new();
    for _ in 0..5 {
        key(cx, "shift-tab");
        back.push(at(&ws, cx));
    }
    assert_eq!(
        back,
        [
            "sidebar".to_owned(),
            page(RECENT_A),
            page(DRIVE_C),
            page(MUSIC),
            "sidebar".to_owned()
        ]
    );
    let mut distinct: Vec<&String> = forward.iter().chain(&back).collect();
    distinct.sort();
    distinct.dedup();
    assert!(distinct.len() > 2, "{distinct:?}");
}
