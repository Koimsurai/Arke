//! View options flyout, opened from the chevron at the end of the view switcher (in split panes,
//! where the switcher is hidden, from a button showing the current view). Like the Files app's
//! layout flyout, it gathers everything about how the folder is shown:
//!
//! - the layout (Details, Icons, Columns);
//! - Icons: the icon size slider, any size from small to extra large (`explorer_icons.rs`);
//!   Details: row density (not in split panes, which always use compact rows);
//! - switches for grouping, the Details tree, hidden items and file name extensions. They are the
//!   same settings as in Settings, so every tab follows.
//!
//! Clicking outside or Esc closes it. Nothing here takes focus away from the list, so the keyboard
//! keeps working while it is open.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, MouseButton, MouseDownEvent, Pixels, Window, anchored,
    deferred, div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{Explorer, IconSize, ViewColumns, ViewDetails, ViewMode};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::settings::{Density, Settings};
use crate::theme;
use crate::workspace::TooltipBelowExt as _;

const PANEL_W: Pixels = px(300.);

/// Height of a segment in the view switcher (shared with its other segments).
pub(super) const SEGMENT_H: Pixels = px(22.);

impl Explorer {
    /// Opens or closes the flyout.
    pub(super) fn toggle_view_options(&mut self, cx: &mut Context<Self>) {
        if !self.close_view_options(cx) {
            self.view_options_open = true;
            self.closing_view_options = None;
            cx.notify();
        }
    }

    /// Closes the flyout (it fades out); returns whether it was open (Esc).
    pub(super) fn close_view_options(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.view_options_open) {
            return false;
        }
        crate::motion::Leaving::start(self, (), |this| &mut this.closing_view_options, cx);
        cx.notify();
        true
    }

    /// The flyout's button with the open flyout under it. `in_switcher`: the chevron segment at
    /// the end of the view switcher; otherwise a standalone button with the current view's icon.
    pub(super) fn render_view_options(
        &self,
        in_switcher: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.view_options_open;
        let chevron = svg()
            .path(Icon::ChevronRight.path())
            .size(theme::ICON_SM)
            .text_color(if open {
                theme::text()
            } else {
                theme::text_subtle()
            })
            .with_transformation(gpui::Transformation::rotate(gpui::radians(
                std::f32::consts::FRAC_PI_2,
            )));
        let button = div()
            .id("view-options")
            .debug_selector(|| "view-options".to_owned())
            .a11y_button(t!("view.options"))
            .aria_expanded(open)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(theme::S_2)
            .cursor_pointer()
            .when(in_switcher, |d| {
                d.w(theme::S_20).h(SEGMENT_H).rounded(theme::R_4)
            })
            .when(!in_switcher, |d| {
                d.h(theme::H_CTL_SM)
                    .px(theme::S_6)
                    .rounded(theme::R_8)
                    .child(
                        svg()
                            .path(view_icon(self.view_mode).path())
                            .size(theme::ICON)
                            .text_color(theme::text_subtle()),
                    )
            })
            .when(open, |d| d.bg(theme::wash_strong()))
            .when(!open, |d| d.hover(|s| s.bg(theme::wash())))
            .child(chevron)
            // Opens on press, like a menu; the scrim closes it again.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle_view_options(cx);
                }),
            )
            .when(!open, |d| d.tooltip_below(t!("view.options")));
        let mut root = div().relative().flex_none().child(button);
        if open {
            let viewport = window.viewport_size();
            let scrim = deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .id("view-options-scrim")
                        .w(viewport.width)
                        .h(viewport.height)
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.close_view_options(cx);
                            }),
                        ),
                ),
            )
            .with_priority(1);
            let panel = crate::motion::appear(
                div().child(self.view_options_panel(cx)),
                "view-options-appear",
                0.,
                -crate::motion::NUDGE,
            );
            root = root.child(scrim).child(Self::view_options_flyout(panel));
        } else if let Some(ghost) = &self.closing_view_options {
            let panel = crate::motion::leave(
                div().child(self.view_options_panel(cx)),
                ghost.id("view-options-leave"),
                0.,
                -crate::motion::NUDGE,
            );
            root = root.child(Self::view_options_flyout(panel));
        }
        root.into_any_element()
    }

    /// The flyout's place under its button, right-aligned and kept inside the window.
    fn view_options_flyout(panel: AnyElement) -> impl IntoElement + use<> {
        div()
            .absolute()
            .top(theme::H_CTL_SM + theme::S_6)
            .right_0()
            .child(
                deferred(
                    anchored()
                        .anchor(gpui::Anchor::TopRight)
                        .snap_to_window_with_margin(theme::S_8)
                        .child(panel),
                )
                .with_priority(2),
            )
    }

    fn view_options_panel(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mode = self.view_mode;
        let settings = crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .clone();
        let layouts = div()
            .flex()
            .gap(theme::S_2)
            .p(theme::S_2)
            .rounded(theme::R_8)
            .bg(theme::segment_track())
            .child(layout_tile(
                "vo-details",
                ViewMode::Details,
                t!("menu.view_details"),
                mode,
                cx.listener(|this, _: &ClickEvent, w, cx| this.view_details(&ViewDetails, w, cx)),
            ))
            .child(layout_tile(
                "vo-icons",
                ViewMode::Icons,
                t!("view.icons"),
                mode,
                cx.listener(|this, _: &ClickEvent, _, cx| this.show_icons(cx)),
            ))
            .child(layout_tile(
                "vo-columns",
                ViewMode::Columns,
                t!("settings.view_columns"),
                mode,
                cx.listener(|this, _: &ClickEvent, w, cx| this.view_columns(&ViewColumns, w, cx)),
            ));
        let size_block = match mode {
            ViewMode::Icons => Some(self.icon_size_block(cx)),
            // Split panes always use compact rows.
            ViewMode::Details if !self.pane.split => Some(density_block(settings.density)),
            ViewMode::Details | ViewMode::Columns => None,
        };
        let mut toggles: Vec<AnyElement> = Vec::new();
        if mode != ViewMode::Columns {
            toggles.push(toggle_row(
                "vo-group-rows",
                t!("settings.group_rows"),
                None,
                None,
                settings.group_rows,
                |st| st.group_rows = !st.group_rows,
            ));
        }
        if mode == ViewMode::Details {
            toggles.push(toggle_row(
                "vo-details-tree",
                t!("settings.details_tree"),
                Some(t!("view.tree_desc")),
                None,
                settings.details_tree,
                |st| st.details_tree = !st.details_tree,
            ));
        }
        toggles.push(toggle_row(
            "vo-show-hidden",
            t!("settings.show_hidden"),
            None,
            Some("Ctrl+H"),
            settings.show_hidden,
            |st| st.show_hidden = !st.show_hidden,
        ));
        toggles.push(toggle_row(
            "vo-show-extensions",
            t!("settings.show_extensions"),
            None,
            None,
            settings.show_extensions,
            |st| st.show_extensions = !st.show_extensions,
        ));
        div()
            .id("view-options-panel")
            .a11y(gpui::Role::Dialog, t!("view.options"))
            .occlude()
            .w(PANEL_W)
            .p(theme::S_8)
            .flex()
            .flex_col()
            .gap(theme::S_4)
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .child(section_label(t!("view.layout")))
            .child(layouts)
            .children(size_block)
            .child(div().my(theme::S_4).h(px(1.)).bg(theme::line()))
            .children(toggles)
    }

    /// Icons: label with the current size, and the slider between a small and a large grid glyph.
    fn icon_size_block(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let on_slide: crate::slider::OnSlide = Rc::new(move |fraction, _, cx: &mut App| {
            let size = IconSize::from_fraction(fraction);
            let _ = this.update(cx, |this, cx| this.pick_icon_size(size, cx));
        });
        let glyph = |size: Pixels| {
            svg()
                .path(Icon::ViewGrid.path())
                .size(size)
                .flex_none()
                .text_color(theme::text_subtle())
        };
        div()
            .flex()
            .flex_col()
            .pt(theme::S_8)
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(theme::S_8)
                    .child(div().flex_1().child(t!("view.icon_size").into_owned()))
                    .child(
                        div()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_muted())
                            .child(self.icon_size.label().into_owned()),
                    ),
            )
            .child(
                div()
                    .id("vo-icon-size")
                    .a11y(gpui::Role::Slider, t!("view.icon_size"))
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .px(theme::S_8)
                    .child(glyph(theme::ICON_SM))
                    .child(
                        crate::slider::slider(
                            self.icon_size.fraction(),
                            self.icon_slider_drag.clone(),
                            on_slide,
                        )
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(glyph(theme::ICON)),
            )
            .into_any_element()
    }
}

const fn view_icon(mode: ViewMode) -> Icon {
    match mode {
        ViewMode::Details => Icon::ViewList,
        ViewMode::Icons => Icon::ViewGrid,
        ViewMode::Columns => Icon::ViewColumns,
    }
}

fn section_label(text: impl Into<String>) -> impl IntoElement {
    div()
        .px(theme::S_8)
        .pt(theme::S_4)
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        .child(text.into())
}

/// One layout in the flyout: icon over its name, the current one raised like the switcher's thumb.
fn layout_tile(
    id: &'static str,
    mode: ViewMode,
    label: impl Into<String>,
    current: ViewMode,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let active = mode == current;
    let label: String = label.into();
    div()
        .id(id)
        .debug_selector(|| id.to_owned())
        .a11y(gpui::Role::RadioButton, label.clone())
        .aria_toggled(crate::a11y::toggled(active))
        .flex_1()
        .min_w_0()
        .h(theme::S_32 + theme::S_20)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme::S_4)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .when(active, |d| {
            d.bg(theme::segment_thumb())
                .shadow(theme::elev_thumb())
                .text_color(theme::text())
        })
        .when(!active, |d| {
            d.text_color(theme::text_muted())
                .hover(|s| s.bg(theme::wash()))
        })
        .on_click(on_click)
        .child(
            svg()
                .path(view_icon(mode).path())
                .size(theme::ICON)
                .text_color(if active {
                    theme::text()
                } else {
                    theme::text_subtle()
                }),
        )
        .child(div().max_w_full().truncate().child(label))
}

/// Details: row density as three text segments (the same setting as in Settings).
fn density_block(density: Density) -> AnyElement {
    let option = |value: Density, label: std::borrow::Cow<'static, str>| {
        let active = value == density;
        let label = label.into_owned();
        div()
            .id(("vo-density", value as usize))
            .a11y(gpui::Role::RadioButton, label.clone())
            .aria_toggled(crate::a11y::toggled(active))
            .flex_1()
            .min_w_0()
            .h(SEGMENT_H)
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::R_4)
            .cursor_pointer()
            .text_size(theme::FS_12)
            .when(active, |d| {
                d.bg(theme::segment_thumb())
                    .shadow(theme::elev_thumb())
                    .text_color(theme::text())
            })
            .when(!active, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()))
            })
            .child(div().truncate().child(label))
            .on_click(move |_, _, cx| set(cx, |st| st.density = value))
    };
    div()
        .flex()
        .flex_col()
        .gap(theme::S_6)
        .pt(theme::S_8)
        .child(div().px(theme::S_8).child(t!("view.density").into_owned()))
        .child(
            div()
                .mx(theme::S_8)
                .flex()
                .gap(theme::S_2)
                .p(theme::S_2)
                .rounded(theme::R_8)
                .bg(theme::segment_track())
                .child(option(Density::Compact, t!("density.compact")))
                .child(option(Density::Standard, t!("density.standard")))
                .child(option(Density::Comfortable, t!("density.comfortable"))),
        )
        .into_any_element()
}

/// A switch row: label (and a one-line description), an optional shortcut, the switch.
fn toggle_row(
    id: &'static str,
    label: std::borrow::Cow<'static, str>,
    desc: Option<std::borrow::Cow<'static, str>>,
    shortcut: Option<&'static str>,
    on: bool,
    toggle: impl Fn(&mut Settings) + 'static,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(theme::S_8)
        .min_h(theme::H_CTL)
        .px(theme::S_8)
        .py(theme::S_4)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(label.into_owned())
                .children(desc.map(|desc| {
                    div()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_subtle())
                        .child(desc.into_owned())
                })),
        )
        .children(shortcut.map(|keys| {
            div()
                .flex_none()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(keys)
        }))
        .child(
            div()
                .flex_none()
                .child(crate::workspace::switch(id, on, toggle)),
        )
        .into_any_element()
}

#[cfg(test)]
impl Explorer {
    pub(crate) const fn view_options_open_for_test(&self) -> bool {
        self.view_options_open
    }

    /// Moves the icon size slider to `fraction`, as a press or drag on it does.
    pub(crate) fn slide_icon_size_for_test(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.pick_icon_size(IconSize::from_fraction(fraction), cx);
    }
}

fn set(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    crate::settings::SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx));
}
