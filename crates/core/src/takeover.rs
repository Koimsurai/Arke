//! "Open folders in Arke": which File Explorer windows Arke takes over, and the bookkeeping for the
//! ones it must leave alone.
//!
//! The shell side (watching new Explorer windows, reading where they are, closing them) lives in the
//! app's `win_takeover.rs`; this module only decides from strings, so it can be tested anywhere.
//!
//! Only plain file-system folders Arke can list are taken over (local, mapped, UNC shares,
//! `\\wsl$`), plus Home / This PC / Quick access (Arke's Home page) and the Recycle Bin. Everything
//! else stays in Explorer: Control Panel, network neighbourhood, phones, inside archives, search
//! results, libraries and any other shell namespace.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where a taken-over Explorer window should open in Arke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A file-system folder.
    Folder(PathBuf),
    /// Home, This PC or Quick access: Arke's Home page.
    Home,
    /// The Recycle Bin page.
    RecycleBin,
}

/// Shell attributes of the folder an Explorer window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShellAttrs {
    /// `SFGAO_FILESYSTEM`: a real file-system location.
    pub filesystem: bool,
    /// `SFGAO_STREAM`: backed by a file, e.g. a .zip shown as a folder.
    pub stream: bool,
}

/// Home (Windows 11).
const HOME: &str = "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}";
/// Quick access (Windows 10).
const QUICK_ACCESS: &str = "::{679F85CB-0220-4080-B29B-5540CC05AAB6}";
/// This PC.
const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
/// Recycle Bin.
const RECYCLE_BIN: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";

/// Folders Explorer shows from inside a file; Arke leaves these to Explorer.
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "cab"];

/// Decides what to do with an Explorer window showing `parsing_name` (the folder's
/// desktop-absolute parsing name). `None` leaves the window to Explorer.
pub fn classify(parsing_name: &str, attrs: ShellAttrs) -> Option<Target> {
    let name = parsing_name.trim();
    if name.starts_with("::") {
        // Only a bare namespace root; anything below one (devices, Control Panel pages) stays.
        let root = name.trim_end_matches('\\');
        return if root.eq_ignore_ascii_case(HOME)
            || root.eq_ignore_ascii_case(QUICK_ACCESS)
            || root.eq_ignore_ascii_case(THIS_PC)
        {
            Some(Target::Home)
        } else if root.eq_ignore_ascii_case(RECYCLE_BIN) {
            Some(Target::RecycleBin)
        } else {
            None
        };
    }
    if !attrs.filesystem || attrs.stream || !is_listable_path(name) || inside_archive(name) {
        return None;
    }
    Some(Target::Folder(PathBuf::from(name)))
}

/// What `arke.exe <argument>` does when the shell starts it as the folder handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Arke shows it.
    Arke(Target),
    /// Arke can't: `explorer.exe` is started with this argument (which bypasses the folder verb,
    /// so nothing loops).
    Explorer(String),
}

/// Decides, before anything is initialized, where a folder argument goes. The shell passes `%1` for
/// every folder it opens, virtual ones included (`::{CLSID}` for Control Panel, This PC, ...).
/// Home, This PC, Quick access, the Recycle Bin and listable paths stay; Control Panel, search
/// folders, URLs, device paths, archives and every other namespace go to Explorer. Plain relative
/// paths (typed on a command line) are left to Arke. Uses [`classify`], so the Explorer window the
/// watcher sees for such a target is one it leaves alone, too.
pub fn route_argument(arg: &str) -> Route {
    let arg = arg.trim().trim_matches('"');
    let lower = arg.to_ascii_lowercase();
    let parsing = if let Some(rest) = lower.strip_prefix("shell:") {
        match rest {
            "recyclebinfolder" => return Route::Arke(Target::RecycleBin),
            "mycomputerfolder" | "homefolder" => return Route::Arke(Target::Home),
            // `shell:::{CLSID}`
            _ if rest.starts_with("::{") => &arg["shell:".len()..],
            _ => return Route::Explorer(arg.to_owned()),
        }
    } else {
        arg
    };
    if parsing.starts_with("::") {
        return classify(parsing, ShellAttrs::default())
            .map_or_else(|| Route::Explorer(format!("shell:{parsing}")), Route::Arke);
    }
    let bytes = parsing.as_bytes();
    let absolute = (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || parsing.starts_with(r"\\");
    if absolute {
        let attrs = ShellAttrs {
            filesystem: true,
            stream: false,
        };
        return classify(parsing, attrs)
            .map_or_else(|| Route::Explorer(parsing.to_owned()), Route::Arke);
    }
    // `search-ms:...`, `ftp://...`, `ms-settings:`: a scheme, not a relative path.
    let scheme = parsing.split_once(':').is_some_and(|(s, _)| {
        s.len() > 1 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    if scheme {
        return Route::Explorer(parsing.to_owned());
    }
    Route::Arke(Target::Folder(PathBuf::from(parsing)))
}

/// `X:\...` or `\\server\share\...`; not device paths (`\\?\`, `\\.\`) or a bare `\\server`.
fn is_listable_path(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return bytes.len() == 2 || bytes[2] == b'\\';
    }
    let Some(rest) = name.strip_prefix(r"\\") else {
        return false;
    };
    if rest.starts_with('?') || rest.starts_with('.') {
        return false;
    }
    let mut parts = rest.split('\\');
    let server = parts.next().unwrap_or_default();
    let share = parts.next().unwrap_or_default();
    !server.is_empty() && !share.is_empty()
}

fn inside_archive(name: &str) -> bool {
    name.split('\\').any(|component| {
        Path::new(component)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ARCHIVE_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
    })
}

/// Whether `path` is `root` or inside it (case-insensitive, whole components only).
pub fn within(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy();
    let root = root.to_string_lossy();
    let root = root.trim_end_matches('\\');
    if root.is_empty() || path.len() < root.len() || !path.is_char_boundary(root.len()) {
        return false;
    }
    let (head, tail) = path.split_at(root.len());
    head.eq_ignore_ascii_case(root) && (tail.is_empty() || tail.starts_with('\\'))
}

/// Whether the window watcher moves an Explorer window showing `target` to Arke. A window that opens
/// at Home or This PC is File Explorer started on purpose (taskbar, Start, `explorer.exe`), so it
/// stays; Win+E reaches Arke through the folder handler instead. Folders and the Recycle Bin, which
/// some program asked Explorer to show, are moved.
pub const fn watcher_takes(target: &Target) -> bool {
    !matches!(target, Target::Home)
}

/// Test-only restriction (`ARKE_TAKEOVER_TEST_ROOT`): with a root set, only folders inside it are
/// taken over, so live checks never touch the tester's own Explorer windows.
pub fn allowed_by_test_root(target: &Target, root: Option<&Path>) -> bool {
    match (root, target) {
        (None, _) => true,
        (Some(root), Target::Folder(path)) => within(path, root),
        (Some(_), _) => false,
    }
}

/// Names of the selected items that sit directly in `folder` (what Arke can select there).
/// `items` are full parsing names; anything elsewhere is dropped, duplicates once.
pub fn selected_names(folder: &Path, items: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in items {
        let item = Path::new(item);
        let (Some(parent), Some(name)) = (item.parent(), item.file_name()) else {
            continue;
        };
        if !same_path(parent, folder) {
            continue;
        }
        let name = name.to_string_lossy().into_owned();
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    names
}

/// Whether `a` and `b` name the same folder (case-insensitive, trailing backslash ignored).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// How long a "leave this one to Explorer" mark stays valid.
pub const BYPASS_TTL: Duration = Duration::from_secs(10);

/// Folders Arke itself just handed to Explorer ("Show in File Explorer"): the next Explorer window
/// on each is left alone once, within [`BYPASS_TTL`].
#[derive(Debug, Default)]
pub struct Bypass {
    entries: Vec<(PathBuf, Instant)>,
}

impl Bypass {
    /// Marks the folder Explorer will show for `path`: the parent when Explorer selects `path`,
    /// otherwise `path` itself.
    pub fn add(&mut self, path: &Path, select: bool, now: Instant) {
        let folder = if select {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        self.prune(now);
        self.entries.push((folder.to_path_buf(), now + BYPASS_TTL));
    }

    /// Whether an Explorer window on `location` should be left alone; uses up the mark.
    pub fn take(&mut self, location: &Path, now: Instant) -> bool {
        self.prune(now);
        let Some(ix) = self
            .entries
            .iter()
            .position(|(folder, _)| same_path(folder, location))
        else {
            return false;
        };
        self.entries.remove(ix);
        true
    }

    /// Number of live marks.
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no marks.
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn prune(&mut self, now: Instant) {
        self.entries.retain(|(_, until)| *until > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: ShellAttrs = ShellAttrs {
        filesystem: true,
        stream: false,
    };
    const VIRTUAL: ShellAttrs = ShellAttrs {
        filesystem: false,
        stream: false,
    };

    fn folder(p: &str) -> Target {
        Target::Folder(PathBuf::from(p))
    }

    #[test]
    fn takes_over_file_system_folders() {
        for p in [
            r"C:\",
            r"C:",
            r"D:\Projects\報告 2026",
            r"Z:\mapped\share",
            r"\\nas\share",
            r"\\nas\share\docs",
            r"\\wsl$\Ubuntu\home",
            r"\\wsl.localhost\Ubuntu",
            r"C:\Users\me\OneDrive",
        ] {
            assert_eq!(classify(p, FS), Some(folder(p)), "{p} should be taken over");
        }
    }

    #[test]
    fn leaves_remote_urls_alone() {
        for p in [
            "sftp://u@h/home",
            r"sftp:\\u@h\home",
            "https://dav.example/files",
        ] {
            assert_eq!(classify(p, FS), None, "{p} is not an Explorer folder");
        }
    }

    #[test]
    fn maps_home_this_pc_and_recycle_bin() {
        assert_eq!(classify(HOME, VIRTUAL), Some(Target::Home));
        assert_eq!(
            classify("::{f874310e-b6b7-47dc-bc84-b9e6b38f5903}", VIRTUAL),
            Some(Target::Home),
            "CLSIDs compare case-insensitively"
        );
        assert_eq!(classify(QUICK_ACCESS, VIRTUAL), Some(Target::Home));
        assert_eq!(classify(THIS_PC, VIRTUAL), Some(Target::Home));
        assert_eq!(classify(RECYCLE_BIN, VIRTUAL), Some(Target::RecycleBin));
    }

    #[test]
    fn leaves_everything_else_to_explorer() {
        let leave = [
            // Control Panel and one of its pages
            ("::{26EE0668-A00A-44D7-9371-BEB064C98683}", VIRTUAL),
            (
                r"::{26EE0668-A00A-44D7-9371-BEB064C98683}\0\::{BB06C0E4-D293-4F75-8A90-CB05B6477EEE}",
                VIRTUAL,
            ),
            // Network neighbourhood, a bare server, printers
            ("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", VIRTUAL),
            (r"\\nas", FS),
            ("::{2227A280-3AEA-1069-A2DE-08002B30309D}", VIRTUAL),
            // A phone under This PC (MTP)
            (
                r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\\?\usb#vid_04e8&pid_6860#r58m#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
                VIRTUAL,
            ),
            // Libraries, Gallery
            (
                r"::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\Documents.library-ms",
                VIRTUAL,
            ),
            ("::{E88865EA-0E1C-4E20-9AA6-EDCD0212C87C}", VIRTUAL),
            // Search results, FTP
            (
                "search-ms:displayname=Search%20Results&crumb=location:C%3A%5C",
                VIRTUAL,
            ),
            ("ftp://example.com/pub", VIRTUAL),
            // Inside archives
            (
                r"C:\Downloads\a.zip",
                ShellAttrs {
                    filesystem: true,
                    stream: true,
                },
            ),
            (r"C:\Downloads\a.zip\inner", VIRTUAL),
            (r"C:\Downloads\b.CAB\x", FS),
            // Device paths, odd strings
            (r"\\?\C:\x", FS),
            (r"\\.\PhysicalDrive0", FS),
            ("", FS),
            ("Desktop", FS),
        ];
        for (p, attrs) in leave {
            assert_eq!(classify(p, attrs), None, "{p} should stay in Explorer");
        }
    }

    #[test]
    fn routes_what_the_shell_passes_as_the_folder_handler() {
        let arke = Route::Arke;
        assert_eq!(route_argument(r"C:\Users\me"), arke(folder(r"C:\Users\me")));
        assert_eq!(route_argument(r#""D:\a b""#), arke(folder(r"D:\a b")));
        assert_eq!(route_argument(r"C:\"), arke(folder(r"C:\")));
        assert_eq!(
            route_argument(r"\\srv\share\x"),
            arke(folder(r"\\srv\share\x"))
        );
        assert_eq!(
            route_argument("docs"),
            arke(folder("docs")),
            "relative stays"
        );
        for home in [
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            "shell:::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}",
            "shell:MyComputerFolder",
        ] {
            assert_eq!(route_argument(home), arke(Target::Home), "{home}");
        }
        for bin in [
            "::{645FF040-5081-101B-9F08-00AA002F954E}",
            "shell:RecycleBinFolder",
        ] {
            assert_eq!(route_argument(bin), arke(Target::RecycleBin), "{bin}");
        }
    }

    #[test]
    fn routes_everything_arke_cannot_show_to_explorer() {
        let explorer = |s: &str| Route::Explorer(s.to_owned());
        // Control Panel (category view), God Mode, Network, libraries.
        assert_eq!(
            route_argument("::{26EE0668-A00A-44D7-9371-BEB064C98683}"),
            explorer("shell:::{26EE0668-A00A-44D7-9371-BEB064C98683}")
        );
        assert_eq!(
            route_argument("::{ED7BA470-8E54-465E-825C-99712043E01C}"),
            explorer("shell:::{ED7BA470-8E54-465E-825C-99712043E01C}")
        );
        assert_eq!(
            route_argument("shell:NetworkPlacesFolder"),
            explorer("shell:NetworkPlacesFolder")
        );
        assert_eq!(
            route_argument("shell:Libraries"),
            explorer("shell:Libraries")
        );
        assert_eq!(
            route_argument("search-ms:query=a&crumb=location:C%3A"),
            explorer("search-ms:query=a&crumb=location:C%3A")
        );
        assert_eq!(route_argument("ftp://host/x"), explorer("ftp://host/x"));
        assert_eq!(route_argument(r"\\?\C:\x"), explorer(r"\\?\C:\x"));
        assert_eq!(route_argument(r"\\server"), explorer(r"\\server"));
        assert_eq!(
            route_argument(r"C:\Temp\a.zip\inner"),
            explorer(r"C:\Temp\a.zip\inner")
        );
    }

    #[test]
    fn what_goes_to_explorer_is_never_taken_back_by_the_watcher() {
        // The Explorer window for these shows the same location; classify (the watcher's rule)
        // leaves it alone.
        for arg in [
            "::{26EE0668-A00A-44D7-9371-BEB064C98683}",
            "shell:NetworkPlacesFolder",
            r"C:\Temp\a.zip\inner",
            r"\\server",
        ] {
            let Route::Explorer(target) = route_argument(arg) else {
                panic!("{arg} should go to Explorer");
            };
            let parsing = target.strip_prefix("shell:").unwrap_or(&target);
            assert_eq!(classify(parsing, ShellAttrs::default()), None, "{arg}");
        }
    }

    #[test]
    fn within_matches_whole_components() {
        let root = Path::new(r"C:\Temp\arke-test");
        assert!(within(Path::new(r"C:\Temp\arke-test"), root));
        assert!(within(Path::new(r"c:\temp\ARKE-TEST\sub\x"), root));
        assert!(within(Path::new(r"C:\Temp\arke-test\"), root));
        assert!(!within(Path::new(r"C:\Temp\arke-test2"), root));
        assert!(!within(Path::new(r"C:\Temp"), root));
        assert!(!within(Path::new(r"D:\Temp\arke-test"), root));
        assert!(within(Path::new(r"C:\x"), Path::new(r"C:\")));
        assert!(!within(Path::new(r"C:\x"), Path::new("")));
    }

    #[test]
    fn explorer_started_on_purpose_is_left_alone() {
        assert!(!watcher_takes(&Target::Home));
        assert!(watcher_takes(&Target::RecycleBin));
        assert!(watcher_takes(&Target::Folder(PathBuf::from(r"C:\Users"))));
    }

    #[test]
    fn test_root_filter_limits_what_is_taken() {
        let root = Path::new(r"C:\Temp\arke-test");
        let inside = Target::Folder(PathBuf::from(r"C:\Temp\arke-test\sub"));
        let outside = Target::Folder(PathBuf::from(r"C:\Temp\arke-test2"));
        assert!(allowed_by_test_root(&inside, None));
        assert!(allowed_by_test_root(&outside, None));
        assert!(allowed_by_test_root(&Target::Home, None));
        assert!(allowed_by_test_root(&inside, Some(root)));
        assert!(!allowed_by_test_root(&outside, Some(root)));
        assert!(!allowed_by_test_root(&Target::Home, Some(root)));
        assert!(!allowed_by_test_root(&Target::RecycleBin, Some(root)));
    }

    #[test]
    fn selected_names_keep_items_of_the_folder() {
        let items = vec![
            r"C:\t\a.txt".to_string(),
            r"c:\T\b.txt".to_string(),
            r"C:\t\sub\deep.txt".to_string(),
            r"C:\t\A.TXT".to_string(),
            r"C:\other\c.txt".to_string(),
            "::{645FF040-5081-101B-9F08-00AA002F954E}".to_string(),
        ];
        assert_eq!(
            selected_names(Path::new(r"C:\t\"), &items),
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert!(selected_names(Path::new(r"C:\t"), &[]).is_empty());
    }

    #[test]
    fn bypass_is_used_once_and_expires() {
        let t0 = Instant::now();
        let mut bypass = Bypass::default();
        bypass.add(Path::new(r"C:\t\a.txt"), true, t0);
        bypass.add(Path::new(r"C:\x"), false, t0);
        assert_eq!(bypass.len(), 2);
        // Selecting a file opens its folder.
        assert!(!bypass.take(Path::new(r"C:\t\a.txt"), t0));
        assert!(bypass.take(Path::new(r"c:\T\"), t0));
        assert!(!bypass.take(Path::new(r"C:\t"), t0), "used up");
        // Expired marks don't count.
        assert!(!bypass.take(Path::new(r"C:\x"), t0 + BYPASS_TTL));
        assert!(bypass.is_empty());
    }

    #[test]
    fn bypass_counts_repeated_reveals() {
        let t0 = Instant::now();
        let mut bypass = Bypass::default();
        bypass.add(Path::new(r"C:\t"), false, t0);
        bypass.add(Path::new(r"C:\t"), false, t0 + Duration::from_secs(1));
        assert!(bypass.take(Path::new(r"C:\t"), t0 + Duration::from_secs(2)));
        assert!(bypass.take(Path::new(r"C:\t"), t0 + Duration::from_secs(2)));
        assert!(!bypass.take(Path::new(r"C:\t"), t0 + Duration::from_secs(2)));
    }
}
