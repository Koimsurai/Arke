//! Dual pane tools on each pane: the synchronized browsing and compare toggles next to the split
//! button, the badges a folder comparison puts on items, and the "N differ · Select" chip.
//!
//! The comparison itself is `arke_core::compare`, run by `Workspace` (it sees both panes); a pane
//! only shows the marks it was handed and drops them when it moves to another folder.

use std::collections::HashMap;
use std::sync::Arc;

use arke_core::compare::Mark;
use gpui::{App, ClickEvent, Context, SharedString, Window, div, prelude::*, svg};
use rust_i18n::t;

use super::{Explorer, ExplorerEvent};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme::{self, UiFont as _};
use crate::workspace::TooltipBelowExt as _;

/// Marks for one pane, by item name.
pub(crate) type Marks = Arc<HashMap<Arc<str>, Mark>>;

impl Explorer {
    /// Shows (or clears) the marks of a folder comparison.
    pub(crate) fn set_compare(&mut self, marks: Option<Marks>, cx: &mut Context<Self>) {
        self.compare = marks;
        cx.notify();
    }

    /// Selects every item the comparison marked, cursor on the first one in list order.
    pub(crate) fn select_compared(&mut self, cx: &mut Context<Self>) {
        let (Some(marks), Some(view)) = (self.compare.clone(), self.view()) else {
            return;
        };
        let marked: Vec<(usize, Arc<str>)> = view
            .iter()
            .enumerate()
            .filter(|(_, e)| marks.contains_key(&e.name))
            .map(|(ix, e)| (ix, e.name.clone()))
            .collect();
        let first = marked.first().cloned();
        self.selection
            .select_names(marked.into_iter().map(|(_, name)| name));
        if let Some((ix, name)) = first {
            self.selection.set_cursor_name(Some(name));
            self.cursor = Some(ix);
            self.scroll_to_index(ix, gpui::ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Context bar chip while comparing: how many items differ here, and a button that selects
    /// them in both panes. `None` outside a comparison.
    pub(super) fn render_compare_chip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.pane.comparing {
            return None;
        }
        let count = self.compare.as_ref().map_or(0, |m| m.len());
        let text = if self.compare.is_none() {
            t!("compare.working").into_owned()
        } else if count == 0 {
            t!("compare.same").into_owned()
        } else {
            tn!("compare.differ", count).into_owned()
        };
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(theme::S_4)
                .text_size(theme::FS_12)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::S_6)
                        .px(theme::S_6)
                        .h(theme::H_CTL_SM)
                        .text_color(theme::text_muted())
                        .child(
                            svg()
                                .path(Icon::Compare.path())
                                .size(theme::ICON)
                                .flex_none()
                                .text_color(theme::diff_changed()),
                        )
                        .child(text),
                )
                .when(count > 0, |d| {
                    d.child(
                        div()
                            .id("compare-select")
                            .a11y_button(t!("compare.select_tip"))
                            .h(theme::H_CTL_SM)
                            .px(theme::S_8)
                            .flex()
                            .items_center()
                            .rounded(theme::R_8)
                            .cursor_pointer()
                            .text_color(theme::text())
                            .hover(|s| s.bg(theme::wash_strong()))
                            .child(t!("compare.select").into_owned())
                            .tooltip_below(t!("compare.select_tip").into_owned())
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(ExplorerEvent::SelectDifferences);
                            })),
                    )
                }),
        )
    }

    /// Dual pane toggles beside the split button: synchronized browsing and compare folders.
    pub(super) fn render_dual_toggles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme::S_4)
            .child(toggle(
                "toggle-sync",
                Icon::Link,
                self.pane.synced,
                format!("{} (Ctrl+Shift+S)", t!("compare.sync_tip")),
                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ExplorerEvent::ToggleSync)),
            ))
            .child(toggle(
                "toggle-compare",
                Icon::Compare,
                self.pane.comparing,
                format!("{} (Ctrl+Shift+D)", t!("compare.compare_tip")),
                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ExplorerEvent::ToggleCompare)),
            ))
    }

    /// The comparison mark of an item, if any.
    pub(super) fn compare_mark(&self, name: &str) -> Option<Mark> {
        self.compare.as_ref()?.get(name).copied()
    }
}

/// A view toggle like the preview and split buttons: a wash while on.
fn toggle(
    id: &'static str,
    icon: Icon,
    on: bool,
    tip: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        // A new id per state, so the tooltip from before the click doesn't linger.
        .id(SharedString::from(format!("{id}-{on}")))
        .a11y_toggle(tip.clone(), on)
        .h(theme::H_CTL_SM)
        .px(theme::S_6)
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(on, |d| d.bg(theme::wash_strong()))
        .when(!on, |d| d.hover(|s| s.bg(theme::wash())))
        .on_click(on_click)
        .child(svg().path(icon.path()).size(theme::ICON).text_color(if on {
            theme::text()
        } else {
            theme::text_subtle()
        }))
        .tooltip_below(tip)
}

/// The short label on a compared item: "Only here", "Newer", "Older" or "Differs", always text
/// (never color alone), tinted blue for one-sided items and amber for changed ones.
pub(super) fn compare_badge(mark: Option<Mark>) -> Option<impl IntoElement> {
    let mark = mark?;
    let (text, color) = match mark {
        Mark::Unique => (t!("compare.only_here"), theme::diff_unique()),
        Mark::Newer => (t!("compare.newer"), theme::diff_changed()),
        Mark::Older => (t!("compare.older"), theme::diff_changed()),
        Mark::Differs => (t!("compare.differs"), theme::diff_changed()),
    };
    Some(
        div()
            .flex_none()
            .px(theme::S_4)
            .rounded(theme::R_4)
            .bg(theme::diff_wash(color))
            .ui_font()
            .text_size(theme::FS_11)
            .font_weight(theme::FW_STRONG)
            .text_color(color)
            .child(text.into_owned()),
    )
}
