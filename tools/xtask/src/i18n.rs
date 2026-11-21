//! Translation helper: `cargo xtask i18n <status|missing|merge>`.
//!
//! The UI text lives in `crates/*/locales/*.yml` (rust-i18n, `_version: 2`): under each key, one
//! line per language. Adding a language by hand means editing the same files key by key, so this
//! works on flat files instead:
//!
//! ```text
//! cargo xtask i18n status                      keys per language, per locale file
//! cargo xtask i18n missing <lang> [--out F] [--with zh-TW]
//!                                              the keys that lack <lang>, one per line:
//!                                              key TAB English [TAB --with text]
//! cargo xtask i18n merge <lang> <file>         insert the translations (key TAB text) under
//!                                              each key, after its last language line
//! ```
//!
//! Newlines inside a text are written as `⏎` in the flat files. Merging keeps the files'
//! comments and key order, leaves other keys alone, replaces a language line that already
//! exists, and refuses a translation whose `%{placeholders}` differ from the English text.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

/// Stands for a newline in the flat files.
const NEWLINE: char = '⏎';

/// One key of a locale file.
#[derive(Debug)]
struct Entry {
    key: String,
    /// `(language, decoded text)` in file order.
    langs: Vec<(String, String)>,
    /// Index into the file's lines of each of `langs`' lines (comments or blank lines may sit
    /// between them).
    at: Vec<usize>,
    /// Index into the file's lines of the first line after this key's language lines.
    end: usize,
}

impl Entry {
    fn text(&self, lang: &str) -> Option<&str> {
        self.langs
            .iter()
            .find(|(l, _)| l == lang)
            .map(|(_, t)| t.as_str())
    }
}

/// Parses a locale file's lines into its keys. Anything that isn't a comment, blank line,
/// `_version`, key or `  lang: value` line is an error, so the editor can't silently skip text.
fn parse(lines: &[&str]) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with('#')
            || line.starts_with("_version")
            || line.trim().is_empty()
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("  ") {
            let entry = entries
                .last_mut()
                .ok_or_else(|| anyhow!("line {}: text before any key", i + 1))?;
            let (lang, value) = rest
                .split_once(':')
                .ok_or_else(|| anyhow!("line {}: expected `lang: text`", i + 1))?;
            ensure!(
                !lang.is_empty() && !lang.contains(' '),
                "line {}: bad language",
                i + 1
            );
            entry.langs.push((
                lang.to_owned(),
                decode(value.trim_start()).with_context(|| format!("line {}", i + 1))?,
            ));
            entry.at.push(i);
            entry.end = i + 1;
        } else if let Some(key) = line.strip_suffix(':') {
            entries.push(Entry {
                key: key.to_owned(),
                langs: Vec::new(),
                at: Vec::new(),
                end: i + 1,
            });
        } else {
            bail!("line {}: unexpected `{line}`", i + 1);
        }
    }
    Ok(entries)
}

/// The text of a YAML scalar: plain, `"double"` (with `\\ \" \n \t \uXXXX`) or `'single'`.
fn decode(value: &str) -> Result<String> {
    if let Some(inner) = value.strip_prefix('\'') {
        let inner = inner
            .strip_suffix('\'')
            .ok_or_else(|| anyhow!("unterminated quote"))?;
        return Ok(inner.replace("''", "'"));
    }
    let Some(inner) = value.strip_prefix('"') else {
        return Ok(value.to_owned());
    };
    let inner = inner
        .strip_suffix('"')
        .ok_or_else(|| anyhow!("unterminated quote"))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(c @ ('\\' | '"' | '/')) => out.push(c),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let c = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| anyhow!("bad \\u escape"))?;
                out.push(c);
            }
            other => bail!("unsupported escape \\{}", other.unwrap_or(' ')),
        }
    }
    Ok(out)
}

/// Text a YAML parser may read as a number or date: `0x1F`, `0o17`, `1_000`, `1e3`, `10:30`,
/// `2026-11-12`. Anything starting with a digit made only of such characters.
fn number_like(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| {
            c.is_ascii_hexdigit()
                || matches!(
                    c,
                    'x' | 'X' | 'o' | 'O' | 't' | 'T' | '_' | '.' | '+' | '-' | ':'
                )
        })
}

/// A YAML scalar for `text`: plain when that reads back as the same string, double-quoted
/// otherwise.
fn encode(text: &str) -> String {
    const WORDS: [&str; 10] = [
        "y", "n", "yes", "no", "on", "off", "true", "false", "null", "~",
    ];
    let first = text.chars().next();
    let plain = first.is_some_and(char::is_alphanumeric)
        && !text.ends_with(' ')
        && !text.ends_with(':')
        && !text.contains(": ")
        && !text.contains(" #")
        && !text.contains(['"', '\'', '\\', '\n', '\t'])
        && !WORDS.contains(&text.to_ascii_lowercase().as_str())
        && text.parse::<f64>().is_err()
        && !number_like(text);
    if plain {
        return text.to_owned();
    }
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `%{name}` placeholders of a text.
fn placeholders(text: &str) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find("%{") {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        found.insert(&rest[start..=start + len]);
        rest = &rest[start + len + 1..];
    }
    found
}

/// The locale files under `crates/*/locales/`, sorted.
fn locale_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for krate in std::fs::read_dir(root.join("crates")).context("crates/")? {
        let dir = krate?.path().join("locales");
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in read {
            let path = file?.path();
            if path.extension().is_some_and(|e| e == "yml") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn read_lines(path: &Path) -> Result<(String, String)> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    Ok((text, eol.to_owned()))
}

fn flat(text: &str) -> String {
    text.replace('\n', &NEWLINE.to_string())
}

fn unflat(text: &str) -> String {
    text.replace(NEWLINE, "\n")
}

fn check_lang(lang: &str) -> Result<()> {
    ensure!(
        !lang.is_empty()
            && lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && lang != "en",
        "`{lang}` is not a language to add (use a code like ja, de, pt-BR)"
    );
    Ok(())
}

/// Per locale file, how many keys each language has.
fn status(root: &Path) -> Result<()> {
    for path in locale_files(root)? {
        let (text, _) = read_lines(&path)?;
        let entries = parse(&text.lines().collect::<Vec<_>>())?;
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (lang, _) in entries.iter().flat_map(|e| &e.langs) {
            *counts.entry(lang).or_default() += 1;
        }
        let name = path.file_name().map_or_default(|n| n.to_string_lossy());
        let langs: Vec<String> = counts.iter().map(|(l, n)| format!("{l} {n}")).collect();
        println!("{name}: {} keys; {}", entries.len(), langs.join(", "));
    }
    Ok(())
}

/// The flat file of keys without `lang`: `key TAB English [TAB context]`.
fn missing(root: &Path, lang: &str, context: Option<&str>) -> Result<String> {
    check_lang(lang)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# key<TAB>English{}. Translate into {lang}; `{NEWLINE}` is a newline.",
        context.map_or_default(|c| format!("<TAB>{c}"))
    );
    for path in locale_files(root)? {
        let (text, _) = read_lines(&path)?;
        for entry in parse(&text.lines().collect::<Vec<_>>())? {
            if entry.text(lang).is_some() {
                continue;
            }
            let en = entry
                .text("en")
                .ok_or_else(|| anyhow!("{} has no English text", entry.key))?;
            let _ = write!(out, "{}\t{}", entry.key, flat(en));
            if let Some(c) = context {
                let _ = write!(out, "\t{}", flat(entry.text(c).unwrap_or_default()));
            }
            out.push('\n');
        }
    }
    Ok(out)
}

/// Inserts `translations` (key to text) as `lang` lines in the locale files. Returns how many
/// were written.
fn merge(root: &Path, lang: &str, translations: &BTreeMap<String, String>) -> Result<usize> {
    check_lang(lang)?;
    let mut left: BTreeSet<&str> = translations.keys().map(String::as_str).collect();
    let mut written = 0;
    for path in locale_files(root)? {
        let (text, eol) = read_lines(&path)?;
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let entries = parse(&text.lines().collect::<Vec<_>>())?;
        // Back to front, so earlier line numbers stay valid while lines are inserted.
        let mut changed = false;
        for entry in entries.iter().rev() {
            let Some(new) = translations.get(&entry.key) else {
                continue;
            };
            left.remove(entry.key.as_str());
            let en = entry.text("en").unwrap_or_default();
            ensure!(
                placeholders(en) == placeholders(new),
                "{}: placeholders {:?} should be {:?}\n  en: {en}\n  {lang}: {new}",
                entry.key,
                placeholders(new),
                placeholders(en)
            );
            let line = format!("  {lang}: {}", encode(new));
            // Replace this language's line when it is there, else add after the last one.
            let existing = entry.langs.iter().position(|(l, _)| l == lang);
            if let Some(at) = existing.and_then(|n| entry.at.get(n)) {
                lines[*at] = line;
            } else {
                lines.insert(entry.end, line);
            }
            written += 1;
            changed = true;
        }
        if changed {
            let mut out = lines.join(&eol);
            out.push_str(&eol);
            std::fs::write(&path, out).with_context(|| path.display().to_string())?;
        }
    }
    ensure!(
        left.is_empty(),
        "keys not found in any locale file: {}",
        left.into_iter().collect::<Vec<_>>().join(", ")
    );
    Ok(written)
}

/// Reads `key TAB text` lines. Blank lines and `#` comments are skipped; extra columns after the
/// text are ignored, so a file from `missing` can be filled in place.
fn read_translations(text: &str) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let (Some(key), Some(value)) = (cols.next(), cols.next()) else {
            bail!("line {}: expected `key<TAB>text`", i + 1);
        };
        ensure!(
            map.insert(key.to_owned(), unflat(value)).is_none(),
            "line {}: {key} appears twice",
            i + 1
        );
    }
    Ok(map)
}

/// `cargo xtask i18n ...`
pub(crate) fn run(root: &Path, args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: cargo xtask i18n status | missing <lang> [--out <file>] [--with <lang>] | merge <lang> <file>";
    match args.first().map(String::as_str) {
        Some("status") => status(root),
        Some("missing") => {
            let lang = args.get(1).ok_or_else(|| anyhow!(USAGE))?;
            let mut out = None;
            let mut context = None;
            let mut rest = args[2..].iter();
            while let Some(flag) = rest.next() {
                let value = rest.next().ok_or_else(|| anyhow!(USAGE))?;
                match flag.as_str() {
                    "--out" => out = Some(value),
                    "--with" => context = Some(value.as_str()),
                    _ => bail!(USAGE),
                }
            }
            let text = missing(root, lang, context)?;
            match out {
                Some(file) => {
                    std::fs::write(file, &text).with_context(|| file.clone())?;
                    println!("{} keys to translate -> {file}", text.lines().count() - 1);
                }
                None => print!("{text}"),
            }
            Ok(())
        }
        Some("merge") => {
            let (Some(lang), Some(file)) = (args.get(1), args.get(2)) else {
                bail!(USAGE);
            };
            let text = std::fs::read_to_string(file).with_context(|| file.clone())?;
            let written = merge(root, lang, &read_translations(&text)?)?;
            println!("{written} {lang} lines written");
            Ok(())
        }
        _ => bail!(USAGE),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a failing test should stop right there")]
mod tests {
    use super::*;

    const FILE: &str = "_version: 2

# a comment
a.one:
  en: One
  zh-TW: 一

# another
a.two:
  en: \"Hi %{name}\\n\"
  zh-TW: '你好 %{name}'
  zh-CN: 你好 %{name}
";

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("arke-xtask-i18n-{name}"));
        let dir = root.join("crates/x/locales");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.yml"), FILE).unwrap();
        root
    }

    #[test]
    fn reads_plain_and_quoted_scalars() {
        assert_eq!(decode("plain: x").unwrap(), "plain: x");
        assert_eq!(decode(r#""a\"b\\c\n""#).unwrap(), "a\"b\\c\n");
        assert_eq!(decode("'it''s'").unwrap(), "it's");
        assert!(decode("\"open").is_err());
    }

    #[test]
    fn writes_scalars_that_read_back_the_same() {
        for text in [
            "Open",
            "Nein",
            "No",
            "10",
            "a: b",
            "%{n} files",
            "C:\\x",
            "say \"hi\"",
            "a\nb",
            "ok #1",
            "Dossier :",
            "0x1F",
            "1_000",
            "0o17",
            "1e3",
            "10:30",
            "2026-11-12",
            ".inf",
            ".NaN",
            "+1",
            "True",
            "Off",
            "NULL",
            "~",
            "Y",
        ] {
            assert_eq!(decode(&encode(text)).unwrap(), text, "{text}");
        }
        assert_eq!(encode("Open"), "Open");
        assert_eq!(encode("No"), "\"No\"");
        for text in [
            "0x1F", "1_000", "0o17", ".inf", "true", "yes", "off", "null", "~",
        ] {
            assert!(encode(text).starts_with('"'), "{text} is quoted");
        }
        assert_eq!(encode("3 files"), "3 files");
        assert_eq!(encode("%{n} files"), "\"%{n} files\"");
    }

    #[test]
    fn finds_placeholders() {
        let found = placeholders("%{a} of %{b}, %USERPROFILE%, %{a}");
        assert_eq!(found, BTreeSet::from(["%{a}", "%{b}"]));
    }

    #[test]
    fn lists_missing_keys_and_merges_them_in_place() {
        let root = temp_root("merge");
        let flat_file = missing(&root, "ja", Some("zh-TW")).unwrap();
        assert!(flat_file.contains("a.one\tOne\t一\n"));
        assert!(flat_file.contains("a.two\tHi %{name}⏎\t你好 %{name}\n"));
        let translations = read_translations("a.one\t一つ\na.two\t%{name} さん⏎\n").unwrap();
        assert_eq!(merge(&root, "ja", &translations).unwrap(), 2);
        let after = std::fs::read_to_string(root.join("crates/x/locales/x.yml")).unwrap();
        assert!(after.contains("  zh-TW: 一\n  ja: 一つ\n\n# another"));
        assert!(after.contains("  zh-CN: 你好 %{name}\n  ja: \"%{name} さん\\n\"\n"));
        assert_eq!(missing(&root, "ja", None).unwrap().lines().count(), 1);
        // A second merge replaces the line instead of adding another.
        let again = read_translations("a.one\t壱\n").unwrap();
        merge(&root, "ja", &again).unwrap();
        let after = std::fs::read_to_string(root.join("crates/x/locales/x.yml")).unwrap();
        assert!(after.contains("  ja: 壱\n") && !after.contains("一つ"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A comment between a key's language lines doesn't make a replacement hit the wrong line.
    #[test]
    fn replaces_the_right_line_when_a_comment_sits_between_languages() {
        let root = std::env::temp_dir().join("arke-xtask-i18n-gap");
        let dir = root.join("crates/x/locales");
        std::fs::create_dir_all(&dir).unwrap();
        let file = "_version: 2
a.one:
  en: One
  # note
  zh-TW: 一

  ja: 古い
  zh-CN: 一
";
        std::fs::write(dir.join("x.yml"), file).unwrap();
        let translations = read_translations(
            "a.one	新しい
",
        )
        .unwrap();
        merge(&root, "ja", &translations).unwrap();
        let after = std::fs::read_to_string(dir.join("x.yml")).unwrap();
        assert_eq!(
            after,
            "_version: 2
a.one:
  en: One
  # note
  zh-TW: 一

  ja: 新しい
  zh-CN: 一
"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_changed_placeholders_and_unknown_keys() {
        let root = temp_root("refuse");
        let bad = read_translations("a.two\tHallo\n").unwrap();
        assert!(merge(&root, "de", &bad).is_err());
        let unknown = read_translations("nope\tx\n").unwrap();
        assert!(merge(&root, "de", &unknown).is_err());
        assert!(missing(&root, "en", None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
