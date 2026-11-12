//! Settings > About > Backup: export settings to `.json` for another machine, or import a backup.
//!
//! - Exports go to Downloads under a unique name and can be revealed in Arke afterwards.
//! - Import with Browse (the Windows "Open" dialog, run in its own process so its shell
//!   extensions never load into Arke, see `win_picker.rs`), by pasting a path and pressing
//!   Enter, or via the `.json` context menu entry. The previous settings are kept so the import
//!   can be undone.
//! - No save dialog: exports go to Downloads under a unique name.
//! - File I/O runs on background threads; the UI thread only gets the result.
//! - Tabs, window placement and recent folders are per machine. They are not exported and survive an import.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable, SharedString, Window, div, prelude::*,
    px, svg,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::settings::{BackupError, Settings, SettingsStore};
use crate::shell::{KnownFolder, PlaceKind};
use crate::text_input::{InputEvent, TextInput};
use crate::theme;

/// What the last backup action did, shown in the card.
pub(super) enum BackupNote {
    Exported(PathBuf),
    /// Imported file name and the settings from before the import, for undo.
    Imported {
        name: String,
        before: Box<Settings>,
    },
    Undone,
    Failed(String),
}

impl Workspace {
    /// Exports to Downloads, falling back to the user profile folder.
    pub(super) fn export_settings(&mut self, cx: &mut Context<Self>) {
        let explorer = self.active_view().read(cx);
        let places = explorer.places.clone();
        let shell = explorer.services.shell.clone();
        let folder = places
            .iter()
            .find(|p| matches!(p.kind, PlaceKind::Folder(KnownFolder::Downloads)))
            .or_else(|| {
                places
                    .iter()
                    .find(|p| matches!(p.kind, PlaceKind::Folder(KnownFolder::Profile)))
            })
            .map(|p| p.path.clone());
        let Some(folder) = folder else {
            self.set_backup_note(BackupNote::Failed(t!("backup.no_folder").into_owned()), cx);
            return;
        };
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let (tx, rx) = flume::bounded::<Result<PathBuf, BackupError>>(1);
        crate::explorer::spawn_thread("settings-export", move || {
            let result = (|| {
                let bytes = crate::settings::export_json(&settings)?;
                let stem = match shell.local_parts(std::time::SystemTime::now()) {
                    Some([y, m, d, ..]) => t!(
                        "backup.file_stem_dated",
                        date = format!("{y}-{m:02}-{d:02}")
                    )
                    .into_owned(),
                    None => t!("backup.file_stem").into_owned(),
                };
                let name =
                    arke_core::path::unique_file_name(&stem, "json", |n| folder.join(n).exists());
                let path = folder.join(name);
                std::fs::write(&path, bytes).map_err(|source| BackupError::Write {
                    path: path.clone(),
                    source,
                })?;
                Ok(path)
            })();
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let note = match result {
                    Ok(path) => BackupNote::Exported(path),
                    Err(e) => {
                        BackupNote::Failed(t!("backup.export_failed", error = e).into_owned())
                    }
                };
                this.set_backup_note(note, cx);
            });
        })
        .detach();
    }

    /// Imports from `path` on a background thread after checking it is an Arke settings file.
    /// On success applies it and keeps the previous settings.
    pub(super) fn import_settings(path: PathBuf, cx: &mut Context<Self>) {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let (tx, rx) = flume::bounded::<Result<Settings, BackupError>>(1);
        crate::explorer::spawn_thread("settings-import", move || {
            let result = std::fs::read(&path)
                .map_err(|source| match source.kind() {
                    std::io::ErrorKind::NotFound => BackupError::Missing { path: path.clone() },
                    _ => BackupError::Read {
                        path: path.clone(),
                        source,
                    },
                })
                .and_then(|bytes| crate::settings::parse_export(&bytes));
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let note = match result {
                    Ok(imported) => {
                        let store = SettingsStore::global(cx);
                        let before = store.read(cx).settings.clone();
                        store.update(cx, |s, cx| {
                            s.update(|st| *st = st.imported(imported), cx);
                        });
                        BackupNote::Imported {
                            name,
                            before: Box::new(before),
                        }
                    }
                    Err(e) => BackupNote::Failed(
                        t!("backup.import_failed", name = name, error = e).into_owned(),
                    ),
                };
                this.set_backup_note(note, cx);
            });
        })
        .detach();
    }

    /// Undo: restores the pre-import settings, keeping the current tabs, window placement and recent folders.
    fn undo_import(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &mut self.settings_panel else {
            return;
        };
        let Some(BackupNote::Imported { before, .. }) = page.backup.take() else {
            return;
        };
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| *st = st.imported(*before), cx);
        });
        self.set_backup_note(BackupNote::Undone, cx);
    }

    fn set_backup_note(&mut self, note: BackupNote, cx: &mut Context<Self>) {
        if let Some(page) = &mut self.settings_panel {
            page.backup = Some(note);
            page.expanded.insert("backup");
        }
        cx.notify();
    }

    /// Closes Settings, opens Downloads in the active tab and selects the exported file.
    fn reveal_export(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        self.close_settings(&super::CloseSettings, window, cx);
        let name = std::sync::Arc::from(name.to_string_lossy().as_ref());
        let dir = dir.to_path_buf();
        self.active_view().clone().update(cx, |v, cx| {
            v.navigate_selecting(dir, arke_core::NavKind::New, Some(name), cx);
        });
    }

    /// Context menu "Import Arke settings" on a `.json`: opens Settings > About and imports it.
    pub(super) fn import_settings_from_menu(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(&super::OpenSettings, window, cx);
        if let Some(page) = &mut self.settings_panel {
            page.category = super::settings_panel::ABOUT;
            page.expanded.insert("backup");
        }
        Self::import_settings(path, cx);
    }

    /// Import path field. Enter imports (quotes from Explorer's "Copy as path" are stripped), Esc cancels.
    pub(super) fn on_import_input(
        &mut self,
        input: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *event == InputEvent::Changed {
            return;
        }
        if *event == InputEvent::Confirm {
            let text = input.read(cx).committed_text();
            let text = text.trim().trim_matches('"').trim().to_string();
            if !text.is_empty() {
                input.update(cx, |input, cx| input.set_text("", cx));
                Self::import_settings(PathBuf::from(text), cx);
            }
        }
        if let Some(page) = &self.settings_panel {
            window.focus(&page.focus, cx);
        }
    }
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn export_settings_for_test(&mut self, cx: &mut Context<Self>) {
        self.export_settings(cx);
    }

    pub(crate) fn import_settings_for_test(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let _ = self;
        Self::import_settings(path, cx);
    }

    pub(crate) fn undo_import_for_test(&mut self, cx: &mut Context<Self>) {
        self.undo_import(cx);
    }

    /// Text of the result line in the card.
    pub(crate) fn backup_note_for_test(&self) -> Option<String> {
        let note = self.settings_panel.as_ref()?.backup.as_ref()?;
        Some(match note {
            BackupNote::Exported(path) => {
                t!("backup.exported", path = path.display().to_string()).into_owned()
            }
            BackupNote::Imported { name, .. } => t!("backup.imported", name = name).into_owned(),
            BackupNote::Undone => t!("backup.undone").into_owned(),
            BackupNote::Failed(why) => why.clone(),
        })
    }
}

/// Summary text on the right side of the card.
pub(super) fn backup_summary() -> AnyElement {
    div()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .child(t!("backup.summary").to_string())
        .into_any_element()
}

/// Expanded card content: export, import and result.
pub(super) fn backup_body(
    note: Option<&BackupNote>,
    input: &Entity<TextInput>,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let focused = input.focus_handle(cx).is_focused(window);
    let field = div()
        .flex_1()
        .min_w_0()
        .h(theme::H_CTL)
        .px(theme::S_8)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(theme::line())
        })
        .child(input.clone());
    div()
        .flex()
        .flex_col()
        .gap(theme::S_12)
        .child(section(
            t!("backup.export_title"),
            t!("backup.export_desc"),
            button(
                "backup-export",
                t!("backup.export"),
                true,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.export_settings(cx);
                }),
            ),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(theme::S_6)
                .child(section_text(
                    t!("backup.import_title"),
                    t!("backup.import_desc"),
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::S_8)
                        .child(field)
                        .child(button(
                            "backup-browse",
                            t!("settings.browse"),
                            false,
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.browse(
                                    super::settings_panel::Browse::ImportSettings,
                                    window,
                                    cx,
                                );
                            }),
                        )),
                ),
        )
        .children(note.map(|note| note_line(note, cx)))
        .into_any_element()
}

fn section(title: Cow<'static, str>, desc: Cow<'static, str>, control: AnyElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(theme::S_12)
        .child(div().flex_1().min_w_0().child(section_text(title, desc)))
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn section_text(title: Cow<'static, str>, desc: Cow<'static, str>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(theme::S_2)
        .child(div().text_size(theme::FS_13).child(title.into_owned()))
        .child(
            div()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(desc.into_owned()),
        )
        .into_any_element()
}

fn button(
    id: &'static str,
    label: Cow<'static, str>,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .a11y_lazy(gpui::Role::Button, || label.to_string())
        .h(theme::H_CTL_SM)
        .px(theme::S_12)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .when(primary, |d| {
            d.bg(theme::signal())
                .text_color(theme::on_signal())
                .hover(|s| s.opacity(0.9))
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(label.into_owned())
        .on_click(on_click)
        .into_any_element()
}

/// Result line: what happened, plus follow-up actions (reveal, undo).
fn note_line(note: &BackupNote, cx: &mut Context<Workspace>) -> AnyElement {
    let (icon, color, text, action): (Icon, gpui::Rgba, String, Option<AnyElement>) = match note {
        BackupNote::Exported(path) => {
            let path = path.clone();
            (
                Icon::Check,
                theme::signal(),
                t!("backup.exported", path = path.display().to_string()).into_owned(),
                Some(button(
                    "backup-reveal",
                    t!("backup.reveal"),
                    false,
                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.reveal_export(&path, window, cx);
                    }),
                )),
            )
        }
        BackupNote::Imported { name, .. } => (
            Icon::Check,
            theme::signal(),
            t!("backup.imported", name = name).into_owned(),
            Some(button(
                "backup-undo",
                t!("backup.undo"),
                false,
                cx.listener(|this, _: &ClickEvent, _, cx| this.undo_import(cx)),
            )),
        ),
        BackupNote::Undone => (
            Icon::Restore,
            theme::text_muted(),
            t!("backup.undone").into_owned(),
            None,
        ),
        BackupNote::Failed(why) => (Icon::Close, theme::danger(), why.clone(), None),
    };
    div()
        .px(theme::S_12)
        .py(theme::S_8)
        .flex()
        .items_center()
        .gap(theme::S_8)
        .rounded(theme::R_8)
        .bg(theme::wash())
        .child(
            svg()
                .path(icon.path())
                .flex_none()
                .size(theme::ICON_SM)
                .text_color(color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme::FS_12)
                .child(SharedString::from(text)),
        )
        .children(action)
        .min_h(px(36.))
        .into_any_element()
}
