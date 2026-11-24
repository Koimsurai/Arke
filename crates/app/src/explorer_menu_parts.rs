//! Pinned Windows commands without waiting for the whole Windows menu (#129, #278).
//!
//! The Windows menu takes as long as all its extensions together (`win_menu_parts.rs`): 0.25 to
//! 2 s. A command the user pinned to the top of Arke's menu comes from one source, read on its
//! own next to the whole menu, so the pinned row shows as soon as that source answers:
//! - an extension (a `shellex` DLL such as 7-Zip): about 40 ms;
//! - a registry command (`<class>\shell\<verb>`: Git Bash, WizTree): a few ms, run by Windows
//!   alone with its class key;
//! - a packaged app's command (Windows Terminal, VS Code, Zed): 2 to 140 ms, its
//!   `IExplorerCommand` created alone (`win_menu_single.rs`).
//!
//! A packaged command's row is known to show before it answers (the item kinds the app
//! registered it for), so it is drawn right away with the label and icon it had, and a click
//! before the source is in runs the command once it is. An extension's or a registry command's
//! row is drawn ahead where it showed for the same kind of item before (`menu_pin_seen`, noted
//! from its source and from the whole menu): being listed doesn't say it shows, since an
//! extension decides for itself and Windows leaves a registry command out where it doesn't apply
//! ("Run as administrator" listed for a zip, #325). Loading a third-party DLL and asking it for
//! its items takes it 40 ms to a second, and the row was waiting on that. If it turns out not to
//! add it this time, the row goes away when it answers. The whole menu still fills "Show more options", and once it is in, the pinned rows
//! come from it.
//!
//! Where a pinned command comes from isn't in the Windows menu's layout, but each of its items
//! says it with its canonical verb: a registry verb's name, a packaged command's `{CLSID}`. That
//! is learned the first time the command shows in a whole menu and remembered in the settings
//! (`menu_pin_owners`), with the item kinds a packaged command is for (its manifest, and the
//! menus it was seen in). Other items (an extension's, which usually have no verb) are found by
//! reading every extension once, in the background, which is also where each extension's time
//! goes in the log, so a slow one can be named. Commands Windows decides on by itself
//! (`AppliesTo`) are left to the whole menu.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::Context;

use super::{ContextMenu, MenuEntry, find_pinned, pin_key};
use crate::explorer::{AppEvent, Explorer, spawn_thread};
use crate::settings::SettingsStore;
use crate::shell::{ShellError, ShellMenu, ShellMenuEntry};
use crate::win_menu_parts::{COMMAND_OWNER, HandlerKind, MenuHandler, VERB_OWNER, kind_matches};

/// A pinned command learned to come from none of the sources read on their own.
pub(crate) const NO_OWNER: &str = "-";

/// One source's menu, as it arrives.
type PartResult = Result<(ShellMenu, Duration), ShellError>;

/// What comes back about the Windows menu for an open context menu.
pub(crate) enum ShellMenuEvent {
    /// The whole menu, for "Show more options".
    Whole {
        request: u64,
        result: Result<ShellMenu, ShellError>,
    },
    /// Pinned commands that will show (their sources are being read): drawn now.
    Coming { request: u64, keys: Vec<String> },
    /// One source's menu, read on its own for a pinned command.
    Part {
        request: u64,
        name: String,
        /// The source's owner string (`MenuHandler::owner`), to match pinned commands to it.
        owner: String,
        /// The kind of item the menu is for (`win_menu_parts::item_kind`), for an extension
        /// to note where its pinned commands show.
        kind: Option<String>,
        result: PartResult,
    },
    /// Pinned commands of extensions that showed, or didn't, for a kind of item
    /// (pin key, kind, shown).
    PinSeen(Vec<(String, String, bool)>),
    /// Where pinned commands come from (pin key, owner).
    PinOwners(Vec<(String, String)>),
    /// Item kinds packaged commands are for (owner, kinds), added to what is known.
    PinTypes(Vec<(String, Vec<String>)>),
    /// A submenu filled only when opened (`explorer_menu_lazy.rs`).
    Submenu(super::lazy::Filled),
}

impl Explorer {
    /// The menu just opened: read the sources of its pinned commands on their own, beside the
    /// whole menu (`request` is that whole menu's id).
    pub(super) fn request_menu_parts(
        &self,
        menu: &mut ContextMenu,
        request: u64,
        cx: &mut Context<Self>,
    ) {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let owned: Vec<(String, String)> = settings
            .menu_pinned
            .iter()
            .filter_map(|key| {
                settings
                    .menu_pin_owners
                    .get(key)
                    .filter(|o| !o.is_empty() && *o != NO_OWNER)
                    .map(|o| (key.clone(), o.clone()))
            })
            .collect();
        let types = settings.menu_pin_types.clone();
        let seen = settings.menu_pin_seen.clone();
        let Some((paths, background)) = self.menu_paths(menu) else {
            return;
        };
        // "Send to" is read on its own for every menu on items: the host lists it from its
        // folder in a few ms, and it takes the place of Windows' own under "Show more options"
        // (#278).
        let send_to = !background;
        if owned.is_empty() && !send_to {
            return;
        }
        let extended = menu.extended;
        menu.parts_request = Some(request);
        // Rows known to show for this kind of item are drawn now, with the menu, from what was
        // seen before (#325: they came a moment after it, once the registry had been read).
        // The sources still say, and a row that doesn't show this time goes.
        let now = crate::win_menu_parts::item_kind_known(menu.targets.first(), background)
            .map_or_else(Vec::new, |kind| drawn_now(&owned, &types, &seen, &kind));
        if !now.is_empty() {
            menu.coming = now;
            let pinned = SettingsStore::global(cx)
                .read(cx)
                .settings
                .menu_pinned
                .clone();
            menu.sync_pinned(&pinned);
        }
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("menu-extensions", move || {
            let found = shell
                .shell_menu_handlers(&paths, background)
                .unwrap_or_default();
            let is_owner = |owner: &str| owned.iter().any(|(_, o)| o.eq_ignore_ascii_case(owner));
            let mut wanted: Vec<MenuHandler> = found
                .into_iter()
                .filter(|h| match h.kind {
                    HandlerKind::Verb { extended: only } => !only || extended,
                    _ => true,
                })
                .filter(|h| is_owner(&h.owner()) || (send_to && h.is_send_to()))
                .collect();
            if send_to && !wanted.iter().any(MenuHandler::is_send_to) {
                wanted.push(MenuHandler::send_to());
            }
            // Packaged commands: by the item kinds they are for.
            let kind = shell.shell_menu_kind(&paths, background);
            for (_, owner) in &owned {
                if let Some(clsid) = owner.strip_prefix(COMMAND_OWNER)
                    && let Some(kind) = &kind
                    && types.get(owner).is_some_and(|t| kind_matches(t, kind))
                    && !wanted.iter().any(|h| h.owner().eq_ignore_ascii_case(owner))
                {
                    wanted.push(MenuHandler::command(clsid));
                }
            }
            // A packaged command's row (its app registered it for this kind of item), and an
            // extension's or a registry command's where it showed for this kind of item last
            // time. Neither is sure to show from being listed: an extension decides for itself,
            // and Windows leaves a registry command out where it doesn't apply ("Run as
            // administrator" listed for a zip, #325). Drawn ahead there, the row came a moment
            // after the menu and went again, on every menu.
            let shown_here =
                |key: &str| kind.as_ref().is_some_and(|kind| seen_for(&seen, key, kind));
            let keys: Vec<String> = owned
                .iter()
                .filter(|(key, o)| {
                    wanted.iter().any(|h| {
                        h.owner().eq_ignore_ascii_case(o)
                            && (h.kind == HandlerKind::Command || shown_here(key))
                    })
                })
                .map(|(key, _)| key.clone())
                .collect();
            // Sent even when empty: it also takes away rows drawn when the menu opened that
            // have no source here this time.
            let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::Coming {
                request,
                keys,
            }));
            for handler in wanted {
                let (shell, tx, paths) = (Arc::clone(&shell), tx.clone(), paths.clone());
                // Where its pinned rows show, to draw them with the menu next time: an
                // extension's, and a registry command's (not one shown only with Shift held).
                let noted = match handler.kind {
                    HandlerKind::Extension => true,
                    HandlerKind::Verb { extended: only } => !only && !extended,
                    HandlerKind::Command => false,
                };
                let kind = noted.then(|| kind.clone()).flatten();
                spawn_thread("menu-extension", move || {
                    let result =
                        shell.shell_menu_handler(&paths, background, extended, &handler, request);
                    let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::Part {
                        request,
                        owner: handler.owner(),
                        name: handler.name,
                        kind,
                        result,
                    }));
                });
            }
        });
    }

    /// Pinned commands that will show: their rows go in now, ahead of their sources.
    fn handle_menu_coming(&mut self, request: u64, keys: Vec<String>, cx: &mut Context<Self>) {
        let Some(menu) = self
            .context_menu
            .as_mut()
            .filter(|m| m.parts_request == Some(request))
        else {
            return;
        };
        // Rows drawn with the menu stay where they still have a source; the others go.
        let gone: Vec<String> = menu
            .coming
            .iter()
            .filter(|k| !keys.contains(k))
            .cloned()
            .collect();
        menu.drop_coming(&gone);
        for key in keys {
            if !menu.coming.contains(&key) {
                menu.coming.push(key);
            }
        }
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        menu.sync_pinned(&pinned);
        cx.notify();
    }

    /// One source's menu arrived: its pinned commands go up top unless the whole menu is
    /// already in.
    fn handle_menu_part(
        &mut self,
        request: u64,
        name: &str,
        owner: &str,
        kind: Option<String>,
        result: PartResult,
        cx: &mut Context<Self>,
    ) {
        // The pinned commands this source was expected to show.
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let expected: Vec<String> = settings
            .menu_pin_owners
            .iter()
            .filter(|(_, o)| o.eq_ignore_ascii_case(owner))
            .map(|(key, _)| key.clone())
            .collect();
        let pinned = settings.menu_pinned.clone();
        let (part, took) = match result {
            Ok(part) => part,
            Err(e) => {
                log::info!("menu extension {name}: {e}");
                // Gone (an app uninstalled) or failing: its rows drawn ahead go away.
                if let Some(menu) = self
                    .context_menu
                    .as_mut()
                    .filter(|m| m.parts_request == Some(request))
                {
                    menu.drop_coming(&expected);
                    menu.sync_pinned(&pinned);
                    cx.notify();
                }
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
        let entries: Vec<MenuEntry> = named_as_pinned(part.entries, owner, &expected)
            .into_iter()
            .map(|e| MenuEntry::from_shell_in(e, &[], Some(part.session)))
            .collect();
        // Rows drawn ahead that this source turned out not to have (hidden for these items).
        // Only a source read in full says so: one cut short may still have them, and taking
        // that for "not shown here" kept them from being drawn ahead next time (#278).
        let (shown, missing): (Vec<String>, Vec<String>) = expected
            .into_iter()
            .partition(|key| find_pinned(&entries, key).is_some());
        let missing = if part.partial { Vec::new() } else { missing };
        menu.drop_coming(&missing);
        // An extension: where its rows can be drawn ahead next time.
        if let Some(kind) = kind {
            let seen = shown
                .into_iter()
                .map(|key| (key, kind.clone(), true))
                .chain(missing.into_iter().map(|key| (key, kind.clone(), false)))
                .collect();
            cx.defer(move |cx| note_pin_seen(seen, cx));
        }
        if owner.eq_ignore_ascii_case(crate::win_menu_parts::SEND_TO)
            && let Some(MenuEntry::Submenu {
                label, children, ..
            }) = entries.first()
        {
            menu.send_to = Some((label.clone(), children.clone()));
            menu.apply_send_to();
        }
        menu.parts.push((part.session, entries));
        menu.sync_pinned(&pinned);
        self.run_pending_pin(cx);
        self.fill_lazy_for_pins(cx);
        cx.notify();
    }

    /// The whole menu is in: learn, in the background, where each pinned command in it comes
    /// from, for the ones not known yet, and the item kind packaged ones showed for.
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
        let mut unknown: Vec<(String, Option<String>)> = Vec::new();
        let mut packaged: Vec<String> = Vec::new();
        for key in &settings.menu_pinned {
            let Some(entry) = find_pinned(children, key) else {
                continue;
            };
            // A whole submenu has no verb: the extension whose menu has it is found by reading
            // them, as for an extension's item.
            let verb = match entry {
                MenuEntry::Item(item) => item.verb.clone(),
                _ => None,
            };
            match settings.menu_pin_owners.get(key).map(String::as_str) {
                None | Some("") => unknown.push((key.clone(), verb)),
                Some(owner) if owner.starts_with(COMMAND_OWNER) => {
                    packaged.push(owner.to_string());
                }
                Some(_) => {}
            }
        }
        let Some((paths, background)) = self.menu_paths(menu) else {
            return;
        };
        if unknown.is_empty() && packaged.is_empty() {
            return;
        }
        // A menu opened with Shift also has commands that show only with it.
        let extended = menu.extended;
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("menu-learn", move || {
            let kind = shell.shell_menu_kind(&paths, background);
            let (owners, mut types) =
                find_owners(shell.as_ref(), &paths, background, &unknown, request);
            if let Some(kind) = &kind {
                // Extensions' and registry commands learned in this menu showed for this kind
                // of item: their rows are drawn ahead for it from now on.
                let seen: Vec<(String, String, bool)> = owners
                    .iter()
                    .filter(|(_, o)| is_clsid(o) || (o.starts_with(VERB_OWNER) && !extended))
                    .map(|(key, _)| (key.clone(), kind.clone(), true))
                    .collect();
                if !seen.is_empty() {
                    let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::PinSeen(seen)));
                }
            }
            if let Some(kind) = kind {
                let learned = owners
                    .iter()
                    .filter(|(_, o)| o.starts_with(COMMAND_OWNER))
                    .map(|(_, o)| o.clone());
                for owner in packaged.into_iter().chain(learned) {
                    types.push((owner, vec![kind.clone()]));
                }
            }
            if !owners.is_empty() {
                let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::PinOwners(owners)));
            }
            if !types.is_empty() {
                let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::PinTypes(types)));
            }
        });
    }

    /// The Windows menu or a source's menu arrived, or what was learned about pinned commands.
    pub(crate) fn handle_shell_menu_event(
        &mut self,
        event: ShellMenuEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(event) = self.keep_for_early_menu(event) else {
            return;
        };
        match event {
            ShellMenuEvent::Whole { request, result } => {
                self.handle_shell_menu(request, result, cx);
            }
            ShellMenuEvent::Coming { request, keys } => self.handle_menu_coming(request, keys, cx),
            ShellMenuEvent::Part {
                request,
                name,
                owner,
                kind,
                result,
            } => self.handle_menu_part(request, &name, &owner, kind, result, cx),
            ShellMenuEvent::PinSeen(seen) => note_pin_seen(seen, cx),
            ShellMenuEvent::Submenu(filled) => self.handle_submenu_filled(filled, cx),
            // Remembers where pinned commands come from.
            ShellMenuEvent::PinOwners(owners) => {
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(|s| s.menu_pin_owners.extend(owners), cx);
                });
            }
            ShellMenuEvent::PinTypes(types) => {
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(|s| add_pin_types(&mut s.menu_pin_types, types), cx);
                });
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

/// A registry command Windows names itself, as "Run as administrator" (`runas`): the registry
/// has no label for it, so its own menu calls it by the verb. It is the pinned command learned
/// to come from it, named as in the whole Windows menu (#325: taken for missing, its row went
/// until the whole menu was in).
fn named_as_pinned(
    mut entries: Vec<ShellMenuEntry>,
    owner: &str,
    expected: &[String],
) -> Vec<ShellMenuEntry> {
    let (Some(verb), [key]) = (owner.strip_prefix(VERB_OWNER), expected) else {
        return entries;
    };
    if key.contains(" › ") {
        return entries;
    }
    for entry in &mut entries {
        if let ShellMenuEntry::Item { label, .. } = entry
            && label.eq_ignore_ascii_case(verb)
        {
            label.clone_from(key);
        }
    }
    entries
}

/// Pinned commands (pin key, owner) to draw as a menu for items of `kind` opens: a packaged
/// command registered for that kind, any other seen there last time.
fn drawn_now(
    owned: &[(String, String)],
    types: &std::collections::BTreeMap<String, Vec<String>>,
    seen: &std::collections::BTreeMap<String, Vec<String>>,
    kind: &str,
) -> Vec<String> {
    owned
        .iter()
        .filter(|(key, owner)| {
            if owner.starts_with(COMMAND_OWNER) {
                types.get(owner).is_some_and(|t| kind_matches(t, kind))
            } else {
                seen_for(seen, key, kind)
            }
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Whether pinned command `key` showed for items of `kind` last time.
fn seen_for(seen: &std::collections::BTreeMap<String, Vec<String>>, key: &str, kind: &str) -> bool {
    seen.get(key)
        .is_some_and(|kinds| kinds.iter().any(|k| k.eq_ignore_ascii_case(kind)))
}

/// Notes for which kinds of item extensions' pinned commands showed (pin key, kind, shown),
/// saving only a change.
fn note_pin_seen(seen: Vec<(String, String, bool)>, cx: &mut gpui::App) {
    let known = &SettingsStore::global(cx).read(cx).settings.menu_pin_seen;
    let has = |key: &str, kind: &str| {
        known
            .get(key)
            .is_some_and(|kinds| kinds.iter().any(|k| k.eq_ignore_ascii_case(kind)))
    };
    let changes: Vec<(String, String, bool)> = seen
        .into_iter()
        .filter(|(key, kind, shown)| has(key, kind) != *shown)
        .collect();
    if changes.is_empty() {
        return;
    }
    SettingsStore::global(cx).update(cx, |store, cx| {
        store.update(
            |s| {
                for (key, kind, shown) in changes {
                    // Only commands still pinned (one unpinned meanwhile is forgotten).
                    if !s.menu_pinned.contains(&key) {
                        continue;
                    }
                    let kinds = s.menu_pin_seen.entry(key.clone()).or_default();
                    kinds.retain(|k| !k.eq_ignore_ascii_case(&kind));
                    if shown {
                        kinds.push(kind);
                    } else if kinds.is_empty() {
                        s.menu_pin_seen.remove(&key);
                    }
                }
            },
            cx,
        );
    });
}

/// Adds item kinds to what is known of packaged commands, each kind once.
fn add_pin_types(
    known: &mut std::collections::BTreeMap<String, Vec<String>>,
    types: Vec<(String, Vec<String>)>,
) {
    for (owner, kinds) in types {
        let have = known.entry(owner).or_default();
        for kind in kinds {
            if !have.iter().any(|k| k.eq_ignore_ascii_case(&kind)) {
                have.push(kind);
            }
        }
    }
}

/// Owners learned (pin key, owner) and packaged commands' item types (owner, types).
type Learned = (Vec<(String, String)>, Vec<(String, Vec<String>)>);

/// Where each of `keys` (pin key, canonical verb) comes from: a packaged command by its
/// `{CLSID}` verb (with the item types its manifest names), a registry command by a verb the
/// registry has for these items, else the extension whose menu has it, read all at once
/// ([`NO_OWNER`] when none has it). Logs each extension's time. Background thread.
#[allow(
    clippy::too_many_lines,
    reason = "three lookups in order, sharing what is still unplaced"
)]
fn find_owners(
    shell: &dyn crate::shell::Shell,
    paths: &[PathBuf],
    background: bool,
    keys: &[(String, Option<String>)],
    group: u64,
) -> Learned {
    // Set when a lookup failed or timed out: what it would have found is unknown, so the
    // commands still unplaced must be asked again next time, not remembered as ownerless.
    let mut incomplete = false;
    let found = shell
        .shell_menu_handlers(paths, background)
        .unwrap_or_else(|_| {
            incomplete = true;
            Vec::new()
        });
    let mut owners: Vec<(String, String)> = Vec::new();
    let mut types: Vec<(String, Vec<String>)> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for (key, verb) in keys {
        let verb = verb.as_deref().unwrap_or_default().trim();
        if is_clsid(verb) {
            let owner = format!("{COMMAND_OWNER}{verb}");
            types.push((owner.clone(), shell.packaged_item_types(verb)));
            owners.push((key.clone(), owner));
        } else if let Some(h) = found.iter().find(|h| {
            matches!(h.kind, HandlerKind::Verb { .. })
                && !verb.is_empty()
                && h.name.eq_ignore_ascii_case(verb)
        }) {
            owners.push((key.clone(), h.owner()));
        } else {
            rest.push(key.clone());
        }
    }
    if rest.is_empty() {
        return (owners, types);
    }
    let extensions: Vec<MenuHandler> = found
        .into_iter()
        .filter(|h| h.kind == HandlerKind::Extension)
        .collect();
    let results = read_parts(shell, paths, background, extensions, group);
    let mut times = Vec::new();
    for (handler, result) in results {
        match result {
            Ok((menu, took)) => {
                times.push(format!("{} {} ms", handler.name, took.as_millis()));
                let mut have = Vec::new();
                pin_keys(&menu.entries, &[], &mut have);
                for key in &rest {
                    if have.contains(key) && !owners.iter().any(|(k, _)| k == key) {
                        owners.push((key.clone(), handler.owner()));
                    }
                }
                shell.release_shell_menu(menu.session);
            }
            Err(e) => {
                incomplete = true;
                times.push(format!("{} failed ({e})", handler.name));
            }
        }
    }
    // Still unknown: a command in a packaged command's own submenu (NanaZip, WinRAR), which has
    // no verb in the Windows menu. Each packaged command for this kind of item is read once.
    let left: Vec<&String> = rest
        .iter()
        .filter(|key| !owners.iter().any(|(k, _)| k == *key))
        .collect();
    let kind = if left.is_empty() {
        None
    } else {
        shell.shell_menu_kind(paths, background)
    };
    if !left.is_empty() && kind.is_none() {
        incomplete = true;
    }
    if let Some(kind) = kind {
        let commands: Vec<MenuHandler> = shell
            .packaged_commands(&kind)
            .iter()
            .map(|clsid| MenuHandler::command(clsid))
            .collect();
        for (handler, result) in read_parts(shell, paths, background, commands, group) {
            let Ok((menu, took)) = result else {
                incomplete = true;
                continue;
            };
            times.push(format!("{} {} ms", handler.clsid, took.as_millis()));
            let mut have = Vec::new();
            pin_keys(&menu.entries, &[], &mut have);
            for key in &left {
                if have.contains(key) && !owners.iter().any(|(k, _)| k == *key) {
                    let owner = handler.owner();
                    types.push((owner.clone(), shell.packaged_item_types(&handler.clsid)));
                    owners.push(((*key).clone(), owner));
                }
            }
            shell.release_shell_menu(menu.session);
        }
    }
    log::info!(
        "menu extensions for {}: {}",
        paths
            .first()
            .map_or_else(String::new, |p| p.display().to_string()),
        times.join(", ")
    );
    // Absent everywhere only counts when every lookup completed.
    for key in rest.into_iter().filter(|_| !incomplete) {
        if !owners.iter().any(|(k, _)| *k == key) {
            owners.push((key, NO_OWNER.to_string()));
        }
    }
    (owners, types)
}

/// Reads each source's menu on its own, all at once.
fn read_parts(
    shell: &dyn crate::shell::Shell,
    paths: &[PathBuf],
    background: bool,
    handlers: Vec<MenuHandler>,
    group: u64,
) -> Vec<(MenuHandler, PartResult)> {
    std::thread::scope(|scope| {
        let reads: Vec<_> = handlers
            .into_iter()
            .map(|handler| {
                scope.spawn(move || {
                    let result =
                        shell.shell_menu_handler(paths, background, false, &handler, group);
                    (handler, result)
                })
            })
            .collect();
        reads.into_iter().filter_map(|r| r.join().ok()).collect()
    })
}

/// A `{...}` class id, as a packaged command's canonical verb.
fn is_clsid(s: &str) -> bool {
    s.len() == 38 && s.starts_with('{') && s.ends_with('}')
}

/// Pin keys of every command and submenu in `entries` (as [`MenuEntry::from_shell_in`] makes
/// them).
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
                out.push(pin_key(&path));
                pin_keys(children, &path, out);
            }
            ShellMenuEntry::Separator => {}
        }
    }
}
