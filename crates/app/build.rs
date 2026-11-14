//! Build info and Windows resources.
//!
//! - Version, channel, commit and update source come from env vars set by `cargo xtask build`;
//!   unset means a dev build. `build_info.rs` reads them with `env!`.
//!   - `ARKE_VERSION`: full version (`0.2.0`, `0.2.0-rc.1`, `0.2.0-nightly.20261110`), defaults to
//!     the Cargo version
//!   - `ARKE_CHANNEL`: `stable`, `rc`, `nightly`; defaults to `dev`
//!   - `ARKE_COMMIT`: short commit hash; defaults to asking git
//!   - `ARKE_RELEASES_REPO`: GitHub repo with releases (`owner/name`); empty disables update checks
//! - Icon and file version info (`arke.rc` template). The icon is generated from SVG by
//!   `cargo run -p genicon`.

// A failing build script should panic; cargo shows the message and stops.
#![allow(clippy::panic)]

use std::path::PathBuf;

fn env_or(name: &str, default: impl FnOnce() -> String) -> String {
    println!("cargo:rerun-if-env-changed={name}");
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(default)
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_default(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// What's new in an RC or nightly build: its changes aren't in the changelogs yet, so the
/// pending changesets go into `OUT_DIR/pending_changesets.txt` (separated by a U+001E line) for
/// `whats_new.rs`. Stable and dev builds get an empty file.
fn pending_changesets(manifest: &std::path::Path, channel: &str) {
    let dir = manifest.join("../../.changeset");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut texts: Vec<(PathBuf, String)> = Vec::new();
    if matches!(channel, "rc" | "nightly")
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for path in entries.flatten().map(|e| e.path()) {
            let changeset = path.extension().is_some_and(|e| e == "md")
                && path.file_name().is_some_and(|n| n != "README.md");
            if changeset && let Ok(text) = std::fs::read_to_string(&path) {
                texts.push((path, text));
            }
        }
    }
    texts.sort();
    let joined: Vec<String> = texts.into_iter().map(|(_, t)| t).collect();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    if let Err(e) = std::fs::write(
        out.join("pending_changesets.txt"),
        joined.join("\n\u{1e}\n"),
    ) {
        panic!("writing pending_changesets.txt: {e}");
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let version = env_or("ARKE_VERSION", || {
        std::env::var("CARGO_PKG_VERSION").unwrap_or_default()
    });
    let channel = env_or("ARKE_CHANNEL", || "dev".into());
    let commit = env_or("ARKE_COMMIT", git_commit);
    let repo = env_or("ARKE_RELEASES_REPO", String::new);
    println!("cargo:rustc-env=ARKE_VERSION={version}");
    println!("cargo:rustc-env=ARKE_CHANNEL={channel}");
    println!("cargo:rustc-env=ARKE_COMMIT={commit}");
    println!("cargo:rustc-env=ARKE_RELEASES_REPO={repo}");
    // Rerun on a new commit so dev builds don't show a stale hash.
    let git = manifest.join("../../.git");
    println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git.join("refs/heads").display()
    );

    pending_changesets(&manifest, &channel);

    println!("cargo:rerun-if-changed=arke.rc");
    println!("cargo:rerun-if-changed=assets/brand/arke.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // File versions are four numbers only: 0.2.0-rc.1 -> 0,2,0,0
    let numbers: Vec<u32> = version
        .split(['-', '+'])
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|n| n.parse().unwrap_or(0))
        .chain(std::iter::repeat(0))
        .take(4)
        .collect();
    let file_version = numbers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let icon = manifest.join("assets/brand/arke.ico");
    let template = std::fs::read_to_string(manifest.join("arke.rc"))
        .unwrap_or_else(|err| panic!("cannot read arke.rc: {err}"));
    let rc = template
        .replace("@ICON@", &icon.display().to_string().replace('\\', "/"))
        .replace("@FILEVERSION@", &file_version)
        .replace("@VERSION@", &version);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("arke.rc");
    std::fs::write(&out, rc).unwrap_or_else(|err| panic!("cannot write {}: {err}", out.display()));
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .unwrap_or_else(|err| panic!("cannot compile Windows resource arke.rc: {err}"));
}
