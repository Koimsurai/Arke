//! Arke's own Properties window (Alt+Enter, "Properties" in the menus).
//!
//! One scrolling page in a window of its own (several can be open), built like the main window:
//! the same material and title bar, cards like the settings page. From the top: what the item is
//! (a picture or icon, the name, a line or two about it), quick actions, then cards for the
//! general facts, the times, attributes and security, and, for folders, drives and several items,
//! where the space goes. Everything applies the moment it is changed, nothing is written that the
//! user did not change, and a failure shows under the thing that failed. The pages only Windows
//! has (security, sharing, previous versions, ...) are one button away.
//!
//! Nothing here reads file contents unasked: sizes, times and attributes come from listings and
//! `GetCompressedFileSize`, so a cloud file that is not on this device is never downloaded. The
//! only reads are the picture of an image or video (never of a cloud file) and a checksum the
//! user asks for.

use std::cell::Cell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arke_core::dirsize::{self, Category, Space, Usage};
use arke_core::exif::PhotoInfo;
use arke_core::filetype::{
    FileKind, can_thumbnail, can_video_thumbnail, file_kind, wants_system_icon, wants_type_icon,
};
use arke_core::format::{format_count, format_size, type_label};
use arke_core::takeover::Target;
use arke_core::{Entry, EntryKind};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyBinding, MouseButton, ObjectFit, ParentElement, Render, RenderImage, SharedString, Styled,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowHandle,
    WindowOptions, actions, div, img, prelude::*, px, relative, size, svg,
};
use rust_i18n::t;
use sha2::{Digest, Sha256};

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{Explorer, Services, caption_buttons, kind_icon, spawn_thread};
use crate::settings::{PropertiesPlace, SettingsStore};
use crate::shell::{ATTR_HIDDEN, ATTR_READONLY, FileDetails, FileOp, OpOutcome, Place};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

actions!(properties, [CloseProperties]);

const CONTEXT: &str = "Properties";

/// Window width and the height it starts with; it then grows to fit what is shown, up to the
/// height of the screen (only what does not fit on the screen scrolls).
const WIDTH: f32 = 760.;
const START_HEIGHT: f32 = 520.;
/// The window is never smaller than this, and keeps this much of the screen free.
const MIN_WIDTH: f32 = 620.;
const MIN_HEIGHT: f32 = 440.;
/// From this width on the cards sit in two columns, below it in one.
const TWO_COLUMNS_FROM: f32 = 680.;
const SCREEN_MARGIN: f32 = 72.;

/// One of the sizes above at the interface scale.
fn sc(base: f32) -> f32 {
    f32::from(theme::scaled_px(base))
}
/// Longest wait between two updates of a folder size that is still being counted.
const PROGRESS_EVERY: Duration = Duration::from_millis(120);
/// Width of the labels in the rows.
const LABEL_W: gpui::Pixels = px(100.);
/// Picture or icon of the item at the top.
const HERO: gpui::Pixels = px(96.);
/// The picture is decoded this big (device pixels).
const HERO_DECODE: u32 = 320;
/// How much of a file a checksum reads at a time.
const HASH_CHUNK: usize = 256 * 1024;
/// Estimated heights, to size the window to its content.
const EST_TITLE: f32 = 44.;
const EST_FOOTER: f32 = 56.;
const EST_NAME: f32 = 40.;
const EST_LINE: f32 = 20.;
const HERO_H: f32 = 96.;
const EST_ACTIONS: f32 = 44.;
const EST_HEADING: f32 = 34.;
const EST_GAP: f32 = 14.;
/// How long after the window resized itself a size change is still its own.
const OWN_RESIZE: Duration = Duration::from_millis(500);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseProperties, Some(CONTEXT))]);
}

/// The item a window shows.
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

/// What a photo says about itself.
struct PhotoMeta {
    width: u32,
    height: u32,
    info: Option<PhotoInfo>,
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
    /// The picture is of the content (a photo, a frame of a video), not an icon.
    picture: bool,
    photo: Option<PhotoMeta>,
    /// The file came from the internet (`Zone.Identifier`).
    blocked: bool,
    places: Vec<Place>,
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

/// A checksum, asked for with a click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HashState {
    Idle,
    /// Percent read so far.
    Running(u8),
    Done(String),
    Failed(String),
}

/// What background work reports back.
enum Msg {
    Loaded(Box<Loaded>),
    Space(Space, Counting),
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
    Hash(HashState),
    Unblocked(Result<(), String>),
    /// Reading this drive's file table took this long (ms): remembered for the next count.
    TableTime(String, crate::settings::TableTime),
    /// An action from the quick row that failed.
    Action(Result<(), String>),
}

pub(crate) struct Properties {
    services: Services,
    /// The search index, which answers a folder's space at once when it follows its drive live
    /// (`IndexService::usage_below`); otherwise the folder is walked.
    index: Option<Arc<arke_core::index_service::IndexService>>,
    /// How long reading each drive's file table took before (`Settings::mft_list`).
    table_times: std::collections::BTreeMap<String, crate::settings::TableTime>,
    focus: FocusHandle,
    /// The items asked about, with whether each is a folder.
    targets: Vec<(PathBuf, bool)>,
    loaded: Option<Loaded>,
    name_input: Entity<TextInput>,
    /// The name as it is on disk (what the box goes back to on Esc).
    name: String,
    space: Option<Space>,
    counting: Counting,
    cancel: Arc<AtomicBool>,
    hash: HashState,
    hash_cancel: Arc<AtomicBool>,
    /// Attribute changes on a folder reach what is inside too.
    recursive: bool,
    tx: flume::Sender<Msg>,
    name_error: Option<String>,
    attr_error: Option<String>,
    app_error: Option<String>,
    native_error: Option<String>,
    action_error: Option<String>,
    unblock_error: Option<String>,
    /// The height the window last sized itself to, when and whether the user took over.
    fit_height: f32,
    fit_at: Instant,
    user_sized: bool,
    /// Height of what is drawn, as worked out while drawing.
    estimate: Cell<f32>,
    /// How many columns the cards were last laid out in.
    columns: Cell<u8>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for Properties {
    fn drop(&mut self) {
        // A folder still being counted, or a checksum under way, stops with the window.
        self.cancel.store(true, Ordering::Relaxed);
        self.hash_cancel.store(true, Ordering::Relaxed);
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
    let bounds = start_bounds(cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Like the main window: the title bar is drawn by us.
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                appears_transparent: true,
                ..Default::default()
            }),
            focus: true,
            window_min_size: Some(size(px(sc(MIN_WIDTH)), px(sc(MIN_HEIGHT)))),
            window_background: if theme::mica() {
                gpui::WindowBackgroundAppearance::MicaBackdrop
            } else {
                gpui::WindowBackgroundAppearance::Opaque
            },
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

/// Where a new window goes: where the last one was if that is still on a screen, else centered.
pub(crate) fn start_bounds(cx: &mut App) -> Bounds<gpui::Pixels> {
    let saved = SettingsStore::global(cx)
        .read(cx)
        .settings
        .properties_window;
    let height = saved
        .and_then(|p| p.height)
        .unwrap_or_else(|| sc(START_HEIGHT));
    // A width saved by the old, narrow window would only give a single column: start afresh.
    let width = saved
        .filter(|p| p.width >= sc(MIN_WIDTH))
        .map_or_else(|| sc(WIDTH), |p| p.width.min(sc(1200.)));
    let fresh = Bounds::centered(None, size(px(width), px(height)), cx);
    let Some(place) = saved else {
        return fresh;
    };
    let origin = gpui::point(px(place.x), px(place.y));
    // A corner of the title bar must be on some screen (a monitor may have been unplugged).
    let grab = origin + gpui::point(px(40.), px(20.));
    if cx.displays().iter().any(|d| d.bounds().contains(&grab)) {
        Bounds::new(origin, size(px(width), px(height)))
    } else {
        fresh
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

fn count_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

impl Properties {
    pub(crate) fn new(
        services: Services,
        targets: Vec<(PathBuf, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Light or dark, the accent and the material, as the main window has them.
        crate::workspace::apply_theme(window, cx);
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
        let user_sized = SettingsStore::global(cx)
            .read(cx)
            .settings
            .properties_window
            .is_some_and(|p| p.height.is_some());
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
            cx.observe_window_appearance(window, |_, window, cx| {
                crate::workspace::apply_theme(window, cx);
                window.refresh();
            }),
            cx.observe_window_bounds(window, |this, window, cx| this.bounds_changed(window, cx)),
        ];
        let this = Self {
            services,
            index: crate::global_search::GlobalSearch::local_service(cx),
            table_times: crate::settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .mft_list
                .clone(),
            focus,
            targets,
            loaded: None,
            name_input,
            name,
            space: None,
            counting: Counting::Running,
            cancel: Arc::new(AtomicBool::new(false)),
            hash: HashState::Idle,
            hash_cancel: Arc::new(AtomicBool::new(false)),
            recursive: false,
            tx,
            name_error: None,
            attr_error: None,
            app_error: None,
            native_error: None,
            action_error: None,
            unblock_error: None,
            fit_height: 0.,
            fit_at: Instant::now(),
            user_sized,
            estimate: Cell::new(0.),
            columns: Cell::new(0),
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

    // ───────────────────────────── Window ─────────────────────────────

    /// The window moved or changed size: remember where it is, and whether the user sized it
    /// (a change the window did not make itself).
    fn bounds_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.fit_at.elapsed() < OWN_RESIZE {
            return;
        }
        let bounds = window.bounds();
        let height = f32::from(bounds.size.height);
        if self.fit_height > 0. && (height - self.fit_height).abs() > 3. {
            self.user_sized = true;
        }
        let place = PropertiesPlace {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: self.user_sized.then_some(height),
        };
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(|s| s.properties_window = Some(place), cx);
        });
    }

    /// Sizes the window to what is drawn, unless the user sized it: as tall as the content, up
    /// to the screen it is on. A window that would grow past the bottom of the screen moves up.
    fn fit_window(&mut self, window: &mut Window, cx: &App) {
        if self.user_sized || self.loaded.is_none() {
            return;
        }
        let screen = window.display(cx).or_else(|| cx.primary_display());
        let area = screen.as_ref().map(|d| d.visible_bounds());
        let room = area.map_or(1000., |a| f32::from(a.size.height)) - SCREEN_MARGIN;
        let want = self
            .estimate
            .get()
            .clamp(sc(MIN_HEIGHT), room.max(sc(MIN_HEIGHT)));
        if (want - self.fit_height).abs() <= 6. {
            return;
        }
        self.fit_height = want;
        self.fit_at = Instant::now();
        let bounds = window.bounds();
        if let (Some(screen), Some(area)) = (screen, area) {
            let top = area.origin.y + px(SCREEN_MARGIN / 2.);
            let bottom = area.origin.y + area.size.height - px(SCREEN_MARGIN / 2.);
            // gpui can resize a window but not move it.
            if bounds.origin.y + px(want) > bottom
                && let Some(hwnd) = crate::explorer::Explorer::owner_from(window)
            {
                let y = (bottom - px(want)).max(top);
                let to = Bounds::new(
                    gpui::point(bounds.origin.x, y),
                    size(bounds.size.width, px(want)),
                );
                // Not while this frame is drawn: the window reads its new size from the
                // message Windows sends it, which would arrive in the middle of the frame.
                let (monitor, screen) = (u64::from(screen.id()), screen.bounds());
                cx.foreground_executor()
                    .spawn(async move { crate::win::move_window(hwnd, monitor, screen, to) })
                    .detach();
                return;
            }
        }
        window.resize(size(bounds.size.width, px(want)));
    }

    // ───────────────────────────── Reading ─────────────────────────────

    fn start_loading(&self) {
        let services = self.services.clone();
        let index = self.index.clone();
        let table_times = self.table_times.clone();
        let targets = self.targets.clone();
        let tx = self.tx.clone();
        let cancel = Arc::clone(&self.cancel);
        spawn_thread("properties-load", move || {
            let loaded = read_items(&services, &targets);
            let cluster = loaded.cluster;
            let rows: Vec<Row> = loaded.rows.clone();
            let details = loaded.details;
            let _ = tx.send(Msg::Loaded(Box::new(loaded)));
            let table = table_reader();
            let profiles = service_profiles();
            // Shared by the folders of this window: what one teaches is used for the next.
            let table_times = parking_lot::Mutex::new(table_times);
            let fast = Fast {
                index: index.as_deref(),
                table: table.as_deref(),
                table_times: &table_times,
                profiles: profiles.as_ref(),
                seeks: crate::win_eject::seeks,
            };
            count_space(&services, &fast, &rows, details, cluster, &cancel, &tx);
        });
    }

    fn handle(&mut self, msg: Msg, cx: &mut Context<Self>) {
        match msg {
            Msg::Loaded(loaded) => {
                self.loaded = Some(*loaded);
            }
            Msg::TableTime(volume, time) => {
                use crate::settings::TableTime;
                TableTime::learn(&mut self.table_times, volume.clone(), time);
                crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(|s| TableTime::learn(&mut s.mft_list, volume, time), cx);
                });
            }
            Msg::Space(space, counting) => {
                // Stopped by the user: what comes in late is dropped.
                if self.counting == Counting::Stopped {
                    return;
                }
                self.space = Some(space);
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
            Msg::Native(result) => self.native_error = result.err(),
            Msg::Hash(state) => {
                // Cancelled meanwhile: what arrives late is dropped.
                if matches!(self.hash, HashState::Running(_)) {
                    self.hash = state;
                }
            }
            Msg::Unblocked(result) => match result {
                Ok(()) => {
                    self.unblock_error = None;
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.blocked = false;
                    }
                }
                Err(error) => self.unblock_error = Some(error),
            },
            Msg::Action(result) => self.action_error = result.err(),
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

    /// A click on a check box: on for all unless it is on for all already. With "apply to items
    /// inside" on, folders pass it on to everything in them.
    fn toggle_attribute(&mut self, bit: u32, cx: &mut Context<Self>) {
        let on = self.attribute(bit) != Tri::On;
        let recursive = self.recursive;
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let before: Vec<u32> = loaded.rows.iter().map(|r| r.entry.attributes).collect();
        let roots: Vec<(PathBuf, bool)> = loaded
            .rows
            .iter()
            .map(|r| (r.path.clone(), r.entry.is_dir_like()))
            .collect();
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
        let fs = Arc::clone(&self.services.fs);
        let tx = self.tx.clone();
        spawn_thread("properties-attributes", move || {
            let mut paths: Vec<PathBuf> = roots.iter().map(|(p, _)| p.clone()).collect();
            if recursive {
                for (path, is_dir) in &roots {
                    if *is_dir {
                        paths.extend(descendants(&*fs, path));
                    }
                }
            }
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
        cx.defer(move |cx| show_in_arke(folder, select, false, cx));
    }

    /// "Open": a file with its program, a folder or drive in Arke.
    pub(crate) fn open_item(&self, cx: &mut Context<Self>) {
        let Some(row) = self.single() else {
            return;
        };
        let path = row.path.clone();
        if row.entry.is_dir_like() {
            cx.defer(move |cx| show_in_arke(path, Vec::new(), false, cx));
            return;
        }
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        spawn_thread("properties-open", move || {
            let result = shell.open(&path, None).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Action(result));
        });
    }

    /// "Open in new tab": the folder, in a tab of the window used last.
    pub(crate) fn open_in_new_tab(&self, cx: &mut Context<Self>) {
        if let Some(row) = self.single() {
            let path = row.path.clone();
            cx.defer(move |cx| show_in_arke(path, Vec::new(), true, cx));
        }
    }

    /// "Copy path": quoted, one per line, as the menu does.
    pub(crate) fn copy_path(&self) {
        let text = self
            .targets
            .iter()
            .map(|(p, _)| format!("\"{}\"", p.display()))
            .collect::<Vec<_>>()
            .join("\r\n");
        self.copy_text(text);
    }

    fn copy_text(&self, text: String) {
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        spawn_thread("properties-copy", move || {
            let result = shell.set_clipboard_text(&text).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Action(result));
        });
    }

    /// Pins the folder (or drive) to the favorites, or takes it off them.
    pub(crate) fn toggle_favorite(&self, cx: &mut Context<Self>) {
        let (Some(row), Some(loaded)) = (self.single(), self.loaded.as_ref()) else {
            return;
        };
        let path = row.path.clone();
        let places = loaded.places.clone();
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    s.toggle_pin(&path, &places);
                },
                cx,
            );
        });
    }

    fn is_favorite(&self, cx: &mut App) -> bool {
        let (Some(row), Some(loaded)) = (self.single(), self.loaded.as_ref()) else {
            return false;
        };
        SettingsStore::global(cx)
            .read(cx)
            .settings
            .is_pinned(&row.path, &loaded.places)
    }

    /// Removes the internet mark.
    pub(crate) fn unblock(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.single() else {
            return;
        };
        let path = row.path.clone();
        let shell = Arc::clone(&self.services.shell);
        let tx = self.tx.clone();
        self.unblock_error = None;
        spawn_thread("properties-unblock", move || {
            let result = shell.unblock(&path).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Unblocked(result));
        });
        cx.notify();
    }

    /// Starts the SHA-256 of the file. Reads it in pieces, so it can be cancelled.
    pub(crate) fn start_hash(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self
            .single()
            .filter(|r| !r.entry.is_dir_like() && !r.entry.is_cloud_placeholder())
        else {
            return;
        };
        if matches!(self.hash, HashState::Running(_)) {
            return;
        }
        let path = row.path.clone();
        let total = row.entry.size;
        let fs = Arc::clone(&self.services.fs);
        let tx = self.tx.clone();
        self.hash_cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.hash_cancel = Arc::clone(&cancel);
        self.hash = HashState::Running(0);
        spawn_thread("properties-hash", move || {
            let state = match hash_file(&*fs, &path, total, &cancel, &mut |percent| {
                let _ = tx.send(Msg::Hash(HashState::Running(percent)));
            }) {
                Ok(Some(hex)) => HashState::Done(hex),
                // Cancelled: the window already knows.
                Ok(None) => return,
                Err(err) => HashState::Failed(err.to_string()),
            };
            let _ = tx.send(Msg::Hash(state));
        });
        cx.notify();
    }

    pub(crate) fn cancel_hash(&mut self, cx: &mut Context<Self>) {
        self.hash_cancel.store(true, Ordering::Relaxed);
        if matches!(self.hash, HashState::Running(_)) {
            self.hash = HashState::Idle;
        }
        cx.notify();
    }

    fn close(&mut self, _: &CloseProperties, window: &mut Window, _: &mut Context<Self>) {
        self.close_window(window);
    }

    /// Closes the window; a folder still being counted stops with it.
    fn close_window(&self, window: &mut Window) {
        self.cancel.store(true, Ordering::Relaxed);
        self.hash_cancel.store(true, Ordering::Relaxed);
        window.remove_window();
    }

    // ───────────────────────────── What is shown ─────────────────────────────

    fn rows(&self) -> &[Row] {
        self.loaded.as_ref().map_or(&[], |l| l.rows.as_slice())
    }

    fn any_folder(&self) -> bool {
        self.rows().iter().any(|r| r.entry.is_dir_like())
    }

    fn usage(&self) -> Option<&Usage> {
        self.space.as_ref().map(|s| &s.usage)
    }

    fn is_drive(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.drive.is_some())
    }

    /// Whether the items are still being walked (only folders take a while).
    fn walking(&self) -> bool {
        self.counting == Counting::Running && self.any_folder()
    }

    /// The size text: counted so far while a folder is still being walked.
    fn size_text(bytes: u64) -> String {
        if bytes < 1024 {
            t!("props.bytes_only", bytes = bytes).into_owned()
        } else {
            t!(
                "props.bytes",
                size = format_size(bytes),
                bytes = format_count(count_usize(bytes))
            )
            .into_owned()
        }
    }

    fn contains_text(usage: &Usage) -> String {
        t!(
            "props.contains",
            files = tn!("props.n_files", count_usize(usage.files)),
            folders = tn!("props.n_folders", count_usize(usage.folders))
        )
        .into_owned()
    }

    /// Bytes of the single file, or of everything counted so far.
    fn size_bytes(&self) -> Option<u64> {
        match self.single() {
            Some(row) if !row.entry.is_dir_like() => Some(row.entry.size),
            _ => self.usage().map(|u| u.bytes),
        }
    }

    /// The lines under the name: what it is, how big, and what is worth knowing at a glance.
    fn summary(&self) -> Vec<String> {
        let Some(loaded) = self.loaded.as_ref() else {
            return Vec::new();
        };
        let dot = " · ";
        let mut lines = Vec::new();
        if let Some(drive) = loaded.drive.as_ref() {
            let mut parts = Vec::new();
            if !drive.file_system.is_empty() {
                parts.push(drive.file_system.clone());
            }
            if drive.total > 0 {
                parts.push(
                    t!(
                        "props.used_of",
                        used = format_size(drive.total.saturating_sub(drive.free)),
                        total = format_size(drive.total)
                    )
                    .into_owned(),
                );
            }
            lines.push(parts.join(dot));
            return lines;
        }
        let size = self.size_bytes().map(format_size);
        let more = if self.walking() { " …" } else { "" };
        if let Some(row) = self.single() {
            let mut parts = vec![type_label(&row.entry)];
            parts.extend(size.map(|s| format!("{s}{more}")));
            if let Some(photo) = loaded.photo.as_ref() {
                parts.push(format!("{} × {}", photo.width, photo.height));
                if let Some(camera) = photo.info.as_ref().and_then(|i| i.camera.clone()) {
                    parts.push(camera);
                }
            }
            if row.entry.is_dir_like()
                && let Some(usage) = self.usage()
            {
                parts.push(Self::contains_text(usage));
            }
            lines.push(parts.join(dot));
        } else {
            let mut parts = vec![tn!("props.items", self.targets.len()).into_owned()];
            parts.extend(size.map(|s| format!("{s}{more}")));
            lines.push(parts.join(dot));
            if let Some(usage) = self.usage().filter(|_| self.any_folder()) {
                lines.push(Self::contains_text(usage));
            }
        }
        lines
    }

    /// The facts of the General card, then the Time card, in order.
    fn info_rows(&self) -> Vec<InfoRow> {
        let Some(loaded) = self.loaded.as_ref() else {
            return Vec::new();
        };
        let shell = &self.services.shell;
        let mut out = Vec::new();
        let single = self.single();
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
                Some(single.map_or_else(|| first.to_path_buf(), |r| r.path.clone())),
            ));
        }
        let calculating = t!("props.calculating").into_owned();
        if loaded.drive.is_none() {
            let walking = self.walking();
            let on_disk = match single {
                Some(row) if !row.entry.is_dir_like() => {
                    Some(on_disk_of(&row.entry, loaded.details, loaded.cluster))
                }
                _ => self.usage().map(|u| u.on_disk),
            };
            let mut size_row = InfoRow::plain(
                "props-size",
                t!("col.size"),
                self.size_bytes().map_or_else(
                    || calculating.clone(),
                    |b| {
                        let mut text = Self::size_text(b);
                        if walking {
                            text.push_str(" …");
                        }
                        if self.usage().is_some_and(|u| u.partial) && !walking {
                            text = t!("props.at_least", size = text).into_owned();
                        }
                        text
                    },
                ),
            );
            size_row.stoppable = walking;
            out.push(size_row);
            out.push(InfoRow::plain(
                "props-ondisk",
                t!("props.size_on_disk"),
                on_disk.map_or(calculating, Self::size_text),
            ));
            if self.any_folder() {
                out.push(InfoRow::plain(
                    "props-contains",
                    t!("props.contains_label"),
                    self.usage()
                        .map_or_else(|| t!("props.calculating").into_owned(), Self::contains_text),
                ));
            }
        }
        if let Some(row) = single {
            let time = |t: Option<std::time::SystemTime>| t.map(|t| shell.format_time(t));
            if let Some(photo) = loaded
                .photo
                .as_ref()
                .and_then(|p| p.info.as_ref())
                .and_then(|i| i.taken.clone())
            {
                out.push(InfoRow::time("props-taken", t!("props.taken"), photo));
            }
            if let Some(text) = time(row.entry.created) {
                out.push(InfoRow::time("props-created", t!("col.created"), text));
            }
            if let Some(text) = time(row.entry.modified) {
                out.push(InfoRow::time("props-modified", t!("col.modified"), text));
            }
            if let Some(text) = time(loaded.details.accessed) {
                out.push(InfoRow::time("props-accessed", t!("props.accessed"), text));
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

    /// Where the space goes: bytes per category (biggest first, none left out when empty), the
    /// biggest folders inside, and the total they are measured against.
    pub(crate) fn space_view(&self) -> Option<SpaceView> {
        let space = self.space.as_ref()?;
        // An empty folder, once counted, has nothing to break down.
        let empty = space.usage.bytes == 0 && matches!(self.counting, Counting::Done);
        if !self.any_folder() || empty {
            return None;
        }
        let mut categories: Vec<(Category, u64)> = Category::ALL
            .into_iter()
            .map(|c| (c, space.usage.categories[c.index()]))
            .filter(|(_, bytes)| *bytes > 0)
            .collect();
        categories.sort_by_key(|c| std::cmp::Reverse(c.1));
        Some(SpaceView {
            total: space.usage.bytes,
            categories,
            largest: space
                .largest(3)
                .into_iter()
                .map(|(path, bytes)| (leaf(&path), bytes))
                .collect(),
        })
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
    pub(crate) fn summary_for_test(&self) -> Vec<String> {
        self.summary()
    }

    #[cfg(test)]
    pub(crate) const fn hash_for_test(&self) -> &HashState {
        &self.hash
    }

    #[cfg(test)]
    pub(crate) fn usage_for_test(&self) -> Option<Usage> {
        self.usage().copied()
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
    pub(crate) fn errors_for_test(&self) -> [Option<&str>; 6] {
        [
            self.name_error.as_deref(),
            self.attr_error.as_deref(),
            self.app_error.as_deref(),
            self.native_error.as_deref(),
            self.action_error.as_deref(),
            self.unblock_error.as_deref(),
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
    pub(crate) fn blocked_for_test(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.blocked)
    }

    #[cfg(test)]
    pub(crate) fn picture_for_test(&self) -> (bool, bool) {
        self.loaded
            .as_ref()
            .map_or((false, false), |l| (l.icon.is_some(), l.picture))
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

    #[cfg(test)]
    pub(crate) const fn columns_for_test(&self) -> u8 {
        self.columns.get()
    }

    #[cfg(test)]
    pub(crate) const fn height_for_test(&self) -> (f32, bool) {
        (self.fit_height, self.user_sized)
    }
}

/// What the "where the space goes" card draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpaceView {
    pub(crate) total: u64,
    pub(crate) categories: Vec<(Category, u64)>,
    /// Name and size of the biggest folders inside, biggest first.
    pub(crate) largest: Vec<(String, u64)>,
}

/// One line of the General or Time card.
struct InfoRow {
    id: &'static str,
    label: String,
    value: String,
    /// Clicking the value shows this in Arke.
    link: Option<PathBuf>,
    /// A "Stop" link follows while it is still being counted.
    stoppable: bool,
    /// Belongs in the Time card.
    time: bool,
}

impl InfoRow {
    fn plain(id: &'static str, label: impl std::fmt::Display, value: String) -> Self {
        Self {
            id,
            label: label.to_string(),
            value,
            link: None,
            stoppable: false,
            time: false,
        }
    }

    fn time(id: &'static str, label: impl std::fmt::Display, value: String) -> Self {
        Self {
            time: true,
            ..Self::plain(id, label, value)
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

/// Brings Arke's window used last to the front on `folder`, with `select` selected; `new_tab`
/// opens it in a tab of its own.
fn show_in_arke(folder: PathBuf, select: Vec<Arc<str>>, new_tab: bool, cx: &mut App) {
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
        if new_tab {
            workspace.open_tab(folder, usize::MAX, true, window, cx);
        } else {
            workspace.open_external(Target::Folder(folder), select, window, cx);
        }
        window.activate_window();
    });
}

// ───────────────────────────── Background reads ─────────────────────────────

/// Every file and folder below `root`, links not followed. Background thread.
fn descendants(fs: &dyn arke_core::Fs, root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs.read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = dir.join(&*entry.name);
            match entry.kind {
                EntryKind::Dir => {
                    stack.push(path.clone());
                    found.push(path);
                }
                EntryKind::File => found.push(path),
                EntryKind::Link { .. } => {}
            }
        }
    }
    found
}

/// SHA-256 of a file, in lowercase hex; `Ok(None)` when cancelled. `progress` gets the percent
/// read whenever it changes. Background thread.
fn hash_file(
    fs: &dyn arke_core::Fs,
    path: &Path,
    total: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u8),
) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    let mut file = fs.open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_CHUNK];
    let (mut read, mut shown) = (0u64, 0u8);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        read += n as u64;
        let percent =
            u8::try_from((read * 100).checked_div(total).unwrap_or(0).min(99)).unwrap_or(99);
        if percent != shown {
            shown = percent;
            progress(percent);
        }
    }
    Ok(Some(hasher.finalize().iter().fold(
        String::new(),
        |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    )))
}

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
    let (icon, picture, photo) = single.map_or((None, false, None), |r| hero_of(services, r));
    // The mark is looked for on files that are here; a cloud file that is not is left alone.
    let blocked = single
        .filter(|r| !r.entry.is_dir_like() && !r.entry.is_cloud_placeholder())
        .is_some_and(|r| shell.is_blocked(&r.path));
    let places = if single.is_some_and(|r| r.entry.is_dir_like()) {
        shell.load_places()
    } else {
        Vec::new()
    };
    Loaded {
        rows,
        unreadable,
        details,
        target,
        app,
        drive,
        cluster,
        icon,
        picture,
        photo,
        blocked,
        places,
    }
}

/// The big picture at the top: a photo or a frame of a video where there is one, else the icon
/// Windows has for the item. A cloud file that is not on this device is never opened for it
/// (`can_thumbnail` and `wants_system_icon` leave those out), so it gets the kind icon.
fn hero_of(services: &Services, row: &Row) -> (Option<Arc<RenderImage>>, bool, Option<PhotoMeta>) {
    let entry = &row.entry;
    if can_thumbnail(entry)
        && let Ok(bytes) = services
            .fs
            .read(&row.path, arke_core::filetype::MAX_THUMBNAIL_SOURCE)
        && let Ok(photo) = arke_core::thumbs::decode_photo(&bytes, HERO_DECODE)
    {
        let image = crate::media::to_render_image(photo.image);
        return (
            image.clone(),
            image.is_some(),
            Some(PhotoMeta {
                width: photo.width,
                height: photo.height,
                info: photo.info,
            }),
        );
    }
    if can_video_thumbnail(entry)
        && let Ok(Some(frame)) = services.shell.video_thumbnail(&row.path, HERO_DECODE)
    {
        let image = crate::media::to_render_image(frame);
        return (image.clone(), image.is_some(), None);
    }
    let thumb = if wants_system_icon(entry) {
        services
            .shell
            .file_icon(&row.path, crate::media::SYSTEM_ICON_SIZE)
    } else if wants_type_icon(entry) {
        services.shell.type_icon(entry.extension())
    } else {
        None
    };
    (thumb.and_then(crate::media::to_render_image), false, None)
}

/// What one file takes on disk: Windows' own number when it gave one, else estimated from the
/// cluster size. A cloud file that is not on this device takes nothing, whatever was asked.
pub(crate) fn on_disk_of(entry: &Entry, details: FileDetails, cluster: u64) -> u64 {
    if entry.is_cloud_placeholder() {
        return 0;
    }
    // Windows gives the bytes stored (the file's own size unless it is compressed or sparse);
    // they take whole clusters, as File Explorer shows.
    details.size_on_disk.map_or_else(
        || dirsize::file_on_disk(entry, cluster),
        |bytes| dirsize::round_to_clusters(bytes, cluster),
    )
}

/// The faster ways to count a folder than walking it.
struct Fast<'a> {
    /// The search index, when it follows the folder's drive live: one pass in memory.
    index: Option<&'a arke_core::index_service::IndexService>,
    /// The index service, which reads a drive's whole file table ($MFT) in seconds, as WizTree
    /// does (admin rights, which it has as a service).
    table: Option<&'a dyn arke_core::journal::Journal>,
    /// How long that took before, per drive (`C:\`), with what this window has learned since.
    table_times:
        &'a parking_lot::Mutex<std::collections::BTreeMap<String, crate::settings::TableTime>>,
    /// The profiles the service lists without contents (for the index and the file table).
    profiles: Option<&'a dirsize::Profiles>,
    /// Whether a drive is a spinning disk (`win_eject::seeks`), where two reads at once slow each
    /// other down.
    seeks: fn(&Path) -> bool,
}

/// The index service, when it is installed, for reading drives' file tables.
fn table_reader() -> Option<Arc<dyn arke_core::journal::Journal>> {
    if cfg!(test) || !arke_indexd::client::installed() {
        return None;
    }
    Some(Arc::new(arke_indexd::client::ServiceJournal::new()))
}

/// Whose profile folders the index service lists without contents. None in tests, which don't
/// depend on the machine's profiles.
fn service_profiles() -> Option<dirsize::Profiles> {
    if cfg!(test) {
        return None;
    }
    arke_indexd::client::profiles()
}

/// Whether the drive's file table can count `dir`: not inside a folder its listing doesn't go
/// into (hidden system folders such as `C:\Windows\Installer`, the recycle bin), through a link
/// (a junction, a mount point: not in the listing) or in another user's profile. Otherwise the
/// table is neither asked (a whole read for nothing) nor handed over to. A stat per folder on the
/// way, only when the table would start.
fn table_can_count(
    fs: &dyn arke_core::fs::Fs,
    root: &Path,
    dir: &Path,
    profiles: Option<&dirsize::Profiles>,
) -> bool {
    if profiles.is_some_and(|p| p.hidden(dir)) {
        return false;
    }
    let mut path = root.to_path_buf();
    dir.components().skip(2).all(|part| {
        path.push(part);
        fs.stat(&path)
            .is_ok_and(|entry| arke_core::index::descends_into(&entry))
    })
}

/// The count from the file table, else from `walk` when the table can't give one (not NTFS, an
/// older service, the folder isn't in it, a bug in the read). `None` when cancelled.
fn table_else_walk(
    read: impl FnOnce() -> std::io::Result<Space>,
    walk: impl FnOnce() -> Option<Space>,
) -> Option<Space> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)) {
        Ok(Ok(space)) => Some(space),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => None,
        Ok(Err(_)) => walk(),
        Err(_) => {
            log::warn!("properties: the file table read panicked");
            walk()
        }
    }
}

/// `C:\` for anything on drive C.
fn drive_root(path: &Path) -> Option<PathBuf> {
    let root: PathBuf = path.components().take(2).collect();
    (root.components().count() == 2 && root.to_string_lossy().ends_with('\\')).then_some(root)
}

/// Counts `dir`, reporting as it goes through `progress`:
/// - from the index when it follows the drive live;
/// - a drive itself from its file table (a whole drive in seconds instead of a minute or more);
/// - a folder by walking it, overtaken by a read of the file table once the walk has taken as long
///   as reading the table took on that drive before (or from the start when that isn't known yet).
///   Whichever is done first counts; the other stops at once. Nothing is a fixed time: a small
///   folder is walked in less than the table takes, a big one is not. A read stopped early still
///   tells that the table isn't worth starting before the walk's time, so small folders soon
///   stop starting it. A folder the table can't count (`table_can_count`) is only walked.
///
/// On a spinning disk the table counts alone, from the start: a walk there seeks for every folder,
/// and the two at once would slow each other down. Only what the table can't count is walked.
fn count_folder(
    fs: &dyn arke_core::fs::Fs,
    fast: &Fast,
    dir: &Path,
    cluster: u64,
    cancel: &AtomicBool,
    tx: &flume::Sender<Msg>,
    progress: &mut dyn FnMut(&Space),
) -> Option<Space> {
    use crate::settings::TableTime;
    let (times, profiles) = (fast.table_times, fast.profiles);
    if let Some(space) = fast
        .index
        .and_then(|ix| ix.usage_below(dir, cluster, profiles, cancel))
    {
        return Some(space);
    }
    let table = fast.table.zip(drive_root(dir));
    let Some((table, root)) = table else {
        return dirsize::usage(fs, dir, cluster, cancel, progress);
    };
    let key = root.to_string_lossy().into_owned();
    // `since`: when the count began. A read stopped early took less than the table takes, and the
    // count was done first anyway: the table isn't worth starting before that time.
    let read_table = |stop: &AtomicBool, since: Instant| {
        let started = Instant::now();
        let result = dirsize::usage_from_listing(table, fs, dir, cluster, profiles, stop);
        let ms = |from: Instant| u64::try_from(from.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Read to the end, also when for nothing (the folder isn't in it): the table's time.
        let read_all = result.as_ref().err().is_none_or(dirsize::listed_to_end);
        let stopped = matches!(&result, Err(e) if e.kind() == std::io::ErrorKind::Interrupted);
        let time = if read_all {
            Some(TableTime {
                ms: ms(started),
                done: true,
            })
        } else if stopped {
            Some(TableTime {
                ms: ms(since),
                done: false,
            })
        } else {
            None
        };
        if let Some(time) = time {
            TableTime::learn(&mut times.lock(), key.clone(), time);
            let _ = tx.send(Msg::TableTime(key.clone(), time));
        }
        result
    };
    // The drive itself (the table has everything), or a spinning disk: there the table is one
    // sweep over the drive, a walk seeks for every folder, and the two at once slow each other
    // down. The table alone, a walk only when it can't count it.
    if dir == root || ((fast.seeks)(&root) && table_can_count(fs, &root, dir, profiles)) {
        return table_else_walk(
            || read_table(cancel, Instant::now()),
            || dirsize::usage(fs, dir, cluster, cancel, progress),
        );
    }
    let known = times.lock().get(&key).copied();
    let overtake = known.map(|t| std::time::Duration::from_millis(t.ms));
    let stop_walk = AtomicBool::new(false);
    let stop_table = AtomicBool::new(false);
    let (done_tx, done_rx) = flume::bounded::<std::io::Result<Space>>(1);
    // Moved into the table's thread, so the channel closes when it ends whatever way (a panic).
    let mut done_tx = Some(done_tx);
    let started = Instant::now();
    std::thread::scope(|scope| {
        let mut asked = false;
        let mut reading = None;
        let mut from_table: Option<Space> = None;
        let walked = dirsize::usage(fs, dir, cluster, &stop_walk, &mut |so_far| {
            if cancel.load(Ordering::Relaxed) {
                stop_walk.store(true, Ordering::Relaxed);
                return;
            }
            if !asked && overtake.is_none_or(|after| started.elapsed() >= after) {
                asked = true;
                if table_can_count(fs, &root, dir, profiles)
                    && let Some(done_tx) = done_tx.take()
                {
                    let (read_table, stop_table) = (&read_table, &stop_table);
                    reading = Some(scope.spawn(move || {
                        let _ = done_tx.send(read_table(stop_table, started));
                    }));
                }
            }
            // The table can't count it (or its thread failed): the walk just goes on.
            if let Ok(Ok(space)) = done_rx.try_recv() {
                from_table = Some(space);
                stop_walk.store(true, Ordering::Relaxed);
                return;
            }
            progress(so_far);
        });
        stop_table.store(true, Ordering::Relaxed);
        // Joined here so a panic in the table's thread doesn't take the count down with it.
        if let Some(reading) = reading
            && reading.join().is_err()
        {
            log::warn!("properties: the file table read panicked");
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        walked.or(from_table)
    })
}

/// Counts the files and folders under the items and reports as it goes. Background thread.
fn count_space(
    services: &Services,
    fast: &Fast,
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
        let before = base.on_disk;
        dirsize::count_file(&mut base, &row.entry, cluster);
        if single {
            base.on_disk = before + on_disk_of(&row.entry, details, cluster);
        }
    }
    let mut done = Space {
        usage: base,
        children: Vec::new(),
    };
    let mut last_sent: Option<Instant> = None;
    for row in rows.iter().filter(|r| r.entry.is_dir_like()) {
        let before = done.clone();
        let walked = count_folder(
            &*services.fs,
            fast,
            &row.path,
            cluster,
            cancel,
            tx,
            &mut |so_far| {
                if last_sent.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                    last_sent = Some(Instant::now());
                    let mut shown = before.clone();
                    shown.usage += so_far.usage;
                    if single {
                        shown.children.clone_from(&so_far.children);
                    } else {
                        shown.children.push((row.path.clone(), so_far.usage.bytes));
                    }
                    let _ = tx.send(Msg::Space(shown, Counting::Running));
                }
            },
        );
        match walked {
            Some(space) => {
                done.usage += space.usage;
                if single {
                    done.children = space.children;
                } else {
                    done.children.push((row.path.clone(), space.usage.bytes));
                }
            }
            // Stopped: the window already knows.
            None => return,
        }
    }
    let _ = tx.send(Msg::Space(done, Counting::Done));
}

// ───────────────────────────── Drawing ─────────────────────────────

impl Focusable for Properties {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A row of a card, with the height it is drawn at (to size the window).
struct CardRow {
    element: AnyElement,
    height: f32,
}

const ROW_H: f32 = 42.;

/// The row layout of every card: label on the left, the value (and what goes with it) after it.
fn field(label: String, value: AnyElement, height: f32) -> CardRow {
    // What's on the right is read with the row's label ("Size, 3 B"; "Location, C:\...").
    let a11y = crate::a11y::active();
    CardRow {
        element: div()
            .min_h(px(ROW_H))
            .px(theme::s_16())
            .py(theme::s_8())
            .flex()
            .items_center()
            .gap(theme::s_16())
            .when(a11y, |d| {
                d.child(crate::a11y::label_scope(label.clone().into()))
            })
            .child(
                div()
                    .w(LABEL_W)
                    .flex_none()
                    .text_size(theme::fs_13())
                    .text_color(theme::text_muted())
                    .child(label),
            )
            .child(div().flex_1().min_w_0().child(value))
            .when(a11y, |d| d.child(crate::a11y::end_label_scope()))
            .into_any_element(),
        height: height.max(ROW_H),
    }
}

/// A row with a single control (a check box), no label column.
fn wide(element: AnyElement, height: f32) -> CardRow {
    CardRow {
        element: div()
            .min_h(px(ROW_H))
            .px(theme::s_16())
            .py(theme::s_8())
            .flex()
            .items_center()
            .child(element)
            .into_any_element(),
        height: height.max(ROW_H),
    }
}

fn text_value(id: &'static str, text: impl Into<SharedString>) -> AnyElement {
    let text: SharedString = text.into();
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_row_text(text.clone())
        .min_w_0()
        .text_size(theme::fs_13())
        .text_color(theme::text())
        .child(text)
        .into_any_element()
}

fn problem_text(id: &'static str, text: &str) -> CardRow {
    CardRow {
        element: div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .a11y_live(gpui::Role::Alert, text.to_owned())
            .px(theme::s_16())
            .py(theme::s_8())
            .text_size(theme::fs_12())
            .text_color(theme::danger())
            .child(text.to_owned())
            .into_any_element(),
        height: 32.,
    }
}

/// A card like the settings page's: icon and title above, a box of rows below, a line between
/// the rows. Returns the card and its height.
fn card(icon: Icon, title: String, rows: Vec<CardRow>) -> (AnyElement, f32) {
    let height = EST_HEADING + rows.iter().map(|r| r.height).sum::<f32>() + EST_GAP;
    let element = div()
        .flex()
        .flex_col()
        .child(
            div()
                .pt(theme::s_4())
                .pb(theme::s_8())
                .px(theme::s_4())
                .flex()
                .items_center()
                .gap(theme::s_8())
                .child(
                    svg()
                        .path(icon.path())
                        .flex_none()
                        .size(theme::icon())
                        .text_color(theme::text_muted()),
                )
                .child(
                    div()
                        .text_size(theme::fs_13())
                        .font_weight(theme::FW_STRONG)
                        .text_color(theme::text_muted())
                        .child(title),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(theme::r_8())
                .bg(theme::card())
                .border_1()
                .border_color(theme::line())
                .overflow_hidden()
                .children(rows.into_iter().enumerate().map(|(ix, row)| {
                    div()
                        .when(ix > 0, |d| d.border_t_1().border_color(theme::line()))
                        .child(row.element)
                })),
        )
        .into_any_element();
    (element, height)
}

/// A text button, like the settings page's.
fn button(
    id: &'static str,
    label: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = SharedString::from(label.to_string());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_row_button(label.clone())
        .h(theme::h_ctl_sm())
        .px(theme::s_12())
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::text())
        .hover(|s| s.bg(theme::wash()))
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

/// A small action under the name: icon and label.
fn action(
    id: &'static str,
    icon: Icon,
    label: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = SharedString::from(label.to_string());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_row_button(label.clone())
        .h(theme::h_ctl_sm())
        .px(theme::s_8())
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_6())
        .rounded(theme::r_8())
        .bg(theme::card())
        .border_1()
        .border_color(theme::line())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::text())
        .hover(|s| s.bg(theme::wash()))
        .child(
            svg()
                .path(icon.path())
                .size(px(14.))
                .flex_none()
                .text_color(theme::text_muted()),
        )
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

/// A link-like click target (accent text).
fn link(
    id: &'static str,
    text: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let text: SharedString = text.into();
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_row_button(text.clone())
        .flex_none()
        .cursor_pointer()
        .text_size(theme::fs_12())
        .text_color(theme::signal())
        .hover(gpui::Styled::underline)
        .child(text)
        .on_click(on_click)
        .into_any_element()
}

fn check_box(state: Tri) -> gpui::Div {
    let on = state != Tri::Off;
    div()
        .size(theme::icon())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
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

/// The color of a category in the space bar: the color its kind of file has in the list.
fn category_color(category: Category) -> gpui::Rgba {
    let kind = match category {
        Category::Code => FileKind::Code,
        Category::Video => FileKind::Video,
        Category::Images => FileKind::Image,
        Category::Documents => FileKind::Document,
        Category::Audio => FileKind::Audio,
        Category::Archives => FileKind::Archive,
        Category::Other => return theme::text_subtle(),
    };
    theme::file_glyph(kind).map_or_else(theme::text_subtle, |(_, color)| color)
}

fn category_name(category: Category) -> String {
    match category {
        Category::Code => t!("props.cat_code"),
        Category::Video => t!("props.cat_video"),
        Category::Images => t!("props.cat_images"),
        Category::Documents => t!("props.cat_documents"),
        Category::Audio => t!("props.cat_audio"),
        Category::Archives => t!("props.cat_archives"),
        Category::Other => t!("props.cat_other"),
    }
    .into_owned()
}

impl Properties {
    /// The picture or icon, the name, what is worth knowing about the item and the quick actions.
    #[allow(
        clippy::too_many_lines,
        reason = "one layout: picture, name and summary"
    )]
    fn hero(&self, cx: &mut Context<Self>) -> (AnyElement, f32) {
        let loaded = self.loaded.as_ref();
        let single = self.single();
        let drive = self.is_drive();
        let picture = loaded.is_some_and(|l| l.picture);
        let inner: AnyElement = match (loaded.and_then(|l| l.icon.clone()), single) {
            (Some(image), _) if picture => img(image)
                .size_full()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            (Some(image), _) => img(image).size(px(56.)).into_any_element(),
            (None, Some(_)) if drive => svg()
                .path(Icon::Drive.path())
                .size(px(56.))
                .text_color(theme::text_muted())
                .into_any_element(),
            (None, Some(row)) => kind_icon(file_kind(&row.entry), px(60.)),
            (None, None) => kind_icon(
                if !self.rows().is_empty() && self.rows().iter().all(|r| r.entry.is_dir_like()) {
                    FileKind::Folder
                } else {
                    FileKind::Other
                },
                px(60.),
            ),
        };
        let thumb = div()
            .id("props-hero")
            .debug_selector(|| "props-hero".to_owned())
            .size(HERO)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .rounded(theme::r_12())
            .bg(theme::card())
            .border_1()
            .border_color(theme::line())
            .when(picture, |d| d.shadow(theme::elev_1()))
            .child(inner);
        let editable = single.is_some_and(|r| !is_drive_root(&r.path));
        let title: AnyElement = if editable {
            div()
                .id("props-name")
                .debug_selector(|| "props-name".to_owned())
                .w_full()
                .min_w_0()
                .h(theme::h_ctl())
                .px(theme::s_8())
                .flex()
                .items_center()
                .rounded(theme::r_8())
                .bg(theme::surface())
                .border_1()
                .border_color(if self.name_error.is_some() {
                    theme::danger()
                } else {
                    theme::line_strong()
                })
                .text_size(theme::fs_15())
                // The field is named "Name" for screen readers (`text_input.rs`).
                .when(crate::a11y::active(), |d| {
                    d.child(crate::a11y::label_scope(t!("col.name").into_owned().into()))
                })
                .child(self.name_input.clone())
                .when(crate::a11y::active(), |d| {
                    d.child(crate::a11y::end_label_scope())
                })
                .into_any_element()
        } else {
            let text = match (single, loaded.and_then(|l| l.drive.as_ref())) {
                (_, Some(d)) if !d.label.is_empty() => format!("{} ({})", d.label, self.name),
                (Some(_), _) => self.name.clone(),
                _ => tn!("props.items", self.targets.len()).into_owned(),
            };
            div()
                .w_full()
                .min_w_0()
                .text_size(theme::fs_20())
                .font_weight(theme::FW_STRONG)
                .text_color(theme::text())
                .truncate()
                .child(text)
                .into_any_element()
        };
        let lines = self.summary();
        let actions = self.actions(cx);
        let height = (EST_NAME
            + lines.len() as f32 * EST_LINE
            + actions.as_ref().map_or(0., |(_, h)| h + 8.))
        .max(HERO_H);
        let element = div()
            .flex()
            .items_start()
            .gap(theme::s_16())
            .child(thumb)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(theme::s_6())
                    .child(title)
                    .when_some(self.name_error.as_ref(), |d, e| {
                        d.child(
                            div()
                                .id("props-name-error")
                                .debug_selector(|| "props-name-error".to_owned())
                                .a11y_live(gpui::Role::Alert, e.clone())
                                .text_size(theme::fs_12())
                                .text_color(theme::danger())
                                .child(e.clone()),
                        )
                    })
                    .children(lines.into_iter().map(|line| {
                        div()
                            .text_size(theme::fs_13())
                            .text_color(theme::text_muted())
                            .child(line)
                    }))
                    .children(actions.map(|(a, _)| div().pt(theme::s_4()).child(a))),
            )
            .into_any_element();
        (element, height)
    }

    /// Open, Copy path, ... for what is shown.
    fn actions(&self, cx: &mut Context<Self>) -> Option<(AnyElement, f32)> {
        self.loaded.as_ref()?;
        let single = self.single();
        let mut row = div().flex().flex_wrap().gap(theme::s_8());
        if let Some(item) = single {
            row = row.child(action(
                "props-act-open",
                Icon::Open,
                t!("props.act_open"),
                cx.listener(|this, _: &ClickEvent, _, cx| this.open_item(cx)),
            ));
            if item.entry.is_dir_like() && !self.is_drive() {
                row = row.child(action(
                    "props-act-tab",
                    Icon::TabOpen,
                    t!("menu.open_in_new_tab"),
                    cx.listener(|this, _: &ClickEvent, _, cx| this.open_in_new_tab(cx)),
                ));
            } else if !item.entry.is_dir_like() {
                row = row.child(action(
                    "props-act-with",
                    Icon::OpenWith,
                    t!("props.act_open_with"),
                    cx.listener(|this, _: &ClickEvent, window, _| this.change_app(window)),
                ));
            }
        }
        row = row.child(action(
            "props-act-copy",
            Icon::CopyPath,
            t!("menu.copy_path"),
            cx.listener(|this, _: &ClickEvent, _, _| this.copy_path()),
        ));
        if let Some(item) = single {
            if item.entry.is_dir_like() && !self.is_drive() {
                let pinned = self.is_favorite(cx);
                row = row.child(action(
                    "props-act-fav",
                    Icon::Pin,
                    if pinned {
                        t!("props.act_unfavorite")
                    } else {
                        t!("props.act_favorite")
                    },
                    cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_favorite(cx)),
                ));
            } else if !item.entry.is_dir_like() {
                let path = item.path.clone();
                row = row.child(action(
                    "props-act-show",
                    Icon::External,
                    t!("props.act_show"),
                    cx.listener(move |_, _: &ClickEvent, _, cx| Self::reveal(&path, cx)),
                ));
            }
        }
        let error = self.action_error.as_ref().map(|e| {
            div()
                .id("props-action-error")
                .debug_selector(|| "props-action-error".to_owned())
                .text_size(theme::fs_12())
                .text_color(theme::danger())
                .child(e.clone())
        });
        let height = EST_ACTIONS + if error.is_some() { 20. } else { 0. };
        Some((
            div()
                .flex()
                .flex_col()
                .gap(theme::s_6())
                .child(row)
                .children(error)
                .into_any_element(),
            height,
        ))
    }

    fn general_card(&self, cx: &mut Context<Self>) -> (AnyElement, f32) {
        let mut rows = Vec::new();
        for info in self.info_rows().into_iter().filter(|r| !r.time) {
            let id = info.id;
            let stoppable = info.stoppable;
            let value: AnyElement = match info.link {
                Some(path) => div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .a11y_row_button(info.value.clone().into())
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .text_size(theme::fs_13())
                    .text_color(theme::signal())
                    .hover(gpui::Styled::underline)
                    .child(info.value)
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        Self::reveal(&path, cx);
                    }))
                    .into_any_element(),
                None => text_value(id, info.value),
            };
            let value = div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .child(value)
                .when(stoppable, |d| {
                    d.child(link(
                        "props-stop",
                        t!("props.stop").to_string(),
                        cx.listener(|this, _: &ClickEvent, _, cx| this.stop_counting(cx)),
                    ))
                })
                .into_any_element();
            rows.push(field(info.label, value, ROW_H));
        }
        if let Some(drive) = self.loaded.as_ref().and_then(|l| l.drive.clone()) {
            rows.extend(Self::drive_rows(&drive));
        }
        if let Some(row) = self.opens_with_row(cx) {
            rows.push(row);
            if let Some(e) = self.app_error.as_ref() {
                rows.push(problem_text("props-app-error", e));
            }
        }
        if let Some(target) = self.loaded.as_ref().and_then(|l| l.target.clone()) {
            let shown = target.display().to_string();
            rows.push(field(
                t!("props.shortcut_target").to_string(),
                div()
                    .id("props-target")
                    .debug_selector(|| "props-target".to_owned())
                    .a11y_row_button(shown.clone().into())
                    .cursor_pointer()
                    .text_size(theme::fs_13())
                    .text_color(theme::signal())
                    .hover(gpui::Styled::underline)
                    .child(shown)
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        Self::reveal(&target, cx);
                    }))
                    .into_any_element(),
                ROW_H + 8.,
            ));
        }
        card(Icon::Info, t!("props.general").into_owned(), rows)
    }

    fn drive_rows(drive: &Drive) -> Vec<CardRow> {
        let used = drive.total.saturating_sub(drive.free);
        let fraction = if drive.total == 0 {
            0.
        } else {
            (used as f32 / drive.total as f32).clamp(0., 1.)
        };
        let bar = div()
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
                    .w(relative(fraction))
                    .rounded(theme::R_FULL)
                    .bg(theme::signal()),
            )
            .into_any_element();
        let mut rows = vec![wide(bar, ROW_H)];
        for (label, bytes) in [
            (t!("props.used"), used),
            (t!("props.free"), drive.free),
            (t!("props.capacity"), drive.total),
        ] {
            rows.push(field(
                label.into_owned(),
                text_value("props-drive-stat", Self::size_text(bytes)),
                ROW_H,
            ));
        }
        if !drive.file_system.is_empty() {
            rows.push(field(
                t!("props.file_system").into_owned(),
                text_value("props-file-system", drive.file_system.clone()),
                ROW_H,
            ));
        }
        rows
    }

    fn opens_with_row(&self, cx: &mut Context<Self>) -> Option<CardRow> {
        self.single().filter(|r| !r.entry.is_dir_like())?;
        let app = self
            .loaded
            .as_ref()
            .and_then(|l| l.app.clone())
            .unwrap_or_else(|| t!("props.no_app").into_owned());
        Some(field(
            t!("props.opens_with").into_owned(),
            div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .child(
                    div()
                        .id("props-app")
                        .debug_selector(|| "props-app".to_owned())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(theme::fs_13())
                        .text_color(theme::text())
                        .child(app),
                )
                .child(button(
                    "props-change-app",
                    t!("props.change"),
                    cx.listener(|this, _: &ClickEvent, window, _| this.change_app(window)),
                ))
                .into_any_element(),
            ROW_H + 4.,
        ))
    }

    fn time_card(&self) -> Option<(AnyElement, f32)> {
        let rows: Vec<CardRow> = self
            .info_rows()
            .into_iter()
            .filter(|r| r.time)
            .map(|r| field(r.label, text_value(r.id, r.value), ROW_H))
            .collect();
        (!rows.is_empty()).then(|| card(Icon::Clock, t!("props.times").into_owned(), rows))
    }

    fn attribute_row(
        &self,
        id: &'static str,
        label: String,
        bit: u32,
        cx: &mut Context<Self>,
    ) -> CardRow {
        let state = self.attribute(bit);
        wide(
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .a11y_toggle(label.clone(), state == Tri::On)
                .flex()
                .items_center()
                .gap(theme::s_8())
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.toggle_attribute(bit, cx)),
                )
                .child(check_box(state))
                .child(
                    div()
                        .text_size(theme::fs_13())
                        .text_color(theme::text())
                        .child(label),
                )
                .into_any_element(),
            ROW_H,
        )
    }

    fn checksum_row(&self, cx: &mut Context<Self>) -> Option<CardRow> {
        self.single()
            .filter(|r| !r.entry.is_dir_like() && !r.entry.is_cloud_placeholder())?;
        let value: AnyElement = match &self.hash {
            HashState::Idle => button(
                "props-hash",
                t!("props.hash_calculate"),
                cx.listener(|this, _: &ClickEvent, _, cx| this.start_hash(cx)),
            ),
            HashState::Running(percent) => div()
                .flex()
                .items_center()
                .gap(theme::s_12())
                .child(text_value("props-hash-progress", format!("{percent}%")))
                .child(link(
                    "props-hash-cancel",
                    t!("props.cancel").to_string(),
                    cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_hash(cx)),
                ))
                .into_any_element(),
            HashState::Done(hex) => {
                let copy = hex.clone();
                let (a, b) = hex.split_at(hex.len() / 2);
                div()
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .child(
                        div()
                            .id("props-hash-value")
                            .debug_selector(|| "props-hash-value".to_owned())
                            .flex_1()
                            .min_w_0()
                            .text_size(theme::fs_12())
                            .text_color(theme::text())
                            .child(format!("{a}\n{b}")),
                    )
                    .child(button(
                        "props-hash-copy",
                        t!("props.hash_copy"),
                        cx.listener(move |this, _: &ClickEvent, _, _| {
                            this.copy_text(copy.clone());
                        }),
                    ))
                    .into_any_element()
            }
            HashState::Failed(error) => text_value("props-hash-error", error.clone()),
        };
        let height = if matches!(self.hash, HashState::Done(_)) {
            ROW_H + 24.
        } else {
            ROW_H
        };
        Some(field("SHA-256".to_owned(), value, height))
    }

    fn security_card(&self, cx: &mut Context<Self>) -> (AnyElement, f32) {
        let mut rows = vec![
            self.attribute_row(
                "props-readonly",
                t!("props.read_only").into_owned(),
                ATTR_READONLY,
                cx,
            ),
            self.attribute_row(
                "props-hidden",
                t!("props.hidden").into_owned(),
                ATTR_HIDDEN,
                cx,
            ),
        ];
        if self.any_folder() && !self.is_drive() {
            let state = if self.recursive { Tri::On } else { Tri::Off };
            rows.push(wide(
                div()
                    .id("props-recursive")
                    .debug_selector(|| "props-recursive".to_owned())
                    .a11y_toggle(t!("props.apply_inside").to_string(), self.recursive)
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.recursive = !this.recursive;
                            cx.notify();
                        }),
                    )
                    .child(check_box(state))
                    .child(
                        div()
                            .text_size(theme::fs_13())
                            .text_color(theme::text())
                            .child(t!("props.apply_inside").to_string()),
                    )
                    .into_any_element(),
                ROW_H,
            ));
        }
        if let Some(e) = self.attr_error.as_ref() {
            rows.push(problem_text("props-attr-error", e));
        }
        if self.loaded.as_ref().is_some_and(|l| l.blocked) {
            rows.push(field(
                t!("props.downloaded").into_owned(),
                button(
                    "props-unblock",
                    t!("props.unblock"),
                    cx.listener(|this, _: &ClickEvent, _, cx| this.unblock(cx)),
                ),
                ROW_H,
            ));
        }
        if let Some(e) = self.unblock_error.as_ref() {
            rows.push(problem_text("props-unblock-error", e));
        }
        rows.extend(self.checksum_row(cx));
        card(Icon::Settings, t!("props.attributes").into_owned(), rows)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one card: bar, legend and folder rows"
    )]
    fn space_card(&self) -> Option<(AnyElement, f32)> {
        let Some(view) = self.space_view() else {
            return self.space_card_counting();
        };
        let total = view.total.max(1) as f32;
        let mut bar = div()
            .id("props-space-bar")
            .debug_selector(|| "props-space-bar".to_owned())
            .flex()
            .h(px(12.))
            .w_full()
            .rounded(theme::R_FULL)
            .bg(theme::wash_strong())
            .overflow_hidden();
        for (category, bytes) in &view.categories {
            bar = bar.child(
                div()
                    .h_full()
                    .w(relative(*bytes as f32 / total))
                    .bg(category_color(*category)),
            );
        }
        let mut legend = div()
            .flex()
            .flex_wrap()
            .gap_x(theme::s_16())
            .gap_y(theme::s_4());
        for (category, bytes) in &view.categories {
            legend = legend.child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::s_6())
                    .text_size(theme::fs_12())
                    .child(
                        div()
                            .size(px(8.))
                            .flex_none()
                            .rounded(theme::R_FULL)
                            .bg(category_color(*category)),
                    )
                    .child(
                        div()
                            .text_color(theme::text_muted())
                            .child(category_name(*category)),
                    )
                    .child(div().text_color(theme::text()).child(format_size(*bytes))),
            );
        }
        let lines = view.categories.len().div_ceil(2).max(1) as f32;
        let mut rows = vec![CardRow {
            element: div()
                .px(theme::s_16())
                .py(theme::s_12())
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .child(bar)
                .child(legend)
                .into_any_element(),
            height: 40. + lines * 20.,
        }];
        let biggest = view.largest.first().map_or(1, |(_, b)| (*b).max(1)) as f32;
        for (name, bytes) in &view.largest {
            rows.push(CardRow {
                element: div()
                    .px(theme::s_16())
                    .py(theme::s_8())
                    .flex()
                    .flex_col()
                    .gap(theme::s_4())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme::s_8())
                            .text_size(theme::fs_13())
                            .child(
                                svg()
                                    .path(Icon::Folder.path())
                                    .size(px(14.))
                                    .flex_none()
                                    .text_color(theme::text_muted()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(theme::text_muted())
                                    .child(format_size(*bytes)),
                            ),
                    )
                    .child(
                        div()
                            .h(px(3.))
                            .w_full()
                            .rounded(theme::R_FULL)
                            .bg(theme::wash())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(*bytes as f32 / biggest))
                                    .rounded(theme::R_FULL)
                                    .bg(theme::signal()),
                            ),
                    )
                    .into_any_element(),
                height: 44.,
            });
        }
        Some(card(Icon::Sort, t!("props.space").into_owned(), rows))
    }

    /// The space card while nothing is counted yet (a drive's file table answers all at once):
    /// there from the start, an empty bar and the rows the folders will fill, so the window
    /// doesn't change its layout when the numbers come.
    fn space_card_counting(&self) -> Option<(AnyElement, f32)> {
        if !self.any_folder() || !matches!(self.counting, Counting::Running) {
            return None;
        }
        let placeholder = |height: f32, width: f32| {
            div()
                .h(px(height))
                .w(relative(width))
                .rounded(theme::R_FULL)
                .bg(theme::wash())
        };
        let mut rows = vec![CardRow {
            element: div()
                .debug_selector(|| "props-space-counting".to_owned())
                .px(theme::s_16())
                .py(theme::s_12())
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .child(
                    div()
                        .h(px(12.))
                        .w_full()
                        .rounded(theme::R_FULL)
                        .bg(theme::wash_strong()),
                )
                .child(
                    div()
                        .h(px(16.))
                        .text_size(theme::fs_12())
                        .text_color(theme::text_muted())
                        .child(t!("props.calculating").into_owned()),
                )
                // The legend's usual lines (`space_card`: 20 px each).
                .child(placeholder(8., 0.7))
                .child(placeholder(8., 0.5))
                .into_any_element(),
            height: 40. + 3. * 20.,
        }];
        // The three biggest folders go here.
        for width in [0.6, 0.45, 0.3] {
            rows.push(CardRow {
                element: div()
                    .px(theme::s_16())
                    .py(theme::s_8())
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(theme::s_8())
                    .h(px(44.))
                    .child(placeholder(8., width))
                    .child(placeholder(3., 1.))
                    .into_any_element(),
                height: 44.,
            });
        }
        Some(card(Icon::Sort, t!("props.space").into_owned(), rows))
    }

    fn title_bar(&self, window: &Window) -> AnyElement {
        let title = window_title(&self.targets);
        div()
            .flex_none()
            .h(px(EST_TITLE))
            .flex()
            .items_center()
            .child(
                div()
                    .id("props-titlebar")
                    .window_control_area(WindowControlArea::Drag)
                    .flex_1()
                    .h_full()
                    .px(theme::s_16())
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .child(
                        svg()
                            .path(Icon::Info.path())
                            .size(theme::icon())
                            .flex_none()
                            .text_color(theme::text_muted()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme::fs_13())
                            .text_color(theme::text_muted())
                            .child(title),
                    ),
            )
            .child(div().pr(theme::s_8()).child(caption_buttons(window)))
            .into_any_element()
    }

    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .px(theme::s_16())
            .py(theme::s_12())
            .flex()
            .flex_col()
            .gap(theme::s_6())
            .when_some(self.native_error.as_ref(), |d, e| {
                d.child(
                    div()
                        .id("props-native-error")
                        .debug_selector(|| "props-native-error".to_owned())
                        .text_size(theme::fs_12())
                        .text_color(theme::danger())
                        .child(e.clone()),
                )
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
                        cx.listener(|this, _: &ClickEvent, window, _| this.close_window(window)),
                    )),
            )
            .into_any_element()
    }
}

impl Render for Properties {
    #[allow(
        clippy::too_many_lines,
        reason = "one layout: hero, the two columns of cards, footer"
    )]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let columns = f32::from(window.bounds().size.width) >= sc(TWO_COLUMNS_FROM);
        self.columns.set(if columns { 2 } else { 1 });
        let mut estimate = EST_TITLE + EST_FOOTER + 8. + 32.;
        let body = if self.loaded.is_some() {
            let (hero, hero_h) = self.hero(cx);
            estimate += hero_h + EST_GAP;
            let mut left: Vec<(AnyElement, f32)> = vec![self.general_card(cx)];
            let mut right: Vec<(AnyElement, f32)> = Vec::new();
            right.extend(self.time_card());
            // A drive has no attributes to set here.
            let security = (!self.is_drive()).then(|| self.security_card(cx));
            let space = self.space_card();
            if columns && space.is_none() {
                // No space card: the attributes go under the times, so the two sides stay level.
                right.extend(security);
            } else {
                left.extend(security);
                right.extend(space);
            }
            let stack = |cards: Vec<(AnyElement, f32)>| {
                let height = cards.iter().map(|(_, h)| h).sum::<f32>();
                let element = div()
                    .flex()
                    .flex_col()
                    .gap(theme::s_12())
                    .children(cards.into_iter().map(|(c, _)| c));
                (element, height)
            };
            let (left, left_h) = stack(left);
            let (right, right_h) = stack(right);
            let grid = if columns {
                estimate += left_h.max(right_h);
                div()
                    .flex()
                    .items_start()
                    .gap(theme::s_16())
                    .child(
                        left.id("props-col-left")
                            .debug_selector(|| "props-col-left".to_owned())
                            .flex_1()
                            .min_w_0(),
                    )
                    .when(right_h > 0., |d| {
                        d.child(
                            right
                                .id("props-col-right")
                                .debug_selector(|| "props-col-right".to_owned())
                                .flex_1()
                                .min_w_0(),
                        )
                    })
            } else {
                estimate += left_h + right_h + if right_h > 0. { EST_GAP } else { 0. };
                div()
                    .flex()
                    .flex_col()
                    .gap(theme::s_12())
                    .child(
                        left.id("props-col-left")
                            .debug_selector(|| "props-col-left".to_owned()),
                    )
                    .when(right_h > 0., |d| d.child(right))
            };
            div()
                .flex()
                .flex_col()
                .gap(theme::s_12())
                .child(hero)
                .child(grid)
                .into_any_element()
        } else {
            estimate += 120.;
            div()
                .text_size(theme::fs_13())
                .text_color(theme::text_muted())
                .child(t!("props.calculating").to_string())
                .into_any_element()
        };
        // Worked out in sizes at 100%.
        self.estimate.set(sc(estimate));
        self.fit_window(window, cx);
        div()
            .id("properties")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::frame())
            .ui_font()
            .text_size(theme::fs_13())
            .text_color(theme::text())
            .child(self.title_bar(window))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .mx(theme::s_8())
                    .rounded(theme::r_12())
                    .bg(theme::paper())
                    .overflow_hidden()
                    .child(
                        div()
                            .id("props-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .p(theme::s_16())
                            .child(body),
                    ),
            )
            .child(self.footer(cx))
    }
}

#[cfg(test)]
mod count_folder_tests {
    use super::*;
    use crate::settings::TableTime;
    use arke_core::entry::{Entry, EntryKind};
    use arke_core::fake::FakeFs;
    use arke_core::journal::{
        Cursor, FakeJournal, Journal, JournalCallback, LISTED_ROOT, ListSink, Listed,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;

    /// A file table that takes `wait` before its first batch, asking the sink meanwhile whether
    /// to go on, as the service's client does.
    struct Slow {
        inner: Arc<FakeJournal>,
        wait: Duration,
    }

    impl Journal for Slow {
        fn cursor(&self, root: &Path) -> Option<Cursor> {
            self.inner.cursor(root)
        }
        fn follow(
            &self,
            root: &Path,
            from: Cursor,
            on_event: JournalCallback,
        ) -> std::io::Result<Box<dyn Send>> {
            self.inner.follow(root, from, on_event)
        }
        fn list(&self, root: &Path, sink: ListSink) -> std::io::Result<()> {
            let started = Instant::now();
            while started.elapsed() < self.wait {
                if !sink(Vec::new()) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.inner.list(root, sink)
        }
    }

    /// C:\w holds a.txt (100 bytes) and an empty folder; the file table also has big.bin (a
    /// million), so a count tells which way it was made. C:\x is on the disk but not in the
    /// table.
    fn setup() -> (Arc<FakeFs>, Arc<FakeJournal>) {
        let fs = FakeFs::new();
        fs.insert_dir(
            r"C:\",
            vec![
                Entry::new("w", EntryKind::Dir, 0, None, 0x10),
                Entry::new("x", EntryKind::Dir, 0, None, 0x10),
            ],
        );
        fs.insert_dir(
            r"C:\w",
            vec![
                Entry::new("a.txt", EntryKind::File, 100, None, 0x20),
                Entry::new("sub", EntryKind::Dir, 0, None, 0x10),
            ],
        );
        fs.insert_dir(r"C:\w\sub", vec![]);
        fs.insert_dir(
            r"C:\x",
            vec![
                Entry::new("b.txt", EntryKind::File, 7, None, 0x20),
                Entry::new("sub", EntryKind::Dir, 0, None, 0x10),
            ],
        );
        fs.insert_dir(r"C:\x\sub", vec![]);
        let item = |parent, name: &str, dir: bool, size| Listed {
            parent,
            name: name.into(),
            attributes: if dir { 0x10 } else { 0x20 },
            size,
            mtime: 0,
            link: false,
        };
        let journal = FakeJournal::new(1);
        journal.set_listing(
            r"C:\",
            vec![
                item(LISTED_ROOT, "w", true, 0),
                item(0, "a.txt", false, 100),
                item(0, "big.bin", false, 1_000_000),
                item(0, "sub", true, 0),
            ],
        );
        (fs, journal)
    }

    /// Counts `dir` with what is known in `times` (and learned on the way, as in one window).
    fn count_with(
        fs: &FakeFs,
        table: &dyn Journal,
        times: &parking_lot::Mutex<BTreeMap<String, TableTime>>,
        seeks: fn(&Path) -> bool,
        dir: &str,
    ) -> (u64, Vec<TableTime>) {
        let fast = Fast {
            index: None,
            table: Some(table),
            table_times: times,
            profiles: None,
            seeks,
        };
        let (tx, rx) = flume::unbounded();
        let cancel = AtomicBool::new(false);
        let space = count_folder(fs, &fast, Path::new(dir), 4096, &cancel, &tx, &mut |_| {})
            .expect("counted");
        drop(tx);
        let learned = rx
            .iter()
            .filter_map(|m| match m {
                Msg::TableTime(_, t) => Some(t),
                _ => None,
            })
            .collect();
        (space.usage.bytes, learned)
    }

    fn count(
        fs: &FakeFs,
        table: &dyn Journal,
        times: &BTreeMap<String, TableTime>,
        seeks: fn(&Path) -> bool,
        dir: &str,
    ) -> (u64, Vec<TableTime>) {
        count_with(
            fs,
            table,
            &parking_lot::Mutex::new(times.clone()),
            seeks,
            dir,
        )
    }

    /// A file table whose read panics, as a bug in it would.
    struct Crashing;

    impl Journal for Crashing {
        fn cursor(&self, _: &Path) -> Option<Cursor> {
            None
        }
        fn follow(
            &self,
            _: &Path,
            _: Cursor,
            _: JournalCallback,
        ) -> std::io::Result<Box<dyn Send>> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        fn list(&self, _: &Path, _: ListSink) -> std::io::Result<()> {
            panic!("simulated bug in the file table read");
        }
    }

    const fn ssd(_: &Path) -> bool {
        false
    }

    const fn hdd(_: &Path) -> bool {
        true
    }

    fn known(ms: u64, done: bool) -> BTreeMap<String, TableTime> {
        BTreeMap::from([(r"C:\".to_owned(), TableTime { ms, done })])
    }

    /// A drive is counted from its file table, and how long that took is remembered.
    #[test]
    fn a_drive_comes_from_the_file_table() {
        let (fs, journal) = setup();
        let (bytes, learned) = count(&fs, &*journal, &BTreeMap::new(), ssd, r"C:\");
        assert_eq!(bytes, 1_000_100);
        assert_eq!(journal.list_calls(), 1);
        assert!(matches!(learned[..], [TableTime { done: true, .. }]));
    }

    /// A folder walked in less time than the table takes on that drive never starts the table.
    #[test]
    fn a_quick_walk_leaves_the_table_alone() {
        let (fs, journal) = setup();
        let (bytes, learned) = count(&fs, &*journal, &known(60_000, true), ssd, r"C:\w");
        assert_eq!(bytes, 100);
        assert_eq!(journal.list_calls(), 0);
        assert!(learned.is_empty());
    }

    /// Not known yet: the table starts with the walk. The walk is done first: its count shows at
    /// once (the table stops instead of being waited for), and the table is known to take at
    /// least that long.
    #[test]
    fn the_walk_does_not_wait_for_the_table_it_beat() {
        let (fs, journal) = setup();
        let slow = Slow {
            inner: journal,
            wait: Duration::from_secs(5),
        };
        let started = Instant::now();
        let (bytes, learned) = count(&fs, &slow, &BTreeMap::new(), ssd, r"C:\w");
        assert_eq!(bytes, 100);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(learned[..], [TableTime { done: false, .. }]),
            "{learned:?}"
        );
    }

    /// A read stopped because the walk was done first tells that the table isn't worth starting
    /// before the walk's whole time, not just the part the table ran: the next count of the same
    /// folder doesn't start it again.
    #[test]
    fn a_stopped_read_learns_the_whole_walk() {
        let (fs, journal) = setup();
        fs.set_delay(r"C:\w", Duration::from_millis(60));
        fs.set_delay(r"C:\w\sub", Duration::from_millis(60));
        let slow = Slow {
            inner: journal,
            wait: Duration::from_secs(5),
        };
        // The table starts after the first folder (60 ms), the walk ends after the second.
        let (bytes, learned) = count(&fs, &slow, &known(1, false), ssd, r"C:\w");
        assert_eq!(bytes, 100);
        assert!(
            matches!(learned[..], [TableTime { ms, done: false }] if ms >= 120),
            "{learned:?}"
        );
    }

    /// What one folder teaches is used by the next in the same window: the times the window
    /// counts with have the drive's time as soon as it is read.
    #[test]
    fn folders_in_one_window_share_what_they_learn() {
        let (fs, journal) = setup();
        let times = parking_lot::Mutex::new(BTreeMap::new());
        let (_, learned) = count_with(&fs, &*journal, &times, ssd, r"C:\");
        assert!(matches!(learned[..], [TableTime { done: true, .. }]));
        assert!(
            times.lock().get(r"C:\").is_some_and(|t| t.done),
            "the next folder starts with it"
        );
    }

    /// On a spinning disk the table goes first and hasn't the folder: it is walked, each folder
    /// listed once, and the read still tells how long the table takes.
    #[test]
    fn a_walk_handed_over_for_nothing_goes_on() {
        let (fs, journal) = setup();
        let (bytes, learned) = count(&fs, &*journal, &known(0, true), hdd, r"C:\x");
        assert_eq!(bytes, 7, "walked");
        assert_eq!(fs.read_dir_calls(), 2, "C:\\x and its folder, once each");
        assert_eq!(journal.list_calls(), 1);
        assert!(
            matches!(learned[..], [TableTime { done: true, .. }]),
            "{learned:?}"
        );
    }

    /// A folder the table can't count (inside a hidden system folder, through a junction) is
    /// only walked: the table is never read for it.
    #[test]
    fn folders_the_table_cannot_count_are_only_walked() {
        let (fs, journal) = setup();
        fs.insert_dir(
            r"C:\",
            vec![
                Entry::new("sys", EntryKind::Dir, 0, None, 0x10 | 0x2 | 0x4),
                Entry::new(
                    "jl",
                    EntryKind::Link { is_dir: true },
                    0,
                    None,
                    0x10 | 0x400,
                ),
            ],
        );
        fs.insert_dir(
            r"C:\sys",
            vec![Entry::new("i.msi", EntryKind::File, 30, None, 0x20)],
        );
        fs.insert_dir(
            r"C:\jl",
            vec![Entry::new("j.txt", EntryKind::File, 40, None, 0x20)],
        );
        for (dir, want) in [(r"C:\sys", 30), (r"C:\jl", 40)] {
            for seeks in [ssd, hdd] {
                let (bytes, learned) = count(&fs, &*journal, &known(0, true), seeks, dir);
                assert_eq!(bytes, want, "{dir}");
                assert!(learned.is_empty());
            }
        }
        assert_eq!(journal.list_calls(), 0);
    }

    /// A panic in the table's read doesn't stop the count: a walk handed over to it goes on.
    #[test]
    fn a_failing_table_read_leaves_the_walk() {
        let (fs, _) = setup();
        let (bytes, learned) = count(&fs, &Crashing, &known(0, true), hdd, r"C:\w");
        assert_eq!(bytes, 100);
        assert!(learned.is_empty());
    }

    /// On a spinning disk the table counts alone from the start, whatever is known about its
    /// time: nothing is walked (a walk there seeks for every folder).
    #[test]
    fn a_spinning_disk_counts_from_the_table() {
        for times in [known(0, true), known(60_000, false), BTreeMap::new()] {
            let (fs, journal) = setup();
            let (bytes, _) = count(&fs, &*journal, &times, hdd, r"C:\w");
            assert_eq!(bytes, 1_000_000 + 100, "from the table");
            assert_eq!(fs.read_dir_calls(), 0, "nothing walked");
        }
    }

    /// A read to the end replaces what was known; a stopped one only raises a lower bound.
    #[test]
    fn table_times_keep_what_is_known_best() {
        let mut times = BTreeMap::new();
        let c = || r"C:\".to_owned();
        let at_least = |ms| TableTime { ms, done: false };
        let took = |ms| TableTime { ms, done: true };
        TableTime::learn(&mut times, c(), at_least(30));
        TableTime::learn(&mut times, c(), at_least(10));
        assert_eq!(times[r"C:\"], at_least(30));
        TableTime::learn(&mut times, c(), took(7000));
        TableTime::learn(&mut times, c(), at_least(9000));
        assert_eq!(times[r"C:\"], took(7000));
        TableTime::learn(&mut times, c(), took(5000));
        assert_eq!(times[r"C:\"], took(5000));
    }
}
