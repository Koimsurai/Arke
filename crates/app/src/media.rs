//! Thumbnail and system icon cache, one per app, shared by all tabs and windows.
//!
//! - Thumbnails: 3 background threads read via `Fs::read` and decode with `arke_core::thumbs`.
//! - System icons: 1 background thread (COM STA), only for `.exe`, `.lnk`, `.ico`.
//! - Both only on local fixed drives (cached per drive letter; the check itself runs in the
//!   background).
//! - The visible tab calls [`MediaStore::want`] every frame with visible items first; requests
//!   scrolled away are dropped before work starts (`arke_core::loader`).
//! - The cache has a memory budget; least recently used entries are freed and removed from the GPU
//!   atlas.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use arke_core::Entry;
use arke_core::filetype::{MAX_THUMBNAIL_SOURCE, can_thumbnail, wants_system_icon};
use arke_core::loader::Loader;
use arke_core::thumbs::{SOURCE_SIZES, THUMBNAIL_SIZE, Thumbnail, decode_thumbnail};
use gpui::{App, AppContext, Context, Entity, Global, RenderImage, Task};

use crate::explorer::Services;

/// System icon pixel size (the grid shows them at 64 px, the list at 16).
pub(crate) const SYSTEM_ICON_SIZE: u32 = 64;
/// Cache budget (decoded pixel bytes). A 192px thumbnail is about 150 KB, so roughly 800 fit.
const CACHE_BUDGET: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MediaKind {
    Thumbnail,
    SystemIcon,
}

/// File path plus mtime and size: an edited file gets a new key and never a stale thumbnail.
/// Thumbnails also carry the size they are decoded at (one of [`SOURCE_SIZES`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MediaKey {
    pub(crate) path: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
    pub(crate) kind: MediaKind,
    /// Longest side to decode a thumbnail at; 0 for system icons.
    px: u32,
}

impl MediaKey {
    /// Whether (and which) media this item needs, with thumbnails at the default size. With
    /// `thumbnails` false (Details view) only system icons.
    pub(crate) fn for_entry(dir: &Path, entry: &Entry, thumbnails: bool) -> Option<Self> {
        Self::sized(dir, entry, thumbnails.then_some(THUMBNAIL_SIZE))
    }

    /// Like [`Self::for_entry`], with thumbnails decoded at `thumbnail_px` (`None`: no
    /// thumbnails). `entry.name` may be a nested tree row's relative path.
    pub(crate) fn sized(dir: &Path, entry: &Entry, thumbnail_px: Option<u32>) -> Option<Self> {
        Self::at(dir.join(&*entry.name), entry, thumbnail_px)
    }

    /// Key for the file at `path` (whose listing entry is `entry`).
    pub(crate) fn at(path: PathBuf, entry: &Entry, thumbnail_px: Option<u32>) -> Option<Self> {
        let (kind, px) = if wants_system_icon(entry) {
            (MediaKind::SystemIcon, 0)
        } else {
            (
                MediaKind::Thumbnail,
                thumbnail_px.filter(|_| can_thumbnail(entry))?,
            )
        };
        Some(Self {
            path,
            modified: entry.modified,
            size: entry.size,
            kind,
            px,
        })
    }
}

enum MediaEvent {
    Loaded(MediaKey, Option<Thumbnail>),
    LocalDisk(PathBuf, bool),
}

struct GlobalMedia(Entity<MediaStore>);

impl Global for GlobalMedia {}

pub(crate) struct MediaStore {
    services: Services,
    /// `None` means loading failed (not retried until the file changes)
    cache: HashMap<MediaKey, Option<Arc<RenderImage>>>,
    /// Recency order (oldest first)
    order: VecDeque<MediaKey>,
    bytes: usize,
    thumbs: Loader<MediaKey>,
    icons: Loader<MediaKey>,
    last_wanted: Vec<MediaKey>,
    /// Wanted by the main list (in priority order)
    main_wanted: Vec<MediaKey>,
    /// Wanted by the side column (column view's parent), queued after the main list
    side_wanted: Vec<MediaKey>,
    local_disk: HashMap<PathBuf, bool>,
    local_pending: HashSet<PathBuf>,
    tx: flume::Sender<MediaEvent>,
    _task: Task<()>,
}

/// Drive root (`C:\`); `None` for UNC or relative paths (never thumbnailed).
fn drive_root(path: &Path) -> Option<PathBuf> {
    match path.components().next()? {
        Component::Prefix(p) if matches!(p.kind(), std::path::Prefix::Disk(_)) => Some(
            PathBuf::from(format!("{}\\", p.as_os_str().to_string_lossy())),
        ),
        _ => None,
    }
}

pub(crate) fn to_render_image(t: Thumbnail) -> Option<Arc<RenderImage>> {
    let mut bgra = t.rgba;
    for p in bgra.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(t.width, t.height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

impl MediaStore {
    /// Get the global media cache, creating it on first use.
    pub(crate) fn global(services: &Services, cx: &mut App) -> Entity<Self> {
        if let Some(g) = cx.try_global::<GlobalMedia>() {
            return g.0.clone();
        }
        let services = services.clone();
        let store = cx.new(|cx| Self::new(services, cx));
        cx.set_global(GlobalMedia(store.clone()));
        store
    }

    fn new(services: Services, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = flume::unbounded::<MediaEvent>();
        let thumbs = {
            let fs = services.fs.clone();
            let tx = tx.clone();
            Loader::new("thumbnail", 3, move |key: &MediaKey| {
                let thumb = fs
                    .read(&key.path, MAX_THUMBNAIL_SOURCE)
                    .ok()
                    .and_then(|bytes| decode_thumbnail(&bytes, key.px.max(1)).ok());
                let _ = tx.send(MediaEvent::Loaded(key.clone(), thumb));
            })
        };
        let icons = {
            let shell = services.shell.clone();
            let tx = tx.clone();
            Loader::new("system-icon", 1, move |key: &MediaKey| {
                let icon = shell.file_icon(&key.path, SYSTEM_ICON_SIZE);
                let _ = tx.send(MediaEvent::Loaded(key.clone(), icon));
            })
        };
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                if this.update(cx, |this, cx| this.handle(event, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            services,
            cache: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            thumbs,
            icons,
            last_wanted: Vec::new(),
            main_wanted: Vec::new(),
            side_wanted: Vec::new(),
            local_disk: HashMap::new(),
            local_pending: HashSet::new(),
            tx,
            _task: task,
        }
    }

    fn handle(&mut self, event: MediaEvent, cx: &mut Context<Self>) {
        match event {
            MediaEvent::Loaded(key, thumb) => {
                // Tell the queue the result was taken (it's redone only if evicted and requested
                // again).
                match key.kind {
                    MediaKind::Thumbnail => self.thumbs.acknowledge(&key),
                    MediaKind::SystemIcon => self.icons.acknowledge(&key),
                }
                let size = thumb.as_ref().map_or(0, |t| t.rgba.len());
                let image = thumb.and_then(to_render_image);
                if self.cache.insert(key.clone(), image).is_none() {
                    self.order.push_back(key);
                    self.bytes += size;
                }
                self.evict(cx);
            }
            MediaEvent::LocalDisk(root, local) => {
                self.local_pending.remove(&root);
                self.local_disk.insert(root, local);
            }
        }
        cx.notify();
    }

    fn evict(&mut self, cx: &mut App) {
        while self.bytes > CACHE_BUDGET {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if let Some(Some(image)) = self.cache.remove(&key) {
                self.bytes = self.bytes.saturating_sub(image_bytes(&image));
                // Remove from every window's GPU atlas; dropping the reference alone leaves the
                // texture in video memory.
                cx.drop_image(image, None);
            }
        }
    }

    /// Loaded thumbnails and icons (for the benchmark report).
    pub(crate) fn loaded_count(&self) -> usize {
        self.cache.values().filter(|v| v.is_some()).count()
    }

    /// Loaded entries (tests).
    #[cfg(test)]
    pub(crate) fn loaded(&self) -> Vec<(PathBuf, MediaKind)> {
        let mut v: Vec<_> = self
            .cache
            .iter()
            .filter(|(_, img)| img.is_some())
            .map(|(k, _)| (k.path.clone(), k.kind))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Loaded image (`None` if failed or not loaded yet).
    pub(crate) fn get(&self, key: &MediaKey) -> Option<Arc<RenderImage>> {
        self.cache.get(key).cloned().flatten()
    }

    /// Like [`Self::get`], but while a thumbnail isn't loaded at this size yet, the same file's
    /// thumbnail at the closest other size (after an icon size change the grid keeps its pictures
    /// instead of blinking to type icons).
    pub(crate) fn get_or_nearby(&self, key: &MediaKey) -> Option<Arc<RenderImage>> {
        if let Some(found) = self.cache.get(key) {
            return found.clone();
        }
        if key.kind != MediaKind::Thumbnail {
            return None;
        }
        let mut others: Vec<u32> = SOURCE_SIZES.into_iter().filter(|&s| s != key.px).collect();
        others.sort_by_key(|&s| s.abs_diff(key.px));
        let mut probe = key.clone();
        others.into_iter().find_map(|px| {
            probe.px = px;
            self.cache.get(&probe).cloned().flatten()
        })
    }

    /// Whether this folder can get thumbnails and system icons. The first time a drive is seen this
    /// returns false, and a redraw follows once the background check finishes (drives are never
    /// probed on the UI thread).
    pub(crate) fn media_allowed(&mut self, dir: &Path) -> bool {
        let Some(root) = drive_root(dir) else {
            return false;
        };
        if let Some(&local) = self.local_disk.get(&root) {
            return local;
        }
        if self.local_pending.insert(root.clone()) {
            let shell = self.services.shell.clone();
            let tx = self.tx.clone();
            let spawned = std::thread::Builder::new()
                .name("local-disk".into())
                .spawn(move || {
                    let local = shell.is_local_disk(&root);
                    let _ = tx.send(MediaEvent::LocalDisk(root, local));
                });
            if let Err(err) = spawned {
                log::error!("cannot spawn thread: {err}");
            }
        }
        false
    }

    /// Already probed and known to be a network location (thumbnails there load only with "Show
    /// thumbnails on network locations" on). Still-probing locations count as not remote (redraws
    /// when done).
    pub(crate) fn known_remote(&mut self, dir: &Path) -> bool {
        match drive_root(dir) {
            // Drive letter: probed and not local (mapped network drive)
            Some(root) => self.local_disk.get(&root) == Some(&false),
            // \\server\share is always remote
            None => matches!(
                dir.components().next(),
                Some(Component::Prefix(p))
                    if matches!(p.kind(), std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..))
            ),
        }
    }

    /// Items the main list needs now (in priority order). Already cached ones are skipped.
    pub(crate) fn want(&mut self, keys: Vec<MediaKey>) {
        if keys == self.main_wanted {
            return;
        }
        self.main_wanted = keys;
        self.apply_wanted();
    }

    /// Items for the side column: queued after the main list and never cancel it.
    pub(crate) fn want_side(&mut self, keys: Vec<MediaKey>) {
        if keys == self.side_wanted {
            return;
        }
        self.side_wanted = keys;
        self.apply_wanted();
    }

    fn apply_wanted(&mut self) {
        let keys: Vec<MediaKey> = self
            .main_wanted
            .iter()
            .chain(self.side_wanted.iter())
            .cloned()
            .collect();
        if keys == self.last_wanted {
            return;
        }
        let (mut thumbs, mut icons) = (Vec::new(), Vec::new());
        for key in &keys {
            if self.cache.contains_key(key) {
                continue;
            }
            match key.kind {
                MediaKind::Thumbnail => thumbs.push(key.clone()),
                MediaKind::SystemIcon => icons.push(key.clone()),
            }
        }
        self.thumbs.set_wanted(thumbs);
        self.icons.set_wanted(icons);
        // Move used entries to the recent end so visible thumbnails aren't evicted.
        for key in &keys {
            if self.cache.get(key).is_some_and(Option::is_some)
                && let Some(pos) = self.order.iter().position(|k| k == key)
                && let Some(k) = self.order.remove(pos)
            {
                self.order.push_back(k);
            }
        }
        self.last_wanted = keys;
    }
}

fn image_bytes(image: &RenderImage) -> usize {
    let size = image.size(0);
    (size.width.0 as usize) * (size.height.0 as usize) * 4
}
