//! GPUI tests for the sidebar's sections (`workspace_sidebar.rs`): Favorites, This PC and Network
//! each collapse to their heading and expand again with a click (remembered in the
//! settings), a drop on a collapsed heading still lands, and a shadow under the Home row shows
//! while the places are scrolled. Home, Favorites and This PC hide and come back from the
//! sidebar's menus, and the vertical tabs get a scrollbar once they don't fit (#323).

use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, point, px};

use super::*;
use crate::settings::{Settings, SettingsStore, SidebarSection};

fn p(name: &str) -> PathBuf {
    PathBuf::from(r"C:\w").join(name)
}

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut Settings) + 'static) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

fn current(cx: &mut VisualTestContext) -> Settings {
    cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.clone())
}

fn refresh(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn drawn(cx: &mut VisualTestContext, selector: &str) -> bool {
    let id: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    cx.debug_bounds(id).is_some()
}

fn bounds(cx: &mut VisualTestContext, selector: &str) -> Bounds<Pixels> {
    let id: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
}

fn click(cx: &mut VisualTestContext, at: gpui::Point<Pixels>) {
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 1,
    });
    cx.run_until_parked();
    refresh(cx);
}

fn collapsed(cx: &mut VisualTestContext, section: SidebarSection) -> bool {
    current(cx).is_collapsed(section)
}

/// A window on `C:\w`, whose folders `a` to `c` and `p00` to `p29` can be favorites.
fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    let mut names: Vec<String> = ["a", "b", "c"].iter().map(|n| (*n).to_owned()).collect();
    names.extend((0..30).map(|i| format!("p{i:02}")));
    fs.insert_dir(r"C:\w", names.iter().map(|n| dir(n)).collect());
    for name in &names {
        fs.insert_dir(p(name), Vec::new());
    }
    let (ws, cx) = setup_workspace(cx, fs, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    refresh(cx);
    (ws, cx)
}

/// Favorites collapses to its heading: the list goes (the Shelf under it too), the tiles stay, and
/// it is remembered. A tile dropped on the collapsed heading still goes first in the list.
#[gpui::test]
fn favorites_collapse_and_still_take_a_drop(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a"), p("b")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    assert!(drawn(cx, "pin-row-0"), "c is listed");

    let heading = bounds(cx, "pin-list-label");
    click(cx, heading.center());
    assert!(
        collapsed(cx, SidebarSection::Favorites),
        "kept in the settings"
    );
    assert!(!drawn(cx, "pin-row-0"), "the list is hidden");
    assert!(
        drawn(cx, "tile-slot-0"),
        "the tiles belong to Favorites, and stay while the list is collapsed"
    );

    // Drag the first tile onto the collapsed heading: it goes first in the list.
    let tile = bounds(cx, "tile-slot-0");
    cx.simulate_mouse_down(tile.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        tile.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    refresh(cx);
    cx.update(|_, cx| assert!(cx.has_active_drag()));
    let heading = bounds(cx, "pin-list-label").center();
    cx.simulate_mouse_move(heading, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    let at = heading + point(px(1.), px(0.));
    cx.simulate_mouse_move(at, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    // Over the heading it went into the list at once, first; no line to say where.
    assert_eq!(current(cx).pinned, vec![p("a"), p("c")]);
    assert!(!drawn(cx, "pin-insertion-mark"));
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let settings = current(cx);
    assert_eq!(settings.pinned, vec![p("a"), p("c")]);
    assert_eq!(settings.favorites, Some(vec![p("b")]));
    assert!(
        settings.is_collapsed(SidebarSection::Favorites),
        "still collapsed"
    );

    refresh(cx);
    let at = bounds(cx, "pin-list-label").center();
    click(cx, at);
    assert!(!collapsed(cx, SidebarSection::Favorites));
    assert!(
        drawn(cx, "pin-row-0") && drawn(cx, "pin-row-1"),
        "listed again"
    );
}

/// Network collapses to its heading and expands again; its add button stays.
#[gpui::test]
fn network_collapses_to_its_heading(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| st.net_places = vec![PathBuf::from(r"\\nas\docs")]);
    refresh(cx);
    let share = format!(
        "sidebar {}",
        t!("sidebar.share_label", share = "docs", server = "nas")
    );
    assert!(drawn(cx, &share));

    let at = bounds(cx, "network-header").center();

    click(cx, at);
    assert!(collapsed(cx, SidebarSection::Network));
    assert!(!drawn(cx, &share), "the shares are hidden");
    assert!(drawn(cx, "network-header"), "the heading stays");

    let at = bounds(cx, "network-header").center();

    click(cx, at);
    assert!(!collapsed(cx, SidebarSection::Network));
    assert!(drawn(cx, &share));
}

/// Every section's state is kept on its own, This PC's included.
#[gpui::test]
fn sections_collapse_independently(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| st.pinned = vec![p("c")]);
    refresh(cx);
    let at = bounds(cx, "this-pc-header").center();
    click(cx, at);
    let at = bounds(cx, "network-header").center();
    click(cx, at);
    assert_eq!(
        current(cx).collapsed_sections,
        vec![SidebarSection::ThisPc, SidebarSection::Network]
    );
    assert!(drawn(cx, "pin-row-0"), "Favorites is still open");
    let at = bounds(cx, "this-pc-header").center();
    click(cx, at);
    assert_eq!(
        current(cx).collapsed_sections,
        vec![SidebarSection::Network]
    );
    // The tabs don't collapse.
    let at = bounds(cx, "tabs-header").center();
    click(cx, at);
    assert!(drawn(cx, "sidebar-tabs-frame"));
    assert_eq!(
        current(cx).collapsed_sections,
        vec![SidebarSection::Network]
    );
}

/// A row dropping in under a heading that was just expanded takes a click made during the
/// transition, even when the press comes while it plays and the release after it ended: the rows
/// are styled in place, never wrapped in an animation that is taken away again (that mounted them
/// anew, and the release found no press).
#[gpui::test]
fn a_click_during_the_drop_in_still_lands(cx: &mut TestAppContext) {
    crate::motion::force_for_test(true);
    let fs = FakeFs::new();
    fs.insert_many(r"C:\w", 1);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(crate::shell::Place {
        label: "Local Disk (C:)".into(),
        path: PathBuf::from(r"C:\"),
        kind: crate::shell::PlaceKind::Drive {
            remote: false,
            space: None,
        },
    });
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\w");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    change(cx, |st| st.toggle_collapsed(SidebarSection::ThisPc));
    refresh(cx);
    let drive = "sidebar Local Disk (C:)";
    assert!(!drawn(cx, drive), "This PC starts collapsed");

    let at = bounds(cx, "this-pc-header").center();
    click(cx, at);
    assert!(!collapsed(cx, SidebarSection::ThisPc));
    // Drawn mid-transition (the cue's clock is real time, so it is set going again here).
    ws.update(cx, |ws, _| {
        ws.section_cue_for_test().set_playing_for_test(true);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let row = bounds(cx, drive).center();
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: row,
        modifiers: Modifiers::none(),
        click_count: 1,
        first_mouse: false,
    });
    // Over before the release, and drawn so.
    ws.update(cx, |ws, _| {
        ws.section_cue_for_test().set_playing_for_test(false);
    });
    refresh(cx);
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: row,
        modifiers: Modifiers::none(),
        click_count: 1,
    });
    cx.run_until_parked();
    wait_until(cx, &view, "the drive opens", loaded(r"C:\"));
    crate::motion::force_for_test(false);
}

/// The shadow under the Home row shows only while the places are scrolled down.
#[gpui::test]
fn the_home_row_casts_a_shadow_only_when_scrolled(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.pinned = (0..30).map(|i| p(&format!("p{i:02}"))).collect();
    });
    refresh(cx);
    assert!(
        ws.update(cx, |ws, _| ws.places_overflow_for_test()) > px(40.),
        "the places scroll"
    );
    assert!(!drawn(cx, "sidebar-scroll-shadow"), "at the top: none");

    ws.update(cx, |ws, cx| ws.scroll_places_for_test(px(40.), cx));
    refresh(cx);
    let shadow = bounds(cx, "sidebar-scroll-shadow");
    let frame = bounds(cx, "sidebar-places-frame");
    assert!(
        (shadow.top() - frame.top()).abs() <= px(1.),
        "at the top edge of the places, under the Home row: {shadow:?} {frame:?}"
    );
    assert!(shadow.size.width >= frame.size.width, "across the sidebar");

    ws.update(cx, |ws, cx| ws.scroll_places_for_test(px(0.), cx));
    refresh(cx);
    assert!(!drawn(cx, "sidebar-scroll-shadow"), "back at the top");
}

/// Opens a tab for each of `p00` on, `count` of them, the last one active.
fn open_tabs(ws: &Entity<Workspace>, cx: &mut VisualTestContext, count: usize) {
    for i in 0..count {
        let path = format!(r"C:\w\p{i:02}");
        ws.update_in(cx, |ws, window, cx| {
            let at = ws.tab_views().count();
            ws.open_tab(PathBuf::from(&path), at, true, window, cx);
        });
        let view = ws.update(cx, |ws, _| ws.active_view().clone());
        wait_until(cx, &view, "load", loaded(&path));
    }
    refresh(cx);
}

/// Tab `ix` is drawn with its whole height inside the vertical tab list.
fn tab_shown(cx: &mut VisualTestContext, ix: usize) -> bool {
    let list = bounds(cx, "sidebar-tabs-frame");
    let id: &'static str = Box::leak(format!("tab-{ix}").into_boxed_str());
    cx.debug_bounds(id)
        .is_some_and(|b| b.top() >= list.top() - px(1.) && b.bottom() <= list.bottom() + px(1.))
}

/// A spot on the tab list's scrollbar, near the bottom: right of the tabs, short of the sidebar's
/// edge.
fn tab_track(cx: &mut VisualTestContext) -> gpui::Point<Pixels> {
    let list = bounds(cx, "sidebar-tabs-frame");
    point(list.right() + px(2.), list.bottom() - px(8.))
}

fn tabs_scroll(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> (Pixels, Pixels) {
    ws.update(cx, |ws, _| ws.tabs_scroll_for_test())
}

/// The vertical tabs get the file list's scrollbar once they don't fit, and only then: a press on
/// its track below the thumb pages down (#323). The active tab is scrolled into view each time it
/// changes.
#[gpui::test]
fn the_tab_list_has_a_scrollbar_once_full(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    // One tab: nothing to scroll, and nothing there takes the press.
    let at = tab_track(cx);
    click(cx, at);
    assert_eq!(tabs_scroll(&ws, cx), (px(0.), px(0.)));

    open_tabs(&ws, cx, 30);
    let (overflow, offset) = tabs_scroll(&ws, cx);
    assert!(overflow > px(0.), "the tabs don't fit");
    assert!(offset < px(0.), "scrolled down");
    assert!(tab_shown(cx, 30), "to the active tab, the last one");

    ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
    refresh(cx);
    assert!(tab_shown(cx, 0), "back up to the first once it is active");
    assert!(tabs_scroll(&ws, cx).1 > px(-0.5));

    let at = tab_track(cx);
    click(cx, at);
    assert!(tabs_scroll(&ws, cx).1 < px(-40.), "paged down");
    assert!(!tab_shown(cx, 0));
    // The active tab didn't change, so it isn't scrolled back to.
    refresh(cx);
    assert!(!tab_shown(cx, 0), "stays where it was paged to");
}

fn right_click(cx: &mut VisualTestContext, at: gpui::Point<Pixels>) {
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
}

/// The open sidebar menu's rows, as drawn.
fn menu(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Vec<String>> {
    ws.update(cx, |ws, cx| ws.open_menu_drawn_for_test(cx))
}

/// A section heading's menu: Hide, a line, then the sections list and the rest of the menu of the
/// sidebar's empty space (#465).
fn with_hide(hide: &str) -> Vec<String> {
    vec![
        hide.to_owned(),
        "-".to_owned(),
        t!("home.title").into_owned(),
        t!("sidebar.favorites").into_owned(),
        t!("sidebar.this_pc").into_owned(),
        t!("sidebar.network").into_owned(),
        "-".to_owned(),
        t!("sidebar.follow_folder").into_owned(),
    ]
}

fn checks(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Vec<bool>> {
    ws.update(cx, |ws, _| ws.open_menu_checks_for_test())
}

fn pick(ws: &Entity<Workspace>, cx: &mut VisualTestContext, label: &str) {
    ws.update_in(cx, |ws, window, cx| {
        ws.run_open_menu_for_test(label, window, cx);
    });
    refresh(cx);
}

/// Favorites and This PC hide from their heading's menu, and a right click on the sidebar's empty
/// space (below the tabs) shows them again, checked while shown; kept in the settings (#323).
#[gpui::test]
fn sections_hide_from_their_heading_and_come_back(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let hide = t!("sidebar.hide_section").into_owned();
    let this_pc = t!("sidebar.this_pc").into_owned();
    let favorites = t!("sidebar.favorites").into_owned();
    let network = t!("sidebar.network").into_owned();

    let at = bounds(cx, "this-pc-header").center();
    right_click(cx, at);
    assert_eq!(menu(&ws, cx), Some(with_hide(&hide)));
    pick(&ws, cx, &hide);
    assert_eq!(current(cx).hidden_sections, vec![SidebarSection::ThisPc]);
    assert!(
        !collapsed(cx, SidebarSection::ThisPc),
        "hidden, not collapsed"
    );
    assert!(!drawn(cx, "this-pc-header"), "gone, heading and all");
    assert!(drawn(cx, "network-header"), "Network stays until hidden");

    let at = bounds(cx, "pin-list-label").center();
    right_click(cx, at);
    pick(&ws, cx, &hide);
    assert!(!drawn(cx, "pin-list-label") && !drawn(cx, "pin-row-0"));
    assert!(
        !drawn(cx, "tile-slot-0"),
        "the tiles are Favorites', and go with it"
    );
    assert_eq!(
        current(cx).hidden_sections,
        vec![SidebarSection::ThisPc, SidebarSection::Favorites]
    );

    let at = bounds(cx, "tab-list-end").center();
    right_click(cx, at);
    assert_eq!(
        menu(&ws, cx),
        Some(vec![
            t!("home.title").into_owned(),
            favorites.clone(),
            this_pc.clone(),
            network,
            "-".to_owned(),
            t!("sidebar.follow_folder").into_owned(),
        ])
    );
    assert_eq!(checks(&ws, cx), Some(vec![true, false, false, true, false]));
    pick(&ws, cx, &this_pc);
    assert_eq!(current(cx).hidden_sections, vec![SidebarSection::Favorites]);
    assert!(drawn(cx, "this-pc-header"), "back");

    let at = bounds(cx, "tab-list-end").center();
    right_click(cx, at);
    assert_eq!(checks(&ws, cx), Some(vec![true, false, true, true, false]));
    pick(&ws, cx, &favorites);
    assert!(current(cx).hidden_sections.is_empty());
    assert!(
        drawn(cx, "pin-list-label") && drawn(cx, "pin-row-0") && drawn(cx, "tile-slot-0"),
        "listed again, the tiles with it"
    );
}

/// Home hides from its row's menu, and only its row: the tiles and the list under Favorites stay
/// (#463). The sidebar's empty space brings Home back.
#[gpui::test]
fn home_hides_only_its_row(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    let hide = t!("sidebar.hide_section").into_owned();
    let home = t!("home.title").into_owned();

    let at = bounds(cx, "open-home").center();
    right_click(cx, at);
    assert_eq!(menu(&ws, cx), Some(with_hide(&hide)));
    pick(&ws, cx, &hide);
    assert_eq!(current(cx).hidden_sections, vec![SidebarSection::Home]);
    assert!(!drawn(cx, "open-home"), "the Home row is gone");
    assert!(drawn(cx, "tile-slot-0"), "the tiles stay");
    assert!(drawn(cx, "pin-row-0"), "the list stays");

    let at = bounds(cx, "tab-list-end").center();
    right_click(cx, at);
    assert_eq!(checks(&ws, cx), Some(vec![false, true, true, true, false]));
    pick(&ws, cx, &home);
    assert!(current(cx).hidden_sections.is_empty());
    assert!(drawn(cx, "open-home") && drawn(cx, "tile-slot-0"));
}

/// Favorites hides the tiles and the list together, Home or not; showing it brings both back
/// (#463).
#[gpui::test]
fn favorites_hides_the_tiles_and_the_list_together(cx: &mut TestAppContext) {
    let (_ws, cx) = window(cx);
    change(cx, |st| {
        st.favorites = Some(vec![p("a")]);
        st.pinned = vec![p("c")];
    });
    refresh(cx);
    assert!(drawn(cx, "tile-slot-0") && drawn(cx, "pin-row-0"));

    change(cx, |st| {
        st.hidden_sections = vec![SidebarSection::Favorites];
    });
    refresh(cx);
    assert!(drawn(cx, "open-home"), "the Home row stays");
    assert!(!drawn(cx, "tile-slot-0"), "no tiles");
    assert!(
        !drawn(cx, "pin-list-label") && !drawn(cx, "pin-row-0"),
        "no list"
    );

    change(cx, |st| {
        st.hidden_sections = vec![SidebarSection::Home, SidebarSection::Favorites];
    });
    refresh(cx);
    assert!(!drawn(cx, "open-home") && !drawn(cx, "tile-slot-0"));
    assert!(!drawn(cx, "pin-list-label") && !drawn(cx, "pin-row-0"));
    assert!(drawn(cx, "this-pc-header"), "the rest of the sidebar stays");

    change(cx, |st| st.hidden_sections = vec![SidebarSection::Home]);
    refresh(cx);
    assert!(!drawn(cx, "open-home"));
    assert!(
        drawn(cx, "tile-slot-0") && drawn(cx, "pin-row-0"),
        "both are back"
    );
}

/// The Recycle Bin, once removed from the favorites, comes back from the menu of the sidebar's
/// empty space, the favorites with it when they were hidden (#308).
#[gpui::test]
fn the_recycle_bin_comes_back_from_the_sidebars_menu(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let add = t!("sidebar.add_recycle").into_owned();
    let at = bounds(cx, "tab-list-end").center();
    right_click(cx, at);
    let rows = menu(&ws, cx).expect("a menu");
    assert!(!rows.contains(&add), "it is there already: {rows:?}");

    ws.update_in(
        cx,
        super::super::workspace::Workspace::unpin_recycle_for_test,
    );
    change(cx, |st| st.set_hidden(SidebarSection::Favorites, true));
    refresh(cx);
    let at = bounds(cx, "tab-list-end").center();
    right_click(cx, at);
    let rows = menu(&ws, cx).expect("a menu");
    assert_eq!(rows.last(), Some(&add));
    assert_eq!(rows[rows.len() - 2], "-", "apart from the sections");
    pick(&ws, cx, &add);
    let settings = current(cx);
    assert!(settings.recycle_pinned);
    assert!(
        !settings.is_hidden(SidebarSection::Favorites),
        "the favorites show, with it"
    );
}

/// Network hides from its heading's menu and comes back from the sidebar's empty space (#387).
#[gpui::test]
fn network_hides_and_comes_back(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let hide = t!("sidebar.hide_section").into_owned();
    let network = t!("sidebar.network").into_owned();

    let at = bounds(cx, "network-header").center();
    right_click(cx, at);
    assert_eq!(menu(&ws, cx), Some(with_hide(&hide)));
    pick(&ws, cx, &hide);
    assert_eq!(current(cx).hidden_sections, vec![SidebarSection::Network]);
    assert!(!drawn(cx, "network-header"), "gone, heading and all");
    assert!(drawn(cx, "this-pc-header"), "This PC stays");

    let at = bounds(cx, "tab-list-end").center();
    right_click(cx, at);
    assert_eq!(checks(&ws, cx), Some(vec![true, true, true, false, false]));
    pick(&ws, cx, &network);
    assert!(current(cx).hidden_sections.is_empty());
    assert!(drawn(cx, "network-header"), "back");
}

/// A right click on a section's heading opens the sections menu, even with no empty space to
/// click (#465).
fn heading_opens_the_sections_menu(cx: &mut TestAppContext, heading: &str) {
    let (ws, cx) = window(cx);
    let this_pc = t!("sidebar.this_pc").into_owned();
    let network = t!("sidebar.network").into_owned();
    let at = bounds(cx, heading).center();
    right_click(cx, at);
    let rows = menu(&ws, cx).expect(heading);
    assert!(
        rows.contains(&this_pc) && rows.contains(&network),
        "{rows:?}"
    );
    assert_eq!(
        checks(&ws, cx),
        Some(vec![false, true, true, true, true, false])
    );
    // Hidden from this heading's menu, shown again from another's.
    pick(&ws, cx, &this_pc);
    assert_eq!(current(cx).hidden_sections, vec![SidebarSection::ThisPc]);
    let other = if heading == "network-header" {
        "pin-list-label"
    } else {
        "network-header"
    };
    let at = bounds(cx, other).center();
    right_click(cx, at);
    pick(&ws, cx, &this_pc);
    assert!(current(cx).hidden_sections.is_empty());
    assert!(drawn(cx, "this-pc-header"), "back");
}

#[gpui::test]
fn the_favorites_heading_opens_the_sections_menu(cx: &mut TestAppContext) {
    heading_opens_the_sections_menu(cx, "pin-list-label");
}

#[gpui::test]
fn the_network_heading_opens_the_sections_menu(cx: &mut TestAppContext) {
    heading_opens_the_sections_menu(cx, "network-header");
}

#[gpui::test]
fn the_home_row_opens_the_sections_menu(cx: &mut TestAppContext) {
    heading_opens_the_sections_menu(cx, "open-home");
}

/// With the tabs on top the Tabs heading is gone, and the headings still open the menu (#465).
#[gpui::test]
fn the_headings_open_the_sections_menu_with_the_tabs_on_top(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    change(cx, |st| {
        st.tab_layout = crate::settings::TabLayout::Horizontal;
    });
    refresh(cx);
    assert!(!drawn(cx, "tabs-header"), "the tabs are on top");
    let network = t!("sidebar.network").into_owned();
    let at = bounds(cx, "this-pc-header").center();
    right_click(cx, at);
    let rows = menu(&ws, cx).expect("a menu");
    assert!(rows.contains(&network), "{rows:?}");
    pick(&ws, cx, &network);
    assert_eq!(current(cx).hidden_sections, vec![SidebarSection::Network]);
    assert!(!drawn(cx, "network-header"));
}

/// A spot on the places list's scrollbar `from_bottom` above its lower end: right of the rows,
/// short of the sidebar's edge.
fn places_track(cx: &mut VisualTestContext, from_bottom: Pixels) -> gpui::Point<Pixels> {
    let frame = bounds(cx, "sidebar-places-frame");
    point(frame.right() + px(2.), frame.bottom() - from_bottom)
}

/// The places list has the tab list's scrollbar (#427): nothing is there while the places fit, and
/// once they don't a press below the thumb pages down and the thumb can be dragged.
#[gpui::test]
fn the_places_list_has_a_scrollbar_once_it_overflows(cx: &mut TestAppContext) {
    let (ws, cx) = window(cx);
    let overflow = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.places_overflow_for_test())
    };
    let offset = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.places_offset_for_test())
    };
    // Fits: no scrollbar to take the press.
    assert!(overflow(&ws, cx) < px(1.), "the places fit");
    let at = places_track(cx, px(8.));
    click(cx, at);
    assert_eq!(offset(&ws, cx), px(0.));

    change(cx, |st| {
        st.pinned = (0..30).map(|i| p(&format!("p{i:02}"))).collect();
    });
    refresh(cx);
    let max = overflow(&ws, cx);
    assert!(max > px(40.), "the places scroll");
    // A press below the thumb pages down.
    let at = places_track(cx, px(8.));
    click(cx, at);
    assert!(offset(&ws, cx) < px(-40.), "paged down");

    // The thumb dragged: from the top of the list to its end.
    ws.update(cx, |ws, cx| ws.scroll_places_for_test(px(0.), cx));
    refresh(cx);
    let frame = bounds(cx, "sidebar-places-frame");
    let grab = point(frame.right() + px(2.), frame.top() + px(3.));
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    let to = point(grab.x, frame.bottom());
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    refresh(cx);
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert!(
        (offset(&ws, cx) + max).abs() < px(1.),
        "dragged to the end: {:?} of {max:?}",
        offset(&ws, cx)
    );
}

/// Every heading has its icon, centered on a row as tall as the sidebar's rows in each density,
/// and a hairline sets each group of places apart from what is above it (the line between the
/// places and the tabs does that for Tabs). With nothing above, no rule.
#[gpui::test]
fn section_headings_have_an_icon_and_follow_the_density(cx: &mut TestAppContext) {
    use crate::settings::Density;
    let (_ws, cx) = window(cx);
    change(cx, |st| st.pinned = vec![p("c")]);
    refresh(cx);
    let headings = [
        "pin-list-label",
        "this-pc-header",
        "network-header",
        "tabs-header",
    ];
    for density in [Density::Compact, Density::Standard, Density::Comfortable] {
        change(cx, move |st| st.density = density);
        refresh(cx);
        let row = crate::theme::h_row_side();
        for id in headings {
            let heading = bounds(cx, id);
            assert_eq!(heading.size.height, row, "{density:?}: {id} is a row tall");
            let icon = bounds(cx, &format!("{id}-icon"));
            assert_eq!(icon.size.height, crate::theme::icon(), "{density:?}: {id}");
            assert!(
                (icon.center().y - heading.center().y).abs() < px(1.),
                "{density:?}: {id}'s icon is centered: {icon:?} in {heading:?}"
            );
            assert!(
                icon.left() >= heading.left() && icon.right() < heading.center().x,
                "{density:?}: {id}'s icon comes first"
            );
        }
        for id in &headings[..3] {
            let rule = bounds(cx, &format!("{id}-rule"));
            let heading = bounds(cx, id);
            assert_eq!(rule.size.height, crate::theme::side_rule_h(), "{density:?}");
            assert!(
                rule.bottom() <= heading.top() && heading.top() - rule.bottom() < px(4.),
                "{density:?}: the rule is right above {id}: {rule:?} / {heading:?}"
            );
        }
        assert!(
            !drawn(cx, "tabs-header-rule"),
            "the split line sets the tabs apart"
        );
    }

    // Home hidden: the tiles stay above Favorites, so its heading keeps its rule (#463).
    change(cx, |st| st.hidden_sections = vec![SidebarSection::Home]);
    refresh(cx);
    assert!(drawn(cx, "pin-list-label"));
    assert!(drawn(cx, "pin-list-label-rule"), "the tiles are above it");
    assert!(drawn(cx, "this-pc-header-rule"));
}
