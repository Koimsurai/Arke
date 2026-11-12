//! Spring-loaded folders: hovering over a folder during a drag opens it, like Explorer and Finder.
//!
//! - On a path bar segment or sidebar folder, a thin bar fills over [`DELAY`] and then the folder
//!   opens; moving away before it fills cancels, so a brush-past doesn't jump in.
//! - On the Back button the same countdown goes back one folder (no need to drag out of the window
//!   after a wrong turn).
//! - If the drag ends without a drop (Esc, unaccepted target, another app), the tab returns to the
//!   folder where the drag started. After a drop it stays in the new folder.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, App, DragMoveEvent, Entity, Global, WeakEntity, Window,
    div, prelude::*, relative,
};

use crate::explorer::{DraggedFiles, Explorer};
use crate::theme;
use crate::workspace::Workspace;

/// Hover time before opening
pub(crate) const DELAY: Duration = Duration::from_millis(900);

/// What to do when the timer fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Open(PathBuf),
    Back,
}

/// Which tab acts: the list itself (path bar, Back in the list top bar) or the window's current tab
/// (sidebar).
#[derive(Clone)]
pub(crate) enum Target {
    Explorer(WeakEntity<Explorer>),
    Workspace(WeakEntity<Workspace>),
}

impl Target {
    fn view(&self, cx: &App) -> Option<Entity<Explorer>> {
        match self {
            Self::Explorer(e) => e.upgrade(),
            Self::Workspace(w) => w.upgrade().map(|w| w.read(cx).active_view().clone()),
        }
    }
}

struct Pending {
    key: String,
    since: Instant,
}

/// App-wide (only one drag at a time).
#[derive(Default)]
struct SpringState {
    pending: Option<Pending>,
    /// Where this drag was before its first auto-open; returned to if there's no drop
    origin: Option<(WeakEntity<Explorer>, PathBuf)>,
    /// This drag was dropped (no need to go back)
    dropped: bool,
    /// Cursor position at the last auto-open. The path bar relayouts after opening and a different
    /// target lands under the cursor, so the next countdown waits for real movement or levels would
    /// open in a chain.
    cooldown: Option<gpui::Point<gpui::Pixels>>,
}

/// Cursor movement needed after an auto-open before a new countdown starts
const MOVE_TO_REARM: f32 = 8.;

impl Global for SpringState {}

/// Called by drop targets while a drag passes over (`on_drag_move`): start the countdown on enter,
/// cancel on leave. `key` identifies the target (the same folder in the path bar and sidebar are
/// two targets).
pub(crate) fn hover(
    key: String,
    action: Action,
    target: Target,
    e: &DragMoveEvent<DraggedFiles>,
    window: &mut Window,
    cx: &mut App,
) {
    let inside = e.bounds.contains(&e.event.position);
    let state = cx.default_global::<SpringState>();
    if let Some(at) = state.cooldown {
        let moved = (e.event.position.x - at.x)
            .abs()
            .max((e.event.position.y - at.y).abs());
        if moved < gpui::px(MOVE_TO_REARM) {
            return;
        }
        state.cooldown = None;
    }
    let current = state.pending.as_ref().map(|p| p.key.as_str());
    if inside && current != Some(key.as_str()) {
        let since = Instant::now();
        state.pending = Some(Pending {
            key: key.clone(),
            since,
        });
        window.refresh();
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(DELAY).await;
                let _ = cx.update(|window, cx| fire(&key, since, &action, &target, window, cx));
            })
            .detach();
    } else if !inside && current == Some(key.as_str()) {
        state.pending = None;
        window.refresh();
    }
}

/// Timer fired: if still on the same target and still dragging, open it (or go back).
fn fire(
    key: &str,
    since: Instant,
    action: &Action,
    target: &Target,
    window: &mut Window,
    cx: &mut App,
) {
    let still = cx
        .try_global::<SpringState>()
        .and_then(|s| s.pending.as_ref())
        .is_some_and(|p| p.key == key && p.since == since);
    if !still || !cx.has_active_drag() {
        return;
    }
    let Some(view) = target.view(cx) else {
        return;
    };
    let here = view
        .read(cx)
        .display_path()
        .map(std::path::Path::to_path_buf);
    let state = cx.global_mut::<SpringState>();
    state.pending = None;
    state.cooldown = Some(window.mouse_position());
    if state.origin.is_none()
        && let Some(here) = here
    {
        state.origin = Some((view.downgrade(), here));
    }
    view.update(cx, |e, cx| match action {
        Action::Open(path) => e.navigate(path.clone(), arke_core::NavKind::New, cx),
        Action::Back => e.go_back(&crate::explorer::GoBack, window, cx),
    });
    window.refresh();
}

/// A drop happened (called from `drop_into`): no need to return to the original folder when the
/// drag ends.
pub(crate) fn note_drop(cx: &mut App) {
    cx.default_global::<SpringState>().dropped = true;
}

/// Cleanup after a drag ends (checked on every window redraw, run in a defer): without a drop and
/// after an auto-open, return to the original folder.
pub(crate) fn settle(cx: &mut App) {
    if cx.has_active_drag() {
        return;
    }
    let Some(state) = cx.try_global::<SpringState>() else {
        return;
    };
    if state.pending.is_none() && state.origin.is_none() && !state.dropped {
        return;
    }
    let state = std::mem::take(cx.global_mut::<SpringState>());
    if let (Some((view, path)), false) = (state.origin, state.dropped)
        && let Some(view) = view.upgrade()
    {
        view.update(cx, |e, cx| e.navigate(path, arke_core::NavKind::New, cx));
    }
}

/// Whether cleanup is needed (not dragging, but a countdown or origin remains).
pub(crate) fn needs_settle(cx: &App) -> bool {
    !cx.has_active_drag()
        && cx
            .try_global::<SpringState>()
            .is_some_and(|s| s.pending.is_some() || s.origin.is_some() || s.dropped)
}

/// The filling bar under the target being counted down (place inside the target's `relative()`).
pub(crate) fn bar(key: &str, cx: &App) -> Option<AnyElement> {
    let pending = cx.try_global::<SpringState>()?.pending.as_ref()?;
    if pending.key != key {
        return None;
    }
    let started = pending.since;
    // Progress from the countdown start (continues across redraws instead of restarting).
    let id = format!("spring-{key}-{:?}", started);
    Some(
        div()
            .absolute()
            .left_0()
            .bottom_0()
            .h(gpui::px(2.))
            .rounded(theme::R_FULL)
            .bg(theme::signal())
            .with_animation(
                gpui::ElementId::Name(id.into()),
                Animation::new(DELAY),
                |el, t| el.w(relative(t)),
            )
            .into_any_element(),
    )
}

/// Wrap the Back button: hovering during a drag counts down and goes back. `slot` tells the list
/// top bar button from the sidebar one.
pub(crate) fn back_target(
    slot: &str,
    target: Target,
    can_back: bool,
    button: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let key = format!("back:{slot}");
    div()
        .relative()
        .flex_none()
        .child(button)
        .children(bar(&key, cx))
        .when(can_back, |d| {
            d.on_drag_move(move |e: &DragMoveEvent<DraggedFiles>, window, cx| {
                hover(key.clone(), Action::Back, target.clone(), e, window, cx);
            })
        })
        .into_any_element()
}
