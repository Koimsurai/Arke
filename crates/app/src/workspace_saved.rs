//! Dual pane extras and saved workspaces, the parts of `Workspace` that look at more than one
//! pane or tab at a time:
//!
//! - **Synchronized browsing** (Ctrl+Shift+S): going into a subfolder or up in the active pane
//!   repeats the same relative move in the other pane, when that folder exists there.
//! - **Compare folders** (Ctrl+Shift+D): marks items that only one side has or that differ in size
//!   or modified time (`arke_core::compare`, run off the UI thread). Stops when either pane goes
//!   to another folder; refreshes when a listing changes.
//! - **Saved workspaces**: the open tabs, with their splits and folders, saved under a name
//!   (`saved_workspaces.rs`). Opening one replaces the open tabs; pinned tabs stay.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::NavKind;
use arke_core::path::{PathMove, apply_move, path_move};
use gpui::{
    AnimationExt, App, ClickEvent, Context, Entity, EventEmitter, Focusable, SharedString,
    Subscription, Task, Window, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{Tab, Workspace};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::Explorer;
use crate::saved_workspaces::{SavedWorkspace, WorkspaceStore, clean_name};
use crate::settings::SessionTab;
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};
use crate::workspace::TooltipBelowExt as _;

/// A running folder comparison in a split tab.
pub(super) struct CompareState {
    /// Listing (generation, version) of each pane the current marks belong to; when either
    /// changes (a refresh, a file copied over) the comparison runs again.
    seen: Option<[(u64, u64); 2]>,
    _task: Option<Task<()>>,
}

impl Workspace {
    // --- Synchronized browsing ---

    pub(super) fn toggle_sync(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(tab_ix).filter(|t| t.is_split()) {
            tab.synced = !tab.synced;
            // Start from where both panes are now.
            for pane in &mut tab.panes {
                pane.last = pane.view.read(cx).current_path().map(Path::to_path_buf);
            }
            cx.notify();
        }
    }

    /// A pane finished opening `path`. Ends a comparison, and with synchronized browsing repeats
    /// the move in the other pane (only for moves the user made in the active pane, so the
    /// repeated move doesn't bounce back).
    pub(super) fn pane_navigated(
        &mut self,
        tab_ix: usize,
        pane_ix: usize,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_ix) else {
            return;
        };
        let Some(pane) = tab.panes.get_mut(pane_ix) else {
            return;
        };
        let old = pane.last.replace(path.to_path_buf());
        if old.as_deref() == Some(path) {
            // A refresh: the comparison picks the new listing up by itself.
            return;
        }
        if tab.compare.take().is_some() {
            for pane in &tab.panes {
                pane.view.update(cx, |e, cx| e.set_compare(None, cx));
            }
            cx.notify();
        }
        if !tab.synced || !tab.is_split() || pane_ix != tab.active_pane {
            return;
        }
        let Some(step) = old.and_then(|old| path_move(&old, path)) else {
            return;
        };
        let other = tab.panes[1 - pane_ix.min(1)].view.clone();
        let Some(base) = other.read(cx).current_path().map(Path::to_path_buf) else {
            return;
        };
        // A parent always exists; a subfolder of the same name may not, which is looked up off
        // the UI thread (it may be a slow network folder).
        let fs = self.services.fs.clone();
        let (tx, rx) = flume::bounded(1);
        let from = base.clone();
        crate::explorer::spawn_thread("sync-browse", move || {
            let target = match &step {
                PathMove::Down(names) => find_subfolder(fs.as_ref(), &from, names),
                PathMove::Up(_) => apply_move(&from, &step),
            };
            let _ = tx.send(target);
        });
        cx.spawn(async move |_, cx| {
            let Ok(Some(target)) = rx.recv_async().await else {
                return;
            };
            other.update(cx, |e, cx| {
                // Skip it if that pane went somewhere else in the meantime.
                if e.current_path() == Some(base.as_path()) {
                    e.navigate(target, NavKind::New, cx);
                }
            });
        })
        .detach();
    }

    // --- Compare folders ---

    pub(super) fn toggle_compare(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(tab_ix).filter(|t| t.is_split()) else {
            return;
        };
        tab.compare = match tab.compare.take() {
            Some(_) => None,
            None => Some(CompareState {
                seen: None,
                _task: None,
            }),
        };
        for pane in &tab.panes {
            pane.view.update(cx, |e, cx| e.set_compare(None, cx));
        }
        cx.notify();
    }

    pub(super) fn select_differences(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(tab_ix).filter(|t| t.compare.is_some()) else {
            return;
        };
        for pane in &tab.panes {
            pane.view.update(cx, Explorer::select_compared);
        }
    }

    /// Before paint: (re)runs the active tab's comparison once both listings are complete and
    /// whenever either changes. The work happens on a background thread.
    pub(super) fn refresh_compare(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.compare.is_none() || !tab.is_split() {
            return;
        }
        let listing = |ix: usize| {
            tab.panes[ix]
                .view
                .read(cx)
                .model
                .as_ref()
                .filter(|m| m.complete)
                .map(|m| ((m.generation, m.version), m.entries.clone()))
        };
        let (Some((left_key, left)), Some((right_key, right))) = (listing(0), listing(1)) else {
            return;
        };
        let key = [left_key, right_key];
        let id = tab.id;
        let views = [tab.panes[0].view.clone(), tab.panes[1].view.clone()];
        if tab.compare.as_ref().is_some_and(|c| c.seen == Some(key)) {
            return;
        }
        let work = cx
            .background_executor()
            .spawn(async move { arke_core::compare::compare(&left, &right) });
        let task = cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                // Still comparing the same listings?
                let current = this
                    .tabs
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.compare.as_ref())
                    .is_some_and(|c| c.seen == Some(key));
                if !current {
                    return;
                }
                let [left, right] = views;
                left.update(cx, |e, cx| e.set_compare(Some(Arc::new(result.left)), cx));
                right.update(cx, |e, cx| e.set_compare(Some(Arc::new(result.right)), cx));
            });
        });
        self.tabs[self.active].compare = Some(CompareState {
            seen: Some(key),
            _task: Some(task),
        });
    }

    // --- Saved workspaces ---

    /// Opens the Workspaces panel: save the open tabs, open, rename or delete saved ones.
    pub(crate) fn open_workspaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.is_some() {
            return;
        }
        let snapshot = self.session_snapshot(cx);
        let panel = cx.new(|cx| WorkspacesPanel::new(snapshot.tabs, snapshot.active, window, cx));
        let events = cx.subscribe_in(&panel, window, |this, _, event, window, cx| {
            this.workspaces = None;
            if let PanelEvent::Open(name) = event {
                this.open_saved_workspace(name, window, cx);
            }
            this.focus_active(window, cx);
            cx.notify();
        });
        self.workspaces = Some((panel, events));
        cx.notify();
    }

    /// Opens the saved workspace `name`: its tabs replace the open ones, except pinned tabs,
    /// which stay (a pinned tab the workspace also has isn't opened twice). The replaced tabs can
    /// be brought back with Ctrl+Shift+T.
    pub(crate) fn open_saved_workspace(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved: Option<SavedWorkspace> = WorkspaceStore::global(cx)
            .read(cx)
            .workspaces()
            .get(name)
            .cloned();
        let Some(saved) = saved.filter(|w| !w.tabs.is_empty()) else {
            return;
        };
        let replaced: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| !t.pinned)
            .map(|t| t.id)
            .collect();
        let pinned: Vec<(u64, Vec<PathBuf>)> = self
            .tabs
            .iter()
            .filter(|t| t.pinned)
            .map(|t| (t.id, tab_paths(t, cx)))
            .collect();
        let mut active = None;
        let mut first = None;
        for (ix, tab) in saved.tabs.iter().enumerate() {
            let already = tab
                .pinned
                .then(|| pinned.iter().find(|(_, p)| *p == tab.panes))
                .flatten();
            let id = match already {
                Some((id, _)) => *id,
                None => match self.open_saved_tab(tab, window, cx) {
                    Some(id) => id,
                    None => continue,
                },
            };
            first.get_or_insert(id);
            if ix == saved.active {
                active = Some(id);
            }
        }
        // Close the replaced tabs from the back so indices stay valid.
        for ix in (0..self.tabs.len()).rev() {
            if replaced.contains(&self.tabs[ix].id) {
                self.remember_closed(ix, cx);
                self.tabs.remove(ix);
            }
        }
        self.keep_pins_first();
        let active = active.or(first);
        self.active = self
            .tabs
            .iter()
            .position(|t| Some(t.id) == active)
            .unwrap_or(0);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Opens one saved tab at the end; returns its id.
    fn open_saved_tab(
        &mut self,
        saved: &SessionTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let first = saved.panes.first()?.clone();
        let at = self.tabs.len();
        let ix = self.insert_tab(first, at, false, window, cx);
        if let Some(second) = saved.panes.get(1) {
            self.split(ix, second.clone(), window, cx);
        }
        let tab = self.tabs.get_mut(ix)?;
        if tab.is_split() {
            tab.active_pane = saved.active_pane.min(1);
            tab.ratio = saved.ratio.clamp(0.2, 0.8);
            tab.synced = saved.synced;
        }
        tab.pinned = saved.pinned;
        Some(tab.id)
    }
}

/// `base` followed by `names`, each matched case-insensitively against the folders actually
/// there (so the path keeps the other side's own spelling). `None` if one is missing.
fn find_subfolder(
    fs: &dyn arke_core::Fs,
    base: &Path,
    names: &[std::ffi::OsString],
) -> Option<PathBuf> {
    let mut dir = base.to_path_buf();
    for name in names {
        let want = name.to_string_lossy().to_lowercase();
        let found = fs
            .read_dir(&dir)
            .ok()?
            .filter_map(Result::ok)
            .find(|e| e.is_dir_like() && *e.sort_key == *want.as_str())?;
        dir.push(&*found.name);
    }
    Some(dir)
}

/// Folder of each pane of a tab.
fn tab_paths(tab: &Tab, cx: &App) -> Vec<PathBuf> {
    tab.panes
        .iter()
        .filter_map(|p| p.view.read(cx).display_path().map(Path::to_path_buf))
        .collect()
}

impl Workspace {
    /// Tabs header and tab strip button that opens the Workspaces panel.
    pub(super) fn workspaces_button(cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("open-workspaces")
            .a11y_button(t!("workspaces.title"))
            .size(theme::S_24)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Workspaces.path())
                    .size(theme::ICON)
                    .text_color(theme::text_subtle()),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_workspaces(window, cx);
            }))
            .tooltip_below(t!("workspaces.title").into_owned())
    }

    #[cfg(test)]
    pub(crate) fn workspaces_panel_for_test(&self) -> Option<Entity<WorkspacesPanel>> {
        self.workspaces.as_ref().map(|(panel, _)| panel.clone())
    }
}

/// What the panel asks the window to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PanelEvent {
    Open(String),
    Closed,
}

/// The Workspaces panel: a name field to save the open tabs, then the saved workspaces, each
/// opened with a click and renamed or deleted from its row. It saves and edits the list itself;
/// only opening one goes back to the window.
pub(crate) struct WorkspacesPanel {
    /// The open tabs when the panel opened (it is modal, so they can't change meanwhile).
    tabs: Vec<SessionTab>,
    active: usize,
    name: Entity<TextInput>,
    error: Option<SharedString>,
    /// Row being renamed: its current name and the input.
    renaming: Option<(String, Entity<TextInput>, Subscription)>,
    rename_error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for WorkspacesPanel {}

impl WorkspacesPanel {
    fn new(
        tabs: Vec<SessionTab>,
        active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| TextInput::new(t!("workspaces.name_hint").into_owned(), cx));
        // A free name ready to keep or type over.
        let suggested = WorkspaceStore::global(cx)
            .read(cx)
            .workspaces()
            .free_name(&t!("workspaces.default_name"));
        name.update(cx, |input, cx| {
            input.set_text(suggested, cx);
            input.select_all_text(cx);
        });
        let focus = name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let events = cx.subscribe_in(&name, window, |this, _, event, _, cx| match event {
            InputEvent::Confirm => this.save(cx),
            InputEvent::Cancel => cx.emit(PanelEvent::Closed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify();
            }
        });
        let store = WorkspaceStore::global(cx);
        let store = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            tabs,
            active,
            name,
            error: None,
            renaming: None,
            rename_error: None,
            _subscriptions: vec![events, store],
        }
    }

    /// Saves the open tabs under the typed name (replacing a workspace of that name).
    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).committed_text();
        let (tabs, active) = (self.tabs.clone(), self.active);
        let result = WorkspaceStore::global(cx).update(cx, |store, cx| {
            store.update(|w| w.save(&name, tabs, active), cx)
        });
        match result {
            Ok(_) => cx.emit(PanelEvent::Closed),
            Err(problem) => {
                self.error = Some(problem.to_string().into());
                cx.notify();
            }
        }
    }

    fn start_rename(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new(String::new(), cx));
        input.update(cx, |input, cx| {
            input.set_text(name.clone(), cx);
            input.select_all_text(cx);
        });
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let events = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            InputEvent::Confirm => this.finish_rename(window, cx),
            InputEvent::Cancel => this.cancel_rename(window, cx),
            InputEvent::Changed => {
                this.rename_error = None;
                cx.notify();
            }
        });
        self.renaming = Some((name, input, events));
        self.rename_error = None;
        cx.notify();
    }

    fn finish_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((from, input, _)) = self.renaming.as_ref() else {
            return;
        };
        let (from, to) = (from.clone(), input.read(cx).committed_text());
        let result = WorkspaceStore::global(cx)
            .update(cx, |store, cx| store.update(|w| w.rename(&from, &to), cx));
        match result {
            Ok(_) => self.cancel_rename(window, cx),
            Err(problem) => {
                self.rename_error = Some(problem.to_string().into());
                cx.notify();
            }
        }
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.rename_error = None;
        let focus = self.name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn delete(name: &str, cx: &mut Context<Self>) {
        WorkspaceStore::global(cx).update(cx, |store, cx| store.update(|w| w.remove(name), cx));
    }

    #[cfg(test)]
    pub(crate) fn type_name_for_test(&mut self, name: &str, cx: &mut Context<Self>) {
        self.name
            .update(cx, |input, cx| input.set_text(name.to_owned(), cx));
        self.save(cx);
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<String> {
        self.error.as_ref().map(ToString::to_string)
    }

    fn render_row(
        &self,
        ix: usize,
        saved: &SavedWorkspace,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let renaming = self
            .renaming
            .as_ref()
            .filter(|(name, _, _)| *name == saved.name)
            .map(|(_, input, _)| input.clone());
        if let Some(input) = renaming {
            return div()
                .flex()
                .flex_col()
                .gap(theme::S_4)
                .child(field(&input, window, cx))
                .children(self.rename_error.clone().map(error_text))
                .into_any_element();
        }
        let group = SharedString::from(format!("ws-row-{ix}"));
        let open = saved.name.clone();
        let rename = saved.name.clone();
        let delete = saved.name.clone();
        let panes: usize = saved.tabs.iter().map(|t| t.panes.len()).sum();
        let detail = if panes > saved.tabs.len() {
            tn!("workspaces.tabs_split", saved.tabs.len(), panes = panes).into_owned()
        } else {
            tn!("workspaces.tabs", saved.tabs.len()).into_owned()
        };
        let spoken = (saved.name.clone(), detail.clone());
        div()
            .id(("ws-row", ix))
            .a11y_lazy(gpui::Role::ListItem, move || {
                crate::a11y::item_label(&spoken.0, &[&spoken.1])
            })
            .group(group.clone())
            .h(theme::H_ROW)
            .px(theme::S_8)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_8)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(
                svg()
                    .path(Icon::Workspaces.path())
                    .size(theme::ICON)
                    .flex_none()
                    .text_color(theme::text_muted()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(saved.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(detail),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(theme::S_2)
                    .invisible()
                    .group_hover(group, gpui::Styled::visible)
                    .child(
                        row_button(("ws-rename", ix), Icon::Rename, t!("workspaces.rename"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.start_rename(rename.clone(), window, cx);
                            })),
                    )
                    .child(
                        row_button(("ws-delete", ix), Icon::Trash, t!("workspaces.delete"))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                Self::delete(&delete, cx);
                            })),
                    ),
            )
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.emit(PanelEvent::Open(open.clone()));
            }))
            .into_any_element()
    }
}

/// A text field in the panel (the same look as the network dialog's).
fn field(input: &Entity<TextInput>, window: &Window, cx: &App) -> impl IntoElement {
    let focused = input.focus_handle(cx).is_focused(window);
    div()
        .flex_1()
        .min_w_0()
        .h(theme::H_CTL)
        .px(theme::S_8)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(theme::line())
        })
        .child(input.clone())
}

fn error_text(error: SharedString) -> impl IntoElement {
    div()
        .text_size(theme::FS_12)
        .text_color(theme::danger())
        .child(error)
}

fn row_button(
    id: impl Into<gpui::ElementId>,
    icon: Icon,
    tip: impl std::fmt::Display,
) -> gpui::Stateful<gpui::Div> {
    let tip = tip.to_string();
    div()
        .id(id)
        .a11y_button(tip.clone())
        .size(theme::S_24)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(icon.path())
                .size(theme::ICON)
                .text_color(theme::text_subtle()),
        )
        .tooltip(crate::workspace::tooltip(tip))
}

impl Render for WorkspacesPanel {
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list: Vec<SavedWorkspace> = WorkspaceStore::global(cx)
            .read(cx)
            .workspaces()
            .list()
            .to_vec();
        let typed = clean_name(self.name.read(cx).text());
        let replaces = list
            .iter()
            .any(|w| w.name.to_lowercase() == typed.to_lowercase());
        let rows: Vec<gpui::AnyElement> = list
            .iter()
            .enumerate()
            .map(|(ix, saved)| self.render_row(ix, saved, window, cx))
            .collect();
        let empty = rows.is_empty();
        let panel = div()
            .id("workspaces-panel")
            .a11y(gpui::Role::Dialog, t!("workspaces.title"))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(440.))
            .p(theme::S_20)
            .flex()
            .flex_col()
            .gap(theme::S_12)
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .child(
                div()
                    .text_size(theme::FS_15)
                    .font_weight(theme::FW_STRONG)
                    .child(t!("workspaces.title").into_owned()),
            )
            .child(
                div()
                    .text_color(theme::text_muted())
                    .child(t!("workspaces.intro").into_owned()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .child(field(&self.name, window, cx))
                    .child(
                        div()
                            .id("workspaces-save")
                            .a11y_button(if replaces {
                                t!("workspaces.replace")
                            } else {
                                t!("workspaces.save")
                            })
                            .h(theme::H_CTL)
                            .px(theme::S_16)
                            .flex()
                            .flex_none()
                            .items_center()
                            .rounded(theme::R_8)
                            .cursor_pointer()
                            .bg(theme::signal())
                            .text_color(theme::on_signal())
                            .child(if replaces {
                                t!("workspaces.replace").into_owned()
                            } else {
                                t!("workspaces.save").into_owned()
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
                    ),
            )
            .children(self.error.clone().map(error_text))
            .child(
                div()
                    .pt(theme::S_4)
                    .text_size(theme::FS_12)
                    .font_weight(theme::FW_STRONG)
                    .text_color(theme::text_muted())
                    .child(t!("workspaces.saved").into_owned()),
            )
            .child(
                div()
                    .id("workspaces-list")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(theme::S_2)
                    .when(empty, |d| {
                        d.child(
                            div()
                                .h(theme::H_ROW)
                                .px(theme::S_8)
                                .flex()
                                .items_center()
                                .text_color(theme::text_subtle())
                                .child(t!("workspaces.none").into_owned()),
                        )
                    })
                    .children(rows),
            );
        // Backdrop: clicking outside closes.
        div()
            .id("workspaces-veil")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::veil())
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Closed)),
            )
            .child(panel)
            .with_animation("workspaces-appear", crate::motion::enter(), |el, t| {
                crate::motion::slide_in(el, t, 0., 0.)
            })
    }
}

impl Focusable for WorkspacesPanel {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.name.read(cx).focus_handle(cx)
    }
}

#[cfg(test)]
impl Workspace {
    /// Pinned flag of each tab, in order.
    pub(crate) fn pinned_for_test(&self) -> Vec<bool> {
        self.tabs.iter().map(|t| t.pinned).collect()
    }

    /// Synchronized browsing flag of each tab, in order.
    pub(crate) fn synced_for_test(&self) -> Vec<bool> {
        self.tabs.iter().map(|t| t.synced).collect()
    }

    /// Folders of every pane of every tab.
    pub(crate) fn all_paths_for_test(&self, cx: &App) -> Vec<Vec<PathBuf>> {
        self.tabs.iter().map(|t| tab_paths(t, cx)).collect()
    }

    pub(crate) fn move_tab_for_test(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.move_tab(from, to, cx);
    }
}
