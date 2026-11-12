//! Classifies files by extension: which theme icon to use, whether to ask the system
//! for an icon, whether a thumbnail is possible.
//!
//! Third-party shell extensions are never loaded. Icons come from the built-in theme
//! (extension only); only `.exe`, `.lnk` and `.ico`, whose icon is their content, ask the system.

use crate::entry::{Entry, EntryKind};

/// Coarse file category for icons, thumbnails and the type column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// Folder, or link to a folder.
    Folder,
    /// Raster or vector image, including camera RAW and PSD.
    Image,
    /// Video container.
    Video,
    /// Audio file.
    Audio,
    /// Compressed archive (not all are extractable; see [`crate::archive`]).
    Archive,
    /// Source code, markup or config file.
    Code,
    /// Plain or lightly formatted text.
    Text,
    /// PDF document.
    Pdf,
    /// Word processor document.
    Document,
    /// Spreadsheet.
    Spreadsheet,
    /// Slide deck.
    Presentation,
    /// Program, installer or library.
    Executable,
    /// `.lnk` or `.url` shortcut.
    Shortcut,
    /// Font file.
    Font,
    /// Disk or virtual drive image.
    DiskImage,
    /// Unknown extension.
    Other,
}

/// Category of an entry: [`FileKind::Folder`] for folder-like entries, else by extension.
pub fn file_kind(entry: &Entry) -> FileKind {
    if matches!(
        entry.kind,
        EntryKind::Dir | EntryKind::Link { is_dir: true }
    ) {
        return FileKind::Folder;
    }
    kind_for_extension(entry.extension())
}

/// Category for a lowercase extension without the dot.
pub fn kind_for_extension(ext: &str) -> FileKind {
    match ext {
        "png" | "jpg" | "jpeg" | "jfif" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico"
        | "svg" | "heic" | "heif" | "avif" | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" => {
            FileKind::Image
        }
        "mp4" | "mkv" | "mov" | "avi" | "wmv" | "webm" | "flv" | "m4v" | "mpg" | "mpeg" => {
            FileKind::Video
        }
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" => FileKind::Audio,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" => {
            FileKind::Archive
        }
        "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cs" | "java" | "kt" | "go" | "py" | "rb"
        | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "vue" | "svelte" | "html" | "htm"
        | "css" | "scss" | "less" | "json" | "jsonc" | "toml" | "yaml" | "yml" | "xml" | "sql"
        | "sh" | "bash" | "ps1" | "psm1" | "bat" | "cmd" | "lua" | "php" | "swift" | "dart"
        | "zig" | "proto" | "graphql" | "ini" | "cfg" | "conf" | "lock" => FileKind::Code,
        "txt" | "log" | "md" | "markdown" | "rst" | "csv" | "tsv" | "rtf" | "nfo" => FileKind::Text,
        "pdf" => FileKind::Pdf,
        "doc" | "docx" | "odt" | "pages" => FileKind::Document,
        "xls" | "xlsx" | "xlsm" | "ods" | "numbers" => FileKind::Spreadsheet,
        "ppt" | "pptx" | "odp" | "key" => FileKind::Presentation,
        "exe" | "msi" | "dll" | "sys" | "com" | "appx" | "msix" => FileKind::Executable,
        "lnk" | "url" => FileKind::Shortcut,
        "ttf" | "otf" | "woff" | "woff2" | "ttc" => FileKind::Font,
        "iso" | "img" | "vhd" | "vhdx" | "dmg" => FileKind::DiskImage,
        _ => FileKind::Other,
    }
}

/// Only these ask the system for an icon (for apps, shortcuts and icon files the icon is the
/// content).
pub fn wants_system_icon(entry: &Entry) -> bool {
    entry.kind == EntryKind::File
        && !entry.is_cloud_placeholder()
        && matches!(entry.extension(), "exe" | "lnk" | "ico")
}

/// Size limit for thumbnail sources. Larger images take too long and too much memory to
/// decode, so they get the type icon.
pub const MAX_THUMBNAIL_SOURCE: u64 = 64 * 1024 * 1024;

/// Formats we can decode in Rust for thumbnails (`.ico` uses the system icon).
pub fn can_thumbnail(entry: &Entry) -> bool {
    entry.kind == EntryKind::File
        // Thumbnailing cloud placeholders would download every photo in the folder.
        && !entry.is_cloud_placeholder()
        && entry.size > 0
        && entry.size <= MAX_THUMBNAIL_SOURCE
        && matches!(
            entry.extension(),
            "png" | "jpg" | "jpeg" | "jfif" | "gif" | "bmp" | "webp" | "tif" | "tiff"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> Entry {
        Entry::new(name, EntryKind::File, size, None, 0)
    }

    /// OneDrive online-only files: no thumbnail and no system icon (both read the content,
    /// which triggers a download).
    #[test]
    fn cloud_placeholders_are_never_read() {
        let online_only = Entry::new("a.jpg", EntryKind::File, 1000, None, 0x40_0000);
        assert!(online_only.is_cloud_placeholder());
        assert!(!can_thumbnail(&online_only));
        let exe = Entry::new("setup.exe", EntryKind::File, 1000, None, 0x40_0000);
        assert!(!wants_system_icon(&exe));
        // Once downloaded (no recall attribute) it is handled normally.
        let local = Entry::new("a.jpg", EntryKind::File, 1000, None, 0);
        assert!(can_thumbnail(&local));
    }

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(file_kind(&file("Photo.JPG", 1)), FileKind::Image);
        assert_eq!(file_kind(&file("main.rs", 1)), FileKind::Code);
        assert_eq!(file_kind(&file("Report.docx", 1)), FileKind::Document);
        assert_eq!(file_kind(&file("README", 1)), FileKind::Other);
        assert_eq!(file_kind(&file(".gitignore", 1)), FileKind::Other);
        let dir = Entry::new("src.rs", EntryKind::Dir, 0, None, 0);
        assert_eq!(file_kind(&dir), FileKind::Folder);
    }

    #[test]
    fn system_icons_only_for_exe_lnk_ico() {
        assert!(wants_system_icon(&file("app.EXE", 1)));
        assert!(wants_system_icon(&file("Shortcut.lnk", 1)));
        assert!(wants_system_icon(&file("a.ico", 1)));
        assert!(!wants_system_icon(&file("a.dll", 1)));
        assert!(!wants_system_icon(&file("a.png", 1)));
        let dir = Entry::new("x.exe", EntryKind::Dir, 0, None, 0);
        assert!(!wants_system_icon(&dir));
    }

    #[test]
    fn thumbnails_have_size_limits() {
        assert!(can_thumbnail(&file("a.png", 10)));
        assert!(!can_thumbnail(&file("a.png", 0)));
        assert!(!can_thumbnail(&file("a.png", MAX_THUMBNAIL_SOURCE + 1)));
        assert!(!can_thumbnail(&file("a.svg", 10)));
        assert!(!can_thumbnail(&file("a.ico", 10)));
    }
}
