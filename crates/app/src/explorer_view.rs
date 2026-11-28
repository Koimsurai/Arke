//! Rendering for `Explorer`.
//!
//! - **Top bar** is drawn on the frame (transparent): back/forward/up, path, filter box, drag area,
//!   window buttons.
//! - **Paper** (`surface` + `elev_2`): context bar, column headers, list; toasts sit in the paper's
//!   bottom-right.
//!
//! Only `theme` tokens are allowed (enforced by `tests::ui_code_uses_theme_tokens`).

use rust_i18n::t;
use std::cell::Cell;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::Entry;
use arke_core::NavKind;
use arke_core::SortColumn;
use arke_core::filetype::{FileKind, file_kind};
use arke_core::format::{format_count, format_size};
use arke_core::path::NameError;
use arke_core::search::MAX_HITS;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, ExternalPaths, MouseButton, MouseDownEvent,
    MouseUpEvent, NavigationDirection, ObjectFit, Pixels, RenderImage, SharedString, Window,
    WindowControlArea, anchored, deferred, div, img, prelude::*, px, svg, uniform_list,
};

use super::fold::{self, Info, RowLayout, RowParts, SelectionLayout, SelectionParts, Summary};
use super::layout;
use super::ops::DeletePermanently;
use super::toolbar::ToolbarStop as _;
use super::{
    CopyFiles, CutFiles, DeleteFiles, ErrorAction, Escape, Explorer, ExplorerEvent, FocusOtherPane,
    GoBack, GoForward, GoUp, NewFolder, OpenSelected, PasteFiles, RenameFile, ViewColumns,
    ViewDetails, ViewMode,
};
use super::{DragPreview, DraggedFiles, drop_external_into, drop_into};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::media::MediaKey;
use crate::op_center::ToastDelays;
use crate::press_guard::is_double_click;
use crate::text_input::TextInput;
use crate::theme::{self, UiFont as _};
use crate::workspace::TooltipBelowExt as _;

/// Empty space after the last row (about three rows) so a long, fully scrolled list still has room
/// to right-click for the folder menu, click to deselect, or drop files into the folder.
///
/// It's added as blank trailing list items (see [`tail_items`]), not as bottom padding:
/// `uniform_list` only draws rows inside its padded area, so padding would leave a blank band at
/// the bottom of the list at every scroll position, not just after the last row.
fn list_tail() -> gpui::Pixels {
    crate::theme::scaled_px(96.)
}

/// Number of blank trailing items of height `item` that make up [`list_tail()`]. The row callbacks
/// draw nothing for indexes past the real items, so these are just empty space.
fn tail_items(item: Pixels) -> usize {
    if item <= px(0.) {
        return 0;
    }
    // A handful at most (3 rows, or one tile row), so the cast can't truncate.
    (list_tail() / item).ceil() as usize
}

/// How long regular toasts stay before closing
const NOTICE_TTL: Duration = Duration::from_secs(5);
/// A notice with Undo stays longer, so there's time to reach the button.
const NOTICE_UNDO_TTL: Duration = Duration::from_secs(10);
/// Delay keys of the progress toasts (see [`ToastDelays`]).
const OPS_TOAST: &str = "ops";
const EXTRACT_TOAST: &str = "extract";

/// Widest the rename problem message gets before wrapping.
fn rename_balloon_w() -> gpui::Pixels {
    crate::theme::scaled_px(440.)
}
/// Above this many selected items the total size isn't computed (avoids a long walk on every
/// render)
pub(super) const SIZE_SUM_MAX: usize = 5_000;
/// Below this pane width context bar buttons show icons only
fn compact_bar() -> gpui::Pixels {
    crate::theme::scaled_px(860.)
}
/// Below this pane width "Copy to/Move to the other side" show icons only (split view)
fn peer_labels_min() -> gpui::Pixels {
    crate::theme::scaled_px(600.)
}
/// Scrollbar width (subtracted when computing grid columns)
fn scrollbar_gutter() -> gpui::Pixels {
    crate::theme::scaled_px(12.)
}

impl Explorer {
    // ───────────────────────── Top bar (on the frame) ─────────────────────────

    /// The search box at `width` (see [`super::top_bar`]). At the width of a button it shows only
    /// its magnifier until clicked (or Ctrl+F).
    pub(super) fn render_filter_box(
        &mut self,
        state: layout::SearchBox,
        width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focused = state == layout::SearchBox::Typing;
        let active = !self.query.is_empty() || self.search_mode;
        let collapsed = width <= theme::h_ctl();
        // Not in use but holding text: it shows from its start and ends in "…"; the tooltip has
        // all of it.
        let summary = !focused && !collapsed && !self.query.is_empty();
        let filter_box = div()
            .id("filter-box")
            .debug_selector(|| "filter-box".to_owned())
            .key_context("FilterBar")
            .on_action(cx.listener(Self::filter_to_list))
            .w_full()
            .map(|d| Self::search_box_presses(d, collapsed, summary, cx))
            .overflow_hidden()
            .when(collapsed, |d| {
                d.tooltip_below(if self.query.is_empty() {
                    t!("filter.placeholder").into_owned()
                } else {
                    self.query.clone()
                })
            })
            .when(summary, |d| d.tooltip_below(self.query.clone()))
            .h(theme::h_ctl())
            .flex()
            .items_center()
            .gap(theme::s_6())
            .when(collapsed, gpui::Styled::justify_center)
            .when(!collapsed, |d| d.pl(theme::s_12()).pr(theme::s_4()))
            .rounded(theme::r_8())
            .border_1()
            .when(focused, |d| {
                d.bg(theme::surface()).border_color(theme::signal_line())
            })
            .when(!focused, |d| {
                d.bg(theme::wash()).border_color(if active {
                    theme::line_strong()
                } else {
                    theme::clear()
                })
            })
            .child(
                svg()
                    .path(Icon::Search.path())
                    .size(theme::icon())
                    .flex_none()
                    .text_color(if active {
                        theme::text_muted()
                    } else {
                        theme::text_subtle()
                    }),
            )
            // Clip overflowing text so it doesn't draw under the scope chip.
            .when(!collapsed, |d| {
                d.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .when(summary, |d| {
                            d.relative()
                                .truncate()
                                .text_color(theme::text())
                                .child(self.search_summary_measure())
                                .child(SharedString::from(self.query.clone()))
                        })
                        .when(!summary, |d| d.child(self.filter_input.clone())),
                )
            })
            .when(!self.query.is_empty() && !collapsed, |d| {
                d.child(Self::filter_clear(cx))
            })
            // Scope other than "this folder": show it on the right.
            .when(!collapsed, |d| d.children(self.render_scope_chip(focused, cx)));
        let filter_box = Self::bind_scope_actions(filter_box, cx);
        div()
            .relative()
            .flex_none()
            .w(width)
            .min_w(theme::h_ctl())
            .child(filter_box)
            // While the box has focus, the scope strip shows below (this folder / subfolders / this
            // PC). It collapses once typing starts so it doesn't cover the list (Tab or the scope
            // chip reopens it).
            // Not while the box's edit menu is open (right-click on the empty box): the menu is
            // what was asked for.
            .when(
                focused
                    && !self.panel_dismissed
                    && (self.query.is_empty() || self.scope_strip_open)
                    && !self.filter_input.read(cx).menu_open(),
                |d| d.child(self.render_scope_strip(width, window, cx)),
            )
    }

    /// The × in the filter box: clear the text and go back to "this folder".
    fn filter_clear(cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("filter-clear")
            .a11y_button(t!("a11y.clear_filter"))
            .size(theme::s_20())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_4())
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme::icon_sm())
                    .text_color(theme::text_muted()),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.reset_filter(cx);
                window.focus(&this.list_focus, cx);
                cx.notify();
            }))
    }

    // ───────────────────────── Context bar ─────────────────────────

    /// Shorter texts in the pane row (in split view or when the pane is narrow).
    pub(super) fn compact_bar(&self) -> bool {
        self.pane.split || self.bar_width < compact_bar()
    }

    /// With nothing selected, the pane row's end before the view buttons: the folder's item count
    /// (or the search's), then paste and new folder. It gives way to the path bar: the count is
    /// left out (`count` false) when the row is short of room (#474); it only tells, so it goes
    /// nowhere else.
    pub(super) fn render_folder_actions(
        &self,
        compact: bool,
        count: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .relative()
            .child(self.info_probe())
            .flex()
            .min_w_0()
            // Squeezed past its buttons (a small window at a large scale): cut off, rather than
            // drawn over the view buttons.
            .overflow_hidden()
            .items_center()
            .gap(theme::s_4())
            .when(count, |d| {
                d.child(
                    div()
                        .debug_selector(|| "pane-info".to_owned())
                        .min_w_0()
                        .truncate()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(self.info_parts(cx).join(" · ")),
                )
            })
            .child(self.render_folder_buttons(compact, cx))
            .into_any_element()
    }

    /// The folder's info that never folds: the comparison's chip, paste and new folder, measured
    /// for the row (#474).
    fn render_folder_buttons(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        // A server's share list takes no new items.
        let loaded = self.model.is_some() && !lists_shares(self.current_path());
        let can_paste = loaded && !self.search_mode && self.services.shell.clipboard_has_files();
        div()
            .relative()
            .child(slot_probe(&self.rest_slot))
            .flex()
            .min_w_0()
            .items_center()
            .gap(theme::s_4())
            .children(self.render_compare_chip(cx))
            .when(can_paste, |d| {
                d.child(action_button(
                    "paste",
                    Icon::Paste,
                    t!("ctx.paste"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| this.paste_files(&PasteFiles, w, cx)),
                ))
            })
            // Regular button, not accent: new folder isn't common enough to be the loudest thing on
            // screen.
            .when(loaded && !self.search_mode, |d| {
                d.child(action_button(
                    "new-folder",
                    Icon::FolderPlus,
                    t!("ctx.new_folder"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| this.new_folder(&NewFolder, w, cx)),
                ))
            })
    }

    /// View tools: sort, details/icons/columns with the view options flyout, preview pane, split.
    /// The first ones fold as `fold` says (#474): the switcher gives way to the view options
    /// button alone, Sort moves into "⋯" (the selection's, or one here with nothing selected).
    pub(super) fn render_view_controls(
        &self,
        fold: &RowLayout,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Split panes are half width: the switcher is always folded into the view options button
        // there (the flyout has the same choices), leaving room for Copy to/Move to.
        let switcher = fold.switcher() && !self.pane.split;
        let tools = div()
            .relative()
            .child(self.tools_probe())
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_4())
            .when(fold.tools_more(), |d| d.child(more_button("row-more", cx)))
            // Search results keep their own order (relevance, or their headers).
            .when(!self.search_mode && !fold.sort_folded(), |d| {
                d.child(self.render_sort_options(window, cx))
            })
            .when(switcher, |d| d.child(self.render_view_switcher(window, cx)))
            .when(!switcher, |d| {
                d.child(self.render_view_options(false, window, cx))
            })
            .when(self.pane.pair_toggles && !fold.toggles_folded(), |d| {
                d.child(self.render_dual_toggles(cx))
            });
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_4())
            .child(tools)
            .child(preview_button(self.preview_pane, cx))
            .child(split_button(
                self.pane.split,
                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ExplorerEvent::ToggleSplit)),
            ))
    }

    /// Details, Icons and Columns, with the view options flyout's chevron at the end.
    fn render_view_switcher(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "view-switcher".to_owned())
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_2())
            .p(theme::s_2())
            .rounded(theme::r_8())
            // Same segmented style as the search scope strip: a recessed track with the current
            // view as a raised pill.
            .bg(theme::segment_track())
            .child(segment(
                "view-details",
                Icon::ViewList,
                self.view_mode == ViewMode::Details,
                t!("ctx.view_details"),
                cx.listener(|this, _: &ClickEvent, w, cx| {
                    this.view_details(&ViewDetails, w, cx);
                }),
            ))
            .child(segment(
                "view-icons",
                Icon::ViewGrid,
                self.view_mode == ViewMode::Icons,
                self.icon_size.tooltip(),
                cx.listener(|this, _: &ClickEvent, _, cx| this.show_icons(cx)),
            ))
            .child(segment(
                "view-columns",
                Icon::ViewColumns,
                self.view_mode == ViewMode::Columns,
                t!("ctx.view_columns"),
                cx.listener(|this, _: &ClickEvent, w, cx| {
                    this.view_columns(&ViewColumns, w, cx);
                }),
            ))
            .child(self.render_view_options(true, window, cx))
    }

    /// A click on a Delete button: Shift deletes permanently after asking, as in File Explorer.
    fn delete_clicked(&mut self, e: &ClickEvent, w: &mut Window, cx: &mut Context<Self>) {
        if e.modifiers().shift {
            self.delete_permanently(&DeletePermanently, w, cx);
        } else {
            self.delete_files(&DeleteFiles, w, cx);
        }
    }

    /// The selection's summary with the total size, and without it.
    fn selection_summaries(&self) -> (String, String) {
        let count = self.selection.len();
        // Split view shows only the count, leaving room for "Copy to/Move to <destination>".
        let short = if self.pane.split {
            t!("sel.count_short", count = format_count(count)).into_owned()
        } else {
            t!("sel.count", count = format_count(count)).into_owned()
        };
        let full = match self.selected_size().filter(|_| !self.pane.split) {
            Some(size) => format!("{short} · {}", format_size(size)),
            None => short.clone(),
        };
        (full, short)
    }

    /// The selection in the pane row, drawn as `layout` says (see [`Self::row_layout`]).
    pub(super) fn render_selection_actions(
        &self,
        layout: SelectionLayout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.selection.len();
        let (full, short) = self.selection_summaries();
        // Split view with both sides on different folders: Copy to/Move to take the room of the
        // regular buttons. Same folder: they'd copy items onto themselves, so they're left out and
        // the regular buttons come back.
        let order = hide_order(self.peer_target().is_some(), count);
        if layout.folded() {
            return self.render_folded_selection(count, cx);
        }
        let shows = |act: Act| {
            order
                .iter()
                .position(|a| *a == act)
                .is_some_and(|ix| layout.shows(ix))
        };
        let summary = match layout.summary {
            Summary::Pill => self.count_pill(count).into_any_element(),
            Summary::Full => summary_label(full).into_any_element(),
            Summary::Count => summary_label(short).into_any_element(),
        };
        let peer_actions = self.render_peer_actions(
            layout.labels,
            shows(Act::CopyToPeer),
            shows(Act::MoveToPeer),
            cx,
        );
        let row = div()
            .debug_selector(|| "selection-actions".to_owned())
            .relative()
            .child(self.info_probe())
            .flex()
            .min_w_0()
            .items_center()
            .gap(theme::s_4())
            .overflow_hidden()
            .child(summary)
            .child(vertical_divider())
            .children(peer_actions);
        Self::add_selection_buttons(row, shows, cx)
            // The buttons that didn't fit, in the selection's menu.
            .when(layout.more(), |d| d.child(more_button("sel-more", cx)))
            .child(div().flex_1())
            .child(clear_selection_button(
                true,
                cx.listener(|this, _: &ClickEvent, w, cx| this.escape(&Escape, w, cx)),
            ))
            .into_any_element()
    }

    /// Adds to the selection's `row` the regular buttons that `shows` keeps there, in drawing
    /// order: Open, Cut, Copy, Rename and Delete.
    fn add_selection_buttons(
        row: gpui::Div,
        shows: impl Fn(Act) -> bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        // Action buttons are always icon-only to keep the bar uncluttered.
        let compact = true;
        row
            // Next to Copy to/Move to there are no Open, Cut, Copy and Rename buttons: Enter,
            // Ctrl+X, Ctrl+C, F2 and the context menu still work.
            .when(shows(Act::Open), |d| {
                d.child(action_button(
                    "sel-open",
                    Icon::ArrowRight,
                    t!("sel.open"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| {
                        this.open_selected(&OpenSelected, w, cx);
                    }),
                ))
            })
            .when(shows(Act::Cut), |d| {
                d.child(action_button(
                    "sel-cut",
                    Icon::Cut,
                    t!("sel.cut"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| this.cut_files(&CutFiles, w, cx)),
                ))
            })
            .when(shows(Act::Copy), |d| {
                d.child(action_button(
                    "sel-copy",
                    Icon::Copy,
                    t!("sel.copy"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| this.copy_files(&CopyFiles, w, cx)),
                ))
            })
            .when(shows(Act::Rename), |d| {
                d.child(action_button(
                    "sel-rename",
                    Icon::Rename,
                    t!("sel.rename"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| {
                        this.rename_file(&RenameFile, w, cx);
                    }),
                ))
            })
            .when(shows(Act::Delete), |d| {
                d.child(action_button(
                    "sel-delete",
                    Icon::Trash,
                    t!("sel.delete"),
                    Tone::Danger,
                    compact,
                    cx.listener(Self::delete_clicked),
                ))
            })
    }

    /// The widths the selection's layouts are made of (see [`fold`]), from the tokens they are
    /// drawn with: the summary (`full` with the size, `short` without), the buttons in `order`
    /// and, in split view (`peer`), what Copy to/Move to's labels add.
    fn selection_parts(
        &self,
        full: &str,
        short: &str,
        count: usize,
        order: &[Act],
        peer: Option<&Path>,
        window: &Window,
    ) -> SelectionParts {
        // Laid out rounded up to whole pixels, as every text is.
        let text = |s: &str| {
            super::top_bar::text_width(s, theme::fs_14(), theme::FW_STRONG, window).ceil()
        };
        // Dot, gap, text and the margin after it.
        let summary = |s: &str| theme::s_6() + theme::s_8() + text(s) + theme::s_4();
        let full_w = summary(full);
        let short_w = if short == full {
            full_w
        } else {
            summary(short)
        };
        // The pill: padding, the dot, the gap and the count.
        let pill = theme::s_6() * 2. + theme::s_6() + theme::s_6() + text(&format_count(count));
        let divider = px(1.) + theme::s_4() * 2.;
        let icon_button = theme::s_6() * 2. + theme::icon();
        // Outlined: a 1 px border on each side.
        let outlined = theme::s_8() * 2. + theme::icon() + px(2.);
        let buttons = order
            .iter()
            .map(|act| {
                let w = match act {
                    Act::MoveToPeer => outlined,
                    // With the divider that ends Copy to/Move to, 4 px after them.
                    Act::CopyToPeer => outlined + theme::s_4() + divider,
                    _ => icon_button,
                };
                f32::from(w)
            })
            .collect();
        let labels = peer
            .filter(|_| self.paper_width >= peer_labels_min())
            .map_or(px(0.), |peer| {
                let name = peer_name(peer);
                [
                    t!("sel.copy_to", name = name),
                    t!("sel.move_to", name = name),
                ]
                .iter()
                .map(|label| {
                    let text = super::top_bar::text_width(
                        label,
                        theme::fs_12(),
                        gpui::FontWeight::NORMAL,
                        window,
                    )
                    .ceil();
                    // The gap between the icon and the label.
                    theme::s_6() + text
                })
                .fold(px(0.), |sum, w| sum + w)
            });
        SelectionParts {
            full: f32::from(full_w),
            count: f32::from(short_w),
            pill: f32::from(pill),
            buttons,
            labels: f32::from(labels),
            more: f32::from(theme::h_ctl_sm()),
            clear: f32::from(theme::s_8() * 2. + theme::icon_sm()),
            divider: f32::from(divider),
            gap: f32::from(theme::s_4()),
            tight_gap: f32::from(theme::s_2()),
        }
    }

    /// How the pane row fits next to the path bar (#388, #474, #528): the view switcher gives way
    /// to the view options button, Sort moves into "⋯", the selection's summary shortens and its
    /// buttons move into "⋯" one at a time, and the path bar loses levels only once all of that
    /// is folded (see [`fold`]). The path bar keeps at least
    /// [`super::top_bar::path_compact_below`] too, below which it shows only the current folder.
    /// The room is what the path bar, the row's info (item count or selection) and the view tools
    /// that fold took on the last frame: their sum doesn't change with what they show, so the
    /// choice settles at once. With nothing selected the info stays as wide as it was drawn.
    /// Before the row was laid out, the pane's width decides between everything and everything
    /// folded.
    pub(super) fn row_layout(&self, selecting: bool, window: &Window, cx: &App) -> RowLayout {
        let info = if selecting {
            let count = self.selection.len();
            let (full, short) = self.selection_summaries();
            let peer = self.peer_target();
            let order = hide_order(peer.is_some(), count);
            Info::Selection(self.selection_parts(&full, &short, count, order, peer, window))
        } else {
            let text = self.info_parts(cx).join(" · ");
            let text =
                super::top_bar::text_width(&text, theme::fs_12(), gpui::FontWeight::NORMAL, window);
            Info::Folder {
                // Laid out rounded up to whole pixels, and the gap after it.
                count: f32::from(text.ceil() + theme::s_4()),
                rest: f32::from(self.rest_slot.get()),
            }
        };
        // Synchronized browsing and compare (the last pane of a split tab): 28 px each, 4 apart,
        // and the gap before them. A pinned tab has no synchronized browsing.
        let toggles = if self.pane.pair_toggles {
            let n = if self.locked { 1. } else { 2. };
            (theme::s_6() * 2. + theme::icon()) * n + theme::s_4() * n
        } else {
            px(0.)
        };
        // The view options button: padding, the view's icon, a gap and the chevron.
        let view = theme::s_6() * 2. + theme::icon() + theme::s_2() + theme::icon_sm();
        let parts = RowParts {
            // Padding, Details, Icons and Columns, the chevron, 2 px apart.
            switcher: f32::from(if self.pane.split {
                view
            } else {
                theme::s_2() * 2. + theme::scaled_px(28.) * 3. + theme::s_20() + theme::s_2() * 3.
            }),
            view: f32::from(view),
            // As wide as the view options button, and a gap.
            sort: if self.search_mode {
                0.
            } else {
                f32::from(view + theme::s_4())
            },
            toggles: f32::from(toggles),
            more: f32::from(theme::h_ctl_sm() + theme::s_4()),
            info,
        };
        let path = self.path_slot.get();
        if path <= px(0.) {
            let needs = if self.pane.split { 600. } else { 720. };
            let room = if selecting && self.paper_width < theme::scaled_px(needs) {
                0.
            } else {
                f32::INFINITY
            };
            return fold::row_layout(room, 0., &parts);
        }
        let (info, tools) = (self.info_slot.get(), self.tools_slot.get());
        let need = self
            .path_full_width(window)
            .max(super::top_bar::path_compact_below());
        let [path, info, tools, need] = [path, info, tools, need].map(f32::from);
        let room = path + info + tools;
        let layout = fold::row_layout(room, need, &parts);
        // Once per change: the row draws on every hover.
        let seen = (path, info, tools, need, layout);
        if log::log_enabled!(target: "arke::fold", log::Level::Debug)
            && self.fold_logged.replace(Some(seen)) != Some(seen)
        {
            let (info_width, tools_width) = parts.widths(&layout);
            log::debug!(
                target: "arke::fold",
                "room {room} = path slot {path} + info {info} + tools {tools}; path needs {need}, \
                 info {info_width} and tools {tools_width} wide: {layout:?}",
            );
        }
        layout
    }

    /// Tests: "⋯" pressed, wherever it is.
    #[cfg(test)]
    pub(crate) fn open_more_menu_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let size = window.viewport_size();
        self.open_more_menu(gpui::point(size.width / 2., size.height / 3.), window, cx);
    }

    /// Tests: the pane row's slots and what its layout was decided from, to say why a row came
    /// out as it did.
    #[cfg(test)]
    pub(crate) fn row_debug_for_test(&self) -> String {
        format!(
            "path slot {:?}, info {:?}, tools {:?}; {:?}",
            self.path_slot.get(),
            self.info_slot.get(),
            self.tools_slot.get(),
            self.row_fold,
        )
    }

    /// Records the width of the pane row's info (the item count or the selection's actions) for
    /// [`Self::row_layout`], drawing once more when it changed, as the path bar's slot does
    /// (they change together, so this adds no frame of its own).
    fn info_probe(&self) -> impl IntoElement + use<> {
        slot_probe(&self.info_slot)
    }

    /// Records the width of the view tools that fold ("⋯", Sort, the view switcher or button) for
    /// [`Self::row_layout`], as [`Self::info_probe`] does.
    fn tools_probe(&self) -> impl IntoElement + use<> {
        slot_probe(&self.tools_slot)
    }

    /// The selection's "⋯" or the view tools' was pressed: the menu it opens has what the row
    /// folded into it. The selection's menu gets "Sort by" while Sort is folded (#474); the
    /// folder's background menu has it anyway.
    pub(super) fn open_more_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_early_menu();
        self.row_fold.menu_from_more = true;
        self.open_context_menu(position, window, cx);
        self.row_fold.menu_from_more = false;
    }

    /// The selection in a narrow pane: "● 3" (the tooltip has the whole summary) and "⋯", which
    /// opens the selection's context menu under it: open, cut, copy, rename, delete, copy or
    /// move to the other side are all there.
    fn render_folded_selection(&self, count: usize, cx: &mut Context<Self>) -> AnyElement {
        div()
            .debug_selector(|| "selection-actions".to_owned())
            .relative()
            .child(self.info_probe())
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_2())
            .child(self.count_pill(count))
            .child(more_button("sel-more", cx))
            .into_any_element()
    }

    /// "● 3", the selection's summary when the pane row is short of room: its tooltip has the
    /// whole summary.
    fn count_pill(&self, count: usize) -> impl IntoElement + use<> {
        let mut summary = t!("sel.count", count = format_count(count)).into_owned();
        if let Some(size) = self.selected_size() {
            let _ = write!(summary, " · {}", format_size(size));
        }
        div()
            .id("sel-count")
            .debug_selector(|| "sel-count".to_owned())
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_6())
            .h(theme::h_ctl_sm())
            .px(theme::s_6())
            // The size the window's text has anyway, which `selection_parts` measures the count
            // at.
            .text_size(theme::fs_14())
            .font_weight(theme::FW_STRONG)
            .child(
                div()
                    .size(theme::s_6())
                    .rounded(theme::R_FULL)
                    .bg(theme::signal()),
            )
            .child(format_count(count))
            .tooltip_below(summary)
    }

    /// The other pane's folder, when copying or moving there makes sense: split view with the other
    /// side somewhere else. Both sides on the same folder would copy items onto themselves.
    fn peer_target(&self) -> Option<&Path> {
        let peer = self.pane.peer.as_deref()?;
        (self.current_path() != Some(peer)).then_some(peer)
    }

    /// Split view: "Copy to/Move to the other side", naming the destination, as far as they are
    /// drawn (`copy`, `moves`; the rest is in "⋯"). In a narrow pane they show only their icons
    /// (the tooltip still names the destination), since a label cut down to "…" says nothing.
    fn render_peer_actions(
        &self,
        labels: bool,
        copy: bool,
        moves: bool,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let name = peer_name(self.peer_target().filter(|_| copy || moves)?);
        Some(
            div()
                .flex()
                .min_w_0()
                .items_center()
                .gap(theme::s_4())
                .when(copy, |d| {
                    d.child(peer_button(
                        "sel-copy-peer",
                        Icon::Copy,
                        t!("sel.copy_to", name = name),
                        "Alt+C",
                        labels,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.transfer_to_peer(false, cx);
                        }),
                    ))
                })
                .when(moves, |d| {
                    d.child(peer_button(
                        "sel-move-peer",
                        Icon::Cut,
                        t!("sel.move_to", name = name),
                        "Alt+M",
                        labels,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.transfer_to_peer(true, cx);
                        }),
                    ))
                })
                .child(vertical_divider()),
        )
    }

    /// Where a drop on item `ix` goes, and whether the item itself is that target (drawn as
    /// one). A folder takes drops itself. A file inside a folder expanded in the list passes them
    /// to that folder, which is where it sits (#260): dropped on the folder's own file, a file
    /// stays put instead of moving up a level. Other files take none (the list's folder does).
    pub(super) fn drop_dir(&self, ix: usize) -> Option<(PathBuf, bool)> {
        if self.search_mode {
            return None;
        }
        let model = self.model.as_ref()?;
        let entry = self.view()?.get(ix)?.clone();
        if entry.is_dir_like() {
            return Some((model.path.join(&*entry.name), true));
        }
        arke_core::tree::parent(&entry.name).map(|parent| (model.path.join(parent), false))
    }

    /// Total size of the selection (folders excluded). Skipped when too many are selected.
    pub(crate) fn selected_size(&self) -> Option<u64> {
        if self.selection.is_empty() || self.selection.len() > SIZE_SUM_MAX {
            return None;
        }
        // The list on screen, so items in expanded tree folders count too.
        let view = self.view()?;
        let mut total = 0u64;
        let mut any_file = false;
        for entry in view.iter().filter(|e| self.selection.contains(&e.name)) {
            if !entry.is_dir_like() {
                total += entry.size;
                any_file = true;
            } else if let super::sizes::FolderSize::Known { bytes, .. } =
                self.folder_size(&entry.name)
            {
                // Include folders whose size is known.
                total += bytes;
                any_file = true;
            }
        }
        any_file.then_some(total)
    }

    /// Info on the left of the context bar (replaces the old status bar).
    pub(crate) fn info_parts(&self, cx: &App) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.search_global {
            return self.computer_info(cx);
        }
        if self.search_mode {
            if let Some(r) = &self.search_results {
                let mut s = tn!("info.matched", r.matched as usize).into_owned();
                if r.matched as usize > MAX_HITS {
                    s.push_str(&t!("info.showing_first", count = format_count(MAX_HITS)));
                }
                parts.push(s);
                let scanned = if r.contents {
                    tn!("info.content_read", r.read as usize).into_owned()
                } else {
                    t!("info.scanned", count = format_count(r.scanned as usize)).into_owned()
                };
                parts.push(if r.searching {
                    t!("info.searching", scanned = scanned).into_owned()
                } else {
                    scanned
                });
                if r.truncated {
                    parts.push(t!("info.truncated").into_owned());
                }
                if r.errors > 0 {
                    parts.push(tn!("info.unreadable", r.errors as usize).into_owned());
                }
            }
            return parts;
        }
        // Once the list says "Loading", the old folder's counts would read as the new one's.
        if let Some(model) = self.model.as_ref().filter(|_| !self.show_loading) {
            let total = format_count(model.entries.len());
            parts.push(if self.is_filtering() {
                let shown = self.view().map_or(0, |v| v.len());
                t!("info.filtered", shown = format_count(shown), total = total).into_owned()
            } else {
                tn!("info.items", model.entries.len()).into_owned()
            });
            if self.is_filtering() {
                parts.extend(self.content_info());
            }
            if self.hidden_count > 0 {
                let count = format_count(self.hidden_count);
                parts.push(if self.compact_bar() {
                    t!("info.hidden_short", count = count).into_owned()
                } else {
                    t!("info.hidden", count = count).into_owned()
                });
            }
            // Cached contents of a network folder: fresh data still loading or server unreachable.
            if self.showing_cached {
                parts.push(if self.error.is_some() {
                    t!("info.cached_offline").into_owned()
                } else {
                    t!("info.cached_refreshing").into_owned()
                });
            }
        }
        if self.pending.is_some() {
            parts.push(t!("info.loading").into_owned());
        }
        parts
    }

    // ───────────────────────── List ─────────────────────────

    /// Screen reader name of an item: name, type, size and date modified.
    fn item_a11y_label(&self, entry: &Entry) -> String {
        use super::sizes::FolderSize;
        let size = if entry.is_dir_like() {
            match self.folder_size(&entry.name) {
                FolderSize::Known { bytes, .. } => format_size(bytes),
                FolderSize::Computing | FolderSize::Unknown => String::new(),
            }
        } else {
            format_size(entry.size)
        };
        let modified = entry
            .modified
            .map_or_default(|t| self.services.shell.format_time(t));
        // Nested tree rows are named by relative path; read the file name. A folder's localized
        // name is read as it is shown (#370).
        let name = entry
            .shown
            .as_deref()
            .unwrap_or_else(|| arke_core::tree::leaf(&entry.name));
        crate::a11y::item_label(
            name,
            &[&arke_core::format::type_label(entry), &size, &modified],
        )
    }

    /// Screen reader name of the file list: the folder shown, or the search.
    fn list_a11y_label(&self) -> String {
        if self.search_mode {
            return if self.scope() == super::scope::Scope::Computer {
                t!("filter.computer_placeholder").into_owned()
            } else {
                self.search_in_name().map_or_else(
                    || t!("filter.search_placeholder").into_owned(),
                    |name| t!("filter.search_in_placeholder", name = name).into_owned(),
                )
            };
        }
        self.display_path().map_or_default(|p| {
            p.file_name().map_or_else(
                || p.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        })
    }

    fn row_shell(
        &self,
        ix: usize,
        is_selected: bool,
        is_cursor: bool,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let drop_dir = self.drop_dir(ix);
        let height = self.row_height();
        // Details rows end with their last column, like the header above (the list is blank past
        // it); the column view's rows span the list.
        let width = self.cols.map(|c| c.row);
        let group = Self::check_group(ix);
        // The row's answer to the drag over it, for its highlight (drawn as a drop target only
        // when the row takes the drop). The row paints before its highlight, so it asks first and
        // the highlight doesn't ask again.
        let drop_ok = drop_dir
            .as_ref()
            .is_some_and(|(_, own)| *own)
            .then(|| Rc::new(Cell::new(false)));
        let wash = row_wash(
            ix,
            &group,
            is_selected,
            is_cursor,
            focused,
            drop_ok.as_ref().map(Rc::clone),
        );
        Self::item_events(ix, drop_dir, drop_ok, cx)
            .debug_selector(move || format!("row-{ix}"))
            .group(group)
            .relative()
            .map(|d| match width {
                Some(w) => d.w(w).flex_none(),
                None => d.w_full(),
            })
            .h(height)
            .flex()
            .items_center()
            .pl(layout::row_pad_l())
            .pr(layout::row_pad_r())
            .gap(layout::col_gap())
            .child(wash)
    }

    /// A tile: [`Self::item_events`], with the selection background, cursor ring and drop
    /// target drawn on the tile itself.
    fn item_shell(
        ix: usize,
        is_selected: bool,
        is_cursor: bool,
        focused: bool,
        drop_dir: Option<(PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let own = drop_dir.as_ref().is_some_and(|(_, own)| *own);
        Self::item_events(ix, drop_dir, None, cx)
            .when(own, |d| {
                d.drag_over::<DraggedFiles>(|s, _, _, _| {
                    s.bg(theme::signal_wash())
                        .border_color(theme::signal_line())
                })
                .drag_over::<ExternalPaths>(|s, _, _, _| {
                    s.bg(theme::signal_wash())
                        .border_color(theme::signal_line())
                })
            })
            .rounded(theme::r_8())
            .border_1()
            .border_color(if is_cursor && focused {
                theme::signal_line()
            } else {
                theme::clear()
            })
            .when(is_selected, |d| d.bg(theme::list_selection(focused)))
            .when(!is_selected, |d| d.hover(|s| s.bg(theme::wash())))
    }

    /// Shared by rows and tiles: mouse events, dragging and drops. `drop_ok` (rows) is told
    /// whether the item takes what is dragged over it.
    fn item_events(
        ix: usize,
        drop_dir: Option<(PathBuf, bool)>,
        drop_ok: Option<Rc<Cell<bool>>>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let source = cx.entity().downgrade();
        let target = cx.entity();
        div()
            .id(ix)
            // Drag: the selected items (or just this one)
            .on_drag(
                DraggedFiles::from_list(source, ix),
                |dragged, grab, _, cx| {
                    let preview = DragPreview::start(dragged, grab, cx);
                    cx.new(|_| preview)
                },
            )
            // Click without drag: in a multi-selection, select only this item.
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if this.on_row_click(ix, event, cx) {
                    return;
                }
                this.on_row_released(ix, event, window, cx);
                if this.click_select == Some(ix) && event.click_count() == 1 {
                    this.click_select = None;
                    this.narrow_selection_to(ix);
                    cx.notify();
                }
            }))
            // Drop: folders, and files inside an expanded folder (into that folder).
            .when_some(drop_dir, move |d, (dir, _)| {
                let external = dir.clone();
                let check = dir.clone();
                d.can_drop(move |value, _, cx| {
                    let ok = super::accepts_drop(value, &check, cx);
                    if let Some(drop_ok) = &drop_ok {
                        drop_ok.set(ok);
                    }
                    ok
                })
                .on_drag_move({
                    let dir = dir.clone();
                    move |e: &gpui::DragMoveEvent<DraggedFiles>, _, cx| {
                        super::report_drop_target(&dir, e, cx);
                    }
                })
                .on_drop(move |dragged: &DraggedFiles, window, cx| {
                    drop_into(dragged, dir.clone(), window, cx);
                })
                .on_drop(move |paths: &ExternalPaths, window, cx| {
                    drop_external_into(paths, external.clone(), &target, window, cx);
                })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_row_mouse_down(ix, event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.on_row_middle_down(ix, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_row_right_down(ix, event, window, cx);
                }),
            )
            // The first moves of a right drag, before the window-wide listener exists.
            .on_mouse_move(cx.listener(move |this, event: &gpui::MouseMoveEvent, window, cx| {
                if this.right_press.is_some() {
                    this.right_press_moved(event, window, cx);
                }
                // Single-click mode: resting on the item selects it (#46).
                this.on_row_pointer_move(ix, event, window, cx);
            }))
            .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                if !*hovered {
                    this.on_row_pointer_left(ix);
                }
            }))
            // Released on the item before the window-wide listener exists (a quick tap).
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseUpEvent, window, cx| {
                    if !cx.has_global::<super::RightDrag>() {
                        this.right_press_released(event.position, window, cx);
                    }
                }),
            )
    }

    /// The name and badges in a Details row's Name column. The compare badge and the cloud glyph
    /// (OneDrive online-only, syncing) are part of this column so they don't push the later
    /// columns out of line. `cloud`: whether the folder is in a sync root, and the item's state.
    fn name_column(
        &self,
        ix: usize,
        entry: &Entry,
        rename_input: Option<(Entity<TextInput>, Option<NameError>)>,
        (in_cloud, sync): (bool, Option<arke_core::entry::CloudSync>),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let press: NamePress =
            Box::new(cx.listener(move |this, _: &gpui::MouseDownEvent, _, _| {
                this.on_name_pressed(ix);
            }));
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(theme::s_12())
            .child(name_cell(
                entry,
                rename_input,
                self.shown_name_of(entry),
                true,
                Some(press),
                rename_box_h(self.row_height()),
            ))
            .children(super::compare::compare_badge(
                self.compare_mark(&entry.name),
            ))
            .children(cloud_badge(ix, entry, in_cloud, sync))
    }

    /// A row's Name column: checkbox, tree lead, icon and name, as wide as the header's Name
    /// (filling the row in the column view).
    fn name_block(&self) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap(layout::col_gap())
            .map(|d| match self.cols {
                Some(c) => d.w(c.name).flex_none(),
                None => d.flex_1().min_w_0(),
            })
    }

    /// `tips`: which of the rows get a tooltip with their whole name (column view names cut off).
    pub(super) fn render_rows(
        &mut self,
        range: Range<usize>,
        focused: bool,
        tips: &[bool],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let started = Instant::now();
        let Some(entries) = self.view() else {
            return Vec::new();
        };
        let shell = Arc::clone(&self.services.shell);
        let cursor = self.cursor;
        let in_cloud = self.current_path().is_some_and(|p| self.in_cloud(p));
        let sync = self.cloud.snapshot();
        let shares = lists_shares(self.current_path());
        // Skip the cursor ring when the only selected item is the cursor, to avoid doubling the
        // selection background.
        let single = self.selection.len() <= 1;
        let visible = names_in(&entries, range.clone());
        let cut = self.cut_flags(&visible, cx);
        let images = self.media_for(&entries, range.clone(), false, cx);
        let renaming = self
            .renaming
            .as_ref()
            .map(|r| (Arc::clone(&r.name), (r.input.clone(), r.problem)));
        let columns = self.columns_active();
        let rows = range
            .filter_map(|ix| entries.get(ix).map(|entry| (ix, entry)))
            .enumerate()
            .map(|(k, (ix, entry))| {
                let image = images.get(k).cloned().flatten();
                let is_cut = cut.get(k).copied().unwrap_or(false);
                let rename_input = renaming
                    .as_ref()
                    .filter(|(name, _)| *name == entry.name)
                    .map(|(_, rename)| rename.clone());
                let is_selected = self.selection.contains(&entry.name);
                let is_cursor = cursor == Some(ix) && !(single && is_selected);
                let check = self
                    .show_checkboxes
                    .then(|| self.item_checkbox(ix, is_selected, cx));
                self.row_shell(ix, is_selected, is_cursor, focused, cx)
                    .when(tips.get(k).copied().unwrap_or(false), |d| {
                        d.tooltip(tooltip(SharedString::from(&*entry.name)))
                    })
                    .a11y_item(ix, is_selected, cursor == Some(ix), || {
                        self.item_a11y_label(entry)
                    })
                    .map(|row| self.tree_a11y(ix, entry, row, cx))
                    .text_color(if entry.is_hidden() {
                        theme::text_subtle()
                    } else {
                        theme::text()
                    })
                    // Cut items pending paste are drawn semi-transparent, like Explorer.
                    .when(is_cut, |d| d.opacity(0.5))
                    .child(
                        self.name_block()
                            .children(check)
                            .child(self.tree_lead(
                                ix,
                                entry,
                                listed_visual(shares, entry, image, theme::icon(), theme::icon()),
                                cx,
                            ))
                            .child(self.name_column(
                                ix,
                                entry,
                                rename_input,
                                (in_cloud, sync.get(&entry.name).copied()),
                                cx,
                            )),
                    )
                    .when(columns, |d| d.children(Self::column_row_tail(entry)))
                    .when(!columns, |d| {
                        d.children(self.detail_cells(entry, shell.as_ref()))
                    })
                    .into_any_element()
            })
            .collect();
        if let Some(bench) = self.bench.as_mut() {
            bench.record_rows(started.elapsed());
        }
        rows
    }

    /// Thumbnails or system icons for visible items (once loaded), and hand the visible range plus
    /// a page ahead and a page back to the media cache.
    pub(crate) fn media_for(
        &mut self,
        entries: &[Entry],
        range: Range<usize>,
        thumbnails: bool,
        cx: &mut Context<Self>,
    ) -> Vec<Option<Arc<RenderImage>>> {
        self.folder_sizes_shown(entries, range.clone(), cx);
        let Some(dir) = self.current_path().map(Path::to_path_buf) else {
            return Vec::new();
        };
        // Elsewhere (network shares) only type icons: they read no file.
        let allowed = self.media_ok(&dir, cx);
        let thumb_px = (thumbnails && self.show_thumbnails).then(|| self.thumbnail_px());
        // System icons come at the size they are drawn at, so they map 1:1 onto screen pixels.
        let icon_px = crate::media::icon_source_px(
            f32::from(if thumbnails {
                self.tile().icon
            } else {
                theme::icon()
            }),
            self.ui_scale,
        );
        let key = |ix: usize| {
            entries.get(ix).and_then(|e| {
                if allowed {
                    MediaKey::sized(&dir, e, thumb_px)
                } else {
                    MediaKey::type_icon(e)
                }
                .map(|k| k.with_icon_px(icon_px))
            })
        };
        let keys: Vec<Option<MediaKey>> = range.clone().map(key).collect();
        // A page ahead, then a page back (scrolling back finds them still decoded).
        let prefetch_end = (range.end + range.len()).min(entries.len());
        let prefetch_start = range.start.saturating_sub(range.len());
        let wanted: Vec<MediaKey> = keys
            .iter()
            .flatten()
            .cloned()
            .chain((range.end..prefetch_end).filter_map(key))
            .chain((prefetch_start..range.start).rev().filter_map(key))
            .collect();
        let view = cx.entity_id();
        self.media.update(cx, |m, cx| m.want(view, wanted, cx));
        let media = self.media.read(cx);
        // A thumbnail decoded for another icon size stands in until this size's arrives.
        keys.iter()
            .map(|k| k.as_ref().and_then(|k| media.get_or_nearby(k)))
            .collect()
    }

    /// Grid view: each uniform_list item is one row of tiles.
    fn render_grid_rows(
        &mut self,
        rows: Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let len = self.view().map_or(0, |v| v.len());
        let cols = self.grid_cols.max(1);
        let rows: Vec<Range<usize>> = rows
            .map(|r| (r * cols).min(len)..((r + 1) * cols).min(len))
            .collect();
        self.render_tile_rows(rows, focused, cx)
    }

    /// Rows of grid tiles: each row is a contiguous run of items (when grouped, a group's last row
    /// may be partial).
    /// A tile's name (two lines at most), or its rename box.
    fn tile_label(
        &self,
        ix: usize,
        entry: &Entry,
        rename_input: Option<(Entity<TextInput>, Option<NameError>)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match rename_input {
            Some(rename) => name_cell(
                entry,
                Some(rename),
                SharedString::default(),
                false,
                None,
                theme::s_24(),
            ),
            None => div()
                .debug_selector({
                    let name = Arc::clone(&entry.name);
                    move || format!("name:{name}")
                })
                .w_full()
                .text_center()
                .line_clamp(2)
                // Two lines can't fit (long name with no break points): ellipsize the second line
                // rather than clipping the sides.
                .text_ellipsis()
                .text_size(theme::fs_12())
                .child(self.shown_name_of(entry))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &gpui::MouseDownEvent, _, _| {
                        this.on_name_pressed(ix);
                    }),
                )
                .into_any_element(),
        }
    }

    pub(super) fn render_tile_rows(
        &mut self,
        rows: Vec<Range<usize>>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(entries) = self.view() else {
            return Vec::new();
        };
        let start = rows.first().map_or(0, |r| r.start).min(entries.len());
        let end = rows
            .last()
            .map_or(0, |r| r.end)
            .min(entries.len())
            .max(start);
        let names = names_in(&entries, start..end);
        let cut = self.cut_flags(&names, cx);
        let images = self.media_for(&entries, start..end, true, cx);
        let cursor = self.cursor;
        let shares = lists_shares(self.current_path());
        let single = self.selection.len() <= 1;
        let renaming = self
            .renaming
            .as_ref()
            .map(|r| (Arc::clone(&r.name), (r.input.clone(), r.problem)));
        let tile = self.tile();
        rows.into_iter()
            .map(|row| {
                let tiles: Vec<AnyElement> = (row.start..row.end.min(entries.len()))
                    .map(|ix| {
                        let entry = &entries[ix];
                        let k = ix - start;
                        let is_selected = self.selection.contains(&entry.name);
                        let is_cursor = cursor == Some(ix) && !(single && is_selected);
                        let rename_input = renaming
                            .as_ref()
                            .filter(|(name, _)| *name == entry.name)
                            .map(|(_, rename)| rename.clone());
                        let image = images.get(k).cloned().flatten();
                        let label = self.tile_label(ix, entry, rename_input, cx);
                        // Folder compare badge in the tile's top-right
                        let badge = super::compare::compare_badge(self.compare_mark(&entry.name))
                            .map(|badge| {
                                div()
                                    .absolute()
                                    .top(theme::s_4())
                                    .right(theme::s_4())
                                    .child(badge)
                            });
                        // Checkbox in the tile's top-left
                        let check = self.show_checkboxes.then(|| {
                            div()
                                .absolute()
                                .top(theme::s_6())
                                .left(theme::s_6())
                                .child(self.item_checkbox(ix, is_selected, cx))
                        });
                        Self::item_shell(ix, is_selected, is_cursor, focused, self.drop_dir(ix), cx)
                            .a11y_item(ix, is_selected, cursor == Some(ix), || {
                                self.item_a11y_label(entry)
                            })
                            .relative()
                            .group(Self::check_group(ix))
                            .children(check)
                            .children(badge)
                            .w(tile.w)
                            .h(tile.h)
                            .flex_none()
                            .flex()
                            .flex_col()
                            .items_center()
                            .pt(theme::s_4())
                            .px(theme::s_4())
                            .gap(theme::s_4())
                            .when(entry.is_hidden(), |d| d.text_color(theme::text_subtle()))
                            .when(cut.get(k).copied().unwrap_or(false), |d| d.opacity(0.5))
                            .child(
                                div()
                                    .size(tile.image)
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(listed_visual(
                                        shares, entry, image, tile.image, tile.icon,
                                    )),
                            )
                            .child(label)
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .px(theme::s_8())
                    .children(tiles)
                    .into_any_element()
            })
            .collect()
    }

    fn grid_list(&mut self, len: usize, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        // Pane width (preview pane already excluded) minus paper margin, list padding and
        // scrollbar.
        let avail = self.pane_width
            - layout::paper_margin()
            - theme::s_32()
            - scrollbar_gutter()
            - layout::list_gutter();
        let tile = self.tile();
        self.grid_cols = ((avail / tile.w).floor() as usize).max(1);
        if std::mem::take(&mut self.recenter_grid)
            && let Some(ix) = self.cursor
        {
            self.scroll_to_index(ix, gpui::ScrollStrategy::Center);
        }
        // Grouped grid counts in cells (a header is one cell, a tile row several), see
        // explorer_groups.rs.
        let (count, item_h) = self.grid_groups().map_or_else(
            || (len.div_ceil(self.grid_cols), tile.h),
            |g| (g.slots.len(), tile.h / super::groups::TILE_SLOTS as f32),
        );
        let list = uniform_list(
            "grid",
            count + tail_items(item_h),
            cx.processor(
                move |this, range: Range<usize>, _window, cx| match this.grid_groups() {
                    Some(groups) => this.render_grouped_grid(&groups, range, focused, cx),
                    None => this.render_grid_rows(range, focused, cx),
                },
            ),
        )
        .track_scroll(&self.scroll)
        // A sideways swipe is back or forward (#70), not a scroll down the list.
        .map(sideways_is_not_down)
        .pt(theme::s_4())
        .flex_1();
        self.with_scrollbar(list, &self.scroll)
    }

    /// Details list (also the last column of the column view); when grouped, headers and items are
    /// laid out together.
    fn details_list(&mut self, len: usize, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let list = uniform_list(
            "files",
            self.display_len(len) + tail_items(self.row_height()),
            cx.processor(
                move |this, range: Range<usize>, window, cx| {
                    if let Some(groups) = this.groups() {
                        return this.render_grouped_rows(&groups, range, focused, cx);
                    }
                    let tips = this.column_name_tips(range.clone(), window);
                    this.render_rows(range, focused, &tips, cx)
                },
            ),
        )
        .track_scroll(&self.scroll)
        // A sideways swipe is back or forward (#70), not a scroll down the list.
        .map(sideways_is_not_down)
        // Rows start a little in from the sidebar, so a marquee started beside it starts in
        // empty space (#448).
        .pl(theme::list_inset())
        .pr(theme::s_8() + layout::list_gutter())
        .flex_1();
        self.with_scrollbars(list, &self.scroll)
    }

    /// [`Self::with_scrollbar`] for Details rows and search results: columns wider than the pane
    /// scroll sideways (the header along), like Explorer, with a scrollbar along the bottom. The
    /// list's right padding keeps [`layout::list_gutter()`] blank after the last column even then.
    fn with_scrollbars(
        &self,
        list: gpui::UniformList,
        handle: &gpui::UniformListScrollHandle,
    ) -> AnyElement {
        if self.cols.is_none() {
            return self.with_scrollbar(list, handle);
        }
        let list =
            list.with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained);
        let base = handle.0.borrow().base_handle.clone();
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(self.with_scrollbar(list, handle))
            .child(crate::scrollbar::hscrollbar(
                base,
                Rc::clone(&self.hscrollbar_drag),
            ))
            .into_any_element()
    }

    /// List with an overlay scrollbar (drawn only when content exceeds one page).
    fn with_scrollbar(
        &self,
        list: impl IntoElement,
        handle: &gpui::UniformListScrollHandle,
    ) -> AnyElement {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(list)
            .child(crate::scrollbar::scrollbar(
                handle.0.borrow().base_handle.clone(),
                Rc::clone(&self.scrollbar_drag),
            ))
            .into_any_element()
    }

    /// Empty state: large icon, one sentence, and what to do next. No match: also offers buttons to
    /// widen the scope (subfolders, this PC).
    pub(super) fn no_match_state(
        &self,
        title: String,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let icon = svg()
            .path(Icon::Search.path())
            .size(theme::s_32())
            .text_color(theme::text_subtle())
            .into_any_element();
        let widen = if self.query.trim().is_empty() {
            None
        } else {
            self.widen_buttons(cx)
        };
        Self::empty_state_with(icon, title, hint, widen)
    }

    pub(super) fn empty_state(icon: AnyElement, title: String, hint: Option<String>) -> AnyElement {
        Self::empty_state_with(icon, title, hint, None)
    }

    fn empty_state_with(
        icon: AnyElement,
        title: String,
        hint: Option<String>,
        extra: Option<AnyElement>,
    ) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::s_8())
            .child(div().opacity(0.7).child(icon))
            .child(
                div()
                    .text_size(theme::fs_15())
                    .text_color(theme::text_muted())
                    .child(title),
            )
            .children(hint.map(|h| {
                div()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(h)
            }))
            .children(extra)
            .into_any_element()
    }

    /// Location that failed to open: say where and why, with retry, go up and edit path.
    fn render_failed(&self, failed: &super::FailedNav, cx: &mut Context<Self>) -> AnyElement {
        let name = failed.path.file_name().map_or_else(
            || failed.path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let has_parent = arke_core::path::parent(&failed.path).is_some();
        let login = matches!(self.error_action, Some(ErrorAction::Login(_)));
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::s_8())
            .px(theme::s_32())
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme::s_24())
                    .text_color(theme::danger()),
            )
            .child(
                div()
                    .text_size(theme::fs_15())
                    .font_weight(theme::FW_STRONG)
                    .child(t!("list.failed_title", name = name).into_owned()),
            )
            .child(
                div()
                    .text_color(theme::text_muted())
                    .text_center()
                    .child(failed.reason.clone()),
            )
            .child(
                div()
                    .max_w_full()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .child(failed.path.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .gap(theme::s_8())
                    .mt(theme::s_8())
                    .child(outline_button(
                        "failed-retry",
                        if login {
                            t!("toast.login")
                        } else {
                            t!("toast.retry")
                        },
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            if this.error_action.is_some() {
                                this.run_error_action(window, cx);
                            } else if let Some(path) = this.failed.as_ref().map(|f| f.path.clone())
                            {
                                this.navigate(path, NavKind::New, cx);
                            }
                        }),
                    ))
                    .when(has_parent, |d| {
                        d.child(outline_button(
                            "failed-up",
                            t!("nav.up"),
                            cx.listener(|this, _: &ClickEvent, w, cx| this.go_up(&GoUp, w, cx)),
                        ))
                    })
                    .child(outline_button(
                        "failed-edit",
                        t!("list.failed_edit"),
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.start_edit_path(window, cx);
                        }),
                    )),
            )
            .into_any_element()
    }

    /// Empty subfolder search: what typing will search.
    fn search_prompt(&self) -> String {
        self.search_in_name().map_or_else(
            || t!("list.search_prompt").into_owned(),
            |name| t!("list.search_in_prompt", name = name).into_owned(),
        )
    }

    fn render_search_rows(
        &mut self,
        range: Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(results) = self.search_results.clone() else {
            return Vec::new();
        };
        let shell = Arc::clone(&self.services.shell);
        let cursor = self.search_cursor;
        // "Search in this folder": locations start at that folder, below the folder shown.
        let base = self.search_in_name().map(PathBuf::from);
        // "Similar results" (#396) is a row between hits, not a hit.
        let similar = self.similar_row();
        let at = similar.map(|(at, _)| at);
        let mut rows = Vec::with_capacity(range.len());
        for row in range {
            match super::similar::hit_at_row(row, at) {
                None => {
                    if let Some((_, kind)) = similar {
                        rows.push(self.similar_row_element(row, kind, focused, cx));
                    }
                }
                Some(ix) => {
                    if let Some(hit) = results.hits.get(ix) {
                        rows.push(self.search_row(
                            ix,
                            hit,
                            cursor,
                            base.as_deref(),
                            &shell,
                            focused,
                            cx,
                        ));
                    }
                }
            }
        }
        rows
    }

    /// One search hit's row (`ix` in the hits).
    fn search_row(
        &self,
        ix: usize,
        hit: &Arc<arke_core::SearchHit>,
        cursor: Option<usize>,
        base: Option<&Path>,
        shell: &Arc<dyn crate::shell::Shell>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = &hit.entry;
        let parent = Path::new(&*hit.rel)
            .parent()
            .filter(|p| !p.as_os_str().is_empty());
        let location = match (base, parent) {
            (Some(base), Some(parent)) => base.join(parent).display().to_string(),
            (Some(base), None) => base.display().to_string(),
            (None, Some(parent)) => parent.display().to_string(),
            (None, None) => t!("list.here").into_owned(),
        };
        let is_selected = self.hit_selected(ix, hit);
        // As in a folder's rows: no ring when the cursor is the only selected item.
        let is_cursor = cursor == Some(ix) && !(is_selected && self.search_selected_count() <= 1);
        self.row_shell(ix, is_selected, is_cursor, focused, cx)
            .a11y_item(ix, is_selected, cursor == Some(ix), || {
                let modified = entry.modified.map(|t| shell.format_time(t));
                let content = hit
                    .snippet
                    .as_deref()
                    .map(|line| t!("search.content_match", line = line).into_owned());
                crate::a11y::item_label(
                    &entry.name,
                    &[
                        content.as_deref().unwrap_or_default(),
                        location.as_str(),
                        modified.as_deref().unwrap_or_default(),
                    ],
                )
            })
            // The Name column, as wide as the header's. A content match shows its line
            // after the name.
            .child(
                self.name_block()
                    .child(entry_visual(entry, None, theme::icon(), theme::icon()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(theme::s_12())
                            .child(
                                div()
                                    .debug_selector({
                                        let name = Arc::clone(&entry.name);
                                        move || format!("name:{name}")
                                    })
                                    .when(hit.snippet.is_some(), |d| {
                                        d.flex_none().max_w(crate::theme::scaled_px(240.))
                                    })
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(Arc::clone(&entry.name))),
                            )
                            .children(hit.snippet.clone().map(|line| {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme::fs_12())
                                    .text_color(theme::text_muted())
                                    .child(SharedString::from(line))
                            })),
                    ),
            )
            .child(Self::location_cell(ix, &hit.rel, location, cx))
            .child(
                div()
                    .w(crate::prefs::modified_width())
                    .flex_none()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_muted())
                    .child(
                        entry
                            .modified
                            .map_or_default(|t| shell.format_time(t)),
                    ),
            )
            .child(size_cell(entry, super::sizes::FolderSize::Unknown))
            .into_any_element()
    }

    /// A search hit's Location. A double-click on it opens the folder with the hit selected
    /// (#398), so it looks like a link under the pointer, as the preview pane's Location does.
    fn location_cell(
        ix: usize,
        rel: &Arc<str>,
        location: String,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let rel = Arc::clone(rel);
        div()
            .debug_selector(move || format!("location:{rel}"))
            .w(theme::col_location())
            .flex_none()
            // Keep the end of long paths (the folders nearest the file).
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis_start()
            .text_size(theme::fs_12())
            .text_color(theme::text_muted())
            .cursor_pointer()
            .hover(|s| s.text_color(theme::text()).underline())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, _| this.on_location_pressed(ix)),
            )
            .child(location)
    }

    /// List content: rows, tiles, search results, or the empty/loading state.
    #[allow(clippy::too_many_lines, reason = "one match over every list state")]
    fn list_body(&mut self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: SharedString| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::text_subtle())
                .child(text)
                .into_any_element()
        };
        if self.search_global
            && let Some(placeholder) = self.computer_placeholder(cx)
        {
            return placeholder;
        }
        if self.search_mode {
            match &self.search_results {
                _ if self.nothing_to_search() => message(self.search_prompt().into()),
                Some(r) if r.hits.is_empty() && !r.searching => {
                    self.no_match_state(t!("list.no_match").into_owned(), None, cx)
                }
                Some(_) => {
                    let list = uniform_list(
                        "search-results",
                        self.search_row_count(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            this.render_search_rows(range, focused, cx)
                        }),
                    )
                    .track_scroll(&self.search_scroll)
                    // A sideways swipe is back or forward (#70), not a scroll down the list.
                    .map(sideways_is_not_down)
                    .pl(theme::list_inset())
                    .pr(theme::s_8() + layout::list_gutter())
                    .flex_1();
                    self.with_scrollbars(list, &self.search_scroll)
                }
                None => message(t!("list.searching").to_string().into()),
            }
        } else {
            let view_len = self.view().map(|v| v.len());
            match &self.model {
                _ if self.show_loading => message(t!("info.loading").to_string().into()),
                Some(_) if view_len == Some(0) && self.content_searching() => {
                    message(t!("list.searching_contents").to_string().into())
                }
                Some(_) if view_len == Some(0) && self.is_filtering() => self.no_match_state(
                    t!("list.no_filter_match").into_owned(),
                    Some(t!("list.no_filter_hint").into_owned()),
                    cx,
                ),
                // A server without (visible) shares takes no new folders.
                Some(model)
                    if view_len == Some(0) && model.complete && lists_shares(Some(&model.path)) =>
                {
                    Self::empty_state(
                        crate::assets::tinted(
                            Icon::NetDrive,
                            crate::theme::scaled_px(40.),
                            theme::text_muted(),
                        ),
                        t!("list.no_shares").into_owned(),
                        None,
                    )
                }
                Some(model) if view_len == Some(0) && model.complete => Self::empty_state(
                    kind_icon(FileKind::Folder, crate::theme::scaled_px(40.)),
                    t!("list.empty").into_owned(),
                    Some(t!("list.empty_hint").into_owned()),
                ),
                Some(_) if view_len == Some(0) => message(t!("info.loading").to_string().into()),
                Some(_) if self.view_mode == ViewMode::Icons => {
                    self.grid_list(view_len.unwrap_or(0), focused, cx)
                }
                Some(_) => {
                    let list = self.details_list(view_len.unwrap_or(0), focused, cx);
                    if self.columns_active() {
                        div()
                            .relative()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_parent_column(cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .pt(theme::s_4())
                                    .child(list),
                            )
                            .child(self.render_parent_divider(cx))
                            .into_any_element()
                    } else {
                        list
                    }
                }
                None if self.failed.is_some() => match &self.failed {
                    Some(failed) => self.render_failed(failed, cx),
                    None => message(t!("list.cannot_show").to_string().into()),
                },
                None if self.error.is_some() => message(t!("list.cannot_show").to_string().into()),
                None => message(t!("info.loading").to_string().into()),
            }
        }
    }

    fn render_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.list_focus.is_focused(window);
        self.ui_scale = window.scale_factor();
        let body = self.list_body(focused, cx);
        let count = if self.search_mode {
            self.search_results.as_ref().map_or(0, |r| r.hits.len())
        } else {
            self.view().map_or(0, |v| v.len())
        };
        div()
            .id("file-list")
            .a11y_list(|| self.list_a11y_label(), count)
            .key_context("FileList")
            .track_focus(&self.list_focus)
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .pb(theme::s_8())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    window.focus(&this.list_focus, cx);
                    // Double click on empty space goes up (setting, off by default); presses on
                    // items are handled by the row.
                    let up = is_double_click(e.click_count)
                        && !this.row_pressed
                        && crate::settings::SettingsStore::global(cx)
                            .read(cx)
                            .settings
                            .double_click_up;
                    if up {
                        this.marquee = None;
                        this.go_up(&GoUp, window, cx);
                        return;
                    }
                    this.on_list_left_down(e, cx);
                }),
            )
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_list_right_down))
            .when_some(self.current_path().map(Path::to_path_buf), |d, here| {
                accept_drops_into(d, here, cx)
            })
            .on_action(cx.listener(Self::open_context_menu_action))
            .on_key_down(cx.listener(Self::on_list_key_down))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::extend_prev))
            .on_action(cx.listener(Self::extend_next))
            .on_action(cx.listener(Self::extend_page_up))
            .on_action(cx.listener(Self::extend_page_down))
            .on_action(cx.listener(Self::extend_first))
            .on_action(cx.listener(Self::extend_last))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::toggle_cursor))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::open_in_new_tab))
            .on_action(cx.listener(Self::show_properties))
            .on_action(cx.listener(Self::open_location))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(|this, _: &super::LookZoomIn, window, cx| {
                this.look_zoom_key(Some(1), window, cx);
            }))
            .on_action(cx.listener(|this, _: &super::LookZoomOut, window, cx| {
                this.look_zoom_key(Some(-1), window, cx);
            }))
            .on_action(cx.listener(|this, _: &super::LookZoomReset, window, cx| {
                this.look_zoom_key(None, window, cx);
            }))
            .child(body)
            .children(self.render_marquee(cx))
            .child(Self::render_zoom_capture(cx))
    }

    // ───────────────────────── Toasts ─────────────────────────

    /// Regular toasts close after a while; errors and progress don't.
    fn schedule_notice_dismiss(&mut self, cx: &mut Context<Self>) {
        let Some(notice) = self.notice.clone() else {
            return;
        };
        if self
            .notice_timer
            .as_ref()
            .is_some_and(|(n, _)| *n == notice)
        {
            return;
        }
        let shown = notice.clone();
        let ttl = if self
            .notice_undo
            .as_ref()
            .is_some_and(|(text, _)| *text == notice)
            || self
                .notice_action
                .as_ref()
                .is_some_and(|a| a.text == notice)
        {
            NOTICE_UNDO_TTL
        } else {
            NOTICE_TTL
        };
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ttl).await;
            let _ = this.update(cx, |this, cx| {
                if this.notice.as_deref() == Some(shown.as_str()) {
                    this.notice = None;
                    cx.notify();
                }
            });
        });
        self.notice_timer = Some((notice, task));
    }

    /// Running file operation: current item, progress bar, cancel. Also drawn by other views
    /// that run an operation of their own (the Recycle Bin page).
    pub(crate) fn op_toast<V: 'static>(
        progress: &Arc<crate::shell::OpProgress>,
        verb: &'static str,
        cx: &mut Context<V>,
    ) -> AnyElement {
        use std::sync::atomic::Ordering;
        let done = progress.done.load(Ordering::Relaxed);
        let total = progress.work_total.load(Ordering::Relaxed);
        let so_far = progress.work_done.load(Ordering::Relaxed);
        let cancelling = progress.cancel.load(Ordering::Relaxed);
        let op = t!(verb);
        // If the operation center has this job, use its text (percent, speed, time left) and
        // progress so both match.
        let job = crate::op_center::OpCenter::jobs(cx)
            .iter()
            .find(|job| Arc::ptr_eq(&job.progress, progress))
            .map(|job| (job.detail(), job.fraction()));
        if let Some((detail, fraction)) = job.filter(|_| !cancelling) {
            let cancel = Arc::clone(progress);
            return toast(
                "toast-ops",
                crate::op_center::op_icon(verb),
                t!("toast.op_detail", op = op, detail = detail).into_owned(),
                Tone::Normal,
                fraction.map_or(Progress::Indeterminate, Progress::Fraction),
                vec![(
                    t!("toast.cancel"),
                    Box::new(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cancel.cancel.store(true, Ordering::Relaxed);
                        cx.notify();
                    })) as Listener,
                )],
            );
        }
        let text = if cancelling {
            t!("toast.op_cancelling", op = op).into_owned()
        } else if done == 0 {
            // Windows is still enumerating the items.
            t!("toast.op_preparing", op = op).into_owned()
        } else {
            t!(
                "toast.op_progress",
                op = op,
                done = arke_core::format::format_count(usize::try_from(done).unwrap_or(usize::MAX))
            )
            .into_owned()
        };
        let bar = if total > 0 && done > 0 {
            Progress::Fraction(so_far.min(total) as f32 / total as f32)
        } else {
            Progress::Indeterminate
        };
        let cancel = Arc::clone(progress);
        toast(
            "toast-ops",
            crate::op_center::op_icon(verb),
            text,
            Tone::Normal,
            bar,
            (!cancelling)
                .then(|| {
                    (
                        t!("toast.cancel"),
                        Box::new(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cancel.cancel.store(true, Ordering::Relaxed);
                            cx.notify();
                        })) as Listener,
                    )
                })
                .into_iter()
                .collect(),
        )
    }

    /// Short message with Close, plus Undo when it reports an operation that can still be undone.
    fn notice_toast(&self, notice: &str, cx: &mut Context<Self>) -> AnyElement {
        let mut actions: Vec<(_, Listener)> = Vec::new();
        if let Some((_, id)) = self
            .notice_undo
            .as_ref()
            .filter(|(text, id)| text == notice && super::UndoHistory::has(cx, *id))
        {
            let id = *id;
            actions.push((
                t!("toast.undo"),
                Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.notice = None;
                    this.undo_op(id, cx);
                })),
            ));
        }
        if let Some(action) = self.notice_action.as_ref().filter(|a| a.text == notice) {
            let run = std::rc::Rc::clone(&action.run);
            actions.push((
                std::borrow::Cow::Owned(action.label.to_string()),
                Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.notice = None;
                    this.notice_action = None;
                    // After this view's update: the action may read it (the sidebar does).
                    let run = std::rc::Rc::clone(&run);
                    cx.defer(move |cx| run(cx));
                    cx.notify();
                })),
            ));
        }
        actions.push((
            t!("toast.close"),
            Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.notice = None;
                cx.notify();
            })),
        ));
        toast(
            "toast-notice",
            Icon::ArrowRight,
            notice.to_owned(),
            Tone::Normal,
            Progress::None,
            actions,
        )
    }

    fn render_toasts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mut toasts: Vec<AnyElement> = Vec::new();
        let view = cx.entity_id();
        // Progress toasts wait: short jobs are only in the operation center.
        if let Some(job) = self
            .extract_jobs
            // The one running; the others wait in the progress center.
            .first()
            .filter(|_| ToastDelays::shown(cx, view, EXTRACT_TOAST))
        {
            let p = job.progress;
            let progress = if p.total > 0 {
                Progress::Fraction(p.done.min(p.total) as f32 / p.total as f32)
            } else {
                Progress::Indeterminate
            };
            toasts.push(toast(
                "toast-extract",
                Icon::FolderPlus,
                if job.compress {
                    t!("toast.compressing", name = job.name).into_owned()
                } else {
                    t!("toast.extracting", name = job.name).into_owned()
                },
                Tone::Normal,
                progress,
                vec![(
                    t!("toast.cancel"),
                    Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.cancel_extract();
                        cx.notify();
                    })),
                )],
            ));
        }
        if self.ops_running > 0 {
            if ToastDelays::shown(cx, view, OPS_TOAST) {
                if let Some((progress, verb)) = &self.op_progress {
                    toasts.push(Self::op_toast(progress, verb, cx));
                } else {
                    toasts.push(toast(
                        "toast-ops",
                        Icon::Copy,
                        t!("toast.ops_running").into_owned(),
                        Tone::Normal,
                        Progress::Indeterminate,
                        Vec::new(),
                    ));
                }
            }
        } else if let Some(notice) = &self.notice {
            toasts.push(self.notice_toast(notice, cx));
        }
        if let Some(error) = &self.error {
            let action: Option<(_, Listener)> = self.error_action.as_ref().map(|a| {
                let label = match a {
                    ErrorAction::Retry(_) | ErrorAction::RetryOp(_) => t!("toast.retry"),
                    ErrorAction::Login(_) => t!("toast.login"),
                    ErrorAction::OpenDefault(_) => t!("toast.open_default"),
                };
                let listener: Listener =
                    Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.run_error_action(window, cx);
                    }));
                (label, listener)
            });
            // Errors stay until dismissed (or the folder changes), so they can always be closed.
            let close: Listener = Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.error = None;
                this.error_action = None;
                cx.notify();
            }));
            toasts.push(toast(
                "toast-error",
                Icon::Close,
                error.clone(),
                Tone::Danger,
                Progress::None,
                action
                    .into_iter()
                    .chain(std::iter::once((t!("toast.close"), close)))
                    .collect(),
            ));
        }
        if toasts.is_empty() {
            return None;
        }
        Some(
            div()
                .absolute()
                .right(theme::s_16())
                .bottom(theme::s_16())
                .w(crate::theme::scaled_px(360.))
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .children(toasts)
                .into_any_element(),
        )
    }
}

impl Explorer {
    /// The notice toast as drawn now: its text and whether it offers Undo. Only shown while no
    /// operation runs (the progress toast takes its place).
    fn shown_notice(&self, cx: &App) -> Option<(String, bool)> {
        let notice = self.notice.as_ref().filter(|_| self.ops_running == 0)?;
        let undo = self
            .notice_undo
            .as_ref()
            .is_some_and(|(text, id)| text == notice && super::UndoHistory::has(cx, *id))
            || self
                .notice_action
                .as_ref()
                .is_some_and(|a| &a.text == notice);
        Some((notice.clone(), undo))
    }

    /// A notice that just went away (timed out, closed, replaced by nothing) fades out. Notices are
    /// cleared from many places, so this compares what is shown with the last frame instead.
    fn track_notice_exit(&mut self, cx: &mut Context<Self>) {
        let shown = self.shown_notice(cx);
        if shown == self.notice_shown {
            return;
        }
        let gone = std::mem::replace(&mut self.notice_shown, shown);
        if self.notice_shown.is_some() {
            self.closing_notice = None;
        } else if let Some(gone) = gone {
            crate::motion::Leaving::start(self, gone, |this| &mut this.closing_notice, cx);
        }
    }

    /// Starts the delay of the progress toasts when a job begins and forgets it when the job ends.
    fn track_progress_toasts(&mut self, cx: &mut Context<Self>) {
        ToastDelays::track(cx, OPS_TOAST, self.ops_running > 0);
        ToastDelays::track(cx, EXTRACT_TOAST, !self.extract_jobs.is_empty());
    }

    /// The toasts up now, or else a notice that is fading out.
    fn render_toast_stack(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.render_toasts(cx)
            .or_else(|| self.render_leaving_notice())
    }

    /// The faded-out notice, where it was. Drawn only when no other toast is up, so it never
    /// overlaps one. Its buttons are drawn (inert) so the text wraps, and the buttons sit beside
    /// or below it, as they did.
    fn render_leaving_notice(&self) -> Option<AnyElement> {
        let ghost = self.closing_notice.as_ref()?;
        let (text, undo) = &ghost.value;
        let noop = || -> Listener { Box::new(|_, _, _| {}) };
        let mut actions: Vec<(std::borrow::Cow<'static, str>, Listener)> = Vec::new();
        if *undo {
            actions.push((t!("toast.undo"), noop()));
        }
        actions.push((t!("toast.close"), noop()));
        let toast = toast_body(
            "toast-notice",
            Icon::ArrowRight,
            text.clone(),
            Tone::Normal,
            Progress::None,
            actions,
        );
        Some(
            div()
                .absolute()
                .right(theme::s_16())
                .bottom(theme::s_16())
                .w(crate::theme::scaled_px(360.))
                .flex()
                .flex_col()
                .child(crate::motion::leave(
                    div().child(toast),
                    ghost.id("toast-leave"),
                    0.,
                    crate::motion::NUDGE,
                ))
                .into_any_element(),
        )
    }
}

// ───────────────────────── Shared components ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Normal,
    Danger,
}

type Listener = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Toast progress bar
#[derive(Clone, Copy)]
enum Progress {
    None,
    /// Unknown remaining time: draw a fixed-length segment
    Indeterminate,
    Fraction(f32),
}

/// An error toast with Close, for views outside the file list (the Recycle Bin page).
pub(crate) fn error_toast(
    id: &'static str,
    text: String,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    toast(
        id,
        Icon::Close,
        text,
        Tone::Danger,
        Progress::None,
        vec![(t!("toast.close"), Box::new(on_close) as Listener)],
    )
}

/// A toast that rises in when it first shows up.
fn toast(
    id: &'static str,
    icon: Icon,
    text: String,
    tone: Tone,
    progress: Progress,
    actions: Vec<(std::borrow::Cow<'static, str>, Listener)>,
) -> AnyElement {
    crate::motion::appear(
        div().child(toast_body(id, icon, text, tone, progress, actions)),
        gpui::ElementId::Name(format!("{id}-appear").into()),
        0.,
        crate::motion::NUDGE,
    )
}

fn toast_body(
    id: &'static str,
    icon: Icon,
    text: String,
    tone: Tone,
    progress: Progress,
    actions: Vec<(std::borrow::Cow<'static, str>, Listener)>,
) -> AnyElement {
    let danger = tone == Tone::Danger;
    let a11y_role = if danger {
        gpui::Role::Alert
    } else {
        gpui::Role::Status
    };
    // Errors read like a WinUI InfoBar (critical): a soft red fill, the message in regular ink
    // and only the icon in the danger color. A red paragraph is hard to read and looks alarming.
    let glyph = if danger {
        // The "i" upside down is an exclamation mark in a circle, the error glyph.
        svg()
            .path(Icon::Info.path())
            .with_transformation(gpui::Transformation::rotate(gpui::radians(
                std::f32::consts::PI,
            )))
            .text_color(theme::danger())
    } else {
        svg().path(icon.path()).text_color(theme::text_muted())
    };
    // Messages are read out when they appear; progress text changes too often for that, so a
    // reader finds it by moving there (the bar below reports the percentage).
    let toast = div().id(id);
    let toast = if matches!(progress, Progress::None) {
        toast.a11y_live(a11y_role, text.clone())
    } else {
        toast.a11y(a11y_role, text.clone())
    };
    toast
        .flex()
        .flex_col()
        .gap(theme::s_8())
        .px(theme::s_16())
        .py(theme::s_12())
        .rounded(theme::r_12())
        .bg(if danger {
            theme::danger_surface()
        } else {
            theme::overlay()
        })
        .shadow(theme::elev_float())
        .child(toast_row(
            id,
            glyph.size(theme::icon()).flex_none(),
            text,
            actions,
            // Progress text changes as the job runs: its Cancel stays beside it rather than
            // jumping between lines whenever the text gets a little longer or shorter.
            matches!(progress, Progress::None),
        ))
        .children(
            match progress {
                Progress::None => None,
                Progress::Indeterminate => Some((0.3, None)),
                Progress::Fraction(f) => Some((f, Some(f))),
            }
            .map(|(fraction, known)| {
                div()
                    .id("toast-progress")
                    .a11y_progress(known)
                    .h(theme::s_4())
                    .w_full()
                    .rounded(theme::R_FULL)
                    .bg(theme::line())
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded(theme::R_FULL)
                            .bg(theme::signal())
                            .w(gpui::relative(fraction)),
                    )
            }),
        )
        .into_any_element()
}

/// Most lines a toast's message takes before it ends in "…". With the buttons out of the way the
/// message has about 300 px, so six lines hold some 250 characters: an error naming a long full
/// path still reads whole, and even a clamped toast stays under 200 px tall.
const TOAST_LINES: usize = 6;

/// A toast's icon and message, then its buttons. Like a WinUI InfoBar, the buttons sit beside a
/// message that fits on one line next to them ("Copied", Close) and, when `wrap` is set and it
/// doesn't, move to a line of their own below it, right-aligned, so the message gets the toast's
/// whole width instead of a narrow column beside them. Without `wrap` (progress) the message stays
/// on one line, ending in "…" when it is too long, so the icon, the text and Cancel stay lined
/// up on its middle (the operation center shows the whole line).
fn toast_row(
    id: &'static str,
    glyph: impl IntoElement,
    text: String,
    actions: Vec<(std::borrow::Cow<'static, str>, Listener)>,
    wrap: bool,
) -> impl IntoElement {
    let buttons = (!actions.is_empty()).then(|| {
        div()
            .debug_selector(move || format!("{id}-actions"))
            .ml_auto()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::s_8())
            .children(
                actions
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (label, on_click))| {
                        toast_button(id, ix, label.into_owned(), on_click)
                    }),
            )
    });
    let row = div()
        .flex()
        .items_center()
        .gap_x(theme::s_8())
        .gap_y(theme::s_4());
    let row = if wrap { row.flex_wrap() } else { row };
    row.child(
        // Grows from the message's own width (not `flex_1`'s zero basis), so a message too wide
        // to sit beside the buttons pushes them to the next line.
        div()
            .flex_auto()
            .min_w_0()
            .flex()
            .items_center()
            .gap(theme::s_8())
            .child(glyph)
            .child({
                let message = div().debug_selector(move || format!("{id}-text")).min_w_0();
                let message = if wrap {
                    message.line_clamp(TOAST_LINES).text_ellipsis()
                } else {
                    message.truncate()
                };
                message.child(text)
            }),
    )
    .children(buttons)
}

/// One of a toast's buttons: quiet text that lights up on hover.
fn toast_button(
    id: &'static str,
    ix: usize,
    label: String,
    on_click: Listener,
) -> impl IntoElement {
    div()
        .id(("toast-action", ix))
        .debug_selector(move || format!("{id}-action-{ix}"))
        .a11y_button(label.clone())
        .flex_none()
        .px(theme::s_6())
        .rounded(theme::r_4())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child(label)
        .on_click(on_click)
}

pub(crate) fn tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text: SharedString = text.into();
    move |_, cx| cx.new(|_| crate::workspace::Tooltip(text.clone())).into()
}

fn kbd(text: &'static str) -> impl IntoElement {
    div()
        .px(theme::s_4())
        .rounded(theme::r_4())
        .border_1()
        .border_color(theme::line_strong())
        .text_size(theme::fs_11())
        .text_color(theme::text_subtle())
        .child(text)
}

/// Outlines the pane the search box is searching, while it's typed in. Drawn over the paper; it
/// takes no clicks.
/// Split view: the active pane's thin accent edge, drawn over the paper's border (a shadow would
/// show through the translucent paper and tint the whole pane).
fn active_outline() -> impl IntoElement {
    div()
        .debug_selector(|| "active-pane-outline".to_owned())
        .absolute()
        .inset_0()
        .rounded(theme::r_paper())
        .border_1()
        .border_color(theme::signal_line())
}

fn search_outline() -> impl IntoElement {
    div()
        .debug_selector(|| "search-target".to_owned())
        .absolute()
        .inset_0()
        .rounded(theme::r_paper())
        .border_2()
        .border_color(theme::signal_line())
}

pub(super) fn vertical_divider() -> impl IntoElement {
    div()
        .w(px(1.))
        .h(theme::s_16())
        .mx(theme::s_4())
        .flex_none()
        .bg(theme::line())
}

/// The other pane's folder as Copy to/Move to name it.
fn peer_name(peer: &Path) -> String {
    peer.file_name().map_or_else(
        || peer.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The selection's summary in the pane row: the dot and `text` ("3 selected · 1.2 MB").
fn summary_label(text: String) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_8())
        .mr(theme::s_4())
        // The size the window's text has anyway, set here because `selection_parts` measures
        // the summary at it.
        .text_size(theme::fs_14())
        .font_weight(theme::FW_STRONG)
        .child(
            div()
                .size(theme::s_6())
                .rounded(theme::R_FULL)
                .bg(theme::signal()),
        )
        .child(text)
}

/// The selection's buttons, as [`hide_order`] lists them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Open,
    Rename,
    Cut,
    Copy,
    MoveToPeer,
    CopyToPeer,
    Delete,
}

/// The selection's buttons in the order they move into "⋯" as the pane row narrows (#528), the
/// least used first: Open (Enter and a double click do it), Rename (F2), Cut, Copy, and Delete
/// last. In split view Copy to/Move to the other side take the place of Open, Cut, Copy and
/// Rename; Move to goes before Copy to, as Cut goes before Copy.
const fn hide_order(peer: bool, count: usize) -> &'static [Act] {
    match (peer, count) {
        (true, _) => &[Act::MoveToPeer, Act::CopyToPeer, Act::Delete],
        (false, 1) => &[Act::Open, Act::Rename, Act::Cut, Act::Copy, Act::Delete],
        (false, _) => &[Act::Open, Act::Cut, Act::Copy, Act::Delete],
    }
}

/// "⋯", for what the pane row has no room for (`id`: the selection's, or the view tools' with
/// nothing selected): it opens the selection's context menu under it, or the folder's, which
/// have them all (see [`Explorer::open_more_menu`]).
fn more_button(id: &'static str, cx: &mut Context<Explorer>) -> impl IntoElement + use<> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_button(t!("sel.more"))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme::h_ctl_sm())
        .rounded(theme::r_8())
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash()))
        .child(
            svg()
                .path(Icon::More.path())
                .size(theme::icon())
                .text_color(theme::text_muted()),
        )
        .tooltip_below(t!("sel.more").to_string())
        // A screen reader's Enter and the keyboard in the pane row: opens where Shift+F10 would
        // (#322).
        .a11y_on_click(open_selection_menu(cx.entity().downgrade()))
        .toolbar_menu(id, open_selection_menu(cx.entity().downgrade()))
        // Opens on press, like the other menus.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.open_more_menu(event.position, window, cx);
            }),
        )
}

/// Opens "⋯"'s menu where Shift+F10 would: "⋯" pressed by a screen reader or from the keyboard.
fn open_selection_menu(
    this: gpui::WeakEntity<Explorer>,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let size = window.viewport_size();
        let at = gpui::point(size.width / 2., size.height / 3.);
        let _ = this.update(cx, |this, cx| {
            this.open_more_menu(at, window, cx);
        });
    }
}

/// Records the width of the element it's drawn in into `slot`, drawing once more on the next
/// frame when it changed: a refresh while drawing is dropped (#528).
fn slot_probe(slot: &super::path_bar::SlotWidth) -> impl IntoElement + use<> {
    let slot = Rc::clone(slot);
    gpui::canvas(
        move |b, window, _| {
            if slot.replace(b.size.width) != b.size.width {
                window.on_next_frame(|window, _| window.refresh());
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Top bar icon button: no background until hover.
pub(crate) fn ghost_button(
    id: &'static str,
    icon: Icon,
    enabled: bool,
    tip: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    ghost_button_turned(id, icon, 0., enabled, tip, on_click)
}

/// [`ghost_button`] with its icon rotated by `turn` radians (a chevron pointing up or down).
pub(crate) fn ghost_button_turned(
    id: &'static str,
    icon: Icon,
    turn: f32,
    enabled: bool,
    tip: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let tip = SharedString::from(tip.to_string());
    div()
        .id(id)
        .debug_selector(|| id.to_owned())
        .a11y_button(tip.clone())
        .a11y_disabled(!enabled)
        .size(theme::h_ctl())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::r_8())
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(theme::wash_strong()))
                .toolbar_button(id, on_click)
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme::icon())
                .with_transformation(gpui::Transformation::rotate(gpui::radians(turn)))
                .text_color(if enabled {
                    theme::text_muted()
                } else {
                    theme::text_disabled()
                }),
        )
        .a11y_button(tip.clone())
        .a11y_disabled(!enabled)
        .tooltip_below(tip)
}

/// Context bar action: icon plus label.
fn action_button(
    id: &'static str,
    icon: Icon,
    label: impl std::fmt::Display,
    tone: Tone,
    compact: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    // Destructive actions look like the others until pointed at: a red icon sitting in the bar
    // all the time draws the eye to the one button you least want to hit by accident.
    let danger = tone == Tone::Danger;
    let label = SharedString::from(label.to_string());
    div()
        .id(id)
        .debug_selector(|| id.to_owned())
        .a11y_button(label.clone())
        .group(id)
        .h(theme::h_ctl_sm())
        .px(theme::s_8())
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_6())
        .rounded(theme::r_8())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .hover(|s| {
            let s = s.bg(theme::wash());
            if danger {
                s.text_color(theme::danger())
            } else {
                s.text_color(theme::text())
            }
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme::icon())
                .text_color(theme::text_muted())
                .group_hover(id, move |s| {
                    s.text_color(if danger {
                        theme::danger()
                    } else {
                        theme::text()
                    })
                }),
        )
        // At half width keep only the icon and move the label to the tooltip.
        .when(compact, |d| d.px(theme::s_6()).tooltip_below(label.clone()))
        .when(!compact, |d| d.child(label.clone()))
        .toolbar_button(id, on_click)
}

/// "Clear selection Esc"; just × when narrow.
fn clear_selection_button(
    compact: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id("sel-clear")
        .a11y_button(t!("sel.clear"))
        .h(theme::h_ctl_sm())
        .px(theme::s_8())
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_6())
        .rounded(theme::r_8())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
        .child(
            svg()
                .path(Icon::Close.path())
                .size(theme::icon_sm())
                .text_color(theme::text_subtle()),
        )
        .when(!compact, |d| {
            d.child(t!("sel.clear").to_string()).child(kbd("Esc"))
        })
        .tooltip_below(t!("sel.clear").to_string() + " (Esc)")
        .toolbar_button("sel-clear", on_click)
}

/// Outlined button ("Retry", "Up" on the error screen).
fn outline_button(
    id: &'static str,
    label: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .a11y_lazy(gpui::Role::Button, || label.to_string())
        .h(theme::h_ctl_sm())
        .px(theme::s_12())
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .hover(|s| s.bg(theme::wash()))
        .child(label.to_string())
        .on_click(on_click)
}

/// List background: drops into the current folder (from the other pane or from Explorer).
fn accept_drops_into(
    d: gpui::Stateful<gpui::Div>,
    here: PathBuf,
    cx: &mut Context<Explorer>,
) -> gpui::Stateful<gpui::Div> {
    let target = cx.entity();
    let external = here.clone();
    let hovered = here.clone();
    d.on_drag_move(move |e: &gpui::DragMoveEvent<DraggedFiles>, _, cx| {
        super::report_drop_target(&hovered, e, cx);
    })
    .on_drop(move |dragged: &DraggedFiles, window, cx| {
        drop_into(dragged, here.clone(), window, cx);
    })
    .on_drop(move |paths: &ExternalPaths, window, cx| {
        drop_external_into(paths, external.clone(), &target, window, cx);
    })
}

/// Toggle the side preview pane (setting shared by all tabs).
fn preview_button(on: bool, cx: &mut Context<Explorer>) -> impl IntoElement + use<> {
    use crate::settings::SettingsStore;
    div()
        .id("toggle-preview")
        .a11y_toggle(t!("ctx.preview"), on)
        .h(theme::h_ctl_sm())
        .px(theme::s_6())
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .when(on, |d| d.bg(theme::wash_strong()))
        .when(!on, |d| d.hover(|s| s.bg(theme::wash())))
        .child(
            svg()
                .path(Icon::Preview.path())
                .size(theme::icon())
                .text_color(if on {
                    theme::text()
                } else {
                    theme::text_subtle()
                }),
        )
        .toolbar_button(
            "toggle-preview",
            cx.listener(move |_, _: &ClickEvent, _, cx| {
                SettingsStore::global(cx)
                    .update(cx, |s, cx| s.update(|st| st.preview_pane = !on, cx));
            }),
        )
        .tooltip_below(t!("ctx.preview"))
}

/// Split / unsplit.
fn split_button(
    split: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        // Different ids per state so the old tooltip ("Split into two panes") doesn't linger after
        // the click.
        .id(if split { "toggle-split-off" } else { "toggle-split" })
        .a11y_toggle(
            if split {
                t!("ctx.split_off")
            } else {
                t!("ctx.split_on")
            },
            split,
        )
        .h(theme::h_ctl_sm())
        .px(theme::s_6())
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .when(split, |d| d.bg(theme::wash_strong()))
        .when(!split, |d| d.hover(|s| s.bg(theme::wash())))
        // One id in both states, so the keyboard stays on it.
        .toolbar_button("toggle-split", on_click)
        .child(
            svg()
                .path(Icon::Split.path())
                .size(theme::icon())
                .text_color(if split {
                    theme::text()
                } else {
                    theme::text_subtle()
                }),
        )
        .tooltip_below(if split {
            t!("ctx.split_off")
        } else {
            t!("ctx.split_on")
        })
}

/// Split view "Copy to/Move to the other side": thin accent outline naming the destination.
fn peer_button(
    id: &'static str,
    icon: Icon,
    label: impl std::fmt::Display,
    key: &'static str,
    show_label: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(theme::h_ctl_sm())
        .px(theme::s_8())
        .flex()
        // Truncate the folder name when space is short so the button isn't squeezed out.
        .when(show_label, |d| d.flex_shrink(1.).min_w(theme::s_32()))
        .when(!show_label, gpui::Styled::flex_none)
        .items_center()
        .gap(theme::s_6())
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::signal_line())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::text())
        .hover(|s| s.bg(theme::signal_wash()))
        .child(
            svg()
                .path(icon.path())
                .size(theme::icon())
                .flex_none()
                .text_color(theme::signal()),
        )
        .when(show_label, |d| {
            d.child(div().min_w_0().truncate().child(label.to_string()))
        })
        .a11y_lazy(gpui::Role::Button, || label.to_string())
        .aria_keyshortcuts(key)
        .tooltip_below(format!("{label} ({key})"))
        .toolbar_button(id, on_click)
}

/// One segment of a segmented toggle.
fn segment(
    id: &'static str,
    icon: Icon,
    active: bool,
    tip: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(crate::theme::scaled_px(28.))
        .h(super::viewopts::segment_h())
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
        .cursor_pointer()
        .when(active, |d| {
            d.bg(theme::segment_thumb()).shadow(theme::elev_thumb())
        })
        .when(!active, |d| d.hover(|s| s.bg(theme::wash())))
        .toolbar_button(id, on_click)
        .child(
            svg()
                .path(icon.path())
                .size(theme::icon())
                .text_color(if active {
                    theme::text()
                } else {
                    theme::text_subtle()
                }),
        )
        .a11y_lazy(gpui::Role::RadioButton, || tip.to_string())
        .aria_toggled(crate::a11y::toggled(active))
        .tooltip_below(tip.to_string())
}

/// Window buttons styled like the other icon buttons (rounded, same stroke), not imitating Windows.
/// Clicks are still handled by the system (`WindowControlArea`), so maximize gets the Windows 11
/// snap layouts menu. Maximized, they also take the screen edge above them and Close the corner
/// (`caption.rs`).
pub(crate) fn caption_buttons(window: &Window) -> impl IntoElement {
    caption_group(window).pl(theme::s_8())
}

/// [`caption_buttons`] without the space before them (the title row makes that space a drag
/// area).
pub(super) fn caption_group(window: &Window) -> gpui::Div {
    let maximized = window.is_maximized();
    let gap = theme::s_2();
    let button = |id: &'static str, icon: Icon, area: WindowControlArea, close: bool| {
        div()
            .id(id)
            .a11y_button(match area {
                WindowControlArea::Min => t!("window.minimize"),
                WindowControlArea::Max if maximized => t!("window.restore"),
                WindowControlArea::Max => t!("window.maximize"),
                _ => t!("window.close"),
            })
            // The system handles the mouse here; a screen reader's Enter comes this way (#322).
            .a11y_on_click(move |window, _| match area {
                WindowControlArea::Min => window.minimize_window(),
                WindowControlArea::Max if maximized => {
                    if let Some(hwnd) = super::Explorer::owner_from(window) {
                        crate::win::restore_window(hwnd);
                    }
                }
                WindowControlArea::Max => window.zoom_window(),
                _ => window.remove_window(),
            })
            .group(id)
            .size(theme::h_ctl())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_8())
            .window_control_area(area)
            .when(!close, |d| d.hover(|s| s.bg(theme::wash_strong())))
            .when(close, |d| d.hover(|s| s.bg(theme::danger())))
            .child(crate::caption::edge_hit(area, close, gap))
            .child(
                svg()
                    .path(icon.path())
                    .size(theme::icon())
                    .text_color(if close && crate::caption::close_edge_hovered() {
                        theme::on_signal()
                    } else {
                        theme::text_muted()
                    })
                    .when(close, |d| {
                        d.group_hover(id, |s| s.text_color(theme::on_signal()))
                    }),
            )
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(gap)
        .child(button(
            "win-min",
            Icon::WinMin,
            WindowControlArea::Min,
            false,
        ))
        .child(button(
            "win-max",
            if maximized {
                Icon::WinRestore
            } else {
                Icon::WinMax
            },
            WindowControlArea::Max,
            false,
        ))
        .child(button(
            "win-close",
            Icon::WinClose,
            WindowControlArea::Close,
            true,
        ))
}

/// Display name: with extensions hidden, files show only the stem (folders and dot-names
/// unchanged).
/// Name as displayed: the file name only (nested tree rows are named by their relative path),
/// without the extension unless `show_ext`.
pub(crate) fn shown_name(entry: &Entry, show_ext: bool) -> SharedString {
    let name = arke_core::tree::leaf(&entry.name);
    if show_ext || entry.is_dir_like() {
        return SharedString::from(name.to_owned());
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => SharedString::from(name[..dot].to_owned()),
        _ => SharedString::from(name.to_owned()),
    }
}

/// A list row's highlight: its selection background, cursor ring, hover wash and drop target.
/// It stays [`theme::row_wash_inset`] inside the row at the top and bottom, so neighbouring
/// highlighted rows keep a gap, as in File Explorer (#448), while the whole row stays the pointer
/// target. It lies under the row's content and follows the row's hover and the drag over it
/// through the row's `group`; `drop_ok` is the row's own answer to that drag (`None`: the row
/// isn't a drop target).
pub(super) fn row_wash(
    ix: usize,
    group: &SharedString,
    is_selected: bool,
    is_cursor: bool,
    focused: bool,
    drop_ok: Option<Rc<Cell<bool>>>,
) -> gpui::Div {
    let inset = theme::row_wash_inset();
    div()
        .debug_selector(move || format!("row-wash-{ix}"))
        .absolute()
        .left_0()
        .right_0()
        .top(inset)
        .bottom(inset)
        .rounded(theme::r_row())
        .border_1()
        .border_color(if is_cursor && focused {
            theme::signal_line()
        } else {
            theme::clear()
        })
        .when(is_selected, |d| d.bg(theme::list_selection(focused)))
        .when(!is_selected, |d| {
            d.group_hover(group.clone(), |s| s.bg(theme::wash()))
        })
        .when_some(drop_ok, |d, drop_ok| {
            d.can_drop(move |_, _, _| drop_ok.get())
                .group_drag_over::<DraggedFiles>(group.clone(), |s| {
                    s.bg(theme::signal_wash())
                        .border_color(theme::signal_line())
                })
                .group_drag_over::<ExternalPaths>(group.clone(), |s| {
                    s.bg(theme::signal_wash())
                        .border_color(theme::signal_line())
                })
        })
}

/// The rename box's height in a Details row `row` high: 24, or the row's height in a shorter
/// (compact) row, like Explorer's box filling its compact row.
fn rename_box_h(row: Pixels) -> Pixels {
    row.min(theme::s_24())
}

/// Space between the rename box's edge and its text: its padding plus the 1 px border.
fn rename_inset() -> gpui::Pixels {
    crate::theme::scaled_px(4. + 1.)
}

/// Names of the entries in `range` (indexes past the end are skipped).
fn names_in(entries: &[Entry], range: Range<usize>) -> Vec<Arc<str>> {
    range
        .filter_map(|ix| entries.get(ix).map(|e| Arc::clone(&e.name)))
        .collect()
}

/// Listener for a press on an item's name.
type NamePress = Box<dyn Fn(&gpui::MouseDownEvent, &mut Window, &mut App)>;

/// Name text, or the inline rename box with its problem message (if any) underneath.
///
/// `in_row`: Details rows pull the box left by its inset, so the text stays exactly where the
/// name was drawn instead of jumping right when renaming starts (grid tiles center the name, so
/// they don't need it). In a tile the box spans the tile's width: tiles center their children,
/// so without a width it would shrink to its border and hide the name (#38).
///
/// `on_press`: a press on the name itself (not the space after it), for click-to-rename (#89).
fn name_cell(
    entry: &Entry,
    rename: Option<(Entity<TextInput>, Option<NameError>)>,
    shown: SharedString,
    in_row: bool,
    on_press: Option<NamePress>,
    box_h: Pixels,
) -> AnyElement {
    match rename {
        Some((input, problem)) => div()
            .debug_selector(|| "rename-box".to_owned())
            // Tab and Shift+Tab here go to the next and previous item (`explorer_ops.rs`).
            .key_context("RenameBox")
            // Presses stay in the box: the row under it would take focus back to the list, which
            // commits the rename, and a double-click would open the file (#69).
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .relative()
            .map(|d| {
                if in_row {
                    d.flex_1().ml(-rename_inset())
                } else {
                    d.w_full().flex_none()
                }
            })
            .min_w_0()
            .h(box_h)
            // Shorter than its usual 24 (a compact row): a tighter line keeps the text whole.
            .when(box_h < theme::s_24(), |d| d.line_height(gpui::relative(theme::LH_TIGHT)))
            .px(rename_inset() - px(1.))
            .flex()
            .items_center()
            .rounded(theme::r_4())
            .bg(theme::surface())
            .border_1()
            .border_color(if problem.is_some() {
                theme::danger()
            } else {
                theme::signal_line()
            })
            // Screen readers name the box "Rename".
            .when(crate::a11y::active(), |d| {
                d.child(crate::a11y::label_scope(t!("menu.rename").into_owned().into()))
            })
            .child(input)
            .when(crate::a11y::active(), |d| {
                d.child(crate::a11y::end_label_scope())
            })
            .children(problem.map(rename_balloon))
            .into_any_element(),
        None => div()
            .flex_1()
            .min_w_0()
            .flex()
            .child(
                div()
                    .debug_selector({
                        let name = Arc::clone(&entry.name);
                        move || format!("name:{name}")
                    })
                    .min_w_0()
                    .truncate()
                    .child(shown)
                    .when_some(on_press, |d, press| {
                        d.on_mouse_down(MouseButton::Left, press)
                    }),
            )
            .into_any_element(),
    }
}

/// Why the typed name can't be used, right under the rename box like Explorer's balloon. Deferred
/// so it draws over the rows below instead of being clipped by the list.
fn rename_balloon(problem: NameError) -> impl IntoElement {
    // Zero-size anchor at the box's bottom-left corner.
    div().absolute().left_0().bottom_0().child(
        deferred(
            anchored()
                .offset(gpui::point(px(0.), theme::s_4()))
                .snap_to_window_with_margin(theme::s_8())
                .child(
                    div()
                        .id("rename-problem")
                        .a11y_live(gpui::Role::Alert, problem.to_string())
                        .occlude()
                        .max_w(rename_balloon_w())
                        .px(theme::s_8())
                        .py(theme::s_6())
                        .rounded(theme::r_8())
                        .bg(theme::overlay())
                        .shadow(theme::elev_float())
                        .border_1()
                        .border_color(theme::danger())
                        .text_size(theme::fs_12())
                        .text_color(theme::text())
                        .whitespace_normal()
                        .child(problem.to_string()),
                ),
        )
        .with_priority(1),
    )
}

/// Item visual: thumbnail or system icon if available (thumbnails fill `image_size`), else the
/// layered kind icon (`icon_size`).
pub(crate) fn entry_visual(
    entry: &Entry,
    image: Option<Arc<RenderImage>>,
    image_size: Pixels,
    icon_size: Pixels,
) -> AnyElement {
    let visual = plain_visual(entry, image, image_size, icon_size);
    if entry.is_link() {
        // Symlinks and junctions get Explorer's shortcut arrow.
        return super::details::with_link_badge(visual, icon_size);
    }
    visual
}

/// [`entry_visual`], except that a server's shares (`share`: the folder listed is `\\nas`) get the
/// network share icon instead of a folder.
pub(crate) fn listed_visual(
    share: bool,
    entry: &Entry,
    image: Option<Arc<RenderImage>>,
    image_size: Pixels,
    icon_size: Pixels,
) -> AnyElement {
    if share {
        return crate::assets::tinted(Icon::NetDrive, icon_size, theme::text_muted());
    }
    entry_visual(entry, image, image_size, icon_size)
}

/// Whether `dir` is a server whose items are its shares (see [`listed_visual`]).
pub(crate) fn lists_shares(dir: Option<&Path>) -> bool {
    dir.and_then(arke_core::net::share_server).is_some()
}

fn plain_visual(
    entry: &Entry,
    image: Option<Arc<RenderImage>>,
    image_size: Pixels,
    icon_size: Pixels,
) -> AnyElement {
    if let Some(image) = image {
        // System icons are small squares and keep icon size; thumbnails fill the image area.
        let size = if image.size(0).width.0 <= 64 {
            icon_size
        } else {
            image_size
        };
        return img(image)
            .size(size)
            .flex_none()
            .rounded(theme::r_4())
            .object_fit(ObjectFit::Contain)
            .into_any_element();
    }
    kind_icon(file_kind(entry), icon_size)
}

/// Layered kind icon: folders are two amber layers, files are white paper, outline and a colored
/// glyph.
pub(crate) fn kind_icon(kind: FileKind, size: Pixels) -> AnyElement {
    let layer = |icon: Icon, color: gpui::Rgba| {
        svg()
            .absolute()
            .top_0()
            .left_0()
            .size(size)
            .path(icon.path())
            .text_color(color)
    };
    let base = div().relative().size(size).flex_none();
    let small = size <= small_icon();
    if kind == FileKind::Folder {
        let (back, front) = theme::folder_colors();
        if small {
            return base
                .child(layer(Icon::FolderBack, back))
                .child(layer(Icon::FolderFront, front))
                .into_any_element();
        }
        // Large: a deeper back panel, a sheet of paper peeking out and a lit front flap edge, so
        // the folder has depth next to thumbnails instead of being a flat block.
        let (deep, paper, lip) = theme::folder_depth_colors();
        return base
            .child(layer(Icon::FolderLgBack, deep))
            .child(layer(Icon::FolderLgPaper, paper))
            .child(layer(Icon::FolderLgFront, front))
            .child(layer(Icon::FolderLgLip, lip))
            .into_any_element();
    }
    let glyph = theme::file_glyph(kind);
    // Small icons: tint the paper with the kind color and enlarge the glyph around its center,
    // staying within the paper.
    let (fill, line) = match glyph {
        Some((_, color)) if small => theme::small_page_colors(color),
        _ => theme::page_colors(),
    };
    base.child(layer(Icon::PageFill, fill))
        .child(layer(Icon::PageLine, line))
        .children(glyph.map(|(icon, color)| {
            if !small {
                return layer(icon, color);
            }
            // The glyph is centered at roughly (8, 9.8) on the 16-unit canvas; scale by GLYPH_ZOOM
            // around that center.
            let big = size * GLYPH_ZOOM;
            svg()
                .absolute()
                .left(size * (-(GLYPH_ZOOM - 1.) * 0.5))
                .top(size * (-(GLYPH_ZOOM - 1.) * (9.8 / 16.)))
                .size(big)
                .path(icon.path())
                .text_color(color)
        }))
        .into_any_element()
}

/// Icons at or below this size count as small (list, Ctrl+T, menus)
fn small_icon() -> gpui::Pixels {
    crate::theme::scaled_px(20.)
}
/// Glyph scale for small icons
const GLYPH_ZOOM: f32 = 1.25;

/// Size column: files show their own size; folders use the background result (a dim "…" or the last
/// value while computing).
pub(super) fn size_cell(entry: &Entry, folder: super::sizes::FolderSize) -> impl IntoElement {
    let (text, subtle) = size_text(entry, folder);
    div()
        .w(crate::prefs::size_width())
        .flex_none()
        .flex()
        .justify_end()
        .text_color(if subtle {
            theme::text_subtle()
        } else {
            theme::text_muted()
        })
        .child(text)
}

/// The Size column's text, and whether it's dimmed (a folder's size still being computed).
pub(super) fn size_text(entry: &Entry, folder: super::sizes::FolderSize) -> (String, bool) {
    use super::sizes::FolderSize;
    if entry.is_dir_like() {
        match folder {
            FolderSize::Known { bytes, fresh } => (format_size(bytes), !fresh),
            FolderSize::Computing => ("…".to_owned(), true),
            FolderSize::Unknown => (String::new(), false),
        }
    } else {
        (format_size(entry.size), false)
    }
}

impl Explorer {
    /// Floating layers over the list: path bar menus, context menu, Quick Look.
    fn render_floating(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        [
            self.render_path_overlays(window, cx),
            self.render_context_menu(cx),
            self.render_quick_look(window, cx),
            self.render_confirm(window, cx),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

impl Explorer {
    /// Split view: a press in this side makes it the active one. Dividers stop their presses,
    /// so they call this themselves.
    pub(super) fn activate_on_press(&self, cx: &mut Context<Self>) {
        if self.pane.split && !self.pane.active {
            cx.emit(ExplorerEvent::Activated);
        }
    }

    /// Before each render: measure widths, collect background results, start background work as
    /// needed (preview, parent, folder sizes).
    fn before_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_frame(window, cx);
        self.follow_localized(cx);
        // Windows' type names arrive after the first sort: order by what the column shows.
        if self.sort.uses(SortColumn::Type)
            && self.type_order != Some(arke_core::format::type_names_version())
        {
            self.ensure_sorted(cx);
        }
        self.track_notice_exit(cx);
        self.track_progress_toasts(cx);
        self.screen = (window.viewport_size(), window.scale_factor());
        let window_w = window.viewport_size().width;
        // A split tab's shared preview takes its room first; the panes share the rest.
        self.paper_width =
            (window_w - theme::sidebar_width() - self.pane.reserve) * self.pane.share;
        self.bar_width = self.paper_width
            + if self.pane.trailing {
                self.pane.reserve
            } else {
                px(0.)
            };
        self.fit = self.compute_fit(self.paper_width, window_w);
        self.pane_width = self.paper_width
            - if self.preview_pane && self.fit.preview {
                self.fit.preview_w
            } else {
                px(0.)
            };
        // While a column divider is dragged, Name keeps its width: fitting it to the other
        // columns live would move the dragged column's left edge, which the drag measures from.
        // It fits again on release.
        let held_name = (self.column_resized && self.column_drag.is_some())
            .then(|| self.cols.map(|c| c.name))
            .flatten();
        self.cols = self.compute_columns(held_name);
        self.header_x.set(None);
        self.poll_pending_create(cx);
        self.poll_pending_rename(window, cx);
        self.poll_too_big_ask(window, cx);
        self.schedule_notice_dismiss(cx);
        self.sync_preview(cx);
        self.sync_parent_column();
        self.sync_link_targets();
        self.sync_folder_sizes(cx);
        self.sync_cloud_states(window, cx);
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::a11y::sync(window);
        // What the lists below ask of the media cache this frame replaces what they asked before.
        let (window_id, view) = (window.window_handle().window_id(), cx.entity_id());
        self.media
            .update(cx, |m, cx| m.drawing(window_id, view, cx));
        self.before_render(window, cx);
        let searching = self.search_box_state(window, cx) == layout::SearchBox::Typing;
        div()
            .id("explorer")
            .key_context("Explorer")
            .size_full()
            .flex()
            .flex_col()
            .text_color(theme::text())
            .ui_font()
            .text_size(theme::fs_14())
            .map(|d| Self::root_actions(d, cx))
            // Split view: clicking a side makes it active.
            .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.activate_on_press(cx);
            }))
            .map(|d| Self::mouse_navigation(d, cx))
            .child(
                // The paper: the single sheet holding the list
                div()
                    .debug_selector(|| "paper".to_owned())
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .mr(self.paper_margin_right())
                    .mb(theme::paper_gap())
                    .rounded(theme::r_paper())
                    .bg(theme::paper())
                    .shadow(theme::elev_2())
                    .children(backdrop_layer())
                    .child(self.render_pane_row(window, cx))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    // Grid view has no columns, so no header. Also none while "this
                                    // PC" has no results (off, no text, still building).
                                    .when(
                                        self.failed.is_none()
                                            && (self.search_mode
                                                || self.view_mode == ViewMode::Details)
                                            && !(self.search_global
                                                && self.search_results.is_none()),
                                        |d| d.child(self.render_header(cx)),
                                    )
                                    .child(self.render_list_appearing(window, cx)),
                            )
                            .children(self.render_preview_appearing(window, cx)),
                    )
                    .children((self.pane.split && self.pane.active).then(active_outline))
                    .children(searching.then(search_outline))
                    .children(self.render_toast_stack(cx)),
            )
            .children(self.render_floating(window, cx))
            .children(self.render_right_drag_listener(cx))
    }
}

impl Explorer {
    /// The pane's commands, reachable from anywhere in the pane and from its search box (which
    /// is drawn in the title row, outside the pane).
    pub(super) fn root_actions(
        el: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::focus_path_bar))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::focus_computer_search))
            .on_action(cx.listener(Self::toggle_search_mode))
            .on_action(cx.listener(Self::copy_as_path))
            .on_action(cx.listener(Self::add_to_shelf))
            .on_action(cx.listener(Self::toggle_hidden))
            .on_action(cx.listener(Self::copy_files))
            .on_action(cx.listener(Self::cut_files))
            .on_action(cx.listener(Self::paste_files))
            .on_action(cx.listener(Self::delete_files))
            .on_action(cx.listener(Self::delete_permanently))
            .on_action(cx.listener(Self::undo_file_op))
            .on_action(cx.listener(Self::redo_file_op))
            .on_action(cx.listener(Self::rename_file))
            .on_action(cx.listener(Self::rename_next))
            .on_action(cx.listener(Self::rename_prev))
            .on_action(cx.listener(Self::new_folder))
            .map(|d| Self::view_actions(d, cx))
            .on_action(cx.listener(Self::copy_to_peer))
            .on_action(cx.listener(Self::move_to_peer))
            .on_action(cx.listener(|this, _: &FocusOtherPane, _, cx| {
                if this.pane.split {
                    cx.emit(ExplorerEvent::FocusOtherPane);
                } else {
                    cx.propagate();
                }
            }))
    }

    /// Tests: whether the preview pane fit at the last render, and its width.
    #[cfg(test)]
    pub(crate) fn fit_for_test(&self) -> (bool, f32) {
        (self.fit.preview, f32::from(self.fit.preview_w))
    }

    /// Space right of the paper: the window margin, and in a split tab's rightmost pane the
    /// shared preview too (the pane's box runs on under it).
    fn paper_margin_right(&self) -> Pixels {
        if self.pane.trailing {
            layout::paper_margin() + self.pane.reserve
        } else {
            layout::paper_margin()
        }
    }

    /// Tests: the paper's box and the top bar's width at the last render.
    #[cfg(test)]
    pub(crate) const fn widths_for_test(&self) -> (Pixels, Pixels) {
        (self.paper_width, self.bar_width)
    }

    /// Tests: how wide the preview pane may be dragged at the last render.
    #[cfg(test)]
    pub(crate) fn preview_room_for_test(&self) -> f32 {
        f32::from(layout::preview_room(
            &self.fit_input(self.paper_width, self.screen.0.width),
        ))
    }

    /// Decides whether the preview pane fits in `bar` (the pane's full width) in a window `window`
    /// wide. See [`layout::fit`].
    fn compute_fit(&self, bar: Pixels, window: Pixels) -> layout::Fit {
        layout::fit(&self.fit_input(bar, window))
    }

    /// This pane's narrowest list: a split tab's shared preview narrows before a side gets
    /// narrower than this.
    pub(crate) fn bare_list_width(&self) -> Pixels {
        layout::bare_list(&self.fit_input(self.paper_width, self.screen.0.width))
    }

    /// What [`layout::fit`] needs to know about this pane.
    pub(super) fn fit_input(&self, bar: Pixels, window: Pixels) -> layout::FitInput {
        let mut input = layout::FitInput {
            bar,
            window,
            details: self.view_mode == ViewMode::Details && !self.search_mode,
            checkboxes: self.show_checkboxes,
            columns_view: self.columns_active(),
            search: self.search_mode,
            // A split tab's preview pane is drawn once for the tab, not by each side.
            wants_preview: (self.preview_pane && !self.pane.split) || self.columns_active(),
            preview_user: crate::prefs::preview_width(),
            parent_w: Self::parent_column_width(),
            current_w: crate::prefs::view_column_width(crate::prefs::ViewColumn::Current),
            location_w: theme::col_location(),
            col_w: crate::prefs::Col::ALL.map(|col| {
                // Search results always show modified and size.
                let on = match col {
                    crate::prefs::Col::Modified | crate::prefs::Col::Size => {
                        col.shown() || self.search_mode
                    }
                    crate::prefs::Col::Created => col.shown() && !self.search_mode,
                    _ => col.shown(),
                };
                on.then(|| crate::prefs::width(col))
            }),
        };
        // The parent column's width is shared by all panes: never more than this one leaves it.
        input.parent_w = input.parent_w.min(layout::parent_room(&input));
        input
    }

    /// Preview pane slides in from the right when it is turned on (not when a tab showing it is
    /// drawn again). Turning it off is instant: the list takes the room back at once, and animating
    /// the width would relayout every row on every frame.
    fn render_preview_appearing(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pane = self.render_preview_pane(cx)?;
        let description = crate::a11y::active()
            .then(|| self.preview_a11y_description())
            .flatten();
        Some(
            self.preview_cue
                .slide(
                    div()
                .id("preview-pane")
                .debug_selector(|| "preview-pane".to_owned())
                // A click here leaves the search box (and the path being edited), as one on the
                // list does; the preview itself takes no focus.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.cancel_path_edit(window, cx);
                    }),
                )
                .a11y(gpui::Role::Region, t!("settings.preview"))
                .when_some(
                    description,
                    gpui::StatefulInteractiveElement::aria_description,
                )
                .relative()
                .flex()
                .flex_none()
                .h_full()
                .child(pane)
                .child(self.preview_resize_handle(false, cx))
                // Drops land in this pane's folder here too, not only over the list: dragging
                // to the other side of a split shouldn't depend on missing its preview pane.
                .when_some(self.current_path().map(Path::to_path_buf), |d, here| {
                    accept_drops_into(d, here, cx)
                }),
                    window,
                    crate::motion::ease_out,
                    crate::motion::SLIDE_LG,
                    0.,
                )
                .into_any_element(),
        )
    }

    /// Split tab: the tab's one preview, drawn by the workspace at the tab's right end, showing
    /// this pane's item (the active side). `None` when this pane isn't the one it follows.
    pub(crate) fn render_shared_preview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let shared = self.pane.preview_host?;
        // The workspace draws this before the pane renders: catch up with the cursor now, so
        // switching sides never shows the other side's item for a frame.
        self.sync_preview(cx);
        let frame = Self::preview_frame(shared.width);
        let contents = if self.quick_look {
            // Quick Look shows the item; the paper stays so neither side moves meanwhile.
            None
        } else if self.preview_shown() {
            // The column view previews in its own last column (when it has room for one).
            Some(
                frame
                    .items_center()
                    .justify_center()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(t!("preview.in_last_column").to_string())
                    .into_any_element(),
            )
        } else {
            Some(self.preview_column(frame, cx))
        };
        let description = crate::a11y::active()
            .then(|| self.preview_a11y_description())
            .flatten();
        Some(
            self.preview_cue
                .slide(
                    div()
                .id("shared-preview")
                .debug_selector(|| "shared-preview".to_owned())
                // A click here leaves the search box, as one in the single-pane preview does.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.cancel_path_edit(window, cx);
                    }),
                )
                .a11y(gpui::Role::Region, t!("settings.preview"))
                .when_some(
                    description,
                    gpui::StatefulInteractiveElement::aria_description,
                )
                // It covers the end of the right pane's box: no hover or clicks reach the pane.
                .occlude()
                .relative()
                .size_full()
                .flex()
                .rounded(theme::r_paper())
                .bg(theme::paper())
                .shadow(theme::elev_2())
                .children(contents)
                .child(self.preview_resize_handle(true, cx))
                // Drops land in the folder of the side it shows.
                .when_some(self.current_path().map(Path::to_path_buf), |d, here| {
                    accept_drops_into(d, here, cx)
                }),
                    window,
                    crate::motion::ease_out,
                    crate::motion::SLIDE_LG,
                    0.,
                )
                .into_any_element(),
        )
    }

    /// The list. Choosing another layout brightens it in quickly from a dim start (never from
    /// blank, so no frame shows an empty pane). Folder and tab switches, search and refresh draw it
    /// at once: they must never wait on an animation.
    fn render_list_appearing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        const FLOOR: f32 = 0.4;
        let list = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_list(window, cx));
        // Styled in place from the cue's clock: the list is never wrapped, so a click made while
        // it brightens still lands.
        let Some(t) = self.view_cue.progress() else {
            return list.into_any_element();
        };
        window.request_animation_frame();
        list.opacity(FLOOR + (1. - FLOOR) * crate::motion::ease_out(t))
            .into_any_element()
    }
}

/// Background image covering the list paper, drawn underneath (gpui reads and decodes it in the
/// background). None if unset.
fn backdrop_layer() -> Option<impl IntoElement> {
    let (path, opacity) = theme::backdrop()?;
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .rounded(theme::r_paper())
            .overflow_hidden()
            .opacity(opacity)
            .child(
                gpui::img(path)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover),
            ),
    )
}

/// Date column (modified, created); short or full format per settings.
pub(super) fn date_cell(
    time: Option<std::time::SystemTime>,
    width: Pixels,
    shell: &dyn crate::shell::Shell,
) -> impl IntoElement {
    div()
        .w(width)
        .flex_none()
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .child(time.map_or_default(|t| {
            if crate::prefs::full_dates() {
                shell.format_time(t)
            } else {
                shell.format_time_short(t)
            }
        }))
}

/// Cloud file state after the name, like Explorer: a cloud for online-only; inside sync roots a
/// check for local files, a filled one for "Always keep on this device", arrows while syncing and
/// a warning for files that aren't synced. Hover names the state. Reads attributes and
/// placeholder info only, never contents.
fn cloud_badge(
    ix: usize,
    entry: &Entry,
    in_cloud: bool,
    sync: Option<arke_core::entry::CloudSync>,
) -> Option<impl IntoElement> {
    use arke_core::entry::CloudStatus as S;
    let (icon, color, tip) = match entry.cloud_status(in_cloud, sync)? {
        S::OnlineOnly => (Icon::Cloud, theme::text_subtle(), t!("cloud.online_only")),
        S::Local => (Icon::CloudLocal, theme::synced(), t!("cloud.local")),
        S::Pinned => (Icon::CloudPinned, theme::synced(), t!("cloud.pinned")),
        S::Syncing => (Icon::CloudSyncing, theme::syncing(), t!("cloud.syncing")),
        S::NotSynced => (Icon::CloudError, theme::danger(), t!("cloud.not_synced")),
    };
    Some(
        div()
            .id(("cloud-badge", ix))
            .flex_none()
            .tooltip(tooltip(tip.into_owned()))
            .child(
                svg()
                    .path(icon.path())
                    .size(theme::icon())
                    .text_color(color),
            ),
    )
}

impl Explorer {
    /// Mouse back/forward over this pane go back in this pane; the window handles them everywhere
    /// else (sidebar, tab list, pages), so they stop here to move only once (#8). They act on release, like File Explorer
    /// and browsers (#497); a release that went to another window never reaches here.
    fn mouse_navigation(
        d: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        d.on_mouse_up(
            MouseButton::Navigate(NavigationDirection::Back),
            cx.listener(|this, _: &MouseUpEvent, w, cx| {
                cx.stop_propagation();
                this.go_back(&GoBack, w, cx);
            }),
        )
        .on_mouse_up(
            MouseButton::Navigate(NavigationDirection::Forward),
            cx.listener(|this, _: &MouseUpEvent, w, cx| {
                cx.stop_propagation();
                this.go_forward(&GoForward, w, cx);
            }),
        )
    }
}

/// Scrolls the list only along the axis of the gesture: a sideways swipe on a touchpad is back
/// or forward (#70), and without this GPUI turns it into a scroll down a list that only scrolls
/// vertically.
fn sideways_is_not_down(mut list: gpui::UniformList) -> gpui::UniformList {
    list.style().restrict_scroll_to_axis = Some(true);
    list
}
