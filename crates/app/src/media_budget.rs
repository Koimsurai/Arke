//! The rules of the decoded-image cache (`media.rs`), kept apart from GPUI so they are tested on
//! their own.
//!
//! - [`Decoded`]: images under a byte budget. Least recently used go first, never a pinned one;
//!   once only pinned ones are left, the cache stays over budget rather than free what is shown.
//! - [`Pins`]: what each view on screen wants. A list asks several times per frame (GPUI lays out
//!   one row to measure it, a grouped view asks per group), so a view's wishes are collected over
//!   the frame and replace its previous ones when the frame ends. A view of a window that drew
//!   without it (its tab switched away or closed, another page shown) stops pinning. Views of
//!   windows that did not draw keep their pins: a window sitting still keeps its pictures.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;

/// Bytes counted for each entry on top of its pixels: the key's path and the map slots. Failed
/// loads are stored too (so they aren't retried), and this keeps them from piling up unbounded.
pub(crate) const ENTRY_BYTES: usize = 256;

struct Slot<V> {
    /// `None`: loading failed.
    value: Option<V>,
    bytes: usize,
    /// When it was last used, the key of [`Decoded::recency`].
    tick: u64,
}

/// Decoded images under a byte budget, least recently used evicted first.
pub(crate) struct Decoded<K, V> {
    slots: HashMap<K, Slot<V>>,
    /// Use order, oldest first.
    recency: BTreeMap<u64, K>,
    tick: u64,
    bytes: usize,
    budget: usize,
}

impl<K: Clone + Eq + Hash, V> Decoded<K, V> {
    /// An empty cache that keeps about `budget` bytes.
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            slots: HashMap::new(),
            recency: BTreeMap::new(),
            tick: 0,
            bytes: 0,
            budget,
        }
    }

    const fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The image, if loaded (not when loading failed: see [`Self::contains`]).
    pub(crate) fn image(&self, key: &K) -> Option<&V> {
        self.slots.get(key).and_then(|slot| slot.value.as_ref())
    }

    /// Loaded, or known to fail.
    pub(crate) fn contains(&self, key: &K) -> bool {
        self.slots.contains_key(key)
    }

    /// Bytes counted: pixels plus [`ENTRY_BYTES`] per entry.
    #[cfg(test)]
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Holds more than its budget (eviction has something to do, unless all of it is pinned).
    pub(crate) const fn over_budget(&self) -> bool {
        self.bytes > self.budget
    }

    /// Entries with an image.
    pub(crate) fn image_count(&self) -> usize {
        self.slots.values().filter(|s| s.value.is_some()).count()
    }

    /// Every entry, with its image (`None`: loading failed).
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, Option<&V>)> {
        self.slots.iter().map(|(k, s)| (k, s.value.as_ref()))
    }

    /// Stores `value` (`None`: loading failed), whose pixels take `pixels` bytes, as the most
    /// recently used. A key that already has an image keeps it: the copy on screen is never
    /// swapped for an equal one, which would upload it again while the GPU atlas kept the old one.
    /// Returns the value not kept, if any.
    pub(crate) fn insert(&mut self, key: K, value: Option<V>, pixels: usize) -> Option<V> {
        let bytes = pixels + ENTRY_BYTES;
        let tick = self.next_tick();
        let existing = self
            .slots
            .get(&key)
            .map(|s| (s.value.is_some(), s.tick, s.bytes));
        match existing {
            Some((had_image, old_tick, _)) if had_image || value.is_none() => {
                self.recency.remove(&old_tick);
                if let Some(slot) = self.slots.get_mut(&key) {
                    slot.tick = tick;
                }
                self.recency.insert(tick, key);
                value
            }
            Some((_, old_tick, old_bytes)) => {
                // It failed before and loads now.
                self.recency.remove(&old_tick);
                self.bytes = self.bytes.saturating_sub(old_bytes) + bytes;
                self.slots.insert(key.clone(), Slot { value, bytes, tick });
                self.recency.insert(tick, key);
                None
            }
            None => {
                self.bytes += bytes;
                self.slots.insert(key.clone(), Slot { value, bytes, tick });
                self.recency.insert(tick, key);
                None
            }
        }
    }

    /// Marks `key` as the most recently used.
    pub(crate) fn touch(&mut self, key: &K) {
        let tick = self.next_tick();
        if let Some(slot) = self.slots.get_mut(key) {
            self.recency.remove(&slot.tick);
            slot.tick = tick;
            self.recency.insert(tick, key.clone());
        }
    }

    /// Frees the least recently used entries that aren't `pinned` until the cache is within its
    /// budget (or only pinned ones are left). Returns the freed images.
    pub(crate) fn evict(&mut self, pinned: impl Fn(&K) -> bool) -> Vec<V> {
        let mut over = self.bytes.saturating_sub(self.budget);
        if over == 0 {
            return Vec::new();
        }
        let mut victims = Vec::new();
        for key in self.recency.values() {
            if over == 0 {
                break;
            }
            if pinned(key) {
                continue;
            }
            over = over.saturating_sub(self.slots.get(key).map_or(0, |s| s.bytes));
            victims.push(key.clone());
        }
        let mut freed = Vec::new();
        for key in victims {
            if let Some(slot) = self.slots.remove(&key) {
                self.recency.remove(&slot.tick);
                self.bytes = self.bytes.saturating_sub(slot.bytes);
                freed.extend(slot.value);
            }
        }
        freed
    }

    /// Frees everything; returns the images.
    pub(crate) fn drain(&mut self) -> Vec<V> {
        let freed: Vec<V> = self.slots.drain().filter_map(|(_, s)| s.value).collect();
        self.slots.shrink_to_fit();
        self.recency.clear();
        self.bytes = 0;
        freed
    }
}

/// What one view wants: the lists it asked for during its last frame, in the order asked.
struct Wishes<K, W> {
    /// The window it drew in last, once known.
    window: Option<W>,
    /// The list (or grid) itself.
    main: Vec<Vec<K>>,
    /// The side column (column view's parent folder), loaded after every main list.
    side: Vec<Vec<K>>,
}

impl<K: PartialEq, W> Wishes<K, W> {
    const fn new() -> Self {
        Self {
            window: None,
            main: Vec::new(),
            side: Vec::new(),
        }
    }

    fn same_lists(&self, other: &Self) -> bool {
        self.main == other.main && self.side == other.side
    }
}

/// What views asked for during the frame under way.
struct Frame<K, C, W> {
    /// Windows that drew.
    windows: Vec<W>,
    /// Views that asked or drew, in that order.
    views: Vec<(C, Wishes<K, W>)>,
}

/// Which images the views on screen want; those are never evicted (see the module docs).
/// `C` names a view, `W` a window.
pub(crate) struct Pins<K, C, W> {
    views: HashMap<C, Wishes<K, W>>,
    /// Views in the order they last drew, latest frame first: the loaders' priority.
    order: Vec<C>,
    frame: Option<Frame<K, C, W>>,
    /// Every key some view wants.
    pinned: HashSet<K>,
}

impl<K, C, W> Default for Pins<K, C, W> {
    fn default() -> Self {
        Self {
            views: HashMap::new(),
            order: Vec::new(),
            frame: None,
            pinned: HashSet::new(),
        }
    }
}

impl<K: Clone + Eq + Hash, C: Copy + Eq + Hash, W: Copy + Eq> Pins<K, C, W> {
    /// A frame is under way: [`Self::end_frame`] is due.
    pub(crate) const fn frame_open(&self) -> bool {
        self.frame.is_some()
    }

    fn frame_view(&mut self, view: C) -> &mut Wishes<K, W> {
        let frame = self.frame.get_or_insert_with(|| Frame {
            windows: Vec::new(),
            views: Vec::new(),
        });
        let ix = if let Some(ix) = frame.views.iter().position(|(c, _)| *c == view) {
            ix
        } else {
            frame.views.push((view, Wishes::new()));
            frame.views.len() - 1
        };
        &mut frame.views[ix].1
    }

    /// `view` draws in `window` this frame: what it asks for until the frame ends replaces what it
    /// wanted before, and asking for nothing unpins all of it.
    pub(crate) fn drawing(&mut self, window: W, view: C) {
        self.frame_view(view).window = Some(window);
        if let Some(frame) = &mut self.frame
            && !frame.windows.contains(&window)
        {
            frame.windows.push(window);
        }
    }

    /// `view` wants `keys` (in priority order) for its main list, or its side column.
    pub(crate) fn want(&mut self, view: C, side: bool, keys: Vec<K>) {
        let wishes = self.frame_view(view);
        if keys.is_empty() {
            return;
        }
        if side {
            wishes.side.push(keys);
        } else {
            wishes.main.push(keys);
        }
    }

    /// Ends the frame: each view that asked or drew replaces its wishes, and views of a window
    /// that drew without them are dropped. Whether what is wanted changed.
    pub(crate) fn end_frame(&mut self) -> bool {
        let Some(frame) = self.frame.take() else {
            return false;
        };
        let mut changed = false;
        let mut drawn = Vec::with_capacity(frame.views.len());
        for (view, mut wishes) in frame.views {
            let (old_window, same) = self
                .views
                .get(&view)
                .map_or((None, false), |old| (old.window, old.same_lists(&wishes)));
            // Asked outside a frame it drew in (tests): still in the window it was.
            if wishes.window.is_none() {
                wishes.window = old_window;
            }
            changed |= !same;
            self.views.insert(view, wishes);
            drawn.push(view);
        }
        let windows = frame.windows;
        let hidden: Vec<C> = self
            .views
            .iter()
            .filter(|(c, wishes)| {
                !drawn.contains(c) && wishes.window.is_some_and(|w| windows.contains(&w))
            })
            .map(|(c, _)| *c)
            .collect();
        for view in &hidden {
            self.views.remove(view);
        }
        changed |= !hidden.is_empty();
        let rest: Vec<C> = self
            .order
            .iter()
            .copied()
            .filter(|c| !drawn.contains(c) && !hidden.contains(c))
            .collect();
        self.order = drawn;
        self.order.extend(rest);
        if changed {
            self.rebuild();
        }
        changed
    }

    fn rebuild(&mut self) {
        self.pinned = self
            .views
            .values()
            .flat_map(|w| w.main.iter().chain(&w.side))
            .flatten()
            .cloned()
            .collect();
    }

    /// Everything wanted, for the loaders, each key once: every view's main lists, the views in
    /// the order they last drew and each view's longest list first (GPUI's one-row measuring
    /// lists go last), then the side columns.
    pub(crate) fn wanted(&self) -> Vec<K> {
        let views: Vec<&Wishes<K, W>> = self
            .order
            .iter()
            .filter_map(|c| self.views.get(c))
            .collect();
        let mut seen: HashSet<&K> = HashSet::with_capacity(self.pinned.len());
        let mut out = Vec::with_capacity(self.pinned.len());
        for side in [false, true] {
            for wishes in &views {
                let lists = if side { &wishes.side } else { &wishes.main };
                let mut lists: Vec<&Vec<K>> = lists.iter().collect();
                lists.sort_by_key(|l| Reverse(l.len()));
                for key in lists.into_iter().flatten() {
                    if seen.insert(key) {
                        out.push(key.clone());
                    }
                }
            }
        }
        out
    }

    /// Some view wants `key`.
    pub(crate) fn is_pinned(&self, key: &K) -> bool {
        self.pinned.contains(key)
    }

    /// Every pinned key.
    pub(crate) fn pinned(&self) -> impl Iterator<Item = &K> {
        self.pinned.iter()
    }

    /// `window` closed: its views pin nothing anymore. Whether anything was unpinned.
    pub(crate) fn forget_window(&mut self, window: W) -> bool {
        let before = self.views.len();
        self.views.retain(|_, w| w.window != Some(window));
        if self.views.len() == before {
            return false;
        }
        let views = &self.views;
        self.order.retain(|c| views.contains_key(c));
        self.rebuild();
        true
    }

    /// Forgets every view (and the frame under way).
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoded, ENTRY_BYTES, Pins};

    const KB: usize = 1024;

    fn cache(budget: usize) -> Decoded<u32, u32> {
        Decoded::new(budget)
    }

    fn keys(cache: &Decoded<u32, u32>) -> Vec<u32> {
        let mut keys: Vec<u32> = cache.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn sizes_count_pixels_plus_the_entry() {
        let mut c = cache(1 << 20);
        c.insert(1, Some(10), 100 * KB);
        c.insert(2, None, 0);
        assert_eq!(c.bytes(), 100 * KB + 2 * ENTRY_BYTES);
        assert_eq!(c.image_count(), 1);
        assert!(
            c.contains(&2) && c.image(&2).is_none(),
            "a failure is remembered"
        );
        assert!(!c.contains(&3));
    }

    #[test]
    fn least_recently_used_go_first_and_only_as_many_as_needed() {
        let mut c = cache(3 * (KB + ENTRY_BYTES));
        for k in 1..=3 {
            c.insert(k, Some(k), KB);
        }
        assert!(c.evict(|_| false).is_empty(), "within the budget");
        c.touch(&1);
        c.insert(4, Some(4), KB);
        assert!(c.over_budget());
        assert_eq!(c.evict(|_| false), [2], "2 is now the oldest");
        assert_eq!(keys(&c), [1, 3, 4]);
        assert!(!c.over_budget());
        c.insert(5, Some(5), 2 * KB);
        assert_eq!(c.evict(|_| false), [3, 1], "oldest first, until it fits");
        assert_eq!(keys(&c), [4, 5]);
    }

    #[test]
    fn pinned_entries_stay_even_over_budget() {
        let mut c = cache(2 * (KB + ENTRY_BYTES));
        for k in 1..=4 {
            c.insert(k, Some(k), KB);
        }
        let freed = c.evict(|k| *k <= 3);
        assert_eq!(freed, [4], "everything not pinned goes");
        assert_eq!(keys(&c), [1, 2, 3]);
        assert!(c.over_budget(), "what is shown is never freed");
        assert!(c.evict(|k| *k <= 3).is_empty(), "nothing left to free");
        // Once unpinned, the oldest go.
        assert_eq!(c.evict(|k| *k == 3), [1]);
        assert_eq!(keys(&c), [2, 3]);
    }

    #[test]
    fn failures_age_out_too() {
        let mut c = cache(2 * ENTRY_BYTES);
        for k in 1..=3 {
            c.insert(k, None, 0);
        }
        assert!(c.evict(|_| false).is_empty(), "no image to free");
        assert_eq!(keys(&c), [2, 3]);
    }

    #[test]
    fn a_loaded_image_is_never_replaced() {
        let mut c = cache(1 << 20);
        assert_eq!(c.insert(1, Some(10), KB), None);
        assert_eq!(
            c.insert(1, Some(11), KB),
            Some(11),
            "the new copy is handed back"
        );
        assert_eq!(c.insert(1, None, 0), None);
        assert_eq!(c.image(&1), Some(&10));
        assert_eq!(c.bytes(), KB + ENTRY_BYTES);
        // A failure is replaced by an image, with its size.
        c.insert(2, None, 0);
        assert_eq!(c.insert(2, Some(20), 4 * KB), None);
        assert_eq!(c.image(&2), Some(&20));
        assert_eq!(c.bytes(), 5 * KB + 2 * ENTRY_BYTES);
    }

    #[test]
    fn a_repeated_insert_counts_as_a_use() {
        let mut c = cache(2 * (KB + ENTRY_BYTES));
        c.insert(1, Some(1), KB);
        c.insert(2, Some(2), KB);
        c.insert(1, Some(1), KB);
        c.insert(3, Some(3), KB);
        assert_eq!(c.evict(|_| false), [2]);
    }

    #[test]
    fn drain_frees_everything() {
        let mut c = cache(1 << 20);
        c.insert(1, Some(1), KB);
        c.insert(2, None, 0);
        c.insert(3, Some(3), KB);
        let mut freed = c.drain();
        freed.sort_unstable();
        assert_eq!(freed, [1, 3]);
        assert_eq!(c.bytes(), 0);
        assert!(keys(&c).is_empty());
        c.insert(4, Some(4), KB);
        assert_eq!(c.bytes(), KB + ENTRY_BYTES, "usable again");
    }

    /// Views are `char`s, windows `u8`s, keys `u32`s.
    type P = Pins<u32, char, u8>;

    fn pinned(p: &P) -> Vec<u32> {
        let mut keys: Vec<u32> = p.pinned().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// One frame of `view` in `window`: GPUI's measuring row twice, then `visible`.
    fn draw(p: &mut P, window: u8, view: char, visible: &[u32]) {
        p.drawing(window, view);
        p.want(view, false, vec![visible[0]]);
        p.want(view, false, vec![visible[0]]);
        p.want(view, false, visible.to_vec());
    }

    #[test]
    fn a_frames_requests_add_up_and_a_still_frame_changes_nothing() {
        let mut p = P::default();
        assert!(!p.end_frame(), "no frame");
        p.drawing(1, 'a');
        assert!(p.frame_open());
        // Measured row, then two groups.
        p.want('a', false, vec![0]);
        p.want('a', false, vec![10, 11]);
        p.want('a', false, vec![20, 21, 22]);
        assert!(p.end_frame());
        assert!(!p.frame_open());
        assert_eq!(pinned(&p), [0, 10, 11, 20, 21, 22]);
        assert_eq!(p.wanted(), [20, 21, 22, 10, 11, 0], "longest first");

        p.drawing(1, 'a');
        p.want('a', false, vec![0]);
        p.want('a', false, vec![10, 11]);
        p.want('a', false, vec![20, 21, 22]);
        assert!(!p.end_frame(), "same as before");

        draw(&mut p, 1, 'a', &[30, 31]);
        assert!(p.end_frame());
        assert_eq!(
            pinned(&p),
            [30, 31],
            "scrolled: the old ones are free to go"
        );
    }

    #[test]
    fn both_sides_of_a_split_stay_pinned() {
        let mut p = P::default();
        draw(&mut p, 1, 'a', &[1, 2]);
        draw(&mut p, 1, 'b', &[3, 4]);
        assert!(p.end_frame());
        assert_eq!(pinned(&p), [1, 2, 3, 4]);
        assert_eq!(p.wanted(), [1, 2, 3, 4]);
        assert!(p.is_pinned(&1) && p.is_pinned(&4));
    }

    #[test]
    fn a_view_its_window_drew_without_is_unpinned() {
        let mut p = P::default();
        draw(&mut p, 1, 'a', &[1, 2]);
        assert!(p.end_frame());
        // Another tab in the same window.
        draw(&mut p, 1, 'b', &[3]);
        assert!(p.end_frame());
        assert_eq!(pinned(&p), [3]);
        // A view that draws but asks for nothing (an empty folder, the home page).
        p.drawing(1, 'b');
        assert!(p.end_frame());
        assert!(pinned(&p).is_empty());
    }

    #[test]
    fn a_window_that_does_not_draw_keeps_its_pins() {
        let mut p = P::default();
        draw(&mut p, 1, 'a', &[1, 2]);
        assert!(p.end_frame());
        for k in 0..5 {
            draw(&mut p, 2, 'b', &[10 + k]);
            assert!(p.end_frame());
        }
        assert_eq!(pinned(&p), [1, 2, 14]);
        assert_eq!(p.wanted(), [14, 1, 2], "the view drawn last first");
        assert!(p.forget_window(1));
        assert_eq!(pinned(&p), [14]);
        assert!(!p.forget_window(1), "nothing left there");
    }

    #[test]
    fn asking_without_drawing_keeps_the_view_in_its_window() {
        let mut p = P::default();
        draw(&mut p, 1, 'a', &[1]);
        assert!(p.end_frame());
        p.want('a', false, vec![2]);
        assert!(p.end_frame());
        assert_eq!(pinned(&p), [2]);
        assert!(p.forget_window(1));
        assert!(pinned(&p).is_empty());
    }

    #[test]
    fn side_columns_come_after_every_main_list() {
        let mut p = P::default();
        p.drawing(1, 'a');
        p.want('a', true, vec![9]);
        p.want('a', false, vec![1, 2]);
        p.drawing(1, 'b');
        p.want('b', false, vec![2, 3]);
        assert!(p.end_frame());
        assert_eq!(p.wanted(), [1, 2, 3, 9], "each key once");
        assert_eq!(pinned(&p), [1, 2, 3, 9]);
    }

    #[test]
    fn clear_forgets_everything() {
        let mut p = P::default();
        draw(&mut p, 1, 'a', &[1]);
        assert!(p.end_frame());
        p.drawing(1, 'a');
        p.clear();
        assert!(!p.frame_open());
        assert!(pinned(&p).is_empty());
        assert!(p.wanted().is_empty());
    }
}
