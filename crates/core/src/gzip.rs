//! Plain `.gz` files: one gzip-compressed file (`.tar.gz` is an archive, see [`crate::archive`]).
//! The preview shows the name stored in the header, the uncompressed size and the first bytes.
//!
//! Only the header, the last four bytes and the first `head_len` bytes of the content are read, so
//! a huge or maliciously compressed file costs the same as a small one.

use std::io::{self, Read, Seek, SeekFrom};

use flate2::read::GzDecoder;

/// Smallest valid gzip file: a 10 byte header, an empty deflate stream and the 8 byte trailer.
const MIN_LEN: u64 = 18;
/// Slack when judging whether the size field plausibly is the real size: stored (uncompressed)
/// deflate blocks and the header make the file a little larger than its content.
const OVERHEAD: u64 = 1024;

/// What the preview shows of a gzip file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GzipInfo {
    /// The file name stored in the header (`gzip` keeps it unless it read from a pipe), without
    /// any folder part.
    pub name: Option<String>,
    /// Uncompressed size, `None` when it can't be told: the size field in the trailer is the size
    /// modulo 4 GiB, so a value that couldn't be real for a file this large is not trusted.
    pub size: Option<u64>,
    /// The first bytes of the content (at most the `head_len` asked for).
    pub head: Vec<u8>,
}

/// Reads the preview of the gzip file in `reader`. An error means it isn't a readable gzip file.
pub fn inspect<R: Read + Seek>(mut reader: R, head_len: u64) -> io::Result<GzipInfo> {
    let compressed = reader.seek(SeekFrom::End(0))?;
    if compressed < MIN_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too short"));
    }
    reader.seek(SeekFrom::End(-4))?;
    let mut tail = [0u8; 4];
    reader.read_exact(&mut tail)?;
    let stored_size = u64::from(u32::from_le_bytes(tail));
    reader.seek(SeekFrom::Start(0))?;

    let mut decoder = GzDecoder::new(reader);
    let mut head = Vec::new();
    // One byte more than asked for tells whether the content ends inside the head.
    (&mut decoder)
        .take(head_len.saturating_add(1))
        .read_to_end(&mut head)?;
    let name = decoder
        .header()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no gzip header"))?
        .filename()
        .and_then(clean_name);

    let size = if head.len() as u64 <= head_len {
        // All of it was read: the exact size, whatever the trailer says.
        Some(head.len() as u64)
    } else {
        head.truncate(usize::try_from(head_len).unwrap_or(usize::MAX));
        (stored_size.saturating_add(OVERHEAD) >= compressed).then_some(stored_size)
    };
    Ok(GzipInfo { name, size, head })
}

/// The stored name as plain text: its last path part, with control characters dropped.
fn clean_name(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let leaf = text.rsplit(['/', '\\']).next().unwrap_or_default();
    let name: String = leaf.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use flate2::{Compression, GzBuilder};

    use super::*;

    fn gzip(name: Option<&str>, content: &[u8]) -> Vec<u8> {
        let mut builder = GzBuilder::new();
        if let Some(name) = name {
            builder = builder.filename(name);
        }
        let mut encoder = builder.write(Vec::new(), Compression::fast());
        encoder.write_all(content).unwrap();
        encoder.finish().unwrap()
    }

    /// Small content: its name, exact size and all of it.
    #[test]
    fn small_files_are_read_whole() {
        let bytes = gzip(Some("notes.txt"), b"hello gzip");
        let info = inspect(Cursor::new(bytes), 64).unwrap();
        assert_eq!(info.name.as_deref(), Some("notes.txt"));
        assert_eq!(info.size, Some(10));
        assert_eq!(info.head, b"hello gzip");
    }

    /// Content past the head: the head is cut, the size comes from the trailer.
    #[test]
    fn long_content_is_cut_and_sized_from_the_trailer() {
        let content = vec![b'a'; 100_000];
        let info = inspect(Cursor::new(gzip(Some("big.log"), &content)), 16).unwrap();
        assert_eq!(info.head, vec![b'a'; 16]);
        assert_eq!(info.size, Some(100_000));
    }

    /// A size field smaller than the file itself is a wrapped 4 GiB count: not shown.
    #[test]
    fn an_implausible_size_is_not_trusted() {
        let mut noise = Vec::new();
        let mut state = 7u32;
        for _ in 0..200_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            noise.push((state >> 24) as u8);
        }
        let mut bytes = gzip(None, &noise);
        let len = bytes.len();
        // Pretend the content was 4 GiB + 5 bytes: the field holds 5.
        bytes[len - 4..].copy_from_slice(&5u32.to_le_bytes());
        let info = inspect(Cursor::new(bytes), 16).unwrap();
        assert_eq!(info.size, None);
        assert_eq!(info.name, None);
        assert_eq!(info.head.len(), 16);
    }

    #[test]
    fn stored_names_lose_folders_and_control_characters() {
        assert_eq!(clean_name(b"C:\\data\\out.csv").as_deref(), Some("out.csv"));
        assert_eq!(clean_name(b"a/b/c.txt").as_deref(), Some("c.txt"));
        assert_eq!(clean_name(b"ev\x07il.txt").as_deref(), Some("evil.txt"));
        assert_eq!(clean_name(b"dir/").as_deref(), None);
        assert_eq!(clean_name(b"").as_deref(), None);
    }

    #[test]
    fn other_data_is_an_error() {
        assert!(inspect(Cursor::new(b"not gzip at all, just text".to_vec()), 16).is_err());
        assert!(inspect(Cursor::new(Vec::new()), 16).is_err());
    }
}
