//! The order of Arke's context menu entries, as dragged in Settings › Context menu (#263).
//!
//! Each row of that list has a stable key ([`DEFAULT`]). `Settings::menu_order` holds the keys
//! in the order they were dragged to, and stays empty while it is the built-in one, so such a
//! menu is drawn just as it is built. Keys it doesn't know (from a newer version, or edited by
//! hand) are ignored, and an entry it doesn't mention (one added since) keeps its built-in
//! place, right after the entry before it there.
//!
//! A menu has only some of the entries (a file's, a folder's, the folder background's): they are
//! sorted by the order, rows without a key of their own staying with the one before them. The
//! groups between separators stay as built, and an entry moved away from its group joins the one
//! it was moved into. The places are worked out once when the setting changes, not per menu.

use std::cell::RefCell;

/// The entries in their built-in order: a folder's menu, top to bottom (the folder
/// background's "View" with the "New" it comes after there).
pub(crate) const DEFAULT: [&str; 20] = [
    "open",
    "new_tab",
    "new",
    "view",
    "search_folder",
    "pin",
    "terminal",
    "archive",
    "set_background",
    "import_settings",
    "move_new_folder",
    "new_window",
    "open_with",
    "shelf",
    "cloud",
    "copy_path",
    "relative_path",
    "reveal",
    "properties",
    "more_options",
];

/// Entries that can be moved but not turned off.
pub(crate) const CORE: [&str; 2] = ["open", "new"];

/// `stored` as a whole order: its known keys in its order (each once), then each key it doesn't
/// mention right after the one before it in the built-in order.
pub(crate) fn resolve(stored: &[String]) -> Vec<&'static str> {
    let mut order: Vec<&'static str> = Vec::with_capacity(DEFAULT.len());
    for key in stored {
        if let Some(known) = DEFAULT.iter().find(|k| **k == key.as_str())
            && !order.contains(known)
        {
            order.push(*known);
        }
    }
    for (ix, key) in DEFAULT.iter().enumerate() {
        if order.contains(key) {
            continue;
        }
        let after = DEFAULT[..ix]
            .iter()
            .rev()
            .find_map(|prev| order.iter().position(|k| k == prev));
        order.insert(after.map_or(0, |at| at + 1), *key);
    }
    order
}

/// What to store for `order`: nothing while it is the built-in order.
pub(crate) fn stored(order: &[&'static str]) -> Vec<String> {
    if order == DEFAULT.as_slice() {
        Vec::new()
    } else {
        order.iter().map(|k| (*k).to_owned()).collect()
    }
}

/// `order` with the entry at `from` moved to `to`.
pub(crate) fn moved(order: &[&'static str], from: usize, to: usize) -> Vec<&'static str> {
    let mut order = order.to_vec();
    if from < order.len() && to < order.len() {
        let key = order.remove(from);
        order.insert(to, key);
    }
    order
}

/// Each entry's place in a custom order, by its index in [`DEFAULT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ranks([u8; DEFAULT.len()]);

impl Ranks {
    fn of(self, key: &str) -> Option<u8> {
        DEFAULT.iter().position(|k| *k == key).map(|ix| self.0[ix])
    }
}

thread_local! {
    /// The stored order last asked about, and its places.
    static RANKS: RefCell<(Vec<String>, Option<Ranks>)> = const { RefCell::new((Vec::new(), None)) };
}

/// The places of `stored`, or `None` for the built-in order (nothing to rearrange). Worked out
/// again only when `stored` changed since the last call.
pub(crate) fn ranks(stored: &[String]) -> Option<Ranks> {
    if stored.is_empty() {
        return None;
    }
    RANKS.with_borrow_mut(|(seen, ranks)| {
        if seen.as_slice() != stored {
            seen.clear();
            seen.extend_from_slice(stored);
            *ranks = ranks_of(&resolve(stored));
        }
        *ranks
    })
}

fn ranks_of(order: &[&'static str]) -> Option<Ranks> {
    if order == DEFAULT.as_slice() {
        return None;
    }
    let mut ranks = [0; DEFAULT.len()];
    for (place, key) in order.iter().enumerate() {
        if let Some(ix) = DEFAULT.iter().position(|k| k == key) {
            ranks[ix] = u8::try_from(place).unwrap_or(u8::MAX);
        }
    }
    Some(Ranks(ranks))
}

/// What an entry is to the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// One of [`DEFAULT`] (an unknown key counts as [`Slot::Follow`]).
    Key(&'static str),
    /// No key of its own: stays right after the entry before it.
    Follow,
    /// Stays at the end (the icon button row).
    Last,
    Separator,
}

/// `entries`, as a menu is built, in the order of `ranks`. Rows of the same entry keep their
/// order. The entries that stay in their built order (the longest run of them) keep their
/// groups; a moved one joins the group of the entry it lands after (before, at the top), and a
/// separator from `separator` goes between groups. Separators may end up at an end; hiding
/// entries tidies them.
pub(crate) fn arrange<T>(
    entries: Vec<T>,
    ranks: Ranks,
    slot: impl Fn(&T) -> Slot,
    separator: impl Fn() -> T,
) -> Vec<T> {
    // (place, row in the built menu, group, entry)
    let mut placed: Vec<(u16, usize, usize, T)> = Vec::with_capacity(entries.len());
    let (mut rank, mut group) = (0, 0);
    for entry in entries {
        match slot(&entry) {
            Slot::Separator => {
                group += 1;
                continue;
            }
            Slot::Key(key) => {
                if let Some(r) = ranks.of(key) {
                    rank = u16::from(r);
                }
            }
            Slot::Follow => {}
            Slot::Last => rank = u16::MAX,
        }
        placed.push((rank, placed.len(), group, entry));
    }
    // Stable: the rows of one entry, and the ones following it, stay together and in order.
    placed.sort_by_key(|(rank, ..)| *rank);
    let rows: Vec<usize> = placed.iter().map(|(_, row, ..)| *row).collect();
    let kept = longest_rising(&rows);
    let mut groups: Vec<usize> = placed.iter().map(|(_, _, group, _)| *group).collect();
    let first_kept = kept.iter().position(|k| *k).map(|at| groups[at]);
    for ix in 0..groups.len() {
        if !kept[ix] {
            groups[ix] = match ix.checked_sub(1) {
                Some(prev) => groups[prev],
                None => first_kept.unwrap_or(groups[ix]),
            };
        }
    }
    let mut out = Vec::with_capacity(placed.len() * 2);
    let mut last = None;
    for ((.., entry), group) in placed.into_iter().zip(groups) {
        if last.is_some_and(|l| l != group) {
            out.push(separator());
        }
        last = Some(group);
        out.push(entry);
    }
    out
}

/// Which of `rows` make up a longest rising run (not necessarily adjacent): the entries that
/// didn't move.
fn longest_rising(rows: &[usize]) -> Vec<bool> {
    let mut len = vec![1_usize; rows.len()];
    let mut prev: Vec<Option<usize>> = vec![None; rows.len()];
    for at in 0..rows.len() {
        if let Some(best) = (0..at)
            .filter(|&before| rows[before] < rows[at])
            .max_by_key(|&before| len[before])
        {
            len[at] = len[best] + 1;
            prev[at] = Some(best);
        }
    }
    let mut kept = vec![false; rows.len()];
    let mut at = (0..rows.len()).max_by_key(|&ix| len[ix]);
    while let Some(ix) = at {
        kept[ix] = true;
        at = prev[ix];
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT, Slot, arrange, moved, ranks, ranks_of, resolve, stored};

    fn keys(order: &[&str]) -> Vec<String> {
        order.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn stored_orders_load_leniently() {
        assert_eq!(resolve(&[]), DEFAULT);
        // Unknown and repeated keys are dropped; missing ones go after their built-in neighbor.
        let order = resolve(&keys(&["properties", "bogus", "open", "properties"]));
        assert_eq!(
            &order[..4],
            ["properties", "more_options", "open", "new_tab"]
        );
        assert_eq!(order.len(), DEFAULT.len());
        assert_eq!(order.last(), Some(&"reveal"));
        // An entry added in a later version: right after the one before it, wherever that went.
        let mut saved_before: Vec<&str> =
            DEFAULT.iter().copied().filter(|k| *k != "shelf").collect();
        let open_with = saved_before.iter().position(|k| *k == "open_with").unwrap();
        let key = saved_before.remove(open_with);
        saved_before.insert(0, key);
        let order = resolve(&keys(&saved_before));
        assert_eq!(&order[..2], ["open_with", "shelf"]);
        assert_eq!(order.len(), DEFAULT.len());
    }

    #[test]
    fn only_a_changed_order_is_stored() {
        assert!(stored(&DEFAULT).is_empty());
        let order = moved(&DEFAULT, DEFAULT.len() - 2, 0);
        assert_eq!(order[0], "properties");
        assert_eq!(stored(&order).len(), DEFAULT.len());
        // Moved back: nothing to store again.
        assert!(stored(&moved(&order, 0, DEFAULT.len() - 2)).is_empty());
        assert_eq!(ranks(&[]), None);
        assert_eq!(
            ranks(&keys(&DEFAULT)),
            None,
            "the built-in order, spelled out"
        );
        assert_eq!(ranks(&keys(&order)), ranks_of(&order));
        assert!(ranks(&keys(&order)).is_some());
    }

    /// A row's label and key (`None`: follows the one before, `"-"` a separator, `"[]"` the icon
    /// row).
    type Row = (&'static str, Option<&'static str>);

    fn slot(row: &Row) -> Slot {
        match row.1 {
            Some("-") => Slot::Separator,
            Some("[]") => Slot::Last,
            Some(key) => Slot::Key(key),
            None => Slot::Follow,
        }
    }

    fn arranged(rows: &[Row], order: &[&str]) -> Vec<&'static str> {
        let ranks = ranks_of(&resolve(&keys(order))).expect("a custom order");
        arrange(rows.to_vec(), ranks, slot, || ("—", Some("-")))
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    /// A folder's menu, shortened.
    const FOLDER: [Row; 14] = [
        ("Open", Some("open")),
        ("New tab", Some("new_tab")),
        ("—", Some("-")),
        ("New", Some("new")),
        ("Search", Some("search_folder")),
        ("—", Some("-")),
        ("Pin", Some("pin")),
        ("Search network", None),
        ("Terminal", Some("terminal")),
        ("—", Some("-")),
        ("Copy path", Some("copy_path")),
        ("Properties", Some("properties")),
        ("—", Some("-")),
        ("[]", Some("[]")),
    ];

    #[test]
    fn a_moved_entry_joins_the_group_it_lands_in() {
        // Copy path dragged right under Open: it joins Open's group; its own keeps its separator.
        let order = moved(&DEFAULT, 15, 1);
        assert_eq!(
            arranged(&FOLDER, &order),
            [
                "Open",
                "Copy path",
                "New tab",
                "—",
                "New",
                "Search",
                "—",
                "Pin",
                "Search network",
                "Terminal",
                "—",
                "Properties",
                "—",
                "[]"
            ]
        );
        // Pin dragged to the very top: with the row that follows it, into Open's group. The icon
        // row stays at the end.
        let order = moved(&DEFAULT, 5, 0);
        assert_eq!(
            arranged(&FOLDER, &order),
            [
                "Pin",
                "Search network",
                "Open",
                "New tab",
                "—",
                "New",
                "Search",
                "—",
                "Terminal",
                "—",
                "Copy path",
                "Properties",
                "—",
                "[]"
            ]
        );
    }

    #[test]
    fn entries_a_menu_lacks_change_nothing() {
        // The background menu has no Open or New tab: moving them around keeps it as built.
        let background: [Row; 5] = [
            ("New", Some("new")),
            ("Paste", None),
            ("—", Some("-")),
            ("View", Some("view")),
            ("Refresh", Some("view")),
        ];
        let order = moved(&DEFAULT, 1, 0);
        assert_eq!(
            arranged(&background, &order),
            ["New", "Paste", "—", "View", "Refresh"]
        );
    }
}
