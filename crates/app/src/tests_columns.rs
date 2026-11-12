//! GPUI tests for the Details header and columns (`explorer_details.rs`): multi-key sorting,
//! reordering, resizing and fitting, the column menu, and the Link target column.

use gpui::{Modifiers, MouseButton, Pixels, point, px};

use super::*;
use crate::folder_views::FolderViewStore;
use crate::prefs::Col;
use crate::settings::SettingsStore;

fn change_settings(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings)) {
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx));
        // The workspace applies list prefs on settings changes; these tests have no workspace.
        crate::prefs::apply(&SettingsStore::global(cx).read(cx).settings);
    });
    cx.run_until_parked();
}

fn apply_prefs(cx: &mut VisualTestContext) {
    cx.update(|_, cx| crate::prefs::apply(&SettingsStore::global(cx).read(cx).settings));
    cx.run_until_parked();
}

/// Middle of a column's header. Columns run left to right from Name, each its own width.
fn header_x(view: &Entity<Explorer>, cx: &mut VisualTestContext, col: Col) -> (Pixels, Pixels) {
    let columns = view.update(cx, |v, _| v.header_columns_for_test());
    let bounds = view.update(cx, |v, _| v.list_bounds_for_test());
    let y = bounds.top() - crate::theme::H_HEADER / 2.;
    columns.iter().find(|(c, _, _)| *c == col).map_or_else(
        || panic!("{col:?} not in the header: {columns:?}"),
        |&(_, left, width)| (left + width / 2., y),
    )
}

/// Where `col`'s divider is: half a gap past its right edge.
fn divider_x(view: &Entity<Explorer>, cx: &mut VisualTestContext, col: Col) -> (Pixels, Pixels) {
    let columns = view.update(cx, |v, _| v.header_columns_for_test());
    let bounds = view.update(cx, |v, _| v.list_bounds_for_test());
    let y = bounds.top() - crate::theme::H_HEADER / 2.;
    columns.iter().find(|(c, _, _)| *c == col).map_or_else(
        || panic!("{col:?} not in the header: {columns:?}"),
        |&(_, left, width)| (left + width + crate::theme::S_12 / 2., y),
    )
}

fn click_header(
    view: &Entity<Explorer>,
    cx: &mut VisualTestContext,
    col: Col,
    modifiers: Modifiers,
) {
    let (x, y) = header_x(view, cx, col);
    cx.simulate_click(point(x, y), modifiers);
    cx.run_until_parked();
}

fn key(view: &Entity<Explorer>, cx: &mut VisualTestContext, col: Col) -> Option<(usize, bool)> {
    view.update(cx, |v, _| v.header_for_test())
        .into_iter()
        .find(|(c, _)| *c == col)
        .and_then(|(_, k)| k)
}

/// Plain click sorts by one column; Shift+click adds the next key (and flips it when clicked
/// again); a plain click goes back to one key. The keys are remembered for the folder.
#[gpui::test]
fn shift_click_adds_sort_keys(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\m",
        vec![
            file("a.txt", 5),
            file("b.txt", 9),
            file("c.jpg", 5),
            file("d.jpg", 9),
        ],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    apply_prefs(cx);

    click_header(&view, cx, Col::Type, Modifiers::none());
    wait_until(cx, &view, "by type", |v| {
        names(v) == ["c.jpg", "d.jpg", "a.txt", "b.txt"]
    });
    click_header(&view, cx, Col::Size, Modifiers::shift());
    wait_until(cx, &view, "type, then largest first", |v| {
        names(v) == ["d.jpg", "c.jpg", "b.txt", "a.txt"]
    });
    assert_eq!(key(&view, cx, Col::Type), Some((0, true)));
    assert_eq!(key(&view, cx, Col::Size), Some((1, false)));
    let remembered = cx.update(|_, cx| {
        FolderViewStore::global(cx)
            .read(cx)
            .peek(Path::new(r"C:\m"))
    });
    assert_eq!(
        remembered.and_then(|v| v.then[0]).map(|k| k.column),
        Some(SortColumn::Size),
        "the second key is remembered for the folder"
    );

    click_header(&view, cx, Col::Size, Modifiers::shift());
    wait_until(cx, &view, "type, then smallest first", |v| {
        names(v) == ["c.jpg", "d.jpg", "a.txt", "b.txt"]
    });
    click_header(&view, cx, Col::Size, Modifiers::none());
    wait_until(cx, &view, "size alone", |v| {
        names(v) == ["d.jpg", "b.txt", "c.jpg", "a.txt"]
    });
    assert_eq!(key(&view, cx, Col::Type), None);
    assert_eq!(view.update(cx, |v, _| v.sort.key_count()), 1);
}

/// Dragging a header onto another moves it there, saved in settings.
#[gpui::test]
fn dragging_a_header_reorders_columns(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\r", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    apply_prefs(cx);
    let shown = |view: &Entity<Explorer>, cx: &mut VisualTestContext| -> Vec<Col> {
        view.update(cx, |v, _| v.header_for_test())
            .into_iter()
            .map(|(c, _)| c)
            .collect()
    };
    assert_eq!(
        shown(&view, cx),
        [Col::Name, Col::Modified, Col::Type, Col::Size]
    );
    let from = header_x(&view, cx, Col::Size);
    let to = header_x(&view, cx, Col::Modified);
    cx.simulate_mouse_down(point(from.0, from.1), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(from.0 - px(20.), from.1),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(point(to.0, to.1), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(to.0, to.1), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    apply_prefs(cx);
    assert_eq!(
        shown(&view, cx),
        [Col::Name, Col::Size, Col::Modified, Col::Type]
    );
    let order = cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .column_order
            .clone()
    });
    assert_eq!(order.first().map(String::as_str), Some("size"), "{order:?}");
    assert_eq!(
        view.update(cx, |v, _| v.sort.column),
        SortColumn::Name,
        "a drag is not a click"
    );
}

/// The header's right-click menu lists every optional column, checked when shown, and switches
/// the same settings as the Settings page.
#[gpui::test]
fn header_menu_toggles_columns(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\h",
        vec![Entry::new("a.TXT", EntryKind::File, 1, None, 0x21)],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\h");
    wait_until(cx, &view, "load", loaded(r"C:\h"));
    apply_prefs(cx);
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_header_menu(point(px(300.), px(60.)), window, cx);
        });
    });
    let checked = view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .map(crate::explorer::ContextMenuForTest::checked_labels)
    });
    assert_eq!(
        checked,
        Some(vec![
            t!("col.modified").into_owned(),
            t!("col.type").into_owned(),
            t!("col.size").into_owned(),
        ])
    );
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        labels.contains(&t!("col.link_target").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::ToggleColumn(Col::Attributes));
    apply_prefs(cx);
    assert!(cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.show_attributes));
    change_settings(cx, |st| st.show_extension = true);
    let cols: Vec<Col> = view
        .update(cx, |v, _| v.header_for_test())
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    assert_eq!(
        cols,
        [
            Col::Name,
            Col::Modified,
            Col::Type,
            Col::Extension,
            Col::Attributes,
            Col::Size
        ]
    );
    // Attributes sort by their letters.
    click_header(&view, cx, Col::Attributes, Modifiers::none());
    assert_eq!(
        view.update(cx, |v, _| v.sort.column),
        SortColumn::Attributes
    );
}

/// Drags `col`'s divider (on its right edge) by `dx`.
fn drag_divider(view: &Entity<Explorer>, cx: &mut VisualTestContext, col: Col, dx: Pixels) {
    let (x, y) = divider_x(view, cx, col);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(x + dx / 2., y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(x + dx, y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(x + dx, y), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn columns(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<(Col, Pixels, Pixels)> {
    view.update(cx, |v, _| v.header_columns_for_test())
}

fn saved_width(cx: &mut VisualTestContext, col: Col) -> Option<u16> {
    cx.update(|_, cx| {
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .column_widths
            .get(col)
    })
}

/// Each column's divider is on its right edge, like Explorer's: dragging Size's makes Size wider
/// and leaves the columns before it alone; dragging Name's sizes Name and moves every other
/// column along. Both are saved, Name's too.
#[gpui::test]
fn a_right_edge_divider_sizes_its_own_column(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\w", vec![file("a.txt", 1), file("b.txt", 2)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\w");
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.simulate_resize(gpui::size(px(1600.), px(800.)));
    apply_prefs(cx);
    let before = columns(&view, cx);
    let size = |cols: &[(Col, Pixels, Pixels)]| cols.iter().find(|c| c.0 == Col::Size).copied();
    let (_, size_left, size_w) = size(&before).unwrap();

    drag_divider(&view, cx, Col::Size, px(40.));
    let after = columns(&view, cx);
    assert_eq!(size(&after), Some((Col::Size, size_left, size_w + px(40.))));
    assert_eq!(
        before[..3],
        after[..3],
        "Name, Modified and Type keep their place and width"
    );
    assert_eq!(saved_width(cx, Col::Size), Some(84 + 40));
    let row = view.update(cx, |v, _| v.row_width_for_test()).unwrap();

    let name_w = after[0].2;
    drag_divider(&view, cx, Col::Name, px(-30.));
    let moved = columns(&view, cx);
    assert_eq!(moved[0].2, name_w - px(30.));
    let close = |a: Pixels, b: Pixels| (a - b).abs() < px(0.01);
    for (was, now) in after.iter().zip(&moved).skip(1) {
        assert!(
            close(now.1, was.1 - px(30.)),
            "{:?} moves with Name's edge",
            now.0
        );
        assert_eq!(now.2, was.2, "{:?} keeps its width", now.0);
    }
    let shorter = view.update(cx, |v, _| v.row_width_for_test()).unwrap();
    assert!(close(shorter, row - px(30.)), "rows end with the columns");
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let name_saved = f32::from(name_w - px(30.)).round() as u16;
    assert_eq!(saved_width(cx, Col::Name), Some(name_saved));
    assert_eq!(
        view.update(cx, |v, _| v.sort.column),
        SortColumn::Name,
        "resizing doesn't sort"
    );
}

/// Double-clicking a divider fits the column to what the rows on screen show in it.
#[gpui::test]
fn double_clicking_a_divider_fits_the_column(cx: &mut TestAppContext) {
    let long = "a-rather-long-file-name-to-fit-the-column-to.txt";
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\f", vec![file(long, 1), file("b.txt", 2)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\f");
    wait_until(cx, &view, "load", loaded(r"C:\f"));
    cx.simulate_resize(gpui::size(px(1600.), px(800.)));
    apply_prefs(cx);
    let double_click = |view: &Entity<Explorer>, cx: &mut VisualTestContext, col: Col| {
        let (x, y) = divider_x(view, cx, col);
        cx.simulate_click(point(x, y), Modifiers::none());
        cx.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Left,
            position: point(x, y),
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: MouseButton::Left,
            position: point(x, y),
            modifiers: Modifiers::none(),
            click_count: 2,
        });
        cx.run_until_parked();
    };
    double_click(&view, cx, Col::Name);
    // The test text system draws every character 0.6 em wide: the icon and its gap, the long
    // name at 14 px and a little air.
    let chars = f32::from(u16::try_from(long.len()).unwrap());
    let want = (crate::theme::ICON + crate::theme::S_12 + px(chars * 14. * 0.6) + px(4.)).ceil();
    let name = columns(&view, cx)[0].2;
    assert_eq!(name, want);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let saved = f32::from(want) as u16;
    assert_eq!(saved_width(cx, Col::Name), Some(saved));
    // Type fits its widest label ("TXT File" or the like), narrower than its default.
    double_click(&view, cx, Col::Type);
    let fitted = crate::prefs::width(Col::Type);
    assert!(fitted < crate::theme::COL_TYPE, "{fitted:?}");
    assert_eq!(
        saved_width(cx, Col::Type).map(|w| px(f32::from(w))),
        Some(fitted)
    );
}

/// Links get their target in the Link target column (read in the background) and "Go to link
/// target", which opens the target's folder with the target selected.
#[gpui::test]
fn link_target_column_and_go_to_target(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\l",
        vec![
            Entry::new("proj", EntryKind::Link { is_dir: true }, 0, None, 0x410),
            Entry::new(
                "note.txt",
                EntryKind::Link { is_dir: false },
                0,
                None,
                0x400,
            ),
            file("plain.txt", 1),
        ],
    );
    fs.insert_link(r"C:\l\proj", r"D:\code\proj");
    fs.insert_dir(r"D:\code", vec![dir("proj"), file("other.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\l");
    wait_until(cx, &view, "load", loaded(r"C:\l"));
    change_settings(cx, |st| st.show_link_target = true);
    wait_until(cx, &view, "link target read", |v| {
        v.link_target_for_test("proj").as_deref() == Some(r"D:\code\proj")
    });
    assert_eq!(
        view.update(cx, |v, _| v.link_target_for_test("note.txt")),
        None,
        "unreadable links show nothing"
    );

    let row = |v: &Explorer, name: &str| names(v).iter().position(|n| n == name);
    let plain = view.update(cx, |v, _| row(v, "plain.txt"));
    right_click(&view, cx, plain);
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        !labels.contains(&t!("menu.go_to_link_target").into_owned()),
        "{labels:?}"
    );
    let proj = view.update(cx, |v, _| row(v, "proj"));
    right_click(&view, cx, proj);
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        labels.contains(&t!("menu.go_to_link_target").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::GoToLinkTarget);
    wait_until(cx, &view, "target folder with the target selected", |v| {
        loaded(r"D:\code")(v) && selected_names(v) == ["proj"]
    });
}
