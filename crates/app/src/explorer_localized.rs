//! The localized names of the folders listed (#370), read in the background once a listing is
//! complete: only folders Windows would read a `desktop.ini` for (`localized::may_have_name`),
//! so most folders cost nothing and the list never waits. The names go onto the listing's
//! entries ([`Entry::shown`]), which then sort, filter and jump by them as File Explorer does.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arke_core::Entry;
use gpui::{Context, SharedString};

use super::Explorer;

/// Names read for one listing.
#[derive(Default)]
pub(crate) struct LocalizedNames {
    /// The listing they were read for (folder and generation): a refresh reads them again.
    at: Option<(PathBuf, arke_core::Gen)>,
    /// Folder name to the name shown for it.
    names: HashMap<Arc<str>, Arc<str>>,
}

impl Explorer {
    /// Starts reading the localized names of a listing that just completed; once per listing.
    pub(super) fn follow_localized(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_ref().filter(|m| m.complete) else {
            return;
        };
        let at = (model.path.clone(), model.generation);
        if self.localized.at.as_ref() == Some(&at) {
            return;
        }
        let same_folder = self.localized.at.as_ref().is_some_and(|(p, _)| *p == at.0);
        if !same_folder {
            self.localized.names.clear();
        }
        self.localized.at = Some(at.clone());
        if arke_core::remote::is_remote(&at.0) {
            self.localized.names.clear();
            return;
        }
        let candidates: Vec<Arc<str>> = model
            .entries
            .iter()
            .filter(|e| e.is_dir_like() && crate::localized::may_have_name(e.attributes))
            .map(|e| Arc::clone(&e.name))
            .collect();
        // A refresh of the same folder: the names it had stay on meanwhile.
        self.put_localized_names(cx);
        if candidates.is_empty() {
            if !self.localized.names.is_empty() {
                self.localized.names.clear();
                self.put_localized_names(cx);
            }
            return;
        }
        let (tx, rx) = flume::bounded(1);
        let dir = at.0.clone();
        let fs = Arc::clone(&self.services.fs);
        super::spawn_thread("localized-names", move || {
            let found: HashMap<Arc<str>, Arc<str>> = candidates
                .into_iter()
                .filter_map(|name| {
                    crate::localized::read(&*fs, &dir.join(&*name))
                        .filter(|shown| *shown != *name)
                        .map(|shown| (name, Arc::from(shown)))
                })
                .collect();
            let _ = tx.send(found);
        });
        cx.spawn(async move |this, cx| {
            let Ok(found) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.localized.at.as_ref() == Some(&at) && this.localized.names != found {
                    this.localized.names = found;
                    this.put_localized_names(cx);
                }
            });
        })
        .detach();
    }

    /// Puts the names read onto the listing's entries (and takes them off folders that lost
    /// theirs), as a change to the listing: it sorts them in again.
    fn put_localized_names(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let upserts: Vec<Entry> = model
            .entries
            .iter()
            .filter_map(|e| {
                let want = self.localized.names.get(&e.name).cloned();
                (e.shown != want).then(|| e.with_shown(want))
            })
            .collect();
        if upserts.is_empty() {
            return;
        }
        let generation = model.generation;
        self.apply_folder_delta(
            generation,
            arke_core::Delta {
                upserts,
                removed: Vec::new(),
            },
            cx,
        );
        cx.notify();
    }

    /// A change to the listing keeps the localized names of the folders it brings in again.
    pub(super) fn keep_localized_names(&self, delta: &mut arke_core::Delta) {
        if self.localized.names.is_empty()
            || self.localized.at.as_ref().map(|(p, _)| p) != self.model.as_ref().map(|m| &m.path)
        {
            return;
        }
        for entry in &mut delta.upserts {
            let want = self.localized.names.get(&entry.name).cloned();
            if entry.shown != want {
                *entry = entry.with_shown(want);
            }
        }
    }

    /// Renames the folder `name` (named by its desktop.ini) to `shown` as File Explorer does:
    /// its desktop.ini is rewritten, the folder keeps its own name (#370).
    pub(super) fn rename_shown(&mut self, name: Arc<str>, shown: String, cx: &mut Context<Self>) {
        let Some(dir) = self.current_path().map(|p| p.join(&*name)) else {
            return;
        };
        let (tx, rx) = flume::bounded(1);
        let text = shown.clone();
        super::spawn_thread("rename-shown", move || {
            let _ = tx.send(crate::localized::write_name(&dir, &text));
        });
        cx.spawn(async move |this, cx| {
            let Ok(written) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        this.localized.names.insert(name, Arc::from(shown));
                        this.put_localized_names(cx);
                    }
                    Err(e) => {
                        this.error = Some(
                            rust_i18n::t!("props.rename_failed", error = e.to_string())
                                .into_owned(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The name an item is shown by: a folder's localized name (#370), otherwise its own
    /// (without the extension unless extensions are shown).
    pub(crate) fn shown_name_of(&self, entry: &Entry) -> SharedString {
        match &entry.shown {
            Some(shown) => SharedString::from(shown.to_string()),
            None => super::view::shown_name(entry, self.show_extensions),
        }
    }
}
