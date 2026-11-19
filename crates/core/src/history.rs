//! Back/forward navigation history.

use std::path::{Path, PathBuf};

/// Kind of navigation. Only written to history once the background listing confirms
/// it (first batch received).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKind {
    /// Regular navigation (double-click, path bar, sidebar, up).
    New,
    /// Go to the top of the back stack.
    Back,
    /// Go to the top of the forward stack.
    Forward,
    /// Reload the current directory without touching history.
    Refresh,
    /// Go `steps` entries down the back (`back`) or forward stack in one step, as if Back or
    /// Forward had been used that many times.
    Jump {
        /// Which stack the target is on.
        back: bool,
        /// How far down it is (1 is the top, what Back or Forward would open).
        steps: usize,
    },
}

/// Back and forward stacks of visited directories (back keeps at most 256).
#[derive(Clone, Default, Debug)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

const MAX_HISTORY: usize = 256;

impl History {
    /// Whether there is anything to go back to.
    pub const fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether there is anything to go forward to.
    pub const fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// How many steps back there are: the position of the current folder in the history.
    pub const fn depth(&self) -> usize {
        self.back.len()
    }

    /// Directory a back navigation would open.
    pub fn back_target(&self) -> Option<&Path> {
        self.back.last().map(PathBuf::as_path)
    }

    /// Directory a forward navigation would open.
    pub fn forward_target(&self) -> Option<&Path> {
        self.forward.last().map(PathBuf::as_path)
    }

    /// The `steps`-th entry down the back stack (1 is the top).
    pub fn back_at(&self, steps: usize) -> Option<&Path> {
        let ix = self.back.len().checked_sub(steps)?;
        self.back.get(ix).map(PathBuf::as_path)
    }

    /// The `steps`-th entry down the forward stack (1 is the top).
    pub fn forward_at(&self, steps: usize) -> Option<&Path> {
        let ix = self.forward.len().checked_sub(steps)?;
        self.forward.get(ix).map(PathBuf::as_path)
    }

    /// Back entries, the one Back opens first.
    pub fn back_entries(&self) -> impl Iterator<Item = &Path> {
        self.back.iter().rev().map(PathBuf::as_path)
    }

    /// Forward entries, the one Forward opens first.
    pub fn forward_entries(&self) -> impl Iterator<Item = &Path> {
        self.forward.iter().rev().map(PathBuf::as_path)
    }

    /// Called after a navigation succeeds. `from` is the previously shown directory
    /// (`None` on the first navigation at startup).
    pub fn commit(&mut self, kind: NavKind, from: Option<&Path>, to: &Path) {
        match kind {
            NavKind::Refresh => {}
            NavKind::New => {
                if let Some(from) = from
                    && from != to
                {
                    self.back.push(from.to_path_buf());
                    if self.back.len() > MAX_HISTORY {
                        self.back.remove(0);
                    }
                    self.forward.clear();
                }
            }
            NavKind::Back => {
                self.back.pop();
                if let Some(from) = from {
                    self.forward.push(from.to_path_buf());
                }
            }
            NavKind::Forward => {
                self.forward.pop();
                if let Some(from) = from {
                    self.back.push(from.to_path_buf());
                }
            }
            NavKind::Jump { back, steps } => {
                let (source, other) = if back {
                    (&mut self.back, &mut self.forward)
                } else {
                    (&mut self.forward, &mut self.back)
                };
                // The same result as `steps` single steps: the folder we leave and the ones
                // passed over move to the other stack, the target is where we arrive.
                let passed = source.split_off(source.len().saturating_sub(steps));
                if let Some(from) = from {
                    other.push(from.to_path_buf());
                }
                other.extend(passed.into_iter().skip(1).rev());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_and_forward() {
        let (a, b, c) = (Path::new("a"), Path::new("b"), Path::new("c"));
        let mut h = History::default();
        h.commit(NavKind::New, None, a);
        h.commit(NavKind::New, Some(a), b);
        h.commit(NavKind::New, Some(b), c);
        assert_eq!(h.back_target(), Some(b));
        h.commit(NavKind::Back, Some(c), b);
        assert_eq!(h.back_target(), Some(a));
        assert_eq!(h.forward_target(), Some(c));
        h.commit(NavKind::Forward, Some(b), c);
        assert_eq!(h.forward_target(), None);
        assert_eq!(h.back_target(), Some(b));
        // A new navigation clears forward.
        h.commit(NavKind::Back, Some(c), b);
        h.commit(NavKind::New, Some(b), a);
        assert!(!h.can_go_forward());
    }

    #[test]
    fn jump_equals_repeated_steps() {
        let names = ["a", "b", "c", "d", "e"];
        let build = || {
            let mut h = History::default();
            h.commit(NavKind::New, None, Path::new(names[0]));
            for w in names.windows(2) {
                h.commit(NavKind::New, Some(Path::new(w[0])), Path::new(w[1]));
            }
            h
        };
        // At "e": back is e's predecessors d, c, b, a.
        let mut jumped = build();
        assert_eq!(jumped.back_at(3), Some(Path::new("b")));
        jumped.commit(
            NavKind::Jump {
                back: true,
                steps: 3,
            },
            Some(Path::new("e")),
            Path::new("b"),
        );
        let mut stepped = build();
        stepped.commit(NavKind::Back, Some(Path::new("e")), Path::new("d"));
        stepped.commit(NavKind::Back, Some(Path::new("d")), Path::new("c"));
        stepped.commit(NavKind::Back, Some(Path::new("c")), Path::new("b"));
        assert_eq!(jumped.back, stepped.back);
        assert_eq!(jumped.forward, stepped.forward);
        // And forward again over the skipped ones.
        jumped.commit(
            NavKind::Jump {
                back: false,
                steps: 3,
            },
            Some(Path::new("b")),
            Path::new("e"),
        );
        assert_eq!(jumped.back_entries().next(), Some(Path::new("d")));
        assert!(!jumped.can_go_forward());
    }
}
