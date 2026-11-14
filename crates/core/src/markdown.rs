//! Markdown for previews: CommonMark plus GitHub's tables, task lists and strikethrough, parsed with
//! pulldown-cmark into a small block tree that the app lays out. Pure computation; the preview
//! loader runs it off the UI thread.

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

    /// Whether there is nothing but white space.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// Parses `text`.
#[must_use]
pub fn parse(text: &str) -> Doc {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut builder = Builder::default();
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
            Event::Html(text) | Event::InlineHtml(text) => {
                let style = self.style();
                self.text(text.trim_end_matches(['\r', '\n']), style);
            }
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
            Tag::Image { .. } => self.image += 1,
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
            TagEnd::Image => self.image = self.image.saturating_sub(1),
            _ => {}
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

    #[test]
    fn unclosed_input_keeps_its_text() {
        let doc = parse("> - a\n> - b\n```\nopen code");
        assert!(matches!(&doc.blocks[0], Block::Quote(_)));
        assert!(matches!(&doc.blocks[1], Block::Code { text, .. } if text == "open code"));
    }
}
