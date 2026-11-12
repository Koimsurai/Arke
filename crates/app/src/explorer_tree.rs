//! Inline folder tree in the Details view (setting "Expand folders in the list", off by default).
//!
//! - Folders get a chevron in the Name cell; clicking it, or → on the cursor folder, lists the
//!   folder's contents indented below it. ← collapses, or moves to the parent folder's row.
//! - Each expanded folder has its own [`DirService`]: listed off the UI thread with the tab's sort
//!   and hidden-items setting, then watched like the folder shown, so the tree stays current.
//!   F5 relists every expanded folder.
//! - The list stays one flat, virtualized list ([`arke_core::tree::flatten`], cached until the
//!   folder or an expanded listing changes). Nested rows are named by their path relative to the
//!   folder shown, so selection, rename, drag-and-drop, the context menu and the preview, which
//!   all join the folder shown with a row's name, work on them unchanged.
//! - Filtering and search show the plain list; leaving the folder or the Details view forgets the
//!   expansions.
//! - Rows appearing or disappearing never move what the user is looking at: a chevron click keeps
//!   its row where it was, and listings or changes arriving for expanded folders keep the first
//!   visible row in place ([`ScrollAnchor`]). Only keyboard moves scroll to the cursor.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arke_core::tree::{self as core_tree, Expansion, TreeRow};
use arke_core::{
    DeltaOutcome, DirModel, DirService, Entry, Gen, IoEvent, ListOptions, Selection, service,
};
use gpui::{
    AnyElement, Context, ElementId, MouseButton, MouseDownEvent, Pixels, ScrollStrategy, Window,
    div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{AppEvent, Explorer, ToggleTree, ViewMode};
use crate::assets::Icon;
use crate::settings::SettingsStore;
use crate::theme;

/// Expanded folders with at most this many items resort inline when the sort changes; larger ones
/// are relisted in the background in the new order.
const INLINE_RESORT_MAX: usize = 2_000;

/// The flattened entries and their rows.
type FlatList = (Arc<Vec<Entry>>, Arc<Vec<TreeRow>>);

/// Where an item sits in the viewport, to keep it there while rows above it come and go.
#[derive(Clone, Debug)]
pub(super) struct ScrollAnchor {
    name: Arc<str>,
    /// Item top minus the list's scroll top (negative when the item is partly above the top).
    offset: Pixels,
}

/// One expanded folder.
struct Node {
    /// Tells this node's events apart from an earlier node for the same folder.
    id: u64,
    service: DirService,
    model: DirModel,
}

/// The flattened list for one state of the folder shown and the expansions.
struct FlatCache {
    key: (Gen, u64, usize, u64),
    entries: Arc<Vec<Entry>>,
    rows: Arc<Vec<TreeRow>>,
}

/// A tab's tree state.
pub(super) struct Tree {
    /// The setting is on.
    enabled: bool,
    /// Expanded folders by row name (path relative to the folder shown).
    nodes: HashMap<Arc<str>, Node>,
    next_id: u64,
    /// Bumped whenever a node or its listing changes.
    revision: u64,
    flat: RefCell<Option<FlatCache>>,
}

impl Tree {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            nodes: HashMap::new(),
            next_id: 0,
            revision: 0,
            flat: RefCell::new(None),
        }
    }

    const fn changed(&mut self) {
        self.revision += 1;
    }
}

impl Explorer {
    /// The Details view shows the tree right now (chevrons on folders, nested rows).
    pub(crate) fn tree_active(&self) -> bool {
        self.tree.enabled
            && self.view_mode == ViewMode::Details
            && !self.search_mode
            && !self.is_filtering()
    }

    /// Changes whenever a folder is expanded, collapsed or relisted.
    pub(super) const fn tree_revision(&self) -> u64 {
        self.tree.revision
    }

    /// The tree is shown with at least one folder expanded.
    pub(crate) fn tree_expanding(&self) -> bool {
        self.tree_active() && !self.tree.nodes.is_empty()
    }

    /// The flattened list, when the tree is on and something is expanded (else the plain listing
    /// is the list). Cached until the listing or an expansion changes.
    pub(super) fn tree_view(&self) -> Option<Arc<Vec<Entry>>> {
        self.tree_flat().map(|(entries, _)| entries)
    }

    fn tree_flat(&self) -> Option<FlatList> {
        if !self.tree_active() || self.tree.nodes.is_empty() {
            return None;
        }
        let model = self.model.as_ref()?;
        let key = (
            model.generation,
            model.version,
            Arc::as_ptr(&model.entries) as usize,
            self.tree.revision,
        );
        if let Some(cached) = self.tree.flat.borrow().as_ref().filter(|c| c.key == key) {
            return Some((cached.entries.clone(), cached.rows.clone()));
        }
        let nodes = &self.tree.nodes;
        let flat = core_tree::flatten(&model.entries, |name| match nodes.get(name) {
            None => Expansion::Collapsed,
            Some(node) if node.model.entries.is_empty() && !node.model.complete => {
                Expansion::Loading
            }
            Some(node) => Expansion::Open(&node.model.entries),
        });
        let entries = Arc::new(flat.entries);
        let rows = Arc::new(flat.rows);
        *self.tree.flat.borrow_mut() = Some(FlatCache {
            key,
            entries: entries.clone(),
            rows: rows.clone(),
        });
        Some((entries, rows))
    }

    /// Depth and state of list row `ix` (all top level when nothing is expanded).
    pub(super) fn tree_row(&self, ix: usize) -> TreeRow {
        self.tree_flat()
            .and_then(|(_, rows)| rows.get(ix).copied())
            .unwrap_or_default()
    }

    /// Width of the space before the Name text taken by the chevron and the icon, for the header.
    pub(super) fn name_lead_width(&self) -> gpui::Pixels {
        if self.tree_active() {
            theme::TREE_CHEVRON + theme::S_4 + theme::ICON
        } else {
            theme::ICON
        }
    }

    /// Row lead in the Name cell: indent, chevron (folders) and the item's icon. Without the tree
    /// it's just the icon.
    pub(super) fn tree_lead(
        &self,
        ix: usize,
        entry: &Entry,
        visual: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.tree_active() {
            return visual;
        }
        let row = self.tree_row(ix);
        let name = entry.name.clone();
        let chevron = entry.is_dir_like().then(|| {
            div()
                .id(ElementId::NamedInteger(
                    "tree-chevron".into(),
                    u64::try_from(ix).unwrap_or_default(),
                ))
                .size(theme::TREE_CHEVRON)
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::R_4)
                .cursor_pointer()
                .hover(|s| s.bg(theme::wash_strong()))
                // Toggles without selecting the row or starting a drag.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        window.focus(&this.list_focus, cx);
                        this.toggle_expanded(&name, cx);
                    }),
                )
                .child(
                    svg()
                        .path(Icon::ChevronRight.path())
                        .size(theme::ICON_SM)
                        .text_color(if row.loading {
                            theme::text_disabled()
                        } else {
                            theme::text_subtle()
                        })
                        .with_transformation(gpui::Transformation::rotate(gpui::radians(
                            if row.expanded {
                                std::f32::consts::FRAC_PI_2
                            } else {
                                0.
                            },
                        ))),
                )
        });
        div()
            .flex()
            .flex_none()
            .items_center()
            .pl(theme::TREE_INDENT * f32::from(row.depth))
            .child(
                div()
                    .size(theme::TREE_CHEVRON)
                    .flex_none()
                    .children(chevron),
            )
            .child(div().w(theme::S_4).flex_none())
            .child(visual)
            .into_any_element()
    }

    /// Screen reader nesting of row `ix` in the Details tree: its level, and whether a folder is
    /// expanded. Rows of the plain list are left alone.
    pub(super) fn tree_a11y<E: gpui::StatefulInteractiveElement>(
        &self,
        ix: usize,
        entry: &Entry,
        row: E,
    ) -> E {
        if !self.tree_active() {
            return row;
        }
        let tree_row = self.tree_row(ix);
        let row = row.aria_level(usize::from(tree_row.depth) + 1);
        if entry.is_dir_like() {
            row.aria_expanded(tree_row.expanded)
        } else {
            row
        }
    }

    // ───────────────────────── Expanding and collapsing ─────────────────────────

    /// Chevron click: expand or collapse the folder row `name` (by name: its index may have moved
    /// since the row was drawn). The row stays where it is on screen, and the cursor, which may be
    /// far away, isn't scrolled to.
    fn toggle_expanded(&mut self, name: &Arc<str>, cx: &mut Context<Self>) {
        let Some(entry) = self
            .view()
            .and_then(|v| v.iter().find(|e| e.name == *name).cloned())
        else {
            return;
        };
        let anchor = self.scroll_anchor(Some(name));
        if self.tree.nodes.contains_key(&entry.name) {
            self.collapse(&entry.name, false, cx);
        } else {
            self.expand(&entry, cx);
        }
        self.restore_scroll_anchor(anchor);
    }

    /// Expands the folder row `name` in place if it isn't already, keeping it where it is on
    /// screen. `false`: no such folder row in the tree shown.
    pub(super) fn tree_open_row(&mut self, name: &Arc<str>, cx: &mut Context<Self>) -> bool {
        if !self.tree_active() {
            return false;
        }
        let Some(entry) = self
            .view()
            .and_then(|v| v.iter().find(|e| e.name == *name).cloned())
            .filter(Entry::is_dir_like)
        else {
            return false;
        };
        if !self.tree.nodes.contains_key(name) {
            let anchor = self.scroll_anchor(Some(name));
            self.expand(&entry, cx);
            self.restore_scroll_anchor(anchor);
        }
        true
    }

    /// The listing of the expanded folder row `name`, while the tree is shown.
    pub(super) fn tree_folder(&self, name: &str) -> Option<&DirModel> {
        if !self.tree_active() {
            return None;
        }
        self.tree.nodes.get(name).map(|node| &node.model)
    }

    /// Lists `entry` (a folder row) below itself.
    fn expand(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if !entry.is_dir_like() || self.tree.nodes.contains_key(&entry.name) {
            return;
        }
        let Some(dir) = self.current_path() else {
            return;
        };
        // Remote URLs in one spelling, as `navigate` does (`Path::join` adds a backslash).
        let path = arke_core::remote::canonical(dir.join(&*entry.name));
        let id = self.tree.next_id;
        self.tree.next_id += 1;
        let tx = self.events_tx.clone();
        let rel = entry.name.clone();
        let mut service = DirService::new(
            self.services.fs.clone(),
            Arc::new(move |event| {
                let _ = tx.send(AppEvent::Tree {
                    node: id,
                    rel: rel.clone(),
                    event,
                });
            }),
        );
        if let Some(probe) = self.services.shell.net_probe() {
            service = service.with_probe(probe);
        }
        let generation = service.navigate(path.clone(), self.tree_list_options());
        let model = DirModel::new(path, generation, self.sort);
        self.tree
            .nodes
            .insert(entry.name.clone(), Node { id, service, model });
        self.tree.changed();
        cx.notify();
    }

    /// Hides everything below the folder row named `name`. A selection inside moves to the folder.
    /// `follow`: a keyboard collapse, so the cursor (now on the folder) is scrolled into view.
    fn collapse(&mut self, name: &Arc<str>, follow: bool, cx: &mut Context<Self>) {
        let before = self.tree.nodes.len();
        self.tree
            .nodes
            .retain(|key, _| key != name && !core_tree::is_within(key, name));
        if self.tree.nodes.len() == before {
            return;
        }
        let cursor_inside = self
            .selection
            .cursor()
            .is_some_and(|c| core_tree::is_within(c, name));
        drop_within(&mut self.selection, |n| core_tree::is_within(n, name));
        if cursor_inside {
            self.selection.select_name(name.clone());
        }
        self.tree.changed();
        self.resolve_cursor();
        if follow && let Some(ix) = self.cursor {
            self.scroll_to_index(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// → in the Details view: expand the cursor folder, or step into it once expanded. Returns
    /// whether the key was used.
    pub(super) fn tree_right(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.tree_active() || self.renaming.is_some() {
            return false;
        }
        let Some(ix) = self.cursor else { return false };
        let Some(entry) = self.view().and_then(|v| v.get(ix).cloned()) else {
            return false;
        };
        if !entry.is_dir_like() {
            return false;
        }
        if !self.tree.nodes.contains_key(&entry.name) {
            self.expand(&entry, cx);
        } else if let Some((_, rows)) = self.tree_flat()
            && let Some(child) = core_tree::first_child_row(&rows, ix)
        {
            self.select_index(child, cx);
        }
        true
    }

    /// ← in the Details view: collapse the cursor folder, or move to the folder holding the cursor
    /// item. Returns whether the key was used.
    pub(super) fn tree_left(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.tree_active() || self.renaming.is_some() {
            return false;
        }
        let Some(ix) = self.cursor else { return false };
        let Some(entry) = self.view().and_then(|v| v.get(ix).cloned()) else {
            return false;
        };
        if self.tree.nodes.contains_key(&entry.name) {
            self.collapse(&entry.name, true, cx);
        } else if let Some((_, rows)) = self.tree_flat()
            && let Some(parent) = core_tree::parent_row(&rows, ix)
        {
            self.select_index(parent, cx);
        }
        true
    }

    /// Turns the setting on or off for every tab.
    pub(super) fn toggle_tree(&mut self, _: &ToggleTree, _: &mut Window, cx: &mut Context<Self>) {
        let on = !self.tree.enabled;
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.details_tree = on, cx));
    }

    /// The setting changed.
    pub(super) fn tree_setting(&mut self, enabled: bool) {
        if self.tree.enabled != enabled {
            self.tree.enabled = enabled;
            if !enabled {
                self.tree_clear();
            }
        }
    }

    /// Forgets every expansion (another folder, another view, setting off).
    pub(super) fn tree_clear(&mut self) {
        if self.tree.nodes.is_empty() {
            return;
        }
        self.tree.nodes.clear();
        self.tree.changed();
        drop_within(&mut self.selection, |n| core_tree::parent(n).is_some());
    }

    // ───────────────────────── Listings ─────────────────────────

    const fn tree_list_options(&self) -> ListOptions {
        ListOptions {
            sort: self.sort,
            show_hidden: self.show_hidden,
            watch: true,
        }
    }

    /// F5 or hidden items toggled: list every expanded folder again (the old contents stay on
    /// screen until the new ones arrive).
    pub(super) fn tree_relist(&mut self) {
        let opts = self.tree_list_options();
        for node in self.tree.nodes.values_mut() {
            node.model.generation = node.service.navigate(node.model.path.clone(), opts);
        }
    }

    /// The sort changed: expanded folders follow.
    pub(super) fn tree_resort(&mut self) {
        let spec = self.sort;
        let opts = self.tree_list_options();
        let mut changed = false;
        for node in self.tree.nodes.values_mut() {
            if node.model.sorted_by == spec {
                continue;
            }
            if node.model.entries.len() <= INLINE_RESORT_MAX {
                let sorted = service::resort(&node.model.entries, spec);
                node.model.replace_sorted(Arc::new(sorted), spec);
                changed = true;
            } else {
                node.model.generation = node.service.navigate(node.model.path.clone(), opts);
            }
        }
        if changed {
            self.tree.changed();
        }
    }

    /// A listing, change or failure for the expanded folder `rel`.
    pub(super) fn handle_tree_io(
        &mut self,
        node_id: u64,
        rel: &Arc<str>,
        event: IoEvent,
        cx: &mut Context<Self>,
    ) {
        let opts = self.tree_list_options();
        if self.tree.nodes.get(rel).is_none_or(|n| n.id != node_id) {
            return;
        }
        // Rows of this folder come and go below its own row, possibly above what's on screen.
        let anchor = self.scroll_anchor(None);
        let Some(node) = self.tree.nodes.get_mut(rel) else {
            return;
        };
        let current = node.service.current_gen();
        let relist = match event {
            IoEvent::Listing {
                generation,
                entries,
                sorted_by,
                complete,
                ..
            } if generation == current => {
                node.model.set_snapshot(entries, sorted_by, complete) == DeltaOutcome::NeedsRescan
            }
            IoEvent::Delta { generation, delta } if generation == current => {
                let removed: Vec<Arc<str>> = delta
                    .removed
                    .iter()
                    .map(|name| core_tree::join(Some(rel), name))
                    .collect();
                self.selection.remove_names(&removed);
                node.model.apply_delta(delta) == DeltaOutcome::NeedsRescan
            }
            IoEvent::NeedsRescan { generation } if generation == current => true,
            IoEvent::ListFailed {
                generation,
                path,
                error,
                ..
            } if generation == current => {
                // Can't be listed (access denied, gone): fold it back up and say why.
                let name = rel.clone();
                self.error = Some(
                    t!(
                        "error.open_failed",
                        name = super::short_path(&path),
                        error = error
                    )
                    .into_owned(),
                );
                self.collapse(&name, false, cx);
                self.restore_scroll_anchor(anchor);
                return;
            }
            // Stale: an earlier listing of this folder.
            _ => return,
        };
        if relist && let Some(node) = self.tree.nodes.get_mut(rel) {
            node.model.generation = node.service.navigate(node.model.path.clone(), opts);
        }
        // A listing requested before the sort last changed arrives in the old order.
        self.tree_resort();
        self.tree.changed();
        self.after_content_change(cx);
        self.restore_scroll_anchor(anchor);
    }

    // ───────────────────────── Keeping rows in place ─────────────────────────

    /// Where `name` (or, with `None`, the first item at the top of the list) is on screen. Only
    /// for the Details list with the tree on: the grid lays items out differently.
    pub(super) fn scroll_anchor(&self, name: Option<&Arc<str>>) -> Option<ScrollAnchor> {
        // The plain list keeps its usual behavior; only nested rows shift what's on screen.
        if !self.tree_active() || (name.is_none() && !self.tree_expanding()) {
            return None;
        }
        let view = self.view()?;
        let row_h = self.row_height();
        if row_h <= px(0.) {
            return None;
        }
        let top = -self.scroll.0.borrow().base_handle.offset().y;
        let ix = if let Some(name) = name {
            view.iter().position(|e| e.name == *name)?
        } else {
            // A header row (grouping) has no item: take the next item below it.
            let first = (top / row_h).floor().max(0.);
            // Not negative, and a row count.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let first = first as usize;
            (first..first + 4).find_map(|row| self.item_at_row(row))?
        };
        let entry = view.get(ix)?;
        #[allow(clippy::cast_precision_loss)]
        let item_top = row_h * self.display_row(ix) as f32;
        Some(ScrollAnchor {
            name: entry.name.clone(),
            offset: item_top - top,
        })
    }

    /// Scrolls so the anchored item is back where [`Self::scroll_anchor`] saw it. If it's gone,
    /// the list stays at the same scroll position.
    pub(super) fn restore_scroll_anchor(&self, anchor: Option<ScrollAnchor>) {
        let Some(anchor) = anchor else { return };
        if !self.tree_active() {
            return;
        }
        let Some(ix) = self
            .view()
            .and_then(|v| v.iter().position(|e| e.name == anchor.name))
        else {
            return;
        };
        #[allow(clippy::cast_precision_loss)]
        let item_top = self.row_height() * self.display_row(ix) as f32;
        let top = (item_top - anchor.offset).max(px(0.));
        let state = self.scroll.0.borrow();
        let x = state.base_handle.offset().x;
        // The list clamps it to the end of the content when it paints.
        state.base_handle.set_offset(point(x, -top));
    }

    /// Drops expansions whose folder is gone from its parent's listing (deleted, renamed, now
    /// hidden), and everything below them.
    pub(super) fn tree_prune(&mut self) {
        let Some(model) = self.model.as_ref().filter(|m| m.complete) else {
            return;
        };
        if self.tree.nodes.is_empty() {
            return;
        }
        let mut keys: Vec<Arc<str>> = self.tree.nodes.keys().cloned().collect();
        keys.sort_by_key(|k| core_tree::depth_of(k));
        let mut gone: HashSet<Arc<str>> = HashSet::new();
        for key in keys {
            let parent = core_tree::parent(&key);
            let listing: Option<&[Entry]> = match parent {
                None => Some(&model.entries),
                Some(p) if gone.contains(p) => None,
                Some(p) => match self.tree.nodes.get(p) {
                    // Parent still listing: can't tell yet.
                    Some(n) if !n.model.complete => continue,
                    Some(n) => Some(&n.model.entries),
                    None => None,
                },
            };
            let leaf = core_tree::leaf(&key);
            let present = listing
                .is_some_and(|list| list.iter().any(|e| e.is_dir_like() && &*e.name == leaf));
            if !present {
                gone.insert(key);
            }
        }
        if gone.is_empty() {
            return;
        }
        self.tree.nodes.retain(|k, _| !gone.contains(k));
        drop_within(&mut self.selection, |n| {
            gone.iter().any(|g| core_tree::is_within(n, g))
        });
        self.tree.changed();
    }

    /// Full path of the expanded folder row `name` (tests).
    #[cfg(test)]
    pub(crate) fn tree_expanded(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tree.nodes.keys().map(ToString::to_string).collect();
        v.sort();
        v
    }

    /// Collapses the folder row `name` (tests).
    #[cfg(test)]
    pub(crate) fn collapse_for_test(&mut self, name: &str, cx: &mut Context<Self>) {
        self.collapse(&Arc::from(name), true, cx);
    }

    /// Clicks the chevron of the folder row `name` (tests).
    #[cfg(test)]
    pub(crate) fn click_chevron_for_test(&mut self, name: &str, cx: &mut Context<Self>) {
        self.toggle_expanded(&Arc::from(name), cx);
    }

    /// Whether `dir` is the folder an expanded row lists (tests).
    #[cfg(test)]
    pub(crate) fn tree_lists(&self, dir: &std::path::Path) -> bool {
        self.tree.nodes.values().any(|n| n.model.path == dir)
    }
}

/// Removes selected names matching `inside` (the cursor stays; it resolves to nothing if hidden).
fn drop_within(selection: &mut Selection, inside: impl Fn(&str) -> bool) {
    let names: Vec<Arc<str>> = selection.names().filter(|n| inside(n)).cloned().collect();
    if !names.is_empty() {
        selection.remove_names(&names);
    }
}
