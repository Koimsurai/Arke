//! The thread that talks to the program dropped files come from (#456).
//!
//! Every call into the source runs here: getting an item's contents, reading its stream, saving
//! its storage, ending a drop that finishes later. The worker writing the files only asks and
//! waits for the answers, watching Cancel meanwhile. A source that never answers (a remote
//! control program whose stream waits for its network) can't hold the copy: the worker stops
//! waiting, and once the source has been slow, Cancel also interrupts the call (`CoCancelCall`,
//! which ends a call into another process or apartment on the caller's side). The thread then
//! ends by itself once its call returns, and releases what it holds there.
//!
//! Streams are read a little at a time at first and in bigger pieces while the source keeps up
//! ([`ChunkSize`]), and one read ahead of the writing, so local sources (zip folders, Outlook)
//! copy at least as fast as before and slow ones aren't asked for more than they give at once.

use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, E_ABORT, E_UNEXPECTED, HANDLE, RPC_S_CALLPENDING};
use windows::Win32::System::Com::{
    CoCancelCall, CoDisableCallCancellation, CoEnableCallCancellation, CoWaitForMultipleHandles,
    IDataObject, STREAM_SEEK_SET,
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

/// How much to ask of a stream at a time.
///
/// Starts at [`FIRST_CHUNK`] and doubles, up to [`CHUNK`], after [`STREAK`] full reads in a row
/// that ran at [`KEEPING_UP`] or faster: a source reading from its own disk or memory gets there
/// within a few megabytes, so it pays for as few calls as before. A source that answers more
/// slowly (one round trip over the network a read) stays at small reads, where a call costs
/// nothing next to the wait; a read slower than [`SLOW`] halves the size again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChunkSize {
    size: usize,
    /// Reads of the current size in a row that kept up.
    streak: u32,
}

impl ChunkSize {
    pub(super) const fn new() -> Self {
        Self {
            size: FIRST_CHUNK,
            streak: 0,
        }
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
        } else if asked == self.size {
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
            self.shared.signal.wait(POLL_MS);
        }
    }

    /// Stops the thread's work: what is still asked isn't done, and a call into another
    /// process or apartment returns now (`RPC_E_CALL_CANCELED`). A call into an object of its
    /// own apartment can't be interrupted; the worker leaves it running.
    fn interrupt(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if self.thread.is_some() && !self.finished() {
            // SAFETY: names this link's thread, whose handle `thread` holds open, so the id is
            // no other thread's. Fails harmlessly when it isn't calling out.
            let _ = unsafe { CoCancelCall(self.thread_id, 0) };
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
            }
            self.shared.signal.wait(POLL_MS);
        }
        if let Some(thread) = self.thread.take() {
            // A panic was logged by the panic hook.
            let _ = thread.join();
        }
    }
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
            // SAFETY: pairs with the call in `Server::read`, on this thread.
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
                // SAFETY: seeking a live stream.
                let _ = unsafe { stream.Seek(0, STREAM_SEEK_SET, None) };
                self.open = Some(Contents::Stream(stream));
                Opened::Stream
            }
            Contents::Storage(storage) => {
                self.open = Some(Contents::Storage(storage));
                Opened::Storage
            }
        })
    }

    fn read(&mut self, mut buffer: Vec<u8>, len: usize, shared: &Shared) -> ReadDone {
        let (got, hr, took) = match &self.open {
            Some(Contents::Stream(_)) if shared.stop.load(Ordering::Relaxed) => {
                (0, E_ABORT, Duration::ZERO)
            }
            Some(Contents::Stream(stream)) => {
                if buffer.len() < len {
                    buffer.resize(len, 0);
                }
                let mut got = 0u32;
                let started = Instant::now();
                // SAFETY: `buffer` has room for `len` bytes; `got` receives the count.
                let hr = unsafe {
                    stream.Read(buffer.as_mut_ptr().cast(), len as u32, Some(&raw mut got))
                };
                (got as usize, hr, started.elapsed())
            }
            _ => (0, E_UNEXPECTED, Duration::ZERO),
        };
        if took >= CANCELLABLE_AFTER && !self.cancellable {
            // SAFETY: plain call on this thread, undone when it ends (`serve`).
            self.cancellable = unsafe { CoEnableCallCancellation(None) }.is_ok();
        }
        ReadDone {
            buffer,
            asked: len,
            got: got.min(len),
            hr,
            took,
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
