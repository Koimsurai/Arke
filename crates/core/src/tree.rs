//! Inline folder tree for the Details view: folders expand in place and their children are listed
//! indented below them.
//!
//! The list on screen stays one flat, virtualized list. [`flatten`] turns the folder shown plus the
//! expanded subfolders into that list: nested items get their path relative to the folder shown as
//! their name (`src\main.rs`), so joining the folder shown with a row's name always gives its full
//! path, and names stay unique across the whole tree (selection, rename and drag-and-drop key on
//! them). [`TreeRow`] carries what the rows need besides the entry: depth and expansion state.

use std::sync::Arc;

use crate::entry::Entry;

/// Separator between the parts of a nested name.
pub const SEP: char = std::path::MAIN_SEPARATOR;

/// How a folder in the tree is shown, as reported to [`flatten`].
#[derive(Clone, Copy, Debug)]
pub enum Expansion<'a> {
    /// Not expanded.
    Collapsed,
    /// Expanded, children still being listed.
    Loading,
    /// Expanded with these children, already sorted and filtered (names are plain file names).
    Open(&'a [Entry]),
}

/// One row of the flattened tree, parallel to the entry list [`flatten`] returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeRow {
    /// Nesting depth: 0 for items of the folder shown.
    pub depth: u16,
    /// Folder that is expanded.
    pub expanded: bool,
    /// Expanded folder whose children haven't been listed yet.
    pub loading: bool,
}

/// The visible tree as one list.
#[derive(Clone, Debug, Default)]
pub struct Flat {
    /// Entries in display order; nested ones are named by their relative path.
    pub entries: Vec<Entry>,
    /// Depth and state of each entry, same order.
    pub rows: Vec<TreeRow>,
}

/// File name part of a (possibly nested) row name.
pub fn leaf(name: &str) -> &str {
    name.rfind(SEP)
        .map_or(name, |ix| &name[ix + SEP.len_utf8()..])
}

/// Relative path of the folder holding a nested row; `None` for items of the folder shown.
pub fn parent(name: &str) -> Option<&str> {
    name.rfind(SEP).map(|ix| &name[..ix])
}

/// Name of `leaf` inside the row named `parent` (`None`: inside the folder shown).
pub fn join(parent: Option<&str>, leaf: &str) -> Arc<str> {
    match parent {
        Some(parent) => format!("{parent}{SEP}{leaf}").into(),
        None => leaf.into(),
    }
}

/// Whether the row named `name` lies somewhere below the row named `ancestor`.
pub fn is_within(name: &str, ancestor: &str) -> bool {
    name.len() > ancestor.len()
        && name.starts_with(ancestor)
        && name[ancestor.len()..].starts_with(SEP)
}

/// Nesting depth of a row name (number of separators).
pub fn depth_of(name: &str) -> usize {
    name.matches(SEP).count()
}

/// `child` renamed to its path relative to the folder shown, given its parent row.
pub fn nested(parent: &Entry, child: &Entry) -> Entry {
    Entry {
        name: format!("{}{SEP}{}", parent.name, child.name).into(),
        sort_key: format!("{}{SEP}{}", parent.sort_key, child.sort_key).into(),
        ..child.clone()
    }
}

/// Flattens `top` (the folder shown, sorted) and every expanded folder below it into one list.
/// `expansion` is asked about each folder by its row name; only folders are asked.
pub fn flatten<'a>(top: &[Entry], expansion: impl Fn(&str) -> Expansion<'a>) -> Flat {
    let mut flat = Flat {
        entries: Vec::with_capacity(top.len()),
        rows: Vec::with_capacity(top.len()),
    };
    for entry in top {
        push(&mut flat, entry.clone(), 0, &expansion);
    }
    flat
}

fn push<'a>(flat: &mut Flat, entry: Entry, depth: u16, expansion: &impl Fn(&str) -> Expansion<'a>) {
    let state = if entry.is_dir_like() {
        expansion(&entry.name)
    } else {
        Expansion::Collapsed
    };
    let row = TreeRow {
        depth,
        expanded: !matches!(state, Expansion::Collapsed),
        loading: matches!(state, Expansion::Loading),
    };
    flat.rows.push(row);
    let Expansion::Open(children) = state else {
        flat.entries.push(entry);
        return;
    };
    let parent = entry.clone();
    flat.entries.push(entry);
    for child in children {
        push(
            flat,
            nested(&parent, child),
            depth.saturating_add(1),
            expansion,
        );
    }
}

/// Row of the folder directly holding row `ix` (Left arrow on a collapsed or nested item).
pub fn parent_row(rows: &[TreeRow], ix: usize) -> Option<usize> {
    let depth = rows.get(ix)?.depth;
    if depth == 0 {
        return None;
    }
    rows[..ix].iter().rposition(|r| r.depth < depth)
}

/// First child row of row `ix`, if it is an expanded folder with listed children (Right arrow on
/// an expanded folder).
pub fn first_child_row(rows: &[TreeRow], ix: usize) -> Option<usize> {
    let row = rows.get(ix)?;
    let next = rows.get(ix + 1)?;
    (row.expanded && next.depth > row.depth).then_some(ix + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
    use std::collections::HashMap;

    fn dir(name: &str) -> Entry {
        Entry::new(name, EntryKind::Dir, 0, None, 0)
    }

    fn file(name: &str) -> Entry {
        Entry::new(name, EntryKind::File, 1, None, 0)
    }

    fn rel(parts: &[&str]) -> String {
        parts.join(&SEP.to_string())
    }

    type Children = HashMap<String, Option<Vec<Entry>>>;

    fn flat_of(top: &[Entry], children: &Children) -> Flat {
        flatten(top, |name| match children.get(name) {
            None => Expansion::Collapsed,
            Some(None) => Expansion::Loading,
            Some(Some(list)) => Expansion::Open(list),
        })
    }

    fn names(flat: &Flat) -> Vec<String> {
        flat.entries.iter().map(|e| e.name.to_string()).collect()
    }

    #[test]
    fn nothing_expanded_is_the_listing_itself() {
        let top = vec![dir("a"), dir("b"), file("c.txt")];
        let flat = flat_of(&top, &Children::new());
        assert_eq!(names(&flat), ["a", "b", "c.txt"]);
        assert!(flat.rows.iter().all(|r| *r == TreeRow::default()));
    }

    #[test]
    fn expanded_folders_list_children_below_with_relative_names() {
        let top = vec![dir("a"), dir("b"), file("c.txt")];
        let mut children = Children::new();
        children.insert("a".into(), Some(vec![dir("inner"), file("x.rs")]));
        children.insert(rel(&["a", "inner"]), Some(vec![file("deep.md")]));
        let flat = flat_of(&top, &children);
        assert_eq!(
            names(&flat),
            [
                "a".to_string(),
                rel(&["a", "inner"]),
                rel(&["a", "inner", "deep.md"]),
                rel(&["a", "x.rs"]),
                "b".into(),
                "c.txt".into(),
            ]
        );
        let depths: Vec<u16> = flat.rows.iter().map(|r| r.depth).collect();
        assert_eq!(depths, [0, 1, 2, 1, 0, 0]);
        assert!(flat.rows[0].expanded && flat.rows[1].expanded);
        assert!(!flat.rows[4].expanded, "b is collapsed");
        // Sort keys follow the names, lowercase.
        assert_eq!(&*flat.entries[3].sort_key, rel(&["a", "x.rs"]));
    }

    #[test]
    fn loading_folders_have_no_children_yet() {
        let top = vec![dir("a"), file("b")];
        let mut children = Children::new();
        children.insert("a".into(), None);
        let flat = flat_of(&top, &children);
        assert_eq!(names(&flat), ["a", "b"]);
        assert!(flat.rows[0].expanded && flat.rows[0].loading);
    }

    #[test]
    fn expansion_under_a_collapsed_folder_stays_hidden() {
        // "a\inner" is remembered as expanded but "a" isn't: nothing below "a" shows.
        let top = vec![dir("a")];
        let mut children = Children::new();
        children.insert(rel(&["a", "inner"]), Some(vec![file("deep.md")]));
        let flat = flat_of(&top, &children);
        assert_eq!(names(&flat), ["a"]);
    }

    #[test]
    fn files_are_never_asked_about() {
        let top = vec![file("a")];
        let flat = flatten(&top, |_| panic!("asked about a file"));
        assert_eq!(names(&flat), ["a"]);
    }

    #[test]
    fn name_helpers() {
        let nested_name = rel(&["a", "b", "c.txt"]);
        assert_eq!(leaf(&nested_name), "c.txt");
        assert_eq!(leaf("top"), "top");
        assert_eq!(parent(&nested_name), Some(rel(&["a", "b"]).as_str()));
        assert_eq!(parent("top"), None);
        assert_eq!(&*join(Some("a"), "b"), rel(&["a", "b"]));
        assert_eq!(&*join(None, "b"), "b");
        assert!(is_within(&nested_name, "a"));
        assert!(is_within(&nested_name, &rel(&["a", "b"])));
        assert!(
            !is_within("ab", "a"),
            "a sibling with a longer name isn't inside"
        );
        assert!(!is_within("a", "a"));
        assert_eq!(depth_of(&nested_name), 2);
        assert_eq!(depth_of("top"), 0);
    }

    #[test]
    fn arrow_targets() {
        let top = vec![dir("a"), dir("b")];
        let mut children = Children::new();
        children.insert("a".into(), Some(vec![dir("inner"), file("x")]));
        children.insert("b".into(), Some(Vec::new()));
        let flat = flat_of(&top, &children);
        // a, a\inner, a\x, b
        assert_eq!(parent_row(&flat.rows, 2), Some(0));
        assert_eq!(parent_row(&flat.rows, 1), Some(0));
        assert_eq!(parent_row(&flat.rows, 0), None);
        assert_eq!(first_child_row(&flat.rows, 0), Some(1));
        assert_eq!(first_child_row(&flat.rows, 1), None, "collapsed");
        assert_eq!(first_child_row(&flat.rows, 3), None, "expanded but empty");
    }
}
