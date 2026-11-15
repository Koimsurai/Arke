//! Which cloud folders This PC lists (#93): the ones File Explorer's navigation pane shows.
//!
//! Windows lists every Cloud Files sync root, and some providers register one per synced folder
//! (Nutstore: one per library), while the navigation pane shows only the folder each provider pins
//! there (`Desktop\NameSpace` with `System.IsPinnedToNameSpaceTree`). So:
//! - a sync root that is a pinned folder, or inside one, is shown through that pinned folder
//!   (once, under the sync root's own name when they're the same folder);
//! - a provider that pinned any of its roots has its other roots left out, as in Explorer;
//! - a provider that pinned nothing keeps all its roots (Explorer would show none, but they're
//!   still worth a click).

use std::path::{Path, PathBuf};

/// A Cloud Files sync root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyncRoot {
    /// Provider part of the sync root id (`OneDrive` in `OneDrive!S-1-5-...!Personal`).
    pub(crate) provider: String,
    pub(crate) label: String,
    pub(crate) path: PathBuf,
}

/// A folder pinned to the navigation pane: its name there and its target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pinned {
    pub(crate) label: String,
    pub(crate) path: PathBuf,
}

/// Lowercase, no trailing separator: Windows paths compare case-insensitively.
fn key(path: &Path) -> String {
    let mut s = path.to_string_lossy().to_lowercase().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// `inner` is `outer` or inside it.
fn within(inner: &str, outer: &str) -> bool {
    inner == outer
        || (inner.len() > outer.len()
            && inner.starts_with(outer)
            && (outer.ends_with('\\') || inner.as_bytes()[outer.len()] == b'\\'))
}

/// The cloud folders to list, as (label, path), in the order of the sync roots.
pub(crate) fn pick(roots: &[SyncRoot], pinned: &[Pinned]) -> Vec<(String, PathBuf)> {
    let pinned_keys: Vec<String> = pinned.iter().map(|p| key(&p.path)).collect();
    // The pinned folder each root is shown through, if any.
    let through: Vec<Option<usize>> = roots
        .iter()
        .map(|r| {
            let k = key(&r.path);
            pinned_keys.iter().position(|p| within(&k, p))
        })
        .collect();
    let pinning: Vec<&str> = roots
        .iter()
        .zip(&through)
        .filter(|(_, t)| t.is_some())
        .map(|(r, _)| r.provider.as_str())
        .collect();
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut shown_pins: Vec<usize> = Vec::new();
    for (root, through) in roots.iter().zip(&through) {
        match *through {
            Some(pin) if shown_pins.contains(&pin) => {}
            Some(pin) => {
                shown_pins.push(pin);
                let entry = if key(&root.path) == pinned_keys[pin] {
                    (root.label.clone(), root.path.clone())
                } else {
                    (pinned[pin].label.clone(), pinned[pin].path.clone())
                };
                out.push(entry);
            }
            None if pinning.contains(&root.provider.as_str()) => {}
            None => out.push((root.label.clone(), root.path.clone())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(provider: &str, label: &str, path: &str) -> SyncRoot {
        SyncRoot {
            provider: provider.into(),
            label: label.into(),
            path: path.into(),
        }
    }

    fn pin(label: &str, path: &str) -> Pinned {
        Pinned {
            label: label.into(),
            path: path.into(),
        }
    }

    fn labels(list: &[(String, PathBuf)]) -> Vec<&str> {
        list.iter().map(|(l, _)| l.as_str()).collect()
    }

    #[test]
    fn one_provider_one_root_is_unchanged() {
        let roots = [root("OneDrive", "OneDrive", r"C:\Users\me\OneDrive")];
        let pinned = [pin("OneDrive", r"c:\users\me\onedrive\")];
        assert_eq!(labels(&pick(&roots, &pinned)), ["OneDrive"]);
        assert_eq!(labels(&pick(&roots, &[])), ["OneDrive"], "nothing pinned");
    }

    #[test]
    fn a_provider_with_a_root_per_library_shows_the_pinned_one() {
        let roots = [
            root("Nutstore", "Nutstore", r"C:\Users\me\Nutstore\Nutstore"),
            root("Nutstore", "Nutstore-media", r"D:\media"),
            root("Nutstore", "Nutstore-work", r"C:\Users\me\Nutstore\work"),
            root("OneDrive", "OneDrive", r"C:\Users\me\OneDrive"),
        ];
        let pinned = [
            pin("Nutstore", r"C:\Users\me\Nutstore\Nutstore"),
            pin("OneDrive", r"C:\Users\me\OneDrive"),
        ];
        assert_eq!(labels(&pick(&roots, &pinned)), ["Nutstore", "OneDrive"]);
    }

    #[test]
    fn roots_inside_a_pinned_folder_show_as_that_folder_once() {
        let roots = [
            root("Nutstore", "Nutstore", r"C:\Users\me\Nutstore\Nutstore"),
            root("Nutstore", "Nutstore-work", r"C:\Users\me\Nutstore\work"),
        ];
        let pinned = [pin("坚果云", r"C:\Users\me\Nutstore")];
        let picked = pick(&roots, &pinned);
        assert_eq!(labels(&picked), ["坚果云"]);
        assert_eq!(picked[0].1, PathBuf::from(r"C:\Users\me\Nutstore"));
    }

    #[test]
    fn a_provider_that_pinned_nothing_keeps_its_roots() {
        let roots = [
            root("Other", "A", r"C:\a"),
            root("Other", "B", r"C:\b"),
            root("OneDrive", "OneDrive", r"C:\Users\me\OneDrive"),
        ];
        let pinned = [pin("OneDrive", r"C:\Users\me\OneDrive")];
        assert_eq!(labels(&pick(&roots, &pinned)), ["A", "B", "OneDrive"]);
    }

    #[test]
    fn a_sibling_with_a_common_prefix_is_not_inside() {
        let roots = [
            root("P", "Docs", r"C:\Docs"),
            root("P", "Docs2", r"C:\Docs2"),
        ];
        let pinned = [pin("Docs", r"C:\Docs")];
        assert_eq!(labels(&pick(&roots, &pinned)), ["Docs"]);
        assert!(!within(r"c:\docs2", r"c:\docs"));
    }
}
