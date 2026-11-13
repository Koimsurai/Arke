//! File operations: copy, cut, paste, delete, rename, new folder.
//!
//! The UI thread only decides what to do and handles the result. The work itself
//! (IFileOperation, system clipboard) always runs on a background thread through `Shell`.
//! The list updates through the directory watcher, and selection is tracked by name, so new
//! items get selected as they appear.

use rust_i18n::t;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::Selection;
use arke_core::path::NameError;
use gpui::{
    App, AppContext, Context, Entity, Focusable, Global, KeyBinding, ScrollStrategy, Subscription,
    Window, actions,
};

use super::{AppEvent, Explorer, spawn_thread};
use crate::shell::{ClipboardFiles, FileOp, OpOutcome, OpProgress, ShellError, Undo};
use crate::text_input::{InputEvent, TextInput};

actions!(
    explorer,
    [
        CopyFiles,
        CutFiles,
        PasteFiles,
        DeleteFiles,
        DeletePermanently,
        RenameFile,
        NewFolder,
        UndoFileOp,
        RedoFileOp,
    ]
);

pub(crate) fn bind_keys(cx: &mut App) {
    // Bound at the Explorer level so text inputs (filter, path bar) handle their own Ctrl+C/V and
    // Delete first; deeper contexts take precedence.
    let root = Some("Explorer");
    cx.bind_keys([
        KeyBinding::new("ctrl-c", CopyFiles, root),
        KeyBinding::new("ctrl-x", CutFiles, root),
        KeyBinding::new("ctrl-v", PasteFiles, root),
        KeyBinding::new("delete", DeleteFiles, root),
        KeyBinding::new("shift-delete", DeletePermanently, root),
        KeyBinding::new("f2", RenameFile, root),
        KeyBinding::new("ctrl-shift-n", NewFolder, root),
        KeyBinding::new("ctrl-z", UndoFileOp, root),
        KeyBinding::new("ctrl-y", RedoFileOp, root),
        KeyBinding::new("ctrl-shift-z", RedoFileOp, root),
        // Ctrl+Z inside a text input must not undo a file operation (nor Ctrl+Y redo one).
        KeyBinding::new("ctrl-z", gpui::NoAction, Some("TextInput")),
        KeyBinding::new("ctrl-y", gpui::NoAction, Some("TextInput")),
        KeyBinding::new("ctrl-shift-z", gpui::NoAction, Some("TextInput")),
    ]);
}

/// Undo history for file operations, global across windows and tabs like Explorer. Holds up to
/// `UNDO_MAX`.
#[derive(Default)]
pub(crate) struct UndoHistory {
    entries: Vec<UndoEntry>,
    /// Id for the next entry; ids are never reused, so a stale "Undo" button can't undo
    /// something else.
    next_id: u64,
}

pub(crate) struct UndoEntry {
    /// Lets an "Undo" button (notice, progress center) undo exactly this operation, even after
    /// newer ones were recorded.
    pub(crate) id: u64,
    /// Original operation (locale key, `op.*`).
    pub(crate) verb: &'static str,
    pub(crate) undo: Undo,
    /// What redoes it once undone (see [`crate::shell::redo_of`]); `None` if it can't be redone.
    redo: Option<FileOp>,
}

const UNDO_MAX: usize = 30;

impl Global for UndoHistory {}

impl UndoHistory {
    /// Records an operation for Ctrl+Z and returns its id. A new operation (`fresh`, anything but
    /// a redo) clears what Ctrl+Y could redo, like every editor.
    pub(super) fn push(
        cx: &mut App,
        verb: &'static str,
        undo: Undo,
        redo: Option<FileOp>,
        fresh: bool,
    ) -> u64 {
        if fresh {
            RedoStack::clear(cx);
        }
        let history = cx.default_global::<Self>();
        history.next_id += 1;
        let id = history.next_id;
        history.entries.push(UndoEntry {
            id,
            verb,
            undo,
            redo,
        });
        if history.entries.len() > UNDO_MAX {
            history.entries.remove(0);
        }
        id
    }

    /// Whether this operation can still be undone (not undone yet, not dropped as too old).
    pub(crate) fn has(cx: &App, id: u64) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|h| h.entries.iter().any(|e| e.id == id))
    }

    /// Removes and returns this operation's entry, wherever it is in the history.
    fn take(cx: &mut App, id: u64) -> Option<UndoEntry> {
        let history = cx.try_global::<Self>()?;
        let ix = history.entries.iter().position(|e| e.id == id)?;
        Some(cx.global_mut::<Self>().entries.remove(ix))
    }

    /// The operation the next Ctrl+Z would undo, for the menu.
    pub(crate) fn last(cx: &App) -> Option<&UndoEntry> {
        cx.try_global::<Self>()?.entries.last()
    }
}

/// Undone operations that Ctrl+Y (or Ctrl+Shift+Z) can redo, newest last. Global like
/// [`UndoHistory`]; cleared when a new operation is recorded.
#[derive(Default)]
pub(crate) struct RedoStack {
    entries: Vec<RedoEntry>,
    /// Ids are never reused, so a stale "Redo" button can't redo something else.
    next_id: u64,
}

pub(crate) struct RedoEntry {
    pub(crate) id: u64,
    /// Original operation (locale key, `op.*`).
    pub(crate) verb: &'static str,
    op: FileOp,
}

impl Global for RedoStack {}

impl RedoStack {
    fn push(cx: &mut App, verb: &'static str, op: FileOp) -> u64 {
        let stack = cx.default_global::<Self>();
        stack.next_id += 1;
        let id = stack.next_id;
        stack.entries.push(RedoEntry { id, verb, op });
        if stack.entries.len() > UNDO_MAX {
            stack.entries.remove(0);
        }
        id
    }

    fn clear(cx: &mut App) {
        if cx
            .try_global::<Self>()
            .is_some_and(|s| !s.entries.is_empty())
        {
            cx.global_mut::<Self>().entries.clear();
        }
    }

    /// Whether this undone operation can still be redone.
    pub(crate) fn has(cx: &App, id: u64) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|s| s.entries.iter().any(|e| e.id == id))
    }

    fn take(cx: &mut App, id: u64) -> Option<RedoEntry> {
        let stack = cx.try_global::<Self>()?;
        let ix = stack.entries.iter().position(|e| e.id == id)?;
        Some(cx.global_mut::<Self>().entries.remove(ix))
    }

    /// The operation the next Ctrl+Y would redo, for the menu.
    pub(crate) fn last(cx: &App) -> Option<&RedoEntry> {
        cx.try_global::<Self>()?.entries.last()
    }
}

/// Cut items, shared by all tabs. Invalid once the clipboard sequence number changes (something
/// else was copied, or it was pasted).
pub(crate) struct CutMarks {
    seq: u32,
    parent: PathBuf,
    names: HashSet<Arc<str>>,
}

impl Global for CutMarks {}

/// Item being renamed in place in the list.
pub(crate) struct Renaming {
    pub(crate) name: Arc<str>,
    pub(crate) input: Entity<TextInput>,
    /// Multi-item rename: all selected items in display order, including the one being edited.
    /// Empty for a single item.
    pub(crate) batch: Vec<Arc<str>>,
    /// Why the typed name can't be used, shown right under the box like Explorer's balloon.
    /// Set while typing for problems more typing can't fix, and by Enter for everything else.
    pub(crate) problem: Option<NameError>,
    /// A file just made by "New file…": the whole name is typed over, so a new extension is what
    /// the user asked for and isn't confirmed.
    pub(crate) fresh: bool,
    _subscriptions: [Subscription; 2],
}

/// Problem to show while a name is being typed. Only what further typing can't fix: a name
/// that ends in a period or space is normal halfway through "report.txt", so those wait for
/// Enter (see [`commit_problem`]).
pub(crate) fn typing_problem(text: &str) -> Option<NameError> {
    match arke_core::path::validate_file_name(text.trim()) {
        Err(problem @ (NameError::BadChars | NameError::TooLong)) => Some(problem),
        _ => None,
    }
}

/// Problem that blocks Enter. Leading and trailing spaces are dropped on commit, and an empty
/// name just cancels, so neither counts.
pub(crate) fn commit_problem(text: &str) -> Option<NameError> {
    let name = text.trim();
    if name.is_empty() {
        return None;
    }
    arke_core::path::validate_file_name(name).err()
}

/// What to do after an operation finishes.
#[derive(Default)]
pub(crate) struct OpFollowUp {
    /// Only apply the actions below if the current folder is still this one.
    dest: Option<PathBuf>,
    /// Names to select (pasted items, the new name after rename).
    select: Vec<Arc<str>>,
    /// Start renaming once this item appears (new folder); `true`: a file from "New file…", whose
    /// whole name is selected.
    rename: Option<(Arc<str>, bool)>,
    /// After delete: deleted names and the old cursor index. Once they leave the list, select the
    /// item at that index.
    deleted: Option<(Vec<Arc<str>>, usize)>,
    /// How to undo this operation (whatever completed, including before a cancel).
    undo: Option<Undo>,
    /// This was an undo: the original operation and item count, for the "undone" toast.
    undid: Option<(&'static str, usize)>,
    /// Job id in the progress center, if listed there.
    job: Option<u64>,
    /// Say what happened once it's done, with an Undo button (quiet operations otherwise only
    /// show in the progress center).
    announce: Option<Announce>,
    /// Subject known only later (paste has to read the clipboard first).
    subject: Option<(String, usize)>,
    /// The operation that ran, so its undo knows how to redo it (boxed: events stay small).
    op: Option<Box<FileOp>>,
    /// This is an undo: once it is done, this (original verb, operation) can be redone.
    redo: Option<Box<(&'static str, FileOp)>>,
    /// This is a redo of that operation (verb): says so, and keeps the rest of the redo stack.
    redone: Option<&'static str>,
    /// Handed to TeraCopy instead of running here.
    handed_off: bool,
    /// Folder the operation changed elsewhere (the other pane's): relisted if it is on a server,
    /// which sends no change notifications.
    touched: Option<PathBuf>,
    /// Copied or moved into this folder under these names: once done, they are selected there,
    /// in this pane or in the other one (whichever shows the folder), like a paste.
    arrive: Option<(PathBuf, Vec<Arc<str>>)>,
}

impl OpFollowUp {
    /// Follow-up for the permanent half of a delete split by [`Explorer::ask_no_recycle_bin`]:
    /// same folder and reselect, no "moved to Recycle Bin" notice.
    pub(crate) fn purge_part(follow: &Self) -> Self {
        Self {
            dest: follow.dest.clone(),
            deleted: follow.deleted.clone(),
            ..Self::default()
        }
    }

    /// This follow-up, announcing `count` recycled items.
    pub(crate) fn recycled(self, count: usize) -> Self {
        Self {
            announce: Some(Announce::Recycled(count)),
            ..self
        }
    }
}

/// What "New folder" and "New file" make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NewItem {
    Folder,
    /// "New text document": `New Text Document.txt`, renamed with the stem selected.
    TextFile,
    /// "New file…" on a folder: the same file, renamed with the whole name selected so any name
    /// and extension can be typed.
    File,
}

/// "New folder…" / "New file…" on a folder item, waiting for that folder to be listed.
pub(crate) struct PendingCreate {
    kind: NewItem,
    /// The folder to create in.
    dir: PathBuf,
    /// Its row in the Details tree, when it was expanded in place; `None` when it was opened.
    row: Option<Arc<str>>,
}

/// Short "done" notice with Undo for operations whose result is easy to miss: recycled items
/// leave the list, and moved items disappear into the new folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Announce {
    /// Moved to the Recycle Bin, with the item count.
    Recycled(usize),
    /// Moved into a new folder ("Move to new folder"), with the item count.
    MovedToNewFolder(usize),
}

impl Announce {
    pub(crate) fn text(self) -> String {
        match self {
            Self::Recycled(count) => tn!("notice.recycled", count).into_owned(),
            Self::MovedToNewFolder(count) => tn!("notice.moved_to_new_folder", count).into_owned(),
        }
    }
}

/// What a job's background thread needs, read on the UI thread when the job starts.
#[derive(Clone)]
pub(crate) struct JobEnv {
    shell: Arc<dyn crate::shell::Shell>,
    owner: Option<isize>,
    /// Jobs on the same drive run one after another (setting).
    serialize: bool,
    /// Read copies back and compare them with their sources (setting).
    verify: bool,
    /// TeraCopy's exe, when copies and moves go to it (setting, and TeraCopy found).
    teracopy: Option<PathBuf>,
}

/// Whether `op` would add, rename, move or delete a share itself (`\\server\share`) or put
/// something straight into a server (`\\server`). A server's shares are only listed: deleting a
/// share root would delete everything in it. Copying a share somewhere else is fine.
pub(crate) fn changes_share_list(op: &FileOp) -> bool {
    use arke_core::net::{bare_server, is_share_root};
    let into_server = |dest: &Path| bare_server(dest).is_some();
    let any_share = |paths: &[PathBuf]| paths.iter().any(|p| is_share_root(p));
    match op {
        FileOp::Copy { dest, .. } => into_server(dest),
        FileOp::Move { sources, dest, .. } => into_server(dest) || any_share(sources),
        FileOp::Delete { paths, .. } => any_share(paths),
        FileOp::Rename { path, .. } => is_share_root(path),
        FileOp::RenameMany { items } => items.iter().any(|(p, _)| is_share_root(p)),
        FileOp::NewFolder { parent, .. } | FileOp::NewFile { parent, .. } => into_server(parent),
    }
}

/// How a job ended: result, how to undo what completed, and whether TeraCopy took it over.
type JobOutcome = (Result<OpOutcome, ShellError>, Option<Undo>, bool);

impl JobEnv {
    /// Waits for this job's turn in the queue; `Err` if cancelled while waiting. Background
    /// thread.
    fn turn(
        &self,
        paths: &[PathBuf],
        progress: &OpProgress,
    ) -> Result<crate::op_queue::Turn<'static>, OpOutcome> {
        let keys = crate::op_queue::drive_keys(&*self.shell, paths.iter().map(PathBuf::as_path));
        crate::op_queue::enter(keys, self.serialize, progress).ok_or(OpOutcome::Cancelled)
    }

    /// Runs an operation: hands copies and moves to TeraCopy when set, otherwise waits for its turn
    /// on the drive (listed jobs only), runs it, and verifies copies when set. Background thread.
    fn run(&self, op: &FileOp, progress: &Arc<OpProgress>) -> JobOutcome {
        // A panic in the copy engine wrapper must still end the job (op center, toast, counters)
        // instead of leaving it running forever.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_job(op, progress)))
            .unwrap_or_else(|_| (Err(job_crashed()), None, false))
    }

    fn run_job(&self, op: &FileOp, progress: &Arc<OpProgress>) -> JobOutcome {
        if changes_share_list(op) {
            let refused = std::io::Error::other(t!("error.share_list").into_owned());
            return (Err(refused.into()), None, false);
        }
        // Servers (SFTP, WebDAV) are Arke's own: TeraCopy and verify-after-copy only see local
        // and network-share paths.
        let remote = op_paths(op).iter().any(|p| arke_core::remote::is_remote(p));
        if let Some(exe) = &self.teracopy
            && !remote
            && matches!(op, FileOp::Copy { .. } | FileOp::Move { .. })
        {
            match crate::teracopy::hand_off(exe, op) {
                Ok(()) => return (Ok(OpOutcome::Done), None, true),
                Err(err) => log::warn!("TeraCopy did not start, copying in Arke: {err}"),
            }
        }
        let _turn = if op.listed() {
            match self.turn(&op_paths(op), progress) {
                Ok(turn) => Some(turn),
                Err(outcome) => return (Ok(outcome), None, false),
            }
        } else {
            None
        };
        let (mut result, undo) = self.shell.file_op_tracked(op, self.owner, progress);
        if self.verify
            && !remote
            && let FileOp::Copy { sources, dest } = op
            && matches!(result, Ok(OpOutcome::Done))
            && let Some(Undo::Remove(created)) = &undo
            && let Err(failed) = verify_copies(sources, dest, created, progress)
        {
            result = Err(failed);
        }
        (result, undo, false)
    }

    /// Runs an undo after waiting for its turn on the drives it touches. Background thread.
    fn undo(&self, undo: &Undo, progress: &Arc<OpProgress>) -> Result<OpOutcome, ShellError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _turn = match self.turn(&crate::op_queue::undo_paths(undo), progress) {
                Ok(turn) => turn,
                Err(outcome) => return Ok(outcome),
            };
            self.shell.undo_tracked(undo, self.owner, progress)
        }))
        .unwrap_or_else(|_| Err(job_crashed()))
    }
}

/// The error a job reports when its thread panicked (logged by the panic hook).
fn job_crashed() -> ShellError {
    std::io::Error::other(t!("ops.crashed").into_owned()).into()
}

/// What to try again after `op` failed on some items: the same operation on the selected items
/// that failed (a folder when something inside it failed). `None` when retrying can't help: the
/// name or the drive is the problem, not a program or a moment.
pub(crate) fn retry_of(op: &FileOp, error: &ShellError) -> Option<FileOp> {
    use arke_core::failure::FailReason;
    let ShellError::Items { failed } = error else {
        return None;
    };
    let FileOp::Delete { paths, recycle } = op else {
        return None;
    };
    let again: Vec<PathBuf> = failed
        .iter()
        .filter(|f| {
            !matches!(
                f.reason,
                FailReason::BadName
                    | FailReason::BadNameInside
                    | FailReason::NoRecycleBin
                    | FailReason::PathTooLong
                    | FailReason::Gone
            )
        })
        .map(|f| f.path.clone())
        .collect();
    let paths = arke_core::failure::retry_targets(paths, &again);
    (!paths.is_empty()).then_some(FileOp::Delete {
        paths,
        recycle: *recycle,
    })
}

/// Top-level paths an operation removed from where they were: deleted or moved away, minus those
/// that failed (or hold something that failed). Search results drop these.
pub(crate) fn removed_paths(op: &FileOp, result: &Result<OpOutcome, ShellError>) -> Vec<PathBuf> {
    let (FileOp::Delete { paths, .. } | FileOp::Move { sources: paths, .. }) = op else {
        return Vec::new();
    };
    match result {
        Ok(OpOutcome::Done) => paths.clone(),
        Err(ShellError::Items { failed }) => paths
            .iter()
            .filter(|p| {
                !failed
                    .iter()
                    .any(|f| arke_core::path::is_same_or_inside(&f.path, p))
            })
            .cloned()
            .collect(),
        // Cancelled or failed as a whole: what was done shows when the search runs again.
        _ => Vec::new(),
    }
}

/// Paths an operation reads or writes, for the queue.
fn op_paths(op: &FileOp) -> Vec<PathBuf> {
    match op {
        FileOp::Copy { sources, dest } | FileOp::Move { sources, dest } => sources
            .iter()
            .chain(std::iter::once(dest))
            .cloned()
            .collect(),
        FileOp::Delete { paths, .. } => paths.clone(),
        FileOp::Rename { path, .. } => vec![path.clone()],
        FileOp::RenameMany { items } => items.iter().map(|(p, _)| p.clone()).collect(),
        FileOp::NewFolder { parent, .. } | FileOp::NewFile { parent, .. } => vec![parent.clone()],
    }
}

/// Which copy belongs to which source. The copy engine reports the top-level copies in the order
/// it was given the sources, so when the counts match they pair up in order (this also covers
/// "x - Copy" renames); otherwise by name. `None`: no copy found.
pub(crate) fn pair_copies(
    sources: &[PathBuf],
    dest: &Path,
    created: &[PathBuf],
) -> Vec<(PathBuf, Option<PathBuf>)> {
    let folder = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let dest = folder(dest);
    let top: Vec<&PathBuf> = created
        .iter()
        .filter(|c| c.parent().is_some_and(|p| folder(p) == dest))
        .collect();
    if top.len() == sources.len() {
        return sources
            .iter()
            .cloned()
            .zip(top.into_iter().cloned().map(Some))
            .collect();
    }
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    sources
        .iter()
        .map(|source| {
            let copy = top
                .iter()
                .find(|c| name(c) == name(source))
                .map(|c| (*c).clone());
            (source.clone(), copy)
        })
        .collect()
}

/// "Verify after copy": reads each copy back and compares it with its source, reporting progress
/// as bytes compared. Background thread.
fn verify_copies(
    sources: &[PathBuf],
    dest: &Path,
    created: &[PathBuf],
    progress: &OpProgress,
) -> Result<(), ShellError> {
    use arke_core::verify::Problem;
    use std::sync::atomic::Ordering;
    progress.verifying.store(true, Ordering::Relaxed);
    progress.work_done.store(0, Ordering::Relaxed);
    progress.work_total.store(
        progress.bytes_total.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    let mut failed = Vec::new();
    for (source, copy) in pair_copies(sources, dest, created) {
        let Some(copy) = copy else {
            let name = source
                .file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned());
            failed.push(t!("verify.reason_missing", name = name).into_owned());
            continue;
        };
        let found = arke_core::verify::compare(&source, &copy, &progress.cancel, &mut |n| {
            progress.work_done.fetch_add(n, Ordering::Relaxed);
        });
        for mismatch in found {
            let name = mismatch.path.display().to_string();
            failed.push(
                match mismatch.problem {
                    Problem::Missing => t!("verify.reason_missing", name = name),
                    Problem::Size => t!("verify.reason_size", name = name),
                    Problem::Content => t!("verify.reason_content", name = name),
                    Problem::Unreadable(why) => {
                        t!("verify.reason_unreadable", name = name, error = why)
                    }
                }
                .into_owned(),
            );
        }
    }
    progress.verifying.store(false, Ordering::Relaxed);
    if failed.is_empty() {
        log::info!("verified {} copies", sources.len());
        Ok(())
    } else {
        log::warn!("verify after copy: {} items differ", failed.len());
        Err(ShellError::Verify { failed })
    }
}

impl Explorer {
    /// Settings for a job thread. `transfer`: a copy or move the user started, which may go to
    /// TeraCopy (undo and redo never do).
    fn job_env(&self, transfer: bool, cx: &mut App) -> JobEnv {
        let settings = &crate::settings::SettingsStore::global(cx).read(cx).settings;
        JobEnv {
            shell: self.services.shell.clone(),
            owner: self.owner,
            serialize: settings.queue_same_drive,
            verify: settings.verify_copies,
            teracopy: (transfer && settings.use_teracopy)
                .then(crate::teracopy::found)
                .flatten()
                .map(Path::to_path_buf),
        }
    }

    pub(crate) fn owner_from(window: &Window) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        }
    }

    /// Full paths of the selection (only visible items while filtering). In search mode, the result
    /// under the cursor.
    pub(super) fn selected_targets(&self) -> Vec<(PathBuf, Arc<str>)> {
        if self.search_mode {
            return self
                .search_hit(self.search_cursor)
                .zip(self.search_results.as_ref())
                .map_or_default(|(hit, r)| vec![(r.root.join(&*hit.rel), hit.entry.name.clone())]);
        }
        let (Some(model), Some(view)) = (self.model.as_ref(), self.view()) else {
            return Vec::new();
        };
        view.iter()
            .filter(|e| self.selection.contains(&e.name))
            .map(|e| (model.path.join(&*e.name), e.name.clone()))
            .collect()
    }

    // ───────────────────────── Clipboard ─────────────────────────

    pub(super) fn copy_files(&mut self, _: &CopyFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.put_on_clipboard(false, cx);
    }

    pub(super) fn cut_files(&mut self, _: &CutFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.put_on_clipboard(true, cx);
    }

    fn put_on_clipboard(&mut self, cut: bool, cx: &mut Context<Self>) {
        let targets = self.selected_targets();
        if targets.is_empty() {
            return;
        }
        // Marked against the folder shown (where the names come from), so items inside expanded
        // tree folders dim too.
        let parent = if self.search_mode {
            targets[0].0.parent().map(PathBuf::from)
        } else {
            self.current_path().map(PathBuf::from)
        }
        .unwrap_or_default();
        let names: HashSet<Arc<str>> = targets.iter().map(|(_, n)| n.clone()).collect();
        let files = ClipboardFiles {
            paths: targets.into_iter().map(|(p, _)| p).collect(),
            cut,
        };
        let count = files.paths.len();
        let shell = self.services.shell.clone();
        let tx = self.events_tx.clone();
        spawn_thread("clipboard", move || {
            let result = shell.set_clipboard_files(&files);
            let _ = tx.send(AppEvent::ClipboardSet {
                result,
                cut,
                count,
                parent,
                names,
            });
        });
        cx.notify();
    }

    pub(super) fn handle_clipboard_set(
        &mut self,
        result: Result<u32, crate::shell::ShellError>,
        cut: bool,
        count: usize,
        parent: PathBuf,
        names: HashSet<Arc<str>>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(seq) => {
                if cut {
                    cx.set_global(CutMarks { seq, parent, names });
                }
                self.notice = Some(if cut {
                    tn!("clip.cut", count).into_owned()
                } else {
                    tn!("clip.copied", count).into_owned()
                });
            }
            Err(e) => self.error = Some(t!("clip.failed", error = e).into_owned()),
        }
        cx.notify();
    }

    /// Which of these names are currently cut and pending paste (drawn translucent).
    pub(crate) fn cut_flags(&self, names: &[Arc<str>], cx: &App) -> Vec<bool> {
        let marks = cx.try_global::<CutMarks>().filter(|m| {
            m.seq == self.services.shell.clipboard_seq()
                && self.current_path() == Some(m.parent.as_path())
        });
        names
            .iter()
            .map(|n| marks.is_some_and(|m| m.names.contains(n)))
            .collect()
    }

    pub(super) fn paste_files(&mut self, _: &PasteFiles, _: &mut Window, cx: &mut Context<Self>) {
        let Some(dest) = self.current_path().map(PathBuf::from) else {
            return;
        };
        let shell = self.services.shell.clone();
        let env = self.job_env(true, cx);
        let tx = self.events_tx.clone();
        self.ops_running += 1;
        let (job, progress) = self.track_op("op.paste", None, true, cx);
        spawn_thread("paste", move || {
            let mut follow = OpFollowUp {
                dest: Some(dest.clone()),
                job,
                ..OpFollowUp::default()
            };
            let mut verb = "op.paste";
            let result = match shell.clipboard_files() {
                Err(e) => Err(e),
                Ok(None) => Err(crate::shell::ShellError::ClipboardEmpty),
                // Cut and pasted back into the same folder: nothing to do.
                Ok(Some((files, _)))
                    if files.cut && files.paths.iter().all(|p| p.parent() == Some(&*dest)) =>
                {
                    Ok(OpOutcome::Done)
                }
                Ok(Some((files, seq))) => {
                    follow.select = files
                        .paths
                        .iter()
                        .filter_map(|p| p.file_name())
                        .map(|n| Arc::from(n.to_string_lossy().as_ref()))
                        .collect();
                    let cut = files.cut;
                    follow.subject = files
                        .paths
                        .first()
                        .and_then(|p| p.file_name())
                        .map(|n| (n.to_string_lossy().into_owned(), files.paths.len()));
                    let op = if cut {
                        FileOp::Move {
                            sources: files.paths,
                            dest,
                        }
                    } else {
                        FileOp::Copy {
                            sources: files.paths,
                            dest,
                        }
                    };
                    verb = op.verb();
                    let (result, undo, handed_off) = env.run(&op, &progress);
                    follow.undo = undo;
                    follow.op = Some(Box::new(op));
                    follow.handed_off = handed_off;
                    // Like Explorer, cut items leave the clipboard once pasted.
                    if cut && matches!(result, Ok(OpOutcome::Done)) {
                        shell.clear_clipboard_if(seq);
                    }
                    result
                }
            };
            let _ = tx.send(AppEvent::OpFinished {
                verb,
                result,
                follow: Box::new(follow),
            });
        });
        cx.notify();
    }

    // ───────────────────────── Dual pane ─────────────────────────

    /// Alt+C: copy to the other pane.
    pub(super) fn copy_to_peer(
        &mut self,
        _: &super::CopyToPeer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane.peer.is_some() {
            self.transfer_to_peer(false, cx);
        } else {
            cx.propagate();
        }
    }

    /// Alt+M: move to the other pane.
    pub(super) fn move_to_peer(
        &mut self,
        _: &super::MoveToPeer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane.peer.is_some() {
            self.transfer_to_peer(true, cx);
        } else {
            cx.propagate();
        }
    }

    /// Copies or moves the selection to the other pane's folder via IFileOperation.
    pub(crate) fn transfer_to_peer(&mut self, is_move: bool, cx: &mut Context<Self>) {
        let Some(dest) = self.pane.peer.clone() else {
            return;
        };
        let sources: Vec<PathBuf> = self
            .selected_targets()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        if !sources.is_empty() && sources.iter().all(|p| p.parent() == Some(dest.as_path())) {
            self.notice = Some(t!("ops.same_folder").into_owned());
            cx.notify();
            return;
        }
        self.transfer_paths(sources, dest, is_move, cx);
    }

    /// Runs a copy or move through IFileOperation on a background thread and reports the result.
    pub(crate) fn run_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        is_move: bool,
        cx: &mut Context<Self>,
    ) {
        let touched = Some(dest.clone());
        let names: Vec<Arc<str>> = sources
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| Arc::from(n.to_string_lossy().as_ref()))
            .collect();
        let arrive = Some((dest.clone(), names));
        let op = if is_move {
            FileOp::Move { sources, dest }
        } else {
            FileOp::Copy { sources, dest }
        };
        let env = self.job_env(true, cx);
        let tx = self.events_tx.clone();
        self.ops_running += 1;
        let (job, progress) = self.track_op(op.verb(), op.subject(), true, cx);
        spawn_thread("transfer", move || {
            let verb = op.verb();
            let (result, undo, handed_off) = env.run(&op, &progress);
            let _ = tx.send(AppEvent::OpFinished {
                verb,
                result,
                follow: Box::new(OpFollowUp {
                    undo,
                    job,
                    op: Some(Box::new(op)),
                    handed_off,
                    touched,
                    arrive,
                    ..OpFollowUp::default()
                }),
            });
        });
        cx.notify();
    }

    // ───────────────────────── Delete ─────────────────────────

    pub(super) fn delete_files(
        &mut self,
        _: &DeleteFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete(true, window, cx);
    }

    pub(super) fn delete_permanently(
        &mut self,
        _: &DeletePermanently,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete(false, window, cx);
    }

    fn delete(&mut self, recycle: bool, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.selected_targets();
        if targets.is_empty() {
            return;
        }
        // Servers have no Recycle Bin: deleting there is permanent, so it always asks.
        let recycle = recycle && !targets.iter().any(|(p, _)| arke_core::remote::is_remote(p));
        let cursor = self.current_cursor().unwrap_or(0);
        let (paths, names): (Vec<_>, Vec<_>) = targets.into_iter().unzip();
        let follow = OpFollowUp {
            dest: self.current_path().map(PathBuf::from),
            deleted: (!self.search_mode).then_some((names, cursor)),
            announce: recycle.then_some(Announce::Recycled(paths.len())),
            ..OpFollowUp::default()
        };
        // Permanent delete always confirms since it can't be recovered. Recycling doesn't by
        // default
        // (Ctrl+Z undoes it) unless the setting asks to.
        let ask = recycle
            && crate::settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .confirm_recycle;
        // Network shares, USB sticks and the like have no Recycle Bin: Windows would delete
        // those permanently, so say so and ask first, like a permanent delete.
        let (paths, no_bin): (Vec<PathBuf>, Vec<PathBuf>) = if recycle {
            let shell = self.services.shell.clone();
            paths.into_iter().partition(|p| shell.has_recycle_bin(p))
        } else {
            (paths, Vec::new())
        };
        if !no_bin.is_empty() {
            self.ask_no_recycle_bin(paths, no_bin, follow, window, cx);
        } else if !recycle {
            self.ask_delete_permanently(paths, follow, window, cx);
        } else if ask {
            self.ask_recycle(paths, follow, window, cx);
        } else {
            self.run_op(FileOp::Delete { paths, recycle }, follow, cx);
        }
    }

    /// Deleted items have left the list: select the item at the old position so Delete can be
    /// pressed again.
    pub(super) fn try_reselect_after_delete(&mut self) {
        let Some((names, ix)) = &self.pending_reselect else {
            return;
        };
        let Some(view) = self.view() else { return };
        if names.iter().any(|n| view.iter().any(|e| e.name == *n)) {
            return; // watcher hasn't caught up yet
        }
        let ix = *ix;
        self.pending_reselect = None;
        if !view.is_empty() {
            let ix = ix.min(view.len() - 1);
            self.selection.select_only(&view, ix);
            self.cursor = Some(ix);
            self.scroll_to_index(ix, ScrollStrategy::Nearest);
        }
    }

    // ───────────────────────── Rename and new folder ─────────────────────────

    pub(super) fn rename_file(
        &mut self,
        _: &RenameFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_mode {
            return;
        }
        let name = self
            .cursor
            .and_then(|ix| self.view()?.get(ix).map(|e| e.name.clone()));
        let Some(name) = name else {
            return;
        };
        // Multiple selected with the cursor inside: edit the cursor item, then rename all selected
        // to
        // "name (1)", "name (2)", ...
        let batch: Vec<Arc<str>> = if self.selection.len() > 1 && self.selection.contains(&name) {
            self.view().map_or_default(|v| {
                v.iter()
                    .filter(|e| self.selection.contains(&e.name))
                    .map(|e| e.name.clone())
                    .collect()
            })
        } else {
            Vec::new()
        };
        self.start_rename(name, window, cx);
        if let Some(renaming) = self.renaming.as_mut() {
            renaming.batch = batch;
        }
    }

    pub(super) fn start_rename(
        &mut self,
        name: Arc<str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_rename_as(name, false, window, cx);
    }

    /// `fresh`: a file just made by "New file…": the whole name is selected, extension and all.
    fn start_rename_as(
        &mut self,
        name: Arc<str>,
        fresh: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .view()
            .and_then(|v| v.iter().find(|e| e.name == name).cloned())
        else {
            return;
        };
        // Nested tree rows are named by their relative path; only the file name is edited.
        let leaf = arke_core::tree::leaf(&name);
        let range = if fresh {
            0..leaf.len()
        } else {
            arke_core::path::rename_selection(leaf, entry.is_dir_like())
        };
        let text = leaf.to_string();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text(text, cx);
            input.select_range(range, cx);
            input
        });
        let focus = input.read(cx).focus_handle(cx);
        let subscriptions =
            [
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Confirm => this.commit_rename(window, cx),
                        InputEvent::Cancel => this.cancel_rename(window, cx),
                        InputEvent::Changed => this.rename_edited(cx),
                    }
                }),
                // Clicking elsewhere commits, like Explorer.
                cx.on_blur(&focus, window, |this, window, cx| {
                    this.commit_rename(window, cx);
                }),
            ];
        // Make this row the only selection and scroll it into view.
        if let Some(view) = self.view()
            && let Some(ix) = view.iter().position(|e| e.name == name)
        {
            self.selection.select_only(&view, ix);
            self.cursor = Some(ix);
            self.scroll_to_index(ix, ScrollStrategy::Nearest);
        }
        self.renaming = Some(Renaming {
            name,
            input,
            batch: Vec::new(),
            problem: None,
            fresh,
            _subscriptions: subscriptions,
        });
        self.error = None;
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Live check while typing: shows or clears the message under the box.
    fn rename_edited(&mut self, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.as_mut() else {
            return;
        };
        let problem = typing_problem(renaming.input.read(cx).text());
        if renaming.problem != problem {
            renaming.problem = problem;
            cx.notify();
        }
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            window.focus(&self.list_focus, cx);
            cx.notify();
        }
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut renaming) = self.renaming.take() else {
            return;
        };
        let typed = renaming.input.read(cx).text().to_owned();
        let new_name = typed.trim().to_owned();
        let old_leaf = arke_core::tree::leaf(&renaming.name);
        if new_name.is_empty() || new_name == old_leaf {
            window.focus(&self.list_focus, cx);
            cx.notify();
            return;
        }
        if let Some(problem) = commit_problem(&typed) {
            // Invalid name: stay in edit mode so it can be fixed, with the reason next to the box
            // rather than in a toast across the window.
            renaming.problem = Some(problem);
            let focus = renaming.input.read(cx).focus_handle(cx);
            self.renaming = Some(renaming);
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        window.focus(&self.list_focus, cx);
        let Some(dir) = self.current_path().map(PathBuf::from) else {
            return;
        };
        if renaming.batch.len() > 1 {
            self.commit_batch_rename(&dir, &renaming, &new_name, cx);
            return;
        }
        // The renamed row keeps its place in the tree.
        let new_name_arc =
            arke_core::tree::join(arke_core::tree::parent(&renaming.name), &new_name);
        // Point the selection at the new name first so it gets selected when the watcher reports
        // it.
        self.selection.select_name(new_name_arc.clone());
        self.scroll_to_cursor = Some(ScrollStrategy::Nearest);
        let follow = OpFollowUp {
            dest: Some(dir.clone()),
            select: vec![new_name_arc],
            ..OpFollowUp::default()
        };
        let path = dir.join(&*renaming.name);
        // Confirm extension changes on files (can be disabled in settings). Folders have no
        // extension.
        let is_file = self
            .view()
            .and_then(|v| {
                v.iter()
                    .find(|e| e.name == renaming.name)
                    .map(|e| !e.is_dir_like())
            })
            .unwrap_or(false);
        let warn = crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .warn_extension_change;
        let asked = renaming.fresh;
        if is_file && warn && !asked && super::confirm::extension_changed(old_leaf, &new_name) {
            self.ask_extension_change(path, new_name, follow, window, cx);
            return;
        }
        self.run_op(FileOp::Rename { path, new_name }, follow, cx);
    }

    /// Multi-item rename like Explorer: the typed name minus extension becomes the stem, and items
    /// are
    /// renamed in display order to "name (1)", "name (2)", ... keeping their own extensions.
    /// Runs as one operation with a single undo.
    fn commit_batch_rename(
        &mut self,
        dir: &Path,
        renaming: &Renaming,
        typed: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view() else { return };
        let edited_is_file = view
            .iter()
            .find(|e| e.name == renaming.name)
            .is_some_and(|e| !e.is_dir_like());
        let stem = if edited_is_file {
            Path::new(typed)
                .file_stem()
                .map_or_else(|| typed.to_owned(), |s| s.to_string_lossy().into_owned())
        } else {
            typed.to_owned()
        };
        let items: Vec<(PathBuf, String)> = renaming
            .batch
            .iter()
            .enumerate()
            .filter_map(|(ix, name)| {
                let entry = view.iter().find(|e| e.name == *name)?;
                let ext = (!entry.is_dir_like())
                    .then(|| Path::new(&**name).extension())
                    .flatten()
                    .map_or_default(|e| format!(".{}", e.to_string_lossy()));
                let new_name = format!("{stem} ({}){ext}", ix + 1);
                (new_name != arke_core::tree::leaf(name)).then(|| (dir.join(&**name), new_name))
            })
            .collect();
        if items.is_empty() {
            return;
        }
        // Select the new names where the old ones were (nested tree rows stay in their folder).
        let select: Vec<Arc<str>> = items
            .iter()
            .map(|(path, n)| {
                let old = path.strip_prefix(dir).ok().and_then(Path::to_str);
                arke_core::tree::join(old.and_then(arke_core::tree::parent), n)
            })
            .collect();
        self.selection.select_names(select.clone());
        let follow = OpFollowUp {
            dest: Some(dir.to_path_buf()),
            select,
            ..OpFollowUp::default()
        };
        self.run_op(FileOp::RenameMany { items }, follow, cx);
    }

    pub(super) fn new_folder(&mut self, _: &NewFolder, _: &mut Window, cx: &mut Context<Self>) {
        self.create_new(NewItem::Folder, None, cx);
    }

    /// Creates a new folder or file in the folder shown (`row: None`) or in the expanded tree
    /// folder `row`, picking a name not taken there, then starts renaming it once it appears.
    fn create_new(&mut self, kind: NewItem, row: Option<&Arc<str>>, cx: &mut Context<Self>) {
        let Some(shown) = self.current_path().map(Path::to_path_buf) else {
            return;
        };
        let listing = match row {
            None => self.model.as_ref(),
            Some(row) => self.tree_folder(row),
        };
        let Some(listing) = listing else { return };
        // Only checks listed items (the UI thread doesn't touch the file system). If it collides
        // with a hidden item, Windows renames it automatically.
        let existing: HashSet<String> = listing
            .entries
            .iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        let name = match kind {
            NewItem::Folder => {
                arke_core::path::unique_name(&t!("ops.new_folder_name"), |n| existing.contains(n))
            }
            NewItem::TextFile | NewItem::File => {
                arke_core::path::unique_file_name(&t!("ops.new_text_file_name"), "txt", |n| {
                    existing.contains(n)
                })
            }
        };
        let parent = listing.path.clone();
        // Nested tree rows are named by their path relative to the folder shown.
        let name_arc = arke_core::tree::join(row.map(|r| &**r), &name);
        let follow = OpFollowUp {
            dest: Some(shown),
            select: vec![name_arc.clone()],
            rename: Some((name_arc, kind == NewItem::File)),
            ..OpFollowUp::default()
        };
        let op = match kind {
            NewItem::Folder => FileOp::NewFolder { parent, name },
            NewItem::TextFile | NewItem::File => FileOp::NewFile { parent, name },
        };
        self.run_op(op, follow, cx);
    }

    /// "New folder…" / "New file…" on a folder item: creates it inside that folder. In the tree
    /// the folder opens in place and the new row is renamed there; elsewhere the folder is opened
    /// first, like Explorer's new items. Waits for the folder's listing so the name is free.
    pub(super) fn new_item_in(&mut self, kind: NewItem, dir: &Path, cx: &mut Context<Self>) {
        // A server's shares are only listed (see `changes_share_list`).
        if arke_core::net::bare_server(dir).is_some() {
            return;
        }
        let Some(shown) = self.current_path().map(Path::to_path_buf) else {
            return;
        };
        let row: Option<Arc<str>> = dir
            .strip_prefix(&shown)
            .ok()
            .and_then(Path::to_str)
            .filter(|rel| !rel.is_empty())
            .map(Arc::from);
        if self.tree_active()
            && let Some(row) = row
            && self.tree_open_row(&row, cx)
        {
            self.pending_create = Some(PendingCreate {
                kind,
                dir: dir.to_path_buf(),
                row: Some(row),
            });
        } else if dir == shown {
            self.create_new(kind, None, cx);
            return;
        } else {
            let dir = arke_core::remote::canonical(dir.to_path_buf());
            self.navigate(dir.clone(), arke_core::NavKind::New, cx);
            self.pending_create = Some(PendingCreate {
                kind,
                dir,
                row: None,
            });
        }
        self.poll_pending_create(cx);
    }

    /// Runs a [`PendingCreate`] once its folder is listed; drops it if the folder went away
    /// (collapsed, failed to open, navigated elsewhere).
    pub(super) fn poll_pending_create(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_create.as_ref() else {
            return;
        };
        let ready = match &pending.row {
            Some(row) => self.tree_folder(row).map(|m| m.complete),
            None if self.pending.is_some() => Some(false),
            None => self
                .model
                .as_ref()
                .filter(|m| m.path == pending.dir)
                .map(|m| m.complete),
        };
        match ready {
            Some(false) => {}
            None => self.pending_create = None,
            Some(true) => {
                if let Some(pending) = self.pending_create.take() {
                    self.create_new(pending.kind, pending.row.as_ref(), cx);
                }
            }
        }
    }

    /// "Move to new folder": creates a folder here, moves the selection into it, then starts
    /// renaming it.
    /// Both steps run in order on one background thread. Ctrl+Z undoes the move and leaves the
    /// empty folder.
    pub(super) fn move_into_new_folder(&mut self, cx: &mut Context<Self>) {
        if self.search_mode {
            return;
        }
        let Some(model) = &self.model else { return };
        let sources: Vec<PathBuf> = self
            .selected_targets()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        if sources.is_empty() {
            return;
        }
        let existing: HashSet<String> = model
            .entries
            .iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        let name =
            arke_core::path::unique_name(&t!("ops.new_folder_name"), |n| existing.contains(n));
        let parent = model.path.clone();
        let name_arc: Arc<str> = Arc::from(name.as_str());
        let follow = OpFollowUp {
            dest: Some(parent.clone()),
            select: vec![name_arc.clone()],
            rename: Some((name_arc, false)),
            announce: Some(Announce::MovedToNewFolder(sources.len())),
            ..OpFollowUp::default()
        };
        let create = FileOp::NewFolder {
            parent: parent.clone(),
            name: name.clone(),
        };
        let mv = FileOp::Move {
            sources,
            dest: parent.join(&name),
        };
        let shell = self.services.shell.clone();
        let env = self.job_env(false, cx);
        let tx = self.events_tx.clone();
        let owner = self.owner;
        self.ops_running += 1;
        self.notice = None;
        let (job, progress) = self.track_op(mv.verb(), mv.subject(), mv.listed(), cx);
        spawn_thread("move-to-new-folder", move || {
            let (created, _) = shell.file_op_tracked(&create, owner, &progress);
            if let Err(error) = created {
                let _ = tx.send(AppEvent::OpFinished {
                    verb: create.verb(),
                    result: Err(error),
                    follow: Box::new(OpFollowUp {
                        job,
                        ..OpFollowUp::default()
                    }),
                });
                return;
            }
            let (result, undo, _) = env.run(&mv, &progress);
            let _ = tx.send(AppEvent::OpFinished {
                verb: mv.verb(),
                result,
                follow: Box::new(OpFollowUp {
                    undo,
                    job,
                    op: Some(Box::new(mv)),
                    ..follow
                }),
            });
        });
        cx.notify();
    }

    /// "New text document": picks a unique name like new folder, then renames it with only the stem
    /// selected.
    pub(super) fn new_text_file(&mut self, cx: &mut Context<Self>) {
        self.create_new(NewItem::TextFile, None, cx);
    }

    /// Starts renaming the new folder once it appears. Checked during render because moving focus
    /// needs a Window.
    pub(super) fn poll_pending_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((name, fresh)) = self.rename_when_appears.clone() else {
            return;
        };
        if self
            .view()
            .is_some_and(|v| v.iter().any(|e| e.name == name))
        {
            self.rename_when_appears = None;
            self.start_rename_as(name, fresh, window, cx);
        }
    }

    // ───────────────────────── Undo ─────────────────────────

    /// Ctrl+Z: undoes the last file operation from any tab. Restored items in the current folder
    /// get selected.
    pub(super) fn undo_file_op(&mut self, _: &UndoFileOp, _: &mut Window, cx: &mut Context<Self>) {
        let last = UndoHistory::last(cx).map(|e| e.id);
        self.undo_by_id(last, cx);
    }

    /// Ctrl+Y / Ctrl+Shift+Z: redoes the last undone file operation.
    pub(super) fn redo_file_op(&mut self, _: &RedoFileOp, _: &mut Window, cx: &mut Context<Self>) {
        let last = RedoStack::last(cx).map(|e| e.id);
        self.redo_by_id(last, cx);
    }

    /// "Redo" in the progress center: redoes that undone operation, the same way Ctrl+Y does.
    pub(crate) fn redo_op(&mut self, id: u64, cx: &mut Context<Self>) {
        self.redo_by_id(Some(id), cx);
    }

    fn redo_by_id(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        let Some(entry) = id.and_then(|id| RedoStack::take(cx, id)) else {
            self.notice = Some(t!("redo.nothing").into_owned());
            cx.notify();
            return;
        };
        // Select what the redo leaves here: the new name of a rename, a new folder.
        let (dest, select): (Option<PathBuf>, Vec<Arc<str>>) = match &entry.op {
            FileOp::Rename { path, new_name } => (
                path.parent().map(Path::to_path_buf),
                vec![Arc::from(new_name.as_str())],
            ),
            FileOp::NewFolder { parent, name } | FileOp::NewFile { parent, name } => {
                (Some(parent.clone()), vec![Arc::from(name.as_str())])
            }
            _ => (None, Vec::new()),
        };
        let follow = OpFollowUp {
            dest,
            select,
            redone: Some(entry.verb),
            ..OpFollowUp::default()
        };
        self.run_op(entry.op, follow, cx);
    }

    /// "Undo" on a notice or in the progress center: undoes that operation (if it still can be),
    /// the same way Ctrl+Z does.
    pub(crate) fn undo_op(&mut self, id: u64, cx: &mut Context<Self>) {
        self.undo_by_id(Some(id), cx);
    }

    fn undo_by_id(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        let Some(entry) = id.and_then(|id| UndoHistory::take(cx, id)) else {
            self.notice = Some(t!("undo.nothing").into_owned());
            cx.notify();
            return;
        };
        let select: Vec<Arc<str>> = match &entry.undo {
            Undo::PutBack(items) => items.iter().map(|i| Arc::from(i.name.as_str())).collect(),
            Undo::Remove(_) => Vec::new(),
        };
        let follow = OpFollowUp {
            dest: entry.undo.target_dir().map(Path::to_path_buf),
            select,
            undid: Some((entry.verb, entry.undo.len())),
            redo: entry.redo.clone().map(|op| Box::new((entry.verb, op))),
            ..OpFollowUp::default()
        };
        let env = self.job_env(false, cx);
        let tx = self.events_tx.clone();
        self.ops_running += 1;
        self.notice = None;
        // Progress center shows "Undo <name> and N more".
        let (job, progress) = self.track_op("op.undo", undo_subject(&entry.undo), true, cx);
        let follow = OpFollowUp { job, ..follow };
        spawn_thread("undo", move || {
            let result = env.undo(&entry.undo, &progress);
            let _ = tx.send(AppEvent::OpFinished {
                verb: "op.undo",
                result,
                follow: Box::new(follow),
            });
        });
        cx.notify();
    }

    // ───────────────────────── Execution and results ─────────────────────────

    /// Starts tracking an operation's progress (drawn by Arke instead of the Windows progress
    /// dialog).
    /// The background thread writes counts into the returned handle. `listed` adds it to the
    /// progress
    /// center and returns its id; `verb` is a locale key; `subject` is the first name and item
    /// count.
    fn track_op(
        &mut self,
        verb: &'static str,
        subject: Option<(String, usize)>,
        listed: bool,
        cx: &mut Context<Self>,
    ) -> (Option<u64>, Arc<crate::shell::OpProgress>) {
        let (job, progress) = if listed {
            let (id, progress) = crate::op_center::OpCenter::begin(cx, verb, subject);
            (Some(id), progress)
        } else {
            (None, Arc::new(crate::shell::OpProgress::default()))
        };
        self.op_progress = Some((progress.clone(), verb));
        (job, progress)
    }

    /// The error toast for a failed operation, with Retry (also in the operation center) when
    /// trying again can help.
    fn show_op_failure(
        &mut self,
        verb: &'static str,
        error: &ShellError,
        op: Option<&FileOp>,
        job: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        // Nothing new to show: don't jump to the cursor later, when something else changes.
        self.scroll_to_cursor = None;
        let retry = op.and_then(|op| retry_of(op, error));
        if let (Some(job), Some(op)) = (job, retry.clone()) {
            crate::op_center::OpCenter::set_retry(cx, job, op);
        }
        self.error = Some(t!("ops.failed", op = t!(verb), error = error).into_owned());
        self.error_action = retry.map(|op| super::ErrorAction::RetryOp(Box::new(op)));
    }

    /// Runs a failed operation again on the items that failed (the error toast's and the
    /// operation center's Retry). Permanent deletes were confirmed the first time.
    pub(crate) fn retry_op(&mut self, op: FileOp, cx: &mut Context<Self>) {
        self.error = None;
        self.error_action = None;
        let announce = match &op {
            FileOp::Delete {
                paths,
                recycle: true,
            } => Some(Announce::Recycled(paths.len())),
            _ => None,
        };
        let follow = OpFollowUp {
            dest: self.current_path().map(PathBuf::from),
            announce,
            ..OpFollowUp::default()
        };
        self.run_op(op, follow, cx);
    }

    pub(super) fn run_op(&mut self, op: FileOp, follow: OpFollowUp, cx: &mut Context<Self>) {
        let env = self.job_env(false, cx);
        let tx = self.events_tx.clone();
        self.ops_running += 1;
        self.notice = None;
        let (job, progress) = self.track_op(op.verb(), op.subject(), op.listed(), cx);
        spawn_thread("file-op", move || {
            let (result, undo, _) = env.run(&op, &progress);
            let _ = tx.send(AppEvent::OpFinished {
                verb: op.verb(),
                result,
                follow: Box::new(OpFollowUp {
                    undo,
                    job,
                    op: Some(Box::new(op)),
                    ..follow
                }),
            });
        });
        cx.notify();
    }

    /// Selects items that just arrived in this folder (a copy or move from either pane), like a
    /// paste does, and scrolls to them. Names not listed yet are picked up when they appear.
    pub(crate) fn select_arrived(&mut self, names: Vec<Arc<str>>, cx: &mut Context<Self>) {
        self.selection = Selection::default();
        self.selection.select_names(names);
        self.scroll_to_cursor = Some(ScrollStrategy::Nearest);
        self.resolve_cursor();
        cx.notify();
    }

    pub(super) fn handle_op_finished(
        &mut self,
        verb: &'static str,
        result: Result<OpOutcome, crate::shell::ShellError>,
        follow: OpFollowUp,
        cx: &mut Context<Self>,
    ) {
        self.ops_running = self.ops_running.saturating_sub(1);
        if self.ops_running == 0 {
            self.op_progress = None;
        }
        // Delete, copy and move change free space, so refresh the sidebar capacity bars.
        self.refresh_disk_space();
        // Shelf items may have been moved or deleted.
        self.recheck_shelf(cx);
        if let Some(id) = follow.job {
            if let Some(subject) = follow.subject.clone() {
                crate::op_center::OpCenter::set_subject(cx, id, subject);
            }
            crate::op_center::OpCenter::finish(cx, id, verb, &result);
        }
        // Record whatever completed, even on cancel, so Ctrl+Z can undo it.
        let redo = follow
            .op
            .as_ref()
            .zip(follow.undo.as_ref())
            .and_then(|(op, undo)| crate::shell::redo_of(op, undo));
        let undo_id = follow
            .undo
            .clone()
            .map(|undo| UndoHistory::push(cx, verb, undo, redo, follow.redone.is_none()));
        if let (Some(job), Some(undo)) = (follow.job, undo_id) {
            crate::op_center::OpCenter::set_undo(cx, job, undo);
        }
        if let (Some((original, count)), Ok(OpOutcome::Done)) = (follow.undid, &result) {
            self.notice = Some(tn!("undo.done", count, op = t!(original)).into_owned());
        }
        // Undone completely: Ctrl+Y can redo it.
        if let (Some(redo), Ok(OpOutcome::Done)) = (follow.redo.clone(), &result) {
            let (original, op) = *redo;
            let redo_id = RedoStack::push(cx, original, op);
            if let Some(job) = follow.job {
                crate::op_center::OpCenter::set_redo(cx, job, redo_id);
            }
        }
        if let (Some(original), Ok(OpOutcome::Done)) = (follow.redone, &result) {
            self.notice = Some(t!("redo.done", op = t!(original)).into_owned());
        }
        if follow.handed_off && result.is_ok() {
            self.notice = Some(t!("teracopy.handed_off").into_owned());
        }
        if let (Some(announce), Ok(OpOutcome::Done)) = (follow.announce, &result) {
            let text = announce.text();
            self.notice_undo = undo_id.map(|id| (text.clone(), id));
            self.notice = Some(text);
        }
        let here = follow.dest.is_none() || follow.dest.as_deref() == self.current_path();
        // Servers send no change notifications: relist to show what the operation changed.
        if self
            .current_path()
            .is_some_and(arke_core::remote::is_remote)
        {
            self.reload(cx);
        }
        if let Some(other) = follow
            .touched
            .clone()
            .filter(|p| arke_core::remote::is_remote(p) && Some(p.as_path()) != self.current_path())
        {
            cx.emit(super::ExplorerEvent::RemoteChanged(other));
        }
        // Copied or moved items: select them where they landed.
        if let (Ok(OpOutcome::Done), false, Some((dir, names))) =
            (&result, follow.handed_off, follow.arrive)
            && !names.is_empty()
        {
            if self.current_path() == Some(dir.as_path()) {
                self.select_arrived(names, cx);
            } else {
                cx.emit(super::ExplorerEvent::Arrived { dir, names });
            }
        }
        if let Some(op) = follow.op.as_deref() {
            self.forget_search_hits(&removed_paths(op, &result), cx);
        }
        match result {
            Err(e) => self.show_op_failure(verb, &e, follow.op.as_deref(), follow.job, cx),
            Ok(OpOutcome::Cancelled) => {
                self.scroll_to_cursor = None;
                self.notice = Some(t!("ops.cancelled", op = t!(verb)).into_owned());
            }
            Ok(OpOutcome::Done) if here => {
                if !follow.select.is_empty() {
                    self.selection = Selection::default();
                    self.selection.select_names(follow.select);
                    self.scroll_to_cursor = Some(ScrollStrategy::Nearest);
                }
                if let Some(rename) = follow.rename {
                    self.rename_when_appears = Some(rename);
                }
                if let Some(deleted) = follow.deleted {
                    self.selection.clear();
                    self.pending_reselect = Some(deleted);
                    self.try_reselect_after_delete();
                }
                self.resolve_cursor();
            }
            Ok(OpOutcome::Done) => {}
        }
        cx.notify();
    }
}

/// First name and item count of an undo, for the progress center title.
fn undo_subject(undo: &Undo) -> Option<(String, usize)> {
    let first = match undo {
        Undo::PutBack(items) => items.first().map(|i| i.name.clone()),
        Undo::Remove(paths) => paths
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
    }?;
    Some((first, undo.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_pair_with_their_sources_in_order_or_by_name() {
        let sources = [PathBuf::from(r"C:\a\x.txt"), PathBuf::from(r"C:\a\y")];
        // Renamed on collision: order decides.
        let renamed = [
            PathBuf::from(r"C:\a\x - Copy.txt"),
            PathBuf::from(r"C:\a\y - Copy"),
        ];
        assert_eq!(
            pair_copies(&sources, Path::new(r"C:\a"), &renamed),
            [
                (sources[0].clone(), Some(renamed[0].clone())),
                (sources[1].clone(), Some(renamed[1].clone())),
            ]
        );
        // Cancelled after the first, with a nested item reported too: by name.
        let partial = [
            PathBuf::from(r"D:\b\X.TXT"),
            PathBuf::from(r"D:\b\X.TXT\inner"),
        ];
        assert_eq!(
            pair_copies(&sources, Path::new(r"D:\b\"), &partial),
            [
                (sources[0].clone(), Some(partial[0].clone())),
                (sources[1].clone(), None),
            ]
        );
    }

    fn failure(path: &str, reason: arke_core::failure::FailReason) -> crate::shell::ItemFailure {
        crate::shell::ItemFailure::new(PathBuf::from(path), reason, "Some reason".into())
    }

    /// Retry takes the selected items that failed (a folder for a failure inside it), and only
    /// when retrying can help; search results drop everything else that went.
    #[test]
    fn retry_and_removed_items_after_a_partial_delete() {
        use arke_core::failure::FailReason;
        let paths: Vec<PathBuf> = [r"C:\t\a.txt", r"C:\t\dir", r"C:\t\ok.txt", r"C:\t\x."]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let op = FileOp::Delete {
            paths: paths.clone(),
            recycle: true,
        };
        let partial = || ShellError::Items {
            failed: vec![
                failure(r"C:\t\a.txt", FailReason::InUse),
                failure(r"C:\t\dir\inner\held.txt", FailReason::AccessDenied),
                failure(r"C:\t\x.", FailReason::BadName),
            ],
        };
        assert_eq!(
            retry_of(&op, &partial()),
            Some(FileOp::Delete {
                paths: vec![PathBuf::from(r"C:\t\a.txt"), PathBuf::from(r"C:\t\dir")],
                recycle: true,
            })
        );
        // A name the Recycle Bin can't take fails the same way every time.
        let names_only = ShellError::Items {
            failed: vec![failure(r"C:\t\x.", FailReason::BadName)],
        };
        assert_eq!(retry_of(&op, &names_only), None);
        assert_eq!(retry_of(&op, &ShellError::Unsupported), None);

        assert_eq!(
            removed_paths(&op, &Err(partial())),
            [PathBuf::from(r"C:\t\ok.txt")]
        );
        assert_eq!(removed_paths(&op, &Ok(OpOutcome::Done)), paths);
        assert!(removed_paths(&op, &Ok(OpOutcome::Cancelled)).is_empty());
    }

    /// One failure reads as one sentence naming the file; several are counted and listed (the
    /// first three); in-use failures say what to do.
    #[test]
    fn failure_reports_name_the_items_and_say_what_to_do() {
        use crate::shell::{Holder, items_failed};
        use arke_core::failure::FailReason;
        let mut held = failure(r"C:\t\report.docx", FailReason::InUse);
        held.holders = vec![
            Holder {
                app: "Microsoft Word".into(),
                pid: 1234,
            },
            Holder {
                app: "Backup".into(),
                pid: 99,
            },
            Holder {
                app: "Indexer".into(),
                pid: 7,
            },
        ];
        let apps = format!(
            "{}{}{}{}",
            t!("fail.app_pid", app = "Microsoft Word", pid = 1234),
            t!("ops.list_separator"),
            t!("fail.app_pid", app = "Backup", pid = 99),
            t!("fail.more_apps", count = 1)
        );
        let held_text = t!("fail.in_use_by", name = "report.docx", apps = apps).into_owned();
        assert_eq!(held.text(), held_text);
        assert_eq!(
            items_failed(std::slice::from_ref(&held)),
            format!(
                "{held_text}{}{}",
                t!("fail.sentence_gap"),
                t!("fail.hint_close")
            )
        );

        let denied = failure(r"C:\t\secret.txt", FailReason::AccessDenied);
        assert_eq!(denied.text(), t!("fail.access_denied", name = "secret.txt"));
        let other = failure(r"C:\t\odd.bin", FailReason::Other);
        assert_eq!(
            other.text(),
            t!("error.path", path = "odd.bin", error = "Some reason")
        );
        let many = [
            denied,
            other,
            failure(r"C:\t\long", FailReason::PathTooLong),
            failure(r"C:\t\fourth", FailReason::DiskFull),
        ];
        let text = items_failed(&many);
        assert!(text.contains("secret.txt") && text.contains("odd.bin") && text.contains("long"));
        assert!(!text.contains("fourth"), "only the first three: {text}");
        assert!(text.contains('…'), "{text}");
        assert!(!text.contains(&*t!("fail.hint_close")), "{text}");

        let bin = [failure(r"\\server\share\x.txt", FailReason::NoRecycleBin)];
        assert!(items_failed(&bin).ends_with(&*t!("fail.hint_permanent")));
    }
}
