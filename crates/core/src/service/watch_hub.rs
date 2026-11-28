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
//!
//! Relists asked for by a folder's watch (lost changes) go out at most once per [`RELIST_EVERY`]
//! for that folder ([`Relists`]): a file server that reports a change whenever its folder is read
//! (#536) would otherwise have it relisted over and over, as fast as it answers. A relist asked
//! for too soon is only delayed, never dropped, and deltas aren't held up.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{IoEvent, Job, WATCH_DEBOUNCE, WATCH_MAX_BATCH, WATCH_MAX_LATENCY};
use crate::fs::{Change, ChangeEvent};

/// A folder's watch asks for a relist at most this often; one asked for sooner waits until then.
const RELIST_EVERY: Duration = Duration::from_secs(1);

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
    /// When the relist asked for goes out, if it has to wait ([`RELIST_EVERY`]).
    rescan_at: Option<Instant>,
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
            rescan_at: None,
            busy: false,
        }
    }

    /// When the pending batch (or a relist that waits) is due; `None` if there is none, or one is
    /// still being stat'ed.
    fn due(&self) -> Option<Instant> {
        if self.busy {
            return None;
        }
        let batch = self
            .first_pending
            .map(|first| (self.last_event + WATCH_DEBOUNCE).min(first + WATCH_MAX_LATENCY));
        match (batch, self.rescan_at) {
            (Some(batch), Some(relist)) => Some(batch.min(relist)),
            (batch, relist) => batch.or(relist),
        }
    }

    fn record(&mut self, event: ChangeEvent, now: Instant) {
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
        self.last_event = now;
        self.first_pending.get_or_insert(now);
    }

    /// Sends the pending batch: a relist if changes were lost or there are too many (unless the
    /// folder was relisted less than [`RELIST_EVERY`] ago: then it waits, and the changes go out
    /// as a delta meanwhile), else the delta, made here or (when something has to be stat'ed) on
    /// a thread of its own.
    fn flush(
        &mut self,
        id: WatchId,
        back: &flume::Sender<Msg>,
        now: Instant,
        relists: &mut Relists,
    ) {
        self.first_pending = None;
        if self.pending.len() > WATCH_MAX_BATCH {
            self.pending.clear();
            self.rescan = true;
        }
        if self.rescan {
            let at = match self.rescan_at {
                Some(at) => at,
                None => relists.ask(&self.dir, now),
            };
            if at <= now {
                self.pending.clear();
                self.rescan = false;
                self.rescan_at = None;
                relists.sent(&self.dir, now);
                self.job.send(IoEvent::NeedsRescan {
                    generation: self.job.generation,
                });
                return;
            }
            self.rescan_at = Some(at);
        }
        if self.pending.is_empty() {
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

/// When each folder was last relisted for its watch, while that is recent: across watches, since
/// every relist opens a new one. Only folders relisted in the last two [`RELIST_EVERY`]s are
/// kept, so with nothing being relisted this is empty and costs nothing.
#[derive(Default)]
struct Relists(HashMap<PathBuf, Relisted>);

/// The latest relist of one folder.
struct Relisted {
    at: Instant,
    /// Relists asked for in a row, each less than two [`RELIST_EVERY`]s after the one before.
    asked: u32,
    /// Whether it was logged that this folder's relists wait.
    logged: bool,
}

impl Relists {
    /// A relist of `dir` is asked for at `now`: when it may go out (`now`, or [`RELIST_EVERY`]
    /// after the last one). The first time one has to wait is logged, so a reporter's log shows
    /// a folder that keeps asking.
    fn ask(&mut self, dir: &Path, now: Instant) -> Instant {
        let Some(last) = self.0.get_mut(dir) else {
            return now;
        };
        last.asked = last.asked.saturating_add(1);
        let at = last.at + RELIST_EVERY;
        if at <= now {
            return now;
        }
        if !last.logged {
            last.logged = true;
            log::info!(
                "relisting {} at most once a second: {} rescans in a row",
                dir.display(),
                last.asked
            );
        }
        at
    }

    /// The relist of `dir` went out at `now`.
    fn sent(&mut self, dir: &Path, now: Instant) {
        self.0.retain(|_, last| now < last.at + 2 * RELIST_EVERY);
        let last = self.0.entry(dir.to_path_buf()).or_insert(Relisted {
            at: now,
            asked: 1,
            logged: false,
        });
        last.at = now;
    }
}

/// The hub's thread: sleeps until a message comes or a batch is due, forever.
fn run(rx: &flume::Receiver<Msg>, back: &flume::Sender<Msg>) {
    let mut folders: HashMap<WatchId, Watched> = HashMap::new();
    let mut relists = Relists::default();
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
            Some(msg) => handle(&mut folders, msg, now),
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
                watched.flush(*id, back, now, &mut relists);
            }
        }
    }
}

fn handle(folders: &mut HashMap<WatchId, Watched>, msg: Msg, now: Instant) {
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
                watched.record(event, now);
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

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;

    type Events = Arc<Mutex<Vec<IoEvent>>>;

    /// A watch of `dir` as the service makes it, with its events collected.
    fn watch_of(dir: &str) -> (Watched, Events) {
        let events: Events = Arc::default();
        let sink = Arc::clone(&events);
        let job = Job {
            generation: 1,
            fs: crate::fake::FakeFs::new(),
            current: Arc::new(AtomicU64::new(1)),
            sink: Arc::new(move |event: IoEvent| sink.lock().push(event)),
            probe: None,
            cache: Arc::default(),
            recheck: Arc::default(),
            watch_stop: Arc::default(),
            wakeups: Arc::default(),
            outcome: Arc::new(AtomicU8::new(0)),
        };
        (Watched::new(job, PathBuf::from(dir), true), events)
    }

    /// Flushes `watched` if its batch is due at `now`, as the hub's thread does.
    fn step(watched: &mut Watched, relists: &mut Relists, now: Instant) {
        let (back, _) = flume::unbounded();
        if watched.due().is_some_and(|due| due <= now) {
            watched.flush(1, &back, now, relists);
        }
    }

    fn relists_sent(events: &Events) -> usize {
        events
            .lock()
            .iter()
            .filter(|e| matches!(e, IoEvent::NeedsRescan { .. }))
            .count()
    }

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A folder whose watch asks for a relist after every listing (#536): ten requests within a
    /// second relist it at once and once more at the end of the second, not ten times; a request
    /// a second later relists it at once. Each relist opens a new watch, as the service does, and
    /// the throttle holds across them.
    #[test]
    fn relists_of_one_folder_go_out_at_most_once_a_second() {
        let mut relists = Relists::default();
        let start = Instant::now();
        let (mut first, first_events) = watch_of(r"O:\");
        first.record(ChangeEvent::Rescan, start);
        step(&mut first, &mut relists, start + WATCH_DEBOUNCE);
        assert_eq!(
            relists_sent(&first_events),
            1,
            "the first relist goes at once"
        );
        assert_eq!(first.due(), None, "nothing left to do");

        // The relist opened a new watch, which keeps asking.
        let (mut second, events) = watch_of(r"O:\");
        for i in 1..10 {
            let at = start + ms(90 * i);
            second.record(ChangeEvent::Rescan, at);
            step(&mut second, &mut relists, at + WATCH_DEBOUNCE);
        }
        assert_eq!(relists_sent(&events), 0, "held back");
        let trailing = start + WATCH_DEBOUNCE + RELIST_EVERY;
        assert_eq!(
            second.due(),
            Some(trailing),
            "one relist waits, at the end of the second"
        );
        let just_before = trailing
            .checked_sub(ms(1))
            .expect("a second after the start");
        step(&mut second, &mut relists, just_before);
        assert_eq!(relists_sent(&events), 0);
        step(&mut second, &mut relists, trailing);
        assert_eq!(
            relists_sent(&events),
            1,
            "the requests went out as one relist"
        );
        assert_eq!(second.due(), None);

        // A second after the last relist: at once.
        let (mut third, events) = watch_of(r"O:\");
        let later = trailing + RELIST_EVERY;
        third.record(ChangeEvent::Rescan, later);
        step(&mut third, &mut relists, later + WATCH_DEBOUNCE);
        assert_eq!(relists_sent(&events), 1);

        // Another folder isn't held back by this one.
        let (mut other, events) = watch_of(r"C:\w");
        other.record(ChangeEvent::Rescan, later);
        step(&mut other, &mut relists, later + WATCH_DEBOUNCE);
        assert_eq!(relists_sent(&events), 1);
    }

    /// While a relist waits, changes of single items still go out as deltas right away; the
    /// relist follows at the end of the second. With nothing being relisted, deltas are as
    /// before and nothing is remembered.
    #[test]
    fn deltas_are_not_held_back() {
        let mut relists = Relists::default();
        let start = Instant::now();
        let (mut watched, events) = watch_of(r"O:\");
        watched.record(
            ChangeEvent::Changed(vec![Change::Removed(PathBuf::from(r"O:\a.txt"))]),
            start,
        );
        step(&mut watched, &mut relists, start + WATCH_DEBOUNCE);
        assert!(matches!(&events.lock()[..], [IoEvent::Delta { .. }]));
        assert!(relists.0.is_empty(), "no relist, nothing remembered");

        let relisted = start + ms(200);
        watched.record(ChangeEvent::Rescan, relisted);
        step(&mut watched, &mut relists, relisted + WATCH_DEBOUNCE);
        assert_eq!(relists_sent(&events), 1);

        let asked = relisted + ms(300);
        watched.record(ChangeEvent::Rescan, asked);
        step(&mut watched, &mut relists, asked + WATCH_DEBOUNCE);
        let changed = asked + ms(200);
        watched.record(
            ChangeEvent::Changed(vec![Change::Removed(PathBuf::from(r"O:\b.txt"))]),
            changed,
        );
        step(&mut watched, &mut relists, changed + WATCH_DEBOUNCE);
        let seen = events.lock();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(
            matches!(&seen[2], IoEvent::Delta { delta, .. } if &*delta.removed[0] == "b.txt"),
            "{seen:?}"
        );
        drop(seen);
        let trailing = relisted + WATCH_DEBOUNCE + RELIST_EVERY;
        assert_eq!(watched.due(), Some(trailing));
        step(&mut watched, &mut relists, trailing);
        assert_eq!(relists_sent(&events), 2);

        // Long after: forgotten when the next relist goes out.
        let quiet = trailing + 10 * RELIST_EVERY;
        let (mut other, _) = watch_of(r"C:\w");
        other.record(ChangeEvent::Rescan, quiet);
        step(&mut other, &mut relists, quiet + WATCH_DEBOUNCE);
        assert_eq!(relists.0.len(), 1, "only the folder just relisted");
    }
}
