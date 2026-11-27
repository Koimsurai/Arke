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

/// A page the workspace shows over a tab instead of a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavPage {
    /// The Home page.
    Home,
    /// The Recycle Bin page.
    RecycleBin,
}

/// Where a pane can be, and so what back and forward step through: a folder, or a page over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A folder.
    Folder(PathBuf),
    /// A page shown over the tab.
    Page(NavPage),
}

impl Location {
    /// The folder, or `None` for a page.
    pub fn as_folder(&self) -> Option<&Path> {
        match self {
            Self::Folder(path) => Some(path),
            Self::Page(_) => None,
        }
    }
}

impl From<NavPage> for Location {
    fn from(page: NavPage) -> Self {
        Self::Page(page)
    }
}

/// Back and forward stacks of visited locations (back keeps at most 256).
#[derive(Clone, Default, Debug)]
pub struct History {
    back: Vec<Location>,
    forward: Vec<Location>,
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

    /// How many steps back there are: the position of the current location in the history.
    pub const fn depth(&self) -> usize {
        self.back.len()
    }

    /// Location a back navigation would open.
    pub fn back_target(&self) -> Option<&Location> {
        self.back.last()
    }

    /// Location a forward navigation would open.
    pub fn forward_target(&self) -> Option<&Location> {
        self.forward.last()
    }

    /// The `steps`-th entry down the back stack (1 is the top).
    pub fn back_at(&self, steps: usize) -> Option<&Location> {
        let ix = self.back.len().checked_sub(steps)?;
        self.back.get(ix)
    }

    /// The `steps`-th entry down the forward stack (1 is the top).
    pub fn forward_at(&self, steps: usize) -> Option<&Location> {
        let ix = self.forward.len().checked_sub(steps)?;
        self.forward.get(ix)
    }

    /// Back entries, the one Back opens first.
    pub fn back_entries(&self) -> impl Iterator<Item = &Location> {
        self.back.iter().rev()
    }

    /// Forward entries, the one Forward opens first.
    pub fn forward_entries(&self) -> impl Iterator<Item = &Location> {
        self.forward.iter().rev()
    }

    /// Called after a navigation succeeds. `from` is the previous location (`None` on the first
    /// navigation at startup).
    pub fn commit(&mut self, kind: NavKind, from: Option<Location>, to: &Location) {
        match kind {
            NavKind::Refresh => {}
            NavKind::New => {
                if let Some(from) = from
                    && from != *to
                {
                    self.back.push(from);
                    if self.back.len() > MAX_HISTORY {
                        self.back.remove(0);
                    }
                    self.forward.clear();
                }
            }
            NavKind::Back => {
                self.back.pop();
                if let Some(from) = from {
                    self.forward.push(from);
                }
            }
            NavKind::Forward => {
                self.forward.pop();
                if let Some(from) = from {
                    self.back.push(from);
                }
            }
            NavKind::Jump { back, steps } => {
                let (source, other) = if back {
                    (&mut self.back, &mut self.forward)
                } else {
                    (&mut self.forward, &mut self.back)
                };
                // The same result as `steps` single steps: the location we leave and the ones
                // passed over move to the other stack, the target is where we arrive.
                let passed = source.split_off(source.len().saturating_sub(steps));
                if let Some(from) = from {
                    other.push(from);
                }
                other.extend(passed.into_iter().skip(1).rev());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str) -> Location {
        Location::Folder(PathBuf::from(name))
    }

    const HOME: Location = Location::Page(NavPage::Home);
    const BIN: Location = Location::Page(NavPage::RecycleBin);

    #[test]
    fn back_and_forward() {
        let (a, b, c) = (f("a"), f("b"), f("c"));
        let mut h = History::default();
        h.commit(NavKind::New, None, &a);
        h.commit(NavKind::New, Some(a.clone()), &b);
        h.commit(NavKind::New, Some(b.clone()), &c);
        assert_eq!(h.back_target(), Some(&b));
        h.commit(NavKind::Back, Some(c.clone()), &b);
        assert_eq!(h.back_target(), Some(&a));
        assert_eq!(h.forward_target(), Some(&c));
        h.commit(NavKind::Forward, Some(b.clone()), &c);
        assert_eq!(h.forward_target(), None);
        assert_eq!(h.back_target(), Some(&b));
        // A new navigation clears forward.
        h.commit(NavKind::Back, Some(c), &b);
        h.commit(NavKind::New, Some(b), &a);
        assert!(!h.can_go_forward());
    }

    #[test]
    fn pages_step_like_folders() {
        // t -> Home -> Desktop, then back twice and forward twice.
        let (t, desktop) = (f("t"), f("desktop"));
        let mut h = History::default();
        h.commit(NavKind::New, None, &t);
        h.commit(NavKind::New, Some(t.clone()), &HOME);
        h.commit(NavKind::New, Some(HOME), &desktop);
        assert_eq!(h.back_target(), Some(&HOME));
        h.commit(NavKind::Back, Some(desktop.clone()), &HOME);
        assert_eq!(h.back_target(), Some(&t));
        h.commit(NavKind::Back, Some(HOME), &t);
        assert!(!h.can_go_back());
        assert_eq!(h.forward_target(), Some(&HOME));
        h.commit(NavKind::Forward, Some(t.clone()), &HOME);
        assert_eq!(h.forward_target(), Some(&desktop));
        h.commit(NavKind::Forward, Some(HOME), &desktop);
        assert_eq!(h.back_target(), Some(&HOME));
        assert!(!h.can_go_forward());
        // The Recycle Bin, then a folder typed in the path bar.
        h.commit(NavKind::New, Some(desktop.clone()), &BIN);
        h.commit(NavKind::New, Some(BIN), &t);
        assert_eq!(h.back_target(), Some(&BIN));
        assert_eq!(h.back_at(2), Some(&desktop));
    }

    #[test]
    fn jump_equals_repeated_steps() {
        let names = ["a", "b", "c", "d", "e"];
        let build = || {
            let mut h = History::default();
            h.commit(NavKind::New, None, &f(names[0]));
            for w in names.windows(2) {
                h.commit(NavKind::New, Some(f(w[0])), &f(w[1]));
            }
            h
        };
        // At "e": back is e's predecessors d, c, b, a.
        let mut jumped = build();
        assert_eq!(jumped.back_at(3), Some(&f("b")));
        jumped.commit(
            NavKind::Jump {
                back: true,
                steps: 3,
            },
            Some(f("e")),
            &f("b"),
        );
        let mut stepped = build();
        stepped.commit(NavKind::Back, Some(f("e")), &f("d"));
        stepped.commit(NavKind::Back, Some(f("d")), &f("c"));
        stepped.commit(NavKind::Back, Some(f("c")), &f("b"));
        assert_eq!(jumped.back, stepped.back);
        assert_eq!(jumped.forward, stepped.forward);
        // And forward again over the skipped ones.
        jumped.commit(
            NavKind::Jump {
                back: false,
                steps: 3,
            },
            Some(f("b")),
            &f("e"),
        );
        assert_eq!(jumped.back_entries().next(), Some(&f("d")));
        assert!(!jumped.can_go_forward());
    }
}
