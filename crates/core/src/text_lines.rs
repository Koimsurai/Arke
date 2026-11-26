//! Where each line of a large text file starts, so a preview can read and decode only the lines
//! on screen ([`read_lines`]), however long the file is.
//!
//! [`LineIndex::build`] scans the bytes once, without decoding them. Lines end at the hard breaks
//! the preview splits decoded text at: LF, CR, CRLF, VT and FF, plus NEL, LINE SEPARATOR and
//! PARAGRAPH SEPARATOR in UTF-8 and UTF-16. The legacy code pages Arke decodes (Big5, GBK,
//! Shift-JIS, the Korean and the single-byte ones) never use the bytes 0x0A-0x0D inside a
//! character, so a byte scan finds their breaks too.
//!
//! Offsets are `u32`, 4 bytes a line: the index stops at 4 GB or at [`MAX_LINES`] lines.

use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::text::{TextEncoding, utf8_prefix, utf16};

/// Most lines indexed (16 MB of offsets); the preview of a longer file ends there.
pub const MAX_LINES: usize = 4_000_000;
/// Read size while scanning.
const CHUNK: usize = 1 << 20;
/// Lines spanning at most this many bytes are read in one go; a longer span line by line, each
/// read only up to its cut.
const SPAN_READ_MAX: u64 = 1 << 20;

/// Byte offsets of the lines of a text file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineIndex {
    /// Where each line starts; a line runs to the next one's start and includes its break.
    starts: Vec<u32>,
    /// Where the indexed text ends: the end of the file, or where the index stopped.
    end: u32,
    encoding: TextEncoding,
}

impl LineIndex {
    /// Scans `reader`, which is at byte `first` of the file (past any byte order mark), to its
    /// end, or until [`MAX_LINES`] lines or 4 GB. `None` when `cancel` was set meanwhile.
    pub fn build<R: Read + ?Sized>(
        reader: &mut R,
        first: u64,
        encoding: TextEncoding,
        cancel: &AtomicBool,
    ) -> io::Result<Option<Self>> {
        let first = u32::try_from(first).unwrap_or(u32::MAX);
        let mut scan = Scanner::new(encoding, first);
        let mut buf = vec![0; CHUNK];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let room = usize::try_from(u32::MAX - scan.pos).unwrap_or(usize::MAX);
            if room == 0 {
                break;
            }
            let want = buf.len().min(room);
            let n = match reader.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if !scan.feed(&buf[..n]) {
                break;
            }
        }
        Ok(Some(scan.finish()))
    }

    /// Number of lines (at least one: an empty file has one empty line).
    pub const fn len(&self) -> usize {
        self.starts.len()
    }

    /// Never true: even an empty file has a line. For clippy's `len_without_is_empty`.
    pub const fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// The encoding the index was built for.
    pub const fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// Bytes of `lines`, with their breaks. Lines past the end are empty.
    pub fn span(&self, lines: Range<usize>) -> Range<u64> {
        let at = |ix: usize| u64::from(self.starts.get(ix).copied().unwrap_or(self.end));
        let start = at(lines.start);
        start..at(lines.end).max(start)
    }

    /// Bytes of line `ix`, with its break.
    pub fn line_len(&self, ix: usize) -> usize {
        let span = self.span(ix..ix + 1);
        usize::try_from(span.end - span.start).unwrap_or(usize::MAX)
    }

    /// Memory the offsets take.
    pub const fn heap_bytes(&self) -> usize {
        self.starts.capacity() * size_of::<u32>()
    }
}

/// The byte scan behind [`LineIndex::build`], fed chunk by chunk: a break may straddle two
/// chunks (CR LF, a UTF-8 sequence, a UTF-16 unit).
struct Scanner {
    encoding: TextEncoding,
    starts: Vec<u32>,
    /// Offset of the next byte fed
    pos: u32,
    /// The last byte (or unit) was a CR: a LF right after it belongs to the same break.
    cr: bool,
    /// UTF-8: how far into NEL (C2 85) or LS/PS (E2 80 A8/A9) the last bytes were: 0 not, 1 after
    /// C2, 2 after E2, 3 after E2 80.
    seq: u8,
    /// UTF-16: the first byte of a unit the chunk ended in.
    half: Option<u8>,
    /// Set once [`MAX_LINES`] is reached: where the indexed text ends.
    full: Option<u32>,
}

impl Scanner {
    fn new(encoding: TextEncoding, first: u32) -> Self {
        Self {
            encoding,
            starts: vec![first],
            pos: first,
            cr: false,
            seq: 0,
            half: None,
            full: None,
        }
    }

    /// A line starts at `at`. False once the index is full.
    fn push(&mut self, at: u32) -> bool {
        if self.starts.len() >= MAX_LINES {
            self.full = Some(at);
            return false;
        }
        self.starts.push(at);
        true
    }

    /// Scans the next bytes. False once the index is full.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let base = self.pos;
        // `build` never reads past 4 GB, so the offsets fit.
        self.pos = base + bytes.len() as u32;
        match self.encoding {
            TextEncoding::Utf16Le => self.feed_units(bytes, base, true),
            TextEncoding::Utf16Be => self.feed_units(bytes, base, false),
            TextEncoding::Utf8 | TextEncoding::Legacy => self.feed_bytes(bytes, base),
        }
    }

    fn feed_bytes(&mut self, bytes: &[u8], base: u32) -> bool {
        let utf8 = self.encoding == TextEncoding::Utf8;
        for (k, &b) in bytes.iter().enumerate() {
            let at = base + k as u32;
            if self.cr {
                self.cr = false;
                if b == b'\n' {
                    self.seq = 0;
                    if !self.push(at + 1) {
                        return false;
                    }
                    continue;
                }
                if !self.push(at) {
                    return false;
                }
            }
            let mut start = match b {
                b'\n' | 0x0B | 0x0C => Some(at + 1),
                b'\r' => {
                    self.cr = true;
                    None
                }
                _ => None,
            };
            if utf8 {
                let (seq, ends) = utf8_step(self.seq, b);
                self.seq = seq;
                if ends {
                    start = Some(at + 1);
                }
            }
            if let Some(start) = start
                && !self.push(start)
            {
                return false;
            }
        }
        true
    }

    fn feed_units(&mut self, bytes: &[u8], base: u32, le: bool) -> bool {
        let join = |a: u8, b: u8| {
            if le {
                u16::from_le_bytes([a, b])
            } else {
                u16::from_be_bytes([a, b])
            }
        };
        let mut k = 0;
        if let Some(first) = self.half.take() {
            let Some(&second) = bytes.first() else {
                self.half = Some(first);
                return true;
            };
            // The unit began with the last byte of the previous chunk.
            if !self.unit(join(first, second), base - 1) {
                return false;
            }
            k = 1;
        }
        while k + 1 < bytes.len() {
            if !self.unit(join(bytes[k], bytes[k + 1]), base + k as u32) {
                return false;
            }
            k += 2;
        }
        if k < bytes.len() {
            self.half = Some(bytes[k]);
        }
        true
    }

    /// One UTF-16 unit, at byte `at`.
    fn unit(&mut self, unit: u16, at: u32) -> bool {
        if self.cr {
            self.cr = false;
            if unit == 0x0A {
                return self.push(at + 2);
            }
            if !self.push(at) {
                return false;
            }
        }
        match unit {
            0x0A | 0x0B | 0x0C | 0x85 | 0x2028 | 0x2029 => self.push(at + 2),
            0x0D => {
                self.cr = true;
                true
            }
            _ => true,
        }
    }

    fn finish(mut self) -> LineIndex {
        // A CR at the very end still ends its line: an empty line follows, as after any break.
        if self.full.is_none() && self.cr {
            let at = self.pos;
            self.push(at);
        }
        self.starts.shrink_to_fit();
        LineIndex {
            end: self.full.unwrap_or(self.pos),
            starts: self.starts,
            encoding: self.encoding,
        }
    }
}

/// UTF-8: the next state of the NEL / LS / PS match after byte `b`, and whether `b` ended one.
const fn utf8_step(seq: u8, b: u8) -> (u8, bool) {
    match (seq, b) {
        (1, 0x85) | (3, 0xA8 | 0xA9) => (0, true),
        (2, 0x80) => (3, false),
        (_, 0xC2) => (1, false),
        (_, 0xE2) => (2, false),
        _ => (0, false),
    }
}

/// Reads `lines` of the file `index` was built from and decodes them, each without its break. A
/// line longer than `max_bytes` is cut there (the rest isn't read) and ends with `…`. `legacy`
/// decodes the system code page. Lines the file no longer has (it got shorter) come back empty.
pub fn read_lines<R: Read + Seek + ?Sized>(
    reader: &mut R,
    index: &LineIndex,
    lines: Range<usize>,
    max_bytes: usize,
    legacy: &dyn Fn(&[u8]) -> String,
) -> io::Result<Vec<String>> {
    let lines = lines.start.min(index.len())..lines.end.min(index.len());
    let span = index.span(lines.clone());
    let encoding = index.encoding;
    let mut out = Vec::with_capacity(lines.len());
    let mut buf = Vec::new();
    if span.end - span.start <= SPAN_READ_MAX {
        reader.seek(SeekFrom::Start(span.start))?;
        read_up_to(reader, &mut buf, (span.end - span.start) as usize)?;
        for ix in lines {
            let line = index.span(ix..ix + 1);
            let from = ((line.start - span.start) as usize).min(buf.len());
            let to = ((line.end - span.start) as usize).min(buf.len());
            out.push(decode_line(&buf[from..to], encoding, max_bytes, legacy));
        }
    } else {
        // Very long lines: read each only up to its cut (and a break that may follow).
        for ix in lines {
            let line = index.span(ix..ix + 1);
            let len = (line.end - line.start).min(max_bytes as u64 + 4) as usize;
            reader.seek(SeekFrom::Start(line.start))?;
            read_up_to(reader, &mut buf, len)?;
            out.push(decode_line(&buf, encoding, max_bytes, legacy));
        }
    }
    Ok(out)
}

/// Reads up to `len` bytes into `buf`; fewer at the end of the file.
fn read_up_to<R: Read + ?Sized>(reader: &mut R, buf: &mut Vec<u8>, len: usize) -> io::Result<()> {
    buf.clear();
    buf.resize(len, 0);
    let mut filled = 0;
    while filled < len {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(())
}

/// One line's bytes (with its break, if it has one) as text: break dropped, cut at `max_bytes`.
fn decode_line(
    bytes: &[u8],
    encoding: TextEncoding,
    max_bytes: usize,
    legacy: &dyn Fn(&[u8]) -> String,
) -> String {
    let content = &bytes[..content_len(bytes, encoding)];
    let cut = content.len() > max_bytes;
    let mut keep = content.len().min(max_bytes);
    if matches!(encoding, TextEncoding::Utf16Le | TextEncoding::Utf16Be) {
        keep &= !1;
    }
    let part = &content[..keep];
    let mut text = match encoding {
        // A character cut at the end is dropped.
        TextEncoding::Utf8 => utf8_prefix(part),
        TextEncoding::Utf16Le => utf16(part, true),
        TextEncoding::Utf16Be => utf16(part, false),
        TextEncoding::Legacy => legacy(part),
    };
    if cut {
        text.push('…');
    }
    text
}

/// Length of a line's bytes without the break at its end.
fn content_len(bytes: &[u8], encoding: TextEncoding) -> usize {
    let n = bytes.len();
    match encoding {
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let le = encoding == TextEncoding::Utf16Le;
            let n = n & !1;
            let unit = |at: usize| {
                let pair = [bytes[at], bytes[at + 1]];
                if le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            };
            if n >= 4 && unit(n - 4) == 0x0D && unit(n - 2) == 0x0A {
                n - 4
            } else if n >= 2 && matches!(unit(n - 2), 0x0A..=0x0D | 0x85 | 0x2028 | 0x2029) {
                n - 2
            } else {
                n
            }
        }
        TextEncoding::Utf8 | TextEncoding::Legacy => {
            let utf8 = encoding == TextEncoding::Utf8;
            if bytes.ends_with(b"\r\n") || (utf8 && bytes.ends_with(&[0xC2, 0x85])) {
                n - 2
            } else if utf8
                && (bytes.ends_with(&[0xE2, 0x80, 0xA8]) || bytes.ends_with(&[0xE2, 0x80, 0xA9]))
            {
                n - 3
            } else if matches!(bytes.last().copied(), Some(0x0A..=0x0D)) {
                n - 1
            } else {
                n
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::io::Cursor;

    /// Hands out one byte per read, so every break straddles two chunks somewhere.
    struct Trickle(Cursor<Vec<u8>>);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(1);
            self.0.read(&mut buf[..n])
        }
    }

    fn no_legacy(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    fn indexed(bytes: &[u8], first: u64, encoding: TextEncoding) -> LineIndex {
        let mut reader = Cursor::new(bytes.to_vec());
        reader.set_position(first);
        let whole = LineIndex::build(&mut reader, first, encoding, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        // Read a byte at a time, breaks cut in two give the same lines.
        let mut trickle = Trickle(Cursor::new(bytes.to_vec()));
        trickle.0.set_position(first);
        let trickled = LineIndex::build(&mut trickle, first, encoding, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(whole, trickled);
        whole
    }

    fn lines(bytes: &[u8], first: u64, encoding: TextEncoding) -> Vec<String> {
        let index = indexed(bytes, first, encoding);
        let mut reader = Cursor::new(bytes.to_vec());
        read_lines(&mut reader, &index, 0..index.len(), 1000, &no_legacy).unwrap()
    }

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn utf16be(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    #[test]
    fn every_hard_break_ends_a_line() {
        let text = "a\nb\r\nc\rd\u{b}e\u{c}f\u{85}g\u{2028}h\u{2029}中文";
        let want = ["a", "b", "c", "d", "e", "f", "g", "h", "中文"];
        assert_eq!(lines(text.as_bytes(), 0, TextEncoding::Utf8), want);
        assert_eq!(lines(&utf16le(text), 0, TextEncoding::Utf16Le), want);
        assert_eq!(lines(&utf16be(text), 0, TextEncoding::Utf16Be), want);
    }

    #[test]
    fn a_trailing_break_leaves_an_empty_last_line() {
        assert_eq!(lines(b"a\n", 0, TextEncoding::Utf8), ["a", ""]);
        assert_eq!(lines(b"a\r", 0, TextEncoding::Utf8), ["a", ""]);
        assert_eq!(lines(b"a\r\n", 0, TextEncoding::Utf8), ["a", ""]);
        assert_eq!(lines(b"", 0, TextEncoding::Utf8), [""]);
        assert_eq!(lines(b"\r\r\n\n", 0, TextEncoding::Utf8), ["", "", "", ""]);
    }

    #[test]
    fn legacy_text_only_breaks_at_control_bytes() {
        // In a code page C2 85 is two characters, not NEL.
        let bytes = [b'a', 0xC2, 0x85, b'b', b'\n', b'c'];
        let index = indexed(&bytes, 0, TextEncoding::Legacy);
        assert_eq!(index.len(), 2);
        assert_eq!(index.line_len(0), 5);
    }

    #[test]
    fn the_byte_order_mark_is_skipped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"one\ntwo");
        assert_eq!(lines(&bytes, 3, TextEncoding::Utf8), ["one", "two"]);
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(utf16le("x\r\ny"));
        assert_eq!(lines(&bytes, 2, TextEncoding::Utf16Le), ["x", "y"]);
    }

    #[test]
    fn long_lines_are_cut_and_only_their_start_is_read() {
        let long = "é".repeat(600); // 1200 bytes
        let text = format!("short\n{long}\nend");
        let index = indexed(text.as_bytes(), 0, TextEncoding::Utf8);
        let mut reader = Cursor::new(text.into_bytes());
        let got = read_lines(&mut reader, &index, 0..3, 1001, &no_legacy).unwrap();
        // Cut at a character boundary (1001 is inside an é), then the ellipsis.
        assert_eq!(got[0], "short");
        assert_eq!(got[1], format!("{}…", "é".repeat(500)));
        assert_eq!(got[2], "end");

        // A span too long to read in one go: line by line, each cut.
        let huge = "x".repeat(SPAN_READ_MAX as usize + 10);
        let text = format!("{huge}\nlast");
        let index = indexed(text.as_bytes(), 0, TextEncoding::Utf8);
        let mut reader = Cursor::new(text.into_bytes());
        let got = read_lines(&mut reader, &index, 0..2, 8, &no_legacy).unwrap();
        assert_eq!(got, ["xxxxxxxx…", "last"]);
    }

    #[test]
    fn a_middle_range_reads_only_its_lines() {
        let text = (0..1000).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "line {n}");
            text
        });
        let index = indexed(text.as_bytes(), 0, TextEncoding::Utf8);
        assert_eq!(index.len(), 1001);
        let mut reader = Cursor::new(text.into_bytes());
        let got = read_lines(&mut reader, &index, 500..503, 100, &no_legacy).unwrap();
        assert_eq!(got, ["line 500", "line 501", "line 502"]);
        // Past the end: clamped.
        let got = read_lines(&mut reader, &index, 999..1100, 100, &no_legacy).unwrap();
        assert_eq!(got, ["line 999", ""]);
    }

    #[test]
    fn a_file_that_got_shorter_reads_empty_lines() {
        let index = indexed(b"a\nb\nc", 0, TextEncoding::Utf8);
        let mut reader = Cursor::new(b"a\n".to_vec());
        let got = read_lines(&mut reader, &index, 0..3, 100, &no_legacy).unwrap();
        assert_eq!(got, ["a", "", ""]);
    }

    #[test]
    fn the_index_stops_at_its_line_limit() {
        let bytes = vec![b'\n'; MAX_LINES + 5];
        let mut reader = Cursor::new(bytes);
        let index = LineIndex::build(&mut reader, 0, TextEncoding::Utf8, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(index.len(), MAX_LINES);
        // The last line indexed ends where the next one would have started.
        assert_eq!(index.span(MAX_LINES - 1..MAX_LINES).end, MAX_LINES as u64);
    }

    #[test]
    fn a_cancelled_scan_gives_nothing() {
        let mut reader = Cursor::new(b"a\nb".to_vec());
        let cancel = AtomicBool::new(true);
        assert!(
            LineIndex::build(&mut reader, 0, TextEncoding::Utf8, &cancel)
                .unwrap()
                .is_none()
        );
    }
}
