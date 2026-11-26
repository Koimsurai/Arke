//! System icons (for `.exe`, `.lnk`, `.ico` only) and local-disk detection.
//!
//! Icons come from `IShellItemImageFactory::GetImage(SIIGBF_ICONONLY)`, which skips
//! thumbnail providers (third-party shell extensions). An STA is initialized on the
//! caller's background thread.

use std::path::{Component, Path, Prefix};

use arke_core::thumbs::Thumbnail;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_ICONONLY,
    SIIGBF_INCACHEONLY, SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

use crate::win_fileops::{Com, wide};

/// `GetDriveTypeW` results (WinBase.h).
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_RAMDISK: u32 = 6;

/// The one definition of "local" for a `GetDriveTypeW` result: internal disks, USB sticks, SD
/// cards and RAM disks. Network and optical drives are not local (thumbnails, folder sizes and
/// cloud status skip them).
pub(crate) const fn is_local_drive_type(drive_type: u32) -> bool {
    matches!(drive_type, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_RAMDISK)
}

/// `GetDriveTypeW` of the drive the path is on; `None` for UNC and relative paths.
/// It doesn't hit the network, but call from a background thread anyway.
fn drive_type_of(path: &Path) -> Option<u32> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return None;
    };
    let root = wide(std::ffi::OsStr::new(&format!("{}:\\", char::from(letter))));
    // SAFETY: root is a NUL-terminated "X:\" wide string.
    Some(unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) })
}

/// Whether the path is on a local disk: internal, USB, SD card or RAM disk (not UNC, network or
/// optical). See [`is_local_drive_type`].
pub(crate) fn is_local_disk(path: &Path) -> bool {
    drive_type_of(path).is_some_and(is_local_drive_type)
}

/// Whether the path is on an internal fixed disk (not removable media). For what should not
/// touch a USB stick on its own, such as the global search index.
pub(crate) fn is_fixed_disk(path: &Path) -> bool {
    drive_type_of(path) == Some(DRIVE_FIXED)
}

/// System icon for a file as straight-alpha RGBA. `None` means use the theme icon.
pub(crate) fn file_icon(path: &Path, size: u32) -> Option<Thumbnail> {
    let _com = Com::init().ok()?;
    let w = wide(path.as_os_str());
    // SAFETY: COM is initialized on this thread; w is NUL-terminated and outlives the call.
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }.ok()?;
    let edge = i32::try_from(size).ok()?;
    // SAFETY: factory is a valid interface; ICONONLY doesn't load thumbnail providers.
    let bitmap = unsafe { factory.GetImage(SIZE { cx: edge, cy: edge }, SIIGBF_ICONONLY) }.ok()?;
    let pixels = bitmap_pixels(bitmap);
    // SAFETY: GetImage hands us ownership of the HBITMAP.
    let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
    pixels
}

/// Windows' thumbnail of a file (for videos: a frame), fitted within `size`. `None` if Windows
/// has none (no thumbnail provider or codec for it). Loads thumbnail providers, which Windows runs
/// in its own surrogate process; Arke calls this only in its thumbnail host (`thumb_host.rs`).
#[cfg_attr(
    test,
    expect(
        dead_code,
        reason = "only the host process calls it; tests use a stand-in host"
    )
)]
pub(crate) fn video_thumbnail(path: &Path, size: u32) -> Option<Thumbnail> {
    let _com = Com::init().ok()?;
    let w = wide(path.as_os_str());
    // SAFETY: COM is initialized on this thread; w is NUL-terminated and outlives the call.
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }.ok()?;
    let edge = i32::try_from(size).ok()?;
    cache_first(|flags| {
        // SAFETY: factory is a valid interface; THUMBNAILONLY returns no icon in place of a
        // picture, and INCACHEONLY never runs a thumbnail provider.
        let bitmap = unsafe { factory.GetImage(SIZE { cx: edge, cy: edge }, flags) }.ok()?;
        let pixels = bitmap_pixels(bitmap);
        // SAFETY: GetImage hands us ownership of the HBITMAP.
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        pixels
    })
}

/// Asks `request` for a thumbnail already in Windows' thumbnail cache first (instant), and only
/// on a miss for one Windows has to generate (seconds for a cold 8K video). The cached picture
/// may be another size than the one asked for; callers fit it.
fn cache_first(mut request: impl FnMut(SIIGBF) -> Option<Thumbnail>) -> Option<Thumbnail> {
    let generate = SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT;
    request(generate | SIIGBF_INCACHEONLY).or_else(|| request(generate))
}

/// Reads an HBITMAP as straight-alpha RGBA.
pub(crate) fn bitmap_pixels(bitmap: HBITMAP) -> Option<Thumbnail> {
    let (width, height, bgra) = bitmap_bgra(bitmap)?;
    Some(Thumbnail {
        width,
        height,
        rgba: bgra_premultiplied_to_rgba(bgra, None),
    })
}

/// Reads an icon's colour bitmap with its AND mask (`ICONINFO::hbmMask`, set bits are
/// transparent) as straight-alpha RGBA. Icons without an alpha channel (older ones, or a 32-bit
/// bitmap whose alpha was never filled in) are cut out by the mask instead of coming out blank.
pub(crate) fn icon_pixels(color: HBITMAP, mask: HBITMAP) -> Option<Thumbnail> {
    let (width, height, bgra) = bitmap_bgra(color)?;
    let mask = bitmap_bgra(mask).filter(|(w, h, _)| (*w, *h) == (width, height));
    Some(Thumbnail {
        width,
        height,
        rgba: bgra_premultiplied_to_rgba(bgra, mask.as_ref().map(|(_, _, m)| m.as_slice())),
    })
}

/// A bitmap's size and its 32-bit BGRA pixels, top-down.
fn bitmap_bgra(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut bm = BITMAP::default();
    // SAFETY: bm is a correctly sized output buffer.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            i32::try_from(size_of::<BITMAP>()).unwrap_or_default(),
            Some((&raw mut bm).cast()),
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
        return None;
    }
    let width = bm.bmWidth;
    let height = bm.bmHeight.abs();
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative: top-down row order.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (width * height * 4) as usize];
    // SAFETY: buf holds exactly width*height 32-bit pixels, the format described by info.
    let lines = unsafe {
        let hdc = GetDC(None);
        let lines = GetDIBits(
            hdc,
            bitmap,
            0,
            height as u32,
            Some(buf.as_mut_ptr().cast()),
            &raw mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        lines
    };
    (lines != 0).then_some((width as u32, height as u32, buf))
}

/// GDI returns premultiplied BGRA. A bitmap whose alpha channel is missing is opaque, or, with
/// an icon's `mask` (BGRA of its AND mask: white is transparent), opaque where the mask says so.
///
/// The alpha channel counts as missing when it is empty, and also when only a few pixels have
/// any while many more have colour at alpha 0: a type's icon built from an old-style resource
/// can come back with a stray pixel or two of alpha, and trusting that drew the whole icon
/// transparent except for a dot.
fn bgra_premultiplied_to_rgba(mut buf: Vec<u8>, mask: Option<&[u8]>) -> Vec<u8> {
    let (mut with_alpha, mut hidden) = (0usize, 0usize);
    for p in buf.as_chunks::<4>().0 {
        if p[3] != 0 {
            with_alpha += 1;
        } else if p[..3].iter().any(|&c| c != 0) {
            hidden += 1;
        }
    }
    let has_alpha = with_alpha > hidden;
    let mask = mask.filter(|m| m.len() == buf.len());
    for (i, p) in buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        p.swap(0, 2);
        if has_alpha {
            if p[3] != 0 && p[3] != 255 {
                let a = u16::from(p[3]);
                for c in &mut p[..3] {
                    *c = (u16::from(*c) * 255 / a).min(255) as u8;
                }
            }
        } else if mask.is_some_and(|m| m[i * 4] != 0) {
            // The mask marks it transparent.
            *p = [0; 4];
        } else {
            p[3] = 255;
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_type_classification() {
        assert!(is_local_drive_type(DRIVE_FIXED));
        assert!(is_local_drive_type(DRIVE_REMOVABLE));
        assert!(is_local_drive_type(DRIVE_RAMDISK));
        // DRIVE_UNKNOWN, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_CDROM
        for other in [0, 1, 4, 5] {
            assert!(!is_local_drive_type(other), "{other}");
        }
    }

    #[test]
    fn local_disk_detection() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(is_local_disk(Path::new(&format!(r"{system}\Windows"))));
        assert!(!is_local_disk(Path::new(r"\\server\share\a.png")));
        assert!(!is_local_disk(Path::new(r"relative\a.png")));
        assert!(is_fixed_disk(Path::new(&format!(r"{system}\Windows"))));
        assert!(!is_fixed_disk(Path::new(r"\\server\share\a.png")));
    }

    #[test]
    fn system_icon_for_an_exe() {
        // Reads only the built-in notepad.exe icon.
        let windir = std::env::var("windir").unwrap_or_else(|_| r"C:\Windows".into());
        let icon =
            file_icon(&Path::new(&windir).join(r"System32\notepad.exe"), 32).expect("notepad icon");
        assert_eq!((icon.width, icon.height), (32, 32));
        assert_eq!(icon.rgba.len(), 32 * 32 * 4);
        assert!(
            icon.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0),
            "icon is not fully transparent"
        );
    }

    fn picture() -> Thumbnail {
        Thumbnail {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        }
    }

    #[test]
    fn a_cached_thumbnail_is_used_without_generating() {
        let mut asked = Vec::new();
        let got = cache_first(|flags| {
            asked.push(flags);
            Some(picture())
        });
        assert!(got.is_some());
        assert_eq!(asked.len(), 1, "one call, from the cache only");
        assert_eq!(asked[0] & SIIGBF_INCACHEONLY, SIIGBF_INCACHEONLY);
    }

    #[test]
    fn a_cache_miss_falls_back_to_generating() {
        let mut asked = Vec::new();
        let got = cache_first(|flags| {
            asked.push(flags);
            (flags.0 & SIIGBF_INCACHEONLY.0 == 0).then(picture)
        });
        assert!(got.is_some());
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0] & SIIGBF_INCACHEONLY, SIIGBF_INCACHEONLY);
        // The second request is today's, unchanged.
        assert_eq!(asked[1], SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT);
    }

    #[test]
    fn no_thumbnail_anywhere_gives_none() {
        let mut calls = 0;
        assert!(
            cache_first(|_| {
                calls += 1;
                None
            })
            .is_none()
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn unpremultiplies_and_swaps_channels() {
        // Premultiplied BGRA (b=50, g=0, r=100, a=128) -> RGBA about (199, 0, 99, 128).
        let out = bgra_premultiplied_to_rgba(vec![50, 0, 100, 128, 0, 0, 0, 0], None);
        assert_eq!(&out[..4], &[199, 0, 99, 128]);
        // A bitmap with no alpha at all is opaque.
        let opaque = bgra_premultiplied_to_rgba(vec![1, 2, 3, 0], None);
        assert_eq!(opaque, [3, 2, 1, 255]);
    }

    /// An old-style icon: colour at alpha 0 everywhere, cut out by its AND mask.
    #[test]
    fn an_icon_without_alpha_is_cut_out_by_its_mask() {
        // Four pixels of green; the mask (white = transparent) hides the last one.
        let green = [0, 200, 0, 0];
        let color = green.repeat(4);
        let mask = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 0];
        let out = bgra_premultiplied_to_rgba(color, Some(&mask));
        assert_eq!(&out[..4], &[0, 200, 0, 255]);
        assert_eq!(&out[8..12], &[0, 200, 0, 255]);
        assert_eq!(&out[12..], &[0, 0, 0, 0]);
    }

    /// One stray pixel of alpha in an icon whose colour sits at alpha 0 used to hide the rest:
    /// the icon came out as a dot.
    #[test]
    fn a_stray_alpha_pixel_does_not_hide_the_icon() {
        let mut color = [10, 120, 10, 0].repeat(64);
        color[3] = 255;
        let out = bgra_premultiplied_to_rgba(color, None);
        let visible = out
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 255)
            .count();
        assert_eq!(visible, 64, "every pixel is drawn");
        // A real alpha channel is still honoured: mostly transparent with a solid shape.
        let mut sparse = [0, 0, 0, 0].repeat(64);
        for p in sparse.as_chunks_mut::<4>().0.iter_mut().take(20) {
            *p = [10, 120, 10, 255];
        }
        let out = bgra_premultiplied_to_rgba(sparse, None);
        let visible = out
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 255)
            .count();
        assert_eq!(visible, 20);
    }
}
