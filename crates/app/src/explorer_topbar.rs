//! The title row above the panes, and the first row of each pane.
//!
//! Title row: with tabs on top, back, forward and refresh on the left (with tabs on the side they
//! sit in the sidebar's header, which lines up with this row); the search box in the middle; the
//! progress ring and the window buttons on the right. It belongs to the window, not to a pane:
//! the active pane draws it with its own search box, so each pane keeps its own query, switching
//! panes swaps the query shown, and splitting the tab doesn't move anything.
//! - The search box rests at about [`SEARCH_SHARE`] of the row, centered on it. Long text grows it
//!   sideways into the row's empty space, never over the buttons.
//! - While it has focus, the pane it searches is outlined.
//! - Every empty part of the row (between controls, above and below them) drags the window; a
//!   double click there maximizes it.
//!
//! Pane row (the first row of each pane's paper): Up, the path bar, a chip while the pane is
//! filtered, the item count or the selection's actions, and the view buttons.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Focusable as _, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, SharedString, Window, canvas, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::layout::{self, SearchBox};
use super::scope::Scope;
use super::toolbar::ToolbarStop as _;
use super::{Explorer, ExplorerEvent, GoUp};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;
use crate::workspace::TooltipBelowExt as _;

/// Share of the title row the search box takes at rest.
const SEARCH_SHARE: f32 = 0.45;
/// Narrowest and widest the search box rests at.
fn search_rest_min() -> gpui::Pixels {
    crate::theme::scaled_px(280.)
}
fn search_rest_max() -> gpui::Pixels {
    crate::theme::scaled_px(600.)
}
/// A path bar narrower than this shows only "… › current folder" (no trailing ›, little blank
/// end), so the current folder's name keeps some room. The selection's buttons fold into "⋯"
/// before they would push the path bar below it (#388).
pub(super) fn path_compact_below() -> gpui::Pixels {
    crate::theme::scaled_px(200.)
}
/// The path bar keeps at least this much of the pane row; the item count and the selection's
/// actions give way first.
fn path_min() -> gpui::Pixels {
    crate::theme::scaled_px(160.)
}
/// The same in a split pane, half as wide with more view buttons: still "… › current folder".
fn path_min_split() -> gpui::Pixels {
    crate::theme::scaled_px(64.)
}
/// Widest the filter chip gets (the query is cut off with "…"; its tooltip has all of it).
fn filter_chip_max() -> gpui::Pixels {
    crate::theme::scaled_px(180.)
}

/// Where the title row and the part of it between its buttons were at the last layout. The
/// window keeps one, since whichever pane is active draws the row.
#[derive(Clone, Default)]
pub(crate) struct TitleLayout {
    row: Rc<Cell<Bounds<Pixels>>>,
    middle: Rc<Cell<Bounds<Pixels>>>,
}

/// Title row state kept between frames.
#[derive(Default)]
pub(crate) struct TopRow {
    /// What the search box did at the last render, and the width it was drawn at.
    last: Option<(SearchBox, Pixels)>,
    /// A width change in progress: the offset from the new width it started at, and when.
    slide: Option<(Pixels, Instant)>,
    /// Where the search box's text was drawn while it showed its query without being used.
    summary_text: Rc<Cell<Bounds<Pixels>>>,
    /// The search box was focused on this press already (its magnifier, or its query): the
    /// click that ends the press leaves its text alone.
    pub(super) search_focused_on_press: bool,
    /// What the search box's placeholder was made for: the scope, the folder it names, and
    /// whether it is the short one.
    placeholder_for: Option<(Scope, Option<String>, bool)>,
}

/// Records `bounds` of the element it's drawn in; draws once more when they change (they don't
/// depend on what's drawn with them, so this settles at once).
fn measure(cell: &Rc<Cell<Bounds<Pixels>>>) -> AnyElement {
    let cell = Rc::clone(cell);
    canvas(
        move |b, window, _| {
            if cell.replace(b) != b {
                window.refresh();
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .size_full()
    .into_any_element()
}

impl Explorer {
    /// What the search box is doing now.
    pub(super) fn search_box_state(&self, window: &Window, cx: &gpui::App) -> SearchBox {
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
        let chip = theme::s_6() * 2.
            + theme::icon_sm()
            + theme::s_4()
            + text_width(&self.scope_chip_label(), theme::fs_12(), normal, window);
        // Padding and border, the magnifier, the gaps between the parts, the clear button and room
        // for the caret.
        theme::s_12()
            + theme::s_4()
            + px(2.)
            + theme::icon()
            + theme::s_6() * 3.
            + text_width(text, theme::fs_14(), normal, window)
            + theme::s_20()
            + chip
            + theme::s_8()
    }

    /// The search box's width for `state` (`content`: what its text needs) in the title row as
    /// laid out last, easing toward it when the state changes.
    fn search_box_width(
        &mut self,
        state: SearchBox,
        content: Pixels,
        title: &TitleLayout,
        window: &Window,
    ) -> Pixels {
        let row = title.row.get().size.width;
        let rest = (row * SEARCH_SHARE).clamp(search_rest_min(), search_rest_max());
        let target = layout::search_width(title.middle.get().size.width, state, rest, content);
        let now = Instant::now();
        if let Some((last, drawn)) = self.top_row.last
            && last != state
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
        width
    }

    /// Makes the next frame fit the search box's placeholder to its width again.
    pub(super) fn reset_placeholder_fit(&mut self) {
        self.top_row.placeholder_for = None;
    }

    /// The search box's placeholder for the scope in use (see [`Self::filter_placeholder`]); a
    /// box too narrow for the full one shows the short one.
    fn sync_search_placeholder(&mut self, width: Pixels, window: &Window, cx: &mut Context<Self>) {
        let locale = rust_i18n::locale();
        let full = self.filter_placeholder(&locale, false);
        let normal = gpui::FontWeight::NORMAL;
        // Padding and border, the magnifier and the gaps around the text, and the scope chip.
        let chrome = theme::s_12()
            + theme::s_4()
            + px(2.)
            + theme::icon()
            + theme::s_6() * 3.
            + theme::s_6() * 2.
            + theme::icon_sm()
            + theme::s_4()
            + text_width(&self.scope_chip_label(), theme::fs_12(), normal, window)
            + theme::s_8();
        let short =
            width > px(0.) && chrome + text_width(&full, theme::fs_14(), normal, window) > width;
        let key = (
            self.scope(),
            self.search_in_name().or_else(|| self.folder_label()),
            short,
        );
        if self.top_row.placeholder_for.as_ref() == Some(&key) {
            return;
        }
        let text = if short {
            self.filter_placeholder(&locale, true)
        } else {
            full
        };
        self.filter_input
            .update(cx, |input, cx| input.set_placeholder(text, cx));
        self.top_row.placeholder_for = Some(key);
    }

    /// The window's title row, drawn by the active pane (see the module docs). With tabs on top
    /// it is the same; the tab strip goes under it.
    pub(crate) fn render_title_row(
        &mut self,
        title: &TitleLayout,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.search_box_state(window, cx);
        let content = self.search_content_width(window, cx);
        let width = self.search_box_width(state, content, title, window);
        self.sync_search_placeholder(width, window, cx);
        // Centered on the row, kept between the buttons on either side.
        let row = title.row.get();
        let middle = title.middle.get();
        let lead = (row.size.width > px(0.) && middle.size.width > px(0.)).then(|| {
            let centered = row.left() + (row.size.width - width) / 2.;
            let last = (middle.right() - width).max(middle.left());
            centered.clamp(middle.left(), last) - middle.left()
        });
        let mut controls: Vec<AnyElement> = Vec::new();
        controls.push(
            div()
                .id("title-middle")
                .relative()
                .flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .child(measure(&title.middle))
                .child(match lead {
                    Some(lead) => Self::title_drag("search-lead", cx).w(lead),
                    None => Self::title_drag("search-lead", cx).flex_1(),
                })
                .child(self.render_search(state, width, window, cx))
                .child(Self::title_drag("search-trail", cx).flex_1())
                .into_any_element(),
        );
        controls.extend(
            self.render_op_center(window, cx)
                .map(IntoElement::into_any_element),
        );
        // The window buttons and the space after them, which the bands above and below the
        // controls leave alone (maximized, the buttons reach up to the screen edge).
        let caption_w = theme::h_ctl() * 3. + theme::s_2() * 2. + theme::s_8();
        let mut bar = div()
            .id("title-row")
            .debug_selector(|| "title-row".to_owned())
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .h(theme::h_bar())
            .child(measure(&title.row))
            .children(Self::title_drag_bands(caption_w, cx));
        for (i, control) in controls.into_iter().enumerate() {
            if i > 0 {
                bar = bar.child(Self::title_drag(("title-gap", i), cx).w(theme::s_2()));
            }
            bar = bar.child(control);
        }
        bar.child(
            Self::title_drag("caption-lead", cx)
                .debug_selector(|| "caption-lead".to_owned())
                .w(theme::s_8()),
        )
        .child(super::view::caption_group(window))
        .child(Self::title_drag("title-end", cx).w(theme::s_8()))
        .into_any_element()
    }

    /// The search box with the pane's commands (it's drawn in the title row, outside the pane).
    fn render_search(
        &mut self,
        state: SearchBox,
        width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("title-search")
            .key_context("Explorer")
            .map(|d| Self::root_actions(d, cx))
            .flex_none()
            .child(self.render_filter_box(state, width, window, cx))
            .into_any_element()
    }

    /// Empty title bar between controls: drags the window and maximizes on double click (the
    /// system handles both through the caption hit test); right click opens the window menu.
    ///
    /// Only ever a sibling of controls: the system asks GPUI about the topmost window control
    /// area under the pointer, so a drag area around a control would take its clicks.
    fn title_drag(
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
                    cx.emit(ExplorerEvent::WindowMenu(e.position));
                    cx.stop_propagation();
                }),
            )
    }

    /// Empty title bar above and below the controls (they're shorter than the row), as drag
    /// areas. They stop `right` short of the row's end, where the window buttons sit.
    fn title_drag_bands(right: Pixels, cx: &mut Context<Self>) -> [AnyElement; 2] {
        let band = (theme::h_bar() - theme::h_ctl()) / 2.;
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

    // ───────────────────────── Pane row ─────────────────────────

    /// The pane's first row: Up, the path bar, the filter chip, the item count or the selection's
    /// actions, and the view buttons.
    pub(super) fn render_pane_row(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let can_up = self
            .display_path()
            .and_then(arke_core::path::parent)
            .is_some();
        // How the row fits (#388, #474, #528), decided once: the path bar collapses levels only
        // when the row says so.
        let selecting = !self.search_mode && !self.selection.is_empty();
        let fold = self.row_layout(selecting, window, cx);
        self.row_fold.row = Some(fold);
        // A narrow path bar (as laid out on the last frame) keeps only the current folder.
        let slot = self.path_slot.get();
        let compact = fold.collapse && slot > px(0.) && slot < path_compact_below();
        // The path bar is a stop of its own: built before the row's buttons are recorded.
        let path = self.render_path_slot(compact, window, cx);
        // The row's buttons are recorded as they are built (see `explorer_toolbar.rs`).
        let held = self.toolbar.focus.is_focused(window);
        self.toolbar
            .begin(held, held && window.last_input_was_keyboard());
        // Up, the filter chip, the folder's or the selection's actions, the view buttons.
        let up = super::ghost_button(
            "up",
            Icon::ArrowUp,
            can_up,
            t!("nav.up"),
            cx.listener(|this, _: &ClickEvent, w, cx| this.go_up(&GoUp, w, cx)),
        )
        .into_any_element();
        let chip = self.render_filter_chip(cx);
        let info = match fold.selection {
            Some(layout) if selecting => self.render_selection_actions(layout, cx),
            _ => self.render_folder_actions(self.compact_bar(), !fold.count_hidden(), cx),
        };
        let view = self
            .render_view_controls(&fold, window, cx)
            .into_any_element();
        if self.toolbar.end() {
            window.request_animation_frame();
        }
        let this = cx.entity().downgrade();
        div()
            .id("pane-row")
            .debug_selector(|| "pane-row".to_owned())
            .a11y(gpui::Role::Toolbar, t!("a11y.toolbar"))
            // Only while F6 put the keyboard here, so a click leaves it where it was.
            .when(held, |d| {
                d.key_context("Toolbar")
                    .track_focus(&self.toolbar.focus)
                    .on_key_down(cx.listener(Self::toolbar_key))
                    .on_click(move |event, window, cx| {
                        Self::toolbar_press(&this, event, window, cx);
                    })
            })
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_4())
            .h(theme::h_ctx())
            .pl(theme::s_8())
            .pr(theme::s_12())
            .child(up)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(if self.pane.split {
                        path_min_split()
                    } else {
                        path_min()
                    })
                    .h_full()
                    .child(path),
            )
            .children(chip)
            .child(info)
            .child(super::view::vertical_divider())
            .child(view)
            .into_any_element()
    }

    /// While the pane is filtered: its query, which puts it back in the search box (making the
    /// pane active) when clicked, and × to clear it.
    fn render_filter_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.query.is_empty() {
            return None;
        }
        let query = SharedString::from(self.query.clone());
        Some(
            div()
                .id("filter-chip")
                .debug_selector(|| "filter-chip".to_owned())
                // Before its × (the row's buttons go in the order they are built).
                .toolbar_button(
                    "filter-chip",
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        if this.pane.split && !this.pane.active {
                            cx.emit(ExplorerEvent::Activated);
                        }
                        this.focus_filter_input(window, cx);
                    }),
                )
                .a11y_button(t!("filter.chip_tip"))
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::s_4())
                .h(theme::s_24())
                .max_w(filter_chip_max())
                .pl(theme::s_6())
                .pr(theme::s_2())
                .rounded(theme::R_FULL)
                // Solid, so a filtered pane stands out at a glance, beside the other side of a
                // split too (#216).
                .bg(theme::signal())
                .text_color(theme::on_signal())
                .font_weight(theme::FW_STRONG)
                .text_size(theme::fs_12())
                .cursor_pointer()
                .tooltip_below(query.clone())
                .child(
                    svg()
                        .path(Icon::Search.path())
                        .size(theme::icon_sm())
                        .flex_none()
                        .text_color(theme::on_signal()),
                )
                .child(div().min_w_0().truncate().child(query))
                .child(
                    div()
                        .id("filter-chip-clear")
                        .debug_selector(|| "filter-chip-clear".to_owned())
                        .a11y_button(t!("a11y.clear_filter"))
                        .size(theme::s_20())
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(theme::R_FULL)
                        .hover(|s| s.bg(gpui::black().opacity(0.18)))
                        .child(
                            svg()
                                .path(Icon::Close.path())
                                .size(theme::icon_sm())
                                .text_color(theme::on_signal()),
                        )
                        .toolbar_button(
                            "filter-chip-clear",
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.reset_filter(cx);
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    // ───────────────────────── The search box holding a query ─────────────────────────

    /// Presses on the search box while it isn't being typed in. Just the magnifier (`collapsed`):
    /// focus on the press, so typing right away goes into the box. Holding a query (`summary`):
    /// the caret goes where it was pressed, as in the box being typed in, and a right-click gives
    /// its edit menu.
    pub(super) fn search_box_presses(
        el: gpui::Stateful<gpui::Div>,
        collapsed: bool,
        summary: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                // A press in the box already being typed in only moves the caret; the click
                // must not select everything over it.
                let typing = this
                    .filter_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window);
                this.top_row.search_focused_on_press = collapsed || summary || typing;
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
        let offset = text_offset(&text, at.x - left, theme::fs_14(), window);
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
