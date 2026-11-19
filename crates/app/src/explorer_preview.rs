//! Preview: the side preview pane and Quick Look on Space (like macOS).
//!
//! Both show the item under the cursor.
//! - Images: decoded in the background to 1600 px on the long side (own decoder, no third-party
//!   thumbnail handlers).
//! - Text and code: first 64 KB with encoding detection (UTF-8, UTF-16, legacy system code pages
//!   like Big5); binary-looking files are skipped. Code gets line numbers and basic highlighting
//!   (`arke_core::highlight`); Markdown is parsed (`arke_core::markdown`) and laid out like GitHub.
//! - PDFs: rendered by Windows (`win_pdf.rs`), sized for the space in device pixels. The pane
//!   scrolls through every page (`explorer_pdf_pane.rs`), Quick Look turns them (Left/Right, Page
//!   Up/Down, Home/End). Recent Quick Look pages are cached per tab.
//! - Archives: list the contents without extracting (zip reads only the central directory, tar
//!   stops at a limit).
//! - Folders: list the contents (in column view the third column is the next level; clicking enters
//!   and selects).
//! - Office Open XML (`explorer_office.rs`): Word text with basic structure, Excel sheets as a grid
//!   (Quick Look switches sheets), PowerPoint's embedded thumbnail and every slide's text in the
//!   pane, slide text cards in Quick Look (page keys turn slides). Parsed by Arke itself; no
//!   Office or preview handlers.
//! - Anything else: large icon and details.
//!
//! Quick Look is a card over the dimmed window, sized from the window: a title bar (name, details,
//! Open, Open with, Close), the content on a stage (images and PDF pages edge to edge, text like an
//! editor), and a bottom bar with key hints and the item's position.
//!
//! Reading and decoding run on background threads. Fast cursor movement is debounced and stale
//! results dropped.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arke_core::archive::{ArchiveListing, archive_kind};
use arke_core::filetype::{FileKind, file_kind};
use arke_core::format::{format_size, type_label};
use arke_core::highlight::Token;
use arke_core::text::{TextEncoding, decode_text};
use arke_core::{Entry, Fs};
use std::cell::Cell;
use std::rc::Rc;

use arke_core::exif::PhotoInfo;
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, DispatchPhase, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Point, RenderImage, ScrollEvent, SharedString,
    Window, anchored, canvas, deferred, div, img, point, prelude::*, px,
};
use rust_i18n::t;

use super::{AppEvent, Explorer, kind_icon, spawn_thread};
use office::{SlidesView, WorkbookView, load_office};

#[path = "explorer_office.rs"]
mod office;
#[path = "explorer_pdf_pane.rs"]
mod pdf_pane;
use crate::a11y::A11yExt as _;
use crate::shell::Shell;
use crate::theme::{self, UiFont as _};
pub(crate) use pdf_pane::RenderedPage;

/// Delay after the cursor stops before reading, so holding an arrow key doesn't read every row. The
/// test clock doesn't advance on its own, so tests skip the delay.
#[cfg(not(test))]
const PREVIEW_DELAY: Duration = Duration::from_millis(80);
#[cfg(test)]
const PREVIEW_DELAY: Duration = Duration::ZERO;
/// Only draw the loading skeleton after this long; most items finish first, so it doesn't flicker.
const LOADING_GRACE: Duration = Duration::from_millis(250);
/// Skeleton pulse period
const SKELETON_PULSE: Duration = Duration::from_millis(1200);
/// Long side of image previews
const IMAGE_SIDE: u32 = 1600;
/// Bytes read for text previews
const TEXT_HEAD: u64 = 64 * 1024;
/// Max lines in text previews
const TEXT_LINES: usize = 400;
/// Max folder preview entries (beyond that only the total is shown)
const FOLDER_ITEMS: usize = 500;
/// Max archive preview entries
const ARCHIVE_ITEMS: usize = 300;

/// One text preview line: the text and its highlight ranges (byte offsets within the line).
pub(crate) struct CodeLine {
    pub(crate) text: SharedString,
    pub(crate) marks: Vec<(std::ops::Range<usize>, Token)>,
}

/// Split text into lines and distribute highlight ranges per line (multi-line comments and strings
/// are marked on each line).
fn split_lines(text: &str, marks: &[(std::ops::Range<usize>, Token)]) -> Vec<CodeLine> {
    let mut lines = Vec::new();
    let mut next = 0;
    for (start, end) in line_spans(text).take(TEXT_LINES) {
        // Skip ranges that ended before this line.
        while marks.get(next).is_some_and(|(r, _)| r.end <= start) {
            next += 1;
        }
        let line_marks = marks[next..]
            .iter()
            .take_while(|(r, _)| r.start < end)
            .filter_map(|(r, token)| {
                let s = r.start.max(start) - start;
                let e = r.end.min(end) - start;
                (s < e).then_some((s..e, *token))
            })
            .collect();
        lines.push(CodeLine {
            text: SharedString::from(text[start..end].to_owned()),
            marks: line_marks,
        });
    }
    lines
}

/// The first [`TEXT_LINES`] lines of a Markdown file, joined with `\n` (the layout splits on that),
/// with every hard break ending a line as in [`line_spans`].
fn markdown_lines(text: &str) -> String {
    line_spans(text)
        .take(TEXT_LINES)
        .map(|(start, end)| &text[start..end])
        .collect::<Vec<_>>()
        .join("\n")
}

/// Byte ranges of the lines in `text`, without their breaks. Every hard break DirectWrite knows
/// ends a line, not just `\n`: CRLF, a lone `\r` (old Mac CSV exports), VT, FF, NEL and the Unicode
/// line and paragraph separators. Left inside a line, DirectWrite breaks it into more lines than
/// GPUI's layout reads, and the whole line draws empty.
fn line_spans(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut start = Some(0);
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let from = start?;
        while let Some((ix, c)) = chars.next() {
            if matches!(
                c,
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            ) {
                if c == '\r' && chars.next_if(|&(_, n)| n == '\n').is_some() {
                    start = Some(ix + 2);
                } else {
                    start = Some(ix + c.len_utf8());
                }
                return Some((from, ix));
            }
        }
        start = None;
        Some((from, text.len()))
    })
}

pub(crate) enum PreviewBody {
    /// Loading, with the start time (nothing is drawn for the first moment)
    Loading(std::time::Instant),
    Image {
        image: Arc<RenderImage>,
        /// Original size (already rotated per EXIF orientation)
        width: u32,
        height: u32,
        /// Capture info (photos only)
        info: Option<PhotoInfo>,
    },
    /// Text and code: lines with highlight ranges
    Text {
        lines: Arc<Vec<CodeLine>>,
        encoding: TextEncoding,
    },
    /// A rendered PDF page
    Pdf(PdfView),
    /// Markdown, parsed
    Markdown(Arc<arke_core::markdown::Doc>),
    /// Archive contents
    Archive(Arc<ArchiveListing>),
    /// Folder contents (folders first, natural name order) and total count
    Folder {
        entries: Arc<Vec<Entry>>,
        total: usize,
    },
    /// Word document text
    Document(Arc<arke_core::office::Document>),
    /// Excel workbook: one sheet's grid
    Workbook(WorkbookView),
    /// PowerPoint slides (and the embedded thumbnail for the pane)
    Slides(SlidesView),
    /// Nothing to preview (shows a large icon and details)
    Nothing,
    /// Cloud placeholder: content isn't local and reading would trigger a download, so it's skipped
    Cloud,
    /// An archive that can't be listed, with the localized reason (e.g. a RAR on a Windows without
    /// a RAR reader), so it doesn't look like an empty preview
    Unreadable(SharedString),
}

pub(crate) struct Preview {
    pub(crate) path: PathBuf,
    pub(crate) entry: Entry,
    pub(crate) body: PreviewBody,
}

/// Content loaded in the background.
pub(crate) enum PreviewData {
    Image {
        image: Arc<RenderImage>,
        /// Original size (already rotated per EXIF orientation)
        width: u32,
        height: u32,
        /// Capture info (photos only)
        info: Option<PhotoInfo>,
    },
    Text {
        lines: Vec<CodeLine>,
        encoding: TextEncoding,
    },
    /// A PDF page rendered for `target`
    Pdf {
        page: u32,
        target: (u32, u32),
        render: PdfPage,
    },
    Markdown(arke_core::markdown::Doc),
    Archive(ArchiveListing),
    Folder {
        entries: Vec<Entry>,
        total: usize,
    },
    Document(arke_core::office::Document),
    Workbook(arke_core::office::Workbook),
    Slides {
        deck: arke_core::office::Presentation,
        thumb: Option<Arc<RenderImage>>,
    },
    Unreadable(String),
    Nothing,
}

/// A rendered PDF page, ready to draw.
#[derive(Clone)]
pub(crate) struct PdfPage {
    pub(crate) image: Arc<RenderImage>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pages: u32,
}

/// The PDF page on screen.
pub(crate) struct PdfView {
    pub(crate) page: u32,
    pub(crate) render: PdfPage,
    /// Box (device pixels) the page was rendered for
    target: (u32, u32),
    /// A page (and box) being rendered to replace this one
    pending: Option<(u32, (u32, u32))>,
    /// The pane's own pages, scrolled through
    pane: pdf_pane::PanePages,
}

#[cfg(test)]
impl PdfView {
    /// The page being turned to while it renders, else the one on screen.
    pub(crate) fn turning_to(&self) -> u32 {
        self.pending.map_or(self.page, |(page, _)| page)
    }
}

/// Where Quick Look goes for a page key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageTurn {
    Prev,
    Next,
    First,
    Last,
}

/// The page a turn leads to, or `None` at the ends (and for single pages).
pub(crate) const fn turned(page: u32, pages: u32, turn: PageTurn) -> Option<u32> {
    let last = pages.saturating_sub(1);
    let next = match turn {
        PageTurn::Prev if page > 0 => page - 1,
        PageTurn::Next if page < last => page + 1,
        PageTurn::First => 0,
        PageTurn::Last => last,
        _ => return None,
    };
    if next == page { None } else { Some(next) }
}

/// Recently rendered pages (keyed by file identity, page and box), least recently used first.
#[derive(Default)]
pub(crate) struct PdfCache(std::collections::VecDeque<(PdfKey, PdfPage)>);

/// Pages kept per tab (a full-screen page at 200 % is about 30 MB, most are far smaller).
const PDF_CACHE: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PdfKey {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    size: u64,
    page: u32,
    target: (u32, u32),
}

impl PdfKey {
    fn new(path: &Path, entry: &Entry, page: u32, target: (u32, u32)) -> Self {
        Self {
            path: path.to_path_buf(),
            modified: entry.modified,
            size: entry.size,
            page,
            target,
        }
    }
}

impl PdfCache {
    /// A page rendered for this file and a box close to `key`'s (see [`close_enough`]).
    fn get(&mut self, key: &PdfKey) -> Option<PdfPage> {
        let ix = self.0.iter().position(|(k, _)| {
            k.page == key.page
                && k.size == key.size
                && k.modified == key.modified
                && k.path == key.path
                && close_enough(k.target, key.target)
        })?;
        let hit = self.0.remove(ix)?;
        let page = hit.1.clone();
        self.0.push_back(hit);
        Some(page)
    }

    fn put(&mut self, key: PdfKey, page: PdfPage) {
        self.0.retain(|(k, _)| *k != key);
        self.0.push_back((key, page));
        while self.0.len() > PDF_CACHE {
            self.0.pop_front();
        }
    }
}

/// A rendered size close enough to the wanted one that rerendering wouldn't show.
fn close_enough(have: (u32, u32), want: (u32, u32)) -> bool {
    let near = |a: u32, b: u32| a.abs_diff(b) * 10 <= b.max(1);
    near(have.0, want.0) && near(have.1, want.1)
}

/// Render a PDF page on a background thread.
fn load_pdf(shell: &dyn Shell, path: &Path, page: u32, target: (u32, u32)) -> PreviewData {
    match shell.render_pdf(path, page, target) {
        Ok(render) => {
            let (width, height, pages) = (render.image.width, render.image.height, render.pages);
            crate::media::to_render_image(render.image).map_or_else(
                || PreviewData::Unreadable(t!("preview.pdf_unreadable").into_owned()),
                |image| PreviewData::Pdf {
                    page,
                    target,
                    render: PdfPage {
                        image,
                        width,
                        height,
                        pages,
                    },
                },
            )
        }
        Err(e) => {
            log::info!("pdf preview of {} failed: {e}", path.display());
            PreviewData::Unreadable(t!("preview.pdf_unreadable").into_owned())
        }
    }
}

/// Markdown by extension.
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// List a folder: folders first, natural name order, at most [`FOLDER_ITEMS`] entries.
fn load_folder(
    fs: &dyn Fs,
    path: &Path,
    show_hidden: bool,
    sort: arke_core::SortSpec,
) -> PreviewData {
    let Ok(iter) = fs.read_dir(path) else {
        return PreviewData::Nothing;
    };
    let mut entries: Vec<Entry> = iter
        .filter_map(Result::ok)
        .filter(|e| show_hidden || !e.is_hidden())
        .collect();
    sort.sort(&mut entries);
    let total = entries.len();
    entries.truncate(FOLDER_ITEMS);
    PreviewData::Folder { entries, total }
}

/// An archive listing, or why it can't be read (shown instead of a blank preview).
fn archive_preview(
    listing: Result<ArchiveListing, arke_core::archive::ArchiveError>,
) -> PreviewData {
    match listing {
        Ok(listing) => PreviewData::Archive(listing.into_tree_order()),
        Err(e) => PreviewData::Unreadable(e.to_string()),
    }
}

/// Read and decode on a background thread.
fn load(
    fs: &dyn Fs,
    shell: &dyn Shell,
    path: &Path,
    entry: &Entry,
    show_hidden: bool,
    sort: arke_core::SortSpec,
    pdf_target: (u32, u32),
) -> PreviewData {
    let kind = file_kind(entry);
    let size = entry.size;
    if entry.is_dir_like() {
        return load_folder(fs, path, show_hidden, sort);
    }
    if let Some(office) = arke_core::office::office_kind(&entry.name) {
        return load_office(fs, path, size, office, 0);
    }
    if let Some(archive) = archive_kind(&entry.name) {
        let decode = |bytes: &[u8]| shell.decode_ansi(bytes);
        if archive.needs_path() && arke_core::remote::is_remote(path) {
            // libarchive opens the file itself, so it can't read from a server.
            return PreviewData::Nothing;
        }
        if archive.needs_path() {
            // RAR and compressed tars are read by Windows' libarchive, which opens the file (and
            // any further volumes) itself rather than through `Fs`.
            return archive_preview(arke_core::archive::list_path(
                archive,
                path,
                ARCHIVE_ITEMS,
                &decode,
            ));
        }
        let Ok(reader) = fs.open(path) else {
            return PreviewData::Nothing;
        };
        return archive_preview(arke_core::archive::list(
            archive,
            reader,
            ARCHIVE_ITEMS,
            &decode,
        ));
    }
    match kind {
        FileKind::Image if size <= arke_core::filetype::MAX_THUMBNAIL_SOURCE => {
            let Ok(bytes) = fs.read(path, arke_core::filetype::MAX_THUMBNAIL_SOURCE) else {
                return PreviewData::Nothing;
            };
            match arke_core::thumbs::decode_photo(&bytes, IMAGE_SIDE) {
                Ok(photo) => crate::media::to_render_image(photo.image).map_or(
                    PreviewData::Nothing,
                    |image| PreviewData::Image {
                        image,
                        width: photo.width,
                        height: photo.height,
                        info: photo.info,
                    },
                ),
                Err(_) => PreviewData::Nothing,
            }
        }
        FileKind::Pdf => load_pdf(shell, path, 0, pdf_target),
        // A frame, as in the icons view (made by Windows in the thumbnail host). The frame's size
        // is not the video's, so no dimensions are shown for it.
        FileKind::Video if arke_core::filetype::can_video_thumbnail(entry) => shell
            .video_thumbnail(path, IMAGE_SIDE)
            .ok()
            .flatten()
            .and_then(|frame| {
                let (width, height) = (frame.width, frame.height);
                crate::media::to_render_image(frame).map(|image| PreviewData::Image {
                    image,
                    width,
                    height,
                    info: None,
                })
            })
            .unwrap_or(PreviewData::Nothing),
        FileKind::Text | FileKind::Code => {
            let Ok(bytes) = fs.read_head(path, TEXT_HEAD) else {
                return PreviewData::Nothing;
            };
            // Detect the encoding (legacy code pages like Big5 go to the system decoder); skip
            // binary-looking files.
            let decode = |bytes: &[u8]| shell.decode_ansi(bytes);
            let Some((text, encoding)) = decode_text(&bytes, &decode) else {
                return PreviewData::Nothing;
            };
            if is_markdown(path) {
                return PreviewData::Markdown(arke_core::markdown::parse(&markdown_lines(&text)));
            }
            let marks = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(arke_core::highlight::lang_for)
                .map_or_default(|lang| arke_core::highlight::highlight(&text, lang));
            PreviewData::Text {
                lines: split_lines(&text, &marks),
                encoding,
            }
        }
        _ => PreviewData::Nothing,
    }
}

impl Explorer {
    /// The item to preview: the cursor item when it's selected, otherwise the only selected item.
    /// With nothing selected (Esc, a click on empty space, back from another folder) there's
    /// nothing to preview, even though the keyboard cursor still rests on an item; showing it
    /// would contradict the status line. Quick Look follows the cursor regardless, since Space on
    /// the cursor item asks for it explicitly.
    fn preview_target(&self) -> Option<(PathBuf, Entry)> {
        self.item_target(self.quick_look)
    }

    /// [`preview_target`](Self::preview_target); `follow_cursor` takes the cursor item even when
    /// it isn't selected (Quick Look).
    fn item_target(&self, follow_cursor: bool) -> Option<(PathBuf, Entry)> {
        if self.search_mode {
            // Search results are single-select: the cursor is the selection.
            let hit = self.search_hit(Some(self.current_cursor()?))?;
            let root = &self.search_results.as_ref()?.root;
            return Some((root.join(&*hit.rel), hit.entry.clone()));
        }
        let model = self.model.as_ref()?;
        let view = self.view()?;
        let at_cursor = self.current_cursor().and_then(|ix| view.get(ix));
        let entry = match at_cursor {
            Some(entry) if follow_cursor || self.selection.contains(&entry.name) => entry,
            _ if follow_cursor || self.selection.len() != 1 => return None,
            _ => {
                let name = self.selection.names().next()?;
                view.iter().find(|e| e.name == *name)?
            }
        };
        Some((model.path.join(&*entry.name), entry.clone()))
    }

    /// Called on every render: when the preview is open and the cursor moved to another item,
    /// schedule a read.
    #[allow(
        clippy::too_many_lines,
        reason = "One pass over what can happen on a frame (same PDF resized, cache hit, cloud, load); split up, the order of the checks would be harder to follow"
    )]
    pub(super) fn sync_preview(&mut self, cx: &mut Context<Self>) {
        if !(self.quick_look || self.preview_shown() || self.hosts_shared_preview()) {
            return;
        }
        let Some((path, entry)) = self.preview_target() else {
            // Quick Look with nothing to show draws nothing, but left switched on it would pop up
            // again on the next plain click (which moves the cursor). Close it for real.
            self.close_quick_look(cx);
            self.set_preview(None, cx);
            return;
        };
        let pdf_target = self.pdf_target();
        if let Some(preview) = self.preview.as_ref().filter(|p| p.path == path) {
            // Same PDF, but Quick Look opened or closed or the window was resized: render the page
            // again for the new space (the current one stays up meanwhile).
            if let PreviewBody::Pdf(view) = &preview.body
                && view.pending.is_none()
                && !close_enough(view.target, pdf_target)
            {
                let page = view.page;
                self.request_pdf_page(page, cx);
            }
            return;
        }
        // New item: reset "actual size" to fit.
        self.reset_zoom(cx);
        self.preview_gen += 1;
        let generation = self.preview_gen;
        let show_hidden = self.show_hidden;
        let sort = self.sort;
        let target = entry.clone();
        if file_kind(&entry) == FileKind::Pdf
            && !entry.is_cloud_placeholder()
            && let Some(render) = self
                .pdf_cache
                .get(&PdfKey::new(&path, &entry, 0, pdf_target))
        {
            self.set_preview(
                Some(Preview {
                    path,
                    entry,
                    body: PreviewBody::Pdf(PdfView {
                        page: 0,
                        render,
                        target: pdf_target,
                        pending: None,
                        pane: pdf_pane::PanePages::default(),
                    }),
                }),
                cx,
            );
            return;
        }
        if entry.is_cloud_placeholder() {
            // OneDrive online-only: downloading on cursor hover would be alarming, so only on open.
            self.set_preview(
                Some(Preview {
                    path,
                    entry,
                    body: PreviewBody::Cloud,
                }),
                cx,
            );
            return;
        }
        self.set_preview(
            Some(Preview {
                path: path.clone(),
                entry,
                body: PreviewBody::Loading(std::time::Instant::now()),
            }),
            cx,
        );
        // If reading takes long, redraw when the grace period ends to show the skeleton.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOADING_GRACE).await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_gen == generation
                    && matches!(
                        this.preview.as_ref().map(|p| &p.body),
                        Some(PreviewBody::Loading(_))
                    )
                {
                    cx.notify();
                }
            });
        })
        .detach();
        let fs = Arc::clone(&self.services.fs);
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            if !PREVIEW_DELAY.is_zero() {
                cx.background_executor().timer(PREVIEW_DELAY).await;
            }
            let still = this
                .read_with(cx, |this, _| this.preview_gen == generation)
                .unwrap_or(false);
            if !still {
                return;
            }
            spawn_thread("preview", move || {
                // Search hits come without a creation time; a local stat is cheap, a server
                // round trip isn't worth it for one row.
                if target.created.is_none()
                    && !arke_core::remote::is_remote(&path)
                    && let Some(created) = fs.stat(&path).ok().and_then(|e| e.created)
                {
                    let _ = tx.send(AppEvent::PreviewCreated {
                        generation,
                        created,
                    });
                }
                let data = load(
                    fs.as_ref(),
                    shell.as_ref(),
                    &path,
                    &target,
                    show_hidden,
                    sort,
                    pdf_target,
                );
                let _ = tx.send(AppEvent::Preview { generation, data });
            });
        }));
    }

    /// Replace the preview and drop the old image from the GPU atlas so memory doesn't grow while
    /// browsing images.
    fn set_preview(&mut self, preview: Option<Preview>, cx: &mut Context<Self>) {
        match self.preview.take() {
            Some(Preview {
                body: PreviewBody::Image { image, .. },
                ..
            }) => cx.drop_image(image, None),
            // The cache keeps the pixels; only the GPU copy goes.
            Some(Preview {
                body: PreviewBody::Pdf(view),
                ..
            }) => {
                view.pane.release(cx);
                cx.drop_image(view.render.image, None);
            }
            Some(Preview {
                body: PreviewBody::Slides(view),
                ..
            }) => view.drop_thumb(cx),
            _ => {}
        }
        // Different item: scroll back to the top.
        if self.preview.as_ref().map(|p| &p.path) != preview.as_ref().map(|p| &p.path) {
            self.preview_scroll.set_offset(point(px(0.), px(0.)));
            self.look_scroll.set_offset(point(px(0.), px(0.)));
        }
        self.preview = preview;
    }

    pub(super) fn handle_preview(
        &mut self,
        generation: u64,
        data: PreviewData,
        cx: &mut Context<Self>,
    ) {
        if generation != self.preview_gen {
            match data {
                PreviewData::Image { image, .. }
                | PreviewData::Slides {
                    thumb: Some(image), ..
                } => cx.drop_image(image, None),
                _ => {}
            }
            return;
        }
        if self.handle_pdf_page(&data, cx) {
            return;
        }
        let Some(data) = self.handle_sheet(data, cx) else {
            return;
        };
        if let Some(preview) = self.preview.as_mut() {
            preview.body = match data {
                PreviewData::Image {
                    image,
                    width,
                    height,
                    info,
                } => PreviewBody::Image {
                    image,
                    width,
                    height,
                    info,
                },
                PreviewData::Text { lines, encoding } => PreviewBody::Text {
                    lines: Arc::new(lines),
                    encoding,
                },
                PreviewData::Pdf {
                    page,
                    target,
                    render,
                } => {
                    self.pdf_cache.put(
                        PdfKey::new(&preview.path, &preview.entry, page, target),
                        render.clone(),
                    );
                    PreviewBody::Pdf(PdfView {
                        page,
                        render,
                        target,
                        pending: None,
                        pane: pdf_pane::PanePages::default(),
                    })
                }
                PreviewData::Markdown(doc) => PreviewBody::Markdown(Arc::new(doc)),
                PreviewData::Archive(listing) => PreviewBody::Archive(Arc::new(listing)),
                PreviewData::Folder { entries, total } => PreviewBody::Folder {
                    entries: Arc::new(entries),
                    total,
                },
                PreviewData::Document(doc) => PreviewBody::Document(Arc::new(doc)),
                PreviewData::Workbook(book) => PreviewBody::Workbook(WorkbookView::new(book)),
                PreviewData::Slides { deck, thumb } => {
                    PreviewBody::Slides(SlidesView::new(deck, thumb))
                }
                PreviewData::Unreadable(reason) => PreviewBody::Unreadable(reason.into()),
                PreviewData::Nothing => PreviewBody::Nothing,
            };
            cx.notify();
        }
    }

    /// Tests: show `entry` as loading since `since`, and drop any load in flight.
    #[cfg(test)]
    pub(crate) fn show_loading_for_test(
        &mut self,
        path: PathBuf,
        entry: Entry,
        since: std::time::Instant,
        cx: &mut Context<Self>,
    ) {
        self.preview_gen += 1;
        self.preview_task = None;
        self.preview = Some(Preview {
            path,
            entry,
            body: PreviewBody::Loading(since),
        });
        cx.notify();
    }

    /// Tests: the preview pane's scroll position and how far it can scroll each way.
    #[cfg(test)]
    pub(crate) fn preview_scroll_for_test(&self) -> (Point<Pixels>, Point<Pixels>) {
        (
            self.preview_scroll.offset(),
            self.preview_scroll.max_offset(),
        )
    }

    /// Tests: the Info list's rows (label, value) for the item on screen.
    #[cfg(test)]
    pub(crate) fn preview_info_for_test(&self) -> Vec<(String, String)> {
        self.preview
            .as_ref()
            .map_or_default(|p| self.preview_details(p))
    }

    pub(super) fn handle_preview_created(
        &mut self,
        generation: u64,
        created: std::time::SystemTime,
        cx: &mut Context<Self>,
    ) {
        if generation != self.preview_gen {
            return;
        }
        if let Some(preview) = self.preview.as_mut() {
            preview.entry.created = Some(created);
            cx.notify();
        }
    }

    /// Space: the user's own QuickLook app when that setting is on and it's running or installed
    /// (it toggles its preview itself, so Space again closes it), otherwise Arke's Quick Look.
    pub(super) fn space_preview(&mut self, cx: &mut Context<Self>) {
        let on = crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .space_quicklook
            && crate::quicklook::available();
        // Servers (SFTP, WebDAV) are Arke's own; QuickLook can't read them.
        let item = (on && !self.quick_look)
            .then(|| self.item_target(true))
            .flatten()
            .filter(|(path, _)| !arke_core::remote::is_remote(path));
        let Some((item, _)) = item else {
            self.toggle_quick_look(cx);
            return;
        };
        let exe = crate::quicklook::exe();
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("quicklook", move || {
            if let Err(e) = shell.run_quicklook(exe.as_deref(), &item) {
                log::warn!("QuickLook did not start: {e}");
                let _ = tx.send(AppEvent::Notice(Err(t!(
                    "error.quicklook_failed",
                    error = e
                )
                .into_owned())));
            }
        });
    }

    /// Space: toggle Quick Look. Closing fades the card out; the item (and zoom) stay until the
    /// fade is over so the ghost doesn't change under it.
    pub(super) fn toggle_quick_look(&mut self, cx: &mut Context<Self>) {
        self.quick_look = !self.quick_look;
        if self.quick_look {
            self.closing_quick_look = None;
        } else {
            crate::motion::Leaving::start(self, (), |this| &mut this.closing_quick_look, cx);
            if self.closing_quick_look.is_some() {
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(crate::motion::EXIT + std::time::Duration::from_millis(40))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if !this.quick_look {
                            this.after_quick_look(cx);
                        }
                    });
                })
                .detach();
            } else {
                self.after_quick_look(cx);
            }
        }
        cx.notify();
    }

    /// Quick Look is closed (and done fading): back to normal zoom, and drop the item unless the
    /// preview pane shows it.
    fn after_quick_look(&mut self, cx: &mut Context<Self>) {
        self.reset_zoom(cx);
        self.zoom.doc = DOC_ZOOM_NORMAL;
        self.zoom.wheel = 0.;
        if !self.preview_pane {
            self.set_preview(None, cx);
        }
    }

    /// Closes Quick Look if it's open (navigation, window deactivation, nothing left to show).
    pub(super) fn close_quick_look(&mut self, cx: &mut Context<Self>) {
        if self.quick_look {
            self.toggle_quick_look(cx);
        }
    }

    // ───────────────────────── PDF pages ─────────────────────────

    /// Box (device pixels) a PDF page is rendered for: Quick Look's page area, or the pane's
    /// page frame.
    fn pdf_target(&self) -> (u32, u32) {
        let scale = self.screen.1;
        let area = if self.quick_look {
            self.look_pdf_box(scale)
        } else {
            let w = self.pane_frame_w();
            gpui::size(w, self.pdf_frame_h(w))
        };
        let device = |v: Pixels| (f32::from(v) * scale).round().max(1.) as u32;
        (device(area.width), device(area.height))
    }

    /// Width of the pane's preview frame (the pane minus its padding).
    fn pane_frame_w(&self) -> Pixels {
        (self.preview_w() - theme::s_16() * 2.).max(px(1.))
    }

    /// Width of the side preview this pane draws into: its own, or the split tab's shared one.
    fn preview_w(&self) -> Pixels {
        match self.pane.preview_host {
            Some(shared) if self.hosts_shared_preview() => shared.width,
            _ => self.fit.preview_w,
        }
    }

    /// Split tab: the tab's shared preview shows this pane's item. A column view showing its
    /// own last column keeps the item there instead, so it isn't drawn twice.
    pub(super) fn hosts_shared_preview(&self) -> bool {
        self.pane.preview_host.is_some() && !self.preview_shown()
    }

    /// Takes the workspace's layout info for this pane. A side that stops being the shared
    /// preview's host drops its preview and any read still on the way, so a late result from
    /// the side left behind can't land anywhere and its images leave the GPU.
    pub(crate) fn set_pane_info(&mut self, info: super::PaneInfo, cx: &mut Context<Self>) {
        if self.pane == info {
            return;
        }
        let lost_host = self.pane.preview_host.is_some() && info.preview_host.is_none();
        self.pane = info;
        if lost_host && !self.quick_look && !self.preview_shown() {
            self.preview_gen += 1;
            self.preview_task = None;
            self.set_preview(None, cx);
        }
    }

    /// Height of the pane's frame for a PDF: page-shaped, but leaving room for the details below.
    fn pdf_frame_h(&self, width: Pixels) -> Pixels {
        (width * PDF_FRAME_RATIO).min(self.screen.0.height * 0.55)
    }

    /// Height of the pane's frame for images and text: the image ratio, but in a pane dragged
    /// wide it stops growing so the name and details stay on screen.
    fn image_frame_h(&self, width: Pixels) -> Pixels {
        (width / PANE_IMAGE_RATIO).min(self.screen.0.height * 0.45)
    }

    /// Shows page `page` of the PDF on screen: from the cache, or rendered in the background
    /// while the current page stays up.
    fn request_pdf_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let target = self.pdf_target();
        let generation = self.preview_gen;
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        let PreviewBody::Pdf(view) = &mut preview.body else {
            return;
        };
        if let Some(render) =
            self.pdf_cache
                .get(&PdfKey::new(&preview.path, &preview.entry, page, target))
        {
            let old = std::mem::replace(&mut view.render, render);
            view.page = page;
            view.target = target;
            view.pending = None;
            cx.drop_image(old.image, None);
            cx.notify();
            return;
        }
        view.pending = Some((page, target));
        let path = preview.path.clone();
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("pdf-page", move || {
            let data = load_pdf(shell.as_ref(), &path, page, target);
            let _ = tx.send(AppEvent::Preview { generation, data });
        });
        cx.notify();
    }

    /// A page rendered for the PDF already on screen (page turn or new size). Returns whether
    /// `data` was that.
    fn handle_pdf_page(&mut self, data: &PreviewData, cx: &mut Context<Self>) -> bool {
        let Some(preview) = self.preview.as_mut() else {
            return false;
        };
        let PreviewBody::Pdf(view) = &mut preview.body else {
            return false;
        };
        match data {
            PreviewData::Pdf {
                page,
                target,
                render,
            } => {
                self.pdf_cache.put(
                    PdfKey::new(&preview.path, &preview.entry, *page, *target),
                    render.clone(),
                );
                if view.pending == Some((*page, *target)) {
                    let old = std::mem::replace(&mut view.render, render.clone());
                    view.page = *page;
                    view.target = *target;
                    view.pending = None;
                    cx.drop_image(old.image, None);
                    cx.notify();
                }
            }
            // A page that failed to render: stay on the current one.
            _ => view.pending = None,
        }
        true
    }

    /// Quick Look on a PDF with several pages (or a presentation with several slides): turn
    /// pages. Returns whether the key was used (otherwise it moves through the list as usual).
    pub(super) fn turn_pdf_page(&mut self, turn: PageTurn, cx: &mut Context<Self>) -> bool {
        if !self.quick_look {
            return false;
        }
        if let Some(used) = self.turn_slide(turn, cx) {
            return used;
        }
        let Some(PreviewBody::Pdf(view)) = self.preview.as_ref().map(|p| &p.body) else {
            return false;
        };
        if view.render.pages < 2 {
            return false;
        }
        let from = view.pending.map_or(view.page, |(page, _)| page);
        if let Some(page) = turned(from, view.render.pages, turn) {
            self.request_pdf_page(page, cx);
        }
        true
    }

    /// The PDF page on screen, centered in `area` (logical pixels): with a hairline edge in the
    /// pane, `raised` like a sheet on Quick Look's stage.
    fn pdf_page_view(render: &PdfPage, area: gpui::Size<Pixels>, raised: bool) -> AnyElement {
        let fit = fit_rect(
            Bounds::new(Point::default(), area),
            render.width,
            render.height,
        );
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(fit.size.width)
                    .h(fit.size.height)
                    .flex_none()
                    .rounded(theme::r_4())
                    .overflow_hidden()
                    .when(raised, |d| d.shadow(theme::elev_float()))
                    .when(!raised, |d| d.border_1().border_color(theme::line()))
                    .child(
                        img(Arc::clone(&render.image))
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                    ),
            )
            .into_any_element()
    }

    // ───────────────────────── Rendering ─────────────────────────

    /// Under the preview, like File Explorer's details pane: the kind's icon and the name on the
    /// left, the item's actions as icon buttons on the right. The row wraps: when the name and the
    /// actions don't fit side by side (a narrow pane, a long name), the actions move to a line of
    /// their own and the name gets the whole width, wrapping instead of being cut.
    fn pane_title(preview: &Preview, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(theme::s_12())
            .gap_y(theme::s_6())
            .child(
                // Grows from the name's own width (not `flex_1`'s zero basis), so a name too wide
                // to sit next to the actions pushes them to the next line.
                div()
                    .flex_grow_1()
                    .flex_shrink_1()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .gap(theme::s_12())
                    .child(
                        div()
                            .flex_none()
                            .child(kind_icon(file_kind(&preview.entry), theme::icon_lg())),
                    )
                    .child(
                        div()
                            .debug_selector(|| "pane-name".into())
                            .min_w_0()
                            .font_weight(theme::FW_STRONG)
                            .text_size(theme::fs_15())
                            .child(SharedString::from(
                                arke_core::tree::leaf(&preview.entry.name).to_owned(),
                            )),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "pane-actions".into())
                    .ml_auto()
                    .child(Self::pane_actions(preview, cx)),
            )
            .into_any_element()
    }

    /// Open, Open with, Copy path, Properties: what the context menu offers most for one item. Quiet
    /// on purpose (small, with a light hover), so the preview stays the thing to look at.
    fn pane_actions(preview: &Preview, cx: &mut Context<Self>) -> AnyElement {
        let is_dir = preview.entry.is_dir_like();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_2())
            .child(quiet_button(
                "pane-open",
                crate::assets::Icon::External,
                true,
                t!("menu.open"),
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    let Some((path, is_dir)) = this
                        .preview
                        .as_ref()
                        .map(|p| (p.path.clone(), p.entry.is_dir_like()))
                    else {
                        return;
                    };
                    if is_dir {
                        this.navigate(path, arke_core::NavKind::New, cx);
                    } else {
                        this.open_file(path, cx);
                    }
                }),
            ))
            .when(!is_dir, |d| {
                d.child(quiet_button(
                    "pane-open-with",
                    crate::assets::Icon::OpenWith,
                    true,
                    t!("menu.choose_app"),
                    cx.listener(|this, _: &ClickEvent, window, _| {
                        if let Some(path) = this.preview.as_ref().map(|p| p.path.clone()) {
                            this.choose_app(path, window);
                        }
                    }),
                ))
            })
            .child(quiet_button(
                "pane-copy-path",
                crate::assets::Icon::CopyPath,
                true,
                t!("menu.copy_path"),
                cx.listener(|this, _: &ClickEvent, _, _| {
                    if let Some(p) = this.preview.as_ref() {
                        this.copy_paths(&[(p.path.clone(), p.entry.is_dir_like())]);
                    }
                }),
            ))
            .child(quiet_button(
                "pane-properties",
                crate::assets::Icon::Info,
                true,
                t!("menu.properties"),
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    if let Some(p) = this.preview.as_ref() {
                        this.open_properties(vec![(p.path.clone(), p.entry.is_dir_like())], cx);
                    }
                }),
            ))
            .into_any_element()
    }

    /// The details as a titled list: label on the left, value on the right, grouped by spacing
    /// rather than lines. Values wrap instead of being cut, so a narrow pane still shows all of
    /// them. `raised`: drawn on Quick Look's card, which needs the stronger subtle text.
    fn pane_info(&self, preview: &Preview, raised: bool, cx: &mut Context<Self>) -> AnyElement {
        let location = t!("col.location").into_owned();
        let dates = [
            (t!("col.created").into_owned(), preview.entry.created),
            (t!("col.modified").into_owned(), preview.entry.modified),
        ];
        let subtle = if raised {
            theme::text_subtle_raised()
        } else {
            theme::text_subtle()
        };
        // In search results the item's folder isn't the one on screen: its location goes there.
        let reveal = self.search_mode;
        let rows = self.preview_details(preview);
        div()
            .flex()
            .flex_col()
            .text_size(theme::fs_12())
            .gap(theme::s_6())
            .child(
                div()
                    .pb(theme::s_2())
                    .text_color(subtle)
                    .child(t!("preview.info").into_owned()),
            )
            .children(rows.into_iter().enumerate().map(|(ix, (key, value))| {
                let icon = detail_icon(&key);
                let link = reveal && key == location;
                let date = dates
                    .iter()
                    .find(|(label, _)| *label == key)
                    .and_then(|(_, time)| *time);
                let value = div()
                    .debug_selector(move || format!("pane-info-value-{ix}"))
                    .min_w_0()
                    .text_color(theme::text_muted())
                    .map(|d| match date.map(|t| self.date_parts(t)) {
                        // A path wraps at its separators, so all of it shows.
                        None if key == location => d.child(wrappable_path(&value)),
                        None => d.child(value),
                        Some((short, None)) => d.child(short),
                        // "Today 06:06" first, the full date and time dimmed after it (on the next
                        // line when there's no room).
                        Some((short, Some(full))) => d
                            .flex()
                            .flex_wrap()
                            .gap_x(theme::s_6())
                            .child(short)
                            .child(div().text_color(subtle).child(full)),
                    });
                // A label column of fixed width with the value right after it, so the eye doesn't
                // cross the pane to pair them.
                div()
                    .flex()
                    .items_start()
                    .gap(theme::s_12())
                    .child(
                        div()
                            .w(detail_label_w())
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(theme::s_6())
                            .text_color(subtle)
                            .children(icon.map(|icon| {
                                gpui::svg()
                                    .path(icon.path())
                                    .size(crate::theme::scaled_px(14.))
                                    .flex_none()
                                    .text_color(subtle)
                            }))
                            .child(key),
                    )
                    .child(div().flex_1().min_w_0().map(|d| {
                        if link {
                            d.child(
                                value
                                    .id("pane-location")
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme::text()).underline())
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                        if let Some(path) =
                                            this.preview.as_ref().map(|p| p.path.clone())
                                        {
                                            let _ = this.events_tx.send(AppEvent::RevealPath(path));
                                        }
                                    }))
                                    .tooltip(crate::workspace::tooltip(t!(
                                        "menu.open_file_location"
                                    ))),
                            )
                        } else {
                            d.child(value)
                        }
                    }))
            }))
            .into_any_element()
    }

    /// A date as the Info list shows it: the list's short form ("Today 06:06") and, when that
    /// leaves something out, the full date and time to dim after it. Only the full form with the
    /// "full dates" setting on.
    fn date_parts(&self, time: std::time::SystemTime) -> (String, Option<String>) {
        let shell = &self.services.shell;
        let full = shell.format_time(time);
        if crate::prefs::full_dates() {
            return (full, None);
        }
        let short = shell.format_time_short(time);
        if short == full {
            (full, None)
        } else {
            (short, Some(full))
        }
    }

    fn preview_details(&self, preview: &Preview) -> Vec<(String, String)> {
        let shell = &self.services.shell;
        let mut rows = vec![(t!("col.type").to_string(), type_label(&preview.entry))];
        if !preview.entry.is_dir_like() {
            rows.push((t!("col.size").to_string(), format_size(preview.entry.size)));
        }
        if let PreviewBody::Image {
            width,
            height,
            info,
            ..
        } = &preview.body
            && file_kind(&preview.entry) != FileKind::Video
        {
            rows.push((
                t!("preview.dimensions").to_string(),
                format!("{width} × {height}"),
            ));
            if let Some(info) = info {
                for (key, value) in [
                    ("preview.taken", &info.taken),
                    ("preview.camera", &info.camera),
                    ("preview.lens", &info.lens),
                    ("preview.exposure", &info.exposure),
                ] {
                    if let Some(value) = value {
                        rows.push((t!(key).into_owned(), value.clone()));
                    }
                }
            }
        }
        if let PreviewBody::Text { encoding, .. } = &preview.body {
            rows.push((
                t!("preview.encoding").to_string(),
                match encoding {
                    TextEncoding::Legacy => t!("preview.encoding_legacy").into_owned(),
                    other => other.label().to_owned(),
                },
            ));
        }
        if let Some(t) = preview.entry.created {
            rows.push((t!("col.created").to_string(), shell.format_time(t)));
        }
        if let Some(t) = preview.entry.modified {
            rows.push((t!("col.modified").to_string(), shell.format_time(t)));
        }
        if let PreviewBody::Pdf(view) = &preview.body {
            rows.push((
                t!("preview.pages").to_string(),
                arke_core::format::format_count(view.render.pages as usize),
            ));
        }
        rows.extend(office::details(&preview.body));
        rows.push((
            t!("col.location").to_string(),
            preview
                .path
                .parent()
                .map_or_default(|p| p.display().to_string()),
        ));
        rows
    }

    /// Scrollable content (text, Markdown, folders, archives) gets a scrollbar overlay.
    const fn scrolls(body: &PreviewBody) -> bool {
        matches!(
            body,
            PreviewBody::Text { .. }
                | PreviewBody::Markdown(_)
                | PreviewBody::Archive(_)
                | PreviewBody::Folder { .. }
                | PreviewBody::Document(_)
        )
    }

    /// Text that is a table (CSV, TSV): lines stay whole and the preview scrolls sideways instead
    /// of wrapping, so the columns stay lined up.
    fn is_table_text(preview: &Preview) -> bool {
        matches!(preview.body, PreviewBody::Text { .. })
            && std::path::Path::new(preview.entry.name.as_ref())
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("tsv"))
    }

    /// Stack scrollable content with its scrollbar on the right.
    fn with_preview_scrollbar(
        body: AnyElement,
        scroll: &gpui::ScrollHandle,
        drag: &crate::scrollbar::ScrollbarDrag,
    ) -> AnyElement {
        Self::with_preview_scrollbars(body, scroll, drag, None)
    }

    /// [`Self::with_preview_scrollbar`], plus a sideways bar along the bottom for content that can
    /// be wider than its frame.
    fn with_preview_scrollbars(
        body: AnyElement,
        scroll: &gpui::ScrollHandle,
        drag: &crate::scrollbar::ScrollbarDrag,
        hdrag: Option<&crate::scrollbar::ScrollbarDrag>,
    ) -> AnyElement {
        div()
            .relative()
            .size_full()
            .child(body)
            .child(crate::scrollbar::scrollbar(scroll.clone(), Rc::clone(drag)))
            .when_some(hdrag, |d, hdrag| {
                d.child(crate::scrollbar::hscrollbar(
                    scroll.clone(),
                    Rc::clone(hdrag),
                ))
            })
            .into_any_element()
    }

    fn preview_body(
        preview: &Preview,
        scroll: &gpui::ScrollHandle,
        bars: office::SheetBars,
    ) -> AnyElement {
        match &preview.body {
            // Give the element the frame's aspect ratio; otherwise GPUI sizes by the image's own
            // ratio and portrait photos overflow.
            PreviewBody::Image { image, .. } => img(Arc::clone(image))
                .size_full()
                .aspect_ratio(PANE_IMAGE_RATIO)
                .object_fit(ObjectFit::Contain)
                .rounded(theme::r_8())
                .into_any_element(),
            PreviewBody::Pdf(view) => img(Arc::clone(&view.render.image))
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            PreviewBody::Text { lines, .. } => {
                code_view(lines, false, !Self::is_table_text(preview), scroll)
            }
            PreviewBody::Archive(listing) => Self::archive_list(listing, scroll),
            PreviewBody::Markdown(text) => div()
                .id("preview-markdown")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .p(theme::s_12())
                .map(theme::preview_sheet)
                .child(super::markdown_view::markdown(text, false))
                .into_any_element(),
            PreviewBody::Folder { entries, total } => {
                Self::folder_list(&preview.path, entries, *total, scroll, None)
            }
            PreviewBody::Document(doc) => office::document_body(doc, false, scroll),
            PreviewBody::Workbook(view) => office::sheet_grid(view, false, scroll, Some(bars)),
            PreviewBody::Slides(view) => office::slides_pane(view),
            PreviewBody::Loading(since) if media_loading(&preview.entry) => {
                media_placeholder(*since, &preview.entry)
            }
            PreviewBody::Loading(since) => loading_skeleton(*since),
            PreviewBody::Cloud => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(theme::s_8())
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(
                    gpui::svg()
                        .path(crate::assets::Icon::Cloud.path())
                        .size(crate::theme::scaled_px(32.))
                        .text_color(theme::text_subtle()),
                )
                .child(t!("preview.cloud").to_string())
                .into_any_element(),
            PreviewBody::Unreadable(reason) => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(theme::s_8())
                .px(theme::s_12())
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(kind_icon(
                    file_kind(&preview.entry),
                    crate::theme::scaled_px(32.),
                ))
                .child(div().text_center().child(reason.clone()))
                .into_any_element(),
            PreviewBody::Nothing => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(kind_icon(
                    file_kind(&preview.entry),
                    crate::theme::scaled_px(64.),
                ))
                .into_any_element(),
        }
    }

    /// Folder contents. With `cx` the entries are clickable: enter that folder and select the
    /// clicked item.
    fn folder_list(
        dir: &Path,
        entries: &Arc<Vec<Entry>>,
        total: usize,
        scroll: &gpui::ScrollHandle,
        cx: Option<&mut Context<Self>>,
    ) -> AnyElement {
        Self::folder_list_at(dir, entries, total, None, scroll, cx)
    }

    /// [`folder_list`](Self::folder_list); `zoom` is Quick Look's text scale (`None` in the
    /// pane, which keeps the text size around it).
    fn folder_list_at(
        dir: &Path,
        entries: &Arc<Vec<Entry>>,
        total: usize,
        zoom: Option<f32>,
        scroll: &gpui::ScrollHandle,
        mut cx: Option<&mut Context<Self>>,
    ) -> AnyElement {
        let scale = zoom.unwrap_or(1.);
        if entries.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(t!("list.empty").into_owned())
                .into_any_element();
        }
        let rows: Vec<AnyElement> = entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                let row = div()
                    .id(("preview-row", ix))
                    .h((theme::s_24() + theme::s_4()) * scale)
                    .px(theme::s_8())
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::s_8())
                    .rounded(theme::r_8())
                    .when(entry.is_hidden(), |d| d.text_color(theme::text_subtle()))
                    .child(kind_icon(file_kind(entry), theme::icon() * scale))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(Arc::clone(&entry.name))),
                    );
                match cx.as_deref_mut() {
                    Some(cx) => {
                        let dir = dir.to_path_buf();
                        let name = Arc::clone(&entry.name);
                        row.cursor_pointer()
                            .hover(|s| s.bg(theme::wash()))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.navigate_selecting(
                                    dir.clone(),
                                    arke_core::NavKind::New,
                                    Some(Arc::clone(&name)),
                                    cx,
                                );
                            }))
                            .into_any_element()
                    }
                    None => row.into_any_element(),
                }
            })
            .collect();
        let more = total.saturating_sub(entries.len());
        div()
            .id("preview-folder")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .when_some(zoom, |d, zoom| d.text_size(theme::fs_14() * zoom))
            .children(rows)
            .when(more > 0, |d| {
                d.child(
                    div()
                        .px(theme::s_8() * scale)
                        .py(theme::s_4() * scale)
                        .text_size(theme::fs_12() * scale)
                        .text_color(theme::text_subtle())
                        .child(
                            t!(
                                "preview.more_items",
                                count = arke_core::format::format_count(more)
                            )
                            .into_owned(),
                        ),
                )
            })
            .into_any_element()
    }

    /// Archive contents: dimmed parent folder, name, size.
    fn archive_list(listing: &ArchiveListing, scroll: &gpui::ScrollHandle) -> AnyElement {
        Self::archive_list_at(listing, None, scroll)
    }

    /// [`archive_list`](Self::archive_list); `zoom` is Quick Look's text scale (`None` in the
    /// pane, which keeps the text size around it).
    fn archive_list_at(
        listing: &ArchiveListing,
        zoom: Option<f32>,
        scroll: &gpui::ScrollHandle,
    ) -> AnyElement {
        let scale = zoom.unwrap_or(1.);
        let rows: Vec<AnyElement> = listing
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                let (dir, name) = match item.path.rsplit_once('/') {
                    Some((dir, name)) => (Some(format!("{dir}/")), name.to_owned()),
                    None => (None, item.path.clone()),
                };
                let kind = if item.is_dir {
                    FileKind::Folder
                } else {
                    file_kind(&Entry::new(
                        name.as_str(),
                        arke_core::EntryKind::File,
                        item.size,
                        None,
                        0,
                    ))
                };
                div()
                    .id(("archive-row", ix))
                    .h((theme::s_24() + theme::s_4()) * scale)
                    .px(theme::s_8())
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::s_8())
                    .child(kind_icon(kind, theme::icon() * scale))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .children(dir.map(|d| {
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .text_ellipsis_start()
                                    .overflow_hidden()
                                    .text_color(theme::text_subtle())
                                    .child(d)
                            }))
                            .child(div().flex_none().child(name)),
                    )
                    .when(!item.is_dir, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(theme::fs_12() * scale)
                                .text_color(theme::text_subtle())
                                .child(format_size(item.size)),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let shown = listing.items.len();
        let more = (listing.files + listing.dirs).saturating_sub(shown);
        div()
            .id("preview-archive")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .when_some(zoom, |d, zoom| d.text_size(theme::fs_14() * zoom))
            .children(rows)
            .when(more > 0 || !listing.complete, |d| {
                d.child(
                    div()
                        .px(theme::s_8() * scale)
                        .py(theme::s_4() * scale)
                        .text_size(theme::fs_12() * scale)
                        .text_color(theme::text_subtle())
                        .child(if listing.complete {
                            t!(
                                "preview.more_items",
                                count = arke_core::format::format_count(more)
                            )
                            .into_owned()
                        } else {
                            t!("preview.more_unknown").into_owned()
                        }),
                )
            })
            .into_any_element()
    }

    fn details_grid(rows: Vec<(String, String)>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(theme::s_6())
            .children(rows.into_iter().map(|(k, v)| {
                div()
                    .flex()
                    .gap(theme::s_12())
                    .text_size(theme::fs_12())
                    .child(
                        div()
                            .w(crate::theme::scaled_px(64.))
                            .flex_none()
                            .text_color(theme::text_subtle())
                            .child(k),
                    )
                    .child(
                        // Keep the end of long paths (the levels nearest the file).
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .text_color(theme::text_muted())
                            .child(v),
                    )
            }))
    }

    /// Divider on the preview pane's left edge: drag to resize the pane (app-wide), double click
    /// to go back to the automatic width. Same feel as the Details column dividers. In the column
    /// view it is the current folder column's divider instead (drag to size that column, double
    /// click to fit it to its names; the preview takes the rest).
    /// `in_gap`: a split tab's shared preview, whose edge is the gap left of its paper.
    pub(super) fn preview_resize_handle(
        &self,
        in_gap: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dragging = (self.preview_resized
            || self.view_column_resized == Some(crate::prefs::ViewColumn::Current))
            && cx.has_active_drag();
        let edge = DraggedPreviewEdge(cx.entity_id());
        let gap = super::layout::shared_gap();
        div()
            .id("preview-resize")
            .absolute()
            .top_0()
            .bottom_0()
            // Starts on the border and covers the pane's padding: the list's scrollbar stops at
            // the border, so both stay easy to grab.
            .when(!in_gap, |d| d.left_0().w(theme::s_8()))
            // The gap lies over the right pane's box: keep the press from reaching that pane,
            // which would make it the active side and hand the preview (and this drag) over.
            .when(in_gap, |d| d.left(-gap).w(gap).occlude())
            .cursor_col_resize()
            .group("preview-resize")
            .child(
                div()
                    .absolute()
                    .top(if in_gap { theme::r_12() } else { px(0.) })
                    .bottom(if in_gap { theme::r_12() } else { px(0.) })
                    .left(if in_gap { gap / 2. - px(1.) } else { px(0.) })
                    .w(px(2.))
                    .when(in_gap, |d| d.rounded(theme::R_FULL))
                    .when(dragging, |d| d.bg(theme::signal()))
                    .when(!dragging, |d| {
                        d.group_hover("preview-resize", |s| s.bg(theme::signal_line()))
                    }),
            )
            // Where the press started and the width then: the drag moves the edge by as much as
            // the cursor moved, so it neither jumps to the cursor nor lags by the drag threshold.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, _| {
                    this.preview_drag_start = Some((e.position.x, this.preview_w()));
                    // In the column view this edge is the current folder column's divider.
                    this.view_column_drag = (this.columns_active() && !in_gap).then(|| {
                        let current = crate::prefs::ViewColumn::Current;
                        (current, e.position.x, this.view_column_shown_width(current))
                    });
                }),
            )
            .on_drag(edge, |_, _, _, cx| cx.new(|_| super::details::ColumnDragView))
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                if e.click_count() < 2 {
                    return;
                }
                if this.columns_active() && !in_gap {
                    this.fit_view_column(crate::prefs::ViewColumn::Current, window, cx);
                } else {
                    crate::prefs::set_preview_width(None);
                    this.preview_resized = true;
                    this.save_preview_width(cx);
                }
            }))
    }

    /// The preview's edge moved during a drag. The preview is anchored to the right edge:
    /// dragging left widens it by as much as the cursor moved since the press.
    pub(super) fn drag_preview_edge(
        &mut self,
        e: &gpui::DragMoveEvent<DraggedPreviewEdge>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = e.drag(cx).0;
        if pane != cx.entity_id() {
            return;
        }
        if let Some((col, from, start)) = self.view_column_drag {
            self.drag_view_column(col, start + e.event.position.x - from, cx);
            return;
        }
        let Some((from, start)) = self.preview_drag_start else {
            return;
        };
        let room = match self.pane.preview_host {
            Some(shared) if self.hosts_shared_preview() => shared.room,
            _ => {
                super::layout::preview_room(&self.fit_input(self.paper_width, self.screen.0.width))
            }
        };
        let width = (start + from - e.event.position.x).clamp(super::layout::preview_min(), room);
        crate::prefs::set_preview_width(Some(width));
        self.preview_resized = true;
        cx.notify();
    }

    /// After dragging the preview pane's edge: save its width to settings (shared by all windows).
    pub(super) fn save_preview_width(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.preview_resized) {
            return;
        }
        let width = crate::prefs::stored_preview_width();
        log::info!("preview pane width: {width:?}");
        crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(|st| st.preview_width = width, cx);
        });
        cx.notify();
    }

    /// The side preview (pane or the column view's last column) is on and fits at this width.
    pub(super) fn preview_shown(&self) -> bool {
        (self.preview_pane || self.columns_active()) && self.fit.preview
    }

    /// Side preview pane (inside the paper, right of the list).
    /// What the preview pane shows, for screen readers: the item's name and details.
    pub(super) fn preview_a11y_description(&self) -> Option<String> {
        if self.selection.len() > 1 && !self.search_mode {
            return Some(tn!("preview.selected", self.selection.len()).into_owned());
        }
        let preview = self.preview.as_ref()?;
        let details: Vec<String> = self
            .preview_details(preview)
            .into_iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect();
        Some(crate::a11y::item_label(&preview.entry.name, &details))
    }

    pub(super) fn render_preview_pane(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.preview_shown() || self.quick_look {
            return None;
        }
        Some(
            self.preview_column(
                Self::preview_frame(self.fit.preview_w)
                    .border_l_1()
                    .border_color(theme::line()),
                cx,
            ),
        )
    }

    /// The side preview's box, `width` wide.
    pub(super) fn preview_frame(width: Pixels) -> gpui::Div {
        div()
            .w(width)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(theme::s_12())
            .p(theme::s_16())
    }

    /// What the side preview shows in `pane` (from [`Self::preview_frame`]): the item, a
    /// summary of a multiple selection, or a note that nothing is selected.
    #[allow(
        clippy::too_many_lines,
        reason = "Layouts for each kind (no selection, archive, folder, other) are kept together for easy comparison"
    )]
    pub(super) fn preview_column(&self, pane: gpui::Div, cx: &mut Context<Self>) -> AnyElement {
        // Several selected: previewing the cursor item means little, show a summary instead.
        if self.selection.len() > 1 && !self.search_mode {
            return self.selection_summary(pane);
        }
        let Some(preview) = &self.preview else {
            return pane
                .items_center()
                .justify_center()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(t!("preview.nothing_selected").to_string())
                .into_any_element();
        };
        // Archive: name and summary, with the contents filling the column below.
        if let PreviewBody::Archive(listing) = &preview.body {
            return pane
                .child(
                    div()
                        .font_weight(theme::FW_STRONG)
                        .text_size(theme::fs_15())
                        .child(SharedString::from(
                            arke_core::tree::leaf(&preview.entry.name).to_owned(),
                        )),
                )
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(archive_summary(listing)),
                )
                .child(div().flex_1().min_h_0().child(Self::with_preview_scrollbar(
                    Self::archive_list(listing, &self.preview_scroll),
                    &self.preview_scroll,
                    &self.preview_scrollbar,
                )))
                .into_any_element();
        }
        // Folder: name and item count, with the contents below (click to enter and select).
        if let PreviewBody::Folder { entries, total } = &preview.body {
            return pane
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(theme::s_8())
                        .child(
                            div()
                                .flex_none()
                                .child(kind_icon(FileKind::Folder, crate::theme::scaled_px(20.))),
                        )
                        .child(
                            // Wraps like a file's name, so a narrow pane shows all of it.
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_weight(theme::FW_STRONG)
                                .text_size(theme::fs_15())
                                .child(SharedString::from(
                                    arke_core::tree::leaf(&preview.entry.name).to_owned(),
                                )),
                        ),
                )
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(tn!("info.items", *total).into_owned()),
                )
                .child({
                    let (scroll, drag) = (
                        self.preview_scroll.clone(),
                        Rc::clone(&self.preview_scrollbar),
                    );
                    let list = Self::folder_list(&preview.path, entries, *total, &scroll, Some(cx));
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(Self::with_preview_scrollbar(list, &scroll, &drag))
                })
                .into_any_element();
        }
        // The pane's width follows the window or the user's drag, so the frame keeps the image
        // ratio instead of a fixed height (a fixed 200 px let wide panes push the image over the
        // name).
        let frame_w = self.pane_frame_w();
        // PDFs get a page-shaped frame from the start, so the layout doesn't jump once it renders.
        let paged =
            file_kind(&preview.entry) == FileKind::Pdf || office::page_shaped(&preview.entry);
        let frame_h = if paged && !matches!(preview.body, PreviewBody::Cloud) {
            self.pdf_frame_h(frame_w)
        } else {
            self.image_frame_h(frame_w)
        };
        // The frame takes up to `frame_h` and gives height back in a short window, so the name and
        // details below it stay on screen. PDF pages fit the height it got (measured at the last
        // layout; drawn once more when that changes).
        let shown = gpui::size(
            frame_w,
            self.pane_frame_h
                .get()
                .map_or(frame_h, |h| h.clamp(pane_frame_min_h(), frame_h)),
        );
        let measured = std::rc::Rc::clone(&self.pane_frame_h);
        let measure = gpui::canvas(
            move |bounds, _, _| measured.set(Some(bounds.size.height)),
            move |bounds, (), window, _| {
                if bounds.size.height.clamp(pane_frame_min_h(), frame_h) != shown.height {
                    window.refresh();
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        pane.child(
            div()
                .relative()
                .flex_1()
                .min_h(pane_frame_min_h())
                .max_h(frame_h)
                .overflow_hidden()
                .child(measure)
                .child({
                    if let PreviewBody::Pdf(view) = &preview.body {
                        if view.render.pages > 1 {
                            self.pane_pdf_pages(view, shown, cx)
                        } else {
                            Self::pdf_page_view(&view.render, shown, false)
                        }
                    } else if let PreviewBody::Slides(view) = &preview.body
                        && view.deck.slides.len() > 1
                    {
                        Self::with_preview_scrollbar(
                            office::slides_list(view, frame_w, &self.preview_scroll),
                            &self.preview_scroll,
                            &self.preview_scrollbar,
                        )
                    } else {
                        let body = Self::preview_body(
                            preview,
                            &self.preview_scroll,
                            (
                                Rc::clone(&self.preview_scrollbar),
                                Rc::clone(&self.preview_hscrollbar),
                            ),
                        );
                        if Self::scrolls(&preview.body) {
                            // A table reaches past the frame sideways: its own bar.
                            Self::with_preview_scrollbars(
                                body,
                                &self.preview_scroll,
                                &self.preview_scrollbar,
                                Self::is_table_text(preview).then_some(&self.preview_hscrollbar),
                            )
                        } else {
                            body
                        }
                    }
                }),
        )
        .child(div().flex_none().child(Self::pane_title(preview, cx)))
        .child(
            // Only in a window too short for even the smallest frame: the details scroll.
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_shrink(1.)
                .min_h_0()
                .child(
                    div()
                        .id("pane-info-scroll")
                        .flex_initial()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.pane_info_scroll.0)
                        .child(self.pane_info(preview, false, cx)),
                )
                .child(crate::scrollbar::scrollbar(
                    self.pane_info_scroll.0.clone(),
                    Rc::clone(&self.pane_info_scroll.1),
                )),
        )
        .into_any_element()
    }

    /// Quick Look: a large card over the dimmed window. Images and PDFs sit edge to edge on a
    /// stage; the title bar names the item and holds its actions; the bottom bar shows the keys
    /// and where the item is in the list. Arrow keys still move the cursor and the card follows.
    pub(super) fn render_quick_look(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let leaving = match &self.closing_quick_look {
            _ if self.quick_look => None,
            Some(ghost) => Some(ghost.id("quick-look-leave")),
            None => return None,
        };
        let preview = self.preview.as_ref()?;
        let viewport = window.viewport_size();
        let size = look_card(viewport);
        let card = div()
            .id("quick-look")
            .a11y_lazy(gpui::Role::Dialog, || preview.entry.name.to_string())
            .w(size.width)
            .h(size.height)
            .flex()
            .flex_col()
            .rounded(theme::r_12())
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .overflow_hidden()
            .ui_font()
            .text_size(theme::fs_14())
            .text_color(theme::text())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(self.quick_look_header(preview, cx))
            .child(self.quick_look_middle(preview, window, cx))
            .child(self.quick_look_footer(preview, size.width, cx));
        if let Some(id) = leaving {
            // Dismissed: veil and card fade out together. The veil lets clicks through to the list
            // at once; only the card is covered.
            let veil = div()
                .w(viewport.width)
                .h(viewport.height)
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::scrim_strong())
                .child(crate::motion::inert(card));
            return Some(
                deferred(anchored().position(point(px(0.), px(0.))).child(
                    gpui::AnimationExt::with_animation(veil, id, crate::motion::exit(), |el, t| {
                        crate::motion::slide_out(el, t, 0., 0.)
                    }),
                ))
                .with_priority(2)
                .into_any_element(),
            );
        }
        // The veil covers the whole window (sidebar and title bar too) and occludes everything
        // under it, so neither the wheel nor clicks reach the list behind.
        let veil = div()
            .id("quick-look-veil")
            .occlude()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim_strong())
            // The wheel belongs to Quick Look (it scrolls documents and zooms images); the list
            // must not scroll underneath.
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_quick_look(cx)))
            .child(crate::motion::appear(
                card,
                "quick-look-card",
                0.,
                crate::motion::SLIDE_LG,
            ));
        Some(
            deferred(
                anchored()
                    .position(point(px(0.), px(0.)))
                    .child(crate::motion::appear(veil, "quick-look-veil", 0., 0.)),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// Between Quick Look's bars: the stage, all of the card's width (the preview is what Quick
    /// Look is for; the details stay one line in the title bar).
    fn quick_look_middle(
        &self,
        preview: &Preview,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .debug_selector(|| "ql-stage".into())
            .flex_1()
            .min_h_0()
            .relative()
            .border_y_1()
            .border_color(theme::line())
            .bg(look_backdrop(preview))
            .child(self.quick_look_body(preview, window, cx))
            .children(self.doc_zoom_chip(preview, cx))
            .child(Self::look_wheel_capture(cx))
    }

    /// Document zoom chip ("150%"), shown only while zoomed; a click goes back to 100%. Images
    /// have their own.
    fn doc_zoom_chip(&self, preview: &Preview, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.zoom.doc == DOC_ZOOM_NORMAL
            || matches!(
                preview.body,
                PreviewBody::Image { .. } | PreviewBody::Loading(_) | PreviewBody::Nothing
            )
        {
            return None;
        }
        let tip = SharedString::from(t!("preview.doc_zoom_tip").into_owned());
        Some(floating(
            div().id("ql-doc-zoom-pill").child(
                div()
                    .id("ql-doc-zoom")
                    .debug_selector(|| "ql-doc-zoom".into())
                    .a11y_button(tip.clone())
                    .h(theme::h_ctl())
                    .px(theme::s_12())
                    .flex()
                    .items_center()
                    .rounded(theme::R_FULL)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                    .tooltip(super::tooltip(tip))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.reset_doc_zoom(cx);
                    }))
                    .child(format!("{:.0}%", self.doc_zoom() * 100.)),
            ),
            true,
        ))
    }

    /// Quick Look's PDF page on the stage, with the page control. Zoomed past fitting, the page
    /// sits in a scroll view: the wheel scrolls it, then turns the page.
    fn pdf_look(&self, view: &PdfView, cx: &mut Context<Self>) -> AnyElement {
        let controls = (view.render.pages > 1).then(|| Self::page_controls(view, cx));
        if self.doc_zoom() <= 1. {
            return div()
                .size_full()
                .pt(pdf_pad())
                .px(pdf_pad())
                .pb(pdf_pad_bottom())
                .child(Self::pdf_page_view(
                    &view.render,
                    self.look_pdf_area(),
                    true,
                ))
                .children(controls)
                .into_any_element();
        }
        let page = fit_rect(
            Bounds::new(Point::default(), self.look_pdf_area()),
            view.render.width,
            view.render.height,
        )
        .size;
        let sheet = div()
            .w(page.width)
            .h(page.height)
            .flex_none()
            .rounded(theme::r_4())
            .overflow_hidden()
            .shadow(theme::elev_float())
            .child(img(Arc::clone(&view.render.image)).size_full());
        // Sized explicitly: centered while narrower than the stage, scrolling sideways when wider.
        let width = (page.width + pdf_pad() * 2.).max(self.look_area().width);
        let scroller = div()
            .id("ql-pdf-scroll")
            .size_full()
            .overflow_scroll()
            .track_scroll(&self.look_scroll)
            .child(
                div()
                    .w(width)
                    .flex()
                    .justify_center()
                    .pt(pdf_pad())
                    .px(pdf_pad())
                    .pb(pdf_pad_bottom())
                    .child(sheet),
            )
            .into_any_element();
        div()
            .relative()
            .size_full()
            .child(Self::with_preview_scrollbar(
                scroller,
                &self.look_scroll,
                &self.look_scrollbar,
            ))
            .children(controls)
            .into_any_element()
    }

    /// Floating page control over a multi-page PDF: previous, "Page 2 of 14", next.
    fn page_controls(view: &PdfView, cx: &mut Context<Self>) -> AnyElement {
        let page = view.pending.map_or(view.page, |(page, _)| page);
        let pages = view.render.pages;
        Self::page_pill(
            t!(
                "preview.page_of",
                page = arke_core::format::format_count(page as usize + 1),
                pages = arke_core::format::format_count(pages as usize)
            )
            .into_owned(),
            (page > 0, page + 1 < pages),
            [
                ("ql-prev-page", t!("preview.prev_page").into_owned()),
                ("ql-next-page", t!("preview.next_page").into_owned()),
            ],
            cx,
        )
    }

    /// Floating previous / `label` / next pill at the bottom of the stage; the buttons turn pages
    /// like ← and →. `can` enables previous and next, `buttons` gives their ids and tooltips.
    pub(super) fn page_pill(
        label: String,
        can: (bool, bool),
        buttons: [(&'static str, String); 2],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let [(prev_id, prev_tip), (next_id, next_tip)] = buttons;
        floating(
            div()
                .id("ql-pages")
                .child(super::ghost_button(
                    prev_id,
                    crate::assets::Icon::ArrowLeft,
                    can.0,
                    prev_tip,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.turn_pdf_page(PageTurn::Prev, cx);
                    }),
                ))
                .child(
                    div()
                        .min_w(crate::theme::scaled_px(96.))
                        .px(theme::s_4())
                        .flex()
                        .justify_center()
                        .child(label),
                )
                .child(super::ghost_button(
                    next_id,
                    crate::assets::Icon::ArrowRight,
                    can.1,
                    next_tip,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.turn_pdf_page(PageTurn::Next, cx);
                    }),
                )),
            false,
        )
    }

    /// Image zoom chip: "Fit · 38%" or the zoom level; a click switches between fit and actual
    /// size, like a click on the image.
    fn zoom_chip(&self, width: u32, height: u32, scale: f32, cx: &mut Context<Self>) -> AnyElement {
        let actual = actual_size(width, height, scale);
        let label = if self.zoom.on {
            format!("{:.0}%", self.zoom.scale * 100.)
        } else {
            let shown = shown_rect(
                Bounds::new(Point::default(), self.look_area()),
                width,
                height,
                scale,
            );
            let percent = shown.size.width / actual.width * 100.;
            if percent >= 99.5 {
                "100%".to_owned()
            } else {
                format!("{} · {percent:.0}%", t!("preview.zoom_fit"))
            }
        };
        let tip = SharedString::from(t!("preview.zoom_tip").into_owned());
        floating(
            div().id("ql-zoom-pill").child(
                div()
                    .id("ql-zoom")
                    .a11y_button(tip.clone())
                    .h(theme::h_ctl())
                    .px(theme::s_12())
                    .flex()
                    .items_center()
                    .rounded(theme::R_FULL)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                    .tooltip(super::tooltip(tip))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        let view = this.zoom.view.get();
                        this.toggle_zoom(view.center(), window.scale_factor(), cx);
                    }))
                    .child(label),
            ),
            true,
        )
    }

    /// Quick Look title bar: kind icon, name over a details line (type, size, modified...), then
    /// Open, Open with, Show in folder (search results) and Close. A name too long for the bar
    /// shows whole on hover.
    fn quick_look_header(&self, preview: &Preview, cx: &mut Context<Self>) -> impl IntoElement {
        let name = SharedString::from(arke_core::tree::leaf(&preview.entry.name).to_owned());
        let is_dir = preview.entry.is_dir_like();
        div()
            .h(theme::h_bar())
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::s_12())
            .pl(theme::s_16())
            .pr(theme::s_8())
            .child(kind_icon(file_kind(&preview.entry), theme::icon_lg()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("ql-name")
                            .truncate()
                            .font_weight(theme::FW_STRONG)
                            .tooltip(super::tooltip(name.clone()))
                            .child(name),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(theme::fs_12())
                            .text_color(theme::text_subtle_raised())
                            .child(self.look_meta(preview)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::s_4())
                    .child(open_button(
                        "ql-open",
                        false,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.look_open(cx)),
                    ))
                    .when(!is_dir, |d| {
                        d.child(super::ghost_button(
                            "ql-open-with",
                            crate::assets::Icon::OpenWith,
                            true,
                            t!("menu.choose_app"),
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                if let Some(path) = this.preview.as_ref().map(|p| p.path.clone()) {
                                    this.choose_app(path, window);
                                }
                                cx.stop_propagation();
                            }),
                        ))
                    })
                    // In the folder itself the item is already selected behind the card; in search
                    // results, go to it.
                    .when(self.search_mode, |d| {
                        d.child(super::ghost_button(
                            "ql-reveal",
                            crate::assets::Icon::Folder,
                            true,
                            t!("menu.open_file_location"),
                            cx.listener(|this, _: &ClickEvent, _, _| {
                                if let Some(path) = this.preview.as_ref().map(|p| p.path.clone()) {
                                    let _ = this.events_tx.send(AppEvent::RevealPath(path));
                                }
                            }),
                        ))
                    })
                    .child(
                        div()
                            .w(px(1.))
                            .h(theme::s_16())
                            .mx(theme::s_4())
                            .bg(theme::line()),
                    )
                    .child(super::ghost_button(
                        "quick-look-close",
                        crate::assets::Icon::Close,
                        true,
                        format!("{} (Esc)", t!("toast.close")),
                        cx.listener(|this, _: &ClickEvent, _, cx| this.close_quick_look(cx)),
                    )),
            )
    }

    /// The details line under the name: kind, size and what matters for this kind (dimensions,
    /// pages, item count, encoding), then the modified date.
    fn look_meta(&self, preview: &Preview) -> String {
        let entry = &preview.entry;
        let mut parts = vec![type_label(entry)];
        match &preview.body {
            PreviewBody::Folder { total, .. } => {
                parts.push(tn!("info.items", *total).into_owned());
            }
            PreviewBody::Archive(listing) => parts.push(archive_summary(listing)),
            _ if entry.is_dir_like() => {}
            body => {
                parts.push(format_size(entry.size));
                match body {
                    PreviewBody::Image { width, height, .. }
                        if file_kind(entry) != FileKind::Video =>
                    {
                        parts.push(format!("{width} × {height}"));
                    }
                    PreviewBody::Pdf(view) => {
                        parts.push(tn!("preview.pages_n", view.render.pages as usize).into_owned());
                    }
                    PreviewBody::Slides(view) => {
                        parts.push(tn!("preview.slides_n", view.deck.count).into_owned());
                    }
                    PreviewBody::Workbook(view) => {
                        parts.push(tn!("preview.sheets_n", view.book.names.len()).into_owned());
                    }
                    PreviewBody::Text { encoding, .. } => parts.push(match encoding {
                        TextEncoding::Legacy => t!("preview.encoding_legacy").into_owned(),
                        other => other.label().to_owned(),
                    }),
                    _ => {}
                }
            }
        }
        if let Some(time) = entry.modified {
            parts.push(self.services.shell.format_time(time));
        }
        parts.join(" · ")
    }

    /// Quick Look bottom bar: key hints on the left (when there's room), the item's position with
    /// previous / next on the right.
    fn quick_look_footer(
        &self,
        preview: &Preview,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let total = self.list_len();
        let index = self.current_cursor();
        let pages = match &preview.body {
            PreviewBody::Pdf(view) => view.render.pages > 1,
            PreviewBody::Slides(view) => view.deck.slides.len() > 1,
            _ => false,
        };
        let hint = |keys: &[&'static str], label: String| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::s_4())
                .children(keys.iter().copied().map(key_cap))
                .child(div().ml(theme::s_2()).child(label))
        };
        let up = -std::f32::consts::FRAC_PI_2;
        div()
            .h(theme::h_ctx())
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::s_16())
            .pl(theme::s_16())
            .pr(theme::s_8())
            .text_size(theme::fs_12())
            .text_color(theme::text_subtle_raised())
            .when(width >= look_hints_min_w(), |d| {
                d.child(hint(&["↑", "↓"], t!("preview.ql_key_move").into_owned()))
                    .when(pages, |d| {
                        d.child(hint(&["←", "→"], t!("preview.ql_key_pages").into_owned()))
                    })
                    .child(hint(&["Enter"], t!("menu.open").into_owned()))
                    .child(hint(&["Esc"], t!("toast.close").into_owned()))
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::s_2())
                    .child(super::view::ghost_button_turned(
                        "ql-prev-item",
                        crate::assets::Icon::ChevronRight,
                        up,
                        index.is_some_and(|i| i > 0),
                        format!("{} (↑)", t!("preview.prev_item")),
                        cx.listener(|this, _: &ClickEvent, _, cx| this.look_step(false, cx)),
                    ))
                    .child(
                        div()
                            .min_w(crate::theme::scaled_px(56.))
                            .flex()
                            .justify_center()
                            .text_color(theme::text_muted())
                            .child(
                                t!(
                                    "preview.position",
                                    index =
                                        arke_core::format::format_count(index.map_or(0, |i| i + 1)),
                                    total = arke_core::format::format_count(total)
                                )
                                .into_owned(),
                            ),
                    )
                    .child(div().debug_selector(|| "ql-next-item".into()).child(
                        super::view::ghost_button_turned(
                            "ql-next-item",
                            crate::assets::Icon::ChevronRight,
                            -up,
                            index.is_some_and(|i| i + 1 < total),
                            format!("{} (↓)", t!("preview.next_item")),
                            cx.listener(|this, _: &ClickEvent, _, cx| this.look_step(true, cx)),
                        ),
                    )),
            )
    }

    /// Quick Look's Open: enter a folder, or open a file with its default app; either way Quick
    /// Look closes.
    fn look_open(&mut self, cx: &mut Context<Self>) {
        let Some((path, is_dir)) = self
            .preview
            .as_ref()
            .map(|p| (p.path.clone(), p.entry.is_dir_like()))
        else {
            return;
        };
        self.close_quick_look(cx);
        if is_dir {
            self.navigate(path, arke_core::NavKind::New, cx);
        } else {
            self.open_file(path, cx);
        }
    }

    /// Quick Look's previous / next buttons: the item before or after the cursor, as ↑ and ↓ do
    /// in a list.
    fn look_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(cursor) = self.current_cursor() else {
            return;
        };
        let next = if forward {
            cursor + 1
        } else if let Some(prev) = cursor.checked_sub(1) {
            prev
        } else {
            return;
        };
        if next < self.list_len() {
            self.select_index(next, cx);
        }
    }
}

/// Quick Look card size for a window: most of it, keeping a margin on every side, and no larger
/// than [`theme::ql_max_w()`] × [`theme::ql_max_h()`] on big screens.
pub(crate) fn look_card(viewport: gpui::Size<Pixels>) -> gpui::Size<Pixels> {
    let side = |total: Pixels, max: Pixels| {
        let margin = (total * (1. - LOOK_SHARE) / 2.).max(theme::ql_margin());
        (total - margin * 2.).min(max).max(px(1.))
    };
    gpui::size(
        side(viewport.width, theme::ql_max_w()),
        side(viewport.height, theme::ql_max_h()),
    )
}

/// Quick Look's stage (between the title and bottom bars and their hairlines) for a window.
fn look_stage(viewport: gpui::Size<Pixels>) -> gpui::Size<Pixels> {
    let card = look_card(viewport);
    gpui::size(
        card.width,
        (card.height - theme::h_bar() - theme::h_ctx() - px(2.)).max(px(1.)),
    )
}

/// Width of the label column in the preview pane's Info list (the icon and the longest label).
fn detail_label_w() -> gpui::Pixels {
    crate::theme::scaled_px(96.)
}

/// A path that may wrap after each separator: the line breaker doesn't break after a backslash by
/// itself, so a zero-width space after every one gives it the place to. Spaces inside names become
/// no-break spaces, so a name like "Project 2026" isn't split at its space.
fn wrappable_path(path: &str) -> String {
    path.replace(' ', "\u{a0}").replace('\\', "\\\u{200b}")
}

/// The small icon in front of a detail in the preview pane's Info list, by the detail's (translated)
/// label.
fn detail_icon(key: &str) -> Option<crate::assets::Icon> {
    use crate::assets::Icon;
    // One icon per row, so no two rows of one item look alike.
    let icons = [
        ("col.type", Icon::FileExt),
        ("col.size", Icon::Drive),
        ("col.created", Icon::Calendar),
        ("col.modified", Icon::Clock),
        ("col.location", Icon::Folder),
        ("preview.dimensions", Icon::Dimensions),
        ("preview.taken", Icon::Image),
        ("preview.camera", Icon::Camera),
        ("preview.lens", Icon::Lens),
        ("preview.exposure", Icon::Sun),
        ("preview.encoding", Icon::Font),
        ("preview.pages", Icon::PageLine),
        ("preview.sheets", Icon::FtSheet),
        ("preview.slides", Icon::FtSlides),
    ];
    icons
        .into_iter()
        .find(|(label, _)| t!(*label) == key)
        .map(|(_, icon)| icon)
}

/// A small icon button for actions next to content (the preview pane's title row): its icon
/// lights up on a light wash under the mouse, where toolbars' `ghost_button` uses the stronger
/// pressed wash.
fn quiet_button(
    id: &'static str,
    icon: crate::assets::Icon,
    enabled: bool,
    tip: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    use crate::a11y::A11yExt as _;
    use crate::workspace::TooltipBelowExt as _;
    let tip = SharedString::from(tip.to_string());
    div()
        .id(id)
        .group(id)
        .a11y_button(tip.clone())
        .a11y_disabled(!enabled)
        .size(theme::h_ctl_sm())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::r_8())
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(theme::wash()))
                .on_click(on_click)
        })
        .child(
            gpui::svg()
                .path(icon.path())
                .size(theme::icon())
                .text_color(theme::text_muted())
                .when(enabled, |s| {
                    s.group_hover(id, |s| s.text_color(theme::text()))
                }),
        )
        .tooltip_below(tip)
}

/// What sits behind Quick Look's content: the darker stage for images, PDFs and pages to read (so
/// they read edge to edge, or as a sheet), paper for text and code, the card itself for lists and
/// empty states.
fn look_backdrop(preview: &Preview) -> gpui::Rgba {
    match &preview.body {
        // Pages to read sit on the stage too, as a sheet (see the Markdown and document bodies).
        PreviewBody::Image { .. }
        | PreviewBody::Pdf(_)
        | PreviewBody::Slides(_)
        | PreviewBody::Markdown(_)
        | PreviewBody::Document(_) => theme::stage(),
        PreviewBody::Loading(_) if media_loading(&preview.entry) => theme::stage(),
        PreviewBody::Text { .. } => theme::surface(),
        _ => theme::overlay(),
    }
}

/// Kinds Quick Look shows on the darker stage (images, PDF pages, slides).
fn on_stage(entry: &Entry) -> bool {
    matches!(file_kind(entry), FileKind::Image | FileKind::Pdf)
        || arke_core::office::office_kind(&entry.name)
            == Some(arke_core::office::OfficeKind::Presentation)
}

/// Media that waits behind a picture-shaped placeholder: what lands on the stage as a picture
/// (images, PDF pages, slides) and a video, which lands as its frame.
fn media_loading(entry: &Entry) -> bool {
    on_stage(entry) || file_kind(entry) == FileKind::Video
}

/// A pill floating at the bottom of Quick Look's stage (page control, zoom). It occludes, so
/// clicks on it don't reach the image underneath.
/// `end` puts it in the bottom-right corner instead of the middle.
fn floating(pill: gpui::Stateful<gpui::Div>, end: bool) -> AnyElement {
    div()
        .absolute()
        .bottom(theme::s_12())
        .left_0()
        .right_0()
        .px(theme::s_12())
        .flex()
        .when(end, gpui::Styled::justify_end)
        .when(!end, gpui::Styled::justify_center)
        .child(
            pill.occlude()
                .h(theme::h_ctl() + theme::s_4())
                .p(theme::s_2())
                .flex()
                .items_center()
                .gap(theme::s_2())
                .rounded(theme::R_FULL)
                .bg(theme::overlay())
                .shadow(theme::elev_float())
                .text_size(theme::fs_12())
                .text_color(theme::text_muted()),
        )
        .into_any_element()
}

/// "Open" text button (Quick Look's title bar and its empty states); `primary` fills it with the
/// accent.
fn open_button(
    id: &'static str,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let label = SharedString::from(t!("menu.open").into_owned());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_button(label.clone())
        .h(if primary {
            theme::h_ctl()
        } else {
            theme::h_ctl_sm()
        })
        .px(if primary {
            theme::s_16()
        } else {
            theme::s_12()
        })
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .text_size(if primary {
            theme::fs_14()
        } else {
            theme::fs_12()
        })
        .when(primary, |d| {
            d.bg(theme::signal())
                .text_color(theme::on_signal())
                .hover(|s| s.opacity(0.9))
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .on_click(on_click)
        .child(label)
}

/// Quick Look's empty states: the kind icon large, a title, an optional detail line, and an Open
/// button.
fn look_empty(icon: AnyElement, detail: Option<SharedString>, open: AnyElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme::s_8())
        .px(theme::s_24())
        .child(div().mb(theme::s_8()).child(icon))
        .child(
            div()
                .text_size(theme::fs_15())
                .font_weight(theme::FW_STRONG)
                .child(t!("preview.no_preview").into_owned()),
        )
        .children(detail.map(|detail| {
            div()
                .max_w(theme::tooltip_max_w())
                .text_center()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle_raised())
                .child(detail)
        }))
        .child(div().mt(theme::s_12()).child(open))
        .into_any_element()
}

/// A key in Quick Look's hints, like the toolbar's shortcut badges but legible on the card.
fn key_cap(key: &'static str) -> impl IntoElement {
    div()
        .min_w(theme::s_16())
        .px(theme::s_4())
        .flex()
        .justify_center()
        .rounded(theme::r_4())
        .border_1()
        .border_color(theme::line_strong())
        .text_size(theme::fs_11())
        .child(key)
}

/// Smallest card width that still shows the key hints next to the position.
fn look_hints_min_w() -> gpui::Pixels {
    crate::theme::scaled_px(640.)
}
/// Share of the window Quick Look takes (before the margin and size caps).
const LOOK_SHARE: f32 = 0.88;
/// Room around a PDF page on the stage; below it the floating page control sits clear of the page.
fn pdf_pad() -> gpui::Pixels {
    theme::s_24()
}
fn pdf_pad_bottom() -> gpui::Pixels {
    crate::theme::scaled_px(56.)
}

/// Quick Look zoom: fit to window by default; the wheel zooms around the pointer, a click toggles
/// actual size (one image pixel per screen pixel), and a drag pans. Everything else (text,
/// documents, listings, PDF pages) zooms in steps with Ctrl+wheel, kept from item to item until
/// Quick Look closes.
pub(crate) struct Zoom {
    /// Step in [`DOC_ZOOMS`] for everything but images
    doc: usize,
    /// Wheel notches not yet turned into a document zoom step or a page turn (touchpads send
    /// fractions)
    wheel: f32,
    on: bool,
    /// Displayed size relative to actual size (1.0 = 100%) while `on`.
    scale: f32,
    /// Full-resolution image (long side up to [`ZOOM_SIDE`]); until loaded, the preview image is
    /// scaled up
    full: Option<Arc<RenderImage>>,
    generation: u64,
    /// Image top-left relative to the image area's top-left
    pan: Point<Pixels>,
    /// Press position and last position; releasing without much movement counts as a click
    press: Option<(Point<Pixels>, Point<Pixels>)>,
    /// Image area bounds, known after paint
    view: Rc<Cell<Bounds<Pixels>>>,
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            doc: DOC_ZOOM_NORMAL,
            wheel: 0.,
            on: false,
            scale: 0.,
            full: None,
            generation: 0,
            pan: Point::default(),
            press: None,
            view: Rc::default(),
        }
    }
}

/// Quick Look's document zoom steps (text size, or PDF page size relative to fitting the stage).
const DOC_ZOOMS: [f32; 10] = [0.75, 0.9, 1., 1.1, 1.25, 1.5, 1.75, 2., 2.5, 3.];
const DOC_ZOOM_NORMAL: usize = 2;
/// Longest side (device pixels) a zoomed PDF page is rendered at; past it the page is scaled up
/// (a full-screen page at 300 % would otherwise be a 100 MB bitmap).
const PDF_ZOOM_SIDE: f32 = 4096.;

/// Preview pane frame ratio: pane width minus 32 padding, to height
const PANE_IMAGE_RATIO: f32 = 268. / 200.;

/// The preview pane's left edge being dragged, in this pane (in split view both panes see the
/// drag; only the one it started in resizes).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DraggedPreviewEdge(pub(super) gpui::EntityId);
/// Pane frame for PDFs: height to width, a little squatter than A4 so details still fit
const PDF_FRAME_RATIO: f32 = 1.3;
/// The least height the side preview's frame keeps when the window is short (it gives up the rest
/// so the name and details stay on screen).
fn pane_frame_min_h() -> gpui::Pixels {
    crate::theme::scaled_px(96.)
}
/// Long side of the high-res image for "actual size" (4096² × 4 ≈ 64 MB)
const ZOOM_SIDE: u32 = 4096;
/// Zoom change per wheel notch
const ZOOM_STEP: f32 = 1.2;
/// Largest zoom (relative to actual size)
const ZOOM_MAX: f32 = 16.;
/// Max items counted for the multi-select summary (beyond that only the count is shown)
const SUMMARY_MAX: usize = 5_000;
/// Number of kinds listed in the multi-select summary
const KINDS_SHOWN: usize = 6;

/// Fit-to-window rect of the image within the image area (centered, aspect kept).
fn fit_rect(view: Bounds<Pixels>, width: u32, height: u32) -> Bounds<Pixels> {
    let aspect = width as f32 / height.max(1) as f32;
    let (vw, vh) = (view.size.width, view.size.height);
    let fw = vw.min(vh * aspect);
    let fh = fw / aspect;
    Bounds::new(
        point(
            view.origin.x + (vw - fw) / 2.,
            view.origin.y + (vh - fh) / 2.,
        ),
        gpui::size(fw, fh),
    )
}

/// Image size at 100% (logical pixels).
fn actual_size(width: u32, height: u32, scale: f32) -> gpui::Size<Pixels> {
    gpui::size(px(width as f32 / scale), px(height as f32 / scale))
}

/// Where Quick Look draws the image when not zoomed: fitted to the area, but never enlarged past
/// its actual size (`scale` is the window's scale factor). A small image stays sharp at 1:1 in
/// the middle instead of being blown up into a blur; the wheel still zooms past that.
fn shown_rect(view: Bounds<Pixels>, width: u32, height: u32, scale: f32) -> Bounds<Pixels> {
    let fit = fit_rect(view, width, height);
    let actual = actual_size(width, height, scale);
    if fit.size.width <= actual.width {
        return fit;
    }
    Bounds::new(
        point(
            view.origin.x + (view.size.width - actual.width) / 2.,
            view.origin.y + (view.size.height - actual.height) / 2.,
        ),
        actual,
    )
}

/// An image larger than the area can't be dragged past its edges; a smaller one stays centered.
fn clamp_pan(
    pan: Point<Pixels>,
    content: gpui::Size<Pixels>,
    view: gpui::Size<Pixels>,
) -> Point<Pixels> {
    let axis = |p: Pixels, c: Pixels, v: Pixels| {
        if c > v {
            p.max(v - c).min(px(0.))
        } else {
            (v - c) / 2.
        }
    };
    point(
        axis(pan.x, content.width, view.width),
        axis(pan.y, content.height, view.height),
    )
}

impl Explorer {
    /// Back to fit-to-window, dropping the high-res image.
    pub(super) fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        if let Some(image) = self.zoom.full.take() {
            cx.drop_image(image, None);
        }
        self.zoom.on = false;
        self.zoom.press = None;
        self.zoom.generation += 1;
    }

    pub(super) fn handle_zoom_image(
        &mut self,
        generation: u64,
        image: Option<Arc<RenderImage>>,
        cx: &mut Context<Self>,
    ) {
        match image {
            Some(image) if generation == self.zoom.generation && self.zoom.on => {
                if let Some(old) = self.zoom.full.replace(image) {
                    cx.drop_image(old, None);
                }
                cx.notify();
            }
            Some(image) => cx.drop_image(image, None),
            None => {}
        }
    }

    /// Click toggles fit and actual size. When zooming in, the clicked point stays centered.
    fn toggle_zoom(&mut self, at: Point<Pixels>, scale: f32, cx: &mut Context<Self>) {
        let Some(Preview {
            path,
            body: PreviewBody::Image { width, height, .. },
            ..
        }) = &self.preview
        else {
            return;
        };
        let (width, height, path) = (*width, *height, path.clone());
        if self.zoom.on {
            self.zoom.on = false;
            cx.notify();
            return;
        }
        let view = self.zoom.view.get();
        let fit = shown_rect(view, width, height, scale);
        let fx = ((at.x - fit.origin.x) / fit.size.width).clamp(0., 1.);
        let fy = ((at.y - fit.origin.y) / fit.size.height).clamp(0., 1.);
        let content = actual_size(width, height, scale);
        let pan = point(
            view.size.width / 2. - content.width * fx,
            view.size.height / 2. - content.height * fy,
        );
        self.zoom.pan = clamp_pan(pan, content, view.size);
        self.zoom.on = true;
        self.zoom.scale = 1.;
        self.load_full_image(path, width, height);
        cx.notify();
    }

    /// Wheel over the image: zoom in or out by `notches` (positive = in), keeping the image point
    /// under the pointer `at` in place. Zooming out past fit-to-window goes back to fit.
    fn wheel_zoom(&mut self, at: Point<Pixels>, notches: f32, scale: f32, cx: &mut Context<Self>) {
        let Some(Preview {
            path,
            body: PreviewBody::Image { width, height, .. },
            ..
        }) = &self.preview
        else {
            return;
        };
        let (width, height, path) = (*width, *height, path.clone());
        let view = self.zoom.view.get();
        if view.size.width <= px(0.) || notches == 0. {
            return;
        }
        let actual = actual_size(width, height, scale);
        // Everything below is relative to the image area's top-left.
        let fit = shown_rect(
            Bounds::new(Point::default(), view.size),
            width,
            height,
            scale,
        );
        let fit_scale = fit.size.width / actual.width;
        let (current, pan) = if self.zoom.on {
            (self.zoom.scale, self.zoom.pan)
        } else {
            (fit_scale, fit.origin)
        };
        let next = (current * ZOOM_STEP.powf(notches)).min(ZOOM_MAX);
        if next <= fit_scale {
            self.zoom.on = false;
            cx.notify();
            return;
        }
        let pointer = at - view.origin;
        let shown = gpui::size(actual.width * current, actual.height * current);
        let fx = (pointer.x - pan.x) / shown.width;
        let fy = (pointer.y - pan.y) / shown.height;
        let content = gpui::size(actual.width * next, actual.height * next);
        let pan = point(
            pointer.x - content.width * fx,
            pointer.y - content.height * fy,
        );
        self.zoom.pan = clamp_pan(pan, content, view.size);
        self.zoom.scale = next;
        self.zoom.on = true;
        self.load_full_image(path, width, height);
        cx.notify();
    }

    /// The preview image is at most `IMAGE_SIDE` on the long side; when zoomed, load a sharp one
    /// in the background if the original is larger.
    fn load_full_image(&mut self, path: PathBuf, width: u32, height: u32) {
        if self.zoom.full.is_none() && width.max(height) > IMAGE_SIDE {
            self.zoom.generation += 1;
            let generation = self.zoom.generation;
            let fs = Arc::clone(&self.services.fs);
            let tx = self.events_tx.clone();
            spawn_thread("zoom-image", move || {
                let image = fs
                    .read(&path, arke_core::filetype::MAX_THUMBNAIL_SOURCE)
                    .ok()
                    .and_then(|bytes| arke_core::thumbs::decode_photo(&bytes, ZOOM_SIDE).ok())
                    .and_then(|photo| crate::media::to_render_image(photo.image));
                let _ = tx.send(AppEvent::ZoomImage { generation, image });
            });
        }
    }

    /// Displayed image size while zoomed.
    fn zoomed_size(&self, width: u32, height: u32, scale: f32) -> gpui::Size<Pixels> {
        let actual = actual_size(width, height, scale);
        gpui::size(
            actual.width * self.zoom.scale,
            actual.height * self.zoom.scale,
        )
    }

    fn pan_zoom(&mut self, delta: Point<Pixels>, scale: f32, cx: &mut Context<Self>) {
        let Some(PreviewBody::Image { width, height, .. }) = self.preview.as_ref().map(|p| &p.body)
        else {
            return;
        };
        let content = self.zoomed_size(*width, *height, scale);
        self.zoom.pan = clamp_pan(self.zoom.pan + delta, content, self.zoom.view.get().size);
        cx.notify();
    }

    /// Quick Look's image area: the stage between the title and bottom bars. Computed from the
    /// window rather than measured, so the first frame and the PDF render size already match.
    fn look_area(&self) -> gpui::Size<Pixels> {
        look_stage(self.screen.0)
    }

    /// Box a PDF page fits in on the stage (inside the room around it).
    fn look_page_area(&self) -> gpui::Size<Pixels> {
        let stage = self.look_area();
        gpui::size(
            (stage.width - pdf_pad() * 2.).max(px(1.)),
            (stage.height - pdf_pad() - pdf_pad_bottom()).max(px(1.)),
        )
    }

    /// Box a PDF page is drawn in at the document zoom: the page area scaled.
    fn look_pdf_area(&self) -> gpui::Size<Pixels> {
        let area = self.look_page_area();
        let zoom = self.doc_zoom();
        gpui::size(area.width * zoom, area.height * zoom)
    }

    /// Box a Quick Look PDF page is rendered for (logical pixels): the zoomed page area, no
    /// larger than [`PDF_ZOOM_SIDE`] device pixels on its long side (`scale` is the window's).
    fn look_pdf_box(&self, scale: f32) -> gpui::Size<Pixels> {
        let area = self.look_pdf_area();
        let long = f32::from(area.width.max(area.height)) * scale;
        let shrink = (PDF_ZOOM_SIDE / long.max(1.)).min(1.);
        gpui::size(area.width * shrink, area.height * shrink)
    }

    /// Quick Look's document zoom (1.0 = normal).
    pub(crate) fn doc_zoom(&self) -> f32 {
        DOC_ZOOMS[self.zoom.doc.min(DOC_ZOOMS.len() - 1)]
    }

    /// Ctrl+wheel in Quick Look: `notches` (positive = in) zoom an image around `at`, or step the
    /// document zoom once per whole notch.
    fn look_zoom_wheel(
        &mut self,
        at: Point<Pixels>,
        notches: f32,
        scale: f32,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Image { .. })
        ) {
            self.wheel_zoom(at, notches, scale, cx);
            return;
        }
        self.zoom.wheel += notches;
        let whole = self.zoom.wheel.trunc();
        if whole != 0. {
            self.zoom.wheel -= whole;
            // A wheel never sends anywhere near i32's range; the steps are clamped anyway.
            self.step_doc_zoom(whole.clamp(-16., 16.) as i32, cx);
        }
    }

    /// Moves the document zoom `steps` steps (clamped to [`DOC_ZOOMS`]).
    fn step_doc_zoom(&mut self, steps: i32, cx: &mut Context<Self>) {
        let last = DOC_ZOOMS.len() - 1;
        let next = self
            .zoom
            .doc
            .saturating_add_signed(steps as isize)
            .min(last);
        if next != self.zoom.doc {
            self.zoom.doc = next;
            cx.notify();
        }
    }

    /// Ctrl+= / Ctrl+- (`Some(±1)`) and Ctrl+0 (`None`) while Quick Look is open; otherwise the
    /// keys pass on.
    pub(super) fn look_zoom_key(
        &mut self,
        steps: Option<i32>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.quick_look {
            cx.propagate();
            return;
        }
        let image = matches!(
            self.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Image { .. })
        );
        match steps {
            Some(steps) if image => {
                let center = self.zoom.view.get().center();
                self.wheel_zoom(center, steps as f32, window.scale_factor(), cx);
            }
            Some(steps) => self.step_doc_zoom(steps, cx),
            None if image => {
                self.zoom.on = false;
                cx.notify();
            }
            None => self.reset_doc_zoom(cx),
        }
    }

    /// Document zoom back to 100%.
    fn reset_doc_zoom(&mut self, cx: &mut Context<Self>) {
        if self.zoom.doc != DOC_ZOOM_NORMAL {
            self.zoom.doc = DOC_ZOOM_NORMAL;
            cx.notify();
        }
    }

    /// Plain wheel over a PDF or slides in Quick Look: scrolls the page while it can (zoomed in),
    /// then turns to the next or previous one, landing on its top or bottom so reading flows on.
    /// Returns whether the wheel was used for a page turn.
    fn look_wheel_pages(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let (pages, scrolls) = match self.preview.as_ref().map(|p| &p.body) {
            // The fitted page isn't in a scroll view; only a zoomed one is.
            Some(PreviewBody::Pdf(view)) => (view.render.pages, self.doc_zoom() > 1.),
            Some(PreviewBody::Slides(view)) => (
                u32::try_from(view.deck.slides.len()).unwrap_or(u32::MAX),
                true,
            ),
            _ => return false,
        };
        if pages < 2 || delta.y == px(0.) {
            return false;
        }
        let down = delta.y < px(0.);
        if scrolls {
            let (offset, max) = (self.look_scroll.offset().y, self.look_scroll.max_offset().y);
            let at_edge = if down {
                offset <= -max + px(0.5)
            } else {
                offset >= px(-0.5)
            };
            if !at_edge {
                self.zoom.wheel = 0.;
                return false;
            }
        }
        let line = crate::theme::scaled_px(20.);
        self.zoom.wheel += delta.y / (line * 3.);
        if self.zoom.wheel.abs() < 1. {
            return true;
        }
        self.zoom.wheel = 0.;
        let turn = if down { PageTurn::Next } else { PageTurn::Prev };
        let x = self.look_scroll.offset().x;
        let before = self.look_page();
        self.turn_pdf_page(turn, cx);
        // At the first or last page nothing turns and the page stays where it is.
        if self.look_page() != before {
            self.look_scroll.set_offset(point(x, px(0.)));
            if !down {
                // Going back lands at the page's end.
                self.look_scroll.scroll_to_bottom();
            }
        }
        true
    }

    /// The PDF page or slide Quick Look shows (or is about to).
    fn look_page(&self) -> Option<usize> {
        match self.preview.as_ref().map(|p| &p.body) {
            Some(PreviewBody::Pdf(view)) => {
                Some(view.pending.map_or(view.page, |(page, _)| page) as usize)
            }
            Some(PreviewBody::Slides(view)) => Some(view.page),
            _ => None,
        }
    }

    /// Invisible layer over Quick Look's stage taking Ctrl+wheel (zoom) and, on PDFs and slides,
    /// the wheel at a page's end (page turns) before the content scrolls (capture phase).
    fn look_wheel_capture(cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        canvas(
            |bounds, _, _| bounds,
            move |_, bounds, window, _| {
                window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || !bounds.contains(&e.position) {
                        return;
                    }
                    let line = crate::theme::scaled_px(20.);
                    let delta = e.delta.pixel_delta(line);
                    let scale = window.scale_factor();
                    let used = this.update(cx, |this, cx| {
                        if !this.quick_look {
                            return false;
                        }
                        if e.modifiers.control {
                            this.look_zoom_wheel(e.position, delta.y / (line * 3.), scale, cx);
                            return true;
                        }
                        this.look_wheel_pages(delta, cx)
                    });
                    if used {
                        cx.stop_propagation();
                    }
                });
            },
        )
        // Pinned to the corner: without insets an absolute box sits where it would flow, below
        // the stage's content.
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// Quick Look body: images fill the stage and zoom with the wheel, toggle actual size on click
    /// and pan by drag; PDF pages sit on the stage with a page control; text reads like an editor;
    /// items without a preview get a large icon and an Open button.
    #[allow(
        clippy::too_many_lines,
        reason = "The image's mouse handling is one chain; the other kinds return early above it"
    )]
    fn quick_look_body(
        &self,
        preview: &Preview,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = || {
            open_button(
                "ql-empty-open",
                true,
                cx.listener(|this, _: &ClickEvent, _, cx| this.look_open(cx)),
            )
            .into_any_element()
        };
        let kind = file_kind(&preview.entry);
        let zoom = self.doc_zoom();
        let (width, height, image) = match &preview.body {
            PreviewBody::Image {
                image,
                width,
                height,
                ..
            } => (*width, *height, image),
            PreviewBody::Pdf(view) => return self.pdf_look(view, cx),
            PreviewBody::Text { lines, .. } => {
                let wrap = !Self::is_table_text(preview);
                return Self::with_preview_scrollbars(
                    code_view_at(lines, Some(zoom), wrap, &self.look_scroll),
                    &self.look_scroll,
                    &self.look_scrollbar,
                    (!wrap).then_some(&self.look_hscrollbar),
                );
            }
            PreviewBody::Markdown(text) => {
                let page = div()
                    .id("preview-markdown")
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .overflow_y_scroll()
                    .track_scroll(&self.look_scroll)
                    .p(theme::s_24())
                    .child(
                        div()
                            .w_full()
                            // Zoomed text keeps as many words per line, up to the stage's width.
                            .max_w(theme::read_w() * zoom)
                            .px(theme::s_32())
                            .py(theme::s_24())
                            .map(theme::preview_sheet)
                            .rounded(theme::r_12())
                            .child(super::markdown_view::markdown_at(text, true, zoom)),
                    )
                    .into_any_element();
                return Self::with_preview_scrollbar(page, &self.look_scroll, &self.look_scrollbar);
            }
            PreviewBody::Nothing => {
                let mut detail = type_label(&preview.entry);
                if !preview.entry.is_dir_like() {
                    detail = format!("{detail} · {}", format_size(preview.entry.size));
                }
                return look_empty(
                    kind_icon(kind, theme::icon_hero()),
                    Some(detail.into()),
                    open(),
                );
            }
            PreviewBody::Cloud => {
                return look_empty(
                    gpui::svg()
                        .path(crate::assets::Icon::Cloud.path())
                        .size(theme::icon_hero() / 2.)
                        .text_color(theme::text_subtle())
                        .into_any_element(),
                    Some(t!("preview.cloud").into_owned().into()),
                    open(),
                );
            }
            PreviewBody::Unreadable(reason) => {
                return look_empty(
                    kind_icon(kind, theme::icon_hero()),
                    Some(reason.clone()),
                    open(),
                );
            }
            PreviewBody::Document(doc) => {
                return Self::with_preview_scrollbar(
                    office::document_at(doc, Some(zoom), &self.look_scroll),
                    &self.look_scroll,
                    &self.look_scrollbar,
                );
            }
            PreviewBody::Workbook(view) => {
                return div()
                    .size_full()
                    .p(theme::s_8())
                    .child(self.workbook_look(view, cx))
                    .into_any_element();
            }
            PreviewBody::Slides(view) => return self.slides_look(view, cx),
            // Pictures, video frames, PDFs and slides wait on the stage with the same pulsing
            // frame as the side pane: bars would flash where the picture lands.
            PreviewBody::Loading(since) if media_loading(&preview.entry) => {
                return div()
                    .size_full()
                    .p(theme::s_8())
                    .child(media_placeholder(*since, &preview.entry))
                    .into_any_element();
            }
            PreviewBody::Loading(_) | PreviewBody::Folder { .. } | PreviewBody::Archive(_) => {
                let body = match &preview.body {
                    PreviewBody::Archive(listing) => {
                        Self::archive_list_at(listing, Some(zoom), &self.look_scroll)
                    }
                    PreviewBody::Folder { entries, total } => Self::folder_list_at(
                        &preview.path,
                        entries,
                        *total,
                        Some(zoom),
                        &self.look_scroll,
                        None,
                    ),
                    _ => Self::preview_body(
                        preview,
                        &self.look_scroll,
                        (
                            Rc::clone(&self.look_scrollbar),
                            Rc::clone(&self.look_hscrollbar),
                        ),
                    ),
                };
                let body = if Self::scrolls(&preview.body) {
                    Self::with_preview_scrollbar(body, &self.look_scroll, &self.look_scrollbar)
                } else {
                    body
                };
                return div()
                    .size_full()
                    .p(theme::s_8())
                    .child(body)
                    .into_any_element();
            }
        };
        let view = Rc::clone(&self.zoom.view);
        let capture = canvas(move |bounds, _, _| view.set(bounds), |_, (), _, _| {})
            .absolute()
            .size_full();
        let picture = if self.zoom.on {
            let content = self.zoomed_size(width, height, window.scale_factor());
            let source = self.zoom.full.clone().unwrap_or_else(|| Arc::clone(image));
            img(source)
                .absolute()
                .left(self.zoom.pan.x)
                .top(self.zoom.pan.y)
                .w(content.width)
                .h(content.height)
                .into_any_element()
        } else {
            // Compute the fit rect ourselves; GPUI's img sizes by the image ratio and crops
            // portrait photos.
            let area = Bounds::new(Point::default(), self.look_area());
            let fit = shown_rect(area, width, height, window.scale_factor());
            img(Arc::clone(image))
                .absolute()
                .left(fit.origin.x)
                .top(fit.origin.y)
                .w(fit.size.width)
                .h(fit.size.height)
                .into_any_element()
        };
        let stage = div()
            .id("ql-image")
            .relative()
            .size_full()
            .overflow_hidden()
            .when(self.zoom.on, |d| d.cursor(gpui::CursorStyle::OpenHand))
            .when(!self.zoom.on, |d| d.cursor(gpui::CursorStyle::PointingHand))
            .child(capture)
            .child(picture)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.zoom.press = Some((e.position, e.position));
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                let Some((start, last)) = this.zoom.press else {
                    return;
                };
                if e.pressed_button != Some(MouseButton::Left) {
                    this.zoom.press = None;
                    return;
                }
                if this.zoom.on {
                    this.pan_zoom(e.position - last, window.scale_factor(), cx);
                }
                this.zoom.press = Some((start, e.position));
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, window, cx| {
                    let Some((start, _)) = this.zoom.press.take() else {
                        return;
                    };
                    let moved = e.position - start;
                    if moved.x.abs() < crate::theme::scaled_px(4.) && moved.y.abs() < crate::theme::scaled_px(4.) {
                        this.toggle_zoom(e.position, window.scale_factor(), cx);
                    }
                    cx.stop_propagation();
                }),
            )
            // Wheel zooms around the pointer (like image viewers). Windows reports a wheel notch as
            // three lines by default, so three lines (or the same distance in touchpad pixels)
            // make one zoom step.
            .on_scroll(cx.listener(|this, e: &ScrollEvent, window, cx| {
                let line = crate::theme::scaled_px(20.);
                let notches = e.delta.pixel_delta(line).y / (line * 3.);
                this.wheel_zoom(e.position, notches, window.scale_factor(), cx);
                cx.stop_propagation();
            }));
        div()
            .relative()
            .size_full()
            .child(stage)
            .child(self.zoom_chip(width, height, window.scale_factor(), cx))
            .into_any_element()
    }

    /// Modified-date range of the selection (a single value if all share the same minute).
    fn modified_range(&self, entries: &[&Entry]) -> Option<String> {
        let times: Vec<std::time::SystemTime> = entries.iter().filter_map(|e| e.modified).collect();
        let (first, last) = (times.iter().min()?, times.iter().max()?);
        let shell = &self.services.shell;
        let (a, b) = (shell.format_time(*first), shell.format_time(*last));
        Some(if a == b {
            a
        } else {
            t!("preview.range", from = a, to = b).into_owned()
        })
    }

    /// Multi-select summary: count, files and folders, total size, modified range, counts by kind.
    fn selection_summary(&self, pane: gpui::Div) -> AnyElement {
        let count = self.selection.len();
        let subtle = |text: String| {
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(text)
        };
        let pane = pane.child(
            div()
                .font_weight(theme::FW_STRONG)
                .text_size(theme::fs_15())
                .child(tn!("preview.selected", count).into_owned()),
        );
        // The list on screen, so items in expanded tree folders count too.
        let Some(view) = self.view() else {
            return pane.into_any_element();
        };
        if count > SUMMARY_MAX {
            return pane
                .child(subtle(t!("preview.too_many").into_owned()))
                .into_any_element();
        }
        let entries: Vec<&Entry> = view
            .iter()
            .filter(|e| self.selection.contains(&e.name))
            .collect();
        let dirs = entries.iter().filter(|e| e.is_dir_like()).count();
        let files = entries.len() - dirs;
        let mut rows: Vec<(String, String)> = Vec::new();
        if files > 0 {
            let size = format_size(self.selected_size().unwrap_or(0));
            rows.push((
                t!("preview.total_size").into_owned(),
                if dirs > 0 {
                    t!("preview.total_size_files", size = size).into_owned()
                } else {
                    size
                },
            ));
        }
        if let Some(range) = self.modified_range(&entries) {
            rows.push((t!("preview.modified_range").into_owned(), range));
        }
        let kinds = kind_counts(&entries);
        let other: usize = kinds.iter().skip(KINDS_SHOWN).map(|k| k.2).sum();
        // Don't write "0 files" when there are only files or only folders.
        let parts = match (files, dirs) {
            (_, 0) => tn!("preview.files_only", files),
            (0, _) => tn!("preview.dirs_only", dirs),
            _ => t!(
                "preview.selected_parts",
                files = tn!("preview.files_only", files),
                dirs = tn!("preview.dirs_only", dirs)
            ),
        };
        pane.child(subtle(parts.into_owned()))
            .child(Self::details_grid(rows))
            .child(
                div()
                    .mt(theme::s_8())
                    .child(subtle(t!("preview.by_type").into_owned())),
            )
            .children(kinds.iter().take(KINDS_SHOWN).map(|(label, kind, n)| {
                div()
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .text_size(theme::fs_12())
                    .child(kind_icon(*kind, theme::icon()))
                    .child(div().flex_1().min_w_0().truncate().child(label.clone()))
                    .child(
                        div()
                            .text_color(theme::text_subtle())
                            .child(arke_core::format::format_count(*n)),
                    )
            }))
            .when(other > 0, |d| {
                d.child(subtle(
                    t!(
                        "preview.other_types",
                        count = arke_core::format::format_count(other)
                    )
                    .into_owned(),
                ))
            })
            .into_any_element()
    }
}

/// Counts by kind, largest first (ties by name).
fn kind_counts(entries: &[&Entry]) -> Vec<(String, FileKind, usize)> {
    let mut kinds: Vec<(String, FileKind, usize)> = Vec::new();
    for entry in entries {
        let label = type_label(entry);
        match kinds.iter_mut().find(|k| k.0 == label) {
            Some(k) => k.2 += 1,
            None => kinds.push((label, file_kind(entry), 1)),
        }
    }
    kinds.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    kinds
}

#[cfg(test)]
mod pdf_tests {
    use super::*;

    #[test]
    fn page_turns_stop_at_the_ends() {
        assert_eq!(turned(0, 3, PageTurn::Next), Some(1));
        assert_eq!(turned(2, 3, PageTurn::Next), None);
        assert_eq!(turned(0, 3, PageTurn::Prev), None);
        assert_eq!(turned(2, 3, PageTurn::Prev), Some(1));
        assert_eq!(turned(1, 3, PageTurn::First), Some(0));
        assert_eq!(turned(1, 3, PageTurn::Last), Some(2));
        assert_eq!(turned(2, 3, PageTurn::Last), None);
        assert_eq!(turned(0, 1, PageTurn::Next), None);
        assert_eq!(turned(0, 0, PageTurn::Last), None);
    }

    #[test]
    fn sizes_within_ten_percent_are_close_enough() {
        assert!(close_enough((1000, 800), (1000, 800)));
        assert!(close_enough((950, 800), (1000, 820)));
        assert!(!close_enough((500, 400), (1000, 800)));
        assert!(!close_enough((1000, 800), (1000, 1000)));
    }

    #[test]
    fn cache_keeps_the_most_recent_pages() {
        let mut cache = PdfCache::default();
        let entry = Entry::new("a.pdf", arke_core::EntryKind::File, 10, None, 0);
        let image = crate::media::to_render_image(arke_core::thumbs::Thumbnail {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        })
        .unwrap();
        let page = PdfPage {
            image,
            width: 1,
            height: 1,
            pages: 40,
        };
        let key = |n| PdfKey::new(Path::new(r"C:.pdf"), &entry, n, (100, 100));
        for n in 0..20 {
            cache.put(key(n), page.clone());
        }
        assert!(cache.get(&key(0)).is_none(), "oldest pages are dropped");
        assert!(cache.get(&key(19)).is_some());
        // A different size or an edited file is a different page.
        let mut other = key(19);
        other.target = (200, 200);
        assert!(cache.get(&other).is_none());
        other.target = (98, 101);
        assert!(cache.get(&other).is_some(), "a nearly equal box is fine");
        other = key(19);
        other.size = 11;
        assert!(cache.get(&other).is_none());
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    #[test]
    fn fit_and_clamp() {
        let view = Bounds::new(
            point(crate::theme::scaled_px(10.), crate::theme::scaled_px(20.)),
            gpui::size(crate::theme::scaled_px(400.), crate::theme::scaled_px(300.)),
        );
        // Wide image: fills the width with space above and below.
        let fit = fit_rect(view, 800, 200);
        assert_eq!(
            fit.size,
            gpui::size(crate::theme::scaled_px(400.), crate::theme::scaled_px(100.))
        );
        assert_eq!(
            fit.origin,
            point(crate::theme::scaled_px(10.), crate::theme::scaled_px(120.))
        );
        // Larger than the view: can't drag past the edges. Smaller: centered.
        let content = gpui::size(
            crate::theme::scaled_px(1000.),
            crate::theme::scaled_px(200.),
        );
        let p = clamp_pan(
            point(crate::theme::scaled_px(50.), px(-80.)),
            content,
            view.size,
        );
        assert_eq!(p, point(px(0.), crate::theme::scaled_px(50.)));
        let p = clamp_pan(point(px(-900.), px(0.)), content, view.size);
        assert_eq!(p.x, px(-600.));
    }

    /// Fit mode shrinks large images but leaves small ones at actual size, centered.
    #[test]
    fn small_images_are_not_enlarged() {
        let view = Bounds::new(
            point(crate::theme::scaled_px(10.), crate::theme::scaled_px(20.)),
            gpui::size(crate::theme::scaled_px(400.), crate::theme::scaled_px(300.)),
        );
        // 64 × 48 at 100 %: drawn 64 × 48 in the middle, not stretched to 400 × 300.
        let shown = shown_rect(view, 64, 48, 1.);
        assert_eq!(
            shown.size,
            gpui::size(crate::theme::scaled_px(64.), crate::theme::scaled_px(48.))
        );
        assert_eq!(shown.origin, point(px(10. + 168.), px(20. + 126.)));
        // At 200 % scaling its actual size is half that in logical pixels.
        assert_eq!(
            shown_rect(view, 64, 48, 2.).size,
            gpui::size(crate::theme::scaled_px(32.), crate::theme::scaled_px(24.))
        );
        // Larger than the view: same as fit.
        assert_eq!(shown_rect(view, 800, 200, 1.), fit_rect(view, 800, 200));
    }

    /// Quick Look takes most of the window, keeps a margin on small windows and stops growing on
    /// big screens; the stage is what's left under the title bar and above the bottom bar.
    #[test]
    fn quick_look_card_follows_the_window() {
        let card = look_card(gpui::size(
            crate::theme::scaled_px(1280.),
            crate::theme::scaled_px(800.),
        ));
        assert!(
            card.width > crate::theme::scaled_px(1100.)
                && card.width <= crate::theme::scaled_px(1280.) - theme::ql_margin() * 2.
        );
        assert!(
            card.height > crate::theme::scaled_px(680.)
                && card.height <= crate::theme::scaled_px(800.) - theme::ql_margin() * 2.
        );
        let small = look_card(gpui::size(
            crate::theme::scaled_px(300.),
            crate::theme::scaled_px(300.),
        ));
        assert_eq!(
            small,
            gpui::size(crate::theme::scaled_px(252.), crate::theme::scaled_px(252.))
        );
        let huge = look_card(gpui::size(
            crate::theme::scaled_px(3840.),
            crate::theme::scaled_px(2160.),
        ));
        assert_eq!(huge, gpui::size(theme::ql_max_w(), theme::ql_max_h()));
        let stage = look_stage(gpui::size(
            crate::theme::scaled_px(3840.),
            crate::theme::scaled_px(2160.),
        ));
        assert_eq!(
            stage.height,
            theme::ql_max_h() - theme::h_bar() - theme::h_ctx() - px(2.)
        );
        // The stage takes the card's whole width.
        assert_eq!(stage.width, theme::ql_max_w());
        // A window too small for a card still gets a positive size.
        let tiny = look_stage(gpui::size(
            crate::theme::scaled_px(10.),
            crate::theme::scaled_px(10.),
        ));
        assert!(tiny.width > px(0.) && tiny.height > px(0.));
    }
}

/// Text and code: line numbers on the left; long lines wrap and the number stays aligned to the
/// first row. `large` (Quick Look) fills the stage like an editor: a ruled gutter, roomier lines.
/// Without `wrap` (tables) lines stay whole and the view scrolls sideways.
fn code_view(
    lines: &Arc<Vec<CodeLine>>,
    large: bool,
    wrap: bool,
    scroll: &gpui::ScrollHandle,
) -> AnyElement {
    code_view_at(lines, large.then_some(1.), wrap, scroll)
}

/// [`code_view`]; `zoom` is Quick Look's text scale (`None` in the pane).
fn code_view_at(
    lines: &Arc<Vec<CodeLine>>,
    zoom: Option<f32>,
    wrap: bool,
    scroll: &gpui::ScrollHandle,
) -> AnyElement {
    let large = zoom.is_some();
    let zoom = zoom.unwrap_or(1.);
    let digits = lines.len().max(1).to_string().len();
    // Line numbers take about 8 px a digit at normal size.
    let numbers = px((8. * digits as f32 + 8.) * zoom);
    let rows: Vec<AnyElement> = lines
        .iter()
        .enumerate()
        .map(|(ix, line)| {
            let marks = line.marks.iter().map(|(range, token)| {
                (
                    range.clone(),
                    gpui::HighlightStyle {
                        color: Some(theme::code_color(*token).into()),
                        ..gpui::HighlightStyle::default()
                    },
                )
            });
            let number = div()
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme::text_disabled())
                .child((ix + 1).to_string());
            let text = div()
                .when(wrap, |d| d.flex_1().min_w_0())
                .when(!wrap, |d| d.flex_none().whitespace_nowrap())
                .child(gpui::StyledText::new(line.text.clone()).with_highlights(marks));
            if large {
                div()
                    .flex()
                    .child(
                        number
                            .w(numbers + theme::s_16() + theme::s_12())
                            .pl(theme::s_16())
                            .pr(theme::s_12())
                            .border_r_1()
                            .border_color(theme::line()),
                    )
                    .child(
                        text.pl(theme::s_16())
                            .pr(theme::s_24())
                            .text_color(theme::text()),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .gap(theme::s_8())
                    .child(number.w(numbers))
                    .child(text.text_color(theme::text_muted()))
                    .into_any_element()
            }
        })
        .collect();
    // Both axes scroll for a table, and the plain wheel still goes up and down: GPUI only turns
    // the wheel sideways when x alone scrolls.
    let area = div()
        .id("preview-text")
        .debug_selector(|| "preview-text".into())
        .size_full();
    let area = if wrap {
        area.overflow_y_scroll()
    } else {
        area.overflow_scroll().flex().items_start()
    };
    area.track_scroll(scroll)
        .font_family("Cascadia Mono")
        .when(large, |d| {
            d.py(theme::s_12())
                .text_size(theme::fs_13() * zoom)
                .line_height(gpui::relative(1.6))
        })
        .when(!large, |d| {
            d.p(theme::s_8())
                .map(theme::preview_sheet)
                .text_size(theme::fs_12())
        })
        .child(
            // Without wrapping the lines set the width, which may pass the frame's.
            div()
                .flex()
                .flex_col()
                .when(!wrap, |d| d.flex_none().min_w_full())
                .children(rows),
        )
        .into_any_element()
}

/// "12 files, 3 folders · 4.2 MB unpacked"
fn archive_summary(listing: &ArchiveListing) -> String {
    let mut text = t!(
        "preview.archive_summary",
        files = tn!("preview.files_only", listing.files),
        dirs = tn!("preview.dirs_only", listing.dirs),
        size = format_size(listing.size)
    )
    .into_owned();
    if !listing.complete {
        text.push_str(&t!("preview.archive_partial"));
    }
    text
}

#[cfg(test)]
impl PreviewBody {
    /// Tests: text preview content (lines joined by newlines).
    pub(crate) fn text(&self) -> Option<String> {
        match self {
            Self::Text { lines, .. } => Some(
                lines
                    .iter()
                    .map(|l| l.text.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod code_tests {
    use super::*;

    #[test]
    fn marks_are_split_per_line() {
        let text = "let a = 1; /* x\ny */ let";
        let lang = arke_core::highlight::lang_for("rs").unwrap();
        let marks = arke_core::highlight::highlight(text, lang);
        let lines = split_lines(text, &marks);
        assert_eq!(lines.len(), 2);
        let first: Vec<_> = lines[0]
            .marks
            .iter()
            .map(|(r, t)| (&lines[0].text[r.clone()], *t))
            .collect();
        assert_eq!(
            first,
            [
                ("let", Token::Keyword),
                ("1", Token::Number),
                ("/* x", Token::Comment)
            ]
        );
        let second: Vec<_> = lines[1]
            .marks
            .iter()
            .map(|(r, t)| (&lines[1].text[r.clone()], *t))
            .collect();
        assert_eq!(second, [("y */", Token::Comment), ("let", Token::Keyword)]);
    }

    /// A long location wraps between folders, not inside a name.
    #[test]
    fn paths_may_wrap_after_each_separator() {
        assert_eq!(
            wrappable_path(r"C:\Users\me\專案"),
            "C:\\\u{200b}Users\\\u{200b}me\\\u{200b}專案"
        );
        assert_eq!(wrappable_path("plain"), "plain");
        assert_eq!(wrappable_path(r"D:\My Files"), "D:\\\u{200b}My\u{a0}Files");
    }

    /// Markdown saved with lone CRs previewed empty (one line with a CR per paragraph), and a
    /// paragraph with form feeds vanished; both now split like text does.
    #[test]
    fn markdown_lines_end_at_every_hard_break() {
        assert_eq!(
            markdown_lines("# Title\r\rtext\r- item\r\nlast"),
            "# Title\n\ntext\n- item\nlast"
        );
        assert_eq!(markdown_lines("a\u{c}b\u{c}c"), "a\nb\nc");
        let long = "line\r".repeat(TEXT_LINES + 10);
        assert_eq!(markdown_lines(&long).lines().count(), TEXT_LINES);
    }

    /// A CSV saved with lone CRs (old Mac Excel), or with CRs, VTs, FFs or NELs inside its fields,
    /// used to preview empty: the line reached DirectWrite with more breaks than GPUI reads.
    #[test]
    fn every_hard_break_ends_a_line() {
        let text = "a,1\rb,2\rc,3\rd,4\re,5\r\nf\u{b}g\u{c}h\u{85}i\u{2028}j\u{2029}k\n";
        let lines: Vec<_> = split_lines(text, &[])
            .into_iter()
            .map(|l| l.text.to_string())
            .collect();
        assert_eq!(
            lines,
            [
                "a,1", "b,2", "c,3", "d,4", "e,5", "f", "g", "h", "i", "j", "k", ""
            ]
        );
        let lang = arke_core::highlight::lang_for("rs").unwrap();
        let text = "let\r/* x */\rlet";
        let lines = split_lines(text, &arke_core::highlight::highlight(text, lang));
        assert_eq!(lines[1].marks, [(0..7, Token::Comment)]);
        assert_eq!(lines[2].marks, [(0..3, Token::Keyword)]);
    }

    #[test]
    fn crlf_lines_keep_marks_inside_the_text() {
        let text = "// a
let
";
        let lang = arke_core::highlight::lang_for("rs").unwrap();
        let lines = split_lines(text, &arke_core::highlight::highlight(text, lang));
        assert_eq!(lines[0].text.as_ref(), "// a");
        assert_eq!(lines[0].marks, [(0..4, Token::Comment)]);
        assert_eq!(lines[1].text.as_ref(), "let");
        assert_eq!(lines[1].marks, [(0..3, Token::Keyword)]);
    }
}

/// Loading: draw nothing for the first moment (most loads finish by then), then a few pulsing gray
/// bars. Quieter than a "Loading…" line. No pulse when system animations are off.
fn loading_skeleton(since: std::time::Instant) -> AnyElement {
    const WIDTHS: [f32; 7] = [0.82, 0.64, 0.9, 0.55, 0.74, 0.6, 0.86];
    if since.elapsed() < LOADING_GRACE {
        return div().size_full().into_any_element();
    }
    let bars = div()
        .id("preview-skeleton")
        .size_full()
        .flex()
        .flex_col()
        .gap(theme::s_12())
        .p(theme::s_8())
        .children(WIDTHS.iter().map(|w| {
            div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .child(
                    div()
                        .size(crate::theme::scaled_px(14.))
                        .flex_none()
                        .rounded(theme::r_4())
                        .bg(theme::wash_strong()),
                )
                .child(
                    div()
                        .h(crate::theme::scaled_px(10.))
                        .w(gpui::relative(*w))
                        .rounded(theme::r_4())
                        .bg(theme::wash_strong()),
                )
        }));
    pulsing(bars, "preview-skeleton-pulse")
}

/// Loading a picture or a video frame: draw nothing for the first moment, then a blank pulsing
/// frame that fills the media area, so the picture replaces it in place. A faint icon of the
/// kind sits in the middle. No text.
fn media_placeholder(since: std::time::Instant, entry: &Entry) -> AnyElement {
    if since.elapsed() < LOADING_GRACE {
        return div().size_full().into_any_element();
    }
    let kind = match file_kind(entry) {
        FileKind::Video => FileKind::Video,
        FileKind::Image => FileKind::Image,
        _ => FileKind::Pdf,
    };
    let frame = div()
        .id("media-placeholder")
        .debug_selector(|| "media-placeholder".into())
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::line())
        .bg(theme::wash())
        .child(
            div()
                .id("media-placeholder-icon")
                .debug_selector(|| "media-placeholder-icon".into())
                .opacity(0.45)
                .child(kind_icon(kind, crate::theme::scaled_px(40.))),
        );
    pulsing(frame, "media-placeholder-pulse")
}

/// `el` pulsing gently while something loads (still with animations off).
pub(super) fn pulsing<E>(el: E, id: impl Into<gpui::ElementId>) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    if !crate::motion::enabled() {
        return el.into_any_element();
    }
    gpui::AnimationExt::with_animation(
        el,
        id,
        gpui::Animation::new(SKELETON_PULSE).repeat(),
        |d, t| {
            // Smooth 0 -> 1 -> 0 wave
            let pulse = (1. - (t * std::f32::consts::TAU).cos()) / 2.;
            d.opacity(0.45 + 0.55 * pulse)
        },
    )
    .into_any_element()
}
