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
use std::rc::Rc;
use std::sync::Arc;

use arke_core::{Entry, Gen, SearchColumn};
use gpui::{
    AnyElement, ClickEvent, Context, IntoElement, MouseButton, MouseDownEvent, Pixels,
    SharedString, Window, div, prelude::*, px, svg,
};

use super::view::{date_cell, size_cell};
use super::{AppEvent, Explorer, ViewMode, layout, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::prefs::Col;
use crate::splitter::{DragEnd, Look, Splitter};
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
            .px(theme::s_8())
            .py(theme::s_4())
            .rounded(theme::r_8())
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .border_1()
            .border_color(theme::line())
            .text_size(theme::fs_12())
            .text_color(theme::text())
            .child(self.0.clone())
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
    /// grid and the column view, whose rows span the list). `held_name` keeps an automatic Name
    /// width as it is (see `before_render`).
    pub(super) fn compute_columns(&self, held_name: Option<Pixels>) -> Option<layout::Columns> {
        let others: Vec<Pixels> = if self.search_mode {
            vec![
                theme::col_location(),
                crate::prefs::modified_width(),
                crate::prefs::size_width(),
            ]
        } else if self.view_mode == ViewMode::Details {
            self.detail_columns().map(crate::prefs::width).collect()
        } else {
            return None;
        };
        Some(layout::columns(
            self.pane_width - layout::list_chrome(),
            crate::prefs::user_width(Col::Name).or(held_name),
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
    /// Whether the list can still scroll sideways toward its start (`toward_start`) or its end:
    /// only Details columns wider than the pane scroll that way.
    pub(crate) fn can_scroll_sideways(&self, toward_start: bool) -> bool {
        if self.cols.is_none() {
            return false;
        }
        let handle = self.header_list();
        let (at, max) = (handle.offset().x, handle.max_offset().x);
        if toward_start {
            at < px(-0.5)
        } else {
            -at < max - px(0.5)
        }
    }

    fn header_list(&self) -> gpui::ScrollHandle {
        let handle = if self.search_mode {
            &self.search_scroll
        } else {
            &self.scroll
        };
        handle.0.borrow().base_handle.clone()
    }

    // ───────────────────────── Header ─────────────────────────

    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The columns, laid out like the rows below (same padding and gaps). What's right of them
        // is blank, like the list.
        let row = div()
            .flex()
            .flex_none()
            .h_full()
            .items_center()
            .pl(layout::row_pad_l())
            .pr(layout::row_pad_r())
            .gap(layout::col_gap())
            // Thin line between header and list to give the columns structure. It belongs to the
            // row, so it scrolls with the columns like the rows' own washes do; the minimum width
            // keeps it running to the pane's right edge while everything fits.
            .border_b_1()
            .border_color(theme::line())
            .min_w(self.pane_width);
        let row = if self.search_mode {
            // Search results start in relevance order; a click on a header sorts them.
            row.child(
                self.name_header(
                    self.search_header_cell(SearchColumn::Name, t!("col.name").into_owned(), cx)
                        .flex_1()
                        .min_w_0()
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                self.search_header_cell(
                    SearchColumn::Location,
                    t!("col.location").into_owned(),
                    cx,
                )
                .w(theme::col_location())
                .flex_none(),
            )
            .child(
                self.resizable(
                    self.search_header_cell(
                        SearchColumn::Modified,
                        t!("col.modified").into_owned(),
                        cx,
                    )
                    .w(crate::prefs::modified_width())
                    .flex_none(),
                    Col::Modified,
                    cx,
                ),
            )
            .child(
                self.resizable(
                    self.search_header_cell(SearchColumn::Size, t!("col.size").into_owned(), cx)
                        .w(crate::prefs::size_width())
                        .flex_none()
                        .justify_end(),
                    Col::Size,
                    cx,
                ),
            )
        } else {
            let name = self.header_cell(Col::Name, cx);
            let cells: Vec<AnyElement> = self
                .detail_columns()
                .map(|col| self.header_cell(col, cx))
                .collect();
            row.child(self.name_header(name, cx)).children(cells)
        };
        let header = div()
            .id("details-header")
            .debug_selector(|| "details-header-box".to_owned())
            .flex()
            .flex_none()
            .h(theme::h_header())
            // Clipped where the rows are, at the list's left edge and 8 px short of its right one,
            // so the columns slide into the inset together with the rows; they start at the
            // inset when not scrolled.
            .pl(theme::list_inset())
            .mr(theme::s_8())
            .overflow_hidden()
            .text_size(theme::fs_12())
            .text_color(theme::text_muted())
            // Moved with the list when it scrolls sideways (see `SideScroll`).
            .child(SideScroll {
                child: row.into_any_element(),
                handle: self.header_list(),
                seen: Rc::clone(&self.header_x),
                content: None,
            })
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
            .into_any_element();
        // The offset is clamped before anything reads it (see `SideScroll`).
        SideScroll {
            child: header,
            handle: self.header_list(),
            seen: Rc::default(),
            content: self
                .cols
                .map(|c| c.row + theme::list_inset() + layout::list_gutter()),
        }
        .into_any_element()
    }

    /// The Name column's header: the select-all checkbox, room for the icon (and the tree's
    /// chevron), then `label`, with the column's divider on its right edge.
    fn name_header(&self, label: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        self.resizable(
            div()
                .w(self.column_width(Col::Name))
                .flex_none()
                .h_full()
                .flex()
                .items_center()
                .gap(layout::col_gap())
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

    /// A header of the search results: the label, and the sort arrow when the results are ordered
    /// by it. Clicking sorts (see `click_search_header`). The caller sets the width.
    fn search_header_cell(
        &self,
        column: SearchColumn,
        label: String,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.search_sort.filter(|s| s.column == column);
        let label: SharedString = label.into();
        div()
            .id(("search-header", column as usize))
            .debug_selector(move || format!("search-header-{}", column.key()))
            .a11y_column_header(label.clone(), active.map(|s| s.ascending))
            .flex()
            .h_full()
            .items_center()
            .gap(theme::s_4())
            .when(active.is_some(), |d| {
                d.text_color(theme::text()).font_weight(theme::FW_STRONG)
            })
            .child(div().min_w_0().truncate().child(label))
            .children(active.map(|s| {
                // Same arrow as the Details headers: up for ascending.
                svg()
                    .path(Icon::ChevronRight.path())
                    .size(theme::icon_sm())
                    .flex_none()
                    .text_color(theme::text_muted())
                    .with_transformation(gpui::Transformation::rotate(gpui::radians(
                        if s.ascending {
                            -std::f32::consts::FRAC_PI_2
                        } else {
                            std::f32::consts::FRAC_PI_2
                        },
                    )))
            }))
            .cursor_pointer()
            .hover(|s| s.text_color(theme::text()))
            .tooltip_below(t!("col.sort_hint").into_owned())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.click_search_header(column, cx);
            }))
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
                .size(theme::icon_sm())
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
                .text_size(theme::fs_11())
                .text_color(theme::text_subtle())
                .child((ix + 1).to_string())
        });
        let label: SharedString = col.label().into_owned().into();
        let cell = div()
            .id(("details-header", col as usize))
            .debug_selector(move || format!("details-header-{}", col.key()))
            .a11y_column_header(label.clone(), key.map(|(_, ascending)| ascending))
            .flex()
            .h_full()
            .items_center()
            .gap(theme::s_4())
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
        self.resizable(cell, col, cx).into_any_element()
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
            text_width(window, text, size, weight)
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
        let mut header = measure(&col.label(), theme::fs_12(), theme::FW_STRONG)
            + (theme::s_4() + theme::icon_sm()) * 2.;
        if col == Col::Name {
            header += self.name_lead(self.tree_active().then_some(0));
        }
        // A little air so subpixel rounding never truncates the widest text.
        let widest = widths.into_iter().fold(header, Ord::max);
        Some((widest + theme::s_4()).ceil())
    }

    /// What comes before the name text in the Name column: the checkbox, the tree's indent and
    /// chevron at `depth`, and the icon, each with its gap.
    pub(super) fn name_lead(&self, depth: Option<u16>) -> Pixels {
        let mut w = theme::icon() + layout::col_gap();
        if self.show_checkboxes && !self.search_mode {
            w += theme::icon() + layout::col_gap();
        }
        if let Some(depth) = depth {
            w += theme::tree_indent() * f32::from(depth) + theme::tree_chevron() + theme::s_4();
        }
        w
    }

    /// A date as its column shows it (full date-times, or short forms like "Today 07:49").
    pub(super) fn date_text(&self, time: Option<std::time::SystemTime>, full: bool) -> String {
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
                    let name = self.shown_name_of(entry);
                    // Room for a compare badge ("Only here", "Newer"…) after the name.
                    let badge = self
                        .compare_mark(&entry.name)
                        .map_or(px(0.), |_| theme::s_12() + crate::theme::scaled_px(64.));
                    self.name_lead(depth) + text_w(&name, theme::fs_14()) + badge
                }
                Col::Modified | Col::Created => {
                    let time = if col == Col::Modified {
                        entry.modified
                    } else {
                        entry.created
                    };
                    text_w(
                        &self.date_text(time, crate::prefs::full_dates()),
                        theme::fs_12(),
                    )
                }
                Col::Size => {
                    let text = super::view::size_text(entry, self.folder_size(&entry.name)).0;
                    text_w(&text, theme::fs_14())
                }
                Col::Type => text_w(&arke_core::format::type_label(entry), theme::fs_12()),
                Col::Extension => text_w(&shown_extension(entry), theme::fs_12()),
                Col::Attributes => text_w(entry.attribute_letters(), theme::fs_12()),
                Col::LinkTarget => match self.link_targets.targets.get(&entry.name) {
                    Some(LinkState::Known(target)) => text_w(target, theme::fs_12()),
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
        // "Similar results" is a row of its own (`explorer_similar.rs`), not a hit.
        let similar = self.similar_row().map(|(at, _)| at);
        for row in self.rows_on_screen(&self.search_scroll) {
            let Some(ix) = super::similar::hit_at_row(row, similar) else {
                continue;
            };
            let Some(hit) = results.hits.get(ix) else {
                break;
            };
            let entry = &hit.entry;
            widths.push(match col {
                Col::Name => self.name_lead(None) + text_w(&entry.name, theme::fs_14()),
                Col::Modified => text_w(&self.date_text(entry.modified, true), theme::fs_12()),
                Col::Size if !entry.is_dir_like() => {
                    text_w(&arke_core::format::format_size(entry.size), theme::fs_14())
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
        // Row counts on screen, never negative.
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
                .text_size(theme::fs_12())
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
        let entries = self
            .tree_view()
            .unwrap_or_else(|| Arc::clone(&model.entries));
        let links = &mut self.link_targets;
        let missing: Vec<Arc<str>> = entries
            .iter()
            .filter(|e| e.is_link() && !links.targets.contains_key(&e.name))
            .map(|e| Arc::clone(&e.name))
            .collect();
        if missing.is_empty() {
            return;
        }
        for name in &missing {
            links.targets.insert(Arc::clone(name), LinkState::Reading);
        }
        let fs = Arc::clone(&self.services.fs);
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
            + theme::list_inset()
            + layout::row_pad_l()
            + self.header_x.get().unwrap_or_default();
        std::iter::once(Col::Name)
            .chain(self.detail_columns())
            .map(|col| {
                let width = self.column_width(col);
                let at = left;
                left += width + layout::col_gap();
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
        .debug_selector(move || format!("cell-{}", col.key()))
        .w(crate::prefs::width(col))
        .flex_none()
        .truncate()
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .child(text.into())
        .into_any_element()
}

/// Width of `text` drawn in the UI font at `size` and `weight`.
pub(super) fn text_width(
    window: &Window,
    text: &str,
    size: Pixels,
    weight: gpui::FontWeight,
) -> Pixels {
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

impl Explorer {
    /// `cell` with its column's divider on its right edge: dragging it sizes `col` (the columns
    /// to its right move along), double-clicking fits it to its content.
    fn resizable<E: ParentElement + Styled>(&self, cell: E, col: Col, cx: &mut Context<Self>) -> E {
        cell.relative().child(self.column_resize_handle(col, cx))
    }

    /// Divider on a column header's right edge, in the gap to the next column (or the row's
    /// padding after the last one): drag to resize, double-click to fit the content.
    fn column_resize_handle(&self, col: Col, cx: &mut Context<Self>) -> impl IntoElement {
        // Columns are 12 px apart: the divider sits in the middle, 8 px wide for an easy grab.
        let hit = div()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(-10.))
            .w(crate::theme::scaled_px(8.))
            .flex()
            .justify_center();
        let line = div()
            .w(px(1.))
            .h(crate::theme::scaled_px(14.))
            .my_auto()
            .bg(theme::line());
        let dragging = self.column_drag.is_some_and(|d| d.0 == col);
        Splitter::new(("col-resize", col as usize), gpui::CursorStyle::ResizeLeftRight)
            .look(Look::Subtle)
            .dragging(dragging)
            .on_start(cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                log::info!("column resize start: {col:?}");
                this.activate_on_press(cx);
                this.column_drag = Some((col, e.position.x, this.column_width(col)));
            }))
            .on_double_click(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                this.activate_on_press(cx);
                this.autofit_column(col, window, cx);
            }))
            // The column's left edge stays put; its right edge moves as far as the cursor did since
            // the press, so it neither jumps to the cursor nor lags behind it.
            .on_drag(cx.listener(move |this, e: &gpui::MouseMoveEvent, _, cx| {
                if let Some((c, from, start)) = this.column_drag
                    && c == col
                {
                    crate::prefs::set_width(col, Some(start + e.position.x - from));
                    this.column_resized = true;
                    cx.notify();
                }
            }))
            .on_end(cx.listener(|this, _: &DragEnd, _, cx| {
                if this.column_drag.take().is_some() {
                    this.save_column_widths(cx);
                    cx.notify();
                }
            }))
            .render(hit, line)
    }
}

/// Explorer's shortcut arrow for symlinks and junctions: a small framed arrow over the icon's
/// bottom-left corner, `icon` being the icon's size.
pub(crate) fn with_link_badge(visual: AnyElement, icon: Pixels) -> AnyElement {
    let size = (icon * 0.4).max(crate::theme::scaled_px(9.));
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

/// Sideways scrolling of the Details header, in two parts that share the list's `ScrollHandle`.
///
/// With `content` set it wraps the whole header and, before anything is laid out under it,
/// clamps the handle's offset to the range the list will have this frame: `content` (the row plus
/// the inset before it and the blank strip after it) less the header's own width, which is the
/// list's width less its right padding. Without `content` it wraps the row and moves it by the handle's
/// offset, which is therefore already the one the list will use: header and rows can never differ,
/// whatever a wheel overshot or however the columns and the pane just changed, and the element
/// tree keeps its order (the header stays before the rows for screen readers and focus).
struct SideScroll {
    child: AnyElement,
    handle: gpui::ScrollHandle,
    /// Receives the offset the row was drawn at (tests read it).
    seen: Rc<std::cell::Cell<Option<Pixels>>>,
    content: Option<Pixels>,
}

impl IntoElement for SideScroll {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for SideScroll {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        (): &mut (),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        if let Some(content) = self.content {
            // Rounded like GPUI rounds a scroll range, so both clamp to the same value.
            let range = ((content - bounds.size.width).max(px(0.)) * 100.).round() / 100.;
            let at = self.handle.offset();
            if at.x < -range || at.x > px(0.) {
                self.handle
                    .set_offset(gpui::point(at.x.clamp(-range, px(0.)), at.y));
            }
            self.child.prepaint(window, cx);
            return;
        }
        let x = self.handle.offset().x.min(px(0.));
        self.seen.set(Some(x));
        window.with_element_offset(gpui::point(x, px(0.)), |window| {
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        (): &mut (),
        (): &mut (),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        self.child.paint(window, cx);
    }
}
