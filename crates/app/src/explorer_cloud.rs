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
//! - The first scan of a folder that finds it inside a sync root also tells the shell the pane
//!   shows it, as File Explorer does ([`crate::shell::Shell::folder_visit`]): some sync clients
//!   (WPS Cloud) only add a folder's online-only items then (#319). The announcement lasts while
//!   the pane shows that folder ([`VisitRule`]; [`VisitGuard`] ends it when the pane goes away).
//!   Folders outside a sync root cost nothing more: the scan reads whether it is one anyway.
//! - A hint from the shell that the folder's items changed (see `win_folder_visit.rs`) lists it
//!   again on a background thread, one listing at a time ([`Recheck`]), and reloads only when
//!   items came or went: placeholders may arrive without a watcher event.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::Gen;
use arke_core::entry::{CloudSync, Entry, EntryKind, unsynced_by_attributes};
use gpui::{Context, Task, Window};

use super::{AppEvent, Explorer, spawn_thread};
use crate::shell::{CloudScan, Shell};

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
    /// The pane's window (`HWND`), for telling the shell what it shows.
    window: Option<isize>,
    visit_rule: VisitRule,
    /// The shell's announcement of the current folder; dropping it ends the announcement.
    visit: Option<VisitGuard>,
    recheck: Recheck,
    /// Hints since the last recheck log line that found one running (merged into its follow-up)
    /// or the list not ready (waiting for it).
    hints_merged: u32,
    hints_waiting: u32,
}

/// What a listing of a cloud folder holds, for the log: how many items are online-only, local
/// placeholders, or plain (not taken over by the sync client).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CloudCounts {
    items: usize,
    online_only: usize,
    placeholders: usize,
    plain_files: usize,
    plain_folders: usize,
}

impl CloudCounts {
    /// Counts `entries` (links count as items only).
    fn of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        /// `FILE_ATTRIBUTE_REPARSE_POINT`
        const REPARSE: u32 = 0x400;
        let mut counts = Self::default();
        for entry in entries {
            counts.items += 1;
            if matches!(entry.kind, EntryKind::Link { .. }) {
                continue;
            }
            if entry.is_cloud_placeholder() {
                counts.online_only += 1;
            } else if entry.attributes & REPARSE != 0 {
                counts.placeholders += 1;
            } else if entry.kind == EntryKind::Dir {
                counts.plain_folders += 1;
            } else {
                counts.plain_files += 1;
            }
        }
        counts
    }
}

impl std::fmt::Display for CloudCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} items (online-only {}, local placeholders {}, plain files {}, plain folders {})",
            self.items, self.online_only, self.placeholders, self.plain_files, self.plain_folders
        )
    }
}

/// When the shell is told what a pane shows: from the first scan that finds the folder inside a
/// sync root until the pane shows another folder (or goes away, see [`VisitGuard`]). Refreshes and
/// later scans of the same folder change nothing.
#[derive(Default)]
struct VisitRule {
    /// The current folder's first scan hasn't come back yet.
    due: bool,
    /// The current folder is announced.
    active: bool,
}

impl VisitRule {
    /// The pane shows another folder. Returns whether the last one's announcement ends.
    const fn folder_changed(&mut self) -> bool {
        self.due = true;
        let ended = self.active;
        self.active = false;
        ended
    }

    /// A scan of the current folder came back. Returns whether to announce the folder now.
    const fn scanned(&mut self, sync_root: bool) -> bool {
        let announce = self.due && sync_root;
        self.due = false;
        if announce {
            self.active = true;
        }
        announce
    }
}

/// Ends the pane's announcement when dropped: when the pane shows another folder, or closes (with
/// its window, too).
struct VisitGuard {
    shell: Arc<dyn Shell>,
    slot: u64,
}

impl Drop for VisitGuard {
    fn drop(&mut self) {
        self.shell.folder_visit(self.slot, None);
    }
}

/// Listing the folder again after a hint that its items changed: one listing at a time, and
/// one more after it for hints that came meanwhile, however many. While the list isn't ready (a
/// listing or navigation is running) the request waits for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Recheck {
    #[default]
    Idle,
    /// Asked while the list wasn't ready.
    Waiting,
    Running {
        /// Asked again meanwhile.
        again: bool,
    },
}

impl Recheck {
    /// A hint came. Returns whether to start a listing now.
    const fn ask(&mut self, ready: bool) -> bool {
        match *self {
            Self::Idle | Self::Waiting if ready => {
                *self = Self::Running { again: false };
                true
            }
            Self::Idle | Self::Waiting => {
                *self = Self::Waiting;
                false
            }
            Self::Running { .. } => {
                *self = Self::Running { again: true };
                false
            }
        }
    }

    /// The list may have become ready (before each render). Returns whether to start a listing.
    const fn ready(&mut self, ready: bool) -> bool {
        if ready && matches!(self, Self::Waiting) {
            *self = Self::Running { again: false };
            return true;
        }
        false
    }

    /// The running listing found items came or went, but the list can't take that from it (it
    /// changed meanwhile, or a listing is running): one more after it, against the list as it is
    /// then, so the change isn't lost.
    const fn rerun(&mut self) {
        if let Self::Running { .. } = *self {
            *self = Self::Running { again: true };
        }
    }

    /// The listing finished. Returns whether to start another one now.
    const fn finished(&mut self, ready: bool) -> bool {
        match *self {
            Self::Running { again: true } if ready => {
                *self = Self::Running { again: false };
                true
            }
            Self::Running { again: true } => {
                *self = Self::Waiting;
                false
            }
            Self::Running { again: false } => {
                *self = Self::Idle;
                false
            }
            Self::Idle | Self::Waiting => false,
        }
    }
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
            if self.cloud.visit_rule.folder_changed() {
                // Dropping it ends the announcement of the folder left.
                self.cloud.visit = None;
            }
            self.cloud.window = Self::owner_from(window);
            self.cloud.recheck = Recheck::default();
            self.cloud.hints_merged = 0;
            self.cloud.hints_waiting = 0;
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
        let ready = model.complete && self.pending.is_none();
        if self.cloud.recheck.ready(ready) {
            self.start_recheck();
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
        // The folder's first scan of this visit: what it holds goes in the log (#319).
        let first = self.cloud.visit_rule.due;
        spawn_thread("cloud-scan", move || {
            let items: Vec<Arc<str>> = entries
                .iter()
                .filter(|e| e.wants_sync_check())
                .map(|e| Arc::clone(&e.name))
                .collect();
            let mut scan = shell.cloud_scan(&path, &items);
            if scan.sync_root {
                // What the attributes call not synced, checked with the items themselves: a
                // placeholder can list as a plain item (#319).
                let plain = unsynced_by_attributes(&entries);
                let (not_synced, syncing) = if plain.is_empty() {
                    (plain, Vec::new())
                } else {
                    shell.cloud_verify(&path, plain)
                };
                scan.not_synced = not_synced;
                scan.syncing.extend(syncing);
                if first {
                    let counts = CloudCounts::of(entries.iter());
                    log::info!("cloud folder {} listed: {counts}", path.display());
                }
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
        if self.cloud.visit_rule.scanned(sync_root) {
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
    /// Tells the shell this pane shows `dir`, a folder inside a sync root.
    fn announce_visit(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let slot = cx.entity_id().as_u64();
        // Any earlier announcement of this pane ends first.
        self.cloud.visit = None;
        let (hint_tx, hint_dir) = (self.events_tx.clone(), dir.clone());
        let (select_tx, select_dir) = (self.events_tx.clone(), dir.clone());
        self.services.shell.folder_visit(
            slot,
            Some(crate::shell::FolderVisit {
                window: self.cloud.window,
                dir,
                changed: Box::new(move || {
                    let _ = hint_tx.send(AppEvent::CloudFolderHint(hint_dir.clone()));
                }),
                select: Box::new(move |names| {
                    let _ = select_tx.send(AppEvent::ShellSelected {
                        dir: select_dir.clone(),
                        names,
                    });
                }),
            }),
        );
        self.cloud.visit = Some(VisitGuard {
            shell: Arc::clone(&self.services.shell),
            slot,
        });
    }

    /// Whether the folder `dir` is shown and fully listed, with no navigation running.
    fn list_ready(&self, dir: &Path) -> bool {
        self.pending.is_none()
            && self
                .model
                .as_ref()
                .is_some_and(|m| m.complete && m.path == dir)
    }

    /// The shell hints that the items of `dir` changed (a sync client may have added placeholders
    /// without a watcher event): list it again, unless one is running already.
    pub(super) fn cloud_folder_hint(&mut self, dir: &Path) {
        if self.current_path() != Some(dir) {
            return;
        }
        let ready = self.list_ready(dir);
        if self.cloud.recheck.ask(ready) {
            self.start_recheck();
        } else if self.cloud.recheck == Recheck::Waiting {
            self.cloud.hints_waiting += 1;
        } else {
            self.cloud.hints_merged += 1;
        }
    }

    /// Lists the current folder again on a background thread and compares it with the list.
    fn start_recheck(&self) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let (generation, version) = (model.generation, model.version);
        let entries = Arc::clone(&model.entries);
        let fs = Arc::clone(&self.services.fs);
        let (dir, show_hidden, tx) = (model.path.clone(), self.show_hidden, self.events_tx.clone());
        spawn_thread("cloud-recheck", move || {
            let listed: Option<Vec<io::Result<Entry>>> =
                fs.read_dir(&dir).ok().map(Iterator::collect);
            let counts = listed.as_ref().map_or_default(|listed| {
                CloudCounts::of(
                    listed
                        .iter()
                        .flatten()
                        .filter(|e| !e.concealed(show_hidden)),
                )
            });
            let changed = listed.is_some_and(|listed| items_changed(&entries, listed, show_hidden));
            let _ = tx.send(AppEvent::CloudRechecked {
                dir,
                generation,
                version,
                changed,
                counts,
            });
        });
    }

    /// A recheck finished: reload when items came or went, and run the next one if hints came
    /// meanwhile. One log line says what it found and did.
    pub(super) fn cloud_rechecked(
        &mut self,
        dir: &Path,
        recheck: Rechecked,
        cx: &mut Context<Self>,
    ) {
        // Another folder since: its own state started over.
        if self.current_path() != Some(dir) {
            return;
        }
        let current = self
            .model
            .as_ref()
            .is_some_and(|m| m.generation == recheck.generation && m.version == recheck.version);
        let list = ListState::of(current, self.pending.is_none());
        let action = recheck_action(recheck.changed, list);
        log::info!(
            "cloud recheck of {}: on disk {}; changed {}, list moved {}: {action:?}; hints merged {}, waiting {}",
            dir.display(),
            recheck.counts,
            recheck.changed,
            !current,
            std::mem::take(&mut self.cloud.hints_merged),
            std::mem::take(&mut self.cloud.hints_waiting),
        );
        match action {
            RecheckAction::Reload => self.reload(cx),
            // Compared with a list that has changed since (a watcher delta, a re-sort) or is
            // being listed again: dropping the result would lose the hint that asked for it.
            RecheckAction::RunAgain => self.cloud.recheck.rerun(),
            RecheckAction::Nothing => {}
        }
        let ready = self.list_ready(dir);
        if self.cloud.recheck.finished(ready) {
            self.start_recheck();
        }
    }
}

/// What a finished recheck found ([`AppEvent::CloudRechecked`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rechecked {
    /// The list it compared with.
    pub(crate) generation: Gen,
    pub(crate) version: u64,
    /// Items came or went.
    pub(crate) changed: bool,
    /// What the folder holds now.
    pub(crate) counts: CloudCounts,
}

/// What a finished recheck leads to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecheckAction {
    /// Nothing came or went.
    Nothing,
    /// Items came or went and the list is the one compared with: list the folder again.
    Reload,
    /// Items came or went, but the list changed meanwhile or is being listed: compare again.
    RunAgain,
}

/// The list a finished recheck compared with, as it is when the result arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListState {
    /// Still the one compared with, and no listing or navigation is running.
    Same,
    /// It changed since (a watcher delta, a re-sort), or the folder is being listed again.
    Moved,
}

impl ListState {
    const fn of(current: bool, idle: bool) -> Self {
        if current && idle {
            Self::Same
        } else {
            Self::Moved
        }
    }
}

/// See [`RecheckAction`].
const fn recheck_action(changed: bool, list: ListState) -> RecheckAction {
    match (changed, list) {
        (false, _) => RecheckAction::Nothing,
        (true, ListState::Same) => RecheckAction::Reload,
        (true, ListState::Moved) => RecheckAction::RunAgain,
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
    let mut changed = false;
    for entry in listed {
        // A listing that fails part way says nothing, even about what it found before failing.
        let Ok(entry) = entry else {
            return false;
        };
        if entry.concealed(show_hidden) {
            continue;
        }
        changed |= known
            .get(&*entry.name)
            .is_none_or(|kind| **kind != entry.kind);
        seen += 1;
    }
    changed || seen != known.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A change the list can take reloads; one it can't runs again.
    #[test]
    fn a_recheck_reloads_or_runs_again() {
        use RecheckAction as A;
        let (same, moved) = (ListState::Same, ListState::Moved);
        assert_eq!(recheck_action(false, same), A::Nothing);
        assert_eq!(recheck_action(false, moved), A::Nothing);
        assert_eq!(recheck_action(true, same), A::Reload);
        assert_eq!(recheck_action(true, moved), A::RunAgain);
        // The list moved, or a listing is running: either way it can't take the change.
        assert_eq!(ListState::of(true, true), same);
        assert_eq!(ListState::of(false, true), moved, "list moved");
        assert_eq!(ListState::of(true, false), moved, "listing");
        assert_eq!(ListState::of(false, false), moved);
    }

    /// The log's counts: online-only, local placeholders, plain files and folders.
    #[test]
    fn cloud_counts_tell_placeholders_from_plain_items() {
        const REPARSE: u32 = 0x400;
        const RECALL: u32 = 0x40_0000;
        let entries = [
            Entry::new("online.docx", EntryKind::File, 1, None, REPARSE | RECALL),
            Entry::new("local.docx", EntryKind::File, 1, None, REPARSE),
            Entry::new("Synced", EntryKind::Dir, 0, None, REPARSE),
            Entry::new("plain.txt", EntryKind::File, 1, None, 0),
            Entry::new("Plain", EntryKind::Dir, 0, None, 0),
            Entry::new("Other", EntryKind::Dir, 0, None, 0),
        ];
        let counts = CloudCounts::of(entries.iter());
        assert_eq!(
            counts.to_string(),
            "6 items (online-only 1, local placeholders 2, plain files 1, plain folders 2)"
        );
        assert_eq!(CloudCounts::of([].iter()), CloudCounts::default());
    }

    /// Announced from the first scan that finds a sync root until the pane shows another folder;
    /// later scans (the list changed, F5) don't announce again, coming back does.
    #[test]
    fn a_folder_is_announced_while_the_pane_shows_it() {
        let mut rule = VisitRule::default();
        assert!(!rule.folder_changed(), "nothing announced yet");
        assert!(rule.scanned(true));
        assert!(!rule.scanned(true), "a rescan of the same folder");
        assert!(rule.folder_changed(), "leaving ends it");
        assert!(!rule.scanned(false), "a folder outside sync roots");
        assert!(
            !rule.scanned(true),
            "only the first scan of a folder counts"
        );
        assert!(!rule.folder_changed(), "nothing to end");
        assert!(rule.scanned(true), "coming back announces again");
    }

    /// One listing at a time; hints meanwhile make exactly one more; hints while the list isn't
    /// ready wait for it.
    #[test]
    fn rechecks_run_one_at_a_time() {
        let mut r = Recheck::default();
        assert!(r.ask(true));
        assert!(!r.ask(true));
        assert!(!r.ask(true));
        assert!(r.finished(true), "one more for the hints meanwhile");
        assert!(!r.finished(true));
        assert_eq!(r, Recheck::Idle);

        assert!(!r.ask(false), "the list isn't ready");
        assert!(!r.ready(false));
        assert!(r.ready(true));
        assert!(!r.ready(true), "started once");
        assert!(!r.ask(true));
        assert!(!r.finished(false), "asked again, but a reload is running");
        assert_eq!(r, Recheck::Waiting);
        assert!(r.ready(true));
        assert!(!r.finished(true));
        assert!(!r.ready(true));
        assert_eq!(r, Recheck::Idle);
    }

    /// A listing that found a change the list couldn't take (it moved meanwhile) runs once more,
    /// also when no hint came meanwhile; with hints meanwhile, still only once more.
    #[test]
    fn a_change_found_against_a_moved_list_runs_again() {
        let mut r = Recheck::default();
        assert!(r.ask(true));
        r.rerun();
        assert!(r.finished(true), "once more");
        assert!(!r.finished(true));
        assert_eq!(r, Recheck::Idle);

        assert!(r.ask(true));
        assert!(!r.ask(true));
        r.rerun();
        assert!(r.finished(true), "once more for both");
        assert!(!r.finished(true));

        assert!(r.ask(true));
        r.rerun();
        assert!(!r.finished(false), "a reload is running");
        assert_eq!(r, Recheck::Waiting);
        assert!(r.ready(true));

        let mut idle = Recheck::default();
        idle.rerun();
        assert_eq!(idle, Recheck::Idle, "nothing running, nothing to repeat");
    }

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
