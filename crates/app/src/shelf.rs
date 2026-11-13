//! The shelf: files and folders collected from anywhere, to copy or move together later.
//!
//! - Items are paths only; nothing is copied until an action runs ("Copy here", "Move here", or
//!   dragging items out of the shelf).
//! - Items that no longer exist stay listed as missing until removed. Existence is rechecked on a
//!   background thread (on activation, after file operations, when the shelf opens).
//! - Items moved out of the shelf leave it once the move is done; copies stay.
//! - Kept across restarts in `%APPDATA%\Arke\shelf.json`, read and written on background threads
//!   only (like the folder views; see `folder_views.rs` for why it isn't in `settings.json`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arke_core::Fs;
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::folder_views::folder_key;

/// A burst of changes becomes one write.
const SAVE_DELAY: Duration = Duration::from_millis(400);
/// Most items kept; adding past this drops the oldest.
pub(crate) const MAX_ITEMS: usize = 2000;
/// Written into the file so a later format can tell old files apart.
const FILE_VERSION: u32 = 1;

/// One item on the shelf.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShelfItem {
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) is_dir: bool,
    /// Gone from its folder at the last check (not stored)
    #[serde(skip)]
    pub(crate) missing: bool,
}

/// The items themselves, without GPUI or files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shelf {
    items: Vec<ShelfItem>,
}

impl Shelf {
    pub(crate) fn items(&self) -> &[ShelfItem] {
        &self.items
    }

    pub(crate) const fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn position(&self, path: &Path) -> Option<usize> {
        let key = folder_key(path);
        self.items.iter().position(|i| folder_key(&i.path) == key)
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.position(path).is_some()
    }

    /// Adds items in order, skipping ones already there (same path in any case or spelling).
    /// Returns how many were new. Past [`MAX_ITEMS`] the oldest go.
    pub(crate) fn add(&mut self, paths: impl IntoIterator<Item = (PathBuf, bool)>) -> usize {
        let mut seen: HashSet<String> = self.items.iter().map(|i| folder_key(&i.path)).collect();
        let mut added = 0;
        for (path, is_dir) in paths {
            if seen.insert(folder_key(&path)) {
                self.items.push(ShelfItem {
                    path,
                    is_dir,
                    missing: false,
                });
                added += 1;
            }
        }
        let over = self.items.len().saturating_sub(MAX_ITEMS);
        self.items.drain(..over);
        added
    }

    /// Removes one item. Returns whether it was there.
    pub(crate) fn remove(&mut self, path: &Path) -> bool {
        let Some(ix) = self.position(path) else {
            return false;
        };
        self.items.remove(ix);
        true
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    /// Paths of the items that still exist, for copying or moving.
    pub(crate) fn present(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter(|i| !i.missing)
            .map(|i| i.path.clone())
            .collect()
    }

    /// Applies an existence check: `found` holds `Some(is_dir)` for items that exist. Items in
    /// `moved` (moved out of the shelf) that are gone now leave it. Returns whether anything
    /// changed that is stored in the file.
    fn apply_check(&mut self, found: &[(PathBuf, Option<bool>)], moved: &HashSet<String>) -> bool {
        let mut stored_changed = false;
        for (path, state) in found {
            let Some(ix) = self.position(path) else {
                continue;
            };
            match state {
                None if moved.contains(&folder_key(path)) => {
                    self.items.remove(ix);
                    stored_changed = true;
                }
                None => self.items[ix].missing = true,
                Some(is_dir) => {
                    let item = &mut self.items[ix];
                    item.missing = false;
                    stored_changed |= item.is_dir != *is_dir;
                    item.is_dir = *is_dir;
                }
            }
        }
        stored_changed
    }
}

/// Errors reading or writing the shelf file. Logged, never shown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ShelfError {
    /// The file exists but could not be read.
    #[error("could not read {}: {source}", .path.display())]
    Read {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a shelf file.
    #[error("{} is not a shelf file: {source}", .path.display())]
    Parse {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
    /// The items could not be serialized.
    #[error("could not encode the shelf: {0}")]
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredFile {
    version: u32,
    #[serde(default)]
    items: Vec<ShelfItem>,
}

/// Parses the file; unreadable items (a mangled hand edit) are skipped one by one.
fn parse(bytes: &[u8], path: &Path) -> Result<Shelf, ShelfError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| ShelfError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let items: Vec<ShelfItem> = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .map_or_default(|list| {
            list.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        });
    let mut shelf = Shelf::default();
    shelf.add(items.into_iter().map(|i| (i.path, i.is_dir)));
    Ok(shelf)
}

/// Reads the file; a missing file is an empty shelf.
pub(crate) fn load(path: &Path) -> Result<Shelf, ShelfError> {
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes, path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Shelf::default()),
        Err(source) => Err(ShelfError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Writes through a temporary file and a rename.
fn save(path: &Path, items: Vec<ShelfItem>) -> Result<(), ShelfError> {
    let write_err = |source| ShelfError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(write_err)?;
    }
    let file = StoredFile {
        version: FILE_VERSION,
        items,
    };
    let json = serde_json::to_vec_pretty(&file).map_err(ShelfError::Encode)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

/// `%APPDATA%\Arke\shelf.json`, next to the settings.
pub(crate) fn default_path() -> Option<PathBuf> {
    crate::settings::default_path().map(|p| p.with_file_name("shelf.json"))
}

/// The app-wide shelf, shared by every window and tab.
pub(crate) struct ShelfStore {
    shelf: Shelf,
    path: Option<PathBuf>,
    /// The file has been read
    loaded: bool,
    /// Cleared or removed before the file arrived: those changes win over the file
    edited: bool,
    /// Keys of items being moved out; they leave once gone from their old place
    moving: HashSet<String>,
    writer: Option<flume::Sender<Vec<ShelfItem>>>,
    save_task: Option<Task<()>>,
    check_task: Option<Task<()>>,
}

struct GlobalShelf(Entity<ShelfStore>);

impl Global for GlobalShelf {}

impl ShelfStore {
    /// Creates the shelf and reads the file in the background. `None` keeps it in memory only
    /// (tests, benchmarks).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            shelf: Shelf::default(),
            path: path.clone(),
            loaded: path.is_none(),
            edited: false,
            moving: HashSet::new(),
            writer: None,
            save_task: None,
            check_task: None,
        });
        cx.set_global(GlobalShelf(store.clone()));
        let Some(path) = path else {
            return store;
        };
        let (tx, rx) = flume::bounded::<Shelf>(1);
        crate::explorer::spawn_thread("shelf-load", move || {
            let shelf = load(&path).unwrap_or_else(|err| {
                log::warn!("{err}");
                Shelf::default()
            });
            let _ = tx.send(shelf);
        });
        let weak = store.downgrade();
        cx.spawn(async move |cx| {
            let shelf = rx.recv_async().await.unwrap_or_default();
            let _ = weak.update(cx, |store, cx| store.finish_loading(shelf, cx));
        })
        .detach();
        store
    }

    /// The app-wide shelf; in memory only if nothing set it up (GPUI tests).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalShelf>() {
            Some(g) => g.0.clone(),
            None => Self::init(None, cx),
        }
    }

    /// The app-wide shelf if it exists, for read-only use.
    pub(crate) fn global_ref(cx: &App) -> Option<&Self> {
        cx.try_global::<GlobalShelf>().map(|g| g.0.read(cx))
    }

    /// Items added before the file arrived go after the loaded ones; a clear or removal in the
    /// meantime drops the file's items.
    fn finish_loading(&mut self, mut loaded: Shelf, cx: &mut Context<Self>) {
        if !self.edited {
            loaded.add(self.shelf.items.drain(..).map(|i| (i.path, i.is_dir)));
            self.shelf = loaded;
        }
        self.loaded = true;
        cx.notify();
    }

    pub(crate) const fn shelf(&self) -> &Shelf {
        &self.shelf
    }

    /// Adds items; returns how many were new.
    pub(crate) fn add(
        &mut self,
        paths: impl IntoIterator<Item = (PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) -> usize {
        let added = self.shelf.add(paths);
        if added > 0 {
            self.schedule_save(cx);
            cx.notify();
        }
        added
    }

    pub(crate) fn remove(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.shelf.remove(path) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.shelf.is_empty() {
            self.shelf.clear();
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// These items are being moved somewhere; once the move is done (and they're gone from the
    /// old place) they leave the shelf.
    pub(crate) fn note_moving(&mut self, paths: &[PathBuf]) {
        self.moving.extend(
            paths
                .iter()
                .filter(|p| self.shelf.contains(p))
                .map(|p| folder_key(p)),
        );
    }

    /// Checks on a background thread which items still exist.
    pub(crate) fn recheck(&mut self, fs: Arc<dyn Fs>, cx: &mut Context<Self>) {
        if self.shelf.is_empty() {
            self.moving.clear();
            return;
        }
        let paths: Vec<PathBuf> = self.shelf.items.iter().map(|i| i.path.clone()).collect();
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("shelf-check", move || {
            let found: Vec<(PathBuf, Option<bool>)> = paths
                .into_iter()
                .map(|p| {
                    let state = fs.stat(&p).ok().map(|e| e.is_dir_like());
                    (p, state)
                })
                .collect();
            let _ = tx.send(found);
        });
        self.check_task = Some(cx.spawn(async move |this, cx| {
            let Ok(found) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |store, cx| {
                let moving = store.moving.clone();
                // A check can land while a move is still running: only moves whose items are gone
                // are done; the rest stay pending for a later check.
                store.moving.retain(|key| {
                    found
                        .iter()
                        .any(|(p, state)| state.is_some() && folder_key(p) == *key)
                });
                let before = store.shelf.clone();
                if store.shelf.apply_check(&found, &moving) {
                    store.edited = true;
                    store.schedule_save(cx);
                }
                if store.shelf != before {
                    cx.notify();
                }
            });
        }));
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
        let items = self.shelf.items.clone();
        let writer = self.writer.get_or_insert_with(|| {
            let (tx, rx) = flume::unbounded::<Vec<ShelfItem>>();
            crate::explorer::spawn_thread("shelf-save", move || {
                while let Ok(mut items) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        items = newer;
                    }
                    if let Err(err) = save(&path, items) {
                        log::warn!("{err}");
                    }
                }
            });
            tx
        });
        if writer.send(items).is_err() {
            self.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn adds_once_in_order() {
        let mut shelf = Shelf::default();
        assert_eq!(
            shelf.add([(p(r"C:\a\x.txt"), false), (p(r"D:\b"), true)]),
            2
        );
        // Same path in another case or with a trailing separator: not added again.
        assert_eq!(
            shelf.add([
                (p(r"c:\A\X.TXT"), false),
                (p(r"D:\b\"), true),
                (p(r"E:\c"), false)
            ]),
            1
        );
        let paths: Vec<_> = shelf.items().iter().map(|i| i.path.clone()).collect();
        assert_eq!(paths, [p(r"C:\a\x.txt"), p(r"D:\b"), p(r"E:\c")]);
        assert!(shelf.remove(Path::new(r"C:\A\x.txt")));
        assert!(!shelf.remove(Path::new(r"C:\a\x.txt")));
        assert_eq!(shelf.len(), 2);
        shelf.clear();
        assert!(shelf.is_empty());
    }

    #[test]
    fn keeps_at_most_the_newest_items() {
        let mut shelf = Shelf::default();
        shelf.add((0..MAX_ITEMS + 5).map(|n| (p(&format!(r"C:\f{n}")), false)));
        assert_eq!(shelf.len(), MAX_ITEMS);
        assert_eq!(shelf.items()[0].path, p(r"C:\f5"));
    }

    #[test]
    fn missing_items_stay_unless_moved_away() {
        let mut shelf = Shelf::default();
        shelf.add([
            (p(r"C:\kept"), false),
            (p(r"C:\gone"), false),
            (p(r"C:\moved"), false),
            (p(r"C:\now_dir"), false),
        ]);
        let moved: HashSet<String> = [folder_key(Path::new(r"C:\moved"))].into();
        let changed = shelf.apply_check(
            &[
                (p(r"C:\kept"), Some(false)),
                (p(r"C:\gone"), None),
                (p(r"C:\moved"), None),
                (p(r"C:\now_dir"), Some(true)),
            ],
            &moved,
        );
        assert!(changed);
        let state: Vec<_> = shelf
            .items()
            .iter()
            .map(|i| (i.path.clone(), i.missing, i.is_dir))
            .collect();
        assert_eq!(
            state,
            [
                (p(r"C:\kept"), false, false),
                (p(r"C:\gone"), true, false),
                (p(r"C:\now_dir"), false, true),
            ]
        );
        assert_eq!(shelf.present(), [p(r"C:\kept"), p(r"C:\now_dir")]);
        // Back again: no longer missing.
        shelf.apply_check(&[(p(r"C:\gone"), Some(false))], &HashSet::new());
        assert!(!shelf.items()[1].missing);
    }

    #[test]
    fn file_round_trip_skips_bad_items() {
        let dir = std::env::temp_dir().join(format!("arke-shelf-test-{}", std::process::id()));
        let file = dir.join("shelf.json");
        let mut shelf = Shelf::default();
        shelf.add([(p(r"C:\a b\c.txt"), false), (p(r"\\nas\share\d"), true)]);
        save(&file, shelf.items().to_vec()).unwrap();
        assert_eq!(load(&file).unwrap(), shelf);
        std::fs::write(
            &file,
            br#"{"version":1,"items":[{"path":"C:\\ok"},{"nope":1},{"path":"C:\\ok"}]}"#,
        )
        .unwrap();
        let loaded = load(&file).unwrap();
        assert_eq!(loaded.len(), 1, "bad and repeated items are skipped");
        assert!(!loaded.items()[0].is_dir);
        assert!(load(&dir.join("absent.json")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
