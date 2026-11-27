//! The keyboard in the sidebar, and F6 between the parts of the window (#322).
//!
//! F6 (Shift+F6 back) moves the keyboard from the file list to the sidebar, the path bar, the
//! pane row's buttons (`explorer_toolbar.rs`), the search box and back to the list, as in File
//! Explorer. In the sidebar the arrow keys move a cursor over its rows (favorites, the Recycle
//! Bin, section headings, the folder tree): Right expands or steps into a folder, Left collapses
//! it or goes up to its parent, Enter or Space opens it (the keyboard then goes to the list, as
//! after a click) and Escape goes back to the list. The row under the cursor is reported to
//! screen readers as the focused item.
//!
//! The sidebar takes the keyboard only through F6: it tracks its focus handle only while that
//! handle is focused, so a click in the sidebar leaves the keyboard where it was (in the list), as
//! before.
//!
//! Rows say what they do through [`NavRow`]; the same actions answer a screen reader asking to
//! click, expand or collapse a row (which also works for a row scrolled out of sight). Rows are
//! recorded only while the keyboard is in the sidebar or a screen reader is attached, so the
//! sidebar costs nothing more otherwise. Their order is where they were drawn last frame (top to
//! bottom, then left to right), which also walks the grid of favorite tiles.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    Bounds, Context, Div, Pixels, SharedString, Stateful, Window, actions, canvas, div, prelude::*,
};

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::theme;

actions!(
    sidebar_keys,
    [
        /// F6: the keyboard goes to the next part of the window (list, sidebar, path bar, the pane
        /// row's buttons, search).
        FocusNextRegion,
        /// Shift+F6: the previous part.
        FocusPrevRegion,
        /// Tab: the keyboard goes to the next stop (see [`Stop`]).
        FocusNextStop,
        /// Shift+Tab: the previous stop.
        FocusPrevStop,
        SidebarUp,
        SidebarDown,
        SidebarFirst,
        SidebarLast,
        /// Right: expand, or into the expanded folder.
        SidebarIn,
        /// Left: collapse, or up to the parent.
        SidebarOut,
        /// Enter or Space: open the row.
        SidebarOpen,
        /// Escape: back to the list.
        SidebarLeave
    ]
);

pub(crate) fn bind_keys(cx: &mut gpui::App) {
    let sidebar = Some("Sidebar");
    cx.bind_keys([
        gpui::KeyBinding::new("up", SidebarUp, sidebar),
        gpui::KeyBinding::new("down", SidebarDown, sidebar),
        gpui::KeyBinding::new("home", SidebarFirst, sidebar),
        gpui::KeyBinding::new("end", SidebarLast, sidebar),
        gpui::KeyBinding::new("right", SidebarIn, sidebar),
        gpui::KeyBinding::new("left", SidebarOut, sidebar),
        gpui::KeyBinding::new("enter", SidebarOpen, sidebar),
        gpui::KeyBinding::new("space", SidebarOpen, sidebar),
        gpui::KeyBinding::new("escape", SidebarLeave, sidebar),
    ]);
    // Tab walks the stops from the places that are one (the list, the tree, the Home page, the
    // pane row). Anywhere else Tab keeps its own meaning: menus, dialogs, a rename box, the
    // settings pages. The path bar and the search box use Tab themselves (completing a path,
    // entering the search panel) and hand it on when they have nothing to do with it, so only
    // Shift+Tab is bound there.
    for context in ["FileList", "Sidebar", "HomePage", "RecycleBin", "Toolbar"] {
        cx.bind_keys([
            gpui::KeyBinding::new("tab", FocusNextStop, Some(context)),
            gpui::KeyBinding::new("shift-tab", FocusPrevStop, Some(context)),
        ]);
    }
    for context in ["PathBar", "FilterBar"] {
        cx.bind_keys([gpui::KeyBinding::new(
            "shift-tab",
            FocusPrevStop,
            Some(context),
        )]);
    }
}

/// Opens a row (a click on it).
pub(super) type Open = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;
/// Expands (`true`) or collapses a row.
pub(super) type Expand = Rc<dyn Fn(&mut Workspace, bool, &mut Context<Workspace>)>;

/// Where a row was drawn, set while painting.
type DrawnAt = Rc<Cell<Option<Bounds<Pixels>>>>;

/// What a sidebar row does, for the keyboard and screen readers.
#[derive(Clone)]
pub(super) struct NavRow {
    /// Nesting: 0 for section headings, more for what is under them (Left goes up to the row
    /// before it with less).
    pub(super) depth: usize,
    /// Expanded or not, for rows that can be.
    pub(super) expanded: Option<bool>,
    pub(super) open: Open,
    pub(super) expand: Option<Expand>,
}

/// The sidebar's keyboard state (in `rail::SidebarUi`).
#[derive(Default)]
pub(super) struct SidebarKeys {
    /// The keyboard is in the sidebar (as of this frame).
    pub(super) focused: Cell<bool>,
    /// Rows drawn this frame, by key, with where they were drawn (set while painting).
    rows: RefCell<HashMap<SharedString, (NavRow, DrawnAt)>>,
    /// The row under the cursor.
    pub(super) cursor: RefCell<Option<SharedString>>,
    /// The current folder's row, where the cursor starts.
    here: RefCell<Option<SharedString>>,
    /// When the current folder has no row (its parents are collapsed), the row of its nearest
    /// drawn ancestor, with how deep that folder is: the deepest one wins.
    nearest: RefCell<Option<(usize, SharedString)>>,
    /// Scroll the cursor's row into view once it has been drawn.
    reveal: Cell<bool>,
}

impl SidebarKeys {
    /// A new frame of the sidebar: forgets the rows (they are recorded again as they are drawn).
    /// Returns where the cursor's row was drawn last frame, when it is to be scrolled into view.
    pub(super) fn start_frame(&self) -> Option<Bounds<Pixels>> {
        let reveal = self
            .reveal
            .replace(false)
            .then(|| {
                let cursor = self.cursor.borrow().clone()?;
                self.rows.borrow().get(&cursor)?.1.get()
            })
            .flatten();
        self.rows.borrow_mut().clear();
        self.here.borrow_mut().take();
        self.nearest.borrow_mut().take();
        reveal
    }

    /// Rows in the order they were drawn: top to bottom, then left to right. Rows not drawn yet
    /// come last, in no particular order.
    fn ordered(&self) -> Vec<(SharedString, NavRow)> {
        let rows = self.rows.borrow();
        let mut out: Vec<_> = rows
            .iter()
            .map(|(key, (row, at))| (key.clone(), row.clone(), at.get()))
            .collect();
        out.sort_by(|a, b| {
            let key = |b: Option<Bounds<Pixels>>| {
                b.map_or((f32::MAX, f32::MAX), |b| {
                    (f32::from(b.top()).round(), f32::from(b.left()))
                })
            };
            key(a.2)
                .partial_cmp(&key(b.2))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.into_iter().map(|(k, r, _)| (k, r)).collect()
    }

    /// Notes that the row `key`, drawn this frame, is a folder above the current one, `depth`
    /// components deep. The deepest such row is where F6 lands when the current folder has no row.
    pub(super) fn note_ancestor(&self, depth: usize, key: &SharedString) {
        let mut nearest = self.nearest.borrow_mut();
        if nearest.as_ref().is_none_or(|(d, _)| depth > *d) {
            *nearest = Some((depth, key.clone()));
        }
    }

    /// Where the cursor starts: the current folder's row, else its nearest drawn ancestor's.
    fn start_row(&self) -> Option<SharedString> {
        self.here
            .borrow()
            .clone()
            .or_else(|| self.nearest.borrow().as_ref().map(|(_, k)| k.clone()))
    }

    fn row(&self, key: &SharedString) -> Option<NavRow> {
        self.rows.borrow().get(key).map(|(row, _)| row.clone())
    }
}

impl Workspace {
    /// Gives `el` (a sidebar row with an id and a role) what the keyboard and screen readers need:
    /// the actions `row` makes, the cursor's outline and focus report. `here`: it is the current
    /// folder's row. `row` only runs while the keyboard is in the sidebar or a screen reader is
    /// attached.
    pub(super) fn nav_row(
        &self,
        el: Stateful<Div>,
        key: SharedString,
        here: bool,
        row: impl FnOnce() -> NavRow,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let keys = &self.sidebar_ui.keys;
        if here {
            *keys.here.borrow_mut() = Some(key.clone());
        }
        let focused = keys.focused.get();
        if !focused && !crate::a11y::active() {
            return el;
        }
        let row = row();
        let ws = cx.entity().downgrade();
        let open = Rc::clone(&row.open);
        let mut el = el.relative().a11y_on_click(move |window, cx| {
            let _ = ws.update(cx, |this, cx| open(this, window, cx));
        });
        if let Some(expand) = row.expand.clone() {
            let ws = cx.entity().downgrade();
            el = el.a11y_on_expand(move |want, _, cx| {
                let _ = ws.update(cx, |this, cx| expand(this, want, cx));
            });
        }
        let at = Rc::new(Cell::new(None));
        keys.rows
            .borrow_mut()
            .insert(key.clone(), (row, Rc::clone(&at)));
        if !focused {
            return el;
        }
        let cursor = keys.cursor.borrow().as_ref() == Some(&key);
        el.child(
            canvas(move |bounds, _, _| at.set(Some(bounds)), |_, (), _, _| {})
                .absolute()
                .size_full(),
        )
        .when(cursor, |d| {
            d.aria_active_descendant().child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(theme::r_8())
                    .border_1()
                    .border_color(theme::signal_line()),
            )
        })
    }

    /// Scrolls the places so the cursor's row (drawn at `row` last frame) is in view.
    pub(super) fn reveal_sidebar_row(&self, row: Bounds<Pixels>) {
        let scroll = &self.sidebar_ui.places_scroll;
        let view = scroll.bounds();
        let offset = scroll.offset();
        let shift = if row.top() < view.top() {
            view.top() - row.top()
        } else if row.bottom() > view.bottom() {
            view.bottom() - row.bottom()
        } else {
            return;
        };
        let y = (offset.y + shift)
            .min(Pixels::ZERO)
            .max(-scroll.max_offset().y);
        scroll.set_offset(gpui::point(offset.x, y));
    }

    /// Whether the sidebar can take the keyboard (shown, not collapsed to its icons).
    fn sidebar_takes_keys(cx: &mut gpui::App) -> bool {
        !crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .sidebar_collapsed
    }

    /// The part of the window the keyboard is in.
    fn region(&self, window: &Window, cx: &gpui::App) -> Region {
        if self.sidebar_focus.is_focused(window) {
            return Region::Sidebar;
        }
        let view = self.active_view().read(cx);
        if view.toolbar_held(window) {
            return Region::Toolbar;
        }
        match view.typing_in(window, cx) {
            Some(crate::explorer::TypingIn::PathBar) => Region::PathBar,
            Some(crate::explorer::TypingIn::Search) => Region::Search,
            None => Region::List,
        }
    }

    pub(super) fn focus_next_region(
        &mut self,
        _: &FocusNextRegion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_region(true, window, cx);
    }

    pub(super) fn focus_prev_region(
        &mut self,
        _: &FocusPrevRegion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_region(false, window, cx);
    }

    pub(super) fn focus_next_stop(
        &mut self,
        _: &FocusNextStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.step_stop(true, window, cx) {
            cx.propagate();
        }
    }

    pub(super) fn focus_prev_stop(
        &mut self,
        _: &FocusPrevStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.step_stop(false, window, cx) {
            cx.propagate();
        }
    }

    /// The stop the keyboard is on, or `None` when it is somewhere that keeps Tab: a menu, a
    /// dialog, a rename box, a page, Quick Look.
    fn stop(&self, window: &Window, cx: &gpui::App) -> Option<Stop> {
        if self.sidebar_focus.is_focused(window) {
            return Some(Stop::Sidebar);
        }
        let view = self.active_view().read(cx);
        if view.toolbar_held(window) {
            return Some(Stop::Toolbar);
        }
        match view.typing_in(window, cx) {
            Some(crate::explorer::TypingIn::PathBar) => return Some(Stop::PathBar),
            Some(crate::explorer::TypingIn::Search) => return Some(Stop::Search),
            None if view.panel_held(window) => return Some(Stop::Search),
            None => {}
        }
        if let Some(page) = self.page_focus(cx) {
            return ((self.home_open() || self.recycle_open()) && page.is_focused(window))
                .then_some(Stop::List(0));
        }
        self.tabs[self.active]
            .panes
            .iter()
            .position(|pane| {
                let view = pane.view.read(cx);
                gpui::Focusable::focus_handle(view, cx).is_focused(window) && !view.list_keeps_tab()
            })
            .map(Stop::List)
    }

    /// Every stop, in the order Tab goes (see [`Stop`]).
    fn stops(&self, cx: &mut gpui::App) -> Vec<Stop> {
        let page = self.page_focus(cx).is_some();
        let lists = if page {
            1
        } else {
            self.tabs[self.active].panes.len()
        };
        let mut ring: Vec<Stop> = (0..lists).map(Stop::List).collect();
        if Self::sidebar_takes_keys(cx) {
            ring.push(Stop::Sidebar);
        }
        // A page replaces the panes, the path bar, the pane row and the search box with it: they
        // aren't drawn, so the keyboard can't be left on them.
        if !page {
            ring.extend([Stop::PathBar, Stop::Toolbar, Stop::Search]);
        }
        ring
    }

    /// Tab (`forward`) or Shift+Tab: one stop on. `false` when the keyboard isn't on a stop, so
    /// the key is left to whatever else wants it.
    pub(super) fn step_stop(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(now) = self.stop(window, cx) else {
            return false;
        };
        // Each button of the pane row is a stop of its own.
        if now == Stop::Toolbar
            && self
                .active_view()
                .update(cx, |e, cx| e.toolbar_walk(forward, cx))
        {
            return true;
        }
        // The Home page's sections are stops of their own.
        if now == Stop::List(0) && self.home_open() && self.home_tab_section(forward, cx) {
            return true;
        }
        let ring = self.stops(cx);
        let at = ring.iter().position(|s| *s == now).unwrap_or(0);
        let next = if forward {
            ring[(at + 1) % ring.len()]
        } else {
            ring[(at + ring.len() - 1) % ring.len()]
        };
        if now == Stop::PathBar {
            self.active_view()
                .update(cx, |e, cx| e.cancel_path_edit(window, cx));
        }
        self.focus_stop(next, forward, window, cx);
        true
    }

    fn focus_stop(
        &mut self,
        stop: Stop,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match stop {
            Stop::List(pane) => {
                if self.home_open() {
                    self.home_enter(forward, cx);
                }
                let tab = self.active;
                if pane == self.tabs[tab].active_pane {
                    self.focus_active(window, cx);
                } else {
                    self.activate_pane(tab, pane, window, cx);
                }
            }
            // Backwards into the pane row: its last button.
            Stop::Toolbar if !forward => {
                self.active_view()
                    .update(cx, |e, cx| e.focus_toolbar_last(window, cx));
            }
            Stop::Toolbar => self.focus_region(Region::Toolbar, window, cx),
            Stop::Sidebar => self.focus_region(Region::Sidebar, window, cx),
            Stop::PathBar => self.focus_region(Region::PathBar, window, cx),
            Stop::Search => self.focus_region(Region::Search, window, cx),
        }
    }

    fn step_region(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let order = Region::ORDER;
        let now = self.region(window, cx);
        let at = order.iter().position(|r| *r == now).unwrap_or(0);
        let sidebar = Self::sidebar_takes_keys(cx);
        let mut next = at;
        let next = loop {
            next = if forward {
                (next + 1) % order.len()
            } else {
                (next + order.len() - 1) % order.len()
            };
            if order[next] != Region::Sidebar || sidebar {
                break order[next];
            }
        };
        if now == Region::PathBar {
            self.active_view()
                .update(cx, |e, cx| e.cancel_path_edit(window, cx));
        }
        self.focus_region(next, window, cx);
    }

    fn focus_region(&mut self, region: Region, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.active_view().clone();
        match region {
            Region::Sidebar => {
                let keys = &self.sidebar_ui.keys;
                let known = keys
                    .cursor
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| keys.rows.borrow().contains_key(c));
                if !known {
                    let start = keys.start_row();
                    *keys.cursor.borrow_mut() = start;
                }
                keys.reveal.set(true);
                window.focus(&self.sidebar_focus, cx);
                cx.notify();
            }
            Region::PathBar => view.update(cx, |e, cx| {
                e.focus_path_bar(&crate::explorer::FocusPathBar, window, cx);
            }),
            Region::Toolbar => view.update(cx, |e, cx| e.focus_toolbar(window, cx)),
            Region::Search => view.update(cx, |e, cx| {
                e.focus_filter(&crate::explorer::FocusFilter, window, cx);
            }),
            Region::List => self.focus_active(window, cx),
        }
    }

    /// Moves the cursor to the row `pick` chooses from the rows in order and the cursor's place.
    fn move_sidebar_cursor(
        &mut self,
        pick: impl FnOnce(&[(SharedString, NavRow)], Option<usize>) -> Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let keys = &self.sidebar_ui.keys;
        let rows = keys.ordered();
        let at = keys
            .cursor
            .borrow()
            .as_ref()
            .and_then(|c| rows.iter().position(|(k, _)| k == c));
        if let Some(to) = pick(&rows, at).and_then(|ix| rows.get(ix)) {
            *keys.cursor.borrow_mut() = Some(to.0.clone());
            keys.reveal.set(true);
            cx.notify();
        }
    }

    pub(super) fn sidebar_up(&mut self, _: &SidebarUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_sidebar_cursor(|_, at| Some(at.map_or(0, |ix| ix.saturating_sub(1))), cx);
    }

    pub(super) fn sidebar_down(&mut self, _: &SidebarDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_sidebar_cursor(
            |rows, at| Some(at.map_or(0, |ix| (ix + 1).min(rows.len().saturating_sub(1)))),
            cx,
        );
    }

    pub(super) fn sidebar_first(
        &mut self,
        _: &SidebarFirst,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_sidebar_cursor(|_, _| Some(0), cx);
    }

    pub(super) fn sidebar_last(&mut self, _: &SidebarLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_sidebar_cursor(|rows, _| rows.len().checked_sub(1), cx);
    }

    /// The row under the cursor, if it is still drawn.
    fn sidebar_cursor_row(&self) -> Option<NavRow> {
        let keys = &self.sidebar_ui.keys;
        let cursor = keys.cursor.borrow().clone()?;
        keys.row(&cursor)
    }

    pub(super) fn sidebar_in(&mut self, _: &SidebarIn, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.sidebar_cursor_row() else {
            return;
        };
        match (row.expanded, row.expand) {
            (Some(false), Some(expand)) => {
                expand(self, true, cx);
                cx.notify();
            }
            // Expanded: into its first row.
            (Some(true), _) => {
                self.move_sidebar_cursor(
                    |rows, at| {
                        let ix = at? + 1;
                        (rows.get(ix)?.1.depth > row.depth).then_some(ix)
                    },
                    cx,
                );
            }
            _ => {}
        }
    }

    pub(super) fn sidebar_out(&mut self, _: &SidebarOut, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.sidebar_cursor_row() else {
            return;
        };
        if let (Some(true), Some(expand)) = (row.expanded, row.expand) {
            expand(self, false, cx);
            cx.notify();
            return;
        }
        // Up to the row it is under.
        self.move_sidebar_cursor(
            |rows, at| (0..at?).rev().find(|&ix| rows[ix].1.depth < row.depth),
            cx,
        );
    }

    pub(super) fn sidebar_open(
        &mut self,
        _: &SidebarOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.sidebar_cursor_row() {
            (row.open)(self, window, cx);
        }
    }

    pub(super) fn sidebar_leave(
        &mut self,
        _: &SidebarLeave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_active(window, cx);
        cx.notify();
    }
}

/// Where Tab stops, in the order it goes (Shift+Tab goes the other way round).
///
/// The order is F6's, each part opened up into its controls:
/// 1. the list of each pane, left to right (the other pane of a split view is the next stop
///    after this one; on the Home page the page's content);
/// 2. the sidebar tree, when it is shown (one stop; the arrows move inside it);
/// 3. the path bar;
/// 4. each button of the pane row, one by one;
/// 5. the search box, then, with Tab, its panel (scope, filters, contents), whose last row hands
///    Tab on to the first list.
///
/// On a page that replaces the panes (Home, the Recycle Bin), the stops are the page and the
/// sidebar only; each section of the Home page (favorites, drives, ...) is a stop of its own.
///
/// File Explorer's own order jumps around the window; this one follows F6, so the two keys
/// agree about where things are. The tab strip and the preview are not stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// The list of this pane (index in the tab).
    List(usize),
    Sidebar,
    PathBar,
    /// The pane row: the button the row is on is the stop.
    Toolbar,
    /// The search box, or its panel.
    Search,
}

/// The parts of the window F6 goes through, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    List,
    Sidebar,
    PathBar,
    /// The pane row's buttons, beside the path bar.
    Toolbar,
    Search,
}

impl Region {
    const ORDER: [Self; 5] = [
        Self::List,
        Self::Sidebar,
        Self::PathBar,
        Self::Toolbar,
        Self::Search,
    ];
}

#[cfg(test)]
impl Workspace {
    /// Key of the row under the sidebar's cursor.
    pub(crate) fn sidebar_cursor_for_test(&self) -> Option<String> {
        self.sidebar_ui
            .keys
            .cursor
            .borrow()
            .as_ref()
            .map(ToString::to_string)
    }

    /// Where the keyboard is, as Tab sees it ("none": on no stop, where Tab isn't ours), with the
    /// pane row's button.
    pub(crate) fn stop_for_test(&self, window: &Window, cx: &gpui::App) -> String {
        match self.stop(window, cx) {
            None => "none".to_owned(),
            Some(Stop::List(pane)) => format!("list{pane}"),
            Some(Stop::Sidebar) => "sidebar".to_owned(),
            Some(Stop::PathBar) => "path".to_owned(),
            Some(Stop::Toolbar) => {
                let at = self.active_view().read(cx).toolbar_at_for_test();
                format!("toolbar:{}", at.unwrap_or("?"))
            }
            Some(Stop::Search) => "search".to_owned(),
        }
    }

    /// Where the keyboard is, as F6 sees it.
    pub(crate) fn region_for_test(&self, window: &Window, cx: &gpui::App) -> &'static str {
        match self.region(window, cx) {
            Region::List => "list",
            Region::Sidebar => "sidebar",
            Region::PathBar => "path",
            Region::Toolbar => "toolbar",
            Region::Search => "search",
        }
    }
}
