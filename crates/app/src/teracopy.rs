//! "Use TeraCopy for copy and move": hands copies and moves to TeraCopy when it is installed.
//!
//! TeraCopy is found by looking in the usual Program Files folders (no registry). Its documented
//! command line is `TeraCopy.exe Copy <source> <target>` (or `Move`), where the source is one item
//! or `*"<list file>"`, a text file listing one path per line. Arke uses the plain form for one item
//! and a list file for more. If TeraCopy can't be started, the built-in engine does the job.
//! TeraCopy then owns the job: its own window shows progress, and Arke has nothing to undo.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shell::FileOp;

/// List files older than this are left over from earlier hand-offs and removed.
const LIST_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(24);
const LIST_PREFIX: &str = "arke-teracopy-";

static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Looks for TeraCopy once, in the background, so Settings can offer the option.
pub(crate) fn detect_in_background() {
    crate::explorer::spawn_thread("teracopy-detect", || {
        let found = FOUND.get_or_init(find);
        if let Some(exe) = found {
            log::info!("TeraCopy found at {}", exe.display());
        }
    });
}

/// TeraCopy's exe, if it was found (`None` also while still looking).
pub(crate) fn found() -> Option<&'static Path> {
    FOUND.get().and_then(Option::as_deref)
}

/// Checks the Program Files folders. Background thread (touches the disk).
fn find() -> Option<PathBuf> {
    candidates(|name| std::env::var_os(name))
        .into_iter()
        .find(|exe| exe.is_file())
}

/// Where an installed TeraCopy would be, from the Program Files environment variables.
fn candidates(env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
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

/// Quotes a path for TeraCopy's command line. TeraCopy takes backslashes literally, so a drive
/// root stays `"D:\"`; folders lose their trailing backslash.
fn quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    let is_root = text.len() <= 3 && text.ends_with(":\\");
    let text = if is_root {
        text.into_owned()
    } else {
        text.trim_end_matches('\\').to_owned()
    };
    format!("\"{text}\"")
}

/// The command line after the exe: `Copy "src" "dest"`, or `Copy *"list" "dest"` with a list file.
/// `None` for operations TeraCopy doesn't take over.
fn arguments(op: &FileOp, list: Option<&Path>) -> Option<String> {
    let (verb, sources, dest) = match op {
        FileOp::Copy { sources, dest } => ("Copy", sources, dest),
        FileOp::Move { sources, dest } => ("Move", sources, dest),
        _ => return None,
    };
    let source = match (sources.as_slice(), list) {
        ([], _) => return None,
        ([one], _) => quoted(one),
        (_, Some(list)) => format!("*{}", quoted(list)),
        (_, None) => return None,
    };
    Some(format!("{verb} {source} {}", quoted(dest)))
}

/// One path per line, UTF-8 with a byte order mark so non-ASCII names read correctly.
fn list_bytes(sources: &[PathBuf]) -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    for source in sources {
        bytes.extend_from_slice(source.to_string_lossy().as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}

/// Writes the list file in the temp folder (TeraCopy reads it after starting, so it is left there
/// and cleaned up on a later hand-off).
fn write_list(sources: &[PathBuf]) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    remove_old_lists(&dir);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{LIST_PREFIX}{}-{n}.txt", std::process::id()));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(&list_bytes(sources))?;
    Ok(path)
}

/// Removes Arke's own list files from earlier hand-offs (only `arke-teracopy-*.txt`, a day old).
fn remove_old_lists(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(LIST_PREFIX)
            || !std::path::Path::new(&*name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
        {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LIST_MAX_AGE);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Starts TeraCopy for a copy or move. Returns once it has started (not when it finishes). An
/// error means it could not start, and the caller runs the job itself. Background thread.
pub(crate) fn hand_off(exe: &Path, op: &FileOp) -> io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    let (FileOp::Copy { sources, .. } | FileOp::Move { sources, .. }) = op else {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    };
    let list = if sources.len() > 1 {
        Some(write_list(sources)?)
    } else {
        None
    };
    let args = arguments(op, list.as_deref())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    // Raw: TeraCopy wants `*"list"` exactly, which normal argument quoting would rewrite.
    std::process::Command::new(exe).raw_arg(args).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_in_both_program_files_folders_once() {
        let env = |name: &str| match name {
            "ProgramW6432" | "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
            "ProgramFiles(x86)" => Some(OsString::from(r"C:\Program Files (x86)")),
            _ => None,
        };
        assert_eq!(
            candidates(env),
            [
                PathBuf::from(r"C:\Program Files\TeraCopy\TeraCopy.exe"),
                PathBuf::from(r"C:\Program Files (x86)\TeraCopy\TeraCopy.exe"),
            ]
        );
    }

    #[test]
    fn one_item_goes_on_the_command_line_more_go_in_a_list() {
        let one = FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\a\photo one.jpg")],
            dest: PathBuf::from(r"E:\backup\"),
        };
        assert_eq!(
            arguments(&one, None).as_deref(),
            Some(r#"Copy "C:\a\photo one.jpg" "E:\backup""#)
        );
        let many = FileOp::Move {
            sources: vec![PathBuf::from(r"C:\a\1"), PathBuf::from(r"C:\a\2")],
            dest: PathBuf::from(r"E:\"),
        };
        assert_eq!(
            arguments(&many, Some(Path::new(r"C:\Temp\list.txt"))).as_deref(),
            Some(r#"Move *"C:\Temp\list.txt" "E:\""#)
        );
        assert_eq!(arguments(&many, None), None, "several items need the list");
        let rename = FileOp::Rename {
            path: PathBuf::from(r"C:\a"),
            new_name: "b".into(),
        };
        assert_eq!(arguments(&rename, None), None);
    }

    #[test]
    fn list_files_are_utf8_with_a_bom_one_path_per_line() {
        let bytes = list_bytes(&[PathBuf::from(r"C:\相片\a.jpg"), PathBuf::from(r"D:\b")]);
        assert_eq!(&bytes[..3], [0xEF, 0xBB, 0xBF]);
        assert_eq!(
            String::from_utf8(bytes[3..].to_vec()).unwrap(),
            "C:\\相片\\a.jpg\r\nD:\\b\r\n"
        );
    }
}
