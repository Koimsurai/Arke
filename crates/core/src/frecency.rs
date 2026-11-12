//! Frecency: how often and how recently the user opened a file or visited a folder, so search can
//! put the copy they actually use first among same-name results.
//!
//! Each item keeps a weight and the time of its last visit. A visit adds 1 to the weight after
//! decaying it with a half-life of [`HALF_LIFE`], so ten visits last month count less than three
//! this week. Items whose weight has faded below [`FORGET_BELOW`] are dropped, and the store keeps
//! at most [`MAX_ITEMS`] (the weakest go first), so the saved file stays small.
//!
//! Pure data and arithmetic; the app records visits, saves the file and passes a
//! [`FrecencySnapshot`] to the ranking (`crate::rank`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A visit counts half after this long.
pub const HALF_LIFE: u64 = 14 * 24 * 60 * 60;
/// Items kept at most; the weakest are dropped first.
pub const MAX_ITEMS: usize = 500;
/// Weight under which an item is forgotten (a single visit fades below it after about two months).
pub const FORGET_BELOW: f64 = 0.05;
/// Visits to the same item closer together than this count once (back and forward, a refresh,
/// opening a file twice in a row).
pub const REPEAT_GAP: u64 = 30;

/// One remembered item.
#[derive(Clone, Debug, PartialEq)]
pub struct Visit {
    /// The path as it was opened.
    pub path: PathBuf,
    /// A folder (visited) rather than a file (opened).
    pub is_dir: bool,
    /// Weight as of `last` (decays from there).
    pub weight: f64,
    /// Last visit, Unix seconds.
    pub last: u64,
}

impl Visit {
    /// Weight at `now`.
    #[must_use]
    pub fn value(&self, now: u64) -> f64 {
        decay(self.weight, now.saturating_sub(self.last))
    }
}

/// `weight` after `elapsed` seconds.
#[must_use]
pub fn decay(weight: f64, elapsed: u64) -> f64 {
    // Seconds fit an f64 exactly far beyond any real elapsed time.
    #[allow(clippy::cast_precision_loss)]
    let half_lives = elapsed as f64 / HALF_LIFE as f64;
    weight * 0.5f64.powf(half_lives)
}

/// Comparison key for a path: lowercase, backslashes, no `\\?\` prefix or trailing separator
/// (Windows paths are case-insensitive). String work only.
#[must_use]
pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    };
    let mut key = text.to_lowercase();
    // Keep the separator of a drive root (`c:\`).
    while key.len() > 3 && key.ends_with('\\') {
        key.pop();
    }
    key
}

/// Visits, keyed by [`path_key`].
#[derive(Clone, Debug)]
pub struct Frecency {
    items: HashMap<String, Visit>,
    cap: usize,
}

impl Default for Frecency {
    fn default() -> Self {
        Self::with_cap(MAX_ITEMS)
    }
}

impl Frecency {
    /// Empty store keeping at most `cap` items.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            items: HashMap::new(),
            cap: cap.max(1),
        }
    }

    /// Store from saved visits: faded ones are dropped, duplicates merged, the cap applied.
    #[must_use]
    pub fn from_visits(visits: impl IntoIterator<Item = Visit>, now: u64) -> Self {
        let mut this = Self::default();
        for visit in visits {
            if !visit.weight.is_finite() || visit.weight <= 0.0 {
                continue;
            }
            let key = path_key(&visit.path);
            match this.items.get_mut(&key) {
                // The same path saved twice (another letter case): keep the stronger.
                Some(old) if old.value(now) >= visit.value(now) => {}
                _ => {
                    this.items.insert(key, visit);
                }
            }
        }
        this.prune(now);
        this
    }

    /// Number of items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Nothing remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Records a visit to `path` at `now` (Unix seconds).
    pub fn record(&mut self, path: &Path, is_dir: bool, now: u64) {
        let key = path_key(path);
        if key.is_empty() {
            return;
        }
        let visit = self.items.entry(key).or_insert_with(|| Visit {
            path: path.to_path_buf(),
            is_dir,
            weight: 0.0,
            last: now,
        });
        let repeat = visit.weight > 0.0 && now.saturating_sub(visit.last) < REPEAT_GAP;
        if !repeat {
            visit.weight = visit.value(now) + 1.0;
        }
        visit.last = visit.last.max(now);
        visit.is_dir = is_dir;
        if self.items.len() > self.cap {
            self.prune(now);
        }
    }

    /// Adds the visits in `other` (recorded while this store was still being read from disk).
    pub fn absorb(&mut self, other: &Self, now: u64) {
        for visit in other.items.values() {
            let key = path_key(&visit.path);
            if let Some(old) = self.items.get_mut(&key) {
                old.weight = old.value(now) + visit.value(now);
                old.last = now.max(old.last);
                old.is_dir = visit.is_dir;
            } else {
                self.items.insert(key, visit.clone());
            }
        }
        if self.items.len() > self.cap {
            self.prune(now);
        }
    }

    /// Forgets `path` (deleted or renamed away).
    pub fn forget(&mut self, path: &Path) {
        self.items.remove(&path_key(path));
    }

    /// Forgets everything ("Clear history").
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Weight of `path` at `now` (0 when never visited).
    #[must_use]
    pub fn value(&self, path: &Path, now: u64) -> f64 {
        self.items
            .get(&path_key(path))
            .map_or(0.0, |v| v.value(now))
    }

    /// The visits, in no particular order (for saving).
    pub fn visits(&self) -> impl Iterator<Item = &Visit> {
        self.items.values()
    }

    /// Drops faded items, then the weakest beyond the cap.
    fn prune(&mut self, now: u64) {
        self.items.retain(|_, v| v.value(now) >= FORGET_BELOW);
        if self.items.len() <= self.cap {
            return;
        }
        let mut values: Vec<(f64, String)> = self
            .items
            .iter()
            .map(|(k, v)| (v.value(now), k.clone()))
            .collect();
        values.sort_by(|a, b| a.0.total_cmp(&b.0));
        let extra = self.items.len() - self.cap;
        for (_, key) in values.into_iter().take(extra) {
            self.items.remove(&key);
        }
    }

    /// Values at `now`, for ranking one or more queries without touching the store.
    #[must_use]
    pub fn snapshot(&self, now: u64) -> FrecencySnapshot {
        let mut items: Vec<FrecentItem> = self
            .items
            .values()
            .map(|v| FrecentItem {
                path: v.path.clone(),
                is_dir: v.is_dir,
                // Ranking needs only a rough value.
                #[allow(clippy::cast_possible_truncation)]
                value: v.value(now) as f32,
            })
            .filter(|i| f64::from(i.value) >= FORGET_BELOW)
            .collect();
        items.sort_by(|a, b| b.value.total_cmp(&a.value));
        let values = self
            .items
            .iter()
            .filter_map(|(k, v)| {
                #[allow(clippy::cast_possible_truncation)]
                let value = v.value(now) as f32;
                (f64::from(value) >= FORGET_BELOW).then(|| (k.clone(), value))
            })
            .collect();
        FrecencySnapshot { values, items }
    }
}

/// One item of a [`FrecencySnapshot`].
#[derive(Clone, Debug, PartialEq)]
pub struct FrecentItem {
    /// The path as it was opened.
    pub path: PathBuf,
    /// A folder rather than a file.
    pub is_dir: bool,
    /// Weight at the time of the snapshot.
    pub value: f32,
}

/// Frozen frecency values: O(1) lookups by [`path_key`] for ranking, and the items strongest
/// first for adding visited items a query matches but the index didn't return.
#[derive(Clone, Debug, Default)]
pub struct FrecencySnapshot {
    values: HashMap<String, f32>,
    items: Vec<FrecentItem>,
}

impl FrecencySnapshot {
    /// Value for a [`path_key`] (0 when never visited).
    #[must_use]
    pub fn value_of(&self, key: &str) -> f32 {
        self.values.get(key).copied().unwrap_or(0.0)
    }

    /// Items, strongest first.
    #[must_use]
    pub fn items(&self) -> &[FrecentItem] {
        &self.items
    }

    /// Nothing remembered.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn keys_ignore_case_separators_and_prefixes() {
        assert_eq!(
            path_key(Path::new(r"C:\Users\Me\Doc.TXT")),
            r"c:\users\me\doc.txt"
        );
        assert_eq!(path_key(Path::new("C:/Users/Me/")), r"c:\users\me");
        assert_eq!(path_key(Path::new(r"\\?\C:\x")), r"c:\x");
        assert_eq!(
            path_key(Path::new(r"\\?\UNC\nas\share\a")),
            r"\\nas\share\a"
        );
        assert_eq!(path_key(Path::new(r"C:\")), r"c:\");
    }

    #[test]
    fn weight_halves_every_half_life() {
        assert!((decay(4.0, HALF_LIFE) - 2.0).abs() < 1e-9);
        assert!((decay(4.0, 2 * HALF_LIFE) - 1.0).abs() < 1e-9);
        assert!((decay(4.0, 0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn visits_add_up_and_fade() {
        let mut f = Frecency::default();
        let p = Path::new(r"C:\work\report.docx");
        f.record(p, false, NOW);
        f.record(p, false, NOW + DAY);
        f.record(p, false, NOW + 2 * DAY);
        let fresh = f.value(p, NOW + 2 * DAY);
        assert!(
            fresh > 2.8 && fresh < 3.0,
            "three visits in two days: {fresh}"
        );
        // Another letter case is the same item.
        assert!((f.value(Path::new(r"c:\WORK\Report.docx"), NOW + 2 * DAY) - fresh).abs() < 1e-9);
        let later = f.value(p, NOW + 2 * DAY + HALF_LIFE);
        assert!((later - fresh / 2.0).abs() < 1e-9);
        assert!(f.value(Path::new(r"C:\other"), NOW).abs() < f64::EPSILON);
    }

    #[test]
    fn recent_visits_beat_old_frequent_ones() {
        let mut f = Frecency::default();
        let old = Path::new(r"C:\old.txt");
        let new = Path::new(r"C:\new.txt");
        for day in 0..10 {
            f.record(old, false, NOW + day * DAY);
        }
        let later = NOW + 100 * DAY;
        for hour in 0..3 {
            f.record(new, false, later + hour * 3600);
        }
        assert!(f.value(new, later + DAY) > f.value(old, later + DAY));
    }

    #[test]
    fn quick_repeats_count_once() {
        let mut f = Frecency::default();
        let p = Path::new(r"C:\a");
        f.record(p, true, NOW);
        f.record(p, true, NOW + 5);
        f.record(p, true, NOW + 10);
        assert!((f.value(p, NOW + 10) - 1.0).abs() < 1e-6);
        f.record(p, true, NOW + 10 + REPEAT_GAP);
        assert!(f.value(p, NOW + 10 + REPEAT_GAP) > 1.9);
    }

    #[test]
    fn keeps_the_strongest_within_the_cap() {
        let mut f = Frecency::with_cap(3);
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            let p = PathBuf::from(format!(r"C:\{name}"));
            // "a" is visited most, "d" once and last.
            for k in 0..(4 - i as u64) {
                f.record(&p, false, NOW + i as u64 * 1000 + k * REPEAT_GAP);
            }
        }
        assert_eq!(f.len(), 3);
        assert!(f.value(Path::new(r"C:\a"), NOW + 4000) > 3.9);
        assert!(
            f.value(Path::new(r"C:\d"), NOW + 4000) <= f64::EPSILON,
            "the weakest is dropped"
        );
    }

    #[test]
    fn loading_drops_faded_and_broken_items() {
        let visit = |path: &str, weight: f64, last: u64| Visit {
            path: PathBuf::from(path),
            is_dir: false,
            weight,
            last,
        };
        let f = Frecency::from_visits(
            [
                visit(r"C:\fresh", 2.0, NOW),
                visit(r"C:\faded", 1.0, NOW - 200 * DAY),
                visit(r"C:\nan", f64::NAN, NOW),
                visit(r"C:\FRESH", 1.0, NOW),
            ],
            NOW,
        );
        assert_eq!(f.len(), 1);
        assert!((f.value(Path::new(r"C:\fresh"), NOW) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn absorbing_adds_visits_made_while_loading() {
        let mut saved = Frecency::default();
        saved.record(Path::new(r"C:\a"), true, NOW);
        let mut fresh = Frecency::default();
        fresh.record(Path::new(r"C:\A"), true, NOW + 60);
        fresh.record(Path::new(r"C:\b"), false, NOW + 60);
        saved.absorb(&fresh, NOW + 60);
        assert_eq!(saved.len(), 2);
        assert!(saved.value(Path::new(r"C:\a"), NOW + 60) > 1.99);
        assert!((saved.value(Path::new(r"C:\b"), NOW + 60) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn snapshot_lists_strongest_first() {
        let mut f = Frecency::default();
        f.record(Path::new(r"C:\one"), true, NOW);
        f.record(Path::new(r"C:\two"), false, NOW);
        f.record(Path::new(r"C:\two"), false, NOW + REPEAT_GAP);
        let snap = f.snapshot(NOW + REPEAT_GAP);
        assert_eq!(snap.items()[0].path, Path::new(r"C:\two"));
        assert!(snap.value_of(r"c:\two") > snap.value_of(r"c:\one"));
        assert!(snap.value_of(r"c:\none").abs() < f32::EPSILON);
        f.clear();
        assert!(f.snapshot(NOW).is_empty());
    }
}
