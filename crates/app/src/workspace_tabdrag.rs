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
//! Favorite tiles reorder the same way on their grid, like app icons on a phone: the dragged tile
//! is lifted under the pointer and leaves an empty slot; entering another tile moves that slot
//! there (saved right away), and the others slide over from their old slots. Dropped, the tile
//! glides from under the pointer into its slot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnimationExt, AnyElement, App, Bounds, Context, DragMoveEvent, ExternalPaths, IntoElement,
    Pixels, Point, Styled, Window, div, prelude::*, px, svg,
};
use rust_i18n::t;

use crate::assets::Icon;
use crate::explorer::DraggedFiles;
use crate::motion::Slide;
use crate::theme;

use super::pindrop::{self, PinArea, PinDrops as _};
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
    /// Favorite tile being dragged (its slot is left empty).
    tile: Option<PathBuf>,
    /// Favorite being dragged, from the tiles or the list: dropped outside the sidebar, it leaves
    /// the favorites (`workspace_sidebar.rs`).
    pin: Option<PathBuf>,
    /// That favorite is outside the sidebar now: its copy under the pointer shows it would go.
    pin_out: bool,
    /// Where the pointer took hold of the dragged tile, from the tile's top left corner.
    grab: Point<Pixels>,
    /// Last reported bounds of each cell of the tile grid, in grid order (outer, without the
    /// sliding offset). Kept per cell, not per tile: a cell stays put whichever tile is in it, so
    /// a reorder never reads where a tile was before the last one.
    tile_cells: HashMap<usize, Bounds<Pixels>>,
    /// Tiles currently sliding back to 0 from their starting offset (x, y).
    tile_shift: HashMap<PathBuf, Slide<(f32, f32)>>,
    /// Bumped each time the dragged favorite turns from a list entry into a tile or back: its copy
    /// under the pointer changes shape then.
    morph: u64,
    /// A tile cell's size, for drawing a list entry that became a tile.
    tile_size: Option<gpui::Size<Pixels>>,
    /// The favorites as they were when the drag started (the drag reorders them live): let go
    /// where nothing takes it, or cancelled (Esc), they go back to this; removed, Undo puts the
    /// favorite back where it was. Taken by a drop that keeps the new order.
    pub(super) origin: Option<PinSnapshot>,
    /// The dragged favorite started on the tiles (`start_tile`).
    from_tile: bool,
    /// The tiles' height when a tile was taken: kept while it is dragged, so a tile gone to the
    /// list leaves the tiles no shorter (the rows under them don't jump up) and the place it left
    /// is still there to take it back.
    pub(super) grid_hold: Option<f32>,
    /// The rows of the places list (the favorites' list and everything under it) slide to their
    /// new places as the favorites change under the drag, like the tiles.
    pub(super) rows: std::rc::Rc<crate::motion::Flip<PlaceRow>>,
}

/// A row of the sidebar's places list, as [`TabDrag::rows`] knows it across a change: a pinned
/// folder (`n`-th row of it, its folders under it when it is expanded), the Recycle Bin, the
/// Shelf's rows and the rows after the favorites, by their place.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum PlaceRow {
    Pinned(PathBuf, usize),
    Recycle,
    Shelf(usize),
    After(usize),
}

/// Both favorites lists, as saved (`Settings::favorites`, `Settings::pinned`).
#[derive(Clone, Debug)]
pub(super) struct PinSnapshot {
    pub(super) favorites: Option<Vec<std::path::PathBuf>>,
    pub(super) pinned: Vec<std::path::PathBuf>,
}

/// Hover time before switching to a tab while dragging files, like Explorer's spring-loaded folders.
const SPRING_DELAY: std::time::Duration = std::time::Duration::from_millis(600);

impl TabDrag {
    /// Called on each frame so the next mouse move may swap again.
    pub(super) const fn frame(&mut self) {
        self.moved = false;
    }

    /// A drag of the favorite at `path` starts, from the tiles (`tile`) or the list, taken hold of
    /// at `grab`.
    pub(super) fn start_tile(&mut self, path: PathBuf, tile: bool, grab: Point<Pixels>) {
        self.from_tile = tile;
        self.tile = tile.then(|| path.clone());
        self.pin = Some(path);
        self.pin_out = false;
        self.grab = grab;
        self.tile_shift.clear();
        self.rows.start();
        // The grid may have moved since the last drag (a wider sidebar): every cell reports again.
        self.tile_cells.clear();
    }

    /// The pin drag ended (dropped or cancelled). Slides still running finish.
    pub(super) fn end_tile(&mut self) {
        self.tile = None;
        self.pin = None;
        self.pin_out = false;
        self.grid_hold = None;
        self.rows.stop();
    }

    /// A favorite from the list is being dragged (not yet a tile).
    pub(super) const fn pin_from_list(&self) -> bool {
        self.pin.is_some() && self.tile.is_none()
    }

    /// The dragged favorite is a tile now (the tiles leave its cell empty), or a list entry
    /// (`None`, its row left empty).
    pub(super) fn set_tile(&mut self, tile: Option<PathBuf>) {
        if self.tile.is_some() != tile.is_some() {
            self.morph += 1;
        }
        self.tile = tile;
    }

    /// The dragged favorite is a tile at the moment.
    pub(super) const fn is_tile_now(&self) -> bool {
        self.tile.is_some()
    }

    /// The dragged favorite was a tile when the drag started (else a list entry). Only the other
    /// area shows where it can go: the one it left just closes its gap.
    pub(super) const fn started_on_tile(&self) -> bool {
        self.from_tile
    }

    /// Changes each time the dragged favorite changes shape.
    pub(super) const fn morph(&self) -> u64 {
        self.morph
    }

    /// A tile cell's size, as last reported.
    pub(super) const fn tile_size(&self) -> Option<gpui::Size<Pixels>> {
        self.tile_size
    }

    /// Whether something already moved on this mouse move.
    pub(super) const fn moved_now(&self) -> bool {
        self.moved
    }

    /// Something moved: no other move until the next frame.
    pub(super) const fn mark_moved(&mut self) {
        self.moved = true;
    }

    /// The favorite being dragged, if any.
    pub(super) fn dragged_pin(&self) -> Option<&Path> {
        self.pin.as_deref()
    }

    /// Whether the dragged favorite is outside the sidebar, where letting go removes it.
    pub(super) const fn pin_out(&self) -> bool {
        self.pin_out
    }

    /// The pointer went outside the sidebar or back over it; returns whether that changed.
    pub(super) const fn set_pin_out(&mut self, out: bool) -> bool {
        let changed = self.pin.is_some() && self.pin_out != out;
        if changed {
            self.pin_out = out;
        }
        changed
    }

    /// A file drag ended: no tab is waiting to come forward.
    pub(super) const fn end_spring(&mut self) {
        self.spring = None;
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
        // The cursor must be on the tab strip (within this tab on the cross axis), and among the
        // tabs: on its way up to the favorites over the sidebar, the tab stays where it is.
        let among_tabs = self
            .tab_layout
            .borrow()
            .area()
            .is_none_or(|area| area.contains(&p));
        let inside = among_tabs
            && if horizontal {
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
                self.tab_drag.shift.insert(
                    id,
                    Slide {
                        from: delta + left,
                        started: now,
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
                cx.notify();
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
        cx.notify();
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

    /// The folder files dropped on tab `ix` go into: what its active side shows, unless a page
    /// (Home, Settings, the Recycle Bin) covers it.
    pub(super) fn tab_drop_folder(&self, ix: usize, cx: &App) -> Option<PathBuf> {
        let view = self.tabs.get(ix)?.view();
        if self.page_title(view.entity_id()).is_some() {
            return None;
        }
        view.read(cx).current_path().map(Path::to_path_buf)
    }

    /// Files dragged onto tab `ix` (from Arke or from Windows) drop into `dest`, its folder, as on
    /// a folder: highlighted, with the move or copy shown on the drag card and the same keys. The
    /// tab comes forward after a short hover ([`Self::tab_drag_shell`]), so the list shows where
    /// they go.
    pub(super) fn with_tab_file_drop(
        &self,
        ix: usize,
        dest: PathBuf,
        row: gpui::Stateful<gpui::Div>,
    ) -> gpui::Stateful<gpui::Div> {
        let (check, hovered, external) = (dest.clone(), dest.clone(), dest.clone());
        let view = self.tabs[ix].view().downgrade();
        row.can_drop(move |value, _, cx| crate::explorer::accepts_drop(value, &check, cx))
            .on_drag_move(move |e: &DragMoveEvent<DraggedFiles>, _, cx| {
                crate::explorer::report_drop_target(&hovered, e, cx);
            })
            .drag_over::<DraggedFiles>(|s, _, _, _| s.bg(theme::signal_wash()))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(theme::signal_wash()))
            .on_drop(move |dragged: &DraggedFiles, window, cx| {
                crate::explorer::drop_into(dragged, dest.clone(), window, cx);
            })
            .on_drop(move |paths: &ExternalPaths, window, cx| {
                if let Some(view) = view.upgrade() {
                    crate::explorer::drop_external_into(paths, external.clone(), &view, window, cx);
                }
            })
    }

    /// The bar filling under a tab while files hover over it, until it comes forward.
    fn tab_spring_bar(&self, id: u64) -> Option<AnyElement> {
        let (_, serial) = self.tab_drag.spring.filter(|(tab, _)| *tab == id)?;
        Some(
            div()
                .absolute()
                .left(theme::s_8())
                .bottom_0()
                .h(px(2.))
                .rounded(theme::R_FULL)
                .bg(theme::signal())
                .debug_selector(|| "tab-spring-bar".to_owned())
                .with_animation(
                    ("tab-spring", serial),
                    gpui::Animation::new(SPRING_DELAY),
                    |el, t| el.w(gpui::relative(t * 0.9)),
                )
                .into_any_element(),
        )
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
                .h(theme::h_row())
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::s_8())
                .px(theme::s_8())
                .rounded(theme::r_8())
                .border_1()
                .border_dashed()
                .border_color(theme::signal_line())
                .bg(theme::signal_wash())
                .text_color(theme::text())
                .child(
                    svg()
                        .path(Icon::Pin.path())
                        .size(theme::icon())
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
        // Sliding from its old place: the offset is styled on the row itself, from the slide's
        // clock, so the row is the same element while it slides and afterwards (a click across
        // the end of the slide still lands).
        let now = std::time::Instant::now();
        let inner = match self.tab_drag.shift.get(&id).filter(|s| s.live(now)) {
            Some(slide) => {
                self.sidebar_ui.frame_wanted.set(true);
                let off = px(slide.left(now));
                if Self::horizontal_tabs(cx) {
                    row.left(off)
                } else {
                    row.top(off)
                }
            }
            None => row,
        };
        div()
            .id(("tab-slot", ix))
            .relative()
            .child(self.tab_slot_recorder(id))
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
            .children(self.tab_spring_bar(id))
            .into_any_element()
    }
}

impl Workspace {
    /// The tile in grid cell `cell` reports its bounds during a pin drag. Entering it with another
    /// tile moves that one here; a favorite dragged up from the list becomes a tile here, its cell
    /// left empty under the pointer like a dragged tile's.
    fn on_tile_drag_move(
        &mut self,
        cell: usize,
        path: &Path,
        e: &DragMoveEvent<DraggedPin>,
        cx: &mut Context<Self>,
    ) {
        self.tab_drag.tile_cells.insert(cell, e.bounds);
        self.tab_drag.tile_size = Some(e.bounds.size);
        let dragged = e.drag(cx).path.clone();
        if dragged == path || self.tab_drag.moved || !e.bounds.contains(&e.event.position) {
            return;
        }
        // The saved order counts every favorite, tiles and the ones listed as rows (#174).
        let old: Vec<PathBuf> = self.favorites(cx).into_iter().map(|(p, ..)| p).collect();
        let Some(to) = old.iter().position(|p| p == path) else {
            return;
        };
        // The grid only holds the favorites drawn as tiles: their slots before the move.
        let old_tiles = self.tile_favorites(cx);
        let slots = self.tile_slots(old_tiles.len());
        // A list entry leaving the list closes its gap there.
        self.tab_drag.rows.moved();
        self.move_pin(&dragged, crate::settings::PinSlot::Tile(to), cx);
        self.tab_drag.moved = true;
        // From the list: it is a tile now, drawn as the gap where it would land.
        self.tab_drag.set_tile(Some(dragged.clone()));
        if let Some(slots) = slots {
            let new_tiles = self.tile_favorites(cx);
            self.slide_tiles(&slots, &old_tiles, &new_tiles);
        }
    }

    /// A favorite from the list over the tiles' empty space, past the last tile (where one that
    /// left for the list was): it becomes a tile again, the last one, as over a tile it would
    /// take that tile's place.
    pub(super) fn pin_over_tiles(&mut self, e: &DragMoveEvent<DraggedPin>, cx: &mut Context<Self>) {
        let at = e.event.position;
        if !e.bounds.contains(&at)
            || self.tab_drag.moved
            || self.tab_drag.tile.is_some()
            || self.tab_drag.pin.is_none()
        {
            return;
        }
        let old_tiles = self.tile_favorites(cx);
        // Over a tile, that tile takes it (`on_tile_drag_move`). Only the cells drawn now count
        // (the one a tile left for the list keeps its last bounds), and only once all of them
        // have said where they are: before, any of them may be under the pointer.
        let Some(cells) = self.tile_slots(old_tiles.len()) else {
            return;
        };
        if cells.iter().any(|cell| cell.contains(&at)) {
            return;
        }
        let places = self.active_view().read(cx).places.clone();
        if crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .tiles(&places)
            .len()
            >= crate::settings::FAVORITES_MAX
        {
            return;
        }
        let dragged = e.drag(cx).path.clone();
        let end = self.favorites(cx).len();
        self.tab_drag.rows.moved();
        self.move_pin(&dragged, crate::settings::PinSlot::Tile(end), cx);
        self.tab_drag.moved = true;
        self.tab_drag.set_tile(Some(dragged));
    }

    /// Where the first `count` cells of the tile grid are, in grid order. `None` until every one
    /// has reported: then nothing animates.
    fn tile_slots(&self, count: usize) -> Option<Vec<Bounds<Pixels>>> {
        (0..count)
            .map(|cell| self.tab_drag.tile_cells.get(&cell).copied())
            .collect()
    }

    /// The tiles whose slot changed from `old` to `new` order slide over from where they were
    /// drawn, every one of them (across rows too). A tile still sliding from the last move goes
    /// on from where it is, so quick moves never jump.
    fn slide_tiles(&mut self, slots: &[Bounds<Pixels>], old: &[PathBuf], new: &[PathBuf]) {
        if !crate::motion::enabled() || old.len() != new.len() {
            return;
        }
        let origins: Vec<(f32, f32)> = slots
            .iter()
            .map(|b| (b.origin.x.into(), b.origin.y.into()))
            .collect();
        let now = std::time::Instant::now();
        for (p, (dx, dy)) in grid_offsets(&origins, old, new) {
            let (lx, ly) = self
                .tab_drag
                .tile_shift
                .get(&p)
                .filter(|s| s.live(now))
                .map_or((0., 0.), |s| s.left(now));
            self.tab_drag.tile_shift.insert(
                p,
                Slide {
                    from: (dx + lx, dy + ly),
                    started: now,
                },
            );
        }
    }

    /// A dragged tile let go over the tiles: the tiles already reordered while it moved, so it
    /// only glides from under the pointer, where its lifted copy was, into its slot.
    pub(super) fn drop_tile(&mut self, path: &Path, window: &Window, cx: &mut Context<Self>) {
        let old_tiles = self.tile_favorites(cx);
        let slots = self.tile_slots(old_tiles.len());
        let grab = self.tab_drag.grab;
        self.dragging_pin = false;
        self.tab_drag.origin = None;
        self.tab_drag.end_tile();
        pindrop::clear(cx);
        cx.notify();
        let Some(slots) = slots else {
            return;
        };
        let new_tiles = self.tile_favorites(cx);
        self.slide_tiles(&slots, &old_tiles, &new_tiles);
        let Some(home) = new_tiles
            .iter()
            .position(|p| p == path)
            .and_then(|ix| slots.get(ix))
            .filter(|_| crate::motion::enabled() && old_tiles.len() == new_tiles.len())
        else {
            return;
        };
        let (dx, dy) = settle_offset(window.mouse_position(), grab, *home);
        self.tab_drag.tile_shift.insert(
            path.to_path_buf(),
            Slide {
                from: (dx, dy),
                started: std::time::Instant::now(),
            },
        );
    }

    /// Wraps a favorite tile like [`Self::tab_drag_shell`]: reports drag bounds and slides it from
    /// its old slot after a reorder. The dragged tile's slot stays empty (the tile is under the
    /// pointer), keeping its place in the grid.
    ///
    /// `ix` is the tile's slot among all the favorites (some are rows, #174), `cell` its place in
    /// the grid.
    pub(super) fn tile_drag_shell(
        &self,
        ix: usize,
        cell: usize,
        path: &Path,
        tile: impl IntoElement + Styled + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dragged = self.tab_drag.tile.as_deref() == Some(path) && cx.has_active_drag();
        let tile = if dragged { tile.invisible() } else { tile };
        // Sliding from its old slot: styled in place, like a tab's slide.
        let now = std::time::Instant::now();
        let inner = match self.tab_drag.tile_shift.get(path).filter(|s| s.live(now)) {
            Some(slide) => {
                self.sidebar_ui.frame_wanted.set(true);
                let (dx, dy) = slide.left(now);
                tile.left(px(dx)).top(px(dy))
            }
            None => tile,
        };
        let key = path.to_path_buf();
        // A folder or tab over its edge (all of it, for a tab) goes in beside it
        // (`workspace_pindrop.rs`).
        let mark = self
            .pin_mark
            .filter(|m| self.pin_shown.is_some() && m.area == PinArea::Tiles && m.ix == ix)
            .map(|m| pindrop::insertion_mark(true, m.before));
        div()
            .id(("tile-slot", ix))
            .debug_selector(move || format!("tile-slot-{ix}"))
            .relative()
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DraggedPin>, _: &mut Window, cx| {
                    this.on_tile_drag_move(cell, &key, e, cx);
                },
            ))
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                    this.pin_hover(PinArea::Tiles, ix, e, cx);
                }),
            )
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<crate::explorer::DraggedFiles>, _, cx| {
                    this.pin_hover(PinArea::Tiles, ix, e, cx);
                },
            ))
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    this.pin_hover(PinArea::Tiles, ix, e, cx);
                },
            ))
            .with_pin_drops(cx)
            .child(inner)
            .children(mark)
            .when(dragged, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .debug_selector(|| "tile-gap".to_owned()),
                )
            })
            .into_any_element()
    }
}

/// How far a dropped tile is from its slot `home`: from the lifted copy under the pointer at
/// `mouse` (taken hold of at `grab`, [`super::sidebar::TILE_LIFT`] times the tile's size) to the
/// slot, center to center.
fn settle_offset(mouse: Point<Pixels>, grab: Point<Pixels>, home: Bounds<Pixels>) -> (f32, f32) {
    let lift = super::sidebar::TILE_LIFT;
    let axis = |mouse: Pixels, grab: Pixels, start: Pixels, len: Pixels| -> f32 {
        let (mouse, grab, start, len) = (
            f32::from(mouse),
            f32::from(grab),
            f32::from(start),
            f32::from(len),
        );
        // The copy's center: the pointer, less the grab point and plus half the size, all lifted.
        let lifted = lift.mul_add(len / 2. - grab, mouse);
        lifted - (start + len / 2.)
    };
    (
        axis(mouse.x, grab.x, home.origin.x, home.size.width),
        axis(mouse.y, grab.y, home.origin.y, home.size.height),
    )
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

    /// Where the tile at `path` slides in from (its offset from its slot), while it slides.
    pub(crate) fn tile_slide_from_for_test(&self, path: &Path) -> Option<(f32, f32)> {
        let now = std::time::Instant::now();
        self.tab_drag
            .tile_shift
            .get(path)
            .filter(|slide| slide.live(now))
            .map(|slide| slide.from)
    }

    /// Where the pinned folder `path`'s row slides in from (its offset from where it is laid
    /// out, down), while it slides; `None` for the Recycle Bin's row.
    pub(crate) fn list_row_slide_from_for_test(&self, path: Option<&Path>) -> Option<f32> {
        let key = path.map_or(PlaceRow::Recycle, |p| PlaceRow::Pinned(p.to_path_buf(), 0));
        self.tab_drag.rows.sliding_from(&key)
    }

    /// The favorite tiles sliding from their old slots after a reorder, sorted.
    pub(crate) fn tiles_sliding_for_test(&self) -> Vec<PathBuf> {
        let now = std::time::Instant::now();
        let mut sliding: Vec<PathBuf> = self
            .tab_drag
            .tile_shift
            .iter()
            .filter(|(_, slide)| slide.live(now))
            .map(|(path, _)| path.clone())
            .collect();
        sliding.sort();
        sliding
    }
}

#[cfg(test)]
mod tests {
    use super::{crosses, flip_offsets, grid_offsets, settle_offset};
    use gpui::{Bounds, point, px, size};

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
    fn a_dropped_tile_glides_in_from_under_the_pointer() {
        // A tile 80 x 56 at (100, 40), taken hold of at its center.
        let home = Bounds::new(point(px(100.), px(40.)), size(px(80.), px(56.)));
        let grab = point(px(40.), px(28.));
        let near = |(dx, dy): (f32, f32), (x, y): (f32, f32)| {
            assert!((dx - x).abs() < 0.01 && (dy - y).abs() < 0.01, "{dx} {dy}");
        };
        // Let go right over its slot: nothing to cover.
        near(
            settle_offset(point(px(140.), px(68.)), grab, home),
            (0., 0.),
        );
        // 30 to the right of it and 20 down: it starts from there.
        near(
            settle_offset(point(px(170.), px(88.)), grab, home),
            (30., 20.),
        );
        // Taken hold of at its top left corner, the lifted copy reaches further right and down.
        near(
            settle_offset(point(px(100.), px(40.)), point(px(0.), px(0.)), home),
            (4., 2.8),
        );
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
