//! Office Open XML previews (`.docx`, `.xlsx`, `.pptx`) for the preview pane and Quick Look.
//!
//! Parsing is Arke's own (`arke_core::office`, zip + XML) and runs on the preview thread; Office,
//! COM and shell preview handlers are never involved, so no third-party code runs in Arke's
//! process. Documents show their text with basic structure, workbooks one sheet as a grid (Quick
//! Look has sheet tabs; other sheets load on demand), presentations their embedded thumbnail in
//! the pane and the slides as text cards in Quick Look, turned with the PDF page keys.
//!
//! The old binary formats (`.doc`, `.xls`, `.ppt`) are deliberately not parsed: OLE binary
//! formats are large to support for little gain, so they keep the kind icon like other files.

use std::path::Path;
use std::sync::Arc;

use arke_core::office::{
    self, Block, Document, OfficeError, OfficeKind, Presentation, Slide, Span, Workbook,
    column_name,
};
use arke_core::{Entry, Fs};
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, FontStyle, FontWeight, HighlightStyle, ObjectFit,
    Pixels, Point, RenderImage, SharedString, StyledText, div, img, point, prelude::*, px,
};
use rust_i18n::t;

use super::super::{AppEvent, kind_icon, spawn_thread};
use super::{
    Explorer, IMAGE_SIDE, PANE_IMAGE_RATIO, PDF_PAD, PDF_PAD_BOTTOM, PageTurn, PreviewBody,
    PreviewData, fit_rect,
};
use crate::a11y::A11yExt as _;
use crate::theme::{self, UiFont as _};

/// Height of a sheet row and of the column letters
const CELL_H: Pixels = theme::S_24;
/// Narrowest and widest sheet column (logical pixels at 12 px text)
const COL_MIN: f32 = 44.;
const COL_MAX: f32 = 240.;

/// A workbook on screen: one sheet's cells, its column widths, and a sheet being loaded.
pub(crate) struct WorkbookView {
    pub(crate) book: Arc<Workbook>,
    /// Column widths at 12 px text
    widths: Arc<Vec<f32>>,
    /// Sheet being read to replace this one
    pub(crate) pending: Option<usize>,
}

impl WorkbookView {
    pub(super) fn new(book: Workbook) -> Self {
        let widths = column_widths(&book);
        Self {
            book: Arc::new(book),
            widths: Arc::new(widths),
            pending: None,
        }
    }
}

/// A presentation on screen: the slides, the thumbnail for the pane, and the slide Quick Look
/// shows.
pub(crate) struct SlidesView {
    pub(crate) deck: Arc<Presentation>,
    thumb: Option<Arc<RenderImage>>,
    pub(crate) page: usize,
}

impl SlidesView {
    pub(super) fn new(deck: Presentation, thumb: Option<Arc<RenderImage>>) -> Self {
        Self {
            deck: Arc::new(deck),
            thumb,
            page: 0,
        }
    }

    /// Drops the thumbnail from the GPU atlas.
    pub(super) fn drop_thumb(self, cx: &mut gpui::App) {
        if let Some(image) = self.thumb {
            cx.drop_image(image, None);
        }
    }
}

/// Estimated width of text at 12 px (CJK and other wide characters count double).
fn text_width(text: &str) -> f32 {
    text.chars()
        .take(64)
        .map(|c| if u32::from(c) >= 0x2E80 { 12. } else { 7. })
        .sum()
}

/// Column widths from the widest cell of each column.
fn column_widths(book: &Workbook) -> Vec<f32> {
    let sheet = &book.sheet;
    (0..sheet.columns())
        .map(|c| {
            let widest = sheet
                .rows
                .iter()
                .filter_map(|row| row.get(c))
                .map(|cell| text_width(&cell.text))
                .fold(0., f32::max);
            (widest + 14.).clamp(COL_MIN, COL_MAX)
        })
        .collect()
}

/// Reads an Office file on a background thread (`sheet` picks a workbook's sheet).
pub(super) fn load_office(
    fs: &dyn Fs,
    path: &Path,
    size: u64,
    kind: OfficeKind,
    sheet: usize,
) -> PreviewData {
    if size > office::MAX_FILE {
        return PreviewData::Unreadable(t!("preview.office_too_large").into_owned());
    }
    let Ok(reader) = fs.open(path) else {
        return PreviewData::Nothing;
    };
    let read = match kind {
        OfficeKind::Document => office::read_document(reader).map(PreviewData::Document),
        OfficeKind::Workbook => office::read_workbook(reader, sheet).map(PreviewData::Workbook),
        OfficeKind::Presentation => office::read_presentation(reader).map(|mut deck| {
            // A thumbnail that doesn't decode (some tools write WMF) just isn't shown.
            let thumb = deck
                .thumbnail
                .take()
                .and_then(|bytes| arke_core::thumbs::decode_photo(&bytes, IMAGE_SIDE).ok())
                .and_then(|photo| crate::media::to_render_image(photo.image));
            PreviewData::Slides { deck, thumb }
        }),
    };
    read.unwrap_or_else(|e| {
        log::info!("office preview of {} failed: {e}", path.display());
        PreviewData::Unreadable(
            match e {
                OfficeError::Protected => t!("preview.office_protected"),
                OfficeError::TooLarge => t!("preview.office_too_large"),
                OfficeError::Malformed | OfficeError::Io(_) => t!("preview.office_unreadable"),
            }
            .into_owned(),
        )
    })
}

/// Documents and workbooks get a page-shaped frame in the pane, like PDFs.
pub(super) fn page_shaped(entry: &Entry) -> bool {
    matches!(
        office::office_kind(&entry.name),
        Some(OfficeKind::Document | OfficeKind::Workbook)
    )
}

/// Extra detail rows: sheet and slide counts.
pub(super) fn details(body: &PreviewBody) -> Option<(String, String)> {
    match body {
        PreviewBody::Workbook(view) => Some((
            t!("preview.sheets").into_owned(),
            arke_core::format::format_count(view.book.names.len()),
        )),
        PreviewBody::Slides(view) => Some((
            t!("preview.slides").into_owned(),
            arke_core::format::format_count(view.deck.count),
        )),
        _ => None,
    }
}

impl Explorer {
    /// A sheet read for the workbook on screen (a click on a sheet tab). Returns `data` when it
    /// was something else.
    pub(super) fn handle_sheet(
        &mut self,
        data: PreviewData,
        cx: &mut Context<Self>,
    ) -> Option<PreviewData> {
        let Some(preview) = self.preview.as_mut() else {
            return Some(data);
        };
        let PreviewBody::Workbook(view) = &mut preview.body else {
            return Some(data);
        };
        if view.pending.is_none() {
            return Some(data);
        }
        match data {
            PreviewData::Workbook(book) => {
                *view = WorkbookView::new(book);
                self.preview_scroll.set_offset(point(px(0.), px(0.)));
                self.look_scroll.set_offset(point(px(0.), px(0.)));
            }
            // A sheet that failed to read: stay on the current one.
            _ => view.pending = None,
        }
        cx.notify();
        None
    }

    /// Shows sheet `index` of the workbook on screen, read in the background while the current
    /// one stays up.
    pub(crate) fn select_sheet(&mut self, index: usize, cx: &mut Context<Self>) {
        let generation = self.preview_gen;
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        let PreviewBody::Workbook(view) = &mut preview.body else {
            return;
        };
        if view.pending.unwrap_or(view.book.index) == index {
            return;
        }
        view.pending = Some(index);
        let (path, size) = (preview.path.clone(), preview.entry.size);
        let fs = self.services.fs.clone();
        let tx = self.events_tx.clone();
        spawn_thread("preview-sheet", move || {
            let data = load_office(fs.as_ref(), &path, size, OfficeKind::Workbook, index);
            let _ = tx.send(AppEvent::Preview { generation, data });
        });
        cx.notify();
    }

    /// Page keys on a presentation in Quick Look: `None` when it isn't one.
    pub(super) fn turn_slide(&mut self, turn: PageTurn, cx: &mut Context<Self>) -> Option<bool> {
        let Some(PreviewBody::Slides(view)) = self.preview.as_mut().map(|p| &mut p.body) else {
            return None;
        };
        let pages = u32::try_from(view.deck.slides.len()).unwrap_or(u32::MAX);
        if pages < 2 {
            return Some(false);
        }
        let page = u32::try_from(view.page).unwrap_or(0);
        if let Some(next) = super::turned(page, pages, turn) {
            view.page = next as usize;
            self.look_scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }
        Some(true)
    }

    /// Quick Look on a workbook: the grid, with sheet tabs below when there are several.
    pub(super) fn workbook_look(&self, view: &WorkbookView, cx: &mut Context<Self>) -> AnyElement {
        let grid = Self::with_preview_scrollbar(
            sheet_grid(view, true, &self.look_scroll),
            &self.look_scroll,
            &self.look_scrollbar,
        );
        if view.book.names.len() < 2 {
            return grid;
        }
        let active = view.pending.unwrap_or(view.book.index);
        let tabs = div()
            .id("sheet-tabs")
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_4)
            .overflow_x_scroll()
            .children(view.book.names.iter().enumerate().map(|(ix, name)| {
                div()
                    .id(("sheet-tab", ix))
                    .h(theme::H_CTL_SM)
                    .px(theme::S_12)
                    .flex()
                    .flex_none()
                    .items_center()
                    .rounded(theme::R_4)
                    .text_size(theme::FS_12)
                    .whitespace_nowrap()
                    .a11y_toggle(SharedString::from(name.clone()), ix == active)
                    .when(ix == active, |d| {
                        d.bg(theme::wash_strong()).text_color(theme::text())
                    })
                    .when(ix != active, |d| {
                        d.cursor_pointer()
                            .text_color(theme::text_muted())
                            .hover(|s| s.bg(theme::wash()))
                    })
                    .child(SharedString::from(name.clone()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select_sheet(ix, cx);
                    }))
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(theme::S_8)
            .child(div().flex_1().min_h_0().child(grid))
            .child(tabs)
            .into_any_element()
    }

    /// Quick Look on a presentation: one slide as a text card on the stage, with the floating
    /// "Slide N of M" pill below like a PDF's pages.
    pub(super) fn slides_look(&self, view: &SlidesView, cx: &mut Context<Self>) -> AnyElement {
        let fit = fit_rect(Bounds::new(Point::default(), self.look_page_area()), 16, 9);
        let pages = view.deck.slides.len();
        let page = view.page.min(pages.saturating_sub(1));
        let card = slide_card(
            view.deck.slides.get(page),
            Some(fit.size),
            true,
            &self.look_scroll,
        );
        div()
            .size_full()
            .pt(PDF_PAD)
            .px(PDF_PAD)
            .pb(PDF_PAD_BOTTOM)
            .flex()
            .items_center()
            .justify_center()
            .child(card)
            .when(pages > 1, |d| {
                d.child(Self::page_pill(
                    t!(
                        "preview.slide_of",
                        page = arke_core::format::format_count(page + 1),
                        pages = arke_core::format::format_count(pages)
                    )
                    .into_owned(),
                    (page > 0, page + 1 < pages),
                    [
                        ("ql-prev-slide", t!("preview.prev_slide").into_owned()),
                        ("ql-next-slide", t!("preview.next_slide").into_owned()),
                    ],
                    cx,
                ))
            })
            .into_any_element()
    }
}

// ───────────────────────── Documents ─────────────────────────

/// A document's blocks, scrollable like the Markdown preview (a reading column in Quick Look).
pub(super) fn document_body(
    doc: &Document,
    large: bool,
    scroll: &gpui::ScrollHandle,
) -> AnyElement {
    div()
        .id("preview-document")
        .size_full()
        .overflow_y_scroll()
        .track_scroll(scroll)
        // Quick Look reads like the Markdown page: a column on the stage's paper.
        .when(large, |d| d.px(theme::S_32).py(theme::S_24))
        .when(!large, |d| d.p(theme::S_12).rounded(theme::R_8).bg(theme::wash()))
        .child(
            div().w_full().flex().justify_center().child(
                div()
                    .w_full()
                    .max_w(theme::READ_W)
                    .child(document_view(doc, large)),
            ),
        )
        .into_any_element()
}

fn document_view(doc: &Document, large: bool) -> AnyElement {
    let body = if large { theme::FS_15 } else { theme::FS_13 };
    let blocks = doc.blocks.iter().map(|block| match block {
        Block::Heading { level, spans } => div()
            .mt(theme::S_4)
            .text_size(match level {
                1 => theme::FS_20,
                2 => theme::FS_15,
                _ => body,
            })
            .font_weight(theme::FW_STRONG)
            .child(rich(spans))
            .into_any_element(),
        Block::Paragraph(spans) if spans.is_empty() => div().h(theme::S_4).into_any_element(),
        Block::Paragraph(spans) => div().child(rich(spans)).into_any_element(),
        Block::ListItem { depth, spans } => div()
            .flex()
            .gap(theme::S_6)
            .pl(theme::S_4 + theme::S_16 * f32::from(*depth))
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text_subtle())
                    .child("•"),
            )
            .child(div().flex_1().min_w_0().child(rich(spans)))
            .into_any_element(),
        Block::Table(rows) => table_view(rows),
        Block::PageBreak => div()
            .my(theme::S_8)
            .h(px(1.))
            .bg(theme::line_strong())
            .into_any_element(),
        Block::Image => div()
            .flex()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_6)
                    .px(theme::S_8)
                    .py(theme::S_4)
                    .rounded(theme::R_4)
                    .bg(theme::wash_strong())
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(kind_icon(arke_core::filetype::FileKind::Image, px(14.)))
                    .child(t!("preview.office_image").into_owned()),
            )
            .into_any_element(),
    });
    div()
        .flex()
        .flex_col()
        .gap(theme::S_6)
        .ui_font()
        .text_size(body)
        .text_color(theme::text())
        .children(blocks)
        .when(doc.truncated, |d| d.child(truncated_note()))
        .into_any_element()
}

/// Text runs with their bold and italic ranges.
fn rich(spans: &[Span]) -> StyledText {
    let mut text = String::new();
    let mut marks = Vec::new();
    for span in spans {
        let start = text.len();
        text.push_str(&span.text);
        if span.bold || span.italic {
            marks.push((
                start..text.len(),
                HighlightStyle {
                    font_weight: span.bold.then_some(FontWeight::BOLD),
                    font_style: span.italic.then_some(FontStyle::Italic),
                    ..HighlightStyle::default()
                },
            ));
        }
    }
    StyledText::new(SharedString::from(text)).with_highlights(marks)
}

/// A table as an even grid with hairlines.
fn table_view(rows: &[Vec<String>]) -> AnyElement {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    div()
        .my(theme::S_4)
        .flex()
        .flex_col()
        .rounded(theme::R_4)
        .border_1()
        .border_color(theme::line())
        .text_size(theme::FS_12)
        .children(rows.iter().enumerate().map(|(r, row)| {
            div()
                .flex()
                .when(r > 0, |d| d.border_t_1().border_color(theme::line()))
                .children((0..cols).map(|c| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(theme::S_6)
                        .py(theme::S_4)
                        .when(c > 0, |d| d.border_l_1().border_color(theme::line()))
                        .child(row.get(c).cloned().unwrap_or_default())
                }))
        }))
        .into_any_element()
}

fn truncated_note() -> AnyElement {
    div()
        .pt(theme::S_8)
        .text_size(theme::FS_12)
        .text_color(theme::text_subtle())
        .child(t!("preview.office_truncated").into_owned())
        .into_any_element()
}

// ───────────────────────── Sheets ─────────────────────────

/// A sheet as a grid: column letters and row numbers stay in place while the cells scroll both
/// ways. Only the rows in view are built.
// Header, gutter and cells share the same scroll math; split up they'd pass it all around.
#[allow(clippy::too_many_lines)]
pub(super) fn sheet_grid(
    view: &WorkbookView,
    large: bool,
    scroll: &gpui::ScrollHandle,
) -> AnyElement {
    let sheet = &view.book.sheet;
    if sheet.rows.is_empty() {
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::R_8)
            .bg(theme::wash())
            .text_size(theme::FS_12)
            .text_color(theme::text_subtle())
            .child(t!("preview.sheet_empty").into_owned())
            .into_any_element();
    }
    let scale = if large { 13. / 12. } else { 1. };
    let widths: Vec<Pixels> = view.widths.iter().map(|w| px(w * scale)).collect();
    let total_w = widths.iter().fold(px(0.), |a, w| a + *w);
    let rows = sheet.rows.len();
    let digits = rows.to_string().len();
    #[allow(clippy::cast_precision_loss)]
    let num_w = px(8. * digits as f32 + 16.);
    let offset = scroll.offset();
    let (off_x, off_y) = ((-offset.x).max(px(0.)), (-offset.y).max(px(0.)));
    let viewport = scroll.bounds().size.height;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (first, visible) = (
        ((off_y / CELL_H).floor().max(0.) as usize).min(rows),
        if viewport > px(0.) {
            (viewport / CELL_H).ceil() as usize + 2
        } else {
            40
        },
    );
    let last = (first + visible).min(rows);
    #[allow(clippy::cast_precision_loss)]
    let row_y = |r: usize| CELL_H * r as f32;
    let hairline = |d: gpui::Div| d.border_color(theme::line());

    let letters = div()
        .absolute()
        .top_0()
        .left(-off_x)
        .h_full()
        .flex()
        .children(widths.iter().enumerate().map(|(c, w)| {
            hairline(
                div()
                    .w(*w)
                    .h_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_r_1(),
            )
            .child(column_name(u32::try_from(c).unwrap_or(0)))
        }));
    let header = hairline(
        div()
            .h(CELL_H)
            .flex_none()
            .flex()
            .bg(theme::wash())
            .border_b_1()
            .text_color(theme::text_subtle()),
    )
    .child(hairline(div().w(num_w).h_full().flex_none().border_r_1()))
    .child(
        div()
            .flex_1()
            .h_full()
            .relative()
            .overflow_hidden()
            .child(letters),
    );
    let numbers = div()
        .absolute()
        .left_0()
        .top(row_y(first) - off_y)
        .w_full()
        .flex()
        .flex_col()
        .children((first..last).map(|r| {
            hairline(
                div()
                    .h(CELL_H)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr(theme::S_6)
                    .border_b_1(),
            )
            .child((r + 1).to_string())
        }));
    let gutter = hairline(
        div()
            .w(num_w)
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .bg(theme::wash())
            .border_r_1()
            .text_color(theme::text_subtle()),
    )
    .child(numbers);
    let cells = div()
        .absolute()
        .left_0()
        .top(row_y(first))
        .flex()
        .flex_col()
        .children(sheet.rows[first..last].iter().map(|row| {
            div()
                .h(CELL_H)
                .flex()
                .flex_none()
                .children(widths.iter().enumerate().map(|(c, w)| {
                    let cell = row.get(c);
                    hairline(
                        div()
                            .w(*w)
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(theme::S_6)
                            .border_r_1()
                            .border_b_1(),
                    )
                    .when(cell.is_some_and(|c| c.numeric), Styled::justify_end)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(cell.map_or_default(|c| c.text.clone()))),
                    )
                }))
        }));
    let body = div()
        .id("preview-sheet")
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_scroll()
        .track_scroll(scroll)
        .child(div().w(total_w).h(row_y(rows)).relative().child(cells));
    let (rows_shown, cols_shown) = (
        arke_core::format::format_count(office::SHEET_ROWS as usize),
        arke_core::format::format_count(office::SHEET_COLS as usize),
    );
    let note = match (sheet.more_rows, sheet.more_cols) {
        (true, true) => Some(t!(
            "preview.sheet_partial",
            rows = rows_shown,
            cols = cols_shown
        )),
        (true, false) => Some(t!("preview.sheet_more_rows", rows = rows_shown)),
        (false, true) => Some(t!("preview.sheet_more_cols", cols = cols_shown)),
        (false, false) => sheet.truncated.then(|| t!("preview.office_truncated")),
    }
    .map(std::borrow::Cow::into_owned);
    hairline(
        div()
            .size_full()
            .flex()
            .flex_col()
            .rounded(theme::R_8)
            .border_1()
            .overflow_hidden()
            .ui_font()
            .text_size(if large { theme::FS_13 } else { theme::FS_12 })
            .text_color(theme::text()),
    )
    .child(header)
    .child(div().flex_1().min_h_0().flex().child(gutter).child(body))
    .children(note.map(|note| {
        hairline(
            div()
                .h(CELL_H)
                .flex_none()
                .flex()
                .items_center()
                .px(theme::S_8)
                .border_t_1()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(div().truncate().child(note)),
        )
    }))
    .into_any_element()
}

// ───────────────────────── Slides ─────────────────────────

/// The pane: the embedded thumbnail, or the first slide's text without one.
pub(super) fn slides_pane(view: &SlidesView) -> AnyElement {
    match &view.thumb {
        Some(image) => img(image.clone())
            .size_full()
            .aspect_ratio(PANE_IMAGE_RATIO)
            .object_fit(ObjectFit::Contain)
            .rounded(theme::R_8)
            .into_any_element(),
        None => slide_card(
            view.deck.slides.first(),
            None,
            false,
            &gpui::ScrollHandle::new(),
        ),
    }
}

/// One slide as a card: the title and the text lines as bullets. `size` fixes the card's size
/// (Quick Look); without it the card fills its frame.
fn slide_card(
    slide: Option<&Slide>,
    size: Option<gpui::Size<Pixels>>,
    large: bool,
    scroll: &gpui::ScrollHandle,
) -> AnyElement {
    let (title_size, line_size) = if large {
        (theme::FS_24, theme::FS_15)
    } else {
        (theme::FS_13, theme::FS_11)
    };
    let indent = if large { theme::S_24 } else { theme::S_12 };
    let card = div()
        .id("slide-card")
        .flex_none()
        .flex()
        .flex_col()
        .gap(if large { theme::S_12 } else { theme::S_4 })
        .p(if large { theme::S_32 } else { theme::S_12 })
        .rounded(theme::R_8)
        .border_1()
        .border_color(theme::line())
        .bg(theme::card())
        .when(large, |d| d.shadow(theme::elev_float()))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .ui_font()
        .text_color(theme::text());
    let card = match size {
        Some(size) => card.w(size.width).h(size.height),
        None => card.size_full(),
    };
    let Some(slide) = slide.filter(|s| !s.title.is_empty() || !s.lines.is_empty()) else {
        return card
            .items_center()
            .justify_center()
            .text_size(line_size)
            .text_color(theme::text_subtle())
            .child(t!("preview.slide_no_text").into_owned())
            .into_any_element();
    };
    card.when(!slide.title.is_empty(), |d| {
        d.child(
            div()
                .flex_none()
                .text_size(title_size)
                .font_weight(theme::FW_STRONG)
                .child(slide.title.clone()),
        )
    })
    .children(slide.lines.iter().map(|(depth, text)| {
        div()
            .flex()
            .flex_none()
            .gap(theme::S_8)
            .pl(indent * f32::from(*depth))
            .text_size(line_size)
            .text_color(theme::text_muted())
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text_subtle())
                    .child("•"),
            )
            .child(div().flex_1().min_w_0().child(text.clone()))
    }))
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_their_widest_cell() {
        let cell = |text: &str| office::Cell {
            text: text.to_owned(),
            numeric: false,
        };
        let book = Workbook {
            names: vec!["S".into()],
            index: 0,
            sheet: office::Sheet {
                rows: vec![
                    vec![cell("a"), cell(&"x".repeat(200)), cell("名前の長い列")],
                    vec![cell("")],
                ],
                ..office::Sheet::default()
            },
        };
        let widths = column_widths(&book);
        assert_eq!(widths.len(), 3);
        assert!((widths[0] - COL_MIN).abs() < f32::EPSILON);
        assert!((widths[1] - COL_MAX).abs() < f32::EPSILON);
        // Six wide characters measure like twelve narrow ones.
        assert!(widths[2] > text_width("abcdefghijkl"));
    }
}
