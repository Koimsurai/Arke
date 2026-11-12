//! Directory entries ([`Entry`]) and the process-wide rules for which ones count as hidden.

use std::sync::Arc;
use std::time::SystemTime;

/// One directory entry. Names are `Arc<str>` so cloning the whole list only bumps refcounts.
#[derive(Clone, Debug)]
pub struct Entry {
    /// File name only, as listed (original case).
    pub name: Arc<str>,
    /// Lowercase name for sorting and type-ahead, computed on the background thread.
    pub sort_key: Arc<str>,
    /// File, folder or link.
    pub kind: EntryKind,
    /// Size in bytes; 0 for folders and links.
    pub size: u64,
    /// Last write time; `None` if unknown.
    pub modified: Option<SystemTime>,
    /// Creation time. Comes with the listing; other sources (search results) may not have it.
    pub created: Option<SystemTime>,
    /// Windows `FILE_ATTRIBUTE_*` bits; 0 on other platforms.
    pub attributes: u32,
}

/// What an [`Entry`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    /// Regular folder.
    Dir,
    /// Regular file (anything that is not a folder or link).
    File,
    /// Symlink or junction (a Windows reparse point).
    Link {
        /// The link target is a folder (the reparse point has the directory attribute).
        is_dir: bool,
    },
}

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Process-wide hiding rules from settings: whether dot-prefixed names count as hidden,
/// and whether protected system files are shown.
static DOT_HIDDEN: AtomicBool = AtomicBool::new(false);
static SHOW_PROTECTED: AtomicBool = AtomicBool::new(false);
/// Bumped whenever the rules change; cached listings filtered under old rules are stale.
static HIDDEN_RULES: AtomicU32 = AtomicU32::new(0);

/// Changes the hiding rules. Returns whether they changed (open folders need a relist).
pub fn set_hidden_rules(dot_hidden: bool, show_protected: bool) -> bool {
    let changed = (DOT_HIDDEN.swap(dot_hidden, Ordering::Relaxed) != dot_hidden)
        | (SHOW_PROTECTED.swap(show_protected, Ordering::Relaxed) != show_protected);
    if changed {
        HIDDEN_RULES.fetch_add(1, Ordering::Relaxed);
    }
    changed
}

/// Current version of the hiding rules.
pub fn hidden_rules_version() -> u32 {
    HIDDEN_RULES.load(Ordering::Relaxed)
}

/// Hidden attribute bit.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
/// System attribute bit (with Hidden: a protected system file).
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
/// Data is not local (legacy offline files).
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
/// Opening the file recalls it from the cloud.
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
/// Reading the data recalls it from the cloud (OneDrive "online-only").
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
/// Cloud file pinned with "Always keep on this device".
const FILE_ATTRIBUTE_PINNED: u32 = 0x8_0000;
/// Has a reparse point. Cloud Files placeholders are reparse points with a cloud tag, so a plain
/// file without one inside a sync root hasn't been taken over by the sync client yet.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// What the sync client reports beyond the attributes (read per item from the Cloud Files API).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloudSync {
    /// A placeholder the sync client hasn't marked in sync: uploading, downloading or waiting.
    Syncing,
    /// A plain file the sync client hasn't converted to a placeholder: not synced yet, or held
    /// back by an error (name not allowed, too large, ...).
    NotSynced,
}

/// Cloud state of one item, as shown after its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloudStatus {
    /// Content isn't local (online-only).
    OnlineOnly,
    /// Available on this device.
    Local,
    /// "Always keep on this device".
    Pinned,
    /// Transfer pending or running.
    Syncing,
    /// Not synced (or a sync error).
    NotSynced,
}

/// Sync states for one folder's items from attributes alone, before any per-item reads: plain
/// files (no reparse point) count as not synced. Only meaningful inside a Cloud Files sync root,
/// and only when the folder holds at least one placeholder; a folder without any is either fully
/// outside the client's control or a provider without placeholders, and flagging every item there
/// would be noise. System files (`desktop.ini`) are left alone.
pub fn unsynced_by_attributes(entries: &[Entry]) -> Vec<Arc<str>> {
    let placeholder = |e: &Entry| e.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if !entries.iter().any(placeholder) {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|e| !placeholder(e) && !e.is_system() && !matches!(e.kind, EntryKind::Link { .. }))
        .map(|e| e.name.clone())
        .collect()
}

impl Entry {
    /// Creates an entry with no creation time; also computes the lowercase `sort_key`.
    pub fn new(
        name: impl Into<Arc<str>>,
        kind: EntryKind,
        size: u64,
        modified: Option<SystemTime>,
        attributes: u32,
    ) -> Self {
        let name: Arc<str> = name.into();
        let sort_key: Arc<str> = if name.chars().any(char::is_uppercase) {
            name.to_lowercase().into()
        } else {
            name.clone()
        };
        Self {
            name,
            sort_key,
            kind,
            size,
            modified,
            created: None,
            attributes,
        }
    }

    /// Sets the creation time.
    #[must_use]
    pub const fn with_created(mut self, created: Option<SystemTime>) -> Self {
        self.created = created;
        self
    }

    /// Builds from a `std::fs::DirEntry`. On Windows `DirEntry::metadata()` reuses the
    /// FindNextFileW data, so there is no extra syscall.
    pub fn from_dir_entry(entry: &std::fs::DirEntry) -> Option<Self> {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let meta = entry.metadata().ok()?;
        Some(Self::from_metadata(&name, &meta))
    }

    /// Builds from metadata read without following links (`symlink_metadata`).
    pub(crate) fn from_metadata(name: &str, meta: &std::fs::Metadata) -> Self {
        let attributes = attributes_of(meta);
        let ft = meta.file_type();
        let kind = if ft.is_symlink() {
            EntryKind::Link {
                is_dir: is_dir_link(meta),
            }
        } else if ft.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let size = if matches!(kind, EntryKind::File) {
            meta.len()
        } else {
            0
        };
        Self::new(name, kind, size, meta.modified().ok(), attributes)
            .with_created(meta.created().ok())
    }

    /// Folder, or a link to a folder: something to navigate into.
    pub const fn is_dir_like(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Link { is_dir: true })
    }

    /// Has the Hidden attribute (ignores the dot-prefix rule; see [`Self::concealed`]).
    pub const fn is_hidden(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_HIDDEN != 0
    }

    /// Whether this entry is hidden under the current rules (see [`set_hidden_rules`]).
    pub fn concealed(&self, show_hidden: bool) -> bool {
        self.concealed_with(
            show_hidden,
            DOT_HIDDEN.load(Ordering::Relaxed),
            SHOW_PROTECTED.load(Ordering::Relaxed),
        )
    }

    /// Protected system files (both Hidden and System, e.g. `pagefile.sys`, `$Recycle.Bin`)
    /// stay hidden even with "show hidden items" on unless `show_protected`, as in Explorer.
    /// Other hidden items, and dot-prefixed names when `dot_hidden`, show only with `show_hidden`.
    pub fn concealed_with(
        &self,
        show_hidden: bool,
        dot_hidden: bool,
        show_protected: bool,
    ) -> bool {
        if self.is_hidden() && self.is_system() && !show_protected {
            return true;
        }
        !show_hidden && (self.is_hidden() || (dot_hidden && self.name.starts_with('.')))
    }

    /// Has the System attribute.
    pub(crate) const fn is_system(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_SYSTEM != 0
    }

    /// Cloud placeholder (OneDrive Files On-Demand etc.): content is not local and
    /// **reading it triggers a download**. Thumbnails, system icons and previews must not
    /// touch it; only attributes from the listing are safe.
    pub const fn is_cloud_placeholder(&self) -> bool {
        self.attributes
            & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_OFFLINE)
            != 0
    }

    /// Cloud file pinned with "Always keep on this device" (attributes only).
    pub const fn is_cloud_pinned(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_PINNED != 0
    }

    /// Whether this item might have a sync state worth reading per item: a local Cloud Files
    /// placeholder (reparse point, content on disk). Online-only items never get a handle opened,
    /// and system files are skipped.
    pub const fn wants_sync_check(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !self.is_cloud_placeholder()
            && !self.is_system()
    }

    /// Cloud state shown after the name. `in_cloud`: the folder is inside a sync root; `sync`:
    /// what the sync client reported for this item, if anything. Online-only wins over everything
    /// (opening it would download); outside sync roots only online-only placeholders are marked.
    pub const fn cloud_status(
        &self,
        in_cloud: bool,
        sync: Option<CloudSync>,
    ) -> Option<CloudStatus> {
        if self.is_cloud_placeholder() {
            return Some(CloudStatus::OnlineOnly);
        }
        if !in_cloud {
            return None;
        }
        Some(match sync {
            Some(CloudSync::Syncing) => CloudStatus::Syncing,
            Some(CloudSync::NotSynced) => CloudStatus::NotSynced,
            None if self.is_cloud_pinned() => CloudStatus::Pinned,
            None => CloudStatus::Local,
        })
    }

    /// The Read-only, Hidden, System and Archive attributes as letters ("RHSA" order, like the
    /// Attributes column in File Explorer); empty when none is set.
    pub const fn attribute_letters(&self) -> &'static str {
        attribute_letters(self.attributes)
    }

    /// Whether this is a symlink or junction.
    pub const fn is_link(&self) -> bool {
        matches!(self.kind, EntryKind::Link { .. })
    }

    /// Lowercase extension without the dot. Empty for folders. Only the file name counts, so a
    /// nested tree row (`photos.2024\readme`, see [`crate::tree`]) has none.
    pub fn extension(&self) -> &str {
        if self.is_dir_like() {
            return "";
        }
        let name = crate::tree::leaf(&self.sort_key);
        match name.rfind('.') {
            Some(0) | None => "",
            Some(ix) => &name[ix + 1..],
        }
    }
}

/// Read-only attribute bit.
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
/// Archive attribute bit.
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;

/// Letters for every combination of R, H, S and A, indexed by those bits in that order.
const ATTRIBUTE_LETTERS: [&str; 16] = [
    "", "R", "H", "RH", "S", "RS", "HS", "RHS", "A", "RA", "HA", "RHA", "SA", "RSA", "HSA", "RHSA",
];

/// See [`Entry::attribute_letters`]. A table lookup, so sorting by it allocates nothing.
pub const fn attribute_letters(attributes: u32) -> &'static str {
    let mut ix = 0;
    if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        ix |= 1;
    }
    if attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
        ix |= 2;
    }
    if attributes & FILE_ATTRIBUTE_SYSTEM != 0 {
        ix |= 4;
    }
    if attributes & FILE_ATTRIBUTE_ARCHIVE != 0 {
        ix |= 8;
    }
    ATTRIBUTE_LETTERS[ix]
}

#[cfg(windows)]
fn attributes_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

#[cfg(not(windows))]
fn attributes_of(_meta: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(windows)]
fn is_dir_link(meta: &std::fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    attributes_of(meta) & FILE_ATTRIBUTE_DIRECTORY != 0
}

#[cfg(not(windows))]
fn is_dir_link(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_is_lowercase_and_skips_dotfiles() {
        let e = Entry::new("Photo.JPG", EntryKind::File, 1, None, 0);
        assert_eq!(e.extension(), "jpg");
        let e = Entry::new(".gitignore", EntryKind::File, 1, None, 0);
        assert_eq!(e.extension(), "");
        let e = Entry::new("a.b", EntryKind::Dir, 0, None, 0);
        assert_eq!(e.extension(), "");
        let sep = crate::tree::SEP;
        let e = Entry::new(
            format!("photos.2024{sep}readme"),
            EntryKind::File,
            1,
            None,
            0,
        );
        assert_eq!(e.extension(), "", "the folder's dot isn't the file's");
        let e = Entry::new(format!("a{sep}.env"), EntryKind::File, 1, None, 0);
        assert_eq!(e.extension(), "");
        let e = Entry::new(format!("a.b{sep}Notes.TXT"), EntryKind::File, 1, None, 0);
        assert_eq!(e.extension(), "txt");
    }

    #[test]
    fn attribute_letters_follow_explorer_order() {
        assert_eq!(attribute_letters(0), "");
        assert_eq!(attribute_letters(FILE_ATTRIBUTE_ARCHIVE), "A");
        assert_eq!(attribute_letters(0x1 | 0x20), "RA");
        assert_eq!(attribute_letters(0x2 | 0x4 | 0x20), "HSA");
        assert_eq!(attribute_letters(0x1 | 0x2 | 0x4 | 0x20 | 0x400), "RHSA");
        // Other bits (directory, reparse point) don't show.
        assert_eq!(attribute_letters(0x10 | 0x400), "");
    }

    #[test]
    fn hidden_rules() {
        let plain = Entry::new("a.txt", EntryKind::File, 1, None, 0);
        let hidden = Entry::new("b.txt", EntryKind::File, 1, None, FILE_ATTRIBUTE_HIDDEN);
        let protected = Entry::new(
            "pagefile.sys",
            EntryKind::File,
            1,
            None,
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM,
        );
        let system_only = Entry::new("s.dll", EntryKind::File, 1, None, FILE_ATTRIBUTE_SYSTEM);
        let dot = Entry::new(".git", EntryKind::Dir, 0, None, 0);
        // Args: (show hidden, dot counts as hidden, show protected).
        // Default: only items with the Hidden attribute are hidden.
        assert!(!plain.concealed_with(false, false, false));
        assert!(hidden.concealed_with(false, false, false));
        assert!(protected.concealed_with(false, false, false));
        assert!(!dot.concealed_with(false, false, false));
        // System without Hidden is not a protected system file.
        assert!(!system_only.concealed_with(false, false, false));
        // Show hidden: plain hidden items appear, protected system files stay hidden.
        assert!(!hidden.concealed_with(true, false, false));
        assert!(protected.concealed_with(true, false, false));
        // Protected files need their own switch, and still need "show hidden".
        assert!(!protected.concealed_with(true, false, true));
        assert!(protected.concealed_with(false, false, true));
        // Dot-hidden follows "show hidden".
        assert!(dot.concealed_with(false, true, false));
        assert!(!dot.concealed_with(true, true, false));
        assert!(!plain.concealed_with(false, true, false));
    }

    #[test]
    fn cloud_status_priority() {
        let file = |attrs| Entry::new("a.txt", EntryKind::File, 1, None, attrs);
        let online = file(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS);
        let local = file(FILE_ATTRIBUTE_REPARSE_POINT);
        let pinned = file(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_PINNED);
        // Online-only shows anywhere and beats any reported state.
        assert_eq!(
            online.cloud_status(false, None),
            Some(CloudStatus::OnlineOnly)
        );
        assert_eq!(
            online.cloud_status(true, Some(CloudSync::Syncing)),
            Some(CloudStatus::OnlineOnly)
        );
        // Outside sync roots nothing else is marked.
        assert_eq!(local.cloud_status(false, Some(CloudSync::Syncing)), None);
        assert_eq!(local.cloud_status(true, None), Some(CloudStatus::Local));
        assert_eq!(pinned.cloud_status(true, None), Some(CloudStatus::Pinned));
        // A reported state beats the pinned attribute.
        assert_eq!(
            pinned.cloud_status(true, Some(CloudSync::Syncing)),
            Some(CloudStatus::Syncing)
        );
        assert_eq!(
            local.cloud_status(true, Some(CloudSync::NotSynced)),
            Some(CloudStatus::NotSynced)
        );
    }

    #[test]
    fn sync_checks_only_local_placeholders() {
        let file = |attrs| Entry::new("a.txt", EntryKind::File, 1, None, attrs);
        assert!(file(FILE_ATTRIBUTE_REPARSE_POINT).wants_sync_check());
        assert!(!file(0).wants_sync_check());
        assert!(
            !file(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                .wants_sync_check(),
            "online-only items are never opened"
        );
        assert!(!file(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_SYSTEM).wants_sync_check());
    }

    #[test]
    fn plain_files_next_to_placeholders_are_not_synced() {
        let entry = |name: &str, attrs| Entry::new(name, EntryKind::File, 1, None, attrs);
        let synced = entry("synced.txt", FILE_ATTRIBUTE_REPARSE_POINT);
        let fresh = entry("fresh.txt", 0);
        let ini = entry("desktop.ini", FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
        let names = unsynced_by_attributes(&[synced.clone(), fresh.clone(), ini.clone()]);
        assert_eq!(names, vec![Arc::<str>::from("fresh.txt")]);
        // No placeholder at all: nothing is flagged.
        assert!(unsynced_by_attributes(&[fresh, ini]).is_empty());
        assert!(unsynced_by_attributes(&[synced]).is_empty());
    }
}
