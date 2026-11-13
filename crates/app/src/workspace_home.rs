//! Home page. Opened from the top of the sidebar and replaces the panes, like the Recycle Bin page.
//! The page itself is in `home.rs`; this handles open/close, the top bar, and yielding on navigation.

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Subscription, Window, div, prelude::*, svg,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::home::{HomeEvent, HomePage};
use crate::settings::SettingsStore;
use crate::theme;

pub(super) struct HomeView {
    view: Entity<HomePage>,
    /// Tab that owns the page.
    pub(super) owner: super::pages::Owner,
    _events: Subscription,
}

impl Workspace {
    /// Whether the active tab is showing Home.
    pub(super) fn home_open(&self) -> bool {
        self.home_page
            .as_ref()
            .is_some_and(|p| self.shows(&p.owner))
    }

    /// A new window opened for a folder another program (File Explorer) was asked to show: the
    /// folder is already loading; select what it was asked to select, or show Home / the Recycle Bin.
    pub(crate) fn show_taken_over(
        &mut self,
        target: arke_core::takeover::Target,
        select: Vec<std::sync::Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use arke_core::takeover::Target;
        match target {
            Target::Folder(_) => {
                self.active_view()
                    .update(cx, |explorer, _| explorer.select_on_arrival(select));
            }
            Target::Home => self.open_home(false, window, cx),
            Target::RecycleBin => self.open_recycle(false, window, cx),
        }
    }

    pub(super) fn home_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        use gpui::Focusable as _;
        self.home_page.as_ref().map(|p| p.view.focus_handle(cx))
    }

    /// Switches to the tab that already has it open, otherwise opens it in a new or the current tab.
    pub(super) fn open_home(&mut self, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.home_page.as_ref().map(|p| p.owner.clone());
        if self.go_to_page(existing.as_ref(), window, cx) {
            return;
        }
        if new_tab {
            self.open_page_tab(window, cx);
        } else {
            self.clear_pages_here();
        }
        let shell = self.services.shell.clone();
        let places = self.active_view().read(cx).places.clone();
        let shares = SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        let view = cx.new(|cx| HomePage::new(shell, places, shares, cx));
        let events = cx.subscribe_in(&view, window, Self::on_home_event);
        self.home_page = Some(HomeView {
            view,
            owner: self.owner_here(new_tab, cx),
            _events: events,
        });
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(super) fn close_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.home_page.take() {
            self.dismiss_page(&page.owner, window, cx);
        }
    }

    fn on_home_event(
        &mut self,
        _: &Entity<HomePage>,
        event: &HomeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Middle click: a new tab in the background; Home stays.
        if let HomeEvent::OpenInNewTab(path) = event {
            self.open_in_background(path.clone(), window, cx);
            return;
        }
        // Home yields and the folder opens in the same tab, even if the tab was created for Home.
        self.home_page = None;
        self.focus_active(window, cx);
        let view = self.active_view().clone();
        match event {
            HomeEvent::OpenFolder(path) => {
                let path = path.clone();
                view.update(cx, |e, cx| e.navigate(path, arke_core::NavKind::New, cx));
            }
            HomeEvent::OpenRecycle => self.open_recycle(false, window, cx),
            HomeEvent::OpenInNewTab(_) => {}
            HomeEvent::Reveal(path) => {
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    return;
                };
                let (parent, name) = (parent.to_path_buf(), name.to_string_lossy().into());
                view.update(cx, |e, cx| {
                    e.navigate_selecting(parent, arke_core::NavKind::New, Some(name), cx);
                });
            }
        }
    }

    /// Called before paint: closes Home once its tab is gone or has navigated elsewhere.
    /// Also follows drive changes.
    pub(super) fn home_follow_navigation(&mut self, cx: &mut App) {
        let Some(page) = &self.home_page else {
            return;
        };
        if !self.owner_alive(&page.owner, cx) {
            self.home_page = None;
            return;
        }
        let places = self.active_view().read(cx).places.clone();
        let shares = SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        page.view
            .update(cx, |home, _| home.set_locations(places, shares));
    }

    /// Reloads on window activation; recent files may have changed.
    pub(super) fn home_refresh(&self, cx: &mut App) {
        if let Some(page) = &self.home_page {
            page.view.update(cx, HomePage::reload);
        }
    }

    /// Recent files the Home page lists (`None` while loading or closed).
    #[cfg(test)]
    pub(crate) fn home_recent_for_test(&self, cx: &App) -> Option<Vec<std::path::PathBuf>> {
        self.home_page.as_ref()?.view.read(cx).recent_for_test()
    }

    /// Sidebar Home button. Shows as selected while open; clicking again closes it.
    pub(super) fn render_home_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open = self.home_open();
        div()
            .id("open-home")
            .a11y(gpui::Role::Link, t!("home.title"))
            .relative()
            .flex_none()
            .h(theme::H_ROW)
            .mx(theme::S_8)
            .mb(theme::S_4)
            .pl(theme::S_8)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_8)
            .cursor_pointer()
            .when(open, |d| d.bg(theme::selection()).text_color(theme::text()))
            .when(!open, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
            })
            .child(crate::assets::tinted(Icon::KfHome, theme::ICON, theme::text()))
            .child(t!("home.title").into_owned())
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                if open {
                    this.close_home(window, cx);
                } else {
                    this.open_home(e.modifiers().control, window, cx);
                }
            }))
            // Middle click opens it in a new tab.
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    this.open_home(true, window, cx);
                }),
            )
    }

    /// Home page for the content area, or `None` when closed.
    pub(super) fn render_home_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let page = self.home_page.as_ref().filter(|p| self.shows(&p.owner))?;
        let horizontal = Self::horizontal_tabs(cx);
        let top_bar = div()
            .h(theme::H_BAR)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_2)
            .pr(theme::S_8)
            .when(horizontal, |d| {
                d.child(crate::explorer::ghost_button(
                    "home-back",
                    Icon::ArrowLeft,
                    true,
                    t!("nav.back"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.close_home(window, cx);
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
                            .path(Icon::KfHome.path())
                            .size(theme::ICON)
                            .text_color(theme::text_muted()),
                    )
                    .child(
                        div()
                            .font_weight(theme::FW_STRONG)
                            .child(t!("home.title").into_owned()),
                    ),
            )
            .child(super::drag_area("home-drag", cx).flex_1().h_full())
            .when(!horizontal, |d| {
                d.child(crate::explorer::caption_buttons(window))
            });
        Some(
            div()
                .id("home-page")
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(top_bar)
                .child(
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
