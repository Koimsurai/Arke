//! Moving a tab to another window. A tab dragged out of the tab list (or strip) and dropped
//! outside the window opens in a new window there; dropped on another Arke window, it moves into
//! that one: between the tabs under the cursor over its tabs, at the end anywhere else. Single
//! window mode doesn't apply: dragging a tab out is asking for a window.
//!
//! - GPUI windows don't share a drag. The window the drag started in keeps the mouse captured while
//!   the button is down, so the release arrives there, at a position outside it.
//! - A pane lives in entities tied to its window, so the tab is rebuilt in the other window from
//!   its folders and back/forward history; the old one closes without going to "Reopen closed tab".
//! - Each window keeps where its tabs were drawn ([`TabLayoutCache`]), so a tab dropped on it from
//!   another window can go between the tabs under the cursor.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use arke_core::History;
use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, IntoElement, MouseButton, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Styled, Window, WindowHandle, canvas, point, px,
};

use super::Workspace;
use crate::explorer::Explorer;

/// A tab on its way to another window: each pane's folder and history, and how the tab was set up.
pub(crate) struct MovedTab {
    panes: Vec<(PathBuf, History)>,
    active_pane: usize,
    ratio: f32,
    pinned: bool,
    synced: bool,
    home: bool,
}

impl MovedTab {
    /// The folder of the first pane, which a new window opens on.
    pub(crate) fn first_folder(&self) -> Option<&Path> {
        self.panes.first().map(|(path, _)| path.as_path())
    }
}

/// The parts of the window's tab area, recorded where they are drawn.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AreaPart {
    /// The "Tabs" heading of the vertical list.
    ListHead,
    /// The empty space under the last tab of the vertical list.
    ListEnd,
    /// The horizontal tab strip.
    Strip,
}

/// Where each tab and the tab area were drawn in the last frame, in window coordinates.
#[derive(Default)]
pub(super) struct TabLayoutCache {
    slots: HashMap<u64, Bounds<Pixels>>,
    area: HashMap<AreaPart, Bounds<Pixels>>,
}

impl TabLayoutCache {
    /// Forgets the last frame; the next one records what it draws.
    pub(super) fn clear(&mut self) {
        self.slots.clear();
        self.area.clear();
    }

    /// The tab area: the strip, or the vertical list from its heading to the bottom.
    pub(super) fn area(&self) -> Option<Bounds<Pixels>> {
        self.area.values().copied().reduce(|a, b| a.union(&b))
    }
}

/// A zero-size element that records its parent's bounds (the parent must be `relative`).
fn recorder(
    cache: &Rc<RefCell<TabLayoutCache>>,
    put: impl Fn(&mut TabLayoutCache, Bounds<Pixels>) + 'static,
) -> AnyElement {
    let cache = Rc::clone(cache);
    canvas(
        move |bounds, _, _| put(&mut cache.borrow_mut(), bounds),
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

/// Where a tab dropped at `at` goes among tabs drawn at `slots` (in tab order): before the first
/// one whose middle is past the cursor.
fn insert_index(slots: &[Bounds<Pixels>], at: Point<Pixels>, horizontal: bool) -> usize {
    slots
        .iter()
        .filter(|b| {
            if horizontal {
                at.x > b.center().x
            } else {
                at.y > b.center().y
            }
        })
        .count()
}

/// Where a window torn off at `screen` (the cursor) goes: `grab` is where its first tab will be
/// inside it, kept under the cursor, as long as the whole window stays on `screen_bounds`. Near
/// an edge it moves in instead: a window whose middle is off every screen is opened by GPUI at the
/// size of the whole screen.
fn torn_window_origin(
    screen: Point<Pixels>,
    grab: Point<Pixels>,
    size: gpui::Size<Pixels>,
    screen_bounds: Option<Bounds<Pixels>>,
) -> Point<Pixels> {
    let origin = screen - grab;
    let Some(vis) = screen_bounds else {
        return origin;
    };
    // Bigger than the screen: its top-left corner stays on it.
    let right = (vis.right() - size.width).max(vis.left());
    let bottom = (vis.bottom() - size.height).max(vis.top());
    point(
        origin.x.clamp(vis.left(), right),
        origin.y.clamp(vis.top(), bottom),
    )
}

impl Workspace {
    /// Records where tab `id` is drawn (inside its slot, which is `relative`).
    pub(super) fn tab_slot_recorder(&self, id: u64) -> AnyElement {
        recorder(&self.tab_layout, move |cache, bounds| {
            cache.slots.insert(id, bounds);
        })
    }

    /// Records where a part of the tab area is drawn (inside it; it must be `relative`).
    pub(super) fn tab_area_recorder(&self, part: AreaPart) -> AnyElement {
        recorder(&self.tab_layout, move |cache, bounds| {
            cache.area.insert(part, bounds);
        })
    }

    /// While a tab is dragged: watches for it being released outside the window (or over another
    /// program's window on top of this one), where nothing else sees the release.
    pub(super) fn render_tab_drop_watch(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.dragging_tab?;
        let workspace = cx.entity().downgrade();
        let shell = Arc::clone(&self.services.shell);
        Some(
            canvas(
                |_, _, _| (),
                move |_, (), window, _| {
                    // The mouse stays captured by this window during the drag, so moves past its
                    // edge arrive here too: past it, the tab is drawn beside the pointer.
                    let mover = workspace.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        let size = window.viewport_size();
                        let outside = e.position.x < px(0.)
                            || e.position.y < px(0.)
                            || e.position.x >= size.width
                            || e.position.y >= size.height;
                        let _ = mover.update(cx, |ws, cx| {
                            ws.tab_ghost_moved(outside, e.position, window, cx);
                        });
                    });
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                        // A cancelled press (`press_guard.rs`) is dispatched outside the window,
                        // but it was released nowhere: the tab stays where it is.
                        if phase != DispatchPhase::Bubble
                            || e.button != MouseButton::Left
                            || crate::press_guard::is_cancel(e)
                        {
                            return;
                        }
                        let size = window.viewport_size();
                        let outside = e.position.x < px(0.)
                            || e.position.y < px(0.)
                            || e.position.x >= size.width
                            || e.position.y >= size.height;
                        if !outside && !shell.covered_at_cursor(Explorer::owner_from(window)) {
                            return;
                        }
                        let screen = window.bounds().origin + e.position;
                        let _ =
                            workspace.update(cx, |ws, cx| ws.drop_tab_outside(screen, window, cx));
                    });
                },
            )
            .absolute()
            .size_0()
            .into_any_element(),
        )
    }

    /// The dragged tab was released at `screen` (screen coordinates), outside this window: it moves
    /// into the Arke window there, or into a new window of its own. The only tab of a window
    /// dropped on no other Arke window stays: there is nothing to tear off.
    pub(super) fn drop_tab_outside(
        &mut self,
        screen: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab_ghost(cx);
        let Some(from) = self.dragging_tab.take() else {
            return;
        };
        cx.notify();
        let Some(id) = self.tabs.get(from).map(|t| t.id) else {
            return;
        };
        let target = self.arke_window_at(screen, window, cx);
        if target.is_none() && self.tabs.len() == 1 {
            return;
        }
        let Some(moved) = self.moved_tab(from, cx) else {
            return;
        };
        let bounds = target
            .is_none()
            .then(|| self.torn_window_bounds(screen, window, cx));
        let services = self.services.clone();
        let source = window.window_handle().downcast::<Self>();
        // Windows open and close outside this window's event handling.
        cx.defer(move |cx| {
            let landed = match (target, bounds) {
                (Some((handle, local)), _) => handle
                    .update(cx, |ws, window, cx| {
                        let at = ws.drop_index(local, cx);
                        ws.adopt_tab(moved, at, window, cx);
                        window.activate_window();
                    })
                    .is_ok(),
                (None, Some(bounds)) => {
                    crate::open_moved_tab_window(cx, &services, moved, bounds).is_some()
                }
                (None, None) => false,
            };
            if landed && let Some(source) = source {
                let _ = source.update(cx, |ws, window, cx| ws.detach_tab(id, window, cx));
            }
        });
    }

    /// The other Arke window at `screen` and the point in it, if any.
    pub(super) fn arke_window_at(
        &self,
        screen: Point<Pixels>,
        window: &Window,
        cx: &mut App,
    ) -> Option<(WindowHandle<Self>, Point<Pixels>)> {
        let me = window.window_handle().window_id();
        let others: Vec<WindowHandle<Self>> = cx
            .windows()
            .into_iter()
            .filter_map(|w| w.downcast::<Self>())
            .filter(|w| w.window_id() != me)
            .collect();
        // The window Windows says is under the cursor: right with overlapping windows and with
        // screens at different scales.
        if let Some((hwnd, (x, y))) = self.services.shell.window_at_cursor() {
            return others.into_iter().find_map(|w| {
                let local = w
                    .update(cx, |_, window, _| {
                        (Explorer::owner_from(window) == Some(hwnd)).then(|| {
                            let scale = window.scale_factor();
                            point(px(x as f32 / scale), px(y as f32 / scale))
                        })
                    })
                    .ok()
                    .flatten()?;
                Some((w, local))
            });
        }
        others.into_iter().find_map(|w| {
            let local = w
                .update(cx, |_, window, _| {
                    let bounds = window.bounds();
                    bounds.contains(&screen).then(|| screen - bounds.origin)
                })
                .ok()
                .flatten()?;
            Some((w, local))
        })
    }

    /// Where a window for a tab torn off at `screen` opens: this window's size, placed so its first
    /// tab is under the cursor.
    fn torn_window_bounds(
        &self,
        screen: Point<Pixels>,
        window: &Window,
        cx: &App,
    ) -> Bounds<Pixels> {
        let size = window.window_bounds().get_bounds().size;
        let grab = self
            .tabs
            .first()
            .and_then(|t| self.tab_layout.borrow().slots.get(&t.id).copied())
            .map_or_else(|| point(px(120.), px(60.)), |b| b.center())
            // Tabs on top, scrolled sideways: where the first tab is in the new window, unscrolled.
            - point(self.sidebar_ui.strip_scroll.offset().x, px(0.));
        let screen_bounds = cx
            .displays()
            .iter()
            .find(|d| d.bounds().contains(&screen))
            .map(|d| d.visible_bounds())
            .or_else(|| cx.primary_display().map(|d| d.visible_bounds()));
        Bounds::new(torn_window_origin(screen, grab, size, screen_bounds), size)
    }

    /// Where a tab dropped at `local` from another window goes: between the tabs under it, or at
    /// the end when it isn't over the tabs.
    pub(super) fn drop_index(&self, local: Point<Pixels>, cx: &App) -> usize {
        let layout = self.tab_layout.borrow();
        if !layout.area().is_some_and(|a| a.contains(&local)) {
            return self.tabs.len();
        }
        let slots: Vec<Bounds<Pixels>> = self
            .tabs
            .iter()
            .filter_map(|t| layout.slots.get(&t.id).copied())
            .collect();
        if slots.len() != self.tabs.len() {
            return self.tabs.len();
        }
        insert_index(&slots, local, Self::horizontal_tabs(cx))
    }

    /// Tab `ix` as it would move to another window. `None` when no pane shows a folder.
    fn moved_tab(&self, ix: usize, cx: &App) -> Option<MovedTab> {
        let tab = self.tabs.get(ix)?;
        let panes: Vec<(PathBuf, History)> = tab
            .panes
            .iter()
            .filter_map(|p| {
                let e = p.view.read(cx);
                Some((e.display_path()?.to_path_buf(), e.history.clone()))
            })
            .collect();
        let home_tab = self.home_page.as_ref().and_then(|p| self.tab_of(&p.owner));
        (!panes.is_empty()).then(|| MovedTab {
            active_pane: tab.active_pane.min(panes.len() - 1),
            ratio: tab.ratio,
            pinned: tab.pinned,
            synced: tab.synced && panes.len() > 1,
            home: home_tab == Some(ix),
            panes,
        })
    }

    /// A tab from another window arrives at `at` (pinned ones at the end of the pinned group) and
    /// becomes the active one.
    pub(super) fn adopt_tab(
        &mut self,
        moved: MovedTab,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first) = moved.first_folder().map(Path::to_path_buf) else {
            return;
        };
        let at = if moved.pinned {
            at.min(self.pinned_count())
        } else {
            at
        };
        let ix = self.insert_tab(first, at, true, window, cx);
        let id = self.tabs[ix].id;
        if moved.pinned {
            self.tabs[ix].pinned = true;
            self.keep_pins_first();
        }
        if let Some(ix) = self.tabs.iter().position(|t| t.id == id) {
            self.apply_moved(ix, moved, window, cx);
        }
    }

    /// A new window opened for a torn-off tab takes it as its first tab (already on its folder).
    pub(crate) fn adopt_first(
        &mut self,
        moved: MovedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.first_mut() {
            tab.pinned = moved.pinned;
        }
        self.apply_moved(0, moved, window, cx);
    }

    /// Gives tab `ix` (open on the moved tab's first folder) the rest of the moved tab: the other
    /// side of a split, each side's history, and Home on top if it was there.
    fn apply_moved(
        &mut self,
        ix: usize,
        moved: MovedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut panes = moved.panes.into_iter();
        let Some((_, first)) = panes.next() else {
            return;
        };
        let second = panes.next();
        if let Some((path, _)) = &second {
            self.split(ix, path.clone(), window, cx);
        }
        let histories = std::iter::once(first).chain(second.map(|(_, h)| h));
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        for (pane, history) in tab.panes.iter().zip(histories) {
            pane.view.update(cx, |e, cx| {
                e.history = history;
                cx.notify();
            });
        }
        tab.active_pane = moved.active_pane.min(tab.panes.len() - 1);
        tab.ratio = moved.ratio.clamp(0.2, 0.8);
        tab.synced = moved.synced && tab.is_split();
        self.active = ix;
        if moved.home {
            self.open_home_as(false, false, window, cx);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Tab `id` moved to another window: it closes here without going to "Reopen closed tab". The
    /// window closes with its last tab.
    fn detach_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        if self.tabs.len() == 1 {
            window.remove_window();
            return;
        }
        self.tabs.remove(ix);
        if self.active > ix || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.focus_active(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
impl Workspace {
    /// Where tab `ix` was drawn in the last frame.
    pub(crate) fn tab_slot_for_test(&self, ix: usize) -> Option<Bounds<Pixels>> {
        let id = self.tabs.get(ix)?.id;
        self.tab_layout.borrow().slots.get(&id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{insert_index, torn_window_origin};
    use gpui::{Bounds, point, px, size};

    fn slot(x: f32, y: f32, w: f32, h: f32) -> Bounds<gpui::Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn a_dropped_tab_goes_before_the_first_tab_past_the_cursor() {
        // Vertical: three rows 30 tall, 2 apart.
        let rows = [
            slot(0., 0., 200., 30.),
            slot(0., 32., 200., 30.),
            slot(0., 64., 200., 30.),
        ];
        assert_eq!(insert_index(&rows, point(px(50.), px(5.)), false), 0);
        assert_eq!(insert_index(&rows, point(px(50.), px(20.)), false), 1);
        assert_eq!(insert_index(&rows, point(px(50.), px(50.)), false), 2);
        assert_eq!(insert_index(&rows, point(px(50.), px(300.)), false), 3);
        // Horizontal: the x position counts.
        let strip = [slot(0., 0., 100., 30.), slot(104., 0., 100., 30.)];
        assert_eq!(insert_index(&strip, point(px(120.), px(10.)), true), 1);
        assert_eq!(insert_index(&strip, point(px(170.), px(10.)), true), 2);
    }

    #[test]
    fn a_torn_off_window_keeps_its_first_tab_under_the_cursor_and_stays_on_screen() {
        let screen = slot(0., 0., 1920., 1040.);
        let window = size(px(1200.), px(800.));
        let at = |cursor: (f32, f32), grab: (f32, f32)| {
            let origin = torn_window_origin(
                point(px(cursor.0), px(cursor.1)),
                point(px(grab.0), px(grab.1)),
                window,
                Some(screen),
            );
            // GPUI opens a window whose middle is off every screen at the whole screen's size.
            assert!(screen.contains(&Bounds::new(origin, window).center()));
            origin
        };
        assert_eq!(at((700., 200.), (100., 60.)), point(px(600.), px(140.)));
        // Near the right and bottom edges: moved in, whole on the screen.
        assert_eq!(at((1919., 1039.), (10., 10.)), point(px(720.), px(240.)));
        assert_eq!(at((1500., 300.), (100., 400.)), point(px(720.), px(0.)));
        // Above the top and past the left: the corner stays on the screen.
        assert_eq!(at((50., 5.), (100., 60.)), point(px(0.), px(0.)));
        // Bigger than the screen: from its top-left corner.
        let huge = torn_window_origin(
            point(px(900.), px(500.)),
            point(px(100.), px(60.)),
            size(px(2400.), px(1400.)),
            Some(screen),
        );
        assert_eq!(huge, point(px(0.), px(0.)));
    }
}
