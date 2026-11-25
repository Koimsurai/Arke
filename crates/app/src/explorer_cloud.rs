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
//! - The first scan of a folder that finds it inside a sync root also tells the shell the tab
//!   shows it, as File Explorer does ([`crate::shell::Shell::folder_visit`]): some sync clients
//!   (WPS Cloud) only add a folder's online-only items then (#319). Their placeholders don't
//!   always come with a watcher event, so once a client heard it the folder is listed again on a
//!   background thread a few times and reloaded when items came or went. Folders outside a sync
//!   root cost nothing more: the scan reads whether it is one anyway.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::Gen;
use arke_core::entry::{CloudSync, Entry, EntryKind, unsynced_by_attributes};
use gpui::{Context, Task, Window};

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
    /// The tab's window (`HWND`), for telling the shell what it shows.
    window: Option<isize>,
    /// The current folder's first scan hasn't come back yet: if it is in a sync root, the shell
    /// is told the tab shows it.
    visit_due: bool,
    /// The shell was told the tab shows the current folder.
    visiting: bool,
}

impl CloudStates {
    /// The sync client's state for an item of the current folder.
    #[cfg(test)]
    pub(crate) fn sync_of(&self, name: &str) -> Option<CloudSync> {
        self.states.get(name).copied()
    }

    /// States of the current folder's items, to look up while drawing rows.
    pub(crate) fn snapshot(&self) -> States {
        Arc::clone(&self.states)
    }

    /// Whether `path` is inside a folder found to be in a sync root.
    pub(crate) fn detected(&self, path: &Path) -> bool {
        !self.roots.is_empty() && self.roots.iter().any(|root| path.starts_with(root))
    }

    fn remember(&mut self, dir: &Path, states: &States) {
        self.cache.retain(|(d, _)| d != dir);
        self.cache
            .push_back((dir.to_path_buf(), Arc::clone(states)));
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
    pub(super) fn sync_cloud_states(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let path = model.path.clone();
        if self.cloud.dir.as_deref() != Some(path.as_path()) {
            if std::mem::take(&mut self.cloud.visiting) {
                self.services
                    .shell
                    .folder_visit(cx.entity_id().as_u64(), None);
            }
            self.cloud.window = Self::owner_from(window);
            self.cloud.visit_due = true;
            // Another folder: show its last known states while rereading.
            self.cloud.states = self
                .cloud
                .cache
                .iter()
                .find(|(d, _)| *d == path)
                .map_or_default(|(_, s)| Arc::clone(s));
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
        let entries = Arc::clone(&model.entries);
        self.cloud.version = Some(version);
        self.cloud.inflight = true;
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("cloud-scan", move || {
            let items: Vec<Arc<str>> = entries
                .iter()
                .filter(|e| e.wants_sync_check())
                .map(|e| Arc::clone(&e.name))
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
        let sync_root = scan.sync_root;
        if sync_root {
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
        if std::mem::take(&mut self.cloud.visit_due) && sync_root {
            self.announce_visit(dir, cx);
        }
        if self.cloud.states != states {
            self.cloud.states = states;
        }
        // Repaint either way: the listing may have changed again while reading.
        cx.notify();
    }
}

impl Explorer {
    /// Tells the shell this tab shows `dir`, a folder inside a sync root.
    fn announce_visit(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.cloud.visiting = true;
        let (tx, heard_dir) = (self.events_tx.clone(), dir.clone());
        self.services.shell.folder_visit(
            cx.entity_id().as_u64(),
            Some(crate::shell::FolderVisit {
                window: self.cloud.window,
                dir,
                heard: Box::new(move || {
                    let _ = tx.send(AppEvent::CloudFolderHeard(heard_dir.clone()));
                }),
            }),
        );
    }

    /// A sync client heard that this tab shows `dir` and may be adding its online-only items: list
    /// it again on a background thread and reload only when items came or went.
    pub(super) fn recheck_cloud_folder(&self, dir: &Path) {
        let Some(model) = self
            .model
            .as_ref()
            .filter(|m| m.complete && m.path == dir)
        else {
            return;
        };
        if self.pending.is_some() {
            return;
        }
        let (generation, version) = (model.generation, model.version);
        let entries = Arc::clone(&model.entries);
        let fs = Arc::clone(&self.services.fs);
        let (dir, show_hidden, tx) = (dir.to_owned(), self.show_hidden, self.events_tx.clone());
        spawn_thread("cloud-recheck", move || {
            let Ok(listed) = fs.read_dir(&dir) else {
                return;
            };
            if items_changed(&entries, listed, show_hidden) {
                let _ = tx.send(AppEvent::CloudFolderChanged {
                    generation,
                    version,
                });
            }
        });
    }

    /// The recheck found items the list doesn't show (or shows no longer): list it again.
    pub(super) fn cloud_folder_changed(
        &mut self,
        generation: Gen,
        version: u64,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .model
            .as_ref()
            .is_some_and(|m| m.generation == generation && m.version == version);
        if current && self.pending.is_none() {
            self.reload(cx);
        }
    }
}

/// Whether listing a folder again found items come or go compared with `shown` (names and kinds;
/// changes to items already shown come through the watcher). `false` when the listing failed part
/// way: nothing to act on.
fn items_changed(
    shown: &[Entry],
    listed: impl IntoIterator<Item = io::Result<Entry>>,
    show_hidden: bool,
) -> bool {
    let known: HashMap<&str, &EntryKind> = shown.iter().map(|e| (&*e.name, &e.kind)).collect();
    let mut seen = 0;
    for entry in listed {
        let Ok(entry) = entry else {
            return false;
        };
        if entry.concealed(show_hidden) {
            continue;
        }
        if known
            .get(&*entry.name)
            .is_none_or(|kind| **kind != entry.kind)
        {
            return true;
        }
        seen += 1;
    }
    seen != known.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIDDEN: u32 = 0x2;
    /// `FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: online-only.
    const ONLINE_ONLY: u32 = 0x400 | 0x40_0000;

    fn file(name: &str, attributes: u32) -> Entry {
        Entry::new(name, EntryKind::File, 1, None, attributes)
    }

    fn listed(entries: &[Entry]) -> Vec<io::Result<Entry>> {
        entries.iter().cloned().map(Ok).collect()
    }

    #[test]
    fn the_same_items_are_no_change() {
        let shown = [file("a.txt", 0), file("b.txt", ONLINE_ONLY)];
        assert!(!items_changed(&shown, listed(&shown), false));
        assert!(!items_changed(&[], Vec::new(), false));
    }

    #[test]
    fn items_added_by_the_sync_client_are_a_change() {
        let shown = [file("local.txt", 0)];
        let now = [file("local.txt", 0), file("remote.txt", ONLINE_ONLY)];
        assert!(items_changed(&shown, listed(&now), false));
        // An empty folder that got its online-only items.
        assert!(items_changed(&[], listed(&now), false));
    }

    #[test]
    fn items_gone_or_of_another_kind_are_a_change() {
        let shown = [file("a.txt", 0), file("b.txt", 0)];
        assert!(items_changed(&shown, listed(&shown[..1]), false));
        let now = [
            file("a.txt", 0),
            Entry::new("b.txt", EntryKind::Dir, 0, None, 0),
        ];
        assert!(items_changed(&shown, listed(&now), false));
    }

    #[test]
    fn hidden_items_count_only_when_shown() {
        let shown = [file("a.txt", 0)];
        let now = [file("a.txt", 0), file("secret", HIDDEN)];
        assert!(!items_changed(&shown, listed(&now), false));
        assert!(items_changed(&shown, listed(&now), true));
    }

    #[test]
    fn a_failed_listing_changes_nothing() {
        let mut now = listed(&[file("a.txt", 0)]);
        now.push(Err(io::Error::other("network")));
        assert!(!items_changed(&[], now, false));
    }

    /// Size and attribute changes of items already shown reach the list through the watcher.
    #[test]
    fn changes_to_shown_items_are_left_to_the_watcher() {
        let shown = [file("a.txt", 0)];
        let now = [Entry::new("a.txt", EntryKind::File, 99, None, ONLINE_ONLY)];
        assert!(!items_changed(&shown, listed(&now), false));
    }
}
