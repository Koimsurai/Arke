//! Change journal (the NTFS USN journal): a more reliable change source than watching.
//!
//! `Fs::watch_tree` only works while Arke is running and overflows under load. The USN
//! journal is kept by the file system even while Arke is closed, so we store a journal
//! position ([`Cursor`]) with the index before building it, replay what changed while we
//! were closed on the next start, and then keep following.
//!
//! Reading the journal needs admin rights, so an optional background service
//! (`arke-indexd`) reads it and hands paths to Arke. Without the service, or on non-NTFS
//! volumes, we fall back to watching.

use std::io;
use std::path::{Path, PathBuf};

/// Position in the journal. `journal` changes when the journal is deleted and recreated,
/// which invalidates old positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Journal ID (UsnJournalID).
    pub journal: u64,
    /// Position of the next record (USN).
    pub usn: i64,
}

/// A path that may have changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Absolute path of the changed item.
    pub path: PathBuf,
    /// A folder moved in (or renamed): its contents need relisting too.
    pub moved_in: bool,
}

/// Event delivered to a [`Journal::follow`] callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalEvent {
    /// These paths may have changed; `cursor` is the position after this batch.
    Changed {
        /// Changed paths in journal order (may repeat a path).
        changes: Vec<Change>,
        /// Position to resume from once this batch is applied.
        cursor: Cursor,
    },
    /// Everything from while we were closed has been sent; live changes from here on.
    CaughtUp,
    /// Gap in the journal (cleared, wrapped, service stopped): rebuild the index.
    Lost,
}

/// Callback for [`Journal::follow`]; runs on a background thread.
pub type JournalCallback = Box<dyn Fn(JournalEvent) + Send + Sync>;

/// [`Listed::parent`] value for the root.
pub const LISTED_ROOT: u32 = u32::MAX;

/// One item of a whole-volume listing (the service reads the NTFS MFT).
/// Parents always come before their children, and siblings are contiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Index of the parent in the listing; [`LISTED_ROOT`] for the root.
    pub parent: u32,
    /// File name only (no path).
    pub name: String,
    /// File attributes (folders have `FILE_ATTRIBUTE_DIRECTORY`).
    pub attributes: u32,
    /// File size in bytes (0 for folders).
    pub size: u64,
    /// Modified time in Unix seconds, 0 if unknown.
    pub mtime: u32,
    /// Symlink, junction or mount point.
    pub link: bool,
}

impl Listed {
    /// Converts to an [`Entry`](crate::entry::Entry); `mtime` 0 becomes no modified time.
    pub fn to_entry(&self) -> crate::entry::Entry {
        use crate::entry::{Entry, EntryKind};
        let is_dir = self.attributes & 0x10 != 0;
        let kind = if self.link {
            EntryKind::Link { is_dir }
        } else if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let modified = (self.mtime != 0)
            .then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(self.mtime)));
        Entry::new(
            self.name.as_str(),
            kind,
            self.size,
            modified,
            self.attributes,
        )
    }
}

/// Receives one batch of the listing; returning false stops (cancel).
pub type ListSink<'a> = &'a mut dyn FnMut(Vec<Listed>) -> bool;

/// Source of change-journal positions, change streams and MFT listings for a volume root.
pub trait Journal: Send + Sync + 'static {
    /// Current journal position for this root; `None` if unsupported (no service, not NTFS).
    /// Connects to the service, so call it on a background thread.
    fn cursor(&self, root: &Path) -> Option<Cursor>;
    /// Sends changes from `from` on, then keeps sending new ones. Stops when the return value is
    /// dropped.
    fn follow(
        &self,
        root: &Path,
        from: Cursor,
        on_event: JournalCallback,
    ) -> io::Result<Box<dyn Send>>;
    /// Lists the whole root at once from the NTFS MFT (much faster than walking folders),
    /// in batches to `sink`. Errors if unsupported (no service, not NTFS, old service) and the
    /// caller walks folders instead. Connects to the service; call on a background thread.
    fn list(&self, root: &Path, sink: ListSink) -> io::Result<()> {
        let _ = (root, sink);
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeJournal;

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use super::{Change, Cursor, Journal, JournalCallback, JournalEvent, ListSink, Listed};
    use parking_lot::Mutex;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// Fake journal for tests: push changes by hand; `follow` replays everything after `from`,
    /// then forwards new ones live.
    #[derive(Default)]
    pub struct FakeJournal {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        journal: u64,
        /// (position, change)
        log: Vec<(i64, Change)>,
        next: i64,
        /// Oldest position still kept (earlier ones were overwritten).
        lowest: i64,
        followers: Vec<(PathBuf, Arc<JournalCallback>)>,
        follow_calls: usize,
        /// Listings returned by [`Journal::list`] (none means unsupported).
        listings: Vec<(PathBuf, Vec<Listed>)>,
        list_calls: usize,
    }

    impl FakeJournal {
        /// Creates an empty journal with ID `journal`; the first record gets USN 1.
        pub fn new(journal: u64) -> Arc<Self> {
            let this = Self::default();
            {
                let mut s = this.state.lock();
                s.journal = journal;
                s.next = 1;
                s.lowest = 1;
            }
            Arc::new(this)
        }

        /// Records a change (current followers get it immediately).
        pub fn push(&self, path: impl Into<PathBuf>, moved_in: bool) {
            let change = Change {
                path: path.into(),
                moved_in,
            };
            let (followers, cursor) = {
                let mut s = self.state.lock();
                let usn = s.next;
                s.next += 1;
                s.log.push((usn, change.clone()));
                let cursor = Cursor {
                    journal: s.journal,
                    usn: s.next,
                };
                (s.followers.clone(), cursor)
            };
            for (root, cb) in followers {
                if change.path.starts_with(&root) {
                    cb(JournalEvent::Changed {
                        changes: vec![change.clone()],
                        cursor,
                    });
                }
            }
        }

        /// All old records overwritten (journal wrapped): following can only start from now.
        pub fn forget_all(&self) {
            let mut s = self.state.lock();
            s.log.clear();
            s.lowest = s.next;
        }

        /// Journal deleted and recreated (new ID).
        pub fn recreate(&self, journal: u64) {
            let followers = {
                let mut s = self.state.lock();
                s.journal = journal;
                s.log.clear();
                s.lowest = s.next;
                std::mem::take(&mut s.followers)
            };
            for (_, cb) in followers {
                cb(JournalEvent::Lost);
            }
        }

        /// Number of [`Journal::follow`] calls so far.
        pub fn follow_calls(&self) -> usize {
            self.state.lock().follow_calls
        }
    }

    impl FakeJournal {
        /// Makes [`Journal::list`] return this listing.
        pub fn set_listing(&self, root: impl Into<PathBuf>, listing: Vec<Listed>) {
            let root = root.into();
            let mut s = self.state.lock();
            s.listings.retain(|(r, _)| *r != root);
            s.listings.push((root, listing));
        }

        /// Number of [`Journal::list`] calls so far.
        pub fn list_calls(&self) -> usize {
            self.state.lock().list_calls
        }
    }

    impl Journal for FakeJournal {
        fn list(&self, root: &Path, sink: ListSink) -> io::Result<()> {
            let listing = {
                let mut s = self.state.lock();
                s.list_calls += 1;
                s.listings
                    .iter()
                    .find(|(r, _)| r == root)
                    .map(|(_, l)| l.clone())
            };
            let listing = listing.ok_or(io::ErrorKind::Unsupported)?;
            // Batches of 3 to exercise the cross-batch case.
            for batch in listing.chunks(3) {
                if !sink(batch.to_vec()) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
            }
            Ok(())
        }

        fn cursor(&self, _root: &Path) -> Option<Cursor> {
            let s = self.state.lock();
            Some(Cursor {
                journal: s.journal,
                usn: s.next,
            })
        }

        fn follow(
            &self,
            root: &Path,
            from: Cursor,
            on_event: JournalCallback,
        ) -> io::Result<Box<dyn Send>> {
            let cb = Arc::new(on_event);
            let (backlog, cursor) = {
                let mut s = self.state.lock();
                s.follow_calls += 1;
                if from.journal != s.journal || from.usn < s.lowest {
                    drop(s);
                    cb(JournalEvent::Lost);
                    return Ok(Box::new(()));
                }
                let backlog: Vec<Change> = s
                    .log
                    .iter()
                    .filter(|(usn, c)| *usn >= from.usn && c.path.starts_with(root))
                    .map(|(_, c)| c.clone())
                    .collect();
                s.followers.push((root.to_path_buf(), cb.clone()));
                let cursor = Cursor {
                    journal: s.journal,
                    usn: s.next,
                };
                drop(s);
                (backlog, cursor)
            };
            if !backlog.is_empty() {
                cb(JournalEvent::Changed {
                    changes: backlog,
                    cursor,
                });
            }
            cb(JournalEvent::CaughtUp);
            Ok(Box::new(()))
        }
    }
}
