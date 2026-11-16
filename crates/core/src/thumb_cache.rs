//! Compressed thumbnails, kept in memory and on disk so a folder seen before fills at once.
//!
//! - A decoded thumbnail (RGBA, about 150 KB at 192 px) is stored compressed: JPEG when opaque
//!   (about 5 KB for a photo), lossless WebP when it has transparency. Decoding one takes about
//!   0.1–0.2 ms, against 7–30 ms or more for reading and scaling the original.
//! - Memory: least recently used entries go first once the byte budget is reached.
//! - Disk: one file per thumbnail under the cache folder, named by a hash of the key; the key is
//!   stored in the file too, so a hash collision reads as a miss. Edited files get a new key (the
//!   key has modified time and size), and old entries age out: when the folder is over its cap the
//!   least recently used files are deleted (a read refreshes a file's time at most once a day).
//! - Windows' own thumbnail cache isn't used: a hit there costs about as much as decoding a small
//!   original, it goes through third-party thumbnail providers, and Disk Cleanup empties it.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use image::{ExtendedColorType, ImageEncoder};
use parking_lot::Mutex;

use crate::thumbs::Thumbnail;

/// JPEG quality for opaque thumbnails: no visible artifacts at thumbnail size.
const JPEG_QUALITY: u8 = 90;
/// File signature and format version; files from another version read as a miss.
const MAGIC: &[u8; 5] = b"ARKT1";
/// A read refreshes the file's time (for least-recently-used trimming) at most this often.
const TOUCH_AFTER: Duration = Duration::from_hours(24);
/// Trimming goes down to this share of the cap, so it doesn't run again after every write.
const TRIM_TO_PERCENT: u64 = 80;

/// Which thumbnail: the source file as listed, and the size it is decoded at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThumbKey {
    /// Source file.
    pub path: PathBuf,
    /// Source modified time.
    pub modified: Option<SystemTime>,
    /// Source size in bytes.
    pub size: u64,
    /// Longest side the thumbnail is decoded at.
    pub px: u32,
}

impl ThumbKey {
    /// The key as bytes (stored in the cache file and hashed for its name).
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(self.path.as_os_str().as_encoded_bytes());
        out.push(0);
        let nanos = self
            .modified
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(u128::MAX, |d| d.as_nanos());
        out.extend_from_slice(&nanos.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.px.to_le_bytes());
        out
    }
}

/// 64-bit FNV-1a: stable across runs and Rust versions (the std hasher isn't promised to be).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Compresses a thumbnail: JPEG when every pixel is opaque, lossless WebP otherwise.
pub fn encode(thumb: &Thumbnail) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let opaque = thumb
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[3] == u8::MAX);
    if opaque {
        let rgb: Vec<u8> = thumb
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .write_image(&rgb, thumb.width, thumb.height, ExtendedColorType::Rgb8)
            .ok()?;
    } else {
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .write_image(
                &thumb.rgba,
                thumb.width,
                thumb.height,
                ExtendedColorType::Rgba8,
            )
            .ok()?;
    }
    Some(out)
}

/// Decodes what [`encode`] made.
pub fn decode(bytes: &[u8]) -> Option<Thumbnail> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(Thumbnail {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// In-memory compressed thumbnails with a byte budget; least recently used go first.
struct Memory {
    map: HashMap<ThumbKey, (Arc<[u8]>, u64)>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl Memory {
    fn get(&mut self, key: &ThumbKey) -> Option<Arc<[u8]>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|(bytes, used)| {
            *used = tick;
            Arc::clone(bytes)
        })
    }

    fn insert(&mut self, key: ThumbKey, bytes: Arc<[u8]>) {
        self.tick += 1;
        self.bytes += bytes.len();
        if let Some((old, _)) = self.map.insert(key, (bytes, self.tick)) {
            self.bytes -= old.len();
        }
        if self.bytes > self.budget {
            // Down to 90% at once: sorting every insert would cost more than the decode it saves.
            let mut by_age: Vec<(u64, ThumbKey)> = self
                .map
                .iter()
                .map(|(k, (_, used))| (*used, k.clone()))
                .collect();
            by_age.sort_unstable_by_key(|(used, _)| *used);
            let target = self.budget / 10 * 9;
            for (_, key) in by_age {
                if self.bytes <= target {
                    break;
                }
                if let Some((old, _)) = self.map.remove(&key) {
                    self.bytes -= old.len();
                }
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    /// Like [`Self::clear`], and also gives the table's memory back.
    fn release(&mut self) {
        self.map = HashMap::new();
        self.bytes = 0;
    }
}

/// The cache folder: one file per thumbnail in 256 subfolders, with a size cap.
struct Disk {
    dir: PathBuf,
    cap: u64,
    /// Bytes in the folder; `None` until the first scan finished.
    used: Mutex<Option<u64>>,
    trimming: AtomicBool,
    /// Bumped by [`ThumbCache::clear`]; a write started before it doesn't count its bytes.
    generation: AtomicU64,
}

impl Disk {
    fn file(&self, key_bytes: &[u8]) -> PathBuf {
        let name = format!("{:016x}", fnv1a(key_bytes));
        self.dir.join(&name[..2]).join(name)
    }

    fn read(&self, key: &ThumbKey) -> Option<Vec<u8>> {
        let key_bytes = key.bytes();
        let path = self.file(&key_bytes);
        let mut file = File::open(&path).ok()?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).ok()?;
        let rest = data.strip_prefix(MAGIC)?;
        let (len, rest) = rest.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        let (stored, image) = rest.split_at_checked(len)?;
        if stored != key_bytes {
            return None;
        }
        let stale = file
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age > TOUCH_AFTER);
        if stale {
            drop(file);
            if let Ok(f) = File::options().write(true).open(&path) {
                let _ = f.set_modified(SystemTime::now());
            }
        }
        Some(image.to_vec())
    }

    /// Writes through a temporary file, so a crash never leaves half an entry under the real name.
    fn write(&self, key: &ThumbKey, image: &[u8]) -> std::io::Result<u64> {
        let key_bytes = key.bytes();
        let path = self.file(&key_bytes);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("{:?}.tmp", std::thread::current().id()));
        let len = u32::try_from(key_bytes.len()).map_err(std::io::Error::other)?;
        let mut file = File::create(&tmp)?;
        file.write_all(MAGIC)?;
        file.write_all(&len.to_le_bytes())?;
        file.write_all(&key_bytes)?;
        file.write_all(image)?;
        drop(file);
        let size = (MAGIC.len() + 4 + key_bytes.len() + image.len()) as u64;
        let replaced = fs::metadata(&path).map_or(0, |m| m.len());
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(size.saturating_sub(replaced))
    }

    /// Every cache file with its time and size.
    fn files(&self) -> Vec<(SystemTime, u64, PathBuf)> {
        let Ok(subdirs) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        subdirs
            .flatten()
            .filter_map(|d| fs::read_dir(d.path()).ok())
            .flat_map(Iterator::flatten)
            .filter_map(|f| {
                let meta = f.metadata().ok()?;
                meta.is_file()
                    .then(|| (meta.modified().unwrap_or(UNIX_EPOCH), meta.len(), f.path()))
            })
            .collect()
    }

    /// Where [`ThumbCache::clear`] moves the folder before deleting it.
    fn aside(&self) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut name = self.dir.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".old-{}-{nanos}", std::process::id()));
        self.dir.with_file_name(name)
    }

    /// Folders a clear moved aside but didn't finish deleting (Arke quit meanwhile).
    fn remove_leftovers(&self) {
        let (Some(parent), Some(name)) = (self.dir.parent(), self.dir.file_name()) else {
            return;
        };
        let mut prefix = name.to_os_string();
        prefix.push(".old-");
        let prefix = prefix.to_string_lossy().into_owned();
        let Ok(entries) = fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Counts the folder, then deletes least recently used files while it is over the cap.
    fn trim(&self) {
        self.remove_leftovers();
        let generation = self.generation.load(Ordering::Acquire);
        let mut files = self.files();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        if total > self.cap {
            files.sort_unstable_by_key(|(time, _, _)| *time);
            let target = self.cap / 100 * TRIM_TO_PERCENT;
            for (_, size, path) in files {
                if total <= target {
                    break;
                }
                if fs::remove_file(&path).is_ok() {
                    total -= size;
                }
            }
        }
        if self.generation.load(Ordering::Acquire) == generation {
            *self.used.lock() = Some(total);
        }
        self.trimming.store(false, Ordering::Release);
    }
}

/// Memory and disk cache of compressed thumbnails, shared by the loader threads.
pub struct ThumbCache {
    memory: Mutex<Memory>,
    disk: Option<Arc<Disk>>,
}

impl ThumbCache {
    /// `memory_budget` in bytes of compressed data; `disk`: the cache folder and its cap in bytes
    /// (`None`: memory only).
    pub fn new(memory_budget: usize, disk: Option<(PathBuf, u64)>) -> Self {
        Self {
            memory: Mutex::new(Memory {
                map: HashMap::new(),
                bytes: 0,
                budget: memory_budget,
                tick: 0,
            }),
            disk: disk.map(|(dir, cap)| {
                Arc::new(Disk {
                    dir,
                    cap,
                    used: Mutex::new(None),
                    trimming: AtomicBool::new(false),
                    generation: AtomicU64::new(0),
                })
            }),
        }
    }

    /// Compressed thumbnail in memory, without touching the disk (cheap enough for the UI thread).
    pub fn in_memory(&self, key: &ThumbKey) -> Option<Arc<[u8]>> {
        self.memory.lock().get(key)
    }

    /// Bytes of compressed thumbnails held in memory.
    pub fn memory_bytes(&self) -> usize {
        self.memory.lock().bytes
    }

    /// Frees the memory layer and keeps the cache folder, so thumbnails come back from disk.
    pub fn release_memory(&self) {
        self.memory.lock().release();
    }

    /// The thumbnail from memory, else from disk, else made by `source` and stored in both. On a
    /// background thread (it may read the disk and decode).
    pub fn load(
        &self,
        key: &ThumbKey,
        source: impl FnOnce() -> Option<Thumbnail>,
    ) -> Option<Thumbnail> {
        if let Some(thumb) = self.in_memory(key).and_then(|bytes| decode(&bytes)) {
            return Some(thumb);
        }
        if let Some(disk) = &self.disk
            && let Some(bytes) = disk.read(key)
            && let Some(thumb) = decode(&bytes)
        {
            self.memory.lock().insert(key.clone(), bytes.into());
            return Some(thumb);
        }
        let thumb = source()?;
        if let Some(bytes) = encode(&thumb) {
            if let Some(disk) = &self.disk {
                Self::store(disk, key, &bytes);
            }
            self.memory.lock().insert(key.clone(), bytes.into());
        }
        Some(thumb)
    }

    fn store(disk: &Arc<Disk>, key: &ThumbKey, bytes: &[u8]) {
        let generation = disk.generation.load(Ordering::Acquire);
        let added = match disk.write(key, bytes) {
            Ok(added) => added,
            Err(err) => {
                log::debug!("thumbnail cache write: {err}");
                return;
            }
        };
        let over = {
            let mut used = disk.used.lock();
            match used.as_mut() {
                Some(used) if disk.generation.load(Ordering::Acquire) == generation => {
                    *used += added;
                    *used > disk.cap
                }
                // Not counted yet: the first write counts the folder (and trims it if needed).
                None => true,
                Some(_) => false,
            }
        };
        if over {
            Self::trim_in_background(disk);
        }
    }

    fn trim_in_background(disk: &Arc<Disk>) {
        if disk.trimming.swap(true, Ordering::AcqRel) {
            return;
        }
        let disk = Arc::clone(disk);
        let spawned = std::thread::Builder::new()
            .name("thumb-cache-trim".into())
            .spawn(move || disk.trim());
        if let Err(err) = spawned {
            log::error!("cannot spawn thread: {err}");
        }
    }

    /// Bytes the cache folder uses; counts it the first time (call off the UI thread).
    pub fn disk_bytes(&self) -> u64 {
        let Some(disk) = &self.disk else {
            return 0;
        };
        let counted = *disk.used.lock();
        if let Some(used) = counted {
            return used;
        }
        let total: u64 = disk.files().iter().map(|(_, size, _)| size).sum();
        *disk.used.lock() = Some(total);
        total
    }

    /// Bytes the cache folder uses if counted already; never touches the disk (UI thread).
    pub fn counted_disk_bytes(&self) -> Option<u64> {
        let disk = self.disk.as_ref()?;
        *disk.used.lock()
    }

    /// Counts the cache folder in the background, trimming it if it is over the cap (at startup,
    /// so Settings can show its size and a smaller cap applies).
    pub fn count_in_background(&self) {
        if let Some(disk) = &self.disk {
            Self::trim_in_background(disk);
        }
    }

    /// Empties memory and the cache folder. The folder is renamed aside at once and deleted in the
    /// background (thousands of small files take seconds); what a quit interrupts is deleted at the
    /// next start.
    pub fn clear(&self) -> std::io::Result<()> {
        self.memory.lock().clear();
        let Some(disk) = &self.disk else {
            return Ok(());
        };
        disk.generation.fetch_add(1, Ordering::AcqRel);
        let aside = disk.aside();
        let result = match fs::rename(&disk.dir, &aside) {
            Ok(()) => {
                let spawned = std::thread::Builder::new()
                    .name("thumb-cache-clear".into())
                    .spawn(move || {
                        let _ = fs::remove_dir_all(aside);
                    });
                if let Err(err) = spawned {
                    log::error!("cannot spawn thread: {err}");
                }
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // Renaming fails while a file in it is open: delete in place instead.
            Err(_) => match fs::remove_dir_all(&disk.dir) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            },
        };
        // A file still being written survives; count what is left.
        let left: u64 = disk.files().iter().map(|(_, size, _)| size).sum();
        *disk.used.lock() = Some(left);
        result
    }

    /// The cache folder (`None`: memory only).
    pub fn dir(&self) -> Option<&Path> {
        self.disk.as_ref().map(|d| d.dir.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "arke-thumbcache-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(name: &str, px: u32) -> ThumbKey {
        ThumbKey {
            path: PathBuf::from(format!(r"C:\pics\{name}")),
            modified: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            size: 1234,
            px,
        }
    }

    /// A smooth gradient, like a photo (noise would compress badly and isn't what thumbnails are).
    fn photo(w: u32, h: u32, alpha: u8) -> Thumbnail {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 128, alpha]);
            }
        }
        Thumbnail {
            width: w,
            height: h,
            rgba,
        }
    }

    #[test]
    fn opaque_thumbnails_become_small_jpegs() {
        let thumb = photo(192, 128, 255);
        let bytes = encode(&thumb).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "JPEG");
        assert!(bytes.len() < thumb.rgba.len() / 10, "{} bytes", bytes.len());
        let back = decode(&bytes).unwrap();
        assert_eq!((back.width, back.height), (192, 128));
        assert!(back.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn transparent_thumbnails_stay_exact() {
        let thumb = photo(64, 48, 100);
        let bytes = encode(&thumb).unwrap();
        assert!(bytes.starts_with(b"RIFF"), "WebP");
        assert_eq!(decode(&bytes).unwrap(), thumb);
    }

    #[test]
    fn a_second_load_comes_from_memory_and_then_from_disk() {
        let tmp = TempDir::new("layers");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 1 << 20)));
        let k = key("a.jpg", 192);
        let made = cache.load(&k, || Some(photo(192, 128, 255))).unwrap();
        assert_eq!((made.width, made.height), (192, 128));
        // Memory: the source isn't asked again.
        assert!(cache.in_memory(&k).is_some());
        assert!(cache.load(&k, || panic!("decoded again")).is_some());
        // A new process (empty memory) reads it from disk.
        let fresh = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 1 << 20)));
        assert!(fresh.in_memory(&k).is_none());
        let from_disk = fresh.load(&k, || panic!("decoded again")).unwrap();
        assert_eq!((from_disk.width, from_disk.height), (192, 128));
        assert!(fresh.in_memory(&k).is_some());
    }

    #[test]
    fn an_edited_file_or_another_size_is_a_miss() {
        let tmp = TempDir::new("keys");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 1 << 20)));
        let k = key("a.jpg", 192);
        cache.load(&k, || Some(photo(32, 32, 255)));
        let mut edited = k.clone();
        edited.size += 1;
        let mut bigger = k;
        bigger.px = 256;
        for other in [edited, bigger] {
            let mut asked = false;
            cache.load(&other, || {
                asked = true;
                Some(photo(32, 32, 255))
            });
            assert!(asked, "{other:?}");
        }
    }

    #[test]
    fn a_file_holding_another_key_reads_as_a_miss() {
        let tmp = TempDir::new("collision");
        let disk = Disk {
            dir: tmp.0.clone(),
            cap: 1 << 20,
            used: Mutex::new(None),
            trimming: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        };
        let a = key("a.jpg", 192);
        let b = key("b.jpg", 192);
        disk.write(&a, b"image").unwrap();
        // Pretend b hashes to a's file.
        fs::rename(disk.file(&a.bytes()), {
            let p = disk.file(&b.bytes());
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            p
        })
        .unwrap();
        assert_eq!(disk.read(&b), None);
        disk.write(&b, b"other").unwrap();
        assert_eq!(disk.read(&b).as_deref(), Some(&b"other"[..]));
        // Truncated or foreign files too.
        fs::write(disk.file(&a.bytes()), b"ARKT1\xff").unwrap();
        assert_eq!(disk.read(&a), None);
    }

    #[test]
    fn memory_drops_the_least_recently_used_first() {
        let cache = ThumbCache::new(1000, None);
        let (a, b, c) = (key("a", 1), key("b", 1), key("c", 1));
        cache.memory.lock().insert(a.clone(), vec![0; 400].into());
        cache.memory.lock().insert(b.clone(), vec![0; 400].into());
        // Using a makes b the oldest.
        assert!(cache.in_memory(&a).is_some());
        cache.memory.lock().insert(c.clone(), vec![0; 400].into());
        assert!(cache.in_memory(&a).is_some());
        assert!(cache.in_memory(&b).is_none());
        assert!(cache.in_memory(&c).is_some());
        assert!(cache.memory.lock().bytes <= 900);
    }

    #[test]
    fn the_folder_is_trimmed_oldest_first_when_over_its_cap() {
        let tmp = TempDir::new("trim");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 10_000)));
        let disk = Arc::clone(cache.disk.as_ref().unwrap());
        let keys: Vec<ThumbKey> = (0..6).map(|i| key(&format!("{i}.jpg"), 192)).collect();
        for (i, k) in keys.iter().enumerate() {
            disk.write(k, &[0; 3000]).unwrap();
            // Oldest first: 0.jpg is the least recently used.
            let t = SystemTime::now() - Duration::from_secs(1000 - i as u64);
            File::options()
                .write(true)
                .open(disk.file(&k.bytes()))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        assert!(cache.disk_bytes() > 18_000);
        disk.trimming.store(true, Ordering::Release);
        disk.trim();
        let used = cache.disk_bytes();
        assert!(used <= 8_000, "{used}");
        assert_eq!(disk.read(&keys[0]), None);
        assert!(disk.read(&keys[5]).is_some());
        assert!(!disk.trimming.load(Ordering::Acquire));
    }

    #[test]
    fn writes_count_toward_the_cap() {
        let tmp = TempDir::new("count");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 1 << 20)));
        assert_eq!(cache.disk_bytes(), 0);
        cache.load(&key("a.jpg", 192), || Some(photo(64, 64, 255)));
        let one = cache.disk_bytes();
        assert!(one > 0);
        // Writing the same key again replaces it.
        cache.memory.lock().clear();
        let disk = Arc::clone(cache.disk.as_ref().unwrap());
        let bytes = encode(&photo(64, 64, 255)).unwrap();
        ThumbCache::store(&disk, &key("a.jpg", 192), &bytes);
        assert_eq!(cache.disk_bytes(), one);
    }

    #[test]
    fn clear_empties_memory_and_the_folder() {
        let tmp = TempDir::new("clear");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 1 << 20)));
        let k = key("a.jpg", 192);
        cache.load(&k, || Some(photo(64, 64, 255)));
        assert!(cache.disk_bytes() > 0);
        cache.clear().unwrap();
        assert_eq!(
            cache.counted_disk_bytes(),
            Some(0),
            "counted as empty at once"
        );
        assert!(cache.in_memory(&k).is_none());
        assert!(!tmp.0.exists());
        let mut asked = false;
        cache.load(&k, || {
            asked = true;
            Some(photo(64, 64, 255))
        });
        assert!(asked);
        // Clearing an empty cache is fine.
        cache.clear().unwrap();
        cache.clear().unwrap();
        assert_eq!(ThumbCache::new(1, None).clear().ok(), Some(()));
    }

    #[test]
    fn a_clear_cut_short_is_finished_at_the_next_start() {
        let tmp = TempDir::new("leftover");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.join("thumbs"), 1 << 20)));
        cache.load(&key("a.jpg", 192), || Some(photo(64, 64, 255)));
        let disk = Arc::clone(cache.disk.as_ref().unwrap());
        // What clear leaves when Arke quits before the background delete ends.
        let aside = disk.aside();
        fs::rename(&disk.dir, &aside).unwrap();
        let unrelated = tmp.0.join("thumbs-notes");
        fs::create_dir_all(&unrelated).unwrap();
        disk.trimming.store(true, Ordering::Release);
        disk.trim();
        assert!(!aside.exists());
        assert!(unrelated.exists(), "only the cache's own leftovers");
    }

    #[test]
    fn a_failed_source_stores_nothing() {
        let tmp = TempDir::new("fail");
        let cache = ThumbCache::new(1 << 20, Some((tmp.0.clone(), 1 << 20)));
        assert_eq!(cache.load(&key("broken.jpg", 192), || None), None);
        assert_eq!(cache.disk_bytes(), 0);
    }
}
