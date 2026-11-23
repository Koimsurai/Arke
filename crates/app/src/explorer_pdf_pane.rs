//! The preview pane on a multi-page PDF: every page, scrolled through in the pane's frame.
//!
//! Pages sit in equal slots as wide as the frame (the first page's shape, no taller than the
//! frame), so a narrow pane reads like a document viewer. Only the slots in view are built
//! (`uniform_list`); those pages and one more on each side are rendered on background threads (at
//! most two at a time), for the slot's size in device pixels. Pages scrolled further away are freed
//! and leave the GPU atlas, so memory stays at a few frames' worth of pixels however long the PDF
//! is. Quick Look keeps its own page (`PdfView::page`); the page the pane opened with is drawn as
//! is instead of being rendered again.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Pixels, UniformListScrollHandle, div, prelude::*, px, uniform_list,
};
use rust_i18n::t;

use super::super::{AppEvent, spawn_thread};
use super::{Explorer, PdfPage, PdfView, PreviewBody};
use crate::shell::Shell;
use crate::theme;

/// Pages rendered and kept on each side of the ones in view.
const NEAR: u32 = 1;
/// Pages rendered at the same time.
const IN_FLIGHT: usize = 2;
/// Most pages kept whatever the layout (very wide pages fit many to a frame).
const MAX_KEPT: usize = 10;
/// Shortest slot, relative to its width, so banner-shaped pages don't put dozens in view.
const MIN_SLOT_RATIO: f32 = 0.25;
/// Space below each page.
fn gap() -> gpui::Pixels {
    theme::s_8()
}

/// The pane's pages of the PDF on screen.
#[derive(Default)]
pub(crate) struct PanePages {
    scroll: UniformListScrollHandle,
    /// Height to width of the slots, from the first page shown: fixed, so the list keeps its
    /// height when Quick Look turns pages
    aspect: Option<f32>,
    /// Box (device pixels) the slots were last laid out for
    target: (u32, u32),
    /// Rendered pages and the box each was rendered for (an older box stands in after a resize)
    kept: Vec<(u32, (u32, u32), PdfPage)>,
    /// Pages being rendered, with their box
    pending: Vec<(u32, (u32, u32))>,
    /// Pages that didn't render (not tried again)
    failed: Vec<u32>,
    /// Pages the last layout showed
    visible: Range<u32>,
}

impl PanePages {
    /// Pages worth keeping: the ones in view and [`NEAR`] on each side.
    fn keep(&self, pages: u32) -> Range<u32> {
        self.visible.start.saturating_sub(NEAR)..(self.visible.end + NEAR).min(pages)
    }

    /// The best image of `page`: rendered for the current box, or for an older one meanwhile.
    fn image(&self, page: u32) -> Option<&PdfPage> {
        let mut found = self.kept.iter().filter(|(p, _, _)| *p == page);
        let first = found.next()?;
        Some(
            &std::iter::once(first)
                .chain(found)
                .find(|(_, target, _)| *target == self.target)
                .unwrap_or(first)
                .2,
        )
    }

    fn fresh(&self, page: u32) -> bool {
        self.kept
            .iter()
            .any(|(p, target, _)| *p == page && *target == self.target)
    }

    /// Frees every page. Deferred: this runs while a window draws (the preview changes during
    /// render), and `drop_image` skips the window being drawn.
    pub(super) fn release(self, cx: &mut App) {
        if self.kept.is_empty() {
            return;
        }
        cx.defer(move |cx| {
            for (_, _, page) in self.kept {
                crate::media::drop_image_later(page.image, cx);
            }
        });
    }

    /// Pages kept now (tests).
    #[cfg(test)]
    pub(crate) fn kept_pages(&self) -> Vec<u32> {
        let mut pages: Vec<u32> = self.kept.iter().map(|(p, _, _)| *p).collect();
        pages.sort_unstable();
        pages.dedup();
        pages
    }
}

/// A page rendered for the pane's list, on its way to the UI thread.
pub(crate) struct RenderedPage {
    generation: u64,
    page: u32,
    /// Box it was rendered for
    target: (u32, u32),
    /// `None`: it didn't render
    render: Option<PdfPage>,
}

/// Height to width of a rendered page.
fn aspect_of(page: &PdfPage) -> f32 {
    page.height as f32 / page.width.max(1) as f32
}

/// Height of a page slot `width` wide in a frame `frame_h` tall.
fn slot_h(width: Pixels, frame_h: Pixels, aspect: f32) -> Pixels {
    (width * aspect).max(width * MIN_SLOT_RATIO).min(frame_h)
}

/// The slots `item_h` apart that a frame `frame_h` tall shows when scrolled `top` down.
fn in_view(top: Pixels, frame_h: Pixels, item_h: Pixels, pages: u32) -> Range<u32> {
    let (top, frame_h, item_h) = (
        f32::from(top),
        f32::from(frame_h),
        f32::from(item_h).max(1.),
    );
    let start = ((top / item_h).floor().max(0.) as u32).min(pages);
    let end = (((top + frame_h) / item_h).ceil().max(0.) as u32).clamp(start, pages);
    start..end
}

/// Device pixels for a logical length.
fn device(v: Pixels, scale: f32) -> u32 {
    (f32::from(v) * scale).round().max(1.) as u32
}

/// An image rendered for a box close enough to `slot` to draw in it: inside it (give or take
/// rounding) and touching it on one side.
fn fills(have: (u32, u32), slot: (u32, u32)) -> bool {
    let near = |a: u32, b: u32| a.abs_diff(b) * 10 <= b.max(1);
    let inside = |a: u32, b: u32| a <= b || near(a, b);
    inside(have.0, slot.0)
        && inside(have.1, slot.1)
        && (near(have.0, slot.0) || near(have.1, slot.1))
}

/// Renders one page on a background thread.
fn render_page(shell: &dyn Shell, path: &Path, page: u32, target: (u32, u32)) -> Option<PdfPage> {
    match shell.render_pdf(path, page, target) {
        Ok(render) => {
            let (width, height, pages) = (render.image.width, render.image.height, render.pages);
            crate::media::to_render_image(render.image).map(|image| PdfPage {
                image,
                width,
                height,
                pages,
            })
        }
        Err(e) => {
            log::info!("pdf page {page} of {} failed: {e}", path.display());
            None
        }
    }
}

impl Explorer {
    /// The pane's frame on a PDF with several pages: the pages, scrolled through, with the page
    /// in the middle of the frame named at the bottom.
    pub(super) fn pane_pdf_pages(
        &self,
        view: &PdfView,
        frame: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let aspect = view.pane.aspect.unwrap_or_else(|| aspect_of(&view.render));
        let slot = slot_h(frame.width, frame.height, aspect);
        let pages = view.render.pages;
        let base = view.pane.scroll.0.borrow().base_handle.clone();
        // From the scroll offset, not the range the list asks to build: it also builds the first
        // row on every frame to measure it, which would keep the first pages wanted.
        let top = -base.offset().y;
        let visible = in_view(top, frame.height, slot + gap(), pages);
        // The page in the middle; at the end the last one, which may never get there.
        let at_end = top > px(0.) && top + frame.height + px(1.) >= (slot + gap()) * pages as f32;
        let middle = if at_end {
            pages.saturating_sub(1)
        } else {
            in_view(top + frame.height / 2., px(0.), slot + gap(), pages).start
        };
        let width = frame.width;
        let list = uniform_list(
            "pane-pdf-pages",
            pages as usize,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                this.pane_pdf_rows(range, visible.clone(), width, slot, cx)
            }),
        )
        .track_scroll(&view.pane.scroll)
        .size_full();
        div()
            .relative()
            .size_full()
            .debug_selector(|| "pane-pdf-pages".into())
            .child(list)
            .child(crate::scrollbar::scrollbar(
                base,
                std::rc::Rc::clone(&self.preview_scrollbar),
            ))
            .child(
                div()
                    .absolute()
                    .bottom(theme::s_8())
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px(theme::s_8())
                            .py(theme::s_2())
                            .rounded(theme::R_FULL)
                            .bg(theme::overlay())
                            .shadow(theme::elev_float())
                            .text_size(theme::fs_11())
                            .text_color(theme::text_muted())
                            .child(
                                t!(
                                    "preview.page_of",
                                    page = arke_core::format::format_count(
                                        middle.min(pages.saturating_sub(1)) as usize + 1
                                    ),
                                    pages = arke_core::format::format_count(pages as usize)
                                )
                                .into_owned(),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The slots the list builds (`range`), asking for the pages in view (`visible`) and their
    /// neighbors.
    fn pane_pdf_rows(
        &mut self,
        range: Range<usize>,
        visible: Range<u32>,
        width: Pixels,
        slot: Pixels,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let scale = self.screen.1;
        let target = (device(width, scale), device(slot, scale));
        let generation = self.preview_gen;
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        let Some(preview) = self.preview.as_mut() else {
            return Vec::new();
        };
        let PreviewBody::Pdf(view) = &mut preview.body else {
            return Vec::new();
        };
        let pages = view.render.pages;
        // The page the pane opened with, when it was rendered for about this slot.
        let opened = (view.pending.is_none()
            && fills((view.render.width, view.render.height), target))
        .then_some(view.page);
        let first = view.render.clone();
        let list = &mut view.pane;
        list.aspect.get_or_insert_with(|| aspect_of(&first));
        list.target = target;
        let (start, end) = (visible.start, visible.end);
        list.visible = visible;
        let keep = list.keep(pages);
        // The pages in view first, then the next one and the previous one.
        let wanted = (start..end)
            .chain(end..keep.end)
            .chain((keep.start..start).rev());
        for page in wanted {
            if list.pending.len() >= IN_FLIGHT {
                break;
            }
            if opened == Some(page)
                || list.fresh(page)
                || list.failed.contains(&page)
                || list.pending.iter().any(|(p, _)| *p == page)
            {
                continue;
            }
            list.pending.push((page, target));
            let (shell, tx, path) = (Arc::clone(&shell), tx.clone(), preview.path.clone());
            spawn_thread("pdf-pane-page", move || {
                let render = render_page(shell.as_ref(), &path, page, target);
                let _ = tx.send(AppEvent::PanePdfPage(RenderedPage {
                    generation,
                    page,
                    target,
                    render,
                }));
            });
        }
        let stray =
            list.kept.len() > MAX_KEPT || list.kept.iter().any(|(p, _, _)| !keep.contains(p));
        if stray {
            // Not now: `drop_image` skips the window being drawn, which would keep the pixels.
            let this = cx.entity();
            cx.defer(move |cx| this.update(cx, Self::evict_pane_pages));
        }
        let area = gpui::size(width, slot);
        range
            .map(|ix| {
                let page = u32::try_from(ix).unwrap_or(u32::MAX);
                let image = if opened == Some(page) {
                    Some(&first)
                } else {
                    list.image(page)
                };
                let failed = list.failed.contains(&page);
                div()
                    .w(width)
                    .h(slot + gap())
                    .pb(gap())
                    .debug_selector(move || format!("pane-pdf-page-{}", page + 1))
                    .child(if let Some(render) = image {
                        Self::pdf_page_view(render, area, false)
                    } else {
                        // Until it renders: a blank sheet of the slot's shape, pulsing like the
                        // other placeholders (still once it failed, so nothing keeps drawing).
                        let sheet = div()
                            .size_full()
                            .rounded(theme::r_4())
                            .border_1()
                            .border_color(theme::line())
                            .bg(theme::wash());
                        if failed {
                            sheet.into_any_element()
                        } else {
                            super::pulsing(
                                sheet,
                                gpui::ElementId::NamedInteger(
                                    "pane-pdf-wait".into(),
                                    u64::from(page),
                                ),
                            )
                        }
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// A page rendered for the pane's list.
    pub(crate) fn handle_pane_pdf_page(&mut self, rendered: RenderedPage, cx: &mut Context<Self>) {
        let RenderedPage {
            generation,
            page,
            target,
            render,
        } = rendered;
        // Another item (or nothing) by now: the pixels never reached the GPU, dropping is enough.
        if generation != self.preview_gen {
            return;
        }
        let Some(PreviewBody::Pdf(view)) = self.preview.as_mut().map(|p| &mut p.body) else {
            return;
        };
        let pages = view.render.pages;
        let list = &mut view.pane;
        list.pending.retain(|&(p, t)| (p, t) != (page, target));
        match render {
            None => list.failed.push(page),
            Some(render) if list.keep(pages).contains(&page) => {
                for (_, _, old) in list.kept.extract_if(.., |(p, _, _)| *p == page) {
                    crate::media::drop_image_later(old.image, cx);
                }
                list.kept.push((page, target, render));
            }
            // Scrolled away meanwhile.
            Some(_) => {}
        }
        cx.notify();
    }

    /// Frees the pages scrolled away from, and the farthest ones over [`MAX_KEPT`].
    fn evict_pane_pages(&mut self, cx: &mut Context<Self>) {
        let Some(PreviewBody::Pdf(view)) = self.preview.as_mut().map(|p| &mut p.body) else {
            return;
        };
        let pages = view.render.pages;
        let list = &mut view.pane;
        let keep = list.keep(pages);
        let mut gone: Vec<PdfPage> = list
            .kept
            .extract_if(.., |(p, _, _)| !keep.contains(p))
            .map(|(_, _, page)| page)
            .collect();
        if list.kept.len() > MAX_KEPT {
            let middle = i64::from(list.visible.start + list.visible.end) / 2;
            list.kept
                .sort_by_key(|(p, _, _)| (i64::from(*p) - middle).abs());
            gone.extend(list.kept.drain(MAX_KEPT..).map(|(_, _, page)| page));
        }
        for page in gone {
            crate::media::drop_image_later(page.image, cx);
        }
    }

    /// Pages the pane keeps rendered (tests).
    #[cfg(test)]
    pub(crate) fn pane_pdf_pages_for_test(&self) -> Option<Vec<u32>> {
        match self.preview.as_ref().map(|p| &p.body) {
            Some(PreviewBody::Pdf(view)) => Some(view.pane.kept_pages()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_follow_the_page_but_stay_in_the_frame() {
        // Portrait A4 in a short frame: the frame's height.
        assert_eq!(
            slot_h(
                crate::theme::scaled_px(300.),
                crate::theme::scaled_px(390.),
                1.414
            ),
            crate::theme::scaled_px(390.)
        );
        // Landscape: the page's own height at the frame's width.
        assert_eq!(
            slot_h(
                crate::theme::scaled_px(300.),
                crate::theme::scaled_px(390.),
                0.5
            ),
            crate::theme::scaled_px(150.)
        );
        // A banner doesn't shrink to a sliver.
        assert_eq!(
            slot_h(
                crate::theme::scaled_px(300.),
                crate::theme::scaled_px(390.),
                0.01
            ),
            crate::theme::scaled_px(75.)
        );
    }

    #[test]
    fn pages_in_view_follow_the_scroll() {
        // Slots 100 apart in a 250 tall frame.
        assert_eq!(
            in_view(
                px(0.),
                crate::theme::scaled_px(250.),
                crate::theme::scaled_px(100.),
                10
            ),
            0..3
        );
        assert_eq!(
            in_view(
                crate::theme::scaled_px(150.),
                crate::theme::scaled_px(250.),
                crate::theme::scaled_px(100.),
                10
            ),
            1..4
        );
        assert_eq!(
            in_view(
                crate::theme::scaled_px(900.),
                crate::theme::scaled_px(250.),
                crate::theme::scaled_px(100.),
                10
            ),
            9..10
        );
        // Past the end (before the list clamps its offset), and an empty document.
        assert_eq!(
            in_view(
                crate::theme::scaled_px(5000.),
                crate::theme::scaled_px(250.),
                crate::theme::scaled_px(100.),
                10
            ),
            10..10
        );
        assert_eq!(
            in_view(
                px(0.),
                crate::theme::scaled_px(250.),
                crate::theme::scaled_px(100.),
                0
            ),
            0..0
        );
    }

    #[test]
    fn an_image_fills_a_slot_it_touches() {
        assert!(fills((300, 150), (300, 400)));
        assert!(fills((280, 400), (300, 400)));
        assert!(fills((302, 398), (300, 400)));
        // Too small on both sides, or too big.
        assert!(!fills((150, 200), (300, 400)));
        assert!(!fills((600, 400), (300, 400)));
    }
}
