//! The dividers that are dragged to size things: the sidebar's edge, the line between its places
//! and its tabs, the split view's gutter, the Details column dividers, the column view's divider,
//! and the preview pane's edge and the boundary inside it.
//!
//! Each owner keeps what its drag means: where it starts from, the size it sets, the limits, what
//! a double-click resets and what is saved when it ends. [`Splitter`] does what they share, the
//! same way for all of them:
//! - the whole hit area lights the line up under the pointer, not only the thin line itself. The
//!   hover is the hit area's own (the same element and hitbox that take the press): it sets a
//!   text color under the pointer, which the line reads while it is painted, and GPUI draws the
//!   hit area again when the pointer enters or leaves it;
//! - a press starts the drag (a double-click resets instead) and goes no further;
//! - moves report the pointer, and the cursor stays the resize cursor all over the window;
//! - the release ends it wherever the pointer is. The handle being dragged listens for it in the
//!   capture phase, so an element under the pointer that keeps the release to itself (it stops
//!   it, or covers what is behind it) can't keep the drag going, and a move with the button up
//!   (released outside the window) ends it too. Each end redraws the window.
//!
//! The window-wide drag listeners exist only while a drag is under way, painted by the dragged
//! handle. At rest a divider has its own mouse and hover listeners and nothing else.
//!
//! `RUST_LOG=arke::splitter=debug` logs each press, each end (and which path ended it) and each
//! frame drawn while a drag is under way.

use std::rc::Rc;

use gpui::{
    App, CursorStyle, DispatchPhase, Div, ElementId, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Rgba, Stateful, Styled, Window,
    canvas, prelude::FluentBuilder, px,
};

use crate::theme;

/// Log target of the dividers' debug lines.
const LOG: &str = "arke::splitter";

/// The end of a drag, passed to [`Splitter::on_end`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct DragEnd;

/// How the line shows that it can be taken, and that it is being dragged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Look {
    /// In the accent: faint under the pointer, full while dragged.
    #[default]
    Accent,
    /// Darker gray under the pointer and as it is while dragged (the Details column dividers,
    /// small marks between the column titles).
    Subtle,
}

type Handler<E> = Rc<dyn Fn(&E, &mut Window, &mut App)>;

/// A divider that is dragged: see the module docs. Built with the owner's callbacks
/// (`cx.listener(..)`), then [`Splitter::render`]ed from the hit area and the line the owner
/// lays out.
pub(crate) struct Splitter {
    id: ElementId,
    cursor: CursorStyle,
    look: Look,
    round: bool,
    dragging: bool,
    on_start: Option<Handler<MouseDownEvent>>,
    on_double_click: Option<Handler<MouseDownEvent>>,
    on_drag: Option<Handler<MouseMoveEvent>>,
    on_end: Option<Handler<DragEnd>>,
}

impl Splitter {
    /// A divider with this element id, showing `cursor` (column or row resize) over its hit area
    /// and all over the window while dragged.
    pub(crate) fn new(id: impl Into<ElementId>, cursor: CursorStyle) -> Self {
        Self {
            id: id.into(),
            cursor,
            look: Look::Accent,
            round: false,
            dragging: false,
            on_start: None,
            on_double_click: None,
            on_drag: None,
            on_end: None,
        }
    }

    /// How the line looks under the pointer and while dragged ([`Look::Accent`] if not set).
    #[must_use]
    pub(crate) const fn look(mut self, look: Look) -> Self {
        self.look = look;
        self
    }

    /// Whether the hover and drag colors have round ends, like a line drawn with
    /// `rounded(R_FULL)`.
    #[must_use]
    pub(crate) const fn round(mut self, round: bool) -> Self {
        self.round = round;
        self
    }

    /// Whether the owner has a drag of this divider under way (started by [`Self::on_start`],
    /// not yet ended by [`Self::on_end`]). Only then does the handle listen to the whole window.
    #[must_use]
    pub(crate) const fn dragging(mut self, dragging: bool) -> Self {
        self.dragging = dragging;
        self
    }

    /// A press that starts a drag (not the second press of a double-click). The owner notes where
    /// it starts and that it is dragging.
    #[must_use]
    pub(crate) fn on_start(
        mut self,
        f: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_start = Some(Rc::new(f));
        self
    }

    /// The second press of a double-click (2nd, 4th... as Windows counts them): reset or fit. It
    /// starts no drag.
    #[must_use]
    pub(crate) fn on_double_click(
        mut self,
        f: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_double_click = Some(Rc::new(f));
        self
    }

    /// The pointer moved during the drag, with the button still down.
    #[must_use]
    pub(crate) fn on_drag(
        mut self,
        f: impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_drag = Some(Rc::new(f));
        self
    }

    /// The button was released, anywhere (or found up on a move). It can come more than once for
    /// one drag, and for a release over the handle with no drag under way (one that came before
    /// the handle could listen to the window): the owner ends its drag if it has one and ignores
    /// it otherwise.
    #[must_use]
    pub(crate) fn on_end(mut self, f: impl Fn(&DragEnd, &mut Window, &mut App) + 'static) -> Self {
        self.on_end = Some(Rc::new(f));
        self
    }

    /// The divider: `hit`, the area that takes the pointer (laid out by the owner), around
    /// `line`, what is drawn (with its color at rest, if it has one). The line takes the hover and
    /// drag colors of the [`Look`], painted over it.
    pub(crate) fn render(self, hit: Div, line: Div) -> Stateful<Div> {
        let Self {
            id,
            cursor,
            look,
            round,
            dragging,
            on_start,
            on_double_click,
            on_drag,
            on_end,
        } = self;
        let (hover, active) = match look {
            Look::Accent => (theme::signal_line(), Some(theme::signal())),
            Look::Subtle => (theme::text_subtle(), None),
        };
        if dragging {
            log::debug!(target: LOG, "{id}: drawn with a drag under way");
        }
        let line = line.child(line_color(LineColor {
            dragging,
            hover,
            active,
            round,
            #[cfg(test)]
            name: id.to_string(),
        }));
        let released = on_end.clone();
        let (down_id, up_id) = (id.clone(), id.clone());
        hit.debug_selector(|| id.to_string())
            .id(id.clone())
            .cursor(cursor)
            // The hover signal the line reads (see `line_color`): no text is drawn in here.
            .text_color(gpui::transparent_black())
            .when(!dragging, |d| d.hover(|s| s.text_color(HOVERED)))
            .on_mouse_down(MouseButton::Left, move |e, window, cx| {
                cx.stop_propagation();
                let double = crate::press_guard::is_double_click(e.click_count);
                log::debug!(
                    target: LOG,
                    "{down_id}: press ({} clicks): {}",
                    e.click_count,
                    if double { "double-click" } else { "drag starts" }
                );
                let handler = if double { &on_double_click } else { &on_start };
                if let Some(f) = handler {
                    f(e, window, cx);
                }
                // Drawn again now, so the handle listens to the window from the next event on.
                window.refresh();
            })
            // A quick click can be released before the frame that would listen to the window.
            .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                if let Some(f) = &released {
                    log::debug!(target: LOG, "{up_id}: release over the handle: end");
                    f(&DragEnd, window, cx);
                    window.refresh();
                }
            })
            .child(line)
            .when(dragging, |d| {
                d.child(drag_listeners(id, cursor, on_drag, on_end))
            })
    }
}

/// The text color the hit area takes under the pointer, as the line's hover signal. Any color
/// with some alpha would do: only whether it is transparent is read.
const HOVERED: gpui::Hsla = gpui::Hsla {
    h: 0.,
    s: 0.,
    l: 0.,
    a: 1.,
};

/// What the line shows over its color at rest.
struct LineColor {
    dragging: bool,
    hover: Rgba,
    active: Option<Rgba>,
    round: bool,
    /// Tests: the divider's element id, under which the painted color is noted.
    #[cfg(test)]
    name: String,
}

/// Paints the line's hover or drag color over the whole line (none at rest). Under the pointer
/// the hit area's text color is [`HOVERED`] instead of transparent; GPUI works that out from the
/// hit area's own hitbox, the one its press listener checks too.
fn line_color(look: LineColor) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let hovered = window.text_style().color.a > 0.;
            let color = if look.dragging {
                look.active
            } else if hovered {
                Some(look.hover)
            } else {
                None
            };
            #[cfg(test)]
            PAINTED.with_borrow_mut(|painted| {
                painted.insert(look.name.clone(), color);
            });
            if let Some(color) = color {
                let radius = if look.round {
                    bounds.size.width.min(bounds.size.height) / 2.
                } else {
                    px(0.)
                };
                window.paint_quad(gpui::fill(bounds, color).corner_radii(radius));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// While a drag is under way: the resize cursor all over the window, and the moves and the
/// release wherever they happen, heard before any element can stop them.
fn drag_listeners(
    id: ElementId,
    cursor: CursorStyle,
    on_drag: Option<Handler<MouseMoveEvent>>,
    on_end: Option<Handler<DragEnd>>,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            window.set_window_cursor_style(cursor);
            let released = on_end.clone();
            let up_id = id.clone();
            window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                log::debug!(
                    target: LOG,
                    "{up_id}: {:?} release at {:?} ({} clicks) heard by the window: {}",
                    e.button,
                    e.position,
                    e.click_count,
                    if e.button == MouseButton::Left { "end" } else { "ignored" }
                );
                if e.button == MouseButton::Left
                    && let Some(f) = &released
                {
                    f(&DragEnd, window, cx);
                    window.refresh();
                }
            });
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                if e.pressed_button == Some(MouseButton::Left) {
                    if let Some(f) = &on_drag {
                        f(e, window, cx);
                    }
                } else if let Some(f) = &on_end {
                    // Released where the window didn't see it.
                    log::debug!(target: LOG, "{id}: move with the button up: end");
                    f(&DragEnd, window, cx);
                    window.refresh();
                }
            });
        },
    )
    .absolute()
    .size_0()
}

#[cfg(test)]
thread_local! {
    /// Tests: the color each divider's line was last painted with (`None`: its color at rest).
    static PAINTED: std::cell::RefCell<std::collections::HashMap<String, Option<Rgba>>> =
        std::cell::RefCell::default();
}

/// Tests: the color the divider with element id `id` last painted over its line: `Some(None)`
/// for none (its color at rest), `None` if it hasn't been painted.
#[cfg(test)]
#[allow(
    clippy::option_option,
    reason = "tests tell \"not drawn\" apart from \"drawn with no color\""
)]
pub(crate) fn painted_for_test(id: &str) -> Option<Option<Rgba>> {
    PAINTED.with_borrow(|painted| painted.get(id).copied())
}

/// The color [`painted_for_test`] gives while the pointer is over a divider of `look`.
#[cfg(test)]
pub(crate) fn hover_color_for_test(look: Look) -> Rgba {
    match look {
        Look::Accent => theme::signal_line(),
        Look::Subtle => theme::text_subtle(),
    }
}
