//! Name order as File Explorer has it (#331): Windows' sort keys for the user's language, with
//! numbers compared as numbers and case ignored. Arke's own order (`sort::natural_cmp`) compared
//! everything that isn't ASCII by code point, so names in Japanese, Chinese and other scripts came
//! out in a different order than in File Explorer, most of all with Japanese as the system
//! language.
//!
//! A sort key is bytes that compare the way the names do (`LCMapStringEx` with `LCMAP_SORTKEY`).
//! Comparing two names through Windows on every comparison is five times slower than Arke's order;
//! a key made once per name and compared as bytes makes sorting faster than before. The keys live
//! only in memory, next to the entry ([`crate::entry::Entry::collation`]); they are never written
//! to disk, since Windows updates can change them. A folder's listing makes them on its own
//! thread, spread over the cores for big folders ([`prepare`]); other lists make them when they
//! are sorted by name.
//!
//! Where Windows can't make keys (other systems, or the call fails) the order is
//! `sort::natural_cmp`, as before.

use std::sync::{Arc, OnceLock};

use crate::entry::Entry;

/// Below this many entries keys are made on the calling thread.
const PARALLEL_FROM: usize = 4096;

/// Whether names are ordered by Windows' sort keys; checked once.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| imp::key("a10").is_some())
}

/// The sort key for `name`. A name Windows can't make a key for (which a valid name shouldn't
/// be) gets one that sorts after every real key, in natural order among themselves, so the order
/// stays total.
pub(crate) fn key(name: &str) -> Arc<[u8]> {
    imp::key(name).unwrap_or_else(|| {
        let mut out = vec![0xFF, 0xFF];
        out.extend_from_slice(name.to_lowercase().as_bytes());
        out.into()
    })
}

/// The sort key for `name` in language `locale` (`zh-CN`), for tests that need one language's
/// order whatever the machine's is.
#[cfg(all(test, windows))]
pub(crate) fn key_for(locale: &str, name: &str) -> Option<Arc<[u8]>> {
    imp::key_for(locale, name)
}

/// Makes the keys `entries` don't have yet; on several threads for big lists. Does nothing where
/// names aren't ordered by keys.
pub fn prepare(entries: &[Entry]) {
    if !available() {
        return;
    }
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    if entries.len() < PARALLEL_FROM || threads < 2 {
        for entry in entries {
            entry.collation();
        }
        return;
    }
    let chunk = entries.len().div_ceil(threads.min(8));
    std::thread::scope(|scope| {
        for part in entries.chunks(chunk) {
            scope.spawn(move || {
                for entry in part {
                    entry.collation();
                }
            });
        }
    });
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::Globalization::{
        LCMAP_SORTKEY, LCMapStringEx, NORM_IGNORECASE, SORT_DIGITSASNUMBERS,
    };
    use windows::core::PCWSTR;

    /// What File Explorer's `StrCmpLogicalW` does: case ignored, digits as numbers.
    const FLAGS: u32 = LCMAP_SORTKEY | NORM_IGNORECASE.0 | SORT_DIGITSASNUMBERS.0;

    pub(super) fn key(name: &str) -> Option<std::sync::Arc<[u8]>> {
        key_in(PCWSTR::null(), name)
    }

    /// The key in the given language; tests pin one.
    #[cfg(test)]
    pub(super) fn key_for(locale: &str, name: &str) -> Option<std::sync::Arc<[u8]>> {
        let locale: Vec<u16> = locale.encode_utf16().chain([0]).collect();
        key_in(PCWSTR(locale.as_ptr()), name)
    }

    fn key_in(locale: PCWSTR, name: &str) -> Option<std::sync::Arc<[u8]>> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        if wide.is_empty() {
            return Some(std::sync::Arc::from(&[][..]));
        }
        // With LCMAP_SORTKEY the destination is bytes and its size is counted in bytes. A null
        // locale name is the user's locale (LOCALE_NAME_USER_DEFAULT).
        // SAFETY: size query only; no buffer.
        let need = unsafe { LCMapStringEx(locale, FLAGS, &wide, None, None, None, LPARAM(0)) };
        let need = usize::try_from(need).ok().filter(|&n| n > 0)?;
        // The wrapper passes the slice's length as the size, which the call reads as bytes: `need`
        // units are twice the room it is told about.
        let mut buf = vec![0u16; need];
        // SAFETY: the call writes at most `need` bytes into `buf`, which holds twice that.
        let got = unsafe {
            LCMapStringEx(
                locale,
                FLAGS,
                &wide,
                Some(&mut buf[..]),
                None,
                None,
                LPARAM(0),
            )
        };
        let got = usize::try_from(got).ok().filter(|&n| n > 0)?;
        // SAFETY: the call wrote `got` bytes at the start of `buf`.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got) };
        Some(std::sync::Arc::from(bytes))
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) const fn key(_: &str) -> Option<std::sync::Arc<[u8]>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
    use crate::sort::{SortColumn, SortSpec};

    fn file(name: &str) -> Entry {
        Entry::new(name, EntryKind::File, 0, None, 0)
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<Entry> = names.iter().map(|n| file(n)).collect();
        SortSpec::by(SortColumn::Name, true, false).sort(&mut v);
        v.iter().map(|e| e.name.to_string()).collect()
    }

    /// Arke's name order is File Explorer's: `StrCmpLogicalW`, in the user's language, for names
    /// in several scripts, with numbers, symbols, case and full-width forms (#331).
    #[cfg(windows)]
    #[test]
    fn names_sort_as_file_explorer_sorts_them() {
        use windows::Win32::UI::Shell::StrCmpLogicalW;
        use windows::core::PCWSTR;
        let names = [
            "写真10.jpg",
            "写真2.jpg",
            "ファイル",
            "ふぁいる",
            "ﾌｧｲﾙ",
            "日本語",
            "東京",
            "资料",
            "資料",
            "報告",
            "会議記録",
            "Éclair",
            "eclair",
            "über",
            "Zebra",
            "zebra",
            "_notes",
            "01 notes",
            "#tag8",
            "#tag008",
            "file10",
            "file2",
            "File1",
            "(1) copy",
            "-dash",
            "~tilde",
            "[x]",
            "ａｂｃ",
            "abc",
            "１２",
            "12",
            "αβγ",
            "Ωmega",
            "кириллица",
            "한국어",
            "a b",
            "a_b",
            "a-b",
        ];
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let mut expected: Vec<&str> = names.to_vec();
        expected.sort_by(|a, b| {
            let (wa, wb) = (wide(a), wide(b));
            // SAFETY: NUL-terminated strings that live through the call.
            let r = unsafe { StrCmpLogicalW(PCWSTR(wa.as_ptr()), PCWSTR(wb.as_ptr())) };
            r.cmp(&0).then_with(|| a.cmp(b))
        });
        assert!(available());
        assert_eq!(sorted(&names), expected);
    }

    #[test]
    fn numbers_compare_as_numbers() {
        assert_eq!(
            sorted(&["file10", "file2", "file1"]),
            ["file1", "file2", "file10"]
        );
    }

    /// A name without a Windows key goes after every real one, in a total order.
    #[test]
    fn names_without_a_key_go_last() {
        let mut keys = [key("b"), key("a")];
        keys.sort();
        let lost: Arc<[u8]> = {
            let mut out = vec![0xFF, 0xFF];
            out.extend_from_slice(b"a");
            out.into()
        };
        assert!(keys.iter().all(|k| *k < lost));
    }

    #[test]
    fn big_lists_get_every_key_on_several_threads() {
        let entries: Vec<Entry> = (0..PARALLEL_FROM * 3)
            .map(|i| file(&format!("f{i}")))
            .collect();
        prepare(&entries);
        if available() {
            assert!(entries.iter().all(|e| e.collation.get().is_some()));
        }
    }

    /// Rows of an expanded folder are ordered by their own path, not by the child's name alone.
    #[test]
    fn a_nested_row_gets_a_key_for_its_own_name() {
        let parent = Entry::new("b", EntryKind::Dir, 0, None, 0);
        let child = file("a");
        let _ = child.collation();
        let row = crate::tree::nested(&parent, &child);
        assert_eq!(row.collation(), &*key(&row.name));
    }
}
