//! The tab's side of the shelf (`shelf.rs`): adding the selection, and "Copy here" / "Move here"
//! into the current folder.

use std::path::PathBuf;

use gpui::{Context, Window};
use rust_i18n::t;

use super::{AddToShelf, Explorer};
use crate::shelf::ShelfStore;

impl Explorer {
    /// Selected items (or the search result under the cursor) with whether each is a folder.
    fn shelf_targets(&self) -> Vec<(PathBuf, bool)> {
        if self.search_mode {
            return self
                .search_hit(self.search_cursor)
                .zip(self.search_results.as_ref())
                .map(|(hit, r)| vec![(r.root.join(&*hit.rel), hit.entry.is_dir_like())])
                .unwrap_or_default();
        }
        let (Some(model), Some(view)) = (self.model.as_ref(), self.view()) else {
            return Vec::new();
        };
        view.iter()
            .filter(|e| self.selection.contains(&e.name))
            .map(|e| (model.path.join(&*e.name), e.is_dir_like()))
            .collect()
    }

    /// Ctrl+Shift+A and "Add to shelf": puts the selection on the shelf (paths only).
    pub(super) fn add_to_shelf(&mut self, _: &AddToShelf, _: &mut Window, cx: &mut Context<Self>) {
        self.add_selection_to_shelf(cx);
    }

    fn add_selection_to_shelf(&mut self, cx: &mut Context<Self>) {
        let targets = self.shelf_targets();
        self.add_paths_to_shelf(targets, cx);
    }

    /// Puts these items on the shelf and says how many were new.
    pub(super) fn add_paths_to_shelf(
        &mut self,
        targets: Vec<(PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() {
            return;
        }
        let count = targets.len();
        let added = ShelfStore::global(cx).update(cx, |s, cx| s.add(targets, cx));
        self.notice = Some(if added == 0 {
            tn!("shelf.already", count).into_owned()
        } else {
            tn!("shelf.added", added).into_owned()
        });
        cx.notify();
    }

    /// "Copy here" / "Move here": the shelf's items that still exist into the current folder.
    /// Moved items leave the shelf once the move is done; copies stay for the next folder.
    pub(crate) fn shelf_to_here(&mut self, is_move: bool, cx: &mut Context<Self>) {
        let Some(dest) = self.current_path().map(PathBuf::from) else {
            return;
        };
        let store = ShelfStore::global(cx);
        let paths = store.read(cx).shelf().present();
        // Items already in this folder stay put; the rest go.
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| p.parent() != Some(dest.as_path()) && !dest.starts_with(p))
            .collect();
        if paths.is_empty() {
            self.notice = Some(t!("shelf.nothing_to_do").into_owned());
            cx.notify();
            return;
        }
        if is_move {
            store.update(cx, |s, _| s.note_moving(&paths));
        }
        self.run_transfer(paths, dest, is_move, cx);
    }

    /// Items dragged onto the Shelf row.
    pub(crate) fn add_dropped_to_shelf(
        &mut self,
        items: Vec<(PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) {
        self.add_paths_to_shelf(items, cx);
    }

    /// Paths dragged from item `ix` (see `drag_paths`) with whether each is a folder. Read from
    /// the list shown, so nested rows of the Details tree (named by relative path) count too.
    pub(crate) fn drag_items(&self, ix: usize) -> Vec<(PathBuf, bool)> {
        if self.search_mode {
            return self.path_at(ix).into_iter().collect();
        }
        let (Some(model), Some(view)) = (self.model.as_ref(), self.view()) else {
            return Vec::new();
        };
        if !view
            .get(ix)
            .is_some_and(|e| self.selection.contains(&e.name))
        {
            return self.path_at(ix).into_iter().collect();
        }
        view.iter()
            .filter(|e| self.selection.contains(&e.name))
            .map(|e| (model.path.join(&*e.name), e.is_dir_like()))
            .collect()
    }

    /// After a file operation: items on the shelf may have moved or been deleted.
    pub(super) fn recheck_shelf(&self, cx: &mut Context<Self>) {
        let fs = self.services.fs.clone();
        ShelfStore::global(cx).update(cx, |s, cx| s.recheck(fs, cx));
    }
}
