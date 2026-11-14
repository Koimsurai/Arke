//! Transition animations and their timing tokens.
//!
//! Every animation in Arke uses one of these durations and easings:
//!
//! - [`FAST`] (120 ms): a control changing state in place (switch thumb).
//! - [`BASE`] (180 ms): a surface entering or moving: menus, flyouts, dialogs, toasts, Quick Look,
//!   panels, the view switch crossfade, tabs and tiles sliding after a reorder.
//! - [`EXIT`] (120 ms): a surface leaving after it was dismissed. Shorter than entering, so closing
//!   never feels like waiting.
//! - [`FLASH`] (1600 ms): a one-off highlight fading out (a folder that was just pinned).
//!
//! Entering eases out ([`ease_out`]: fast start, soft landing). Leaving moves away eased in
//! ([`ease_in`]) but fades linearly, so it visibly starts on the very next frame (an eased-in fade
//! stays near full opacity for its first half and reads as a delay).
//! Offsets are [`NUDGE`] (flyouts dropping from their button), [`SLIDE`] (menus, palette, dialogs)
//! and [`SLIDE_LG`] (Quick Look card, preview pane).
//!
//! Nothing that should be instant is animated: folder switches, tab switches, list navigation,
//! selection, typing, scrolling, resizing by drag. Hover and press styles are instant too (GPUI has
//! no style transitions, and Windows list hover is immediate).
//!
//! Exits: GPUI drops an element as soon as render stops returning it, so a surface that fades out is
//! kept as a [`Leaving`] ghost. The real state closes at once (focus, key bindings and every "is it
//! open" check see it closed); the ghost is only drawn, under a cover that takes its clicks (see
//! [`leave`]), for [`EXIT`], then a timer drops it. Choosing something (a menu command, a dialog
//! button) closes instantly; only dismissing (Escape, clicking outside, the toggle button) fades.
//!
//! Reduced motion: [`sync_app`] mirrors Windows "Animation effects" into `App::set_reduce_motion`,
//! so every `with_animation` draws its final state at once, and no ghosts are made.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, ElementId, InteractiveElement, IntoElement,
    ParentElement, Styled, div, px,
};

/// A control changing state in place.
pub(crate) const FAST: Duration = Duration::from_millis(120);
/// A surface entering or moving.
pub(crate) const BASE: Duration = Duration::from_millis(180);
/// A dismissed surface leaving.
pub(crate) const EXIT: Duration = Duration::from_millis(120);
/// A one-off highlight fading out.
pub(crate) const FLASH: Duration = Duration::from_millis(1600);

/// Entrance offset for flyouts dropping from their button.
pub(crate) const NUDGE: f32 = 4.;
/// Entrance offset for menus, the palette and dialogs.
pub(crate) const SLIDE: f32 = 8.;
/// Entrance offset for large surfaces (Quick Look card, preview pane).
pub(crate) const SLIDE_LG: f32 = 12.;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Re-read the Windows animation setting (at startup, before the app exists).
pub(crate) fn refresh_system_setting() {
    ENABLED.store(crate::win::animations_enabled(), Ordering::Relaxed);
}

/// Re-read the Windows animation setting and apply it to GPUI, so every `with_animation` (including
/// ones that don't check [`enabled`]) skips to its end state when animations are off. Called at
/// startup and whenever a window is activated.
pub(crate) fn sync_app(cx: &mut App) {
    refresh_system_setting();
    cx.set_reduce_motion(!enabled());
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Tests: animate anyway (to check exit ghosts and other timing logic).
#[cfg(test)]
pub(crate) fn force_for_test(on: bool) {
    FORCED.set(on);
}

/// Whether to animate now. Never in tests (unless forced), so frames are deterministic.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    {
        FORCED.get()
    }
    #[cfg(not(test))]
    {
        ENABLED.load(Ordering::Relaxed)
    }
}

/// Decelerating curve for entering: quick start, soft landing (quintic).
pub(crate) fn ease_out(t: f32) -> f32 {
    1. - (1. - t).powi(5)
}

/// Accelerating curve for leaving: starts gently, then goes (quadratic).
pub(crate) fn ease_in(t: f32) -> f32 {
    t * t
}

/// Surfaces entering or moving ([`BASE`], eased out).
pub(crate) fn enter() -> Animation {
    Animation::new(BASE).with_easing(ease_out)
}

/// A control changing state in place ([`FAST`], eased out).
pub(crate) fn fast() -> Animation {
    Animation::new(FAST).with_easing(ease_out)
}

/// A dismissed surface leaving ([`EXIT`], linear progress: [`slide_out`] eases the motion in).
pub(crate) fn exit() -> Animation {
    Animation::new(EXIT)
}

/// Element at progress `t` (0 to 1): fades in and slides from offset (`dx`, `dy`) into place.
///
/// The offset is set with `left` and `top`: on an element placed with `right` or `bottom` (a corner
/// card) it would pin the element to the top left instead, so animate a child of the placed box
/// ([`corner_card`]).
pub(crate) fn slide_in<E: Styled>(el: E, t: f32, dx: f32, dy: f32) -> E {
    if !enabled() {
        return el;
    }
    el.opacity(t.clamp(0., 1.))
        .left(px(dx * (1. - t)))
        .top(px(dy * (1. - t)))
}

/// A card in the window's bottom right corner (the first-run card, the update notice), 360 px wide.
/// The box is placed here; the card inside it may slide in, since its offsets move it within the box.
pub(crate) fn corner_card(card: impl IntoElement) -> AnyElement {
    gpui::div()
        .absolute()
        .bottom(crate::theme::S_16)
        .right(crate::theme::S_16)
        .w(px(360.))
        .child(card)
        .into_any_element()
}

/// Element at progress `t` (0 to 1, linear): fades out while sliding (eased in) toward offset
/// (`dx`, `dy`).
pub(crate) fn slide_out<E: Styled>(el: E, t: f32, dx: f32, dy: f32) -> E {
    if !enabled() {
        return el;
    }
    let moved = ease_in(t);
    el.opacity((1. - t).clamp(0., 1.))
        .left(px(dx * moved))
        .top(px(dy * moved))
}

/// Fade `el` in on appearance (sliding from the offset). Replays when `id` changes or the element is
/// mounted again, so only use it for surfaces that appear because the user opened them.
pub(crate) fn appear<E>(el: E, id: impl Into<ElementId>, dx: f32, dy: f32) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    if !enabled() {
        return el.into_any_element();
    }
    el.with_animation(id, enter(), move |el, t| slide_in(el, t, dx, dy))
        .into_any_element()
}

/// A dismissed surface fading out toward (`dx`, `dy`), drawn [`inert`].
pub(crate) fn leave<E>(el: E, id: impl Into<ElementId>, dx: f32, dy: f32) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    inert(el.with_animation(id, exit(), move |el, t| slide_out(el, t, dx, dy)))
}

/// `el` under a transparent cover that takes the pointer, so a ghost's rows and buttons don't
/// highlight or run anything while it fades. GPUI has no way to switch hit testing off for a
/// subtree, and a ghost is drawn by the same code as the live surface, handlers included.
pub(crate) fn inert(el: impl IntoElement) -> AnyElement {
    div()
        .relative()
        .child(el)
        .child(
            div()
                .id("leaving-cover")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude(),
        )
        .into_any_element()
}

/// A transition the user just started (switching the view, opening the preview pane). The element
/// animates only while the cue is fresh, so mounting it again later (switching tabs, a folder with
/// the pane already open) draws it in place instead of replaying the entrance.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cue {
    serial: u64,
    at: Option<Instant>,
}

impl Cue {
    /// The user started the transition now (no-op when animations are off).
    pub(crate) fn fire(&mut self) {
        if enabled() {
            self.serial += 1;
            self.at = Some(Instant::now());
        }
    }

    /// Changes each time the cue fires: an animation id that replays per transition.
    pub(crate) const fn serial(&self) -> u64 {
        self.serial
    }

    /// Whether the transition is still playing at `now`.
    pub(crate) fn live_at(&self, now: Instant) -> bool {
        self.at
            .is_some_and(|at| now.saturating_duration_since(at) < BASE + Duration::from_millis(60))
    }

    /// `el` entering (fading in from offset `dx`, `dy`) while the cue is live, otherwise as is.
    pub(crate) fn wrap<E>(&self, el: E, name: &'static str, dx: f32, dy: f32) -> AnyElement
    where
        E: Styled + IntoElement + 'static,
    {
        if self.live_at(Instant::now()) {
            let id = ElementId::NamedInteger(name.into(), self.serial);
            el.with_animation(id, enter(), move |el, t| slide_in(el, t, dx, dy))
                .into_any_element()
        } else {
            el.into_any_element()
        }
    }
}

static NEXT_LEAVING: AtomicU64 = AtomicU64::new(1);

/// A surface that was dismissed and is fading out: what it needs to draw itself once more. The
/// owner keeps it in an `Option<Leaving<T>>` next to the real state and draws it with [`leave`].
pub(crate) struct Leaving<T> {
    pub(crate) value: T,
    serial: u64,
}

impl<T: 'static> Leaving<T> {
    /// Start fading `value` out: stores the ghost in `slot(view)` and drops it after [`EXIT`]. Does
    /// nothing (the surface just vanishes) when animations are off.
    pub(crate) fn start<V: 'static>(
        view: &mut V,
        value: T,
        slot: fn(&mut V) -> &mut Option<Self>,
        cx: &mut Context<V>,
    ) {
        if !enabled() {
            return;
        }
        let serial = NEXT_LEAVING.fetch_add(1, Ordering::Relaxed);
        *slot(view) = Some(Self { value, serial });
        cx.spawn(async move |this, cx| {
            // One frame of slack: the exit animation starts on the next frame, not right now.
            cx.background_executor()
                .timer(EXIT + Duration::from_millis(20))
                .await;
            let _ = this.update(cx, |view, cx| {
                let ghost = slot(view);
                if ghost.as_ref().is_some_and(|g| g.serial == serial) {
                    *ghost = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Animation id for this ghost, so the next ghost of the same surface replays the exit.
    pub(crate) fn id(&self, name: &'static str) -> ElementId {
        ElementId::NamedInteger(name.into(), self.serial)
    }
}

/// Fraction of a [`BASE`] slide (eased out) still ahead `elapsed` after it started: 1 at the start,
/// 0 once done. A slide that is retargeted mid-way adds `offset * remaining` to its new offset, so
/// the element continues from where it is drawn instead of jumping.
pub(crate) fn remaining(elapsed: Duration) -> f32 {
    let t = (elapsed.as_secs_f32() / BASE.as_secs_f32()).clamp(0., 1.);
    1. - ease_out(t)
}

/// A slide in progress: its starting offset, when it started, and its animation serial.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slide<T> {
    pub(crate) from: T,
    pub(crate) started: Instant,
    pub(crate) serial: usize,
}

impl<T> Slide<T> {
    /// Whether it is still moving at `now` (with a frame of slack). A finished slide must not be
    /// drawn with its animation again: an element mounted anew would replay it from the start.
    pub(crate) fn live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < BASE + Duration::from_millis(60)
    }
}

impl Slide<f32> {
    /// Offset still to cover at `now`.
    pub(crate) fn left(&self, now: Instant) -> f32 {
        self.from * remaining(now.saturating_duration_since(self.started))
    }
}

impl Slide<(f32, f32)> {
    /// Offset still to cover at `now`.
    pub(crate) fn left(&self, now: Instant) -> (f32, f32) {
        let r = remaining(now.saturating_duration_since(self.started));
        (self.from.0 * r, self.from.1 * r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easings_run_from_zero_to_one() {
        for ease in [ease_out, ease_in] {
            assert!(ease(0.).abs() < 1e-6, "starts at 0");
            assert!((ease(1.) - 1.).abs() < 1e-6, "ends at 1");
            let mut last = 0.;
            for i in 1..=20 {
                let v = ease(i as f32 / 20.);
                assert!(v >= last, "never goes back");
                last = v;
            }
        }
        // Entering is ahead of linear, leaving behind it.
        assert!(ease_out(0.5) > 0.5);
        assert!(ease_in(0.5) < 0.5);
    }

    #[test]
    fn exits_are_shorter_than_entrances() {
        assert!(EXIT < BASE);
        assert!(FAST <= EXIT);
    }

    #[test]
    fn a_retargeted_slide_continues_from_where_it_is() {
        assert!((remaining(Duration::ZERO) - 1.).abs() < 1e-6);
        assert!(remaining(BASE).abs() < 1e-6);
        assert!(remaining(BASE * 2).abs() < 1e-6);
        let start = Instant::now();
        let slide = Slide {
            from: 30.,
            started: start,
            serial: 1,
        };
        let mid = slide.left(start + BASE / 4);
        assert!(mid > 0. && mid < 30., "partway: {mid}");
        let grid = Slide {
            from: (10., -20.),
            started: start,
            serial: 2,
        };
        let (x, y) = grid.left(start + BASE);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6);
    }

    #[test]
    fn cues_play_only_right_after_they_fire() {
        force_for_test(true);
        let mut cue = Cue::default();
        let now = Instant::now();
        assert!(!cue.live_at(now), "nothing fired yet");
        cue.fire();
        assert!(cue.live_at(Instant::now()));
        assert!(
            !cue.live_at(Instant::now() + BASE * 2),
            "over once it has played"
        );
        force_for_test(false);
        let mut off = Cue::default();
        off.fire();
        assert!(!off.live_at(Instant::now()), "reduced motion never plays");
    }
}
