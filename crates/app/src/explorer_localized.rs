//! The localized names of the folders listed (#370), read in the background once a listing is
//! complete: only folders Windows would read a `desktop.ini` for (`localized::may_have_name`),
//! so most folders cost nothing and the list never waits.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, SharedString};

use super::Explorer;

/// Names read for one listing.
#[derive(Default)]
pub(crate) struct LocalizedNames {
    /// The listing they were read for (folder and generation): a refresh reads them again.
    at: Option<(PathBuf, arke_core::Gen)>,
    /// Folder name to the name shown for it.
    names: HashMap<Arc<str>, SharedString>,
}

impl LocalizedNames {
    pub(crate) fn get(&self, name: &str) -> Option<&SharedString> {
        self.names.get(name)
    }
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
            return;
        }
        let candidates: Vec<Arc<str>> = model
            .entries
            .iter()
            .filter(|e| e.is_dir_like() && crate::localized::may_have_name(e.attributes))
            .map(|e| Arc::clone(&e.name))
            .collect();
        if candidates.is_empty() {
            self.localized.names.clear();
            return;
        }
        let (tx, rx) = flume::bounded(1);
        let dir = at.0.clone();
        let fs = Arc::clone(&self.services.fs);
        super::spawn_thread("localized-names", move || {
            let found: HashMap<Arc<str>, SharedString> = candidates
                .into_iter()
                .filter_map(|name| {
                    crate::localized::read(&*fs, &dir.join(&*name))
                        .filter(|shown| *shown != *name)
                        .map(|shown| (name, SharedString::from(shown)))
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
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The name an item is shown by: a folder's localized name (#370), otherwise its own
    /// (without the extension unless extensions are shown).
    pub(crate) fn shown_name_of(&self, entry: &arke_core::Entry) -> SharedString {
        self.localized
            .get(&entry.name)
            .cloned()
            .unwrap_or_else(|| super::view::shown_name(entry, self.show_extensions))
    }
}
