//! Office Open XML previews: the text of `.docx` documents, the cells of `.xlsx` sheets and the
//! slides of `.pptx` presentations, read straight from the package (a zip of XML parts).
//!
//! Only what a preview needs is read, and every part is decompressed up to a cap, so a zip bomb
//! or a huge part ends in a truncated preview instead of unbounded memory or time. XML is read
//! with a streaming parser that never expands DTD entities. The old binary formats (`.doc`,
//! `.xls`, `.ppt`) aren't read at all. A password-protected OOXML file is an OLE compound file
//! wrapping the encrypted package, reported as [`OfficeError::Protected`].

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{self, Read, Seek, SeekFrom};

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Largest file previewed.
pub const MAX_FILE: u64 = 50 * 1024 * 1024;
/// Decompressed bytes read from a main part (document body, sheet, shared strings).
const MAX_PART: u64 = 4 * 1024 * 1024;
/// Decompressed bytes read from relationship, workbook and presentation parts.
const MAX_META: u64 = 1024 * 1024;
/// Decompressed bytes read from style parts.
const MAX_STYLES: u64 = 2 * 1024 * 1024;
/// Decompressed bytes read from one slide.
const MAX_SLIDE: u64 = 1024 * 1024;
/// Decompressed bytes read from all slides together.
const MAX_SLIDES_TOTAL: u64 = 16 * 1024 * 1024;
/// Largest embedded thumbnail read.
const MAX_THUMBNAIL: u64 = 4 * 1024 * 1024;
/// More parts than this isn't an Office document worth listing.
const MAX_PARTS: usize = 50_000;
/// Blocks kept from a document.
pub const MAX_BLOCKS: usize = 800;
/// Characters of text kept from a document.
const MAX_TEXT: usize = 200_000;
/// Rows kept from a document table.
const MAX_TABLE_ROWS: usize = 300;
/// Characters kept per table or sheet cell.
const MAX_CELL: usize = 500;
/// Rows of a sheet shown.
pub const SHEET_ROWS: u32 = 200;
/// Columns of a sheet shown.
pub const SHEET_COLS: u32 = 40;
/// Slides read from a presentation.
pub const MAX_SLIDES: usize = 300;
/// Text lines kept per slide.
const MAX_SLIDE_LINES: usize = 60;
/// Signature of an OLE compound file (old binary formats and encrypted OOXML).
const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Kind of Office Open XML file, by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfficeKind {
    /// Word document (`.docx`, `.docm`, `.dotx`, `.dotm`)
    Document,
    /// Excel workbook (`.xlsx`, `.xlsm`, `.xltx`, `.xltm`)
    Workbook,
    /// PowerPoint presentation (`.pptx`, `.pptm`, `.ppsx`, `.ppsm`, `.potx`, `.potm`)
    Presentation,
}

/// The Office Open XML kind of a file name. The old binary formats (`.doc`, `.xls`, `.ppt`)
/// aren't included.
pub fn office_kind(name: &str) -> Option<OfficeKind> {
    let (_, ext) = name.rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "docx" | "docm" | "dotx" | "dotm" => Some(OfficeKind::Document),
        "xlsx" | "xlsm" | "xltx" | "xltm" => Some(OfficeKind::Workbook),
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => Some(OfficeKind::Presentation),
        _ => None,
    }
}

/// Why an Office file can't be previewed.
#[derive(Debug, thiserror::Error)]
pub enum OfficeError {
    /// Larger than [`MAX_FILE`].
    #[error("the file is too large to preview")]
    TooLarge,
    /// An encrypted package (an OLE compound file instead of a zip).
    #[error("the document is password-protected")]
    Protected,
    /// Not a zip, a required part is missing, or its XML is broken.
    #[error("not a readable Office Open XML package")]
    Malformed,
    /// Reading the file failed.
    #[error("can't read the file: {0}")]
    Io(#[from] io::Error),
}

// ───────────────────────── Package ─────────────────────────

/// A part's decompressed bytes, and whether the cap cut it short.
struct Part {
    bytes: Vec<u8>,
    truncated: bool,
}

/// A relationship from one part to another.
struct Rel {
    id: String,
    kind: String,
    /// Absolute part name (no leading slash)
    target: String,
}

struct Package<R: Read + Seek> {
    zip: zip::ZipArchive<R>,
}

impl<R: Read + Seek> Package<R> {
    fn open(mut reader: R) -> Result<Self, OfficeError> {
        let len = reader.seek(SeekFrom::End(0))?;
        if len > MAX_FILE {
            return Err(OfficeError::TooLarge);
        }
        reader.seek(SeekFrom::Start(0))?;
        let mut magic = [0_u8; 8];
        let mut got = 0;
        while got < magic.len() {
            match reader.read(&mut magic[got..])? {
                0 => break,
                n => got += n,
            }
        }
        if got == magic.len() && magic == OLE_MAGIC {
            return Err(OfficeError::Protected);
        }
        reader.seek(SeekFrom::Start(0))?;
        let zip = zip::ZipArchive::new(reader).map_err(|_| OfficeError::Malformed)?;
        if zip.len() > MAX_PARTS {
            return Err(OfficeError::Malformed);
        }
        Ok(Self { zip })
    }

    /// Index of a part (OPC part names compare case-insensitively).
    fn find(&self, name: &str) -> Option<usize> {
        self.zip.index_for_name(name).or_else(|| {
            self.zip
                .file_names()
                .position(|n| n.is_ok_and(|n| n.eq_ignore_ascii_case(name)))
        })
    }

    /// A part decompressed up to `cap` bytes, or `None` if the package doesn't have it.
    fn read(&mut self, name: &str, cap: u64) -> Result<Option<Part>, OfficeError> {
        let Some(ix) = self.find(name) else {
            return Ok(None);
        };
        let file = self.zip.by_index(ix).map_err(|_| OfficeError::Malformed)?;
        let mut bytes = Vec::new();
        file.take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| OfficeError::Malformed)?;
        let truncated = bytes.len() as u64 > cap;
        bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
        Ok(Some(Part { bytes, truncated }))
    }

    /// Relationships of `part` ("" for the package's own).
    fn rels(&mut self, part: &str) -> Result<Vec<Rel>, OfficeError> {
        let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
        let name = if dir.is_empty() {
            format!("_rels/{file}.rels")
        } else {
            format!("{dir}/_rels/{file}.rels")
        };
        let Some(xml) = self.read(&name, MAX_META)? else {
            return Ok(Vec::new());
        };
        let mut rels = Vec::new();
        let mut reader = Xml::new(&xml);
        while let Some(event) = reader.next()? {
            let (Event::Start(e) | Event::Empty(e)) = event else {
                continue;
            };
            if e.local_name().as_ref() != b"Relationship"
                || attr(&e, b"TargetMode").is_some_and(|m| m.eq_ignore_ascii_case("external"))
            {
                continue;
            }
            if let (Some(id), Some(target)) = (attr(&e, b"Id"), attr(&e, b"Target")) {
                rels.push(Rel {
                    id,
                    kind: attr(&e, b"Type").unwrap_or_default(),
                    target: resolve(dir, &target),
                });
            }
        }
        Ok(rels)
    }

    /// The main part (`officeDocument` relationship of the package), or `default`.
    fn main_part(&mut self, default: &str) -> Result<String, OfficeError> {
        let rels = self.rels("")?;
        Ok(rel_of(&rels, "/officeDocument").unwrap_or_else(|| default.to_owned()))
    }
}

/// Target of the first relationship whose type ends with `suffix`.
fn rel_of(rels: &[Rel], suffix: &str) -> Option<String> {
    rels.iter()
        .find(|r| r.kind.ends_with(suffix))
        .map(|r| r.target.clone())
}

/// A relationship target as an absolute part name (`..` resolved, no leading slash).
fn resolve(dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let joined;
    let path = if let Some(abs) = target.strip_prefix('/') {
        abs
    } else {
        joined = if dir.is_empty() {
            target.to_owned()
        } else {
            format!("{dir}/{target}")
        };
        &joined
    };
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            seg => parts.push(seg),
        }
    }
    parts.join("/")
}

/// Streaming XML events from a part. A syntax error in a part the cap cut short is just the cut,
/// so it ends the part; in a complete part it's [`OfficeError::Malformed`].
struct Xml<'a> {
    reader: Reader<&'a [u8]>,
    truncated: bool,
}

impl<'a> Xml<'a> {
    fn new(part: &'a Part) -> Self {
        let bytes = part
            .bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .unwrap_or(&part.bytes);
        Self {
            reader: Reader::from_reader(bytes),
            truncated: part.truncated,
        }
    }

    fn next(&mut self) -> Result<Option<Event<'a>>, OfficeError> {
        match self.reader.read_event() {
            Ok(Event::Eof) => Ok(None),
            Ok(event) => Ok(Some(event)),
            Err(_) if self.truncated => Ok(None),
            Err(_) => Err(OfficeError::Malformed),
        }
    }
}

/// An attribute by local name (the prefix is ignored), unescaped.
fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    let mut attrs = e.attributes();
    attrs.with_checks(false);
    attrs
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(Cow::into_owned)
        })
}

/// A WordprocessingML toggle (`<w:b/>`, `<w:b w:val="0"/>`).
fn toggle(e: &BytesStart) -> bool {
    attr(e, b"val").is_none_or(|v| !matches!(v.as_str(), "0" | "false" | "off" | "none"))
}

/// The character a reference (`&amp;`, `&#x41;`) stands for; unknown entities are dropped.
fn ref_char(r: &BytesRef) -> Option<char> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c);
    }
    match &**r {
        b"lt" => Some('<'),
        b"gt" => Some('>'),
        b"amp" => Some('&'),
        b"apos" => Some('\''),
        b"quot" => Some('"'),
        _ => None,
    }
}

/// Text of a text or reference event, if it is one.
fn event_text(event: &Event) -> Option<String> {
    match event {
        Event::Text(t) => t.xml10_content().ok().map(Cow::into_owned),
        Event::CData(t) => t.decode().ok().map(Cow::into_owned),
        Event::GeneralRef(r) => ref_char(r).map(String::from),
        _ => None,
    }
}

/// Appends up to `max` characters in total.
fn push_capped(out: &mut String, text: &str, max: usize) {
    let have = out.chars().count();
    if have < max {
        out.extend(text.chars().take(max - have));
    }
}

// ───────────────────────── Documents ─────────────────────────

/// A run of text with one formatting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The text (line breaks as `\n`, tabs as spaces)
    pub text: String,
    /// Bold
    pub bold: bool,
    /// Italic
    pub italic: bool,
}

/// A block of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A heading, level 1 (title, Heading 1) and deeper
    Heading {
        /// 1-based level
        level: u8,
        /// The text
        spans: Vec<Span>,
    },
    /// A paragraph; empty for a blank line
    Paragraph(Vec<Span>),
    /// A bulleted or numbered list item (both shown as bullets)
    ListItem {
        /// Nesting depth, 0 for the outermost
        depth: u8,
        /// The text
        spans: Vec<Span>,
    },
    /// A table: rows of cell texts
    Table(Vec<Vec<String>>),
    /// An explicit page break
    PageBreak,
    /// An image or other drawing (not rendered)
    Image,
}

/// What a document preview shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    /// The blocks in order
    pub blocks: Vec<Block>,
    /// The body was longer than the preview reads
    pub truncated: bool,
}

/// What a paragraph style contributes.
#[derive(Clone, Debug, Default)]
struct ParaStyle {
    heading: Option<u8>,
    list: Option<bool>,
    based_on: Option<String>,
}

/// Heading level from a style name ("heading 2", "Title").
fn heading_from_name(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase();
    if name == "title" {
        return Some(1);
    }
    let rest = name.strip_prefix("heading")?.trim_start();
    rest.parse::<u8>().ok().filter(|l| (1..=9).contains(l))
}

/// Paragraph styles from `styles.xml` by style id.
fn parse_styles(part: &Part) -> Result<HashMap<String, ParaStyle>, OfficeError> {
    let mut styles = HashMap::new();
    let mut current: Option<(String, ParaStyle)> = None;
    let mut in_numpr = false;
    let mut xml = Xml::new(part);
    while let Some(event) = xml.next()? {
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                let name = e.local_name();
                match name.as_ref() {
                    b"style" => {
                        let paragraph = attr(e, b"type").is_none_or(|t| t == "paragraph");
                        current = attr(e, b"styleId")
                            .filter(|_| paragraph)
                            .map(|id| (id, ParaStyle::default()));
                    }
                    b"name" => {
                        if let Some((_, style)) = current.as_mut()
                            && let Some(level) = attr(e, b"val").and_then(|n| heading_from_name(&n))
                        {
                            style.heading = Some(level);
                        }
                    }
                    b"basedOn" => {
                        if let Some((_, style)) = current.as_mut() {
                            style.based_on = attr(e, b"val");
                        }
                    }
                    b"outlineLvl" => {
                        if let Some((_, style)) = current.as_mut()
                            && let Some(level) = outline_level(e)
                        {
                            style.heading.get_or_insert(level);
                        }
                    }
                    b"numPr" => {
                        in_numpr = !empty;
                        if let Some((_, style)) = current.as_mut() {
                            style.list = Some(true);
                        }
                    }
                    b"numId" if in_numpr => {
                        if let Some((_, style)) = current.as_mut()
                            && attr(e, b"val").as_deref() == Some("0")
                        {
                            style.list = Some(false);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"style" => {
                    if let Some((id, style)) = current.take() {
                        styles.insert(id, style);
                    }
                }
                b"numPr" => in_numpr = false,
                _ => {}
            },
            _ => {}
        }
    }
    Ok(styles)
}

/// `<w:outlineLvl w:val="0"/>` as a 1-based heading level (9 is body text).
fn outline_level(e: &BytesStart) -> Option<u8> {
    attr(e, b"val")?
        .parse::<u8>()
        .ok()
        .filter(|&v| v < 9)
        .map(|v| v + 1)
}

/// Heading level and list flag of a style, following `basedOn`.
fn style_info(styles: &HashMap<String, ParaStyle>, id: &str) -> (Option<u8>, bool) {
    let mut heading = None;
    let mut list = None;
    let mut next = Some(id.to_owned());
    for _ in 0..8 {
        let Some(id) = next.take() else { break };
        let Some(style) = styles.get(&id) else {
            // No styles part: the built-in ids ("Heading1", "Title") still say enough.
            heading = heading.or_else(|| heading_from_name(&id));
            break;
        };
        heading = heading.or(style.heading);
        list = list.or(style.list);
        next.clone_from(&style.based_on);
    }
    (heading, list.unwrap_or(false))
}

#[derive(Default)]
struct Para {
    spans: Vec<Span>,
    style: Option<String>,
    outline: Option<u8>,
    list: Option<u8>,
    no_list: bool,
    image: bool,
    break_before: bool,
}

#[derive(Default)]
struct TableBuild {
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    cell: Option<String>,
}

// The flags track where the reader is (inside pPr, rPr, numPr, w:t) and the run's toggles.
#[allow(clippy::struct_excessive_bools)]
struct DocParser<'s> {
    styles: &'s HashMap<String, ParaStyle>,
    blocks: Vec<Block>,
    text_len: usize,
    full: bool,
    para: Option<Para>,
    tables: Vec<TableBuild>,
    in_ppr: bool,
    in_rpr: bool,
    in_numpr: bool,
    in_t: bool,
    bold: bool,
    italic: bool,
    /// Depth inside a subtree being skipped (drawings, fallbacks, section properties)
    skip: usize,
}

impl DocParser<'_> {
    fn start(&mut self, e: &BytesStart, empty: bool) {
        let name = e.local_name();
        match name.as_ref() {
            b"p" => {
                self.para = Some(Para::default());
                self.in_ppr = false;
            }
            b"pPr" => self.in_ppr = !empty,
            b"pStyle" if self.in_ppr => {
                if let Some(para) = self.para.as_mut() {
                    para.style = attr(e, b"val");
                }
            }
            b"outlineLvl" if self.in_ppr => {
                if let Some(para) = self.para.as_mut() {
                    para.outline = outline_level(e);
                }
            }
            b"numPr" if self.in_ppr => {
                self.in_numpr = !empty;
                if let Some(para) = self.para.as_mut() {
                    para.list.get_or_insert(0);
                }
            }
            b"ilvl" if self.in_numpr => {
                if let Some(para) = self.para.as_mut() {
                    let level = attr(e, b"val").and_then(|v| v.parse::<u8>().ok());
                    para.list = Some(level.unwrap_or(0).min(8));
                }
            }
            b"numId" if self.in_numpr => {
                if let Some(para) = self.para.as_mut()
                    && attr(e, b"val").as_deref() == Some("0")
                {
                    para.no_list = true;
                }
            }
            b"pageBreakBefore" if self.in_ppr => {
                if let Some(para) = self.para.as_mut() {
                    para.break_before = toggle(e);
                }
            }
            b"rPr" => self.in_rpr = !empty,
            b"r" => {
                self.bold = false;
                self.italic = false;
            }
            b"b" if self.in_rpr && !self.in_ppr => self.bold = toggle(e),
            b"i" if self.in_rpr && !self.in_ppr => self.italic = toggle(e),
            b"t" if !self.in_ppr => self.in_t = !empty,
            b"tab" | b"ptab" if !self.in_ppr => self.push_text("    "),
            b"br" if !self.in_ppr => {
                if attr(e, b"type").as_deref() == Some("page") {
                    self.page_break();
                } else {
                    self.push_text("\n");
                }
            }
            b"cr" if !self.in_ppr => self.push_text("\n"),
            b"noBreakHyphen" => self.push_text("-"),
            b"drawing" | b"pict" | b"object" => {
                if let Some(para) = self.para.as_mut() {
                    para.image = true;
                }
                if !empty {
                    self.skip = 1;
                }
            }
            b"Fallback" | b"sectPr" | b"txbxContent" if !empty => self.skip = 1,
            b"tbl" => self.tables.push(TableBuild::default()),
            b"tr" => {
                if let Some(table) = self.tables.last_mut() {
                    table.row = Some(Vec::new());
                }
            }
            b"tc" => {
                if let Some(table) = self.tables.last_mut() {
                    table.cell = Some(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &[u8]) {
        match name {
            b"pPr" => self.in_ppr = false,
            b"rPr" => self.in_rpr = false,
            b"numPr" => self.in_numpr = false,
            b"t" => self.in_t = false,
            b"p" => self.finish_para(),
            b"tc" => {
                if let Some(table) = self.tables.last_mut()
                    && let Some(cell) = table.cell.take()
                {
                    table.row.get_or_insert_default().push(cell);
                }
            }
            b"tr" => {
                if let Some(table) = self.tables.last_mut()
                    && let Some(row) = table.row.take()
                {
                    if table.rows.len() < MAX_TABLE_ROWS {
                        table.rows.push(row);
                    } else {
                        self.full = true;
                    }
                }
            }
            b"tbl" => {
                let Some(table) = self.tables.pop() else {
                    return;
                };
                if let Some(outer) = self.tables.last_mut() {
                    // A table in a cell: its text goes into the outer cell.
                    let text: Vec<String> = table.rows.iter().map(|r| r.join(" ")).collect();
                    let cell = outer.cell.get_or_insert_default();
                    if !cell.is_empty() {
                        cell.push('\n');
                    }
                    push_capped(cell, &text.join("\n"), MAX_CELL);
                } else if !table.rows.is_empty() {
                    self.push(Block::Table(table.rows));
                }
            }
            _ => {}
        }
    }

    fn push(&mut self, block: Block) {
        if self.blocks.len() >= MAX_BLOCKS {
            self.full = true;
        } else {
            self.blocks.push(block);
        }
    }

    fn push_text(&mut self, text: &str) {
        let Some(para) = self.para.as_mut() else {
            return;
        };
        self.text_len += text.len();
        if self.text_len > MAX_TEXT {
            self.full = true;
            return;
        }
        let (bold, italic) = (self.bold, self.italic);
        match para.spans.last_mut() {
            Some(last) if last.bold == bold && last.italic == italic => last.text.push_str(text),
            _ => para.spans.push(Span {
                text: text.to_owned(),
                bold,
                italic,
            }),
        }
    }

    /// `<w:br w:type="page"/>`: the text so far becomes its own block, then the break.
    fn page_break(&mut self) {
        if !self.tables.is_empty() {
            return;
        }
        let Some(para) = self.para.as_mut() else {
            return;
        };
        let spans = std::mem::take(&mut para.spans);
        if spans.iter().any(|s| !s.text.trim().is_empty()) {
            let block = self.block_for(spans);
            self.push(block);
        }
        if self.blocks.last().is_some_and(|b| *b != Block::PageBreak) {
            self.push(Block::PageBreak);
        }
    }

    /// The block for the current paragraph's properties and `spans`.
    fn block_for(&self, spans: Vec<Span>) -> Block {
        let Some(para) = self.para.as_ref() else {
            return Block::Paragraph(spans);
        };
        let (style_heading, style_list) = para
            .style
            .as_deref()
            .map_or((None, false), |id| style_info(self.styles, id));
        if let Some(level) = para.outline.or(style_heading) {
            return Block::Heading { level, spans };
        }
        let depth = para.list.or_else(|| style_list.then_some(0));
        match depth {
            Some(depth) if !para.no_list => Block::ListItem { depth, spans },
            _ => Block::Paragraph(spans),
        }
    }

    fn finish_para(&mut self) {
        let Some(mut para) = self.para.take() else {
            return;
        };
        let spans = std::mem::take(&mut para.spans);
        if let Some(table) = self.tables.last_mut() {
            let cell = table.cell.get_or_insert_default();
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            if !cell.is_empty() {
                cell.push('\n');
            }
            push_capped(cell, text.trim_end(), MAX_CELL);
            return;
        }
        if para.break_before && self.blocks.last().is_some_and(|b| *b != Block::PageBreak) {
            self.push(Block::PageBreak);
        }
        let blank = spans.iter().all(|s| s.text.trim().is_empty());
        let image = para.image;
        self.para = Some(para);
        if !blank {
            let block = self.block_for(spans);
            self.push(block);
        } else if !image
            && self
                .blocks
                .last()
                .is_some_and(|b| !matches!(b, Block::Paragraph(s) if s.is_empty()))
        {
            // Blank lines keep their spacing, but only one in a row and none at the start.
            self.push(Block::Paragraph(Vec::new()));
        }
        if image {
            self.push(Block::Image);
        }
        self.para = None;
    }
}

/// Reads the body of a `.docx` (or `.docm`, `.dotx`).
pub fn read_document(reader: impl Read + Seek) -> Result<Document, OfficeError> {
    let mut pkg = Package::open(reader)?;
    let main = pkg.main_part("word/document.xml")?;
    let rels = pkg.rels(&main)?;
    let styles_part = rel_of(&rels, "/styles").unwrap_or_else(|| "word/styles.xml".to_owned());
    // Broken styles only lose heading levels; the text still shows.
    let styles = pkg
        .read(&styles_part, MAX_STYLES)?
        .and_then(|part| parse_styles(&part).ok())
        .unwrap_or_default();
    let part = pkg.read(&main, MAX_PART)?.ok_or(OfficeError::Malformed)?;
    let mut doc = parse_document(&part, &styles)?;
    doc.truncated |= part.truncated;
    Ok(doc)
}

fn parse_document(
    part: &Part,
    styles: &HashMap<String, ParaStyle>,
) -> Result<Document, OfficeError> {
    let mut p = DocParser {
        styles,
        blocks: Vec::new(),
        text_len: 0,
        full: false,
        para: None,
        tables: Vec::new(),
        in_ppr: false,
        in_rpr: false,
        in_numpr: false,
        in_t: false,
        bold: false,
        italic: false,
        skip: 0,
    };
    let mut xml = Xml::new(part);
    while let Some(event) = xml.next()? {
        if p.full {
            break;
        }
        if p.skip > 0 {
            match event {
                Event::Start(_) => p.skip += 1,
                Event::End(_) => p.skip -= 1,
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(e) => p.start(e, false),
            Event::Empty(e) => {
                p.start(e, true);
                p.end(e.local_name().as_ref());
            }
            Event::End(e) => p.end(e.local_name().as_ref()),
            _ if p.in_t => {
                if let Some(text) = event_text(&event) {
                    p.push_text(&text);
                }
            }
            _ => {}
        }
    }
    while matches!(p.blocks.last(), Some(Block::Paragraph(s)) if s.is_empty()) {
        p.blocks.pop();
    }
    Ok(Document {
        blocks: p.blocks,
        truncated: p.full,
    })
}

// ───────────────────────── Workbooks ─────────────────────────

/// One cell's display text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// The text, formatted for display (one line)
    pub text: String,
    /// A number (shown right-aligned)
    pub numeric: bool,
}

/// The top-left part of a sheet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sheet {
    /// Rows from row 1, each with cells from column A, up to the last used row and column
    /// (at most [`SHEET_ROWS`] × [`SHEET_COLS`])
    pub rows: Vec<Vec<Cell>>,
    /// The sheet has rows past [`SHEET_ROWS`]
    pub more_rows: bool,
    /// The sheet has columns past [`SHEET_COLS`]
    pub more_cols: bool,
    /// Rows in the sheet per its stored dimension, when known
    pub total_rows: Option<u32>,
    /// A part was larger than the preview reads (cells may be missing)
    pub truncated: bool,
}

impl Sheet {
    /// Columns shown (the widest row).
    pub fn columns(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// A workbook's sheet names and one sheet's cells.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workbook {
    /// Names of the visible sheets, in tab order
    pub names: Vec<String>,
    /// Which of them `sheet` is
    pub index: usize,
    /// The cells of sheet `index`
    pub sheet: Sheet,
}

/// Column letters for a 0-based column index (0 → A, 26 → AA).
pub fn column_name(index: u32) -> String {
    let mut n = index + 1;
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push(char::from(b'A' + u8::try_from(rem).unwrap_or(0)));
        n = (n - 1) / 26;
    }
    out.iter().rev().collect()
}

/// A cell reference ("B12") as 1-based (column, row); either may be missing ("B", "12").
fn parse_ref(r: &str) -> (Option<u32>, Option<u32>) {
    let r = r.trim_start_matches('$');
    let split = r
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(r.len());
    let (letters, digits) = r.split_at(split);
    let col = if letters.is_empty() || letters.len() > 3 {
        None
    } else {
        letters.bytes().try_fold(0_u32, |acc, b| {
            Some(acc * 26 + u32::from(b.to_ascii_uppercase().checked_sub(b'A')?) + 1)
        })
    };
    (col, digits.trim_start_matches('$').parse().ok())
}

/// How a cell's number is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// As stored
    General,
    /// Times 100 with a percent sign and this many decimals
    Percent(usize),
    /// A date and/or time of day
    Date {
        date: bool,
        time: bool,
        seconds: bool,
    },
}

/// Built-in number format ids (ECMA-376 §18.8.30, plus the East Asian date ids).
const fn builtin_format(id: u32) -> Format {
    const DATE: Format = Format::Date {
        date: true,
        time: false,
        seconds: false,
    };
    match id {
        9 => Format::Percent(0),
        10 => Format::Percent(2),
        14..=17 | 27..=31 | 36 | 50..=58 => DATE,
        22 => Format::Date {
            date: true,
            time: true,
            seconds: false,
        },
        18 | 20 | 32 | 33 => Format::Date {
            date: false,
            time: true,
            seconds: false,
        },
        19 | 21 | 34 | 35 | 45..=47 => Format::Date {
            date: false,
            time: true,
            seconds: true,
        },
        _ => Format::General,
    }
}

/// Classifies a custom format code ("yyyy-mm-dd", "0.0%", "#,##0.00").
fn classify_format(code: &str) -> Format {
    // First section only; drop quoted text, escapes, fills and [color]/[$-locale] brackets.
    let mut plain = String::new();
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            ';' => break,
            '"' => {
                for q in chars.by_ref() {
                    if q == '"' {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            '[' => {
                let inner: String = chars.by_ref().take_while(|&b| b != ']').collect();
                // Elapsed time ([h]:mm) keeps its letter.
                if inner
                    .chars()
                    .all(|c| matches!(c.to_ascii_lowercase(), 'h' | 'm' | 's'))
                {
                    plain.push_str(&inner);
                }
            }
            c => plain.push(c.to_ascii_lowercase()),
        }
    }
    if plain.contains("general") || plain.trim() == "@" {
        return Format::General;
    }
    let has = |c: char| plain.contains(c);
    let digits = has('0') || has('#') || has('?');
    let date = has('y') || has('d') || (has('m') && !has('h') && !has('s') && !digits);
    let time = has('h') || has('s');
    if date || time {
        return Format::Date {
            date,
            time,
            seconds: has('s'),
        };
    }
    if let Some(pct) = plain.find('%') {
        let decimals = plain[..pct].rsplit_once('.').map_or(0, |(_, d)| {
            d.chars().filter(|&c| c == '0' || c == '#').count()
        });
        return Format::Percent(decimals.min(10));
    }
    Format::General
}

/// Number format per cell style index, from `styles.xml`.
fn parse_number_formats(part: &Part) -> Result<Vec<Format>, OfficeError> {
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut xf_ids: Vec<u32> = Vec::new();
    let mut in_xfs = false;
    let mut xml = Xml::new(part);
    while let Some(event) = xml.next()? {
        match event {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"numFmt" => {
                    if let (Some(id), Some(code)) = (
                        attr(&e, b"numFmtId").and_then(|v| v.parse().ok()),
                        attr(&e, b"formatCode"),
                    ) {
                        custom.insert(id, code);
                    }
                }
                b"cellXfs" => in_xfs = true,
                b"xf" if in_xfs => xf_ids.push(
                    attr(&e, b"numFmtId")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                ),
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == b"cellXfs" => in_xfs = false,
            _ => {}
        }
    }
    Ok(xf_ids
        .into_iter()
        .map(|id| {
            custom
                .get(&id)
                .map_or_else(|| builtin_format(id), |c| classify_format(c))
        })
        .collect())
}

/// Civil date from days since 1970-01-01 (proleptic Gregorian).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m, d)
}

/// A date serial as "2024-03-15", "13:45" or both. `None` for values no date format shows.
fn format_date(
    value: f64,
    date1904: bool,
    (date, time, seconds): (bool, bool, bool),
) -> Option<String> {
    // Excel shows #### for negative dates and anything past 9999-12-31.
    if !value.is_finite() || !(0.0..2_958_466.0).contains(&value) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let mut days = value.floor() as i64;
    #[allow(clippy::cast_possible_truncation)]
    let mut secs = ((value - value.floor()) * 86_400.0).round() as i64;
    if secs >= 86_400 {
        days += 1;
        secs -= 86_400;
    }
    let mut out = String::new();
    if date {
        let (y, m, d) = if date1904 {
            civil_from_days(days - 24_107)
        } else if days == 60 {
            // Lotus' phantom 1900-02-29, kept by Excel
            (1900, 2, 29)
        } else if days == 0 {
            (1900, 1, 0)
        } else if days < 60 {
            civil_from_days(days - 25_568)
        } else {
            civil_from_days(days - 25_569)
        };
        let _ = write!(out, "{y:04}-{m:02}-{d:02}");
    }
    if time {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "{:02}:{:02}", secs / 3600, secs / 60 % 60);
        if seconds {
            let _ = write!(out, ":{:02}", secs % 60);
        }
    }
    Some(out)
}

/// A stored number shown with its cell's format.
fn format_number(raw: &str, format: Format, date1904: bool) -> String {
    let Ok(value) = raw.trim().parse::<f64>() else {
        return raw.to_owned();
    };
    match format {
        Format::General => raw.to_owned(),
        Format::Percent(decimals) => format!("{:.decimals$}%", value * 100.0),
        Format::Date {
            date,
            time,
            seconds,
        } => format_date(value, date1904, (date, time, seconds)).unwrap_or_else(|| raw.to_owned()),
    }
}

/// A cell as stored.
enum RawValue {
    Number(String),
    Shared(usize),
    Text(String),
}

struct RawCell {
    row: u32,
    col: u32,
    value: RawValue,
    style: usize,
}

/// Cells of a sheet part in the first [`SHEET_ROWS`] × [`SHEET_COLS`].
#[allow(clippy::too_many_lines)]
fn parse_sheet(part: &Part, sheet: &mut Sheet) -> Result<Vec<RawCell>, OfficeError> {
    struct Pending {
        row: u32,
        col: u32,
        kind: Option<String>,
        style: usize,
        value: String,
        inline: String,
    }
    let mut cells = Vec::new();
    let mut row = 0_u32;
    let mut col = 0_u32;
    let mut cell: Option<Pending> = None;
    let (mut in_v, mut in_is, mut in_t) = (false, false, false);
    let mut skip = 0_usize;
    let mut xml = Xml::new(part);
    let finish = |cell: Pending, cells: &mut Vec<RawCell>| {
        let value = match cell.kind.as_deref() {
            Some("s") => match cell.value.trim().parse() {
                Ok(ix) => RawValue::Shared(ix),
                Err(_) => return,
            },
            Some("inlineStr") => RawValue::Text(cell.inline),
            Some("b") => RawValue::Text(
                if cell.value.trim() == "1" {
                    "TRUE"
                } else {
                    "FALSE"
                }
                .to_owned(),
            ),
            Some("str" | "e") => RawValue::Text(cell.value),
            Some("d") => RawValue::Text(cell.value.trim_end_matches("T00:00:00").to_owned()),
            _ if cell.value.trim().is_empty() => return,
            _ => RawValue::Number(cell.value),
        };
        if matches!(&value, RawValue::Text(t) if t.is_empty()) {
            return;
        }
        cells.push(RawCell {
            row: cell.row,
            col: cell.col,
            value,
            style: cell.style,
        });
    };
    while let Some(event) = xml.next()? {
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"dimension" => {
                        if let Some(r) = attr(e, b"ref") {
                            let last = r.rsplit(':').next().unwrap_or(&r);
                            sheet.total_rows = parse_ref(last).1;
                        }
                    }
                    b"row" => {
                        row = attr(e, b"r")
                            .and_then(|r| r.parse().ok())
                            .unwrap_or(row + 1);
                        col = 0;
                        if row > SHEET_ROWS {
                            sheet.more_rows = true;
                            break;
                        }
                    }
                    b"c" => {
                        let (c, r) = attr(e, b"r").map_or((None, None), |r| parse_ref(&r));
                        col = c.unwrap_or(col + 1);
                        let at_row = r.unwrap_or(row);
                        if col > SHEET_COLS {
                            sheet.more_cols = true;
                            cell = None;
                        } else if at_row <= SHEET_ROWS && at_row > 0 {
                            let pending = Pending {
                                row: at_row,
                                col,
                                kind: attr(e, b"t"),
                                style: attr(e, b"s").and_then(|s| s.parse().ok()).unwrap_or(0),
                                value: String::new(),
                                inline: String::new(),
                            };
                            if empty {
                                finish(pending, &mut cells);
                            } else {
                                cell = Some(pending);
                            }
                        }
                    }
                    b"v" if !empty => in_v = cell.is_some(),
                    b"is" if !empty => in_is = cell.is_some(),
                    b"t" if in_is && !empty => in_t = true,
                    b"rPh" if !empty => skip = 1,
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"v" => in_v = false,
                b"is" => in_is = false,
                b"t" => in_t = false,
                b"c" => {
                    if let Some(pending) = cell.take() {
                        finish(pending, &mut cells);
                    }
                }
                b"sheetData" => break,
                _ => {}
            },
            _ => {
                if let Some(pending) = cell.as_mut()
                    && (in_v || in_t)
                    && let Some(text) = event_text(&event)
                {
                    let target = if in_t {
                        &mut pending.inline
                    } else {
                        &mut pending.value
                    };
                    push_capped(target, &text, MAX_CELL);
                }
            }
        }
    }
    Ok(cells)
}

/// The shared strings with the given indices.
fn parse_shared_strings(
    part: &Part,
    wanted: &BTreeSet<usize>,
) -> Result<HashMap<usize, String>, OfficeError> {
    let mut found = HashMap::new();
    let Some(&last) = wanted.last() else {
        return Ok(found);
    };
    let mut ix = 0_usize;
    let mut current: Option<String> = None;
    let mut in_t = false;
    let mut skip = 0_usize;
    let mut xml = Xml::new(part);
    while let Some(event) = xml.next()? {
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(e) => match e.local_name().as_ref() {
                b"si" => current = Some(String::new()),
                b"t" => in_t = current.is_some(),
                b"rPh" => skip = 1,
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == b"si" => {
                ix += 1;
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_t = false,
                b"si" => {
                    if let Some(text) = current.take()
                        && wanted.contains(&ix)
                    {
                        found.insert(ix, text);
                    }
                    if ix >= last {
                        break;
                    }
                    ix += 1;
                }
                _ => {}
            },
            _ if in_t => {
                if let (Some(current), Some(text)) = (current.as_mut(), event_text(&event)) {
                    push_capped(current, &text, MAX_CELL);
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

/// One sheet entry of `workbook.xml`.
struct SheetRef {
    name: String,
    rel: String,
    hidden: bool,
}

/// Reads a workbook's sheet names and the cells of visible sheet `index` (the first if out of
/// range).
// One pass: sheet list, the sheet's cells, then only the styles and shared strings it uses.
#[allow(clippy::too_many_lines)]
pub fn read_workbook(reader: impl Read + Seek, index: usize) -> Result<Workbook, OfficeError> {
    let mut pkg = Package::open(reader)?;
    let main = pkg.main_part("xl/workbook.xml")?;
    let part = pkg.read(&main, MAX_META)?.ok_or(OfficeError::Malformed)?;
    let mut sheet_refs = Vec::new();
    let mut date1904 = false;
    let mut xml = Xml::new(&part);
    while let Some(event) = xml.next()? {
        let (Event::Start(e) | Event::Empty(e)) = event else {
            continue;
        };
        match e.local_name().as_ref() {
            b"sheet" => {
                if let (Some(name), Some(rel)) = (attr(&e, b"name"), attr(&e, b"id")) {
                    let hidden = attr(&e, b"state").is_some_and(|s| s != "visible");
                    sheet_refs.push(SheetRef { name, rel, hidden });
                }
            }
            b"workbookPr" => {
                date1904 = attr(&e, b"date1904").is_some_and(|v| v == "1" || v == "true");
            }
            _ => {}
        }
    }
    if sheet_refs.iter().any(|s| !s.hidden) {
        sheet_refs.retain(|s| !s.hidden);
    }
    let mut book = Workbook {
        names: sheet_refs.iter().map(|s| s.name.clone()).collect(),
        index: index.min(sheet_refs.len().saturating_sub(1)),
        sheet: Sheet::default(),
    };
    let Some(sheet_ref) = sheet_refs.get(book.index) else {
        return Ok(book);
    };
    let rels = pkg.rels(&main)?;
    let Some(target) = rels.iter().find(|r| r.id == sheet_ref.rel) else {
        return Ok(book);
    };
    let target = target.target.clone();
    let Some(part) = pkg.read(&target, MAX_PART)? else {
        return Ok(book);
    };
    let mut sheet = Sheet {
        truncated: part.truncated,
        ..Sheet::default()
    };
    let cells = parse_sheet(&part, &mut sheet)?;
    drop(part);
    let formats = match rel_of(&rels, "/styles") {
        Some(styles) => pkg
            .read(&styles, MAX_STYLES)?
            .and_then(|part| parse_number_formats(&part).ok())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let wanted: BTreeSet<usize> = cells
        .iter()
        .filter_map(|c| match c.value {
            RawValue::Shared(ix) => Some(ix),
            _ => None,
        })
        .collect();
    let mut shared = HashMap::new();
    if !wanted.is_empty()
        && let Some(sst) = rel_of(&rels, "/sharedStrings")
        && let Some(part) = pkg.read(&sst, MAX_PART)?
    {
        sheet.truncated |= part.truncated;
        shared = parse_shared_strings(&part, &wanted)?;
    }
    // Dense grid up to the last used row and column.
    let mut grid: BTreeMap<u32, BTreeMap<u32, Cell>> = BTreeMap::new();
    for cell in cells {
        let (text, numeric) = match cell.value {
            RawValue::Number(raw) => {
                let format = formats.get(cell.style).copied().unwrap_or(Format::General);
                (format_number(&raw, format, date1904), true)
            }
            RawValue::Shared(ix) => (shared.get(&ix).cloned().unwrap_or_default(), false),
            RawValue::Text(text) => (text, false),
        };
        let text = text.replace(['\r', '\n', '\t'], " ");
        if text.is_empty() {
            continue;
        }
        grid.entry(cell.row)
            .or_default()
            .insert(cell.col, Cell { text, numeric });
    }
    let last_row = grid.keys().next_back().copied().unwrap_or(0);
    let last_col = grid
        .values()
        .filter_map(|r| r.keys().next_back().copied())
        .max()
        .unwrap_or(0);
    let width = usize::try_from(last_col).unwrap_or(0);
    sheet.rows = (1..=last_row)
        .map(|r| {
            let mut row = vec![Cell::default(); width];
            if let Some(cells) = grid.remove(&r) {
                for (c, cell) in cells {
                    if let Some(slot) = usize::try_from(c - 1).ok().and_then(|c| row.get_mut(c)) {
                        *slot = cell;
                    }
                }
            }
            row
        })
        .collect();
    book.sheet = sheet;
    Ok(book)
}

// ───────────────────────── Presentations ─────────────────────────

/// One slide's text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slide {
    /// The title placeholder's text (empty without one)
    pub title: String,
    /// Other text paragraphs with their indent level (0 outermost)
    pub lines: Vec<(u8, String)>,
}

/// What a presentation preview shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presentation {
    /// Slides in show order (at most [`MAX_SLIDES`])
    pub slides: Vec<Slide>,
    /// Slides in the presentation
    pub count: usize,
    /// The embedded thumbnail (usually a JPEG of the first slide)
    pub thumbnail: Option<Vec<u8>>,
    /// Some slides or text weren't read (caps)
    pub truncated: bool,
}

/// Text of one slide part.
fn parse_slide(part: &Part) -> Result<(Slide, bool), OfficeError> {
    struct Shape {
        title: bool,
        paras: Vec<(u8, String)>,
    }
    let mut slide = Slide::default();
    let mut shape: Option<Shape> = None;
    let mut paragraph: Option<(u8, String)> = None;
    let mut in_t = false;
    let mut skip = 0_usize;
    let mut full = false;
    let mut xml = Xml::new(part);
    let mut add = |slide: &mut Slide, lines: Vec<(u8, String)>, title: bool| {
        if title && slide.title.is_empty() {
            let text: Vec<String> = lines.into_iter().map(|(_, t)| t).collect();
            slide.title = text.join(" ").replace('\n', " ");
            return;
        }
        for line in lines {
            if slide.lines.len() >= MAX_SLIDE_LINES {
                full = true;
                return;
            }
            slide.lines.push(line);
        }
    };
    while let Some(event) = xml.next()? {
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"sp" if !empty => {
                        shape = Some(Shape {
                            title: false,
                            paras: Vec::new(),
                        });
                    }
                    b"ph" => {
                        if let Some(shape) = shape.as_mut() {
                            shape.title =
                                matches!(attr(e, b"type").as_deref(), Some("title" | "ctrTitle"));
                        }
                    }
                    b"p" if !empty => paragraph = Some((0, String::new())),
                    b"pPr" => {
                        if let Some(paragraph) = paragraph.as_mut() {
                            paragraph.0 = attr(e, b"lvl")
                                .and_then(|l| l.parse::<u8>().ok())
                                .unwrap_or(0)
                                .min(8);
                        }
                    }
                    b"t" if !empty => in_t = paragraph.is_some(),
                    b"br" => {
                        if let Some(paragraph) = paragraph.as_mut() {
                            paragraph.1.push('\n');
                        }
                    }
                    b"Fallback" if !empty => skip = 1,
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_t = false,
                b"p" => {
                    if let Some((level, text)) = paragraph.take() {
                        let text = text.trim().to_owned();
                        if !text.is_empty() {
                            match shape.as_mut() {
                                Some(shape) => shape.paras.push((level, text)),
                                None => add(&mut slide, vec![(level, text)], false),
                            }
                        }
                    }
                }
                b"sp" => {
                    if let Some(shape) = shape.take() {
                        add(&mut slide, shape.paras, shape.title);
                    }
                }
                _ => {}
            },
            _ if in_t => {
                if let (Some(paragraph), Some(text)) = (paragraph.as_mut(), event_text(&event)) {
                    push_capped(&mut paragraph.1, &text, MAX_CELL * 4);
                }
            }
            _ => {}
        }
    }
    Ok((slide, full))
}

/// Reads a presentation's slide text (in show order) and its embedded thumbnail.
pub fn read_presentation(reader: impl Read + Seek) -> Result<Presentation, OfficeError> {
    let mut pkg = Package::open(reader)?;
    let root = pkg.rels("")?;
    let main = rel_of(&root, "/officeDocument").unwrap_or_else(|| "ppt/presentation.xml".into());
    let thumb = rel_of(&root, "/thumbnail").unwrap_or_else(|| "docProps/thumbnail.jpeg".into());
    let thumbnail = pkg
        .read(&thumb, MAX_THUMBNAIL)?
        .filter(|p| !p.truncated)
        .map(|p| p.bytes);
    let part = pkg.read(&main, MAX_META)?.ok_or(OfficeError::Malformed)?;
    let mut ids = Vec::new();
    let mut xml = Xml::new(&part);
    while let Some(event) = xml.next()? {
        if let Event::Start(e) | Event::Empty(e) = event
            && e.local_name().as_ref() == b"sldId"
            && let Some(rel) = rel_attr(&e)
        {
            ids.push(rel);
        }
    }
    let rels = pkg.rels(&main)?;
    let mut deck = Presentation {
        count: ids.len(),
        thumbnail,
        truncated: part.truncated || ids.len() > MAX_SLIDES,
        ..Presentation::default()
    };
    let mut budget = MAX_SLIDES_TOTAL;
    for id in ids.iter().take(MAX_SLIDES) {
        let Some(target) = rels.iter().find(|r| r.id == *id) else {
            deck.slides.push(Slide::default());
            continue;
        };
        let target = target.target.clone();
        let Some(part) = pkg.read(&target, MAX_SLIDE.min(budget))? else {
            deck.slides.push(Slide::default());
            continue;
        };
        budget = budget.saturating_sub(part.bytes.len() as u64);
        let (slide, full) = parse_slide(&part)?;
        deck.truncated |= full || part.truncated;
        deck.slides.push(slide);
        if budget == 0 {
            deck.truncated = true;
            break;
        }
    }
    Ok(deck)
}

/// The relationship id of a `<p:sldId id="256" r:id="rId2"/>` (the `id` attribute with a prefix).
fn rel_attr(e: &BytesStart) -> Option<String> {
    let mut attrs = e.attributes();
    attrs.with_checks(false);
    attrs
        .flatten()
        .find(|a| a.key.local_name().as_ref() == b"id" && a.key.prefix().is_some())
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(Cow::into_owned)
        })
}

// ───────────────────────── Text search ─────────────────────────

/// Decompressed bytes a text search reads from one package, over all its parts.
const SEARCH_BUDGET: u64 = 64 * 1024 * 1024;
/// Decompressed bytes a text search reads from one part.
const SEARCH_PART: u64 = 16 * 1024 * 1024;
/// Bytes a line of text grows to before it is checked in pieces.
const SEARCH_LINE: usize = 64 * 1024;
/// XML events between two polls of the stop check.
const STOP_EVERY: u32 = 2048;

/// Which elements of one kind of part hold searchable text.
struct TextSpec {
    /// The element whose text is one line: a paragraph, a shared string, a cell
    line: &'static [u8],
    /// Elements holding text
    text: &'static [&'static [u8]],
    /// Empty elements that stand for a space (tabs, line breaks)
    space: &'static [&'static [u8]],
    /// Subtrees skipped (phonetic guides, fallback copies of the same text)
    skip: &'static [&'static [u8]],
    /// Lines whose text isn't text: a cell holding a shared-string index
    shared_index: bool,
}

/// Paragraphs of WordprocessingML (and DrawingML text boxes inside it).
const WORD_TEXT: TextSpec = TextSpec {
    line: b"p",
    text: &[b"t"],
    space: &[b"tab", b"br", b"cr"],
    skip: &[b"Fallback"],
    shared_index: false,
};
/// Paragraphs of a slide.
const SLIDE_TEXT: TextSpec = TextSpec {
    line: b"p",
    text: &[b"t"],
    space: &[b"br", b"tab"],
    skip: &[b"Fallback"],
    shared_index: false,
};
/// Entries of the shared string table.
const SHARED_TEXT: TextSpec = TextSpec {
    line: b"si",
    text: &[b"t"],
    space: &[],
    skip: &[b"rPh"],
    shared_index: false,
};
/// Cell values and inline strings of a sheet.
const CELL_TEXT: TextSpec = TextSpec {
    line: b"c",
    text: &[b"v", b"t"],
    space: &[],
    skip: &[b"rPh", b"f"],
    shared_index: true,
};

/// Searches the text of an Office Open XML file: a document's paragraphs and table cells (and
/// its headers, footers and notes), a workbook's shared strings and the values of every sheet,
/// a presentation's slides in show order. `found` gets each line until it returns `Some`;
/// `stop` is polled while reading and ends the search with `None`.
///
/// Parts are streamed, never held whole, and each part and all of them together are
/// decompressed only up to a cap, so a zip bomb costs a bounded amount of time and memory.
pub fn search_text<T>(
    reader: impl Read + Seek,
    kind: OfficeKind,
    stop: &mut dyn FnMut() -> bool,
    found: &mut dyn FnMut(&str) -> Option<T>,
) -> Result<Option<T>, OfficeError> {
    let mut pkg = Package::open(reader)?;
    let parts: Vec<(String, &TextSpec)> = match kind {
        OfficeKind::Document => {
            let main = pkg.main_part("word/document.xml")?;
            let rels = pkg.rels(&main)?;
            let extra = rels.iter().filter(|r| {
                ["/header", "/footer", "/footnotes", "/endnotes"]
                    .iter()
                    .any(|k| r.kind.ends_with(k))
            });
            std::iter::once(main.clone())
                .chain(extra.map(|r| r.target.clone()))
                .map(|p| (p, &WORD_TEXT))
                .collect()
        }
        OfficeKind::Workbook => {
            let main = pkg.main_part("xl/workbook.xml")?;
            let rels = pkg.rels(&main)?;
            // Most text lives in the shared strings, so they go first.
            let shared = rel_of(&rels, "/sharedStrings").map(|p| (p, &SHARED_TEXT));
            let sheets = rels
                .iter()
                .filter(|r| r.kind.ends_with("/worksheet"))
                .map(|r| (r.target.clone(), &CELL_TEXT));
            shared.into_iter().chain(sheets).collect()
        }
        OfficeKind::Presentation => {
            let main = pkg.main_part("ppt/presentation.xml")?;
            let part = pkg.read(&main, MAX_META)?.ok_or(OfficeError::Malformed)?;
            let mut ids = Vec::new();
            let mut xml = Xml::new(&part);
            while let Some(event) = xml.next()? {
                if let Event::Start(e) | Event::Empty(e) = event
                    && e.local_name().as_ref() == b"sldId"
                    && let Some(rel) = rel_attr(&e)
                {
                    ids.push(rel);
                }
            }
            let rels = pkg.rels(&main)?;
            ids.iter()
                .filter_map(|id| rels.iter().find(|r| r.id == *id))
                .take(MAX_SLIDES)
                .map(|r| (r.target.clone(), &SLIDE_TEXT))
                .collect()
        }
    };
    let mut budget = SEARCH_BUDGET;
    for (part, spec) in parts {
        if budget == 0 || stop() {
            return Ok(None);
        }
        let Some(ix) = pkg.find(&part) else {
            continue;
        };
        let Ok(file) = pkg.zip.by_index(ix) else {
            continue;
        };
        let cap = budget.min(SEARCH_PART);
        let mut limited = file.take(cap);
        let hit = scan_text(io::BufReader::new(&mut limited), spec, stop, found);
        budget -= cap - limited.limit();
        if hit.is_some() {
            return Ok(hit);
        }
    }
    Ok(None)
}

/// Streams one part's lines of text to `found`. A syntax error (or the budget cutting the part
/// short) ends the part.
fn scan_text<T>(
    src: impl io::BufRead,
    spec: &TextSpec,
    stop: &mut dyn FnMut() -> bool,
    found: &mut dyn FnMut(&str) -> Option<T>,
) -> Option<T> {
    let mut reader = Reader::from_reader(src);
    let mut buf = Vec::new();
    let mut line = String::new();
    // Paragraphs nest (a text box inside a paragraph): the inner one is its own line.
    let mut line_depth = 0_usize;
    let mut text_depth = 0_usize;
    let mut skip = 0_usize;
    let mut shared_index = false;
    let mut events = 0_u32;
    loop {
        buf.clear();
        let event = match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) | Err(_) => break,
            Ok(event) => event,
        };
        events = events.wrapping_add(1);
        if events.is_multiple_of(STOP_EVERY) && stop() {
            return None;
        }
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if spec.skip.contains(&name) {
                    skip = 1;
                } else if name == spec.line {
                    if let Some(hit) = flush_line(&mut line, found) {
                        return Some(hit);
                    }
                    line_depth += 1;
                    shared_index = spec.shared_index && attr(e, b"t").as_deref() == Some("s");
                } else if spec.text.contains(&name) {
                    text_depth += 1;
                }
            }
            Event::Empty(e) => {
                if line_depth > 0 && spec.space.contains(&e.local_name().as_ref()) {
                    line.push(' ');
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if name == spec.line {
                    if let Some(hit) = flush_line(&mut line, found) {
                        return Some(hit);
                    }
                    line_depth = line_depth.saturating_sub(1);
                    shared_index = false;
                } else if spec.text.contains(&name) {
                    text_depth = text_depth.saturating_sub(1);
                }
            }
            _ if line_depth > 0 && text_depth > 0 && !shared_index => {
                if let Some(text) = event_text(&event) {
                    line.push_str(&text);
                    if line.len() > SEARCH_LINE {
                        // A huge paragraph: check it so far, keep a tail for a match across the cut.
                        if let Some(hit) = found(&line) {
                            return Some(hit);
                        }
                        let mut cut = line.len() - 1024;
                        while !line.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        line.drain(..cut);
                    }
                }
            }
            _ => {}
        }
    }
    flush_line(&mut line, found)
}

/// Hands a finished line to `found` and clears it.
fn flush_line<T>(line: &mut String, found: &mut dyn FnMut(&str) -> Option<T>) -> Option<T> {
    let hit = if line.trim().is_empty() {
        None
    } else {
        found(line)
    };
    line.clear();
    hit
}

// ───────────────────────── Test fixtures ─────────────────────────

/// Builds small Office packages for tests (no binary fixtures in the repository).
#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::expect_used)]
pub mod fixtures {
    use std::fmt::Write as _;
    use std::io::{Cursor, Write as _};

    use zip::write::SimpleFileOptions;

    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    /// A zip with these parts (deflated).
    pub fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in parts {
            zip.start_file(*name, options).expect("start part");
            zip.write_all(bytes).expect("write part");
        }
        zip.finish().expect("finish zip").into_inner()
    }

    fn rels(items: &[(&str, &str, &str)]) -> String {
        let mut out = format!(r#"<?xml version="1.0"?><Relationships xmlns="{PKG_REL}">"#);
        for (id, kind, target) in items {
            let _ = write!(
                out,
                r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{target}"/>"#
            );
        }
        out.push_str("</Relationships>");
        out
    }

    /// A `.docx` whose `<w:body>` holds `body`, with Heading1–3 styles (style ids "h1"–"h3", as
    /// localized Word names them by number) and a "ListBullet" style.
    pub fn docx(body: &str) -> Vec<u8> {
        let doc = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="{W}" xmlns:r="{REL}"><w:body>{body}<w:sectPr><w:pgSz w:w="12240"/></w:sectPr></w:body></w:document>"#
        );
        let mut styles = format!(r#"<?xml version="1.0"?><w:styles xmlns:w="{W}">"#);
        for n in 1..=3 {
            let _ = write!(
                styles,
                r#"<w:style w:type="paragraph" w:styleId="h{n}"><w:name w:val="heading {n}"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="{}"/></w:pPr></w:style>"#,
                n - 1
            );
        }
        styles.push_str(
            r#"<w:style w:type="paragraph" w:styleId="ListBullet"><w:name w:val="List Bullet"/><w:pPr><w:numPr><w:numId w:val="3"/></w:numPr></w:pPr></w:style><w:style w:type="character" w:styleId="h9"><w:name w:val="heading 9 Char"/></w:style></w:styles>"#,
        );
        package(&[
            (
                "_rels/.rels",
                rels(&[("rId1", "officeDocument", "word/document.xml")]).as_bytes(),
            ),
            (
                "word/_rels/document.xml.rels",
                rels(&[("rId1", "styles", "styles.xml")]).as_bytes(),
            ),
            ("word/document.xml", doc.as_bytes()),
            ("word/styles.xml", styles.as_bytes()),
        ])
    }

    /// An `.xlsx` with these sheets (name, `<sheetData>` contents), shared strings and extra
    /// `styles.xml` content (`<numFmts>` and `<cellXfs>`).
    pub fn xlsx(sheets: &[(&str, &str)], shared: &[&str], styles: &str) -> Vec<u8> {
        const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
        let mut book = format!(
            r#"<?xml version="1.0"?><workbook xmlns="{S}" xmlns:r="{REL}"><workbookPr/><sheets>"#
        );
        let mut book_rels = vec![
            ("rIdS".to_owned(), "styles", "styles.xml".to_owned()),
            (
                "rIdT".to_owned(),
                "sharedStrings",
                "sharedStrings.xml".to_owned(),
            ),
        ];
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for (i, (name, data)) in sheets.iter().enumerate() {
            let n = i + 1;
            let _ = write!(
                book,
                r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
            );
            book_rels.push((
                format!("rId{n}"),
                "worksheet",
                format!("worksheets/sheet{n}.xml"),
            ));
            parts.push((
                format!("xl/worksheets/sheet{n}.xml"),
                format!(
                    r#"<?xml version="1.0"?><worksheet xmlns="{S}"><sheetData>{data}</sheetData></worksheet>"#
                )
                .into_bytes(),
            ));
        }
        book.push_str("</sheets></workbook>");
        let mut sst = format!(r#"<?xml version="1.0"?><sst xmlns="{S}">"#);
        for s in shared {
            let _ = write!(sst, "<si><t>{s}</t></si>");
        }
        sst.push_str("</sst>");
        let styles =
            format!(r#"<?xml version="1.0"?><styleSheet xmlns="{S}">{styles}</styleSheet>"#);
        let book_rels: Vec<(&str, &str, &str)> = book_rels
            .iter()
            .map(|(id, kind, target)| (id.as_str(), *kind, target.as_str()))
            .collect();
        let root = rels(&[("rId1", "officeDocument", "xl/workbook.xml")]);
        let book_rels = rels(&book_rels);
        let mut all: Vec<(&str, &[u8])> = vec![
            ("_rels/.rels", root.as_bytes()),
            ("xl/workbook.xml", book.as_bytes()),
            ("xl/_rels/workbook.xml.rels", book_rels.as_bytes()),
            ("xl/sharedStrings.xml", sst.as_bytes()),
            ("xl/styles.xml", styles.as_bytes()),
        ];
        all.extend(parts.iter().map(|(n, b)| (n.as_str(), b.as_slice())));
        package(&all)
    }

    /// A `.pptx` whose slides each have a title and these bullet lines, listed in
    /// `presentation.xml` in reverse part order (so show order differs from file names), with
    /// an optional thumbnail.
    pub fn pptx(slides: &[(&str, &[&str])], thumbnail: Option<&[u8]>) -> Vec<u8> {
        const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
        const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
        let count = slides.len();
        let mut pres = format!(
            r#"<?xml version="1.0"?><p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldIdLst>"#
        );
        let mut pres_rels = Vec::new();
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for (i, (title, lines)) in slides.iter().enumerate() {
            // Slide i is stored as part number count - i.
            let file = count - i;
            let _ = write!(pres, r#"<p:sldId id="{}" r:id="rId{file}"/>"#, 256 + i);
            pres_rels.push((format!("rId{file}"), format!("slides/slide{file}.xml")));
            let mut body = String::new();
            for line in *lines {
                let (level, text) = line.strip_prefix("  ").map_or((0, *line), |rest| (1, rest));
                let _ = write!(
                    body,
                    r#"<a:p><a:pPr lvl="{level}"/><a:r><a:t>{text}</a:t></a:r></a:p>"#
                );
            }
            parts.push((
                format!("ppt/slides/slide{file}.xml"),
                format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody>{body}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                )
                .into_bytes(),
            ));
        }
        pres.push_str("</p:sldIdLst></p:presentation>");
        let pres_rels: Vec<(&str, &str, &str)> = pres_rels
            .iter()
            .map(|(id, target)| (id.as_str(), "slide", target.as_str()))
            .collect();
        let pres_rels = rels(&pres_rels);
        let root = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail" Target="docProps/thumbnail.jpeg"/></Relationships>"#
        );
        let mut all: Vec<(&str, &[u8])> = vec![
            ("_rels/.rels", root.as_bytes()),
            ("ppt/presentation.xml", pres.as_bytes()),
            ("ppt/_rels/presentation.xml.rels", pres_rels.as_bytes()),
        ];
        if let Some(thumb) = thumbnail {
            all.push(("docProps/thumbnail.jpeg", thumb));
        }
        all.extend(parts.iter().map(|(n, b)| (n.as_str(), b.as_slice())));
        package(&all)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn doc(body: &str) -> Document {
        read_document(Cursor::new(fixtures::docx(body))).unwrap()
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn para(style: &str, runs: &str) -> String {
        format!(r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>{runs}</w:p>"#)
    }

    fn run(text: &str) -> String {
        format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#)
    }

    #[test]
    fn office_kinds_by_extension() {
        assert_eq!(office_kind("Report.DOCX"), Some(OfficeKind::Document));
        assert_eq!(office_kind("a.xlsm"), Some(OfficeKind::Workbook));
        assert_eq!(office_kind("deck.ppsx"), Some(OfficeKind::Presentation));
        // The binary formats are deliberately not parsed.
        assert_eq!(office_kind("old.doc"), None);
        assert_eq!(office_kind("old.xls"), None);
        assert_eq!(office_kind("old.ppt"), None);
        assert_eq!(office_kind("docx"), None);
    }

    #[test]
    fn document_headings_runs_and_lists() {
        let body = [
            para("h1", &run("Title")),
            para("h2", &run("Sub")),
            r#"<w:p><w:r><w:t>plain </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:rPr><w:i/><w:b w:val="0"/></w:rPr><w:t> it&amp;al</w:t></w:r></w:p>"#.to_owned(),
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>nested</w:t></w:r></w:p>"#.to_owned(),
            para("ListBullet", &run("styled bullet")),
            r#"<w:p><w:pPr><w:outlineLvl w:val="2"/></w:pPr><w:r><w:t>outline</w:t></w:r></w:p>"#.to_owned(),
            para("Heading3", &run("by id")),
        ]
        .concat();
        let doc = doc(&body);
        assert!(!doc.truncated);
        let b = &doc.blocks;
        assert!(matches!(&b[0], Block::Heading { level: 1, spans } if text(spans) == "Title"));
        assert!(matches!(&b[1], Block::Heading { level: 2, .. }));
        let Block::Paragraph(spans) = &b[2] else {
            panic!("paragraph expected, got {:?}", b[2]);
        };
        assert_eq!(
            spans,
            &[
                Span {
                    text: "plain ".into(),
                    bold: false,
                    italic: false
                },
                Span {
                    text: "bold".into(),
                    bold: true,
                    italic: false
                },
                Span {
                    text: " it&al".into(),
                    bold: false,
                    italic: true
                },
            ]
        );
        assert!(matches!(&b[3], Block::ListItem { depth: 1, spans } if text(spans) == "nested"));
        assert!(matches!(&b[4], Block::ListItem { depth: 0, .. }));
        assert!(matches!(&b[5], Block::Heading { level: 3, .. }));
        // Without a style entry, the built-in id still reads as a heading.
        assert!(matches!(&b[6], Block::Heading { level: 3, .. }));
    }

    #[test]
    fn document_tables_breaks_and_images() {
        let cell = |t: &str| format!("<w:tc><w:p>{}</w:p></w:tc>", run(t));
        let body = [
            format!(
                "<w:tbl><w:tr>{}{}</w:tr><w:tr>{}<w:tc><w:p>{}</w:p><w:p>{}</w:p></w:tc></w:tr></w:tbl>",
                cell("a"),
                cell("b"),
                cell("c"),
                run("d1"),
                run("d2")
            ),
            format!(
                r#"<w:p>{}<w:r><w:br w:type="page"/></w:r>{}</w:p>"#,
                run("before"),
                run("after")
            ),
            // The drawing's text box text is skipped, and the drawing shows as an image.
            r#"<w:p><w:r><w:drawing><wp:inline xmlns:wp="x"><w:txbxContent><w:p><w:r><w:t>hidden</w:t></w:r></w:p></w:txbxContent></wp:inline></w:drawing></w:r></w:p>"#.to_owned(),
            r"<w:p/><w:p/><w:p/>".to_owned(),
            r"<w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>new page</w:t><w:tab/><w:t>x</w:t></w:r></w:p>".to_owned(),
            r"<w:p/>".to_owned(),
        ]
        .concat();
        let doc = doc(&body);
        let b = &doc.blocks;
        assert_eq!(
            b[0],
            Block::Table(vec![
                vec!["a".into(), "b".into()],
                vec!["c".into(), "d1\nd2".into()]
            ])
        );
        assert!(matches!(&b[1], Block::Paragraph(s) if text(s) == "before"));
        assert_eq!(b[2], Block::PageBreak);
        assert!(matches!(&b[3], Block::Paragraph(s) if text(s) == "after"));
        assert_eq!(b[4], Block::Image);
        // Three blank paragraphs collapse to one; trailing blanks go.
        assert_eq!(b[5], Block::Paragraph(Vec::new()));
        assert_eq!(b[6], Block::PageBreak);
        assert!(matches!(&b[7], Block::Paragraph(s) if text(s) == "new page    x"));
        assert_eq!(b.len(), 8);
    }

    #[test]
    fn long_documents_are_truncated() {
        let body = para("Normal", &run("line")).repeat(MAX_BLOCKS + 10);
        let doc = doc(&body);
        assert_eq!(doc.blocks.len(), MAX_BLOCKS);
        assert!(doc.truncated);
    }

    #[test]
    fn broken_and_protected_files_are_reported() {
        // Not a zip
        assert!(matches!(
            read_document(Cursor::new(b"hello world".to_vec())),
            Err(OfficeError::Malformed)
        ));
        // Encrypted OOXML is an OLE compound file.
        let mut ole = OLE_MAGIC.to_vec();
        ole.resize(4096, 0);
        assert!(matches!(
            read_workbook(Cursor::new(ole), 0),
            Err(OfficeError::Protected)
        ));
        // Broken XML in a complete part
        let bad = fixtures::package(&[("word/document.xml", b"<w:document><w:body><w:p></w:x>")]);
        assert!(matches!(
            read_document(Cursor::new(bad)),
            Err(OfficeError::Malformed)
        ));
        // A zip without the main part
        let empty = fixtures::package(&[("other.txt", b"x")]);
        assert!(matches!(
            read_presentation(Cursor::new(empty)),
            Err(OfficeError::Malformed)
        ));
    }

    #[test]
    fn zip_bombs_stop_at_the_cap() {
        // 16 MB of text inside a few dozen KB: reading stops at the cap and the preview says
        // it's truncated.
        let mut huge = br#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>start</w:t></w:r></w:p><w:p><w:r><w:t>"#.to_vec();
        huge.resize(16 * 1024 * 1024, b'a');
        let bomb = fixtures::package(&[("word/document.xml", &huge)]);
        assert!(bomb.len() < 1024 * 1024);
        let doc = read_document(Cursor::new(bomb)).unwrap();
        assert!(doc.truncated);
        assert!(matches!(&doc.blocks[0], Block::Paragraph(s) if text(s) == "start"));
    }

    #[test]
    fn workbook_cells_and_formats() {
        let styles = r#"<numFmts><numFmt numFmtId="164" formatCode="yyyy/mm/dd"/><numFmt numFmtId="165" formatCode="0.0%"/><numFmt numFmtId="166" formatCode="&quot;$&quot;#,##0.00"/><numFmt numFmtId="167" formatCode="[h]:mm:ss"/></numFmts><cellStyleXfs><xf numFmtId="14"/></cellStyleXfs><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/><xf numFmtId="9"/><xf numFmtId="14"/><xf numFmtId="22"/><xf numFmtId="167"/></cellXfs>"#;
        let data = r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="inlineStr"><is><t>inline</t></is></c></row><row r="2"><c r="A2" s="1"><v>45366</v></c><c r="B2" s="2"><v>0.125</v></c><c r="C2" s="3"><v>1234.5</v></c><c r="D2" s="4"><v>0.5</v></c></row><row r="4"><c r="A4" s="5"><v>60</v></c><c r="B4" s="6"><v>45366.5</v></c><c r="C4" s="7"><v>0.75</v></c><c r="D4" t="b"><v>1</v></c><c r="E4" t="e"><v>#DIV/0!</v></c><c r="F4" t="s"><v>1</v></c></row>"#;
        let bytes = fixtures::xlsx(
            &[
                ("First", data),
                ("Second", r#"<row r="1"><c r="B1"><v>7</v></c></row>"#),
            ],
            &["hello", "x&lt;y"],
            styles,
        );
        let book = read_workbook(Cursor::new(bytes.clone()), 0).unwrap();
        assert_eq!(book.names, ["First", "Second"]);
        let rows: Vec<Vec<&str>> = book
            .sheet
            .rows
            .iter()
            .map(|r| r.iter().map(|c| c.text.as_str()).collect())
            .collect();
        assert_eq!(
            rows,
            [
                vec!["hello", "", "inline", "", "", ""],
                vec!["2024-03-15", "12.5%", "1234.5", "50%", "", ""],
                vec![""; 6],
                vec![
                    "1900-02-29",
                    "2024-03-15 12:00",
                    "18:00:00",
                    "TRUE",
                    "#DIV/0!",
                    "x<y"
                ],
            ]
        );
        assert!(book.sheet.rows[1][2].numeric);
        assert!(!book.sheet.rows[0][0].numeric);
        assert!(!book.sheet.more_rows && !book.sheet.more_cols);
        let second = read_workbook(Cursor::new(bytes), 1).unwrap();
        assert_eq!(second.index, 1);
        assert_eq!(
            second.sheet.rows,
            [vec![
                Cell::default(),
                Cell {
                    text: "7".into(),
                    numeric: true
                }
            ]]
        );
    }

    #[test]
    fn large_sheets_show_the_top_left() {
        let mut data = String::new();
        for r in 1..=SHEET_ROWS + 5 {
            let _ = write!(
                data,
                r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c><c r="AZ{r}"><v>1</v></c></row>"#
            );
        }
        let book = read_workbook(Cursor::new(fixtures::xlsx(&[("S", &data)], &[], "")), 0).unwrap();
        assert_eq!(book.sheet.rows.len(), SHEET_ROWS as usize);
        assert_eq!(book.sheet.columns(), 1);
        assert!(book.sheet.more_rows);
        assert!(book.sheet.more_cols);
    }

    #[test]
    fn column_names_and_refs() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
        assert_eq!(parse_ref("AB12"), (Some(28), Some(12)));
        assert_eq!(parse_ref("$C$3"), (Some(3), Some(3)));
        assert_eq!(parse_ref("7"), (None, Some(7)));
    }

    #[test]
    fn format_codes_are_classified() {
        let date = |time, seconds| Format::Date {
            date: true,
            time,
            seconds,
        };
        assert_eq!(classify_format("yyyy-mm-dd"), date(false, false));
        assert_eq!(classify_format("[$-404]e/m/d"), date(false, false));
        assert_eq!(classify_format("m/d/yy h:mm"), date(true, false));
        assert_eq!(classify_format("mmm"), date(false, false));
        assert_eq!(classify_format("0.00%"), Format::Percent(2));
        assert_eq!(classify_format("[Red]0%;-0%"), Format::Percent(0));
        assert_eq!(classify_format("0.00E+00"), Format::General);
        assert_eq!(classify_format("\"days\" 0"), Format::General);
        assert_eq!(
            classify_format("#,##0.00_);[Red](#,##0.00)"),
            Format::General
        );
        assert_eq!(classify_format("General"), Format::General);
        assert_eq!(
            classify_format("mm:ss"),
            Format::Date {
                date: false,
                time: true,
                seconds: true
            }
        );
        // 1904 date system and out-of-range serials
        assert_eq!(
            format_date(0.0, true, (true, false, false)).as_deref(),
            Some("1904-01-01")
        );
        assert_eq!(
            format_date(1.0, false, (true, false, false)).as_deref(),
            Some("1900-01-01")
        );
        assert_eq!(
            format_date(61.0, false, (true, false, false)).as_deref(),
            Some("1900-03-01")
        );
        assert_eq!(format_date(-1.0, false, (true, false, false)), None);
        assert_eq!(format_number("abc", Format::Percent(1), false), "abc");
    }

    #[test]
    fn presentation_slides_in_show_order() {
        let thumb = [0xFF_u8, 0xD8, 0xFF, 0xE0];
        let bytes = fixtures::pptx(
            &[
                ("Intro", &["Point one", "  sub point"]),
                ("Second", &[]),
                ("Third &amp; last", &["Done"]),
            ],
            Some(&thumb),
        );
        let deck = read_presentation(Cursor::new(bytes)).unwrap();
        assert_eq!(deck.count, 3);
        assert!(!deck.truncated);
        assert_eq!(deck.thumbnail.as_deref(), Some(&thumb[..]));
        let titles: Vec<&str> = deck.slides.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Intro", "Second", "Third & last"]);
        assert_eq!(
            deck.slides[0].lines,
            [(0, "Point one".to_owned()), (1, "sub point".to_owned())]
        );
        assert!(deck.slides[1].lines.is_empty());
        let none = read_presentation(Cursor::new(fixtures::pptx(&[("A", &[])], None))).unwrap();
        assert_eq!(none.thumbnail, None);
    }

    #[test]
    fn relationship_targets_resolve() {
        assert_eq!(resolve("ppt", "slides/slide1.xml"), "ppt/slides/slide1.xml");
        assert_eq!(
            resolve("xl/worksheets", "../sharedStrings.xml"),
            "xl/sharedStrings.xml"
        );
        assert_eq!(resolve("word", "/word/styles.xml"), "word/styles.xml");
        assert_eq!(resolve("", "xl/workbook.xml"), "xl/workbook.xml");
    }

    #[test]
    fn oversized_files_are_refused() {
        struct Huge(u64);
        impl Read for Huge {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl Seek for Huge {
            fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
                Ok(match pos {
                    SeekFrom::End(_) => self.0,
                    _ => 0,
                })
            }
        }
        assert!(matches!(
            read_document(Huge(MAX_FILE + 1)),
            Err(OfficeError::TooLarge)
        ));
    }
    /// Every line a text search sees, in order.
    fn search_lines(bytes: Vec<u8>, kind: OfficeKind) -> Vec<String> {
        let mut lines = Vec::new();
        let hit: Option<()> = search_text(Cursor::new(bytes), kind, &mut || false, &mut |line| {
            lines.push(line.to_owned());
            None
        })
        .unwrap();
        assert!(hit.is_none());
        lines
    }

    #[test]
    fn search_reads_document_paragraphs_tables_and_text_boxes() {
        let body = format!(
            "{}{}<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>{}",
            para(
                "h1",
                "<w:r><w:t>Invoice </w:t></w:r><w:r><w:t>No. 7</w:t></w:r>"
            ),
            para(
                "",
                "<w:r><w:t>before</w:t><w:tab/><w:t>tab</w:t></w:r><w:r><mc:AlternateContent><mc:Choice><w:drawing><w:txbxContent><w:p><w:r><w:t>in a box</w:t></w:r></w:p></w:txbxContent></w:drawing></mc:Choice><mc:Fallback><w:pict><w:p><w:r><w:t>in a box</w:t></w:r></w:p></w:pict></mc:Fallback></mc:AlternateContent></w:r><w:r><w:t>after</w:t></w:r>",
            ),
            para("", &run("cell &amp; one")),
            para("", &run("總金額 1,200")),
            para(
                "",
                "<w:r><w:delText>deleted</w:delText><w:instrText>PAGE</w:instrText><w:t>kept</w:t></w:r>"
            ),
        );
        assert_eq!(
            search_lines(fixtures::docx(&body), OfficeKind::Document),
            [
                "Invoice No. 7",
                "before tab",
                "in a box",
                "after",
                "cell & one",
                "總金額 1,200",
                "kept"
            ],
            "the fallback copy, deleted text and field codes are skipped"
        );
    }

    #[test]
    fn search_reads_shared_strings_and_every_sheets_values() {
        let book = fixtures::xlsx(
            &[
                (
                    "One",
                    r#"<row r="1"><c r="A1" t="s"><v>1</v></c><c r="B1"><f>SUM(C1)</f><v>3.5</v></c></row>"#,
                ),
                (
                    "Two",
                    r#"<row r="900"><c r="ZZ900" t="inlineStr"><is><t>far away</t></is></c><c r="A1" t="str"><v>formula text</v></c></row>"#,
                ),
            ],
            &["first", "second<rPh><t>ruby</t></rPh>"],
            "",
        );
        assert_eq!(
            search_lines(book, OfficeKind::Workbook),
            ["first", "second", "3.5", "far away", "formula text"],
            "shared-string indices and formulas aren't text; sheets aren't cut at the preview size"
        );
    }

    #[test]
    fn search_reads_slides_in_show_order() {
        let deck = fixtures::pptx(&[("Intro", &["hello"]), ("Second", &["world"])], None);
        assert_eq!(
            search_lines(deck, OfficeKind::Presentation),
            ["Intro", "hello", "Second", "world"]
        );
    }

    #[test]
    fn search_stops_on_a_hit_on_stop_and_at_the_budget() {
        let deck = fixtures::pptx(&[("Intro", &["hello"]), ("Second", &["world"])], None);
        let mut seen = 0;
        let hit = search_text(
            Cursor::new(deck.clone()),
            OfficeKind::Presentation,
            &mut || false,
            &mut |line| {
                seen += 1;
                (line == "hello").then_some(line.len())
            },
        )
        .unwrap();
        assert_eq!((hit, seen), (Some(5), 2));
        let stopped: Option<()> = search_text(
            Cursor::new(deck),
            OfficeKind::Presentation,
            &mut || true,
            &mut |_| Some(()),
        )
        .unwrap();
        assert_eq!(stopped, None);
        // A zip bomb: 80 MB of one paragraph inside a small file ends at the part cap.
        let mut huge = br#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>"#.to_vec();
        huge.resize(80 * 1024 * 1024, b'a');
        huge.extend_from_slice(b"needle</w:t></w:r></w:p></w:body></w:document>");
        let bomb = fixtures::package(&[("word/document.xml", &huge)]);
        let hit = search_text(
            Cursor::new(bomb),
            OfficeKind::Document,
            &mut || false,
            &mut |line| line.contains("needle").then_some(()),
        )
        .unwrap();
        assert_eq!(hit, None, "the end is past the cap");
        // Encrypted packages and broken files are errors.
        let mut ole = OLE_MAGIC.to_vec();
        ole.resize(4096, 0);
        assert!(matches!(
            search_text::<()>(
                Cursor::new(ole),
                OfficeKind::Document,
                &mut || false,
                &mut |_| None
            ),
            Err(OfficeError::Protected)
        ));
    }
}
