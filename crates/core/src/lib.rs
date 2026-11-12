//! arke-core: platform-independent directory model, sorting, filtering, search,
//! thumbnail decoding and the background directory service.
//! No UI framework and no direct Windows API calls (only std and notify), with one exception:
//! `archive` loads Windows' own libarchive (archiveint.dll) or runs its tar.exe for RAR and
//! compressed tars, so RAR support comes from the copy Windows ships instead of a decoder linked
//! into Arke.

// Tests may unwrap / panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

// Translations for user-facing text (type names, error messages). The app picks the
// locale with rust_i18n::set_locale.
rust_i18n::i18n!("locales", fallback = "en");

pub mod archive;
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
pub mod highlight;
pub mod history;
pub mod index;
pub mod index_query;
pub mod index_service;
pub mod journal;
pub mod link;
pub mod loader;
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
pub mod thumbs;
pub mod tree;
pub mod typeahead;
pub mod verify;
pub mod watches;

pub use entry::{Entry, EntryKind};
pub use fs::{Fs, RealFs};
pub use history::{History, NavKind};
pub use model::{Delta, DeltaOutcome, DirModel};
pub use search::{SearchHit, SearchResults, SearchService};
pub use selection::Selection;
pub use service::{DirService, Gen, IoEvent, ListOptions, ListTiming};
pub use sort::{SortColumn, SortKey, SortSpec};
pub use typeahead::TypeAhead;
