//! Custom names for mapped network drives, network places and saved remote locations, and the
//! names given to favorites (#259).
//!
//! Kept in Arke's settings only: Windows' own drive label, its registry and `MountPoints2` are never
//! touched. An empty name (or the default one) removes the entry and brings the default name back.

use std::collections::BTreeMap;
use std::path::{Component, Path, Prefix};

use gpui::{AnyElement, Entity, MouseButton, div, prelude::*};
use rust_i18n::t;

use crate::settings::Settings;
use crate::shell::{Place, PlaceKind};
use crate::text_input::TextInput;
use crate::theme;

/// Longest custom name, in characters.
const NAME_MAX: usize = 64;

/// Custom names by [`key`].
pub(crate) type Names = BTreeMap<String, String>;

/// Settings key of a place: drive roots and shares compare ignoring case and trailing
/// separators, remote URLs in their canonical form.
pub(crate) fn key(path: &Path) -> String {
    if arke_core::remote::is_remote(path) {
        return arke_core::remote::canonical(path.to_path_buf())
            .to_string_lossy()
            .into_owned();
    }
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// The letter of a drive root (`Z:\`), `None` for anything else.
pub(crate) fn drive_letter(path: &Path) -> Option<char> {
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return None;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return None;
    };
    match (parts.next(), parts.next()) {
        (None | Some(Component::RootDir), None) => Some(char::from(letter.to_ascii_uppercase())),
        _ => None,
    }
}

/// What to show for `path` when it is a place with a custom name: the name, and for a drive root
/// the letter after it, like File Explorer shows a drive.
pub(crate) fn shown_name(names: &Names, path: &Path) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let custom = names.get(&key(path))?;
    Some(match drive_letter(path) {
        Some(letter) => format!("{custom} ({letter}:)"),
        None => custom.clone(),
    })
}

/// A share's default name: `\\nas\docs` -> "docs (nas)".
pub(crate) fn share_label(share: &Path) -> String {
    let s = share.to_string_lossy();
    let mut parts = s.trim_start_matches('\\').splitn(2, '\\');
    match (parts.next(), parts.next()) {
        (Some(server), Some(name)) => {
            t!("sidebar.share_label", share = name, server = server).into_owned()
        }
        _ => s.into_owned(),
    }
}

/// A place's label with its custom name applied (mapped drives only; the rest keep theirs).
pub(crate) fn place_label(names: &Names, place: &Place) -> String {
    match place.kind {
        PlaceKind::Drive { remote: true, .. } => {
            shown_name(names, &place.path).unwrap_or_else(|| place.label.clone())
        }
        _ => place.label.clone(),
    }
}

/// A mapped drive's own name without its letter (`Backup (Z:)` -> `Backup`).
pub(crate) fn drive_default_name(place: &Place) -> String {
    drive_letter(&place.path)
        .and_then(|letter| place.label.strip_suffix(&format!(" ({letter}:)")))
        .unwrap_or(&place.label)
        .to_owned()
}

/// Where the ` (C:)` Windows ends a drive's name with starts, if `label` ends with one.
pub(crate) fn letter_suffix(label: &str) -> Option<usize> {
    let at = label.len().checked_sub(5).filter(|&at| at > 0)?;
    match label.as_bytes().get(at..)? {
        [b' ', b'(', letter, b':', b')'] if letter.is_ascii_alphabetic() => Some(at),
        _ => None,
    }
}

/// A drive's name on one line that keeps its letter: a long name is cut before ` (C:)`, so two
/// drives of the same long name (German "Lokaler Datenträger") still tell apart. Other names are
/// cut at the end as usual.
pub(crate) fn drive_name(label: String) -> gpui::Div {
    let Some(at) = letter_suffix(&label) else {
        return div().min_w_0().truncate().child(label);
    };
    // A no-break space: a plain one at the start of its own text box may not take up room.
    let letter = format!("\u{a0}{}", &label[at + 1..]);
    let mut name = label;
    name.truncate(at);
    div()
        .min_w_0()
        .flex()
        .child(div().min_w_0().truncate().child(name))
        .child(div().flex_none().child(letter))
}

/// The box a place is renamed in, in the sidebar or on Home: like the one in a file list. Presses
/// stay in it, so the row under it neither opens the place nor takes the focus back.
pub(crate) fn rename_box(input: Entity<TextInput>) -> AnyElement {
    div()
        .debug_selector(|| "place-rename-box".to_owned())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .flex_1()
        .min_w_0()
        .h(theme::s_24())
        .px(theme::s_4())
        .flex()
        .items_center()
        .rounded(theme::r_4())
        .bg(theme::surface())
        .border_1()
        .border_color(theme::signal_line())
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::label_scope(
                t!("menu.rename").into_owned().into(),
            ))
        })
        .child(input)
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::end_label_scope())
        })
        .into_any_element()
}

impl Settings {
    /// The custom name of a place, without a drive's letter.
    pub(crate) fn place_name(&self, path: &Path) -> Option<&str> {
        self.place_names.get(&key(path)).map(String::as_str)
    }

    /// Gives a place a name. Empty, or the default name, brings the default back.
    pub(crate) fn rename_place(&mut self, path: &Path, name: &str, default: &str) {
        set_name(&mut self.place_names, path, name, default);
    }

    /// The name the user gave a favorite (#259), shown only where the favorite is: its tile or
    /// row in the sidebar. Kept apart from [`Self::place_names`], which also renames the folder in
    /// tab titles and the path bar.
    pub(crate) fn favorite_name(&self, path: &Path) -> Option<&str> {
        self.favorite_names.get(&key(path)).map(String::as_str)
    }

    /// Gives a favorite a name. Empty, or the default name, brings the default back.
    pub(crate) fn rename_favorite(&mut self, path: &Path, name: &str, default: &str) {
        set_name(&mut self.favorite_names, path, name, default);
    }
}

/// Stores `name` for `path` in `names`, cleaned up (no control characters, trimmed, at most
/// [`NAME_MAX`] characters); empty or `default` removes the entry instead.
fn set_name(names: &mut Names, path: &Path, name: &str, default: &str) {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(NAME_MAX)
        .collect();
    if name.is_empty() || name == default {
        names.remove(&key(path));
    } else {
        names.insert(key(path), name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn keys_ignore_case_and_trailing_separators() {
        assert_eq!(key(Path::new(r"Z:\")), "z:");
        assert_eq!(key(Path::new(r"\\NAS\Docs\")), r"\\nas\docs");
        assert_eq!(key(Path::new(r"\\nas\docs")), key(Path::new(r"\\NAS\DOCS")));
    }

    #[test]
    fn only_drive_roots_have_a_letter() {
        assert_eq!(drive_letter(Path::new(r"z:\")), Some('Z'));
        assert_eq!(drive_letter(Path::new(r"Z:")), Some('Z'));
        assert_eq!(drive_letter(Path::new(r"Z:\a")), None);
        assert_eq!(drive_letter(Path::new(r"\\nas\docs")), None);
    }

    #[test]
    fn a_drive_name_keeps_its_letter_apart() {
        assert_eq!(letter_suffix("Lokaler Datenträger (C:)"), Some(20));
        assert_eq!(letter_suffix(r"docs (\\nas) (Z:)"), Some(12));
        assert_eq!(letter_suffix(" (C:)"), None, "no name before it");
        assert_eq!(letter_suffix("Fotos (2024)"), None);
        assert_eq!(letter_suffix("Fotos"), None);
        assert_eq!(letter_suffix("(C:)"), None);
    }

    #[test]
    fn a_custom_name_replaces_the_default_and_empty_restores_it() {
        let mut s = Settings::default();
        let drive = PathBuf::from(r"Z:\");
        s.rename_place(&drive, "  Backup\n ", "docs");
        assert_eq!(s.place_name(&drive), Some("Backup"));
        assert_eq!(
            shown_name(&s.place_names, &drive).as_deref(),
            Some("Backup (Z:)")
        );
        s.rename_place(&drive, "  ", "docs");
        assert_eq!(s.place_name(&drive), None);
        s.rename_place(&drive, "Backup", "docs");
        s.rename_place(&drive, "docs", "docs");
        assert!(
            s.place_names.is_empty(),
            "typing the default name restores it"
        );
    }

    #[test]
    fn a_favorite_name_is_kept_apart_from_place_names() {
        let mut s = Settings::default();
        let folder = PathBuf::from(r"C:\Work\Reports");
        s.rename_favorite(&folder, " Q4 ", "Reports");
        assert_eq!(s.favorite_name(Path::new(r"c:\work\reports\")), Some("Q4"));
        assert!(
            s.place_names.is_empty(),
            "tab titles and the path bar keep the folder's name"
        );
        s.rename_favorite(&folder, "Reports", "Reports");
        assert!(s.favorite_names.is_empty(), "the default name restores it");
        s.rename_favorite(&folder, "Q4", "Reports");
        s.rename_favorite(&folder, "", "Reports");
        assert_eq!(s.favorite_name(&folder), None, "an empty name restores it");
    }

    #[test]
    fn a_drive_keeps_its_letter_out_of_the_editable_name() {
        let place = Place {
            label: "docs (\\\\nas) (Z:)".into(),
            path: PathBuf::from(r"Z:\"),
            kind: PlaceKind::Drive {
                remote: true,
                space: None,
            },
        };
        assert_eq!(drive_default_name(&place), r"docs (\\nas)");
    }
}
