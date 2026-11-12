//! Search box scope: **this folder** (live filter), **subfolders** (background walk),
//! **this PC** (index).
//!
//! This is the only place to find files; scopes differ only in reach (Ctrl+T is just "go to").
//! While the box has focus the search panel shows below it: where to search, the filters
//! (`explorer_filters.rs`) and, for "this PC", index status (building, live updates, rebuild,
//! background service).
//!
//! Keyboard: Tab (Shift+Tab) moves from the box into the panel's first (last) row; arrows move
//! inside it ([`PanelItem`], [`step`]), Tab and Shift+Tab go row by row and back to the box, Enter
//! or Space activates, Delete removes a filter, Esc closes a menu and then the panel. The scopes
//! are a radio group: Left and Right select. Typing goes on into the box.
//!
//! "This PC" reuses the subfolder search result list with an empty root and the full path in `rel`,
//! so open, open location, context menu and preview need no separate code.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use arke_core::format::format_count;
use arke_core::index::IndexHit;
use arke_core::index_query as iq;
use arke_core::index_query::Facet;
use arke_core::{Entry, EntryKind, SearchHit, SearchResults};
use gpui::{
    AnyElement, App, ClickEvent, Context, Focusable, KeyBinding, KeyDownEvent, MouseButton,
    MouseDownEvent, SharedString, Window, actions, anchored, deferred, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::Explorer;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::global_search::GlobalSearch;
use crate::theme::{self, UiFont as _};
use crate::workspace::TooltipBelowExt as _;

actions!(
    explorer,
    [
        PanelEnter,
        PanelEnterLast,
        PanelUp,
        PanelDown,
        PanelLeft,
        PanelRight,
        PanelHome,
        PanelEnd,
        PanelActivate,
        PanelClose,
        PanelNext,
        PanelPrev,
        PanelClear,
    ]
);

/// Key context of the search panel.
const PANEL_CONTEXT: &str = "SearchPanel";

pub(crate) fn bind_keys(cx: &mut App) {
    let filter = Some("FilterBar");
    let panel = Some(PANEL_CONTEXT);
    cx.bind_keys([
        // Tab in the filter box moves into the search panel (Shift+Tab: its last row).
        KeyBinding::new("tab", PanelEnter, filter),
        KeyBinding::new("shift-tab", PanelEnterLast, filter),
        KeyBinding::new("up", PanelUp, panel),
        KeyBinding::new("down", PanelDown, panel),
        KeyBinding::new("left", PanelLeft, panel),
        KeyBinding::new("right", PanelRight, panel),
        KeyBinding::new("home", PanelHome, panel),
        KeyBinding::new("end", PanelEnd, panel),
        KeyBinding::new("enter", PanelActivate, panel),
        KeyBinding::new("space", PanelActivate, panel),
        KeyBinding::new("escape", PanelClose, panel),
        KeyBinding::new("tab", PanelNext, panel),
        KeyBinding::new("shift-tab", PanelPrev, panel),
        KeyBinding::new("delete", PanelClear, panel),
        KeyBinding::new("backspace", PanelClear, panel),
    ]);
}

/// A keyboard stop in the search panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelItem {
    Scope(Scope),
    /// "This PC": live updates (background service).
    IndexService,
    /// "This PC": rebuild the index.
    IndexRebuild,
    Filter(Facet),
    ClearFilters,
    Contents,
}

/// A move inside the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Move {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Tab: the next row's first item.
    NextRow,
    /// Shift+Tab: the previous row's first item.
    PrevRow,
}

/// Where `mv` from `at` lands in `rows` (non-empty rows, top to bottom); `None` leaves the panel
/// for the box (up from the first row, Tab past the last, Shift+Tab before the first). Left and
/// right stop at the ends of a row; up and down keep the column as far as the row reaches.
pub(crate) fn step(rows: &[Vec<PanelItem>], at: PanelItem, mv: Move) -> Option<PanelItem> {
    let (r, c) = rows
        .iter()
        .enumerate()
        .find_map(|(r, row)| row.iter().position(|i| *i == at).map(|c| (r, c)))?;
    let row = &rows[r];
    let column = |row: &[PanelItem]| row.get(c.min(row.len().saturating_sub(1))).copied();
    match mv {
        Move::Left => row.get(c.saturating_sub(1)).copied(),
        Move::Right => row.get((c + 1).min(row.len() - 1)).copied(),
        Move::Home => row.first().copied(),
        Move::End => row.last().copied(),
        Move::Up => r.checked_sub(1).and_then(|p| column(&rows[p])),
        Move::Down => rows.get(r + 1).map_or(Some(at), |n| column(n)),
        Move::NextRow => rows.get(r + 1).and_then(|n| n.first().copied()),
        Move::PrevRow => r.checked_sub(1).and_then(|p| rows[p].first().copied()),
    }
}

/// Section heading in the panel, with an optional control on the right.
pub(super) fn panel_heading(
    label: impl Into<SharedString>,
    trailing: Option<AnyElement>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(theme::S_8)
        .min_h(theme::S_24)
        .px(theme::S_4)
        .child(
            div()
                .text_size(theme::FS_12)
                .font_weight(theme::FW_STRONG)
                .text_color(theme::text_muted())
                .child(label.into()),
        )
        .children(trailing)
}

/// Max "this PC" results (top matches by score).
const MAX_COMPUTER_HITS: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Here,
    Subfolders,
    Computer,
}

impl Scope {
    const ALL: [Self; 3] = [Self::Here, Self::Subfolders, Self::Computer];

    fn label(self) -> Cow<'static, str> {
        match self {
            Self::Here => t!("scope.here"),
            Self::Subfolders => t!("scope.subfolders"),
            Self::Computer => t!("scope.computer"),
        }
    }

    const fn icon(self) -> Icon {
        match self {
            Self::Here => Icon::Folder,
            Self::Subfolders => Icon::Subfolders,
            Self::Computer => Icon::Drive,
        }
    }
}

/// Convert index hits to search results: empty root, full path in `rel` (`root.join(rel)` is the
/// path).
fn to_results(hits: Vec<IndexHit>, generation: arke_core::service::Gen) -> SearchResults {
    let hits: Vec<Arc<SearchHit>> = hits
        .into_iter()
        .map(|hit| {
            let name = hit.path.file_name().map_or_else(
                || hit.path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let kind = if hit.is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            Arc::new(SearchHit {
                rel: Arc::from(hit.path.to_string_lossy().as_ref()),
                entry: Entry::new(name.as_str(), kind, hit.size, hit.modified, 0),
                snippet: None,
            })
        })
        .collect();
    SearchResults {
        generation,
        root: PathBuf::new(),
        matched: u32::try_from(hits.len()).unwrap_or(u32::MAX),
        hits: Arc::new(hits),
        scanned: 0,
        errors: 0,
        searching: false,
        truncated: false,
        contents: false,
        read: 0,
    }
}

impl Explorer {
    pub(crate) const fn scope(&self) -> Scope {
        match (self.search_mode, self.search_global) {
            (false, _) => Scope::Here,
            (true, false) => Scope::Subfolders,
            (true, true) => Scope::Computer,
        }
    }

    /// Change scope: keep the typed text and rerun for the new scope.
    pub(crate) fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if scope == self.scope() {
            // "Subfolders" again while searching one folder ("Search in this folder"): back to
            // the whole folder shown.
            if scope == Scope::Subfolders && self.search_in.take().is_some() {
                self.search_root = None;
                self.refresh_placeholders(&rust_i18n::locale(), cx);
                self.update_search(cx);
                cx.notify();
            }
            return;
        }
        let global = scope == Scope::Computer;
        if self.search_mode && global != self.search_global {
            // Subfolders <-> this PC: old results can't stay.
            self.search.stop();
            self.search_root = None;
            self.search_results = None;
            self.search_cursor = None;
            self.global_task = None;
        }
        self.search_global = global;
        if scope == Scope::Here {
            self.set_search_mode(false, cx);
        } else if self.search_mode {
            self.refresh_placeholders(&rust_i18n::locale(), cx);
            self.update_search(cx);
        } else {
            self.set_search_mode(true, cx);
        }
        cx.notify();
    }

    /// "Search in this folder" on a folder item: a subfolder search rooted at `dir`, with the
    /// cursor in the search box, like VS Code's "Find in Folder". The folder shown stays.
    pub(crate) fn search_in_folder(
        &mut self,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = arke_core::remote::canonical(dir);
        let here = self.current_path() == Some(dir.as_path());
        self.search_in = (!here).then_some(dir);
        // Walk again from the new root.
        self.search_root = None;
        if self.scope() == Scope::Subfolders {
            self.refresh_placeholders(&rust_i18n::locale(), cx);
            self.update_search(cx);
        } else {
            self.set_scope(Scope::Subfolders, cx);
        }
        self.panel_cursor = PanelItem::Scope(Scope::Subfolders);
        self.focus_filter_input(window, cx);
    }

    /// The folder "Search in this folder" searches, as its path below the folder shown
    /// (`src\ui`), or its name when it isn't below it.
    pub(crate) fn search_in_name(&self) -> Option<String> {
        let dir = self.search_in.as_ref()?;
        let below = self
            .current_path()
            .and_then(|here| dir.strip_prefix(here).ok())
            .filter(|rel| !rel.as_os_str().is_empty());
        Some(below.map_or_else(
            || {
                dir.file_name().map_or_else(
                    || dir.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )
            },
            |rel| rel.display().to_string(),
        ))
    }

    /// Keep focus in the box after a scope change (without selecting all, so typing doesn't
    /// overwrite).
    pub(crate) fn keep_filter_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.filter_input.read(cx).focus_handle(cx);
        if !focus.is_focused(window) {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn bind_scope_actions<E: InteractiveElement>(el: E, cx: &mut Context<Self>) -> E {
        el.on_action(cx.listener(|this, _: &PanelEnter, window, cx| {
            let at = PanelItem::Scope(this.scope());
            this.focus_panel(at, window, cx);
        }))
        .on_action(cx.listener(|this, _: &PanelEnterLast, window, cx| {
            let at = this
                .panel_rows(cx)
                .last()
                .and_then(|row| row.first().copied())
                .unwrap_or(PanelItem::Contents);
            this.focus_panel(at, window, cx);
        }))
    }

    /// Keyboard stops of the panel, row by row, as drawn.
    pub(crate) fn panel_rows(&self, cx: &App) -> Vec<Vec<PanelItem>> {
        let mut rows = vec![Scope::ALL.map(PanelItem::Scope).to_vec()];
        if self.scope() == Scope::Computer {
            let (service, rebuild) = index_control_state(cx);
            let controls: Vec<PanelItem> = [
                service.map(|_| PanelItem::IndexService),
                rebuild.then_some(PanelItem::IndexRebuild),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !controls.is_empty() {
                rows.push(controls);
            }
        }
        let mut filters: Vec<PanelItem> = super::filters::FACETS.map(PanelItem::Filter).to_vec();
        if self.any_filter() {
            filters.push(PanelItem::ClearFilters);
        }
        rows.push(filters);
        rows.push(vec![PanelItem::Contents]);
        rows
    }

    /// The keyboard position, moved to a neighbour when its item went away (Clear filters after
    /// clearing, index controls after leaving "this PC").
    pub(crate) fn panel_at(&self, rows: &[Vec<PanelItem>]) -> PanelItem {
        let at = self.panel_cursor;
        if rows.iter().any(|row| row.contains(&at)) {
            return at;
        }
        match at {
            PanelItem::ClearFilters => PanelItem::Filter(Facet::Date),
            _ => PanelItem::Scope(self.scope()),
        }
    }

    /// Moves keyboard focus into the panel at `at`, opening it.
    fn focus_panel(&mut self, at: PanelItem, window: &mut Window, cx: &mut Context<Self>) {
        self.panel_dismissed = false;
        self.scope_strip_open = true;
        self.filter_menu = None;
        self.panel_cursor = at;
        window.focus(&self.panel_focus, cx);
        cx.notify();
    }

    /// Back to the box, the panel staying open.
    fn panel_to_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_menu = None;
        self.keep_filter_focus(window, cx);
    }

    fn panel_move(&mut self, mv: Move, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(facet) = self.filter_menu {
            let count = self.menu_choices(facet).len().max(1);
            match mv {
                Move::Up => self.menu_cursor = (self.menu_cursor + count - 1) % count,
                Move::Down => self.menu_cursor = (self.menu_cursor + 1) % count,
                Move::Home => self.menu_cursor = 0,
                Move::End => self.menu_cursor = count - 1,
                // Like a menu bar: the neighbouring filter's menu opens instead.
                Move::Left | Move::Right => {
                    let rows = self.panel_rows(cx);
                    if let Some(PanelItem::Filter(next)) = step(&rows, PanelItem::Filter(facet), mv)
                        && next != facet
                    {
                        self.filter_menu = None;
                        self.toggle_filter_menu(next, window, cx);
                    }
                }
                Move::NextRow | Move::PrevRow => {
                    self.filter_menu = None;
                    self.panel_move(mv, window, cx);
                    return;
                }
            }
            cx.notify();
            return;
        }
        let rows = self.panel_rows(cx);
        let at = self.panel_at(&rows);
        let Some(next) = step(&rows, at, mv) else {
            self.panel_to_box(window, cx);
            return;
        };
        match (next, mv) {
            // Scopes are radio buttons: Left, Right, Home and End select.
            (PanelItem::Scope(scope), Move::Left | Move::Right | Move::Home | Move::End) => {
                self.panel_cursor = next;
                self.set_scope(scope, cx);
            }
            // Coming into the scope row from another row: the selected one.
            (PanelItem::Scope(_), _) => self.panel_cursor = PanelItem::Scope(self.scope()),
            _ => self.panel_cursor = next,
        }
        cx.notify();
    }

    fn panel_activate(&mut self, _: &PanelActivate, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(facet) = self.filter_menu {
            let ix = self.menu_cursor;
            self.pick_menu_choice(facet, ix, window, cx);
            return;
        }
        let rows = self.panel_rows(cx);
        match self.panel_at(&rows) {
            PanelItem::Scope(scope) => self.set_scope(scope, cx),
            PanelItem::Filter(facet) => self.toggle_filter_menu(facet, window, cx),
            PanelItem::ClearFilters => {
                self.clear_filters(window, cx);
                self.panel_cursor = PanelItem::Filter(Facet::Type);
            }
            PanelItem::Contents => {
                if self.contents_usable() {
                    self.toggle_contents(window, cx);
                }
            }
            PanelItem::IndexService => {
                let (service, _) = index_control_state(cx);
                if let (Some(global), Some((installed, false))) =
                    (GlobalSearch::global(cx), service)
                {
                    global.update(cx, |g, cx| g.set_service(!installed, cx));
                }
            }
            PanelItem::IndexRebuild => {
                if let Some(global) = GlobalSearch::global(cx) {
                    global.read(cx).rebuild();
                }
            }
        }
        cx.notify();
    }

    /// Esc: closes the open menu, else the panel (back to the box, which keeps its text).
    fn panel_close(&mut self, _: &PanelClose, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_menu.take().is_some() {
            cx.notify();
            return;
        }
        self.panel_dismissed = true;
        self.scope_strip_open = false;
        self.keep_filter_focus(window, cx);
    }

    /// Delete on a filter removes it.
    fn panel_clear(&mut self, _: &PanelClear, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_menu.is_none()
            && let PanelItem::Filter(facet) = self.panel_cursor
        {
            self.set_facet(facet, None, window, cx);
        }
    }

    /// A character typed while the panel has focus goes on into the box.
    fn panel_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys = &event.keystroke;
        if keys.modifiers.control || keys.modifiers.alt || keys.modifiers.platform {
            return;
        }
        let Some(text) = keys.key_char.as_deref().filter(|c| !c.trim().is_empty()) else {
            return;
        };
        let query = format!("{}{text}", self.query);
        let end = query.len();
        self.filter_input.update(cx, |input, cx| {
            input.set_text(query.clone(), cx);
            input.select_range(end..end, cx);
        });
        self.query = query;
        self.filter_menu = None;
        self.scope_strip_open = false;
        self.on_query_changed(cx);
        self.keep_filter_focus(window, cx);
        cx.stop_propagation();
    }

    fn bind_panel_actions<E: InteractiveElement>(el: E, cx: &mut Context<Self>) -> E {
        let mover = |mv: Move| {
            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                this.panel_move(mv, window, cx);
            }
        };
        let (up, down, left, right) = (
            mover(Move::Up),
            mover(Move::Down),
            mover(Move::Left),
            mover(Move::Right),
        );
        let (home, end, next, prev) = (
            mover(Move::Home),
            mover(Move::End),
            mover(Move::NextRow),
            mover(Move::PrevRow),
        );
        el.on_action(cx.listener(move |this, _: &PanelUp, w, cx| up(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelDown, w, cx| down(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelLeft, w, cx| left(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelRight, w, cx| right(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelHome, w, cx| home(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelEnd, w, cx| end(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelNext, w, cx| next(this, w, cx)))
            .on_action(cx.listener(move |this, _: &PanelPrev, w, cx| prev(this, w, cx)))
            .on_action(cx.listener(Self::panel_activate))
            .on_action(cx.listener(Self::panel_close))
            .on_action(cx.listener(Self::panel_clear))
            .on_key_down(cx.listener(Self::panel_key_down))
    }

    /// Query "this PC" in the background and swap in results when they arrive (dropped if the text
    /// or scope changed meanwhile). Old results stay until then to avoid flicker.
    pub(crate) fn run_global(&mut self, cx: &mut Context<Self>) {
        self.watch_global(cx);
        self.global_serial += 1;
        let serial = self.global_serial;
        // The index has names only: `content:` is left out (the result area says so).
        let query = iq::without_content(&self.query).trim().to_string();
        let search = GlobalSearch::global(cx).and_then(|g| {
            let g = g.read(cx);
            self.global_count = g.total_count();
            if query.is_empty() || !g.enabled() {
                None
            } else {
                g.search(query.clone(), MAX_COMPUTER_HITS, cx)
            }
        });
        let Some(search) = search else {
            self.global_task = None;
            self.search_results = None;
            self.search_cursor = None;
            cx.notify();
            return;
        };
        self.global_task = Some(cx.spawn(async move |this, cx| {
            let hits = search.await;
            let _ = this.update(cx, |this, cx| {
                if this.global_serial != serial || this.scope() != Scope::Computer {
                    return;
                }
                let results = this.without_gone(to_results(hits, this.search.current_gen()));
                let len = results.hits.len();
                this.search_cursor = match this.search_cursor {
                    _ if len == 0 => None,
                    Some(ix) => Some(ix.min(len - 1)),
                    None => Some(0),
                };
                this.search_results = Some(results);
                cx.notify();
            });
        }));
    }

    /// Subscribe to index status on first use of "this PC": requery when built or refreshed, redraw
    /// on other changes (build progress).
    fn watch_global(&mut self, cx: &mut Context<Self>) {
        if self.global_watch.is_some() {
            return;
        }
        let Some(global) = GlobalSearch::global(cx) else {
            return;
        };
        self.global_watch = Some(cx.observe(&global, |this: &mut Self, global, cx| {
            if this.scope() != Scope::Computer {
                return;
            }
            let count = global.read(cx).total_count();
            if count != this.global_count {
                this.run_global(cx);
            }
            cx.notify();
        }));
    }

    fn computer_enabled(cx: &App) -> bool {
        GlobalSearch::global(cx).is_some_and(|g| g.read(cx).enabled())
    }

    /// Left side of the context bar (this PC): match count and index status.
    pub(super) fn computer_info(&self, cx: &App) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(r) = &self.search_results {
            // The index returns only the top matches, so the total is unknown.
            parts.push(if r.hits.len() >= MAX_COMPUTER_HITS {
                t!("scope.top_hits", count = format_count(MAX_COMPUTER_HITS)).into_owned()
            } else {
                tn!("info.matched", r.hits.len()).into_owned()
            });
        }
        if let Some(status) = index_status_text(cx) {
            parts.push(status);
        }
        parts
    }

    /// Placeholder for "this PC" when there's nothing to list yet (off, no text, still indexing);
    /// `None` once there are results.
    pub(super) fn computer_placeholder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !Self::computer_enabled(cx) {
            return Some(Self::computer_off_state(cx));
        }
        if iq::has_content_term(&self.query) {
            return Some(self.no_match_state(
                t!("scope.contents_unavailable").into_owned(),
                Some(t!("scope.contents_unavailable_hint").into_owned()),
                cx,
            ));
        }
        if self.query.trim().is_empty() {
            return Some(Self::empty_state(
                svg()
                    .path(Icon::Drive.path())
                    .size(theme::S_32)
                    .text_color(theme::text_subtle())
                    .into_any_element(),
                t!("scope.computer_prompt").into_owned(),
                Some(t!("search.syntax").into_owned()),
            ));
        }
        let building = GlobalSearch::global(cx).is_some_and(|g| g.read(cx).status.count.is_none());
        if building && self.search_results.is_none() {
            return Some(Self::empty_state(
                svg()
                    .path(Icon::Drive.path())
                    .size(theme::S_32)
                    .text_color(theme::text_subtle())
                    .into_any_element(),
                index_status_text(cx).unwrap_or_default(),
                Some(t!("scope.building_hint").into_owned()),
            ));
        }
        None
    }

    fn computer_off_state(cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::S_8)
            .child(
                svg()
                    .path(Icon::Drive.path())
                    .size(theme::S_32)
                    .text_color(theme::text_subtle())
                    .opacity(0.7),
            )
            .child(
                div()
                    .text_size(theme::FS_15)
                    .text_color(theme::text_muted())
                    .child(t!("scope.computer_off").to_string()),
            )
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(t!("search.index_enable_tip").to_string()),
            )
            .child(
                div()
                    .id("scope-enable-index")
                    .a11y_button(t!("scope.enable"))
                    .mt(theme::S_8)
                    .h(theme::H_CTL)
                    .px(theme::S_16)
                    .flex()
                    .items_center()
                    .rounded(theme::R_8)
                    .cursor_pointer()
                    .bg(theme::signal())
                    .text_color(theme::on_signal())
                    .hover(|s| s.opacity(0.9))
                    .child(t!("scope.enable").to_string())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        GlobalSearch::enable(cx);
                        this.run_global(cx);
                    })),
            )
            .into_any_element()
    }

    /// No match in this folder: offer buttons to widen the scope directly.
    pub(crate) fn widen_buttons(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let targets: &[(Scope, &'static str, Cow<'static, str>)] = match self.scope() {
            Scope::Here => &[
                (
                    Scope::Subfolders,
                    "widen-subfolders",
                    t!("scope.widen_subfolders"),
                ),
                (
                    Scope::Computer,
                    "widen-computer",
                    t!("scope.widen_computer"),
                ),
            ],
            Scope::Subfolders => &[(
                Scope::Computer,
                "widen-computer",
                t!("scope.widen_computer"),
            )],
            // Content search needs the files themselves: offer the scopes that read them.
            Scope::Computer if iq::has_content_term(&self.query) => &[
                (
                    Scope::Subfolders,
                    "narrow-subfolders",
                    t!("scope.widen_subfolders"),
                ),
                (Scope::Here, "narrow-here", t!("scope.search_here")),
            ],
            Scope::Computer => return None,
        };
        let mut row = div().mt(theme::S_8).flex().gap(theme::S_8);
        for (target, id, label) in targets {
            let target = *target;
            row = row.child(
                div()
                    .id(*id)
                    .a11y_lazy(gpui::Role::Button, || label.to_string())
                    .h(theme::H_CTL)
                    .px(theme::S_12)
                    .flex()
                    .items_center()
                    .gap(theme::S_6)
                    .rounded(theme::R_8)
                    .border_1()
                    .border_color(theme::line_strong())
                    .cursor_pointer()
                    .text_size(theme::FS_14)
                    .text_color(theme::text())
                    .hover(|s| s.bg(theme::wash_strong()))
                    .child(
                        svg()
                            .path(target.icon().path())
                            .size(theme::ICON_SM)
                            .text_color(theme::text_muted()),
                    )
                    .child(label.to_string())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_scope(target, cx);
                        this.keep_filter_focus(window, cx);
                    })),
            );
        }
        Some(row.into_any_element())
    }

    /// Small chip on the right of the box: shows the scope when it isn't "this folder", even
    /// without focus. Also shown while typing with the strip collapsed; click to reopen the strip.
    pub(super) fn render_scope_chip(
        &self,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let scope = self.scope();
        let here = scope == Scope::Here;
        if here && (!focused || self.query.is_empty()) {
            return None;
        }
        Some(
            div()
                .id("scope-chip")
                .a11y_button(t!("scope.chip_tip"))
                .aria_description(scope.label().to_string())
                .h(theme::S_20)
                .px(theme::S_6)
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::S_4)
                .rounded(theme::R_4)
                .cursor_pointer()
                .when(here, |d| {
                    d.text_color(theme::text_muted())
                        .hover(|s| s.bg(theme::wash_strong()))
                })
                .when(!here, |d| {
                    d.bg(theme::signal_wash())
                        .text_color(theme::text())
                        .hover(|s| s.bg(theme::wash_strong()))
                })
                .text_size(theme::FS_12)
                .child(
                    svg()
                        .path(scope.icon().path())
                        .size(theme::ICON_SM)
                        .text_color(if here {
                            theme::text_subtle()
                        } else {
                            theme::signal()
                        }),
                )
                // Searching one folder: its name says where.
                .when(!self.pane.split, |d| {
                    d.child(
                        self.search_in_name()
                            .filter(|_| scope == Scope::Subfolders)
                            .unwrap_or_else(|| scope.label().to_string()),
                    )
                })
                // No tooltip while the strip it opens is showing: it would cover the strip.
                .when(
                    !(focused && (self.query.is_empty() || self.scope_strip_open)),
                    |d| d.tooltip_below(t!("scope.chip_tip")),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    // Don't let the box's click select all text again.
                    cx.stop_propagation();
                    this.scope_strip_open = !this.scope_strip_open;
                    this.panel_dismissed = false;
                    this.keep_filter_focus(window, cx);
                }))
                .into_any_element(),
        )
    }

    /// One segment of the scope row; `at`: the keyboard is on it.
    fn scope_segment(scope: Scope, on: bool, at: bool, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(match scope {
                Scope::Here => "scope-here",
                Scope::Subfolders => "scope-subfolders",
                Scope::Computer => "scope-computer",
            })
            .a11y_lazy(gpui::Role::RadioButton, || scope.label().to_string())
            .aria_toggled(crate::a11y::toggled(on))
            .when(at, gpui::StatefulInteractiveElement::aria_active_descendant)
            .flex_1()
            .min_w_0()
            .h(theme::H_CTL_MD)
            .px(theme::S_8)
            .flex()
            .items_center()
            .justify_center()
            .gap(theme::S_6)
            .rounded(theme::R_4)
            .border_1()
            .border_color(if at {
                theme::signal_line()
            } else {
                theme::clear()
            })
            .cursor_pointer()
            .text_size(theme::FS_12)
            .whitespace_nowrap()
            // Same segmented style as the view switcher: raised pill on a recessed track.
            .when(on, |d| {
                d.bg(theme::segment_thumb())
                    .shadow(theme::elev_thumb())
                    .text_color(theme::text())
                    .font_weight(theme::FW_STRONG)
            })
            .when(!on, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
            })
            .child(
                svg()
                    .path(scope.icon().path())
                    .flex_none()
                    .size(theme::ICON_SM)
                    .text_color(if on {
                        theme::signal()
                    } else {
                        theme::text_muted()
                    }),
            )
            .child(div().min_w_0().truncate().child(scope.label().to_string()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.set_scope(scope, cx);
                this.panel_cursor = PanelItem::Scope(scope);
                if !this.panel_focus.is_focused(window) {
                    this.keep_filter_focus(window, cx);
                }
            }))
            .into_any_element()
    }

    /// The search panel below the focused box (floats over the content): where to search, what
    /// that scope does (index status for "this PC"), the filters, and a keyboard hint.
    pub(super) fn render_scope_strip(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.scope();
        let keyboard = self.panel_focus.is_focused(window);
        let rows = self.panel_rows(cx);
        // The keyboard position, while the panel has focus and no menu is open.
        let cursor = (keyboard && self.filter_menu.is_none()).then(|| self.panel_at(&rows));
        let segments = Scope::ALL.map(|scope| {
            Self::scope_segment(
                scope,
                scope == current,
                cursor == Some(PanelItem::Scope(scope)),
                cx,
            )
        });
        let status = div()
            .min_h(theme::S_24)
            .px(theme::S_4)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .text_size(theme::FS_12)
            .text_color(theme::text_subtle())
            .child(div().flex_1().min_w_0().child(match current {
                Scope::Here => t!("scope.here_hint").to_string(),
                Scope::Subfolders => self.search_in_name().map_or_else(
                    || t!("scope.subfolders_hint").to_string(),
                    |name| t!("scope.search_in_hint", name = name).to_string(),
                ),
                Scope::Computer => {
                    index_status_text(cx).unwrap_or_else(|| t!("scope.computer_off").into_owned())
                }
            }))
            .when(current == Scope::Computer, |d| {
                d.children(index_controls(cursor, cx))
            });
        let footer = div()
            .pt(theme::S_8)
            .px(theme::S_4)
            .flex()
            .justify_end()
            .border_t_1()
            .border_color(theme::line())
            .text_size(theme::FS_12)
            .text_color(theme::text_subtle())
            .child(if keyboard {
                t!("scope.keys_hint").into_owned()
            } else {
                t!("scope.tab_hint").into_owned()
            });
        let panel = div()
            .id("search-panel")
            .key_context(PANEL_CONTEXT)
            .track_focus(&self.panel_focus)
            .a11y(gpui::Role::Group, t!("search.panel"))
            .relative()
            .w(theme::SEARCH_PANEL_W)
            .p(theme::S_12)
            .flex()
            .flex_col()
            .gap(theme::S_8)
            // No border: GPUI draws it over the children, through an open filter menu.
            .rounded(theme::R_8)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .ui_font()
            .occlude()
            // Clicks on the panel must not start a marquee in the list below or move focus.
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
            })
            .child(super::filters::panel_measure(&self.panel_geometry))
            .child(panel_heading(t!("scope.section"), None))
            .child(
                div()
                    .id("scope-group")
                    .a11y(gpui::Role::RadioGroup, t!("scope.section"))
                    .p(theme::S_2)
                    .flex()
                    .gap(theme::S_2)
                    .rounded(theme::R_8)
                    .bg(theme::segment_track())
                    .children(segments),
            )
            .child(status)
            .child(self.render_filter_section(cursor, cx))
            .child(footer)
            // Last, so it covers what is below its button.
            .children(self.render_filter_menu(keyboard, cx));
        let panel = Self::bind_panel_actions(panel, cx);
        float_under_box(crate::motion::appear(
            panel,
            "search-panel-appear",
            0.,
            -crate::motion::NUDGE,
        ))
    }
}

/// The search panel's place: under the box, right-aligned, kept inside the window.
fn float_under_box(panel: AnyElement) -> AnyElement {
    div()
        .absolute()
        .top(theme::H_CTL + theme::S_4)
        .right_0()
        .child(
            deferred(
                anchored()
                    .anchor(gpui::Anchor::TopRight)
                    .snap_to_window_with_margin(theme::S_8)
                    .child(panel),
            )
            .with_priority(2),
        )
        .into_any_element()
}

#[cfg(test)]
impl Explorer {
    /// The folder the subfolder search walks (`None` before the first walk), and whether the
    /// search box has focus.
    pub(crate) fn search_walk_for_test(
        &self,
        window: &Window,
        cx: &App,
    ) -> (Option<PathBuf>, bool) {
        let focused = self
            .filter_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        (
            self.search_root.as_ref().map(|(root, _)| root.clone()),
            focused,
        )
    }

    /// Whether the panel has focus, the keyboard position, and the open menu with its cursor.
    pub(crate) fn panel_state_for_test(
        &self,
        window: &Window,
    ) -> (bool, PanelItem, Option<(Facet, usize)>) {
        (
            self.panel_focus.is_focused(window),
            self.panel_cursor,
            self.filter_menu.map(|f| (f, self.menu_cursor)),
        )
    }

    pub(crate) const fn panel_dismissed_for_test(&self) -> bool {
        self.panel_dismissed
    }
}

/// Index status ("this PC N items · live updates…"); `None` when off.
fn index_status_text(cx: &App) -> Option<String> {
    let global = GlobalSearch::global(cx)?;
    let global = global.read(cx);
    if !global.enabled() {
        return None;
    }
    let status = global.status;
    let text = match (status.count, status.scanning) {
        (None, scanned) => t!(
            "search.index_building",
            count = format_count(scanned.unwrap_or(0))
        ),
        (Some(n), Some(_)) => t!("search.index_updating", count = format_count(n)),
        (Some(n), None) if status.live => t!("search.index_live", count = format_count(n)),
        (Some(n), None) => t!("search.index_ready", count = format_count(n)),
    };
    let mut text = text.into_owned();
    if status.waiting {
        text.push_str(&t!("search.index_waiting"));
    }
    // Network locations: total count; unreachable ones are named (results are from last time).
    let count: usize = global.net.iter().filter_map(|n| n.status.count).sum();
    // No network results yet (none added or never opened): don't write "0 network items".
    if count > 0 {
        text.push_str(" · ");
        text.push_str(&tn!("search.index_network_total", count));
        let offline = global.net.iter().filter(|n| n.offline).count();
        if offline > 0 {
            text.push_str(&t!("search.index_network_offline", count = offline));
        }
    }
    Some(text)
}

/// "This PC" index controls: the background service (`Some((installed, busy))` once known) and
/// whether a rebuild can be started (only once built: changes made while off need it).
fn index_control_state(cx: &App) -> (Option<(bool, bool)>, bool) {
    let Some(global) = GlobalSearch::global(cx) else {
        return (None, false);
    };
    let g = global.read(cx);
    if !g.enabled() {
        return (None, false);
    }
    let status = g.status;
    (
        g.service_installed
            .map(|installed| (installed, g.service_busy)),
        status.scanning.is_none() && status.count.is_some(),
    )
}

/// Controls for "this PC" in the panel: live updates (background service), rebuild index.
fn index_controls(cursor: Option<PanelItem>, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let Some(global) = GlobalSearch::global(cx) else {
        return Vec::new();
    };
    let (service, rebuild) = index_control_state(cx);
    let status = global.read(cx).status;
    let mut out = Vec::new();
    if let Some((installed, busy)) = service {
        let tip = if busy {
            t!("search.service_busy")
        } else if !installed {
            t!("search.service_install")
        } else if status.live {
            t!("search.service_live")
        } else if status.scanning.is_some() || status.count.is_none() {
            // Rebuilding and not yet following the journal; not a disconnect.
            t!("search.service_building")
        } else {
            t!("search.service_offline")
        };
        let color = if installed && status.live {
            theme::signal()
        } else {
            theme::text_muted()
        };
        let global = global.clone();
        out.push(
            icon_button(
                "scope-index-service",
                Icon::Bolt,
                color,
                tip,
                busy,
                cursor == Some(PanelItem::IndexService),
            )
            .on_click(move |_: &ClickEvent, _, cx| {
                if !busy {
                    global.update(cx, |g, cx| g.set_service(!installed, cx));
                }
            })
            .into_any_element(),
        );
    }
    if rebuild {
        let tip = if status.live {
            t!("search.index_rebuild_live")
        } else {
            t!("search.index_rebuild")
        };
        out.push(
            icon_button(
                "scope-index-rebuild",
                Icon::Refresh,
                theme::text_muted(),
                tip,
                false,
                cursor == Some(PanelItem::IndexRebuild),
            )
            .on_click(move |_: &ClickEvent, _, cx| global.read(cx).rebuild())
            .into_any_element(),
        );
    }
    out
}

fn icon_button(
    id: &'static str,
    icon: Icon,
    color: gpui::Rgba,
    tip: Cow<'static, str>,
    busy: bool,
    at: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_button(tip.clone())
        .a11y_disabled(busy)
        .when(at, gpui::StatefulInteractiveElement::aria_active_descendant)
        .flex_none()
        .size(theme::S_24)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .border_1()
        .border_color(if at {
            theme::signal_line()
        } else {
            theme::clear()
        })
        .when(!busy, |d| {
            d.cursor_pointer().hover(|s| s.bg(theme::wash_strong()))
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme::ICON_SM)
                .text_color(color),
        )
        .tooltip(crate::workspace::tooltip(tip))
}

#[cfg(test)]
mod tests {
    use super::{Move, PanelItem, Scope, step};
    use arke_core::index_query::Facet;

    #[test]
    fn panel_moves_by_row_and_column_and_leaves_at_the_edges() {
        use PanelItem::{ClearFilters, Contents, Filter};
        let rows = vec![
            Scope::ALL.map(PanelItem::Scope).to_vec(),
            vec![
                Filter(Facet::Type),
                Filter(Facet::Size),
                Filter(Facet::Date),
                ClearFilters,
            ],
            vec![Contents],
        ];
        let here = PanelItem::Scope(Scope::Here);
        let computer = PanelItem::Scope(Scope::Computer);
        assert_eq!(
            step(&rows, here, Move::Right),
            Some(PanelItem::Scope(Scope::Subfolders))
        );
        assert_eq!(
            step(&rows, here, Move::Left),
            Some(here),
            "stops at the start"
        );
        assert_eq!(
            step(&rows, computer, Move::Right),
            Some(computer),
            "stops at the end"
        );
        assert_eq!(step(&rows, here, Move::End), Some(computer));
        // Down keeps the column; a shorter row takes its last item.
        assert_eq!(step(&rows, computer, Move::Down), Some(Filter(Facet::Date)));
        assert_eq!(step(&rows, ClearFilters, Move::Down), Some(Contents));
        assert_eq!(step(&rows, Contents, Move::Down), Some(Contents));
        assert_eq!(step(&rows, Contents, Move::Up), Some(Filter(Facet::Type)));
        // Up from the first row, Tab past the last and Shift+Tab before the first: the box.
        assert_eq!(step(&rows, here, Move::Up), None);
        assert_eq!(step(&rows, Contents, Move::NextRow), None);
        assert_eq!(step(&rows, here, Move::PrevRow), None);
        assert_eq!(
            step(&rows, computer, Move::NextRow),
            Some(Filter(Facet::Type))
        );
        assert_eq!(
            step(&rows, Contents, Move::PrevRow),
            Some(Filter(Facet::Type))
        );
        // Gone from the rows (Clear filters after clearing): no move.
        assert_eq!(step(&rows[..1], ClearFilters, Move::Left), None);
    }
}
