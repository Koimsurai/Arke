//! Windows submenus that are filled only when they are opened.
//!
//! The Windows menu is read up front, and each submenu is asked to fill itself then. An extension
//! may fill its submenu only when it is really opened, the way File Explorer's menu asks for it,
//! so it reads as empty; such a submenu used to be dropped. Now it comes with a number (`ShellMenuEntry::Submenu::lazy`) and shows "Loading…" until the menu host fills and
//! reads it. That is asked as soon as the pointer rests on the row, so it is usually ready by
//! the time the submenu opens. Its items run in the same Windows menu as the rest.
//!
//! Filling one only changes that submenu's own rows; the rows above it, and pinned commands at
//! the top, stay where they are.

use std::sync::Arc;

use gpui::Context;
use rust_i18n::t;

use super::{ContextMenu, MenuEntry, ShellMenuEvent, error_line, placeholder};
use crate::explorer::{AppEvent, Explorer, spawn_thread};
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

impl Explorer {
    /// Row `ix` at `level` is a submenu about to open: if Windows fills it only when opened, ask
    /// the menu host to, once.
    pub(super) fn fill_lazy_submenu(&mut self, level: usize, ix: usize) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let (serial, whole) = (menu.serial, menu.shell_session);
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

    /// A submenu was filled: its rows replace "Loading…" if the menu is still open.
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
        let Some(MenuEntry::Submenu { children: top, .. }) = menu.entries.get_mut(slot) else {
            return;
        };
        let Some(path) = find(top, menu.shell_session, filled.session, filled.index) else {
            return;
        };
        // Down to it, collecting the submenu names for pin keys (below "Show more options").
        let mut entries = top;
        let mut names: Vec<String> = Vec::new();
        let mut target = None;
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
                target = Some((children, lazy.and_then(|l| l.session)));
                break;
            }
            entries = children;
        }
        let Some((children, own)) = target else {
            return;
        };
        let parent: Vec<&str> = names.iter().map(String::as_str).collect();
        *children = match filled.result {
            Ok(rows) if rows.is_empty() => vec![placeholder(t!("menu.submenu_empty"))],
            Ok(rows) => rows
                .into_iter()
                .map(|e| MenuEntry::from_shell_in(e, &parent, own))
                .collect(),
            Err(e) => vec![error_line(t!("menu.load_failed", error = e))],
        };
        // Keyboard already went in while it held only "Loading…": select its first item.
        let mut open_path = vec![slot];
        open_path.extend(&path);
        if menu.keyboard && menu.highlighted.is_none() && menu.open == open_path {
            menu.highlighted = menu
                .level(menu.open.len())
                .iter()
                .position(MenuEntry::selectable);
        }
        cx.notify();
    }
}
