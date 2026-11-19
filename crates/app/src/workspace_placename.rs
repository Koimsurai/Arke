//! Renaming a mapped network drive, network place or saved remote location in place, in the
//! sidebar or on Home. The name is kept in Arke's settings (`place_names.rs`) and nothing in
//! Windows changes.

use std::path::{Path, PathBuf};

use gpui::{AppContext as _, Context, Entity, Focusable as _, Subscription, Window};

use super::Workspace;
use crate::settings::SettingsStore;
use crate::text_input::{InputEvent, TextInput};

/// The place being renamed and the box it is typed in.
pub(super) struct PlaceRename {
    pub(super) path: PathBuf,
    /// The name without a custom one; typing it (or nothing) brings it back.
    default: String,
    pub(super) input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

impl Workspace {
    /// Starts renaming `path` if it is a place that can be (mapped drive, network place, saved
    /// remote location); anything else is left alone.
    pub(super) fn start_place_rename(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        // The box is drawn in the sidebar or on Home, so one of them has to show.
        if settings.sidebar_collapsed && !self.home_open() {
            return;
        }
        let places = self.active_view().read(cx).places.clone();
        let Some(default) = super::sidebar::place_default_name(settings, &places, path) else {
            return;
        };
        let text = settings
            .place_name(path)
            .map_or_else(|| default.clone(), str::to_owned);
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text(text, cx);
            input.select_all_text(cx);
            input
        });
        let focus = input.read(cx).focus_handle(cx);
        let subscriptions =
            [
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Confirm => this.commit_place_rename(window, cx),
                        InputEvent::Cancel => this.cancel_place_rename(window, cx),
                        InputEvent::Changed => {}
                    }
                }),
                // Clicking elsewhere keeps what was typed, like renaming a file.
                cx.on_blur(&focus, window, |this, window, cx| {
                    this.commit_place_rename(window, cx);
                }),
            ];
        self.sidebar_ui.place_rename = Some(PlaceRename {
            path: path.to_path_buf(),
            default,
            input,
            _subscriptions: subscriptions,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn commit_place_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.sidebar_ui.place_rename.take() else {
            return;
        };
        let typed = rename.input.read(cx).committed_text();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| st.rename_place(&rename.path, &typed, &rename.default),
                cx,
            );
        });
        self.focus_active(window, cx);
        cx.notify();
    }

    fn cancel_place_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_ui.place_rename.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// The box to draw in the sidebar for `path`, while it is the place being renamed (Home draws
    /// it instead when it shows).
    pub(super) fn sidebar_rename_box(&self, path: &Path) -> Option<Entity<TextInput>> {
        if self.home_open() {
            return None;
        }
        self.place_rename_input(path)
    }

    /// The input of the rename in progress, if it is for `path`.
    pub(super) fn place_rename_input(&self, path: &Path) -> Option<Entity<TextInput>> {
        self.sidebar_ui
            .place_rename
            .as_ref()
            .filter(|r| crate::place_names::key(&r.path) == crate::place_names::key(path))
            .map(|r| r.input.clone())
    }

    #[cfg(test)]
    pub(crate) fn place_rename_for_test(&self, path: &Path) -> Option<Entity<TextInput>> {
        self.place_rename_input(path)
    }
}
