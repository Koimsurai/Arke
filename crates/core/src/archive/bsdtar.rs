//! Fallback when archiveint.dll is missing (Windows 10, Server 2019/2022): `System32\tar.exe`.
//!
//! tar.exe is used only as a decoder: `tar.exe --format pax -cf - @-` reads the archive from stdin
//! and writes every entry back out as a pax tar on stdout, which the tar crate reads with the same
//! checks as a plain `.tar` (safe paths, no overwrite, links skipped). Letting tar.exe extract
//! itself (`-x -k`) was rejected: it can't skip links, and it reports neither skipped existing files
//! nor what it refused except as ANSI text on stderr.
//!
//! Known limits, all from tar.exe's ANSI front end or its age:
//! - stdin is the archive's own file handle: tar.exe reads command-line paths in the ANSI code
//!   page (a path outside it fails to open), and RAR needs a seekable input. pax names on stdout
//!   are UTF-8, so every name that fits the code page comes out exact.
//! - A name that doesn't fit the ANSI code page makes tar.exe crash; that is reported as
//!   [`ArchiveError::TarNames`] (files before it are kept).
//! - Windows 10's tar.exe has no RAR5, xz or zstd: [`ArchiveError::SystemUnsupported`].
//! - Only the first volume of a multi-volume RAR is read.

use super::system::system_dir;
use super::{
    ArchiveError, ArchiveKind, ArchiveListing, ExtractOptions, ExtractSummary, SystemReaderError,
};
use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;

/// No console window flashes up for the child.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// NTSTATUS of an access violation, as an exit code.
const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
/// How much of tar.exe's stderr is kept for the error message.
const STDERR_LIMIT: u64 = 16 * 1024;

/// `System32\tar.exe`, if present. Never `tar` from `PATH`: Git for Windows puts GNU tar there,
/// which has no RAR support and different options.
pub(super) fn tar_exe() -> Option<PathBuf> {
    let path = system_dir()?.join("tar.exe");
    path.is_file().then_some(path)
}

/// The decoding command: `archive` on stdin, pax on stdout.
fn command(tar: &Path, archive: File) -> Command {
    let mut cmd = Command::new(tar);
    cmd.args(["--format", "pax", "-cf", "-", "@-"])
        // bsdtar applies these to every run; a user's setting must not change our stream.
        .env_remove("TAR_READER_OPTIONS")
        .env_remove("TAR_WRITER_OPTIONS")
        // The file itself, not a pipe: libarchive's RAR reader needs to seek, and from a pipe
        // tar.exe silently outputs no entries and exits 0.
        .stdin(Stdio::from(archive))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// A running tar.exe decoding one archive. Dropping it kills the process (cancel, or enough
/// entries listed).
struct Decoder {
    child: Child,
    stdout: ChildStdout,
    /// A duplicate of tar.exe's stdin handle. Duplicates share the file position, so this tells
    /// how far tar.exe has read (progress).
    position: File,
    stderr: Option<JoinHandle<Vec<u8>>>,
    done: bool,
}

impl Decoder {
    fn spawn(tar: &Path, archive: &Path) -> Result<Self, ArchiveError> {
        let open_error = |source| ArchiveError::Open {
            path: archive.to_path_buf(),
            source,
        };
        let file = File::open(archive).map_err(open_error)?;
        let position = file.try_clone().map_err(open_error)?;
        let run_error = |source| ArchiveError::RunTar {
            path: tar.to_path_buf(),
            source,
        };
        let mut child = command(tar, file).spawn().map_err(run_error)?;
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(run_error(io::Error::other("tar.exe pipes")));
        };
        // Drained on its own thread so a chatty tar.exe never blocks on a full stderr pipe.
        let stderr = std::thread::Builder::new()
            .name("tar-stderr".into())
            .spawn(move || {
                let mut kept = Vec::new();
                let _ = (&mut stderr).take(STDERR_LIMIT).read_to_end(&mut kept);
                let _ = io::copy(&mut stderr, &mut io::sink());
                kept
            })
            .ok();
        Ok(Self {
            child,
            stdout,
            position,
            stderr,
            done: false,
        })
    }

    /// Reads the rest of stdout, waits for tar.exe and turns a failure into an error.
    fn finish(
        mut self,
        kind: ArchiveKind,
        decode_legacy: &dyn Fn(&[u8]) -> String,
    ) -> Result<(), ArchiveError> {
        // The pax end-of-archive padding may still be in the pipe; tar.exe can't exit until
        // it's read.
        let _ = io::copy(&mut self.stdout, &mut io::sink());
        let status = self.child.wait();
        self.done = true;
        let stderr = self
            .stderr
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        let message = match std::str::from_utf8(&stderr) {
            Ok(s) => s.trim().to_owned(),
            Err(_) => decode_legacy(&stderr).trim().to_owned(),
        };
        status_error(kind, code, message).map_or(Ok(()), Err)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Ends once the process is gone (the pipe breaks).
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

/// Maps tar.exe's exit code and stderr to an error; `None` on success.
fn status_error(kind: ArchiveKind, code: i32, message: String) -> Option<ArchiveError> {
    if code == 0 {
        return None;
    }
    // Exit codes are NTSTATUS values for crashes; compare the bit pattern.
    let source = SystemReaderError { code, message };
    let lower = source.message.to_ascii_lowercase();
    Some(if code.cast_unsigned() == STATUS_ACCESS_VIOLATION {
        ArchiveError::TarNames { source }
    } else if lower.contains("encrypt") {
        ArchiveError::Encrypted
    } else if lower.contains("unrecognized archive format")
        || lower.contains("not supported")
        || lower.contains("unsupported")
        || lower.contains("can't initialize filter")
    {
        ArchiveError::SystemUnsupported {
            format: kind.label(),
            source,
        }
    } else {
        ArchiveError::TarFailed { source }
    })
}

/// Lists through tar.exe, stopping (and killing it) at item `max`.
pub(super) fn list(
    tar: &Path,
    kind: ArchiveKind,
    path: &Path,
    max: usize,
    decode_legacy: &dyn Fn(&[u8]) -> String,
) -> Result<ArchiveListing, ArchiveError> {
    let mut decoder = Decoder::spawn(tar, path)?;
    match super::list_tar(&mut decoder.stdout, max, decode_legacy) {
        Ok(listing) if !listing.complete => Ok(listing),
        Ok(listing) => decoder.finish(kind, decode_legacy).map(|()| listing),
        // A broken stream usually means tar.exe failed; its message is the useful one.
        Err(e) => Err(decoder.finish(kind, decode_legacy).err().unwrap_or(e)),
    }
}

/// Extracts through tar.exe; progress is how far tar.exe has read the archive.
pub(super) fn extract(
    tar: &Path,
    kind: ArchiveKind,
    archive: &Path,
    dest: &Path,
    opts: &ExtractOptions<'_>,
) -> Result<ExtractSummary, ArchiveError> {
    let total = fs::metadata(archive).map_or(0, |m| m.len());
    let mut decoder = Decoder::spawn(tar, archive)?;
    let Decoder {
        stdout, position, ..
    } = &mut decoder;
    let read = || (&*position).stream_position().unwrap_or(0);
    match super::extract_tar_with_counter(stdout, &read, total, dest, opts) {
        // Dropping the decoder kills tar.exe.
        Ok(summary) if summary.cancelled => Ok(summary),
        Ok(summary) => decoder.finish(kind, opts.decode_legacy).map(|()| summary),
        Err(
            e @ (ArchiveError::NotTar { .. }
            | ArchiveError::CorruptTar { .. }
            | ArchiveError::Read { .. }),
        ) => Err(decoder.finish(kind, opts.decode_legacy).err().unwrap_or(e)),
        // Our side failed (e.g. the destination is full): stop tar.exe instead of draining it.
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_through_system_tar_with_stdin_and_no_window() {
        let tar = PathBuf::from(r"C:\Windows\System32\tar.exe");
        let input = std::env::temp_dir().join(format!("arke-bsdtar-cmd-{}", std::process::id()));
        fs::write(&input, b"").unwrap();
        let cmd = command(&tar, File::open(&input).unwrap());
        assert_eq!(cmd.get_program(), tar.as_os_str());
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // `@-`: the archive comes from stdin, never as an ANSI command-line path; no `-P`.
        assert_eq!(args, ["--format", "pax", "-cf", "-", "@-"]);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(removed.contains(&"TAR_READER_OPTIONS".to_owned()));
        drop(cmd);
        let _ = fs::remove_file(&input);
        if let Some(found) = tar_exe() {
            let found = found.to_string_lossy().to_ascii_lowercase();
            assert!(found.ends_with(r"\system32\tar.exe"), "{found}");
        }
    }

    #[test]
    fn maps_tar_exe_failures() {
        let kind = ArchiveKind::Rar;
        assert!(status_error(kind, 0, String::new()).is_none());
        assert!(matches!(
            status_error(kind, STATUS_ACCESS_VIOLATION.cast_signed(), String::new()),
            Some(ArchiveError::TarNames { .. })
        ));
        assert!(matches!(
            status_error(kind, 1, "tar.exe: Unrecognized archive format".into()),
            Some(ArchiveError::SystemUnsupported { format: "RAR", .. })
        ));
        assert!(matches!(
            status_error(kind, 1, "tar.exe: Encrypted file is unsupported".into()),
            Some(ArchiveError::Encrypted)
        ));
        let other = status_error(kind, 2, "tar.exe: Truncated input file".into());
        assert!(
            matches!(&other, Some(ArchiveError::TarFailed { source }) if source.code == 2),
            "{other:?}"
        );
    }
}
