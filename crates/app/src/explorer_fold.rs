//! How the selection's actions give way in the pane row (#388, #528), kept free of gpui state so
//! it can be tested over generated widths.
//!
//! As the room the path bar and the selection share shrinks, the selection gives way in steps,
//! the way File Explorer's and Finder's toolbars do, and the path bar keeps every level until
//! nothing else is left to give:
//!
//! 1. everything: the summary with the total size, every button, the whole path;
//! 2. the summary without the size, then just the pill "● 3" (its tooltip has the rest);
//! 3. Copy to/Move to the other side drop their labels, then the buttons move into "⋯" one at a
//!    time, the least used first and Delete last; × goes with the last of them;
//! 4. with only "● 3 ⋯" left, the path bar collapses levels into "…".
//!
//! Text is measured by the caller; this only adds widths up.

/// How the selection's summary reads, longest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Summary {
    /// "1 selected · 312 B".
    Full,
    /// "1 selected".
    Count,
    /// "● 1", with the whole summary in its tooltip.
    Pill,
}

/// The widths the selection's layouts are made of, as drawn.
#[derive(Clone, Debug)]
pub(super) struct SelectionParts {
    /// The summary with the total size (the same as [`Self::count`] without one).
    pub(super) full: f32,
    /// The summary without the size.
    pub(super) count: f32,
    /// The pill.
    pub(super) pill: f32,
    /// The buttons in the order they move into "⋯", the least used first.
    pub(super) buttons: Vec<f32>,
    /// What Copy to/Move to's labels add to the buttons (zero: they have none).
    pub(super) labels: f32,
    /// "⋯".
    pub(super) more: f32,
    /// The clear button (×).
    pub(super) clear: f32,
    /// The divider after the summary.
    pub(super) divider: f32,
    /// The gap between the row's pieces.
    pub(super) gap: f32,
    /// The gap between the pill and "⋯" once they are all that's left.
    pub(super) tight_gap: f32,
}

/// How the selection is drawn, and whether the path bar has to collapse levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SelectionLayout {
    pub(super) summary: Summary,
    /// Copy to/Move to show their labels.
    pub(super) labels: bool,
    /// How many of the buttons, from the least used, are in "⋯".
    pub(super) hidden: usize,
    /// How many buttons there are.
    pub(super) buttons: usize,
    /// Even "● 3 ⋯" leaves the path bar short of its whole width: it collapses levels (from its
    /// own slot's width, as it would anyway).
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the path bar collapses from its own width")
    )]
    pub(super) collapse: bool,
}

impl SelectionLayout {
    /// Whether button `ix` (in the order of [`SelectionParts::buttons`]) is drawn.
    pub(super) const fn shows(&self, ix: usize) -> bool {
        ix >= self.hidden && ix < self.buttons
    }

    /// "⋯" is drawn: some button is in it.
    pub(super) const fn more(&self) -> bool {
        self.hidden > 0
    }

    /// Every button is in "⋯": only "● 3 ⋯" is drawn (× goes with the last button).
    pub(super) const fn folded(&self) -> bool {
        self.hidden >= self.buttons
    }
}

impl SelectionParts {
    /// How wide `layout` is drawn.
    pub(super) fn width(&self, layout: &SelectionLayout) -> f32 {
        if layout.folded() {
            return self.pill + self.tight_gap + self.more;
        }
        let summary = match layout.summary {
            Summary::Full => self.full,
            Summary::Count => self.count,
            Summary::Pill => self.pill,
        };
        let shown = &self.buttons[layout.hidden.min(self.buttons.len())..];
        let labels = if layout.labels { self.labels } else { 0. };
        let more = if layout.more() { self.more } else { 0. };
        // The summary, the divider, the buttons, "⋯", the spacer and ×, a gap apart.
        let pieces = 4 + shown.len() + usize::from(layout.more());
        let buttons: f32 = shown.iter().sum();
        let gaps = self.gap * (pieces - 1) as f32;
        summary + self.divider + buttons + labels + more + self.clear + gaps
    }

    /// Whether `layout` leaves the path bar `path` wide (every level) out of `room`, as the row is
    /// laid out: in whole pixels, so the selection may take up to its width rounded up, and the
    /// path bar collapses levels unless its slot holds `path` exactly.
    pub(super) fn fits(&self, layout: &SelectionLayout, path: f32, room: f32) -> bool {
        self.width(layout).ceil() + path.ceil() <= room
    }

    /// The layouts from the roomiest to the narrowest, each one step shorter than the one
    /// before: the summary shortens, the labels go, then the buttons one by one.
    fn steps(&self) -> impl Iterator<Item = SelectionLayout> + use<> {
        let buttons = self.buttons.len();
        let labels = self.labels > 0.;
        let layout = move |summary, labels, hidden| SelectionLayout {
            summary,
            labels,
            hidden,
            buttons,
            collapse: false,
        };
        [
            layout(Summary::Full, labels, 0),
            layout(Summary::Count, labels, 0),
            layout(Summary::Pill, labels, 0),
            layout(Summary::Pill, false, 0),
        ]
        .into_iter()
        .chain((1..=buttons).map(move |hidden| layout(Summary::Pill, false, hidden)))
    }
}

/// How the selection fits the pane row: the roomiest layout that leaves the path bar `path` wide
/// (every level) out of the `room` the two share, or "● 3 ⋯" with the path bar collapsing levels
/// when not even that does.
pub(super) fn selection_layout(room: f32, path: f32, parts: &SelectionParts) -> SelectionLayout {
    let buttons = parts.buttons.len();
    parts
        .steps()
        .find(|layout| parts.fits(layout, path, room))
        .unwrap_or(SelectionLayout {
            summary: Summary::Pill,
            labels: false,
            hidden: buttons,
            buttons,
            collapse: true,
        })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// One selected file in a pane as measured on the machine at 100 %: the summary
    /// "1 selected · 3 B" and Open, Rename, Cut, Copy and Delete come to 320 px, the path
    /// `C: › top1 › projects › clients › acme › reports` to 492 px.
    fn measured() -> SelectionParts {
        SelectionParts {
            full: 111.,
            count: 81.,
            pill: 32.,
            buttons: vec![28.; 5],
            labels: 0.,
            more: 26.,
            clear: 28.,
            divider: 9.,
            gap: 4.,
            tight_gap: 2.,
        }
    }

    const PATH: f32 = 492.;

    const fn layout(summary: Summary, hidden: usize) -> SelectionLayout {
        SelectionLayout {
            summary,
            labels: false,
            hidden,
            buttons: 5,
            collapse: false,
        }
    }

    #[test]
    fn the_measured_selection_gives_way_in_steps() {
        let parts = measured();
        let at = |room: f32| selection_layout(room, PATH, &parts);
        assert!((parts.width(&layout(Summary::Full, 0)) - 320.).abs() < 0.01);
        // Everything, down to 320 + 492.
        assert_eq!(at(1200.), layout(Summary::Full, 0));
        assert_eq!(at(812.), layout(Summary::Full, 0));
        // Then the size goes, then the summary shrinks to the pill: all buttons stay.
        assert_eq!(at(811.), layout(Summary::Count, 0));
        assert_eq!(at(782.), layout(Summary::Count, 0));
        assert_eq!(at(781.), layout(Summary::Pill, 0));
        assert_eq!(at(733.), layout(Summary::Pill, 0));
        // Then the buttons, one by one: Open, Rename, Cut, Copy, Delete.
        assert_eq!(at(732.), layout(Summary::Pill, 1));
        assert_eq!(at(731.), layout(Summary::Pill, 1));
        assert_eq!(at(730.), layout(Summary::Pill, 2));
        assert_eq!(at(699.), layout(Summary::Pill, 2));
        assert_eq!(at(698.), layout(Summary::Pill, 3));
        assert_eq!(at(667.), layout(Summary::Pill, 3));
        assert_eq!(at(666.), layout(Summary::Pill, 4));
        assert_eq!(at(635.), layout(Summary::Pill, 4));
        // Only "● 1 ⋯": the path still has every level.
        assert_eq!(at(634.), layout(Summary::Pill, 5));
        assert_eq!(at(552.), layout(Summary::Pill, 5));
        // Not even that: the path bar collapses.
        let collapsed = SelectionLayout {
            collapse: true,
            ..layout(Summary::Pill, 5)
        };
        assert_eq!(at(551.), collapsed);
        assert_eq!(at(0.), collapsed);
    }

    /// #528: 0.10.1 folded every button anywhere between "● 1 ⋯" and everything fitting
    /// (552 to 812 px here). Now most of that range keeps buttons.
    #[test]
    fn a_moderately_narrow_pane_keeps_its_buttons() {
        let parts = measured();
        let at = |room: f32| selection_layout(room, PATH, &parts);
        assert_eq!(at(760.), layout(Summary::Pill, 0));
        // Delete, Copy and Cut still there.
        let mid = at(700.);
        assert_eq!(mid, layout(Summary::Pill, 2));
        assert!((2..5).all(|ix| mid.shows(ix)) && !mid.shows(0) && !mid.shows(1));
        assert!(mid.more());
    }

    /// Widths as the machine measured them are fractional, the row's slots whole pixels: at a
    /// room of 781, "1 selected" and every button (288.59) next to the path (492.11) would take
    /// 289 + 492, a hair short of the path, which collapsed a level (#528). The pill steps in
    /// instead.
    #[test]
    fn fractional_widths_leave_the_path_its_whole_pixels() {
        let parts = SelectionParts {
            full: 120.4,
            count: 79.592,
            pill: 29.66,
            ..measured()
        };
        let path = 492.107;
        let at = |room: f32| selection_layout(room, path, &parts);
        assert!((parts.width(&layout(Summary::Count, 0)) - 288.592).abs() < 0.001);
        assert!((parts.width(&layout(Summary::Pill, 0)) - 238.66).abs() < 0.001);
        assert_eq!(at(781.), layout(Summary::Pill, 0));
        assert_eq!(at(782.), layout(Summary::Count, 0));
        assert_eq!(at(731.), layout(Summary::Pill, 1));
        assert_eq!(at(732.), layout(Summary::Pill, 0));
    }

    /// Split view: Copy to/Move to the other side keep their labels until the summary is the
    /// pill, then lose them before any of them goes.
    #[test]
    fn the_peer_labels_go_after_the_summary_and_before_the_buttons() {
        let parts = SelectionParts {
            full: 26.,
            count: 26.,
            pill: 32.,
            // Move to, Copy to (with the divider after the pair), Delete.
            buttons: vec![34., 47., 28.],
            labels: 120.,
            more: 26.,
            clear: 28.,
            divider: 9.,
            gap: 4.,
            tight_gap: 2.,
        };
        let at = |room: f32| selection_layout(room, 200., &parts);
        // 26 + 9 + 109 + 120 + 28 + 6 gaps.
        let wide = at(200. + 316.);
        assert_eq!(
            (wide.summary, wide.labels, wide.hidden),
            (Summary::Full, true, 0)
        );
        // The pill with the labels (322) is wider than the short summary with them: skipped.
        let icons = at(200. + 315.);
        assert_eq!(
            (icons.summary, icons.labels, icons.hidden),
            (Summary::Pill, false, 0)
        );
        let one = at(200. + 200.);
        assert_eq!((one.hidden, one.more()), (1, true), "Move to goes first");
    }

    prop_compose! {
        /// Widths like the pane row's: buttons, the summary and the pill a few tokens wide. The
        /// pill is never much wider than the summary without the size, so "● 3 ⋯" is the
        /// narrowest layout.
        fn parts()(
            buttons in prop::collection::vec(16_f32..120., 1..=6),
            count in 40_f32..240.,
            size in 0_f32..120.,
            pill in 20_f32..40.,
            labels in prop_oneof![Just(0_f32), 20_f32..240.],
            more in 20_f32..32.,
            clear in 20_f32..32.,
            divider in 0_f32..16.,
            gap in 0_f32..8.,
            tight in 0_f32..=1.,
        ) -> SelectionParts {
            SelectionParts {
                full: count + size,
                count,
                pill,
                buttons,
                labels,
                more,
                clear,
                divider,
                gap,
                tight_gap: gap * tight,
            }
        }
    }

    const fn everything(parts: &SelectionParts) -> SelectionLayout {
        SelectionLayout {
            summary: Summary::Full,
            labels: parts.labels > 0.,
            hidden: 0,
            buttons: parts.buttons.len(),
            collapse: false,
        }
    }

    const fn folded(parts: &SelectionParts) -> SelectionLayout {
        SelectionLayout {
            summary: Summary::Pill,
            labels: false,
            hidden: parts.buttons.len(),
            buttons: parts.buttons.len(),
            collapse: false,
        }
    }

    /// The layout one step roomier than `layout`, if there is one.
    const fn roomier(parts: &SelectionParts, layout: SelectionLayout) -> Option<SelectionLayout> {
        let base = SelectionLayout {
            collapse: false,
            ..layout
        };
        if base.hidden > 0 {
            return Some(SelectionLayout {
                hidden: base.hidden - 1,
                ..base
            });
        }
        if parts.labels > 0. && !base.labels {
            return Some(SelectionLayout {
                labels: true,
                ..base
            });
        }
        let summary = match base.summary {
            Summary::Full => return None,
            Summary::Count => Summary::Full,
            Summary::Pill => Summary::Count,
        };
        Some(SelectionLayout { summary, ..base })
    }

    proptest! {
        /// Room for everything: nothing is shortened or hidden, and the path is whole.
        #[test]
        fn everything_fits_untouched(
            parts in parts(),
            path in 40_f32..900.,
            spare in 0_f32..400.,
        ) {
            let all = everything(&parts);
            let room = parts.width(&all).ceil() + path.ceil() + spare;
            prop_assert_eq!(selection_layout(room, path, &parts), all);
        }

        /// More room never hides more buttons, shortens the summary further, drops the labels
        /// or collapses the path.
        #[test]
        fn more_room_never_gives_way_more(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
            more_room in 0_f32..800.,
        ) {
            let narrow = selection_layout(room, path, &parts);
            let wide = selection_layout(room + more_room, path, &parts);
            prop_assert!(wide.hidden <= narrow.hidden, "{:?} vs {:?}", wide, narrow);
            prop_assert!(wide.summary <= narrow.summary, "{:?} vs {:?}", wide, narrow);
            prop_assert!(wide.labels >= narrow.labels, "{:?} vs {:?}", wide, narrow);
            prop_assert!(wide.collapse <= narrow.collapse, "{:?} vs {:?}", wide, narrow);
        }

        /// The path bar gives way last: collapsed, every button is in "⋯" and the summary is
        /// the pill.
        #[test]
        fn the_path_collapses_last(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = selection_layout(room, path, &parts);
            if layout.collapse {
                prop_assert!(layout.folded(), "{:?}", layout);
                prop_assert_eq!(layout.summary, Summary::Pill);
                prop_assert!(!layout.labels);
            }
        }

        /// "⋯" shows exactly when some button is in it, × while any button is drawn, and the
        /// summary is the pill before any button goes.
        #[test]
        fn more_shows_with_hidden_buttons(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = selection_layout(room, path, &parts);
            let hidden = (0..parts.buttons.len()).filter(|&ix| !layout.shows(ix)).count();
            prop_assert_eq!(layout.more(), hidden > 0);
            prop_assert_eq!(!layout.folded(), hidden < parts.buttons.len());
            if layout.more() {
                prop_assert_eq!(layout.summary, Summary::Pill);
                prop_assert!(!layout.labels);
            }
        }

        /// The layout fits whenever one does ("● 3 ⋯" is the narrowest); when none does it is
        /// "● 3 ⋯" with the path collapsing.
        #[test]
        fn the_layout_fits_the_room(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = selection_layout(room, path, &parts);
            if parts.fits(&folded(&parts), path, room) {
                prop_assert!(!layout.collapse, "{:?}", layout);
                prop_assert!(parts.fits(&layout, path, room), "{:?}", layout);
            } else {
                prop_assert_eq!(layout, SelectionLayout { collapse: true, ..folded(&parts) });
            }
        }

        /// Nothing gives way that doesn't have to: one step roomier wouldn't fit.
        #[test]
        fn nothing_gives_way_needlessly(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = selection_layout(room, path, &parts);
            if let Some(roomier) = roomier(&parts, layout) {
                prop_assert!(!parts.fits(&roomier, path, room), "{:?} fits", roomier);
            }
        }

        /// Laid out in whole pixels, the selection takes up to its width rounded up out of a
        /// whole-pixel room, and whatever is left must hold the path's (fractional) width
        /// whenever the layout keeps every level.
        #[test]
        fn the_path_keeps_its_levels_in_whole_pixels(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_u16..1600,
        ) {
            let room = f32::from(room);
            let layout = selection_layout(room, path, &parts);
            if !layout.collapse {
                let slot = room - parts.width(&layout).ceil();
                prop_assert!(slot >= path, "slot {} for {}: {:?}", slot, path, layout);
            }
        }

        /// The buttons go in their order: a hidden button never has a less used one still
        /// drawn.
        #[test]
        fn the_buttons_go_in_order(
            parts in parts(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = selection_layout(room, path, &parts);
            let n = parts.buttons.len();
            for hidden in (0..n).filter(|&ix| !layout.shows(ix)) {
                prop_assert!((0..hidden).all(|less| !layout.shows(less)), "{:?}", layout);
            }
        }
    }
}
