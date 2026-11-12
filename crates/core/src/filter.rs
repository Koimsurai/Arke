//! Live filter for the current folder: nucleo fuzzy match on names, keeping the sort order.
//!
//! Same syntax as fzf / Helix: space-separated terms (all must match), `'exact`, `^prefix`,
//! `suffix$`, `!exclude`. Smart case: a term with uppercase is case-sensitive.
//!
//! 100k entries take ~10-20ms, so callers should run it in the background.

use nucleo::Matcher;
use nucleo::Utf32Str;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};

use crate::entry::Entry;
use crate::index_query::Query;

/// Matching entries in input order. `None` for a blank query, meaning "no filter".
pub fn filter_entries(entries: &[Entry], query: &str) -> Option<Vec<Entry>> {
    if query.trim().is_empty() {
        return None;
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::default();
    let mut buf = Vec::new();
    Some(
        entries
            .iter()
            .filter(|e| {
                pattern
                    .score(Utf32Str::new(&e.name, &mut buf), &mut matcher)
                    .is_some()
            })
            .cloned()
            .collect(),
    )
}

/// Like [`filter_entries`], but with the search box syntax ([`Query`]): filters (`ext:`, `size:`,
/// `dm:`, `type:`...) narrow the list first, then the name text matches fuzzily. Content search is
/// not done here (it reads files; see [`crate::content`]), but folders drop out when it is on.
/// `None` when the query is empty, meaning "no filter".
pub fn filter_by_query(entries: &[Entry], query: &Query) -> Option<Vec<Entry>> {
    if query.is_empty() && query.content.is_none() {
        return None;
    }
    let pattern = (!query.text.trim().is_empty())
        .then(|| Pattern::parse(&query.text, CaseMatching::Smart, Normalization::Smart));
    let mut matcher = Matcher::default();
    let mut buf = Vec::new();
    Some(
        entries
            .iter()
            .filter(|e| entry_accepted(query, e))
            .filter(|e| {
                pattern.as_ref().is_none_or(|p| {
                    p.score(Utf32Str::new(&e.name, &mut buf), &mut matcher)
                        .is_some()
                })
            })
            .cloned()
            .collect(),
    )
}

/// Whether an entry passes the query's filters (not the name text).
pub fn entry_accepted(query: &Query, entry: &Entry) -> bool {
    let modified = entry
        .modified
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let is_dir = entry.is_dir_like();
    // `content:` is still being typed (no text yet): files only, like when it has text.
    if is_dir && query.content.is_some() {
        return false;
    }
    query.accepts(&entry.name, is_dir, entry.size, modified)
}

/// One path match: index in the input list and the matched **byte** ranges for highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatch {
    /// Index into the `candidates` slice.
    pub index: usize,
    /// Byte ranges in the candidate that matched the query.
    pub highlights: Vec<std::ops::Range<usize>>,
}

/// For Ctrl+T: fuzzy-matches a short list of paths (recent, frequent, tabs) in path mode,
/// sorted by score with ties kept in input order (more recent first). A few hundred items
/// take microseconds, so the UI thread is fine.
pub fn rank_paths(candidates: &[String], query: &str) -> Vec<PathMatch> {
    if query.trim().is_empty() {
        return (0..candidates.len())
            .map(|index| PathMatch {
                index,
                highlights: Vec::new(),
            })
            .collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(nucleo::Config::DEFAULT.match_paths());
    let mut buf = Vec::new();
    let mut indices = Vec::new();
    let mut scored: Vec<(u32, PathMatch)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, text)| {
            indices.clear();
            let score =
                pattern.indices(Utf32Str::new(text, &mut buf), &mut matcher, &mut indices)?;
            Some((
                score,
                PathMatch {
                    index,
                    highlights: char_ranges_to_bytes(text, &mut indices),
                },
            ))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.index.cmp(&b.1.index)));
    scored.into_iter().map(|(_, m)| m).collect()
}

/// nucleo returns char positions (possibly duplicated, unsorted); convert to merged byte ranges.
pub(crate) fn char_ranges_to_bytes(
    text: &str,
    chars: &mut Vec<u32>,
) -> Vec<std::ops::Range<usize>> {
    chars.sort_unstable();
    chars.dedup();
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut wanted = chars.iter().peekable();
    for (ci, (bi, ch)) in text.char_indices().enumerate() {
        let Some(&&next) = wanted.peek() else {
            break;
        };
        if usize::try_from(next).ok() != Some(ci) {
            continue;
        }
        wanted.next();
        let end = bi + ch.len_utf8();
        match out.last_mut() {
            Some(last) if last.end == bi => last.end = end,
            _ => out.push(bi..end),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    #[test]
    fn rank_paths_scores_and_highlights() {
        // CJK path segments check that char positions become multi-byte ranges correctly.
        let list: Vec<String> = [
            r"C:\Users\me\Downloads",
            r"D:\相簿\2026",
            r"D:\相簿\2026\十月",
            r"C:\Projects\garden-planner",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        // Blank query: everything, original order.
        assert_eq!(rank_paths(&list, " ").len(), 4);
        let ranked = rank_paths(&list, "相簿 2026");
        let order: Vec<usize> = ranked.iter().map(|m| m.index).collect();
        assert_eq!(order, [1, 2]);
        // Highlight ranges are byte ranges on char boundaries.
        for m in &ranked {
            let text = &list[m.index];
            for r in &m.highlights {
                assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
            }
            let marked: String = m.highlights.iter().map(|r| &text[r.clone()]).collect();
            assert_eq!(marked, "相簿2026");
        }
        assert_eq!(rank_paths(&list, "gardplan")[0].index, 3);
        assert!(rank_paths(&list, "zzz").is_empty());
    }

    fn names(list: &[Entry]) -> Vec<&str> {
        list.iter().map(|e| &*e.name).collect()
    }

    #[test]
    fn fuzzy_keeps_order_and_supports_syntax() {
        let list: Vec<Entry> = [
            "Cargo.toml",
            "README.md",
            "report_2026.docx",
            "src",
            "cargo-lock.txt",
        ]
        .iter()
        .map(|n| Entry::new(*n, EntryKind::File, 0, None, 0))
        .collect();
        assert!(filter_entries(&list, "  ").is_none());
        // Fuzzy: c-r-g in order is enough; original order kept.
        assert_eq!(
            names(&filter_entries(&list, "crg").unwrap()),
            ["Cargo.toml", "cargo-lock.txt"]
        );
        // Smart case: uppercase makes it case-sensitive.
        assert_eq!(
            names(&filter_entries(&list, "Crg").unwrap()),
            ["Cargo.toml"]
        );
        assert_eq!(names(&filter_entries(&list, "^src").unwrap()), ["src"]);
        assert_eq!(
            names(&filter_entries(&list, ".md$").unwrap()),
            ["README.md"]
        );
        assert_eq!(
            names(&filter_entries(&list, "report").unwrap()),
            ["report_2026.docx"]
        );
        assert_eq!(
            names(&filter_entries(&list, "cargo !lock").unwrap()),
            ["Cargo.toml"]
        );
    }
    #[test]
    fn query_filters_then_matches_names() {
        use crate::index_query::Clock;
        let mb = 1024 * 1024;
        let list = vec![
            Entry::new("photos", EntryKind::Dir, 0, None, 0),
            Entry::new("trip.jpg", EntryKind::File, 3 * mb, None, 0),
            Entry::new("thumb.jpg", EntryKind::File, 10, None, 0),
            Entry::new("trip notes.txt", EntryKind::File, 10, None, 0),
        ];
        let q = |s: &str| Query::parse(s, Clock::default());
        assert!(filter_by_query(&list, &q(" ")).is_none());
        assert_eq!(
            names(&filter_by_query(&list, &q("type:image")).unwrap()),
            ["trip.jpg", "thumb.jpg"]
        );
        assert_eq!(
            names(&filter_by_query(&list, &q("trip size:>1mb")).unwrap()),
            ["trip.jpg"]
        );
        assert_eq!(
            names(&filter_by_query(&list, &q("folder:")).unwrap()),
            ["photos"]
        );
        // Content search keeps files only, even before the text is typed.
        assert_eq!(
            names(&filter_by_query(&list, &q("content:")).unwrap()).len(),
            3
        );
        // Unknown terms stay fuzzy name text (fzf syntax still works).
        assert_eq!(
            names(&filter_by_query(&list, &q("trip !notes")).unwrap()),
            ["trip.jpg"]
        );
    }
}
