//! Rendering for `Explorer`.
//!
//! - **Top bar** is drawn on the frame (transparent): back/forward/up, path, drag area, filter box,
//!   window buttons.
//! - **Paper** (`surface` + `elev_2`): context bar, column headers, list; toasts sit in the paper's
//!   bottom-right.
//!
//! Only `theme` tokens are allowed (enforced by `tests::ui_code_uses_theme_tokens`).

use rust_i18n::t;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arke_core::Entry;
use arke_core::NavKind;
use arke_core::filetype::{FileKind, file_kind};
use arke_core::format::{format_count, format_size};
use arke_core::path::NameError;
use arke_core::search::MAX_HITS;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, ExternalPaths, Focusable, MouseButton,
    MouseDownEvent, NavigationDirection, ObjectFit, Pixels, RenderImage, SharedString, Window,
    WindowControlArea, anchored, deferred, div, img, prelude::*, px, svg, uniform_list,
};

use super::layout;
use super::{
    CopyFiles, CutFiles, DeleteFiles, ErrorAction, Escape, Explorer, ExplorerEvent, FocusOtherPane,
    GoBack, GoForward, GoUp, NewFolder, OpenSelected, PasteFiles, Refresh, RenameFile, ViewColumns,
    ViewDetails, ViewMode,
};
use super::{DragPreview, DraggedFiles, drop_external_into, drop_into};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::media::MediaKey;
use crate::text_input::TextInput;
use crate::theme::{self, UiFont as _};
use crate::workspace::TooltipBelowExt as _;

/// Empty space after the last row (about three rows) so a long, fully scrolled list still has room
/// to right-click for the folder menu, click to deselect, or drop files into the folder.
///
/// It's added as blank trailing list items (see [`tail_items`]), not as bottom padding:
/// `uniform_list` only draws rows inside its padded area, so padding would leave a blank band at
/// the bottom of the list at every scroll position, not just after the last row.
const LIST_TAIL: Pixels = px(96.);

/// Number of blank trailing items of height `item` that make up [`LIST_TAIL`]. The row callbacks
/// draw nothing for indexes past the real items, so these are just empty space.
fn tail_items(item: Pixels) -> usize {
    if item <= px(0.) {
        return 0;
    }
    // A handful at most (3 rows, or one tile row), so the cast can't truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let count = (LIST_TAIL / item).ceil() as usize;
    count
}

/// How long regular toasts stay before closing
const NOTICE_TTL: Duration = Duration::from_secs(5);
/// A notice with Undo stays longer, so there's time to reach the button.
const NOTICE_UNDO_TTL: Duration = Duration::from_secs(10);
/// Widest the rename problem message gets before wrapping.
const RENAME_BALLOON_W: Pixels = px(440.);
/// Above this many selected items the total size isn't computed (avoids a long walk on every
/// render)
const SIZE_SUM_MAX: usize = 5_000;
/// Below this pane width the filter box collapses to a magnifier, leaving room for the path
const NARROW_TOP_BAR: Pixels = px(760.);
/// Below this pane width context bar buttons show icons only
const COMPACT_BAR: Pixels = px(860.);
/// Below this pane width "Copy to/Move to the other side" show icons only (split view)
const PEER_LABELS_MIN: Pixels = px(600.);
/// Scrollbar width (subtracted when computing grid columns)
const SCROLLBAR_GUTTER: Pixels = px(12.);

impl Explorer {
    // ───────────────────────── Top bar (on the frame) ─────────────────────────

    fn render_top_bar(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (can_back, can_forward) = self
            .global_nav
            .unwrap_or_else(|| (self.history.can_go_back(), self.history.can_go_forward()));
        let can_up = self
            .display_path()
            .and_then(arke_core::path::parent)
            .is_some();
        let nav_here = !self.pane.nav_in_sidebar;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_2)
            .h(theme::H_BAR)
            .pr(theme::S_8)
            .when(nav_here, |d| {
                d.child(crate::spring::back_target(
                    "list",
                    crate::spring::Target::Explorer(cx.entity().downgrade()),
                    can_back,
                    ghost_button(
                        "back",
                        Icon::ArrowLeft,
                        can_back,
                        t!("nav.back"),
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            cx.emit(ExplorerEvent::HistoryButton(true));
                        }),
                    ),
                    cx,
                ))
                .child(ghost_button(
                    "forward",
                    Icon::ArrowRight,
                    can_forward,
                    t!("nav.forward"),
                    cx.listener(|_, _: &ClickEvent, _, cx| {
                        cx.emit(ExplorerEvent::HistoryButton(false));
                    }),
                ))
            })
            .child(ghost_button(
                "up",
                Icon::ArrowUp,
                can_up,
                t!("nav.up"),
                cx.listener(|this, _: &ClickEvent, w, cx| this.go_up(&GoUp, w, cx)),
            ))
            .when(nav_here && !self.pane.split, |d| d.child(ghost_button(
                "refresh",
                Icon::Refresh,
                self.model.is_some(),
                t!("nav.refresh"),
                cx.listener(|this, _: &ClickEvent, w, cx| this.refresh(&Refresh, w, cx)),
            )))
            // Space after the path drags the window and maximizes on double click (the system
            // handles both through the caption hit test).
            .child({
                let drag = div()
                    .id("drag")
                    .window_control_area(WindowControlArea::Drag)
                    // Click on empty title bar space ends path editing (not consumed, so the window
                    // still drags).
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.cancel_path_edit(window, cx);
                        }),
                    )
                    .on_mouse_up(
                        gpui::MouseButton::Right,
                        cx.listener(|_, e: &gpui::MouseUpEvent, _, cx| {
                            cx.emit(ExplorerEvent::WindowMenu(e.position));
                            cx.stop_propagation();
                        }),
                    );
                self.render_path_slot(drag, window, cx)
            })
            // The operation center is window-wide state, so split view shows it once (right pane).
            .when(self.pane.show_ops, |d| {
                d.children(self.render_op_center(window, cx))
            })
            .child(self.render_filter_box(window, cx))
            .when(self.pane.show_caption, |d| d.child(caption_buttons(window)))
    }

    fn render_filter_box(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let in_box = self
            .filter_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        // The search panel below counts as part of the box.
        let focused = in_box || self.panel_focus.contains_focused(window, cx);
        let active = !self.query.is_empty() || self.search_mode;
        // When narrow, show only the magnifier until clicked (or Ctrl+F), leaving room for the
        // path.
        let collapsed = !focused
            && !active
            && if self.pane.split {
                self.narrow()
            } else {
                self.bar_width < NARROW_TOP_BAR
            };
        let filter_box = div()
            .id("filter-box")
            .key_context("FilterBar")
            .on_action(cx.listener(Self::filter_to_list))
            .w_full()
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.focus_filter_input(window, cx);
            }))
            .overflow_hidden()
            .when(collapsed, |d| d.tooltip_below(t!("filter.placeholder")))
            .h(theme::H_CTL)
            .flex()
            .items_center()
            .gap(theme::S_6)
            .when(collapsed, gpui::Styled::justify_center)
            .when(!collapsed, |d| d.pl(theme::S_12).pr(theme::S_4))
            .rounded(theme::R_8)
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
                    .size(theme::ICON)
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
                        .child(self.filter_input.clone()),
                )
            })
            .when(!self.query.is_empty(), |d| d.child(Self::filter_clear(cx)))
            // Scope other than "this folder": show it on the right.
            .when(!collapsed, |d| d.children(self.render_scope_chip(focused, cx)));
        let filter_box = Self::bind_scope_actions(filter_box, cx);
        div()
            .relative()
            .w(if collapsed {
                theme::H_CTL
            } else if self.pane.split {
                px(150.)
            } else {
                theme::FILTER_W
            })
            .min_w(theme::H_CTL)
            .when(!self.pane.split, gpui::Styled::flex_none)
            .mr(theme::S_8)
            .child(filter_box)
            // While the box has focus, the scope strip shows below (this folder / subfolders / this
            // PC). It collapses once typing starts so it doesn't cover the list (Tab or the scope
            // chip reopens it).
            .when(
                focused
                    && !self.panel_dismissed
                    && (self.query.is_empty() || self.scope_strip_open),
                |d| d.child(self.render_scope_strip(window, cx)),
            )
    }

    /// The × in the filter box: clear the text and go back to "this folder".
    fn filter_clear(cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("filter-clear")
            .a11y_button(t!("a11y.clear_filter"))
            .size(theme::S_20)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme::ICON_SM)
                    .text_color(theme::text_muted()),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.reset_filter(cx);
                window.focus(&this.list_focus, cx);
                cx.notify();
            }))
    }

    // ───────────────────────── Context bar ─────────────────────────

    /// The left side changes: folder info with no selection, actions for the selection otherwise.
    /// View tools on the right never move, even with a selection, so they can't be misclicked.
    fn render_context_bar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact_bar();
        let left = if !self.search_mode && !self.selection.is_empty() {
            self.render_selection_actions(compact, cx)
        } else {
            self.render_folder_actions(compact, cx)
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_4)
            .h(theme::H_CTX)
            .pl(theme::S_20)
            .pr(theme::S_12)
            .child(left)
            .child(vertical_divider())
            .child(self.render_view_controls(window, cx))
            .into_any_element()
    }

    /// Context bar buttons show icons only (labels move to tooltips): in split view or when the
    /// pane is narrow.
    pub(super) fn compact_bar(&self) -> bool {
        self.pane.split || self.bar_width < COMPACT_BAR
    }

    fn render_folder_actions(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        // A server's share list takes no new items.
        let loaded = self.model.is_some() && !lists_shares(self.current_path());
        let can_paste = loaded && !self.search_mode && self.services.shell.clipboard_has_files();
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(theme::S_4)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(self.info_parts(cx).join(" · ")),
            )
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
            .into_any_element()
    }

    /// View tools: details/icons/columns with the view options flyout, preview pane, split.
    fn render_view_controls(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_4)
            // Split panes are half width: the switcher folds into the view options button (the
            // flyout has the same choices), leaving room for Copy to/Move to.
            .when(!self.pane.split, |d| {
                d.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_2)
                    .p(theme::S_2)
                    .rounded(theme::R_8)
                    // Same segmented style as the search scope strip: a recessed track with the
                    // current view as a raised pill.
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
                    .child(self.render_view_options(true, window, cx)),
            )
            })
            .when(self.pane.split, |d| {
                d.child(self.render_view_options(false, window, cx))
            })
            .when(self.pane.pair_toggles, |d| d.child(self.render_dual_toggles(cx)))
            .child(preview_button(self.preview_pane, cx))
            .child(split_button(
                self.pane.split,
                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ExplorerEvent::ToggleSplit)),
            ))
    }

    fn render_selection_actions(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        // `compact` decides whether the summary includes the total size; action buttons are always
        // icon-only to keep the bar uncluttered.
        let narrow = compact;
        let compact = true;
        let count = self.selection.len();
        // Split view shows only the count, leaving room for "Copy to/Move to <destination>".
        let mut summary = if self.pane.split {
            t!("sel.count_short", count = format_count(count)).into_owned()
        } else {
            t!("sel.count", count = format_count(count)).into_owned()
        };
        // Skip the total size when narrow (including split view) to leave room for actions.
        if let Some(size) = self.selected_size().filter(|_| !narrow) {
            let _ = write!(summary, " · {}", format_size(size));
        }
        // Split view with both sides on different folders: Copy to/Move to take the room of the
        // regular buttons. Same folder: they'd copy items onto themselves, so they're left out and
        // the regular buttons come back.
        let peer_actions = self.render_peer_actions(cx);
        let crowded = peer_actions.is_some();
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(theme::S_4)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_8)
                    .mr(theme::S_4)
                    .font_weight(theme::FW_STRONG)
                    .child(
                        div()
                            .size(theme::S_6)
                            .rounded(theme::R_FULL)
                            .bg(theme::signal()),
                    )
                    .child(summary),
            )
            .child(vertical_divider())
            .children(peer_actions)
            // Omitted next to Copy to/Move to at half width: Enter, Ctrl+X, Ctrl+C and the context
            // menu still work.
            .when(!crowded, |d| {
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
                .child(action_button(
                    "sel-cut",
                    Icon::Cut,
                    t!("sel.cut"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| this.cut_files(&CutFiles, w, cx)),
                ))
                .child(action_button(
                    "sel-copy",
                    Icon::Copy,
                    t!("sel.copy"),
                    Tone::Normal,
                    compact,
                    cx.listener(|this, _: &ClickEvent, w, cx| this.copy_files(&CopyFiles, w, cx)),
                ))
            })
            // Omitted next to Copy to/Move to (F2 still works) to leave room for them.
            .when(count == 1 && !crowded, |d| {
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
            .child(action_button(
                "sel-delete",
                Icon::Trash,
                t!("sel.delete"),
                Tone::Danger,
                compact,
                cx.listener(|this, _: &ClickEvent, w, cx| this.delete_files(&DeleteFiles, w, cx)),
            ))
            .child(div().flex_1())
            .child(clear_selection_button(
                compact,
                cx.listener(|this, _: &ClickEvent, w, cx| this.escape(&Escape, w, cx)),
            ))
            .into_any_element()
    }

    /// The other pane's folder, when copying or moving there makes sense: split view with the other
    /// side somewhere else. Both sides on the same folder would copy items onto themselves.
    fn peer_target(&self) -> Option<&Path> {
        let peer = self.pane.peer.as_deref()?;
        (self.current_path() != Some(peer)).then_some(peer)
    }

    /// Split view: "Copy to/Move to the other side", naming the destination. In a narrow pane
    /// they show only their icons (the tooltip still names the destination), since a label cut
    /// down to "…" says nothing.
    fn render_peer_actions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let peer = self.peer_target()?.to_path_buf();
        let labels = self.paper_width >= PEER_LABELS_MIN;
        let name = peer.file_name().map_or_else(
            || peer.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Some(
            div()
                .flex()
                .min_w_0()
                .items_center()
                .gap(theme::S_4)
                .child(peer_button(
                    "sel-copy-peer",
                    Icon::Copy,
                    t!("sel.copy_to", name = name),
                    "Alt+C",
                    labels,
                    cx.listener(|this, _: &ClickEvent, _, cx| this.transfer_to_peer(false, cx)),
                ))
                .child(peer_button(
                    "sel-move-peer",
                    Icon::Cut,
                    t!("sel.move_to", name = name),
                    "Alt+M",
                    labels,
                    cx.listener(|this, _: &ClickEvent, _, cx| this.transfer_to_peer(true, cx)),
                ))
                .child(vertical_divider()),
        )
    }

    /// Item `ix` is a drop target if it's a folder.
    fn drop_dir(&self, ix: usize) -> Option<PathBuf> {
        if self.search_mode {
            return None;
        }
        let model = self.model.as_ref()?;
        let entry = self.view()?.get(ix)?.clone();
        entry.is_dir_like().then(|| model.path.join(&*entry.name))
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
    pub(super) fn info_parts(&self, cx: &App) -> Vec<String> {
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
        if let Some(model) = &self.model {
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
        // Nested tree rows are named by relative path; read the file name.
        let name = arke_core::tree::leaf(&entry.name);
        crate::a11y::item_label(
            name,
            &[&arke_core::format::type_label(entry), &size, &modified],
        )
    }

    /// Screen reader name of the file list: the folder shown, or the search.
    fn list_a11y_label(&self) -> String {
        if self.search_mode {
            return t!("filter.search_placeholder").into_owned();
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
        Self::item_shell(ix, is_selected, is_cursor, focused, drop_dir, cx)
            .relative()
            .map(|d| match width {
                Some(w) => d.w(w).flex_none(),
                None => d.w_full(),
            })
            .h(height)
            .flex()
            .items_center()
            .pl(layout::ROW_PAD_L)
            .pr(layout::ROW_PAD_R)
            .gap(layout::COL_GAP)
    }

    /// Shared by rows and tiles: selection background, cursor ring, mouse events.
    fn item_shell(
        ix: usize,
        is_selected: bool,
        is_cursor: bool,
        focused: bool,
        drop_dir: Option<PathBuf>,
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
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if this.on_row_click(ix, event, cx) {
                    return;
                }
                if this.click_select == Some(ix) && event.click_count() == 1 {
                    this.click_select = None;
                    if let Some(view) = this.view() {
                        this.selection.select_only(&view, ix);
                        cx.notify();
                    }
                }
            }))
            // Drop: folders
            .when_some(drop_dir, move |d, dir| {
                let external = dir.clone();
                let check = dir.clone();
                d.can_drop(move |value, _, cx| super::accepts_drop(value, &check, cx))
                    .drag_over::<DraggedFiles>(|s, _, _, _| {
                    s.bg(theme::signal_wash()).border_color(theme::signal_line())
                })
                .drag_over::<ExternalPaths>(|s, _, _, _| {
                    s.bg(theme::signal_wash()).border_color(theme::signal_line())
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
            .rounded(theme::R_8)
            .border_1()
            .border_color(if is_cursor && focused {
                theme::signal_line()
            } else {
                theme::clear()
            })
            .when(is_selected, |d| d.bg(theme::list_selection(focused)))
            .when(!is_selected, |d| d.hover(|s| s.bg(theme::wash())))
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
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(theme::S_12)
            .child(name_cell(entry, rename_input, self.show_extensions, true))
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
            .gap(layout::COL_GAP)
            .map(|d| match self.cols {
                Some(c) => d.w(c.name).flex_none(),
                None => d.flex_1().min_w_0(),
            })
    }

    pub(super) fn render_rows(
        &mut self,
        range: Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let started = Instant::now();
        let Some(entries) = self.view() else {
            return Vec::new();
        };
        let shell = self.services.shell.clone();
        let cursor = self.cursor;
        let in_cloud = self.current_path().is_some_and(|p| self.in_cloud(p));
        let sync = self.cloud.snapshot();
        let shares = lists_shares(self.current_path());
        // Skip the cursor ring when the only selected item is the cursor, to avoid doubling the
        // selection background.
        let single = self.selection.len() <= 1;
        let visible: Vec<Arc<str>> = range
            .clone()
            .filter_map(|ix| entries.get(ix).map(|e| e.name.clone()))
            .collect();
        let cut = self.cut_flags(&visible, cx);
        let images = self.media_for(&entries, range.clone(), false, cx);
        let renaming = self
            .renaming
            .as_ref()
            .map(|r| (r.name.clone(), (r.input.clone(), r.problem)));
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
                    .a11y_item(ix, is_selected, cursor == Some(ix), || {
                        self.item_a11y_label(entry)
                    })
                    .map(|row| self.tree_a11y(ix, entry, row))
                    .group(Self::check_group(ix))
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
                                listed_visual(shares, entry, image, theme::ICON, theme::ICON),
                                cx,
                            ))
                            .child(self.name_column(
                                ix,
                                entry,
                                rename_input,
                                (in_cloud, sync.get(&entry.name).copied()),
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
    /// one page ahead to the media cache.
    fn media_for(
        &mut self,
        entries: &[Entry],
        range: Range<usize>,
        thumbnails: bool,
        cx: &mut Context<Self>,
    ) -> Vec<Option<Arc<RenderImage>>> {
        let Some(dir) = self.current_path().map(Path::to_path_buf) else {
            return Vec::new();
        };
        let allowed = self.media_ok(&dir, cx);
        if !allowed {
            return Vec::new();
        }
        let thumb_px = (thumbnails && self.show_thumbnails).then(|| self.thumbnail_px());
        let key = |ix: usize| {
            entries
                .get(ix)
                .and_then(|e| MediaKey::sized(&dir, e, thumb_px))
        };
        let keys: Vec<Option<MediaKey>> = range.clone().map(key).collect();
        let prefetch_end = (range.end + range.len()).min(entries.len());
        let wanted: Vec<MediaKey> = keys
            .iter()
            .flatten()
            .cloned()
            .chain((range.end..prefetch_end).filter_map(key))
            .collect();
        self.media.update(cx, |m, _| m.want(wanted));
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
        let names: Vec<Arc<str>> = entries[start..end].iter().map(|e| e.name.clone()).collect();
        let cut = self.cut_flags(&names, cx);
        let images = self.media_for(&entries, start..end, true, cx);
        let cursor = self.cursor;
        let shares = lists_shares(self.current_path());
        let single = self.selection.len() <= 1;
        let renaming = self
            .renaming
            .as_ref()
            .map(|r| (r.name.clone(), (r.input.clone(), r.problem)));
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
                        let label = match rename_input {
                            Some(rename) => name_cell(entry, Some(rename), true, false),
                            None => div()
                            .w_full()
                            .text_center()
                            .line_clamp(2)
                            // Two lines can't fit (long name with no break points): ellipsize the
                            // second line rather than clipping the sides.
                            .text_ellipsis()
                            .text_size(theme::FS_12)
                            .child(shown_name(entry, self.show_extensions))
                            .into_any_element(),
                        };
                        // Folder compare badge in the tile's top-right
                        let badge = super::compare::compare_badge(self.compare_mark(&entry.name))
                            .map(|badge| {
                                div()
                                    .absolute()
                                    .top(theme::S_4)
                                    .right(theme::S_4)
                                    .child(badge)
                            });
                        // Checkbox in the tile's top-left
                        let check = self.show_checkboxes.then(|| {
                            div()
                                .absolute()
                                .top(theme::S_6)
                                .left(theme::S_6)
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
                            .pt(theme::S_4)
                            .px(theme::S_4)
                            .gap(theme::S_4)
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
                    .px(theme::S_8)
                    .children(tiles)
                    .into_any_element()
            })
            .collect()
    }

    fn grid_list(&mut self, len: usize, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        // Pane width (preview pane already excluded) minus paper margin, list padding and
        // scrollbar.
        let avail =
            self.pane_width - theme::S_8 - theme::S_32 - SCROLLBAR_GUTTER - layout::LIST_GUTTER;
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
        .pt(theme::S_4)
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
                move |this, range: Range<usize>, _window, cx| match this.groups() {
                    Some(groups) => this.render_grouped_rows(&groups, range, focused, cx),
                    None => this.render_rows(range, focused, cx),
                },
            ),
        )
        .track_scroll(&self.scroll)
        .pl(theme::S_8)
        .pr(theme::S_8 + layout::LIST_GUTTER)
        .flex_1();
        self.with_scrollbars(list, &self.scroll)
    }

    /// [`Self::with_scrollbar`] for Details rows and search results: columns wider than the pane
    /// scroll sideways (the header along), like Explorer, with a scrollbar along the bottom. The
    /// list's right padding keeps [`layout::LIST_GUTTER`] blank after the last column even then.
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
                self.hscrollbar_drag.clone(),
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
                self.scrollbar_drag.clone(),
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
            .size(theme::S_32)
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
            .gap(theme::S_8)
            .child(div().opacity(0.7).child(icon))
            .child(
                div()
                    .text_size(theme::FS_15)
                    .text_color(theme::text_muted())
                    .child(title),
            )
            .children(hint.map(|h| {
                div()
                    .text_size(theme::FS_12)
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
            .gap(theme::S_8)
            .px(theme::S_32)
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme::S_24)
                    .text_color(theme::danger()),
            )
            .child(
                div()
                    .text_size(theme::FS_15)
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
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .child(failed.path.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .gap(theme::S_8)
                    .mt(theme::S_8)
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
        let shell = self.services.shell.clone();
        let cursor = self.search_cursor;
        // "Search in this folder": locations start at that folder, below the folder shown.
        let base = self.search_in_name().map(PathBuf::from);
        range
            .filter_map(|ix| results.hits.get(ix).map(|hit| (ix, hit)))
            .map(|(ix, hit)| {
                let entry = &hit.entry;
                let parent = Path::new(&*hit.rel)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty());
                let location = match (&base, parent) {
                    (Some(base), Some(parent)) => base.join(parent).display().to_string(),
                    (Some(base), None) => base.display().to_string(),
                    (None, Some(parent)) => parent.display().to_string(),
                    (None, None) => t!("list.here").into_owned(),
                };
                self.row_shell(ix, cursor == Some(ix), false, focused, cx)
                    .a11y_item(ix, cursor == Some(ix), cursor == Some(ix), || {
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
                            .child(entry_visual(entry, None, theme::ICON, theme::ICON))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(theme::S_12)
                                    .child(
                                        div()
                                            .when(hit.snippet.is_some(), |d| {
                                                d.flex_none().max_w(px(240.))
                                            })
                                            .min_w_0()
                                            .truncate()
                                            .child(SharedString::from(entry.name.clone())),
                                    )
                                    .children(hit.snippet.clone().map(|line| {
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(theme::FS_12)
                                            .text_color(theme::text_muted())
                                            .child(SharedString::from(line))
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w(theme::COL_LOCATION)
                            .flex_none()
                            // Keep the end of long paths (the folders nearest the file).
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_muted())
                            .child(location),
                    )
                    .child(
                        div()
                            .w(crate::prefs::modified_width())
                            .flex_none()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_muted())
                            .child(
                                entry
                                    .modified
                                    .map_or_default(|t| shell.format_time(t)),
                            ),
                    )
                    .child(size_cell(entry, super::sizes::FolderSize::Unknown))
                    .into_any_element()
            })
            .collect()
    }

    /// List content: rows, tiles, search results, or the empty/loading state.
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
                Some(r) => {
                    let list = uniform_list(
                        "search-results",
                        r.hits.len(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            this.render_search_rows(range, focused, cx)
                        }),
                    )
                    .track_scroll(&self.search_scroll)
                    .pl(theme::S_8)
                    .pr(theme::S_8 + layout::LIST_GUTTER)
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
                        crate::assets::tinted(Icon::NetDrive, px(40.), theme::text_muted()),
                        t!("list.no_shares").into_owned(),
                        None,
                    )
                }
                Some(model) if view_len == Some(0) && model.complete => Self::empty_state(
                    kind_icon(FileKind::Folder, px(40.)),
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
                                    .pt(theme::S_4)
                                    .child(list),
                            )
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
            .pb(theme::S_8)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    window.focus(&this.list_focus, cx);
                    // Double click on empty space goes up (setting, off by default); presses on
                    // items are handled by the row.
                    let up = e.click_count == 2
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
            .on_action(cx.listener(Self::open_location))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
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

    /// Running file operation: current item, progress bar, cancel.
    fn op_toast(
        progress: &Arc<crate::shell::OpProgress>,
        verb: &'static str,
        cx: &mut Context<Self>,
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
            let cancel = progress.clone();
            return toast(
                "toast-ops",
                Icon::Copy,
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
        #[allow(clippy::cast_precision_loss)]
        let bar = if total > 0 && done > 0 {
            Progress::Fraction(so_far.min(total) as f32 / total as f32)
        } else {
            Progress::Indeterminate
        };
        let cancel = progress.clone();
        toast(
            "toast-ops",
            Icon::Copy,
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
        if let Some(job) = &self.extract_job {
            let p = job.progress;
            #[allow(clippy::cast_precision_loss)]
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
        if let Some((progress, verb)) = self.op_progress.as_ref().filter(|_| self.ops_running > 0) {
            toasts.push(Self::op_toast(progress, verb, cx));
        } else if self.ops_running > 0 {
            toasts.push(toast(
                "toast-ops",
                Icon::Copy,
                t!("toast.ops_running").into_owned(),
                Tone::Normal,
                Progress::Indeterminate,
                Vec::new(),
            ));
        } else if let Some(notice) = &self.notice {
            toasts.push(self.notice_toast(notice, cx));
        }
        if let Some(error) = &self.error {
            let action: Option<(_, Listener)> = self.error_action.as_ref().map(|a| {
                let label = match a {
                    ErrorAction::Retry(_) | ErrorAction::RetryOp(_) => t!("toast.retry"),
                    ErrorAction::Login(_) => t!("toast.login"),
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
                .right(theme::S_16)
                .bottom(theme::S_16)
                .w(px(360.))
                .flex()
                .flex_col()
                .gap(theme::S_8)
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
            .is_some_and(|(text, id)| text == notice && super::UndoHistory::has(cx, *id));
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

    /// The toasts up now, or else a notice that is fading out.
    fn render_toast_stack(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.render_toasts(cx)
            .or_else(|| self.render_leaving_notice())
    }

    /// The faded-out notice, where it was. Drawn only when no other toast is up, so it never
    /// overlaps one. Its buttons are drawn (inert) so the text wraps as it did.
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
                .right(theme::S_16)
                .bottom(theme::S_16)
                .w(px(360.))
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
        .gap(theme::S_8)
        .px(theme::S_16)
        .py(theme::S_12)
        .rounded(theme::R_12)
        .bg(if danger {
            theme::danger_surface()
        } else {
            theme::overlay()
        })
        .shadow(theme::elev_float())
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme::S_8)
                .child(glyph.size(theme::ICON).flex_none())
                .child(div().flex_1().min_w_0().line_clamp(4).child(text))
                .children(
                    actions
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (label, on_click))| {
                            div()
                                .id(("toast-action", ix))
                                .a11y_button(label.to_string())
                                .flex_none()
                                .px(theme::S_6)
                                .rounded(theme::R_4)
                                .cursor_pointer()
                                .text_size(theme::FS_12)
                                .text_color(theme::text_muted())
                                .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                                .child(label.to_string())
                                .on_click(on_click)
                        }),
                ),
        )
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
                    .h(theme::S_4)
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

pub(crate) fn tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text: SharedString = text.into();
    move |_, cx| cx.new(|_| crate::workspace::Tooltip(text.clone())).into()
}

fn kbd(text: &'static str) -> impl IntoElement {
    div()
        .px(theme::S_4)
        .rounded(theme::R_4)
        .border_1()
        .border_color(theme::line_strong())
        .text_size(theme::FS_11)
        .text_color(theme::text_subtle())
        .child(text)
}

fn vertical_divider() -> impl IntoElement {
    div()
        .w(px(1.))
        .h(theme::S_16)
        .mx(theme::S_4)
        .flex_none()
        .bg(theme::line())
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
        .a11y_button(tip.clone())
        .a11y_disabled(!enabled)
        .size(theme::H_CTL)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::R_8)
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(theme::wash_strong()))
                .on_click(on_click)
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme::ICON)
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
        .a11y_button(label.clone())
        .group(id)
        .h(theme::H_CTL_SM)
        .px(theme::S_8)
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::S_6)
        .rounded(theme::R_8)
        .cursor_pointer()
        .text_size(theme::FS_12)
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
                .size(theme::ICON)
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
        .when(compact, |d| d.px(theme::S_6).tooltip_below(label.clone()))
        .when(!compact, |d| d.child(label.clone()))
        .on_click(on_click)
}

/// "Clear selection Esc"; just × when narrow.
fn clear_selection_button(
    compact: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id("sel-clear")
        .a11y_button(t!("sel.clear"))
        .h(theme::H_CTL_SM)
        .px(theme::S_8)
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::S_6)
        .rounded(theme::R_8)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
        .child(
            svg()
                .path(Icon::Close.path())
                .size(theme::ICON_SM)
                .text_color(theme::text_subtle()),
        )
        .when(!compact, |d| {
            d.child(t!("sel.clear").to_string()).child(kbd("Esc"))
        })
        .tooltip_below(t!("sel.clear").to_string() + " (Esc)")
        .on_click(on_click)
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
        .h(theme::H_CTL_SM)
        .px(theme::S_12)
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::FS_12)
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
        .h(theme::H_CTL_SM)
        .px(theme::S_6)
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(on, |d| d.bg(theme::wash_strong()))
        .when(!on, |d| d.hover(|s| s.bg(theme::wash())))
        .child(
            svg()
                .path(Icon::Preview.path())
                .size(theme::ICON)
                .text_color(if on {
                    theme::text()
                } else {
                    theme::text_subtle()
                }),
        )
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.preview_pane = !on, cx));
        }))
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
        .h(theme::H_CTL_SM)
        .px(theme::S_6)
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(split, |d| d.bg(theme::wash_strong()))
        .when(!split, |d| d.hover(|s| s.bg(theme::wash())))
        .on_click(on_click)
        .child(
            svg()
                .path(Icon::Split.path())
                .size(theme::ICON)
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
        .h(theme::H_CTL_SM)
        .px(theme::S_8)
        .flex()
        // Truncate the folder name when space is short so the button isn't squeezed out.
        .when(show_label, |d| d.flex_shrink(1.).min_w(theme::S_32))
        .when(!show_label, gpui::Styled::flex_none)
        .items_center()
        .gap(theme::S_6)
        .rounded(theme::R_8)
        .border_1()
        .border_color(theme::signal_line())
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text())
        .hover(|s| s.bg(theme::signal_wash()))
        .child(
            svg()
                .path(icon.path())
                .size(theme::ICON)
                .flex_none()
                .text_color(theme::signal()),
        )
        .when(show_label, |d| {
            d.child(div().min_w_0().truncate().child(label.to_string()))
        })
        .a11y_lazy(gpui::Role::Button, || label.to_string())
        .aria_keyshortcuts(key)
        .tooltip_below(format!("{label} ({key})"))
        .on_click(on_click)
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
        .w(px(28.))
        .h(super::viewopts::SEGMENT_H)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .cursor_pointer()
        .when(active, |d| {
            d.bg(theme::segment_thumb()).shadow(theme::elev_thumb())
        })
        .when(!active, |d| d.hover(|s| s.bg(theme::wash())))
        .on_click(on_click)
        .child(
            svg()
                .path(icon.path())
                .size(theme::ICON)
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
/// snap layouts menu.
pub(crate) fn caption_buttons(window: &Window) -> impl IntoElement {
    let maximized = window.is_maximized();
    let button = |id: &'static str, icon: Icon, area: WindowControlArea, close: bool| {
        div()
            .id(id)
            .a11y_button(match area {
                WindowControlArea::Min => t!("window.minimize"),
                WindowControlArea::Max if maximized => t!("window.restore"),
                WindowControlArea::Max => t!("window.maximize"),
                _ => t!("window.close"),
            })
            .group(id)
            .size(theme::H_CTL)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_8)
            .window_control_area(area)
            .when(!close, |d| d.hover(|s| s.bg(theme::wash_strong())))
            .when(close, |d| d.hover(|s| s.bg(theme::danger())))
            .child(
                svg()
                    .path(icon.path())
                    .size(theme::ICON)
                    .text_color(theme::text_muted())
                    .when(close, |d| {
                        d.group_hover(id, |s| s.text_color(theme::on_signal()))
                    }),
            )
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::S_2)
        .pl(theme::S_8)
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

/// Space between the rename box's edge and its text: its padding plus the 1 px border.
const RENAME_INSET: Pixels = px(4. + 1.);

/// Name text, or the inline rename box with its problem message (if any) underneath.
///
/// `in_row`: Details rows pull the box left by its inset, so the text stays exactly where the
/// name was drawn instead of jumping right when renaming starts (grid tiles center the name, so
/// they don't need it). In a tile the box spans the tile's width: tiles center their children,
/// so without a width it would shrink to its border and hide the name (#38).
fn name_cell(
    entry: &Entry,
    rename: Option<(Entity<TextInput>, Option<NameError>)>,
    show_ext: bool,
    in_row: bool,
) -> AnyElement {
    match rename {
        Some((input, problem)) => div()
            .debug_selector(|| "rename-box".to_owned())
            // Presses stay in the box: the row under it would take focus back to the list, which
            // commits the rename, and a double-click would open the file (#69).
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .relative()
            .map(|d| {
                if in_row {
                    d.flex_1().ml(-RENAME_INSET)
                } else {
                    d.w_full().flex_none()
                }
            })
            .min_w_0()
            .h(theme::S_24)
            .px(RENAME_INSET - px(1.))
            .flex()
            .items_center()
            .rounded(theme::R_4)
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
            .truncate()
            .child(shown_name(entry, show_ext))
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
                .offset(gpui::point(px(0.), theme::S_4))
                .snap_to_window_with_margin(theme::S_8)
                .child(
                    div()
                        .id("rename-problem")
                        .a11y_live(gpui::Role::Alert, problem.to_string())
                        .occlude()
                        .max_w(RENAME_BALLOON_W)
                        .px(theme::S_8)
                        .py(theme::S_6)
                        .rounded(theme::R_8)
                        .bg(theme::overlay())
                        .shadow(theme::elev_float())
                        .border_1()
                        .border_color(theme::danger())
                        .text_size(theme::FS_12)
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
            .rounded(theme::R_4)
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
    let small = size <= SMALL_ICON;
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
const SMALL_ICON: Pixels = px(20.);
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
            self.render_path_overlays(cx),
            self.render_context_menu(window, cx),
            self.render_quick_look(window, cx),
            self.render_confirm(cx),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

impl Explorer {
    /// Before each render: measure widths, collect background results, start background work as
    /// needed (preview, parent, folder sizes).
    fn before_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_frame(window, cx);
        self.track_notice_exit(cx);
        self.screen = (window.viewport_size(), window.scale_factor());
        let window_w = window.viewport_size().width;
        // A split tab's shared preview takes its room first; the panes share the rest.
        self.paper_width = (window_w - theme::SIDEBAR_W - self.pane.reserve) * self.pane.share;
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
        self.cols = self.compute_columns();
        self.header_x.set(None);
        // Preview pane edge released: save the width (app-wide).
        if self.preview_resized && !cx.has_active_drag() {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = this.update(cx, Self::save_preview_width);
            });
        }
        self.poll_pending_create(cx);
        self.poll_pending_rename(window, cx);
        self.schedule_notice_dismiss(cx);
        self.sync_preview(cx);
        self.sync_parent_column();
        self.sync_link_targets();
        self.sync_folder_sizes(cx);
        self.sync_cloud_states(cx);
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::a11y::sync(window);
        self.before_render(window, cx);
        div()
            .id("explorer")
            .key_context("Explorer")
            .size_full()
            .flex()
            .flex_col()
            .text_color(theme::text())
            .ui_font()
            .text_size(theme::FS_14)
            .on_action(cx.listener(Self::go_back))
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
            // Split view: clicking a side makes it active.
            .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if this.pane.split && !this.pane.active {
                    cx.emit(ExplorerEvent::Activated);
                }
            }))
            .map(|d| Self::mouse_navigation(d, cx))
            .child(self.render_top_bar(window, cx))
            .child(
                // The paper: the single sheet holding the list
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .mr(self.paper_margin_right())
                    .mb(theme::S_8)
                    .rounded(theme::R_12)
                    .bg(theme::paper())
                    .shadow(if self.pane.split && self.pane.active {
                        theme::elev_2_active()
                    } else {
                        theme::elev_2()
                    })
                    .children(backdrop_layer())
                    .child(self.render_context_bar(window, cx))
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
                            .children(self.render_preview_appearing(cx)),
                    )
                    .children(self.render_toast_stack(cx)),
            )
            .children(self.render_floating(window, cx))
            .children(self.render_right_drag_listener(cx))
    }
}

impl Explorer {
    /// Tests: whether the preview pane fit at the last render, and its width.
    #[cfg(test)]
    pub(crate) fn fit_for_test(&self) -> (bool, f32) {
        (self.fit.preview, f32::from(self.fit.preview_w))
    }

    /// Space right of the paper: the window margin, and in a split tab's rightmost pane the
    /// shared preview too (the pane's box runs on under it).
    fn paper_margin_right(&self) -> Pixels {
        if self.pane.trailing {
            theme::S_8 + self.pane.reserve
        } else {
            theme::S_8
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
        layout::FitInput {
            bar,
            window,
            details: self.view_mode == ViewMode::Details && !self.search_mode,
            checkboxes: self.show_checkboxes,
            columns_view: self.columns_active(),
            search: self.search_mode,
            // A split tab's preview pane is drawn once for the tab, not by each side.
            wants_preview: (self.preview_pane && !self.pane.split) || self.columns_active(),
            preview_user: crate::prefs::preview_width(),
            parent_w: super::columns::PARENT_W,
            location_w: theme::COL_LOCATION,
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
        }
    }

    /// Preview pane slides in from the right when it is turned on (not when a tab showing it is
    /// drawn again). Turning it off is instant: the list takes the room back at once, and animating
    /// the width would relayout every row on every frame.
    fn render_preview_appearing(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pane = self.render_preview_pane(cx)?;
        let description = crate::a11y::active()
            .then(|| self.preview_a11y_description())
            .flatten();
        Some(
            self.preview_cue.wrap(
                div()
                .id("preview-pane")
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
                })
                .on_drag_move(cx.listener(Self::drag_preview_edge)),
                "preview-pane-appear",
                crate::motion::SLIDE_LG,
                0.,
            ),
        )
    }

    /// Split tab: the tab's one preview, drawn by the workspace at the tab's right end, showing
    /// this pane's item (the active side). `None` when this pane isn't the one it follows.
    pub(crate) fn render_shared_preview(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                    .text_size(theme::FS_12)
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
            self.preview_cue.wrap(
                div()
                .id("shared-preview")
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
                .rounded(theme::R_12)
                .bg(theme::paper())
                .shadow(theme::elev_2())
                .children(contents)
                .child(self.preview_resize_handle(true, cx))
                // Drops land in the folder of the side it shows.
                .when_some(self.current_path().map(Path::to_path_buf), |d, here| {
                    accept_drops_into(d, here, cx)
                })
                .on_drag_move(cx.listener(Self::drag_preview_edge)),
                "shared-preview-appear",
                crate::motion::SLIDE_LG,
                0.,
            ),
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
        if !self.view_cue.live_at(std::time::Instant::now()) {
            return list.into_any_element();
        }
        gpui::AnimationExt::with_animation(
            list,
            ("list-view-switch", self.view_cue.serial()),
            crate::motion::enter(),
            |el, t| el.opacity(FLOOR + (1. - FLOOR) * t),
        )
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
            .rounded(theme::R_12)
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
        .text_size(theme::FS_12)
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
            .child(svg().path(icon.path()).size(theme::ICON).text_color(color)),
    )
}

impl Explorer {
    /// Mouse back/forward over this pane go back in this pane; the window handles them everywhere
    /// else (sidebar, tab list, pages), so they stop here to move only once (#8).
    fn mouse_navigation(
        d: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        d.on_mouse_down(
            MouseButton::Navigate(NavigationDirection::Back),
            cx.listener(|this, _: &MouseDownEvent, w, cx| {
                cx.stop_propagation();
                this.go_back(&GoBack, w, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Navigate(NavigationDirection::Forward),
            cx.listener(|this, _: &MouseDownEvent, w, cx| {
                cx.stop_propagation();
                this.go_forward(&GoForward, w, cx);
            }),
        )
    }
}
