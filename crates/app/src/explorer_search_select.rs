//! Selecting in search results (#413) and opening a hit's folder from its Location (#398).
//!
//! The results use the file list's own [`Selection`], keyed by each hit's path below the search
//! root (`rel`, unique where names repeat across folders). Clicks with Ctrl and Shift, the keys,
//! Ctrl+A and the marquee make the same `Selection` calls as in a folder, and the actions (menu,
//! clipboard, drag, delete, shelf) take [`Explorer::selected_hits`].
//!
//! Until the user selects something, the hit under the cursor counts as selected, as the first
//! hit always did while results come in; the first gesture makes that the real selection.
//!
//! Nothing here runs per frame: rows only look their hit up in the selection. The hits as
//! entries `Selection` takes are made once per set of hits, when a gesture first needs them.

use std::path::PathBuf;
use std::sync::Arc;

use arke_core::{Entry, NavKind, SearchHit, SearchResults, Selection};
use gpui::{Bounds, Context, Modifiers, Pixels, ScrollStrategy};

use super::Explorer;
use super::similar::hit_at_row;

/// The hits, and the same hits as entries named by `rel` (what `Selection` takes).
type HitKeys = (Arc<Vec<Arc<SearchHit>>>, Arc<Vec<Entry>>);

/// Selection state of the search results (see the module docs).
#[derive(Default)]
pub(crate) struct SearchPick {
    /// The selected hits, by `rel`.
    pub(crate) selection: Selection,
    /// The user chose the selection. Until then the hit under the cursor counts as selected.
    pub(crate) picked: bool,
    /// The hits `keys` was made from, and the hits as entries named by `rel`.
    keys: Option<HitKeys>,
    /// The last press landed on this hit's Location (set by the cell, read by the row's
    /// handler, which runs after it).
    location_pressed: Option<usize>,
    /// Single-click mode: the press on this hit's Location opens the folder on release.
    location_click: Option<usize>,
}

impl Explorer {
    /// The hits as entries named by `rel`, which `Selection`'s index arguments refer to. Made
    /// again only when the hits changed (Arc clones: a few ms for 100k hits).
    fn search_keys(&mut self) -> Option<Arc<Vec<Entry>>> {
        let hits = &self.search_results.as_ref()?.hits;
        if let Some((of, keys)) = &self.search_pick.keys
            && Arc::ptr_eq(of, hits)
        {
            return Some(Arc::clone(keys));
        }
        let keys: Arc<Vec<Entry>> = Arc::new(
            hits.iter()
                .map(|hit| {
                    let mut entry = hit.entry.clone();
                    entry.name = Arc::clone(&hit.rel);
                    entry
                })
                .collect(),
        );
        self.search_pick.keys = Some((Arc::clone(hits), Arc::clone(&keys)));
        Some(keys)
    }

    /// A new query, or no results: nothing chosen yet.
    pub(super) fn reset_search_pick(&mut self) {
        self.search_pick = SearchPick::default();
    }

    /// Whether hit `ix` is selected.
    pub(crate) fn hit_selected(&self, ix: usize, hit: &SearchHit) -> bool {
        if self.search_pick.picked {
            self.search_pick.selection.contains(&hit.rel)
        } else {
            self.search_cursor == Some(ix)
        }
    }

    /// Hit `ix` is selected (`false` past the end).
    fn hit_ix_selected(&self, ix: usize) -> bool {
        self.search_results
            .as_ref()
            .and_then(|r| r.hits.get(ix))
            .is_some_and(|hit| self.hit_selected(ix, hit))
    }

    /// How many hits are selected.
    pub(crate) fn search_selected_count(&self) -> usize {
        if self.search_pick.picked {
            self.search_pick.selection.len()
        } else {
            usize::from(self.search_hit(self.search_cursor).is_some())
        }
    }

    /// The selected hits in list order, with their full paths.
    pub(crate) fn selected_hits(&self) -> Vec<(PathBuf, Arc<SearchHit>)> {
        let Some(results) = self.search_results.as_ref() else {
            return Vec::new();
        };
        let at = |hit: &Arc<SearchHit>| (results.root.join(&*hit.rel), Arc::clone(hit));
        if !self.search_pick.picked {
            return self
                .search_hit(self.search_cursor)
                .map(|h| at(&h))
                .into_iter()
                .collect();
        }
        if self.search_pick.selection.is_empty() {
            return Vec::new();
        }
        results
            .hits
            .iter()
            .filter(|hit| self.search_pick.selection.contains(&hit.rel))
            .map(at)
            .collect()
    }

    /// The selected hits with whether each is a folder (menus, the shelf).
    pub(crate) fn selected_hit_items(&self) -> Vec<(PathBuf, bool)> {
        self.selected_hits()
            .into_iter()
            .map(|(path, hit)| (path, hit.entry.is_dir_like()))
            .collect()
    }

    /// Dragged from hit `ix`: the selection if `ix` is in it, else just that hit.
    pub(crate) fn search_drag_items(&self, ix: usize) -> Vec<(PathBuf, bool)> {
        if self.hit_ix_selected(ix) {
            self.selected_hit_items()
        } else {
            self.path_at(ix).into_iter().collect()
        }
    }

    /// The first gesture makes the hit under the cursor the real selection, so Ctrl and Shift
    /// start from it.
    fn adopt_implicit(&mut self, keys: &[Entry]) {
        if self.search_pick.picked {
            return;
        }
        self.search_pick.picked = true;
        self.search_pick.selection = Selection::default();
        if let Some(ix) = self.search_cursor {
            self.search_pick.selection.select_only(keys, ix);
        }
    }

    /// Left press on hit `ix`, as in a folder's list (`press_selection` in `explorer.rs`).
    pub(super) fn search_press(&mut self, ix: usize, m: Modifiers, click_count: usize) {
        self.similar_focus = false;
        self.click_select = None;
        let Some(keys) = self.search_keys() else {
            return;
        };
        self.adopt_implicit(&keys);
        if super::press_selection(&mut self.search_pick.selection, &keys, ix, m, click_count) {
            self.click_select = Some(ix);
        }
        self.search_cursor = Some(ix);
    }

    /// Selects only hit `ix` (arrow keys, a click inside a multi-selection, a right-click
    /// outside it).
    pub(super) fn search_select_only(&mut self, ix: usize) {
        let Some(keys) = self.search_keys() else {
            return;
        };
        self.search_pick.picked = true;
        self.search_pick.selection.select_only(&keys, ix);
        self.search_cursor = Some(ix);
        self.similar_focus = false;
    }

    /// Right press on hit `ix`: selects only it unless it is selected already.
    pub(super) fn search_right_press(&mut self, ix: usize) {
        if !self.hit_ix_selected(ix) {
            self.search_select_only(ix);
        }
        self.search_cursor = Some(ix);
        self.similar_focus = false;
    }

    /// Shift+arrows: selects from the anchor to hit `ix`.
    pub(super) fn search_extend(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(keys) = self.search_keys() else {
            return;
        };
        let Some(last) = keys.len().checked_sub(1) else {
            return;
        };
        let ix = ix.min(last);
        self.adopt_implicit(&keys);
        self.search_pick.selection.extend_to(&keys, ix, false);
        self.search_cursor = Some(ix);
        self.similar_focus = false;
        let row = self.search_row_of(ix);
        self.search_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Ctrl+A: every hit.
    pub(super) fn search_select_all(&mut self) {
        let Some(keys) = self.search_keys() else {
            return;
        };
        self.search_pick.picked = true;
        self.search_pick.selection.select_all(&keys);
    }

    /// Ctrl+Space: the hit under the cursor in or out of the selection.
    pub(super) fn search_toggle_cursor(&mut self) {
        let (Some(ix), Some(keys)) = (self.search_cursor, self.search_keys()) else {
            return;
        };
        self.adopt_implicit(&keys);
        self.search_pick.selection.toggle(&keys, ix);
    }

    /// Fresh results on screen: the cursor stays where it was (or goes to the first hit), and a
    /// selection the user made keeps the hits that are still there.
    pub(super) fn show_search_results(&mut self, results: SearchResults) {
        let len = results.hits.len();
        self.search_cursor = match self.search_cursor {
            _ if len == 0 => None,
            Some(ix) => Some(ix.min(len - 1)),
            // On "Similar results" (just opened): it stays there.
            None if self.similar_focused() => None,
            None => Some(0),
        };
        self.search_results = Some(results);
        self.sync_search_pick();
    }

    /// The hits changed: hits that left the results leave the selection, and the cursor follows
    /// its hit. When the cursor's hit left (deleted, moved) and nothing selected is left, the hit
    /// now in its place is selected, as after a delete in a folder.
    pub(super) fn sync_search_pick(&mut self) {
        if !self.search_pick.picked {
            return;
        }
        let Some(keys) = self.search_keys() else {
            self.reset_search_pick();
            return;
        };
        self.search_pick.selection.retain_existing(&keys);
        // On "Similar results" there is no hit under the cursor.
        if self.similar_focused() {
            return;
        }
        let selection = &mut self.search_pick.selection;
        if let Some(ix) = selection.cursor_index(&keys, self.search_cursor) {
            self.search_cursor = Some(ix);
            return;
        }
        let Some(last) = keys.len().checked_sub(1) else {
            self.search_cursor = None;
            return;
        };
        let ix = self.search_cursor.unwrap_or(0).min(last);
        if selection.is_empty() {
            selection.select_only(&keys, ix);
        } else {
            selection.set_cursor_name(Some(Arc::clone(&keys[ix].name)));
        }
        self.search_cursor = Some(ix);
    }

    /// A marquee starts on empty space: without Ctrl the selection is cleared. Returns the
    /// selection it starts from (kept with Ctrl).
    pub(super) fn search_marquee_base(&mut self, additive: bool) -> Vec<Arc<str>> {
        let Some(keys) = self.search_keys() else {
            return Vec::new();
        };
        self.adopt_implicit(&keys);
        if !additive {
            self.search_pick.selection.clear();
            self.search_cursor = None;
        }
        self.search_pick.selection.names().cloned().collect()
    }

    /// The marquee covers `area` (list content coordinates): `base` and the hits it reaches
    /// into are selected.
    pub(super) fn apply_search_marquee(&mut self, area: Bounds<Pixels>, base: Vec<Arc<str>>) {
        let Some(keys) = self.search_keys() else {
            return;
        };
        let at = self.similar_row().map(|(at, _)| at);
        let hits = self.marquee_rows(area, keys.len(), |row| hit_at_row(row, at));
        // The selection before the drag (kept with Ctrl) and the hits in the box.
        let selection = &mut self.search_pick.selection;
        selection.select_names(
            base.into_iter().chain(
                hits.iter()
                    .filter_map(|&ix| keys.get(ix).map(|e| Arc::clone(&e.name))),
            ),
        );
        if let Some(&last) = hits.last() {
            self.search_cursor = Some(last);
            self.similar_focus = false;
            selection.set_cursor_name(keys.get(last).map(|e| Arc::clone(&e.name)));
        }
    }

    // ───────────────────────── Location (#398) ─────────────────────────

    /// Records a press on hit `ix`'s Location; the row's own press handler runs next.
    pub(super) const fn on_location_pressed(&mut self, ix: usize) {
        self.search_pick.location_pressed = Some(ix);
    }

    /// The row's press: whether it landed on hit `ix`'s Location. Remembered for the release in
    /// single-click mode.
    pub(super) fn take_location_press(&mut self, ix: usize) -> bool {
        let on = self.search_pick.location_pressed.take() == Some(ix) && self.search_mode;
        self.search_pick.location_click = on.then_some(ix);
        on
    }

    /// Single-click mode, released on hit `ix`: whether the press was on its Location.
    pub(super) fn take_location_click(&mut self, ix: usize) -> bool {
        self.search_pick.location_click.take() == Some(ix) && self.search_mode
    }

    /// Goes to hit `ix`'s folder and selects it there (Shift+Enter, a double-click on its
    /// Location).
    pub(super) fn open_hit_location(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(hit) = self.search_hit(Some(ix)) else {
            return;
        };
        let Some(root) = self.search_results.as_ref().map(|r| r.root.clone()) else {
            return;
        };
        let path = root.join(&*hit.rel);
        if let Some(parent) = path.parent() {
            self.navigate_selecting(
                parent.to_path_buf(),
                NavKind::New,
                Some(Arc::clone(&hit.entry.name)),
                cx,
            );
        }
    }
}
