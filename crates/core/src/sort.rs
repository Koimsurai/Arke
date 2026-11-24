//! Sorting of directory listings (natural name order, folders first like File Explorer).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::entry::{Entry, EntryKind};
use crate::search::SearchHit;

/// Column a listing is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum SortColumn {
    /// Name in natural order ("file2" before "file10"), case-insensitive.
    #[default]
    Name,
    /// Last write time.
    Modified,
    /// Type text as shown in the Type column (natural order), like File Explorer.
    Type,
    /// Size in bytes (folders use computed sizes where available).
    Size,
    /// Creation time.
    Created,
    /// Extension on its own column (sorted by the extension itself, with its own groups).
    Extension,
    /// Read-only, Hidden, System and Archive attributes, by their letters.
    Attributes,
}

impl SortColumn {
    /// Direction a column starts in when first picked: dates and sizes newest/largest first, text
    /// columns A to Z, which is what people expect.
    #[must_use]
    pub const fn starts_ascending(self) -> bool {
        matches!(
            self,
            Self::Name | Self::Type | Self::Extension | Self::Attributes
        )
    }
}

/// One sort key: a column and its direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SortKey {
    /// Column compared.
    pub column: SortColumn,
    /// Ascending order.
    pub ascending: bool,
}

/// Most keys a sort can have: the primary column plus two more (Shift+click in the header).
pub const MAX_SORT_KEYS: usize = 3;

/// How to order a listing; ties always fall back to name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SortSpec {
    /// Primary sort column.
    pub column: SortColumn,
    /// Ascending order. With folders kept apart ([`Self::folders_apart`]) they lead an ascending
    /// sort and trail a descending one, as in File Explorer.
    pub ascending: bool,
    /// Sort folders and files together (default `false`: folders kept apart, see
    /// [`Self::folders_apart`]).
    pub mixed: bool,
    /// Secondary and tertiary keys, compared in order when the ones before tie. Filled from the
    /// front: a `None` is never followed by a key. A fixed array keeps the spec `Copy` and the
    /// comparison free of allocation.
    pub then: [Option<SortKey>; MAX_SORT_KEYS - 1],
    /// The list is grouped by its date column (#286): with folders kept apart, they still go into
    /// the section of their date, as in File Explorer, so the sections come first and the folder
    /// block applies within each. `None` when not grouping by a date.
    pub sections: Option<DayBuckets>,
}

/// The day the date sections (today, yesterday, ..., years) are counted from: the local day
/// number and the local offset from UTC in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DayBuckets {
    /// Today's local day number (days since 1970-01-01).
    pub day: i64,
    /// Local offset from UTC, in seconds.
    pub offset: i64,
}

/// A date section, in time order (oldest first): unknown dates, then years, then the relative
/// sections of this year up to today, then dates in the future. Weeks start on Monday.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DateSection {
    /// No date.
    Unknown,
    /// An earlier year.
    Year(i64),
    /// Earlier this year.
    ThisYear,
    /// Last month.
    LastMonth,
    /// Earlier this month.
    ThisMonth,
    /// Last week.
    LastWeek,
    /// Earlier this week.
    ThisWeek,
    /// Yesterday.
    Yesterday,
    /// Today.
    Today,
    /// After today.
    Future,
}

impl DayBuckets {
    /// The section `t` falls in.
    #[must_use]
    pub fn section(self, t: Option<std::time::SystemTime>) -> DateSection {
        let Some(t) = t else {
            return DateSection::Unknown;
        };
        let day = (unix_secs(t) + self.offset).div_euclid(86_400);
        let now = self.day;
        let week_start = now - (now + 3).rem_euclid(7);
        let (y, m) = year_month(day);
        let (ny, nm) = year_month(now);
        if day > now {
            DateSection::Future
        } else if day == now {
            DateSection::Today
        } else if day == now - 1 {
            DateSection::Yesterday
        } else if day >= week_start {
            DateSection::ThisWeek
        } else if day >= week_start - 7 {
            DateSection::LastWeek
        } else if (y, m) == (ny, nm) {
            DateSection::ThisMonth
        } else if y * 12 + m == ny * 12 + nm - 1 {
            DateSection::LastMonth
        } else if y == ny {
            DateSection::ThisYear
        } else {
            DateSection::Year(y)
        }
    }
}

/// Seconds since the Unix epoch, negative before it.
fn unix_secs(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Civil date to days since 1970-01-01 (Howard Hinnant's algorithm).
#[must_use]
pub const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Day number to (year, month).
#[must_use]
pub const fn year_month(days: i64) -> (i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m)
}

impl Default for SortSpec {
    fn default() -> Self {
        Self::by(SortColumn::Name, true, false)
    }
}

impl SortSpec {
    /// A single-key sort.
    #[must_use]
    pub const fn by(column: SortColumn, ascending: bool, mixed: bool) -> Self {
        Self {
            column,
            ascending,
            mixed,
            then: [None; MAX_SORT_KEYS - 1],
            sections: None,
        }
    }

    /// The date sections the list is grouped in, when folders go into them (#286): grouping by
    /// the primary column, which is a date, with folders kept apart (mixed, they already do).
    #[must_use]
    pub fn date_sections(&self) -> Option<DayBuckets> {
        self.sections.filter(|_| {
            self.folders_apart()
                && matches!(self.column, SortColumn::Modified | SortColumn::Created)
        })
    }

    /// All keys in order, the primary first.
    pub fn keys(&self) -> impl Iterator<Item = SortKey> + '_ {
        std::iter::once(SortKey {
            column: self.column,
            ascending: self.ascending,
        })
        .chain(self.then.iter().flatten().copied())
    }

    /// Number of keys (1 to [`MAX_SORT_KEYS`]).
    #[must_use]
    pub fn key_count(&self) -> usize {
        1 + self.then.iter().flatten().count()
    }

    /// Where `column` is among the keys (0 is the primary) and its direction.
    #[must_use]
    pub fn key_of(&self, column: SortColumn) -> Option<(usize, bool)> {
        self.keys()
            .enumerate()
            .find(|(_, k)| k.column == column)
            .map(|(ix, k)| (ix, k.ascending))
    }

    /// Whether any key compares `column`.
    #[must_use]
    pub fn uses(&self, column: SortColumn) -> bool {
        self.keys().any(|k| k.column == column)
    }

    /// Same keys and directions (where folders go and the date sections aside).
    #[must_use]
    pub fn same_order(&self, other: &Self) -> bool {
        self.keys().eq(other.keys())
    }

    /// Plain click on a column header: sort by that column alone. Clicking the primary column flips
    /// its direction; another column starts in its natural direction
    /// ([`SortColumn::starts_ascending`]). Any further keys are dropped.
    #[must_use]
    pub const fn toggled(self, column: SortColumn) -> Self {
        let ascending = if matches_column(self.column, column) {
            !self.ascending
        } else {
            column.starts_ascending()
        };
        Self::by(column, ascending, self.mixed)
    }

    /// Shift+click on a column header: a column already in the sort flips its direction; a new one
    /// is added as the next key (once there are [`MAX_SORT_KEYS`], it replaces the last one).
    #[must_use]
    pub fn shift_toggled(self, column: SortColumn) -> Self {
        let mut next = self;
        if self.column == column {
            next.ascending = !next.ascending;
            return next;
        }
        if let Some(key) = next.then.iter_mut().flatten().find(|k| k.column == column) {
            key.ascending = !key.ascending;
            return next;
        }
        let slot = next
            .then
            .iter()
            .position(Option::is_none)
            .unwrap_or(MAX_SORT_KEYS - 2);
        next.then[slot] = Some(SortKey {
            column,
            ascending: column.starts_ascending(),
        });
        next
    }

    /// Whether folders form their own block (everything but "folders and files together"). Like
    /// File Explorer, the block leads an ascending sort and trails a descending one, dates
    /// included, except by Type, where folders lead either way ([`Self::folders_lead`]).
    #[must_use]
    pub const fn folders_apart(&self) -> bool {
        !self.mixed
    }

    /// Whether the folder block comes before the files. Only the primary key counts; Shift+click
    /// keys never move folders.
    #[must_use]
    pub const fn folders_lead(&self) -> bool {
        self.ascending || matches!(self.column, SortColumn::Type)
    }

    /// Full comparison: folders as a block first (usually last when descending; see
    /// [`Self::folders_lead`]), then each key in turn, then name (in the primary direction) as a
    /// tiebreaker so the order is total.
    ///
    /// Type texts are read afresh on every call, so for many comparisons in a row (sorting,
    /// searching a sorted list) take [`TypeKeys::for_entries`] once and use [`Self::compare_with`].
    pub fn compare(&self, a: &Entry, b: &Entry) -> Ordering {
        self.compare_with(a, b, &TypeKeys::default())
    }

    /// [`Self::compare`] with type texts frozen in `types`.
    pub fn compare_with(&self, a: &Entry, b: &Entry, types: &TypeKeys) -> Ordering {
        self.compare_sized(a, b, a.size, b.size, types)
    }

    /// Like `compare`, but the caller supplies sizes (folder sizes are computed separately, not in
    /// `Entry`).
    fn compare_sized(
        &self,
        a: &Entry,
        b: &Entry,
        size_a: u64,
        size_b: u64,
        types: &TypeKeys,
    ) -> Ordering {
        if let Some(days) = self.date_sections() {
            let ord = days
                .section(self.date_of(a))
                .cmp(&days.section(self.date_of(b)));
            if ord != Ordering::Equal {
                return if self.ascending { ord } else { ord.reverse() };
            }
        }
        if self.folders_apart() {
            let lead = self.folders_lead();
            match (a.is_dir_like(), b.is_dir_like()) {
                (true, false) => {
                    return if lead {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
                (false, true) => {
                    return if lead {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    };
                }
                _ => {}
            }
        }
        let ord = column_cmp(self.column, a, b, (size_a, size_b), types);
        if ord != Ordering::Equal {
            return if self.ascending { ord } else { ord.reverse() };
        }
        for key in self.then.iter().flatten() {
            let ord = column_cmp(key.column, a, b, (size_a, size_b), types);
            if ord != Ordering::Equal {
                return if key.ascending { ord } else { ord.reverse() };
            }
        }
        let ord = name_cmp(a, b);
        if self.ascending { ord } else { ord.reverse() }
    }

    /// The date the date sections go by: the primary column's.
    const fn date_of(&self, e: &Entry) -> Option<std::time::SystemTime> {
        if matches!(self.column, SortColumn::Created) {
            e.created
        } else {
            e.modified
        }
    }

    /// Sorts `entries` in place by [`Self::compare`].
    pub fn sort(&self, entries: &mut [Entry]) {
        // Unstable sort is faster, and the comparator is a total order, so the result is
        // deterministic.
        let types = TypeKeys::for_entries(self, &*entries);
        // By name every entry needs its key: make them up front, on several threads for big lists.
        // Other columns only need them to break ties, made as they come.
        if matches_column(self.column, SortColumn::Name) {
            crate::collate::prepare(entries);
        }
        if let Some(days) = self.date_sections() {
            // Sorted as usual, the folder block and the file block are each in date order, so
            // one merge by section puts the folders into their sections: the section of each
            // item is worked out once, and the sort costs about what it does ungrouped.
            let rest = Self {
                sections: None,
                ..*self
            };
            entries.sort_unstable_by(|a, b| rest.compare_with(a, b, &types));
            let split = entries
                .iter()
                .position(|e| e.is_dir_like() != entries[0].is_dir_like())
                .unwrap_or(entries.len());
            let sections: Vec<DateSection> = entries
                .iter()
                .map(|e| days.section(self.date_of(e)))
                .collect();
            let first = |a: DateSection, b: DateSection| {
                if self.ascending { a <= b } else { a >= b }
            };
            let (mut i, mut j) = (0, split);
            let mut from = Vec::with_capacity(entries.len());
            while i < split || j < entries.len() {
                // Same section: the leading block's items first.
                if j == entries.len() || (i < split && first(sections[i], sections[j])) {
                    from.push(i);
                    i += 1;
                } else {
                    from.push(j);
                    j += 1;
                }
            }
            // Into place by following the permutation's cycles: swaps, no copies.
            for start in 0..from.len() {
                let mut at = start;
                while from[at] != start {
                    let next = from[at];
                    entries.swap(at, next);
                    from[at] = at;
                    at = next;
                }
                from[at] = at;
            }
            return;
        }
        entries.sort_unstable_by(|a, b| self.compare_with(a, b, &types));
    }

    /// Returns a sorted copy. When sorting by size, folders use separately computed sizes
    /// (`dir_sizes`: name -> bytes, missing counts as 0). Otherwise same as `sort`.
    #[must_use]
    pub fn sorted_with_dir_sizes(
        &self,
        entries: &[Entry],
        dir_sizes: &HashMap<Arc<str>, u64>,
    ) -> Vec<Entry> {
        if !self.uses(SortColumn::Size) {
            let mut v = entries.to_vec();
            self.sort(&mut v);
            return v;
        }
        // Look up sizes once up front instead of in every comparison.
        let mut sized: Vec<(u64, &Entry)> = entries
            .iter()
            .map(|e| {
                let size = if e.is_dir_like() {
                    dir_sizes.get(&e.name).copied().unwrap_or(0)
                } else {
                    e.size
                };
                (size, e)
            })
            .collect();
        let types = TypeKeys::for_entries(self, entries);
        sized.sort_unstable_by(|a, b| self.compare_sized(a.1, b.1, a.0, b.0, &types));
        sized.into_iter().map(|(_, e)| e.clone()).collect()
    }
}

/// `const` column equality (derived `PartialEq` isn't `const`).
const fn matches_column(a: SortColumn, b: SortColumn) -> bool {
    a as u8 == b as u8
}

/// One column's order for two entries (ascending), `Equal` when they tie on it.
fn column_cmp(
    column: SortColumn,
    a: &Entry,
    b: &Entry,
    (size_a, size_b): (u64, u64),
    types: &TypeKeys,
) -> Ordering {
    match column {
        // Name is the final tiebreaker too; as a key it decides on its own.
        SortColumn::Name => name_cmp(a, b),
        SortColumn::Modified => a.modified.cmp(&b.modified),
        SortColumn::Type => type_cmp(a, b, types),
        SortColumn::Extension => a.extension().cmp(b.extension()),
        SortColumn::Size => size_a.cmp(&size_b),
        SortColumn::Created => a.created.cmp(&b.created),
        SortColumn::Attributes => a.attribute_letters().cmp(b.attribute_letters()),
    }
}

/// Lowercased type texts, one per extension, frozen when a sort starts: the texts can change
/// while a sort runs (Windows' type names arrive on another thread), and a comparison whose
/// answers change midway is no total order. Never asks the shell. Each text comes with its
/// Windows sort key (`collate.rs`), made once per extension, as names have theirs (#331).
#[derive(Default)]
pub struct TypeKeys {
    keys: HashMap<String, TypeKey>,
}

/// A type text and, where Windows orders texts, its sort key.
struct TypeKey {
    text: Arc<str>,
    collation: Option<Arc<[u8]>>,
}

impl TypeKey {
    fn of(e: &Entry) -> Self {
        let text = type_text(e);
        let collation = crate::collate::available().then(|| crate::collate::key(&text));
        Self { text, collation }
    }
}

/// What the text of `e` is keyed by: folders and folder links have their own texts (no
/// extension can contain `<`), files go by extension.
fn type_id(e: &Entry) -> &str {
    match e.kind {
        EntryKind::Dir => "<dir>",
        EntryKind::Link { is_dir: true } => "<link>",
        _ => e.extension(),
    }
}

fn type_text(e: &Entry) -> Arc<str> {
    crate::format::type_label(e).to_lowercase().into()
}

impl TypeKeys {
    /// The texts of `entries` as they are now, when `spec` sorts by type (otherwise none are
    /// needed). Takes one text per distinct extension.
    pub fn for_entries<'a>(spec: &SortSpec, entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        let mut keys = HashMap::new();
        if spec.uses(SortColumn::Type) {
            for e in entries {
                let id = type_id(e);
                if !keys.contains_key(id) {
                    keys.insert(id.to_owned(), TypeKey::of(e));
                }
            }
        }
        Self { keys }
    }

    /// The frozen key of `e`'s type, or one made now into `own` for a type the sort didn't see.
    fn key<'a>(&'a self, e: &Entry, own: &'a mut Option<TypeKey>) -> &'a TypeKey {
        if let Some(known) = self.keys.get(type_id(e)) {
            known
        } else {
            own.insert(TypeKey::of(e))
        }
    }

    fn cmp(&self, a: &Entry, b: &Entry) -> Ordering {
        let (mut own_a, mut own_b) = (None, None);
        let a = self.key(a, &mut own_a);
        let b = self.key(b, &mut own_b);
        match (&a.collation, &b.collation) {
            (Some(x), Some(y)) => x.cmp(y).then_with(|| natural_cmp(&a.text, &b.text)),
            _ => natural_cmp(&a.text, &b.text),
        }
    }
}

/// Type column order: the shown type text in File Explorer's order (`collate.rs`; natural order
/// where Windows can't give it), not the extension: File Explorer sorts by the type's display
/// name too.
fn type_cmp(a: &Entry, b: &Entry, types: &TypeKeys) -> Ordering {
    // Same kind of item with the same extension: same text, no lookup.
    if a.kind == b.kind && a.extension() == b.extension() {
        return Ordering::Equal;
    }
    types.cmp(a, b)
}

/// File Explorer's name order (`collate.rs`), or the natural, case-insensitive order where Windows
/// can't give it; then the exact name so the order is total.
fn name_cmp(a: &Entry, b: &Entry) -> Ordering {
    let ord = if crate::collate::available() {
        a.collation().cmp(b.collation())
    } else {
        natural_cmp(&a.shown_key(), &b.shown_key())
    };
    ord.then_with(|| a.name.cmp(&b.name))
}

/// Natural sort: digit runs compare numerically (file2 < file10), everything else per char, with
/// spaces and ASCII symbols before digits and digits before letters, as in File Explorer
/// (`_notes` before `01 notes`, #311). Inputs must already be lowercase.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < ab.len() && j < bb.len() {
        let (ca, cb) = (ab[i], bb[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let si = i;
            while i < ab.len() && ab[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < bb.len() && bb[j].is_ascii_digit() {
                j += 1;
            }
            let na = trim_zeros(&ab[si..i]);
            let nb = trim_zeros(&bb[sj..j]);
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
            // Equal values: fewer leading zeros first.
            let ord = (i - si).cmp(&(j - sj));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            // UTF-8 byte order matches code point order, so compare bytes directly. Don't slice the
            // &str here: two multibyte chars can share a lead byte, and the index would land inside
            // a char. Digits are ASCII and never occur inside a multibyte char.
            if ca != cb {
                return char_rank(ca).cmp(&char_rank(cb));
            }
            i += 1;
            j += 1;
        }
    }
    (ab.len() - i).cmp(&(bb.len() - j))
}

/// Where a byte goes in [`natural_cmp`]: ASCII spaces and symbols, then digits, then the rest
/// (letters, and every byte of a multibyte char, in code point order).
const fn char_rank(c: u8) -> (u8, u8) {
    let group = if c.is_ascii_digit() {
        1
    } else if c.is_ascii() && !c.is_ascii_alphabetic() {
        0
    } else {
        2
    };
    (group, c)
}

/// A column of the search results that can be sorted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SearchColumn {
    /// Name in natural order.
    Name,
    /// The folder the item is in.
    Location,
    /// Last write time.
    Modified,
    /// Size in bytes.
    Size,
}

impl SearchColumn {
    /// Direction a column starts in when first picked (see [`SortColumn::starts_ascending`]).
    #[must_use]
    pub const fn starts_ascending(self) -> bool {
        matches!(self, Self::Name | Self::Location)
    }

    /// Short stable name, for test selectors.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Location => "location",
            Self::Modified => "modified",
            Self::Size => "size",
        }
    }
}

/// How the search results are ordered when a header was clicked. Without one they stay in the
/// search's own relevance order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchSort {
    /// Column compared.
    pub column: SearchColumn,
    /// Ascending (A to Z, oldest, smallest first).
    pub ascending: bool,
}

impl SearchSort {
    /// `column` in the direction it starts in.
    #[must_use]
    pub const fn new(column: SearchColumn) -> Self {
        Self {
            column,
            ascending: column.starts_ascending(),
        }
    }

    /// The same column the other way round.
    #[must_use]
    pub const fn flipped(self) -> Self {
        Self {
            column: self.column,
            ascending: !self.ascending,
        }
    }

    /// What a click on `column`'s header leads to: its first direction, then the other, then back
    /// to relevance (`None`).
    #[must_use]
    pub fn after_click(current: Option<Self>, column: SearchColumn) -> Option<Self> {
        match current {
            Some(sort) if sort.column == column => {
                (sort.ascending == column.starts_ascending()).then(|| sort.flipped())
            }
            _ => Some(Self::new(column)),
        }
    }
}

/// The folder part of a hit's path, lowercase, for the Location column.
fn hit_location(hit: &SearchHit) -> String {
    hit.rel
        .rsplit_once(['\\', '/'])
        .map_or_else(String::new, |(parent, _)| parent.to_lowercase())
}

/// `hits` ordered by `sort`. Hits that tie keep the order they came in (relevance), in either
/// direction. Folders are mixed with files, as in the relevance order.
#[must_use]
pub fn sort_search_hits(hits: &[Arc<SearchHit>], sort: SearchSort) -> Vec<Arc<SearchHit>> {
    let directed = |ord: Ordering| if sort.ascending { ord } else { ord.reverse() };
    let mut out = hits.to_vec();
    match sort.column {
        SearchColumn::Name => out.sort_by(|a, b| directed(name_cmp(&a.entry, &b.entry))),
        SearchColumn::Modified => {
            out.sort_by(|a, b| directed(a.entry.modified.cmp(&b.entry.modified)));
        }
        SearchColumn::Size => out.sort_by(|a, b| directed(a.entry.size.cmp(&b.entry.size))),
        SearchColumn::Location => {
            // Each folder text is built once, not on every comparison.
            let mut keyed: Vec<(String, Arc<SearchHit>)> = out
                .into_iter()
                .map(|hit| (hit_location(&hit), hit))
                .collect();
            keyed.sort_by(|a, b| directed(natural_cmp(&a.0, &b.0)));
            out = keyed.into_iter().map(|(_, hit)| hit).collect();
        }
    }
    out
}

fn trim_zeros(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| c == b'0').count();
    &s[n..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "file2", "file1", "file02", "a", "b1c", "b01c"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            ["a", "b1c", "b01c", "file1", "file2", "file02", "file10"]
        );
    }

    /// #311: symbols first, then digits, then letters, as File Explorer has them.
    #[test]
    fn symbols_go_before_digits() {
        let mut v = vec![
            "01 folder two",
            "_folder one",
            "alpha",
            "(draft)",
            "1a",
            "z",
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            [
                "(draft)",
                "_folder one",
                "1a",
                "01 folder two",
                "alpha",
                "z"
            ]
        );
        assert_eq!(natural_cmp("_text.txt", "01 text.txt"), Ordering::Less);
        assert_eq!(natural_cmp("a_b", "a1"), Ordering::Less);
    }

    #[test]
    fn natural_handles_cjk() {
        assert_eq!(natural_cmp("中文2", "中文10"), Ordering::Less);
        assert_eq!(natural_cmp("中", "中文"), Ordering::Less);
        // Regression: different chars sharing a UTF-8 lead byte used to slice mid-char and panic.
        assert_eq!(natural_cmp("報告", "報表"), "報告".cmp("報表"));
        assert_eq!(natural_cmp("照片_1", "專案_1"), "照片".cmp("專案"));
        assert_eq!(natural_cmp("a報", "a1"), Ordering::Greater);
    }

    /// Random strings (CJK, digits, emoji): must not panic and must be a total order.
    #[test]
    fn natural_is_total_order_on_random_strings() {
        const ALPHABET: &[&str] = &[
            "a", "b", "0", "1", "9", "報", "表", "告", "照", "專", "é", "🙂", "_", ".",
        ];
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let words: Vec<String> = (0..400)
            .map(|_| {
                let len = (next() % 6) as usize;
                (0..len)
                    .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize])
                    .collect()
            })
            .collect();
        for a in &words {
            assert_eq!(natural_cmp(a, a), Ordering::Equal);
            for b in &words {
                assert_eq!(
                    natural_cmp(a, b),
                    natural_cmp(b, a).reverse(),
                    "{a:?} {b:?}"
                );
            }
        }
        let mut sorted = words;
        sorted.sort_by(|a, b| natural_cmp(a, b));
        for w in sorted.windows(3) {
            assert_ne!(natural_cmp(&w[0], &w[2]), Ordering::Greater);
        }
    }

    /// #177: like File Explorer, folders lead an ascending sort and trail a descending one.
    #[test]
    fn folders_lead_ascending_and_trail_descending() {
        let d = Entry::new("zzz", EntryKind::Dir, 0, None, 0);
        let f = Entry::new("aaa", EntryKind::File, 5, None, 0);
        for column in [
            SortColumn::Name,
            SortColumn::Modified,
            SortColumn::Size,
            SortColumn::Created,
            SortColumn::Extension,
            SortColumn::Attributes,
        ] {
            let up = SortSpec::by(column, true, false);
            assert_eq!(up.compare(&d, &f), Ordering::Less, "{column:?} ascending");
            assert_eq!(up.compare(&f, &d), Ordering::Greater);
            let down = SortSpec::by(column, false, false);
            assert_eq!(
                down.compare(&d, &f),
                Ordering::Greater,
                "{column:?} descending"
            );
            assert_eq!(down.compare(&f, &d), Ordering::Less);
        }
    }

    /// #177: sorted by a date, folders stay a block like in File Explorer (checked on Windows 11):
    /// newest files first, then the folders newest first.
    #[test]
    fn dates_keep_folders_apart() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |name: &str, kind: EntryKind, secs: u64| {
            Entry::new(
                name,
                kind,
                1,
                Some(UNIX_EPOCH + Duration::from_secs(secs)),
                0,
            )
            .with_created(Some(UNIX_EPOCH + Duration::from_secs(secs)))
        };
        let mut v = vec![
            at("old.txt", EntryKind::File, 10),
            at("mid-dir", EntryKind::Dir, 20),
            at("new.txt", EntryKind::File, 30),
            at("old-dir", EntryKind::Dir, 5),
        ];
        for column in [SortColumn::Modified, SortColumn::Created] {
            let down = SortSpec::by(column, false, false);
            down.sort(&mut v);
            assert_eq!(names(&v), ["new.txt", "old.txt", "mid-dir", "old-dir"]);
            let up = SortSpec::by(column, true, false);
            up.sort(&mut v);
            assert_eq!(names(&v), ["old-dir", "mid-dir", "old.txt", "new.txt"]);
            // Together with files, they sort by date among them.
            SortSpec::by(column, false, true).sort(&mut v);
            assert_eq!(names(&v), ["new.txt", "mid-dir", "old.txt", "old-dir"]);
        }
    }

    /// The primary key decides where folders go; Shift+click keys don't move them.
    #[test]
    fn folder_placement_follows_the_primary_key() {
        let d = Entry::new("d", EntryKind::Dir, 0, None, 0);
        let f = Entry::new("f", EntryKind::File, 1, None, 0);
        let down_second =
            SortSpec::by(SortColumn::Name, true, false).shift_toggled(SortColumn::Modified);
        assert!(down_second.folders_lead());
        assert_eq!(down_second.compare(&d, &f), Ordering::Less);
        let down_first =
            SortSpec::by(SortColumn::Modified, false, false).shift_toggled(SortColumn::Name);
        assert!(!down_first.folders_lead());
        assert_eq!(down_first.compare(&d, &f), Ordering::Greater);
        // By Type folders lead either way, as in File Explorer.
        assert!(SortSpec::by(SortColumn::Type, false, false).folders_lead());
        // "Folders and files together" is always mixed.
        assert!(!SortSpec::by(SortColumn::Name, true, true).folders_apart());
    }

    #[test]
    fn sorts_by_creation_time_newest_first_by_default() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |name: &str, secs: u64| {
            Entry::new(name, EntryKind::File, 1, None, 0)
                .with_created(Some(UNIX_EPOCH + Duration::from_secs(secs)))
        };
        let mut v = vec![at("old", 10), at("new", 30), at("mid", 20)];
        let spec = SortSpec::default().toggled(SortColumn::Created);
        assert!(!spec.ascending, "date defaults to newest first");
        spec.sort(&mut v);
        let names: Vec<&str> = v.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["new", "mid", "old"]);
    }

    #[test]
    fn civil_dates_round_trip() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 11, 10), (2026, 12, 31)] {
            assert_eq!(year_month(days_from_civil(y, m, d)), (y, m));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    /// The sections follow time: every later day is in the same section or a later one.
    #[test]
    fn date_sections_run_in_time_order() {
        use std::time::{Duration, UNIX_EPOCH};
        // 2026-11-10 is a Tuesday.
        let days = DayBuckets {
            day: days_from_civil(2026, 11, 10),
            offset: 8 * 3600,
        };
        let day = |n: i64| {
            let secs = n * 86_400 - days.offset + 12 * 3600;
            Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap()))
        };
        let mut last = DateSection::Unknown;
        for n in days.day - 800..=days.day + 2 {
            let section = days.section(day(n));
            assert!(section >= last, "day {n}: {section:?} after {last:?}");
            last = section;
        }
        assert_eq!(days.section(day(days.day)), DateSection::Today);
        assert_eq!(days.section(day(days.day - 3)), DateSection::LastWeek);
        assert_eq!(days.section(None), DateSection::Unknown);
    }

    /// #286: grouped by a date, folders go into their date's section, and the folder block
    /// applies within it: after the files newest first, before them oldest first.
    #[test]
    fn date_sections_take_the_folders_in() {
        use std::time::{Duration, UNIX_EPOCH};
        let days = DayBuckets {
            day: days_from_civil(2026, 11, 10),
            offset: 0,
        };
        let at = |name: &str, kind, ago: u64| {
            let today = u64::try_from(days.day).unwrap() * 86_400 + 12 * 3600;
            Entry::new(
                name,
                kind,
                1,
                Some(UNIX_EPOCH + Duration::from_secs(today - ago * 86_400)),
                0,
            )
        };
        let entries = vec![
            at("old-dir", EntryKind::Dir, 400),
            at("new.txt", EntryKind::File, 0),
            at("old.txt", EntryKind::File, 401),
            at("new-dir", EntryKind::Dir, 0),
        ];
        let order = |spec: SortSpec| {
            let mut v = entries.clone();
            spec.sort(&mut v);
            v.iter().map(|e| e.name.to_string()).collect::<Vec<_>>()
        };
        let newest = SortSpec::by(SortColumn::Modified, false, false);
        let grouped = SortSpec {
            sections: Some(days),
            ..newest
        };
        assert_eq!(order(newest), ["new.txt", "old.txt", "new-dir", "old-dir"]);
        assert_eq!(order(grouped), ["new.txt", "new-dir", "old.txt", "old-dir"]);
        let oldest = SortSpec {
            ascending: true,
            ..grouped
        };
        assert_eq!(order(oldest), ["old-dir", "old.txt", "new-dir", "new.txt"]);
        // The merge in `sort` gives what the comparison does, for many items in both directions.
        let many: Vec<Entry> = (0..500u64)
            .map(|i| {
                let kind = if i % 3 == 0 {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                at(&format!("n{}", i % 17), kind, i * 7 % 900)
            })
            .collect();
        for ascending in [true, false] {
            let spec = SortSpec {
                ascending,
                ..grouped
            };
            let mut merged = many.clone();
            spec.sort(&mut merged);
            let mut compared = many.clone();
            compared.sort_by(|a, b| spec.compare(a, b));
            let names = |v: &[Entry]| {
                v.iter()
                    .map(|e| (e.name.to_string(), e.modified))
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(&merged), names(&compared));
        }
        // By name, or with folders and files together, the sections change nothing.
        for spec in [
            SortSpec {
                sections: Some(days),
                ..SortSpec::default()
            },
            SortSpec {
                mixed: true,
                ..grouped
            },
        ] {
            assert!(spec.date_sections().is_none());
        }
    }

    #[test]
    fn mixed_sorts_folders_with_files() {
        let d = Entry::new("b", EntryKind::Dir, 0, None, 0);
        let f = Entry::new("a", EntryKind::File, 5, None, 0);
        let spec = SortSpec {
            mixed: true,
            ..SortSpec::default()
        };
        assert_eq!(spec.compare(&f, &d), Ordering::Less);
        assert!(spec.toggled(SortColumn::Size).mixed, "keeps mixed");
    }

    #[test]
    fn size_order_uses_computed_folder_sizes() {
        let entries = vec![
            Entry::new("a", EntryKind::Dir, 0, None, 0),
            Entry::new("b", EntryKind::Dir, 0, None, 0),
            Entry::new("c", EntryKind::Dir, 0, None, 0),
            Entry::new("f", EntryKind::File, 500, None, 0),
        ];
        let sizes: HashMap<Arc<str>, u64> =
            [("a".into(), 10), ("b".into(), 1000)].into_iter().collect();
        let names = |spec: SortSpec| -> Vec<String> {
            spec.sorted_with_dir_sizes(&entries, &sizes)
                .iter()
                .map(|e| e.name.to_string())
                .collect()
        };
        let mut spec = SortSpec::by(SortColumn::Size, false, false);
        // Descending: folders last (the uncomputed one, c, counts as 0).
        assert_eq!(names(spec), ["f", "b", "a", "c"]);
        spec.ascending = true;
        assert_eq!(names(spec), ["c", "a", "b", "f"]);
        // Mixed: folders and files compared together.
        spec.mixed = true;
        assert_eq!(names(spec), ["c", "a", "f", "b"]);
        // Other columns are unaffected.
        spec.column = SortColumn::Name;
        assert_eq!(names(spec), ["a", "b", "c", "f"]);
    }

    #[test]
    fn descending_reverses_within_group() {
        let a = Entry::new("a", EntryKind::File, 1, None, 0);
        let b = Entry::new("b", EntryKind::File, 1, None, 0);
        let spec = SortSpec::by(SortColumn::Name, false, false);
        assert_eq!(spec.compare(&a, &b), Ordering::Greater);
    }

    fn names(v: &[Entry]) -> Vec<&str> {
        v.iter().map(|e| &*e.name).collect()
    }

    /// Plain click: one key (flipping the primary); Shift+click adds, flips and caps at three.
    #[test]
    fn shift_click_builds_up_to_three_keys() {
        let spec = SortSpec::default().toggled(SortColumn::Type);
        assert_eq!(spec.key_count(), 1);
        let spec = spec.shift_toggled(SortColumn::Size);
        assert_eq!(spec.key_count(), 2);
        assert_eq!(
            spec.key_of(SortColumn::Size),
            Some((1, false)),
            "size starts largest first"
        );
        // Shift+click on a key that's already there flips it, wherever it is.
        let spec = spec.shift_toggled(SortColumn::Size);
        assert_eq!(spec.key_of(SortColumn::Size), Some((1, true)));
        let flipped = spec.shift_toggled(SortColumn::Type);
        assert_eq!(flipped.key_of(SortColumn::Type), Some((0, false)));
        assert_eq!(flipped.key_count(), 2, "flipping keeps the other keys");
        let spec = spec.shift_toggled(SortColumn::Modified);
        assert_eq!(spec.key_count(), 3);
        assert_eq!(spec.key_of(SortColumn::Modified), Some((2, false)));
        // A fourth column replaces the last key.
        let spec = spec.shift_toggled(SortColumn::Name);
        assert_eq!(spec.key_count(), 3);
        assert_eq!(spec.key_of(SortColumn::Modified), None);
        assert_eq!(spec.key_of(SortColumn::Name), Some((2, true)));
        // A plain click goes back to a single key; on the primary it also flips.
        let single = spec.toggled(SortColumn::Type);
        assert_eq!(single, SortSpec::by(SortColumn::Type, false, false));
        assert_eq!(spec.toggled(SortColumn::Size).key_count(), 1);
        assert!(spec.uses(SortColumn::Size) && !single.uses(SortColumn::Size));
    }

    /// Later keys only decide ties of the earlier ones; name breaks the rest, in the primary's
    /// direction.
    #[test]
    fn secondary_keys_break_ties_in_their_own_direction() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |name: &str, size: u64, secs: u64| {
            Entry::new(
                name,
                EntryKind::File,
                size,
                Some(UNIX_EPOCH + Duration::from_secs(secs)),
                0,
            )
        };
        let mut v = vec![
            at("b.txt", 5, 1),
            at("a.txt", 5, 1),
            at("c.jpg", 9, 3),
            at("d.txt", 9, 2),
            at("e.jpg", 1, 3),
        ];
        // Type A-Z, then size largest first, then modified newest first.
        let spec = SortSpec::by(SortColumn::Type, true, false)
            .shift_toggled(SortColumn::Size)
            .shift_toggled(SortColumn::Modified);
        spec.sort(&mut v);
        assert_eq!(names(&v), ["c.jpg", "e.jpg", "d.txt", "a.txt", "b.txt"]);
        // Primary descending: the name tiebreak follows it.
        let spec = SortSpec::by(SortColumn::Type, false, false).shift_toggled(SortColumn::Size);
        spec.sort(&mut v);
        assert_eq!(names(&v), ["d.txt", "b.txt", "a.txt", "c.jpg", "e.jpg"]);
        // By Type folders lead even descending, whatever the other keys.
        v.push(Entry::new("zz", EntryKind::Dir, 0, None, 0));
        spec.sort(&mut v);
        assert_eq!(v[0].name.as_ref(), "zz");
        // By Size descending they trail.
        SortSpec::by(SortColumn::Size, false, false)
            .shift_toggled(SortColumn::Modified)
            .sort(&mut v);
        assert_eq!(v.last().map(|e| e.name.as_ref()), Some("zz"));
        let spec = SortSpec::by(SortColumn::Type, true, false).shift_toggled(SortColumn::Size);
        spec.sort(&mut v);
        assert_eq!(v[0].name.as_ref(), "zz");
    }

    /// A secondary size key uses the computed folder sizes too.
    #[test]
    fn secondary_size_key_uses_folder_sizes() {
        let entries = vec![
            Entry::new("x.a", EntryKind::Dir, 0, None, 0),
            Entry::new("y.a", EntryKind::Dir, 0, None, 0),
        ];
        let sizes: HashMap<Arc<str>, u64> = [("y.a".into(), 7)].into_iter().collect();
        let spec = SortSpec::by(SortColumn::Extension, true, false).shift_toggled(SortColumn::Size);
        let sorted = spec.sorted_with_dir_sizes(&entries, &sizes);
        assert_eq!(names(&sorted), ["y.a", "x.a"]);
    }

    #[test]
    fn sorts_by_extension_and_attributes() {
        let e = |name: &str, attrs: u32| Entry::new(name, EntryKind::File, 1, None, attrs);
        let mut v = vec![
            e("b.zip", 0x20),
            e("a.TXT", 0x21),
            e("c", 0x2),
            e("d.txt", 0),
        ];
        SortSpec::default()
            .toggled(SortColumn::Extension)
            .sort(&mut v);
        assert_eq!(names(&v), ["c", "a.TXT", "d.txt", "b.zip"]);
        // Letters: "" < "A" < "H" < "RA".
        SortSpec::default()
            .toggled(SortColumn::Attributes)
            .sort(&mut v);
        assert_eq!(names(&v), ["d.txt", "b.zip", "c", "a.TXT"]);
    }

    /// The Type column sorts by the type text shown, like File Explorer, not by extension: `.log`
    /// files are "Text Document" and so come after "Markdown Document", and same-text types tie on
    /// name.
    #[test]
    fn type_sorts_by_the_shown_type_text() {
        rust_i18n::set_locale("en");
        let f = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        let mut v = vec![
            f("z.log"),
            f("a.txt"),
            f("b.md"),
            f("c.dwg"),
            f("d.bak"),
            f("noext"),
            Entry::new("dir", EntryKind::Dir, 0, None, 0),
        ];
        SortSpec::by(SortColumn::Type, true, false).sort(&mut v);
        assert_eq!(
            names(&v),
            ["dir", "d.bak", "c.dwg", "noext", "b.md", "a.txt", "z.log"]
        );
        // Descending, folders still lead (File Explorer does the same by Type).
        SortSpec::by(SortColumn::Type, false, false).sort(&mut v);
        assert_eq!(
            names(&v),
            ["dir", "z.log", "a.txt", "b.md", "noext", "c.dwg", "d.bak"]
        );
        // The Extension column still sorts by the extension itself.
        SortSpec::by(SortColumn::Extension, true, false).sort(&mut v);
        assert_eq!(
            names(&v),
            ["dir", "noext", "d.bak", "c.dwg", "z.log", "b.md", "a.txt"]
        );
    }

    /// A sort takes the type texts once: names arriving meanwhile don't change its answers.
    #[test]
    fn type_texts_are_frozen_per_sort() {
        rust_i18n::set_locale("en");
        let f = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        let v = vec![f("b.bak"), f("a.dwg")];
        let spec = SortSpec::by(SortColumn::Type, true, false);
        let types = TypeKeys::for_entries(&spec, &v);
        crate::format::set_type_name("dwg", Some("AutoCAD Drawing"));
        assert_eq!(spec.compare_with(&v[0], &v[1], &types), Ordering::Less);
        assert_eq!(spec.compare(&v[0], &v[1]), Ordering::Greater, "fresh texts");
    }

    /// Windows' names for unknown types arrive on their own thread while sorts run, as fast as
    /// can be: every sort still gets a total order (a sort whose comparison changes its mind
    /// panics or scrambles), and once the names are in, the order follows them.
    #[test]
    fn sorting_while_type_names_arrive_is_a_total_order() {
        rust_i18n::set_locale("en");
        crate::format::start_type_names(|ext| Some(format!("Kind {ext}")), || {});
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let base: Vec<Entry> = (0..100_000)
            .map(|i| {
                Entry::new(
                    format!("file{i}.x{:03}", next() % 400),
                    EntryKind::File,
                    1,
                    None,
                    0,
                )
            })
            .collect();
        let spec = SortSpec::by(SortColumn::Type, true, false);
        for _ in 0..4 {
            let mut v = base.clone();
            spec.sort(&mut v);
        }
        // Wait until every name is in, then the order must follow them.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while (0..400).any(|n| {
            let probe = Entry::new(format!("f.x{n:03}"), EntryKind::File, 1, None, 0);
            crate::format::type_label(&probe) != format!("Kind x{n:03}")
        }) {
            assert!(std::time::Instant::now() < deadline, "names never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut v = base;
        spec.sort(&mut v);
        let types = TypeKeys::for_entries(&spec, &v);
        for w in v.windows(2) {
            assert_ne!(spec.compare_with(&w[0], &w[1], &types), Ordering::Greater);
        }
    }

    /// Sorting uses exactly the text shown: once Windows' name for a type arrives, the order follows
    /// it ("AutoCAD Drawing" before "BAK File"); until then the built-in "DWG File" is used.
    #[test]
    fn type_sort_follows_system_type_names() {
        rust_i18n::set_locale("en");
        let f = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        let mut v = vec![f("b.bak"), f("a.dwg")];
        let spec = SortSpec::by(SortColumn::Type, true, false);
        spec.sort(&mut v);
        assert_eq!(names(&v), ["b.bak", "a.dwg"], "BAK File, DWG File");
        crate::format::set_type_name("dwg", Some("AutoCAD Drawing"));
        spec.sort(&mut v);
        assert_eq!(names(&v), ["a.dwg", "b.bak"], "AutoCAD Drawing, BAK File");
        // A name that is only a ProgID doesn't count.
        crate::format::set_type_name("dwg", Some("AutoCAD.Drawing.24"));
        spec.sort(&mut v);
        assert_eq!(names(&v), ["b.bak", "a.dwg"]);
    }

    /// The type texts are cached per extension, but a language change gives new ones.
    #[test]
    fn type_order_follows_the_language() {
        let f = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        let mut v = vec![f("a.zzz"), f("b.txt")];
        let spec = SortSpec::by(SortColumn::Type, true, false);
        rust_i18n::set_locale("en");
        spec.sort(&mut v);
        assert_eq!(names(&v), ["b.txt", "a.zzz"], "Text Document, ZZZ File");
        rust_i18n::set_locale("zh-TW");
        spec.sort(&mut v);
        assert_eq!(names(&v), ["a.zzz", "b.txt"], "Latin text before CJK");
        rust_i18n::set_locale("en");
    }

    /// Type texts go in File Explorer's order, as names do (#331): by Windows' sort keys, not by
    /// code point. "éclair" comes before "zeta" in every language (by code point it came after),
    /// and in Simplified Chinese File Explorer puts 配置设置 (pèi) before 应用程序 (yìng), with
    /// Latin text first.
    #[test]
    fn type_order_is_file_explorers() {
        if !crate::collate::available() {
            return;
        }
        rust_i18n::set_locale("en");
        let f = |name: &str| Entry::new(name, EntryKind::File, 1, None, 0);
        crate::format::set_type_name("t331z", Some("Zeta File"));
        crate::format::set_type_name("t331e", Some("Éclair File"));
        let mut v = vec![f("a.t331z"), f("b.t331e")];
        let spec = SortSpec::by(SortColumn::Type, true, false);
        spec.sort(&mut v);
        assert_eq!(names(&v), ["b.t331e", "a.t331z"]);

        crate::format::set_type_name("t331x", Some("应用程序"));
        crate::format::set_type_name("t331i", Some("配置设置"));
        crate::format::set_type_name("t331d", Some("DWG 文件"));
        let mut v = vec![f("a.t331x"), f("b.t331i"), f("c.t331d")];
        spec.sort(&mut v);
        // The order is the user's language's: the same as the keys'.
        let key = |e: &Entry| crate::collate::key(&crate::format::type_label(e).to_lowercase());
        for w in v.windows(2) {
            assert!(key(&w[0]) <= key(&w[1]), "{:?}", names(&v));
        }
        assert_eq!(names(&v)[0], "c.t331d", "Latin text first");
        // Simplified Chinese, as on hzhbest's machine.
        let zh = |t: &str| crate::collate::key_for("zh-CN", t).unwrap();
        assert!(zh("配置设置") < zh("应用程序"));
        assert!(zh("dwg 文件") < zh("配置设置"));
    }

    /// Three keys over 100k entries: a total order (every neighbor pair in order), and quick.
    #[test]
    fn three_keys_sort_100k_entries() {
        const EXTS: [&str; 5] = ["txt", "jpg", "rs", "md", ""];
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut v: Vec<Entry> = (0..100_000)
            .map(|i| {
                let ext = EXTS[(next() % 5) as usize];
                let name = if ext.is_empty() {
                    format!("file{i}")
                } else {
                    format!("file{i}.{ext}")
                };
                let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(next() % 50);
                Entry::new(name, EntryKind::File, next() % 100, Some(modified), 0)
            })
            .collect();
        let spec = SortSpec::by(SortColumn::Type, true, false)
            .shift_toggled(SortColumn::Size)
            .shift_toggled(SortColumn::Modified);
        let started = std::time::Instant::now();
        spec.sort(&mut v);
        let took = started.elapsed();
        for w in v.windows(2) {
            assert_ne!(spec.compare(&w[0], &w[1]), Ordering::Greater);
        }
        // Generous even for an unoptimized test build; release sorts this in a few ms.
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    }

    fn hit(rel: &str, size: u64, secs: Option<u64>) -> Arc<SearchHit> {
        let name = rel.rsplit(['\\', '/']).next().unwrap_or(rel);
        let modified = secs.map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
        Arc::new(SearchHit {
            rel: Arc::from(rel),
            entry: Entry::new(name, EntryKind::File, size, modified, 0),
            snippet: None,
        })
    }

    fn rels(hits: &[Arc<SearchHit>]) -> Vec<&str> {
        hits.iter().map(|h| &*h.rel).collect()
    }

    #[test]
    fn search_results_sort_by_each_column_and_keep_ties_in_relevance_order() {
        // In relevance order.
        let hits = vec![
            hit(r"b\file10.txt", 5, Some(30)),
            hit(r"a\file2.txt", 9, Some(10)),
            hit(r"file1.txt", 5, None),
            hit(r"a\sub\x.txt", 1, Some(20)),
        ];
        let by = |column, ascending| sort_search_hits(&hits, SearchSort { column, ascending });
        let name = by(SearchColumn::Name, true);
        assert_eq!(
            rels(&name),
            [
                r"file1.txt",
                r"a\file2.txt",
                r"b\file10.txt",
                r"a\sub\x.txt"
            ]
        );
        let name = by(SearchColumn::Name, false);
        assert_eq!(rels(&name)[0], r"a\sub\x.txt");
        // Files in no folder come first; "a" before "a\sub" before "b".
        let place = by(SearchColumn::Location, true);
        assert_eq!(
            rels(&place),
            [
                r"file1.txt",
                r"a\file2.txt",
                r"a\sub\x.txt",
                r"b\file10.txt"
            ]
        );
        // Unknown dates are the oldest.
        let date = by(SearchColumn::Modified, false);
        assert_eq!(
            rels(&date),
            [
                r"b\file10.txt",
                r"a\sub\x.txt",
                r"a\file2.txt",
                r"file1.txt"
            ]
        );
        // The two 5-byte files tie: relevance order (file10 first) in both directions.
        let small = by(SearchColumn::Size, true);
        assert_eq!(
            rels(&small),
            [
                r"a\sub\x.txt",
                r"b\file10.txt",
                r"file1.txt",
                r"a\file2.txt"
            ]
        );
        let big = by(SearchColumn::Size, false);
        assert_eq!(
            rels(&big),
            [
                r"a\file2.txt",
                r"b\file10.txt",
                r"file1.txt",
                r"a\sub\x.txt"
            ]
        );
    }

    #[test]
    fn search_header_clicks_go_first_direction_then_back_then_relevance() {
        use SearchColumn::{Modified, Name};
        let first = SearchSort::after_click(None, Name);
        assert_eq!(first, Some(SearchSort::new(Name)));
        assert!(first.is_some_and(|s| s.ascending));
        let second = SearchSort::after_click(first, Name);
        assert_eq!(second.map(|s| s.ascending), Some(false));
        assert_eq!(SearchSort::after_click(second, Name), None);
        // Dates start newest first.
        let date = SearchSort::after_click(first, Modified);
        assert_eq!(
            date.map(|s| (s.column, s.ascending)),
            Some((Modified, false))
        );
        assert_eq!(
            SearchSort::after_click(date, Modified).map(|s| s.ascending),
            Some(true)
        );
    }
}
