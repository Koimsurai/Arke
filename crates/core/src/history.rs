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
}

/// Back and forward stacks of visited directories (back keeps at most 256).
#[derive(Default, Debug)]
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

    /// Directory a back navigation would open.
    pub fn back_target(&self) -> Option<&Path> {
        self.back.last().map(PathBuf::as_path)
    }

    /// Directory a forward navigation would open.
    pub fn forward_target(&self) -> Option<&Path> {
        self.forward.last().map(PathBuf::as_path)
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
}
