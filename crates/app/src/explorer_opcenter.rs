//! Operation center in the top bar: a progress ring button that shows while file operations run or
//! just finished, opening a list. State lives in `op_center.rs` (global, shared by all tabs); this
//! only draws.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, SharedString, Transformation,
    Window, anchored, deferred, div, point, prelude::*, px, radians, svg,
};
use rust_i18n::t;

use super::Explorer;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::op_center::{JobState, OpCenter, TitleParts};
use crate::shell::OpProgress;
use crate::theme;
use crate::workspace::TooltipBelowExt as _;

/// As wide as the toasts, so a running job's status line fits beside its Cancel button.
fn panel_w() -> gpui::Pixels {
    crate::theme::scaled_px(360.)
}
fn panel_max_h() -> gpui::Pixels {
    crate::theme::scaled_px(440.)
}
/// How much more readily a job title's name shrinks than the words after it (flex shrink
/// factors): so much that they don't give up even a fraction of a pixel (which would cut their
/// last letter) while the name can still shrink.
const NAME_GIVES_WAY: f32 = 1_000_000.;
/// A job title's name longer than this many characters can shrink to a few letters and "…";
/// a shorter one is about that wide already and keeps its width.
const NAME_KEEPS_CHARS: usize = 6;
/// Length of the spinning arc while the total is unknown (in turns)
const SPINNER_SWEEP: f32 = 0.25;
/// Time for one full turn (ms)
const SPINNER_PERIOD_MS: u128 = 1200;
/// Time between ring frames while it moves: about 30 fps. Each frame redraws the whole window, so
/// the ring doesn't follow the display's refresh rate.
const RING_FRAME: Duration = Duration::from_millis(33);
/// Share of the gap to the real progress the ring covers each frame
const RING_EASE: f32 = 0.35;

/// Progress ring state of one Explorer: where the ring is drawn now, and whether its next frame is
/// already scheduled.
#[derive(Default)]
pub(crate) struct RingState {
    shown: Cell<f32>,
    frame_pending: Cell<bool>,
}

impl RingState {
    /// Where to draw the ring for `fraction` (`None`: total unknown, spinning): start and length
    /// in turns, and whether it is still moving (so it needs another frame).
    fn step(&self, fraction: Option<f32>, motion: bool, now_ms: u128) -> (f32, f32, bool) {
        if let Some(fraction) = fraction {
            // Draw a small arc even at the start so it visibly moves.
            let target = fraction.clamp(0.04, 1.);
            let current = self.shown.get();
            // A new job (progress dropped) jumps straight there; otherwise ease toward it.
            let next = if current <= 0. || target < current || !motion {
                target
            } else {
                current + (target - current) * RING_EASE
            };
            let moving = (target - next).abs() > 0.002;
            self.shown.set(if moving { next } else { target });
            (0., self.shown.get(), moving)
        } else {
            self.shown.set(0.);
            let turn = (now_ms % SPINNER_PERIOD_MS) as f32 / SPINNER_PERIOD_MS as f32;
            (turn, SPINNER_SWEEP, motion)
        }
    }

    /// Redraws the Explorer after [`RING_FRAME`], once however often it is asked.
    fn schedule_frame(self: &Rc<Self>, cx: &mut Context<Explorer>) {
        if self.frame_pending.replace(true) {
            return;
        }
        let ring = Rc::clone(self);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RING_FRAME).await;
            ring.frame_pending.set(false);
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }
}

/// One list row, copied out of the global state so events can be attached while drawing.
struct Row {
    id: u64,
    icon: Icon,
    title: TitleParts,
    detail: String,
    state: JobState,
    fraction: Option<f32>,
    stopping: bool,
    progress: Arc<OpProgress>,
    /// Undo history id, while the operation can still be undone.
    undo: Option<u64>,
    /// Redo stack id, for a finished undo that can still be redone.
    redo: Option<u64>,
    /// A failed job that can be tried again on the failed items.
    retry: bool,
    /// Waiting in the queue.
    queued: bool,
}

impl Explorer {
    /// The ring's start and length for this frame (`overall`: progress, `None` while unknown),
    /// scheduling the next frame while it moves.
    fn ring_frame(&self, overall: Option<f32>, cx: &mut Context<Self>) -> (f32, f32) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let (start, sweep, moving) = self.ring.step(overall, crate::motion::enabled(), now_ms);
        if moving {
            self.ring.schedule_frame(cx);
        }
        (start, sweep)
    }

    /// Opens or closes the list from its button; closing it there fades it out.
    fn toggle_ops(&mut self, cx: &mut Context<Self>) {
        if !self.close_ops(cx) {
            self.ops_open = true;
            self.closing_ops = None;
            OpCenter::mark_seen(cx);
            cx.notify();
        }
    }

    /// Dismisses the list (its button or a click outside): it fades out. Returns whether it was
    /// open.
    fn close_ops(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.ops_open) {
            return false;
        }
        crate::motion::Leaving::start(self, (), |this| &mut this.closing_ops, cx);
        cx.notify();
        true
    }

    /// Top bar button plus its open list; takes no space when there are no jobs.
    pub(super) fn render_op_center(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let jobs = OpCenter::jobs(cx);
        if jobs.is_empty() {
            return None;
        }
        // Newest first
        let rows: Vec<Row> = jobs
            .iter()
            .rev()
            .map(|job| Row {
                id: job.id,
                icon: crate::op_center::op_icon(job.verb),
                title: job.title_parts(),
                detail: job.detail(),
                state: job.state.clone(),
                fraction: job.fraction(),
                stopping: job.progress.cancel.load(Ordering::Relaxed),
                progress: Arc::clone(&job.progress),
                undo: job
                    .undo
                    .filter(|id| crate::explorer::UndoHistory::has(cx, *id)),
                redo: job
                    .redo
                    .filter(|id| crate::explorer::RedoStack::has(cx, *id)),
                retry: job.retry.is_some(),
                queued: job.queued(),
            })
            .collect();
        let running: Vec<&Row> = rows
            .iter()
            .filter(|row| row.state == JobState::Running)
            .collect();
        // Several running: ring shows the average; any unknown total makes it spin.
        let overall = running
            .iter()
            .map(|row| row.fraction)
            .sum::<Option<f32>>()
            .map(|sum| sum / running.len().max(1) as f32);
        let unseen = OpCenter::unseen(cx);
        let open = self.ops_open;
        let tip = if running.is_empty() {
            t!("ops.center_title").into_owned()
        } else {
            t!("ops.center_running", count = running.len()).into_owned()
        };
        let busy = !running.is_empty();
        // Speed next to the ring (first running job with a known speed)
        let speed = jobs
            .iter()
            .rev()
            .filter(|job| job.running())
            .find_map(crate::op_center::Job::speed_text);
        let ring = busy.then(|| self.ring_frame(overall, cx));
        let button = button(ring, unseen, open, tip, speed, cx);
        let viewport = window.viewport_size();
        // A short window: the list ends above the window's edge (and scrolls) rather than being
        // pushed up over its own button. It opens under the button, which is centered in the
        // title row.
        let top = (theme::h_bar() + theme::h_ctl()) / 2. + theme::s_4();
        let max_h = panel_max_h()
            .min(viewport.height - top - theme::s_8())
            .max(theme::h_ctl() * 2.);
        if !open {
            let Some(ghost) = &self.closing_ops else {
                return Some(button.into_any_element());
            };
            let panel = crate::motion::leave(
                panel(rows, max_h, cx),
                ghost.id("op-center-leave"),
                0.,
                -crate::motion::NUDGE,
            );
            return Some(Self::op_center_with(button, None, panel));
        }
        let scrim = deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("op-center-scrim")
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.close_ops(cx);
                        }),
                    ),
            ),
        )
        .with_priority(1);
        let panel = crate::motion::appear(
            panel(rows, max_h, cx),
            "op-center-appear",
            0.,
            -crate::motion::NUDGE,
        );
        Some(Self::op_center_with(button, Some(scrim), panel))
    }

    /// The button with its list (open, or fading out) under it, right-aligned and kept inside the
    /// window; `scrim` closes it on a click elsewhere.
    fn op_center_with(
        button: gpui::Stateful<gpui::Div>,
        scrim: Option<gpui::Deferred>,
        panel: AnyElement,
    ) -> AnyElement {
        let flyout = deferred(
            anchored()
                .anchor(gpui::Anchor::TopRight)
                .snap_to_window_with_margin(theme::s_8())
                .child(panel),
        )
        .with_priority(2);
        div()
            .relative()
            .flex_none()
            .child(button)
            .children(scrim)
            .child(
                div()
                    .absolute()
                    .top(theme::h_ctl() + theme::s_4())
                    .right_0()
                    .child(flyout),
            )
            .into_any_element()
    }
}

/// Top bar button: a ring while running (`ring`: its start and length in turns), a check when
/// done, and a dot if something finished while the list was closed.
fn button(
    ring: Option<(f32, f32)>,
    unseen: Option<bool>,
    open: bool,
    tip: String,
    speed: Option<String>,
    cx: &mut Context<Explorer>,
) -> gpui::Stateful<gpui::Div> {
    let busy = ring.is_some();
    div()
        .id("op-center")
        .debug_selector(|| "op-center".to_owned())
        .a11y_button(tip.clone())
        .a11y_open_close(open, cx.entity().downgrade(), |this, _, cx| {
            this.toggle_ops(cx);
        })
        .relative()
        .h(theme::h_ctl())
        .min_w(theme::h_ctl())
        .px(if speed.is_some() { theme::s_8() } else { px(0.) })
        .gap(theme::s_6())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .when(open, |d| d.bg(theme::wash_strong()))
        .hover(|s| s.bg(theme::wash_strong()))
        .children(speed.filter(|_| busy).map(|speed| {
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(speed)
        }))
        .child(if let Some((start, sweep)) = ring {
            self::ring(start, sweep).into_any_element()
        } else {
            svg()
                .path(Icon::History.path())
                .size(theme::icon())
                .text_color(theme::text_muted())
                .into_any_element()
        })
        // Finished while the list was closed: dot in the top-right (red if anything failed)
        .children(unseen.filter(|_| !open).map(|failed| {
            div()
                .absolute()
                .top(theme::s_4())
                .right(theme::s_4())
                .size(theme::s_6())
                .rounded(theme::R_FULL)
                .bg(if failed {
                    theme::danger()
                } else {
                    theme::signal()
                })
        }))
        // No tooltip while its list is open: it would sit on top of it.
        .when(!open, |d| d.tooltip_below(tip))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.toggle_ops(cx);
        }))
}

/// The open list: the header (title, "Pause queue", "Clear finished"), a hairline, then one row
/// per job (newest first) with a hairline between rows. The header and the rows share one inset,
/// so their left and right edges line up.
fn panel(
    rows: Vec<Row>,
    max_h: gpui::Pixels,
    cx: &mut Context<Explorer>,
) -> gpui::Stateful<gpui::Div> {
    let any_finished = rows.iter().any(|row| row.state != JobState::Running);
    let any_running = rows.iter().any(|row| row.state == JobState::Running);
    let paused = crate::op_queue::paused();
    let mut list: Vec<AnyElement> = Vec::with_capacity(rows.len() * 2);
    for (ix, row) in rows.into_iter().enumerate() {
        if ix > 0 {
            list.push(rule(format!("op-center-rule-{ix}")).into_any_element());
        }
        list.push(job_row(ix, row, cx));
    }
    div()
        .id("op-center-panel")
        .debug_selector(|| "op-center-panel".to_owned())
        .a11y(gpui::Role::List, t!("ops.center_title"))
        .occlude()
        .w(panel_w())
        .max_h(max_h)
        .overflow_y_scroll()
        .p(theme::s_8())
        .flex()
        .flex_col()
        .rounded(theme::r_12())
        .bg(theme::overlay())
        .shadow(theme::elev_3())
        .text_size(theme::fs_14())
        .text_color(theme::text())
        .child(header(any_running || paused, paused, any_finished, cx))
        .child(rule("op-center-header-rule".to_owned()).my(theme::s_4()))
        .children(list)
}

/// The title with the queue's buttons. They are always both there, greyed out while they have
/// nothing to act on, so the header reads the same whatever the list holds. When a language's
/// labels don't fit beside the title, the buttons move to a line of their own below it, kept at
/// the right edge.
fn header(
    can_pause: bool,
    paused: bool,
    any_finished: bool,
    cx: &mut Context<Explorer>,
) -> impl IntoElement {
    div()
        .debug_selector(|| "op-center-header".to_owned())
        .flex_none()
        .min_h(theme::h_ctl())
        .px(theme::s_8())
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(theme::s_8())
        .gap_y(theme::s_4())
        .child(
            div()
                .flex_auto()
                .min_w_0()
                .truncate()
                .font_weight(theme::FW_STRONG)
                .child(t!("ops.center_title").into_owned()),
        )
        .child(
            div()
                .debug_selector(|| "op-center-header-actions".to_owned())
                .ml_auto()
                .flex()
                .flex_wrap()
                .justify_end()
                .items_center()
                .gap(theme::s_4())
                .child(pause_button(can_pause, paused, cx))
                .child(clear_button(any_finished, cx)),
        )
}

/// The hairline under the header and between two jobs, inset like the rows' content.
fn rule(selector: String) -> gpui::Div {
    div()
        .debug_selector(move || selector)
        .flex_none()
        .mx(theme::s_8())
        .h(px(1.))
        .bg(theme::line())
}

/// One job (`ix`: its place in the list): the icon, then the title, status line and progress
/// bar, then its button (Cancel, Retry, Undo or Redo) or a check. The icon and the button are
/// centered on the text between them, so they sit with the job rather than on its first line.
fn job_row(ix: usize, row: Row, cx: &mut Context<Explorer>) -> AnyElement {
    let running = row.state == JobState::Running;
    let failed = matches!(row.state, JobState::Failed(_));
    let trailing = match &row.state {
        JobState::Running if !row.stopping => Some(cancel_button(ix, &row, cx)),
        // Failed on some items: Retry those (what was done, if anything, stays undoable via
        // Ctrl+Z).
        JobState::Failed(_) if row.retry => Some(retry_button(ix, row.id, cx)),
        // Finished and still undoable: Undo, the same as Ctrl+Z but for this operation.
        _ if row.undo.is_some() => row.undo.map(|undo| undo_button(ix, row.id, undo, cx)),
        // A finished undo that can still be redone: Redo, the same as Ctrl+Y.
        _ if row.redo.is_some() => row.redo.map(|redo| redo_button(ix, row.id, redo, cx)),
        JobState::Done { .. } => Some(
            svg()
                .path(Icon::Check.path())
                .size(theme::icon())
                .flex_none()
                .text_color(theme::text_subtle())
                .into_any_element(),
        ),
        _ => None,
    };
    let tint = if failed {
        theme::danger()
    } else {
        theme::text_muted()
    };
    div()
        .id(("op-center-job", usize::try_from(row.id).unwrap_or(0)))
        .debug_selector(move || format!("op-center-row-{ix}"))
        .a11y_lazy(gpui::Role::ListItem, || {
            crate::a11y::item_label(&row.title.text(), &[&row.detail])
        })
        .flex_none()
        .px(theme::s_8())
        .py(theme::s_8())
        .flex()
        .items_center()
        .gap(theme::s_8())
        .rounded(theme::r_8())
        .hover(|s| s.bg(theme::wash()))
        .child(
            svg()
                .debug_selector(move || format!("op-center-row-{ix}-icon"))
                .path(row.icon.path())
                .size(theme::icon())
                .flex_none()
                .text_color(tint),
        )
        .child(
            div()
                .debug_selector(move || format!("op-center-row-{ix}-text"))
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::s_4())
                .child(title_line(ix, row.title))
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .line_clamp(3)
                        .text_color(tint)
                        .child(row.detail),
                )
                .when(running, |d| {
                    d.child(
                        div()
                            .id("op-center-progress")
                            .a11y_progress(row.fraction)
                            .h(theme::s_4())
                            .w_full()
                            .rounded(theme::R_FULL)
                            .bg(theme::line())
                            .overflow_hidden()
                            // Queued: an empty track, nothing is happening yet.
                            .when(!row.queued, |d| {
                                d.child(
                                    div()
                                        .h_full()
                                        .rounded(theme::R_FULL)
                                        .bg(theme::signal())
                                        .w(gpui::relative(row.fraction.unwrap_or(0.3))),
                                )
                            }),
                    )
                }),
        )
        .children(trailing)
        .into_any_element()
}

/// A job's title on one line. When it doesn't fit, a long name gives way to "…" first, down to
/// `theme::s_32()` (a few letters); then the words between the name and the count do (a long
/// "and others to Recycle Bin"). The operation before the name, and the count with what follows
/// it, never shrink, so "(4 items)" always shows.
fn title_line(ix: usize, title: TitleParts) -> impl IntoElement {
    let TitleParts {
        before,
        name,
        between,
        tail,
    } = title;
    // A short name keeps its width: a minimum wider than the name would leave a gap after it.
    let long_name = name.chars().count() > NAME_KEEPS_CHARS;
    // No subject yet: the operation is the whole title and is cut short like one.
    let only_op = name.is_empty() && between.is_empty() && tail.is_empty();
    let piece = |text: String| (!text.is_empty()).then(|| div().child(text));
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .flex()
        .children(piece(before).map(|d| {
            if only_op {
                d.min_w_0().truncate()
            } else {
                d.flex_none()
            }
        }))
        .children(piece(name).map(|d| {
            let d = d.debug_selector(move || format!("op-center-row-{ix}-name"));
            if long_name {
                d.flex_shrink(NAME_GIVES_WAY)
                    .min_w(theme::s_32())
                    .truncate()
            } else {
                d.flex_none()
            }
        }))
        .children(piece(between).map(|d| d.min_w_0().truncate()))
        .children(piece(tail).map(|d| {
            d.debug_selector(move || format!("op-center-row-{ix}-count"))
                .flex_none()
        }))
}

/// A quiet text button (Cancel, Undo, Pause queue): the same height and padding in the header
/// and the rows. Greyed out and inert unless `enabled`.
fn text_button(
    button: gpui::Stateful<gpui::Div>,
    label: String,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    button
        .flex_none()
        .h(theme::h_ctl_sm())
        .px(theme::s_6())
        .flex()
        .items_center()
        .rounded(theme::r_4())
        .whitespace_nowrap()
        .text_size(theme::fs_12())
        .text_color(if enabled {
            theme::text_muted()
        } else {
            theme::text_disabled()
        })
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                .on_click(on_click)
        })
        .child(label)
}

/// "Clear finished": removes the finished jobs from the list.
fn clear_button(enabled: bool, cx: &mut Context<Explorer>) -> AnyElement {
    text_button(
        div()
            .id("op-center-clear")
            .debug_selector(|| "op-center-clear".to_owned())
            .a11y_button(t!("ops.center_clear"))
            .a11y_disabled(!enabled),
        t!("ops.center_clear").into_owned(),
        enabled,
        cx.listener(|this, _: &ClickEvent, _, cx| {
            OpCenter::clear_finished(cx);
            // Nothing left in the list once everything is cleared, so close it.
            if OpCenter::jobs(cx).is_empty() {
                this.ops_open = false;
            }
            cx.notify();
        }),
    )
    .into_any_element()
}

/// "Cancel" on a running (or queued) row.
fn cancel_button(ix: usize, row: &Row, cx: &mut Context<Explorer>) -> AnyElement {
    let progress = Arc::clone(&row.progress);
    text_button(
        div()
            .id(("op-center-cancel", usize::try_from(row.id).unwrap_or(0)))
            .debug_selector(move || format!("op-center-action-{ix}"))
            .a11y_button(t!("toast.cancel")),
        t!("toast.cancel").into_owned(),
        true,
        cx.listener(move |_, _: &ClickEvent, _, cx| {
            progress.cancel.store(true, Ordering::Relaxed);
            // A queued job leaves the line right away.
            crate::op_queue::wake();
            cx.notify();
        }),
    )
    .into_any_element()
}

/// "Retry" on a failed row: runs the operation again on the items that failed.
fn retry_button(ix: usize, row: u64, cx: &mut Context<Explorer>) -> AnyElement {
    text_button(
        div()
            .id(("op-center-retry", usize::try_from(row).unwrap_or(0)))
            .debug_selector(move || format!("op-center-action-{ix}"))
            .a11y_button(t!("toast.retry")),
        t!("toast.retry").into_owned(),
        true,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(op) = OpCenter::take_retry(cx, row) {
                this.retry_op(op, cx);
            }
        }),
    )
    .into_any_element()
}

/// "Undo" on a finished row: undoes that operation like Ctrl+Z would, even if newer ones exist.
fn undo_button(ix: usize, row: u64, undo: u64, cx: &mut Context<Explorer>) -> AnyElement {
    text_button(
        div()
            .id(("op-center-undo", usize::try_from(row).unwrap_or(0)))
            .debug_selector(move || format!("op-center-action-{ix}"))
            .a11y_button(t!("toast.undo")),
        t!("toast.undo").into_owned(),
        true,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.undo_op(undo, cx);
        }),
    )
    .into_any_element()
}

/// "Redo" on a finished undo: redoes that operation like Ctrl+Y would.
fn redo_button(ix: usize, row: u64, redo: u64, cx: &mut Context<Explorer>) -> AnyElement {
    text_button(
        div()
            .id(("op-center-redo", usize::try_from(row).unwrap_or(0)))
            .debug_selector(move || format!("op-center-action-{ix}"))
            .a11y_button(t!("toast.redo")),
        t!("toast.redo").into_owned(),
        true,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.redo_op(redo, cx);
        }),
    )
    .into_any_element()
}

/// "Pause queue" / "Resume": holds jobs that haven't started. Pause only matters while something
/// is running or waiting (`enabled`), or to resume. A running job can't be paused halfway
/// (Windows' copy engine), so the tooltip says that it finishes first.
fn pause_button(enabled: bool, paused: bool, cx: &mut Context<Explorer>) -> AnyElement {
    let label = if paused {
        t!("ops.queue_resume")
    } else {
        t!("ops.queue_pause")
    };
    text_button(
        div()
            .id("op-center-pause")
            .debug_selector(|| "op-center-pause".to_owned())
            .a11y_toggle(t!("ops.queue_pause"), paused)
            .aria_description(t!("ops.queue_pause_tip").into_owned())
            .a11y_disabled(!enabled),
        label.into_owned(),
        enabled,
        cx.listener(move |_, _: &ClickEvent, _, cx| {
            crate::op_queue::set_paused(!paused);
            cx.notify();
        }),
    )
    .when(paused, |d| d.text_color(theme::text()))
    .tooltip_below(t!("ops.queue_pause_tip").into_owned())
    .into_any_element()
}

/// Ring: a thin track with the done portion on top, or a short spinning arc while the total is
/// unknown. Both are SVG layers (see `assets::ring_arc`), drawn as sprites: vector paths would
/// need a window-sized multisampled texture each frame.
fn ring(start: f32, sweep: f32) -> impl IntoElement {
    let layer = |path: SharedString| {
        svg()
            .absolute()
            .top_0()
            .left_0()
            .size(theme::icon())
            .path(path)
    };
    div()
        .relative()
        .flex_none()
        .size(theme::icon())
        .child(layer(crate::assets::RING_TRACK.into()).text_color(theme::line_strong()))
        .child(
            layer(crate::assets::ring_arc(sweep).into())
                .text_color(theme::signal())
                .with_transformation(Transformation::rotate(radians(
                    start * std::f32::consts::TAU,
                ))),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_eases_toward_progress_and_then_rests() {
        let ring = RingState::default();
        // A new job starts where its progress is.
        assert_eq!(ring.step(Some(0.5), true, 0), (0., 0.5, false));
        // Progress moves on: the ring follows over a few frames, then stops asking for them.
        let (_, first, moving) = ring.step(Some(0.9), true, 0);
        assert!(moving && first > 0.5 && first < 0.9, "{first}");
        let mut frames = 1;
        while ring.step(Some(0.9), true, 0).2 {
            frames += 1;
            assert!(frames < 30, "the ring never settles");
        }
        assert_eq!(ring.step(Some(0.9), true, 0), (0., 0.9, false));
    }

    #[test]
    fn without_motion_the_ring_jumps_and_never_animates() {
        let ring = RingState::default();
        ring.step(Some(0.2), false, 0);
        assert_eq!(ring.step(Some(0.7), false, 0), (0., 0.7, false));
        assert!(!ring.step(None, false, 300).2, "the spinner stands still");
    }

    #[test]
    fn the_spinner_turns_with_time() {
        let ring = RingState::default();
        let (at_0, sweep, moving) = ring.step(None, true, 0);
        assert!(moving);
        assert_eq!(sweep, SPINNER_SWEEP);
        let (quarter, ..) = ring.step(None, true, SPINNER_PERIOD_MS / 4);
        assert!((quarter - at_0 - 0.25).abs() < 1e-6, "{quarter}");
    }
}
