//! Release notes in English, Traditional and Simplified Chinese: the changeset format and the
//! `CHANGELOG*.md` files. Shared by xtask (which writes them) and Arke's What's new page
//! (`crates/app/src/whats_new.rs`, which reads them), so both agree on the format.
//!
//! A changeset is English first, then a block per translation:
//!
//! ```md
//! ---
//! default: minor
//! ---
//!
//! Tabs can be reordered by dragging
//!
//! <!-- zh-TW -->
//! 分頁可以拖曳排序
//!
//! <!-- zh-CN -->
//! 标签页可以拖动排序
//! ```

use std::fmt::Write as _;

/// Languages of the notes. English is the source and the fallback.
pub(crate) const LANGS: [&str; 3] = ["en", "zh-TW", "zh-CN"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Bump {
    Patch,
    Minor,
    Major,
}

impl Bump {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "patch" => Some(Self::Patch),
            "minor" => Some(Self::Minor),
            "major" => Some(Self::Major),
            _ => None,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }

    /// Changelog heading for this kind of change.
    pub(crate) fn heading(self, lang: &str) -> &'static str {
        match (self, lang) {
            (Self::Major, "zh-TW") => "重大變更",
            (Self::Major, "zh-CN") => "重大变更",
            (Self::Major, _) => "Breaking changes",
            (Self::Minor, "zh-TW" | "zh-CN") => "新功能",
            (Self::Minor, _) => "Features",
            (Self::Patch, "zh-TW") => "修正",
            (Self::Patch, "zh-CN") => "修复",
            (Self::Patch, _) => "Fixes",
        }
    }
}

/// One changeset: its bump and its text in each language it was written in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeText {
    pub(crate) bump: Bump,
    /// `(language, text)`, English first.
    texts: Vec<(String, String)>,
}

impl ChangeText {
    /// Reads `---\ndefault: minor\n---\ntext`; with several packages the largest bump wins.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
        let rest = text.strip_prefix("---\n")?;
        let (front, body) = rest.split_once("\n---")?;
        let bump = front
            .lines()
            .filter_map(|l| l.split_once(':'))
            .filter_map(|(_, v)| Bump::parse(v))
            .max()?;
        let mut texts: Vec<(String, String)> = vec![("en".into(), String::new())];
        for line in body.trim_start_matches('-').lines() {
            if let Some(lang) = marker(line) {
                texts.push((lang.into(), String::new()));
            } else if let Some((_, text)) = texts.last_mut() {
                text.push_str(line);
                text.push('\n');
            }
        }
        for (_, text) in &mut texts {
            *text = text.trim().to_string();
        }
        texts.retain(|(lang, text)| lang == "en" || !text.is_empty());
        Some(Self { bump, texts })
    }

    /// The text in `lang`, or the English one when it wasn't translated.
    pub(crate) fn text(&self, lang: &str) -> &str {
        self.texts
            .iter()
            .find(|(l, _)| l == lang)
            .or_else(|| self.texts.first())
            .map_or("", |(_, t)| t.as_str())
    }

    /// Whether the change was written in `lang`.
    pub(crate) fn has(&self, lang: &str) -> bool {
        self.texts.iter().any(|(l, t)| l == lang && !t.is_empty())
    }

    /// The changeset file's contents.
    pub(crate) fn to_file(bump: Bump, texts: &[(&str, &str)]) -> String {
        let mut out = format!("---\ndefault: {}\n---\n", bump.name());
        for (lang, text) in texts {
            if *lang != "en" {
                let _ = write!(out, "\n<!-- {lang} -->");
            }
            let _ = write!(out, "\n{}\n", text.trim());
        }
        out
    }
}

/// `<!-- zh-TW -->` starts the Traditional Chinese text.
fn marker(line: &str) -> Option<&'static str> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    LANGS[1..].iter().copied().find(|l| *l == inner)
}

/// File name of the changelog in `lang`.
pub(crate) fn changelog_file(lang: &str) -> &'static str {
    match lang {
        "zh-TW" => "CHANGELOG.zh-TW.md",
        "zh-CN" => "CHANGELOG.zh-CN.md",
        _ => "CHANGELOG.md",
    }
}

/// What a new changelog in `lang` starts with.
pub(crate) fn changelog_header(lang: &str) -> &'static str {
    match lang {
        "zh-TW" => {
            "# 更新紀錄\n\n由 `cargo xtask version` 從 `.changeset/` 的檔案產生；英文版是 [CHANGELOG.md](CHANGELOG.md)。\n\n"
        }
        "zh-CN" => {
            "# 更新日志\n\n由 `cargo xtask version` 根据 `.changeset/` 中的文件生成；英文版是 [CHANGELOG.md](CHANGELOG.md)。\n\n"
        }
        _ => {
            "# Changelog\n\nGenerated by `cargo xtask version` from the files in `.changeset/`.\n\n"
        }
    }
}

/// `changes` grouped by major, minor and patch, in `lang`.
pub(crate) fn section(changes: &[&ChangeText], lang: &str) -> String {
    let mut out = String::new();
    for bump in [Bump::Major, Bump::Minor, Bump::Patch] {
        let items: Vec<&&ChangeText> = changes.iter().filter(|c| c.bump == bump).collect();
        if items.is_empty() {
            continue;
        }
        let _ = write!(out, "### {}\n\n", bump.heading(lang));
        for c in items {
            let mut lines = c.text(lang).lines();
            let _ = writeln!(out, "- {}", lines.next().unwrap_or_default());
            for line in lines {
                let _ = writeln!(out, "  {line}");
            }
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// One version in a changelog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Release {
    pub(crate) version: String,
    pub(crate) date: Option<String>,
    pub(crate) blocks: Vec<Block>,
}

/// A piece of a version's notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Block {
    /// `### Features`
    Heading(String),
    /// `- item`, continuation lines joined
    Item(String),
    /// Any other paragraph
    Text(String),
}

/// The versions in a changelog, newest first as written: `## 0.3.0 (2026-11-13)` sections.
pub(crate) fn parse_changelog(md: &str) -> Vec<Release> {
    let md = md.replace("\r\n", "\n");
    let mut out = Vec::new();
    for part in md.split("\n## ").skip(1) {
        let (head, body) = part.split_once('\n').unwrap_or((part, ""));
        let (version, date) = match head.split_once(" (") {
            Some((v, d)) => (v.trim(), Some(d.trim_end_matches(')').trim().to_string())),
            None => (head.trim(), None),
        };
        out.push(Release {
            version: version.to_string(),
            date,
            blocks: parse_blocks(body),
        });
    }
    out
}

/// Headings, items and paragraphs of a notes body (a changelog section or a GitHub release).
pub(crate) fn parse_blocks(body: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    // Whether the last block can take a continuation line.
    let mut open = false;
    for line in body.replace("\r\n", "\n").lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            open = false;
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("### ") {
            out.push(Block::Heading(h.trim().to_string()));
            open = false;
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            out.push(Block::Item(item.trim().to_string()));
            open = true;
        } else if open && let Some(Block::Item(text) | Block::Text(text)) = out.last_mut() {
            join(text, trimmed);
        } else {
            out.push(Block::Text(trimmed.to_string()));
            open = true;
        }
    }
    out
}

/// Appends a wrapped line: a space between words, nothing between CJK characters.
fn join(text: &mut String, line: &str) {
    let ascii_edge = text.chars().last().is_some_and(|c| c.is_ascii())
        || line.starts_with(|c: char| c.is_ascii());
    if ascii_edge {
        text.push(' ');
    }
    text.push_str(line);
}

/// Markdown inline markup dropped for display: `**bold**`, `` `code` `` and `[text](url)`.
pub(crate) fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after
            .split_once("](")
            .and_then(|(label, tail)| tail.find(')').map(|close| (label, &tail[close + 1..])))
        {
            Some((label, tail)) if !label.contains('[') => {
                out.push_str(label);
                rest = tail;
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "")
}

/// `0.3.0`, `0.4.0-rc.1` -> `((0, 3, 0), stable?)`.
pub(crate) fn triple(version: &str) -> Option<((u64, u64, u64), bool)> {
    let (base, pre) = match version.split_once('-') {
        Some((b, _)) => (b, true),
        None => (version, false),
    };
    let mut parts = base.trim().split('.').map(|p| p.parse::<u64>().ok());
    let t = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some((t, !pre))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn reads_changesets_like_knope() {
        let c =
            ChangeText::parse("---\ndefault: minor\n---\n\nTabs can be reordered by dragging\n")
                .unwrap();
        assert_eq!(c.bump, Bump::Minor);
        assert_eq!(c.text("en"), "Tabs can be reordered by dragging");
        assert!(ChangeText::parse("no front matter").is_none());
        // Windows line endings; with several packages the largest bump wins.
        let c = ChangeText::parse("---\r\na: patch\r\nb: major\r\n---\r\nx\r\n").unwrap();
        assert_eq!(c.bump, Bump::Major);
    }

    #[test]
    fn changesets_carry_translations_and_fall_back_to_english() {
        let file = ChangeText::to_file(
            Bump::Patch,
            &[
                ("en", "Fixed it"),
                ("zh-TW", "修好了"),
                ("zh-CN", "修好了 "),
            ],
        );
        let c = ChangeText::parse(&file).unwrap();
        assert_eq!(c.bump, Bump::Patch);
        assert_eq!(c.text("en"), "Fixed it");
        assert_eq!(c.text("zh-TW"), "修好了");
        assert_eq!(c.text("zh-CN"), "修好了");
        let only_en = ChangeText::parse("---\ndefault: patch\n---\n\nFixed it\n").unwrap();
        assert!(!only_en.has("zh-TW"));
        assert_eq!(only_en.text("zh-TW"), "Fixed it");
        // An empty translation block counts as missing.
        let empty = ChangeText::parse("---\ndefault: patch\n---\nA\n\n<!-- zh-CN -->\n\n").unwrap();
        assert!(!empty.has("zh-CN"));
    }

    #[test]
    fn sections_are_grouped_and_localized() {
        let fix =
            ChangeText::parse("---\ndefault: patch\n---\nA fix\n<!-- zh-TW -->\n修正一項").unwrap();
        let feat = ChangeText::parse("---\ndefault: minor\n---\nA feature").unwrap();
        assert_eq!(
            section(&[&fix, &feat], "en"),
            "### Features\n\n- A feature\n\n### Fixes\n\n- A fix"
        );
        assert_eq!(
            section(&[&fix, &feat], "zh-TW"),
            "### 新功能\n\n- A feature\n\n### 修正\n\n- 修正一項"
        );
    }

    #[test]
    fn changelogs_read_back_as_versions_and_blocks() {
        let md = "# Changelog\n\nIntro.\n\n## 0.3.0 (2026-11-13)\n\n### Features\n\n- One\n- Two\n  wrapped\n\n## 0.1.0\n\nFirst release.\n\n- **Bold** item\n";
        let releases = parse_changelog(md);
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].version, "0.3.0");
        assert_eq!(releases[0].date.as_deref(), Some("2026-11-13"));
        assert_eq!(
            releases[0].blocks,
            [
                Block::Heading("Features".into()),
                Block::Item("One".into()),
                Block::Item("Two wrapped".into()),
            ]
        );
        assert_eq!(releases[1].date, None);
        assert_eq!(
            releases[1].blocks,
            [
                Block::Text("First release.".into()),
                Block::Item("**Bold** item".into()),
            ]
        );
    }

    #[test]
    fn wrapped_chinese_lines_join_without_a_space() {
        let blocks = parse_blocks("- 第一行\n  第二行\n- word\n  next");
        assert_eq!(
            blocks,
            [
                Block::Item("第一行第二行".into()),
                Block::Item("word next".into())
            ]
        );
    }

    #[test]
    fn markup_is_dropped_for_display() {
        assert_eq!(
            plain("**Never freezes.** Run `arke --uninstall`"),
            "Never freezes. Run arke --uninstall"
        );
        assert_eq!(
            plain("[arke.exe](https://example.com/x) · [b](u)"),
            "arke.exe · b"
        );
        assert_eq!(plain("a [bracket] stays"), "a [bracket] stays");
    }

    #[test]
    fn versions_compare_by_triple_and_prerelease() {
        assert_eq!(triple("0.3.0"), Some(((0, 3, 0), true)));
        assert_eq!(triple("0.4.0-rc.1"), Some(((0, 4, 0), false)));
        assert_eq!(triple("0.4"), None);
        assert_eq!(triple("x.y.z"), None);
    }

    /// The changelogs in the repo parse, every version is in all three, and the Chinese ones use
    /// their own headings.
    #[test]
    fn the_changelogs_in_the_repo_match() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read = |lang: &str| {
            parse_changelog(&std::fs::read_to_string(root.join(changelog_file(lang))).unwrap())
        };
        let en = read("en");
        assert!(!en.is_empty());
        for lang in &LANGS[1..] {
            let other = read(lang);
            let versions = |r: &[Release]| r.iter().map(|r| r.version.clone()).collect::<Vec<_>>();
            assert_eq!(versions(&other), versions(&en), "{lang}");
            for (a, b) in en.iter().zip(&other) {
                let shape = |r: &Release| {
                    r.blocks
                        .iter()
                        .map(|b| matches!(b, Block::Item(_)))
                        .collect::<Vec<_>>()
                };
                assert_eq!(shape(b), shape(a), "{lang} {}: same items", a.version);
                for block in &b.blocks {
                    if let Block::Heading(h) = block {
                        assert!(
                            [Bump::Major, Bump::Minor, Bump::Patch]
                                .iter()
                                .any(|k| k.heading(lang) == h),
                            "{lang} {}: heading {h}",
                            a.version
                        );
                    }
                }
            }
        }
    }
}
