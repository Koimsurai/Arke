//! Operation queue: copies, moves and deletes that touch the same physical drive run one after
//! another instead of all at once (several jobs fighting over one disk are slower together than
//! in turn, especially on hard disks and USB sticks). Jobs on different drives still run side by
//! side. Turned off in settings, a job only waits while the queue is paused.
//!
//! A job's background thread calls [`enter`] before starting and holds the returned [`Turn`] until
//! it is done. While it waits, its progress says `queued` (the progress center shows "Queued"),
//! and Cancel takes it out of the line without doing anything.
//!
//! Pausing holds jobs that haven't started yet. Windows' copy engine can't pause a job halfway, so
//! a running job finishes first; the progress center says so.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::shell::{OpProgress, Shell};

/// How often a waiting job checks for Cancel even without a wake-up.
const POLL: Duration = Duration::from_millis(200);

struct Waiter {
    ticket: u64,
    keys: Vec<String>,
    /// Wait for earlier jobs on the same drives (the setting, read when the job started).
    serialize: bool,
}

#[derive(Default)]
struct State {
    paused: bool,
    next_ticket: u64,
    /// Not started yet, in arrival order.
    waiting: Vec<Waiter>,
    /// Running jobs and their drive keys.
    running: Vec<(u64, Vec<String>)>,
}

impl State {
    /// Whether the job with this ticket may start now: not paused, and (when it serializes) no
    /// running job and no job that arrived earlier shares a drive with it. Waiting behind an
    /// earlier job keeps the order fair, so a job on two drives can't be starved.
    fn may_start(&self, ticket: u64) -> bool {
        if self.paused {
            return false;
        }
        let Some(pos) = self.waiting.iter().position(|w| w.ticket == ticket) else {
            return false;
        };
        let me = &self.waiting[pos];
        if !me.serialize {
            return true;
        }
        let clash = |keys: &[String]| keys.iter().any(|k| me.keys.contains(k));
        !self.running.iter().any(|(_, keys)| clash(keys))
            && !self.waiting[..pos].iter().any(|w| clash(&w.keys))
    }

    fn take_waiter(&mut self, ticket: u64) -> Option<Waiter> {
        let pos = self.waiting.iter().position(|w| w.ticket == ticket)?;
        Some(self.waiting.remove(pos))
    }
}

/// The queue. One per process ([`enter`] and friends); tests make their own.
#[derive(Default)]
pub(crate) struct Queue {
    state: Mutex<State>,
    changed: Condvar,
}

/// A job's turn; dropping it lets the next job on its drives start.
pub(crate) struct Turn<'a> {
    queue: &'a Queue,
    ticket: u64,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.queue
            .state
            .lock()
            .running
            .retain(|(t, _)| *t != self.ticket);
        self.queue.changed.notify_all();
    }
}

impl Queue {
    /// Waits until this job may start (see [`State::may_start`]). `None` if it was cancelled while
    /// waiting. Background thread.
    pub(crate) fn enter(
        &self,
        keys: Vec<String>,
        serialize: bool,
        progress: &OpProgress,
    ) -> Option<Turn<'_>> {
        let mut state = self.state.lock();
        state.next_ticket += 1;
        let ticket = state.next_ticket;
        state.waiting.push(Waiter {
            ticket,
            keys,
            serialize,
        });
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                state.take_waiter(ticket);
                drop(state);
                progress.queued.store(false, Ordering::Relaxed);
                // Jobs behind this one may be free now.
                self.changed.notify_all();
                return None;
            }
            if state.may_start(ticket) {
                let me = state.take_waiter(ticket)?;
                state.running.push((ticket, me.keys));
                drop(state);
                progress.queued.store(false, Ordering::Relaxed);
                self.changed.notify_all();
                return Some(Turn {
                    queue: self,
                    ticket,
                });
            }
            progress.queued.store(true, Ordering::Relaxed);
            self.changed.wait_for(&mut state, POLL);
        }
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.state.lock().paused = paused;
        self.changed.notify_all();
    }

    pub(crate) fn paused(&self) -> bool {
        self.state.lock().paused
    }

    /// Something changed for a waiting job (Cancel pressed): check now instead of at the next poll.
    pub(crate) fn wake(&self) {
        self.changed.notify_all();
    }
}

fn queue() -> &'static Queue {
    static QUEUE: LazyLock<Queue> = LazyLock::new(Queue::default);
    &QUEUE
}

/// [`Queue::enter`] on the process queue.
pub(crate) fn enter(
    keys: Vec<String>,
    serialize: bool,
    progress: &OpProgress,
) -> Option<Turn<'static>> {
    queue().enter(keys, serialize, progress)
}

/// Holds (or releases) jobs that haven't started.
pub(crate) fn set_paused(paused: bool) {
    queue().set_paused(paused);
}

pub(crate) fn paused() -> bool {
    queue().paused()
}

/// See [`Queue::wake`].
pub(crate) fn wake() {
    queue().wake();
}

/// Physical drives the paths are on, one key each ([`Shell::drive_key`], asked once per volume).
/// Background thread: the real shell asks the drive for its disk number.
pub(crate) fn drive_keys<'a>(
    shell: &dyn Shell,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Vec<String> {
    let mut volumes: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for path in paths {
        // Server paths have no volume: their connection stands in for it.
        let volume = arke_core::watches::volume_key(path)
            .or_else(|| arke_core::remote::RemoteUrl::from_path(path).map(|url| url.origin()))
            .unwrap_or_default();
        if volumes.contains(&volume) {
            continue;
        }
        volumes.push(volume);
        let key = shell.drive_key(path);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Drives an undo touches: where items are now and where they go back to.
pub(crate) fn undo_paths(undo: &crate::shell::Undo) -> Vec<PathBuf> {
    match undo {
        crate::shell::Undo::PutBack(items) => items
            .iter()
            .flat_map(|i| [PathBuf::from(&i.now), i.parent.clone()])
            .collect(),
        crate::shell::Undo::Remove(paths) => paths.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn waiter(ticket: u64, keys: &[&str]) -> Waiter {
        Waiter {
            ticket,
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
            serialize: true,
        }
    }

    #[test]
    fn same_drive_waits_other_drives_run() {
        let state = State {
            running: vec![(1, vec!["disk1".into()])],
            waiting: vec![waiter(2, &["disk1"]), waiter(3, &["disk2"])],
            ..State::default()
        };
        assert!(!state.may_start(2), "disk1 is busy");
        assert!(state.may_start(3), "disk2 is free");
    }

    #[test]
    fn later_jobs_keep_their_place_in_line() {
        // Job 2 needs disk1 and disk2; job 3 only disk2. Job 3 must not jump ahead of 2.
        let state = State {
            running: vec![(1, vec!["disk1".into()])],
            waiting: vec![waiter(2, &["disk1", "disk2"]), waiter(3, &["disk2"])],
            ..State::default()
        };
        assert!(!state.may_start(3));
    }

    #[test]
    fn pause_holds_everything_and_the_setting_off_runs_side_by_side() {
        let mut state = State {
            running: vec![(1, vec!["disk1".into()])],
            waiting: vec![Waiter {
                serialize: false,
                ..waiter(2, &["disk1"])
            }],
            ..State::default()
        };
        assert!(
            state.may_start(2),
            "queue off: no waiting for the same drive"
        );
        state.paused = true;
        assert!(!state.may_start(2), "paused: nothing new starts");
    }

    #[test]
    fn a_queued_job_starts_when_the_first_finishes_and_can_be_cancelled() {
        let queue = Arc::new(Queue::default());
        let first = OpProgress::default();
        let turn = queue.enter(vec!["disk1".into()], true, &first);
        assert!(turn.is_some());

        let second = Arc::new(OpProgress::default());
        let started = {
            let (queue, second) = (queue.clone(), second.clone());
            std::thread::spawn(move || queue.enter(vec!["disk1".into()], true, &second).is_some())
        };
        while !second.queued.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        drop(turn);
        assert!(started.join().unwrap(), "runs once the first is done");
        assert!(!second.queued.load(Ordering::Relaxed));

        // Cancelled while waiting: leaves the line without running.
        let holder = OpProgress::default();
        let _held = queue.enter(vec!["disk1".into()], true, &holder);
        let third = Arc::new(OpProgress::default());
        let cancelled = {
            let (queue, third) = (queue.clone(), third.clone());
            std::thread::spawn(move || queue.enter(vec!["disk1".into()], true, &third).is_none())
        };
        while !third.queued.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        third.cancel.store(true, Ordering::Relaxed);
        queue.wake();
        assert!(cancelled.join().unwrap());
    }

    #[test]
    fn paused_jobs_wait_until_resumed() {
        let queue = Arc::new(Queue::default());
        queue.set_paused(true);
        let job = Arc::new(OpProgress::default());
        let started = {
            let (queue, job) = (queue.clone(), job.clone());
            std::thread::spawn(move || queue.enter(vec!["disk9".into()], true, &job).is_some())
        };
        while !job.queued.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        queue.set_paused(false);
        assert!(started.join().unwrap());
    }
}
