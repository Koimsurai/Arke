//! Image thumbnails, decoded with the `image` crate on a background thread. Windows
//! thumbnail providers are not used since they are third-party shell extensions.
//! Local disks only; the caller decides.

use std::io::Cursor;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

use crate::exif::PhotoInfo;

/// Longest thumbnail side in pixels when nothing asks for a particular size (drag images). The
/// grid asks for [`source_size`] of its current icon size instead.
pub const THUMBNAIL_SIZE: u32 = 192;

/// Sizes thumbnails are decoded at. A few fixed steps rather than the exact pixel size, so
/// switching icon sizes or moving between monitors reuses what is cached, and every step is at
/// least as large as what it is drawn at.
pub const SOURCE_SIZES: [u32; 6] = [96, 128, 192, 256, 384, 512];

/// Decode size for a thumbnail drawn `logical` pixels wide on a display scaled by `scale`: the
/// smallest of [`SOURCE_SIZES`] that covers the physical size, so it stays sharp on high DPI.
pub fn source_size(logical: f32, scale: f32) -> u32 {
    let physical = (logical * scale.max(1.)).ceil();
    SOURCE_SIZES
        .into_iter()
        .find(|&s| f64::from(s) >= f64::from(physical))
        .unwrap_or(SOURCE_SIZES[SOURCE_SIZES.len() - 1])
}

/// Decode allocation cap, to reject tiny files that expand into huge images (malicious or corrupt).
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// Why an image could not be decoded. Display is the `image` crate's message.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Reading the header to detect the format failed.
    #[error("{source}")]
    Sniff {
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Unknown format, or the decoder could not be set up (bad header, size over the limit).
    #[error("{source}")]
    Format {
        /// Underlying error.
        #[source]
        source: image::ImageError,
    },
    /// The pixel data could not be decoded (truncated or corrupt).
    #[error("{source}")]
    Pixels {
        /// Underlying error.
        #[source]
        source: image::ImageError,
    },
}

/// Decoded, downscaled image ready for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8, straight alpha, rows top to bottom.
    pub rgba: Vec<u8>,
}

/// Photo for the preview: downscaled image, original size (after EXIF rotation), capture info.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Photo {
    /// Downscaled, upright image for display.
    pub image: Thumbnail,
    /// Original width in pixels, after EXIF rotation.
    pub width: u32,
    /// Original height in pixels, after EXIF rotation.
    pub height: u32,
    /// Capture info from EXIF; `None` if there is none.
    pub info: Option<PhotoInfo>,
}

/// Thumbnail from file content (format sniffed from content, not extension). Phone photos are
/// rotated upright per EXIF.
pub fn decode_thumbnail(bytes: &[u8], max_side: u32) -> Result<Thumbnail, DecodeError> {
    decode_photo(bytes, max_side).map(|p| p.image)
}

/// Like [`decode_thumbnail`], plus original size and capture info for the preview pane.
///
/// Memory stays low however big the image: a JPEG is decoded straight at a reduced size where
/// that still covers `max_side`, and the image is shrunk before it is turned upright, so no
/// second full-size copy is made. What must be decoded at full size and is big takes turns with
/// other such decodes (thumbnail threads run side by side).
pub fn decode_photo(bytes: &[u8], max_side: u32) -> Result<Photo, DecodeError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|source| DecodeError::Sniff { source })?;
    let format = reader.format();
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|source| DecodeError::Format { source })?;
    let orientation = decoder.orientation().ok();
    let info = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .and_then(|raw| crate::exif::parse(&raw));
    let (raw_width, raw_height) = decoder.dimensions();
    let (width, height) = if orientation.is_some_and(turns_sideways) {
        (raw_height, raw_width)
    } else {
        (raw_width, raw_height)
    };
    let scaled = (format == Some(ImageFormat::Jpeg)
        && (raw_width > max_side || raw_height > max_side))
        .then(|| jpeg_scaled(bytes, raw_width, raw_height, max_side))
        .flatten();
    let mut small = if let Some(image) = scaled {
        fit(image, max_side)
    } else {
        let big = decoder.total_bytes() > BIG_DECODE;
        let _turn = big.then(|| BIG_DECODES.lock());
        #[cfg(test)]
        let _probe = big.then(probe::enter);
        let image =
            DynamicImage::from_decoder(decoder).map_err(|source| DecodeError::Pixels { source })?;
        // Shrunk before the turn ends, so only one full-size image is held at a time.
        fit(image, max_side)
    };
    if let Some(orientation) = orientation {
        small.apply_orientation(orientation);
    }
    let rgba = small.to_rgba8();
    Ok(Photo {
        image: Thumbnail {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        },
        width,
        height,
        info,
    })
}

/// `image` shrunk to fit in `max_side` × `max_side`; never enlarged.
fn fit(image: DynamicImage, max_side: u32) -> DynamicImage {
    // thumbnail() uses a faster downscaler than resize(), better for many thumbnails.
    if image.width() > max_side || image.height() > max_side {
        image.thumbnail(max_side, max_side)
    } else {
        image
    }
}

/// An orientation that swaps width and height.
const fn turns_sideways(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// A `width`×`height` JPEG decoded at 1/2, 1/4 or 1/8 of its size (scaled in the DCT, so the
/// full-size pixels never exist), the smallest of those that still covers the thumbnail fitting
/// `max_side`. `None` when no reduction covers it, or that decoder can't (CMYK, 12-bit, or a
/// file it rejects); the full-size decoder does it then.
fn jpeg_scaled(bytes: &[u8], width: u32, height: u32, max_side: u32) -> Option<DynamicImage> {
    let fit = f64::from(max_side) / f64::from(width.max(height));
    let want = |side: u32| ((f64::from(side) * fit).ceil() as u16).max(1);
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    decoder.set_max_decoding_buffer_size(MAX_DECODE_ALLOC as usize);
    decoder.read_info().ok()?;
    let (w, h) = decoder.scale(want(width), want(height)).ok()?;
    // Not reduced at all: the usual decoder is faster for that.
    if u32::from(w) >= width {
        return None;
    }
    let pixels = decoder.decode().ok()?;
    let (w, h) = (u32::from(w), u32::from(h));
    match decoder.info()?.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            image::RgbImage::from_raw(w, h, pixels).map(DynamicImage::ImageRgb8)
        }
        jpeg_decoder::PixelFormat::L8 => {
            image::GrayImage::from_raw(w, h, pixels).map(DynamicImage::ImageLuma8)
        }
        _ => None,
    }
}

/// Full-size decodes at least this large (decoded bytes, about 8 megapixels of RGB) take turns.
const BIG_DECODE: u64 = 24 * 1024 * 1024;

/// Held by a big full-size decode, until its image is shrunk.
static BIG_DECODES: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Counts full-size decodes of big images running at once (tests check they take turns).
#[cfg(test)]
mod probe {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NOW: AtomicUsize = AtomicUsize::new(0);
    pub(super) static MOST: AtomicUsize = AtomicUsize::new(0);

    pub(super) struct Running;

    pub(super) fn enter() -> Running {
        let now = NOW.fetch_add(1, Ordering::SeqCst) + 1;
        MOST.fetch_max(now, Ordering::SeqCst);
        Running
    }

    impl Drop for Running {
        fn drop(&mut self) {
            NOW.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    /// Tests that decode big images take turns, so the allocation peak measured is one test's.
    static BIG: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// A `w`×`h` JPEG of a gradient, with an EXIF orientation when given.
    fn jpeg_bytes(w: u32, h: u32, orientation: Option<u16>) -> Vec<u8> {
        use image::codecs::jpeg::JpegEncoder;
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 80)
            .encode_image(&img)
            .unwrap();
        let Some(orientation) = orientation else {
            return jpeg;
        };
        // APP1 with a little-endian TIFF holding only IFD0's orientation.
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
        out.extend(app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// A portrait phone photo (landscape pixels, EXIF says turn it): the thumbnail comes out
    /// upright, and decoding it never holds anything near the full-size image.
    #[test]
    fn a_large_portrait_photo_is_shrunk_before_it_is_turned() {
        let _turn = BIG.lock();
        let bytes = jpeg_bytes(4000, 3000, Some(6));
        let full = 4000 * 3000 * 3;
        let (photo, peak) = crate::alloc_peak::during(|| decode_photo(&bytes, 512).unwrap());
        assert_eq!((photo.width, photo.height), (3000, 4000), "upright size");
        assert_eq!((photo.image.width, photo.image.height), (384, 512));
        assert!(
            peak < full / 4,
            "peak {} MB while decoding a {} MB image",
            peak >> 20,
            full >> 20
        );
    }

    /// A big JPEG is decoded at a quarter of its size here, the smallest reduction that still
    /// covers a 512 px thumbnail.
    #[test]
    fn large_jpegs_decode_at_a_reduced_size() {
        let _turn = BIG.lock();
        let bytes = jpeg_bytes(4000, 3000, Some(6));
        let image = jpeg_scaled(&bytes, 4000, 3000, 512).expect("a reduced decode");
        assert_eq!((image.width(), image.height()), (1000, 750));
    }

    /// The pixels turn too: the top-left of the upright thumbnail is the original's bottom-left.
    #[test]
    fn a_shrunk_photo_is_turned_the_right_way() {
        let mut img = image::RgbImage::from_pixel(1200, 800, image::Rgb([0, 0, 255]));
        for y in 600..800 {
            for x in 0..300 {
                img.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&img)
            .unwrap();
        let mut tagged = jpeg_bytes(8, 8, Some(6));
        // Same EXIF header, this image's data.
        let header = 4 + usize::from(u16::from_be_bytes([tagged[4], tagged[5]]));
        tagged.truncate(header);
        tagged.extend_from_slice(&jpeg[2..]);
        let t = decode_thumbnail(&tagged, 120).unwrap();
        assert_eq!((t.width, t.height), (80, 120));
        let px = &t.rgba[..4];
        assert!(px[0] > 200 && px[2] < 60, "top-left is red: {px:?}");
    }

    /// Big images that must be decoded at full size take turns instead of all being held at once.
    #[test]
    fn large_images_decode_one_at_a_time() {
        let _turn = BIG.lock();
        let bytes = png_bytes(3000, 2500);
        let start = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {
                    start.wait();
                    decode_thumbnail(&bytes, 256).unwrap();
                });
            }
        });
        assert_eq!(probe::MOST.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    pub(crate) fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, _| Rgba([(x % 256) as u8, 10, 200, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn shrinks_keeping_aspect_ratio() {
        let t = decode_thumbnail(&png_bytes(800, 400), THUMBNAIL_SIZE).unwrap();
        assert_eq!((t.width, t.height), (192, 96));
        assert_eq!(t.rgba.len(), 192 * 96 * 4);
    }

    #[test]
    fn photo_keeps_the_original_size() {
        let p = decode_photo(&png_bytes(800, 400), THUMBNAIL_SIZE).unwrap();
        assert_eq!((p.width, p.height), (800, 400));
        assert_eq!((p.image.width, p.image.height), (192, 96));
        assert_eq!(p.info, None);
    }

    /// Portrait phone photo: pixels are landscape and EXIF says rotate 90 degrees.
    #[test]
    fn jpeg_is_turned_upright_and_exif_is_read() {
        use image::codecs::jpeg::JpegEncoder;
        let img = RgbaImage::from_pixel(40, 20, Rgba([200, 30, 30, 255]));
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode_image(&DynamicImage::ImageRgba8(img).to_rgb8())
            .unwrap();
        // Insert APP1 (Exif) after SOI: orientation 6 = rotate 90 clockwise, plus camera info.
        let mut exif = crate::exif::tests::sample();
        let tiff = exif.split_off(6);
        let mut tiff = with_orientation(&tiff, 6);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.append(&mut tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
        out.extend(app1);
        out.extend_from_slice(&jpeg[2..]);
        let p = decode_photo(&out, 1000).unwrap();
        assert_eq!((p.width, p.height), (20, 40), "portrait after rotation");
        assert_eq!(
            p.info.and_then(|i| i.camera).as_deref(),
            Some("Canon EOS R6")
        );
    }

    /// Adds an orientation field to IFD0 of the sample EXIF (big-endian).
    fn with_orientation(tiff: &[u8], orientation: u16) -> Vec<u8> {
        // The sample IFD0 is at offset 8 with 3 entries; making it 4 inserts 12 bytes, shifting
        // every later offset by 12.
        let mut out = tiff[..8].to_vec();
        out.extend_from_slice(&4u16.to_be_bytes());
        let shift = 12u32;
        for i in 0..3 {
            let entry = &tiff[10 + i * 12..22 + i * 12];
            let mut e = entry.to_vec();
            let kind = u16::from_be_bytes([e[2], e[3]]);
            let count = u32::from_be_bytes([e[4], e[5], e[6], e[7]]);
            let inline = kind == 2 && count <= 4;
            if !inline {
                let v = u32::from_be_bytes([e[8], e[9], e[10], e[11]]) + shift;
                e[8..12].copy_from_slice(&v.to_be_bytes());
            }
            out.extend(e);
        }
        out.extend_from_slice(&0x0112u16.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&orientation.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        // IFD0's next-IFD pointer and the data after it; offsets inside the Exif IFD shift too.
        out.extend_from_slice(&tiff[46..50]);
        let exif_at = 50usize;
        let count = u16::from_be_bytes([tiff[exif_at], tiff[exif_at + 1]]);
        out.extend_from_slice(&tiff[exif_at..exif_at + 2]);
        for i in 0..usize::from(count) {
            let start = exif_at + 2 + i * 12;
            let mut e = tiff[start..start + 12].to_vec();
            let kind = u16::from_be_bytes([e[2], e[3]]);
            let count = u32::from_be_bytes([e[4], e[5], e[6], e[7]]);
            let size = match kind {
                2 => count,
                3 => count * 2,
                _ => count * 8,
            };
            if size > 4 {
                let v = u32::from_be_bytes([e[8], e[9], e[10], e[11]]) + shift;
                e[8..12].copy_from_slice(&v.to_be_bytes());
            }
            out.extend(e);
        }
        out.extend_from_slice(&tiff[exif_at + 2 + usize::from(count) * 12..]);
        out
    }

    #[test]
    fn source_size_covers_the_physical_size() {
        assert_eq!(source_size(48., 1.), 96, "never below the smallest step");
        assert_eq!(source_size(88., 1.), 96);
        assert_eq!(source_size(88., 1.5), 192);
        assert_eq!(source_size(88., 2.), 192);
        assert_eq!(source_size(192., 1.), 192);
        assert_eq!(source_size(192., 1.25), 256);
        assert_eq!(source_size(192., 2.), 384);
        assert_eq!(source_size(256., 3.), 512, "capped at the largest step");
        // A scale below 1 still decodes at least the logical size.
        assert_eq!(source_size(120., 0.5), 128);
    }

    #[test]
    fn small_images_are_not_enlarged() {
        let t = decode_thumbnail(&png_bytes(16, 8), THUMBNAIL_SIZE).unwrap();
        assert_eq!((t.width, t.height), (16, 8));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(matches!(
            decode_thumbnail(b"not an image", THUMBNAIL_SIZE),
            Err(DecodeError::Format { .. })
        ));
        // Garbage after the PNG header.
        let mut bytes = png_bytes(4, 4);
        bytes.truncate(40);
        assert!(decode_thumbnail(&bytes, THUMBNAIL_SIZE).is_err());
    }

    /// Peak memory while three threads (as many as the thumbnail loader runs) make 256 px grid
    /// thumbnails of the JPEGs in `ARKE_PHOTOS` (default `%TEMP%\arke-test-d1-photos`). Run with
    /// `cargo nextest run -p arke-core --run-ignored only --no-capture -E 'test(thumbnail_peak_memory)'`.
    #[cfg(windows)]
    #[test]
    #[ignore = "benchmark: decodes a folder of photos of your own"]
    #[allow(
        clippy::print_stderr,
        reason = "a benchmark run by hand prints its numbers"
    )]
    fn thumbnail_peak_memory() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = std::env::var_os("ARKE_PHOTOS").map_or_else(
            || std::env::temp_dir().join("arke-test-d1-photos"),
            std::path::PathBuf::from,
        );
        let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg")))
            .collect();
        assert!(!files.is_empty(), "no photos in {}", dir.display());
        let (_, private_before) = process_memory();
        let next = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {
                    while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let bytes = std::fs::read(path).unwrap();
                        decode_thumbnail(&bytes, 256).unwrap();
                    }
                });
            }
        });
        let took = started.elapsed();
        let (peak_private, _) = process_memory();
        eprintln!(
            "{} photos, 3 threads, 256 px: {took:?}; peak private {} MB (private {} MB before)",
            files.len(),
            peak_private >> 20,
            private_before >> 20
        );
    }

    /// Peak private bytes and private bytes of this process.
    #[cfg(windows)]
    fn process_memory() -> (u64, u64) {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        let cb = u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap();
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb,
            ..Default::default()
        };
        // SAFETY: a pseudo handle that needs no closing; `counters` has the size given.
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, cb) }.unwrap();
        (
            counters.PeakPagefileUsage as u64,
            counters.PagefileUsage as u64,
        )
    }
}
