//! Back and forward across tabs (`core::timeline`). Each pane keeps its own folder history; the
//! timeline records the order of navigations (in any pane) and switches between tabs,
//! so back first undoes what happened in the current tab and, past its first folder, returns to
//! the tab you came from. "Within the current tab" in settings turns it off: back and forward
//! then only ever move the pane they act on.
//!
//! Alt+Left/Right, the mouse buttons and the panes' toolbar buttons all come through here when
//! the timeline is on (caught before the panes see them). So does a two-finger swipe on a
//! touchpad (#70).

use std::time::{Duration, Instant};

use arke_core::NavKind;
use arke_core::timeline::{Step, Timeline};
use gpui::{App, Context, EntityId, ScrollDelta, ScrollEvent, Window};

use super::Workspace;
use crate::explorer::{GoBack, GoForward};
use crate::settings::{NavScope, SettingsStore};

#[derive(Default)]
pub(super) struct NavState {
    timeline: Timeline<EntityId>,
    /// The active pane when last checked, to notice switches.
    last_active: Option<EntityId>,
    /// Back or forward is switching panes itself: not a new step.
    replaying: bool,
    swipe: Swipe,
}

/// A two-finger swipe across the touchpad, as Windows reports it: horizontal wheel messages in
/// fractions of a notch, with no start or end. Messages closer than [`SWIPE_GAP`] are one swipe,
/// inertia included, and one swipe moves one step.
#[derive(Default)]
struct Swipe {
    /// Lines moved sideways so far, positive toward back.
    lines: f32,
    last: Option<Instant>,
    done: bool,
}

/// A row of an open menu, held by a test.
#[cfg(test)]
pub(crate) struct MenuRow(super::sidebar::SidebarAction);

/// A pause longer than this starts a new swipe.
const SWIPE_GAP: Duration = Duration::from_millis(250);
/// How far a swipe goes before it counts, in wheel lines (a notch is three by default).
const SWIPE_LINES: f32 = 6.;

impl Workspace {
    /// Whether back and forward follow the timeline across tabs (the default).
    pub(super) fn across_tabs(cx: &mut App) -> bool {
        SettingsStore::global(cx).read(cx).settings.nav_scope == NavScope::AcrossTabs
    }

    fn pane_of(&self, id: EntityId) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(t, tab)| {
            tab.panes
                .iter()
                .position(|p| p.view.entity_id() == id)
                .map(|p| (t, p))
        })
    }

    /// Records a change of the active pane as a step. Called wherever the active pane may change.
    pub(super) fn note_active(&mut self, cx: &mut Context<Self>) {
        let now = self.active_view().entity_id();
        let before = self.nav.last_active.replace(now);
        // Only a change of tab is a step: moving between the sides of a split view is not.
        if let Some(before) = before
            && before != now
            && !self.nav.replaying
            && self
                .pane_of(before)
                .is_some_and(|(tab, _)| tab != self.active)
        {
            self.nav.timeline.record_switch(before, now);
            self.sync_nav_buttons(cx);
        }
    }

    /// A navigation that `view`'s history recorded.
    pub(super) fn note_navigation(&mut self, view: EntityId, cx: &mut Context<Self>) {
        self.nav.timeline.record_nav(view);
        self.sync_nav_buttons(cx);
    }

    /// Back (or forward) one step on the timeline. Steps of closed panes, and navigations the
    /// pane can no longer undo, are skipped.
    pub(super) fn step_timeline(
        &mut self,
        back: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        loop {
            let step = if back {
                self.nav.timeline.back()
            } else {
                self.nav.timeline.forward()
            };
            let Some(step) = step else {
                break;
            };
            match step {
                Step::Nav(id) => {
                    let Some((t, p)) = self.pane_of(id) else {
                        continue;
                    };
                    let view = self.tabs[t].panes[p].view.clone();
                    if !Self::pane_can(&view, back, cx) {
                        continue;
                    }
                    self.replay_activate(t, p, window, cx);
                    view.update(cx, |e, cx| {
                        if back {
                            e.go_back(&GoBack, window, cx);
                        } else {
                            e.go_forward(&GoForward, window, cx);
                        }
                    });
                    break;
                }
                Step::Switch { from, to } => {
                    let Some((t, p)) = self.pane_of(if back { from } else { to }) else {
                        continue;
                    };
                    self.replay_activate(t, p, window, cx);
                    break;
                }
            }
        }
        self.sync_nav_buttons(cx);
    }

    /// A sideways scroll anywhere in the window: a two-finger swipe on a precision touchpad goes
    /// back (fingers moving right) or forward, like a browser, once the list can't scroll that
    /// way itself. A mouse wheel tilted sideways sends whole notches and is left alone.
    pub(super) fn on_swipe(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ScrollDelta::Lines(delta) = event.delta else {
            return;
        };
        if delta.y != 0. || delta.x == 0. || !SettingsStore::global(cx).read(cx).settings.swipe_nav
        {
            return;
        }
        let now = Instant::now();
        let ongoing = self
            .nav
            .swipe
            .last
            .is_some_and(|last| now.duration_since(last) < SWIPE_GAP);
        if !ongoing {
            if delta.x.fract() == 0. {
                return;
            }
            self.nav.swipe = Swipe::default();
        }
        let swipe = &mut self.nav.swipe;
        swipe.last = Some(now);
        let back = delta.x > 0.;
        let list_scrolls =
            self.page_focus(cx).is_none() && self.active_view().read(cx).can_scroll_sideways(back);
        let swipe = &mut self.nav.swipe;
        if list_scrolls || (swipe.lines != 0. && (swipe.lines > 0.) != back) {
            swipe.lines = 0.;
        }
        if list_scrolls {
            return;
        }
        swipe.lines += delta.x;
        if swipe.done || swipe.lines.abs() < SWIPE_LINES {
            return;
        }
        swipe.done = true;
        self.go_in_history(back, window, cx);
    }

    /// Back or forward from outside a pane (mouse buttons, swipes): closes Home opened over a
    /// folder, else follows the timeline or moves the active pane.
    pub(crate) fn go_in_history(
        &mut self,
        back: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if back && self.back_closes_home(window, cx) {
            return;
        }
        if Self::across_tabs(cx) {
            self.step_timeline(back, window, cx);
            return;
        }
        let view = self.active_view().clone();
        view.update(cx, |e, cx| {
            if back {
                e.go_back(&GoBack, window, cx);
            } else {
                e.go_forward(&GoForward, window, cx);
            }
        });
    }

    /// Right click on a back or forward button outside the panes: lists the active pane's
    /// folders.
    pub(super) fn history_menu_listener(
        back: bool,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
            let view = this.active_view().clone();
            this.open_history_menu(&view, back, e.position, cx);
        })
    }

    /// Opens the back (or forward) history menu for `view`, as a right click on the button does.
    #[cfg(test)]
    pub(crate) fn open_history_menu_for_test(
        &mut self,
        view: &gpui::Entity<crate::explorer::Explorer>,
        back: bool,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_history_menu(view, back, position, cx);
    }

    /// The menu of a back or forward button: the folders of `view`'s history, nearest first.
    /// With back and forward across tabs, when the button would first go to another tab, that
    /// comes first, so the top row is always what the button does. Nothing opens when there is
    /// nothing to list.
    pub(super) fn open_history_menu(
        &mut self,
        view: &gpui::Entity<crate::explorer::Explorer>,
        back: bool,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let pane = view.entity_id();
        let mut items: Vec<_> = self.timeline_entry(pane, back, cx).into_iter().collect();
        items.extend(
            view.read(cx)
                .history_menu(back)
                .into_iter()
                .enumerate()
                .map(|(ix, path)| {
                    (
                        super::sidebar::display_name(&path).into(),
                        super::sidebar::SidebarAction::HistoryJump {
                            pane,
                            back,
                            steps: ix + 1,
                            path,
                        },
                    )
                }),
        );
        if items.is_empty() {
            self.sidebar_menu = None;
            cx.notify();
        } else {
            self.open_sidebar_menu(position, items, cx);
        }
    }

    /// The step back (or forward) across tabs takes first, when it isn't one in `pane`'s own
    /// history: a menu row for the folder it goes to.
    fn timeline_entry(
        &self,
        pane: EntityId,
        back: bool,
        cx: &mut App,
    ) -> Option<(gpui::SharedString, super::sidebar::SidebarAction)> {
        if !Self::across_tabs(cx) {
            return None;
        }
        let step = self.next_step(back, cx)?;
        let (to, path) = match step {
            Step::Nav(id) if id == pane => return None,
            Step::Nav(id) => {
                let (t, p) = self.pane_of(id)?;
                let view = self.tabs[t].panes[p].view.read(cx);
                (id, view.history_menu(back).into_iter().next()?)
            }
            Step::Switch { from, to } => {
                let id = if back { from } else { to };
                let (t, p) = self.pane_of(id)?;
                let view = self.tabs[t].panes[p].view.read(cx);
                (id, view.display_path()?.to_path_buf())
            }
        };
        Some((
            super::sidebar::display_name(&path).into(),
            super::sidebar::SidebarAction::TimelineStep { back, to, path },
        ))
    }

    /// The step back (or forward) on the timeline would take, skipping the ones it skips.
    fn next_step(&self, back: bool, cx: &App) -> Option<Step<EntityId>> {
        let usable = |step: &&Step<EntityId>| match **step {
            Step::Nav(id) => self
                .pane_of(id)
                .is_some_and(|(t, p)| Self::pane_can(&self.tabs[t].panes[p].view, back, cx)),
            Step::Switch { from, to } => self.pane_of(if back { from } else { to }).is_some(),
        };
        if back {
            self.nav.timeline.behind().find(usable).copied()
        } else {
            self.nav.timeline.ahead().find(usable).copied()
        }
    }

    /// The top row of a history menu that goes to another tab: steps back (or forward) once,
    /// if that still goes to the pane the row named.
    pub(super) fn timeline_step_to(
        &mut self,
        back: bool,
        to: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let goes_to = match self.next_step(back, cx) {
            Some(Step::Nav(id)) => Some(id),
            Some(Step::Switch { from, to }) => Some(if back { from } else { to }),
            None => None,
        };
        if goes_to == Some(to) {
            self.step_timeline(back, window, cx);
        }
    }

    /// Chose a folder in a history menu: goes there in one step. The history may have changed
    /// since the menu opened (a folder finished loading, Back with the keyboard): the folder is
    /// looked up again, and nothing happens if it is no longer on that side. The timeline's steps
    /// for the folders passed over are used up once the folder opens ([`Self::history_landed`]),
    /// so Back and Forward stay in line.
    pub(super) fn jump_in_history(
        &mut self,
        pane: EntityId,
        back: bool,
        steps: usize,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((t, p)) = self.pane_of(pane) else {
            return;
        };
        let view = self.tabs[t].panes[p].view.clone();
        let Some(steps) = view.read(cx).history_steps_to(back, steps, path) else {
            return;
        };
        if !view.update(cx, |e, cx| e.go_to_history(back, steps, cx)) {
            return;
        }
        if (t, p) != (self.active, self.tabs[t].active_pane) {
            self.replay_activate(t, p, window, cx);
        }
        self.sync_nav_buttons(cx);
    }

    /// A pane arrived somewhere. After a jump from a history menu, the timeline's steps for the
    /// folders passed over are used up; a jump that never arrived (the folder is gone) leaves
    /// the timeline as it was. Its history menu, if open, would list stale folders: it closes.
    pub(super) fn history_landed(&mut self, pane: EntityId, kind: NavKind, cx: &mut Context<Self>) {
        if let NavKind::Jump { back, steps } = kind {
            self.nav.timeline.shift(pane, steps, back);
            self.sync_nav_buttons(cx);
        }
        if kind != NavKind::Refresh && self.menu_lists_history_of(pane) {
            self.sidebar_menu = None;
            cx.notify();
        }
    }

    /// Labels of the open back/forward menu, if one is open.
    #[cfg(test)]
    pub(crate) fn history_menu_labels_for_test(&self) -> Option<Vec<String>> {
        self.sidebar_menu_labels_for_test()
    }

    /// The second line of each row of the open menu.
    #[cfg(test)]
    pub(crate) fn menu_details_for_test(&self) -> Option<Vec<String>> {
        self.sidebar_menu_details_for_test()
    }

    /// Row `ix` of the open menu, to choose later (as a click that comes after other events).
    #[cfg(test)]
    pub(crate) fn menu_row_for_test(&self, ix: usize) -> Option<MenuRow> {
        self.sidebar_menu_action_for_test(ix).map(MenuRow)
    }

    /// Chooses a row taken earlier with [`Self::menu_row_for_test`].
    #[cfg(test)]
    pub(crate) fn run_menu_row_for_test(
        &mut self,
        row: MenuRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_sidebar_action(row.0, window, cx);
    }

    /// Chooses row `ix` of the open menu.
    #[cfg(test)]
    pub(crate) fn pick_menu_row_for_test(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(action) = self.sidebar_menu_action_for_test(ix) {
            self.run_sidebar_action(action, window, cx);
        }
    }

    fn pane_can(view: &gpui::Entity<crate::explorer::Explorer>, back: bool, cx: &App) -> bool {
        view.read(cx).can_go(back)
    }

    fn replay_activate(&mut self, t: usize, p: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.nav.replaying = true;
        self.activate_pane(t, p, window, cx);
        self.nav.replaying = false;
    }

    /// Whether back or forward has a step it can take.
    fn can_step(&self, back: bool, cx: &App) -> bool {
        self.next_step(back, cx).is_some()
    }

    /// The panes' back and forward buttons show what the timeline can do when it is on, and the
    /// pane's own history when it is off.
    pub(super) fn sync_nav_buttons(&self, cx: &mut Context<Self>) {
        let global =
            Self::across_tabs(cx).then(|| (self.can_step(true, cx), self.can_step(false, cx)));
        let views: Vec<_> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter().map(|p| p.view.clone()))
            .collect();
        for view in views {
            view.update(cx, |e, cx| {
                if e.global_nav != global {
                    e.global_nav = global;
                    cx.notify();
                }
            });
        }
    }
}
