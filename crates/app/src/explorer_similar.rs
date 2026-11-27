//! "Similar results" in search results (#396).
//!
//! Subfolder and "This PC" searches list the names that have the text as typed. Names that only
//! have its letters in order (fuzzy matches) wait behind one closed row after them, without a
//! count, and aren't looked for until it is opened: the usual search costs nothing more. When
//! nothing matches as typed, the similar names are listed right away under a note saying so. A
//! query starting with `'` asks for exact matches only and gets no such row.
//!
//! - The hits keep the exact ones first and the similar ones last
//!   ([`arke_core::SearchResults::similar`]), in any sort order.
//! - The row is a list row, not a hit: the menus, opening and the count in the status bar only see
//!   hits (so similar results left closed aren't counted). Rows and hits are converted for drawing
//!   and scrolling only, as with group headers (`explorer_groups.rs`).
//! - The keyboard stops on it like on a row (`similar_focus`, with no hit under the cursor): Up
//!   and Down go through it, Enter and Space open or close it, and the cursor stays on it, so Down
//!   then goes to the first similar hit. Closed, it is the last stop.

use std::sync::Arc;

use arke_core::index_query::{self as iq, NameMatch};
use arke_core::{SearchHit, SearchSort};
use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, ScrollStrategy, SharedString,
    Window, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{Explorer, Step};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme;

/// The extra row of a search's results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SimilarRow {
    /// After the exact hits, closed: similar names aren't looked for.
    Closed,
    /// After the exact hits, open: the similar hits follow it.
    Open,
    /// Nothing matched as typed: a note above the similar hits.
    Only,
}

/// `hits` in `sort` order, with the last `similar` of them (the similar matches) kept after the
/// others.
pub(crate) fn sort_hits(
    hits: &[Arc<SearchHit>],
    similar: usize,
    sort: SearchSort,
) -> Vec<Arc<SearchHit>> {
    let split = hits.len().saturating_sub(similar);
    let mut out = arke_core::sort_search_hits(&hits[..split], sort);
    out.extend(arke_core::sort_search_hits(&hits[split..], sort));
    out
}

/// Hit at list row `row` when the extra row is at `at`; `None` for the extra row itself.
pub(crate) const fn hit_at_row(row: usize, at: Option<usize>) -> Option<usize> {
    match at {
        Some(at) if row == at => None,
        Some(at) if row > at => Some(row - 1),
        _ => Some(row),
    }
}

/// List row of hit `ix` when the extra row is at `at`.
pub(crate) const fn row_of_hit(ix: usize, at: Option<usize>) -> usize {
    match at {
        Some(at) if ix >= at => ix + 1,
        _ => ix,
    }
}

impl Explorer {
    /// Whether similar results could add anything to the exact ones for the query typed.
    fn query_has_similar(&self) -> bool {
        // The clock only shifts date ranges, which doesn't change the name text.
        iq::has_similar(&iq::Query::parse(&self.query, iq::Clock::default()).text)
    }

    /// Whether the user opened similar results for the query typed.
    pub(super) fn similar_opened(&self) -> bool {
        self.similar_open.as_deref() == Some(self.query.as_str())
    }

    /// How the subfolder search matches names now: as typed, or fuzzily once similar results
    /// were opened or nothing matched as typed.
    pub(super) fn search_names(&self) -> NameMatch {
        let fallback = self.similar_fallback.as_deref() == Some(self.query.as_str());
        if self.similar_opened() || fallback {
            NameMatch::Similar
        } else {
            NameMatch::Exact
        }
    }

    /// The subfolder search's query.
    pub(super) fn search_query(&self) -> iq::Query {
        iq::Query {
            names: self.search_names(),
            ..iq::Query::parse(&self.query, crate::win::clock())
        }
    }

    /// The finished exact search found nothing in the subfolders: look for similar names instead.
    /// Returns whether it did (the empty result is then not shown).
    pub(super) fn fall_back_to_similar(&mut self, results: &arke_core::SearchResults) -> bool {
        if results.searching
            || !results.hits.is_empty()
            || self.search_names() != NameMatch::Exact
            || !self.query_has_similar()
        {
            return false;
        }
        self.similar_fallback = Some(self.query.clone());
        self.search.set_parsed(self.search_query());
        true
    }

    /// The extra row of the results on screen, with its list row; `None` when there is none.
    pub(crate) fn similar_row(&self) -> Option<(usize, SimilarRow)> {
        let results = self.search_results.as_ref().filter(|_| self.search_mode)?;
        let exact = results.hits.len().saturating_sub(results.similar);
        if exact == 0 {
            return (results.similar > 0).then_some((0, SimilarRow::Only));
        }
        if !self.query_has_similar() {
            return None;
        }
        let open = self.similar_opened() || results.similar > 0;
        Some((
            exact,
            if open {
                SimilarRow::Open
            } else {
                SimilarRow::Closed
            },
        ))
    }

    /// Whether the keyboard cursor is on "Similar results" (and the row is there to be on).
    pub(crate) fn similar_focused(&self) -> bool {
        self.similar_focus
            && matches!(
                self.similar_row(),
                Some((_, SimilarRow::Closed | SimilarRow::Open))
            )
    }

    /// Puts the keyboard cursor on "Similar results": no hit is under it then.
    fn focus_similar_row(&mut self, at: usize, cx: &mut Context<Self>) {
        self.search_cursor = None;
        self.similar_focus = true;
        self.search_scroll
            .scroll_to_item(at, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// A cursor key in search results that have "Similar results" to stop on: moves by list
    /// rows, the row included. Returns `false` (nothing done) without such a row, so the key
    /// moves by hits as before.
    pub(super) fn search_move(
        &mut self,
        step: Step,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.search_move_by(step, false, window, cx)
    }

    /// [`Self::search_move`] with Shift: landing on a hit extends the selection to it (#413).
    pub(super) fn search_extend_move(
        &mut self,
        step: Step,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.search_move_by(step, true, window, cx)
    }

    fn search_move_by(
        &mut self,
        step: Step,
        extend: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.search_mode || matches!(step, Step::Left | Step::Right) {
            return false;
        }
        let Some((at, SimilarRow::Closed | SimilarRow::Open)) = self.similar_row() else {
            return false;
        };
        let last = self.search_row_count().saturating_sub(1);
        let current = if self.similar_focused() {
            Some(at)
        } else {
            self.search_cursor.map(|ix| row_of_hit(ix, Some(at)))
        };
        let page = self.page_rows(window);
        let to = match step {
            Step::Prev => current.map_or(0, |r| r.saturating_sub(1)),
            Step::Next => current.map_or(0, |r| (r + 1).min(last)),
            Step::PageUp => current.map_or(0, |r| r.saturating_sub(page)),
            Step::PageDown => current.map_or(0, |r| (r + page).min(last)),
            Step::First => 0,
            Step::Last | Step::Left | Step::Right => last,
        };
        match hit_at_row(to, Some(at)) {
            None => self.focus_similar_row(at, cx),
            Some(ix) if extend => self.extend_index(ix, cx),
            Some(ix) => self.select_index(ix, cx),
        }
        true
    }

    /// Rows in the results list: the hits and the extra row.
    pub(crate) fn search_row_count(&self) -> usize {
        let hits = self.search_results.as_ref().map_or(0, |r| r.hits.len());
        hits + usize::from(self.similar_row().is_some())
    }

    /// List row of hit `ix` (for scrolling to it).
    pub(crate) fn search_row_of(&self, ix: usize) -> usize {
        row_of_hit(ix, self.similar_row().map(|(at, _)| at))
    }

    /// Opens or closes "Similar results" (a click on the row, or a screen reader's expand and
    /// collapse).
    pub(crate) fn toggle_similar(&mut self, cx: &mut Context<Self>) {
        match self.similar_row() {
            Some((_, SimilarRow::Closed)) => {
                self.similar_open = Some(self.query.clone());
                if self.search_global {
                    self.run_global(cx);
                } else if self.search_root.is_some() {
                    self.search.set_parsed(self.search_query());
                }
            }
            Some((_, SimilarRow::Open)) => {
                self.similar_open = None;
                self.drop_similar_hits();
                if self.search_global {
                    // A query in flight would bring them back.
                    self.global_serial += 1;
                    self.global_task = None;
                } else if self.search_root.is_some() {
                    self.search.set_parsed(self.search_query());
                }
            }
            Some((_, SimilarRow::Only)) | None => return,
        }
        cx.notify();
    }

    /// Takes the similar hits off the results on screen (closing them needs no search).
    fn drop_similar_hits(&mut self) {
        let Some(results) = self.search_results.as_mut() else {
            return;
        };
        if results.similar == 0 {
            return;
        }
        let exact = results.hits.len().saturating_sub(results.similar);
        results.hits = Arc::new(results.hits[..exact].to_vec());
        results.matched = u32::try_from(exact).unwrap_or(u32::MAX);
        results.similar = 0;
        // The relevance order has the exact ones first too.
        if let Some(relevance) = self.search_relevance.take() {
            let kept = relevance[..exact.min(relevance.len())].to_vec();
            self.search_relevance = Some(Arc::new(kept));
        }
        self.search_cursor = self.search_cursor.and_then(|ix| {
            if ix < exact {
                Some(ix)
            } else {
                exact.checked_sub(1)
            }
        });
        self.sync_search_pick();
    }

    /// The extra row at list row `row`: "Similar results" with a chevron, opened and closed by a
    /// click, or the note that nothing matched as typed.
    pub(super) fn similar_row_element(
        &self,
        row: usize,
        kind: SimilarRow,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shell = div()
            .id(("similar-row", row))
            // As wide as the hit rows: Details rows end with their last column.
            .map(|d| match self.cols {
                Some(c) => d.w(c.row).flex_none(),
                None => d.w_full(),
            })
            .h(self.row_height())
            .px(theme::s_12())
            .flex()
            .items_center()
            .gap(theme::s_8())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, _| {
                    // Not a press on empty space: no marquee.
                    this.row_pressed = true;
                }),
            );
        if kind == SimilarRow::Only {
            let note = t!("list.similar_only").into_owned();
            return shell
                .a11y(gpui::Role::Heading, note.clone())
                .aria_level(1)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_muted())
                        .child(note),
                )
                .into_any_element();
        }
        let open = kind == SimilarRow::Open;
        // Under the cursor it looks and reads like a hit under the cursor (search rows show the
        // cursor as the selection), with the same inset highlight.
        let on = self.similar_focused();
        let group = SharedString::from(format!("similar-row-{row}"));
        let wash = super::view::row_wash(row, &group, on, false, focused, None);
        shell
            .a11y_item(row, on, on, || t!("list.similar").into_owned())
            .a11y_open_close(open, cx.entity().downgrade(), |this, _, cx| {
                this.toggle_similar(cx);
            })
            .group(group)
            .relative()
            .cursor_pointer()
            .child(wash)
            .child(
                div()
                    .size(theme::tree_chevron())
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .path(Icon::ChevronRight.path())
                            .size(theme::icon_sm())
                            .text_color(theme::text_subtle())
                            .with_transformation(gpui::Transformation::rotate(gpui::radians(
                                if open {
                                    std::f32::consts::FRAC_PI_2
                                } else {
                                    0.
                                },
                            ))),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs_14())
                    .text_color(theme::text_muted())
                    .child(t!("list.similar").into_owned()),
            )
            .child(div().flex_1().h(px(1.)).bg(theme::line()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                window.focus(&this.list_focus, cx);
                // The cursor goes there, as on a clicked hit.
                this.focus_similar_row(row, cx);
                this.toggle_similar(cx);
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The extra row sits between hits: rows and hits convert both ways around it.
    #[test]
    fn rows_skip_the_similar_row() {
        // Two exact hits, the row, then similar hits.
        assert_eq!(hit_at_row(0, Some(2)), Some(0));
        assert_eq!(hit_at_row(1, Some(2)), Some(1));
        assert_eq!(hit_at_row(2, Some(2)), None);
        assert_eq!(hit_at_row(3, Some(2)), Some(2));
        assert_eq!(row_of_hit(1, Some(2)), 1);
        assert_eq!(row_of_hit(2, Some(2)), 3);
        // The note above similar hits only.
        assert_eq!(hit_at_row(0, Some(0)), None);
        assert_eq!(hit_at_row(1, Some(0)), Some(0));
        assert_eq!(row_of_hit(0, Some(0)), 1);
        // No row.
        assert_eq!(hit_at_row(5, None), Some(5));
        assert_eq!(row_of_hit(5, None), 5);
    }
}
