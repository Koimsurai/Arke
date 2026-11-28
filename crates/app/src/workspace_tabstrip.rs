//! Tabs on top when they don't all fit (#533, #541): how narrow a tab gets before the strip
//! scrolls, the close button that only takes room from the title where it shows, and the scroll
//! buttons at the ends of the strip that show there are more tabs and step to them.
//!
//! Everything here reads what the strip's scroll handle already holds from the last layout (its
//! bounds, offset, largest offset and the tabs' places): nothing is measured on its own.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Pixels, Point, ScrollHandle, Window, canvas, div, point,
    prelude::*, px, svg,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;

/// The narrowest an unpinned tab on top gets (logical pixels at 100%) before the strip scrolls
/// instead. It leaves 122 px of title on an inactive tab and 98 px on the active or hovered one
/// (its close button takes the rest): about 7 and 6 CJK characters at the 14 px UI size, about 16
/// and 12 Latin ones, so tabs on similar folders still read apart.
pub(super) const TAB_MIN_W: f32 = 160.;

/// What rounding may leave of an offset at either end.
const EDGE: Pixels = px(0.5);

/// The room the close button takes at the end of a tab when it shows: the button and the gap
/// before it.
pub(super) fn close_room() -> Pixels {
    theme::s_20() + theme::s_4()
}

/// Whether the tabs overflow the strip and which ways it can scroll, as last laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StripEdges {
    /// The tabs are wider than the strip: the scroll buttons show.
    pub(super) overflow: bool,
    /// Tabs are cut off at the start (the back button works).
    pub(super) back: bool,
    /// Tabs are cut off at the end (the forward button works).
    pub(super) forward: bool,
}

impl StripEdges {
    /// From the strip's scroll handle.
    pub(super) fn of(scroll: &ScrollHandle) -> Self {
        let max = scroll.max_offset().x;
        let x = scroll.offset().x;
        let overflow = max > EDGE;
        Self {
            overflow,
            back: overflow && x < -EDGE,
            forward: overflow && x > -max + EDGE,
        }
    }
}

/// Where a press on a scroll button moves the strip's offset: the next tab cut off at that end
/// comes fully into view, about one tab's width. `view` is the strip's visible left and right
/// edge, `items` each tab's left and right edge unscrolled, `x` the offset now (0 or less) and
/// `max` the largest scroll.
fn step_to(
    view: (Pixels, Pixels),
    items: &[(Pixels, Pixels)],
    x: Pixels,
    max: Pixels,
    forward: bool,
) -> Pixels {
    let target = if forward {
        items
            .iter()
            .find(|(_, right)| *right + x > view.1 + EDGE)
            .map_or(-max, |(_, right)| view.1 - *right)
    } else {
        items
            .iter()
            .rev()
            .find(|(left, _)| *left + x < view.0 - EDGE)
            .map_or(px(0.), |(left, _)| view.0 - *left)
    };
    target.clamp(-max, px(0.))
}

/// The tab under `mouse` in the strip, if any: only where the strip shows it, not where it is
/// scrolled out of view.
fn tab_at(scroll: &ScrollHandle, mouse: Point<Pixels>) -> Option<usize> {
    if !scroll.bounds().contains(&mouse) {
        return None;
    }
    let at = point(mouse.x - scroll.offset().x, mouse.y);
    (0..scroll.children_count()).find(|&ix| {
        scroll
            .bounds_for_item(ix)
            .is_some_and(|bounds| bounds.contains(&at))
    })
}

/// The tab the pointer rests on, as hover styles count it: not while something is dragged or
/// after a key press.
fn hovered_tab(scroll: &ScrollHandle, window: &Window, cx: &App) -> Option<usize> {
    if cx.has_active_drag() || window.last_input_was_keyboard() {
        return None;
    }
    tab_at(scroll, window.mouse_position())
}

impl Workspace {
    /// A press on the scroll button at one end of the strip.
    fn scroll_strip(&mut self, forward: bool, cx: &mut Context<Self>) {
        let scroll = &self.sidebar_ui.strip_scroll;
        let view = scroll.bounds();
        let items: Vec<(Pixels, Pixels)> = (0..scroll.children_count())
            .filter_map(|ix| scroll.bounds_for_item(ix))
            .map(|b| (b.left(), b.right()))
            .collect();
        let offset = scroll.offset();
        let x = step_to(
            (view.left(), view.right()),
            &items,
            offset.x,
            scroll.max_offset().x,
            forward,
        );
        if x != offset.x {
            scroll.set_offset(point(x, offset.y));
            cx.notify();
        }
    }

    /// The scroll button at the start (`forward` false) or the end of the strip, drawn while the
    /// tabs overflow; dimmed once the strip is scrolled all the way that way (#541).
    pub(super) fn strip_scroll_button(
        forward: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, label) = if forward {
            ("strip-scroll-right", t!("sidebar.scroll_tabs_right"))
        } else {
            ("strip-scroll-left", t!("sidebar.scroll_tabs_left"))
        };
        div()
            .id(id)
            .debug_selector(|| id.to_owned())
            .a11y_button(label)
            .a11y_disabled(!enabled)
            .w(theme::s_24())
            .h(theme::h_ctl())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_8())
            .when(enabled, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(theme::wash_strong()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.scroll_strip(forward, cx);
                    }))
            })
            .child(
                svg()
                    .path(Icon::ChevronRight.path())
                    .size(theme::icon_sm())
                    .when(!forward, |s| {
                        s.with_transformation(gpui::Transformation::rotate(gpui::radians(
                            std::f32::consts::PI,
                        )))
                    })
                    .text_color(if enabled {
                        theme::text_muted()
                    } else {
                        theme::text_disabled()
                    }),
            )
            .into_any_element()
    }

    /// Draws the strip once more when this frame's layout changed what it was drawn from: the
    /// tabs started or stopped overflowing, reached an end (the active tab was scrolled into
    /// view), or another tab is under the pointer. `drawn` is what this frame's buttons were
    /// drawn from. It settles at once: neither depends on what it changes (a hovered tab's title
    /// gives way inside the tab, which keeps its width).
    pub(super) fn strip_watch(&self, drawn: StripEdges) -> AnyElement {
        let scroll = self.sidebar_ui.strip_scroll.clone();
        let hovered = Rc::clone(&self.sidebar_ui.strip_hovered);
        let drawn_hover = hovered.get();
        canvas(
            move |_, window, cx| {
                let hover = hovered_tab(&scroll, window, cx);
                if StripEdges::of(&scroll) != drawn || hover != drawn_hover {
                    hovered.set(hover);
                    window.refresh();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// Whether tab `ix` on top shows its close button: it is the active tab, or the pointer rests
    /// on it.
    pub(super) fn strip_close_shown(&self, ix: usize) -> bool {
        ix == self.active || self.sidebar_ui.strip_hovered.get() == Some(ix)
    }
}

#[cfg(test)]
impl Workspace {
    /// The strip's scroll offset (0 or less).
    pub(crate) fn strip_offset_for_test(&self) -> Pixels {
        self.sidebar_ui.strip_scroll.offset().x
    }

    /// The strip's largest scroll offset.
    pub(crate) fn strip_max_offset_for_test(&self) -> Pixels {
        self.sidebar_ui.strip_scroll.max_offset().x
    }

    /// Whether the strip's back and forward buttons work.
    pub(crate) fn strip_edges_for_test(&self) -> (bool, bool) {
        let edges = StripEdges::of(&self.sidebar_ui.strip_scroll);
        (edges.back, edges.forward)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(count: usize, width: f32) -> Vec<(Pixels, Pixels)> {
        let mut left = 0.;
        (0..count)
            .map(|_| {
                let tab = (px(left), px(left + width));
                left += width;
                tab
            })
            .collect()
    }

    #[test]
    fn forward_brings_the_next_cut_off_tab_into_view() {
        // Ten 100 px tabs in a 350 px strip: 650 px of scroll.
        let items = tabs(10, 100.);
        let view = (px(0.), px(350.));
        let x = step_to(view, &items, px(0.), px(650.), true);
        assert_eq!(
            x,
            px(-50.),
            "the fourth tab, half shown, comes fully into view"
        );
        let x = step_to(view, &items, x, px(650.), true);
        assert_eq!(x, px(-150.), "then one tab further");
    }

    #[test]
    fn back_brings_the_previous_cut_off_tab_into_view() {
        let items = tabs(10, 100.);
        let view = (px(0.), px(350.));
        let x = step_to(view, &items, px(-150.), px(650.), false);
        assert_eq!(
            x,
            px(-100.),
            "the second tab, half shown, comes fully into view"
        );
        let x = step_to(view, &items, x, px(650.), false);
        assert_eq!(x, px(0.), "then the first");
    }

    #[test]
    fn steps_stop_at_the_ends() {
        let items = tabs(10, 100.);
        let view = (px(0.), px(350.));
        assert_eq!(step_to(view, &items, px(0.), px(650.), false), px(0.));
        assert_eq!(step_to(view, &items, px(-650.), px(650.), true), px(-650.));
        // A strip that doesn't start at the window's edge.
        let view = (px(200.), px(550.));
        let items: Vec<_> = items
            .into_iter()
            .map(|(l, r)| (l + px(200.), r + px(200.)))
            .collect();
        assert_eq!(step_to(view, &items, px(0.), px(650.), true), px(-50.));
    }
}
