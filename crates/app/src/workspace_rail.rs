//! The collapsed sidebar (#91, #122): a narrow column of icons, toggled with Ctrl+B or the button at
//! its top. Home, the quick access folders, the pinned folders, the Recycle Bin and the Shelf, the
//! drives (as their letters), the vertical tabs and Settings stay one click away, with their names
//! on hover.
//!
//! Resting the pointer on the column opens the full sidebar over the content until the pointer
//! leaves it, like a browser's collapsed vertical tabs (right click the column to turn that off).
//! Clicking the Shelf opens it the same way, since its items need the full width.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, ExternalPaths, MouseButton, MouseDownEvent,
    MouseMoveEvent, Window, deferred, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::sidebar::{SidebarAction, display_name};
use super::{OpenPalette, ToggleSidebar, Workspace};

/// Sidebar state that isn't a setting.
#[derive(Default)]
pub(super) struct SidebarUi {
    /// Ctrl+B: the width sliding (collapsing?, serial); see `Workspace::slide_sidebar`.
    pub(super) motion: Option<(bool, u64)>,
    motion_task: Option<gpui::Task<()>>,
    /// The lists that scroll on their own: places (pins, This PC, Network) and tabs, in the full
    /// sidebar and in the column of icons.
    pub(super) places_scroll: gpui::ScrollHandle,
    pub(super) tabs_scroll: gpui::ScrollHandle,
    rail_places_scroll: gpui::ScrollHandle,
    rail_tabs_scroll: gpui::ScrollHandle,
    /// Tabs on top: the strip, which scrolls sideways when the tabs don't fit, and the tab last
    /// scrolled into view there (the active one, once each time it changes).
    pub(super) strip_scroll: gpui::ScrollHandle,
    pub(super) strip_revealed: std::cell::Cell<Option<(usize, gpui::Pixels)>>,
    /// The open-over-the-content sidebar just closed and fades out (serial, for the animation id).
    peek_ghost: Option<u64>,
    peek_ghost_task: Option<gpui::Task<()>>,
    /// The mapped drive or network place being renamed in place.
    pub(super) place_rename: Option<super::place_rename::PlaceRename>,
}
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::{DraggedFiles, kind_icon};
use crate::settings::SettingsStore;
use crate::shell::PlaceKind;
use crate::theme;
use arke_core::filetype::FileKind;

/// Side of a rail button.
const BUTTON: f32 = 36.;

/// Height of `n` buttons stacked with their gaps.
fn buttons_h(n: f32) -> gpui::Pixels {
    px(BUTTON) * n + theme::s_2() * (n - 1.).max(0.)
}
/// How long the pointer rests on the column before the full sidebar opens over the content (short
/// enough to feel immediate, long enough that crossing the column to the window edge doesn't).
const PEEK_OPEN: Duration = Duration::from_millis(300);
/// How long the pointer can be outside the open sidebar before it closes (a slip past its edge
/// doesn't close it).
const PEEK_CLOSE: Duration = Duration::from_millis(200);

/// One square button of the rail: an icon, its name on hover.
fn rail_button(
    id: impl Into<ElementId>,
    icon: AnyElement,
    label: &str,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector({
            let label = label.to_owned();
            move || format!("rail {label}")
        })
        .a11y_button(label.to_owned())
        .aria_selected(selected)
        .relative()
        .size(px(BUTTON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .when(selected, |d| d.bg(theme::selection()))
        .when(!selected, |d| d.hover(|s| s.bg(theme::wash())))
        .child(icon)
        .tooltip(super::sidebar::tooltip_text(label.to_owned()))
}

fn icon(icon: Icon, selected: bool) -> AnyElement {
    svg()
        .path(icon.path())
        .size(theme::icon())
        .flex_none()
        .text_color(if selected {
            theme::text()
        } else {
            theme::text_muted()
        })
        .into_any_element()
}

/// A thin line between groups of buttons.
fn divider() -> AnyElement {
    div()
        .flex_none()
        .w(crate::theme::scaled_px(24.))
        .h(px(1.))
        .my(theme::s_4())
        .bg(theme::line())
        .into_any_element()
}

/// "C:" for `C:\`; the root's own text otherwise.
fn drive_letter(root: &Path) -> String {
    let text = root.to_string_lossy();
    text.trim_end_matches('\\').to_owned()
}

impl Workspace {
    /// Ctrl+B or the sidebar button: the setting is kept, so the next window starts the same way.
    pub(super) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rail_peek = false;
        let collapsing = !theme::sidebar_collapsed();
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(|st| st.sidebar_collapsed = collapsing, cx);
        });
        self.slide_sidebar(collapsing, cx);
    }

    /// The sidebar's width slides between full and the column of icons for `motion::BASE`
    /// (instant with Windows animations off).
    fn slide_sidebar(&mut self, collapsing: bool, cx: &mut Context<Self>) {
        if !crate::motion::enabled() {
            self.sidebar_ui.motion = None;
            return;
        }
        let serial = self.sidebar_ui.motion.map_or(0, |(_, n)| n) + 1;
        self.sidebar_ui.motion = Some((collapsing, serial));
        self.sidebar_ui.motion_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(crate::motion::BASE).await;
            let _ = this.update(cx, |this, cx| {
                if this.sidebar_ui.motion.is_some_and(|(_, n)| n == serial) {
                    this.sidebar_ui.motion = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
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
            .size(theme::h_ctl())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(theme::r_8())
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
        let home_open = self.home_open();
        let settings_open = self.settings_open();
        let peek = SettingsStore::global(cx).read(cx).settings.sidebar_peek;
        let mut places = self.rail_tiles(cx);
        places.extend(self.rail_pins(cx));
        let drives = self.rail_drives(cx);
        if !drives.is_empty() {
            places.push(divider());
            places.extend(drives);
        }
        let tabs: Vec<AnyElement> = if horizontal {
            Vec::new()
        } else {
            (0..self.tabs.len())
                .map(|ix| self.rail_tab(ix, cx))
                .collect()
        };
        let menu = vec![(
            if peek {
                t!("sidebar.peek_off")
            } else {
                t!("sidebar.peek_on")
            }
            .into_owned()
            .into(),
            SidebarAction::SetSidebarPeek(!peek),
        )];
        div()
            .id("sidebar-rail")
            .debug_selector(|| "sidebar-rail".to_owned())
            .a11y(gpui::Role::Navigation, t!("a11y.sidebar"))
            .w(theme::rail_w())
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(theme::s_2())
            .pb(theme::s_8())
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.rail_hovered(*hovered, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.open_sidebar_menu(e.position, menu.clone(), cx);
                }),
            )
            // The toggle sits where the title row is.
            .child(
                div()
                    .h(theme::h_bar())
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(Self::sidebar_toggle(cx)),
            )
            // Back and forward, which the full sidebar has in its title row.
            .child(self.rail_nav(cx))
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
            // Places and tabs each scroll on their own when the window is short, like the full
            // sidebar.
            .child(super::sidebar::scrolling(
                div()
                    .id("rail-places")
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(theme::s_2()),
                places,
                &self.sidebar_ui.rail_places_scroll,
                div().w_full().flex_initial().min_h(buttons_h(2.)),
            ))
            .when(!horizontal, |d| {
                d.children(Self::rail_tab_list(
                    tabs,
                    &self.sidebar_ui.rail_tabs_scroll,
                    cx,
                ))
            })
            .child(div().flex_1().min_h(theme::s_8()))
            .child(Self::rail_settings(settings_open, cx))
    }

    /// Settings, with a dot for a new version where the full sidebar shows "Update available".
    fn rail_settings(settings_open: bool, cx: &mut Context<Self>) -> AnyElement {
        rail_button(
            "rail-settings",
            icon(Icon::Settings, settings_open),
            &t!("settings.title"),
            settings_open,
        )
        .children(update_dot(cx))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            if settings_open {
                this.close_settings(&super::settings_panel::CloseSettings, window, cx);
            } else {
                this.open_settings(&super::settings_panel::OpenSettings, window, cx);
            }
        }))
        .into_any_element()
    }

    /// The vertical tabs (scrolling on their own) and the New tab button.
    fn rail_tab_list(
        tabs: Vec<AnyElement>,
        scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> [AnyElement; 3] {
        // Up to two tabs stay in view however short the window gets; one tab keeps only its own
        // height, leaving the rest to the places above.
        let keep = tabs.len().clamp(1, 2) as f32;
        [
            divider(),
            super::sidebar::scrolling(
                div()
                    .id("rail-tabs")
                    .a11y(gpui::Role::TabList, t!("sidebar.tabs"))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(theme::s_2()),
                tabs,
                scroll,
                div()
                    .w_full()
                    .flex_initial()
                    .flex_shrink(super::sidebar::TABS_GIVE_WAY)
                    .min_h(buttons_h(keep)),
            )
            .into_any_element(),
            rail_button(
                "rail-new-tab",
                icon(Icon::Plus, false),
                &t!("sidebar.new_tab"),
                false,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_palette(&OpenPalette, window, cx);
            }))
            .into_any_element(),
        ]
    }

    /// The quick access folders, the current one lit.
    fn rail_tiles(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let current = self.rail_here(cx);
        self.favorites(cx)
            .into_iter()
            .enumerate()
            .map(|(ix, (path, label, kf_icon))| {
                let here = current.as_deref() == Some(path.as_path());
                let tint = crate::assets::tinted(
                    kf_icon,
                    theme::icon(),
                    if here {
                        theme::signal()
                    } else {
                        theme::text_muted()
                    },
                );
                Self::rail_folder(
                    ("rail-tile", ix),
                    tint.into_any_element(),
                    &label,
                    path,
                    here,
                    cx,
                )
            })
            .collect()
    }

    /// The folder the rail marks: none while a page covers the active tab (#98).
    fn rail_here(&self, cx: &App) -> Option<PathBuf> {
        if self.page_shown() {
            return None;
        }
        self.active_view()
            .read(cx)
            .display_path()
            .map(Path::to_path_buf)
    }

    /// A button that opens a folder here (middle click: in a new tab).
    fn rail_folder(
        id: impl Into<ElementId>,
        icon_el: AnyElement,
        label: &str,
        path: PathBuf,
        here: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (open, background) = (path.clone(), path);
        rail_button(id, icon_el, label, here)
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
    }

    /// The pinned list: pinned folders, then the Recycle Bin (when pinned) and the Shelf.
    fn rail_pins(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let (pinned, recycle_pinned) = (settings.pinned.clone(), settings.recycle_pinned);
        let current = self.rail_here(cx);
        let mut out = vec![divider()];
        for (ix, path) in pinned.into_iter().enumerate() {
            let here = current.as_deref() == Some(path.as_path());
            let label = display_name(&path);
            out.push(Self::rail_folder(
                ("rail-pin", ix),
                kind_icon(FileKind::Folder, theme::icon()),
                &label,
                path,
                here,
                cx,
            ));
        }
        if recycle_pinned {
            let open = self.recycle_open();
            out.push(
                rail_button(
                    "rail-recycle",
                    icon(Icon::Trash, open),
                    &t!("recycle.title"),
                    open,
                )
                .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                    this.open_recycle(e.modifiers().control, window, cx);
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.open_recycle(true, window, cx);
                    }),
                )
                .into_any_element(),
            );
        }
        out.push(Self::rail_shelf(cx));
        out
    }

    /// The Shelf: files dropped on it join the shelf; a click opens the full sidebar with the
    /// shelf's items, which need its width.
    fn rail_shelf(cx: &mut Context<Self>) -> AnyElement {
        let held = crate::shelf::ShelfStore::global(cx).read(cx).shelf().len();
        rail_button(
            "rail-shelf",
            icon(Icon::Shelf, false),
            &t!("shelf.title"),
            false,
        )
        .when(held > 0, |d| {
            d.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(1.))
                    .min_w(crate::theme::scaled_px(14.))
                    .h(crate::theme::scaled_px(14.))
                    .px(crate::theme::scaled_px(3.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::R_FULL)
                    .bg(theme::wash_strong())
                    .text_size(theme::fs_11())
                    .text_color(theme::text_muted())
                    .child(arke_core::format::format_count(held)),
            )
        })
        .can_drop(|value, _, _| {
            value
                .downcast_ref::<DraggedFiles>()
                .is_none_or(|d| d.shelf.is_none())
        })
        .drag_over::<DraggedFiles>(|s, _, _, _| s.bg(theme::signal_wash()))
        .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(theme::signal_wash()))
        .on_drop(cx.listener(|this, dragged: &DraggedFiles, _, cx| {
            let items = dragged
                .source
                .upgrade()
                .map_or_default(|e| e.read(cx).drag_items(dragged.ix));
            this.add_to_shelf_from_drop(items, cx);
        }))
        .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
            // Virtual files aren't anywhere the shelf could point to.
            if crate::win_virtual_drop::is_virtual(paths.paths()) {
                return;
            }
            let (paths, _) = crate::explorer::hold_temp_drop(paths.paths().to_vec());
            let items = paths.into_iter().map(|p| (p, false)).collect();
            this.add_to_shelf_from_drop(items, cx);
        }))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            if !this.shelf_open {
                this.toggle_shelf(cx);
            }
            this.rail_hover += 1;
            this.rail_peek = true;
            cx.notify();
        }))
        .into_any_element()
    }

    /// The drives, local and network, as their letters.
    fn rail_drives(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let current = self.rail_here(cx);
        let places = self.active_view().read(cx).places.clone();
        let names = SettingsStore::global(cx)
            .read(cx)
            .settings
            .place_names
            .clone();
        places
            .into_iter()
            .filter(|p| matches!(p.kind, PlaceKind::Drive { .. }))
            .enumerate()
            .map(|(ix, place)| {
                let here = current.as_deref() == Some(place.path.as_path());
                let letter = div()
                    .text_size(theme::fs_12())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(if here {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .child(drive_letter(&place.path))
                    .into_any_element();
                Self::rail_folder(
                    ("rail-drive", ix),
                    letter,
                    &crate::place_names::place_label(&names, &place),
                    place.path,
                    here,
                    cx,
                )
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
                .rounded(theme::r_8())
                .when(enabled, |d| {
                    d.cursor_pointer().hover(|s| s.bg(theme::wash()))
                })
                .child(
                    svg()
                        .path(glyph.path())
                        .size(theme::icon())
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
            .gap(theme::s_2())
            .child(
                button(
                    "rail-back",
                    Icon::ArrowLeft,
                    t!("nav.back").into_owned(),
                    can_back,
                )
                .on_click(move |_, window, cx| {
                    view.update(cx, |e, cx| e.go_back(&GoBack, window, cx));
                })
                .on_mouse_down(MouseButton::Right, Self::history_menu_listener(true, cx)),
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
                })
                .on_mouse_down(MouseButton::Right, Self::history_menu_listener(false, cx)),
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
                kind_icon(FileKind::Folder, theme::icon()),
            ),
        };
        rail_button(("rail-tab", ix), icon_el, &title, active)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.activate(ix, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    // Pinned tabs don't close by accident, as in the full sidebar.
                    if !this.tabs.get(ix).is_some_and(|t| t.pinned) {
                        this.close_tab(ix, window, cx);
                    }
                }),
            )
            .into_any_element()
    }

    /// The pointer entered or left the column: open the full sidebar over the content after a
    /// short rest (when that is on). Leaving before then cancels it.
    fn rail_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.rail_hover += 1;
        if !hovered || self.rail_peek || !SettingsStore::global(cx).read(cx).settings.sidebar_peek {
            return;
        }
        let ticket = self.rail_hover;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PEEK_OPEN).await;
            let _ = this.update(cx, |this, cx| {
                if this.rail_hover == ticket && theme::sidebar_collapsed() {
                    this.rail_peek = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The pointer moved anywhere in the window: past the open sidebar's edge it closes after a
    /// moment; back over it, it stays.
    pub(super) fn peek_pointer_moved(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.rail_peek {
            return;
        }
        let outside = e.position.x > theme::sidebar_full_width();
        if !outside {
            // Cancels a pending close.
            self.rail_hover += 1;
            return;
        }
        if self.peek_closing {
            return;
        }
        self.peek_closing = true;
        self.rail_hover += 1;
        let ticket = self.rail_hover;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PEEK_CLOSE).await;
            let _ = this.update(cx, |this, cx| {
                this.peek_closing = false;
                if this.rail_hover == ticket {
                    this.rail_peek = false;
                    this.fade_out_peek(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The open sidebar closed because the pointer left: draw it fading out for `motion::EXIT`.
    fn fade_out_peek(&mut self, cx: &mut Context<Self>) {
        if !crate::motion::enabled() {
            return;
        }
        let serial = self.sidebar_ui.peek_ghost.unwrap_or(0) + 1;
        self.sidebar_ui.peek_ghost = Some(serial);
        self.sidebar_ui.peek_ghost_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::motion::EXIT + Duration::from_millis(20))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.sidebar_ui.peek_ghost == Some(serial) {
                    this.sidebar_ui.peek_ghost = None;
                    cx.notify();
                }
            });
        }));
    }

    /// The full sidebar over the content while the collapsed one is open (painted after the panes,
    /// so it covers them). It slides in from the column's edge and fades out when the pointer
    /// leaves, like the other panels over the content.
    pub(super) fn render_peek(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !theme::sidebar_collapsed() {
            return None;
        }
        let frame = || {
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(theme::sidebar_full_width())
        };
        if self.rail_peek {
            let body = self.peek_body(cx);
            return Some(
                deferred(
                    frame()
                        .id("sidebar-peek")
                        .debug_selector(|| "sidebar-peek".to_owned())
                        .occlude()
                        // The window's own listener doesn't see moves over this cover: coming back
                        // here must still cancel a pending close.
                        .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                            this.peek_pointer_moved(e, cx);
                        }))
                        .child(crate::motion::appear(
                            body,
                            "sidebar-peek-in",
                            -crate::motion::SLIDE,
                            0.,
                        )),
                )
                .with_priority(1)
                .into_any_element(),
            );
        }
        let serial = self.sidebar_ui.peek_ghost?;
        let body = self.peek_body(cx);
        Some(
            deferred(
                frame()
                    .child(gpui::AnimationExt::with_animation(
                        body,
                        ElementId::NamedInteger("sidebar-peek-out".into(), serial),
                        crate::motion::exit(),
                        |el, t| crate::motion::slide_out(el, t, -crate::motion::SLIDE, 0.),
                    ))
                    // Inert while it fades: a cover takes the pointer.
                    .child(
                        div()
                            .id("sidebar-peek-cover")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .occlude(),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn peek_body(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::frame_solid())
            .border_r_1()
            .border_color(theme::line())
            .shadow(theme::elev_float())
            .child(self.render_brand_row(cx))
            .child(self.render_home_button(cx))
            .child(self.render_sidebar(cx))
            .child(self.render_settings_button(cx))
    }

    /// Right click on the column: turn opening on hover on or off.
    pub(super) fn set_sidebar_peek(on: bool, cx: &mut App) {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.sidebar_peek = on, cx));
    }

    /// Whether the full sidebar is open over the collapsed one (tests).
    #[cfg(test)]
    pub(crate) const fn peek_for_test(&self) -> bool {
        self.rail_peek
    }
}

/// The new-version dot on the Settings button.
fn update_dot(cx: &App) -> Option<AnyElement> {
    crate::update::UpdateState::has_news(cx).then(|| {
        div()
            .absolute()
            .top(crate::theme::scaled_px(6.))
            .right(crate::theme::scaled_px(6.))
            .size(crate::theme::scaled_px(7.))
            .rounded(theme::R_FULL)
            .bg(theme::signal())
            .into_any_element()
    })
}
