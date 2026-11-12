//! Symlinks and junctions: where they point.
//!
//! Reading a link's target is file system I/O (it opens the reparse point), so call these from a
//! background thread. Nothing here follows a link into its target.

use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

/// The target of the symlink or junction at `link`, as an absolute path without the `\\?\` prefix.
/// Relative symlink targets are resolved against the link's folder (`..` included, without asking
/// the file system). The target may not exist (a dangling link).
///
/// # Errors
/// When `link` is not a link or can't be read.
pub fn link_target(link: &Path) -> io::Result<PathBuf> {
    let raw = std::fs::read_link(link)?;
    Ok(resolve_target(link, &raw))
}

/// Turns the raw target read from `link` into what the user should see and where "Go to link
/// target" goes. Pure: no file system access.
pub fn resolve_target(link: &Path, raw: &Path) -> PathBuf {
    let plain = strip_verbatim(raw);
    let absolute = if plain.has_root() || plain.components().next().is_some_and(is_prefix) {
        plain
    } else {
        link.parent().unwrap_or(link).join(plain)
    };
    crate::path::normalize(&absolute)
}

const fn is_prefix(c: Component<'_>) -> bool {
    matches!(c, Component::Prefix(_))
}

/// `\\?\C:\x` becomes `C:\x`, `\\?\UNC\server\share` becomes `\\server\share` (and the NT form
/// `\??\` that junctions store the same way). Other paths are returned unchanged.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    for lead in [r"\\?\", r"\??\"] {
        if let Some(rest) = text.strip_prefix(lead) {
            if let Some(unc) = rest.strip_prefix(r"UNC\") {
                return PathBuf::from(format!(r"\\{unc}"));
            }
            return PathBuf::from(rest);
        }
    }
    // A verbatim prefix std already parsed (e.g. built from components).
    if let Some(Component::Prefix(p)) = path.components().next()
        && let Prefix::VerbatimDisk(letter) = p.kind()
    {
        let rest: PathBuf = path.components().skip(1).collect();
        return Path::new(&format!("{}:\\", char::from(letter))).join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_lose_the_verbatim_prefix() {
        let link = Path::new(r"C:\work\link");
        assert_eq!(
            resolve_target(link, Path::new(r"\\?\D:\data\proj")),
            PathBuf::from(r"D:\data\proj")
        );
        assert_eq!(
            resolve_target(link, Path::new(r"\??\D:\data")),
            PathBuf::from(r"D:\data")
        );
        assert_eq!(
            resolve_target(link, Path::new(r"\\?\UNC\nas\share\x")),
            PathBuf::from(r"\\nas\share\x")
        );
        assert_eq!(
            resolve_target(link, Path::new(r"E:\plain")),
            PathBuf::from(r"E:\plain")
        );
    }

    /// Relative symlinks point from the link's own folder.
    #[test]
    fn relative_targets_resolve_from_the_link_folder() {
        let link = Path::new(r"C:\work\sub\link");
        assert_eq!(
            resolve_target(link, Path::new(r"..\other\file.txt")),
            PathBuf::from(r"C:\work\other\file.txt")
        );
        assert_eq!(
            resolve_target(link, Path::new("sibling")),
            PathBuf::from(r"C:\work\sub\sibling")
        );
    }

    /// Temp folder for link tests, removed afterwards.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("arke-link-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A real junction (`mklink /J` needs no admin rights) reads back as its target, listed as a
    /// folder link.
    #[cfg(windows)]
    #[test]
    fn junction_target_is_read_without_following() {
        let sb = Sandbox::new("junction");
        let target = sb.0.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("inside.txt"), "x").unwrap();
        let link = sb.0.join("link");
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "failed to create junction");
        let same = |a: &Path, b: &Path| {
            std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
        };
        assert!(same(&link_target(&link).unwrap(), &target));
        let meta = std::fs::symlink_metadata(&link).unwrap();
        let entry = crate::Entry::from_metadata("link", &meta);
        assert_eq!(entry.kind, crate::EntryKind::Link { is_dir: true });
        // Not a link: an error, not a guess.
        assert!(link_target(&target).is_err());
    }

    /// File symlinks need Developer Mode or admin rights; skipped when Windows refuses.
    #[cfg(windows)]
    #[test]
    fn file_symlink_target_when_allowed() {
        let sb = Sandbox::new("symlink");
        let target = sb.0.join("real.txt");
        std::fs::write(&target, "x").unwrap();
        let link = sb.0.join("link.txt");
        if std::os::windows::fs::symlink_file("real.txt", &link).is_err() {
            // No symlink privilege on this machine: nothing to check.
            return;
        }
        let same = |a: &Path, b: &Path| {
            std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
        };
        assert!(same(&link_target(&link).unwrap(), &target));
        let meta = std::fs::symlink_metadata(&link).unwrap();
        let entry = crate::Entry::from_metadata("link.txt", &meta);
        assert_eq!(entry.kind, crate::EntryKind::Link { is_dir: false });
    }
}
