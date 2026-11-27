//! Converts the TeraCopy and QuickLook switches of Arke before 0.10 (`use_teracopy`,
//! `space_quicklook`) to the generic external copy and preview programs (`external.rs`), so a user
//! who had one on keeps the same hand-off: it finds where that program is, once, while reading a
//! settings file that still has the old switch on. Nothing else uses it; it can go once settings
//! files from 0.9 no longer need converting.
//!
//! - TeraCopy: `TeraCopy\TeraCopy.exe` in the Program Files folders. Its documented command line
//!   is `TeraCopy.exe Copy|Move <source> <target>`, where the source may be `*"<list file>"`.
//! - QuickLook: `QuickLook.exe` in its default install folders (per user under
//!   `%LOCALAPPDATA%\Programs`, Program Files, Scoop's `apps\quicklook\current`), then in the
//!   setup exe's uninstall entry (`InstallLocation`, read only), then through its Start menu and
//!   Startup shortcuts, which follow a folder chosen in the installer or a portable copy.
//!   Arke wrote one line to QuickLook's named pipe, `\\.\pipe\QuickLook.App.Pipe.<user SID>`
//!   (`QuickLook.App.PipeMessages.Toggle|<path>`, UTF-8), and started `QuickLook.exe "<path>"`
//!   when the pipe wasn't there. [`quicklook_setting`] is that, in the pipe form of the external
//!   preview program with the exe as its fallback.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The arguments that make TeraCopy run a copy or move as Arke handed it before.
pub(crate) const TERACOPY_ARGS: &str = "{op} *{list} {dest}";
/// What QuickLook is sent for a preview: its pipe, then the line it reads (`external.rs`).
const QUICKLOOK_PIPE: &str =
    r"pipe:\\.\pipe\QuickLook.App.Pipe.{sid} QuickLook.App.PipeMessages.Toggle|{file}";

/// The external preview setting that does what `space_quicklook` did: the pipe, then QuickLook's
/// exe as the fallback when it was found. Without the exe the pipe stands alone, and Space opens
/// Arke's own Quick Look while QuickLook isn't running.
pub(crate) fn quicklook_setting(exe: Option<&Path>) -> String {
    match exe {
        Some(exe) => format!(
            "{QUICKLOOK_PIPE} || {}",
            crate::external::command_line(exe, "{file}")
        ),
        None => QUICKLOOK_PIPE.to_owned(),
    }
}

/// QuickLook's uninstall entry, written by its setup exe (MicaSetup, `KeyName` "QuickLook").
const UNINSTALL_KEYS: [&str; 2] = [
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\QuickLook",
    r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\QuickLook",
];

/// A program an old switch handed work to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    TeraCopy,
    QuickLook,
}

/// Where `tool` is installed, if it is found. Settings-load thread (touches the disk, reads the
/// registry, reads shortcuts with COM); only for a file that still has the old switch on.
pub(crate) fn find(tool: Tool) -> Option<PathBuf> {
    let env = |name: &str| std::env::var_os(name);
    match tool {
        Tool::TeraCopy => teracopy_candidates(env)
            .into_iter()
            .find(|exe| exe.is_file()),
        Tool::QuickLook => find_quicklook(),
    }
}

/// Where an installed TeraCopy would be, from the Program Files environment variables.
fn teracopy_candidates(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = env(var) {
            let exe = PathBuf::from(dir).join("TeraCopy").join("TeraCopy.exe");
            if !out.contains(&exe) {
                out.push(exe);
            }
        }
    }
    out
}

/// Looks in QuickLook's install folders, its uninstall entry and its shortcuts, in that order.
fn find_quicklook() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let installed = || {
        UNINSTALL_KEYS.iter().find_map(|key| {
            [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
                .into_iter()
                .find_map(|root| crate::win::read_reg_string(root, key, "InstallLocation"))
                .filter(|dir| !dir.trim().is_empty())
        })
    };
    find_quicklook_with(
        |name| std::env::var_os(name),
        installed,
        |link| crate::win::read_shortcut(link).map(|(target, _)| target),
    )
}

/// [`find_quicklook`] with the environment, the uninstall entry's folder and shortcut reading
/// given.
fn find_quicklook_with(
    env: impl Fn(&str) -> Option<OsString>,
    install_location: impl FnOnce() -> Option<String>,
    shortcut_target: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let is_exe = |path: &Path| {
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("QuickLook.exe"))
            && path.is_file()
    };
    if let Some(exe) = quicklook_candidates(&env)
        .into_iter()
        .find(|exe| exe.is_file())
    {
        return Some(exe);
    }
    if let Some(exe) = install_location()
        .map(|dir| PathBuf::from(dir.trim().trim_matches('"')).join("QuickLook.exe"))
        .filter(|exe| is_exe(exe))
    {
        return Some(exe);
    }
    quicklook_shortcuts(&env)
        .into_iter()
        .filter(|link| link.is_file())
        .filter_map(|link| shortcut_target(&link))
        .find(|target| is_exe(target))
}

/// QuickLook's shortcuts: the Start menu one its installers make (per user for the MSI, for all
/// users for a per-machine setup) and the Startup one it makes itself for "Run at startup".
fn quicklook_shortcuts(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let programs = |root: OsString| {
        PathBuf::from(root)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    };
    let mut out = Vec::new();
    if let Some(roaming) = env("APPDATA") {
        let programs = programs(roaming);
        out.push(programs.join("QuickLook.lnk"));
        out.push(programs.join("Startup").join("QuickLook.lnk"));
    }
    if let Some(common) = env("ProgramData") {
        out.push(programs(common).join("QuickLook.lnk"));
    }
    out
}

/// Where an installed QuickLook would be, from environment variables, most likely first.
fn quicklook_candidates(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
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
    fn teracopy_is_looked_for_in_both_program_files_folders_once() {
        let env = |name: &str| match name {
            "ProgramW6432" | "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
            "ProgramFiles(x86)" => Some(OsString::from(r"C:\Program Files (x86)")),
            _ => None,
        };
        assert_eq!(
            teracopy_candidates(env),
            [
                PathBuf::from(r"C:\Program Files\TeraCopy\TeraCopy.exe"),
                PathBuf::from(r"C:\Program Files (x86)\TeraCopy\TeraCopy.exe"),
            ]
        );
    }

    #[test]
    fn quicklook_is_looked_for_in_its_install_folders_and_scoop() {
        let env = |name: &str| match name {
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\u\AppData\Local")),
            "ProgramW6432" | "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
            "ProgramFiles(x86)" => Some(OsString::from(r"C:\Program Files (x86)")),
            "USERPROFILE" => Some(OsString::from(r"C:\Users\u")),
            _ => None,
        };
        assert_eq!(
            quicklook_candidates(env),
            [
                PathBuf::from(r"C:\Users\u\AppData\Local\Programs\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Program Files\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Program Files (x86)\QuickLook\QuickLook.exe"),
                PathBuf::from(r"C:\Users\u\scoop\apps\quicklook\current\QuickLook.exe"),
            ]
        );
        let scoop = |name: &str| (name == "SCOOP").then(|| OsString::from(r"D:\scoop"));
        assert_eq!(
            quicklook_candidates(scoop),
            [PathBuf::from(
                r"D:\scoop\apps\quicklook\current\QuickLook.exe"
            )]
        );
        let env = |name: &str| match name {
            "APPDATA" => Some(OsString::from(r"C:\Users\u\AppData\Roaming")),
            "ProgramData" => Some(OsString::from(r"C:\ProgramData")),
            _ => None,
        };
        let menu = r"Microsoft\Windows\Start Menu\Programs";
        assert_eq!(
            quicklook_shortcuts(env),
            [
                PathBuf::from(format!(r"C:\Users\u\AppData\Roaming\{menu}\QuickLook.lnk")),
                PathBuf::from(format!(
                    r"C:\Users\u\AppData\Roaming\{menu}\Startup\QuickLook.lnk"
                )),
                PathBuf::from(format!(r"C:\ProgramData\{menu}\QuickLook.lnk")),
            ]
        );
    }

    /// A sandbox folder under %TEMP%, removed on drop.
    struct Sandbox(
        PathBuf,
        #[allow(dead_code, reason = "held for its Drop, which deletes the folder")]
        tempfile::TempDir,
    );

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let guard = tempfile::Builder::new()
                .prefix(&format!("arke-test-legacy-tools-{tag}-"))
                .tempdir()
                .expect("sandbox");
            Self(guard.path().to_path_buf(), guard)
        }

        /// An empty file at `rel`, with its folders.
        fn file(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("folders");
            std::fs::write(&path, b"").expect("file");
            path
        }

        /// The environment of a user whose profile folders are all in the sandbox.
        fn env(&self) -> impl Fn(&str) -> Option<OsString> + '_ {
            |name: &str| {
                let sub = match name {
                    "LOCALAPPDATA" => "local",
                    "APPDATA" => "roaming",
                    "ProgramData" => "programdata",
                    "ProgramW6432" | "ProgramFiles" | "ProgramFiles(x86)" => "programfiles",
                    "USERPROFILE" => "home",
                    _ => return None,
                };
                Some(self.0.join(sub).into_os_string())
            }
        }
    }

    #[test]
    fn a_quicklook_folder_chosen_in_the_installer_is_found_through_its_shortcut() {
        let sb = Sandbox::new("menu");
        let none = || None;
        assert_eq!(
            find_quicklook_with(sb.env(), none, |_| panic!("no shortcut yet")),
            None
        );
        let exe = sb.file(r"D-Apps\QuickLook\QuickLook.exe");
        let link = sb.file(r"roaming\Microsoft\Windows\Start Menu\Programs\QuickLook.lnk");
        let target = |l: &Path| (l == link).then(|| exe.clone());
        assert_eq!(
            find_quicklook_with(sb.env(), none, target),
            Some(exe.clone())
        );
        // A shortcut to something else doesn't count.
        let other = sb.file(r"D-Apps\Other\Other.exe");
        assert_eq!(
            find_quicklook_with(sb.env(), none, |_| Some(other.clone())),
            None
        );
        // The default folder comes first.
        let default = sb.file(r"local\Programs\QuickLook\QuickLook.exe");
        assert_eq!(find_quicklook_with(sb.env(), none, target), Some(default));
    }

    #[test]
    fn quicklooks_setup_install_location_is_used() {
        let sb = Sandbox::new("arp");
        let exe = sb.file(r"Tools\QuickLook\QuickLook.exe");
        let dir = exe.parent().expect("dir").display().to_string();
        let no_link = |_: &Path| -> Option<PathBuf> { panic!("the uninstall entry was enough") };
        assert_eq!(
            find_quicklook_with(sb.env(), || Some(format!("\"{dir}\\\"")), no_link),
            Some(exe)
        );
    }
}
