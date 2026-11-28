//! Where [`crate::fs::RealFs`] calls Win32 directly instead of going through std and notify:
//!
//! - [`DirWatcher`]: `ReadDirectoryChangesW` on one folder, waiting with no timeout. notify 8.2
//!   wakes every 100 ms per watch, and when the change buffer overflows it either logs the error
//!   and stops watching or hands back an empty buffer, so a folder that got thousands of new files
//!   at once (an extracted archive) kept showing the old list. Here an overflow is reported as
//!   [`WatchEvent::Rescan`] and watching goes on. Every watch shares one completion port and the
//!   thread waiting on it (`dir-watch`; folders on file servers have their own, `dir-watch-net`,
//!   so a server that takes long to answer never holds up local folders). Watches that want
//!   [`Change`]s ask for `ReadDirectoryChangesExW`'s extended records, which carry each item's
//!   size, times and attributes, so nothing has to be stat'ed; where the file system doesn't have
//!   them (Windows 10 before 1709, FAT, most file servers) the call fails and that watch reads
//!   plain records instead. A file server that takes the request and then answers in another
//!   layout or refuses it (a NAS, #536) gets one relist, and plain records from then on, in that
//!   watch and every later one on the volume.
//! - [`scan_sizes`]: one directory read for the folder size walk, with `FindFirstFileExW` (basic
//!   info, large fetch) and no allocation per file.
//! - [`read_dir`]: folder listings, the same entries as std's but fetched in large batches (one
//!   round trip to a file server per few hundred entries instead of a few dozen).
//! - [`is_network`]: whether a path is on a file server, from the drive type Windows records.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, FindClose,
    FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    GetCompressedFileSizeW, GetDriveTypeW, OPEN_EXISTING, ReadDirectoryChangesW,
    ReadDirectoryNotifyExtendedInformation, WIN32_FIND_DATAW,
};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatus, OVERLAPPED,
    PostQueuedCompletionStatus,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::INFINITE;
use windows::core::{PCSTR, PCWSTR, w};

use crate::entry::{Entry, EntryKind};
use crate::fs::{Change, ChangeCallback, ChangeEvent, SizeItem, WatchCallback, WatchEvent};

/// Change buffer of one watch. The kernel keeps a buffer of this size for changes made while no
/// read is pending; 64 KB (about 1,500 changes with short names, about 600 extended records) is
/// the most a network share accepts.
const BUF_BYTES: usize = 64 * 1024;
/// How long [`DirWatcher::close`] waits for the handle to be closed.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Name surrogate bit of a reparse tag: symlinks and junctions (std treats these as links).
const NAME_SURROGATE: u32 = 0x2000_0000;
/// What a watch is told about: names, attributes, size, times and security of the items in it.
const FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(
    FILE_NOTIFY_CHANGE_FILE_NAME.0
        | FILE_NOTIFY_CHANGE_DIR_NAME.0
        | FILE_NOTIFY_CHANGE_ATTRIBUTES.0
        | FILE_NOTIFY_CHANGE_SIZE.0
        | FILE_NOTIFY_CHANGE_LAST_WRITE.0
        | FILE_NOTIFY_CHANGE_CREATION.0
        | FILE_NOTIFY_CHANGE_SECURITY.0,
);
/// Completion key of a port's own wake-ups ([`Port::send`]); watches get keys from 1 up.
const COMMAND_KEY: usize = 0;

/// An owned kernel handle.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: kernel handles may be used from any thread; the owner decides when to close.
unsafe impl Send for Handle {}
// SAFETY: as above; posting to and waiting on a completion port are thread safe.
unsafe impl Sync for Handle {}

/// Space the file at `path` really takes on disk (`GetCompressedFileSizeW`: the compressed size
/// of an NTFS-compressed file, the allocated size of a sparse one). `None` when it can't be read.
/// One call by name, so only for the few files whose attributes say the cluster estimate is off.
pub(crate) fn allocated_size(path: &Path) -> Option<u64> {
    let wide = wide(path);
    let mut high = 0u32;
    // SAFETY: wide is NUL-terminated; the out pointer is valid for the call.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&raw mut high)) };
    // `u32::MAX` is also a valid low half; only the last error tells a failure.
    // SAFETY: reads the calling thread's last error, right after the call above.
    if low == u32::MAX && unsafe { GetLastError() } != NO_ERROR {
        return None;
    }
    Some((u64::from(high) << 32) | u64::from(low))
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Where a watch sends what it reads.
pub(crate) enum Sink {
    /// Changed paths ([`crate::fs::Fs::watch`]): plain records are all it needs.
    Paths(Arc<WatchCallback>),
    /// Changes described as far as the file system can ([`crate::fs::Fs::watch_changes`]).
    Changes(Arc<ChangeCallback>),
}

/// Which records a watch reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Records {
    /// `FILE_NOTIFY_INFORMATION`: names only.
    Plain,
    /// `FILE_NOTIFY_EXTENDED_INFORMATION`; `confirmed` once a read came back with them.
    Extended {
        /// A read succeeded with these records, so a later failure is not about them.
        confirmed: bool,
    },
}

impl Records {
    /// What a new watch of `root` asks for first: plain records where extended ones turned out
    /// unusable on its volume before ([`plain_only`]).
    fn first(sink: &Sink, root: &Path) -> Self {
        if matches!(sink, Sink::Paths(_)) {
            return Self::Plain;
        }
        let volume = volume_of(root);
        if plain_only().lock().contains(&volume) {
            Self::Plain
        } else {
            Self::Extended { confirmed: false }
        }
    }

    /// Whether a read that failed with Win32 error `code` means this watch's file system (or this
    /// Windows) has no extended records, so it should read plain ones instead. Also after extended
    /// reads came back: a file server that took them at first and then refuses them (#536) would
    /// otherwise stop the watch. A folder that is really gone fails the plain reads too.
    fn falls_back(self, code: u32) -> bool {
        /// `ERROR_INVALID_FUNCTION`, `ERROR_NOT_SUPPORTED`, `ERROR_INVALID_PARAMETER`,
        /// `ERROR_CALL_NOT_IMPLEMENTED`, `ERROR_INVALID_LEVEL`.
        const UNSUPPORTED: [u32; 5] = [1, 50, 87, 120, 124];
        matches!(self, Self::Extended { .. }) && UNSUPPORTED.contains(&code)
    }
}

/// Volumes (as [`volume_of`] gives them) whose extended records turned out unusable after the
/// file system took the request (#536): their later watches read plain records from the start.
/// Every relist opens a new watch, so without this each one would find out again, with a relist.
fn plain_only() -> &'static Mutex<std::collections::HashSet<String>> {
    static VOLUMES: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    VOLUMES.get_or_init(Default::default)
}

/// What a read that brought `raw` says, for a watch reading `records`, and what the watch reads
/// next. Extended records that don't read as such mean the file system took the request but
/// answers in another layout (a NAS, #536): what changed is unknown, so the folder is relisted
/// once, and plain records are read from then on (instead of a relist after every change).
fn read_records(records: Records, raw: &[u8], root: &Path) -> (Read, Records) {
    match records {
        Records::Plain => (Read::Paths(changed_paths(raw, root)), Records::Plain),
        Records::Extended { .. } => match extended_records(raw) {
            Some(records) => (
                Read::Changes(changes_of(records, root)),
                Records::Extended { confirmed: true },
            ),
            None => (Read::Rescan, Records::Plain),
        },
    }
}

/// Opens `path` for watching.
fn open_dir(path: &Path) -> io::Result<Handle> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let dir = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
    }?;
    Ok(Handle(dir))
}

/// `ReadDirectoryChangesExW`, looked up at run time: kernel32 has it from Windows 10 1709 on, and
/// a static import of a missing function would keep Arke from starting at all.
type ReadChangesEx = unsafe extern "system" fn(
    HANDLE,
    *mut core::ffi::c_void,
    u32,
    i32,
    u32,
    *mut u32,
    *mut OVERLAPPED,
    *const core::ffi::c_void,
    i32,
) -> i32;

fn read_changes_ex() -> Option<ReadChangesEx> {
    static API: OnceLock<Option<ReadChangesEx>> = OnceLock::new();
    *API.get_or_init(|| {
        // SAFETY: kernel32 is loaded in every process; the name is a NUL-terminated literal.
        let module = unsafe { GetModuleHandleW(w!("kernel32.dll")) }.ok()?;
        // SAFETY: as above.
        let proc =
            unsafe { GetProcAddress(module, PCSTR(c"ReadDirectoryChangesExW".as_ptr().cast())) }?;
        // SAFETY: the documented signature (BOOL is an i32, the information class an enum).
        Some(unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, ReadChangesEx>(proc)
        })
    })
}

/// Watches one folder (or a tree) until dropped. Dropping only asks the port's thread to stop
/// it, so it never blocks; [`Self::close`] also waits until the directory handle is closed.
pub(crate) struct DirWatcher {
    port: Arc<Port>,
    key: usize,
    done: flume::Receiver<()>,
}

impl DirWatcher {
    /// Opens `path` and starts watching it; returns once the first read is pending, so changes
    /// made after this returns are seen. Can block on network paths; background threads only.
    pub(crate) fn start(path: &Path, recursive: bool, sink: Sink) -> io::Result<Self> {
        static NEXT_KEY: AtomicUsize = AtomicUsize::new(COMMAND_KEY + 1);
        let dir = open_dir(path)?;
        let port = Port::get(is_network(path))?;
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        // SAFETY: both handles are alive; reads on `dir` now complete to the port under `key`.
        unsafe { CreateIoCompletionPort(dir.0, Some(port.handle.0), key, 0) }?;
        let (done_tx, done) = flume::bounded(1);
        let (ready_tx, ready) = flume::bounded(1);
        let slot = Slot {
            records: Records::first(&sink, path),
            key,
            dir,
            root: path.to_path_buf(),
            recursive,
            sink,
            overlapped: Box::default(),
            buf: vec![0u32; BUF_BYTES / 4],
            pending: false,
            stopping: false,
            done: done_tx,
        };
        port.send(Command::Start {
            key,
            slot: Box::new(slot),
            ready: ready_tx,
        });
        match ready.recv() {
            Ok(Ok(())) => Ok(Self { port, key, done }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(io::Error::other("watch thread ended")),
        }
    }

    /// Stops watching and returns once the directory handle is closed (or after 2 s).
    pub(crate) fn close(self) {
        self.port.send(Command::Stop(self.key));
        let _ = self.done.recv_timeout(CLOSE_WAIT);
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        // After `close` this asks again; the port ignores keys it no longer has.
        self.port.send(Command::Stop(self.key));
    }
}

/// What a port's thread is asked to do.
enum Command {
    /// Issue the first read of a new watch and report whether it could.
    Start {
        key: usize,
        slot: Box<Slot>,
        ready: flume::Sender<io::Result<()>>,
    },
    /// Stop the watch with this key.
    Stop(usize),
}

/// A completion port, and the thread that waits on it and reads every watch's changes.
struct Port {
    handle: Arc<Handle>,
    commands: flume::Sender<Command>,
}

impl Port {
    /// The port for local folders or the one for folders on file servers, made on first use.
    fn get(network: bool) -> io::Result<Arc<Self>> {
        static LOCAL: Mutex<Option<Arc<Port>>> = Mutex::new(None);
        static NETWORK: Mutex<Option<Arc<Port>>> = Mutex::new(None);
        let which = if network { &NETWORK } else { &LOCAL };
        let mut slot = which.lock();
        if let Some(port) = slot.as_ref() {
            return Ok(Arc::clone(port));
        }
        let port = Arc::new(Self::open(if network {
            "dir-watch-net"
        } else {
            "dir-watch"
        })?);
        *slot = Some(Arc::clone(&port));
        drop(slot);
        Ok(port)
    }

    fn open(thread: &str) -> io::Result<Self> {
        // SAFETY: a new port, not tied to any file yet; one thread waits on it.
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1) }?;
        let handle = Arc::new(Handle(handle));
        let (commands, received) = flume::unbounded();
        let waiting = Arc::clone(&handle);
        std::thread::Builder::new()
            .name(thread.into())
            .spawn(move || run_port(&waiting, &received))?;
        Ok(Self { handle, commands })
    }

    /// Queues `command` and wakes the port's thread to take it.
    fn send(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            // SAFETY: the port handle is alive while `self` is.
            let _ = unsafe { PostQueuedCompletionStatus(self.handle.0, 0, COMMAND_KEY, None) };
        }
    }
}

/// A port's thread: waits for completed reads and commands, forever.
fn run_port(port: &Handle, commands: &flume::Receiver<Command>) {
    let mut watches = Watches::default();
    loop {
        let mut bytes = 0u32;
        let mut key = 0usize;
        let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
        // SAFETY: the out pointers are valid for the call. A failed read also comes back here
        // (as an error with its OVERLAPPED); its status is read from the OVERLAPPED below.
        let waited = unsafe {
            GetQueuedCompletionStatus(
                port.0,
                &raw mut bytes,
                &raw mut key,
                &raw mut overlapped,
                INFINITE,
            )
        };
        if !overlapped.is_null() {
            watches.completed(key, port);
        } else if key == COMMAND_KEY && waited.is_ok() {
            while let Ok(command) = commands.try_recv() {
                watches.command(command);
            }
        } else {
            // Nothing was dequeued: only a closed port does that with no timeout.
            log::error!("folder watch port failed: {waited:?}");
            return;
        }
    }
}

/// The watches of one port, by completion key.
#[derive(Default)]
struct Watches(HashMap<usize, Slot>);

impl Watches {
    fn command(&mut self, command: Command) {
        match command {
            Command::Start {
                key,
                mut slot,
                ready,
            } => {
                let started = slot.start();
                let ok = started.is_ok();
                let _ = ready.send(started);
                if ok {
                    self.0.insert(key, *slot);
                }
            }
            Command::Stop(key) => {
                let Some(slot) = self.0.get_mut(&key) else {
                    return;
                };
                if slot.pending {
                    // Closed once the cancelled read comes back: until then the kernel may still
                    // write to its buffer.
                    slot.cancel();
                } else if let Some(slot) = self.0.remove(&key) {
                    slot.finish();
                }
            }
        }
    }

    fn completed(&mut self, key: usize, port: &Handle) {
        let Some(slot) = self.0.get_mut(&key) else {
            return;
        };
        slot.pending = false;
        if (slot.stopping || !slot.completed(port))
            && let Some(slot) = self.0.remove(&key)
        {
            slot.finish();
        }
    }
}

impl Drop for Watches {
    fn drop(&mut self) {
        // Only if the thread ends: a read still pending may be written to, so its buffer leaks.
        for (_, slot) in self.0.drain() {
            if slot.pending {
                std::mem::forget(slot);
            }
        }
    }
}

/// What one read brought.
enum Read {
    /// Changes were lost (the buffer overflowed): the folder has to be listed again.
    Rescan,
    /// Plain records: the paths named.
    Paths(Vec<PathBuf>),
    /// Extended records.
    Changes(Vec<Change>),
}

/// One watch, as its port's thread sees it.
struct Slot {
    dir: Handle,
    /// Completion key of the watch's reads on its port.
    key: usize,
    root: PathBuf,
    recursive: bool,
    sink: Sink,
    records: Records,
    /// The read's OVERLAPPED and buffer, on the heap so they stay put while the kernel has them.
    overlapped: Box<OVERLAPPED>,
    /// u32s: the buffer must be DWORD-aligned.
    buf: Vec<u32>,
    /// A read is pending: the kernel may write to `buf` and `overlapped` until it completes.
    pending: bool,
    /// Stopping: close once the pending read comes back.
    stopping: bool,
    done: flume::Sender<()>,
}

// SAFETY: the OVERLAPPED's handle and pointers are only touched by the port's thread once the
// slot is handed to it; before that it is just moved there.
unsafe impl Send for Slot {}

impl Slot {
    /// Issues the first read: extended records if asked for and available, else plain ones.
    fn start(&mut self) -> io::Result<()> {
        let issued = match self.issue() {
            Err(err) if self.records.falls_back(win32_code(&err)) => {
                log::debug!(
                    "no extended change records for {} ({err}), reading plain ones",
                    self.root.display()
                );
                self.records = Records::Plain;
                self.issue()
            }
            issued => issued,
        };
        issued.map_err(io::Error::from)
    }

    /// Issues the next read; it completes to the port.
    fn issue(&mut self) -> windows::core::Result<()> {
        *self.overlapped = OVERLAPPED::default();
        let buf = self.buf.as_mut_ptr().cast();
        let overlapped: *mut OVERLAPPED = &raw mut *self.overlapped;
        let issued = match self.records {
            // SAFETY: `buf` and `overlapped` stay allocated until the read completes (`pending`).
            Records::Plain => unsafe {
                ReadDirectoryChangesW(
                    self.dir.0,
                    buf,
                    BUF_BYTES as u32,
                    self.recursive,
                    FILTER,
                    None,
                    Some(overlapped),
                    None,
                )
            },
            Records::Extended { .. } => match read_changes_ex() {
                // SAFETY: as above; the arguments are those of ReadDirectoryChangesW plus the
                // information class.
                Some(read) => unsafe {
                    let ok = read(
                        self.dir.0,
                        buf,
                        BUF_BYTES as u32,
                        i32::from(self.recursive),
                        FILTER.0,
                        std::ptr::null_mut(),
                        overlapped,
                        std::ptr::null(),
                        ReadDirectoryNotifyExtendedInformation.0,
                    );
                    if ok == 0 {
                        Err(windows::core::Error::from_thread())
                    } else {
                        Ok(())
                    }
                },
                // Windows 10 before 1709: `ERROR_INVALID_FUNCTION`, which falls back.
                None => Err(windows::core::Error::from_hresult(
                    windows::core::HRESULT::from_win32(1),
                )),
            },
        };
        self.pending = issued.is_ok();
        issued
    }

    /// Cancels the pending read; the slot closes when it comes back.
    fn cancel(&mut self) {
        self.stopping = true;
        // SAFETY: `overlapped` is the pending request on `dir`.
        let _ = unsafe { CancelIoEx(self.dir.0, Some(&raw const *self.overlapped)) };
    }

    /// Closes the directory handle, then tells [`DirWatcher::close`].
    fn finish(self) {
        let Self { dir, done, .. } = self;
        drop(dir);
        let _ = done.send(());
    }

    /// The pending read came back: hands on what it brought and issues the next one. Returns
    /// whether watching goes on (not once the folder is gone, the drive removed, or the read
    /// cancelled).
    fn completed(&mut self, port: &Handle) -> bool {
        let mut bytes = 0u32;
        // SAFETY: the read completed (it was dequeued from the port), so this only reads its
        // status from `overlapped`.
        let result = unsafe {
            GetOverlappedResult(
                self.dir.0,
                &raw const *self.overlapped,
                &raw mut bytes,
                false,
            )
        };
        let before = self.records;
        let mut refused = None;
        let read = match result {
            // The buffer overflowed: the changes are lost, the folder has to be listed again.
            Ok(()) if bytes == 0 => Read::Rescan,
            Ok(()) => self.parse(bytes as usize),
            Err(err) if err.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => Read::Rescan,
            Err(err) if err.code() == ERROR_OPERATION_ABORTED.to_hresult() => return false,
            // Extended records refused only now: changes since the last read may be lost.
            Err(err) if self.records.falls_back(win32_code(&err)) => {
                self.records = Records::Plain;
                refused = Some(err);
                Read::Rescan
            }
            Err(err) => {
                log::debug!("stopped watching {}: {err}", self.root.display());
                return false;
            }
        };
        if let Records::Extended { confirmed } = before
            && self.records == Records::Plain
            && let Err(err) = self.switch_to_plain(port, confirmed, refused.as_ref())
        {
            self.deliver(read);
            log::debug!("stopped watching {}: {err}", self.root.display());
            return false;
        }
        // The records are copied out, so the buffer can take the next ones while they're handled.
        let issued = self.issue();
        self.deliver(read);
        if let Err(err) = issued {
            log::debug!("stopped watching {}: {err}", self.root.display());
            return false;
        }
        true
    }

    /// Extended records turned out unusable here after a read with them was taken (`confirmed`
    /// if one came back readable; `refused` the error if the file system refused them now rather
    /// than answering in another layout): plain records from now on, on a new handle (the kernel
    /// may keep queuing the old one's changes in the extended layout, and plain records are read
    /// without such checks), and for the later watches of this volume. Logged, once per watch,
    /// so a reporter's log shows it.
    fn switch_to_plain(
        &mut self,
        port: &Handle,
        confirmed: bool,
        refused: Option<&windows::core::Error>,
    ) -> io::Result<()> {
        let why = refused.map_or_else(|| "unreadable records".to_owned(), ToString::to_string);
        let after = if confirmed {
            " after readable ones"
        } else {
            ""
        };
        log::info!(
            "extended change records unusable for {} ({why}{after}): plain ones from now on",
            self.root.display()
        );
        let volume = volume_of(&self.root);
        plain_only().lock().insert(volume);
        let dir = open_dir(&self.root)?;
        // SAFETY: both handles are alive; reads on `dir` complete to the port under this watch's
        // key, as those on the old handle did.
        unsafe { CreateIoCompletionPort(dir.0, Some(port.0), self.key, 0) }?;
        // No read is pending on the old handle (this one just came back): closing it is safe.
        self.dir = dir;
        Ok(())
    }

    /// The first `len` bytes of the buffer, as records of the kind asked for ([`read_records`]).
    fn parse(&mut self, len: usize) -> Read {
        // SAFETY: u32 to u8 view of the same initialized memory.
        let raw: &[u8] =
            unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast(), self.buf.len() * 4) };
        let raw = &raw[..len.min(raw.len())];
        let (read, records) = read_records(self.records, raw, &self.root);
        self.records = records;
        read
    }

    /// Calls the watch's handler. A panic in it is logged, so it can't end every other watch.
    fn deliver(&self, read: Read) {
        let call = || match (&self.sink, read) {
            (Sink::Paths(on_event), Read::Rescan) => on_event(WatchEvent::Rescan),
            (Sink::Paths(on_event), Read::Paths(paths)) => on_event(WatchEvent::Changed(paths)),
            (Sink::Paths(on_event), Read::Changes(changes)) => on_event(WatchEvent::Changed(
                changes.iter().map(|c| c.path().to_path_buf()).collect(),
            )),
            (Sink::Changes(on_change), Read::Rescan) => on_change(ChangeEvent::Rescan),
            (Sink::Changes(on_change), Read::Paths(paths)) => on_change(ChangeEvent::Changed(
                paths.into_iter().map(Change::Unknown).collect(),
            )),
            (Sink::Changes(on_change), Read::Changes(changes)) => {
                on_change(ChangeEvent::Changed(changes));
            }
        };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).is_err() {
            log::error!("folder watch handler panicked: {}", self.root.display());
        }
    }
}

/// Paths named in a buffer of `FILE_NOTIFY_INFORMATION` records.
fn changed_paths(raw: &[u8], root: &Path) -> Vec<PathBuf> {
    const HEADER: usize = 12; // NextEntryOffset, Action, FileNameLength
    let mut paths = Vec::new();
    let mut at = 0usize;
    while let (Some(next), Some(name_bytes)) = (u32_at(raw, at), u32_at(raw, at + 8)) {
        let start = at + HEADER;
        let Some(name) = raw.get(start..start + name_bytes as usize) else {
            break;
        };
        paths.push(root.join(OsString::from_wide(&utf16(name))));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    paths
}

fn u32_at(raw: &[u8], at: usize) -> Option<u32> {
    raw.get(at..at.checked_add(4)?)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

fn u64_at(raw: &[u8], at: usize) -> Option<u64> {
    raw.get(at..at.checked_add(8)?)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect()
}

/// One `FILE_NOTIFY_EXTENDED_INFORMATION` record: what changed, and the item as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ExtendedRecord {
    /// `FILE_ACTION_*`.
    action: u32,
    /// FILETIMEs.
    created: u64,
    modified: u64,
    size: u64,
    attributes: u32,
    /// The reparse tag when `attributes` has the reparse point bit (else the EA size).
    reparse_tag: u32,
    /// Path relative to the watched folder, UTF-16.
    name: Vec<u16>,
}

/// Offsets in a `FILE_NOTIFY_EXTENDED_INFORMATION` record (NextEntryOffset at 0, Action at 4,
/// then CreationTime, LastModificationTime, LastChangeTime, LastAccessTime, AllocatedLength,
/// FileSize, FileAttributes, ReparsePointTag, FileId, ParentFileId, FileNameLength, FileName).
mod extended {
    pub(super) const ACTION: usize = 4;
    pub(super) const CREATED: usize = 8;
    pub(super) const MODIFIED: usize = 16;
    pub(super) const SIZE: usize = 48;
    pub(super) const ATTRIBUTES: usize = 56;
    pub(super) const REPARSE_TAG: usize = 60;
    pub(super) const NAME_LENGTH: usize = 80;
    pub(super) const NAME: usize = 84;
}

/// The records in a buffer of `FILE_NOTIFY_EXTENDED_INFORMATION`. `None` if it doesn't read as
/// one (truncated, offsets that don't fit, an unknown action): the caller can't trust any of it.
fn extended_records(raw: &[u8]) -> Option<Vec<ExtendedRecord>> {
    let mut records = Vec::new();
    let mut at = 0usize;
    loop {
        let next = u32_at(raw, at)? as usize;
        let action = u32_at(raw, at + extended::ACTION)?;
        let name_bytes = u32_at(raw, at + extended::NAME_LENGTH)? as usize;
        let start = at + extended::NAME;
        let name = raw.get(start..start.checked_add(name_bytes)?)?;
        let fits = next == 0 || (next >= extended::NAME + name_bytes && next.is_multiple_of(4));
        if !(1..=5).contains(&action) || !name_bytes.is_multiple_of(2) || !fits {
            return None;
        }
        records.push(ExtendedRecord {
            action,
            created: u64_at(raw, at + extended::CREATED)?,
            modified: u64_at(raw, at + extended::MODIFIED)?,
            size: u64_at(raw, at + extended::SIZE)?,
            attributes: u32_at(raw, at + extended::ATTRIBUTES)?,
            reparse_tag: u32_at(raw, at + extended::REPARSE_TAG)?,
            name: utf16(name),
        });
        if next == 0 {
            return Some(records);
        }
        at = at.checked_add(next)?;
    }
}

/// What extended records say about each item, in order. Deleted and renamed-away items are
/// [`Change::Removed`]; the others are described as the records have them, except that a name
/// with a `~` may be a short 8.3 name (the item is listed under its long one), and a record
/// without attributes describes nothing: those are [`Change::Unknown`], to be looked up.
fn changes_of(records: Vec<ExtendedRecord>, root: &Path) -> Vec<Change> {
    records
        .into_iter()
        .map(|record| {
            let path = root.join(OsString::from_wide(&record.name));
            if record.action == FILE_ACTION_REMOVED.0
                || record.action == FILE_ACTION_RENAMED_OLD_NAME.0
            {
                return Change::Removed(path);
            }
            let short = record.name.contains(&u16::from(b'~'));
            match path.file_name() {
                Some(name) if !short && record.attributes != 0 => {
                    let entry = record.entry(&name.to_string_lossy());
                    Change::Present(path, entry)
                }
                _ => Change::Unknown(path),
            }
        })
        .collect()
}

impl ExtendedRecord {
    /// The entry [`crate::fs::Fs::stat`] gives for the item, from the record.
    fn entry(&self, name: &str) -> Entry {
        let attributes = self.attributes;
        let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
        // std counts name surrogates (symlinks, junctions) as links.
        let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            && self.reparse_tag & NAME_SURROGATE != 0
        {
            EntryKind::Link { is_dir }
        } else if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let size = if kind == EntryKind::File {
            self.size
        } else {
            0
        };
        let time = |t: u64| filetime(t as u32, (t >> 32) as u32);
        Entry::new(name, kind, size, time(self.modified), attributes)
            .with_created(time(self.created))
    }
}

/// Converts a FILETIME (100 ns intervals since 1601) the way std does, so the result compares
/// equal to `Metadata::modified()` of the same entry.
fn filetime(low: u32, high: u32) -> Option<SystemTime> {
    crate::time::system_time_from_filetime((u64::from(high) << 32) | u64::from(low))
}

/// Owned `FindFirstFileExW` handle.
struct Find(HANDLE);

impl Drop for Find {
    fn drop(&mut self) {
        // SAFETY: owned find handle, closed once.
        let _ = unsafe { FindClose(self.0) };
    }
}

/// Longest `dir\*` pattern (in UTF-16 units) passed to `FindFirstFileExW` as is; longer ones
/// need the `\\?\` form, which std builds (see [`read_dir`]).
const FIND_PATTERN_MAX: usize = 247;

/// Lists `dir` with `FindFirstFileExW` asking for large fetches, the same entries as
/// `std::fs::read_dir` (which asks for small ones). On a network share each fetch is a round
/// trip to the server, so a folder of a few hundred files comes back in one instead of a dozen.
/// `None` for paths too long for a plain pattern: std lists those.
pub(crate) fn read_dir(dir: &Path) -> Option<io::Result<crate::fs::DirIter>> {
    // std refuses a path with a NUL in it; passed on, Windows would read it only up to the NUL.
    if dir.as_os_str().encode_wide().any(|c| c == 0) {
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        )));
    }
    let pattern = wide(&dir.join("*"));
    if pattern.len() > FIND_PATTERN_MAX + 1 {
        return None;
    }
    let volume = volume_of(dir);
    if no_large_fetch().lock().contains(&volume) {
        return None;
    }
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated; `data` is a WIN32_FIND_DATAW as FindExInfoBasic wants.
    let find = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    match find {
        Ok(find) => Some(Ok(Box::new(FindEntries {
            find: Some(Find(find)),
            first: Some(data),
        }))),
        Err(err) => match find_failure(win32_code(&err)) {
            // Nothing matched, as for the root of an empty drive (which has no `.`): std lists
            // that as empty too.
            FindFailure::Empty => Some(Ok(Box::new(std::iter::empty()))),
            FindFailure::Error => Some(Err(err.into())),
            FindFailure::Unsupported => {
                log::info!(
                    "{volume} doesn't take large directory fetches ({err}), listing it like std"
                );
                no_large_fetch().lock().insert(volume);
                None
            }
            FindFailure::Other => None,
        },
    }
}

/// What a failed `FindFirstFileExW` means for [`read_dir`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FindFailure {
    /// Nothing matched: an empty listing.
    Empty,
    /// The folder can't be listed (missing, access denied, the network is down): std would fail
    /// the same way, and trying again only waits on the network twice.
    Error,
    /// The file system or redirector rejects the large-fetch or basic-info flags: list it (and
    /// the rest of that volume) the way std does.
    Unsupported,
    /// Anything else: let std try.
    Other,
}

fn find_failure(code: u32) -> FindFailure {
    /// `ERROR_INVALID_FUNCTION`, `ERROR_NOT_SUPPORTED`, `ERROR_INVALID_PARAMETER`,
    /// `ERROR_INVALID_LEVEL`.
    const UNSUPPORTED: [u32; 4] = [1, 50, 87, 124];
    /// Path not found, access denied, a file not a folder, and network failures: bad network
    /// path, unexpected network error, name deleted, bad network name, semaphore timeout,
    /// network unreachable, host unreachable, no network provider, logon failure.
    const ERRORS: [u32; 13] = [3, 5, 267, 53, 59, 64, 67, 121, 1231, 1232, 1203, 1326, 1222];
    match code {
        2 => FindFailure::Empty,
        c if ERRORS.contains(&c) => FindFailure::Error,
        c if UNSUPPORTED.contains(&c) => FindFailure::Unsupported,
        _ => FindFailure::Other,
    }
}

/// The Win32 error code inside an HRESULT from windows-rs (`0x8007xxxx`).
const fn win32_code(err: &windows::core::Error) -> u32 {
    let hr = err.code().0.cast_unsigned();
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        hr
    }
}

/// Volumes (`x:` or `\\server\share`, lowercase) whose listings go through std: they rejected
/// a large fetch.
fn no_large_fetch() -> &'static parking_lot::Mutex<std::collections::HashSet<String>> {
    static VOLUMES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    VOLUMES.get_or_init(Default::default)
}

/// The volume part of a path (`x:` or `\\server\share`), lowercase.
fn volume_of(path: &Path) -> String {
    path.components()
        .next()
        .map_or_default(|c| c.as_os_str().to_string_lossy().to_lowercase())
}

/// Whether `path` is on a file server: `\\server\share\…` or a mapped drive. Reads the drive
/// type Windows records, no network access.
pub(crate) fn is_network(path: &Path) -> bool {
    /// `GetDriveTypeW`: a network drive.
    const DRIVE_REMOTE: u32 = 4;
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\UNC\") {
        return true;
    }
    if text.starts_with(r"\\") {
        return !text.starts_with(r"\\?\") && !text.starts_with(r"\\.\");
    }
    let mut chars = text.chars();
    let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
        return false;
    };
    if !letter.is_ascii_alphabetic() {
        return false;
    }
    let root = wide(Path::new(&format!("{letter}:\\")));
    // SAFETY: `root` is a NUL-terminated `X:\`; only reads the recorded drive type.
    (unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }) == DRIVE_REMOTE
}

/// Entries of one `FindFirstFileExW` listing, read as they're asked for.
struct FindEntries {
    /// `None` once the listing has ended (or failed).
    find: Option<Find>,
    /// What `FindFirstFileExW` returned, not handed out yet.
    first: Option<WIN32_FIND_DATAW>,
}

// SAFETY: a find handle may be used from any thread, one at a time (the iterator is not Sync).
unsafe impl Send for FindEntries {}

impl Iterator for FindEntries {
    type Item = io::Result<crate::entry::Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let find = self.find.as_ref()?;
            let data = if let Some(first) = self.first.take() {
                first
            } else {
                let mut data = WIN32_FIND_DATAW::default();
                // SAFETY: valid find handle and output buffer.
                if let Err(err) = unsafe { FindNextFileW(find.0, &raw mut data) } {
                    self.find = None;
                    return (err.code() != ERROR_NO_MORE_FILES.to_hresult())
                        .then(|| Err(err.into()));
                }
                data
            };
            if let Some(entry) = entry_of(&data) {
                return Some(Ok(entry));
            }
        }
    }
}

/// The entry `std::fs::DirEntry` would give for this find record; `None` for `.` and `..`.
fn entry_of(data: &WIN32_FIND_DATAW) -> Option<Entry> {
    let len = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.cFileName.len());
    let name = &data.cFileName[..len];
    if name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')] {
        return None;
    }
    let attributes = data.dwFileAttributes;
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    // dwReserved0 holds the reparse tag when the entry is a reparse point; std counts name
    // surrogates (symlinks, junctions) as links.
    let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        && data.dwReserved0 & NAME_SURROGATE != 0
    {
        EntryKind::Link { is_dir }
    } else if is_dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    let size = if kind == EntryKind::File {
        (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow)
    } else {
        0
    };
    let time =
        |t: windows::Win32::Foundation::FILETIME| filetime(t.dwLowDateTime, t.dwHighDateTime);
    let name = OsString::from_wide(name);
    Some(
        Entry::new(
            name.to_string_lossy(),
            kind,
            size,
            time(data.ftLastWriteTime),
            attributes,
        )
        .with_created(time(data.ftCreationTime)),
    )
}

/// The name the item at `path` is listed under in its folder (see
/// [`crate::fs::Fs::listed_name`]). `None` when it can't be found this way: gone, a drive root,
/// a path too long for a plain one, or a name with wildcard characters.
pub(crate) fn listed_name(path: &Path) -> Option<OsString> {
    let name = path.file_name()?;
    if name
        .encode_wide()
        .any(|c| c == u16::from(b'*') || c == u16::from(b'?') || c == 0)
    {
        return None;
    }
    let wide_path = wide(path);
    if wide_path.len() > FIND_PATTERN_MAX + 1 {
        return None;
    }
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `wide_path` is NUL-terminated; `data` is a WIN32_FIND_DATAW as FindExInfoBasic
    // wants.
    let find = unsafe {
        FindFirstFileExW(
            PCWSTR(wide_path.as_ptr()),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            windows::Win32::Storage::FileSystem::FIND_FIRST_EX_FLAGS(0),
        )
    }
    .ok()?;
    drop(Find(find));
    let len = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.cFileName.len());
    Some(OsString::from_wide(&data.cFileName[..len]))
}

/// Lists `dir` for a folder size walk (see [`crate::fs::Fs::scan_sizes`]).
pub(crate) fn scan_sizes(dir: &Path, each: &mut dyn FnMut(SizeItem)) -> io::Result<()> {
    let pattern = wide(&dir.join("*"));
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated; `data` is a WIN32_FIND_DATAW as FindExInfoBasic wants.
    let find = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }?;
    let find = Find(find);
    loop {
        each(size_item(&data));
        // SAFETY: valid find handle and output buffer.
        if let Err(err) = unsafe { FindNextFileW(find.0, &raw mut data) } {
            return if err.code() == ERROR_NO_MORE_FILES.to_hresult() {
                Ok(())
            } else {
                Err(err.into())
            };
        }
    }
}

fn size_item(data: &WIN32_FIND_DATAW) -> SizeItem {
    let attributes = data.dwFileAttributes;
    // dwReserved0 holds the reparse tag when the entry is a reparse point.
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 && data.dwReserved0 & NAME_SURROGATE != 0 {
        return SizeItem::Link;
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return SizeItem::File(
            (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        );
    }
    let len = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.cFileName.len());
    let name = &data.cFileName[..len];
    if name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')] {
        return SizeItem::Skip;
    }
    SizeItem::Dir {
        name: OsString::from_wide(name),
        modified: filetime(
            data.ftLastWriteTime.dwLowDateTime,
            data.ftLastWriteTime.dwHighDateTime,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_change_records() {
        let mut raw = Vec::new();
        let record = |next: u32, name: &str, raw: &mut Vec<u8>| {
            let units: Vec<u16> = name.encode_utf16().collect();
            raw.extend(next.to_le_bytes());
            raw.extend(1u32.to_le_bytes());
            raw.extend((units.len() as u32 * 2).to_le_bytes());
            raw.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        };
        record(24, "ab.txt", &mut raw); // 12 + 12 bytes
        record(0, "新しい", &mut raw);
        let paths = changed_paths(&raw, Path::new(r"C:\w"));
        assert_eq!(
            paths,
            [PathBuf::from(r"C:\w\ab.txt"), PathBuf::from(r"C:\w\新しい")]
        );
        // A truncated record ends the list instead of reading past the buffer.
        assert_eq!(changed_paths(&raw[..30], Path::new(r"C:\w")).len(), 1);
    }

    /// The large-fetch listing gives exactly what std's listing does: names, kinds (a junction is
    /// a link to a folder), sizes, times and attributes; an empty folder is empty and a missing one
    /// is an error.
    #[test]
    fn listing_matches_std() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("\u{5831}\u{544a}.md"), vec![0u8; 70_000]).unwrap();
        let linked = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(dir.join("link"))
            .arg(dir.join("sub"))
            .output()
            .unwrap()
            .status
            .success();
        assert!(linked, "failed to create junction");

        let key = |e: &crate::entry::Entry| {
            (
                e.name.to_string(),
                e.kind,
                e.size,
                e.modified,
                e.created,
                e.attributes,
            )
        };
        let mut ours: Vec<_> = read_dir(&dir)
            .unwrap()
            .unwrap()
            .map(|e| key(&e.unwrap()))
            .collect();
        let mut std_list: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| key(&crate::entry::Entry::from_dir_entry(&e.unwrap()).unwrap()))
            .collect();
        ours.sort();
        std_list.sort();
        assert_eq!(ours.len(), 5);
        assert_eq!(ours, std_list);
        assert!(
            ours.iter()
                .any(|e| e.0 == "link" && e.1 == crate::entry::EntryKind::Link { is_dir: true })
        );
        assert_eq!(read_dir(&dir.join("empty")).unwrap().unwrap().count(), 0);
        assert!(read_dir(&dir.join("missing")).unwrap().is_err());
        let _ = std::fs::remove_dir(dir.join("link"));
    }

    /// A path with a NUL in it is refused, as std refuses it, instead of listing what comes
    /// before the NUL.
    #[test]
    fn a_path_with_a_nul_is_refused() {
        let dir = std::env::temp_dir();
        let mut bad = dir.into_os_string();
        bad.push("\0x");
        let err = read_dir(Path::new(&bad)).unwrap().err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Errors that std would hit too are returned; a redirector rejecting the large-fetch flags
    /// sends that volume's listings through std from then on.
    #[test]
    fn unsupported_large_fetches_fall_back_to_std() {
        assert_eq!(find_failure(2), FindFailure::Empty);
        for code in [3, 5, 53, 67, 1231] {
            assert_eq!(find_failure(code), FindFailure::Error, "{code}");
        }
        for code in [1, 50, 87, 124] {
            assert_eq!(find_failure(code), FindFailure::Unsupported, "{code}");
        }
        assert_eq!(find_failure(1117), FindFailure::Other);
        let err = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(87));
        assert_eq!(win32_code(&err), 87);

        let dir = std::env::temp_dir();
        assert!(read_dir(&dir).is_some());
        let volume = volume_of(&dir);
        no_large_fetch().lock().insert(volume.clone());
        assert!(read_dir(&dir).is_none(), "listed like std");
        assert!(crate::fs::Fs::read_dir(&crate::fs::RealFs, &dir).is_ok());
        no_large_fetch().lock().remove(&volume);
        assert_eq!(volume_of(Path::new(r"\\NAS\Share\docs")), r"\\nas\share");
        assert_eq!(volume_of(Path::new(r"X:\docs")), "x:");
    }

    #[test]
    fn network_paths_are_told_by_the_path_or_the_drive_type() {
        assert!(is_network(Path::new(r"\\nas\share\docs")));
        assert!(is_network(Path::new(r"\\?\UNC\nas\share")));
        assert!(!is_network(Path::new(r"\\?\C:\Windows")));
        assert!(!is_network(Path::new(r"\\.\pipe\x")));
        assert!(!is_network(Path::new("relative")));
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        assert!(!is_network(&Path::new(&system).join("Windows")));
    }

    #[test]
    fn file_times_match_std() {
        let dir = std::env::temp_dir();
        let listed = std::fs::metadata(&dir).unwrap().modified().unwrap();
        let since = listed.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let intervals = since.as_secs() * 10_000_000
            + u64::from(since.subsec_nanos()) / 100
            + 116_444_736_000_000_000;
        assert_eq!(
            filetime(intervals as u32, (intervals >> 32) as u32),
            Some(listed)
        );
        assert!(filetime(0, 0).is_some(), "1601 is before 1970 but valid");
    }

    /// 2025-11-29 02:00:00 UTC and an hour later, as FILETIMEs.
    const CREATED: u64 = 134_088_552_000_000_000;
    const MODIFIED: u64 = CREATED + 36_000_000_000;

    /// One `FILE_NOTIFY_EXTENDED_INFORMATION` record as the kernel lays it out, padded to 8 bytes;
    /// `last` ends the buffer (NextEntryOffset 0).
    fn record(
        action: u32,
        name: &str,
        size: u64,
        attributes: u32,
        tag: u32,
        last: bool,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (extended::NAME + units.len() * 2).next_multiple_of(8);
        let mut raw = vec![0u8; len];
        let mut put = |at: usize, bytes: &[u8]| raw[at..at + bytes.len()].copy_from_slice(bytes);
        put(0, &(if last { 0 } else { len as u32 }).to_le_bytes());
        put(extended::ACTION, &action.to_le_bytes());
        put(extended::CREATED, &CREATED.to_le_bytes());
        put(extended::MODIFIED, &MODIFIED.to_le_bytes());
        put(24, &(MODIFIED + 1).to_le_bytes()); // LastChangeTime
        put(32, &(MODIFIED + 2).to_le_bytes()); // LastAccessTime
        put(40, &size.next_multiple_of(4096).to_le_bytes()); // AllocatedLength
        put(extended::SIZE, &size.to_le_bytes());
        put(extended::ATTRIBUTES, &attributes.to_le_bytes());
        put(extended::REPARSE_TAG, &tag.to_le_bytes());
        put(64, &0x0001_0000_0000_1234u64.to_le_bytes()); // FileId
        put(72, &0x0001_0000_0000_0005u64.to_le_bytes()); // ParentFileId
        put(
            extended::NAME_LENGTH,
            &(units.len() as u32 * 2).to_le_bytes(),
        );
        let name: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        put(extended::NAME, &name);
        raw
    }

    /// The offsets used to read the records are the SDK's.
    #[test]
    fn extended_record_offsets_match_the_sdk() {
        use std::mem::offset_of;
        use windows::Win32::Storage::FileSystem::FILE_NOTIFY_EXTENDED_INFORMATION as Info;
        assert_eq!(offset_of!(Info, Action), extended::ACTION);
        assert_eq!(offset_of!(Info, CreationTime), extended::CREATED);
        assert_eq!(offset_of!(Info, LastModificationTime), extended::MODIFIED);
        assert_eq!(offset_of!(Info, FileSize), extended::SIZE);
        assert_eq!(offset_of!(Info, FileAttributes), extended::ATTRIBUTES);
        assert_eq!(offset_of!(Info, Anonymous), extended::REPARSE_TAG);
        assert_eq!(offset_of!(Info, FileNameLength), extended::NAME_LENGTH);
        assert_eq!(offset_of!(Info, FileName), extended::NAME);
    }

    /// Extended records become changes: a new file with its size, times and attributes, a
    /// deleted file, a renamed one (old name gone, new name there), a junction (a link to a
    /// folder), and names that still have to be looked up.
    #[test]
    fn parses_extended_records() {
        const ARCHIVE: u32 = 0x20;
        const DIRECTORY: u32 = 0x10;
        const REPARSE: u32 = 0x400;
        const MOUNT_POINT: u32 = 0xA000_0003;
        let mut raw = Vec::new();
        raw.extend(record(1, "報告 2026.docx", 70_000, ARCHIVE, 0, false));
        raw.extend(record(2, "old.txt", 5, ARCHIVE, 0, false));
        raw.extend(record(4, "draft.md", 3, ARCHIVE, 0, false));
        raw.extend(record(5, "final.md", 3, ARCHIVE, 0, false));
        raw.extend(record(
            3,
            "link",
            0,
            DIRECTORY | REPARSE,
            MOUNT_POINT,
            false,
        ));
        raw.extend(record(3, "sub", 4096, DIRECTORY, 0, false));
        raw.extend(record(3, "LONGNA~1.TXT", 9, ARCHIVE, 0, false));
        raw.extend(record(1, "no-info.bin", 9, 0, 0, true));
        let records = extended_records(&raw).unwrap();
        assert_eq!(records.len(), 8);
        let root = Path::new(r"C:\w");
        let changes = changes_of(records, root);
        let time = |t: u64| filetime(t as u32, (t >> 32) as u32);

        let Change::Present(path, entry) = &changes[0] else {
            panic!("{:?}", changes[0]);
        };
        assert_eq!(path, &root.join("報告 2026.docx"));
        assert_eq!(&*entry.name, "報告 2026.docx");
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(entry.size, 70_000);
        assert_eq!(entry.attributes, ARCHIVE);
        assert_eq!(entry.modified, time(MODIFIED));
        assert_eq!(entry.created, time(CREATED));
        assert!(entry.modified.is_some());

        assert!(matches!(&changes[1], Change::Removed(p) if p == &root.join("old.txt")));
        assert!(matches!(&changes[2], Change::Removed(p) if p == &root.join("draft.md")));
        assert!(
            matches!(&changes[3], Change::Present(p, e) if p == &root.join("final.md") && e.size == 3)
        );
        let Change::Present(_, link) = &changes[4] else {
            panic!("{:?}", changes[4]);
        };
        assert_eq!(link.kind, EntryKind::Link { is_dir: true });
        assert_eq!(link.size, 0);
        let Change::Present(_, sub) = &changes[5] else {
            panic!("{:?}", changes[5]);
        };
        assert_eq!(
            (sub.kind, sub.size),
            (EntryKind::Dir, 0),
            "folders have no size"
        );
        // Possibly a short 8.3 name, and a record that describes nothing: looked up.
        assert!(matches!(&changes[6], Change::Unknown(p) if p == &root.join("LONGNA~1.TXT")));
        assert!(matches!(&changes[7], Change::Unknown(p) if p == &root.join("no-info.bin")));
    }

    /// A buffer that doesn't read as extended records is refused whole, never read past its end.
    #[test]
    fn malformed_extended_records_are_refused() {
        let one = record(1, "a.txt", 1, 0x20, 0, true);
        assert_eq!(extended_records(&one).unwrap().len(), 1);
        // Truncated: in the name, and in the header.
        assert!(extended_records(&one[..one.len() - 4]).is_none());
        assert!(extended_records(&one[..40]).is_none());
        assert!(extended_records(&[]).is_none());
        // An action Windows doesn't have.
        let mut bad = one.clone();
        bad[extended::ACTION] = 9;
        assert!(extended_records(&bad).is_none());
        // An odd name length.
        let mut bad = one.clone();
        bad[extended::NAME_LENGTH] = 9;
        assert!(extended_records(&bad).is_none());
        // A next record that would overlap this one, or isn't aligned.
        let mut bad = record(1, "a.txt", 1, 0x20, 0, false);
        bad[0] = 40;
        bad.extend(&one);
        assert!(extended_records(&bad).is_none());
        let mut bad = record(1, "a.txt", 1, 0x20, 0, false);
        bad[0] += 2;
        bad.extend(&one);
        assert!(extended_records(&bad).is_none());
        // Pointing past the end.
        let mut bad = record(1, "a.txt", 1, 0x20, 0, false);
        bad.extend(&one[..50]);
        assert!(extended_records(&bad).is_none());
        // Plain records read as extended ones.
        let mut plain = Vec::new();
        plain.extend(0u32.to_le_bytes());
        plain.extend(1u32.to_le_bytes());
        plain.extend(10u32.to_le_bytes());
        plain.extend("a.txt".encode_utf16().flat_map(u16::to_le_bytes));
        assert!(extended_records(&plain).is_none());
    }

    /// Watches that want described changes ask for extended records first, and read plain ones
    /// when that fails as unsupported, also after extended reads came back (#536); nothing else
    /// falls back.
    #[test]
    fn extended_records_fall_back_only_when_unsupported() {
        let paths = Sink::Paths(Arc::new(Box::new(|_| {}) as WatchCallback));
        let changes = Sink::Changes(Arc::new(Box::new(|_| {}) as ChangeCallback));
        let root = Path::new(r"C:\w");
        assert_eq!(Records::first(&paths, root), Records::Plain);
        let fresh = Records::first(&changes, root);
        assert_eq!(fresh, Records::Extended { confirmed: false });
        let confirmed = Records::Extended { confirmed: true };
        // Invalid function (FAT, old Windows), not supported, invalid parameter or level.
        for code in [1, 50, 87, 120, 124] {
            assert!(fresh.falls_back(code), "{code}");
            assert!(
                confirmed.falls_back(code),
                "{code} after extended reads worked"
            );
            assert!(
                !Records::Plain.falls_back(code),
                "{code} with plain records"
            );
        }
        // Access denied, folder gone, network errors: a real failure, as with plain records.
        for code in [5, 2, 3, 53, 64, 1117] {
            assert!(!fresh.falls_back(code), "{code}");
            assert!(!confirmed.falls_back(code), "{code}");
        }
        let missing = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(1));
        assert!(fresh.falls_back(win32_code(&missing)));
    }

    /// A file server that takes the request for extended records and answers in another layout
    /// (#536): the watch relists once and reads plain records from then on (not a relist after
    /// every change), the same when it took readable ones before, and later watches of the
    /// volume start with plain records.
    #[test]
    fn unreadable_extended_records_switch_the_watch_to_plain_ones() {
        let root = Path::new(r"\\nas-536\share\docs");
        let mut plain = Vec::new();
        plain.extend(0u32.to_le_bytes());
        plain.extend(3u32.to_le_bytes());
        plain.extend(10u32.to_le_bytes());
        plain.extend("a.txt".encode_utf16().flat_map(u16::to_le_bytes));

        let (read, records) = read_records(Records::Extended { confirmed: false }, &plain, root);
        assert!(matches!(read, Read::Rescan));
        assert_eq!(records, Records::Plain);
        let (read, records) = read_records(records, &plain, root);
        assert!(
            matches!(&read, Read::Paths(p) if p == &[root.join("a.txt")]),
            "read as paths"
        );
        assert_eq!(records, Records::Plain);
        let (read, _) = read_records(records, &plain, root);
        assert!(matches!(read, Read::Paths(_)), "no second relist");

        // Readable at first, then not.
        let good = record(1, "b.txt", 1, 0x20, 0, true);
        let (read, records) = read_records(Records::Extended { confirmed: false }, &good, root);
        assert!(matches!(&read, Read::Changes(c) if c.len() == 1));
        assert_eq!(records, Records::Extended { confirmed: true });
        let (read, records) = read_records(records, &plain, root);
        assert!(matches!(read, Read::Rescan));
        assert_eq!(records, Records::Plain);

        // What the switch remembers: later watches of the volume, and only of it, read plain.
        let changes = Sink::Changes(Arc::new(Box::new(|_| {}) as ChangeCallback));
        let other = Path::new(r"\\nas-536\other");
        assert_eq!(
            Records::first(&changes, other),
            Records::Extended { confirmed: false }
        );
        plain_only().lock().insert(volume_of(root));
        let later = Path::new(r"\\NAS-536\Share\docs\sub");
        assert_eq!(Records::first(&changes, later), Records::Plain);
        assert_eq!(
            Records::first(&changes, other),
            Records::Extended { confirmed: false }
        );
        plain_only().lock().remove(&volume_of(root));
    }

    /// The temp folder (NTFS, so extended records): every change arrives, what the records
    /// describe is what stat says, and a deleted file needs no lookup. Closing returns at once.
    #[test]
    fn described_changes_match_stat() {
        let dir = std::env::temp_dir().join(format!("arke-winfs-ex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let seen: Arc<Mutex<Vec<Change>>> = Arc::default();
        let sink = Arc::clone(&seen);
        let on_change: ChangeCallback = Box::new(move |event| {
            if let ChangeEvent::Changed(changes) = event {
                sink.lock().extend(changes);
            }
        });
        let watcher = DirWatcher::start(&dir, false, Sink::Changes(Arc::new(on_change))).unwrap();
        let file = dir.join("data.bin");
        std::fs::write(&file, vec![7u8; 12_345]).unwrap();
        let wait = |done: &dyn Fn(&[Change]) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !done(&seen.lock()) {
                assert!(std::time::Instant::now() < deadline, "{:?}", seen.lock());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let sized = |c: &Change| matches!(c, Change::Present(_, e) if e.size == 12_345);
        let unknown = |c: &Change| matches!(c, Change::Unknown(p) if p == &file);
        wait(&|changes| changes.iter().any(|c| sized(c) || unknown(c)));
        let stat = crate::fs::Fs::stat(&crate::fs::RealFs, &file).unwrap();
        let described = seen.lock().iter().rev().find_map(|c| match c {
            Change::Present(_, e) => Some(e.clone()),
            _ => None,
        });
        if let Some(entry) = described {
            let key = |e: &Entry| (e.name.to_string(), e.kind, e.size, e.attributes, e.created);
            assert_eq!(key(&entry), key(&stat));
            assert!(entry.modified.is_some());
        }
        std::fs::remove_file(&file).unwrap();
        wait(&|changes| {
            changes
                .iter()
                .any(|c| matches!(c, Change::Removed(p) | Change::Unknown(p) if p == &file))
                && !matches!(changes.last(), Some(Change::Present(..)))
        });
        let started = std::time::Instant::now();
        watcher.close();
        assert!(started.elapsed() < Duration::from_secs(1), "close waited");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// All watches share the same thread: local folders one, folders on file servers another.
    #[test]
    fn watches_share_one_port_per_kind() {
        let a = Port::get(false).unwrap();
        let b = Port::get(false).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let net = Port::get(true).unwrap();
        assert!(!Arc::ptr_eq(&a, &net));
    }
}
