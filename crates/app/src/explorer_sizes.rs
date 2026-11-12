//! Folder sizes (the Size column and selection totals).
//!
//! - Computed one by one on a background thread at lowest I/O priority, only after the listing
//!   completes and only on local disks (network locations are skipped, like thumbnails). Canceled
//!   when the folder changes.
//! - Results go in an app-wide cache: returning shows the last value while recomputing; F5
//!   recomputes too.
//! - New subfolders, or ones whose mtime changed (direct children added or removed), are recomputed
//!   without F5. This only runs when the listing version changes, not on every render. Deeper
//!   changes need F5.
//! - Volumes the search index follows live through the change journal (background service) are
//!   answered from the index in one pass instead of a walk (`IndexService::child_dir_sizes`);
//!   folders the index skips are still walked, so totals are the same. Others are walked.
//! - Can be turned off with the "Calculate folder sizes" setting.
//! - Sorting by size uses these values for folders (`Explorer::ensure_sorted`); any change bumps
//!   `rev` so the list re-sorts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arke_core::EntryKind;
use gpui::{App, Context, Global};

use super::{AppEvent, Explorer, spawn_thread};

/// Computed folder sizes shared by all windows and tabs, shown first on return.
#[derive(Default)]
struct SizeCache(HashMap<PathBuf, u64>);

impl Global for SizeCache {}

/// Sizes of subfolders in this tab's current folder.
#[derive(Default)]
pub(crate) struct FolderSizes {
    /// Folder that `sizes` belongs to (computing or done)
    dir: Option<PathBuf>,
    /// Shared as `Arc` so background sorting doesn't copy it
    sizes: Arc<HashMap<Arc<str>, u64>>,
    /// Not computed yet
    pending: HashSet<Arc<str>>,
    cancel: Option<Arc<AtomicBool>>,
    /// Bumped whenever `sizes` changes
    rev: u64,
    /// F5: recompute everything after the listing completes (old values stay visible)
    stale: bool,
    /// Each subfolder's mtime when computed (recompute if it changes)
    seen: HashMap<Arc<str>, Option<std::time::SystemTime>>,
    /// Listing version at the last recompute check
    checked_version: u64,
}

impl FolderSizes {
    fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Recompute after the listing reloads (F5).
    pub(crate) fn invalidate(&mut self) {
        self.stop();
        self.stale = true;
    }

    /// Drop everything (folder changed or setting turned off).
    fn clear(&mut self) {
        self.stop();
        self.dir = None;
        self.stale = false;
        self.pending.clear();
        self.seen.clear();
        if !self.sizes.is_empty() {
            self.sizes = Arc::default();
            self.rev += 1;
        }
    }

    /// Some folders are still being computed.
    pub(crate) fn computing(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Revision of the computed sizes, if any (sorting uses it to decide whether to re-sort).
    pub(crate) fn revision(&self) -> Option<u64> {
        (!self.sizes.is_empty()).then_some(self.rev)
    }

    /// Current sizes (name -> bytes) for sorting.
    pub(crate) fn snapshot(&self) -> Arc<HashMap<Arc<str>, u64>> {
        self.sizes.clone()
    }
}

impl Drop for FolderSizes {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the Size column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FolderSize {
    /// Not computed (setting off, network location)
    Unknown,
    /// Done; `fresh` means computed this time rather than an old cached value
    Known { bytes: u64, fresh: bool },
    /// Still computing with no old value
    Computing,
}

fn enabled(cx: &App) -> bool {
    cx.try_global::<crate::settings::GlobalSettings>()
        .is_some_and(|g| g.0.read(cx).settings.folder_sizes)
}

impl Explorer {
    /// Before each render: start computing once the current folder is listed and not yet computed.
    pub(super) fn sync_folder_sizes(&mut self, cx: &mut Context<Self>) {
        let on = enabled(cx) && !self.search_mode;
        let shown = self.model.as_ref().map(|m| m.path.as_path());
        // Folder changed (or turned off): drop the previous folder's sizes, or same-named
        // subfolders would show wrong values.
        if self.folder_sizes.dir.is_some() && (!on || self.folder_sizes.dir.as_deref() != shown) {
            self.folder_sizes.clear();
            self.ensure_sorted(cx);
        }
        if !on {
            return;
        }
        let Some(model) = self.model.as_ref().filter(|m| m.complete) else {
            return;
        };
        if self.folder_sizes.dir.as_deref() == Some(model.path.as_path())
            && !self.folder_sizes.stale
        {
            self.refresh_changed_sizes(cx);
            return;
        }
        let path = model.path.clone();
        // Skip network locations; while the local check is still running, wait (it redraws when
        // done).
        if !self.media.update(cx, |m, _| m.media_allowed(&path)) {
            return;
        }
        let dirs: Vec<(Arc<str>, Option<std::time::SystemTime>)> = model
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .map(|e| (e.name.clone(), e.modified))
            .collect();
        let names: Vec<Arc<str>> = dirs.iter().map(|(n, _)| n.clone()).collect();
        self.folder_sizes.seen = dirs.into_iter().collect();
        self.folder_sizes.checked_version = model.version;
        self.folder_sizes.stop();
        self.folder_sizes.stale = false;
        let cache = cx.default_global::<SizeCache>();
        self.folder_sizes.sizes = Arc::new(
            names
                .iter()
                .filter_map(|n| cache.0.get(&path.join(&**n)).map(|&b| (n.clone(), b)))
                .collect(),
        );
        self.folder_sizes.rev += 1;
        self.folder_sizes.pending = names.iter().cloned().collect();
        self.folder_sizes.dir = Some(path.clone());
        // With previous values, sort by them first when sorting by size.
        self.ensure_sorted(cx);
        if names.is_empty() {
            return;
        }
        self.start_size_worker(path, names, cx);
    }

    /// Listing changed (watcher saw adds or edits): recompute new subfolders and ones whose mtime
    /// changed.
    fn refresh_changed_sizes(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        if model.version == self.folder_sizes.checked_version || self.folder_sizes.computing() {
            return;
        }
        self.folder_sizes.checked_version = model.version;
        let changed: Vec<(Arc<str>, Option<std::time::SystemTime>)> = model
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .filter(|e| self.folder_sizes.seen.get(&e.name) != Some(&e.modified))
            .map(|e| (e.name.clone(), e.modified))
            .collect();
        if changed.is_empty() {
            return;
        }
        let path = model.path.clone();
        let names: Vec<Arc<str>> = changed.iter().map(|(n, _)| n.clone()).collect();
        self.folder_sizes.seen.extend(changed);
        self.folder_sizes.pending.extend(names.iter().cloned());
        self.start_size_worker(path, names, cx);
        cx.notify();
    }

    /// Compute `names` (subfolders of `path`) in the background: from the index when it covers
    /// the folder, otherwise one by one by walking.
    fn start_size_worker(&mut self, path: PathBuf, names: Vec<Arc<str>>, cx: &App) {
        self.folder_sizes.stop();
        let cancel = Arc::new(AtomicBool::new(false));
        self.folder_sizes.cancel = Some(cancel.clone());
        let fs = self.services.fs.clone();
        let tx = self.events_tx.clone();
        let index = crate::global_search::GlobalSearch::local_service(cx);
        spawn_thread("folder-sizes", move || {
            crate::win::lower_io_priority();
            let mut names = names;
            if let Some(sizes) = index.and_then(|svc| svc.child_dir_sizes(&path, &cancel)) {
                let by_name: HashMap<String, u64> = sizes
                    .into_iter()
                    .map(|(name, size)| (name.to_lowercase(), size.bytes))
                    .collect();
                let mut missing = Vec::new();
                for name in names {
                    match by_name.get(&name.to_lowercase()) {
                        Some(&bytes) => {
                            let _ = tx.send(AppEvent::FolderSize {
                                dir: path.clone(),
                                name,
                                bytes: Some(bytes),
                            });
                        }
                        // New since the index last heard (or not indexed): walk it.
                        None => missing.push(name),
                    }
                }
                names = missing;
            }
            for name in names {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let size = arke_core::dirsize::dir_size(&*fs, &path.join(&*name), &cancel);
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let _ = tx.send(AppEvent::FolderSize {
                    dir: path.clone(),
                    name,
                    bytes: size.map(|s| s.bytes),
                });
            }
        });
    }

    pub(super) fn handle_folder_size(
        &mut self,
        dir: &Path,
        name: Arc<str>,
        bytes: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self.folder_sizes.dir.as_deref() != Some(dir) {
            return;
        }
        self.folder_sizes.pending.remove(&name);
        let before = self.folder_sizes.sizes.get(&name).copied();
        if before != bytes {
            // Same as the last value: leave it so no re-sort is needed.
            let sizes = Arc::make_mut(&mut self.folder_sizes.sizes);
            match bytes {
                Some(bytes) => sizes.insert(name.clone(), bytes),
                None => sizes.remove(&name),
            };
            self.folder_sizes.rev += 1;
        }
        if let Some(bytes) = bytes {
            cx.default_global::<SizeCache>()
                .0
                .insert(dir.join(&*name), bytes);
        }
        // Re-sort when sorting by size (always once when everything is done).
        self.ensure_sorted(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn folder_sizes_computing(&self) -> bool {
        self.folder_sizes.computing()
    }

    /// What the Size column shows for subfolder `name`.
    pub(crate) fn folder_size(&self, name: &str) -> FolderSize {
        if self.folder_sizes.dir.is_none() {
            return FolderSize::Unknown;
        }
        let pending = self.folder_sizes.pending.contains(name);
        match self.folder_sizes.sizes.get(name) {
            Some(&bytes) => FolderSize::Known {
                bytes,
                fresh: !pending,
            },
            None if pending => FolderSize::Computing,
            None => FolderSize::Unknown,
        }
    }
}
