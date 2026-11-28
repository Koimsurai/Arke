//! Home, Settings and Recycle Bin are pages owned by a single tab, so they can sit beside folder tabs
//! as in Files. A page survives tab switches and closes only when its tab navigates away or is closed.

use std::path::PathBuf;

use gpui::{App, Context, EntityId, Window};

use super::Workspace;
use crate::assets::Icon;
use crate::explorer::PageClosed;

/// The owning tab: its list view (which side in dual pane) and the folder it was on when the page opened.
#[derive(Clone, Debug)]
pub(super) struct Owner {
    pub(super) view: EntityId,
    path: Option<PathBuf>,
    /// The tab was opened just for this page, so closing the page closes the tab too.
    own_tab: bool,
    /// For an own tab: the tab it was opened from, shown again when the page closes (like a
    /// browser), instead of whichever tab happens to be next to it.
    opener: Option<EntityId>,
}

impl Owner {
    /// The tab was opened just for this page.
    pub(super) const fn own_tab(&self) -> bool {
        self.own_tab
    }
}

impl Workspace {
    /// Makes the active tab the owner.
    pub(super) fn owner_here(&mut self, own_tab: bool, cx: &App) -> Owner {
        let opener = self.page_opener.take().filter(|_| own_tab);
        let view = self.active_view();
        Owner {
            view: view.entity_id(),
            path: view.read(cx).display_path().map(PathBuf::from),
            own_tab,
            opener,
        }
    }

    /// A folder key pressed on a page that covers the active tab (Ctrl+K, Ctrl+F, Ctrl+L,
    /// Alt+Up...): the page steps aside, the tab stays, and the key acts on the folder under it
    /// (#77).
    pub(super) fn page_key(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        act: impl FnOnce(
            &mut crate::explorer::Explorer,
            &mut Window,
            &mut Context<crate::explorer::Explorer>,
        ),
    ) {
        // A tab opened just for the page closes with it (unless it's the last), so the key acts
        // on the tab that was there before instead of leaving a stray one. That tab may show a
        // page too (What's new opened from Settings): step aside until a folder shows.
        let mut stepped = false;
        while let Some(owner) = self.take_shown_page() {
            self.dismiss_page(&owner, window, cx);
            stepped = true;
        }
        if !stepped {
            return;
        }
        let view = self.active_view().clone();
        view.update(cx, |e, cx| act(e, window, cx));
        cx.notify();
    }

    /// History stepped to `page` in `view`: shows it over the tab. Every tab has its own Home
    /// page; only one Recycle Bin page exists, so one open in another tab moves here.
    pub(super) fn show_history_page(
        &mut self,
        view: &gpui::Entity<crate::explorer::Explorer>,
        page: arke_core::NavPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A page only shows over the active pane: the pane whose button was used comes first.
        if let Some((tab, pane)) = self.locate(view)
            && self.active_view().entity_id() != view.entity_id()
        {
            self.activate_pane(tab, pane, window, cx);
        }
        self.clear_pages_here();
        match page {
            arke_core::NavPage::Home => {
                self.show_home(false, window, cx);
                self.focus_active(window, cx);
            }
            arke_core::NavPage::RecycleBin => {
                self.recycle_page = None;
                self.show_recycle(false, window, cx);
            }
        }
        cx.notify();
    }

    /// History stepped from a page to the folder under it in `view`: the page closes, the tab
    /// stays.
    pub(super) fn hide_pages_of(
        &mut self,
        view: &gpui::Entity<crate::explorer::Explorer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = view.entity_id();
        self.home_pages.retain(|p| p.owner.view != id);
        if self
            .recycle_page
            .as_ref()
            .is_some_and(|p| p.owner.view == id)
        {
            self.recycle_page = None;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// A pane the history has on a page that the workspace no longer shows (the page was closed
    /// without going anywhere) is on the folder under it, as a folder opened from the page.
    pub(super) fn sync_history_pages(&mut self, cx: &mut Context<Self>) {
        let shown = |owner: Option<&Owner>| owner.map(|o| o.view);
        let recycle = shown(self.recycle_page.as_ref().map(|p| &p.owner));
        let views: Vec<_> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter().map(|p| p.view.clone()))
            .collect();
        for view in views {
            let id = view.entity_id();
            let covered = match view.read(cx).page() {
                None => continue,
                Some(arke_core::NavPage::Home) => {
                    self.home_pages.iter().any(|p| p.owner.view == id)
                }
                Some(arke_core::NavPage::RecycleBin) => recycle == Some(id),
            };
            if covered {
                continue;
            }
            match view.update(cx, |e, _| e.page_closed()) {
                PageClosed::Nothing => {}
                PageClosed::Stepped => self.note_navigation(id, cx),
                PageClosed::Undone => self.undo_nav_step(id, cx),
            }
        }
    }

    /// Closes the page the active tab shows, keeping the tab.
    pub(super) fn drop_shown_page(&mut self) {
        let _ = self.take_shown_page();
    }

    /// Removes the page the active tab shows and returns its owner (`None` when a folder shows).
    fn take_shown_page(&mut self) -> Option<Owner> {
        if self.home_open() {
            self.take_home_here().map(|p| p.owner)
        } else if self.settings_open() {
            self.settings_panel.take().map(|p| p.owner)
        } else if self.recycle_open() {
            self.recycle_page.take().map(|p| p.owner)
        } else if self.whats_new_open() {
            self.whats_new.take().map(|p| p.owner)
        } else {
            None
        }
    }

    /// Whether a page covers the active tab.
    pub(super) fn page_shown(&self) -> bool {
        self.home_open() || self.settings_open() || self.recycle_open() || self.whats_new_open()
    }

    /// Whether the page is visible, i.e. the active tab owns it.
    pub(super) fn shows(&self, owner: &Owner) -> bool {
        self.active_view().entity_id() == owner.view
    }

    /// Index of the owning tab, or `None` once it is closed.
    pub(super) fn tab_of(&self, owner: &Owner) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.panes.iter().any(|p| p.view.entity_id() == owner.view))
    }

    /// The tab `view` was moved to `path` without the user going anywhere (its drive was removed):
    /// the pages it shows stay open over the new folder instead of closing as on a navigation.
    /// Returns whether a page covers the tab.
    pub(super) fn keep_pages_over(&mut self, view: EntityId, path: &std::path::Path) -> bool {
        let homes = self.home_pages.iter_mut().map(|p| &mut p.owner);
        let owners = [
            self.settings_panel.as_mut().map(|p| &mut p.owner),
            self.recycle_page.as_mut().map(|p| &mut p.owner),
            self.whats_new.as_mut().map(|p| &mut p.owner),
        ];
        let mut covered = false;
        for owner in homes.chain(owners.into_iter().flatten()) {
            if owner.view == view {
                owner.path = Some(path.to_path_buf());
                covered = true;
            }
        }
        covered
    }

    /// The owner still exists and is still on the original folder.
    pub(super) fn owner_alive(&self, owner: &Owner, cx: &App) -> bool {
        self.tabs
            .iter()
            .flat_map(|t| t.panes.iter())
            .find(|p| p.view.entity_id() == owner.view)
            .is_some_and(|p| p.view.read(cx).display_path() == owner.path.as_deref())
    }

    /// Focus handle of the page the active tab shows, if one covers its list.
    pub(super) fn page_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        if self.settings_open() {
            return self.settings_panel.as_ref().map(|p| p.focus.clone());
        }
        if self.recycle_open() {
            return self.recycle_focus(cx);
        }
        if self.home_open() {
            return self.home_focus(cx);
        }
        if self.whats_new_open() {
            return self.whats_new_focus(cx);
        }
        None
    }

    /// Page shown in this list view, used for the tab title.
    pub(super) fn page_title(&self, view: EntityId) -> Option<(String, Icon)> {
        let owns = |owner: &Owner| owner.view == view;
        if self.settings_panel.as_ref().is_some_and(|p| owns(&p.owner)) {
            return Some((rust_i18n::t!("settings.title").into_owned(), Icon::Settings));
        }
        if self.recycle_page.as_ref().is_some_and(|p| owns(&p.owner)) {
            return Some((rust_i18n::t!("recycle.title").into_owned(), Icon::Trash));
        }
        if self.home_pages.iter().any(|p| owns(&p.owner)) {
            return Some((rust_i18n::t!("home.title").into_owned(), Icon::KfHome));
        }
        if self.whats_new.as_ref().is_some_and(|p| owns(&p.owner)) {
            return Some((rust_i18n::t!("whats_new.title").into_owned(), Icon::Bolt));
        }
        None
    }

    /// One page per tab: closes whatever page this tab already has before opening another.
    pub(super) fn clear_pages_here(&mut self) {
        let here = self.active_view().entity_id();
        if self
            .settings_panel
            .as_ref()
            .is_some_and(|p| p.owner.view == here)
        {
            self.settings_panel = None;
        }
        if self
            .recycle_page
            .as_ref()
            .is_some_and(|p| p.owner.view == here)
        {
            self.recycle_page = None;
        }
        self.home_pages.retain(|p| p.owner.view != here);
        if self
            .whats_new
            .as_ref()
            .is_some_and(|p| p.owner.view == here)
        {
            self.whats_new = None;
        }
    }

    /// If the page is already open in another tab, switches there and returns true.
    pub(super) fn go_to_page(
        &mut self,
        owner: Option<&Owner>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ix) = owner.and_then(|o| self.tab_of(o)) else {
            return false;
        };
        if ix != self.active {
            self.activate(ix, window, cx);
        }
        true
    }

    /// Opens a tab for a page, at the current folder, right after the active tab.
    pub(super) fn open_page_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.current_dir(cx);
        let at = self.active + 1;
        self.page_opener = Some(self.active_view().entity_id());
        self.open_tab(path, at, true, window, cx);
    }

    /// Closes the page, along with its tab if the tab was opened for it.
    pub(super) fn dismiss_page(
        &mut self,
        owner: &Owner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if owner.own_tab
            && let Some(ix) = self.tab_of(owner)
            && self.tabs.len() > 1
        {
            let opener = owner.opener.and_then(|view| {
                self.tabs
                    .iter()
                    .position(|t| t.panes.iter().any(|p| p.view.entity_id() == view))
            });
            self.close_tab(ix, window, cx);
            if let Some(opener) = opener.map(|o| if o > ix { o - 1 } else { o })
                && opener != self.active
            {
                self.activate(opener, window, cx);
            }
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// The window's title: the active tab's folder or page, as the taskbar and tools that read
    /// window titles (Listary, AutoHotkey) see it.
    pub(crate) fn window_title(&self, cx: &App) -> String {
        let view = self.active_view();
        let page = self
            .page_title(view.entity_id())
            .filter(|_| {
                self.home_open()
                    || self.settings_open()
                    || self.recycle_open()
                    || self.whats_new_open()
            })
            .map(|(title, _)| title);
        if let Some(page) = page {
            return format!("{page} - Arke");
        }
        let explorer = view.read(cx);
        match explorer.display_path() {
            Some(_) => format!("{} - Arke", explorer.tab_title()),
            None => "Arke".into(),
        }
    }

    pub(super) fn sync_window_title(&mut self, window: &mut Window, cx: &App) {
        let title = self.window_title(cx);
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }
}

#[cfg(test)]
impl Workspace {
    /// Title of each tab; pages report their own name.
    pub(crate) fn tab_titles_for_test(&self, cx: &App) -> Vec<String> {
        self.tabs
            .iter()
            .map(|t| {
                let view = t.view();
                self.page_title(view.entity_id())
                    .map_or_else(|| view.read(cx).tab_title().to_string(), |(title, _)| title)
            })
            .collect()
    }

    pub(crate) fn open_home_for_test(
        &mut self,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_home(new_tab, window, cx);
    }

    /// Home as the start of the tab (Home at startup or in a new window).
    pub(crate) fn open_start_home_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_home_as(false, true, window, cx);
    }

    /// A folder tile on Home was clicked.
    pub(crate) fn home_open_folder_for_test(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.home_opens_folder(path, window, cx);
    }

    pub(crate) fn close_home_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_home(window, cx);
    }

    pub(crate) fn open_settings_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(&super::settings_panel::OpenSettings, window, cx);
    }

    pub(crate) fn close_settings_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_settings(&super::settings_panel::CloseSettings, window, cx);
    }

    /// Page shown in the active tab, if any.
    pub(crate) fn shown_page_for_test(&self) -> Option<String> {
        self.page_title(self.active_view().entity_id())
            .filter(|_| {
                self.home_open()
                    || self.settings_open()
                    || self.recycle_open()
                    || self.whats_new_open()
            })
            .map(|(title, _)| title)
    }
}
