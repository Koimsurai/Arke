//! Recycle Bin page. Opened from its row in the sidebar's pinned list; replaces the panes, like Settings.
//! The page itself is in `recycle_bin.rs`; this handles open/close, the top bar, and yielding on navigation.

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, MouseButton, MouseDownEvent, Subscription,
    Window, div, prelude::*, svg,
};
use rust_i18n::t;

use super::Workspace;
use super::sidebar::SidebarAction;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::recycle_bin::{RecycleBin, RecycleEvent};
use crate::settings::SettingsStore;
use crate::theme;

pub(super) struct RecyclePage {
    view: Entity<RecycleBin>,
    /// Tab that owns the page.
    pub(super) owner: super::pages::Owner,
    _events: Subscription,
}

impl Workspace {
    /// Whether the active tab is showing the Recycle Bin.
    pub(super) fn recycle_open(&self) -> bool {
        self.recycle_page
            .as_ref()
            .is_some_and(|p| self.shows(&p.owner))
    }

    /// Switches to the tab that already has it open, otherwise opens it in a new or the current tab.
    pub(super) fn open_recycle(
        &mut self,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.recycle_page.as_ref().map(|p| p.owner.clone());
        if self.go_to_page(existing.as_ref(), window, cx) {
            return;
        }
        if new_tab {
            self.open_page_tab(window, cx);
        } else {
            self.clear_pages_here();
        }
        let shell = self.services.shell.clone();
        let view = cx.new(|cx| RecycleBin::new(shell, window, cx));
        let events = cx.subscribe_in(&view, window, Self::on_recycle_event);
        self.recycle_page = Some(RecyclePage {
            view,
            owner: self.owner_here(new_tab, cx),
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn recycle_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        use gpui::Focusable as _;
        self.recycle_page.as_ref().map(|p| p.view.focus_handle(cx))
    }

    pub(super) fn close_recycle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.recycle_page.take() {
            self.dismiss_page(&page.owner, window, cx);
        }
    }

    fn on_recycle_event(
        &mut self,
        _: &Entity<RecycleBin>,
        event: &RecycleEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            RecycleEvent::Close => self.close_recycle(window, cx),
            RecycleEvent::Changed => self.refresh_disk_space(cx),
            RecycleEvent::OpenFolder(path) => {
                self.close_recycle(window, cx);
                let path = path.clone();
                self.active_view().update(cx, |explorer, cx| {
                    explorer.navigate(path, arke_core::NavKind::New, cx);
                });
            }
        }
    }

    /// Called before paint: closes the page once its tab is gone or has navigated elsewhere.
    pub(super) fn recycle_follow_navigation(&mut self, cx: &App) {
        let Some(page) = &self.recycle_page else {
            return;
        };
        if !self.owner_alive(&page.owner, cx) {
            self.recycle_page = None;
        }
    }

    /// Reloads on window activation; another program may have changed the bin.
    pub(super) fn recycle_refresh(&self, cx: &mut App) {
        if let Some(page) = &self.recycle_page {
            page.view.update(cx, RecycleBin::reload);
        }
    }

    #[cfg(test)]
    pub(crate) fn recycle_for_test(&self) -> Option<Entity<RecycleBin>> {
        self.recycle_page.as_ref().map(|page| page.view.clone())
    }

    #[cfg(test)]
    pub(crate) fn open_recycle_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_recycle(false, window, cx);
    }

    /// Pinned-list row for the Recycle Bin. Click opens it here (Ctrl or middle click: new tab).
    pub(super) fn render_recycle_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let open = self.recycle_open();
        let menu = vec![
            (
                t!("sidebar.open_in_new_tab").to_string().into(),
                SidebarAction::OpenRecycleInNewTab,
            ),
            (
                t!("sidebar.unpin").to_string().into(),
                SidebarAction::SetRecyclePinned(false),
            ),
        ];
        div()
            .id("open-recycle")
            .a11y(gpui::Role::TreeItem, t!("recycle.title"))
            .aria_selected(open)
            .h(theme::H_ROW)
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_4)
            .pl(theme::S_4)
            .pr(theme::S_8)
            .rounded(theme::R_8)
            .cursor_pointer()
            // Shown in the active tab: marked like the current folder in the tree above.
            .relative()
            .when(open, |d| d.bg(theme::selection()))
            .when(!open, |d| d.hover(|s| s.bg(theme::wash())))
            // Same width as a tree chevron, so the icon lines up with the folders above.
            .child(div().size(theme::S_20).flex_none())
            .child(
                svg()
                    .path(Icon::Trash.path())
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
                    .child(t!("recycle.title").into_owned()),
            )
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                this.open_recycle(e.modifiers().control, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.open_recycle(true, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.open_sidebar_menu(e.position, menu.clone(), cx);
                }),
            )
            .into_any_element()
    }

    /// Settings changes repaint the workspace, so no notify is needed.
    pub(super) fn set_recycle_pinned(pinned: bool, cx: &mut App) {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.recycle_pinned = pinned, cx));
    }

    /// Recycle Bin page for the content area, or `None` when closed.
    pub(super) fn render_recycle_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let page = self
            .recycle_page
            .as_ref()
            .filter(|p| self.shows(&p.owner))?;
        let horizontal = Self::horizontal_tabs(cx);
        let pinned = SettingsStore::global(cx).read(cx).settings.recycle_pinned;
        let top_bar = div()
            .h(theme::H_BAR)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_2)
            .pr(theme::S_8)
            // With horizontal tabs the sidebar has no back button, so add one here.
            .when(horizontal, |d| {
                d.child(crate::explorer::ghost_button(
                    "recycle-back",
                    Icon::ArrowLeft,
                    true,
                    t!("nav.back"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.close_recycle(window, cx);
                    }),
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_8)
                    .px(theme::S_8)
                    .child(
                        svg()
                            .path(Icon::Trash.path())
                            .size(theme::ICON)
                            .text_color(theme::text_muted()),
                    )
                    .child(
                        div()
                            .font_weight(theme::FW_STRONG)
                            .child(t!("recycle.title").into_owned()),
                    ),
            )
            .child(super::drag_area("recycle-drag", cx).flex_1().h_full())
            // Unpinned from the sidebar: offer to pin it back.
            .when(!pinned, |d| {
                d.child(crate::explorer::ghost_button(
                    "recycle-pin",
                    Icon::Pin,
                    true,
                    t!("sidebar.pin"),
                    |_: &ClickEvent, _: &mut Window, cx: &mut App| Self::set_recycle_pinned(true, cx),
                ))
            })
            .when(!horizontal, |d| {
                d.child(crate::explorer::caption_buttons(window))
            });
        Some(
            div()
                .id("recycle-page")
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(top_bar)
                .child(
                    // Same paper surface as the file list.
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .mr(theme::S_8)
                        .mb(theme::S_8)
                        .rounded(theme::R_12)
                        .bg(theme::paper())
                        .shadow(theme::elev_2())
                        .overflow_hidden()
                        .child(page.view.clone()),
                )
                .into_any_element(),
        )
    }
}
