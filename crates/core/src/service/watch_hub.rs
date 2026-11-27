//! The one thread (`watch-batch`) that turns the change notifications of every watched folder
//! into deltas.
//!
//! Each watched folder used to have a thread of its own, asleep until the folder changed. Here
//! they share one: it gathers each folder's changes until they pause for [`WATCH_DEBOUNCE`] (or
//! for at most [`WATCH_MAX_LATENCY`] while they keep coming) and sends the delta. Changes the
//! notification described ([`Change::Present`], [`Change::Removed`]) need no I/O, so their deltas
//! are made right here; a batch with anything to stat goes to a short-lived thread of its own
//! (`watch-stat-N`), so a file server that doesn't answer holds up only its own folder. A
//! folder's next batch waits until the one being stat'ed is sent, so deltas stay in order.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{IoEvent, Job, WATCH_DEBOUNCE, WATCH_MAX_BATCH, WATCH_MAX_LATENCY};
use crate::fs::{Change, ChangeEvent};

/// Identifies one watch to the hub.
pub(super) type WatchId = u64;

/// What the hub is told.
pub(super) enum Msg {
    /// A folder is about to be watched (its OS watch is opening on another thread, so changes
    /// can come before [`Msg::Armed`]).
    Add(WatchId, Box<Watched>),
    /// The OS watch is open; dropping this stops it.
    Armed(WatchId, Box<dyn Send>),
    /// Changes in the folder.
    Event(WatchId, ChangeEvent),
    /// The folder was left, or its OS watch couldn't be opened.
    Stop(WatchId),
    /// The batch that was being stat'ed has been sent.
    Flushed(WatchId),
}

/// A new watch's id.
pub(super) fn next_id() -> WatchId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The hub, started on first use. `None` if its thread couldn't be started.
pub(super) fn hub() -> Option<&'static flume::Sender<Msg>> {
    static HUB: OnceLock<Option<flume::Sender<Msg>>> = OnceLock::new();
    HUB.get_or_init(|| {
        let (tx, rx) = flume::unbounded();
        let back = tx.clone();
        match std::thread::Builder::new()
            .name("watch-batch".into())
            .spawn(move || run(&rx, &back))
        {
            Ok(_) => Some(tx),
            Err(err) => {
                log::error!("cannot start the folder watch thread: {err}");
                None
            }
        }
    })
    .as_ref()
}

/// One watched folder, as the hub sees it.
pub(super) struct Watched {
    job: Job,
    dir: PathBuf,
    show_hidden: bool,
    /// The OS watch, once open.
    #[allow(dead_code, reason = "held only so the OS watch stays open")]
    guard: Option<Box<dyn Send>>,
    /// Names changed since the last batch, with the latest change of each.
    pending: HashMap<OsString, Change>,
    first_pending: Option<Instant>,
    last_event: Instant,
    rescan: bool,
    /// A batch is being stat'ed on another thread; the next one waits for it.
    busy: bool,
}

impl Watched {
    pub(super) fn new(job: Job, dir: PathBuf, show_hidden: bool) -> Self {
        Self {
            job,
            dir,
            show_hidden,
            guard: None,
            pending: HashMap::new(),
            first_pending: None,
            last_event: Instant::now(),
            rescan: false,
            busy: false,
        }
    }

    /// When the pending batch is due; `None` if there is none, or one is still being stat'ed.
    fn due(&self) -> Option<Instant> {
        if self.busy {
            return None;
        }
        let first = self.first_pending?;
        Some((self.last_event + WATCH_DEBOUNCE).min(first + WATCH_MAX_LATENCY))
    }

    fn record(&mut self, event: ChangeEvent) {
        match event {
            ChangeEvent::Changed(changes) => {
                for change in changes {
                    let path = change.path();
                    if path.parent() == Some(self.dir.as_path())
                        && let Some(name) = path.file_name()
                    {
                        self.pending.insert(name.to_os_string(), change);
                    }
                }
            }
            ChangeEvent::Rescan => self.rescan = true,
        }
        self.last_event = Instant::now();
        self.first_pending.get_or_insert(self.last_event);
    }

    /// Sends the pending batch: a relist if changes were lost or there are too many, else the
    /// delta, made here or (when something has to be stat'ed) on a thread of its own.
    fn flush(&mut self, id: WatchId, back: &flume::Sender<Msg>) {
        self.first_pending = None;
        if self.rescan || self.pending.len() > WATCH_MAX_BATCH {
            self.pending.clear();
            self.rescan = false;
            self.job.send(IoEvent::NeedsRescan {
                generation: self.job.generation,
            });
            return;
        }
        let changes: Vec<Change> = self.pending.drain().map(|(_, change)| change).collect();
        if !changes.iter().any(|c| matches!(c, Change::Unknown(_))) {
            self.job.send_delta(&self.dir, changes, self.show_hidden);
            return;
        }
        self.busy = true;
        let job = self.job.clone();
        let dir = self.dir.clone();
        let show_hidden = self.show_hidden;
        let back = back.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("watch-stat-{}", job.generation))
            .spawn(move || {
                job.send_delta(&dir, changes, show_hidden);
                let _ = back.send(Msg::Flushed(id));
            });
        if let Err(err) = spawned {
            // The changes went with the thread that didn't start: relist instead.
            log::warn!("cannot look up changes in {}: {err}", self.dir.display());
            self.busy = false;
            self.job.send(IoEvent::NeedsRescan {
                generation: self.job.generation,
            });
        }
    }
}

/// The hub's thread: sleeps until a message comes or a batch is due, forever.
fn run(rx: &flume::Receiver<Msg>, back: &flume::Sender<Msg>) {
    let mut folders: HashMap<WatchId, Watched> = HashMap::new();
    loop {
        let due = folders.values().filter_map(Watched::due).min();
        let msg = match due {
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(flume::RecvError::Disconnected) => return,
            },
            Some(due) => match rx.recv_deadline(due) {
                Ok(msg) => Some(msg),
                Err(flume::RecvTimeoutError::Timeout) => None,
                Err(flume::RecvTimeoutError::Disconnected) => return,
            },
        };
        let now = Instant::now();
        match msg {
            Some(msg) => handle(&mut folders, msg),
            None => {
                for watched in folders.values() {
                    if watched.due().is_some_and(|due| due <= now) {
                        watched.job.woke();
                    }
                }
            }
        }
        // Folders left since are dropped (and their OS watches stopped); due batches go out.
        folders.retain(|_, watched| !watched.job.is_stale());
        for (id, watched) in &mut folders {
            if watched.due().is_some_and(|due| due <= now) {
                watched.flush(*id, back);
            }
        }
    }
}

fn handle(folders: &mut HashMap<WatchId, Watched>, msg: Msg) {
    match msg {
        Msg::Add(id, watched) => {
            // Left before the watch got here: its stop may have come first.
            if !watched.job.is_stale() {
                folders.insert(id, *watched);
            }
        }
        Msg::Armed(id, guard) => {
            if let Some(watched) = folders.get_mut(&id) {
                watched.guard = Some(guard);
            }
        }
        Msg::Event(id, event) => {
            if let Some(watched) = folders.get_mut(&id) {
                watched.job.woke();
                watched.record(event);
            }
        }
        Msg::Stop(id) => {
            folders.remove(&id);
        }
        Msg::Flushed(id) => {
            if let Some(watched) = folders.get_mut(&id) {
                watched.job.woke();
                watched.busy = false;
            }
        }
    }
}
