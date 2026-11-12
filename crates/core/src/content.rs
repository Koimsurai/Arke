//! Text search inside files (`content:` in the search box, see [`crate::index_query`]).
//!
//! - Only text-like files are read: known binary types are skipped by extension, and the first
//!   block is sniffed (a NUL byte without a UTF-16 byte order mark means binary).
//! - UTF-8 (with or without BOM) and UTF-16 LE/BE with a BOM are decoded as a stream, block by
//!   block, so a large log is never loaded whole.
//! - Case-insensitive, line by line: the first matching line becomes the snippet.
//! - Documents are searched through their text: Word, Excel and PowerPoint files (Office Open
//!   XML, see [`crate::office::search_text`]) paragraph, cell and slide line by line, PDFs
//!   page by page (see [`crate::pdf_text`]). The old binary Office formats aren't read.
//! - Files over [`MAX_CONTENT_FILE`] (documents: [`MAX_DOCUMENT_FILE`]) and cloud placeholders
//!   are never opened (reading an online-only OneDrive file would download it).
//!
//! Callers run this on background threads; `cancel` is checked once per block, and while a
//! document's text is extracted, which also stops after [`DOCUMENT_TIME`].

use std::io::{self, Read};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::entry::{Entry, EntryKind};
use crate::filetype::{FileKind, kind_for_extension};
use crate::fs::Fs;
use crate::office::{OfficeKind, office_kind};

/// Files larger than this are not searched.
pub const MAX_CONTENT_FILE: u64 = 20 * 1024 * 1024;
/// PDF and Office documents larger than this are not searched.
pub const MAX_DOCUMENT_FILE: u64 = 50 * 1024 * 1024;
/// Time spent on one document's text before moving on to the next file.
pub const DOCUMENT_TIME: Duration = Duration::from_secs(5);
/// Bytes read per step.
const BLOCK: usize = 64 * 1024;
/// Bytes sniffed for NUL to tell binary files.
const SNIFF: usize = 8 * 1024;
/// A line longer than this is checked in pieces (minified files have megabyte lines).
const MAX_LINE: usize = 64 * 1024;
/// Snippet length in characters.
const SNIPPET_CHARS: usize = 120;
/// Characters kept before the match when a long line is shortened.
const SNIPPET_LEAD: usize = 40;

/// A document whose text is extracted for a content search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocumentKind {
    Office(OfficeKind),
    Pdf,
}

fn document_kind(name: &str) -> Option<DocumentKind> {
    office_kind(name).map(DocumentKind::Office).or_else(|| {
        name.rsplit_once('.')
            .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("pdf"))
            .then_some(DocumentKind::Pdf)
    })
}

/// Whether a listed file is worth opening for a content search: a regular local file within the
/// size cap that is text-like (its extension isn't a known binary format) or a PDF or Office
/// Open XML document.
pub fn worth_reading(entry: &Entry) -> bool {
    if entry.kind != EntryKind::File || entry.is_cloud_placeholder() || entry.size == 0 {
        return false;
    }
    if document_kind(&entry.name).is_some() {
        return entry.size <= MAX_DOCUMENT_FILE;
    }
    if entry.size > MAX_CONTENT_FILE {
        return false;
    }
    let ext = entry.extension();
    match kind_for_extension(ext) {
        // SVG is XML text.
        FileKind::Image => ext == "svg",
        FileKind::Video
        | FileKind::Audio
        | FileKind::Archive
        | FileKind::Executable
        | FileKind::Font
        | FileKind::DiskImage
        | FileKind::Pdf
        | FileKind::Document
        | FileKind::Spreadsheet
        | FileKind::Presentation => false,
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// Case-insensitive text to find.
#[derive(Clone, Debug)]
pub struct ContentMatcher {
    /// Lowercase needle.
    needle: String,
}

impl ContentMatcher {
    /// `None` for blank text.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self {
            needle: text.to_lowercase(),
        })
    }

    /// Searches one file through `fs`. `None` if it doesn't match, can't be read, looks binary
    /// or the search was cancelled; otherwise the first matching line (for a document, the
    /// matching paragraph, cell or line of a slide or page).
    pub fn find_in(&self, fs: &dyn Fs, path: &Path, cancel: &AtomicBool) -> Option<String> {
        let mut reader = fs.open(path).ok()?;
        if let Some(kind) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(document_kind)
        {
            return self.find_in_document(reader, kind, cancel);
        }
        self.find(&mut reader, cancel).ok().flatten()
    }

    fn find_in_document(
        &self,
        reader: Box<dyn crate::fs::ReadSeek>,
        kind: DocumentKind,
        cancel: &AtomicBool,
    ) -> Option<String> {
        let deadline = Instant::now() + DOCUMENT_TIME;
        let mut stop = || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline;
        let mut found = |line: &str| self.match_line(line);
        // Parsers of untrusted files: a panic in one costs that file, not the search.
        std::panic::catch_unwind(AssertUnwindSafe(|| match kind {
            DocumentKind::Office(kind) => {
                crate::office::search_text(reader, kind, &mut stop, &mut found)
                    .ok()
                    .flatten()
            }
            DocumentKind::Pdf => crate::pdf_text::search_text(reader, &mut stop, &mut found),
        }))
        .ok()
        .flatten()
    }

    /// Searches a stream (see [`Self::find_in`]).
    pub fn find(&self, reader: &mut dyn Read, cancel: &AtomicBool) -> io::Result<Option<String>> {
        self.find_blocks(reader, cancel, BLOCK)
    }

    fn find_blocks(
        &self,
        reader: &mut dyn Read,
        cancel: &AtomicBool,
        block: usize,
    ) -> io::Result<Option<String>> {
        let mut buf = vec![0u8; block.max(4)];
        let mut lines = Lines::new(self);
        let mut decoder: Option<Decoder> = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let n = read_full(reader, &mut buf)?;
            let mut chunk = &buf[..n];
            if decoder.is_none() {
                let Some((encoding, skip)) = sniff(chunk) else {
                    return Ok(None);
                };
                chunk = &chunk[skip..];
                decoder = Some(Decoder::new(encoding));
            }
            let Some(decoder) = decoder.as_mut() else {
                return Ok(None);
            };
            let mut text = String::new();
            decoder.decode(chunk, n == 0, &mut text);
            if let Some(hit) = lines.feed(&text) {
                return Ok(Some(hit));
            }
            if n == 0 {
                return Ok(lines.finish());
            }
        }
    }

    /// The snippet if `line` contains the needle.
    fn match_line(&self, line: &str) -> Option<String> {
        let at_char = if self.needle.is_ascii() {
            let needle = self.needle.as_bytes();
            let at = line
                .as_bytes()
                .windows(needle.len())
                .position(|w| w.eq_ignore_ascii_case(needle))?;
            line[..at].chars().count()
        } else {
            let lower = line.to_lowercase();
            let at = lower.find(&self.needle)?;
            // Lowercasing rarely changes the length; the position only places the snippet.
            lower[..at].chars().count()
        };
        Some(snippet(line, at_char))
    }
}

/// Fills `buf` as far as the stream allows (short reads are normal for pipes and network files).
fn read_full(reader: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// Encoding and BOM length from the first block, `None` for binary.
fn sniff(head: &[u8]) -> Option<(Encoding, usize)> {
    match head {
        [0xEF, 0xBB, 0xBF, ..] => Some((Encoding::Utf8, 3)),
        [0xFF, 0xFE, ..] => Some((Encoding::Utf16Le, 2)),
        [0xFE, 0xFF, ..] => Some((Encoding::Utf16Be, 2)),
        _ if head[..head.len().min(SNIFF)].contains(&0) => None,
        _ => Some((Encoding::Utf8, 0)),
    }
}

/// Streaming decoder: keeps a split character for the next block.
struct Decoder {
    encoding: Encoding,
    pending: Vec<u8>,
}

impl Decoder {
    const fn new(encoding: Encoding) -> Self {
        Self {
            encoding,
            pending: Vec::new(),
        }
    }

    fn decode(&mut self, chunk: &[u8], last: bool, out: &mut String) {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        match self.encoding {
            Encoding::Utf8 => self.decode_utf8(bytes, last, out),
            Encoding::Utf16Le | Encoding::Utf16Be => self.decode_utf16(&bytes, last, out),
        }
    }

    fn decode_utf8(&mut self, bytes: Vec<u8>, last: bool, out: &mut String) {
        let mut rest = bytes.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    return;
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    // Checked by from_utf8 just now.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match err.error_len() {
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[bad..];
                        }
                        // A character split across blocks: finish it next time.
                        None if !last => {
                            self.pending = after.to_vec();
                            return;
                        }
                        None => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            return;
                        }
                    }
                }
            }
        }
    }

    fn decode_utf16(&mut self, bytes: &[u8], last: bool, out: &mut String) {
        let pairs = bytes.len() / 2;
        let mut units: Vec<u16> = (0..pairs)
            .map(|i| {
                let pair = [bytes[2 * i], bytes[2 * i + 1]];
                if self.encoding == Encoding::Utf16Le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        let mut keep: Vec<u8> = bytes[pairs * 2..].to_vec();
        // A surrogate pair split across blocks: keep the high half for next time.
        if !last && units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            let at = (units.len() - 1) * 2;
            keep.splice(0..0, bytes[at..at + 2].iter().copied());
            units.pop();
        }
        self.pending = if last { Vec::new() } else { keep };
        out.extend(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
    }
}

/// Splits decoded text into lines across blocks and checks each.
struct Lines<'a> {
    matcher: &'a ContentMatcher,
    carry: String,
}

impl<'a> Lines<'a> {
    const fn new(matcher: &'a ContentMatcher) -> Self {
        Self {
            matcher,
            carry: String::new(),
        }
    }

    fn feed(&mut self, text: &str) -> Option<String> {
        self.carry.push_str(text);
        let mut done = 0;
        while let Some(nl) = self.carry[done..].find('\n') {
            let line = &self.carry[done..done + nl];
            if let Some(hit) = self.matcher.match_line(line.trim_end_matches('\r')) {
                return Some(hit);
            }
            done += nl + 1;
        }
        self.carry.drain(..done);
        if self.carry.len() > MAX_LINE {
            // A huge line: check what we have, keep a tail so a match across the cut is found.
            if let Some(hit) = self.matcher.match_line(&self.carry) {
                return Some(hit);
            }
            let keep = self.matcher.needle.len() * 4;
            let mut cut = self.carry.len().saturating_sub(keep);
            while !self.carry.is_char_boundary(cut) {
                cut -= 1;
            }
            self.carry.drain(..cut);
        }
        None
    }

    fn finish(&mut self) -> Option<String> {
        let line = std::mem::take(&mut self.carry);
        self.matcher.match_line(line.trim_end_matches('\r'))
    }
}

/// The line, trimmed and shortened around the match (`at` is in characters).
fn snippet(line: &str, at: usize) -> String {
    let lead = line.chars().take_while(|c| c.is_whitespace()).count();
    let line: String = line
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let len = line.chars().count();
    if len <= SNIPPET_CHARS {
        return line;
    }
    let at = at.saturating_sub(lead);
    let start = at.saturating_sub(SNIPPET_LEAD).min(len - SNIPPET_CHARS);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(line.chars().skip(start).take(SNIPPET_CHARS));
    if start + SNIPPET_CHARS < len {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::RealFs;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "arke-content-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn utf16(text: &str, little: bool) -> Vec<u8> {
        let mut out = if little {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for unit in text.encode_utf16() {
            out.extend(if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        out
    }

    fn find(needle: &str, path: &Path) -> Option<String> {
        ContentMatcher::new(needle)
            .unwrap()
            .find_in(&RealFs, path, &AtomicBool::new(false))
    }

    #[test]
    fn finds_text_in_every_supported_encoding() {
        let dir = TempDir::new("enc");
        let body = "first line\r\n  Total due: 42 EUR\r\nlast 發票 line\n";
        let plain = dir.file("plain.txt", body.as_bytes());
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(body.as_bytes());
        let bom = dir.file("bom.txt", &bom);
        let le = dir.file("le.txt", &utf16(body, true));
        let be = dir.file("be.txt", &utf16(body, false));
        for path in [&plain, &bom, &le, &be] {
            assert_eq!(
                find("TOTAL DUE", path).as_deref(),
                Some("Total due: 42 EUR"),
                "{}",
                path.display()
            );
            assert_eq!(find("發票", path).as_deref(), Some("last 發票 line"));
            assert_eq!(find("missing", path), None);
        }
    }

    #[test]
    fn non_ascii_text_is_case_insensitive() {
        let dir = TempDir::new("case");
        let path = dir.file("de.md", "Grüße aus ÄGYPTEN\n".as_bytes());
        assert!(find("ägypten", &path).is_some());
        assert!(
            find("GRÜSSE", &path).is_none(),
            "no full case folding, only lowercase"
        );
        assert!(find("grüße", &path).is_some());
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = TempDir::new("bin");
        let mut bytes = b"MZ needle".to_vec();
        bytes.extend([0, 1, 2, 3]);
        let path = dir.file("tool.dat", &bytes);
        assert_eq!(find("needle", &path), None, "NUL bytes mean binary");
        // Unreadable files are just no match.
        assert_eq!(find("needle", &dir.0.join("gone.txt")), None);
    }

    #[test]
    fn lines_and_characters_split_across_blocks_still_match() {
        let matcher = ContentMatcher::new("needle").unwrap();
        let cancel = AtomicBool::new(false);
        let text = format!(
            "{}\nhay hay neédle? no: the NEEDLE here\nend",
            "x".repeat(37)
        );
        for block in [4, 5, 7, 16, 1024] {
            let hit = matcher
                .find_blocks(&mut text.as_bytes(), &cancel, block)
                .unwrap();
            assert_eq!(hit.as_deref(), Some("hay hay neédle? no: the NEEDLE here"));
            // UTF-16 with a surrogate pair (emoji) split across blocks.
            let wide = utf16("🎉 party\n🎉 Needle 🎉\n", true);
            let hit = matcher.find_blocks(&mut wide.as_slice(), &cancel, block);
            assert_eq!(
                hit.unwrap().as_deref(),
                Some("🎉 Needle 🎉"),
                "block {block}"
            );
        }
        // A line far longer than the line limit, match near the end, no newline at all.
        let long = format!("{}needle{}", "a".repeat(MAX_LINE * 3), "b".repeat(500));
        let hit = matcher
            .find_blocks(&mut long.as_bytes(), &cancel, 4096)
            .unwrap()
            .unwrap();
        assert!(hit.contains("needle") && hit.starts_with('…') && hit.ends_with('…'));
        assert!(hit.chars().count() <= SNIPPET_CHARS + 2, "{hit}");
    }

    #[test]
    fn cancel_stops_reading() {
        let matcher = ContentMatcher::new("x").unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(matcher.find(&mut &b"x"[..], &cancel).unwrap(), None);
    }

    #[test]
    fn only_local_text_like_files_within_the_cap_are_opened() {
        let file = |name: &str, size: u64, attrs: u32| {
            Entry::new(name, EntryKind::File, size, None, attrs)
        };
        assert!(worth_reading(&file("notes.txt", 10, 0)));
        assert!(worth_reading(&file("main.rs", 10, 0)));
        assert!(
            worth_reading(&file("README", 10, 0)),
            "unknown types get sniffed"
        );
        assert!(worth_reading(&file("logo.svg", 10, 0)));
        assert!(!worth_reading(&file("a.jpg", 10, 0)));
        assert!(!worth_reading(&file("a.zip", 10, 0)));
        assert!(!worth_reading(&file("big.log", MAX_CONTENT_FILE + 1, 0)));
        assert!(!worth_reading(&file("empty.txt", 0, 0)));
        // OneDrive online-only: reading would download it.
        assert!(!worth_reading(&file("cloud.txt", 10, 0x40_0000)));
        // Offline files.
        assert!(!worth_reading(&file("offline.txt", 10, 0x1000)));
        let dir = Entry::new("src", EntryKind::Dir, 0, None, 0);
        assert!(!worth_reading(&dir));
        // Documents have their own, larger cap; the old binary formats aren't read.
        for name in ["a.docx", "a.DOCM", "a.xlsx", "a.pptx", "a.pdf"] {
            assert!(
                worth_reading(&file(name, MAX_CONTENT_FILE + 1, 0)),
                "{name}"
            );
            assert!(
                !worth_reading(&file(name, MAX_DOCUMENT_FILE + 1, 0)),
                "{name}"
            );
            assert!(!worth_reading(&file(name, 10, 0x40_0000)), "{name}");
        }
        for name in ["a.doc", "a.xls", "a.ppt", "a.odt"] {
            assert!(!worth_reading(&file(name, 10, 0)), "{name}");
        }
    }

    #[test]
    fn finds_text_inside_office_documents_and_pdfs() {
        use crate::office::fixtures as office;
        let dir = TempDir::new("docs");
        let docx = dir.file(
            "memo.docx",
            &office::docx(
                "<w:p><w:r><w:t>Dear team,</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>  Total due: 42 EUR</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
            ),
        );
        let xlsx = dir.file(
            "book.xlsx",
            &office::xlsx(
                &[("S", r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#)],
                &["Total due per month"],
                "",
            ),
        );
        let pptx = dir.file(
            "deck.pptx",
            &office::pptx(&[("Plan", &["total due next week"])], None),
        );
        let pdf = dir.file(
            "scan.pdf",
            &crate::pdf_text::fixtures::pdf(&[&["cover"], &["發票", "TOTAL DUE 99"]], true),
        );
        assert_eq!(
            find("total due", &docx).as_deref(),
            Some("Total due: 42 EUR")
        );
        assert_eq!(
            find("total due", &xlsx).as_deref(),
            Some("Total due per month")
        );
        assert_eq!(
            find("total due", &pptx).as_deref(),
            Some("total due next week")
        );
        assert_eq!(find("total due", &pdf).as_deref(), Some("TOTAL DUE 99"));
        assert_eq!(find("發票", &pdf).as_deref(), Some("發票"));
        for path in [&docx, &xlsx, &pptx, &pdf] {
            assert_eq!(find("missing", path), None, "{}", path.display());
        }
        // A long paragraph is cut around the match like a long line.
        let long = format!("{} needle {}", "word ".repeat(200), "word ".repeat(200));
        let long = dir.file(
            "long.docx",
            &office::docx(&format!("<w:p><w:r><w:t>{long}</w:t></w:r></w:p>")),
        );
        let hit = find("needle", &long).unwrap();
        assert!(
            hit.starts_with('…') && hit.ends_with('…') && hit.contains("needle"),
            "{hit}"
        );
        // Broken documents are just no match, under any name.
        let broken = dir.file("broken.pdf", b"%PDF-1.4 needle");
        assert_eq!(find("needle", &broken), None);
        let fake = dir.file("fake.docx", b"needle");
        assert_eq!(find("needle", &fake), None);
        // Cancelled before it starts.
        let matcher = ContentMatcher::new("total due").unwrap();
        assert_eq!(matcher.find_in(&RealFs, &pdf, &AtomicBool::new(true)), None);
        assert_eq!(
            matcher.find_in(&RealFs, &docx, &AtomicBool::new(true)),
            None
        );
    }
}
