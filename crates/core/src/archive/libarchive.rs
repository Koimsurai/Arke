//! Windows' own libarchive, `System32\archiveint.dll`, loaded at run time.
//!
//! Windows 11 23H2 added it for Explorer's RAR / 7z / tar.xz support, and it exports the normal
//! libarchive C API by name. Using it in-process instead of `tar.exe` matters for names:
//! `archive_entry_pathname_w` returns UTF-16, while tar.exe's front end goes through the ANSI code
//! page (a zh-TW system can't extract Simplified Chinese or Korean names with it).
//!
//! Only the reading side is used, and entries are written by the shared code in `archive.rs`, so
//! libarchive never touches the destination. Compression filters are enabled one by one instead
//! of `archive_read_support_filter_all`, which would also allow filters that run external
//! programs (lrzip, lzop, grzip) found on `PATH`.

use super::{
    ArchiveError, ArchiveItem, ArchiveKind, ArchiveListing, ExtractOptions, ExtractProgress,
    ExtractSummary, SystemReaderError, cancelled, display_name, rar_volumes, safe_relative_path,
    write_new_file,
};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fs;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{PCSTR, w};

/// `struct archive` (opaque).
#[repr(C)]
pub(super) struct RawArchive {
    _opaque: [u8; 0],
}

/// `struct archive_entry` (opaque).
#[repr(C)]
pub(super) struct RawEntry {
    _opaque: [u8; 0],
}

const ARCHIVE_EOF: c_int = 1;
const ARCHIVE_OK: c_int = 0;
const ARCHIVE_WARN: c_int = -20;
/// After this, libarchive aborts the process if anything but `archive_error_string` or
/// `archive_read_free` is called on the handle (its state check calls abort()).
const ARCHIVE_FATAL: c_int = -30;
/// File type bits of `archive_entry_filetype` (an `unsigned short` mode on Windows).
const AE_IFMT: u16 = 0o170_000;
const AE_IFREG: u16 = 0o100_000;
const AE_IFDIR: u16 = 0o040_000;
/// libarchive's read buffer per volume.
const BLOCK_SIZE: usize = 64 * 1024;

/// Declares the functions used, loaded by name from the DLL.
macro_rules! api {
    ($($field:ident: fn($($arg:ty),*) -> $ret:ty = $symbol:literal;)*) => {
        /// The libarchive functions Arke uses, resolved from archiveint.dll.
        pub(super) struct Api {
            $($field: unsafe extern "C" fn($($arg),*) -> $ret,)*
        }

        impl Api {
            fn load() -> Option<Self> {
                // SAFETY: plain DLL load restricted to System32 (no current-directory or PATH
                // search, so no planted copy). The module is never freed: the function pointers
                // below live for the rest of the process.
                let module = unsafe {
                    LoadLibraryExW(w!("archiveint.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
                }
                .ok()?;
                Some(Self {
                    $($field: {
                        // SAFETY: the name is a NUL-terminated literal and the module is loaded.
                        let proc = unsafe {
                            GetProcAddress(module, PCSTR(concat!($symbol, "\0").as_ptr()))
                        }?;
                        // SAFETY: the signature is the one in libarchive's public archive.h /
                        // archive_entry.h (3.x, a stable C ABI; x64 and ARM64 each have one C
                        // calling convention, so `extern "C"` matches the DLL on both).
                        unsafe {
                            std::mem::transmute::<
                                unsafe extern "system" fn() -> isize,
                                unsafe extern "C" fn($($arg),*) -> $ret,
                            >(proc)
                        }
                    },)*
                })
            }
        }
    };
}

api! {
    read_new: fn() -> *mut RawArchive = "archive_read_new";
    read_free: fn(*mut RawArchive) -> c_int = "archive_read_free";
    support_format_rar: fn(*mut RawArchive) -> c_int = "archive_read_support_format_rar";
    support_format_rar5: fn(*mut RawArchive) -> c_int = "archive_read_support_format_rar5";
    support_format_tar: fn(*mut RawArchive) -> c_int = "archive_read_support_format_tar";
    support_filter_xz: fn(*mut RawArchive) -> c_int = "archive_read_support_filter_xz";
    support_filter_bzip2: fn(*mut RawArchive) -> c_int = "archive_read_support_filter_bzip2";
    support_filter_zstd: fn(*mut RawArchive) -> c_int = "archive_read_support_filter_zstd";
    open_filenames_w: fn(*mut RawArchive, *const *const u16, usize) -> c_int
        = "archive_read_open_filenames_w";
    next_header: fn(*mut RawArchive, *mut *mut RawEntry) -> c_int = "archive_read_next_header";
    read_data: fn(*mut RawArchive, *mut c_void, usize) -> isize = "archive_read_data";
    error_string: fn(*mut RawArchive) -> *const c_char = "archive_error_string";
    filter_bytes: fn(*mut RawArchive, c_int) -> i64 = "archive_filter_bytes";
    has_encrypted_entries: fn(*mut RawArchive) -> c_int
        = "archive_read_has_encrypted_entries";
    entry_pathname_w: fn(*mut RawEntry) -> *const u16 = "archive_entry_pathname_w";
    entry_pathname: fn(*mut RawEntry) -> *const c_char = "archive_entry_pathname";
    entry_size: fn(*mut RawEntry) -> i64 = "archive_entry_size";
    entry_filetype: fn(*mut RawEntry) -> u16 = "archive_entry_filetype";
    entry_hardlink: fn(*mut RawEntry) -> *const c_char = "archive_entry_hardlink";
    entry_hardlink_w: fn(*mut RawEntry) -> *const u16 = "archive_entry_hardlink_w";
    entry_is_encrypted: fn(*mut RawEntry) -> c_int = "archive_entry_is_encrypted";
}

/// The DLL's API, loaded once; `None` before Windows 11 23H2 (or if an export is missing).
pub(super) fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(Api::load).as_ref()
}

/// What an entry is, as far as extraction cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryType {
    File,
    Dir,
    /// Symlinks, hard links, devices: skipped.
    Other,
}

/// One entry's header, copied out before the next `next_header` invalidates it.
struct Header {
    /// UTF-8 when libarchive had a Unicode name, otherwise its ANSI bytes.
    name: Vec<u8>,
    size: u64,
    kind: EntryType,
    encrypted: bool,
}

/// An open `struct archive`, freed on drop.
struct Reader<'a> {
    api: &'static Api,
    raw: *mut RawArchive,
    /// Compressed bytes consumed so far, for progress (updated as data is read).
    position: Arc<AtomicU64>,
    /// A header has been read (errors before that mean "not this format").
    started: bool,
    /// A call returned ARCHIVE_FATAL: only error_string and free are allowed from now on.
    fatal: bool,
    decode_legacy: &'a dyn Fn(&[u8]) -> String,
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` came from archive_read_new and is freed exactly once, here.
        unsafe { (self.api.read_free)(self.raw) };
    }
}

/// libarchive wants Windows paths as NUL-terminated UTF-16; long paths need the `\\?\` form
/// because it calls CreateFileW directly.
fn wide_path(path: &Path) -> Vec<u16> {
    let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    let long = raw.len() >= 240;
    let mut out: Vec<u16> = if long && path.is_absolute() && !raw.starts_with(&[92, 92]) {
        r"\\?\".encode_utf16().chain(raw).collect()
    } else if long && raw.starts_with(&[92, 92]) && !raw.starts_with(&[92, 92, 63, 92]) {
        // \\server\share → \\?\UNC\server\share
        r"\\?\UNC"
            .encode_utf16()
            .chain(raw[1..].iter().copied())
            .collect()
    } else {
        raw
    };
    out.push(0);
    out
}

impl<'a> Reader<'a> {
    fn open(
        api: &'static Api,
        kind: ArchiveKind,
        volumes: &[PathBuf],
        decode_legacy: &'a dyn Fn(&[u8]) -> String,
    ) -> Result<Self, ArchiveError> {
        // SAFETY: no preconditions; null is handled below.
        let raw = unsafe { (api.read_new)() };
        if raw.is_null() {
            return Err(ArchiveError::CorruptSystem {
                source: SystemReaderError {
                    code: -30,
                    message: "archive_read_new failed".into(),
                },
            });
        }
        let mut reader = Self {
            api,
            raw,
            position: Arc::default(),
            started: false,
            fatal: false,
            decode_legacy,
        };
        let enable: &[unsafe extern "C" fn(*mut RawArchive) -> c_int] = match kind {
            ArchiveKind::Rar => &[api.support_format_rar, api.support_format_rar5],
            ArchiveKind::TarXz => &[api.support_format_tar, api.support_filter_xz],
            ArchiveKind::TarBz2 => &[api.support_format_tar, api.support_filter_bzip2],
            ArchiveKind::TarZst => &[api.support_format_tar, api.support_filter_zstd],
            ArchiveKind::Zip | ArchiveKind::Tar | ArchiveKind::TarGz | ArchiveKind::SevenZip => &[],
        };
        for support in enable {
            // SAFETY: `raw` is a fresh, unopened read handle.
            let status = unsafe { support(raw) };
            // ARCHIVE_WARN means "built without the library, will run an external program
            // instead"; refuse rather than run whatever `xz` is on PATH.
            if status != ARCHIVE_OK {
                return Err(ArchiveError::SystemUnsupported {
                    format: kind.label(),
                    source: reader.error(status),
                });
            }
        }
        let wide: Vec<Vec<u16>> = volumes.iter().map(|p| wide_path(p)).collect();
        let mut names: Vec<*const u16> = wide.iter().map(Vec::as_ptr).collect();
        names.push(std::ptr::null());
        // SAFETY: `names` is a NULL-terminated array of NUL-terminated UTF-16 strings that
        // outlives the call; libarchive copies the names while opening.
        let status = unsafe { (api.open_filenames_w)(raw, names.as_ptr(), BLOCK_SIZE) };
        if status != ARCHIVE_OK && status != ARCHIVE_WARN {
            reader.fatal |= status == ARCHIVE_FATAL;
            return Err(reader.classify(kind, status, true));
        }
        Ok(reader)
    }

    /// The current error as reported by libarchive.
    fn error(&self, code: c_int) -> SystemReaderError {
        // SAFETY: `raw` is valid; the string (if any) is NUL-terminated and owned by the handle.
        let text = unsafe { (self.api.error_string)(self.raw) };
        let message = if text.is_null() {
            format!("libarchive error {code}")
        } else {
            // SAFETY: non-null and NUL-terminated (see above); copied before the next call.
            let bytes = unsafe { CStr::from_ptr(text) }.to_bytes();
            match std::str::from_utf8(bytes) {
                Ok(s) => s.to_owned(),
                Err(_) => (self.decode_legacy)(bytes),
            }
        };
        SystemReaderError { code, message }
    }

    /// Turns a failed open / header read into the most specific error.
    fn classify(&self, kind: ArchiveKind, code: c_int, opening: bool) -> ArchiveError {
        let source = self.error(code);
        // SAFETY: `raw` is valid and not in the fatal state (checked first).
        let encrypted = !self.fatal && unsafe { (self.api.has_encrypted_entries)(self.raw) } > 0;
        if encrypted || source.message.to_ascii_lowercase().contains("encrypt") {
            ArchiveError::Encrypted
        } else if opening {
            ArchiveError::NotSystemFormat {
                format: kind.label(),
                source,
            }
        } else {
            ArchiveError::CorruptSystem { source }
        }
    }

    fn next(&mut self, kind: ArchiveKind) -> Result<Option<Header>, ArchiveError> {
        if self.fatal {
            return Err(ArchiveError::CorruptSystem {
                source: self.error(ARCHIVE_FATAL),
            });
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: `raw` is open and not fatal; `entry` receives a pointer owned by the handle.
        let status = unsafe { (self.api.next_header)(self.raw, &raw mut entry) };
        self.fatal |= status == ARCHIVE_FATAL;
        self.update_position();
        match status {
            ARCHIVE_EOF => return Ok(None),
            // A warning (e.g. a name that doesn't fit the ANSI code page) still gives a usable
            // entry; the wide name below is unaffected.
            ARCHIVE_OK | ARCHIVE_WARN if !entry.is_null() => {}
            // Failing on the first header means the format wasn't recognized after all.
            status => return Err(self.classify(kind, status, !self.started)),
        }
        // SAFETY: `entry` is valid until the next next_header call, and every string it returns
        // is NUL-terminated and copied out before then.
        Ok(Some(unsafe { self.header(entry) }))
    }

    /// Copies an entry's fields.
    ///
    /// # Safety
    /// `entry` must be the entry returned by the last successful `next_header` on this handle.
    unsafe fn header(&self, entry: *mut RawEntry) -> Header {
        let api = self.api;
        // SAFETY: see the function contract; wide and narrow strings returned by
        // libarchive are NUL-terminated and owned by the entry.
        unsafe {
            let wide = (api.entry_pathname_w)(entry);
            let name = if wide.is_null() {
                // No Unicode form: libarchive's bytes are in the ANSI code page, which
                // safe_relative_path decodes with the legacy decoder.
                let narrow = (api.entry_pathname)(entry);
                if narrow.is_null() {
                    Vec::new()
                } else {
                    CStr::from_ptr(narrow).to_bytes().to_vec()
                }
            } else {
                let mut len = 0;
                while *wide.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(wide, len)).into_bytes()
            };
            let hardlink =
                !(api.entry_hardlink_w)(entry).is_null() || !(api.entry_hardlink)(entry).is_null();
            let kind = match (api.entry_filetype)(entry) & AE_IFMT {
                _ if hardlink => EntryType::Other,
                AE_IFREG => EntryType::File,
                AE_IFDIR => EntryType::Dir,
                _ => EntryType::Other,
            };
            Header {
                name,
                size: u64::try_from((api.entry_size)(entry)).unwrap_or(0),
                kind,
                encrypted: (api.entry_is_encrypted)(entry) != 0,
            }
        }
    }

    fn update_position(&self) {
        if self.fatal {
            return;
        }
        // SAFETY: `raw` is valid and not fatal; -1 is the raw (compressed) end of the filters.
        let bytes = unsafe { (self.api.filter_bytes)(self.raw, -1) };
        self.position
            .store(u64::try_from(bytes).unwrap_or(0), Ordering::Relaxed);
    }
}

impl Read for Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.fatal {
            return Err(io::Error::other(self.error(ARCHIVE_FATAL)));
        }
        // SAFETY: `buf` is valid for `buf.len()` writable bytes; libarchive writes at most that.
        let n = unsafe { (self.api.read_data)(self.raw, buf.as_mut_ptr().cast(), buf.len()) };
        let Ok(n) = usize::try_from(n) else {
            let code = c_int::try_from(n).unwrap_or(ARCHIVE_FATAL);
            self.fatal |= code == ARCHIVE_FATAL;
            return Err(io::Error::other(self.error(code)));
        };
        self.update_position();
        Ok(n)
    }
}

fn volumes_for(kind: ArchiveKind, path: &Path) -> Vec<PathBuf> {
    if kind == ArchiveKind::Rar {
        rar_volumes(path)
    } else {
        vec![path.to_path_buf()]
    }
}

/// Lists entries, stopping at item `max` (a solid RAR has to decode to skip, so the rest is not
/// read just for totals).
pub(super) fn list(
    api: &'static Api,
    kind: ArchiveKind,
    path: &Path,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    let mut reader = Reader::open(api, kind, &volumes_for(kind, path), decode_legacy)?;
    let mut listing = ArchiveListing {
        complete: true,
        ..ArchiveListing::default()
    };
    while let Some(header) = reader.next(kind)? {
        if header.kind == EntryType::Other {
            continue;
        }
        if listing.is_full(max) {
            listing.complete = false;
            break;
        }
        let is_dir = header.kind == EntryType::Dir;
        let item = ArchiveItem {
            path: display_name(&header.name, decode_legacy),
            size: if is_dir { 0 } else { header.size },
            is_dir,
        };
        listing.push(item, max);
    }
    Ok(listing)
}

/// Extracts with the shared rules; progress is compressed bytes read out of all volumes.
pub(super) fn extract(
    api: &'static Api,
    kind: ArchiveKind,
    archive: &Path,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    let volumes = volumes_for(kind, archive);
    let total = volumes
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut reader = Reader::open(api, kind, &volumes, opts.decode_legacy)?;
    let mut summary = ExtractSummary::default();
    let mut progress = ExtractProgress {
        total,
        ..ExtractProgress::default()
    };
    (opts.progress)(progress);
    while let Some(header) = reader.next(kind)? {
        progress.done = reader.position.load(Ordering::Relaxed).min(total);
        (opts.progress)(progress);
        if cancelled(opts) {
            summary.cancelled = true;
            return Ok(summary);
        }
        let Some(rel) = safe_relative_path(&header.name, opts.decode_legacy) else {
            summary.skipped_unsafe += 1;
            continue;
        };
        let target = dest.join(&rel);
        match header.kind {
            EntryType::Dir => {
                fs::create_dir_all(&target).map_err(|source| ArchiveError::Create {
                    path: target.clone(),
                    source,
                })?;
                summary.dirs += 1;
            }
            EntryType::File => {
                if header.encrypted {
                    return Err(ArchiveError::Encrypted);
                }
                let position = reader.position.clone();
                let mut on_bytes = |_: u64| {
                    progress.done = position.load(Ordering::Relaxed).min(total);
                    (opts.progress)(progress);
                };
                if write_new_file(&target, &mut reader, &mut on_bytes, opts)? {
                    summary.files += 1;
                    progress.files = summary.files;
                } else {
                    summary.skipped_existing += 1;
                }
            }
            EntryType::Other => summary.skipped_unsafe += 1,
        }
    }
    summary.cancelled = cancelled(opts);
    if !summary.cancelled {
        // The position stops short of the end (trailing headers are never read).
        progress.done = total;
        (opts.progress)(progress);
    }
    Ok(summary)
}
