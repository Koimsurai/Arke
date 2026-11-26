//! Text search inside files (`content:` in the search box, see [`crate::index_query`]).
//!
//! - Only text-like files are read: known binary types are skipped by extension, and the first
//!   block is sniffed (a NUL byte without a UTF-16 byte order mark means binary).
//! - UTF-8 (with or without BOM) and UTF-16 LE/BE with a BOM are decoded as a stream, block by
//!   block, so a large log is never loaded whole.
//! - Case-insensitive, line by line: the first matching line becomes the snippet. Each block is
//!   scanned whole for the text first ([`Prefilter`]) and only the lines around a hit are
//!   checked line by line, which decides the result.
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

use aho_corasick::AhoCorasick;
use memchr::memmem;

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

/// Finds candidate positions in a whole block. It never misses a line that
/// [`ContentMatcher::match_line`] accepts, but may point into a line that it rejects (a hit
/// across a line break), so the line check stays the judge.
#[derive(Clone, Debug)]
enum Prefilter {
    /// Every line is a candidate.
    Unfiltered,
    /// ASCII case-insensitive (SIMD prefilters inside the automaton).
    Ascii(Box<AhoCorasick>),
    /// Exact bytes: the needle has no letter whose case could differ.
    Literal(Box<memmem::Finder<'static>>),
}

impl Prefilter {
    /// For a lowercase needle.
    fn new(needle: &str) -> Self {
        if !needle.is_ascii() && !needle.chars().all(uncased_or_plain_ascii) {
            return Self::Unfiltered;
        }
        if needle.is_ascii() || needle.bytes().any(|b| b.is_ascii_alphabetic()) {
            return AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle])
                .map_or(Self::Unfiltered, |ac| Self::Ascii(Box::new(ac)));
        }
        Self::Literal(Box::new(
            memmem::Finder::new(needle.as_bytes()).into_owned(),
        ))
    }

    /// Offset of the first candidate in `hay`; for [`Self::Unfiltered`] the start.
    fn find(&self, hay: &[u8]) -> Option<usize> {
        match self {
            Self::Unfiltered => (!hay.is_empty()).then_some(0),
            Self::Ascii(ac) => ac.find(hay).map(|m| m.start()),
            Self::Literal(finder) => finder.find(hay),
        }
    }
}

/// Whether a character of a non-ASCII needle looks the same in the file as in the lowercased
/// line the check compares: ASCII except `i` and `k` (the dotted capital I and the Kelvin sign
/// lowercase to them), or a character without case that no other character lowercases to.
fn uncased_or_plain_ascii(c: char) -> bool {
    if c.is_ascii() {
        return c != 'i' && c != 'k';
    }
    !c.is_lowercase()
        && !c.is_uppercase()
        && c.to_lowercase().eq(std::iter::once(c))
        && c.to_uppercase().eq(std::iter::once(c))
        // Combining marks: the lowercase of a dotted capital I ends in one.
        && !('\u{300}'..='\u{36F}').contains(&c)
}

/// Case-insensitive text to find.
#[derive(Clone, Debug)]
pub struct ContentMatcher {
    /// Lowercase needle.
    needle: String,
    /// Finds candidate lines in a block.
    prefilter: Prefilter,
}

impl ContentMatcher {
    /// `None` for blank text.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| {
            let needle = text.to_lowercase();
            Self {
                prefilter: Prefilter::new(&needle),
                needle,
            }
        })
    }

    /// Searches one file through `fs`. `None` if it doesn't match, can't be read, looks binary
    /// or the search was cancelled; otherwise the first matching line (for a document, the
    /// matching paragraph, cell or line of a slide or page).
    pub fn find_in(&self, fs: &dyn Fs, path: &Path, cancel: &AtomicBool) -> Option<String> {
        if let Some(kind) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(document_kind)
        {
            let reader = fs.open(path).ok()?;
            return self.find_in_document(reader, kind, cancel);
        }
        let mut reader = fs.open_sequential(path).ok()?;
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
        // Reused for blocks that are decoded (UTF-16, a character split across blocks).
        let mut scratch = String::new();
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
            let last = n == 0;
            let block_text = if let Some(valid) = decoder.valid_utf8(chunk, last) {
                valid
            } else {
                scratch.clear();
                decoder.decode(chunk, last, &mut scratch);
                scratch.as_str()
            };
            if let Some(hit) = lines.feed(block_text) {
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

    /// UTF-8 blocks that are valid as they are (or end in a split character, which is kept for
    /// the next block) are searched in place, without decoding into another buffer.
    fn valid_utf8<'a>(&mut self, chunk: &'a [u8], last: bool) -> Option<&'a str> {
        if self.encoding != Encoding::Utf8 || !self.pending.is_empty() {
            return None;
        }
        match std::str::from_utf8(chunk) {
            Ok(text) => Some(text),
            Err(err) if err.error_len().is_none() && !last => {
                let (valid, split) = chunk.split_at(err.valid_up_to());
                self.pending = split.to_vec();
                // Checked by from_utf8 just now.
                std::str::from_utf8(valid).ok()
            }
            Err(_) => None,
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
        if self.carry.is_empty() {
            // Search the block in place; only the unfinished last line is copied.
            let (hit, done) = scan_lines(self.matcher, text);
            if hit.is_some() {
                return hit;
            }
            self.carry.push_str(&text[done..]);
        } else {
            self.carry.push_str(text);
            let (hit, done) = scan_lines(self.matcher, &self.carry);
            if hit.is_some() {
                return hit;
            }
            self.carry.drain(..done);
        }
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

/// Checks the complete lines of `text` (those ending in a line break) in order. Returns the
/// first hit, or `None` and the length of the complete lines.
fn scan_lines(matcher: &ContentMatcher, text: &str) -> (Option<String>, usize) {
    let Some(last_nl) = text.rfind('\n') else {
        return (None, 0);
    };
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < last_nl {
        // Only the complete lines are searched, so a hit never reaches into the unfinished one.
        let Some(at) = matcher.prefilter.find(&bytes[start..last_nl]) else {
            break;
        };
        let at = start + at;
        let line_start = memchr::memrchr(b'\n', &bytes[start..at]).map_or(start, |i| start + i + 1);
        let line_end = memchr::memchr(b'\n', &bytes[at..]).map_or(last_nl, |i| at + i);
        if let Some(line) = text.get(line_start..line_end)
            && let Some(hit) = matcher.match_line(line.trim_end_matches('\r'))
        {
            return (Some(hit), 0);
        }
        start = line_end + 1;
    }
    (None, last_nl + 1)
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

    struct TempDir(
        PathBuf,
        #[allow(dead_code, reason = "held for its Drop, which deletes the folder")]
        tempfile::TempDir,
    );

    impl TempDir {
        fn new(tag: &str) -> Self {
            let guard = tempfile::Builder::new()
                .prefix(&format!("arke-content-{tag}-"))
                .tempdir()
                .unwrap();
            Self(guard.path().to_path_buf(), guard)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
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

    /// The search as it was before blocks were scanned whole: every line is checked in turn.
    /// Kept as the reference the block scan must agree with.
    fn oracle(matcher: &ContentMatcher, bytes: &[u8], block: usize) -> Option<String> {
        let mut reader = bytes;
        let mut buf = vec![0u8; block.max(4)];
        let mut decoder: Option<Decoder> = None;
        let mut carry = String::new();
        loop {
            let n = read_full(&mut reader, &mut buf).unwrap();
            let mut chunk = &buf[..n];
            if decoder.is_none() {
                let (encoding, skip) = sniff(chunk)?;
                chunk = &chunk[skip..];
                decoder = Some(Decoder::new(encoding));
            }
            let decoder = decoder.as_mut().unwrap();
            let mut text = String::new();
            decoder.decode(chunk, n == 0, &mut text);
            carry.push_str(&text);
            let mut done = 0;
            while let Some(nl) = carry[done..].find('\n') {
                let line = &carry[done..done + nl];
                if let Some(hit) = matcher.match_line(line.trim_end_matches('\r')) {
                    return Some(hit);
                }
                done += nl + 1;
            }
            carry.drain(..done);
            if carry.len() > MAX_LINE {
                if let Some(hit) = matcher.match_line(&carry) {
                    return Some(hit);
                }
                let mut cut = carry.len().saturating_sub(matcher.needle.len() * 4);
                while !carry.is_char_boundary(cut) {
                    cut -= 1;
                }
                carry.drain(..cut);
            }
            if n == 0 {
                return matcher.match_line(carry.trim_end_matches('\r'));
            }
        }
    }

    /// The block scan and the reference agree on `bytes`, at several block sizes.
    fn assert_same(needle: &str, bytes: &[u8]) -> Option<String> {
        let matcher = ContentMatcher::new(needle).unwrap();
        let cancel = AtomicBool::new(false);
        for block in [4, 5, 13, 64, 1000, BLOCK] {
            let got = matcher.find_blocks(&mut &*bytes, &cancel, block).unwrap();
            assert_eq!(
                got,
                oracle(&matcher, bytes, block),
                "needle {needle:?}, block {block}"
            );
        }
        oracle(&matcher, bytes, BLOCK)
    }

    #[test]
    fn block_scan_agrees_with_the_line_by_line_search() {
        let words = [
            "alpha",
            "Needle",
            "NEEDLE",
            "nEeDlE",
            "發票",
            "總計 Total",
            "ägypten",
            "ÄGYPTEN",
            "Grüße",
            "K發",
            "İstanbul",
            "x",
            "",
        ];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut text = String::new();
        for _ in 0..600 {
            for _ in 0..(seed % 5) {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                text.push_str(words[(seed >> 33) as usize % words.len()]);
                text.push(' ');
            }
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            text.push_str(if (seed >> 40) & 1 == 0 { "\r\n" } else { "\n" });
        }
        let no_match = text.to_lowercase().replace("needle", "n");
        let needles = [
            "needle",
            "NEEDLE",
            "發票",
            "總計 total",
            "ägypten",
            "ÄGYPTEN",
            "k發",
            "İstanbul",
            "istanbul",
            "x",
            "alpha needle",
            "gone",
            "total\r",
            "a\nb",
            "needle \r",
        ];
        for needle in needles {
            for body in [&text, &no_match] {
                let plain = body.as_bytes().to_vec();
                let mut bom = vec![0xEF, 0xBB, 0xBF];
                bom.extend_from_slice(&plain);
                for bytes in [
                    plain,
                    bom,
                    utf16(body, true),
                    utf16(body, false),
                    // No line break at the end.
                    body.trim_end().as_bytes().to_vec(),
                ] {
                    assert_same(needle, &bytes);
                }
            }
        }
    }

    #[test]
    fn match_across_the_block_boundary_is_found() {
        let filler = |len: usize| "abcdefghij".repeat(len / 10 + 1)[..len].to_string();
        // The needle starts a few bytes before the 64 KB mark, and ends after it.
        for offset in [1, 2, 3, 5] {
            let mut text = filler(BLOCK - offset - 1);
            text.push('\n');
            text.push_str("NeEdLe tail\nnext line\n");
            // Line ends exactly before the boundary, needle straddles it.
            let mut straddle = filler(BLOCK - offset);
            straddle.push_str("NeEdLe tail\nnext line\n");
            let hit = assert_same("needle", straddle.as_bytes());
            assert_eq!(
                hit.map(|h| h.ends_with("NeEdLe tail")),
                Some(true),
                "offset {offset}"
            );
            assert!(assert_same("needle", text.as_bytes()).is_some());
        }
        // A CJK character cut in two by the boundary, and a CJK needle across it.
        let mut cjk = "a".repeat(BLOCK - 1);
        cjk.push_str("發票 total\n");
        assert_eq!(
            assert_same("發票", cjk.as_bytes()).map(|h| h.ends_with("發票 total")),
            Some(true)
        );
        let mut cjk = "a".repeat(BLOCK - 4);
        cjk.push_str("發票發票\n");
        assert!(assert_same("票發", cjk.as_bytes()).is_some());
        // Needle across the boundary in UTF-16 (the boundary is in the middle of the code units).
        let wide = utf16(&format!("{}Needle\n", "ab".repeat(BLOCK / 4 - 2)), true);
        assert!(assert_same("needle", &wide).is_some());
        // Absent text is still absent.
        assert_eq!(assert_same("needlf", straddle_text().as_bytes()), None);
    }

    fn straddle_text() -> String {
        format!("{}needle\n", "x".repeat(BLOCK - 3))
    }

    #[test]
    fn first_last_line_and_line_endings() {
        let hit = |needle: &str, body: &str| assert_same(needle, body.as_bytes());
        assert_eq!(
            hit("first", "  The FIRST line  \nsecond\nthird").as_deref(),
            Some("The FIRST line")
        );
        assert_eq!(
            hit("third", "first\nsecond\nTHIRD").as_deref(),
            Some("THIRD")
        );
        assert_eq!(
            hit("third", "first\r\nsecond\r\nthird\r\n").as_deref(),
            Some("third")
        );
        assert_eq!(
            hit("third", "first\r\nsecond\r\nthird\r").as_deref(),
            Some("third")
        );
        assert_eq!(
            hit("second", "a\r\nsecond\r\n\r\nz").as_deref(),
            Some("second")
        );
        // A match is one line: never across a line break, and not on the CR of a CRLF.
        assert_eq!(hit("a b", "a\nb"), None);
        assert_eq!(hit("a\nb", "a\nb"), None);
        // The first matching line wins.
        assert_eq!(hit("dup", "one DUP\ntwo dup\n").as_deref(), Some("one DUP"));
    }

    #[test]
    fn utf16_files_use_the_same_scan() {
        let dir = TempDir::new("wide");
        let body = format!("{}\r\n總計 Total due\r\nend\r\n", "line\r\n".repeat(30_000));
        let le = dir.file("le.txt", &utf16(&body, true));
        let be = dir.file("be.txt", &utf16(&body, false));
        for path in [&le, &be] {
            assert_eq!(find("TOTAL DUE", path).as_deref(), Some("總計 Total due"));
            assert_eq!(find("總計", path).as_deref(), Some("總計 Total due"));
            assert_eq!(find("missing", path), None);
        }
    }

    #[test]
    fn needles_with_case_variants_are_not_filtered_out() {
        // The Kelvin sign lowercases to `k`, the dotted capital I to `i` and a dot.
        assert_eq!(
            assert_same("k發", "\u{212A}發 here\n".as_bytes()).as_deref(),
            Some("\u{212A}發 here")
        );
        // Lowercase "İ" is "i" plus a combining dot: plain "istanbul" never matched it.
        assert!(assert_same("istanbul", "İSTANBUL\n".as_bytes()).is_none());
        assert!(assert_same("i\u{307}stanbul", "İSTANBUL\n".as_bytes()).is_some());
        assert!(assert_same("ägypten", "ÄGYPTEN\n".as_bytes()).is_some());
        // These can use the plain byte search.
        assert!(matches!(
            ContentMatcher::new("發票").unwrap().prefilter,
            Prefilter::Literal(_)
        ));
        assert!(matches!(
            ContentMatcher::new("Needle").unwrap().prefilter,
            Prefilter::Ascii(_)
        ));
        assert!(matches!(
            ContentMatcher::new("ägypten").unwrap().prefilter,
            Prefilter::Unfiltered
        ));
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
