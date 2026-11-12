//! Image thumbnails, decoded with the `image` crate on a background thread. Windows
//! thumbnail providers are not used since they are third-party shell extensions.
//! Local disks only; the caller decides.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

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
pub fn decode_photo(bytes: &[u8], max_side: u32) -> Result<Photo, DecodeError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|source| DecodeError::Sniff { source })?;
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
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|source| DecodeError::Pixels { source })?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    let (width, height) = (image.width(), image.height());
    // thumbnail() uses a faster downscaler than resize(), better for many thumbnails.
    let small = if width > max_side || height > max_side {
        image.thumbnail(max_side, max_side)
    } else {
        image
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

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
}
