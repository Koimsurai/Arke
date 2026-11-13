//! "Verify after copy": reads every copied file back and compares it with its source.
//!
//! Compares the bytes directly rather than hashing both sides: each file is read once either way,
//! and a direct compare can't miss a difference. Folders are walked recursively; symbolic links
//! and junctions are skipped (the copy engine copies the link, not what it points to).

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Read size per step; also how often cancel is checked.
const CHUNK: usize = 1 << 20;

/// Why a copied item doesn't match its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The copy is not there.
    Missing,
    /// Same name, different size.
    Size,
    /// Same size, different bytes.
    Content,
    /// One side could not be read (reason).
    Unreadable(String),
}

/// One item that failed verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// Path relative to the copied item's parent (`Photos\2024\a.jpg`).
    pub path: PathBuf,
    /// What is wrong with it.
    pub problem: Problem,
}

/// Compares `copy` with `source` (a file or a folder tree). `on_bytes` is told how many bytes were
/// compared after each step. Stops early when `cancel` is set. Returns the items that differ.
pub fn compare(
    source: &Path,
    copy: &Path,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    let name = source.file_name().map_or_default(PathBuf::from);
    walk(source, copy, &name, cancel, on_bytes, &mut out);
    out
}

fn walk(
    source: &Path,
    copy: &Path,
    rel: &Path,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
    out: &mut Vec<Mismatch>,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let mismatch = |problem| Mismatch {
        path: rel.to_path_buf(),
        problem,
    };
    let meta = match std::fs::symlink_metadata(source) {
        Ok(meta) => meta,
        Err(e) => return out.push(mismatch(Problem::Unreadable(e.to_string()))),
    };
    if meta.file_type().is_symlink() || is_reparse_dir(&meta) {
        return;
    }
    if meta.is_dir() {
        let entries = match std::fs::read_dir(source) {
            Ok(entries) => entries,
            Err(e) => return out.push(mismatch(Problem::Unreadable(e.to_string()))),
        };
        if !copy.is_dir() {
            return out.push(mismatch(Problem::Missing));
        }
        for entry in entries.flatten() {
            let name = entry.file_name();
            walk(
                &entry.path(),
                &copy.join(&name),
                &rel.join(&name),
                cancel,
                on_bytes,
                out,
            );
        }
        return;
    }
    if let Some(problem) = compare_files(source, copy, meta.len(), cancel, on_bytes) {
        out.push(mismatch(problem));
    }
}

#[cfg(windows)]
fn is_reparse_dir(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_reparse_dir(_: &std::fs::Metadata) -> bool {
    false
}

fn compare_files(
    source: &Path,
    copy: &Path,
    len: u64,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Option<Problem> {
    let unreadable = |e: io::Error| Some(Problem::Unreadable(e.to_string()));
    let copy_meta = match std::fs::metadata(copy) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Some(Problem::Missing),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(Problem::Missing),
        Err(e) => return unreadable(e),
    };
    if copy_meta.len() != len {
        return Some(Problem::Size);
    }
    let (mut a, mut b) = match (File::open(source), File::open(copy)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return unreadable(e),
    };
    let mut left = vec![0u8; CHUNK];
    let mut right = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let n = match fill(&mut a, &mut left) {
            Ok(n) => n,
            Err(e) => return unreadable(e),
        };
        let m = match fill(&mut b, &mut right) {
            Ok(m) => m,
            Err(e) => return unreadable(e),
        };
        if n != m || left[..n] != right[..m] {
            return Some(Problem::Content);
        }
        if n == 0 {
            return None;
        }
        on_bytes(n as u64);
    }
}

/// Reads until `buf` is full or the file ends; returns how much was read.
fn fill(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("arke-verify-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, body: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn check(source: &Path, copy: &Path) -> (Vec<Mismatch>, u64) {
        let mut bytes = 0;
        let found = compare(source, copy, &AtomicBool::new(false), &mut |n| bytes += n);
        (found, bytes)
    }

    #[test]
    fn identical_trees_pass_and_count_their_bytes() {
        let t = Temp::new("same");
        t.write(r"src\a.txt", b"hello");
        t.write(r"src\sub\b.bin", &[7u8; 3000]);
        t.write(r"dst\src\a.txt", b"hello");
        t.write(r"dst\src\sub\b.bin", &[7u8; 3000]);
        let (found, bytes) = check(&t.0.join("src"), &t.0.join(r"dst\src"));
        assert_eq!(found, []);
        assert_eq!(bytes, 3005);
    }

    #[test]
    fn differences_missing_files_and_sizes_are_reported_by_relative_path() {
        let t = Temp::new("diff");
        t.write(r"src\same.txt", b"abc");
        t.write(r"src\changed.txt", b"abc");
        t.write(r"src\short.txt", b"abcdef");
        t.write(r"src\deep\gone.txt", b"x");
        t.write(r"dst\src\same.txt", b"abc");
        t.write(r"dst\src\changed.txt", b"abx");
        t.write(r"dst\src\short.txt", b"abc");
        std::fs::create_dir_all(t.0.join(r"dst\src\deep")).unwrap();
        let (mut found, _) = check(&t.0.join("src"), &t.0.join(r"dst\src"));
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let summary: Vec<_> = found
            .iter()
            .map(|m| (m.path.to_string_lossy().into_owned(), m.problem.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (r"src\changed.txt".to_owned(), Problem::Content),
                (r"src\deep\gone.txt".to_owned(), Problem::Missing),
                (r"src\short.txt".to_owned(), Problem::Size),
            ]
        );
    }

    #[test]
    fn single_files_compare_too() {
        let t = Temp::new("file");
        let a = t.write("a.txt", b"one");
        let b = t.write("b.txt", b"one");
        let c = t.write("c.txt", b"two");
        assert_eq!(check(&a, &b).0, []);
        assert_eq!(check(&a, &c).0[0].problem, Problem::Content);
        assert_eq!(
            check(&a, &t.0.join("none.txt")).0[0].problem,
            Problem::Missing
        );
    }
}
