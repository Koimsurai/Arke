//! Per-folder view memory, like Windows File Explorer: the view mode, icon size and sort the user
//! picked in a folder come back whenever that folder is opened again, in any tab and after a restart.
//!
//! Stored in its own file, `%APPDATA%\Arke\folder_views.json`, next to `settings.json` rather than
//! inside it:
//! - it can hold up to [`MAX_FOLDERS`] entries; inside `settings.json` they would bury the settings a
//!   person might want to read or edit by hand;
//! - `SettingsStore::update` clones, compares and rewrites the whole settings file on every change,
//!   and a view is recorded far more often than a setting is changed;
//! - the entries are this computer's paths, so they don't belong in a settings backup: keeping them
//!   out of `settings.json` keeps them out of export and import without special cases;
//! - a damaged file only loses the remembered views, never the settings.
//!
//! Like the settings, the file is read and written on background threads only (never the UI
//! thread): reads at startup, writes after a short pause that merges a burst of changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use arke_core::sort::{MAX_SORT_KEYS, SortColumn, SortKey};
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::explorer::{IconSize, ViewMode};
use crate::settings::SortBy;

/// How many folders are remembered; the least recently used one is forgotten first.
pub(crate) const MAX_FOLDERS: usize = 1000;
/// A burst of changes (clicking through sort columns) becomes one write.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// How long startup waits for the file (read in the background, usually 1–2 ms), so the first
/// window already opens its folder the remembered way. A slow `%APPDATA%` doesn't hold up the
/// window: the views are applied once they arrive.
const STARTUP_WAIT: Duration = Duration::from_millis(60);
/// Written into the file so a later format can tell old files apart.
const FILE_VERSION: u32 = 1;

/// What is remembered for one folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FolderView {
    pub(crate) view: ViewMode,
    /// Icon size, also kept while the folder shows another view (Ctrl+Shift+2 goes back to it).
    pub(crate) icons: IconSize,
    pub(crate) column: SortColumn,
    pub(crate) ascending: bool,
    /// Further sort keys (Shift+click in the header).
    pub(crate) then: [Option<SortKey>; MAX_SORT_KEYS - 1],
}

/// The key a folder is remembered under. Windows paths are case-insensitive and the same folder
/// can be typed with a trailing separator, forward slashes, `.` / `..` or the `\\?\` prefix, so all
/// of those have to land on one entry. String work only; the file system is never asked.
pub(crate) fn folder_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    };
    let normal = arke_core::path::normalize(Path::new(&text));
    normal
        .to_string_lossy()
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Errors reading or writing the folder views file. They are logged, never shown: losing the
/// remembered views is an inconvenience, not something to stop the user for.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FolderViewsError {
    /// The file exists but could not be read.
    #[error("could not read {}: {source}", .path.display())]
    Read {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not JSON.
    #[error("{} is not a folder views file: {source}", .path.display())]
    Parse {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
    /// The views could not be serialized.
    #[error("could not encode the folder views: {0}")]
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

/// One folder as stored in the file. The path is kept as the user last saw it (not the lowercase
/// key) so the file stays readable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StoredFolder {
    path: PathBuf,
    view: ViewMode,
    /// Missing in files from before icon sizes, or unknown: large icons, as it always was. Explorer's
    /// four sizes are stored by name, sizes in between (the slider) as pixels.
    #[serde(default, deserialize_with = "lenient_icon_size")]
    icons: IconSize,
    sort_by: SortBy,
    #[serde(default)]
    descending: bool,
    /// Secondary and tertiary sort keys; left out when there are none. Older Arke ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    then_by: Vec<StoredKey>,
}

/// A further sort key as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredKey {
    sort_by: SortBy,
    #[serde(default)]
    descending: bool,
}

impl StoredKey {
    const fn from_key(key: SortKey) -> Self {
        Self {
            sort_by: SortBy::from_column(key.column),
            descending: !key.ascending,
        }
    }

    const fn key(self) -> SortKey {
        SortKey {
            column: self.sort_by.column(),
            ascending: !self.descending,
        }
    }
}

/// Stored keys back into the fixed slots; extra ones (a hand edit) are dropped, and a key repeating
/// an earlier column is skipped.
fn then_keys(primary: SortColumn, stored: &[StoredKey]) -> [Option<SortKey>; MAX_SORT_KEYS - 1] {
    let mut out = [None; MAX_SORT_KEYS - 1];
    let mut seen = vec![primary];
    let mut slots = out.iter_mut();
    for key in stored.iter().map(|k| k.key()) {
        if seen.contains(&key.column) {
            continue;
        }
        let Some(slot) = slots.next() else { break };
        seen.push(key.column);
        *slot = Some(key);
    }
    out
}

/// An unknown icon size (edited by hand, or from a newer Arke) falls back to the default rather
/// than dropping the folder.
fn lenient_icon_size<'de, D: serde::Deserializer<'de>>(d: D) -> Result<IconSize, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// The file: most recently used folder first.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
struct StoredFile {
    version: u32,
    folders: Vec<StoredFolder>,
}

struct Slot {
    /// The path as last recorded, for the file
    path: PathBuf,
    view: FolderView,
    /// Logical clock of the last use; the smallest is evicted first
    used: u64,
}

/// The remembered views themselves, without GPUI or files: lookup, recording and the
/// least-recently-used cap.
pub(crate) struct FolderViews {
    slots: HashMap<String, Slot>,
    clock: u64,
    cap: usize,
}

impl Default for FolderViews {
    fn default() -> Self {
        Self::with_cap(MAX_FOLDERS)
    }
}

impl FolderViews {
    /// An empty memory that keeps at most `cap` folders.
    pub(crate) fn with_cap(cap: usize) -> Self {
        Self {
            slots: HashMap::new(),
            clock: 0,
            cap: cap.max(1),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    const fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The remembered view for `path`, counting this as a use (so folders that are opened often
    /// aren't the ones forgotten).
    pub(crate) fn get(&mut self, path: &Path) -> Option<FolderView> {
        let now = self.tick();
        let slot = self.slots.get_mut(&folder_key(path))?;
        slot.used = now;
        Some(slot.view)
    }

    /// The remembered view for `path`, without counting it as a use.
    #[cfg(test)]
    pub(crate) fn peek(&self, path: &Path) -> Option<FolderView> {
        self.slots.get(&folder_key(path)).map(|s| s.view)
    }

    /// Remember `view` for `path`; past the cap, the least recently used folder is forgotten.
    pub(crate) fn insert(&mut self, path: &Path, view: FolderView) {
        let used = self.tick();
        self.slots.insert(
            folder_key(path),
            Slot {
                path: path.to_path_buf(),
                view,
                used,
            },
        );
        while self.slots.len() > self.cap {
            let Some(oldest) = self
                .slots
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.slots.remove(&oldest);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }

    /// Folders from most to least recently used.
    fn by_recency(&self) -> Vec<&Slot> {
        let mut slots: Vec<&Slot> = self.slots.values().collect();
        slots.sort_by_key(|s| std::cmp::Reverse(s.used));
        slots
    }

    fn to_file(&self) -> StoredFile {
        StoredFile {
            version: FILE_VERSION,
            folders: self
                .by_recency()
                .into_iter()
                .map(|s| StoredFolder {
                    path: s.path.clone(),
                    view: s.view.view,
                    icons: s.view.icons,
                    sort_by: SortBy::from_column(s.view.column),
                    descending: !s.view.ascending,
                    then_by: s
                        .view
                        .then
                        .iter()
                        .flatten()
                        .map(|k| StoredKey::from_key(*k))
                        .collect(),
                })
                .collect(),
        }
    }

    /// Rebuild from the file's list (most recent first). A folder listed twice (the file was
    /// edited by hand) keeps its first, most recent, entry.
    fn from_folders(folders: Vec<StoredFolder>, cap: usize) -> Self {
        let mut out = Self::with_cap(cap);
        for folder in folders.into_iter().rev() {
            out.insert(
                &folder.path,
                FolderView {
                    view: folder.view,
                    icons: folder.icons,
                    column: folder.sort_by.column(),
                    ascending: !folder.descending,
                    then: then_keys(folder.sort_by.column(), &folder.then_by),
                },
            );
        }
        out
    }

    /// Lay `newer` (recorded before the file finished loading) over what was loaded.
    fn merge_newer(&mut self, newer: &Self) {
        for slot in newer.by_recency().into_iter().rev() {
            self.insert(&slot.path, slot.view);
        }
    }
}

/// Parse the file. Entries that can't be read (an unknown view mode from a newer Arke, a mangled
/// hand edit) are skipped one by one instead of losing every folder.
fn parse(bytes: &[u8], path: &Path) -> Result<Vec<StoredFolder>, FolderViewsError> {
    // Notepad and PowerShell add a byte order mark when saving UTF-8; serde_json rejects it
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| FolderViewsError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let folders = value
        .get("folders")
        .and_then(serde_json::Value::as_array)
        .map_or_default(|list| {
            list.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        });
    Ok(folders)
}

/// Read the file; a missing file is simply an empty memory (first run).
pub(crate) fn load(path: &Path) -> Result<FolderViews, FolderViewsError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(FolderViews::default()),
        Err(source) => {
            return Err(FolderViewsError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    Ok(FolderViews::from_folders(parse(&bytes, path)?, MAX_FOLDERS))
}

/// Write through a temporary file and a rename, so a crash halfway never leaves a broken file.
fn save(path: &Path, file: &StoredFile) -> Result<(), FolderViewsError> {
    let write_err = |source| FolderViewsError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(write_err)?;
    }
    let json = serde_json::to_vec_pretty(file).map_err(FolderViewsError::Encode)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

/// `%APPDATA%\Arke\folder_views.json`, next to the settings (string work only).
pub(crate) fn default_path() -> Option<PathBuf> {
    crate::settings::default_path().map(|p| p.with_file_name("folder_views.json"))
}

/// The app-wide memory. Tabs look up and record through it; it saves itself.
pub(crate) struct FolderViewStore {
    views: FolderViews,
    /// The file has been read (always true without a file)
    loaded: bool,
    /// Bumped by [`Self::clear`], so tabs showing a remembered view go back to the defaults
    resets: u64,
    path: Option<PathBuf>,
    /// The save thread; started on the first save
    writer: Option<flume::Sender<StoredFile>>,
    save_task: Option<Task<()>>,
}

pub(crate) struct GlobalFolderViews(Entity<FolderViewStore>);

impl Global for GlobalFolderViews {}

impl FolderViewStore {
    /// Create the app-wide memory and read the file in the background. `path` is `None` to keep
    /// it in memory only (tests, benchmarks).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            views: FolderViews::default(),
            loaded: path.is_none(),
            resets: 0,
            path: path.clone(),
            writer: None,
            save_task: None,
        });
        cx.set_global(GlobalFolderViews(store.clone()));
        let Some(path) = path else {
            return store;
        };
        let (tx, rx) = flume::bounded::<FolderViews>(1);
        let spawned = std::thread::Builder::new()
            .name("folder-views-load".into())
            .spawn(move || {
                let views = load(&path).unwrap_or_else(|err| {
                    // Keep the broken file for the user to look at; it is replaced on the next save
                    log::warn!("{err}");
                    FolderViews::default()
                });
                let _ = tx.send(views);
            });
        if spawned.is_err() {
            store.update(cx, |store, cx| {
                store.finish_loading(FolderViews::default(), cx);
            });
            return store;
        }
        // Only waiting on the background thread's answer here, never touching the file
        if let Ok(views) = rx.recv_timeout(STARTUP_WAIT) {
            store.update(cx, |store, cx| store.finish_loading(views, cx));
            return store;
        }
        let weak = store.downgrade();
        cx.spawn(async move |cx| {
            let views = rx.recv_async().await.unwrap_or_default();
            let _ = weak.update(cx, |store, cx| store.finish_loading(views, cx));
        })
        .detach();
        store
    }

    /// The app-wide memory; created in memory only if nothing set it up (unit and GPUI tests).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalFolderViews>() {
            Some(g) => g.0.clone(),
            None => Self::init(None, cx),
        }
    }

    /// The file arrived. Anything recorded in the meantime is newer and wins; a reset in the
    /// meantime means the user wanted none of the old ones.
    fn finish_loading(&mut self, mut loaded: FolderViews, cx: &mut Context<Self>) {
        if self.resets == 0 {
            loaded.merge_newer(&self.views);
            self.views = loaded;
        }
        self.loaded = true;
        cx.notify();
    }

    pub(crate) const fn loaded(&self) -> bool {
        self.loaded
    }

    pub(crate) const fn resets(&self) -> u64 {
        self.resets
    }

    pub(crate) fn len(&self) -> usize {
        self.views.len()
    }

    /// The remembered view for `path` (a use for the least-recently-used order). The new order is
    /// only kept in memory until the next save: opening a folder alone doesn't write the file.
    pub(crate) fn lookup(&mut self, path: &Path) -> Option<FolderView> {
        self.views.get(path)
    }

    /// The remembered view for `path` without touching the order.
    #[cfg(test)]
    pub(crate) fn peek(&self, path: &Path) -> Option<FolderView> {
        self.views.peek(path)
    }

    /// Remember `view` for `path` and save soon. Doesn't notify: open tabs don't change because
    /// another tab picked a view.
    pub(crate) fn record(&mut self, path: &Path, view: FolderView, cx: &mut Context<Self>) {
        self.views.insert(path, view);
        self.schedule_save(cx);
    }

    /// Forget every folder ("Reset folder views").
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.views.clear();
        self.resets += 1;
        self.schedule_save(cx);
        cx.notify();
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.path.is_none() {
            return;
        }
        // A newer change replaces the save that hasn't happened yet
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |store, _| store.send_to_writer());
        }));
    }

    fn send_to_writer(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let file = self.views.to_file();
        let writer = self.writer.get_or_insert_with(|| {
            let (tx, rx) = flume::unbounded::<StoredFile>();
            // One thread writes everything in order, so an older snapshot can never land after a
            // newer one. It ends when the store (the sender) goes away.
            crate::explorer::spawn_thread("folder-views-save", move || {
                while let Ok(mut file) = rx.recv() {
                    // Only the newest snapshot matters
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
            // The thread couldn't start; try again on the next save
            self.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fv(view: ViewMode, column: SortColumn, ascending: bool) -> FolderView {
        FolderView {
            view,
            icons: IconSize::default(),
            column,
            ascending,
            then: [None; MAX_SORT_KEYS - 1],
        }
    }

    fn icons() -> FolderView {
        fv(ViewMode::Icons, SortColumn::Name, true)
    }

    /// Case, trailing separators, forward slashes, `.`/`..` and `\\?\` all reach the same folder.
    #[test]
    fn keys_ignore_case_and_trailing_separators() {
        let key = |p: &str| folder_key(Path::new(p));
        let want = key(r"C:\Users\Me\Photos");
        for same in [
            r"c:\users\me\photos",
            r"C:\Users\Me\Photos\",
            r"C:\Users\Me\Photos\\",
            "C:/Users/Me/Photos/",
            r"C:\Users\Me\.\Photos",
            r"C:\Users\Me\Other\..\Photos",
            r"\\?\C:\Users\Me\Photos",
        ] {
            assert_eq!(key(same), want, "{same}");
        }
        assert_ne!(key(r"C:\Users\Me\Photos2"), want);
        assert_eq!(key(r"\\NAS\Share\Docs\"), key(r"\\nas\share\docs"));
        assert_eq!(key(r"\\?\UNC\nas\share\docs"), key(r"\\nas\share\docs"));
        assert_eq!(key(r"C:\"), key("c:/"), "drive roots too");
        // Non-ASCII names fold case as well (Windows does)
        assert_eq!(key(r"D:\Ärger"), key(r"d:\ärger"));
    }

    #[test]
    fn records_and_looks_up_case_insensitively() {
        let mut views = FolderViews::default();
        assert_eq!(views.get(Path::new(r"C:\a")), None);
        views.insert(Path::new(r"C:\A\"), icons());
        assert_eq!(views.get(Path::new(r"c:\a")), Some(icons()));
        // Recording again replaces, doesn't add
        let size = fv(ViewMode::Details, SortColumn::Size, false);
        views.insert(Path::new(r"C:\a"), size);
        assert_eq!(views.len(), 1);
        assert_eq!(views.peek(Path::new(r"C:\A")), Some(size));
        views.clear();
        assert!(views.is_empty());
    }

    /// Past the cap the least recently used folder goes; looking a folder up counts as a use.
    #[test]
    fn evicts_the_least_recently_used_folder() {
        let p = |n: usize| PathBuf::from(format!(r"C:\f{n}"));
        let mut views = FolderViews::with_cap(3);
        for n in 0..3 {
            views.insert(&p(n), icons());
        }
        // f0 is the oldest record but was just opened: f1 is the one forgotten
        assert!(views.get(&p(0)).is_some());
        views.insert(&p(3), icons());
        assert_eq!(views.len(), 3);
        assert!(views.peek(&p(1)).is_none());
        for n in [0, 2, 3] {
            assert!(views.peek(&p(n)).is_some(), "f{n}");
        }
        // The full-size cap holds too
        let mut big = FolderViews::default();
        for n in 0..=MAX_FOLDERS {
            big.insert(&p(n), icons());
        }
        assert_eq!(big.len(), MAX_FOLDERS);
        assert!(big.peek(&p(0)).is_none());
        assert!(big.peek(&p(MAX_FOLDERS)).is_some());
    }

    /// The file lists folders most recent first, keeps the path as recorded, and comes back with
    /// the same order (so eviction after a restart still drops the right folder).
    #[test]
    fn file_round_trip_keeps_order_and_paths() {
        let dir = std::env::temp_dir().join(format!("arke-folder-views-{}", std::process::id()));
        let path = dir.join("folder_views.json");
        let mut views = FolderViews::default();
        views.insert(Path::new(r"C:\Old"), icons());
        views.insert(
            Path::new(r"D:\Music"),
            fv(ViewMode::Columns, SortColumn::Modified, false),
        );
        save(&path, &views.to_file()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let music = text.find(r"D:\\Music").unwrap();
        let old = text.find(r"C:\\Old").unwrap();
        assert!(music < old, "most recent first:\n{text}");
        assert!(text.contains(r#""sort_by": "modified""#), "{text}");

        let mut back = load(&path).unwrap();
        assert_eq!(back.to_file(), views.to_file());
        assert_eq!(
            back.get(Path::new(r"d:\music\")),
            Some(fv(ViewMode::Columns, SortColumn::Modified, false))
        );
        // Order survived: with room for one more, the oldest (C:\Old) is the one evicted
        let mut tight = FolderViews::from_folders(parse(text.as_bytes(), &path).unwrap(), 2);
        tight.insert(Path::new(r"E:\new"), icons());
        assert!(tight.peek(Path::new(r"C:\Old")).is_none());
        assert!(tight.peek(Path::new(r"D:\Music")).is_some());

        std::fs::remove_dir_all(&dir).unwrap();
        // No file yet: empty, not an error
        assert!(load(&path).unwrap().is_empty());
    }

    /// Secondary sort keys survive the file; repeats and extras from a hand edit are dropped, and a
    /// view without them writes no `then_by` at all.
    #[test]
    fn secondary_sort_keys_round_trip() {
        let mut multi = fv(ViewMode::Details, SortColumn::Type, true);
        multi.then[0] = Some(SortKey {
            column: SortColumn::Size,
            ascending: false,
        });
        multi.then[1] = Some(SortKey {
            column: SortColumn::Attributes,
            ascending: true,
        });
        let mut views = FolderViews::default();
        views.insert(Path::new(r"C:\multi"), multi);
        views.insert(Path::new(r"C:\plain"), icons());
        let text = serde_json::to_string(&views.to_file()).unwrap();
        assert_eq!(text.matches("then_by").count(), 1, "{text}");
        assert!(text.contains(r#""sort_by":"attributes""#), "{text}");
        let mut back =
            FolderViews::from_folders(parse(text.as_bytes(), Path::new("x")).unwrap(), 9);
        assert_eq!(back.get(Path::new(r"C:\multi")), Some(multi));
        assert_eq!(back.get(Path::new(r"C:\plain")), Some(icons()));

        let edited = br#"{"folders": [{"path": "C:\\e", "view": "details", "sort_by": "name",
            "then_by": [{"sort_by": "name"}, {"sort_by": "size"}, {"sort_by": "size"},
                        {"sort_by": "type", "descending": true}, {"sort_by": "modified"}]}]}"#;
        let mut views = FolderViews::from_folders(parse(edited, Path::new("x")).unwrap(), 9);
        let view = views.get(Path::new(r"C:\e")).unwrap();
        assert_eq!(
            view.then,
            [
                Some(SortKey {
                    column: SortColumn::Size,
                    ascending: true
                }),
                Some(SortKey {
                    column: SortColumn::Type,
                    ascending: false
                }),
            ]
        );
    }

    /// A bad entry is skipped, a broken file is an error (logged by the caller), a BOM is fine,
    /// and a folder listed twice keeps its first (most recent) entry.
    #[test]
    fn tolerates_odd_files() {
        let path = Path::new("folder_views.json");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            br#"{"version": 1, "folders": [
                {"path": "C:\\a", "view": "icons", "sort_by": "size", "descending": true},
                {"path": "C:\\b", "view": "hologram", "sort_by": "name"},
                {"path": "C:\\A", "view": "details", "sort_by": "name"},
                {"path": "C:\\c", "view": "columns", "sort_by": "type"}
            ]}"#,
        );
        let mut views = FolderViews::from_folders(parse(&bytes, path).unwrap(), MAX_FOLDERS);
        assert_eq!(views.len(), 2);
        assert_eq!(
            views.get(Path::new(r"C:\a")),
            Some(fv(ViewMode::Icons, SortColumn::Size, false))
        );
        assert_eq!(
            views.get(Path::new(r"C:\c")),
            Some(fv(ViewMode::Columns, SortColumn::Type, true)),
            "descending is optional"
        );
        assert!(matches!(
            parse(b"{not json", path),
            Err(FolderViewsError::Parse { .. })
        ));
        assert!(parse(b"{}", path).unwrap().is_empty());
    }

    /// The icon size round-trips; old files without it and unknown sizes get large icons.
    #[test]
    fn icon_sizes_are_kept_and_old_files_get_large() {
        let mut views = FolderViews::default();
        let small = FolderView {
            icons: IconSize::SMALL,
            ..icons()
        };
        views.insert(Path::new(r"C:\pics"), small);
        let json = serde_json::to_vec(&views.to_file()).unwrap();
        let path = Path::new("folder_views.json");
        let mut back = FolderViews::from_folders(parse(&json, path).unwrap(), MAX_FOLDERS);
        assert_eq!(back.get(Path::new(r"C:\pics")), Some(small));
        let old = br#"{"version": 1, "folders": [
            {"path": "C:\\a", "view": "icons", "sort_by": "name"},
            {"path": "C:\\b", "view": "icons", "icons": "gigantic", "sort_by": "name"},
            {"path": "C:\\c", "view": "icons", "icons": "extra_large", "sort_by": "name"},
            {"path": "C:\\d", "view": "icons", "icons": 132, "sort_by": "name"}
        ]}"#;
        let mut views = FolderViews::from_folders(parse(old, path).unwrap(), MAX_FOLDERS);
        assert_eq!(
            views.get(Path::new(r"C:\a")).map(|v| v.icons),
            Some(IconSize::LARGE)
        );
        assert_eq!(
            views.get(Path::new(r"C:\b")).map(|v| v.icons),
            Some(IconSize::LARGE)
        );
        assert_eq!(
            views.get(Path::new(r"C:\c")).map(|v| v.icons),
            Some(IconSize::EXTRA_LARGE)
        );
        assert_eq!(
            views.get(Path::new(r"C:\d")).map(|v| v.icons),
            Some(IconSize::from_px(132.)),
            "a slider size in between"
        );
        // A size in between round-trips too.
        let between = FolderView {
            icons: IconSize::from_px(132.),
            ..icons()
        };
        let mut views = FolderViews::default();
        views.insert(Path::new(r"C:\pics"), between);
        let json = serde_json::to_vec(&views.to_file()).unwrap();
        assert!(String::from_utf8_lossy(&json).contains(r#""icons":132"#));
        let mut back = FolderViews::from_folders(parse(&json, path).unwrap(), MAX_FOLDERS);
        assert_eq!(back.get(Path::new(r"C:\pics")), Some(between));
    }

    /// Recorded before the file finished loading: kept, and newer than what was on disk.
    #[test]
    fn records_made_while_loading_win() {
        let mut loaded = FolderViews::default();
        loaded.insert(Path::new(r"C:\a"), icons());
        loaded.insert(Path::new(r"C:\b"), icons());
        let mut early = FolderViews::default();
        let details = fv(ViewMode::Details, SortColumn::Name, false);
        early.insert(Path::new(r"C:\A"), details);
        loaded.merge_newer(&early);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.peek(Path::new(r"C:\a")), Some(details));
        assert_eq!(
            loaded.to_file().folders[0].path,
            PathBuf::from(r"C:\A"),
            "most recent first"
        );
    }
}
