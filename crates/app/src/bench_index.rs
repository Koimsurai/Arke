//! `arke --bench index [--bench-out file]`: build the whole-disk search index once, from scratch,
//! and report how long it took and how much memory it used.
//!
//! - Same volumes, build rules, thread count and I/O priority as the app with default settings
//!   (see `global_search::local_index_options`), and the same search service when it's installed.
//!   Only resource checks are skipped: the app defers a rebuild in battery saver, which would make
//!   the benchmark wait forever.
//! - The index is written to a fresh folder under `%TEMP%` that is deleted afterwards. The user's
//!   real index, settings and logs (`%APPDATA%\Arke`, `%LOCALAPPDATA%\Arke`) are never touched.
//! - Headless: no window, tray or single-instance pipe, so a running Arke is left alone. Logging
//!   is off unless `RUST_LOG` is set, then it goes to stderr.
//! - Release builds are GUI programs without a console, so the report goes to the console of the
//!   terminal that launched it (`AttachConsole`), and to `--bench-out` (appended) when given. cmd
//!   and PowerShell don't wait for GUI programs: the prompt comes back at once and the report
//!   prints when the build is done (`start /wait arke --bench index` waits).
//! - Ctrl+C ends the process; a temp folder left behind that way is removed by the next run.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::RealFs;
use arke_core::format::format_count;
use arke_core::index_service::IndexService;
use arke_core::journal::{Cursor, Journal, JournalCallback, ListSink};
use parking_lot::Mutex;

use crate::settings::Settings;

/// Temp folder name prefix; the process id follows so concurrent runs don't collide.
const TEMP_PREFIX: &str = "arke-bench-index-";

/// Why the benchmark could not run.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BenchError {
    #[error("no local fixed drives to index")]
    NoDrives,
    #[error("cannot create the temporary index folder {}: {source}", path.display())]
    TempDir { path: PathBuf, source: io::Error },
    #[error("cannot write {}: {source}", path.display())]
    Out { path: PathBuf, source: io::Error },
}

/// Runs the benchmark and returns the process exit code.
pub(crate) fn run(out: Option<&Path>) -> ExitCode {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = env_logger::Builder::from_default_env()
            .format_timestamp_millis()
            .try_init();
    }
    let attached = console::attach();
    // The shell already printed its prompt when it launched us without waiting, so start on a new
    // line.
    let lead = if attached { "\n" } else { "" };
    let header = header(crate::build_info::VERSION);
    console::stdout(&format!("{lead}{header}"));
    match measure() {
        Ok(report) => {
            let body = report.body();
            console::stdout(&body);
            match append(out, &format!("{header}{body}\n")) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    console::stderr(&format!("error: {err}\n"));
                    ExitCode::FAILURE
                }
            }
        }
        Err(err) => {
            let line = format!("error: {err}\n");
            console::stderr(&line);
            let _ = append(out, &format!("{header}{line}\n"));
            ExitCode::FAILURE
        }
    }
}

/// Appends to the `--bench-out` file (like the window benchmarks), if one was given.
fn append(out: Option<&Path>, text: &str) -> Result<(), BenchError> {
    let Some(path) = out else {
        return Ok(());
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|source| BenchError::Out {
            path: path.to_path_buf(),
            source,
        })
}

/// Builds the index and collects the numbers.
fn measure() -> Result<Report, BenchError> {
    let base = std::env::temp_dir();
    remove_leftovers(&base);
    let dir = TempDir::create(temp_dir_in(&base, std::process::id()))?;

    // Default settings, not the user's: the point is a number others can reproduce.
    let defaults = Settings::default();
    let roots = crate::global_search::without_skipped(
        crate::global_search::local_drives(&crate::win::WinShell),
        &defaults.search_skipped_drives,
    );
    if roots.is_empty() {
        return Err(BenchError::NoDrives);
    }
    let installed = arke_indexd::client::installed();
    let journal = installed.then(|| Arc::new(RecordingJournal::default()));
    let options = crate::global_search::local_index_options(
        roots.clone(),
        Some(dir.path().join("index.bin")),
        journal.clone().map(|j| j as Arc<dyn Journal>),
        Arc::new(|| true),
        defaults.search_skip_build_dirs,
        defaults.search_excluded,
    );

    let started = Instant::now();
    let (changed_tx, changed_rx) = flume::unbounded::<()>();
    let service = IndexService::start(Arc::new(RealFs), options, move || {
        let _ = changed_tx.send(());
    });
    // Built once the index exists and no walk is running. A fresh folder has no cache to load, so
    // the first index to appear is the one just built (and saved).
    let items = loop {
        let status = service.status();
        if status.scanning.is_none()
            && let Some(count) = status.count
        {
            break count;
        }
        // Status changes notify; the timeout only guards against a missed wakeup.
        let _ = changed_rx.recv_timeout(Duration::from_millis(500));
    };
    let elapsed = started.elapsed();
    // Taken while the service still holds the index, as the app does after a build.
    let memory = process_memory().unwrap_or_default();
    let index_file = index_file_size(dir.path());
    // Unmap the index before the folder is deleted.
    drop(service);

    let volumes = roots
        .iter()
        .map(|root| {
            let source = journal
                .as_ref()
                .map_or(Source::Walk(WalkReason::NoService), |j| j.source_of(root));
            (root.clone(), source)
        })
        .collect();
    Ok(Report {
        volumes,
        items,
        elapsed,
        memory,
        index_file,
    })
}

/// `<base>\arke-bench-index-<pid>`.
fn temp_dir_in(base: &Path, pid: u32) -> PathBuf {
    base.join(format!("{TEMP_PREFIX}{pid}"))
}

/// Process id in a benchmark temp folder name, if it is one.
fn leftover_pid(name: &str) -> Option<u32> {
    name.strip_prefix(TEMP_PREFIX)?.parse().ok()
}

/// Deletes temp folders of earlier runs that ended without cleaning up (Ctrl+C, crash). Folders
/// of runs still going are kept.
fn remove_leftovers(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let own = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(leftover_pid) else {
            continue;
        };
        if pid != own && !process_running(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn process_running(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: only requests wait access; the handle is closed before returning. A zero timeout
    // only polls.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        let _ = CloseHandle(handle);
        running
    }
}

/// Benchmark temp folder, deleted on drop (normal exit and errors alike).
struct TempDir(PathBuf);

impl TempDir {
    fn create(path: PathBuf) -> Result<Self, BenchError> {
        // A previous process with the same id may have left one behind.
        let _ = std::fs::remove_dir_all(&path);
        match std::fs::create_dir_all(&path) {
            Ok(()) => Ok(Self(path)),
            Err(source) => Err(BenchError::TempDir { path, source }),
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Background threads of the index service can hold the mapped file for a moment after
        // the service is dropped, so retry briefly.
        for _ in 0..30 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        log::warn!("could not delete {}", self.0.display());
    }
}

/// Size of the saved index (`index.bin`, or `index.alt.bin` after an alternate save).
fn index_file_size(dir: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(dir).ok()?;
    let total: u64 = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "bin"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    (total > 0).then_some(total)
}

/// Process memory in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Memory {
    /// Highest working set so far (physical memory in use).
    peak_working_set: u64,
    working_set: u64,
    /// Highest private bytes so far (committed memory only this process uses).
    peak_private: u64,
    private: u64,
}

fn process_memory() -> Option<Memory> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let cb = u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb,
        ..Default::default()
    };
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing; counters is a
    // PROCESS_MEMORY_COUNTERS of size `cb`, valid for the call.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, cb) }.ok()?;
    Some(Memory {
        peak_working_set: counters.PeakWorkingSetSize as u64,
        working_set: counters.WorkingSetSize as u64,
        peak_private: counters.PeakPagefileUsage as u64,
        private: counters.PagefileUsage as u64,
    })
}

/// How a volume's items were gathered.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// The search service read the NTFS master file table.
    Mft,
    /// Folders were listed one by one.
    Walk(WalkReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WalkReason {
    /// The search service isn't installed.
    NoService,
    /// The service gave no journal for this volume (not NTFS, or the service didn't answer).
    NoJournal,
    /// The service tried and failed; the error as reported.
    ListFailed(String),
}

/// The search service journal, recording what happened per volume so the report can say where
/// items came from.
#[derive(Default)]
struct RecordingJournal {
    inner: arke_indexd::client::ServiceJournal,
    /// Whether each root got a journal position (last answer per root).
    cursors: Mutex<Vec<(PathBuf, bool)>>,
    /// Result of each root's MFT listing.
    listings: Mutex<Vec<(PathBuf, Result<(), String>)>>,
}

impl RecordingJournal {
    fn source_of(&self, root: &Path) -> Source {
        let listed = self
            .listings
            .lock()
            .iter()
            .find(|(r, _)| r == root)
            .map(|(_, result)| result.clone());
        let journaled = self
            .cursors
            .lock()
            .iter()
            .any(|(r, some)| r == root && *some);
        source_from(journaled, listed)
    }
}

/// Source of a volume given whether it had a journal and how its listing went (service installed).
fn source_from(journaled: bool, listed: Option<Result<(), String>>) -> Source {
    match listed {
        Some(Ok(())) => Source::Mft,
        Some(Err(why)) => Source::Walk(WalkReason::ListFailed(why)),
        None if journaled => Source::Walk(WalkReason::ListFailed("not listed".into())),
        None => Source::Walk(WalkReason::NoJournal),
    }
}

impl Journal for RecordingJournal {
    fn cursor(&self, root: &Path) -> Option<Cursor> {
        let cursor = self.inner.cursor(root);
        let mut cursors = self.cursors.lock();
        cursors.retain(|(r, _)| r != root);
        cursors.push((root.to_path_buf(), cursor.is_some()));
        drop(cursors);
        cursor
    }

    fn follow(
        &self,
        _root: &Path,
        _from: Cursor,
        _on_event: JournalCallback,
    ) -> io::Result<Box<dyn Send>> {
        // The benchmark exits right after the build; following live changes would only open
        // another connection to the service.
        Ok(Box::new(()))
    }

    fn list(&self, root: &Path, sink: ListSink) -> io::Result<()> {
        let result = self.inner.list(root, sink);
        self.listings.lock().push((
            root.to_path_buf(),
            result.as_ref().copied().map_err(ToString::to_string),
        ));
        result
    }
}

/// What the benchmark measured.
struct Report {
    volumes: Vec<(PathBuf, Source)>,
    items: usize,
    /// From starting the service until the index was built, saved and searchable.
    elapsed: Duration,
    memory: Memory,
    /// Bytes on disk; `None` if saving failed.
    index_file: Option<u64>,
}

fn header(version: &str) -> String {
    format!("Arke {version} index benchmark\n")
}

impl Report {
    /// Report lines after the header.
    fn body(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let width = self
            .volumes
            .iter()
            .map(|(root, _)| root.as_os_str().len())
            .max()
            .unwrap_or(0);
        for (root, source) in &self.volumes {
            let _ = writeln!(
                s,
                "  {:<width$}  {}",
                root.display(),
                describe(source),
                width = width
            );
        }
        let _ = writeln!(s, "  items      {}", format_count(self.items));
        let _ = writeln!(s, "  time       {:.1} s", self.elapsed.as_secs_f64());
        let m = &self.memory;
        let _ = writeln!(
            s,
            "  memory     peak {}, after {} (working set)",
            megabytes(m.peak_working_set),
            megabytes(m.working_set)
        );
        let _ = writeln!(
            s,
            "  private    peak {}, after {}",
            megabytes(m.peak_private),
            megabytes(m.private)
        );
        let _ = writeln!(
            s,
            "  index file {}",
            self.index_file
                .map_or_else(|| "not saved".into(), megabytes)
        );
        if self
            .volumes
            .iter()
            .any(|(_, src)| *src == Source::Walk(WalkReason::NoService))
        {
            let _ = writeln!(
                s,
                "Settings › Search › Live updates installs the search service, which reads NTFS \
                 volumes from the master file table instead of walking them."
            );
        }
        s
    }
}

fn describe(source: &Source) -> String {
    match source {
        Source::Mft => "NTFS master file table (search service)".into(),
        Source::Walk(WalkReason::NoService) => {
            "directory walk (search service not installed)".into()
        }
        Source::Walk(WalkReason::NoJournal) => {
            "directory walk (not NTFS, or the search service didn't answer)".into()
        }
        Source::Walk(WalkReason::ListFailed(why)) => {
            format!("directory walk (master file table read failed: {why})")
        }
    }
}

/// Bytes as whole megabytes (MiB, as Task Manager counts them), rounded.
fn megabytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    format!("{} MB", (bytes + MB / 2) / MB)
}

/// Writing the report to the launching terminal (release builds have no console of their own).
pub(crate) mod console {
    use std::io::{self, Write as _};

    /// Makes stdout/stderr reach the launching terminal. Returns whether it attached to the
    /// parent's console (false when the handles already work: debug build, redirected output).
    pub(crate) fn attach() -> bool {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        if has_stdout() {
            return false;
        }
        // SAFETY: no pointers; fails harmlessly when the parent has no console (Explorer).
        if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
            return false;
        }
        if !has_stdout() {
            // Std handles weren't set up by attaching: point them at the console directly.
            open_conout();
        }
        true
    }

    fn has_stdout() -> bool {
        use windows::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
        // SAFETY: reads this process's standard handle; nothing is passed.
        unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.is_ok_and(|h| !h.0.is_null())
    }

    fn open_conout() {
        use std::os::windows::io::IntoRawHandle as _;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONOUT$")
        else {
            return;
        };
        // Kept open for the rest of the process as its stdout and stderr.
        let handle = HANDLE(file.into_raw_handle());
        // SAFETY: handle is an open console handle that is never closed.
        unsafe {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }

    // Through io::Write rather than print!: the workspace denies print macros, and a missing
    // console must not panic.
    pub(crate) fn stdout(text: &str) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    pub(crate) fn stderr(text: &str) {
        let mut err = io::stderr().lock();
        let _ = err.write_all(text.as_bytes());
        let _ = err.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_whole_megabytes() {
        assert_eq!(megabytes(0), "0 MB");
        assert_eq!(megabytes(171 * 1024 * 1024 + 100), "171 MB");
        assert_eq!(megabytes(1024 * 1024 / 2), "1 MB");
    }

    #[test]
    fn report_lists_volumes_and_numbers() {
        let mb = 1024 * 1024;
        let report = Report {
            volumes: vec![
                (PathBuf::from(r"C:\"), Source::Mft),
                (PathBuf::from(r"D:\"), Source::Walk(WalkReason::NoJournal)),
            ],
            items: 3_992_215,
            elapsed: Duration::from_millis(8_240),
            memory: Memory {
                peak_working_set: 437 * mb,
                working_set: 171 * mb,
                peak_private: 400 * mb,
                private: 120 * mb,
            },
            index_file: Some(159 * mb),
        };
        assert_eq!(
            format!("{}{}", header("0.1.0"), report.body()),
            "Arke 0.1.0 index benchmark\n\
             \x20 C:\\  NTFS master file table (search service)\n\
             \x20 D:\\  directory walk (not NTFS, or the search service didn't answer)\n\
             \x20 items      3,992,215\n\
             \x20 time       8.2 s\n\
             \x20 memory     peak 437 MB, after 171 MB (working set)\n\
             \x20 private    peak 400 MB, after 120 MB\n\
             \x20 index file 159 MB\n"
        );
    }

    #[test]
    fn walking_without_the_service_points_to_the_setting() {
        let report = Report {
            volumes: vec![(PathBuf::from(r"C:\"), Source::Walk(WalkReason::NoService))],
            items: 10,
            elapsed: Duration::from_secs(60),
            memory: Memory::default(),
            index_file: None,
        };
        let body = report.body();
        assert!(body.contains("directory walk (search service not installed)"));
        assert!(body.contains("Settings › Search › Live updates installs the search service"));
        assert!(body.contains("index file not saved"));
    }

    #[test]
    fn sources_follow_what_the_service_did() {
        assert_eq!(source_from(true, Some(Ok(()))), Source::Mft);
        assert_eq!(
            source_from(true, Some(Err("timed out".into()))),
            Source::Walk(WalkReason::ListFailed("timed out".into()))
        );
        assert_eq!(
            source_from(false, None),
            Source::Walk(WalkReason::NoJournal)
        );
    }

    #[test]
    fn temp_folders_are_per_process_and_recognized_later() {
        let base = Path::new(r"C:\Users\me\AppData\Local\Temp");
        let dir = temp_dir_in(base, 4242);
        assert_eq!(dir, base.join("arke-bench-index-4242"));
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap();
        assert_eq!(leftover_pid(name), Some(4242));
        assert_eq!(leftover_pid("arke-bench-index-"), None);
        assert_eq!(leftover_pid("arke-bench-index-x1"), None);
        assert_eq!(leftover_pid("Arke"), None);
    }

    #[test]
    fn temp_folder_is_deleted_on_drop() {
        let base = std::env::temp_dir().join(format!("arke-bench-test-{}", std::process::id()));
        let path = base.join("run");
        {
            let dir = TempDir::create(path.clone()).unwrap();
            std::fs::write(dir.path().join("index.bin"), b"1234").unwrap();
            assert_eq!(index_file_size(dir.path()), Some(4));
        }
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn leftovers_of_finished_runs_are_removed() {
        let base = std::env::temp_dir().join(format!("arke-bench-left-{}", std::process::id()));
        // u32::MAX is never a live process id; our own id is always kept.
        let dead = temp_dir_in(&base, u32::MAX);
        let own = temp_dir_in(&base, std::process::id());
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(&own).unwrap();
        remove_leftovers(&base);
        assert!(!dead.exists());
        assert!(own.exists());
        let _ = std::fs::remove_dir_all(base);
    }
}
