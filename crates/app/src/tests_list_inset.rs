//! The file list's insets (#448), after File Explorer: the rows (and the column header over
//! them) start a little in from the sidebar, so a marquee started beside it starts in empty
//! space, and a row's highlight stays inside the row so neighbouring highlights keep a gap.

use super::*;
use crate::settings::{Density, SettingsStore};
use crate::theme;
use gpui::{Bounds, Modifiers, MouseButton, Pixels, point, px};

/// Each density with the rows' left inset from the paper's edge and the highlight's inset at
/// the top and bottom of its row.
const DENSITIES: [(Density, f32, f32); 3] = [
    (Density::Standard, 16., 2.),
    (Density::Compact, 12., 1.),
    (Density::Comfortable, 20., 2.),
];

fn set_density(cx: &mut VisualTestContext, density: Density) {
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.density = density, cx));
    });
    cx.run_until_parked();
}

fn drawn(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} not drawn"))
}

fn near(a: Pixels, b: f32) -> bool {
    (a - px(b)).abs() < px(0.5)
}

/// A workspace showing a folder of a few files, wide enough for the Details columns.
fn open_list(cx: &mut TestAppContext) -> (Entity<Explorer>, &mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\i", 6);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::default(), r"C:\i");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\i"));
    cx.simulate_resize(gpui::size(px(1400.), px(800.)));
    cx.run_until_parked();
    (view, cx)
}

/// Rows keep their height (pitch); their highlight is as wide as the row but inset at its top
/// and bottom, so two neighbouring highlights show a gap of twice the inset. The rows start the
/// density's inset in from the paper's left edge, which touches the sidebar.
#[gpui::test]
fn row_highlights_are_inset_and_rows_start_in_from_the_sidebar(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx);
    for (density, inset, wash) in DENSITIES {
        set_density(cx, density);
        assert_eq!(theme::list_inset(), px(inset), "{density:?}");
        assert_eq!(theme::row_wash_inset(), px(wash), "{density:?}");
        let pitch = f32::from(view.update(cx, |v, _| v.row_height()));
        let paper = drawn(cx, "paper");
        let (row0, row1) = (drawn(cx, "row-0"), drawn(cx, "row-1"));
        let (wash0, wash1) = (drawn(cx, "row-wash-0"), drawn(cx, "row-wash-1"));
        assert!(near(row0.size.height, pitch), "{density:?}: {row0:?}");
        assert!(near(row1.top() - row0.top(), pitch), "{density:?}");
        assert!(
            near(row0.left() - paper.left(), inset),
            "{density:?}: rows start {:?} in from the paper",
            row0.left() - paper.left()
        );
        assert!(near(wash0.left() - row0.left(), 0.), "{density:?}");
        assert!(near(wash0.right() - row0.right(), 0.), "{density:?}");
        assert!(
            near(wash0.top() - row0.top(), wash),
            "{density:?}: {wash0:?} in {row0:?}"
        );
        assert!(
            near(row0.bottom() - wash0.bottom(), wash),
            "{density:?}: {wash0:?} in {row0:?}"
        );
        assert!(
            near(wash1.top() - wash0.bottom(), 2. * wash),
            "{density:?}: gap between highlights"
        );
    }
}

/// The column header moves with the rows: its columns start where the rows' do.
#[gpui::test]
fn the_header_lines_up_with_the_inset_rows(cx: &mut TestAppContext) {
    let (_view, cx) = open_list(cx);
    for (density, _, _) in DENSITIES {
        set_density(cx, density);
        let header = drawn(cx, "details-header-type");
        let cell = drawn(cx, "cell-type");
        assert!(
            near(header.left() - cell.left(), 0.),
            "{density:?}: Type header at {:?}, its cells at {:?}",
            header.left(),
            cell.left()
        );
        assert!(near(header.size.width - cell.size.width, 0.), "{density:?}");
    }
}

/// The left inset is empty space: a click there selects nothing (and clears the selection), a
/// marquee starts there. The gap between two highlights is still part of the rows: a click in it
/// selects the row above it.
#[gpui::test]
fn the_left_inset_is_empty_space(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx);
    for (density, inset, wash) in DENSITIES {
        set_density(cx, density);
        let list = view.update(cx, |v, _| v.list_bounds_for_test());
        let row0 = drawn(cx, "row-0");
        let row = row0.size.height;
        let beside = |r: f32| point(row0.left() - px(inset / 2.), row0.top() + row * (r + 0.5));
        assert!(
            beside(0.).x > list.left(),
            "{density:?}: inside the list {list:?}"
        );
        // In the gap below row 2's highlight: still row 2.
        let gap = point(
            row0.left() + px(200.),
            row0.top() + row * 3. - px(wash / 2.),
        );
        cx.simulate_click(gap, Modifiers::none());
        assert_eq!(
            view.update(cx, |v, _| selected_names(v)).len(),
            1,
            "{density:?}"
        );
        cx.simulate_click(beside(2.), Modifiers::none());
        assert!(
            view.update(cx, |v, _| v.selection.is_empty()),
            "{density:?}: a click beside the rows selects nothing"
        );
        // A marquee from beside row 1 to the middle of row 3 selects rows 1 to 3.
        let from = beside(1.);
        let to = point(row0.left() + px(200.), row0.top() + row * 3.5);
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        assert_eq!(
            view.update(cx, |v, _| selected_names(v)).len(),
            3,
            "{density:?}: the marquee starts in the inset"
        );
        cx.simulate_click(beside(5.), Modifiers::none());
    }
}
