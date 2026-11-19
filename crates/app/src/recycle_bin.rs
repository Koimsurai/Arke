//! Recycle Bin (Arke's own page, opened from the sidebar in place of the pane).
//!
//! Lists the recycle bins of all drives: name, original location, deletion date, size. Restore or
//! permanently delete the selection, restore all, or empty. Permanent delete always asks first
//! (Arke's own dialog) and then runs as Arke's own operation, with no Windows dialogs: progress and
//! Cancel in a toast, listed in the operation center, items that fail named in an error toast while
//! the rest go. Name conflicts on restore are left to Windows. Listing, restore and delete run in
//! the background.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnimationExt, AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable,
    KeyBinding, MouseButton, MouseDownEvent, Pixels, Point, ScrollStrategy, SharedString,
    Subscription, UniformListScrollHandle, Window, actions, anchored, deferred, div, point,
    prelude::*, px, svg, uniform_list,
};
use rust_i18n::t;

use arke_core::filetype::{FileKind, kind_for_extension};
use arke_core::format::{format_count, format_size};

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{Explorer, kind_icon, spawn_thread};
use crate::op_center::OpCenter;
use crate::scrollbar::ScrollbarDrag;
use crate::shell::{OpOutcome, OpProgress, RecycledItem, Shell, ShellError};
use crate::theme;

/// Permanent delete's name in the operation center and its toast (locale key)
const PURGE_VERB: &str = "op.delete_permanently";

actions!(
    recycle_bin,
    [
        RecycleClose,
        RecycleSelectAll,
        RecyclePurge,
        RecycleReload,
        RecycleUp,
        RecycleDown,
        RecycleConfirm
    ]
);

const CONTEXT: &str = "RecycleBin";
/// Hide the original location column below this window width
fn from_column_from() -> gpui::Pixels {
    crate::theme::scaled_px(980.)
}
fn menu_w() -> gpui::Pixels {
    crate::theme::scaled_px(220.)
}

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", RecycleClose, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", RecycleSelectAll, Some(CONTEXT)),
        KeyBinding::new("delete", RecyclePurge, Some(CONTEXT)),
        KeyBinding::new("f5", RecycleReload, Some(CONTEXT)),
        KeyBinding::new("up", RecycleUp, Some(CONTEXT)),
        KeyBinding::new("down", RecycleDown, Some(CONTEXT)),
        KeyBinding::new("enter", RecycleConfirm, Some(CONTEXT)),
    ]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecycleEvent {
    /// Leave this page
    Close,
    /// Open this folder (an item's original location)
    OpenFolder(PathBuf),
    /// Restore or permanent delete finished (drive capacity changed)
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Column {
    Name,
    From,
    Deleted,
    Size,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    Restore,
    Purge,
}

/// Pending confirmation.
struct Ask {
    job: Job,
    items: Vec<RecycledItem>,
    /// The whole recycle bin (different wording)
    all: bool,
}

impl Ask {
    /// Dialog title, message and confirm button label.
    fn text(&self) -> (String, String, String) {
        let n = self.items.len();
        let count = format_count(n);
        let (title, body, ok) = match (self.job, self.all) {
            (Job::Purge, true) => (
                t!("recycle.confirm_empty_title"),
                tn!("recycle.confirm_empty", n),
                t!("recycle.empty_bin"),
            ),
            (Job::Purge, false) => (
                t!("recycle.confirm_purge_title"),
                if let [one] = self.items.as_slice() {
                    t!("recycle.confirm_purge_one", name = one.name)
                } else {
                    t!("recycle.confirm_purge_many", count = count)
                },
                t!("recycle.purge"),
            ),
            (Job::Restore, _) => (
                t!("recycle.confirm_restore_all_title"),
                tn!("recycle.confirm_restore_all", n),
                t!("recycle.restore_all"),
            ),
        };
        (title.into_owned(), body.into_owned(), ok.into_owned())
    }
}

pub(crate) struct RecycleBin {
    shell: Arc<dyn Shell>,
    focus: FocusHandle,
    owner: Option<isize>,
    items: Vec<RecycledItem>,
    /// First load done (don't say "empty" before that)
    loaded: bool,
    error: Option<SharedString>,
    selected: BTreeSet<usize>,
    /// Last clicked row: anchor for Shift ranges and arrow keys
    cursor: Option<usize>,
    sort: (Column, bool),
    scroll: UniformListScrollHandle,
    scrollbar_drag: ScrollbarDrag,
    ask: Option<Ask>,
    /// The open context menu: where it was opened, and the window it is drawn in (`popup.rs`).
    menu: Option<(Point<Pixels>, crate::popup::Single)>,
    notice: Option<SharedString>,
    /// Why the last restore or permanent delete failed (some items or all), until closed
    failure: Option<SharedString>,
    /// Listing request counter; older results are dropped
    serial: u32,
    /// Redraws the progress toast as the operation center polls the job, and follows a restore or
    /// permanent delete starting and ending (also one started by an earlier page)
    _subscriptions: [Subscription; 2],
}

/// The restore or permanent delete running now, with its progress. Kept by the operation rather
/// than the page, so closing and reopening the page while the bin empties can't start emptying the
/// same items again.
#[derive(Default)]
struct Running(Option<(Job, Arc<OpProgress>)>);

impl gpui::Global for Running {}

impl EventEmitter<RecycleEvent> for RecycleBin {}

impl Focusable for RecycleBin {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl RecycleBin {
    pub(crate) fn new(shell: Arc<dyn Shell>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let ops = cx.observe_global::<OpCenter>(|_: &mut Self, cx| {
            if Self::busy(cx).is_some() {
                cx.notify();
            }
        });
        let running = cx.observe_global::<Running>(|this: &mut Self, cx| {
            // Finished: show what is left.
            if Self::busy(cx).is_none() {
                this.reload(cx);
            }
            cx.notify();
        });
        let mut this = Self {
            shell,
            focus,
            owner: Explorer::owner_from(window),
            items: Vec::new(),
            loaded: false,
            error: None,
            selected: BTreeSet::new(),
            cursor: None,
            // Most recently deleted first: usually what you're looking for.
            sort: (Column::Deleted, false),
            scroll: UniformListScrollHandle::new(),
            scrollbar_drag: ScrollbarDrag::default(),
            ask: None,
            menu: None,
            notice: None,
            failure: None,
            serial: 0,
            _subscriptions: [ops, running],
        };
        this.reload(cx);
        this
    }

    /// The restore or permanent delete running now, from this page or an earlier one.
    fn busy(cx: &App) -> Option<Job> {
        cx.try_global::<Running>()?.0.as_ref().map(|(job, _)| *job)
    }

    /// Progress of the permanent delete running now (drawn as a toast, with Cancel).
    fn progress(cx: &App) -> Option<Arc<OpProgress>> {
        match &cx.try_global::<Running>()?.0 {
            Some((Job::Purge, progress)) => Some(Arc::clone(progress)),
            _ => None,
        }
    }

    /// Reload the listing in the background. Selection is kept by item.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let shell = Arc::clone(&self.shell);
        let (tx, rx) = flume::bounded(1);
        spawn_thread("recycle-list", move || {
            let _ = tx.send(shell.recycle_bin());
        });
        cx.spawn(async move |this, cx| {
            // Shown in the page as is; no reply (thread gone) is an error without text.
            let result = match rx.recv_async().await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err(String::new()),
            };
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                this.loaded = true;
                match result {
                    Ok(items) => {
                        this.error = None;
                        this.replace_items(items);
                    }
                    Err(why) => this.error = Some(why.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Swap in new items sorted by the current order, keeping selections that still exist.
    fn replace_items(&mut self, items: Vec<RecycledItem>) {
        let chosen: BTreeSet<String> = self
            .selected
            .iter()
            .filter_map(|&ix| self.items.get(ix).map(|item| item.now.clone()))
            .collect();
        let cursor = self
            .cursor
            .and_then(|ix| self.items.get(ix).map(|item| item.now.clone()));
        self.items = items;
        self.sort_items();
        self.selected = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| chosen.contains(&item.now))
            .map(|(ix, _)| ix)
            .collect();
        self.cursor = cursor.and_then(|now| self.items.iter().position(|item| item.now == now));
    }

    fn sort_items(&mut self) {
        let (column, ascending) = self.sort;
        self.items.sort_by(|a, b| {
            let order = match column {
                Column::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Column::From => a.from.cmp(&b.from),
                Column::Deleted => a.deleted.cmp(&b.deleted),
                Column::Size => a.size.cmp(&b.size),
            };
            let order = if ascending { order } else { order.reverse() };
            // Ties by name so the order is stable.
            order.then_with(|| a.name.cmp(&b.name))
        });
    }

    fn set_sort(&mut self, column: Column, cx: &mut Context<Self>) {
        let ascending = if self.sort.0 == column {
            !self.sort.1
        } else {
            // Name and location ascending; date and size newest/largest first.
            matches!(column, Column::Name | Column::From)
        };
        self.sort = (column, ascending);
        // Keep the same items selected after re-sorting.
        let items = self.items.clone();
        self.replace_items(items);
        cx.notify();
    }

    fn click_row(&mut self, ix: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        let m = event.modifiers();
        if m.shift {
            let from = self.cursor.unwrap_or(ix);
            let (lo, hi) = (from.min(ix), from.max(ix));
            if !m.control {
                self.selected.clear();
            }
            self.selected.extend(lo..=hi);
        } else if m.control {
            if !self.selected.remove(&ix) {
                self.selected.insert(ix);
            }
            self.cursor = Some(ix);
        } else {
            self.selected = BTreeSet::from([ix]);
            self.cursor = Some(ix);
        }
        self.notice = None;
        cx.notify();
    }

    fn open_menu(&mut self, ix: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        if Self::busy(cx).is_some() {
            return;
        }
        // Right-click on an unselected row selects only it (like the file list).
        if !self.selected.contains(&ix) {
            self.selected = BTreeSet::from([ix]);
            self.cursor = Some(ix);
        }
        self.menu = Some((position, crate::popup::Single::new()));
        self.notice = None;
        cx.notify();
    }

    fn chosen(&self) -> Vec<RecycledItem> {
        self.selected
            .iter()
            .filter_map(|&ix| self.items.get(ix).cloned())
            .collect()
    }

    fn move_cursor(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.items.is_empty() || self.ask.is_some() {
            return;
        }
        let last = self.items.len() - 1;
        let next = match (self.cursor, down) {
            (None, _) => 0,
            (Some(ix), true) => (ix + 1).min(last),
            (Some(ix), false) => ix.saturating_sub(1),
        };
        self.cursor = Some(next);
        self.selected = BTreeSet::from([next]);
        self.scroll.scroll_to_item(next, ScrollStrategy::Top);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected = (0..self.items.len()).collect();
        cx.notify();
    }

    /// Esc unwinds one level at a time (dialog -> menu -> selection -> leave the page).
    fn close(&mut self, cx: &mut Context<Self>) {
        if self.ask.take().is_some() || self.menu.take().is_some() {
            cx.notify();
        } else if !self.selected.is_empty() {
            self.selected.clear();
            cx.notify();
        } else {
            cx.emit(RecycleEvent::Close);
        }
    }

    fn restore_chosen(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        let items = self.chosen();
        self.run(Job::Restore, items, cx);
    }

    fn ask_purge_chosen(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        let items = self.chosen();
        self.ask(Job::Purge, items, false, cx);
    }

    fn ask(&mut self, job: Job, items: Vec<RecycledItem>, all: bool, cx: &mut Context<Self>) {
        if items.is_empty() || Self::busy(cx).is_some() {
            cx.notify();
            return;
        }
        self.ask = Some(Ask { job, items, all });
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(ask) = self.ask.take() {
            self.run(ask.job, ask.items, cx);
        }
    }

    fn open_origin(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(item) = self.chosen().into_iter().next() {
            cx.emit(RecycleEvent::OpenFolder(item.from));
        }
        cx.notify();
    }

    /// Restore or permanently delete in the background, then reload and report the count in the
    /// command bar. A permanent delete is Arke's own (no Windows dialogs): it is listed in the
    /// operation center and shows a progress toast with Cancel; failures stay in an error toast.
    fn run(&mut self, job: Job, items: Vec<RecycledItem>, cx: &mut Context<Self>) {
        if items.is_empty() || Self::busy(cx).is_some() {
            return;
        }
        self.notice = None;
        self.failure = None;
        let shell = Arc::clone(&self.shell);
        let owner = self.owner;
        let count = items.len();
        let (center_job, progress) = if job == Job::Purge {
            let subject = items.first().map(|item| (item.name.clone(), count));
            let (id, progress) = OpCenter::begin(cx, PURGE_VERB, subject);
            (Some(id), progress)
        } else {
            (None, Arc::new(OpProgress::default()))
        };
        cx.set_global(Running(Some((job, Arc::clone(&progress)))));
        cx.notify();
        let (tx, rx) = flume::bounded(1);
        spawn_thread("recycle-op", move || {
            let result = match job {
                Job::Restore => shell.recycle_restore(&items, owner),
                Job::Purge => shell.recycle_purge(&items, &progress),
            };
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let result = rx.recv_async().await;
            // Cancelled after some items failed or with one half deleted: both are said.
            let stopped = matches!(result, Ok(Err(ShellError::Stopped { .. })));
            let result = match result {
                Ok(result) => result.map_err(|e| e.to_string()),
                // The thread died without an answer.
                Err(_) => Err(t!("ops.crashed").into_owned()),
            };
            // Recorded even if the page was closed meanwhile.
            cx.update(|cx| {
                cx.set_global(Running(None));
                if let Some(id) = center_job {
                    OpCenter::finish(cx, id, PURGE_VERB, &result);
                }
            });
            let _ = this.update(cx, |this, cx| {
                this.notice = match &result {
                    Ok(OpOutcome::Done) => Some(
                        match job {
                            Job::Restore => tn!("recycle.restored", count),
                            Job::Purge => tn!("recycle.purged", count),
                        }
                        .into_owned()
                        .into(),
                    ),
                    Ok(OpOutcome::Cancelled) => match job {
                        Job::Purge => {
                            Some(t!("ops.cancelled", op = t!(PURGE_VERB)).into_owned().into())
                        }
                        // The user canceled in the Windows dialog; no need to say it again.
                        Job::Restore => None,
                    },
                    Err(_) if stopped => {
                        Some(t!("ops.cancelled", op = t!(PURGE_VERB)).into_owned().into())
                    }
                    Err(_) => None,
                };
                this.failure = result.err().map(|why| {
                    if stopped {
                        return why.into();
                    }
                    let op = match job {
                        Job::Restore => t!("recycle.restore"),
                        Job::Purge => t!(PURGE_VERB),
                    };
                    t!("ops.failed", op = op, error = why).into_owned().into()
                });
                this.selected.clear();
                this.cursor = None;
                this.reload(cx);
                cx.emit(RecycleEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(test)]
    pub(crate) fn items_for_test(&self) -> Vec<String> {
        self.items.iter().map(|item| item.name.clone()).collect()
    }

    #[cfg(test)]
    pub(crate) fn select_for_test(&mut self, names: &[&str]) {
        self.selected = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| names.contains(&item.name.as_str()))
            .map(|(ix, _)| ix)
            .collect();
    }

    /// Tests: press a command bar button (restore, delete permanently, restore all, empty); returns
    /// whether a confirmation opened.
    #[cfg(test)]
    pub(crate) fn press_for_test(&mut self, job: Job, all: bool, cx: &mut Context<Self>) -> bool {
        match (job, all) {
            (Job::Restore, false) => self.restore_chosen(cx),
            (Job::Purge, false) => self.ask_purge_chosen(cx),
            (job, true) => self.ask(job, self.items.clone(), true, cx),
        }
        self.ask.is_some()
    }

    /// Tests: the error toast's text, the command bar status, and whether a permanent delete runs.
    #[cfg(test)]
    pub(crate) fn failure_for_test(&self) -> Option<String> {
        self.failure.as_ref().map(ToString::to_string)
    }

    #[cfg(test)]
    pub(crate) fn status_for_test(&self, cx: &App) -> String {
        self.status(cx).to_string()
    }

    #[cfg(test)]
    pub(crate) fn purging_for_test(cx: &App) -> bool {
        Self::progress(cx).is_some()
    }

    #[cfg(test)]
    pub(crate) fn answer_for_test(&mut self, yes: bool, cx: &mut Context<Self>) {
        if yes {
            self.confirm(cx);
        } else {
            self.ask = None;
        }
    }
}

impl Render for RecycleBin {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let wide = window.viewport_size().width >= from_column_from();
        let body = if let Some(error) = self.error.clone() {
            message(Icon::Trash, error, None)
        } else if !self.loaded {
            message(Icon::Trash, t!("recycle.loading").into_owned().into(), None)
        } else if self.items.is_empty() {
            message(
                Icon::Trash,
                t!("recycle.empty").into_owned().into(),
                Some(t!("recycle.empty_hint").into_owned().into()),
            )
        } else {
            let list = uniform_list(
                "recycle-rows",
                self.items.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    this.render_rows(range, wide, cx)
                }),
            )
            .track_scroll(&self.scroll)
            .pt(theme::s_4())
            .flex_1();
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(list)
                .child(crate::scrollbar::scrollbar(
                    self.scroll.0.borrow().base_handle.clone(),
                    Rc::clone(&self.scrollbar_drag),
                ))
                .into_any_element()
        };
        div()
            .id("recycle-bin")
            .a11y_list(|| t!("recycle.title").into_owned(), self.items.len())
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &RecycleClose, _, cx| this.close(cx)))
            .on_action(cx.listener(|this, _: &RecycleSelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &RecyclePurge, _, cx| this.ask_purge_chosen(cx)))
            .on_action(cx.listener(|this, _: &RecycleReload, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &RecycleUp, _, cx| this.move_cursor(false, cx)))
            .on_action(cx.listener(|this, _: &RecycleDown, _, cx| this.move_cursor(true, cx)))
            .on_action(cx.listener(|this, _: &RecycleConfirm, _, cx| this.confirm(cx)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_commands(cx))
            .when(self.loaded && !self.items.is_empty(), |d| {
                d.child(self.render_header(wide, cx))
            })
            .child(body)
            .children(self.render_toasts(cx))
            .children(self.render_menu(window, cx))
            .children(self.render_ask(cx))
    }
}

impl RecycleBin {
    /// Command bar status: what's running or just finished, otherwise item count and size.
    fn status(&self, cx: &App) -> SharedString {
        let total: u64 = self.items.iter().map(|item| item.size).sum();
        let chosen: u64 = self
            .selected
            .iter()
            .filter_map(|&ix| self.items.get(ix))
            .map(|item| item.size)
            .sum();
        match (Self::busy(cx), &self.notice) {
            (Some(Job::Restore), _) => t!("recycle.restoring").into_owned().into(),
            (Some(Job::Purge), _) => t!("recycle.purging").into_owned().into(),
            (None, Some(notice)) => notice.clone(),
            (None, None) if !self.loaded || self.items.is_empty() => SharedString::default(),
            (None, None) if self.selected.is_empty() => tn!(
                "recycle.summary",
                self.items.len(),
                size = format_size(total)
            )
            .into_owned()
            .into(),
            (None, None) => t!(
                "recycle.selected",
                count = format_count(self.selected.len()),
                size = format_size(chosen)
            )
            .into_owned()
            .into(),
        }
    }

    /// Bottom right, like the file list's: the running permanent delete (progress, Cancel) and
    /// what failed (Close).
    fn render_toasts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mut toasts: Vec<AnyElement> = Vec::new();
        if let Some(progress) = &Self::progress(cx) {
            toasts.push(Explorer::op_toast(progress, PURGE_VERB, cx));
        }
        if let Some(failure) = &self.failure {
            toasts.push(crate::explorer::error_toast(
                "recycle-error",
                failure.to_string(),
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.failure = None;
                    cx.notify();
                }),
            ));
        }
        if toasts.is_empty() {
            return None;
        }
        Some(
            div()
                .absolute()
                .right(theme::s_16())
                .bottom(theme::s_16())
                .w(crate::theme::scaled_px(360.))
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .children(toasts)
                .into_any_element(),
        )
    }

    /// Command bar: status on the left, actions on the right.
    fn render_commands(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status(cx);
        let idle = Self::busy(cx).is_none();
        let some = idle && !self.selected.is_empty();
        let any = idle && !self.items.is_empty();
        div()
            .flex_none()
            .h(theme::h_ctx())
            .px(theme::s_12())
            .flex()
            .items_center()
            .gap(theme::s_4())
            .border_b_1()
            .border_color(theme::line())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(theme::s_8())
                    .truncate()
                    .text_color(theme::text_muted())
                    .child(status),
            )
            .child(
                command(
                    "recycle-restore",
                    Icon::Restore,
                    t!("recycle.restore"),
                    some,
                    false,
                )
                .when(some, |d| {
                    d.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.restore_chosen(cx)))
                }),
            )
            .child(
                command(
                    "recycle-purge",
                    Icon::Trash,
                    t!("recycle.purge"),
                    some,
                    true,
                )
                .when(some, |d| {
                    d.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.ask_purge_chosen(cx)))
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(theme::s_16())
                    .mx(theme::s_4())
                    .bg(theme::line()),
            )
            .child(
                command(
                    "recycle-restore-all",
                    Icon::Restore,
                    t!("recycle.restore_all"),
                    any,
                    false,
                )
                .when(any, |d| {
                    d.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.ask(Job::Restore, this.items.clone(), true, cx);
                    }))
                }),
            )
            .child(
                command(
                    "recycle-empty",
                    Icon::Trash,
                    t!("recycle.empty_bin"),
                    any,
                    true,
                )
                .when(any, |d| {
                    d.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.ask(Job::Purge, this.items.clone(), true, cx);
                    }))
                }),
            )
    }

    fn render_header(&self, wide: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(theme::h_header())
            .mx(theme::s_8())
            .px(theme::s_12())
            .gap(theme::s_12())
            .border_b_1()
            .border_color(theme::line())
            .text_size(theme::fs_12())
            .text_color(theme::text_subtle())
            .child(div().w(theme::icon()).flex_none())
            .child(self.header_cell(t!("col.name"), Column::Name, None, cx))
            .when(wide, |d| {
                d.child(self.header_cell(
                    t!("recycle.col_from"),
                    Column::From,
                    Some(theme::col_location()),
                    cx,
                ))
            })
            .child(self.header_cell(
                t!("recycle.col_deleted"),
                Column::Deleted,
                Some(crate::prefs::modified_width()),
                cx,
            ))
            .child(self.header_cell(
                t!("col.size"),
                Column::Size,
                Some(crate::prefs::size_width()),
                cx,
            ))
    }

    fn header_cell(
        &self,
        label: impl std::fmt::Display,
        column: Column,
        width: Option<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.sort.0 == column;
        let arrow = active.then(|| {
            svg()
                .path(Icon::ChevronRight.path())
                .size(theme::icon_sm())
                .flex_none()
                .text_color(theme::text_muted())
                .with_transformation(gpui::Transformation::rotate(gpui::radians(
                    if self.sort.1 {
                        -std::f32::consts::FRAC_PI_2
                    } else {
                        std::f32::consts::FRAC_PI_2
                    },
                )))
        });
        let label: SharedString = label.to_string().into();
        let cell = div()
            .id(SharedString::from(format!("recycle-header-{column:?}")))
            .a11y_column_header(label.clone(), active.then_some(self.sort.1))
            .flex()
            .h_full()
            .items_center()
            .gap(theme::s_4())
            .cursor_pointer()
            .hover(|s| s.text_color(theme::text()))
            .when(active, |d| {
                d.text_color(theme::text_muted())
                    .font_weight(theme::FW_STRONG)
            })
            .child(label)
            .children(arrow)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_sort(column, cx);
            }));
        match width {
            Some(w) if column == Column::Size => cell.w(w).flex_none().justify_end(),
            Some(w) => cell.w(w).flex_none(),
            None => cell.flex_1().min_w_0(),
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        wide: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let full_dates = crate::prefs::full_dates();
        range
            .filter_map(|ix| {
                let item = self.items.get(ix)?;
                let selected = self.selected.contains(&ix);
                let kind = if item.is_dir {
                    FileKind::Folder
                } else {
                    let ext = item
                        .name
                        .rsplit_once('.')
                        .map_or("", |(_, ext)| ext)
                        .to_lowercase();
                    kind_for_extension(&ext)
                };
                let deleted = item.deleted.map_or_else(String::new, |time| {
                    if full_dates {
                        self.shell.format_time(time)
                    } else {
                        self.shell.format_time_short(time)
                    }
                });
                let size = if item.is_dir && item.size == 0 {
                    String::new()
                } else {
                    format_size(item.size)
                };
                // The outer box fills the list width with side margins; the inner one is the row
                // (columns aligned with the header).
                let label = || item_a11y_label(item, &deleted, &size);
                let row = div()
                    .id(("recycle-row", ix))
                    .a11y_item(ix, selected, self.cursor == Some(ix), label)
                    .relative()
                    .w_full()
                    .h(theme::h_row())
                    .px(theme::s_12())
                    .gap(theme::s_12())
                    .flex()
                    .items_center()
                    .rounded(theme::r_8())
                    .when(selected, |d| d.bg(theme::selection()))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::wash())))
                    .child(kind_icon(kind, theme::icon()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(item.name.clone())),
                    )
                    .when(wide, |d| {
                        d.child(
                            div()
                                .w(theme::col_location())
                                .flex_none()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                // Elide the start of long paths; the last folder is what matters.
                                .text_ellipsis_start()
                                .text_color(theme::text_muted())
                                .child(SharedString::from(item.from.display().to_string())),
                        )
                    })
                    .child(
                        div()
                            .w(crate::prefs::modified_width())
                            .flex_none()
                            .text_color(theme::text_muted())
                            .child(deleted),
                    )
                    .child(
                        div()
                            .w(crate::prefs::size_width())
                            .flex_none()
                            .flex()
                            .justify_end()
                            .text_color(theme::text_muted())
                            .child(size),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.click_row(ix, event, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.open_menu(ix, event.position, cx);
                        }),
                    );
                Some(
                    div()
                        .w_full()
                        .px(theme::s_8())
                        .child(row)
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// Context menu: restore, open original location (single selection), delete permanently. It
    /// is drawn in a window of its own (`popup.rs`); what stays here is the scrim that closes it.
    fn render_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (position, popup) = self.menu.as_mut()?;
        let at = crate::popup::to_screen(window, *position);
        popup.ensure(
            || crate::popup::Anchor::At(at.0, at.1),
            |this: &mut Self| this.menu.as_mut().map(|(_, popup)| popup),
            |this, _, _, cx| Some(this.menu_panel(cx)),
            |this, _, cx| {
                this.menu = None;
                cx.notify();
            },
            window,
            cx,
        );
        let viewport = window.viewport_size();
        let scrim = deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("recycle-menu-scrim")
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.menu = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.menu = None;
                            cx.notify();
                        }),
                    ),
            ),
        )
        .with_priority(1);
        Some(scrim.into_any_element())
    }

    /// The context menu's rows, drawn in its own window.
    fn menu_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let one = self.selected.len() == 1;
        div()
            .id("recycle-menu")
            .role(gpui::Role::Menu)
            .occlude()
            .w(menu_w())
            .p(theme::s_4())
            .flex()
            .flex_col()
            .gap(theme::s_2())
            .rounded(theme::r_8())
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .child(
                menu_row(
                    "recycle-menu-restore",
                    Icon::Restore,
                    t!("recycle.restore"),
                    false,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.restore_chosen(cx))),
            )
            .when(one, |d| {
                d.child(
                    menu_row(
                        "recycle-menu-origin",
                        Icon::Open,
                        t!("recycle.open_from"),
                        false,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_origin(cx))),
                )
            })
            .child(
                div()
                    .h(px(1.))
                    .mx(theme::s_4())
                    .my(theme::s_2())
                    .bg(theme::line()),
            )
            .child(
                menu_row("recycle-menu-purge", Icon::Trash, t!("recycle.purge"), true)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.ask_purge_chosen(cx))),
            )
            .with_animation(
                "recycle-menu-appear",
                crate::motion::menu_enter(),
                crate::motion::fade_in,
            )
            .into_any_element()
    }

    /// Confirmation dialog over the page: Enter confirms, Esc or clicking outside cancels.
    fn render_ask(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ask = self.ask.as_ref()?;
        let (title, body, ok) = ask.text();
        let danger = ask.job == Job::Purge;
        Some(
            deferred(
                div()
                    .id("recycle-veil")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::veil())
                    .occlude()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.ask = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id("recycle-ask")
                            .a11y(gpui::Role::AlertDialog, title.clone())
                            .w(crate::theme::scaled_px(420.))
                            .p(theme::s_20())
                            .flex()
                            .flex_col()
                            .gap(theme::s_12())
                            .rounded(theme::r_12())
                            .bg(theme::overlay())
                            .shadow(theme::elev_3())
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                div()
                                    .text_size(theme::fs_15())
                                    .font_weight(theme::FW_STRONG)
                                    .child(title),
                            )
                            .child(div().text_color(theme::text_muted()).child(body))
                            .child(
                                div()
                                    .pt(theme::s_8())
                                    .flex()
                                    .justify_end()
                                    .gap(theme::s_8())
                                    .child(
                                        dialog_button(
                                            "recycle-ask-cancel",
                                            t!("confirm.cancel").into_owned().into(),
                                            false,
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                                this.ask = None;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        dialog_button("recycle-ask-ok", ok.into(), true, danger)
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, _, cx| this.confirm(cx),
                                            )),
                                    ),
                            ),
                    )
                    // The veil and the dialog fade in together.
                    .with_animation("recycle-ask-appear", crate::motion::enter(), |el, t| {
                        crate::motion::slide_in(el, t, 0., 0.)
                    }),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }
}

/// Screen reader name of a row: name, original location, date deleted and size.
fn item_a11y_label(item: &RecycledItem, deleted: &str, size: &str) -> String {
    let from = item.from.display().to_string();
    crate::a11y::item_label(&item.name, &[from.as_str(), deleted, size])
}

/// Centered message (loading, empty, unreadable).
fn message(icon: Icon, title: SharedString, hint: Option<SharedString>) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme::s_8())
        .child(
            svg()
                .path(icon.path())
                .size(crate::theme::scaled_px(40.))
                .text_color(theme::text_disabled()),
        )
        .child(
            div()
                .text_size(theme::fs_15())
                .text_color(theme::text_muted())
                .child(title),
        )
        .children(hint.map(|hint| {
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(hint)
        }))
        .into_any_element()
}

/// Command bar button: icon plus label, dimmed when disabled.
fn command(
    id: &'static str,
    icon: Icon,
    label: impl std::fmt::Display,
    enabled: bool,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let color = match (enabled, danger) {
        (false, _) => theme::text_disabled(),
        (true, true) => theme::danger(),
        (true, false) => theme::text(),
    };
    div()
        .id(id)
        .a11y_lazy(gpui::Role::Button, || label.to_string())
        .a11y_disabled(!enabled)
        .flex_none()
        .h(theme::h_ctl())
        .px(theme::s_8())
        .flex()
        .items_center()
        .gap(theme::s_6())
        .rounded(theme::r_8())
        .text_color(color)
        .when(enabled, |d| {
            d.cursor_pointer().hover(|s| s.bg(theme::wash_strong()))
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme::icon())
                .text_color(color),
        )
        .child(label.to_string())
}

fn menu_row(
    id: &'static str,
    icon: Icon,
    label: impl std::fmt::Display,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let color = if danger {
        theme::danger()
    } else {
        theme::text()
    };
    div()
        .id(id)
        .a11y_lazy(gpui::Role::MenuItem, || label.to_string())
        .h(theme::h_ctl())
        .px(theme::s_8())
        .flex()
        .items_center()
        .gap(theme::s_8())
        .rounded(theme::r_4())
        .cursor_pointer()
        .text_color(color)
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(icon.path())
                .size(theme::icon())
                .text_color(if danger {
                    theme::danger()
                } else {
                    theme::text_muted()
                }),
        )
        .child(label.to_string())
}

fn dialog_button(
    id: &'static str,
    label: SharedString,
    primary: bool,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_button(label.clone())
        .h(theme::h_ctl())
        .px(theme::s_16())
        .flex()
        .items_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .when(primary && danger, |d| {
            d.bg(theme::danger()).text_color(theme::on_signal())
        })
        .when(primary && !danger, |d| {
            d.bg(theme::signal()).text_color(theme::on_signal())
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(label)
}
