//! A tab's side of the per-folder view memory (`folder_views.rs`).
//!
//! Rules, with "Remember view for each folder" on:
//! - Opening a folder (new tab, back/forward, path bar, sidebar, double-click) shows its remembered
//!   view, icon size and sort, or the Settings defaults when it has none. Refreshing changes nothing.
//! - Picking a view or sort records it for the folder shown. Leaving a folder records nothing.
//! - Search results neither record nor apply: they aren't a folder.
//! - Columns view is a way of browsing: moving to the parent, a subfolder or a sibling (what the
//!   columns themselves do) keeps the columns and the sort instead of switching per folder.
//! - New defaults in Settings reach tabs showing a folder without a remembered view; remembered
//!   folders keep theirs. "Reset folder views" puts tabs showing a remembered view back on the
//!   defaults.
//!
//! With it off, a tab keeps its own view and sort and follows the defaults until changed, as
//! before; nothing is recorded or applied.

use std::path::Path;

use arke_core::{NavKind, SortSpec};
use gpui::{Context, Entity};

use super::{Explorer, IconSize, ViewMode};
use crate::folder_views::{FolderView, FolderViewStore, folder_key};

/// What a tab showed before a navigation applied the target's view, to put back if the target
/// can't be opened.
#[derive(Clone, Copy, Debug)]
struct Before {
    sort: SortSpec,
    view: ViewMode,
    icons: IconSize,
    applied: bool,
}

/// Per-folder view bookkeeping carried by a navigation that hasn't finished yet.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PendingView {
    before: Option<Before>,
    /// The user picked a view or sort while the folder was opening: save it once it opens.
    record: bool,
}

/// How a tab treats new defaults from Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DefaultsPolicy {
    /// Memory off: follow only where the tab still shows the old defaults (the user didn't pick).
    FollowUnchanged,
    /// The folder has no remembered view, so it shows the defaults, whatever they become.
    Follow,
    /// The folder's own remembered view stays.
    Keep,
}

/// A tab's state for the per-folder memory.
pub(super) struct State {
    /// The setting is on (and this isn't a benchmark, which must not be steered by saved views)
    enabled: bool,
    /// The current sort and view came from, or were saved to, the memory for this folder
    applied: bool,
    /// `FolderViewStore::resets` last seen
    resets: u64,
    /// The memory had finished loading when last seen
    loaded: bool,
}

impl State {
    pub(super) const fn new(enabled: bool, store: &FolderViewStore) -> Self {
        Self {
            enabled,
            applied: false,
            resets: store.resets(),
            loaded: store.loaded(),
        }
    }
}

impl Explorer {
    const fn spec_of(&self, view: FolderView) -> SortSpec {
        SortSpec {
            column: view.column,
            ascending: view.ascending,
            // Where folders go is one setting for every folder
            mixed: self.seen_defaults.0.mixed,
            then: view.then,
        }
    }

    /// A navigation to `path` is about to be sent: show the target's view, and return what the
    /// navigation needs to carry. Called before the listing is requested, because the listing is
    /// sorted with `self.sort`.
    pub(super) fn folder_view_on_navigate(
        &mut self,
        path: &Path,
        kind: NavKind,
        cx: &mut Context<Self>,
    ) -> PendingView {
        // The same target sent again (hidden files toggled while it was opening): keep what was
        // applied, and anything the user picked since.
        let target = folder_key(path);
        if let Some(pending) = self
            .pending
            .as_ref()
            .filter(|p| folder_key(&p.path) == target)
        {
            return pending.folder_view;
        }
        if kind == NavKind::Refresh {
            return PendingView::default();
        }
        // Still on the way somewhere else: what is on screen is what the earlier navigation saved
        let before = self
            .pending
            .as_ref()
            .and_then(|p| p.folder_view.before)
            .unwrap_or(Before {
                sort: self.sort,
                view: self.view_mode,
                icons: self.icon_size,
                applied: self.folder_view.applied,
            });
        self.apply_folder_view(path, cx);
        PendingView {
            before: Some(before),
            record: false,
        }
    }

    fn apply_folder_view(&mut self, target: &Path, cx: &mut Context<Self>) {
        if !self.folder_view.enabled || self.columns_move(target) {
            return;
        }
        let remembered = FolderViewStore::global(cx).update(cx, |s, _| s.lookup(target));
        let (sort, view) = match remembered {
            Some(v) => (self.spec_of(v), v.view),
            None => self.seen_defaults,
        };
        self.folder_view.applied = remembered.is_some();
        // Only the field: the listing about to be requested comes sorted this way, and the folder
        // being left shouldn't be re-sorted for the moment it stays on screen.
        self.sort = sort;
        self.set_icon_size(remembered.map_or_else(IconSize::default, |v| v.icons));
        self.set_view_mode(view, cx);
    }

    /// Columns view moving the way the columns do (to the parent, a subfolder, or a folder next to
    /// this one in the parent column). Switching views on every such step would throw the user out
    /// of the columns, so these keep the tab's view and sort.
    fn columns_move(&self, target: &Path) -> bool {
        if self.view_mode != ViewMode::Columns {
            return false;
        }
        let Some(here) = self.display_path() else {
            return false;
        };
        let parent_key = |p: &Path| p.parent().map(folder_key);
        let (here_key, target_key) = (folder_key(here), folder_key(target));
        let (here_parent, target_parent) = (parent_key(here), parent_key(target));
        target_parent.as_ref() == Some(&here_key)
            || here_parent.as_ref() == Some(&target_key)
            || (target_parent.is_some() && target_parent == here_parent)
    }

    /// The navigation opened its folder.
    pub(super) fn folder_view_on_commit(&mut self, pending: PendingView, cx: &mut Context<Self>) {
        if pending.record {
            self.remember_folder_view(cx);
        }
    }

    /// The navigation failed and the tab stays where it was: put that folder's view back.
    pub(super) fn folder_view_on_fail(&mut self, pending: PendingView, cx: &mut Context<Self>) {
        if let Some(before) = pending.before {
            self.folder_view.applied = before.applied;
            self.sort = before.sort;
            self.set_icon_size(before.icons);
            self.set_view_mode(before.view, cx);
        }
    }

    /// The user picked a view or sort: remember it for the folder shown.
    pub(super) fn remember_folder_view(&mut self, cx: &mut Context<Self>) {
        if !self.folder_view.enabled || self.search_mode {
            return;
        }
        // Picked while a folder is still opening: it's meant for that folder
        if let Some(pending) = self.pending.as_mut().filter(|p| p.kind != NavKind::Refresh) {
            pending.folder_view.record = true;
            return;
        }
        // Nothing opened (error page): nothing to remember it for
        let Some(path) = self.current_path().map(Path::to_path_buf) else {
            return;
        };
        self.folder_view.applied = true;
        let view = FolderView {
            view: self.view_mode,
            icons: self.icon_size,
            column: self.sort.column,
            ascending: self.sort.ascending,
            then: self.sort.then,
        };
        FolderViewStore::global(cx).update(cx, |s, cx| s.record(&path, view, cx));
    }

    /// Settings changed: follow the switch, and say how new defaults apply to this tab.
    pub(super) const fn folder_view_settings(&mut self, remember: bool) -> DefaultsPolicy {
        let enabled = remember && self.bench.is_none();
        if enabled != self.folder_view.enabled {
            self.folder_view.enabled = enabled;
            // Off: back to a view of the tab's own, nothing is "remembered" anymore
            self.folder_view.applied &= enabled;
        }
        match (self.folder_view.enabled, self.folder_view.applied) {
            (false, _) => DefaultsPolicy::FollowUnchanged,
            (true, false) => DefaultsPolicy::Follow,
            (true, true) => DefaultsPolicy::Keep,
        }
    }

    /// The memory was reset or finished loading.
    pub(super) fn on_folder_views_changed(
        &mut self,
        store: Entity<FolderViewStore>,
        cx: &mut Context<Self>,
    ) {
        let (resets, loaded) = {
            let store = store.read(cx);
            (store.resets(), store.loaded())
        };
        if resets != self.folder_view.resets {
            self.folder_view.resets = resets;
            if self.folder_view.applied {
                self.folder_view.applied = false;
                let (sort, view) = self.seen_defaults;
                self.show_view(sort, view, IconSize::default(), cx);
            }
        }
        if loaded && !self.folder_view.loaded {
            self.folder_view.loaded = true;
            // The file arrived after this tab opened its folder: show it the way it was left,
            // unless the user already picked something here
            if self.folder_view.enabled
                && !self.folder_view.applied
                && !self.search_mode
                && let Some(path) = self.display_path().map(Path::to_path_buf)
                && let Some(view) = store.update(cx, |s, _| s.lookup(&path))
            {
                self.folder_view.applied = true;
                let sort = self.spec_of(view);
                self.show_view(sort, view.view, view.icons, cx);
            }
        }
    }

    /// Switch the folder shown to `sort`, `view` and `icons` without recording anything.
    fn show_view(
        &mut self,
        sort: SortSpec,
        view: ViewMode,
        icons: IconSize,
        cx: &mut Context<Self>,
    ) {
        if sort != self.sort {
            self.apply_sort(sort, cx);
        }
        self.set_icon_size(icons);
        self.set_view_mode(view, cx);
        cx.notify();
    }
}

#[cfg(test)]
impl Explorer {
    /// The current sort and view come from the per-folder memory.
    pub(crate) fn folder_view_applied(&self) -> bool {
        self.folder_view.applied
    }
}
