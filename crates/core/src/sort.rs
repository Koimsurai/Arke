//! Sorting of directory listings (natural name order, folders first).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::entry::Entry;

/// Column a listing is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum SortColumn {
    /// Name in natural order ("file2" before "file10"), case-insensitive.
    #[default]
    Name,
    /// Last write time.
    Modified,
    /// Extension.
    Type,
    /// Size in bytes (folders use computed sizes where available).
    Size,
    /// Creation time.
    Created,
    /// Extension on its own column (same order as `Type`, its own column and groups).
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
    /// Ascending order; folders stay first either way.
    pub ascending: bool,
    /// Sort folders and files together (default `false`: folders always first).
    pub mixed: bool,
    /// Secondary and tertiary keys, compared in order when the ones before tie. Filled from the
    /// front: a `None` is never followed by a key. A fixed array keeps the spec `Copy` and the
    /// comparison free of allocation.
    pub then: [Option<SortKey>; MAX_SORT_KEYS - 1],
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
        }
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

    /// Same keys and directions (where folders go aside).
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

    /// Full comparison: folders first (regardless of direction; not with `mixed`), then each key in
    /// turn, then name (in the primary direction) as a tiebreaker so the order is total.
    pub fn compare(&self, a: &Entry, b: &Entry) -> Ordering {
        self.compare_sized(a, b, a.size, b.size)
    }

    /// Like `compare`, but the caller supplies sizes (folder sizes are computed separately, not in
    /// `Entry`).
    fn compare_sized(&self, a: &Entry, b: &Entry, size_a: u64, size_b: u64) -> Ordering {
        if !self.mixed {
            match (a.is_dir_like(), b.is_dir_like()) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
        }
        let ord = column_cmp(self.column, a, b, size_a, size_b);
        if ord != Ordering::Equal {
            return if self.ascending { ord } else { ord.reverse() };
        }
        for key in self.then.iter().flatten() {
            let ord = column_cmp(key.column, a, b, size_a, size_b);
            if ord != Ordering::Equal {
                return if key.ascending { ord } else { ord.reverse() };
            }
        }
        let ord = name_cmp(a, b);
        if self.ascending { ord } else { ord.reverse() }
    }

    /// Sorts `entries` in place by [`Self::compare`].
    pub fn sort(&self, entries: &mut [Entry]) {
        // Unstable sort is faster, and the comparator is a total order, so the result is
        // deterministic.
        entries.sort_unstable_by(|a, b| self.compare(a, b));
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
        sized.sort_unstable_by(|a, b| self.compare_sized(a.1, b.1, a.0, b.0));
        sized.into_iter().map(|(_, e)| e.clone()).collect()
    }
}

/// `const` column equality (derived `PartialEq` isn't `const`).
const fn matches_column(a: SortColumn, b: SortColumn) -> bool {
    a as u8 == b as u8
}

/// One column's order for two entries (ascending), `Equal` when they tie on it.
fn column_cmp(column: SortColumn, a: &Entry, b: &Entry, size_a: u64, size_b: u64) -> Ordering {
    match column {
        // Name is the final tiebreaker too; as a key it decides on its own.
        SortColumn::Name => name_cmp(a, b),
        SortColumn::Modified => a.modified.cmp(&b.modified),
        SortColumn::Type | SortColumn::Extension => a.extension().cmp(b.extension()),
        SortColumn::Size => size_a.cmp(&size_b),
        SortColumn::Created => a.created.cmp(&b.created),
        SortColumn::Attributes => a.attribute_letters().cmp(b.attribute_letters()),
    }
}

/// Natural, case-insensitive name order, then the exact name so the order is total.
fn name_cmp(a: &Entry, b: &Entry) -> Ordering {
    natural_cmp(&a.sort_key, &b.sort_key).then_with(|| a.name.cmp(&b.name))
}

/// Natural sort: digit runs compare numerically (file2 < file10), everything else per char.
/// Inputs must already be lowercase.
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
                return ca.cmp(&cb);
            }
            i += 1;
            j += 1;
        }
    }
    (ab.len() - i).cmp(&(bb.len() - j))
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

    #[test]
    fn dirs_first_in_both_directions() {
        let d = Entry::new("zzz", EntryKind::Dir, 0, None, 0);
        let f = Entry::new("aaa", EntryKind::File, 5, None, 0);
        for ascending in [true, false] {
            let spec = SortSpec::by(SortColumn::Name, ascending, false);
            assert_eq!(spec.compare(&d, &f), Ordering::Less);
        }
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
        // Folders still first; the uncomputed one (c) counts as 0.
        assert_eq!(names(spec), ["b", "a", "c", "f"]);
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
        // Folders stay first whatever the keys.
        v.push(Entry::new("zz", EntryKind::Dir, 0, None, 0));
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
}
