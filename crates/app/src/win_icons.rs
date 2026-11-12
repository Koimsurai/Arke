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
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
};
use windows::core::PCWSTR;

use crate::win_fileops::{Com, wide};

const DRIVE_FIXED: u32 = 3;

/// Whether the path is on a local fixed disk (not UNC, network, removable or optical).
/// `GetDriveTypeW` doesn't hit the network, but call from a background thread anyway.
pub(crate) fn is_local_disk(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return false;
    };
    let root = wide(std::ffi::OsStr::new(&format!("{}:\\", char::from(letter))));
    // SAFETY: root is a NUL-terminated "X:\" wide string.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED }
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

/// Reads an HBITMAP as straight-alpha RGBA.
pub(crate) fn bitmap_pixels(bitmap: HBITMAP) -> Option<Thumbnail> {
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
    if lines == 0 {
        return None;
    }
    Some(Thumbnail {
        width: width as u32,
        height: height as u32,
        rgba: bgra_premultiplied_to_rgba(buf),
    })
}

/// GDI returns premultiplied BGRA. Bitmaps without an alpha channel have alpha 0
/// everywhere and are treated as opaque.
fn bgra_premultiplied_to_rgba(mut buf: Vec<u8>) -> Vec<u8> {
    let has_alpha = buf.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    for p in buf.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
        if !has_alpha {
            p[3] = 255;
        } else if p[3] != 0 && p[3] != 255 {
            let a = u16::from(p[3]);
            for c in &mut p[..3] {
                *c = (u16::from(*c) * 255 / a).min(255) as u8;
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_disk_detection() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(is_local_disk(Path::new(&format!(r"{system}\Windows"))));
        assert!(!is_local_disk(Path::new(r"\\server\share\a.png")));
        assert!(!is_local_disk(Path::new(r"relative\a.png")));
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

    #[test]
    fn unpremultiplies_and_swaps_channels() {
        // Premultiplied BGRA (b=50, g=0, r=100, a=128) -> RGBA about (199, 0, 99, 128).
        let out = bgra_premultiplied_to_rgba(vec![50, 0, 100, 128, 0, 0, 0, 0]);
        assert_eq!(&out[..4], &[199, 0, 99, 128]);
        // A bitmap with no alpha at all is opaque.
        let opaque = bgra_premultiplied_to_rgba(vec![1, 2, 3, 0]);
        assert_eq!(opaque, [3, 2, 1, 255]);
    }
}
