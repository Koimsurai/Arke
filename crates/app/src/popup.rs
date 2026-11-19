//! Menus in windows of their own.
//!
//! A menu level, each submenu, and the dropdown lists that are menus are each a borderless popup
//! window (`WindowKind::AnchoredPopup`, see `third_party/README.md`) owned by the window the menu
//! belongs to. A popup is never activated and never takes focus: keys still go to that window,
//! which keeps the menu's key context and actions, and a text box keeps editing. The panel is
//! drawn as before (rounded, shadow, fade) with room around it for the shadow, and the window is
//! sized to it. It can reach past the main window, up to the edge of the monitor's work area,
//! like a Windows menu.
//!
//! Placement is done in physical screen pixels, so monitors with different scales line up:
//! - at a point (a right-click): below and right of it, flipped up or left only where it doesn't
//!   fit the work area, then kept inside it ([`place_at`]);
//! - beside a submenu row: on the side its parent level opened to, otherwise on the other side of
//!   that level, never past the levels further out; if neither side fits it covers its parent,
//!   as in Windows ([`place_beside`]);
//! - under a control (a dropdown): below it, or above when it doesn't fit ([`place_below`]).
//!
//! The owner keeps the menu state; a popup's content is drawn from it on every frame, so rows
//! filled in later show up in the popup, which then fits itself to them. A popup closes itself
//! once its content is gone.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gpui::popup::{PopupAnchor, PopupConstraintAdjustment, PopupGravity, PopupOptions};

use crate::theme::UiFont as _;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Point, Render, Size,
    Styled as _, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div,
    point, px, size,
};

/// Room around a panel for its shadow (`theme::elev_float`: 24 px of blur, 8 px down).
const SHADOW_X: f32 = 24.;
const SHADOW_TOP: f32 = 16.;
const SHADOW_BOTTOM: f32 = 32.;
/// A submenu overlaps its parent level by this much, as before.
const OVERLAP: f32 = 2.;
/// Space between a control and the dropdown under it.
const DROP_GAP: f32 = 4.;
/// Widest a popup window starts, so its panel lays out at its natural width before the window is
/// fitted to it.
const START_W: f32 = 640.;

/// A rectangle in physical screen pixels (right and bottom exclusive).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScreenRect {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

impl ScreenRect {
    pub(crate) const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }

    pub(crate) const fn width(self) -> i32 {
        self.right - self.left
    }

    pub(crate) const fn height(self) -> i32 {
        self.bottom - self.top
    }

    const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// How far `(x, y)` is from this rectangle (0 inside), squared.
    fn distance_to(self, x: i32, y: i32) -> i64 {
        let dx = i64::from((self.left - x).max(0).max(x - (self.right - 1)));
        let dy = i64::from((self.top - y).max(0).max(y - (self.bottom - 1)));
        dx * dx + dy * dy
    }
}

/// A monitor: its whole area, its work area (without the taskbar and docked bars) and its scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Monitor {
    pub(crate) bounds: ScreenRect,
    pub(crate) work: ScreenRect,
    pub(crate) scale: f32,
}

/// The monitor `(x, y)` is on, or the nearest one when it is on none.
pub(crate) fn monitor_at(monitors: &[Monitor], x: i32, y: i32) -> Monitor {
    monitors
        .iter()
        .find(|m| m.bounds.contains(x, y))
        .or_else(|| monitors.iter().min_by_key(|m| m.bounds.distance_to(x, y)))
        .copied()
        .unwrap_or(Monitor {
            bounds: ScreenRect::new(0, 0, 1920, 1080),
            work: ScreenRect::new(0, 0, 1920, 1080),
            scale: 1.,
        })
}

/// One coordinate of a popup `len` long that starts at `at` and runs toward the end of
/// `start..end`: flipped to end at `at` when it doesn't fit, kept inside when neither fits.
fn flip_or_clamp(at: i32, len: i32, start: i32, end: i32) -> i32 {
    let place = if at + len <= end {
        at
    } else if at - len >= start {
        at - len
    } else {
        end - len
    };
    // A point off the work area (on the taskbar) still gives a place inside it.
    place.min(end - len).max(start)
}

/// A menu opened at `at` (a right-click): its top-left corner there, flipped left or up only
/// where it doesn't fit `work`, and kept inside it when neither way fits. `size` is physical.
pub(crate) fn place_at(at: (i32, i32), size: (i32, i32), work: ScreenRect) -> ScreenRect {
    let left = flip_or_clamp(at.0, size.0, work.left, work.right);
    let top = flip_or_clamp(at.1, size.1, work.top, work.bottom);
    ScreenRect::new(left, top, size.0, size.1)
}

/// A submenu beside its parent level `parent`, its first row at `row_top`: on the side the parent
/// opened to (`left`), overlapping it by `overlap`; on the other side of the parent when that
/// doesn't fit `work`, never past the levels further out. When neither fits, it takes the side with
/// more room and is kept inside, covering the parent as in Windows. Shifted up when it would run
/// past the bottom. Returns the place and whether it opens to the left.
pub(crate) fn place_beside(
    parent: ScreenRect,
    row_top: i32,
    size: (i32, i32),
    work: ScreenRect,
    left: bool,
    overlap: i32,
) -> (ScreenRect, bool) {
    let (width, height) = size;
    let right_x = parent.right - overlap;
    let left_x = parent.left + overlap - width;
    let fits_right = right_x + width <= work.right;
    let fits_left = left_x >= work.left;
    let goes_left = match (left, fits_left, fits_right) {
        (true, true, _) | (false, true, false) => true,
        (false, _, true) | (true, false, true) => false,
        // Neither side: the roomier one, kept inside the work area.
        (_, false, false) => parent.left - work.left > work.right - parent.right,
    };
    let x = if goes_left { left_x } else { right_x };
    let x = x.min(work.right - width).max(work.left);
    let y = row_top.min(work.bottom - height).max(work.top);
    (ScreenRect::new(x, y, width, height), goes_left)
}

/// A dropdown under `control`, `gap` below it; above when it doesn't fit below but does above.
/// Left edges line up, or right edges with `align_right`; kept inside `work`.
pub(crate) fn place_below(
    control: ScreenRect,
    size: (i32, i32),
    work: ScreenRect,
    align_right: bool,
    gap: i32,
) -> ScreenRect {
    let (width, height) = size;
    let below = control.bottom + gap;
    let above = control.top - gap - height;
    let y = if below + height <= work.bottom || above < work.top {
        below.min(work.bottom - height).max(work.top)
    } else {
        above
    };
    let x = if align_right {
        control.right - width
    } else {
        control.left
    };
    let x = x.min(work.right - width).max(work.left);
    ScreenRect::new(x, y, width, height)
}

/// Where a popup goes.
pub(crate) enum Anchor {
    /// Its top-left corner at a point (a right-click), in physical screen pixels.
    At(i32, i32),
    /// Beside a row of another popup (a submenu). `row` says where the row is in that popup's
    /// window (logical pixels); `pad` is this panel's padding above its first row, so the first
    /// row lines up with it.
    Beside {
        parent: Weak<PopupState>,
        row: Rc<dyn Fn() -> Option<Bounds<Pixels>>>,
        pad: Pixels,
    },
    /// Under a control (a dropdown), in physical screen pixels; right edges line up with
    /// `align_right`.
    Below {
        control: ScreenRect,
        align_right: bool,
    },
}

/// The popups of one menu (a level and its submenus). Each redraws when another moves, since a
/// submenu sits beside its parent and a parent row's chevron points where its submenu opened.
#[derive(Clone, Default)]
pub(crate) struct Group(Rc<RefCell<Vec<AnyWindowHandle>>>);

impl Group {
    /// Redraws every popup of the group, after the current update.
    pub(crate) fn refresh(&self, cx: &mut App) {
        let windows = self.0.borrow().clone();
        cx.defer(move |cx| {
            for window in windows {
                let _ = window.update(cx, |_, window, _| window.refresh());
            }
        });
    }
}

/// What a popup draws: its panel, given the tallest it may be. `None` once the menu (or this
/// level of it) is gone, and the popup closes.
pub(crate) type Content = Rc<dyn Fn(Pixels, &mut Window, &mut App) -> Option<AnyElement>>;
/// Closes the menu, for a press beside the panel (on its shadow), which counts as outside it.
pub(crate) type Dismiss = Rc<dyn Fn(&mut App)>;

/// An open popup: where it is and which way it opened.
pub(crate) struct PopupState {
    anchor: Anchor,
    /// The monitor it is on: where it opened, or where a moved popup is now.
    monitor: Cell<Monitor>,
    /// The panel in its window (logical pixels), once drawn.
    panel: Cell<Option<Bounds<Pixels>>>,
    /// The panel's place on the screen, once placed.
    placed: Cell<Option<ScreenRect>>,
    /// The window's place (the panel and the room for its shadow).
    frame: Cell<Option<ScreenRect>>,
    left: Cell<bool>,
    window: Cell<Option<AnyWindowHandle>>,
    group: Group,
    /// A popup opened at a point that was moved since (a drag image following the pointer).
    moved_to: Cell<Option<Moved>>,
}

/// Where a moved popup goes (physical screen pixels): its top-left corner at `at`, or, where it
/// doesn't fit, its far corner at `flip` (the other side of the pointer, so it never covers it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Moved {
    pub(crate) at: (i32, i32),
    pub(crate) flip: (i32, i32),
}

/// A panel of `size` placed at `moved`, kept inside `work`.
fn place_moved(moved: Moved, size: (i32, i32), work: ScreenRect) -> ScreenRect {
    let left = if moved.at.0 + size.0 <= work.right {
        moved.at.0
    } else {
        moved.flip.0 - size.0
    };
    let top = if moved.at.1 + size.1 <= work.bottom {
        moved.at.1
    } else {
        moved.flip.1 - size.1
    };
    let left = left.clamp(work.left, (work.right - size.0).max(work.left));
    let top = top.clamp(work.top, (work.bottom - size.1).max(work.top));
    ScreenRect::new(left, top, size.0, size.1)
}

impl PopupState {
    /// Moves a popup opened at a point to `at` (physical screen pixels) on its next frame, which
    /// is asked for here.
    pub(crate) fn move_to(&self, to: Moved, cx: &mut App) {
        if self.moved_to.get() == Some(to) {
            return;
        }
        self.moved_to.set(Some(to));
        // Onto a monitor with another scale: drawn again, it gets that monitor's size.
        let monitor = monitor_at(&monitors(), to.at.0, to.at.1);
        if monitor != self.monitor.get() {
            self.monitor.set(monitor);
            self.group.refresh(cx);
            return;
        }
        // Placed already: the window moves as it is, without drawing again (a redraw racing the
        // move leaves parts of the last frame behind). A frame drawn later places it the same way.
        let (Some(panel), Some(frame), Some(window)) =
            (self.panel.get(), self.frame.get(), self.window.get())
        else {
            self.group.refresh(cx);
            return;
        };
        let Some((placed, _)) = self.target(panel.size) else {
            return;
        };
        let (dx, dy) = (
            placed.left - self.placed.get().map_or(placed.left, |p| p.left),
            placed.top - self.placed.get().map_or(placed.top, |p| p.top),
        );
        let moved = ScreenRect {
            left: frame.left + dx,
            top: frame.top + dy,
            right: frame.right + dx,
            bottom: frame.bottom + dy,
        };
        self.placed.set(Some(placed));
        self.frame.set(Some(moved));
        if let Ok(Some(hwnd)) = window.update(cx, |_, window, _| hwnd_of(window)) {
            // Not now: the window answers the move at once, while the app is busy with the event
            // that moved it.
            cx.foreground_executor()
                .spawn(async move { crate::win_popup::move_popup(hwnd, moved) })
                .detach();
        }
    }

    /// Whether it opened to the left of its parent level.
    pub(crate) const fn opens_left(&self) -> bool {
        self.left.get()
    }

    pub(crate) const fn window(&self) -> Option<AnyWindowHandle> {
        self.window.get()
    }

    /// The panel's place on the screen (physical pixels), once placed.
    #[cfg(test)]
    pub(crate) const fn placed(&self) -> Option<ScreenRect> {
        self.placed.get()
    }

    /// Tests: where the panel as last drawn is on the screen (physical pixels): its window's place
    /// plus its place in the window.
    #[cfg(test)]
    pub(crate) fn drawn_on_screen(&self) -> Option<ScreenRect> {
        let (frame, panel) = (self.frame.get()?, self.panel.get()?);
        let scale = self.monitor.get().scale;
        let left = frame.left + phys(f32::from(panel.origin.x), scale);
        let top = frame.top + phys(f32::from(panel.origin.y), scale);
        let size = (
            (f32::from(panel.size.width) * scale).ceil() as i32,
            (f32::from(panel.size.height) * scale).ceil() as i32,
        );
        Some(ScreenRect::new(left, top, size.0, size.1))
    }

    /// The tallest the panel may be: its monitor's work area, less the room for the shadow.
    fn max_height(&self) -> Pixels {
        let monitor = self.monitor.get();
        px(monitor.work.height() as f32 / monitor.scale - SHADOW_TOP - SHADOW_BOTTOM)
    }

    /// Where the panel goes for a panel of `panel` size, and whether it opens to the left.
    fn target(&self, panel: Size<Pixels>) -> Option<(ScreenRect, bool)> {
        let monitor = self.monitor.get();
        let scale = monitor.scale;
        let size = (
            (f32::from(panel.width) * scale).ceil() as i32,
            (f32::from(panel.height) * scale).ceil() as i32,
        );
        let work = monitor.work;
        Some(match &self.anchor {
            Anchor::At(x, y) => match self.moved_to.get() {
                Some(moved) => (place_moved(moved, size, work), false),
                None => (place_at((*x, *y), size, work), false),
            },
            Anchor::Below {
                control,
                align_right,
            } => (
                place_below(*control, size, work, *align_right, phys(DROP_GAP, scale)),
                false,
            ),
            Anchor::Beside { parent, row, pad } => {
                let parent = parent.upgrade()?;
                let (parent_panel, parent_frame) = (parent.placed.get()?, parent.frame.get()?);
                let row = row()?;
                let row_top = parent_frame.top
                    + phys(f32::from(row.top()), parent.monitor.get().scale)
                    - phys(f32::from(*pad), scale);
                place_beside(
                    parent_panel,
                    row_top,
                    size,
                    work,
                    parent.left.get(),
                    phys(OVERLAP, scale),
                )
            }
        })
    }

    /// After the panel was laid out at `bounds` in its window: moves the window where it goes.
    fn drawn(&self, bounds: Bounds<Pixels>, window: &Window, cx: &mut App) {
        self.panel.set(Some(bounds));
        let Some((panel, left)) = self.target(bounds.size) else {
            return;
        };
        if self.placed.get() == Some(panel) && self.left.get() == left {
            return;
        }
        let scale = self.monitor.get().scale;
        // The panel's resting place in its window, not `bounds.origin`: a panel that slides in is
        // first drawn off that place, and a window fitted to that first frame would leave the
        // panel short of its target once the slide ends (a dropdown a few pixels too low).
        let frame = ScreenRect {
            left: panel.left - phys(SHADOW_X, scale),
            top: panel.top - phys(SHADOW_TOP, scale),
            right: panel.right + phys(SHADOW_X, scale),
            bottom: panel.bottom + phys(SHADOW_BOTTOM, scale),
        };
        self.placed.set(Some(panel));
        self.frame.set(Some(frame));
        self.left.set(left);
        if let Some(hwnd) = hwnd_of(window) {
            // Not now: the window answers the move at once, and GPUI is drawing it.
            cx.foreground_executor()
                .spawn(async move { crate::win_popup::move_popup(hwnd, frame) })
                .detach();
        }
        self.group.refresh(cx);
    }

    /// Whether `position` (in its window) is on the panel.
    fn on_panel(&self, position: Point<Pixels>) -> bool {
        self.panel.get().is_some_and(|b| b.contains(&position))
    }
}

fn phys(logical: f32, scale: f32) -> i32 {
    (logical * scale).round() as i32
}

/// Opens a popup owned by window `owner`. Call it outside entity updates (from `App::defer`): the
/// popup draws its content at once, and the content reads its owner's state. `reads_owner`: the
/// content reads the state of views in `owner` (`Single::ensure`, the context menu), rather than
/// something handed to it. `None` when the window could not be made.
pub(crate) fn open(
    owner: AnyWindowHandle,
    anchor: Anchor,
    group: &Group,
    content: Content,
    dismiss: Dismiss,
    reads_owner: bool,
    cx: &mut App,
) -> Option<Rc<PopupState>> {
    let monitor = match &anchor {
        Anchor::At(x, y) => monitor_at(&monitors(), *x, *y),
        // A control across two monitors: the one with the edge the list lines up with, where
        // it can stay lined up, not the one its left edge happens to be on.
        Anchor::Below {
            control,
            align_right,
        } => {
            let x = if *align_right {
                control.right - 1
            } else {
                control.left
            };
            monitor_at(&monitors(), x, i32::midpoint(control.top, control.bottom))
        }
        Anchor::Beside { parent, .. } => parent.upgrade()?.monitor.get(),
    };
    let state = Rc::new(PopupState {
        anchor,
        monitor: Cell::new(monitor),
        panel: Cell::new(None),
        placed: Cell::new(None),
        frame: Cell::new(None),
        left: Cell::new(false),
        window: Cell::new(None),
        group: group.clone(),
        moved_to: Cell::new(None),
    });
    // Made hidden over the monitor's work area, where it gets that monitor's scale, and large
    // enough for the panel's natural size; its first frame fits and places it, and shows it.
    let work = monitor.work;
    let (origin, owner_scale) = owner
        .update(cx, |_, window, _| {
            (window.bounds().origin, window.scale_factor())
        })
        .ok()?;
    let anchor_rect = Bounds::new(
        point(
            px(work.left as f32 / owner_scale) - origin.x,
            px(work.top as f32 / owner_scale) - origin.y,
        ),
        Size::default(),
    );
    let start = size(
        px((work.width() as f32 / monitor.scale).min(START_W)),
        px(work.height() as f32 / monitor.scale),
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(Point::default(), start))),
        titlebar: None,
        focus: false,
        show: false,
        kind: WindowKind::AnchoredPopup(PopupOptions {
            parent: owner,
            anchor_rect,
            anchor: PopupAnchor::TopLeft,
            gravity: PopupGravity::BottomRight,
            constraint_adjustment: PopupConstraintAdjustment::empty(),
            offset: Point::default(),
            grab: false,
        }),
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let view_state = Rc::clone(&state);
    let handle = cx
        .open_window(options, move |_, cx| {
            cx.new(|_| PopupView {
                state: view_state,
                content,
                dismiss,
                owner: reads_owner.then_some(owner),
            })
        })
        .inspect_err(|err| log::warn!("could not open a menu window: {err:#}"))
        .ok()?;
    let handle: AnyWindowHandle = handle.into();
    state.window.set(Some(handle));
    group.0.borrow_mut().push(handle);
    // It drew before its handle was known: place it again now that the group can refresh it.
    group.refresh(cx);
    Some(state)
}

/// The window of a menu with a single level (tab, sidebar and history menus, a text box's edit
/// menu, the dropdowns that are menus), kept in the owner's state next to the menu. A new menu gets
/// a new one; the window of the one before closes itself.
pub(crate) struct Single {
    serial: u64,
    owner: Option<AnyWindowHandle>,
    state: Option<Rc<PopupState>>,
    opening: bool,
    failed: bool,
    watch: Vec<gpui::Subscription>,
}

impl Single {
    pub(crate) fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            serial: NEXT.fetch_add(1, Ordering::Relaxed),
            owner: None,
            state: None,
            opening: false,
            failed: false,
            watch: Vec::new(),
        }
    }

    /// The popup's window, once open.
    pub(crate) fn window(&self) -> Option<AnyWindowHandle> {
        self.state.as_ref()?.window()
    }

    /// Tests: where the popup's panel was placed on the screen (physical pixels), once placed.
    #[cfg(test)]
    pub(crate) fn placed(&self) -> Option<ScreenRect> {
        self.state.as_ref()?.placed()
    }

    /// Tests: where the popup's panel as last drawn is on the screen.
    #[cfg(test)]
    pub(crate) fn drawn_on_screen(&self) -> Option<ScreenRect> {
        self.state.as_ref()?.drawn_on_screen()
    }

    /// The window the menu belongs to, where its rows run (`listener_in`).
    pub(crate) const fn owner(&self) -> Option<AnyWindowHandle> {
        self.owner
    }

    /// From the owner's render while the menu is open in `window`: opens its popup once, placed at
    /// `anchor`. `slot` finds this again in the owner; `content` draws the panel (given the tallest
    /// it may be) and runs in the popup; `close` closes the menu, and also runs when `window` stops
    /// being the active window or moves, as a Windows menu closes then.
    pub(crate) fn ensure<T: 'static>(
        &mut self,
        anchor: impl FnOnce() -> Anchor,
        slot: fn(&mut T) -> Option<&mut Self>,
        content: fn(&mut T, Pixels, &mut Window, &mut Context<T>) -> Option<AnyElement>,
        close: fn(&mut T, &mut Window, &mut Context<T>),
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
        let serial = self.serial;
        let this = cx.weak_entity();
        let content: Content = Rc::new(move |max_h, window, cx| {
            this.update(cx, |owner, cx| {
                if slot(owner).is_some_and(|single| single.serial == serial) {
                    content(owner, max_h, window, cx)
                } else {
                    None
                }
            })
            .ok()
            .flatten()
        });
        self.open_once(anchor, slot, content, true, close, window, cx);
    }

    /// [`Self::ensure`] for a panel built elsewhere and handed over in `content`, which doesn't
    /// read the owner (a settings dropdown's list, built while the page draws).
    pub(crate) fn ensure_handed<T: 'static>(
        &mut self,
        anchor: impl FnOnce() -> Anchor,
        slot: fn(&mut T) -> Option<&mut Self>,
        content: Content,
        close: fn(&mut T, &mut Window, &mut Context<T>),
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
        self.open_once(anchor, slot, content, false, close, window, cx);
    }

    /// This menu's number, different for every menu.
    pub(crate) const fn serial(&self) -> u64 {
        self.serial
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the parts of `ensure`, passed on"
    )]
    fn open_once<T: 'static>(
        &mut self,
        anchor: impl FnOnce() -> Anchor,
        slot: fn(&mut T) -> Option<&mut Self>,
        content: Content,
        reads_owner: bool,
        close: fn(&mut T, &mut Window, &mut Context<T>),
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
        if self.watch.is_empty() {
            self.owner = Some(window.window_handle());
            self.watch = close_with_window(window, cx, close);
        }
        if self.state.is_some() || self.opening || self.failed {
            return;
        }
        self.opening = true;
        let (serial, owner, anchor) = (self.serial, window.window_handle(), anchor());
        let this = cx.weak_entity();
        // Not now: the popup draws its content as it opens, which may read the owner.
        App::defer(cx, move |cx| {
            let dismiss: Dismiss = {
                let this = this.clone();
                Rc::new(move |cx| {
                    let _ = owner.update(cx, |_, window, cx| {
                        this.update(cx, |owner, cx| close(owner, window, cx))
                    });
                })
            };
            let state = open(
                owner,
                anchor,
                &Group::default(),
                content,
                dismiss,
                reads_owner,
                cx,
            );
            let _ = this.update(cx, |owner, cx| {
                if let Some(single) = slot(owner).filter(|single| single.serial == serial) {
                    single.opening = false;
                    single.failed = state.is_none();
                    single.state = state;
                }
                // The owner draws again and learns the popup's window: closing the menu later
                // redraws that window so it goes, even if nothing else made the owner draw in
                // between (a settings dropdown stayed open after a click beside it).
                cx.notify();
            });
        });
    }
}

/// Runs `close` when `window` stops being the active window (a click in another program, on the
/// taskbar, Alt+Tab) or moves or changes size, as a Windows menu closes then. Keep the
/// subscriptions while the menu is open.
pub(crate) fn close_with_window<T: 'static>(
    window: &mut Window,
    cx: &mut Context<T>,
    close: fn(&mut T, &mut Window, &mut Context<T>),
) -> Vec<gpui::Subscription> {
    let opened_at = window.bounds();
    vec![
        cx.observe_window_activation(window, move |this, window, cx| {
            if !window.is_window_active() {
                close(this, window, cx);
            }
        }),
        // GPUI reports bounds on activation too; only a real move or resize counts.
        cx.observe_window_bounds(window, move |this, window, cx| {
            if window.bounds() != opened_at {
                close(this, window, cx);
            }
        }),
    ]
}

/// A popup's root view: its panel, with room around it for the shadow.
pub(crate) struct PopupView {
    state: Rc<PopupState>,
    content: Content,
    dismiss: Dismiss,
    /// The window whose views the content reads, drawn again after this popup draws.
    owner: Option<AnyWindowHandle>,
}

impl Render for PopupView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // GPUI takes the last window that drew a view for the window it lives in: events it emits
        // are handled there, and closing that window leaves it with none. Drawing the owner's
        // window again after this one keeps that window the views' own.
        if let Some(owner) = self.owner {
            App::defer(cx, move |cx| {
                let _ = owner.update(cx, |_, window, _| window.refresh());
            });
        }
        let Some(content) = (self.content)(self.state.max_height(), window, cx) else {
            window.remove_window();
            return div().into_any_element();
        };
        let measured = Rc::clone(&self.state);
        let beside = |state: Rc<PopupState>, dismiss: Dismiss| {
            move |e: &MouseDownEvent, _: &mut Window, cx: &mut App| {
                if !state.on_panel(e.position) {
                    dismiss(cx);
                }
            }
        };
        div()
            .id("popup")
            .size_full()
            // What the main window's root gives everything in it.
            .text_color(crate::theme::text())
            .ui_font()
            .text_size(crate::theme::fs_14())
            // A press on the shadow around the panel is outside the menu.
            .on_mouse_down(
                MouseButton::Left,
                beside(Rc::clone(&self.state), Rc::clone(&self.dismiss)),
            )
            .on_mouse_down(
                MouseButton::Right,
                beside(Rc::clone(&self.state), Rc::clone(&self.dismiss)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(SHADOW_X))
                    .top(px(SHADOW_TOP))
                    // Where the panel was laid out; the window is fitted to it. (A canvas
                    // stretched over it measured twice its height.)
                    .on_children_prepainted(move |bounds, window, cx| {
                        if let Some(panel) = bounds.first() {
                            measured.drawn(*panel, window, cx);
                        }
                    })
                    .child(content),
            )
            .into_any_element()
    }
}

/// `position` in `window` (logical pixels) on the screen, in physical pixels.
pub(crate) fn to_screen(window: &Window, position: Point<Pixels>) -> (i32, i32) {
    let origin = window.bounds().origin;
    let scale = window.scale_factor();
    (
        phys(f32::from(origin.x + position.x), scale),
        phys(f32::from(origin.y + position.y), scale),
    )
}

/// `bounds` in `window` (logical pixels) on the screen, in physical pixels.
pub(crate) fn rect_to_screen(window: &Window, bounds: Bounds<Pixels>) -> ScreenRect {
    let (left, top) = to_screen(window, bounds.origin);
    let (right, bottom) = to_screen(window, bounds.bottom_right());
    ScreenRect {
        left,
        top,
        right,
        bottom,
    }
}

/// Lets the mouse through popup `window` (a menu fading out): presses reach Arke's window below.
pub(crate) fn pass_mouse_through(window: &Window) {
    if let Some(hwnd) = hwnd_of(window) {
        crate::win_popup::pass_mouse_through(hwnd);
    }
}

/// Like `cx.listener`, but `f` runs in window `main`, the one the popup belongs to, not in the
/// popup the event arrived in: commands focus, open dialogs and place things there. Does nothing
/// without a `main` window.
pub(crate) fn listener_in<T: 'static, E: ?Sized + 'static>(
    main: Option<AnyWindowHandle>,
    cx: &Context<T>,
    f: impl Fn(&mut T, &E, &mut Window, &mut Context<T>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let this = cx.weak_entity();
    move |event, _, cx| {
        let Some(main) = main else {
            return;
        };
        let _ = main.update(cx, |_, window, cx| {
            this.update(cx, |this, cx| f(this, event, window, cx))
        });
    }
}

fn hwnd_of(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(test))]
fn monitors() -> Vec<Monitor> {
    crate::win_popup::monitors()
}

#[cfg(test)]
thread_local! {
    static TEST_MONITORS: RefCell<Vec<Monitor>> = RefCell::new(vec![Monitor {
        bounds: ScreenRect::new(0, 0, 1920, 1080),
        work: ScreenRect::new(0, 0, 1920, 1040),
        scale: 1.,
    }]);
}

/// Tests: the monitors popups are placed on (one 1920x1080 monitor unless set).
#[cfg(test)]
pub(crate) fn set_test_monitors(monitors: Vec<Monitor>) {
    TEST_MONITORS.with_borrow_mut(|m| *m = monitors);
}

#[cfg(test)]
fn monitors() -> Vec<Monitor> {
    TEST_MONITORS.with_borrow(Clone::clone)
}

/// Tests: the popup windows open now, oldest first.
#[cfg(test)]
pub(crate) fn open_popups(cx: &App) -> Vec<gpui::WindowHandle<PopupView>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<PopupView>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Monitor, ScreenRect, monitor_at, place_at, place_below, place_beside};

    const WORK: ScreenRect = ScreenRect::new(0, 0, 1920, 1040);

    #[test]
    fn a_menu_opens_at_the_pointer_and_flips_only_where_it_does_not_fit() {
        // Room everywhere: top-left corner at the pointer.
        assert_eq!(
            place_at((500, 300), (240, 400), WORK),
            ScreenRect::new(500, 300, 240, 400)
        );
        // Near the right edge: flipped left only.
        assert_eq!(
            place_at((1800, 300), (240, 400), WORK),
            ScreenRect::new(1560, 300, 240, 400)
        );
        // Near the bottom right corner: flipped up and left.
        assert_eq!(
            place_at((1800, 900), (240, 400), WORK),
            ScreenRect::new(1560, 500, 240, 400)
        );
        // Fits neither below nor above: kept inside the work area, bottom edge on its bottom.
        assert_eq!(
            place_at((500, 500), (240, 800), WORK),
            ScreenRect::new(500, 240, 240, 800)
        );
        // Taller than the work area: starts at its top (the level scrolls).
        assert_eq!(place_at((500, 500), (240, 1200), WORK).top, 0);
    }

    #[test]
    fn a_submenu_that_does_not_fit_opens_left_of_its_own_level() {
        // Level 0 at 1200..1440, level 1 opened right of it at 1438..1678.
        let level1 = ScreenRect::new(1438, 300, 240, 400);
        assert_eq!(
            place_beside(
                ScreenRect::new(1200, 280, 240, 500),
                300,
                (240, 400),
                WORK,
                false,
                2
            ),
            (level1, false)
        );
        // Level 2 doesn't fit right of level 1: left of level 1, covering level 0 like Windows,
        // not past all levels.
        let (level2, left) = place_beside(level1, 340, (300, 300), WORK, false, 2);
        assert!(left);
        assert_eq!(level2, ScreenRect::new(1140, 340, 300, 300));
        assert_eq!(level2.right, level1.left + 2, "flush with its own level");
        // Level 3 keeps going left.
        let (level3, left) = place_beside(level2, 360, (240, 200), WORK, true, 2);
        assert!(left);
        assert_eq!(level3.right, level2.left + 2);
    }

    #[test]
    fn a_submenu_going_left_turns_right_at_the_left_edge() {
        let parent = ScreenRect::new(100, 300, 240, 400);
        let (rect, left) = place_beside(parent, 320, (240, 300), WORK, true, 2);
        assert!(!left, "no room on the left");
        assert_eq!(rect.left, parent.right - 2);
    }

    #[test]
    fn a_submenu_with_no_room_on_either_side_covers_its_parent() {
        let work = ScreenRect::new(0, 0, 600, 1040);
        let parent = ScreenRect::new(150, 100, 300, 400);
        let (rect, _) = place_beside(parent, 120, (400, 200), work, false, 2);
        assert!(
            rect.left >= work.left && rect.right <= work.right,
            "{rect:?}"
        );
    }

    #[test]
    fn a_submenu_near_the_bottom_shifts_up() {
        let parent = ScreenRect::new(400, 600, 240, 400);
        let (rect, _) = place_beside(parent, 900, (240, 300), WORK, false, 2);
        assert_eq!(rect.bottom, WORK.bottom);
    }

    #[test]
    fn a_dropdown_opens_below_its_control_or_above_near_the_bottom() {
        let control = ScreenRect::new(300, 100, 120, 32);
        assert_eq!(
            place_below(control, (200, 300), WORK, false, 4),
            ScreenRect::new(300, 136, 200, 300)
        );
        assert_eq!(
            place_below(control, (200, 300), WORK, true, 4),
            ScreenRect::new(220, 136, 200, 300)
        );
        let low = ScreenRect::new(300, 900, 120, 32);
        assert_eq!(
            place_below(low, (200, 300), WORK, false, 4),
            ScreenRect::new(300, 596, 200, 300)
        );
    }

    /// Two monitors side by side: a 100% one on the left, a 150% one on the right whose taskbar
    /// takes 72 physical pixels at the bottom.
    fn two_monitors() -> Vec<Monitor> {
        vec![
            Monitor {
                bounds: ScreenRect::new(0, 0, 1920, 1080),
                work: ScreenRect::new(0, 0, 1920, 1040),
                scale: 1.,
            },
            Monitor {
                bounds: ScreenRect::new(1920, 0, 2880, 1620),
                work: ScreenRect::new(1920, 0, 2880, 1548),
                scale: 1.5,
            },
        ]
    }

    #[test]
    fn menus_stay_on_the_monitor_they_open_on() {
        let monitors = two_monitors();
        // Near the right edge of the left monitor: flipped left there, not spilled onto the right
        // one.
        let left = monitor_at(&monitors, 1850, 500);
        assert_eq!(left.scale, 1.);
        assert_eq!(place_at((1850, 500), (240, 400), left.work).right, 1850);
        // On the right monitor at 150%: a 240x400 logical menu is 360x600 physical, and near its
        // bottom it flips up above its taskbar.
        let right = monitor_at(&monitors, 4700, 1400);
        assert_eq!(right.scale, 1.5);
        let size = ((240. * right.scale) as i32, (400. * right.scale) as i32);
        let rect = place_at((4700, 1400), size, right.work);
        assert_eq!(rect, ScreenRect::new(4340, 800, 360, 600));
        assert!(rect.bottom <= 1548);
        // A submenu near the right monitor's right edge opens left of its own level.
        let level = ScreenRect::new(4400, 300, 360, 600);
        let (sub, left) = place_beside(level, 330, size, right.work, false, 3);
        assert!(left);
        assert_eq!(sub.right, level.left + 3);
        // Off every monitor (a monitor just unplugged): the nearest one.
        assert_eq!(monitor_at(&monitors, 9000, 100).scale, 1.5);
    }
}
