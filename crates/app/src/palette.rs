//! Ctrl+T: "go to" (like Arc's new tab bar).
//!
//! Candidates are recent folders, known locations and open tabs, fuzzy-matched as paths
//! (`arke_core::filter::rank_paths`, pure computation). When the query looks like a path (`:`, `\`,
//! `%`, `~`, ...), the first row is "go to this path". The filesystem is never touched; navigation
//! handles missing paths.
//!
//! Enter opens a new tab (or switches to an already open one); Shift+Enter opens in a split beside
//! the current tab; Esc closes.
//!
//! Finding files (including this PC) is the search box's job (`explorer_scope.rs`); this is
//! locations only.

use rust_i18n::t;
use std::path::PathBuf;
use std::sync::Arc;

use arke_core::filter::rank_paths;
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    KeyBinding, SharedString, StyledText, Subscription, Window, actions, div, prelude::*, px, svg,
};

use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::shell::Shell;
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

actions!(palette, [PaletteUp, PaletteDown, PaletteSplit]);

const CONTEXT: &str = "LocationPalette";
/// Max rows
const MAX_ROWS: usize = 8;

pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", PaletteUp, ctx),
        KeyBinding::new("down", PaletteDown, ctx),
        KeyBinding::new("shift-enter", PaletteSplit, ctx),
    ]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Path(PathBuf),
    /// Switch to an open tab
    Tab(usize),
    /// Open a saved workspace by name
    Workspace(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Current,
    Typed,
    Tab,
    Recent,
    Place,
    Workspace,
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) label: String,
    /// The location's own icon (Desktop, Downloads, ...); falls back to the source's icon
    pub(crate) icon: Option<Icon>,
    pub(crate) target: Target,
    pub(crate) source: Source,
}

#[derive(Clone, Debug)]
pub(crate) enum PaletteEvent {
    Chosen { target: Target, split: bool },
    Dismissed,
}

impl EventEmitter<PaletteEvent> for LocationPalette {}

#[derive(Clone)]
struct Row {
    candidate: Candidate,
    highlights: Vec<std::ops::Range<usize>>,
}

pub(crate) struct LocationPalette {
    input: Entity<TextInput>,
    current: PathBuf,
    candidates: Vec<Candidate>,
    shell: Arc<dyn Shell>,
    rows: Vec<Row>,
    pub(crate) highlighted: usize,
    _subscriptions: Vec<Subscription>,
}

impl LocationPalette {
    pub(crate) fn new(
        current: PathBuf,
        candidates: Vec<Candidate>,
        shell: Arc<dyn Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new(t!("palette.placeholder").to_string(), cx));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Changed => this.refresh(cx),
                InputEvent::Confirm => this.choose(false, window, cx),
                InputEvent::Cancel => cx.emit(PaletteEvent::Dismissed),
            });
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let mut this = Self {
            input,
            current,
            candidates,
            shell,
            rows: Vec::new(),
            highlighted: 0,
            _subscriptions: vec![subscription],
        };
        this.refresh(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn row_labels(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|r| r.candidate.label.clone())
            .collect()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).committed_text();
        let query = query.trim();
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push(Row {
                candidate: Candidate {
                    label: self.current.display().to_string(),
                    icon: None,
                    target: Target::Path(self.current.clone()),
                    source: Source::Current,
                },
                highlights: Vec::new(),
            });
        } else if looks_like_path(query) {
            let expanded = self.shell.expand_env(query);
            if let Some(path) = arke_core::path::resolve_input(&self.current, &expanded) {
                rows.push(Row {
                    candidate: Candidate {
                        label: path.display().to_string(),
                        icon: None,
                        target: Target::Path(path),
                        source: Source::Typed,
                    },
                    highlights: Vec::new(),
                });
            }
        }
        let labels: Vec<String> = self.candidates.iter().map(|c| c.label.clone()).collect();
        rows.extend(
            rank_paths(&labels, query)
                .into_iter()
                .map(|m| Row {
                    candidate: self.candidates[m.index].clone(),
                    highlights: m.highlights,
                })
                .take(MAX_ROWS.saturating_sub(rows.len())),
        );
        self.rows = rows;
        self.highlighted = 0;
        cx.notify();
    }

    fn choose(&mut self, split: bool, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(self.highlighted) else {
            return;
        };
        // In a split, "switch to open tab" makes no sense; open the same folder beside it instead.
        let target = row.candidate.target.clone();
        cx.emit(PaletteEvent::Chosen { target, split });
    }

    fn up(&mut self, _: &PaletteUp, _: &mut Window, cx: &mut Context<Self>) {
        self.highlighted = self.highlighted.saturating_sub(1);
        cx.notify();
    }

    fn down(&mut self, _: &PaletteDown, _: &mut Window, cx: &mut Context<Self>) {
        self.highlighted = (self.highlighted + 1).min(self.rows.len().saturating_sub(1));
        cx.notify();
    }

    fn split(&mut self, _: &PaletteSplit, window: &mut Window, cx: &mut Context<Self>) {
        self.choose(true, window, cx);
    }

    fn render_row(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let on = ix == self.highlighted;
        let (icon, tag) = match row.candidate.source {
            Source::Current => (Icon::Plus, t!("palette.current")),
            Source::Typed => (Icon::ArrowRight, t!("palette.typed")),
            Source::Tab => (Icon::ViewList, t!("palette.tab")),
            Source::Recent => (Icon::Refresh, t!("palette.recent")),
            Source::Place => (Icon::Drive, t!("palette.place")),
            Source::Workspace => (Icon::Workspaces, t!("palette.workspace")),
        };
        let icon = crate::assets::tinted(
            row.candidate.icon.unwrap_or(icon),
            theme::ICON,
            theme::text_muted(),
        );
        let mark = HighlightStyle {
            color: Some(theme::signal().into()),
            font_weight: Some(theme::FW_STRONG),
            ..HighlightStyle::default()
        };
        let (name, parent) = row_text(row, mark);
        let height = theme::S_32 + theme::S_4;
        div()
            .id(("palette-row", ix))
            .a11y_lazy(gpui::Role::ListBoxOption, || {
                crate::a11y::item_label(&row.candidate.label, &[tag.as_ref()])
            })
            .aria_selected(on)
            .aria_position_in_set(ix)
            .relative()
            .h(height)
            .mx(theme::S_4)
            .px(theme::S_12)
            .flex()
            .items_center()
            .gap(theme::S_12)
            .rounded(theme::R_8)
            .cursor_pointer()
            // The panel owns the keyboard, so the highlighted row looks like a focused list's
            // selection: the accent wash.
            .when(on, |d| d.bg(theme::list_selection(true)))
            .when(!on, |d| d.hover(|s| s.bg(theme::wash())))
            .child(icon)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(theme::S_8)
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .font_weight(theme::FW_STRONG)
                            .child(name),
                    )
                    .children(parent.map(|parent| {
                        // Parent folder: dimmer than the name and truncated first when space is
                        // short (keeping the end).
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .text_size(theme::FS_12)
                            .text_color(theme::text_subtle())
                            .child(parent)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(tag.to_string()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.highlighted = ix;
                this.choose(false, window, cx);
            }))
    }
}

impl LocationPalette {
    /// Footer: key hints, plus a pointer to where file search lives.
    fn render_footer() -> impl IntoElement {
        let hint = |key: &'static str, text: String| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(theme::S_6)
                .child(
                    div()
                        .px(theme::S_4)
                        .rounded(theme::R_4)
                        .border_1()
                        .border_color(theme::line_strong())
                        .child(key),
                )
                .child(text)
        };
        div()
            .h(theme::S_32 + theme::S_4)
            .px(theme::S_16)
            .flex()
            .items_center()
            .gap(theme::S_16)
            .border_t_1()
            .border_color(theme::line())
            .text_size(theme::FS_12)
            .text_color(theme::text_subtle())
            .child(hint("Enter", t!("palette.hint_open").to_string()))
            .child(hint("Shift Enter", t!("palette.hint_split").to_string()))
            .child(hint("Esc", t!("palette.hint_close").to_string()))
            // File search is in the search box at the top right.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .child(t!("palette.find_files_hint").to_string()),
            )
    }
}

/// Row text: path split into bold name plus dim parent folder, so the folder is recognizable
/// without scanning a long path.
fn row_text(row: &Row, mark: HighlightStyle) -> (StyledText, Option<StyledText>) {
    let label = &row.candidate.label;
    let (name_at, parent_end) = split_label(label);
    let parent = (parent_end > 0).then(|| {
        let mut text = label[..parent_end].to_owned();
        // "C:" back to "C:\"
        if text.ends_with(':') {
            text.push(std::path::MAIN_SEPARATOR);
        }
        let marks = clip(&row.highlights, 0..parent_end);
        StyledText::new(SharedString::from(text))
            .with_highlights(marks.into_iter().map(|r| (r, mark)))
    });
    (
        styled(label, name_at..label.len(), &row.highlights, mark),
        parent,
    )
}

/// Byte offset where the name starts and where the parent ends (separator excluded). Drive roots
/// (`C:\`) and single-segment text are all name.
fn split_label(label: &str) -> (usize, usize) {
    let trimmed = label.trim_end_matches(['\\', '/']);
    match trimmed.rfind(['\\', '/']) {
        Some(i) if i + 1 < trimmed.len() => (i + 1, i),
        _ => (0, 0),
    }
}

/// Keep only highlights inside `range`, rebased to `range.start`.
fn clip(
    highlights: &[std::ops::Range<usize>],
    range: std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    highlights
        .iter()
        .filter_map(|r| {
            let start = r.start.max(range.start);
            let end = r.end.min(range.end);
            (start < end).then(|| start - range.start..end - range.start)
        })
        .collect()
}

/// `text[range]` with match highlights.
fn styled(
    text: &str,
    range: std::ops::Range<usize>,
    highlights: &[std::ops::Range<usize>],
    mark: HighlightStyle,
) -> StyledText {
    let marks = clip(highlights, range.clone());
    StyledText::new(SharedString::from(text[range].to_owned()))
        .with_highlights(marks.into_iter().map(move |r| (r, mark)))
}

/// Query looks like a path: absolute, env var, home, relative, or containing separators.
fn looks_like_path(query: &str) -> bool {
    if arke_core::index_query::has_filter_syntax(query) {
        return false;
    }
    query.contains(':')
        || query.contains('\\')
        || query.contains('/')
        || query.starts_with(['%', '~', '.'])
}

impl Focusable for LocationPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for LocationPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| self.render_row(ix, row, cx).into_any_element())
            .collect();
        let count = self.rows.len();
        div()
            .id("location-palette")
            .a11y(gpui::Role::Dialog, t!("palette.placeholder"))
            .a11y_announce(|| {
                let row = self.rows.get(self.highlighted)?;
                Some(row.candidate.label.clone())
            })
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::split))
            .w(px(640.))
            .flex()
            .flex_col()
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .overflow_hidden()
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            // Clicks on the panel don't close it (only the backdrop does).
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(52.))
                    .px(theme::S_16)
                    .flex()
                    .items_center()
                    .gap(theme::S_12)
                    .text_size(theme::FS_15)
                    .border_b_1()
                    .border_color(theme::line())
                    .child(
                        svg()
                            .path(Icon::Search.path())
                            .size(theme::ICON)
                            .flex_none()
                            .text_color(theme::text_subtle()),
                    )
                    .child(div().flex_1().min_w_0().child(self.input.clone())),
            )
            .child(
                div()
                    .id("palette-results")
                    .role(gpui::Role::ListBox)
                    .aria_size_of_set(count)
                    .py(theme::S_4)
                    .flex()
                    .flex_col()
                    .when(rows.is_empty(), |d| {
                        d.child(
                            div()
                                .h(theme::S_32 + theme::S_4)
                                .px(theme::S_16)
                                .flex()
                                .items_center()
                                .text_color(theme::text_subtle())
                                .child(t!("palette.no_match").to_string()),
                        )
                    })
                    .children(rows),
            )
            .child(Self::render_footer())
    }
}

#[cfg(test)]
mod label_tests {
    use super::{clip, split_label};

    #[test]
    fn splits_paths_into_name_and_parent() {
        let label = r"C:\Users\alex\Downloads";
        let (name, parent) = split_label(label);
        assert_eq!(&label[name..], "Downloads");
        assert_eq!(&label[..parent], r"C:\Users\alex");
        // Drive root and trailing separator
        assert_eq!(split_label(r"C:\"), (0, 0));
        let label = r"C:\Users\";
        let (name, parent) = split_label(label);
        assert_eq!((&label[name..], &label[..parent]), (r"Users\", "C:"));
    }

    #[test]
    fn clips_highlights_to_a_part() {
        let clipped = clip(&[0..2, 5..9, 12..13], 4..10);
        assert_eq!(clipped.len(), 1);
        assert_eq!(clipped.first(), Some(&(1..5)));
        assert_eq!(clip(&[3..6, 20..21], 4..10).first(), Some(&(0..2)));
    }
}
