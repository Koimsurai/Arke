# Contributing

Arke doesn't accept pull requests for now; they are closed automatically. Bug reports, ideas and
feedback are very welcome as [issues](https://github.com/Koimsurai/Arke/issues). The notes below
are for building Arke yourself or working on a fork.

## Development

```bash
cargo arke                     # run
cargo nextest run --workspace  # tests (GPUI UI tests run headless)
cargo lint                     # clippy with -D warnings
cargo fmt --all
```

Tests run with [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`).
CI runs on pushes to `main` and on pull requests: fmt, [typos](https://github.com/crate-ci/typos)
(spelling), [cargo-shear](https://github.com/Boshen/cargo-shear) (unused dependencies), clippy,
tests and `cargo deny`. Run the same checks locally with `typos` and `cargo shear`
(`cargo install typos-cli cargo-shear --locked`).

Lints are set once in the workspace `Cargo.toml` (`clippy::pedantic` on, with a short list of
documented exceptions) and thresholds in `clippy.toml`. `todo!`, `dbg!`, `println!` and unsafe
blocks without a `// SAFETY:` comment fail the build. Comments explain why, not what.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org):
`feat(search): add size filter`, `fix(dnd): keep selection after drop`. Keep the subject under
72 characters; use the body, if needed, for why. To check them locally, run
`git config core.hooksPath .githooks` once.

Ground rules for the code:

- Nothing slow on the UI thread: file system, shell and network calls run on background
  threads and report back.
- The registry is only written by the opt-in "Open folders in Arke" setting, only under
  `HKEY_CURRENT_USER\Software\Classes`, through `arke_core::folder_handler` (previous values
  backed up and restored exactly when the setting is turned off). Tests use the in-memory
  registry, never the real one. The optional index service is installed only when the user asks,
  with administrator approval (the Service Control Manager then records it under
  `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services`).
- Third-party shell extensions are never loaded into the Arke process; the Windows context
  menu is read in a separate `--shell-host` process.
- UI text lives in `crates/app/locales/app.yml` and `crates/core/locales/core.yml`
  (English, Traditional Chinese and Simplified Chinese).
- Anything Arke leaves on the system outside its own folders (a shortcut, a Credential Manager
  entry, the jump list, the search service) must also be undone by "Remove Arke from this PC"
  (`crates/app/src/uninstall.rs`). Try `arke --uninstall` only on a copy of the exe with
  `APPDATA` and `LOCALAPPDATA` pointing into `%TEMP%` and `ARKE_UNINSTALL_SANDBOX=1`, which leaves
  out the service, saved passwords and the jump list; `--dry-run` only lists the steps.
- "Report a problem" passes the log through `arke_core::scrub`, which hides the profile folder,
  account, computer and server names. Still never log passwords or file contents.
- Tests must not touch real user files; use `FakeFs` or a temp directory.
- Code, comments, log messages and tests are in English.

## Releases

Arke ships as a single executable. There are three channels:

| Channel | Version | When |
| --- | --- | --- |
| Nightly | `0.2.0-nightly.20261110` | by hand (Actions › nightly › Run workflow) |
| RC | `0.2.0-rc.1` | before a stable release |
| Stable | `0.2.0` | after the RC |

The nightly workflow's daily schedule is off for now, so the releases page isn't flooded with
prereleases; it still has a dry-run option that builds both exes without publishing.

Record user-visible changes as changesets:

```bash
cargo xtask change minor "Tabs can be reordered by dragging"
cargo xtask status
```

`patch` is a fix, `minor` a feature, `major` a breaking change. The files in `.changeset/` use
the same format as changesets and knope.

Stable release:

```bash
cargo xtask version            # consume changesets, bump Cargo.toml, update CHANGELOG.md
cargo xtask licenses           # refresh THIRD_PARTY_LICENSES.md (git add it if it is new)
git commit -am "chore(release): 0.2.0"
git tag v0.2.0
git push --follow-tags         # the release workflow tests, builds and publishes
```

The release workflow fails if the tag isn't the version xtask would publish (the `Cargo.toml`
version for stable, the next version plus `-rc.N` for an RC). Tests and the build run with a
read-only token; only the final job, which uploads what was built, can write to the repo.

`cargo xtask build <nightly|rc|stable>` packages locally into `dist/` (`arke-<version>.exe` for
x64 and `arke-<version>-arm64.exe`, each with a `.sha256` used by the updater, release notes and
`THIRD_PARTY_LICENSES.md`). It first regenerates `THIRD_PARTY_LICENSES.md`, the full license texts
of everything in the exe, with [cargo-about](https://github.com/EmbarkStudios/cargo-about)
(`cargo install cargo-about --locked --features cli`; `about.toml` mirrors the `deny.toml`
allowlist); the exe embeds it for Settings › About › Licenses. `cargo xtask licenses` does only
that step; commit the file when dependencies change.
ARM64 needs the MSVC ARM64 build tools; `--arch x64` builds only x64. `cargo xtask publish <channel>` uploads to the repo in
`release.toml` and needs `GITHUB_TOKEN`; with `--prebuilt` it uploads what is already in `dist/`
instead of building.
