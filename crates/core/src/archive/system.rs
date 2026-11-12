//! Formats read by Windows' own libarchive: RAR and the xz / bzip2 / zstd tars.
//!
//! Two ways in, best first:
//! 1. `archiveint.dll` (Windows 11 23H2 and later), loaded in-process ([`super::libarchive`]).
//!    Names come back as UTF-16, so every name survives.
//! 2. `tar.exe` (Windows 10 1803 and later, Server 2019+; [`super::bsdtar`]). Its front end
//!    converts names to the ANSI code page: names that don't fit are skipped by `-x` and crash
//!    re-encoding, and older builds lack RAR5, xz and zstd. Used only when the DLL is missing.
//!
//! Either way the entries go through the same checks as zip and tar (safe paths, no overwrite,
//! links skipped).

use super::{ArchiveError, ArchiveKind, ArchiveListing, ExtractOptions, ExtractSummary};
use std::path::Path;

/// Lists an archive with Windows' libarchive.
pub(super) fn list(
    kind: ArchiveKind,
    path: &Path,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    #[cfg(windows)]
    {
        if let Some(api) = super::libarchive::api() {
            return super::libarchive::list(api, kind, path, max, decode_legacy);
        }
        if let Some(tar) = super::bsdtar::tar_exe() {
            return super::bsdtar::list(&tar, kind, path, max, decode_legacy);
        }
    }
    #[cfg(not(windows))]
    let _ = (path, max, decode_legacy);
    Err(ArchiveError::SystemReaderMissing {
        format: kind.label(),
    })
}

/// Extracts an archive with Windows' libarchive.
pub(super) fn extract(
    kind: ArchiveKind,
    archive: &Path,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    #[cfg(windows)]
    {
        if let Some(api) = super::libarchive::api() {
            return super::libarchive::extract(api, kind, archive, dest, opts);
        }
        if let Some(tar) = super::bsdtar::tar_exe() {
            return super::bsdtar::extract(&tar, kind, archive, dest, opts);
        }
    }
    #[cfg(not(windows))]
    let _ = (archive, dest, opts);
    Err(ArchiveError::SystemReaderMissing {
        format: kind.label(),
    })
}

/// `%SystemRoot%\System32`, from the API rather than the environment, so a changed `SystemRoot`
/// or `PATH` can't point us at another `tar.exe` or DLL (Git for Windows puts GNU tar on `PATH`).
#[cfg(windows)]
pub(super) fn system_dir() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = vec![0u16; 512];
    // SAFETY: the buffer is valid for its whole length, which the wrapper passes as the size.
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    Some(std::ffi::OsString::from_wide(&buf[..len]).into())
}
