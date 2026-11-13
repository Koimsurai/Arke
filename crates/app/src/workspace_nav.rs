//! Back and forward across tabs (`core::timeline`). Each pane keeps its own folder history; the
//! timeline records the order of navigations (in any pane) and switches between tabs,
//! so back first undoes what happened in the current tab and, past its first folder, returns to
//! the tab you came from. "Within the current tab" in settings turns it off: back and forward
//! then only ever move the pane they act on.
//!
//! Alt+Left/Right, the mouse buttons and the panes' toolbar buttons all come through here when
//! the timeline is on (caught before the panes see them).

use arke_core::timeline::{Step, Timeline};
use gpui::{App, Context, EntityId, Window};

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
}

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

    /// A pane's back or forward toolbar button: the timeline when it is on, else that pane.
    pub(super) fn history_button(
        &mut self,
        view: &gpui::Entity<crate::explorer::Explorer>,
        back: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if Self::across_tabs(cx) {
            self.step_timeline(back, window, cx);
        } else {
            view.update(cx, |e, cx| {
                if back {
                    e.go_back(&GoBack, window, cx);
                } else {
                    e.go_forward(&GoForward, window, cx);
                }
            });
        }
    }

    fn pane_can(view: &gpui::Entity<crate::explorer::Explorer>, back: bool, cx: &App) -> bool {
        let history = &view.read(cx).history;
        if back {
            history.can_go_back()
        } else {
            history.can_go_forward()
        }
    }

    fn replay_activate(&mut self, t: usize, p: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.nav.replaying = true;
        self.activate_pane(t, p, window, cx);
        self.nav.replaying = false;
    }

    /// Whether back or forward has a step it can take.
    fn can_step(&self, back: bool, cx: &App) -> bool {
        let usable = |step: &Step<EntityId>| match *step {
            Step::Nav(id) => self
                .pane_of(id)
                .is_some_and(|(t, p)| Self::pane_can(&self.tabs[t].panes[p].view, back, cx)),
            Step::Switch { from, to } => self.pane_of(if back { from } else { to }).is_some(),
        };
        if back {
            self.nav.timeline.behind().any(usable)
        } else {
            self.nav.timeline.ahead().any(usable)
        }
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
