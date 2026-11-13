//! Multi-selection. Tracks selected names, so re-sorting, watcher deltas and filtering
//! never shift the selection onto other items.
//!
//! - `cursor`: keyboard cursor (last touched item); arrow keys move from here.
//! - `anchor`: start of a Shift range selection.
//!
//! Index arguments are positions in the list the caller currently shows (possibly filtered).

use std::collections::HashSet;
use std::sync::Arc;

use crate::entry::Entry;

/// Selected names plus keyboard cursor and range anchor (see the module docs).
#[derive(Debug, Default, Clone)]
pub struct Selection {
    names: HashSet<Arc<str>>,
    cursor: Option<Arc<str>>,
    anchor: Option<Arc<str>>,
}

fn position(entries: &[Entry], name: &str, hint: Option<usize>) -> Option<usize> {
    if let Some(ix) = hint
        && entries.get(ix).is_some_and(|e| &*e.name == name)
    {
        return Some(ix);
    }
    entries.iter().position(|e| &*e.name == name)
}

fn same_ignoring_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

impl Selection {
    /// Number of selected items.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Nothing selected.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Whether the item named `name` is selected.
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Selected names, in no particular order.
    pub fn names(&self) -> impl Iterator<Item = &Arc<str>> {
        self.names.iter()
    }

    /// Name of the keyboard cursor item, if any (it may not be selected).
    pub const fn cursor(&self) -> Option<&Arc<str>> {
        self.cursor.as_ref()
    }

    /// Deselects everything and drops the range anchor; the cursor stays.
    pub fn clear(&mut self) {
        self.names.clear();
        self.anchor = None;
    }

    /// Selects only this name (e.g. the folder we just came up from; it may not be loaded yet).
    pub fn select_name(&mut self, name: Arc<str>) {
        self.names.clear();
        self.names.insert(name.clone());
        self.cursor = Some(name.clone());
        self.anchor = Some(name);
    }

    /// Selects several names (e.g. pasted items), cursor on the first.
    pub fn select_names(&mut self, names: impl IntoIterator<Item = Arc<str>>) {
        self.names.clear();
        self.names.extend(names);
        let first = self.names.iter().next().cloned();
        self.anchor.clone_from(&first);
        self.cursor = first;
    }

    /// Gives names that aren't in `entries` as spelled, but are there in another case, the entry's
    /// spelling. Windows names ignore case, and a name taken from a typed path or one another
    /// program passed may not match the folder's (#7). Returns whether anything changed.
    pub fn adopt_case(&mut self, entries: &[Entry]) -> bool {
        let respell = |name: &Arc<str>| -> Option<Arc<str>> {
            if entries.iter().any(|e| e.name == *name) {
                return None;
            }
            entries
                .iter()
                .find(|e| same_ignoring_case(&e.name, name))
                .map(|e| e.name.clone())
        };
        let mut changed = false;
        let names: Vec<Arc<str>> = self.names.drain().collect();
        for name in names {
            match respell(&name) {
                Some(entry) => {
                    changed = true;
                    self.names.insert(entry);
                }
                None => {
                    self.names.insert(name);
                }
            }
        }
        for slot in [&mut self.cursor, &mut self.anchor] {
            if let Some(entry) = slot.as_ref().and_then(respell) {
                *slot = Some(entry);
                changed = true;
            }
        }
        changed
    }

    /// Sets cursor and anchor without changing the selection (e.g. pre-pointing at a folder after
    /// navigation).
    pub fn set_cursor_name(&mut self, name: Option<Arc<str>>) {
        self.anchor.clone_from(&name);
        self.cursor = name;
    }

    /// Cursor position in the list. `hint` is the last known position, checked before a linear
    /// scan.
    pub fn cursor_index(&self, entries: &[Entry], hint: Option<usize>) -> Option<usize> {
        position(entries, self.cursor.as_deref()?, hint)
    }

    /// Click / arrow key: select just this item.
    pub fn select_only(&mut self, entries: &[Entry], ix: usize) {
        let Some(entry) = entries.get(ix) else { return };
        self.names.clear();
        self.names.insert(entry.name.clone());
        self.cursor = Some(entry.name.clone());
        self.anchor = Some(entry.name.clone());
    }

    /// Ctrl+click / Ctrl+Space: toggle this item and make it the new anchor.
    pub fn toggle(&mut self, entries: &[Entry], ix: usize) {
        let Some(entry) = entries.get(ix) else { return };
        if !self.names.remove(&entry.name) {
            self.names.insert(entry.name.clone());
        }
        self.cursor = Some(entry.name.clone());
        self.anchor = Some(entry.name.clone());
    }

    /// Shift+click / Shift+arrow: select from the anchor to `ix`. With `additive` (Ctrl+Shift)
    /// the existing selection is kept. If the anchor is gone, `ix` becomes the anchor.
    pub fn extend_to(&mut self, entries: &[Entry], ix: usize, additive: bool) {
        let Some(entry) = entries.get(ix) else { return };
        let anchor_ix = self
            .anchor
            .as_deref()
            .and_then(|a| position(entries, a, None))
            .unwrap_or(ix);
        if !additive {
            self.names.clear();
        }
        let (lo, hi) = (anchor_ix.min(ix), anchor_ix.max(ix));
        self.names
            .extend(entries[lo..=hi].iter().map(|e| e.name.clone()));
        if self.anchor.is_none() {
            self.anchor = Some(entry.name.clone());
        }
        self.cursor = Some(entry.name.clone());
    }

    /// Ctrl+A. A few ms for 100k items (only Arc clones and hashing).
    pub fn select_all(&mut self, entries: &[Entry]) {
        self.names.clear();
        self.names.extend(entries.iter().map(|e| e.name.clone()));
    }

    /// After a full content replacement, drops names that no longer exist.
    pub fn retain_existing(&mut self, entries: &[Entry]) {
        if self.names.is_empty() {
            return;
        }
        let present: HashSet<&str> = entries.iter().map(|e| &*e.name).collect();
        self.names.retain(|n| present.contains(&**n));
    }

    /// Keeps only items in `entries` (the list on screen, e.g. after a filter), dropping the cursor
    /// and range anchor too when their item isn't there. Nothing hidden stays selected, so actions
    /// on the selection can't reach an item the user can't see.
    pub fn retain_visible(&mut self, entries: &[Entry]) {
        if self.names.is_empty() && self.cursor.is_none() && self.anchor.is_none() {
            return;
        }
        let present: HashSet<&str> = entries.iter().map(|e| &*e.name).collect();
        self.names.retain(|n| present.contains(&**n));
        if self.cursor.as_deref().is_some_and(|c| !present.contains(c)) {
            self.cursor = None;
        }
        if self.anchor.as_deref().is_some_and(|a| !present.contains(a)) {
            self.anchor = None;
        }
    }

    /// Drops names reported deleted by the watcher, without scanning the list.
    pub fn remove_names(&mut self, removed: &[Arc<str>]) {
        for name in removed {
            self.names.remove(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn entries(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| Entry::new(*n, EntryKind::File, 0, None, 0))
            .collect()
    }

    fn selected(sel: &Selection, list: &[Entry]) -> Vec<String> {
        list.iter()
            .filter(|e| sel.contains(&e.name))
            .map(|e| e.name.to_string())
            .collect()
    }

    #[test]
    fn names_in_another_case_take_the_entry_spelling() {
        let list = entries(&["Projects", "readme.md", "Ärger"]);
        let mut sel = Selection::default();
        sel.select_names(["projects".into(), "ärger".into()]);
        assert!(sel.adopt_case(&list));
        assert_eq!(selected(&sel, &list), ["Projects", "Ärger"]);
        assert!(
            sel.cursor_index(&list, None).is_some(),
            "the cursor follows"
        );
        assert!(!sel.adopt_case(&list), "nothing left to fix");
        // An exact match wins; a name that isn't there at all stays as it is.
        let both = entries(&["foo", "Foo"]);
        sel.select_names(["Foo".into(), "gone".into()]);
        assert!(!sel.adopt_case(&both));
        assert_eq!(selected(&sel, &both), ["Foo"]);
    }

    #[test]
    fn click_ctrl_click_shift_click() {
        let list = entries(&["a", "b", "c", "d", "e"]);
        let mut sel = Selection::default();
        sel.select_only(&list, 1);
        assert_eq!(selected(&sel, &list), ["b"]);
        sel.toggle(&list, 3);
        assert_eq!(selected(&sel, &list), ["b", "d"]);
        // Anchor at d, Shift to a: replaced with a..=d.
        sel.extend_to(&list, 0, false);
        assert_eq!(selected(&sel, &list), ["a", "b", "c", "d"]);
        assert_eq!(sel.cursor_index(&list, None), Some(0));
        // Ctrl unselects c, then Ctrl+Shift to e: keeps the selection and adds c..=e.
        sel.toggle(&list, 2);
        assert_eq!(selected(&sel, &list), ["a", "b", "d"]);
        sel.extend_to(&list, 4, true);
        assert_eq!(selected(&sel, &list), ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn selection_survives_reorder_and_removal() {
        let list = entries(&["a", "b", "c"]);
        let mut sel = Selection::default();
        sel.select_only(&list, 0);
        sel.extend_to(&list, 1, false);
        let reordered = entries(&["c", "b", "a"]);
        assert_eq!(selected(&sel, &reordered), ["b", "a"]);
        assert_eq!(sel.cursor_index(&reordered, Some(1)), Some(1));
        sel.remove_names(&[Arc::from("a")]);
        assert_eq!(sel.len(), 1);
        sel.retain_existing(&entries(&["c"]));
        assert!(sel.is_empty());
    }

    #[test]
    fn retain_visible_drops_hidden_items_cursor_and_anchor() {
        let list = entries(&["a", "b", "c"]);
        let mut sel = Selection::default();
        sel.select_only(&list, 0);
        sel.extend_to(&list, 2, false);
        // A filter leaves only "b" and "x" on screen.
        let shown = entries(&["b", "x"]);
        sel.retain_visible(&shown);
        assert_eq!(selected(&sel, &list), ["b"]);
        assert_eq!(sel.cursor(), None, "cursor was on hidden c");
        // The anchor (a) was hidden too: Shift now starts at the clicked item.
        sel.extend_to(&shown, 1, false);
        assert_eq!(selected(&sel, &shown), ["x"]);
    }

    #[test]
    fn select_all_and_missing_anchor() {
        let list = entries(&["a", "b", "c"]);
        let mut sel = Selection::default();
        sel.select_all(&list);
        assert_eq!(sel.len(), 3);
        // Without an anchor, Shift selects only the target.
        let mut fresh = Selection::default();
        fresh.extend_to(&list, 2, false);
        assert_eq!(selected(&fresh, &list), ["c"]);
    }
}
