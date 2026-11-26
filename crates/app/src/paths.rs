//! Where Arke keeps its files: the one place that reads `%APPDATA%`, `%LOCALAPPDATA%` and
//! `%TEMP%` and adds the `Arke` folder.
//!
//! - **Roaming** (`%APPDATA%\Arke`): what the user would want on another PC: settings, saved
//!   workspaces, the shelf, folder views, search history, trusted host keys.
//! - **Local** (`%LOCALAPPDATA%\Arke`): what Arke rebuilds or that belongs to this PC: the log,
//!   the search index cache, thumbnails, downloaded updates, recycle bin usage records.
//! - **Temp** (`%TEMP%\Arke`): downloads from remote places.
//!
//! The variables are read on every call, never cached, so a process started with `APPDATA` and
//! `LOCALAPPDATA` pointing at a temp profile (the live checks and the crash log test do this)
//! keeps all of its files there. Every function gives `None` when the variable isn't set, and
//! does no file system access (string work only), so it is safe to call from any thread.
//! Callers that must stay out of the real profile in tests (`cfg!(test)`) check that themselves.

use std::path::PathBuf;

/// The folder name under each root.
pub(crate) const APP_DIR: &str = "Arke";

/// `%APPDATA%` itself (the roaming profile), for what lives outside Arke's folder, like the
/// Start menu shortcuts.
pub(crate) fn roaming_root() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

/// `%LOCALAPPDATA%` itself, for what lives outside Arke's folder, like other programs'
/// install folders.
pub(crate) fn local_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// `%APPDATA%\Arke`.
pub(crate) fn roaming_dir() -> Option<PathBuf> {
    roaming_root().map(|root| root.join(APP_DIR))
}

/// `%LOCALAPPDATA%\Arke`.
pub(crate) fn local_dir() -> Option<PathBuf> {
    local_root().map(|root| root.join(APP_DIR))
}

/// `%APPDATA%\Arke\<name>`: a file next to the settings.
pub(crate) fn roaming_file(name: &str) -> Option<PathBuf> {
    roaming_dir().map(|dir| dir.join(name))
}

/// `%LOCALAPPDATA%\Arke\<name>`.
pub(crate) fn local_file(name: &str) -> Option<PathBuf> {
    local_dir().map(|dir| dir.join(name))
}

/// `%APPDATA%\Arke\settings.json`.
pub(crate) fn settings_file() -> Option<PathBuf> {
    roaming_file("settings.json")
}

/// `%APPDATA%\Arke\known_hosts`: the SFTP host keys the user trusted.
pub(crate) fn known_hosts_file() -> Option<PathBuf> {
    roaming_file("known_hosts")
}

/// `%LOCALAPPDATA%\Arke\arke.log`.
pub(crate) fn log_file() -> Option<PathBuf> {
    local_file("arke.log")
}

/// `%LOCALAPPDATA%\Arke\index.bin`: the search index kept between runs.
pub(crate) fn index_cache_file() -> Option<PathBuf> {
    local_file("index.bin")
}

/// `%LOCALAPPDATA%\Arke\menu-icons.bin`: the pinned menu commands' pictures.
pub(crate) fn menu_icons_file() -> Option<PathBuf> {
    local_file("menu-icons.bin")
}

/// `%LOCALAPPDATA%\Arke\thumbs`: the thumbnail disk cache.
pub(crate) fn thumbs_dir() -> Option<PathBuf> {
    local_file("thumbs")
}

/// `%LOCALAPPDATA%\Arke\updates`: downloaded updates.
pub(crate) fn updates_dir() -> Option<PathBuf> {
    local_file("updates")
}

/// `%LOCALAPPDATA%\Arke\bin-usage`: the recycle bin usage records.
pub(crate) fn bin_usage_dir() -> Option<PathBuf> {
    local_file("bin-usage")
}

/// `%TEMP%\Arke\remote`: where remote files are downloaded to. Always has a value (the temp
/// folder falls back to the Windows folder).
pub(crate) fn remote_cache_dir() -> PathBuf {
    std::env::temp_dir().join(APP_DIR).join("remote")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files sit under the right root, with the names other code and the uninstaller know.
    /// Nothing is written, and the assertions hold for any value of the variables, so no test
    /// changes the process's environment.
    #[test]
    fn files_sit_under_their_roots() {
        if let Some(dir) = roaming_dir() {
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some(APP_DIR));
            assert_eq!(settings_file(), Some(dir.join("settings.json")));
            assert_eq!(known_hosts_file(), Some(dir.join("known_hosts")));
            assert_eq!(roaming_file("shelf.json"), Some(dir.join("shelf.json")));
        } else {
            assert_eq!(settings_file(), None);
        }
        if let Some(dir) = local_dir() {
            assert_eq!(log_file(), Some(dir.join("arke.log")));
            assert_eq!(index_cache_file(), Some(dir.join("index.bin")));
            assert_eq!(menu_icons_file(), Some(dir.join("menu-icons.bin")));
            assert_eq!(thumbs_dir(), Some(dir.join("thumbs")));
            assert_eq!(updates_dir(), Some(dir.join("updates")));
            assert_eq!(bin_usage_dir(), Some(dir.join("bin-usage")));
        } else {
            assert_eq!(log_file(), None);
        }
    }

    #[test]
    fn remote_downloads_go_to_the_temp_folder() {
        assert!(remote_cache_dir().ends_with(r"Arke\remote"));
    }
}
