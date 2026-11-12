//! Abstraction over the OS shell. The real implementation is in `win.rs`; tests use a fake.
//!
//! Methods that block or run third-party code (`load_places`, `open`) must be called off the UI thread.
//! Pure methods (`expand_env`, `format_time`) are fine on the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use arke_core::failure::FailReason;
use rust_i18n::t;

/// Errors from [`Shell`] and its Windows implementation (`win*.rs`). Display is the localized,
/// user-facing text; callers put it in notices and banners as is.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ShellError {
    /// A Windows API call failed; shows the system message for its HRESULT.
    #[error("{}", crate::win_fileops::message(.0))]
    Windows(#[from] windows::core::Error),
    /// A file system or process call failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// A path could not be turned into a shell item.
    #[error("{}", t!("error.path", path = .path.display(), error = crate::win_fileops::message(.source)))]
    Path {
        /// The path that failed.
        path: PathBuf,
        /// Why the shell rejected it.
        #[source]
        source: windows::core::Error,
    },
    /// Launching was denied, usually by antivirus or policy blocking other programs.
    #[error("{}", t!("error.open_blocked", error = .0.message()))]
    OpenBlocked(#[source] windows::core::Error),
    /// Not available on this shell (default trait implementations).
    #[error("{}", t!("undo.unsupported"))]
    Unsupported,
    /// A required environment variable (named) is not set.
    #[error("{0}")]
    MissingEnv(&'static str),
    /// Some items of a file operation failed while the rest completed. Each failure names its item
    /// and why, in words the user can act on ("open in Word (PID 1234)").
    #[error("{}", items_failed(.failed))]
    Items {
        /// The items that failed, in order.
        failed: Vec<ItemFailure>,
    },
    /// Some items' cloud pin state could not be changed; one "name: reason" line each.
    #[error("{}", cloud_failures(.failed))]
    CloudPin {
        /// Item names and why they failed, in order.
        failed: Vec<(String, windows::core::Error)>,
    },
    /// A Recycle Bin item to put back or purge is no longer where it was recorded.
    #[error("{}", t!("undo.missing", name = .name))]
    Missing {
        /// The item's name (or recorded location).
        name: String,
    },
    /// Another program holds the clipboard.
    #[error("{}", t!("clip.busy"))]
    ClipboardBusy,
    /// There are no files to put on (or read from) the clipboard.
    #[error("{}", t!("clip.empty"))]
    ClipboardEmpty,
    /// Clipboard memory could not be locked.
    #[error("{}", t!("clip.lock_failed"))]
    ClipboardLock,
    /// Global memory for clipboard or drag data was not available.
    #[error("HGLOBAL")]
    GlobalMemory,
    /// The thread that reads context menus could not start.
    #[error("{}", t!("menu.thread_start_failed"))]
    MenuThreadStart,
    /// The context menu thread has exited.
    #[error("{}", t!("menu.thread_gone"))]
    MenuThreadGone,
    /// The context menu thread did not reply.
    #[error("{}", t!("menu.thread_no_reply"))]
    MenuThreadNoReply,
    /// The menu session was replaced or released before an item was invoked.
    #[error("{}", t!("menu.expired"))]
    MenuExpired,
    /// The menu host process exited.
    #[error("{}", t!("menu.host_gone"))]
    HostGone,
    /// The menu host process could not be started (blocked by antivirus or policy).
    #[error("{}", t!("menu.host_blocked"))]
    HostBlocked,
    /// The menu host took too long and was stopped.
    #[error("{}", t!("menu.host_timeout"))]
    HostTimeout,
    /// Error text reported by the menu host process (`shell_host.rs`).
    #[error("{0}")]
    Host(String),
    /// A request to the menu host could not be encoded.
    #[error("{0}")]
    Encode(#[source] serde_json::Error),
    /// Connecting to or disconnecting a network share failed.
    #[error(transparent)]
    Net(#[from] crate::win_net::NetError),
    /// A remote location (SFTP, WebDAV) failed.
    #[error(transparent)]
    Remote(#[from] arke_remote::RemoteError),
    /// A Windows-only feature asked for on a remote location.
    #[error("{}", t!("error.not_on_remote"))]
    NotOnRemote,
    /// Ejecting a drive failed.
    #[error(transparent)]
    Eject(#[from] crate::win_eject::EjectError),
    /// "Verify after copy" found copies that differ from their source ("path: reason" each).
    #[error("{}", verify_failures(.failed))]
    Verify {
        /// Relative paths and what is wrong with each, in order.
        failed: Vec<String>,
    },
}

/// A program that has an item open, from the Restart Manager (`win_inuse.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Holder {
    /// Friendly name ("Microsoft Word"), or the program's file name when Windows has none.
    pub(crate) app: String,
    pub(crate) pid: u32,
}

/// One item a file operation could not process, and why.
#[derive(Debug)]
pub(crate) struct ItemFailure {
    /// The item's full path (just its name when the copy engine gave no path).
    pub(crate) path: PathBuf,
    pub(crate) reason: FailReason,
    /// Windows' own text for the error, shown when the reason has no message of its own.
    pub(crate) detail: String,
    /// Programs that have it open; looked up after the operation for in-use failures.
    pub(crate) holders: Vec<Holder>,
}

impl ItemFailure {
    pub(crate) const fn new(path: PathBuf, reason: FailReason, detail: String) -> Self {
        Self {
            path,
            reason,
            detail,
            holders: Vec::new(),
        }
    }

    /// From an I/O error (permanent delete).
    pub(crate) fn io(path: PathBuf, error: &std::io::Error) -> Self {
        let text = error.to_string();
        // Without the trailing "(os error N)".
        let detail = text
            .rfind(" (os error")
            .map_or(text.as_str(), |at| text[..at].trim_end())
            .to_owned();
        Self::new(path, arke_core::failure::from_io(error), detail)
    }

    /// File name shown in messages (the whole path when it has none, e.g. a drive).
    pub(crate) fn name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// "“a.txt” is in use by Microsoft Word (PID 1234)".
    pub(crate) fn text(&self) -> String {
        const APPS_SHOWN: usize = 2;
        let name = self.name();
        let name = name.as_str();
        match self.reason {
            FailReason::InUse if !self.holders.is_empty() => {
                let mut apps = self
                    .holders
                    .iter()
                    .take(APPS_SHOWN)
                    .map(|h| t!("fail.app_pid", app = h.app, pid = h.pid).into_owned())
                    .collect::<Vec<_>>()
                    .join(&t!("ops.list_separator"));
                if self.holders.len() > APPS_SHOWN {
                    apps.push_str(&t!(
                        "fail.more_apps",
                        count = self.holders.len() - APPS_SHOWN
                    ));
                }
                t!("fail.in_use_by", name = name, apps = apps)
            }
            FailReason::InUse => t!("fail.in_use", name = name),
            FailReason::AccessDenied => t!("fail.access_denied", name = name),
            FailReason::ReadOnly => t!("fail.read_only", name = name),
            FailReason::PathTooLong => t!("fail.too_long", name = name),
            FailReason::NoRecycleBin => t!("fail.no_recycle_bin", name = name),
            FailReason::BadName => t!("fail.bad_name", name = name),
            FailReason::BadNameInside => t!("fail.bad_name_inside", name = name),
            FailReason::DiskFull => t!("fail.disk_full", name = name),
            FailReason::Offline => t!("fail.offline", name = name),
            FailReason::Gone | FailReason::Other => {
                t!("error.path", path = name, error = self.detail)
            }
        }
        .into_owned()
    }
}

/// The failure report: one item's reason, or "3 items failed: …" with the first few, then what
/// to do about it.
pub(crate) fn items_failed(failed: &[ItemFailure]) -> String {
    const SHOWN: usize = 3;
    let mut text = match failed {
        [one] => one.text(),
        many => {
            let mut list = many
                .iter()
                .take(SHOWN)
                .map(ItemFailure::text)
                .collect::<Vec<_>>()
                .join(&t!("fail.list_separator"));
            if many.len() > SHOWN {
                list.push('…');
            }
            tn!("fail.many", many.len(), list = list).into_owned()
        }
    };
    let any = |reasons: &[FailReason]| failed.iter().any(|f| reasons.contains(&f.reason));
    let hint = if any(&[FailReason::InUse]) {
        Some(t!("fail.hint_close"))
    } else if any(&[
        FailReason::NoRecycleBin,
        FailReason::BadName,
        FailReason::BadNameInside,
    ]) {
        Some(t!("fail.hint_permanent"))
    } else {
        None
    };
    if let Some(hint) = hint {
        text.push_str(&t!("fail.sentence_gap"));
        text.push_str(&hint);
    }
    text
}

/// "Copies don't match: a.txt (different content), ..." with at most five names.
fn verify_failures(failed: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut names = failed
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if failed.len() > SHOWN {
        names.push_str(&t!("verify.more", count = failed.len() - SHOWN));
    }
    tn!("verify.failed", failed.len(), names = names).into_owned()
}

/// "name: reason" per failed item, one per line.
fn cloud_failures(failed: &[(String, windows::core::Error)]) -> String {
    failed
        .iter()
        .map(|(name, e)| format!("{name}: {}", e.message()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) label: String,
    pub(crate) path: PathBuf,
    pub(crate) kind: PlaceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaceKind {
    Folder(KnownFolder),
    /// `space` is (free, total) bytes, queried only for fixed local disks.
    Drive {
        remote: bool,
        space: Option<(u64, u64)>,
    },
    /// A WSL distro (`\\wsl.localhost\<name>`). WSL only starts when it is opened.
    Wsl,
    /// Cloud sync folders such as OneDrive (sync roots registered with Windows).
    Cloud,
}

/// How cloud files are kept on this machine (`Shell::set_cloud_pin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloudPin {
    /// Always keep on this device; downloads anything not yet local.
    Keep,
    /// Free up space (online only).
    Free,
    /// Clear "always keep" and let the sync client decide.
    Default,
}

/// Known folder kind; picks the sidebar icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KnownFolder {
    Profile,
    Desktop,
    Downloads,
    Documents,
    Pictures,
    Music,
    Videos,
}

/// The "More options" menu read from the Windows context menu. Read in a separate process and sent back serialized.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellMenu {
    pub(crate) session: u64,
    pub(crate) entries: Vec<ShellMenuEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum ShellMenuEntry {
    Item {
        id: u32,
        /// Canonical verb ("open", "copy", ...). Most extension entries have none.
        #[serde(default)]
        verb: Option<String>,
        label: String,
        enabled: bool,
        checked: bool,
        /// Default command (Windows draws it bold).
        default: bool,
        #[serde(with = "icon_wire")]
        icon: Option<arke_core::thumbs::Thumbnail>,
    },
    Submenu {
        label: String,
        enabled: bool,
        #[serde(with = "icon_wire")]
        icon: Option<arke_core::thumbs::Thumbnail>,
        children: Vec<Self>,
    },
    Separator,
}

/// Collapses consecutive separators and drops leading and trailing ones.
pub(crate) fn tidy_separators(entries: Vec<ShellMenuEntry>) -> Vec<ShellMenuEntry> {
    let mut out: Vec<ShellMenuEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        let sep = matches!(e, ShellMenuEntry::Separator);
        if sep
            && out
                .last()
                .is_none_or(|l| matches!(l, ShellMenuEntry::Separator))
        {
            continue;
        }
        out.push(e);
    }
    if matches!(out.last(), Some(ShellMenuEntry::Separator)) {
        out.pop();
    }
    out
}

/// Cross-process wire format for menu icons: (width, height, RGBA as hex).
mod icon_wire {
    use arke_core::thumbs::Thumbnail;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
    use std::fmt::Write;

    // serde's `with` requires the first parameter to be &FieldType.
    #[allow(clippy::ref_option)]
    pub(super) fn serialize<S: Serializer>(
        icon: &Option<Thumbnail>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        icon.as_ref()
            .map(|t| {
                let mut hex = String::with_capacity(t.rgba.len() * 2);
                for b in &t.rgba {
                    let _ = write!(hex, "{b:02x}");
                }
                (t.width, t.height, hex)
            })
            .serialize(s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Thumbnail>, D::Error> {
        let Some((width, height, hex)) = Option::<(u32, u32, String)>::deserialize(d)? else {
            return Ok(None);
        };
        let rgba = hex
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|p| u8::from_str_radix(p, 16).ok())
            })
            .collect::<Option<Vec<u8>>>()
            .filter(|v| v.len() as u64 == u64::from(width) * u64::from(height) * 4)
            .ok_or_else(|| D::Error::custom("bad icon"))?;
        Ok(Some(Thumbnail {
            width,
            height,
            rgba,
        }))
    }
}

/// File operations. On Windows these all go through `IFileOperation`, the same copy engine,
/// progress and conflict dialogs as Explorer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileOp {
    Copy {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// `recycle` moves to the Recycle Bin; false deletes permanently. Permanent deletes do not prompt
    /// again, so the caller must confirm first (Arke's own dialog).
    Delete {
        paths: Vec<PathBuf>,
        recycle: bool,
    },
    Rename {
        path: PathBuf,
        new_name: String,
    },
    /// Renames several items at once (multi-select rename); done and undone as one operation.
    RenameMany {
        items: Vec<(PathBuf, String)>,
    },
    NewFolder {
        parent: PathBuf,
        name: String,
    },
    /// Empty file ("New text document").
    NewFile {
        parent: PathBuf,
        name: String,
    },
}

impl FileOp {
    /// First item name and item count. Only for copy, move and delete.
    pub(crate) fn subject(&self) -> Option<(String, usize)> {
        let paths = match self {
            Self::Copy { sources, .. } | Self::Move { sources, .. } => sources,
            Self::Delete { paths, .. } => paths,
            _ => return None,
        };
        let first = paths.first()?.file_name()?.to_string_lossy().into_owned();
        Some((first, paths.len()))
    }

    /// Whether to list it in the progress center. Only potentially long operations; quick renames and creates would just add noise.
    pub(crate) const fn listed(&self) -> bool {
        matches!(
            self,
            Self::Copy { .. } | Self::Move { .. } | Self::Delete { .. }
        )
    }

    /// Verb locale key for notifications (`op.*` in `locales/app.yml`).
    pub(crate) const fn verb(&self) -> &'static str {
        match self {
            Self::Copy { .. } => "op.copy",
            Self::Move { .. } => "op.move",
            Self::Delete { recycle: true, .. } => "op.recycle",
            Self::Delete { recycle: false, .. } => "op.delete_permanently",
            Self::Rename { .. } | Self::RenameMany { .. } => "op.rename",
            Self::NewFolder { .. } => "op.new_folder",
            Self::NewFile { .. } => "op.new_file",
        }
    }
}

/// The operation that redoes `op` once its undo `undo` has run: it replays what actually completed
/// (a move or recycle cancelled halfway redoes only the items it reached). `None` for operations
/// that can't be redone (permanent delete).
pub(crate) fn redo_of(op: &FileOp, undo: &Undo) -> Option<FileOp> {
    let done: Vec<PathBuf> = match undo {
        Undo::PutBack(items) => items.iter().map(|i| i.parent.join(&i.name)).collect(),
        Undo::Remove(_) => Vec::new(),
    };
    let reached = |sources: &[PathBuf]| -> Vec<PathBuf> {
        let kept: Vec<PathBuf> = sources
            .iter()
            .filter(|s| done.contains(s))
            .cloned()
            .collect();
        if kept.is_empty() { done.clone() } else { kept }
    };
    match op {
        FileOp::Move { sources, dest } => {
            let sources = reached(sources);
            (!sources.is_empty()).then(|| FileOp::Move {
                sources,
                dest: dest.clone(),
            })
        }
        FileOp::Delete {
            paths,
            recycle: true,
        } => {
            let paths = reached(paths);
            (!paths.is_empty()).then_some(FileOp::Delete {
                paths,
                recycle: true,
            })
        }
        FileOp::Delete { recycle: false, .. } => None,
        FileOp::RenameMany { items } => {
            let items: Vec<_> = items
                .iter()
                .filter(|(path, _)| done.contains(path))
                .cloned()
                .collect();
            (!items.is_empty()).then_some(FileOp::RenameMany { items })
        }
        // A copy cancelled halfway: copy again the sources whose copies were made (same name),
        // or all of them when the copies were renamed ("x - Copy").
        FileOp::Copy { sources, dest } => {
            let made: Vec<&std::ffi::OsStr> = match undo {
                Undo::Remove(paths) => paths.iter().filter_map(|p| p.file_name()).collect(),
                Undo::PutBack(_) => Vec::new(),
            };
            let kept: Vec<PathBuf> = sources
                .iter()
                .filter(|s| s.file_name().is_some_and(|n| made.contains(&n)))
                .cloned()
                .collect();
            Some(FileOp::Copy {
                sources: if kept.is_empty() {
                    sources.clone()
                } else {
                    kept
                },
                dest: dest.clone(),
            })
        }
        FileOp::Rename { .. } | FileOp::NewFolder { .. } | FileOp::NewFile { .. } => {
            Some(op.clone())
        }
    }
}

/// How a drag into another program (Windows drag and drop) ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragOutEnd {
    /// Dropped in another program, or cancelled. Arke's drag ends too.
    Finished,
    /// Dragged back into an Arke window. Windows drag and drop is cancelled and Arke's own drag resumes
    /// (the drop still picks move or copy as usual).
    Returned,
}

/// How to undo a file operation (Ctrl+Z). Built from the per-item results reported by the copy engine,
/// so auto-renamed copies and Recycle Bin locations are what actually happened, not guesses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Undo {
    /// Put back: current location (file path, or the Recycle Bin item's parsing name), original folder and name.
    /// Used for move, rename and recycle.
    PutBack(Vec<PutBack>),
    /// Items the operation created (copies, new folders and files): recycled, so still recoverable.
    Remove(Vec<PathBuf>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PutBack {
    pub(crate) now: String,
    pub(crate) parent: PathBuf,
    pub(crate) name: String,
}

impl Undo {
    /// Number of items the undo touches.
    pub(crate) const fn len(&self) -> usize {
        match self {
            Self::PutBack(items) => items.len(),
            Self::Remove(paths) => paths.len(),
        }
    }

    /// Folder to show after undo (for selection): where items were put back.
    pub(crate) fn target_dir(&self) -> Option<&Path> {
        match self {
            Self::PutBack(items) => items.first().map(|i| i.parent.as_path()),
            Self::Remove(paths) => paths.first().and_then(|p| p.parent()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpOutcome {
    Done,
    /// User cancelled in the progress or confirmation dialog; part of the work may be done.
    Cancelled,
}

/// File list on the clipboard, interoperable with Explorer (CF_HDROP + Preferred DropEffect).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClipboardFiles {
    pub(crate) paths: Vec<PathBuf>,
    /// Cut (move on paste); false means copy.
    pub(crate) cut: bool,
}

/// An item in the Recycle Bin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecycledItem {
    /// Physical location inside the Recycle Bin; restore and purge identify the item by it.
    pub(crate) now: String,
    /// Original name.
    pub(crate) name: String,
    /// Original folder.
    pub(crate) from: PathBuf,
    /// When it was deleted.
    pub(crate) deleted: Option<SystemTime>,
    /// Size; for folders the total of their contents.
    pub(crate) size: u64,
    pub(crate) is_dir: bool,
}

/// File operation progress. Arke draws its own progress instead of the Windows progress window.
/// Written by the background thread, read by the UI thread.
#[derive(Default)]
pub(crate) struct OpProgress {
    /// Cancel requested; stops before the next item starts.
    pub(crate) cancel: std::sync::atomic::AtomicBool,
    /// Items completed.
    pub(crate) done: std::sync::atomic::AtomicU64,
    /// Work reported by Windows (done/total); total is 0 while still counting.
    pub(crate) work_done: std::sync::atomic::AtomicU64,
    pub(crate) work_total: std::sync::atomic::AtomicU64,
    /// Bytes of files finished copying or moving across volumes, for speed. Same-volume moves are renames and do not count.
    pub(crate) bytes: std::sync::atomic::AtomicU64,
    /// Total size to copy, computed in the background at start (0 if unknown or too many to count).
    /// Multiplied by progress it gives bytes done, so speed works mid-way through a single large file.
    pub(crate) bytes_total: std::sync::atomic::AtomicU64,
    /// Waiting in the operation queue for another job on the same drive (or a paused queue).
    pub(crate) queued: std::sync::atomic::AtomicBool,
    /// Copied; now reading the copies back to compare ("Verify after copy").
    pub(crate) verifying: std::sync::atomic::AtomicBool,
}

/// Parameters for connecting to a share, collected by Arke's dialog.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct NetConnect {
    /// `\\server\share`
    pub(crate) share: PathBuf,
    /// Drive letter to map; `None` only logs in without taking a letter.
    pub(crate) drive: Option<char>,
    /// User name and password; `None` uses the current Windows identity.
    pub(crate) credentials: Option<(String, String)>,
    /// Save the credentials in Windows Credential Manager.
    pub(crate) remember: bool,
}

// Passwords must never appear in logs or test failure messages.
impl std::fmt::Debug for NetConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetConnect")
            .field("share", &self.share)
            .field("drive", &self.drive)
            .field("user", &self.credentials.as_ref().map(|(user, _)| user))
            .field("remember", &self.remember)
            .finish()
    }
}

/// Image under the cursor when dragging into other programs: BGRA, premultiplied alpha, top-down (Windows bitmap layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DragImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bgra: Vec<u8>,
}

/// What to build the drag image from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DragImageRequest {
    /// Item under the cursor.
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    /// `.exe`/`.lnk`/`.ico` use the file's own icon (reads the file, local disks only).
    pub(crate) own_icon: bool,
    /// Number of dragged items; above 1 a count badge is added.
    pub(crate) count: usize,
    /// Count badge color (RGB).
    pub(crate) badge: [u8; 3],
}

pub(crate) trait Shell: Send + Sync + 'static {
    /// Known folders and drive letters. Background thread; must not probe network drives.
    fn load_places(&self) -> Vec<Place>;
    /// Requeries (free, total) bytes for these drives (sidebar usage bars). Only asks for volume info, reads no files;
    /// drives that fail are omitted. Background thread.
    fn disk_spaces(&self, roots: &[PathBuf]) -> Vec<(PathBuf, (u64, u64))> {
        let _ = roots;
        Vec::new()
    }
    /// Opens a file with its default program. Background thread (runs shell extensions and other third-party code).
    fn open(&self, path: &Path) -> Result<(), ShellError>;
    /// Expands environment variables such as `%USERPROFILE%` and a leading `~`. Pure.
    fn expand_env(&self, input: &str) -> String;
    /// Local time for display. Pure.
    fn format_time(&self, time: SystemTime) -> String;
    /// Short time for list columns: time only for today and yesterday, month and day this year, full date otherwise. Pure.
    fn format_time_short(&self, time: SystemTime) -> String {
        self.format_time(time)
    }

    /// Runs a file operation and returns when it finishes or is cancelled. Background thread: it shows system
    /// dialogs, loads shell extensions and may hit the network. `owner` is the dialog parent HWND.
    fn file_op(&self, op: &FileOp, owner: Option<isize>) -> Result<OpOutcome, ShellError>;
    /// Like `file_op`, also returning how to undo the completed part (including after a cancel). Background thread.
    fn file_op_undoable(
        &self,
        op: &FileOp,
        owner: Option<isize>,
    ) -> (Result<OpOutcome, ShellError>, Option<Undo>) {
        (self.file_op(op, owner), None)
    }
    /// Like `file_op_undoable` without the Windows progress window: progress goes to `progress` for Arke to draw,
    /// and setting `progress.cancel` stops it. Windows still shows confirmation and conflict dialogs. Background thread.
    fn file_op_tracked(
        &self,
        op: &FileOp,
        owner: Option<isize>,
        progress: &std::sync::Arc<OpProgress>,
    ) -> (Result<OpOutcome, ShellError>, Option<Undo>) {
        let _ = progress;
        self.file_op_undoable(op, owner)
    }
    /// Drags files into another program via Windows drag and drop; returns on drop or cancel with how it ended.
    /// Call on the UI thread while the left button is down (the drag loop needs a thread with a window). No file system access.
    /// `image` is the cursor image from [`Shell::drag_image`]; if it is not ready only the cursor shows.
    fn drag_out(
        &self,
        paths: &[PathBuf],
        image: Option<&DragImage>,
    ) -> Result<DragOutEnd, ShellError> {
        let _ = (paths, image);
        Ok(DragOutEnd::Finished)
    }
    /// Builds the drag image. `base` is an existing thumbnail; falls back to the system icon. Background thread.
    fn drag_image(
        &self,
        base: Option<arke_core::thumbs::Thumbnail>,
        request: &DragImageRequest,
    ) -> Option<DragImage> {
        let _ = (base, request);
        None
    }
    /// Undoes a file operation. Background thread (shows system conflict and confirmation dialogs).
    fn undo(&self, undo: &Undo, owner: Option<isize>) -> Result<OpOutcome, ShellError> {
        let _ = (undo, owner);
        Err(ShellError::Unsupported)
    }
    /// Like `undo` without the Windows progress window: progress goes to `progress` (drawn by the progress center),
    /// and setting `progress.cancel` stops it. Background thread.
    fn undo_tracked(
        &self,
        undo: &Undo,
        owner: Option<isize>,
        progress: &std::sync::Arc<OpProgress>,
    ) -> Result<OpOutcome, ShellError> {
        let _ = progress;
        self.undo(undo, owner)
    }
    /// Recycle Bin contents across all drives. Read only. Background thread.
    fn recycle_bin(&self) -> Result<Vec<RecycledItem>, ShellError> {
        Ok(Vec::new())
    }
    /// Restores Recycle Bin items to their original location, recreating missing folders.
    /// Windows asks if a file with the same name already exists. Background thread.
    fn recycle_restore(
        &self,
        items: &[RecycledItem],
        owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        let back = items
            .iter()
            .map(|item| PutBack {
                now: item.now.clone(),
                parent: item.from.clone(),
                name: item.name.clone(),
            })
            .collect();
        self.undo(&Undo::PutBack(back), owner)
    }
    /// Permanently deletes Recycle Bin items. Does not prompt; the caller must confirm first. Background thread.
    fn recycle_purge(
        &self,
        items: &[RecycledItem],
        owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        let _ = (items, owner);
        Err(ShellError::Unsupported)
    }
    /// Puts files on the system clipboard and returns the resulting clipboard sequence number.
    /// Background thread; retries while another program holds the clipboard.
    fn set_clipboard_files(&self, files: &ClipboardFiles) -> Result<u32, ShellError>;
    /// Reads files from the clipboard with the sequence number at read time; `None` if there are no files. Background thread.
    fn clipboard_files(&self) -> Result<Option<(ClipboardFiles, u32)>, ShellError>;
    /// Clears the clipboard after a cut and paste, only if the sequence is still `seq` so later copies survive. Background thread.
    fn clear_clipboard_if(&self, seq: u32);
    /// Clipboard sequence number; changes on every content change. Does not open the clipboard or block; UI thread safe.
    fn clipboard_seq(&self) -> u32;
    /// Whether the clipboard has files (format check only, clipboard not opened). UI thread safe.
    fn clipboard_has_files(&self) -> bool;

    /// Whether the path is on a fixed local disk (thumbnails and system icons are local only). Background thread.
    fn is_local_disk(&self, path: &Path) -> bool;
    /// Whether deleting `path` can go to the Recycle Bin. Windows has none on network shares, USB
    /// sticks and optical discs, and deletes there permanently. Reads local drive info only, so
    /// it is fine on the UI thread.
    fn has_recycle_bin(&self, path: &Path) -> bool {
        let _ = path;
        true
    }
    /// System icon (only requested for `.exe`, `.lnk`, `.ico`). Background thread (COM, may read the file).
    fn file_icon(&self, path: &Path, size: u32) -> Option<arke_core::thumbs::Thumbnail>;

    /// "More options": reads the Windows context menu entries (including third-party shell extensions) for Arke to draw.
    /// With `background`, `paths` is one folder and the menu is its empty-space menu (New, Open in Terminal, ...).
    /// Shell extensions are loaded in a separate process (`shell_host.rs`), never in Arke itself. Background thread.
    /// The menu object stays alive there; later calls invoke an entry or release it by `session`.
    fn shell_menu(&self, paths: &[PathBuf], background: bool) -> Result<ShellMenu, ShellError>;
    /// Whether "More options" can be prefetched when the context menu opens. Only possible out of process;
    /// otherwise it is read when the user opens it.
    fn shell_menu_prefetch(&self) -> bool {
        true
    }
    /// Invokes entry `id` from [`Shell::shell_menu`]; the menu is released afterwards. `client_point` is where the
    /// right click happened (physical pixels in Arke's client area); some commands open windows there. Background thread.
    fn invoke_shell_menu(
        &self,
        session: u64,
        id: u32,
        client_point: (i32, i32),
        owner: Option<isize>,
    ) -> Result<(), ShellError>;
    /// Releases the menu object when the menu closes without a choice. Fire and forget; UI thread safe.
    fn release_shell_menu(&self, session: u64);
    /// Puts text on the clipboard ("Copy path"). Background thread.
    fn set_clipboard_text(&self, text: &str) -> Result<(), ShellError>;
    /// Opens in Windows Explorer, selecting the item when `select` is set. Background thread.
    fn reveal_in_explorer(&self, path: &Path, select: bool) -> Result<(), ShellError>;
    /// Shortcut (`.lnk`) target and whether it is a folder. Uses the data stored in the link (`IShellLink`) without
    /// touching the target, so disconnected network targets do not hang. `None` for non-file-system targets
    /// (Control Panel, URLs, ...). Background thread.
    fn shortcut_target(&self, path: &Path) -> Option<(PathBuf, bool)> {
        let _ = path;
        None
    }
    /// Cloud files "Always keep on this device" / "Free up space", recursive for folders. The sync client does the
    /// actual download or eviction. Background thread.
    fn set_cloud_pin(&self, paths: &[PathBuf], pin: CloudPin) -> Result<(), ShellError> {
        let _ = (paths, pin);
        Err(ShellError::Unsupported)
    }
    /// Creates a shortcut in `dest` for each of `sources` (localized "<name> - Shortcut.lnk", numbered on collision)
    /// and returns the created paths. Background thread.
    fn create_shortcuts(
        &self,
        sources: &[PathBuf],
        dest: &Path,
    ) -> Result<Vec<PathBuf>, ShellError> {
        let _ = (sources, dest);
        Err(ShellError::Unsupported)
    }
    /// Recent files from the Windows recent list, newest first, at most `limit`; folders excluded.
    /// Reads the stored link targets without touching them, so network files do not hang. Background thread.
    fn recent_files(&self, limit: usize) -> Vec<crate::home::RecentFile> {
        let _ = limit;
        Vec::new()
    }
    /// Which physical drive `path` is on, for the operation queue: jobs on the same key run one
    /// after another. The default is the volume (`E:`, or `\\server\share` for network shares). Background thread.
    fn drive_key(&self, path: &Path) -> String {
        arke_core::watches::volume_key(path).unwrap_or_default()
    }
    /// Whether the drive can be ejected (USB, memory card, optical, external disk). Fast enough to call while opening a menu.
    fn is_ejectable(&self, root: &Path) -> bool {
        let _ = root;
        false
    }
    /// Ejects a drive (Safely Remove Hardware). Returns the reason if it is in use. Background thread.
    fn eject_drive(&self, root: &Path) -> Result<(), ShellError> {
        let _ = root;
        Err(ShellError::Unsupported)
    }
    /// Windows "Open with" dialog. "Always use this app" is remembered by Windows itself.
    /// Blocks until the dialog closes; background thread.
    fn choose_app(&self, path: &Path, owner: Option<isize>) -> Result<(), ShellError> {
        let _ = (path, owner);
        Err(ShellError::Unsupported)
    }
    fn open_terminal(&self, dir: &Path) -> Result<(), ShellError> {
        let _ = dir;
        Err(ShellError::Unsupported)
    }
    /// Runs a command line in `dir` (typing `cmd`, `powershell`, `bash`, `wt`, `code .` in the path bar, like Explorer's
    /// address bar). The first word is the program (resolved via PATH and App Paths), the rest are arguments. Background thread.
    fn run_in(&self, command_line: &str, dir: &Path) -> Result<(), ShellError> {
        let _ = (command_line, dir);
        Err(ShellError::Unsupported)
    }
    /// Local (year, month, day, hour, minute, second), for archive modification times. Pure.
    fn local_parts(&self, time: SystemTime) -> Option<[u16; 6]> {
        let _ = time;
        None
    }
    /// Decodes non-UTF-8 archive entry names with the system ANSI code page (Big5 on Traditional Chinese Windows). Pure.
    fn decode_ansi(&self, bytes: &[u8]) -> String;

    /// Network location checks (probing servers, mapped drives). `None` disables probing and network folder tracking (tests).
    fn net_probe(&self) -> Option<std::sync::Arc<dyn arke_core::net::NetProbe>> {
        None
    }
    /// Connects to a share (log in, or map a drive letter). On failure returns a user-facing reason. Background thread.
    /// The password is only used for this call and never persisted to the registry ("Reconnect at sign-in" is not set).
    fn net_connect(&self, request: &NetConnect) -> Result<(), ShellError> {
        let _ = request;
        Err(ShellError::Unsupported)
    }
    /// Disconnects a network drive (`Z:\`). Background thread.
    fn net_disconnect(&self, drive: &Path) -> Result<(), ShellError> {
        let _ = drive;
        Err(ShellError::Unsupported)
    }
    /// Installed programs suitable for "Open with" (name, exe). Background thread (checks that files exist).
    fn editor_apps(&self) -> Vec<(String, PathBuf)> {
        Vec::new()
    }
    /// Start at Windows sign-in by adding or removing a shortcut in the user's Startup folder, no registry.
    /// Background thread.
    fn set_autostart(&self, on: bool) -> Result<(), ShellError> {
        let _ = on;
        Ok(())
    }
    /// Adds or removes Arke's Start menu shortcut (a file in the user's Start menu folder, no
    /// registry). Background thread.
    fn set_start_menu(&self, on: bool) -> Result<(), ShellError> {
        let _ = on;
        Ok(())
    }
    /// Windows "Map network drive" dialog. Returns `true` if a drive was mapped, `false` on cancel. Background thread.
    fn map_drive_dialog(&self, owner: Option<isize>) -> Result<bool, ShellError> {
        let _ = owner;
        Ok(false)
    }
    /// Windows "Disconnect network drive" dialog. Returns `true` if a drive was disconnected. Background thread.
    fn disconnect_drive_dialog(&self, owner: Option<isize>) -> Result<bool, ShellError> {
        let _ = owner;
        Ok(false)
    }
    /// Windows "Open" dialog for one existing file, owned by `owner`; `None` on cancel. Shown in
    /// a separate process so its shell extensions never load into Arke (`win_picker.rs`).
    /// Blocks until the dialog closes; background thread.
    fn pick_file(
        &self,
        request: &PickRequest,
        owner: Option<isize>,
    ) -> Result<Option<PathBuf>, ShellError> {
        let _ = (request, owner);
        Err(ShellError::Unsupported)
    }
    /// Renders page `page` (0-based) of a PDF to fit within `max` (device pixels), with the
    /// built-in Windows PDF renderer. Never call it on a cloud placeholder: reading would download
    /// the file. Background thread.
    fn render_pdf(&self, path: &Path, page: u32, max: (u32, u32)) -> Result<PdfRender, ShellError> {
        let _ = (path, page, max);
        Err(ShellError::Unsupported)
    }
    /// Sync state of one folder's items from the Cloud Files API. `items` are the names worth a
    /// per-item read (local placeholders, see `Entry::wants_sync_check`). Reads attributes and
    /// placeholder info only: nothing is downloaded and the sync client isn't woken. Background
    /// thread.
    fn cloud_scan(&self, dir: &Path, items: &[Arc<str>]) -> CloudScan {
        let _ = (dir, items);
        CloudScan::default()
    }
}

/// A rendered PDF page.
#[derive(Clone, Debug)]
pub(crate) struct PdfRender {
    /// Pages in the document.
    pub(crate) pages: u32,
    pub(crate) image: arke_core::thumbs::Thumbnail,
}

/// What [`Shell::cloud_scan`] found for a folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CloudScan {
    /// The folder is inside a Cloud Files sync root (any provider, listed in the sidebar or not).
    pub(crate) sync_root: bool,
    /// Items the sync client reports as not in sync.
    pub(crate) syncing: Vec<Arc<str>>,
    /// Plain files the sync client hasn't taken over (filled in by the explorer from the
    /// attributes it already has).
    pub(crate) not_synced: Vec<Arc<str>>,
}

/// What a file dialog asks for (Settings' Browse buttons).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PickRequest {
    /// Dialog title.
    pub(crate) title: String,
    /// Name of the file type filter ("Images").
    pub(crate) filter_name: String,
    /// Its patterns, `;`-separated (`*.png;*.jpg`).
    pub(crate) patterns: String,
    /// Folder to open in (that of the current choice), or the dialog's own default.
    pub(crate) folder: Option<PathBuf>,
}

/// Shortcut file name such as `notes.txt - Shortcut.lnk` (localized; drive roots use the letter).
/// Adds a number when `taken` reports the name exists.
pub(crate) fn shortcut_path(source: &Path, dest: &Path, taken: impl Fn(&Path) -> bool) -> PathBuf {
    shortcut_path_named(
        source,
        dest,
        |name| rust_i18n::t!("dnd.shortcut_name", name = name).into_owned(),
        taken,
    )
}

/// [`shortcut_path`] with the naming pattern from `label`, so tests do not depend on the locale.
pub(crate) fn shortcut_path_named(
    source: &Path,
    dest: &Path,
    label: impl Fn(&str) -> String,
    taken: impl Fn(&Path) -> bool,
) -> PathBuf {
    let name = source.file_name().map_or_else(
        || {
            source
                .to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .replace([':', '\\', '/'], "")
        },
        |n| n.to_string_lossy().into_owned(),
    );
    let base = label(&name);
    let mut path = dest.join(format!("{base}.lnk"));
    let mut n = 2;
    while taken(&path) {
        path = dest.join(format!("{base} ({n}).lnk"));
        n += 1;
    }
    path
}
