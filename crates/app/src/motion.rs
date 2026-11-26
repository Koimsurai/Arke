//! Transition animations and their timing tokens.
//!
//! Every animation in Arke uses one of these durations and easings:
//!
//! - [`FAST`] (120 ms): a control changing state in place (switch thumb).
//! - [`MENU`] (80 ms): a context menu, submenu or tooltip appearing. A short fade in place, no
//!   slide, like Windows 11 menus: a menu is opened to pick from right away, so it shouldn't
//!   travel.
//! - [`BASE`] (180 ms): a surface entering or moving: flyouts, dialogs, toasts, Quick Look,
//!   panels, the sidebar expanding, a new split pane, the view switch crossfade, tabs and tiles
//!   sliding after a reorder.
//! - [`EXIT`] (120 ms): a surface leaving after it was dismissed. Shorter than entering, so closing
//!   never feels like waiting.
//! - [`FLASH`] (1600 ms): a one-off highlight fading out (a folder that was just pinned).
//!
//! Entering eases out ([`ease_out`]: fast start, soft landing). A panel sliding by its own width
//! (the sidebar) eases out more gently ([`drawer`]), so the slide is seen. Leaving moves away
//! eased in ([`ease_in`]) but fades linearly, so it visibly starts on the very next frame (an
//! eased-in fade stays near full opacity for its first half and reads as a delay).
//! Offsets are [`NUDGE`] (flyouts dropping from their button), [`SLIDE`] (palette, dialogs) and
//! [`SLIDE_LG`] (Quick Look card, preview pane, a new split pane). Context menus and tooltips
//! don't move, entering or leaving. Whatever moves is laid out in its final place at once: only
//! its offset and opacity animate, so it can be used (and the content beside it has its new size)
//! from the first frame.
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
    ParentElement, Styled, Window, div, px,
};

/// A control changing state in place.
pub(crate) const FAST: Duration = Duration::from_millis(120);
/// A context menu appearing.
pub(crate) const MENU: Duration = Duration::from_millis(80);
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
    // The scrollbar setting rides along: same moments, no polling.
    crate::scrollbar::refresh_system_setting();
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

/// Decelerating curve for a panel sliding by its own width (cubic). Over a couple of hundred pixels
/// [`ease_out`] covers two thirds of the way in the first two frames, which reads as a jump.
pub(crate) fn ease_out_soft(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

/// A spring a little under critical damping: it passes the target by about a tenth, comes back
/// a hair short and settles, as a soft thing that is pushed into a new shape (a dragged favorite
/// changing between a tile and a row). 0 at the start, 1 at the end; between, it may leave
/// [0, 1], so use it only for sizes and places, never for opacity.
pub(crate) fn spring(t: f32) -> f32 {
    const ZETA: f32 = 0.55;
    const OMEGA: f32 = 11.;
    if t >= 1. {
        return 1.;
    }
    let damped = OMEGA * (1. - ZETA * ZETA).sqrt();
    let decay = (-ZETA * OMEGA * t).exp();
    1. - decay * ((damped * t).cos() + ZETA * OMEGA / damped * (damped * t).sin())
}

/// Accelerating curve for leaving: starts gently, then goes (quadratic).
pub(crate) fn ease_in(t: f32) -> f32 {
    t * t
}

/// Surfaces entering or moving ([`BASE`], eased out).
pub(crate) fn enter() -> Animation {
    Animation::new(BASE).with_easing(ease_out)
}

/// A panel sliding in by its own width, the sidebar ([`BASE`], eased with [`ease_out_soft`]).
pub(crate) fn drawer() -> Animation {
    Animation::new(BASE).with_easing(ease_out_soft)
}

/// A context menu appearing ([`MENU`], eased out: most of the fade lands on the first frame, so
/// it reads as instant without popping). Draw it with [`fade_in`].
pub(crate) fn menu_enter() -> Animation {
    Animation::new(MENU).with_easing(ease_out)
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

/// Element at progress `t` (0 to 1): fades in where it is.
pub(crate) fn fade_in<E: Styled>(el: E, t: f32) -> E {
    if !enabled() {
        return el;
    }
    el.opacity(t.clamp(0., 1.))
}

/// A card in the window's bottom right corner (the first-run card, the update notice), 360 px wide.
/// The box is placed here; the card inside it may slide in, since its offsets move it within the box.
pub(crate) fn corner_card(card: impl IntoElement) -> AnyElement {
    gpui::div()
        .absolute()
        .bottom(crate::theme::s_16())
        .right(crate::theme::s_16())
        .w(crate::theme::scaled_px(360.))
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
/// the pane already open) draws it in place instead of replaying the entrance. It is drawn from
/// the cue's own clock by styling the element in place ([`Cue::slide`], [`drop_in`]), never by
/// wrapping it in an animation for a while: taking the wrapper away mounts everything inside anew,
/// and a press made before that frame would lose its release.
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

    /// Tests: play the transition from now (`true`) or have it over (`false`), without firing
    /// again.
    #[cfg(test)]
    pub(crate) fn set_playing_for_test(&mut self, playing: bool) {
        self.at = playing.then(Instant::now);
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

    /// `el` entering over [`BASE`] (fading in from offset `dx`, `dy`, progress eased with `ease`),
    /// styled in place: the element tree is the same while it plays and afterwards, so nothing in
    /// it is mounted anew when it ends. Asks `window` for the next frame while it plays (call it
    /// while rendering the view that owns the cue) and for none afterwards.
    pub(crate) fn slide<E: Styled>(
        &self,
        el: E,
        window: &Window,
        ease: fn(f32) -> f32,
        dx: f32,
        dy: f32,
    ) -> E {
        if let Some(t) = self.progress_at(Instant::now()) {
            window.request_animation_frame();
            slide_in(el, ease(t), dx, dy)
        } else {
            el
        }
    }

    /// [`Cue::progress_at`] now, for drawing code that has no `Window`: while it is `Some`, the
    /// view that owns the cue has to ask for the next frame itself.
    pub(crate) fn progress(&self) -> Option<f32> {
        self.progress_at(Instant::now())
    }

    /// Linear progress (0 to 1) through [`BASE`] of the transition at `now`; `None` before it
    /// fired and once it is over.
    pub(crate) fn progress_at(&self, now: Instant) -> Option<f32> {
        let elapsed = now.saturating_duration_since(self.at?);
        (elapsed < BASE).then(|| elapsed.as_secs_f32() / BASE.as_secs_f32())
    }
}

/// Rows just revealed under their heading (a tree node, a sidebar section, the Shelf) at linear
/// progress `t` of a cue: each fades in as it drops from [`NUDGE`] above, eased out. The box each
/// row is put in has no id, so nothing inside it is mounted anew when the rows are drawn without
/// it again: a click on a row during the transition still lands.
pub(crate) fn drop_in(rows: Vec<AnyElement>, t: f32) -> impl Iterator<Item = AnyElement> {
    let t = ease_out(t);
    rows.into_iter()
        .map(move |row| slide_in(div().child(row), t, 0., -NUDGE).into_any_element())
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

/// A slide in progress: its starting offset and when it started.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slide<T> {
    pub(crate) from: T,
    pub(crate) started: Instant,
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

/// Rows of a list that slide to their new places when the list changes under a drag, wherever
/// the change moved them (a reorder, a row coming or going above them). Each row is drawn through
/// [`Flip::row`], which notes where it was drawn; [`Flip::moved`] says the list is about to
/// change, and on the next frame each row that moved starts from where it was drawn before, as
/// tiles and tabs do. Rows are laid out in their final place at once: only the drawing moves.
///
/// Off until [`Flip::start`] (and once [`Flip::stop`]ped, after its slides end): then rows are
/// drawn as they are, with nothing noted.
pub(crate) struct Flip<K> {
    state: std::cell::RefCell<FlipState<K>>,
}

struct FlipState<K> {
    on: bool,
    /// Where each row was drawn last frame (its top, with its slide).
    drawn: std::collections::HashMap<K, f32>,
    /// Taken by [`Flip::moved`]: where the rows were before the change. Kept for one frame.
    before: Option<(std::collections::HashMap<K, f32>, bool)>,
    slides: std::collections::HashMap<K, Slide<f32>>,
}

impl<K> Default for Flip<K> {
    fn default() -> Self {
        Self {
            state: std::cell::RefCell::new(FlipState {
                on: false,
                drawn: std::collections::HashMap::new(),
                before: None,
                slides: std::collections::HashMap::new(),
            }),
        }
    }
}

impl<K: Eq + std::hash::Hash + Clone + 'static> Flip<K> {
    /// A drag starts: rows note where they are drawn.
    pub(crate) fn start(&self) {
        let mut st = self.state.borrow_mut();
        st.on = true;
        st.drawn.clear();
        st.before = None;
        st.slides.clear();
    }

    /// The drag ended: slides still running finish, nothing new is noted.
    pub(crate) fn stop(&self) {
        self.state.borrow_mut().on = false;
    }

    /// The list is about to change: the rows that move slide from where they are drawn now.
    pub(crate) fn moved(&self) {
        if !enabled() {
            return;
        }
        let mut st = self.state.borrow_mut();
        if !st.drawn.is_empty() {
            st.before = Some((st.drawn.clone(), false));
        }
    }

    /// Called once a frame, before the rows are built: the positions taken by [`Self::moved`]
    /// are used by the frame after it only, and finished slides go.
    pub(crate) fn frame(&self) {
        let mut st = self.state.borrow_mut();
        match &mut st.before {
            Some((_, seen)) if *seen => st.before = None,
            Some((_, seen)) => *seen = true,
            None => {}
        }
        let now = Instant::now();
        st.slides.retain(|_, s| s.live(now));
        if !st.on && st.before.is_none() && st.slides.is_empty() {
            st.drawn.clear();
        }
    }

    /// Whether rows are drawn through [`Self::row`] now: during a drag and while slides run.
    pub(crate) fn wanted(&self) -> bool {
        let st = self.state.borrow();
        st.on || st.before.is_some() || !st.slides.is_empty()
    }

    /// `child`, the row known as `key`, drawn where it slides (or as it is when not [`Self::wanted`]).
    pub(crate) fn row(self: &std::rc::Rc<Self>, key: K, child: AnyElement) -> AnyElement {
        if !self.wanted() {
            return child;
        }
        FlipRow {
            key,
            flip: std::rc::Rc::clone(self),
            child,
        }
        .into_any_element()
    }

    /// The row `key` is laid out with its top at `y`: how far down from there it is drawn now.
    fn place(&self, key: &K, y: f32) -> f32 {
        let now = Instant::now();
        let mut st = self.state.borrow_mut();
        let was = st
            .before
            .as_mut()
            .and_then(|(before, _)| before.remove(key));
        if let Some(was) = was {
            if (was - y).abs() > 0.5 {
                st.slides.insert(
                    key.clone(),
                    Slide {
                        from: was - y,
                        started: now,
                    },
                );
            } else {
                st.slides.remove(key);
            }
        }
        let dy = st
            .slides
            .get(key)
            .filter(|s| s.live(now))
            .map_or(0., |s| s.left(now));
        if st.on {
            st.drawn.insert(key.clone(), y + dy);
        }
        dy
    }

    #[cfg(test)]
    pub(crate) fn sliding_from(&self, key: &K) -> Option<f32> {
        let now = Instant::now();
        self.state
            .borrow()
            .slides
            .get(key)
            .filter(|s| s.live(now))
            .map(|s| s.from)
    }
}

/// A row drawn through [`Flip::row`]: laid out as its child is, drawn moved by its slide.
struct FlipRow<K> {
    key: K,
    flip: std::rc::Rc<Flip<K>>,
    child: AnyElement,
}

impl<K: Eq + std::hash::Hash + Clone + 'static> IntoElement for FlipRow<K> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<K: Eq + std::hash::Hash + Clone + 'static> gpui::Element for FlipRow<K> {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        // The child's own layout: the row takes the same place as without the wrapper.
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let dy = self.flip.place(&self.key, f32::from(bounds.origin.y));
        if dy != 0. {
            window.request_animation_frame();
        }
        window.with_element_offset(gpui::point(px(0.), px(dy)), |window| {
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spring_overshoots_and_settles() {
        assert!(spring(0.).abs() < 1e-6, "starts at 0");
        assert!((spring(1.) - 1.).abs() < 1e-6, "ends at 1");
        let peak = (0..=100)
            .map(|i| spring(i as f32 / 100.))
            .fold(0., f32::max);
        assert!(
            peak > 1.05 && peak < 1.2,
            "passes the target a little: {peak}"
        );
        assert!((spring(0.9) - 1.).abs() < 0.02, "settled near the end");
    }

    #[test]
    fn easings_run_from_zero_to_one() {
        for ease in [ease_out, ease_out_soft, ease_in] {
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
        // A drawer is still ahead of linear, but leaves more of its slide to be seen.
        assert!(ease_out_soft(0.2) > 0.2 && ease_out_soft(0.2) < ease_out(0.2));
    }

    #[test]
    fn exits_are_shorter_than_entrances() {
        assert!(EXIT < BASE);
        assert!(FAST <= EXIT);
    }

    /// Menus open faster than any other surface, and only fade: no offset, so they appear exactly
    /// where they are drawn. Reduced motion draws them as they are.
    #[test]
    fn menus_fade_in_place_quickly() {
        assert!(MENU < EXIT && MENU <= Duration::from_millis(100));
        force_for_test(true);
        let mut el = fade_in(div(), 0.25);
        let style = el.style();
        assert!(style.opacity.is_some_and(|o| (o - 0.25).abs() < 1e-6));
        assert!(style.inset.left.is_none() && style.inset.top.is_none());
        force_for_test(false);
        assert!(fade_in(div(), 0.25).style().opacity.is_none());
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
        };
        let mid = slide.left(start + BASE / 4);
        assert!(mid > 0. && mid < 30., "partway: {mid}");
        let grid = Slide {
            from: (10., -20.),
            started: start,
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
        assert!(off.progress_at(Instant::now()).is_none());
    }

    /// A cue styled in place runs from 0 to 1 over `BASE`, then stops asking for frames.
    #[test]
    fn a_cue_in_place_progresses_over_base() {
        force_for_test(true);
        let mut cue = Cue::default();
        let now = Instant::now();
        assert!(cue.progress_at(now).is_none(), "nothing fired yet");
        cue.fire();
        let at = cue.at.expect("fired");
        assert!(cue.progress_at(at).is_some_and(|t| t.abs() < 1e-6));
        assert!(
            cue.progress_at(at + BASE / 2)
                .is_some_and(|t| (t - 0.5).abs() < 1e-3)
        );
        assert!(cue.progress_at(at + BASE).is_none(), "over");
        force_for_test(false);
    }
}
