//! Home, Settings and Recycle Bin are pages owned by a single tab, so they can sit beside folder tabs
//! as in Files. A page survives tab switches and closes only when its tab navigates away or is closed.

use std::path::PathBuf;

use gpui::{App, Context, EntityId, Window};

use super::Workspace;
use crate::assets::Icon;

/// The owning tab: its list view (which side in dual pane) and the folder it was on when the page opened.
#[derive(Clone, Debug)]
pub(super) struct Owner {
    view: EntityId,
    path: Option<PathBuf>,
    /// The tab was opened just for this page, so closing the page closes the tab too.
    own_tab: bool,
}

impl Workspace {
    /// Makes the active tab the owner.
    pub(super) fn owner_here(&self, own_tab: bool, cx: &App) -> Owner {
        let view = self.active_view();
        Owner {
            view: view.entity_id(),
            path: view.read(cx).display_path().map(PathBuf::from),
            own_tab,
        }
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
        if self.home_page.as_ref().is_some_and(|p| owns(&p.owner)) {
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
        if self
            .home_page
            .as_ref()
            .is_some_and(|p| p.owner.view == here)
        {
            self.home_page = None;
        }
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
            self.close_tab(ix, window, cx);
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
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
