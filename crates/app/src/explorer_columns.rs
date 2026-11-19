//! Column view (like Finder's columns): parent | current folder | preview.
//!
//! - Left column lists the parent folder with the current folder marked; clicking another folder
//!   goes there.
//! - Middle column is the current folder (names only); Left goes up, Right enters a folder.
//! - Right column shows the preview without opening the preview pane.
//!
//! The parent is listed in the background (`Fs::read_dir`, never on the UI thread) and only when
//! the folder changes.
//!
//! The parent and current columns can be sized like Finder's: drag the divider on a column's right
//! edge (the current column's is the preview's edge), double-click it to fit the column to its
//! longest name. The widths are app-wide, the same in every folder. Names cut off by a narrow
//! column show in full in a tooltip.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::filetype::FileKind;
use arke_core::{Entry, NavKind};
use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, Pixels, ScrollStrategy,
    SharedString, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use super::view::listed_visual;
use super::{AppEvent, Explorer, ViewMode, layout, spawn_thread};
use crate::media::MediaKey;
use crate::prefs::ViewColumn;
use crate::theme;

/// Default width of the left (parent) column
pub(super) fn parent_w() -> gpui::Pixels {
    crate::theme::scaled_px(240.)
}
/// Fitting measures at most this many names, the longest by character count: enough to find the
/// widest one without shaping every name of a huge folder.
const FIT_CANDIDATES: usize = 256;

/// The parent column's divider being dragged, in this pane.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DraggedParentEdge(gpui::EntityId);

pub(crate) struct ParentColumn {
    pub(crate) dir: PathBuf,
    /// `None` while listing
    pub(crate) entries: Option<Arc<Vec<Entry>>>,
    pub(crate) scroll: UniformListScrollHandle,
}

impl Explorer {
    pub(super) fn columns_active(&self) -> bool {
        self.view_mode == ViewMode::Columns && !self.search_mode
    }

    /// Called on every render: in column view, list the parent in the background after a folder
    /// change.
    pub(super) fn sync_parent_column(&mut self) {
        if !self.columns_active() {
            return;
        }
        // A share root's parent is its server (`\\nas`), listed as its shares.
        let Some(parent) = self.current_path().and_then(arke_core::path::parent) else {
            self.parent_column = None;
            return;
        };
        if self.parent_column.as_ref().is_some_and(|c| c.dir == parent) {
            return;
        }
        self.parent_column = Some(ParentColumn {
            dir: parent.clone(),
            entries: None,
            scroll: UniformListScrollHandle::new(),
        });
        let fs = Arc::clone(&self.services.fs);
        let probe = self.services.shell.net_probe();
        let tx = self.events_tx.clone();
        let show_hidden = self.show_hidden;
        // Sort like the middle column (folders first, case-insensitive natural order, current sort
        // column).
        let sort = self.sort;
        spawn_thread("parent-column", move || {
            let mut entries: Vec<Entry> =
                super::path_bar::list_blocking(&*fs, probe.as_deref(), &parent)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| show_hidden || !e.is_hidden())
                    .collect();
            sort.sort(&mut entries);
            let _ = tx.send(AppEvent::ParentColumn {
                dir: parent,
                entries: Arc::new(entries),
            });
        });
    }

    pub(super) fn handle_parent_column(
        &mut self,
        dir: PathBuf,
        entries: Arc<Vec<Entry>>,
        cx: &mut Context<Self>,
    ) {
        let here = self
            .current_path()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned());
        let Some(column) = self.parent_column.as_mut().filter(|c| c.dir == dir) else {
            return;
        };
        if let Some(ix) = here.and_then(|h| entries.iter().position(|e| *e.name == *h)) {
            column.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        column.entries = Some(entries);
        cx.notify();
    }

    fn render_parent_rows(
        &mut self,
        range: Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(column) = &self.parent_column else {
            return Vec::new();
        };
        let Some(entries) = column.entries.clone() else {
            return Vec::new();
        };
        let dir = column.dir.clone();
        let here = self.current_path().map(Path::to_path_buf);
        let shares = super::view::lists_shares(Some(&dir));
        // System icons for .exe/.lnk/.ico like the middle column, loaded after it so they don't
        // compete.
        let images: Vec<Option<Arc<gpui::RenderImage>>> = if self.media_ok(&dir, cx) {
            let keys: Vec<Option<MediaKey>> = range
                .clone()
                .map(|ix| {
                    entries
                        .get(ix)
                        .and_then(|e| MediaKey::for_entry(&dir, e, false))
                })
                .collect();
            self.media.update(cx, |m, _| {
                m.want_side(keys.iter().flatten().cloned().collect());
            });
            let media = self.media.read(cx);
            keys.iter()
                .map(|k| k.as_ref().and_then(|k| media.get(k)))
                .collect()
        } else {
            Vec::new()
        };
        let show_ext = self.show_extensions;
        let width = self.parent_column_drawn();
        let start = range.start;
        range
            .filter_map(|ix| entries.get(ix).map(|e| (ix, e)))
            .map(|(ix, entry)| {
                let path = dir.join(&*entry.name);
                let current = here.as_deref() == Some(path.as_path());
                let is_dir = entry.is_dir_like();
                let name: Arc<str> = Arc::clone(&entry.name);
                let parent = dir.clone();
                let shown = super::view::shown_name(entry, show_ext);
                let cut = name_width(window, &shown) > width - parent_row_chrome(is_dir);
                div()
                    .id(("parent-col", ix))
                    .when(cut, |d| {
                        d.tooltip(super::view::tooltip(SharedString::from(&*entry.name)))
                    })
                    .relative()
                    .h(theme::h_row_compact())
                    .px(theme::s_8())
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .rounded(theme::r_8())
                    .cursor_pointer()
                    // The folder shown on the right: neutral selection (focus is there).
                    .when(current, |d| d.bg(theme::selection()))
                    .when(!current, |d| d.hover(|s| s.bg(theme::wash())))
                    .text_color(if entry.is_hidden() {
                        theme::text_subtle()
                    } else {
                        theme::text()
                    })
                    .child(listed_visual(
                        shares,
                        entry,
                        images.get(ix - start).cloned().flatten(),
                        theme::icon(),
                        theme::icon(),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(shown),
                    )
                    .when(is_dir, |d| d.child(chevron()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if is_dir {
                            // Sibling folder: go there
                            this.navigate(path.clone(), NavKind::New, cx);
                        } else {
                            // File in the parent: go up and select it
                            this.navigate_selecting(
                                parent.clone(),
                                NavKind::New,
                                Some(Arc::clone(&name)),
                                cx,
                            );
                        }
                    }))
                    .into_any_element()
            })
            .collect()
    }

    /// Left column of the column view.
    pub(super) fn render_parent_column(&self, cx: &mut Context<Self>) -> AnyElement {
        let column = div()
            .w(self.parent_column_drawn())
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .pt(theme::s_4())
            .border_r_1()
            .border_color(theme::line());
        let Some(parent) = &self.parent_column else {
            return column.into_any_element();
        };
        let Some(entries) = &parent.entries else {
            return column
                .items_center()
                .justify_center()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(rust_i18n::t!("info.loading").to_string())
                .into_any_element();
        };
        column
            .child(
                uniform_list(
                    "parent-column",
                    entries.len(),
                    cx.processor(|this, range: Range<usize>, window, cx| {
                        this.render_parent_rows(range, window, cx)
                    }),
                )
                .track_scroll(&parent.scroll)
                .px(theme::s_8())
                .flex_1(),
            )
            .into_any_element()
    }

    /// Middle column rows show only the name; folders get a trailing arrow.
    pub(super) fn column_row_tail(entry: &Entry) -> Option<AnyElement> {
        Self::column_row_tail_shown(entry).then(|| chevron().into_any_element())
    }

    /// Width of the parent column: what the user gave it, or the default.
    pub(super) fn parent_column_width() -> Pixels {
        crate::prefs::view_column_width(ViewColumn::Parent).unwrap_or_else(parent_w)
    }

    /// Width the parent column is drawn at: [`Self::parent_column_width`], but no more than this
    /// pane leaves it. The width is shared by all panes, so one set in a wide pane would otherwise
    /// squeeze the current column to nothing in a split tab or a smaller window.
    pub(super) fn parent_column_drawn(&self) -> Pixels {
        self.fit_input(self.paper_width, self.screen.0.width)
            .parent_w
    }

    /// How wide `col` is drawn (the current column: what the parent left of the list at the last
    /// paint, so also while it fills the room).
    pub(super) fn view_column_shown_width(&self, col: ViewColumn) -> Pixels {
        let parent = self.parent_column_drawn();
        match col {
            ViewColumn::Parent => parent,
            ViewColumn::Current => (self.list_bounds.get().size.width - parent).max(px(0.)),
        }
    }

    /// Widest `col` may be made in this pane now.
    fn view_column_room(&self, col: ViewColumn) -> Pixels {
        let input = self.fit_input(self.paper_width, self.screen.0.width);
        match col {
            ViewColumn::Parent => layout::parent_room(&input),
            ViewColumn::Current => layout::current_room(&input),
        }
    }

    /// A divider moved during a drag: size `col` to `width`, kept between the minimum and what
    /// the pane leaves. Saved on release (see `before_render`).
    pub(super) fn drag_view_column(
        &mut self,
        col: ViewColumn,
        width: Pixels,
        cx: &mut Context<Self>,
    ) {
        let width = width.clamp(layout::view_column_min(), self.view_column_room(col));
        crate::prefs::set_view_column_width(col, Some(width));
        self.view_column_resized = Some(col);
        cx.notify();
    }

    /// A divider was double-clicked: fit `col` to its longest name (within the fit bounds and
    /// what the pane leaves), and save it.
    pub(super) fn fit_view_column(
        &mut self,
        col: ViewColumn,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let entries = match col {
            ViewColumn::Parent => self.parent_column.as_ref().and_then(|c| c.entries.clone()),
            ViewColumn::Current => self.view(),
        };
        let Some(entries) = entries.filter(|e| !e.is_empty()) else {
            return;
        };
        let show_ext = self.show_extensions;
        let mut names: Vec<SharedString> = entries
            .iter()
            .map(|e| super::view::shown_name(e, show_ext))
            .collect();
        names.sort_unstable_by_key(|n| std::cmp::Reverse(n.chars().count()));
        names.truncate(FIT_CANDIDATES);
        let widest = names
            .iter()
            .map(|n| name_width(window, n))
            .fold(px(0.), Ord::max);
        let chrome = match col {
            ViewColumn::Parent => parent_row_chrome(true),
            ViewColumn::Current => self.current_row_chrome(true),
        };
        // A little air so subpixel rounding never truncates the widest name.
        let width = (chrome + widest + theme::s_4())
            .ceil()
            .clamp(layout::view_column_min(), layout::view_column_fit_max())
            .min(self.view_column_room(col));
        log::info!("column view fit: {col:?} {width:?}");
        crate::prefs::set_view_column_width(col, Some(width));
        self.view_column_resized = Some(col);
        self.save_view_column_widths(cx);
    }

    /// After sizing a column view column: save the widths to settings (shared by all windows).
    pub(crate) fn save_view_column_widths(&mut self, cx: &mut Context<Self>) {
        if self.view_column_resized.take().is_none() {
            return;
        }
        let widths = crate::prefs::stored_view_column_widths();
        log::info!("column view widths: {widths:?}");
        crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(|st| st.column_view_widths = widths, cx);
        });
        cx.notify();
    }

    /// Room a current column row takes besides its name: the list's padding and gutter, the row's
    /// padding, the checkbox and icon, and the folder arrow (`chevron`).
    fn current_row_chrome(&self, chevron: bool) -> Pixels {
        let mut w = theme::s_8() * 2.
            + layout::list_gutter()
            + layout::row_pad_l()
            + layout::row_pad_r()
            + self.name_lead(None);
        if chevron {
            w += layout::col_gap() + theme::icon_sm();
        }
        w
    }

    /// Which of the current column's rows in `range` cut their name off (they get a tooltip with
    /// the whole name). Empty outside the column view.
    pub(super) fn column_name_tips(&self, range: Range<usize>, window: &Window) -> Vec<bool> {
        if !self.columns_active() {
            return Vec::new();
        }
        let Some(entries) = self.view() else {
            return Vec::new();
        };
        let width = self.view_column_shown_width(ViewColumn::Current);
        let in_cloud = self.current_path().is_some_and(|p| self.in_cloud(p));
        range
            .filter_map(|ix| entries.get(ix))
            .map(|entry| {
                let mut room = width - self.current_row_chrome(Self::column_row_tail_shown(entry));
                // The sync or compare badge after the name, roughly.
                if in_cloud {
                    room -= theme::s_12() + theme::icon();
                }
                if self.compare_mark(&entry.name).is_some() {
                    room -= theme::s_12() + crate::theme::scaled_px(64.);
                }
                name_width(
                    window,
                    &super::view::shown_name(entry, self.show_extensions),
                ) > room
            })
            .collect()
    }

    /// Whether a middle column row ends with a folder arrow.
    fn column_row_tail_shown(entry: &Entry) -> bool {
        entry.is_dir_like() && arke_core::filetype::file_kind(entry) == FileKind::Folder
    }

    /// The parent column's divider, over its right border (drawn above the current column, whose
    /// left padding it covers, so rows on either side keep their clicks): drag to size the
    /// column, double-click to fit it to its names.
    pub(super) fn render_parent_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dragging = self.view_column_resized == Some(ViewColumn::Parent) && cx.has_active_drag();
        div()
            .id("parent-column-resize")
            .absolute()
            .top_0()
            .bottom_0()
            .left(self.parent_column_drawn() - theme::s_4())
            .w(theme::s_8())
            // Presses stay here: the list under it would clear the selection.
            .occlude()
            .cursor_col_resize()
            .group("parent-column-resize")
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(theme::s_4() - px(1.))
                    .w(px(2.))
                    .when(dragging, |d| d.bg(theme::signal()))
                    .when(!dragging, |d| {
                        d.group_hover("parent-column-resize", |s| s.bg(theme::signal_line()))
                    }),
            )
            // Like the preview's edge: the drag moves the divider by as much as the cursor moved
            // since the press, so it neither jumps nor lags by the drag threshold.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.view_column_drag =
                        Some((ViewColumn::Parent, e.position.x, this.parent_column_drawn()));
                }),
            )
            .on_drag(DraggedParentEdge(cx.entity_id()), |_, _, _, cx| {
                cx.new(|_| super::details::ColumnDragView)
            })
            .on_drag_move(cx.listener(
                |this, e: &gpui::DragMoveEvent<DraggedParentEdge>, _, cx| {
                    if e.drag(cx).0 != cx.entity_id() {
                        return;
                    }
                    if let Some((ViewColumn::Parent, from, start)) = this.view_column_drag {
                        this.drag_view_column(
                            ViewColumn::Parent,
                            start + e.event.position.x - from,
                            cx,
                        );
                    }
                },
            ))
            .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                if e.click_count() >= 2 {
                    this.fit_view_column(ViewColumn::Parent, window, cx);
                }
            }))
    }
}

/// Room a parent column row takes besides its name: the column's border, the list's and the
/// row's padding, the icon, and the folder arrow (`chevron`).
fn parent_row_chrome(chevron: bool) -> Pixels {
    let mut w = px(1.) + theme::s_8() * 4. + theme::icon() + theme::s_8();
    if chevron {
        w += theme::s_8() + theme::icon_sm();
    }
    w
}

/// Width of a row's name as drawn.
fn name_width(window: &Window, name: &str) -> Pixels {
    super::details::text_width(window, name, theme::fs_14(), gpui::FontWeight::NORMAL)
}

fn chevron() -> impl IntoElement {
    gpui::svg()
        .path(crate::assets::Icon::ChevronRight.path())
        .size(theme::icon_sm())
        .flex_none()
        .text_color(theme::text_subtle())
}

#[cfg(test)]
impl Explorer {
    pub(crate) fn parent_column_names(&self) -> Vec<String> {
        self.parent_column
            .as_ref()
            .and_then(|c| c.entries.as_ref())
            .map_or_default(|e| e.iter().map(|e| e.name.to_string()).collect())
    }

    /// Which of the current column's names are cut off (and get a tooltip).
    pub(crate) fn current_name_tips_for_test(&self, window: &Window) -> Vec<bool> {
        let len = self.view().map_or(0, |v| v.len());
        self.column_name_tips(0..len, window)
    }
}
