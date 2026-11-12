//! File drag and drop.
//!
//! Drag sources are list rows and grid cells; dragging an unselected item drags only that item.
//! Shelf items drag out too: they copy unless Shift is held, since the shelf only holds them.
//! Drop targets are folders, the other pane's list, breadcrumb segments, and sidebar entries.
//! Files dragged in from Explorer or the desktop arrive as `ExternalPaths`.
//!
//! Same rules as Explorer: same volume moves, different volume copies; Ctrl forces copy and
//! Shift forces move. The transfer itself runs through IFileOperation on a background thread.
//!
//! Dragging with the right button works like Explorer's: the drop asks whether to move, copy or
//! create shortcuts. GPUI only starts drags for the left button, so once the right button has moved
//! far enough the press is replayed to GPUI as a left press and move (see
//! [`Explorer::right_press_moved`]); from there it is the same drag, with the same targets, card and
//! spring-loaded folders. The real right-button release drops it (GPUI's drop handlers don't look at
//! which button was released), and [`RightDrag`] tells the drop to ask.
//!
//! The image shown when dragging into other apps is built in the background as soon as the drag
//! starts. If it isn't ready when the cursor leaves the window, the drag proceeds without it.

use crate::assets::Icon;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arke_core::filetype::{FileKind, MAX_THUMBNAIL_SOURCE, can_thumbnail, wants_system_icon};
use arke_core::thumbs::{THUMBNAIL_SIZE, Thumbnail, decode_thumbnail};
use gpui::{
    AnyElement, App, Context, DispatchPhase, ExternalPaths, IntoElement, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, PlatformInput, Point,
    Render, RenderImage, SharedString, Styled, WeakEntity, Window, canvas, div,
};
use rust_i18n::t;

use super::{Explorer, kind_icon, spawn_thread};
use crate::media::MediaKey;
use crate::shell::{DragImage, DragImageRequest};
use crate::theme::{self, UiFont as _};

/// Longest edge when decoding the drag image ourselves.
const DRAG_THUMBNAIL: u32 = 96;

/// Files being dragged. Paths are resolved from the source on drop; the selection can't change
/// mid-drag.
#[derive(Clone)]
pub(crate) struct DraggedFiles {
    pub(crate) source: WeakEntity<Explorer>,
    /// Index of the item the drag started on.
    pub(crate) ix: usize,
    /// Dragged from the shelf: these paths, with `source` (the active tab) running the drop.
    pub(crate) shelf: Option<ShelfDrag>,
}

/// Shelf items being dragged.
#[derive(Clone)]
pub(crate) struct ShelfDrag {
    pub(crate) paths: Arc<Vec<PathBuf>>,
    /// Icon for the drag card
    pub(crate) kind: FileKind,
}

impl DraggedFiles {
    /// A drag from the list of `source`, starting on item `ix`.
    pub(crate) const fn from_list(source: WeakEntity<Explorer>, ix: usize) -> Self {
        Self {
            source,
            ix,
            shelf: None,
        }
    }

    /// The dragged paths.
    pub(crate) fn paths(&self, cx: &App) -> Vec<PathBuf> {
        if let Some(shelf) = &self.shelf {
            return shelf.paths.as_ref().clone();
        }
        self.source
            .upgrade()
            .map(|e| e.read(cx).drag_paths(self.ix))
            .unwrap_or_default()
    }

    /// Whether a drop moves: shelf items copy unless Shift is held; list items follow
    /// Explorer's rules ([`drop_is_move`]).
    pub(crate) fn moves(&self, modifiers: Modifiers, paths: &[PathBuf], dest: &Path) -> bool {
        if self.shelf.is_some() {
            return modifiers.shift && !modifiers.control;
        }
        drop_is_move(modifiers, paths, dest)
    }
}

/// Files in the current drag, kept so they can be handed to Windows if the cursor leaves the
/// window.
pub(crate) struct PendingDragOut {
    pub(crate) paths: Vec<PathBuf>,
    /// Image that follows the cursor, once the background job finishes.
    pub(crate) image: Option<DragImage>,
    /// Drag generation; a finished image is only applied to the drag it was made for.
    id: u64,
}

impl gpui::Global for PendingDragOut {}

/// The drag has left the window and Windows OLE drag is running, so the in-app drag card is hidden.
pub(crate) struct DraggingOutside;

impl gpui::Global for DraggingOutside {}

static DRAG_ID: AtomicU64 = AtomicU64::new(0);

/// The current drag was started with the right button: dropping asks what to do.
pub(crate) struct RightDrag;

impl gpui::Global for RightDrag {}

/// Right button pressed on an item at `at`, not yet moved far enough to count as a drag.
/// Released in place, it opens the context menu (on release, like Explorer, so it can still
/// turn into a drag).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RightPress {
    pub(crate) at: Point<Pixels>,
}

/// How far the right button must move before the press becomes a drag (a little more than GPUI's
/// own threshold, so a shaky right click still opens the menu).
const RIGHT_DRAG_THRESHOLD: f64 = 4.;
/// Most bytes downloaded so remote items can be dragged into other programs.
const DRAG_STAGE_MAX: u64 = 64 * 1024 * 1024;

/// Inputs for the drag image, gathered on the UI thread without touching disk.
struct DragImageJob {
    request: DragImageRequest,
    /// Thumbnail or icon the list already loaded.
    ready: Option<Thumbnail>,
    /// No cached image, but the file is a decodable image on a local disk.
    decode: bool,
}

/// Converts a loaded image back to pixels (BGRA to RGBA).
fn thumbnail_of(image: &RenderImage) -> Option<Thumbnail> {
    let size = image.size(0);
    let mut rgba = image.as_bytes(0)?.to_vec();
    for p in rgba.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    Some(Thumbnail {
        width: u32::try_from(size.width.0).ok()?,
        height: u32::try_from(size.height.0).ok()?,
        rgba,
    })
}

/// Small card that follows the cursor during a drag.
pub(crate) struct DragPreview {
    label: SharedString,
    /// Dragged paths, used to tell whether a target is the same folder or volume.
    paths: Vec<PathBuf>,
    count: usize,
    kind: FileKind,
    /// Dragged from the shelf (copies unless Shift is held).
    shelf: bool,
    /// Offset of the press from the row's top-left. GPUI draws the drag view at cursor minus this
    /// offset, so without compensating, grabbing a wide row in the middle puts the card far away.
    grab: gpui::Point<gpui::Pixels>,
}

impl DragPreview {
    /// Builds the drag card and records the dragged paths for a possible OLE drag-out.
    pub(crate) fn start(
        dragged: &DraggedFiles,
        grab: gpui::Point<gpui::Pixels>,
        cx: &mut App,
    ) -> Self {
        let preview = Self {
            grab,
            ..Self::new(dragged, cx)
        };
        let paths = dragged.paths(cx);
        let id = DRAG_ID.fetch_add(1, Ordering::Relaxed) + 1;
        let count = paths.len();
        let first = paths.first().cloned();
        if paths.iter().any(|p| arke_core::remote::is_remote(p)) {
            if let Some(source) = dragged.source.upgrade() {
                Self::stage_remote(paths, id, &source.read(cx).services.clone(), cx);
            }
        } else {
            cx.set_global(PendingDragOut {
                paths,
                image: None,
                id,
            });
        }
        if dragged.shelf.is_some() {
            // Shelf items: Windows' own icon for the first one, nothing read from disk here.
            if let (Some(path), Some(source)) = (first, dragged.source.upgrade()) {
                let is_dir = preview.kind == FileKind::Folder;
                let job = shelf_image_job(path, is_dir, count);
                Self::prepare_image(job, id, &source.read(cx).services.clone(), cx);
            }
        } else if let Some(source) = dragged.source.upgrade() {
            let job = source.update(cx, |e, cx| e.drag_image_job(dragged.ix, count, cx));
            if let Some(job) = job {
                Self::prepare_image(job, id, &source.read(cx).services.clone(), cx);
            }
        }
        preview
    }

    /// Other programs need files on disk: remote items are downloaded first (up to
    /// [`DRAG_STAGE_MAX`]), and the drag can leave the window once their copies are ready.
    /// Drops inside Arke don't wait for this; they transfer from the server directly.
    fn stage_remote(paths: Vec<PathBuf>, id: u64, services: &super::Services, cx: &mut App) {
        let (tx, rx) = flume::bounded(1);
        let remotes = services.remotes.clone();
        spawn_thread("drag-stage", move || {
            let _ = tx.send(crate::remote_shell::stage_local_copies(
                &remotes,
                &paths,
                DRAG_STAGE_MAX,
            ));
        });
        cx.spawn(async move |cx| {
            let staged = rx.recv_async().await;
            cx.update(|cx| match staged {
                Ok(Ok(local)) if cx.has_active_drag() && DRAG_ID.load(Ordering::Relaxed) == id => {
                    cx.set_global(PendingDragOut {
                        paths: local,
                        image: None,
                        id,
                    });
                }
                Ok(Err(err)) => log::info!("remote items not staged for drag out: {err}"),
                _ => {}
            });
        })
        .detach();
    }

    /// Builds the drag image in the background and stores it if the same drag is still active.
    fn prepare_image(job: DragImageJob, id: u64, services: &super::Services, cx: &mut App) {
        let (tx, rx) = flume::bounded::<Option<DragImage>>(1);
        let fs = services.fs.clone();
        let shell = services.shell.clone();
        spawn_thread("drag-image", move || {
            let base = job.ready.or_else(|| {
                job.decode
                    .then(|| fs.read(&job.request.path, MAX_THUMBNAIL_SOURCE).ok())
                    .flatten()
                    .and_then(|bytes| decode_thumbnail(&bytes, DRAG_THUMBNAIL).ok())
            });
            let _ = tx.send(shell.drag_image(base, &job.request));
        });
        cx.spawn(async move |cx| {
            let Ok(Some(image)) = rx.recv_async().await else {
                return;
            };
            cx.update(|cx| {
                if cx
                    .try_global::<PendingDragOut>()
                    .is_some_and(|p| p.id == id)
                {
                    cx.global_mut::<PendingDragOut>().image = Some(image);
                }
            });
        })
        .detach();
    }

    pub(crate) fn new(dragged: &DraggedFiles, cx: &App) -> Self {
        let (paths, kind) = match &dragged.shelf {
            Some(shelf) => (shelf.paths.as_ref().clone(), shelf.kind),
            None => dragged.source.upgrade().map_or_else(
                || (Vec::new(), FileKind::Other),
                |e| {
                    let e = e.read(cx);
                    (e.drag_paths(dragged.ix), e.drag_kind(dragged.ix))
                },
            ),
        };
        let label = match paths.as_slice() {
            [one] => one
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            many => tn!("dnd.items", many.len()).into_owned(),
        };
        Self {
            label: label.into(),
            count: paths.len(),
            paths,
            kind,
            shelf: dragged.shelf.is_some(),
            grab: gpui::Point::default(),
        }
    }
}

/// Drag image for shelf items: the shell's icon for the first one.
fn shelf_image_job(path: PathBuf, is_dir: bool, count: usize) -> DragImageJob {
    let signal = theme::signal();
    let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    DragImageJob {
        request: DragImageRequest {
            path,
            is_dir,
            own_icon: false,
            count,
            badge: [channel(signal.r), channel(signal.g), channel(signal.b)],
        },
        ready: None,
        decode: false,
    }
}

impl DragPreview {
    /// What a drop would do: "move to" or "copy to" the folder, or "choose after drop" with Alt
    /// held.
    /// Over no folder (or the source folder) it shows a dimmed hint. Updates with target and
    /// modifiers.
    fn action_line(&self, window: &Window, cx: &App) -> gpui::Div {
        let dest = cx
            .try_global::<DropHover>()
            .filter(|h| {
                let at = window.mouse_position();
                (h.at.x - at.x).abs() < gpui::px(2.) && (h.at.y - at.y).abs() < gpui::px(2.)
            })
            .and_then(|h| h.dest.as_ref().map(|(p, _)| p.clone()))
            .filter(|d| useful_drop(&self.paths, d));
        let modifiers = window.modifiers();
        let (icon, text, strong) = match dest {
            None => (Icon::Folder, t!("dnd.no_target").into_owned(), false),
            Some(_) if modifiers.alt || cx.has_global::<RightDrag>() => {
                (Icon::Menu, t!("dnd.will_ask").into_owned(), true)
            }
            Some(dest) => {
                let name = dest.file_name().map_or_else(
                    || dest.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let moves = if self.shelf {
                    modifiers.shift && !modifiers.control
                } else {
                    drop_is_move(modifiers, &self.paths, &dest)
                };
                if moves {
                    (
                        Icon::Cut,
                        t!("dnd.menu_move", dest = name).into_owned(),
                        true,
                    )
                } else {
                    (
                        Icon::Copy,
                        t!("dnd.menu_copy", dest = name).into_owned(),
                        true,
                    )
                }
            }
        };
        let color = if strong {
            theme::text()
        } else {
            theme::text_subtle()
        };
        div()
            .flex()
            .items_center()
            .gap(theme::S_6)
            .text_size(theme::FS_12)
            .text_color(color)
            .child(
                gpui::svg()
                    .path(icon.path())
                    .size(theme::ICON_SM)
                    .text_color(color),
            )
            .child(text)
    }
}

/// Drop target under the cursor, for the drag card label. The smallest wins; folder rows beat list
/// backgrounds.
#[derive(Clone, Default)]
pub(crate) struct DropHover {
    /// Cursor position of this report, refreshed on every mouse move.
    at: gpui::Point<gpui::Pixels>,
    /// Target folder and its area.
    dest: Option<(PathBuf, f32)>,
}

impl gpui::Global for DropHover {}

/// Called by every visible drop target on each mouse move during a drag.
pub(crate) fn report_drop_target(dest: &Path, e: &gpui::DragMoveEvent<DraggedFiles>, cx: &mut App) {
    let pos = e.event.position;
    let hover = cx.default_global::<DropHover>();
    if hover.at != pos {
        *hover = DropHover {
            at: pos,
            dest: None,
        };
    }
    if e.bounds.contains(&pos) {
        let area = f32::from(e.bounds.size.width) * f32::from(e.bounds.size.height);
        if hover.dest.as_ref().is_none_or(|(_, a)| area < *a) {
            hover.dest = Some((dest.to_path_buf(), area));
        }
    }
}

impl Render for DragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Hidden while Windows draws its own drag image outside the window; otherwise it sticks to
        // the edge.
        if cx.has_global::<DraggingOutside>() {
            return div();
        }
        // Place the card just below-right of the cursor like Explorer, regardless of where the row
        // was grabbed.
        let card = div()
            .flex()
            .flex_col()
            .gap(theme::S_2)
            .px(theme::S_12)
            .py(theme::S_6)
            .rounded(theme::R_8)
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .child(kind_icon(
                        if self.count > 1 {
                            FileKind::Other
                        } else {
                            self.kind
                        },
                        theme::ICON,
                    ))
                    .child(self.label.clone()),
            )
            .child(self.action_line(window, cx))
            .child(
                div()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(t!("dnd.keys").to_string()),
            );
        div()
            .pl(self.grab.x + theme::S_12)
            .pt(self.grab.y + theme::S_12)
            .child(card)
    }
}

/// Volume or share root: the first path component.
fn volume(path: &Path) -> Option<String> {
    // Each server is its own volume: drops between servers copy.
    if let Some(url) = arke_core::remote::RemoteUrl::from_path(path) {
        return Some(url.origin());
    }
    match path.components().next()? {
        Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().to_ascii_lowercase()),
        _ => None,
    }
}

/// Whether a drop moves or copies, following Explorer's rules.
pub(crate) fn drop_is_move(modifiers: Modifiers, sources: &[PathBuf], dest: &Path) -> bool {
    if modifiers.control {
        return false;
    }
    if modifiers.shift {
        return true;
    }
    let dest_volume = volume(dest);
    sources.iter().all(|s| volume(s) == dest_volume)
}

/// Whether dropping on `dest` makes sense. Rejects the source folder and dropping a folder into
/// itself.
/// Used by `can_drop`, so rejected targets don't highlight and dropping there does nothing.
pub(crate) fn accepts_drop(value: &dyn std::any::Any, dest: &Path, cx: &App) -> bool {
    let Some(dragged) = value.downcast_ref::<DraggedFiles>() else {
        // External drags are always accepted; the same-folder check happens later.
        return true;
    };
    if dragged.source.upgrade().is_none() {
        return false;
    }
    useful_drop(&dragged.paths(cx), dest)
}

pub(crate) fn useful_drop(sources: &[PathBuf], dest: &Path) -> bool {
    !sources.is_empty()
        && !sources.iter().all(|p| p.parent() == Some(dest))
        && !sources.iter().any(|p| dest.starts_with(p))
}

/// Drops the dragged files into `dest`. Shared by all drop targets.
/// With Alt held, or after a right-button drag, shows a menu at the cursor to pick move, copy or
/// create shortcut.
pub(crate) fn drop_into(dragged: &DraggedFiles, dest: PathBuf, window: &mut Window, cx: &mut App) {
    crate::spring::note_drop(cx);
    let right = cx.has_global::<RightDrag>();
    if right {
        cx.remove_global::<RightDrag>();
    }
    let Some(source) = dragged.source.upgrade() else {
        return;
    };
    let paths = dragged.paths(cx);
    log::info!(
        "drop {} items into {} (right {right}, Alt {}, Ctrl {}, Shift {}{})",
        paths.len(),
        dest.display(),
        window.modifiers().alt,
        window.modifiers().control,
        window.modifiers().shift,
        if useful_drop(&paths, &dest) {
            ""
        } else {
            "; same folder, ignored"
        }
    );
    if window.modifiers().alt || right {
        let at = window.mouse_position();
        source.update(cx, |e, cx| e.open_drop_menu(paths, dest, at, window, cx));
        return;
    }
    let is_move = dragged.moves(window.modifiers(), &paths, &dest);
    if is_move && dragged.shelf.is_some() && useful_drop(&paths, &dest) {
        crate::shelf::ShelfStore::global(cx).update(cx, |s, _| s.note_moving(&paths));
    }
    source.update(cx, |e, cx| e.transfer_paths(paths, dest, is_move, cx));
}

/// Ends a right-button drag that was released where nothing takes a drop (or outside the window).
fn end_right_drag(window: &mut Window, cx: &mut App) {
    if cx.has_global::<RightDrag>() {
        cx.remove_global::<RightDrag>();
        cx.stop_active_drag(window);
    }
}

impl Explorer {
    /// Right button pressed on an item: remember it; the menu opens on release, or it becomes a
    /// drag once the mouse moves.
    pub(crate) fn note_right_press(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.right_press = Some(RightPress { at });
        cx.notify();
    }

    /// Right button released without dragging: the context menu, where it was released.
    pub(crate) fn right_press_released(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.right_press.take().is_some() {
            self.open_context_menu(at, window, cx);
        }
    }

    /// The mouse moved with the right button down: past the threshold, start the drag by replaying
    /// the press to GPUI as a left press and move, after this event (GPUI's mouse handlers aren't
    /// available while it is dispatching one).
    pub(crate) fn right_press_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(press) = self.right_press else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Right) {
            // Released somewhere we didn't hear about.
            self.right_press = None;
            cx.notify();
            return;
        }
        if (event.position - press.at).magnitude() <= RIGHT_DRAG_THRESHOLD {
            return;
        }
        self.right_press = None;
        let (from, to, modifiers) = (press.at, event.position, event.modifiers);
        cx.set_global(RightDrag);
        window.defer(cx, move |window, cx| {
            let _ = window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: from,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            let _ = window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: to,
                    pressed_button: Some(MouseButton::Left),
                    modifiers,
                }),
                cx,
            );
            // The item didn't start a drag (it is being renamed, say): nothing to drop.
            if !cx.has_active_drag() && cx.has_global::<RightDrag>() {
                cx.remove_global::<RightDrag>();
            }
            window.refresh();
        });
        cx.notify();
    }

    /// Window-wide mouse listeners while the right button is down on an item or a right drag runs:
    /// item rows only hear events over themselves.
    pub(crate) fn render_right_drag_listener(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.right_press.is_none() && !cx.has_global::<RightDrag>() {
            return None;
        }
        let this = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| (),
                move |_, (), window, _| {
                    let on_move = this.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        if cx.has_global::<RightDrag>() {
                            // No button down any more: released where no event reached us. (The
                            // replayed left move that starts the drag has a button.)
                            if e.pressed_button.is_none() && cx.has_active_drag() {
                                end_right_drag(window, cx);
                            }
                            return;
                        }
                        let _ =
                            on_move.update(cx, |this, cx| this.right_press_moved(e, window, cx));
                    });
                    let on_up = this;
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble || e.button != MouseButton::Right {
                            return;
                        }
                        if cx.has_global::<RightDrag>() {
                            // A drop target may still take this release (it runs in the same
                            // dispatch and clears the flag itself); otherwise the drag just ends.
                            window.defer(cx, end_right_drag);
                            return;
                        }
                        let _ = on_up.update(cx, |this, cx| {
                            this.right_press_released(e.position, window, cx);
                        });
                    });
                },
            )
            .absolute()
            .size_0()
            .into_any_element(),
        )
    }
}

/// Hands files dropped from Explorer or the desktop to `target`, the tab that received them.
pub(crate) fn drop_external_into(
    paths: &ExternalPaths,
    dest: PathBuf,
    target: &gpui::Entity<Explorer>,
    window: &mut Window,
    cx: &mut App,
) {
    let paths: Vec<PathBuf> = paths.paths().to_vec();
    if window.modifiers().alt {
        let at = window.mouse_position();
        target.update(cx, |e, cx| e.open_drop_menu(paths, dest, at, window, cx));
        return;
    }
    let is_move = drop_is_move(window.modifiers(), &paths, &dest);
    target.update(cx, |e, cx| e.transfer_paths(paths, dest, is_move, cx));
}

impl Explorer {
    /// Paths dragged when starting from item `ix`: the whole selection if it's selected, else just
    /// that item.
    pub(crate) fn drag_paths(&self, ix: usize) -> Vec<PathBuf> {
        if self.search_mode {
            return self.path_at(ix).map(|(p, _)| p).into_iter().collect();
        }
        let selected = self
            .view()
            .and_then(|v| v.get(ix).map(|e| self.selection.contains(&e.name)))
            .unwrap_or(false);
        if selected {
            self.selected_targets()
                .into_iter()
                .map(|(p, _)| p)
                .collect()
        } else {
            self.path_at(ix).map(|(p, _)| p).into_iter().collect()
        }
    }

    /// What to build the drag image from for item `ix`, the one under the cursor.
    fn drag_image_job(
        &mut self,
        ix: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> Option<DragImageJob> {
        let (path, is_dir) = self.path_at(ix)?;
        let signal = theme::signal();
        let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
        let mut job = DragImageJob {
            request: DragImageRequest {
                path,
                is_dir,
                own_icon: false,
                count,
                badge: [channel(signal.r), channel(signal.g), channel(signal.b)],
            },
            ready: None,
            decode: false,
        };
        // Thumbnails and per-file icons only on local disks, like the list. Elsewhere use type
        // icons without reading the file.
        let entry = (!self.search_mode)
            .then(|| self.view().and_then(|v| v.get(ix).cloned()))
            .flatten();
        let dir = job.request.path.parent().map(Path::to_path_buf);
        if let (Some(entry), Some(dir)) = (entry, dir)
            && self.media_ok(&dir, cx)
        {
            job.request.own_icon = wants_system_icon(&entry);
            // The row's name may be a nested tree path: key on the dragged file itself.
            job.ready = MediaKey::at(job.request.path.clone(), &entry, Some(THUMBNAIL_SIZE))
                .and_then(|key| self.media.read(cx).get_or_nearby(&key))
                .and_then(|image| thumbnail_of(&image));
            job.decode = job.ready.is_none() && can_thumbnail(&entry);
        }
        Some(job)
    }

    fn drag_kind(&self, ix: usize) -> FileKind {
        self.view()
            .and_then(|v| v.get(ix).map(arke_core::filetype::file_kind))
            .unwrap_or(FileKind::Other)
    }

    /// Copies or moves to `dest`, skipping drops into the source folder or into the dragged folder
    /// itself.
    pub(crate) fn transfer_paths(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        is_move: bool,
        cx: &mut Context<Self>,
    ) {
        // Not highlighted as a target, so no message either.
        if !useful_drop(&sources, &dest) {
            return;
        }
        self.run_transfer(sources, dest, is_move, cx);
    }
}

#[cfg(test)]
impl Explorer {
    pub(crate) fn on_row_mouse_down_for_test(
        &mut self,
        ix: usize,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_row_mouse_down(ix, event, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(control: bool, shift: bool) -> Modifiers {
        Modifiers {
            control,
            shift,
            ..Modifiers::default()
        }
    }

    #[test]
    fn same_volume_moves_other_volume_copies() {
        let src = [PathBuf::from(r"C:\a\x.txt")];
        assert!(drop_is_move(mods(false, false), &src, Path::new(r"C:\b")));
        assert!(
            drop_is_move(mods(false, false), &src, Path::new(r"c:\B")),
            "drive letter case doesn't matter"
        );
        assert!(!drop_is_move(mods(false, false), &src, Path::new(r"D:\b")));
        assert!(!drop_is_move(
            mods(false, false),
            &src,
            Path::new(r"\\nas\share")
        ));
        let remote = [PathBuf::from("sftp://u@h/a/x.txt")];
        assert!(drop_is_move(
            mods(false, false),
            &remote,
            Path::new("sftp://u@h/b")
        ));
        assert!(!drop_is_move(
            mods(false, false),
            &remote,
            Path::new("sftp://u@other/b")
        ));
        assert!(!drop_is_move(
            mods(false, false),
            &remote,
            Path::new(r"C:")
        ));
        // Ctrl always copies, Shift always moves.
        assert!(!drop_is_move(mods(true, false), &src, Path::new(r"C:\b")));
        assert!(drop_is_move(mods(false, true), &src, Path::new(r"D:\b")));
    }
}
