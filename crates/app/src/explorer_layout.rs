//! Layout math for the explorer pane, kept free of gpui state so it can be unit tested:
//!
//! - the Details columns' geometry ([`columns`]),
//! - whether the preview pane fits ([`fit`]),
//! - the preview pane's width for a window size, or the width the user dragged it to
//!   ([`preview_width`]), and how wide it may be dragged ([`preview_room`]),
//! - how wide a split tab's shared preview is ([`shared_preview`]),
//! - how wide the column view's columns may be dragged ([`parent_room`], [`current_room`]),
//! - which breadcrumb segments collapse into "…" ([`collapse_crumbs`]),
//! - how wide the title row's search box is ([`search_width`]).
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
pub(super) fn name_min() -> gpui::Pixels {
    crate::theme::scaled_px(200.)
}
/// Preview pane width bounds; in between it takes [`PREVIEW_SHARE`] of the window.
pub(super) fn preview_w_min() -> gpui::Pixels {
    crate::theme::scaled_px(280.)
}
pub(super) fn preview_w_max() -> gpui::Pixels {
    crate::theme::scaled_px(380.)
}
/// Share of the window width the preview pane takes between its bounds.
const PREVIEW_SHARE: f32 = 0.22;
/// Narrowest the preview pane gets when squeezed by a narrow pane; below this it's hidden instead.
/// Also the narrowest it can be dragged.
pub(super) fn preview_min() -> gpui::Pixels {
    crate::theme::scaled_px(220.)
}
/// Widest the preview pane can be dragged (the list keeps its own minimum on top of this).
pub(super) fn preview_drag_max() -> gpui::Pixels {
    crate::theme::scaled_px(960.)
}
/// Details row padding: left and right (the last column keeps this much air before its divider).
pub(super) fn row_pad_l() -> Pixels {
    theme::s_12()
}
pub(super) fn row_pad_r() -> Pixels {
    theme::s_16()
}
/// Gap between Details columns. A column's divider sits in the middle of the gap after it.
pub(super) fn col_gap() -> Pixels {
    theme::s_12()
}
/// Blank strip right of the columns, even when they fill the pane or scroll: there's always empty
/// space beside the rows to right-click for the folder menu or to start a marquee.
pub(super) fn list_gutter() -> Pixels {
    theme::s_24()
}
/// Horizontal space a Details row can't use for columns: [`paper_margin()`], the list's left
/// inset ([`theme::list_inset()`]) and right padding (8), [`list_gutter()`], the row's padding
/// ([`row_pad_l()`] + [`row_pad_r()`]), and the icon with its gap. At standard spacing this is
/// what it was before the inset (#448): the inset took the place of 8 px once counted as a left
/// margin the paper doesn't have.
pub(super) fn row_chrome() -> Pixels {
    paper_margin() + theme::list_inset() + theme::scaled_px(8. + 24. + 12. + 16. + 16. + 12.)
}
/// What a pane's box loses before its rows: [`paper_margin()`], the list's left inset and right
/// padding, and [`list_gutter()`].
pub(super) fn list_chrome() -> Pixels {
    paper_margin() + theme::list_inset() + theme::scaled_px(8. + 24.)
}
/// The paper's right margin (its left edge touches the sidebar or the split divider): a pane's
/// box less this is the list and the preview pane.
pub(super) fn paper_margin() -> Pixels {
    theme::paper_gap()
}
/// Column view: narrowest a column can be dragged, and the bounds of fitting one to its names.
pub(super) fn view_column_min() -> gpui::Pixels {
    crate::theme::scaled_px(160.)
}
pub(super) fn view_column_fit_max() -> gpui::Pixels {
    crate::theme::scaled_px(600.)
}

/// A Details row's geometry: the Name column's width (its checkbox, tree indent and icon
/// included) and the whole row's, padding included. Rows, their selection wash and the header
/// span exactly [`Self::row`]; the list is blank background after that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Columns {
    pub(super) name: Pixels,
    pub(super) row: Pixels,
}

/// The Details columns in a list with `room` for rows (the pane less [`list_chrome()`]). `name` is
/// the width the user gave the Name column; until then it takes what the other columns
/// (`others`, left to right, as shown and sized) leave, kept between [`theme::col_name()`] and
/// [`theme::col_name_max()`] so it's roomy in a wide window without pushing the other columns far
/// away. So Name gives way first when the pane narrows (the preview pane widens, a column is
/// switched on or sized wider); only once it's at its minimum do the columns scroll.
pub(super) fn columns(room: Pixels, name: Option<Pixels>, others: &[Pixels]) -> Columns {
    let rest = others.iter().fold(px(0.), |sum, w| sum + col_gap() + *w);
    let pad = row_pad_l() + row_pad_r();
    let name =
        name.unwrap_or_else(|| (room - pad - rest).clamp(theme::col_name(), theme::col_name_max()));
    Columns {
        name,
        row: pad + name + rest,
    }
}

/// Whether the preview pane fits at the current width, decided before each render. In a narrow
/// pane it narrows (down to [`preview_min()`]) and then hides so the list keeps its minimum; the
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
            preview_w: preview_w_min(),
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
    /// Column view: the width the user gave the current folder's column (`None`: it fills what
    /// the preview leaves). Set, the preview takes the rest instead.
    pub(super) current_w: Option<Pixels>,
    /// Location column in search results.
    pub(super) location_w: Pixels,
    /// Each Details column's width, by [`Col`] (`None` when switched off; Name is ignored).
    pub(super) col_w: [Option<Pixels>; Col::ALL.len()],
}

/// Preview pane width for a window `window` wide. Automatic (`user` is `None`): 22 % of the
/// window, kept between 280 and 380 px so it neither crowds a laptop screen nor turns into a
/// billboard on a wide monitor. Dragged: that width, between [`preview_min()`] and
/// [`preview_drag_max()`]. Either way [`fit`] may still squeeze or hide it in a narrow pane.
pub(super) fn preview_width(window: Pixels, user: Option<Pixels>) -> Pixels {
    user.map_or_else(
        || (window * PREVIEW_SHARE).clamp(preview_w_min(), preview_w_max()),
        |w| w.clamp(preview_min(), preview_drag_max()),
    )
}

/// Widest the preview pane can be dragged in this pane: whatever the list leaves at its minimum
/// ([`bare_list`]), up to [`preview_drag_max()`]. Never below [`preview_min()`], so a drag in a
/// cramped pane still has a valid range.
pub(super) fn preview_room(input: &FitInput) -> Pixels {
    (input.bar - list_min(input)).clamp(preview_min(), preview_drag_max())
}

/// The narrowest list the preview pane leaves: row chrome, a name [`name_min()`] wide and the Size
/// column (search: its fixed columns; column view: the parent column). Wider columns scroll.
fn list_min(input: &FitInput) -> Pixels {
    let gap = col_gap();
    let width = |col: Col| input.col_w[col as usize];
    let mut fixed = row_chrome() + name_min();
    if input.checkboxes {
        fixed += theme::icon() + gap;
    }
    // Search modified/size are always shown; fall back to the default widths if switched off.
    if input.columns_view {
        // A current folder column the user made narrower than that is its own minimum.
        if let Some(current) = input.current_w {
            fixed = fixed.min(paper_margin() + current);
        }
        fixed += input.parent_w;
    } else if input.search {
        let modified = width(Col::Modified).unwrap_or_else(theme::col_modified);
        let size = width(Col::Size).unwrap_or_else(theme::col_size);
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
    let preview_w = match input.current_w {
        // Like Finder's last column: the sized columns keep their widths, the preview gets the
        // rest (so the current column comes out exactly as wide as the user made it).
        Some(current) if input.columns_view => {
            (input.bar - paper_margin() - input.parent_w - current).max(preview_min())
        }
        _ => preview_width(input.window, input.preview_user),
    };
    if input.wants_preview {
        if input.bar - preview_w >= list {
            return Fit {
                preview: true,
                preview_w,
            };
        }
        // Too narrow for the list next to it: narrow the preview pane before hiding it.
        let squeezed = input.bar - list;
        if squeezed >= preview_min() {
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
pub(crate) fn shared_gap() -> gpui::Pixels {
    theme::paper_gap()
}

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
        self.width + shared_gap()
    }
}

/// The shared preview of a split tab `content` wide (the window minus the sidebar) in a window
/// `window` wide. `ratio` is the left pane's share of what the preview leaves and `mins` each
/// pane's narrowest list ([`list_min`]). Like one pane's preview in [`fit`]: it keeps its width
/// while both panes have their narrowest list (wider columns scroll), then narrows down to
/// [`preview_min()`], then hides (`None`).
pub(crate) fn shared_preview(
    window: Pixels,
    content: Pixels,
    ratio: f32,
    mins: [Pixels; 2],
    user: Option<Pixels>,
) -> Option<SharedPreview> {
    let ratio = ratio.clamp(0.05, 0.95);
    let needed = (mins[0] / ratio).max(mins[1] / (1. - ratio));
    let room = content - shared_gap() - needed;
    if room < preview_min() {
        return None;
    }
    Some(SharedPreview {
        width: preview_width(window, user).min(room),
        room: room.min(preview_drag_max()),
    })
}

/// A pane's narrowest list: what [`fit`] leaves it before the preview pane gives way.
pub(super) fn bare_list(input: &FitInput) -> Pixels {
    list_min(input)
}

/// What a shown preview needs while a column view column is sized: its minimum.
fn preview_keep(input: &FitInput) -> Pixels {
    if input.wants_preview && fit(input).preview {
        preview_min()
    } else {
        px(0.)
    }
}

/// Column view: widest the parent column may be dragged, so the current folder's column keeps
/// its minimum (see [`list_min`]) and a shown preview keeps [`preview_min()`].
pub(super) fn parent_room(input: &FitInput) -> Pixels {
    let others = list_min(&FitInput {
        parent_w: px(0.),
        ..*input
    });
    (input.bar - others - preview_keep(input)).max(view_column_min())
}

/// Column view: widest the current folder's column may be made, leaving a shown preview its
/// minimum.
pub(super) fn current_room(input: &FitInput) -> Pixels {
    (input.bar - paper_margin() - input.parent_w - preview_keep(input)).max(view_column_min())
}

/// Breadcrumb segments to hide behind "…" so the path fits in `avail`.
///
/// `widths` are the segments' drawn widths (each with its leading ›), `more` the width of the
/// "…" piece. All fit: `None`, every level shows. Otherwise levels are hidden from the middle
/// outwards, keeping the drive (first) and the current folder (last) and preferring to keep the
/// levels nearest the current folder: the returned range is never empty and never includes the
/// last segment. If even hiding everything between first and last doesn't fit, the drive goes too
/// ("… › current folder") and the current folder truncates.
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
    if shown(&hidden) > avail {
        hidden.start = 0;
    }
    Some(hidden)
}

/// Narrowest search box that still shows its text field. Below it, an empty search box that isn't
/// in use shows only its magnifier.
pub(super) fn search_min() -> Pixels {
    crate::theme::scaled_px(120.)
}

/// What the search box in the title row is doing, which decides its width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SearchBox {
    /// Not focused; `text`: it holds a query (or a search runs).
    Idle { text: bool },
    /// Focused: grows with its text.
    Typing,
}

/// Width of the search box in `room` (the title row between its buttons). `rest`: its usual
/// width. While typing it grows with `content` (what it needs to show the text, its icon, clear
/// button and scope chip) into the rest of `room`. Zero `room` (not measured yet): the usual
/// width.
pub(super) fn search_width(
    room: Pixels,
    state: SearchBox,
    rest: Pixels,
    content: Pixels,
) -> Pixels {
    if room <= px(0.) {
        return rest;
    }
    let fitted = match state {
        SearchBox::Typing => content.max(rest).min(room),
        SearchBox::Idle { .. } => rest.min(room),
    };
    if fitted >= search_min() {
        return fitted;
    }
    match state {
        // In use: keep the field as long as there's room for it.
        SearchBox::Typing | SearchBox::Idle { text: true } => search_min().min(room),
        SearchBox::Idle { text: false } => theme::h_ctl(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_box_width_per_state() {
        let rest = px(400.);
        let idle = SearchBox::Idle { text: false };
        let with_text = SearchBox::Idle { text: true };
        let typing = SearchBox::Typing;
        let w = |room: f32, state: SearchBox, content: f32| {
            search_width(px(room), state, rest, px(content))
        };
        // A roomy row: the usual width, idle with or without text.
        assert_eq!(w(900., idle, 0.), rest);
        assert_eq!(w(900., with_text, 1200.), rest);
        // Typing grows with the text from the usual width into the room there is.
        assert_eq!(w(900., typing, 0.), rest);
        assert_eq!(w(900., typing, 300.), rest);
        assert_eq!(w(900., typing, 600.), px(600.));
        assert_eq!(w(900., typing, 1200.), px(900.));
        // A narrow row: the box takes what there is.
        assert_eq!(w(260., idle, 0.), px(260.));
        // Narrower still: an unused box folds to its magnifier, one in use keeps its minimum.
        assert_eq!(w(100., idle, 0.), theme::h_ctl());
        assert_eq!(w(100., with_text, 0.), px(100.));
        assert_eq!(w(100., typing, 900.), px(100.));
        // Not measured yet.
        assert_eq!(w(0., idle, 0.), rest);
    }

    fn input(bar: f32) -> FitInput {
        FitInput {
            bar: px(bar),
            window: px(bar) + theme::sidebar_width(),
            details: true,
            checkboxes: false,
            columns_view: false,
            search: false,
            wants_preview: false,
            preview_user: None,
            parent_w: crate::theme::scaled_px(220.),
            current_w: None,
            location_w: theme::col_location(),
            col_w: widths(&[
                (Col::Modified, theme::col_modified()),
                (Col::Type, theme::col_type()),
                (Col::Size, theme::col_size()),
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
        row_chrome() + name_min() + col_gap() + theme::col_size()
    }

    fn others() -> [Pixels; 3] {
        [theme::col_modified(), theme::col_type(), theme::col_size()]
    }

    /// The preview pane scales with the window between its bounds.
    #[test]
    fn preview_width_is_a_clamped_share_of_the_window() {
        assert_eq!(
            preview_width(crate::theme::scaled_px(1000.), None),
            preview_w_min()
        );
        assert_eq!(
            preview_width(crate::theme::scaled_px(1500.), None),
            crate::theme::scaled_px(330.)
        );
        assert_eq!(
            preview_width(crate::theme::scaled_px(1936.), None),
            preview_w_max()
        );
    }

    /// A dragged width wins over the window share, within the drag bounds.
    #[test]
    fn dragged_preview_width_is_kept_within_bounds() {
        assert_eq!(
            preview_width(
                crate::theme::scaled_px(1000.),
                Some(crate::theme::scaled_px(520.))
            ),
            crate::theme::scaled_px(520.)
        );
        assert_eq!(
            preview_width(
                crate::theme::scaled_px(1000.),
                Some(crate::theme::scaled_px(50.))
            ),
            preview_min()
        );
        assert_eq!(
            preview_width(
                crate::theme::scaled_px(4000.),
                Some(crate::theme::scaled_px(5000.))
            ),
            preview_drag_max()
        );
    }

    /// A wide dragged pane keeps its width while the bare list fits beside it (the columns
    /// scroll), then is squeezed to what that list leaves, and hides when not even that fits.
    #[test]
    fn dragged_preview_squeezes_like_the_automatic_one() {
        let mut i = input(1200.);
        i.wants_preview = true;
        i.preview_user = Some(crate::theme::scaled_px(700.));
        let f = fit(&i);
        assert!(
            f.preview && f.preview_w == crate::theme::scaled_px(700.),
            "{f:?}"
        );
        i.bar = bare() + crate::theme::scaled_px(500.);
        let f = fit(&i);
        assert!(
            f.preview && f.preview_w == crate::theme::scaled_px(500.),
            "{f:?}"
        );
        i.bar = bare() + preview_min() - px(1.);
        assert!(!fit(&i).preview);
    }

    /// The drag range leaves the bare list its room, and stays valid in a cramped pane.
    #[test]
    fn preview_room_leaves_the_list_its_minimum() {
        let mut i = input(0.);
        i.bar = bare() + crate::theme::scaled_px(600.);
        assert_eq!(preview_room(&i), crate::theme::scaled_px(600.));
        i.bar = crate::theme::scaled_px(300.);
        assert_eq!(preview_room(&i), preview_min());
        i.bar = crate::theme::scaled_px(4000.);
        assert_eq!(preview_room(&i), preview_drag_max());
    }

    /// A split tab's preview keeps its width while both panes fit their bare list, then
    /// narrows to its minimum and hides; the panes share what it leaves by the divider.
    #[test]
    fn shared_preview_narrows_then_hides() {
        let mins = [bare(), bare()];
        let window = crate::theme::scaled_px(1600.);
        let want = preview_width(window, None);
        let wide =
            shared_preview(window, crate::theme::scaled_px(1400.), 0.5, mins, None).expect("fits");
        assert_eq!(wide.width, want);
        assert_eq!(wide.reserve(), want + shared_gap());
        // Exactly two bare lists, the gap and the minimum preview.
        let tight = bare() * 2. + shared_gap() + preview_min();
        let f = shared_preview(window, tight, 0.5, mins, None).expect("fits at its minimum");
        assert_eq!(f.width, preview_min());
        assert!(shared_preview(window, tight - px(1.), 0.5, mins, None).is_none());
        // An uneven divider: the narrower side decides.
        let left =
            shared_preview(window, crate::theme::scaled_px(1400.), 0.4, mins, None).expect("fits");
        assert_eq!(
            left.room,
            crate::theme::scaled_px(1400.) - shared_gap() - bare() / 0.4
        );
        // A dragged width is kept within what the panes leave.
        let dragged = shared_preview(
            window,
            crate::theme::scaled_px(1400.),
            0.5,
            mins,
            Some(crate::theme::scaled_px(900.)),
        )
        .expect("fits");
        assert_eq!(
            dragged.width,
            crate::theme::scaled_px(1400.) - shared_gap() - bare() * 2.
        );
        assert_eq!(dragged.width, dragged.room);
    }

    /// Columns are laid out left to right: the row is its padding, Name and each other column
    /// with the gap before it, so it ends where the last column does, whatever the pane's width.
    #[test]
    fn the_row_spans_exactly_the_columns() {
        let rest = others().iter().fold(px(0.), |s, w| s + col_gap() + *w);
        for room in [
            crate::theme::scaled_px(400.),
            crate::theme::scaled_px(900.),
            crate::theme::scaled_px(2400.),
        ] {
            let c = columns(room, Some(crate::theme::scaled_px(300.)), &others());
            assert_eq!(
                c.name,
                crate::theme::scaled_px(300.),
                "the user's width wins at {room:?}"
            );
            assert_eq!(
                c.row,
                row_pad_l() + crate::theme::scaled_px(300.) + rest + row_pad_r()
            );
        }
        // Each column's right edge (where its divider is) moves with the columns before it only.
        let narrow = columns(
            crate::theme::scaled_px(900.),
            Some(crate::theme::scaled_px(300.)),
            &others(),
        );
        let wide = columns(
            crate::theme::scaled_px(900.),
            Some(crate::theme::scaled_px(360.)),
            &others(),
        );
        assert_eq!(wide.row - narrow.row, crate::theme::scaled_px(60.));
    }

    /// Until the user sizes it, Name takes what the other columns leave, between its bounds: a
    /// wide window gets a roomy name and blank space after the columns, a narrow one the minimum
    /// (and scrolls sideways). A wider or an extra column takes its room from Name.
    #[test]
    fn the_default_name_width_follows_the_room() {
        let rest =
            others().iter().fold(px(0.), |s, w| s + col_gap() + *w) + row_pad_l() + row_pad_r();
        let mid = columns(rest + crate::theme::scaled_px(380.), None, &others());
        assert_eq!(mid.name, crate::theme::scaled_px(380.));
        assert_eq!(
            mid.row,
            rest + crate::theme::scaled_px(380.),
            "fills the room exactly"
        );
        let wide = columns(crate::theme::scaled_px(2400.), None, &others());
        assert_eq!(wide.name, theme::col_name_max());
        assert!(
            wide.row < crate::theme::scaled_px(2400.),
            "blank background after the columns"
        );
        let wider = [
            theme::col_modified(),
            theme::col_type(),
            crate::theme::scaled_px(200.),
        ];
        let c = columns(rest + crate::theme::scaled_px(380.), None, &wider);
        assert_eq!(
            c.name,
            crate::theme::scaled_px(380.) - (crate::theme::scaled_px(200.) - theme::col_size())
        );
        assert_eq!(
            c.row,
            rest + crate::theme::scaled_px(380.),
            "a wider Size still fits"
        );
        let more = [
            theme::col_modified(),
            theme::col_modified(),
            theme::col_type(),
            theme::col_size(),
        ];
        let c = columns(rest + crate::theme::scaled_px(420.), None, &more);
        assert_eq!(
            c.name,
            crate::theme::scaled_px(420.) - col_gap() - theme::col_modified()
        );
        assert_eq!(
            c.row,
            rest + crate::theme::scaled_px(420.),
            "so does an extra column"
        );
        let narrow = columns(crate::theme::scaled_px(300.), None, &others());
        assert_eq!(narrow.name, theme::col_name());
        assert!(
            narrow.row > crate::theme::scaled_px(300.),
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
            i.window = bar + theme::sidebar_width();
            i.wants_preview = true;
            fit(&i)
        };
        let wide = with_preview(crate::theme::scaled_px(1400.));
        assert!(
            wide.preview && wide.preview_w == preview_width(crate::theme::scaled_px(1648.), None)
        );
        let at = bare() + preview_w_min();
        let f = with_preview(at);
        assert!(f.preview && f.preview_w == preview_w_min(), "{f:?}");
        let f = with_preview(at - crate::theme::scaled_px(10.));
        assert!(
            f.preview && f.preview_w == preview_w_min() - crate::theme::scaled_px(10.),
            "{f:?}"
        );
        let f = with_preview(bare() + preview_min());
        assert!(f.preview && f.preview_w == preview_min(), "{f:?}");
        let f = with_preview(bare() + preview_min() - px(1.));
        assert!(!f.preview, "{f:?}");
        assert!(!fit(&input(1400.)).preview, "only when switched on");
    }

    /// Column view with a sized current column: the preview takes what the two columns leave,
    /// down to its minimum (the current column gives way after that), then hides. Unsized, the
    /// preview keeps its own width as before.
    #[test]
    fn column_view_preview_takes_what_the_columns_leave() {
        let columns = |bar: f32, current: Option<f32>| {
            let mut i = input(bar);
            i.details = false;
            i.columns_view = true;
            i.wants_preview = true;
            i.parent_w = crate::theme::scaled_px(240.);
            i.current_w = current.map(px);
            i
        };
        let f = fit(&columns(1400., Some(300.)));
        assert!(
            f.preview && f.preview_w == px(1400. - 4. - 240. - 300.),
            "{f:?}"
        );
        let f = fit(&columns(1400., None));
        assert_eq!(
            f.preview_w,
            preview_width(
                crate::theme::scaled_px(1400.) + theme::sidebar_width(),
                None
            )
        );
        // No room left for the preview's minimum: it stays at that while the list keeps its own.
        let f = fit(&columns(800., Some(400.)));
        assert!(f.preview && f.preview_w == preview_min(), "{f:?}");
        assert!(!fit(&columns(500., Some(400.))).preview);
        // A narrow current column is its own minimum, so a small pane keeps the preview.
        let f = fit(&columns(4. + 240. + 160. + 300., Some(160.)));
        assert!(
            f.preview && f.preview_w == crate::theme::scaled_px(300.),
            "{f:?}"
        );
        // Dragging stops where the preview would go below its minimum.
        let i = columns(1400., Some(300.));
        assert_eq!(current_room(&i), px(1400. - 4. - 240.) - preview_min());
        // The parent may take room until the current column is down to its own width.
        assert_eq!(
            parent_room(&i),
            crate::theme::scaled_px(1400.)
                - paper_margin()
                - crate::theme::scaled_px(300.)
                - preview_min()
        );
        assert_eq!(
            parent_room(&columns(1400., None)),
            crate::theme::scaled_px(1400.) - row_chrome() - name_min() - preview_min()
        );
        assert_eq!(parent_room(&columns(300., None)), view_column_min());
    }

    fn crumbs(widths: &[f32]) -> Vec<Pixels> {
        widths.iter().map(|w| px(*w)).collect()
    }

    /// Wide enough: the whole path shows, nothing collapses.
    #[test]
    fn crumbs_show_in_full_when_they_fit() {
        let w = crumbs(&[40., 80., 80., 80., 120.]);
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(400.),
                crate::theme::scaled_px(30.)
            ),
            None
        );
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(1000.),
                crate::theme::scaled_px(30.)
            ),
            None
        );
        // Two levels never collapse (there's no middle).
        assert_eq!(
            collapse_crumbs(
                &crumbs(&[40., 900.]),
                crate::theme::scaled_px(100.),
                crate::theme::scaled_px(30.)
            ),
            None
        );
    }

    /// Too narrow: levels collapse from the middle, keeping the drive and the current folder.
    #[test]
    fn crumbs_collapse_from_the_middle() {
        let w = crumbs(&[40., 80., 80., 80., 80., 120.]);
        // Hiding one middle level is enough.
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(450.),
                crate::theme::scaled_px(30.)
            ),
            Some(2..3)
        );
        // Then it grows outwards, alternating, towards the drive first.
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(370.),
                crate::theme::scaled_px(30.)
            ),
            Some(1..3)
        );
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(300.),
                crate::theme::scaled_px(30.)
            ),
            Some(1..4)
        );
        // Only the drive, "…" and the current folder fit.
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(190.),
                crate::theme::scaled_px(30.)
            ),
            Some(1..5)
        );
        // Not even that: the drive goes too, leaving "… › current folder".
        assert_eq!(
            collapse_crumbs(
                &w,
                crate::theme::scaled_px(50.),
                crate::theme::scaled_px(30.)
            ),
            Some(0..5)
        );
    }
}
