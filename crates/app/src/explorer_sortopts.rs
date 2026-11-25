//! Sort flyout, opened from the Sort button beside the view switcher (#179). Like the Files app's
//! sort menu: the columns the list can be sorted by, the current one checked, then Ascending and
//! Descending. Icons and Columns have no column headers, so this is how they are sorted; in
//! Details it is the same as the headers. The background menu's "Sort by" submenu offers the same
//! choices (`explorer_menu.rs`).
//!
//! A choice goes through [`Explorer::set_sort`], as a header click does: the list resorts and the
//! sort is remembered for the folder. The flyout stays open, so a column and a direction can be
//! picked in a row; clicking outside or Esc closes it. It is only built while open.

use std::borrow::Cow;

use arke_core::{SortColumn, SortSpec};
use gpui::{
    AnyElement, ClickEvent, Context, ElementId, MouseButton, MouseDownEvent, Window, anchored,
    deferred, div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use super::Explorer;
use super::toolbar::ToolbarStop as _;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;
use crate::workspace::TooltipBelowExt as _;

fn panel_w() -> gpui::Pixels {
    crate::theme::scaled_px(240.)
}

/// The columns offered, in the order of the Files app's menu and then the extra ones. Link target
/// has nothing to sort by (see `Col::sort_column`).
pub(super) const SORT_COLUMNS: [SortColumn; 7] = [
    SortColumn::Name,
    SortColumn::Modified,
    SortColumn::Type,
    SortColumn::Size,
    SortColumn::Created,
    SortColumn::Extension,
    SortColumn::Attributes,
];

/// The column's name, as its Details header shows it.
pub(super) fn column_label(column: SortColumn) -> Cow<'static, str> {
    match column {
        SortColumn::Name => t!("col.name"),
        SortColumn::Modified => t!("col.modified"),
        SortColumn::Type => t!("col.type"),
        SortColumn::Size => t!("col.size"),
        SortColumn::Created => t!("col.created"),
        SortColumn::Extension => t!("col.extension"),
        SortColumn::Attributes => t!("col.attributes"),
    }
}

/// Short stable name, for test selectors.
const fn column_key(column: SortColumn) -> &'static str {
    match column {
        SortColumn::Name => "name",
        SortColumn::Modified => "modified",
        SortColumn::Type => "type",
        SortColumn::Size => "size",
        SortColumn::Created => "created",
        SortColumn::Extension => "extension",
        SortColumn::Attributes => "attributes",
    }
}

impl Explorer {
    /// Opens or closes the flyout.
    pub(super) fn toggle_sort_options(&mut self, cx: &mut Context<Self>) {
        if !self.close_sort_options(cx) {
            // One flyout at a time.
            self.close_view_options(cx);
            self.sort_options_open = true;
            self.closing_sort_options = None;
            cx.notify();
        }
    }

    /// Closes the flyout (it fades out); returns whether it was open (Esc).
    pub(super) fn close_sort_options(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.sort_options_open) {
            return false;
        }
        crate::motion::Leaving::start(self, (), |this| &mut this.closing_sort_options, cx);
        cx.notify();
        true
    }

    /// A column picked in the flyout or the background menu: a plain click on its header (it
    /// starts in its own direction, further keys are dropped). The column already sorted by keeps
    /// its direction, which Ascending and Descending set.
    pub(super) fn pick_sort_column(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        if self.sort.column == column {
            return;
        }
        let sort = self.sort.toggled(column);
        self.set_sort(sort, cx);
    }

    /// Ascending or Descending picked: the primary key turns that way; any further keys stay, as
    /// with Shift+click on the primary header.
    pub(super) fn pick_sort_direction(&mut self, ascending: bool, cx: &mut Context<Self>) {
        if self.sort.ascending == ascending {
            return;
        }
        let sort = SortSpec {
            ascending,
            ..self.sort
        };
        self.set_sort(sort, cx);
    }

    /// The Sort button with the open flyout under it. Beside the sort icon, a small arrow shows
    /// the direction, as in the column headers.
    pub(super) fn render_sort_options(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.sort_options_open;
        // Up for ascending, as the header's arrow.
        let turn = if self.sort.ascending {
            -std::f32::consts::FRAC_PI_2
        } else {
            std::f32::consts::FRAC_PI_2
        };
        let arrow = svg()
            .path(Icon::ChevronRight.path())
            .size(theme::icon_sm())
            .flex_none()
            .text_color(if open {
                theme::text()
            } else {
                theme::text_subtle()
            })
            .with_transformation(gpui::Transformation::rotate(gpui::radians(turn)));
        let button = div()
            .id("sort-options")
            .debug_selector(|| "sort-options".to_owned())
            .a11y_button(t!("view.sort"))
            .a11y_open_close(open, cx.entity().downgrade(), |this, _, cx| {
                this.toggle_sort_options(cx);
            })
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(theme::s_2())
            .h(theme::h_ctl_sm())
            .px(theme::s_6())
            .rounded(theme::r_8())
            .cursor_pointer()
            .when(open, |d| d.bg(theme::wash_strong()))
            .when(!open, |d| d.hover(|s| s.bg(theme::wash())))
            .child(
                svg()
                    .path(Icon::Sort.path())
                    .size(theme::icon())
                    .flex_none()
                    .text_color(if open {
                        theme::text()
                    } else {
                        theme::text_subtle()
                    }),
            )
            .child(arrow)
            // Opens on press, like a menu; the scrim closes it again.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle_sort_options(cx);
                }),
            )
            .toolbar_menu("sort-options", {
                let this = cx.entity().downgrade();
                move |_, cx| {
                    let _ = this.update(cx, Self::toggle_sort_options);
                }
            })
            .when(!open, |d| d.tooltip_below(t!("view.sort")));
        let mut root = div().relative().flex_none().child(button);
        if open {
            let viewport = window.viewport_size();
            let scrim = deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .id("sort-options-scrim")
                        .w(viewport.width)
                        .h(viewport.height)
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.close_sort_options(cx);
                            }),
                        ),
                ),
            )
            .with_priority(1);
            let panel = crate::motion::appear(
                div().child(self.sort_options_panel(cx)),
                "sort-options-appear",
                0.,
                -crate::motion::NUDGE,
            );
            root = root.child(scrim).child(Self::view_options_flyout(panel));
        } else if let Some(ghost) = &self.closing_sort_options {
            let panel = crate::motion::leave(
                div().child(self.sort_options_panel(cx)),
                ghost.id("sort-options-leave"),
                0.,
                -crate::motion::NUDGE,
            );
            root = root.child(Self::view_options_flyout(panel));
        }
        root.into_any_element()
    }

    fn sort_options_panel(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let sort = self.sort;
        let columns: Vec<AnyElement> = SORT_COLUMNS
            .into_iter()
            .map(|column| {
                choice_row(
                    ("sort-by", column as usize).into(),
                    format!("sort-by-{}", column_key(column)),
                    column_label(column),
                    sort.column == column,
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.pick_sort_column(column, cx);
                    }),
                )
            })
            .collect();
        let directions: Vec<AnyElement> = [true, false]
            .into_iter()
            .map(|ascending| {
                let (key, label) = if ascending {
                    ("ascending", t!("settings.ascending"))
                } else {
                    ("descending", t!("settings.descending"))
                };
                choice_row(
                    ("sort-order", usize::from(ascending)).into(),
                    format!("sort-{key}"),
                    label,
                    sort.ascending == ascending,
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.pick_sort_direction(ascending, cx);
                    }),
                )
            })
            .collect();
        div()
            .id("sort-options-panel")
            .a11y(gpui::Role::Dialog, t!("view.sort"))
            .occlude()
            .w(panel_w())
            .p(theme::s_8())
            .flex()
            .flex_col()
            .gap(theme::s_2())
            .rounded(theme::r_12())
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .text_size(theme::fs_14())
            .text_color(theme::text())
            .child(super::viewopts::section_label(t!("settings.sort_by")))
            .children(columns)
            .child(div().my(theme::s_4()).h(px(1.)).bg(theme::line()))
            .children(directions)
    }
}

/// One choice in the flyout: a check when it is the current one, then its name.
fn choice_row(
    id: ElementId,
    selector: String,
    label: Cow<'static, str>,
    checked: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let label = label.into_owned();
    div()
        .id(id)
        .debug_selector(move || selector)
        .a11y(gpui::Role::RadioButton, label.clone())
        .aria_toggled(crate::a11y::toggled(checked))
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_8())
        .h(theme::h_ctl())
        .px(theme::s_8())
        .rounded(theme::r_4())
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash()))
        .child(
            svg()
                .path(Icon::Check.path())
                .flex_none()
                .size(theme::icon())
                .text_color(if checked {
                    theme::signal()
                } else {
                    theme::clear()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(checked, |d| d.font_weight(theme::FW_STRONG))
                .child(label),
        )
        .on_click(on_click)
        .into_any_element()
}

#[cfg(test)]
impl Explorer {
    pub(crate) const fn sort_options_open_for_test(&self) -> bool {
        self.sort_options_open
    }
}
