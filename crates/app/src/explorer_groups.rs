//! Group-by view (off by default). The details view splits into sections by the sort column,
//! each with a header row.
//!
//! - Date modified uses relative buckets (today, yesterday, earlier this week, ...), then years.
//!   Size uses the same buckets as Windows. Name groups by first letter. Folders kept apart (first
//!   when ascending, last when descending) get their own section; sorted by a date, or mixed with
//!   files, folders go into the section of their date like any file.
//! - Grouping by the sort column means sections are already contiguous, so one linear pass is
//!   enough (a few ms for tens of thousands of items). Cached by (list, sort, today).
//! - Rows mix headers and items. Selection, cursor and keyboard only see items; rows are
//!   converted only for scrolling and marquee selection. Clicking a header selects its section.
//! - A section can be collapsed (#287, chevron or double-click on the header): it keeps its header
//!   and count but contributes no item rows, so marquee and scrolling need no special case. The
//!   keyboard skips its items, and selecting everything or a range leaves them out. The set of
//!   collapsed labels lives on the tab and is cleared when it opens another folder.
//! - Grid view groups too. List rows must have uniform height, so the unit is a smaller slot:
//!   a header takes one slot, a row of tiles takes [`TILE_SLOTS`] (drawn in its first slot and
//!   overflowing down; when scrolled mid-row, the top slot draws the row upward).

use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use arke_core::sort::days_from_civil;
use arke_core::{Entry, SortColumn};
use gpui::{AnyElement, ClickEvent, Context, SharedString, div, prelude::*, px, svg};
use rust_i18n::t;

use super::{Explorer, ViewMode};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;

/// One row in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// Section header: label, first item, item count, and the items it covers (more than the
    /// count when expanded folders list their items under themselves). `collapsed`: the items
    /// get no rows.
    Header {
        label: SharedString,
        start: usize,
        count: usize,
        span: usize,
        collapsed: bool,
    },
    /// An item, by index in the list.
    Item(usize),
}

/// The list a layout was built for. Held weakly: while it is, the list can't be changed in place
/// (`Arc::make_mut` moves it out, as a rename does in `DirModel::apply_delta`) nor its memory
/// handed to a new list, so the same address always means the same items (#335: the sections of
/// the list before a rename were drawn over the list after it).
#[derive(Clone)]
pub(crate) struct ListOf(std::sync::Weak<Vec<Entry>>);

impl ListOf {
    fn new(list: &Arc<Vec<Entry>>) -> Self {
        Self(Arc::downgrade(list))
    }

    fn is(&self, list: &Arc<Vec<Entry>>) -> bool {
        std::ptr::eq(self.0.as_ptr(), Arc::as_ptr(list))
    }
}

pub(crate) struct Groups {
    list: ListOf,
    key: (SortColumn, bool, i64),
    pub(crate) rows: Vec<Row>,
    /// Row index of each item; an item of a collapsed section maps to the section's header.
    row_of: Vec<u32>,
    /// Items of the collapsed sections, in order.
    pub(crate) hidden: Vec<Range<usize>>,
}

/// Cached layouts, queried on every render.
#[derive(Default)]
pub(crate) struct GroupCache(RefCell<Option<Rc<Groups>>>, RefCell<Option<Rc<GridGroups>>>);

impl GroupCache {
    /// Forgets both layouts (the collapsed sections changed).
    fn clear(&self) {
        self.0.borrow_mut().take();
        self.1.borrow_mut().take();
    }
}

/// Slots taken by one row of tiles when grouping in grid view (a header takes one).
pub(crate) const TILE_SLOTS: usize = 4;

/// One slot of the grouped grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Header {
        label: SharedString,
        start: usize,
        count: usize,
        collapsed: bool,
    },
    /// Tile row and slot within it (0 is where the row is drawn).
    Row(usize, usize),
}

/// Layout of the grouped grid.
pub(crate) struct GridGroups {
    list: ListOf,
    key: (SortColumn, bool, i64, usize),
    pub(crate) slots: Vec<Slot>,
    /// Each tile row: item range and first slot.
    pub(crate) rows: Vec<(std::ops::Range<usize>, usize)>,
    /// Tile row of each item (`u32::MAX` in a collapsed section).
    row_of_item: Vec<u32>,
    /// Items of each collapsed section, with the slot of its header.
    pub(crate) hidden: Vec<(Range<usize>, usize)>,
}

impl GridGroups {
    fn new(list: ListOf, key: (SortColumn, bool, i64, usize), detail: &[Row], cols: usize) -> Self {
        let cols = cols.max(1);
        let mut slots = Vec::new();
        let mut rows = Vec::new();
        let mut row_of_item = Vec::new();
        let mut hidden = Vec::new();
        for row in detail {
            let Row::Header {
                label,
                start,
                count,
                collapsed,
                ..
            } = row
            else {
                continue;
            };
            let end = start + count;
            if *collapsed {
                hidden.push((*start..end, slots.len()));
                row_of_item.resize(row_of_item.len() + count, u32::MAX);
            }
            slots.push(Slot::Header {
                label: label.clone(),
                start: *start,
                count: *count,
                collapsed: *collapsed,
            });
            if *collapsed {
                continue;
            }
            let mut at = *start;
            while at < end {
                let stop = (at + cols).min(end);
                let r = rows.len();
                rows.push((at..stop, slots.len()));
                for _ in at..stop {
                    row_of_item.push(u32::try_from(r).unwrap_or(u32::MAX));
                }
                slots.extend((0..TILE_SLOTS).map(|k| Slot::Row(r, k)));
                at = stop;
            }
        }
        Self {
            list,
            key,
            slots,
            rows,
            row_of_item,
            hidden,
        }
    }

    /// Tile row containing the item; `None` for an item of a collapsed section.
    pub(crate) fn row_of(&self, ix: usize) -> Option<usize> {
        self.row_of_item
            .get(ix)
            .filter(|r| **r != u32::MAX)
            .map(|r| *r as usize)
    }

    /// The collapsed section holding the item: its items and header slot.
    pub(crate) fn hidden_at(&self, ix: usize) -> Option<&(Range<usize>, usize)> {
        let p = self.hidden.partition_point(|(r, _)| r.end <= ix);
        self.hidden.get(p).filter(|(r, _)| r.start <= ix)
    }

    /// First slot of the item's row, for scrolling (the header's, for a hidden item).
    pub(crate) fn slot_of(&self, ix: usize) -> usize {
        self.row_of(ix)
            .and_then(|r| self.rows.get(r))
            .map_or_else(|| self.hidden_at(ix).map_or(0, |h| h.1), |r| r.1)
    }

    /// Moves `delta` rows, keeping the column (clamped to the last item of a shorter row).
    pub(crate) fn step(&self, ix: usize, delta: isize) -> usize {
        let Some(row) = self.row_of(ix) else {
            // In a collapsed section: the nearest row before or after its header.
            let Some((_, slot)) = self.hidden_at(ix) else {
                return ix;
            };
            let next = self.rows.partition_point(|r| r.1 < *slot);
            let target = if delta > 0 {
                next
            } else {
                match next.checked_sub(1) {
                    Some(t) => t,
                    None => return ix,
                }
            };
            return self.rows.get(target).map_or(ix, |r| r.0.start);
        };
        let col = ix - self.rows[row].0.start;
        let target = row
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        let range = &self.rows[target].0;
        (range.start + col).min(range.end.saturating_sub(1))
    }

    /// Rows overlapping slots `s0..=s1`.
    pub(crate) fn rows_in_slots(
        &self,
        s0: usize,
        s1: usize,
    ) -> impl Iterator<Item = &std::ops::Range<usize>> {
        self.rows
            .iter()
            .filter(move |(_, first)| *first <= s1 && first + TILE_SLOTS > s0)
            .map(|(range, _)| range)
    }
}

impl Groups {
    pub(crate) fn row_of(&self, ix: usize) -> usize {
        self.row_of.get(ix).map_or(ix, |r| *r as usize)
    }
}

/// "Today" for date sections: local day number and the local offset from UTC in seconds.
pub(crate) type Today = arke_core::DayBuckets;

/// Today's day and the local offset, for date sections.
pub(crate) fn day_buckets(shell: &dyn crate::shell::Shell) -> Today {
    let now = SystemTime::now();
    let utc = match now.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    };
    let offset = shell.local_parts(now).map_or(0, |[y, m, d, h, mi, s]| {
        let local = days_from_civil(i64::from(y), i64::from(m), i64::from(d)) * 86_400
            + i64::from(h) * 3600
            + i64::from(mi) * 60
            + i64::from(s);
        // Round to whole minutes.
        ((local - utc) as f64 / 60.).round() as i64 * 60
    });
    Today {
        day: (utc + offset).div_euclid(86_400),
        offset,
    }
}

/// Date section label.
fn date_group(t: Option<SystemTime>, today: Today) -> String {
    use arke_core::DateSection as D;
    let key = match today.section(t) {
        D::Unknown => "group.unknown",
        D::Future => "group.future",
        D::Today => "group.today",
        D::Yesterday => "group.yesterday",
        D::ThisWeek => "group.this_week",
        D::LastWeek => "group.last_week",
        D::ThisMonth => "group.this_month",
        D::LastMonth => "group.last_month",
        D::ThisYear => "group.this_year",
        D::Year(year) => return t!("group.year", year = year).into_owned(),
    };
    t!(key).into_owned()
}

/// Size section, using the same buckets as Windows.
fn size_group(entry: &Entry) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    let key = match entry.size {
        0 => "group.size_empty",
        s if s < 16 * KB => "group.size_tiny",
        s if s < MB => "group.size_small",
        s if s < 128 * MB => "group.size_medium",
        s if s < GB => "group.size_large",
        s if s < 4 * GB => "group.size_huge",
        _ => "group.size_gigantic",
    };
    t!(key).into_owned()
}

/// Name section: first ASCII letter, with one section for digits and one for everything else.
fn name_group(entry: &Entry) -> String {
    match entry.name.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
        Some(c) if c.is_ascii_digit() => "0–9".into(),
        _ => t!("group.other").into_owned(),
    }
}

/// Section label for an entry. `folders_apart`: folders are kept apart from files and form their
/// own section.
fn group_of(entry: &Entry, column: SortColumn, folders_apart: bool, today: Today) -> String {
    let dir = entry.is_dir_like();
    if dir && (folders_apart || !matches!(column, SortColumn::Modified | SortColumn::Created)) {
        // Same localized "folder" label as the Type column.
        return arke_core::format::type_label(entry);
    }
    match column {
        SortColumn::Name => name_group(entry),
        SortColumn::Modified => date_group(entry.modified, today),
        SortColumn::Type => arke_core::format::type_label(entry),
        SortColumn::Size => size_group(entry),
        SortColumn::Created => date_group(entry.created, today),
        SortColumn::Extension => match entry.extension() {
            "" => t!("group.no_extension").into_owned(),
            ext => ext.to_uppercase(),
        },
        SortColumn::Attributes => match entry.attribute_letters() {
            "" => t!("group.no_attributes").into_owned(),
            letters => letters.to_owned(),
        },
    }
}

/// Splits the list into sections, merging adjacent entries with the same label. Items inside an
/// expanded folder (`depth` above 0) stay in their folder's section.
fn build(
    entries: &[Entry],
    depth: impl Fn(usize) -> u16,
    column: SortColumn,
    folders_apart: bool,
    today: Today,
    collapsed: &HashSet<SharedString>,
) -> (Vec<Row>, Vec<u32>) {
    let mut rows = Vec::with_capacity(entries.len() + 16);
    let mut row_of = Vec::with_capacity(entries.len());
    let mut current: Option<String> = None;
    let mut header_at = 0usize;
    let mut hide = false;
    for (ix, entry) in entries.iter().enumerate() {
        let nested = depth(ix) > 0 && current.is_some();
        if !nested {
            let group = group_of(entry, column, folders_apart, today);
            if current.as_deref() != Some(group.as_str()) {
                header_at = rows.len();
                let label = SharedString::from(group.clone());
                hide = collapsed.contains(&label);
                rows.push(Row::Header {
                    label,
                    start: ix,
                    count: 0,
                    span: 0,
                    collapsed: hide,
                });
                current = Some(group);
            }
        }
        if let Some(Row::Header { count, span, .. }) = rows.get_mut(header_at) {
            *count += usize::from(!nested);
            *span += 1;
        }
        if hide {
            row_of.push(u32::try_from(header_at).unwrap_or(u32::MAX));
        } else {
            row_of.push(u32::try_from(rows.len()).unwrap_or(u32::MAX));
            rows.push(Row::Item(ix));
        }
    }
    (rows, row_of)
}

/// Items covered by the collapsed sections.
fn hidden_ranges(rows: &[Row]) -> Vec<Range<usize>> {
    rows.iter()
        .filter_map(|r| match r {
            Row::Header {
                start,
                span,
                collapsed: true,
                ..
            } => Some(*start..start + span),
            _ => None,
        })
        .collect()
}

impl Explorer {
    /// What the sections are counted from: the day the list was sorted for, when it is sorted
    /// into date sections (#286, folders go into them then; labels from that same day keep the
    /// sections whole until it is sorted again), otherwise today. And whether folders form a
    /// section of their own.
    fn section_basis(&self) -> Option<(Today, bool)> {
        let sorted = self.model.as_ref()?.sorted_by;
        Some(match sorted.date_sections() {
            Some(days) => (days, false),
            None => (
                day_buckets(self.services.shell.as_ref()),
                self.sort.folders_apart(),
            ),
        })
    }

    /// Whether to group: setting on, details view, not search results or column view. Expanded
    /// folders keep their items under themselves, in the folder's section (#279).
    pub(super) fn grouping(&self) -> bool {
        self.group_rows
            && self.view_mode == ViewMode::Details
            && !self.search_mode
            && !self.columns_active()
    }

    /// Grouped grid layout, or `None` when not grouping.
    pub(crate) fn grid_groups(&self) -> Option<Rc<GridGroups>> {
        if !(self.group_rows && self.view_mode == ViewMode::Icons && !self.search_mode) {
            return None;
        }
        let view = self.view()?;
        let (today, folders_apart) = self.section_basis()?;
        let cols = self.grid_cols.max(1);
        let key = (self.sort.column, folders_apart, today.day, cols);
        if let Some(cached) = self
            .group_cache
            .1
            .borrow()
            .as_ref()
            .filter(|g| g.key == key && g.list.is(&view))
        {
            return Some(Rc::clone(cached));
        }
        let (rows, _) = build(
            &view,
            |_| 0,
            self.sort.column,
            folders_apart,
            today,
            &self.collapsed_groups,
        );
        let grid = Rc::new(GridGroups::new(ListOf::new(&view), key, &rows, cols));
        *self.group_cache.1.borrow_mut() = Some(Rc::clone(&grid));
        Some(grid)
    }

    /// Draws slots `range` of the grouped grid. Headers draw themselves; a tile row draws in its
    /// first
    /// slot and overflows down. If the top slot is mid-row, that row is drawn upward from here.
    pub(super) fn render_grouped_grid(
        &mut self,
        groups: &GridGroups,
        range: std::ops::Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let tile_h = self.tile().h;
        let slot_h = tile_h / TILE_SLOTS as f32;
        let first = range.start;
        let mut wanted: Vec<(usize, usize)> = Vec::new();
        for slot in range.clone() {
            if let Some(Slot::Row(r, k)) = groups.slots.get(slot)
                && (*k == 0 || slot == first)
            {
                wanted.push((*r, *k));
            }
        }
        let ranges: Vec<std::ops::Range<usize>> = wanted
            .iter()
            .map(|(r, _)| groups.rows[*r].0.clone())
            .collect();
        let mut tiles = self.render_tile_rows(ranges, focused, cx).into_iter();
        let mut wanted = wanted.into_iter().peekable();
        range
            .map(|slot| match groups.slots.get(slot) {
                Some(Slot::Header {
                    label,
                    start,
                    count,
                    collapsed,
                }) => {
                    let n = *count;
                    self.group_header(
                        slot,
                        label.clone(),
                        *start,
                        (n, n, *collapsed),
                        Some(slot_h),
                        cx,
                    )
                }
                Some(Slot::Row(r, k)) if wanted.peek() == Some(&(*r, *k)) => {
                    wanted.next();
                    let row = tiles.next().unwrap_or_else(|| div().into_any_element());
                    let up = slot_h * *k as f32;
                    div()
                        .h(slot_h)
                        .relative()
                        .child(
                            div()
                                .absolute()
                                .top(-up)
                                .left_0()
                                .right_0()
                                .h(tile_h)
                                .child(row),
                        )
                        .into_any_element()
                }
                _ => div().h(slot_h).into_any_element(),
            })
            .collect()
    }

    /// Sections of the current list, or `None` when not grouping.
    pub(crate) fn groups(&self) -> Option<Rc<Groups>> {
        if !self.grouping() {
            return None;
        }
        let view = self.view()?;
        let (today, folders_apart) = self.section_basis()?;
        let key = (self.sort.column, folders_apart, today.day);
        if let Some(cached) = self
            .group_cache
            .0
            .borrow()
            .as_ref()
            .filter(|g| g.key == key && g.list.is(&view))
        {
            return Some(Rc::clone(cached));
        }
        // Items of expanded folders, when the list is the tree (not a filter of it).
        let tree = self
            .tree_flat()
            .filter(|(entries, _)| Arc::ptr_eq(entries, &view))
            .map(|(_, rows)| rows);
        let depth = |ix: usize| {
            tree.as_ref()
                .and_then(|rows| rows.get(ix))
                .map_or(0, |r| r.depth)
        };
        let (rows, row_of) = build(
            &view,
            depth,
            self.sort.column,
            folders_apart,
            today,
            &self.collapsed_groups,
        );
        let hidden = hidden_ranges(&rows);
        let groups = Rc::new(Groups {
            list: ListOf::new(&view),
            key,
            rows,
            row_of,
            hidden,
        });
        *self.group_cache.0.borrow_mut() = Some(Rc::clone(&groups));
        Some(groups)
    }

    /// Number of rows in the list, including headers when grouping.
    pub(super) fn display_len(&self, items: usize) -> usize {
        self.groups().map_or(items, |g| g.rows.len())
    }

    /// Row of an item, for scrolling.
    pub(super) fn display_row(&self, ix: usize) -> usize {
        self.groups().map_or(ix, |g| g.row_of(ix))
    }

    /// Item at a row, for marquee selection. `None` for headers.
    pub(super) fn item_at_row(&self, row: usize) -> Option<usize> {
        match self.groups() {
            None => Some(row),
            Some(g) => match g.rows.get(row)? {
                Row::Item(ix) => Some(*ix),
                Row::Header { .. } => None,
            },
        }
    }

    /// Draws rows `range` when grouping. Headers draw themselves; runs of items use the normal
    /// rows.
    pub(super) fn render_grouped_rows(
        &mut self,
        groups: &Groups,
        range: std::ops::Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::with_capacity(range.len());
        let mut run: Option<std::ops::Range<usize>> = None;
        for row in range {
            match groups.rows.get(row) {
                Some(Row::Item(ix)) => {
                    run = Some(match run {
                        Some(r) if r.end == *ix => r.start..ix + 1,
                        Some(r) => {
                            out.extend(self.render_rows(r, focused, &[], cx));
                            *ix..ix + 1
                        }
                        None => *ix..ix + 1,
                    });
                }
                Some(Row::Header {
                    label,
                    start,
                    count,
                    span,
                    collapsed,
                }) => {
                    if let Some(r) = run.take() {
                        out.extend(self.render_rows(r, focused, &[], cx));
                    }
                    let (start, count, span) = (*start, *count, *span);
                    out.push(self.group_header(
                        row,
                        label.clone(),
                        start,
                        (count, span, *collapsed),
                        None,
                        cx,
                    ));
                }
                None => {}
            }
        }
        if let Some(r) = run {
            out.extend(self.render_rows(r, focused, &[], cx));
        }
        out
    }

    /// Flips a section between collapsed and expanded. Collapsing deselects its items, so nothing
    /// the user can't see stays selected.
    pub(crate) fn toggle_group(&mut self, label: &SharedString, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(label) {
            self.collapsed_groups.insert(label.clone());
        }
        self.group_cache.clear();
        self.drop_hidden_selection();
        cx.notify();
    }

    /// Expands every section again (a new folder starts with all sections open).
    pub(super) fn reset_collapsed_groups(&mut self) {
        if !self.collapsed_groups.is_empty() {
            self.collapsed_groups.clear();
            self.group_cache.clear();
        }
    }

    /// Items inside collapsed sections, in order. Empty when nothing is collapsed or the list
    /// isn't grouped.
    fn hidden_items(&self) -> Vec<Range<usize>> {
        if self.collapsed_groups.is_empty() {
            return Vec::new();
        }
        if let Some(groups) = self.groups() {
            groups.hidden.clone()
        } else if let Some(grid) = self.grid_groups() {
            grid.hidden.iter().map(|(r, _)| r.clone()).collect()
        } else {
            Vec::new()
        }
    }

    /// Deselects items of collapsed sections (after Ctrl+A or a Shift range).
    pub(super) fn drop_hidden_selection(&mut self) {
        let hidden = self.hidden_items();
        if hidden.is_empty() || self.selection.is_empty() {
            return;
        }
        let Some(view) = self.view() else { return };
        let names: Vec<Arc<str>> = hidden
            .iter()
            .flat_map(|r| view[r.start.min(view.len())..r.end.min(view.len())].iter())
            .map(|e| Arc::clone(&e.name))
            .collect();
        self.selection.remove_names(&names);
    }

    /// The nearest item at or after (`forward`) or before `ix` that isn't in a collapsed section.
    pub(super) fn skip_collapsed(&self, ix: usize, forward: bool) -> Option<usize> {
        let len = self.list_len();
        let mut at = ix.min(len.checked_sub(1)?);
        let hidden = self.hidden_items();
        loop {
            let p = hidden.partition_point(|r| r.end <= at);
            match hidden.get(p).filter(|r| r.start <= at) {
                None => return Some(at),
                Some(r) if forward => {
                    at = r.end;
                    if at >= len {
                        return None;
                    }
                }
                Some(r) => at = r.start.checked_sub(1)?,
            }
        }
    }

    /// Expands the section holding `ix` if it is collapsed, for something that selects an item by
    /// name or index (type to select, a pasted file). Returns whether it did.
    pub(super) fn reveal_item(&mut self, ix: usize) -> bool {
        if self.collapsed_groups.is_empty() {
            return false;
        }
        let label = if let Some(groups) = self.groups() {
            match groups.rows.get(groups.row_of(ix)) {
                Some(Row::Header {
                    label,
                    collapsed: true,
                    ..
                }) => label.clone(),
                _ => return false,
            }
        } else if let Some(grid) = self.grid_groups() {
            match grid
                .hidden_at(ix)
                .and_then(|(_, slot)| grid.slots.get(*slot))
            {
                Some(Slot::Header { label, .. }) => label.clone(),
                _ => return false,
            }
        } else {
            return false;
        };
        self.collapsed_groups.remove(&label);
        self.group_cache.clear();
        true
    }

    /// Section header with label, count and a rule. Clicking selects the section: `span` items
    /// from `start` (expanded folders' items included). The chevron, or a double click, collapses
    /// and expands it.
    fn group_header(
        &self,
        row: usize,
        label: SharedString,
        start: usize,
        (count, span, collapsed): (usize, usize, bool),
        height: Option<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_chevron = label.clone();
        let on_double = label.clone();
        div()
            .id(("group-header", row))
            .a11y_lazy(gpui::Role::Heading, || format!("{label} ({count})"))
            .aria_level(1)
            .a11y_open_close(!collapsed, cx.entity().downgrade(), {
                let label = label.clone();
                move |this, _, cx| this.toggle_group(&label, cx)
            })
            // Full width, and at least as wide as Details rows when they scroll sideways.
            .w_full()
            .when_some(self.cols, |d, c| d.min_w(c.row))
            .h(height.unwrap_or_else(|| self.row_height()))
            .px(theme::s_12())
            .flex()
            .items_end()
            .pb(theme::s_6())
            .gap(theme::s_8())
            .cursor_pointer()
            .rounded(theme::r_8())
            .hover(|s| s.bg(theme::wash()))
            .child(
                div()
                    .id(("group-chevron", row))
                    .size(theme::tree_chevron())
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::r_4())
                    .hover(|s| s.bg(theme::wash_strong()))
                    // Toggles without selecting the section or starting a marquee.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            window.focus(&this.list_focus, cx);
                            this.toggle_group(&on_chevron, cx);
                        }),
                    )
                    .child(
                        svg()
                            .path(Icon::ChevronRight.path())
                            .size(theme::icon_sm())
                            .text_color(theme::text_subtle())
                            .with_transformation(gpui::Transformation::rotate(gpui::radians(
                                if collapsed {
                                    0.
                                } else {
                                    std::f32::consts::FRAC_PI_2
                                },
                            ))),
                    ),
            )
            // A heading, not a caption: item size, semibold, full ink.
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs_14())
                    .font_weight(theme::FW_STRONG)
                    .text_color(theme::text())
                    .child(label),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(arke_core::format::format_count(count)),
            )
            .child(div().flex_1().h(px(1.)).mb(crate::theme::scaled_px(6.)).bg(theme::line()))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _: &gpui::MouseDownEvent, _, _| {
                // Don't start a marquee from a header.
                this.row_pressed = true;
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                window.focus(&this.list_focus, cx);
                if event.click_count() >= 2 {
                    this.toggle_group(&on_double, cx);
                    return;
                }
                // A collapsed section has nothing on screen to select.
                if collapsed {
                    return;
                }
                let Some(view) = this.view() else {
                    return;
                };
                // Ctrl adds to the existing selection.
                let mut names: Vec<std::sync::Arc<str>> = if event.modifiers().control {
                    this.selection.names().cloned().collect()
                } else {
                    Vec::new()
                };
                let end = (start + span).min(view.len());
                names.extend(view[start.min(end)..end].iter().map(|e| Arc::clone(&e.name)));
                this.selection.select_names(names);
                this.selection
                    .set_cursor_name(view.get(start).map(|e| Arc::clone(&e.name)));
                this.cursor = Some(start);
                cx.notify();
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_rows_break_at_groups_and_step_by_column() {
        // The locale is process-wide; every test that sets it uses English, the default.
        rust_i18n::set_locale("en");
        let today = Today {
            day: 20_000,
            offset: 0,
        };
        let mut entries = vec![Entry::new("docs", EntryKind::Dir, 0, None, 0)];
        for name in ["a1", "a2", "a3", "a4", "a5", "b1", "b2"] {
            entries.push(Entry::new(name, EntryKind::File, 1, None, 0));
        }
        let (rows, _) = build(
            &entries,
            |_| 0,
            SortColumn::Name,
            true,
            today,
            &HashSet::new(),
        );
        let grid = GridGroups::new(
            ListOf(std::sync::Weak::new()),
            (SortColumn::Name, true, 0, 3),
            &rows,
            3,
        );
        // Folders 1 row, A two rows (3+2), B 1 row; a header slot before each section.
        let ranges: Vec<_> = grid.rows.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, [0..1, 1..4, 4..6, 6..8]);
        assert_eq!(grid.slots.len(), 3 + 4 * TILE_SLOTS);
        assert_eq!(grid.slot_of(0), 1);
        assert_eq!(grid.slot_of(5), 1 + TILE_SLOTS + 1 + TILE_SLOTS);
        // Down from a3 (column 2): the next row has only two items, so a5; then column 2 of B, b2.
        assert_eq!(grid.step(3, 1), 5);
        assert_eq!(grid.step(5, 1), 7);
        assert_eq!(grid.step(7, -1), 5);
        assert_eq!(grid.step(0, -1), 0);
        // Slots 2-3 (A's first row).
        let hit: Vec<_> = grid.rows_in_slots(6, 7).cloned().collect();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0], 1..4);
    }
    use arke_core::EntryKind;
    use std::time::Duration;

    fn at(today: Today, days_ago: i64) -> SystemTime {
        let secs = today.day * 86_400 - today.offset + 12 * 3600 - days_ago * 86_400;
        SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap())
    }

    #[test]
    fn dates_fall_into_relative_buckets() {
        rust_i18n::set_locale("en");
        // 2026-11-10 is a Tuesday.
        let today = Today {
            day: days_from_civil(2026, 11, 10),
            offset: 8 * 3600,
        };
        let g = |ago| date_group(Some(at(today, ago)), today);
        assert_eq!(g(0), t!("group.today"));
        assert_eq!(g(1), t!("group.yesterday"));
        assert_eq!(g(3), t!("group.last_week"), "Nov 7 is last Saturday");
        assert_eq!(g(9), t!("group.this_month"));
        assert_eq!(g(20), t!("group.last_month"));
        assert_eq!(g(100), t!("group.this_year"));
        assert_eq!(g(400), t!("group.year", year = 2025));
    }

    #[test]
    fn runs_become_groups_with_counts() {
        rust_i18n::set_locale("en");
        let today = Today {
            day: 20_000,
            offset: 0,
        };
        let entries = vec![
            Entry::new("docs", EntryKind::Dir, 0, None, 0),
            Entry::new("apple.txt", EntryKind::File, 1, None, 0),
            Entry::new("avocado.txt", EntryKind::File, 1, None, 0),
            Entry::new("banana.txt", EntryKind::File, 1, None, 0),
            // A non-ASCII first letter falls into "Other".
            Entry::new("報告.docx", EntryKind::File, 1, None, 0),
        ];
        let (rows, row_of) = build(
            &entries,
            |_| 0,
            SortColumn::Name,
            true,
            today,
            &HashSet::new(),
        );
        let headers: Vec<(String, usize)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Header { label, count, .. } => Some((label.to_string(), *count)),
                Row::Item(_) => None,
            })
            .collect();
        assert_eq!(
            headers,
            [
                (arke_core::format::type_label(&entries[0]), 1),
                ("A".into(), 2),
                ("B".into(), 1),
                (t!("group.other").into_owned(), 1)
            ]
        );
        assert_eq!(rows.len(), entries.len() + 4);
        // Every item maps back to its row.
        for (ix, row) in row_of.iter().enumerate() {
            assert_eq!(rows[*row as usize], Row::Item(ix));
        }
    }

    /// #276: with folders and files together, a date sort puts folders in the section of their
    /// date; kept apart they have a section of their own.
    #[test]
    fn folders_go_by_date_when_not_kept_apart() {
        rust_i18n::set_locale("en");
        let today = Today {
            day: days_from_civil(2026, 11, 10),
            offset: 0,
        };
        let spec = arke_core::SortSpec::by(SortColumn::Modified, false, true);
        let with_date = |name: &str, kind, ago| Entry::new(name, kind, 1, Some(at(today, ago)), 0);
        let entries = vec![
            with_date("new.txt", EntryKind::File, 0),
            with_date("new-dir", EntryKind::Dir, 0),
            with_date("old.txt", EntryKind::File, 400),
            with_date("old-dir", EntryKind::Dir, 400),
        ];
        let labels = |apart: bool| -> Vec<(String, usize)> {
            build(
                &entries,
                |_| 0,
                SortColumn::Modified,
                apart,
                today,
                &HashSet::new(),
            )
            .0
            .iter()
            .filter_map(|r| match r {
                Row::Header { label, count, .. } => Some((label.to_string(), *count)),
                Row::Item(_) => None,
            })
            .collect()
        };
        assert!(!spec.folders_apart());
        assert_eq!(
            labels(spec.folders_apart()),
            [(t!("group.today").into_owned(), 2), ("2025".into(), 2)]
        );
        // Kept apart: the folders form sections of their own.
        assert_eq!(labels(true).len(), 4);
    }

    #[test]
    fn collapsed_sections_keep_their_header_and_lose_their_rows() {
        rust_i18n::set_locale("en");
        let today = Today {
            day: 20_000,
            offset: 0,
        };
        let entries = vec![
            Entry::new("docs", EntryKind::Dir, 0, None, 0),
            Entry::new("apple.txt", EntryKind::File, 1, None, 0),
            Entry::new("avocado.txt", EntryKind::File, 1, None, 0),
            Entry::new("banana.txt", EntryKind::File, 1, None, 0),
        ];
        let collapsed: HashSet<SharedString> = [SharedString::from("A")].into_iter().collect();
        let (rows, row_of) = build(&entries, |_| 0, SortColumn::Name, true, today, &collapsed);
        // Three headers and the two items outside A.
        assert_eq!(rows.len(), 5);
        assert!(matches!(
            &rows[2],
            Row::Header {
                count: 2,
                span: 2,
                collapsed: true,
                ..
            }
        ));
        // A's items point at their header, so scrolling to one lands there.
        assert_eq!(row_of, [1, 2, 2, 4]);
        assert_eq!(hidden_ranges(&rows), [Range { start: 1, end: 3 }]);
    }

    #[test]
    fn grid_skips_collapsed_sections() {
        rust_i18n::set_locale("en");
        let today = Today {
            day: 20_000,
            offset: 0,
        };
        let mut entries = vec![Entry::new("docs", EntryKind::Dir, 0, None, 0)];
        for name in ["a1", "a2", "a3", "b1"] {
            entries.push(Entry::new(name, EntryKind::File, 1, None, 0));
        }
        let collapsed: HashSet<SharedString> = [SharedString::from("A")].into_iter().collect();
        let (rows, _) = build(&entries, |_| 0, SortColumn::Name, true, today, &collapsed);
        let grid = GridGroups::new(
            ListOf(std::sync::Weak::new()),
            (SortColumn::Name, true, 0, 3),
            &rows,
            3,
        );
        // Folders: header + row; A: header only; B: header + row.
        let ranges: Vec<_> = grid.rows.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, [0..1, 4..5]);
        assert_eq!(grid.slots.len(), 3 + 2 * TILE_SLOTS);
        assert_eq!(grid.hidden, [(1..4, 5)]);
        assert_eq!(grid.row_of(2), None);
        assert_eq!(grid.row_of(4), Some(1));
        assert_eq!(grid.slot_of(2), 5, "a hidden item scrolls to its header");
        // Down from docs goes over A to b1; up comes back.
        assert_eq!(grid.step(0, 1), 4);
        assert_eq!(grid.step(4, -1), 0);
        // From inside the collapsed section too.
        assert_eq!(grid.step(2, 1), 4);
        assert_eq!(grid.step(2, -1), 0);
    }
}
