//! Generates lots of test files in the system temp directory.
//!
//! Usage:
//!   genfiles [count=50000]          create them in %TEMP%\arke-bench-<count>
//!   genfiles <count> --clean        delete a previously generated directory (only if marked)
//!   genfiles <count> --images       create real 1600x1200 JPEGs in
//!                                   %TEMP%\arke-bench-images-<count>, for thumbnail timing
//!
//! Safety: only creates or deletes under `std::env::temp_dir()`, and deletes only directories
//! that carry this tool's marker file.

// CLI tool: printing to the console and aborting on errors is intended.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)]

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

const MARKER: &str = ".arke-genfiles-marker";

const EXTS: &[&str] = &[
    "txt", "jpg", "png", "pdf", "docx", "xlsx", "rs", "json", "mp4", "zip", "log", "csv", "exe",
    "md",
];
/// Name parts. The CJK words are deliberate: they mix non-ASCII names into the stress data.
const WORDS: &[&str] = &[
    "report",
    "photo",
    "invoice",
    "backup",
    "draft",
    "notes",
    "data",
    "meeting-minutes",
    "design",
    "專案",
    "報告",
    "final",
    "v2",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let count: usize = args.iter().find_map(|a| a.parse().ok()).unwrap_or(50_000);
    let clean = args.iter().any(|a| a == "--clean");
    let images = args.iter().any(|a| a == "--images");

    let root = std::env::temp_dir();
    let dir = if images {
        root.join(format!("arke-bench-images-{count}"))
    } else {
        root.join(format!("arke-bench-{count}"))
    };
    ensure_under_temp(&root, &dir);

    if clean {
        if !dir.join(MARKER).exists() {
            eprintln!("{} has no marker file, refusing to delete", dir.display());
            std::process::exit(1);
        }
        fs::remove_dir_all(&dir).expect("delete failed");
        println!("deleted {}", dir.display());
        return;
    }

    if dir.exists() {
        if !dir.join(MARKER).exists() {
            eprintln!(
                "{} exists but was not created by this tool, stopping",
                dir.display()
            );
            std::process::exit(1);
        }
        println!("already exists: {}", dir.display());
        return;
    }

    let start = Instant::now();
    fs::create_dir_all(&dir).expect("create directory failed");
    File::create(dir.join(MARKER)).expect("create marker file failed");
    if images {
        write_images(&dir, count);
        println!(
            "generated {count} images in {}, took {:.1}s",
            dir.display(),
            start.elapsed().as_secs_f64()
        );
        return;
    }

    // About 1% are folders.
    let dirs = (count / 100).max(1);
    for i in 0..dirs {
        let name = format!("{} folder {i:04}", WORDS[i % WORDS.len()]);
        fs::create_dir(dir.join(name)).expect("create subfolder failed");
    }
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    for i in 0..count.saturating_sub(dirs) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let word = WORDS[(seed % WORDS.len() as u64) as usize];
        let ext = EXTS[((seed >> 8) % EXTS.len() as u64) as usize];
        let name = format!("{word}_{i:06}.{ext}");
        let mut f = File::create(dir.join(&name)).expect("create file failed");
        // Write only 0-255 bytes of real content. Don't fake big files with set_len: NTFS
        // allocates the space (they aren't sparse), and 50k files would fill the disk.
        let size = (seed % 256) as usize;
        f.write_all(&vec![b'x'; size]).expect("write failed");
    }
    println!(
        "generated {count} items in {}, took {:.1}s",
        dir.display(),
        start.elapsed().as_secs_f64()
    );
}

fn ensure_under_temp(root: &Path, dir: &Path) {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let parent = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    let parent = parent.canonicalize().unwrap_or(parent);
    if parent != root {
        eprintln!(
            "safety check failed: {} is not under the temp directory",
            dir.display()
        );
        std::process::exit(2);
    }
}

/// 1600x1200 JPEGs, each a different color (gradient plus stripes, about 100 KB compressed, so
/// decoding costs about the same as a typical photo).
fn write_images(dir: &Path, count: usize) {
    for i in 0..count {
        let hue = (i * 37 % 256) as u8;
        let img = image::RgbImage::from_fn(1600, 1200, |x, y| {
            let stripe = if (x / 40 + y / 40) % 2 == 0 { 30 } else { 0 };
            image::Rgb([
                hue.wrapping_add((x / 7) as u8),
                ((y / 5) as u8).wrapping_add(stripe),
                hue.wrapping_mul(3).wrapping_add(((x + y) / 11) as u8),
            ])
        });
        let file = File::create(dir.join(format!("photo_{i:05}.jpg"))).expect("create file failed");
        let mut writer = std::io::BufWriter::new(file);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 85)
            .encode_image(&img)
            .expect("write JPEG failed");
        writer.flush().expect("write failed");
    }
}
