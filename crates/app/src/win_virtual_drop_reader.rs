//! The thread that talks to the program dropped files come from (#456).
//!
//! Every call into the source runs here: getting an item's contents, reading its stream, saving
//! its storage, ending a drop that finishes later. The worker writing the files only asks and
//! waits for the answers, watching Cancel meanwhile. A source that never answers (a remote
//! control program whose stream waits for its network) can't hold the copy: the worker stops
//! waiting, and once the source has been slow or reads have grown, Cancel also interrupts the
//! call (`CoCancelCall`, which ends a call into another process or apartment on the caller's
//! side). The thread then ends by itself once its call returns, and releases what it holds
//! there.
//!
//! Streams are read a little at a time at first and in bigger pieces while the source keeps up
//! ([`ChunkSize`]), and one read ahead of the writing, so local sources (zip folders, Outlook)
//! copy at least as fast as before and slow ones aren't asked for more than they give at once.
//! A bigger read that doesn't come back within [`RECOVER_AFTER`] is given up on, and the stream
//! is read on from where it started in small reads ([`Link::watch`]).

use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{
    CloseHandle, E_ABORT, E_UNEXPECTED, HANDLE, RPC_E_CALL_CANCELED, RPC_S_CALLPENDING,
};
use windows::Win32::System::Com::{
    CoCancelCall, CoDisableCallCancellation, CoEnableCallCancellation, CoWaitForMultipleHandles,
    IDataObject, IStream, STREAM_SEEK_SET,
};
use windows::Win32::System::Threading::{CreateEventW, GetThreadId, SetEvent, WaitForSingleObject};
use windows::core::{AgileReference, HRESULT, PCWSTR};

use super::{
    AsyncOperation, CHUNK, ComThread, Contents, Fetched, Item, Source, Stop, contents,
    write_storage,
};

/// The first read asked of a stream. NetEase UU Remote answers reads this big, where a 1 MiB
/// read never came back (#456).
pub(super) const FIRST_CHUNK: usize = 64 << 10;
/// Reads asked before the first answer comes back: the next piece is read while the last one
/// is written.
pub(super) const AHEAD: usize = 2;
/// A read at least this fast (bytes a second) keeps up: the source has the data at hand.
const KEEPING_UP: u128 = 64 << 20;
/// Reads in a row that keep up before reads double.
const STREAK: u32 = 4;
/// A read this slow halves the reads.
const SLOW: Duration = Duration::from_millis(250);
/// How often a wait looks at Cancel, in milliseconds.
const POLL_MS: u32 = 50;
/// How long a cancelled copy waits for the thread to let go before leaving it behind.
const GRACE: Duration = Duration::from_millis(500);
/// A read this slow makes the thread's later calls cancellable. Not from the start: Windows makes
/// cancellable calls into other processes on the same computer slower (File Explorer's zip
/// folders, Outlook), which only matters next to fast reads.
const CANCELLABLE_AFTER: Duration = Duration::from_millis(50);
/// A read bigger than [`FIRST_CHUNK`] that takes this long is given up on: the source may never
/// answer reads that big (#456). Well above [`SLOW`], so a source that is only slow isn't.
const RECOVER_AFTER: Duration = Duration::from_secs(3);

/// How much to ask of a stream at a time.
///
/// Starts at [`FIRST_CHUNK`] and doubles, up to [`CHUNK`], after [`STREAK`] full reads in a row
/// that ran at [`KEEPING_UP`] or faster: a source reading from its own disk or memory gets there
/// within a few megabytes, so it pays for as few calls as before. A source that answers more
/// slowly (one round trip over the network a read) stays at small reads, where a call costs
/// nothing next to the wait. A read slower than [`SLOW`] halves the size again, and the reads of
/// that drop never grow after it; nor after a read that was given up on ([`ChunkSize::pin`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChunkSize {
    size: usize,
    /// Reads of the current size in a row that kept up.
    streak: u32,
    /// A read of the drop was slow or given up on: no more growing.
    settled: bool,
}

impl ChunkSize {
    pub(super) const fn new() -> Self {
        Self {
            size: FIRST_CHUNK,
            streak: 0,
            settled: false,
        }
    }

    /// For the drop's next item: small reads first again, still never growing if settled.
    pub(super) const fn next_item(self) -> Self {
        Self {
            size: FIRST_CHUNK,
            streak: 0,
            settled: self.settled,
        }
    }

    /// A read was given up on: [`FIRST_CHUNK`] from now on.
    pub(super) const fn pin(&mut self) {
        self.size = FIRST_CHUNK;
        self.streak = 0;
        self.settled = true;
    }

    /// The size of the next read.
    pub(super) const fn get(&self) -> usize {
        self.size
    }

    /// Learns from a read of `asked` bytes that gave `got` in `took`. Reads asked before the
    /// size last changed (still on their way when it did) don't make it grow.
    pub(super) fn learn(&mut self, asked: usize, got: usize, took: Duration) {
        if took >= SLOW {
            self.size = (asked / 2).max(FIRST_CHUNK).min(self.size);
            self.streak = 0;
            self.settled = true;
        } else if asked == self.size && !self.settled {
            if got >= asked && keeps_up(asked, took) {
                self.streak += 1;
                if self.streak >= STREAK {
                    self.size = (self.size * 2).min(CHUNK);
                    self.streak = 0;
                }
            } else {
                self.streak = 0;
            }
        }
    }
}

/// Whether `bytes` in `took` is at least [`KEEPING_UP`].
const fn keeps_up(bytes: usize, took: Duration) -> bool {
    took.as_nanos().saturating_mul(KEEPING_UP) <= (bytes as u128).saturating_mul(1_000_000_000)
}

/// What the worker asks of the source's thread.
pub(super) enum Ask {
    /// Get item `n`'s contents ready (an index into the drop's items).
    Open(usize),
    /// Read up to this many bytes of the open stream into the buffer (grown if it is smaller).
    Read(Vec<u8>, usize),
    /// Save the open storage as a compound file here.
    Save(PathBuf),
    /// Tell a source that finishes later how the drop went: the result, and whether it copied.
    End(HRESULT, bool),
}

/// An answer, in the order asked (`End` has none).
pub(super) enum Answer {
    Opened(windows::core::Result<Opened>),
    Read(ReadDone),
    Saved(windows::core::Result<()>),
}

/// An item's contents, as far as the worker needs to know.
pub(super) enum Opened {
    /// In memory, handed over.
    Bytes(Vec<u8>),
    /// A stream, read with [`Ask::Read`].
    Stream,
    /// A storage, saved with [`Ask::Save`].
    Storage,
}

/// A read of the open stream.
pub(super) struct ReadDone {
    /// The buffer it was asked with; the bytes read are at its start.
    pub(super) buffer: Vec<u8>,
    /// How many bytes were asked for.
    pub(super) asked: usize,
    /// How many arrived, at most `asked`.
    pub(super) got: usize,
    /// What the stream said: `S_FALSE` at its end.
    pub(super) hr: HRESULT,
    /// How long the source took.
    pub(super) took: Duration,
    /// The stream is read [`FIRST_CHUNK`] at a time from now on: a read was given up on.
    pub(super) pinned: bool,
}

impl ReadDone {
    /// A read that wasn't made, or failed before any bytes arrived.
    const fn none(buffer: Vec<u8>, asked: usize, hr: HRESULT) -> Self {
        Self {
            buffer,
            asked,
            got: 0,
            hr,
            took: Duration::ZERO,
            pinned: false,
        }
    }
}

/// The error for a source thread that is gone (it panicked) or answered out of turn.
pub(super) fn gone() -> Stop {
    Stop::Failed(windows::core::Error::from(E_UNEXPECTED).into())
}

/// An auto-reset event.
struct Signal(HANDLE);

// SAFETY: an event handle may be set, waited on and closed from any thread.
unsafe impl Send for Signal {}
// SAFETY: as above; setting and waiting need no exclusive access.
unsafe impl Sync for Signal {}

impl Signal {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: an unnamed auto-reset event, closed when this is dropped.
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map(Self)
    }

    fn set(&self) {
        // SAFETY: the event lives as long as this.
        let _ = unsafe { SetEvent(self.0) };
    }

    /// Waits until set, at most `ms` milliseconds. On a single-threaded apartment, COM calls
    /// into it are handled meanwhile: the source's objects may live there (a paste reads the
    /// clipboard on one), and the source's thread may be calling them.
    fn wait(&self, ms: u32) {
        // SAFETY: waits on a live handle.
        let waited = unsafe { CoWaitForMultipleHandles(0, ms, &[self.0]) };
        if let Err(e) = waited
            && e.code() != RPC_S_CALLPENDING
        {
            // COM isn't set up on this thread: a plain wait.
            // SAFETY: waits on a live handle.
            let _ = unsafe { WaitForSingleObject(self.0, ms) };
        }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: created in `new`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// What the worker and the source's thread share.
struct Shared {
    /// Set after each answer, and when the thread is done.
    signal: Signal,
    /// The worker stopped waiting (Cancel): reads and saves still asked aren't made.
    stop: AtomicBool,
    /// The thread has let go of everything and is ending.
    done: AtomicBool,
    /// The read being made.
    reading: Mutex<Reading>,
}

/// The read the source thread is making, for the worker's [`Link::watch`].
#[derive(Default)]
struct Reading {
    /// When it started and its size.
    current: Option<(Instant, usize)>,
    /// The worker gave up on it (cancelled it to read on in small pieces).
    given_up: bool,
}

/// The worker's end: asks, waits for answers while watching Cancel, and lets go.
pub(super) struct Link {
    asks: Option<flume::Sender<Ask>>,
    answers: flume::Receiver<Answer>,
    shared: Arc<Shared>,
    /// `None` once joined.
    thread: Option<JoinHandle<()>>,
    /// For `CoCancelCall`. No other thread can have this id while `thread` holds the handle.
    thread_id: u32,
}

impl Link {
    /// Starts the thread, which takes `source` (and its items' contents) over.
    pub(super) fn start(items: Vec<Item>, format: u16, source: Source) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            signal: Signal::new()?,
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
            reading: Mutex::new(Reading::default()),
        });
        let (asks, asked) = flume::unbounded();
        let (answer, answers) = flume::unbounded();
        let thread = std::thread::Builder::new()
            .name("virtual drop source".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || serve(items, format, source, &asked, &answer, &shared)
            })?;
        // SAFETY: the handle of a thread that hasn't been joined, held open by `thread`.
        let thread_id = unsafe { GetThreadId(HANDLE(thread.as_raw_handle())) };
        Ok(Self {
            asks: Some(asks),
            answers,
            shared,
            thread: Some(thread),
            thread_id,
        })
    }

    /// Asks; `false` if the thread is gone.
    pub(super) fn ask(&self, ask: Ask) -> bool {
        self.asks
            .as_ref()
            .is_some_and(|asks| asks.send(ask).is_ok())
    }

    /// The next answer. Cancel ends the wait, and interrupts the call the thread is making.
    pub(super) fn answer(&self, cancel: &AtomicBool) -> Result<Answer, Stop> {
        loop {
            match self.answers.try_recv() {
                Ok(answer) => return Ok(answer),
                Err(flume::TryRecvError::Disconnected) => return Err(gone()),
                Err(flume::TryRecvError::Empty) => {}
            }
            if cancel.load(Ordering::Relaxed) {
                self.interrupt();
                return Err(Stop::Cancelled);
            }
            self.watch();
            self.shared.signal.wait(POLL_MS);
        }
    }

    /// Gives up on a read bigger than [`FIRST_CHUNK`] that hasn't come back within
    /// [`RECOVER_AFTER`]: a source may hang on reads bigger than it can carry (#456). The call
    /// is cancelled (bigger reads are always cancellable), and the thread reads on from where it
    /// started, [`FIRST_CHUNK`] at a time.
    fn watch(&self) {
        if self.thread.is_none() || self.finished() {
            return;
        }
        let mut reading = self.shared.reading.lock();
        if let Some((since, len)) = reading.current
            && len > FIRST_CHUNK
            && since.elapsed() >= RECOVER_AFTER
            && !reading.given_up
        {
            reading.given_up = true;
            // Under the lock: the thread can't have moved on to another read.
            cancel_call(self.thread_id);
        }
        drop(reading);
    }

    /// Stops the thread's work: what is still asked isn't done, and a call into another
    /// process or apartment returns now (`RPC_E_CALL_CANCELED`). A call into an object of its
    /// own apartment can't be interrupted; the worker leaves it running.
    fn interrupt(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if self.thread.is_some() && !self.finished() {
            cancel_call(self.thread_id);
        }
    }

    fn finished(&self) -> bool {
        self.shared.done.load(Ordering::Acquire)
            || self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Lets the thread go: waits for it to release what it holds. Once the copy is cancelled,
    /// waits at most [`GRACE`], then leaves the thread to end by itself when the source answers.
    pub(super) fn close(mut self, cancel: &AtomicBool) {
        drop(self.asks.take());
        let mut cancelled: Option<Instant> = None;
        while !self.finished() {
            if cancel.load(Ordering::Relaxed) {
                if cancelled.get_or_insert_with(Instant::now).elapsed() >= GRACE {
                    log::warn!("the source of dropped files doesn't answer; leaving it behind");
                    return;
                }
                self.interrupt();
            } else {
                self.watch();
            }
            self.shared.signal.wait(POLL_MS);
        }
        if let Some(thread) = self.thread.take() {
            // A panic was logged by the panic hook.
            let _ = thread.join();
        }
    }
}

/// Cancels the call thread `id` is making into another process or apartment, if any. Callers
/// name the source thread while its handle is held open (see [`Link`]), so the id is no other
/// thread's.
fn cancel_call(id: u32) {
    // SAFETY: plain call; fails harmlessly when the thread isn't calling out.
    let _ = unsafe { CoCancelCall(id, 0) };
    #[cfg(test)]
    super::tests::cancel_call(id);
}

/// The source thread: answers asks until the worker lets go, then releases everything here.
fn serve(
    items: Vec<Item>,
    format: u16,
    source: Source,
    asks: &flume::Receiver<Ask>,
    answers: &flume::Sender<Answer>,
    shared: &Shared,
) {
    {
        let _com = ComThread::init();
        let mut server = Server::new(items, format, source);
        while let Ok(ask) = asks.recv() {
            if let Some(answer) = server.handle(ask, shared) {
                // Nobody waits any more (cancelled): the answer goes with the buffer.
                let _ = answers.send(answer);
            }
            shared.signal.set();
        }
        let cancellable = server.cancellable;
        drop(server);
        if cancellable {
            // SAFETY: pairs with `enable_cancellation`, on this thread.
            let _ = unsafe { CoDisableCallCancellation(None) };
        }
    }
    shared.done.store(true, Ordering::Release);
    shared.signal.set();
}

/// The source's side, on its own thread. Fields drop in order: the open item first, and a drop
/// that finishes later is ended before its data object goes.
struct Server {
    /// The item being read or saved.
    open: Option<Contents>,
    items: Vec<Item>,
    format: u16,
    bodies: Bodies,
    /// The worker can interrupt this thread's calls (`CoEnableCallCancellation`).
    cancellable: bool,
    /// Bytes of the open stream read so far.
    pos: u64,
    /// The open stream was put at its start, so `pos` is where it is.
    from_start: bool,
    /// The open stream is read [`FIRST_CHUNK`] at a time: a read was given up on.
    pinned: bool,
}

enum Bodies {
    /// Read during the drop, one per item (`None` for folders and items already opened).
    Fetched(Vec<Option<Fetched>>),
    /// The source finishes later: asked for each item here.
    Later {
        /// `None` once ended.
        operation: Option<AsyncOperation>,
        resolved: Result<IDataObject, HRESULT>,
        /// Kept for as long as the drop is read, as the source handed it over.
        _data: AgileReference<IDataObject>,
    },
}

impl Server {
    fn new(items: Vec<Item>, format: u16, source: Source) -> Self {
        let bodies = match source {
            Source::Fetched(fetched) => Bodies::Fetched(fetched),
            Source::Later { data, operation } => Bodies::Later {
                operation: Some(operation),
                resolved: data.resolve().map_err(|e| {
                    log::warn!("can't reach a dropped data object from a worker: {e}");
                    e.code()
                }),
                _data: data,
            },
        };
        Self {
            open: None,
            items,
            format,
            bodies,
            cancellable: false,
            pos: 0,
            from_start: false,
            pinned: false,
        }
    }

    fn handle(&mut self, ask: Ask, shared: &Shared) -> Option<Answer> {
        match ask {
            Ask::Open(n) => Some(Answer::Opened(self.open(n))),
            Ask::Read(buffer, len) => Some(Answer::Read(self.read(buffer, len, shared))),
            Ask::Save(path) => Some(Answer::Saved(self.save(&path, shared))),
            Ask::End(hr, ok) => {
                self.open = None;
                if let Bodies::Later { operation, .. } = &mut self.bodies
                    && let Some(operation) = operation.take()
                {
                    operation.end(hr, ok);
                }
                None
            }
        }
    }

    fn open(&mut self, n: usize) -> windows::core::Result<Opened> {
        self.open = None;
        let item = self.items.get(n).ok_or(E_UNEXPECTED)?;
        let got = match &mut self.bodies {
            Bodies::Fetched(fetched) => match fetched.get_mut(n).and_then(Option::take) {
                Some(Fetched::Bytes(bytes)) => Ok(Contents::Bytes(bytes)),
                Some(Fetched::Stream(stream)) => stream.resolve().map(Contents::Stream),
                Some(Fetched::Storage(storage)) => storage.resolve().map(Contents::Storage),
                Some(Fetched::Failed(e)) => Err(e),
                None => Err(E_UNEXPECTED.into()),
            },
            Bodies::Later { resolved, .. } => match resolved {
                Ok(data) => contents(data, self.format, item),
                Err(hr) => Err((*hr).into()),
            },
        }?;
        Ok(match got {
            Contents::Bytes(bytes) => Opened::Bytes(bytes),
            Contents::Stream(stream) => {
                // Some sources leave the stream's position at its end.
                self.from_start = seek_to(&stream, 0).is_ok();
                self.pos = 0;
                self.pinned = false;
                self.open = Some(Contents::Stream(stream));
                Opened::Stream
            }
            Contents::Storage(storage) => {
                self.open = Some(Contents::Storage(storage));
                Opened::Storage
            }
        })
    }

    /// Reads up to `asked` bytes of the open stream ([`FIRST_CHUNK`] at most once pinned). A
    /// bigger read the worker gave up on is made again from where it started, a [`FIRST_CHUNK`]
    /// big, and the stream stays pinned; if it can't be put back there, the item fails.
    fn read(&mut self, mut buffer: Vec<u8>, asked: usize, shared: &Shared) -> ReadDone {
        let stream = match &self.open {
            Some(Contents::Stream(stream)) if !shared.stop.load(Ordering::Relaxed) => {
                stream.clone()
            }
            Some(Contents::Stream(_)) => return ReadDone::none(buffer, asked, E_ABORT),
            _ => return ReadDone::none(buffer, asked, E_UNEXPECTED),
        };
        let len = if self.pinned {
            asked.min(FIRST_CHUNK)
        } else {
            asked
        };
        if len > FIRST_CHUNK && !self.cancellable {
            // The worker may give up on a bigger read, which takes a cancellable call.
            self.cancellable = enable_cancellation();
        }
        if buffer.len() < len {
            buffer.resize(len, 0);
        }
        let (mut got, mut hr, mut took, given_up) = read_once(&stream, &mut buffer, len, shared);
        if given_up && hr == RPC_E_CALL_CANCELED && !shared.stop.load(Ordering::Relaxed) {
            log::info!(
                "a {} KiB read of a dropped file didn't come back in {RECOVER_AFTER:?}; reading \
                 on {} KiB at a time",
                len >> 10,
                FIRST_CHUNK >> 10
            );
            self.pinned = true;
            let back = if self.from_start {
                seek_to(&stream, self.pos)
            } else {
                Err(hr)
            };
            if let Err(e) = back {
                // Where the stream is can't be known: the item fails, and what is still asked
                // of it with it.
                self.open = None;
                return ReadDone::none(buffer, asked, e);
            }
            (got, hr, took, _) = read_once(&stream, &mut buffer, FIRST_CHUNK, shared);
        }
        if took >= CANCELLABLE_AFTER && !self.cancellable {
            self.cancellable = enable_cancellation();
        }
        let got = got.min(len);
        self.pos += got as u64;
        ReadDone {
            buffer,
            asked,
            got,
            hr,
            took,
            pinned: self.pinned,
        }
    }

    fn save(&mut self, path: &Path, shared: &Shared) -> windows::core::Result<()> {
        let Some(Contents::Storage(storage)) = self.open.take() else {
            return Err(E_UNEXPECTED.into());
        };
        if shared.stop.load(Ordering::Relaxed) {
            return Err(E_ABORT.into());
        }
        let saved = write_storage(path, &storage);
        if shared.stop.load(Ordering::Relaxed) {
            // The worker stopped waiting (Cancel) and couldn't remove the file this held open.
            let _ = std::fs::remove_file(path);
        }
        saved
    }
}

/// One read of `len` bytes (at most the buffer's), published while it runs for the worker's
/// [`Link::watch`]. Also says whether the worker gave up on it meanwhile.
fn read_once(
    stream: &IStream,
    buffer: &mut [u8],
    len: usize,
    shared: &Shared,
) -> (usize, HRESULT, Duration, bool) {
    let len = len.min(buffer.len());
    let started = Instant::now();
    shared.reading.lock().current = Some((started, len));
    let mut got = 0u32;
    // SAFETY: `buffer` has room for `len` bytes; `got` receives the count.
    let hr = unsafe { stream.Read(buffer.as_mut_ptr().cast(), len as u32, Some(&raw mut got)) };
    let took = started.elapsed();
    let given_up = std::mem::take(&mut *shared.reading.lock()).given_up;
    (got as usize, hr, took, given_up)
}

/// Puts a stream at `pos` bytes from its start.
fn seek_to(stream: &IStream, pos: u64) -> Result<(), HRESULT> {
    let pos = i64::try_from(pos).ok().ok_or(E_UNEXPECTED)?;
    // SAFETY: seeking a live stream.
    unsafe { stream.Seek(pos, STREAM_SEEK_SET, None) }.map_err(|e| e.code())
}

/// Makes this thread's calls cancellable (`CoCancelCall`); undone when it ends (`serve`).
fn enable_cancellation() -> bool {
    // SAFETY: plain call on this thread, paired in `serve`.
    unsafe { CoEnableCallCancellation(None) }.is_ok()
}
