//! Zip extraction on several threads. Each thread opens the archive again and takes the next
//! entry when it is done with one, so a zip of many small files (where creating each file, and the
//! virus scanner looking at it, costs more than its bytes) is written side by side.
//!
//! The result is the same as extracting entry by entry (`extract_zip`):
//! - Only used when no two entries can land on the same path (the same name, names that differ
//!   only in case, a file where another entry needs a folder, a name that may be another file's
//!   8.3 short name), so which entry wins never depends on timing. Such a zip is extracted in
//!   order, as before.
//! - Existing files are never overwritten; each file makes its folders before it is created, as
//!   before (`create_dir_all` takes another thread making the same folder).
//! - Cancel stops every thread within one buffer and removes the file each was writing; an error
//!   stops the others the same way. Of several errors, the first entry's is reported.
//! - Progress is counted in one place, so it only grows.

use super::{
    ArchiveError, ExtractOptions, ExtractProgress, ExtractSummary, Written, ZipEntryInfo,
    ZipOutcome, cancelled, extract_zip_entry,
};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Fewer files than this per thread: another thread isn't worth starting.
const FILES_PER_WORKER: usize = 8;
/// Most threads whatever the caller allows: past this the drive is the limit, not the CPU.
const MAX_WORKERS: usize = 8;

/// How many threads to extract `entries` on, at most `allowed`: 1 for a small zip, or one where
/// two entries could land on the same path.
pub(super) fn workers(entries: &[ZipEntryInfo], allowed: usize) -> usize {
    let files = entries.iter().filter(|e| !e.dir && e.rel.is_some()).count();
    let workers = allowed.min(MAX_WORKERS).min(files / FILES_PER_WORKER);
    if workers <= 1 || clashes(entries) {
        1
    } else {
        workers
    }
}

/// Whether two entries could land on the same path, compared without case as Windows compares
/// names (`to_uppercase` folds more than Windows does, which only means extracting in order).
/// Folders named twice are fine: making one again changes nothing.
fn clashes(entries: &[ZipEntryInfo]) -> bool {
    let key = |p: &Path| p.to_string_lossy().to_uppercase();
    let mut files = HashSet::new();
    // Every folder some entry is or needs; a folder in it means its parents are too.
    let mut folders = HashSet::new();
    for entry in entries {
        let Some(rel) = &entry.rel else {
            continue;
        };
        if rel
            .iter()
            .any(|part| short_name_like(&part.to_string_lossy()))
        {
            return true;
        }
        let first = if entry.dir {
            Some(rel.as_path())
        } else {
            rel.parent()
        };
        for folder in first
            .into_iter()
            .flat_map(Path::ancestors)
            .filter(|p| !p.as_os_str().is_empty())
        {
            if !folders.insert(key(folder)) {
                break;
            }
        }
        if !entry.dir && !files.insert(key(rel)) {
            return true;
        }
    }
    files.iter().any(|file| folders.contains(file))
}

/// Looks like an 8.3 short name (`REPORT~1.DOC`), which can be another long name's second name
/// on the drive.
fn short_name_like(name: &str) -> bool {
    name.as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
}

/// State shared by the threads of one extraction.
struct Shared {
    /// The next entry to take.
    next: AtomicUsize,
    /// Cancelled, or a thread failed: every thread stops, and its file copy checks this.
    stop: AtomicBool,
    progress: Mutex<ExtractProgress>,
    summary: Mutex<ExtractSummary>,
    /// The error of the first entry that failed, with its index.
    failed: Mutex<Option<(usize, ArchiveError)>>,
}

impl Shared {
    /// Takes entries until none are left or everything stops.
    fn run<R: Read + Seek>(
        &self,
        zip: &mut zip::ZipArchive<R>,
        entries: &[ZipEntryInfo],
        dest: &Path,
        opts: &ExtractOptions<'_>,
        inner: &ExtractOptions<'_>,
    ) {
        let mut summary = ExtractSummary::default();
        let mut on_bytes = |n: u64| {
            if cancelled(opts) {
                self.stop.store(true, Ordering::Relaxed);
            }
            // Reported under the lock, so the caller never sees it go back.
            let mut progress = self.progress.lock();
            progress.done = progress.done.saturating_add(n);
            (opts.progress)(*progress);
        };
        loop {
            if cancelled(opts) {
                self.stop.store(true, Ordering::Relaxed);
            }
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            let Some(info) = entries.get(i) else {
                break;
            };
            match extract_zip_entry(zip, i, info, dest, inner, &mut on_bytes) {
                Ok(outcome) => {
                    outcome.count(&mut summary);
                    if outcome == ZipOutcome::File(Written::Done) {
                        self.progress.lock().files += 1;
                    }
                }
                Err(error) => {
                    self.fail(i, error);
                    break;
                }
            }
        }
        let mut total = self.summary.lock();
        total.files += summary.files;
        total.dirs += summary.dirs;
        total.skipped_existing += summary.skipped_existing;
        total.skipped_unsafe += summary.skipped_unsafe;
    }

    fn fail(&self, i: usize, error: ArchiveError) {
        self.stop.store(true, Ordering::Relaxed);
        let mut failed = self.failed.lock();
        if failed.as_ref().is_none_or(|(first, _)| i < *first) {
            *failed = Some((i, error));
        }
    }
}

/// Extracts `entries` of `zip` (its central directory read, the password checked) under `dest`
/// on `workers` threads, this one included; the others open `archive` again. `progress`: the
/// totals reported so far.
pub(super) fn extract(
    mut zip: zip::ZipArchive<BufReader<File>>,
    archive: &Path,
    entries: &[ZipEntryInfo],
    dest: &Path,
    opts: &ExtractOptions<'_>,
    progress: ExtractProgress,
    workers: usize,
) -> Result<ExtractSummary, ArchiveError> {
    let shared = Shared {
        next: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        progress: Mutex::new(progress),
        summary: Mutex::new(ExtractSummary::default()),
        failed: Mutex::new(None),
    };
    let inner = ExtractOptions {
        cancel: &shared.stop,
        workers: 1,
        ..*opts
    };
    let metadata = zip.metadata();
    let (state, inner, metadata) = (&shared, &inner, &metadata);
    std::thread::scope(|scope| {
        for _ in 1..workers {
            let helper = move || match File::open(archive) {
                Ok(file) => {
                    // SAFETY: the metadata was read from this same file a moment ago. Were it
                    // replaced in between, its entries would read as a damaged zip: the call only
                    // asks the caller to pair them, nothing in memory depends on it.
                    let mut zip = unsafe {
                        zip::ZipArchive::unsafe_new_with_metadata(
                            BufReader::new(file),
                            Arc::clone(metadata),
                        )
                    };
                    state.run(&mut zip, entries, dest, opts, inner);
                }
                // The other threads take its share.
                Err(err) => log::warn!("unzip: can't open the archive again: {err}"),
            };
            let spawned = std::thread::Builder::new()
                .name("unzip".to_owned())
                .spawn_scoped(scope, helper);
            if let Err(err) = spawned {
                log::warn!("unzip: no more threads: {err}");
                break;
            }
        }
        state.run(&mut zip, entries, dest, opts, inner);
    });
    if let Some((_, error)) = shared.failed.into_inner() {
        return Err(error);
    }
    let mut summary = shared.summary.into_inner();
    summary.cancelled = cancelled(opts);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(rel: &str) -> ZipEntryInfo {
        ZipEntryInfo {
            rel: Some(PathBuf::from(rel)),
            ..ZipEntryInfo::default()
        }
    }

    fn dir(rel: &str) -> ZipEntryInfo {
        ZipEntryInfo {
            dir: true,
            ..file(rel)
        }
    }

    #[test]
    fn only_zips_where_no_two_entries_meet_are_split() {
        let clean = [
            dir("a"),
            dir("a"),
            file("a/b/c.txt"),
            file("a/d.txt"),
            file("e"),
        ];
        assert!(!clashes(&clean));
        // The same name twice, or differing only in case.
        assert!(clashes(&[file("a/x.txt"), file("a/X.TXT")]));
        assert!(clashes(&[file("x"), file("x")]));
        // A file where a folder is needed, either way round.
        assert!(clashes(&[file("a"), file("a/b.txt")]));
        assert!(clashes(&[file("a/b/c.txt"), file("A/B")]));
        assert!(clashes(&[dir("a/b"), file("a")]));
        assert!(clashes(&[file("a"), dir("a")]));
        // Could be another file's short name.
        assert!(clashes(&[
            file("REPORT~1.DOC"),
            file("report document.doc")
        ]));
        assert!(!clashes(&[file("~$draft.docx"), file("draft.docx")]));
    }

    #[test]
    fn small_zips_stay_on_one_thread() {
        let many: Vec<ZipEntryInfo> = (0..100).map(|i| file(&format!("f{i}"))).collect();
        assert_eq!(workers(&many, 1), 1);
        assert_eq!(workers(&many, 4), 4);
        assert_eq!(workers(&many, 64), MAX_WORKERS);
        assert_eq!(workers(&many[..20], 4), 2);
        assert_eq!(workers(&many[..15], 4), 1);
        let mut clash = many;
        clash.push(file("F1"));
        assert_eq!(workers(&clash, 4), 1);
    }
}
