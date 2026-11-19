//! What's new: which release notes to show, and when.
//!
//! The notes come from the changelogs built into the exe (`CHANGELOG.md`, `.zh-TW.md`,
//! `.zh-CN.md`), in the UI language, so the page works offline and matches this build exactly. An
//! RC or nightly build also carries its pending changesets (`build.rs`), since its changes aren't in
//! the changelogs yet.
//!
//! The first time a stable or RC build runs after an update, the page opens once
//! (`Settings::whats_new_seen` remembers the version). Not on a first install, not for nightly
//! builds (they come daily) or dev builds, and not when Settings › About turns it off. The page
//! is in `workspace_whats_new.rs`.

#[allow(
    dead_code,
    reason = "the half that writes changesets and changelogs is for xtask"
)]
#[path = "../../../tools/xtask/src/notes.rs"]
mod notes;

pub(crate) use notes::{Block, Release};

use crate::build_info::{self, Channel};

const CHANGELOGS: [(&str, &str); 3] = [
    ("en", include_str!("../../../CHANGELOG.md")),
    ("zh-TW", include_str!("../../../CHANGELOG.zh-TW.md")),
    ("zh-CN", include_str!("../../../CHANGELOG.zh-CN.md")),
];

/// Changesets of an RC or nightly build, separated by U+001E lines; empty otherwise.
const PENDING: &str = include_str!(concat!(env!("OUT_DIR"), "/pending_changesets.txt"));

/// Language of the notes for the current UI language.
pub(crate) fn notes_lang() -> &'static str {
    match rust_i18n::locale().as_ref() {
        "zh-TW" => "zh-TW",
        "zh-CN" => "zh-CN",
        _ => "en",
    }
}

/// Released versions in `lang`, newest first.
pub(crate) fn releases(lang: &str) -> Vec<Release> {
    let text = CHANGELOGS
        .iter()
        .find(|(l, _)| *l == lang)
        .map_or(CHANGELOGS[0].1, |(_, t)| t);
    notes::parse_changelog(text)
}

/// This build's own changes when it is an RC or nightly (`None` for stable and dev builds).
pub(crate) fn this_build(lang: &str) -> Option<Release> {
    pending_release(PENDING, build_info::VERSION, lang)
}

fn pending_release(pending: &str, version: &str, lang: &str) -> Option<Release> {
    let changes: Vec<notes::ChangeText> = pending
        .split("\n\u{1e}\n")
        .filter_map(notes::ChangeText::parse)
        .collect();
    if changes.is_empty() {
        return None;
    }
    let refs: Vec<&notes::ChangeText> = changes.iter().collect();
    Some(Release {
        version: version.to_string(),
        date: None,
        blocks: notes::parse_blocks(&notes::section(&refs, lang)),
    })
}

/// A new version's notes as GitHub has them (English), for an update that isn't installed yet.
pub(crate) fn available(version: &str, notes_md: &str) -> Release {
    let blocks = notes::parse_blocks(notes_md)
        .into_iter()
        // The release page adds VirusTotal links under the notes; they aren't changes.
        .take_while(|b| !matches!(b, Block::Text(t) if t.starts_with("VirusTotal")))
        .collect();
    Release {
        version: version.to_string(),
        date: None,
        blocks,
    }
}

/// The new version's notes in `lang` from that language's changelog as it is in the release
/// (`localized`, fetched by [`fetch_localized_changelog`]), so they read the same before the
/// update as What's new does after it. `None` when the text is missing, doesn't have the version
/// or has nothing under it: the caller keeps the English release notes.
pub(crate) fn available_in(version: &str, localized: Option<&str>) -> Option<Release> {
    let release = notes::parse_changelog(localized?)
        .into_iter()
        .find(|r| r.version == version)?;
    (!release.blocks.is_empty()).then_some(release)
}

/// URL of the changelog in `lang` at release tag `v<version>`. `None` for English, whose notes
/// are the release body already.
fn localized_changelog_url(repo: &str, lang: &str, version: &str) -> Option<String> {
    (lang != "en").then(|| {
        format!(
            "https://raw.githubusercontent.com/{repo}/v{version}/{}",
            changelog_file(lang)
        )
    })
}

/// Fetches the changelog in `lang` at the tag of `version` from GitHub. `None` for English or
/// when anything goes wrong (offline, no such tag). Blocks on the network: background thread.
#[cfg(not(test))]
pub(crate) fn fetch_localized_changelog(lang: &str, version: &str) -> Option<String> {
    let url = localized_changelog_url(build_info::RELEASES_REPO, lang, version)?;
    crate::win_http::get_text(&url, &[]).ok()
}

/// Tests have no network: they get what [`set_localized_changelog_for_test`] put in.
#[cfg(test)]
pub(crate) fn fetch_localized_changelog(lang: &str, version: &str) -> Option<String> {
    localized_changelog_url("Koimsurai/Arke", lang, version)?;
    TEST_CHANGELOG.lock().clone()
}

#[cfg(test)]
static TEST_CHANGELOG: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_localized_changelog_for_test(text: Option<&str>) {
    *TEST_CHANGELOG.lock() = text.map(str::to_owned);
}

/// The changelog file in `lang` (for its link on GitHub).
pub(crate) fn changelog_file(lang: &str) -> &'static str {
    notes::changelog_file(lang)
}

/// Text of a block without Markdown markup.
pub(crate) fn plain(text: &str) -> String {
    notes::plain(text)
}

/// Orders versions: by number, and a prerelease before its stable release.
fn key(version: &str) -> Option<((u64, u64, u64), bool)> {
    notes::triple(version)
}

/// Released versions after `seen` up to `current`, newest first. Without `seen` (the version
/// before this one didn't record it), only `current` itself.
pub(crate) fn since(all: &[Release], seen: Option<&str>, current: &str) -> Vec<Release> {
    let Some((now, _)) = key(current) else {
        return Vec::new();
    };
    let after = seen.and_then(key);
    all.iter()
        .filter(|r| {
            let Some((v, _)) = key(&r.version) else {
                return false;
            };
            let newer = match after {
                // A prerelease of a version comes before that version's release.
                Some((seen, seen_stable)) => v > seen || (v == seen && !seen_stable),
                None => v == now,
            };
            newer && v <= now
        })
        .cloned()
        .collect()
}

/// What to do about What's new when Arke starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Startup {
    /// Nothing changed (or a dev build): leave the setting alone.
    Nothing,
    /// Remember this version without showing anything.
    Record,
    /// Show what changed since the version Arke remembers (if any), then remember this one.
    Show,
}

/// Decides [`Startup`] for this run.
pub(crate) fn on_startup(
    seen: Option<&str>,
    first_run: bool,
    current: &str,
    channel: Option<Channel>,
    auto: bool,
) -> Startup {
    let Some(channel) = channel else {
        return Startup::Nothing;
    };
    if seen == Some(current) {
        return Startup::Nothing;
    }
    let downgrade = seen
        .and_then(key)
        .zip(key(current))
        .is_some_and(|(seen, now)| now < seen);
    if first_run || downgrade || channel == Channel::Nightly || !auto {
        Startup::Record
    } else {
        Startup::Show
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(version: &str) -> Release {
        Release {
            version: version.into(),
            date: None,
            blocks: Vec::new(),
        }
    }

    fn versions(r: &[Release]) -> Vec<&str> {
        r.iter().map(|r| r.version.as_str()).collect()
    }

    #[test]
    fn shows_the_versions_since_the_one_last_run() {
        let all = [
            rel("0.4.0"),
            rel("0.3.0"),
            rel("0.2.0"),
            rel("0.1.1"),
            rel("0.1.0"),
        ];
        assert_eq!(
            versions(&since(&all, Some("0.1.1"), "0.3.0")),
            ["0.3.0", "0.2.0"]
        );
        assert_eq!(
            versions(&since(&all, Some("0.3.0"), "0.3.0")),
            Vec::<&str>::new()
        );
        // Unknown last version (it was before this was recorded): only this one.
        assert_eq!(versions(&since(&all, None, "0.3.0")), ["0.3.0"]);
        // From an RC to its release: the release is new.
        assert_eq!(
            versions(&since(&all, Some("0.4.0-rc.2"), "0.4.0")),
            ["0.4.0"]
        );
        // An RC of the next version: nothing released yet (its own notes come from the build).
        assert_eq!(
            versions(&since(&all, Some("0.3.0"), "0.5.0-rc.1")),
            ["0.4.0"]
        );
    }

    #[test]
    fn opens_once_after_an_update_and_never_for_nightly_dev_or_a_new_install() {
        use Startup::{Nothing, Record, Show};
        let stable = Some(Channel::Stable);
        assert_eq!(
            on_startup(Some("0.2.0"), false, "0.3.0", stable, true),
            Show
        );
        assert_eq!(
            on_startup(None, false, "0.3.0", stable, true),
            Show,
            "updated from 0.3.0 or older"
        );
        assert_eq!(
            on_startup(Some("0.3.0"), false, "0.3.0", stable, true),
            Nothing
        );
        assert_eq!(
            on_startup(None, true, "0.3.0", stable, true),
            Record,
            "first install"
        );
        assert_eq!(
            on_startup(Some("0.4.0"), false, "0.3.0", stable, true),
            Record,
            "downgrade"
        );
        assert_eq!(
            on_startup(Some("0.2.0"), false, "0.3.0", stable, false),
            Record,
            "turned off"
        );
        assert_eq!(
            on_startup(Some("0.3.0"), false, "0.4.0-rc.1", Some(Channel::Rc), true),
            Show
        );
        assert_eq!(
            on_startup(
                Some("0.3.0"),
                false,
                "0.4.0-nightly.20261120",
                Some(Channel::Nightly),
                true
            ),
            Record
        );
        assert_eq!(
            on_startup(Some("0.2.0"), false, "0.3.0", None, true),
            Nothing,
            "dev build"
        );
    }

    #[test]
    fn an_rc_carries_its_pending_changesets_in_the_ui_language() {
        let pending = "---\ndefault: minor\n---\n\nA feature\n\n<!-- zh-TW -->\n新功能一\n\n<!-- zh-CN -->\n新功能一\n\u{0a}\u{1e}\n---\ndefault: patch\n---\n\nA fix\n";
        let r = pending_release(pending, "0.4.0-rc.1", "zh-TW").unwrap();
        assert_eq!(r.version, "0.4.0-rc.1");
        assert_eq!(
            r.blocks,
            [
                Block::Heading("新功能".into()),
                Block::Item("新功能一".into()),
                Block::Heading("修正".into()),
                // Not translated: English rather than nothing.
                Block::Item("A fix".into()),
            ]
        );
        assert_eq!(pending_release("", "0.4.0", "en"), None);
    }

    #[test]
    fn an_available_update_drops_the_virustotal_links() {
        let body = "### Features\n\n- New thing\n\n### Fixes\n\n- A fix\n\n\nVirusTotal scans of this release:\n[arke.exe](https://www.virustotal.com/gui/file/x)";
        let r = available("0.4.0", body);
        assert_eq!(
            r.blocks,
            [
                Block::Heading("Features".into()),
                Block::Item("New thing".into()),
                Block::Heading("Fixes".into()),
                Block::Item("A fix".into()),
            ]
        );
    }

    const ZH: &str = "# 變更記錄\n\n## 0.7.1 (2026-11-20)\n\n### 修正\n\n- 修好圖示\n\n## 0.7.0 (2026-11-10)\n\n### 新功能\n\n- 首頁\n";

    #[test]
    fn the_new_versions_notes_come_from_the_localized_changelog() {
        let r = available_in("0.7.1", Some(ZH)).expect("section");
        assert_eq!(r.version, "0.7.1");
        assert_eq!(
            r.blocks,
            [
                Block::Heading("修正".into()),
                Block::Item("修好圖示".into())
            ]
        );
    }

    #[test]
    fn missing_or_unusable_localized_notes_fall_back_to_english() {
        // Fetch failed, the tag has no such version (an RC), or the section is empty.
        assert_eq!(available_in("0.7.1", None), None);
        assert_eq!(available_in("0.7.2-rc.1", Some(ZH)), None);
        assert_eq!(
            available_in("0.7.1", Some("## 0.7.1 (2026-11-20)\n\n")),
            None
        );
        assert_eq!(available_in("0.7.1", Some("not a changelog")), None);
    }

    #[test]
    fn the_changelog_is_fetched_from_the_release_tag_only_for_translated_languages() {
        let url = |lang| localized_changelog_url("Koimsurai/Arke", lang, "0.7.1");
        assert_eq!(
            url("zh-TW").as_deref(),
            Some("https://raw.githubusercontent.com/Koimsurai/Arke/v0.7.1/CHANGELOG.zh-TW.md")
        );
        assert_eq!(
            url("zh-CN").as_deref(),
            Some("https://raw.githubusercontent.com/Koimsurai/Arke/v0.7.1/CHANGELOG.zh-CN.md")
        );
        assert_eq!(url("en"), None);
    }

    #[test]
    fn the_built_in_notes_cover_this_version_in_every_language() {
        let version = env!("CARGO_PKG_VERSION");
        for lang in notes::LANGS {
            let all = releases(lang);
            assert!(
                all.iter().any(|r| r.version == version),
                "{lang}: {version} is in the changelog"
            );
        }
    }
}
