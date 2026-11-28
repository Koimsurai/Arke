//! Home page. Opened from the top of the sidebar and replaces the panes, like the Recycle Bin page.
//! The page itself is in `home.rs`; this handles open/close, the top bar, and yielding on navigation.

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Subscription, Window, div, prelude::*, svg,
};
use rust_i18n::t;
use std::sync::Arc;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::home::{Favorite, HomeEvent, HomePage};
use crate::settings::SettingsStore;
use crate::theme;

pub(super) struct HomeView {
    view: Entity<HomePage>,
    /// Tab that owns the page.
    pub(super) owner: super::pages::Owner,
    _events: Subscription,
}

/// The favorites as the sidebar's tiles show them: same list and order, a name of its own first
/// (#259), otherwise the place's (mapped drives keep their custom name) or the folder's; same icons.
fn sidebar_favorites(places: &[crate::shell::Place], cx: &mut App) -> Vec<Favorite> {
    super::sidebar::favorite_tiles(places, cx)
}

impl Workspace {
    /// The Home page the active tab shows. Each tab that shows Home has its own page (#530).
    fn home_here(&self) -> Option<&HomeView> {
        self.home_pages.iter().find(|p| self.shows(&p.owner))
    }

    /// Whether the active tab is showing Home.
    pub(super) fn home_open(&self) -> bool {
        self.home_here().is_some()
    }

    /// Removes the Home page the active tab shows and returns it.
    pub(super) fn take_home_here(&mut self) -> Option<HomeView> {
        let ix = self.home_pages.iter().position(|p| self.shows(&p.owner))?;
        Some(self.home_pages.remove(ix))
    }

    /// Whether tab `ix` shows Home (one of its panes owns a Home page).
    pub(super) fn tab_has_home(&self, ix: usize) -> bool {
        self.home_pages
            .iter()
            .any(|p| self.tab_of(&p.owner) == Some(ix))
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
            Target::Home => self.open_home_as(false, true, window, cx),
            Target::RecycleBin => self.open_recycle(false, window, cx),
        }
    }

    /// Tab or Shift+Tab inside the Home page: on to its next or previous section. `false` when
    /// the page has none left, so the keyboard goes on to the rest of the window.
    pub(super) fn home_tab_section(&self, forward: bool, cx: &mut Context<Self>) -> bool {
        self.home_here()
            .is_some_and(|p| p.view.update(cx, |home, cx| home.tab_section(forward, cx)))
    }

    /// Tab or Shift+Tab brings the keyboard to the Home page: its first or last section.
    pub(super) fn home_enter(&self, forward: bool, cx: &mut Context<Self>) {
        if let Some(p) = self.home_here() {
            p.view.update(cx, |home, cx| home.enter(forward, cx));
        }
    }

    pub(super) fn home_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        use gpui::Focusable as _;
        self.home_here().map(|p| p.view.focus_handle(cx))
    }

    /// Switches to the tab that already has it open, otherwise opens it in a new or the current tab.
    pub(super) fn open_home(&mut self, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open_home_as(new_tab, new_tab, window, cx);
    }

    /// Ctrl+T (and the New tab buttons) set to Home: always a new tab with Home, even when Home
    /// is already open somewhere. Every tab has its own Home page, so the others stay as they are.
    pub(super) fn open_home_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_home_fresh(true, true, window, cx);
    }

    /// [`Self::open_home`]; `start`: Home is where the tab starts (a new window or tab, or Home
    /// at startup), so nothing comes before it in the history.
    pub(super) fn open_home_as(
        &mut self,
        new_tab: bool,
        start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .home_here()
            .or_else(|| self.home_pages.first())
            .map(|p| p.owner.clone());
        if self.go_to_page(existing.as_ref(), window, cx) {
            return;
        }
        self.open_home_fresh(new_tab, start, window, cx);
    }

    /// Shows Home in a new tab or the active one, whether or not another tab shows it.
    pub(super) fn open_home_fresh(
        &mut self,
        new_tab: bool,
        start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if new_tab {
            self.open_page_tab(window, cx);
        } else {
            self.clear_pages_here();
        }
        self.show_home(new_tab, window, cx);
        let view = self.active_view().clone();
        if view.update(cx, |e, _| e.enter_page(arke_core::NavPage::Home, start)) {
            self.note_navigation(view.entity_id(), cx);
        }
    }

    /// Shows the Home page over the active tab (`new_tab`: the tab was opened for it). The
    /// history is the caller's: this only draws the page.
    pub(super) fn show_home(&mut self, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let shell = Arc::clone(&self.services.shell);
        let places = self.active_view().read(cx).places.clone();
        let shares = SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        let favorites = sidebar_favorites(&places, cx);
        let view = cx.new(|cx| HomePage::new(shell, places, favorites, shares, cx));
        let events = cx.subscribe_in(&view, window, Self::on_home_event);
        let owner = self.owner_here(new_tab, cx);
        // One page per tab: a page this tab already has is replaced.
        self.home_pages.retain(|p| p.owner.view != owner.view);
        self.home_pages.push(HomeView {
            view,
            owner,
            _events: events,
        });
        self.focus_active(window, cx);
        cx.notify();
    }

    /// What a folder tile on Home does (tests drive it without the page's own events).
    #[cfg(test)]
    pub(super) fn home_opens_folder(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(page) = self.home_here() {
            let view = page.view.clone();
            self.on_home_event(&view, &HomeEvent::OpenFolder(path), window, cx);
        }
    }

    pub(super) fn close_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.take_home_here() {
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
        // Right click on a card or a recent file: its menu, Home stays.
        match event {
            HomeEvent::PlaceMenu { path, position } => {
                let items = self.tree_menu(path, 0, cx);
                self.open_sidebar_menu(*position, items, cx);
                return;
            }
            HomeEvent::FavoriteMenu { path, position } => {
                let items = self.folder_menu(path, super::sidebar::MenuFor::HomeFavorite, cx);
                self.open_sidebar_menu(*position, items, cx);
                return;
            }
            HomeEvent::FileMenu { path, position } => {
                let items = super::sidebar::file_menu(path);
                self.open_sidebar_menu(*position, items, cx);
                return;
            }
            // Home's favorites are the sidebar's: reordering them on Home saves the same order.
            HomeEvent::MoveFavorite { path, to } => {
                self.move_pin(path, crate::settings::PinSlot::Tile(*to), cx);
                return;
            }
            _ => {}
        }
        self.run_home_event(event, window, cx);
    }

    /// What a pick on Home does (also from a recent file's menu): opens the folder, here or
    /// elsewhere, or goes to the file.
    pub(super) fn run_home_event(
        &mut self,
        event: &HomeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Middle or Ctrl+click: a new tab, in the background (Home stays) or in front.
        if let HomeEvent::OpenInNewTab { path, activate } = event {
            let at = self.active + 1;
            self.open_folder_tab(path.clone(), at, *activate, window, cx);
            return;
        }
        if self.home_opens_elsewhere(event, window, cx) {
            return;
        }
        // Home yields and the folder opens in the same tab, even if the tab was created for Home.
        // Back from that folder brings Home back: the history has Home as the place it came from.
        self.take_home_here();
        self.focus_active(window, cx);
        let view = self.active_view().clone();
        match event {
            HomeEvent::OpenFolder(path) => {
                let path = path.clone();
                view.update(cx, |e, cx| e.navigate(path, arke_core::NavKind::New, cx));
            }
            HomeEvent::OpenRecycle => self.open_recycle(false, window, cx),
            HomeEvent::OpenInNewTab { .. }
            | HomeEvent::PlaceMenu { .. }
            | HomeEvent::FavoriteMenu { .. }
            | HomeEvent::FileMenu { .. }
            | HomeEvent::MoveFavorite { .. } => {}
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

    /// A folder picked on Home that doesn't open under it: in a pinned tab, which keeps Home, it
    /// opens in a new tab; on Home in a tab of its own (Ctrl+T), a tab already showing it comes
    /// forward and the Home tab closes (one tab per folder). Returns whether it was handled.
    fn home_opens_elsewhere(
        &mut self,
        event: &HomeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (folder, select) = match event {
            HomeEvent::OpenFolder(path) => (path.clone(), Vec::new()),
            HomeEvent::Reveal(path) => match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => (
                    parent.to_path_buf(),
                    vec![std::sync::Arc::from(name.to_string_lossy().as_ref())],
                ),
                _ => return false,
            },
            HomeEvent::OpenRecycle
            | HomeEvent::OpenInNewTab { .. }
            | HomeEvent::PlaceMenu { .. }
            | HomeEvent::FavoriteMenu { .. }
            | HomeEvent::FileMenu { .. }
            | HomeEvent::MoveFavorite { .. } => {
                return false;
            }
        };
        if self.active_pinned_elsewhere(&folder, cx) {
            let at = self.active + 1;
            self.open_folder_tab(folder.clone(), at, true, window, cx);
        } else {
            let own_tab = self.home_here().is_some_and(|p| p.owner.own_tab());
            let Some(view) = self
                .tab_showing(&folder, cx)
                .filter(|&(tab, _)| own_tab && tab != self.active && Self::one_tab_per_folder(cx))
                .map(|(tab, pane)| self.tabs[tab].panes[pane].view.clone())
            else {
                return false;
            };
            if let Some(page) = self.take_home_here() {
                self.dismiss_page(&page.owner, window, cx);
            }
            if let Some((tab, pane)) = self.locate(&view) {
                self.activate_pane(tab, pane, window, cx);
            }
        }
        if !select.is_empty() {
            self.active_view()
                .update(cx, |e, cx| e.select_in_folder(&folder, select, cx));
        }
        true
    }

    /// Called before paint: closes each Home once its tab is gone or has navigated elsewhere.
    /// Also follows drive changes of the one shown (the others catch up when their tab shows).
    pub(super) fn home_follow_navigation(&mut self, cx: &mut App) {
        if self.home_pages.is_empty() {
            return;
        }
        if self
            .home_pages
            .iter()
            .any(|p| !self.owner_alive(&p.owner, cx))
        {
            let pages = std::mem::take(&mut self.home_pages);
            self.home_pages = pages
                .into_iter()
                .filter(|p| self.owner_alive(&p.owner, cx))
                .collect();
        }
        let Some(page) = self.home_here() else {
            return;
        };
        let places = self.active_view().read(cx).places.clone();
        let shares = SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        let names = SettingsStore::global(cx)
            .read(cx)
            .settings
            .place_names
            .clone();
        let rename = self
            .sidebar_ui
            .place_rename
            .as_ref()
            .map(|r| (r.path.clone(), r.input.clone()));
        let favorites = sidebar_favorites(&places, cx);
        page.view.update(cx, |home, _| {
            home.set_locations(places, shares);
            home.set_favorites(favorites);
            home.set_names(names, rename);
        });
    }

    /// Reloads on window activation; recent files may have changed.
    pub(super) fn home_refresh(&self, cx: &mut App) {
        for page in &self.home_pages {
            page.view.update(cx, HomePage::reload);
        }
    }

    /// Recent files the Home page lists (`None` while loading or closed).
    #[cfg(test)]
    pub(crate) fn home_recent_for_test(&self, cx: &App) -> Option<Vec<std::path::PathBuf>> {
        self.home_here()?.view.read(cx).recent_for_test()
    }

    /// The favorites Home lists, as `(path, name)` (`None` while closed).
    #[cfg(test)]
    pub(crate) fn home_favorites_for_test(
        &self,
        cx: &App,
    ) -> Option<Vec<(std::path::PathBuf, String)>> {
        Some(self.home_here()?.view.read(cx).favorites_for_test())
    }

    /// The card under Home's keyboard cursor (`None` while closed or with no ring and nothing
    /// picked).
    #[cfg(test)]
    pub(crate) fn home_cursor_for_test(&self, cx: &App) -> Option<String> {
        self.home_here()?.view.read(cx).cursor_for_test()
    }

    /// Whether Home draws its recent files section (tests).
    #[cfg(test)]
    pub(crate) fn home_recent_shown_for_test(&self, cx: &App) -> Option<bool> {
        Some(self.home_here()?.view.read(cx).recent_shown_for_test())
    }

    /// Sidebar Home button. Shows as selected while open; clicking again closes it. `None` while
    /// Home is hidden (#323): a right click on it offers that.
    pub(super) fn render_home_button(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        use crate::settings::SidebarSection;
        if super::sidebar::section_hidden(SidebarSection::Home, cx) {
            return None;
        }
        let open = self.home_open();
        let row = div()
            .id("open-home")
            .debug_selector(|| "open-home".to_owned())
            .a11y(gpui::Role::Link, t!("home.title"))
            .relative()
            .flex_none()
            .h(theme::h_row_side())
            .mx(theme::s_8())
            .mb(theme::s_4())
            .pl(theme::s_8())
            .flex()
            .items_center()
            .gap(theme::s_8())
            .rounded(theme::r_8())
            .cursor_pointer()
            .when(open, |d| d.bg(theme::selection()).text_color(theme::text()))
            .when(!open, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
            })
            .child(crate::assets::tinted(Icon::KfHome, theme::icon(), theme::text()))
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
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    let items = super::sidebar::section_menu(SidebarSection::Home, cx);
                    this.open_sidebar_menu(e.position, items, cx);
                }),
            );
        Some(row)
    }

    /// Home page for the content area, or `None` when closed.
    pub(super) fn render_home_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let page = self.home_here()?;
        let top_bar = div()
            .h(theme::h_bar())
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::s_2())
            .pr(theme::s_8())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::s_8())
                    .px(theme::s_8())
                    .child(
                        svg()
                            .path(Icon::KfHome.path())
                            .size(theme::icon())
                            .text_color(theme::text_muted()),
                    )
                    .child(
                        div()
                            .font_weight(theme::FW_STRONG)
                            .child(t!("home.title").into_owned()),
                    ),
            )
            .child(super::drag_area("home-drag", cx).flex_1().h_full())
            .child(crate::explorer::caption_buttons(window));
        Some(
            div()
                .id("home-page")
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(top_bar)
                .children(self.tab_strip_under_bar(window, cx))
                .child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .mr(theme::paper_gap())
                        .mb(theme::paper_gap())
                        .rounded(theme::r_paper())
                        .bg(theme::paper())
                        .shadow(theme::elev_2())
                        .overflow_hidden()
                        .child(page.view.clone()),
                )
                .into_any_element(),
        )
    }
}
