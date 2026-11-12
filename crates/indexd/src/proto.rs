//! Protocol between Arke and `arke-indexd`.
//!
//! Byte-mode named pipe; each message is prefixed with a 4-byte little-endian length. Paths are
//! sent as UTF-16 since Windows file names may not be valid UTF-8. The first message is always
//! [`Request::Hello`], and a version mismatch drops the connection.
//!
//! Later requests ([`Request::List`]) get new tags without a version bump: an older service
//! doesn't understand them and disconnects, and Arke falls back to its old path.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use arke_core::journal::{Change, Cursor, Listed};

/// Protocol version exchanged in [`Request::Hello`] / [`Reply::Hello`]; both sides must match.
pub const VERSION: u32 = 1;
/// Pipe the installed service listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\arke-indexd";
/// Windows service name (SCM).
pub const SERVICE_NAME: &str = "ArkeIndex";
/// Maximum message length.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Message from Arke to the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// First message on every connection.
    Hello {
        /// Client's [`VERSION`].
        version: u32,
    },
    /// Current journal position for this root (`C:\`).
    Cursor {
        /// Volume root, e.g. `C:\`.
        root: PathBuf,
    },
    /// Send changes from `from` onward and keep sending; the connection only carries [`Reply`]
    /// after this.
    Follow {
        /// Volume root to follow.
        root: PathBuf,
        /// Resume position (usually saved with the index).
        from: Cursor,
    },
    /// List the whole root from the MFT: several [`Reply::Listing`] batches, then
    /// [`Reply::ListDone`] (or [`Reply::ListFailed`]).
    List {
        /// Volume root to list.
        root: PathBuf,
    },
}

/// Message from the service to Arke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Answer to [`Request::Hello`].
    Hello {
        /// Service's [`VERSION`].
        version: u32,
    },
    /// `None`: unsupported (not NTFS, no journal, or not a local disk).
    Cursor(Option<Cursor>),
    /// Batch of changed paths while following.
    Changes {
        /// Changed paths in journal order.
        changes: Vec<Change>,
        /// Position after this batch.
        cursor: Cursor,
    },
    /// Backlog replayed; live changes from here on.
    CaughtUp,
    /// Journal gap (deleted, wrapped): the client must rebuild.
    Lost,
    /// Sent periodically when idle so either side notices the other is gone.
    Ping,
    /// One batch of the listing (order described on [`Listed`]).
    Listing(Vec<Listed>),
    /// Listing finished successfully.
    ListDone,
    /// Listing failed (not NTFS, read error).
    ListFailed(String),
}

/// Minimum encoded size of a listing item, used to check counts before trusting them.
const LISTED_MIN: usize = 4 + 4 + 8 + 4 + 1 + 2;

// ─── Encoding

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn path(&mut self, p: &std::path::Path) {
        let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
        self.u32(wide.len() as u32);
        for u in wide {
            self.0.extend_from_slice(&u.to_le_bytes());
        }
    }
    fn cursor(&mut self, c: Cursor) {
        self.u64(c.journal);
        self.i64(c.usn);
    }
    /// Short UTF-8 string (up to 64 KB), used for names. MFT names are already UTF-8 by now.
    fn str(&mut self, s: &str) {
        let bytes = &s.as_bytes()[..s.len().min(usize::from(u16::MAX))];
        self.0
            .extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        self.0.extend_from_slice(bytes);
    }
    fn listed(&mut self, l: &Listed) {
        self.u32(l.parent);
        self.u32(l.attributes);
        self.u64(l.size);
        self.u32(l.mtime);
        self.u8(u8::from(l.link));
        self.str(&l.name);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take().map(i64::from_le_bytes)
    }
    fn path(&mut self) -> Option<PathBuf> {
        let len = self.u32()? as usize;
        let bytes = self.0.get(..len.checked_mul(2)?)?;
        self.0 = &self.0[len * 2..];
        let wide: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        Some(PathBuf::from(OsString::from_wide(&wide)))
    }
    fn cursor(&mut self) -> Option<Cursor> {
        Some(Cursor {
            journal: self.u64()?,
            usn: self.i64()?,
        })
    }
    fn str(&mut self) -> Option<String> {
        let len = usize::from(u16::from_le_bytes(self.take()?));
        let bytes = self.0.get(..len)?;
        self.0 = &self.0[len..];
        String::from_utf8(bytes.to_vec()).ok()
    }
    fn listed(&mut self) -> Option<Listed> {
        Some(Listed {
            parent: self.u32()?,
            attributes: self.u32()?,
            size: self.u64()?,
            mtime: self.u32()?,
            link: self.u8()? != 0,
            name: self.str()?,
        })
    }
    fn end(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

impl Request {
    /// Serializes to a frame body (without the length prefix).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        match self {
            Self::Hello { version } => {
                w.u8(0);
                w.u32(*version);
            }
            Self::Cursor { root } => {
                w.u8(1);
                w.path(root);
            }
            Self::Follow { root, from } => {
                w.u8(2);
                w.path(root);
                w.cursor(*from);
            }
            Self::List { root } => {
                w.u8(3);
                w.path(root);
            }
        }
        w.0
    }

    /// Parses a frame body; `None` for unknown tags, truncation or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        let req = match r.u8()? {
            0 => Self::Hello { version: r.u32()? },
            1 => Self::Cursor { root: r.path()? },
            2 => Self::Follow {
                root: r.path()?,
                from: r.cursor()?,
            },
            3 => Self::List { root: r.path()? },
            _ => return None,
        };
        r.end()?;
        Some(req)
    }
}

impl Reply {
    /// Serializes to a frame body (without the length prefix).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        match self {
            Self::Hello { version } => {
                w.u8(0);
                w.u32(*version);
            }
            Self::Cursor(cursor) => {
                w.u8(1);
                match cursor {
                    Some(c) => {
                        w.u8(1);
                        w.cursor(*c);
                    }
                    None => w.u8(0),
                }
            }
            Self::Changes { changes, cursor } => {
                w.u8(2);
                w.cursor(*cursor);
                w.u32(changes.len() as u32);
                for c in changes {
                    w.u8(u8::from(c.moved_in));
                    w.path(&c.path);
                }
            }
            Self::CaughtUp => w.u8(3),
            Self::Lost => w.u8(4),
            Self::Ping => w.u8(5),
            Self::Listing(items) => {
                w.u8(6);
                w.u32(items.len() as u32);
                for item in items {
                    w.listed(item);
                }
            }
            Self::ListDone => w.u8(7),
            Self::ListFailed(why) => {
                w.u8(8);
                w.str(why);
            }
        }
        w.0
    }

    /// Parses a frame body; `None` for unknown tags, truncation, bad counts or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        let reply = match r.u8()? {
            0 => Self::Hello { version: r.u32()? },
            1 => Self::Cursor(match r.u8()? {
                0 => None,
                1 => Some(r.cursor()?),
                _ => return None,
            }),
            2 => {
                let cursor = r.cursor()?;
                let n = r.u32()? as usize;
                // Each change is at least 5 bytes: check the count against the length first.
                if n > r.0.len() / 5 {
                    return None;
                }
                let mut changes = Vec::with_capacity(n);
                for _ in 0..n {
                    let moved_in = r.u8()? != 0;
                    let path = r.path()?;
                    changes.push(Change { path, moved_in });
                }
                Self::Changes { changes, cursor }
            }
            3 => Self::CaughtUp,
            4 => Self::Lost,
            5 => Self::Ping,
            6 => {
                let n = r.u32()? as usize;
                if n > r.0.len() / LISTED_MIN {
                    return None;
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(r.listed()?);
                }
                Self::Listing(items)
            }
            7 => Self::ListDone,
            8 => Self::ListFailed(r.str()?),
            _ => return None,
        };
        r.end()?;
        Some(reply)
    }
}

/// Writes one message (length plus payload).
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Reads one message. Returns `UnexpectedEof` when the other side closes.
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_round_trip() {
        let cursor = Cursor {
            journal: u64::MAX - 1,
            usn: 9_876_543_210,
        };
        // A lone surrogate is a valid Windows file name but not valid UTF-8.
        let odd = PathBuf::from(OsString::from_wide(&[0x43, 0x3A, 0x5C, 0xD800, 0x42]));
        let requests = [
            Request::Hello { version: VERSION },
            Request::Cursor {
                root: PathBuf::from(r"C:\"),
            },
            Request::Follow {
                root: PathBuf::from(r"D:\"),
                from: cursor,
            },
            Request::List {
                root: PathBuf::from(r"C:\"),
            },
        ];
        for req in requests {
            assert_eq!(Request::decode(&req.encode()), Some(req));
        }
        // Non-ASCII on purpose: paths go as UTF-16, names as length-prefixed UTF-8 bytes.
        let replies = [
            Reply::Hello { version: 3 },
            Reply::Cursor(None),
            Reply::Cursor(Some(cursor)),
            Reply::Changes {
                changes: vec![
                    Change {
                        path: PathBuf::from(r"C:\Users\me\報告.docx"),
                        moved_in: false,
                    },
                    Change {
                        path: odd,
                        moved_in: true,
                    },
                ],
                cursor,
            },
            Reply::Changes {
                changes: vec![],
                cursor,
            },
            Reply::CaughtUp,
            Reply::Lost,
            Reply::Ping,
            Reply::Listing(vec![
                Listed {
                    parent: arke_core::journal::LISTED_ROOT,
                    name: "季報告.pdf".into(),
                    attributes: 0x20,
                    size: u64::MAX,
                    mtime: 1_700_000_000,
                    link: false,
                },
                Listed {
                    parent: 0,
                    name: String::new(),
                    attributes: 0x410,
                    size: 0,
                    mtime: 0,
                    link: true,
                },
            ]),
            Reply::Listing(vec![]),
            Reply::ListDone,
            Reply::ListFailed("not NTFS".into()),
        ];
        for reply in replies {
            assert_eq!(Reply::decode(&reply.encode()), Some(reply));
        }
    }

    #[test]
    fn rejects_garbage_and_lies_about_lengths() {
        assert_eq!(Request::decode(&[]), None);
        assert_eq!(Request::decode(&[9]), None);
        // Trailing bytes.
        let mut extra = Request::Hello { version: 1 }.encode();
        extra.push(0);
        assert_eq!(Request::decode(&extra), None);
        // Path length longer than the data.
        assert_eq!(Request::decode(&[1, 0xFF, 0xFF, 0, 0, 0x41, 0]), None);
        // Claims billions of changes.
        let mut lie = vec![2];
        lie.extend_from_slice(&[0; 16]);
        lie.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Reply::decode(&lie), None);
        // Claims a huge listing.
        let mut lie = vec![6];
        lie.extend_from_slice(&1000u32.to_le_bytes());
        lie.extend_from_slice(&[0; 100]);
        assert_eq!(Reply::decode(&lie), None);
    }

    #[test]
    fn frames_round_trip_and_refuse_huge_lengths() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), b"hello");
        assert_eq!(read_frame(&mut r).unwrap(), b"");
        assert_eq!(
            read_frame(&mut r).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let huge = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert_eq!(
            read_frame(&mut huge.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
