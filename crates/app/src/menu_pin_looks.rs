//! How pinned Windows commands last looked: their icon, and whether the row is a whole submenu.
//! A row drawn before its source answers (`explorer_menu::coming_row`) shows it already. Kept on
//! disk too, so the first menu after Arke starts has the icons as well (#278).
//!
//! Only a change is written, a moment later, on a thread of its own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use gpui::RenderImage;
use parking_lot::Mutex;

#[derive(Clone, Default)]
struct Look {
    icon: Option<Arc<RenderImage>>,
    submenu: bool,
}

static LOOKS: LazyLock<Mutex<HashMap<String, Look>>> = LazyLock::new(Mutex::default);
/// A write is already on its way.
static SAVING: AtomicBool = AtomicBool::new(false);

/// File header, with the format's version.
const MAGIC: &[u8; 4] = b"AMI1";
/// Menu icons are 16 to 32 px; anything past this isn't one of ours.
const MAX_SIDE: u16 = 256;
/// The write waits this long, so the rows filled in one after another save once.
const SAVE_AFTER: Duration = Duration::from_secs(2);

/// The icon pinned row `key` last had.
pub(crate) fn icon(key: &str) -> Option<Arc<RenderImage>> {
    LOOKS.lock().get(key).and_then(|l| l.icon.clone())
}

/// Whether pinned row `key` was last seen as a whole submenu.
pub(crate) fn is_submenu(key: &str) -> bool {
    LOOKS.lock().get(key).is_some_and(|l| l.submenu)
}

/// Notes how pinned row `key` looks now (an icon left out keeps the one it had); saved a moment
/// later when that changed anything.
pub(crate) fn remember(key: &str, icon: Option<&Arc<RenderImage>>, submenu: bool) {
    if note(&mut LOOKS.lock(), key, icon, submenu) {
        save_soon();
    }
}

/// Puts the look in `looks`; whether that changed it.
fn note(
    looks: &mut HashMap<String, Look>,
    key: &str,
    icon: Option<&Arc<RenderImage>>,
    submenu: bool,
) -> bool {
    if !looks.contains_key(key) {
        looks.insert(key.to_string(), Look::default());
    }
    let Some(look) = looks.get_mut(key) else {
        return false;
    };
    let same_icon = icon.is_none_or(|new| look.icon.as_ref().is_some_and(|old| same(old, new)));
    if same_icon && look.submenu == submenu {
        return false;
    }
    if let Some(new) = icon.filter(|_| !same_icon) {
        look.icon = Some(Arc::clone(new));
    }
    look.submenu = submenu;
    true
}

/// The same picture: the same image, or the same size and pixels (each menu read makes new
/// images; the one already drawn is kept).
fn same(a: &Arc<RenderImage>, b: &Arc<RenderImage>) -> bool {
    Arc::ptr_eq(a, b) || (a.size(0) == b.size(0) && a.as_bytes(0) == b.as_bytes(0))
}

fn file() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Arke").join("menu-icons.bin"))
}

/// Reads what was saved, for the commands still in `pinned`, keeping anything noted meanwhile.
/// Background thread, at startup.
pub(crate) fn load(pinned: &[String]) {
    if cfg!(test) || pinned.is_empty() {
        return;
    }
    let Some(bytes) = file().and_then(|f| std::fs::read(f).ok()) else {
        return;
    };
    let Some(saved) = decode(&bytes) else {
        log::warn!("menu icons file unreadable, ignored");
        return;
    };
    let mut looks = LOOKS.lock();
    for (key, look) in saved {
        if pinned.contains(&key) {
            looks.entry(key).or_insert(look);
        }
    }
}

fn save_soon() {
    if cfg!(test) || SAVING.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("menu-icons-save".into())
        .spawn(|| {
            std::thread::sleep(SAVE_AFTER);
            SAVING.store(false, Ordering::Release);
            let bytes = encode(&LOOKS.lock());
            let Some(path) = file() else { return };
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Written whole, then put in place: a crash midway leaves the old file.
            let tmp = path.with_extension("tmp");
            if let Err(e) = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path))
            {
                log::warn!("menu icons not saved: {e}");
            }
        });
}

/// `MAGIC`, then per row: key length (u16 LE), key (UTF-8), flags (1: submenu, 2: icon), and with
/// an icon its width and height (u16 LE) and BGRA pixels.
fn encode(looks: &HashMap<String, Look>) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for (key, look) in looks {
        let Ok(len) = u16::try_from(key.len()) else {
            continue;
        };
        let pixels = look.icon.as_ref().and_then(|icon| {
            let size = icon.size(0);
            let w = u16::try_from(size.width.0)
                .ok()
                .filter(|&w| w <= MAX_SIDE)?;
            let h = u16::try_from(size.height.0)
                .ok()
                .filter(|&h| h <= MAX_SIDE)?;
            let bytes = icon.as_bytes(0)?;
            (bytes.len() == usize::from(w) * usize::from(h) * 4).then_some((w, h, bytes))
        });
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.push(u8::from(look.submenu) | if pixels.is_some() { 2 } else { 0 });
        if let Some((w, h, bytes)) = pixels {
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Vec<(String, Look)>> {
    let mut rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let mut take = |n: usize| -> Option<&[u8]> {
        let (head, tail) = rest.split_at_checked(n)?;
        rest = tail;
        Some(head)
    };
    let mut out = Vec::new();
    while let Some(len) = take(2) {
        let len = u16::from_le_bytes([len[0], len[1]]);
        let key = std::str::from_utf8(take(usize::from(len))?)
            .ok()?
            .to_string();
        let flags = take(1)?[0];
        let icon = if flags & 2 == 0 {
            None
        } else {
            let size = take(4)?;
            let w = u16::from_le_bytes([size[0], size[1]]);
            let h = u16::from_le_bytes([size[2], size[3]]);
            if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
                return None;
            }
            let pixels = take(usize::from(w) * usize::from(h) * 4)?.to_vec();
            let buffer = image::RgbaImage::from_raw(u32::from(w), u32::from(h), pixels)?;
            Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
        };
        out.push((
            key,
            Look {
                icon,
                submenu: flags & 1 != 0,
            },
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, fill: u8) -> Arc<RenderImage> {
        let buffer = image::RgbaImage::from_raw(w, h, vec![fill; (w * h * 4) as usize]).unwrap();
        Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
    }

    #[test]
    fn looks_round_trip_through_the_file() {
        let mut looks = HashMap::new();
        looks.insert(
            "7-Zip › Add to archive".to_string(),
            Look {
                icon: Some(image(16, 16, 7)),
                submenu: false,
            },
        );
        looks.insert(
            "Send to".to_string(),
            Look {
                icon: None,
                submenu: true,
            },
        );
        let mut back = decode(&encode(&looks)).unwrap();
        back.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(back.len(), 2);
        let (key, look) = &back[0];
        assert_eq!(key, "7-Zip › Add to archive");
        assert!(!look.submenu);
        let icon = look.icon.as_ref().unwrap();
        assert!(same(icon, &image(16, 16, 7)));
        assert!(!same(icon, &image(16, 16, 8)));
        assert_eq!(back[1].0, "Send to");
        assert!(back[1].1.submenu && back[1].1.icon.is_none());
    }

    #[test]
    fn a_damaged_file_reads_as_nothing() {
        let mut looks = HashMap::new();
        looks.insert(
            "x".to_string(),
            Look {
                icon: Some(image(16, 16, 1)),
                submenu: false,
            },
        );
        let bytes = encode(&looks);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none(), "cut short");
        assert!(decode(b"nope").is_none(), "not ours");
        assert_eq!(decode(MAGIC).map(|v| v.len()), Some(0), "empty");
    }
}
