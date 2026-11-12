//! Operation center in the top bar: a progress ring button that shows while file operations run or
//! just finished, opening a list. State lives in `op_center.rs` (global, shared by all tabs); this
//! only draws.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use gpui::{
    AnimationExt, AnyElement, Bounds, ClickEvent, Context, Hsla, MouseButton, MouseDownEvent,
    PathBuilder, Pixels, Point, Window, anchored, canvas, deferred, div, point, prelude::*, px,
    svg,
};
use rust_i18n::t;

use super::Explorer;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::op_center::{JobState, OpCenter};
use crate::shell::OpProgress;
use crate::theme;
use crate::workspace::TooltipBelowExt as _;

const PANEL_W: Pixels = px(380.);
const PANEL_MAX_H: Pixels = px(440.);
/// Length of the spinning arc while the total is unknown (in turns)
const SPINNER_SWEEP: f32 = 0.25;
/// Time for one full turn (ms)
const SPINNER_PERIOD_MS: u128 = 1200;

/// One list row, copied out of the global state so events can be attached while drawing.
struct Row {
    id: u64,
    icon: Icon,
    title: String,
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
                icon: match job.verb {
                    "op.recycle" | "op.delete_permanently" => Icon::Trash,
                    "op.move" => Icon::Cut,
                    _ => Icon::Copy,
                },
                title: job.title(),
                detail: job.detail(),
                state: job.state.clone(),
                fraction: job.fraction(),
                stopping: job.progress.cancel.load(Ordering::Relaxed),
                progress: job.progress.clone(),
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
        #[allow(clippy::cast_precision_loss)]
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
        let ring_shown = self.ring_shown.clone();
        let button = button(busy, overall, unseen, open, tip, speed, ring_shown, cx);
        if !open {
            return Some(button.into_any_element());
        }
        let viewport = window.viewport_size();
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
                            this.ops_open = false;
                            cx.notify();
                        }),
                    ),
            ),
        )
        .with_priority(1);
        let panel = panel(rows, cx);
        let flyout = deferred(
            anchored()
                .anchor(gpui::Anchor::TopRight)
                .snap_to_window_with_margin(theme::S_8)
                .child(panel),
        )
        .with_priority(2);
        Some(
            div()
                .relative()
                .flex_none()
                .child(button)
                .child(scrim)
                .child(
                    div()
                        .absolute()
                        .top(theme::H_CTL + theme::S_4)
                        .right_0()
                        .child(flyout),
                )
                .into_any_element(),
        )
    }
}

/// Top bar button: a ring while running, a check when done, and a dot if something finished while
/// the list was closed.
#[allow(clippy::too_many_arguments)]
fn button(
    busy: bool,
    overall: Option<f32>,
    unseen: Option<bool>,
    open: bool,
    tip: String,
    speed: Option<String>,
    ring_shown: std::rc::Rc<std::cell::Cell<f32>>,
    cx: &mut Context<Explorer>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("op-center")
        .a11y_button(tip.clone())
        .aria_expanded(open)
        .relative()
        .h(theme::H_CTL)
        .min_w(theme::H_CTL)
        .px(if speed.is_some() { theme::S_8 } else { px(0.) })
        .gap(theme::S_6)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(open, |d| d.bg(theme::wash_strong()))
        .hover(|s| s.bg(theme::wash_strong()))
        .children(speed.filter(|_| busy).map(|speed| {
            div()
                .text_size(theme::FS_12)
                .text_color(theme::text_muted())
                .child(speed)
        }))
        .child(if busy {
            ring(overall, ring_shown).into_any_element()
        } else {
            svg()
                .path(Icon::Check.path())
                .size(theme::ICON)
                .text_color(theme::text_muted())
                .into_any_element()
        })
        // Finished while the list was closed: dot in the top-right (red if anything failed)
        .children(unseen.filter(|_| !open).map(|failed| {
            div()
                .absolute()
                .top(theme::S_4)
                .right(theme::S_4)
                .size(theme::S_6)
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
            this.ops_open = !this.ops_open;
            if this.ops_open {
                OpCenter::mark_seen(cx);
            }
            cx.notify();
        }))
}

/// The open list: title, "Clear completed", then one row per job (newest first).
fn panel(rows: Vec<Row>, cx: &mut Context<Explorer>) -> impl IntoElement + use<> {
    let any_finished = rows.iter().any(|row| row.state != JobState::Running);
    // Pause only matters while something is running or waiting. A running job can't be paused
    // halfway (Windows' copy engine), so the button says it takes effect after the current job.
    let any_running = rows.iter().any(|row| row.state == JobState::Running);
    let paused = crate::op_queue::paused();
    div()
        .id("op-center-panel")
        .a11y(gpui::Role::List, t!("ops.center_title"))
        .occlude()
        .w(PANEL_W)
        .max_h(PANEL_MAX_H)
        .overflow_y_scroll()
        .p(theme::S_8)
        .flex()
        .flex_col()
        .gap(theme::S_2)
        .rounded(theme::R_12)
        .bg(theme::overlay())
        .shadow(theme::elev_3())
        .text_size(theme::FS_14)
        .text_color(theme::text())
        .child(
            div()
                .flex()
                .items_center()
                .px(theme::S_8)
                .h(theme::H_CTL)
                .child(
                    div()
                        .flex_1()
                        .font_weight(theme::FW_STRONG)
                        .child(t!("ops.center_title").into_owned()),
                )
                .when(any_running || paused, |d| d.child(pause_button(paused, cx)))
                .when(any_finished, |d| {
                    d.child(
                        div()
                            .id("op-center-clear")
                            .a11y_button(t!("ops.center_clear"))
                            .px(theme::S_6)
                            .rounded(theme::R_4)
                            .cursor_pointer()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_muted())
                            .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                            .child(t!("ops.center_clear").into_owned())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                OpCenter::clear_finished(cx);
                                // The button disappears once everything is cleared, so close the
                                // list too.
                                if OpCenter::jobs(cx).is_empty() {
                                    this.ops_open = false;
                                }
                                cx.notify();
                            })),
                    )
                }),
        )
        .children(rows.into_iter().map(|row| job_row(row, cx)))
        .with_animation("op-center-appear", crate::motion::enter(), |el, t| {
            crate::motion::slide_in(el, t, 0., -crate::motion::NUDGE)
        })
}

/// One job: title, cancel or result on the right, a status line, and a progress bar while running.
fn job_row(row: Row, cx: &mut Context<Explorer>) -> AnyElement {
    let running = row.state == JobState::Running;
    let failed = matches!(row.state, JobState::Failed(_));
    let trailing = match &row.state {
        JobState::Running if !row.stopping => Some(cancel_button(&row, cx)),
        // Failed on some items: Retry those (what was done, if anything, stays undoable via
        // Ctrl+Z).
        JobState::Failed(_) if row.retry => Some(retry_button(row.id, cx)),
        // Finished and still undoable: Undo, the same as Ctrl+Z but for this operation.
        _ if row.undo.is_some() => row.undo.map(|undo| undo_button(row.id, undo, cx)),
        // A finished undo that can still be redone: Redo, the same as Ctrl+Y.
        _ if row.redo.is_some() => row.redo.map(|redo| redo_button(row.id, redo, cx)),
        JobState::Done { .. } => Some(
            svg()
                .path(Icon::Check.path())
                .size(theme::ICON)
                .flex_none()
                .text_color(theme::text_subtle())
                .into_any_element(),
        ),
        _ => None,
    };
    div()
        .id(("op-center-job", usize::try_from(row.id).unwrap_or(0)))
        .a11y_lazy(gpui::Role::ListItem, || {
            crate::a11y::item_label(&row.title, &[&row.detail])
        })
        .px(theme::S_8)
        .py(theme::S_8)
        .flex()
        .flex_col()
        .gap(theme::S_4)
        .rounded(theme::R_8)
        .hover(|s| s.bg(theme::wash()))
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme::S_8)
                .child(
                    svg()
                        .path(row.icon.path())
                        .size(theme::ICON)
                        .flex_none()
                        .text_color(if failed {
                            theme::danger()
                        } else {
                            theme::text_muted()
                        }),
                )
                .child(div().flex_1().min_w_0().truncate().child(row.title))
                .children(trailing),
        )
        .child(
            div()
                .pl(theme::ICON + theme::S_8)
                .text_size(theme::FS_12)
                .line_clamp(3)
                .text_color(if failed {
                    theme::danger()
                } else {
                    theme::text_muted()
                })
                .child(row.detail),
        )
        .when(running, |d| {
            d.child(
                div().pl(theme::ICON + theme::S_8).pt(theme::S_2).child(
                    div()
                        .id("op-center-progress")
                        .a11y_progress(row.fraction)
                        .h(theme::S_4)
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
                ),
            )
        })
        .into_any_element()
}

/// "Cancel" on a running (or queued) row.
fn cancel_button(row: &Row, cx: &mut Context<Explorer>) -> AnyElement {
    let progress = row.progress.clone();
    div()
        .id(("op-center-cancel", usize::try_from(row.id).unwrap_or(0)))
        .a11y_button(t!("toast.cancel"))
        .flex_none()
        .px(theme::S_6)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child(t!("toast.cancel").into_owned())
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            progress.cancel.store(true, Ordering::Relaxed);
            // A queued job leaves the line right away.
            crate::op_queue::wake();
            cx.notify();
        }))
        .into_any_element()
}

/// "Retry" on a failed row: runs the operation again on the items that failed.
fn retry_button(row: u64, cx: &mut Context<Explorer>) -> AnyElement {
    div()
        .id(("op-center-retry", usize::try_from(row).unwrap_or(0)))
        .a11y_button(t!("toast.retry"))
        .flex_none()
        .px(theme::S_6)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child(t!("toast.retry").into_owned())
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(op) = OpCenter::take_retry(cx, row) {
                this.retry_op(op, cx);
            }
        }))
        .into_any_element()
}

/// "Undo" on a finished row: undoes that operation like Ctrl+Z would, even if newer ones exist.
fn undo_button(row: u64, undo: u64, cx: &mut Context<Explorer>) -> AnyElement {
    div()
        .id(("op-center-undo", usize::try_from(row).unwrap_or(0)))
        .a11y_button(t!("toast.undo"))
        .flex_none()
        .px(theme::S_6)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child(t!("toast.undo").into_owned())
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.undo_op(undo, cx);
        }))
        .into_any_element()
}

/// "Redo" on a finished undo: redoes that operation like Ctrl+Y would.
fn redo_button(row: u64, redo: u64, cx: &mut Context<Explorer>) -> AnyElement {
    div()
        .id(("op-center-redo", usize::try_from(row).unwrap_or(0)))
        .a11y_button(t!("toast.redo"))
        .flex_none()
        .px(theme::S_6)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child(t!("toast.redo").into_owned())
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.redo_op(redo, cx);
        }))
        .into_any_element()
}

/// "Pause queue" / "Resume": holds jobs that haven't started. The tooltip says that the running
/// job finishes first.
fn pause_button(paused: bool, cx: &mut Context<Explorer>) -> AnyElement {
    div()
        .id("op-center-pause")
        .a11y_toggle(t!("ops.queue_pause"), paused)
        .aria_description(t!("ops.queue_pause_tip").into_owned())
        .px(theme::S_6)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .text_color(if paused {
            theme::text()
        } else {
            theme::text_muted()
        })
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child(if paused {
            t!("ops.queue_resume").into_owned()
        } else {
            t!("ops.queue_pause").into_owned()
        })
        .tooltip_below(t!("ops.queue_pause_tip").into_owned())
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            crate::op_queue::set_paused(!paused);
            cx.notify();
        }))
        .into_any_element()
}

/// Ring: a thin track with the done portion on top, or a short spinning arc while the total is
/// unknown. Progress updates every 200 ms; the ring eases toward the target each frame so it
/// doesn't step.
fn ring(fraction: Option<f32>, shown: std::rc::Rc<std::cell::Cell<f32>>) -> impl IntoElement {
    canvas(
        |bounds, _, _| bounds,
        move |_, bounds: Bounds<Pixels>, window, _| {
            let center = bounds.center();
            let radius = bounds.size.width.min(bounds.size.height) / 2. - px(1.);
            let track: Hsla = theme::line_strong().into();
            let fill: Hsla = theme::signal().into();
            if let Some(path) = arc(center, radius, 0., 1.) {
                window.paint_path(path, track);
            }
            let (start, sweep) = if let Some(fraction) = fraction {
                // Draw a small arc even at the start so it visibly moves.
                let target = fraction.clamp(0.04, 1.);
                let current = shown.get();
                // A new job (progress dropped) jumps straight there; otherwise cover 20% of the gap
                // each frame.
                let next = if current <= 0. || target < current || !crate::motion::enabled() {
                    target
                } else {
                    current + (target - current) * 0.2
                };
                shown.set(next);
                if (target - next).abs() > 0.002 {
                    window.request_animation_frame();
                }
                (0., next)
            } else {
                shown.set(0.);
                if crate::motion::enabled() {
                    window.request_animation_frame();
                }
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                #[allow(clippy::cast_precision_loss)]
                let turn = (ms % SPINNER_PERIOD_MS) as f32 / SPINNER_PERIOD_MS as f32;
                (turn, SPINNER_SWEEP)
            };
            if let Some(path) = arc(center, radius, start, sweep) {
                window.paint_path(path, fill);
            }
        },
    )
    .size(theme::ICON)
}

/// Arc from turn `start` with length `sweep`, clockwise from 12 o'clock, built from short segments.
fn arc(
    center: Point<Pixels>,
    radius: Pixels,
    start: f32,
    sweep: f32,
) -> Option<gpui::Path<Pixels>> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = ((sweep * 48.).ceil() as usize).max(2);
    let mut builder = PathBuilder::stroke(px(2.));
    for step in 0..=steps {
        #[allow(clippy::cast_precision_loss)]
        let turn = start + sweep * step as f32 / steps as f32;
        let angle = turn * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let at = point(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        );
        if step == 0 {
            builder.move_to(at);
        } else {
            builder.line_to(at);
        }
    }
    builder.build().ok()
}
