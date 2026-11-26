//! `arke [folder] --bench list [--bench-out file]`: list and sort a folder of 100,000 items,
//! headless, and report the timings and the peak memory. It is the CI performance gate's
//! benchmark (`.github/workflows/perf.yml`); no window or GPU is needed.
//!
//! - With a folder, that folder is used and filled up to [`ITEMS`] items when it has fewer (so a
//!   CI job generates it once and every run reuses it). It is never deleted.
//! - Without one, a fresh folder under `%TEMP%` is used and deleted afterwards.
//! - The listing and the sort are the ones the app uses: [`RealFs::read_dir`] and
//!   [`SortSpec::sort`] (Name, ascending, folders first). Settings, logs and the user's folders
//!   are not touched.
//! - Output is `key: value` lines, to the launching terminal like `--bench index`, and appended
//!   to `--bench-out` when given.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use arke_core::fs::{Fs as _, RealFs};
use arke_core::sort::SortSpec;

use crate::bench_index::{console, peak_working_set};

/// Items in the benchmark folder.
const ITEMS: usize = 100_000;
/// One folder in this many items.
const FOLDER_EVERY: usize = 10;
/// Threads creating the files.
const WRITERS: usize = 4;

/// Runs the benchmark and returns the process exit code.
pub(crate) fn run(folder: Option<&Path>, out: Option<&Path>) -> ExitCode {
    console::attach();
    match measure(folder) {
        Ok(report) => {
            let text = format!(
                "== arke bench list {}\n{}",
                crate::build_info::VERSION,
                report.body()
            );
            console::stdout(&text);
            if let Some(out) = out
                && let Err(err) = append(out, &format!("{text}\n"))
            {
                console::stderr(&format!("error: cannot write {}: {err}\n", out.display()));
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            console::stderr(&format!("error: {err}\n"));
            ExitCode::FAILURE
        }
    }
}

fn append(path: &Path, text: &str) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
}

/// What one run measured.
#[derive(Debug, Clone, Copy)]
struct Report {
    items: usize,
    enumerate: Duration,
    sort: Duration,
    /// Highest working set of the process, in bytes; `None` when Windows didn't say.
    peak_working_set: Option<u64>,
}

impl Report {
    fn body(&self) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let mut s = String::new();
        let _ = writeln!(s, "items: {}", self.items);
        let _ = writeln!(s, "list_enumerate_ms: {:.1}", ms(self.enumerate));
        let _ = writeln!(s, "list_sort_ms: {:.1}", ms(self.sort));
        let _ = writeln!(s, "list_total_ms: {:.1}", ms(self.enumerate + self.sort));
        let _ = writeln!(
            s,
            "peak_working_set_bytes: {}",
            self.peak_working_set
                .map_or(-1, |b| i64::try_from(b).unwrap_or(-1))
        );
        s
    }
}

fn measure(folder: Option<&Path>) -> io::Result<Report> {
    let temp = folder
        .is_none()
        .then(|| std::env::temp_dir().join(format!("arke-bench-list-{}", std::process::id())));
    let dir = folder
        .map(Path::to_path_buf)
        .or_else(|| temp.clone())
        .unwrap_or_default();
    let result = fill(&dir, ITEMS).and_then(|()| time_listing(&dir));
    if let Some(temp) = temp {
        let _ = std::fs::remove_dir_all(temp);
    }
    result
}

/// Lists and sorts `dir` once.
fn time_listing(dir: &Path) -> io::Result<Report> {
    let started = Instant::now();
    let mut entries = RealFs.read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    let enumerate = started.elapsed();
    let started = Instant::now();
    SortSpec::default().sort(&mut entries);
    let sort = started.elapsed();
    Ok(Report {
        items: entries.len(),
        enumerate,
        sort,
        peak_working_set: peak_working_set(),
    })
}

/// Name of item `i`: mixed case, digits and a few shapes, so the name sort has real work to do.
fn item_name(i: usize) -> String {
    let n = i.wrapping_mul(2_654_435_761) % 1_000_000;
    match i % 4 {
        0 => format!("Report {n} final.txt"),
        1 => format!("img_{n:06}.JPG"),
        2 => format!("notes-{n}.md"),
        _ => format!("Data{n}"),
    }
}

/// Creates items until `dir` has `want`, one in [`FOLDER_EVERY`] a folder, the rest empty files.
fn fill(dir: &Path, want: usize) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let have = std::fs::read_dir(dir)?.count();
    if have >= want {
        return Ok(());
    }
    let missing = want - have;
    std::thread::scope(|scope| {
        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                scope.spawn(move || -> io::Result<()> {
                    for i in (w..missing).step_by(WRITERS) {
                        // The count already there is part of the name, so a top-up never
                        // collides with what is in the folder.
                        let path: PathBuf = dir.join(format!("{}.{have}-{i}", item_name(i)));
                        if i % FOLDER_EVERY == 0 {
                            std::fs::create_dir(&path)?;
                        } else {
                            std::fs::File::create(&path)?;
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        writers.into_iter().try_for_each(|w| {
            w.join()
                .unwrap_or_else(|_| Err(io::Error::other("a file writer panicked")))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_mixed_and_distinct_enough() {
        let names: std::collections::HashSet<_> = (0..1000).map(item_name).collect();
        assert!(names.len() > 900);
        let has = |ext: &str| {
            names
                .iter()
                .any(|n| Path::new(n).extension().is_some_and(|e| e == ext))
        };
        assert!(has("JPG"));
        assert!(has("md"));
    }

    #[test]
    fn fill_tops_up_and_a_second_call_adds_nothing() {
        let dir = std::env::temp_dir().join(format!("arke-bench-list-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fill(&dir, 25).expect("fill");
        assert_eq!(std::fs::read_dir(&dir).expect("read").count(), 25);
        fill(&dir, 25).expect("fill again");
        assert_eq!(std::fs::read_dir(&dir).expect("read").count(), 25);
        let report = time_listing(&dir).expect("listing");
        assert_eq!(report.items, 25);
        assert!(report.body().contains("items: 25\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
