//! Fast copy (on by default, can be turned off in settings).
//!
//! The Windows engine (`IFileOperation`) copies one file after another, and each one goes
//! through the shell, its change notifications and real-time antivirus: 10,000 small files in
//! 100 folders take it about 25 s. Here several files are copied at once, each with `CopyFile2`,
//! which keeps attributes, alternate data streams, timestamps and encryption (and clones blocks
//! instead of copying them on a Dev Drive or ReFS where Windows supports that). Threads that add
//! names to the same folder wait on each other, so the work is spread across folders: big files
//! first, each a job of its own, then the small ones taken from each folder in turn.
//!
//! Only what is plainly safe is copied here; every other top-level item goes to the Windows
//! engine afterwards, exactly as before (`perform_tracked` in `win_fileops.rs` splits the copy and
//! merges failures and undo). An item is copied here when:
//!
//! - Source and destination are on local drive letters (fixed or removable; not network, subst,
//!   shell namespace or server paths), on solid-state drives. Parallel writes to a disk with a seek
//!   penalty would be slower than the engine, so that is read from the drive
//!   (`IOCTL_STORAGE_QUERY_PROPERTY`, cached per volume) and an answer that can't be read counts as
//!   a hard disk.
//! - The destination keeps alternate data streams (NTFS, ReFS), and encryption when the item holds
//!   encrypted files or folders. Where those would be lost Windows asks first, so that stays with it.
//! - Nothing in the destination has the item's name, and no earlier item of the same copy takes it.
//!   The whole copy is then new, and no conflict can come up anywhere inside it; Windows' conflict
//!   dialog stays as it is for everything else.
//! - The destination is neither the item nor inside it (compared with links resolved).
//! - The tree, listed first (which also gives the totals for progress), holds no links: no
//!   symlinks, junctions or any reparse point other than a cloud placeholder (OneDrive). A tree
//!   that can't be listed in full goes to the engine too.
//! - Everything taken fits in the free space of the destination drive, counting a cluster for
//!   every file and folder; otherwise the engine asks, as it does today.
//!
//! How it copies, with `min(8, cores)` threads:
//!
//! 1. Folders are made level by level with `CreateDirectoryExW`, with the source folder as the
//!    template so its attributes and streams come along (`desktop.ini` customisations keep the
//!    folder's read-only or system attribute). A cloud folder is not used as a template, so that
//!    nothing of its placeholder can come along: it gets a plain folder with its attributes.
//! 2. Files are copied with `CopyFile2`, which fails rather than replaces
//!    (`COPY_FILE_FAIL_IF_EXISTS`) and goes past the cache for very large files
//!    (`COPY_FILE_NO_BUFFERING`).
//! 3. Each folder gets its source's modified time back: adding its contents changed it.
//!
//! A copy with an end on the network (a UNC path or a mapped drive) asks for SMB compression
//! (`COPY_FILE_REQUEST_COMPRESSED_TRAFFIC`) on Windows 11 22H2 and later only; older builds never
//! get the flag. The checks above keep network paths out for now, so this only matters if they are
//! ever let in; for now this stays dormant, by decision (no network copies in fast copy).
//!
//! Progress counts files and folders done (`done`, as the engine counts items) and work done out
//! of a total: the bytes plus a fixed share per item, so a copy of many small files still moves
//! steadily. Big files report their bytes as they go. Cancel stops before the next file and in the
//! middle of a big one, whose half-written copy `CopyFile2` removes; what is copied stays, as with
//! Windows. A file that fails is reported with the others at the end while the rest go on; nothing
//! is retried through the engine. Undo removes the top-level copies, like the engine's undo.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::Storage::FileSystem::{
    COPY_FILE_FAIL_IF_EXISTS, COPY_FILE_NO_BUFFERING, COPY_FILE_REQUEST_COMPRESSED_TRAFFIC,
    COPYFILE_FLAGS, COPYFILE2_CALLBACK_CHUNK_FINISHED, COPYFILE2_EXTENDED_PARAMETERS,
    COPYFILE2_MESSAGE, COPYFILE2_MESSAGE_ACTION, COPYFILE2_PROGRESS_CANCEL,
    COPYFILE2_PROGRESS_CONTINUE, CopyFile2, CreateDirectoryExW, CreateDirectoryW, CreateFileW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetDiskFreeSpaceExW, GetDriveTypeW,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
    SetFileAttributesW, SetFileTime, WIN32_FIND_DATAW,
};
use windows::core::HSTRING;

use crate::shell::{ItemFailure, OpProgress, Undo};

/// The setting, mirrored for the file operation threads.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Most threads copying at once. Past this they mostly wait on the drive and on antivirus.
const MAX_THREADS: usize = 8;
/// Files from this size on are jobs of their own, started first, and report progress as they go.
const BIG_FILE: u64 = 1 << 20;
/// Files from this size on are copied past the cache (`COPY_FILE_NO_BUFFERING`), so one huge file
/// doesn't push everything else out of memory.
const UNBUFFERED_FILE: u64 = 256 << 20;
/// Work a file or folder counts for besides its bytes, about what creating one costs, so progress
/// through many small files is steady.
const ITEM_WORK: u64 = 64 << 10;
/// Space a file or folder takes at least (a cluster), for the free space check.
const ITEM_SPACE: u64 = 4 << 10;
/// Most new items reported to Explorer one by one; past this the destination is reported once.
const NOTIFY_EACH_MAX: usize = 64;

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;
const ATTR_ENCRYPTED: u32 = 0x4000;
/// What a plain folder made for a cloud folder keeps: read-only, hidden, system, archive and not
/// content indexed.
const ATTR_KEPT: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x2000;
/// `IO_REPARSE_TAG_CLOUD`; `IO_REPARSE_TAG_CLOUD_1` to `_F` differ in the bits of the mask.
const TAG_CLOUD: u32 = 0x9000_001A;
const TAG_CLOUD_MASK: u32 = 0x0000_F000;
/// `GetVolumeInformationW` file system flags (WinNT.h).
const FILE_SUPPORTS_ENCRYPTION: u32 = 0x0002_0000;
const FILE_NAMED_STREAMS: u32 = 0x0004_0000;
/// `GetDriveTypeW` results (WinBase.h).
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
/// First Windows build whose `CopyFile2` takes [`COPY_FILE_REQUEST_COMPRESSED_TRAFFIC`]
/// (Windows 11 22H2).
const COMPRESSION_MIN_BUILD: u32 = 22621;

/// What [`copy`] did.
#[derive(Debug, Default)]
pub(crate) struct Copied {
    /// Top-level copies made, with the index of their source, in the order of the sources.
    pub(crate) made: Vec<(usize, PathBuf)>,
    /// Sources left to the Windows engine, with their index, in order.
    pub(crate) rest: Vec<(usize, PathBuf)>,
    /// Files and folders that could not be copied.
    pub(crate) failed: Vec<ItemFailure>,
    /// Cancel was pressed: nothing is left to the engine.
    pub(crate) cancelled: bool,
}

/// Copies what it can of `sources` into the folder `dest` (see the module docs); the rest is
/// returned for the engine. Background thread.
pub(crate) fn copy(sources: &[PathBuf], dest: &Path, progress: &OpProgress) -> Copied {
    copy_with(sources, dest, progress, true)
}

/// [`copy`]; `ssd_only: false` takes drives with a seek penalty too (tests, which may run on any
/// disk).
fn copy_with(sources: &[PathBuf], dest: &Path, progress: &OpProgress, ssd_only: bool) -> Copied {
    let mut out = Copied::default();
    let everything = || sources.iter().cloned().enumerate().collect::<Vec<_>>();
    if progress.cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        return out;
    }
    let mut volumes = Volumes {
        ssd_only,
        known: HashMap::new(),
    };
    let Some(target) = Target::find(dest, &mut volumes) else {
        out.rest = everything();
        return out;
    };
    let mut admit = Admit {
        volumes,
        parents: HashMap::new(),
        names: HashSet::new(),
    };
    let mut plan = Plan {
        compress: wants_compression(dest, sources),
        ..Plan::default()
    };
    for (index, source) in sources.iter().enumerate() {
        if progress.cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            return out;
        }
        let walked = match admit.item(source, &target) {
            Some((real, entry)) => plan.add(index, real, dest, &target, entry, &progress.cancel),
            None => Walk::Engine,
        };
        match walked {
            Walk::Taken => {}
            Walk::Engine => out.rest.push((index, source.clone())),
            Walk::Cancelled => {
                out.cancelled = true;
                return out;
            }
        }
    }
    if plan.items.is_empty() {
        return out;
    }
    if plan.space() > target.free {
        log::info!("fast copy: not enough free space; left to Windows, which asks");
        out.rest = everything();
        return out;
    }
    // The same total the engine path gets from its own count (`file_op_tracked` in win.rs), only
    // sooner; with items left to the engine that count covers them too, so it stays.
    if out.rest.is_empty() {
        progress.bytes_total.store(plan.bytes, Ordering::Relaxed);
    }
    let started = std::time::Instant::now();
    let mut failed = plan.run(progress, threads());
    log::info!(
        "fast copy: {} items, {} files, {} folders in {:?}; {} failed, {} left to Windows",
        plan.items.len(),
        plan.files.len(),
        plan.folders.len(),
        started.elapsed(),
        failed.len(),
        out.rest.len()
    );
    crate::win_fileops::explain(&mut failed);
    let made: Vec<&Item> = plan
        .items
        .iter()
        .filter(|item| item.made.load(Ordering::Relaxed))
        .collect();
    notify(&made, dest);
    out.made = made
        .iter()
        .map(|item| (item.index, item.shown.clone()))
        .collect();
    out.failed = failed;
    out.cancelled = progress.cancel.load(Ordering::Relaxed);
    out
}

/// Undo for a copy split between this engine and Windows': the top-level copies in the order of
/// their sources (verify after copy pairs copies with sources by that order), then whatever else
/// Windows reported creating. A copy only creates, so Windows' undo of it is a list to remove.
pub(crate) fn merged_undo(fast: &Copied, dest: &Path, engine: Option<Undo>) -> Option<Undo> {
    let folder = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let dest = folder(dest);
    let engine = match engine {
        Some(Undo::Remove(paths)) => paths,
        Some(other @ Undo::PutBack(_)) if fast.made.is_empty() => return Some(other),
        _ => Vec::new(),
    };
    let (top, inside): (Vec<PathBuf>, Vec<PathBuf>) = engine
        .into_iter()
        .partition(|p| p.parent().is_some_and(|parent| folder(parent) == dest));
    let mut order: Vec<(usize, Option<&PathBuf>)> = fast
        .made
        .iter()
        .map(|(index, copy)| (*index, Some(copy)))
        .chain(fast.rest.iter().map(|(index, _)| (*index, None)))
        .collect();
    order.sort_by_key(|(index, _)| *index);
    let mut top = top.into_iter();
    let mut out = Vec::new();
    for (_, copy) in order {
        match copy {
            Some(copy) => out.push(copy.clone()),
            None => out.extend(top.next()),
        }
    }
    out.extend(top);
    out.extend(inside);
    (!out.is_empty()).then_some(Undo::Remove(out))
}

// ───────────────────────── Checks ─────────────────────────

/// A volume files may be copied to or from in parallel.
#[derive(Clone)]
struct Volume {
    /// Where it is mounted, `X:\` (or a folder it is mounted in).
    root: PathBuf,
    /// File system flags (`FILE_NAMED_STREAMS`, ...).
    flags: u32,
}

/// Volumes looked up during one copy, by where they are mounted.
struct Volumes {
    ssd_only: bool,
    known: HashMap<PathBuf, Option<Volume>>,
}

impl Volumes {
    /// The volume holding `real` (a path with links resolved), if it is local and fast to write to
    /// in parallel.
    fn usable(&mut self, real: &Path) -> Option<Volume> {
        let root = volume_root(real)?;
        let ssd_only = self.ssd_only;
        self.known
            .entry(root)
            .or_insert_with_key(|root| read_volume(root, ssd_only))
            .clone()
    }
}

/// The destination folder, when copies may go there.
struct Target {
    /// `\\?\` and with links resolved: what sources are checked against.
    real: PathBuf,
    /// The file system keeps encrypted (EFS) files encrypted.
    encrypts: bool,
    /// Free bytes for this user.
    free: u64,
}

impl Target {
    fn find(dest: &Path, volumes: &mut Volumes) -> Option<Self> {
        if !local_letter(dest) {
            return None;
        }
        let real = real_path(dest)?;
        if !std::fs::metadata(&real).is_ok_and(|m| m.is_dir()) {
            return None;
        }
        let volume = volumes.usable(&real)?;
        if volume.flags & FILE_NAMED_STREAMS == 0 {
            return None;
        }
        let root = HSTRING::from(volume.root.as_os_str());
        let mut free = 0u64;
        // SAFETY: root is NUL-terminated; the out pointer is valid for the call.
        unsafe { GetDiskFreeSpaceExW(&root, Some(&raw mut free), None, None) }.ok()?;
        Some(Self {
            real,
            encrypts: volume.flags & FILE_SUPPORTS_ENCRYPTION != 0,
            free,
        })
    }
}

/// Decides, item by item, which sources are copied here.
struct Admit {
    volumes: Volumes,
    /// Source folders seen, with their real path when items in them may be copied here. Most
    /// copies take many items from one folder.
    parents: HashMap<PathBuf, Option<PathBuf>>,
    /// Names already taken in the destination by this copy, lowercased.
    names: HashSet<String>,
}

impl Admit {
    /// `source` with links in its path resolved, and its entry, when it may be copied here.
    fn item(&mut self, source: &Path, target: &Target) -> Option<(PathBuf, Entry)> {
        let name = source.file_name()?;
        // A second item of the same name would land on the first one's copy: Windows asks.
        if !self.names.insert(name.to_string_lossy().to_lowercase()) {
            return None;
        }
        if !absent(&target.real.join(name)) {
            return None;
        }
        let real = self.real_folder(source.parent()?)?.join(name);
        if arke_core::path::is_same_or_inside(&target.real, &real) {
            return None;
        }
        let entry = find_one(&real)?;
        // Given by its short (8.3) name: the copy takes the real one, which must be free too.
        if !entry.name.eq_ignore_ascii_case(name)
            && (!self
                .names
                .insert(entry.name.to_string_lossy().to_lowercase())
                || !absent(&target.real.join(&entry.name)))
        {
            return None;
        }
        let encryption_kept = target.encrypts || !entry.encrypted();
        (!entry.is_link() && encryption_kept).then_some((real, entry))
    }

    fn real_folder(&mut self, folder: &Path) -> Option<PathBuf> {
        if let Some(known) = self.parents.get(folder) {
            return known.clone();
        }
        let real = local_letter(folder)
            .then(|| real_path(folder))
            .flatten()
            .filter(|real| self.volumes.usable(real).is_some());
        self.parents.insert(folder.to_path_buf(), real.clone());
        real
    }
}

/// Nothing is at `path`, not even a dangling link.
pub(crate) fn absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// `path` with every link resolved, `\\?\X:\...`; `None` when it isn't on a drive letter then.
pub(crate) fn real_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(arke_core::path::verbatim(path))
        .ok()
        .filter(|real| {
            matches!(real.components().next(), Some(Component::Prefix(p))
                if matches!(p.kind(), Prefix::VerbatimDisk(_)))
        })
}

/// A path on a fixed or removable drive letter that isn't a subst drive. Local mount information
/// only, never the network.
pub(crate) fn local_letter(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return false;
    };
    let letter = char::from(letter);
    let root = HSTRING::from(format!(r"{letter}:\"));
    // SAFETY: root is NUL-terminated.
    let kind = unsafe { GetDriveTypeW(&root) };
    // A subst drive reports the type of the drive under it; its device is `\??\` plus its folder.
    matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE)
        && !crate::win::dos_device(letter).is_some_and(|device| device.starts_with(r"\??\"))
}

/// Where the volume holding `path` is mounted, `X:\` or a folder, with a trailing backslash.
pub(crate) fn volume_root(path: &Path) -> Option<PathBuf> {
    fn ask(path: &Path, buf: &mut [u16]) -> bool {
        let wide = HSTRING::from(path.as_os_str());
        // SAFETY: wide is NUL-terminated; the buffer and its length are passed together.
        unsafe { GetVolumePathNameW(&wide, buf) }.is_ok()
    }
    let mut buf = [0u16; 1024];
    if !ask(path, &mut buf) && !ask(&arke_core::path::unverbatim(path), &mut buf) {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0)?;
    let root = arke_core::path::unverbatim(Path::new(&OsString::from_wide(&buf[..len])));
    let mut root = root.into_os_string();
    if !root.to_string_lossy().ends_with('\\') {
        root.push(r"\");
    }
    Some(PathBuf::from(root))
}

/// The volume mounted at `root` when it is a fixed or removable drive (and solid state, with
/// `ssd_only`). Local volume information only.
fn read_volume(root: &Path, ssd_only: bool) -> Option<Volume> {
    let wide = HSTRING::from(root.as_os_str());
    // SAFETY: wide is NUL-terminated.
    let kind = unsafe { GetDriveTypeW(&wide) };
    if !matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE) {
        return None;
    }
    let mut flags = 0u32;
    // SAFETY: wide is NUL-terminated; the flags pointer is valid for the call.
    unsafe { GetVolumeInformationW(&wide, None, None, None, Some(&raw mut flags), None) }.ok()?;
    if ssd_only {
        let mut name = [0u16; 64];
        // SAFETY: wide is NUL-terminated; the buffer and its length are passed together.
        unsafe { GetVolumeNameForVolumeMountPointW(&wide, &mut name) }.ok()?;
        let len = name.iter().position(|&c| c == 0)?;
        if !solid_state(&String::from_utf16_lossy(&name[..len])) {
            return None;
        }
    }
    Some(Volume {
        root: root.to_path_buf(),
        flags,
    })
}

/// Per volume (`\\?\Volume{GUID}\`), whether it is known to have no seek penalty. Asked once per
/// volume while Arke runs: it is a property of the hardware.
static SOLID_STATE: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

/// The volume is on a drive without a seek penalty (an SSD). `false` when that can't be read.
fn solid_state(volume: &str) -> bool {
    let known = SOLID_STATE
        .lock()
        .iter()
        .find(|(v, _)| v == volume)
        .map(|(_, ssd)| *ssd);
    if let Some(ssd) = known {
        return ssd;
    }
    let ssd = seek_penalty(volume) == Some(false);
    log::info!("fast copy: {volume} solid state: {ssd}");
    SOLID_STATE.lock().push((volume.to_owned(), ssd));
    ssd
}

/// The local drive holding `path` is solid state (see [`solid_state`]); `false` for a network
/// path or when that can't be read. Background thread.
pub(crate) fn solid_state_at(path: &Path) -> bool {
    let Some(root) = volume_root(path) else {
        return false;
    };
    let wide = HSTRING::from(root.as_os_str());
    let mut name = [0u16; 64];
    // SAFETY: wide is NUL-terminated; the buffer and its length are passed together.
    if unsafe { GetVolumeNameForVolumeMountPointW(&wide, &mut name) }.is_err() {
        return false;
    }
    name.iter()
        .position(|&c| c == 0)
        .is_some_and(|len| solid_state(&String::from_utf16_lossy(&name[..len])))
}

/// Whether the drive under `volume` (`\\?\Volume{GUID}\`) has a seek penalty, as its driver
/// reports it. `None` when it can't be read (a volume over several disks, an old driver).
fn seek_penalty(volume: &str) -> Option<bool> {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
        StorageDeviceSeekPenaltyProperty,
    };
    // The volume device itself, without the trailing backslash; no access rights are needed to
    // ask about it.
    let device = HSTRING::from(volume.trim_end_matches('\\'));
    // SAFETY: device is NUL-terminated; the handle is closed below.
    let handle = unsafe {
        CreateFileW(
            &device,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version, Size, then IncursSeekPenalty (one byte). Read as
    // plain integers, so no byte the driver writes can make an invalid `bool`.
    let mut answer = [0u32; 3];
    let mut written = 0u32;
    // SAFETY: the handle is open; the query and answer buffers outlive the call and their sizes
    // are passed with them; no OVERLAPPED, so the call completes before it returns.
    let asked = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(answer.as_mut_ptr().cast()),
            size_of_val(&answer) as u32,
            Some(&raw mut written),
            None,
        )
    };
    // SAFETY: opened above, closed once.
    let _ = unsafe { CloseHandle(handle) };
    asked.ok()?;
    (written >= 9).then_some(answer[2] & 0xFF != 0)
}

// ───────────────────────── Listing ─────────────────────────

/// A file or folder as a folder listing gives it.
struct Entry {
    name: OsString,
    attributes: u32,
    /// The reparse tag of a reparse point, else 0.
    tag: u32,
    size: u64,
    /// Last write time, FILETIME.
    written: u64,
}

impl Entry {
    fn new(data: &WIN32_FIND_DATAW) -> Self {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let attributes = data.dwFileAttributes;
        Self {
            name: OsString::from_wide(&data.cFileName[..len]),
            attributes,
            tag: if attributes & ATTR_REPARSE_POINT == 0 {
                0
            } else {
                data.dwReserved0
            },
            size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
            written: (u64::from(data.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(data.ftLastWriteTime.dwLowDateTime),
        }
    }

    const fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }

    const fn encrypted(&self) -> bool {
        self.attributes & ATTR_ENCRYPTED != 0
    }

    /// A reparse point that isn't a cloud placeholder: a symlink, a junction or anything else the
    /// engine knows how to treat.
    const fn is_link(&self) -> bool {
        self.attributes & ATTR_REPARSE_POINT != 0 && self.tag & !TAG_CLOUD_MASK != TAG_CLOUD
    }
}

/// Gives each entry `FindFirstFileExW` finds for `pattern` to `each`.
fn find(pattern: &Path, mut each: impl FnMut(Entry)) -> windows::core::Result<()> {
    use windows::Win32::Foundation::ERROR_NO_MORE_FILES;
    let wide = HSTRING::from(pattern.as_os_str());
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: wide is NUL-terminated and the find data buffer outlives the call; the handle is
    // closed below.
    let handle = unsafe {
        FindFirstFileExW(
            &wide,
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }?;
    let result = loop {
        each(Entry::new(&data));
        // SAFETY: the search handle opened above; the buffer outlives the call.
        match unsafe { FindNextFileW(handle, &raw mut data) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    // SAFETY: opened above, closed once.
    let _ = unsafe { FindClose(handle) };
    result
}

/// What is in the folder `dir` (a `\\?\` path).
fn list(dir: &Path) -> windows::core::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    find(&dir.join("*"), |entry| {
        if entry.name != "." && entry.name != ".." {
            entries.push(entry);
        }
    })?;
    Ok(entries)
}

/// The name as stored and the attributes of the item at `path` (a `\\?\` path); for fast move.
pub(crate) fn entry_of(path: &Path) -> Option<(OsString, u32)> {
    find_one(path).map(|entry| (entry.name, entry.attributes))
}

/// The entry of the item at `path` itself (a `\\?\` path).
fn find_one(path: &Path) -> Option<Entry> {
    let mut found = None;
    find(path, |entry| {
        if found.is_none() {
            found = Some(entry);
        }
    })
    .ok()?;
    found
}

// ───────────────────────── Plan ─────────────────────────

/// A top-level item copied here.
struct Item {
    /// Its index among the sources.
    index: usize,
    /// Source and copy, `\\?\` with links resolved.
    src: PathBuf,
    dst: PathBuf,
    /// The copy as the caller names it (the destination as given plus the name): for undo and
    /// Explorer.
    shown: PathBuf,
    is_dir: bool,
    /// The copy exists: the file is copied, or the folder made (whatever happens inside it).
    made: AtomicBool,
}

/// A folder to make.
struct Folder {
    src: PathBuf,
    dst: PathBuf,
    /// The folder it is made in; `None` for a top-level folder, made in the destination.
    parent: Option<usize>,
    depth: usize,
    attributes: u32,
    /// The source's last write time, FILETIME, given to the copy at the end.
    written: u64,
    /// The top-level item it belongs to.
    item: usize,
}

/// A file to copy.
struct File {
    /// The folder it is copied into; `None` for a top-level file (its paths are its item's).
    folder: Option<usize>,
    item: usize,
    name: OsString,
    size: u64,
}

/// Everything one copy takes, listed before anything is copied.
#[derive(Default)]
struct Plan {
    items: Vec<Item>,
    folders: Vec<Folder>,
    files: Vec<File>,
    /// Bytes of all the files.
    bytes: u64,
    /// Ask for SMB compression on each file (see [`wants_compression`]).
    compress: bool,
}

/// How listing an item ended.
enum Walk {
    Taken,
    /// The item goes to the engine; nothing of it stays in the plan.
    Engine,
    Cancelled,
}

impl Plan {
    /// Adds the item at `real` (links resolved) with everything in it, unless something in it
    /// leaves it to the engine.
    fn add(
        &mut self,
        index: usize,
        real: PathBuf,
        dest: &Path,
        target: &Target,
        entry: Entry,
        cancel: &AtomicBool,
    ) -> Walk {
        let (items, folders, files, bytes) = (
            self.items.len(),
            self.folders.len(),
            self.files.len(),
            self.bytes,
        );
        let copy = target.real.join(&entry.name);
        self.items.push(Item {
            index,
            src: real.clone(),
            dst: copy.clone(),
            shown: dest.join(&entry.name),
            is_dir: entry.is_dir(),
            made: AtomicBool::new(false),
        });
        let walked = if entry.is_dir() {
            self.folders.push(Folder {
                src: real,
                dst: copy,
                parent: None,
                depth: 0,
                attributes: entry.attributes,
                written: entry.written,
                item: items,
            });
            self.walk(folders, target.encrypts, cancel)
        } else {
            self.files.push(File {
                folder: None,
                item: items,
                name: OsString::new(),
                size: entry.size,
            });
            self.bytes = self.bytes.saturating_add(entry.size);
            Walk::Taken
        };
        if !matches!(walked, Walk::Taken) {
            self.items.truncate(items);
            self.folders.truncate(folders);
            self.files.truncate(files);
            self.bytes = bytes;
        }
        walked
    }

    /// Lists the tree under the folder `root`, adding its folders and files.
    fn walk(&mut self, root: usize, encrypts: bool, cancel: &AtomicBool) -> Walk {
        let mut pending = vec![root];
        while let Some(at) = pending.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Walk::Cancelled;
            }
            let (src, dst, depth, item) = {
                let folder = &self.folders[at];
                (
                    folder.src.clone(),
                    folder.dst.clone(),
                    folder.depth,
                    folder.item,
                )
            };
            let Ok(entries) = list(&src) else {
                return Walk::Engine;
            };
            for entry in entries {
                if entry.is_link() || (entry.encrypted() && !encrypts) {
                    return Walk::Engine;
                }
                if entry.is_dir() {
                    pending.push(self.folders.len());
                    self.folders.push(Folder {
                        src: src.join(&entry.name),
                        dst: dst.join(&entry.name),
                        parent: Some(at),
                        depth: depth + 1,
                        attributes: entry.attributes,
                        written: entry.written,
                        item,
                    });
                } else {
                    self.bytes = self.bytes.saturating_add(entry.size);
                    self.files.push(File {
                        folder: Some(at),
                        item,
                        name: entry.name,
                        size: entry.size,
                    });
                }
            }
        }
        Walk::Taken
    }

    /// Bytes the copies take at least on the destination.
    const fn space(&self) -> u64 {
        let items = (self.files.len() + self.folders.len()) as u64;
        self.bytes.saturating_add(items.saturating_mul(ITEM_SPACE))
    }

    /// Work in all: the bytes plus [`ITEM_WORK`] per file and folder.
    const fn work(&self) -> u64 {
        let items = (self.files.len() + self.folders.len()) as u64;
        self.bytes.saturating_add(items.saturating_mul(ITEM_WORK))
    }

    /// Source and copy of a file.
    fn paths(&self, file: &File) -> (PathBuf, PathBuf) {
        if let Some(folder) = file.folder {
            let folder = &self.folders[folder];
            (folder.src.join(&file.name), folder.dst.join(&file.name))
        } else {
            let item = &self.items[file.item];
            (item.src.clone(), item.dst.clone())
        }
    }

    /// Folders by depth: each level is made after the one above it.
    fn levels(&self) -> Vec<Vec<usize>> {
        let mut levels: Vec<Vec<usize>> = Vec::new();
        for (at, folder) in self.folders.iter().enumerate() {
            if levels.len() <= folder.depth {
                levels.resize_with(folder.depth + 1, Vec::new);
            }
            levels[folder.depth].push(at);
        }
        levels
    }

    /// The order files are taken in: big files first, largest first, each a job of its own; then
    /// the small ones one from each folder in turn, so threads seldom add names to the same
    /// folder at once.
    fn job_order(&self) -> Vec<usize> {
        let mut big: Vec<usize> = (0..self.files.len())
            .filter(|&at| self.files[at].size >= BIG_FILE)
            .collect();
        big.sort_by_key(|&at| std::cmp::Reverse(self.files[at].size));
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut group_of: HashMap<Option<usize>, usize> = HashMap::new();
        for (at, file) in self.files.iter().enumerate() {
            if file.size >= BIG_FILE {
                continue;
            }
            let group = *group_of.entry(file.folder).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[group].push(at);
        }
        let mut order = big;
        order.reserve(self.files.len() - order.len());
        let mut live = Vec::with_capacity(groups.len());
        for group in &groups {
            live.push(group.iter());
        }
        while !live.is_empty() {
            live.retain_mut(|files| match files.next() {
                Some(&at) => {
                    order.push(at);
                    true
                }
                None => false,
            });
        }
        order
    }

    /// Makes the folders, copies the files, then dates the folders; returns what failed.
    fn run(&self, progress: &OpProgress, threads: usize) -> Vec<ItemFailure> {
        let state = State {
            progress,
            ready: (0..self.folders.len())
                .map(|_| AtomicBool::new(false))
                .collect(),
            failed: Mutex::new(Vec::new()),
            full: AtomicBool::new(false),
        };
        progress.work_done.store(0, Ordering::Relaxed);
        progress.work_in_bytes.store(true, Ordering::Relaxed);
        progress.work_total.store(self.work(), Ordering::Relaxed);
        for level in self.levels() {
            parallel(&level, threads, |&at| self.make_folder(at, &state));
        }
        parallel(&self.job_order(), threads, |&at| self.copy_file(at, &state));
        if !state.cancelled() {
            let made: Vec<usize> = (0..self.folders.len())
                .filter(|&at| state.ready[at].load(Ordering::Relaxed))
                .collect();
            parallel(&made, threads, |&at| {
                let folder = &self.folders[at];
                if let Err(err) = stamp(&folder.dst, folder.written) {
                    log::debug!(
                        "fast copy: {} keeps the time it was made: {err}",
                        folder.dst.display()
                    );
                }
            });
        }
        state.failed.into_inner()
    }

    fn make_folder(&self, at: usize, state: &State<'_>) {
        if state.stopped() {
            return;
        }
        let folder = &self.folders[at];
        // Inside a folder that couldn't be made: that failure is reported, not each item in it.
        if folder
            .parent
            .is_some_and(|parent| !state.ready[parent].load(Ordering::Relaxed))
        {
            return;
        }
        match create_folder(folder) {
            Ok(()) => {
                state.ready[at].store(true, Ordering::Relaxed);
                state.finished(ITEM_WORK);
                if folder.parent.is_none() {
                    self.items[folder.item].made.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => state.fail(&folder.src, &error),
        }
    }

    fn copy_file(&self, at: usize, state: &State<'_>) {
        if state.stopped() {
            return;
        }
        let file = &self.files[at];
        if file
            .folder
            .is_some_and(|folder| !state.ready[folder].load(Ordering::Relaxed))
        {
            return;
        }
        let (src, dst) = self.paths(file);
        let chunks = Chunks {
            progress: state.progress,
            size: file.size,
            sent: Cell::new(0),
        };
        let copied = copy_one(&src, &dst, file.size, self.compress, &chunks);
        let left = file.size.saturating_sub(chunks.sent.get());
        match copied {
            Ok(()) => {
                state.progress.bytes.fetch_add(left, Ordering::Relaxed);
                state.finished(ITEM_WORK + left);
                if file.folder.is_none() {
                    self.items[file.item].made.store(true, Ordering::Relaxed);
                }
            }
            // `CopyFile2` has removed the half-written copy.
            Err(_) if state.cancelled() => {}
            Err(error) => {
                state.fail(&src, &error);
                state
                    .progress
                    .work_done
                    .fetch_add(ITEM_WORK + left, Ordering::Relaxed);
            }
        }
    }
}

/// What the copying threads share.
struct State<'a> {
    progress: &'a OpProgress,
    /// Per folder: it has been made.
    ready: Vec<AtomicBool>,
    failed: Mutex<Vec<ItemFailure>>,
    /// The destination drive is full: nothing more is started.
    full: AtomicBool,
}

impl State<'_> {
    fn cancelled(&self) -> bool {
        self.progress.cancel.load(Ordering::Relaxed)
    }

    fn stopped(&self) -> bool {
        self.cancelled() || self.full.load(Ordering::Relaxed)
    }

    /// A file or folder is done, worth `work`.
    fn finished(&self, work: u64) {
        self.progress.done.fetch_add(1, Ordering::Relaxed);
        self.progress.work_done.fetch_add(work, Ordering::Relaxed);
    }

    /// `path` (a source, `\\?\`) failed. A full drive stops the rest.
    fn fail(&self, path: &Path, error: &windows::core::Error) {
        use windows::Win32::Foundation::{ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL};
        if error.code() == ERROR_DISK_FULL.to_hresult()
            || error.code() == ERROR_HANDLE_DISK_FULL.to_hresult()
        {
            self.full.store(true, Ordering::Relaxed);
        }
        let failure = ItemFailure::new(
            arke_core::path::unverbatim(path),
            arke_core::failure::from_hresult(error.code().0),
            crate::win_fileops::message(error),
        );
        self.failed.lock().push(failure);
    }
}

/// Runs `work` on each of `items`, on up to `threads` threads (this one included), each taking
/// the next item when it is done with one.
fn parallel<T: Sync>(items: &[T], threads: usize, work: impl Fn(&T) + Sync) {
    let counter = AtomicUsize::new(0);
    let (next, work) = (&counter, &work);
    let run = move || {
        while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
            work(item);
        }
    };
    let helpers = threads.min(items.len()).saturating_sub(1);
    if helpers == 0 {
        run();
        return;
    }
    std::thread::scope(|scope| {
        for _ in 0..helpers {
            let spawned = std::thread::Builder::new()
                .name("fast-copy".to_owned())
                .spawn_scoped(scope, run);
            if let Err(err) = spawned {
                log::warn!("fast copy: no more threads: {err}");
                break;
            }
        }
        run();
    });
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(MAX_THREADS)
}

// ───────────────────────── Copying ─────────────────────────

/// Makes the copy of `folder`, empty.
fn create_folder(folder: &Folder) -> windows::core::Result<()> {
    let src = HSTRING::from(folder.src.as_os_str());
    let dst = HSTRING::from(folder.dst.as_os_str());
    if folder.attributes & ATTR_REPARSE_POINT == 0 {
        // SAFETY: both strings are NUL-terminated and outlive the call.
        return unsafe { CreateDirectoryExW(&src, &dst, None) };
    }
    // A cloud folder: a plain folder with its attributes.
    // SAFETY: dst is NUL-terminated and outlives the call.
    unsafe { CreateDirectoryW(&dst, None) }?;
    let kept = folder.attributes & ATTR_KEPT;
    if kept != 0 {
        // SAFETY: dst is NUL-terminated and outlives the call.
        if let Err(err) = unsafe { SetFileAttributesW(&dst, FILE_FLAGS_AND_ATTRIBUTES(kept)) } {
            log::debug!(
                "fast copy: {} keeps plain attributes: {err}",
                folder.dst.display()
            );
        }
    }
    Ok(())
}

/// Progress of one big file, for [`on_chunk`].
struct Chunks<'a> {
    progress: &'a OpProgress,
    size: u64,
    /// Bytes of the file already added to the progress.
    sent: Cell<u64>,
}

/// Whether a copy of `size` bytes goes past the cache. Not on one ReFS volume: there Windows can
/// clone the blocks instead of copying them, and an unbuffered copy may keep it from doing so.
const fn no_buffering(size: u64, same_refs_volume: bool) -> bool {
    size >= UNBUFFERED_FILE && !same_refs_volume
}

/// Per volume root, whether it is ReFS (which includes a Dev Drive). The file system of a mounted
/// volume doesn't change while Arke runs, so each root is asked once.
static REFS_ROOTS: Mutex<Vec<(PathBuf, bool)>> = Mutex::new(Vec::new());

/// Whether the volume mounted at `root` is ReFS. `false` when that can't be read.
fn is_refs(root: &Path) -> bool {
    let known = REFS_ROOTS
        .lock()
        .iter()
        .find(|(r, _)| r == root)
        .map(|(_, refs)| *refs);
    if let Some(refs) = known {
        return refs;
    }
    let wide = HSTRING::from(root.as_os_str());
    let mut name = [0u16; 32];
    // SAFETY: wide is NUL-terminated; the buffer and its length are passed together.
    let read = unsafe { GetVolumeInformationW(&wide, None, None, None, None, Some(&mut name)) };
    let refs = read.is_ok()
        && name
            .iter()
            .position(|&c| c == 0)
            .is_some_and(|len| String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("ReFS"));
    REFS_ROOTS.lock().push((root.to_path_buf(), refs));
    refs
}

/// `src` and `dst` are on the same ReFS volume (same mount root).
fn same_refs_volume(src: &Path, dst: &Path) -> bool {
    match (volume_root(src), volume_root(dst)) {
        (Some(a), Some(b)) => a == b && is_refs(&a),
        _ => false,
    }
}

/// Whether `path` is on the network: a UNC path, or a drive letter that `drive_type` (the
/// `GetDriveTypeW` answer for a letter) says is remote. Path text only, apart from `drive_type`.
fn is_network_path(path: &Path, drive_type: impl FnOnce(char) -> u32) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    match prefix.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => true,
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            drive_type(char::from(letter)) == DRIVE_REMOTE
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => false,
    }
}

/// The drive type of `letter:\`, local mount information only.
fn drive_type_of(letter: char) -> u32 {
    let root = HSTRING::from(format!(r"{letter}:\"));
    // SAFETY: root is NUL-terminated.
    unsafe { GetDriveTypeW(&root) }
}

/// Whether this Windows build takes the compression flag; the build is read once.
fn os_takes_compression() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        crate::win::windows_version()
            .is_some_and(|(major, _, build)| build_takes_compression(major, build))
    })
}

/// The build gate of [`os_takes_compression`].
const fn build_takes_compression(major: u32, build: u32) -> bool {
    major >= 10 && build >= COMPRESSION_MIN_BUILD
}

/// A copy between `dest` and `sources` asks for SMB compression when the Windows build takes the
/// flag and any end is on the network. The build is checked first, so older Windows does no more
/// than read a cached flag.
fn wants_compression(dest: &Path, sources: &[PathBuf]) -> bool {
    os_takes_compression()
        && std::iter::once(dest)
            .chain(sources.iter().map(PathBuf::as_path))
            .any(|path| is_network_path(path, drive_type_of))
}

/// The flags for one file: `base`, plus the compression request when asked for and the build
/// takes it (`build_ok`; never on an older Windows, which would fail the call).
const fn with_compression(base: COPYFILE_FLAGS, compress: bool, build_ok: bool) -> COPYFILE_FLAGS {
    if compress && build_ok {
        COPYFILE_FLAGS(base.0 | COPY_FILE_REQUEST_COMPRESSED_TRAFFIC.0)
    } else {
        base
    }
}

/// Copies the file `src` to `dst` (both `\\?\`), which must not exist. Big files report their
/// bytes to `chunks` as they go. `compress` asks for SMB compression (see [`wants_compression`]).
fn copy_one(
    src: &Path,
    dst: &Path,
    size: u64,
    compress: bool,
    chunks: &Chunks<'_>,
) -> windows::core::Result<()> {
    let mut flags = with_compression(
        COPY_FILE_FAIL_IF_EXISTS,
        compress,
        compress && os_takes_compression(),
    );
    // Only the rare huge file asks about the volume (cached), so small files pay nothing.
    if size >= UNBUFFERED_FILE && no_buffering(size, same_refs_volume(src, dst)) {
        flags |= COPY_FILE_NO_BUFFERING;
    }
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: flags,
        pfCancel: std::ptr::null_mut(),
        pProgressRoutine: if size >= BIG_FILE {
            Some(on_chunk)
        } else {
            None
        },
        pvCallbackContext: std::ptr::from_ref(chunks).cast_mut().cast(),
    };
    let src = HSTRING::from(src.as_os_str());
    let dst = HSTRING::from(dst.as_os_str());
    // SAFETY: both strings are NUL-terminated; the parameters and the `Chunks` they point to
    // outlive the call, which calls `on_chunk` only on this thread and before it returns.
    unsafe { CopyFile2(&src, &dst, Some(&raw const params)) }
}

/// `CopyFile2`'s progress callback for big files: adds the bytes of each finished chunk, and
/// cancels the copy when asked to (`CopyFile2` then removes what it wrote).
///
/// # Safety
/// `message` points to a valid message and `context` to the [`Chunks`] given with the call;
/// `CopyFile2` calls this on the thread that called it, before it returns.
unsafe extern "system" fn on_chunk(
    message: *const COPYFILE2_MESSAGE,
    context: *const c_void,
) -> COPYFILE2_MESSAGE_ACTION {
    // SAFETY: as the function's contract says.
    let (message, chunks) = unsafe { (&*message, &*context.cast::<Chunks<'_>>()) };
    if chunks.progress.cancel.load(Ordering::Relaxed) {
        return COPYFILE2_PROGRESS_CANCEL;
    }
    if message.Type == COPYFILE2_CALLBACK_CHUNK_FINISHED {
        // SAFETY: a chunk-finished message carries `ChunkFinished`.
        let total = unsafe { message.Info.ChunkFinished.uliTotalBytesTransferred };
        // Alternate streams come after the file's data and count in the total; only the data is
        // in the progress total.
        let now = total.min(chunks.size).max(chunks.sent.get());
        let added = now - chunks.sent.replace(now);
        chunks.progress.bytes.fetch_add(added, Ordering::Relaxed);
        chunks
            .progress
            .work_done
            .fetch_add(added, Ordering::Relaxed);
    }
    COPYFILE2_PROGRESS_CONTINUE
}

/// Gives the folder at `path` (`\\?\`) the last write time `written` (FILETIME).
fn stamp(path: &Path, written: u64) -> windows::core::Result<()> {
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: wide is NUL-terminated; the handle is closed below.
    let handle = unsafe {
        CreateFileW(
            &wide,
            FILE_WRITE_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }?;
    let time = FILETIME {
        dwLowDateTime: written as u32,
        dwHighDateTime: (written >> 32) as u32,
    };
    // SAFETY: a handle opened with FILE_WRITE_ATTRIBUTES; the time outlives the call.
    let set = unsafe { SetFileTime(handle, None, None, Some(&raw const time)) };
    // SAFETY: opened above, closed once.
    let _ = unsafe { CloseHandle(handle) };
    set
}

/// Tells Explorer windows and the desktop about the new top-level items: one by one up to
/// [`NOTIFY_EACH_MAX`], else the destination folder once.
fn notify(made: &[&Item], dest: &Path) {
    use windows::Win32::UI::Shell::{SHCNE_CREATE, SHCNE_MKDIR, SHCNE_UPDATEDIR};
    if made.len() > NOTIFY_EACH_MAX {
        crate::win_fast_recycle::send(SHCNE_UPDATEDIR, dest);
        return;
    }
    for item in made {
        let event = if item.is_dir {
            SHCNE_MKDIR
        } else {
            SHCNE_CREATE
        };
        crate::win_fast_recycle::send(event, &item.shown);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::shell::{FileOp, OpOutcome};

    /// A folder under %TEMP% named for this test and process, with `src` and `dst` in it; only it
    /// is ever touched.
    struct Scratch(
        PathBuf,
        #[allow(dead_code, reason = "held for its Drop, which deletes the folder")]
        tempfile::TempDir,
    );

    impl Scratch {
        fn new(tag: &str) -> Self {
            let guard = tempfile::Builder::new()
                .prefix(&format!("arke-test-fastcopy-{tag}-"))
                .tempdir()
                .unwrap();
            let dir = guard.path().to_path_buf();
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::create_dir_all(dir.join("dst")).unwrap();
            Self(dir, guard)
        }

        fn src(&self, rel: &str) -> PathBuf {
            self.0.join("src").join(rel)
        }

        fn dst(&self, rel: &str) -> PathBuf {
            self.0.join("dst").join(rel)
        }

        fn file(&self, rel: &str, body: &[u8]) -> PathBuf {
            let path = self.src(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        }
    }

    /// Copies with the fast path, on whatever disk %TEMP% is on.
    fn fast(sources: &[PathBuf], dest: &Path) -> (Copied, OpProgress) {
        let progress = OpProgress::default();
        let copied = copy_with(sources, dest, &progress, false);
        (copied, progress)
    }

    #[test]
    fn network_paths_are_unc_or_remote_drives() {
        let remote = |letter: char| {
            if letter == 'Z' {
                DRIVE_REMOTE
            } else {
                DRIVE_FIXED
            }
        };
        assert!(is_network_path(Path::new(r"\\server\share\a.txt"), remote));
        assert!(is_network_path(
            Path::new(r"\\?\UNC\server\share\a.txt"),
            remote
        ));
        assert!(is_network_path(Path::new(r"Z:\a.txt"), remote));
        assert!(is_network_path(Path::new(r"\\?\Z:\a.txt"), remote));
        assert!(!is_network_path(Path::new(r"C:\a.txt"), remote));
        assert!(!is_network_path(Path::new(r"\\?\C:\a.txt"), remote));
        assert!(!is_network_path(Path::new(r"\\.\PIPE\x"), remote));
        assert!(!is_network_path(Path::new("relative.txt"), remote));
    }

    #[test]
    fn compression_needs_windows_11_22h2() {
        assert!(!build_takes_compression(10, 19045), "Windows 10");
        assert!(!build_takes_compression(10, 22000), "Windows 11 21H2");
        assert!(!build_takes_compression(10, 22620));
        assert!(build_takes_compression(10, 22621), "Windows 11 22H2");
        assert!(build_takes_compression(10, 26200));
        assert!(!build_takes_compression(6, 30000), "an old major never");
    }

    #[test]
    fn the_compression_flag_is_added_only_when_asked_and_supported() {
        let base = COPY_FILE_FAIL_IF_EXISTS;
        assert_eq!(with_compression(base, false, true), base);
        assert_eq!(with_compression(base, true, false), base, "older Windows");
        assert_eq!(with_compression(base, false, false), base);
        assert_eq!(with_compression(base, true, true).0, base.0 | 0x1000_0000);
        assert_eq!(
            with_compression(COPY_FILE_NO_BUFFERING, true, true).0,
            COPY_FILE_NO_BUFFERING.0 | 0x1000_0000,
            "other flags stay"
        );
    }

    #[test]
    fn a_local_copy_does_not_ask_for_compression() {
        let sb = Scratch::new("nocompress");
        let file = sb.file("a.txt", b"a");
        assert!(!wants_compression(&sb.dst(""), &[file]));
    }

    #[test]
    fn refs_same_volume_skips_no_buffering() {
        assert!(no_buffering(UNBUFFERED_FILE, false));
        assert!(!no_buffering(UNBUFFERED_FILE, true));
        assert!(!no_buffering(UNBUFFERED_FILE - 1, false));
    }

    /// Sets the last write time of a file or folder.
    fn set_written(path: &Path, when: SystemTime) {
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    /// Every file and folder under `root`, relative, with its bytes (folders: none) and
    /// modified time.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, SystemTime)> {
        let mut out = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::metadata(&path).unwrap();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let body = if meta.is_dir() {
                    pending.push(path);
                    None
                } else {
                    Some(fs::read(&path).unwrap())
                };
                out.push((rel, body, meta.modified().unwrap()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn a_tree_is_copied_with_its_bytes_names_and_times() {
        let sb = Scratch::new("tree");
        let old = SystemTime::UNIX_EPOCH + Duration::from_hours(416_666);
        let mut bytes = 0u64;
        for d in 0..20u64 {
            for f in 0..10u64 {
                let body = format!("folder {d} file {f}");
                bytes += body.len() as u64;
                let file = sb.file(&format!(r"tree\d{d:02}\f{f:02}.txt"), body.as_bytes());
                set_written(&file, old + Duration::from_secs(d * 100 + f));
            }
        }
        let big: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        bytes += big.len() as u64;
        sb.file(r"tree\big.bin", &big);
        // Folders last: adding their contents changed their times.
        for d in 0..20 {
            set_written(
                &sb.src(&format!(r"tree\d{d:02}")),
                old + Duration::from_secs(d),
            );
        }
        set_written(&sb.src("tree"), old);

        let (copied, progress) = fast(&[sb.src("tree")], &sb.dst(""));
        assert!(copied.failed.is_empty(), "{:?}", copied.failed);
        assert!(copied.rest.is_empty());
        assert!(!copied.cancelled);
        assert_eq!(copied.made, [(0, sb.0.join("dst").join("tree"))]);
        let (src, dst) = (snapshot(&sb.src("tree")), snapshot(&sb.dst("tree")));
        assert_eq!(src.len(), 221, "200 small files, a big one and 20 folders");
        assert_eq!(dst.len(), src.len());
        for (s, d) in src.iter().zip(&dst) {
            assert!(s == d, "{} differs from its copy", s.0.display());
        }
        assert_eq!(
            fs::metadata(sb.dst("tree")).unwrap().modified().unwrap(),
            old,
            "the top folder's time too"
        );
        assert_eq!(progress.bytes.load(Ordering::Relaxed), bytes);
        assert_eq!(
            progress.done.load(Ordering::Relaxed),
            222,
            "files and folders"
        );
        assert_eq!(
            progress.work_done.load(Ordering::Relaxed),
            progress.work_total.load(Ordering::Relaxed)
        );
        assert_eq!(progress.bytes_total.load(Ordering::Relaxed), bytes);
    }

    #[test]
    fn a_name_already_in_the_destination_is_left_to_windows() {
        let sb = Scratch::new("exists");
        let taken = sb.file("a.txt", b"new");
        let free = sb.file("b.txt", b"b");
        fs::write(sb.dst("A.TXT"), b"old").unwrap();
        let (copied, _) = fast(&[taken.clone(), free], &sb.dst(""));
        assert_eq!(copied.rest, [(0, taken)]);
        assert_eq!(copied.made, [(1, sb.0.join("dst").join("b.txt"))]);
        assert_eq!(fs::read(sb.dst("a.txt")).unwrap(), b"old", "untouched");
    }

    #[test]
    fn two_items_of_one_name_leave_the_second_to_windows() {
        let sb = Scratch::new("twice");
        let first = sb.file(r"one\x.txt", b"1");
        let second = sb.file(r"two\X.txt", b"2");
        let (copied, _) = fast(&[first, second.clone()], &sb.dst(""));
        assert_eq!(copied.made.len(), 1);
        assert_eq!(copied.rest, [(1, second)]);
    }

    #[test]
    fn cancel_before_the_start_copies_nothing() {
        let sb = Scratch::new("cancel");
        let source = sb.file(r"tree\a.txt", b"a");
        let progress = OpProgress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let copied = copy_with(&[sb.src("tree"), source], &sb.dst(""), &progress, false);
        assert!(copied.cancelled);
        assert!(copied.made.is_empty() && copied.rest.is_empty());
        assert_eq!(fs::read_dir(sb.dst("")).unwrap().count(), 0);
    }

    #[test]
    fn read_only_files_stay_read_only() {
        let sb = Scratch::new("readonly");
        let file = sb.file("ro.txt", b"keep");
        let set_readonly = |path: &Path, on: bool| {
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_readonly(on);
            fs::set_permissions(path, perms).unwrap();
        };
        set_readonly(&file, true);
        let (copied, _) = fast(std::slice::from_ref(&file), &sb.dst(""));
        let copy = sb.dst("ro.txt");
        let kept = fs::metadata(&copy).map(|m| m.permissions().readonly());
        set_readonly(&file, false);
        if copy.exists() {
            set_readonly(&copy, false);
        }
        assert!(copied.failed.is_empty(), "{:?}", copied.failed);
        assert!(kept.unwrap(), "the copy is read-only too");
    }

    #[test]
    fn alternate_data_streams_come_along() {
        let sb = Scratch::new("streams");
        let file = sb.file(r"folder\file.txt", b"main");
        fs::write(sb.src(r"folder\file.txt:extra"), b"hidden").unwrap();
        let (copied, _) = fast(&[sb.src("folder")], &sb.dst(""));
        assert!(copied.failed.is_empty(), "{:?}", copied.failed);
        assert_eq!(fs::read(sb.dst(r"folder\file.txt")).unwrap(), b"main");
        assert_eq!(
            fs::read(sb.dst(r"folder\file.txt:extra")).unwrap(),
            b"hidden"
        );
        assert!(file.exists());
    }

    #[test]
    fn a_tree_with_a_link_is_left_to_windows() {
        let sb = Scratch::new("link");
        let target = sb.file(r"linked\target.txt", b"t");
        // Symlinks need Developer Mode or elevation; without them there is nothing to test.
        if std::os::windows::fs::symlink_file(&target, sb.src(r"linked\link.txt")).is_err() {
            return;
        }
        let (copied, _) = fast(&[sb.src("linked")], &sb.dst(""));
        assert_eq!(copied.rest, [(0, sb.src("linked"))]);
        assert!(copied.made.is_empty());
        assert!(!sb.dst("linked").exists());
    }

    #[test]
    fn a_destination_inside_the_source_is_left_to_windows() {
        let sb = Scratch::new("inside");
        sb.file(r"outer\inner\a.txt", b"a");
        let (copied, _) = fast(&[sb.src("outer")], &sb.src(r"outer\inner"));
        assert_eq!(copied.rest, [(0, sb.src("outer"))]);
        assert!(!sb.src(r"outer\inner\outer").exists());
    }

    #[test]
    fn small_files_are_taken_from_each_folder_in_turn_after_the_big_ones() {
        let file = |folder: Option<usize>, size: u64| File {
            folder,
            item: 0,
            name: OsString::new(),
            size,
        };
        let plan = Plan {
            files: vec![
                file(Some(0), 1),
                file(Some(0), 1),
                file(Some(0), 1),
                file(Some(1), 1),
                file(Some(1), BIG_FILE),
                file(Some(2), 1),
                file(None, BIG_FILE * 2),
            ],
            ..Plan::default()
        };
        assert_eq!(plan.job_order(), [6, 4, 0, 3, 5, 1, 2]);
    }

    #[test]
    fn undo_lists_top_level_copies_in_the_order_of_their_sources() {
        let dest = Path::new(r"C:\d");
        let fast = Copied {
            made: vec![(0, dest.join("a")), (2, dest.join("c"))],
            rest: vec![(1, PathBuf::from(r"C:\s\b")), (3, PathBuf::from(r"C:\s\e"))],
            ..Copied::default()
        };
        let engine = Undo::Remove(vec![
            dest.join("b - Copy"),
            dest.join(r"b - Copy\inner.txt"),
            dest.join("e"),
        ]);
        assert_eq!(
            merged_undo(&fast, dest, Some(engine)),
            Some(Undo::Remove(vec![
                dest.join("a"),
                dest.join("b - Copy"),
                dest.join("c"),
                dest.join("e"),
                dest.join(r"b - Copy\inner.txt"),
            ]))
        );
        assert_eq!(
            merged_undo(&Copied::default(), dest, None),
            None,
            "nothing made"
        );
    }

    /// Timing for 10,000 small files in 100 folders, this engine against Windows'. Run with
    /// `cargo test -p arke-app --bin arke fast_copy_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: writes 20,000 files to %TEMP%"]
    #[allow(
        clippy::print_stderr,
        reason = "a benchmark run by hand prints its timings"
    )]
    fn fast_copy_timing() {
        let sb = Scratch::new("bench");
        for d in 0..100 {
            let dir = sb.src(&format!(r"bench\d{d:03}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..100 {
                fs::write(dir.join(format!("f{f:03}.txt")), [b'x'; 200]).unwrap();
            }
        }
        let progress = OpProgress::default();
        let started = std::time::Instant::now();
        let copied = copy(&[sb.src("bench")], &sb.dst(""), &progress);
        let took = started.elapsed();
        eprintln!(
            "fast copy of 10,000 files: {took:?} ({} made, {} left to Windows, {} failed)",
            copied.made.len(),
            copied.rest.len(),
            copied.failed.len()
        );
        let engine_dest = sb.0.join("engine");
        fs::create_dir_all(&engine_dest).unwrap();
        let started = std::time::Instant::now();
        let result = crate::win_fileops::perform(
            &FileOp::Copy {
                sources: vec![sb.src("bench")],
                dest: engine_dest.clone(),
            },
            None,
            true,
        );
        eprintln!(
            "IFileOperation copy of 10,000 files: {:?} ({:?})",
            started.elapsed(),
            result.as_ref().map_err(ToString::to_string)
        );
        assert_eq!(result.map_err(|e| e.to_string()), Ok(OpOutcome::Done));
        assert_eq!(
            fs::read_dir(engine_dest.join(r"bench\d099"))
                .unwrap()
                .count(),
            100
        );
        if copied.made.len() == 1 {
            assert_eq!(fs::read_dir(sb.dst(r"bench\d099")).unwrap().count(), 100);
        }
    }
}
