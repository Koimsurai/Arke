//! Background load queue that favors visible items (shared by thumbnails and system icons).
//!
//! - Each frame the UI calls [`Loader::set_wanted`] with what it needs, in priority order
//!   (visible first). Requests no longer wanted are dropped from the queue, so we never
//!   decode images that have scrolled away.
//! - Several workers, so one stuck item (e.g. a file on a dead drive) doesn't block the rest.
//! - Items in progress are not queued again. `work` delivers results itself (usually a channel).
//! - Items done but not yet received by the UI are not queued again either; the UI calls
//!   [`Loader::acknowledge`] once it takes them. Otherwise a wanted-list change in between
//!   would do the same item twice.

use parking_lot::{Condvar, Mutex};
use std::collections::{HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Arc;

struct State<K> {
    queue: VecDeque<K>,
    wanted: HashSet<K>,
    in_flight: HashSet<K>,
    /// Done, waiting for the UI to take it.
    delivered: HashSet<K>,
    shutdown: bool,
}

struct Shared<K> {
    state: Mutex<State<K>>,
    ready: Condvar,
}

/// Priority load queue served by a pool of worker threads; dropping it stops the workers.
pub struct Loader<K> {
    shared: Arc<Shared<K>>,
}

impl<K> Loader<K>
where
    K: Clone + Eq + Hash + Send + 'static,
{
    /// Spawns `threads` workers. `work` runs on a background thread and may block.
    pub fn new(name: &str, threads: usize, work: impl Fn(&K) + Send + Sync + 'static) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                wanted: HashSet::new(),
                in_flight: HashSet::new(),
                delivered: HashSet::new(),
                shutdown: false,
            }),
            ready: Condvar::new(),
        });
        let work = Arc::new(work);
        for i in 0..threads.max(1) {
            let shared = shared.clone();
            let work = work.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || worker(&shared, &*work));
            if let Err(err) = spawned {
                log::error!("failed to spawn {name} worker thread: {err}");
            }
        }
        Self { shared }
    }

    /// Replaces the wanted items (in priority order). Returns immediately.
    pub fn set_wanted(&self, keys: Vec<K>) {
        let mut state = self.shared.state.lock();
        state.wanted = keys.iter().cloned().collect();
        let queue: VecDeque<K> = keys
            .into_iter()
            .filter(|k| !state.in_flight.contains(k) && !state.delivered.contains(k))
            .collect();
        state.queue = queue;
        drop(state);
        self.shared.ready.notify_all();
    }

    /// The UI took the result for `key`; wanting it again later (e.g. after cache eviction) redoes
    /// it.
    pub fn acknowledge(&self, key: &K) {
        self.shared.state.lock().delivered.remove(key);
    }

    /// Number of queued items not yet started.
    pub fn queued(&self) -> usize {
        self.shared.state.lock().queue.len()
    }
}

impl<K> Drop for Loader<K> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.shutdown = true;
        state.queue.clear();
        drop(state);
        // No join: a stuck worker must not make drop hang too. Workers exit after their current
        // item.
        self.shared.ready.notify_all();
    }
}

fn worker<K: Clone + Eq + Hash>(shared: &Shared<K>, work: &(dyn Fn(&K) + Send + Sync)) {
    loop {
        let key = {
            let mut state = shared.state.lock();
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(key) = state.queue.pop_front() {
                    if state.wanted.contains(&key)
                        && !state.in_flight.contains(&key)
                        && !state.delivered.contains(&key)
                    {
                        state.in_flight.insert(key.clone());
                        drop(state);
                        break key;
                    }
                    continue;
                }
                shared.ready.wait(&mut state);
            }
        };
        // A panic in `work` fails only this item; the thread survives.
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&key))).is_ok();
        let mut state = shared.state.lock();
        state.in_flight.remove(&key);
        if ok {
            state.delivered.insert(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn recorder() -> (Arc<Mutex<Vec<u32>>>, impl Fn(&u32) + Send + Sync + 'static) {
        let done = Arc::new(Mutex::new(Vec::new()));
        let d = done.clone();
        (done, move |k: &u32| d.lock().push(*k))
    }

    fn wait_for(pred: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pred() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn processes_in_priority_order_and_drops_unwanted() {
        // A gate keeps the single worker stuck on the first item.
        let (gate_tx, gate_rx) = flume::unbounded::<()>();
        let (done, record) = recorder();
        let loader = Loader::new("t", 1, move |k: &u32| {
            if *k == 0 {
                let _ = gate_rx.recv();
            }
            record(k);
        });
        loader.set_wanted(vec![0]);
        wait_for(|| loader.queued() == 0);
        // While stuck: want 1..=5, then scroll so only 9 and 4 remain (visible first).
        loader.set_wanted(vec![0, 1, 2, 3, 4, 5]);
        loader.set_wanted(vec![9, 4]);
        gate_tx.send(()).unwrap();
        wait_for(|| done.lock().len() == 3);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(*done.lock(), [0, 9, 4]);
    }

    #[test]
    fn hung_item_does_not_block_other_workers() {
        let (_gate_tx, gate_rx) = flume::unbounded::<()>();
        let (done, record) = recorder();
        let loader = Loader::new("t", 2, move |k: &u32| {
            if *k == 666 {
                let _ = gate_rx.recv(); // never arrives
            }
            record(k);
        });
        loader.set_wanted(vec![666, 1, 2, 3]);
        wait_for(|| done.lock().len() == 3);
        // The stuck item is still in progress and is not queued again.
        loader.set_wanted(vec![666, 4]);
        wait_for(|| done.lock().len() == 4);
        assert_eq!(loader.queued(), 0);
    }

    #[test]
    fn delivered_items_wait_for_acknowledge() {
        let (done, record) = recorder();
        let loader = Loader::new("t", 1, record);
        loader.set_wanted(vec![7]);
        wait_for(|| done.lock().len() == 1);
        // Result not taken yet: wanting it again does not redo it.
        loader.set_wanted(vec![8, 7]);
        wait_for(|| done.lock().len() == 2);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(*done.lock(), [7, 8]);
        // After acknowledging (e.g. evicted from cache), wanting it again redoes it.
        loader.acknowledge(&7);
        loader.set_wanted(vec![7]);
        wait_for(|| done.lock().len() == 3);
    }

    #[test]
    fn panicking_work_only_fails_that_item() {
        let (done, record) = recorder();
        let loader = Loader::new("t", 1, move |k: &u32| {
            assert!(*k != 1, "broken image");
            record(k);
        });
        loader.set_wanted(vec![1, 2]);
        wait_for(|| done.lock().len() == 1);
        assert_eq!(*done.lock(), [2]);
    }
}
