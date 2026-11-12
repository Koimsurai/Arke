//! Path string helpers: path bar input, breadcrumbs, file name validation and unique names.

use rust_i18n::t;
use std::path::{Component, Path, PathBuf, Prefix};

/// Turns path bar input into an absolute path. String handling only; never touches the
/// file system (network paths could block). Environment variables are expanded in the app
/// layer since that needs the Windows API.
pub fn resolve_input(current: &Path, input: &str) -> Option<PathBuf> {
    let trimmed = input.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(url) = crate::remote::RemoteUrl::parse_input(trimmed) {
        return Some(url.to_path());
    }
    // Relative input inside a remote folder stays on that server.
    if let Some(here) = crate::remote::RemoteUrl::from_path(current) {
        return Some(here.join(trimmed).to_path());
    }
    let mut s = trimmed.replace('/', "\\");
    // "C:" -> "C:\"; bare "C:" means the drive's current directory, which is surprising.
    if s.len() == 2 && s.ends_with(':') {
        s.push('\\');
    }
    let p = PathBuf::from(&s);
    let joined = if p.is_absolute() || s.starts_with("\\\\") {
        p
    } else {
        current.join(p)
    };
    Some(normalize(&joined))
}

/// Removes `.` and resolves `..` without following symlinks.
pub fn normalize(path: &Path) -> PathBuf {
    if let Some(url) = crate::remote::RemoteUrl::from_path(path) {
        return url.to_path();
    }
    // `\\server` has no prefix in `std::path`, so rebuilding it from components would drop a
    // backslash.
    if let Some(server) = crate::net::bare_server(path) {
        return PathBuf::from(format!(r"\\{server}"));
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never go above the drive root.
                if out.parent().is_some() {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Parent directory. A UNC share root (\\server\share) goes up to its server (\\server). `None`
/// for a drive root (C:\), a server (\\server) and a remote server's root.
pub fn parent(path: &Path) -> Option<PathBuf> {
    if let Some(url) = crate::remote::RemoteUrl::from_path(path) {
        return url.parent().map(|u| u.to_path());
    }
    if crate::net::bare_server(path).is_some() {
        return None;
    }
    if let Some(parent) = path.parent() {
        return Some(parent.to_path_buf());
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(server, _) => {
                let mut s = std::ffi::OsString::from(r"\\");
                s.push(server);
                Some(PathBuf::from(s))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Name of the item in its parent folder: the last component, or the share name of a UNC share
/// root (`\\server\docs` -> `docs`, which is what its server lists). `None` for drive roots.
pub fn leaf_name(path: &Path) -> Option<String> {
    if let Some(name) = path.file_name() {
        return Some(name.to_string_lossy().into_owned());
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(_, share) => Some(share.to_string_lossy().into_owned()),
            _ => None,
        },
        _ => None,
    }
}

/// Path bar text with the quotes "Copy as path" adds removed: `"C:\a b\c.txt"` -> `C:\a b\c.txt`.
/// Only a single quoted string counts; `"C:\Program Files\app.exe" --flag` is left alone (a
/// command with arguments).
pub fn unquote(input: &str) -> &str {
    let trimmed = input.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|inner| !inner.contains('"'))
        .map_or(trimmed, str::trim)
}

/// Breadcrumbs: display name and full path of each segment.
pub fn breadcrumbs(path: &Path) -> Vec<(String, PathBuf)> {
    if let Some(url) = crate::remote::RemoteUrl::from_path(path) {
        return url.breadcrumbs();
    }
    if let Some(server) = crate::net::bare_server(path) {
        return vec![(server.to_string(), normalize(path))];
    }
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => {
                acc.push(comp.as_os_str());
                let label = match prefix.kind() {
                    // The server and the share are separate segments, like Explorer: the server
                    // lists its shares.
                    Prefix::UNC(server, share) => {
                        let mut root = std::ffi::OsString::from(r"\\");
                        root.push(server);
                        out.push((server.to_string_lossy().into_owned(), PathBuf::from(root)));
                        share.to_string_lossy().into_owned()
                    }
                    Prefix::VerbatimUNC(server, share) => format!(
                        "\\\\{}\\{}",
                        server.to_string_lossy(),
                        share.to_string_lossy()
                    ),
                    Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                        format!("{}:", letter as char)
                    }
                    _ => comp.as_os_str().to_string_lossy().into_owned(),
                };
                out.push((label, acc.clone()));
            }
            Component::RootDir => {
                acc.push("\\");
                if let Some(last) = out.last_mut() {
                    last.1.clone_from(&acc);
                } else {
                    out.push(("\\".into(), acc.clone()));
                }
            }
            Component::Normal(name) => {
                acc.push(name);
                out.push((name.to_string_lossy().into_owned(), acc.clone()));
            }
            Component::CurDir | Component::ParentDir => {}
        }
    }
    out
}

/// UNC path (network share).
pub fn is_unc(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

/// The `\\?\` form of an absolute path, which Windows' file functions take literally: no
/// 260-character limit, and a trailing period or space, or a device name like `con`, stays part of
/// the name instead of being stripped or turned into a device. `.` and `..` are resolved first,
/// since they would be taken literally too. Relative and already-verbatim paths come back as they
/// are.
pub fn verbatim(path: &Path) -> PathBuf {
    use std::ffi::{OsStr, OsString};
    let mut parts = path.components();
    let mut out = match parts.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) => OsString::from(format!(r"\\?\{}:", char::from(letter))),
            Prefix::UNC(server, share) => {
                let mut out = OsString::from(r"\\?\UNC\");
                out.push(server);
                out.push(r"\");
                out.push(share);
                out
            }
            _ => return path.to_path_buf(),
        },
        _ => return path.to_path_buf(),
    };
    if !matches!(parts.next(), Some(Component::RootDir)) {
        // `C:foo`, relative to the drive's current folder.
        return path.to_path_buf();
    }
    let mut names: Vec<&OsStr> = Vec::new();
    for part in parts {
        match part {
            Component::Normal(name) => names.push(name),
            Component::ParentDir => {
                names.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if names.is_empty() {
        out.push(r"\");
    }
    for name in names {
        out.push(r"\");
        out.push(name);
    }
    PathBuf::from(out)
}

/// The usual form of a `\\?\` path (`C:\…`, `\\server\share\…`), for showing it.
pub fn unverbatim(path: &Path) -> PathBuf {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path.to_path_buf();
    };
    let rest = || path.components().skip(1).collect::<PathBuf>();
    match prefix.kind() {
        Prefix::VerbatimDisk(letter) => {
            PathBuf::from(format!("{}:", char::from(letter))).join(rest())
        }
        Prefix::VerbatimUNC(server, share) => {
            let mut root = std::ffi::OsString::from(r"\\");
            root.push(server);
            root.push(r"\");
            root.push(share);
            PathBuf::from(root).join(rest())
        }
        _ => path.to_path_buf(),
    }
}

/// Whether `path` is `top` or inside it, compared the way Windows does (ignoring case, either
/// slash, a trailing slash). String handling only.
pub fn is_same_or_inside(path: &Path, top: &Path) -> bool {
    let plain = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let (path, top) = (plain(path), plain(top));
    path == top
        || path
            .strip_prefix(&top)
            .is_some_and(|rest| rest.starts_with('\\'))
}

/// Whether the Windows shell (Explorer's copy engine, the Recycle Bin) chokes on this name even
/// though NTFS stores it: a trailing period or space, or a device name such as `con` or `nul`.
/// Such names come from WSL, Git or archives; only `\\?\` paths reach them ([`verbatim`]).
pub fn shell_unsafe_name(name: &str) -> bool {
    matches!(
        validate_file_name(name),
        Err(NameError::Trailing | NameError::Reserved)
    )
}

/// A move between related folders, for synchronized browsing in dual pane: the other pane repeats
/// it from its own folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathMove {
    /// Into these subfolders, outermost first.
    Down(Vec<std::ffi::OsString>),
    /// Up this many levels.
    Up(usize),
}

/// The components of `path` after normalizing, as lowercase strings for comparing (Windows
/// paths are case-insensitive) next to the original components.
fn parts(path: &Path) -> Vec<(String, std::ffi::OsString)> {
    normalize(path)
        .components()
        .filter(|c| !matches!(c, Component::RootDir))
        .map(|c| {
            let os = c.as_os_str().to_os_string();
            (os.to_string_lossy().to_lowercase(), os)
        })
        .collect()
}

/// How `to` relates to `from`: inside it (down) or above it (up). `None` when they're the same
/// folder or unrelated (a sibling, another drive), since there is no move to repeat.
/// String handling only.
pub fn path_move(from: &Path, to: &Path) -> Option<PathMove> {
    let (from, to) = (parts(from), parts(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a.0 == b.0).count();
    // The roots differ (C: and D:): nothing in common.
    if common == 0 {
        return None;
    }
    if common == from.len() && to.len() > common {
        return Some(PathMove::Down(
            to[common..].iter().map(|(_, os)| os.clone()).collect(),
        ));
    }
    if common == to.len() && from.len() > common {
        return Some(PathMove::Up(from.len() - common));
    }
    None
}

/// Where `base` ends up after `step`. `None` if going up would pass the drive or share root.
pub fn apply_move(base: &Path, step: &PathMove) -> Option<PathBuf> {
    match step {
        PathMove::Down(names) => {
            let mut out = base.to_path_buf();
            out.extend(names);
            Some(out)
        }
        // A server folder stops at the server's root, never at a bare `sftp:`.
        PathMove::Up(levels) if crate::remote::is_remote(base) => {
            let mut url = crate::remote::RemoteUrl::from_path(base)?;
            for _ in 0..*levels {
                url = url.parent()?;
            }
            Some(url.to_path())
        }
        PathMove::Up(levels) => {
            let mut out = normalize(base);
            for _ in 0..*levels {
                if !out.pop() {
                    return None;
                }
            }
            Some(out)
        }
    }
}

/// Why a file name is not valid on Windows. Display is the localized, user-facing message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Empty or only whitespace.
    #[error("{}", t!("name.empty"))]
    Empty,
    /// Contains `\ / : * ? " < > |` or a control character.
    #[error("{}", t!("name.bad_chars"))]
    BadChars,
    /// Ends with a period or a space.
    #[error("{}", t!("name.trailing"))]
    Trailing,
    /// Longer than 255 characters.
    #[error("{}", t!("name.too_long"))]
    TooLong,
    /// A reserved device name (CON, NUL, COM1...), with or without an extension.
    #[error("{}", t!("name.reserved"))]
    Reserved,
}

/// Checks that a file name is valid on Windows.
pub fn validate_file_name(name: &str) -> Result<(), NameError> {
    if name.trim().is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().any(|c| {
        matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()
    }) {
        return Err(NameError::BadChars);
    }
    if name.ends_with(['.', ' ']) {
        return Err(NameError::Trailing);
    }
    if name.chars().count() > 255 {
        return Err(NameError::TooLong);
    }
    // Reserved device names in any case and with any extension, e.g. CON, nul.txt, COM1.
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    let numbered = (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.len() == 4
        && matches!(stem.as_bytes()[3], b'1'..=b'9');
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered {
        return Err(NameError::Reserved);
    }
    Ok(())
}

/// Unique name: `New folder`, `New folder (2)`, `New folder (3)`, ...
/// `exists` receives lowercase names (Windows names are case-insensitive).
pub fn unique_name(base: &str, exists: impl Fn(&str) -> bool) -> String {
    let taken = |candidate: &str| exists(&candidate.to_lowercase());
    if !taken(base) {
        return base.to_owned();
    }
    // The cap is only a safety net.
    (2..100_000u32)
        .map(|n| format!("{base} ({n})"))
        .find(|c| !taken(c))
        .unwrap_or_else(|| base.to_owned())
}

/// Like [`unique_name`] but numbers before the extension ("New Text Document (2).txt"),
/// as Explorer does.
pub fn unique_file_name(stem: &str, ext: &str, exists: impl Fn(&str) -> bool) -> String {
    let taken = |candidate: &str| exists(&candidate.to_lowercase());
    let first = format!("{stem}.{ext}");
    if !taken(&first) {
        return first;
    }
    (2..100_000u32)
        .map(|n| format!("{stem} ({n}).{ext}"))
        .find(|c| !taken(c))
        .unwrap_or(first)
}

/// Default selection when renaming, as a byte range: the stem for files, everything for folders.
pub fn rename_selection(name: &str, is_dir: bool) -> std::ops::Range<usize> {
    if is_dir {
        return 0..name.len();
    }
    match name.rfind('.') {
        // Dot-prefixed names like ".gitignore" are selected whole.
        Some(0) | None => 0..name.len(),
        Some(dot) => 0..dot,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn down(names: &[&str]) -> PathMove {
        PathMove::Down(names.iter().map(std::ffi::OsString::from).collect())
    }

    #[test]
    fn path_moves_between_related_folders() {
        let p = Path::new;
        assert_eq!(
            path_move(p(r"C:\a"), p(r"C:\a\b\c")),
            Some(down(&["b", "c"]))
        );
        assert_eq!(path_move(p(r"C:\a\b\c"), p(r"C:\a")), Some(PathMove::Up(2)));
        assert_eq!(path_move(p(r"C:\a"), p(r"C:\")), Some(PathMove::Up(1)));
        assert_eq!(path_move(p(r"C:\"), p(r"C:\Users")), Some(down(&["Users"])));
        // Case and trailing separators don't matter; the new folder keeps its own spelling.
        assert_eq!(
            path_move(p(r"c:\DATA\"), p(r"C:\data\Photos")),
            Some(down(&["Photos"]))
        );
        assert_eq!(
            path_move(p(r"\\nas\share"), p(r"\\NAS\Share\x")),
            Some(down(&["x"]))
        );
        // Same folder, siblings and other drives are not moves to repeat.
        assert_eq!(path_move(p(r"C:\a"), p(r"C:\A\")), None);
        assert_eq!(path_move(p(r"C:\a\b"), p(r"C:\a\c")), None);
        assert_eq!(path_move(p(r"C:\a"), p(r"D:\a\b")), None);
    }

    #[test]
    fn moves_apply_to_the_other_folder() {
        let p = Path::new;
        assert_eq!(
            apply_move(p(r"D:\backup"), &down(&["b", "c"])),
            Some(PathBuf::from(r"D:\backup\b\c"))
        );
        assert_eq!(
            apply_move(p(r"D:\x\y\z"), &PathMove::Up(2)),
            Some(PathBuf::from(r"D:\x"))
        );
        assert_eq!(
            apply_move(p(r"D:\x"), &PathMove::Up(1)),
            Some(PathBuf::from(r"D:\"))
        );
        assert_eq!(
            apply_move(p(r"D:\x"), &PathMove::Up(2)),
            None,
            "past the root"
        );
        assert_eq!(apply_move(p(r"\\nas\share"), &PathMove::Up(1)), None);
        // Server folders: up stops at the server root; local and server folders are unrelated.
        assert_eq!(
            apply_move(p("sftp://u@h/a/b"), &PathMove::Up(1)),
            Some(PathBuf::from("sftp://u@h/a"))
        );
        assert_eq!(apply_move(p("sftp://u@h/a"), &PathMove::Up(2)), None);
        assert_eq!(
            path_move(p("sftp://u@h/a"), p("sftp://u@h/a/b")),
            Some(down(&["b"]))
        );
        assert_eq!(path_move(p(r"C:\a"), p("sftp://u@h/a")), None);
    }

    #[test]
    fn resolve() {
        let cur = Path::new(r"C:\Users\me");
        assert_eq!(resolve_input(cur, "D:"), Some(PathBuf::from(r"D:\")));
        assert_eq!(
            resolve_input(cur, "..\\you"),
            Some(PathBuf::from(r"C:\Users\you"))
        );
        assert_eq!(
            resolve_input(cur, "\"C:/Windows\""),
            Some(PathBuf::from(r"C:\Windows"))
        );
        assert_eq!(resolve_input(cur, r"C:\..\.."), Some(PathBuf::from(r"C:\")));
        assert_eq!(resolve_input(cur, "  "), None);
        assert_eq!(
            resolve_input(cur, "sftp://u@h/home/My%20Docs/"),
            Some(PathBuf::from("sftp://u@h/home/My Docs"))
        );
        let remote = Path::new("sftp://h/home/u");
        assert_eq!(
            resolve_input(remote, "../v"),
            Some(PathBuf::from("sftp://h/home/v"))
        );
        assert_eq!(
            parent(Path::new("sftp://h/a")),
            Some(PathBuf::from("sftp://h"))
        );
        assert_eq!(parent(Path::new("sftp://h/")), None);
        assert_eq!(breadcrumbs(Path::new("https://h/a"))[0].0, "h");
    }

    #[test]
    fn crumbs() {
        let c = breadcrumbs(Path::new(r"C:\Users\me"));
        let labels: Vec<_> = c.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["C:", "Users", "me"]);
        assert_eq!(c[0].1, PathBuf::from(r"C:\"));
        assert_eq!(c[2].1, PathBuf::from(r"C:\Users\me"));
        let u = breadcrumbs(Path::new(r"\\srv\share\dir"));
        let labels: Vec<_> = u.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["srv", "share", "dir"]);
        assert_eq!(u[0].1, PathBuf::from(r"\\srv"));
        assert_eq!(u[1].1, PathBuf::from(r"\\srv\share\"));
        assert_eq!(u[2].1, PathBuf::from(r"\\srv\share\dir"));
        assert!(is_unc(Path::new(r"\\srv\share")));
        assert_eq!(parent(Path::new(r"C:\")), None);
    }

    #[test]
    fn servers_list_their_shares() {
        let cur = Path::new(r"C:\Users\me");
        assert_eq!(
            resolve_input(cur, r"\\192.0.2.10"),
            Some(PathBuf::from(r"\\192.0.2.10"))
        );
        assert_eq!(
            resolve_input(cur, r"\\nas\"),
            Some(PathBuf::from(r"\\nas")),
            "one spelling, so history and the sidebar compare right"
        );
        assert_eq!(resolve_input(cur, "//nas"), Some(PathBuf::from(r"\\nas")));
        assert_eq!(
            breadcrumbs(Path::new(r"\\nas")),
            [("nas".to_string(), PathBuf::from(r"\\nas"))]
        );
        // Up: folder -> share -> server, and no further.
        assert_eq!(
            parent(Path::new(r"\\nas\docs\a")),
            Some(PathBuf::from(r"\\nas\docs\"))
        );
        assert_eq!(
            parent(Path::new(r"\\nas\docs\")),
            Some(PathBuf::from(r"\\nas"))
        );
        assert_eq!(
            parent(Path::new(r"\\nas\docs")),
            Some(PathBuf::from(r"\\nas"))
        );
        assert_eq!(parent(Path::new(r"\\nas")), None);
        assert_eq!(parent(Path::new(r"\\?\UNC\nas\docs")), None);
        assert_eq!(
            leaf_name(Path::new(r"\\nas\docs\")).as_deref(),
            Some("docs")
        );
        assert_eq!(leaf_name(Path::new(r"\\nas\docs\a")).as_deref(), Some("a"));
        assert_eq!(leaf_name(Path::new(r"C:\")), None);
    }

    #[test]
    fn pasted_paths_lose_their_quotes() {
        assert_eq!(unquote(r#" "C:\a b\c.docx" "#), r"C:\a b\c.docx");
        assert_eq!(
            unquote(r#""\\192.0.2.10\team\報告\Q4 Review — 11月.docx""#),
            r"\\192.0.2.10\team\報告\Q4 Review — 11月.docx"
        );
        assert_eq!(unquote(r"C:\plain"), r"C:\plain");
        assert_eq!(
            unquote(r#""C:\Program Files\app.exe" --flag"#),
            r#""C:\Program Files\app.exe" --flag"#,
            "a quoted program with arguments stays a command"
        );
        assert_eq!(unquote(r#""a" "b""#), r#""a" "b""#);
        assert_eq!(unquote(r#"""#), r#"""#);
        let cur = Path::new(r"C:\Users\me");
        assert_eq!(
            resolve_input(cur, r#""\\nas\docs\週報 — v2.docx""#),
            Some(PathBuf::from(r"\\nas\docs\週報 — v2.docx"))
        );
    }

    #[test]
    fn validates_windows_file_names() {
        assert!(validate_file_name("Report 2026.docx").is_ok());
        assert!(validate_file_name("a.b.c").is_ok());
        assert_eq!(validate_file_name("  "), Err(NameError::Empty));
        for bad in [
            "a/b", r"a\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b", "a\tb",
        ] {
            assert_eq!(validate_file_name(bad), Err(NameError::BadChars), "{bad:?}");
        }
        assert_eq!(validate_file_name("name."), Err(NameError::Trailing));
        assert_eq!(validate_file_name("name "), Err(NameError::Trailing));
        assert_eq!(
            validate_file_name(&"x".repeat(256)),
            Err(NameError::TooLong)
        );
        for reserved in ["CON", "con.txt", "Nul", "COM1", "lpt9.log"] {
            assert_eq!(
                validate_file_name(reserved),
                Err(NameError::Reserved),
                "{reserved}"
            );
        }
        assert!(validate_file_name("COM0").is_ok());
        assert!(validate_file_name("CONSOLE.txt").is_ok());
    }

    #[test]
    fn unique_names_like_explorer() {
        let existing = ["Photos", "Photos (2)"];
        let exists = |n: &str| existing.iter().any(|e| e.to_lowercase() == n);
        assert_eq!(unique_name("Photos", exists), "Photos (3)");
        assert_eq!(unique_name("Other", exists), "Other");
        let ci = |n: &str| n == "new folder";
        assert_eq!(unique_name("New Folder", ci), "New Folder (2)");
    }

    #[test]
    fn unique_file_names_keep_extension() {
        let existing = ["New Text Document.txt", "New Text Document (2).txt"];
        let exists = |n: &str| existing.iter().any(|e| e.to_lowercase() == n);
        assert_eq!(
            unique_file_name("New Text Document", "txt", exists),
            "New Text Document (3).txt"
        );
        assert_eq!(unique_file_name("a", "txt", |_| false), "a.txt");
    }

    #[test]
    fn rename_selects_stem() {
        // The range is in bytes: a multi-byte stem must not be counted in chars.
        assert_eq!(rename_selection("報告.docx", false), 0.."報告".len());
        assert_eq!(
            rename_selection("archive.tar.gz", false),
            0.."archive.tar".len()
        );
        assert_eq!(rename_selection(".gitignore", false), 0..10);
        assert_eq!(rename_selection("folder.v2", true), 0..9);
    }

    #[test]
    fn verbatim_paths_keep_names_literal() {
        let v = |s: &str| verbatim(Path::new(s));
        assert_eq!(v(r"C:\Temp\con"), PathBuf::from(r"\\?\C:\Temp\con"));
        assert_eq!(v(r"C:\Temp\x.\y "), PathBuf::from(r"\\?\C:\Temp\x.\y "));
        assert_eq!(v("C:/a/./b/../c"), PathBuf::from(r"\\?\C:\a\c"));
        assert_eq!(v(r"C:\"), PathBuf::from(r"\\?\C:\"));
        assert_eq!(
            v(r"\\server\share\dir\f.txt"),
            PathBuf::from(r"\\?\UNC\server\share\dir\f.txt")
        );
        // Already verbatim, relative or drive-relative: unchanged.
        assert_eq!(v(r"\\?\C:\a"), PathBuf::from(r"\\?\C:\a"));
        assert_eq!(v(r"rel\a"), PathBuf::from(r"rel\a"));
        assert_eq!(v("C:rel"), PathBuf::from("C:rel"));
        // And back.
        for plain in [r"C:\Temp\con", r"\\server\share\dir\f.txt", r"rel\a"] {
            assert_eq!(unverbatim(&v(plain)), PathBuf::from(plain));
        }
    }

    #[test]
    fn inside_compares_like_windows() {
        let inside = |a: &str, b: &str| is_same_or_inside(Path::new(a), Path::new(b));
        assert!(inside(r"C:\t\Folder\x.txt", r"c:\t\folder"));
        assert!(inside("C:/t/folder/", r"C:\t\Folder"));
        assert!(inside(r"C:\t\a", r"C:\t\a"));
        assert!(!inside(r"C:\t\Folder2\x", r"C:\t\Folder"));
        assert!(!inside(r"C:\t", r"C:\t\Folder"));
    }

    #[test]
    fn names_the_shell_cannot_handle() {
        for name in ["x.", "y ", "con", "NUL", "aux.txt", "com1"] {
            assert!(shell_unsafe_name(name), "{name}");
        }
        for name in ["a.txt", ".gitignore", "console", "x.y"] {
            assert!(!shell_unsafe_name(name), "{name}");
        }
    }
}
