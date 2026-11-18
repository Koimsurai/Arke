//! The title row around the path bar (#120).
//!
//! - The path bar fills the row up to the search box, so the whole "address bar area" is a target.
//!   The search box keeps a fixed width; while typing it grows with its text (up to a cap) and the
//!   path bar gives up only that much, folding middle levels into "…". While the path is edited it
//!   keeps its width unless the row is narrow; then it steps aside to its magnifier, and a press
//!   on that ends the path edit and focuses the search box at once.
//! - With a preview column below (the preview pane, or a split tab's shared preview), the gap
//!   between the path bar and the search box sits over its divider, so it reads as part of that
//!   line; the search box runs from there to the window buttons (180 to 560 px, else the usual
//!   layout).
//! - A double click on the path bar's blank part maximizes or restores the window, as on any title
//!   bar (the first click starts editing; the second ends it).
//! - Every empty bit of the row (between controls, above and below them) drags the window (double
//!   click maximizes).
//! - A press on the path bar's blank end that moves past the system drag distance moves the
//!   window instead of editing the path, for people who drag the title row from habit. Windows
//!   runs the move, so Snap works as on any title bar.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, Focusable as _, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Window, canvas, div, prelude::*, px,
};

use super::Explorer;
use super::layout::{self, SearchBox};
use crate::theme;

/// Widest the search box gets while typing (it also stops at half the row).
const TYPING_MAX: Pixels = px(560.);
/// Space before the path bar.
const PATH_LEAD: Pixels = theme::S_4;
/// Search box width in a split pane (half width).
const SPLIT_SEARCH_W: Pixels = px(150.);
/// Space between the path bar and the search box.
const ROW_GAP: Pixels = theme::S_8;
/// With a preview column below, the gap between the path bar and the search box sits over its
/// divider: this much on each side of it (about the divider and the list's scrollbar).
const DIVIDER_GAP: Pixels = px(14.);
/// The search box lines up with the preview column only while that leaves it at least this wide
/// (a wider column keeps it at [`TYPING_MAX`], right-aligned).
const ALIGNED_MIN: Pixels = px(180.);
/// A path bar narrower than this shows only "… › current folder" (no trailing ›, little blank
/// end), so the current folder's name keeps some room.
const PATH_COMPACT_BELOW: Pixels = px(200.);
/// A press from touch or a pen moves the window only past this many times the drag distance.
const TOUCH_DRAG_FACTOR: f32 = 3.;

/// Title row state kept between frames.
#[derive(Default)]
pub(crate) struct TopRow {
    /// Where the part of the row the path bar and the search box share was, at the last layout.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Right edge of the whole title row at the last layout (the pane's right edge).
    bar_right: Rc<Cell<Pixels>>,
    /// What the search box did at the last render, and the width it was drawn at.
    last: Option<(SearchBox, Pixels)>,
    /// A width change in progress: the offset from the new width it started at, and when.
    slide: Option<(Pixels, Instant)>,
    /// Left press on the path bar's blank end (window coordinates) and whether it came from touch
    /// or a pen, until it's released or turns into a window move.
    press: Option<(Point<Pixels>, bool)>,
    /// Where the search box's text was drawn while it showed its query without being used.
    summary_text: Rc<Cell<Bounds<Pixels>>>,
    moved: Moved,
    /// When and where a click on the path bar's blank part started editing.
    edit_click: Option<(Instant, Point<Pixels>)>,
    /// The search box was focused on this press already (its magnifier, while the path was
    /// edited): the click that ends the press leaves its text alone.
    pub(super) search_focused_on_press: bool,
}

/// Whether the latest press on the path bar became a window move.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Moved {
    #[default]
    No,
    /// Handed to Windows from this point. Windows replays the press there as a title bar press
    /// (GPUI sees it too), and the release that ends the move isn't a click.
    Started(Point<Pixels>),
    /// The replayed press arrived; the click that may still follow isn't an edit either.
    Replayed,
}

impl Explorer {
    /// What the search box is doing now.
    pub(super) fn search_box_state(&self, window: &Window, cx: &gpui::App) -> SearchBox {
        if self.editing_path {
            return SearchBox::PathEditing;
        }
        let in_box = self
            .filter_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        // The search panel below counts as part of the box.
        if in_box || self.panel_focus.contains_focused(window, cx) {
            return SearchBox::Typing;
        }
        SearchBox::Idle {
            text: !self.query.is_empty() || self.search_mode,
        }
    }

    /// Width the search box needs to show all its text with its magnifier, clear button and
    /// scope chip (zero while empty).
    fn search_content_width(&self, window: &Window, cx: &gpui::App) -> Pixels {
        let input = self.filter_input.read(cx);
        let text = input.text();
        if text.is_empty() {
            return px(0.);
        }
        let normal = gpui::FontWeight::NORMAL;
        let chip = theme::S_6 * 2.
            + theme::ICON_SM
            + self.scope_chip_label().map_or(px(0.), |label| {
                theme::S_4 + text_width(&label, theme::FS_12, normal, window)
            });
        // Padding and border, the magnifier, the gaps between the parts, the clear button and room
        // for the caret.
        theme::S_12
            + theme::S_4
            + px(2.)
            + theme::ICON
            + theme::S_6 * 3.
            + text_width(text, theme::FS_14, normal, window)
            + theme::S_20
            + chip
            + theme::S_8
    }

    /// Resting width of the search box that puts the gap before it right over the divider of the
    /// preview column below (this pane's preview pane, or a split tab's shared preview right of
    /// it). `None`: no such column, or it would leave the search box narrower than
    /// [`ALIGNED_MIN`].
    ///
    /// The column's width is this frame's (so the gap follows a divider drag at once); how far
    /// the row's end sits from the pane's right edge comes from the last layout, and only changes
    /// when the window buttons or the progress ring come or go.
    fn aligned_search_width(&self) -> Option<Pixels> {
        let column = if self.pane.trailing && self.pane.reserve > px(0.) {
            self.pane.reserve
        } else if self.preview_shown() && !self.quick_look {
            self.fit.preview_w
        } else {
            return None;
        };
        let row = self.top_row.bounds.get();
        let bar_right = self.top_row.bar_right.get();
        if row.size.width <= px(0.) || bar_right <= px(0.) {
            return None;
        }
        // Past the row's end: the window buttons and spacing.
        let tail = bar_right - row.right();
        // The paper (holding the list and the preview) ends a margin before the pane's edge.
        let width = column + layout::PAPER_MARGIN - DIVIDER_GAP - tail;
        (width >= ALIGNED_MIN).then(|| width.min(TYPING_MAX))
    }

    /// The search box's width for `state` (`content`: what its text needs) in the row measured at
    /// the last layout, easing toward it when the state changes; and the gap before it.
    fn search_box_width(
        &mut self,
        state: SearchBox,
        content: Pixels,
        window: &Window,
    ) -> (Pixels, Pixels) {
        let aligned = self.aligned_search_width();
        let normal = aligned.unwrap_or(if self.pane.split {
            SPLIT_SEARCH_W
        } else {
            theme::FILTER_W
        });
        // At its cap the box no longer reaches the divider: the usual spacing, and the path bar
        // takes the rest.
        let gap = if aligned.is_some_and(|w| w < TYPING_MAX) {
            DIVIDER_GAP * 2.
        } else {
            ROW_GAP
        };
        let row = self.top_row.bounds.get().size.width;
        // Typing widens it with the text, up to half the row or TYPING_MAX.
        let typing_max = (self.bar_width * 0.5).min(TYPING_MAX);
        let target = layout::search_width(
            (row - PATH_LEAD - gap).max(px(0.)),
            state,
            normal,
            typing_max,
            content,
        );
        let now = Instant::now();
        // Leaving or entering path editing snaps: the box is clicked or typed into right then.
        if let Some((last, drawn)) = self.top_row.last
            && last != state
            && last != SearchBox::PathEditing
            && state != SearchBox::PathEditing
            && row > px(0.)
            && crate::motion::enabled()
        {
            self.top_row.slide = Some((drawn - target, now));
        }
        let width = match self.top_row.slide {
            Some((offset, started)) => {
                let left = crate::motion::remaining(now.saturating_duration_since(started));
                if left > 0. {
                    window.request_animation_frame();
                    target + offset * left
                } else {
                    self.top_row.slide = None;
                    target
                }
            }
            None => target,
        };
        self.top_row.last = Some((state, width));
        (width, gap)
    }

    /// The part of the title row shared by the path bar and the search box. Its width is measured
    /// so the search box can size against it.
    pub(super) fn render_top_row(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let state = self.search_box_state(window, cx);
        let content = self.search_content_width(window, cx);
        let (search_w, gap) = self.search_box_width(state, content, window);
        // A narrow path bar (as laid out on the last frame) keeps only the current folder.
        let slot = self.path_slot.get();
        let compact = slot > px(0.) && slot < PATH_COMPACT_BELOW;
        let bounds = Rc::clone(&self.top_row.bounds);
        // The search box sizes from these bounds: when they change, draw once more with the new
        // value (they don't depend on the row's content, so this settles at once).
        let capture = canvas(
            move |b, window, _| {
                if bounds.replace(b) != b {
                    window.refresh();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full();
        div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .child(capture)
            .child(Self::title_drag("path-lead", cx).w(PATH_LEAD))
            .child(self.render_path_slot(compact, window, cx))
            .child(
                Self::title_drag("path-search-gap", cx)
                    .debug_selector(|| "path-search-gap".to_owned())
                    .w(gap),
            )
            .child(self.render_filter_box(state, search_w, window, cx))
            .into_any_element()
    }

    /// Measures the whole title row (its right edge is the pane's), for lining the search box up
    /// with the preview column below.
    pub(super) fn bar_right_probe(&self) -> AnyElement {
        let right = Rc::clone(&self.top_row.bar_right);
        canvas(
            move |b, window, _| {
                if right.replace(b.right()) != b.right() {
                    window.refresh();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// Empty title bar between controls: drags the window and maximizes on double click (the
    /// system handles both through the caption hit test); right click opens the window menu.
    ///
    /// Only ever a sibling of controls: the system asks GPUI about the topmost window control
    /// area under the pointer, so a drag area around a control would take its clicks.
    pub(super) fn title_drag(
        id: impl Into<gpui::ElementId>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .h_full()
            .window_control_area(gpui::WindowControlArea::Drag)
            // Click on empty title bar space ends path editing (not consumed, so the window still
            // drags).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.cancel_path_edit(window, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|_, e: &MouseUpEvent, _, cx| {
                    cx.emit(super::ExplorerEvent::WindowMenu(e.position));
                    cx.stop_propagation();
                }),
            )
    }

    /// Empty title bar above and below the controls (they're shorter than the row), as drag
    /// areas. They stop `right` short of the row's end, where the window buttons sit.
    pub(super) fn title_drag_bands(right: Pixels, cx: &mut Context<Self>) -> [AnyElement; 2] {
        let band = (theme::H_BAR - theme::H_CTL) / 2.;
        [
            Self::title_drag("title-band-top", cx)
                .absolute()
                .top_0()
                .left_0()
                .right(right)
                .h(band)
                .into_any_element(),
            Self::title_drag("title-band-bottom", cx)
                .absolute()
                .bottom_0()
                .left_0()
                .right(right)
                .h(band)
                .into_any_element(),
        ]
    }

    // ───────────────────────── Press on the path bar: click or window drag ─────────────────────────

    /// Any press on the path bar (capture phase, before the parts see it): a new press forgets
    /// the last move; the press Windows replays to start a move doesn't.
    pub(super) fn path_bar_pressed(&mut self, e: &MouseDownEvent) {
        self.top_row.moved = match self.top_row.moved {
            Moved::Started(at)
                if !layout::beyond_drag(at, e.position, gpui::size(px(2.), px(2.))) =>
            {
                Moved::Replayed
            }
            _ => Moved::No,
        };
    }

    /// Left press on the path bar's blank end: a click there edits the path, a drag moves the
    /// window.
    pub(super) fn path_blank_pressed(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        if e.button != MouseButton::Left || self.top_row.moved != Moved::No {
            return;
        }
        self.top_row.press = Some((e.position, self.services.shell.press_from_touch()));
        // Draw once more so the listener that follows the pointer is in place.
        cx.notify();
    }

    /// A click on the path bar: whether it edits the path (not when it ends a window move).
    pub(super) fn path_click_edits(&mut self) -> bool {
        self.top_row.press = None;
        std::mem::take(&mut self.top_row.moved) == Moved::No
    }

    fn path_press_moved(&mut self, e: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let Some((at, touch)) = self.top_row.press else {
            return;
        };
        if e.pressed_button != Some(MouseButton::Left) {
            self.top_row.press = None;
            return;
        }
        let scale = window.scale_factor();
        let (dx, dy) = self.services.shell.drag_threshold(self.owner);
        // A finger wobbles more than a mouse: a tap on the path bar should still edit it.
        let k = if touch { TOUCH_DRAG_FACTOR } else { 1. };
        let threshold = gpui::size(px(dx as f32 * k / scale), px(dy as f32 * k / scale));
        if !layout::beyond_drag(at, e.position, threshold) {
            return;
        }
        self.top_row.press = None;
        self.top_row.moved = Moved::Started(at);
        // From where the press was, so the window keeps its offset to the pointer as it moves.
        let physical = (
            (f32::from(at.x) * scale).round() as i32,
            (f32::from(at.y) * scale).round() as i32,
        );
        self.services.shell.begin_window_move(self.owner, physical);
        cx.notify();
    }

    /// While a press on the path bar's blank end is held: follows the pointer anywhere in the
    /// window (it may leave the bar before reaching the drag distance).
    pub(super) fn path_press_tracker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.top_row.press?;
        let me = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| {},
                move |_, (), window, _| {
                    let on_move = me.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture
                            && let Some(this) = on_move.upgrade()
                        {
                            this.update(cx, |this, cx| this.path_press_moved(e, window, cx));
                        }
                    });
                    let on_up = me;
                    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble
                            && let Some(this) = on_up.upgrade()
                        {
                            this.update(cx, |this, _| this.top_row.press = None);
                        }
                    });
                },
            )
            .absolute()
            .size_0()
            .into_any_element(),
        )
    }

    // ───────────────────────── Double click on the path bar: maximize ─────────────────────────

    /// A click on the path bar's blank part started editing at `at`: a second press there soon
    /// after is a title bar double click.
    pub(super) fn path_edit_clicked(&mut self, at: Point<Pixels>) {
        self.top_row.edit_click = Some((Instant::now(), at));
    }

    /// A press on the path being edited: whether it completes a double click with the click that
    /// started editing (within the system double-click time and distance). If so, editing ends and
    /// the window maximizes or restores, as a double click on the title bar does; the press goes no
    /// further (it doesn't select a word).
    pub(super) fn path_edit_pressed(
        &mut self,
        e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((when, at)) = self.top_row.edit_click.take() else {
            return;
        };
        if e.button != MouseButton::Left {
            return;
        }
        let (time, (w, h)) = self.services.shell.double_click_rule(self.owner);
        let scale = window.scale_factor();
        // The system's double-click rectangle is centered on the first click.
        let reach = gpui::size(px(w as f32 / scale / 2.), px(h as f32 / scale / 2.));
        if when.elapsed() > time || layout::beyond_drag(at, e.position, reach) {
            return;
        }
        cx.stop_propagation();
        self.stop_edit_path(window, cx);
        self.services.shell.toggle_maximize(self.owner);
    }

    // ───────────────────────── The search box holding a query ─────────────────────────

    /// Presses on the search box while it isn't being typed in. Just the magnifier (`collapsed`):
    /// focus on the press, so typing right away goes into the box (a path being edited has ended
    /// on this press already). Holding a query (`summary`): the caret goes where it was pressed,
    /// as in the box being typed in, and a right-click gives its edit menu.
    pub(super) fn search_box_presses(
        el: gpui::Stateful<gpui::Div>,
        collapsed: bool,
        summary: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                this.top_row.search_focused_on_press = collapsed || summary;
                if collapsed {
                    this.focus_filter_input(window, cx);
                } else if summary {
                    this.search_summary_pressed(e.position, false, window, cx);
                }
            }),
        )
        .when(summary, |d| {
            d.on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.search_summary_pressed(e.position, true, window, cx);
                }),
            )
        })
        .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
            if !std::mem::take(&mut this.top_row.search_focused_on_press) {
                this.focus_filter_input(window, cx);
            }
        }))
    }

    /// Records where the search box's text is drawn while it shows its query without being used
    /// (the text box itself isn't drawn then).
    pub(super) fn search_summary_measure(&self) -> AnyElement {
        let drawn = Rc::clone(&self.top_row.summary_text);
        canvas(move |bounds, _, _| drawn.set(bounds), |_, (), _, _| {})
            .absolute()
            .size_full()
            .into_any_element()
    }

    /// A press on the search box showing its query: it's focused with the caret where the press
    /// was, as in a box being typed in, not with everything selected. With the right button
    /// (`menu`) its edit menu opens there too.
    pub(super) fn search_summary_pressed(
        &mut self,
        at: Point<Pixels>,
        menu: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.filter_input.read(cx).text().to_owned();
        let left = self.top_row.summary_text.get().left();
        let offset = text_offset(&text, at.x - left, theme::FS_14, window);
        self.panel_dismissed = false;
        self.filter_input.update(cx, |input, cx| {
            input.select_range(offset..offset, cx);
            if menu {
                input.open_menu_at(at, window, cx);
            }
        });
        if !menu {
            let focus = self.filter_input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Test helper: where the search box's query was drawn while it wasn't in use.
    #[cfg(test)]
    pub(crate) fn search_summary_for_test(&self) -> Bounds<Pixels> {
        self.top_row.summary_text.get()
    }
}

/// The character boundary in `text` (UI font at `size`) closest to `x` from its start.
fn text_offset(text: &str, x: Pixels, size: Pixels, window: &Window) -> usize {
    let run = gpui::TextRun {
        len: text.len(),
        font: theme::ui_font_at(gpui::FontWeight::NORMAL),
        color: gpui::Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        gpui::SharedString::from(text.to_owned()),
        size,
        &[run],
        None,
    );
    text.floor_char_boundary(line.closest_index_for_x(x.max(px(0.))))
}

/// Width of `text` in the UI font.
pub(super) fn text_width(
    text: &str,
    size: Pixels,
    weight: gpui::FontWeight,
    window: &Window,
) -> Pixels {
    let run = gpui::TextRun {
        len: text.len(),
        font: theme::ui_font_at(weight),
        color: gpui::Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(
            gpui::SharedString::from(text.to_owned()),
            size,
            &[run],
            None,
        )
        .width
}
