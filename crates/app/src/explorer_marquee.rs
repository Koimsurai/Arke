//! Marquee selection: drag from empty list space (below or right of the rows, gaps in the grid) to
//! select everything inside the box, in a folder's list or in search results. Ctrl adds to the
//! selection; dragging past the edge auto-scrolls.
//! Pressing on an item still starts a file drag, like Explorer.
//!
//! The start point is kept in content coordinates (window minus scroll) so the box grows while
//! scrolling. Drag events are registered with the window during paint so they arrive outside the
//! list.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, UniformListScrollHandle, canvas, div, point, prelude::*, px,
};

use super::{Explorer, ViewMode};
use crate::theme;

/// Scroll per mouse move when dragged past the edge, as a fraction of the overshoot
const EDGE_SCROLL: f32 = 0.5;

pub(crate) struct Marquee {
    /// Start point (content coordinates)
    start: Point<Pixels>,
    /// Current cursor (window coordinates)
    current: Point<Pixels>,
    /// Selection before the drag (kept with Ctrl)
    base: Vec<Arc<str>>,
    /// Past the threshold: only then draw the box and change the selection
    dragging: bool,
}

/// Visible list bounds (for marquee and auto-scroll)
pub(crate) type ListBounds = Rc<Cell<Bounds<Pixels>>>;

impl Explorer {
    /// The list the marquee is over: the search results, or the folder's.
    const fn marquee_list(&self) -> &UniformListScrollHandle {
        if self.search_mode {
            &self.search_scroll
        } else {
            &self.scroll
        }
    }

    fn scroll_offset(&self) -> Point<Pixels> {
        self.marquee_list().0.borrow().base_handle.offset()
    }

    /// Marquee works in the plain list or grid and in search results, not in the column view.
    fn marquee_allowed(&self) -> bool {
        self.model.is_some() && !self.columns_active() && self.renaming.is_none()
    }

    /// Left press on the list. Items handle their own presses; empty space starts a marquee.
    pub(super) fn on_list_left_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.row_pressed) || !self.marquee_allowed() {
            return;
        }
        let additive = event.modifiers.control;
        // Click on empty space clears the selection, like Explorer.
        let base = if self.search_mode {
            self.search_marquee_base(additive)
        } else {
            if !additive {
                self.selection.clear();
                self.cursor = None;
            }
            self.selection.names().cloned().collect()
        };
        let offset = self.scroll_offset();
        self.marquee = Some(Marquee {
            start: event.position - offset,
            current: event.position,
            base,
            dragging: false,
        });
        cx.notify();
    }

    fn marquee_move(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        // Read from the list before the marquee is borrowed.
        let handle = self.marquee_list().0.borrow().base_handle.clone();
        let offset = handle.offset();
        let view = self.list_bounds.get();
        let Some(marquee) = self.marquee.as_mut() else {
            return;
        };
        marquee.current = position;
        let moved = (position - (marquee.start + offset)).map(|v| v.abs());
        let threshold = cx.drag_threshold();
        if !marquee.dragging && moved.x < threshold.width && moved.y < threshold.height {
            return;
        }
        marquee.dragging = true;
        // Dragged outside the list: scroll that way.
        let over = if position.y > view.bottom() {
            position.y - view.bottom()
        } else if position.y < view.top() {
            position.y - view.top()
        } else {
            px(0.)
        };
        if over != px(0.) {
            let max = handle.max_offset().y;
            let y = (offset.y - over * EDGE_SCROLL).min(px(0.)).max(-max);
            handle.set_offset(point(offset.x, y));
        }
        self.apply_marquee();
        cx.notify();
    }

    fn marquee_up(&mut self, cx: &mut Context<Self>) {
        if self.marquee.take().is_some() {
            cx.notify();
        }
    }

    /// Update the selection from the box.
    fn apply_marquee(&mut self) {
        let Some(marquee) = &self.marquee else {
            return;
        };
        let offset = self.scroll_offset();
        let bounds = self.list_bounds.get();
        // Convert to list content coordinates (origin at the list's top-left when scrolled to top).
        let to_content = |p: Point<Pixels>| p - bounds.origin;
        let a = to_content(marquee.start + offset) - offset;
        let b = to_content(marquee.current) - offset;
        let area = Bounds::from_corners(
            point(a.x.min(b.x), a.y.min(b.y)),
            point(a.x.max(b.x), a.y.max(b.y)),
        );
        let base = marquee.base.clone();
        if self.search_mode {
            self.apply_search_marquee(area, base);
            return;
        }
        let Some(view) = self.view() else {
            return;
        };
        let len = view.len();
        let hits = if self.view_mode == ViewMode::Icons {
            self.marquee_tiles(area, len)
        } else {
            // Grouped rows include headers: map to items and skip the headers.
            self.marquee_rows(area, len, |row| self.item_at_row(row))
        };
        // The selection before the drag (kept with Ctrl) and the items in the box.
        self.selection.select_names(
            base.into_iter().chain(
                hits.iter()
                    .filter_map(|&ix| view.get(ix).map(|e| Arc::clone(&e.name))),
            ),
        );
        if let Some(&last) = hits.last() {
            self.cursor = Some(last);
            self.selection
                .set_cursor_name(view.get(last).map(|e| Arc::clone(&e.name)));
        }
    }

    /// Tiles the box (`area`, list content coordinates) reaches into, of `len` items.
    fn marquee_tiles(&self, area: Bounds<Pixels>, len: usize) -> Vec<usize> {
        let (x0, x1, y0, y1) = (area.left(), area.right(), area.top(), area.bottom());
        let cols = self.grid_cols.max(1);
        let top = theme::s_4();
        let left = theme::s_8();
        let tile = self.tile();
        let row = |y: Pixels| ((y - top) / tile.h).floor();
        let col = |x: Pixels| ((x - left) / tile.w).floor();
        let (r0, r1) = (row(y0).max(0.), row(y1));
        let (c0, c1) = (col(x0).max(0.), col(x1).min((cols - 1) as f32));
        let mut hits: Vec<usize> = Vec::new();
        // Grouped grid works per cell: find the cell rows that overlap the box.
        if let Some(groups) = self.grid_groups() {
            let slot_h = tile.h / super::groups::TILE_SLOTS as f32;
            let slot = |y: Pixels| ((y - top) / slot_h).floor().max(0.);
            let (s0, s1) = (slot(y0) as usize, slot(y1) as usize);
            if c1 >= c0 {
                let (c0, c1) = (c0 as usize, c1 as usize);
                for range in groups.rows_in_slots(s0, s1) {
                    hits.extend(
                        (range.start + c0..=range.start + c1).filter(|ix| range.contains(ix)),
                    );
                }
            }
        } else if r1 >= r0 && c1 >= c0 {
            for r in (r0 as usize)..=(r1 as usize) {
                for c in (c0 as usize)..=(c1 as usize) {
                    let ix = r * cols + c;
                    if ix < len {
                        hits.push(ix);
                    }
                }
            }
        }
        hits
    }

    /// Items whose rows the box (`area`, list content coordinates) reaches into, of `len` items.
    /// `item` maps a list row to its item: `None` for a group header or "Similar results".
    pub(super) fn marquee_rows(
        &self,
        area: Bounds<Pixels>,
        len: usize,
        item: impl Fn(usize) -> Option<usize>,
    ) -> Vec<usize> {
        let height = self.row_height();
        let (r0, r1) = (
            (area.top() / height).floor().max(0.),
            (area.bottom() / height).floor(),
        );
        // Rows start after the list's left inset and end with the last column (in the column
        // view they span the list); the box has to reach into them.
        let left = theme::list_inset();
        let overlaps =
            area.right() >= left && self.cols.is_none_or(|c| area.left() <= left + c.row);
        if !overlaps || r1 < r0 {
            return Vec::new();
        }
        (r0 as usize..=r1 as usize)
            .filter_map(item)
            .take_while(|&ix| ix < len)
            .collect()
    }

    /// Tests: list bounds (set after the first paint).
    #[cfg(test)]
    pub(crate) fn list_bounds_for_test(&self) -> Bounds<Pixels> {
        self.list_bounds.get()
    }

    /// Tests: current list scroll offset.
    #[cfg(test)]
    pub(crate) fn list_scroll_for_test(&self) -> Point<Pixels> {
        self.scroll.0.borrow().base_handle.offset()
    }

    /// Tests: how far the list can scroll each way.
    #[cfg(test)]
    pub(crate) fn list_scroll_max_for_test(&self) -> Point<Pixels> {
        self.scroll.0.borrow().base_handle.max_offset()
    }

    /// Tests: the sideways scroll the header was drawn at (`None`: no header).
    #[cfg(test)]
    pub(crate) fn header_offset_for_test(&self) -> Option<Pixels> {
        self.header_x.get()
    }

    /// Tests: scrolls the list as the wheel would (`top` pixels down from the start).
    #[cfg(test)]
    pub(crate) fn set_list_scroll_for_test(&self, top: Pixels) {
        let state = self.scroll.0.borrow();
        state
            .base_handle
            .set_offset(gpui::point(Pixels::ZERO, -top));
    }

    /// Marquee layer over the list: measures the list, registers drag events, draws the box.
    pub(super) fn render_marquee(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let bounds = Rc::clone(&self.list_bounds);
        let active = self.marquee.is_some();
        let this = cx.entity();
        let capture = canvas(
            move |b, _, _| bounds.set(b),
            move |_, (), window, _| {
                if !active {
                    return;
                }
                let target = this.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        target.update(cx, |this, cx| {
                            if e.pressed_button == Some(MouseButton::Left) {
                                this.marquee_move(e.position, cx);
                            } else {
                                this.marquee_up(cx);
                            }
                        });
                    }
                });
                let target = this;
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        target.update(cx, Self::marquee_up);
                    }
                });
            },
        )
        .absolute()
        .size_full()
        .into_any_element();
        let mut out = vec![capture];
        if let Some(marquee) = self.marquee.as_ref().filter(|m| m.dragging) {
            let origin = self.list_bounds.get().origin;
            let start = marquee.start + self.scroll_offset() - origin;
            let end = marquee.current - origin;
            let (x0, y0) = (start.x.min(end.x), start.y.min(end.y));
            let (w, h) = ((start.x - end.x).abs(), (start.y - end.y).abs());
            out.push(
                div()
                    .absolute()
                    .left(x0)
                    .top(y0)
                    .w(w)
                    .h(h)
                    .rounded(theme::r_4())
                    .bg(theme::signal_wash())
                    .border_1()
                    .border_color(theme::signal_line())
                    .into_any_element(),
            );
        }
        out
    }
}
