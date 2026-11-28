//! Column view (like Finder's columns): parent | current folder | preview.
//!
//! - Left column lists the parent folder with the current folder marked; clicking another folder
//!   goes there. At a drive root, which has no parent, it lists the computer's drives instead
//!   (#523): the drives of the sidebar's This PC, the current one marked.
//! - Middle column is the current folder (names only); Left goes up, Right enters a folder.
//! - Right column shows the preview without opening the preview pane.
//!
//! The parent has its own [`DirService`], like an expanded folder of the Details tree: listed off
//! the UI thread with the tab's sort and hidden-items setting, then watched, so items copied or
//! moved into it, or changed by other programs, show up (#524). It is listed again only when the
//! parent changes (moving among the folders it lists costs nothing), on F5 and when hidden items
//! are toggled, and its watch stops once the parent changes or the column view is left. A listing
//! that fails says why in the column and is tried again on the next navigation or F5.
//!
//! The parent and current columns can be sized like Finder's: drag the divider on a column's right
//! edge (the current column's is the preview's edge), double-click it to fit the column to its
//! longest name. The widths are app-wide, the same in every folder. Names cut off by a narrow
//! column show in full in a tooltip.

use std::ops::Range;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arke_core::filetype::FileKind;
use arke_core::{DeltaOutcome, DirModel, DirService, Entry, IoEvent, NavKind, service};
use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, ExternalPaths, MouseDownEvent, Pixels,
    ScrollStrategy, SharedString, UniformListScrollHandle, Window, div, prelude::*, px, svg,
    uniform_list,
};
use rust_i18n::t;

use super::view::listed_visual;
use super::{
    AppEvent, DraggedFiles, Explorer, ViewMode, accepts_drop, drop_external_into, drop_into,
    layout, report_drop_target,
};
use crate::assets::Icon;
use crate::media::MediaKey;
use crate::prefs::ViewColumn;
use crate::settings::Density;
use crate::shell::{Place, PlaceKind};
use crate::splitter::{DragEnd, Splitter};
use crate::theme;

/// Default width of the left (parent) column
pub(super) fn parent_w() -> gpui::Pixels {
    crate::theme::scaled_px(240.)
}
/// Fitting measures at most this many names, the longest by character count: enough to find the
/// widest one without shaping every name of a huge folder.
const FIT_CANDIDATES: usize = 256;

/// Tells a parent listing's events apart from an earlier one's.
static NEXT_LISTING: AtomicU64 = AtomicU64::new(0);

/// What the parent column lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParentLevel {
    /// The current folder's parent (for a share root, its server's shares).
    Folder(PathBuf),
    /// The computer's drives: the current folder is a drive root (#523).
    Drives,
}

pub(crate) struct ParentColumn {
    pub(crate) level: ParentLevel,
    /// The parent folder's live listing (`None` for the drives, which are the sidebar's places).
    listing: Option<ParentListing>,
    pub(crate) scroll: UniformListScrollHandle,
}

/// The parent folder, listed and watched (#524). Dropping it stops the watch.
struct ParentListing {
    id: u64,
    service: DirService,
    model: DirModel,
    /// The current folder has been scrolled into view.
    placed: bool,
    failed: Option<ParentFailure>,
}

/// Why the parent folder couldn't be listed.
struct ParentFailure {
    reason: String,
    /// The current folder then: the parent is listed again once it changes.
    from: Option<PathBuf>,
}

/// `C:\`: a drive's root, which has no parent folder.
pub(super) fn is_drive_root(path: &Path) -> bool {
    let mut parts = path.components();
    let disk = match parts.next() {
        Some(Component::Prefix(p)) => matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)),
        _ => false,
    };
    disk && parts.next() == Some(Component::RootDir) && parts.next().is_none()
}

/// What the parent column lists for the folder `here`: its parent (a share root's is its
/// server, listed as its shares), or the drives for a drive root, which has none.
fn parent_level(here: &Path) -> Option<ParentLevel> {
    match arke_core::path::parent(here) {
        Some(parent) => Some(ParentLevel::Folder(parent)),
        None => is_drive_root(here).then_some(ParentLevel::Drives),
    }
}

impl Explorer {
    pub(super) fn columns_active(&self) -> bool {
        self.view_mode == ViewMode::Columns && !self.search_mode
    }

    /// Called on every render: in column view, list the parent after a folder change. Nothing to
    /// do while it stays the parent.
    pub(super) fn sync_parent_column(&mut self) {
        if self.view_mode != ViewMode::Columns {
            // Left the column view: the parent's watch goes with it.
            self.parent_column = None;
            return;
        }
        if self.search_mode {
            return;
        }
        let Some(level) = self.current_path().and_then(parent_level) else {
            self.parent_column = None;
            return;
        };
        if self
            .parent_column
            .as_ref()
            .is_some_and(|c| c.level == level)
        {
            // Couldn't be listed: try again once the user went to another folder.
            let moved = self
                .parent_column
                .as_ref()
                .and_then(|c| c.listing.as_ref())
                .and_then(|l| l.failed.as_ref())
                .is_some_and(|f| f.from.as_deref() != self.current_path());
            if moved {
                self.parent_relist();
            }
            return;
        }
        let scroll = UniformListScrollHandle::new();
        let listing = match &level {
            ParentLevel::Folder(dir) => Some(self.list_parent(dir.clone())),
            ParentLevel::Drives => {
                let here = self.current_path();
                let current = self
                    .column_drives()
                    .position(|d| here.is_some_and(|h| super::same_folder(&d.path, h)));
                if let Some(ix) = current {
                    scroll.scroll_to_item(ix, ScrollStrategy::Center);
                }
                None
            }
        };
        // Replacing the column drops the previous parent's service, which stops its watch.
        self.parent_column = Some(ParentColumn {
            level,
            listing,
            scroll,
        });
    }

    /// Starts listing and watching `dir` for the parent column.
    fn list_parent(&self, dir: PathBuf) -> ParentListing {
        let id = NEXT_LISTING.fetch_add(1, Ordering::Relaxed);
        let tx = self.events_tx.clone();
        let mut service = DirService::new(
            Arc::clone(&self.services.fs),
            Arc::new(move |event| {
                let _ = tx.send(AppEvent::ParentIo { column: id, event });
            }),
        );
        if let Some(probe) = self.services.shell.net_probe() {
            service = service.with_probe(probe);
        }
        // Sorted like the middle column (folders first, case-insensitive natural order, current
        // sort column).
        let generation = service.navigate(dir.clone(), self.tree_list_options());
        ParentListing {
            id,
            service,
            model: DirModel::new(dir, generation, self.sort),
            placed: false,
            failed: None,
        }
    }

    /// The parent folder's listing, when the column lists one.
    fn parent_listing_mut(&mut self) -> Option<&mut ParentListing> {
        self.parent_column.as_mut().and_then(|c| c.listing.as_mut())
    }

    /// F5, hidden items toggled, or a failed listing retried: list the parent folder again (what
    /// it shows stays until the new listing arrives).
    pub(super) fn parent_relist(&mut self) {
        let opts = self.tree_list_options();
        if let Some(listing) = self.parent_listing_mut() {
            listing.failed = None;
            listing.model.generation = listing.service.navigate(listing.model.path.clone(), opts);
        }
    }

    /// The sort changed: the parent column follows.
    pub(super) fn parent_resort(&mut self) {
        let spec = self.sort;
        let opts = self.tree_list_options();
        let Some(listing) = self.parent_listing_mut() else {
            return;
        };
        let model = &mut listing.model;
        if model.sorted_by == spec {
            return;
        }
        if model.entries.len() <= super::tree::INLINE_RESORT_MAX {
            let sorted = service::resort(&model.entries, spec);
            model.replace_sorted(Arc::new(sorted), spec);
        } else {
            model.generation = listing.service.navigate(model.path.clone(), opts);
        }
    }

    /// A listing, change or failure for the parent column's folder.
    pub(super) fn handle_parent_io(&mut self, id: u64, event: IoEvent, cx: &mut Context<Self>) {
        let opts = self.tree_list_options();
        let here = self.current_path().map(Path::to_path_buf);
        let here_name = here.as_deref().and_then(arke_core::path::leaf_name);
        let Some(column) = self.parent_column.as_mut() else {
            return;
        };
        let Some(listing) = column.listing.as_mut().filter(|l| l.id == id) else {
            return;
        };
        let current = listing.service.current_gen();
        let relist = match event {
            IoEvent::Listing {
                generation,
                entries,
                sorted_by,
                complete,
                ..
            } if generation == current => {
                listing.failed = None;
                let outcome = listing.model.set_snapshot(entries, sorted_by, complete);
                if !listing.placed
                    && let Some(ix) = here_name.and_then(|name| listing.model.index_of(&name))
                {
                    column.scroll.scroll_to_item(ix, ScrollStrategy::Center);
                    listing.placed = true;
                }
                outcome == DeltaOutcome::NeedsRescan
            }
            IoEvent::Delta { generation, delta } if generation == current => {
                listing.model.apply_delta(delta) == DeltaOutcome::NeedsRescan
            }
            IoEvent::NeedsRescan { generation } if generation == current => true,
            IoEvent::ListFailed {
                generation,
                path,
                error,
                ..
            } if generation == current => {
                log::warn!("parent column: listing {} failed: {error}", path.display());
                listing.failed = Some(ParentFailure {
                    reason: error,
                    from: here,
                });
                false
            }
            // Stale: an earlier listing of this folder.
            _ => return,
        };
        if relist {
            listing.model.generation = listing.service.navigate(listing.model.path.clone(), opts);
        }
        // A listing requested before the sort last changed arrives in the old order.
        self.parent_resort();
        cx.notify();
    }

    /// The drives the column lists at a drive root: the ones under This PC, mapped network
    /// drives included (each is a drive root too).
    fn column_drives(&self) -> impl Iterator<Item = &Place> {
        self.places
            .iter()
            .filter(|p| matches!(p.kind, PlaceKind::Drive { .. }))
    }

    /// Height of a parent column row.
    fn parent_row_h(&self) -> Pixels {
        if self.density == Density::Compact {
            theme::h_row_compact()
        } else {
            theme::h_row_tight()
        }
    }

    /// Asks for the side column's icons (queued after the middle column's) and returns the ones
    /// already loaded.
    fn side_images(
        &mut self,
        keys: &[Option<MediaKey>],
        cx: &mut Context<Self>,
    ) -> Vec<Option<Arc<gpui::RenderImage>>> {
        let view = cx.entity_id();
        let wanted: Vec<MediaKey> = keys.iter().flatten().cloned().collect();
        self.media.update(cx, |m, cx| m.want_side(view, wanted, cx));
        let media = self.media.read(cx);
        keys.iter()
            .map(|k| k.as_ref().and_then(|k| media.get(k)))
            .collect()
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
        let Some(listing) = &column.listing else {
            return self.render_drive_rows(range, window, cx);
        };
        let entries = Arc::clone(&listing.model.entries);
        let dir = listing.model.path.clone();
        let here = self.current_path().map(Path::to_path_buf);
        let shares = super::view::lists_shares(Some(&dir));
        // System icons for .exe/.lnk/.ico like the middle column, loaded after it so they don't
        // compete.
        let images: Vec<Option<Arc<gpui::RenderImage>>> = if self.media_ok(&dir, cx) {
            let icon_px = crate::media::icon_source_px(f32::from(theme::icon()), self.ui_scale);
            let keys: Vec<Option<MediaKey>> = range
                .clone()
                .map(|ix| {
                    entries
                        .get(ix)
                        .and_then(|e| MediaKey::for_entry(&dir, e, false))
                        .map(|k| k.with_icon_px(icon_px))
                })
                .collect();
            self.side_images(&keys, cx)
        } else {
            Vec::new()
        };
        let show_ext = self.show_extensions;
        let width = self.parent_column_drawn();
        let parent_row_h = self.parent_row_h();
        let me = cx.entity();
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
                let drop_to = is_dir.then(|| path.clone());
                div()
                    .id(("parent-col", ix))
                    .debug_selector(move || format!("parent-row-{ix}"))
                    .when(cut, |d| {
                        d.tooltip(super::view::tooltip(SharedString::from(&*entry.name)))
                    })
                    .relative()
                    .h(parent_row_h)
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
                    // A folder here takes drops like a folder row in the list.
                    .when_some(drop_to, |d, dest| accept_drops(d, dest, true, me.clone()))
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

    /// Rows of the drives (#523): like folder rows, with the sidebar's drive icons and names. A
    /// drop on a drive goes into it, as on the sidebar's drive.
    fn render_drive_rows(
        &self,
        range: Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let drives: Vec<(PathBuf, SharedString, Icon, bool)> = {
            let here = self.current_path();
            self.column_drives()
                .enumerate()
                .filter(|(ix, _)| range.contains(ix))
                .map(|(_, place)| {
                    let icon = if matches!(place.kind, PlaceKind::Drive { remote: true, .. }) {
                        Icon::NetDrive
                    } else {
                        Icon::Drive
                    };
                    let current = here.is_some_and(|h| super::same_folder(&place.path, h));
                    let label = crate::place_names::place_label(&self.place_names, place);
                    (place.path.clone(), label.into(), icon, current)
                })
                .collect()
        };
        let width = self.parent_column_drawn();
        let row_h = self.parent_row_h();
        let me = cx.entity();
        let start = range.start;
        drives
            .into_iter()
            .enumerate()
            .map(|(i, (root, label, icon, current))| {
                let ix = start + i;
                let cut = name_width(window, &label) > width - parent_row_chrome(true);
                let open = root.clone();
                let tip = label.clone();
                let row = div()
                    .id(("parent-drive", ix))
                    .debug_selector(move || format!("parent-drive-{ix}"))
                    .when(cut, |d| d.tooltip(super::view::tooltip(tip)))
                    .relative()
                    .h(row_h)
                    .px(theme::s_8())
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .rounded(theme::r_8())
                    .cursor_pointer()
                    // The drive shown on the right: neutral selection, like the folder rows.
                    .when(current, |d| d.bg(theme::selection()))
                    .when(!current, |d| d.hover(|s| s.bg(theme::wash())))
                    .text_color(theme::text())
                    .child(crate::assets::tinted(icon, theme::icon(), theme::text_muted()))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(chevron());
                accept_drops(row, root, true, me.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.navigate(open.clone(), NavKind::New, cx);
                    }))
                    .into_any_element()
            })
            .collect()
    }

    /// Left column of the column view.
    pub(super) fn render_parent_column(&self, cx: &mut Context<Self>) -> AnyElement {
        let column = div()
            .debug_selector(|| "parent-column".to_owned())
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
        let (column, len) = match &parent.listing {
            Some(listing) => {
                let dir = &listing.model.path;
                // The empty space takes drops for the folder the column shows, like the list's
                // own.
                let column = accept_drops(column, dir.clone(), false, cx.entity());
                if let Some(failed) = &listing.failed {
                    return parent_failed(column, dir, &failed.reason);
                }
                let model = &listing.model;
                if model.entries.is_empty() {
                    return if model.complete {
                        parent_empty(column, dir)
                    } else {
                        column
                            .items_center()
                            .justify_center()
                            .text_size(theme::fs_12())
                            .text_color(theme::text_subtle())
                            .child(t!("info.loading").to_string())
                            .into_any_element()
                    };
                }
                (column, model.entries.len())
            }
            None => (column, self.column_drives().count()),
        };
        column
            .child(
                uniform_list(
                    "parent-column",
                    len,
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

    /// The names the parent column shows, for fitting its width.
    fn parent_shown_names(&self) -> Vec<SharedString> {
        let Some(column) = &self.parent_column else {
            return Vec::new();
        };
        match &column.listing {
            Some(listing) => listing
                .model
                .entries
                .iter()
                .map(|e| super::view::shown_name(e, self.show_extensions))
                .collect(),
            None => self
                .column_drives()
                .map(|p| crate::place_names::place_label(&self.place_names, p).into())
                .collect(),
        }
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
        let show_ext = self.show_extensions;
        let mut names: Vec<SharedString> = match col {
            ViewColumn::Parent => self.parent_shown_names(),
            ViewColumn::Current => self.view().map_or_else(Vec::new, |entries| {
                entries
                    .iter()
                    .map(|e| super::view::shown_name(e, show_ext))
                    .collect()
            }),
        };
        if names.is_empty() {
            return;
        }
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

    /// Room a current column row takes besides its name: the list's left inset, right padding and
    /// gutter, the row's padding, the checkbox and icon, and the folder arrow (`chevron`).
    fn current_row_chrome(&self, chevron: bool) -> Pixels {
        let mut w = theme::list_inset()
            + theme::s_8()
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
        let hit = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(self.parent_column_drawn() - theme::s_4())
            .w(theme::s_8())
            // Presses stay here: the list under it would clear the selection.
            .occlude();
        let line = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(theme::s_4() - px(1.))
            .w(px(2.));
        let dragging = matches!(self.view_column_drag, Some((ViewColumn::Parent, ..)));
        Splitter::new("parent-column-resize", gpui::CursorStyle::ResizeColumn)
            .dragging(dragging)
            // Like the preview's edge: the drag moves the divider by as much as the cursor moved
            // since the press, so it neither jumps nor lags.
            .on_start(cx.listener(|this, e: &MouseDownEvent, _, cx| {
                this.activate_on_press(cx);
                this.view_column_drag =
                    Some((ViewColumn::Parent, e.position.x, this.parent_column_drawn()));
            }))
            .on_double_click(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.activate_on_press(cx);
                this.fit_view_column(ViewColumn::Parent, window, cx);
            }))
            .on_drag(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if let Some((ViewColumn::Parent, from, start)) = this.view_column_drag {
                    this.drag_view_column(ViewColumn::Parent, start + e.position.x - from, cx);
                }
            }))
            .on_end(cx.listener(|this, _: &DragEnd, _, cx| {
                if matches!(this.view_column_drag, Some((ViewColumn::Parent, ..))) {
                    this.view_column_drag = None;
                    this.save_view_column_widths(cx);
                    cx.notify();
                }
            }))
            .render(hit, line)
    }
}

/// Makes `d` take drops into `dest`, with the list's own functions: the same move or copy, drop
/// menu, undo and card label as a folder row or the list's background. `highlight`: tint while a
/// drag that `dest` accepts is over it (rows do; the empty space doesn't, like the list).
pub(super) fn accept_drops<E: InteractiveElement + IntoElement>(
    d: E,
    dest: PathBuf,
    highlight: bool,
    target: Entity<Explorer>,
) -> E {
    let (check, hovered, external) = (dest.clone(), dest.clone(), dest.clone());
    d.can_drop(move |value, _, cx| accepts_drop(value, &check, cx))
        .when(highlight, |d| {
            d.drag_over::<DraggedFiles>(|s, _, _, _| s.bg(theme::signal_wash()))
                .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(theme::signal_wash()))
        })
        .on_drag_move(move |e: &gpui::DragMoveEvent<DraggedFiles>, _, cx| {
            report_drop_target(&hovered, e, cx);
        })
        .on_drop(move |dragged: &DraggedFiles, window, cx| {
            drop_into(dragged, dest.clone(), window, cx);
        })
        .on_drop(move |paths: &ExternalPaths, window, cx| {
            drop_external_into(paths, external.clone(), &target, window, cx);
        })
}

/// The parent column when its folder couldn't be listed: why, like the list's own error page.
fn parent_failed(column: Div, dir: &Path, reason: &str) -> AnyElement {
    let name = arke_core::path::leaf_name(dir).unwrap_or_else(|| dir.display().to_string());
    let icon = svg()
        .path(Icon::Close.path())
        .size(theme::s_24())
        .text_color(theme::danger())
        .into_any_element();
    column
        .child(
            div()
                .debug_selector(|| "parent-column-error".to_owned())
                .flex_1()
                .flex()
                .flex_col()
                .px(theme::s_12())
                .text_center()
                .child(Explorer::empty_state(
                    icon,
                    t!("list.failed_title", name = name).into_owned(),
                    Some(reason.to_owned()),
                )),
        )
        .into_any_element()
}

/// The parent column when its folder lists nothing (all hidden, or a server without shares),
/// like the list's empty state.
fn parent_empty(column: Div, dir: &Path) -> AnyElement {
    let state = if super::view::lists_shares(Some(dir)) {
        Explorer::empty_state(
            crate::assets::tinted(Icon::NetDrive, theme::scaled_px(40.), theme::text_muted()),
            t!("list.no_shares").into_owned(),
            None,
        )
    } else {
        Explorer::empty_state(
            super::kind_icon(FileKind::Folder, theme::scaled_px(40.)),
            t!("list.empty").into_owned(),
            None,
        )
    };
    column
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px(theme::s_12())
                .child(state),
        )
        .into_any_element()
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
        let Some(listing) = self
            .parent_column
            .as_ref()
            .and_then(|c| c.listing.as_ref())
            .filter(|l| l.failed.is_none())
        else {
            return Vec::new();
        };
        listing
            .model
            .entries
            .iter()
            .map(|e| e.name.to_string())
            .collect()
    }

    /// The drives the parent column lists at a drive root, and which of them is current; `None`
    /// when it lists a folder.
    pub(crate) fn parent_column_drives(&self) -> Option<(Vec<PathBuf>, Option<usize>)> {
        let column = self.parent_column.as_ref()?;
        if column.listing.is_some() {
            return None;
        }
        let here = self.current_path();
        let drives: Vec<PathBuf> = self.column_drives().map(|d| d.path.clone()).collect();
        let current = drives
            .iter()
            .position(|d| here.is_some_and(|h| super::same_folder(d, h)));
        Some((drives, current))
    }

    /// Why the parent column's folder couldn't be listed.
    pub(crate) fn parent_column_error(&self) -> Option<String> {
        self.parent_column
            .as_ref()
            .and_then(|c| c.listing.as_ref())
            .and_then(|l| l.failed.as_ref())
            .map(|f| f.reason.clone())
    }

    /// The folder the parent column lists and watches (`None`: no service, so no watch).
    pub(crate) fn parent_column_lists(&self) -> Option<PathBuf> {
        self.parent_column
            .as_ref()
            .and_then(|c| c.listing.as_ref())
            .map(|l| l.model.path.clone())
    }

    /// Which of the current column's names are cut off (and get a tooltip).
    pub(crate) fn current_name_tips_for_test(&self, window: &Window) -> Vec<bool> {
        let len = self.view().map_or(0, |v| v.len());
        self.column_name_tips(0..len, window)
    }
}
