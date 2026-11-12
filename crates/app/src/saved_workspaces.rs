//! Saved workspaces: a named set of tabs (with their split panes and folders) to come back to,
//! like "Work" or "Photos". Opening one replaces the open tabs except the pinned ones.
//!
//! Stored in its own file, `%APPDATA%\Arke\workspaces.json`, next to `settings.json` (like
//! `folder_views.json`): the entries are this computer's paths, so they stay out of settings
//! export and import, and a damaged file only loses the saved workspaces. The file is read and
//! written on background threads only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::settings::SessionTab;

/// A burst of changes (renaming twice, deleting a few) becomes one write.
const SAVE_DELAY: Duration = Duration::from_millis(300);
/// Written into the file so a later format can tell old files apart.
const FILE_VERSION: u32 = 1;
/// Longest name kept; the list is a menu, not a document.
pub(crate) const NAME_MAX: usize = 60;

/// One saved workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedWorkspace {
    pub(crate) name: String,
    pub(crate) tabs: Vec<SessionTab>,
    /// The tab that was active when it was saved.
    #[serde(default)]
    pub(crate) active: usize,
}

/// Why a name can't be used. Display is the localized message shown under the name field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NameProblem {
    #[error("{}", rust_i18n::t!("workspaces.name_empty"))]
    Empty,
    #[error("{}", rust_i18n::t!("workspaces.name_taken"))]
    Taken,
}

/// Errors reading or writing the file. They are logged, never shown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspacesError {
    #[error("could not read {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a workspaces file: {source}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not encode the workspaces: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("could not write {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The name as it will be stored: trimmed, inner runs of whitespace kept, capped at [`NAME_MAX`].
pub(crate) fn clean_name(name: &str) -> String {
    name.trim()
        .chars()
        .take(NAME_MAX)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The saved workspaces in the order they were first saved, without GPUI or files.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Workspaces {
    list: Vec<SavedWorkspace>,
}

impl Workspaces {
    pub(crate) fn list(&self) -> &[SavedWorkspace] {
        &self.list
    }

    /// The workspace called `name` (any case).
    pub(crate) fn get(&self, name: &str) -> Option<&SavedWorkspace> {
        self.list.iter().find(|w| same_name(&w.name, name))
    }

    /// Saves `tabs` under `name`. An existing workspace of that name (any case) is replaced in
    /// place, keeping its spot in the list. Tabs without a folder are not saved.
    pub(crate) fn save(
        &mut self,
        name: &str,
        tabs: Vec<SessionTab>,
        active: usize,
    ) -> Result<String, NameProblem> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err(NameProblem::Empty);
        }
        let tabs: Vec<SessionTab> = tabs.into_iter().filter(|t| !t.panes.is_empty()).collect();
        let saved = SavedWorkspace {
            active: active.min(tabs.len().saturating_sub(1)),
            name: name.clone(),
            tabs,
        };
        match self.list.iter_mut().find(|w| same_name(&w.name, &name)) {
            Some(old) => *old = saved,
            None => self.list.push(saved),
        }
        Ok(name)
    }

    /// Renames `from` to `to`. Changing only the case of a name is allowed.
    pub(crate) fn rename(&mut self, from: &str, to: &str) -> Result<String, NameProblem> {
        let to = clean_name(to);
        if to.is_empty() {
            return Err(NameProblem::Empty);
        }
        if self
            .list
            .iter()
            .any(|w| same_name(&w.name, &to) && !same_name(&w.name, from))
        {
            return Err(NameProblem::Taken);
        }
        if let Some(w) = self.list.iter_mut().find(|w| same_name(&w.name, from)) {
            w.name.clone_from(&to);
        }
        Ok(to)
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.list.retain(|w| !same_name(&w.name, name));
    }

    /// A name for a new workspace that isn't taken: `base`, then `base (2)`, `base (3)`, ...
    pub(crate) fn free_name(&self, base: &str) -> String {
        arke_core::path::unique_name(base, |n| self.get(n).is_some())
    }
}

/// The file as written.
#[derive(Serialize)]
struct StoredFile<'a> {
    version: u32,
    workspaces: &'a [SavedWorkspace],
}

/// Parse the file. Entries that can't be read (a mangled hand edit) are skipped one by one, and a
/// name listed twice keeps its first entry.
fn parse(bytes: &[u8], path: &Path) -> Result<Workspaces, WorkspacesError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| WorkspacesError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let mut out = Workspaces::default();
    for item in value
        .get("workspaces")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Ok(saved) = serde_json::from_value::<SavedWorkspace>(item.clone()) else {
            continue;
        };
        let name = clean_name(&saved.name);
        if name.is_empty() || out.get(&name).is_some() {
            continue;
        }
        out.list.push(SavedWorkspace { name, ..saved });
    }
    Ok(out)
}

/// Read the file; a missing file is simply no workspaces yet.
fn load(path: &Path) -> Result<Workspaces, WorkspacesError> {
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes, path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Workspaces::default()),
        Err(source) => Err(WorkspacesError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write through a temporary file and a rename, so a crash halfway never leaves a broken file.
fn save(path: &Path, list: &[SavedWorkspace]) -> Result<(), WorkspacesError> {
    let write_err = |source| WorkspacesError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(write_err)?;
    }
    let json = serde_json::to_vec_pretty(&StoredFile {
        version: FILE_VERSION,
        workspaces: list,
    })
    .map_err(WorkspacesError::Encode)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

/// `%APPDATA%\Arke\workspaces.json`, next to the settings (string work only).
pub(crate) fn default_path() -> Option<PathBuf> {
    crate::settings::default_path().map(|p| p.with_file_name("workspaces.json"))
}

/// The app-wide list. Windows read it to draw the Workspaces panel; it saves itself.
pub(crate) struct WorkspaceStore {
    workspaces: Workspaces,
    /// Changed before the file finished loading (kept over what was on disk).
    dirty: bool,
    path: Option<PathBuf>,
    writer: Option<flume::Sender<Vec<SavedWorkspace>>>,
    save_task: Option<Task<()>>,
}

struct GlobalWorkspaces(Entity<WorkspaceStore>);

impl Global for GlobalWorkspaces {}

impl WorkspaceStore {
    /// Create the app-wide list and read the file in the background. `path` is `None` to keep it
    /// in memory only (tests, benchmarks).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            workspaces: Workspaces::default(),
            dirty: false,
            path: path.clone(),
            writer: None,
            save_task: None,
        });
        cx.set_global(GlobalWorkspaces(store.clone()));
        let Some(path) = path else {
            return store;
        };
        let (tx, rx) = flume::bounded::<Workspaces>(1);
        crate::explorer::spawn_thread("workspaces-load", move || {
            let list = load(&path).unwrap_or_else(|err| {
                // Keep the broken file for the user to look at; it is replaced on the next save
                log::warn!("{err}");
                Workspaces::default()
            });
            let _ = tx.send(list);
        });
        let weak = store.downgrade();
        cx.spawn(async move |cx| {
            let Ok(loaded) = rx.recv_async().await else {
                return;
            };
            let _ = weak.update(cx, |store, cx| {
                if !store.dirty {
                    store.workspaces = loaded;
                }
                cx.notify();
            });
        })
        .detach();
        store
    }

    /// The app-wide list; created in memory only if nothing set it up (GPUI tests).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalWorkspaces>() {
            Some(g) => g.0.clone(),
            None => Self::init(None, cx),
        }
    }

    pub(crate) const fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// Change the list and save soon.
    pub(crate) fn update<R>(
        &mut self,
        f: impl FnOnce(&mut Workspaces) -> R,
        cx: &mut Context<Self>,
    ) -> R {
        let before = self.workspaces.clone();
        let out = f(&mut self.workspaces);
        if self.workspaces != before {
            self.dirty = true;
            self.schedule_save(cx);
            cx.notify();
        }
        out
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.path.is_none() {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |store, _| store.send_to_writer());
        }));
    }

    fn send_to_writer(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let list = self.workspaces.list.clone();
        let writer = self.writer.get_or_insert_with(|| {
            let (tx, rx) = flume::unbounded::<Vec<SavedWorkspace>>();
            // One thread writes in order, so an older list can never land after a newer one.
            crate::explorer::spawn_thread("workspaces-save", move || {
                while let Ok(mut list) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        list = newer;
                    }
                    if let Err(err) = save(&path, &list) {
                        log::warn!("{err}");
                    }
                }
            });
            tx
        });
        if writer.send(list).is_err() {
            self.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(paths: &[&str]) -> SessionTab {
        SessionTab {
            panes: paths.iter().map(PathBuf::from).collect(),
            active_pane: 0,
            ratio: 0.5,
            pinned: false,
            synced: false,
        }
    }

    #[test]
    fn saving_under_a_taken_name_replaces_in_place() {
        let mut w = Workspaces::default();
        assert_eq!(
            w.save("  Work  ", vec![tab(&[r"C:\a"])], 0),
            Ok("Work".into())
        );
        w.save("Photos", vec![tab(&[r"D:\p"])], 0).unwrap();
        w.save("work", vec![tab(&[r"C:\b"]), tab(&[r"C:\c", r"C:\d"])], 5)
            .unwrap();
        let names: Vec<&str> = w.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["work", "Photos"], "same spot, the new spelling");
        let work = w.get("WORK").unwrap();
        assert_eq!(work.tabs.len(), 2);
        assert_eq!(work.active, 1, "active is clamped to the tabs");
        assert_eq!(
            w.save("   ", vec![tab(&[r"C:\a"])], 0),
            Err(NameProblem::Empty)
        );
    }

    #[test]
    fn tabs_without_a_folder_are_left_out() {
        let mut w = Workspaces::default();
        w.save("x", vec![tab(&[]), tab(&[r"C:\a"])], 1).unwrap();
        let x = w.get("x").unwrap();
        assert_eq!(x.tabs, vec![tab(&[r"C:\a"])]);
        assert_eq!(x.active, 0);
    }

    #[test]
    fn rename_checks_for_empty_and_taken_names() {
        let mut w = Workspaces::default();
        w.save("A", vec![tab(&[r"C:\a"])], 0).unwrap();
        w.save("B", vec![tab(&[r"C:\b"])], 0).unwrap();
        assert_eq!(w.rename("A", "b"), Err(NameProblem::Taken));
        assert_eq!(w.rename("A", " "), Err(NameProblem::Empty));
        assert_eq!(w.rename("A", "a"), Ok("a".into()), "only the case changes");
        assert_eq!(w.rename("a", "Archive"), Ok("Archive".into()));
        assert!(w.get("A").is_none());
        assert_eq!(w.get("archive").unwrap().tabs, vec![tab(&[r"C:\a"])]);
        w.remove("ARCHIVE");
        assert_eq!(w.list().len(), 1);
    }

    #[test]
    fn names_are_trimmed_and_capped() {
        let long = "x".repeat(NAME_MAX + 10);
        assert_eq!(clean_name(&long).chars().count(), NAME_MAX);
        assert_eq!(clean_name("  two  words "), "two  words");
        let mut w = Workspaces::default();
        assert_eq!(w.free_name("Workspace"), "Workspace");
        w.save("Workspace", vec![tab(&[r"C:\a"])], 0).unwrap();
        assert_ne!(w.free_name("Workspace"), "Workspace");
    }

    #[test]
    fn file_round_trip_and_odd_files() {
        let dir = std::env::temp_dir().join(format!("arke-workspaces-{}", std::process::id()));
        let path = dir.join("workspaces.json");
        assert!(load(&path).unwrap().list().is_empty(), "no file yet");
        let mut w = Workspaces::default();
        let mut split = tab(&[r"C:\left", r"D:\right"]);
        split.pinned = true;
        split.synced = true;
        split.active_pane = 1;
        w.save("Both", vec![split, tab(&[r"C:\x"])], 1).unwrap();
        save(&path, w.list()).unwrap();
        assert_eq!(load(&path).unwrap(), w);
        std::fs::remove_dir_all(&dir).unwrap();

        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            br#"{"version": 1, "workspaces": [
                {"name": "One", "tabs": [{"panes": ["C:\\a"]}]},
                {"name": 7},
                {"name": "one", "tabs": []},
                {"name": "  ", "tabs": []},
                {"name": "Two", "tabs": [], "active": 3}
            ]}"#,
        );
        let parsed = parse(&bytes, &path).unwrap();
        let names: Vec<&str> = parsed.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["One", "Two"]);
        assert!((parsed.get("one").unwrap().tabs[0].ratio - 0.5).abs() < f32::EPSILON);
        assert!(matches!(
            parse(b"{not json", &path),
            Err(WorkspacesError::Parse { .. })
        ));
    }
}
