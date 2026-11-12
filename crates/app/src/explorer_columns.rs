//! Column view (like Finder's columns): parent | current folder | preview.
//!
//! - Left column lists the parent folder with the current folder marked; clicking another folder
//!   goes there.
//! - Middle column is the current folder (names only); Left goes up, Right enters a folder.
//! - Right column shows the preview without opening the preview pane.
//!
//! The parent is listed in the background (`Fs::read_dir`, never on the UI thread) and only when
//! the folder changes.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::filetype::FileKind;
use arke_core::{Entry, NavKind};
use gpui::{
    AnyElement, ClickEvent, Context, ScrollStrategy, UniformListScrollHandle, div, prelude::*, px,
    uniform_list,
};

use super::view::listed_visual;
use super::{AppEvent, Explorer, ViewMode, spawn_thread};
use crate::media::MediaKey;
use crate::theme;

/// Width of the left (parent) column
pub(super) const PARENT_W: gpui::Pixels = px(240.);

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
        let fs = self.services.fs.clone();
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
        let start = range.start;
        range
            .filter_map(|ix| entries.get(ix).map(|e| (ix, e)))
            .map(|(ix, entry)| {
                let path = dir.join(&*entry.name);
                let current = here.as_deref() == Some(path.as_path());
                let is_dir = entry.is_dir_like();
                let name: Arc<str> = entry.name.clone();
                let parent = dir.clone();
                div()
                    .id(("parent-col", ix))
                    .relative()
                    .h(theme::H_ROW_COMPACT)
                    .px(theme::S_8)
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .rounded(theme::R_8)
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
                        theme::ICON,
                        theme::ICON,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(super::view::shown_name(entry, show_ext)),
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
                                Some(name.clone()),
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
            .w(PARENT_W)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .pt(theme::S_4)
            .border_r_1()
            .border_color(theme::line());
        let Some(parent) = &self.parent_column else {
            return column.into_any_element();
        };
        let Some(entries) = &parent.entries else {
            return column
                .items_center()
                .justify_center()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(rust_i18n::t!("info.loading").to_string())
                .into_any_element();
        };
        column
            .child(
                uniform_list(
                    "parent-column",
                    entries.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        this.render_parent_rows(range, cx)
                    }),
                )
                .track_scroll(&parent.scroll)
                .px(theme::S_8)
                .flex_1(),
            )
            .into_any_element()
    }

    /// Middle column rows show only the name; folders get a trailing arrow.
    pub(super) fn column_row_tail(entry: &Entry) -> Option<AnyElement> {
        (entry.is_dir_like() && arke_core::filetype::file_kind(entry) == FileKind::Folder)
            .then(|| chevron().into_any_element())
    }
}

fn chevron() -> impl IntoElement {
    gpui::svg()
        .path(crate::assets::Icon::ChevronRight.path())
        .size(theme::ICON_SM)
        .flex_none()
        .text_color(theme::text_subtle())
}

#[cfg(test)]
impl Explorer {
    pub(crate) fn parent_column_names(&self) -> Vec<String> {
        self.parent_column
            .as_ref()
            .and_then(|c| c.entries.as_ref())
            .map(|e| e.iter().map(|e| e.name.to_string()).collect())
            .unwrap_or_default()
    }
}
