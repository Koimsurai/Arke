//! Arke release tool: `cargo xtask <command>`.
//!
//! ```text
//! cargo xtask change <patch|minor|major> <text> --zh-TW <text> --zh-CN <text>
//!                                                 record a change (.changeset/*.md, knope format,
//!                                                 with the translations; see notes.rs)
//! cargo xtask status                              next version and pending changes
//! cargo xtask version                             bump version from changesets, write the three
//!                                                 changelogs (CHANGELOG.md, .zh-TW.md, .zh-CN.md)
//! cargo xtask release-version <stable|rc|nightly> [--number N]
//!                                                 print the version a build would get
//! cargo xtask i18n <status|missing|merge> ...     translation helper for the locale files (i18n.rs)
//! cargo xtask licenses                            regenerate THIRD_PARTY_LICENSES.md (cargo-about)
//! cargo xtask build <stable|rc|nightly> [--number N] [--arch x64|arm64]
//!                                                 package the exes into dist/ (exe, installer,
//!                                                 each with a .sha256, notes,
//!                                                 THIRD_PARTY_LICENSES.md)
//! cargo xtask publish <stable|rc|nightly> [--number N] [--arch x64|arm64] [--prebuilt]
//!                                                 build (or with --prebuilt take what is in dist/),
//!                                                 then upload to GitHub Releases (repo from
//!                                                 release.toml; needs GITHUB_TOKEN; curl), each
//!                                                 file Arke downloads signed with
//!                                                 MINISIGN_SECRET_KEY (.minisig)
//! cargo xtask sign-check                          check that MINISIGN_SECRET_KEY is the key whose
//!                                                 public half Arke has, without releasing
//! ```
//!
//! Both architectures are built unless `--arch` picks one: `arke-<version>.exe` (x64) and
//! `arke-<version>-arm64.exe`, and with Inno Setup 6.6+ installed (or `ISCC` pointing at
//! `ISCC.exe`) their installers `arke-<version>[-arm64]-setup.exe` (`installer/arke.iss`).
//! `ARKE_REQUIRE_SETUP` makes a missing Inno Setup an error, as the release workflow wants. ARM64 needs the MSVC ARM64 build tools (Visual Studio component
//! "MSVC ARM64/ARM64EC build tools") and `rustup target add aarch64-pc-windows-msvc`.
//!
//! Versions: stable is the `Cargo.toml` version; RC is the version the pending changesets lead to
//! plus `-rc.N`; nightly is that version plus `-nightly.YYYYMMDD`. So 0.2.0-nightly < 0.2.0-rc.1
//! < 0.2.0, which is how the update check compares them.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "command-line tool: prints its output"
)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};

use notes::{Bump, ChangeText, LANGS};

mod i18n;
#[allow(
    dead_code,
    reason = "the reading half is for Arke's What's new page (whats_new.rs)"
)]
mod notes;

fn root() -> PathBuf {
    // tools/xtask -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// On 0.x a major change only bumps the minor version, as in Cargo.
const fn apply(bump: Bump, (major, minor, patch): (u64, u64, u64)) -> (u64, u64, u64) {
    match bump {
        Bump::Major if major == 0 => (0, minor + 1, 0),
        Bump::Major => (major + 1, 0, 0),
        Bump::Minor => (major, minor + 1, 0),
        Bump::Patch => (major, minor, patch + 1),
    }
}

/// One changeset.
struct Change {
    path: PathBuf,
    text: ChangeText,
}

fn changesets() -> Vec<Change> {
    let dir = root().join(".changeset");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter(|p| p.file_name().is_some_and(|n| n != "README.md"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            ChangeText::parse(&text).map(|text| Change { path, text })
        })
        .collect()
}

/// Changesets missing a translation, as `file (languages)`.
fn untranslated(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .filter_map(|c| {
            let missing: Vec<&str> = LANGS[1..]
                .iter()
                .copied()
                .filter(|l| !c.text.has(l))
                .collect();
            let name = c.path.file_name()?.to_string_lossy().into_owned();
            (!missing.is_empty()).then(|| format!("{name} ({})", missing.join(", ")))
        })
        .collect()
}

fn cargo_toml() -> PathBuf {
    root().join("Cargo.toml")
}

/// Workspace version (`[workspace.package] version`).
fn current_version() -> Result<(u64, u64, u64)> {
    let path = cargo_toml();
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("version = \""))
        .context("no version found in Cargo.toml")?;
    parse_triple(line.split('"').nth(1).unwrap_or_default())
}

fn parse_triple(v: &str) -> Result<(u64, u64, u64)> {
    let mut parts = v.split('.').map(str::parse::<u64>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) => Ok((a, b, c)),
        _ => bail!("unrecognized version: {v}"),
    }
}

fn fmt_triple((a, b, c): (u64, u64, u64)) -> String {
    format!("{a}.{b}.{c}")
}

/// Version the pending changesets lead to (patch if there are none).
fn next_version(changes: &[Change]) -> Result<(u64, u64, u64)> {
    let bump = changes
        .iter()
        .map(|c| c.text.bump)
        .max()
        .unwrap_or(Bump::Patch);
    Ok(apply(bump, current_version()?))
}

/// Today's date (UTC) as YYYYMMDD.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (y, m, d) = civil(days);
    format!("{y:04}{m:02}{d:02}")
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(root())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// `repo = "owner/name"` from `release.toml`: the GitHub repo that hosts releases.
fn releases_repo() -> String {
    std::fs::read_to_string(root().join("release.toml"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "repo").then(|| v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_default()
}

fn value_of(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

/// Full license texts of everything in arke.exe, generated by cargo-about from `about.toml` and
/// `about.hbs`. The exe embeds it (Settings › About › Licenses) and each release carries it.
const LICENSES: &str = "THIRD_PARTY_LICENSES.md";

/// Regenerates `THIRD_PARTY_LICENSES.md` for the crates in arke.exe (offline: only the license
/// files in the local registry, so the output depends on nothing but `Cargo.lock`).
fn cmd_licenses() -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    // cargo-about reads `cargo metadata` for every platform, so crates a Windows build never
    // downloads (macOS-only ones) must be in the registry first; a fresh CI runner lacks them.
    let fetched = Command::new(&cargo)
        .args(["fetch", "--locked"])
        .current_dir(root())
        .status()
        .with_context(|| format!("running {cargo} fetch"))?;
    ensure!(fetched.success(), "cargo fetch failed");
    let status = Command::new(&cargo)
        .args([
            "about",
            "generate",
            "--locked",
            "--offline",
            "--fail",
            "-c",
            "about.toml",
            "-m",
            "crates/app/Cargo.toml",
            "-o",
            LICENSES,
            "about.hbs",
        ])
        .current_dir(root())
        .status()
        .with_context(|| format!("running {cargo} about"))?;
    ensure!(
        status.success(),
        "cargo about failed; install it with: cargo install cargo-about --locked --features cli"
    );
    println!("wrote {LICENSES}");
    Ok(())
}

/// Full version for this build.
fn release_version(channel: &str, args: &[String]) -> Result<String> {
    let changes = changesets();
    match channel {
        "stable" => {
            ensure!(
                changes.is_empty(),
                "{} unused changesets: run cargo xtask version before a stable release",
                changes.len()
            );
            Ok(fmt_triple(current_version()?))
        }
        "rc" => {
            let next = fmt_triple(next_version(&changes)?);
            // Not given: number of local v<version>-rc.* tags plus one.
            let number = if let Some(n) = value_of(args, "--number") {
                n.parse::<u32>()
                    .with_context(|| format!("invalid --number: {n}"))?
            } else {
                let tags = git(&["tag", "-l", &format!("v{next}-rc.*")]).unwrap_or_default();
                u32::try_from(tags.lines().count()).unwrap_or(0) + 1
            };
            Ok(format!("{next}-rc.{number}"))
        }
        "nightly" => Ok(format!(
            "{}-nightly.{}",
            fmt_triple(next_version(&changes)?),
            today()
        )),
        other => bail!("unknown channel: {other} (stable, rc, nightly)"),
    }
}

/// Release notes: this version's CHANGELOG section for stable, pending changesets for RC and
/// nightly.
fn release_notes(channel: &str, version: &str) -> String {
    if channel == "stable" {
        let changelog = std::fs::read_to_string(root().join("CHANGELOG.md")).unwrap_or_default();
        let head = format!("## {version}");
        if let Some(at) = changelog.find(&head) {
            let rest = &changelog[at + head.len()..];
            let end = rest.find("\n## ").unwrap_or(rest.len());
            return rest[..end].trim().to_string();
        }
        return String::new();
    }
    let changes = changesets();
    if changes.is_empty() {
        return "(No changes recorded for this build.)".into();
    }
    section(&changes, "en")
}

/// Lists changes grouped by major, minor and patch, in `lang`.
fn section(changes: &[Change], lang: &str) -> String {
    let texts: Vec<&ChangeText> = changes.iter().map(|c| &c.text).collect();
    notes::section(&texts, lang)
}

fn cmd_change(args: &[String]) -> Result<()> {
    const USAGE: &str =
        "usage: cargo xtask change <patch|minor|major> <summary> --zh-TW <text> --zh-CN <text>";
    let bump = args.first().and_then(|b| Bump::parse(b)).context(USAGE)?;
    // English words until the first --lang option; each option takes the next argument.
    let mut english = Vec::new();
    let mut translations: Vec<(&str, String)> = Vec::new();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.strip_prefix("--") {
            Some(lang) => {
                let lang = LANGS[1..]
                    .iter()
                    .copied()
                    .find(|l| l.eq_ignore_ascii_case(lang))
                    .with_context(|| format!("unknown option --{lang}; {USAGE}"))?;
                let text = rest
                    .next()
                    .with_context(|| format!("--{lang} needs a text"))?;
                translations.push((lang, text.trim().to_string()));
            }
            None => english.push(arg.as_str()),
        }
    }
    let summary = english.join(" ");
    ensure!(!summary.trim().is_empty(), "a summary is required");
    let dir = root().join(".changeset");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let slug: String = summary
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(24)
        .collect::<String>()
        .to_lowercase();
    let name = if slug.is_empty() {
        format!("{stamp}.md")
    } else {
        format!("{stamp}-{slug}.md")
    };
    let path = dir.join(name);
    let mut texts: Vec<(&str, &str)> = vec![("en", summary.trim())];
    texts.extend(translations.iter().map(|(l, t)| (*l, t.as_str())));
    std::fs::write(&path, ChangeText::to_file(bump, &texts))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("recorded: {}", path.display());
    let missing: Vec<&str> = LANGS[1..]
        .iter()
        .copied()
        .filter(|l| !translations.iter().any(|(t, _)| t == l))
        .collect();
    if !missing.is_empty() {
        println!(
            "still to add before the release: {} (a <!-- lang --> block in the file)",
            missing.join(", ")
        );
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let changes = changesets();
    let current = fmt_triple(current_version()?);
    let next = fmt_triple(next_version(&changes)?);
    println!("current version: {current}");
    println!(
        "these changes bump to: {next} ({} changesets)",
        changes.len()
    );
    if !changes.is_empty() {
        println!("\n{}", section(&changes, "en"));
    }
    let missing = untranslated(&changes);
    if !missing.is_empty() {
        println!("\nnot translated yet:\n  {}", missing.join("\n  "));
    }
    Ok(())
}

fn cmd_version() -> Result<()> {
    let changes = changesets();
    ensure!(
        !changes.is_empty(),
        "no changesets: record what changed with cargo xtask change first"
    );
    let missing = untranslated(&changes);
    ensure!(
        missing.is_empty(),
        "every changeset needs its zh-TW and zh-CN text (Arke shows the notes in the user's \
         language); missing:\n  {}",
        missing.join("\n  ")
    );
    let current = fmt_triple(current_version()?);
    let next = fmt_triple(next_version(&changes)?);
    // Cargo.toml: replace only the [workspace.package] line.
    let path = cargo_toml();
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let old = format!("version = \"{current}\"");
    ensure!(text.contains(&old), "{old} not found in Cargo.toml");
    std::fs::write(
        &path,
        text.replacen(&old, &format!("version = \"{next}\""), 1),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    // The changelogs: newest version on top.
    let date = today();
    for lang in LANGS {
        let entry = format!(
            "## {next} ({}-{}-{})\n\n{}\n\n",
            &date[..4],
            &date[4..6],
            &date[6..],
            section(&changes, lang)
        );
        let changelog_path = root().join(notes::changelog_file(lang));
        let changelog = std::fs::read_to_string(&changelog_path)
            .unwrap_or_else(|_| notes::changelog_header(lang).into());
        std::fs::write(&changelog_path, with_entry(&changelog, &entry, lang))
            .with_context(|| format!("writing {}", changelog_path.display()))?;
    }
    for c in &changes {
        std::fs::remove_file(&c.path).with_context(|| format!("removing {}", c.path.display()))?;
    }
    println!(
        "{current} -> {next}: updated Cargo.toml and the changelogs, used {} changesets",
        changes.len()
    );
    println!("next: git commit, git tag v{next}, cargo xtask publish stable");
    Ok(())
}

/// `changelog` with `entry` above its newest version.
fn with_entry(changelog: &str, entry: &str, lang: &str) -> String {
    let (head, rest) = changelog
        .find("\n## ")
        .map_or((changelog, ""), |at| changelog.split_at(at + 1));
    let head = if head.trim().is_empty() {
        notes::changelog_header(lang)
    } else {
        head
    };
    format!("{}\n\n{entry}{rest}", head.trim_end())
}

/// A Windows architecture Arke ships for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Arch {
    /// `--arch` value
    name: &'static str,
    /// Rust target triple
    target: &'static str,
    /// Added to the exe name; the updater looks for its own (`update::ARCH_SUFFIX`)
    suffix: &'static str,
}

/// x64 first: it is uploaded first, so older x64 builds that take the first `arke-*.exe` asset
/// keep getting theirs.
const ARCHES: [Arch; 2] = [
    Arch {
        name: "x64",
        target: "x86_64-pc-windows-msvc",
        suffix: "",
    },
    Arch {
        name: "arm64",
        target: "aarch64-pc-windows-msvc",
        suffix: "-arm64",
    },
];

/// The architectures `--arch` asks for (all by default).
fn arches(args: &[String]) -> Result<Vec<Arch>> {
    match value_of(args, "--arch").as_deref() {
        None | Some("all") => Ok(ARCHES.to_vec()),
        Some(name) => ARCHES
            .iter()
            .find(|a| a.name == name)
            .map(|a| vec![*a])
            .with_context(|| format!("unknown --arch {name} (x64, arm64 or all)")),
    }
}

/// Release asset name of the exe for `arch`.
fn exe_name(version: &str, arch: Arch) -> String {
    format!("arke-{version}{}.exe", arch.suffix)
}

/// Release asset name of the installer for `arch`.
fn setup_name(version: &str, arch: Arch) -> String {
    format!("arke-{version}{}-setup.exe", arch.suffix)
}

/// Inno Setup's compiler: `ISCC`, or where its installer puts it.
fn iscc() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ISCC") {
        return Some(PathBuf::from(path));
    }
    ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|base| {
            let base = PathBuf::from(base);
            if base.ends_with("Local") {
                base.join("Programs")
            } else {
                base
            }
        })
        .map(|base| base.join("Inno Setup 6").join("ISCC.exe"))
        .find(|path| path.is_file())
}

/// Writes `<file>.sha256` next to `file` and returns the hash.
fn write_sha256(file: &Path) -> Result<(String, u64)> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let hash = Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
    let name = file
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned());
    let sha_path = file.with_file_name(format!("{name}.sha256"));
    std::fs::write(&sha_path, format!("{hash}  {name}\n"))
        .with_context(|| format!("writing {}", sha_path.display()))?;
    Ok((hash, bytes.len() as u64))
}

/// The update signing key from `MINISIGN_SECRET_KEY` (an unencrypted minisign secret key, as
/// `minisign -G -W` writes it; the release workflow passes the repository secret). The public half
/// is built into Arke (`update_sig.rs`).
fn signing_key() -> Result<minisign::SecretKey> {
    let text = std::env::var("MINISIGN_SECRET_KEY")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .context("MINISIGN_SECRET_KEY must be set: Arke only installs signed updates")?;
    secret_key_from(&text)
}

/// An unencrypted minisign secret key from its text: the whole file or the key line alone. Read
/// as bytes, not through `SecretKeyBox::into_secret_key`: that checks a checksum the minisign
/// crate leaves empty in the unencrypted keys it makes, so it took the right key for a wrong
/// password. Whether the key is the right one is checked by signing with it ([`check_key`]).
fn secret_key_from(text: &str) -> Result<minisign::SecretKey> {
    use base64::Engine as _;
    let line = text
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .context("MINISIGN_SECRET_KEY has no key line")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(line)
        .map_err(|e| anyhow!("MINISIGN_SECRET_KEY isn't base64: {e}"))?;
    minisign::SecretKey::from_bytes(&bytes).map_err(|e| anyhow!("MINISIGN_SECRET_KEY: {e}"))
}

/// What a signature vouches for besides the bytes: the release asset name and the version, so a
/// signature can't stand for another file or an older version (`update_sig.rs` checks both).
fn trusted_comment(asset: &str, version: &str) -> String {
    format!("file:{asset} version:{version}")
}

/// Signs `file`, published as `asset`, into `dist/<asset>.minisig`; returns that path.
fn sign(file: &Path, asset: &str, version: &str, key: &minisign::SecretKey) -> Result<PathBuf> {
    let data = std::fs::File::open(file).with_context(|| format!("reading {}", file.display()))?;
    let signature = minisign::sign(
        None,
        key,
        data,
        Some(&trusted_comment(asset, version)),
        Some("arke update signature"),
    )
    .map_err(|e| anyhow!("signing {}: {e}", file.display()))?;
    // A key that isn't the one built into Arke would publish a release no Arke can update to.
    let keys = arke_keys()?;
    let checks = keys.iter().any(|key| {
        std::fs::File::open(file)
            .is_ok_and(|data| minisign::verify(key, &signature, data, true, false, false).is_ok())
    });
    ensure!(
        checks,
        "the signing key doesn't match the update keys in {UPDATE_KEYS_FILE}"
    );
    let path = root().join("dist").join(format!("{asset}.minisig"));
    std::fs::write(&path, signature.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Whether `key` signs what Arke's update keys accept: signs a few bytes in memory and checks
/// them. The release checks every file it signs too ([`sign`]); this answers before a release.
fn check_key(key: &minisign::SecretKey) -> Result<()> {
    const PROBE: &[u8] = b"arke update key check";
    let signature = minisign::sign(
        None,
        key,
        std::io::Cursor::new(PROBE),
        Some("arke key check"),
        None,
    )
    .map_err(|e| anyhow!("signing: {e}"))?;
    let checks = arke_keys()?.iter().any(|public| {
        minisign::verify(
            public,
            &signature,
            std::io::Cursor::new(PROBE),
            true,
            false,
            false,
        )
        .is_ok()
    });
    ensure!(
        checks,
        "the signing key doesn't match the update keys in {UPDATE_KEYS_FILE}"
    );
    Ok(())
}

/// `cargo xtask sign-check`: the CI secret against the keys built into Arke.
fn cmd_sign_check() -> Result<()> {
    check_key(&signing_key()?)?;
    println!("MINISIGN_SECRET_KEY matches the update key built into Arke");
    Ok(())
}

/// Where Arke's update public keys are (`KEYS`).
const UPDATE_KEYS_FILE: &str = "crates/app/src/update_sig.rs";

/// The update public keys built into Arke, read from its source.
fn arke_keys() -> Result<Vec<minisign::PublicKey>> {
    let source = std::fs::read_to_string(root().join(UPDATE_KEYS_FILE))
        .with_context(|| format!("reading {UPDATE_KEYS_FILE}"))?;
    let keys: Vec<minisign::PublicKey> = source
        .split('"')
        .filter(|part| part.starts_with("RW"))
        .filter_map(|part| minisign::PublicKey::from_base64(part).ok())
        .collect();
    ensure!(
        !keys.is_empty(),
        "no update keys found in {UPDATE_KEYS_FILE}"
    );
    Ok(keys)
}

/// Release asset name of the notes in `lang`: the English release notes, or the changelog in
/// another language (Arke shows the new version's part of it before updating).
fn notes_asset(version: &str, lang: &str) -> String {
    if lang == "en" {
        format!("arke-{version}-notes.md")
    } else {
        format!("arke-{version}-changelog.{lang}.md")
    }
}

/// Builds the installer around `exe` with Inno Setup; returns its path.
fn build_setup(iscc: &Path, version: &str, arch: Arch, exe: &Path, dist: &Path) -> Result<PathBuf> {
    let status = Command::new(iscc)
        .arg("/Q")
        .arg(format!("/DAppVersion={version}"))
        .arg(format!("/DArch={}", arch.name))
        .arg(format!("/DSourceExe={}", exe.display()))
        .arg(format!("/O{}", dist.display()))
        .arg(root().join("installer").join("arke.iss"))
        .status()
        .with_context(|| format!("running {}", iscc.display()))?;
    ensure!(status.success(), "Inno Setup failed");
    let setup = dist.join(setup_name(version, arch));
    ensure!(setup.is_file(), "{} was not made", setup.display());
    Ok(setup)
}

/// Packages release builds as dist/arke-<version>[-arm64].exe and, with Inno Setup, the
/// installers, each with a .sha256, plus release notes. Returns (version, file paths).
fn cmd_build(args: &[String]) -> Result<(String, Vec<PathBuf>)> {
    let channel = args.first().map_or_default(String::as_str);
    let version = release_version(channel, args)?;
    // Before compiling: the exe embeds this file.
    cmd_licenses()?;
    let commit = git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_default();
    let repo = releases_repo();
    println!("building Arke {version} ({channel})...");
    let arches = arches(args)?;
    let target_dir = root().join("target").join("dist");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(&cargo);
    command.args(["build", "--release", "-p", "arke-app", "--bin", "arke"]);
    for arch in &arches {
        command.args(["--target", arch.target]);
    }
    let status = command
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("ARKE_VERSION", &version)
        .env("ARKE_CHANNEL", channel)
        .env("ARKE_COMMIT", &commit)
        .env("ARKE_RELEASES_REPO", &repo)
        .current_dir(root())
        .status()
        .with_context(|| format!("running {cargo} build"))?;
    ensure!(status.success(), "build failed");
    let dist = root().join("dist");
    std::fs::create_dir_all(&dist).with_context(|| format!("creating {}", dist.display()))?;
    let mut packaged = Vec::new();
    for arch in arches {
        let file_name = exe_name(&version, arch);
        let exe = dist.join(&file_name);
        let built = target_dir
            .join(arch.target)
            .join("release")
            .join("arke.exe");
        std::fs::copy(&built, &exe)
            .with_context(|| format!("copying {} to {}", built.display(), exe.display()))?;
        let (hash, len) = write_sha256(&exe)?;
        println!(
            "done: {} ({:.1} MB, SHA-256 {hash})",
            exe.display(),
            len as f64 / 1_048_576.0
        );
        packaged.push(exe.clone());
        match iscc() {
            Some(iscc) => {
                let setup = build_setup(&iscc, &version, arch, &exe, &dist)?;
                let (hash, len) = write_sha256(&setup)?;
                println!(
                    "done: {} ({:.1} MB, SHA-256 {hash})",
                    setup.display(),
                    len as f64 / 1_048_576.0
                );
                packaged.push(setup);
            }
            None if std::env::var_os("ARKE_REQUIRE_SETUP").is_some() => {
                bail!("Inno Setup not found (install it or set ISCC)")
            }
            None => println!("(Inno Setup not found: no installer)"),
        }
    }
    let notes_path = dist.join(format!("arke-{version}.md"));
    std::fs::write(&notes_path, release_notes(channel, &version))
        .with_context(|| format!("writing {}", notes_path.display()))?;
    std::fs::copy(root().join(LICENSES), dist.join(LICENSES))
        .with_context(|| format!("copying {LICENSES} to {}", dist.display()))?;
    if repo.is_empty() {
        println!("(release.toml has no repo set: this build won't check for updates)");
    }
    Ok((version, packaged))
}

/// What `cargo xtask build` left in dist/ for this version: the exes `--arch` asks for. Lets CI
/// build and test without write access and publish in a separate job.
fn prebuilt(args: &[String]) -> Result<(String, Vec<PathBuf>)> {
    let channel = args.first().map_or_default(String::as_str);
    let version = release_version(channel, args)?;
    let dist = root().join("dist");
    let mut packaged = Vec::new();
    for arch in arches(args)? {
        let exe = dist.join(exe_name(&version, arch));
        ensure!(
            exe.is_file(),
            "{} not found: run cargo xtask build {channel} first",
            exe.display()
        );
        packaged.push(exe);
        let setup = dist.join(setup_name(&version, arch));
        if setup.is_file() {
            packaged.push(setup);
        } else {
            ensure!(
                std::env::var_os("ARKE_REQUIRE_SETUP").is_none(),
                "{} not found",
                setup.display()
            );
        }
    }
    ensure!(
        dist.join(LICENSES).is_file(),
        "{LICENSES} not found in {}",
        dist.display()
    );
    Ok((version, packaged))
}

/// Publishes to GitHub Releases: creates the v<version> release (RC and nightly marked as
/// pre-release) and uploads each exe with its .sha256, then the license texts.
fn cmd_publish(args: &[String]) -> Result<()> {
    let repo = releases_repo();
    ensure!(
        !repo.is_empty(),
        "release.toml has no repo set (the GitHub repo for releases, owner/name)"
    );
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .context("GITHUB_TOKEN must be set (with write access to that repo)")?;
    let channel = args.first().cloned().unwrap_or_default();
    // Before creating the release: nothing goes out unsigned.
    let key = signing_key()?;
    let (version, packaged) = if args.iter().any(|a| a == "--prebuilt") {
        prebuilt(args)?
    } else {
        cmd_build(args)?
    };
    // The notes written with the build, so a prebuilt dist/ publishes what it was built with.
    let notes_path = root().join("dist").join(format!("arke-{version}.md"));
    let notes = std::fs::read_to_string(&notes_path).unwrap_or_else(|_| {
        let notes = release_notes(&channel, &version);
        let _ = std::fs::write(&notes_path, &notes);
        notes
    });
    // Every file Arke downloads, with its name on the release and its signature.
    let mut uploads: Vec<(PathBuf, String)> = Vec::new();
    for file in &packaged {
        let name = file
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        let sha = file.with_file_name(format!("{name}.sha256"));
        let sig = sign(file, &name, &version, &key)?;
        uploads.push((file.clone(), name.clone()));
        uploads.push((sha, format!("{name}.sha256")));
        uploads.push((sig, format!("{name}.minisig")));
    }
    for lang in LANGS {
        let file = if lang == "en" {
            notes_path.clone()
        } else {
            root().join(notes::changelog_file(lang))
        };
        let name = notes_asset(&version, lang);
        let sig = sign(&file, &name, &version, &key)?;
        uploads.push((file, name.clone()));
        uploads.push((sig, format!("{name}.minisig")));
    }
    uploads.push((root().join("dist").join(LICENSES), LICENSES.to_owned()));
    let body = serde_json::json!({
        "tag_name": format!("v{version}"),
        "name": format!("Arke {version}"),
        "body": notes,
        "prerelease": channel != "stable",
    });
    let created = curl(&[
        "-sS",
        "-X",
        "POST",
        "-H",
        &format!("Authorization: Bearer {token}"),
        "-H",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{repo}/releases"),
        "-d",
        &body.to_string(),
    ])
    .with_context(|| format!("creating release v{version} on {repo}"))?;
    let release: serde_json::Value = serde_json::from_str(&created)
        .with_context(|| format!("parsing the GitHub response: {created}"))?;
    let id = release["id"]
        .as_u64()
        .with_context(|| format!("creating the release failed: {created}"))?;
    for (file, name) in uploads {
        curl(&[
            "-sS",
            "-X",
            "POST",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "-H",
            "Content-Type: application/octet-stream",
            "--data-binary",
            &format!("@{}", file.display()),
            &format!("https://uploads.github.com/repos/{repo}/releases/{id}/assets?name={name}"),
        ])
        .with_context(|| format!("uploading {}", file.display()))?;
        println!("uploaded {name}");
    }
    println!(
        "published: {}",
        release["html_url"].as_str().unwrap_or_default()
    );
    Ok(())
}

fn curl(args: &[&str]) -> Result<String> {
    let out = Command::new("curl.exe")
        .args(args)
        .output()
        .context("curl")?;
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("change") => cmd_change(rest),
        Some("status") => cmd_status(),
        Some("version") => cmd_version(),
        Some("release-version") => {
            let channel = rest.first().map_or_default(String::as_str);
            release_version(channel, rest).map(|v| println!("{v}"))
        }
        Some("i18n") => i18n::run(&root(), rest),
        Some("licenses") => cmd_licenses(),
        Some("build") => cmd_build(rest).map(|_| ()),
        Some("publish") => cmd_publish(rest),
        Some("sign-check") => cmd_sign_check(),
        _ => Err(anyhow!(
            "usage: cargo xtask <change|status|version|release-version|i18n|licenses|build|publish|sign-check> ..."
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a failing test should stop right there")]
mod tests {
    use super::*;

    #[test]
    fn bumps_follow_cargo_for_zero_versions() {
        assert_eq!(apply(Bump::Patch, (0, 1, 0)), (0, 1, 1));
        assert_eq!(apply(Bump::Minor, (0, 1, 3)), (0, 2, 0));
        assert_eq!(apply(Bump::Major, (0, 1, 3)), (0, 2, 0));
        assert_eq!(apply(Bump::Major, (1, 2, 3)), (2, 0, 0));
        assert_eq!(apply(Bump::Minor, (1, 2, 3)), (1, 3, 0));
    }

    #[test]
    fn parses_version_triples() {
        assert_eq!(parse_triple("0.12.3").unwrap(), (0, 12, 3));
        let err = parse_triple("1.x").unwrap_err();
        assert_eq!(err.to_string(), "unrecognized version: 1.x");
    }

    #[test]
    fn names_an_exe_per_architecture() {
        let args = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
        let all = arches(&args(&["nightly"])).unwrap();
        let names: Vec<String> = all.iter().map(|a| exe_name("0.3.0-rc.1", *a)).collect();
        assert_eq!(names, ["arke-0.3.0-rc.1.exe", "arke-0.3.0-rc.1-arm64.exe"]);
        let arm = arches(&args(&["stable", "--arch", "arm64"])).unwrap();
        let targets: Vec<&str> = arm.iter().map(|a| a.target).collect();
        assert_eq!(targets, ["aarch64-pc-windows-msvc"]);
        assert!(arches(&args(&["stable", "--arch", "x86"])).is_err());
    }

    #[test]
    fn new_versions_go_on_top_of_each_changelog() {
        let entry = "## 0.4.0 (2026-11-20)

### 新功能

- 新的

";
        let old = "# 更新紀錄

說明。

## 0.3.0 (2026-11-13)

- 舊的
";
        assert_eq!(
            with_entry(old, entry, "zh-TW"),
            "# 更新紀錄

說明。

## 0.4.0 (2026-11-20)

### 新功能

- 新的

## 0.3.0 (2026-11-13)

- 舊的
"
        );
        // A missing changelog starts with its header.
        assert!(with_entry("", entry, "zh-CN").starts_with("# 更新日志"));
    }

    #[test]
    fn dates_are_civil() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_402), (2025, 11, 10));
    }

    #[test]
    fn finds_the_update_key_built_into_arke() {
        assert_eq!(arke_keys().unwrap().len(), 1);
    }

    /// A key Arke doesn't know stops the release before anything is uploaded.
    #[test]
    fn signing_with_another_key_stops_the_release() {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let file = std::env::temp_dir().join(format!("xtask-sign-{}.txt", std::process::id()));
        std::fs::write(&file, "notes").unwrap();
        let signed = sign(&file, "arke-0.0.1-notes.md", "0.0.1", &pair.sk);
        let _ = std::fs::remove_file(&file);
        let err = signed.unwrap_err().to_string();
        assert!(err.contains("doesn't match"), "{err}");
        assert!(!root().join("dist/arke-0.0.1-notes.md.minisig").exists());
    }

    /// An unencrypted key saved as text and read back (the repository secret) signs what its
    /// public key accepts, whole file or key line alone. Through `into_secret_key` it was
    /// refused as "Wrong password for that key".
    #[test]
    fn an_unencrypted_key_reads_back_from_its_text() {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let text = pair.sk.to_box(None).unwrap().to_string();
        let line = text.lines().nth(1).unwrap().to_owned();
        for text in [text, line] {
            let key = secret_key_from(&text).unwrap();
            let signature =
                minisign::sign(None, &key, std::io::Cursor::new(b"x"), None, None).unwrap();
            minisign::verify(
                &pair.pk,
                &signature,
                std::io::Cursor::new(b"x"),
                true,
                false,
                false,
            )
            .unwrap();
        }
    }

    /// The check before a release tells a key Arke doesn't know from one it does.
    #[test]
    fn key_check_needs_the_key_arke_has() {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let err = check_key(&pair.sk).unwrap_err().to_string();
        assert!(err.contains("doesn't match"), "{err}");
    }

    #[test]
    fn notes_have_an_asset_per_language() {
        assert_eq!(notes_asset("0.9.0", "en"), "arke-0.9.0-notes.md");
        assert_eq!(
            notes_asset("0.9.0", "zh-TW"),
            "arke-0.9.0-changelog.zh-TW.md"
        );
        assert_eq!(
            trusted_comment("arke-0.9.0.exe", "0.9.0"),
            "file:arke-0.9.0.exe version:0.9.0"
        );
    }
}
