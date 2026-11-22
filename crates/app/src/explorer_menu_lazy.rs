//! Windows submenus that are filled only when they are opened.
//!
//! The Windows menu is read up front, and each submenu is asked to fill itself then. An extension
//! may fill its submenu only when it is really opened, the way File Explorer's menu asks for it,
//! so it reads as empty; such a submenu used to be dropped. Now it comes with a number
//! (`ShellMenuEntry::Submenu::lazy`) and shows "Loading…" until the menu host fills and reads it.
//! That is asked as the submenu opens (after the hover delay, or at once by click or keyboard),
//! not for every row the pointer passes over: fills queue up in the Windows menu, ahead of a
//! command the user then picks. Its items run in the same Windows menu as the rest.
//!
//! A pinned command may live in such a submenu. The menu then fills that submenu (and one filled
//! inside it, down to the command) as soon as it is read, so the pinned row shows up top as
//! usual.
//!
//! Filling one only changes that submenu's own rows; the rows above it, and pinned commands at
//! the top, stay where they are.

use std::sync::Arc;

use gpui::Context;
use rust_i18n::t;

use super::{
    ContextMenu, MenuEntry, ShellMenuEvent, error_line, find_pinned, pin_key, placeholder,
};
use crate::explorer::{AppEvent, Explorer, spawn_thread};
use crate::settings::SettingsStore;
use crate::shell::{ShellError, ShellMenuEntry};

/// A Windows submenu still to be filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LazySubmenu {
    /// The extension's own menu, for one read on its own; `None` for the whole Windows menu.
    session: Option<u64>,
    /// Its number in that menu.
    index: u32,
    /// Already asked for.
    asked: bool,
}

impl LazySubmenu {
    pub(super) const fn new(session: Option<u64>, index: u32) -> Self {
        Self {
            session,
            index,
            asked: false,
        }
    }
}

/// A filled submenu, as it comes back.
pub(crate) struct Filled {
    /// The context menu it was asked for (`ContextMenu::serial`).
    pub(super) menu: u64,
    pub(super) session: u64,
    pub(super) index: u32,
    pub(super) result: Result<Vec<ShellMenuEntry>, ShellError>,
}

impl ContextMenu {
    /// The entries at `level`, to change.
    fn level_mut(&mut self, level: usize) -> Option<&mut Vec<MenuEntry>> {
        let mut entries = &mut self.entries;
        for &ix in self.open.iter().take(level) {
            match entries.get_mut(ix) {
                Some(MenuEntry::Submenu { children, .. }) => entries = children,
                _ => return None,
            }
        }
        Some(entries)
    }

    /// The rows below the row path `path` (row indexes from the top level).
    fn rows_at_mut(&mut self, path: &[usize]) -> Option<&mut Vec<MenuEntry>> {
        let mut entries = &mut self.entries;
        for &ix in path {
            match entries.get_mut(ix) {
                Some(MenuEntry::Submenu { children, .. }) => entries = children,
                _ => return None,
            }
        }
        Some(entries)
    }

    /// Takes away the row at `at` (row indexes from the top level), which isn't open, keeping
    /// open submenus and the highlight on their rows.
    fn remove_row(&mut self, at: &[usize]) {
        let Some((&ix, parent)) = at.split_last() else {
            return;
        };
        let Some(rows) = self.rows_at_mut(parent).filter(|rows| ix < rows.len()) else {
            return;
        };
        rows.remove(ix);
        let level = parent.len();
        if self.open.len() < level || self.open[..level] != *parent {
            return;
        }
        if let Some(open) = self.open.get_mut(level) {
            if *open > ix {
                *open -= 1;
            }
        } else if let Some(h) = self.highlighted {
            self.highlighted = match h.cmp(&ix) {
                std::cmp::Ordering::Less => Some(h),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(h - 1),
            };
        }
    }

    /// Pinned commands not in the menu yet (`pinned` in pin order).
    fn missing_pins(&self, pinned: &[String]) -> Vec<String> {
        let Some(MenuEntry::Submenu { children, .. }) =
            self.shell_slot.and_then(|ix| self.entries.get(ix))
        else {
            return Vec::new();
        };
        pinned
            .iter()
            .filter(|key| {
                find_pinned(children, key).is_none()
                    && !self
                        .parts
                        .iter()
                        .any(|(_, entries)| find_pinned(entries, key).is_some())
            })
            .cloned()
            .collect()
    }

    /// Submenus still to fill that a pinned command in `wanted` would be in, marked as asked: the
    /// Windows menu each belongs to and its number there.
    fn take_lazy_for_pins(&mut self, wanted: &[String]) -> Vec<(u64, u32)> {
        let mut found: Vec<&mut LazySubmenu> = Vec::new();
        let whole = self.shell_session;
        if let Some(MenuEntry::Submenu { children, .. }) =
            self.shell_slot.and_then(|ix| self.entries.get_mut(ix))
        {
            lazy_under(children, &mut Vec::new(), wanted, &mut found);
        }
        let mut asks: Vec<(u64, u32)> = found
            .into_iter()
            .filter_map(|lazy| {
                let session = lazy.session.or(whole)?;
                lazy.asked = true;
                Some((session, lazy.index))
            })
            .collect();
        for (_, entries) in &mut self.parts {
            let mut found = Vec::new();
            lazy_under(entries, &mut Vec::new(), wanted, &mut found);
            asks.extend(found.into_iter().filter_map(|lazy| {
                let session = lazy.session?;
                lazy.asked = true;
                Some((session, lazy.index))
            }));
        }
        asks
    }
}

/// The unfilled submenus below `entries` (labels `path` down to them) whose rows would hold a
/// command in `wanted`, going into filled ones.
fn lazy_under<'a>(
    entries: &'a mut [MenuEntry],
    path: &mut Vec<String>,
    wanted: &[String],
    out: &mut Vec<&'a mut LazySubmenu>,
) {
    for entry in entries {
        let MenuEntry::Submenu {
            label,
            children,
            lazy,
            shell: true,
            ..
        } = entry
        else {
            continue;
        };
        path.push(label.to_string());
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        let below = format!("{} › ", pin_key(&names));
        if wanted.iter().any(|key| key.starts_with(&below)) {
            match lazy {
                Some(lazy) if !lazy.asked => out.push(lazy),
                _ => lazy_under(children, path, wanted, out),
            }
        }
        path.pop();
    }
}

/// Marks every submenu below `entries` still to be filled as `session`'s number `index` as asked.
fn mark_asked(entries: &mut [MenuEntry], whole: Option<u64>, session: u64, index: u32) {
    for entry in entries {
        if let MenuEntry::Submenu { children, lazy, .. } = entry {
            if let Some(lazy) = lazy
                && lazy.index == index
                && lazy.session.or(whole) == Some(session)
            {
                lazy.asked = true;
            }
            mark_asked(children, whole, session, index);
        }
    }
}

/// Whether a submenu below `entries` waits for `session`/`index` and hasn't been asked for yet.
fn unasked(entries: &[MenuEntry], whole: Option<u64>, session: u64, index: u32) -> bool {
    entries.iter().any(|entry| match entry {
        MenuEntry::Submenu { children, lazy, .. } => {
            lazy.is_some_and(|l| {
                l.index == index && l.session.or(whole) == Some(session) && !l.asked
            }) || unasked(children, whole, session, index)
        }
        _ => false,
    })
}

/// Where the submenu waiting for `session`/`index` is below `entries`: row indexes down to it.
fn find(entries: &[MenuEntry], whole: Option<u64>, session: u64, index: u32) -> Option<Vec<usize>> {
    entries.iter().enumerate().find_map(|(ix, entry)| {
        let MenuEntry::Submenu { children, lazy, .. } = entry else {
            return None;
        };
        if lazy.is_some_and(|l| l.index == index && l.session.or(whole) == Some(session)) {
            return Some(vec![ix]);
        }
        let mut path = find(children, whole, session, index)?;
        path.insert(0, ix);
        Some(path)
    })
}

/// Puts `result` in as the rows of the submenu at `path` below `entries`.
fn put_filled(
    entries: &mut [MenuEntry],
    path: &[usize],
    result: Result<Vec<ShellMenuEntry>, ShellError>,
) {
    // Down to it, collecting the submenu names for pin keys (below "Show more options").
    let mut entries = entries;
    let mut names: Vec<String> = Vec::new();
    for (depth, &ix) in path.iter().enumerate() {
        let Some(MenuEntry::Submenu {
            label,
            children,
            lazy,
            ..
        }) = entries.get_mut(ix)
        else {
            return;
        };
        names.push(label.to_string());
        if depth + 1 == path.len() {
            let own = lazy.and_then(|l| l.session);
            let parent: Vec<&str> = names.iter().map(String::as_str).collect();
            *children = match result {
                Ok(rows) if rows.is_empty() => vec![placeholder(t!("menu.submenu_empty"))],
                Ok(rows) => rows
                    .into_iter()
                    .map(|e| MenuEntry::from_shell_in(e, &parent, own))
                    .collect(),
                Err(e) => vec![error_line(t!("menu.load_failed", error = e))],
            };
            return;
        }
        entries = children;
    }
}

impl Explorer {
    /// Row `ix` at `level` is a submenu opening: if Windows fills it only when opened, ask the
    /// menu host to, once.
    pub(super) fn fill_lazy_submenu(&mut self, level: usize, ix: usize) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let (whole, serial) = (menu.shell_session, menu.serial);
        let Some(MenuEntry::Submenu {
            lazy: Some(lazy), ..
        }) = menu
            .level_mut(level)
            .and_then(|entries| entries.get_mut(ix))
        else {
            return;
        };
        let Some(session) = lazy.session.or(whole).filter(|_| !lazy.asked) else {
            return;
        };
        lazy.asked = true;
        let index = lazy.index;
        // A submenu pinned up top is the same Windows submenu as the one it was copied from
        // (#88): that one isn't asked again.
        mark_asked(&mut menu.entries, whole, session, index);
        for (_, entries) in &mut menu.parts {
            mark_asked(entries, None, session, index);
        }
        self.ask_fill(serial, session, index);
    }

    /// The menu or one of its parts was read, or a submenu filled: fill the submenus pinned
    /// commands are still hidden in, without waiting for them to be opened.
    pub(super) fn fill_lazy_for_pins(&mut self, cx: &mut Context<Self>) {
        // A pinned submenu opened while it was drawn ahead of its source, now in: filled as if
        // it had just been opened (#88).
        let open_pin = self.context_menu.as_ref().and_then(|m| {
            let (ix, slot) = (*m.open.first()?, m.shell_slot?);
            (ix < slot && ix + m.pinned_rows >= slot).then_some(ix)
        });
        if let Some(ix) = open_pin {
            self.fill_lazy_submenu(0, ix);
        }
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let wanted = menu.missing_pins(&pinned);
        if wanted.is_empty() {
            return;
        }
        let serial = menu.serial;
        for (session, index) in menu.take_lazy_for_pins(&wanted) {
            self.ask_fill(serial, session, index);
        }
    }

    /// The whole Windows menu came with submenus still filling, not waited for (`indexes`,
    /// #278): filled now, in the background, so they are usually in by the time they open.
    pub(super) fn fill_left_filling(&mut self, indexes: &[u32]) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let (Some(session), serial) = (menu.shell_session, menu.serial) else {
            return;
        };
        let mut asks = Vec::new();
        for &index in indexes {
            // One a pinned command is in is asked for already.
            if unasked(&menu.entries, Some(session), session, index) {
                mark_asked(&mut menu.entries, Some(session), session, index);
                asks.push(index);
            }
        }
        for index in asks {
            self.ask_fill(serial, session, index);
        }
    }

    fn ask_fill(&self, serial: u64, session: u64, index: u32) {
        let shell = Arc::clone(&self.services.shell);
        let tx = self.events_tx.clone();
        spawn_thread("shell-submenu", move || {
            let result = shell.fill_shell_submenu(session, index);
            let _ = tx.send(AppEvent::ShellMenu(ShellMenuEvent::Submenu(Filled {
                menu: serial,
                session,
                index,
                result,
            })));
        });
    }

    /// A submenu was filled: its rows replace "Loading…" if the menu is still open, and pinned
    /// commands in it go up top.
    pub(super) fn handle_submenu_filled(&mut self, filled: Filled, cx: &mut Context<Self>) {
        let Some(menu) = self
            .context_menu
            .as_mut()
            .filter(|m| m.serial == filled.menu)
        else {
            return;
        };
        let Some(slot) = menu.shell_slot else {
            return;
        };
        let whole = menu.shell_session;
        let in_whole = match menu.entries.get(slot) {
            Some(MenuEntry::Submenu { children, .. }) => {
                find(children, whole, filled.session, filled.index)
            }
            _ => None,
        };
        if let Some(path) = in_whole {
            let mut open_path = vec![slot];
            open_path.extend(&path);
            // Left filling and now in with nothing Arke can show (owner-drawn items): it goes,
            // unless it is open.
            let nothing = matches!(&filled.result, Ok(rows) if rows.is_empty());
            if nothing
                && menu.left_filling.contains(&filled.index)
                && !menu.open.starts_with(&open_path)
            {
                menu.remove_row(&open_path);
            } else if let Some(MenuEntry::Submenu { children, .. }) = menu.entries.get_mut(slot) {
                put_filled(children, &path, filled.result);
            }
            // Keyboard already went in while it held only "Loading…": select its first item.
            if menu.keyboard && menu.highlighted.is_none() && menu.open == open_path {
                menu.highlighted = menu
                    .level(menu.open.len())
                    .iter()
                    .position(MenuEntry::selectable);
            }
        } else {
            // One of the extensions read on their own for a pinned command.
            let Some((entries, path)) = menu.parts.iter_mut().find_map(|(_, entries)| {
                let path = find(entries, None, filled.session, filled.index)?;
                Some((entries, path))
            }) else {
                return;
            };
            put_filled(entries, &path, filled.result);
        }
        let pinned = SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone();
        menu.sync_pinned(&pinned);
        self.fill_lazy_for_pins(cx);
        cx.notify();
    }
}
