//! Reads the NTFS master file table ($MFT) directly to list a whole volume in seconds, so the
//! first index build doesn't have to walk every folder.
//!
//! Same approach as Everything: open the volume (admin, hence done in the service), locate $MFT
//! via `FSCTL_GET_NTFS_VOLUME_DATA`, and read it sequentially in large chunks following its $DATA
//! runs. Each record gets its update sequence (fixup) applied, then we take
//! `$STANDARD_INFORMATION` (mtime, attributes), `$FILE_NAME` (parent and name; one per hard link),
//! the unnamed `$DATA` (size) and `$REPARSE_POINT` (link type). Extension records are merged into
//! their base record.
//!
//! The volume is opened without the system cache (reads are sector aligned, see [`align_range`]),
//! in 4 MB requests (1 MB on a drive with a seek penalty, so a foreground read waits less), and
//! one chunk is read while the previous one is parsed. Extents are read in disk order within a
//! small window (see [`read_order`]) but always parsed in file order, so the result is the same as
//! a plain front-to-back read.
//!
//! Read-only. Parsing ([`Table::add`], [`decode_runs`], [`read_mft`], [`listing`]) never touches
//! a real disk and is tested with fake data; only [`read_volume`] opens the volume. ReFS has no
//! MFT: `FSCTL_GET_NTFS_VOLUME_DATA` fails and Arke walks folders as usual.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use arke_core::entry::{Entry, EntryKind};
use arke_core::journal::{LISTED_ROOT, Listed};

use crate::changes::Privacy;

/// The root directory is always record 5.
pub(crate) const ROOT: u32 = 5;
/// The first 24 records belong to the file system ($MFT, $LogFile, $Extend...) and aren't listed.
const FIRST_USER_RECORD: u32 = 24;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Record header flags.
const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;
/// `$FILE_NAME` namespace for 8.3-only names; skipped since the file also has a long name.
const NAMESPACE_DOS: u8 = 2;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Name surrogate bit of a reparse tag: symlinks, junctions, mount points (same rule as std's
/// `is_symlink`).
const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

/// Read size per request on a drive without a seek penalty (rounded down to whole records).
const CHUNK_FAST: usize = 4 * 1024 * 1024;
/// Read size per request on a drive with a seek penalty: a foreground read that arrives meanwhile
/// waits for at most one of these (about 6 ms at hard disk speed instead of about 25 ms).
const CHUNK_SEEK: usize = 1024 * 1024;
/// Memory for chunks being read or waiting to be parsed.
const BUFFER_BUDGET: usize = 12 * 1024 * 1024;
/// Most chunks in flight, however small they are.
const MAX_WINDOW: usize = 16;

/// Request size for a drive: small on one that reports a seek penalty (hard disk), the default
/// when it doesn't or the answer is unknown.
pub(crate) const fn chunk_bytes(seek_penalty: Option<bool>) -> usize {
    match seek_penalty {
        Some(true) => CHUNK_SEEK,
        _ => CHUNK_FAST,
    }
}

/// How many chunks of `chunk` bytes may be read ahead of the parser (at least 2 so reading and
/// parsing overlap).
pub(crate) const fn window_chunks(chunk: usize) -> usize {
    let n = BUFFER_BUDGET / if chunk == 0 { 1 } else { chunk };
    if n < 2 {
        2
    } else if n > MAX_WINDOW {
        MAX_WINDOW
    } else {
        n
    }
}

/// A byte range widened to the sector grid (for unbuffered reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AlignedRange {
    /// First byte to read (a multiple of the sector size, at or before the wanted offset).
    pub(crate) start: u64,
    /// Bytes to read (a multiple of the sector size).
    pub(crate) len: usize,
    /// Where the wanted bytes begin inside the widened range.
    pub(crate) skip: usize,
}

/// Widens `offset..offset + len` to whole sectors.
pub(crate) fn align_range(offset: u64, len: usize, sector: usize) -> Option<AlignedRange> {
    let sector = sector.max(1) as u64;
    let start = offset - offset % sector;
    let end = offset
        .checked_add(len as u64)?
        .checked_next_multiple_of(sector)?;
    Some(AlignedRange {
        start,
        len: usize::try_from(end - start).ok()?,
        skip: usize::try_from(offset - start).ok()?,
    })
}

/// Whether a read of `len` bytes at `offset` into memory at `addr` can go straight to an
/// unbuffered handle: offset, length and buffer address must all be sector aligned.
pub(crate) const fn is_aligned(offset: u64, addr: usize, len: usize, sector: usize) -> bool {
    let sector = if sector == 0 { 1 } else { sector };
    offset.is_multiple_of(sector as u64)
        && addr.is_multiple_of(sector)
        && len.is_multiple_of(sector)
}

/// Order to read chunks in. `keys[i]` is where chunk `i` lies on disk. The parser needs chunk 0,
/// 1, 2... in that order, so the reader may only run `window` chunks ahead of the first one not
/// yet read; within that window it sweeps upward by disk position (elevator) instead of jumping
/// back and forth when the MFT's extents lie out of order. Extents that are already in order give
/// plain `0, 1, 2...`.
pub(crate) fn read_order(keys: &[u64], window: usize) -> Vec<usize> {
    let n = keys.len();
    let window = window.max(1);
    let mut read = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let (mut first_unread, mut head) = (0usize, 0u64);
    while order.len() < n {
        let limit = (first_unread + window).min(n);
        let pick = (first_unread..limit)
            .filter(|&i| !read[i])
            .min_by_key(|&i| (keys[i] < head, keys[i], i));
        let Some(i) = pick else { break };
        read[i] = true;
        order.push(i);
        head = keys[i];
        while first_unread < n && read[first_unread] {
            first_unread += 1;
        }
    }
    order
}

/// 4 KB aligned page; volume reads must be sector aligned in memory too.
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

/// Zeroed memory aligned to 4 KB (enough for any sector size), usable as a byte slice.
struct AlignedBuf {
    pages: Vec<Page>,
}

impl AlignedBuf {
    fn new(bytes: usize) -> Self {
        Self {
            pages: vec![Page([0; 4096]); bytes.div_ceil(4096).max(1)],
        }
    }

    /// The first `len` bytes (fewer if the buffer is smaller).
    fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        let len = len.min(self.pages.len() * 4096);
        // SAFETY: pages is one contiguous allocation of pages.len() * 4096 initialized bytes,
        // len is clamped to it, and `u8` has no alignment or validity requirements.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_mut_ptr().cast::<u8>(), len) }
    }
}

/// Merged file info, indexed by base record number.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub(crate) size: u64,
    /// Modification time in Unix seconds, 0 if unknown.
    pub(crate) mtime: u32,
    /// `$STANDARD_INFORMATION` attributes; directories also get `FILE_ATTRIBUTE_DIRECTORY`.
    pub(crate) attributes: u32,
    pub(crate) reparse_tag: u32,
    flags: u8,
}

const INFO_IN_USE: u8 = 1;
const INFO_DIR: u8 = 2;
const INFO_HAS_SIZE: u8 = 4;

impl FileInfo {
    pub(crate) const fn in_use(&self) -> bool {
        self.flags & INFO_IN_USE != 0
    }

    pub(crate) const fn is_dir(&self) -> bool {
        self.flags & INFO_DIR != 0
    }

    /// Symlink, junction or mount point (not descended into). An unknown tag counts as a link.
    pub(crate) const fn is_link(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && (self.reparse_tag == 0 || self.reparse_tag & REPARSE_TAG_NAME_SURROGATE != 0)
    }
}

/// One name (a hard-linked file has one per name).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NameRef {
    pub(crate) parent: u32,
    pub(crate) file: u32,
    start: u32,
    len: u16,
}

/// Parsed MFT: info for every record plus all names.
#[derive(Default, Debug)]
pub(crate) struct Table {
    pub(crate) files: Vec<FileInfo>,
    pub(crate) names: Vec<NameRef>,
    pool: String,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(*b.get(at..)?.first_chunk()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*b.get(at..)?.first_chunk()?))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(*b.get(at..)?.first_chunk()?))
}

/// FILETIME (100 ns since 1601) to Unix seconds; before 1970 or too far ahead counts as unknown.
fn unix_secs(filetime: u64) -> u32 {
    (filetime / 10_000_000)
        .checked_sub(arke_core::time::UNIX_EPOCH_SECS)
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or(0)
}

/// Applies the update sequence: the last 2 bytes of each sector hold the sequence number and the
/// real bytes live in the sequence array. A mismatch (torn write) or bad layout rejects the record.
pub(crate) fn apply_fixups(rec: &mut [u8], sector: usize) -> bool {
    if rec.get(..4) != Some(b"FILE") || sector < 2 {
        return false;
    }
    let (Some(offset), Some(count)) = (le16(rec, 4), le16(rec, 6)) else {
        return false;
    };
    let (offset, count) = (usize::from(offset), usize::from(count));
    if count < 2 || (count - 1) * sector > rec.len() || offset + count * 2 > rec.len() {
        return false;
    }
    let usn = [rec[offset], rec[offset + 1]];
    for i in 1..count {
        let end = i * sector;
        if rec[end - 2..end] != usn {
            return false;
        }
        rec[end - 2] = rec[offset + 2 * i];
        rec[end - 1] = rec[offset + 2 * i + 1];
    }
    true
}

/// One attribute, starting at its header.
struct Attr<'a> {
    kind: u32,
    non_resident: bool,
    named: bool,
    bytes: &'a [u8],
}

impl<'a> Attr<'a> {
    /// Content of a resident attribute.
    fn value(&self) -> Option<&'a [u8]> {
        if self.non_resident {
            return None;
        }
        let len = le32(self.bytes, 0x10)? as usize;
        let at = usize::from(le16(self.bytes, 0x14)?);
        self.bytes.get(at..at.checked_add(len)?)
    }

    /// Non-resident: the first cluster this extent covers.
    fn start_vcn(&self) -> Option<u64> {
        self.non_resident.then(|| le64(self.bytes, 0x10)).flatten()
    }

    /// Real size of a non-resident attribute; only valid in the first extent (`start_vcn` 0).
    fn data_size(&self) -> Option<u64> {
        self.non_resident.then(|| le64(self.bytes, 0x30)).flatten()
    }

    fn runs(&self) -> Option<Vec<Run>> {
        let at = usize::from(le16(self.bytes, 0x20)?);
        decode_runs(self.bytes.get(at..)?, self.start_vcn()?)
    }

    /// Attribute size: content length if resident, real size if not.
    fn size(&self) -> Option<u64> {
        if self.non_resident {
            self.data_size()
        } else {
            le32(self.bytes, 0x10).map(u64::from)
        }
    }
}

/// Iterates a record's attributes, stopping at any implausible length (on-disk lengths are not
/// trusted).
fn attributes(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let used = le32(rec, 0x18).map_or(0, |u| (u as usize).min(rec.len()));
    let mut at = le16(rec, 0x14).map_or(usize::MAX, usize::from);
    std::iter::from_fn(move || {
        let kind = le32(rec, at)?;
        if kind == ATTR_END {
            return None;
        }
        let len = le32(rec, at + 4)? as usize;
        if len < 0x18 || at + len > used {
            return None;
        }
        let bytes = &rec[at..at + len];
        at += len;
        Some(Attr {
            kind,
            non_resident: bytes[8] != 0,
            named: bytes[9] != 0,
            bytes,
        })
    })
}

/// One data run: `len` clusters starting at virtual cluster `vcn`, stored at disk cluster `lcn`
/// (`None` is sparse, all zeros).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) vcn: u64,
    pub(crate) lcn: Option<u64>,
    pub(crate) len: u64,
}

/// Decodes a run list. Each run starts with a header byte: low nibble is the size of the length
/// field, high nibble the size of the offset field. Offsets are signed and relative to the
/// previous run; no offset means a sparse run. A 0 byte ends the list.
pub(crate) fn decode_runs(data: &[u8], start_vcn: u64) -> Option<Vec<Run>> {
    let mut out = Vec::new();
    let (mut i, mut lcn, mut vcn) = (0usize, 0i64, start_vcn);
    while let Some(&head) = data.get(i) {
        if head == 0 {
            break;
        }
        let (len_size, off_size) = (usize::from(head & 0xF), usize::from(head >> 4));
        i += 1;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return None;
        }
        let field = data.get(i..i + len_size + off_size)?;
        let len = field[..len_size]
            .iter()
            .rev()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let run_lcn = if off_size == 0 {
            None
        } else {
            let raw = &field[len_size..];
            // Sign extension: start from all ones if the top byte is negative.
            let start = if raw[off_size - 1] & 0x80 != 0 {
                -1i64
            } else {
                0
            };
            let delta = raw
                .iter()
                .rev()
                .fold(start, |acc, &b| (acc << 8) | i64::from(b));
            lcn = lcn.checked_add(delta)?;
            Some(u64::try_from(lcn).ok()?)
        };
        i += len_size + off_size;
        if len == 0 {
            return None;
        }
        out.push(Run {
            vcn,
            lcn: run_lcn,
            len,
        });
        vcn = vcn.checked_add(len)?;
    }
    Some(out)
}

impl Table {
    pub(crate) fn name(&self, n: &NameRef) -> &str {
        let start = n.start as usize;
        &self.pool[start..start + usize::from(n.len)]
    }

    fn slot(&mut self, recno: u32) -> &mut FileInfo {
        let ix = recno as usize;
        if self.files.len() <= ix {
            self.files.resize(ix + 1, FileInfo::default());
        }
        &mut self.files[ix]
    }

    /// Adds one record (`rec` gets its fixups applied). Extension records count toward their base.
    pub(crate) fn add(&mut self, recno: u32, rec: &mut [u8], sector: usize) {
        if !apply_fixups(rec, sector) {
            return;
        }
        let Some(flags) = le16(rec, 0x16) else {
            return;
        };
        if flags & RECORD_IN_USE == 0 {
            return;
        }
        let base = le64(rec, 0x20).unwrap_or(0) & 0xFFFF_FFFF_FFFF;
        let Ok(target) = u32::try_from(if base == 0 { u64::from(recno) } else { base }) else {
            return;
        };
        if base == 0 {
            let info = self.slot(target);
            info.flags |= INFO_IN_USE;
            if flags & RECORD_IS_DIRECTORY != 0 {
                info.flags |= INFO_DIR;
                info.attributes |= FILE_ATTRIBUTE_DIRECTORY;
            }
        }
        let rec: &[u8] = rec;
        for attr in attributes(rec) {
            match attr.kind {
                ATTR_STANDARD_INFORMATION => {
                    let Some(v) = attr.value() else { continue };
                    let (Some(mtime), Some(attrs)) = (le64(v, 8), le32(v, 0x20)) else {
                        continue;
                    };
                    let info = self.slot(target);
                    info.mtime = unix_secs(mtime);
                    info.attributes |= attrs;
                }
                ATTR_FILE_NAME => {
                    let Some(v) = attr.value() else { continue };
                    self.add_name(target, v);
                }
                ATTR_DATA if !attr.named => {
                    // A non-resident $DATA split into extents has its size in the first one only.
                    if attr.non_resident && attr.start_vcn() != Some(0) {
                        continue;
                    }
                    if let Some(size) = attr.size() {
                        let info = self.slot(target);
                        info.size = size;
                        info.flags |= INFO_HAS_SIZE;
                    }
                }
                ATTR_REPARSE_POINT => {
                    if let Some(tag) = attr.value().and_then(|v| le32(v, 0)) {
                        self.slot(target).reparse_tag = tag;
                    }
                }
                _ => {}
            }
        }
    }

    fn add_name(&mut self, file: u32, v: &[u8]) {
        let (Some(parent), Some(&len), Some(&namespace)) = (le64(v, 0), v.get(0x40), v.get(0x41))
        else {
            return;
        };
        if namespace == NAMESPACE_DOS {
            return;
        }
        let Some(raw) = v.get(0x42..0x42 + usize::from(len) * 2) else {
            return;
        };
        let Ok(parent) = u32::try_from(parent & 0xFFFF_FFFF_FFFF) else {
            return;
        };
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        let name = String::from_utf16_lossy(&units);
        // With no tag in `$STANDARD_INFORMATION`, `$FILE_NAME` keeps a copy (reparse points only).
        let fallback_tag = le32(v, 0x3C).unwrap_or(0);
        let fn_attrs = le32(v, 0x38).unwrap_or(0);
        let info = self.slot(file);
        if info.reparse_tag == 0 && fn_attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            info.reparse_tag = fallback_tag;
        }
        // The same name in the same folder (POSIX and Win32 namespaces) is kept once.
        let duplicate = self
            .names
            .iter()
            .rev()
            .take_while(|n| n.file == file)
            .any(|n| n.parent == parent && self.name(n) == name);
        let (Ok(start), Ok(len)) = (u32::try_from(self.pool.len()), u16::try_from(name.len()))
        else {
            return;
        };
        if duplicate {
            return;
        }
        self.pool.push_str(&name);
        self.names.push(NameRef {
            parent,
            file,
            start,
            len,
        });
    }
}

/// Volume reader (faked in tests). Offsets and lengths are multiples of the sector size.
pub(crate) trait Disk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> bool;
}

/// Volume geometry (`NTFS_VOLUME_DATA_BUFFER`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub(crate) cluster: u64,
    pub(crate) record: usize,
    pub(crate) sector: usize,
    pub(crate) mft_start_lcn: u64,
    /// Valid length of $MFT; the rest is reserved and unused.
    pub(crate) valid: u64,
}

/// Reads part of a stream following its run list, possibly across runs.
fn read_stream(
    disk: &mut impl Disk,
    geo: &Geometry,
    runs: &[Run],
    offset: u64,
    buf: &mut [u8],
) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let pos = offset + done as u64;
        let vcn = pos / geo.cluster;
        let ix = runs.partition_point(|r| r.vcn + r.len <= vcn);
        let Some(run) = runs.get(ix).filter(|r| r.vcn <= vcn) else {
            return false;
        };
        let run_end = (run.vcn + run.len) * geo.cluster;
        let n = (buf.len() - done).min(usize::try_from(run_end - pos).unwrap_or(usize::MAX));
        let out = &mut buf[done..done + n];
        match run.lcn {
            None => out.fill(0),
            Some(lcn) => {
                let at = lcn * geo.cluster + (pos - run.vcn * geo.cluster);
                if !disk.read(at, out) {
                    return false;
                }
            }
        }
        done += n;
    }
    true
}

/// Location of the MFT itself: the $DATA runs and real size of record 0 ($MFT). A very fragmented
/// run list may not fit in one record and spills into extension records (record 0 then has an
/// $ATTRIBUTE_LIST); those are read too.
fn mft_runs(disk: &mut impl Disk, geo: &Geometry) -> Option<(Vec<Run>, u64)> {
    let mut rec0 = vec![0u8; geo.record];
    if !disk.read(geo.mft_start_lcn * geo.cluster, &mut rec0)
        || !apply_fixups(&mut rec0, geo.sector)
    {
        return None;
    }
    let mut runs = Vec::new();
    let mut size = None;
    let mut list = None;
    for attr in attributes(&rec0) {
        match attr.kind {
            ATTR_DATA if !attr.named && attr.non_resident => {
                if attr.start_vcn() == Some(0) {
                    size = attr.data_size();
                }
                runs.extend(attr.runs()?);
            }
            ATTR_ATTRIBUTE_LIST => {
                list = Some((attr.value().map(<[u8]>::to_vec), attr.runs(), attr.size()));
            }
            _ => {}
        }
    }
    runs.sort_by_key(|r| r.vcn);
    if let Some((value, list_runs, list_size)) = list {
        // The attribute list itself may be non-resident.
        let bytes = if let Some(v) = value {
            v
        } else {
            let (list_runs, len) = (list_runs?, usize::try_from(list_size?).ok()?);
            let mut buf = vec![0u8; len.div_ceil(geo.sector) * geo.sector];
            if !read_stream(disk, geo, &list_runs, 0, &mut buf) {
                return None;
            }
            buf.truncate(len);
            buf
        };
        let mut at = 0usize;
        while let (Some(kind), Some(len)) = (le32(&bytes, at), le16(&bytes, at + 4)) {
            let len = usize::from(len);
            if len < 0x1A {
                break;
            }
            let named = bytes.get(at + 6).is_some_and(|&n| n != 0);
            let segment = le64(&bytes, at + 0x10).unwrap_or(0) & 0xFFFF_FFFF_FFFF;
            if kind == ATTR_DATA && !named && segment != 0 {
                let mut rec = vec![0u8; geo.record];
                let at_byte = segment * geo.record as u64;
                if !read_stream(disk, geo, &runs, at_byte, &mut rec)
                    || !apply_fixups(&mut rec, geo.sector)
                {
                    return None;
                }
                for attr in attributes(&rec) {
                    if attr.kind == ATTR_DATA && !attr.named && attr.non_resident {
                        if attr.start_vcn() == Some(0) {
                            size = attr.data_size();
                        }
                        runs.extend(attr.runs()?);
                    }
                }
                runs.sort_by_key(|r| r.vcn);
                runs.dedup_by_key(|r| r.vcn);
            }
            at += len;
        }
    }
    Some((runs, size?))
}

/// Where a byte of the stream lies on disk (`u64::MAX` if sparse or unmapped, read last).
fn disk_position(geo: &Geometry, runs: &[Run], pos: u64) -> u64 {
    let vcn = pos / geo.cluster;
    let ix = runs.partition_point(|r| r.vcn + r.len <= vcn);
    match runs.get(ix) {
        Some(&Run {
            vcn: first,
            lcn: Some(lcn),
            ..
        }) if first <= vcn => lcn * geo.cluster + (pos - first * geo.cluster),
        _ => u64::MAX,
    }
}

/// What to read and in which order.
struct Plan {
    per_chunk: usize,
    records: usize,
    window: usize,
    order: Vec<usize>,
}

/// Reads the chunks on a second thread (in `plan.order`) while this one parses them in file
/// order. The reader needs a free buffer per chunk and the parser frees one after each chunk, so
/// at most `plan.window` chunks are in memory; the reader only reads within `window` of the first
/// unread chunk (see [`read_order`]), so the parser's next chunk always gets read.
fn fill_table(
    disk: &mut (impl Disk + Send),
    geo: &Geometry,
    runs: &[Run],
    plan: &Plan,
    cancel: &dyn Fn() -> bool,
    table: &mut Table,
) -> Option<()> {
    let Plan {
        per_chunk,
        records,
        window,
        ref order,
    } = *plan;
    std::thread::scope(|scope| {
        let (free_tx, free_rx) = flume::unbounded::<AlignedBuf>();
        let (filled_tx, filled_rx) = flume::unbounded::<(usize, Option<AlignedBuf>)>();
        for _ in 0..window {
            free_tx.send(AlignedBuf::new(per_chunk * geo.record)).ok()?;
        }
        scope.spawn(move || {
            for &i in order {
                let Ok(mut buf) = free_rx.recv() else { return };
                let first = i * per_chunk;
                let n = per_chunk.min(records - first);
                let bytes = buf.bytes_mut(n * geo.record);
                let ok = read_stream(disk, geo, runs, (first * geo.record) as u64, bytes);
                if filled_tx.send((i, ok.then_some(buf))).is_err() || !ok {
                    return;
                }
            }
        });
        // Leaving early drops both channels, which ends the reader (blocked on a buffer, or
        // failing to hand one over).
        let mut waiting: HashMap<usize, AlignedBuf> = HashMap::new();
        for i in 0..records.div_ceil(per_chunk) {
            if cancel() {
                return None;
            }
            let mut buf = loop {
                if let Some(buf) = waiting.remove(&i) {
                    break buf;
                }
                let (j, buf) = filled_rx.recv().ok()?;
                waiting.insert(j, buf?);
            };
            let first = i * per_chunk;
            let n = per_chunk.min(records - first);
            for (k, rec) in buf
                .bytes_mut(n * geo.record)
                .chunks_exact_mut(geo.record)
                .enumerate()
            {
                table.add(u32::try_from(first + k).ok()?, rec, geo.sector);
            }
            let _ = free_tx.send(buf);
        }
        Some(())
    })
}

/// Reads the whole MFT in requests of about `chunk` bytes. Stops with `None` once `cancel()`
/// returns true.
pub(crate) fn read_mft(
    disk: &mut (impl Disk + Send),
    geo: &Geometry,
    chunk: usize,
    cancel: &dyn Fn() -> bool,
) -> Option<Table> {
    if geo.record == 0
        || geo.sector == 0
        || geo.cluster == 0
        || !geo.record.is_multiple_of(geo.sector)
    {
        return None;
    }
    let (runs, size) = mft_runs(disk, geo)?;
    let total = size.min(geo.valid);
    let records = usize::try_from(total / geo.record as u64).ok()?;
    let mut table = Table {
        files: Vec::with_capacity(records),
        ..Table::default()
    };
    // No bigger than the MFT itself, so a small one doesn't allocate a full chunk per buffer.
    let per_chunk = (chunk / geo.record).clamp(1, records.max(1));
    let window = window_chunks(per_chunk * geo.record);
    let keys: Vec<u64> = (0..records.div_ceil(per_chunk))
        .map(|i| disk_position(geo, &runs, (i * per_chunk * geo.record) as u64))
        .collect();
    let plan = Plan {
        per_chunk,
        records,
        window,
        order: read_order(&keys, window),
    };
    fill_table(disk, geo, &runs, &plan, cancel, &mut table)?;
    Some(table)
}

/// Lists breadth-first from the root: parents always come before children, and siblings are
/// contiguous (Arke's build folder detection looks at siblings). Nothing is sent from inside
/// folders Arke doesn't descend into ([`arke_core::index::descends_into`]), such as other users'
/// profiles or the recycle bin. Stops and returns false once `emit` returns false.
pub(crate) fn listing(
    table: &mut Table,
    root: &Path,
    privacy: &Privacy,
    mut emit: impl FnMut(Listed) -> bool,
) -> bool {
    table.names.sort_unstable_by_key(|n| n.parent);
    let table: &Table = table;
    let mut visited = vec![false; table.files.len()];
    let mut queue: VecDeque<(u32, u32, PathBuf)> = VecDeque::new();
    queue.push_back((ROOT, LISTED_ROOT, root.to_path_buf()));
    let mut next: u32 = 0;
    while let Some((dir, dir_ix, dir_path)) = queue.pop_front() {
        match visited.get_mut(dir as usize) {
            Some(seen) if !*seen => *seen = true,
            _ => continue,
        }
        let start = table.names.partition_point(|n| n.parent < dir);
        let end = table.names.partition_point(|n| n.parent <= dir);
        for name in &table.names[start..end] {
            let Some(info) = table.files.get(name.file as usize) else {
                continue;
            };
            if name.file < FIRST_USER_RECORD || name.file == dir || !info.in_use() {
                continue;
            }
            let text = table.name(name);
            let link = info.is_link();
            let ix = next;
            next += 1;
            if info.is_dir() && !link {
                let entry = Entry::new(text, EntryKind::Dir, 0, None, info.attributes);
                let path = dir_path.join(text);
                // Another user's profile: the folder itself is listed, its contents aren't.
                if arke_core::index::descends_into(&entry) && privacy.allows(&path.join("x")) {
                    queue.push_back((name.file, ix, path));
                }
            }
            let listed = Listed {
                parent: dir_ix,
                name: text.to_string(),
                attributes: info.attributes,
                size: if info.is_dir() { 0 } else { info.size },
                mtime: info.mtime,
                link,
            };
            if !emit(listed) {
                return false;
            }
        }
    }
    true
}

/// Why [`read_volume`] failed. Display is the text logged and sent to clients as `ListFailed`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MftError {
    /// The volume could not be opened (usually: not running as administrator).
    #[error("{}", .source.message())]
    Open {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The volume did not answer the NTFS volume data request (not NTFS).
    #[error("not NTFS: {}", .source.message())]
    NotNtfs {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The MFT records could not be read, or reading was cancelled.
    #[error("cannot read the MFT")]
    Read,
}

/// Whether the drive under `volume` has a seek penalty (a hard disk), as its driver reports it.
/// `None` when it can't be read (a volume over several disks, an old driver).
fn seek_penalty(volume: &crate::usn::Volume) -> Option<bool> {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
        StorageDeviceSeekPenaltyProperty,
    };
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version, Size, then IncursSeekPenalty (one byte). Read as
    // plain integers, so no byte the driver writes can make an invalid `bool`.
    let mut answer = [0u32; 3];
    let mut written = 0u32;
    // SAFETY: the handle is open; the query and answer buffers outlive the call and their sizes
    // are passed with them; no OVERLAPPED, so the call completes before it returns.
    unsafe {
        DeviceIoControl(
            volume.handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(answer.as_mut_ptr().cast()),
            size_of_val(&answer) as u32,
            Some(&raw mut written),
            None,
        )
    }
    .ok()?;
    (written >= 9).then_some(answer[2] & 0xFF != 0)
}

/// Marks this handle's I/O as very low priority: Windows serves it only when nothing of normal
/// priority waits (with a trickle so it can't starve), so a rescan yields to the user.
fn lower_io_priority(volume: &crate::usn::Volume) -> bool {
    use windows::Win32::Storage::FileSystem::{
        FILE_IO_PRIORITY_HINT_INFO, FileIoPriorityHintInfo, IoPriorityHintVeryLow,
        SetFileInformationByHandle,
    };
    let info = FILE_IO_PRIORITY_HINT_INFO {
        PriorityHint: IoPriorityHintVeryLow,
    };
    // SAFETY: the handle is open; info is a valid FILE_IO_PRIORITY_HINT_INFO of the size passed.
    unsafe {
        SetFileInformationByHandle(
            volume.handle(),
            FileIoPriorityHintInfo,
            (&raw const info).cast(),
            size_of::<FILE_IO_PRIORITY_HINT_INFO>() as u32,
        )
    }
    .is_ok()
}

/// Opens the volume and reads the MFT. Needs admin; fails on non-NTFS volumes. `background` reads
/// at very low I/O priority (for a rescan when an older index is already usable).
pub(crate) fn read_volume(
    root: &Path,
    background: bool,
    cancel: &dyn Fn() -> bool,
) -> Result<Table, MftError> {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
    let started = Instant::now();
    let volume =
        crate::usn::Volume::open_unbuffered(root).map_err(|source| MftError::Open { source })?;
    let low = background && lower_io_priority(&volume);
    let seek = seek_penalty(&volume);
    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    // SAFETY: the out buffer is a valid NTFS_VOLUME_DATA_BUFFER of the right size; sync call.
    unsafe {
        DeviceIoControl(
            volume.handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some((&raw mut data).cast()),
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(|source| MftError::NotNtfs { source })?;
    let geo = Geometry {
        cluster: u64::from(data.BytesPerCluster),
        record: data.BytesPerFileRecordSegment as usize,
        sector: data.BytesPerSector as usize,
        mft_start_lcn: data.MftStartLcn.max(0).cast_unsigned(),
        valid: data.MftValidDataLength.max(0).cast_unsigned(),
    };
    let mut disk = VolumeDisk {
        volume: &volume,
        sector: geo.sector,
    };
    let table = read_mft(&mut disk, &geo, chunk_bytes(seek), cancel).ok_or(MftError::Read)?;
    log::info!(
        "{}: MFT {} records, {} names, {:.1} s (seek penalty {seek:?}, low priority {low})",
        root.display(),
        table.files.len(),
        table.names.len(),
        started.elapsed().as_secs_f32()
    );
    Ok(table)
}

struct VolumeDisk<'a> {
    volume: &'a crate::usn::Volume,
    sector: usize,
}

impl VolumeDisk<'_> {
    /// One read of exactly `buf.len()` bytes; offset, length and address must be sector aligned.
    fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> bool {
        use windows::Win32::Storage::FileSystem::ReadFile;
        use windows::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
        let len = buf.len();
        let mut overlapped = OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: offset as u32,
                    OffsetHigh: (offset >> 32) as u32,
                },
            },
            ..OVERLAPPED::default()
        };
        let mut read = 0u32;
        // SAFETY: sync handle; buf, read and overlapped are valid for the call (OVERLAPPED only
        // sets the offset).
        let ok = unsafe {
            ReadFile(
                self.volume.handle(),
                Some(buf),
                Some(&raw mut read),
                Some(&raw mut overlapped),
            )
        }
        .is_ok();
        ok && read as usize == len
    }
}

impl Disk for VolumeDisk<'_> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> bool {
        // The chunk reader hands over aligned memory and sector-multiple ranges, so this goes
        // straight into the caller's buffer. Anything else (the first record, an attribute list)
        // is read widened into a bounce buffer.
        if is_aligned(offset, buf.as_ptr().addr(), buf.len(), self.sector) {
            return self.read_aligned(offset, buf);
        }
        let Some(range) = align_range(offset, buf.len(), self.sector) else {
            return false;
        };
        let mut bounce = AlignedBuf::new(range.len);
        let bytes = bounce.bytes_mut(range.len);
        if !self.read_aligned(range.start, bytes) {
            return false;
        }
        buf.copy_from_slice(&bytes[range.skip..range.skip + buf.len()]);
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SECTOR: usize = 512;
    const RECORD: usize = 1024;

    /// Builds one attribute of a record.
    pub(crate) fn resident(kind: u32, value: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 0x18];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a.extend_from_slice(value);
        while !a.len().is_multiple_of(8) {
            a.push(0);
        }
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    fn non_resident(kind: u32, start_vcn: u64, size: u64, runlist: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 0x40];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[8] = 1;
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a.extend_from_slice(runlist);
        while !a.len().is_multiple_of(8) {
            a.push(0);
        }
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    fn standard_info(mtime_unix: u64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        let ft = (mtime_unix + 11_644_473_600) * 10_000_000;
        v[8..16].copy_from_slice(&ft.to_le_bytes());
        v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, &v)
    }

    fn file_name(parent: u64, name: &str, namespace: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42];
        // Parent reference: the top 16 bits are the sequence number.
        v[0..8].copy_from_slice(&(parent | (7 << 48)).to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = namespace;
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, &v)
    }

    /// Builds a full record, including the update sequence (last 2 bytes of each sector replaced).
    pub(crate) fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RECORD];
        r[0..4].copy_from_slice(b"FILE");
        let usa_count = (RECORD / SECTOR + 1) as u16;
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&usa_count.to_le_bytes());
        let first_attr = 0x38u16;
        r[0x14..0x16].copy_from_slice(&first_attr.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut at = usize::from(first_attr);
        for a in attrs {
            r[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        r[0x18..0x1C].copy_from_slice(&((at + 8) as u32).to_le_bytes());
        // Update sequence: number 0xABCD, original bytes saved to the array.
        r[0x30..0x32].copy_from_slice(&0xABCDu16.to_le_bytes());
        for i in 1..usize::from(usa_count) {
            let end = i * SECTOR;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x30 + 2 * i] = a;
            r[0x30 + 2 * i + 1] = b;
            r[end - 2..end].copy_from_slice(&0xABCDu16.to_le_bytes());
        }
        r
    }

    const IN_USE: u16 = RECORD_IN_USE;
    const DIR: u16 = RECORD_IN_USE | RECORD_IS_DIRECTORY;

    fn dir(parent: u64, name: &str, attrs: u32) -> Vec<u8> {
        record(
            DIR,
            0,
            &[
                standard_info(1_700_000_000, attrs),
                file_name(parent, name, 1),
            ],
        )
    }

    fn file(parent: u64, name: &str, size: u64) -> Vec<u8> {
        record(
            IN_USE,
            0,
            &[
                standard_info(1_700_000_100, 0x20),
                file_name(parent, name, 3),
                resident(ATTR_DATA, &vec![0x55; size as usize]),
            ],
        )
    }

    #[test]
    fn fixups_restore_sector_tails_and_reject_torn_records() {
        let mut r = file(5, "a.txt", 3);
        let tail_before = r[SECTOR - 2..SECTOR].to_vec();
        assert_eq!(tail_before, 0xABCDu16.to_le_bytes());
        assert!(apply_fixups(&mut r, SECTOR));
        assert_eq!(&r[SECTOR - 2..SECTOR], &[0, 0]);
        // Torn write: the second sector's sequence number differs.
        let mut torn = file(5, "a.txt", 3);
        torn[2 * SECTOR - 1] ^= 0xFF;
        assert!(!apply_fixups(&mut torn, SECTOR));
        let mut junk = vec![0u8; RECORD];
        assert!(!apply_fixups(&mut junk, SECTOR));
    }

    #[test]
    fn decodes_runs_with_negative_offsets_and_sparse_gaps() {
        // Length 0x10 at LCN 0x100; length 8 sparse; length 4 at 0x100 - 0x10 = 0xF0.
        let runs = decode_runs(
            &[0x21, 0x10, 0x00, 0x01, 0x01, 0x08, 0x11, 0x04, 0xF0, 0x00],
            0,
        )
        .unwrap();
        assert_eq!(
            runs,
            [
                Run {
                    vcn: 0,
                    lcn: Some(0x100),
                    len: 0x10
                },
                Run {
                    vcn: 0x10,
                    lcn: None,
                    len: 8
                },
                Run {
                    vcn: 0x18,
                    lcn: Some(0xF0),
                    len: 4
                },
            ]
        );
        // Field longer than the data; zero length.
        assert_eq!(decode_runs(&[0x44, 1, 2], 0), None);
        assert_eq!(decode_runs(&[0x11, 0, 5, 0], 0), None);
    }

    #[test]
    fn records_merge_names_sizes_and_extension_records() {
        let mut t = Table::default();
        // Named streams (e.g. Zone.Identifier) don't count toward size.
        let mut named_stream = resident(ATTR_DATA, b"named streams don't count");
        named_stream[9] = 1;
        t.add(5, &mut dir(5, ".", 0x16), SECTOR);
        // Long name plus short name: the short one isn't listed.
        t.add(
            30,
            &mut record(
                IN_USE,
                0,
                &[
                    standard_info(1_700_000_000, 0x20),
                    file_name(5, "Quarterly Report.docx", 1),
                    file_name(5, "QUARTE~1.DOC", NAMESPACE_DOS),
                    non_resident(ATTR_DATA, 0, 123_456_789, &[0x11, 0x01, 0x40, 0]),
                    named_stream.clone(),
                ],
            ),
            SECTOR,
        );
        // Extension record (base 31): a second hard link name and the $DATA.
        t.add(
            31,
            &mut record(
                IN_USE,
                0,
                &[standard_info(1_700_000_000, 0x20), file_name(5, "a.txt", 1)],
            ),
            SECTOR,
        );
        t.add(
            40,
            &mut record(
                IN_USE,
                31,
                &[
                    file_name(30, "link-to-a.txt", 1),
                    resident(ATTR_DATA, b"hello"),
                ],
            ),
            SECTOR,
        );
        // Record not in use.
        t.add(
            41,
            &mut record(0, 0, &[file_name(5, "deleted.txt", 1)]),
            SECTOR,
        );
        let names: Vec<(u32, u32, &str)> = t
            .names
            .iter()
            .map(|n| (n.parent, n.file, t.name(n)))
            .collect();
        assert_eq!(
            names,
            [
                (5, 5, "."),
                (5, 30, "Quarterly Report.docx"),
                (5, 31, "a.txt"),
                (30, 31, "link-to-a.txt"),
            ]
        );
        assert_eq!(t.files[30].size, 123_456_789);
        assert_eq!(t.files[31].size, 5);
        assert_eq!(t.files[31].mtime, 1_700_000_000);
        assert!(t.files[5].is_dir() && t.files[5].attributes & FILE_ATTRIBUTE_DIRECTORY != 0);
        assert!(t.files.get(41).is_none_or(|f| !f.in_use()));
    }

    /// Fake disk backed by memory.
    struct Memory(Vec<u8>);

    impl Disk for Memory {
        fn read(&mut self, offset: u64, buf: &mut [u8]) -> bool {
            let at = offset as usize;
            match self.0.get(at..at + buf.len()) {
                Some(src) => {
                    buf.copy_from_slice(src);
                    true
                }
                None => false,
            }
        }
    }

    /// Fake volume: 4 KB clusters, $MFT in two runs (record 0 at cluster 2, the rest at 10).
    fn fake_volume(records: Vec<(u32, Vec<u8>)>) -> (Memory, Geometry) {
        fake_volume_in(records, false)
    }

    /// As [`fake_volume`]; with `reversed` the first half of the MFT lies after the second half
    /// on disk (second run at cluster 2, first at 10), so file order and disk order differ.
    fn fake_volume_in(records: Vec<(u32, Vec<u8>)>, reversed: bool) -> (Memory, Geometry) {
        const CLUSTER: usize = 4096;
        let per_cluster = CLUSTER / RECORD;
        let total = 64usize;
        let mut disk = vec![0u8; 20 * CLUSTER];
        // Clusters 2..10 hold the first 32 records, clusters 10..18 the last 32 (or the other way
        // round when reversed; the run list then steps back by 8 clusters, 0xF8 as a signed byte).
        let (first_at, second_at) = if reversed { (10, 2) } else { (2, 10) };
        let runlist = if reversed {
            [0x11, 8, 10, 0x11, 8, 0xF8, 0]
        } else {
            [0x11, 8, 2, 0x11, 8, 8, 0]
        };
        let mut all = records;
        all.push((
            0,
            record(
                IN_USE,
                0,
                &[non_resident(
                    ATTR_DATA,
                    0,
                    (total * RECORD) as u64,
                    &runlist,
                )],
            ),
        ));
        for (recno, rec) in all {
            let vcn = recno as usize / per_cluster;
            let lcn = if vcn < 8 {
                first_at + vcn
            } else {
                second_at + (vcn - 8)
            };
            let at = lcn * CLUSTER + (recno as usize % per_cluster) * RECORD;
            disk[at..at + RECORD].copy_from_slice(&rec);
        }
        let geo = Geometry {
            cluster: CLUSTER as u64,
            record: RECORD,
            sector: SECTOR,
            mft_start_lcn: first_at as u64,
            valid: (total * RECORD) as u64,
        };
        (Memory(disk), geo)
    }

    fn privacy() -> Privacy {
        Privacy {
            profiles: Some(PathBuf::from(r"C:\Users")),
            mine: Some(PathBuf::from(r"C:\Users\Me")),
        }
    }

    #[test]
    fn reads_a_fragmented_mft_and_lists_parents_first_without_private_folders() {
        let records = vec![
            (5, dir(5, ".", 0x16)),
            (11, dir(5, "$Extend", 0x16)),
            (24, dir(5, "Users", 0x11)),
            (25, dir(24, "Me", 0x10)),
            (26, dir(24, "Bob", 0x10)),
            (27, file(25, "mine.txt", 4)),
            (28, file(26, "bobs-secret.txt", 4)),
            (29, dir(5, "$Recycle.Bin", 0x16)),
            (30, file(29, "deleted.txt", 1)),
            // Second half (record 32 and up, in the other run).
            (40, dir(25, "Projects", 0x10)),
            (41, file(40, "notes.md", 10)),
            (
                42,
                record(
                    DIR,
                    0,
                    &[
                        standard_info(1_700_000_000, 0x10 | FILE_ATTRIBUTE_REPARSE_POINT),
                        file_name(25, "Junction", 1),
                        resident(ATTR_REPARSE_POINT, &0xA000_0003u32.to_le_bytes()),
                    ],
                ),
            ),
            (43, file(42, "through-junction.txt", 1)),
        ];
        let (mut disk, geo) = fake_volume(records);
        let mut table = read_mft(&mut disk, &geo, CHUNK_FAST, &|| false).unwrap();
        let mut out = Vec::new();
        assert!(listing(&mut table, Path::new(r"C:\"), &privacy(), |l| {
            out.push(l);
            true
        }));
        let path_of = |ix: usize| {
            let mut parts = vec![out[ix].name.clone()];
            let mut p = out[ix].parent;
            while p != LISTED_ROOT {
                assert!((p as usize) < ix, "parent folders come first");
                parts.push(out[p as usize].name.clone());
                p = out[p as usize].parent;
            }
            parts.reverse();
            parts.join("\\")
        };
        let mut paths: Vec<String> = (0..out.len()).map(path_of).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "$Recycle.Bin",
                "Users",
                "Users\\Bob",
                "Users\\Me",
                "Users\\Me\\Junction",
                "Users\\Me\\Projects",
                "Users\\Me\\Projects\\notes.md",
                "Users\\Me\\mine.txt",
            ]
        );
        let notes = out.iter().find(|l| l.name == "notes.md").unwrap();
        assert_eq!((notes.size, notes.mtime), (10, 1_700_000_100));
        assert!(out.iter().find(|l| l.name == "Junction").unwrap().link);
        // Stopping early.
        let mut n = 0;
        assert!(!listing(&mut table, Path::new(r"C:\"), &privacy(), |_| {
            n += 1;
            n < 2
        }));
    }

    #[test]
    fn refuses_bad_geometry_and_cancels() {
        let (mut disk, mut geo) = fake_volume(vec![(5, dir(5, ".", 0x16))]);
        assert!(read_mft(&mut disk, &geo, CHUNK_FAST, &|| true).is_none());
        geo.record = 1000;
        assert!(read_mft(&mut disk, &geo, CHUNK_FAST, &|| false).is_none());
    }

    #[test]
    fn widens_unaligned_reads_to_whole_sectors() {
        // Already aligned: nothing to skip.
        assert_eq!(
            align_range(4096, 8192, 512),
            Some(AlignedRange {
                start: 4096,
                len: 8192,
                skip: 0
            })
        );
        // Starts mid-sector and ends mid-sector: both ends move out.
        assert_eq!(
            align_range(1000, 100, 512),
            Some(AlignedRange {
                start: 512,
                len: 1024,
                skip: 488
            })
        );
        // Ends exactly on a sector boundary after widening the start only.
        assert_eq!(
            align_range(4100, 4092, 4096),
            Some(AlignedRange {
                start: 4096,
                len: 4096,
                skip: 4
            })
        );
        // A record smaller than a 4Kn sector reads the whole sector.
        assert_eq!(
            align_range(8192 + 1024, 1024, 4096),
            Some(AlignedRange {
                start: 8192,
                len: 4096,
                skip: 1024
            })
        );
        assert_eq!(align_range(u64::MAX - 10, 100, 512), None);
        for (offset, len) in [(0u64, 1usize), (511, 2), (513, 511), (4095, 4097)] {
            let r = align_range(offset, len, 512).unwrap();
            assert_eq!(r.start % 512, 0);
            assert_eq!(r.len % 512, 0);
            assert_eq!(r.start + r.skip as u64, offset);
            assert!(r.skip + len <= r.len);
            assert!(r.len < len + 2 * 512);
        }
    }

    #[test]
    fn direct_reads_need_offset_address_and_length_on_the_sector_grid() {
        assert!(is_aligned(4096, 8192, 1024 * 1024, 512));
        assert!(!is_aligned(4097, 8192, 4096, 512));
        assert!(!is_aligned(4096, 8193, 4096, 512));
        assert!(!is_aligned(4096, 8192, 4097, 512));
        assert!(!is_aligned(512, 8192, 4096, 4096));
        // A buffer from AlignedBuf always qualifies, at any sector size up to 4096.
        let mut buf = AlignedBuf::new(10_000);
        assert!(buf.bytes_mut(8192).as_ptr().addr().is_multiple_of(4096));
        assert_eq!(buf.bytes_mut(1 << 30).len(), 12_288);
    }

    #[test]
    fn small_requests_on_a_drive_with_a_seek_penalty() {
        assert_eq!(chunk_bytes(Some(true)), 1024 * 1024);
        assert_eq!(chunk_bytes(Some(false)), 4 * 1024 * 1024);
        // Unknown keeps the default.
        assert_eq!(chunk_bytes(None), 4 * 1024 * 1024);
        // Whole records either way, and the memory in flight stays about the same.
        for chunk in [chunk_bytes(Some(true)), chunk_bytes(Some(false))] {
            assert_eq!(chunk % 4096, 0);
            let window = window_chunks(chunk);
            assert!((2..=MAX_WINDOW).contains(&window));
            assert!(window * chunk <= BUFFER_BUDGET.max(2 * chunk));
        }
        assert_eq!(window_chunks(0), MAX_WINDOW);
    }

    /// Replays a read order the way the reader and parser interact: a chunk may only be read while
    /// it lies within `window` of the first unparsed chunk, and the parser takes chunks in file
    /// order as soon as they are there.
    fn check_order(keys: &[u64], window: usize) -> Vec<usize> {
        let order = read_order(keys, window);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..keys.len()).collect::<Vec<_>>(), "a permutation");
        let (mut read, mut next) = (vec![false; keys.len()], 0usize);
        for &i in &order {
            assert!(i < next + window, "chunk {i} is too far ahead of {next}");
            read[i] = true;
            while next < keys.len() && read[next] {
                next += 1;
            }
        }
        assert_eq!(next, keys.len());
        order
    }

    #[test]
    fn extents_in_file_order_are_read_front_to_back() {
        let keys: Vec<u64> = (0..40).map(|i| 1_000_000 + i * 4096).collect();
        assert_eq!(check_order(&keys, 3), (0..40).collect::<Vec<_>>());
        assert!(check_order(&[], 3).is_empty());
        assert_eq!(check_order(&[7], 3), [0]);
    }

    #[test]
    fn out_of_order_extents_are_swept_in_disk_order_within_the_window() {
        // Chunks 0-3 lie after chunks 4-7 on disk; the window covers four chunks.
        let keys = [80, 90, 100, 110, 10, 20, 30, 40];
        let order = check_order(&keys, 4);
        // Window 0..4 only holds the late extent: read it upward, then the early one.
        assert_eq!(order, [0, 1, 2, 3, 4, 5, 6, 7]);
        // With room to look ahead the early extent goes first where that is allowed.
        let order = check_order(&keys, 8);
        assert_eq!(order, [4, 5, 6, 7, 0, 1, 2, 3]);
        // Reversed extents: still one upward sweep inside each window.
        let keys = [50, 40, 30, 20, 10];
        assert_eq!(check_order(&keys, 5), [4, 3, 2, 1, 0]);
        assert_eq!(check_order(&keys, 2), [1, 0, 3, 2, 4]);
        // A sparse chunk (no disk position) goes last in its window.
        let keys = [u64::MAX, 5, 6];
        assert_eq!(check_order(&keys, 3), [1, 2, 0]);
    }

    /// The old reader: one chunk at a time, in file order, nothing overlapped.
    fn read_mft_sequential(disk: &mut Memory, geo: &Geometry, chunk: usize) -> Option<Table> {
        let (runs, size) = mft_runs(disk, geo)?;
        let records = usize::try_from(size.min(geo.valid) / geo.record as u64).ok()?;
        let mut table = Table::default();
        let per_chunk = (chunk / geo.record).max(1);
        let mut buf = vec![0u8; per_chunk * geo.record];
        let mut first = 0usize;
        while first < records {
            let n = per_chunk.min(records - first);
            let bytes = &mut buf[..n * geo.record];
            if !read_stream(disk, geo, &runs, (first * geo.record) as u64, bytes) {
                return None;
            }
            for (i, rec) in bytes.chunks_exact_mut(geo.record).enumerate() {
                table.add(u32::try_from(first + i).ok()?, rec, geo.sector);
            }
            first += n;
        }
        Some(table)
    }

    /// Everything a table holds, in the order it holds it.
    fn dump(t: &Table) -> (Vec<FileInfo>, Vec<(u32, u32, String)>) {
        (
            t.files.clone(),
            t.names
                .iter()
                .map(|n| (n.parent, n.file, t.name(n).to_string()))
                .collect(),
        )
    }

    fn busy_records() -> Vec<(u32, Vec<u8>)> {
        let mut records = vec![
            (5, dir(5, ".", 0x16)),
            (24, dir(5, "Users", 0x11)),
            (25, dir(24, "Me", 0x10)),
        ];
        // Files in both halves of the MFT, an extension record, a deleted record.
        for n in 26..60u32 {
            let parent = if n % 3 == 0 { 25 } else { 24 };
            records.push((n, file(parent, &format!("file-{n}.txt"), u64::from(n))));
        }
        records.push((
            60,
            record(IN_USE, 29, &[file_name(25, "second-link-to-29", 1)]),
        ));
        records.push((61, record(0, 0, &[file_name(5, "deleted.txt", 1)])));
        records
    }

    #[test]
    fn double_buffered_reader_gives_the_same_records_as_the_sequential_one() {
        for reversed in [false, true] {
            for chunk in [1024, 2048, 4096, 8192, 16384, 1 << 20] {
                let (mut a, geo) = fake_volume_in(busy_records(), reversed);
                let (mut b, _) = fake_volume_in(busy_records(), reversed);
                let want = read_mft_sequential(&mut a, &geo, chunk).unwrap();
                let got = read_mft(&mut b, &geo, chunk, &|| false).unwrap();
                assert_eq!(dump(&got), dump(&want), "reversed={reversed} chunk={chunk}");
                // Highest record that creates a slot is 59 (60 is an extension of 29, 61 is deleted).
                assert_eq!(got.files.len(), 60);
            }
        }
    }

    #[test]
    fn disk_order_follows_the_extents_but_parsing_does_not() {
        let (mut disk, geo) = fake_volume_in(busy_records(), true);
        let (runs, _) = mft_runs(&mut disk, &geo).unwrap();
        // File order: VCN 0 is at cluster 10, VCN 8 at cluster 2.
        assert_eq!(disk_position(&geo, &runs, 0), 10 * 4096);
        assert_eq!(disk_position(&geo, &runs, 8 * 4096), 2 * 4096);
        assert_eq!(disk_position(&geo, &runs, 8 * 4096 + 1024), 2 * 4096 + 1024);
        assert_eq!(disk_position(&geo, &runs, 99 * 4096), u64::MAX);
        let keys: Vec<u64> = (0..16)
            .map(|i| disk_position(&geo, &runs, i * 4096))
            .collect();
        // One cluster (4 records) per chunk, window of 16: the second half is read first.
        let order = check_order(&keys, 16);
        assert_eq!(order[..8], [8, 9, 10, 11, 12, 13, 14, 15]);
    }

    /// Fails every read after the first `ok` ones.
    struct Flaky {
        inner: Memory,
        ok: usize,
    }

    impl Disk for Flaky {
        fn read(&mut self, offset: u64, buf: &mut [u8]) -> bool {
            if self.ok == 0 {
                return false;
            }
            self.ok -= 1;
            self.inner.read(offset, buf)
        }
    }

    #[test]
    fn a_failed_read_or_a_cancel_stops_the_pipeline() {
        let (inner, geo) = fake_volume_in(busy_records(), true);
        // The first read (record 0) works; the chunk reads then fail partway.
        for ok in [1, 2, 3, 5] {
            let (inner2, _) = fake_volume_in(busy_records(), true);
            let mut disk = Flaky { inner: inner2, ok };
            assert!(read_mft(&mut disk, &geo, 4096, &|| false).is_none());
        }
        // Cancelling after a few chunks returns without hanging on the reader thread.
        let mut disk = Memory(inner.0);
        let calls = std::cell::Cell::new(0);
        assert!(
            read_mft(&mut disk, &geo, 4096, &|| {
                calls.set(calls.get() + 1);
                calls.get() > 3
            })
            .is_none()
        );
    }
}
