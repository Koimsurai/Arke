//! GPUI tests for the sidebar's sections (`workspace_sidebar.rs`): Favorites, This PC and Network
//! each collapse to their heading and expand again with a click (remembered in the
//! settings), a drop on a collapsed heading still lands, and a shadow under the Home row shows
//! while the places are scrolled.

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
        "the tiles belong to Home and stay"
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
    assert!(drawn(cx, "pin-insertion-mark"), "under the heading");
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
