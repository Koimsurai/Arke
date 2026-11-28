//! View options flyout, opened from the chevron at the end of the view switcher (in split panes,
//! where the switcher is hidden, from a button showing the current view). Like the Files app's
//! layout flyout, it gathers everything about how the folder is shown:
//!
//! - the layout (Details, Icons, Columns);
//! - Icons: the icon size slider, any size from small to 256 px (`explorer_icons.rs`);
//!   Details: row density (not in split panes, whose rows are tighter unless compact);
//! - switches for grouping, the Details tree, hidden items and file name extensions. They are the
//!   same settings as in Settings, so every tab follows.
//!
//! Clicking outside or Esc closes it. Opened with the mouse, nothing here takes focus away from the
//! list, so the keyboard keeps working while it is open; opened from the keyboard or by a screen
//! reader, it takes the keyboard itself, as `explorer_toolbar.rs` says (#322).

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, MouseButton, MouseDownEvent, Pixels, Window, anchored,
    deferred, div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use super::toolbar::{Choice, Mark, ToolbarStop as _, ViewSwitch};
use super::{Explorer, IconSize, ViewColumns, ViewDetails, ViewMode};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::settings::{Density, Settings};
use crate::theme;
use crate::workspace::TooltipBelowExt as _;

fn panel_w() -> gpui::Pixels {
    crate::theme::scaled_px(300.)
}

/// Height of a segment in the view switcher (shared with its other segments).
pub(super) fn segment_h() -> gpui::Pixels {
    crate::theme::scaled_px(22.)
}

/// The densities offered, in order.
const DENSITIES: [Density; 3] = [Density::Compact, Density::Standard, Density::Comfortable];

impl Explorer {
    /// Opens or closes the flyout from the keyboard or a screen reader: open, it takes the focus
    /// on the current layout.
    fn toggle_view_options_by_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_view_options(cx);
        let at = match self.view_mode {
            ViewMode::Details => 0,
            ViewMode::Icons => 1,
            ViewMode::Columns => 2,
        };
        self.flyout_by_key(self.view_options_open, at, "view-options", window, cx);
    }

    /// The flyout's choices in order: the layouts, then the icon size (Icons) or the row density
    /// (Details, not in split panes, whose rows are tighter unless compact), then the switches.
    pub(super) fn view_choices(&self) -> Vec<Choice> {
        let mode = self.view_mode;
        let mut choices: Vec<Choice> = [ViewMode::Details, ViewMode::Icons, ViewMode::Columns]
            .into_iter()
            .map(Choice::Layout)
            .collect();
        match mode {
            ViewMode::Icons => choices.push(Choice::IconSize),
            ViewMode::Details if !self.pane.split => {
                choices.extend(DENSITIES.map(Choice::Density));
            }
            ViewMode::Details | ViewMode::Columns => {}
        }
        if mode != ViewMode::Columns {
            choices.push(Choice::Switch(ViewSwitch::GroupRows));
        }
        if mode == ViewMode::Details {
            choices.push(Choice::Switch(ViewSwitch::DetailsTree));
        }
        choices.extend([
            Choice::Switch(ViewSwitch::ShowHidden),
            Choice::Switch(ViewSwitch::ShowExtensions),
        ]);
        choices
    }

    /// Opens or closes the flyout.
    pub(super) fn toggle_view_options(&mut self, cx: &mut Context<Self>) {
        if !self.close_view_options(cx) {
            // One flyout at a time.
            self.close_sort_options(cx);
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
            .size(theme::icon_sm())
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
            .a11y_open_close(open, cx.entity().downgrade(), |this, window, cx| {
                this.toggle_view_options_by_key(window, cx);
            })
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(theme::s_2())
            .cursor_pointer()
            .when(in_switcher, |d| {
                d.w(theme::s_20()).h(segment_h()).rounded(theme::r_4())
            })
            .when(!in_switcher, |d| {
                d.h(theme::h_ctl_sm())
                    .px(theme::s_6())
                    .rounded(theme::r_8())
                    .child(
                        svg()
                            .path(view_icon(self.view_mode).path())
                            .size(theme::icon())
                            .text_color(theme::text_subtle()),
                    )
            })
            .when(open, |d| d.bg(theme::wash_strong()))
            .when(!open, |d| d.hover(|s| s.bg(theme::wash())))
            .child(chevron)
            // Opens on press, like a menu; the scrim closes it again.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.toggle_view_options(cx);
                    this.flyout_return("view-options", window, cx);
                }),
            )
            .toolbar_menu("view-options", {
                let this = cx.entity().downgrade();
                move |window, cx| {
                    let _ = this.update(cx, |this, cx| this.toggle_view_options_by_key(window, cx));
                }
            })
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
                            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.close_view_options(cx);
                                this.flyout_return("view-options", window, cx);
                            }),
                        ),
                ),
            )
            .with_priority(1);
            let panel = crate::motion::appear(
                div().child(self.view_options_panel(window, cx)),
                "view-options-appear",
                0.,
                -crate::motion::NUDGE,
            );
            root = root.child(scrim).child(Self::view_options_flyout(panel));
        } else if let Some(ghost) = &self.closing_view_options {
            let panel = crate::motion::leave(
                div().child(self.view_options_panel(window, cx)),
                ghost.id("view-options-leave"),
                0.,
                -crate::motion::NUDGE,
            );
            root = root.child(Self::view_options_flyout(panel));
        }
        root.into_any_element()
    }

    /// The flyout's place under its button, right-aligned and kept inside the window (the sort
    /// flyout's too).
    pub(super) fn view_options_flyout(panel: AnyElement) -> impl IntoElement + use<> {
        div()
            .absolute()
            .top(theme::h_ctl_sm() + theme::s_6())
            .right_0()
            .child(
                deferred(
                    anchored()
                        .anchor(gpui::Anchor::TopRight)
                        .snap_to_window_with_margin(theme::s_8())
                        .child(panel),
                )
                .with_priority(2),
            )
    }

    fn view_options_panel(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mode = self.view_mode;
        let settings = crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .clone();
        let choices = self.view_choices();
        let mark =
            |choice: Choice| self.flyout_mark(choices.iter().position(|c| *c == choice), window);
        let layouts = div()
            .flex()
            .gap(theme::s_2())
            .p(theme::s_2())
            .rounded(theme::r_8())
            .bg(theme::segment_track())
            .child(layout_tile(
                "vo-details",
                (ViewMode::Details, mode),
                t!("menu.view_details"),
                mark(Choice::Layout(ViewMode::Details)),
                cx.listener(|this, _: &ClickEvent, w, cx| this.view_details(&ViewDetails, w, cx)),
            ))
            .child(layout_tile(
                "vo-icons",
                (ViewMode::Icons, mode),
                t!("view.icons"),
                mark(Choice::Layout(ViewMode::Icons)),
                cx.listener(|this, _: &ClickEvent, _, cx| this.show_icons(cx)),
            ))
            .child(layout_tile(
                "vo-columns",
                (ViewMode::Columns, mode),
                t!("settings.view_columns"),
                mark(Choice::Layout(ViewMode::Columns)),
                cx.listener(|this, _: &ClickEvent, w, cx| this.view_columns(&ViewColumns, w, cx)),
            ));
        let size_block = if choices.contains(&Choice::IconSize) {
            Some(self.icon_size_block(mark(Choice::IconSize), cx))
        } else if choices.contains(&Choice::Density(Density::Standard)) {
            Some(density_block(
                settings.density,
                DENSITIES.map(|d| mark(Choice::Density(d))),
            ))
        } else {
            None
        };
        let toggles: Vec<AnyElement> = choices
            .iter()
            .filter_map(|&choice| match choice {
                Choice::Switch(switch) => Some(toggle_row(switch, &settings, mark(choice))),
                _ => None,
            })
            .collect();
        div()
            .id("view-options-panel")
            .a11y(gpui::Role::Dialog, t!("view.options"))
            .map(|d| self.flyout_keys(self.view_options_open, d, window, cx))
            .occlude()
            .w(panel_w())
            .p(theme::s_8())
            .flex()
            .flex_col()
            .gap(theme::s_4())
            .rounded(theme::r_12())
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .text_size(theme::fs_14())
            .text_color(theme::text())
            .child(section_label(t!("view.layout")))
            .child(layouts)
            .children(size_block)
            .child(div().my(theme::s_4()).h(px(1.)).bg(theme::line()))
            .children(toggles)
    }

    /// Icons: label with the current size, and the slider between a small and a large grid glyph.
    /// `(claim, ring)`: the keyboard is on the slider.
    fn icon_size_block(&self, (claim, ring): Mark, cx: &mut Context<Self>) -> AnyElement {
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
            .pt(theme::s_8())
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(theme::s_8())
                    .child(div().flex_1().child(t!("view.icon_size").into_owned()))
                    .child(
                        div()
                            .text_size(theme::fs_12())
                            .text_color(theme::text_muted())
                            .child(self.icon_size.label().into_owned()),
                    ),
            )
            .child(
                div()
                    .id("vo-icon-size")
                    .a11y(gpui::Role::Slider, t!("view.icon_size"))
                    .when(crate::a11y::active(), |d| {
                        d.aria_value(self.icon_size.label().into_owned())
                    })
                    .when(
                        claim,
                        gpui::StatefulInteractiveElement::aria_active_descendant,
                    )
                    .when(ring, |d| {
                        d.rounded(theme::r_4()).shadow(crate::keynav::ring())
                    })
                    .flex()
                    .items_center()
                    .gap(theme::s_8())
                    .px(theme::s_8())
                    .child(glyph(theme::icon_sm()))
                    .child(
                        crate::slider::slider(
                            self.icon_size.fraction(),
                            Rc::clone(&self.icon_slider_drag),
                            on_slide,
                        )
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(glyph(theme::icon())),
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

pub(super) fn section_label(text: impl Into<String>) -> impl IntoElement {
    div()
        .px(theme::s_8())
        .pt(theme::s_4())
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .child(text.into())
}

/// One layout in the flyout: icon over its name, the current one raised like the switcher's thumb.
/// `(claim, ring)`: the keyboard is on it.
fn layout_tile(
    id: &'static str,
    (mode, current): (ViewMode, ViewMode),
    label: impl Into<String>,
    (claim, ring): Mark,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let active = mode == current;
    let label: String = label.into();
    div()
        .id(id)
        .debug_selector(|| id.to_owned())
        .a11y(gpui::Role::RadioButton, label.clone())
        .aria_toggled(crate::a11y::toggled(active))
        .when(
            claim,
            gpui::StatefulInteractiveElement::aria_active_descendant,
        )
        .flex_1()
        .min_w_0()
        .h(theme::s_32() + theme::s_20())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme::s_4())
        .rounded(theme::r_4())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .when(active, |d| {
            d.bg(theme::segment_thumb())
                .shadow(theme::elev_thumb())
                .text_color(theme::text())
        })
        .when(!active, |d| {
            d.text_color(theme::text_muted())
                .hover(|s| s.bg(theme::wash()))
        })
        .when(ring, |d| d.shadow(crate::keynav::ring()))
        .on_click(on_click)
        .child(
            svg()
                .path(view_icon(mode).path())
                .size(theme::icon())
                .text_color(if active {
                    theme::text()
                } else {
                    theme::text_subtle()
                }),
        )
        .child(div().max_w_full().truncate().child(label))
}

/// Details: row density as three text segments (the same setting as in Settings). `marks`: where
/// the keyboard is, as [`DENSITIES`].
fn density_block(density: Density, marks: [Mark; 3]) -> AnyElement {
    let option = |value: Density, label: std::borrow::Cow<'static, str>| {
        let active = value == density;
        let (claim, ring) = DENSITIES
            .iter()
            .position(|d| *d == value)
            .map_or((false, false), |ix| marks[ix]);
        let label = label.into_owned();
        div()
            .id(("vo-density", value as usize))
            .a11y(gpui::Role::RadioButton, label.clone())
            .aria_toggled(crate::a11y::toggled(active))
            .when(
                claim,
                gpui::StatefulInteractiveElement::aria_active_descendant,
            )
            .flex_1()
            .min_w_0()
            .h(segment_h())
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::r_4())
            .cursor_pointer()
            .text_size(theme::fs_12())
            .when(active, |d| {
                d.bg(theme::segment_thumb())
                    .shadow(theme::elev_thumb())
                    .text_color(theme::text())
            })
            .when(!active, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()))
            })
            .when(ring, |d| d.shadow(crate::keynav::ring()))
            .child(div().truncate().child(label))
            .on_click(move |_, _, cx| set(cx, |st| st.density = value))
    };
    div()
        .flex()
        .flex_col()
        .gap(theme::s_6())
        .pt(theme::s_8())
        .child(
            div()
                .px(theme::s_8())
                .child(t!("view.density").into_owned()),
        )
        .child(
            div()
                .mx(theme::s_8())
                .flex()
                .gap(theme::s_2())
                .p(theme::s_2())
                .rounded(theme::r_8())
                .bg(theme::segment_track())
                .child(option(Density::Compact, t!("density.compact")))
                .child(option(Density::Standard, t!("density.standard")))
                .child(option(Density::Comfortable, t!("density.comfortable"))),
        )
        .into_any_element()
}

/// A switch row: label (and a one-line description), an optional shortcut, the switch.
/// `(claim, ring)`: the keyboard is on the switch.
fn toggle_row(switch: ViewSwitch, settings: &Settings, (claim, ring): Mark) -> AnyElement {
    let (id, label, desc, shortcut) = match switch {
        ViewSwitch::GroupRows => ("vo-group-rows", t!("settings.group_rows"), None, None),
        ViewSwitch::DetailsTree => (
            "vo-details-tree",
            t!("settings.details_tree"),
            Some(t!("view.tree_desc")),
            None,
        ),
        ViewSwitch::ShowHidden => (
            "vo-show-hidden",
            t!("settings.show_hidden"),
            None,
            Some("Ctrl+H"),
        ),
        ViewSwitch::ShowExtensions => (
            "vo-show-extensions",
            t!("settings.show_extensions"),
            None,
            None,
        ),
    };
    let on = switch.on(settings);
    // The switch is named after the row for screen readers (#322).
    let a11y = crate::a11y::active();
    let spoken = desc
        .as_ref()
        .map(|d| gpui::SharedString::from(d.to_string()));
    div()
        .flex()
        .items_center()
        .gap(theme::s_8())
        .min_h(theme::h_ctl())
        .px(theme::s_8())
        .py(theme::s_4())
        .rounded(theme::r_4())
        .when(ring, |d| d.shadow(crate::keynav::ring()))
        .when(a11y, |d| {
            d.child(crate::a11y::described_scope(
                gpui::SharedString::from(label.to_string()),
                spoken,
            ))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(label.into_owned())
                .children(desc.map(|desc| {
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(desc.into_owned())
                })),
        )
        .children(shortcut.map(|keys| {
            div()
                .flex_none()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(keys)
        }))
        .child(div().flex_none().child(crate::workspace::flyout_switch(
            id,
            on,
            move |s| switch.flip(s),
            claim,
        )))
        .when(a11y, |d| d.child(crate::a11y::end_label_scope()))
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
