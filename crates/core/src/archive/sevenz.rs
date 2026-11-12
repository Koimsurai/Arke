//! 7z, decoded in-process by sevenz-rust2.
//!
//! Chosen over Windows' tar.exe because 7z stores names as UTF-16 and this keeps them exact (tar.exe
//! converts names to the ANSI code page and skips or crashes on the rest), progress is per byte,
//! cancelling needs no child process, and it also works on Windows 10, whose tar.exe has no LZMA.

use super::{
    ArchiveError, ArchiveItem, ArchiveListing, ExtractOptions, ExtractProgress, ExtractSummary,
    cancelled, display_name, safe_relative_path, write_new_file,
};
use sevenz_rust2::{Archive, ArchiveEntry, ArchiveReader, EntryDecision, Password};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek};
use std::path::Path;

/// Windows attribute bit for reparse points (symlinks and junctions archived by 7-Zip).
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// 7-Zip's marker that the high 16 attribute bits hold a Unix mode (archives made on Unix).
const FILE_ATTRIBUTE_UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;

/// Links are stored as small files holding the target; extracting them as files would be wrong
/// and as links unsafe, so they are skipped like zip and tar links.
const fn is_link(entry: &ArchiveEntry) -> bool {
    if !entry.has_windows_attributes {
        return false;
    }
    let attrs = entry.windows_attributes;
    attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (attrs & FILE_ATTRIBUTE_UNIX_EXTENSION != 0 && (attrs >> 16) & S_IFMT == S_IFLNK)
}

/// Maps a sevenz-rust2 error; `corrupt` picks the fallback for errors after the header was read.
fn map_error(source: sevenz_rust2::Error, corrupt: bool) -> ArchiveError {
    use sevenz_rust2::Error as E;
    match source {
        E::PasswordRequired | E::MaybeBadPassword(_) => ArchiveError::Encrypted,
        // Built without AES, so an encrypted archive shows up as an unknown method.
        E::UnsupportedCompressionMethod(method) if method.contains("AES") => {
            ArchiveError::Encrypted
        }
        E::UnsupportedCompressionMethod(method) => ArchiveError::UnsupportedMethod { method },
        source if corrupt => ArchiveError::CorruptSevenZip { source },
        source => ArchiveError::NotSevenZip { source },
    }
}

/// Lists a 7z from its header alone (no data is decoded, so this is fast even for solid archives).
pub(super) fn list(
    mut reader: impl Read + Seek,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    let archive =
        Archive::read(&mut reader, &Password::empty()).map_err(|e| map_error(e, false))?;
    let mut listing = ArchiveListing {
        complete: true,
        ..ArchiveListing::default()
    };
    // Anti-items are deletion markers for update archives, not content.
    for entry in archive.files.iter().filter(|e| !e.is_anti_item) {
        let item = ArchiveItem {
            path: display_name(entry.name.as_bytes(), decode_legacy),
            size: if entry.is_directory { 0 } else { entry.size },
            is_dir: entry.is_directory,
        };
        listing.push(item, max);
    }
    Ok(listing)
}

/// Extracts a 7z with the shared rules (safe paths, never overwrite, links skipped).
pub(super) fn extract(
    file: File,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    let mut reader = ArchiveReader::new(BufReader::new(file), Password::empty())
        .map_err(|e| map_error(e, false))?;
    let archive = reader.archive();
    // Fail before writing anything rather than halfway through.
    if (0..archive.files.len()).any(|i| archive.is_file_encrypted(i)) {
        return Err(ArchiveError::Encrypted);
    }
    let mut progress = ExtractProgress {
        total: archive
            .files
            .iter()
            .filter(|e| !e.is_directory && !e.is_anti_item)
            .map(|e| e.size)
            .sum(),
        ..ExtractProgress::default()
    };
    (opts.progress)(progress);
    let mut summary = ExtractSummary::default();
    // The callback must return the crate's error type; ours is kept here and wins.
    let mut failure = None;
    let result = reader.for_each_entries_with_decision(|entry, data| {
        if cancelled(opts) {
            return Ok(EntryDecision::Stop);
        }
        if entry.is_anti_item {
            return Ok(EntryDecision::Continue);
        }
        let rel = safe_relative_path(entry.name.as_bytes(), opts.decode_legacy);
        let Some(rel) = rel.filter(|_| !is_link(entry)) else {
            summary.skipped_unsafe += 1;
            // Skipped data is still decoded (and the bar must reach the end).
            progress.done = progress.done.saturating_add(entry.size);
            (opts.progress)(progress);
            return Ok(EntryDecision::Continue);
        };
        let target = dest.join(&rel);
        if entry.is_directory {
            if let Err(source) = fs::create_dir_all(&target) {
                failure = Some(ArchiveError::Create {
                    path: target,
                    source,
                });
                return Ok(EntryDecision::Stop);
            }
            summary.dirs += 1;
            return Ok(EntryDecision::Continue);
        }
        let mut on_bytes = |n: u64| {
            progress.done = progress.done.saturating_add(n);
            (opts.progress)(progress);
        };
        match write_new_file(&target, data, &mut on_bytes, opts) {
            Ok(true) => {
                summary.files += 1;
                progress.files = summary.files;
            }
            Ok(false) => {
                summary.skipped_existing += 1;
                // Unread data is still decoded (solid blocks need it), so it counts as progress.
                progress.done = progress.done.saturating_add(entry.size);
                (opts.progress)(progress);
            }
            Err(e) => {
                failure = Some(e);
                return Ok(EntryDecision::Stop);
            }
        }
        Ok(if cancelled(opts) {
            EntryDecision::Stop
        } else {
            EntryDecision::Continue
        })
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| map_error(e, true))?;
    summary.cancelled = cancelled(opts);
    Ok(summary)
}
