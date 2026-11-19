//! Custom names for mapped network drives, network places and saved remote locations.
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
        let name: String = name
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .chars()
            .take(NAME_MAX)
            .collect();
        if name.is_empty() || name == default {
            self.place_names.remove(&key(path));
        } else {
            self.place_names.insert(key(path), name);
        }
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
