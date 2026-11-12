//! Final ordering of whole-PC search results: match quality first, then what the user uses.
//!
//! The index picks its top hits by fuzzy score (`crate::index`). This pass reorders those hits
//! (and visited items the index didn't return, [`frecent_hits`]) in O(results):
//!
//! 1. **Match class** ([`MatchClass`]): exact name > prefix > substring > fuzzy. It always comes
//!    first, so no amount of use lifts a fuzzy match above an exact one.
//! 2. Within a class, the fuzzy score plus small bonuses:
//!    - **frecency** ([`frecency_bonus`], capped at [`MAX_FRECENCY_BONUS`]): the copy of
//!      `report.docx` the user keeps opening beats the other 30;
//!    - **path** ([`path_bonus`]): under the user's profile a little up, under Windows, Program
//!      Files, `ProgramData` or `AppData` down, very deep paths slightly down.
//!
//!    The bonuses are a few matched characters' worth of score: enough to order equal or nearly
//!    equal matches, not enough to beat a clearly better one.
//! 3. Ties: shorter path first.
//!
//! Queries without a name (filters only) keep the index order (newest first).

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::frecency::{FrecencySnapshot, path_key};
use crate::index::{IndexHit, PathMatcher, split_query};
use crate::index_query::Query;

/// How well a name matches the typed text, best last (so `Ord` sorts by quality).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchClass {
    /// Letters in order with gaps (`rprt` in `report`).
    Fuzzy,
    /// Every term appears as typed, not at the start.
    Contains,
    /// The name starts with the first term (and contains the others).
    Prefix,
    /// The name, or the name without its extension, is the typed text.
    Exact,
}

/// Class of `name` for the name part of a query (`!` exclusions and `^'$` markers ignored,
/// letter case ignored).
#[must_use]
pub fn match_class(name: &str, name_query: &str) -> MatchClass {
    let atoms: Vec<String> = name_query
        .split_whitespace()
        .filter(|a| !a.starts_with('!'))
        .map(|a| {
            a.trim_start_matches(['^', '\''])
                .trim_end_matches('$')
                .to_lowercase()
        })
        .filter(|a| !a.is_empty())
        .collect();
    let Some(first) = atoms.first() else {
        return MatchClass::Fuzzy;
    };
    let name = name.to_lowercase();
    if !atoms.iter().all(|a| name.contains(a.as_str())) {
        return MatchClass::Fuzzy;
    }
    let whole = atoms.join(" ");
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name.as_str(),
    };
    if name == whole || stem == whole {
        MatchClass::Exact
    } else if name.starts_with(first.as_str()) {
        MatchClass::Prefix
    } else {
        MatchClass::Contains
    }
}

/// Largest frecency bonus (four matched characters' worth of fuzzy score).
pub const MAX_FRECENCY_BONUS: i64 = 64;
/// Bonus per natural-log unit of frecency: one recent visit is worth about 17, three about 33.
const FRECENCY_SCALE: f32 = 24.0;
/// Items under the user's profile.
pub const HOME_BONUS: i64 = 8;
/// Items under Windows, Program Files, `ProgramData` or `AppData`.
pub const SYSTEM_PENALTY: i64 = 16;
/// Path depth (separators) that costs nothing; each level beyond costs 1, up to
/// [`MAX_DEPTH_PENALTY`].
const FREE_DEPTH: usize = 5;
/// Largest depth penalty.
pub const MAX_DEPTH_PENALTY: i64 = 8;

/// Bonus for a frecency value (0 for never visited). Grows with the logarithm, so the hundredth
/// visit adds little, and is capped.
#[must_use]
pub fn frecency_bonus(value: f32) -> i64 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    // Bounded by the cap below, so the cast can't overflow.
    #[allow(clippy::cast_possible_truncation)]
    let bonus = (FRECENCY_SCALE * value.ln_1p()).round() as i64;
    bonus.clamp(0, MAX_FRECENCY_BONUS)
}

/// What the ranking knows besides the hits: frecency and where the user's own files are.
#[derive(Clone, Debug, Default)]
pub struct RankSignals {
    /// Visit history (`None`: history is off).
    pub frecency: Option<Arc<FrecencySnapshot>>,
    /// [`path_key`] of the user's profile folder.
    home: Option<String>,
    /// [`path_key`]s of system and program folders.
    system: Vec<String>,
}

impl RankSignals {
    /// Signals from the visit history, the profile folder and the system and program folders
    /// (`C:\Windows`, `C:\Program Files`, `C:\ProgramData`, `...\AppData`).
    #[must_use]
    pub fn new(
        frecency: Option<Arc<FrecencySnapshot>>,
        home: Option<&Path>,
        system: &[PathBuf],
    ) -> Self {
        Self {
            frecency,
            home: home.map(path_key).filter(|k| !k.is_empty()),
            system: system
                .iter()
                .map(|p| path_key(p))
                .filter(|k| !k.is_empty())
                .collect(),
        }
    }

    fn frecency_of(&self, key: &str) -> f32 {
        self.frecency.as_ref().map_or(0.0, |f| f.value_of(key))
    }
}

/// `key` is inside the folder `dir` (both [`path_key`]s).
fn is_under(key: &str, dir: &str) -> bool {
    key.len() > dir.len()
        && key.starts_with(dir)
        && (dir.ends_with('\\') || key.as_bytes()[dir.len()] == b'\\')
}

/// Path bonus for a [`path_key`]: profile up, system and program folders down (they win over
/// the profile: `AppData` is inside it), very deep paths slightly down.
#[must_use]
pub fn path_bonus(key: &str, signals: &RankSignals) -> i64 {
    let mut bonus = if signals.system.iter().any(|s| is_under(key, s)) {
        -SYSTEM_PENALTY
    } else if signals.home.as_deref().is_some_and(|h| is_under(key, h)) {
        HOME_BONUS
    } else {
        0
    };
    let depth = key.bytes().filter(|&b| b == b'\\').count();
    let extra = i64::try_from(depth.saturating_sub(FREE_DEPTH)).unwrap_or(i64::MAX);
    bonus -= extra.min(MAX_DEPTH_PENALTY);
    bonus
}

/// Sort key of one hit: class, then adjusted score, then shorter paths.
type Key = Reverse<(MatchClass, i64, Reverse<usize>)>;

fn rank_key(hit: &IndexHit, name_part: &str, signals: &RankSignals) -> Key {
    let name = hit
        .path
        .file_name()
        .map_or_else(|| hit.path.to_string_lossy(), |n| n.to_string_lossy());
    let key = path_key(&hit.path);
    let class = match_class(&name, name_part);
    let adjusted = i64::from(hit.score())
        + frecency_bonus(signals.frecency_of(&key))
        + path_bonus(&key, signals);
    Reverse((class, adjusted, Reverse(key.len())))
}

/// Reorders `hits` for `query` (see the module docs). Filters-only queries are left as they are.
pub fn rerank(hits: &mut [IndexHit], query: &Query, signals: &RankSignals) {
    let (_, name_part) = split_query(query.text.trim());
    if name_part.is_empty() {
        return;
    }
    // Stable, and the key (a lowercase copy of the path, a hash lookup) is built once per hit.
    hits.sort_by_cached_key(|hit| rank_key(hit, name_part, signals));
}

/// Merges index hits with `extra` ones (visited items, [`frecent_hits`]), drops duplicates (any
/// letter case), reranks and keeps the best `limit`.
#[must_use]
pub fn merge(
    hits: Vec<IndexHit>,
    extra: Vec<IndexHit>,
    query: &Query,
    signals: &RankSignals,
    limit: usize,
) -> Vec<IndexHit> {
    let mut out = hits;
    if !extra.is_empty() {
        let known: std::collections::HashSet<String> =
            out.iter().map(|h| path_key(&h.path)).collect();
        out.extend(
            extra
                .into_iter()
                .filter(|h| !known.contains(&path_key(&h.path))),
        );
    }
    rerank(&mut out, query, signals);
    out.truncate(limit);
    out
}

/// What [`frecent_hits`] needs to know about a visited item, from the file system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// A folder (or link to one).
    pub is_dir: bool,
    /// Size in bytes (0 for folders).
    pub size: u64,
    /// Last write time.
    pub modified: Option<SystemTime>,
    /// Hidden (or a dot-file with dot-files hidden).
    pub hidden: bool,
}

/// Visited items that match `query`, as hits: the index returns only its top matches by fuzzy
/// score, so the one copy of `README.md` the user opens may not be among the thousands it found.
/// `covers` says which paths this index is responsible for (roots, exclusions); `stat` reads the
/// item (dropped if gone). At most `max` hits; only matching items are read.
pub fn frecent_hits(
    snapshot: &FrecencySnapshot,
    query: &Query,
    show_hidden: bool,
    max: usize,
    covers: impl Fn(&Path) -> bool,
    stat: impl Fn(&Path) -> Option<Stat>,
) -> Vec<IndexHit> {
    let mut out = Vec::new();
    let Some(mut matcher) = PathMatcher::new(query) else {
        return out;
    };
    for item in snapshot.items() {
        if out.len() >= max {
            break;
        }
        if !covers(&item.path) {
            continue;
        }
        let Some((score, highlights)) = matcher.matches(&item.path) else {
            continue;
        };
        let Some(st) = stat(&item.path) else {
            continue;
        };
        if st.hidden && !show_hidden {
            continue;
        }
        let name = item
            .path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        let modified = st
            .modified
            .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        if query.has_filters() && !query.accepts(&name, st.is_dir, st.size, modified) {
            continue;
        }
        out.push(IndexHit::new(
            item.path.clone(),
            st.is_dir,
            st.size,
            st.modified,
            highlights,
            score,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frecency::Frecency;
    use crate::index_query::Clock;

    const NOW: u64 = 1_800_000_000;

    fn q(text: &str) -> Query {
        Query::parse(text, Clock::default())
    }

    fn hit(path: &str, score: u32) -> IndexHit {
        IndexHit::new(PathBuf::from(path), false, 1, None, Vec::new(), score)
    }

    fn paths(hits: &[IndexHit]) -> Vec<String> {
        hits.iter().map(|h| h.path.display().to_string()).collect()
    }

    fn signals(visits: &[(&str, u32)]) -> RankSignals {
        let mut f = Frecency::default();
        for (path, times) in visits {
            for i in 0..*times {
                f.record(Path::new(path), false, NOW + u64::from(i) * 60);
            }
        }
        RankSignals::new(
            Some(Arc::new(f.snapshot(NOW + 3600))),
            Some(Path::new(r"C:\Users\me")),
            &[
                PathBuf::from(r"C:\Windows"),
                PathBuf::from(r"C:\Program Files"),
                PathBuf::from(r"C:\Users\me\AppData"),
            ],
        )
    }

    #[test]
    fn classes_order_exact_prefix_contains_fuzzy() {
        assert_eq!(match_class("Report.docx", "report"), MatchClass::Exact);
        assert_eq!(match_class("report", "REPORT"), MatchClass::Exact);
        assert_eq!(
            match_class("annual report.pdf", "annual report"),
            MatchClass::Exact
        );
        assert_eq!(
            match_class("report-2026.docx", "report"),
            MatchClass::Prefix
        );
        assert_eq!(
            match_class("old report.txt", "report old"),
            MatchClass::Contains
        );
        assert_eq!(
            match_class("quarterly-report.pdf", "report"),
            MatchClass::Contains
        );
        assert_eq!(match_class("r_e_p_o_r_t.txt", "report"), MatchClass::Fuzzy);
        assert_eq!(match_class(".gitignore", ".gitignore"), MatchClass::Exact);
        // Markers and exclusions don't count as text.
        assert_eq!(match_class("readme.md", "^readme !old"), MatchClass::Exact);
        assert_eq!(match_class("anything", "!old"), MatchClass::Fuzzy);
        assert_eq!(match_class("季報告.xlsx", "季報告"), MatchClass::Exact);
        assert!(MatchClass::Exact > MatchClass::Prefix);
        assert!(MatchClass::Contains > MatchClass::Fuzzy);
    }

    #[test]
    fn frecency_bonus_grows_slowly_and_is_capped() {
        assert_eq!(frecency_bonus(0.0), 0);
        assert_eq!(frecency_bonus(-1.0), 0);
        assert_eq!(frecency_bonus(f32::NAN), 0);
        let one = frecency_bonus(1.0);
        let three = frecency_bonus(3.0);
        assert!(one > 10 && one < 20, "one visit: {one}");
        assert!(three > one && three < 2 * one + 5);
        assert_eq!(frecency_bonus(1e9), MAX_FRECENCY_BONUS);
    }

    #[test]
    fn path_bonus_prefers_the_profile_and_shallow_paths() {
        let s = signals(&[]);
        assert_eq!(path_bonus(r"c:\users\me\documents\a.txt", &s), HOME_BONUS);
        assert_eq!(
            path_bonus(r"c:\users\me\appdata\local\a.txt", &s),
            -SYSTEM_PENALTY
        );
        assert_eq!(path_bonus(r"c:\windows\a.txt", &s), -SYSTEM_PENALTY);
        assert_eq!(path_bonus(r"c:\program files\x\a.txt", &s), -SYSTEM_PENALTY);
        // Only whole folder names: "C:\Windows.old" isn't Windows, "C:\Users\meg" isn't "me".
        assert_eq!(path_bonus(r"c:\windows.old\a.txt", &s), 0);
        assert_eq!(path_bonus(r"c:\users\meg\a.txt", &s), 0);
        let deep = r"c:\d1\d2\d3\d4\d5\d6\d7\a.txt";
        assert_eq!(path_bonus(deep, &s), -3);
        let very_deep = format!(r"c:{}\a.txt", r"\d".repeat(40));
        assert_eq!(path_bonus(&very_deep, &s), -MAX_DEPTH_PENALTY);
    }

    #[test]
    fn the_copy_in_use_comes_first_among_same_names() {
        let mut hits = vec![
            hit(r"C:\a\README.md", 200),
            hit(r"C:\b\README.md", 200),
            hit(r"C:\Users\me\code\arke\README.md", 200),
            hit(r"C:\c\README.md", 200),
        ];
        rerank(
            &mut hits,
            &q("readme"),
            &signals(&[(r"C:\Users\me\code\arke\README.md", 3)]),
        );
        assert_eq!(hits[0].path, Path::new(r"C:\Users\me\code\arke\README.md"));
        // The rest: shorter paths first, otherwise as they were.
        assert_eq!(
            paths(&hits[1..]),
            [r"C:\a\README.md", r"C:\b\README.md", r"C:\c\README.md"]
        );
    }

    #[test]
    fn use_never_lifts_a_worse_match_above_a_better_one() {
        let mut hits = vec![
            hit(r"C:\x\report.docx", 150),
            hit(r"C:\x\report-final.docx", 150),
            hit(r"C:\x\r_e_p_o_r_t.txt", 160),
        ];
        // The fuzzy and prefix matches are used a lot, the exact one never.
        rerank(
            &mut hits,
            &q("report"),
            &signals(&[
                (r"C:\x\r_e_p_o_r_t.txt", 50),
                (r"C:\x\report-final.docx", 50),
            ]),
        );
        assert_eq!(
            paths(&hits),
            [
                r"C:\x\report.docx",
                r"C:\x\report-final.docx",
                r"C:\x\r_e_p_o_r_t.txt"
            ]
        );
    }

    #[test]
    fn use_reorders_only_close_scores_within_a_class() {
        // Both fuzzy: a small score gap is overcome by use, a large one isn't.
        let mut close = vec![hit(r"C:\x\rxpxrt1", 120), hit(r"C:\x\rxpxrt2", 110)];
        rerank(&mut close, &q("rprt"), &signals(&[(r"C:\x\rxpxrt2", 3)]));
        assert_eq!(close[0].path, Path::new(r"C:\x\rxpxrt2"));
        let mut far = vec![hit(r"C:\x\rxpxrt1", 200), hit(r"C:\x\rxpxrt2", 100)];
        rerank(&mut far, &q("rprt"), &signals(&[(r"C:\x\rxpxrt2", 50)]));
        assert_eq!(far[0].path, Path::new(r"C:\x\rxpxrt1"));
    }

    #[test]
    fn profile_beats_system_folders_on_equal_matches() {
        let mut hits = vec![
            hit(r"C:\Windows\System32\notes.txt", 100),
            hit(r"C:\Users\me\AppData\Local\notes.txt", 100),
            hit(r"C:\Users\me\Documents\notes.txt", 100),
            hit(r"D:\notes.txt", 100),
        ];
        rerank(&mut hits, &q("notes"), &signals(&[]));
        assert_eq!(
            paths(&hits),
            [
                r"C:\Users\me\Documents\notes.txt",
                r"D:\notes.txt",
                r"C:\Windows\System32\notes.txt",
                r"C:\Users\me\AppData\Local\notes.txt",
            ]
        );
    }

    #[test]
    fn filters_only_queries_keep_their_order() {
        let mut hits = vec![
            hit(r"C:\Windows\new.txt", 900),
            hit(r"C:\Users\me\old.txt", 100),
        ];
        rerank(
            &mut hits,
            &q("ext:txt"),
            &signals(&[(r"C:\Users\me\old.txt", 9)]),
        );
        assert_eq!(hits[0].path, Path::new(r"C:\Windows\new.txt"));
    }

    #[test]
    fn merge_adds_visited_items_once_and_keeps_the_limit() {
        let hits = vec![hit(r"C:\a\README.md", 200), hit(r"C:\b\README.md", 200)];
        let extra = vec![
            hit(r"c:\B\readme.md", 200),
            hit(r"C:\Users\me\deep\README.md", 200),
        ];
        let out = merge(
            hits,
            extra,
            &q("readme"),
            &signals(&[(r"C:\Users\me\deep\README.md", 2)]),
            2,
        );
        assert_eq!(
            paths(&out),
            [r"C:\Users\me\deep\README.md", r"C:\a\README.md"]
        );
    }

    #[test]
    fn frecent_hits_match_filter_and_check_the_disk() {
        let mut f = Frecency::default();
        for path in [
            r"C:\Users\me\README.md",
            r"C:\Users\me\gone\README.md",
            r"C:\Users\me\notes.txt",
            r"C:\Users\me\.hidden\README.md",
            r"E:\README.md",
        ] {
            f.record(Path::new(path), false, NOW);
        }
        let snap = f.snapshot(NOW);
        let stat = |p: &Path| {
            let s = p.to_string_lossy();
            (!s.contains("gone")).then_some(Stat {
                is_dir: false,
                size: 10,
                modified: None,
                hidden: s.contains(".hidden"),
            })
        };
        let covers = |p: &Path| p.starts_with(r"C:\");
        let found = frecent_hits(&snap, &q("readme"), false, 10, covers, stat);
        assert_eq!(paths(&found), [r"C:\Users\me\README.md"]);
        assert_eq!(found[0].highlights.len(), 1);
        assert_eq!(found[0].highlights[0], 0..6);
        let shown = frecent_hits(&snap, &q("readme"), true, 10, covers, stat);
        assert_eq!(shown.len(), 2, "hidden ones with hidden items shown");
        // Filters apply; the name is required.
        assert!(frecent_hits(&snap, &q("readme size:>1mb"), false, 10, covers, stat).is_empty());
        assert!(frecent_hits(&snap, &q("ext:md"), false, 10, covers, stat).is_empty());
        // Folder part.
        assert_eq!(
            frecent_hits(&snap, &q(r"me\notes"), false, 10, covers, stat).len(),
            1
        );
    }
}
