//! Thumbnail and system icon cache, one per app, shared by all tabs and windows.
//!
//! - Thumbnails: 3 background threads read via `Fs::read` and decode with `arke_core::thumbs`.
//!   Each result is also kept compressed ([`arke_core::thumb_cache`]) in memory and on disk, so
//!   seeing it again costs a 0.1–0.2 ms decode instead of reading the original.
//! - System icons: 1 background thread (COM STA), only for `.exe`, `.lnk`, `.ico`.
//! - Both only on local fixed drives (cached per drive letter; the check itself runs in the
//!   background).
//! - The visible tab calls [`MediaStore::want`] every frame with visible items first; requests
//!   scrolled away are dropped before work starts (`arke_core::loader`).
//! - Decoded images have a small budget: least recently used ones are freed and removed from the GPU
//!   atlas, but never the ones on screen or one page around it, in any window or pane, nor a
//!   picture of another size standing in for one (`media_budget.rs` has the rules). Scrolling back
//!   decodes them again from the compressed copy, on the UI thread while it fits in a few
//!   milliseconds.
//! - Each view says per frame what it wants: [`MediaStore::drawing`] when it renders, then
//!   [`MediaStore::want`] per list. The loaders are told when the frame ends, if that changed.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use arke_core::Entry;
use arke_core::filetype::{
    MAX_THUMBNAIL_SOURCE, can_pdf_thumbnail, can_thumbnail, can_video_thumbnail, wants_system_icon,
    wants_type_icon,
};
use arke_core::loader::Loader;
use arke_core::thumb_cache::{ThumbCache, ThumbKey};
use arke_core::thumbs::{SOURCE_SIZES, THUMBNAIL_SIZE, Thumbnail, decode_thumbnail};
use gpui::{
    App, AppContext, Context, Entity, EntityId, Global, RenderImage, Subscription, Task, WindowId,
};
use parking_lot::Mutex;

use crate::explorer::Services;
use crate::media_budget::{Decoded, Pins};

/// System icon pixel size (the grid shows them at 64 px, the list at 16).
pub(crate) const SYSTEM_ICON_SIZE: u32 = 64;
/// Path of a type icon's key: `*` can't be in a file name, so it never names a file.
const TYPE_ICON_PREFIX: &str = "*.";
/// Decoded pixel bytes kept, counting what is on screen and a page around it (which stay even past
/// it). A 192px thumbnail is about 150 KB, so roughly 100 fit; the compressed copies cover the rest.
const CACHE_BUDGET: usize = 16 * 1024 * 1024;
/// Compressed thumbnails in memory: 5–15 KB each, so several thousand.
const COMPRESSED_BUDGET: usize = 48 * 1024 * 1024;
/// The disk cache folder's cap (`%LOCALAPPDATA%\Arke\thumbs`): tens of thousands of thumbnails.
const DISK_CAP: u64 = 256 * 1024 * 1024;
/// Time per frame for decoding compressed thumbnails on the UI thread (the rest go to the loader).
const SYNC_DECODE: Duration = Duration::from_millis(3);

/// Thumbnails the background loader was asked for (tests).
#[cfg(test)]
pub(crate) static THUMBNAIL_LOADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Compressed thumbnails in memory and on disk, for every window. Tests keep them in memory only.
pub(crate) fn thumb_cache() -> &'static ThumbCache {
    static CACHE: OnceLock<ThumbCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let dir = crate::paths::thumbs_dir().filter(|_| !cfg!(test));
        ThumbCache::new(COMPRESSED_BUDGET, dir.map(|d| (d, DISK_CAP)))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MediaKind {
    Thumbnail,
    SystemIcon,
    /// A video's thumbnail: a frame, from Windows, made in a separate process (`thumb_host.rs`).
    Video,
    /// A PDF's first page, rendered by Windows in process (the same renderer as the preview).
    Pdf,
}

impl MediaKind {
    /// A picture of the content (shown like a photo), not an icon.
    pub(crate) const fn is_picture(self) -> bool {
        matches!(self, Self::Thumbnail | Self::Video | Self::Pdf)
    }
}

/// File path plus mtime and size: an edited file gets a new key and never a stale thumbnail.
/// Thumbnails also carry the size they are decoded at (one of [`SOURCE_SIZES`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MediaKey {
    pub(crate) path: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
    pub(crate) kind: MediaKind,
    /// Longest side to decode a thumbnail at. For system icons the physical size they are drawn at
    /// (see [`icon_source_px`]), or 0 for the fixed [`SYSTEM_ICON_SIZE`].
    px: u32,
}

/// Per extension: whether the registered icon is the file itself (so no type icon exists), as
/// far as the background lookup ([`start_icon_probe`]) has found out; `false` until it answers.
#[derive(Default)]
struct PerFileIcons {
    known: HashMap<String, bool>,
    asked: HashSet<String>,
}

static PER_FILE_ICONS: LazyLock<Mutex<PerFileIcons>> = LazyLock::new(Mutex::default);
static PROBE: OnceLock<flume::Sender<String>> = OnceLock::new();

fn icon_is_per_file(ext: &str) -> bool {
    let mut state = PER_FILE_ICONS.lock();
    if let Some(&known) = state.known.get(ext) {
        return known;
    }
    if let Some(probe) = PROBE.get()
        && state.asked.insert(ext.to_owned())
    {
        let _ = probe.send(ext.to_owned());
    }
    drop(state);
    false
}

/// Starts the thread that reads the `DefaultIcon` registered for each extension asked about
/// (`source`; the registry is never read on the UI thread). `changed` runs after answers
/// arrived, so views ask for the right icons.
pub(crate) fn start_icon_probe(
    source: impl Fn(&str) -> Option<String> + Send + 'static,
    changed: impl Fn() + Send + 'static,
) {
    let (tx, rx) = flume::unbounded::<String>();
    if PROBE.set(tx).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("icon-probe".into())
        .spawn(move || {
            while let Ok(ext) = rx.recv() {
                let per_file = source(&ext)
                    .is_some_and(|icon| arke_core::filetype::default_icon_is_the_file(&icon));
                PER_FILE_ICONS.lock().known.insert(ext, per_file);
                changed();
            }
        });
    if let Err(e) = spawned {
        log::warn!("icon probe thread: {e}");
    }
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
        if wants_type_icon(entry)
            && !entry.is_cloud_placeholder()
            && icon_is_per_file(entry.extension())
        {
            // The registered icon is the file's own (`%1`): ask for this file, like `.ico`. Not
            // for an online-only cloud file: reading it would download it.
            return Some(Self {
                path,
                modified: entry.modified,
                size: entry.size,
                kind: MediaKind::SystemIcon,
                px: 0,
            });
        }
        if let Some(key) = Self::type_icon(entry) {
            return Some(key);
        }
        let (kind, px) = if wants_system_icon(entry) {
            (MediaKind::SystemIcon, 0)
        } else if can_video_thumbnail(entry) {
            (MediaKind::Video, thumbnail_px?)
        } else if can_pdf_thumbnail(entry) {
            (MediaKind::Pdf, thumbnail_px?)
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

    /// Key for the icon Windows has for `entry`'s type ([`wants_type_icon`]): one per
    /// extension, shared by every file of that type. Reads no file, so also for folders where
    /// thumbnails and file icons aren't loaded (network shares).
    pub(crate) fn type_icon(entry: &Entry) -> Option<Self> {
        (wants_type_icon(entry)
            && (entry.is_cloud_placeholder() || !icon_is_per_file(entry.extension())))
        .then(|| Self {
            path: PathBuf::from(format!("{TYPE_ICON_PREFIX}{}", entry.extension())),
            modified: None,
            size: 0,
            kind: MediaKind::SystemIcon,
            px: 0,
        })
    }

    /// A system icon key that wants its icon at `px` physical pixels (see [`icon_source_px`]), so
    /// it is drawn 1:1 instead of being shrunk by the GPU. Other kinds are returned as they are.
    pub(crate) fn with_icon_px(mut self, px: u32) -> Self {
        if self.kind == MediaKind::SystemIcon {
            self.px = px;
        }
        self
    }

    /// The extension of a [`Self::type_icon`] key.
    fn type_icon_extension(&self) -> Option<&str> {
        self.path.to_str()?.strip_prefix(TYPE_ICON_PREFIX)
    }

    fn thumb_key(&self) -> ThumbKey {
        ThumbKey {
            path: self.path.clone(),
            modified: self.modified,
            size: self.size,
            px: self.px,
        }
    }
}

enum MediaEvent {
    Loaded(MediaKey, Option<Thumbnail>),
    /// The thumbnail host couldn't be asked: nothing is kept, so the item is asked again the next
    /// time it's wanted (by then the host may start).
    Unavailable(MediaKey),
    LocalDisk(PathBuf, bool),
}

struct GlobalMedia(Entity<MediaStore>);

impl Global for GlobalMedia {}

pub(crate) struct MediaStore {
    services: Services,
    /// Decoded images; a `None` means loading failed (not retried until the file changes).
    decoded: Decoded<MediaKey, Arc<RenderImage>>,
    /// What each view on screen wants, by view and window.
    pins: Pins<MediaKey, EntityId, WindowId>,
    thumbs: Loader<MediaKey>,
    icons: Loader<MediaKey>,
    videos: Loader<MediaKey>,
    pdfs: Loader<MediaKey>,
    /// Compressed thumbnails were decoded on the UI thread this frame (eviction is due after it).
    decoded_in_frame: bool,
    /// The loaders are told what is wanted at the end of the next frame even if it didn't change
    /// (the thumbnail host couldn't be asked, and may start now).
    resend: bool,
    local_disk: HashMap<PathBuf, bool>,
    local_pending: HashSet<PathBuf>,
    tx: flume::Sender<MediaEvent>,
    _task: Task<()>,
    _window_closed: Subscription,
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

/// Physical side a system icon drawn `logical` pixels wide on a display scaled by `scale` is
/// extracted at: the size it covers on screen, never above [`SYSTEM_ICON_SIZE`] (bigger tiles
/// stretch the largest one) and never below 16.
pub(crate) fn icon_source_px(logical: f32, scale: f32) -> u32 {
    let physical = (logical * scale.max(1.)).round().clamp(16., 64.);
    // In 16..=64 after the clamp.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped"
    )]
    let px = physical as u32;
    px.min(SYSTEM_ICON_SIZE)
}

/// Side to ask Windows for a system icon of a key with `px` (0: the fixed size).
const fn icon_request_px(px: u32) -> u32 {
    if px == 0 || px > SYSTEM_ICON_SIZE {
        SYSTEM_ICON_SIZE
    } else {
        px
    }
}

/// Brings an icon bigger than `px` down to `px` on its longest side by averaging the pixels each
/// new pixel covers (so a line one new pixel wide stays fully opaque), working
/// on premultiplied colour so transparent pixels don't darken the edge. `px` 0 or an icon not
/// bigger than `px` is returned as it is (never enlarged). `None` for a malformed bitmap.
pub(crate) fn fit_icon(icon: Thumbnail, px: u32) -> Option<Thumbnail> {
    let longest = icon.width.max(icon.height);
    if px == 0 || longest <= px {
        return Some(icon);
    }
    if icon.width == 0
        || icon.height == 0
        || icon.rgba.len() != icon.width as usize * icon.height as usize * 4
    {
        return None;
    }
    let width = (icon.width * px / longest).max(1);
    let height = (icon.height * px / longest).max(1);
    let mut rgba = icon.rgba;
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        for c in &mut p[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    let mut rgba = box_shrink(&rgba, (icon.width, icon.height), (width, height));
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Some(Thumbnail {
        width,
        height,
        rgba,
    })
}

/// Area average of premultiplied RGBA `src` (`from` pixels) down to `to` pixels: each output pixel
/// is the mean of the source area it covers, partial source pixels weighted by their overlap.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    reason = "pixel arithmetic on bounded sizes"
)]
fn box_shrink(src: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (sw, sh) = (from.0 as usize, from.1 as usize);
    let (dw, dh) = (to.0 as usize, to.1 as usize);
    let (xs, ys) = (sw as f32 / dw as f32, sh as f32 / dh as f32);
    // Source pixels a span covers, with their overlap.
    let span = |start: f32, len: f32, max: usize| {
        let end = start + len;
        (start.floor() as usize..(end.ceil() as usize).min(max)).map(move |i| {
            let w = (end.min(i as f32 + 1.) - start.max(i as f32)).max(0.);
            (i, w)
        })
    };
    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        for dx in 0..dw {
            let (mut sum, mut total) = ([0f32; 4], 0f32);
            for (y, wy) in span(dy as f32 * ys, ys, sh) {
                for (x, wx) in span(dx as f32 * xs, xs, sw) {
                    let w = wx * wy;
                    let p = &src[(y * sw + x) * 4..(y * sw + x) * 4 + 4];
                    for (s, &c) in sum.iter_mut().zip(p) {
                        *s += f32::from(c) * w;
                    }
                    total += w;
                }
            }
            let o = &mut out[(dy * dw + dx) * 4..(dy * dw + dx) * 4 + 4];
            for (o, s) in o.iter_mut().zip(sum) {
                *o = (s / total.max(f32::EPSILON) + 0.5).min(255.) as u8;
            }
        }
    }
    out
}

pub(crate) fn to_render_image(t: Thumbnail) -> Option<Arc<RenderImage>> {
    let mut bgra = t.rgba;
    for p in bgra.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(t.width, t.height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// Pixels with less alpha than this are ignored when judging an icon (rounding noise).
const FAINT_ALPHA: u8 = 16;
/// Brightest channel a "dark" glyph pixel may have.
const DARK_MAX: u8 = 96;
/// Widest spread between a pixel's channels for it to count as gray.
const GRAY_SPREAD: u8 = 24;

/// Whether BGRA `pixels` are a monochrome dark glyph: every visible pixel is a dark, near-gray
/// color. Windows hands out these for the light theme, and they vanish on a dark menu.
pub(crate) fn is_dark_mono(pixels: &[u8]) -> bool {
    let mut seen = false;
    for p in pixels.as_chunks::<4>().0 {
        if p[3] < FAINT_ALPHA {
            continue;
        }
        let hi = p[0].max(p[1]).max(p[2]);
        let lo = p[0].min(p[1]).min(p[2]);
        if hi > DARK_MAX || hi - lo > GRAY_SPREAD {
            return false;
        }
        seen = true;
    }
    seen
}

/// A Windows menu icon, with a copy drawn in the dark theme's text color when the icon is a dark
/// monochrome glyph. Made once, where the icon is decoded, not per frame.
#[derive(Clone)]
pub(crate) struct MenuIcon {
    image: Arc<RenderImage>,
    light_on_dark: Option<Arc<RenderImage>>,
}

impl MenuIcon {
    pub(crate) fn new(image: Arc<RenderImage>) -> Self {
        let light_on_dark = recolored(&image);
        Self {
            image,
            light_on_dark,
        }
    }

    /// The image to draw in the current theme.
    pub(crate) fn shown(&self) -> Arc<RenderImage> {
        match &self.light_on_dark {
            Some(twin) if crate::theme::is_dark() => Arc::clone(twin),
            _ => Arc::clone(&self.image),
        }
    }

    /// The icon as Windows gave it.
    pub(crate) const fn original(&self) -> &Arc<RenderImage> {
        &self.image
    }
}

/// `image` redrawn in the dark theme's text color, keeping each pixel's alpha, if it is a dark
/// monochrome glyph.
fn recolored(image: &RenderImage) -> Option<Arc<RenderImage>> {
    let size = image.size(0);
    let (w, h) = (
        u32::try_from(size.width.0).ok()?,
        u32::try_from(size.height.0).ok()?,
    );
    let bytes = image.as_bytes(0).filter(|b| is_dark_mono(b))?;
    let ink = crate::theme::dark_text();
    let [b, g, r] = [ink.b, ink.g, ink.r].map(|c| (c.clamp(0., 1.) * 255.).round() as u8);
    let mut pixels = bytes.to_vec();
    for p in pixels.as_chunks_mut::<4>().0 {
        p[..3].copy_from_slice(&[b, g, r]);
    }
    let buffer = image::RgbaImage::from_raw(w, h, pixels)?;
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

    #[allow(clippy::too_many_lines, reason = "one short loader per kind of media")]
    fn new(services: Services, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = flume::unbounded::<MediaEvent>();
        let thumbs = {
            let fs = Arc::clone(&services.fs);
            let tx = tx.clone();
            Loader::new("thumbnail", 3, move |key: &MediaKey| {
                #[cfg(test)]
                THUMBNAIL_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let thumb = thumb_cache().load(&key.thumb_key(), || {
                    fs.read(&key.path, MAX_THUMBNAIL_SOURCE)
                        .ok()
                        .and_then(|bytes| decode_thumbnail(&bytes, key.px.max(1)).ok())
                });
                let _ = tx.send(MediaEvent::Loaded(key.clone(), thumb));
            })
        };
        let icons = {
            let shell = Arc::clone(&services.shell);
            let tx = tx.clone();
            Loader::new("system-icon", 1, move |key: &MediaKey| {
                // Asked at the size it is drawn at (Windows then returns its hand-made small
                // frame), and brought down to exactly that: drawn from a bigger bitmap the GPU
                // samples every few source pixels and drops one-pixel lines.
                let icon = match key.type_icon_extension() {
                    Some(ext) => shell.type_icon(ext, key.px),
                    None => shell.file_icon(&key.path, icon_request_px(key.px)),
                }
                .and_then(|icon| fit_icon(icon, key.px));
                let _ = tx.send(MediaEvent::Loaded(key.clone(), icon));
            })
        };
        let videos = {
            let shell = Arc::clone(&services.shell);
            let tx = tx.clone();
            // Videos Windows had no thumbnail for: not asked again this run (each try can take
            // seconds, and a broken file stays broken). A changed file has a new key. Not when the
            // host couldn't be asked at all: that's not the file's doing.
            let failed: Arc<parking_lot::Mutex<HashSet<ThumbKey>>> = Arc::default();
            // One at a time: the host makes them one by one anyway.
            Loader::new("video-thumbnail", 1, move |key: &MediaKey| {
                let thumb_key = key.thumb_key();
                let thumb = if failed.lock().contains(&thumb_key) {
                    None
                } else {
                    let mut unavailable = false;
                    let made = thumb_cache().load(&thumb_key, || {
                        shell
                            .video_thumbnail(&key.path, key.px.max(1))
                            .unwrap_or_else(|_| {
                                unavailable = true;
                                None
                            })
                    });
                    if unavailable {
                        let _ = tx.send(MediaEvent::Unavailable(key.clone()));
                        return;
                    }
                    if made.is_none() {
                        failed.lock().insert(thumb_key);
                    }
                    made
                };
                let _ = tx.send(MediaEvent::Loaded(key.clone(), thumb));
            })
        };
        let pdfs = {
            let shell = Arc::clone(&services.shell);
            let tx = tx.clone();
            // Rendered in the thumbnail host, like videos: a damaged file can stall or crash the
            // renderer, and the host is ended after a timeout. A failed or unsupported file
            // (encrypted, damaged) stays a type icon: the store remembers the `None`. A host that
            // can't be asked at all says nothing about the file, so nothing is remembered.
            Loader::new("pdf-thumbnail", 2, move |key: &MediaKey| {
                let mut unavailable = false;
                let thumb = thumb_cache().load(&key.thumb_key(), || {
                    shell
                        .pdf_thumbnail(&key.path, key.px.max(1))
                        .unwrap_or_else(|_| {
                            unavailable = true;
                            None
                        })
                });
                if unavailable {
                    let _ = tx.send(MediaEvent::Unavailable(key.clone()));
                    return;
                }
                let _ = tx.send(MediaEvent::Loaded(key.clone(), thumb));
            })
        };
        thumb_cache().count_in_background();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                if this.update(cx, |this, cx| this.handle(event, cx)).is_err() {
                    break;
                }
            }
        });
        // A closed window's views pin nothing anymore.
        let this = cx.weak_entity();
        let window_closed = cx.on_window_closed(move |cx, window| {
            let _ = this.update(cx, |this, cx| {
                if this.pins.forget_window(window) {
                    this.apply_wanted();
                    this.evict(cx);
                }
            });
        });
        Self {
            services,
            decoded: Decoded::new(CACHE_BUDGET),
            pins: Pins::default(),
            thumbs,
            icons,
            videos,
            pdfs,
            decoded_in_frame: false,
            resend: false,
            local_disk: HashMap::new(),
            local_pending: HashSet::new(),
            tx,
            _task: task,
            _window_closed: window_closed,
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
                    MediaKind::Video => self.videos.acknowledge(&key),
                    MediaKind::Pdf => self.pdfs.acknowledge(&key),
                }
                self.insert(key, thumb);
                self.evict(cx);
            }
            MediaEvent::Unavailable(key) => {
                match key.kind {
                    MediaKind::Pdf => self.pdfs.acknowledge(&key),
                    _ => self.videos.acknowledge(&key),
                }
                // Asked again when the next frame ends.
                self.resend = true;
            }
            MediaEvent::LocalDisk(root, local) => {
                self.local_pending.remove(&root);
                self.local_disk.insert(root, local);
            }
        }
        cx.notify();
    }

    fn insert(&mut self, key: MediaKey, thumb: Option<Thumbnail>) {
        let image = thumb.and_then(to_render_image);
        let pixels = image.as_deref().map_or(0, image_bytes);
        // A copy made while one was already here (decoded in the frame while the loader made it
        // too) was never drawn: dropping it is enough.
        drop(self.decoded.insert(key, image, pixels));
    }

    /// Frees the least recently used decoded images over the budget, except the ones a view wants
    /// and the other-size pictures standing in for those still loading.
    fn evict(&mut self, cx: &mut App) {
        if !self.decoded.over_budget() {
            return;
        }
        let stand_ins: HashSet<MediaKey> = self
            .pins
            .pinned()
            .filter(|key| !self.decoded.contains(key))
            .filter_map(|key| self.stand_in(key))
            .collect();
        let pins = &self.pins;
        for image in self
            .decoded
            .evict(|key| pins.is_pinned(key) || stand_ins.contains(key))
        {
            // Remove from every window's GPU atlas; dropping the reference alone leaves the
            // texture in video memory.
            drop_image_later(image, cx);
        }
    }

    /// Loaded thumbnails and icons (for the benchmark report).
    pub(crate) fn loaded_count(&self) -> usize {
        self.decoded.image_count()
    }

    /// Loaded entries (tests).
    #[cfg(test)]
    pub(crate) fn loaded(&self) -> Vec<(PathBuf, MediaKind)> {
        let mut v: Vec<_> = self
            .decoded
            .iter()
            .filter(|(_, img)| img.is_some())
            .map(|(k, _)| (k.path.clone(), k.kind))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Frees every decoded image and forgets what was wanted, like scrolling far away and back.
    /// Used when the last window closes, and by tests.
    pub(crate) fn drop_decoded(&mut self, cx: &mut App) {
        for image in self.decoded.drain() {
            drop_image_later(image, cx);
        }
        self.pins.clear();
        self.decoded_in_frame = false;
    }

    /// Decoded bytes held (tests).
    #[cfg(test)]
    pub(crate) const fn decoded_bytes(&self) -> usize {
        self.decoded.bytes()
    }

    /// With no window open: frees decoded images and the in-memory compressed thumbnails. The
    /// disk cache stays, so the next window gets its thumbnails back without decoding again.
    pub(crate) fn release_all(cx: &mut App) {
        if let Some(store) = cx.try_global::<GlobalMedia>().map(|g| g.0.clone()) {
            store.update(cx, |store, cx| store.drop_decoded(cx));
        }
        thumb_cache().release_memory();
    }

    /// Loaded image (`None` if failed or not loaded yet).
    pub(crate) fn get(&self, key: &MediaKey) -> Option<Arc<RenderImage>> {
        self.decoded.image(key).cloned()
    }

    /// Like [`Self::get`], but while a thumbnail isn't loaded at this size yet, the same file's
    /// thumbnail at the closest other size (after an icon size change the grid keeps its pictures
    /// instead of blinking to type icons).
    pub(crate) fn get_or_nearby(&self, key: &MediaKey) -> Option<Arc<RenderImage>> {
        if self.decoded.contains(key) {
            return self.get(key);
        }
        self.stand_in(key).and_then(|other| self.get(&other))
    }

    /// For a picture not loaded at its own size: the key of the same file's picture at the closest
    /// other size that is loaded.
    fn stand_in(&self, key: &MediaKey) -> Option<MediaKey> {
        if !key.kind.is_picture() {
            return None;
        }
        let mut others: Vec<u32> = SOURCE_SIZES.into_iter().filter(|&s| s != key.px).collect();
        others.sort_by_key(|&s| s.abs_diff(key.px));
        let mut probe = key.clone();
        others.into_iter().find_map(|px| {
            probe.px = px;
            self.decoded.image(&probe).is_some().then(|| probe.clone())
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
            let shell = Arc::clone(&self.services.shell);
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

    /// `view` (an explorer pane) starts drawing in `window`. What it asks for until the frame ends
    /// replaces what it wanted before, so asking for nothing lets its pictures go; views of this
    /// window that don't draw now (a tab switched away) stop holding theirs.
    pub(crate) fn drawing(&mut self, window: WindowId, view: EntityId, cx: &mut Context<Self>) {
        self.open_frame(cx);
        self.pins.drawing(window, view);
    }

    /// Items `view`'s main list needs now (in priority order); it may ask several times per frame
    /// (one list per group). Already cached ones are skipped; ones with a compressed copy in memory
    /// are decoded right away while that stays within a few milliseconds, so scrolling back shows
    /// them in the same frame.
    pub(crate) fn want(&mut self, view: EntityId, keys: Vec<MediaKey>, cx: &mut Context<Self>) {
        self.open_frame(cx);
        let start = Instant::now();
        for key in &keys {
            if start.elapsed() > SYNC_DECODE {
                break;
            }
            if !key.kind.is_picture() || self.decoded.contains(key) {
                continue;
            }
            if let Some(thumb) = thumb_cache()
                .in_memory(&key.thumb_key())
                .and_then(|bytes| arke_core::thumb_cache::decode(&bytes))
            {
                self.insert(key.clone(), Some(thumb));
                self.decoded_in_frame = true;
            }
        }
        self.pins.want(view, false, keys);
    }

    /// Items for `view`'s side column: queued after every main list and never cancel them.
    pub(crate) fn want_side(
        &mut self,
        view: EntityId,
        keys: Vec<MediaKey>,
        cx: &mut Context<Self>,
    ) {
        self.open_frame(cx);
        self.pins.want(view, true, keys);
    }

    /// Ends the frame once the update under way is done. Not now: this runs while a window draws,
    /// and `drop_image` skips the window being updated, so its GPU atlas would keep every freed
    /// thumbnail.
    fn open_frame(&mut self, cx: &mut Context<Self>) {
        if self.pins.frame_open() {
            return;
        }
        let this = cx.entity();
        cx.defer(move |cx| this.update(cx, Self::end_frame));
    }

    /// What the views asked for this frame takes effect: the loaders hear of it (only if it
    /// changed) and what no view holds anymore may be evicted.
    fn end_frame(&mut self, cx: &mut Context<Self>) {
        let changed = self.pins.end_frame();
        let resend = std::mem::take(&mut self.resend);
        let decoded = std::mem::take(&mut self.decoded_in_frame);
        if changed || resend {
            self.apply_wanted();
        }
        if changed || decoded {
            self.evict(cx);
        }
    }

    fn apply_wanted(&mut self) {
        let keys = self.pins.wanted();
        let (mut thumbs, mut icons, mut videos, mut pdfs) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for key in &keys {
            if self.decoded.contains(key) {
                continue;
            }
            match key.kind {
                MediaKind::Thumbnail => thumbs.push(key.clone()),
                MediaKind::SystemIcon => icons.push(key.clone()),
                MediaKind::Video => videos.push(key.clone()),
                MediaKind::Pdf => pdfs.push(key.clone()),
            }
        }
        self.thumbs.set_wanted(thumbs);
        self.icons.set_wanted(icons);
        self.videos.set_wanted(videos);
        self.pdfs.set_wanted(pdfs);
        // Wanted ones become the most recently used, the first (on screen) last: once no view
        // holds them, they are the last to go.
        for key in keys.iter().rev() {
            self.decoded.touch(key);
        }
    }
}

fn image_bytes(image: &RenderImage) -> usize {
    let size = image.size(0);
    (size.width.0 as usize) * (size.height.0 as usize) * 4
}

/// Frees `image` from every window's GPU atlas once the update under way is done. GPUI leaves out
/// the window it is updating, and nearly everything runs inside one (rendering, key and mouse
/// handlers), so dropping it right away kept that window's copy in video memory: the preview pane
/// kept every picture it had shown, about 7 MB each.
pub(crate) fn drop_image_later(image: Arc<RenderImage>, cx: &mut App) {
    cx.defer(move |cx| cx.drop_image(image, None));
}

#[cfg(test)]
mod dark_mono_tests {
    use super::{MenuIcon, is_dark_mono};
    use gpui::RenderImage;
    use std::sync::Arc;

    /// BGRA pixels from (b, g, r, a) tuples.
    fn px(list: &[[u8; 4]]) -> Vec<u8> {
        list.concat()
    }

    #[test]
    fn dark_gray_glyph_is_mono() {
        assert!(is_dark_mono(&px(&[
            [20, 20, 20, 255],
            [30, 30, 30, 255],
            [0, 0, 0, 0]
        ])));
    }

    #[test]
    fn colored_icon_is_not_mono() {
        assert!(!is_dark_mono(&px(&[
            [200, 120, 30, 255],
            [20, 20, 20, 255]
        ])));
        assert!(!is_dark_mono(&px(&[[30, 30, 90, 255]])));
    }

    #[test]
    fn gray_antialiased_edges_stay_mono() {
        assert!(is_dark_mono(&px(&[
            [10, 10, 10, 255],
            [60, 60, 60, 128],
            [90, 90, 90, 40],
            [0, 0, 0, 0],
        ])));
    }

    #[test]
    fn empty_or_transparent_is_not_mono() {
        assert!(!is_dark_mono(&[]));
        assert!(!is_dark_mono(&px(&[[0, 0, 0, 0], [5, 5, 5, 0]])));
    }

    #[test]
    fn light_gray_glyph_is_not_mono() {
        assert!(!is_dark_mono(&px(&[[220, 220, 220, 255]])));
    }

    #[test]
    fn twin_keeps_alpha_and_changes_color() {
        let buffer =
            image::RgbaImage::from_raw(2, 1, px(&[[10, 10, 10, 255], [0, 0, 0, 0]])).unwrap();
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        let icon = MenuIcon::new(image);
        let twin = icon.light_on_dark.as_ref().expect("dark glyph gets a twin");
        let bytes = twin.as_bytes(0).unwrap();
        assert_eq!([bytes[3], bytes[7]], [255, 0]);
        assert!(bytes[0] > 100);
        // The original is untouched.
        assert_eq!(icon.original().as_bytes(0).unwrap()[0], 10);
    }
}

#[cfg(test)]
mod icon_fit_tests {
    use super::{MediaKey, MediaKind, fit_icon, icon_request_px, icon_source_px};
    use arke_core::thumbs::Thumbnail;
    use std::path::PathBuf;

    /// `side` x `side`, opaque `ink` in the first `stripe` columns, transparent elsewhere.
    fn left_stripe(side: u32, stripe: u32, ink: [u8; 4]) -> Thumbnail {
        let mut rgba = Vec::new();
        for _ in 0..side {
            for x in 0..side {
                rgba.extend_from_slice(if x < stripe { &ink } else { &[0, 0, 0, 0] });
            }
        }
        Thumbnail {
            width: side,
            height: side,
            rgba,
        }
    }

    #[test]
    fn an_icon_at_its_size_is_untouched() {
        let icon = left_stripe(16, 1, [10, 120, 10, 255]);
        assert_eq!(fit_icon(icon.clone(), 16), Some(icon.clone()));
        assert_eq!(fit_icon(icon.clone(), 0), Some(icon.clone()));
        // Never enlarged.
        assert_eq!(fit_icon(icon.clone(), 64), Some(icon));
    }

    #[test]
    fn an_opaque_left_column_survives_the_shrink() {
        // Content touching the left edge: a stripe 4 px wide in 64 is one column in 16.
        let small = fit_icon(left_stripe(64, 4, [10, 120, 10, 255]), 16).unwrap();
        assert_eq!((small.width, small.height), (16, 16));
        for y in 0..16usize {
            let p = &small.rgba[y * 16 * 4..y * 16 * 4 + 4];
            assert!(p[3] >= 250, "row {y} left column faded: {p:?}");
            assert!(p[0].abs_diff(10) <= 12 && p[1].abs_diff(120) <= 12, "{p:?}");
        }
        // The right half stays empty.
        assert_eq!(small.rgba[15 * 4 + 3], 0);
    }

    #[test]
    fn transparent_pixels_do_not_darken_the_edge() {
        // White on the left half, transparent black on the right.
        let icon = left_stripe(64, 32, [255, 255, 255, 255]);
        let small = fit_icon(icon, 16).unwrap();
        for p in small.rgba.as_chunks::<4>().0 {
            if p[3] >= 128 {
                assert!(p[..3].iter().all(|&c| c >= 240), "dark fringe: {p:?}");
            }
        }
    }

    #[test]
    fn the_longest_side_is_fitted_and_the_ratio_kept() {
        let wide = Thumbnail {
            width: 64,
            height: 32,
            rgba: vec![255; 64 * 32 * 4],
        };
        let small = fit_icon(wide, 16).unwrap();
        assert_eq!((small.width, small.height), (16, 8));
        let broken = Thumbnail {
            width: 64,
            height: 64,
            rgba: vec![0; 10],
        };
        assert!(fit_icon(broken, 16).is_none());
    }

    #[test]
    fn icons_are_asked_at_the_physical_size_they_are_drawn() {
        assert_eq!(icon_source_px(16., 1.), 16);
        assert_eq!(icon_source_px(16., 1.25), 20);
        assert_eq!(icon_source_px(16., 1.5), 24);
        assert_eq!(icon_source_px(16., 2.), 32);
        // Bigger tiles stretch the largest icon; a scale below 1 doesn't shrink it.
        assert_eq!(icon_source_px(48., 2.), 64);
        assert_eq!(icon_source_px(16., 0.5), 16);
        assert_eq!(icon_request_px(0), 64);
        assert_eq!(icon_request_px(20), 20);
        assert_eq!(icon_request_px(200), 64);
    }

    #[test]
    fn only_system_icon_keys_take_a_size() {
        let key = |kind| MediaKey {
            path: PathBuf::from(r"C:\a.exe"),
            modified: None,
            size: 1,
            kind,
            px: 0,
        };
        assert_eq!(key(MediaKind::SystemIcon).with_icon_px(20).px, 20);
        assert_eq!(key(MediaKind::Thumbnail).with_icon_px(20).px, 0);
        assert_ne!(
            key(MediaKind::SystemIcon).with_icon_px(16),
            key(MediaKind::SystemIcon).with_icon_px(20),
            "each size is its own picture"
        );
    }
}
