//! The shelf in the sidebar (`shelf.rs` holds the items).
//!
//! - The Shelf row is always there: drop files on it (from Arke or Windows) to add them, click it
//!   to show or hide the items. Its menu has "Copy here", "Move here" and "Clear shelf".
//! - Each item shows its name and folder; click goes to it, drag copies it out (Shift moves), the
//!   x removes it. Missing items are dimmed.
//! - "Copy here" / "Move here" act on the active tab's folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::filetype::{FileKind, kind_for_extension};
use gpui::{
    AnyElement, ClickEvent, Context, ExternalPaths, MouseButton, MouseDownEvent, SharedString,
    Window, div, prelude::*, svg,
};
use rust_i18n::t;

use super::Workspace;
use super::sidebar::{SidebarAction, tooltip_text};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{DragPreview, DraggedFiles, ShelfDrag, kind_icon};
use crate::shelf::{ShelfItem, ShelfStore};
use crate::theme;

/// Icon kind for a shelf item, from what the shelf recorded (no disk access).
fn item_kind(item: &ShelfItem) -> FileKind {
    if item.is_dir {
        return FileKind::Folder;
    }
    item.path
        .extension()
        .and_then(|e| e.to_str())
        .map_or(FileKind::Other, |e| {
            kind_for_extension(&e.to_ascii_lowercase())
        })
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The folder an item is in, by name (a drive root keeps its full name, `D:\`).
fn folder_name(path: &Path) -> String {
    path.parent().map_or_default(file_name)
}

impl Workspace {
    /// Where "Copy here" and "Move here" go: the active tab's folder, unless a page (Settings,
    /// Recycle Bin, Home) covers it.
    pub(super) fn shelf_destination(&self, cx: &gpui::App) -> Option<PathBuf> {
        let view = self.active_view();
        if self.page_title(view.entity_id()).is_some() {
            return None;
        }
        view.read(cx).current_path().map(Path::to_path_buf)
    }

    /// Runs a shelf action from the sidebar menu or buttons.
    pub(super) fn run_shelf_action(
        &mut self,
        action: &SidebarAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SidebarAction::ShelfToHere { is_move } => {
                if self.shelf_destination(cx).is_none() {
                    return;
                }
                let is_move = *is_move;
                self.active_view()
                    .update(cx, |e, cx| e.shelf_to_here(is_move, cx));
            }
            SidebarAction::ShelfClear => {
                ShelfStore::global(cx).update(cx, ShelfStore::clear);
            }
            SidebarAction::ShelfRemove(path) => {
                ShelfStore::global(cx).update(cx, |s, cx| s.remove(path, cx));
            }
            SidebarAction::ShelfReveal(path) => self.reveal_from_shelf(path.clone(), window, cx),
            _ => {}
        }
    }

    /// Goes to an item's folder in the active tab and selects it.
    fn reveal_from_shelf(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let (parent, name) = (parent.to_path_buf(), name.to_string_lossy().into_owned());
        self.active_view().update(cx, |e, cx| {
            e.navigate_selecting(parent, arke_core::NavKind::New, Some(name.into()), cx);
        });
        self.focus_active(window, cx);
    }

    /// Shows or hides the items; opening rechecks which ones still exist.
    fn toggle_shelf(&mut self, cx: &mut Context<Self>) {
        self.shelf_open = !self.shelf_open;
        if self.shelf_open {
            let fs = self.services.fs.clone();
            ShelfStore::global(cx).update(cx, |s, cx| s.recheck(fs, cx));
        }
        cx.notify();
    }

    fn shelf_menu(&self, cx: &gpui::App) -> Vec<(SharedString, SidebarAction)> {
        let store = ShelfStore::global_ref(cx);
        let any = store.is_some_and(|s| !s.shelf().present().is_empty());
        let mut items = Vec::new();
        if any && self.shelf_destination(cx).is_some() {
            items.push((
                t!("shelf.copy_here").into_owned().into(),
                SidebarAction::ShelfToHere { is_move: false },
            ));
            items.push((
                t!("shelf.move_here").into_owned().into(),
                SidebarAction::ShelfToHere { is_move: true },
            ));
        }
        if store.is_some_and(|s| !s.shelf().is_empty()) {
            items.push((
                t!("shelf.clear").into_owned().into(),
                SidebarAction::ShelfClear,
            ));
        }
        items
    }

    /// The Shelf row and, when open, its items and actions.
    // The row is one element chain (look, drop target, drag source, menu); split up it reads worse.
    #[allow(clippy::too_many_lines)]
    pub(super) fn render_shelf(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let held = ShelfStore::global(cx).read(cx).shelf().clone();
        let present: Vec<PathBuf> = held.present();
        let open = self.shelf_open;
        let menu = self.shelf_menu(cx);
        let source = self.active_view().downgrade();
        let mut header = div()
            .id("shelf-row")
            .a11y(gpui::Role::Button, t!("shelf.title"))
            .aria_expanded(open)
            .relative()
            .h(theme::H_ROW)
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_4)
            .pl(theme::S_4)
            .pr(theme::S_8)
            .rounded(theme::R_8)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(
                div()
                    .size(theme::S_20)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .path(Icon::ChevronRight.path())
                            .size(theme::ICON_SM)
                            .text_color(theme::text_subtle())
                            .when(open, |s| {
                                s.with_transformation(gpui::Transformation::rotate(
                                    gpui::percentage(0.25),
                                ))
                            }),
                    ),
            )
            .child(
                svg()
                    .path(Icon::Shelf.path())
                    .size(theme::ICON)
                    .flex_none()
                    .text_color(theme::text_muted()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .pl(theme::S_4)
                    .child(t!("shelf.title").into_owned()),
            )
            .when(!held.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_subtle())
                        .child(arke_core::format::format_count(held.len())),
                )
            })
            // Explains itself until it's in use.
            .when(held.is_empty(), |d| {
                d.tooltip(tooltip_text(t!("shelf.tip").into_owned()))
            })
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_shelf(cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    if !menu.is_empty() {
                        this.open_sidebar_menu(e.position, menu.clone(), cx);
                    }
                }),
            )
            // Files dropped here join the shelf; nothing is copied or moved.
            .can_drop(|value, _, _| {
                value
                    .downcast_ref::<DraggedFiles>()
                    .is_none_or(|d| d.shelf.is_none())
            })
            .drag_over::<DraggedFiles>(|s, _, _, _| {
                s.bg(theme::signal_wash())
                    .border_color(theme::signal_line())
            })
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(theme::signal_wash()))
            .on_drop(cx.listener(|this, dragged: &DraggedFiles, _, cx| {
                let items = dragged
                    .source
                    .upgrade()
                    .map_or_default(|e| e.read(cx).drag_items(dragged.ix));
                this.add_to_shelf_from_drop(items, cx);
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                // Windows doesn't say which are folders; the recheck finds out.
                let items = paths.paths().iter().map(|p| (p.clone(), false)).collect();
                this.add_to_shelf_from_drop(items, cx);
            }));
        // The whole shelf drags out from its row.
        if !present.is_empty() {
            let dragged = DraggedFiles {
                source: source.clone(),
                ix: 0,
                shelf: Some(ShelfDrag {
                    kind: match held.items() {
                        [one] => item_kind(one),
                        _ => FileKind::Other,
                    },
                    paths: Arc::new(present),
                }),
            };
            header = header.on_drag(dragged, |dragged, grab, _, cx| {
                let preview = DragPreview::start(dragged, grab, cx);
                cx.new(|_| preview)
            });
        }
        let mut out = vec![header.into_any_element()];
        if !open {
            return out;
        }
        if held.is_empty() {
            out.push(
                div()
                    .h(theme::H_ROW)
                    .flex()
                    .flex_none()
                    .items_center()
                    .pl(theme::S_4 + theme::S_20 + theme::S_4)
                    .pr(theme::S_8)
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(t!("shelf.empty").into_owned())
                    .into_any_element(),
            );
            return out;
        }
        for (ix, item) in held.items().iter().enumerate() {
            out.push(Self::shelf_item_row(ix, item, &source, cx));
        }
        out.push(self.shelf_actions(&held, cx));
        out
    }

    /// Files dropped on the Shelf row: add them and say so on the active tab.
    fn add_to_shelf_from_drop(&mut self, items: Vec<(PathBuf, bool)>, cx: &mut Context<Self>) {
        let unknown = items.iter().any(|(_, is_dir)| !is_dir);
        self.active_view()
            .update(cx, |e, cx| e.add_dropped_to_shelf(items, cx));
        if unknown {
            let fs = self.services.fs.clone();
            ShelfStore::global(cx).update(cx, |s, cx| s.recheck(fs, cx));
        }
        cx.notify();
    }

    // One row's look, drag, menu and remove button, kept together like the tree rows.
    #[allow(clippy::too_many_lines)]
    fn shelf_item_row(
        ix: usize,
        item: &ShelfItem,
        source: &gpui::WeakEntity<crate::explorer::Explorer>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = item.path.clone();
        let name = file_name(&path);
        let folder = folder_name(&path);
        let missing = item.missing;
        let tip = if missing {
            t!("shelf.missing", path = path.display()).into_owned()
        } else {
            path.display().to_string()
        };
        let group: SharedString = format!("shelf-item-{ix}").into();
        let menu = vec![
            (
                t!("shelf.show_in_folder").into_owned().into(),
                SidebarAction::ShelfReveal(path.clone()),
            ),
            (
                t!("shelf.remove").into_owned().into(),
                SidebarAction::ShelfRemove(path.clone()),
            ),
        ];
        let remove = path.clone();
        let reveal = path.clone();
        let spoken = (name.clone(), folder.clone(), missing);
        let row = div()
            .id(("shelf-item", ix))
            .a11y_lazy(gpui::Role::ListItem, move || {
                let (name, folder, missing) = &spoken;
                let state = if *missing {
                    t!("shelf.missing_short").into_owned()
                } else {
                    String::new()
                };
                crate::a11y::item_label(name, &[folder.as_str(), state.as_str()])
            })
            .group(group.clone())
            .h(theme::H_ROW)
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_8)
            // Lines up under the Shelf label, like a tree child.
            .pl(theme::S_4 + theme::S_20 + theme::S_4)
            .pr(theme::S_4)
            .rounded(theme::R_8)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .when(missing, |d| d.opacity(0.5))
            .child(kind_icon(item_kind(item), theme::ICON))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(theme::S_6)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .when(missing, gpui::Styled::line_through)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_shrink(2.)
                            .min_w_0()
                            .truncate()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle())
                            .child(if missing {
                                t!("shelf.missing_short").into_owned()
                            } else {
                                folder
                            }),
                    ),
            )
            .child(
                div()
                    .id(("shelf-remove", ix))
                    .a11y_button(t!("shelf.remove"))
                    .size(theme::S_20)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(theme::R_4)
                    .invisible()
                    .group_hover(group, gpui::Styled::visible)
                    .hover(|s| s.bg(theme::wash_strong()))
                    .child(
                        svg()
                            .path(Icon::Close.path())
                            .size(theme::ICON_SM)
                            .text_color(theme::text_muted()),
                    )
                    .tooltip(tooltip_text(t!("shelf.remove").into_owned()))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        ShelfStore::global(cx).update(cx, |s, cx| s.remove(&remove, cx));
                    })),
            )
            .tooltip(tooltip_text(tip))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.reveal_from_shelf(reveal.clone(), window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.open_sidebar_menu(e.position, menu.clone(), cx);
                }),
            );
        if missing {
            return row.into_any_element();
        }
        let dragged = DraggedFiles {
            source: source.clone(),
            ix: 0,
            shelf: Some(ShelfDrag {
                paths: Arc::new(vec![path]),
                kind: item_kind(item),
            }),
        };
        row.on_drag(dragged, |dragged, grab, _, cx| {
            let preview = DragPreview::start(dragged, grab, cx);
            cx.new(|_| preview)
        })
        .into_any_element()
    }

    /// "Copy here", "Move here" and "Clear" under the items.
    fn shelf_actions(&self, held: &crate::shelf::Shelf, cx: &mut Context<Self>) -> AnyElement {
        let target = self.shelf_destination(cx);
        let can = target.is_some() && !held.present().is_empty();
        let here = target.as_deref().map(file_name);
        let button = |id: &'static str, label: String, tip: Option<String>, enabled: bool| {
            div()
                .id(id)
                .a11y_button(label.clone())
                .a11y_disabled(!enabled)
                .h(theme::S_24)
                .px(theme::S_8)
                .flex()
                .flex_none()
                .items_center()
                .rounded(theme::R_4)
                .text_size(theme::FS_12)
                .when(enabled, |d| {
                    d.cursor_pointer()
                        .text_color(theme::text_muted())
                        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                })
                .when(!enabled, |d| d.text_color(theme::text_disabled()))
                .when_some(tip, |d, tip| d.tooltip(tooltip_text(tip)))
                .child(label)
        };
        let to_here = |is_move: bool| match &here {
            Some(name) if is_move => t!("shelf.move_to", name = name).into_owned(),
            Some(name) => t!("shelf.copy_to", name = name).into_owned(),
            None => t!("shelf.no_folder").into_owned(),
        };
        div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap(theme::S_2)
            .pl(theme::S_4 + theme::S_20)
            .pb(theme::S_4)
            .child(
                button(
                    "shelf-copy-here",
                    t!("shelf.copy_here").into_owned(),
                    Some(to_here(false)),
                    can,
                )
                .when(can, |d| {
                    d.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.run_shelf_action(
                            &SidebarAction::ShelfToHere { is_move: false },
                            window,
                            cx,
                        );
                    }))
                }),
            )
            .child(
                button(
                    "shelf-move-here",
                    t!("shelf.move_here").into_owned(),
                    Some(to_here(true)),
                    can,
                )
                .when(can, |d| {
                    d.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.run_shelf_action(
                            &SidebarAction::ShelfToHere { is_move: true },
                            window,
                            cx,
                        );
                    }))
                }),
            )
            .child(div().flex_1())
            .child(
                button(
                    "shelf-clear",
                    t!("shelf.clear_short").into_owned(),
                    None,
                    true,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.run_shelf_action(&SidebarAction::ShelfClear, window, cx);
                })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_folders() {
        assert_eq!(file_name(Path::new(r"C:\a\b.txt")), "b.txt");
        assert_eq!(folder_name(Path::new(r"C:\a\b.txt")), "a");
        assert_eq!(folder_name(Path::new(r"D:\b.txt")), r"D:\");
        let item = ShelfItem {
            path: PathBuf::from(r"C:\a\photo.JPG"),
            is_dir: false,
            missing: false,
        };
        assert_eq!(item_kind(&item), FileKind::Image);
        let dir = ShelfItem {
            is_dir: true,
            ..item
        };
        assert_eq!(item_kind(&dir), FileKind::Folder);
    }
}
