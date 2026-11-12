//! Text of PDF files for content search (`content:` in the search box, see [`crate::content`]).
//!
//! The file is parsed with `lopdf` (pure Rust), then each page's content stream is walked for
//! its text-showing operators, decoded through the page's fonts (`/ToUnicode` CMaps included,
//! so CJK text set in Identity-H fonts is found). Text in form XObjects is read too.
//!
//! - Files over [`MAX_FILE`] aren't read; only the first [`MAX_PAGES`] pages are searched.
//! - Every stream is decompressed up to a cap, so a decompression bomb fails instead of
//!   exhausting memory; images are dropped while loading.
//! - Files that need a password are skipped. Files encrypted only to restrict printing or
//!   copying (an empty user password, which any viewer opens without asking) are searched.
//! - `stop` is polled between pages and while walking a page, so a slow file ends at the
//!   caller's deadline. Parsing itself can't be interrupted; callers run this on a reader
//!   thread and catch panics (see [`crate::content`]).
//!
//! Text lines follow the text's vertical position. Words set apart only by positioning (no space
//! character, no large gap in a `TJ` array) run together, so a single word is always found but a
//! phrase may not be.

use std::collections::BTreeMap;
use std::io::Read;

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Encoding, LoadOptions, Object, ObjectId, Stream};

/// Largest PDF searched.
pub const MAX_FILE: u64 = 50 * 1024 * 1024;
/// Pages searched, from the first.
pub const MAX_PAGES: usize = 300;
/// Bytes any one stream (object stream, font CMap, form) may decompress to.
const MAX_STREAM: usize = 16 * 1024 * 1024;
/// Bytes a page's content streams may decompress to together.
const MAX_PAGE_CONTENT: usize = 32 * 1024 * 1024;
/// Nesting of form XObjects followed.
const MAX_FORM_DEPTH: usize = 4;
/// Operators between two polls of the stop check.
const STOP_EVERY: usize = 4096;
/// A gap in a `TJ` array wider than this (thousandths of a text space unit) is a word break.
const TJ_SPACE: f32 = 200.0;
/// Vertical moves smaller than this (text space units) stay on the same line.
const SAME_LINE: f32 = 0.5;

/// Searches a PDF's text: `found` gets each line until it returns `Some`; `stop` is polled
/// between pages and while reading a page and ends the search with `None`. Unreadable,
/// password-protected or oversized files are no match.
pub fn search_text<T>(
    reader: impl Read,
    stop: &mut dyn FnMut() -> bool,
    found: &mut dyn FnMut(&str) -> Option<T>,
) -> Option<T> {
    let mut bytes = Vec::new();
    reader.take(MAX_FILE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE || !is_pdf(&bytes) || stop() {
        return None;
    }
    let options = LoadOptions {
        filter: Some(drop_images),
        max_decompressed_size: Some(MAX_STREAM),
        ..LoadOptions::default()
    };
    let doc = Document::load_mem_with_options(&bytes, options).ok()?;
    drop(bytes);
    // Still encrypted after loading: the empty password didn't open it.
    if doc.is_encrypted() {
        return None;
    }
    for page in doc.get_pages().into_values().take(MAX_PAGES) {
        if stop() {
            return None;
        }
        let Some(text) = page_text(&doc, page, stop) else {
            continue;
        };
        if let Some(hit) = text.lines().find_map(|line| {
            let line = line.trim();
            if line.is_empty() { None } else { found(line) }
        }) {
            return Some(hit);
        }
    }
    None
}

/// A PDF header in the first kilobyte (some writers put junk before it).
fn is_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|w| w == b"%PDF-")
}

/// Load filter: image streams are the bulk of most PDFs and hold no text.
///
/// lopdf keeps `object` itself for objects stored directly in the file and only uses the
/// returned copy for objects inside object streams, which are never streams. So a stream (often
/// megabytes) isn't copied here; the tests notice if that ever changes.
fn drop_images(id: ObjectId, object: &mut Object) -> Option<(ObjectId, Object)> {
    match object {
        Object::Stream(stream) => {
            let image = stream
                .dict
                .get(b"Subtype")
                .and_then(Object::as_name)
                .is_ok_and(|n| n == b"Image");
            (!image).then_some((id, Object::Null))
        }
        other => Some((id, other.clone())),
    }
}

/// The text of one page, lines separated by `\n`; `None` if it can't be read or `stop` fired.
fn page_text(doc: &Document, page: ObjectId, stop: &mut dyn FnMut() -> bool) -> Option<String> {
    let fonts: Fonts = doc
        .get_page_fonts(page)
        .ok()?
        .into_iter()
        .filter_map(|(name, font)| {
            font.get_font_encoding_with_limit(doc, MAX_STREAM)
                .ok()
                .map(|e| (name, e))
        })
        .collect();
    let (own, inherited) = doc.get_page_resources(page).ok()?;
    let mut resources: Vec<&Dictionary> = own.into_iter().collect();
    resources.extend(
        inherited
            .iter()
            .filter_map(|id| doc.get_dictionary(*id).ok()),
    );
    let content = doc
        .get_page_content_with_limit(page, MAX_PAGE_CONTENT)
        .ok()?;
    let content = Content::decode(&content).ok()?;
    let mut walker = Walker {
        doc,
        fonts: vec![fonts],
        text: String::new(),
        state: TextState::default(),
        ops: 0,
        stop,
        stopped: false,
    };
    walker.walk(&content.operations, &resources, 0);
    (!walker.stopped).then_some(walker.text)
}

/// Font encodings by resource name.
type Fonts<'a> = BTreeMap<Vec<u8>, Encoding<'a>>;

/// Where text is being set, as far as line breaks go.
#[derive(Default)]
struct TextState {
    /// Font resource name set by `Tf`
    font: Option<Vec<u8>>,
    /// Vertical position of the line matrix
    y: f32,
    /// Vertical scale of the text matrix (`Td` moves are in text space)
    scale: f32,
    /// Position of the last text shown, `None` right after a line break
    shown_y: Option<f32>,
}

struct Walker<'a, 's> {
    doc: &'a Document,
    /// Font scopes, innermost last (a form's own fonts over the page's)
    fonts: Vec<Fonts<'a>>,
    text: String,
    state: TextState,
    ops: usize,
    stop: &'s mut dyn FnMut() -> bool,
    stopped: bool,
}

impl<'a> Walker<'a, '_> {
    fn walk(&mut self, ops: &[Operation], resources: &[&'a Dictionary], depth: usize) {
        for op in ops {
            self.ops += 1;
            if self.ops.is_multiple_of(STOP_EVERY) && (self.stop)() {
                self.stopped = true;
            }
            if self.stopped {
                return;
            }
            let float = |i: usize| op.operands.get(i).and_then(|o| o.as_float().ok());
            match op.operator.as_str() {
                "BT" => {
                    self.state.y = 0.0;
                    self.state.scale = 1.0;
                }
                "Tf" => {
                    self.state.font = op
                        .operands
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .map(<[u8]>::to_vec);
                }
                "Td" | "TD" => {
                    if let Some(ty) = float(1) {
                        self.state.y += ty * self.scale();
                    }
                }
                "Tm" => {
                    if let (Some(d), Some(f)) = (float(3), float(5)) {
                        self.state.scale = d;
                        self.state.y = f;
                    }
                }
                "T*" => self.line_break(),
                "Tj" => self.show(&op.operands),
                "'" => {
                    self.line_break();
                    self.show(&op.operands);
                }
                "\"" => {
                    self.line_break();
                    self.show(op.operands.get(2..).unwrap_or_default());
                }
                "TJ" => {
                    if let Some(Ok(items)) = op.operands.first().map(Object::as_array) {
                        self.show(items);
                    }
                }
                "Do" if depth < MAX_FORM_DEPTH => {
                    if let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) {
                        self.form(name, resources, depth);
                    }
                }
                _ => {}
            }
        }
    }

    fn scale(&self) -> f32 {
        let scale = self.state.scale.abs();
        if scale < f32::EPSILON { 1.0 } else { scale }
    }

    fn line_break(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.state.shown_y = None;
    }

    /// Appends the strings among `items` (and word gaps between them) in the current font.
    fn show(&mut self, items: &[Object]) {
        let y = self.state.y;
        if self
            .state
            .shown_y
            .is_some_and(|shown| (shown - y).abs() > SAME_LINE)
        {
            self.line_break();
        }
        self.state.shown_y = Some(y);
        for item in items {
            match item {
                Object::String(bytes, _) => self.decode(bytes),
                Object::Integer(_) | Object::Real(_)
                    if item.as_float().is_ok_and(|gap| -gap > TJ_SPACE)
                        && !self.text.ends_with(char::is_whitespace) =>
                {
                    self.text.push(' ');
                }
                _ => {}
            }
        }
    }

    fn decode(&mut self, bytes: &[u8]) {
        let encoding = self
            .state
            .font
            .as_ref()
            .and_then(|name| self.fonts.iter().rev().find_map(|f| f.get(name)));
        match encoding {
            Some(encoding) => {
                if let Ok(text) = Document::decode_text(encoding, bytes) {
                    self.text.extend(
                        text.chars()
                            .map(|c| if c.is_control() && c != '\n' { ' ' } else { c }),
                    );
                }
            }
            // No usable font: plain bytes are most likely Latin text.
            None => self.text.extend(bytes.iter().map(|&b| {
                if b.is_ascii_graphic() {
                    char::from(b)
                } else {
                    ' '
                }
            })),
        }
    }

    /// Walks the form XObject `name` with its own resources over the current ones.
    fn form(&mut self, name: &[u8], resources: &[&'a Dictionary], depth: usize) {
        let Some(stream) = resources
            .iter()
            .find_map(|res| form_named(self.doc, res, name))
        else {
            return;
        };
        let Ok(bytes) = stream.decompressed_content_with_limit(MAX_STREAM) else {
            return;
        };
        let Ok(content) = Content::decode(&bytes) else {
            return;
        };
        let own = stream
            .dict
            .get_deref(b"Resources", self.doc)
            .and_then(Object::as_dict)
            .ok();
        let mut scope: Vec<&'a Dictionary> = own.into_iter().collect();
        scope.extend_from_slice(resources);
        let fonts = own.map(|res| fonts_in(self.doc, res)).unwrap_or_default();
        self.fonts.push(fonts);
        // A form's text is its own text object; the page's position resumes after it.
        let saved = std::mem::take(&mut self.state);
        self.line_break();
        self.walk(&content.operations, &scope, depth + 1);
        self.state = saved;
        self.state.shown_y = None;
        self.fonts.pop();
    }
}

/// A form XObject named in a resource dictionary.
fn form_named<'a>(doc: &'a Document, resources: &'a Dictionary, name: &[u8]) -> Option<&'a Stream> {
    let xobjects = resources
        .get_deref(b"XObject", doc)
        .and_then(Object::as_dict)
        .ok()?;
    let stream = xobjects
        .get_deref(name, doc)
        .and_then(Object::as_stream)
        .ok()?;
    stream
        .dict
        .get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Form")
        .then_some(stream)
}

/// The fonts of a resource dictionary.
fn fonts_in<'a>(doc: &'a Document, resources: &'a Dictionary) -> Fonts<'a> {
    let Ok(fonts) = resources.get_deref(b"Font", doc).and_then(Object::as_dict) else {
        return Fonts::new();
    };
    fonts
        .iter()
        .filter_map(|(name, font)| {
            let font = doc.dereference(font).ok()?.1.as_dict().ok()?;
            let encoding = font.get_font_encoding_with_limit(doc, MAX_STREAM).ok()?;
            Some((name.clone(), encoding))
        })
        .collect()
}

// ───────────────────────── Test fixtures ─────────────────────────

/// Builds small PDFs for tests (no binary fixtures in the repository).
#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::expect_used)]
pub mod fixtures {
    use std::fmt::Write as _;
    use std::io::Write as _;

    /// A PDF with one page per entry, each with these lines of text. ASCII is set in Helvetica;
    /// other characters in a Type0 font (Identity-H) whose `/ToUnicode` CMap maps them back,
    /// as CJK PDFs do. With `compress`, content streams are FlateDecode.
    pub fn pdf(pages: &[&[&str]], compress: bool) -> Vec<u8> {
        build(pages, compress, false)
    }

    /// Like [`pdf`], with an `/Encrypt` entry whose password checks fail for the empty
    /// password (the text itself stays plain).
    pub fn protected_pdf(pages: &[&[&str]]) -> Vec<u8> {
        build(pages, false, true)
    }

    /// Characters not set in Helvetica get CIDs from 1 in order of appearance.
    fn cids(pages: &[&[&str]]) -> Vec<char> {
        let mut out: Vec<char> = Vec::new();
        for c in pages.iter().flat_map(|p| p.iter()).flat_map(|l| l.chars()) {
            if !c.is_ascii() && !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    fn content(lines: &[&str], cids: &[char]) -> String {
        let mut out = String::from("BT\n72 720 Td\n");
        for line in lines {
            let mut rest = *line;
            while !rest.is_empty() {
                let ascii = rest.chars().next().is_some_and(|c| c.is_ascii());
                let end = rest
                    .find(|c: char| c.is_ascii() != ascii)
                    .unwrap_or(rest.len());
                let (run, tail) = rest.split_at(end);
                if ascii {
                    let escaped = run
                        .replace('\\', "\\\\")
                        .replace('(', "\\(")
                        .replace(')', "\\)");
                    let _ = writeln!(out, "/F1 12 Tf ({escaped}) Tj");
                } else {
                    out.push_str("/F2 12 Tf <");
                    for c in run.chars() {
                        let cid = cids.iter().position(|x| *x == c).unwrap_or(0) + 1;
                        let _ = write!(out, "{cid:04X}");
                    }
                    out.push_str("> Tj\n");
                }
                rest = tail;
            }
            out.push_str("0 -16 Td\n");
        }
        out.push_str("ET\n");
        out
    }

    fn cmap(cids: &[char]) -> String {
        let mut out = String::from(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
        );
        for (block, chunk) in cids.chunks(100).enumerate() {
            let _ = writeln!(out, "{} beginbfchar", chunk.len());
            for (i, c) in chunk.iter().enumerate() {
                let mut units = [0_u16; 2];
                let hex = c
                    .encode_utf16(&mut units)
                    .iter()
                    .fold(String::new(), |mut hex, u| {
                        let _ = write!(hex, "{u:04X}");
                        hex
                    });
                let _ = writeln!(out, "<{:04X}> <{hex}>", block * 100 + i + 1);
            }
            out.push_str("endbfchar\n");
        }
        out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
        out
    }

    fn stream(data: &[u8], compress: bool) -> Vec<u8> {
        let (data, filter) = if compress {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            z.write_all(data).expect("deflate");
            (z.finish().expect("deflate"), " /Filter /FlateDecode")
        } else {
            (data.to_vec(), "")
        };
        let mut out = format!("<< /Length {}{filter} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\nendstream");
        out
    }

    fn build(pages: &[&[&str]], compress: bool, encrypt: bool) -> Vec<u8> {
        let cids = cids(pages);
        let first_page = 7;
        let kids: Vec<String> = (0..pages.len())
            .map(|i| format!("{} 0 R", first_page + 2 * i))
            .collect();
        let mut objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.join(" "),
                pages.len()
            )
            .into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MingLiU /Encoding /Identity-H /DescendantFonts [5 0 R] /ToUnicode 6 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MingLiU /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> >>"
                .to_vec(),
            stream(cmap(&cids).as_bytes(), compress),
        ];
        for (i, lines) in pages.iter().enumerate() {
            objects.push(
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                    first_page + 2 * i + 1
                )
                .into_bytes(),
            );
            objects.push(stream(content(lines, &cids).as_bytes(), compress));
        }
        if encrypt {
            objects.push(
                format!(
                    "<< /Filter /Standard /V 1 /R 2 /Length 40 /P -4 /O <{o}> /U <{u}> >>",
                    o = "7A".repeat(32),
                    u = "3C".repeat(32)
                )
                .into_bytes(),
            );
        }
        let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            let _ = writeln!(table, "{offset:010} 00000 n ");
        }
        let encrypt = if encrypt {
            format!(
                " /Encrypt {} 0 R /ID [<{id}> <{id}>]",
                objects.len(),
                id = "01".repeat(16)
            )
        } else {
            String::new()
        };
        let _ = write!(
            table,
            "trailer\n<< /Size {} /Root 1 0 R{encrypt} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        );
        out.extend_from_slice(table.as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    fn lines(bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let none: Option<()> = search_text(bytes, &mut || false, &mut |line| {
            out.push(line.to_owned());
            None
        });
        assert!(none.is_none());
        out
    }

    fn find(bytes: &[u8], needle: &str) -> Option<String> {
        search_text(bytes, &mut || false, &mut |line| {
            line.contains(needle).then(|| line.to_owned())
        })
    }

    #[test]
    fn plain_and_compressed_pages_read_line_by_line() {
        let pages: &[&[&str]] = &[
            &["Quarterly report", "Total due: 42 (EUR)"],
            &["second page", "the needle is here"],
        ];
        for compress in [false, true] {
            let bytes = fixtures::pdf(pages, compress);
            assert_eq!(
                lines(&bytes),
                [
                    "Quarterly report",
                    "Total due: 42 (EUR)",
                    "second page",
                    "the needle is here"
                ],
                "compress {compress}"
            );
            assert_eq!(
                find(&bytes, "needle").as_deref(),
                Some("the needle is here")
            );
        }
    }

    #[test]
    fn cjk_text_maps_back_through_to_unicode() {
        let bytes = fixtures::pdf(&[&["發票 Invoice 2024", "總金額：新台幣 1,200 元"]], true);
        assert_eq!(
            find(&bytes, "總金額").as_deref(),
            Some("總金額：新台幣 1,200 元")
        );
        assert_eq!(
            find(&bytes, "發票").as_deref(),
            Some("發票 Invoice 2024"),
            "font switches inside a line stay on the line"
        );
    }

    #[test]
    fn password_protected_broken_and_non_pdf_files_are_no_match() {
        let protected = fixtures::protected_pdf(&[&["secret text"]]);
        assert_eq!(find(&protected, "secret"), None);
        let mut broken = fixtures::pdf(&[&["needle"]], true);
        broken.truncate(broken.len() / 2);
        let _ = find(&broken, "needle");
        assert_eq!(find(b"needle in plain text", "needle"), None);
        assert_eq!(find(b"%PDF-1.7\n garbage needle", "needle"), None);
    }

    #[test]
    fn stop_ends_the_search() {
        let bytes = fixtures::pdf(&[&["needle"]], false);
        let hit: Option<()> = search_text(&bytes[..], &mut || true, &mut |_| Some(()));
        assert_eq!(hit, None);
    }

    #[test]
    fn only_the_first_pages_are_searched() {
        let mut pages: Vec<&[&str]> = vec![&["filler"]; MAX_PAGES];
        pages.push(&["needle"]);
        let bytes = fixtures::pdf(&pages, true);
        assert_eq!(find(&bytes, "filler").as_deref(), Some("filler"));
        assert_eq!(find(&bytes, "needle"), None);
    }

    #[test]
    fn tj_gaps_become_spaces_and_forms_are_read() {
        let page = b"BT /F1 12 Tf 72 700 Td [(Hello)-20(World)-450(again)] TJ ET /X1 Do";
        let form = b"BT /F1 12 Tf 10 10 Td (inside a form) Tj ET";
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let form = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            },
            form.to_vec(),
        ));
        let content = doc.add_object(Stream::new(Dictionary::new(), page.to_vec()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content,
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => font },
                "XObject" => dictionary! { "X1" => form },
            },
        });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert_eq!(lines(&bytes), ["HelloWorld again", "inside a form"]);
    }
}
