//! Encoding detection for text previews: BOM, then UTF-16 (with or without BOM), then
//! UTF-8, then the system legacy code page (Big5 on Traditional Chinese systems).
//!
//! Only the head of the file is read (64 KB), so it may end mid-character; UTF-8 drops the
//! incomplete tail. Binary-looking data returns `None`.

/// Detected encoding, shown in the preview details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    /// UTF-8, with or without BOM.
    Utf8,
    /// UTF-16 little-endian.
    Utf16Le,
    /// UTF-16 big-endian.
    Utf16Be,
    /// System ANSI code page (Big5 on Traditional Chinese Windows).
    Legacy,
}

impl TextEncoding {
    /// Short display name ("UTF-8", "UTF-16 LE", "ANSI"...).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Legacy => "ANSI",
        }
    }
}

/// Decodes the head of a file. `legacy` decodes the system legacy encoding. `None` for binary data.
pub fn decode_text(
    bytes: &[u8],
    legacy: &dyn Fn(&[u8]) -> String,
) -> Option<(String, TextEncoding)> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some((utf8_prefix(rest), TextEncoding::Utf8));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some((utf16(rest, true), TextEncoding::Utf16Le));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some((utf16(rest, false), TextEncoding::Utf16Be));
    }
    // UTF-16 without BOM: mostly-ASCII text has a 0 in every other byte, always on the same side.
    if let Some(le) = utf16_without_bom(bytes) {
        return Some((
            utf16(bytes, le),
            if le {
                TextEncoding::Utf16Le
            } else {
                TextEncoding::Utf16Be
            },
        ));
    }
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some((text.to_owned(), TextEncoding::Utf8)),
        // Only the tail was cut mid-character: still UTF-8.
        Err(e) if e.error_len().is_none() => Some((utf8_prefix(bytes), TextEncoding::Utf8)),
        Err(_) => Some((legacy(bytes), TextEncoding::Legacy)),
    }
}

/// Valid UTF-8 prefix (truncated last char dropped); invalid bytes in the middle become U+FFFD.
fn utf8_prefix(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(e) if e.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| {
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Guesses BOM-less UTF-16: odd (LE) or even (BE) bytes are mostly 0 and the other side almost
/// never is.
fn utf16_without_bom(bytes: &[u8]) -> Option<bool> {
    let pairs = bytes.len() / 2;
    if pairs < 4 {
        return None;
    }
    let (mut zero_even, mut zero_odd) = (0usize, 0usize);
    for pair in bytes.as_chunks::<2>().0 {
        zero_even += usize::from(pair[0] == 0);
        zero_odd += usize::from(pair[1] == 0);
    }
    let mostly = |n: usize| n * 10 >= pairs * 4;
    let rarely = |n: usize| n * 20 <= pairs;
    if mostly(zero_odd) && rarely(zero_even) {
        Some(true)
    } else if mostly(zero_even) && rarely(zero_odd) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake "Big5" for tests: maps 0xA4 0xA4 to a single CJK char.
    fn fake_big5(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).replace('\u{FFFD}', "中")
    }

    fn le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn detects_encodings() {
        assert_eq!(
            decode_text("hello 世界".as_bytes(), &fake_big5),
            Some(("hello 世界".to_owned(), TextEncoding::Utf8))
        );
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(b"hi");
        assert_eq!(
            decode_text(&bom, &fake_big5),
            Some(("hi".to_owned(), TextEncoding::Utf8))
        );
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(le("中文 text"));
        assert_eq!(
            decode_text(&utf16, &fake_big5),
            Some(("中文 text".to_owned(), TextEncoding::Utf16Le))
        );
        assert_eq!(
            decode_text(&le("plain ascii text"), &fake_big5),
            Some(("plain ascii text".to_owned(), TextEncoding::Utf16Le))
        );
        let big5 = [b'a', 0xA4, 0xA4, b'b'];
        assert_eq!(
            decode_text(&big5, &fake_big5).map(|(_, e)| e),
            Some(TextEncoding::Legacy)
        );
        // Binary.
        assert_eq!(decode_text(&[0, 1, 2, 3, 0, 9, 7], &fake_big5), None);
    }

    #[test]
    fn cut_utf8_tail_is_dropped() {
        let text = "abc世";
        let bytes = &text.as_bytes()[..text.len() - 1];
        assert_eq!(
            decode_text(bytes, &fake_big5),
            Some(("abc".to_owned(), TextEncoding::Utf8))
        );
    }
}
