//! Archives: zip, tar and tar.gz in Rust; 7z in Rust (`sevenz`); RAR and tar.xz/.bz2/.zst through
//! Windows' own libarchive (`system`). Background threads only (heavy file I/O).
//!
//! No RAR decoder is linked: the UnRAR license isn't AGPL-compatible, and libarchive's own RAR
//! reader is only used as the copy Windows ships (archiveint.dll, or tar.exe on older systems).
//! Every format's entries go through the same `safe_relative_path` and `write_new_file` below, so
//! the rules are the same for all of them.
//!
//! Safety rules:
//! - Every entry path is rechecked: absolute paths, `..` and drive letters are rejected (zip slip).
//!   Characters Windows forbids become `_`; reserved names (CON, NUL...) get a `_` prefix.
//! - **Existing files are never overwritten**: name clashes are skipped and counted.
//! - Symlinks, hard links and device files are skipped.
//! - Cancellable at any time; already extracted files are kept (as in Explorer) and the caller
//!   reports them.
//!
//! File names: valid UTF-8 is used as is, otherwise the caller's legacy decoder is used (the system
//! ANSI code page on Windows), so names in old non-UTF-8 zips stay readable.

use rust_i18n::t;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
mod bsdtar;
#[cfg(windows)]
mod libarchive;
mod sevenz;
mod system;

/// Why an archive could not be extracted, compressed or listed.
///
/// Display is the localized, user-facing message (`archive.*` keys in `core.yml`). Cancellation is
/// not an error: it is reported through `cancelled` in the summaries.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The destination archive already exists; compressing never overwrites.
    #[error("{}", t!("archive.exists", path = .path.display()))]
    Exists {
        /// The existing destination.
        path: PathBuf,
    },
    /// A folder or the archive file could not be created.
    #[error("{}", t!("archive.create_failed", path = .path.display(), error = .source))]
    Create {
        /// What was being created.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The archive file could not be opened.
    #[error("{}", t!("archive.open_failed", error = .source))]
    Open {
        /// The archive.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// An extracted file could not be written.
    #[error("{}", t!("archive.write_failed", path = .path.display(), error = .source))]
    Write {
        /// The file being written.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// An entry's data could not be read from the archive.
    #[error("{}", t!("archive.read_failed", name = .path.display(), error = .source))]
    Read {
        /// Where the entry was being extracted to.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not a valid zip.
    #[error("{}", t!("archive.not_zip", error = .source))]
    NotZip {
        /// Underlying error.
        #[source]
        source: zip::result::ZipError,
    },
    /// The file is not a valid tar.
    #[error("{}", t!("archive.not_tar", error = .source))]
    NotTar {
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A zip entry is damaged.
    #[error("{}", t!("archive.corrupt", error = .source))]
    CorruptZip {
        /// Underlying error.
        #[source]
        source: zip::result::ZipError,
    },
    /// A tar entry is damaged.
    #[error("{}", t!("archive.corrupt", error = .source))]
    CorruptTar {
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The zip writer failed while compressing.
    #[error("{}", t!("archive.zip_failed", error = .source))]
    Zip {
        /// Underlying error.
        #[source]
        source: zip::result::ZipError,
    },
    /// A source file could not be read while compressing.
    #[error("{}", t!("archive.zip_failed", error = .source))]
    ReadSource {
        /// The file being added.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The zip being created could not be written.
    #[error("{}", t!("archive.zip_failed", error = .source))]
    WriteZip {
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not a valid 7z archive.
    #[error("{}", t!("archive.not_format", format = "7z", error = .source))]
    NotSevenZip {
        /// Underlying error.
        #[source]
        source: sevenz_rust2::Error,
    },
    /// A 7z entry is damaged (bad CRC, truncated stream...).
    #[error("{}", t!("archive.corrupt", error = .source))]
    CorruptSevenZip {
        /// Underlying error.
        #[source]
        source: sevenz_rust2::Error,
    },
    /// The archive (or an entry in it) is password-protected; there is no password prompt yet.
    #[error("{}", t!("archive.encrypted"))]
    Encrypted,
    /// The archive uses a compression method the reader doesn't implement.
    #[error("{}", t!("archive.unsupported_method", method = .method))]
    UnsupportedMethod {
        /// Method name as reported by the decoder.
        method: String,
    },
    /// Windows' libarchive doesn't recognize the file as the expected format.
    #[error("{}", t!("archive.not_format", format = .format, error = .source))]
    NotSystemFormat {
        /// Format label (`RAR`, `tar.xz`...).
        format: &'static str,
        /// What libarchive reported.
        #[source]
        source: SystemReaderError,
    },
    /// Windows' libarchive failed in the middle of the archive.
    #[error("{}", t!("archive.corrupt", error = .source))]
    CorruptSystem {
        /// What libarchive reported.
        #[source]
        source: SystemReaderError,
    },
    /// Neither archiveint.dll nor tar.exe is available, so this format can't be read at all.
    #[error("{}", t!("archive.system_missing", format = .format))]
    SystemReaderMissing {
        /// Format label (`RAR`, `tar.xz`...).
        format: &'static str,
    },
    /// Only the older tar.exe is available and it can't read this file: either the file is damaged
    /// or that tar.exe predates support for the format (Windows 10's has no RAR5, xz or zstd).
    #[error("{}", t!("archive.system_unsupported", format = .format, error = .source))]
    SystemUnsupported {
        /// Format label (`RAR`, `tar.xz`...).
        format: &'static str,
        /// What tar.exe reported.
        #[source]
        source: SystemReaderError,
    },
    /// tar.exe crashed. Its front end converts names to the ANSI code page and crashes on names
    /// that don't fit, so this is reported as a file name problem.
    #[error("{}", t!("archive.system_names"))]
    TarNames {
        /// Exit code and what tar.exe printed.
        #[source]
        source: SystemReaderError,
    },
    /// tar.exe failed for another reason.
    #[error("{}", t!("archive.tar_failed", error = .source))]
    TarFailed {
        /// Exit code and what tar.exe printed.
        #[source]
        source: SystemReaderError,
    },
    /// tar.exe could not be started.
    #[error("{}", t!("archive.run_failed", path = .path.display(), error = .source))]
    RunTar {
        /// tar.exe's path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// [`list`] was given a stream for a kind that can only be read from a file
    /// ([`ArchiveKind::needs_path`]); use [`list_path`].
    #[error("{}", t!("archive.needs_file", format = .format))]
    NeedsFile {
        /// Format label.
        format: &'static str,
    },
}

/// An error reported by Windows' libarchive (archiveint.dll) or by tar.exe.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SystemReaderError {
    /// libarchive's status code, or tar.exe's exit code.
    pub code: i32,
    /// The message as reported (libarchive's text is English, decoded from the ANSI code page).
    pub message: String,
}

/// Archive format, detected from the file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.zip` (stored or deflate).
    Zip,
    /// Uncompressed `.tar`.
    Tar,
    /// Gzip-compressed tar (`.tar.gz`, `.tgz`).
    TarGz,
    /// xz-compressed tar (`.tar.xz`, `.txz`), read by Windows' libarchive.
    TarXz,
    /// bzip2-compressed tar (`.tar.bz2`, `.tbz2`, `.tbz`), read by Windows' libarchive.
    TarBz2,
    /// Zstandard-compressed tar (`.tar.zst`, `.tzst`), read by Windows' libarchive.
    TarZst,
    /// `.7z`, decoded in-process (sevenz-rust2).
    SevenZip,
    /// `.rar` (RAR4 and RAR5, and the first volume of a `.partN.rar` set), read by Windows'
    /// libarchive.
    Rar,
}

impl ArchiveKind {
    /// Whether the archive must be read from a file on disk (Windows' libarchive opens files by
    /// path, and multi-volume sets need the sibling files), so [`list_path`] is needed instead of
    /// [`list`].
    pub const fn needs_path(self) -> bool {
        matches!(self, Self::TarXz | Self::TarBz2 | Self::TarZst | Self::Rar)
    }

    /// Short format name for messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::TarBz2 => "tar.bz2",
            Self::TarZst => "tar.zst",
            Self::SevenZip => "7z",
            Self::Rar => "RAR",
        }
    }
}

/// Supported extensions, longest first (`.tar.gz` must match before `.tar`).
const SUFFIXES: &[(&str, ArchiveKind)] = &[
    (".tar.gz", ArchiveKind::TarGz),
    (".tar.xz", ArchiveKind::TarXz),
    (".tar.bz2", ArchiveKind::TarBz2),
    (".tar.zst", ArchiveKind::TarZst),
    (".tbz2", ArchiveKind::TarBz2),
    (".tzst", ArchiveKind::TarZst),
    (".tgz", ArchiveKind::TarGz),
    (".txz", ArchiveKind::TarXz),
    (".tbz", ArchiveKind::TarBz2),
    (".zip", ArchiveKind::Zip),
    (".tar", ArchiveKind::Tar),
    (".rar", ArchiveKind::Rar),
    (".7z", ArchiveKind::SevenZip),
];

/// Most volumes looked for in a multi-volume RAR set.
const MAX_VOLUMES: u32 = 9999;

/// Case-insensitive suffix match (`get` avoids slicing inside a multibyte char).
fn suffix_len(name: &str) -> Option<(usize, ArchiveKind)> {
    let (len, kind) = SUFFIXES.iter().find_map(|(suffix, kind)| {
        let start = name.len().checked_sub(suffix.len())?;
        name.get(start..)?
            .eq_ignore_ascii_case(suffix)
            .then_some((suffix.len(), *kind))
    })?;
    if kind == ArchiveKind::Rar
        && let Some(part) = rar_part(&name[..name.len() - len])
    {
        // libarchive can only read a set from its first volume, so later parts aren't offered;
        // the folder name drops ".part1" too.
        return (part.number == 1).then_some((len + part.suffix_len, kind));
    }
    Some((len, kind))
}

/// The `.partN` in `name.partN.rar` (the stem passed here has `.rar` removed).
#[derive(Debug, PartialEq, Eq)]
struct RarPart {
    /// Length of `.partN`.
    suffix_len: usize,
    /// Number of digits (`part01` → 2), kept when naming the next volumes.
    width: usize,
    number: u32,
}

fn rar_part(stem: &str) -> Option<RarPart> {
    let dot = stem.rfind('.')?;
    let tail = &stem[dot + 1..];
    if !tail.get(..4)?.eq_ignore_ascii_case("part") {
        return None;
    }
    let digits = tail.get(4..)?;
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(RarPart {
        suffix_len: stem.len() - dot,
        width: digits.len(),
        number: digits.parse().ok()?,
    })
}

/// Every volume of the RAR set starting at `first` (`x.part1.rar`, `x.part2.rar`... or the older
/// `x.rar`, `x.r00`, `x.r01`...), in order and stopping at the first missing one. Just `first` for a
/// single-volume archive.
fn rar_volumes(first: &Path) -> Vec<PathBuf> {
    let mut out = vec![first.to_path_buf()];
    let Some(name) = first.file_name().and_then(|n| n.to_str()) else {
        return out;
    };
    let Some(stem) = name.len().checked_sub(4).and_then(|cut| name.get(..cut)) else {
        return out;
    };
    let ext = &name[stem.len()..];
    let next_names: Box<dyn Iterator<Item = String>> = match rar_part(stem) {
        Some(part) => {
            let base = &stem[..stem.len() - part.suffix_len];
            // Keep the original spelling of ".part" (5 ASCII bytes).
            let word = &stem[base.len()..base.len() + 5];
            Box::new(
                (part.number + 1..=MAX_VOLUMES)
                    .map(move |n| format!("{base}{word}{n:0width$}{ext}", width = part.width)),
            )
        }
        None => Box::new((0..100).map(move |n| format!("{stem}.r{n:02}"))),
    };
    for next in next_names {
        let path = first.with_file_name(next);
        if !path.is_file() {
            break;
        }
        out.push(path);
    }
    out
}

/// Whether a file can be extracted, by name.
pub fn archive_kind(name: &str) -> Option<ArchiveKind> {
    suffix_len(name).map(|(_, kind)| kind)
}

/// Folder name for "Extract to <name>\": the archive name without its extension.
pub fn folder_name_for(archive_name: &str) -> String {
    let cut = suffix_len(archive_name).map_or(0, |(len, _)| len);
    let stem = archive_name[..archive_name.len() - cut].trim_end_matches(['.', ' ']);
    if stem.is_empty() {
        t!("archive.folder_fallback").into_owned()
    } else {
        stem.to_owned()
    }
}

/// Progress report passed to [`ExtractOptions::progress`] and [`CompressOptions::progress`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtractProgress {
    /// Progress and total (uncompressed size for zip and 7z; compressed bytes read for tar and the
    /// formats read by Windows' libarchive).
    pub done: u64,
    /// Final value of `done` (same unit).
    pub total: u64,
    /// Files written so far.
    pub files: u64,
}

/// Result of [`extract`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtractSummary {
    /// Files extracted.
    pub files: u64,
    /// Folder entries created (or already present).
    pub dirs: u64,
    /// Skipped because the destination already had that name (no overwrite).
    pub skipped_existing: u64,
    /// Skipped as unsafe path or unsupported type (links etc.).
    pub skipped_unsafe: u64,
    /// Stopped by `cancel`; files extracted before that are kept.
    pub cancelled: bool,
}

/// Callbacks and cancellation for [`extract`].
pub struct ExtractOptions<'a> {
    /// Set from another thread to stop as soon as possible.
    pub cancel: &'a AtomicBool,
    /// Called periodically from the extracting thread.
    pub progress: &'a (dyn Fn(ExtractProgress) + Sync),
    /// Decoder for non-UTF-8 names.
    pub decode_legacy: &'a (dyn Fn(&[u8]) -> String + Sync),
}

/// Extracts into `dest`, creating it if needed.
pub fn extract(
    archive: &Path,
    dest: &Path,
    kind: ArchiveKind,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    crate::fs::debug_assert_not_ui_thread("extract", archive);
    fs::create_dir_all(dest).map_err(|source| ArchiveError::Create {
        path: dest.to_path_buf(),
        source,
    })?;
    let file = File::open(archive).map_err(|source| ArchiveError::Open {
        path: archive.to_path_buf(),
        source,
    })?;
    match kind {
        ArchiveKind::Zip => extract_zip(file, dest, opts),
        ArchiveKind::Tar => {
            let total = file.metadata().map_or(0, |m| m.len());
            extract_tar(CountingReader::new(file), total, dest, opts)
        }
        ArchiveKind::TarGz => {
            let total = file.metadata().map_or(0, |m| m.len());
            let counting = CountingReader::new(file);
            let read = counting.read_counter();
            let gz = flate2::read::GzDecoder::new(counting);
            extract_tar_with_counter(gz, &|| read.load(Ordering::Relaxed), total, dest, opts)
        }
        ArchiveKind::SevenZip => sevenz::extract(file, dest, opts),
        ArchiveKind::TarXz | ArchiveKind::TarBz2 | ArchiveKind::TarZst | ArchiveKind::Rar => {
            // Opened above only to report a missing or locked file the same way for every kind.
            drop(file);
            system::extract(kind, archive, dest, opts)
        }
    }
}

// ───────────────────────── Compress ─────────────────────────

/// Result of compressing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressSummary {
    /// Files added.
    pub files: u64,
    /// Folders added.
    pub dirs: u64,
    /// Skipped links and unreadable items.
    pub skipped: u64,
    /// Cancelled by the user (no partial archive is left).
    pub cancelled: bool,
}

/// Callbacks and cancellation for compressing.
pub struct CompressOptions<'a> {
    /// Set from another thread to stop; the partial archive is deleted.
    pub cancel: &'a AtomicBool,
    /// Progress in bytes compressed / total (same `ExtractProgress` type, shared by the toast).
    pub progress: &'a (dyn Fn(ExtractProgress) + Sync),
    /// Converts a modified time to local (year, month, day, hour, minute, second); zip stores local
    /// time.
    pub local_time: &'a (dyn Fn(std::time::SystemTime) -> Option<[u16; 6]> + Sync),
}

/// One item to put in the archive.
enum Entry {
    Dir {
        name: String,
        modified: Option<std::time::SystemTime>,
    },
    File {
        path: PathBuf,
        name: String,
        size: u64,
        modified: Option<std::time::SystemTime>,
    },
}

/// Compresses `sources` (files and folders in one folder) into `dest` (.zip).
///
/// - Writes to `dest.partial` and renames on success; deleted on cancel or failure, so no
///   broken archive is left behind.
/// - Fails if `dest` exists (no overwrite); the caller picks a unique name first.
/// - Links and junctions are skipped and counted, never followed (no whole drives or cycles).
pub fn compress(
    sources: &[PathBuf],
    dest: &Path,
    opts: &CompressOptions<'_>,
) -> Result<CompressSummary, ArchiveError> {
    crate::fs::debug_assert_not_ui_thread("compress", dest);
    let mut summary = CompressSummary::default();
    let mut entries = Vec::new();
    for source in sources {
        let Some(name) = source.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        collect(source, &name, &mut entries, &mut summary, opts.cancel);
    }
    if opts.cancel.load(Ordering::Relaxed) {
        summary.cancelled = true;
        return Ok(summary);
    }
    let total: u64 = entries
        .iter()
        .map(|e| match e {
            Entry::File { size, .. } => *size,
            Entry::Dir { .. } => 0,
        })
        .sum();
    if dest.exists() {
        return Err(ArchiveError::Exists {
            path: dest.to_path_buf(),
        });
    }
    let partial = dest.with_extension("zip.partial");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(|source| ArchiveError::Create {
            path: partial.clone(),
            source,
        })?;
    let result = write_zip(file, &entries, total, &mut summary, opts);
    match result {
        Ok(()) if !summary.cancelled => {
            fs::rename(&partial, dest).map_err(|source| {
                let _ = fs::remove_file(&partial);
                ArchiveError::Create {
                    path: dest.to_path_buf(),
                    source,
                }
            })?;
            Ok(summary)
        }
        Ok(()) => {
            let _ = fs::remove_file(&partial);
            Ok(summary)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Lists items under `path` to compress (names `/`-separated, relative to the source's folder).
fn collect(
    path: &Path,
    name: &str,
    out: &mut Vec<Entry>,
    summary: &mut CompressSummary,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    // symlink_metadata: don't follow links (junctions and symlinks both count as links).
    let Ok(meta) = fs::symlink_metadata(path) else {
        summary.skipped += 1;
        return;
    };
    if meta.file_type().is_symlink() || is_reparse_point(&meta) {
        summary.skipped += 1;
        return;
    }
    if meta.is_dir() {
        out.push(Entry::Dir {
            name: format!("{name}/"),
            modified: meta.modified().ok(),
        });
        let Ok(read) = fs::read_dir(path) else {
            summary.skipped += 1;
            return;
        };
        let mut children: Vec<_> = read.filter_map(Result::ok).collect();
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
            collect(&child.path(), &child_name, out, summary, cancel);
        }
    } else {
        out.push(Entry::File {
            path: path.to_path_buf(),
            name: name.to_owned(),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    }
}

#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

/// Stores the local modified time (zip format); falls back to the default if conversion fails.
fn with_time(
    options: zip::write::SimpleFileOptions,
    modified: Option<std::time::SystemTime>,
    opts: &CompressOptions<'_>,
) -> zip::write::SimpleFileOptions {
    if let Some([y, mo, d, h, mi, se]) = modified.and_then(|m| (opts.local_time)(m))
        && let (Ok(mo), Ok(d), Ok(h), Ok(mi), Ok(se)) = (
            u8::try_from(mo),
            u8::try_from(d),
            u8::try_from(h),
            u8::try_from(mi),
            u8::try_from(se),
        )
        && let Ok(time) = zip::DateTime::from_date_and_time(y, mo, d, h, mi, se)
    {
        options.last_modified_time(time)
    } else {
        options
    }
}

fn write_zip(
    file: File,
    entries: &[Entry],
    total: u64,
    summary: &mut CompressSummary,
    opts: &CompressOptions<'_>,
) -> Result<(), ArchiveError> {
    use zip::write::SimpleFileOptions;
    let zip_err = |source| ArchiveError::Zip { source };
    let write_err = |source| ArchiveError::WriteZip { source };
    let mut zip = zip::ZipWriter::new(BufWriter::new(file));
    let base = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut done = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    for entry in entries {
        if opts.cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            return Ok(());
        }
        match entry {
            Entry::Dir { name, modified } => {
                zip.add_directory(name.as_str(), with_time(base, *modified, opts))
                    .map_err(zip_err)?;
                summary.dirs += 1;
            }
            Entry::File {
                path,
                name,
                size,
                modified,
            } => {
                let Ok(mut src) = File::open(path) else {
                    summary.skipped += 1;
                    continue;
                };
                let options = with_time(
                    base.large_file(*size >= u64::from(u32::MAX)),
                    *modified,
                    opts,
                );
                zip.start_file(name.as_str(), options).map_err(zip_err)?;
                loop {
                    if opts.cancel.load(Ordering::Relaxed) {
                        summary.cancelled = true;
                        return Ok(());
                    }
                    let n = src
                        .read(&mut buf)
                        .map_err(|source| ArchiveError::ReadSource {
                            path: path.clone(),
                            source,
                        })?;
                    if n == 0 {
                        break;
                    }
                    zip.write_all(&buf[..n]).map_err(write_err)?;
                    done += n as u64;
                    (opts.progress)(ExtractProgress {
                        done,
                        total,
                        files: summary.files,
                    });
                }
                summary.files += 1;
            }
        }
    }
    let mut out = zip.finish().map_err(zip_err)?;
    out.flush().map_err(write_err)?;
    Ok(())
}

/// One item in an archive (for previews).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveItem {
    /// Path in the archive (`/`-separated, no trailing `/` for folders).
    pub path: String,
    /// Uncompressed size in bytes (0 for folders).
    pub size: u64,
    /// Folder entry.
    pub is_dir: bool,
}

/// Archive contents listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveListing {
    /// First `max` items in archive order.
    pub items: Vec<ArchiveItem>,
    /// Files counted (all of them only if `complete`).
    pub files: usize,
    /// Folders counted (all of them only if `complete`).
    pub dirs: usize,
    /// Total uncompressed size.
    pub size: u64,
    /// Whether the whole archive was read (tar needs a full read for totals; we stop when there are
    /// too many).
    pub complete: bool,
}

impl ArchiveListing {
    /// Counts `item` and keeps it if fewer than `max` are kept.
    fn push(&mut self, item: ArchiveItem, max: usize) {
        if item.is_dir {
            self.dirs += 1;
        } else {
            self.files += 1;
            self.size = self.size.saturating_add(item.size);
        }
        if self.items.len() < max {
            self.items.push(item);
        }
    }

    /// Whether streaming listers should stop (the totals are then incomplete).
    const fn is_full(&self, max: usize) -> bool {
        self.files + self.dirs >= max
    }
}

/// A raw entry name for display: UTF-8 or the legacy code page, `/`-separated, no trailing `/`.
fn display_name(raw: &[u8], decode_legacy: &dyn Fn(&[u8]) -> String) -> String {
    let name = match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => decode_legacy(raw),
    };
    name.replace('\\', "/").trim_end_matches('/').to_owned()
}

/// Lists an archive file of any kind without extracting it; see [`list`].
pub fn list_path(
    kind: ArchiveKind,
    path: &Path,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    crate::fs::debug_assert_not_ui_thread("list_path", path);
    if kind.needs_path() {
        return system::list(kind, path, max, decode_legacy);
    }
    let file = File::open(path).map_err(|source| ArchiveError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    list(kind, BufReader::new(file), max, decode_legacy)
}

/// Lists archive contents without extracting. zip and 7z read only their directory; tar
/// stops at item `max`. Kinds that [need a path](ArchiveKind::needs_path) fail with
/// [`ArchiveError::NeedsFile`]; use [`list_path`] for those.
pub fn list(
    kind: ArchiveKind,
    reader: impl Read + io::Seek + 'static,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    let name = |raw: &[u8]| display_name(raw, decode_legacy);
    let mut listing = ArchiveListing::default();
    let push = |listing: &mut ArchiveListing, item: ArchiveItem| listing.push(item, max);
    match kind {
        ArchiveKind::Zip => {
            let mut zip =
                zip::ZipArchive::new(reader).map_err(|source| ArchiveError::NotZip { source })?;
            for i in 0..zip.len() {
                let entry = zip
                    .by_index_raw(i)
                    .map_err(|source| ArchiveError::CorruptZip { source })?;
                let item = ArchiveItem {
                    path: name(entry.name_raw()),
                    size: entry.size(),
                    is_dir: entry.is_dir(),
                };
                push(&mut listing, item);
            }
            listing.complete = true;
        }
        ArchiveKind::Tar => return list_tar(reader, max, decode_legacy),
        ArchiveKind::TarGz => {
            return list_tar(flate2::read::GzDecoder::new(reader), max, decode_legacy);
        }
        ArchiveKind::SevenZip => return sevenz::list(reader, max, decode_legacy),
        ArchiveKind::TarXz | ArchiveKind::TarBz2 | ArchiveKind::TarZst | ArchiveKind::Rar => {
            return Err(ArchiveError::NeedsFile {
                format: kind.label(),
            });
        }
    }
    Ok(listing)
}

/// Lists a tar stream, stopping at item `max`.
fn list_tar(
    reader: impl Read,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|source| ArchiveError::NotTar { source })?;
    let mut listing = ArchiveListing {
        complete: true,
        ..ArchiveListing::default()
    };
    for entry in entries {
        if listing.is_full(max) {
            listing.complete = false;
            break;
        }
        let entry = entry.map_err(|source| ArchiveError::CorruptTar { source })?;
        let is_dir = entry.header().entry_type() == tar::EntryType::Directory;
        let item = ArchiveItem {
            path: display_name(&entry.path_bytes(), decode_legacy),
            size: entry.header().size().unwrap_or(0),
            is_dir,
        };
        listing.push(item, max);
    }
    Ok(listing)
}

/// Turns a name inside an archive into a safe relative path; `None` if unsafe.
pub fn safe_relative_path(raw: &[u8], decode_legacy: &dyn Fn(&[u8]) -> String) -> Option<PathBuf> {
    let name = match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => decode_legacy(raw),
    };
    if name.contains('\0') {
        return None;
    }
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in unified.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            // A drive letter or alternate data stream (`C:`, `a:stream`) would escape the
            // destination on Windows.
            p if p.len() >= 2 && p.as_bytes()[1] == b':' && out.as_os_str().is_empty() => {
                return None;
            }
            p => out.push(sanitize_component(p)),
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    // Final check: plain relative components only.
    out.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then_some(out)
}

fn sanitize_component(part: &str) -> String {
    let mut s: String = part
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.is_empty() {
        s.push('_');
    }
    if crate::path::validate_file_name(&s).is_err() {
        // Only reserved names (CON, NUL, COM1...) are left.
        s.insert(0, '_');
    }
    s
}

fn cancelled(opts: &ExtractOptions<'_>) -> bool {
    opts.cancel.load(Ordering::Relaxed)
}

/// Writes a file, skipping it if the destination exists. Returns true if written.
fn write_new_file(
    path: &Path,
    mut reader: impl Read,
    on_bytes: &mut dyn FnMut(u64),
    opts: &ExtractOptions<'_>,
) -> Result<bool, ArchiveError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArchiveError::Create {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // create_new: atomic create-if-absent, never overwrites the user's file.
    let file = match File::options().write(true).create_new(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(source) => {
            return Err(ArchiveError::Write {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut out = BufWriter::new(file);
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancelled(opts) {
            break;
        }
        let n = reader.read(&mut buf).map_err(|source| ArchiveError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|source| ArchiveError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        on_bytes(n as u64);
    }
    out.flush().map_err(|source| ArchiveError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(true)
}

fn extract_zip(
    file: File,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|source| ArchiveError::NotZip { source })?;
    let mut summary = ExtractSummary::default();
    let mut progress = ExtractProgress::default();
    for i in 0..zip.len() {
        if let Ok(entry) = zip.by_index_raw(i) {
            progress.total = progress.total.saturating_add(entry.size());
        }
    }
    (opts.progress)(progress);
    for i in 0..zip.len() {
        if cancelled(opts) {
            summary.cancelled = true;
            return Ok(summary);
        }
        let mut entry = zip
            .by_index(i)
            .map_err(|source| ArchiveError::CorruptZip { source })?;
        let Some(rel) = safe_relative_path(entry.name_raw(), opts.decode_legacy) else {
            summary.skipped_unsafe += 1;
            continue;
        };
        if entry.is_symlink() {
            summary.skipped_unsafe += 1;
            continue;
        }
        let target = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|source| ArchiveError::Create {
                path: target.clone(),
                source,
            })?;
            summary.dirs += 1;
            continue;
        }
        let mut on_bytes = |n: u64| {
            progress.done = progress.done.saturating_add(n);
            (opts.progress)(progress);
        };
        if write_new_file(&target, &mut entry, &mut on_bytes, opts)? {
            summary.files += 1;
            progress.files = summary.files;
        } else {
            summary.skipped_existing += 1;
        }
    }
    summary.cancelled = cancelled(opts);
    Ok(summary)
}

fn extract_tar(
    reader: CountingReader<File>,
    total: u64,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    let counter = reader.read_counter();
    extract_tar_with_counter(
        reader,
        &|| counter.load(Ordering::Relaxed),
        total,
        dest,
        opts,
    )
}

fn extract_tar_with_counter(
    reader: impl Read,
    // Compressed position so far, the progress unit for tar streams.
    read: &dyn Fn() -> u64,
    total: u64,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let mut summary = ExtractSummary::default();
    let mut progress = ExtractProgress {
        total,
        ..ExtractProgress::default()
    };
    (opts.progress)(progress);
    let entries = archive
        .entries()
        .map_err(|source| ArchiveError::NotTar { source })?;
    for entry in entries {
        if cancelled(opts) {
            summary.cancelled = true;
            return Ok(summary);
        }
        let mut entry = entry.map_err(|source| ArchiveError::CorruptTar { source })?;
        let Some(rel) = safe_relative_path(&entry.path_bytes(), opts.decode_legacy) else {
            summary.skipped_unsafe += 1;
            continue;
        };
        let target = dest.join(&rel);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target).map_err(|source| ArchiveError::Create {
                    path: target.clone(),
                    source,
                })?;
                summary.dirs += 1;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut on_bytes = |_: u64| {
                    progress.done = read();
                    (opts.progress)(progress);
                };
                if write_new_file(&target, &mut entry, &mut on_bytes, opts)? {
                    summary.files += 1;
                    progress.files = summary.files;
                } else {
                    summary.skipped_existing += 1;
                }
            }
            // pax / GNU long names are handled by the tar crate; links and device files are
            // skipped.
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader => {}
            _ => summary.skipped_unsafe += 1,
        }
    }
    summary.cancelled = cancelled(opts);
    Ok(summary)
}

/// Counts bytes read (tar.gz uses the compressed read position as overall progress).
struct CountingReader<R> {
    inner: R,
    read: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            read: std::sync::Arc::default(),
        }
    }

    fn read_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.read.clone()
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use zip::write::SimpleFileOptions;

    /// Tests run only in their own folder under %TEMP%, deleted afterwards.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("arke-archive-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            if self.0.starts_with(std::env::temp_dir()) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    /// Fake "Big5" for tests: maps 0xA4 0xA4 to a single CJK char.
    fn fake_big5(bytes: &[u8]) -> String {
        if bytes == [0xA4, 0xA4, b'.', b't', b'x', b't'] {
            "中.txt".into()
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }

    fn opts<'a>(
        cancel: &'a AtomicBool,
        progress: &'a (dyn Fn(ExtractProgress) + Sync),
    ) -> ExtractOptions<'a> {
        ExtractOptions {
            cancel,
            progress,
            decode_legacy: &fake_big5,
        }
    }

    fn write_zip(path: &Path, entries: &[(&[u8], &[u8])]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, body) in entries {
            if name.ends_with(b"/") {
                zip.add_directory(String::from_utf8_lossy(name), SimpleFileOptions::default())
                    .unwrap();
            } else {
                // Raw bytes as the name (an old zip without the UTF-8 flag).
                zip.start_file_from_path(unsafe_name(name), SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(body).unwrap();
            }
        }
        zip.finish().unwrap();
    }

    fn unsafe_name(name: &[u8]) -> PathBuf {
        // The zip writer only takes strings; the non-UTF-8 case is tested separately in
        // safe_relative_path.
        PathBuf::from(String::from_utf8_lossy(name).into_owned())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn fixed_local_time(_: std::time::SystemTime) -> Option<[u16; 6]> {
        Some([2026, 11, 10, 9, 30, 0])
    }

    #[test]
    fn compresses_folders_and_round_trips() {
        let sb = Sandbox::new("zip-make");
        let src = sb.0.join("project");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir_all(src.join("empty")).unwrap();
        fs::write(src.join("a.txt"), "aaa").unwrap();
        // Non-ASCII name: must survive the round trip (zip UTF-8 name flag).
        fs::write(src.join("sub").join("中文.md"), "# Title").unwrap();
        fs::write(sb.0.join("x.txt"), "xxx").unwrap();
        let dest = sb.0.join("project.zip");
        let cancel = AtomicBool::new(false);
        let progress = |_: ExtractProgress| {};
        let opts = CompressOptions {
            cancel: &cancel,
            progress: &progress,
            local_time: &fixed_local_time,
        };
        let summary = compress(&[src.clone(), sb.0.join("x.txt")], &dest, &opts).unwrap();
        assert_eq!(
            (summary.files, summary.dirs, summary.cancelled),
            (3, 3, false)
        );
        assert!(!sb.0.join("project.zip.partial").exists());
        // No overwrite.
        assert!(matches!(
            compress(&[sb.0.join("x.txt")], &dest, &opts),
            Err(ArchiveError::Exists { path }) if path == dest
        ));

        let mut names: Vec<String> = zip::ZipArchive::new(File::open(&dest).unwrap())
            .unwrap()
            .file_names()
            .map(|n| n.unwrap().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "project/",
                "project/a.txt",
                "project/empty/",
                "project/sub/",
                "project/sub/中文.md",
                "x.txt"
            ]
        );
        let out = sb.0.join("out");
        let noop = |_: ExtractProgress| {};
        extract(&dest, &out, ArchiveKind::Zip, &self::opts(&cancel, &noop)).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("project").join("sub").join("中文.md")).unwrap(),
            "# Title"
        );
        assert!(out.join("project").join("empty").is_dir());

        // Cancel: leaves no files.
        cancel.store(true, Ordering::Relaxed);
        let dest2 = sb.0.join("cancelled.zip");
        let summary = compress(&[src], &dest2, &opts).unwrap();
        assert!(summary.cancelled);
        assert!(!dest2.exists() && !sb.0.join("cancelled.zip.partial").exists());
    }

    #[test]
    fn lists_zip_without_extracting() {
        let sb = Sandbox::new("zip-list");
        let zip = sb.0.join("a.zip");
        write_zip(
            &zip,
            &[
                (b"dir/", b""),
                (b"dir/report.txt", b"hello"),
                (b"b.bin", b"xy"),
            ],
        );
        let listing = list(ArchiveKind::Zip, File::open(&zip).unwrap(), 2, &fake_big5).unwrap();
        assert!(listing.complete);
        assert_eq!((listing.files, listing.dirs, listing.size), (2, 1, 7));
        assert_eq!(
            listing.items,
            [
                ArchiveItem {
                    path: "dir".into(),
                    size: 0,
                    is_dir: true
                },
                ArchiveItem {
                    path: "dir/report.txt".into(),
                    size: 5,
                    is_dir: false
                },
            ]
        );
        assert!(!sb.0.join("dir").exists(), "listing extracts nothing");
    }

    #[test]
    fn extracts_zip_with_nested_dirs_and_chinese_names() {
        // CJK names are the point here: they must extract to the same Unicode paths.
        let sb = Sandbox::new("zip");
        let zip_path = sb.0.join("專案.zip");
        let big = vec![7u8; 300_000];
        write_zip(
            &zip_path,
            &[
                ("資料夾/".as_bytes(), b""),
                ("資料夾/報告.txt".as_bytes(), b"hello"),
                (b"a/b/c.bin", &big),
            ],
        );
        let dest = sb.0.join(folder_name_for("專案.zip"));
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let progress = |p: ExtractProgress| seen.lock().push(p);
        let summary = extract(
            &zip_path,
            &dest,
            ArchiveKind::Zip,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(summary.files, 2);
        assert!(!summary.cancelled);
        assert_eq!(
            fs::read_to_string(dest.join("資料夾/報告.txt")).unwrap(),
            "hello"
        );
        assert_eq!(fs::metadata(dest.join("a/b/c.bin")).unwrap().len(), 300_000);
        let last = *seen.lock().last().unwrap();
        assert_eq!(last.done, last.total);
        assert_eq!(last.total, 300_005);
    }

    #[test]
    fn never_overwrites_and_rejects_path_traversal() {
        let sb = Sandbox::new("safe");
        let zip_path = sb.0.join("evil.zip");
        // ZipWriter refuses `..`, so test the path check directly and "no overwrite" separately.
        write_zip(
            &zip_path,
            &[(b"keep.txt", b"from zip"), (b"new.txt", b"new")],
        );
        let dest = sb.0.join("dest");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("keep.txt"), "user data").unwrap();
        let cancel = AtomicBool::new(false);
        let progress = |_: ExtractProgress| {};
        let summary = extract(
            &zip_path,
            &dest,
            ArchiveKind::Zip,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(summary.skipped_existing, 1);
        assert_eq!(summary.files, 1);
        assert_eq!(
            fs::read_to_string(dest.join("keep.txt")).unwrap(),
            "user data"
        );

        let none = |_: &[u8]| String::new();
        for bad in [
            &b"../evil.txt"[..],
            b"a/../../evil.txt",
            b"/etc/passwd",
            b"\\windows\\system32\\x.dll",
            b"C:/Windows/x.dll",
            b"C:x.dll",
            b"a\0b",
            b"",
            b"./",
        ] {
            assert_eq!(
                safe_relative_path(bad, &none),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(
            safe_relative_path(b"a\\b/./c.txt", &none),
            Some(PathBuf::from("a").join("b").join("c.txt"))
        );
        assert_eq!(
            safe_relative_path(b"dir/CON.txt", &none),
            Some(PathBuf::from("dir").join("_CON.txt"))
        );
        assert_eq!(
            safe_relative_path(b"what?.txt", &none),
            Some(PathBuf::from("what_.txt"))
        );
        // Non-UTF-8 legacy names go through the decoder.
        assert_eq!(
            safe_relative_path(&[0xA4, 0xA4, b'.', b't', b'x', b't'], &fake_big5),
            Some(PathBuf::from("中.txt"))
        );
    }

    #[test]
    fn extracts_tar_gz_and_can_cancel() {
        let sb = Sandbox::new("tgz");
        let tgz = sb.0.join("backup.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                File::create(&tgz).unwrap(),
                flate2::Compression::fast(),
            );
            let mut builder = tar::Builder::new(gz);
            for i in 0..20 {
                let body = vec![b'x'; 10_000];
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("docs/file_{i}.txt"), &body[..])
                    .unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(archive_kind("backup.TAR.GZ"), Some(ArchiveKind::TarGz));
        assert_eq!(folder_name_for("backup.tar.gz"), "backup");
        // Preview: only the first few items; tar is not read further.
        let listing = list(ArchiveKind::TarGz, File::open(&tgz).unwrap(), 5, &fake_big5).unwrap();
        assert_eq!(listing.items.len(), 5);
        assert!(!listing.complete);
        assert_eq!(listing.items[0].path, "docs/file_0.txt");
        assert_eq!(listing.items[0].size, 10_000);

        let cancel = AtomicBool::new(false);
        let progress = |_: ExtractProgress| {};
        let dest = sb.0.join("all");
        let summary = extract(&tgz, &dest, ArchiveKind::TarGz, &opts(&cancel, &progress)).unwrap();
        assert_eq!(summary.files, 20);
        assert!(dest.join("docs").join("file_7.txt").exists());

        // Cancel at the third file.
        let cancel = AtomicBool::new(false);
        let progress = |p: ExtractProgress| {
            if p.files >= 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        };
        let dest = sb.0.join("partial");
        let summary = extract(&tgz, &dest, ArchiveKind::TarGz, &opts(&cancel, &progress)).unwrap();
        assert!(summary.cancelled);
        assert!(summary.files < 20, "{summary:?}");
    }

    #[test]
    fn garbage_is_a_clear_error() {
        let sb = Sandbox::new("bad");
        let path = sb.0.join("bad.zip");
        fs::write(&path, b"definitely not a zip").unwrap();
        let cancel = AtomicBool::new(false);
        let progress = |_: ExtractProgress| {};
        let err = extract(
            &path,
            &sb.0.join("out"),
            ArchiveKind::Zip,
            &opts(&cancel, &progress),
        )
        .unwrap_err();
        assert!(matches!(err, ArchiveError::NotZip { .. }), "{err:?}");
        assert!(err.to_string().contains("zip"), "{err}");
    }

    // ───────────────────────── 7z, RAR and compressed tars ─────────────────────────

    /// Names outside every ANSI code page (Korean, Simplified Chinese, emoji next to Traditional
    /// Chinese): they must come out exact, which tar.exe can't do.
    const CJK_DIR: &str = "資料夾";
    const CJK_FILE: &str = "資料夾/報告.txt";
    const WIDE_FILE: &str = "한국어_简体_😀.txt";

    #[test]
    fn detects_new_formats_and_rar_volumes() {
        for (name, kind) in [
            ("a.7z", ArchiveKind::SevenZip),
            ("A.RAR", ArchiveKind::Rar),
            ("a.tar.xz", ArchiveKind::TarXz),
            ("a.TXZ", ArchiveKind::TarXz),
            ("a.tar.bz2", ArchiveKind::TarBz2),
            ("a.tbz", ArchiveKind::TarBz2),
            ("a.tar.zst", ArchiveKind::TarZst),
            ("a.tzst", ArchiveKind::TarZst),
            ("movie.part1.rar", ArchiveKind::Rar),
            ("movie.PART001.rar", ArchiveKind::Rar),
            ("a.party.rar", ArchiveKind::Rar),
        ] {
            assert_eq!(archive_kind(name), Some(kind), "{name}");
        }
        // Later volumes can't be opened on their own.
        assert_eq!(archive_kind("movie.part2.rar"), None);
        assert_eq!(archive_kind("movie.part02.RAR"), None);
        assert_eq!(folder_name_for("movie.part01.rar"), "movie");
        assert_eq!(folder_name_for("a.party.rar"), "a.party");
        assert_eq!(folder_name_for("資料.7z"), "資料");
        assert_eq!(folder_name_for("x.tar.zst"), "x");
        assert!(ArchiveKind::Rar.needs_path() && ArchiveKind::TarXz.needs_path());
        assert!(!ArchiveKind::SevenZip.needs_path() && !ArchiveKind::TarGz.needs_path());
        assert!(matches!(
            list(ArchiveKind::Rar, io::Cursor::new(Vec::new()), 1, &fake_big5),
            Err(ArchiveError::NeedsFile { format: "RAR" })
        ));

        let sb = Sandbox::new("volumes");
        for name in [
            "set.part01.rar",
            "set.part02.rar",
            "set.part03.rar",
            "set.part05.rar",
        ] {
            fs::write(sb.0.join(name), b"").unwrap();
        }
        assert_eq!(
            rar_volumes(&sb.0.join("set.part01.rar")),
            ["set.part01.rar", "set.part02.rar", "set.part03.rar"].map(|n| sb.0.join(n))
        );
        for name in ["old.rar", "old.r00", "old.r01"] {
            fs::write(sb.0.join(name), b"").unwrap();
        }
        assert_eq!(
            rar_volumes(&sb.0.join("old.rar")),
            ["old.rar", "old.r00", "old.r01"].map(|n| sb.0.join(n))
        );
        assert_eq!(
            rar_volumes(&sb.0.join("single.rar")),
            [sb.0.join("single.rar")]
        );
    }

    /// Writes a 7z: `solid` entries go into one solid block, after the separate ones.
    fn write_7z(path: &Path, separate: Vec<(sevenz_rust2::ArchiveEntry, Vec<u8>)>, solid: usize) {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};
        let mut writer = ArchiveWriter::create(path).unwrap();
        for (entry, body) in separate {
            writer.push_archive_entry(entry, Some(&body[..])).unwrap();
        }
        if solid > 0 {
            let bodies: Vec<Vec<u8>> = (0..solid)
                .map(|i| vec![b'a' + (i % 26) as u8; 10_000])
                .collect();
            let entries = (0..solid)
                .map(|i| ArchiveEntry::new_file(&format!("solid/檔案_{i}.txt")))
                .collect();
            let readers = bodies.iter().map(|b| SourceReader::new(&b[..])).collect();
            writer.push_archive_entries(entries, readers).unwrap();
        }
        writer.finish().unwrap();
    }

    fn sample_7z(path: &Path) {
        use sevenz_rust2::ArchiveEntry;
        let mut link = ArchiveEntry::new_file("link");
        link.has_windows_attributes = true;
        link.windows_attributes = 0x400;
        write_7z(
            path,
            vec![
                (ArchiveEntry::new_directory(CJK_DIR), Vec::new()),
                (ArchiveEntry::new_file(CJK_FILE), b"hello".to_vec()),
                (ArchiveEntry::new_file(WIDE_FILE), b"wide".to_vec()),
                (ArchiveEntry::new_file("../evil.txt"), b"evil".to_vec()),
                (link, b"C:\\Windows".to_vec()),
            ],
            4,
        );
    }

    #[test]
    fn lists_and_extracts_7z_with_cjk_names() {
        let sb = Sandbox::new("7z");
        let path = sb.0.join("專案.7z");
        sample_7z(&path);

        let listing = list_path(ArchiveKind::SevenZip, &path, 100, &fake_big5).unwrap();
        assert!(listing.complete);
        assert_eq!((listing.files, listing.dirs), (8, 1));
        let names: Vec<&str> = listing.items.iter().map(|i| i.path.as_str()).collect();
        assert!(
            names.contains(&CJK_FILE) && names.contains(&WIDE_FILE),
            "{names:?}"
        );
        assert!(names.contains(&"solid/檔案_3.txt"), "{names:?}");
        let short = list(
            ArchiveKind::SevenZip,
            File::open(&path).unwrap(),
            2,
            &fake_big5,
        )
        .unwrap();
        assert_eq!((short.items.len(), short.files + short.dirs), (2, 9));

        let dest = sb.0.join(folder_name_for("專案.7z"));
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let progress = |p: ExtractProgress| seen.lock().push(p);
        let summary = extract(
            &path,
            &dest,
            ArchiveKind::SevenZip,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(
            (summary.files, summary.dirs, summary.skipped_unsafe),
            (6, 1, 2),
            "{summary:?}"
        );
        assert_eq!(fs::read_to_string(dest.join(CJK_FILE)).unwrap(), "hello");
        assert_eq!(fs::read_to_string(dest.join(WIDE_FILE)).unwrap(), "wide");
        assert_eq!(
            fs::read(dest.join("solid").join("檔案_2.txt")).unwrap(),
            vec![b'c'; 10_000]
        );
        assert!(!dest.join("link").exists() && !sb.0.join("evil.txt").exists());
        let last = *seen.lock().last().unwrap();
        assert_eq!((last.done, last.total), (40_023, 40_023));

        // Again into the same folder: nothing is overwritten, even inside the solid block.
        fs::write(dest.join("solid").join("檔案_1.txt"), "user data").unwrap();
        let summary = extract(
            &path,
            &dest,
            ArchiveKind::SevenZip,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!((summary.files, summary.skipped_existing), (0, 6));
        assert_eq!(
            fs::read_to_string(dest.join("solid").join("檔案_1.txt")).unwrap(),
            "user data"
        );
    }

    #[test]
    fn cancels_7z_and_reports_garbage() {
        let sb = Sandbox::new("7z-cancel");
        let path = sb.0.join("many.7z");
        write_7z(&path, Vec::new(), 20);
        let cancel = AtomicBool::new(false);
        let progress = |p: ExtractProgress| {
            if p.files >= 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        };
        let summary = extract(
            &path,
            &sb.0.join("out"),
            ArchiveKind::SevenZip,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert!(summary.cancelled);
        assert!(summary.files >= 3 && summary.files < 20, "{summary:?}");

        let bad = sb.0.join("bad.7z");
        fs::write(&bad, b"not a 7z at all").unwrap();
        let err = list_path(ArchiveKind::SevenZip, &bad, 10, &fake_big5).unwrap_err();
        assert!(matches!(err, ArchiveError::NotSevenZip { .. }), "{err:?}");
        assert!(err.to_string().contains("7z"), "{err}");
    }

    /// `System32\tar.exe`, or `None` (the test is then skipped).
    #[cfg(windows)]
    fn system_tar() -> Option<PathBuf> {
        bsdtar::tar_exe()
    }

    #[cfg(windows)]
    fn run_tar(args: &[&std::ffi::OsStr]) -> bool {
        system_tar().is_some_and(|tar| {
            std::process::Command::new(tar)
                .args(args)
                .output()
                .is_ok_and(|o| o.status.success())
        })
    }

    #[cfg(windows)]
    #[test]
    fn reads_7z_made_by_windows_tar() {
        // libarchive's 7z writer (LZMA by default) and our reader must agree. ASCII names only:
        // tar.exe reads names from disk in the ANSI code page, which varies between machines.
        let sb = Sandbox::new("7z-bsdtar");
        let src = sb.0.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub").join("a.txt"), "from bsdtar").unwrap();
        let path = sb.0.join("made.7z");
        let made = run_tar(&[
            "--format".as_ref(),
            "7zip".as_ref(),
            "-cf".as_ref(),
            path.as_os_str(),
            "-C".as_ref(),
            src.as_os_str(),
            "sub".as_ref(),
        ]);
        if !made {
            return; // No tar.exe, or one without a 7z writer (Windows 10).
        }
        let cancel = AtomicBool::new(false);
        let progress = |_: ExtractProgress| {};
        let dest = sb.0.join("out");
        let summary = extract(
            &path,
            &dest,
            ArchiveKind::SevenZip,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(summary.files, 1);
        assert_eq!(
            fs::read_to_string(dest.join("sub").join("a.txt")).unwrap(),
            "from bsdtar"
        );
    }

    /// One pax record (`LEN key=value\n`, where LEN counts the whole record).
    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let body = format!(" {key}={value}\n");
        let mut len = body.len() + 1;
        loop {
            let record = format!("{len}{body}");
            if record.len() == len {
                return record.into_bytes();
            }
            len = record.len();
        }
    }

    enum Item<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8]),
        Symlink(&'a str, &'a str),
    }

    /// A pax tar whose names are UTF-8 pax records (what tar.exe and libarchive read as Unicode).
    fn write_pax_tar(path: &Path, items: &[Item<'_>]) {
        let mut builder = tar::Builder::new(File::create(path).unwrap());
        for item in items {
            let (name, kind, body): (&str, tar::EntryType, &[u8]) = match item {
                Item::Dir(name) => (name, tar::EntryType::Directory, b""),
                Item::File(name, body) => (name, tar::EntryType::Regular, body),
                Item::Symlink(name, _) => (name, tar::EntryType::Symlink, b""),
            };
            let mut records = pax_record("path", name);
            if let Item::Symlink(_, target) = item {
                records.extend(pax_record("linkpath", target));
            }
            let mut pax = tar::Header::new_ustar();
            pax.set_entry_type(tar::EntryType::XHeader);
            pax.set_path("PaxHeader").unwrap();
            pax.set_size(records.len() as u64);
            pax.set_mode(0o644);
            pax.set_cksum();
            builder.append(&pax, &records[..]).unwrap();
            let mut header = tar::Header::new_ustar();
            header.set_entry_type(kind);
            // Placeholder; the pax path above is what readers use.
            header.set_path("placeholder").unwrap();
            header.set_size(body.len() as u64);
            header.set_mode(if kind == tar::EntryType::Directory {
                0o755
            } else {
                0o644
            });
            header.set_cksum();
            builder.append(&header, body).unwrap();
        }
        builder.into_inner().unwrap().flush().unwrap();
    }

    fn sample_pax_tar(path: &Path, extra_files: usize) {
        let bodies: Vec<(String, Vec<u8>)> = (0..extra_files)
            .map(|i| (format!("bulk/file_{i}.txt"), vec![b'x'; 10_000]))
            .collect();
        let mut items = vec![
            Item::Dir(CJK_DIR),
            Item::File(CJK_FILE, b"hello"),
            Item::File(WIDE_FILE, b"wide"),
            Item::File("../evil.txt", b"evil"),
            Item::Symlink("link", "C:/Windows"),
        ];
        items.extend(bodies.iter().map(|(n, b)| Item::File(n, b)));
        write_pax_tar(path, &items);
    }

    #[cfg(windows)]
    #[test]
    fn system_libarchive_keeps_unicode_names() {
        let Some(api) = libarchive::api() else {
            return; // Before Windows 11 23H2: no archiveint.dll.
        };
        let sb = Sandbox::new("libarchive");
        // Uncompressed on purpose: libarchive always accepts no compression, so the name handling
        // of the DLL is tested without needing an xz writer here (real xz is tested below).
        let path = sb.0.join("名前.tar.xz");
        sample_pax_tar(&path, 3);
        let listing = list_path(ArchiveKind::TarXz, &path, 100, &fake_big5).unwrap();
        let names: Vec<&str> = listing.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            names,
            [
                CJK_DIR,
                CJK_FILE,
                WIDE_FILE,
                "../evil.txt",
                "bulk/file_0.txt",
                "bulk/file_1.txt",
                "bulk/file_2.txt"
            ]
        );
        assert!(listing.complete);

        let dest = sb.0.join("out");
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let progress = |p: ExtractProgress| seen.lock().push(p);
        let summary = libarchive::extract(
            api,
            ArchiveKind::TarXz,
            &path,
            &dest,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(
            (summary.files, summary.dirs, summary.skipped_unsafe),
            (5, 1, 2),
            "{summary:?}"
        );
        assert_eq!(fs::read_to_string(dest.join(CJK_FILE)).unwrap(), "hello");
        assert_eq!(fs::read_to_string(dest.join(WIDE_FILE)).unwrap(), "wide");
        assert!(!dest.join("link").exists() && !sb.0.join("evil.txt").exists());
        let last = *seen.lock().last().unwrap();
        assert_eq!(last.done, last.total);

        fs::write(dest.join(WIDE_FILE), "user data").unwrap();
        let summary = extract(&path, &dest, ArchiveKind::TarXz, &opts(&cancel, &progress)).unwrap();
        assert_eq!((summary.files, summary.skipped_existing), (0, 5));
        assert_eq!(
            fs::read_to_string(dest.join(WIDE_FILE)).unwrap(),
            "user data"
        );

        // Cancel partway.
        let many = sb.0.join("many.tar.xz");
        sample_pax_tar(&many, 20);
        let cancel = AtomicBool::new(false);
        let progress = |p: ExtractProgress| {
            if p.files >= 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        };
        let summary = extract(
            &many,
            &sb.0.join("partial"),
            ArchiveKind::TarXz,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert!(summary.cancelled && summary.files < 22, "{summary:?}");

        // Not a RAR: a clear error naming the format.
        let bad = sb.0.join("bad.rar");
        fs::write(&bad, b"Rar! but not really").unwrap();
        let err = list_path(ArchiveKind::Rar, &bad, 10, &fake_big5).unwrap_err();
        assert!(
            matches!(err, ArchiveError::NotSystemFormat { format: "RAR", .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("RAR"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn system_libarchive_reads_real_xz() {
        let Some(api) = libarchive::api() else {
            return;
        };
        let sb = Sandbox::new("libarchive-xz");
        let src = sb.0.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), "xz data").unwrap();
        let path = sb.0.join("real.tar.xz");
        if !run_tar(&[
            "-cJf".as_ref(),
            path.as_os_str(),
            "-C".as_ref(),
            src.as_os_str(),
            "a.txt".as_ref(),
        ]) {
            return;
        }
        let listing = list_path(ArchiveKind::TarXz, &path, 10, &fake_big5).unwrap();
        assert_eq!(listing.items[0].path, "a.txt");
        let cancel = AtomicBool::new(false);
        let progress = |_: ExtractProgress| {};
        let dest = sb.0.join("out");
        libarchive::extract(
            api,
            ArchiveKind::TarXz,
            &path,
            &dest,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "xz data");
    }

    #[cfg(windows)]
    #[test]
    fn tar_exe_fallback_extracts_safely() {
        let Some(tar) = system_tar() else {
            return;
        };
        let sb = Sandbox::new("bsdtar");
        // ASCII names here: tar.exe goes through the ANSI code page (see below for the rest).
        let path = sb.0.join("名前.tar.xz");
        write_pax_tar(
            &path,
            &[
                Item::Dir("dir"),
                Item::File("dir/a.txt", b"hello"),
                Item::File("../evil.txt", b"evil"),
                Item::Symlink("link", "C:/Windows"),
                Item::File("keep.txt", b"from archive"),
            ],
        );
        let listing = bsdtar::list(&tar, ArchiveKind::TarXz, &path, 100, &fake_big5).unwrap();
        let names: Vec<&str> = listing.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            names,
            ["dir", "dir/a.txt", "../evil.txt", "link", "keep.txt"]
        );

        let dest = sb.0.join("out");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("keep.txt"), "user data").unwrap();
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let progress = |p: ExtractProgress| seen.lock().push(p);
        let summary = bsdtar::extract(
            &tar,
            ArchiveKind::TarXz,
            &path,
            &dest,
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert_eq!(
            (
                summary.files,
                summary.skipped_existing,
                summary.skipped_unsafe
            ),
            (1, 1, 2),
            "{summary:?}"
        );
        assert_eq!(
            fs::read_to_string(dest.join("dir").join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(dest.join("keep.txt")).unwrap(),
            "user data"
        );
        assert!(!dest.join("link").exists() && !sb.0.join("evil.txt").exists());
        let last = *seen.lock().last().unwrap();
        assert!(last.total > 0 && last.done <= last.total, "{last:?}");

        // A name no ANSI code page has: tar.exe crashes (reported as a name problem), unless the
        // system code page is UTF-8, in which case the name comes out exact.
        let wide = sb.0.join("wide.tar");
        write_pax_tar(&wide, &[Item::File(WIDE_FILE, b"wide")]);
        let dest = sb.0.join("wide-out");
        match bsdtar::extract(
            &tar,
            ArchiveKind::Rar,
            &wide,
            &dest,
            &opts(&cancel, &progress),
        ) {
            Ok(summary) => {
                assert_eq!(summary.files, 1);
                assert_eq!(fs::read_to_string(dest.join(WIDE_FILE)).unwrap(), "wide");
            }
            Err(e) => assert!(matches!(e, ArchiveError::TarNames { .. }), "{e:?}"),
        }

        // Cancel partway: tar.exe is killed and the call returns.
        let many = sb.0.join("many.tar");
        sample_pax_tar_ascii(&many, 30);
        let cancel = AtomicBool::new(false);
        let progress = |p: ExtractProgress| {
            if p.files >= 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        };
        let summary = bsdtar::extract(
            &tar,
            ArchiveKind::TarXz,
            &many,
            &sb.0.join("partial"),
            &opts(&cancel, &progress),
        )
        .unwrap();
        assert!(summary.cancelled && summary.files < 30, "{summary:?}");

        // Garbage: tar.exe's complaint becomes a localized "can't read" error.
        let bad = sb.0.join("bad.rar");
        fs::write(&bad, b"not a rar").unwrap();
        let err = bsdtar::list(&tar, ArchiveKind::Rar, &bad, 10, &fake_big5).unwrap_err();
        assert!(
            matches!(err, ArchiveError::SystemUnsupported { format: "RAR", .. }),
            "{err:?}"
        );
    }

    #[cfg(windows)]
    fn sample_pax_tar_ascii(path: &Path, files: usize) {
        let bodies: Vec<(String, Vec<u8>)> = (0..files)
            .map(|i| (format!("bulk/file_{i}.txt"), vec![b'x'; 10_000]))
            .collect();
        let items: Vec<Item<'_>> = bodies.iter().map(|(n, b)| Item::File(n, b)).collect();
        write_pax_tar(path, &items);
    }
}
