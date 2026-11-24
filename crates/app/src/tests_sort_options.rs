//! GPUI tests for the Sort button's flyout and the background menu's "Sort by" submenu
//! (`explorer_sortopts.rs`, #179): both sort the list as a header click does, in every view.

use gpui::Modifiers;

use super::*;
use crate::explorer::ViewMode;
use crate::folder_views::FolderViewStore;

/// Sizes 5, 9 and 1: by name a, b, c; largest first b, a, c; smallest first c, a, b.
fn sized_folder(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\s",
        vec![file("a.txt", 5), file("b.txt", 9), file("c.jpg", 1)],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\s");
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["a.txt", "b.txt", "c.jpg"]
    );
    (view, cx)
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} should be drawn"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

fn is_open(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> bool {
    view.update(cx, |v, _| v.sort_options_open_for_test())
}

/// Opens the background menu and its "Sort by" submenu (right after "View").
fn open_sort_submenu(view: &Entity<Explorer>, cx: &mut VisualTestContext) {
    right_click(view, cx, None);
    let labels = menu_labels(view, cx).expect("background menu");
    let sort_by = format!("{} ›", t!("settings.sort_by"));
    let ix = labels
        .iter()
        .position(|l| *l == sort_by)
        .unwrap_or_else(|| panic!("no Sort by in {labels:?}"));
    assert_eq!(labels[ix - 1], format!("{} ›", t!("menu.view")));
    view.update(cx, |v, _| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(ix);
        }
    });
    cx.simulate_keystrokes("right");
}

fn submenu_checks(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<String> {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .map_or_default(|m| m.checked_labels_at(1))
    })
}

/// The flyout lists the columns with the current one checked, then the directions. Picking a
/// column sorts as its header would (sizes start largest first), the flyout stays open for a
/// direction, the choice is remembered for the folder, and Esc closes it.
#[gpui::test]
fn the_sort_flyout_sorts_the_list(cx: &mut TestAppContext) {
    let (view, cx) = sized_folder(cx);
    click(cx, "sort-options");
    assert!(is_open(&view, cx));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    for id in [
        "sort-by-name",
        "sort-by-modified",
        "sort-by-type",
        "sort-by-size",
        "sort-by-created",
        "sort-by-extension",
        "sort-by-attributes",
        "sort-ascending",
        "sort-descending",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "{id} in the flyout");
    }

    click(cx, "sort-by-size");
    wait_until(cx, &view, "largest first", |v| {
        names(v) == ["b.txt", "a.txt", "c.jpg"]
    });
    let sort = view.update(cx, |v, _| v.sort);
    assert_eq!(
        (sort.column, sort.ascending, sort.key_count()),
        (SortColumn::Size, false, 1),
        "the same as a click on the Size header"
    );
    assert!(is_open(&view, cx), "stays open");
    // The column already sorted by keeps its direction.
    click(cx, "sort-by-size");
    assert!(!view.update(cx, |v, _| v.sort.ascending));

    click(cx, "sort-ascending");
    wait_until(cx, &view, "smallest first", |v| {
        names(v) == ["c.jpg", "a.txt", "b.txt"]
    });
    let remembered = cx.update(|_, cx| {
        FolderViewStore::global(cx)
            .read(cx)
            .peek(Path::new(r"C:\s"))
    });
    assert_eq!(
        remembered.map(|v| (v.column, v.ascending)),
        Some((SortColumn::Size, true)),
        "remembered for the folder"
    );

    cx.simulate_keystrokes("escape");
    assert!(!is_open(&view, cx));
}

/// Icons have no headers: the flyout sorts them, and Details shows the same sort afterwards.
#[gpui::test]
fn the_sort_flyout_works_in_icons_view(cx: &mut TestAppContext) {
    let (view, cx) = sized_folder(cx);
    cx.dispatch_action(crate::explorer::ViewIcons);
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.view_mode), ViewMode::Icons);
    click(cx, "sort-options");
    click(cx, "sort-descending");
    wait_until(cx, &view, "name, Z to A", |v| {
        names(v) == ["c.jpg", "b.txt", "a.txt"]
    });
    click(cx, "sort-by-size");
    wait_until(cx, &view, "largest first", |v| {
        names(v) == ["b.txt", "a.txt", "c.jpg"]
    });
    assert_eq!(view.update(cx, |v, _| v.view_mode), ViewMode::Icons);
    assert_eq!(
        view.update(cx, |v, _| view_names(v)),
        ["b.txt", "a.txt", "c.jpg"]
    );

    cx.simulate_keystrokes("escape");
    assert!(!is_open(&view, cx));
    cx.dispatch_action(crate::explorer::ViewDetails);
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.view_mode), ViewMode::Details);
    // The default columns, Size among them (the workspace applies them in the app).
    cx.update(|_, cx| {
        crate::prefs::apply(&crate::settings::SettingsStore::global(cx).read(cx).settings);
    });
    let size = view
        .update(cx, |v, _| v.header_for_test())
        .into_iter()
        .find(|(col, _)| *col == crate::prefs::Col::Size)
        .and_then(|(_, key)| key);
    assert_eq!(size, Some((0, false)), "the Size header shows the sort");
}

/// The empty-area menu has "Sort by" after "View", with the same choices and checks; picking one
/// sorts the list, in Icons too.
#[gpui::test]
fn the_background_menu_sorts_the_list(cx: &mut TestAppContext) {
    let (view, cx) = sized_folder(cx);
    open_sort_submenu(&view, cx);
    let level = view.update(cx, |v, _| {
        v.context_menu.as_ref().map(|m| (m.depth(), m.labels_at(1)))
    });
    assert_eq!(
        level,
        Some((
            1,
            vec![
                t!("col.name").into_owned(),
                t!("col.modified").into_owned(),
                t!("col.type").into_owned(),
                t!("col.size").into_owned(),
                t!("col.created").into_owned(),
                t!("col.extension").into_owned(),
                t!("col.attributes").into_owned(),
                "—".to_owned(),
                t!("settings.ascending").into_owned(),
                t!("settings.descending").into_owned(),
            ]
        ))
    );
    assert_eq!(
        submenu_checks(&view, cx),
        [
            t!("col.name").into_owned(),
            t!("settings.ascending").into_owned()
        ]
    );
    // Name, Date modified, Type, Size.
    cx.simulate_keystrokes("down down down enter");
    assert!(menu_labels(&view, cx).is_none(), "the menu closed");
    wait_until(cx, &view, "largest first", |v| {
        names(v) == ["b.txt", "a.txt", "c.jpg"]
    });

    cx.dispatch_action(crate::explorer::ViewIcons);
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.view_mode), ViewMode::Icons);
    right_click(&view, cx, None);
    run_command(&view, cx, MenuCommand::SortDirection(true));
    wait_until(cx, &view, "smallest first", |v| {
        names(v) == ["c.jpg", "a.txt", "b.txt"]
    });
    open_sort_submenu(&view, cx);
    assert_eq!(
        submenu_checks(&view, cx),
        [
            t!("col.size").into_owned(),
            t!("settings.ascending").into_owned()
        ]
    );
}
