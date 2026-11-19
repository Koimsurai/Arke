//! Adding folders to the sidebar by drag: a tab, folders from a list or a pane, or folders from
//! Explorer dropped on the favorite tiles or the Pinned list go in where the insertion mark shows.
//!
//! - Tiles and pinned folders also take files (moved or copied into them). For folders, the edges
//!   of a tile (its left and right quarters) or a pinned row (top and bottom quarters) insert, and
//!   the middle drops into the folder as before. A tab has nothing to drop into a folder, so all of
//!   a tile or row inserts.
//! - Over the tiles they get a wash and a dashed "Add to favorites" slot after the last one: what
//!   the drop does, and the end of the row to drop on.
//! - Files can't be favorites: they get no mark, and the tiles take them into their folders.
//! - Where the drag would go in is reported on each mouse move ([`report`]) and holds for a few
//!   pixels, across the gaps between tiles and rows ([`current`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, DragMoveEvent, ExternalPaths, Global, IntoElement, Pixels, Point,
    SharedString, Styled, Window, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{DraggedTab, Workspace};
use crate::assets::Icon;
use crate::explorer::DraggedFiles;
use crate::settings::{PinSlot, SettingsStore};
use crate::theme;

/// How far the cursor may move from where an insertion point was reported and still drop there:
/// the gaps between tiles and rows are narrower.
const REACH: f32 = 6.;

/// Where the dragged folders would go in, as last reported. One drag at a time, app-wide.
#[derive(Default)]
struct PinDrop {
    slot: Option<PinSlot>,
    at: Point<Pixels>,
    /// A drag that can add a favorite is over the tiles: they show the "Add to favorites" slot.
    over_tiles: bool,
    /// The item the cursor is over and which side of it gets the insertion mark.
    mark: Option<Mark>,
}

/// Where the insertion mark goes: item `ix` of `area`, at its start (`before`) or end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mark {
    pub(super) area: PinArea,
    pub(super) ix: usize,
    pub(super) before: bool,
}

impl Global for PinDrop {}

/// The two areas folders can be added to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PinArea {
    Tiles,
    List,
}

impl PinArea {
    const fn slot(self, ix: usize) -> PinSlot {
        match self {
            Self::Tiles => PinSlot::Tile(ix),
            Self::List => PinSlot::List(ix),
        }
    }
}

/// The insertion point over an item at `start` (`len` long, along the area's axis) for a cursor
/// at `pos`: before it (`ix`) or after it. With `edges` only the outer quarters insert; the
/// middle (`None`) drops into the folder.
fn zone(start: f32, len: f32, pos: f32, ix: usize, edges: bool) -> Option<usize> {
    if len <= 0. {
        return None;
    }
    let at = (pos - start) / len;
    if edges {
        if at < 0.25 {
            Some(ix)
        } else if at > 0.75 {
            Some(ix + 1)
        } else {
            None
        }
    } else if at < 0.5 {
        Some(ix)
    } else {
        Some(ix + 1)
    }
}

/// Reports where a drag at `at` would go in (`None`: nowhere). Returns whether that changed.
fn report(slot: Option<PinSlot>, mark: Option<Mark>, at: Point<Pixels>, cx: &mut App) -> bool {
    let state = cx.default_global::<PinDrop>();
    let changed = state.slot != slot || state.mark != mark;
    state.slot = slot;
    state.mark = mark.filter(|_| slot.is_some());
    state.at = at;
    changed
}

/// The last report, if the cursor is still near where it was made.
fn fresh(window: &Window, cx: &App) -> Option<(PinSlot, Option<Mark>)> {
    let state = cx.try_global::<PinDrop>()?;
    let at = window.mouse_position();
    let near = (state.at.x - at.x).abs() <= px(REACH) && (state.at.y - at.y).abs() <= px(REACH);
    state.slot.filter(|_| near).map(|slot| (slot, state.mark))
}

/// Where a drop at the cursor would go in, if it is near where that was last reported.
pub(crate) fn current(window: &Window, cx: &App) -> Option<PinSlot> {
    fresh(window, cx).map(|(slot, _)| slot)
}

/// Where the insertion mark shows this frame.
pub(super) fn current_mark(window: &Window, cx: &App) -> Option<Mark> {
    fresh(window, cx).and_then(|(_, mark)| mark)
}

/// Whether the cursor is where dropped folders would be added to the sidebar (not into a folder).
pub(crate) fn inserting(window: &Window, cx: &App) -> bool {
    current(window, cx).is_some()
}

/// The drag ended or was dropped.
pub(super) fn clear(cx: &mut App) {
    if cx.has_global::<PinDrop>() {
        cx.remove_global::<PinDrop>();
    }
}

fn over_tiles(cx: &App) -> bool {
    cx.try_global::<PinDrop>().is_some_and(|s| s.over_tiles)
}

/// The drag card's line over the favorites (the tiles or the list): what dropping there does.
pub(crate) fn action_label() -> SharedString {
    t!("sidebar.add_favorite").into_owned().into()
}

/// What can be added from a drag: `None` when it carries none (files, a tab showing a page);
/// otherwise whether only the item edges insert (`true`, folders that could also drop into it).
pub(super) trait PinDrag: 'static {
    fn edges(&self, ws: &Workspace, cx: &App) -> Option<bool>;
}

impl PinDrag for DraggedTab {
    fn edges(&self, ws: &Workspace, cx: &App) -> Option<bool> {
        ws.dragged_tab_folder(cx).map(|_| false)
    }
}

impl PinDrag for DraggedFiles {
    fn edges(&self, _: &Workspace, cx: &App) -> Option<bool> {
        self.folders(cx).map(|_| true)
    }
}

impl PinDrag for ExternalPaths {
    fn edges(&self, _: &Workspace, _: &App) -> Option<bool> {
        // Whether they are folders is only known after the drop, off the UI thread.
        (!crate::win_virtual_drop::is_virtual(self.paths())).then_some(true)
    }
}

impl Workspace {
    /// The folder of the tab being dragged, unless a page (Home, Settings) covers it.
    pub(super) fn dragged_tab_folder(&self, cx: &App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.dragging_tab?)?;
        let view = tab.view();
        if self.page_title(view.entity_id()).is_some() {
            return None;
        }
        view.read(cx).display_path().map(Path::to_path_buf)
    }

    /// A drag moved over item `ix` of `area` (drawn at `bounds`): reports where it would go in.
    pub(super) fn pin_hover<T: PinDrag>(
        &mut self,
        area: PinArea,
        ix: usize,
        e: &DragMoveEvent<T>,
        cx: &mut Context<Self>,
    ) {
        let (pos, b) = (e.event.position, e.bounds);
        if !b.contains(&pos) {
            return;
        }
        let slot = e.drag(cx).edges(self, cx).and_then(|edges| {
            let (start, len, at) = match area {
                PinArea::Tiles => (b.left(), b.size.width, pos.x),
                PinArea::List => (b.top(), b.size.height, pos.y),
            };
            zone(start.into(), len.into(), at.into(), ix, edges)
        });
        let mark = slot.map(|i| Mark {
            area,
            ix,
            before: i == ix,
        });
        if report(slot.map(|i| area.slot(i)), mark, pos, cx) {
            cx.notify();
        }
    }

    /// A drag moved over something that inserts at `slot` as a whole (the "Pinned" heading, the
    /// "Add to favorites" slot).
    pub(super) fn pin_hover_at<T: PinDrag>(
        &mut self,
        slot: PinSlot,
        e: &DragMoveEvent<T>,
        cx: &mut Context<Self>,
    ) {
        if !e.bounds.contains(&e.event.position) {
            return;
        }
        let slot = e.drag(cx).edges(self, cx).map(|_| slot);
        if report(slot, None, e.event.position, cx) {
            cx.notify();
        }
    }

    /// A drag moved over the tile grid or away from it: the grid shows the "Add to favorites" slot
    /// while something that can be added is over it.
    pub(super) fn tiles_hover<T: PinDrag>(&mut self, e: &DragMoveEvent<T>, cx: &mut Context<Self>) {
        let over = e.bounds.contains(&e.event.position) && e.drag(cx).edges(self, cx).is_some();
        let state = cx.default_global::<PinDrop>();
        if state.over_tiles != over {
            state.over_tiles = over;
            cx.notify();
        }
    }

    /// Adds `paths` at `slot`, in order.
    pub(super) fn pin_paths(&mut self, paths: &[PathBuf], slot: PinSlot, cx: &mut Context<Self>) {
        clear(cx);
        if paths.is_empty() {
            return;
        }
        let places = self.active_view().read(cx).places.clone();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    let mut at = slot;
                    for path in paths {
                        at = match st.insert_pin(path, at, &places) {
                            PinSlot::Tile(ix) => PinSlot::Tile(ix + 1),
                            PinSlot::List(ix) => PinSlot::List(ix + 1),
                        };
                    }
                },
                cx,
            );
        });
        cx.notify();
    }

    /// Folders dropped on a pin area at the cursor: added there. `false` (nothing done) when the
    /// cursor isn't where they would be added.
    pub(super) fn drop_files_to_pin(
        &mut self,
        dragged: &DraggedFiles,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(slot), Some(folders)) = (current(window, cx), dragged.folders(cx)) else {
            return false;
        };
        crate::spring::note_drop(cx);
        self.pin_paths(&folders, slot, cx);
        true
    }

    /// Paths from Explorer dropped on a pin area at the cursor: the folders among them are added
    /// there once checked in the background.
    pub(super) fn drop_external_to_pin(
        &mut self,
        paths: &ExternalPaths,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(slot) = current(window, cx) else {
            return false;
        };
        clear(cx);
        let paths = paths.paths().to_vec();
        let fs = Arc::clone(&self.services.fs);
        let (tx, rx) = flume::bounded::<Vec<PathBuf>>(1);
        crate::explorer::spawn_thread("pin-check", move || {
            let folders = paths
                .into_iter()
                .filter(|p| p.parent().is_none() || fs.stat(p).is_ok_and(|e| e.is_dir_like()))
                .collect();
            let _ = tx.send(folders);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(folders) = rx.recv_async().await {
                let _ = this.update(cx, |this, cx| this.pin_paths(&folders, slot, cx));
            }
        })
        .detach();
        true
    }

    /// The dragged tab dropped on a pin area at the cursor: its folder is added there.
    pub(super) fn drop_tab_to_pin_area(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let slot = current(window, cx);
        let folder = self.dragged_tab_folder(cx);
        self.dragging_tab = None;
        let (Some(slot), Some(folder)) = (slot, folder) else {
            clear(cx);
            cx.notify();
            return false;
        };
        self.pin_paths(&[folder], slot, cx);
        true
    }

    /// The "Add to favorites" slot after the last tile while something that can be added is over
    /// the tiles. `count` is the number of tiles.
    pub(super) fn render_add_favorite_slot(
        &self,
        count: usize,
        large: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !over_tiles(cx) {
            return None;
        }
        let hot = self.pin_shown == Some(PinSlot::Tile(count));
        let slot = PinSlot::Tile(count);
        Some(
            div()
                .id("tile-add-favorite")
                .debug_selector(|| "tile-add-favorite".to_owned())
                .h(if large { px(56.) } else { px(48.) })
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(theme::s_2())
                .px(theme::s_4())
                .rounded(theme::r_8())
                .border_1()
                .border_dashed()
                .border_color(if hot {
                    theme::signal()
                } else {
                    theme::signal_line()
                })
                .when(hot, |d| d.bg(theme::signal_wash()))
                .text_size(theme::fs_11())
                .text_color(theme::text())
                .child(
                    svg()
                        .path(Icon::Plus.path())
                        .size(theme::icon_sm())
                        .text_color(theme::signal()),
                )
                .child(
                    div()
                        .max_w_full()
                        .truncate()
                        .child(t!("sidebar.add_favorite").into_owned()),
                )
                .on_drag_move(
                    cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                        this.pin_hover_at(slot, e, cx);
                    }),
                )
                .on_drag_move(
                    cx.listener(move |this, e: &DragMoveEvent<DraggedFiles>, _, cx| {
                        this.pin_hover_at(slot, e, cx);
                    }),
                )
                .on_drag_move(
                    cx.listener(move |this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                        this.pin_hover_at(slot, e, cx);
                    }),
                )
                .with_pin_drops(cx)
                .into_any_element(),
        )
    }
}

/// The insertion mark: a short accent bar beside a tile (`vertical`) or a line over a row,
/// placed in the item's `relative` wrapper at its start (`before`) or end.
pub(super) fn insertion_mark(vertical: bool, before: bool) -> AnyElement {
    let bar = div().absolute().rounded(theme::R_FULL).bg(theme::signal());
    let bar = if vertical {
        let bar = bar.top(theme::s_4()).bottom(theme::s_4()).w(px(2.));
        if before {
            bar.left(px(-3.))
        } else {
            bar.right(px(-3.))
        }
    } else {
        let bar = bar.left(theme::s_4()).right(theme::s_4()).h(px(2.));
        if before {
            bar.top(px(-2.))
        } else {
            bar.bottom(px(-2.))
        }
    };
    bar.debug_selector(|| "pin-insertion-mark".to_owned())
        .into_any_element()
}

/// Drop handlers for folders, a tab or paths from Explorer added where [`current`] says.
pub(super) trait PinDrops: Sized {
    fn with_pin_drops(self, cx: &mut Context<Workspace>) -> Self;
}

impl PinDrops for gpui::Stateful<gpui::Div> {
    fn with_pin_drops(self, cx: &mut Context<Workspace>) -> Self {
        self.on_drop(cx.listener(|this, dragged: &DraggedFiles, window, cx| {
            this.drop_files_to_pin(dragged, window, cx);
        }))
        .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            this.drop_external_to_pin(paths, window, cx);
        }))
        .on_drop(cx.listener(|this, _: &DraggedTab, window, cx| {
            this.drop_tab_to_pin_area(window, cx);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::zone;

    #[test]
    fn folder_drops_insert_at_the_edges_and_tabs_anywhere() {
        // A tile 80 wide at 100: the outer quarters insert before or after it.
        assert_eq!(zone(100., 80., 110., 3, true), Some(3));
        assert_eq!(
            zone(100., 80., 140., 3, true),
            None,
            "the middle drops into it"
        );
        assert_eq!(zone(100., 80., 175., 3, true), Some(4));
        // A tab inserts on either half.
        assert_eq!(zone(100., 80., 139., 3, false), Some(3));
        assert_eq!(zone(100., 80., 141., 3, false), Some(4));
        assert_eq!(zone(100., 0., 100., 3, false), None);
    }
}
