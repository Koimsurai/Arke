//! GPUI integration tests driving the whole Explorer view with FakeFs / FakeShell.
//!
//! The test thread is the UI thread (`mark_ui_thread` is called) and FakeFs panics when called
//! from it, so any UI-thread I/O fails the test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use arke_core::failure::FailReason;
use arke_core::fake::FakeFs;
use arke_core::path::NameError;
use arke_core::{Entry, EntryKind, NavKind, SortColumn, SortSpec};
use gpui::{App, Entity, TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use rust_i18n::t;

use crate::explorer::MenuCommand;
use crate::explorer::{
    DeleteFiles, Explorer, ExplorerEvent, FocusFilter, FocusPathBar, FocusSearch, OpenInNewTab,
    Services,
};
use crate::media::MediaKind;
use crate::shell::{
    ClipboardFiles, FileOp, KnownFolder, OpOutcome, Place, PlaceKind, Shell, ShellError,
};
use crate::workspace::{CloseTab, NewTab, Workspace};

#[path = "tests_folder_views.rs"]
mod folder_views;
#[cfg(windows)]
#[path = "tests_live_recycle.rs"]
mod live_recycle;
#[path = "tests_motion.rs"]
mod motion;
#[path = "tests_preview_shelf.rs"]
mod preview_shelf;
#[path = "tests_remote.rs"]
mod remote;
#[path = "tests_tabs.rs"]
mod tabs;
#[path = "tests_columns.rs"]
mod tests_columns;
#[path = "tests_views.rs"]
mod views;

type OpHook = Box<dyn Fn(&FileOp) + Send + Sync>;
/// A PDF page render request: path, page, box.
type PdfRenderCall = (PathBuf, u32, (u32, u32));

#[derive(Default)]
struct FakeShell {
    opened: Mutex<Vec<PathBuf>>,
    ops: Mutex<Vec<FileOp>>,
    clipboard: Mutex<Option<(ClipboardFiles, u32)>>,
    seq: AtomicU32,
    /// Simulates an operation's effect on the file system (e.g. a new folder appears in FakeFs).
    on_op: Mutex<Option<OpHook>>,
    icons: Mutex<Vec<PathBuf>>,
    system_menus: Mutex<Vec<Vec<PathBuf>>>,
    shell_invoked: Mutex<Vec<(u64, u32)>>,
    shell_released: Mutex<Vec<u64>>,
    /// Folders whose background "Show more options" menu was read.
    background_menus: Mutex<Vec<Vec<PathBuf>>>,
    /// Each menu read gets a new session, starting at 7.
    shell_sessions: std::sync::atomic::AtomicU64,
    clipboard_text: Mutex<Option<String>>,
    revealed: Mutex<Vec<(PathBuf, bool)>>,
    /// Undos that ran.
    undone: Mutex<Vec<crate::shell::Undo>>,
    /// Fake recycle bin, plus names that were permanently deleted.
    recycled: Mutex<Vec<crate::shell::RecycledItem>>,
    purged: Mutex<Vec<String>>,
    /// Files handed to Windows drag and drop (dragged into other apps).
    dragged_out: Mutex<Vec<Vec<PathBuf>>>,
    /// Network connect requests sent by Arke's dialog.
    net_connects: Mutex<Vec<crate::shell::NetConnect>>,
    /// Network drives that were disconnected.
    net_disconnects: Mutex<Vec<PathBuf>>,
    /// Item the drag-out image was built for, and the item count.
    dragged_images: Mutex<Vec<Option<(PathBuf, usize)>>>,
    /// Commands run from the path bar (command line, folder).
    ran: Mutex<Vec<(String, PathBuf)>>,
    /// When dragging out to another app, simulate dragging back into Arke.
    drag_returns: std::sync::atomic::AtomicBool,
    /// Extra places (e.g. Downloads pointing at a temp folder).
    extra_places: Mutex<Vec<Place>>,
    /// Drag images built (request, whether a cached thumbnail existed).
    drag_image_requests: Mutex<Vec<(crate::shell::DragImageRequest, bool)>>,
    /// Current disk (free, total).
    disk_free: Mutex<Vec<(PathBuf, (u64, u64))>>,
    /// Shortcut targets (path, whether it's a folder).
    shortcuts: Mutex<Vec<(PathBuf, PathBuf, bool)>>,
    /// Cloud file "keep on device" / "free up space" calls.
    cloud_pins: Mutex<Vec<(Vec<PathBuf>, crate::shell::CloudPin)>>,
    /// Create-shortcut requests (sources, destination).
    shortcuts_made: Mutex<Vec<(Vec<PathBuf>, PathBuf)>>,
    /// Windows' recent files list, newest first, and the limit Arke asked for.
    recent: Mutex<Vec<crate::home::RecentFile>>,
    recent_limit: Mutex<Option<usize>>,
    /// What the next file dialogs return (`None` is Cancel), and the requests they got.
    picks: Mutex<Vec<Option<PathBuf>>>,
    pick_requests: Mutex<Vec<crate::shell::PickRequest>>,
    /// PDF pages rendered.
    pdf_renders: Mutex<Vec<PdfRenderCall>>,
    /// What `cloud_scan` reports, and the folders and items it was asked about.
    cloud: Mutex<crate::shell::CloudScan>,
    cloud_scans: Mutex<Vec<(PathBuf, Vec<Arc<str>>)>>,
    /// Network probe handed to the directory service (servers and their shares).
    net_probe: Mutex<Option<Arc<dyn arke_core::net::NetProbe>>>,
    /// Items deletes fail on: why, and the programs holding them.
    fail_delete: Mutex<Vec<(PathBuf, FailReason, Vec<crate::shell::Holder>)>>,
    /// Paths whose drive has no Recycle Bin.
    no_bin: Mutex<Vec<PathBuf>>,
}

impl FakeShell {
    fn ops(&self) -> Vec<FileOp> {
        self.ops.lock().clone()
    }

    fn clipboard(&self) -> Option<ClipboardFiles> {
        self.clipboard.lock().as_ref().map(|(c, _)| c.clone())
    }
}

impl Shell for FakeShell {
    fn load_places(&self) -> Vec<Place> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "load_places called on the UI thread"
        );
        let mut places = vec![Place {
            label: "Desktop".into(),
            path: PathBuf::from(r"C:\home\Desktop"),
            kind: PlaceKind::Folder(KnownFolder::Desktop),
        }];
        places.extend(self.extra_places.lock().iter().cloned());
        places
    }

    fn open(&self, path: &Path) -> Result<(), ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "open called on the UI thread"
        );
        self.opened.lock().push(path.to_path_buf());
        Ok(())
    }

    fn expand_env(&self, input: &str) -> String {
        input.replace("%HOME%", r"C:\home")
    }

    fn format_time(&self, _: SystemTime) -> String {
        "2026-01-01 00:00".into()
    }

    fn file_op(&self, op: &FileOp, _owner: Option<isize>) -> Result<OpOutcome, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "file_op called on the UI thread"
        );
        self.ops.lock().push(op.clone());
        if let Some(hook) = self.on_op.lock().as_ref() {
            hook(op);
        }
        Ok(OpOutcome::Done)
    }

    fn file_op_undoable(
        &self,
        op: &FileOp,
        owner: Option<isize>,
    ) -> (Result<OpOutcome, ShellError>, Option<crate::shell::Undo>) {
        use crate::shell::{PutBack, Undo};
        let put_back = |paths: &[PathBuf], now: &dyn Fn(&Path) -> String| {
            Undo::PutBack(
                paths
                    .iter()
                    .map(|p| PutBack {
                        now: now(p),
                        parent: p.parent().unwrap().to_path_buf(),
                        name: p.file_name().unwrap().to_string_lossy().into_owned(),
                    })
                    .collect(),
            )
        };
        // Deletes that fail on some items: the rest go, and only they can be undone.
        if let FileOp::Delete { paths, recycle } = op {
            let failing = self.fail_delete.lock().clone();
            let (bad, good): (Vec<PathBuf>, Vec<PathBuf>) = paths
                .iter()
                .cloned()
                .partition(|p| failing.iter().any(|(f, _, _)| f == p));
            if !bad.is_empty() {
                let done = FileOp::Delete {
                    paths: good.clone(),
                    recycle: *recycle,
                };
                // The operation as asked; only the items that went change the file system.
                self.ops.lock().push(op.clone());
                if !good.is_empty()
                    && let Some(hook) = self.on_op.lock().as_ref()
                {
                    hook(&done);
                }
                let undo = (*recycle && !good.is_empty()).then(|| {
                    put_back(&good, &|p| {
                        format!(
                            r"C:\$Recycle.Bin\{}",
                            p.file_name().unwrap().to_string_lossy()
                        )
                    })
                });
                let failed = bad
                    .into_iter()
                    .map(|path| {
                        let (_, reason, holders) =
                            failing.iter().find(|(f, _, _)| *f == path).unwrap().clone();
                        crate::shell::ItemFailure {
                            path,
                            reason,
                            detail: String::new(),
                            holders,
                        }
                    })
                    .collect();
                return (Err(ShellError::Items { failed }), undo);
            }
        }
        let undo = match op {
            FileOp::Delete {
                paths,
                recycle: true,
            } => Some(put_back(paths, &|p| {
                format!(
                    r"C:\$Recycle.Bin\{}",
                    p.file_name().unwrap().to_string_lossy()
                )
            })),
            FileOp::Delete { recycle: false, .. } => None,
            FileOp::Move { sources, dest } => Some(put_back(sources, &|p| {
                dest.join(p.file_name().unwrap())
                    .to_string_lossy()
                    .into_owned()
            })),
            FileOp::Rename { path, new_name } => Some(put_back(std::slice::from_ref(path), &|p| {
                p.with_file_name(new_name).to_string_lossy().into_owned()
            })),
            FileOp::RenameMany { items } => {
                let paths: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
                Some(put_back(&paths, &|p| {
                    let name = &items.iter().find(|(q, _)| q == p).unwrap().1;
                    p.with_file_name(name).to_string_lossy().into_owned()
                }))
            }
            FileOp::Copy { sources, dest } => Some(Undo::Remove(
                sources
                    .iter()
                    .map(|p| dest.join(p.file_name().unwrap()))
                    .collect(),
            )),
            FileOp::NewFolder { parent, name } | FileOp::NewFile { parent, name } => {
                Some(Undo::Remove(vec![parent.join(name)]))
            }
        };
        (self.file_op(op, owner), undo)
    }

    fn net_connect(&self, request: &crate::shell::NetConnect) -> Result<(), ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "net_connect called on the UI thread"
        );
        self.net_connects.lock().push(request.clone());
        // Fake server: rejects the password "wrong".
        match &request.credentials {
            Some((_, password)) if password == "wrong" => {
                Err(crate::win_net::NetError::BadLogin.into())
            }
            _ => Ok(()),
        }
    }

    fn has_recycle_bin(&self, path: &Path) -> bool {
        !self.no_bin.lock().iter().any(|p| p == path)
    }

    fn net_disconnect(&self, drive: &Path) -> Result<(), ShellError> {
        self.net_disconnects.lock().push(drive.to_path_buf());
        Ok(())
    }

    fn drag_out(
        &self,
        paths: &[PathBuf],
        image: Option<&crate::shell::DragImage>,
    ) -> Result<crate::shell::DragOutEnd, ShellError> {
        self.dragged_out.lock().push(paths.to_vec());
        // Fake image: width holds the item count, pixels hold the path.
        self.dragged_images.lock().push(image.map(|i| {
            (
                PathBuf::from(String::from_utf8_lossy(&i.bgra).into_owned()),
                i.width as usize,
            )
        }));
        Ok(if self.drag_returns.load(Ordering::Relaxed) {
            crate::shell::DragOutEnd::Returned
        } else {
            crate::shell::DragOutEnd::Finished
        })
    }

    fn drag_image(
        &self,
        base: Option<arke_core::thumbs::Thumbnail>,
        request: &crate::shell::DragImageRequest,
    ) -> Option<crate::shell::DragImage> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "drag_image called on the UI thread"
        );
        self.drag_image_requests
            .lock()
            .push((request.clone(), base.is_some()));
        Some(crate::shell::DragImage {
            width: request.count as u32,
            height: 1,
            bgra: request.path.to_string_lossy().as_bytes().to_vec(),
        })
    }

    fn undo(
        &self,
        undo: &crate::shell::Undo,
        _owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "undo called on the UI thread"
        );
        self.undone.lock().push(undo.clone());
        Ok(OpOutcome::Done)
    }

    fn recycle_bin(&self) -> Result<Vec<crate::shell::RecycledItem>, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "recycle bin read on the UI thread"
        );
        Ok(self.recycled.lock().clone())
    }

    fn recycle_restore(
        &self,
        items: &[crate::shell::RecycledItem],
        _owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "recycle_restore called on the UI thread"
        );
        self.recycled.lock().retain(|kept| !items.contains(kept));
        self.undone.lock().push(crate::shell::Undo::PutBack(
            items
                .iter()
                .map(|item| crate::shell::PutBack {
                    now: item.now.clone(),
                    parent: item.from.clone(),
                    name: item.name.clone(),
                })
                .collect(),
        ));
        Ok(OpOutcome::Done)
    }

    fn recycle_purge(
        &self,
        items: &[crate::shell::RecycledItem],
        _owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "recycle_purge called on the UI thread"
        );
        self.recycled.lock().retain(|kept| !items.contains(kept));
        self.purged
            .lock()
            .extend(items.iter().map(|item| item.name.clone()));
        Ok(OpOutcome::Done)
    }

    fn set_clipboard_files(&self, files: &ClipboardFiles) -> Result<u32, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "clipboard opened on the UI thread"
        );
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        *self.clipboard.lock() = Some((files.clone(), seq));
        Ok(seq)
    }

    fn clipboard_files(&self) -> Result<Option<(ClipboardFiles, u32)>, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "clipboard opened on the UI thread"
        );
        Ok(self.clipboard.lock().clone())
    }

    fn clear_clipboard_if(&self, seq: u32) {
        let mut clip = self.clipboard.lock();
        if clip.as_ref().is_some_and(|(_, s)| *s == seq) {
            *clip = None;
            drop(clip);
            // Bumped outside the lock, like `set_clipboard_files`.
            self.seq.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn clipboard_seq(&self) -> u32 {
        self.seq.load(Ordering::SeqCst)
    }

    fn clipboard_has_files(&self) -> bool {
        self.clipboard.lock().is_some()
    }

    fn is_local_disk(&self, path: &Path) -> bool {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "is_local_disk called on the UI thread"
        );
        path.starts_with(r"C:\")
    }

    fn shell_menu(
        &self,
        paths: &[PathBuf],
        background: bool,
    ) -> Result<crate::shell::ShellMenu, ShellError> {
        use crate::shell::ShellMenuEntry as E;
        assert!(
            !arke_core::fs::is_ui_thread(),
            "shell menu read on the UI thread"
        );
        if background {
            self.background_menus.lock().push(paths.to_vec());
        } else {
            self.system_menus.lock().push(paths.to_vec());
        }
        let verb_item = |id, label: &str, verb: Option<&str>| E::Item {
            id,
            verb: verb.map(Into::into),
            label: label.into(),
            enabled: true,
            checked: false,
            default: false,
            icon: None,
        };
        let item = |id, label: &str| verb_item(id, label, None);
        Ok(crate::shell::ShellMenu {
            session: 7 + self
                .shell_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            // "Open" and "Copy" duplicate Arke's own items (matched by verb), so they're dropped
            // along with
            // the extra separator.
            entries: vec![
                verb_item(4, "Open", Some("open")),
                E::Separator,
                item(1, "Compress with 7-Zip"),
                E::Separator,
                E::Submenu {
                    label: "Send to".into(),
                    enabled: true,
                    icon: None,
                    children: vec![item(2, "Desktop"), item(3, "Mail recipient")],
                },
                E::Separator,
                verb_item(5, "Copy", Some("Copy")),
            ],
        })
    }

    fn invoke_shell_menu(
        &self,
        session: u64,
        id: u32,
        _client_point: (i32, i32),
        _owner: Option<isize>,
    ) -> Result<(), ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "shell menu command run on the UI thread"
        );
        self.shell_invoked.lock().push((session, id));
        Ok(())
    }

    fn release_shell_menu(&self, session: u64) {
        self.shell_released.lock().push(session);
    }

    fn run_in(&self, command_line: &str, dir: &Path) -> Result<(), ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "run_in called on the UI thread"
        );
        self.ran
            .lock()
            .push((command_line.to_string(), dir.to_path_buf()));
        Ok(())
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "clipboard opened on the UI thread"
        );
        *self.clipboard_text.lock() = Some(text.to_owned());
        Ok(())
    }

    fn disk_spaces(&self, roots: &[PathBuf]) -> Vec<(PathBuf, (u64, u64))> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "disk space queried on the UI thread"
        );
        self.disk_free
            .lock()
            .iter()
            .filter(|(root, _)| roots.contains(root))
            .cloned()
            .collect()
    }

    fn shortcut_target(&self, path: &Path) -> Option<(PathBuf, bool)> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "shortcut target read on the UI thread"
        );
        self.shortcuts
            .lock()
            .iter()
            .find(|(link, _, _)| link == path)
            .map(|(_, target, dir)| (target.clone(), *dir))
    }

    fn create_shortcuts(
        &self,
        sources: &[PathBuf],
        dest: &Path,
    ) -> Result<Vec<PathBuf>, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "shortcuts created on the UI thread"
        );
        self.shortcuts_made
            .lock()
            .push((sources.to_vec(), dest.to_path_buf()));
        Ok(Vec::new())
    }

    fn set_cloud_pin(
        &self,
        paths: &[PathBuf],
        pin: crate::shell::CloudPin,
    ) -> Result<(), ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "cloud pin changed on the UI thread"
        );
        self.cloud_pins.lock().push((paths.to_vec(), pin));
        Ok(())
    }

    fn reveal_in_explorer(&self, path: &Path, select: bool) -> Result<(), ShellError> {
        assert!(!arke_core::fs::is_ui_thread());
        self.revealed.lock().push((path.to_path_buf(), select));
        Ok(())
    }

    fn decode_ansi(&self, bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Every fake PDF has 5 pages; the page is a gray image fitting the box.
    fn render_pdf(
        &self,
        path: &Path,
        page: u32,
        max: (u32, u32),
    ) -> Result<crate::shell::PdfRender, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "render_pdf called on the UI thread"
        );
        self.pdf_renders
            .lock()
            .push((path.to_path_buf(), page, max));
        let (width, height) = (max.0.clamp(1, 40), max.1.clamp(1, 40));
        Ok(crate::shell::PdfRender {
            pages: 5,
            image: arke_core::thumbs::Thumbnail {
                width,
                height,
                rgba: vec![200; (width * height * 4) as usize],
            },
        })
    }

    fn net_probe(&self) -> Option<Arc<dyn arke_core::net::NetProbe>> {
        self.net_probe.lock().clone()
    }

    fn cloud_scan(&self, dir: &Path, items: &[Arc<str>]) -> crate::shell::CloudScan {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "cloud_scan called on the UI thread"
        );
        self.cloud_scans
            .lock()
            .push((dir.to_path_buf(), items.to_vec()));
        self.cloud.lock().clone()
    }

    /// Returns the whole fake list, ignoring `limit`, so the page's own cap is tested too.
    fn recent_files(&self, limit: usize) -> Vec<crate::home::RecentFile> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "recent files read on the UI thread"
        );
        *self.recent_limit.lock() = Some(limit);
        self.recent.lock().clone()
    }

    fn pick_file(
        &self,
        request: &crate::shell::PickRequest,
        _owner: Option<isize>,
    ) -> Result<Option<PathBuf>, ShellError> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "file dialog shown on the UI thread"
        );
        self.pick_requests.lock().push(request.clone());
        let mut picks = self.picks.lock();
        Ok(if picks.is_empty() {
            None
        } else {
            picks.remove(0)
        })
    }

    fn file_icon(&self, path: &Path, size: u32) -> Option<arke_core::thumbs::Thumbnail> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "file_icon called on the UI thread"
        );
        self.icons.lock().push(path.to_path_buf());
        Some(arke_core::thumbs::Thumbnail {
            width: size,
            height: size,
            rgba: vec![255; (size * size * 4) as usize],
        })
    }
}

fn file(name: &str, size: u64) -> Entry {
    Entry::new(name, EntryKind::File, size, None, 0)
}

fn dir(name: &str) -> Entry {
    Entry::new(name, EntryKind::Dir, 0, None, 0)
}

fn setup<'a>(
    cx: &'a mut TestAppContext,
    fs: Arc<FakeFs>,
    shell: Arc<FakeShell>,
    start: &str,
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    arke_core::fs::mark_ui_thread();
    // The locale is process-wide. Tests run in English, the default, so expected text built
    // with t!() before this call still matches.
    rust_i18n::set_locale("en");
    // Background results wake the foreground task through channels, so the scheduler may park.
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
        crate::palette::bind_keys(cx);
    });
    let services = Services {
        fs,
        shell,
        remotes: crate::remote_shell::test_remotes(),
    };
    let start = PathBuf::from(start);
    cx.add_window_view(move |window, cx| {
        Explorer::new(services, start, None, Instant::now(), window, cx)
    })
}

fn wait_until(
    cx: &mut VisualTestContext,
    view: &Entity<Explorer>,
    what: &str,
    pred: impl Fn(&Explorer) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if view.update(cx, |v, _| pred(v)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn loaded(path: &str) -> impl Fn(&Explorer) -> bool + '_ {
    move |v| {
        v.current_path() == Some(Path::new(path)) && v.model.as_ref().is_some_and(|m| m.complete)
    }
}

fn names(v: &Explorer) -> Vec<String> {
    v.model
        .as_ref()
        .map(|m| m.entries.iter().map(|e| e.name.to_string()).collect())
        .unwrap_or_default()
}

fn selected_name(v: &Explorer) -> Option<String> {
    let view = v.view()?;
    v.cursor
        .and_then(|ix| view.get(ix))
        .map(|e| e.name.to_string())
}

/// Stands in for a placeholder value so a translated message can be cut to its fixed prefix.
const MARK: &str = "\u{1}";

/// The part of `message` before `MARK`.
fn before_mark(message: &str) -> String {
    message.split(MARK).next().unwrap_or(message).to_owned()
}

/// A permanently hung network share never blocks the UI: you can navigate elsewhere right away,
/// and the stale request returning later doesn't overwrite the current view.
#[gpui::test]
fn hung_network_share_never_blocks_ui(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\home", 3);
    fs.insert_many(r"C:\other", 5);
    fs.insert_many(r"\\dead\share", 10);
    fs.hang(r"\\dead\share");
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\home");
    wait_until(cx, &view, "load C:\\home", loaded(r"C:\home"));

    let t = Instant::now();
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"\\dead\share"), NavKind::New, cx);
    });
    cx.run_until_parked();
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "navigation blocked the UI"
    );
    view.update(cx, |v, _| {
        assert!(
            !v.show_loading,
            "loading state should not show within 150 ms"
        );
    });
    // Advance the fake clock to fire the loading timer.
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert_eq!(v.display_path(), Some(Path::new(r"\\dead\share")));
        assert!(v.show_loading, "loading state should show");
        assert_eq!(v.current_path(), Some(Path::new(r"C:\home")));
    });

    // While hung, navigate elsewhere via the path bar using the keyboard.
    cx.dispatch_action(FocusPathBar);
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.editing_path));
    cx.simulate_input(r"C:\other");
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "load C:\\other", loaded(r"C:\other"));
    view.update(cx, |v, _| {
        assert_eq!(names(v).len(), 5);
        assert!(!v.show_loading);
        assert!(!v.editing_path);
    });

    // Release the hung share: the result is stale and must be dropped.
    fs.release_all();
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert_eq!(v.current_path(), Some(Path::new(r"C:\other")));
        assert_eq!(v.history.back_target(), Some(Path::new(r"C:\home")));
    });
}

#[gpui::test]
fn keyboard_navigation_typeahead_and_history(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\root",
        vec![
            dir("docs"),
            file("alpha.txt", 1),
            file("beta.txt", 1),
            file("bravo.txt", 1),
        ],
    );
    fs.insert_dir(r"C:\root\docs", vec![file("inner.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\root");
    wait_until(cx, &view, "load C:\\root", loaded(r"C:\root"));
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        ["docs", "alpha.txt", "beta.txt", "bravo.txt"]
    );

    cx.simulate_keystrokes("down");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("docs")
    );

    // Type to jump to a matching file.
    cx.simulate_keystrokes("b r");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("bravo.txt")
    );

    cx.simulate_keystrokes("home enter");
    wait_until(cx, &view, "enter docs", loaded(r"C:\root\docs"));

    // Backspace goes up and selects the folder we came from.
    cx.simulate_keystrokes("backspace");
    wait_until(cx, &view, "back to root", loaded(r"C:\root"));
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("docs")
    );

    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &view, "back to docs", loaded(r"C:\root\docs"));
    cx.simulate_keystrokes("alt-right");
    wait_until(cx, &view, "forward to root", loaded(r"C:\root"));
}

#[gpui::test]
fn sorting_happens_in_background_and_keeps_selection(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\s",
        vec![file("a.txt", 30), file("b.txt", 10), file("c.txt", 20)],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\s");
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.simulate_keystrokes("a");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("a.txt")
    );

    let spec = SortSpec::by(SortColumn::Size, true, false);
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "sort by size", |v| {
        v.model.as_ref().is_some_and(|m| m.sorted_by == spec)
    });
    view.update(cx, |v, _| {
        assert_eq!(names(v), ["b.txt", "c.txt", "a.txt"]);
        assert_eq!(selected_name(v).as_deref(), Some("a.txt"));
    });
}

#[gpui::test]
fn watcher_changes_update_list_incrementally(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\w");
    fs.insert_dir(dir_path, vec![file("a.txt", 1), file("c.txt", 1)]);
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\w");
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs.watcher_count(dir_path) == 0 {
        assert!(Instant::now() < deadline, "watcher did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let reads_before = fs.read_dir_calls();

    fs.add_file(dir_path, "b.txt", 1);
    fs.remove(dir_path, "c.txt");
    fs.emit_changed(dir_path, &["b.txt", "c.txt"]);
    wait_until(cx, &view, "incremental update", |v| {
        names(v) == ["a.txt", "b.txt"]
    });
    assert_eq!(
        fs.read_dir_calls(),
        reads_before,
        "a small change should not relist the folder"
    );
}

#[gpui::test]
fn failed_navigation_keeps_current_directory(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\home", 2);
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\home");
    wait_until(cx, &view, "load", loaded(r"C:\home"));

    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\missing"), NavKind::New, cx);
    });
    wait_until(cx, &view, "error shown", |v| v.error.is_some());
    wait_until(cx, &view, "back in the original folder", loaded(r"C:\home"));
    view.update(cx, |v, _| {
        assert_eq!(names(v).len(), 2);
        assert!(
            !v.history.can_go_back(),
            "failed navigation should not enter history"
        );
    });
    // The watcher should restart.
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs.watcher_count(Path::new(r"C:\home")) == 0 {
        assert!(Instant::now() < deadline, "watcher did not restart");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[gpui::test]
fn opening_files_and_loading_places_run_off_ui_thread(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\home", vec![file("readme.md", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\home");
    wait_until(cx, &view, "load", loaded(r"C:\home"));
    wait_until(cx, &view, "sidebar", |v| !v.places.is_empty());

    cx.simulate_keystrokes("down enter");
    let deadline = Instant::now() + Duration::from_secs(20);
    while shell.opened.lock().is_empty() {
        assert!(Instant::now() < deadline, "file was not opened");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        shell.opened.lock().as_slice(),
        [PathBuf::from(r"C:\home\readme.md")]
    );
}

#[gpui::test]
fn path_bar_expands_environment_variables(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\home", 1);
    fs.insert_many(r"C:\home\Desktop", 4);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\");
    cx.dispatch_action(FocusPathBar);
    cx.simulate_input(r"%HOME%\Desktop");
    cx.simulate_keystrokes("enter");
    wait_until(
        cx,
        &view,
        "path with environment variable",
        loaded(r"C:\home\Desktop"),
    );
}

/// 100k items: the list only builds visible rows.
#[gpui::test]
fn large_directory_loads(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\big", 100_000);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\big");
    wait_until(cx, &view, "load 100k items", loaded(r"C:\big"));
    assert_eq!(view.update(cx, |v, _| names(v).len()), 100_000);
    cx.simulate_keystrokes("end");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("file_099999.txt")
    );
}

/// Regression: CJK file names (sharing a UTF-8 lead byte) used to make sorting panic.
#[gpui::test]
fn chinese_file_names_sort_without_panic(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\zh",
        vec![
            file("報表_10.xlsx", 1),
            file("報告_2.docx", 1),
            file("報告_10.docx", 1),
            file("照片.png", 1),
            dir("專案"),
        ],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\zh");
    wait_until(cx, &view, "load", loaded(r"C:\zh"));
    assert_eq!(
        view.update(cx, |v, _| names(v)),
        [
            "專案",
            "報告_2.docx",
            "報告_10.docx",
            "報表_10.xlsx",
            "照片.png"
        ]
    );
}

fn selected_names(v: &Explorer) -> Vec<String> {
    v.view()
        .map(|view| {
            view.iter()
                .filter(|e| v.selection.contains(&e.name))
                .map(|e| e.name.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn view_names(v: &Explorer) -> Vec<String> {
    v.view()
        .map(|view| view.iter().map(|e| e.name.to_string()).collect())
        .unwrap_or_default()
}

#[gpui::test]
fn multi_select_survives_sort_and_watcher(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\m");
    fs.insert_dir(
        dir_path,
        vec![
            file("a.txt", 40),
            file("b.txt", 30),
            file("c.txt", 20),
            file("d.txt", 10),
        ],
    );
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));

    cx.simulate_keystrokes("down shift-down shift-down");
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["a.txt", "b.txt", "c.txt"]
    );
    // Ctrl+Space deselects c under the cursor.
    cx.simulate_keystrokes("ctrl-space");
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["a.txt", "b.txt"]
    );

    // Selection survives sorting by size (tracked by name).
    let spec = SortSpec::by(SortColumn::Size, true, false);
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "sort by size", |v| {
        v.model.as_ref().is_some_and(|m| m.sorted_by == spec)
    });
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["b.txt", "a.txt"]
    );

    // Deleted items leave the selection.
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs.watcher_count(dir_path) == 0 {
        assert!(Instant::now() < deadline, "watcher did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    fs.remove(dir_path, "a.txt");
    fs.emit_changed(dir_path, &["a.txt"]);
    wait_until(cx, &view, "update after delete", |v| names(v).len() == 3);
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 1);

    cx.simulate_keystrokes("ctrl-a");
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 3);
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
}

#[gpui::test]
fn live_filter_small_and_large_directories(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\f",
        vec![
            dir("docs"),
            file("alpha.txt", 1),
            file("beta.txt", 1),
            file("bravo.txt", 1),
        ],
    );
    fs.insert_many(r"C:\big", 100_000);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\f");
    wait_until(cx, &view, "load", loaded(r"C:\f"));

    cx.dispatch_action(FocusFilter);
    cx.simulate_input("bra");
    assert_eq!(view.update(cx, |v, _| view_names(v)), ["bravo.txt"]);
    // Enter opens the first match.
    cx.simulate_keystrokes("enter");
    let deadline = Instant::now() + Duration::from_secs(20);
    while shell.opened.lock().is_empty() {
        assert!(Instant::now() < deadline, "file was not opened");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        shell.opened.lock().as_slice(),
        [PathBuf::from(r"C:\f\bravo.txt")]
    );
    // Esc clears the filter and restores the full list.
    cx.simulate_keystrokes("escape");
    assert_eq!(view.update(cx, |v, _| view_names(v)).len(), 4);

    // 100k items: filtering runs in the background and the UI returns immediately.
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\big"), NavKind::New, cx);
    });
    wait_until(cx, &view, "load 100k items", loaded(r"C:\big"));
    cx.dispatch_action(FocusFilter);
    let t = Instant::now();
    cx.simulate_input("099999");
    assert!(t.elapsed() < Duration::from_millis(500));
    wait_until(cx, &view, "background filter", |v| {
        view_names(v) == ["file_099999.txt"]
    });
    // Down moves focus to the list and selects a result.
    cx.simulate_keystrokes("down");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("file_099999.txt")
    );
}

/// Filtering drops the items it hides from the selection, so the selection bar counts only what's
/// on screen and Delete can never reach an item the user can't see.
#[gpui::test]
fn delete_after_filtering_never_hits_hidden_items(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\s");
    fs.insert_dir(
        dir_path,
        vec![
            file("alpha.txt", 1),
            file("beta.txt", 1),
            file("gamma.txt", 1),
        ],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\s");
    wait_until(cx, &view, "load", loaded(r"C:\s"));

    // beta selected, then a filter that hides it: nothing is selected any more and Delete does
    // nothing.
    cx.simulate_keystrokes("down down");
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["beta.txt"]);
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("gamma");
    view.update(cx, |v, _| {
        assert_eq!(view_names(v), ["gamma.txt"]);
        assert!(v.selection.is_empty(), "hidden beta still selected");
        assert_eq!(v.selection.cursor(), None, "cursor still on hidden beta");
    });
    cx.dispatch_action(DeleteFiles);
    cx.run_until_parked();
    assert!(shell.ops().is_empty(), "deleted a hidden item");

    // Clearing the filter doesn't bring the dropped selection back.
    cx.simulate_keystrokes("escape");
    view.update(cx, |v, _| {
        assert_eq!(view_names(v).len(), 3);
        assert!(v.selection.is_empty());
    });

    // alpha and beta selected, filter keeps only alpha: Delete takes alpha alone.
    cx.simulate_keystrokes("home shift-down");
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["alpha.txt", "beta.txt"]
    );
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("alp");
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 1);
    cx.dispatch_action(DeleteFiles);
    wait_until(cx, &view, "delete runs", |_| !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::Delete {
            paths: vec![PathBuf::from(r"C:\s\alpha.txt")],
            recycle: true,
        }]
    );
}

/// Progress center jobs: title and whether finished.
fn finished_jobs(cx: &mut VisualTestContext) -> Vec<(String, bool)> {
    cx.update(|_, cx| {
        crate::op_center::OpCenter::jobs(cx)
            .iter()
            .map(|j| {
                let done = matches!(j.state, crate::op_center::JobState::Done { .. });
                (j.title(), done)
            })
            .collect()
    })
}

fn final_hits(v: &Explorer) -> Vec<String> {
    v.search_results
        .as_ref()
        .filter(|r| !r.searching)
        .map(|r| r.hits.iter().map(|h| h.rel.to_string()).collect())
        .unwrap_or_default()
}

#[gpui::test]
fn search_subfolders_and_open_location(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\proj",
        vec![dir("docs"), dir("src"), file("notes.txt", 1)],
    );
    fs.insert_dir(r"C:\proj\docs", vec![file("inner_report.md", 1)]);
    fs.insert_dir(r"C:\proj\src", vec![file("main.rs", 1), dir("deep")]);
    fs.insert_dir(r"C:\proj\src\deep", vec![file("report_gen.rs", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\proj");
    wait_until(cx, &view, "load", loaded(r"C:\proj"));

    cx.dispatch_action(FocusSearch);
    cx.simulate_input("report");
    wait_until(cx, &view, "search results", |v| final_hits(v).len() == 2);
    let found = view.update(cx, |v, _| final_hits(v));
    assert!(
        found.contains(&r"docs\inner_report.md".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&r"src\deep\report_gen.rs".to_string()),
        "{found:?}"
    );

    // Changing only the query doesn't rewalk.
    cx.simulate_input("_g");
    wait_until(cx, &view, "narrowed query", |v| {
        final_hits(v) == [r"src\deep\report_gen.rs"]
    });

    // Down into the list, then Shift+Enter: go to the containing folder and select the item.
    cx.simulate_keystrokes("down shift-enter");
    wait_until(
        cx,
        &view,
        "open containing folder",
        loaded(r"C:\proj\src\deep"),
    );
    view.update(cx, |v, _| {
        assert!(!v.search_mode, "changing folders should leave search");
        assert!(v.query.is_empty());
        assert_eq!(selected_name(v).as_deref(), Some("report_gen.rs"));
    });
}

/// With a permanently hung subfolder, search still streams the results found so far and the UI
/// stays responsive.
#[gpui::test]
fn search_with_hung_subfolder_streams_partial_results(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\h", vec![dir("mnt"), file("target_here.txt", 1)]);
    fs.insert_dir(r"C:\h\mnt", vec![file("target_never.txt", 1)]);
    fs.hang(r"C:\h\mnt");
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\h");
    wait_until(cx, &view, "load", loaded(r"C:\h"));
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("target");
    wait_until(cx, &view, "partial results", |v| {
        v.search_results
            .as_ref()
            .is_some_and(|r| r.searching && r.hits.len() == 1)
    });
    // Can leave immediately while still searching.
    cx.simulate_keystrokes("escape");
    view.update(cx, |v, _| {
        assert!(!v.search_mode);
        assert_eq!(view_names(v).len(), 2);
    });
    fs.release_all();
}

/// Filter chips write the box text (and read it back); "Contents" searches this folder's files in
/// the background and in subfolders through the search service, with the matching line.
#[gpui::test]
fn filter_chips_and_content_search(cx: &mut TestAppContext) {
    use arke_core::index_query::Facet;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\c",
        vec![
            dir("sub"),
            file("a.jpg", 1),
            file("memo.txt", 1),
            file("todo.txt", 1),
        ],
    );
    fs.insert_dir(r"C:\c\sub", vec![file("deep.md", 1)]);
    fs.set_content(r"C:\c\memo.txt", b"pay the Invoice\n".to_vec());
    fs.set_content(r"C:\c\todo.txt", b"nothing\n".to_vec());
    fs.set_content(r"C:\c\sub\deep.md", b"# notes\nold invoice 7\n".to_vec());
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\c");
    wait_until(cx, &view, "load", loaded(r"C:\c"));
    cx.dispatch_action(FocusFilter);
    let chip = |cx: &mut VisualTestContext, facet: Facet, term: Option<&str>| {
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_facet(facet, term, window, cx));
        });
    };
    chip(cx, Facet::Type, Some("type:image"));
    assert_eq!(view.update(cx, |v, _| v.query.clone()), "type:image");
    wait_until(cx, &view, "type filter", |v| view_names(v) == ["a.jpg"]);
    chip(cx, Facet::Type, None);
    assert!(
        view.update(cx, |v, _| v.query.is_empty()),
        "removing the chip removes the term"
    );

    cx.simulate_input("invoice");
    cx.update(|window, cx| view.update(cx, |v, cx| v.toggle_contents(window, cx)));
    assert_eq!(
        view.update(cx, |v, _| v.query.clone()),
        r#"content:"invoice""#
    );
    wait_until(cx, &view, "content matches in this folder", |v| {
        view_names(v) == ["memo.txt"] && !v.content_searching()
    });

    // Subfolders: the service reads the files and keeps the line.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.set_scope(crate::explorer::scope::Scope::Subfolders, cx);
            v.keep_filter_focus(window, cx);
        });
    });
    wait_until(cx, &view, "content matches in subfolders", |v| {
        final_hits(v).len() == 2
    });
    let lines: Vec<String> = view.update(cx, |v, _| {
        let mut lines: Vec<String> = v
            .search_results
            .as_ref()
            .map(|r| {
                r.hits
                    .iter()
                    .filter_map(|h| h.snippet.as_deref().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        lines.sort();
        lines
    });
    assert_eq!(lines, ["old invoice 7", "pay the Invoice"]);
}

/// Content search reads the text inside Word documents and PDFs, in this folder and subfolders.
#[gpui::test]
fn content_search_finds_text_inside_documents(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\c",
        vec![
            dir("sub"),
            file("letter.docx", 1),
            file("other.docx", 1),
            file("notes.txt", 1),
        ],
    );
    fs.insert_dir(r"C:\c\sub", vec![file("scan.pdf", 1)]);
    let docx = |text: &str| {
        arke_core::office::fixtures::docx(&format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"))
    };
    fs.set_content(r"C:\c\letter.docx", docx("Please pay Invoice 12"));
    fs.set_content(r"C:\c\other.docx", docx("nothing to see"));
    fs.set_content(
        r"C:\c
otes.txt",
        b"nothing".to_vec(),
    );
    fs.set_content(
        r"C:\c\sub\scan.pdf",
        arke_core::pdf_text::fixtures::pdf(&[&["cover page"], &["發票 invoice 13"]], true),
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\c");
    wait_until(cx, &view, "load", loaded(r"C:\c"));
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("invoice");
    cx.update(|window, cx| view.update(cx, |v, cx| v.toggle_contents(window, cx)));
    wait_until(cx, &view, "document matches in this folder", |v| {
        view_names(v) == ["letter.docx"] && !v.content_searching()
    });
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.set_scope(crate::explorer::scope::Scope::Subfolders, cx);
            v.keep_filter_focus(window, cx);
        });
    });
    wait_until(cx, &view, "document matches in subfolders", |v| {
        final_hits(v).len() == 2
    });
    let mut found: Vec<(String, String)> = view.update(cx, |v, _| {
        v.search_results
            .as_ref()
            .map(|r| {
                r.hits
                    .iter()
                    .map(|h| {
                        (
                            h.rel.to_string(),
                            h.snippet.as_deref().unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    found.sort();
    assert_eq!(
        found,
        [
            (
                "letter.docx".to_string(),
                "Please pay Invoice 12".to_string()
            ),
            (r"sub\scan.pdf".to_string(), "發票 invoice 13".to_string()),
        ]
    );
}

fn setup_workspace<'a>(
    cx: &'a mut TestAppContext,
    fs: Arc<FakeFs>,
    start: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), start)
}

fn setup_workspace_with<'a>(
    cx: &'a mut TestAppContext,
    fs: Arc<FakeFs>,
    shell: Arc<FakeShell>,
    start: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let services = Services {
        fs,
        shell,
        remotes: crate::remote_shell::test_remotes(),
    };
    setup_workspace_services(cx, services, PathBuf::from(start))
}

fn setup_workspace_services(
    cx: &mut TestAppContext,
    services: Services,
    start: PathBuf,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    arke_core::fs::mark_ui_thread();
    // The locale is process-wide. Tests run in English, the default, so expected text built
    // with t!() before this call still matches.
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
        crate::palette::bind_keys(cx);
    });
    cx.add_window_view(move |window, cx| {
        Workspace::new(
            services,
            start,
            false,
            None,
            None,
            Instant::now(),
            window,
            cx,
        )
    })
}

fn tab_paths(ws: &Workspace, cx: &App) -> Vec<Option<PathBuf>> {
    ws.tab_views()
        .map(|t| t.read(cx).display_path().map(Path::to_path_buf))
        .collect()
}

#[gpui::test]
fn restores_last_session_tabs(cx: &mut TestAppContext) {
    use crate::settings::{Session, SessionTab};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 1);
    fs.insert_many(r"C:\t\two", 2);
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
        crate::workspace::bind_keys(cx);
    });
    let session = Session {
        tabs: vec![
            SessionTab {
                panes: vec![PathBuf::from(r"C:\t")],
                active_pane: 0,
                ratio: 0.5,
                pinned: true,
                synced: false,
            },
            SessionTab {
                panes: vec![PathBuf::from(r"C:\t\one"), PathBuf::from(r"C:\t\two")],
                active_pane: 0,
                ratio: 0.3,
                pinned: false,
                synced: true,
            },
        ],
        active: 1,
    };
    let services = Services {
        fs,
        shell: Arc::new(FakeShell::default()),
        remotes: crate::remote_shell::test_remotes(),
    };
    let restore = session.clone();
    let (ws, cx) = cx.add_window_view(move |window, cx| {
        Workspace::new(
            services,
            PathBuf::from(r"C:\t"),
            false,
            Some(restore),
            None,
            Instant::now(),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.active, 1, "last active tab should be restored");
        assert_eq!(
            ws.active_pane(),
            0,
            "active side of the split should be restored"
        );
        let panes: Vec<_> = ws
            .pane_views()
            .iter()
            .map(|v| v.read(cx).display_path().map(Path::to_path_buf))
            .collect();
        assert_eq!(
            panes,
            [
                Some(PathBuf::from(r"C:\t\one")),
                Some(PathBuf::from(r"C:\t\two"))
            ]
        );
        assert_eq!(
            ws.session_snapshot(cx),
            session,
            "snapshot should match the restored session"
        );
    });
}

#[gpui::test]
fn dragging_files_out_of_the_window_hands_them_to_windows(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    // Press on row 2 (b.txt), move a little inside the window to start the drag, then out past the
    // left edge.
    let start = point(bounds.left() + px(60.), bounds.top() + row * 1.5);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(30.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert!(
        shell.dragged_out.lock().is_empty(),
        "still inside the window: Arke's own drag"
    );
    // The drag image is built in the background.
    wait_for_drag_image(cx);
    cx.simulate_mouse_move(
        point(px(-20.), start.y),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert_eq!(
        *shell.dragged_out.lock(),
        [vec![PathBuf::from(r"C:\o\b.txt")]]
    );
    assert_eq!(
        *shell.dragged_images.lock(),
        [Some((PathBuf::from(r"C:\o\b.txt"), 1))],
        "image should show the item under the cursor"
    );
    cx.update(|_, cx| assert!(!cx.has_active_drag(), "Arke's own drag should have ended"));
}

/// Dragging out and back in: Windows drag is cancelled and Arke's own drag resumes. Dropping on a
/// folder still moves (not the copy GPUI always reports), and dragging out again hands off to
/// Windows again.
#[gpui::test]
fn dragging_back_into_the_window_resumes_arkes_own_drag(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\r", vec![dir("sub"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\r\sub", vec![]);
    let shell = Arc::new(FakeShell::default());
    shell
        .drag_returns
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\r");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    // Press on a.txt and drag out of the window.
    let start = point(bounds.left() + px(60.), bounds.top() + row * 1.5);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(30.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.simulate_mouse_move(
        point(px(-20.), start.y),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert_eq!(shell.dragged_out.lock().len(), 1);
    cx.update(|_, cx| {
        assert!(
            cx.has_active_drag(),
            "dragged back in: Arke's own drag should continue"
        );
        assert!(
            cx.has_global::<crate::explorer::PendingDragOut>(),
            "dragging out again should hand off to Windows"
        );
        assert!(!cx.has_global::<crate::explorer::DraggingOutside>());
    });
    // Drop on sub: move (same volume).
    let sub = point(bounds.left() + px(200.), bounds.top() + row * 0.5);
    cx.simulate_mouse_move(sub, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(sub, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    // File operations run on a background thread.
    wait_shell("move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::Move {
            sources: vec![PathBuf::from(r"C:\r\a.txt")],
            dest: PathBuf::from(r"C:\r\sub"),
        }]
    );
}

/// Accent hex field: shows the current color, applies only complete codes, and follows changes
/// made elsewhere. The actual color is process-wide and other tests run concurrently, so only the
/// setting is checked here; conversion is tested in theme.
#[gpui::test]
fn accent_hex_field_applies_complete_codes(cx: &mut TestAppContext) {
    use crate::settings::{Accent, SettingsStore};
    use gpui::Focusable;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    let set = |cx: &mut VisualTestContext, accent: Accent| {
        cx.update(|_, cx| {
            SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.accent = accent, cx));
        });
        cx.run_until_parked();
    };
    let chosen = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.accent)
    };

    set(cx, Accent::Color(0x8e_4ec6));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    let input = ws
        .update(cx, |ws, _| ws.accent_input_for_test())
        .expect("settings page should have a hex field");
    assert_eq!(input.update(cx, |i, _| i.text().to_string()), "#8E4EC6");
    cx.update(|window, cx| {
        let handle = input.focus_handle(cx);
        window.focus(&handle, cx);
    });
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("#0078d");
    cx.run_until_parked();
    assert_eq!(
        chosen(cx),
        Accent::Color(0x8e_4ec6),
        "incomplete hex code should not apply"
    );
    cx.simulate_input("4");
    cx.run_until_parked();
    assert_eq!(chosen(cx), Accent::Color(0x0078d4));

    // Changed elsewhere (swatch) while not typing: the hex field follows.
    cx.simulate_keystrokes("enter");
    set(cx, Accent::Color(0x2e_8b3a));
    assert_eq!(input.update(cx, |i, _| i.text().to_string()), "#2E8B3A");
    set(cx, Accent::Arke);
    assert_eq!(input.update(cx, |i, _| i.text().to_string()), "#C2410C");
}

/// Map network drive: Arke's dialog collects folder, letter and credentials and connects in the
/// background. Failures show in the dialog for editing; it closes only on success. The password
/// never appears in debug output.
#[gpui::test]
fn network_drive_dialog_connects_in_the_background(cx: &mut TestAppContext) {
    use crate::net_dialog::NetDialogKind;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_net_dialog(
                NetDialogKind::Map {
                    letters: vec!['Y', 'Z'],
                },
                None,
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();
    let dialog = ws
        .update(cx, |ws, _| ws.net_dialog_for_test())
        .expect("dialog should be open");
    let wait_for =
        |cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "{what}: error {:?}, requests {:?}",
                    dialog.update(cx, |d, _| d.error_for_test()),
                    shell.net_connects.lock()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };

    // Invalid folder: not submitted, error shown directly.
    dialog.update(cx, |d, cx| d.fill_for_test("nas", "bob", "secret", cx));
    cx.run_until_parked();
    assert!(dialog.update(cx, |d, _| d.error_for_test()).is_some());
    assert!(shell.net_connects.lock().is_empty());

    // Wrong password: reason shown in the dialog, which stays open.
    dialog.update(cx, |d, cx| {
        d.fill_for_test(r"\\nas\media", "bob", "wrong", cx);
    });
    wait_for(cx, "failure not reported", &|cx| {
        dialog.update(cx, |d, _| d.error_for_test()).as_deref() == Some(&*t!("net.bad_login"))
    });
    assert!(ws.update(cx, |ws, _| ws.net_dialog_for_test()).is_some());

    // Correct: connects and the dialog closes.
    dialog.update(cx, |d, cx| {
        d.fill_for_test(r"\\nas\media", "bob", "secret", cx);
    });
    wait_for(cx, "dialog did not close", &|cx| {
        ws.update(cx, |ws, _| ws.net_dialog_for_test()).is_none()
    });
    let requests = shell.net_connects.lock().clone();
    let last = requests.last().unwrap();
    assert_eq!(last.share, PathBuf::from(r"\\nas\media"));
    assert_eq!(
        last.drive,
        Some('Z'),
        "should default to the last drive letter"
    );
    assert_eq!(
        last.credentials,
        Some(("bob".to_owned(), "secret".to_owned()))
    );
    assert!(!last.remember);
    let shown = format!("{last:?}");
    assert!(
        shown.contains("bob") && !shown.contains("secret"),
        "{shown}"
    );
}

/// Recycle bin page lists items newest first. Restore doesn't ask; permanent delete and empty
/// confirm first and cancel does nothing. The list refreshes afterwards, and neither listing nor
/// operations run on the UI thread.
#[gpui::test]
fn recycle_bin_page_restores_and_purges(cx: &mut TestAppContext) {
    use crate::recycle_bin::Job;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let item = |name: &str, secs: u64, size: u64| crate::shell::RecycledItem {
        now: format!(r"C:\$Recycle.Bin\S-1\$R{name}"),
        name: name.to_owned(),
        from: PathBuf::from(r"C:\o"),
        deleted: Some(std::time::UNIX_EPOCH + Duration::from_secs(secs)),
        size,
        is_dir: false,
    };
    *shell.recycled.lock() = vec![
        item("old.txt", 100, 1),
        item("new.txt", 300, 2),
        item("mid.txt", 200, 3),
    ];
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| ws.open_recycle_for_test(window, cx));
    });
    let page = ws
        .update(cx, |ws, _| ws.recycle_for_test())
        .expect("page should be open");
    let names = |cx: &mut VisualTestContext| page.update(cx, |p, _| p.items_for_test());
    let wait_for = |cx: &mut VisualTestContext, what: &str, want: &[&str]| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            let now = names(cx);
            if now == want {
                return;
            }
            assert!(Instant::now() < deadline, "{what}: {now:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    wait_for(cx, "items not listed", &["new.txt", "mid.txt", "old.txt"]);

    // Permanent delete asks first; cancel does nothing.
    page.update(cx, |p, _| p.select_for_test(&["mid.txt"]));
    assert!(page.update(cx, |p, cx| p.press_for_test(Job::Purge, false, cx)));
    page.update(cx, |p, cx| p.answer_for_test(false, cx));
    cx.run_until_parked();
    assert!(shell.purged.lock().is_empty());
    assert_eq!(names(cx).len(), 3);

    // Confirm: only the selected item is deleted.
    assert!(page.update(cx, |p, cx| p.press_for_test(Job::Purge, false, cx)));
    page.update(cx, |p, cx| p.answer_for_test(true, cx));
    wait_for(cx, "item not deleted", &["new.txt", "old.txt"]);
    assert_eq!(*shell.purged.lock(), ["mid.txt"]);

    // Restore doesn't ask: back to the original folder.
    page.update(cx, |p, _| p.select_for_test(&["old.txt"]));
    assert!(!page.update(cx, |p, cx| p.press_for_test(Job::Restore, false, cx)));
    wait_for(cx, "item not restored", &["new.txt"]);
    assert_eq!(
        *shell.undone.lock(),
        [crate::shell::Undo::PutBack(vec![crate::shell::PutBack {
            now: r"C:\$Recycle.Bin\S-1\$Rold.txt".to_owned(),
            parent: PathBuf::from(r"C:\o"),
            name: "old.txt".to_owned(),
        }])]
    );

    // Nothing selected: restore and permanent delete do nothing.
    assert!(!page.update(cx, |p, cx| p.press_for_test(Job::Purge, false, cx)));
    assert!(!page.update(cx, |p, cx| p.press_for_test(Job::Restore, false, cx)));
    cx.run_until_parked();
    assert_eq!(shell.purged.lock().len(), 1);

    // Empty: asks first, then deletes everything.
    assert!(page.update(cx, |p, cx| p.press_for_test(Job::Purge, true, cx)));
    page.update(cx, |p, cx| p.answer_for_test(true, cx));
    wait_for(cx, "bin not emptied", &[]);
    assert_eq!(*shell.purged.lock(), ["mid.txt", "new.txt"]);
}

/// Waits for the background drag image.
fn wait_for_drag_image(cx: &mut VisualTestContext) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        let ready = cx.update(|_, cx| {
            cx.try_global::<crate::explorer::PendingDragOut>()
                .is_some_and(|p| p.image.is_some())
        });
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "drag image was not built");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[gpui::test]
fn dragging_a_selection_out_carries_a_count(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\o",
        vec![file("a.txt", 1), file("b.txt", 1), file("c.txt", 1)],
    );
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\o");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\o"));
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-a");
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    // Select all, then drag from row 2 (b.txt).
    let start = point(bounds.left() + px(60.), bounds.top() + row * 1.5);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(30.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    wait_for_drag_image(cx);
    cx.simulate_mouse_move(
        point(px(-20.), start.y),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert_eq!(shell.dragged_out.lock()[0].len(), 3);
    assert_eq!(
        *shell.dragged_images.lock(),
        [Some((PathBuf::from(r"C:\o\b.txt"), 3))],
        "image of the item under the cursor, count of the whole selection"
    );
    let requests = shell.drag_image_requests.lock().clone();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].0.is_dir);
    assert!(
        !requests[0].1,
        "text files have no thumbnail, so the system icon is used"
    );
}

#[gpui::test]
fn tabs_open_switch_and_close(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 1);
    fs.insert_many(r"C:\t\two", 2);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\t"));

    // Ctrl+T then Enter: the first item opens a new tab on the current folder.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.active, 1);
        assert_eq!(
            tab_paths(ws, cx),
            [Some(PathBuf::from(r"C:\t")), Some(PathBuf::from(r"C:\t"))]
        );
    });

    // Back to the first tab; Ctrl+Enter opens two to its right.
    cx.simulate_keystrokes("ctrl-1");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active), 0);
    cx.simulate_keystrokes("down down");
    cx.dispatch_action(OpenInNewTab);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.active, 1);
        assert_eq!(tab_paths(ws, cx)[1], Some(PathBuf::from(r"C:\t\two")));
        assert_eq!(tab_paths(ws, cx).len(), 3);
    });

    // Middle click (background tab): no switch, no focus steal.
    first.update(cx, |_, cx| {
        cx.emit(ExplorerEvent::OpenInNewTab {
            path: PathBuf::from(r"C:\t\one"),
            activate: false,
        });
    });
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(tab_paths(ws, cx).len(), 4);
        assert_eq!(tab_paths(ws, cx)[1], Some(PathBuf::from(r"C:\t\one")));
        // The new tab is inserted before, shifting the current tab right by one.
        assert_eq!(ws.active, 2);
    });

    cx.dispatch_action(CloseTab);
    cx.dispatch_action(CloseTab);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| assert_eq!(tab_paths(ws, cx).len(), 2));
    cx.dispatch_action(NewTab);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| assert_eq!(tab_paths(ws, cx).len(), 3));
}

fn wait_watcher(fs: &FakeFs, dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs.watcher_count(dir) == 0 {
        assert!(Instant::now() < deadline, "watcher did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_shell(what: &str, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A folder listed a moment ago (a tree folder expanded to create in it) is watched from a
/// thread of its own: give it a moment to start, as Windows' watcher would have, so the change
/// isn't lost. Folders nobody watches just wait a little.
fn settle_watcher(fs: &FakeFs, dir: &Path) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while fs.watcher_count(dir) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Simulates IFileOperation's effect on the file system and sends watcher events.
fn apply_to_fake_fs(fs: &Arc<FakeFs>, shell: &FakeShell) {
    let fs = fs.clone();
    *shell.on_op.lock() = Some(Box::new(move |op| match op {
        FileOp::Delete { paths, .. } => {
            for p in paths {
                let (dir, name) = (
                    p.parent().unwrap(),
                    p.file_name().unwrap().to_str().unwrap(),
                );
                fs.remove(dir, name);
                fs.emit_changed(dir, &[name]);
            }
        }
        FileOp::NewFolder { parent, name } => {
            settle_watcher(&fs, parent);
            fs.add_entry(
                parent,
                Entry::new(name.as_str(), EntryKind::Dir, 0, None, 0),
            );
            fs.emit_changed(parent, &[name.as_str()]);
        }
        FileOp::NewFile { parent, name } => {
            settle_watcher(&fs, parent);
            fs.add_file(parent, name, 0);
            fs.emit_changed(parent, &[name.as_str()]);
        }
        FileOp::Rename { path, new_name } => {
            let dir = path.parent().unwrap();
            let old = path.file_name().unwrap().to_str().unwrap();
            fs.remove(dir, old);
            fs.add_file(dir, new_name, 1);
            fs.emit_changed(dir, &[old, new_name]);
        }
        FileOp::RenameMany { items } => {
            for (path, new_name) in items {
                let dir = path.parent().unwrap();
                let old = path.file_name().unwrap().to_str().unwrap();
                fs.remove(dir, old);
                fs.add_file(dir, new_name, 1);
                fs.emit_changed(dir, &[old, new_name]);
            }
        }
        FileOp::Copy { .. } | FileOp::Move { .. } => {}
    }));
}

#[gpui::test]
fn copy_cut_paste_through_clipboard(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\a",
        vec![file("a.txt", 1), file("b.txt", 1), file("c.txt", 1)],
    );
    fs.insert_dir(r"C:\b", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\a");
    wait_until(cx, &view, "load", loaded(r"C:\a"));

    cx.simulate_keystrokes("down shift-down ctrl-c");
    wait_shell("put on clipboard", || shell.clipboard().is_some());
    assert_eq!(
        shell.clipboard(),
        Some(ClipboardFiles {
            paths: vec![PathBuf::from(r"C:\a\a.txt"), PathBuf::from(r"C:\a\b.txt")],
            cut: false,
        })
    );
    wait_until(cx, &view, "notice", |v| v.notice.is_some());

    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\b"), NavKind::New, cx);
    });
    wait_until(cx, &view, "load b", loaded(r"C:\b"));
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, &view, "paste done", |v| {
        v.ops_running == 0 && !shell.ops().is_empty()
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\a\a.txt"), PathBuf::from(r"C:\a\b.txt")],
            dest: PathBuf::from(r"C:\b"),
        }]
    );
    // Pasted items get selected once the watcher reports them.
    view.update(cx, |v, _| {
        assert!(v.selection.contains("a.txt") && v.selection.contains("b.txt"));
    });

    // Cut: sources drawn translucent; clipboard cleared after paste.
    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &view, "back to a", loaded(r"C:\a"));
    cx.simulate_keystrokes("end ctrl-x");
    wait_shell("cut", || shell.clipboard().is_some_and(|c| c.cut));
    cx.run_until_parked();
    view.update(cx, |v, cx| {
        let names: Vec<Arc<str>> = ["b.txt", "c.txt"].map(Arc::from).to_vec();
        assert_eq!(v.cut_flags(&names, cx), [false, true]);
    });
    cx.simulate_keystrokes("alt-right");
    wait_until(cx, &view, "to b", loaded(r"C:\b"));
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, &view, "move done", |_| shell.ops().len() == 2);
    wait_shell("clipboard cleared", || shell.clipboard().is_none());
    assert_eq!(
        shell.ops()[1],
        FileOp::Move {
            sources: vec![PathBuf::from(r"C:\a\c.txt")],
            dest: PathBuf::from(r"C:\b"),
        }
    );
}

#[gpui::test]
fn delete_recycles_and_selects_next(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\d");
    fs.insert_dir(
        dir_path,
        vec![file("a.txt", 1), file("b.txt", 1), file("c.txt", 1)],
    );
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\d");
    wait_until(cx, &view, "load", loaded(r"C:\d"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down down delete");
    wait_until(cx, &view, "next item selected after delete", |v| {
        names(v) == ["a.txt", "c.txt"] && selected_name(v).as_deref() == Some("c.txt")
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Delete {
            paths: vec![PathBuf::from(r"C:\d\b.txt")],
            recycle: true,
        }]
    );
    // Permanent delete always asks; Esc does nothing.
    cx.simulate_keystrokes("shift-delete");
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("recycle.confirm_purge_title"))
        );
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(shell.ops().len(), 1);
    view.update(cx, |v, _| assert_eq!(names(v), ["a.txt", "c.txt"]));
    cx.simulate_keystrokes("shift-delete enter");
    wait_until(cx, &view, "permanent delete", |v| names(v) == ["a.txt"]);
    assert!(matches!(
        shell.ops()[1],
        FileOp::Delete { recycle: false, .. }
    ));
}

/// A delete where one item is open in another program: the rest go, the toast names the file and
/// the program, only what went can be undone, and Retry (toast and operation center) tries just
/// the failed item again.
#[gpui::test]
fn failed_delete_names_the_program_and_retries_only_what_failed(cx: &mut TestAppContext) {
    use crate::shell::{Holder, PutBack, Undo};
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\fd");
    fs.insert_dir(
        dir_path,
        vec![file("a.txt", 1), file("c.txt", 1), file("locked.docx", 1)],
    );
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let locked = PathBuf::from(r"C:\fd\locked.docx");
    shell.fail_delete.lock().push((
        locked.clone(),
        FailReason::InUse,
        vec![Holder {
            app: "Microsoft Word".into(),
            pid: 4242,
        }],
    ));
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\fd");
    wait_until(cx, &view, "load", loaded(r"C:\fd"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("ctrl-a delete");
    wait_until(cx, &view, "partial delete", |v| {
        v.ops_running == 0 && v.error.is_some() && names(v) == ["locked.docx"]
    });
    let retry = FileOp::Delete {
        paths: vec![locked],
        recycle: true,
    };
    view.update(cx, |v, _| {
        let error = v.error.clone().unwrap();
        let reason = t!(
            "fail.in_use_by",
            name = "locked.docx",
            apps = t!("fail.app_pid", app = "Microsoft Word", pid = 4242)
        );
        assert!(error.contains(&*reason), "{error}");
        assert!(error.contains(&*t!("fail.hint_close")), "{error}");
        assert_eq!(
            v.error_action,
            Some(crate::explorer::ErrorAction::RetryOp(Box::new(
                retry.clone()
            )))
        );
        // The failed item stays selected, so it can be acted on again.
        assert_eq!(selected_names(v), ["locked.docx"]);
    });
    let job = cx.update(|_, cx| {
        let job = crate::op_center::OpCenter::jobs(cx).last().unwrap();
        (
            matches!(job.state, crate::op_center::JobState::Failed(_)),
            job.retry.clone(),
            job.undo.is_some(),
        )
    });
    assert_eq!(
        job,
        (true, Some(retry.clone()), true),
        "failed, retry, undo"
    );

    // Closed in Word: Retry deletes it.
    shell.fail_delete.lock().clear();
    cx.update(|window, cx| view.update(cx, |v, cx| v.run_error_action(window, cx)));
    wait_until(cx, &view, "retried", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    assert_eq!(shell.ops().last(), Some(&retry));
    view.update(cx, |v, _| {
        assert_eq!(v.error, None);
        assert_eq!(v.notice.as_deref(), Some(&*tn!("notice.recycled", 1)));
    });

    // Undo goes back one step at a time: the retried file, then exactly the two that went the
    // first time (not the one that failed).
    cx.simulate_keystrokes("ctrl-z");
    wait_until(cx, &view, "undo retry", |_| shell.undone.lock().len() == 1);
    cx.simulate_keystrokes("ctrl-z");
    wait_until(cx, &view, "undo first", |_| shell.undone.lock().len() == 2);
    let put_back = |name: &str| PutBack {
        now: format!(r"C:\$Recycle.Bin\{name}"),
        parent: PathBuf::from(r"C:\fd"),
        name: name.into(),
    };
    assert_eq!(
        *shell.undone.lock(),
        [
            Undo::PutBack(vec![put_back("locked.docx")]),
            Undo::PutBack(vec![put_back("a.txt"), put_back("c.txt")]),
        ]
    );
}

/// Items on a drive without a Recycle Bin are never deleted silently: Arke asks like a
/// permanent delete and says why; the others still go to the Recycle Bin.
#[gpui::test]
fn deleting_where_there_is_no_recycle_bin_asks_first(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\nb");
    fs.insert_dir(dir_path, vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    shell.no_bin.lock().push(PathBuf::from(r"C:\nb\b.txt"));
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\nb");
    wait_until(cx, &view, "load", loaded(r"C:\nb"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("ctrl-a delete");
    cx.run_until_parked();
    assert!(shell.ops().is_empty(), "nothing happens before confirming");
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("recycle.confirm_purge_title"))
        );
        let body = v.confirm_body().unwrap();
        assert!(
            body.contains(&*t!("confirm.no_bin_one", name = "b.txt")),
            "{body}"
        );
        assert!(body.contains(&*tn!("confirm.no_bin_others", 1)), "{body}");
    });
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "both deleted", |v| {
        v.ops_running == 0 && names(v).is_empty()
    });
    let mut ops = shell.ops();
    ops.sort_by_key(|op| matches!(op, FileOp::Delete { recycle: false, .. }));
    assert_eq!(
        ops,
        [
            FileOp::Delete {
                paths: vec![PathBuf::from(r"C:\nb\a.txt")],
                recycle: true,
            },
            FileOp::Delete {
                paths: vec![PathBuf::from(r"C:\nb\b.txt")],
                recycle: false,
            },
        ]
    );
}

/// The subfolder search doesn't watch for changes: a hit deleted from the results leaves them
/// at once and stays out while the query is refined.
#[gpui::test]
fn deleting_a_search_hit_removes_it_from_the_results(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\sr", vec![dir("docs"), file("report_a.txt", 1)]);
    fs.insert_dir(r"C:\sr\docs", vec![file("report_b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\sr");
    wait_until(cx, &view, "load", loaded(r"C:\sr"));

    cx.dispatch_action(FocusSearch);
    cx.simulate_input("report");
    wait_until(cx, &view, "search results", |v| final_hits(v).len() == 2);
    let first = view.update(cx, |v, _| final_hits(v)[0].clone());
    cx.simulate_keystrokes("down");
    cx.dispatch_action(DeleteFiles);
    wait_until(cx, &view, "hit removed", |v| {
        v.ops_running == 0 && final_hits(v).len() == 1
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Delete {
            paths: vec![Path::new(r"C:\sr").join(&first)],
            recycle: true,
        }]
    );
    let left = view.update(cx, |v, _| final_hits(v));
    assert!(!left.contains(&first), "{left:?}");
    // Refining the query reuses the walk; the deleted hit doesn't come back.
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("_");
    wait_until(cx, &view, "requery", |v| final_hits(v).len() == 1);
    assert_eq!(view.update(cx, |v, _| final_hits(v)), left);
}

#[gpui::test]
fn ctrl_z_undoes_the_last_file_op_but_not_in_text_boxes(cx: &mut TestAppContext) {
    use crate::shell::{PutBack, Undo};
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\u");
    fs.insert_dir(dir_path, vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\u");
    wait_until(cx, &view, "load", loaded(r"C:\u"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down down delete");
    wait_until(cx, &view, "delete", |v| {
        names(v) == ["a.txt"] && v.ops_running == 0
    });

    // Ctrl+Z in the filter box belongs to the input and doesn't undo file operations.
    cx.simulate_keystrokes("ctrl-f ctrl-z");
    cx.run_until_parked();
    assert!(shell.undone.lock().is_empty());

    cx.simulate_keystrokes("escape ctrl-z");
    wait_until(cx, &view, "undo", |v| {
        v.ops_running == 0 && v.notice.is_some()
    });
    assert_eq!(
        *shell.undone.lock(),
        [Undo::PutBack(vec![PutBack {
            now: r"C:\$Recycle.Bin\b.txt".into(),
            parent: PathBuf::from(r"C:\u"),
            name: "b.txt".into(),
        }])]
    );
    view.update(cx, |v, _| {
        let undone = tn!("undo.done", 1, op = t!("op.recycle"));
        assert_eq!(v.notice.as_deref(), Some(&*undone));
    });
    // Undo is listed in the progress center too.
    let undo = t!("op.undo");
    let jobs = finished_jobs(cx);
    assert!(
        jobs.iter()
            .any(|(title, done)| title.contains(&*undo) && title.contains("b.txt") && *done),
        "{jobs:?}"
    );

    // Nothing left to undo.
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(shell.undone.lock().len(), 1);
    view.update(cx, |v, _| {
        assert_eq!(v.notice.as_deref(), Some(&*t!("undo.nothing")));
    });
}

/// Recycling says where the items went, with an Undo that undoes exactly that operation even
/// after something newer happened. The progress center says the same and offers Undo too.
#[gpui::test]
fn recycling_offers_undo_on_the_notice_and_in_the_progress_center(cx: &mut TestAppContext) {
    use crate::shell::{PutBack, Undo};
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\rb");
    fs.insert_dir(dir_path, vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\rb");
    wait_until(cx, &view, "load", loaded(r"C:\rb"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("down down delete");
    wait_until(cx, &view, "recycle", |v| {
        names(v) == ["a.txt"] && v.ops_running == 0 && v.notice.is_some()
    });
    let undo_id = view.update(cx, |v, _| {
        assert_eq!(v.notice.as_deref(), Some(&*tn!("notice.recycled", 1)));
        let (text, id) = v.notice_undo.clone().expect("the notice offers Undo");
        assert_eq!(Some(text), v.notice.clone());
        id
    });
    // Listed as moved to the Recycle Bin, not deleted, with the same undo.
    let job = cx.update(|_, cx| {
        crate::op_center::OpCenter::jobs(cx)
            .iter()
            .map(|j| (j.title(), j.undo))
            .next_back()
            .unwrap()
    });
    assert_eq!(
        job,
        (
            t!("ops.recycle_one", name = "b.txt").into_owned(),
            Some(undo_id)
        )
    );

    // Something newer happens: the notice's Undo still puts back the recycled file.
    cx.simulate_keystrokes("f2");
    cx.simulate_input("c");
    cx.simulate_keystrokes("enter");
    wait_shell("rename", || shell.ops().len() == 2);
    wait_until(cx, &view, "rename recorded", |v| v.ops_running == 0);
    view.update(cx, |v, cx| v.undo_op(undo_id, cx));
    wait_until(cx, &view, "undo", |v| {
        v.ops_running == 0 && v.notice.as_deref() != Some(&*tn!("notice.recycled", 1))
    });
    assert_eq!(
        *shell.undone.lock(),
        [Undo::PutBack(vec![PutBack {
            now: r"C:\$Recycle.Bin\b.txt".into(),
            parent: PathBuf::from(r"C:\rb"),
            name: "b.txt".into(),
        }])]
    );
    // Undone: no longer offered, and Ctrl+Z now undoes the rename.
    assert!(!cx.update(|_, cx| crate::explorer::UndoHistory::has(cx, undo_id)));
    assert_eq!(
        cx.update(|_, cx| crate::explorer::UndoHistory::last(cx).map(|e| e.verb)),
        Some("op.rename")
    );
}

#[gpui::test]
fn asks_before_recycling_and_before_changing_extensions(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\k");
    fs.insert_dir(dir_path, vec![file("a.txt", 1), file("report.docx", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\k");
    wait_until(cx, &view, "load", loaded(r"C:\k"));
    wait_watcher(&fs, dir_path);

    // Default: recycling doesn't ask.
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.confirm_recycle = true, cx));
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("down delete");
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("confirm.recycle_title"))
        );
    });
    assert!(shell.ops().is_empty(), "nothing deleted before confirming");
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "deleted after confirming", |v| {
        names(v) == ["report.docx"]
    });
    assert!(view.update(cx, |v, _| v.confirm.is_none()));

    // Changing the extension asks first; Esc keeps it.
    // F2 selects only the stem; select all before typing to change the extension.
    cx.simulate_keystrokes("home f2 ctrl-a");
    cx.simulate_input("report.pdf");
    cx.simulate_keystrokes("enter");
    view.update(cx, |v, _| {
        assert_eq!(
            v.confirm_title().as_deref(),
            Some(&*t!("confirm.ext_title"))
        );
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(shell.ops().len(), 1, "cancelled: nothing renamed");
    // Changing only the stem doesn't ask.
    cx.simulate_keystrokes("f2 ctrl-a");
    cx.simulate_input("new-report.docx");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.confirm.is_none()));
    // The rename is sent from a background thread; wait for it to be recorded.
    wait_shell("rename", || {
        shell.ops().last().is_some_and(
            |op| matches!(op, FileOp::Rename { new_name, .. } if new_name == "new-report.docx"),
        )
    });
}

#[gpui::test]
fn inline_rename_validates_names(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\r");
    fs.insert_dir(dir_path, vec![file("report.docx", 1), file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("end f2");
    let renaming = |v: &Explorer| v.renaming.as_ref().map(|r| r.name.to_string());
    assert_eq!(
        view.update(cx, |v, _| renaming(v)).as_deref(),
        Some("report.docx")
    );

    // A bad character is flagged under the box as soon as it's typed, not in a toast.
    let problem = |v: &Explorer| v.renaming.as_ref().and_then(|r| r.problem);
    cx.simulate_input("a|b");
    view.update(cx, |v, _| {
        assert_eq!(problem(v), Some(NameError::BadChars));
        assert!(v.error.is_none(), "no toast");
    });
    // Enter is blocked: still editing, message still there, nothing sent.
    cx.simulate_keystrokes("enter");
    view.update(cx, |v, _| {
        assert!(v.renaming.is_some());
        assert_eq!(problem(v), Some(NameError::BadChars));
        assert!(v.error.is_none(), "no toast");
    });
    assert!(shell.ops().is_empty());
    // Fixing it clears the message.
    cx.simulate_keystrokes("backspace backspace");
    view.update(cx, |v, _| assert_eq!(problem(v), None));
    // A trailing period is normal halfway through typing, so it only counts on Enter.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("draft.");
    view.update(cx, |v, _| assert_eq!(problem(v), None));
    cx.simulate_keystrokes("enter");
    view.update(cx, |v, _| {
        assert!(v.renaming.is_some());
        assert_eq!(problem(v), Some(NameError::Trailing));
    });
    assert!(shell.ops().is_empty());

    // Only the stem is selected by default, so typing replaces the stem and keeps the extension.
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.renaming.is_none()));
    cx.simulate_keystrokes("f2");
    cx.simulate_input("summary");
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "rename done", |v| {
        names(v).contains(&"summary.docx".to_string())
            && selected_name(v).as_deref() == Some("summary.docx")
    });
    assert_eq!(
        shell.ops(),
        [FileOp::Rename {
            path: PathBuf::from(r"C:\r\report.docx"),
            new_name: "summary.docx".into(),
        }]
    );
}

/// The profile folder under This PC is named after the account, so it isn't a second "Home".
#[test]
fn profile_folder_is_named_after_the_account() {
    assert_eq!(
        crate::win::profile_label(Path::new(r"C:\Users\alex.CONTOSO")),
        "alex.CONTOSO"
    );
    assert_eq!(
        crate::win::profile_label(Path::new(r"C:\")),
        t!("place.profile")
    );
}

/// What the rename box says while typing versus on Enter.
#[test]
fn rename_problems_while_typing_and_on_enter() {
    use crate::explorer::{commit_problem, typing_problem};
    // Typing: only what more typing can't fix.
    assert_eq!(typing_problem("a:b"), Some(NameError::BadChars));
    assert_eq!(typing_problem("what?"), Some(NameError::BadChars));
    assert_eq!(typing_problem(&"x".repeat(256)), Some(NameError::TooLong));
    assert_eq!(typing_problem("report."), None, "the extension comes next");
    assert_eq!(typing_problem("my "), None, "the next word comes next");
    assert_eq!(typing_problem("con"), None, "could become \"console\"");
    assert_eq!(typing_problem(""), None);
    // Enter: everything, except what commit drops (outer spaces) or treats as cancel (empty).
    assert_eq!(commit_problem("report."), Some(NameError::Trailing));
    assert_eq!(commit_problem("con"), Some(NameError::Reserved));
    assert_eq!(commit_problem("a:b"), Some(NameError::BadChars));
    assert_eq!(commit_problem("  notes.txt  "), None);
    assert_eq!(commit_problem("   "), None);
    // Shown as the localized message.
    assert_eq!(
        NameError::BadChars.to_string(),
        arke_core::path::validate_file_name("a?")
            .unwrap_err()
            .to_string()
    );
}

#[gpui::test]
fn new_folder_gets_unique_name_and_starts_rename(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\n");
    let new_folder = t!("ops.new_folder_name").into_owned();
    let second = format!("{new_folder} (2)");
    fs.insert_dir(dir_path, vec![dir(&new_folder), file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\n");
    wait_until(cx, &view, "load", loaded(r"C:\n"));
    wait_watcher(&fs, dir_path);

    cx.simulate_keystrokes("ctrl-shift-n");
    wait_until(cx, &view, "new folder appears in rename mode", |v| {
        v.renaming
            .as_ref()
            .is_some_and(|r| &*r.name == second.as_str())
    });
    assert_eq!(
        shell.ops(),
        [FileOp::NewFolder {
            parent: PathBuf::from(r"C:\n"),
            name: second.clone(),
        }]
    );
}

/// Background menu: "New text document" picks a unique name (number before the extension) and
/// starts renaming. "Show more options" reads this folder's background menu.
#[gpui::test]
fn background_menu_new_text_file_and_more_options(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\t");
    let text_doc = t!("ops.new_text_file_name").into_owned();
    let second = format!("{text_doc} (2).txt");
    fs.insert_dir(dir_path, vec![file(&format!("{text_doc}.txt"), 0)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\t");
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    wait_watcher(&fs, dir_path);

    right_click(&view, cx, None);
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(
        &labels[..2],
        [
            t!("menu.new_folder").into_owned(),
            t!("menu.new_text_file").into_owned()
        ]
    );
    let more_options = format!("{} ›", t!("menu.more_options"));
    assert_eq!(labels.last(), Some(&more_options));
    wait_shell("background menu", || {
        !shell.background_menus.lock().is_empty()
    });
    assert_eq!(shell.background_menus.lock()[0], [PathBuf::from(r"C:\t")]);

    run_command(&view, cx, MenuCommand::NewTextFile);
    wait_until(cx, &view, "new file appears in rename mode", |v| {
        v.renaming
            .as_ref()
            .is_some_and(|r| &*r.name == second.as_str())
    });
    assert_eq!(
        shell.ops(),
        [FileOp::NewFile {
            parent: PathBuf::from(r"C:\t"),
            name: second.clone(),
        }]
    );
}

/// Sidebar disk capacity is refreshed after file operations. Network drives are skipped.
#[gpui::test]
fn disk_space_updates_after_file_operations(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\d", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    shell.extra_places.lock().extend([
        Place {
            label: "Local Disk (C:)".into(),
            path: PathBuf::from(r"C:\"),
            kind: PlaceKind::Drive {
                remote: false,
                space: Some((10, 100)),
            },
        },
        Place {
            label: "Network Drive (X:)".into(),
            path: PathBuf::from(r"X:\"),
            kind: PlaceKind::Drive {
                remote: true,
                space: None,
            },
        },
    ]);
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\d");
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\d")(v) && v.places.len() > 2
    });
    shell.disk_free.lock().extend([
        (PathBuf::from(r"C:\"), (60, 100)),
        (PathBuf::from(r"X:\"), (1, 2)),
    ]);
    right_click(&view, cx, None);
    run_command(&view, cx, MenuCommand::NewFolder);
    let space = |v: &Explorer, root: &str| {
        v.places
            .iter()
            .find(|p| p.path == Path::new(root))
            .map(|p| p.kind)
    };
    wait_until(cx, &view, "disk space updated", |v| {
        space(v, r"C:\")
            == Some(PlaceKind::Drive {
                remote: false,
                space: Some((60, 100)),
            })
    });
    assert_eq!(
        view.update(cx, |v, _| space(v, r"X:\")),
        Some(PlaceKind::Drive {
            remote: true,
            space: None
        }),
        "network drives are not queried"
    );
}

/// Shortcuts to folders open inside Arke; shortcuts to files go to the default app. The menu has
/// "Open file location" (select the target in its folder) and "Open with...".
#[gpui::test]
fn shortcuts_to_folders_open_inside_arke(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![file("app.lnk", 1), file("Work.lnk", 1)]);
    fs.insert_dir(r"C:\target", vec![file("doc.txt", 1)]);
    fs.insert_dir(r"C:\apps", vec![file("a.exe", 1), file("tool.exe", 1)]);
    let shell = Arc::new(FakeShell::default());
    shell.shortcuts.lock().extend([
        (
            PathBuf::from(r"C:\s\Work.lnk"),
            PathBuf::from(r"C:\target"),
            true,
        ),
        (
            PathBuf::from(r"C:\s\app.lnk"),
            PathBuf::from(r"C:\apps\tool.exe"),
            false,
        ),
    ]);
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\s");
    wait_until(cx, &view, "load", loaded(r"C:\s"));

    right_click(&view, cx, Some(1));
    run_command(&view, cx, MenuCommand::Open);
    wait_until(cx, &view, "shortcut target folder", loaded(r"C:\target"));
    assert!(
        shell.opened.lock().is_empty(),
        "folder shortcuts should not go to Windows"
    );

    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &view, "back", loaded(r"C:\s"));
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        labels.contains(&t!("menu.open_file_location").into_owned()),
        "{labels:?}"
    );
    assert!(
        labels.contains(&t!("menu.choose_app").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::Open);
    wait_shell("file shortcut opened with the default app", || {
        !shell.opened.lock().is_empty()
    });
    assert_eq!(*shell.opened.lock(), [PathBuf::from(r"C:\s\app.lnk")]);

    right_click(&view, cx, Some(0));
    run_command(&view, cx, MenuCommand::OpenFileLocation);
    wait_until(cx, &view, "folder containing the target", |v| {
        loaded(r"C:\apps")(v) && selected_names(v) == ["tool.exe"]
    });
}

/// In cloud folders like OneDrive the menu offers "Always keep on this device" (checked when all
/// are kept, click again to undo) and "Free up space", run in the background. Not shown outside
/// cloud folders.
#[gpui::test]
fn cloud_folders_offer_keep_and_free_up_space(cx: &mut TestAppContext) {
    use crate::shell::CloudPin;
    const PINNED: u32 = 0x8_0000;
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\od",
        vec![
            file("a.txt", 1),
            Entry::new("b.txt", EntryKind::File, 1, None, PINNED),
        ],
    );
    fs.insert_dir(r"C:\plain", vec![file("c.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(Place {
        label: "OneDrive".into(),
        path: PathBuf::from(r"C:\od"),
        kind: PlaceKind::Cloud,
    });
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\od");
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\od")(v) && v.places.len() > 1
    });

    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        labels.contains(&t!("menu.keep_on_device").into_owned()),
        "{labels:?}"
    );
    assert!(
        labels.contains(&t!("menu.free_up_space").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::FreeUpSpace);
    wait_shell("free up space", || !shell.cloud_pins.lock().is_empty());
    assert_eq!(
        shell.cloud_pins.lock()[0],
        (vec![PathBuf::from(r"C:\od\a.txt")], CloudPin::Free)
    );

    // Already kept: checked, clicking again undoes it.
    right_click(&view, cx, Some(1));
    let checked = view.update(cx, |v, _| v.context_menu.as_ref().unwrap().checked_labels());
    assert_eq!(checked, [t!("menu.keep_on_device").into_owned()]);
    run_command(&view, cx, MenuCommand::KeepOnDevice);
    wait_shell("stop keeping on device", || {
        shell.cloud_pins.lock().len() == 2
    });
    assert_eq!(
        shell.cloud_pins.lock()[1],
        (vec![PathBuf::from(r"C:\od\b.txt")], CloudPin::Default)
    );

    cx.simulate_keystrokes("escape");
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\plain"), NavKind::New, cx);
    });
    wait_until(cx, &view, "plain folder", loaded(r"C:\plain"));
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        !labels.contains(&t!("menu.free_up_space").into_owned()),
        "{labels:?}"
    );
}

/// Pinned Windows commands (including ones from submenus) appear right above "Show more options"
/// and still run in the background. Unpinning removes them from an open menu immediately.
#[gpui::test]
fn pinned_shell_commands_show_in_the_main_menu(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.menu_pinned = vec![
                        "Send to › Desktop".into(),
                        "Missing".into(),
                        "Compress with 7-Zip".into(),
                    ];
                },
                cx,
            );
        });
    });
    right_click(&view, cx, Some(0));
    wait_until(cx, &view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    let more_options = format!("{} ›", t!("menu.more_options"));
    let labels = menu_labels(&view, cx).unwrap();
    let more = labels.iter().position(|l| *l == more_options).unwrap();
    assert_eq!(
        &labels[more - 3..more],
        ["—", "Desktop", "Compress with 7-Zip"],
        "{labels:?}"
    );
    let session = view
        .update(cx, |v, _| v.context_menu.as_ref().unwrap().shell_state().1)
        .unwrap();

    view.update(cx, |v, cx| v.toggle_shell_pin("Send to › Desktop", cx));
    let labels = menu_labels(&view, cx).unwrap();
    let more = labels.iter().position(|l| *l == more_options).unwrap();
    assert_eq!(
        &labels[more - 2..more],
        ["—", "Compress with 7-Zip"],
        "{labels:?}"
    );
    assert_eq!(
        cx.update(|_, cx| crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .menu_pinned
            .clone()),
        ["Missing", "Compress with 7-Zip"]
    );
    // The "Show more options" submenu is still complete and opens.
    cx.simulate_keystrokes("up right");
    assert_eq!(
        view.update(cx, |v, _| v.context_menu.as_ref().map(|m| m.labels_at(1))),
        Some(vec![
            "Compress with 7-Zip".to_owned(),
            "—".to_owned(),
            "Send to ›".to_owned()
        ])
    );
    cx.simulate_keystrokes("left up enter");
    wait_shell("run pinned command", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(session, 1)]);
}

/// A start folder that can't be opened shows an error page instead of a toast. The path bar and
/// tab title keep the location, and going up works.
#[gpui::test]
fn missing_start_folder_shows_error_state(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\home", 1);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\home\missing");
    wait_until(cx, &view, "error page", |v| v.failed.is_some());
    view.update(cx, |v, _| {
        assert!(
            v.error.is_none(),
            "no toast when there is no folder to fall back to"
        );
        assert_eq!(v.display_path(), Some(Path::new(r"C:\home\missing")));
        assert_eq!(v.tab_title().as_ref(), "missing");
    });
    cx.simulate_keystrokes("alt-up");
    wait_until(cx, &view, "up one level", loaded(r"C:\home"));
    assert!(view.update(cx, |v, _| v.failed.is_none()));
}

/// Path bar autocomplete lists matching folders (not files); Down selects, Tab completes, Enter
/// goes.
#[gpui::test]
fn path_bar_suggests_folders(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\p",
        vec![
            dir("Docs"),
            dir("Downloads"),
            dir("music"),
            file("do.txt", 1),
        ],
    );
    fs.insert_many(r"C:\p\Docs", 1);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    cx.simulate_keystrokes("ctrl-l");
    assert!(view.update(cx, |v, _| v.editing_path));
    cx.simulate_input(r"C:\p\do");
    wait_until(cx, &view, "suggestions", |v| {
        v.suggestion_names() == ["Docs", "Downloads"]
    });
    cx.simulate_keystrokes("down tab");
    assert_eq!(view.update(cx, |v, cx| v.path_text(cx)), r"C:\p\Docs\");
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "go", loaded(r"C:\p\Docs"));
    assert!(view.update(cx, |v, _| !v.editing_path));
}

/// Double-clicking empty space goes up a level when the setting is on.
#[gpui::test]
fn double_click_on_empty_space_goes_up_when_enabled(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("child")]);
    fs.insert_many(r"C:\p\child", 2);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\p\child");
    wait_until(cx, &view, "load", loaded(r"C:\p\child"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let blank = point(bounds.left() + px(200.), bounds.top() + row * 6.);
    let double_click = |cx: &mut VisualTestContext| {
        for click_count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: blank,
                modifiers: Modifiers::none(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: blank,
                modifiers: Modifiers::none(),
                click_count,
            });
        }
        cx.run_until_parked();
    };
    double_click(cx);
    assert!(
        view.update(cx, |v, _| loaded(r"C:\p\child")(v)),
        "off by default: should stay"
    );
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.double_click_up = true, cx));
    });
    double_click(cx);
    wait_until(cx, &view, "back up one level", loaded(r"C:\p"));
}

#[gpui::test]
fn folder_sizes_are_computed_in_the_background(cx: &mut TestAppContext) {
    use crate::explorer::FolderSize;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\z", vec![dir("big"), dir("empty"), file("f.txt", 7)]);
    fs.insert_dir(r"C:\z\big", vec![dir("deep"), file("a", 1000)]);
    fs.insert_dir(r"C:\z\big\deep", vec![file("b", 24)]);
    fs.insert_dir(r"C:\z\empty", vec![]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\z");
    wait_until(cx, &view, "load", loaded(r"C:\z"));
    wait_until(cx, &view, "folder sizes", |v| {
        matches!(v.folder_size("big"), FolderSize::Known { fresh: true, .. })
            && matches!(
                v.folder_size("empty"),
                FolderSize::Known { fresh: true, .. }
            )
    });
    view.update(cx, |v, _| {
        assert_eq!(
            v.folder_size("big"),
            FolderSize::Known {
                bytes: 1024,
                fresh: true
            }
        );
        assert_eq!(
            v.folder_size("f.txt"),
            FolderSize::Unknown,
            "files have no folder size"
        );
    });
    // Selected total size includes folders.
    cx.simulate_keystrokes("ctrl-a");
    assert_eq!(view.update(cx, |v, _| v.selected_size()), Some(1031));
}

// One scenario end to end: sizes arrive in the background while the sort is applied.
#[allow(clippy::too_many_lines)]
#[gpui::test]
fn sorting_by_size_uses_folder_sizes(cx: &mut TestAppContext) {
    use crate::explorer::FolderSize;
    let fs = FakeFs::new();
    let root = Path::new(r"C:\q");
    fs.insert_dir(
        root,
        vec![
            dir("a-small"),
            dir("b-big"),
            dir("c-mid"),
            file("f.bin", 500),
        ],
    );
    fs.insert_dir(r"C:\q\a-small", vec![file("x", 10)]);
    fs.insert_dir(r"C:\q\b-big", vec![dir("deep"), file("x", 900)]);
    fs.insert_dir(r"C:\q\b-big\deep", vec![file("y", 100)]);
    fs.insert_dir(r"C:\q\c-mid", vec![file("x", 300)]);
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    let all_sized = |v: &Explorer| {
        ["a-small", "b-big", "c-mid"]
            .iter()
            .all(|n| matches!(v.folder_size(n), FolderSize::Known { fresh: true, .. }))
    };
    wait_until(cx, &view, "folder sizes", all_sized);
    cx.simulate_keystrokes("c");
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("c-mid")
    );

    // Largest first; folders still sort before files.
    let mut spec = SortSpec::by(SortColumn::Size, false, false);
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "sort by size", |v| {
        names(v) == ["b-big", "c-mid", "a-small", "f.bin"]
    });
    assert_eq!(
        view.update(cx, |v, _| selected_name(v)).as_deref(),
        Some("c-mid"),
        "selection should follow the item"
    );
    spec.ascending = true;
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "smallest first", |v| {
        names(v) == ["a-small", "c-mid", "b-big", "f.bin"]
    });
    // Mixed: folders and files compared by size together.
    spec.mixed = true;
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "mixed", |v| {
        names(v) == ["a-small", "c-mid", "f.bin", "b-big"]
    });
    spec.mixed = false;
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "not mixed", |v| {
        names(v) == ["a-small", "c-mid", "b-big", "f.bin"]
    });

    // List changed (one more folder): others stay sorted by size; the new one counts as 0 until
    // computed.
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs.watcher_count(root) == 0 {
        assert!(Instant::now() < deadline, "watcher did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    fs.insert_dir(r"C:\q\d-new", vec![]);
    fs.add_entry(root, dir("d-new"));
    fs.emit_changed(root, &["d-new"]);
    wait_until(
        cx,
        &view,
        "still sorted by size after a folder is added",
        |v| names(v) == ["d-new", "a-small", "c-mid", "b-big", "f.bin"],
    );
    // The new folder gets computed on its own, without F5.
    wait_until(cx, &view, "new folder computed", |v| {
        v.folder_size("d-new")
            == FolderSize::Known {
                bytes: 0,
                fresh: true,
            }
    });
    // Something was added directly in a subfolder (mtime changed): only that one is recomputed.
    let later = std::time::SystemTime::now();
    fs.add_file(Path::new(r"C:\q\a-small"), "more", 5000);
    fs.add_entry(
        root,
        Entry::new("a-small", EntryKind::Dir, 0, Some(later), 0),
    );
    fs.emit_changed(root, &["a-small"]);
    wait_until(cx, &view, "changed folder recomputed", |v| {
        v.folder_size("a-small")
            == FolderSize::Known {
                bytes: 5010,
                fresh: true,
            }
    });
    // F5 recomputes everything; order unchanged.
    cx.simulate_keystrokes("f5");
    wait_until(cx, &view, "after refresh", |v| {
        v.folder_size("d-new")
            == FolderSize::Known {
                bytes: 0,
                fresh: true,
            }
            && all_sized(v)
            // a-small just grew by 5000 bytes, so it sorts last.
            && names(v) == ["d-new", "c-mid", "b-big", "a-small", "f.bin"]
    });
}

#[gpui::test]
fn size_order_follows_the_folder_being_shown(cx: &mut TestAppContext) {
    use crate::explorer::FolderSize;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![dir("a-small"), dir("b-big")]);
    fs.insert_dir(r"C:\q\a-small", vec![file("x", 10)]);
    fs.insert_dir(r"C:\q\b-big", vec![file("x", 1000)]);
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    // The sort has to travel with the tab into the next folders (per-tab views)
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.remember_folder_views = false, cx);
        });
    });
    let spec = SortSpec::by(SortColumn::Size, true, false);
    view.update(cx, |v, cx| v.set_sort(spec, cx));
    wait_until(cx, &view, "sorted by size", |v| {
        !v.folder_sizes_computing() && names(v) == ["a-small", "b-big"]
    });

    // Another folder: sizes from the previous folder don't leak onto same-named subfolders.
    fs.insert_dir(r"C:\r", vec![dir("b-big"), dir("z")]);
    fs.insert_dir(r"C:\r\b-big", vec![file("x", 1)]);
    fs.insert_dir(r"C:\r\z", vec![file("x", 50)]);
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\r"), NavKind::New, cx);
    });
    wait_until(cx, &view, "changed folder", loaded(r"C:\r"));
    wait_until(cx, &view, "sizes in the new folder", |v| {
        v.folder_size("b-big")
            == FolderSize::Known {
                bytes: 1,
                fresh: true,
            }
            && names(v) == ["b-big", "z"]
    });

    // Same for large lists (sorted in the background).
    let many = Path::new(r"C:\many");
    fs.insert_many(many, 1500);
    fs.add_entry(many, dir("zz-small"));
    fs.add_entry(many, dir("zz-large"));
    fs.insert_dir(r"C:\many\zz-small", vec![file("x", 1)]);
    fs.insert_dir(r"C:\many\zz-large", vec![file("x", 9)]);
    view.update(cx, |v, cx| {
        v.navigate(many.to_path_buf(), NavKind::New, cx);
    });
    wait_until(cx, &view, "large list", loaded(r"C:\many"));
    wait_until(cx, &view, "large list sorted by size", |v| {
        names(v)[..2] == ["zz-small", "zz-large"]
    });
}

#[gpui::test]
fn marquee_selects_rows(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_many(r"C:\m", 6);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    assert!(
        bounds.size.height > row * 8.,
        "need empty space below the rows: {bounds:?}"
    );
    let x = bounds.left() + px(200.);
    // Drag up from row 7 (empty space) to the middle of row 3: selects rows 3-6 (indices 2-5).
    let start = point(x, bounds.top() + row * 6.5);
    let end = point(x + px(40.), bounds.top() + row * 2.5);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    let names = view.update(cx, |v, _| selected_names(v));
    assert_eq!(names.len(), 4, "{names:?}");
    // Click on empty space clears the selection.
    cx.simulate_click(start, Modifiers::none());
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
}

/// Details rows end with their last column, like Explorer's: the row (and its selection wash)
/// covers the columns, and the list right of them is blank background, so a click there clears
/// the selection and a right-click opens the folder's menu.
#[gpui::test]
fn details_rows_end_with_their_columns(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_many(r"C:\m", 6);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    cx.simulate_resize(gpui::size(px(1600.), px(800.)));
    cx.run_until_parked();
    let (bounds, row, width) = view.update(cx, |v, _| {
        (
            v.list_bounds_for_test(),
            v.row_height(),
            v.row_width_for_test().expect("Details rows have a width"),
        )
    });
    // Rows start after the list's side padding.
    let end = bounds.left() + px(8.) + width;
    assert!(
        end < bounds.right() - px(100.),
        "a wide window leaves blank space after the columns: {end:?} in {bounds:?}"
    );
    let y = bounds.top() + row * 1.5;
    // Inside the last column: the row.
    cx.simulate_click(point(end - px(6.), y), Modifiers::none());
    let names = view.update(cx, |v, _| selected_names(v));
    assert_eq!(names.len(), 1, "{names:?}");
    // Right of the columns, beside the same row: empty space clears the selection.
    cx.simulate_click(point(end + px(6.), y), Modifiers::none());
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
    // A right-click there opens the folder's menu, with nothing selected.
    cx.simulate_click(point(end - px(6.), y), Modifiers::none());
    let blank = point(end + px(40.), y);
    cx.simulate_mouse_down(blank, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(blank, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
    let labels = menu_labels(&view, cx).expect("a menu opens");
    assert_eq!(labels.first(), Some(&t!("menu.new_folder").into_owned()));
    // A marquee started there selects the rows it reaches into, not the ones it doesn't.
    cx.simulate_keystrokes("escape");
    let from = point(end + px(40.), bounds.top() + row * 0.5);
    let to = point(end - px(20.), bounds.top() + row * 2.5);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    assert_eq!(view.update(cx, |v, _| selected_names(v)).len(), 3);
    let short = point(end + px(10.), bounds.top() + row * 4.5);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(short, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(short, MouseButton::Left, Modifiers::none());
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
}

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn loaded_media(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<(PathBuf, MediaKind)> {
    view.update(cx, |v, cx| v.media.read(cx).loaded())
}

/// Grid view: thumbnails for local images, system icons only for .exe. One hung image doesn't
/// block other thumbnails or input.
#[gpui::test]
fn grid_view_loads_thumbnails_and_icons_without_blocking(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\pics",
        vec![
            file("a.png", 100),
            file("b.png", 100),
            file("stuck.png", 100),
            file("app.exe", 100),
            file("notes.txt", 100),
        ],
    );
    fs.set_content(r"C:\pics\a.png", png_bytes(400, 200));
    fs.set_content(r"C:\pics\b.png", png_bytes(50, 50));
    fs.set_content(r"C:\pics\stuck.png", png_bytes(10, 10));
    fs.hang(r"C:\pics\stuck.png");
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\pics");
    wait_until(cx, &view, "load", loaded(r"C:\pics"));

    cx.simulate_keystrokes("ctrl-shift-2");
    let expected = vec![
        (PathBuf::from(r"C:\pics\a.png"), MediaKind::Thumbnail),
        (PathBuf::from(r"C:\pics\app.exe"), MediaKind::SystemIcon),
        (PathBuf::from(r"C:\pics\b.png"), MediaKind::Thumbnail),
    ];
    let deadline = Instant::now() + Duration::from_secs(20);
    while loaded_media(&view, cx) != expected {
        assert!(
            Instant::now() < deadline,
            "thumbnails not loaded: {:?}",
            loaded_media(&view, cx)
        );
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        shell.icons.lock().as_slice(),
        [PathBuf::from(r"C:\pics\app.exe")],
        "only .exe/.lnk/.ico should get system icons"
    );

    // The hung image is still loading; grid keyboard navigation works as usual.
    view.update(cx, |v, _| assert!(v.grid_cols > 1));
    cx.simulate_keystrokes("home right");
    assert_eq!(view.update(cx, |v, _| v.cursor), Some(1));
    cx.simulate_keystrokes("down");
    assert_eq!(
        view.update(cx, |v, _| v.cursor),
        Some(4),
        "down a row past the end should stop at the last item"
    );
    cx.simulate_keystrokes("ctrl-shift-6 down");
    assert_eq!(
        view.update(cx, |v, _| v.view_mode),
        crate::explorer::ViewMode::Details
    );
    fs.release_all();
}

/// Network locations get no thumbnails and no system icons (local disks only).
#[gpui::test]
fn network_folders_get_no_thumbnails_or_system_icons(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"\\server\share",
        vec![file("a.png", 100), file("app.exe", 100)],
    );
    fs.set_content(r"\\server\share\a.png", png_bytes(10, 10));
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"\\server\share");
    wait_until(cx, &view, "load", loaded(r"\\server\share"));
    cx.simulate_keystrokes("ctrl-shift-2");
    for _ in 0..20 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(loaded_media(&view, cx).is_empty());
    assert!(shell.icons.lock().is_empty());

    // With "thumbnails on network locations" enabled: images are read and exe icons requested.
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.network_thumbnails = true, cx));
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while loaded_media(&view, cx).is_empty() || shell.icons.lock().is_empty() {
        assert!(
            Instant::now() < deadline,
            "no thumbnails or icons after enabling the setting"
        );
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn right_click(view: &Entity<Explorer>, cx: &mut VisualTestContext, row: Option<usize>) {
    let event = gpui::MouseDownEvent {
        button: gpui::MouseButton::Right,
        position: gpui::point(gpui::px(300.), gpui::px(200.)),
        modifiers: gpui::Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    };
    cx.update(|window, cx| {
        view.update(cx, |v, cx| match row {
            // Item menus open on release, so the press can still become a right-button drag.
            Some(ix) => {
                v.on_row_right_down(ix, &event, window, cx);
                v.right_press_released(event.position, window, cx);
            }
            None => v.on_list_right_down(&event, window, cx),
        });
    });
    cx.run_until_parked();
}

// ContextMenu lives in a private explorer submodule, so `labels` can't be named by path.
#[allow(clippy::redundant_closure_for_method_calls)]
fn menu_labels(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Option<Vec<String>> {
    view.update(cx, |v, _| v.context_menu.as_ref().map(|m| m.labels()))
}

fn run_command(view: &Entity<Explorer>, cx: &mut VisualTestContext, command: MenuCommand) {
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.run_menu_command(command, window, cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
#[allow(clippy::too_many_lines, clippy::redundant_closure_for_method_calls)]
fn context_menu_items_selection_and_keyboard(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\c",
        vec![
            dir("docs"),
            file("a.txt", 1),
            file("b.txt", 1),
            file("pack.zip", 1),
        ],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\c");
    wait_until(cx, &view, "load", loaded(r"C:\c"));
    // Order: docs, a.txt, b.txt, pack.zip.

    // Right-click an unselected archive: selects only it, and the menu offers extract.
    right_click(&view, cx, Some(3));
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["pack.zip"]);
    let labels = menu_labels(&view, cx).expect("menu should be open");
    assert!(
        labels.contains(&t!("menu.extract_to", name = "pack").into_owned()),
        "{labels:?}"
    );
    assert!(labels.contains(&t!("menu.extract_here").into_owned()));
    // The bottom is a row of icon buttons (like the Windows 11 menu), with "Show more options"
    // above it.
    let icon_row = format!(
        "[{} {} {} {}]",
        t!("menu.cut"),
        t!("menu.copy"),
        t!("menu.rename"),
        t!("menu.delete")
    );
    assert_eq!(labels.last(), Some(&icon_row));
    assert!(
        labels.contains(&format!("{} ›", t!("menu.more_options"))),
        "{labels:?}"
    );
    assert!(
        labels.contains(&t!("menu.properties").into_owned()),
        "{labels:?}"
    );
    assert!(
        !labels.contains(&t!("menu.open_new_window").into_owned()),
        "files should not offer \"Open in new window\""
    );
    // The menu has focus: Esc closes it.
    cx.simulate_keystrokes("escape");
    assert!(menu_labels(&view, cx).is_none());

    // Right-click inside a multi-selection keeps it.
    cx.simulate_keystrokes("home down shift-down");
    right_click(&view, cx, Some(2));
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["a.txt", "b.txt"]
    );
    let rename = t!("menu.rename").into_owned();
    let rename_enabled = view.update(cx, |v, _| {
        v.context_menu.as_ref().and_then(|m| {
            m.entries.iter().find_map(|e| match e {
                crate::explorer::MenuEntryForTest::Actions(items) => items
                    .iter()
                    .find(|i| i.label == rename.as_str())
                    .map(|i| i.enabled),
                _ => None,
            })
        })
    });
    // Multi-item rename is allowed ("name (1)", "name (2)", ...).
    assert_eq!(rename_enabled, Some(true));

    // Copy as path (quoted, one per line) and "Show more options" (background thread).
    run_command(&view, cx, MenuCommand::CopyPath);
    wait_shell("copy path", || shell.clipboard_text.lock().is_some());
    assert_eq!(
        shell.clipboard_text.lock().as_deref(),
        Some("\"C:\\c\\a.txt\"\r\n\"C:\\c\\b.txt\"")
    );
    // "Show more options": the Windows menu is read in the background on open and placed in the
    // submenu (earlier menus were each read once too).
    right_click(&view, cx, Some(2));
    let shell_state = |v: &Explorer| v.context_menu.as_ref().map(|m| m.shell_state());
    wait_until(cx, &view, "more options loaded", |v| {
        shell_state(v).is_some_and(|(loaded, _)| loaded)
    });
    // Reads from earlier right-clicks run in the background and may be recorded after this one, so
    // don't check only the last entry.
    let wanted = vec![PathBuf::from(r"C:\c\a.txt"), PathBuf::from(r"C:\c\b.txt")];
    assert!(shell.system_menus.lock().contains(&wanted));
    let session = view.update(cx, |v, _| shell_state(v)).unwrap().1.unwrap();
    // The Windows menu is drawn as an Arke menu: arrow keys enter and leave submenus, and the
    // choice
    // is sent back to the background.
    let at = |v: &Explorer, level| {
        v.context_menu
            .as_ref()
            .map(|m| (m.depth(), m.labels_at(level)))
    };
    cx.simulate_keystrokes("up right");
    assert_eq!(
        view.update(cx, |v, _| at(v, 1)),
        Some((
            1,
            vec![
                "Compress with 7-Zip".to_owned(),
                "—".to_owned(),
                "Send to ›".to_owned()
            ]
        ))
    );
    cx.simulate_keystrokes("down right");
    assert_eq!(
        view.update(cx, |v, _| at(v, 2)),
        Some((2, vec!["Desktop".to_owned(), "Mail recipient".to_owned()]))
    );
    cx.simulate_keystrokes("left");
    assert_eq!(view.update(cx, |v, _| at(v, 1)).map(|(d, _)| d), Some(1));
    cx.simulate_keystrokes("right down enter");
    wait_shell("run Windows menu item", || {
        !shell.shell_invoked.lock().is_empty()
    });
    assert_eq!(*shell.shell_invoked.lock(), [(session, 3)]);
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(
        !shell.shell_released.lock().contains(&session),
        "the invoked menu is released by the background thread"
    );
    // Closed without a choice: the background menu is released.
    right_click(&view, cx, Some(2));
    wait_until(cx, &view, "more options loaded", |v| {
        shell_state(v).is_some_and(|(loaded, _)| loaded)
    });
    let session = view.update(cx, |v, _| shell_state(v)).unwrap().1.unwrap();
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    assert!(shell.shell_released.lock().contains(&session));

    // Empty space: selection cleared, folder menu.
    right_click(&view, cx, None);
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
    let labels = menu_labels(&view, cx).unwrap();
    assert_eq!(labels.first(), Some(&t!("menu.new_folder").into_owned()));
    cx.simulate_keystrokes("escape");

    // Keyboard: Shift+F10 opens the menu; Down x6 then Enter runs the sixth enabled item (after
    // "Add to shelf"). Cut, copy, rename and delete are in the icon row, and the keyboard uses
    // their shortcuts.
    *shell.clipboard_text.lock() = None;
    cx.simulate_keystrokes("home down shift-f10");
    assert!(menu_labels(&view, cx).is_some());
    cx.simulate_keystrokes("down down down down down down enter");
    wait_shell("copy path", || shell.clipboard_text.lock().is_some());
    assert_eq!(
        shell.clipboard_text.lock().as_deref(),
        Some("\"C:\\c\\a.txt\"")
    );
    assert!(menu_labels(&view, cx).is_none());
}

/// Extraction writes to a real disk, so the FakeFs folder here is a real temp folder under %TEMP%.
#[gpui::test]
fn extract_zip_from_context_menu(cx: &mut TestAppContext) {
    use std::io::Write as _;
    let sandbox = std::env::temp_dir().join(format!("arke-menu-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).unwrap();
    let zip_path = sandbox.join("album.zip");
    {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("2026/photo.txt", opts).unwrap();
        zip.write_all(b"one").unwrap();
        zip.start_file("readme.txt", opts).unwrap();
        zip.write_all(b"two").unwrap();
        zip.finish().unwrap();
    }
    let fs = FakeFs::new();
    let dir_str = sandbox.to_string_lossy().into_owned();
    fs.insert_dir(&sandbox, vec![file("album.zip", 100)]);
    let (view, cx) = setup(cx, fs, Arc::default(), &dir_str);
    let sb = sandbox.clone();
    wait_until(cx, &view, "load", move |v| {
        v.current_path() == Some(sb.as_path()) && v.model.as_ref().is_some_and(|m| m.complete)
    });

    right_click(&view, cx, Some(0));
    run_command(&view, cx, MenuCommand::ExtractToFolder);
    wait_until(cx, &view, "extract done", |v| {
        v.extract_job.is_none() && v.notice.is_some()
    });
    let notice = view.update(cx, |v, _| v.notice.clone().unwrap());
    assert_eq!(notice, tn!("extract.done", 2));
    // The progress center has a finished extraction.
    let jobs = finished_jobs(cx);
    assert!(
        jobs.iter()
            .any(|(title, done)| title.contains(&*t!("op.extract"))
                && title.contains("album.zip")
                && *done),
        "{jobs:?}"
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.join("album").join("2026").join("photo.txt")).unwrap(),
        "one"
    );
    assert!(
        view.update(cx, |v, _| v.selection.contains("album")),
        "extracted folder should be selected"
    );

    // Extract here again: all files already exist and are skipped, not overwritten.
    std::fs::write(sandbox.join("readme.txt"), "user").unwrap();
    let skipped = before_mark(&t!("extract.skipped_existing_other", count = MARK));
    right_click(&view, cx, Some(0));
    run_command(&view, cx, MenuCommand::ExtractHere);
    wait_until(cx, &view, "extract here", |v| {
        v.extract_job.is_none()
            && v.notice
                .as_deref()
                .is_some_and(|n| n.contains(skipped.as_str()))
    });
    assert_eq!(
        std::fs::read_to_string(sandbox.join("readme.txt")).unwrap(),
        "user"
    );
    let _ = std::fs::remove_dir_all(&sandbox);
}

/// Design system check: outside `theme.rs`, no literal colors or ad hoc radii, shadows, font
/// sizes or Tailwind-style spacing shortcuts. Everything goes through `theme` tokens.
#[test]
fn ui_code_uses_theme_tokens() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        ("rgb(0x", "literal color: use a theme semantic color"),
        ("rgba(0x", "literal color: use a theme semantic color"),
        ("hsla(", "literal color: use a theme semantic color"),
        (".rounded_", "radius: use .rounded(theme::R_*)"),
        (".shadow_sm(", "shadow: use theme::elev_*"),
        (".shadow_md(", "shadow: use theme::elev_*"),
        (".shadow_lg(", "shadow: use theme::elev_*"),
        ("text_size(px(", "font size: use theme::FS_*"),
    ];
    let spacing = [
        "gap_", "px_", "py_", "pl_", "pr_", "pt_", "pb_", "mx_", "my_", "ml_", "mr_", "mt_", "mb_",
        "p_", "m_",
    ];
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let is_rust = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rs"));
        if !is_rust || name == "theme.rs" || name == "tests.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for (no, line) in source.lines().enumerate() {
            for (pattern, why) in forbidden {
                if line.contains(pattern) {
                    problems.push(format!("{name}:{} {why}: {}", no + 1, line.trim()));
                }
            }
            // Shortcuts like `.gap_2()`: an underscore followed by a digit.
            for prefix in spacing {
                let needle = format!(".{prefix}");
                if let Some(at) = line.find(&needle)
                    && line[at + needle.len()..].starts_with(|c: char| c.is_ascii_digit())
                {
                    problems.push(format!(
                        "{name}:{} spacing: use theme::S_*: {}",
                        no + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[gpui::test]
fn ctrl_t_opens_typed_paths_recent_folders_and_splits(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 1);
    fs.insert_many(r"C:\t\two", 2);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\t"));

    // Full path + Enter opens in a new tab.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    cx.simulate_input(r"C:\t\one");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let second = ws.update(cx, |ws, cx| {
        assert!(ws.palette.is_none(), "palette should close after choosing");
        assert_eq!(
            tab_paths(ws, cx),
            [
                Some(PathBuf::from(r"C:\t")),
                Some(PathBuf::from(r"C:\t\one"))
            ]
        );
        ws.active_view().clone()
    });
    wait_until(cx, &second, "load", loaded(r"C:\t\one"));

    // Fuzzy match on recent folders: "t" is an open tab, so Enter switches to it.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        let palette = ws.palette.as_ref().unwrap().0.read(cx);
        // With an empty query the first item is the current folder.
        assert_eq!(palette.row_labels()[0], r"C:\t\one");
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(ws.update(cx, |ws, _| ws.palette.is_none()));

    // Shift+Enter opens in a split.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    cx.simulate_input(r"C:\t\two");
    cx.simulate_keystrokes("shift-enter");
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        let panes = ws.pane_views();
        assert_eq!(panes.len(), 2, "the same tab should become a split");
        assert_eq!(ws.active_pane(), 1);
        assert_eq!(
            panes[1].read(cx).display_path(),
            Some(Path::new(r"C:\t\two"))
        );
        assert_eq!(tab_paths(ws, cx).len(), 2, "no extra tab should open");
    });
}

#[gpui::test]
fn split_panes_copy_to_the_other_side_and_switch_with_tab(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 3);
    fs.insert_many(r"C:\t\two", 1);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\t\one");
    let left = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &left, "load", loaded(r"C:\t\one"));

    // Split: the right side opens the same folder, then goes to two.
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    let right = ws.update(cx, |ws, _| ws.pane_views()[1].clone());
    right.update(cx, |e, cx| {
        e.navigate(PathBuf::from(r"C:\t\two"), NavKind::New, cx);
    });
    wait_until(cx, &right, "load", loaded(r"C:\t\two"));
    // Render once so each pane learns the other side's folder.
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 1);

    // Tab to the left side, select the first item, Alt+C copies to the right.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active_pane()), 0);
    left.update(cx, |e, cx| e.select_index(0, cx));
    cx.simulate_keystrokes("alt-c");
    wait_shell("copy", || !shell.ops().is_empty());
    match &shell.ops()[0] {
        FileOp::Copy { sources, dest } => {
            assert_eq!(dest, Path::new(r"C:\t\two"));
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].parent(), Some(Path::new(r"C:\t\one")));
        }
        other => panic!("expected a copy, got {other:?}"),
    }

    // Unsplit: the other side becomes its own tab instead of being lost.
    left.update(cx, |_, cx| cx.emit(ExplorerEvent::ToggleSplit));
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        assert_eq!(ws.pane_views().len(), 1);
        assert_eq!(
            tab_paths(ws, cx),
            [
                Some(PathBuf::from(r"C:\t\one")),
                Some(PathBuf::from(r"C:\t\two"))
            ]
        );
    });
}

/// i18n checks:
/// 1. No hard-coded UI text (string literals with CJK characters); it belongs in `locales/*.yml`.
///    Developer-facing text (comments, logs, assertions, panic messages) and tests are exempt.
/// 2. Every locale key used in code exists in the yml for every language.
#[test]
#[allow(clippy::too_many_lines)]
fn ui_text_comes_from_locales() {
    use std::collections::{BTreeMap, BTreeSet};
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let is_cjk = |c: char| matches!(c, '\u{3000}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}');
    let dev_only = [
        "log::",
        "debug_assert",
        "assert!",
        "assert_eq!",
        "panic!",
        "expect(",
    ];
    let mut hardcoded = Vec::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut defined: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for krate in ["app", "core"] {
        let dir = crates.join(krate);
        for entry in std::fs::read_dir(dir.join("src")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let is_rust = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rs"));
            if !is_rust || name == "tests.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            let mut in_dev_macro = false;
            // `tn!(` ended the previous line, so the key is this line's first literal.
            let mut counted_key_next = false;
            for (no, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                // A trailing `#[cfg(test)] mod tests` counts as test code.
                if trimmed.starts_with("#[cfg(")
                    && trimmed.contains("test")
                    && lines
                        .get(no + 1)
                        .is_some_and(|l| l.trim_start().starts_with("mod "))
                {
                    break;
                }
                if trimmed.starts_with("//") {
                    continue;
                }
                if dev_only.iter().any(|m| line.contains(m)) {
                    in_dev_macro = !line.trim_end().ends_with(';');
                    continue;
                }
                if in_dev_macro {
                    in_dev_macro = !line.trim_end().ends_with(';');
                    continue;
                }
                let code = line.split(" // ").next().unwrap_or(line);
                // String literals.
                let mut rest = code;
                while let Some(start) = rest.find('"') {
                    let after = &rest[start + 1..];
                    let Some(end) = after.find('"') else { break };
                    let literal = &after[..end];
                    if literal.chars().any(is_cjk) {
                        hardcoded.push(format!("{krate}/{name}:{} {literal}", no + 1));
                    }
                    let key_like = literal.contains('.')
                        && !literal.contains(['/', ' ', '{'])
                        && literal.chars().all(|c| {
                            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
                        });
                    let before = rest[..start].trim_end();
                    // Counted messages (`tn!`) need both forms.
                    let counted = before.ends_with("tn!(")
                        || (counted_key_next && before.is_empty() && rest.len() == code.len());
                    let in_t = before.ends_with("t!(");
                    let dynamic = ["op.", "type.", "place.", "drive."]
                        .iter()
                        .any(|p| literal.starts_with(p));
                    if key_like && counted {
                        used.insert(format!("{literal}_one"));
                        used.insert(format!("{literal}_other"));
                    } else if key_like && (in_t || dynamic) {
                        used.insert(literal.to_owned());
                    }
                    rest = &after[end + 1..];
                }
                counted_key_next = code.trim_end().ends_with("tn!(");
            }
        }
        read_locale_keys(&dir.join("locales"), &mut defined);
    }
    assert!(
        hardcoded.is_empty(),
        "UI text hard-coded in source:\n{}",
        hardcoded.join("\n")
    );
    let languages: BTreeSet<String> = ["en", "zh-TW", "zh-CN"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let missing: Vec<String> = used
        .iter()
        .filter(|k| {
            defined
                .get(*k)
                .is_none_or(|langs| !languages.is_subset(langs))
        })
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "keys missing from the locale files (or missing a language): {missing:?}"
    );
    let incomplete: Vec<&String> = defined
        .iter()
        .filter(|(_, langs)| !languages.is_subset(langs))
        .map(|(k, _)| k)
        .collect();
    assert!(
        incomplete.is_empty(),
        "keys missing a language: {incomplete:?}"
    );
}

/// Reads `locales/*.yml` (_version 2): under each `key:`, one line per language.
fn read_locale_keys(
    dir: &std::path::Path,
    defined: &mut std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with("_version") || line.trim().is_empty() {
                continue;
            }
            if !line.starts_with(' ') {
                current = line.strip_suffix(':').map(str::to_owned);
                if let Some(key) = &current {
                    defined.entry(key.clone()).or_default();
                }
            } else if let (Some(key), Some((lang, _))) = (&current, line.trim().split_once(':')) {
                defined.get_mut(key).unwrap().insert(lang.to_owned());
            }
        }
    }
}

fn settings(cx: &mut VisualTestContext) -> crate::settings::Settings {
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .clone()
    })
}

#[gpui::test]
fn pin_folders_favorites_and_expand_the_tree(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("a"), dir("b"), file("f.txt", 1)]);
    fs.insert_dir(
        r"C:\p\b",
        vec![dir("y"), dir("x"), dir("Z"), file("g.txt", 1)],
    );
    fs.insert_dir(r"C:\p\b\x", vec![dir("deep")]);
    fs.insert_many(r"C:\p\a", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\p");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    wait_until(cx, &view, "sidebar", |v| !v.places.is_empty());
    let b = PathBuf::from(r"C:\p\b");

    // Folder context menu: pin to the sidebar.
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).expect("menu should be open");
    assert!(
        labels.contains(&t!("sidebar.pin").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::TogglePin);
    // Only one "Pin" entry: goes to a free tile (one after the default Desktop), not the list.
    assert!(
        !labels.iter().any(|l| l.to_lowercase().contains("favorite")),
        "no separate \"add to favorites\" item: {labels:?}"
    );
    assert_eq!(
        settings(cx).favorites,
        Some(vec![PathBuf::from(r"C:\home\Desktop"), b.clone()])
    );
    assert!(settings(cx).pinned.is_empty());
    // Already pinned: the menu shows "Unpin", which removes it from the tiles.
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        labels.contains(&t!("sidebar.unpin").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::TogglePin);
    assert_eq!(
        settings(cx).favorites,
        Some(vec![PathBuf::from(r"C:\home\Desktop")])
    );

    // Tree: expanding b lists subfolders (no files) in the background, in case-insensitive natural
    // order. Folders without subfolders are detected up front (no expand chevron).
    ws.update(cx, |ws, cx| ws.toggle_expand_for_test(b.clone(), cx));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if let Some(children) = ws.update(cx, |ws, _| ws.tree_children_for_test(&b)) {
            assert_eq!(children, [b.join("x"), b.join("y"), b.join("Z")]);
            ws.update(cx, |ws, _| {
                assert!(!ws.tree_is_leaf_for_test(&b.join("x")), "x has subfolders");
                assert!(ws.tree_is_leaf_for_test(&b.join("y")));
                assert!(ws.tree_is_leaf_for_test(&b.join("Z")));
            });
            break;
        }
        assert!(Instant::now() < deadline, "subfolders not listed");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Entering a folder records it in Recent (for Ctrl+T).
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\p\a"), NavKind::New, cx);
    });
    wait_until(cx, &view, "load", loaded(r"C:\p\a"));
    assert_eq!(settings(cx).recent.first(), Some(&PathBuf::from(r"C:\p\a")));
}

#[gpui::test]
fn drag_selected_files_into_a_folder(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\d",
        vec![
            dir("target"),
            file("a.txt", 1),
            file("b.txt", 1),
            file("c.txt", 1),
        ],
    );
    fs.insert_many(r"C:\d\target", 1);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\d");
    wait_until(cx, &view, "load", loaded(r"C:\d"));
    // Order: target, a.txt, b.txt, c.txt. Select a and b (the first Down lands on target).
    cx.simulate_keystrokes("down down shift-down");
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["a.txt", "b.txt"]
    );
    // Pressing inside a multi-selection keeps it (ready to drag it all).
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            let down = gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position: gpui::point(gpui::px(10.), gpui::px(10.)),
                modifiers: gpui::Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            v.on_row_mouse_down_for_test(2, &down, window, cx);
        });
    });
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["a.txt", "b.txt"]
    );

    // Dropping on the target folder: same volume, so move.
    let dragged = crate::explorer::DraggedFiles::from_list(view.downgrade(), 2);
    cx.update(|window, cx| {
        crate::explorer::drop_into(&dragged, PathBuf::from(r"C:\d\target"), window, cx);
    });
    wait_shell("move", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops()[0],
        FileOp::Move {
            sources: vec![PathBuf::from(r"C:\d\a.txt"), PathBuf::from(r"C:\d\b.txt")],
            dest: PathBuf::from(r"C:\d\target"),
        }
    );

    // Dropping into the source folder or into itself does nothing.
    let folder = crate::explorer::DraggedFiles::from_list(view.downgrade(), 0);
    cx.update(|window, cx| {
        crate::explorer::drop_into(&dragged, PathBuf::from(r"C:\d"), window, cx);
        crate::explorer::drop_into(&folder, PathBuf::from(r"C:\d\target\sub"), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(shell.ops().len(), 1);
    // No message: the drop target simply doesn't accept (not highlighted).
    assert!(view.update(cx, |v, _| v.notice.clone()).is_none());
    cx.update(|_, cx| {
        use crate::explorer::accepts_drop;
        assert!(!accepts_drop(&folder, Path::new(r"C:\d\target\sub"), cx));
        assert!(!accepts_drop(&dragged, Path::new(r"C:\d"), cx));
        assert!(accepts_drop(&dragged, Path::new(r"C:\d\target"), cx));
    });
}

/// The preview pane shows the selection, not the keyboard cursor: once nothing is selected
/// (Esc here), it goes back to its empty state.
#[gpui::test]
fn preview_pane_empties_when_the_selection_is_cleared(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("a.txt", 5), file("b.txt", 5)]);
    fs.set_content(Path::new(r"C:\p\a.txt"), b"hello".to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "a.txt previewed", |v| {
        v.preview
            .as_ref()
            .is_some_and(|p| p.entry.name.as_ref() == "a.txt")
    });
    cx.simulate_keystrokes("escape");
    wait_until(cx, &view, "preview cleared", |v| {
        v.selection.is_empty() && v.preview.is_none()
    });
    // The cursor is still on a.txt (arrow keys continue from there); selecting it again brings the
    // preview back.
    cx.simulate_keystrokes("ctrl-space");
    wait_until(cx, &view, "a.txt previewed again", |v| {
        v.preview
            .as_ref()
            .is_some_and(|p| p.entry.name.as_ref() == "a.txt")
    });
}

/// Quick Look's buttons: next moves the cursor like ↓ and the card follows; an item without a
/// preview offers Open; Open on a folder enters it and closes Quick Look.
#[gpui::test]
fn quick_look_buttons_move_and_open(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\q",
        vec![dir("sub"), file("a.txt", 5), file("b.bin", 4)],
    );
    fs.insert_dir(r"C:\q\sub", vec![file("inner.txt", 1)]);
    fs.set_content(Path::new(r"C:\q\a.txt"), b"hello".to_vec());
    fs.set_content(Path::new(r"C:\q\b.bin"), vec![0, 1, 2, 3]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    cx.simulate_keystrokes("down space");
    let previewing = |name: &'static str| {
        move |v: &Explorer| {
            v.preview
                .as_ref()
                .is_some_and(|p| p.entry.name.as_ref() == name)
        }
    };
    wait_until(cx, &view, "folder preview", previewing("sub"));
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("{id} should be drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };
    click(cx, "ql-next-item");
    wait_until(cx, &view, "next item", previewing("a.txt"));
    assert!(
        view.update(cx, |v, _| v.quick_look),
        "Quick Look stays open"
    );
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["a.txt"]);
    // Nothing to show: a large icon and an Open button instead of a blank card.
    click(cx, "ql-next-item");
    wait_until(cx, &view, "binary file", |v| {
        v.preview
            .as_ref()
            .is_some_and(|p| matches!(p.body, crate::explorer::PreviewBody::Nothing))
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("ql-empty-open").is_some());
    cx.simulate_keystrokes("up up");
    wait_until(cx, &view, "back on the folder", previewing("sub"));
    click(cx, "ql-open");
    wait_until(cx, &view, "entered", loaded(r"C:\q\sub"));
    assert!(!view.update(cx, |v, _| v.quick_look));
}

#[gpui::test]
fn space_opens_quick_look_with_text_preview(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\q",
        vec![file("a.txt", 11), file("b.bin", 4), file("c.md", 14)],
    );
    fs.set_content(Path::new(r"C:\q\a.txt"), b"hello\nworld".to_vec());
    fs.set_content(Path::new(r"C:\q\b.bin"), vec![0, 1, 2, 3]);
    fs.set_content(Path::new(r"C:\q\c.md"), b"# Title\n- item".to_vec());
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    cx.simulate_keystrokes("down space");
    assert!(view.update(cx, |v, _| v.quick_look));
    wait_until(cx, &view, "text preview", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(body) if body.text().is_some_and(|t| t.contains("world"))
        )
    });
    // Down to the next item: a binary file has nothing to preview.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "binary file", |v| {
        v.preview.as_ref().is_some_and(|p| {
            p.entry.name.as_ref() == "b.bin"
                && matches!(p.body, crate::explorer::PreviewBody::Nothing)
        })
    });
    // Markdown gets its own layout.
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "Markdown preview", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(crate::explorer::PreviewBody::Markdown(t)) if t.contains("# Title")
        )
    });
    cx.simulate_keystrokes("up");
    // Esc closes Quick Look first; the selection stays.
    cx.simulate_keystrokes("escape");
    assert!(!view.update(cx, |v, _| v.quick_look));
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["b.bin"]);
    // During type-to-select, space is part of the name.
    cx.simulate_keystrokes("a space");
    assert!(!view.update(cx, |v, _| v.quick_look));
}

/// Quick Look owns the wheel while open (the list underneath doesn't scroll), and once it's
/// closed, however that happens, a plain click only selects; it never pops up again.
#[gpui::test]
fn quick_look_owns_the_wheel_and_closes_for_good(cx: &mut TestAppContext) {
    use gpui::{Modifiers, ScrollDelta, ScrollEvent, point, px};
    let fs = FakeFs::new();
    fs.insert_many(r"C:\q", 200);
    fs.insert_many(r"C:\other", 3);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    let list = view.update(cx, |v, _| v.list_bounds_for_test());
    let over_list = list.origin + point(px(80.), px(100.));
    let wheel = |cx: &mut VisualTestContext| {
        cx.simulate_event(ScrollEvent {
            position: over_list,
            delta: ScrollDelta::Lines(point(0., -5.)),
            ..ScrollEvent::default()
        });
    };
    let scrolled = |cx: &mut VisualTestContext| view.update(cx, |v, _| v.list_scroll_for_test().y);

    // The wheel scrolls the list normally...
    cx.simulate_keystrokes("down");
    wheel(cx);
    assert!(
        scrolled(cx) < px(0.),
        "the wheel should scroll the plain list"
    );
    // ...but not while Quick Look is open.
    cx.simulate_keystrokes("space");
    assert!(view.update(cx, |v, _| v.quick_look));
    cx.run_until_parked();
    let before = scrolled(cx);
    wheel(cx);
    assert_eq!(scrolled(cx), before, "list scrolled behind Quick Look");

    // Esc closes it; a plain click afterwards only selects.
    cx.simulate_keystrokes("escape");
    assert!(!view.update(cx, |v, _| v.quick_look));
    let row_h = view.update(cx, |v, _| v.row_height());
    let row = |n: f32| list.origin + point(px(80.), row_h * n);
    cx.simulate_click(row(2.5), Modifiers::default());
    assert!(
        !view.update(cx, |v, _| v.quick_look),
        "click reopened Quick Look"
    );

    // Leaving the folder closes it too, rather than hiding it until the next click.
    cx.simulate_keystrokes("space");
    assert!(view.update(cx, |v, _| v.quick_look));
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\other"), NavKind::New, cx);
    });
    wait_until(cx, &view, "other folder", loaded(r"C:\other"));
    assert!(!view.update(cx, |v, _| v.quick_look));
    cx.simulate_click(row(0.5), Modifiers::default());
    cx.run_until_parked();
    assert!(
        !view.update(cx, |v, _| v.quick_look),
        "click reopened Quick Look"
    );

    // So does switching to another window.
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    cx.simulate_keystrokes("space");
    assert!(view.update(cx, |v, _| v.quick_look));
    cx.deactivate_window();
    assert!(!view.update(cx, |v, _| v.quick_look));
}

#[gpui::test]
fn column_view_moves_with_arrows_and_lists_the_parent(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\m",
        vec![
            dir("one"),
            dir("two"),
            file("top.txt", 1),
            file("Zeta.txt", 1),
        ],
    );
    fs.insert_dir(r"C:\m\one", vec![dir("inner"), file("x.txt", 1)]);
    fs.insert_many(r"C:\m\one\inner", 1);
    fs.insert_many(r"C:\m\two", 1);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\m\one");
    wait_until(cx, &view, "load", loaded(r"C:\m\one"));
    cx.dispatch_action(crate::explorer::ViewColumns);
    // The left column lists the parent, sorted like the middle column (folders first,
    // case-insensitive).
    wait_until(cx, &view, "parent column", |v| {
        v.parent_column_names() == ["one", "two", "top.txt", "Zeta.txt"]
    });
    // Selecting a folder lists its contents in the right column (like Finder's next column).
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "right column lists the folder", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(crate::explorer::PreviewBody::Folder { total: 1, .. })
        )
    });
    // Right enters the folder; Left goes back up and selects it.
    cx.simulate_keystrokes("right");
    wait_until(cx, &view, "enter", loaded(r"C:\m\one\inner"));
    cx.simulate_keystrokes("left");
    wait_until(cx, &view, "back up one level", loaded(r"C:\m\one"));
    wait_until(cx, &view, "previous folder selected", |v| {
        selected_names(v) == ["inner"]
    });
}

/// Whole-PC search: network locations added by the user have their own index, and their results
/// merge with local ones. Removing a location drops its results.
#[gpui::test]
fn whole_computer_search_includes_chosen_network_locations(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    use crate::global_search::{GlobalSearch, Roots};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![]);
    fs.insert_dir(r"C:\docs", vec![file("report-local.txt", 1)]);
    fs.insert_dir(r"\nas\share", vec![dir("team")]);
    fs.insert_dir(r"\nas\share\team", vec![file("report-nas.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs.clone(), shell.clone(), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    let global = cx.update(|_, cx| {
        GlobalSearch::init(
            fs.clone(),
            shell.clone(),
            Roots::Fixed(vec![PathBuf::from(r"C:\")]),
            None,
            cx,
        )
    });
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.global_search = true;
                    st.search_network = vec![PathBuf::from(r"\nas\share")];
                },
                cx,
            );
        });
    });
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("report");
    // Into the search panel, End: the last scope (this PC).
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("end");
    view.update(cx, |v, _| assert_eq!(v.scope(), Scope::Computer));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        let mut hits = view.update(cx, |v, _| final_hits(v));
        hits.sort();
        if hits
            == [
                r"C:\docs\report-local.txt",
                r"\nas\share\team\report-nas.txt",
            ]
        {
            break;
        }
        let count = global.update(cx, |g, _| g.total_count());
        assert!(Instant::now() < deadline, "not found: {hits:?} {count:?}");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Removed in settings: network results no longer appear.
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx)
            .update(cx, |s, cx| s.update(|st| st.search_network.clear(), cx));
    });
    assert!(global.update(cx, |g, _| g.net.is_empty()));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        let hits = view.update(cx, |v, _| final_hits(v));
        if hits == [r"C:\docs\report-local.txt"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "network results still shown: {hits:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Search box scope: this folder, subfolders, whole PC (index) from the search panel, keeping the
/// query.
/// Whole-PC results open with Enter in the default app; Shift+Enter opens the containing folder
/// and selects the item. Ctrl+T lists only locations, not files.
#[gpui::test]
fn search_box_scopes_reach_the_whole_computer(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    use crate::global_search::{GlobalSearch, Roots};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![Entry::new("one", EntryKind::Dir, 0, None, 0)]);
    fs.insert_dir(r"C:\t\one", vec![]);
    fs.insert_dir(
        r"C:\deep\reports",
        vec![
            Entry::new("quarterly-report-2026.pdf", EntryKind::File, 1, None, 0),
            Entry::new("other.txt", EntryKind::File, 1, None, 0),
        ],
    );
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs.clone(), shell.clone(), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    let global = cx.update(|_, cx| {
        GlobalSearch::init(
            fs.clone(),
            shell.clone(),
            Roots::Fixed(vec![PathBuf::from(r"C:\")]),
            None,
            cx,
        )
    });
    assert!(!global.update(cx, |g, _| g.enabled()), "off by default");

    // This folder, subfolders, whole PC; the query stays.
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("quarterly");
    view.update(cx, |v, _| assert_eq!(v.scope(), Scope::Here));
    // Tab goes into the search panel, onto the selected scope; Right selects the next one.
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("right");
    wait_until(cx, &view, "nothing found in subfolders", |v| {
        v.scope() == Scope::Subfolders && v.search_results.as_ref().is_some_and(|r| !r.searching)
    });
    view.update(cx, |v, _| assert!(final_hits(v).is_empty()));
    cx.simulate_keystrokes("right");
    view.update(cx, |v, _| {
        assert_eq!(v.scope(), Scope::Computer);
        assert_eq!(v.query, "quarterly");
        assert!(v.search_results.is_none(), "index not enabled yet");
    });
    // Left goes back, Right forward again; typing goes on into the box, Esc closes the panel.
    cx.simulate_keystrokes("left");
    view.update(cx, |v, _| assert_eq!(v.scope(), Scope::Subfolders));
    cx.simulate_keystrokes("right");
    cx.simulate_keystrokes("escape");

    // Enabled: results appear on their own once the index is built (full paths).
    cx.update(|_, cx| GlobalSearch::enable(cx));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        let hits = view.update(cx, |v, _| final_hits(v));
        if hits == [r"C:\deep\reports\quarterly-report-2026.pdf"] {
            break;
        }
        let status = global.update(cx, |g, _| g.status);
        assert!(Instant::now() < deadline, "not found: {hits:?} {status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Enter opens with the default app.
    cx.simulate_keystrokes("enter");
    let file = PathBuf::from(r"C:\deep\reports\quarterly-report-2026.pdf");
    wait_shell("open file", || shell.opened.lock().contains(&file));

    // Down into the list, Shift+Enter: open the containing folder and select the item.
    cx.simulate_keystrokes("down shift-enter");
    wait_until(cx, &view, "file selected", |v| {
        loaded(r"C:\deep\reports")(v) && selected_names(v) == ["quarterly-report-2026.pdf"]
    });
    view.update(cx, |v, _| {
        assert_eq!(
            v.scope(),
            Scope::Here,
            "scope should reset to this folder after changing folders"
        );
        assert!(v.query.is_empty());
    });

    // Ctrl+T doesn't list files.
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    cx.simulate_input("quarterly");
    cx.run_until_parked();
    let labels = ws.update(cx, |ws, cx| {
        ws.palette.as_ref().unwrap().0.read(cx).row_labels()
    });
    assert!(
        !labels.iter().any(|l| l.contains("quarterly-report-2026")),
        "{labels:?}"
    );
}

/// The search panel works from the keyboard: Tab from the box onto the scopes, arrows between
/// rows, Enter opens a filter's menu and picks, Delete removes the filter, Space switches
/// Contents (the box takes focus to type), letters go on into the box, Esc closes the panel.
#[gpui::test]
fn search_panel_keyboard(cx: &mut TestAppContext) {
    use crate::explorer::scope::{PanelItem, Scope};
    use arke_core::index_query::Facet;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\k", vec![file("a.jpg", 1), file("memo.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\k");
    wait_until(cx, &view, "load", loaded(r"C:\k"));
    cx.dispatch_action(FocusFilter);
    cx.run_until_parked();
    let state = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.update(cx, |v, _| v.panel_state_for_test(window)))
    };

    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(state(cx), (true, PanelItem::Scope(Scope::Here), None));
    cx.simulate_keystrokes("down");
    assert_eq!(state(cx), (true, PanelItem::Filter(Facet::Type), None));
    // Enter opens the menu on "Any type"; Images is three rows down.
    cx.simulate_keystrokes("enter");
    assert_eq!(state(cx).2, Some((Facet::Type, 0)));
    cx.simulate_keystrokes("down down down");
    assert_eq!(state(cx).2, Some((Facet::Type, 3)));
    cx.simulate_keystrokes("enter");
    assert_eq!(view.update(cx, |v, _| v.query.clone()), "type:image");
    assert_eq!(
        state(cx),
        (true, PanelItem::Filter(Facet::Type), None),
        "the menu closes, focus stays on its button"
    );
    wait_until(cx, &view, "type filter", |v| view_names(v) == ["a.jpg"]);
    // Right to Clear filters (shown while a filter is set), Delete on a filter removes it.
    cx.simulate_keystrokes("right right right");
    assert_eq!(state(cx).1, PanelItem::ClearFilters);
    cx.simulate_keystrokes("left left left delete");
    assert!(view.update(cx, |v, _| v.query.is_empty()));

    // Tab to Contents, Space: the box takes focus inside the empty quotes.
    cx.simulate_keystrokes("tab");
    assert_eq!(state(cx).1, PanelItem::Contents);
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.query.clone()), r#"content:"""#);
    assert!(!state(cx).0, "focus is back in the box");
    cx.simulate_input("memo");
    assert_eq!(view.update(cx, |v, _| v.query.clone()), r#"content:"memo""#);

    // Letters typed in the panel go on into the box; Esc in the panel closes it.
    cx.simulate_keystrokes("tab");
    assert!(state(cx).0);
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| v.query.clone()),
        r#"content:"memo"x"#
    );
    assert!(!state(cx).0);
    cx.simulate_keystrokes("tab escape");
    cx.run_until_parked();
    assert!(!state(cx).0);
    assert!(
        view.update(cx, |v, _| v.panel_dismissed_for_test()),
        "Esc closes the panel"
    );
}

/// "This PC" puts the copy the user opens first among same-name results, until the history is
/// cleared or turned off.
#[gpui::test]
fn whole_pc_results_put_what_you_open_first(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    use crate::global_search::{GlobalSearch, Roots};
    use crate::search_history::SearchHistory;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![]);
    for dir in [r"C:\a", r"C:\b", r"C:\c\deep\er"] {
        fs.insert_dir(dir, vec![file("README.md", 1)]);
    }
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs.clone(), shell.clone(), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.update(|_, cx| {
        GlobalSearch::init(
            fs.clone(),
            shell.clone(),
            Roots::Fixed(vec![PathBuf::from(r"C:\")]),
            None,
            cx,
        );
        GlobalSearch::enable(cx);
    });
    let deep = r"C:\c\deep\er\README.md";
    let search = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.set_scope(Scope::Computer, cx);
                v.keep_filter_focus(window, cx);
                v.run_global(cx);
            });
        });
    };
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("readme");
    search(cx);
    wait_until(cx, &view, "all three found", |v| final_hits(v).len() == 3);
    assert_ne!(view.update(cx, |v, _| final_hits(v))[0], deep);

    // Opened once: first.
    cx.update(|_, cx| SearchHistory::record(Path::new(deep), false, cx));
    search(cx);
    wait_until(cx, &view, "the opened one first", |v| {
        final_hits(v).first().map(String::as_str) == Some(deep)
    });

    // Off in settings: ignored (and nothing new is recorded).
    let set_history = |cx: &mut VisualTestContext, on: bool| {
        cx.update(|_, cx| {
            crate::settings::SettingsStore::global(cx)
                .update(cx, |s, cx| s.update(|st| st.search_history = on, cx));
        });
    };
    set_history(cx, false);
    search(cx);
    wait_until(cx, &view, "history off", |v| {
        final_hits(v).len() == 3 && final_hits(v)[0] != deep
    });
    set_history(cx, true);
    search(cx);
    wait_until(cx, &view, "history back on", |v| {
        final_hits(v).first().map(String::as_str) == Some(deep)
    });

    // Cleared: back to the plain order.
    cx.update(|_, cx| SearchHistory::global(cx).update(cx, SearchHistory::clear));
    search(cx);
    wait_until(cx, &view, "history cleared", |v| {
        final_hits(v).len() == 3 && final_hits(v)[0] != deep
    });
    assert_eq!(
        cx.update(|_, cx| SearchHistory::global(cx).read(cx).len()),
        0
    );
}

/// When nothing in this folder matches, the empty state offers wider scopes; the query is kept.
#[gpui::test]
fn no_filter_match_offers_wider_scopes(cx: &mut TestAppContext) {
    use crate::explorer::scope::Scope;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\w", vec![dir("sub"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\w\sub", vec![file("needle.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\w");
    wait_until(cx, &view, "load", loaded(r"C:\w"));
    cx.dispatch_action(FocusFilter);
    cx.simulate_input("needle");
    wait_until(cx, &view, "no filter match", |v| view_names(v).is_empty());
    // This folder: offers "search subfolders" and "search whole PC"; pressing one switches scope.
    view.update(cx, |v, cx| {
        assert!(v.widen_buttons(cx).is_some());
        v.set_scope(Scope::Subfolders, cx);
    });
    wait_until(cx, &view, "found in subfolders", |v| {
        v.scope() == Scope::Subfolders && final_hits(v) == [r"sub\needle.txt"]
    });
    view.update(cx, |v, _| assert_eq!(v.query, "needle"));
}

/// Settings backup: export to Downloads (a temp folder here); importing after changes restores
/// the exported state and can be undone. JSON that isn't Arke settings isn't applied.
#[gpui::test]
fn settings_export_import_and_undo(cx: &mut TestAppContext) {
    use crate::settings::{Density, SettingsStore};
    let sandbox = std::env::temp_dir().join(format!("arke-backup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).unwrap();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\b", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    shell.extra_places.lock().push(Place {
        label: "Downloads".into(),
        path: sandbox.clone(),
        kind: PlaceKind::Folder(KnownFolder::Downloads),
    });
    let (ws, cx) = setup_workspace_with(cx, fs, shell, r"C:\b");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", |v| {
        loaded(r"C:\b")(v) && v.places.len() > 1
    });
    let density = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| SettingsStore::global(cx).read(cx).settings.density)
    };
    let set_density = |cx: &mut VisualTestContext, d: Density| {
        cx.update(|_, cx| {
            SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.density = d, cx));
        });
    };
    let note = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.update(cx, |ws, _| ws.backup_note_for_test())
    };
    let wait_note = |ws: &Entity<Workspace>,
                     cx: &mut VisualTestContext,
                     what: &str,
                     pred: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if note(ws, cx).as_deref().is_some_and(pred) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for: {what} {:?}",
                note(ws, cx)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    let exported_prefix = before_mark(&t!("backup.exported", path = MARK));
    let imported_prefix = before_mark(&t!("backup.imported", name = MARK));
    set_density(cx, Density::Compact);
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, Workspace::export_settings_for_test);
    wait_note(&ws, cx, "export", &|n| {
        n.starts_with(exported_prefix.as_str())
    });
    let exported: Vec<PathBuf> = std::fs::read_dir(&sandbox)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(exported.len(), 1, "{exported:?}");
    let file_name = exported[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        file_name.starts_with(&*t!("backup.file_stem"))
            && Path::new(&file_name)
                .extension()
                .is_some_and(|e| e == "json"),
        "{file_name}"
    );

    // Change, then import: back to the exported state.
    set_density(cx, Density::Comfortable);
    ws.update(cx, |ws, cx| {
        ws.import_settings_for_test(exported[0].clone(), cx);
    });
    wait_note(&ws, cx, "import", &|n| {
        n.starts_with(imported_prefix.as_str())
    });
    assert_eq!(density(cx), Density::Compact);

    // Undo restores the pre-import state.
    ws.update(cx, Workspace::undo_import_for_test);
    cx.run_until_parked();
    assert_eq!(density(cx), Density::Comfortable);

    // Other JSON isn't applied.
    let other = sandbox.join("package.json");
    std::fs::write(&other, br#"{"name": "x"}"#).unwrap();
    ws.update(cx, |ws, cx| ws.import_settings_for_test(other, cx));
    wait_note(&ws, cx, "rejected", &|n| {
        n.contains(&*t!("backup.not_arke"))
    });
    assert_eq!(density(cx), Density::Comfortable);
    let _ = std::fs::remove_dir_all(&sandbox);
}

/// Items disabled under "Context menu" settings are hidden along with adjacent separators; basic
/// items stay.
#[gpui::test]
fn hidden_menu_items_leave_the_context_menu(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("docs"), file("pack.zip", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    let new_tab = t!("menu.open_in_new_tab").into_owned();
    let compress = before_mark(&t!("menu.compress", name = MARK));
    let more_options = t!("menu.more_options").into_owned();
    right_click(&view, cx, Some(0));
    let before = menu_labels(&view, cx).unwrap();
    assert!(before.contains(&new_tab), "{before:?}");
    assert!(
        before.iter().any(|l| l.starts_with(compress.as_str())),
        "{before:?}"
    );
    assert!(
        before.iter().any(|l| l.starts_with(more_options.as_str())),
        "{before:?}"
    );
    cx.simulate_keystrokes("escape");

    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.menu_hidden =
                        vec!["new_tab".into(), "archive".into(), "more_options".into()];
                },
                cx,
            );
        });
    });
    right_click(&view, cx, Some(0));
    let after = menu_labels(&view, cx).unwrap();
    assert!(!after.contains(&new_tab), "{after:?}");
    assert!(
        !after.iter().any(|l| l.starts_with(compress.as_str())),
        "{after:?}"
    );
    assert!(
        !after.iter().any(|l| l.starts_with(more_options.as_str())),
        "{after:?}"
    );
    assert!(after.iter().any(|l| *l == t!("menu.open")), "{after:?}");
    // Extract also falls under "compress and extract".
    cx.simulate_keystrokes("escape");
    right_click(&view, cx, Some(1));
    let zip = menu_labels(&view, cx).unwrap();
    let extract = [
        t!("menu.extract_here").into_owned(),
        t!("menu.extract_to", name = "pack").into_owned(),
    ];
    assert!(!zip.iter().any(|l| extract.contains(l)), "{zip:?}");
}

/// Checkboxes: clicking a box toggles that item without Ctrl; clicking elsewhere on the row selects
/// only it. The header box selects all, and clicking again clears.
#[gpui::test]
fn item_checkboxes_add_to_the_selection(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    use gpui::{Modifiers, point, px};
    let fs = FakeFs::new();
    fs.insert_many(r"C:\k", 5);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\k");
    wait_until(cx, &view, "load", loaded(r"C:\k"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.show_checkboxes = true, cx));
    });
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    // The box is at the far left: list padding 8 + row padding 12 + half the box 8.
    let box_x = bounds.left() + px(28.);
    let at_row = |r: f32| point(box_x, bounds.top() + row * (r + 0.5));
    cx.simulate_click(at_row(1.), Modifiers::none());
    cx.simulate_click(at_row(3.), Modifiers::none());
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)).len(),
        2,
        "both boxes should be checked"
    );
    // Click again to deselect.
    cx.simulate_click(at_row(3.), Modifiers::none());
    assert_eq!(view.update(cx, |v, _| selected_names(v)).len(), 1);
    // Clicking the row name selects only that item.
    cx.simulate_click(
        point(bounds.left() + px(200.), bounds.top() + row * 4.5),
        Modifiers::none(),
    );
    assert_eq!(view.update(cx, |v, _| selected_names(v)).len(), 1);
    // Header box: select all, click again to clear.
    let header = point(box_x, bounds.top() - crate::theme::H_HEADER / 2.);
    cx.simulate_click(header, Modifiers::none());
    assert_eq!(view.update(cx, |v, _| selected_names(v)).len(), 5);
    cx.simulate_click(header, Modifiers::none());
    assert!(view.update(cx, |v, _| v.selection.is_empty()));
}

/// Grouped details: sections by sort column; headers take rows but aren't items. Clicking a header
/// selects its section, and the keyboard steps item by item, skipping headers.
#[gpui::test]
fn grouped_details_have_clickable_headers(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    use gpui::{Modifiers, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\g",
        vec![
            dir("docs"),
            file("apple.txt", 1),
            file("avocado.txt", 1),
            file("banana.txt", 1),
        ],
    );
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\g");
    wait_until(cx, &view, "load", loaded(r"C:\g"));
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.group_rows = true, cx));
    });
    cx.run_until_parked();
    let rows = view.update(cx, |v, _| v.groups().map(|g| g.rows.len()));
    assert_eq!(rows, Some(7), "4 items plus 3 headers (folders, A, B)");
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    // Row 3 is the "A" header (folder header, docs, A header).
    cx.simulate_click(
        point(bounds.left() + px(200.), bounds.top() + row * 2.5),
        Modifiers::none(),
    );
    assert_eq!(
        view.update(cx, |v, _| selected_names(v)),
        ["apple.txt", "avocado.txt"]
    );
    // Keyboard: down from avocado is banana, skipping B's header.
    cx.update(|_, cx| view.update(cx, |v, cx| v.select_index(2, cx)));
    cx.simulate_keystrokes("down");
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["banana.txt"]);
}

/// Hovering dragged files over another tab switches to it after a pause; leaving early cancels.
#[gpui::test]
fn dragging_files_onto_a_tab_switches_to_it_after_a_pause(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![dir("one"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\s\one", vec![]);
    let (ws, cx) = setup_workspace_with(cx, fs, Arc::new(FakeShell::default()), r"C:\s");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\s"));
    first.update(cx, |_, cx| {
        cx.emit(ExplorerEvent::OpenInNewTab {
            path: PathBuf::from(r"C:\s\one"),
            activate: false,
        });
    });
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active), 0);
    // Start dragging a.txt.
    let (bounds, row) = first.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let start = point(bounds.left() + px(60.), bounds.top() + row * 1.5);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(30.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.update(|_, cx| assert!(cx.has_active_drag()));

    // Hovering the second tab: no switch before the delay.
    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.spring_hover_for_test(1, true, window, cx)));
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active), 0);
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    assert_eq!(
        ws.update(cx, |ws, _| ws.active),
        1,
        "should switch after a pause"
    );

    // Passing over the first tab and leaving right away: no switch.
    cx.update(|window, cx| {
        ws.update(cx, |ws, cx| {
            ws.spring_hover_for_test(0, true, window, cx);
            ws.spring_hover_for_test(0, false, window, cx);
        });
    });
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(ws.update(cx, |ws, _| ws.active), 1);
}

/// Dragging a favorite tile over another moves it there right away (like tabs) and saves the order;
/// releasing keeps it.
#[gpui::test]
fn dragging_a_tile_reorders_the_tiles_live(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:", vec![dir("a"), dir("b"), dir("c"), dir("d")]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:"));
    let tiles: Vec<PathBuf> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| PathBuf::from(r"C:").join(n))
        .collect();
    let order = |names: &[usize]| -> Option<Vec<PathBuf>> {
        Some(names.iter().map(|&i| tiles[i].clone()).collect())
    };
    cx.update(|_, cx| {
        crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.favorites = order(&[0, 1, 2, 3]), cx);
        });
    });
    cx.run_until_parked();
    let slot = |cx: &mut VisualTestContext, selector| {
        cx.debug_bounds(selector).expect("the tile should be drawn")
    };
    let (first, third) = (slot(cx, "tile-slot-0"), slot(cx, "tile-slot-2"));

    // Pick up the first tile and move within it: nothing moves yet.
    cx.simulate_mouse_down(first.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        first.center() + point(px(6.), px(4.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.update(|_, cx| assert!(cx.has_active_drag()));
    assert_eq!(settings(cx).favorites, order(&[0, 1, 2, 3]));

    // Entering the third tile moves it there while the drag is still going.
    cx.simulate_mouse_move(third.center(), Some(MouseButton::Left), Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(cx.has_active_drag(), "still dragging"));
    assert_eq!(
        settings(cx).favorites,
        order(&[1, 2, 0, 3]),
        "the dragged tile should take the slot under the cursor"
    );
    // Back over the first slot (now b): moves back.
    cx.simulate_mouse_move(first.center(), Some(MouseButton::Left), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(settings(cx).favorites, order(&[0, 1, 2, 3]));
    // And forward again, then drop: the order stays.
    cx.simulate_mouse_move(third.center(), Some(MouseButton::Left), Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_up(third.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(!cx.has_active_drag()));
    assert_eq!(settings(cx).favorites, order(&[1, 2, 0, 3]));
    assert_eq!(
        view.read_with(cx, |v, _| v.display_path().map(Path::to_path_buf)),
        Some(PathBuf::from(r"C:")),
        "dropping a tile does not open it"
    );
}

/// Ctrl+Shift+C copies selected paths quoted, one per line, or the current folder if nothing is
/// selected (network locations start with \). The path bar menu has copy address (unquoted), copy
/// as path, edit address and clear history.
#[gpui::test]
fn copy_as_path_and_the_path_bar_menu(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    let fs = FakeFs::new();
    fs.insert_dir(r"\\nas\share", vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"\\nas\share");
    wait_until(cx, &view, "load", loaded(r"\\nas\share"));
    let copied = |shell: &FakeShell| shell.clipboard_text.lock().clone();

    // Nothing selected: the current folder.
    cx.simulate_keystrokes("ctrl-shift-c");
    wait_shell("copy", || copied(&shell).is_some());
    assert_eq!(copied(&shell).as_deref(), Some(r#""\\nas\share""#));

    // Two selected: one per line.
    *shell.clipboard_text.lock() = None;
    cx.simulate_keystrokes("down shift-down ctrl-shift-c");
    wait_shell("copy selection", || copied(&shell).is_some());
    assert_eq!(
        copied(&shell).as_deref(),
        Some(concat!(
            r#""\\nas\share\a.txt""#,
            "\r\n",
            r#""\\nas\share\b.txt""#
        ))
    );

    // Path bar context menu.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_path_menu(gpui::point(gpui::px(300.), gpui::px(20.)), window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(
        menu_labels(&view, cx).unwrap(),
        [
            t!("menu.copy_address"),
            t!("menu.copy_as_path"),
            t!("menu.edit_address"),
            "—".into(),
            t!("menu.clear_history"),
        ]
    );
    *shell.clipboard_text.lock() = None;
    run_command(&view, cx, MenuCommand::CopyAddress);
    wait_shell("copy address", || copied(&shell).is_some());
    assert_eq!(
        copied(&shell).as_deref(),
        Some(r"\\nas\share"),
        "address should not be quoted"
    );

    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.recent = vec![PathBuf::from(r"C:\x")], cx);
        });
    });
    let open_menu = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.open_path_menu(gpui::point(gpui::px(300.), gpui::px(20.)), window, cx);
            });
        });
    };
    open_menu(cx);
    run_command(&view, cx, MenuCommand::ClearHistory);
    cx.update(|_, cx| {
        assert!(
            SettingsStore::global(cx)
                .read(cx)
                .settings
                .recent
                .is_empty()
        );
    });
    open_menu(cx);
    run_command(&view, cx, MenuCommand::EditAddress);
    assert!(view.update(cx, |v, _| v.editing_path));
}

/// Commands like cmd, powershell or `code .` typed in the path bar run in the current folder (like
/// Explorer). A name matching a subfolder navigates into it instead.
#[gpui::test]
fn typing_a_command_in_the_path_bar_runs_it_here(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("docs"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\p\docs", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\p");
    wait_until(cx, &view, "load", loaded(r"C:\p"));
    let ran = |shell: &FakeShell| shell.ran.lock().clone();

    cx.dispatch_action(FocusPathBar);
    cx.simulate_input("cmd");
    cx.simulate_keystrokes("enter");
    wait_shell("run cmd", || !ran(&shell).is_empty());
    assert_eq!(ran(&shell), [("cmd".to_string(), PathBuf::from(r"C:\p"))]);
    assert!(!view.update(cx, |v, _| v.editing_path));

    cx.dispatch_action(FocusPathBar);
    cx.simulate_input("code .");
    cx.simulate_keystrokes("enter");
    wait_shell("run code", || ran(&shell).len() == 2);
    assert_eq!(ran(&shell)[1].0, "code .");

    // A folder name navigates in instead of running.
    cx.dispatch_action(FocusPathBar);
    cx.simulate_input("docs");
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, "enter docs", loaded(r"C:\p\docs"));
    assert_eq!(ran(&shell).len(), 2);
}

/// User-added apps appear under "Open with" and disappear once removed.
#[gpui::test]
fn custom_open_with_programs_show_in_the_menu(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\ow", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\ow");
    wait_until(cx, &view, "load", loaded(r"C:\ow"));
    let set = |cx: &mut VisualTestContext, extra: Vec<PathBuf>| {
        cx.update(|_, cx| {
            SettingsStore::global(cx)
                .update(cx, |s, cx| s.update(|st| st.open_with_extra = extra, cx));
        });
    };
    set(cx, vec![PathBuf::from(r"C:\Tools\Notepad++\notepad++.exe")]);
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        labels
            .iter()
            .any(|l| *l == t!("menu.open_with", name = "notepad++")),
        "{labels:?}"
    );
    cx.simulate_keystrokes("escape");
    set(cx, Vec::new());
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(
        !labels.iter().any(|l| l.contains("notepad++")),
        "{labels:?}"
    );
}

/// Alt-drop menu: the default action comes first; "Copy" and "Create shortcut" do that, "Cancel"
/// does nothing.
#[gpui::test]
fn alt_drop_asks_what_to_do(cx: &mut TestAppContext) {
    use crate::explorer::MenuCommand as C;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\d", vec![dir("target"), file("a.txt", 1)]);
    fs.insert_many(r"C:\d\target", 1);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\d");
    wait_until(cx, &view, "load", loaded(r"C:\d"));
    let sources = vec![PathBuf::from(r"C:\d\a.txt")];
    let dest = PathBuf::from(r"C:\d\target");
    let open = |cx: &mut gpui::VisualTestContext| {
        let (sources, dest) = (sources.clone(), dest.clone());
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.open_drop_menu(
                    sources,
                    dest,
                    gpui::point(gpui::px(100.), gpui::px(100.)),
                    window,
                    cx,
                );
            });
        });
    };
    let labels = |cx: &mut gpui::VisualTestContext| {
        view.update(cx, |v, _| {
            v.context_menu
                .as_ref()
                .map(crate::explorer::ContextMenuForTest::labels)
                .unwrap_or_default()
        })
    };
    open(cx);
    let shown = labels(cx);
    // Move, copy, create shortcut, separator, cancel. Locale depends on other tests, so only the
    // structure is checked.
    assert_eq!(shown.len(), 5, "{shown:?}");
    assert!(
        shown[0].contains("target") && shown[1].contains("target"),
        "{shown:?}"
    );
    cx.update(|window, cx| view.update(cx, |v, cx| v.run_menu_command(C::DropCancel, window, cx)));
    cx.run_until_parked();
    assert!(shell.ops().is_empty() && view.update(cx, |v, _| v.pending_drop.is_none()));

    open(cx);
    cx.update(|window, cx| view.update(cx, |v, cx| v.run_menu_command(C::DropCopy, window, cx)));
    wait_shell("copy", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops()[0],
        FileOp::Copy {
            sources: sources.clone(),
            dest: dest.clone(),
        }
    );

    open(cx);
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.run_menu_command(C::DropShortcut, window, cx));
    });
    wait_shell("create shortcut", || {
        !shell.shortcuts_made.lock().is_empty()
    });
    assert_eq!(
        shell.shortcuts_made.lock()[0],
        (sources.clone(), dest.clone())
    );

    // Dropping into the source folder shows no menu.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.open_drop_menu(
                sources.clone(),
                PathBuf::from(r"C:\d"),
                gpui::point(gpui::px(1.), gpui::px(1.)),
                window,
                cx,
            );
        });
    });
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
}

#[test]
fn shortcut_names_follow_windows_and_never_overwrite() {
    let label = |name: &str| format!("{name} - Shortcut");
    let first = crate::shell::shortcut_path_named(
        Path::new(r"C:\x\notes.txt"),
        Path::new(r"C:\d"),
        label,
        |_| false,
    );
    assert_eq!(first, PathBuf::from(r"C:\d\notes.txt - Shortcut.lnk"));
    let second = crate::shell::shortcut_path_named(
        Path::new(r"C:\x\notes.txt"),
        Path::new(r"C:\d"),
        label,
        |p| p == first,
    );
    assert!(second.to_string_lossy().ends_with(" (2).lnk"), "{second:?}");
    let drive =
        crate::shell::shortcut_path_named(Path::new(r"D:\"), Path::new(r"C:\d"), label, |_| false);
    assert!(
        drive
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("D "),
        "{drive:?}"
    );
}

/// Ctrl+Shift+T reopens closed tabs in their old position; duplicate tab; close other tabs.
#[gpui::test]
fn reopen_duplicate_and_close_other_tabs(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 1);
    fs.insert_many(r"C:\t\two", 2);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let p = PathBuf::from;
    let paths = |cx: &mut VisualTestContext| ws.update(cx, |ws, cx| tab_paths(ws, cx));
    for dir in [r"C:\t\one", r"C:\t\two"] {
        ws.update_in(cx, |ws, window, cx| {
            let at = ws.tab_views().count();
            ws.open_tab(p(dir), at, true, window, cx);
        });
    }
    cx.run_until_parked();
    assert_eq!(paths(cx).len(), 3);

    // Close the middle tab one, then Ctrl+Shift+T: it returns to its old position.
    ws.update_in(cx, |ws, window, cx| ws.close_tab(1, window, cx));
    cx.run_until_parked();
    assert_eq!(paths(cx), [Some(p(r"C:\t")), Some(p(r"C:\t\two"))]);
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    assert_eq!(
        paths(cx),
        [Some(p(r"C:\t")), Some(p(r"C:\t\one")), Some(p(r"C:\t\two"))]
    );
    assert_eq!(ws.update(cx, |ws, _| ws.active), 1);

    // Duplicate opens next to it.
    ws.update_in(cx, |ws, window, cx| ws.duplicate_tab(2, window, cx));
    cx.run_until_parked();
    assert_eq!(paths(cx)[3], Some(p(r"C:\t\two")));

    // Close others: only one remains, and the rest can still be reopened.
    ws.update_in(cx, |ws, window, cx| ws.close_other_tabs(1, window, cx));
    cx.run_until_parked();
    assert_eq!(paths(cx), [Some(p(r"C:\t\one"))]);
    cx.simulate_keystrokes("ctrl-shift-t");
    cx.run_until_parked();
    assert_eq!(paths(cx).len(), 2);
}

/// "Move to new folder": creates a uniquely named folder, moves the selection into it, then starts
/// renaming it.
#[gpui::test]
fn move_selection_into_a_new_folder(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\m");
    let new_folder = t!("ops.new_folder_name").into_owned();
    let second = format!("{new_folder} (2)");
    fs.insert_dir(
        dir_path,
        vec![dir(&new_folder), file("a.txt", 1), file("b.txt", 1)],
    );
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    wait_watcher(&fs, dir_path);
    // New folder, a.txt, b.txt: select a and b.
    cx.simulate_keystrokes("down down shift-down");
    right_click(&view, cx, Some(2));
    run_command(&view, cx, MenuCommand::MoveToNewFolder);
    wait_until(cx, &view, "new folder appears in rename mode", |v| {
        v.renaming
            .as_ref()
            .is_some_and(|r| &*r.name == second.as_str())
    });
    assert_eq!(
        shell.ops(),
        [
            FileOp::NewFolder {
                parent: PathBuf::from(r"C:\m"),
                name: second.clone(),
            },
            FileOp::Move {
                sources: vec![PathBuf::from(r"C:\m\a.txt"), PathBuf::from(r"C:\m\b.txt")],
                dest: dir_path.join(&second),
            },
        ]
    );
}

/// Multi-item rename: items renamed in display order to "name (1)", "name (2)", ... keeping
/// extensions, sent as one operation.
#[gpui::test]
fn renaming_several_items_numbers_them(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:b");
    fs.insert_dir(
        dir_path,
        vec![
            dir("album"),
            file("a.jpg", 1),
            file("b.png", 1),
            file("z.txt", 1),
        ],
    );
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:b");
    wait_until(cx, &view, "load", loaded(r"C:b"));
    wait_watcher(&fs, dir_path);
    // Album, a.jpg, b.png: select the first three with the cursor on b.png.
    cx.simulate_keystrokes("down shift-down shift-down f2 ctrl-a");
    cx.simulate_input("trip.jpg");
    cx.simulate_keystrokes("enter");
    wait_shell("batch rename", || !shell.ops().is_empty());
    let b = PathBuf::from(r"C:b");
    assert_eq!(
        shell.ops(),
        [FileOp::RenameMany {
            items: vec![
                (b.join("album"), "trip (1)".into()),
                (b.join("a.jpg"), "trip (2).jpg".into()),
                (b.join("b.png"), "trip (3).png".into()),
            ],
        }]
    );
}

/// Home and settings are tabs: they survive switching away and back. Settings opens in a new tab
/// that closes with it.
#[gpui::test]
fn home_and_settings_live_in_tabs(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one")]);
    fs.insert_many(r"C:\t\one", 1);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let titles = |cx: &mut VisualTestContext| ws.update(cx, |ws, cx| ws.tab_titles_for_test(cx));
    let shown = |cx: &mut VisualTestContext| ws.update(cx, |ws, _| ws.shown_page_for_test());

    // Home opens in the current tab, with a folder tab next to it.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    cx.run_until_parked();
    ws.update_in(cx, |ws, window, cx| {
        ws.open_tab(PathBuf::from(r"C:\t\one"), 1, true, window, cx);
    });
    cx.run_until_parked();
    let home = rust_i18n::t!("home.title").into_owned();
    assert_eq!(titles(cx)[0], home);
    assert_eq!(shown(cx), None, "folder tab should not show home");
    // Back to the first tab: home is still there.
    cx.simulate_keystrokes("ctrl-1");
    cx.run_until_parked();
    assert_eq!(shown(cx).as_deref(), Some(home.as_str()));

    // Settings opens in a new tab; closing settings closes that tab.
    ws.update_in(cx, Workspace::open_settings_for_test);
    cx.run_until_parked();
    let settings = rust_i18n::t!("settings.title").into_owned();
    assert_eq!(titles(cx).len(), 3);
    assert_eq!(shown(cx).as_deref(), Some(settings.as_str()));
    ws.update_in(cx, Workspace::close_settings_for_test);
    cx.run_until_parked();
    assert_eq!(titles(cx).len(), 2);
    assert_eq!(titles(cx)[0], home, "home should stay in its original tab");
}

/// Home lists only the ten most recent files, newest first, even if the shell returns more.
#[gpui::test]
fn home_lists_the_ten_most_recent_files(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let now = SystemTime::now();
    *shell.recent.lock() = (0..15)
        .map(|i| crate::home::RecentFile {
            path: PathBuf::from(format!(r"C:\docs\file{i}.txt")),
            used: now - Duration::from_secs(60 * i),
        })
        .collect();
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\t");
    ws.update_in(cx, |ws, window, cx| {
        ws.open_home_for_test(false, window, cx);
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let listed = loop {
        cx.run_until_parked();
        if let Some(listed) = ws.update(cx, |ws, cx| ws.home_recent_for_test(cx)) {
            break listed;
        }
        assert!(Instant::now() < deadline, "recent files never loaded");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(*shell.recent_limit.lock(), Some(crate::home::RECENT_MAX));
    assert_eq!(listed.len(), 10);
    assert_eq!(listed[0], PathBuf::from(r"C:\docs\file0.txt"));
    assert_eq!(listed[9], PathBuf::from(r"C:\docs\file9.txt"));
}

/// Settings › Search: while "Search this PC" is off, the settings that depend on it are covered
/// and clicks don't reach them; once it is on they work.
#[gpui::test]
fn search_settings_need_search_this_pc(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    let skip_build = |cx: &mut VisualTestContext| settings(cx).search_skip_build_dirs;
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.search"), cx);
    });
    cx.run_until_parked();
    assert!(!settings(cx).global_search, "off by default");
    let click_switch = |cx: &mut VisualTestContext| {
        let bounds = cx
            .debug_bounds("search-skip-build")
            .expect("the skip build folders switch should be drawn");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };
    click_switch(cx);
    assert!(!skip_build(cx), "covered while whole-PC search is off");

    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.global_search = true, cx));
    });
    cx.run_until_parked();
    click_switch(cx);
    assert!(skip_build(cx), "works once whole-PC search is on");
}

/// Browse buttons: the file dialog runs off the UI thread with a filter for the card, and the
/// chosen file is applied; Cancel changes nothing.
#[gpui::test]
fn browse_buttons_apply_the_chosen_file(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\o");
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    let browse = |cx: &mut VisualTestContext, image: bool, pick: Option<&str>| {
        shell.picks.lock().push(pick.map(PathBuf::from));
        ws.update_in(cx, |ws, window, cx| ws.browse_for_test(image, window, cx));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !shell.picks.lock().is_empty() {
            assert!(Instant::now() < deadline, "file dialog never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let the result come back to the UI thread.
        for _ in 0..20 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    browse(cx, true, None);
    assert_eq!(
        settings(cx).background_image,
        None,
        "cancel changes nothing"
    );
    browse(cx, true, Some(r"C:\pics\sky.png"));
    assert_eq!(
        settings(cx).background_image,
        Some(PathBuf::from(r"C:\pics\sky.png"))
    );
    let request = shell.pick_requests.lock().last().cloned().unwrap();
    assert!(request.patterns.contains("*.png"), "{request:?}");

    browse(cx, false, Some(r"C:\tools\editor.exe"));
    assert_eq!(
        settings(cx).open_with_extra,
        vec![PathBuf::from(r"C:\tools\editor.exe")]
    );
    let request = shell.pick_requests.lock().last().cloned().unwrap();
    assert_eq!(request.patterns, "*.exe");
}

/// In a narrow window the Details columns keep their widths, like Explorer: the list scrolls
/// sideways (the header with it) instead of dropping any. Only the preview pane gives way,
/// narrowing and then hiding, and it comes back when the window grows.
#[gpui::test]
fn narrow_window_scrolls_columns_and_squeezes_the_preview(cx: &mut TestAppContext) {
    use gpui::{ScrollDelta, ScrollEvent, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\n", vec![file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::new(FakeShell::default()), r"C:\n");
    wait_until(cx, &view, "load", loaded(r"C:\n"));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    let at = |cx: &mut VisualTestContext, width: f32| {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(700.)));
        cx.run_until_parked();
        let columns: Vec<_> = view
            .update(cx, |v, _| v.header_for_test())
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        assert_eq!(
            columns,
            [
                crate::prefs::Col::Name,
                crate::prefs::Col::Modified,
                crate::prefs::Col::Type,
                crate::prefs::Col::Size
            ],
            "every column stays at {width}"
        );
        view.update(cx, |v, _| v.fit_for_test())
    };
    // (preview pane, its width): 22 % of the window, between 280 and 380 px, while the bare list
    // (a 200 px name and the size column) fits beside it.
    assert_eq!(at(cx, 1800.), (true, 380.));
    // A Name column sized wider than the pane has room for: the list scrolls, nothing drops.
    crate::prefs::set_width(crate::prefs::Col::Name, Some(px(560.)));
    assert_eq!(at(cx, 1000.), (true, 280.));
    let (list, row) = view.update(cx, |v, _| {
        (v.list_bounds_for_test(), v.row_width_for_test())
    });
    assert!(row > Some(list.size.width), "{row:?} in {list:?}");
    let max = view.update(cx, |v, _| v.list_scroll_max_for_test());
    assert!(
        max.x > px(0.),
        "columns wider than the pane scroll: {max:?}"
    );
    // Shift+wheel (sideways) scrolls the rows, and the header follows.
    cx.simulate_event(ScrollEvent {
        position: list.origin + point(px(80.), px(40.)),
        delta: ScrollDelta::Pixels(point(px(-60.), px(0.))),
        ..ScrollEvent::default()
    });
    cx.run_until_parked();
    let scrolled = view.update(cx, |v, _| v.list_scroll_for_test().x);
    assert!(scrolled < px(0.), "{scrolled:?}");
    assert_eq!(
        view.update(cx, |v, _| v.header_offset_for_test()),
        Some(scrolled)
    );
    assert_eq!(at(cx, 880.), (true, 224.), "then the preview pane narrows");
    assert!(!at(cx, 800.).0, "and finally hides");
    assert_eq!(at(cx, 1800.), (true, 380.), "it comes back");
    view.update(cx, |_, cx| {
        crate::prefs::set_width(crate::prefs::Col::Name, None);
        cx.notify();
    });
    cx.run_until_parked();
    // Wide enough again: no sideways scroll left, header back in line.
    assert_eq!(view.update(cx, |v, _| v.list_scroll_for_test().x), px(0.));
    assert_eq!(
        view.update(cx, |v, _| v.header_offset_for_test()),
        Some(px(0.))
    );
}

/// Ctrl+Y and Ctrl+Shift+Z redo what Ctrl+Z undid, the progress center offers Redo on the undo,
/// and a new operation clears what could be redone.
#[gpui::test]
fn ctrl_y_redoes_the_last_undo_until_something_new_happens(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let dir_path = Path::new(r"C:\redo");
    fs.insert_dir(dir_path, vec![file("a.txt", 1), file("b.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = setup(cx, fs.clone(), shell.clone(), r"C:\redo");
    wait_until(cx, &view, "load", loaded(r"C:\redo"));
    wait_watcher(&fs, dir_path);
    let recycle_b = FileOp::Delete {
        paths: vec![PathBuf::from(r"C:\redo\b.txt")],
        recycle: true,
    };

    // Nothing undone yet: nothing to redo.
    cx.simulate_keystrokes("ctrl-y");
    cx.run_until_parked();
    view.update(cx, |v, _| {
        assert_eq!(v.notice.as_deref(), Some(&*t!("redo.nothing")));
    });

    cx.simulate_keystrokes("down down delete");
    wait_until(cx, &view, "recycle", |v| {
        v.ops_running == 0 && names(v) == ["a.txt"]
    });
    cx.simulate_keystrokes("ctrl-z");
    wait_shell("undo", || shell.undone.lock().len() == 1);
    wait_until(cx, &view, "undo done", |v| v.ops_running == 0);
    // The finished undo offers Redo in the progress center.
    let redo_offered = cx.update(|_, cx| {
        crate::op_center::OpCenter::jobs(cx)
            .last()
            .and_then(|j| j.redo)
            .is_some_and(|id| crate::explorer::RedoStack::has(cx, id))
    });
    assert!(redo_offered);

    // Ctrl+Y recycles b.txt again (only what the original operation did).
    cx.simulate_keystrokes("ctrl-y");
    wait_shell("redo", || shell.ops().len() == 2);
    wait_until(cx, &view, "redo done", |v| v.ops_running == 0);
    assert_eq!(shell.ops(), [recycle_b.clone(), recycle_b]);
    view.update(cx, |v, _| {
        let redid = t!("redo.done", op = t!("op.recycle"));
        assert_eq!(v.notice.as_deref(), Some(&*redid));
    });
    // The redo is itself undoable, and Ctrl+Shift+Z redoes again after that.
    cx.simulate_keystrokes("ctrl-z");
    wait_shell("second undo", || shell.undone.lock().len() == 2);
    wait_until(cx, &view, "second undo done", |v| v.ops_running == 0);
    cx.simulate_keystrokes("ctrl-shift-z");
    wait_shell("second redo", || shell.ops().len() == 3);
    wait_until(cx, &view, "second redo done", |v| v.ops_running == 0);

    // Undo once more, then do something new: the redo is gone.
    cx.simulate_keystrokes("ctrl-z");
    wait_shell("third undo", || shell.undone.lock().len() == 3);
    wait_until(cx, &view, "third undo done", |v| v.ops_running == 0);
    assert!(cx.update(|_, cx| crate::explorer::RedoStack::last(cx).is_some()));
    cx.simulate_keystrokes("ctrl-shift-n");
    wait_shell("new file", || shell.ops().len() == 4);
    wait_until(cx, &view, "new file done", |v| v.ops_running == 0);
    assert!(cx.update(|_, cx| crate::explorer::RedoStack::last(cx).is_none()));
}

/// A rename undone and redone renames again; a cancelled move redoes only what it reached.
#[test]
fn redo_replays_what_the_operation_actually_did() {
    use crate::shell::{PutBack, Undo, redo_of};
    let rename = FileOp::Rename {
        path: PathBuf::from(r"C:\a\old.txt"),
        new_name: "new.txt".into(),
    };
    let undo = Undo::PutBack(vec![PutBack {
        now: r"C:\a\new.txt".into(),
        parent: PathBuf::from(r"C:\a"),
        name: "old.txt".into(),
    }]);
    assert_eq!(redo_of(&rename, &undo), Some(rename.clone()));

    let moved = FileOp::Move {
        sources: vec![PathBuf::from(r"C:\a\1"), PathBuf::from(r"C:\a\2")],
        dest: PathBuf::from(r"D:\b"),
    };
    let reached_one = Undo::PutBack(vec![PutBack {
        now: r"D:\b\1".into(),
        parent: PathBuf::from(r"C:\a"),
        name: "1".into(),
    }]);
    assert_eq!(
        redo_of(&moved, &reached_one),
        Some(FileOp::Move {
            sources: vec![PathBuf::from(r"C:\a\1")],
            dest: PathBuf::from(r"D:\b"),
        })
    );
    let purge = FileOp::Delete {
        paths: vec![PathBuf::from(r"C:\a\1")],
        recycle: false,
    };
    assert_eq!(
        redo_of(&purge, &reached_one),
        None,
        "permanent deletes can't be redone"
    );

    let copy = FileOp::Copy {
        sources: vec![PathBuf::from(r"C:\a\x"), PathBuf::from(r"C:\a\y")],
        dest: PathBuf::from(r"E:\"),
    };
    let copied_x = Undo::Remove(vec![PathBuf::from(r"E:\x")]);
    assert_eq!(
        redo_of(&copy, &copied_x),
        Some(FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\a\x")],
            dest: PathBuf::from(r"E:\"),
        })
    );
}

/// Dragging with the right button and dropping on a folder asks (move, copy, shortcut, cancel)
/// instead of moving; a right click without moving still opens the item's menu on release.
#[gpui::test]
fn right_button_drag_asks_what_to_do_on_drop(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\rd", vec![dir("sub"), file("a.txt", 1)]);
    fs.insert_dir(r"C:\rd\sub", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, fs, shell.clone(), r"C:\rd");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\rd"));
    cx.run_until_parked();
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let on_a = point(bounds.left() + px(60.), bounds.top() + row * 1.5);
    let on_sub = point(bounds.left() + px(200.), bounds.top() + row * 0.5);

    // A plain right click: the menu opens on release, no drag.
    cx.simulate_mouse_down(on_a, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(
        view.update(cx, |v, _| v.context_menu.is_none()),
        "not before release"
    );
    cx.simulate_mouse_up(on_a, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(view.update(cx, |v, _| v.context_menu.is_some()));
    cx.update(|_, cx| assert!(!cx.has_active_drag()));
    view.update(cx, |v, cx| {
        v.context_menu = None;
        cx.notify();
    });
    cx.run_until_parked();

    // Right drag a.txt onto sub.
    cx.simulate_mouse_down(on_a, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_move(
        on_a + point(px(30.), px(4.)),
        Some(MouseButton::Right),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert!(cx.has_active_drag(), "the right button started Arke's drag");
        assert!(cx.has_global::<crate::explorer::RightDrag>());
    });
    cx.simulate_mouse_move(on_sub, Some(MouseButton::Right), Modifiers::none());
    cx.simulate_mouse_up(on_sub, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    let (menu, pending) = view.update(cx, |v, _| {
        (
            v.context_menu
                .as_ref()
                .map(crate::explorer::ContextMenuForTest::labels)
                .unwrap_or_default(),
            v.pending_drop.clone(),
        )
    });
    // Same volume, so Move comes first; then copy, shortcut, separator, cancel.
    assert_eq!(menu.len(), 5, "{menu:?}");
    assert!(menu[0].contains("sub"), "{menu:?}");
    assert_eq!(
        pending,
        Some((
            vec![PathBuf::from(r"C:\rd\a.txt")],
            PathBuf::from(r"C:\rd\sub")
        ))
    );
    assert!(shell.ops().is_empty(), "nothing happens before choosing");
    cx.update(|_, cx| {
        assert!(!cx.has_active_drag());
        assert!(!cx.has_global::<crate::explorer::RightDrag>());
    });
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.run_menu_command(MenuCommand::DropCopy, window, cx);
        });
    });
    wait_shell("copy", || !shell.ops().is_empty());
    assert_eq!(
        shell.ops(),
        [FileOp::Copy {
            sources: vec![PathBuf::from(r"C:\rd\a.txt")],
            dest: PathBuf::from(r"C:\rd\sub"),
        }]
    );

    // Released on empty space in the same folder (nowhere to move to): the drag just ends.
    cx.simulate_mouse_down(on_a, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_move(
        on_a + point(px(30.), px(4.)),
        Some(MouseButton::Right),
        Modifiers::none(),
    );
    cx.run_until_parked();
    let nowhere = point(bounds.left() + px(60.), bounds.top() + row * 4.5);
    cx.simulate_mouse_move(nowhere, Some(MouseButton::Right), Modifiers::none());
    cx.simulate_mouse_up(nowhere, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert!(!cx.has_active_drag());
        assert!(!cx.has_global::<crate::explorer::RightDrag>());
    });
    assert_eq!(shell.ops().len(), 1);
}

/// The remote wrapper must pass every local call through to the Windows shell: trait methods with
/// a default (PDF pages, cloud states, drive keys) are easy to miss there and would quietly turn
/// the feature off.
#[test]
fn remote_shell_passes_local_calls_through_and_keeps_servers_out() {
    let inner = Arc::new(FakeShell::default());
    let shell =
        crate::remote_shell::RemoteShell::new(inner.clone(), crate::remote_shell::test_remotes());
    let local = Path::new(r"C:\docs\a.pdf");
    let server = Path::new("sftp://u@h/docs/a.pdf");
    assert!(shell.render_pdf(local, 0, (10, 10)).is_ok());
    assert!(matches!(
        shell.render_pdf(server, 0, (10, 10)),
        Err(ShellError::NotOnRemote)
    ));
    assert_eq!(inner.pdf_renders.lock().len(), 1);
    let _ = shell.cloud_scan(Path::new(r"C:\docs"), &[]);
    let _ = shell.cloud_scan(Path::new("sftp://u@h/docs"), &[]);
    assert_eq!(inner.cloud_scans.lock().len(), 1);
    assert_eq!(shell.drive_key(local), "C:");
    assert_eq!(shell.drive_key(server), "sftp://u@h");
}

/// A full path to a file typed or pasted in the path bar (with the quotes "Copy as path" adds)
/// opens the file with its default app and shows its folder with it selected, like Explorer.
#[gpui::test]
fn path_bar_file_path_opens_file_and_selects_it(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_many(r"C:\home", 1);
    let name = "Quarterly Review — 11月專案.docx";
    fs.insert_dir(
        r"C:\team\報告\Monthly\202611",
        vec![file("other.txt", 1), file(name, 10)],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell.clone(), r"C:\home");
    wait_until(cx, &view, "start folder", loaded(r"C:\home"));
    let target = PathBuf::from(r"C:\team\報告\Monthly\202611").join(name);
    cx.dispatch_action(FocusPathBar);
    cx.run_until_parked();
    cx.simulate_input(&format!("\"{}\"", target.display()));
    cx.simulate_keystrokes("enter");
    wait_until(
        cx,
        &view,
        "the file's folder",
        loaded(r"C:\team\報告\Monthly\202611"),
    );
    wait_until(cx, &view, "the file selected", |v| {
        selected_name(v).as_deref() == Some(name)
    });
    wait_shell("file opened", || !shell.opened.lock().is_empty());
    assert_eq!(*shell.opened.lock(), [target]);
    view.update(cx, |v, _| {
        assert!(v.error.is_none() && v.failed.is_none(), "not an error");
        assert!(
            shell.ran.lock().is_empty(),
            "a quoted path is not a command"
        );
        assert_eq!(v.history.back_target(), Some(Path::new(r"C:\home")));
    });
}

/// Test probe: every `\\` path is on the network and the server offers these shares.
struct SharesProbe(Vec<arke_core::net::Share>);

impl arke_core::net::NetProbe for SharesProbe {
    fn unc(&self, path: &Path) -> Option<PathBuf> {
        path.to_string_lossy()
            .starts_with(r"\\")
            .then(|| path.to_path_buf())
    }
    fn reach(&self, _server: &str) -> arke_core::net::Reach {
        arke_core::net::Reach::Up
    }
    fn shares(&self, _server: &str) -> std::io::Result<Vec<arke_core::net::Share>> {
        assert!(
            !arke_core::fs::is_ui_thread(),
            "shares listed on the UI thread"
        );
        Ok(self.0.clone())
    }
}

/// `\\server` lists its disk shares (no `$` shares while hidden items are off); Up from a share
/// goes back to the server with the share selected.
#[gpui::test]
fn server_lists_its_shares_and_up_returns_to_it(cx: &mut TestAppContext) {
    use arke_core::net::{STYPE_DISKTREE, Share};
    let fs = FakeFs::new();
    fs.insert_many(r"\\nas\team", 2);
    let shell = Arc::new(FakeShell::default());
    *shell.net_probe.lock() = Some(Arc::new(SharesProbe(vec![
        Share {
            name: "public".into(),
            kind: STYPE_DISKTREE,
        },
        Share {
            name: "team".into(),
            kind: STYPE_DISKTREE,
        },
        Share {
            name: "C$".into(),
            kind: STYPE_DISKTREE | 0x8000_0000,
        },
        Share {
            name: "IPC$".into(),
            kind: 3 | 0x8000_0000,
        },
    ])));
    let (view, cx) = setup(cx, fs, shell, r"\\nas");
    wait_until(cx, &view, "the server's shares", loaded(r"\\nas"));
    view.update(cx, |v, _| {
        let mut shown = names(v);
        shown.sort();
        assert_eq!(shown, ["public", "team"]);
        assert_eq!(v.tab_title().as_ref(), "nas");
    });
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"\\nas\team"), NavKind::New, cx);
    });
    wait_until(cx, &view, "into a share", loaded(r"\\nas\team"));
    assert_eq!(view.update(cx, |v, _| names(v).len()), 2);
    cx.dispatch_action(crate::explorer::GoUp);
    wait_until(cx, &view, "up to the server", loaded(r"\\nas"));
    wait_until(cx, &view, "the share selected", |v| {
        selected_name(v).as_deref() == Some("team")
    });
}

/// A server's shares are only listed: nothing deletes, renames or moves a share root, or creates
/// items straight in a server. Copying a share elsewhere and working inside a share still run.
#[test]
fn share_roots_are_never_changed() {
    use crate::explorer::changes_share_list;
    let p = PathBuf::from;
    let share = PathBuf::from(r"\\nas").join("team");
    assert!(changes_share_list(&FileOp::Delete {
        paths: vec![share.clone()],
        recycle: true,
    }));
    assert!(changes_share_list(&FileOp::Rename {
        path: p(r"\\nas\team\"),
        new_name: "x".into(),
    }));
    assert!(changes_share_list(&FileOp::RenameMany {
        items: vec![(share.clone(), "x".into())],
    }));
    assert!(changes_share_list(&FileOp::Move {
        sources: vec![share.clone()],
        dest: p(r"C:\backup"),
    }));
    assert!(changes_share_list(&FileOp::Copy {
        sources: vec![p(r"C:\a.txt")],
        dest: p(r"\\nas"),
    }));
    assert!(changes_share_list(&FileOp::NewFolder {
        parent: p(r"\\nas"),
        name: "New folder".into(),
    }));
    assert!(!changes_share_list(&FileOp::Copy {
        sources: vec![share.clone()],
        dest: p(r"C:\backup"),
    }));
    assert!(!changes_share_list(&FileOp::Delete {
        paths: vec![share.join("old.txt")],
        recycle: true,
    }));
    assert!(!changes_share_list(&FileOp::NewFolder {
        parent: share,
        name: "New folder".into(),
    }));
}

/// Settings › About › Remove Arke: the confirmation opens over the page, "Keep my settings"
/// toggles, and Esc closes it without removing anything.
#[gpui::test]
fn uninstall_asks_before_removing(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.about"), cx);
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("uninstall-open").is_some(),
        "the About page shows the remove button"
    );
    assert!(
        cx.debug_bounds("about-source").is_some() && cx.debug_bounds("about-licenses").is_some(),
        "the license line links to the source code and the license texts"
    );
    ws.update_in(cx, Workspace::open_uninstall_for_test);
    cx.run_until_parked();
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.uninstall_ask_for_test()),
        Some(false)
    );
    assert!(cx.debug_bounds("uninstall-confirm").is_some());
    let keep = cx
        .debug_bounds("uninstall-keep")
        .expect("the keep settings box is drawn");
    cx.simulate_click(keep.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.uninstall_ask_for_test()),
        Some(true)
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(ws.read_with(cx, |ws, _| ws.uninstall_ask_for_test()), None);
    assert!(
        cx.debug_bounds("uninstall-open").is_some(),
        "Esc closes the dialog, not the settings page"
    );
}

/// First run: the corner card offers the Start menu shortcut; adding it (with "start when I sign
/// in" ticked) turns both settings on and the card goes away for good.
#[gpui::test]
fn first_run_card_adds_the_start_menu_shortcut(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.run_until_parked();
    assert!(cx.debug_bounds("welcome").is_none(), "not a first run");
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.set_first_run_for_test(true);
            cx.notify();
        });
    });
    cx.run_until_parked();
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("{id} is drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };
    click(cx, "welcome-autostart");
    click(cx, "welcome-add");
    let s = settings(cx);
    assert!(s.start_menu && s.autostart && s.start_menu_offered);
    assert!(cx.debug_bounds("welcome").is_none(), "answered");
}

/// "Not now" dismisses the card without adding anything.
#[gpui::test]
fn first_run_card_can_be_dismissed(cx: &mut TestAppContext) {
    use crate::settings::SettingsStore;
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\o", vec![file("a.txt", 1)]);
    let (_ws, cx) = setup_workspace(cx, fs, r"C:\o");
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.set_first_run_for_test(true);
            cx.notify();
        });
    });
    cx.run_until_parked();
    let bounds = cx
        .debug_bounds("welcome-not-now")
        .expect("the card is shown");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let s = settings(cx);
    assert!(!s.start_menu && !s.autostart && s.start_menu_offered);
    assert!(cx.debug_bounds("welcome").is_none());
}

/// #7: the name to select may come from a path in another case than the folder's entry (a typed
/// path, or one another program passed); it is still selected, under the entry's spelling.
#[gpui::test]
fn a_name_in_another_case_is_still_selected_after_navigating(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![dir("Child"), dir("other")]);
    fs.insert_many(r"C:\p\Child", 1);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\p\Child");
    wait_until(cx, &view, "load", loaded(r"C:\p\Child"));
    view.update(cx, |e, cx| {
        e.navigate_selecting(
            PathBuf::from(r"C:\p"),
            NavKind::New,
            Some("child".into()),
            cx,
        );
    });
    wait_until(cx, &view, "up", loaded(r"C:\p"));
    cx.run_until_parked();
    assert_eq!(view.update(cx, |e, _| selected_names(e)), vec!["Child"]);
    assert!(
        view.update(cx, |e, _| e.cursor).is_some(),
        "and gets the cursor"
    );
}

/// #7: going back to an ancestor more than one level up selects the folder on the way, as
/// File Explorer does, not only when it is the direct parent.
#[gpui::test]
fn going_back_to_an_ancestor_selects_the_folder_on_the_way(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![dir("b"), dir("x")]);
    fs.insert_dir(r"C:\a\b", vec![dir("c")]);
    fs.insert_many(r"C:\a\b\c", 1);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\a");
    wait_until(cx, &view, "a", loaded(r"C:\a"));
    view.update(cx, |e, cx| {
        e.navigate(PathBuf::from(r"C:\a\b\c"), NavKind::New, cx);
    });
    wait_until(cx, &view, "c", loaded(r"C:\a\b\c"));
    view.update_in(cx, |e, window, cx| {
        e.go_back(&crate::explorer::GoBack, window, cx);
    });
    wait_until(cx, &view, "back", loaded(r"C:\a"));
    cx.run_until_parked();
    assert_eq!(view.update(cx, |e, _| selected_names(e)), vec!["b"]);
}
