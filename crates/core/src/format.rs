//! Display formatting: file sizes, counts and type labels.

use crate::entry::{Entry, EntryKind};
use rust_i18n::t;

/// Use base-1000 sizes (disk vendors) instead of base-1024 (Windows). Process-wide.
static DECIMAL_SIZES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switches the base; returns whether it changed.
pub fn set_decimal_sizes(decimal: bool) -> bool {
    DECIMAL_SIZES.swap(decimal, std::sync::atomic::Ordering::Relaxed) != decimal
}

/// File size with one decimal below 10. Base follows the setting (default 1024).
pub fn format_size(bytes: u64) -> String {
    format_size_base(
        bytes,
        DECIMAL_SIZES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// File size in base 1000 if `decimal`, otherwise 1024.
pub fn format_size_base(bytes: u64, decimal: bool) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let step = if decimal { 1000.0 } else { 1024.0 };
    if (bytes as f64) < step {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= step && unit < UNITS.len() - 1 {
        value /= step;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Integer with thousands separators, for the item count in the status bar.
pub fn format_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Text for the Type column. Deliberately avoids the registry file type description
/// (that loads registry data and shell extensions) and uses a built-in table.
pub fn type_label(entry: &Entry) -> String {
    match entry.kind {
        EntryKind::Dir => return t!("type.folder").into_owned(),
        EntryKind::Link { is_dir: true } => return t!("type.folder_link").into_owned(),
        _ => {}
    }
    let ext = entry.extension();
    if ext.is_empty() {
        return t!("type.file").into_owned();
    }
    if let Some(key) = known_type(ext) {
        return t!(key).into_owned();
    }
    t!("type.ext_file", ext = ext.to_uppercase()).into_owned()
}

/// Translation key for an extension (`type.*` in `locales/core.yml`).
fn known_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "txt" | "log" => "type.text",
        "md" => "type.markdown",
        "exe" => "type.exe",
        "msi" => "type.msi",
        "dll" => "type.dll",
        "lnk" => "type.lnk",
        "url" => "type.url",
        "bat" | "cmd" => "type.batch",
        "ps1" => "type.ps1",
        "zip" => "type.zip",
        "7z" => "type.7z",
        "rar" => "type.rar",
        "jpg" | "jpeg" => "type.jpeg",
        "png" => "type.png",
        "gif" => "type.gif",
        "webp" => "type.webp",
        "svg" => "type.svg",
        "ico" => "type.ico",
        "mp3" => "type.mp3",
        "wav" => "type.wav",
        "flac" => "type.flac",
        "mp4" => "type.mp4",
        "mkv" => "type.mkv",
        "mov" => "type.mov",
        "pdf" => "type.pdf",
        "doc" | "docx" => "type.word",
        "xls" | "xlsx" => "type.excel",
        "ppt" | "pptx" => "type.powerpoint",
        "csv" => "type.csv",
        "json" => "type.json",
        "xml" => "type.xml",
        "html" | "htm" => "type.html",
        "rs" => "type.rust",
        "py" => "type.python",
        "js" => "type.javascript",
        "ts" => "type.typescript",
        "toml" => "type.toml",
        "yaml" | "yml" => "type.yaml",
        "iso" => "type.iso",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        // Base 1000: the same byte count gives a larger number.
        assert_eq!(format_size_base(999, true), "999 B");
        assert_eq!(format_size_base(1000, true), "1.0 KB");
        assert_eq!(format_size_base(1536, true), "1.5 KB");
        assert_eq!(format_size_base(50 * 1024 * 1024, true), "52 MB");
        assert_eq!(format_size_base(1536, false), "1.5 KB");
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(50 * 1024 * 1024), "50 MB");
    }

    #[test]
    fn counts() {
        assert_eq!(format_count(5), "5");
        assert_eq!(format_count(50000), "50,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }
}
