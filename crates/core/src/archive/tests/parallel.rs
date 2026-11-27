//! Zip extraction on several threads (`zip_parallel`): the same tree, summary and conflicts as
//! entry by entry, and Cancel leaves no part of a file behind.
//!
//! Every file written costs a create (and a virus scan) on a disk the whole test run shares, so
//! the zips are only as big as the point needs: enough files for four threads
//! (`FILES_PER_WORKER` each and more), and files bigger than one copy buffer where Cancel must
//! land inside them.

use super::*;

/// A zip of `files` (name, body) and `dirs` (folder entries), in that order: folders first.
fn make_zip(path: &Path, dirs: &[String], files: &[(String, Vec<u8>)]) {
    make_zip_with(path, dirs, files, SimpleFileOptions::default());
}

/// [`make_zip`] with the files written with `options`.
fn make_zip_with(
    path: &Path,
    dirs: &[String],
    files: &[(String, Vec<u8>)],
    options: SimpleFileOptions,
) {
    let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
    for dir in dirs {
        zip.add_directory(dir.as_str(), SimpleFileOptions::default())
            .unwrap();
    }
    for (name, body) in files {
        zip.start_file(name.as_str(), options).unwrap();
        zip.write_all(body).unwrap();
    }
    zip.finish().unwrap();
}

/// Everything under `root`: relative path (with `/`) and the file's bytes (`None` for a folder),
/// sorted.
fn tree(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Option<Vec<u8>>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                out.push((rel, None));
                walk(root, &path, out);
            } else {
                out.push((rel, Some(fs::read(&path).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn extract_on(zip: &Path, dest: &Path, workers: usize) -> Result<ExtractSummary, ArchiveError> {
    let cancel = AtomicBool::new(false);
    let options = ExtractOptions {
        workers,
        ..opts(&cancel, &|_| {})
    };
    extract(zip, dest, ArchiveKind::Zip, &options)
}

/// Small files in nested folders, some with folder entries and some without.
fn many_small_files() -> (Vec<String>, Vec<(String, Vec<u8>)>) {
    let dirs = (0..6).map(|d| format!("dir{d}/")).collect();
    let files = (0..96)
        .map(|i| {
            let name = format!("dir{}/sub{}/deeper{}/file{i}.txt", i % 6, i % 5, i % 3);
            let body = format!("file {i} ").repeat(1 + i % 40).into_bytes();
            (name, body)
        })
        .chain(std::iter::once(("top.txt".to_owned(), b"top".to_vec())))
        .chain(std::iter::once(("empty.txt".to_owned(), Vec::new())))
        .collect();
    (dirs, files)
}

#[test]
fn many_small_files_extract_to_the_same_tree_on_several_threads() {
    let sb = Sandbox::new("unzip-parallel");
    let zip = sb.0.join("many.zip");
    let (dirs, files) = many_small_files();
    make_zip(&zip, &dirs, &files);

    let one = sb.0.join("one");
    let several = sb.0.join("several");
    let in_order = extract_on(&zip, &one, 1).unwrap();
    let progress = Mutex::new(Vec::new());
    let cancel = AtomicBool::new(false);
    let record = |p: ExtractProgress| progress.lock().push(p);
    let options = ExtractOptions {
        workers: 4,
        ..opts(&cancel, &record)
    };
    let parallel = extract(&zip, &several, ArchiveKind::Zip, &options).unwrap();

    assert_eq!(parallel, in_order);
    assert_eq!(parallel.files, 98);
    assert_eq!(parallel.dirs, 6);
    assert!(!parallel.cancelled);
    // Both trees hold exactly the zip's files, with their bytes (and the folders they need).
    let mut expected: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    for (name, body) in &files {
        expected.push((name.clone(), Some(body.clone())));
        // The names use `/`, which `Path` splits on and keeps.
        for dir in Path::new(name)
            .ancestors()
            .skip(1)
            .filter(|d| !d.as_os_str().is_empty())
        {
            expected.push((dir.to_string_lossy().into_owned(), None));
        }
    }
    expected.sort();
    expected.dedup();
    let got = tree(&several);
    assert_eq!(got, expected);
    assert_eq!(tree(&one), got);
    // Progress never goes back and ends at the total.
    let seen = std::mem::take(&mut *progress.lock());
    assert!(
        seen.windows(2)
            .all(|w| w[0].done <= w[1].done && w[0].files <= w[1].files)
    );
    let last = *seen.last().unwrap();
    assert_eq!(last.done, last.total);
    let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    assert_eq!(last.total, total);
}

#[test]
fn cancelling_on_several_threads_leaves_only_whole_files() {
    let sb = Sandbox::new("unzip-parallel-cancel");
    let zip = sb.0.join("big.zip");
    // Each file takes several 256 KB buffers, so Cancel lands inside files. Stored, so making
    // the zip costs no compressing.
    let files: Vec<(String, Vec<u8>)> = (0..32u32)
        .map(|i| {
            let body: Vec<u8> = (0..600_000u32)
                .map(|n| (n.wrapping_mul(31).wrapping_add(i) % 251) as u8)
                .collect();
            (format!("part{}/file{i}.bin", i % 4), body)
        })
        .collect();
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    make_zip_with(&zip, &[], &files, stored);

    let cancel = AtomicBool::new(false);
    let progress = |p: ExtractProgress| {
        if p.done > 0 {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let options = ExtractOptions {
        workers: 4,
        ..opts(&cancel, &progress)
    };
    let dest = sb.0.join("out");
    let summary = extract(&zip, &dest, ArchiveKind::Zip, &options).unwrap();
    assert!(summary.cancelled);
    assert!(summary.files < 32, "{summary:?}");
    assert_eq!(summary.skipped_existing, 0, "not counted as already there");
    // What is there is whole; what isn't whole isn't there.
    let left: Vec<_> = tree(&dest)
        .into_iter()
        .filter_map(|(name, body)| Some((name, body?)))
        .collect();
    assert_eq!(left.len() as u64, summary.files);
    for (name, body) in left {
        let (_, expected) = files.iter().find(|(n, _)| *n == name).unwrap();
        assert_eq!(&body, expected, "{name} is whole");
    }
}

#[test]
fn conflicts_come_out_as_entry_by_entry() {
    let sb = Sandbox::new("unzip-parallel-clash");
    let mut files: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| (format!("f{i}.txt"), format!("zip {i}").into_bytes()))
        .collect();
    // Two names Windows sees as one: the first entry wins, the second is skipped.
    files.push(("Data/Read.txt".to_owned(), b"first".to_vec()));
    files.push(("data/READ.TXT".to_owned(), b"second".to_vec()));
    let clash = sb.0.join("clash.zip");
    make_zip(&clash, &[], &files);
    let one = sb.0.join("one");
    let several = sb.0.join("several");
    let in_order = extract_on(&clash, &one, 1).unwrap();
    assert_eq!(extract_on(&clash, &several, 4).unwrap(), in_order);
    assert_eq!(tree(&several), tree(&one));
    // Windows compares names without case.
    if cfg!(windows) {
        assert_eq!(in_order.skipped_existing, 1);
        assert_eq!(
            fs::read_to_string(several.join("Data").join("Read.txt")).unwrap(),
            "first"
        );
    }

    // No clash inside the zip, so it is split; a file already in the destination still stays.
    files.truncate(40);
    let clean = sb.0.join("clean.zip");
    make_zip(&clean, &[], &files);
    for (dest, workers) in [(sb.0.join("kept-one"), 1), (sb.0.join("kept-several"), 4)] {
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("f7.txt"), "user data").unwrap();
        let summary = extract_on(&clean, &dest, workers).unwrap();
        assert_eq!(summary.skipped_existing, 1);
        assert_eq!(summary.files, 39);
        assert_eq!(
            fs::read_to_string(dest.join("f7.txt")).unwrap(),
            "user data"
        );
        assert_eq!(fs::read_to_string(dest.join("f8.txt")).unwrap(), "zip 8");
    }

    // A file where a later entry needs a folder fails the same way, after the same files.
    files.insert(0, ("x".to_owned(), b"a file".to_vec()));
    files.push(("x/inside.txt".to_owned(), b"needs x as a folder".to_vec()));
    let blocked = sb.0.join("blocked.zip");
    make_zip(&blocked, &[], &files);
    let (one, several) = (sb.0.join("blocked-one"), sb.0.join("blocked-several"));
    assert!(matches!(
        extract_on(&blocked, &one, 1),
        Err(ArchiveError::Create { .. })
    ));
    assert!(matches!(
        extract_on(&blocked, &several, 4),
        Err(ArchiveError::Create { .. })
    ));
    assert_eq!(tree(&several), tree(&one));
    assert_eq!(fs::read_to_string(several.join("x")).unwrap(), "a file");
}
