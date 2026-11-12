//! Reads the USN journal on NTFS and ReFS (Dev Drive). Read-only: never creates or deletes a
//! journal or changes anything on disk.
//!
//! NTFS records are version 2 (64-bit file IDs), ReFS records version 3 (128-bit). Reads accept
//! both and IDs are always stored as `u128`. A journal that offers neither version (a future file
//! system) is treated as no journal, so Arke falls back to a regular folder watcher instead of
//! silently receiving nothing.
//!
//! Parsing ([`parse_records`]) is pure and tested with fake data; only [`Volume`] opens the
//! volume, which needs admin.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_NAME, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ExtendedFileIdType, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType,
    GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW, OPEN_EXISTING, OpenFileById,
    VOLUME_NAME_DOS,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2,
};
use windows::core::PCWSTR;

pub(crate) const REASON_DATA_OVERWRITE: u32 = 0x1;
pub(crate) const REASON_DATA_EXTEND: u32 = 0x2;
pub(crate) const REASON_DATA_TRUNCATION: u32 = 0x4;
pub(crate) const REASON_FILE_CREATE: u32 = 0x100;
pub(crate) const REASON_FILE_DELETE: u32 = 0x200;
pub(crate) const REASON_RENAME_OLD_NAME: u32 = 0x1000;
pub(crate) const REASON_RENAME_NEW_NAME: u32 = 0x2000;
pub(crate) const REASON_BASIC_INFO_CHANGE: u32 = 0x8000;
pub(crate) const REASON_HARD_LINK_CHANGE: u32 = 0x10000;
pub(crate) const REASON_REPARSE_POINT_CHANGE: u32 = 0x100000;
pub(crate) const ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Changes that affect the index (name, size, time, hidden); others (permissions, compression,
/// encryption...) are ignored.
pub(crate) const INTERESTING: u32 = REASON_DATA_OVERWRITE
    | REASON_DATA_EXTEND
    | REASON_DATA_TRUNCATION
    | REASON_FILE_CREATE
    | REASON_FILE_DELETE
    | REASON_RENAME_OLD_NAME
    | REASON_RENAME_NEW_NAME
    | REASON_BASIC_INFO_CHANGE
    | REASON_HARD_LINK_CHANGE
    | REASON_REPARSE_POINT_CHANGE;

/// One journal record (USN_RECORD_V2 or V3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    /// File ID: 64-bit on NTFS, 128-bit on ReFS.
    pub(crate) frn: u128,
    pub(crate) parent: u128,
    pub(crate) usn: i64,
    pub(crate) reason: u32,
    pub(crate) attributes: u32,
    pub(crate) name: OsString,
}

impl Record {
    pub(crate) const fn is_dir(&self) -> bool {
        self.attributes & ATTRIBUTE_DIRECTORY != 0
    }
}

const V2_HEADER: usize = 60;
const V3_HEADER: usize = 76;
/// Record versions we can parse.
const MIN_MAJOR: u16 = 2;
const MAX_MAJOR: u16 = 3;

/// Parses `FSCTL_READ_USN_JOURNAL` output: the first 8 bytes are the next USN, then records.
/// Stops at anything malformed (length fields aren't trusted). Records other than v2 and v3 are
/// skipped; they shouldn't arrive since reads only request those.
pub(crate) fn parse_records(buf: &[u8]) -> Option<(i64, Vec<Record>)> {
    let next = i64::from_le_bytes(*buf.first_chunk::<8>()?);
    let mut records = Vec::new();
    let mut rest = &buf[8..];
    while rest.len() >= 8 {
        let len = u32::from_le_bytes(*rest.first_chunk::<4>()?) as usize;
        if len < 8 || len > rest.len() || !len.is_multiple_of(8) {
            break;
        }
        let rec = &rest[..len];
        rest = &rest[len..];
        let major = u16::from_le_bytes([rec[4], rec[5]]);
        let u64_at = |o: usize| u64::from_le_bytes(rec[o..o + 8].try_into().unwrap_or([0; 8]));
        let u32_at = |o: usize| u32::from_le_bytes(rec[o..o + 4].try_into().unwrap_or([0; 4]));
        let u16_at = |o: usize| u16::from_le_bytes([rec[o], rec[o + 1]]) as usize;
        let u128_at = |o: usize| u128::from_le_bytes(rec[o..o + 16].try_into().unwrap_or([0; 16]));
        // (frn, parent, usn, reason, attributes, name length, name offset)
        let fields = match major {
            2 if len >= V2_HEADER => (
                u128::from(u64_at(8)),
                u128::from(u64_at(16)),
                u64_at(24),
                u32_at(40),
                u32_at(52),
                u16_at(56),
                u16_at(58),
            ),
            3 if len >= V3_HEADER => (
                u128_at(8),
                u128_at(24),
                u64_at(40),
                u32_at(56),
                u32_at(68),
                u16_at(72),
                u16_at(74),
            ),
            _ => continue,
        };
        let (frn, parent, usn, reason, attributes, name_len, name_off) = fields;
        let Some(name_bytes) = rec.get(name_off..name_off + name_len) else {
            continue;
        };
        if !name_len.is_multiple_of(2) {
            continue;
        }
        let wide: Vec<u16> = name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        records.push(Record {
            frn,
            parent,
            usn: usn.cast_signed(),
            reason,
            attributes,
            name: OsString::from_wide(&wide),
        });
    }
    Some((next, records))
}

/// Journal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JournalData {
    pub(crate) id: u64,
    /// Oldest record still kept.
    pub(crate) first: i64,
    /// USN of the next record.
    pub(crate) next: i64,
}

#[derive(Debug)]
pub(crate) enum ReadError {
    /// The requested records were overwritten, or the journal was deleted or disabled.
    Lost,
    Io(windows::core::Error),
}

/// An open volume (`\\.\C:`).
pub(crate) struct Volume {
    handle: HANDLE,
}

// SAFETY: a HANDLE is just a kernel object reference; DeviceIoControl and OpenFileById may be
// called from any thread.
unsafe impl Send for Volume {}
// SAFETY: as above; only read-only ioctls are issued and no mutable state is shared.
unsafe impl Sync for Volume {}

impl Drop for Volume {
    fn drop(&mut self) {
        // SAFETY: handle is a valid handle owned by this struct and not yet closed.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// `C:\` to `C`; anything else (UNC, subfolders, device paths) is rejected.
pub(crate) fn drive_letter(root: &Path) -> Option<char> {
    let s = root.to_str()?;
    let mut chars = s.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    (letter.is_ascii_uppercase() && chars.as_str() == r":\").then_some(letter)
}

impl Volume {
    pub(crate) const fn handle(&self) -> HANDLE {
        self.handle
    }

    /// Opens the volume with read access only.
    pub(crate) fn open(root: &Path) -> windows::core::Result<Self> {
        Self::open_with(root, GENERIC_READ.0)
    }

    /// Opens the volume with the given access (read-write needs admin).
    fn open_with(root: &Path, access: u32) -> windows::core::Result<Self> {
        let letter = drive_letter(root)
            .ok_or_else(|| windows::core::Error::from_hresult(ERROR_INVALID_NAME.to_hresult()))?;
        let name: Vec<u16> = format!(r"\\.\{letter}:")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: name is a NUL-terminated UTF-16 string valid for the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }?;
        Ok(Self { handle })
    }

    /// Current journal state. Fails without a journal (not NTFS/ReFS, or disabled) or with an
    /// unreadable record version; Arke then uses a regular folder watcher.
    pub(crate) fn query(&self) -> windows::core::Result<JournalData> {
        let mut data = USN_JOURNAL_DATA_V2::default();
        let mut returned = 0u32;
        // SAFETY: the out buffer is a valid USN_JOURNAL_DATA_V2 of the right size; sync call.
        // Older systems fill only the V0 part, and `returned` says how much.
        unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some((&raw mut data).cast()),
                size_of::<USN_JOURNAL_DATA_V2>() as u32,
                Some(&raw mut returned),
                None,
            )
        }?;
        // Only check versions if reported (V1+ struct); older systems returning V0 are always v2.
        let versions = (returned as usize > 56)
            .then_some((data.MinSupportedMajorVersion, data.MaxSupportedMajorVersion));
        if !readable(versions) {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::ERROR_NOT_SUPPORTED.to_hresult(),
            ));
        }
        Ok(JournalData {
            id: data.UsnJournalID,
            first: data.FirstUsn,
            next: data.NextUsn,
        })
    }

    /// Reads one batch from `start` without waiting; empty if there are no new records.
    pub(crate) fn read(
        &self,
        journal: u64,
        start: i64,
        buf: &mut [u8],
    ) -> Result<(i64, Vec<Record>), ReadError> {
        // Accept v2 and v3: NTFS returns v2, ReFS v3.
        let input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: start,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal,
            MinMajorVersion: MIN_MAJOR,
            MaxMajorVersion: MAX_MAJOR,
        };
        let mut returned = 0u32;
        // SAFETY: input is a valid READ_USN_JOURNAL_DATA_V1; the out buffer is valid and writable.
        let result = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_READ_USN_JOURNAL,
                Some((&raw const input).cast()),
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&raw mut returned),
                None,
            )
        };
        if let Err(err) = result {
            let lost = [
                ERROR_JOURNAL_ENTRY_DELETED,
                ERROR_JOURNAL_DELETE_IN_PROGRESS,
                ERROR_JOURNAL_NOT_ACTIVE,
            ]
            .iter()
            .any(|e| err.code() == e.to_hresult());
            return Err(if lost {
                ReadError::Lost
            } else {
                ReadError::Io(err)
            });
        }
        let data = buf.get(..returned as usize).unwrap_or_default();
        parse_records(data).ok_or(ReadError::Lost)
    }

    /// Current full path of a folder from its file ID (opens attributes only, no content).
    /// IDs that fit in 64 bits use the regular ID type, others (ReFS) the 128-bit one.
    pub(crate) fn path_of(&self, frn: u128) -> Option<PathBuf> {
        let id = match u64::try_from(frn) {
            Ok(short) => FILE_ID_DESCRIPTOR {
                dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
                Type: FileIdType,
                Anonymous: FILE_ID_DESCRIPTOR_0 {
                    FileId: short.cast_signed(),
                },
            },
            Err(_) => FILE_ID_DESCRIPTOR {
                dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
                Type: ExtendedFileIdType,
                Anonymous: FILE_ID_DESCRIPTOR_0 {
                    ExtendedFileId: FILE_ID_128 {
                        Identifier: frn.to_le_bytes(),
                    },
                },
            },
        };
        // SAFETY: id is a valid FILE_ID_DESCRIPTOR; self.handle is a valid handle on this volume.
        let handle = unsafe {
            OpenFileById(
                self.handle,
                &raw const id,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        }
        .ok()?;
        let mut buf = vec![0u16; 1024];
        let path = loop {
            // SAFETY: handle is valid; buf is valid and writable for the call.
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    handle,
                    &mut buf,
                    GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
                )
            } as usize;
            if len == 0 {
                break None;
            }
            if len < buf.len() {
                break Some(OsString::from_wide(&buf[..len]));
            }
            if len > 32 * 1024 {
                break None;
            }
            buf.resize(len + 1, 0);
        };
        // SAFETY: handle was opened above and not yet closed.
        let _ = unsafe { CloseHandle(handle) };
        strip_verbatim(&PathBuf::from(path?))
    }
}

/// `\\?\C:\a\b` to `C:\a\b`; other forms (`\\?\UNC\...`, volume GUIDs) are rejected.
pub(crate) fn strip_verbatim(path: &Path) -> Option<PathBuf> {
    let s = path.to_str().map(str::to_owned).or_else(|| {
        // If the name isn't valid UTF-8, check only the prefix.
        let lossy = path.to_string_lossy();
        lossy.starts_with(r"\\?\").then(|| lossy.into_owned())
    })?;
    let rest = s.strip_prefix(r"\\?\")?;
    drive_letter(Path::new(rest.get(..3)?))?;
    // Slice the original OsStr to keep non-UTF-8 names intact.
    let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
        .skip(4)
        .collect();
    Some(PathBuf::from(OsString::from_wide(&wide)))
}

/// Journal size as (max kept, currently used) bytes. Query only, so it works for standard users
/// (no read-write access). `None` without a journal (not NTFS/ReFS, or disabled).
pub fn journal_size(root: &Path) -> Option<(u64, u64)> {
    drive_letter(root)?;
    // Opening the root with attribute access is enough to query; opening `\\.\C:` needs admin.
    let name: Vec<u16> = root
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: name is NUL-terminated; the handle is owned by Volume and closed on drop.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .ok()?;
    let volume = Volume { handle };
    let mut data = USN_JOURNAL_DATA_V2::default();
    let mut returned = 0u32;
    // SAFETY: the out buffer is a valid USN_JOURNAL_DATA_V2 of the right size; sync call.
    unsafe {
        DeviceIoControl(
            volume.handle,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some((&raw mut data).cast()),
            size_of::<USN_JOURNAL_DATA_V2>() as u32,
            Some(&raw mut returned),
            None,
        )
    }
    .ok()?;
    let used = data
        .NextUsn
        .saturating_sub(data.FirstUsn)
        .max(0)
        .cast_unsigned();
    Some((data.MaximumSize, used))
}

/// Sets the journal's max size (`FSCTL_CREATE_USN_JOURNAL`: on an existing journal this only
/// resizes it, records are kept). Needs admin; run by `arke.exe --indexd journal-size` after UAC.
pub(crate) fn set_journal_size(root: &Path, max: u64) -> windows::core::Result<()> {
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::System::Ioctl::{CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL};
    let volume = Volume::open_with(root, GENERIC_READ.0 | GENERIC_WRITE.0)?;
    let input = CREATE_USN_JOURNAL_DATA {
        MaximumSize: max,
        // The system default delta is 1/4 of the max; 1/8 keeps a little more history.
        AllocationDelta: max / 8,
    };
    let mut returned = 0u32;
    // SAFETY: input is a valid CREATE_USN_JOURNAL_DATA; no output; sync call.
    unsafe {
        DeviceIoControl(
            volume.handle,
            FSCTL_CREATE_USN_JOURNAL,
            Some((&raw const input).cast()),
            size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            None,
            0,
            Some(&raw mut returned),
            None,
        )
    }
}

/// Whether the volume's reported record version range (`None`: older system, meaning v2)
/// includes v2 or v3.
fn readable(versions: Option<(u16, u16)>) -> bool {
    versions.is_none_or(|(min, max)| min <= MAX_MAJOR && max >= MIN_MAJOR)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a USN_RECORD_V3 (ReFS, 128-bit IDs).
    fn record_v3_bytes(
        frn: u128,
        parent: u128,
        usn: i64,
        reason: u32,
        attributes: u32,
        name: &str,
    ) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let len = (V3_HEADER + name.len() * 2).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        r[8..24].copy_from_slice(&frn.to_le_bytes());
        r[24..40].copy_from_slice(&parent.to_le_bytes());
        r[40..48].copy_from_slice(&usn.to_le_bytes());
        r[56..60].copy_from_slice(&reason.to_le_bytes());
        r[68..72].copy_from_slice(&attributes.to_le_bytes());
        r[72..74].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        r[74..76].copy_from_slice(&(V3_HEADER as u16).to_le_bytes());
        for (i, u) in name.iter().enumerate() {
            r[V3_HEADER + i * 2..V3_HEADER + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    /// ReFS (Dev Drive) v3 records: 128-bit IDs parse fine; unknown versions are skipped.
    #[test]
    fn parses_refs_v3_records() {
        let big = (7u128 << 64) | 0x2a;
        let mut buf = 900i64.to_le_bytes().to_vec();
        buf.extend(record_v3_bytes(
            big,
            5,
            800,
            REASON_FILE_CREATE,
            ATTRIBUTE_DIRECTORY,
            "src",
        ));
        let mut v4 = record_bytes(1, 2, 3, REASON_FILE_CREATE, 0, "x");
        v4[4..6].copy_from_slice(&4u16.to_le_bytes());
        buf.extend(v4);
        buf.extend(record_bytes(10, 5, 801, REASON_FILE_DELETE, 0, "old.txt"));
        let (next, records) = parse_records(&buf).unwrap();
        assert_eq!(next, 900);
        assert_eq!(records.len(), 2);
        assert_eq!(
            (records[0].frn, records[0].parent, records[0].usn),
            (big, 5, 800)
        );
        assert_eq!(records[0].name, "src");
        assert!(records[0].is_dir());
        assert_eq!(
            (records[1].frn, records[1].name.to_str()),
            (10, Some("old.txt"))
        );
    }

    /// Record versions: v2 and v3 are readable; a volume offering only newer ones counts as
    /// having no journal.
    #[test]
    fn only_journals_with_readable_records_are_used() {
        assert!(readable(None), "not reported by older systems: v2");
        assert!(readable(Some((2, 3))), "NTFS");
        assert!(readable(Some((3, 3))), "ReFS");
        assert!(readable(Some((2, 4))));
        assert!(!readable(Some((4, 4))));
    }

    /// Builds a USN_RECORD_V2.
    pub(crate) fn record_bytes(
        frn: u128,
        parent: u128,
        usn: i64,
        reason: u32,
        attributes: u32,
        name: &str,
    ) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let len = (V2_HEADER + name.len() * 2).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&(frn as u64).to_le_bytes());
        r[16..24].copy_from_slice(&(parent as u64).to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attributes.to_le_bytes());
        r[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&(V2_HEADER as u16).to_le_bytes());
        for (i, u) in name.iter().enumerate() {
            r[V2_HEADER + i * 2..V2_HEADER + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_v2_records_and_skips_others() {
        // The CJK name checks UTF-16 name decoding and length in code units.
        let mut buf = 500i64.to_le_bytes().to_vec();
        buf.extend(record_bytes(
            10,
            5,
            100,
            REASON_FILE_CREATE,
            0x20,
            "報告.docx",
        ));
        // Unknown version: skipped.
        let mut v9 = record_bytes(11, 5, 200, REASON_FILE_CREATE, 0, "x");
        v9[4..6].copy_from_slice(&9u16.to_le_bytes());
        buf.extend(v9);
        buf.extend(record_bytes(
            12,
            5,
            300,
            REASON_RENAME_NEW_NAME,
            ATTRIBUTE_DIRECTORY,
            "dir",
        ));
        let (next, records) = parse_records(&buf).unwrap();
        assert_eq!(next, 500);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "報告.docx");
        assert_eq!(
            (records[0].frn, records[0].parent, records[0].usn),
            (10, 5, 100)
        );
        assert!(!records[0].is_dir());
        assert!(records[1].is_dir());
        assert_eq!(records[1].reason, REASON_RENAME_NEW_NAME);
    }

    #[test]
    fn stops_at_lying_lengths() {
        assert_eq!(parse_records(&[1, 2, 3]), None);
        let mut buf = 7i64.to_le_bytes().to_vec();
        buf.extend(record_bytes(1, 2, 3, REASON_FILE_CREATE, 0, "ok"));
        let mut bad = record_bytes(4, 2, 5, REASON_FILE_CREATE, 0, "bad");
        bad[0..4].copy_from_slice(&10_000u32.to_le_bytes());
        buf.extend(bad);
        let (_, records) = parse_records(&buf).unwrap();
        assert_eq!(
            records.len(),
            1,
            "records longer than the buffer are skipped"
        );
        // Name offset outside the record.
        let mut buf = 7i64.to_le_bytes().to_vec();
        let mut odd = record_bytes(1, 2, 3, REASON_FILE_CREATE, 0, "ok");
        odd[58..60].copy_from_slice(&200u16.to_le_bytes());
        buf.extend(odd);
        assert!(parse_records(&buf).unwrap().1.is_empty());
    }

    /// A standard user can read the C: journal size (no admin; needs a real disk, so ignored).
    #[test]
    #[ignore = "real volume"]
    fn reads_the_journal_size_without_admin() {
        let (max, used) = journal_size(Path::new("C:\\")).expect("C: has a journal");
        assert!(
            used > 0,
            "C: journal max {} MB, used {} MB",
            max >> 20,
            used >> 20
        );
        assert!(max > 0);
    }

    #[test]
    fn only_plain_drive_roots() {
        assert_eq!(drive_letter(Path::new(r"C:\")), Some('C'));
        assert_eq!(drive_letter(Path::new(r"d:\")), Some('D'));
        assert_eq!(drive_letter(Path::new(r"C:\Users")), None);
        assert_eq!(drive_letter(Path::new(r"\\server\share\")), None);
        assert_eq!(drive_letter(Path::new(r"\\.\C:")), None);
        assert_eq!(drive_letter(Path::new("C:")), None);
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\me")),
            Some(PathBuf::from(r"C:\Users\me"))
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\")),
            Some(PathBuf::from(r"C:\"))
        );
        assert_eq!(strip_verbatim(Path::new(r"\\?\UNC\server\share")), None);
        assert_eq!(strip_verbatim(Path::new(r"\\?\Volume{1234}\x")), None);
        assert_eq!(strip_verbatim(Path::new(r"C:\no-prefix")), None);
    }
}
