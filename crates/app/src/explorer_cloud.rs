//! Cloud sync states of the current folder's items (syncing, not synced), for the badges after the
//! names.
//!
//! - Read on a background thread once the listing completes, and again when it changes (the sync
//!   client updates attributes as files finish, which the watcher reports), at most every
//!   [`RESCAN_GAP`].
//! - Only local folders; network locations are skipped like thumbnails.
//! - Results are cached per folder, so going back shows the last states at once while rereading.
//! - Folders the Cloud Files API places inside a sync root are remembered as cloud folders even
//!   when the provider isn't listed in the sidebar, so their badges and cloud menu items show too.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::entry::{CloudSync, unsynced_by_attributes};
use gpui::{Context, Task};

use super::{AppEvent, Explorer, spawn_thread};
use crate::shell::CloudScan;

/// Shortest time between two reads of the same folder while it keeps changing.
const RESCAN_GAP: Duration = Duration::from_millis(500);
/// Folders whose last states are kept for going back.
const CACHED_FOLDERS: usize = 32;
/// Sync root folders remembered by this tab.
const KNOWN_ROOTS: usize = 256;

type States = Arc<HashMap<Arc<str>, CloudSync>>;

/// This tab's cloud states.
#[derive(Default)]
pub(crate) struct CloudStates {
    /// Folder the states belong to
    dir: Option<PathBuf>,
    /// Listing version last read (or being read)
    version: Option<u64>,
    inflight: bool,
    states: States,
    /// Last read finished at, for [`RESCAN_GAP`]
    last_scan: Option<Instant>,
    rescan: Option<Task<()>>,
    /// Folders found inside a sync root
    roots: HashSet<PathBuf>,
    /// Last states per folder, newest last
    cache: VecDeque<(PathBuf, States)>,
}

impl CloudStates {
    /// The sync client's state for an item of the current folder.
    #[cfg(test)]
    pub(crate) fn sync_of(&self, name: &str) -> Option<CloudSync> {
        self.states.get(name).copied()
    }

    /// States of the current folder's items, to look up while drawing rows.
    pub(crate) fn snapshot(&self) -> States {
        self.states.clone()
    }

    /// Whether `path` is inside a folder found to be in a sync root.
    pub(crate) fn detected(&self, path: &Path) -> bool {
        !self.roots.is_empty() && self.roots.iter().any(|root| path.starts_with(root))
    }

    fn remember(&mut self, dir: &Path, states: &States) {
        self.cache.retain(|(d, _)| d != dir);
        self.cache.push_back((dir.to_path_buf(), states.clone()));
        while self.cache.len() > CACHED_FOLDERS {
            self.cache.pop_front();
        }
    }

    fn note_root(&mut self, dir: &Path) {
        if self.detected(dir) {
            return;
        }
        if self.roots.len() >= KNOWN_ROOTS {
            self.roots.clear();
        }
        self.roots.insert(dir.to_path_buf());
    }
}

impl Explorer {
    /// Before each render: read the current folder's sync states when it is new or changed.
    pub(super) fn sync_cloud_states(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let path = model.path.clone();
        if self.cloud.dir.as_deref() != Some(path.as_path()) {
            // Another folder: show its last known states while rereading.
            self.cloud.states = self
                .cloud
                .cache
                .iter()
                .find(|(d, _)| *d == path)
                .map_or_default(|(_, s)| s.clone());
            self.cloud.dir = Some(path.clone());
            self.cloud.version = None;
            self.cloud.inflight = false;
            self.cloud.last_scan = None;
            self.cloud.rescan = None;
        }
        if !model.complete || self.cloud.inflight || self.cloud.version == Some(model.version) {
            return;
        }
        if !self.media.update(cx, |m, _| m.media_allowed(&path)) {
            return;
        }
        if let Some(last) = self.cloud.last_scan
            && last.elapsed() < RESCAN_GAP
        {
            if self.cloud.rescan.is_none() {
                let wait = RESCAN_GAP.saturating_sub(last.elapsed());
                self.cloud.rescan = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |this, cx| {
                        this.cloud.rescan = None;
                        cx.notify();
                    });
                }));
            }
            return;
        }
        let version = model.version;
        let entries = model.entries.clone();
        self.cloud.version = Some(version);
        self.cloud.inflight = true;
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("cloud-scan", move || {
            let items: Vec<Arc<str>> = entries
                .iter()
                .filter(|e| e.wants_sync_check())
                .map(|e| e.name.clone())
                .collect();
            let mut scan = shell.cloud_scan(&path, &items);
            if scan.sync_root {
                scan.not_synced = unsynced_by_attributes(&entries);
            }
            let _ = tx.send(AppEvent::CloudScan {
                dir: path,
                version,
                scan,
            });
        });
    }

    pub(super) fn handle_cloud_scan(
        &mut self,
        dir: PathBuf,
        version: u64,
        scan: CloudScan,
        cx: &mut Context<Self>,
    ) {
        if scan.sync_root {
            self.cloud.note_root(&dir);
        }
        let states: HashMap<Arc<str>, CloudSync> = scan
            .syncing
            .into_iter()
            .map(|n| (n, CloudSync::Syncing))
            .chain(
                scan.not_synced
                    .into_iter()
                    .map(|n| (n, CloudSync::NotSynced)),
            )
            .collect();
        let states: States = Arc::new(states);
        self.cloud.remember(&dir, &states);
        if self.cloud.dir.as_deref() != Some(dir.as_path()) || self.cloud.version != Some(version) {
            return;
        }
        self.cloud.inflight = false;
        self.cloud.last_scan = Some(Instant::now());
        if self.cloud.states != states {
            self.cloud.states = states;
        }
        // Repaint either way: the listing may have changed again while reading.
        cx.notify();
    }
}
