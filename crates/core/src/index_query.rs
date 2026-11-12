//! Query syntax for global search (close to Everything):
//!
//! | Syntax | Meaning |
//! |---|---|
//! | `report` | fuzzy name match (`rprt` finds report) |
//! | `docs\report` | last segment matches the name, the rest the containing folder |
//! | `ext:pdf`, `ext:pdf;docx`, `*.pdf` | extension |
//! | `size:>10mb`, `size:<1kb`, `size:1mb..10mb`, `size:large` | size (files only); `empty` `tiny` `small` `medium` `large` `huge` `gigantic` as in Everything |
//! | `dm:today`, `dm:yesterday`, `dm:thisweek`, `dm:thismonth`, `dm:thisyear` | modified date (local time; UI-language words also work, see `locales/core.yml`) |
//! | `dm:7d`, `dm:24h`, `dm:2w` | last N days / hours / weeks |
//! | `dm:2026`, `dm:2026-11`, `dm:2026-11-08`, `dm:>2026-01-01`, `dm:2026-01..2026-03` | date range |
//! | `file:`, `folder:` | files only / folders only |
//! | `type:image`, `type:document;video` | file type ([`TypeFilter`]; `type:folder` is folders only) |
//! | `content:invoice`, `content:"due date"` | text inside files (this folder and subfolders only) |
//!
//! Unknown `xxx:` is treated as part of the name. With filters but no name, results are
//! listed newest first. Quotes keep spaces inside one term. Pure computation, no file system
//! access.
//!
//! The search box's filter chips edit the text through [`set_facet`] and read it back with
//! [`type_state`] and friends, so the typed syntax and the chips are always one model.

use crate::filetype::{FileKind, kind_for_extension};

/// Clock for date parsing: current time and time zone (from the system in the app, fixed in tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    /// Unix seconds.
    pub now: i64,
    /// Seconds local time is ahead of UTC (+28800 for UTC+8).
    pub utc_offset: i64,
}

/// Item type restriction from `file:` / `folder:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Files only (also implied by a size filter).
    File,
    /// Folders only.
    Folder,
}

/// File type group for `type:` (and the Type chip).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeFilter {
    /// Folders.
    Folder,
    /// Documents: PDF, word processor, spreadsheet, slides and plain text.
    Document,
    /// Images.
    Image,
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// Compressed archives and disk images.
    Archive,
    /// Source code, markup and config files.
    Code,
    /// Programs and installers.
    Executable,
}

impl TypeFilter {
    /// Every type, in chip order.
    pub const ALL: [Self; 8] = [
        Self::Folder,
        Self::Document,
        Self::Image,
        Self::Audio,
        Self::Video,
        Self::Archive,
        Self::Code,
        Self::Executable,
    ];

    /// Keyword written after `type:`.
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Document => "document",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Archive => "archive",
            Self::Code => "code",
            Self::Executable => "app",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "folder" | "folders" | "dir" => Self::Folder,
            "document" | "documents" | "doc" | "docs" => Self::Document,
            "image" | "images" | "picture" | "pictures" | "pic" | "photo" => Self::Image,
            "audio" | "music" | "sound" => Self::Audio,
            "video" | "videos" | "movie" => Self::Video,
            "archive" | "archives" | "zip" | "compressed" => Self::Archive,
            "code" | "source" => Self::Code,
            "app" | "apps" | "exe" | "program" | "executable" => Self::Executable,
            _ => return None,
        })
    }

    /// Whether a file of this category belongs to the group (folders are checked separately).
    const fn has(self, kind: FileKind) -> bool {
        matches!(
            (self, kind),
            (
                Self::Document,
                FileKind::Pdf
                    | FileKind::Document
                    | FileKind::Spreadsheet
                    | FileKind::Presentation
                    | FileKind::Text
            ) | (Self::Image, FileKind::Image)
                | (Self::Audio, FileKind::Audio)
                | (Self::Video, FileKind::Video)
                | (Self::Archive, FileKind::Archive | FileKind::DiskImage)
                | (Self::Code, FileKind::Code)
                | (Self::Executable, FileKind::Executable)
        )
    }

    fn accepts(self, name: &str, is_dir: bool) -> bool {
        if self == Self::Folder || is_dir {
            return self == Self::Folder && is_dir;
        }
        let ext = match name.rfind('.') {
            Some(at) if at > 0 => name[at + 1..].to_ascii_lowercase(),
            _ => String::new(),
        };
        self.has(kind_for_extension(&ext))
    }
}

/// Parsed global search input: name text plus filters (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Name for fuzzy matching (may contain `folder\`).
    pub text: String,
    /// Extensions, lowercase without the dot.
    pub ext: Vec<String>,
    /// Size range in bytes, inclusive.
    pub size: Option<(u64, u64)>,
    /// Modified time [start, end) in Unix seconds.
    pub modified: Option<(i64, i64)>,
    /// Restrict to files or folders.
    pub kind: Option<Kind>,
    /// File type groups from `type:` (any of them).
    pub types: Vec<TypeFilter>,
    /// Text to find inside files (`content:`); may be empty while still being typed.
    pub content: Option<String>,
}

const DAY: i64 = 86_400;
const KB: u64 = 1024;

impl Query {
    /// Parses `input`; unrecognized `xxx:` tokens stay part of the name text.
    pub fn parse(input: &str, clock: Clock) -> Self {
        let mut q = Self::default();
        let mut text: Vec<&str> = Vec::new();
        for (_, token) in tokens(input) {
            if !q.apply_filter(token, clock) {
                text.push(token);
            }
        }
        q.text = text.join(" ");
        // Size only makes sense for files (folder sizes are not in the listing).
        if q.size.is_some() && q.kind.is_none() {
            q.kind = Some(Kind::File);
        }
        q
    }

    /// Whether any filter (extension, size, date, kind, type) is set. Content search
    /// (`content:`) is not one of them; see [`Self::content_needle`].
    pub const fn has_filters(&self) -> bool {
        !self.ext.is_empty()
            || self.size.is_some()
            || self.modified.is_some()
            || self.kind.is_some()
            || !self.types.is_empty()
    }

    /// Text to search for inside files: `content:` with something after it.
    pub fn content_needle(&self) -> Option<&str> {
        self.content.as_deref().filter(|c| !c.trim().is_empty())
    }

    /// No name text, no filters and no content text; searching it returns no hits.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && !self.has_filters() && self.content_needle().is_none()
    }

    /// Same filters and content text (everything except the name text).
    pub fn same_filters(&self, other: &Self) -> bool {
        self.ext == other.ext
            && self.size == other.size
            && self.modified == other.modified
            && self.kind == other.kind
            && self.types == other.types
            && self.content_needle() == other.content_needle()
    }

    /// Whether an item passes the filters (fuzzy name matching not included). `modified` is
    /// Unix seconds, 0 if unknown.
    pub fn accepts(&self, name: &str, is_dir: bool, size: u64, modified: i64) -> bool {
        match self.kind {
            Some(Kind::File) if is_dir => return false,
            Some(Kind::Folder) if !is_dir => return false,
            _ => {}
        }
        if !self.ext.is_empty() {
            let ext = match name.rfind('.') {
                Some(at) if at > 0 && !is_dir => &name[at + 1..],
                _ => return false,
            };
            if !self.ext.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                return false;
            }
        }
        if let Some((lo, hi)) = self.size
            && !(lo..=hi).contains(&size)
        {
            return false;
        }
        if let Some((lo, hi)) = self.modified
            && (modified == 0 || !(lo..hi).contains(&modified))
        {
            return false;
        }
        if !self.types.is_empty() && !self.types.iter().any(|t| t.accepts(name, is_dir)) {
            return false;
        }
        // Folders have no contents to search.
        !(is_dir && self.content_needle().is_some())
    }

    fn apply_filter(&mut self, token: &str, clock: Clock) -> bool {
        // *.pdf
        if let Some(ext) = token.strip_prefix("*.")
            && !ext.is_empty()
            && !ext.contains(['*', '?', '.', '\\', '/'])
        {
            self.ext.push(ext.to_lowercase());
            return true;
        }
        let Some((key, value)) = token.split_once(':') else {
            return false;
        };
        match key.to_ascii_lowercase().as_str() {
            "ext" if !value.is_empty() => {
                self.ext.extend(
                    value
                        .split([';', ','])
                        .map(|e| e.trim_start_matches('.').to_lowercase())
                        .filter(|e| !e.is_empty()),
                );
                true
            }
            "size" => match parse_size_filter(value) {
                Some(range) => {
                    self.size = Some(range);
                    true
                }
                None => false,
            },
            "dm" | "datemodified" => match parse_date_filter(value, clock) {
                Some(range) => {
                    self.modified = Some(range);
                    true
                }
                None => false,
            },
            "file" if value.is_empty() => {
                self.kind = Some(Kind::File);
                true
            }
            "folder" if value.is_empty() => {
                self.kind = Some(Kind::Folder);
                true
            }
            "type" if !value.is_empty() => {
                let types: Option<Vec<TypeFilter>> = value
                    .split([';', ','])
                    .filter(|t| !t.is_empty())
                    .map(|t| TypeFilter::parse(&t.to_ascii_lowercase()))
                    .collect();
                match types {
                    Some(types) if !types.is_empty() => {
                        self.types.extend(types);
                        true
                    }
                    _ => false,
                }
            }
            "content" => {
                self.content = Some(value.replace('"', ""));
                true
            }
            _ => false,
        }
    }
}

// ───────────────────────── Terms and chips ─────────────────────────

/// Splits input into terms at whitespace outside double quotes; each with its byte offset.
fn tokens(input: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (at, ch) in input.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch.is_whitespace() && !quoted {
            if let Some(s) = start.take() {
                out.push((s, &input[s..at]));
            }
        } else if start.is_none() {
            start = Some(at);
        }
    }
    if let Some(s) = start {
        out.push((s, &input[s..]));
    }
    out
}

/// Which chip a term belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Facet {
    /// `type:`, `folder:`, `file:`, `ext:`, `*.x`.
    Type,
    /// `size:`.
    Size,
    /// `dm:`.
    Date,
    /// `content:`.
    Content,
}

/// The facet of one term, `None` for name text (anything not recognized as a filter).
fn facet_of(token: &str) -> Option<Facet> {
    let mut q = Query::default();
    if !q.apply_filter(token, Clock::default()) {
        return None;
    }
    Some(if q.size.is_some() {
        Facet::Size
    } else if q.modified.is_some() {
        Facet::Date
    } else if q.content.is_some() {
        Facet::Content
    } else {
        Facet::Type
    })
}

/// Size chip presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizePreset {
    /// Over 1 MB.
    Over1Mb,
    /// Over 100 MB.
    Over100Mb,
    /// Over 1 GB.
    Over1Gb,
    /// Empty files.
    Empty,
}

impl SizePreset {
    /// Every preset, in chip order.
    pub const ALL: [Self; 4] = [Self::Over1Mb, Self::Over100Mb, Self::Over1Gb, Self::Empty];

    /// The term the chip writes.
    pub const fn term(self) -> &'static str {
        match self {
            Self::Over1Mb => "size:>1mb",
            Self::Over100Mb => "size:>100mb",
            Self::Over1Gb => "size:>1gb",
            Self::Empty => "size:empty",
        }
    }
}

/// Date modified chip presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePreset {
    /// Today.
    Today,
    /// This week (from Monday).
    ThisWeek,
    /// This month.
    ThisMonth,
    /// This year.
    ThisYear,
}

impl DatePreset {
    /// Every preset, in chip order.
    pub const ALL: [Self; 4] = [Self::Today, Self::ThisWeek, Self::ThisMonth, Self::ThisYear];

    /// The term the chip writes.
    pub const fn term(self) -> &'static str {
        match self {
            Self::Today => "dm:today",
            Self::ThisWeek => "dm:thisweek",
            Self::ThisMonth => "dm:thismonth",
            Self::ThisYear => "dm:thisyear",
        }
    }
}

/// What a chip shows for the current text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FacetState<T> {
    /// No term for this facet.
    Off,
    /// Exactly one of the presets.
    Preset(T),
    /// Something else typed by hand (the raw terms, shown as the chip label).
    Custom(String),
}

/// Terms of one facet in `input`, in order.
fn facet_terms(input: &str, facet: Facet) -> Vec<&str> {
    tokens(input)
        .into_iter()
        .filter(|(_, t)| facet_of(t) == Some(facet))
        .map(|(_, t)| t)
        .collect()
}

fn state_of<T: Copy>(terms: &[&str], found: Option<T>) -> FacetState<T> {
    match (terms, found) {
        ([], _) => FacetState::Off,
        ([_], Some(preset)) => FacetState::Preset(preset),
        _ => FacetState::Custom(terms.join(" ")),
    }
}

/// Type chip state: a single `type:x` (or `folder:`) is that preset.
pub fn type_state(input: &str) -> FacetState<TypeFilter> {
    let terms = facet_terms(input, Facet::Type);
    let found = match terms.as_slice() {
        [term] => {
            let q = Query::parse(term, Clock::default());
            match (q.types.as_slice(), q.kind, q.ext.is_empty()) {
                ([t], None, true) => Some(*t),
                ([], Some(Kind::Folder), true) => Some(TypeFilter::Folder),
                _ => None,
            }
        }
        _ => None,
    };
    state_of(&terms, found)
}

/// Size chip state: a term with the same range as a preset (`size:>1MB` too) is that preset.
pub fn size_state(input: &str) -> FacetState<SizePreset> {
    let terms = facet_terms(input, Facet::Size);
    let found = match terms.as_slice() {
        [term] => {
            let size = Query::parse(term, Clock::default()).size;
            SizePreset::ALL
                .into_iter()
                .find(|p| Query::parse(p.term(), Clock::default()).size == size)
        }
        _ => None,
    };
    state_of(&terms, found)
}

/// Date chip state: a term with the same range as a preset (UI-language words too) is that
/// preset.
pub fn date_state(input: &str, clock: Clock) -> FacetState<DatePreset> {
    let terms = facet_terms(input, Facet::Date);
    let found = match terms.as_slice() {
        [term] => {
            let range = Query::parse(term, clock).modified;
            DatePreset::ALL
                .into_iter()
                .find(|p| Query::parse(p.term(), clock).modified == range)
        }
        _ => None,
    };
    state_of(&terms, found)
}

/// Replaces every term of `facet` with `term` (or just removes them with `None`). Other text
/// keeps its order; the new term goes where the first old one was, or at the end.
pub fn set_facet(input: &str, facet: Facet, term: Option<&str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut placed = false;
    for (_, token) in tokens(input) {
        if facet_of(token) == Some(facet) {
            if !placed && let Some(term) = term {
                out.push(term);
            }
            placed = true;
        } else {
            out.push(token);
        }
    }
    if !placed && let Some(term) = term {
        out.push(term);
    }
    out.join(" ")
}

/// Whether the text has a `content:` term (the "search in file contents" chip is on).
pub fn has_content_term(input: &str) -> bool {
    !facet_terms(input, Facet::Content).is_empty()
}

/// Turns content search on or off. On: the name text moves into one `content:"…"` term (an
/// empty `content:""` when there is no text, ready to type into). Off: the content text becomes
/// name text again. Filters stay.
pub fn set_content_search(input: &str, on: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    for (_, token) in tokens(input) {
        match facet_of(token) {
            Some(Facet::Content) => {
                let value = token
                    .split_once(':')
                    .map_or("", |(_, v)| v)
                    .replace('"', "");
                if !value.trim().is_empty() {
                    words.push(value.trim().to_string());
                }
            }
            Some(_) => out.push(token.to_string()),
            None => words.push(token.to_string()),
        }
    }
    let text = words.join(" ");
    if on {
        out.push(format!("content:\"{text}\""));
    } else if !text.is_empty() {
        out.push(text);
    }
    out.join(" ")
}

/// Input without any `content:` term (for "This PC", which can't search contents).
pub fn without_content(input: &str) -> String {
    set_facet(input, Facet::Content, None)
}

// ───────────────────────── Size ─────────────────────────

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let digits_end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let num: f64 = num.parse().ok()?;
    let mult = match unit {
        "" | "b" => 1,
        "k" | "kb" => KB,
        "m" | "mb" => KB * KB,
        "g" | "gb" => KB * KB * KB,
        "t" | "tb" => KB * KB * KB * KB,
        _ => return None,
    };
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let bytes = (num * mult as f64) as u64;
    Some(bytes)
}

/// Size filter -> [min, max] inclusive.
fn parse_size_filter(value: &str) -> Option<(u64, u64)> {
    let named = match value.to_ascii_lowercase().as_str() {
        "empty" => Some((0, 0)),
        "tiny" => Some((1, 10 * KB)),
        "small" => Some((10 * KB + 1, 100 * KB)),
        "medium" => Some((100 * KB + 1, KB * KB)),
        "large" => Some((KB * KB + 1, 16 * KB * KB)),
        "huge" => Some((16 * KB * KB + 1, 128 * KB * KB)),
        "gigantic" => Some((128 * KB * KB + 1, u64::MAX)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some((a, b)) = value.split_once("..") {
        return Some((parse_size(a)?, parse_size(b)?));
    }
    if let Some(v) = value.strip_prefix(">=") {
        return Some((parse_size(v)?, u64::MAX));
    }
    if let Some(v) = value.strip_prefix("<=") {
        return Some((0, parse_size(v)?));
    }
    if let Some(v) = value.strip_prefix('>') {
        return Some((parse_size(v)?.saturating_add(1), u64::MAX));
    }
    if let Some(v) = value.strip_prefix('<') {
        return Some((0, parse_size(v)?.checked_sub(1)?));
    }
    let v = parse_size(value.strip_prefix('=').unwrap_or(value))?;
    Some((v, v))
}

// ───────────────────────── Dates ─────────────────────────

/// Civil date -> days since 1970-01-01 (Howard Hinnant's algorithm).
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 -> civil date.
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Date keyword. English always works; UI-language words from `query.dm.*` in
/// `locales/core.yml` are accepted too (comma-separated alternatives).
fn date_word(s: &str) -> String {
    const WORDS: [&str; 5] = ["today", "yesterday", "thisweek", "thismonth", "thisyear"];
    for word in WORDS {
        let local = rust_i18n::t!(format!("query.dm.{word}"));
        if local.split(',').any(|w| w.trim() == s) {
            return word.to_string();
        }
    }
    s.to_string()
}

/// One date term -> [start, end) in Unix seconds.
fn parse_date_span(s: &str, clock: Clock) -> Option<(i64, i64)> {
    let local_day = (clock.now + clock.utc_offset).div_euclid(DAY);
    // Unix seconds at local midnight of a day.
    let start = |day: i64| day * DAY - clock.utc_offset;
    let (y, m, _) = civil_from_days(local_day);
    let word = date_word(&s.to_lowercase());
    let span = match word.as_str() {
        "today" => (start(local_day), start(local_day + 1)),
        "yesterday" => (start(local_day - 1), start(local_day)),
        "thisweek" => {
            // Weeks start on Monday (1970-01-01 was a Thursday).
            let monday = local_day - (local_day + 3).rem_euclid(7);
            (start(monday), start(monday + 7))
        }
        "thismonth" => {
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            (
                start(days_from_civil(y, m, 1)),
                start(days_from_civil(ny, nm, 1)),
            )
        }
        "thisyear" => (
            start(days_from_civil(y, 1, 1)),
            start(days_from_civil(y + 1, 1, 1)),
        ),
        other => {
            // 7d, 24h, 2w: a recent period.
            let unit = match other.chars().last()? {
                'd' => DAY,
                'h' => 3600,
                'w' => 7 * DAY,
                _ => return parse_calendar(other, start),
            };
            let n: i64 = other[..other.len() - 1].parse().ok()?;
            (clock.now - n * unit, i64::MAX)
        }
    };
    Some(span)
}

/// 2026, 2026-11, 2026-11-08 (`/` and `.` also accepted).
fn parse_calendar(s: &str, start: impl Fn(i64) -> i64) -> Option<(i64, i64)> {
    let parts: Vec<i64> = s
        .split(['-', '/', '.'])
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [y] if *y >= 1970 => Some((
            start(days_from_civil(*y, 1, 1)),
            start(days_from_civil(y + 1, 1, 1)),
        )),
        [y, m] if (1..=12).contains(m) => {
            let (ny, nm) = if *m == 12 { (y + 1, 1) } else { (*y, m + 1) };
            Some((
                start(days_from_civil(*y, *m, 1)),
                start(days_from_civil(ny, nm, 1)),
            ))
        }
        [y, m, d] if (1..=12).contains(m) && (1..=31).contains(d) => {
            let day = days_from_civil(*y, *m, *d);
            Some((start(day), start(day + 1)))
        }
        _ => None,
    }
}

fn parse_date_filter(value: &str, clock: Clock) -> Option<(i64, i64)> {
    if let Some((a, b)) = value.split_once("..") {
        return Some((parse_date_span(a, clock)?.0, parse_date_span(b, clock)?.1));
    }
    if let Some(v) = value.strip_prefix(">=") {
        return Some((parse_date_span(v, clock)?.0, i64::MAX));
    }
    if let Some(v) = value.strip_prefix("<=") {
        return Some((i64::MIN, parse_date_span(v, clock)?.1));
    }
    if let Some(v) = value.strip_prefix('>') {
        return Some((parse_date_span(v, clock)?.1, i64::MAX));
    }
    if let Some(v) = value.strip_prefix('<') {
        return Some((i64::MIN, parse_date_span(v, clock)?.0));
    }
    parse_date_span(value.strip_prefix('=').unwrap_or(value), clock)
}

/// Looks like filter syntax (Ctrl+T uses it to tell it's not a path).
pub fn has_filter_syntax(input: &str) -> bool {
    Query::parse(input, Clock::default()).has_filters()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFSET: i64 = 8 * 3600;

    /// 2026-11-08 (Sunday) 20:00, UTC+8.
    fn tw() -> Clock {
        Clock {
            now: local(2026, 11, 8) + 20 * 3600,
            utc_offset: OFFSET,
        }
    }

    fn local(y: i64, m: i64, d: i64) -> i64 {
        days_from_civil(y, m, d) * DAY - OFFSET
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 11, 8)), (2026, 11, 8));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        // Local date of tw().now is 2026-11-08.
        assert_eq!(
            civil_from_days((tw().now + tw().utc_offset).div_euclid(DAY)),
            (2026, 11, 8)
        );
    }

    #[test]
    fn parses_names_and_filters() {
        let q = Query::parse("report ext:pdf;DOCX size:>10mb dm:today", tw());
        assert_eq!(q.text, "report");
        assert_eq!(q.ext, ["pdf", "docx"]);
        assert_eq!(q.size, Some((10 * KB * KB + 1, u64::MAX)));
        assert_eq!(q.kind, Some(Kind::File), "size implies files");
        assert_eq!(q.modified, Some((local(2026, 11, 8), local(2026, 11, 9))));

        let q = Query::parse(r"docs\report *.md folder:", tw());
        assert_eq!(q.text, r"docs\report");
        assert_eq!(q.ext, ["md"]);
        assert_eq!(q.kind, Some(Kind::Folder));

        // Unknown terms become name text: drive letters, mistyped filters.
        let q = Query::parse("C: size:abc foo:bar", tw());
        assert_eq!(q.text, "C: size:abc foo:bar");
        assert!(!q.has_filters());
        assert!(Query::parse("  ", tw()).is_empty());
        assert!(has_filter_syntax("ext:pdf"));
        assert!(!has_filter_syntax(r"C:\Users"));
    }

    #[test]
    fn size_and_date_forms() {
        let size = |v| Query::parse(&format!("size:{v}"), tw()).size;
        assert_eq!(size("1.5kb"), Some((1536, 1536)));
        assert_eq!(size("<1kb"), Some((0, 1023)));
        assert_eq!(size("1mb..2mb"), Some((KB * KB, 2 * KB * KB)));
        assert_eq!(size("empty"), Some((0, 0)));
        assert_eq!(size("gigantic").map(|s| s.1), Some(u64::MAX));

        // Date keywords are also accepted in the UI language; the zh-TW words are the point.
        rust_i18n::set_locale("zh-TW");
        let dm = |v: &str| Query::parse(&format!("dm:{v}"), tw()).modified;
        assert_eq!(dm("這週"), dm("thisweek"), "UI-language word");
        rust_i18n::set_locale("zh-CN");
        assert_eq!(dm("本周"), dm("thisweek"), "zh-CN word");
        assert_eq!(dm("这个月"), dm("thismonth"), "zh-CN word");
        assert_eq!(dm("今年"), dm("thisyear"), "zh-CN word");
        rust_i18n::set_locale("zh-TW");
        assert_eq!(dm("昨天"), Some((local(2026, 11, 7), local(2026, 11, 8))));
        // 2026-11-08 is a Sunday: this week is 11-02 (Mon) to 11-09.
        assert_eq!(
            dm("thisweek"),
            Some((local(2026, 11, 2), local(2026, 11, 9)))
        );
        assert_eq!(
            dm("thismonth"),
            Some((local(2026, 11, 1), local(2026, 12, 1)))
        );
        assert_eq!(dm("2026-12"), Some((local(2026, 12, 1), local(2027, 1, 1))));
        assert_eq!(
            dm("2026/1/31"),
            Some((local(2026, 1, 31), local(2026, 2, 1)))
        );
        assert_eq!(dm("7d"), Some((tw().now - 7 * DAY, i64::MAX)));
        assert_eq!(dm(">2026-01-01"), Some((local(2026, 1, 2), i64::MAX)));
        assert_eq!(
            dm("2026-01..2026-03"),
            Some((local(2026, 1, 1), local(2026, 4, 1)))
        );
        assert_eq!(dm("13/40"), None);
    }

    #[test]
    fn accepts_checks_every_filter() {
        let q = Query::parse("ext:pdf size:>1kb dm:2026", tw());
        let in_2026 = local(2026, 6, 1);
        assert!(q.accepts("a.PDF", false, 2048, in_2026));
        assert!(!q.accepts("a.pdf", false, 10, in_2026), "too small");
        assert!(!q.accepts("a.txt", false, 2048, in_2026), "extension");
        assert!(!q.accepts("a.pdf", false, 2048, local(2025, 6, 1)), "date");
        assert!(!q.accepts("a.pdf", false, 2048, 0), "unknown date");
        assert!(
            !q.accepts(".pdf", false, 2048, in_2026),
            "a leading dot alone is not an extension"
        );
        let folders = Query::parse("folder:", tw());
        assert!(folders.accepts("src", true, 0, 0));
        assert!(!folders.accepts("a.rs", false, 1, 0));
    }

    #[test]
    fn type_and_content_terms() {
        let q = Query::parse("type:image;document report", tw());
        assert_eq!(q.types, [TypeFilter::Image, TypeFilter::Document]);
        assert_eq!(q.text, "report");
        assert!(q.accepts("a.JPG", false, 1, 1));
        assert!(
            q.accepts("notes.md", false, 1, 1),
            "text counts as a document"
        );
        assert!(!q.accepts("a.rs", false, 1, 1));
        assert!(!q.accepts("photos", true, 0, 1), "folders are not images");
        let folders = Query::parse("type:folder", tw());
        assert!(folders.accepts("photos", true, 0, 0));
        assert!(!folders.accepts("a.jpg", false, 1, 0));
        // Unknown type words stay name text.
        assert_eq!(Query::parse("type:banana", tw()).text, "type:banana");

        let q = Query::parse(r#"ext:txt content:"due date" memo"#, tw());
        assert_eq!(q.content_needle(), Some("due date"));
        assert_eq!(q.text, "memo");
        assert_eq!(q.ext, ["txt"]);
        assert!(!q.accepts("docs", true, 0, 1), "folders have no contents");
        let q = Query::parse("content:", tw());
        assert_eq!(q.content.as_deref(), Some(""));
        assert_eq!(q.content_needle(), None, "still being typed");
        assert!(q.is_empty());
        assert!(!Query::parse("content:x", tw()).is_empty());
    }

    #[test]
    fn chips_read_and_write_the_text() {
        // Writing replaces the facet's term in place and keeps everything else.
        let input = "report dm:7d size:<1kb";
        assert_eq!(
            set_facet(input, Facet::Size, Some(SizePreset::Over1Gb.term())),
            "report dm:7d size:>1gb"
        );
        assert_eq!(
            set_facet(input, Facet::Date, None),
            "report size:<1kb",
            "removing a chip removes its term"
        );
        assert_eq!(
            set_facet("report", Facet::Type, Some("type:image")),
            "report type:image"
        );
        assert_eq!(
            set_facet("a ext:pdf type:code b", Facet::Type, Some("type:video")),
            "a type:video b",
            "every type term is replaced by one"
        );

        // Reading: presets are recognized however they were typed, the rest shows as typed.
        assert_eq!(
            type_state("x type:IMAGE"),
            FacetState::Preset(TypeFilter::Image)
        );
        assert_eq!(
            type_state("folder:"),
            FacetState::Preset(TypeFilter::Folder)
        );
        assert_eq!(type_state("*.pdf"), FacetState::Custom("*.pdf".into()));
        assert_eq!(type_state("report"), FacetState::Off);
        assert_eq!(
            size_state("size:>1MB"),
            FacetState::Preset(SizePreset::Over1Mb)
        );
        assert_eq!(
            size_state("size:>2mb"),
            FacetState::Custom("size:>2mb".into())
        );
        assert_eq!(
            date_state("dm:thisweek", tw()),
            FacetState::Preset(DatePreset::ThisWeek)
        );
        rust_i18n::set_locale("zh-TW");
        assert_eq!(
            date_state("dm:今天", tw()),
            FacetState::Preset(DatePreset::Today),
            "UI-language words light the chip too"
        );
        rust_i18n::set_locale("en");
        assert_eq!(
            date_state("dm:7d", tw()),
            FacetState::Custom("dm:7d".into())
        );
        assert_eq!(size_state("size:abc"), FacetState::Off, "not a size term");
    }

    #[test]
    fn content_toggle_moves_the_name_text() {
        assert_eq!(
            set_content_search("due date ext:txt", true),
            r#"ext:txt content:"due date""#
        );
        assert_eq!(set_content_search("ext:txt", true), r#"ext:txt content:"""#);
        assert!(has_content_term(r#"content:"""#));
        assert_eq!(
            set_content_search(r#"ext:txt content:"due date""#, false),
            "ext:txt due date"
        );
        assert_eq!(set_content_search(r#"content:"""#, false), "");
        assert_eq!(without_content(r#"a content:"x y" b"#), "a b");
        // Quoted terms stay whole when other chips edit the text.
        assert_eq!(
            set_facet(r#"content:"x y" size:empty"#, Facet::Size, None),
            r#"content:"x y""#
        );
    }
}
