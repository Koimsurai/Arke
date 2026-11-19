//! Virtual files dropped from other programs (#206).
//!
//! Some programs don't list files on disk when something is dragged out of them. They describe
//! each item (`FileGroupDescriptorW`, or the ANSI `FileGroupDescriptor`) and hand over its bytes
//! through `FileContents` when asked: Outlook attachments and messages, images dragged out of a
//! browser, files inside a zip folder in File Explorer, a phone's files. The platform layer drops
//! those as a marker path and keeps the data object for the drop handler to take
//! ([`dropped_data`]).
//!
//! The data object belongs to the thread that received the drop, so the drop is read there:
//! the descriptor, and (unless the source can finish later) each item's contents. Bytes in
//! memory are copied out; streams and storages are handed to the worker thread through agile
//! references. A source that supports `IDataObjectAsyncCapability` (File Explorer's zip
//! folders, browsers) is told the drop finishes later, and the worker asks it for the contents
//! itself.
//!
//! The worker writes the items into a folder of Arke's own in the temp folder (folders, the
//! relative names inside them, sizes and times from the descriptor), a chunk at a time with
//! progress and Cancel, then moves them into the target like any other drop. Name conflicts in
//! the target therefore get the same question as a copy.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use rust_i18n::t;
use windows::Win32::Foundation::{E_ABORT, E_FAIL, FILETIME, HGLOBAL, S_FALSE, S_OK};
use windows::Win32::Globalization::{CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows::Win32::System::Com::StructuredStorage::{
    IStorage, STGFMT_STORAGE, StgCreateStorageEx,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, DVASPECT_CONTENT, FORMATETC, IDataObject,
    IStream, STGC_DEFAULT, STGM_CREATE, STGM_READWRITE, STGM_SHARE_EXCLUSIVE, STREAM_SEEK_SET,
    TYMED, TYMED_HGLOBAL, TYMED_ISTORAGE, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, DROPEFFECT_NONE, OleGetClipboard, OleInitialize, OleUninitialize,
    ReleaseStgMedium,
};
use windows::Win32::UI::Shell::{
    FD_ACCESSTIME, FD_ATTRIBUTES, FD_CREATETIME, FD_FILESIZE, FD_WRITESTIME, FILEDESCRIPTORA,
    FILEDESCRIPTORW, IDataObjectAsyncCapability,
};
use windows::core::{AgileReference, HRESULT, Interface as _, PCWSTR, w};

use crate::shell::{OpProgress, ShellError};

/// Bytes read from a stream at a time.
pub(crate) const CHUNK: usize = 1 << 20;

/// The data object of a drop of virtual files being handled, on the thread that received it.
pub(crate) fn dropped_data() -> Option<IDataObject> {
    #[cfg(test)]
    if let Some(data) = tests::INJECTED.with(|d| d.borrow_mut().take()) {
        return Some(data);
    }
    gpui_windows::take_dropped_data_object()
}

/// Whether the clipboard offers virtual files (a Remote Desktop copy, an Outlook attachment,
/// a file inside a zip folder): described by a descriptor, with no `CF_HDROP` list on disk.
/// Only checks the format, so it is cheap and never opens the clipboard.
pub(crate) fn clipboard_has_virtual_files() -> bool {
    // SAFETY: only checks format availability; the clipboard isn't opened.
    unsafe {
        IsClipboardFormatAvailable(u32::from(registered(w!("FileGroupDescriptorW")))).is_ok()
            || IsClipboardFormatAvailable(u32::from(registered(w!("FileGroupDescriptor")))).is_ok()
    }
}

/// Reads the virtual files on the clipboard, `None` if it has none (#221). Pasting reads them
/// like a drop, but from the clipboard's data object. Background thread with OLE
/// ([`ComThread::ole`]), which must stay alive until the files are written: the data object may
/// be a proxy to the program that copied them (Remote Desktop's clipboard).
pub(crate) fn clipboard_virtual_files() -> Result<Option<VirtualDrop>, ShellError> {
    if !clipboard_has_virtual_files() {
        return Ok(None);
    }
    // SAFETY: plain call; OLE is initialized on this thread by the caller.
    let data = unsafe { OleGetClipboard() }.map_err(ShellError::from)?;
    VirtualDrop::take(&data).map(Some)
}

/// Whether dropped paths stand for virtual files (see the module docs).
pub(crate) fn is_virtual(paths: &[PathBuf]) -> bool {
    gpui_windows::is_virtual_drop(paths)
}

/// One item of the descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    /// Index in `FileContents` (`lindex`).
    index: i32,
    /// The relative name, one safe name per folder level.
    parts: Vec<String>,
    is_dir: bool,
    size: Option<u64>,
    created: Option<SystemTime>,
    accessed: Option<SystemTime>,
    written: Option<SystemTime>,
}

/// An item's contents, ready for the worker.
enum Fetched {
    Bytes(Vec<u8>),
    Stream(AgileReference<IStream>),
    Storage(AgileReference<IStorage>),
    Failed(windows::core::Error),
}

/// An item's contents on the thread reading them.
enum Contents {
    Bytes(Vec<u8>),
    Stream(IStream),
    Storage(IStorage),
}

enum Source {
    /// Read during the drop, one per item (`None` for folders).
    Fetched(Vec<Option<Fetched>>),
    /// The source finishes later (`IDataObjectAsyncCapability`): read by the worker.
    Later {
        data: AgileReference<IDataObject>,
        operation: AsyncOperation,
    },
}

/// A drop of virtual files, read from its data object.
pub(crate) struct VirtualDrop {
    items: Vec<Item>,
    format: u16,
    source: Source,
}

/// What [`VirtualDrop::extract`] wrote.
#[derive(Debug, Default)]
pub(crate) struct Extraction {
    /// The top-level items written, in the order the source listed them.
    pub(crate) top: Vec<PathBuf>,
    pub(crate) cancelled: bool,
    /// The first item that couldn't be written, if any.
    pub(crate) failed: Option<ShellError>,
}

impl VirtualDrop {
    /// Reads the drop. Call on the thread that received it, while it is handled.
    pub(crate) fn take(data: &IDataObject) -> Result<Self, ShellError> {
        let items = read_descriptor(data).ok_or_else(nothing_offered)?;
        if items.is_empty() {
            return Err(nothing_offered());
        }
        let format = registered(w!("FileContents"));
        if let Some(operation) = AsyncOperation::start(data) {
            match AgileReference::new(data) {
                Ok(data) => {
                    return Ok(Self {
                        items,
                        format,
                        source: Source::Later { data, operation },
                    });
                }
                Err(e) => {
                    log::warn!("can't hand a dropped data object to a worker: {e}");
                    operation.end(E_FAIL, false);
                }
            }
        }
        let fetched = items
            .iter()
            .map(|item| {
                (!item.is_dir).then(|| match contents(data, format, item) {
                    Ok(Contents::Bytes(bytes)) => Fetched::Bytes(bytes),
                    Ok(Contents::Stream(s)) => {
                        AgileReference::new(&s).map_or_else(Fetched::Failed, Fetched::Stream)
                    }
                    Ok(Contents::Storage(s)) => {
                        AgileReference::new(&s).map_or_else(Fetched::Failed, Fetched::Storage)
                    }
                    Err(e) => Fetched::Failed(e),
                })
            })
            .collect();
        Ok(Self {
            items,
            format,
            source: Source::Fetched(fetched),
        })
    }

    /// The first top-level name and how many top-level items there are.
    pub(crate) fn subject(&self) -> Option<(String, usize)> {
        let mut tops: Vec<&str> = Vec::new();
        for item in &self.items {
            if let Some(top) = item.parts.first()
                && !tops.contains(&top.as_str())
            {
                tops.push(top);
            }
        }
        Some(((*tops.first()?).to_owned(), tops.len()))
    }

    /// Whether the worker asks the source for the contents (it said the drop may finish later).
    #[cfg(test)]
    pub(crate) const fn reads_later(&self) -> bool {
        matches!(self.source, Source::Later { .. })
    }

    /// Writes the items into `stage`, an empty folder. Background thread, which must not have
    /// COM initialized as single-threaded ([`ComThread`] sets it up).
    pub(crate) fn extract(self, stage: &Path, progress: &OpProgress) -> Extraction {
        let Self {
            items,
            format,
            source,
        } = self;
        let known: u64 = items.iter().filter_map(|i| i.size).sum();
        let files = items.iter().filter(|i| !i.is_dir).count() as u64;
        progress.bytes_total.store(known, Ordering::Relaxed);
        progress
            .work_total
            .store(if known > 0 { known } else { files }, Ordering::Relaxed);
        let mut writer = Writer::new(stage, progress, known > 0);
        match source {
            Source::Fetched(fetched) => {
                for (item, fetched) in items.iter().zip(fetched) {
                    let contents = fetched.map(|f| match f {
                        Fetched::Bytes(bytes) => Ok(Contents::Bytes(bytes)),
                        Fetched::Stream(s) => s.resolve().map(Contents::Stream),
                        Fetched::Storage(s) => s.resolve().map(Contents::Storage),
                        Fetched::Failed(e) => Err(e),
                    });
                    if !writer.write(item, contents) {
                        break;
                    }
                }
            }
            Source::Later { data, operation } => {
                match data.resolve() {
                    Ok(data) => {
                        for item in &items {
                            let contents = (!item.is_dir).then(|| contents(&data, format, item));
                            if !writer.write(item, contents) {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(item) = items.first() {
                            writer.fail(item, e.into());
                        }
                    }
                }
                let ok = !writer.out.cancelled && writer.out.failed.is_none();
                let hr = if writer.out.cancelled {
                    E_ABORT
                } else if ok {
                    S_OK
                } else {
                    E_FAIL
                };
                operation.end(hr, ok);
            }
        }
        writer.out
    }
}

fn nothing_offered() -> ShellError {
    std::io::Error::other(t!("ops.virtual_none").into_owned()).into()
}

fn registered(name: PCWSTR) -> u16 {
    // SAFETY: registering a format name has no preconditions.
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

const fn query(format: u16, index: i32, tymed: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: tymed.0 as u32,
    }
}

/// The items `data` describes, wide names first; `None` without a descriptor.
fn read_descriptor(data: &IDataObject) -> Option<Vec<Item>> {
    for (name, wide) in [
        (w!("FileGroupDescriptorW"), true),
        (w!("FileGroupDescriptor"), false),
    ] {
        let format = registered(name);
        let ask = query(format, -1, TYMED_HGLOBAL);
        // SAFETY: `ask` lives for the call; the medium is released below.
        let Ok(mut medium) = (unsafe { data.GetData(&raw const ask) }) else {
            continue;
        };
        let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            // SAFETY: the medium holds an HGLOBAL, as its tymed says.
            unsafe { global_bytes(medium.u.hGlobal, None) }
        } else {
            None
        };
        // SAFETY: releases the medium GetData returned, once.
        unsafe { ReleaseStgMedium(&raw mut medium) };
        if let Some(bytes) = bytes {
            return Some(parse_descriptor(&bytes, wide));
        }
    }
    None
}

/// Copies an HGLOBAL's bytes (at most `limit`).
///
/// # Safety
/// `global` must be a valid HGLOBAL.
unsafe fn global_bytes(global: HGLOBAL, limit: Option<u64>) -> Option<Vec<u8>> {
    // SAFETY: the caller passes a valid HGLOBAL; it stays locked while read.
    unsafe {
        let size = GlobalSize(global);
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            return (size == 0).then(Vec::new);
        }
        let len = limit.map_or(size, |l| size.min(usize::try_from(l).unwrap_or(usize::MAX)));
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

/// The items of a `FILEGROUPDESCRIPTOR(W)`: a count, then that many descriptors.
fn parse_descriptor(bytes: &[u8], wide: bool) -> Vec<Item> {
    let Some(count) = bytes
        .get(..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    else {
        return Vec::new();
    };
    let size = if wide {
        size_of::<FILEDESCRIPTORW>()
    } else {
        size_of::<FILEDESCRIPTORA>()
    };
    let mut items = Vec::new();
    for index in 0..count as usize {
        let Some(raw) = bytes.get(4 + index * size..4 + (index + 1) * size) else {
            break;
        };
        let item = if wide {
            // SAFETY: `raw` holds exactly one (unaligned) FILEDESCRIPTORW.
            let d = unsafe { raw.as_ptr().cast::<FILEDESCRIPTORW>().read_unaligned() };
            let name = d.cFileName;
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let name = String::from_utf16_lossy(&name[..end]);
            describe(
                index,
                &name,
                d.dwFlags,
                d.dwFileAttributes,
                [d.ftCreationTime, d.ftLastAccessTime, d.ftLastWriteTime],
                d.nFileSizeHigh,
                d.nFileSizeLow,
            )
        } else {
            // SAFETY: `raw` holds exactly one (unaligned) FILEDESCRIPTORA.
            let d = unsafe { raw.as_ptr().cast::<FILEDESCRIPTORA>().read_unaligned() };
            let name: Vec<u8> = d
                .cFileName
                .iter()
                .map(|&c| c as u8)
                .take_while(|&c| c != 0)
                .collect();
            describe(
                index,
                &ansi(&name),
                d.dwFlags,
                d.dwFileAttributes,
                [d.ftCreationTime, d.ftLastAccessTime, d.ftLastWriteTime],
                d.nFileSizeHigh,
                d.nFileSizeLow,
            )
        };
        items.extend(item);
    }
    items
}

/// An item from the fields both descriptor layouts share; `times` are creation, last access and
/// last write.
fn describe(
    index: usize,
    name: &str,
    flags: u32,
    attributes: u32,
    times: [FILETIME; 3],
    size_high: u32,
    size_low: u32,
) -> Option<Item> {
    let [created, accessed, written] = times;
    let (created, accessed, written) = (&created, &accessed, &written);
    let has = |flag: windows::Win32::UI::Shell::FD_FLAGS| flags & flag.0 as u32 != 0;
    let parts = safe_parts(name);
    if parts.is_empty() {
        log::warn!("dropped item {index} has no usable name: {name:?}");
        return None;
    }
    let time = |set: bool, t: &FILETIME| set.then(|| system_time(t)).flatten();
    Some(Item {
        index: i32::try_from(index).ok()?,
        parts,
        is_dir: has(FD_ATTRIBUTES) && attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        size: has(FD_FILESIZE).then(|| (u64::from(size_high) << 32) | u64::from(size_low)),
        created: time(has(FD_CREATETIME), created),
        accessed: time(has(FD_ACCESSTIME), accessed),
        written: time(has(FD_WRITESTIME), written),
    })
}

/// ANSI (the system code page) to a string.
fn ansi(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    // SAFETY: both buffers are valid slices; the first call only measures.
    unsafe {
        let len = MultiByteToWideChar(CP_ACP, flags, bytes, None);
        let mut wide = vec![0u16; usize::try_from(len).unwrap_or(0)];
        let len = MultiByteToWideChar(CP_ACP, flags, bytes, Some(&mut wide));
        wide.truncate(usize::try_from(len).unwrap_or(0));
        String::from_utf16_lossy(&wide)
    }
}

fn system_time(t: &FILETIME) -> Option<SystemTime> {
    /// 100 ns intervals from 1601 to 1970.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    if ticks == 0 {
        return None;
    }
    let since = |ticks: u64| Duration::from_nanos(ticks.saturating_mul(100));
    if ticks >= UNIX_EPOCH_TICKS {
        SystemTime::UNIX_EPOCH.checked_add(since(ticks - UNIX_EPOCH_TICKS))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(since(UNIX_EPOCH_TICKS - ticks))
    }
}

/// A source's relative name as names that stay inside the target folder: split at `\` and `/`,
/// with `.` and `..` levels dropped, characters Windows doesn't allow in names replaced, trailing
/// dots and spaces trimmed, and device names (`con`, `nul.txt`) prefixed.
fn safe_parts(name: &str) -> Vec<String> {
    name.split(['\\', '/'])
        .filter(|part| !matches!(part.trim(), "" | "." | ".."))
        .map(|part| {
            let cleaned: String = part
                .chars()
                .map(|c| {
                    if c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let cleaned = cleaned.trim_end_matches(['.', ' ']).trim_start();
            if cleaned.is_empty() {
                return "_".to_owned();
            }
            let stem = cleaned
                .split('.')
                .next()
                .unwrap_or(cleaned)
                .to_ascii_uppercase();
            let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && stem.as_bytes()[3].is_ascii_digit());
            if device {
                format!("_{cleaned}")
            } else {
                cleaned.to_owned()
            }
        })
        .collect()
}

/// Asks `data` for an item's contents, in any medium it offers.
fn contents(data: &IDataObject, format: u16, item: &Item) -> windows::core::Result<Contents> {
    let any = TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0 | TYMED_ISTORAGE.0);
    let mut last = windows::core::Error::from(E_FAIL);
    // Some sources only answer a request for exactly the medium they have.
    for tymed in [any, TYMED_ISTREAM, TYMED_HGLOBAL, TYMED_ISTORAGE] {
        let ask = query(format, item.index, tymed);
        // SAFETY: `ask` lives for the call; the medium is released below.
        let mut medium = match unsafe { data.GetData(&raw const ask) } {
            Ok(medium) => medium,
            Err(e) => {
                last = e;
                continue;
            }
        };
        // SAFETY: each union field is read only for the tymed the medium says it holds; the
        // interfaces are AddRef'd (cloned) before the medium is released.
        let got = unsafe {
            let got = match TYMED(medium.tymed.cast_signed()) {
                TYMED_HGLOBAL => global_bytes(medium.u.hGlobal, item.size).map(Contents::Bytes),
                TYMED_ISTREAM => medium.u.pstm.as_ref().cloned().map(Contents::Stream),
                TYMED_ISTORAGE => medium.u.pstg.as_ref().cloned().map(Contents::Storage),
                _ => None,
            };
            ReleaseStgMedium(&raw mut medium);
            got
        };
        if let Some(got) = got {
            return Ok(got);
        }
    }
    Err(last)
}

/// Writes items into the stage folder and keeps the score.
struct Writer<'a> {
    stage: &'a Path,
    progress: &'a OpProgress,
    /// Progress counts bytes (the sizes are known), else files.
    by_bytes: bool,
    /// Folders made so far: the source's relative path and where it went.
    dirs: Vec<(Vec<String>, PathBuf)>,
    out: Extraction,
}

/// Why writing stopped short.
enum Stop {
    Cancelled,
    Failed(ShellError),
}

impl From<std::io::Error> for Stop {
    fn from(e: std::io::Error) -> Self {
        Self::Failed(e.into())
    }
}

impl From<windows::core::Error> for Stop {
    fn from(e: windows::core::Error) -> Self {
        Self::Failed(e.into())
    }
}

impl<'a> Writer<'a> {
    fn new(stage: &'a Path, progress: &'a OpProgress, by_bytes: bool) -> Self {
        Self {
            stage,
            progress,
            by_bytes,
            dirs: Vec::new(),
            out: Extraction::default(),
        }
    }

    /// Writes one item; `false` once cancelled. A failure is noted and the next item tried.
    fn write(&mut self, item: &Item, contents: Option<windows::core::Result<Contents>>) -> bool {
        if self.progress.cancel.load(Ordering::Relaxed) {
            self.out.cancelled = true;
            return false;
        }
        let result = match contents {
            None => self.dir(&item.parts).map(|_| ()).map_err(Stop::from),
            Some(contents) => contents
                .map_err(Stop::from)
                .and_then(|contents| self.file(item, contents)),
        };
        match result {
            Ok(()) => {
                self.progress.done.fetch_add(1, Ordering::Relaxed);
                if !self.by_bytes && !item.is_dir {
                    self.progress.work_done.fetch_add(1, Ordering::Relaxed);
                }
                true
            }
            Err(Stop::Cancelled) => {
                self.out.cancelled = true;
                false
            }
            Err(Stop::Failed(e)) => {
                self.fail(item, e);
                true
            }
        }
    }

    fn fail(&mut self, item: &Item, error: ShellError) {
        let name = item.parts.join("\\");
        log::warn!("can't take dropped {name}: {error}");
        if self.out.failed.is_none() {
            let message = t!("ops.virtual_failed", name = name, error = error).into_owned();
            self.out.failed = Some(std::io::Error::other(message).into());
        }
    }

    /// The folder for a relative path, made (with its parents) if needed.
    fn dir(&mut self, parts: &[String]) -> std::io::Result<PathBuf> {
        let Some((last, parent)) = parts.split_last() else {
            return Ok(self.stage.to_path_buf());
        };
        if let Some((_, made)) = self.dirs.iter().find(|(p, _)| p.as_slice() == parts) {
            return Ok(made.clone());
        }
        let parent = self.dir(parent)?;
        // A file of the same name came first: the folder gets a numbered name.
        let path = free_name(&parent.join(last));
        std::fs::create_dir(&path)?;
        self.arrived(&parent, &path);
        self.dirs.push((parts.to_vec(), path.clone()));
        Ok(path)
    }

    fn arrived(&mut self, parent: &Path, path: &Path) {
        if parent == self.stage {
            self.out.top.push(path.to_path_buf());
        }
    }

    fn file(&mut self, item: &Item, contents: Contents) -> Result<(), Stop> {
        let (name, parent) = item.parts.split_last().ok_or_else(|| {
            Stop::Failed(std::io::Error::from(std::io::ErrorKind::InvalidInput).into())
        })?;
        let parent = self.dir(parent)?;
        // Two items of the same name (two attachments both called image001.png): numbered.
        let path = free_name(&parent.join(name));
        let written = match contents {
            Contents::Storage(storage) => write_storage(&path, &storage).map_err(Stop::from),
            contents => self.write_file(&path, contents),
        };
        if let Err(stop) = written {
            let _ = std::fs::remove_file(&path);
            return Err(stop);
        }
        set_times(&path, item);
        self.arrived(&parent, &path);
        Ok(())
    }

    fn write_file(&self, path: &Path, contents: Contents) -> Result<(), Stop> {
        let mut file = std::fs::File::create_new(path)?;
        match contents {
            Contents::Bytes(bytes) => {
                for chunk in bytes.chunks(CHUNK) {
                    self.check_cancel()?;
                    file.write_all(chunk)?;
                    self.count(chunk.len());
                }
            }
            Contents::Stream(stream) => {
                // Some sources leave the stream's position at its end.
                // SAFETY: seeking a live stream.
                let _ = unsafe { stream.Seek(0, STREAM_SEEK_SET, None) };
                let mut buffer = vec![0u8; CHUNK];
                loop {
                    self.check_cancel()?;
                    let mut read = 0u32;
                    // SAFETY: `buffer` has room for CHUNK bytes; `read` receives the count.
                    let hr: HRESULT = unsafe {
                        stream.Read(
                            buffer.as_mut_ptr().cast(),
                            CHUNK as u32,
                            Some(&raw mut read),
                        )
                    };
                    hr.ok()?;
                    let read = (read as usize).min(CHUNK);
                    if read > 0 {
                        file.write_all(&buffer[..read])?;
                        self.count(read);
                    }
                    if read == 0 || hr == S_FALSE {
                        break;
                    }
                }
            }
            Contents::Storage(_) => {}
        }
        file.flush()?;
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), Stop> {
        if self.progress.cancel.load(Ordering::Relaxed) {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    fn count(&self, bytes: usize) {
        let bytes = bytes as u64;
        self.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
        if self.by_bytes {
            self.progress.work_done.fetch_add(bytes, Ordering::Relaxed);
        }
    }
}

/// `path`, or `name (2).ext`, `name (3).ext`… if it is taken.
fn free_name(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy());
    let parent = path.parent().unwrap_or(path);
    (2..=u32::MAX)
        .map(|n| match &ext {
            Some(ext) => parent.join(format!("{stem} ({n}).{ext}")),
            None => parent.join(format!("{stem} ({n})")),
        })
        .find(|p| !p.exists())
        .unwrap_or_else(|| path.to_path_buf())
}

/// An Outlook message (`.msg`) and the like: a compound file copied from the source's storage.
fn write_storage(path: &Path, source: &IStorage) -> windows::core::Result<()> {
    let wide: Vec<u16> = path.as_os_str().encode_wide_null();
    let mut created: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `wide` is a null-terminated path; `created` receives an IStorage, owned below.
    let target = unsafe {
        StgCreateStorageEx(
            PCWSTR(wide.as_ptr()),
            STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
            STGFMT_STORAGE,
            0,
            None,
            None,
            &IStorage::IID,
            &raw mut created,
        )?;
        IStorage::from_raw(created)
    };
    // SAFETY: both storages are live; the copy is committed before the target is released.
    unsafe {
        source.CopyTo(None, None, &target)?;
        target.Commit(STGC_DEFAULT.0 as u32)
    }
}

trait EncodeWideNull {
    fn encode_wide_null(&self) -> Vec<u16>;
}

impl EncodeWideNull for std::ffi::OsStr {
    fn encode_wide_null(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt as _;
        self.encode_wide().chain(std::iter::once(0)).collect()
    }
}

/// The times the descriptor gave, where it gave them.
fn set_times(path: &Path, item: &Item) {
    use std::os::windows::fs::FileTimesExt as _;
    if item.created.is_none() && item.accessed.is_none() && item.written.is_none() {
        return;
    }
    let mut times = std::fs::FileTimes::new();
    if let Some(t) = item.created {
        times = times.set_created(t);
    }
    if let Some(t) = item.accessed {
        times = times.set_accessed(t);
    }
    if let Some(t) = item.written {
        times = times.set_modified(t);
    }
    let set = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_times(times));
    if let Err(e) = set {
        log::debug!("can't set the times of {}: {e}", path.display());
    }
}

/// A source's promise that the drop may finish after it returns, and the call that ends it.
struct AsyncOperation(Option<AgileReference<IDataObjectAsyncCapability>>);

impl AsyncOperation {
    /// Starts it when `data` supports finishing later.
    fn start(data: &IDataObject) -> Option<Self> {
        let capability = data.cast::<IDataObjectAsyncCapability>().ok()?;
        // SAFETY: plain calls on a live interface.
        let started = unsafe {
            capability
                .GetAsyncMode()
                .is_ok_and(windows::core::BOOL::as_bool)
                && capability.StartOperation(None).is_ok()
        };
        if !started {
            return None;
        }
        match AgileReference::new(&capability) {
            Ok(agile) => Some(Self(Some(agile))),
            Err(e) => {
                log::warn!("can't hand a drop operation to a worker: {e}");
                // SAFETY: ends the operation started above.
                let _ = unsafe { capability.EndOperation(E_FAIL, None, DROPEFFECT_NONE.0) };
                None
            }
        }
    }

    /// Tells the source the drop is over: copied (`ok`) or not.
    fn end(mut self, hr: HRESULT, ok: bool) {
        self.finish(hr, ok);
    }

    fn finish(&mut self, hr: HRESULT, ok: bool) {
        let Some(agile) = self.0.take() else {
            return;
        };
        let effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        match agile.resolve() {
            Ok(capability) => {
                // SAFETY: ends the operation this started, once.
                let ended = unsafe { capability.EndOperation(hr, None, effect.0) };
                if let Err(e) = ended {
                    log::debug!("dropped data's source didn't take the end of the drop: {e}");
                }
            }
            Err(e) => log::warn!("can't end a drop operation: {e}"),
        }
    }
}

impl Drop for AsyncOperation {
    /// Never left open: a drop dropped unfinished ends as aborted.
    fn drop(&mut self) {
        self.finish(E_ABORT, false);
    }
}

/// COM for a worker thread, undone when dropped.
pub(crate) struct ComThread {
    initialized: bool,
    /// Set up with OLE (single-threaded), as the clipboard needs.
    ole: bool,
}

impl ComThread {
    /// Multithreaded.
    pub(crate) fn init() -> Self {
        // SAFETY: initializes COM on this thread; paired with CoUninitialize on drop.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        Self {
            initialized,
            ole: false,
        }
    }

    /// With OLE, for reading the clipboard's data object. Single-threaded, so everything made
    /// from that object has to be used on this thread until this is dropped.
    pub(crate) fn ole() -> Self {
        // SAFETY: initializes OLE on this thread; paired with OleUninitialize on drop.
        let initialized = unsafe { OleInitialize(None) }.is_ok();
        Self {
            initialized,
            ole: true,
        }
    }
}

impl Drop for ComThread {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: pairs with the successful initialization in `init` or `ole`.
            unsafe {
                if self.ole {
                    OleUninitialize();
                } else {
                    CoUninitialize();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "win_virtual_drop_tests.rs"]
pub(crate) mod tests;
