//! How much each Recycle Bin holds, kept in memory for the fast Recycle Bin (#379).
//!
//! Windows' `SHQueryRecycleBinW` lists the whole bin and reads every `$I` record each time it is
//! called: with tens of thousands of items in the bin, a delete waited tens of seconds for it.
//! Here a background thread per bin folder (`X:\$Recycle.Bin\<SID>`) sums the sizes the `$I`
//! records hold once, then keeps the sum up to date from change notifications on the folder. It
//! sleeps until the folder changes (no polling), and a delete only reads the number.
//!
//! The first sum opens every record, which real-time antivirus may scan. What it read is kept in
//! `%LOCALAPPDATA%\Arke\bin-usage\`, matched to the records by name, creation time and length
//! (all in the folder listing, no file opened), so later starts read only the records added since.
//! A record is written once and never changed, so those three tell it apart.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use arke_core::EntryKind;
use arke_core::fs::{Fs, WatchEvent};
use parking_lot::{Condvar, Mutex};

use crate::win_fast_recycle::{HEAD_LEN, Head, VERSION, head};

/// A scan that read at least this many records saves what it knows for the next start.
const SAVE_AFTER_READS: usize = 64;

/// The tracker for the real bins.
pub(crate) fn tracker() -> &'static Tracker {
    static TRACKER: LazyLock<Tracker> =
        LazyLock::new(|| Tracker::new(Arc::new(arke_core::RealFs), cache_dir()));
    &TRACKER
}

/// `%LOCALAPPDATA%\Arke\bin-usage`; nothing is kept from tests.
fn cache_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::paths::bin_usage_dir()
}

/// What tells a record apart from another one under the same name, from a listing alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// Creation time, ns since 1970; [`Self::UNKNOWN`] when not known.
    created: u64,
    len: u64,
}

impl Stamp {
    const UNKNOWN: u64 = u64::MAX;

    fn new(created: Option<SystemTime>, len: u64) -> Self {
        let created = created
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok())
            .unwrap_or(Self::UNKNOWN);
        Self { created, len }
    }

    /// The stamp of a record from its metadata (as the fast path has it after writing one).
    pub(crate) fn of(meta: &std::fs::Metadata) -> Self {
        Self::new(meta.created().ok(), meta.len())
    }

    /// The same record as when `other` was taken. Never without a creation time.
    fn same(self, other: Self) -> bool {
        self.created != Self::UNKNOWN && self == other
    }
}

/// One `$I` record as the tracker knows it.
#[derive(Clone, Copy, Debug)]
struct Rec {
    head: Head,
    stamp: Stamp,
    /// The scan that last saw it listed, or that was running when it was noted.
    seen: u64,
}

impl Rec {
    /// Bytes it counts for: the size versions 1 and 2 record; nothing for a layout not known.
    const fn bytes(&self) -> u64 {
        if self.head.version == 1 || self.head.version == VERSION {
            self.head.size
        } else {
            0
        }
    }
}

/// A bin item that could be deleted for good to make room (see `win_bin_evict.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Its `$I` record's name.
    pub(crate) name: String,
    /// Bytes its record says it holds.
    pub(crate) size: u64,
    /// Deletion time its record holds, FILETIME.
    pub(crate) deleted: i64,
}

#[derive(Default)]
struct State {
    /// By `$I` name.
    records: HashMap<String, Rec>,
    /// Sum of the records' [`Rec::bytes`].
    total: u64,
    /// `None` until the first scan is done; then whether the folder could be listed last time.
    listed: Option<bool>,
    /// Records the watcher named, to look at again.
    pending: HashSet<String>,
    /// The watcher lost changes: list the folder again.
    rescan: bool,
    /// Bumped by each scan, so it tells the records it saw from ones gone since.
    scan: u64,
    stopped: bool,
}

impl State {
    fn insert(&mut self, name: String, rec: Rec) {
        let added = rec.bytes();
        if let Some(old) = self.records.insert(name, rec) {
            self.total = self.total.saturating_sub(old.bytes());
        }
        self.total = self.total.saturating_add(added);
    }

    fn remove(&mut self, name: &str) {
        if let Some(old) = self.records.remove(name) {
            self.total = self.total.saturating_sub(old.bytes());
        }
    }
}

/// One bin folder and its thread.
struct Bin {
    path: PathBuf,
    state: Mutex<State>,
    /// Wakes the bin's thread: something to look at, or stop.
    work: Condvar,
    /// Wakes those waiting for the first scan.
    ready: Condvar,
}

/// The bins being kept track of, each with its own thread.
pub(crate) struct Tracker {
    fs: Arc<dyn Fs>,
    /// Where scans keep what they read; `None` keeps nothing.
    cache: Option<PathBuf>,
    bins: Mutex<HashMap<PathBuf, Arc<Bin>>>,
}

impl Tracker {
    pub(crate) fn new(fs: Arc<dyn Fs>, cache: Option<PathBuf>) -> Self {
        Self {
            fs,
            cache,
            bins: Mutex::default(),
        }
    }

    /// Starts keeping `bin`'s total (once), without waiting for the first scan.
    pub(crate) fn track(&self, bin: &Path) {
        let _ = self.bin(bin);
    }

    /// Bytes `bin`'s records hold, once its first scan is done: waits for that, so call it off
    /// the UI thread. `None` when the folder can't be listed or watched. A bin that couldn't be
    /// listed last time (a drive back, its bin folder made again) is started over once.
    pub(crate) fn bytes(&self, bin: &Path) -> Option<u64> {
        let tracked = self.bin(bin);
        if let Some(total) = tracked.wait() {
            return Some(total);
        }
        self.forget(&tracked);
        self.bin(bin).wait()
    }

    /// The fast path wrote this record: counted now, and not read again when the watcher names
    /// it.
    pub(crate) fn note_added(&self, bin: &Path, name: &str, head: Head, stamp: Stamp) {
        if let Some(tracked) = self.get(bin) {
            let mut state = tracked.state.lock();
            let seen = state.scan;
            state.insert(name.to_owned(), Rec { head, stamp, seen });
        }
    }

    /// This record was deleted (with its item, to make room).
    pub(crate) fn note_removed(&self, bin: &Path, name: &str) {
        if let Some(tracked) = self.get(bin) {
            tracked.state.lock().remove(name);
        }
    }

    /// The items in `bin` whose record is version 2 (the layout the fast path reads), in no order.
    pub(crate) fn candidates(&self, bin: &Path) -> Vec<Candidate> {
        let Some(tracked) = self.get(bin) else {
            return Vec::new();
        };
        let state = tracked.state.lock();
        state
            .records
            .iter()
            .filter(|(_, rec)| rec.head.version == VERSION)
            .map(|(name, rec)| Candidate {
                name: name.clone(),
                size: rec.head.size,
                deleted: rec.head.deleted,
            })
            .collect()
    }

    fn get(&self, bin: &Path) -> Option<Arc<Bin>> {
        self.bins.lock().get(bin).map(Arc::clone)
    }

    fn bin(&self, path: &Path) -> Arc<Bin> {
        let mut bins = self.bins.lock();
        if let Some(bin) = bins.get(path) {
            return Arc::clone(bin);
        }
        let bin = Arc::new(Bin {
            path: path.to_path_buf(),
            state: Mutex::default(),
            work: Condvar::new(),
            ready: Condvar::new(),
        });
        bins.insert(path.to_path_buf(), Arc::clone(&bin));
        drop(bins);
        let fs = Arc::clone(&self.fs);
        let cache = self.cache.as_deref().and_then(|dir| cache_file(dir, path));
        let worker = Arc::clone(&bin);
        let spawned = std::thread::Builder::new()
            .name("bin-usage".into())
            .spawn(move || {
                // A bin of tens of thousands of items takes a while to sum: it mustn't compete with
                // what the user is doing on the disk, as folder sizes don't.
                #[cfg(not(test))]
                crate::win::lower_io_priority();
                run(&*fs, &worker, cache.as_deref());
            });
        if let Err(e) = spawned {
            log::warn!("recycle bin {}: not summed: {e}", path.display());
            bin.finish_scan(false);
        }
        bin
    }

    fn forget(&self, bin: &Arc<Bin>) {
        let mut bins = self.bins.lock();
        if bins.get(&bin.path).is_some_and(|b| Arc::ptr_eq(b, bin)) {
            bins.remove(&bin.path);
        }
        drop(bins);
        bin.stop();
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        for bin in self.bins.lock().values() {
            bin.stop();
        }
    }
}

/// The bin's thread: watches the folder, sums it, then sleeps until the watcher names a change.
fn run(fs: &dyn Fs, bin: &Arc<Bin>, cache: Option<&Path>) {
    // Watching first, so nothing that changes during the first scan is missed.
    let weak = Arc::downgrade(bin);
    let watching = fs.watch(
        &bin.path,
        Box::new(move |event| {
            if let Some(bin) = weak.upgrade() {
                bin.on_event(event);
            }
        }),
    );
    let _watch = match watching {
        Ok(guard) => guard,
        Err(e) => {
            log::info!(
                "recycle bin {}: not watched ({e}); deletes there go through Windows",
                bin.path.display()
            );
            bin.finish_scan(false);
            return;
        }
    };
    if let Some(file) = cache {
        bin.load(file);
    }
    let mut read = bin.scan(fs);
    loop {
        if read >= SAVE_AFTER_READS
            && let Some(file) = cache
        {
            bin.save(file);
        }
        let Some((names, rescan)) = bin.next_work() else {
            return;
        };
        read = if rescan {
            bin.scan(fs)
        } else {
            bin.refresh(fs, names);
            0
        };
    }
}

impl Bin {
    fn wait(&self) -> Option<u64> {
        let mut state = self.state.lock();
        while state.listed.is_none() {
            self.ready.wait(&mut state);
        }
        let listed = state.listed == Some(true);
        let total = state.total;
        drop(state);
        listed.then_some(total)
    }

    fn finish_scan(&self, listed: bool) {
        self.state.lock().listed = Some(listed);
        self.ready.notify_all();
    }

    fn stop(&self) {
        let mut state = self.state.lock();
        state.stopped = true;
        if state.listed.is_none() {
            state.listed = Some(false);
        }
        drop(state);
        self.work.notify_all();
        self.ready.notify_all();
    }

    /// Called on the watcher's thread: notes what to look at and wakes the bin's thread.
    fn on_event(&self, event: WatchEvent) {
        let mut state = self.state.lock();
        match event {
            WatchEvent::Changed(paths) => {
                state
                    .pending
                    .extend(paths.into_iter().filter_map(|p| record_name(&p)));
            }
            WatchEvent::Rescan => state.rescan = true,
        }
        let wake = state.rescan || !state.pending.is_empty();
        drop(state);
        if wake {
            self.work.notify_one();
        }
    }

    /// Waits for the watcher: the records it named, and whether to list the folder again.
    /// `None` once stopped.
    fn next_work(&self) -> Option<(Vec<String>, bool)> {
        let mut state = self.state.lock();
        while !state.stopped && !state.rescan && state.pending.is_empty() {
            self.work.wait(&mut state);
        }
        if state.stopped {
            return None;
        }
        let rescan = std::mem::take(&mut state.rescan);
        let names = state.pending.drain().collect();
        drop(state);
        Some((names, rescan))
    }

    /// Lists the folder, reads the records not known yet and forgets those gone. Returns how
    /// many it read.
    fn scan(&self, fs: &dyn Fs) -> usize {
        let scan = {
            let mut state = self.state.lock();
            state.scan += 1;
            state.rescan = false;
            state.scan
        };
        // A listing that fails partway would undercount: the whole scan counts as failed.
        let listed: io::Result<Vec<(String, Stamp)>> = fs.read_dir(&self.path).and_then(|it| {
            let mut out = Vec::new();
            for entry in it {
                let entry = entry?;
                if entry.kind == EntryKind::File && entry.name.starts_with("$I") {
                    out.push((
                        (*entry.name).to_owned(),
                        Stamp::new(entry.created, entry.size),
                    ));
                }
            }
            Ok(out)
        });
        let listed = match listed {
            Ok(listed) => listed,
            Err(e) => {
                log::info!("recycle bin {}: can't be listed: {e}", self.path.display());
                self.finish_scan(false);
                return 0;
            }
        };
        let unknown: Vec<(String, Stamp)> = {
            let mut state = self.state.lock();
            listed
                .into_iter()
                .filter(|(name, stamp)| match state.records.get_mut(name) {
                    Some(rec) if rec.stamp.same(*stamp) => {
                        rec.seen = scan;
                        false
                    }
                    _ => true,
                })
                .collect()
        };
        let read = unknown.len();
        for (name, stamp) in unknown {
            if self.state.lock().stopped {
                return read;
            }
            if let Some(head) = read_head(fs, &self.path.join(&name)) {
                let rec = Rec {
                    head,
                    stamp,
                    seen: scan,
                };
                self.state.lock().insert(name, rec);
            }
        }
        let mut state = self.state.lock();
        let gone: Vec<String> = state
            .records
            .iter()
            .filter(|(_, rec)| rec.seen < scan)
            .map(|(name, _)| name.clone())
            .collect();
        for name in gone {
            state.remove(&name);
        }
        drop(state);
        self.finish_scan(true);
        read
    }

    /// Looks again at the records the watcher named: new, gone or (in theory) replaced.
    fn refresh(&self, fs: &dyn Fs, names: Vec<String>) {
        for name in names {
            let path = self.path.join(&name);
            let stamp = match fs.stat(&path) {
                Ok(entry) if entry.kind == EntryKind::File => Stamp::new(entry.created, entry.size),
                Ok(_) => {
                    self.state.lock().remove(&name);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.state.lock().remove(&name);
                    continue;
                }
                Err(_) => continue,
            };
            let known = self
                .state
                .lock()
                .records
                .get(&name)
                .is_some_and(|rec| rec.stamp.same(stamp));
            if known {
                continue;
            }
            let head = read_head(fs, &path);
            let mut state = self.state.lock();
            match head {
                Some(head) => {
                    let seen = state.scan;
                    state.insert(name, Rec { head, stamp, seen });
                }
                None => state.remove(&name),
            }
        }
    }

    /// Takes what an earlier start knew about this bin. Before the first scan, which keeps only
    /// the records still listed the same.
    fn load(&self, file: &Path) {
        let Ok(bytes) = std::fs::read(file) else {
            return;
        };
        let Some(records) = decode_cache(&bytes, &self.path) else {
            log::info!("recycle bin cache {} unreadable, ignored", file.display());
            return;
        };
        let mut state = self.state.lock();
        for (name, head, stamp) in records {
            state.insert(
                name,
                Rec {
                    head,
                    stamp,
                    seen: 0,
                },
            );
        }
    }

    fn save(&self, file: &Path) {
        let bytes = {
            let state = self.state.lock();
            encode_cache(&self.path, &state.records)
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Written whole, then put in place: a crash midway leaves the old file.
        let tmp = file.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, file)) {
            log::warn!("recycle bin cache not saved: {e}");
        }
    }
}

/// The `$I` record a change notification names, if it names one.
fn record_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    name.starts_with("$I").then(|| name.to_owned())
}

fn read_head(fs: &dyn Fs, path: &Path) -> Option<Head> {
    fs.read_head(path, HEAD_LEN as u64)
        .ok()
        .and_then(|bytes| head(&bytes))
}

/// The cache file of the bin on drive `X:`: `X.bin` (the folder is per user already).
fn cache_file(dir: &Path, bin: &Path) -> Option<PathBuf> {
    let mut chars = bin.to_str()?.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then(|| dir.join(format!("{}.bin", letter.to_ascii_uppercase())))
}

const CACHE_MAGIC: &[u8; 8] = b"ARKEBIN1";

/// `CACHE_MAGIC`, the bin folder's path (u16 LE length, UTF-8), then per record: its name (u16 LE
/// length, UTF-8), creation time and length (u64 LE each), and version, size and deletion time
/// (i64, u64, i64 LE).
fn encode_cache(bin: &Path, records: &HashMap<String, Rec>) -> Vec<u8> {
    let mut out = CACHE_MAGIC.to_vec();
    let text = |out: &mut Vec<u8>, s: &str| -> bool {
        let Ok(len) = u16::try_from(s.len()) else {
            return false;
        };
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        true
    };
    if !bin.to_str().is_some_and(|path| text(&mut out, path)) {
        return Vec::new();
    }
    for (name, rec) in records {
        if !text(&mut out, name) {
            continue;
        }
        out.extend_from_slice(&rec.stamp.created.to_le_bytes());
        out.extend_from_slice(&rec.stamp.len.to_le_bytes());
        out.extend_from_slice(&rec.head.version.to_le_bytes());
        out.extend_from_slice(&rec.head.size.to_le_bytes());
        out.extend_from_slice(&rec.head.deleted.to_le_bytes());
    }
    out
}

/// What [`encode_cache`] wrote for `bin`; `None` for anything else.
fn decode_cache(bytes: &[u8], bin: &Path) -> Option<Vec<(String, Head, Stamp)>> {
    let mut at = Reader(bytes.strip_prefix(CACHE_MAGIC)?);
    if Path::new(at.text()?) != bin {
        return None;
    }
    let mut out = Vec::new();
    while !at.0.is_empty() {
        let name = at.text()?.to_owned();
        let stamp = Stamp {
            created: at.u64()?,
            len: at.u64()?,
        };
        let head = Head {
            version: at.i64()?,
            size: at.u64()?,
            deleted: at.i64()?,
        };
        out.push((name, head, stamp));
    }
    Some(out)
}

/// Reads [`encode_cache`]'s fields off the front of a byte slice.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }

    fn i64(&mut self) -> Option<i64> {
        self.take().map(i64::from_le_bytes)
    }

    fn text(&mut self) -> Option<&'a str> {
        let len = usize::from(u16::from_le_bytes(self.take()?));
        let (text, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        std::str::from_utf8(text).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arke_core::Entry;
    use arke_core::fake::FakeFs;
    use std::time::Duration;

    fn bin() -> PathBuf {
        PathBuf::from(r"C:\$Recycle.Bin\S-1-5-21-0-0-0-1001")
    }

    fn time(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    /// A version `version` record of `size` bytes, as Windows names its pair: `$I` + `tag`.
    fn add_record(fs: &FakeFs, tag: &str, version: i64, size: u64) {
        let deleted = 0x01DA_0000_0000_0000 + i64::from(tag.as_bytes()[0]);
        let mut bytes = crate::win_fast_recycle::encode(
            VERSION,
            size,
            deleted,
            Path::new(r"C:\Users\someone\x.txt"),
        );
        bytes[..8].copy_from_slice(&version.to_le_bytes());
        let name = format!("$I{tag}");
        let entry = Entry::new(name.as_str(), EntryKind::File, bytes.len() as u64, None, 0)
            .with_created(Some(time(u64::from(tag.as_bytes()[0]))));
        fs.add_entry(&bin(), entry);
        fs.set_content(bin().join(&name), bytes);
        fs.add_file(&bin(), &format!("$R{tag}"), size);
    }

    fn fake() -> Arc<FakeFs> {
        let fs = FakeFs::new();
        fs.insert_dir(bin(), Vec::new());
        fs.add_file(&bin(), "desktop.ini", 129);
        fs
    }

    /// Waits for the bin's thread to catch up with a change.
    fn settles(tracker: &Tracker, want: u64) {
        for _ in 0..500 {
            if tracker.bytes(&bin()) == Some(want) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(tracker.bytes(&bin()), Some(want));
    }

    #[test]
    fn the_total_is_what_the_records_hold() {
        let fs = fake();
        add_record(&fs, "AAAAAA.txt", VERSION, 100);
        add_record(&fs, "BBBBBB", VERSION, 2_000);
        add_record(&fs, "CCCCCC.v1", 1, 30);
        add_record(&fs, "DDDDDD.v9", 9, 50_000);
        let tracker = Tracker::new(fs, None);
        assert_eq!(
            tracker.bytes(&bin()),
            Some(2_130),
            "versions 1 and 2 count; a layout not known, desktop.ini and $R items don't"
        );
        let mut names: Vec<String> = tracker
            .candidates(&bin())
            .into_iter()
            .map(|c| c.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["$IAAAAAA.txt", "$IBBBBBB"],
            "only version 2 records can make room"
        );
    }

    #[test]
    fn changes_the_watcher_reports_keep_the_total_current() {
        let fs = fake();
        add_record(&fs, "AAAAAA", VERSION, 100);
        let tracker = Tracker::new(Arc::clone(&fs) as Arc<dyn Fs>, None);
        assert_eq!(tracker.bytes(&bin()), Some(100));

        add_record(&fs, "BBBBBB", VERSION, 40);
        fs.emit_changed(&bin(), &["$IBBBBBB", "$RBBBBBB"]);
        settles(&tracker, 140);

        fs.remove(&bin(), "$IAAAAAA");
        fs.remove(&bin(), "$RAAAAAA");
        fs.emit_changed(&bin(), &["$RAAAAAA", "$IAAAAAA"]);
        settles(&tracker, 40);

        // Changes the watcher lost: a new listing finds them.
        add_record(&fs, "CCCCCC", VERSION, 5);
        fs.remove(&bin(), "$IBBBBBB");
        fs.emit(&bin(), &WatchEvent::Rescan);
        settles(&tracker, 5);
    }

    #[test]
    fn records_the_fast_path_notes_count_at_once_and_are_not_read_again() {
        let fs = fake();
        let tracker = Tracker::new(Arc::clone(&fs) as Arc<dyn Fs>, None);
        assert_eq!(tracker.bytes(&bin()), Some(0));
        let head = Head {
            version: VERSION,
            size: 70,
            deleted: 1,
        };
        let stamp = Stamp::new(Some(time(1)), 60);
        tracker.note_added(&bin(), "$INEWONE", head, stamp);
        assert_eq!(
            tracker.bytes(&bin()),
            Some(70),
            "counted before the watcher"
        );
        // Listed the same as noted, and no content to read: reading it would drop it.
        fs.add_entry(
            &bin(),
            Entry::new("$INEWONE", EntryKind::File, 60, None, 0).with_created(Some(time(1))),
        );
        fs.emit_changed(&bin(), &["$INEWONE"]);
        fs.emit(&bin(), &WatchEvent::Rescan);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(tracker.bytes(&bin()), Some(70));
        tracker.note_removed(&bin(), "$INEWONE");
        assert_eq!(tracker.bytes(&bin()), Some(0));
    }

    #[test]
    fn a_bin_that_cant_be_listed_has_no_total() {
        let fs = FakeFs::new();
        let tracker = Tracker::new(fs, None);
        assert_eq!(tracker.bytes(&bin()), None);
    }

    #[test]
    fn the_cache_reads_back_as_written() {
        let mut records = HashMap::new();
        let rec = |size, created| Rec {
            head: Head {
                version: VERSION,
                size,
                deleted: -5,
            },
            stamp: Stamp { created, len: 544 },
            seen: 3,
        };
        records.insert("$IAAAAAA.txt".to_owned(), rec(10, 7));
        records.insert("$I中文.md".to_owned(), rec(u64::MAX, Stamp::UNKNOWN));
        let bytes = encode_cache(&bin(), &records);
        let mut back = decode_cache(&bytes, &bin()).expect("reads back");
        back.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].0, "$IAAAAAA.txt");
        assert_eq!(back[0].1.size, 10);
        assert_eq!(back[0].1.deleted, -5);
        assert_eq!(
            back[0].2,
            Stamp {
                created: 7,
                len: 544
            }
        );
        assert_eq!(back[1].1.size, u64::MAX);
        assert!(
            decode_cache(&bytes, Path::new(r"D:\$Recycle.Bin\S-1")).is_none(),
            "another bin's"
        );
        assert!(
            decode_cache(&bytes[..bytes.len() - 1], &bin()).is_none(),
            "cut short"
        );
        assert!(decode_cache(b"nonsense", &bin()).is_none());
    }

    #[test]
    fn cache_files_are_per_drive() {
        let dir = Path::new(r"C:\cache");
        assert_eq!(
            cache_file(dir, Path::new(r"d:\$Recycle.Bin\S-1")),
            Some(dir.join("D.bin"))
        );
        assert_eq!(cache_file(dir, Path::new(r"\\server\share\x")), None);
    }

    /// A later start lists the bin and reads only what is new: the records it knew from the
    /// cache have no content here, so reading them would have dropped them.
    #[test]
    fn a_later_start_reads_only_new_records() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        let tags: Vec<String> = (0..SAVE_AFTER_READS + 6)
            .map(|i| format!("A{i:05}.txt"))
            .collect();
        let first = fake();
        for tag in &tags {
            add_record(&first, tag, VERSION, 10);
        }
        let tracker = Tracker::new(first, Some(dir.clone()));
        assert_eq!(tracker.bytes(&bin()), Some(10 * tags.len() as u64));
        let file = cache_file(&dir, &bin()).unwrap();
        for _ in 0..500 {
            if file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(tracker);

        let later = fake();
        for tag in &tags[1..] {
            add_record(&later, tag, VERSION, 10);
            later.set_content(bin().join(format!("$I{tag}")), Vec::new());
        }
        add_record(&later, "NEW", VERSION, 1_000);
        let tracker = Tracker::new(later, Some(dir));
        assert_eq!(
            tracker.bytes(&bin()),
            Some(10 * (tags.len() as u64 - 1) + 1_000),
            "the one gone is forgotten, the new one read, the rest from the cache"
        );
        drop(tracker);
    }
}
