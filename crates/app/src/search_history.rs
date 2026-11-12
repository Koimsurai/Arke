//! Search history: which files the user opens and which folders they visit, so "This PC" search
//! ranks the copy they actually use first (`arke_core::frecency`, `arke_core::rank`).
//!
//! Local only: `%APPDATA%\Arke\search_history.json`, next to the settings but not inside them (it
//! holds this computer's paths, so it stays out of settings export and backups). At most
//! [`arke_core::frecency::MAX_ITEMS`] items, each fading with time. Settings › Search turns it off
//! (nothing is recorded or used) and clears it.
//!
//! Like `folder_views.rs`, the file is only read and written on background threads: read at
//! startup, written a moment after a change.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arke_core::frecency::{Frecency, FrecencySnapshot, Visit};
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::settings::SettingsStore;

/// A burst of navigations becomes one write.
const SAVE_DELAY: Duration = Duration::from_secs(2);
/// Written into the file so a later format can tell old files apart.
const FILE_VERSION: u32 = 1;

/// Errors reading or writing the history file. Logged, never shown: losing it only makes ranking
/// a little less personal.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoryError {
    /// The file exists but could not be read.
    #[error("could not read {}: {source}", .path.display())]
    Read {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a history file.
    #[error("{} is not a search history file: {source}", .path.display())]
    Parse {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
    /// The history could not be serialized.
    #[error("could not encode the search history: {0}")]
    Encode(#[source] serde_json::Error),
    /// The file could not be written.
    #[error("could not write {}: {source}", .path.display())]
    Write {
        /// The file being written.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StoredVisit {
    path: PathBuf,
    #[serde(default)]
    folder: bool,
    weight: f64,
    /// Unix seconds.
    last: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct StoredFile {
    version: u32,
    items: Vec<StoredVisit>,
}

/// Unix seconds now.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn to_file(frecency: &Frecency) -> StoredFile {
    let mut items: Vec<StoredVisit> = frecency
        .visits()
        .map(|v| StoredVisit {
            path: v.path.clone(),
            folder: v.is_dir,
            weight: v.weight,
            last: v.last,
        })
        .collect();
    // Stable output (a hash map underneath), newest first, for anyone reading the file.
    items.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.path.cmp(&b.path)));
    StoredFile {
        version: FILE_VERSION,
        items,
    }
}

fn parse(bytes: &[u8], path: &Path, now: u64) -> Result<Frecency, HistoryError> {
    // Editors may add a byte order mark when saving UTF-8; serde_json rejects it.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| HistoryError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    // One bad item doesn't lose the rest.
    let visits = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| serde_json::from_value::<StoredVisit>(v.clone()).ok())
                .map(|v| Visit {
                    path: v.path,
                    is_dir: v.folder,
                    weight: v.weight,
                    last: v.last,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Frecency::from_visits(visits, now))
}

/// Reads the file; a missing file is an empty history (first run).
fn load(path: &Path) -> Result<Frecency, HistoryError> {
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes, path, now()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Frecency::default()),
        Err(source) => Err(HistoryError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Writes through a temporary file and a rename, so a crash halfway never leaves a broken file.
fn save(path: &Path, file: &StoredFile) -> Result<(), HistoryError> {
    let write_err = |source| HistoryError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(write_err)?;
    }
    let json = serde_json::to_vec_pretty(file).map_err(HistoryError::Encode)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

/// `%APPDATA%\Arke\search_history.json`, next to the settings (string work only).
pub(crate) fn default_path() -> Option<PathBuf> {
    crate::settings::default_path().map(|p| p.with_file_name("search_history.json"))
}

/// Whether `path` is worth remembering: a local or network path, not a server (SFTP, WebDAV) or
/// a virtual location.
fn recordable(path: &Path) -> bool {
    path.is_absolute() && !arke_core::remote::is_remote(path)
}

/// The app-wide history. Explorers record into it; global search reads a snapshot.
pub(crate) struct SearchHistory {
    frecency: Frecency,
    /// Values as of the last change, handed to queries on background threads.
    snapshot: Arc<FrecencySnapshot>,
    /// The file has been read (always true without a file).
    loaded: bool,
    /// Bumped by [`Self::clear`]: a clear before the file arrived drops what it holds.
    resets: u64,
    path: Option<PathBuf>,
    writer: Option<flume::Sender<StoredFile>>,
    save_task: Option<Task<()>>,
}

struct GlobalHistory(Entity<SearchHistory>);

impl Global for GlobalHistory {}

impl SearchHistory {
    /// Creates the app-wide history and reads the file in the background. `path` is `None` to keep
    /// it in memory only (tests, benchmarks).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            frecency: Frecency::default(),
            snapshot: Arc::default(),
            loaded: path.is_none(),
            resets: 0,
            path: path.clone(),
            writer: None,
            save_task: None,
        });
        cx.set_global(GlobalHistory(store.clone()));
        let Some(path) = path else {
            return store;
        };
        let (tx, rx) = flume::bounded::<Frecency>(1);
        let spawned = std::thread::Builder::new()
            .name("search-history-load".into())
            .spawn(move || {
                let history = load(&path).unwrap_or_else(|err| {
                    // The broken file stays for the user to look at; the next save replaces it.
                    log::warn!("{err}");
                    Frecency::default()
                });
                let _ = tx.send(history);
            });
        if spawned.is_err() {
            store.update(cx, |store, cx| {
                store.finish_loading(Frecency::default(), cx);
            });
            return store;
        }
        let weak = store.downgrade();
        cx.spawn(async move |cx| {
            let history = rx.recv_async().await.unwrap_or_default();
            let _ = weak.update(cx, |store, cx| store.finish_loading(history, cx));
        })
        .detach();
        store
    }

    /// The app-wide history; created in memory only if nothing set it up (GPUI tests).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalHistory>() {
            Some(g) => g.0.clone(),
            None => Self::init(None, cx),
        }
    }

    /// Values for ranking, or `None` while history is off (or nothing is remembered).
    pub(crate) fn snapshot_for_search(cx: &App) -> Option<Arc<FrecencySnapshot>> {
        if SettingsStore::try_global(cx).is_some_and(|s| !s.read(cx).settings.search_history) {
            return None;
        }
        let snapshot = cx
            .try_global::<GlobalHistory>()?
            .0
            .read(cx)
            .snapshot
            .clone();
        (!snapshot.is_empty()).then_some(snapshot)
    }

    /// Visits made while the file was being read are added to what it holds.
    fn finish_loading(&mut self, mut loaded: Frecency, cx: &mut Context<Self>) {
        if self.resets == 0 {
            loaded.absorb(&self.frecency, now());
            self.frecency = loaded;
            self.refresh_snapshot();
        }
        self.loaded = true;
        cx.notify();
    }

    pub(crate) fn len(&self) -> usize {
        self.frecency.len()
    }

    /// Records an opened file or visited folder, unless history is off in settings.
    pub(crate) fn record(path: &Path, is_dir: bool, cx: &mut App) {
        if !recordable(path) || !SettingsStore::global(cx).read(cx).settings.search_history {
            return;
        }
        let store = Self::global(cx);
        store.update(cx, |store, cx| {
            store.frecency.record(path, is_dir, now());
            store.refresh_snapshot();
            store.schedule_save(cx);
        });
    }

    /// Forgets everything ("Clear history").
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.frecency.clear();
        self.resets += 1;
        self.refresh_snapshot();
        self.schedule_save(cx);
        cx.notify();
    }

    fn refresh_snapshot(&mut self) {
        self.snapshot = Arc::new(self.frecency.snapshot(now()));
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.path.is_none() {
            return;
        }
        // A newer change replaces the save that hasn't happened yet.
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |store, _| store.send_to_writer());
        }));
    }

    fn send_to_writer(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        // Not before the file was read: that would overwrite it with only this session's visits.
        if !self.loaded {
            return;
        }
        let file = to_file(&self.frecency);
        let writer = self.writer.get_or_insert_with(|| {
            let (tx, rx) = flume::unbounded::<StoredFile>();
            // One thread writes in order, so an older snapshot never lands after a newer one.
            crate::explorer::spawn_thread("search-history-save", move || {
                while let Ok(mut file) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        file = newer;
                    }
                    if let Err(err) = save(&path, &file) {
                        log::warn!("{err}");
                    }
                }
            });
            tx
        });
        if writer.send(file).is_err() {
            self.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_round_trips_and_skips_bad_items() {
        let mut f = Frecency::default();
        let t = now();
        f.record(Path::new(r"C:\Users\me\report.docx"), false, t);
        f.record(Path::new(r"C:\Users\me\Projects"), true, t);
        let json = serde_json::to_vec(&to_file(&f)).unwrap_or_default();
        let back = parse(&json, Path::new("h.json"), t).unwrap_or_default();
        assert_eq!(back.len(), 2);
        assert!(back.value(Path::new(r"C:\Users\me\Projects"), t) > 0.99);

        let mixed = br#"{"version":1,"items":[{"path":"C:\\a","weight":1.0,"last":4102444800},{"path":7}]}"#;
        let back = parse(mixed, Path::new("h.json"), 4_102_444_800).unwrap_or_default();
        assert_eq!(back.len(), 1);
        assert!(parse(b"not json", Path::new("h.json"), t).is_err());
    }

    #[test]
    fn servers_and_relative_paths_are_not_recorded() {
        assert!(recordable(Path::new(r"C:\Users")));
        assert!(recordable(Path::new(r"\\nas\share\x")));
        assert!(!recordable(Path::new("relative")));
    }
}
