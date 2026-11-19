//! Arke's own Properties window (Alt+Enter, "Properties" in the menus).
//!
//! One scrolling page of sections, in a window of its own (several can be open). Everything
//! applies the moment it is changed: a name on Enter or when the box loses focus, an attribute on
//! click. Nothing is written that the user did not change, and a failure shows under the thing
//! that failed. The pages only Windows has (security, sharing, previous versions, ...) are one
//! button away ("Windows properties...").
//!
//! Nothing here reads file contents: sizes, times and attributes come from listings and from
//! `GetCompressedFileSize`, so a cloud file that is not on this device is never downloaded.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arke_core::dirsize::{self, Usage};
use arke_core::filetype::{FileKind, file_kind, wants_system_icon, wants_type_icon};
use arke_core::format::{format_count, format_size, type_label};
use arke_core::takeover::Target;
use arke_core::{Entry, EntryKind};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyBinding, MouseButton, ParentElement, Render, RenderImage, SharedString, Styled,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, actions, div,
    img, prelude::*, px, size, svg,
};
use rust_i18n::t;

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{Explorer, Services, kind_icon, spawn_thread};
use crate::shell::{ATTR_HIDDEN, ATTR_READONLY, FileDetails, FileOp, OpOutcome};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

actions!(properties, [CloseProperties]);

const CONTEXT: &str = "Properties";

/// Window size when it opens.
const WIDTH: f32 = 460.;
const HEIGHT: f32 = 640.;
/// Longest wait between two updates of a folder size that is still being counted.
const PROGRESS_EVERY: Duration = Duration::from_millis(120);
/// Width of the labels in the info rows.
const LABEL_W: gpui::Pixels = px(116.);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseProperties, Some(CONTEXT))]);
}

/// The item a window shows, and what it is called there.
#[derive(Clone, Debug)]
struct Row {
    path: PathBuf,
    entry: Entry,
}

/// A drive's capacity and file system.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Drive {
    label: String,
    file_system: String,
    free: u64,
    total: u64,
}

/// What the first read of the items brings.
struct Loaded {
    rows: Vec<Row>,
    unreadable: usize,
    details: FileDetails,
    target: Option<PathBuf>,
    app: Option<String>,
    drive: Option<Drive>,
    cluster: u64,
    icon: Option<Arc<RenderImage>>,
}

/// Where counting a folder stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Counting {
    Running,
    Done,
    Stopped,
}

/// A check box that can also say "some".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tri {
    Off,
    Mixed,
    On,
}

/// What background work reports back.
enum Msg {
    Loaded(Box<Loaded>),
    Usage(Usage, Counting),
    Attribute {
        /// The attributes the items had before, to put back when it failed.
        before: Vec<u32>,
        result: Result<(), String>,
    },
    Renamed {
        op: FileOp,
        to: PathBuf,
        result: Result<Option<crate::shell::Undo>, String>,
    },
    App(Result<Option<String>, String>),
    Native(Result<(), String>),
}

pub(crate) struct Properties {
    services: Services,
    focus: FocusHandle,
    /// The items asked about, with whether each is a folder.
    targets: Vec<(PathBuf, bool)>,
    loaded: Option<Loaded>,
    name_input: Entity<TextInput>,
    /// The name as it is on disk (what the box goes back to on Esc).
    name: String,
    usage: Option<Usage>,
    counting: Counting,
    cancel: Arc<AtomicBool>,
    tx: flume::Sender<Msg>,
    name_error: Option<String>,
    attr_error: Option<String>,
    app_error: Option<String>,
    native_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for Properties {
    fn drop(&mut self) {
        // A folder still being counted stops with the window.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Opens a Properties window for `targets` (full path, whether it is a folder).
pub(crate) fn open(
    services: Services,
    targets: Vec<(PathBuf, bool)>,
    cx: &mut App,
) -> Option<WindowHandle<Properties>> {
    if targets.is_empty() {
        return None;
    }
    let title = window_title(&targets);
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            focus: true,
            window_min_size: Some(size(px(360.), px(320.))),
            ..Default::default()
        },
        move |window, cx| cx.new(|cx| Properties::new(services, targets, window, cx)),
    );
    match opened {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::error!("cannot open the Properties window: {err:#}");
            None
        }
    }
}

fn leaf(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn window_title(targets: &[(PathBuf, bool)]) -> String {
    let what = match targets {
        [(path, _)] => leaf(path),
        many => tn!("props.items", many.len()).into_owned(),
    };
    format!("{what} - {}", t!("menu.properties"))
}

/// `C:\` for `C:\a\b`; `None` for paths without a drive (servers, relative paths).
fn root_of(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut root = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir => root.push(part),
            _ => break,
        }
    }
    (root.parent().is_none() && root.as_os_str().len() >= 2).then_some(root)
}

fn is_drive_root(path: &Path) -> bool {
    path.parent().is_none() && root_of(path).is_some_and(|r| r == path)
}

impl Properties {
    pub(crate) fn new(
        services: Services,
        targets: Vec<(PathBuf, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, rx) = flume::unbounded::<Msg>();
        let name = match targets.as_slice() {
            [(path, _)] => leaf(path),
            _ => String::new(),
        };
        let name_input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_text(name.clone(), cx);
            input
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let input_focus = name_input.read(cx).focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe_in(
                &name_input,
                window,
                |this, _, event: &InputEvent, _window, cx| match event {
                    InputEvent::Confirm => this.commit_name(cx),
                    InputEvent::Cancel => this.revert_name(cx),
                    InputEvent::Changed => {
                        if this.name_error.take().is_some() {
                            cx.notify();
                        }
                    }
                },
            ),
            // Clicking elsewhere applies the name, like pressing Enter.
            cx.on_blur(&input_focus, window, |this, _, cx| this.commit_name(cx)),
        ];
        let this = Self {
            services,
            focus,
            targets,
            loaded: None,
            name_input,
            name,
            usage: None,
            counting: Counting::Running,
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            name_error: None,
            attr_error: None,
            app_error: None,
            native_error: None,
            _subscriptions: subscriptions,
        };
        cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv_async().await {
                if this.update(cx, |this, cx| this.handle(msg, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        this.start_loading();
        this
    }

    // ───────────────────────────── Reading ─────────────────────────────

    fn start_loading(&self) {
        let services = self.services.clone();
        let targets = self.targets.clone();
        let tx = self.tx.clone();
        let cancel = Arc::clone(&self.cancel);
        spawn_thread("properties-load", move || {
            let loaded = read_items(&services, &targets);
            let cluster = loaded.cluster;
            let rows: Vec<Row> = loaded.rows.clone();
            let details = loaded.details;
            let drive = loaded.drive.is_some();
            let _ = tx.send(Msg::Loaded(Box::new(loaded)));
            if !drive {
                count_usage(&services, &rows, details, cluster, &cancel, &tx);
            }
        });
    }

    fn handle(&mut self, msg: Msg, cx: &mut Context<Self>) {
        match msg {
            Msg::Loaded(loaded) => {
                self.loaded = Some(*loaded);
            }
            Msg::Usage(usage, counting) => {
                // Stopped by the user: what comes in late is dropped.
                if self.counting == Counting::Stopped {
                    return;
                }
                self.usage = Some(usage);
                self.counting = counting;
            }
            Msg::Attribute { before, result } => match result {
                Ok(()) => self.attr_error = None,
                Err(error) => {
                    // Put the check box back: what is shown is what is on disk.
                    if let Some(loaded) = self.loaded.as_mut() {
                        for (row, old) in loaded.rows.iter_mut().zip(before) {
                            row.entry.attributes = old;
                        }
                    }
                    self.attr_error = Some(t!("props.attr_failed", error = error).into_owned());
                }
            },
            Msg::Renamed { op, to, result } => match result {
                Ok(undo) => {
                    self.name_error = None;
                    if let FileOp::Rename { new_name, .. } = &op {
                        self.renamed(to, new_name);
                    }
                    if let Some(undo) = undo {
                        let redo = crate::shell::redo_of(&op, &undo);
                        crate::explorer::UndoHistory::push(cx, op.verb(), undo, redo, true);
                    }
                }
                Err(error) => {
                    self.name_error = Some(t!("props.rename_failed", error = error).into_owned());
                }
            },
            Msg::App(result) => match result {
                Ok(app) => {
                    self.app_error = None;
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.app = app;
                    }
                }
                Err(error) => self.app_error = Some(error),
            },
            Msg::Native(result) => {
                self.native_error = result.err();
            }
        }
        cx.notify();
    }

    /// The single item now lives at `to`.
    fn renamed(&mut self, to: PathBuf, new_name: &str) {
        new_name.clone_into(&mut self.name);
        if let Some(target) = self.targets.first_mut() {
            target.0.clone_from(&to);
        }
        if let Some(row) = self.loaded.as_mut().and_then(|l| l.rows.first_mut()) {
            row.path = to;
            row.entry.name = Arc::from(new_name);
            row.entry.sort_key = Arc::from(new_name.to_lowercase());
        }
    }

    // ───────────────────────────── Name ─────────────────────────────

    fn single(&self) -> Option<&Row> {
        match (&self.loaded, self.targets.len()) {
            (Some(l), 1) => l.rows.first(),
            _ => None,
        }
    }

    fn revert_name(&mut self, cx: &mut Context<Self>) {
        let name = self.name.clone();
        self.name_input
            .update(cx, |input, cx| input.set_text(name, cx));
        self.name_error = None;
        cx.notify();
    }

    /// Renames to what is typed, if that is a change.
    fn commit_name(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.single().filter(|r| !is_drive_root(&r.path)) else {
            return;
        };
        let typed = self.name_input.read(cx).text().trim().to_owned();
        if typed.is_empty() || typed == self.name {
            return;
        }
        if let Some(problem) = crate::explorer::commit_problem(&typed) {
            self.name_error = Some(problem.to_string());
            cx.notify();
            return;
        }
        let from = row.path.clone();
        let to = from.with_file_name(&typed);
        let op = FileOp::Rename {
            path: from,
            new_name: typed,
        };
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        spawn_thread("properties-rename", move || {
            let (result, undo) = shell.file_op_undoable(&op, None);
            let result = match result {
                Ok(OpOutcome::Done) => Ok(undo),
                // Cancelled in Windows' own dialog (a name that is taken, say): nothing changed.
                Ok(OpOutcome::Cancelled) => return,
                Err(err) => Err(err.to_string()),
            };
            let _ = tx.send(Msg::Renamed { op, to, result });
        });
    }

    // ───────────────────────────── Attributes ─────────────────────────────

    pub(crate) fn attribute(&self, bit: u32) -> Tri {
        let Some(loaded) = self.loaded.as_ref() else {
            return Tri::Off;
        };
        let have = loaded
            .rows
            .iter()
            .filter(|r| r.entry.attributes & bit != 0)
            .count();
        match have {
            0 => Tri::Off,
            n if n == loaded.rows.len() => Tri::On,
            _ => Tri::Mixed,
        }
    }

    /// A click on a check box: on for all unless it is on for all already.
    fn toggle_attribute(&mut self, bit: u32, cx: &mut Context<Self>) {
        let on = self.attribute(bit) != Tri::On;
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let before: Vec<u32> = loaded.rows.iter().map(|r| r.entry.attributes).collect();
        let paths: Vec<PathBuf> = loaded.rows.iter().map(|r| r.path.clone()).collect();
        for row in &mut loaded.rows {
            if on {
                row.entry.attributes |= bit;
            } else {
                row.entry.attributes &= !bit;
            }
        }
        self.attr_error = None;
        let (set, clear) = if on { (bit, 0) } else { (0, bit) };
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        spawn_thread("properties-attributes", move || {
            let result = shell
                .set_attributes(&paths, set, clear)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Attribute { before, result });
        });
        cx.notify();
    }

    // ───────────────────────────── Actions ─────────────────────────────

    fn stop_counting(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.counting = Counting::Stopped;
        cx.notify();
    }

    /// Windows' "Choose another app" dialog; afterwards the name is read again.
    fn change_app(&mut self, window: &Window) {
        let Some(row) = self.single() else {
            return;
        };
        let path = row.path.clone();
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        let owner = Explorer::owner_from(window);
        spawn_thread("properties-open-with", move || {
            let result = shell
                .choose_app(&path, owner)
                .map(|()| shell.default_app(&path))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::App(result));
        });
    }

    fn windows_properties(&self, window: &Window) {
        let paths: Vec<PathBuf> = self.targets.iter().map(|(p, _)| p.clone()).collect();
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        let owner = Explorer::owner_from(window);
        spawn_thread("properties-native", move || {
            let result = shell
                .show_properties(&paths, owner)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Native(result));
        });
    }

    /// Shows `path` in Arke: its folder in the window used last, with the item selected.
    pub(crate) fn reveal(path: &Path, cx: &mut Context<Self>) {
        let (folder, select) = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => (
                parent.to_path_buf(),
                vec![Arc::<str>::from(name.to_string_lossy().as_ref())],
            ),
            _ => (path.to_path_buf(), Vec::new()),
        };
        cx.defer(move |cx| show_in_arke(folder, select, cx));
    }

    fn close(&mut self, _: &CloseProperties, window: &mut Window, _: &mut Context<Self>) {
        self.close_window(window);
    }

    /// Closes the window; a folder still being counted stops with it.
    fn close_window(&self, window: &mut Window) {
        self.cancel.store(true, Ordering::Relaxed);
        window.remove_window();
    }

    // ───────────────────────────── What is shown ─────────────────────────────

    fn rows(&self) -> &[Row] {
        self.loaded.as_ref().map_or(&[], |l| l.rows.as_slice())
    }

    fn folders_only(&self) -> bool {
        !self.rows().is_empty() && self.rows().iter().all(|r| r.entry.is_dir_like())
    }

    /// The size text: counted so far while a folder is still being walked.
    fn size_text(bytes: u64) -> String {
        if bytes < 1024 {
            t!("props.bytes_only", bytes = bytes).into_owned()
        } else {
            t!(
                "props.bytes",
                size = format_size(bytes),
                bytes = format_count(usize::try_from(bytes).unwrap_or(usize::MAX))
            )
            .into_owned()
        }
    }

    fn contains_text(usage: &Usage) -> String {
        t!(
            "props.contains",
            files = tn!(
                "props.n_files",
                usize::try_from(usage.files).unwrap_or(usize::MAX)
            ),
            folders = tn!(
                "props.n_folders",
                usize::try_from(usage.folders).unwrap_or(usize::MAX)
            )
        )
        .into_owned()
    }

    /// The label / value pairs of the info section, in order.
    fn info_rows(&self) -> Vec<InfoRow> {
        let Some(loaded) = self.loaded.as_ref() else {
            return Vec::new();
        };
        let shell = &self.services.shell;
        let mut out = Vec::new();
        let single = self.single();
        if let Some(row) = single.filter(|_| loaded.drive.is_none()) {
            out.push(InfoRow::plain(
                "props-type",
                t!("col.type"),
                type_label(&row.entry),
            ));
        }
        // Where: the shared folder of the items.
        let parents: Vec<&Path> = loaded.rows.iter().filter_map(|r| r.path.parent()).collect();
        if let Some(first) = parents.first()
            && parents.iter().all(|p| p == first)
            && parents.len() == loaded.rows.len()
        {
            out.push(InfoRow::link(
                "props-location",
                t!("col.location"),
                first.display().to_string(),
                Some(row_target(single, first)),
            ));
        }
        // Only folders take a while to count.
        let counting = self.counting == Counting::Running
            && loaded.drive.is_none()
            && self.rows().iter().any(|r| r.entry.is_dir_like());
        let calculating = t!("props.calculating").into_owned();
        // Size: a file's own size is known at once, folders count up as they are walked.
        let (size, on_disk) = match (&self.usage, single) {
            (_, Some(row)) if !row.entry.is_dir_like() => (
                Some(row.entry.size),
                Some(on_disk_of(&row.entry, loaded.details, loaded.cluster)),
            ),
            (Some(usage), _) => (Some(usage.bytes), Some(usage.on_disk)),
            _ => (None, None),
        };
        if loaded.drive.is_none() {
            let mut size_row = InfoRow::plain(
                "props-size",
                t!("col.size"),
                size.map_or_else(
                    || calculating.clone(),
                    |b| {
                        let mut text = Self::size_text(b);
                        if counting {
                            text.push_str(" …");
                        }
                        if self.usage.is_some_and(|u| u.partial) && !counting {
                            text = t!("props.at_least", size = text).into_owned();
                        }
                        text
                    },
                ),
            );
            size_row.stoppable = counting;
            out.push(size_row);
            out.push(InfoRow::plain(
                "props-ondisk",
                t!("props.size_on_disk"),
                on_disk.map_or(calculating, Self::size_text),
            ));
        }
        if self.rows().iter().any(|r| r.entry.is_dir_like()) && loaded.drive.is_none() {
            out.push(InfoRow::plain(
                "props-contains",
                t!("props.contains_label"),
                self.usage
                    .as_ref()
                    .map_or_else(|| t!("props.calculating").into_owned(), Self::contains_text),
            ));
        }
        if let Some(row) = single {
            let time = |t: Option<std::time::SystemTime>| t.map(|t| shell.format_time(t));
            if let Some(text) = time(row.entry.created) {
                out.push(InfoRow::plain("props-created", t!("col.created"), text));
            }
            if let Some(text) = time(row.entry.modified) {
                out.push(InfoRow::plain("props-modified", t!("col.modified"), text));
            }
            if let Some(text) = time(loaded.details.accessed) {
                out.push(InfoRow::plain("props-accessed", t!("props.accessed"), text));
            }
        }
        if loaded.unreadable > 0 {
            out.push(InfoRow::plain(
                "props-unreadable",
                String::new(),
                tn!("props.unreadable", loaded.unreadable).into_owned(),
            ));
        }
        out
    }

    /// The info rows as (id, label, value), for tests.
    #[cfg(test)]
    pub(crate) fn info_for_test(&self) -> Vec<(&'static str, String, String)> {
        self.info_rows()
            .into_iter()
            .map(|r| (r.id, r.label, r.value))
            .collect()
    }

    #[cfg(test)]
    pub(crate) const fn usage_for_test(&self) -> Option<Usage> {
        self.usage
    }

    #[cfg(test)]
    pub(crate) const fn is_counting_for_test(&self) -> bool {
        matches!(self.counting, Counting::Running)
    }

    #[cfg(test)]
    pub(crate) const fn is_stopped_for_test(&self) -> bool {
        matches!(self.counting, Counting::Stopped)
    }

    #[cfg(test)]
    pub(crate) const fn is_loaded_for_test(&self) -> bool {
        self.loaded.is_some()
    }

    #[cfg(test)]
    pub(crate) fn errors_for_test(&self) -> [Option<&str>; 4] {
        [
            self.name_error.as_deref(),
            self.attr_error.as_deref(),
            self.app_error.as_deref(),
            self.native_error.as_deref(),
        ]
    }

    #[cfg(test)]
    pub(crate) fn app_for_test(&self) -> Option<String> {
        self.loaded.as_ref().and_then(|l| l.app.clone())
    }

    #[cfg(test)]
    pub(crate) fn target_for_test(&self) -> Option<PathBuf> {
        self.loaded.as_ref().and_then(|l| l.target.clone())
    }

    #[cfg(test)]
    pub(crate) fn drive_for_test(&self) -> Option<(String, String, u64, u64)> {
        self.loaded
            .as_ref()
            .and_then(|l| l.drive.clone())
            .map(|d| (d.label, d.file_system, d.free, d.total))
    }

    #[cfg(test)]
    pub(crate) fn name_input_for_test(&self) -> Entity<TextInput> {
        self.name_input.clone()
    }

    #[cfg(test)]
    pub(crate) fn cancel_for_test(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    #[cfg(test)]
    pub(crate) fn path_for_test(&self) -> Option<PathBuf> {
        self.targets.first().map(|t| t.0.clone())
    }
}

/// One line of the info section.
struct InfoRow {
    id: &'static str,
    label: String,
    value: String,
    /// Clicking the value shows this in Arke.
    link: Option<PathBuf>,
    /// A "Stop" link follows while it is still being counted.
    stoppable: bool,
}

impl InfoRow {
    fn plain(id: &'static str, label: impl std::fmt::Display, value: String) -> Self {
        Self {
            id,
            label: label.to_string(),
            value,
            link: None,
            stoppable: false,
        }
    }

    fn link(
        id: &'static str,
        label: impl std::fmt::Display,
        value: String,
        link: Option<PathBuf>,
    ) -> Self {
        Self {
            link,
            ..Self::plain(id, label, value)
        }
    }
}

/// What clicking the location shows: the item itself when there is one, else the folder.
fn row_target(single: Option<&Row>, folder: &Path) -> PathBuf {
    single.map_or_else(|| folder.to_path_buf(), |r| r.path.clone())
}

/// Brings Arke's window used last to the front on `folder`, with `select` selected.
fn show_in_arke(folder: PathBuf, select: Vec<Arc<str>>, cx: &mut App) {
    let last = cx
        .try_global::<crate::workspace::LastActiveWindow>()
        .and_then(|l| l.0)
        .and_then(|w| w.downcast::<crate::workspace::Workspace>());
    let window = last.or_else(|| {
        cx.windows()
            .into_iter()
            .rev()
            .find_map(|w| w.downcast::<crate::workspace::Workspace>())
    });
    let Some(window) = window else {
        return;
    };
    let _ = window.update(cx, |workspace, window, cx| {
        workspace.open_external(Target::Folder(folder), select, window, cx);
        window.activate_window();
    });
}

// ───────────────────────────── Background reads ─────────────────────────────

/// Reads what the window shows about `targets`. Background thread.
fn read_items(services: &Services, targets: &[(PathBuf, bool)]) -> Loaded {
    let shell = &services.shell;
    let root = targets.first().and_then(|(p, _)| root_of(p));
    let volume = root.as_deref().and_then(|r| shell.volume_info(r));
    let cluster = volume.as_ref().map_or(0, |v| v.cluster);
    let mut rows = Vec::new();
    let mut unreadable = 0;
    for (path, _) in targets {
        if is_drive_root(path) {
            rows.push(Row {
                path: path.clone(),
                entry: Entry::new(path.display().to_string(), EntryKind::Dir, 0, None, 0),
            });
            continue;
        }
        match services.fs.stat(path) {
            Ok(entry) => rows.push(Row {
                path: path.clone(),
                entry,
            }),
            Err(err) => {
                log::info!("properties: cannot read {}: {err}", path.display());
                unreadable += 1;
            }
        }
    }
    let single = (targets.len() == 1).then(|| rows.first()).flatten();
    let details = single
        .filter(|r| !is_drive_root(&r.path))
        .map_or_default(|r| shell.file_details(&r.path));
    let target = single
        .filter(|r| r.entry.extension().eq_ignore_ascii_case("lnk"))
        .and_then(|r| shell.shortcut_target(&r.path))
        .map(|(target, _)| target);
    let app = single
        .filter(|r| !r.entry.is_dir_like())
        .and_then(|r| shell.default_app(&r.path));
    let drive = single.filter(|r| is_drive_root(&r.path)).map(|r| {
        let (free, total) = shell
            .disk_spaces(std::slice::from_ref(&r.path))
            .first()
            .map_or((0, 0), |(_, space)| *space);
        let volume = volume.clone().unwrap_or_default();
        Drive {
            label: volume.label,
            file_system: volume.file_system,
            free,
            total,
        }
    });
    let icon = single.and_then(|r| icon_of(shell.as_ref(), r));
    Loaded {
        rows,
        unreadable,
        details,
        target,
        app,
        drive,
        cluster,
        icon,
    }
}

/// The icon Windows has for the item, for the kinds the list asks it for too. Never opens a cloud
/// file that is not on this device (`wants_system_icon` leaves those out).
fn icon_of(shell: &dyn crate::shell::Shell, row: &Row) -> Option<Arc<RenderImage>> {
    let thumb = if wants_system_icon(&row.entry) {
        shell.file_icon(&row.path, crate::media::SYSTEM_ICON_SIZE)
    } else if wants_type_icon(&row.entry) {
        shell.type_icon(row.entry.extension())
    } else {
        None
    };
    thumb.and_then(crate::media::to_render_image)
}

/// What one file takes on disk: Windows' own number when it gave one, else estimated from the
/// cluster size. A cloud file that is not on this device takes nothing, whatever was asked.
fn on_disk_of(entry: &Entry, details: FileDetails, cluster: u64) -> u64 {
    if entry.is_cloud_placeholder() {
        return 0;
    }
    details
        .size_on_disk
        .unwrap_or_else(|| dirsize::file_on_disk(entry, cluster))
}

/// Counts the files and folders under the items and reports as it goes. Background thread.
fn count_usage(
    services: &Services,
    rows: &[Row],
    details: FileDetails,
    cluster: u64,
    cancel: &AtomicBool,
    tx: &flume::Sender<Msg>,
) {
    let single = rows.len() == 1;
    // Files in the selection itself are known from the listing.
    let mut base = Usage::default();
    for row in rows.iter().filter(|r| !r.entry.is_dir_like()) {
        base.bytes += row.entry.size;
        base.on_disk += if single {
            on_disk_of(&row.entry, details, cluster)
        } else {
            dirsize::file_on_disk(&row.entry, cluster)
        };
        base.files += 1;
    }
    let mut done = base;
    let mut last_sent: Option<Instant> = None;
    for row in rows.iter().filter(|r| r.entry.is_dir_like()) {
        let before = done;
        let walked = dirsize::usage(&*services.fs, &row.path, cluster, cancel, &mut |so_far| {
            let mut shown = before;
            shown += *so_far;
            if last_sent.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                last_sent = Some(Instant::now());
                let _ = tx.send(Msg::Usage(shown, Counting::Running));
            }
        });
        match walked {
            Some(usage) => done += usage,
            // Stopped: the window already knows.
            None => return,
        }
    }
    let _ = tx.send(Msg::Usage(done, Counting::Done));
}

// ───────────────────────────── Drawing ─────────────────────────────

impl Focusable for Properties {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(theme::S_8)
        .p(theme::S_12)
        .rounded(theme::R_12)
        .bg(theme::card())
        .border_1()
        .border_color(theme::line())
}

fn label_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .w(LABEL_W)
        .flex_none()
        .text_size(theme::FS_13)
        .text_color(theme::text_muted())
        .child(text.into())
}

fn problem_text(id: &'static str, text: &str) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_live(gpui::Role::Alert, text.to_owned())
        .text_size(theme::FS_12)
        .text_color(theme::danger())
        .child(text.to_owned())
        .into_any_element()
}

fn button(
    id: &'static str,
    label: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = SharedString::from(label.to_string());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_button(label.clone())
        .h(theme::H_CTL_SM)
        .px(theme::S_12)
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text())
        .hover(|s| s.bg(theme::wash()))
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

fn check_box(state: Tri) -> gpui::Div {
    let on = state != Tri::Off;
    div()
        .size(theme::ICON)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .border_1()
        .when(on, |d| d.bg(theme::signal()).border_color(theme::signal()))
        .when(!on, |d| {
            d.bg(theme::surface()).border_color(theme::text_subtle())
        })
        .children(match state {
            Tri::On => Some(
                svg()
                    .path(Icon::Check.path())
                    .size(px(12.))
                    .text_color(theme::on_signal())
                    .into_any_element(),
            ),
            Tri::Mixed => Some(
                div()
                    .w(px(8.))
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(theme::on_signal())
                    .into_any_element(),
            ),
            Tri::Off => None,
        })
}

impl Properties {
    fn header(&self) -> AnyElement {
        let loaded = self.loaded.as_ref();
        let single = self.single();
        let icon: AnyElement = match (loaded.and_then(|l| l.icon.clone()), single) {
            (Some(image), _) => img(image).size(theme::ICON_HERO / 2.).into_any_element(),
            (None, Some(_)) if loaded.is_some_and(|l| l.drive.is_some()) => svg()
                .path(Icon::Drive.path())
                .size(theme::ICON_HERO / 2.)
                .text_color(theme::text_muted())
                .into_any_element(),
            (None, Some(row)) => kind_icon(file_kind(&row.entry), theme::ICON_HERO / 2.),
            (None, None) => kind_icon(
                if self.folders_only() {
                    FileKind::Folder
                } else {
                    FileKind::Other
                },
                theme::ICON_HERO / 2.,
            ),
        };
        let editable = single.is_some_and(|r| !is_drive_root(&r.path));
        let title: AnyElement = if editable {
            let problem = self.name_error.is_some();
            div()
                .id("props-name")
                .debug_selector(|| "props-name".to_owned())
                .flex_1()
                .min_w_0()
                .h(theme::H_CTL)
                .px(theme::S_8)
                .flex()
                .items_center()
                .rounded(theme::R_8)
                .bg(theme::surface())
                .border_1()
                .border_color(if problem {
                    theme::danger()
                } else {
                    theme::line_strong()
                })
                .text_size(theme::FS_15)
                .child(self.name_input.clone())
                .into_any_element()
        } else {
            let text = match (single, loaded.and_then(|l| l.drive.as_ref())) {
                (_, Some(drive)) if !drive.label.is_empty() => {
                    format!("{} ({})", drive.label, self.name)
                }
                (Some(_), _) => self.name.clone(),
                _ => tn!("props.items", self.targets.len()).into_owned(),
            };
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme::FS_15)
                .text_color(theme::text())
                .truncate()
                .child(text)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(theme::S_6)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_12)
                    .child(div().flex_none().child(icon))
                    .child(title),
            )
            .when_some(self.name_error.as_ref(), |d, e| {
                d.child(problem_text("props-name-error", e))
            })
            .into_any_element()
    }

    fn info_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.info_rows();
        let mut section = card();
        for row in rows {
            let id = row.id;
            let value: AnyElement = match row.link {
                Some(path) => div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .a11y_button(row.value.clone())
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .text_size(theme::FS_13)
                    .text_color(theme::signal())
                    .hover(gpui::Styled::underline)
                    .child(row.value)
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        Self::reveal(&path, cx);
                    }))
                    .into_any_element(),
                None => div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .min_w_0()
                    .text_size(theme::FS_13)
                    .text_color(theme::text())
                    .child(row.value)
                    .into_any_element(),
            };
            section = section.child(
                div()
                    .flex()
                    .items_start()
                    .gap(theme::S_8)
                    .child(label_text(row.label))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(theme::S_8)
                            .child(value)
                            .when(row.stoppable, |d| {
                                d.child(
                                    div()
                                        .id("props-stop")
                                        .debug_selector(|| "props-stop".to_owned())
                                        .a11y_button(t!("props.stop").to_string())
                                        .flex_none()
                                        .cursor_pointer()
                                        .text_size(theme::FS_12)
                                        .text_color(theme::signal())
                                        .hover(gpui::Styled::underline)
                                        .child(t!("props.stop").to_string())
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.stop_counting(cx);
                                        })),
                                )
                            }),
                    ),
            );
        }
        section.into_any_element()
    }

    fn attribute_row(
        &self,
        id: &'static str,
        label: String,
        bit: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.attribute(bit);
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .a11y_toggle(label.clone(), state == Tri::On)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .h(theme::H_ROW_COMPACT)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.toggle_attribute(bit, cx);
                }),
            )
            .child(check_box(state))
            .child(
                div()
                    .text_size(theme::FS_13)
                    .text_color(theme::text())
                    .child(label),
            )
            .into_any_element()
    }

    fn attributes_section(&self, cx: &mut Context<Self>) -> AnyElement {
        card()
            .child(self.attribute_row(
                "props-readonly",
                t!("props.read_only").into_owned(),
                ATTR_READONLY,
                cx,
            ))
            .child(self.attribute_row(
                "props-hidden",
                t!("props.hidden").into_owned(),
                ATTR_HIDDEN,
                cx,
            ))
            .when_some(self.attr_error.as_ref(), |d, e| {
                d.child(problem_text("props-attr-error", e))
            })
            .into_any_element()
    }

    fn opens_with_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.single().filter(|r| !r.entry.is_dir_like())?;
        let app = self
            .loaded
            .as_ref()
            .and_then(|l| l.app.clone())
            .unwrap_or_else(|| t!("props.no_app").into_owned());
        Some(
            card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::S_8)
                        .child(label_text(t!("props.opens_with").to_string()))
                        .child(
                            div()
                                .id("props-app")
                                .debug_selector(|| "props-app".to_owned())
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(theme::FS_13)
                                .text_color(theme::text())
                                .child(app),
                        )
                        .child(button(
                            "props-change-app",
                            t!("props.change"),
                            cx.listener(|this, _: &ClickEvent, window, _| {
                                this.change_app(window);
                            }),
                        )),
                )
                .when_some(self.app_error.as_ref(), |d, e| {
                    d.child(problem_text("props-app-error", e))
                })
                .into_any_element(),
        )
    }

    fn shortcut_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let target = self.loaded.as_ref()?.target.clone()?;
        let shown = target.display().to_string();
        Some(
            card()
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(theme::S_8)
                        .child(label_text(t!("props.shortcut_target").to_string()))
                        .child(
                            div()
                                .id("props-target")
                                .debug_selector(|| "props-target".to_owned())
                                .a11y_button(shown.clone())
                                .flex_1()
                                .min_w_0()
                                .cursor_pointer()
                                .text_size(theme::FS_13)
                                .text_color(theme::signal())
                                .hover(gpui::Styled::underline)
                                .child(shown)
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                    Self::reveal(&target, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn drive_section(&self) -> Option<AnyElement> {
        let drive = self.loaded.as_ref()?.drive.clone()?;
        let used = drive.total.saturating_sub(drive.free);
        let fraction = if drive.total == 0 {
            0.
        } else {
            (used as f32 / drive.total as f32).clamp(0., 1.)
        };
        let stat = |label: String, bytes: u64| {
            div()
                .flex()
                .items_center()
                .gap(theme::S_8)
                .child(label_text(label))
                .child(
                    div()
                        .text_size(theme::FS_13)
                        .text_color(theme::text())
                        .child(Self::size_text(bytes)),
                )
        };
        Some(
            card()
                .child(
                    div()
                        .id("props-capacity")
                        .debug_selector(|| "props-capacity".to_owned())
                        .h(px(10.))
                        .w_full()
                        .rounded(theme::R_FULL)
                        .bg(theme::wash_strong())
                        .overflow_hidden()
                        .child(
                            div()
                                .id("props-capacity-used")
                                .debug_selector(|| "props-capacity-used".to_owned())
                                .h_full()
                                .w(gpui::relative(fraction))
                                .rounded(theme::R_FULL)
                                .bg(theme::signal()),
                        ),
                )
                .child(stat(t!("props.used").into_owned(), used))
                .child(stat(t!("props.free").into_owned(), drive.free))
                .child(stat(t!("props.capacity").into_owned(), drive.total))
                .when(!drive.file_system.is_empty(), |d| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme::S_8)
                            .child(label_text(t!("props.file_system").to_string()))
                            .child(
                                div()
                                    .id("props-file-system")
                                    .debug_selector(|| "props-file-system".to_owned())
                                    .text_size(theme::FS_13)
                                    .text_color(theme::text())
                                    .child(drive.file_system.clone()),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(theme::S_6)
            .when_some(self.native_error.as_ref(), |d, e| {
                d.child(problem_text("props-native-error", e))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(button(
                        "props-native",
                        t!("props.windows_properties"),
                        cx.listener(|this, _: &ClickEvent, window, _| {
                            this.windows_properties(window);
                        }),
                    ))
                    .child(button(
                        "props-close",
                        t!("props.close"),
                        cx.listener(|this, _: &ClickEvent, window, _| {
                            this.close_window(window);
                        }),
                    )),
            )
            .into_any_element()
    }
}

impl Render for Properties {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.loaded.is_some() {
            let drive = self.drive_section();
            let is_drive = drive.is_some();
            div()
                .flex()
                .flex_col()
                .gap(theme::S_12)
                .child(self.header())
                .child(self.info_section(cx))
                .children(drive)
                .when(!is_drive, |d| d.child(self.attributes_section(cx)))
                .children(self.opens_with_section(cx))
                .children(self.shortcut_section(cx))
                .into_any_element()
        } else {
            div()
                .text_size(theme::FS_13)
                .text_color(theme::text_muted())
                .child(t!("props.calculating").to_string())
                .into_any_element()
        };
        div()
            .id("properties")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::frame_solid())
            .ui_font()
            .text_size(theme::FS_13)
            .text_color(theme::text())
            .child(
                div()
                    .id("props-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(theme::S_16)
                    .child(body),
            )
            .child(
                div()
                    .flex_none()
                    .px(theme::S_16)
                    .py(theme::S_12)
                    .border_t_1()
                    .border_color(theme::line())
                    .child(self.footer(cx)),
            )
    }
}
