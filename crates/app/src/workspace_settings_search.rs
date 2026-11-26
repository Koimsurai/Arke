//! Settings search (#400): the box in the Settings title bar finds settings on every page.
//!
//! The index is not a list of its own: while a query is typed, every settings row the pages build
//! (`card`, `row`, `sub_row`, `expander`, group headings) notes its title and second line here as
//! it is made, under the page being built ([`page`]), so the search always matches what the pages
//! show. Rows are matched in the UI language and in English, so "search X in Settings" written in
//! English works in every language.
//!
//! Picking a hit shows its page, scrolls the row into view and lights it for a moment.
//! Ctrl+F (the "focus filter" key) puts the cursor in the box, Up and Down move through the hits,
//! Enter goes to one, Esc clears the box.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, Entity, Focusable as _, SharedString,
    Subscription, Window, canvas, div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{LAST_TOGGLE, Workspace};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::text_input::{InputEvent, TextInput};
use crate::theme;

/// How long a row picked from the search stays lit (it holds, then fades).
const FLASH: Duration = Duration::from_millis(1600);
/// Share of [`FLASH`] the light holds before it fades.
const FLASH_HOLD: f32 = 0.35;

/// A settings row as the pages built it: what the search matches and where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    /// Index of the page (category) it is on.
    pub(super) page: usize,
    pub(super) title: SharedString,
    /// The second line, when it is text.
    pub(super) desc: Option<SharedString>,
    /// The page itself (its name in the category list): going there lights no row.
    pub(super) whole_page: bool,
}

/// The row a hit went to, lit while it is drawn.
#[derive(Clone, Debug)]
struct Flash {
    page: usize,
    title: SharedString,
    /// Counts the jumps, so going to the same row again lights it again.
    serial: usize,
}

/// The search box and what it found.
pub(super) struct Search {
    pub(super) input: Entity<TextInput>,
    _input: Subscription,
    /// The hit Enter goes to.
    cursor: usize,
    /// The hits as last drawn, for Enter and clicks.
    hits: Rc<RefCell<Vec<Row>>>,
    flash: Option<Flash>,
    jumps: usize,
}

impl Search {
    /// An empty box, Tab stops at it.
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let input =
            cx.new(|cx| TextInput::new(t!("settings.find_setting").to_string(), cx).key_stop());
        let subscription = cx.subscribe_in(&input, window, Workspace::on_settings_search);
        Self {
            input,
            _input: subscription,
            cursor: 0,
            hits: Rc::default(),
            flash: None,
            jumps: 0,
        }
    }

    /// The typed query, trimmed; empty shows the pages.
    pub(super) fn query(&self, cx: &App) -> String {
        self.input.read(cx).committed_text().trim().to_owned()
    }

    /// Starts noting rows for this frame: all of them while `query` is typed, and the row to
    /// light while `category` (the page shown) is the one a hit went to.
    pub(super) fn begin(&self, query: &str, category: usize) {
        let target = self
            .flash
            .as_ref()
            .filter(|f| query.is_empty() && f.page == category)
            .cloned();
        BUILD.with_borrow_mut(|b| {
            b.page = 0;
            b.rows = (!query.is_empty()).then(Vec::new);
            b.target = target;
        });
    }

    /// A page was picked from the categories: the box empties (the page shows) and a hit's row
    /// is no longer lit.
    pub(super) fn left(&mut self, cx: &mut App) {
        self.flash = None;
        self.cursor = 0;
        if !self.input.read(cx).text().is_empty() {
            self.input.update(cx, |i, cx| i.set_text("", cx));
        }
    }

    /// The hits for `query` among `rows` (from [`finish`]) and the page names (`pages`, in
    /// order), as a list to draw in place of the page.
    pub(super) fn results(
        &self,
        query: &str,
        rows: Vec<Row>,
        pages: &[(SharedString, Icon)],
        focused: bool,
        cx: &mut Context<Workspace>,
    ) -> AnyElement {
        let mut all: Vec<Row> = pages
            .iter()
            .enumerate()
            .map(|(page, (title, _))| Row {
                page,
                title: title.clone(),
                desc: None,
                whole_page: true,
            })
            .collect();
        all.extend(rows);
        // Page by page, each page's name before its rows (the sort keeps the order they were
        // made in).
        all.sort_by_key(|r| (r.page, !r.whole_page));
        let hits = matches(&all, query, &english);
        let cursor = self.cursor.min(hits.len().saturating_sub(1));
        let list = results_list(&hits, pages, focused.then_some(cursor), cx);
        *self.hits.borrow_mut() = hits;
        list
    }
}

thread_local! {
    /// Rows noted while the pages are built this frame (see the module docs).
    static BUILD: RefCell<Build> = const {
        RefCell::new(Build {
            page: 0,
            rows: None,
            target: None,
        })
    };
    /// The lit row scrolls into view when it is next drawn.
    static REVEAL: Cell<bool> = const { Cell::new(false) };
    /// English text of each string of the UI language, lowercased, for that language.
    static ENGLISH: RefCell<Option<(String, EnglishOf)>> = const { RefCell::new(None) };
}

/// Each string of one UI language and its English text (lowercased), see [`english`].
type EnglishOf = HashMap<String, Vec<String>>;

/// What the pages note while they are built (see [`note`]).
struct Build {
    /// The page being built.
    page: usize,
    /// Every row so far, while searching.
    rows: Option<Vec<Row>>,
    /// The row to light, until it is drawn.
    target: Option<Flash>,
}

/// The rows made from here on are on page `ix`.
pub(super) fn page(ix: usize) {
    BUILD.with_borrow_mut(|b| b.page = ix);
}

/// A row was made (title and second line). Returns the serial to light it with when it is the row
/// a hit went to.
pub(super) fn note(title: &str, desc: Option<&str>) -> Option<usize> {
    BUILD.with_borrow_mut(|b| {
        let page = b.page;
        if let Some(rows) = &mut b.rows {
            rows.push(Row {
                page,
                title: SharedString::from(title.to_owned()),
                desc: desc.map(|d| SharedString::from(d.to_owned())),
                whole_page: false,
            });
        }
        let hit = b
            .target
            .as_ref()
            .is_some_and(|t| t.page == page && t.title.as_ref() == title);
        if hit {
            b.target.take().map(|t| t.serial)
        } else {
            None
        }
    })
}

/// A row made on the current page was dropped before it was shown.
pub(super) fn forget(title: &str) {
    BUILD.with_borrow_mut(|b| {
        let page = b.page;
        if let Some(rows) = &mut b.rows {
            rows.retain(|r| r.page != page || r.title.as_ref() != title);
        }
    });
}

/// The rows noted since [`Search::begin`] (none unless searching).
pub(super) fn finish() -> Vec<Row> {
    BUILD.with_borrow_mut(|b| {
        b.target = None;
        b.rows.take().unwrap_or_default()
    })
}

/// `el`, lit when [`note`] said it is the row a hit went to: the accent wash that fades, and the
/// row scrolled into view the first time it is drawn.
pub(super) fn light<E>(el: E, lit: Option<usize>) -> AnyElement
where
    E: IntoElement + Styled + ParentElement + 'static,
{
    let Some(serial) = lit else {
        return el.into_any_element();
    };
    el.relative()
        .child(
            div()
                .debug_selector(|| "settings-search-target".to_owned())
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(
                    canvas(
                        |bounds, window, _| {
                            if REVEAL.replace(false) {
                                crate::keynav::show(bounds, window);
                            }
                        },
                        |_, (), _, _| {},
                    )
                    .size_full(),
                ),
        )
        .with_animation(
            ("settings-search-flash", serial),
            Animation::new(FLASH),
            |el, t| el.bg(flash_color(t)),
        )
        .into_any_element()
}

/// The lit row's background at `t` (0 to 1) of [`FLASH`].
fn flash_color(t: f32) -> gpui::Rgba {
    let mut color = theme::signal_wash();
    let left = if t < FLASH_HOLD {
        1.
    } else {
        (1. - (t - FLASH_HOLD) / (1. - FLASH_HOLD)).max(0.)
    };
    color.a *= left;
    color
}

/// The rows matching `query`: every word of it is in the row's title or second line, in the UI
/// language or in English (`english`). Rows whose title has every word come first; otherwise the
/// order of `rows` is kept. A row shown twice on a page counts once.
fn matches(rows: &[Row], query: &str, english: &dyn Fn(&str) -> Vec<String>) -> Vec<Row> {
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let has_all = |text: &str| words.iter().all(|w| text.contains(w));
    let texts = |text: &str| {
        let mut out = text.to_lowercase();
        for en in english(text) {
            out.push('\n');
            out.push_str(&en);
        }
        out
    };
    let mut seen = std::collections::HashSet::new();
    let mut found: Vec<(bool, Row)> = Vec::new();
    for row in rows {
        if !seen.insert((row.page, row.title.clone())) {
            continue;
        }
        let title = texts(&row.title);
        if has_all(&title) {
            found.push((true, row.clone()));
            continue;
        }
        let mut all = title;
        if let Some(desc) = &row.desc {
            all.push('\n');
            all.push_str(&texts(desc));
        }
        if has_all(&all) {
            found.push((false, row.clone()));
        }
    }
    found.sort_by_key(|(in_title, _)| !in_title);
    found.into_iter().map(|(_, row)| row).collect()
}

/// The English text of `text` when it is one of Arke's strings in the UI language, lowercased
/// (one string can stand for several keys). Nothing while the UI is in English.
fn english(text: &str) -> Vec<String> {
    let locale = rust_i18n::locale();
    if locale.starts_with("en") {
        return Vec::new();
    }
    ENGLISH.with_borrow_mut(|cache| {
        if cache.as_ref().is_none_or(|(l, _)| *l != *locale) {
            // Once per language: a few thousand strings, well under a millisecond.
            let backend = crate::_rust_i18n_backend();
            let mut map = EnglishOf::new();
            for (key, shown) in backend.messages_for_locale(&locale).unwrap_or_default() {
                let Some(en) = backend.translate("en", &key) else {
                    continue;
                };
                if en != shown {
                    map.entry(shown.into_owned())
                        .or_default()
                        .push(en.to_lowercase());
                }
            }
            *cache = Some((locale.to_string(), map));
        }
        cache
            .as_ref()
            .and_then(|(_, map)| map.get(text).cloned())
            .unwrap_or_default()
    })
}

/// The hits in a list like a settings group: the page's icon, the setting, and the page it is on.
/// `cursor` is the hit Enter goes to, while the box has the focus.
fn results_list(
    hits: &[Row],
    pages: &[(SharedString, Icon)],
    cursor: Option<usize>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let page_name = |page: usize| pages.get(page).map_or_default(|(t, _)| t.clone());
    let announce = cursor
        .and_then(|ix| hits.get(ix))
        .map(|hit| crate::a11y::item_label(&hit.title, &[page_name(hit.page)]));
    let rows: Vec<AnyElement> = hits
        .iter()
        .enumerate()
        .map(|(ix, hit)| {
            let icon = pages.get(hit.page).map_or(Icon::Settings, |(_, i)| *i);
            hit_row(ix, hit, icon, page_name(hit.page), cursor == Some(ix), cx)
        })
        .collect();
    let empty = rows.is_empty();
    div()
        .id("settings-search-results")
        .debug_selector(|| "settings-search-results".to_owned())
        .role(gpui::Role::ListBox)
        .aria_label(t!("a11y.results"))
        .aria_size_of_set(rows.len())
        .a11y_announce(|| announce)
        .flex()
        .flex_col()
        .rounded(theme::r_8())
        .bg(theme::card())
        .border_1()
        .border_color(theme::line())
        .overflow_hidden()
        .when(empty, |d| {
            d.child(
                div()
                    .min_h(theme::s_32() + theme::s_12())
                    .px(theme::s_16())
                    .flex()
                    .items_center()
                    .text_color(theme::text_subtle())
                    .child(t!("list.no_match").into_owned()),
            )
        })
        .children(rows)
        .into_any_element()
}

/// One hit: clicking it goes there.
fn hit_row(
    ix: usize,
    hit: &Row,
    icon: Icon,
    page_name: SharedString,
    on: bool,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let title = hit.title.clone();
    let spoken = (title.clone(), page_name.clone());
    div()
        .id(("settings-search-hit", ix))
        .debug_selector(move || format!("settings-search-hit {ix}"))
        .a11y_lazy(gpui::Role::ListBoxOption, move || {
            crate::a11y::item_label(&spoken.0, &[spoken.1])
        })
        .aria_selected(on)
        .aria_position_in_set(ix)
        .when(ix > 0, |d| d.border_t_1().border_color(theme::line()))
        .min_h(theme::s_32() + theme::s_12())
        .px(theme::s_16())
        .py(theme::s_8())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .cursor_pointer()
        .when(on, |d| d.bg(theme::list_selection(true)))
        .when(!on, |d| d.hover(|s| s.bg(theme::wash())))
        .child(super::row_icon(icon, theme::text_muted()))
        .child(div().flex_1().min_w_0().truncate().child(title))
        .when(!hit.whole_page, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(page_name),
            )
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.settings_search_go(ix, window, cx);
        }))
        .into_any_element()
}

/// The box in the Settings title bar, drawn like the folder search box.
pub(super) fn search_box(
    search: &Search,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let focused = search.input.focus_handle(cx).is_focused(window);
    let typed = !search.input.read(cx).text().is_empty();
    div()
        .debug_selector(|| "settings-search".to_owned())
        .flex_shrink(1.)
        .w(theme::scaled_px(320.))
        .min_w(theme::scaled_px(140.))
        .h(theme::h_ctl())
        .flex()
        .items_center()
        .gap(theme::s_6())
        .pl(theme::s_12())
        .pr(theme::s_4())
        .rounded(theme::r_8())
        .border_1()
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(if typed {
                theme::line_strong()
            } else {
                theme::clear()
            })
        })
        .child(
            svg()
                .path(Icon::Search.path())
                .size(theme::icon())
                .flex_none()
                .text_color(if typed {
                    theme::text_muted()
                } else {
                    theme::text_subtle()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(search.input.clone()),
        )
        .when(typed, |d| d.child(clear_button(cx)))
        .into_any_element()
}

/// The × in the box: empties it, the cursor stays in it.
fn clear_button(cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .id("settings-search-clear")
        .debug_selector(|| "settings-search-clear".to_owned())
        .a11y_button(t!("a11y.clear_filter"))
        .size(theme::s_20())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
        .cursor_pointer()
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(Icon::Close.path())
                .size(theme::icon_sm())
                .text_color(theme::text_muted()),
        )
        .on_click(cx.listener(|this, _, window, cx| {
            let Some(page) = &mut this.settings_panel else {
                return;
            };
            let input = page.search.input.clone();
            page.search.cursor = 0;
            input.update(cx, |i, cx| i.set_text("", cx));
            window.focus(&input.focus_handle(cx), cx);
            cx.notify();
        }))
}

impl Workspace {
    /// Typing in the search box shows the hits; Enter goes to the one under the cursor; Esc
    /// empties the box, and in an empty box gives the focus back to the page.
    fn on_settings_search(
        &mut self,
        input: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = &mut self.settings_panel else {
            return;
        };
        match event {
            InputEvent::Changed => {
                page.search.cursor = 0;
                page.search.flash = None;
                // The hits start at the top, whatever the page was scrolled to.
                page.scroll.set_offset(point(px(0.), px(0.)));
            }
            InputEvent::Confirm => {
                let cursor = page.search.cursor;
                self.settings_search_go(cursor, window, cx);
                return;
            }
            InputEvent::Cancel => {
                page.search.cursor = 0;
                if input.read(cx).text().is_empty() {
                    window.focus(&page.focus, cx);
                } else {
                    input.update(cx, |i, cx| i.set_text("", cx));
                }
            }
        }
        cx.notify();
    }

    /// Ctrl+F (the key of "focus filter") on Settings: the cursor goes to the search box, its
    /// text selected, instead of to the folder's search box under the page.
    pub(super) fn focus_settings_search(
        &mut self,
        _: &crate::explorer::FocusFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = &self.settings_panel else {
            return;
        };
        // Not from under the "Remove Arke" dialog.
        if page.uninstall.is_some() {
            return;
        }
        let input = page.search.input.clone();
        window.focus(&input.focus_handle(cx), cx);
        input.update(cx, TextInput::select_all_text);
        cx.notify();
    }

    /// Up or Down with the cursor in the search box and hits shown: moves through them. `false`
    /// when the key isn't for the search.
    pub(super) fn settings_search_move(
        &mut self,
        down: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(page) = &mut self.settings_panel else {
            return false;
        };
        if !page.search.input.focus_handle(cx).is_focused(window)
            || page.search.query(cx).is_empty()
        {
            return false;
        }
        let last = page.search.hits.borrow().len().saturating_sub(1);
        let at = page.search.cursor.min(last);
        page.search.cursor = if down {
            (at + 1).min(last)
        } else {
            at.saturating_sub(1)
        };
        cx.notify();
        true
    }

    /// Goes to hit `ix`: shows its page with the row scrolled into view and lit, and empties the
    /// box.
    pub(super) fn settings_search_go(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = &mut self.settings_panel else {
            return;
        };
        let Some(hit) = page.search.hits.borrow().get(ix).cloned() else {
            return;
        };
        page.category = hit.page;
        page.dropdown = None;
        LAST_TOGGLE.set(None);
        page.scroll.set_offset(point(px(0.), px(0.)));
        page.search.jumps += 1;
        page.search.flash = (!hit.whole_page).then(|| Flash {
            page: hit.page,
            title: hit.title.clone(),
            serial: page.search.jumps,
        });
        REVEAL.set(page.search.flash.is_some());
        page.search.cursor = 0;
        page.search.hits.borrow_mut().clear();
        let input = page.search.input.clone();
        let focus = page.focus.clone();
        input.update(cx, |i, cx| i.set_text("", cx));
        window.focus(&focus, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn settings_search_for_test(&self) -> Option<Entity<TextInput>> {
        self.settings_panel.as_ref().map(|p| p.search.input.clone())
    }

    /// The hits as last drawn: each one's page and title.
    #[cfg(test)]
    pub(crate) fn settings_search_hits_for_test(&self) -> Vec<(usize, String)> {
        self.settings_panel.as_ref().map_or_default(|p| {
            p.search
                .hits
                .borrow()
                .iter()
                .map(|r| (r.page, r.title.to_string()))
                .collect()
        })
    }

    /// The page shown and the row a hit lit there, if any.
    #[cfg(test)]
    pub(crate) fn settings_search_target_for_test(&self) -> Option<(usize, Option<String>)> {
        let page = self.settings_panel.as_ref()?;
        Some((
            page.category,
            page.search.flash.as_ref().map(|f| f.title.to_string()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(page: usize, title: &str, desc: Option<&str>) -> Row {
        Row {
            page,
            title: SharedString::from(title.to_owned()),
            desc: desc.map(|d| SharedString::from(d.to_owned())),
            whole_page: false,
        }
    }

    fn titles(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.title.as_ref()).collect()
    }

    #[test]
    fn every_word_has_to_match_and_titles_come_first() {
        let rows = [
            row(0, "Theme", Some("Light or dark")),
            row(1, "Show hidden items", None),
            row(
                1,
                "Hide dotfiles",
                Some("Items whose name starts with a dot are hidden"),
            ),
            row(2, "Dark mode", None),
        ];
        let none = |_: &str| Vec::<String>::new();
        assert_eq!(
            titles(&matches(&rows, "dark", &none)),
            ["Dark mode", "Theme"]
        );
        assert_eq!(
            titles(&matches(&rows, "HIDDEN items", &none)),
            ["Show hidden items", "Hide dotfiles"]
        );
        assert_eq!(
            titles(&matches(&rows, "hidden theme", &none)),
            Vec::<&str>::new()
        );
        assert!(matches(&rows, "   ", &none).is_empty());
    }

    #[test]
    fn the_english_text_matches_too() {
        let rows = [row(1, "顯示隱藏的項目", None), row(1, "主題", None)];
        let english = |text: &str| {
            if text == "顯示隱藏的項目" {
                vec!["show hidden items".to_owned()]
            } else {
                Vec::new()
            }
        };
        assert_eq!(
            titles(&matches(&rows, "Hidden", &english)),
            ["顯示隱藏的項目"]
        );
        assert_eq!(
            titles(&matches(&rows, "隱藏", &english)),
            ["顯示隱藏的項目"]
        );
    }

    #[test]
    fn a_row_shown_twice_on_a_page_is_one_hit() {
        let rows = [
            row(1, "Fast copy", None),
            row(1, "Fast copy", None),
            row(2, "Fast copy", None),
        ];
        let hits = matches(&rows, "fast", &|_: &str| Vec::<String>::new());
        assert_eq!(
            hits.iter().map(|r| r.page).collect::<Vec<_>>(),
            [1, 2],
            "once per page"
        );
    }

    #[test]
    fn the_light_holds_then_fades_out() {
        let full = theme::signal_wash().a;
        assert!((flash_color(0.).a - full).abs() < f32::EPSILON);
        assert!((flash_color(FLASH_HOLD / 2.).a - full).abs() < f32::EPSILON);
        assert!(flash_color(0.7).a < full);
        assert!(flash_color(1.).a.abs() < f32::EPSILON);
    }
}
