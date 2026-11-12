//! Tab reordering by drag. Dragging over another tab swaps it immediately and the displaced tabs
//! slide into place, like browsers and Arc.
//!
//! - Each tab reports its bounds during the drag (`on_drag_move`); crossing a tab's midline moves the dragged tab there.
//! - Swaps use FLIP: the new layout applies at once and tabs animate from their old positions.
//!   Hit testing uses the outer bounds without the animation offset, so sliding tabs do not cause flicker.
//! - Once the cursor leaves the tab strip (e.g. to drop on the right half for split view) swapping stops.
//! - Pinning by drag: while an unpinned tab is dragged a "Drop here to pin" zone shows at the top of
//!   the tab list (in place of the New tab row, so nothing shifts) or under the start of the
//!   horizontal strip. A pinned tab dragged past the end of the pinned group is unpinned there.
//!
//! Favorite tiles reorder the same way on their grid: entering another tile moves the dragged one
//! into its slot (saved right away), and the others slide over from their old slots.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnimationExt, AnyElement, Bounds, Context, DragMoveEvent, IntoElement, Pixels, Styled, Window,
    div, prelude::*, px, svg,
};
use rust_i18n::t;

use crate::assets::Icon;
use crate::motion::Slide;
use crate::theme;

use super::sidebar::DraggedPin;
use super::{DraggedTab, Workspace};

/// Stable tab identity, so animations survive reordering.
pub(super) fn next_tab_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Drag state.
#[derive(Default)]
pub(super) struct TabDrag {
    /// Last reported bounds per tab (outer, without animation offset).
    bounds: HashMap<u64, Bounds<Pixels>>,
    /// Tabs currently sliding back to 0 from their starting offset.
    shift: HashMap<u64, Slide<f32>>,
    serial: usize,
    /// Already swapped during this mouse move; blocks further swaps until the next frame.
    moved: bool,
    /// Tab that files are being dragged over (tab id, wait serial); switches to it after a short hover.
    spring: Option<(u64, usize)>,
    /// Favorite tile being dragged (dimmed in its slot).
    tile: Option<PathBuf>,
    /// Last reported tile bounds (outer, without animation offset).
    tile_bounds: HashMap<PathBuf, Bounds<Pixels>>,
    /// Tiles currently sliding back to 0 from their starting offset (x, y).
    tile_shift: HashMap<PathBuf, Slide<(f32, f32)>>,
}

/// Hover time before switching to a tab while dragging files, like Explorer's spring-loaded folders.
const SPRING_DELAY: std::time::Duration = std::time::Duration::from_millis(600);

impl TabDrag {
    /// Called on each frame so the next mouse move may swap again.
    pub(super) const fn frame(&mut self) {
        self.moved = false;
    }

    /// A pin drag starts; `tile` is set when it comes from the tiles.
    pub(super) fn start_tile(&mut self, tile: Option<PathBuf>) {
        self.tile = tile;
        self.tile_shift.clear();
    }

    /// The pin drag ended (dropped or cancelled). Slides still running finish.
    pub(super) fn end_tile(&mut self) {
        self.tile = None;
    }
}

/// Offset each grid item slides back from after a reorder. `slots` are the slot positions, `old`
/// and `new` the item order before and after; an item now in slot j came from its old slot i.
fn grid_offsets<K: PartialEq + Clone>(
    slots: &[(f32, f32)],
    old: &[K],
    new: &[K],
) -> Vec<(K, (f32, f32))> {
    new.iter()
        .zip(slots)
        .filter_map(|(item, &(x, y))| {
            let was = old.iter().position(|o| o == item)?;
            let &(ox, oy) = slots.get(was)?;
            let delta = (ox - x, oy - y);
            (delta.0.abs() > 0.5 || delta.1.abs() > 0.5).then(|| (item.clone(), delta))
        })
        .collect()
}

/// Offset each tab slides back from after a reorder. `old` is (id, start, length) in old order,
/// `new_order` is the new order of ids. Gaps between tabs are taken from the old layout.
fn flip_offsets(old: &[(u64, f32, f32)], new_order: &[u64]) -> Vec<(u64, f32)> {
    let Some(&(_, start, first_len)) = old.first() else {
        return Vec::new();
    };
    let gap = old.get(1).map_or(0., |&(_, o, _)| o - start - first_len);
    let lookup: HashMap<u64, (f32, f32)> = old.iter().map(|&(id, o, l)| (id, (o, l))).collect();
    let mut cursor = start;
    let mut out = Vec::new();
    for id in new_order {
        let Some(&(was, len)) = lookup.get(id) else {
            continue;
        };
        let delta = was - cursor;
        if delta.abs() > 0.5 {
            out.push((*id, delta));
        }
        cursor += len + gap;
    }
    out
}

/// Whether dragging `from` past `over` (start and length along the strip) at `pos` should move it.
/// The cursor has to cross the midline in either direction so touching an edge does not swap.
fn crosses(from: usize, over: usize, pos: f32, start: f32, len: f32) -> bool {
    let mid = start + len / 2.;
    (from < over && pos > mid) || (from > over && pos < mid)
}

impl Workspace {
    /// A tab reports its bounds during a drag. Records them and moves the dragged tab once its midline is crossed.
    fn on_tab_drag_move(&mut self, id: u64, e: &DragMoveEvent<DraggedTab>, cx: &mut Context<Self>) {
        self.tab_drag.bounds.insert(id, e.bounds);
        let Some(from) = self.dragging_tab else {
            return;
        };
        let Some(over) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        if over == from || self.tab_drag.moved {
            return;
        }
        let horizontal = Self::horizontal_tabs(cx);
        let p = e.event.position;
        let b = e.bounds;
        // The cursor must be on the tab strip (within this tab on the cross axis).
        let inside = if horizontal {
            p.y >= b.top() && p.y <= b.bottom()
        } else {
            p.x >= b.left() && p.x <= b.right()
        };
        let (pos, start, len) = if horizontal {
            (p.x, b.left(), b.size.width)
        } else {
            (p.y, b.top(), b.size.height)
        };
        if !inside || !crosses(from, over, pos.into(), start.into(), len.into()) {
            return;
        }
        self.reorder_dragged_tab(from, over, horizontal, cx);
    }

    fn reorder_dragged_tab(
        &mut self,
        from: usize,
        to: usize,
        horizontal: bool,
        cx: &mut Context<Self>,
    ) {
        // A pinned tab dragged onto the tabs below the pinned group leaves the group there.
        // Otherwise pinned tabs only reorder among themselves, the others below them.
        let unpin = self.tabs.get(from).is_some_and(|t| t.pinned)
            && self.tabs.get(to).is_some_and(|t| !t.pinned);
        if unpin {
            self.tabs[from].pinned = false;
            cx.notify();
        }
        let to = self.clamp_move(from, to);
        if to == from {
            return;
        }
        let axis = |b: &Bounds<Pixels>| -> (f32, f32) {
            if horizontal {
                (b.left().into(), b.size.width.into())
            } else {
                (b.top().into(), b.size.height.into())
            }
        };
        // Positions before the swap. Only animate when every tab has reported.
        let old: Option<Vec<(u64, f32, f32)>> = self
            .tabs
            .iter()
            .map(|t| {
                self.tab_drag.bounds.get(&t.id).map(|b| {
                    let (o, l) = axis(b);
                    (t.id, o, l)
                })
            })
            .collect();
        self.move_tab(from, to, cx);
        self.dragging_tab = Some(to);
        self.tab_drag.moved = true;
        if let Some(old) = old.filter(|_| crate::motion::enabled()) {
            let order: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();
            let now = std::time::Instant::now();
            for (id, delta) in flip_offsets(&old, &order) {
                // A tab still sliding from the last swap continues from where it is drawn.
                let left = self.tab_drag.shift.get(&id).map_or(0., |s| s.left(now));
                self.tab_drag.serial += 1;
                self.tab_drag.shift.insert(
                    id,
                    Slide {
                        from: delta + left,
                        started: now,
                        serial: self.tab_drag.serial,
                    },
                );
            }
        }
    }

    /// Dragging files over a tab: start waiting on enter and switch if the cursor (and drag) is still there
    /// when the delay ends. Leaving cancels.
    fn spring_hover(&mut self, id: u64, inside: bool, window: &mut Window, cx: &mut Context<Self>) {
        let waiting = self.tab_drag.spring.map(|(tab, _)| tab);
        if !inside {
            if waiting == Some(id) {
                self.tab_drag.spring = None;
            }
            return;
        }
        if waiting == Some(id) {
            return;
        }
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        if ix == self.active {
            self.tab_drag.spring = None;
            return;
        }
        self.tab_drag.serial += 1;
        let serial = self.tab_drag.serial;
        self.tab_drag.spring = Some((id, serial));
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SPRING_DELAY).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.tab_drag.spring != Some((id, serial)) || !cx.has_active_drag() {
                    return;
                }
                this.tab_drag.spring = None;
                if let Some(ix) = this.tabs.iter().position(|t| t.id == id) {
                    this.activate(ix, window, cx);
                }
            });
        })
        .detach();
    }

    /// While an unpinned tab is being dragged: the drop zone that pins it. The caller places it.
    pub(super) fn pin_drop_zone(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let from = self.dragging_tab?;
        if self.tabs.get(from).is_none_or(|t| t.pinned) {
            return None;
        }
        Some(
            div()
                .id("tab-pin-drop")
                .debug_selector(|| "tab-pin-drop".to_owned())
                .h(theme::H_ROW)
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::S_8)
                .px(theme::S_8)
                .rounded(theme::R_8)
                .border_1()
                .border_dashed()
                .border_color(theme::signal_line())
                .bg(theme::signal_wash())
                .text_color(theme::text())
                .child(
                    svg()
                        .path(Icon::Pin.path())
                        .size(theme::ICON)
                        .flex_none()
                        .text_color(theme::signal()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(t!("sidebar.drop_to_pin_tab").into_owned()),
                )
                .drag_over::<DraggedTab>(|s, _, _, _| s.border_color(theme::signal()))
                .on_drop(cx.listener(|this, dragged: &DraggedTab, _, cx| {
                    this.drop_tab_to_pin(dragged, cx);
                })),
        )
    }

    /// A tab dropped on the pin zone: pins it (at the end of the pinned group).
    fn drop_tab_to_pin(&mut self, dragged: &DraggedTab, cx: &mut Context<Self>) {
        // The tab may already have moved during the drag; use its current index.
        let from = self.dragging_tab.take().unwrap_or(dragged.ix);
        if self.tabs.get(from).is_some_and(|t| !t.pinned) {
            self.toggle_pin(from, cx);
        }
        cx.notify();
    }

    /// Wraps a tab: reports drag bounds, slides recently moved tabs from their old position, and dims
    /// the dragged tab while keeping its slot.
    pub(super) fn tab_drag_shell(
        &self,
        ix: usize,
        row: impl IntoElement + Styled + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self.tabs[ix].id;
        let dragged = self.dragging_tab == Some(ix);
        let row = row.opacity(if dragged { 0.4 } else { 1. });
        let now = std::time::Instant::now();
        let inner = match self
            .tab_drag
            .shift
            .get(&id)
            .copied()
            .filter(|s| s.live(now))
        {
            Some(Slide {
                from: delta,
                serial,
                ..
            }) => {
                let horizontal = Self::horizontal_tabs(cx);
                row.with_animation(
                    ("tab-shift", serial),
                    crate::motion::enter(),
                    move |el, t| {
                        let off = px(delta * (1. - t));
                        if horizontal {
                            el.left(off)
                        } else {
                            el.top(off)
                        }
                    },
                )
                .into_any_element()
            }
            None => row.into_any_element(),
        };
        div()
            .id(("tab-slot", ix))
            .on_drag_move(cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _: &mut Window, cx| {
                this.on_tab_drag_move(id, e, cx);
            }))
            // Files dragged over this tab (from Arke or from Windows) switch to it after a short wait.
            .on_drag_move(cx.listener(move |this, e: &DragMoveEvent<crate::explorer::DraggedFiles>, window, cx| {
                this.spring_hover(id, e.bounds.contains(&e.event.position), window, cx);
            }))
            .on_drag_move(cx.listener(move |this, e: &DragMoveEvent<gpui::ExternalPaths>, window, cx| {
                this.spring_hover(id, e.bounds.contains(&e.event.position), window, cx);
            }))
            .child(inner)
            .into_any_element()
    }
}

impl Workspace {
    /// A tile reports its bounds during a pin drag. Entering it with another tile moves that one here.
    fn on_tile_drag_move(
        &mut self,
        path: &Path,
        e: &DragMoveEvent<DraggedPin>,
        cx: &mut Context<Self>,
    ) {
        self.tab_drag
            .tile_bounds
            .insert(path.to_path_buf(), e.bounds);
        let dragged = e.drag(cx);
        if !dragged.from_tile
            || dragged.path == path
            || self.tab_drag.moved
            || !e.bounds.contains(&e.event.position)
        {
            return;
        }
        let dragged = dragged.path.clone();
        let old: Vec<PathBuf> = self.favorites(cx).into_iter().map(|(p, ..)| p).collect();
        let Some(to) = old.iter().position(|p| p == path) else {
            return;
        };
        if !old.contains(&dragged) {
            return;
        }
        // Slot positions before the move. Only animate when every tile has reported.
        let slots: Option<Vec<(f32, f32)>> = old
            .iter()
            .map(|p| {
                self.tab_drag
                    .tile_bounds
                    .get(p)
                    .map(|b| (b.origin.x.into(), b.origin.y.into()))
            })
            .collect();
        self.move_pin(&dragged, crate::settings::PinSlot::Tile(to), cx);
        self.tab_drag.moved = true;
        if let Some(slots) = slots.filter(|_| crate::motion::enabled()) {
            let new: Vec<PathBuf> = self.favorites(cx).into_iter().map(|(p, ..)| p).collect();
            let now = std::time::Instant::now();
            for (p, (dx, dy)) in grid_offsets(&slots, &old, &new) {
                // A tile still sliding from the last move continues from where it is drawn.
                let (lx, ly) = self
                    .tab_drag
                    .tile_shift
                    .get(&p)
                    .map_or((0., 0.), |s| s.left(now));
                self.tab_drag.serial += 1;
                self.tab_drag.tile_shift.insert(
                    p,
                    Slide {
                        from: (dx + lx, dy + ly),
                        started: now,
                        serial: self.tab_drag.serial,
                    },
                );
            }
        }
    }

    /// Wraps a favorite tile like [`Self::tab_drag_shell`]: reports drag bounds, slides it from its
    /// old slot after a reorder, and dims the dragged tile while keeping its slot.
    pub(super) fn tile_drag_shell(
        &self,
        ix: usize,
        path: &Path,
        tile: impl IntoElement + Styled + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dragged = self.tab_drag.tile.as_deref() == Some(path);
        let tile = tile.opacity(if dragged { 0.4 } else { 1. });
        let now = std::time::Instant::now();
        let inner = match self
            .tab_drag
            .tile_shift
            .get(path)
            .copied()
            .filter(|s| s.live(now))
        {
            Some(Slide {
                from: (dx, dy),
                serial,
                ..
            }) => tile
                .with_animation(
                    ("tile-shift", serial),
                    crate::motion::enter(),
                    move |el, t| el.left(px(dx * (1. - t))).top(px(dy * (1. - t))),
                )
                .into_any_element(),
            None => tile.into_any_element(),
        };
        let key = path.to_path_buf();
        div()
            .id(("tile-slot", ix))
            .debug_selector(move || format!("tile-slot-{ix}"))
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DraggedPin>, _: &mut Window, cx| {
                    this.on_tile_drag_move(&key, e, cx);
                },
            ))
            .child(inner)
            .into_any_element()
    }
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn spring_hover_for_test(
        &mut self,
        ix: usize,
        inside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.tabs[ix].id;
        self.spring_hover(id, inside, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{crosses, flip_offsets, grid_offsets};

    #[test]
    fn grid_moves_slide_tiles_from_their_old_slots() {
        // Three tiles in a row of 3 (80 apart), the fourth on the next row (60 down).
        let slots = [(0., 0.), (80., 0.), (160., 0.), (0., 60.)];
        // Tile a moves to the third slot: b and c slide left one slot, a slides right two.
        let offsets = grid_offsets(&slots, &['a', 'b', 'c', 'd'], &['b', 'c', 'a', 'd']);
        assert_eq!(
            offsets,
            vec![('b', (80., 0.)), ('c', (80., 0.)), ('a', (-160., 0.))]
        );
        // Across rows: d moves to the front, a to c shift right, c wraps down to the next row.
        let offsets = grid_offsets(&slots, &['a', 'b', 'c', 'd'], &['d', 'a', 'b', 'c']);
        assert_eq!(
            offsets,
            vec![
                ('d', (0., 60.)),
                ('a', (-80., 0.)),
                ('b', (-80., 0.)),
                ('c', (160., -60.))
            ]
        );
        assert!(grid_offsets(&slots, &['a', 'b'], &['a', 'b']).is_empty());
    }

    #[test]
    fn moving_down_slides_the_passed_tabs_up() {
        // Three tabs 30 tall with a 2 gap: a at 0, b at 32, c at 64.
        let old = [(1, 0., 30.), (2, 32., 30.), (3, 64., 30.)];
        // Move a to the end: b and c slide up one slot, a slides down.
        let offsets = flip_offsets(&old, &[2, 3, 1]);
        assert_eq!(offsets, vec![(2, 32.), (3, 32.), (1, -64.)]);
        // Tabs that did not move get no animation.
        assert!(flip_offsets(&old, &[1, 2, 3]).is_empty());
    }

    #[test]
    fn taller_split_tabs_keep_their_size() {
        // Dual-pane tabs are taller (38).
        let old = [(1, 0., 38.), (2, 40., 30.)];
        let offsets = flip_offsets(&old, &[2, 1]);
        assert_eq!(offsets, vec![(2, 40.), (1, -32.)]);
    }

    #[test]
    fn swaps_only_past_the_midline() {
        // Dragging forward swaps only past the next tab's midline.
        assert!(!crosses(0, 1, 40., 32., 30.));
        assert!(crosses(0, 1, 48., 32., 30.));
        // Dragging backward swaps only past the previous tab's midline.
        assert!(!crosses(2, 1, 50., 32., 30.));
        assert!(crosses(2, 1, 46., 32., 30.));
    }
}
