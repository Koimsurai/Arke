//! "Space opens QuickLook": hands Space to QuickLook (QL-Win) when it is installed.
//!
//! QuickLook's command line is `QuickLook.exe "<path>"`. Started that way it toggles its preview
//! of the path: a second instance passes the path to the running one over QuickLook's pipe and
//! exits, so pressing Space again on the same item closes the preview, as Space does in File
//! Explorer with QuickLook. QuickLook runs in its own process; nothing of it is loaded into Arke.
//!
//! It is looked for (no registry) where its installer puts it, `%LOCALAPPDATA%\Programs\QuickLook`
//! (the MSI installs per user), in Program Files (the installer lets the folder be chosen), and in
//! Scoop's `apps\quicklook\current`. The Microsoft Store version has no command-line alias, so it
//! can't be started with a path and isn't offered.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Looks for QuickLook once, in the background, so Settings can offer the option.
pub(crate) fn detect_in_background() {
    crate::explorer::spawn_thread("quicklook-detect", || {
        let found = FOUND.get_or_init(find);
        if let Some(exe) = found {
            log::info!("QuickLook found at {}", exe.display());
        }
    });
}

/// QuickLook's exe, if it was found (`None` also while still looking).
pub(crate) fn found() -> Option<&'static Path> {
    FOUND.get().and_then(Option::as_deref)
}

/// Tests: act as if QuickLook were installed at `exe`.
#[cfg(test)]
pub(crate) fn pretend_found(exe: PathBuf) {
    let _ = FOUND.set(Some(exe));
}

/// Checks the usual install folders. Background thread (touches the disk).
fn find() -> Option<PathBuf> {
    candidates(|name| std::env::var_os(name))
        .into_iter()
        .find(|exe| exe.is_file())
}

/// Where an installed QuickLook would be, from environment variables, most likely first.
fn candidates(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let exe = |dir: PathBuf| dir.join("QuickLook.exe");
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(local) = env("LOCALAPPDATA") {
        push(exe(PathBuf::from(local).join("Programs").join("QuickLook")));
    }
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = env(var) {
            push(exe(PathBuf::from(dir).join("QuickLook")));
        }
    }
    let scoop = env("SCOOP")
        .map(PathBuf::from)
        .or_else(|| env("USERPROFILE").map(|home| PathBuf::from(home).join("scoop")));
    if let Some(scoop) = scoop {
        push(exe(scoop.join("apps").join("quicklook").join("current")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_in_the_install_folders_and_scoop() {
        let env = |name: &str| match name {
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\u\AppData\Local")),
            "ProgramW6432" | "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
            "ProgramFiles(x86)" => Some(OsString::from(r"C:\Program Files (x86)")),
            "USERPROFILE" => Some(OsString::from(r"C:\Users\u")),
            _ => None,
        };
        assert_eq!(
            candidates(env),
            [
                PathBuf::from(r"C:\Users\u\AppData\Local\Programs\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Program Files\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Program Files (x86)\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Users\u\scoop\apps\quicklook\current\QuickLook.exe"),
            ]
        );
        let scoop = |name: &str| (name == "SCOOP").then(|| OsString::from(r"D:\scoop"));
        assert_eq!(
            candidates(scoop),
            [PathBuf::from(
                r"D:\scoop\apps\quicklook\current\QuickLook.exe"
            )]
        );
    }
}
