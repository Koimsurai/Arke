//! UI-side directory state ([`DirModel`]) and incremental updates ([`Delta`]).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::entry::Entry;
use crate::service::Gen;
use crate::sort::{SortSpec, TypeKeys};

/// Current directory state held by the UI. `entries` is sorted by `sort` with hidden files
/// filtered.
#[derive(Debug)]
pub struct DirModel {
    /// Directory being shown.
    pub path: PathBuf,
    /// Listing generation this model belongs to; events from other generations are stale.
    pub generation: Gen,
    /// Visible entries (hidden ones already filtered), sorted by `sorted_by`.
    pub entries: Arc<Vec<Entry>>,
    /// Actual order of `entries`; may differ from the chosen sort until a background re-sort
    /// finishes.
    pub sorted_by: SortSpec,
    /// Listing finished. While false, `entries` is a partial streaming snapshot.
    pub complete: bool,
    /// Bumped whenever `entries` changes; used to drop stale background sort results.
    pub version: u64,
    pending_deltas: Vec<Delta>,
}

/// Changes to a listed directory found by watching.
#[derive(Debug, Clone)]
pub struct Delta {
    /// New or changed entries; replace any entry with the same name.
    pub upserts: Vec<Entry>,
    /// Names of entries that disappeared.
    pub removed: Vec<Arc<str>>,
}

impl Delta {
    /// Total number of upserts and removals.
    pub const fn len(&self) -> usize {
        self.upserts.len() + self.removed.len()
    }

    /// No changes at all.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Result of applying a [`Delta`] or snapshot to a [`DirModel`].
#[derive(Debug, PartialEq, Eq)]
pub enum DeltaOutcome {
    /// Applied in place.
    Applied,
    /// Listing not finished yet; queued.
    Queued,
    /// Too many changes to apply on the UI thread; rescan in the background instead.
    NeedsRescan,
}

/// Cost limit (roughly number of comparisons) for applying deltas on the UI thread.
/// Beyond it we rescan in the background so one frame never spends more than ~1ms here.
const MAX_INLINE_DELTA_COST: usize = 2_000_000;

impl DirModel {
    /// Empty, incomplete model for `path`; the first snapshot fills it.
    pub fn new(path: PathBuf, generation: Gen, sorted_by: SortSpec) -> Self {
        Self {
            path,
            generation,
            entries: Arc::new(Vec::new()),
            sorted_by,
            complete: false,
            version: 0,
            pending_deltas: Vec::new(),
        }
    }

    /// Receives a listing snapshot. Applies queued deltas once the listing is complete.
    pub fn set_snapshot(
        &mut self,
        entries: Arc<Vec<Entry>>,
        sorted_by: SortSpec,
        complete: bool,
    ) -> DeltaOutcome {
        self.entries = entries;
        self.sorted_by = sorted_by;
        self.complete = complete;
        self.version += 1;
        if complete && !self.pending_deltas.is_empty() {
            let pending = std::mem::take(&mut self.pending_deltas);
            for delta in pending {
                if self.apply_delta(delta) == DeltaOutcome::NeedsRescan {
                    return DeltaOutcome::NeedsRescan;
                }
            }
        }
        DeltaOutcome::Applied
    }

    /// Whether [`Self::apply_delta`] would turn `delta` down as too much work for the UI thread
    /// (a large folder). [`merged`] can apply it on a background thread instead.
    pub fn delta_too_big(&self, delta: &Delta) -> bool {
        self.complete
            && delta.len().saturating_mul(self.entries.len().max(1)) > MAX_INLINE_DELTA_COST
    }

    /// Applies `delta` in sort order, or queues it while the listing is incomplete.
    pub fn apply_delta(&mut self, delta: Delta) -> DeltaOutcome {
        if !self.complete {
            self.pending_deltas.push(delta);
            return DeltaOutcome::Queued;
        }
        if self.delta_too_big(&delta) {
            return DeltaOutcome::NeedsRescan;
        }
        let spec = self.sorted_by;
        self.version += 1;
        let entries = Arc::make_mut(&mut self.entries);
        // One text per type for the whole delta (they can change meanwhile).
        let types = TypeKeys::for_entries(&spec, entries.iter().chain(&delta.upserts));
        for name in &delta.removed {
            if let Some(ix) = entries.iter().position(|e| *e.name == **name) {
                entries.remove(ix);
            }
        }
        for entry in delta.upserts {
            if let Some(ix) = entries.iter().position(|e| e.name == entry.name) {
                entries.remove(ix);
            }
            let ix = entries
                .binary_search_by(|probe| spec.compare_with(probe, &entry, &types))
                .unwrap_or_else(|ix| ix);
            entries.insert(ix, entry);
        }
        DeltaOutcome::Applied
    }

    /// Swaps in [`merged`] entries computed from this model at `version`. Returns false (and
    /// changes nothing) if the entries changed since; the caller relists then.
    pub fn replace_merged(&mut self, version: u64, entries: Vec<Entry>) -> bool {
        if !self.complete || self.version != version {
            return false;
        }
        self.entries = Arc::new(entries);
        self.version += 1;
        true
    }

    /// Swaps in the result of a background re-sort (same contents, new order).
    pub fn replace_sorted(&mut self, entries: Arc<Vec<Entry>>, sorted_by: SortSpec) {
        self.entries = entries;
        self.sorted_by = sorted_by;
        self.version += 1;
    }

    /// Position of the entry named `name` in `entries` (exact, case-sensitive match).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| &*e.name == name)
    }
}

/// `entries` (sorted by `spec`) with `delta` applied, in one pass: O(n + k log k) for k changes
/// where [`DirModel::apply_delta`] costs O(n·k). Same result as applying it there. Runs on a
/// background thread for large folders.
pub fn merged(entries: &[Entry], delta: &Delta, spec: SortSpec) -> Vec<Entry> {
    // The last upsert of a name wins, as when applying them one after another.
    let mut upserts: HashMap<&str, &Entry> = HashMap::with_capacity(delta.upserts.len());
    for entry in &delta.upserts {
        upserts.insert(&entry.name, entry);
    }
    let gone: HashSet<&str> = delta
        .removed
        .iter()
        .map(|n| &**n)
        .chain(upserts.keys().copied())
        .collect();
    let types = TypeKeys::for_entries(&spec, entries.iter().chain(&delta.upserts));
    let mut added: Vec<&Entry> = upserts.into_values().collect();
    added.sort_unstable_by(|a, b| spec.compare_with(a, b, &types));
    let mut out = Vec::with_capacity(entries.len() + added.len());
    let mut added = added.into_iter().peekable();
    for entry in entries.iter().filter(|e| !gone.contains(&*e.name)) {
        while let Some(new) = added.next_if(|new| spec.compare_with(new, entry, &types).is_lt()) {
            out.push(new.clone());
        }
        out.push(entry.clone());
    }
    out.extend(added.cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn file(name: &str) -> Entry {
        Entry::new(name, EntryKind::File, 0, None, 0)
    }

    #[test]
    fn delta_keeps_order_and_queues_until_complete() {
        let spec = SortSpec::default();
        let mut m = DirModel::new("x".into(), 1, spec);
        let d = Delta {
            upserts: vec![file("b")],
            removed: vec!["c".into()],
        };
        assert_eq!(m.apply_delta(d), DeltaOutcome::Queued);
        let mut v = vec![file("a"), file("c"), file("d")];
        spec.sort(&mut v);
        m.set_snapshot(Arc::new(v), spec, true);
        let names: Vec<_> = m.entries.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, ["a", "b", "d"]);
        // Upserting an existing name replaces it instead of duplicating.
        m.apply_delta(Delta {
            upserts: vec![file("a")],
            removed: vec![],
        });
        assert_eq!(m.entries.len(), 3);
    }

    /// The one-pass merge gives the same list as applying changes one by one.
    #[test]
    fn merge_matches_applying_in_place() {
        let spec = SortSpec::default();
        let mut base: Vec<Entry> = (0..200).map(|i| file(&format!("f{i:03}"))).collect();
        base.push(Entry::new("dir", EntryKind::Dir, 0, None, 0));
        spec.sort(&mut base);
        let delta = Delta {
            upserts: vec![
                file("f050"),
                file("new-a"),
                Entry::new("new-dir", EntryKind::Dir, 0, None, 0),
                file("f199"),
                file("new-a"),
                file("A-first"),
                file("zzz"),
            ],
            removed: vec![
                "f000".into(),
                "f100".into(),
                "missing".into(),
                "f199".into(),
            ],
        };
        let mut model = DirModel::new("x".into(), 1, spec);
        model.set_snapshot(Arc::new(base.clone()), spec, true);
        assert_eq!(model.apply_delta(delta.clone()), DeltaOutcome::Applied);
        let names = |v: &[Entry]| v.iter().map(|e| e.name.to_string()).collect::<Vec<_>>();
        let merged = merged(&base, &delta, spec);
        assert_eq!(names(&merged), names(&model.entries));
        assert_eq!(merged.len(), 201 - 3 + 5);

        // Swapping it in only works on the version it was computed from.
        let version = model.version;
        assert!(model.replace_merged(version, merged.clone()));
        assert!(!model.replace_merged(version, merged));
    }

    #[test]
    fn large_folders_hand_big_deltas_to_the_background() {
        let spec = SortSpec::default();
        let entries: Vec<Entry> = (0..100_000).map(|i| file(&format!("f{i:06}"))).collect();
        let mut model = DirModel::new("x".into(), 1, spec);
        model.set_snapshot(Arc::new(entries), spec, true);
        let small = Delta {
            upserts: vec![file("a")],
            removed: vec![],
        };
        let big = Delta {
            upserts: (0..50).map(|i| file(&format!("n{i}"))).collect(),
            removed: vec![],
        };
        assert!(!model.delta_too_big(&small));
        assert!(model.delta_too_big(&big));
    }
}
