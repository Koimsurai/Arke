//! Pinned Windows commands without waiting for the whole Windows menu (#129).
//!
//! The Windows menu takes as long as all its extensions together (`win_menu_parts.rs`). A command
//! the user pinned to the top of Arke's menu comes from one extension, so that extension is read
//! on its own next to the whole menu, and the pinned row shows as soon as it answers: about 20 ms
//! for 7-Zip here, against 0.25 to 2 s for the whole menu. The whole menu still fills "Show more
//! options", and once it is in, the pinned rows come from it.
//!
//! Which extension a pinned command comes from isn't in the Windows menu. It is learned the
//! first time the command shows in a whole menu: every extension that applies is read once, in
//! the background, and the one whose menu has the command is remembered in the settings (an
//! empty entry when none has it, such as a command Windows itself registered). That read is also
//! where each extension's time goes in the log, so a slow one can be named.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::Context;

use super::{ContextMenu, MenuEntry, find_pinned, pin_key};
use crate::explorer::{AppEvent, Explorer, spawn_thread};
use crate::settings::SettingsStore;
use crate::shell::{ShellError, ShellMenu, ShellMenuEntry};
use crate::win_menu_parts::MenuHandler;

/// One extension's menu, as it arrives.
type PartResult = Result<(ShellMenu, Duration), ShellError>;

/// What comes back about the Windows menu for an open context menu.
pub(crate) enum ShellMenuEvent {
    /// The whole menu, for "Show more options".
    Whole {
        request: u64,
        result: Result<ShellMenu, ShellError>,
    },
    /// One extension's menu, read on its own for a pinned command.
    Part {
        request: u64,
        name: String,
        result: PartResult,
    },
    /// Which extension pinned commands come from (empty: none).
    PinOwners(Vec<(String, String)>),
    /// A submenu filled only when opened (`explorer_menu_lazy.rs`).
    Submenu(super::lazy::Filled),
}

impl Explorer {
    /// The menu just opened: read the extensions of its pinned commands on their own, beside the
    /// whole menu (`request` is that whole menu's id).
    pub(super) fn request_menu_parts(
        &self,
        menu: &mut ContextMenu,
        request: u64,
        cx: &mut Context<Self>,
    ) {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let mut owners: Vec<String> = settings
            .menu_pinned
            .iter()
            .filter_map(|key| settings.menu_pin_owners.get(key))
            .filter(|clsid| !clsid.is_empty())
            .cloned()
            .collect();
        owners.sort_unstable_by_key(|c| c.to_ascii_lowercase());
        owners.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let Some((paths, background)) = self.menu_paths(menu) else {
            return;
        };
        if owners.is_empty() {
            return;
        }
        menu.parts_request = Some(request);
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("menu-extensions", move || {
            let Ok(found) = shell.shell_menu_handlers(&paths, background) else {
                return;
            };
            for handler in found
                .into_iter()
                .filter(|h| owners.iter().any(|o| o.eq_ignore_ascii_case(&h.clsid)))
            {
                let (shell, tx, paths) = (Arc::clone(&shell), tx.clone(), paths.clone());
                spawn_thread("menu-extension", move || {
                    let result = shell.shell_menu_handler(&paths, background, &handler, request);
                    let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::Part {
                        request,
                        name: handler.name,
                        result,
                    }));
                });
            }
        });
    }

    /// One extension's menu arrived: its pinned commands go up top unless the whole menu is
    /// already in.
    fn handle_menu_part(
        &mut self,
        request: u64,
        name: &str,
        result: PartResult,
        cx: &mut Context<Self>,
    ) {
        let (part, took) = match result {
            Ok(part) => part,
            Err(e) => {
                log::info!("menu extension {name}: {e}");
                return;
            }
        };
        log::info!("menu extension {name}: {} ms", took.as_millis());
        let Some(menu) = self
            .context_menu
            .as_mut()
            .filter(|m| m.parts_request == Some(request))
        else {
            self.services.shell.release_shell_menu(part.session);
            return;
        };
        let entries = part
            .entries
            .into_iter()
            .map(|e| MenuEntry::from_shell_in(e, &[], Some(part.session)))
            .collect();
        menu.parts.push((part.session, entries));
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        menu.sync_pinned(&pinned);
        self.fill_lazy_for_pins(cx);
        cx.notify();
    }

    /// The whole menu is in: learn, in the background, which extension each pinned command in
    /// it comes from, for the ones not known yet.
    pub(super) fn learn_pin_owners(
        &self,
        menu: &ContextMenu,
        request: u64,
        cx: &mut Context<Self>,
    ) {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let Some(MenuEntry::Submenu { children, .. }) =
            menu.shell_slot.and_then(|ix| menu.entries.get(ix))
        else {
            return;
        };
        let unknown: Vec<String> = settings
            .menu_pinned
            .iter()
            .filter(|key| !settings.menu_pin_owners.contains_key(*key))
            .filter(|key| find_pinned(children, key).is_some())
            .cloned()
            .collect();
        let Some((paths, background)) = self.menu_paths(menu) else {
            return;
        };
        if unknown.is_empty() {
            return;
        }
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("menu-learn", move || {
            let owners = find_owners(shell.as_ref(), &paths, background, &unknown, request);
            let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::PinOwners(owners)));
        });
    }

    /// The Windows menu or an extension's menu arrived, or what was learned about pinned
    /// commands.
    pub(crate) fn handle_shell_menu_event(
        &mut self,
        event: ShellMenuEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ShellMenuEvent::Whole { request, result } => {
                self.handle_shell_menu(request, result, cx);
            }
            ShellMenuEvent::Part {
                request,
                name,
                result,
            } => self.handle_menu_part(request, &name, result, cx),
            ShellMenuEvent::Submenu(filled) => self.handle_submenu_filled(filled, cx),
            // Remembers which extension pinned commands come from.
            ShellMenuEvent::PinOwners(owners) => {
                if !owners.is_empty() {
                    SettingsStore::global(cx).update(cx, |store, cx| {
                        store.update(|s| s.menu_pin_owners.extend(owners), cx);
                    });
                }
            }
        }
    }

    /// Items and whether it's the folder background, as the Windows menu is read for `menu`.
    pub(super) fn menu_paths(&self, menu: &ContextMenu) -> Option<(Vec<PathBuf>, bool)> {
        let background = menu.targets.is_empty();
        let paths: Vec<PathBuf> = if background {
            self.current_path().map(PathBuf::from).into_iter().collect()
        } else {
            menu.targets.iter().map(|(p, _)| p.clone()).collect()
        };
        (!paths.is_empty()).then_some((paths, background))
    }
}

/// Reads every extension that applies, all at once, and names the one each of `keys` comes from
/// (empty: none has it). Logs each extension's time. Background thread.
fn find_owners(
    shell: &dyn crate::shell::Shell,
    paths: &[PathBuf],
    background: bool,
    keys: &[String],
    group: u64,
) -> Vec<(String, String)> {
    let Ok(found) = shell.shell_menu_handlers(paths, background) else {
        return Vec::new();
    };
    let results: Vec<(MenuHandler, PartResult)> = std::thread::scope(|scope| {
        let reads: Vec<_> = found
            .into_iter()
            .map(|handler| {
                scope.spawn(move || {
                    let result = shell.shell_menu_handler(paths, background, &handler, group);
                    (handler, result)
                })
            })
            .collect();
        reads.into_iter().filter_map(|r| r.join().ok()).collect()
    });
    let mut times = Vec::new();
    let mut owners: Vec<(String, String)> = Vec::new();
    for (handler, result) in results {
        match result {
            Ok((menu, took)) => {
                times.push(format!("{} {} ms", handler.name, took.as_millis()));
                let mut have = Vec::new();
                pin_keys(&menu.entries, &[], &mut have);
                for key in keys {
                    if have.contains(key) && !owners.iter().any(|(k, _)| k == key) {
                        owners.push((key.clone(), handler.clsid.clone()));
                    }
                }
                shell.release_shell_menu(menu.session);
            }
            Err(e) => times.push(format!("{} failed ({e})", handler.name)),
        }
    }
    log::info!(
        "menu extensions for {}: {}",
        paths
            .first()
            .map_or_else(String::new, |p| p.display().to_string()),
        times.join(", ")
    );
    for key in keys {
        if !owners.iter().any(|(k, _)| k == key) {
            owners.push((key.clone(), String::new()));
        }
    }
    owners
}

/// Pin keys of every command in `entries` (as [`MenuEntry::from_shell_in`] makes them).
fn pin_keys(entries: &[ShellMenuEntry], parent: &[&str], out: &mut Vec<String>) {
    for entry in entries {
        match entry {
            ShellMenuEntry::Item { label, .. } => {
                let mut path = parent.to_vec();
                path.push(label);
                out.push(pin_key(&path));
            }
            ShellMenuEntry::Submenu {
                label, children, ..
            } => {
                let mut path = parent.to_vec();
                path.push(label);
                pin_keys(children, &path, out);
            }
            ShellMenuEntry::Separator => {}
        }
    }
}
