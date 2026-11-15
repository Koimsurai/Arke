//! The collapsed sidebar (#91): a narrow column of icons, toggled with Ctrl+B or the button at its
//! top. Home, the quick access folders, the vertical tabs and Settings stay one click away, with
//! their names on hover; the long lists (pins, This PC, Network) wait for the full sidebar.

use std::path::Path;

use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, MouseButton, MouseDownEvent, Window, div,
    prelude::*, px, svg,
};
use rust_i18n::t;

use super::{OpenPalette, ToggleSidebar, Workspace};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::kind_icon;
use crate::settings::SettingsStore;
use crate::theme;
use arke_core::filetype::FileKind;

/// Side of a rail button.
const BUTTON: f32 = 36.;

/// One square button of the rail: an icon, its name on hover.
fn rail_button(
    id: impl Into<ElementId>,
    icon: AnyElement,
    label: &str,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_button(label.to_owned())
        .aria_selected(selected)
        .relative()
        .size(px(BUTTON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(selected, |d| d.bg(theme::selection()))
        .when(!selected, |d| d.hover(|s| s.bg(theme::wash())))
        .child(icon)
        .tooltip(super::sidebar::tooltip_text(label.to_owned()))
}

fn icon(icon: Icon, selected: bool) -> AnyElement {
    svg()
        .path(icon.path())
        .size(theme::ICON)
        .flex_none()
        .text_color(if selected {
            theme::text()
        } else {
            theme::text_muted()
        })
        .into_any_element()
}

impl Workspace {
    /// Ctrl+B or the sidebar button: the setting is kept, so the next window starts the same way.
    #[allow(
        clippy::unused_self,
        reason = "GPUI action listeners need the &mut self signature"
    )]
    pub(super) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.sidebar_collapsed = !st.sidebar_collapsed, cx);
        });
    }

    /// The button that collapses or expands the sidebar.
    pub(super) fn sidebar_toggle(cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let label = if theme::sidebar_collapsed() {
            t!("sidebar.expand")
        } else {
            t!("sidebar.collapse")
        }
        .into_owned();
        div()
            .id("sidebar-toggle")
            .a11y_button(label.clone())
            .aria_keyshortcuts("Ctrl+B")
            .size(theme::H_CTL)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::R_8)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash_strong()))
            .child(icon(Icon::Sidebar, false))
            .tooltip(super::sidebar::tooltip_text(format!("{label} (Ctrl+B)")))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_sidebar(&ToggleSidebar, window, cx);
            }))
    }

    /// The collapsed sidebar.
    pub(super) fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let horizontal = Self::horizontal_tabs(cx);
        let tiles = self.rail_tiles(cx);
        let home_open = self.home_open();
        let settings_open = self.settings_open();
        let tabs: Vec<AnyElement> = if horizontal {
            Vec::new()
        } else {
            (0..self.tabs.len())
                .map(|ix| self.rail_tab(ix, cx))
                .collect()
        };
        div()
            .id("sidebar-rail")
            .debug_selector(|| "sidebar-rail".to_owned())
            .a11y(gpui::Role::Navigation, t!("a11y.sidebar"))
            .w(theme::RAIL_W)
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(theme::S_2)
            .pb(theme::S_8)
            // Vertical tabs: the toggle sits where the title row is; with tabs on top it is at the
            // start of the tab strip.
            .when(!horizontal, |d| {
                d.child(
                    div()
                        .h(theme::H_BAR)
                        .flex_none()
                        .flex()
                        .items_center()
                        .child(Self::sidebar_toggle(cx)),
                )
            })
            // Back and forward, which the full sidebar has in its title row.
            .when(!horizontal, |d| d.child(self.rail_nav(cx)))
            .when(horizontal, |d| d.pt(theme::S_4))
            .child(
                rail_button(
                    "rail-home",
                    icon(Icon::KfHome, home_open),
                    &t!("home.title"),
                    home_open,
                )
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    if home_open {
                        this.close_home(window, cx);
                    } else {
                        this.open_home(e.modifiers().control, window, cx);
                    }
                })),
            )
            .children(tiles)
            .when(!horizontal, |d| {
                d.child(
                    div()
                        .flex_none()
                        .w(px(24.))
                        .h(px(1.))
                        .my(theme::S_4)
                        .bg(theme::line()),
                )
                .child(
                    div()
                        .id("rail-tabs")
                        .a11y(gpui::Role::TabList, t!("sidebar.tabs"))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(theme::S_2)
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(tabs),
                )
                .child(
                    rail_button("rail-new-tab", icon(Icon::Plus, false), &t!("sidebar.new_tab"), false)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_palette(&OpenPalette, window, cx);
                        })),
                )
            })
            .child(div().flex_1().min_h(theme::S_8))
            .child(
                rail_button(
                    "rail-settings",
                    icon(Icon::Settings, settings_open),
                    &t!("settings.title"),
                    settings_open,
                )
                // A dot for a new version, where the full sidebar shows "Update available".
                .children(update_dot(cx))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    if settings_open {
                        this.close_settings(&super::settings_panel::CloseSettings, window, cx);
                    } else {
                        this.open_settings(&super::settings_panel::OpenSettings, window, cx);
                    }
                })),
            )
    }

    /// The quick access folders, the current one lit.
    fn rail_tiles(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let page_open = self.page_shown();
        let current = self
            .active_view()
            .read(cx)
            .display_path()
            .map(Path::to_path_buf);
        self.favorites(cx)
            .into_iter()
            .enumerate()
            .map(|(ix, (path, label, kf_icon))| {
                let here = !page_open && current.as_deref() == Some(path.as_path());
                let tint = crate::assets::tinted(
                    kf_icon,
                    theme::ICON,
                    if here {
                        theme::signal()
                    } else {
                        theme::text_muted()
                    },
                );
                let (open, background) = (path.clone(), path);
                rail_button(("rail-tile", ix), tint.into_any_element(), &label, here)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.navigate_active(open.clone(), window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            this.open_in_background(background.clone(), window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Back and forward for the active tab, dimmed when there is nowhere to go.
    fn rail_nav(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        use crate::explorer::{GoBack, GoForward};
        let (can_back, can_forward, _) = self.active_view().read(cx).nav_state();
        let button = |id: &'static str, glyph: Icon, label: String, enabled: bool| {
            div()
                .id(id)
                .a11y_button(label.clone())
                .a11y_disabled(!enabled)
                .size(px(BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::R_8)
                .when(enabled, |d| {
                    d.cursor_pointer().hover(|s| s.bg(theme::wash()))
                })
                .child(
                    svg()
                        .path(glyph.path())
                        .size(theme::ICON)
                        .text_color(if enabled {
                            theme::text_muted()
                        } else {
                            theme::text_disabled()
                        }),
                )
                .tooltip(super::sidebar::tooltip_text(label))
        };
        let view = self.active_view().clone();
        let forward_view = view.clone();
        div()
            .flex()
            .flex_col()
            .gap(theme::S_2)
            .child(
                button(
                    "rail-back",
                    Icon::ArrowLeft,
                    t!("nav.back").into_owned(),
                    can_back,
                )
                .on_click(move |_, window, cx| {
                    view.update(cx, |e, cx| e.go_back(&GoBack, window, cx));
                }),
            )
            .child(
                button(
                    "rail-forward",
                    Icon::ArrowRight,
                    t!("nav.forward").into_owned(),
                    can_forward,
                )
                .on_click(move |_, window, cx| {
                    forward_view.update(cx, |e, cx| e.go_forward(&GoForward, window, cx));
                }),
            )
    }

    /// One vertical tab in the rail: its folder or page icon, its name on hover.
    fn rail_tab(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[ix];
        let active = ix == self.active;
        let (title, icon_el) = match self.page_title(tab.view().entity_id()) {
            Some((title, page_icon)) => (title, icon(page_icon, active)),
            None => (
                tab.view().read(cx).tab_title().to_string(),
                kind_icon(FileKind::Folder, theme::ICON),
            ),
        };
        rail_button(("rail-tab", ix), icon_el, &title, active)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.activate(ix, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.close_tab(ix, window, cx);
                }),
            )
            .into_any_element()
    }
}

/// The new-version dot on the Settings button.
fn update_dot(cx: &App) -> Option<AnyElement> {
    crate::update::UpdateState::has_news(cx).then(|| {
        div()
            .absolute()
            .top(px(6.))
            .right(px(6.))
            .size(px(7.))
            .rounded(theme::R_FULL)
            .bg(theme::signal())
            .into_any_element()
    })
}
