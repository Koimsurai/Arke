//! How the pane row gives way (#388, #474, #528), kept free of gpui state so it can be tested
//! over generated widths.
//!
//! The row holds Up, the path bar, the folder's item count or the selection, and the view tools.
//! As the room shrinks, everything but the path bar gives way first, one step at a time, the way
//! File Explorer's and Finder's toolbars do; the path bar keeps every level until nothing else is
//! left to give:
//!
//! 1. everything: the view switcher, Sort, the item count, synchronized browsing and compare
//!    (split view), the summary with the total size, every button;
//! 2. the view switcher (Details, Icons, Columns and its chevron) gives way to the view options
//!    button alone, which shows the current view and opens the same flyout, as in split panes;
//! 3. Sort moves into "⋯" (its menu has "Sort by", the current column and direction checked);
//! 4. with nothing selected, the item count goes: it only tells, so it isn't moved anywhere;
//! 5. synchronized browsing and compare move into "⋯", checked while on;
//! 6. the selection's summary drops the size, then shrinks to the pill "● 3" (its tooltip has the
//!    rest);
//! 7. Copy to/Move to the other side drop their labels, then the selection's buttons move into
//!    "⋯" one at a time, the least used first and Delete last; × goes with the last of them;
//! 8. with all of that folded, the path bar collapses levels into "…".
//!
//! "⋯" is one per row: the selection's, or with nothing selected one among the view tools. The
//! view options button, new folder, paste and the preview and split toggles never fold: the
//! flyout holds settings found nowhere else in the row's menus, and a toggle that changes the
//! pane's width would fold away under the pointer that pressed it (and is the way out of a narrow
//! pane).
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

/// How the selection is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SelectionLayout {
    pub(super) summary: Summary,
    /// Copy to/Move to show their labels.
    pub(super) labels: bool,
    /// How many of the buttons, from the least used, are in "⋯".
    pub(super) hidden: usize,
    /// How many buttons there are.
    pub(super) buttons: usize,
    /// Some of the view tools (Sort, the dual pane toggles) are in the selection's "⋯" too,
    /// which shows even with every button drawn.
    pub(super) tools: bool,
}

impl SelectionLayout {
    /// Whether button `ix` (in the order of [`SelectionParts::buttons`]) is drawn.
    pub(super) const fn shows(&self, ix: usize) -> bool {
        ix >= self.hidden && ix < self.buttons
    }

    /// "⋯" is drawn: some button, or some view tool, is in it.
    pub(super) const fn more(&self) -> bool {
        self.hidden > 0 || self.tools
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

    /// The selection's layouts from the roomiest to the narrowest, each one step shorter than the
    /// one before: the summary shortens, the labels go, then the buttons one by one. `tools`:
    /// some view tools are in "⋯".
    fn steps(&self, tools: bool) -> impl Iterator<Item = SelectionLayout> + use<> {
        let buttons = self.buttons.len();
        let labels = self.labels > 0.;
        let layout = move |summary, labels, hidden| SelectionLayout {
            summary,
            labels,
            hidden,
            buttons,
            tools,
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

    /// Everything drawn.
    const fn everything(&self, tools: bool) -> SelectionLayout {
        SelectionLayout {
            summary: Summary::Full,
            labels: self.labels > 0.,
            hidden: 0,
            buttons: self.buttons.len(),
            tools,
        }
    }

    /// Just "● 3 ⋯".
    const fn folded(&self, tools: bool) -> SelectionLayout {
        SelectionLayout {
            summary: Summary::Pill,
            labels: false,
            hidden: self.buttons.len(),
            buttons: self.buttons.len(),
            tools,
        }
    }
}

/// What the row shows between the path bar and the view tools.
#[derive(Clone, Debug)]
pub(super) enum Info {
    /// Nothing selected: the folder's item count, with the gap after it, which hides, and the
    /// rest (paste, new folder, the comparison's chip), as wide as it was drawn, which doesn't
    /// fold.
    Folder { count: f32, rest: f32 },
    /// The selection, which gives way in steps.
    Selection(SelectionParts),
}

/// The widths the pane row's layouts are made of, as drawn, the path bar's aside.
#[derive(Clone, Debug)]
pub(super) struct RowParts {
    /// The view switcher.
    pub(super) switcher: f32,
    /// The view options button that stands in for it (as wide as the switcher: there is none,
    /// in split panes).
    pub(super) view: f32,
    /// Sort, with the gap after it (zero: there is none, while searching).
    pub(super) sort: f32,
    /// Synchronized browsing and compare, with the gap before them (zero: there are none, but in
    /// a split tab's last pane).
    pub(super) toggles: f32,
    /// "⋯" among the view tools, with the gap after it, where nothing is selected to hold it.
    pub(super) more: f32,
    pub(super) info: Info,
}

/// How far the view tools have given way, in the order they do: each step keeps the ones
/// before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Tools {
    /// Everything drawn.
    All,
    /// The view switcher gave way to the view options button.
    ViewButton,
    /// Sort is in "⋯" too.
    SortFolded,
    /// The item count is left out too.
    CountHidden,
    /// Synchronized browsing and compare are in "⋯" too.
    TogglesFolded,
}

impl Tools {
    /// Every step, roomiest first.
    const ALL: [Self; 5] = [
        Self::All,
        Self::ViewButton,
        Self::SortFolded,
        Self::CountHidden,
        Self::TogglesFolded,
    ];

    /// The view switcher is drawn (where there is one).
    pub(super) const fn switcher(self) -> bool {
        matches!(self, Self::All)
    }

    /// Sort is in "⋯" (where there is a Sort).
    pub(super) const fn sort_folded(self) -> bool {
        matches!(
            self,
            Self::SortFolded | Self::CountHidden | Self::TogglesFolded
        )
    }

    /// The item count is left out (where there is one).
    pub(super) const fn count_hidden(self) -> bool {
        matches!(self, Self::CountHidden | Self::TogglesFolded)
    }

    /// Synchronized browsing and compare are in "⋯" (where there are any).
    pub(super) const fn toggles_folded(self) -> bool {
        matches!(self, Self::TogglesFolded)
    }
}

/// How the pane row is drawn, and whether the path bar has to collapse levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowLayout {
    /// How far the view tools have given way.
    pub(super) tools: Tools,
    /// Some of the view tools the row has (Sort, the dual pane toggles) are in "⋯".
    pub(super) in_more: bool,
    /// The selection's layout; `None` with nothing selected.
    pub(super) selection: Option<SelectionLayout>,
    /// Even everything folded leaves the path bar short of its whole width: it collapses levels.
    /// The path bar takes this, the one decision, rather than making its own from its slot.
    pub(super) collapse: bool,
}

impl RowLayout {
    /// The view switcher is drawn (where there is one).
    pub(super) const fn switcher(&self) -> bool {
        self.tools.switcher()
    }

    /// Sort is in "⋯" (where there is a Sort).
    pub(super) const fn sort_folded(&self) -> bool {
        self.tools.sort_folded()
    }

    /// The item count is left out (where there is one).
    pub(super) const fn count_hidden(&self) -> bool {
        self.tools.count_hidden()
    }

    /// Synchronized browsing and compare are in "⋯" (where there are any).
    pub(super) const fn toggles_folded(&self) -> bool {
        self.tools.toggles_folded()
    }

    /// Some of the view tools are in "⋯".
    #[cfg(test)]
    pub(super) const fn tools_folded(&self) -> bool {
        self.in_more
    }

    /// "⋯" among the view tools: some of them are in it, and no selection's "⋯" holds them.
    pub(super) const fn tools_more(&self) -> bool {
        self.in_more && self.selection.is_none()
    }
}

impl RowParts {
    /// How wide the row's info (item count or selection) and its foldable view tools ("⋯", Sort,
    /// the view switcher or button, the dual pane toggles) are drawn in `layout`.
    pub(super) fn widths(&self, layout: &RowLayout) -> (f32, f32) {
        let info = match (&self.info, &layout.selection) {
            (Info::Folder { count, rest }, _) => {
                if layout.count_hidden() {
                    *rest
                } else {
                    count + rest
                }
            }
            (Info::Selection(parts), Some(selection)) => parts.width(selection),
            (Info::Selection(_), None) => 0.,
        };
        let view = if layout.switcher() {
            self.switcher
        } else {
            self.view
        };
        let sort = if layout.sort_folded() { 0. } else { self.sort };
        let toggles = if layout.toggles_folded() {
            0.
        } else {
            self.toggles
        };
        let more = if layout.tools_more() { self.more } else { 0. };
        (info, view + sort + toggles + more)
    }

    /// Whether `layout` leaves the path bar `path` wide (every level) out of `room`, as the row is
    /// laid out: in whole pixels, so the info and the view tools may each take up to their width
    /// rounded up, and the path bar keeps its levels when its slot holds `path` rounded up.
    pub(super) fn fits(&self, layout: &RowLayout, path: f32, room: f32) -> bool {
        let (info, tools) = self.widths(layout);
        info.ceil() + tools.ceil() + path.ceil() <= room
    }

    /// The row with its view tools given way as far as `tools`, and the selection, if any, all
    /// drawn.
    pub(super) const fn at(&self, tools: Tools) -> RowLayout {
        let in_more = (tools.sort_folded() && self.sort > 0.)
            || (tools.toggles_folded() && self.toggles > 0.);
        RowLayout {
            tools,
            in_more,
            selection: match &self.info {
                Info::Folder { .. } => None,
                Info::Selection(parts) => Some(parts.everything(in_more)),
            },
            collapse: false,
        }
    }

    /// Everything drawn.
    #[cfg(test)]
    pub(super) const fn everything(&self) -> RowLayout {
        self.at(Tools::All)
    }

    /// Everything that folds, folded.
    pub(super) const fn folded(&self) -> RowLayout {
        let row = self.at(Tools::TogglesFolded);
        RowLayout {
            selection: match &self.info {
                Info::Folder { .. } => None,
                Info::Selection(parts) => Some(parts.folded(row.in_more)),
            },
            ..row
        }
    }
}

/// How the pane row fits: the roomiest layout that leaves the path bar `path` wide (every level)
/// out of the `room` the path bar, the info and the foldable view tools share, or everything
/// folded with the path bar collapsing levels when not even that does. A step that folds
/// something the row doesn't have is as wide as the one before it, so it is never the one taken.
pub(super) fn row_layout(room: f32, path: f32, parts: &RowParts) -> RowLayout {
    let last = parts.at(Tools::TogglesFolded);
    let selection = match &parts.info {
        Info::Folder { .. } => None,
        Info::Selection(selection) => Some(selection.steps(last.in_more).skip(1)),
    };
    let selection = selection.into_iter().flatten().map(|selection| RowLayout {
        selection: Some(selection),
        ..last
    });
    Tools::ALL
        .into_iter()
        .map(|tools| parts.at(tools))
        .chain(selection)
        .find(|layout| parts.fits(layout, path, room))
        .unwrap_or_else(|| RowLayout {
            collapse: true,
            ..parts.folded()
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

    /// A row with nothing to fold but the selection (no Sort, no view switcher).
    fn selection_only(parts: &SelectionParts) -> RowParts {
        RowParts {
            switcher: 30.,
            view: 30.,
            sort: 0.,
            toggles: 0.,
            more: 0.,
            info: Info::Selection(parts.clone()),
        }
    }

    /// The selection's layout in [`selection_only`]'s row, and whether the path collapses.
    fn selection_at(room: f32, path: f32, parts: &SelectionParts) -> (SelectionLayout, bool) {
        // The view button's 30 px come off the room first.
        let row = row_layout(room + 30., path, &selection_only(parts));
        (row.selection.expect("a selection"), row.collapse)
    }

    const fn layout(summary: Summary, hidden: usize) -> (SelectionLayout, bool) {
        let selection = SelectionLayout {
            summary,
            labels: false,
            hidden,
            buttons: 5,
            tools: false,
        };
        (selection, false)
    }

    #[test]
    fn the_measured_selection_gives_way_in_steps() {
        let parts = measured();
        let at = |room: f32| selection_at(room, PATH, &parts);
        assert!((parts.width(&layout(Summary::Full, 0).0) - 320.).abs() < 0.01);
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
        let collapsed = (layout(Summary::Pill, 5).0, true);
        assert_eq!(at(551.), collapsed);
        assert_eq!(at(0.), collapsed);
    }

    /// #528: 0.10.1 folded every button anywhere between "● 1 ⋯" and everything fitting
    /// (552 to 812 px here). Now most of that range keeps buttons.
    #[test]
    fn a_moderately_narrow_pane_keeps_its_buttons() {
        let parts = measured();
        let at = |room: f32| selection_at(room, PATH, &parts).0;
        assert_eq!(at(760.), layout(Summary::Pill, 0).0);
        // Delete, Copy and Cut still there.
        let mid = at(700.);
        assert_eq!(mid, layout(Summary::Pill, 2).0);
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
        let at = |room: f32| selection_at(room, path, &parts);
        assert!((parts.width(&layout(Summary::Count, 0).0) - 288.592).abs() < 0.001);
        assert!((parts.width(&layout(Summary::Pill, 0).0) - 238.66).abs() < 0.001);
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
        let at = |room: f32| selection_at(room, 200., &parts).0;
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

    /// #474: the measured selection in a row with the view switcher (114 px, the view options
    /// button alone 42) and Sort (42 and its 4 px gap). The switcher gives way first, then Sort
    /// moves into the selection's "⋯", then the selection's own steps.
    #[test]
    fn the_view_tools_give_way_before_the_selection() {
        let parts = RowParts {
            switcher: 114.,
            view: 42.,
            sort: 46.,
            toggles: 0.,
            more: 30.,
            info: Info::Selection(measured()),
        };
        let at = |room: f32| row_layout(room, PATH, &parts);
        // 320 + 114 + 46 + 492.
        let all = at(972.);
        assert_eq!(all, parts.everything());
        assert!(all.switcher() && !all.sort_folded());
        // The switcher goes: 320 + 42 + 46 + 492.
        let compact = at(971.);
        assert!(!compact.switcher() && !compact.sort_folded());
        assert_eq!(compact.selection, Some(layout(Summary::Full, 0).0));
        assert_eq!(at(900.), compact);
        // Sort goes into the selection's "⋯" (26 and a gap): 350 + 42 + 492.
        let sort = at(899.);
        assert!(!sort.switcher() && sort.sort_folded() && !sort.tools_more());
        let selection = sort.selection.expect("a selection");
        assert_eq!((selection.summary, selection.hidden), (Summary::Full, 0));
        assert!(selection.more());
        assert_eq!(at(884.), sort);
        // Then the selection's summary.
        let count = at(883.).selection.expect("a selection");
        assert_eq!((count.summary, count.hidden), (Summary::Count, 0));
        // The path collapses last, with everything folded.
        let last = at(100.);
        assert_eq!(
            last,
            RowLayout {
                collapse: true,
                ..parts.folded()
            }
        );
    }

    /// #474: with nothing selected, the folder's info stays as it is; the switcher, then Sort
    /// give way, Sort into a "⋯" of the view tools' own.
    #[test]
    fn nothing_selected_folds_the_view_tools() {
        let parts = RowParts {
            switcher: 114.,
            view: 42.,
            sort: 46.,
            toggles: 0.,
            more: 30.,
            info: Info::Folder {
                count: 0.,
                rest: 150.,
            },
        };
        let at = |room: f32| row_layout(room, PATH, &parts);
        // 150 + 114 + 46 + 492.
        assert_eq!(at(802.), parts.everything());
        let compact = at(801.);
        assert!(!compact.switcher() && !compact.sort_folded() && compact.selection.is_none());
        // 150 + 42 + 46 + 492 = 730; with "⋯" for Sort: 150 + 42 + 30 + 492 = 714.
        assert_eq!(at(730.), compact);
        let sort = at(729.);
        assert!(sort.sort_folded() && sort.tools_more() && !sort.collapse);
        assert_eq!(at(714.), sort);
        assert!(at(713.).collapse);
        // While searching there is no Sort, and nothing to fold after the switcher.
        let searching = RowParts { sort: 0., ..parts };
        let at = |room: f32| row_layout(room, PATH, &searching);
        assert!(!at(684.).collapse && !at(684.).tools_more());
        assert!(at(683.).collapse && !at(683.).tools_more());
    }

    /// #474: the two rows from the machine's log that left the path bar a level short. The room
    /// was exactly the path (532.6, 533 in whole pixels), the info and the view tools: that fits,
    /// and the path bar takes this decision instead of making its own.
    #[test]
    fn a_room_that_fits_exactly_keeps_the_whole_path() {
        let tools = |info| RowParts {
            switcher: 114.,
            view: 42.,
            sort: 46.,
            toggles: 0.,
            more: 30.,
            info,
        };
        // 1251 px, nothing selected: 533 + 165 + 114 + 46.
        let parts = tools(Info::Folder {
            count: 0.,
            rest: 165.,
        });
        let row = row_layout(858., 532.6, &parts);
        assert_eq!(row, parts.everything());
        assert!(!row.collapse);
        assert!(!row_layout(857., 532.6, &parts).switcher());
        // 1360 px, one item selected: Sort in "⋯", the selection 391.4 wide, the view button 42.
        let parts = tools(Info::Selection(SelectionParts {
            full: 152.4,
            ..measured()
        }));
        let row = row_layout(967., 532.6, &parts);
        assert!(
            !row.collapse && !row.switcher() && row.sort_folded(),
            "{row:?}"
        );
        let selection = row.selection.expect("a selection");
        assert_eq!((selection.summary, selection.hidden), (Summary::Full, 0));
        assert!((parts.widths(&row).0.ceil() + 42. + 533. - 967.).abs() < 0.01);
    }

    /// #474: a split tab's last pane at about 1500 px, nothing selected, the path as logged on the
    /// machine (532.6). Token widths: the view options button 42, Sort 42 and a gap, "⋯" 26 and
    /// a gap, synchronized browsing and compare 28 each, 4 apart, and a gap; new folder 28. "2
    /// items" taken as 46 px and its gap. Sort goes first, then the count, then the toggles,
    /// and only then the path.
    #[test]
    fn a_split_pane_folds_its_count_and_toggles_before_the_path() {
        let parts = RowParts {
            switcher: 42.,
            view: 42.,
            sort: 46.,
            toggles: 64.,
            more: 30.,
            info: Info::Folder {
                count: 50.,
                rest: 28.,
            },
        };
        let at = |room: f32| row_layout(room, 532.6, &parts);
        // 78 + 42 + 46 + 64 + 533 (no switcher in a split pane: the view button all along).
        assert_eq!(at(763.), parts.at(Tools::All));
        // Sort into "⋯": 78 + 42 + 64 + 30 + 533.
        assert_eq!(at(762.), parts.at(Tools::SortFolded));
        assert_eq!(at(747.), parts.at(Tools::SortFolded));
        // The count goes: 28 + 136 + 533.
        assert_eq!(at(746.), parts.at(Tools::CountHidden));
        assert_eq!(at(697.), parts.at(Tools::CountHidden));
        // The toggles into "⋯": 28 + 42 + 30 + 533.
        assert_eq!(at(696.), parts.at(Tools::TogglesFolded));
        assert_eq!(at(633.), parts.at(Tools::TogglesFolded));
        assert!(at(633.).tools_more() && at(633.).selection.is_none());
        // Only now the path.
        let last = at(632.);
        assert!(last.collapse && last.toggles_folded() && last.count_hidden());
    }

    prop_compose! {
        /// Widths like the pane row's selection: buttons, the summary and the pill a few tokens
        /// wide. The pill is never much wider than the summary without the size, so "● 3 ⋯" is
        /// the narrowest layout.
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

    prop_compose! {
        /// Widths like the pane row's: a view switcher or none (split view), Sort or none
        /// (searching), the dual pane toggles or none, and the folder's info (an item count or
        /// none) or a selection. Folding into "⋯" always saves room, as on screen (Sort is 46 px
        /// with its gap, the toggles 64, "⋯" 30).
        fn row()(
            view in 30_f32..50.,
            switcher in prop_oneof![Just(0_f32), 40_f32..120.],
            sort in prop_oneof![Just(0_f32), 40_f32..60.],
            toggles in prop_oneof![Just(0_f32), 40_f32..70.],
            more in 20_f32..34.,
            info in prop_oneof![
                (prop_oneof![Just(0_f32), 10_f32..120.], 0_f32..200.)
                    .prop_map(|(count, rest)| Info::Folder { count, rest }),
                parts().prop_map(Info::Selection),
            ],
        ) -> RowParts {
            RowParts { switcher: view + switcher, view, sort, toggles, more, info }
        }
    }

    /// One step roomier than a selection's `layout`, if there is one.
    const fn roomier_selection(
        parts: &SelectionParts,
        layout: SelectionLayout,
    ) -> Option<SelectionLayout> {
        if layout.hidden > 0 {
            return Some(SelectionLayout {
                hidden: layout.hidden - 1,
                ..layout
            });
        }
        if parts.labels > 0. && !layout.labels {
            return Some(SelectionLayout {
                labels: true,
                ..layout
            });
        }
        let summary = match layout.summary {
            Summary::Full => return None,
            Summary::Count => Summary::Full,
            Summary::Pill => Summary::Count,
        };
        Some(SelectionLayout { summary, ..layout })
    }

    /// The row's layout one step roomier than `layout`, if there is one.
    fn roomier(parts: &RowParts, layout: RowLayout) -> Option<RowLayout> {
        let base = RowLayout {
            collapse: false,
            ..layout
        };
        if let (Info::Selection(selection), Some(at)) = (&parts.info, base.selection)
            && let Some(roomier) = roomier_selection(selection, at)
        {
            return Some(RowLayout {
                selection: Some(roomier),
                ..base
            });
        }
        // The view tools a step back, the selection all drawn.
        let at = Tools::ALL.iter().position(|t| *t == base.tools)?;
        at.checked_sub(1).map(|before| parts.at(Tools::ALL[before]))
    }

    /// Whether the selection in `layout` has given anything up.
    const fn selection_gave_way(parts: &RowParts, layout: &RowLayout) -> bool {
        match (&parts.info, layout.selection) {
            (Info::Selection(selection), Some(at)) => {
                !matches!(at.summary, Summary::Full)
                    || at.hidden > 0
                    || (selection.labels > 0. && !at.labels)
            }
            _ => false,
        }
    }

    proptest! {
        /// Room for the full path and every button: nothing folds.
        #[test]
        fn everything_fits_untouched(
            parts in row(),
            path in 40_f32..900.,
            spare in 0_f32..400.,
        ) {
            let all = parts.everything();
            let (info, tools) = parts.widths(&all);
            let room = info.ceil() + tools.ceil() + path.ceil() + spare;
            prop_assert_eq!(row_layout(room, path, &parts), all);
        }

        /// More room never folds anything more, shortens the summary further, drops the labels
        /// or collapses the path.
        #[test]
        fn more_room_never_gives_way_more(
            parts in row(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
            more_room in 0_f32..800.,
        ) {
            let narrow = row_layout(room, path, &parts);
            let wide = row_layout(room + more_room, path, &parts);
            prop_assert!(wide.tools <= narrow.tools, "{:?} vs {:?}", wide, narrow);
            prop_assert!(wide.collapse <= narrow.collapse, "{:?} vs {:?}", wide, narrow);
            if let (Some(w), Some(n)) = (wide.selection, narrow.selection) {
                prop_assert!(w.hidden <= n.hidden, "{:?} vs {:?}", wide, narrow);
                prop_assert!(w.summary <= n.summary, "{:?} vs {:?}", wide, narrow);
                prop_assert!(w.labels >= n.labels, "{:?} vs {:?}", wide, narrow);
            }
        }

        /// While anything that folds is still drawn, the path keeps every level: collapsed, the
        /// switcher is gone, Sort and the toggles are in "⋯", the item count is left out, and
        /// the selection is "● 3 ⋯".
        #[test]
        fn the_path_collapses_last(
            parts in row(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = row_layout(room, path, &parts);
            if layout.collapse {
                prop_assert_eq!(layout.tools, Tools::TogglesFolded);
                prop_assert_eq!(layout.tools_folded(), parts.sort > 0. || parts.toggles > 0.);
                if let Some(selection) = layout.selection {
                    prop_assert!(selection.folded(), "{:?}", layout);
                    prop_assert_eq!(selection.summary, Summary::Pill);
                    prop_assert!(!selection.labels);
                }
            }
        }

        /// Folding follows the order strictly: the switcher, Sort, the item count, the toggles,
        /// then the selection's summary, labels and buttons.
        #[test]
        fn folding_follows_the_order(
            parts in row(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = row_layout(room, path, &parts);
            // The view tools' own order is the order of [`Tools`]: a step keeps the ones before
            // it. What "⋯" holds of them is what the row has of the steps taken.
            let held = (layout.sort_folded() && parts.sort > 0.)
                || (layout.toggles_folded() && parts.toggles > 0.);
            prop_assert_eq!(layout.tools_folded(), held);
            if selection_gave_way(&parts, &layout) {
                prop_assert_eq!(layout.tools, Tools::TogglesFolded);
            }
            if let Some(selection) = layout.selection {
                prop_assert_eq!(selection.tools, layout.tools_folded());
                let n = selection.buttons;
                for hidden in (0..n).filter(|&ix| !selection.shows(ix)) {
                    prop_assert!((0..hidden).all(|less| !selection.shows(less)), "{:?}", layout);
                }
            }
        }

        /// "⋯" shows exactly when something is in it: Sort, the toggles, or some of the
        /// selection's buttons; once, with the selection's if there is one. × shows while any
        /// button is drawn, and the summary is the pill before any button goes.
        #[test]
        fn more_shows_with_what_is_in_it(
            parts in row(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = row_layout(room, path, &parts);
            if let Some(selection) = layout.selection {
                let hidden = (0..selection.buttons).filter(|&ix| !selection.shows(ix)).count();
                prop_assert_eq!(selection.more(), hidden > 0 || layout.tools_folded());
                prop_assert!(!layout.tools_more());
                prop_assert_eq!(!selection.folded(), hidden < selection.buttons);
                if hidden > 0 {
                    prop_assert_eq!(selection.summary, Summary::Pill);
                    prop_assert!(!selection.labels);
                }
            } else {
                prop_assert_eq!(layout.tools_more(), layout.tools_folded());
            }
        }

        /// The layout fits whenever one does (everything folded is the narrowest); when none
        /// does it is everything folded with the path collapsing.
        #[test]
        fn the_layout_fits_the_room(
            parts in row(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = row_layout(room, path, &parts);
            if parts.fits(&parts.folded(), path, room) {
                prop_assert!(!layout.collapse, "{:?}", layout);
                prop_assert!(parts.fits(&layout, path, room), "{:?}", layout);
            } else {
                prop_assert_eq!(layout, RowLayout { collapse: true, ..parts.folded() });
            }
        }

        /// The path bar collapses levels exactly when the row can't fit it whole even with
        /// everything else folded: [`RowLayout::collapse`] is the one decision, and the path bar
        /// takes it rather than deciding from its slot on its own.
        #[test]
        fn the_path_bar_collapses_exactly_when_nothing_fits(
            parts in row(),
            path in 40_f32..900.,
            room in 0_u16..1600,
        ) {
            let room = f32::from(room);
            let layout = row_layout(room, path, &parts);
            prop_assert_eq!(layout.collapse, !parts.fits(&parts.folded(), path, room));
            // At a room that holds the layout exactly, the path is still whole.
            if !layout.collapse {
                let (info, tools) = parts.widths(&layout);
                let exact = info.ceil() + tools.ceil() + path.ceil();
                prop_assert!(!row_layout(exact, path, &parts).collapse);
            }
        }

        /// Laid out in whole pixels, the info and the view tools take up to their widths rounded
        /// up out of a whole-pixel room, and whatever is left must hold the path's (fractional)
        /// width whenever the layout keeps every level.
        #[test]
        fn the_path_keeps_its_levels_in_whole_pixels(
            parts in row(),
            path in 40_f32..900.,
            room in 0_u16..1600,
        ) {
            let room = f32::from(room);
            let layout = row_layout(room, path, &parts);
            if !layout.collapse {
                let (info, tools) = parts.widths(&layout);
                let slot = room - info.ceil() - tools.ceil();
                prop_assert!(slot >= path, "slot {} for {}: {:?}", slot, path, layout);
            }
        }

        /// Nothing gives way that doesn't have to: one step roomier wouldn't fit.
        #[test]
        fn nothing_gives_way_needlessly(
            parts in row(),
            path in 40_f32..900.,
            room in 0_f32..1600.,
        ) {
            let layout = row_layout(room, path, &parts);
            if let Some(roomier) = roomier(&parts, layout) {
                prop_assert!(!parts.fits(&roomier, path, room), "{:?} fits", roomier);
            }
        }
    }
}
