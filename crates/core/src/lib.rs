//! arke-core: platform-independent directory model, sorting, filtering, search,
//! thumbnail decoding and the background directory service.
//! No UI framework and no direct Windows API calls (only std and notify), with two exceptions:
//! - `archive` loads Windows' own libarchive (archiveint.dll) or runs its tar.exe for RAR and
//!   compressed tars, so RAR support comes from the copy Windows ships instead of a decoder linked
//!   into Arke.
//! - `winfs`, behind `RealFs`: the folder watcher (`ReadDirectoryChangesW`, see there for why not
//!   notify), folder listings and the folder size walk (`FindFirstFileExW` with large fetches), and
//!   telling network drives apart (`GetDriveTypeW`).

// Tests may unwrap / panic.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic,
        reason = "a failing test should stop right there"
    )
)]

// Translations for user-facing text (type names, error messages). The app picks the
// locale with rust_i18n::set_locale.
rust_i18n::i18n!("locales", fallback = "en");

/// Tests: how much a piece of code allocates at most. Counted for the whole process, since memory
/// is often freed on another thread than the one that allocated it (decoders hand rows to worker
/// threads), so it is exact only with no other test running at the same time, as under nextest.
#[cfg(test)]
pub(crate) mod alloc_peak {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicIsize, Ordering};

    static NOW: AtomicIsize = AtomicIsize::new(0);
    static PEAK: AtomicIsize = AtomicIsize::new(0);

    struct Counting;

    #[global_allocator]
    static COUNTING: Counting = Counting;

    fn add(bytes: isize) {
        let now = NOW.fetch_add(bytes, Ordering::Relaxed) + bytes;
        PEAK.fetch_max(now, Ordering::Relaxed);
    }

    // SAFETY: every call goes straight to the system allocator; only the counters are added.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout, passed on.
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                add(layout.size().cast_signed());
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: as above.
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                add(layout.size().cast_signed());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller's pointer and layout, from `alloc` above.
            unsafe { System.dealloc(ptr, layout) };
            add(-layout.size().cast_signed());
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as above.
            let new = unsafe { System.realloc(ptr, layout, new_size) };
            if !new.is_null() {
                add(new_size.cast_signed() - layout.size().cast_signed());
            }
            new
        }
    }

    /// Runs `f` and returns its result with the most bytes allocated at once while it ran, above
    /// what was allocated when it started.
    pub(crate) fn during<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let base = NOW.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let out = f();
        let peak = PEAK.load(Ordering::Relaxed);
        (out, (peak - base).max(0).cast_unsigned())
    }
}
pub mod archive;
pub mod collate;
pub mod compare;
pub mod content;
pub mod dirsize;
pub mod entry;
pub mod exif;
pub mod failure;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod filetype;
pub mod filter;
pub mod folder_handler;
pub mod format;
pub mod frecency;
pub mod fs;
pub mod gzip;
pub mod highlight;
pub mod history;
pub mod index;
pub mod index_query;
pub mod index_service;
pub mod journal;
pub mod link;
pub mod loader;
pub mod markdown;
pub mod model;
pub mod net;
pub mod office;
pub mod path;
pub mod pdf_text;
pub mod rank;
pub mod remote;
pub mod scrub;
pub mod search;
pub mod selection;
pub mod service;
pub mod sort;
pub mod takeover;
pub mod text;
pub mod thumb_cache;
pub mod thumbs;
pub mod timeline;
pub mod tree;
pub mod typeahead;
pub mod verify;
pub mod watches;
#[cfg(windows)]
mod winfs;

pub use entry::{Entry, EntryKind};
pub use fs::{Fs, RealFs};
pub use history::{History, NavKind};
pub use model::{Delta, DeltaOutcome, DirModel};
pub use search::{SearchHit, SearchResults, SearchService};
pub use selection::Selection;
pub use service::{DirService, Gen, IoEvent, ListOptions, ListTiming};
pub use sort::{
    DateSection, DayBuckets, SearchColumn, SearchSort, SortColumn, SortKey, SortSpec,
    sort_search_hits,
};
pub use typeahead::TypeAhead;
