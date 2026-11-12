//! Layout math for the explorer pane, kept free of gpui state so it can be unit tested:
//!
//! - the Details columns' geometry ([`columns`]),
//! - whether the preview pane fits ([`fit`]),
//! - the preview pane's width for a window size, or the width the user dragged it to
//!   ([`preview_width`]), and how wide it may be dragged ([`preview_room`]),
//! - how wide a split tab's shared preview is ([`shared_preview`]),
//! - which breadcrumb segments collapse into "…" ([`collapse_crumbs`]).
//!
//! Details columns work like File Explorer's: laid out left to right from Name, each with its own
//! width (Name too). What's right of the last column is blank list background; when the columns
//! are wider than the pane, the list scrolls sideways instead of dropping any.

use std::ops::Range;

use gpui::{Pixels, px};

use crate::prefs::Col;
use crate::theme;

/// The narrowest list the preview pane leaves: room for a name this wide and the Size column.
/// Below that the preview pane narrows and then hides.
pub(super) const NAME_MIN: Pixels = px(200.);
/// Preview pane width bounds; in between it takes [`PREVIEW_SHARE`] of the window.
pub(super) const PREVIEW_W_MIN: Pixels = px(280.);
pub(super) const PREVIEW_W_MAX: Pixels = px(380.);
/// Share of the window width the preview pane takes between its bounds.
const PREVIEW_SHARE: f32 = 0.22;
/// Narrowest the preview pane gets when squeezed by a narrow pane; below this it's hidden instead.
/// Also the narrowest it can be dragged.
pub(super) const PREVIEW_MIN: Pixels = px(220.);
/// Widest the preview pane can be dragged (the list keeps its own minimum on top of this).
pub(super) const PREVIEW_DRAG_MAX: Pixels = px(960.);
/// Details row padding: left and right (the last column keeps this much air before its divider).
pub(super) const ROW_PAD_L: Pixels = theme::S_12;
pub(super) const ROW_PAD_R: Pixels = theme::S_16;
/// Gap between Details columns. A column's divider sits in the middle of the gap after it.
pub(super) const COL_GAP: Pixels = theme::S_12;
/// Blank strip right of the columns, even when they fill the pane or scroll: there's always empty
/// space beside the rows to right-click for the folder menu or to start a marquee.
pub(super) const LIST_GUTTER: Pixels = theme::S_24;
/// Horizontal space a Details row can't use for columns: the paper's outer margins (2 × 8), the
/// list's side padding (2 × 8), [`LIST_GUTTER`], the row's padding ([`ROW_PAD_L`] +
/// [`ROW_PAD_R`]), and the icon with its gap.
pub(super) const ROW_CHROME: Pixels = px(16. + 16. + 24. + 12. + 16. + 16. + 12.);
/// What a pane's box loses before its rows: the paper's outer margins, the list's side padding
/// and [`LIST_GUTTER`].
pub(super) const LIST_CHROME: Pixels = px(16. + 16. + 24.);

/// A Details row's geometry: the Name column's width (its checkbox, tree indent and icon
/// included) and the whole row's, padding included. Rows, their selection wash and the header
/// span exactly [`Self::row`]; the list is blank background after that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Columns {
    pub(super) name: Pixels,
    pub(super) row: Pixels,
}

/// What the default columns (Modified, Type and Size, with their gaps and the row's padding)
/// take next to Name; the automatic Name width is what's left of the pane after them.
const DEFAULT_OTHERS: Pixels = px(12. + 16. + (12. + 140.) + (12. + 130.) + (12. + 84.));

/// The Details columns in a list with `room` for rows (the pane less [`LIST_CHROME`]). `name` is
/// the width the user gave the Name column; until then it takes what the default columns would
/// leave, kept between [`theme::COL_NAME`] and [`theme::COL_NAME_MAX`] so it's roomy in a wide
/// window without pushing the other columns far away. It follows the pane only, never the other
/// columns (`others`, left to right), so sizing one of them never moves Name's edge.
pub(super) fn columns(room: Pixels, name: Option<Pixels>, others: &[Pixels]) -> Columns {
    let rest = others.iter().fold(px(0.), |sum, w| sum + COL_GAP + *w);
    let name =
        name.unwrap_or_else(|| (room - DEFAULT_OTHERS).clamp(theme::COL_NAME, theme::COL_NAME_MAX));
    Columns {
        name,
        row: ROW_PAD_L + ROW_PAD_R + name + rest,
    }
}

/// Whether the preview pane fits at the current width, decided before each render. In a narrow
/// pane it narrows (down to [`PREVIEW_MIN`]) and then hides so the list keeps its minimum; the
/// Details columns keep their widths and scroll sideways instead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Fit {
    /// Preview pane, or the column view's preview column, when switched on.
    pub(super) preview: bool,
    /// Width of the preview pane when shown.
    pub(super) preview_w: Pixels,
}

impl Default for Fit {
    fn default() -> Self {
        Self {
            preview: true,
            preview_w: PREVIEW_W_MIN,
        }
    }
}

/// Everything [`fit`] needs about the pane and the visible columns.
#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat snapshot of independent view flags, read once by `fit`"
)]
pub(super) struct FitInput {
    /// The pane's full width (window minus sidebar, times the split share).
    pub(super) bar: Pixels,
    /// The whole window's width (the preview pane scales with it).
    pub(super) window: Pixels,
    /// Details view outside search.
    pub(super) details: bool,
    pub(super) checkboxes: bool,
    /// Column view: a fixed parent column instead of Details columns.
    pub(super) columns_view: bool,
    /// Search results: fixed location, modified and size columns.
    pub(super) search: bool,
    /// Preview pane (or the column view's preview column) is switched on.
    pub(super) wants_preview: bool,
    /// Width the user dragged the preview pane to (`None`: automatic, see [`preview_width`]).
    pub(super) preview_user: Option<Pixels>,
    /// Width of the parent column in column view.
    pub(super) parent_w: Pixels,
    /// Location column in search results.
    pub(super) location_w: Pixels,
    /// Each Details column's width, by [`Col`] (`None` when switched off; Name is ignored).
    pub(super) col_w: [Option<Pixels>; Col::ALL.len()],
}

/// Preview pane width for a window `window` wide. Automatic (`user` is `None`): 22 % of the
/// window, kept between 280 and 380 px so it neither crowds a laptop screen nor turns into a
/// billboard on a wide monitor. Dragged: that width, between [`PREVIEW_MIN`] and
/// [`PREVIEW_DRAG_MAX`]. Either way [`fit`] may still squeeze or hide it in a narrow pane.
pub(super) fn preview_width(window: Pixels, user: Option<Pixels>) -> Pixels {
    user.map_or_else(
        || (window * PREVIEW_SHARE).clamp(PREVIEW_W_MIN, PREVIEW_W_MAX),
        |w| w.clamp(PREVIEW_MIN, PREVIEW_DRAG_MAX),
    )
}

/// Widest the preview pane can be dragged in this pane: whatever the list leaves at its minimum
/// ([`bare_list`]), up to [`PREVIEW_DRAG_MAX`]. Never below [`PREVIEW_MIN`], so a drag in a
/// cramped pane still has a valid range.
pub(super) fn preview_room(input: &FitInput) -> Pixels {
    (input.bar - list_min(input)).clamp(PREVIEW_MIN, PREVIEW_DRAG_MAX)
}

/// The narrowest list the preview pane leaves: row chrome, a name [`NAME_MIN`] wide and the Size
/// column (search: its fixed columns; column view: the parent column). Wider columns scroll.
fn list_min(input: &FitInput) -> Pixels {
    let gap = COL_GAP;
    let width = |col: Col| input.col_w[col as usize];
    let mut fixed = ROW_CHROME + NAME_MIN;
    if input.checkboxes {
        fixed += theme::ICON + gap;
    }
    // Search modified/size are always shown; fall back to the default widths if switched off.
    if input.columns_view {
        fixed += input.parent_w;
    } else if input.search {
        let modified = width(Col::Modified).unwrap_or(theme::COL_MODIFIED);
        let size = width(Col::Size).unwrap_or(theme::COL_SIZE);
        fixed += gap * 3. + input.location_w + modified + size;
    } else if input.details
        && let Some(size) = width(Col::Size)
    {
        fixed += gap + size;
    }
    fixed
}

/// Decides whether and how wide the preview pane shows (see [`Fit`]).
pub(super) fn fit(input: &FitInput) -> Fit {
    let list = list_min(input);
    let preview_w = preview_width(input.window, input.preview_user);
    if input.wants_preview {
        if input.bar - preview_w >= list {
            return Fit {
                preview: true,
                preview_w,
            };
        }
        // Too narrow for the list next to it: narrow the preview pane before hiding it.
        let squeezed = input.bar - list;
        if squeezed >= PREVIEW_MIN {
            return Fit {
                preview: true,
                preview_w: squeezed,
            };
        }
    }
    Fit {
        preview: false,
        preview_w,
    }
}

/// Space between the right pane's paper and a split tab's shared preview.
pub(crate) const SHARED_GAP: Pixels = theme::S_8;

/// A split tab's preview: one for the whole tab, at its right end after both panes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SharedPreview {
    /// Width of the preview.
    pub(crate) width: Pixels,
    /// Widest it may be dragged before a pane would get narrower than its bare list.
    pub(crate) room: Pixels,
}

impl SharedPreview {
    /// Room the preview takes at the right end of the tab, its gap to the right pane included.
    pub(crate) fn reserve(self) -> Pixels {
        self.width + SHARED_GAP
    }
}

/// The shared preview of a split tab `content` wide (the window minus the sidebar) in a window
/// `window` wide. `ratio` is the left pane's share of what the preview leaves and `mins` each
/// pane's narrowest list ([`list_min`]). Like one pane's preview in [`fit`]: it keeps its width
/// while both panes have their narrowest list (wider columns scroll), then narrows down to
/// [`PREVIEW_MIN`], then hides (`None`).
pub(crate) fn shared_preview(
    window: Pixels,
    content: Pixels,
    ratio: f32,
    mins: [Pixels; 2],
    user: Option<Pixels>,
) -> Option<SharedPreview> {
    let ratio = ratio.clamp(0.05, 0.95);
    let needed = (mins[0] / ratio).max(mins[1] / (1. - ratio));
    let room = content - SHARED_GAP - needed;
    if room < PREVIEW_MIN {
        return None;
    }
    Some(SharedPreview {
        width: preview_width(window, user).min(room),
        room: room.min(PREVIEW_DRAG_MAX),
    })
}

/// A pane's narrowest list: what [`fit`] leaves it before the preview pane gives way.
pub(super) fn bare_list(input: &FitInput) -> Pixels {
    list_min(input)
}

/// Breadcrumb segments to hide behind "…" so the path fits in `avail`.
///
/// `widths` are the segments' drawn widths (each with its leading ›), `more` the width of the
/// "…" piece. All fit: `None`, every level shows. Otherwise levels are hidden from the middle
/// outwards, keeping the drive (first) and the current folder (last) and preferring to keep the
/// levels nearest the current folder: the returned range is never empty and never includes the
/// first or last segment. If even that doesn't fit, everything between first and last is hidden
/// and the current folder truncates.
pub(super) fn collapse_crumbs(
    widths: &[Pixels],
    avail: Pixels,
    more: Pixels,
) -> Option<Range<usize>> {
    let total: Pixels = widths.iter().fold(px(0.), |sum, w| sum + *w);
    let n = widths.len();
    if total <= avail || n < 3 {
        return None;
    }
    let shown = |hidden: &Range<usize>| {
        let hidden_w = widths[hidden.clone()]
            .iter()
            .fold(px(0.), |sum, w| sum + *w);
        total - hidden_w + more
    };
    // Start with the middle level (rounding towards the drive, so deeper levels survive longer).
    let mid = (n - 1) / 2;
    let mut hidden = mid.max(1)..(mid.max(1) + 1);
    let mut left_turn = true;
    while shown(&hidden) > avail {
        let left = match (hidden.start > 1, hidden.end < n - 1) {
            (false, false) => break,
            (true, false) => true,
            (false, true) => false,
            // Alternate, starting towards the drive: ancestors far from here go first.
            (true, true) => left_turn,
        };
        if left {
            hidden.start -= 1;
        } else {
            hidden.end += 1;
        }
        left_turn = !left_turn;
    }
    Some(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(bar: f32) -> FitInput {
        FitInput {
            bar: px(bar),
            window: px(bar) + theme::SIDEBAR_W,
            details: true,
            checkboxes: false,
            columns_view: false,
            search: false,
            wants_preview: false,
            preview_user: None,
            parent_w: px(220.),
            location_w: theme::COL_LOCATION,
            col_w: widths(&[
                (Col::Modified, theme::COL_MODIFIED),
                (Col::Type, theme::COL_TYPE),
                (Col::Size, theme::COL_SIZE),
            ]),
        }
    }

    fn widths(cols: &[(Col, Pixels)]) -> [Option<Pixels>; Col::ALL.len()] {
        let mut out = [None; Col::ALL.len()];
        for &(col, w) in cols {
            out[col as usize] = Some(w);
        }
        out
    }

    /// The narrowest list: chrome, the name's minimum and the Size column.
    fn bare() -> Pixels {
        ROW_CHROME + NAME_MIN + COL_GAP + theme::COL_SIZE
    }

    const OTHERS: [Pixels; 3] = [theme::COL_MODIFIED, theme::COL_TYPE, theme::COL_SIZE];

    /// The preview pane scales with the window between its bounds.
    #[test]
    fn preview_width_is_a_clamped_share_of_the_window() {
        assert_eq!(preview_width(px(1000.), None), PREVIEW_W_MIN);
        assert_eq!(preview_width(px(1500.), None), px(330.));
        assert_eq!(preview_width(px(1936.), None), PREVIEW_W_MAX);
    }

    /// A dragged width wins over the window share, within the drag bounds.
    #[test]
    fn dragged_preview_width_is_kept_within_bounds() {
        assert_eq!(preview_width(px(1000.), Some(px(520.))), px(520.));
        assert_eq!(preview_width(px(1000.), Some(px(50.))), PREVIEW_MIN);
        assert_eq!(preview_width(px(4000.), Some(px(5000.))), PREVIEW_DRAG_MAX);
    }

    /// A wide dragged pane keeps its width while the bare list fits beside it (the columns
    /// scroll), then is squeezed to what that list leaves, and hides when not even that fits.
    #[test]
    fn dragged_preview_squeezes_like_the_automatic_one() {
        let mut i = input(1200.);
        i.wants_preview = true;
        i.preview_user = Some(px(700.));
        let f = fit(&i);
        assert!(f.preview && f.preview_w == px(700.), "{f:?}");
        i.bar = bare() + px(500.);
        let f = fit(&i);
        assert!(f.preview && f.preview_w == px(500.), "{f:?}");
        i.bar = bare() + PREVIEW_MIN - px(1.);
        assert!(!fit(&i).preview);
    }

    /// The drag range leaves the bare list its room, and stays valid in a cramped pane.
    #[test]
    fn preview_room_leaves_the_list_its_minimum() {
        let mut i = input(0.);
        i.bar = bare() + px(600.);
        assert_eq!(preview_room(&i), px(600.));
        i.bar = px(300.);
        assert_eq!(preview_room(&i), PREVIEW_MIN);
        i.bar = px(4000.);
        assert_eq!(preview_room(&i), PREVIEW_DRAG_MAX);
    }

    /// A split tab's preview keeps its width while both panes fit their bare list, then
    /// narrows to its minimum and hides; the panes share what it leaves by the divider.
    #[test]
    fn shared_preview_narrows_then_hides() {
        let mins = [bare(), bare()];
        let window = px(1600.);
        let want = preview_width(window, None);
        let wide = shared_preview(window, px(1400.), 0.5, mins, None).expect("fits");
        assert_eq!(wide.width, want);
        assert_eq!(wide.reserve(), want + SHARED_GAP);
        // Exactly two bare lists, the gap and the minimum preview.
        let tight = bare() * 2. + SHARED_GAP + PREVIEW_MIN;
        let f = shared_preview(window, tight, 0.5, mins, None).expect("fits at its minimum");
        assert_eq!(f.width, PREVIEW_MIN);
        assert!(shared_preview(window, tight - px(1.), 0.5, mins, None).is_none());
        // An uneven divider: the narrower side decides.
        let left = shared_preview(window, px(1400.), 0.4, mins, None).expect("fits");
        assert_eq!(left.room, px(1400.) - SHARED_GAP - bare() / 0.4);
        // A dragged width is kept within what the panes leave.
        let dragged = shared_preview(window, px(1400.), 0.5, mins, Some(px(900.))).expect("fits");
        assert_eq!(dragged.width, px(1400.) - SHARED_GAP - bare() * 2.);
        assert_eq!(dragged.width, dragged.room);
    }

    /// Columns are laid out left to right: the row is its padding, Name and each other column
    /// with the gap before it, so it ends where the last column does, whatever the pane's width.
    #[test]
    fn the_row_spans_exactly_the_columns() {
        let rest = OTHERS.iter().fold(px(0.), |s, w| s + COL_GAP + *w);
        for room in [px(400.), px(900.), px(2400.)] {
            let c = columns(room, Some(px(300.)), &OTHERS);
            assert_eq!(c.name, px(300.), "the user's width wins at {room:?}");
            assert_eq!(c.row, ROW_PAD_L + px(300.) + rest + ROW_PAD_R);
        }
        // Each column's right edge (where its divider is) moves with the columns before it only.
        let narrow = columns(px(900.), Some(px(300.)), &OTHERS);
        let wide = columns(px(900.), Some(px(360.)), &OTHERS);
        assert_eq!(wide.row - narrow.row, px(60.));
    }

    /// Until the user sizes it, Name takes what the default columns leave, between its bounds: a
    /// wide window gets a roomy name and blank space after the columns, a narrow one the minimum
    /// (and scrolls sideways). Other columns' widths don't move it.
    #[test]
    fn the_default_name_width_follows_the_room() {
        let rest = OTHERS.iter().fold(px(0.), |s, w| s + COL_GAP + *w) + ROW_PAD_L + ROW_PAD_R;
        assert_eq!(
            rest, DEFAULT_OTHERS,
            "in step with the theme's default widths"
        );
        let mid = columns(rest + px(380.), None, &OTHERS);
        assert_eq!(mid.name, px(380.));
        assert_eq!(mid.row, rest + px(380.), "fills the room exactly");
        let wide = columns(px(2400.), None, &OTHERS);
        assert_eq!(wide.name, theme::COL_NAME_MAX);
        assert!(wide.row < px(2400.), "blank background after the columns");
        let wider = [theme::COL_MODIFIED, theme::COL_TYPE, px(200.)];
        assert_eq!(
            columns(rest + px(380.), None, &wider).name,
            px(380.),
            "a wider Size doesn't squeeze the name"
        );
        let narrow = columns(px(300.), None, &OTHERS);
        assert_eq!(narrow.name, theme::COL_NAME);
        assert!(
            narrow.row > px(300.),
            "wider than the pane: the list scrolls"
        );
    }

    /// Narrow panes keep every column (they scroll); only the preview pane gives way, narrowing
    /// to its minimum before it hides, and it comes back at full width when there's room.
    #[test]
    fn narrow_panes_keep_their_columns_and_squeeze_the_preview() {
        let with_preview = |bar: Pixels| {
            let mut i = input(0.);
            i.bar = bar;
            i.window = bar + theme::SIDEBAR_W;
            i.wants_preview = true;
            fit(&i)
        };
        let wide = with_preview(px(1400.));
        assert!(wide.preview && wide.preview_w == preview_width(px(1648.), None));
        let at = bare() + PREVIEW_W_MIN;
        let f = with_preview(at);
        assert!(f.preview && f.preview_w == PREVIEW_W_MIN, "{f:?}");
        let f = with_preview(at - px(10.));
        assert!(f.preview && f.preview_w == PREVIEW_W_MIN - px(10.), "{f:?}");
        let f = with_preview(bare() + PREVIEW_MIN);
        assert!(f.preview && f.preview_w == PREVIEW_MIN, "{f:?}");
        let f = with_preview(bare() + PREVIEW_MIN - px(1.));
        assert!(!f.preview, "{f:?}");
        assert!(!fit(&input(1400.)).preview, "only when switched on");
    }

    fn crumbs(widths: &[f32]) -> Vec<Pixels> {
        widths.iter().map(|w| px(*w)).collect()
    }

    /// Wide enough: the whole path shows, nothing collapses.
    #[test]
    fn crumbs_show_in_full_when_they_fit() {
        let w = crumbs(&[40., 80., 80., 80., 120.]);
        assert_eq!(collapse_crumbs(&w, px(400.), px(30.)), None);
        assert_eq!(collapse_crumbs(&w, px(1000.), px(30.)), None);
        // Two levels never collapse (there's no middle).
        assert_eq!(
            collapse_crumbs(&crumbs(&[40., 900.]), px(100.), px(30.)),
            None
        );
    }

    /// Too narrow: levels collapse from the middle, keeping the drive and the current folder.
    #[test]
    fn crumbs_collapse_from_the_middle() {
        let w = crumbs(&[40., 80., 80., 80., 80., 120.]);
        // Hiding one middle level is enough.
        assert_eq!(collapse_crumbs(&w, px(450.), px(30.)), Some(2..3));
        // Then it grows outwards, alternating, towards the drive first.
        assert_eq!(collapse_crumbs(&w, px(370.), px(30.)), Some(1..3));
        assert_eq!(collapse_crumbs(&w, px(300.), px(30.)), Some(1..4));
        // Nothing fits: everything between the drive and the current folder is hidden.
        assert_eq!(collapse_crumbs(&w, px(50.), px(30.)), Some(1..5));
    }
}
