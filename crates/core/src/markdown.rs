//! Markdown for previews: CommonMark plus GitHub's tables, task lists and strikethrough, parsed with
//! pulldown-cmark into a small block tree that the app lays out. Pure computation; the preview
//! loader runs it off the UI thread.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::highlight::{self, Token};

/// A parsed Markdown file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    /// Top-level blocks in order.
    pub blocks: Vec<Block>,
}

/// One block of a [`Doc`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// `#` to `######`.
    Heading {
        /// 1 to 6.
        level: u8,
        /// The heading's text.
        text: Inline,
    },
    /// A paragraph (also the text of a tight list item).
    Paragraph(Inline),
    /// `>` quote, with its own blocks.
    Quote(Vec<Self>),
    /// Bulleted (`start` is `None`) or numbered list.
    List {
        /// Number of the first item of a numbered list.
        start: Option<u64>,
        /// The items.
        items: Vec<Item>,
    },
    /// Fenced or indented code.
    Code {
        /// First word of the fence's info string (`rust` in ```` ```rust ````), or empty.
        lang: String,
        /// The code, without the final line break.
        text: String,
        /// Syntax colors, when the language is one [`highlight`] knows.
        marks: Vec<(Range<usize>, Token)>,
    },
    /// `---`.
    Rule,
    /// GitHub table.
    Table {
        /// Alignment of each column.
        aligns: Vec<Align>,
        /// Header cells.
        head: Vec<Inline>,
        /// Body rows.
        rows: Vec<Vec<Inline>>,
    },
}

/// A list item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// `[x]` / `[ ]` task box: whether it's checked.
    pub task: Option<bool>,
    /// The item's blocks (its text, nested lists, ...).
    pub blocks: Vec<Block>,
}

/// Column alignment of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// Not given.
    None,
    /// `:--`.
    Left,
    /// `:-:`.
    Center,
    /// `--:`.
    Right,
}

/// Text with styled runs that cover it from start to end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    /// The text as shown: markers removed, soft breaks as spaces, hard breaks as `\n`.
    pub text: String,
    /// Consecutive runs; their lengths (bytes) add up to `text.len()`.
    pub runs: Vec<Run>,
    /// Link targets, indexed by [`Style::link`].
    pub links: Vec<String>,
}

/// A run of [`Inline`] text in one style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Length in bytes.
    pub len: usize,
    /// How it looks.
    pub style: Style,
}

/// Inline style of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent inline styles that combine (bold italic code, ...)"
)]
pub struct Style {
    /// `**bold**`.
    pub strong: bool,
    /// `*italic*`.
    pub emph: bool,
    /// `~~struck~~`.
    pub strike: bool,
    /// `` `code` ``.
    pub code: bool,
    /// An image's alt text (images aren't drawn).
    pub image: bool,
    /// Index into [`Inline::links`].
    pub link: Option<usize>,
}

impl Inline {
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        match self.runs.last_mut() {
            Some(last) if last.style == style => last.len += text.len(),
            _ => self.runs.push(Run {
                len: text.len(),
                style,
            }),
        }
    }

    /// Drops white space at the end, keeping the runs in step with the text.
    fn trim_end(&mut self) {
        let mut excess = self.text.len() - self.text.trim_end().len();
        self.text.truncate(self.text.len() - excess);
        while excess > 0 {
            let Some(last) = self.runs.last_mut() else {
                break;
            };
            if last.len > excess {
                last.len -= excess;
                break;
            }
            excess -= last.len;
            self.runs.pop();
        }
    }

    /// Whether there is nothing but white space.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// What an image without a description shows, when the caller has no localized text.
const DEFAULT_IMAGE_PLACEHOLDER: &str = "[image]";

/// Parses `text`.
#[must_use]
pub fn parse(text: &str) -> Doc {
    parse_with(text, DEFAULT_IMAGE_PLACEHOLDER)
}

/// Parses `text`; an image with no description (`![](x.png)`, `<img src=x>`) shows
/// `image_placeholder`.
#[must_use]
pub fn parse_with(text: &str, image_placeholder: &str) -> Doc {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut builder = Builder {
        placeholder: image_placeholder.to_owned(),
        ..Builder::default()
    };
    builder.stack.push(Container::Doc(Vec::new()));
    for event in Parser::new_ext(text, options) {
        builder.event(event);
    }
    builder.finish()
}

/// Where the text being collected goes.
enum Target {
    Paragraph,
    Heading(u8),
    Cell,
    /// Text of a tight list item (no paragraph events around it).
    Loose,
}

enum Container {
    Doc(Vec<Block>),
    Quote(Vec<Block>),
    Item(Item),
}

#[derive(Default)]
struct Table {
    aligns: Vec<Align>,
    head: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
    row: Vec<Inline>,
}

#[derive(Default)]
struct Builder {
    /// Open containers, the document at the bottom.
    stack: Vec<Container>,
    /// Open lists: their start number and the items finished so far.
    lists: Vec<(Option<u64>, Vec<Item>)>,
    inline: Option<(Target, Inline)>,
    code: Option<(String, String)>,
    table: Option<Table>,
    strong: usize,
    emph: usize,
    strike: usize,
    image: usize,
    link: Vec<usize>,
    /// Shown for an image without a description.
    placeholder: String,
    /// Text length where each open Markdown image began, to tell whether it had a description.
    image_starts: Vec<usize>,
    /// Open HTML `<a>` tags: whether each one started a link.
    html_links: Vec<bool>,
    /// The start of an HTML tag cut off by the end of its line, finished by the next HTML event.
    html_tail: String,
}

impl Builder {
    fn style(&self) -> Style {
        Style {
            strong: self.strong > 0,
            emph: self.emph > 0,
            strike: self.strike > 0,
            code: false,
            image: self.image > 0,
            link: self.link.last().copied(),
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Container::Doc(blocks) | Container::Quote(blocks)) => blocks.push(block),
            Some(Container::Item(item)) => item.blocks.push(block),
            None => {}
        }
    }

    /// The text being collected, starting a loose paragraph if none is open.
    fn inline(&mut self) -> &mut Inline {
        &mut self
            .inline
            .get_or_insert_with(|| (Target::Loose, Inline::default()))
            .1
    }

    fn text(&mut self, text: &str, style: Style) {
        if let Some((_, code)) = self.code.as_mut() {
            code.push_str(text);
        } else {
            self.inline().push(text, style);
        }
    }

    /// Ends a loose paragraph before a block starts or its item ends.
    fn close_loose(&mut self) {
        if matches!(self.inline, Some((Target::Loose, _)))
            && let Some((_, text)) = self.inline.take()
            && !text.is_blank()
        {
            let mut text = text;
            text.trim_end();
            self.push_block(Block::Paragraph(text));
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                let style = self.style();
                self.text(&text, style);
            }
            // Math isn't typeset; it shows as code.
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                let style = Style {
                    code: true,
                    ..self.style()
                };
                self.text(&text, style);
            }
            Event::Html(text) | Event::InlineHtml(text) => self.html(&text),
            Event::SoftBreak => {
                let style = self.style();
                self.text(" ", style);
            }
            Event::HardBreak => {
                let style = self.style();
                self.text("\n", style);
            }
            Event::Rule => {
                self.close_loose();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Container::Item(item)) = self.stack.last_mut() {
                    item.task = Some(done);
                }
            }
            Event::FootnoteReference(label) => {
                let style = self.style();
                self.text(&format!("[{label}]"), style);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.close_loose();
                self.inline = Some((Target::Paragraph, Inline::default()));
            }
            Tag::Heading { level, .. } => {
                self.close_loose();
                self.inline = Some((Target::Heading(level as u8), Inline::default()));
            }
            Tag::BlockQuote(_) => {
                self.close_loose();
                self.stack.push(Container::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.close_loose();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.close_loose();
                self.lists.push((start, Vec::new()));
            }
            Tag::Item => self.stack.push(Container::Item(Item::default())),
            Tag::Table(aligns) => {
                self.close_loose();
                self.table = Some(Table {
                    aligns: aligns
                        .into_iter()
                        .map(|a| match a {
                            Alignment::None => Align::None,
                            Alignment::Left => Align::Left,
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                        })
                        .collect(),
                    ..Table::default()
                });
            }
            Tag::TableCell => self.inline = Some((Target::Cell, Inline::default())),
            Tag::Emphasis => self.emph += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                let inline = self.inline();
                inline.links.push(dest_url.into_string());
                let ix = inline.links.len() - 1;
                self.link.push(ix);
            }
            Tag::Image { .. } => {
                let start = self.inline().text.len();
                self.image_starts.push(start);
                self.image += 1;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if let Some((_, text)) = self.inline.take()
                    && !text.is_blank()
                {
                    self.push_block(Block::Paragraph(text));
                }
            }
            TagEnd::Heading(_) => {
                if let Some((Target::Heading(level), text)) = self.inline.take() {
                    self.push_block(Block::Heading { level, text });
                }
            }
            TagEnd::BlockQuote(_) => {
                self.close_loose();
                if let Some(Container::Quote(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut text)) = self.code.take() {
                    while text.ends_with(['\n', '\r']) {
                        text.pop();
                    }
                    let marks = highlight::lang_for(fence_ext(&lang))
                        .map_or_default(|syntax| highlight::highlight(&text, syntax));
                    self.push_block(Block::Code { lang, text, marks });
                }
            }
            TagEnd::List(_) => {
                if let Some((start, items)) = self.lists.pop() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.close_loose();
                if let Some(Container::Item(item)) = self.stack.pop()
                    && let Some((_, items)) = self.lists.last_mut()
                {
                    items.push(item);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.head = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let text = self.inline.take().map_or_default(|(_, text)| text);
                if let Some(table) = self.table.as_mut() {
                    table.row.push(text);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(Block::Table {
                        aligns: table.aligns,
                        head: table.head,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.emph = self.emph.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.link.pop();
            }
            TagEnd::Image => {
                // No description: show the placeholder so the image doesn't vanish.
                if let Some(start) = self.image_starts.pop()
                    && self.inline().text.len() == start
                {
                    self.placeholder_image();
                }
                self.image = self.image.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Shows the image placeholder.
    fn placeholder_image(&mut self) {
        let style = Style {
            image: true,
            ..self.style()
        };
        let placeholder = std::mem::take(&mut self.placeholder);
        self.text(&placeholder, style);
        self.placeholder = placeholder;
    }

    /// Raw HTML: images show their description, `<br>` breaks the line, `<a href>` is a link, and
    /// every other tag and comment is dropped, keeping the text between them. One linear pass.
    fn html(&mut self, raw: &str) {
        let joined;
        let mut rest = if self.html_tail.is_empty() {
            raw
        } else {
            joined = std::mem::take(&mut self.html_tail) + raw;
            &joined
        };
        while let Some(lt) = rest.find('<') {
            self.html_text(&rest[..lt]);
            let tag = &rest[lt..];
            if let Some(comment) = tag.strip_prefix("<!--") {
                match comment.find("-->") {
                    Some(end) => rest = &comment[end + 3..],
                    None => return,
                }
                continue;
            }
            let starts_tag = tag[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/')
                || tag[1..].starts_with(['!', '?']);
            if !starts_tag {
                self.html_text("<");
                rest = &tag[1..];
                continue;
            }
            let Some(end) = tag_end(tag) else {
                // The tag goes on in the next line.
                tag.clone_into(&mut self.html_tail);
                return;
            };
            self.html_tag(&tag[1..end]);
            rest = &tag[end + 1..];
        }
        self.html_text(rest);
    }

    /// Text between HTML tags: white space collapses to single spaces and entities are decoded.
    fn html_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.style();
        let at_gap = |inline: &mut Inline| {
            inline.text.is_empty() || inline.text.ends_with(char::is_whitespace)
        };
        let mut any = false;
        for word in text.split_whitespace() {
            if any || (text.starts_with(char::is_whitespace) && !at_gap(self.inline())) {
                self.text(" ", style);
            }
            any = true;
            self.text(&unescape(word), style);
        }
        if (!any || text.ends_with(char::is_whitespace)) && !at_gap(self.inline()) {
            self.text(" ", style);
        }
    }

    /// One HTML tag, without its `<` and `>`.
    fn html_tag(&mut self, tag: &str) {
        let (closing, tag) = tag.strip_prefix('/').map_or((false, tag), |t| (true, t));
        let name_end = tag
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let (name, attrs) = tag.split_at(name_end);
        let is = |want: &str| name.eq_ignore_ascii_case(want);
        if is("img") && !closing {
            let alt = attr(attrs, "alt").map_or_else(Default::default, unescape);
            let alt = alt.trim();
            if alt.is_empty() {
                self.placeholder_image();
            } else {
                let style = Style {
                    image: true,
                    ..self.style()
                };
                self.text(alt, style);
            }
        } else if is("br") || (is("summary") && closing) {
            // The summary is a line of its own above the content.
            let style = self.style();
            self.text("\n", style);
        } else if is("a") {
            if closing {
                if self.html_links.pop() == Some(true) {
                    self.link.pop();
                }
            } else {
                let href = attr(attrs, "href")
                    .map(unescape)
                    .filter(|href| !href.is_empty());
                self.html_links.push(href.is_some());
                if let Some(href) = href {
                    let inline = self.inline();
                    inline.links.push(href.into_owned());
                    let ix = inline.links.len() - 1;
                    self.link.push(ix);
                }
            }
        }
    }

    fn finish(mut self) -> Doc {
        self.close_loose();
        // Close whatever malformed input left open, so nothing collected is lost.
        while self.stack.len() > 1 {
            match self.stack.pop() {
                Some(Container::Quote(blocks)) => self.push_block(Block::Quote(blocks)),
                Some(Container::Item(item)) => {
                    if let Some((_, items)) = self.lists.last_mut() {
                        items.push(item);
                    }
                }
                _ => {}
            }
        }
        while let Some((start, items)) = self.lists.pop() {
            self.push_block(Block::List { start, items });
        }
        match self.stack.pop() {
            Some(Container::Doc(blocks)) => Doc { blocks },
            _ => Doc::default(),
        }
    }
}

/// Index of the `>` that ends the tag at the start of `tag` (quoted attribute values may hold one).
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (i, b) in tag.bytes().enumerate().skip(1) {
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// The value of attribute `want` in the attribute part of a tag: quoted either way or bare, in any
/// order, name in any case.
fn attr<'a>(attrs: &'a str, want: &str) -> Option<&'a str> {
    let b = attrs.as_bytes();
    let skip_space = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut i = 0;
    loop {
        i = skip_space(i);
        while i < b.len() && b[i] == b'/' {
            i = skip_space(i + 1);
        }
        if i >= b.len() {
            return None;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'/') {
            i += 1;
        }
        if i == start {
            // A stray `=`.
            i += 1;
            continue;
        }
        let name = &attrs[start..i];
        i = skip_space(i);
        let mut value = "";
        if i < b.len() && b[i] == b'=' {
            i = skip_space(i + 1);
            let start = i;
            if i < b.len() && matches!(b[i], b'"' | b'\'') {
                let quote = b[i];
                i += 1;
                let start = i;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                value = &attrs[start..i];
                i = (i + 1).min(b.len());
            } else {
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = &attrs[start..i];
                // `<img src=x/>`: the slash closes the tag.
                if i == b.len() {
                    value = value.strip_suffix('/').unwrap_or(value);
                }
            }
        }
        if name.eq_ignore_ascii_case(want) {
            return Some(value);
        }
    }
}

/// Decodes the common HTML entities (`&amp;`, `&quot;`, `&#39;`, `&#x27;`, ...); anything else is
/// left as written. Borrows when there is nothing to decode.
fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let ch = match &rest[1..semi] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => '\u{a0}',
                num => {
                    let num = num.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((ch, semi))
        });
        if let Some((ch, semi)) = decoded {
            out.push(ch);
            rest = &rest[semi + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// File extension [`highlight::lang_for`] knows for a fence's language name (`rust` -> `rs`); other
/// names are tried as extensions (`rs`, `toml`, ...).
fn fence_ext(lang: &str) -> &str {
    match lang.to_ascii_lowercase().as_str() {
        "rust" => "rs",
        "python" | "python3" => "py",
        "javascript" | "node" => "js",
        "typescript" => "ts",
        "csharp" | "c#" => "cs",
        "c++" => "cpp",
        "kotlin" => "kt",
        "ruby" => "rb",
        "shell" | "console" | "shellscript" => "sh",
        "powershell" | "pwsh" => "ps1",
        "batch" | "dos" => "bat",
        "golang" => "go",
        "yaml" => "yml",
        _ => lang,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(inline: &Inline) -> Vec<(&str, Style)> {
        let mut at = 0;
        inline
            .runs
            .iter()
            .map(|run| {
                let part = &inline.text[at..at + run.len];
                at += run.len;
                (part, run.style)
            })
            .collect()
    }

    fn paragraph(doc: &Doc, ix: usize) -> &Inline {
        match &doc.blocks[ix] {
            Block::Paragraph(text) => text,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inline_styles_become_runs() {
        let doc = parse("A **bold** and *it* with `code`, ~~gone~~ and [a link](https://x.y).");
        let text = paragraph(&doc, 0);
        assert_eq!(text.text, "A bold and it with code, gone and a link.");
        let runs = styled(text);
        let find = |part: &str| runs.iter().find(|(p, _)| *p == part).expect(part).1;
        assert!(find("bold").strong);
        assert!(find("it").emph);
        assert!(find("code").code);
        assert!(find("gone").strike);
        assert_eq!(find("a link").link, Some(0));
        assert_eq!(text.links, ["https://x.y"]);
        assert_eq!(
            runs.iter().map(|(p, _)| p.len()).sum::<usize>(),
            text.text.len()
        );
    }

    #[test]
    fn blocks_headings_quotes_code_and_rules() {
        let doc =
            parse("# Title\n\npara one\nsame para\n\n> quoted\n\n```rust\nfn x() {}\n```\n\n---\n");
        assert!(
            matches!(&doc.blocks[0], Block::Heading { level: 1, text } if text.text == "Title")
        );
        // A soft break joins the lines with a space.
        assert_eq!(paragraph(&doc, 1).text, "para one same para");
        assert!(
            matches!(&doc.blocks[2], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph(t) if t.text == "quoted"))
        );
        let Block::Code { lang, text, marks } = &doc.blocks[3] else {
            panic!("{:?}", doc.blocks[3]);
        };
        assert_eq!((lang.as_str(), text.as_str()), ("rust", "fn x() {}"));
        // `fn` is a keyword: the language name found the Rust colors.
        assert!(
            marks
                .iter()
                .any(|(r, t)| &text[r.clone()] == "fn" && *t == Token::Keyword)
        );
        assert_eq!(doc.blocks[4], Block::Rule);
    }

    #[test]
    fn lists_nest_number_and_take_task_boxes() {
        let doc = parse("- one\n  - nested\n- [x] done\n- [ ] todo\n\n3. three\n4. four\n");
        let Block::List { start: None, items } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks[0]);
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0].blocks[0], Block::Paragraph(t) if t.text == "one"));
        assert!(
            matches!(&items[0].blocks[1], Block::List { items: inner, .. } if inner.len() == 1)
        );
        assert_eq!(items[1].task, Some(true));
        assert_eq!(items[2].task, Some(false));
        assert!(
            matches!(&doc.blocks[1], Block::List { start: Some(3), items } if items.len() == 2)
        );
    }

    #[test]
    fn tables_keep_head_rows_and_alignment() {
        let doc = parse("| a | b |\n|:--|--:|\n| 1 | **2** |\n| 3 | 4 |\n");
        let Block::Table { aligns, head, rows } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks[0]);
        };
        assert_eq!(aligns, &[Align::Left, Align::Right]);
        assert_eq!(
            head.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0][1].runs[0].style.strong);
    }

    #[test]
    fn lone_cr_line_breaks_parse_like_lf() {
        assert_eq!(
            parse("# T\r\rtext\r- item\r"),
            parse("# T\n\ntext\n- item\n")
        );
    }

    /// Every run of the paragraphs, headings and table cells, in order.
    fn all_runs(doc: &Doc) -> Vec<(String, Style)> {
        fn inline(out: &mut Vec<(String, Style)>, text: &Inline) {
            out.extend(styled(text).into_iter().map(|(p, s)| (p.to_owned(), s)));
        }
        let mut out = Vec::new();
        for block in &doc.blocks {
            match block {
                Block::Paragraph(t) | Block::Heading { text: t, .. } => inline(&mut out, t),
                Block::Table { head, rows, .. } => {
                    for cell in head.iter().chain(rows.iter().flatten()) {
                        inline(&mut out, cell);
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn doc_text(doc: &Doc) -> String {
        all_runs(doc).into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn html_image_shows_its_alt_in_the_image_style() {
        let doc = parse("<div align=\"center\"><img src=\"a.png\" alt=\"Logo\"></div>\n");
        let runs = all_runs(&doc);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].0, "Logo");
        assert!(runs[0].1.image);
        assert!(!doc_text(&doc).contains('<'));
    }

    #[test]
    fn html_image_attributes_parse_in_any_form() {
        for html in [
            "<p><IMG SRC='a.png' ALT='Shot'/></p>",
            "<p><img alt=Shot src=a.png></p>",
            "<p><img src=\"a.png\" width=\"10\" alt = \"Shot\" /></p>",
            "<p><img\n  src=\"a.png\"\n  alt=\"Shot\"\n></p>",
        ] {
            assert_eq!(doc_text(&parse(html)), "Shot", "{html}");
        }
        assert_eq!(doc_text(&parse("<p><img alt=\"a &amp; b\"></p>")), "a & b");
    }

    #[test]
    fn html_image_in_table_cells_shows_the_alt() {
        let doc = parse(
            "| A | B |\n|---|---|\n| <img src=\"x/1.png\" alt=\"Main window\"> | <img src=x/2.png alt=\"Settings\"> |\n",
        );
        let Block::Table { rows, .. } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks[0]);
        };
        assert_eq!(rows[0][0].text, "Main window");
        assert_eq!(rows[0][1].text, "Settings");
        assert!(rows[0][0].runs[0].style.image);
    }

    #[test]
    fn images_without_a_description_show_the_placeholder() {
        assert_eq!(doc_text(&parse("<img src=x>")), "[image]");
        assert_eq!(doc_text(&parse("<img src=x alt=\"\">")), "[image]");
        assert_eq!(doc_text(&parse("![](x.png)")), "[image]");
        assert_eq!(doc_text(&parse("a ![](x.png) b")), "a [image] b");
        assert_eq!(doc_text(&parse_with("![](x.png)", "[Bild]")), "[Bild]");
        assert!(all_runs(&parse("![](x.png)"))[0].1.image);
        // A description still wins.
        assert_eq!(doc_text(&parse("![Logo](x.png)")), "Logo");
    }

    #[test]
    fn br_breaks_the_line_and_details_show_summary_and_content() {
        assert_eq!(doc_text(&parse("one<br>two<br/>three")), "one\ntwo\nthree");
        let doc = parse("<details>\n<summary>More</summary>\n\nhidden text\n\n</details>\n");
        let text = doc_text(&doc);
        assert!(text.contains("More"), "{text:?}");
        assert!(text.contains("hidden text"), "{text:?}");
        assert!(!text.contains('<'));
        assert_eq!(
            doc_text(&parse("<details><summary>More</summary>text</details>")),
            "More\ntext"
        );
    }

    #[test]
    fn wrapper_and_unknown_tags_keep_their_text() {
        let doc = parse(
            "<p align=\"center\">\n  <b>Bold</b> and <sup>sup</sup> <custom-tag x=\"1\">text</custom-tag>\n</p>\n\n<!-- hidden -->\n\nafter <span>span</span> &amp; <u>u</u>\n",
        );
        assert_eq!(doc_text(&doc), "Bold and sup textafter span & u");
    }

    #[test]
    fn html_anchor_becomes_a_link() {
        let doc = parse("see <a href=\"https://x.y\">the site</a> now");
        let text = paragraph(&doc, 0);
        assert_eq!(text.text, "see the site now");
        assert_eq!(text.links, ["https://x.y"]);
        let runs = styled(text);
        assert_eq!(runs[1].0, "the site");
        assert_eq!(runs[1].1.link, Some(0));
        assert_eq!(runs[2].1.link, None);
        // No href: just text.
        assert!(
            paragraph(&parse("<a name=\"top\">x</a>"), 0)
                .links
                .is_empty()
        );
    }

    #[test]
    fn code_keeps_html_verbatim() {
        let doc = parse(
            "use `<div>` here\n\n```html\n<div align=\"center\"><img src=x></div>\n```\n\n    <p>indented</p>\n",
        );
        assert_eq!(paragraph(&doc, 0).text, "use <div> here");
        assert!(
            matches!(&doc.blocks[1], Block::Code { text, .. } if text == "<div align=\"center\"><img src=x></div>")
        );
        assert!(matches!(&doc.blocks[2], Block::Code { text, .. } if text == "<p>indented</p>"));
    }

    #[test]
    fn readme_with_html_shows_no_tag_source() {
        let text = doc_text(&parse(README_EXCERPT));
        assert!(!text.contains('<'), "{text:?}");
        assert!(!text.contains("src="), "{text:?}");
        for want in [
            "laPower",
            "Release",
            "Main window",
            "Settings",
            "[image]",
            "More",
        ] {
            assert!(text.contains(want), "{want}: {text:?}");
        }
    }

    /// The HTML shapes of a GitHub README (centered header, badges, a table of screenshots),
    /// trimmed.
    const README_EXCERPT: &str = r#"<div align="center">

<img src="docs/icon.png" alt="laPower" width="96" height="96">

# laPower

<p align="center">
  <a href="https://github.com/x/y/releases"><img src="https://img.shields.io/badge/v1-blue" alt="Release"></a>
  <img src="https://img.shields.io/badge/license-MIT">
</p>

<sub>Power tools</sub><br>

</div>

| Main | Settings |
|---|---|
| <img src="docs/screenshots/en/main.png" alt="Main window"> | <img src="docs/screenshots/en/settings.png" alt="Settings"> |

<details>
<summary>More</summary>

Extra text.

</details>
"#;

    #[test]
    fn unclosed_input_keeps_its_text() {
        let doc = parse("> - a\n> - b\n```\nopen code");
        assert!(matches!(&doc.blocks[0], Block::Quote(_)));
        assert!(matches!(&doc.blocks[1], Block::Code { text, .. } if text == "open code"));
    }
}
