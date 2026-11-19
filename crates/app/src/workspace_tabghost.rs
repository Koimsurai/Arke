//! The dragged tab drawn beside the pointer once it leaves the window, where GPUI's own drag image
//! stops: the tab, and what letting go there does ("Open in a new window", "Move to this window").
//! It is a popup window of its own (`popup.rs`) that the mouse passes through, drawn a little right
//! of and below the pointer, so the window found under the pointer on release is the one below it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use arke_core::filetype::FileKind;
use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement as _, Pixels, Point, SharedString, Styled,
    Window, div,
};
use rust_i18n::t;

use super::Workspace;
use crate::theme::{self, UiFont as _};

/// The dragged tab beside the pointer, while it is outside the window.
pub(crate) struct TabGhost {
    popup: Rc<RefCell<Option<Rc<crate::popup::PopupState>>>>,
    hint: Rc<RefCell<SharedString>>,
    alive: Rc<Cell<bool>>,
}

impl Drop for TabGhost {
    /// The window closed during the drag: the drag image inside the other windows is not left
    /// switched off.
    fn drop(&mut self) {
        if self.alive.get() {
            SHOWN.with(|s| s.set(false));
        }
    }
}

/// How far right of and below the pointer the dragged tab is drawn (logical pixels).
const GHOST_OFFSET: f32 = 14.;

thread_local! {
    /// The dragged tab is drawn beside the pointer: the drag image inside the window, stuck at its
    /// edge, is left out.
    static SHOWN: Cell<bool> = const { Cell::new(false) };
}

/// Whether the dragged tab is drawn beside the pointer, outside the window.
pub(crate) fn shown() -> bool {
    SHOWN.with(Cell::get)
}

impl Workspace {
    /// The pointer moved to `local` (in this window) during a tab drag; `outside`: past its edge.
    pub(super) fn tab_ghost_moved(
        &mut self,
        outside: bool,
        local: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.dragging_tab.filter(|_| outside) else {
            self.close_tab_ghost(cx);
            return;
        };
        let screen = window.bounds().origin + local;
        let hint: SharedString = if self.arke_window_at(screen, window, cx).is_some() {
            t!("tabs.drop_into_window").into()
        } else if self.tabs.len() > 1 {
            t!("tabs.drop_new_window").into()
        } else {
            // The only tab dropped on nothing stays where it is: nothing to show.
            self.close_tab_ghost(cx);
            return;
        };
        let (x, y) = crate::popup::to_screen(window, local);
        let offset = (GHOST_OFFSET * window.scale_factor()).round() as i32;
        let at = (x + offset, y + offset);
        // Near the right or bottom edge it goes to the other side of the pointer.
        let moved = crate::popup::Moved {
            at,
            flip: (x - offset, y - offset),
        };
        if let Some(ghost) = &self.tab_ghost {
            let changed = *ghost.hint.borrow() != hint;
            if changed {
                *ghost.hint.borrow_mut() = hint;
            }
            if let Some(popup) = ghost.popup.borrow().as_ref() {
                popup.move_to(moved, cx);
                if changed && let Some(window) = popup.window() {
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, _| window.refresh());
                    });
                }
            }
            return;
        }
        let title = self.dragged_tab_title(from, cx);
        let ghost = TabGhost {
            popup: Rc::default(),
            hint: Rc::new(RefCell::new(hint)),
            alive: Rc::new(Cell::new(true)),
        };
        let content: crate::popup::Content = {
            let (hint, alive) = (Rc::clone(&ghost.hint), Rc::clone(&ghost.alive));
            Rc::new(move |_, window, _| {
                if !alive.get() {
                    return None;
                }
                crate::popup::pass_mouse_through(window);
                Some(ghost_chip(title.clone(), hint.borrow().clone()))
            })
        };
        SHOWN.with(|s| s.set(true));
        let slot = Rc::clone(&ghost.popup);
        let alive = Rc::clone(&ghost.alive);
        let owner = window.window_handle();
        self.tab_ghost = Some(ghost);
        // Not now: the popup draws as it opens.
        cx.defer(move |cx| {
            if !alive.get() {
                return;
            }
            let state = crate::popup::open(
                owner,
                crate::popup::Anchor::At(at.0, at.1),
                &crate::popup::Group::default(),
                content,
                Rc::new(|_| {}),
                false,
                cx,
            );
            // Dropped in the meantime: it goes again at once.
            if !alive.get()
                && let Some(window) = state.as_ref().and_then(|s| s.window())
            {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| window.refresh());
                });
            }
            if let Some(state) = &state {
                state.move_to(moved, cx);
            }
            *slot.borrow_mut() = state;
        });
    }

    /// The pointer came back, or the drag ended: the dragged tab beside the pointer goes.
    pub(super) fn close_tab_ghost(&mut self, cx: &mut App) {
        let Some(ghost) = self.tab_ghost.take() else {
            return;
        };
        SHOWN.with(|s| s.set(false));
        ghost.alive.set(false);
        let popup = ghost.popup.borrow_mut().take();
        if let Some(window) = popup.and_then(|p| p.window()) {
            // Its content is gone: drawn again, it closes itself.
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, _| window.refresh());
            });
        }
    }

    /// Whether the dragged tab is shown beside the pointer (tests).
    #[cfg(test)]
    pub(crate) fn tab_ghost_shown_for_test(&self) -> Option<String> {
        self.tab_ghost
            .as_ref()
            .filter(|g| g.alive.get())
            .map(|g| g.hint.borrow().to_string())
    }

    /// The title a tab shows, both sides for a split tab.
    fn dragged_tab_title(&self, ix: usize, cx: &App) -> SharedString {
        let Some(tab) = self.tabs.get(ix) else {
            return SharedString::default();
        };
        tab.panes
            .iter()
            .map(|p| {
                self.page_title(p.view.entity_id())
                    .map_or_else(|| p.view.read(cx).tab_title(), |(title, _)| title.into())
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(" | ")
            .into()
    }
}

/// The dragged tab as drawn beside the pointer: like the drag image inside the window, with what
/// letting go there does under it.
fn ghost_chip(title: SharedString, hint: SharedString) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(theme::s_2())
        .py(theme::s_6())
        .px(theme::s_8())
        .rounded(theme::r_8())
        .bg(theme::overlay())
        .shadow(theme::elev_float())
        .ui_font()
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .text_size(theme::fs_14())
                .text_color(theme::text())
                .child(crate::explorer::kind_icon(FileKind::Folder, theme::icon()))
                .child(title),
        )
        .child(
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(hint),
        )
        .into_any_element()
}
