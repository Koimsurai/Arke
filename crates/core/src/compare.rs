//! Folder comparison for dual pane: which items exist on one side only, and which are on both
//! sides but differ in size or modified time. Pure: works on the listings the panes already have,
//! never touches the file system.
//!
//! Names match case-insensitively, as Windows does. Folders on both sides count as equal (their
//! contents are not walked); a folder on one side and a file of the same name on the other differ.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::entry::Entry;

/// Modified times closer than this count as the same: FAT and exFAT keep them in 2 second steps,
/// so a copy on a USB stick would otherwise always look changed.
pub const TIME_TOLERANCE: Duration = Duration::from_secs(2);

/// How an item compares with its namesake in the other pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mark {
    /// Only this side has it.
    Unique,
    /// Both sides have it; this copy was modified later.
    Newer,
    /// Both sides have it; this copy was modified earlier.
    Older,
    /// Both sides have it with the same (or an unknown) time but a different size, or it is a
    /// folder on one side and a file on the other.
    Differs,
}

/// Marks for both panes, by item name as listed on that side. Items that match are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Marks for the left pane's items.
    pub left: HashMap<Arc<str>, Mark>,
    /// Marks for the right pane's items.
    pub right: HashMap<Arc<str>, Mark>,
}

impl Comparison {
    /// Both folders hold the same items.
    pub fn is_same(&self) -> bool {
        self.left.is_empty() && self.right.is_empty()
    }

    /// Marks for one side: 0 is the left pane, anything else the right.
    pub const fn side(&self, side: usize) -> &HashMap<Arc<str>, Mark> {
        if side == 0 { &self.left } else { &self.right }
    }
}

/// Whether two modified times are the same within [`TIME_TOLERANCE`]; `None` if either is unknown.
fn time_order(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<std::cmp::Ordering> {
    let (a, b) = (a?, b?);
    let gap = a.duration_since(b).or_else(|_| b.duration_since(a)).ok()?;
    Some(if gap <= TIME_TOLERANCE {
        std::cmp::Ordering::Equal
    } else {
        a.cmp(&b)
    })
}

/// Marks for `a` and `b`, two items with the same name; `None` if they match.
fn marks(a: &Entry, b: &Entry) -> Option<(Mark, Mark)> {
    match (a.is_dir_like(), b.is_dir_like()) {
        (true, true) => return None,
        (false, false) => {}
        _ => return Some((Mark::Differs, Mark::Differs)),
    }
    match time_order(a.modified, b.modified) {
        Some(std::cmp::Ordering::Greater) => Some((Mark::Newer, Mark::Older)),
        Some(std::cmp::Ordering::Less) => Some((Mark::Older, Mark::Newer)),
        _ if a.size != b.size => Some((Mark::Differs, Mark::Differs)),
        _ => None,
    }
}

/// Compares two listings.
pub fn compare(left: &[Entry], right: &[Entry]) -> Comparison {
    let by_key: HashMap<&str, &Entry> = right.iter().map(|e| (&*e.sort_key, e)).collect();
    let mut out = Comparison::default();
    let mut matched: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for a in left {
        match by_key.get(&*a.sort_key) {
            None => {
                out.left.insert(a.name.clone(), Mark::Unique);
            }
            Some(b) => {
                matched.insert(&b.sort_key);
                if let Some((on_left, on_right)) = marks(a, b) {
                    out.left.insert(a.name.clone(), on_left);
                    out.right.insert(b.name.clone(), on_right);
                }
            }
        }
    }
    for b in right {
        if !matched.contains(&*b.sort_key) {
            out.right.insert(b.name.clone(), Mark::Unique);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn file(name: &str, size: u64, modified: Option<SystemTime>) -> Entry {
        Entry::new(name, EntryKind::File, size, modified, 0)
    }

    fn dir(name: &str) -> Entry {
        Entry::new(name, EntryKind::Dir, 0, Some(at(0)), 0)
    }

    fn mark(map: &HashMap<Arc<str>, Mark>, name: &str) -> Option<Mark> {
        map.get(name).copied()
    }

    #[test]
    fn items_on_one_side_only_are_unique() {
        let c = compare(
            &[
                file("a.txt", 1, Some(at(0))),
                file("left.txt", 1, Some(at(0))),
            ],
            &[
                file("a.txt", 1, Some(at(0))),
                file("right.txt", 1, Some(at(0))),
            ],
        );
        assert_eq!(mark(&c.left, "left.txt"), Some(Mark::Unique));
        assert_eq!(mark(&c.right, "right.txt"), Some(Mark::Unique));
        assert_eq!(
            mark(&c.left, "a.txt"),
            None,
            "identical files are not marked"
        );
        assert_eq!(mark(&c.right, "a.txt"), None);
        assert_eq!((c.left.len(), c.right.len()), (1, 1));
    }

    #[test]
    fn later_copy_is_newer_and_other_older() {
        let c = compare(
            &[file("r.doc", 10, Some(at(100)))],
            &[file("r.doc", 10, Some(at(0)))],
        );
        assert_eq!(mark(&c.left, "r.doc"), Some(Mark::Newer));
        assert_eq!(mark(&c.right, "r.doc"), Some(Mark::Older));
        let c = compare(
            &[file("r.doc", 10, Some(at(0)))],
            &[file("r.doc", 99, Some(at(100)))],
        );
        assert_eq!(
            mark(&c.left, "r.doc"),
            Some(Mark::Older),
            "time wins over size"
        );
        assert_eq!(mark(&c.right, "r.doc"), Some(Mark::Newer));
    }

    #[test]
    fn same_time_different_size_differs() {
        let c = compare(&[file("x", 1, Some(at(5)))], &[file("x", 2, Some(at(5)))]);
        assert_eq!(mark(&c.left, "x"), Some(Mark::Differs));
        assert_eq!(mark(&c.right, "x"), Some(Mark::Differs));
        // Unknown times fall back to the size alone.
        let c = compare(&[file("x", 1, None)], &[file("x", 2, Some(at(5)))]);
        assert_eq!(mark(&c.left, "x"), Some(Mark::Differs));
        assert!(compare(&[file("x", 1, None)], &[file("x", 1, Some(at(5)))]).is_same());
    }

    #[test]
    fn fat_time_rounding_is_not_a_difference() {
        let c = compare(&[file("x", 1, Some(at(10)))], &[file("x", 1, Some(at(12)))]);
        assert!(c.is_same(), "two seconds apart is the same time");
        let c = compare(&[file("x", 1, Some(at(10)))], &[file("x", 1, Some(at(13)))]);
        assert_eq!(mark(&c.right, "x"), Some(Mark::Newer));
    }

    #[test]
    fn names_match_case_insensitively_and_keep_each_sides_spelling() {
        let c = compare(
            &[file("Report.PDF", 1, Some(at(0)))],
            &[file("report.pdf", 2, Some(at(0)))],
        );
        assert_eq!(mark(&c.left, "Report.PDF"), Some(Mark::Differs));
        assert_eq!(mark(&c.right, "report.pdf"), Some(Mark::Differs));
    }

    #[test]
    fn folders_compare_by_presence_only() {
        let c = compare(&[dir("src"), dir("docs")], &[dir("SRC")]);
        assert_eq!(mark(&c.left, "src"), None, "folder contents are not walked");
        assert_eq!(mark(&c.left, "docs"), Some(Mark::Unique));
        // A folder against a file of the same name.
        let c = compare(&[dir("build")], &[file("build", 1, Some(at(0)))]);
        assert_eq!(mark(&c.left, "build"), Some(Mark::Differs));
        assert_eq!(mark(&c.right, "build"), Some(Mark::Differs));
    }

    #[test]
    fn empty_and_identical_listings_are_the_same() {
        assert!(compare(&[], &[]).is_same());
        let one = [file("a", 1, Some(at(0))), dir("b")];
        assert!(compare(&one, &one).is_same());
        let c = compare(&one, &[]);
        assert_eq!(c.left.len(), 2);
        assert!(c.right.is_empty());
        assert_eq!(c.side(0), &c.left);
        assert_eq!(c.side(1), &c.right);
    }
}
