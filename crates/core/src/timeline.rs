//! Back and forward across tabs, in the order things happened.
//!
//! Each pane keeps its own folder history (`history.rs`). The timeline records the order of the
//! steps over all of them: a navigation in a pane, or a switch from one pane (tab or split side)
//! to another. Back undoes the latest step: inside the current pane first; past its first folder,
//! the switch that brought you there, which returns to the pane you came from as you left it.
//!
//! `P` identifies a pane. Steps whose pane has since closed are for the caller to skip.

/// One step on the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step<P> {
    /// A navigation in this pane that its history recorded.
    Nav(P),
    /// The active pane changed.
    Switch {
        /// Pane that was active.
        from: P,
        /// Pane that became active.
        to: P,
    },
}

/// Steps kept: older ones fall off, like the per-pane history.
const MAX_STEPS: usize = 512;

/// Steps in order, and how many of them are currently done (the rest can be redone with
/// forward).
#[derive(Debug)]
pub struct Timeline<P> {
    steps: Vec<Step<P>>,
    done: usize,
}

impl<P> Default for Timeline<P> {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            done: 0,
        }
    }
}

impl<P: Copy + Eq> Timeline<P> {
    /// A navigation the pane's history recorded. Drops the steps that could have been redone.
    pub fn record_nav(&mut self, pane: P) {
        self.push(Step::Nav(pane));
    }

    /// The active pane changed from `from` to `to`.
    pub fn record_switch(&mut self, from: P, to: P) {
        if from != to {
            self.push(Step::Switch { from, to });
        }
    }

    fn push(&mut self, step: Step<P>) {
        self.steps.truncate(self.done);
        self.steps.push(step);
        if self.steps.len() > MAX_STEPS {
            self.steps.remove(0);
        }
        self.done = self.steps.len();
    }

    /// The step back undoes, which is then no longer done.
    pub fn back(&mut self) -> Option<Step<P>> {
        let ix = self.done.checked_sub(1)?;
        self.done = ix;
        self.steps.get(ix).copied()
    }

    /// The step forward redoes, which is then done again.
    pub fn forward(&mut self) -> Option<Step<P>> {
        let step = self.steps.get(self.done).copied()?;
        self.done += 1;
        Some(step)
    }

    /// Steps back would undo, latest first.
    pub fn behind(&self) -> impl Iterator<Item = &Step<P>> {
        self.steps[..self.done].iter().rev()
    }

    /// Steps forward would redo, next first.
    pub fn ahead(&self) -> impl Iterator<Item = &Step<P>> {
        self.steps[self.done..].iter()
    }

    /// Forgets every step that involves `pane` (closed). A switch to or from it can't be undone
    /// anymore.
    pub fn forget(&mut self, pane: P) {
        let involves = |s: &Step<P>| match *s {
            Step::Nav(p) => p == pane,
            Step::Switch { from, to } => from == pane || to == pane,
        };
        let removed_done = self.steps[..self.done]
            .iter()
            .filter(|s| involves(s))
            .count();
        self.steps.retain(|s| !involves(s));
        self.done -= removed_done;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_undoes_the_latest_step_and_forward_redoes_it() {
        let mut t = Timeline::default();
        t.record_nav('a');
        t.record_switch('a', 'b');
        t.record_nav('b');
        assert_eq!(t.back(), Some(Step::Nav('b')));
        assert_eq!(t.back(), Some(Step::Switch { from: 'a', to: 'b' }));
        assert_eq!(t.back(), Some(Step::Nav('a')));
        assert_eq!(t.back(), None);
        assert_eq!(t.forward(), Some(Step::Nav('a')));
        assert_eq!(t.forward(), Some(Step::Switch { from: 'a', to: 'b' }));
        assert_eq!(t.forward(), Some(Step::Nav('b')));
        assert_eq!(t.forward(), None);
    }

    #[test]
    fn a_new_step_after_going_back_drops_what_could_be_redone() {
        let mut t = Timeline::default();
        t.record_nav('a');
        t.record_switch('a', 'b');
        t.back();
        t.record_nav('a');
        assert_eq!(t.ahead().count(), 0);
        assert_eq!(
            t.behind().copied().collect::<Vec<_>>(),
            [Step::Nav('a'), Step::Nav('a')]
        );
    }

    #[test]
    fn switching_to_the_same_pane_is_no_step() {
        let mut t = Timeline::default();
        t.record_switch('a', 'a');
        assert_eq!(t.back(), None);
    }

    #[test]
    fn a_closed_pane_is_forgotten_with_its_switches() {
        let mut t = Timeline::default();
        t.record_nav('a');
        t.record_switch('a', 'b');
        t.record_nav('b');
        t.record_switch('b', 'a');
        t.record_nav('a');
        t.back();
        t.forget('b');
        assert_eq!(t.behind().copied().collect::<Vec<_>>(), [Step::Nav('a')]);
        assert_eq!(t.ahead().copied().collect::<Vec<_>>(), [Step::Nav('a')]);
    }

    #[test]
    fn old_steps_fall_off() {
        let mut t = Timeline::default();
        for _ in 0..MAX_STEPS + 10 {
            t.record_nav(1);
        }
        assert_eq!(t.behind().count(), MAX_STEPS);
    }
}
