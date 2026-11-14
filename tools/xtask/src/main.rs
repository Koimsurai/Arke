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
//! cargo xtask licenses                            regenerate THIRD_PARTY_LICENSES.md (cargo-about)
//! cargo xtask build <stable|rc|nightly> [--number N] [--arch x64|arm64]
//!                                                 package the exes into dist/ (exe, .sha256, notes,
//!                                                 THIRD_PARTY_LICENSES.md)
//! cargo xtask publish <stable|rc|nightly> [--number N] [--arch x64|arm64] [--prebuilt]
//!                                                 build (or with --prebuilt take what is in dist/),
//!                                                 then upload to GitHub Releases (repo from
//!                                                 release.toml; needs GITHUB_TOKEN; curl)
//! ```
//!
//! Both architectures are built unless `--arch` picks one: `arke-<version>.exe` (x64) and
//! `arke-<version>-arm64.exe`. ARM64 needs the MSVC ARM64 build tools (Visual Studio component
//! "MSVC ARM64/ARM64EC build tools") and `rustup target add aarch64-pc-windows-msvc`.
//!
//! Versions: stable is the `Cargo.toml` version; RC is the version the pending changesets lead to
//! plus `-rc.N`; nightly is that version plus `-nightly.YYYYMMDD`. So 0.2.0-nightly < 0.2.0-rc.1
//! < 0.2.0, which is how the update check compares them.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};

use notes::{Bump, ChangeText, LANGS};

#[allow(dead_code)] // the reading half is for Arke's What's new page (whats_new.rs)
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

/// Packages release builds as dist/arke-<version>[-arm64].exe, each with a .sha256, plus release
/// notes. Returns (version, exe paths).
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
        let bytes = std::fs::read(&exe).with_context(|| format!("reading {}", exe.display()))?;
        let hash = Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            });
        let sha_path = dist.join(format!("{file_name}.sha256"));
        std::fs::write(&sha_path, format!("{hash}  {file_name}\n"))
            .with_context(|| format!("writing {}", sha_path.display()))?;
        println!(
            "done: {} ({:.1} MB, SHA-256 {hash})",
            exe.display(),
            bytes.len() as f64 / 1_048_576.0
        );
        packaged.push(exe);
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
    let (version, packaged) = if args.iter().any(|a| a == "--prebuilt") {
        prebuilt(args)?
    } else {
        cmd_build(args)?
    };
    // The notes written with the build, so a prebuilt dist/ publishes what it was built with.
    let notes = std::fs::read_to_string(root().join("dist").join(format!("arke-{version}.md")))
        .unwrap_or_else(|_| release_notes(&channel, &version));
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
    let files = packaged.iter().flat_map(|exe| {
        let sha = exe.with_file_name(format!(
            "{}.sha256",
            exe.file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned())
        ));
        [exe.clone(), sha]
    });
    let licenses = root().join("dist").join(LICENSES);
    for file in files.chain([licenses]) {
        let name = file
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
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
        Some("licenses") => cmd_licenses(),
        Some("build") => cmd_build(rest).map(|_| ()),
        Some("publish") => cmd_publish(rest),
        _ => Err(anyhow!(
            "usage: cargo xtask <change|status|version|release-version|licenses|build|publish> ..."
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
#[allow(clippy::unwrap_used)]
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
}
