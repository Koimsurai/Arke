//! Preview: the side preview pane and Quick Look on Space (like macOS).
//!
//! Both show the item under the cursor.
//! - Images: decoded in the background to 1600 px on the long side (own decoder, no third-party
//!   thumbnail handlers).
//! - Text and code: first 64 KB with encoding detection (UTF-8, UTF-16, legacy system code pages
//!   like Big5); binary-looking files are skipped. Code gets line numbers and basic highlighting
//!   (`arke_core::highlight`); Markdown gets basic layout (headings, lists, quotes, code blocks).
//! - PDFs: rendered by Windows (`win_pdf.rs`), the first page in the pane and every page in Quick
//!   Look (Left/Right, Page Up/Down, Home/End turn pages), sized for the space in device pixels.
//!   Recent pages are cached per tab.
//! - Archives: list the contents without extracting (zip reads only the central directory, tar
//!   stops at a limit).
//! - Folders: list the contents (in column view the third column is the next level; clicking enters
//!   and selects).
//! - Office Open XML (`explorer_office.rs`): Word text with basic structure, Excel sheets as a grid
//!   (Quick Look switches sheets), PowerPoint's embedded thumbnail in the pane and slide text cards
//!   in Quick Look (page keys turn slides). Parsed by Arke itself; no Office or preview handlers.
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
    AnyElement, Bounds, ClickEvent, Context, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, Pixels, Point, RenderImage, ScrollEvent, SharedString, Window,
    anchored, canvas, deferred, div, img, point, prelude::*, px,
};
use rust_i18n::t;

use super::{AppEvent, Explorer, kind_icon, spawn_thread};
use office::{SlidesView, WorkbookView, load_office};

#[path = "explorer_office.rs"]
mod office;
use crate::a11y::A11yExt as _;
use crate::shell::Shell;
use crate::theme::{self, UiFont as _};

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
    let mut start = 0;
    let mut next = 0;
    for line in text.split('\n').take(TEXT_LINES) {
        // CRLF: hide the \r and keep ranges before it so they never extend past the line's text.
        let visible = line.trim_end_matches('\r');
        let end = start + visible.len();
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
            text: SharedString::from(visible.to_owned()),
            marks: line_marks,
        });
        start += line.len() + 1;
    }
    lines
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
    Markdown(SharedString),
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
    Markdown(String),
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
        Ok(listing) => PreviewData::Archive(listing),
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
                let text: String = text.lines().take(TEXT_LINES).collect::<Vec<_>>().join("\n");
                return PreviewData::Markdown(text);
            }
            let marks = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(arke_core::highlight::lang_for)
                .map(|lang| arke_core::highlight::highlight(&text, lang))
                .unwrap_or_default();
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
            Some(entry) if self.quick_look || self.selection.contains(&entry.name) => entry,
            _ if self.quick_look || self.selection.len() != 1 => return None,
            _ => {
                let name = self.selection.names().next()?;
                view.iter().find(|e| e.name == *name)?
            }
        };
        Some((model.path.join(&*entry.name), entry.clone()))
    }

    /// Called on every render: when the preview is open and the cursor moved to another item,
    /// schedule a read.
    // One pass over what can happen on a frame (same PDF resized, cache hit, cloud, load); split
    // up, the order of the checks would be harder to follow.
    #[allow(clippy::too_many_lines)]
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
        let fs = self.services.fs.clone();
        let shell = self.services.shell.clone();
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
            }) => cx.drop_image(view.render.image, None),
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
                    })
                }
                PreviewData::Markdown(text) => PreviewBody::Markdown(text.into()),
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
            self.look_page_area()
        } else {
            let w = self.pane_frame_w();
            gpui::size(w, self.pdf_frame_h(w))
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let device = |v: Pixels| (f32::from(v) * scale).round().max(1.) as u32;
        (device(area.width), device(area.height))
    }

    /// Width of the pane's preview frame (the pane minus its padding).
    fn pane_frame_w(&self) -> Pixels {
        (self.preview_w() - theme::S_16 * 2.).max(px(1.))
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
        let shell = self.services.shell.clone();
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
                    .rounded(theme::R_4)
                    .overflow_hidden()
                    .when(raised, |d| d.shadow(theme::elev_float()))
                    .when(!raised, |d| d.border_1().border_color(theme::line()))
                    .child(img(render.image.clone()).size_full()),
            )
            .into_any_element()
    }

    // ───────────────────────── Rendering ─────────────────────────

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
        if let Some(t) = preview.entry.modified {
            rows.push((t!("col.modified").to_string(), shell.format_time(t)));
        }
        // After the date, so Quick Look's title row (the first three details) reads type, size,
        // date for PDFs too.
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
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
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
                | PreviewBody::Workbook(_)
        )
    }

    /// Stack scrollable content with its scrollbar on the right.
    fn with_preview_scrollbar(
        body: AnyElement,
        scroll: &gpui::ScrollHandle,
        drag: &crate::scrollbar::ScrollbarDrag,
    ) -> AnyElement {
        div()
            .relative()
            .size_full()
            .child(body)
            .child(crate::scrollbar::scrollbar(scroll.clone(), drag.clone()))
            .into_any_element()
    }

    fn preview_body(preview: &Preview, scroll: &gpui::ScrollHandle) -> AnyElement {
        match &preview.body {
            // Give the element the frame's aspect ratio; otherwise GPUI sizes by the image's own
            // ratio and portrait photos overflow.
            PreviewBody::Image { image, .. } => img(image.clone())
                .size_full()
                .aspect_ratio(PANE_IMAGE_RATIO)
                .object_fit(ObjectFit::Contain)
                .rounded(theme::R_8)
                .into_any_element(),
            PreviewBody::Pdf(view) => img(view.render.image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            PreviewBody::Text { lines, .. } => code_view(lines, false, scroll),
            PreviewBody::Archive(listing) => Self::archive_list(listing, scroll),
            PreviewBody::Markdown(text) => div()
                .id("preview-markdown")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .p(theme::S_12)
                .rounded(theme::R_8)
                .bg(theme::wash())
                .child(markdown(text, false))
                .into_any_element(),
            PreviewBody::Folder { entries, total } => {
                Self::folder_list(&preview.path, entries, *total, scroll, None)
            }
            PreviewBody::Document(doc) => office::document_body(doc, false, scroll),
            PreviewBody::Workbook(view) => office::sheet_grid(view, false, scroll),
            PreviewBody::Slides(view) => office::slides_pane(view),
            PreviewBody::Loading(since) => loading_skeleton(*since),
            PreviewBody::Cloud => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(theme::S_8)
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(
                    gpui::svg()
                        .path(crate::assets::Icon::Cloud.path())
                        .size(px(32.))
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
                .gap(theme::S_8)
                .px(theme::S_12)
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(kind_icon(file_kind(&preview.entry), px(32.)))
                .child(div().text_center().child(reason.clone()))
                .into_any_element(),
            PreviewBody::Nothing => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(kind_icon(file_kind(&preview.entry), px(64.)))
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
        mut cx: Option<&mut Context<Self>>,
    ) -> AnyElement {
        if entries.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme::FS_12)
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
                    .h(theme::S_24 + theme::S_4)
                    .px(theme::S_8)
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_8)
                    .rounded(theme::R_8)
                    .when(entry.is_hidden(), |d| d.text_color(theme::text_subtle()))
                    .child(kind_icon(file_kind(entry), theme::ICON))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(entry.name.clone())),
                    );
                match cx.as_deref_mut() {
                    Some(cx) => {
                        let dir = dir.to_path_buf();
                        let name = entry.name.clone();
                        row.cursor_pointer()
                            .hover(|s| s.bg(theme::wash()))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.navigate_selecting(
                                    dir.clone(),
                                    arke_core::NavKind::New,
                                    Some(name.clone()),
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
            .children(rows)
            .when(more > 0, |d| {
                d.child(
                    div()
                        .px(theme::S_8)
                        .py(theme::S_4)
                        .text_size(theme::FS_12)
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
                    .h(theme::S_24 + theme::S_4)
                    .px(theme::S_8)
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_8)
                    .child(kind_icon(kind, theme::ICON))
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
                                .text_size(theme::FS_12)
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
            .children(rows)
            .when(more > 0 || !listing.complete, |d| {
                d.child(
                    div()
                        .px(theme::S_8)
                        .py(theme::S_4)
                        .text_size(theme::FS_12)
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
            .gap(theme::S_6)
            .children(rows.into_iter().map(|(k, v)| {
                div()
                    .flex()
                    .gap(theme::S_12)
                    .text_size(theme::FS_12)
                    .child(
                        div()
                            .w(px(64.))
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
    /// to go back to the automatic width. Same feel as the Details column dividers.
    /// `in_gap`: a split tab's shared preview, whose edge is the gap left of its paper.
    pub(super) fn preview_resize_handle(
        &self,
        in_gap: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dragging = self.preview_resized && cx.has_active_drag();
        let edge = DraggedPreviewEdge(cx.entity_id());
        let gap = super::layout::SHARED_GAP;
        div()
            .id("preview-resize")
            .absolute()
            .top_0()
            .bottom_0()
            // Starts on the border and covers the pane's padding: the list's scrollbar stops at
            // the border, so both stay easy to grab.
            .when(!in_gap, |d| d.left_0().w(theme::S_8))
            // The gap lies over the right pane's box: keep the press from reaching that pane,
            // which would make it the active side and hand the preview (and this drag) over.
            .when(in_gap, |d| d.left(-gap).w(gap).occlude())
            .cursor_col_resize()
            .group("preview-resize")
            .child(
                div()
                    .absolute()
                    .top(if in_gap { theme::R_12 } else { px(0.) })
                    .bottom(if in_gap { theme::R_12 } else { px(0.) })
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
                cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.preview_drag_start = Some((e.position.x, this.preview_w()));
                }),
            )
            .on_drag(edge, |_, _, _, cx| cx.new(|_| super::details::ColumnDragView))
            .on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                if e.click_count() >= 2 {
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
        let Some((from, start)) = self.preview_drag_start.filter(|_| pane == cx.entity_id()) else {
            return;
        };
        let room = match self.pane.preview_host {
            Some(shared) if self.hosts_shared_preview() => shared.room,
            _ => {
                super::layout::preview_room(&self.fit_input(self.paper_width, self.screen.0.width))
            }
        };
        let width = (start + from - e.event.position.x).clamp(super::layout::PREVIEW_MIN, room);
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
            .gap(theme::S_12)
            .p(theme::S_16)
    }

    /// What the side preview shows in `pane` (from [`Self::preview_frame`]): the item, a
    /// summary of a multiple selection, or a note that nothing is selected.
    // Layouts for each kind (no selection, archive, folder, other) are kept together for easy
    // comparison.
    #[allow(clippy::too_many_lines)]
    pub(super) fn preview_column(&self, pane: gpui::Div, cx: &mut Context<Self>) -> AnyElement {
        // Several selected: previewing the cursor item means little, show a summary instead.
        if self.selection.len() > 1 && !self.search_mode {
            return self.selection_summary(pane);
        }
        let Some(preview) = &self.preview else {
            return pane
                .items_center()
                .justify_center()
                .text_size(theme::FS_12)
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
                        .text_size(theme::FS_15)
                        .child(SharedString::from(
                            arke_core::tree::leaf(&preview.entry.name).to_owned(),
                        )),
                )
                .child(
                    div()
                        .text_size(theme::FS_12)
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
                        .items_center()
                        .gap(theme::S_8)
                        .child(kind_icon(FileKind::Folder, px(20.)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(theme::FW_STRONG)
                                .text_size(theme::FS_15)
                                .child(SharedString::from(
                                    arke_core::tree::leaf(&preview.entry.name).to_owned(),
                                )),
                        ),
                )
                .child(
                    div()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_subtle())
                        .child(tn!("info.items", *total).into_owned()),
                )
                .child({
                    let (scroll, drag) =
                        (self.preview_scroll.clone(), self.preview_scrollbar.clone());
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
        pane.child(div().h(frame_h).flex_none().child({
            if let PreviewBody::Pdf(view) = &preview.body {
                Self::pdf_page_view(&view.render, gpui::size(frame_w, frame_h), false)
            } else {
                let body = Self::preview_body(preview, &self.preview_scroll);
                if Self::scrolls(&preview.body) {
                    Self::with_preview_scrollbar(
                        body,
                        &self.preview_scroll,
                        &self.preview_scrollbar,
                    )
                } else {
                    body
                }
            }
        }))
        .child(
            div()
                .font_weight(theme::FW_STRONG)
                .text_size(theme::FS_15)
                .child(SharedString::from(
                    arke_core::tree::leaf(&preview.entry.name).to_owned(),
                )),
        )
        .child(Self::details_grid(self.preview_details(preview)))
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
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .overflow_hidden()
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(self.quick_look_header(preview, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .border_y_1()
                    .border_color(theme::line())
                    .bg(look_backdrop(preview))
                    .child(self.quick_look_body(preview, window, cx)),
            )
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
            // The wheel belongs to Quick Look (images zoom with it); the list must not scroll
            // underneath.
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
                        .min_w(px(96.))
                        .px(theme::S_4)
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
                    .h(theme::H_CTL)
                    .px(theme::S_12)
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

    /// Quick Look title bar: kind icon, name over a details line, then Open, Open with, Show in
    /// folder (search results) and Close.
    fn quick_look_header(&self, preview: &Preview, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dir = preview.entry.is_dir_like();
        div()
            .h(theme::H_BAR)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_12)
            .pl(theme::S_16)
            .pr(theme::S_8)
            .child(kind_icon(file_kind(&preview.entry), theme::ICON_LG))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().font_weight(theme::FW_STRONG).child(
                        SharedString::from(arke_core::tree::leaf(&preview.entry.name).to_owned()),
                    ))
                    .child(
                        div()
                            .truncate()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle_raised())
                            .child(self.look_meta(preview)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_4)
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
                            .h(theme::S_16)
                            .mx(theme::S_4)
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
                    PreviewBody::Image { width, height, .. } => {
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
                .gap(theme::S_4)
                .children(keys.iter().copied().map(key_cap))
                .child(div().ml(theme::S_2).child(label))
        };
        let up = -std::f32::consts::FRAC_PI_2;
        div()
            .h(theme::H_CTX)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_16)
            .pl(theme::S_16)
            .pr(theme::S_8)
            .text_size(theme::FS_12)
            .text_color(theme::text_subtle_raised())
            .when(width >= LOOK_HINTS_MIN_W, |d| {
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
                    .gap(theme::S_2)
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
                            .min_w(px(56.))
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
/// than [`theme::QL_MAX_W`] × [`theme::QL_MAX_H`] on big screens.
pub(crate) fn look_card(viewport: gpui::Size<Pixels>) -> gpui::Size<Pixels> {
    let side = |total: Pixels, max: Pixels| {
        let margin = (total * (1. - LOOK_SHARE) / 2.).max(theme::QL_MARGIN);
        (total - margin * 2.).min(max).max(px(1.))
    };
    gpui::size(
        side(viewport.width, theme::QL_MAX_W),
        side(viewport.height, theme::QL_MAX_H),
    )
}

/// Quick Look's stage (between the title and bottom bars and their hairlines) for a window.
fn look_stage(viewport: gpui::Size<Pixels>) -> gpui::Size<Pixels> {
    let card = look_card(viewport);
    gpui::size(
        card.width,
        (card.height - theme::H_BAR - theme::H_CTX - px(2.)).max(px(1.)),
    )
}

/// What sits behind Quick Look's content: the darker stage for images and PDFs (so they read
/// edge to edge), paper for text, the card itself for lists and empty states.
fn look_backdrop(preview: &Preview) -> gpui::Rgba {
    match &preview.body {
        PreviewBody::Image { .. } | PreviewBody::Pdf(_) | PreviewBody::Slides(_) => theme::stage(),
        PreviewBody::Loading(_) if on_stage(&preview.entry) => theme::stage(),
        PreviewBody::Text { .. } | PreviewBody::Markdown(_) | PreviewBody::Document(_) => {
            theme::surface()
        }
        _ => theme::overlay(),
    }
}

/// Kinds Quick Look shows on the darker stage (images, PDF pages, slides).
fn on_stage(entry: &Entry) -> bool {
    matches!(file_kind(entry), FileKind::Image | FileKind::Pdf)
        || arke_core::office::office_kind(&entry.name)
            == Some(arke_core::office::OfficeKind::Presentation)
}

/// A pill floating at the bottom of Quick Look's stage (page control, zoom). It occludes, so
/// clicks on it don't reach the image underneath.
/// `end` puts it in the bottom-right corner instead of the middle.
fn floating(pill: gpui::Stateful<gpui::Div>, end: bool) -> AnyElement {
    div()
        .absolute()
        .bottom(theme::S_12)
        .left_0()
        .right_0()
        .px(theme::S_12)
        .flex()
        .when(end, gpui::Styled::justify_end)
        .when(!end, gpui::Styled::justify_center)
        .child(
            pill.occlude()
                .h(theme::H_CTL + theme::S_4)
                .p(theme::S_2)
                .flex()
                .items_center()
                .gap(theme::S_2)
                .rounded(theme::R_FULL)
                .bg(theme::overlay())
                .shadow(theme::elev_float())
                .text_size(theme::FS_12)
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
            theme::H_CTL
        } else {
            theme::H_CTL_SM
        })
        .px(if primary { theme::S_16 } else { theme::S_12 })
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .text_size(if primary { theme::FS_14 } else { theme::FS_12 })
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
        .gap(theme::S_8)
        .px(theme::S_24)
        .child(div().mb(theme::S_8).child(icon))
        .child(
            div()
                .text_size(theme::FS_15)
                .font_weight(theme::FW_STRONG)
                .child(t!("preview.no_preview").into_owned()),
        )
        .children(detail.map(|detail| {
            div()
                .max_w(theme::TOOLTIP_MAX_W)
                .text_center()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle_raised())
                .child(detail)
        }))
        .child(div().mt(theme::S_12).child(open))
        .into_any_element()
}

/// A key in Quick Look's hints, like the toolbar's shortcut badges but legible on the card.
fn key_cap(key: &'static str) -> impl IntoElement {
    div()
        .min_w(theme::S_16)
        .px(theme::S_4)
        .flex()
        .justify_center()
        .rounded(theme::R_4)
        .border_1()
        .border_color(theme::line_strong())
        .text_size(theme::FS_11)
        .child(key)
}

/// Smallest card width that still shows the key hints next to the position.
const LOOK_HINTS_MIN_W: Pixels = px(640.);
/// Share of the window Quick Look takes (before the margin and size caps).
const LOOK_SHARE: f32 = 0.88;
/// Room around a PDF page on the stage; below it the floating page control sits clear of the page.
const PDF_PAD: Pixels = theme::S_24;
const PDF_PAD_BOTTOM: Pixels = px(56.);

/// Quick Look zoom: fit to window by default; the wheel zooms around the pointer, a click toggles
/// actual size (one image pixel per screen pixel), and a drag pans.
#[derive(Default)]
pub(crate) struct Zoom {
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

/// Preview pane frame ratio: pane width minus 32 padding, to height
const PANE_IMAGE_RATIO: f32 = 268. / 200.;

/// The preview pane's left edge being dragged, in this pane (in split view both panes see the
/// drag; only the one it started in resizes).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DraggedPreviewEdge(pub(super) gpui::EntityId);
/// Pane frame for PDFs: height to width, a little squatter than A4 so details still fit
const PDF_FRAME_RATIO: f32 = 1.3;
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
    #[allow(clippy::cast_precision_loss)]
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
#[allow(clippy::cast_precision_loss)]
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
            let fs = self.services.fs.clone();
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
            (stage.width - PDF_PAD * 2.).max(px(1.)),
            (stage.height - PDF_PAD - PDF_PAD_BOTTOM).max(px(1.)),
        )
    }

    /// Quick Look body: images fill the stage and zoom with the wheel, toggle actual size on click
    /// and pan by drag; PDF pages sit on the stage with a page control; text reads like an editor;
    /// items without a preview get a large icon and an Open button.
    // The image's mouse handling is one chain; the other kinds return early above it.
    #[allow(clippy::too_many_lines)]
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
        let (width, height, image) = match &preview.body {
            PreviewBody::Image {
                image,
                width,
                height,
                ..
            } => (*width, *height, image),
            PreviewBody::Pdf(view) => {
                return div()
                    .size_full()
                    .pt(PDF_PAD)
                    .px(PDF_PAD)
                    .pb(PDF_PAD_BOTTOM)
                    .child(Self::pdf_page_view(
                        &view.render,
                        self.look_page_area(),
                        true,
                    ))
                    .when(view.render.pages > 1, |d| {
                        d.child(Self::page_controls(view, cx))
                    })
                    .into_any_element();
            }
            PreviewBody::Text { lines, .. } => {
                return Self::with_preview_scrollbar(
                    code_view(lines, true, &self.look_scroll),
                    &self.look_scroll,
                    &self.look_scrollbar,
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
                    .child(
                        div()
                            .w_full()
                            .max_w(theme::READ_W)
                            .px(theme::S_32)
                            .py(theme::S_24)
                            .child(markdown(text, true)),
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
                    kind_icon(kind, theme::ICON_HERO),
                    Some(detail.into()),
                    open(),
                );
            }
            PreviewBody::Cloud => {
                return look_empty(
                    gpui::svg()
                        .path(crate::assets::Icon::Cloud.path())
                        .size(theme::ICON_HERO / 2.)
                        .text_color(theme::text_subtle())
                        .into_any_element(),
                    Some(t!("preview.cloud").into_owned().into()),
                    open(),
                );
            }
            PreviewBody::Unreadable(reason) => {
                return look_empty(
                    kind_icon(kind, theme::ICON_HERO),
                    Some(reason.clone()),
                    open(),
                );
            }
            PreviewBody::Document(doc) => {
                return Self::with_preview_scrollbar(
                    office::document_body(doc, true, &self.look_scroll),
                    &self.look_scroll,
                    &self.look_scrollbar,
                );
            }
            PreviewBody::Workbook(view) => {
                return div()
                    .size_full()
                    .p(theme::S_8)
                    .child(self.workbook_look(view, cx))
                    .into_any_element();
            }
            PreviewBody::Slides(view) => return self.slides_look(view, cx),
            // Images, PDFs and slides wait on the bare stage: bars would flash where a picture
            // lands.
            PreviewBody::Loading(_) if on_stage(&preview.entry) => {
                return div().size_full().into_any_element();
            }
            PreviewBody::Loading(_) | PreviewBody::Folder { .. } | PreviewBody::Archive(_) => {
                let body = Self::preview_body(preview, &self.look_scroll);
                let body = if Self::scrolls(&preview.body) {
                    Self::with_preview_scrollbar(body, &self.look_scroll, &self.look_scrollbar)
                } else {
                    body
                };
                return div()
                    .size_full()
                    .p(theme::S_8)
                    .child(body)
                    .into_any_element();
            }
        };
        let view = self.zoom.view.clone();
        let capture = canvas(move |bounds, _, _| view.set(bounds), |_, (), _, _| {})
            .absolute()
            .size_full();
        let picture = if self.zoom.on {
            let content = self.zoomed_size(width, height, window.scale_factor());
            let source = self.zoom.full.clone().unwrap_or_else(|| image.clone());
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
            img(image.clone())
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
                    if moved.x.abs() < px(4.) && moved.y.abs() < px(4.) {
                        this.toggle_zoom(e.position, window.scale_factor(), cx);
                    }
                    cx.stop_propagation();
                }),
            )
            // Wheel zooms around the pointer (like image viewers). Windows reports a wheel notch as
            // three lines by default, so three lines (or the same distance in touchpad pixels)
            // make one zoom step.
            .on_scroll(cx.listener(|this, e: &ScrollEvent, window, cx| {
                let line = px(20.);
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
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(text)
        };
        let pane = pane.child(
            div()
                .font_weight(theme::FW_STRONG)
                .text_size(theme::FS_15)
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
                    .mt(theme::S_8)
                    .child(subtle(t!("preview.by_type").into_owned())),
            )
            .children(kinds.iter().take(KINDS_SHOWN).map(|(label, kind, n)| {
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .text_size(theme::FS_12)
                    .child(kind_icon(*kind, theme::ICON))
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
        let view = Bounds::new(point(px(10.), px(20.)), gpui::size(px(400.), px(300.)));
        // Wide image: fills the width with space above and below.
        let fit = fit_rect(view, 800, 200);
        assert_eq!(fit.size, gpui::size(px(400.), px(100.)));
        assert_eq!(fit.origin, point(px(10.), px(120.)));
        // Larger than the view: can't drag past the edges. Smaller: centered.
        let content = gpui::size(px(1000.), px(200.));
        let p = clamp_pan(point(px(50.), px(-80.)), content, view.size);
        assert_eq!(p, point(px(0.), px(50.)));
        let p = clamp_pan(point(px(-900.), px(0.)), content, view.size);
        assert_eq!(p.x, px(-600.));
    }

    /// Fit mode shrinks large images but leaves small ones at actual size, centered.
    #[test]
    fn small_images_are_not_enlarged() {
        let view = Bounds::new(point(px(10.), px(20.)), gpui::size(px(400.), px(300.)));
        // 64 × 48 at 100 %: drawn 64 × 48 in the middle, not stretched to 400 × 300.
        let shown = shown_rect(view, 64, 48, 1.);
        assert_eq!(shown.size, gpui::size(px(64.), px(48.)));
        assert_eq!(shown.origin, point(px(10. + 168.), px(20. + 126.)));
        // At 200 % scaling its actual size is half that in logical pixels.
        assert_eq!(
            shown_rect(view, 64, 48, 2.).size,
            gpui::size(px(32.), px(24.))
        );
        // Larger than the view: same as fit.
        assert_eq!(shown_rect(view, 800, 200, 1.), fit_rect(view, 800, 200));
    }

    /// Quick Look takes most of the window, keeps a margin on small windows and stops growing on
    /// big screens; the stage is what's left under the title bar and above the bottom bar.
    #[test]
    fn quick_look_card_follows_the_window() {
        let card = look_card(gpui::size(px(1280.), px(800.)));
        assert!(card.width > px(1100.) && card.width <= px(1280.) - theme::QL_MARGIN * 2.);
        assert!(card.height > px(680.) && card.height <= px(800.) - theme::QL_MARGIN * 2.);
        let small = look_card(gpui::size(px(300.), px(300.)));
        assert_eq!(small, gpui::size(px(252.), px(252.)));
        let huge = look_card(gpui::size(px(3840.), px(2160.)));
        assert_eq!(huge, gpui::size(theme::QL_MAX_W, theme::QL_MAX_H));
        let stage = look_stage(gpui::size(px(3840.), px(2160.)));
        assert_eq!(
            stage.height,
            theme::QL_MAX_H - theme::H_BAR - theme::H_CTX - px(2.)
        );
        // A window too small for a card still gets a positive size.
        let tiny = look_stage(gpui::size(px(10.), px(10.)));
        assert!(tiny.width > px(0.) && tiny.height > px(0.));
    }
}

/// Text and code: line numbers on the left; long lines wrap and the number stays aligned to the
/// first row. `large` (Quick Look) fills the stage like an editor: a ruled gutter, roomier lines.
fn code_view(lines: &Arc<Vec<CodeLine>>, large: bool, scroll: &gpui::ScrollHandle) -> AnyElement {
    let digits = lines.len().max(1).to_string().len();
    #[allow(clippy::cast_precision_loss)]
    let numbers = px(8. * digits as f32 + 8.);
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
                .flex_1()
                .min_w_0()
                .child(gpui::StyledText::new(line.text.clone()).with_highlights(marks));
            if large {
                div()
                    .flex()
                    .child(
                        number
                            .w(numbers + theme::S_16 + theme::S_12)
                            .pl(theme::S_16)
                            .pr(theme::S_12)
                            .border_r_1()
                            .border_color(theme::line()),
                    )
                    .child(
                        text.pl(theme::S_16)
                            .pr(theme::S_24)
                            .text_color(theme::text()),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .gap(theme::S_8)
                    .child(number.w(numbers))
                    .child(text.text_color(theme::text_muted()))
                    .into_any_element()
            }
        })
        .collect();
    div()
        .id("preview-text")
        .size_full()
        .overflow_y_scroll()
        .track_scroll(scroll)
        .font_family("Cascadia Mono")
        .when(large, |d| {
            d.py(theme::S_12)
                .text_size(theme::FS_13)
                .line_height(gpui::relative(1.6))
        })
        .when(!large, |d| {
            d.p(theme::S_8)
                .rounded(theme::R_8)
                .bg(theme::wash())
                .text_size(theme::FS_12)
        })
        .children(rows)
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

/// Basic Markdown layout: headings, lists, quotes, rules, code blocks; inline `**` and `` ` ` `
/// markers are stripped. Not full Markdown, just readable in a preview.
fn markdown(text: &str, large: bool) -> AnyElement {
    let body = if large { theme::FS_15 } else { theme::FS_13 };
    let mut blocks: Vec<AnyElement> = Vec::new();
    let mut code: Option<Vec<String>> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            match code.take() {
                Some(lines) => blocks.push(code_block(&lines.join("\n"))),
                None => code = Some(Vec::new()),
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(line.to_owned());
            continue;
        }
        blocks.push(md_line(trimmed, body));
    }
    // Draw an unterminated code block anyway.
    if let Some(lines) = code {
        blocks.push(code_block(&lines.join("\n")));
    }
    div()
        .flex()
        .flex_col()
        .gap(theme::S_4)
        .ui_font()
        .text_size(body)
        .text_color(theme::text())
        .children(blocks)
        .into_any_element()
}

/// One Markdown line (outside code blocks).
fn md_line(line: &str, body: gpui::Pixels) -> AnyElement {
    if line.is_empty() {
        return div().h(theme::S_4).into_any_element();
    }
    let heading = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&heading) && line[heading..].starts_with(' ') {
        let size = match heading {
            1 => theme::FS_20,
            2 => theme::FS_15,
            _ => body,
        };
        return div()
            .mt(theme::S_4)
            .text_size(size)
            .font_weight(theme::FW_STRONG)
            .child(inline(line[heading..].trim()))
            .into_any_element();
    }
    if matches!(line, "---" | "***" | "___") {
        return div()
            .my(theme::S_4)
            .h(px(1.))
            .bg(theme::line())
            .into_any_element();
    }
    if let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix('>')) {
        return div()
            .pl(theme::S_8)
            .border_l_2()
            .border_color(theme::line_strong())
            .text_color(theme::text_muted())
            .child(inline(rest.trim()))
            .into_any_element();
    }
    let bullet = ["- ", "* ", "+ "].iter().find_map(|b| line.strip_prefix(b));
    let numbered = line
        .split_once(". ")
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    if bullet.is_some() || numbered.is_some() {
        let (mark, rest) = match (bullet, numbered) {
            (Some(rest), _) => ("•".to_owned(), rest),
            (None, Some((n, rest))) => (format!("{n}."), rest),
            (None, None) => (String::new(), line),
        };
        return div()
            .flex()
            .gap(theme::S_6)
            .pl(theme::S_4)
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text_subtle())
                    .child(mark),
            )
            .child(div().flex_1().min_w_0().child(inline(rest)))
            .into_any_element();
    }
    div().child(inline(line)).into_any_element()
}

/// Strip inline markers (`**`/`__` for bold and italics, code backticks); links keep only their
/// text.
fn inline(text: &str) -> String {
    let text = text.replace("**", "").replace("__", "").replace('`', "");
    // [text](url) -> text
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let Some(end) = after[close + 2..].find(')') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&after[..close]);
        rest = &after[close + 2 + end + 1..];
    }
    out.push_str(rest);
    out
}

fn code_block(text: &str) -> AnyElement {
    div()
        .p(theme::S_8)
        .rounded(theme::R_4)
        .bg(theme::wash_strong())
        .font_family("Cascadia Mono")
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .child(text.to_owned())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::inline;

    #[test]
    fn strips_inline_markup() {
        assert_eq!(inline("a **bold** and `code`"), "a bold and code");
        assert_eq!(
            inline("see [the docs](https://x.y/z) now"),
            "see the docs now"
        );
        assert_eq!(inline("[a](b) [c](d)"), "a c");
        assert_eq!(inline("no [link here"), "no [link here");
    }
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
        .gap(theme::S_12)
        .p(theme::S_8)
        .children(WIDTHS.iter().map(|w| {
            div()
                .flex()
                .items_center()
                .gap(theme::S_8)
                .child(
                    div()
                        .size(px(14.))
                        .flex_none()
                        .rounded(theme::R_4)
                        .bg(theme::wash_strong()),
                )
                .child(
                    div()
                        .h(px(10.))
                        .w(gpui::relative(*w))
                        .rounded(theme::R_4)
                        .bg(theme::wash_strong()),
                )
        }));
    if !crate::motion::enabled() {
        return bars.into_any_element();
    }
    gpui::AnimationExt::with_animation(
        bars,
        "preview-skeleton-pulse",
        gpui::Animation::new(SKELETON_PULSE).repeat(),
        |d, t| {
            // Smooth 0 -> 1 -> 0 wave
            let pulse = (1. - (t * std::f32::consts::TAU).cos()) / 2.;
            d.opacity(0.45 + 0.55 * pulse)
        },
    )
    .into_any_element()
}
