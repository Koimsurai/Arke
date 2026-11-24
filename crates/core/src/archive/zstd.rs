//! zstd entries of a zip (#341), decoded by ruzstd: pure Rust, where the zip crate's own zstd is
//! the C library (and brings its encoder along). Read from the entry's raw bytes, frame after
//! frame (a parallel compressor writes several), with the CRC-32 checked at the end as the zip
//! crate does for the methods it decodes itself.

use std::io::{self, Read};

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

/// The zip method id of zstd (APPNOTE 4.4.5).
pub(super) const ZIP_ZSTD: u16 = 93;

/// The raw bytes of an entry, counted, to tell another frame from the end.
struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

/// An entry's decoded content.
pub(super) struct ZstdEntry<R: Read> {
    decoder: Option<StreamingDecoder<Counted<R>, FrameDecoder>>,
    /// Compressed size: the raw bytes there are.
    packed: u64,
    crc: crc32fast::Hasher,
    expected: u32,
}

impl<R: Read> ZstdEntry<R> {
    /// `raw`: the entry's compressed bytes, `packed` of them; `crc`: its CRC-32 from the zip.
    pub(super) fn new(raw: R, packed: u64, crc: u32) -> io::Result<Self> {
        let decoder = StreamingDecoder::new(Counted {
            inner: raw,
            read: 0,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            decoder: Some(decoder),
            packed,
            crc: crc32fast::Hasher::new(),
            expected: crc,
        })
    }
}

impl<R: Read> Read for ZstdEntry<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(0);
            };
            let n = decoder.read(buf)?;
            if n > 0 || buf.is_empty() {
                self.crc.update(&buf[..n]);
                return Ok(n);
            }
            // A frame ended: another follows while raw bytes are left.
            let Some(decoder) = self.decoder.take() else {
                return Ok(0);
            };
            let (source, frame) = decoder.into_parts();
            if source.read < self.packed {
                let next = StreamingDecoder::new_with_decoder(source, frame)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                self.decoder = Some(next);
                continue;
            }
            if self.crc.clone().finalize() != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd entry: CRC-32 mismatch",
                ));
            }
            return Ok(0);
        }
    }
}
