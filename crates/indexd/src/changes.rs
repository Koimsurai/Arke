//! Journal records to paths. A record only carries the parent folder's FRN and a name, so the
//! parent's current path has to be looked up.
//!
//! Also filters out other people's data: changes inside other users' profiles
//! (`C:\Users\<other>`) are not sent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use arke_core::journal::Change;

use crate::usn::{
    INTERESTING, REASON_FILE_DELETE, REASON_RENAME_NEW_NAME, REASON_RENAME_OLD_NAME, Record,
};

/// Folder FRN to its current path.
pub(crate) trait Resolve {
    fn dir_path(&mut self, frn: u128) -> Option<PathBuf>;
    /// A folder was renamed or deleted, so earlier lookups may be stale.
    fn forget(&mut self);
}

/// Cached lookup. Cleared whenever a folder is renamed or deleted, since paths below it changed.
pub(crate) struct Cached<F> {
    lookup: F,
    cache: HashMap<u128, Option<PathBuf>>,
}

const CACHE_MAX: usize = 50_000;

impl<F: FnMut(u128) -> Option<PathBuf>> Cached<F> {
    pub(crate) fn new(lookup: F) -> Self {
        Self {
            lookup,
            cache: HashMap::new(),
        }
    }
}

impl<F: FnMut(u128) -> Option<PathBuf>> Resolve for Cached<F> {
    fn dir_path(&mut self, frn: u128) -> Option<PathBuf> {
        if let Some(hit) = self.cache.get(&frn) {
            return hit.clone();
        }
        if self.cache.len() >= CACHE_MAX {
            self.cache.clear();
        }
        let path = (self.lookup)(frn);
        self.cache.insert(frn, path.clone());
        path
    }

    fn forget(&mut self) {
        self.cache.clear();
    }
}

/// Limits what is sent to what the connected user may see.
pub(crate) struct Privacy {
    /// Where all profiles live (`C:\Users`).
    pub(crate) profiles: Option<PathBuf>,
    /// The connected user's own profile.
    pub(crate) mine: Option<PathBuf>,
}

impl Privacy {
    pub(crate) fn allows(&self, path: &Path) -> bool {
        let Some(profiles) = &self.profiles else {
            return true;
        };
        let Ok(rest) = path.strip_prefix(profiles) else {
            return true;
        };
        let mut parts = rest.components();
        let Some(owner) = parts.next() else {
            return true;
        };
        // `C:\Users\Bob` itself (the folder name) is fine; anything inside is not.
        if parts.next().is_none() {
            return true;
        }
        let owner = owner.as_os_str().to_string_lossy().to_lowercase();
        if owner == "public" {
            return true;
        }
        self.mine.as_ref().is_some_and(|mine| {
            mine.file_name()
                .is_some_and(|m| m.to_string_lossy().to_lowercase() == owner)
                && mine
                    .parent()
                    .is_some_and(|p| p.as_os_str().eq_ignore_ascii_case(profiles.as_os_str()))
        })
    }
}

/// A batch of records to deduplicated changed paths.
pub(crate) fn to_changes(
    records: &[Record],
    resolve: &mut impl Resolve,
    privacy: &Privacy,
) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    for rec in records {
        if rec.reason & INTERESTING == 0 {
            continue;
        }
        let Some(parent) = resolve.dir_path(rec.parent) else {
            continue;
        };
        let path = parent.join(&rec.name);
        let moved_in = rec.is_dir() && rec.reason & REASON_RENAME_NEW_NAME != 0;
        if rec.is_dir() && rec.reason & (REASON_RENAME_OLD_NAME | REASON_FILE_DELETE) != 0 {
            resolve.forget();
        }
        if !privacy.allows(&path) {
            continue;
        }
        if let Some(&i) = index.get(&path) {
            out[i].moved_in |= moved_in;
        } else {
            index.insert(path.clone(), out.len());
            out.push(Change { path, moved_in });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::{ATTRIBUTE_DIRECTORY, REASON_DATA_EXTEND, REASON_FILE_CREATE};
    use std::ffi::OsString;

    fn rec(frn: u128, parent: u128, reason: u32, dir: bool, name: &str) -> Record {
        Record {
            frn,
            parent,
            usn: 0,
            reason,
            attributes: if dir { ATTRIBUTE_DIRECTORY } else { 0x20 },
            name: OsString::from(name),
        }
    }

    fn privacy() -> Privacy {
        Privacy {
            profiles: Some(PathBuf::from(r"C:\Users")),
            mine: Some(PathBuf::from(r"C:\Users\Me")),
        }
    }

    #[test]
    fn resolves_dedupes_and_marks_moved_folders() {
        let dirs: HashMap<u128, PathBuf> = [
            (5, PathBuf::from(r"C:\Users\Me\Docs")),
            (6, PathBuf::from(r"C:\")),
        ]
        .into();
        let mut lookups = 0;
        let mut resolve = Cached::new(|frn| {
            lookups += 1;
            dirs.get(&frn).cloned()
        });
        let records = [
            rec(10, 5, REASON_FILE_CREATE, false, "a.txt"),
            rec(10, 5, REASON_DATA_EXTEND, false, "a.txt"),
            // Permission-only change: ignored.
            rec(11, 5, 0x800, false, "secret.txt"),
            // Parent not found (already deleted): skipped.
            rec(12, 99, REASON_FILE_CREATE, false, "gone.txt"),
            rec(13, 6, REASON_RENAME_NEW_NAME, true, "Moved"),
            rec(14, 6, REASON_RENAME_NEW_NAME, false, "file-rename.txt"),
        ];
        let changes = to_changes(&records, &mut resolve, &privacy());
        assert_eq!(
            changes,
            [
                Change {
                    path: PathBuf::from(r"C:\Users\Me\Docs\a.txt"),
                    moved_in: false
                },
                Change {
                    path: PathBuf::from(r"C:\Moved"),
                    moved_in: true
                },
                Change {
                    path: PathBuf::from(r"C:\file-rename.txt"),
                    moved_in: false
                },
            ]
        );
        drop(resolve);
        assert_eq!(lookups, 3, "each folder is looked up once");
    }

    #[test]
    fn forgets_paths_after_a_folder_is_renamed() {
        struct Counting(usize);
        impl Resolve for Counting {
            fn dir_path(&mut self, _: u128) -> Option<PathBuf> {
                Some(PathBuf::from(r"C:\x"))
            }
            fn forget(&mut self) {
                self.0 += 1;
            }
        }
        let mut resolve = Counting(0);
        let records = [
            rec(1, 2, REASON_RENAME_OLD_NAME, true, "old"),
            rec(3, 2, REASON_FILE_DELETE, false, "file"),
            rec(4, 2, REASON_FILE_DELETE, true, "dir"),
        ];
        to_changes(&records, &mut resolve, &privacy());
        assert_eq!(
            resolve.0, 2,
            "only folder renames and deletes clear the cache"
        );
    }

    #[test]
    fn hides_other_peoples_profiles() {
        let p = privacy();
        assert!(p.allows(Path::new(r"C:\Users\Me\Desktop\a.txt")));
        assert!(
            p.allows(Path::new(r"C:\Users\me\Desktop\a.txt")),
            "case-insensitive"
        );
        assert!(p.allows(Path::new(r"C:\Users\Public\Music\x.mp3")));
        assert!(
            p.allows(Path::new(r"C:\Users\Bob")),
            "the profile folder itself is allowed"
        );
        assert!(!p.allows(Path::new(r"C:\Users\Bob\Desktop\diary.txt")));
        assert!(p.allows(Path::new(r"C:\Windows\notepad.exe")));
        assert!(
            p.allows(Path::new(r"D:\Users\Bob\x")),
            "other drives hold no profiles"
        );
        let nobody = Privacy {
            profiles: Some(PathBuf::from(r"C:\Users")),
            mine: None,
        };
        assert!(
            !nobody.allows(Path::new(r"C:\Users\Me\a.txt")),
            "unknown user sees no profiles"
        );
    }
}
