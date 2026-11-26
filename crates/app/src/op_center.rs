//! Operation center: file operations (copy, move, delete) tracked in one place, shared by all
//! windows and tabs.
//!
//! The bottom-right toast only says "working" and "just finished"; this keeps the list. Each
//! running job has progress, speed, time left and cancel; finished jobs keep their result (count,
//! duration, failure reasons) for later. Memory only, gone when Arke quits.

use std::collections::VecDeque;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui::{App, BorrowAppContext, Context, EntityId, Global, Task};
use rust_i18n::t;

use crate::shell::{OpOutcome, OpProgress};

/// A running operation gets its bottom-right progress toast only after this long; shorter ones
/// are just in the top bar's operation center.
pub(crate) const TOAST_DELAY: Duration = Duration::from_secs(2);
/// A progress toast that is waiting for [`TOAST_DELAY`], or showing.
struct PendingToast {
    shown: bool,
    /// Reveals the toast and redraws the view; dropped with the job.
    _timer: Task<()>,
}

/// Progress toasts of running jobs, per view and kind. A job that ends before the delay never
/// shows one: short operations are only in the operation center.
#[derive(Default)]
pub(crate) struct ToastDelays(std::collections::HashMap<(EntityId, &'static str), PendingToast>);

impl Global for ToastDelays {}

impl ToastDelays {
    /// Call on every render of `cx`'s view with whether the job `kind` is running: starts the
    /// delay when it begins (the timer redraws the view at the mark, so the toast appears even if
    /// nothing else does) and forgets it when it ends.
    pub(crate) fn track<V: 'static>(cx: &mut Context<V>, kind: &'static str, running: bool) {
        let view = cx.entity_id();
        let waiting = cx
            .try_global::<Self>()
            .is_some_and(|g| g.0.contains_key(&(view, kind)));
        if running == waiting {
            return;
        }
        if !running {
            cx.global_mut::<Self>().0.remove(&(view, kind));
            return;
        }
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_DELAY).await;
            let _ = this.update(cx, |_, cx| {
                let view = cx.entity_id();
                if let Some(toast) = cx.default_global::<Self>().0.get_mut(&(view, kind)) {
                    toast.shown = true;
                }
                cx.notify();
            });
        });
        cx.default_global::<Self>().0.insert(
            (view, kind),
            PendingToast {
                shown: false,
                _timer: timer,
            },
        );
    }

    /// Whether the progress toast of `kind` is due in `view`.
    pub(crate) fn shown(cx: &App, view: EntityId, kind: &'static str) -> bool {
        cx.try_global::<Self>()
            .and_then(|g| g.0.get(&(view, kind)))
            .is_some_and(|toast| toast.shown)
    }
}

/// Finished jobs kept
const HISTORY_MAX: usize = 20;
/// Progress poll interval (redraw, speed)
const TICK: Duration = Duration::from_millis(200);
/// Speed averages over this window
const RATE_WINDOW: Duration = Duration::from_secs(4);
/// Estimate time left only past this fraction and running time (early estimates jump around)
const ETA_MIN_FRACTION: f64 = 0.03;
const ETA_MIN_ELAPSED: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JobState {
    Running,
    Done { took: Duration },
    Cancelled,
    Failed(String),
}

pub(crate) struct Job {
    pub(crate) id: u64,
    /// Operation name (locale key, `op.*`)
    pub(crate) verb: &'static str,
    /// Target: first item's name and total count (unknown for paste until the clipboard is read)
    pub(crate) subject: Option<(String, usize)>,
    pub(crate) progress: Arc<OpProgress>,
    pub(crate) started: Instant,
    pub(crate) state: JobState,
    /// Undo history id once finished, for its "Undo" button (see `explorer_ops::UndoHistory`).
    pub(crate) undo: Option<u64>,
    /// For a finished undo: redo stack id, for its "Redo" button (see `explorer_ops::RedoStack`).
    pub(crate) redo: Option<u64>,
    /// For a failed job: the operation that tries the failed items again ("Retry").
    pub(crate) retry: Option<crate::shell::FileOp>,
    /// Was waiting in the queue at the last poll; the clock restarts when it starts for real.
    was_queued: bool,
    /// Recent (time, items done, bytes done) samples for speed
    samples: VecDeque<(Instant, u64, u64)>,
}

impl Job {
    pub(crate) fn running(&self) -> bool {
        self.state == JobState::Running
    }

    /// Waiting for its turn in the queue (not started yet).
    pub(crate) fn queued(&self) -> bool {
        self.running() && self.progress.queued.load(Ordering::Relaxed)
    }

    pub(crate) fn done_items(&self) -> u64 {
        self.progress.done.load(Ordering::Relaxed)
    }

    /// Progress (0-1); `None` until Windows starts reporting. Covers partial progress within one
    /// large file.
    pub(crate) fn fraction(&self) -> Option<f32> {
        let total = self.progress.work_total.load(Ordering::Relaxed);
        let so_far = self.progress.work_done.load(Ordering::Relaxed);
        (total > 0 && so_far > 0).then(|| so_far.min(total) as f32 / total as f32)
    }

    /// Bytes done: progress times total size when the total is known (counts a half-copied large
    /// file), else the sum of finished files.
    fn bytes_so_far(&self) -> u64 {
        let total = self.progress.bytes_total.load(Ordering::Relaxed);
        match self.fraction() {
            Some(f) if total > 0 => (f64::from(f) * total as f64) as u64,
            _ => self.progress.bytes.load(Ordering::Relaxed),
        }
    }

    /// Recent items per second.
    pub(crate) fn rate(&self) -> Option<f64> {
        let (first_at, first, _) = *self.samples.front()?;
        let (last_at, last, _) = *self.samples.back()?;
        rate(last.saturating_sub(first), last_at.duration_since(first_at))
    }

    /// Recent bytes per second (copies and cross-drive moves only).
    pub(crate) fn byte_rate(&self) -> Option<f64> {
        let (first_at, _, first) = *self.samples.front()?;
        let (last_at, _, last) = *self.samples.back()?;
        rate(last.saturating_sub(first), last_at.duration_since(first_at))
    }

    /// Speed: "MB/s" when bytes are known (copy, move), like Explorer, else items per second.
    pub(crate) fn speed_text(&self) -> Option<String> {
        if !self.running()
            || self.progress.cancel.load(Ordering::Relaxed)
            || self.progress.queued.load(Ordering::Relaxed)
            || self.progress.verifying.load(Ordering::Relaxed)
        {
            return None;
        }
        if let Some(bytes) = self.byte_rate() {
            let size = arke_core::format::format_size(bytes.round() as u64);
            return Some(t!("ops.rate_bytes", size = size).into_owned());
        }
        self.rate().map(|rate| {
            let per_second = rate.round() as usize;
            tn!("ops.rate", per_second).into_owned()
        })
    }

    /// Estimated time left.
    pub(crate) fn remaining(&self) -> Option<Duration> {
        remaining(f64::from(self.fraction()?), self.started.elapsed())
    }

    /// Title: what is done to what, as one string (the list draws it from [`Self::title_parts`]).
    #[cfg(test)]
    pub(crate) fn title(&self) -> String {
        self.title_parts().text()
    }

    /// The title cut around the name, so the list can shorten the name alone (see
    /// [`TitleParts`]).
    pub(crate) fn title_parts(&self) -> TitleParts {
        let subject = self
            .subject
            .as_ref()
            .map(|(name, count)| (name.as_str(), *count));
        title_parts(self.verb, subject, &rust_i18n::locale())
    }

    /// Line below the title: progress, speed, time left, or the final result.
    pub(crate) fn detail(&self) -> String {
        let items = usize::try_from(self.done_items()).unwrap_or(usize::MAX);
        match &self.state {
            JobState::Running if self.progress.cancel.load(Ordering::Relaxed) => {
                t!("ops.state_stopping").into_owned()
            }
            // Extractions and compressions wait in a queue of their own, which doesn't pause.
            JobState::Running if self.queued() => {
                if crate::op_queue::paused() && !matches!(self.verb, "op.extract" | "op.compress") {
                    t!("ops.state_queued_paused")
                } else {
                    t!("ops.state_queued")
                }
                .into_owned()
            }
            JobState::Running if self.progress.verifying.load(Ordering::Relaxed) => {
                self.fraction().map_or_else(
                    || t!("ops.state_verifying").into_owned(),
                    |f| {
                        let percent = (f * 100.).floor() as u32;
                        t!("ops.state_verifying_percent", percent = percent).into_owned()
                    },
                )
            }
            JobState::Running if self.done_items() == 0 => {
                // One large file: no item finished yet, but there is progress.
                if let Some(f) = self.fraction() {
                    let percent = (f * 100.).floor() as u32;
                    let mut parts = vec![t!("ops.state_percent", percent = percent).into_owned()];
                    parts.extend(self.speed_text());
                    if let Some(left) = self.remaining() {
                        parts.push(
                            t!("ops.eta", time = duration_text(round_eta(left))).into_owned(),
                        );
                    }
                    parts.join(&t!("ops.separator"))
                } else {
                    // The copy engine counted the items but reports nothing until one finishes
                    // (a big folder going to the Recycle Bin is a single item): say how many. A
                    // total of bytes isn't a count (a file dragged out of a zip, before any
                    // arrived).
                    let total = self.progress.work_total.load(Ordering::Relaxed);
                    if total == 0 || self.progress.work_in_bytes.load(Ordering::Relaxed) {
                        t!("ops.state_preparing").into_owned()
                    } else {
                        tn!(
                            "ops.state_preparing_items",
                            usize::try_from(total).unwrap_or(usize::MAX)
                        )
                        .into_owned()
                    }
                }
            }
            JobState::Running => {
                let mut parts = vec![tn!("ops.state_running", items).into_owned()];
                if let Some(speed) = self.speed_text() {
                    parts.push(speed);
                }
                if let Some(left) = self.remaining() {
                    parts.push(t!("ops.eta", time = duration_text(round_eta(left))).into_owned());
                }
                parts.join(&t!("ops.separator"))
            }
            JobState::Done { took } => {
                tn!("ops.state_done", items, time = duration_text(*took)).into_owned()
            }
            JobState::Cancelled => tn!("ops.state_cancelled", items).into_owned(),
            JobState::Failed(why) => t!("ops.state_failed", error = why).into_owned(),
        }
    }
}

/// Stand-ins for the name and the count while a title's template is filled in: private-use
/// characters, which no translation contains, so the text around them can be cut out.
const NAME_MARK: &str = "\u{e000}";
const COUNT_MARK: &str = "\u{e001}";

/// A job title in the pieces the list lays out on one line: when it doesn't fit, the name gives
/// way to "…" first, so the operation and the item count stay readable ("Copy “ClassifiedArch…”
/// and others (4 items)"). The pieces follow the translation, so any word order works.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TitleParts {
    /// Up to the name ("Copy “").
    pub(crate) before: String,
    pub(crate) name: String,
    /// From the name to the count ("” and others ("), or to the end without a count.
    pub(crate) between: String,
    /// The count and what follows it ("4 items)"); empty without a count.
    pub(crate) tail: String,
}

impl TitleParts {
    /// Cuts `filled`, a title template filled in with [`NAME_MARK`] and [`COUNT_MARK`], at the
    /// marks and puts `name` and `count` in their place.
    fn split(filled: &str, name: &str, count: &str) -> Self {
        let Some((before, rest)) = filled.split_once(NAME_MARK) else {
            return Self {
                before: filled.replace(COUNT_MARK, count),
                ..Self::default()
            };
        };
        let (between, tail) = match rest.split_once(COUNT_MARK) {
            Some((between, after)) => (between.to_owned(), format!("{count}{after}")),
            None => (rest.to_owned(), String::new()),
        };
        Self {
            before: before.replace(COUNT_MARK, count),
            name: name.to_owned(),
            between: between.replace(NAME_MARK, name),
            tail: tail.replace(NAME_MARK, name),
        }
    }

    /// The whole title, as one string.
    pub(crate) fn text(&self) -> String {
        [
            self.before.as_str(),
            self.name.as_str(),
            self.between.as_str(),
            self.tail.as_str(),
        ]
        .concat()
    }
}

/// The title of an operation `verb` on `subject` (first item's name and how many items) in
/// `locale`.
fn title_parts(verb: &str, subject: Option<(&str, usize)>, locale: &str) -> TitleParts {
    let Some((name, count)) = subject else {
        return TitleParts {
            before: t!(verb, locale = locale).into_owned(),
            ..TitleParts::default()
        };
    };
    // "Move ... to Recycle Bin" wraps the name, so it doesn't fit the "<op> <name>" pattern.
    let filled = match (verb, count) {
        ("op.recycle", 1) => t!("ops.recycle_one", locale = locale, name = NAME_MARK),
        ("op.recycle", _) => t!(
            "ops.recycle_many",
            locale = locale,
            name = NAME_MARK,
            count = COUNT_MARK
        ),
        (_, 1) => t!(
            "ops.subject_one",
            locale = locale,
            op = t!(verb, locale = locale),
            name = NAME_MARK
        ),
        _ => t!(
            "ops.subject_many",
            locale = locale,
            op = t!(verb, locale = locale),
            name = NAME_MARK,
            count = COUNT_MARK
        ),
    };
    TitleParts::split(&filled, name, &arke_core::format::format_count(count))
}

/// Items per second; `None` when too short (fewer than two samples) or nothing moved.
fn rate(items: u64, over: Duration) -> Option<f64> {
    (over >= Duration::from_secs(1) && items > 0).then(|| items as f64 / over.as_secs_f64())
}

/// Time left at the current speed, given `fraction` done in `elapsed`.
fn remaining(fraction: f64, elapsed: Duration) -> Option<Duration> {
    if fraction < ETA_MIN_FRACTION || fraction >= 1. || elapsed < ETA_MIN_ELAPSED {
        return None;
    }
    Some(elapsed.mul_f64((1. - fraction) / fraction))
}

/// Round time left: to 5 seconds under a minute, to minutes beyond.
fn round_eta(left: Duration) -> Duration {
    let secs = left.as_secs();
    Duration::from_secs(if secs < 60 {
        secs.div_ceil(5).max(1) * 5
    } else {
        secs.div_ceil(60) * 60
    })
}

/// "42 s", "3 min 5 s", "2 h 10 min".
/// The icon of a file operation, by its verb: the same in the operation center and the toast at
/// the bottom (a move shows scissors, as Cut does).
pub(crate) fn op_icon(verb: &str) -> crate::assets::Icon {
    use crate::assets::Icon;
    match verb {
        "op.recycle" | "op.delete_permanently" => Icon::Trash,
        "op.move" => Icon::Cut,
        _ => Icon::Copy,
    }
}

pub(crate) fn duration_text(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        t!("ops.seconds", s = secs.max(1)).into_owned()
    } else if secs < 3600 {
        let (m, s) = (secs / 60, secs % 60);
        if s == 0 {
            t!("ops.minutes", m = m).into_owned()
        } else {
            t!("ops.minutes_seconds", m = m, s = s).into_owned()
        }
    } else {
        t!("ops.hours_minutes", h = secs / 3600, m = secs % 3600 / 60).into_owned()
    }
}

#[derive(Default)]
pub(crate) struct OpCenter {
    /// Newest last
    jobs: Vec<Job>,
    next_id: u64,
    /// The periodic progress task is running
    ticking: bool,
    /// Something finished while the list was closed: mark the button (`true` if anything failed)
    unseen: Option<bool>,
}

impl Global for OpCenter {}

impl OpCenter {
    /// Start a job: returns its id and the progress handle for the background thread.
    pub(crate) fn begin(
        cx: &mut App,
        verb: &'static str,
        subject: Option<(String, usize)>,
    ) -> (u64, Arc<OpProgress>) {
        let progress = Arc::new(OpProgress::default());
        let center = cx.default_global::<Self>();
        center.next_id += 1;
        let id = center.next_id;
        center.jobs.push(Job {
            id,
            verb,
            subject,
            progress: Arc::clone(&progress),
            started: Instant::now(),
            state: JobState::Running,
            undo: None,
            redo: None,
            retry: None,
            was_queued: false,
            samples: VecDeque::new(),
        });
        let start_ticking = !std::mem::replace(&mut center.ticking, true);
        // Notify views watching this global that a job was added.
        cx.update_global::<Self, _>(|_, _| {});
        if start_ticking {
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    let more =
                        cx.update(|cx| cx.update_global::<Self, _>(|center, _| center.tick()));
                    if !more {
                        break;
                    }
                }
            })
            .detach();
        }
        (id, progress)
    }

    /// Job done (or canceled, failed). `verb` may be more specific than at start (paste turns out
    /// to be copy or move). A failure is kept as its display text for the list.
    pub(crate) fn finish<E: std::fmt::Display>(
        cx: &mut App,
        id: u64,
        verb: &'static str,
        result: &Result<OpOutcome, E>,
    ) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global::<Self, _>(|center, _| {
            let Some(job) = center.jobs.iter_mut().find(|job| job.id == id) else {
                return;
            };
            job.verb = verb;
            job.state = match result {
                Ok(OpOutcome::Done) => JobState::Done {
                    took: job.started.elapsed(),
                },
                Ok(OpOutcome::Cancelled) => JobState::Cancelled,
                Err(why) => JobState::Failed(why.to_string()),
            };
            let failed = matches!(job.state, JobState::Failed(_));
            center.unseen = Some(center.unseen.unwrap_or(false) || failed);
            center.trim();
        });
    }

    /// Drops a job that stopped only to ask something (an archive's password, #346): the retry
    /// is a job of its own.
    pub(crate) fn forget(cx: &mut App, id: u64) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global::<Self, _>(|center, _| center.jobs.retain(|job| job.id != id));
    }

    /// The finished job can be undone with this undo history id.
    pub(crate) fn set_undo(cx: &mut App, id: u64, undo: u64) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global::<Self, _>(|center, _| {
            if let Some(job) = center.jobs.iter_mut().find(|job| job.id == id) {
                job.undo = Some(undo);
            }
        });
    }

    /// The finished undo can be redone with this redo stack id.
    pub(crate) fn set_redo(cx: &mut App, id: u64, redo: u64) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global::<Self, _>(|center, _| {
            if let Some(job) = center.jobs.iter_mut().find(|job| job.id == id) {
                job.redo = Some(redo);
            }
        });
    }

    /// The failed job can be retried with this operation.
    pub(crate) fn set_retry(cx: &mut App, id: u64, op: crate::shell::FileOp) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global::<Self, _>(|center, _| {
            if let Some(job) = center.jobs.iter_mut().find(|job| job.id == id) {
                job.retry = Some(op);
            }
        });
    }

    /// Takes the job's retry operation (once: the retry runs as a new job).
    pub(crate) fn take_retry(cx: &mut App, id: u64) -> Option<crate::shell::FileOp> {
        if !cx.has_global::<Self>() {
            return None;
        }
        cx.update_global::<Self, _>(|center, _| {
            center
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .and_then(|job| job.retry.take())
        })
    }

    /// Paste learns its target only after reading the clipboard.
    pub(crate) fn set_subject(cx: &mut App, id: u64, subject: (String, usize)) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global::<Self, _>(|center, _| {
            if let Some(job) = center.jobs.iter_mut().find(|job| job.id == id) {
                job.subject = Some(subject);
            }
        });
    }

    /// Periodic: record a speed sample; returns whether anything is still running (otherwise stop
    /// polling).
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        for job in self.jobs.iter_mut().filter(|job| job.running()) {
            // The progress writer and its tab are both gone (tab closed), so nobody will report a
            // result: treat it as finished.
            if Arc::strong_count(&job.progress) == 1 {
                job.state = JobState::Done {
                    took: job.started.elapsed(),
                };
                continue;
            }
            // Waiting in the queue: no speed, and time spent waiting doesn't count.
            let queued = job.progress.queued.load(Ordering::Relaxed);
            let was_queued = std::mem::replace(&mut job.was_queued, queued);
            if queued || was_queued {
                job.started = now;
                job.samples.clear();
                continue;
            }
            job.samples
                .push_back((now, job.done_items(), job.bytes_so_far()));
            while job
                .samples
                .front()
                .is_some_and(|(at, _, _)| now.duration_since(*at) > RATE_WINDOW)
            {
                job.samples.pop_front();
            }
        }
        self.ticking = self.jobs.iter().any(Job::running);
        self.ticking
    }

    /// Keep only the latest finished jobs.
    fn trim(&mut self) {
        let finished = self.jobs.iter().filter(|job| !job.running()).count();
        let mut drop = finished.saturating_sub(HISTORY_MAX);
        self.jobs.retain(|job| {
            if drop > 0 && !job.running() {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn jobs(cx: &App) -> &[Job] {
        cx.try_global::<Self>()
            .map_or(&[], |center| center.jobs.as_slice())
    }

    /// Finished while the list was closed and not yet seen: `Some(true)` if any failed.
    pub(crate) fn unseen(cx: &App) -> Option<bool> {
        cx.try_global::<Self>()?.unseen
    }

    pub(crate) fn mark_seen(cx: &mut App) {
        if cx.try_global::<Self>().is_some_and(|c| c.unseen.is_some()) {
            cx.update_global::<Self, _>(|center, _| center.unseen = None);
        }
    }

    /// Remove finished jobs from the list (running ones stay).
    pub(crate) fn clear_finished(cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.update_global::<Self, _>(|center, _| {
                center.jobs.retain(Job::running);
                center.unseen = None;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A move shows scissors in the toast and the operation center alike; copies a copy icon.
    #[test]
    fn a_move_shows_scissors() {
        use crate::assets::Icon;
        assert_eq!(op_icon("op.move"), Icon::Cut);
        assert_eq!(op_icon("op.copy"), Icon::Copy);
        assert_eq!(op_icon("op.recycle"), Icon::Trash);
    }

    /// A long name is what gets cut short in the list, never the item count (#403): in every
    /// language the name and the count are pieces of their own, and the pieces still read as the
    /// whole title.
    #[test]
    fn the_title_keeps_the_count_apart_from_the_name() {
        let name = "ClassifiedArchive-2026-final.zip";
        for locale in ["en", "zh-TW", "zh-CN", "ja", "de", "es", "pt-BR"] {
            for verb in ["op.copy", "op.move", "op.recycle"] {
                let parts = title_parts(verb, Some((name, 1234)), locale);
                assert_eq!(parts.name, name, "{locale} {verb}: {parts:?}");
                assert!(
                    parts.tail.starts_with("1,234"),
                    "{locale} {verb}: the count leads its own piece: {parts:?}"
                );
                assert!(
                    !parts.before.contains("1,234") && !parts.between.contains("1,234"),
                    "{locale} {verb}: {parts:?}"
                );
                let whole = if verb == "op.recycle" {
                    t!(
                        "ops.recycle_many",
                        locale = locale,
                        name = name,
                        count = "1,234"
                    )
                } else {
                    t!(
                        "ops.subject_many",
                        locale = locale,
                        op = t!(verb, locale = locale),
                        name = name,
                        count = "1,234"
                    )
                };
                assert_eq!(parts.text(), whole, "{locale} {verb}");
            }
        }
        let one = title_parts("op.copy", Some((name, 1)), "en");
        assert_eq!((one.name.as_str(), one.tail.as_str()), (name, ""));
        assert_eq!(one.text(), "Copy “ClassifiedArchive-2026-final.zip”");
        // Paste before the clipboard is read: just the operation.
        assert_eq!(
            title_parts("op.paste", None, "en"),
            TitleParts {
                before: "Paste".to_owned(),
                ..TitleParts::default()
            }
        );
    }

    /// Recycling one big folder reports no finished item for a long time; the line says how many
    /// items the copy engine counted instead of a bare "getting ready".
    #[test]
    fn a_long_single_item_job_says_how_many_items_it_counted() {
        let progress = Arc::new(OpProgress::default());
        let job = Job {
            id: 1,
            verb: "op.recycle",
            subject: Some(("big".into(), 1)),
            progress: Arc::clone(&progress),
            started: Instant::now(),
            state: JobState::Running,
            undo: None,
            redo: None,
            retry: None,
            was_queued: false,
            samples: VecDeque::new(),
        };
        assert_eq!(job.detail(), t!("ops.state_preparing"));
        progress.work_total.store(20_022, Ordering::Relaxed);
        assert_eq!(job.detail(), tn!("ops.state_preparing_items", 20_022));
    }

    /// A job counted in bytes that hasn't read any yet (a big file dragged out of a zip while
    /// File Explorer unpacks it) is getting ready, not getting 1,572,864,000 items ready (#456).
    #[test]
    fn a_job_counted_in_bytes_is_getting_ready_until_bytes_arrive() {
        let progress = Arc::new(OpProgress::default());
        let job = Job {
            id: 1,
            verb: "op.copy",
            subject: Some(("big.iso".into(), 1)),
            progress: Arc::clone(&progress),
            started: Instant::now(),
            state: JobState::Running,
            undo: None,
            redo: None,
            retry: None,
            was_queued: false,
            samples: VecDeque::new(),
        };
        progress.work_in_bytes.store(true, Ordering::Relaxed);
        progress.work_total.store(1_572_864_000, Ordering::Relaxed);
        assert_eq!(job.detail(), t!("ops.state_preparing"));
        progress.work_done.store(786_432_000, Ordering::Relaxed);
        assert!(
            job.detail()
                .starts_with(&*t!("ops.state_percent", percent = 50))
        );
    }

    #[test]
    fn rate_needs_time_and_movement() {
        assert_eq!(rate(600, Duration::from_secs(2)), Some(300.));
        assert_eq!(
            rate(600, Duration::from_millis(400)),
            None,
            "too short to count"
        );
        assert_eq!(
            rate(0, Duration::from_secs(3)),
            None,
            "no movement, no rate of 0/s"
        );
    }

    #[test]
    fn eta_waits_until_it_means_something() {
        // A quarter done in 10 seconds: 30 seconds left.
        assert_eq!(
            remaining(0.25, Duration::from_secs(10)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            remaining(0.01, Duration::from_secs(10)),
            None,
            "just started"
        );
        assert_eq!(
            remaining(0.5, Duration::from_secs(1)),
            None,
            "not running long enough"
        );
        assert_eq!(
            remaining(1.0, Duration::from_secs(10)),
            None,
            "already done"
        );
    }

    #[test]
    fn eta_is_rounded_up_to_something_readable() {
        assert_eq!(round_eta(Duration::from_secs(3)), Duration::from_secs(5));
        assert_eq!(round_eta(Duration::from_secs(42)), Duration::from_secs(45));
        assert_eq!(round_eta(Duration::from_secs(61)), Duration::from_secs(120));
        assert_eq!(
            round_eta(Duration::from_millis(200)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn finished_history_is_capped_but_running_jobs_stay() {
        let job = |id: u64, state: JobState| Job {
            id,
            verb: "op.copy",
            subject: None,
            progress: Arc::new(OpProgress::default()),
            started: Instant::now(),
            state,
            undo: None,
            redo: None,
            retry: None,
            was_queued: false,
            samples: VecDeque::new(),
        };
        let mut center = OpCenter::default();
        center.jobs.push(job(0, JobState::Running));
        for id in 1..=(HISTORY_MAX as u64 + 5) {
            center.jobs.push(job(id, JobState::Cancelled));
        }
        center.trim();
        assert_eq!(center.jobs.len(), HISTORY_MAX + 1);
        assert!(center.jobs[0].running(), "running jobs are never evicted");
        assert_eq!(center.jobs[1].id, 6, "the oldest are evicted");
    }

    /// Time spent waiting in the queue doesn't count toward the job's duration or speed.
    #[test]
    fn the_clock_starts_when_a_queued_job_leaves_the_queue() {
        let mut center = OpCenter::default();
        let progress = Arc::new(OpProgress::default());
        progress.queued.store(true, Ordering::Relaxed);
        let long_ago = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        center.jobs.push(Job {
            id: 1,
            verb: "op.copy",
            subject: None,
            progress: Arc::clone(&progress),
            started: long_ago,
            state: JobState::Running,
            undo: None,
            redo: None,
            retry: None,
            was_queued: false,
            samples: VecDeque::new(),
        });
        assert!(center.jobs[0].queued());
        assert!(center.tick());
        assert!(center.jobs[0].started > long_ago, "waiting doesn't count");
        assert!(center.jobs[0].samples.is_empty());
        let waited = center.jobs[0].started;
        progress.queued.store(false, Ordering::Relaxed);
        center.tick();
        assert!(
            center.jobs[0].started >= waited,
            "restarted once it left the queue"
        );
        center.tick();
        assert_eq!(center.jobs[0].samples.len(), 1, "now measured");
    }

    /// The starting tab closed and the background work finished: nobody reports, so the next poll
    /// treats it as done.
    #[test]
    fn orphaned_jobs_are_closed_out() {
        let mut center = OpCenter::default();
        let held = Arc::new(OpProgress::default());
        for (id, progress) in [(1, Arc::clone(&held)), (2, Arc::new(OpProgress::default()))] {
            center.jobs.push(Job {
                id,
                verb: "op.recycle",
                subject: None,
                progress,
                started: Instant::now(),
                state: JobState::Running,
                undo: None,
                redo: None,
                retry: None,
                was_queued: false,
                samples: VecDeque::new(),
            });
        }
        assert!(center.tick(), "one is still running");
        assert!(center.jobs[0].running());
        assert!(matches!(center.jobs[1].state, JobState::Done { .. }));
        drop(held);
        assert!(!center.tick());
    }
}
