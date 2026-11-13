//! Photo EXIF: date taken, camera, lens, exposure. Reads only the fields the preview
//! shows; not a full EXIF parser.
//!
//! Input is raw TIFF-structured data (from `image`'s `exif_metadata()`; a JPEG
//! `Exif\0\0` prefix is stripped). GPS is skipped on purpose so the preview never shows
//! where a photo was taken. Corrupt fields are skipped, never a panic.

/// Formatted capture info for the preview.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhotoInfo {
    /// "2026-05-01 13:22"
    pub taken: Option<String>,
    /// "Apple iPhone 15 Pro"
    pub camera: Option<String>,
    /// Lens model, e.g. "iPhone 15 Pro back camera 6.86mm f/1.78".
    pub lens: Option<String>,
    /// "1/125 s · f/1.8 · ISO 100 · 26 mm"
    pub exposure: Option<String>,
}

impl PhotoInfo {
    /// No capture info at all (the preview hides the section).
    pub const fn is_empty(&self) -> bool {
        self.taken.is_none()
            && self.camera.is_none()
            && self.lens.is_none()
            && self.exposure.is_none()
    }
}

const MAKE: u16 = 0x010F;
const MODEL: u16 = 0x0110;
const EXIF_IFD: u16 = 0x8769;
const EXPOSURE_TIME: u16 = 0x829A;
const F_NUMBER: u16 = 0x829D;
const ISO: u16 = 0x8827;
const DATE_TAKEN: u16 = 0x9003;
const FOCAL_LENGTH: u16 = 0x920A;
const FOCAL_35MM: u16 = 0xA405;
const LENS_MODEL: u16 = 0xA434;

struct Tiff<'a> {
    data: &'a [u8],
    little: bool,
}

#[derive(Clone, Copy)]
struct Field {
    kind: u16,
    count: u32,
    /// The value itself (if it fits in 4 bytes) or its offset.
    value_at: usize,
}

impl Tiff<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at + 2)?.try_into().ok()?;
        Some(if self.little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at + 4)?.try_into().ok()?;
        Some(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    /// Fields of one IFD as (tag, field).
    fn ifd(&self, at: usize) -> Vec<(u16, Field)> {
        let Some(count) = self.u16(at) else {
            return Vec::new();
        };
        (0..usize::from(count))
            .filter_map(|i| {
                let entry = at + 2 + i * 12;
                let tag = self.u16(entry)?;
                let kind = self.u16(entry + 2)?;
                let count = self.u32(entry + 4)?;
                let unit = match kind {
                    1 | 2 | 7 => 1,
                    3 => 2,
                    4 | 9 => 4,
                    5 | 10 => 8,
                    _ => return None,
                };
                let size = usize::try_from(count).ok()?.checked_mul(unit)?;
                let value_at = if size <= 4 {
                    entry + 8
                } else {
                    usize::try_from(self.u32(entry + 8)?).ok()?
                };
                Some((
                    tag,
                    Field {
                        kind,
                        count,
                        value_at,
                    },
                ))
            })
            .collect()
    }

    fn ascii(&self, f: Field) -> Option<String> {
        if f.kind != 2 {
            return None;
        }
        let bytes = self
            .data
            .get(f.value_at..f.value_at + usize::try_from(f.count).ok()?)?;
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn rational(&self, f: Field) -> Option<(u32, u32)> {
        if f.kind != 5 && f.kind != 10 {
            return None;
        }
        let (n, d) = (self.u32(f.value_at)?, self.u32(f.value_at + 4)?);
        (d != 0).then_some((n, d))
    }

    fn short(&self, f: Field) -> Option<u32> {
        match f.kind {
            3 => self.u16(f.value_at).map(u32::from),
            4 => self.u32(f.value_at),
            _ => None,
        }
    }
}

fn find(fields: &[(u16, Field)], tag: u16) -> Option<Field> {
    fields.iter().find(|(t, _)| *t == tag).map(|(_, f)| *f)
}

/// Parses EXIF; `None` if there is nothing to show.
pub fn parse(raw: &[u8]) -> Option<PhotoInfo> {
    let data = raw.strip_prefix(b"Exif\0\0").unwrap_or(raw);
    let little = match data.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let tiff = Tiff { data, little };
    let ifd0 = tiff.ifd(usize::try_from(tiff.u32(4)?).ok()?);
    let exif = find(&ifd0, EXIF_IFD)
        .and_then(|f| tiff.u32(f.value_at))
        .and_then(|at| usize::try_from(at).ok())
        .map_or_default(|at| tiff.ifd(at));

    let make = find(&ifd0, MAKE).and_then(|f| tiff.ascii(f));
    let model = find(&ifd0, MODEL).and_then(|f| tiff.ascii(f));
    let camera = match (make, model) {
        // The model often includes the make already ("Canon EOS R6"); don't repeat it.
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model)
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    };
    let taken = find(&exif, DATE_TAKEN)
        .and_then(|f| tiff.ascii(f))
        .map(|d| format_date(&d));
    let lens = find(&exif, LENS_MODEL).and_then(|f| tiff.ascii(f));

    let mut parts: Vec<String> = Vec::new();
    if let Some((n, d)) = find(&exif, EXPOSURE_TIME).and_then(|f| tiff.rational(f)) {
        parts.push(format_exposure(n, d));
    }
    if let Some((n, d)) = find(&exif, F_NUMBER).and_then(|f| tiff.rational(f)) {
        parts.push(format!("f/{}", trim_float(f64::from(n) / f64::from(d))));
    }
    if let Some(iso) = find(&exif, ISO).and_then(|f| tiff.short(f)) {
        parts.push(format!("ISO {iso}"));
    }
    // The 35mm-equivalent focal length reads better (a phone's 6.8 mm is really 24 mm).
    if let Some(mm) = find(&exif, FOCAL_35MM)
        .and_then(|f| tiff.short(f))
        .filter(|&mm| mm > 0)
    {
        parts.push(format!("{mm} mm"));
    } else if let Some((n, d)) = find(&exif, FOCAL_LENGTH).and_then(|f| tiff.rational(f)) {
        parts.push(format!("{} mm", trim_float(f64::from(n) / f64::from(d))));
    }
    let info = PhotoInfo {
        taken,
        camera,
        lens,
        exposure: (!parts.is_empty()).then(|| parts.join(" · ")),
    };
    (!info.is_empty()).then_some(info)
}

/// "2026:05:01 13:22:10" -> "2026-05-01 13:22"
fn format_date(raw: &str) -> String {
    let mut out = raw.to_owned();
    if raw.len() >= 10 && raw.as_bytes()[4] == b':' && raw.as_bytes()[7] == b':' {
        out.replace_range(4..5, "-");
        out.replace_range(7..8, "-");
    }
    if out.len() >= 16 && out.is_char_boundary(16) {
        out.truncate(16);
    }
    out
}

/// 1/125 s, 0.5 s, 2 s
fn format_exposure(n: u32, d: u32) -> String {
    if n == 0 {
        return "0 s".to_owned();
    }
    if n < d {
        let denominator = (f64::from(d) / f64::from(n)).round();
        format!("1/{denominator} s")
    } else {
        format!("{} s", trim_float(f64::from(n) / f64::from(d)))
    }
}

/// At most one decimal, trailing zeros dropped (1.8, 2, 5.6).
fn trim_float(v: f64) -> String {
    let mut s = format!("{v:.1}");
    if s.ends_with(".0") {
        s.truncate(s.len() - 2);
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a minimal big-endian EXIF: IFD0 with make, model and the Exif IFD pointer;
    /// the Exif IFD with date, exposure etc.
    pub(crate) fn sample() -> Vec<u8> {
        struct W(Vec<u8>);
        impl W {
            fn u16(&mut self, v: u16) {
                self.0.extend_from_slice(&v.to_be_bytes());
            }
            fn u32(&mut self, v: u32) {
                self.0.extend_from_slice(&v.to_be_bytes());
            }
        }
        // Layout: header 8 | IFD0 (3 entries) | Exif IFD (6 entries) | data
        let ifd0_at = 8u32;
        let ifd0_len = 2 + 3 * 12 + 4;
        let exif_at = ifd0_at + ifd0_len;
        let exif_len = 2 + 6 * 12 + 4;
        let data_at = exif_at + exif_len;
        let make = b"Canon\0";
        let model = b"Canon EOS R6\0";
        let date = b"2026:05:01 13:22:10\0";
        let lens = b"RF24-105mm\0";
        let make_at = data_at;
        let model_at = make_at + make.len() as u32;
        let taken_at = model_at + model.len() as u32;
        let lens_at = taken_at + date.len() as u32;
        let exposure_at = lens_at + lens.len() as u32;
        let fnum_at = exposure_at + 8;

        let mut w = W(b"MM\0*".to_vec());
        w.u32(ifd0_at);
        w.u16(3);
        for (tag, kind, count, value) in [
            (MAKE, 2, make.len() as u32, make_at),
            (MODEL, 2, model.len() as u32, model_at),
            (EXIF_IFD, 4, 1, exif_at),
        ] {
            w.u16(tag);
            w.u16(kind);
            w.u32(count);
            w.u32(value);
        }
        w.u32(0);
        w.u16(6);
        for (tag, kind, count, value) in [
            (EXPOSURE_TIME, 5, 1, exposure_at),
            (F_NUMBER, 5, 1, fnum_at),
            // A SHORT sits in the first two bytes of the value field.
            (ISO, 3, 1, 400 << 16),
            (DATE_TAKEN, 2, date.len() as u32, taken_at),
            (FOCAL_35MM, 3, 1, 50 << 16),
            (LENS_MODEL, 2, lens.len() as u32, lens_at),
        ] {
            w.u16(tag);
            w.u16(kind);
            w.u32(count);
            w.u32(value);
        }
        w.u32(0);
        for s in [&make[..], &model[..], &date[..], &lens[..]] {
            w.0.extend_from_slice(s);
        }
        w.u32(1);
        w.u32(125);
        w.u32(28);
        w.u32(10);
        let mut out = b"Exif\0\0".to_vec();
        out.extend(w.0);
        out
    }

    #[test]
    fn reads_the_fields_we_show() {
        let info = parse(&sample()).unwrap();
        assert_eq!(info.taken.as_deref(), Some("2026-05-01 13:22"));
        assert_eq!(info.camera.as_deref(), Some("Canon EOS R6"));
        assert_eq!(info.lens.as_deref(), Some("RF24-105mm"));
        assert_eq!(
            info.exposure.as_deref(),
            Some("1/125 s · f/2.8 · ISO 400 · 50 mm")
        );
    }

    #[test]
    fn broken_data_is_ignored() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"II*\0\xff\xff\xff\xff"), None);
        let mut cut = sample();
        cut.truncate(40);
        // Readable fields are used, unreadable ones skipped, no panic.
        let _ = parse(&cut);
        assert_eq!(format_exposure(1, 2), "1/2 s");
        assert_eq!(format_exposure(2, 1), "2 s");
    }
}
