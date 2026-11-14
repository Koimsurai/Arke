//! Auto-update: look on GitHub Releases (`build_info::RELEASES_REPO`) for a newer version, download
//! the single exe, swap it in and restart.
//!
//! - Channels: stable sees stable only; RC sees RC and stable; nightly sees everything. Versions
//!   compare by SemVer (`0.2.0-nightly.20261110` < `0.2.0-rc.1` < `0.2.0`).
//! - Downloads go to `%LOCALAPPDATA%\Arke\updates`, verified against `.sha256` when present.
//! - Swap: a running exe can be renamed on Windows, so `arke.exe` -> `arke.exe.old` and the new one
//!   becomes `arke.exe`, rolled back on failure. Then start the new one
//!   (`--after-update <old pid>`, which waits for this process to exit before taking over
//!   residency) and exit; `.old` is deleted on next start.
//! - Network and file work run on background threads; the UI only reads [`UpdateState`].

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use gpui::{App, Global};
use rust_i18n::t;
use sha2::{Digest, Sha256};

use crate::build_info::{self, Channel};

/// Argument for the new version's launch, followed by the old pid (waits for it to exit before
/// taking over).
pub(crate) const AFTER_UPDATE_ARG: &str = "--after-update";

// ───────────────────────── Version ─────────────────────────

/// SemVer, only as much as comparison needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    core: (u64, u64, u64),
    pre: Vec<String>,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_string).collect()),
            None => (text, Vec::new()),
        };
        let mut parts = core.split('.').map(str::parse::<u64>);
        let (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) = (parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        Some(Self {
            core: (a, b, c),
            pre,
        })
    }

    /// Channel of this version (from the pre-release tag).
    pub(crate) fn channel(&self) -> Channel {
        match self.pre.first().map(String::as_str) {
            None => Channel::Stable,
            Some("rc") => Channel::Rc,
            Some(_) => Channel::Nightly,
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            // No pre-release tag ranks higher; otherwise compare segment by segment (numbers
            // numerically).
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

// ───────────────────────── Releases ─────────────────────────

/// Why checking, downloading or installing an update failed. Display is the text shown in
/// Settings > About.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UpdateError {
    /// This build has no release repository to check.
    #[error("{}", t!("update.no_source"))]
    NoSource,
    /// This build's own version does not parse.
    #[error("version")]
    BadVersion,
    /// `LOCALAPPDATA` is not set, so there is nowhere to download to.
    #[error("LOCALAPPDATA")]
    NoUpdatesDir,
    /// The release list or a file could not be fetched.
    #[error(transparent)]
    Http(#[from] crate::win_http::HttpError),
    /// A download file could not be created, read or moved.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The download does not match its published SHA-256.
    #[error("{}", t!("update.bad_hash"))]
    BadHash,
    /// The running exe could not be swapped for the new one.
    #[error("{}", t!("update.cannot_replace", error = .0))]
    Replace(#[source] std::io::Error),
}

/// A newer release on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Release {
    pub(crate) version: String,
    /// Download URL and size of the single exe
    pub(crate) exe_url: String,
    pub(crate) exe_name: String,
    pub(crate) size: u64,
    /// `.sha256` download URL, if any
    pub(crate) sha_url: Option<String>,
    /// Release notes (Markdown) and web page
    pub(crate) notes: String,
    pub(crate) page: String,
}

/// Suffix of this build's release asset: `arke-<version>.exe` is x64, `arke-<version>-arm64.exe`
/// is ARM64 (the names `cargo xtask build` gives them).
pub(crate) const ARCH_SUFFIX: &str = if cfg!(target_arch = "aarch64") {
    "-arm64"
} else {
    ""
};

/// Whether `name` is the exe for `version` built for the architecture with `suffix`.
fn is_exe_for(name: &str, version: &str, suffix: &str) -> bool {
    name.eq_ignore_ascii_case(&format!("arke-{version}{suffix}.exe"))
}

/// Added to the asset name for the installer (`installer/arke.iss`): `arke-<version>-setup.exe`,
/// `arke-<version>-arm64-setup.exe`.
const SETUP_SUFFIX: &str = "-setup";

/// Pick from the GitHub releases list the newest one visible on this channel, newer than `current`,
/// with an exe for this architecture: the installer when the installer put this copy here (it
/// keeps Settings > Apps up to date and can write to Program Files), else the single exe.
pub(crate) fn pick(json: &str, channel: Channel, current: &Version) -> Option<Release> {
    let suffix = if installed_by_setup() {
        format!("{ARCH_SUFFIX}{SETUP_SUFFIX}")
    } else {
        ARCH_SUFFIX.to_owned()
    };
    pick_for(json, channel, current, &suffix)
}

/// Whether the installer put the running exe where it is (its uninstaller sits next to it).
pub(crate) fn installed_by_setup() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| std::env::current_exe().is_ok_and(|exe| setup_install_at(&exe)))
}

/// Whether `exe` is an installed copy: Inno Setup's `unins000.exe` is in the same folder.
pub(crate) fn setup_install_at(exe: &Path) -> bool {
    exe.parent()
        .is_some_and(|dir| dir.join(SETUP_UNINSTALLER).is_file())
}

/// The uninstaller Inno Setup leaves next to the exe.
pub(crate) const SETUP_UNINSTALLER: &str = "unins000.exe";

/// Whether a downloaded update is the installer rather than the exe itself.
fn is_setup(path: &Path) -> bool {
    path.file_stem()
        .is_some_and(|stem| stem.to_string_lossy().ends_with(SETUP_SUFFIX))
}

/// [`pick`] for the architecture whose assets end in `suffix`.
fn pick_for(json: &str, channel: Channel, current: &Version, suffix: &str) -> Option<Release> {
    let list: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut best: Option<(Version, Release)> = None;
    for item in list.as_array()? {
        if item["draft"].as_bool() == Some(true) {
            continue;
        }
        let Some(version) = item["tag_name"].as_str().and_then(Version::parse) else {
            continue;
        };
        if version.channel() > channel || version <= *current {
            continue;
        }
        if best.as_ref().is_some_and(|(v, _)| *v >= version) {
            continue;
        }
        let assets = item["assets"].as_array().cloned().unwrap_or_default();
        let tag = item["tag_name"].as_str().unwrap_or_default();
        let tag = tag.strip_prefix('v').unwrap_or(tag);
        let exe = assets.iter().find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| is_exe_for(n, tag, suffix))
        });
        let Some(exe) = exe else {
            continue;
        };
        let exe_name = exe["name"].as_str().unwrap_or_default().to_string();
        let sha_url = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(&format!("{exe_name}.sha256")))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_string);
        let release = Release {
            version: item["tag_name"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('v')
                .to_string(),
            exe_url: exe["browser_download_url"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            exe_name,
            size: exe["size"].as_u64().unwrap_or(0),
            sha_url,
            notes: item["body"].as_str().unwrap_or_default().to_string(),
            page: item["html_url"].as_str().unwrap_or_default().to_string(),
        };
        best = Some((version, release));
    }
    best.map(|(_, r)| r)
}

/// Check once (background thread): `Ok(None)` means up to date.
pub(crate) fn check(channel: Channel) -> Result<Option<Release>, UpdateError> {
    let repo = build_info::RELEASES_REPO;
    if repo.is_empty() {
        return Err(UpdateError::NoSource);
    }
    let current = Version::parse(build_info::VERSION).ok_or(UpdateError::BadVersion)?;
    let json = crate::win_http::get_text(
        &format!("https://api.github.com/repos/{repo}/releases?per_page=30"),
        &["Accept: application/vnd.github+json"],
    )?;
    Ok(pick(&json, channel, &current))
}

fn updates_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Arke").join("updates"))
}

/// Download the new version (background thread) and verify SHA-256. `progress(downloaded, total)`.
pub(crate) fn download(
    release: &Release,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, UpdateError> {
    let dir = updates_dir().ok_or(UpdateError::NoUpdatesDir)?;
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(&release.exe_name);
    let part = target.with_extension("part");
    {
        let mut file = std::fs::File::create(&part)?;
        crate::win_http::get(&release.exe_url, &[], &mut file, progress)?;
    }
    if let Some(sha_url) = &release.sha_url {
        let expected = crate::win_http::get_text(sha_url, &[])?;
        let expected = expected
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase();
        let bytes = std::fs::read(&part)?;
        let actual = Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut s, b| {
                use std::fmt::Write as _;
                let _ = write!(s, "{b:02x}");
                s
            });
        if actual != expected {
            let _ = std::fs::remove_file(&part);
            return Err(UpdateError::BadHash);
        }
    }
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&part, &target)?;
    Ok(target)
}

/// Replace the running exe (background thread): `arke.exe` -> `arke.exe.old`, the new one becomes
/// `arke.exe`. Returns the new exe's path.
pub(crate) fn install(new_exe: &Path) -> Result<PathBuf, UpdateError> {
    // The installer replaces the exe itself when it runs (`restart`).
    if is_setup(new_exe) {
        return Ok(new_exe.to_path_buf());
    }
    let me = std::env::current_exe()?;
    let old = old_path(&me);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&me, &old).map_err(UpdateError::Replace)?;
    if let Err(e) = std::fs::copy(new_exe, &me) {
        // Couldn't place it: rename back and leave things as they were.
        let _ = std::fs::rename(&old, &me);
        return Err(UpdateError::Replace(e));
    }
    let _ = std::fs::remove_file(new_exe);
    Ok(me)
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// At startup: delete `arke.exe.old` and installers left by the last update (background thread).
pub(crate) fn clean_up_old() {
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&me));
    }
    if let Some(Ok(entries)) = updates_dir().map(std::fs::read_dir) {
        for entry in entries.flatten() {
            if is_setup(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// New version launched by the old one: wait for the old process to exit (up to 15 s) before taking
/// over residency and single instance.
pub(crate) fn wait_for_previous(args: &[String]) {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let Some(pid) = args
        .windows(2)
        .find(|w| w[0] == AFTER_UPDATE_ARG)
        .and_then(|w| w[1].parse::<u32>().ok())
    else {
        return;
    };
    // SAFETY: only requests wait access; the handle is closed after use.
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(handle, 15_000);
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
    }
}

// ───────────────────────── Package managers ─────────────────────────

/// Package manager that installed this copy. It owns the exe, so Arke must not replace it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PackageManager {
    Scoop,
    Winget,
}

impl PackageManager {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Scoop => "Scoop",
            Self::Winget => "winget",
        }
    }

    /// Command that updates Arke through this package manager.
    pub(crate) const fn update_command(self) -> &'static str {
        match self {
            Self::Scoop => "scoop update arke",
            Self::Winget => "winget upgrade Koimsurai.Arke",
        }
    }

    /// Command that removes Arke through this package manager.
    pub(crate) const fn uninstall_command(self) -> &'static str {
        match self {
            Self::Scoop => "scoop uninstall arke",
            Self::Winget => "winget uninstall Koimsurai.Arke",
        }
    }
}

/// Whether Scoop or winget manages the running exe, judged by where it lives.
pub(crate) fn package_manager() -> Option<PackageManager> {
    static FOUND: std::sync::OnceLock<Option<PackageManager>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let scoop_roots = ["SCOOP", "SCOOP_GLOBAL"]
            .iter()
            .filter_map(|var| std::env::var_os(var).map(PathBuf::from))
            .collect::<Vec<_>>();
        package_manager_for(&exe, &scoop_roots)
    })
}

/// `scoop_roots`: custom Scoop install folders from the SCOOP / SCOOP_GLOBAL variables.
pub(crate) fn package_manager_for(exe: &Path, scoop_roots: &[PathBuf]) -> Option<PackageManager> {
    let path = exe.to_string_lossy().to_lowercase().replace('/', "\\");
    let in_custom_scoop = scoop_roots.iter().any(|root| {
        let root = root.to_string_lossy().to_lowercase().replace('/', "\\");
        let root = root.trim_end_matches('\\');
        !root.is_empty() && path.starts_with(&format!("{root}\\apps\\"))
    });
    if in_custom_scoop || path.contains("\\scoop\\apps\\") {
        Some(PackageManager::Scoop)
    } else if path.contains("\\microsoft\\winget\\packages\\")
        || path.contains("\\microsoft\\winget\\links\\")
    {
        Some(PackageManager::Winget)
    } else {
        None
    }
}

// ───────────────────────── State ─────────────────────────

/// Update progress (read by Settings > About and the sidebar hint).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    /// Downloading: downloaded and total bytes (0 if unknown)
    Downloading(Release, u64, u64),
    /// Downloaded and swapped; "Restart" switches to the new version
    Ready(Release, PathBuf),
    /// What went wrong, as shown to the user (the typed error is turned into text here, where
    /// it reaches the UI).
    Failed(String),
}

impl Global for UpdateState {}

impl UpdateState {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    fn set(cx: &mut App, state: Self) {
        cx.set_global(state);
        cx.refresh_windows();
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(cx: &mut App, state: Self) {
        Self::set(cx, state);
    }

    /// A new version is available (whether the sidebar shows a hint).
    pub(crate) fn has_news(cx: &App) -> bool {
        matches!(
            Self::get(cx),
            Self::Available(_) | Self::Downloading(..) | Self::Ready(..)
        )
    }
}

/// Check for updates in the background; results go to [`UpdateState`].
pub(crate) fn start_check(channel: Channel, cx: &mut App) {
    if matches!(
        UpdateState::get(cx),
        UpdateState::Checking | UpdateState::Downloading(..)
    ) {
        return;
    }
    UpdateState::set(cx, UpdateState::Checking);
    let (tx, rx) = flume::bounded(1);
    crate::explorer::spawn_thread("update-check", move || {
        let _ = tx.send(check(channel));
    });
    cx.spawn(async move |cx| {
        let Ok(result) = rx.recv_async().await else {
            return;
        };
        cx.update(|cx| {
            // The channel changed while this was running: its answer is for the old one.
            let now = crate::settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .update_channel();
            if now != channel {
                UpdateState::set(cx, UpdateState::Idle);
                start_check(now, cx);
                return;
            }
            let state = match result {
                Ok(Some(release)) => UpdateState::Available(release),
                Ok(None) => UpdateState::UpToDate,
                Err(e) => UpdateState::Failed(e.to_string()),
            };
            UpdateState::set(cx, state);
            crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
                s.update(|st| st.update_checked = Some(unix_now()), cx);
            });
        });
    })
    .detach();
}

/// Download and swap in the new version in the background; progress and result go to
/// [`UpdateState`].
pub(crate) fn start_download(release: Release, cx: &mut App) {
    UpdateState::set(
        cx,
        UpdateState::Downloading(release.clone(), 0, release.size),
    );
    let (tx, rx) = flume::unbounded::<Result<(u64, u64), Result<PathBuf, UpdateError>>>();
    let job = release.clone();
    crate::explorer::spawn_thread("update-download", move || {
        let progress_tx = tx.clone();
        let mut last = std::time::Instant::now();
        let result = download(&job, &mut |done, total| {
            if last.elapsed() >= std::time::Duration::from_millis(150) {
                last = std::time::Instant::now();
                let _ = progress_tx.send(Ok((done, total.unwrap_or(job.size))));
            }
        })
        .and_then(|path| install(&path));
        let _ = tx.send(Err(result));
    });
    cx.spawn(async move |cx| {
        while let Ok(message) = rx.recv_async().await {
            let done = cx.update(|cx| match message {
                Ok((got, total)) => {
                    UpdateState::set(cx, UpdateState::Downloading(release.clone(), got, total));
                    false
                }
                Err(Ok(exe)) => {
                    UpdateState::set(cx, UpdateState::Ready(release.clone(), exe));
                    true
                }
                Err(Err(e)) => {
                    UpdateState::set(cx, UpdateState::Failed(e.to_string()));
                    true
                }
            });
            if done {
                break;
            }
        }
    })
    .detach();
}

/// Restart into the new version: launch the new exe (it waits for this process to exit), then quit.
/// An installer runs without its wizard (Windows asks for approval if Arke is installed for all
/// users), closes this process if it is still running and starts the new version when done.
pub(crate) fn restart(exe: &Path, cx: &mut App) {
    let spawned = if is_setup(exe) {
        std::process::Command::new(exe)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/LAUNCH"])
            .spawn()
    } else {
        std::process::Command::new(exe)
            .arg(AFTER_UPDATE_ARG)
            .arg(std::process::id().to_string())
            .spawn()
    };
    match spawned {
        Ok(_) => cx.quit(),
        Err(e) => UpdateState::set(cx, UpdateState::Failed(e.to_string())),
    }
}

/// Automatic check after startup: when enabled and more than 20 hours since the last check (nightly
/// ships daily).
/// What to do with the shown result once the update channel changes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnChannelChange {
    /// A download is running or done: that release stays.
    Keep,
    /// Checks are off: drop the old channel's answer, don't contact GitHub.
    Forget,
    /// Check the new channel right away.
    Check,
}

pub(crate) fn on_channel_change(state: &UpdateState, can_check: bool) -> OnChannelChange {
    match state {
        UpdateState::Downloading(..) | UpdateState::Ready(..) => OnChannelChange::Keep,
        _ if can_check => OnChannelChange::Check,
        _ => OnChannelChange::Forget,
    }
}

/// The channel was changed in Settings. Without this the page kept showing the old channel's
/// answer ("up to date") until the next check, so switching seemed to do nothing.
pub(crate) fn channel_changed(channel: Channel, cx: &mut App) {
    let auto = crate::settings::SettingsStore::global(cx)
        .read(cx)
        .settings
        .update_auto;
    let can_check = auto && !build_info::RELEASES_REPO.is_empty() && package_manager().is_none();
    match on_channel_change(&UpdateState::get(cx), can_check) {
        OnChannelChange::Keep => {}
        OnChannelChange::Forget => UpdateState::set(cx, UpdateState::Idle),
        // A check for the old channel may be running; it restarts itself when it lands.
        OnChannelChange::Check => start_check(channel, cx),
    }
}

pub(crate) fn auto_check(cx: &mut App) {
    let settings = crate::settings::SettingsStore::global(cx)
        .read(cx)
        .settings
        .clone();
    if !settings.update_auto || build_info::RELEASES_REPO.is_empty() || package_manager().is_some()
    {
        return;
    }
    let due = settings
        .update_checked
        .is_none_or(|at| unix_now().saturating_sub(at) > 20 * 3600);
    if due {
        start_check(settings.update_channel(), cx);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_scoop_and_winget_installs() {
        let pm = |p: &str, roots: &[&str]| {
            let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
            package_manager_for(Path::new(p), &roots)
        };
        assert_eq!(
            pm(r"C:\Users\me\scoop\apps\arke\current\arke.exe", &[]),
            Some(PackageManager::Scoop)
        );
        assert_eq!(
            pm(r"D:\Tools\apps\arke\0.2.0\arke.exe", &[r"D:\Tools\"]),
            Some(PackageManager::Scoop)
        );
        assert_eq!(
            pm(
                r"C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\Koimsurai.Arke_x\arke.exe",
                &[]
            ),
            Some(PackageManager::Winget)
        );
        assert_eq!(pm(r"C:\Users\me\Downloads\arke.exe", &[r"D:\Tools"]), None);
        assert_eq!(pm(r"D:\Tools-old\apps\arke.exe", &[r"D:\Tools"]), None);
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn versions_order_like_semver() {
        assert!(v("0.2.0-nightly.20261110") < v("0.2.0-nightly.20261111"));
        assert!(v("0.2.0-nightly.20261111") < v("0.2.0-rc.1"));
        assert!(v("0.2.0-rc.1") < v("0.2.0-rc.2"));
        assert!(v("0.2.0-rc.2") < v("0.2.0"));
        assert!(v("0.2.0") < v("0.2.1-nightly.20261101"));
        assert!(v("v1.0.0") > v("0.9.9"));
        assert_eq!(v("0.2.0-rc.1").channel(), Channel::Rc);
        assert_eq!(v("0.2.0").channel(), Channel::Stable);
        assert_eq!(v("0.2.0-nightly.1").channel(), Channel::Nightly);
        assert!(Version::parse("garbage").is_none());
    }

    #[test]
    fn picks_the_newest_release_the_channel_can_see() {
        let json = r#"[
          {"tag_name":"v0.3.0-nightly.20261112","draft":false,"body":"n","html_url":"p3",
           "assets":[{"name":"arke-0.3.0-nightly.20261112.exe","browser_download_url":"u3","size":3},
                     {"name":"arke-0.3.0-nightly.20261112.exe.sha256","browser_download_url":"s3"}]},
          {"tag_name":"v0.2.0-rc.1","draft":false,"body":"r","html_url":"p2",
           "assets":[{"name":"arke-0.2.0-rc.1.exe","browser_download_url":"u2","size":2}]},
          {"tag_name":"v0.2.0","draft":true,"body":"","html_url":"",
           "assets":[{"name":"arke-0.2.0.exe","browser_download_url":"x","size":1}]},
          {"tag_name":"v0.1.5","draft":false,"body":"s","html_url":"p1",
           "assets":[{"name":"arke-0.1.5.exe","browser_download_url":"u1","size":1}]}
        ]"#;
        let current = v("0.1.0");
        let nightly = pick(json, Channel::Nightly, &current).unwrap();
        assert_eq!(nightly.version, "0.3.0-nightly.20261112");
        assert_eq!(nightly.sha_url.as_deref(), Some("s3"));
        assert_eq!(
            pick(json, Channel::Rc, &current).unwrap().version,
            "0.2.0-rc.1"
        );
        // Drafts don't count.
        assert_eq!(
            pick(json, Channel::Stable, &current).unwrap().version,
            "0.1.5"
        );
        assert!(
            pick(json, Channel::Stable, &v("0.1.5")).is_none(),
            "already up to date"
        );
        assert!(pick("not json", Channel::Stable, &current).is_none());
    }

    #[test]
    fn changing_the_channel_rechecks_unless_a_download_holds_on() {
        let release = Release {
            version: "0.4.0-rc.1".into(),
            exe_url: String::new(),
            exe_name: String::new(),
            size: 0,
            sha_url: None,
            notes: String::new(),
            page: String::new(),
        };
        for state in [
            UpdateState::Idle,
            UpdateState::UpToDate,
            UpdateState::Checking,
            UpdateState::Available(release.clone()),
            UpdateState::Failed("x".into()),
        ] {
            assert_eq!(on_channel_change(&state, true), OnChannelChange::Check);
            assert_eq!(on_channel_change(&state, false), OnChannelChange::Forget);
        }
        let held = [
            UpdateState::Downloading(release.clone(), 1, 2),
            UpdateState::Ready(release, PathBuf::new()),
        ];
        for state in held {
            assert_eq!(on_channel_change(&state, true), OnChannelChange::Keep);
        }
    }

    /// Against the real releases: `cargo test -p arke live_channels -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn live_channels() {
        let json = crate::win_http::get_text(
            "https://api.github.com/repos/Koimsurai/Arke/releases?per_page=30",
            &["Accept: application/vnd.github+json"],
        )
        .unwrap();
        let current = v("0.3.0");
        for channel in [Channel::Stable, Channel::Rc, Channel::Nightly] {
            let found = pick(&json, channel, &current).map(|r| r.version);
            println!("{channel:?} from 0.3.0: {found:?}");
        }
        assert!(pick(&json, Channel::Stable, &current).is_none_or(|r| !r.version.contains('-')));
        assert!(pick(&json, Channel::Rc, &current).is_some());
    }

    #[test]
    fn picks_the_exe_for_this_architecture() {
        let json = r#"[
          {"tag_name":"v0.3.0","draft":false,"body":"","html_url":"p",
           "assets":[{"name":"arke-0.3.0-arm64.exe","browser_download_url":"a","size":2},
                     {"name":"arke-0.3.0-arm64.exe.sha256","browser_download_url":"as"},
                     {"name":"arke-0.3.0.exe","browser_download_url":"x","size":1},
                     {"name":"arke-0.3.0.exe.sha256","browser_download_url":"xs"}]},
          {"tag_name":"v0.2.0","draft":false,"body":"","html_url":"p",
           "assets":[{"name":"arke-0.2.0.exe","browser_download_url":"old","size":1}]}
        ]"#;
        let current = v("0.1.0");
        let x64 = pick_for(json, Channel::Stable, &current, "").unwrap();
        assert_eq!(
            (x64.exe_url.as_str(), x64.sha_url.as_deref()),
            ("x", Some("xs"))
        );
        let arm = pick_for(json, Channel::Stable, &current, "-arm64").unwrap();
        assert_eq!(
            (arm.exe_url.as_str(), arm.sha_url.as_deref()),
            ("a", Some("as"))
        );
        // An ARM64 build skips releases that only have the x64 exe.
        assert!(pick_for(json, Channel::Stable, &v("0.3.0"), "-arm64").is_none());
        assert_eq!(
            pick_for(
                &json.replace("arke-0.3.0-arm64", "gone"),
                Channel::Stable,
                &current,
                "-arm64"
            ),
            None,
            "no ARM64 exe in either release"
        );
    }

    #[test]
    fn an_installed_copy_updates_through_the_installer() {
        let json = r#"[
          {"tag_name":"v0.4.0","draft":false,"body":"","html_url":"p",
           "assets":[{"name":"arke-0.4.0.exe","browser_download_url":"x","size":1},
                     {"name":"arke-0.4.0-setup.exe","browser_download_url":"s","size":3},
                     {"name":"arke-0.4.0-setup.exe.sha256","browser_download_url":"ss"},
                     {"name":"arke-0.4.0-arm64-setup.exe","browser_download_url":"as","size":4}]},
          {"tag_name":"v0.3.0","draft":false,"body":"","html_url":"p",
           "assets":[{"name":"arke-0.3.0.exe","browser_download_url":"old","size":1}]}
        ]"#;
        let current = v("0.2.0");
        let setup = pick_for(json, Channel::Stable, &current, "-setup").unwrap();
        assert_eq!(
            (
                setup.exe_url.as_str(),
                setup.sha_url.as_deref(),
                setup.exe_name.as_str()
            ),
            ("s", Some("ss"), "arke-0.4.0-setup.exe")
        );
        let arm = pick_for(json, Channel::Stable, &current, "-arm64-setup").unwrap();
        assert_eq!(arm.exe_url, "as");
        // Releases from before the installer have none: an installed copy waits for one.
        assert!(pick_for(json, Channel::Stable, &v("0.4.0"), "-setup").is_none());
        assert!(is_setup(Path::new(r"C:\u\arke-0.4.0-setup.exe")));
        assert!(is_setup(Path::new(r"C:\u\arke-0.4.0-arm64-setup.exe")));
        assert!(!is_setup(Path::new(r"C:\u\arke-0.4.0.exe")));
    }

    #[test]
    fn tells_installed_copies_by_the_uninstaller_next_to_them() {
        let dir = std::env::temp_dir().join(format!("arke-test-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("arke.exe");
        std::fs::write(&exe, b"").unwrap();
        assert!(!setup_install_at(&exe));
        std::fs::write(dir.join(SETUP_UNINSTALLER), b"").unwrap();
        assert!(setup_install_at(&exe));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
