//! Markdown preview layout: lays out the block tree from `arke_core::markdown` (parsed off the UI
//! thread) the way GitHub shows a README: styled inline text, headings with rules, nested and task
//! lists, quotes, tables and highlighted code. Links open with Ctrl+click.

use std::cell::Cell;

use arke_core::markdown::{Align, Block, Doc, Inline, Item, Style};
use gpui::{
    AnyElement, App, FontStyle, FontWeight, InteractiveText, SharedString, StrikethroughStyle,
    StyledText, TextRun, UnderlineStyle, div, prelude::*, px, svg,
};
use rust_i18n::t;

use crate::assets::Icon;
use crate::theme::{self, UiFont as _};

/// Code font (the text and code previews use the same).
const MONO: &str = "Cascadia Mono";

/// One Markdown document. `large` is Quick Look: bigger text and more room between blocks.
pub(super) fn markdown(doc: &Doc, large: bool) -> AnyElement {
    markdown_at(doc, large, 1.)
}

/// [`markdown`] with its text scaled by `zoom` (Quick Look's Ctrl+wheel).
pub(super) fn markdown_at(doc: &Doc, large: bool, zoom: f32) -> AnyElement {
    let layout = Layout {
        large,
        zoom,
        body: if large {
            theme::fs_15()
        } else {
            theme::fs_13()
        } * zoom,
        next_id: Cell::new(0),
    };
    div()
        .flex()
        .flex_col()
        .gap(layout.block_gap())
        .ui_font()
        .text_size(layout.body)
        .text_color(theme::text())
        .children(doc.blocks.iter().map(|b| layout.block(b, Tone::Normal)))
        .into_any_element()
}

/// Text color of a block: quotes are muted.
#[derive(Clone, Copy)]
enum Tone {
    Normal,
    Quoted,
}

impl Tone {
    fn color(self) -> gpui::Rgba {
        match self {
            Self::Normal => theme::text(),
            Self::Quoted => theme::text_muted(),
        }
    }
}

struct Layout {
    large: bool,
    /// Text scale; `body` already includes it
    zoom: f32,
    body: gpui::Pixels,
    /// Ids for text with links (each needs its own).
    next_id: Cell<usize>,
}

impl Layout {
    fn block_gap(&self) -> gpui::Pixels {
        if self.large {
            theme::s_16()
        } else {
            theme::s_12()
        }
    }

    fn block(&self, block: &Block, tone: Tone) -> AnyElement {
        match block {
            Block::Heading { level, text } => self.heading(*level, text),
            Block::Paragraph(text) => self.text(text, FontWeight::NORMAL, tone),
            Block::Quote(blocks) => div()
                .pl(theme::s_12())
                .border_l_2()
                .border_color(theme::line_strong())
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .children(blocks.iter().map(|b| self.block(b, Tone::Quoted)))
                .into_any_element(),
            Block::List { start, items } => self.list(*start, items, tone),
            Block::Code { lang, text, marks } => self.code(lang, text, marks),
            Block::Rule => div()
                .my(theme::s_4())
                .h(px(1.))
                .bg(theme::line())
                .into_any_element(),
            Block::Table { aligns, head, rows } => self.table(aligns, head, rows),
        }
    }

    /// `#` and `##` get a rule below, like GitHub; the rest step down in size and color.
    fn heading(&self, level: u8, text: &Inline) -> AnyElement {
        let size = match (level, self.large) {
            (1, _) | (2, true) => theme::fs_20() * self.zoom,
            (2, false) | (3, true) => theme::fs_15() * self.zoom,
            _ => self.body,
        };
        let tone = if level >= 4 {
            Tone::Quoted
        } else {
            Tone::Normal
        };
        div()
            .when(level > 1, |d| d.mt(theme::s_8()))
            .when(level <= 2, |d| {
                d.pb(theme::s_6()).border_b_1().border_color(theme::line())
            })
            .text_size(size)
            .child(self.text(text, theme::FW_STRONG, tone))
            .into_any_element()
    }

    fn list(&self, start: Option<u64>, items: &[Item], tone: Tone) -> AnyElement {
        let gap = if self.large {
            theme::s_6()
        } else {
            theme::s_4()
        };
        div()
            .flex()
            .flex_col()
            .gap(gap)
            .children(items.iter().enumerate().map(|(ix, item)| {
                let marker = match (item.task, start) {
                    (Some(done), _) => task_box(done),
                    (None, Some(first)) => div()
                        .text_color(theme::text_subtle())
                        .child(format!("{}.", first + ix as u64))
                        .into_any_element(),
                    (None, None) => div()
                        .text_color(theme::text_subtle())
                        .child("•")
                        .into_any_element(),
                };
                div()
                    .flex()
                    .gap(theme::s_8())
                    .child(
                        div()
                            .flex_none()
                            .min_w(theme::s_16())
                            .flex()
                            .justify_end()
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(gap)
                            .children(item.blocks.iter().map(|b| self.block(b, tone))),
                    )
            }))
            .into_any_element()
    }

    /// A card with the language on top and colored code below; long lines wrap.
    fn code(
        &self,
        lang: &str,
        text: &str,
        marks: &[(std::ops::Range<usize>, arke_core::highlight::Token)],
    ) -> AnyElement {
        let highlights = marks.iter().map(|(range, token)| {
            (
                range.clone(),
                gpui::HighlightStyle {
                    color: Some(theme::code_color(*token).into()),
                    ..gpui::HighlightStyle::default()
                },
            )
        });
        div()
            .rounded(theme::r_8())
            .border_1()
            .border_color(theme::line())
            .bg(theme::wash())
            .when(!lang.is_empty(), |d| {
                d.child(
                    div()
                        .px(theme::s_12())
                        .pt(theme::s_8())
                        .text_size(theme::fs_11() * self.zoom)
                        .text_color(theme::text_subtle())
                        .child(lang.to_owned()),
                )
            })
            .child(
                div()
                    .px(theme::s_12())
                    .py(theme::s_8())
                    .font_family(MONO)
                    .text_size(
                        if self.large {
                            theme::fs_13()
                        } else {
                            theme::fs_12()
                        } * self.zoom,
                    )
                    .child(StyledText::new(text.to_owned()).with_highlights(highlights)),
            )
            .into_any_element()
    }

    /// Header on a light wash, rules between rows and columns, cells aligned as the separator
    /// line says.
    fn table(&self, aligns: &[Align], head: &[Inline], rows: &[Vec<Inline>]) -> AnyElement {
        let row = |cells: &[Inline], header: bool| {
            div().flex().when(header, |d| d.bg(theme::wash())).children(
                cells.iter().enumerate().map(|(ix, cell)| {
                    let align = aligns.get(ix).copied().unwrap_or(Align::None);
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(theme::s_8())
                        .py(theme::s_6())
                        .flex()
                        .when(align == Align::Center, gpui::Styled::justify_center)
                        .when(align == Align::Right, gpui::Styled::justify_end)
                        .when(ix > 0, |d| d.border_l_1().border_color(theme::line()))
                        .child(self.text(
                            cell,
                            if header {
                                theme::FW_STRONG
                            } else {
                                FontWeight::NORMAL
                            },
                            Tone::Normal,
                        ))
                }),
            )
        };
        div()
            .rounded(theme::r_8())
            .border_1()
            .border_color(theme::line())
            .overflow_hidden()
            .child(row(head, true))
            .children(
                rows.iter()
                    .map(|cells| row(cells, false).border_t_1().border_color(theme::line())),
            )
            .into_any_element()
    }

    /// Inline text in runs: bold, italic, struck, code on a wash, links underlined in the accent
    /// color (Ctrl+click opens them; a tooltip says so).
    fn text(&self, inline: &Inline, weight: FontWeight, tone: Tone) -> AnyElement {
        let mut runs = Vec::with_capacity(inline.runs.len());
        let mut link_ranges = Vec::new();
        let mut link_targets = Vec::new();
        let mut at = 0;
        for run in &inline.runs {
            let style = run.style;
            runs.push(text_run(run.len, style, weight, tone));
            if let Some(link) = style.link
                && let Some(url) = inline.links.get(link)
            {
                link_ranges.push(at..at + run.len);
                link_targets.push(url.clone());
            }
            at += run.len;
        }
        let styled = StyledText::new(inline.text.clone()).with_runs(runs);
        if link_ranges.is_empty() {
            return styled.into_any_element();
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let targets = std::rc::Rc::new(link_targets);
        let ranges = link_ranges.clone();
        let tips = std::rc::Rc::clone(&targets);
        InteractiveText::new(("md-text", id), styled)
            .on_click(link_ranges, move |ix, window, cx| {
                if window.modifiers().control
                    && let Some(url) = targets.get(ix)
                {
                    open_link(url, cx);
                }
            })
            .tooltip(move |at, _, cx| {
                let ix = ranges.iter().position(|r| r.contains(&at))?;
                let url = tips.get(ix)?;
                let tip = t!("preview.md_link_tip", url = url).into_owned();
                Some(cx.new(|_| crate::workspace::Tooltip(tip.into())).into())
            })
            .into_any_element()
    }
}

fn text_run(len: usize, style: Style, weight: FontWeight, tone: Tone) -> TextRun {
    let mut font = theme::ui_font_at(if style.strong {
        theme::FW_STRONG
    } else {
        weight
    });
    if style.code {
        font.family = SharedString::from(MONO);
    }
    if style.emph {
        font.style = FontStyle::Italic;
    }
    let color = if style.link.is_some() {
        theme::signal()
    } else if style.image {
        theme::text_subtle()
    } else {
        tone.color()
    };
    TextRun {
        len,
        font,
        color: color.into(),
        background_color: style.code.then(|| theme::wash_strong().into()),
        underline: style.link.map(|_| UnderlineStyle {
            thickness: px(1.),
            color: Some(theme::signal().into()),
            wavy: false,
        }),
        strikethrough: style.strike.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(color.into()),
        }),
    }
}

/// `[x]` / `[ ]` drawn like Arke's check boxes.
fn task_box(done: bool) -> AnyElement {
    div()
        .mt(crate::theme::scaled_px(3.))
        .size(crate::theme::scaled_px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
        .border_1()
        .when(done, |d| {
            d.bg(theme::signal()).border_color(theme::signal())
        })
        .when(!done, |d| d.border_color(theme::text_subtle()))
        .when(done, |d| {
            d.child(
                svg()
                    .path(Icon::Check.path())
                    .size(crate::theme::scaled_px(10.))
                    .text_color(theme::on_signal()),
            )
        })
        .into_any_element()
}

/// Opens web and mail links in the default program; other targets (relative paths, `file:`) are
/// left alone.
fn open_link(url: &str, cx: &mut App) {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
    {
        crate::workspace::update_page::open_url(url, cx);
    }
}
