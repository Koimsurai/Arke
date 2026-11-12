//! The filter section of the search panel (Type, Size, Date modified, Contents) and the content
//! search for "this folder".
//!
//! Each filter is a dropdown button showing its value ("Type: Images"); it opens a menu with a
//! check on the current choice, and a set filter is a token with an × to remove it. The buttons
//! are a view of the box text: picking writes the term (`type:image`, `size:>1mb`, `dm:thisweek`,
//! `content:"…"`) through `arke_core::index_query::set_facet`, and every render reads the state
//! back from the text, so typing a term sets its button and removing the button removes the term.
//!
//! Content search in "this folder" reads the folder's files on a background thread and streams
//! matches into the filtered list; "subfolders" does it in the search service. "This PC" can't
//! (the index has names only), which the Contents switch and the result area say.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use arke_core::Entry;
use arke_core::content::{ContentMatcher, worth_reading};
use arke_core::filter::filter_by_query;
use arke_core::index_query::{
    self as iq, DatePreset, Facet, FacetState, Query, SizePreset, TypeFilter,
};
use gpui::{
    AnimationExt as _, AnyElement, Bounds, ClickEvent, Context, Focusable as _, MouseButton,
    MouseDownEvent, Pixels, Window, canvas, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::scope::{PanelItem, Scope};
use super::{Explorer, FilterKey, Filtered, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;

/// Minimum time between list updates while matches stream in.
const STREAM_EVERY: Duration = Duration::from_millis(80);

/// The dropdown filters, in panel order.
pub(crate) const FACETS: [Facet; 3] = [Facet::Type, Facet::Size, Facet::Date];

/// A running content search over the current folder. Dropping it stops the reader thread.
pub(crate) struct ContentFilter {
    cancel: Arc<AtomicBool>,
    /// Files read so far.
    read: Arc<AtomicUsize>,
    /// Files to read (known once the reader has applied the other filters).
    total: Arc<AtomicUsize>,
}

impl Drop for ContentFilter {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Found {
    Match(Entry),
    Progress,
    Done,
}

/// Where the panel and its filter buttons were drawn last frame, so a filter's menu can open
/// right under its button inside the panel (screen readers then keep it in the panel's tree).
#[derive(Default)]
pub(crate) struct PanelGeometry {
    panel: Option<Bounds<Pixels>>,
    buttons: [Option<Bounds<Pixels>>; 3],
}

/// Which bounds a [`measure`] element records.
#[derive(Clone, Copy)]
enum Measured {
    Panel,
    Button(usize),
}

impl PanelGeometry {
    /// Records `bounds`; true when they moved (redraw so the menu follows).
    fn set(&mut self, what: Measured, bounds: Bounds<Pixels>) -> bool {
        let slot = match what {
            Measured::Panel => &mut self.panel,
            Measured::Button(ix) => &mut self.buttons[ix],
        };
        let moved = *slot != Some(bounds);
        *slot = Some(bounds);
        moved
    }
}

/// Invisible element recording where its parent was drawn.
fn measure(geometry: &Rc<RefCell<PanelGeometry>>, what: Measured) -> impl IntoElement {
    let geometry = geometry.clone();
    canvas(
        move |bounds, window, _| {
            if geometry.borrow_mut().set(what, bounds) {
                window.refresh();
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .size_full()
}

/// Records the panel's bounds (place it inside the panel).
pub(crate) fn panel_measure(geometry: &Rc<RefCell<PanelGeometry>>) -> impl IntoElement {
    measure(geometry, Measured::Panel)
}

fn type_label(t: TypeFilter) -> Cow<'static, str> {
    match t {
        TypeFilter::Folder => t!("filter.type_folder"),
        TypeFilter::Document => t!("filter.type_document"),
        TypeFilter::Image => t!("filter.type_image"),
        TypeFilter::Audio => t!("filter.type_audio"),
        TypeFilter::Video => t!("filter.type_video"),
        TypeFilter::Archive => t!("filter.type_archive"),
        TypeFilter::Code => t!("filter.type_code"),
        TypeFilter::Executable => t!("filter.type_app"),
    }
}

fn size_label(p: SizePreset) -> Cow<'static, str> {
    match p {
        SizePreset::Over1Mb => t!("filter.size_over", size = "1 MB"),
        SizePreset::Over100Mb => t!("filter.size_over", size = "100 MB"),
        SizePreset::Over1Gb => t!("filter.size_over", size = "1 GB"),
        SizePreset::Empty => t!("filter.size_empty"),
    }
}

fn date_label(p: DatePreset) -> Cow<'static, str> {
    match p {
        DatePreset::Today => t!("filter.date_today"),
        DatePreset::ThisWeek => t!("filter.date_week"),
        DatePreset::ThisMonth => t!("filter.date_month"),
        DatePreset::ThisYear => t!("filter.date_year"),
    }
}

pub(crate) fn facet_name(facet: Facet) -> Cow<'static, str> {
    match facet {
        Facet::Type => t!("filter.type"),
        Facet::Size => t!("filter.size"),
        Facet::Date => t!("filter.date"),
        Facet::Content => t!("filter.contents"),
    }
}

/// The "no filter" row of a menu.
fn any_label(facet: Facet) -> Cow<'static, str> {
    match facet {
        Facet::Size => t!("filter.any_size"),
        Facet::Date => t!("filter.any_date"),
        Facet::Type | Facet::Content => t!("filter.any_type"),
    }
}

const fn facet_id(facet: Facet) -> &'static str {
    match facet {
        Facet::Type => "filter-type",
        Facet::Size => "filter-size",
        Facet::Date => "filter-date",
        Facet::Content => "filter-contents",
    }
}

const fn facet_slot(facet: Facet) -> usize {
    match facet {
        Facet::Type | Facet::Content => 0,
        Facet::Size => 1,
        Facet::Date => 2,
    }
}

/// What a set filter shows: the preset's name, or the terms as typed. `None` when off.
fn state_label<T: Copy>(
    state: &FacetState<T>,
    label: impl Fn(T) -> Cow<'static, str>,
) -> Option<String> {
    match state {
        FacetState::Off => None,
        FacetState::Preset(p) => Some(label(*p).into_owned()),
        FacetState::Custom(raw) => Some(raw.clone()),
    }
}

/// One row of a filter menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuChoice {
    pub(crate) label: String,
    /// The term it writes; `None` removes the filter.
    pub(crate) term: Option<String>,
    pub(crate) selected: bool,
}

/// Menu rows for one facet's state: "Any …" first, then terms typed by hand (kept as they are),
/// then the presets.
fn choices<T: Copy + PartialEq>(
    facet: Facet,
    state: &FacetState<T>,
    presets: &[T],
    label: impl Fn(T) -> Cow<'static, str>,
    term: impl Fn(T) -> &'static str,
) -> Vec<MenuChoice> {
    let mut out = vec![MenuChoice {
        label: any_label(facet).into_owned(),
        term: None,
        selected: *state == FacetState::Off,
    }];
    if let FacetState::Custom(raw) = state {
        out.push(MenuChoice {
            label: t!("filter.custom", term = raw).into_owned(),
            term: Some(raw.clone()),
            selected: true,
        });
    }
    out.extend(presets.iter().map(|p| MenuChoice {
        label: label(*p).into_owned(),
        term: Some(term(*p).to_owned()),
        selected: *state == FacetState::Preset(*p),
    }));
    out
}

impl Explorer {
    /// Replaces the box text the way a filter does and reruns the search, keeping the panel open.
    /// Focus stays in the panel when it has it (keyboard use); `focus_box` moves it to the box
    /// with the caret at `caret` (default: the end), for terms that need typing.
    fn write_query(
        &mut self,
        text: String,
        caret: Option<usize>,
        focus_box: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter_input.update(cx, |input, cx| {
            input.set_text(text.clone(), cx);
            let at = caret.unwrap_or(text.len());
            input.select_range(at..at, cx);
        });
        self.query = text;
        self.scope_strip_open = true;
        self.on_query_changed(cx);
        let focus = self.filter_input.read(cx).focus_handle(cx);
        if focus_box || !self.panel_focus.is_focused(window) {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(crate) fn set_facet(
        &mut self,
        facet: Facet,
        term: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = iq::set_facet(&self.query, facet, term);
        self.filter_menu = None;
        self.write_query(text, None, false, window, cx);
    }

    pub(crate) fn toggle_contents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !iq::has_content_term(&self.query);
        let text = iq::set_content_search(&self.query, on);
        // Empty `content:""`: put the caret between the quotes, ready to type.
        let empty = on && text.ends_with("\"\"");
        let caret = empty.then(|| text.len() - 1);
        self.filter_menu = None;
        self.write_query(text, caret, empty, window, cx);
    }

    /// Removes every filter (the name text stays; content text becomes name text again).
    pub(crate) fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut text = iq::set_content_search(&self.query, false);
        for facet in FACETS {
            text = iq::set_facet(&text, facet, None);
        }
        self.filter_menu = None;
        self.write_query(text, None, false, window, cx);
    }

    /// Why Contents can't be used here: whole-PC search only knows names, and on SFTP or
    /// WebDAV reading every file would download it.
    fn contents_unavailable(&self) -> Option<Cow<'static, str>> {
        if self.scope() == Scope::Computer {
            Some(t!("filter.contents_unavailable"))
        } else if self.on_remote() {
            Some(t!("filter.contents_remote"))
        } else {
            None
        }
    }

    /// Contents can be switched now (it can always be turned off).
    pub(crate) fn contents_usable(&self) -> bool {
        self.contents_unavailable().is_none() || iq::has_content_term(&self.query)
    }

    /// The value a Type, Size or Date filter shows; `None` when off.
    pub(crate) fn facet_value(&self, facet: Facet) -> Option<String> {
        let text = self.query.as_str();
        match facet {
            Facet::Type => state_label(&iq::type_state(text), type_label),
            Facet::Size => state_label(&iq::size_state(text), size_label),
            Facet::Date => state_label(&iq::date_state(text, crate::win::clock()), date_label),
            Facet::Content => iq::has_content_term(text).then(|| facet_name(facet).into_owned()),
        }
    }

    /// Some filter is set (Clear filters shows).
    pub(crate) fn any_filter(&self) -> bool {
        FACETS.iter().any(|f| self.facet_value(*f).is_some()) || iq::has_content_term(&self.query)
    }

    /// Rows of `facet`'s menu for the current text.
    pub(crate) fn menu_choices(&self, facet: Facet) -> Vec<MenuChoice> {
        let text = self.query.as_str();
        match facet {
            Facet::Type => choices(
                facet,
                &iq::type_state(text),
                &TypeFilter::ALL,
                type_label,
                type_term,
            ),
            Facet::Size => choices(
                facet,
                &iq::size_state(text),
                &SizePreset::ALL,
                size_label,
                SizePreset::term,
            ),
            Facet::Date => choices(
                facet,
                &iq::date_state(text, crate::win::clock()),
                &DatePreset::ALL,
                date_label,
                DatePreset::term,
            ),
            Facet::Content => Vec::new(),
        }
    }

    /// Opens `facet`'s menu (or closes it when open) with the cursor on the current choice. The
    /// panel takes keyboard focus so the arrows move in the menu.
    pub(crate) fn toggle_filter_menu(
        &mut self,
        facet: Facet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_menu == Some(facet) {
            self.filter_menu = None;
        } else {
            self.filter_menu = Some(facet);
            self.menu_cursor = self
                .menu_choices(facet)
                .iter()
                .position(|c| c.selected)
                .unwrap_or(0);
        }
        self.panel_cursor = PanelItem::Filter(facet);
        self.scope_strip_open = true;
        window.focus(&self.panel_focus, cx);
        cx.notify();
    }

    /// Picks row `ix` of `facet`'s menu and closes it.
    pub(crate) fn pick_menu_choice(
        &mut self,
        facet: Facet,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(choice) = self.menu_choices(facet).into_iter().nth(ix) {
            self.set_facet(facet, choice.term.as_deref(), window, cx);
        }
        self.filter_menu = None;
        cx.notify();
    }

    /// The Filters section of the panel: heading (with Clear filters), the three dropdowns and the
    /// Contents switch. `cursor` is the keyboard position while the panel has focus.
    pub(super) fn render_filter_section(
        &self,
        cursor: Option<PanelItem>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let clear = self.any_filter().then(|| {
            let at = cursor == Some(PanelItem::ClearFilters);
            div()
                .id("filter-clear-all")
                .a11y_button(t!("filter.clear_all"))
                .when(at, gpui::StatefulInteractiveElement::aria_active_descendant)
                .h(theme::S_24)
                .px(theme::S_8)
                .flex()
                .items_center()
                .rounded(theme::R_4)
                .border_1()
                .border_color(if at {
                    theme::signal_line()
                } else {
                    theme::clear()
                })
                .cursor_pointer()
                .text_size(theme::FS_12)
                .text_color(theme::text_muted())
                .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
                .child(t!("filter.clear_all").into_owned())
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.clear_filters(window, cx);
                }))
                .into_any_element()
        });
        let buttons = FACETS.map(|facet| {
            let at = cursor == Some(PanelItem::Filter(facet));
            self.filter_button(facet, at, cx)
        });
        div()
            .flex()
            .flex_col()
            .gap(theme::S_8)
            .child(super::scope::panel_heading(t!("filter.section"), clear))
            .child(
                div()
                    .id("filter-group")
                    .a11y(gpui::Role::Group, t!("filter.section"))
                    .flex()
                    .flex_wrap()
                    .gap(theme::S_8)
                    .children(buttons),
            )
            .child(self.contents_row(cursor == Some(PanelItem::Contents), cx))
            .into_any_element()
    }

    /// A Type / Size / Date dropdown: "Type ⌄" when off; when set, a token "Type: Images ×".
    fn filter_button(&self, facet: Facet, at: bool, cx: &mut Context<Self>) -> AnyElement {
        let value = self.facet_value(facet);
        let set = value.is_some();
        let open = self.filter_menu == Some(facet);
        let name = facet_name(facet).into_owned();
        let spoken = value
            .clone()
            .unwrap_or_else(|| any_label(facet).into_owned());
        let remove = value.as_ref().map(|v| {
            let label = t!("filter.remove_named", name = format!("{name}: {v}")).into_owned();
            div()
                .id(match facet {
                    Facet::Type => "filter-type-clear",
                    Facet::Size => "filter-size-clear",
                    Facet::Date | Facet::Content => "filter-date-clear",
                })
                .a11y_button(label)
                .size(theme::S_24)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(theme::R_4)
                .hover(|s| s.bg(theme::wash_strong()))
                .child(
                    svg()
                        .path(Icon::Close.path())
                        .size(theme::ICON_SM)
                        .text_color(theme::text_muted()),
                )
                .tooltip(crate::workspace::tooltip(t!("filter.remove")))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.set_facet(facet, None, window, cx);
                }))
        });
        div()
            .id(facet_id(facet))
            .a11y_combo_named(name.clone(), spoken, open)
            .when(set, |d| d.aria_keyshortcuts("Delete"))
            .when(
                at && !open,
                gpui::StatefulInteractiveElement::aria_active_descendant,
            )
            .relative()
            .flex_none()
            .h(theme::H_CTL_MD)
            .pl(theme::S_12)
            .pr(if set { theme::S_2 } else { theme::S_8 })
            .flex()
            .items_center()
            .gap(theme::S_6)
            .rounded(theme::R_8)
            .border_1()
            .cursor_pointer()
            .text_size(theme::FS_12)
            .text_color(theme::text())
            .whitespace_nowrap()
            .border_color(if at {
                theme::signal_line()
            } else if set {
                theme::clear()
            } else {
                theme::line_strong()
            })
            .when(set, |d| d.bg(theme::signal_wash()))
            .when(!set, |d| {
                d.bg(if open {
                    theme::wash_strong()
                } else {
                    theme::wash()
                })
                .hover(|s| s.bg(theme::wash_strong()))
            })
            .child(measure(
                &self.panel_geometry,
                Measured::Button(facet_slot(facet)),
            ))
            .child(div().child(if set { format!("{name}:") } else { name }))
            .when_some(value, |d, v| {
                d.child(
                    div()
                        .min_w_0()
                        .max_w(theme::FILTER_VALUE_MAX_W)
                        .truncate()
                        .font_weight(theme::FW_STRONG)
                        .child(v),
                )
            })
            .when(!set, |d| d.child(chevron()))
            .children(remove)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.toggle_filter_menu(facet, window, cx);
            }))
            .into_any_element()
    }

    /// Row `ix` of `facet`'s menu (`count` rows): a check on the current choice, `at` the
    /// keyboard row (`focused`: reported as focused).
    fn menu_row(
        facet: Facet,
        ix: usize,
        count: usize,
        choice: MenuChoice,
        focused: bool,
        at: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id((facet_id(facet), ix))
            .a11y(gpui::Role::ListBoxOption, choice.label.clone())
            .aria_selected(choice.selected)
            .aria_position_in_set(ix)
            .aria_size_of_set(count)
            .when(
                focused,
                gpui::StatefulInteractiveElement::aria_active_descendant,
            )
            .flex_none()
            .h(theme::H_CTL_MD)
            .pl(theme::S_8)
            .pr(theme::S_16)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_4)
            .cursor_pointer()
            .text_size(theme::FS_13)
            .text_color(theme::text())
            .whitespace_nowrap()
            .hover(|s| s.bg(theme::wash()))
            .when(at, |d| d.bg(theme::wash_strong()))
            .child(
                svg()
                    .path(Icon::Check.path())
                    .flex_none()
                    .size(theme::ICON)
                    .text_color(if choice.selected {
                        theme::signal()
                    } else {
                        theme::clear()
                    }),
            )
            .child(
                div()
                    .when(choice.selected, |d| d.font_weight(theme::FW_STRONG))
                    .child(choice.label),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.pick_menu_choice(facet, ix, window, cx);
            }))
            .into_any_element()
    }

    /// The open filter's menu, placed under its button. Drawn last inside the panel so it covers
    /// what is below; `keyboard`: the panel has focus (the cursor row is reported as focused).
    pub(super) fn render_filter_menu(
        &self,
        keyboard: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let facet = self.filter_menu?;
        let (left, top) = {
            let g = self.panel_geometry.borrow();
            let (panel, button) = (g.panel?, g.buttons[facet_slot(facet)]?);
            // Under the button, but not past the panel's right edge (the window edge may be
            // right there).
            let room = (panel.size.width - theme::FILTER_MENU_W).max(gpui::px(0.));
            (
                (button.left() - panel.left()).min(room),
                button.bottom() - panel.top() + theme::S_4,
            )
        };
        let choices = self.menu_choices(facet);
        let count = choices.len();
        let mut rows: Vec<AnyElement> = Vec::with_capacity(count + 1);
        for (ix, choice) in choices.into_iter().enumerate() {
            let at = ix == self.menu_cursor;
            rows.push(Self::menu_row(
                facet,
                ix,
                count,
                choice,
                keyboard && at,
                at,
                cx,
            ));
            // "Any …" stands apart from the choices.
            if ix == 0 {
                rows.push(
                    div()
                        .flex_none()
                        .h(px(1.))
                        .my(theme::S_2)
                        .mx(theme::S_8)
                        .bg(theme::line())
                        .into_any_element(),
                );
            }
        }
        Some(
            div()
                .absolute()
                .left(left)
                .top(top)
                .child(
                    div()
                        .id(("filter-menu", facet_slot(facet)))
                        .a11y(gpui::Role::ListBox, facet_name(facet).into_owned())
                        .aria_size_of_set(count)
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                        })
                        .min_w(theme::FILTER_MENU_W)
                        .p(theme::S_4)
                        .flex()
                        .flex_col()
                        .gap(theme::S_2)
                        .rounded(theme::R_8)
                        .border_1()
                        .border_color(theme::line())
                        .bg(theme::overlay())
                        .shadow(theme::elev_2())
                        .children(rows)
                        .with_animation(
                            ("filter-menu-appear", facet_slot(facet)),
                            crate::motion::enter(),
                            |el, t| crate::motion::slide_in(el, t, 0., -crate::motion::NUDGE),
                        ),
                )
                .into_any_element(),
        )
    }

    /// "Search file contents" switch, with what it does or why it can't be used here.
    fn contents_row(&self, at: bool, cx: &mut Context<Self>) -> AnyElement {
        let on = iq::has_content_term(&self.query);
        let usable = self.contents_usable();
        let label = t!("filter.contents_tip").into_owned();
        let detail = self
            .contents_unavailable()
            .unwrap_or_else(|| t!("filter.contents_desc"))
            .into_owned();
        div()
            .id("filter-contents")
            .a11y(gpui::Role::Switch, label.clone())
            .aria_toggled(crate::a11y::toggled(on))
            .aria_description(detail.clone())
            .a11y_disabled(!usable)
            .when(at, gpui::StatefulInteractiveElement::aria_active_descendant)
            .min_h(theme::H_CTL)
            .px(theme::S_8)
            .py(theme::S_4)
            .flex()
            .items_center()
            .gap(theme::S_12)
            .rounded(theme::R_8)
            .border_1()
            .border_color(if at {
                theme::signal_line()
            } else {
                theme::clear()
            })
            .when(usable, |d| {
                d.cursor_pointer().hover(|s| s.bg(theme::wash()))
            })
            .child(contents_icon(if on {
                theme::signal()
            } else {
                theme::text_muted()
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(theme::FS_13)
                            .text_color(if usable {
                                theme::text()
                            } else {
                                theme::text_muted()
                            })
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle())
                            .child(detail),
                    ),
            )
            .child(switch_look(on, usable))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.contents_usable() {
                    this.panel_cursor = PanelItem::Contents;
                    this.toggle_contents(window, cx);
                }
            }))
            .into_any_element()
    }

    /// Content search in this folder is running.
    pub(crate) const fn content_searching(&self) -> bool {
        self.content_filter.is_some() && self.filter_inflight.is_some()
    }

    /// Context bar text for a content search in this folder.
    pub(super) fn content_info(&self) -> Option<String> {
        let filter = self.content_filter.as_ref()?;
        let read = filter.read.load(Ordering::Relaxed);
        let total = filter.total.load(Ordering::Relaxed);
        Some(if self.content_searching() {
            t!(
                "info.content_reading",
                read = arke_core::format::format_count(read),
                total = arke_core::format::format_count(total)
            )
            .into_owned()
        } else {
            tn!("info.content_read", total).into_owned()
        })
    }

    /// Starts reading this folder's files for `query`'s content text; matches stream into the
    /// filtered list. Replacing or dropping `content_filter` stops it.
    pub(super) fn start_content_filter(
        &mut self,
        key: FilterKey,
        entries: Arc<Vec<Entry>>,
        query: Query,
        cx: &mut Context<Self>,
    ) {
        let (Some(matcher), Some(dir)) = (
            query.content_needle().and_then(ContentMatcher::new),
            self.model.as_ref().map(|m| m.path.clone()),
        ) else {
            return;
        };
        // On a server every file would be downloaded: no matches, and say why.
        if arke_core::remote::is_remote(&dir) {
            self.content_filter = None;
            self.filter_inflight = None;
            self.filtered = Some(Filtered {
                key,
                entries: Arc::default(),
            });
            self.notice = Some(t!("filter.contents_remote").into_owned());
            cx.notify();
            return;
        }
        let filter = ContentFilter {
            cancel: Arc::new(AtomicBool::new(false)),
            read: Arc::new(AtomicUsize::new(0)),
            total: Arc::new(AtomicUsize::new(0)),
        };
        let (cancel, read, total) = (
            filter.cancel.clone(),
            filter.read.clone(),
            filter.total.clone(),
        );
        self.content_filter = Some(filter);
        // Start empty: the last query's matches would be wrong for this one.
        self.filtered = Some(Filtered {
            key: key.clone(),
            entries: Arc::default(),
        });
        self.filter_inflight = Some(key.clone());
        let fs = self.services.fs.clone();
        let (tx, rx) = flume::unbounded::<Found>();
        spawn_thread("content-filter", move || {
            let candidates: Vec<Entry> = filter_by_query(&entries, &query)
                .unwrap_or_default()
                .into_iter()
                .filter(worth_reading)
                .collect();
            total.store(candidates.len(), Ordering::Relaxed);
            let _ = tx.send(Found::Progress);
            let mut last = Instant::now();
            for entry in candidates {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let hit = matcher.find_in(&*fs, &dir.join(&*entry.name), &cancel);
                read.fetch_add(1, Ordering::Relaxed);
                if hit.is_some() {
                    let _ = tx.send(Found::Match(entry));
                } else if last.elapsed() >= STREAM_EVERY {
                    let _ = tx.send(Found::Progress);
                    last = Instant::now();
                }
            }
            let _ = tx.send(Found::Done);
        });
        self.filter_task = Some(cx.spawn(async move |this, cx| {
            let mut found: Vec<Entry> = Vec::new();
            loop {
                let mut done = rx
                    .recv_async()
                    .await
                    .map_or(true, |msg| take(msg, &mut found));
                // Take whatever else is ready, so matches arrive in batches.
                while !done && let Ok(msg) = rx.try_recv() {
                    done = take(msg, &mut found);
                }
                let entries = Arc::new(found.clone());
                let current = this
                    .update(cx, |this, cx| {
                        if this.filter_inflight.as_ref() != Some(&key) {
                            return false;
                        }
                        this.filtered = Some(Filtered {
                            key: key.clone(),
                            entries,
                        });
                        if done {
                            this.filter_inflight = None;
                        }
                        this.drop_filtered_out();
                        this.resolve_cursor();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !current || done {
                    return;
                }
                cx.background_executor().timer(STREAM_EVERY).await;
            }
        }));
    }
}

/// Adds a match; true once the reader is done.
fn take(msg: Found, found: &mut Vec<Entry>) -> bool {
    match msg {
        Found::Match(entry) => {
            found.push(entry);
            false
        }
        Found::Progress => false,
        Found::Done => true,
    }
}

/// The term a Type choice writes.
const fn type_term(t: TypeFilter) -> &'static str {
    match t {
        TypeFilter::Folder => "type:folder",
        TypeFilter::Document => "type:document",
        TypeFilter::Image => "type:image",
        TypeFilter::Audio => "type:audio",
        TypeFilter::Video => "type:video",
        TypeFilter::Archive => "type:archive",
        TypeFilter::Code => "type:code",
        TypeFilter::Executable => "type:app",
    }
}

/// A page with lines of text: the outline and the lines drawn on top of each other.
fn contents_icon(color: gpui::Rgba) -> impl IntoElement {
    let layer = |icon: Icon| {
        svg()
            .absolute()
            .top_0()
            .left_0()
            .path(icon.path())
            .size(theme::ICON)
            .text_color(color)
    };
    div()
        .relative()
        .flex_none()
        .size(theme::ICON)
        .child(layer(Icon::PageLine))
        .child(layer(Icon::FtText))
}

/// Down chevron of a closed dropdown.
fn chevron() -> impl IntoElement {
    svg()
        .path(Icon::ChevronRight.path())
        .flex_none()
        .size(theme::ICON_SM)
        .text_color(theme::text_muted())
        .with_transformation(gpui::Transformation::rotate(gpui::radians(
            std::f32::consts::FRAC_PI_2,
        )))
}

/// A switch drawn like the settings switches; the row around it takes the click.
fn switch_look(on: bool, usable: bool) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(40.))
        .h(px(20.))
        .flex()
        .items_center()
        .px(px(3.))
        .rounded(theme::R_FULL)
        .border_1()
        .border_color(if on {
            theme::clear()
        } else {
            theme::line_strong()
        })
        .bg(if on {
            theme::signal()
        } else {
            theme::wash_strong()
        })
        .when(!usable, |d| d.opacity(0.6))
        .child(
            div()
                .size(px(14.))
                .rounded(theme::R_FULL)
                .ml(px(if on { 18. } else { 0. }))
                .bg(if on {
                    theme::on_signal()
                } else {
                    theme::text_muted()
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_start_with_any_and_keep_typed_terms() {
        let off = choices(
            Facet::Size,
            &FacetState::<SizePreset>::Off,
            &SizePreset::ALL,
            size_label,
            SizePreset::term,
        );
        assert_eq!(off.len(), 1 + SizePreset::ALL.len());
        assert!(off[0].selected && off[0].term.is_none());
        let preset = choices(
            Facet::Size,
            &FacetState::Preset(SizePreset::Over1Gb),
            &SizePreset::ALL,
            size_label,
            SizePreset::term,
        );
        let picked: Vec<Option<&str>> = preset
            .iter()
            .filter(|c| c.selected)
            .map(|c| c.term.as_deref())
            .collect();
        assert_eq!(picked, [Some("size:>1gb")]);
        let custom = choices(
            Facet::Size,
            &FacetState::<SizePreset>::Custom("size:>5mb".into()),
            &SizePreset::ALL,
            size_label,
            SizePreset::term,
        );
        assert_eq!(custom[1].term.as_deref(), Some("size:>5mb"));
        assert!(custom[1].selected && !custom[0].selected);
    }

    #[test]
    fn type_terms_round_trip_through_the_query() {
        for t in TypeFilter::ALL {
            assert_eq!(iq::type_state(type_term(t)), FacetState::Preset(t), "{t:?}");
        }
    }
}
