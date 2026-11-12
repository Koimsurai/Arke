//! Drag image shown under the cursor when dragging to other apps (`IDragSourceHelper`).
//!
//! Uses the caller's thumbnail, else the system icon for the file type
//! (`SHGFI_USEFILEATTRIBUTES` looks at the extension only, so no file is opened and no
//! third-party icon handler loads). `.exe`/`.lnk`/`.ico` use their own icon, as in the
//! list ([`crate::win_icons::file_icon`]). Multiple items get a count badge.
//!
//! [`compose`] runs on a background thread; the UI thread only wraps the pixels in a
//! bitmap for Windows ([`attach`]).

use std::path::Path;

use arke_core::thumbs::Thumbnail;
use windows::Win32::Foundation::{COLORREF, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject,
    DrawTextW, FW_SEMIBOLD, FillRect, GetTextExtentPoint32W, HBITMAP, HGDIOBJ, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, DSH_ALLOWDROPDESCRIPTIONTEXT, IDragSourceHelper, IDragSourceHelper2,
    SHDRAGIMAGE, SHFILEINFOW, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
    SHGetImageList, SHIL_EXTRALARGE, SHIL_JUMBO,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};
use windows::core::{Interface, PCWSTR, w};

use crate::shell::DragImage;
use crate::win_fileops::{Com, wide};
use crate::win_icons::{bitmap_pixels, file_icon};

/// Longest side of the image, about what Explorer uses.
const SIDE: u32 = 96;
/// Count badge height, horizontal padding, and how far it overhangs the image.
const BADGE_H: u32 = 22;
const BADGE_PAD: u32 = 7;
const BADGE_OVERHANG: u32 = 6;
/// Side of the SHIL_EXTRALARGE icons.
const EXTRALARGE: u32 = 48;

/// Builds the drag image. Call from a background thread.
pub(crate) fn compose(
    base: Option<Thumbnail>,
    path: &Path,
    is_dir: bool,
    own_icon: bool,
    count: usize,
    badge: [u8; 3],
) -> Option<DragImage> {
    let base = base
        .or_else(|| own_icon.then(|| file_icon(path, SIDE)).flatten())
        .or_else(|| type_icon(path, is_dir))?;
    let base = fit(base)?;
    let mut canvas = Canvas::around(&base, count > 1);
    canvas.draw(&base, 0, 0);
    if count > 1
        && let Some(mark) = badge_pixels(count, badge)
    {
        let x = canvas.width - mark.width;
        let y = canvas.height - mark.height;
        canvas.draw(&mark, x, y);
    }
    Some(canvas.finish())
}

/// Attaches the image to the drag data. Call on the UI thread before the drag starts.
/// On failure the drag still works, just without an image.
pub(crate) fn attach(data: &IDataObject, image: &DragImage) {
    let (Ok(width), Ok(height)) = (i32::try_from(image.width), i32::try_from(image.height)) else {
        return;
    };
    if image.bgra.len() != (image.width * image.height * 4) as usize {
        return;
    }
    let info = bitmap_info(width, height);
    // SAFETY: a 32-bit top-down DIB; bits points to width*height*4 bytes, exactly the size of
    // image.bgra. The helper owns the bitmap on success; we free it on failure.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(
            None,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        ) else {
            return;
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return;
        }
        std::ptr::copy_nonoverlapping(image.bgra.as_ptr(), bits.cast::<u8>(), image.bgra.len());
        let helper: windows::core::Result<IDragSourceHelper> =
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER);
        let Ok(helper) = helper else {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return;
        };
        // Lets drop targets show descriptions like "Copy to Desktop".
        if let Ok(helper2) = helper.cast::<IDragSourceHelper2>() {
            let _ = helper2.SetFlags(DSH_ALLOWDROPDESCRIPTIONTEXT.0.cast_unsigned());
        }
        let drag = SHDRAGIMAGE {
            sizeDragImage: SIZE {
                cx: width,
                cy: height,
            },
            // Cursor at the bottom center, as in Explorer.
            ptOffset: POINT {
                x: width / 2,
                y: height - height / 8,
            },
            hbmpDragImage: bitmap,
            // Use the alpha channel, no color key.
            crColorKey: COLORREF(0xFFFF_FFFF),
        };
        if helper.InitializeFromBitmap(&raw const drag, data).is_err() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
        }
    }
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative: top-down.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// System icon for this file type or folder, from name and attributes only.
fn type_icon(path: &Path, is_dir: bool) -> Option<Thumbnail> {
    let _com = Com::init().ok()?;
    // Pass only the extension; the real location and existence don't matter.
    let name = if is_dir {
        std::ffi::OsString::from("folder")
    } else {
        path.extension().map_or_else(
            || std::ffi::OsString::from("file"),
            |ext| {
                let mut s = std::ffi::OsString::from("file.");
                s.push(ext);
                s
            },
        )
    };
    let name = wide(&name);
    let attributes = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let mut info = SHFILEINFOW::default();
    // SAFETY: name is NUL-terminated; info is a correctly sized out struct. With
    // USEFILEATTRIBUTES the file is not accessed.
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(name.as_ptr()),
            attributes,
            Some(&raw mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if ok == 0 {
        return None;
    }
    // In the jumbo (256) list, types without a large icon get a 48x48 image in the top-left
    // corner with the rest transparent; use the 48 px list for those.
    list_icon(SHIL_JUMBO, info.iIcon)
        .filter(|icon| !only_top_left(icon, EXTRALARGE))
        .or_else(|| list_icon(SHIL_EXTRALARGE, info.iIcon))
}

fn list_icon(list: u32, index: i32) -> Option<Thumbnail> {
    // SAFETY: the system image list is a process-wide COM object. We free the HICON from
    // GetIcon and both bitmaps from GetIconInfo.
    unsafe {
        let images: IImageList = SHGetImageList(i32::try_from(list).ok()?).ok()?;
        let icon = images.GetIcon(index, ILD_TRANSPARENT.0).ok()?;
        let mut parts = ICONINFO::default();
        let got = GetIconInfo(icon, &raw mut parts);
        let pixels = got.is_ok().then(|| bitmap_pixels(parts.hbmColor)).flatten();
        if got.is_ok() {
            let _ = DeleteObject(HGDIOBJ(parts.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(parts.hbmMask.0));
        }
        let _ = DestroyIcon(icon);
        pixels
    }
}

/// Whether only the top-left `side`x`side` area has visible pixels.
fn only_top_left(image: &Thumbnail, side: u32) -> bool {
    if image.width <= side || image.height <= side {
        return false;
    }
    let width = image.width as usize;
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .all(|(i, p)| p[3] == 0 || ((i % width) < side as usize && (i / width) < side as usize))
}

/// Shrinks to fit [`SIDE`]; smaller images are not enlarged.
fn fit(image: Thumbnail) -> Option<Thumbnail> {
    let longest = image.width.max(image.height);
    if longest == 0 {
        return None;
    }
    if longest <= SIDE {
        return Some(image);
    }
    let width = (image.width * SIDE / longest).max(1);
    let height = (image.height * SIDE / longest).max(1);
    let source = image::RgbaImage::from_raw(image.width, image.height, image.rgba)?;
    let scaled = image::imageops::resize(
        &source,
        width,
        height,
        image::imageops::FilterType::Triangle,
    );
    Some(Thumbnail {
        width,
        height,
        rgba: scaled.into_raw(),
    })
}

/// Straight-alpha RGBA canvas, converted to premultiplied BGRA for Windows at the end.
struct Canvas {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn around(base: &Thumbnail, with_badge: bool) -> Self {
        let extra = if with_badge { BADGE_OVERHANG } else { 0 };
        // Leave room for the badge even on tiny images.
        let min = if with_badge { BADGE_H * 2 } else { 1 };
        let width = (base.width + extra).max(min);
        let height = (base.height + extra).max(min);
        Self {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
        }
    }

    /// Composites `image` over the canvas (source-over).
    fn draw(&mut self, image: &Thumbnail, left: u32, top: u32) {
        for y in 0..image.height.min(self.height.saturating_sub(top)) {
            for x in 0..image.width.min(self.width.saturating_sub(left)) {
                let s = ((y * image.width + x) * 4) as usize;
                let d = (((top + y) * self.width + left + x) * 4) as usize;
                let (Some(src), Some(dst)) =
                    (image.rgba.get(s..s + 4), self.rgba.get_mut(d..d + 4))
                else {
                    continue;
                };
                let sa = u32::from(src[3]);
                let da = u32::from(dst[3]);
                let out_a = sa + da * (255 - sa) / 255;
                if out_a == 0 {
                    continue;
                }
                for c in 0..3 {
                    let v = (u32::from(src[c]) * sa + u32::from(dst[c]) * da * (255 - sa) / 255)
                        / out_a;
                    dst[c] = v.min(255) as u8;
                }
                dst[3] = out_a.min(255) as u8;
            }
        }
    }

    fn finish(self) -> DragImage {
        let mut bgra = self.rgba;
        for p in bgra.as_chunks_mut::<4>().0 {
            let a = u16::from(p[3]);
            let r = (u16::from(p[0]) * a / 255) as u8;
            let g = (u16::from(p[1]) * a / 255) as u8;
            let b = (u16::from(p[2]) * a / 255) as u8;
            p[0] = b;
            p[1] = g;
            p[2] = r;
        }
        DragImage {
            width: self.width,
            height: self.height,
            bgra,
        }
    }
}

/// Count badge: white text on a `color` pill. GDI draws the text on an opaque fill and
/// the rounded ends are computed here, since GDI doesn't write alpha.
fn badge_pixels(count: usize, color: [u8; 3]) -> Option<Thumbnail> {
    let text: Vec<u16> = if count > 9999 {
        "9999+".encode_utf16().collect()
    } else {
        count.to_string().encode_utf16().collect()
    };
    let fill =
        COLORREF(u32::from(color[0]) | (u32::from(color[1]) << 8) | (u32::from(color[2]) << 16));
    // SAFETY: the memory DC, DIB, font and brush are created and freed here; bits points to
    // width*height*4 bytes and stays valid until the bitmap is deleted.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = CreateFontW(
            -13,
            0,
            0,
            0,
            FW_SEMIBOLD.0.cast_signed(),
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        let mut extent = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &text, &raw mut extent);
        let width = (u32::try_from(extent.cx).unwrap_or(0) + BADGE_PAD * 2).max(BADGE_H);
        let height = BADGE_H;
        let info = bitmap_info(i32::try_from(width).ok()?, i32::try_from(height).ok()?);
        let mut bits = std::ptr::null_mut();
        let bitmap: Option<HBITMAP> = CreateDIBSection(
            Some(dc),
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
        .ok();
        let mut result = None;
        if let Some(bitmap) = bitmap
            && !bits.is_null()
        {
            let old_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));
            let brush = CreateSolidBrush(fill);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width.cast_signed(),
                bottom: height.cast_signed(),
            };
            FillRect(dc, &raw const rect, brush);
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            let mut text = text;
            DrawTextW(
                dc,
                &mut text,
                &raw mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            let _ = windows::Win32::Graphics::Gdi::GdiFlush();
            let len = (width * height * 4) as usize;
            let bgr = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
            SelectObject(dc, old_bitmap);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            result = Some(pill(width, height, &bgr));
        }
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(dc);
        result
    }
}

/// Cuts opaque BGR pixels into an antialiased pill shape.
fn pill(width: u32, height: u32, bgr: &[u8]) -> Thumbnail {
    let radius = height as f32 / 2.;
    let mut rgba = vec![0u8; bgr.len()];
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            // Distance to the segment between the two circle centers.
            let cx = px.clamp(radius, width as f32 - radius);
            let distance = ((px - cx).powi(2) + (py - radius).powi(2)).sqrt();
            let coverage = (radius - distance + 0.5).clamp(0., 1.);
            rgba[i] = bgr[i + 2];
            rgba[i + 1] = bgr[i + 1];
            rgba[i + 2] = bgr[i];
            rgba[i + 3] = (coverage * 255.) as u8;
        }
    }
    Thumbnail {
        width,
        height,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Thumbnail {
        Thumbnail {
            width,
            height,
            rgba: rgba.repeat((width * height) as usize),
        }
    }

    #[test]
    fn big_images_shrink_and_small_ones_stay() {
        let big = fit(solid(400, 200, [10, 20, 30, 255])).unwrap();
        assert_eq!((big.width, big.height), (96, 48));
        let small = fit(solid(32, 32, [10, 20, 30, 255])).unwrap();
        assert_eq!((small.width, small.height), (32, 32));
    }

    #[test]
    fn output_is_premultiplied_bgra() {
        let image = compose(
            Some(solid(4, 4, [200, 100, 50, 128])),
            Path::new(r"C:\x\a.png"),
            false,
            false,
            1,
            [0, 0, 0],
        )
        .unwrap();
        assert_eq!((image.width, image.height), (4, 4));
        // 200*128/255=100, 100*128/255=50, 50*128/255=25, in B G R A order.
        assert_eq!(image.bgra[..4], [25, 50, 100, 128]);
    }

    #[test]
    fn several_items_get_a_count_badge() {
        let one = compose(
            Some(solid(96, 96, [255, 255, 255, 255])),
            Path::new(r"C:\x\a.png"),
            false,
            false,
            1,
            [200, 60, 30],
        )
        .unwrap();
        assert_eq!((one.width, one.height), (96, 96));
        let many = compose(
            Some(solid(96, 96, [255, 255, 255, 255])),
            Path::new(r"C:\x\a.png"),
            false,
            false,
            12,
            [200, 60, 30],
        )
        .unwrap();
        assert_eq!(
            (many.width, many.height),
            (102, 102),
            "the badge sticks out a little"
        );
        // Next to the text, the badge shows its fill color (BGRA).
        let at = |x: u32, y: u32| {
            let i = ((y * many.width + x) * 4) as usize;
            [
                many.bgra[i],
                many.bgra[i + 1],
                many.bgra[i + 2],
                many.bgra[i + 3],
            ]
        };
        assert_eq!(at(many.width - 4, many.height - 11), [30, 60, 200, 255]);
        // Rounded ends: the bottom-right corner is transparent.
        assert_eq!(at(many.width - 1, many.height - 1)[3], 0);
        // White text inside.
        let badge_left = many.width - 26;
        let white = (badge_left..many.width)
            .flat_map(|x| (many.height - 22..many.height).map(move |y| (x, y)))
            .filter(|&(x, y)| at(x, y) == [255, 255, 255, 255])
            .count();
        assert!(white > 10, "count not drawn ({white} white pixels)");
    }

    #[test]
    fn type_icons_come_from_the_extension_alone() {
        // Works for files that don't exist; only the extension matters.
        let icon = compose(
            None,
            Path::new(r"Z:\nowhere\x.txt"),
            false,
            false,
            1,
            [0, 0, 0],
        )
        .unwrap();
        assert!(icon.width >= 32 && icon.width <= 96, "{}", icon.width);
        assert!(icon.bgra.as_chunks::<4>().0.iter().any(|p| p[3] != 0));
        let folder = compose(
            None,
            Path::new(r"Z:\nowhere\dir"),
            true,
            false,
            1,
            [0, 0, 0],
        )
        .unwrap();
        assert!(folder.bgra.as_chunks::<4>().0.iter().any(|p| p[3] != 0));
    }
}
