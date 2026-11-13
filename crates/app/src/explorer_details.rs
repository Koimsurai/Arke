//! Details view columns: the header (sorting, multi-key sorting, resizing, reordering, the
//! right-click column menu) and the cells of each row.
//!
//! - Click a header to sort by it alone (again to flip). Shift+click adds it as the next sort key
//!   (up to three); small 1/2/3 marks show the order once there is more than one.
//! - Columns run left to right from Name, each with its own width (saved app-wide). A column's
//!   divider is on its right edge: drag it to size that column, double-click it to fit the
//!   content on screen. Past the last column the list is blank; wider than the pane, it scrolls
//!   sideways and the header moves with it.
//! - Drag a header onto another to move it; the order is saved in settings, app-wide like widths.
//!   Name stays first.
//! - Right-click the header to switch columns on and off (the same settings as the Settings page).
//! - Link target is read in the background (`Fs::read_link`, never on the UI thread) and cached
//!   per listing.

use rust_i18n::t;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arke_core::{Entry, Gen};
use gpui::{
    AnyElement, ClickEvent, Context, IntoElement, MouseButton, MouseDownEvent, Pixels,
    SharedString, Window, div, prelude::*, px, svg,
};

use super::view::{date_cell, size_cell};
use super::{AppEvent, Explorer, ViewMode, layout, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::prefs::Col;
use crate::theme::{self, UiFont as _};
use crate::workspace::TooltipBelowExt as _;

/// What the Link target column knows about one link.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkState {
    Reading,
    Known(SharedString),
    Unreadable,
}

/// Link targets of the folder shown, read in the background once per listing version.
#[derive(Default)]
pub(crate) struct LinkTargets {
    /// Listing generation the targets belong to; a new folder or refresh starts over.
    generation: Option<Gen>,
    /// Listing generation and version, and Details tree revision, last scanned for links.
    seen: Option<(Gen, u64, u64)>,
    targets: HashMap<Arc<str>, LinkState>,
}

/// Header being dragged to a new place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DraggedHeader(Col);

/// The header label following the cursor while it is dragged.
struct HeaderGhost(SharedString);

impl Render for HeaderGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .ui_font()
            .px(theme::S_8)
            .py(theme::S_4)
            .rounded(theme::R_8)
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .border_1()
            .border_color(theme::line())
            .text_size(theme::FS_12)
            .text_color(theme::text())
            .child(self.0.clone())
    }
}

/// Column divider being dragged: which column, in which pane (in split view both panes' headers
/// see the drag, and only the one it started in may size the column from its own edge).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DraggedColumn(Col, gpui::EntityId);

/// Nothing under the cursor while resizing (the column or the preview pane itself moves).
pub(super) struct ColumnDragView;

impl Render for ColumnDragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Explorer {
    /// The Details columns after Name that are switched on, left to right. A split pane is half
    /// as wide, so it always leaves Type out.
    pub(super) fn detail_columns(&self) -> impl Iterator<Item = Col> + use<> {
        let split = self.pane.split;
        crate::prefs::order()
            .into_iter()
            .filter(move |&col| col.shown() && !(split && col == Col::Type))
    }

    /// The fixed-width columns of the Details view or search results in this pane (`None` in the
    /// grid and the column view, whose rows span the list).
    pub(super) fn compute_columns(&self) -> Option<layout::Columns> {
        let others: Vec<Pixels> = if self.search_mode {
            vec![
                theme::COL_LOCATION,
                crate::prefs::modified_width(),
                crate::prefs::size_width(),
            ]
        } else if self.view_mode == ViewMode::Details {
            self.detail_columns().map(crate::prefs::width).collect()
        } else {
            return None;
        };
        Some(layout::columns(
            self.pane_width - layout::LIST_CHROME,
            crate::prefs::user_width(Col::Name),
            &others,
        ))
    }

    /// Width of `col` as laid out now (Name's follows the pane until the user sizes it).
    pub(super) fn column_width(&self, col: Col) -> Pixels {
        match (col, self.cols) {
            (Col::Name, Some(cols)) => cols.name,
            _ => crate::prefs::width(col),
        }
    }

    /// The scroll handle of the list under the header (search results or the folder's list).
    fn header_list(&self) -> gpui::ScrollHandle {
        let handle = if self.search_mode {
            &self.search_scroll
        } else {
            &self.scroll
        };
        handle.0.borrow().base_handle.clone()
    }

    /// The header's sideways scroll at this render and the list it follows, so painting can tell
    /// when the list clamped its scroll (the pane grew, a column narrowed) and draw once more to
    /// line the header up again. `None` while no header is drawn.
    pub(super) fn header_follow(&self) -> Option<(Pixels, gpui::ScrollHandle)> {
        self.header_x.get().map(|x| (x, self.header_list()))
    }

    // ───────────────────────── Header ─────────────────────────

    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Column resize released (anywhere): save to settings. Release triggers a redraw, which is
        // where we notice.
        if self.column_resized && !cx.has_active_drag() {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = this.update(cx, Self::save_column_widths);
            });
        }
        let scroll_x = self.header_list().offset().x.min(px(0.));
        self.header_x.set(Some(scroll_x));
        // The columns, laid out like the rows below (same padding and gaps) and moved with the
        // list when it scrolls sideways. What's right of them is blank, like the list.
        let row = div()
            .flex()
            .flex_none()
            .h_full()
            .items_center()
            .ml(scroll_x)
            .pl(layout::ROW_PAD_L)
            .pr(layout::ROW_PAD_R)
            .gap(layout::COL_GAP);
        let row = if self.search_mode {
            // Search results are sorted by relevance; no column sorting.
            row.child(
                self.name_header(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(t!("col.name").to_string())
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                div()
                    .w(theme::COL_LOCATION)
                    .flex_none()
                    .truncate()
                    .child(t!("col.location").to_string()),
            )
            .child(resizable(
                div()
                    .w(crate::prefs::modified_width())
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(t!("col.modified").to_string()),
                    ),
                Col::Modified,
                cx,
            ))
            .child(resizable(
                div()
                    .w(crate::prefs::size_width())
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(t!("col.size").to_string()),
                Col::Size,
                cx,
            ))
        } else {
            let name = self.header_cell(Col::Name, cx);
            let cells: Vec<AnyElement> = self
                .detail_columns()
                .map(|col| self.header_cell(col, cx))
                .collect();
            row.child(self.name_header(name, cx)).children(cells)
        };
        div()
            .id("details-header")
            .flex()
            .flex_none()
            .h(theme::H_HEADER)
            .mx(theme::S_8)
            .overflow_hidden()
            // Thin line between header and list to give the columns structure.
            .border_b_1()
            .border_color(theme::line())
            .text_size(theme::FS_12)
            .text_color(theme::text_muted())
            .child(row)
            // Right-click anywhere on the header, the blank part too: which columns to show.
            .when(!self.search_mode, |d| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, e: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_header_menu(e.position, window, cx);
                        window.prevent_default();
                    }),
                )
            })
            .into_any_element()
    }

    /// The Name column's header: the select-all checkbox, room for the icon (and the tree's
    /// chevron), then `label`, with the column's divider on its right edge.
    fn name_header(&self, label: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        resizable(
            div()
                .w(self.column_width(Col::Name))
                .flex_none()
                .h_full()
                .flex()
                .items_center()
                .gap(layout::COL_GAP)
                .when(self.show_checkboxes && !self.search_mode, |d| {
                    d.child(self.header_checkbox(cx))
                })
                .child(div().w(self.name_lead_width()).flex_none())
                .child(label),
            Col::Name,
            cx,
        )
        .into_any_element()
    }

    fn header_cell(&self, col: Col, cx: &mut Context<Self>) -> AnyElement {
        let sort_column = col.sort_column();
        let key = sort_column.and_then(|c| self.sort.key_of(c));
        let primary = key.is_some_and(|(ix, _)| ix == 0);
        let several = self.sort.key_count() > 1;
        // Sort direction: small arrow from the same icon set (up for ascending), and the key's
        // place when sorting by more than one column.
        let arrow = key.map(|(_, ascending)| {
            svg()
                .path(Icon::ChevronRight.path())
                .size(theme::ICON_SM)
                .flex_none()
                .text_color(theme::text_muted())
                .with_transformation(gpui::Transformation::rotate(gpui::radians(if ascending {
                    -std::f32::consts::FRAC_PI_2
                } else {
                    std::f32::consts::FRAC_PI_2
                })))
        });
        let place = key.filter(|_| several).map(|(ix, _)| {
            div()
                .flex_none()
                .text_size(theme::FS_11)
                .text_color(theme::text_subtle())
                .child((ix + 1).to_string())
        });
        let label: SharedString = col.label().into_owned().into();
        let cell = div()
            .id(("details-header", col as usize))
            .a11y_column_header(label.clone(), key.map(|(_, ascending)| ascending))
            .flex()
            .h_full()
            .items_center()
            .gap(theme::S_4)
            .when(key.is_some(), |d| d.text_color(theme::text()))
            .when(primary, |d| d.font_weight(theme::FW_STRONG))
            .child(div().min_w_0().truncate().child(label.clone()))
            .children(arrow)
            .children(place)
            .when_some(sort_column, |d, column| {
                d.cursor_pointer()
                    .hover(|s| s.text_color(theme::text()))
                    .tooltip_below(t!("col.sort_hint").into_owned())
                    .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                        let sort = if e.modifiers().shift {
                            this.sort.shift_toggled(column)
                        } else {
                            this.sort.toggled(column)
                        };
                        this.set_sort(sort, cx);
                    }))
            })
            // Reordering: drag any header but Name onto another.
            .when(col != Col::Name, |d| {
                d.on_drag(DraggedHeader(col), move |_, _, _, cx| {
                    cx.new(|_| HeaderGhost(label.clone()))
                })
            })
            .drag_over::<DraggedHeader>(move |style, dragged, _, _| {
                // A line where the column will land: before this one when moving left, after it
                // when moving right.
                match drop_side(dragged.0, col) {
                    Some(true) => style.border_r_2().border_color(theme::signal()),
                    Some(false) => style.border_l_2().border_color(theme::signal()),
                    None => style,
                }
            })
            .on_drop(cx.listener(move |_, dragged: &DraggedHeader, _, cx| {
                Self::move_column(dragged.0, col, cx);
            }));
        if col == Col::Name {
            // The label part of the Name header (see `name_header`).
            return cell.flex_1().min_w_0().into_any_element();
        }
        let cell = cell.w(crate::prefs::width(col)).flex_none();
        let cell = if col == Col::Size {
            cell.justify_end()
        } else {
            cell
        };
        resizable(cell, col, cx).into_any_element()
    }

    /// A header was dropped on `target`: move `moved` there and save the order (app-wide).
    fn move_column(moved: Col, target: Col, cx: &mut gpui::App) {
        let order = crate::prefs::reordered(crate::prefs::order(), moved, target);
        if order == crate::prefs::order() {
            return;
        }
        log::info!("column order: {order:?}");
        crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(|st| st.column_order = crate::prefs::order_keys(&order), cx);
        });
    }

    /// After resizing: save the current widths to settings (shared by all windows).
    pub(crate) fn save_column_widths(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.column_resized) {
            return;
        }
        log::info!(
            "column widths: {:?}",
            Col::ALL.map(crate::prefs::stored_width)
        );
        crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |st| {
                    for col in Col::ALL {
                        st.column_widths.set(col, crate::prefs::stored_width(col));
                    }
                },
                cx,
            );
        });
    }

    /// A divider was double-clicked: fit `col` to what the visible rows show in it (and its
    /// header), like Explorer; with nothing to measure, back to its default width.
    fn autofit_column(&mut self, col: Col, window: &Window, cx: &mut Context<Self>) {
        let width = self.content_width(col, window);
        log::info!("column fit: {col:?} {width:?}");
        crate::prefs::set_width(col, width);
        self.column_resized = true;
        self.save_column_widths(cx);
    }

    /// The width `col` needs for its header and the rows on screen (`None` without rows).
    fn content_width(&self, col: Col, window: &Window) -> Option<Pixels> {
        let measure = |text: &str, size: Pixels, weight: gpui::FontWeight| {
            if text.is_empty() {
                return px(0.);
            }
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
                .shape_line(SharedString::from(text.to_owned()), size, &[run], None)
                .width
        };
        let text_w = |text: &str, size: Pixels| measure(text, size, gpui::FontWeight::NORMAL);
        let widths = if self.search_mode {
            self.search_cell_widths(col, &text_w)?
        } else {
            self.detail_cell_widths(col, &text_w)?
        };
        if widths.is_empty() {
            return None;
        }
        // The header: its label (semibold, as the primary sort key draws it) with room for the
        // sort arrow and the key's place.
        let mut header = measure(&col.label(), theme::FS_12, theme::FW_STRONG)
            + (theme::S_4 + theme::ICON_SM) * 2.;
        if col == Col::Name {
            header += self.name_lead(self.tree_active().then_some(0));
        }
        // A little air so subpixel rounding never truncates the widest text.
        let widest = widths.into_iter().fold(header, Ord::max);
        Some((widest + theme::S_4).ceil())
    }

    /// What comes before the name text in the Name column: the checkbox, the tree's indent and
    /// chevron at `depth`, and the icon, each with its gap.
    fn name_lead(&self, depth: Option<u16>) -> Pixels {
        let mut w = theme::ICON + layout::COL_GAP;
        if self.show_checkboxes && !self.search_mode {
            w += theme::ICON + layout::COL_GAP;
        }
        if let Some(depth) = depth {
            w += theme::TREE_INDENT * f32::from(depth) + theme::TREE_CHEVRON + theme::S_4;
        }
        w
    }

    /// A date as its column shows it (full date-times, or short forms like "Today 07:49").
    fn date_text(&self, time: Option<std::time::SystemTime>, full: bool) -> String {
        let shell = &self.services.shell;
        time.map_or_default(|t| {
            if full {
                shell.format_time(t)
            } else {
                shell.format_time_short(t)
            }
        })
    }

    /// Widths of `col`'s cells in the Details rows on screen, measured with `text_w`.
    fn detail_cell_widths(
        &self,
        col: Col,
        text_w: &dyn Fn(&str, Pixels) -> Pixels,
    ) -> Option<Vec<Pixels>> {
        let view = self.view()?;
        let tree = self.tree_active();
        let mut widths = Vec::new();
        for row in self.rows_on_screen(&self.scroll) {
            let Some(ix) = self.item_at_row(row) else {
                continue;
            };
            let Some(entry) = view.get(ix) else {
                break;
            };
            widths.push(match col {
                Col::Name => {
                    let depth = tree.then(|| self.tree_row(ix).depth);
                    let name = super::view::shown_name(entry, self.show_extensions);
                    // Room for a compare badge ("Only here", "Newer"…) after the name.
                    let badge = self
                        .compare_mark(&entry.name)
                        .map_or(px(0.), |_| theme::S_12 + px(64.));
                    self.name_lead(depth) + text_w(&name, theme::FS_14) + badge
                }
                Col::Modified | Col::Created => {
                    let time = if col == Col::Modified {
                        entry.modified
                    } else {
                        entry.created
                    };
                    text_w(
                        &self.date_text(time, crate::prefs::full_dates()),
                        theme::FS_12,
                    )
                }
                Col::Size => {
                    let text = super::view::size_text(entry, self.folder_size(&entry.name)).0;
                    text_w(&text, theme::FS_14)
                }
                Col::Type => text_w(&arke_core::format::type_label(entry), theme::FS_12),
                Col::Extension => text_w(&shown_extension(entry), theme::FS_12),
                Col::Attributes => text_w(entry.attribute_letters(), theme::FS_12),
                Col::LinkTarget => match self.link_targets.targets.get(&entry.name) {
                    Some(LinkState::Known(target)) => text_w(target, theme::FS_12),
                    _ => px(0.),
                },
            });
        }
        Some(widths)
    }

    /// Widths of `col`'s cells in the search results on screen, measured with `text_w`.
    fn search_cell_widths(
        &self,
        col: Col,
        text_w: &dyn Fn(&str, Pixels) -> Pixels,
    ) -> Option<Vec<Pixels>> {
        let results = self.search_results.as_ref()?;
        let mut widths = Vec::new();
        for ix in self.rows_on_screen(&self.search_scroll) {
            let Some(hit) = results.hits.get(ix) else {
                break;
            };
            let entry = &hit.entry;
            widths.push(match col {
                Col::Name => self.name_lead(None) + text_w(&entry.name, theme::FS_14),
                Col::Modified => text_w(&self.date_text(entry.modified, true), theme::FS_12),
                Col::Size if !entry.is_dir_like() => {
                    text_w(&arke_core::format::format_size(entry.size), theme::FS_14)
                }
                _ => px(0.),
            });
        }
        Some(widths)
    }

    /// Indexes of the list rows (group headers included) that are on screen in the list
    /// scrolled by `handle`.
    fn rows_on_screen(&self, handle: &gpui::UniformListScrollHandle) -> std::ops::Range<usize> {
        let row = self.row_height();
        let top = -handle.0.borrow().base_handle.offset().y;
        let height = self.list_bounds.get().size.height;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "row counts on screen, never negative"
        )]
        let (first, last) = (
            (top / row).floor().max(0.) as usize,
            ((top + height) / row).ceil().max(0.) as usize,
        );
        first..last
    }

    // ───────────────────────── Cells ─────────────────────────

    /// A Details row's cells after Name, in the header's order.
    pub(super) fn detail_cells(
        &self,
        entry: &Entry,
        shell: &dyn crate::shell::Shell,
    ) -> Vec<AnyElement> {
        self.detail_columns()
            .map(|col| match col {
                Col::Modified | Col::Created => {
                    let time = if col == Col::Modified {
                        entry.modified
                    } else {
                        entry.created
                    };
                    date_cell(time, crate::prefs::width(col), shell).into_any_element()
                }
                Col::Size => size_cell(entry, self.folder_size(&entry.name)).into_any_element(),
                Col::Type => text_cell(col, arke_core::format::type_label(entry)),
                Col::Extension => text_cell(col, shown_extension(entry)),
                Col::Attributes => text_cell(col, entry.attribute_letters()),
                Col::LinkTarget => self.link_target_cell(entry),
                // Never listed after Name.
                Col::Name => div().into_any_element(),
            })
            .collect()
    }

    fn link_target_cell(&self, entry: &Entry) -> AnyElement {
        let state = entry
            .is_link()
            .then(|| self.link_targets.targets.get(&entry.name))
            .flatten();
        match state {
            Some(LinkState::Known(target)) => text_cell(Col::LinkTarget, target.clone()),
            Some(LinkState::Reading) => div()
                .w(crate::prefs::width(Col::LinkTarget))
                .flex_none()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child("…")
                .into_any_element(),
            Some(LinkState::Unreadable) | None => text_cell(Col::LinkTarget, ""),
        }
    }

    // ───────────────────────── Link targets ─────────────────────────

    /// Before each render: with the Link target column on, read the targets of links in a new or
    /// changed listing on a background thread.
    pub(super) fn sync_link_targets(&mut self) {
        if !Col::LinkTarget.shown() || self.view_mode != ViewMode::Details || self.search_mode {
            return;
        }
        let Some(model) = &self.model else { return };
        let seen = (model.generation, model.version, self.tree_revision());
        if self.link_targets.seen == Some(seen) {
            return;
        }
        let links = &mut self.link_targets;
        links.seen = Some(seen);
        if links.generation != Some(model.generation) {
            links.generation = Some(model.generation);
            links.targets.clear();
        }
        // The Details tree's nested rows too (named by relative path, so `dir.join` still works).
        let entries = self.tree_view().unwrap_or_else(|| model.entries.clone());
        let links = &mut self.link_targets;
        let missing: Vec<Arc<str>> = entries
            .iter()
            .filter(|e| e.is_link() && !links.targets.contains_key(&e.name))
            .map(|e| e.name.clone())
            .collect();
        if missing.is_empty() {
            return;
        }
        for name in &missing {
            links.targets.insert(name.clone(), LinkState::Reading);
        }
        let fs = self.services.fs.clone();
        let tx = self.events_tx.clone();
        let dir = model.path.clone();
        let generation = model.generation;
        spawn_thread("link-targets", move || {
            let found = missing
                .into_iter()
                .map(|name| {
                    let target = fs.read_link(&dir.join(&*name)).ok();
                    (name, target)
                })
                .collect();
            let _ = tx.send(AppEvent::LinkTargets { generation, found });
        });
    }

    pub(super) fn handle_link_targets(
        &mut self,
        generation: Gen,
        found: Vec<(Arc<str>, Option<PathBuf>)>,
        cx: &mut Context<Self>,
    ) {
        if self.link_targets.generation != Some(generation) {
            return;
        }
        for (name, target) in found {
            let state = target.map_or(LinkState::Unreadable, |t| {
                LinkState::Known(t.display().to_string().into())
            });
            self.link_targets.targets.insert(name, state);
        }
        cx.notify();
    }

    /// Tests: the Link target column's text for `name` (`None` while reading or unknown).
    #[cfg(test)]
    pub(crate) fn link_target_for_test(&self, name: &str) -> Option<String> {
        match self.link_targets.targets.get(name) {
            Some(LinkState::Known(t)) => Some(t.to_string()),
            _ => None,
        }
    }

    /// Tests: each header column's left edge and width, in window coordinates.
    #[cfg(test)]
    pub(crate) fn header_columns_for_test(&self) -> Vec<(Col, Pixels, Pixels)> {
        let mut left = self.list_bounds.get().left()
            + theme::S_8
            + layout::ROW_PAD_L
            + self.header_x.get().unwrap_or_default();
        std::iter::once(Col::Name)
            .chain(self.detail_columns())
            .map(|col| {
                let width = self.column_width(col);
                let at = left;
                left += width + layout::COL_GAP;
                (col, at, width)
            })
            .collect()
    }

    /// Tests: the Details row's width (`None` where rows span the list).
    #[cfg(test)]
    pub(crate) fn row_width_for_test(&self) -> Option<Pixels> {
        self.cols.map(|c| c.row)
    }

    /// Tests: header labels left to right, with each sort key's place and direction.
    #[cfg(test)]
    pub(crate) fn header_for_test(&self) -> Vec<(Col, Option<(usize, bool)>)> {
        std::iter::once(Col::Name)
            .chain(self.detail_columns())
            .map(|col| (col, col.sort_column().and_then(|c| self.sort.key_of(c))))
            .collect()
    }
}

/// Which edge of `target` the drop line goes on for `moved`: `Some(true)` right, `Some(false)`
/// left, `None` when dropping it on itself.
fn drop_side(moved: Col, target: Col) -> Option<bool> {
    if moved == target {
        return None;
    }
    if target == Col::Name {
        return Some(true);
    }
    let order = crate::prefs::order();
    let at = |c: Col| order.iter().position(|o| *o == c);
    Some(at(moved) < at(target))
}

/// Small muted text in a fixed-width column.
fn text_cell(col: Col, text: impl Into<SharedString>) -> AnyElement {
    div()
        .w(crate::prefs::width(col))
        .flex_none()
        .truncate()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .child(text.into())
        .into_any_element()
}

/// The extension as written in the name, with its dot (".JPG"); empty for folders and names
/// without one.
fn shown_extension(entry: &Entry) -> SharedString {
    if entry.extension().is_empty() {
        return SharedString::default();
    }
    entry
        .name
        .rfind('.')
        .map_or_default(|dot| SharedString::from(entry.name[dot..].to_owned()))
}

/// `cell` with its column's divider on its right edge: dragging it sizes `col` (the columns to its
/// right move along), double-clicking fits it to its content.
fn resizable<E: ParentElement + InteractiveElement + Styled>(
    cell: E,
    col: Col,
    cx: &mut Context<Explorer>,
) -> E {
    cell.relative()
        .child(column_resize_handle(col, cx))
        .on_drag_move(
            cx.listener(move |this, e: &gpui::DragMoveEvent<DraggedColumn>, _, cx| {
                let dragged = *e.drag(cx);
                if dragged.0 == col && dragged.1 == cx.entity_id() {
                    // The column's left edge stays put; the divider (half a gap past the right
                    // edge) follows the cursor.
                    let width = e.event.position.x - e.bounds.left() - layout::COL_GAP / 2.;
                    crate::prefs::set_width(col, Some(width));
                    this.column_resized = true;
                    cx.notify();
                }
            }),
        )
}

/// Divider on a column header's right edge, in the gap to the next column (or the row's padding
/// after the last one): drag to resize, double-click to fit the content.
fn column_resize_handle(col: Col, cx: &mut Context<Explorer>) -> impl IntoElement {
    div()
        .id(("col-resize", col as usize))
        .absolute()
        .top_0()
        .bottom_0()
        // Columns are 12 px apart: the divider sits in the middle, 8 px wide for an easy grab.
        .right(px(-10.))
        .w(px(8.))
        .flex()
        .justify_center()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .group("col-resize")
        .child(
            div()
                .w(px(1.))
                .h(px(14.))
                .my_auto()
                .bg(theme::line())
                .group_hover("col-resize", |s| s.bg(theme::text_subtle())),
        )
        .on_drag(DraggedColumn(col, cx.entity_id()), |d, _, _, cx| {
            log::info!("column resize start: {:?}", d.0);
            cx.new(|_| ColumnDragView)
        })
        .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
            if e.click_count() >= 2 {
                this.autofit_column(col, window, cx);
            }
        }))
}

/// Explorer's shortcut arrow for symlinks and junctions: a small framed arrow over the icon's
/// bottom-left corner, `icon` being the icon's size.
pub(crate) fn with_link_badge(visual: AnyElement, icon: Pixels) -> AnyElement {
    let size = (icon * 0.4).max(px(9.));
    let layer = |path: Icon, color: gpui::Rgba| {
        svg()
            .absolute()
            .top_0()
            .left_0()
            .size(size)
            .path(path.path())
            .text_color(color)
    };
    div()
        .relative()
        .flex_none()
        .child(visual)
        .child(
            div()
                .absolute()
                .left(px(-1.))
                .bottom(px(-1.))
                .size(size)
                .child(layer(Icon::LinkBadgeFill, theme::surface()))
                .child(layer(Icon::LinkBadgeFrame, theme::line_strong()))
                .child(layer(Icon::LinkBadgeArrow, theme::signal())),
        )
        .into_any_element()
}
