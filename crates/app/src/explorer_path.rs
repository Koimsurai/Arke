//! Path bar: breadcrumbs, the › and "…" folder menus, and autocomplete while typing a path.
//!
//! - The bar is as wide as its segments (with a minimum, so a short path is still easy to hit); the
//!   rest of the title bar drags the window and double click maximizes it.
//! - A segment navigates; the blank end of the bar starts editing (also Ctrl+L, Alt+D).
//! - A › between segments lists the folders in the level to its left (current one highlighted) for
//!   jumping to siblings; the trailing › lists subfolders. The collapsed "…" lists the hidden
//!   levels.
//! - While typing, matching folders show below: Up/Down to pick, Tab to complete, Enter to go.
//!
//! Folder listing always runs on a background thread; results carry a request id and stale ones are
//! dropped.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arke_core::NavKind;
use arke_core::entry::Entry;
use arke_core::filetype::FileKind;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, FocusHandle, KeyBinding, MouseDownEvent, Pixels,
    Point, SharedString, Window, actions, anchored, canvas, deferred, div, point, prelude::*, px,
    svg,
};
use rust_i18n::t;

use super::{AppEvent, Explorer, kind_icon, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme::{self, UiFont as _};

actions!(explorer, [PathUp, PathDown, PathAccept, PathClose]);

/// Empty space kept at the end of the path bar so there's always somewhere to click to edit the
/// path (the breadcrumbs collapse before eating into it).
const PATH_BLANK_MIN: Pixels = px(40.);
/// Max autocomplete entries
const MAX_SUGGESTIONS: usize = 8;
/// Folder menu width
const POPUP_W: Pixels = px(260.);
const POPUP_MAX_H: Pixels = px(360.);

pub(super) fn bind_keys(cx: &mut App) {
    let bar = Some("PathBar");
    let popup = Some("PathPopup");
    cx.bind_keys([
        KeyBinding::new("up", PathUp, bar),
        KeyBinding::new("down", PathDown, bar),
        KeyBinding::new("tab", PathAccept, bar),
        KeyBinding::new("up", PathUp, popup),
        KeyBinding::new("down", PathDown, popup),
        KeyBinding::new("enter", PathAccept, popup),
        KeyBinding::new("escape", PathClose, popup),
    ]);
}

/// Folder menu opened from › or "…".
pub(crate) struct PathPopup {
    position: Point<Pixels>,
    /// None while reading
    items: Option<Vec<(String, PathBuf)>>,
    /// The entry on the current path (highlighted)
    current: Option<PathBuf>,
    highlighted: Option<usize>,
    /// Opened from the trailing › (that button draws as open)
    tail: bool,
    request: u64,
    focus: FocusHandle,
}

/// Autocomplete while typing a path.
#[derive(Default)]
pub(crate) struct Suggest {
    items: Vec<(String, PathBuf)>,
    highlighted: Option<usize>,
    /// Last listed folder, so typing doesn't reread on every keystroke. Whole entries are kept
    /// (not just names) so the hidden rules are applied at match time and a "Show hidden items"
    /// toggle while typing takes effect without relisting.
    listed: Option<(PathBuf, Arc<Vec<Entry>>)>,
    request: u64,
}

/// Path bar bounds after layout (menus align to it).
pub(crate) type PathBounds = Rc<Cell<Bounds<Pixels>>>;
/// Width of the title bar slot holding the path bar and the drag area after it, after layout (the
/// breadcrumbs collapse to fit it).
pub(crate) type SlotWidth = Rc<Cell<Pixels>>;

fn next_request() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Everything in `dir`, or a server's shares for `\\nas` (no folder there to list). Blocks; call
/// from a background thread.
pub(super) fn list_blocking(
    fs: &dyn arke_core::Fs,
    probe: Option<&dyn arke_core::net::NetProbe>,
    dir: &Path,
) -> std::io::Result<Vec<Entry>> {
    match probe.zip(arke_core::net::share_server(dir)) {
        Some((probe, server)) => probe
            .shares(server)
            .map(|shares| arke_core::net::share_entries(&shares)),
        None => fs
            .read_dir(dir)
            .map(|iter| iter.filter_map(Result::ok).collect()),
    }
}

/// Split input into the folder to list and the name prefix. `C:\Users\Pu` -> (`C:\Users\`, `Pu`).
fn split_input(base: &Path, text: &str) -> Option<(PathBuf, String)> {
    let text = text.trim().trim_matches('"').replace('/', "\\");
    if text.is_empty() {
        return None;
    }
    // Drive letter only: list the drive root.
    if text.len() == 2 && text.ends_with(':') {
        return Some((PathBuf::from(format!("{text}\\")), String::new()));
    }
    match text.rfind('\\') {
        Some(i) => {
            let dir = arke_core::path::resolve_input(base, &text[..=i])?;
            Some((dir, text[i + 1..].to_owned()))
        }
        None => Some((base.to_path_buf(), text)),
    }
}

/// Prefix matches first, then substring matches; exact matches are skipped (already typed).
///
/// `names` pairs each folder name with whether the list would hide it (see [`Entry::concealed`]).
/// Hidden folders are offered only when the typed prefix starts with `$` or `.` and the name starts
/// with it, so typing `$Win` still completes `$WinREAgent` the way File Explorer lets you type a
/// hidden name, while an empty or ordinary prefix never surfaces them.
fn matching<'a>(names: impl IntoIterator<Item = (&'a str, bool)>, prefix: &str) -> Vec<String> {
    let lower = prefix.to_lowercase();
    let reveal = lower.starts_with(['$', '.']);
    let mut starts: Vec<&str> = Vec::new();
    let mut contains: Vec<&str> = Vec::new();
    for (name, concealed) in names {
        let n = name.to_lowercase();
        if n == lower {
            continue;
        }
        if concealed {
            if reveal && n.starts_with(&lower) {
                starts.push(name);
            }
            continue;
        }
        if n.starts_with(&lower) {
            starts.push(name);
        } else if !lower.is_empty() && n.contains(&lower) {
            contains.push(name);
        }
    }
    starts
        .into_iter()
        .chain(contains)
        .take(MAX_SUGGESTIONS)
        .map(str::to_owned)
        .collect()
}

impl Explorer {
    // ───────────────────────── Folder menus (› and "…") ─────────────────────────

    /// List folders in `dir` in the background. Results go to `handle_path_folders`.
    /// Lists the folders in `dir` on a background thread. Hidden ones are kept: the › menu and
    /// autocomplete each apply the list's hidden rules when they use the result.
    fn list_folders(&self, dir: PathBuf, request: u64) {
        let fs = self.services.fs.clone();
        let probe = self.services.shell.net_probe();
        let tx = self.events_tx.clone();
        spawn_thread("path-folders", move || {
            let mut folders: Vec<Entry> = list_blocking(&*fs, probe.as_deref(), &dir)
                .unwrap_or_default()
                .into_iter()
                .filter(Entry::is_dir_like)
                .collect();
            folders.sort_by(|a, b| arke_core::sort::natural_cmp(&a.sort_key, &b.sort_key));
            let _ = tx.send(AppEvent::PathFolders {
                request,
                dir,
                folders,
            });
        });
    }

    fn open_path_popup(
        &mut self,
        items: Option<Vec<(String, PathBuf)>>,
        current: Option<PathBuf>,
        tail: bool,
        at: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let request = next_request();
        let bar = self.path_bounds.get();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let highlighted = items
            .as_ref()
            .and_then(|items| items.iter().position(|(_, p)| Some(p) == current.as_ref()));
        self.path_popup = Some(PathPopup {
            position: point(at - theme::S_8, bar.bottom() + theme::S_4),
            items,
            current,
            highlighted,
            tail,
            request,
            focus,
        });
        cx.notify();
        request
    }

    /// › clicked: list folders in `dir` with `current` highlighted.
    fn show_folders(
        &mut self,
        dir: PathBuf,
        current: Option<PathBuf>,
        at: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tail = current.is_none();
        let request = self.open_path_popup(None, current, tail, at, window, cx);
        self.list_folders(dir, request);
    }

    pub(super) fn close_path_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_popup.take().is_some() {
            window.focus(&self.list_focus, cx);
            cx.notify();
        }
    }

    pub(super) fn handle_path_folders(
        &mut self,
        request: u64,
        dir: &Path,
        folders: Vec<Entry>,
        cx: &mut Context<Self>,
    ) {
        let show_hidden = self.show_hidden;
        if let Some(popup) = self.path_popup.as_mut().filter(|p| p.request == request) {
            // Same rules as the list, so protected system folders and dot folders follow their
            // settings here too.
            let items: Vec<(String, PathBuf)> = folders
                .iter()
                .filter(|e| !e.concealed(show_hidden))
                .map(|e| {
                    let name = e.name.to_string();
                    let path = dir.join(&name);
                    (name, path)
                })
                .collect();
            popup.highlighted = items
                .iter()
                .position(|(_, p)| Some(p) == popup.current.as_ref());
            popup.items = Some(items);
            cx.notify();
        } else if self.suggest.request == request {
            self.suggest.listed = Some((dir.to_path_buf(), Arc::new(folders)));
            self.refresh_suggestions(cx);
        }
    }

    fn popup_go(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .path_popup
            .as_ref()
            .and_then(|p| p.items.as_ref()?.get(ix).map(|(_, path)| path.clone()));
        self.close_path_popup(window, cx);
        if let Some(target) = target {
            self.navigate(target, NavKind::New, cx);
        }
    }

    // ───────────────────────── Autocomplete ─────────────────────────

    /// Input changed: filter the cached listing right away, or list in the background if not
    /// cached.
    pub(super) fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().to_owned();
        let expanded = self.services.shell.expand_env(&text);
        let base = self
            .display_path()
            .map_or_else(|| PathBuf::from("C:\\"), Path::to_path_buf);
        let Some((dir, prefix)) = split_input(&base, &expanded) else {
            self.suggest.items.clear();
            self.suggest.highlighted = None;
            cx.notify();
            return;
        };
        match &self.suggest.listed {
            Some((listed, folders)) if *listed == dir => {
                let show_hidden = self.show_hidden;
                let names = folders.iter().map(|e| (&*e.name, e.concealed(show_hidden)));
                self.suggest.items = matching(names, &prefix)
                    .into_iter()
                    .map(|n| {
                        let path = dir.join(&n);
                        (n, path)
                    })
                    .collect();
                self.suggest.highlighted = None;
            }
            _ => {
                self.suggest.items.clear();
                self.suggest.highlighted = None;
                self.suggest.request = next_request();
                self.list_folders(dir, self.suggest.request);
            }
        }
        cx.notify();
    }

    pub(super) fn clear_suggestions(&mut self) {
        self.suggest.items.clear();
        self.suggest.highlighted = None;
        self.suggest.request = 0;
    }

    /// Selected suggestion (Enter goes there).
    pub(super) fn chosen_suggestion(&self) -> Option<PathBuf> {
        let ix = self.suggest.highlighted?;
        self.suggest.items.get(ix).map(|(_, p)| p.clone())
    }

    /// Tab: complete the selected suggestion (or the first) and append `\` for the next level.
    fn complete_suggestion(&mut self, cx: &mut Context<Self>) {
        let ix = self.suggest.highlighted.unwrap_or(0);
        let Some((_, path)) = self.suggest.items.get(ix).cloned() else {
            return;
        };
        let mut text = path.display().to_string();
        if !text.ends_with('\\') {
            text.push('\\');
        }
        self.path_input
            .update(cx, |input, cx| input.set_text(text, cx));
        self.refresh_suggestions(cx);
    }

    // ───────────────────────── Keyboard ─────────────────────────

    fn move_path_highlight(&mut self, forward: bool, cx: &mut Context<Self>) {
        let (len, current) = match &self.path_popup {
            Some(p) => (p.items.as_ref().map_or(0, Vec::len), p.highlighted),
            None => (self.suggest.items.len(), self.suggest.highlighted),
        };
        if len == 0 {
            return;
        }
        let next = match (current, forward) {
            (None, true) => 0,
            (None, false) => len - 1,
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
        };
        match self.path_popup.as_mut() {
            Some(p) => p.highlighted = Some(next),
            None => self.suggest.highlighted = Some(next),
        }
        cx.notify();
    }

    pub(super) fn path_up(&mut self, _: &PathUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_path_highlight(false, cx);
    }

    pub(super) fn path_down(&mut self, _: &PathDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_path_highlight(true, cx);
    }

    pub(super) fn path_accept(
        &mut self,
        _: &PathAccept,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.path_popup.as_ref().and_then(|p| p.highlighted) {
            Some(ix) => self.popup_go(ix, window, cx),
            None if self.path_popup.is_none() => self.complete_suggestion(cx),
            None => {}
        }
    }

    pub(super) fn path_close(
        &mut self,
        _: &PathClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_path_popup(window, cx);
    }

    // ───────────────────────── Rendering ─────────────────────────

    /// Segments to draw (index, name and path; None is the collapsed "…"), total count, and the
    /// collapsed segments.
    ///
    /// The whole path shows whenever it fits; only when it doesn't do levels collapse into "…",
    /// from the middle outwards (see [`super::layout::collapse_crumbs`]), keeping the drive and
    /// the current folder.
    #[allow(clippy::type_complexity)]
    fn shown_crumbs(
        &self,
        window: &Window,
    ) -> (
        Vec<(usize, Option<(String, PathBuf)>)>,
        usize,
        Vec<(String, PathBuf)>,
    ) {
        let crumbs = self
            .display_path()
            .map_or_default(arke_core::path::breadcrumbs);
        let total = crumbs.len();
        let Some(hide) = self.crumbs_to_hide(&crumbs, window) else {
            let shown = crumbs
                .into_iter()
                .enumerate()
                .map(|(i, c)| (i, Some(c)))
                .collect();
            return (shown, total, Vec::new());
        };
        let hidden = crumbs[hide.clone()].to_vec();
        let mut shown = Vec::with_capacity(total - hidden.len() + 1);
        for (i, crumb) in crumbs.into_iter().enumerate() {
            if i == hide.start {
                shown.push((i, None));
            }
            if !hide.contains(&i) {
                shown.push((i, Some(crumb)));
            }
        }
        (shown, total, hidden)
    }

    /// Levels to collapse so the breadcrumbs fit the path bar as laid out on the last frame
    /// (`None`: everything fits, or the bar hasn't been measured yet).
    fn crumbs_to_hide(
        &self,
        crumbs: &[(String, PathBuf)],
        window: &Window,
    ) -> Option<std::ops::Range<usize>> {
        let slot = self.path_slot.get();
        if slot <= px(0.) {
            return None;
        }
        // The drag area kept after the bar, the bar's margin and padding, the trailing › and the
        // blank area for clicking to edit.
        let avail = slot - self.path_drag_min() - theme::S_4 * 3. - theme::S_20 - PATH_BLANK_MIN;
        let text_w = |text: &str, size: Pixels, weight: gpui::FontWeight| {
            let run = gpui::TextRun {
                len: text.len(),
                font: theme::ui_font_at(weight),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(SharedString::from(text.to_owned()), size, &[run], None)
                .width
        };
        let pad = theme::S_6 * 2.;
        let last = crumbs.len().saturating_sub(1);
        let widths: Vec<Pixels> = crumbs
            .iter()
            .enumerate()
            .map(|(i, (label, _))| {
                // Same sizes as `crumb_piece`: the current folder is larger and semibold.
                let (size, weight) = if i == last {
                    (theme::FS_15, theme::FW_STRONG)
                } else {
                    (theme::FS_14, gpui::FontWeight::NORMAL)
                };
                let chevron = if i > 0 { theme::S_20 } else { px(0.) };
                chevron + text_w(label, size, weight) + pad
            })
            .collect();
        let more = theme::S_20 + text_w("…", theme::FS_14, gpui::FontWeight::NORMAL) + pad;
        super::layout::collapse_crumbs(&widths, avail, more)
    }

    /// Narrowest window drag area kept after the path bar (the sidebar top bar drags too).
    pub(super) fn path_drag_min(&self) -> Pixels {
        if self.pane.split {
            theme::S_8
        } else {
            theme::S_32 * 2.
        }
    }

    /// The path bar's slot in the title bar. The bar is as wide as its breadcrumbs (at least
    /// [`theme::PATH_MIN_W`]; segments collapse once they would crowd out the drag area) and the
    /// rest of the slot is `drag`, which moves the window. While typing a path the bar fills the
    /// slot.
    pub(super) fn render_path_slot(
        &mut self,
        drag: gpui::Stateful<gpui::Div>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = self.path_slot.clone();
        // The breadcrumbs collapse based on this width: when it changes, draw once more with the
        // new value (the slot's width doesn't depend on its content, so this settles at once).
        let capture = canvas(
            move |b, window, _| {
                if slot.replace(b.size.width) != b.size.width {
                    window.refresh();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full();
        let drag_min = self.path_drag_min();
        // While typing, the bar takes the space and the drag area keeps only its minimum.
        let drag = if self.editing_path {
            drag.flex_none().w(drag_min)
        } else {
            drag.flex_1().min_w(drag_min)
        };
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .child(capture)
            .child(self.render_path_bar(window, cx))
            .child(drag.h_full())
            .into_any_element()
    }

    fn render_path_bar(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let bounds = self.path_bounds.clone();
        // Menus and autocomplete line up with the bar; its width follows the path, so draw once
        // more when it changes.
        let capture = canvas(
            move |b, window, _| {
                if bounds.replace(b) != b {
                    window.refresh();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full();
        let bar = div()
            .id("path-bar")
            .a11y(gpui::Role::Toolbar, t!("a11y.address"))
            .relative()
            .min_w_0()
            .h(theme::H_CTL)
            .flex()
            .items_center()
            .ml(theme::S_4)
            .px(theme::S_4)
            .rounded(theme::R_8)
            .child(capture);
        if self.editing_path {
            return bar
                .key_context("PathBar")
                .on_action(cx.listener(Self::path_up))
                .on_action(cx.listener(Self::path_down))
                .on_action(cx.listener(Self::path_accept))
                .flex_1()
                .px(theme::S_8)
                .bg(theme::paper())
                .border_1()
                .border_color(theme::signal_line())
                .child(div().flex_1().min_w_0().child(self.path_input.clone()))
                .into_any_element();
        }
        let crumbs = self.crumb_elements(window, cx);
        let current_dir = self.display_path().map(Path::to_path_buf);
        // Trailing ›: subfolders of the current folder
        let tail_open = self.path_popup.as_ref().is_some_and(|p| p.tail);
        let tail = current_dir.map(|dir| {
            chevron_button("crumb-tail", tail_open).on_click(cx.listener(
                move |this, e: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.show_folders(dir.clone(), None, e.position().x, window, cx);
                },
            ))
        });
        // Sized to the breadcrumbs, but never so narrow that a short path is hard to hit.
        let min_w = theme::PATH_MIN_W
            .min(self.path_slot.get() - self.path_drag_min() - theme::S_4)
            .max(px(0.));
        bar.min_w(min_w)
            .cursor_text()
            .hover(|s| s.bg(theme::wash()))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.start_edit_path(window, cx);
            }))
            // Right click: copy address, copy path, edit address, clear history
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, e: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_path_menu(e.position, window, cx);
                }),
            )
            .child(div().flex().items_center().min_w_0().children(crumbs))
            .children(tail)
            // Tooltip only on empty space, so it doesn't cover the menus from segments and ›.
            .child(
                div()
                    .id("path-blank")
                    .a11y_button(t!("path.edit_tip"))
                    .flex_1()
                    .min_w(PATH_BLANK_MIN)
                    .h_full()
                    .tooltip(|_, cx| {
                        cx.new(|_| crate::workspace::Tooltip(t!("path.edit_tip").into_owned().into()))
                            .into()
                    }),
            )
            .into_any_element()
    }

    /// Path segments, each preceded by ›.
    fn crumb_elements(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (shown, total, hidden) = self.shown_crumbs(window);
        let last = total.saturating_sub(1);
        let popup_parent = self.path_popup.as_ref().and_then(|p| p.current.clone());
        shown
            .into_iter()
            .map(|(i, crumb)| {
                let is_last = i == last;
                // The › before a segment lists the folders of the level to its left, with this one
                // highlighted.
                let here = crumb
                    .as_ref()
                    .map(|(_, p)| p.clone())
                    .or_else(|| hidden.first().map(|(_, p)| p.clone()));
                let chevron = (i > 0).then(|| {
                    let open = here.is_some() && popup_parent == here;
                    chevron_button(("crumb-sep", i), open).on_click(cx.listener({
                        let here = here.clone();
                        move |this, e: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            if let Some(here) = here.clone()
                                && let Some(parent) = here.parent().map(Path::to_path_buf)
                            {
                                this.show_folders(parent, Some(here), e.position().x, window, cx);
                            }
                        }
                    }))
                });
                let piece = match crumb {
                    // Collapsed levels: list them for direct jumps.
                    None => more_piece(i, hidden.clone(), cx),
                    Some((label, target)) => crumb_piece(i, label, target, is_last, cx),
                };
                div()
                    .flex()
                    .items_center()
                    .when(is_last, gpui::Styled::flex_none)
                    .when(!is_last, gpui::Styled::min_w_0)
                    .children(chevron)
                    .child(piece)
                    .into_any_element()
            })
            .collect()
    }

    /// Overlays below the path bar: the › folder menu or autocomplete while typing.
    pub(super) fn render_path_overlays(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(popup) = &self.path_popup {
            return Some(Self::render_path_popup(popup, cx));
        }
        if self.editing_path && !self.suggest.items.is_empty() {
            return Some(self.render_suggestions(cx));
        }
        None
    }

    fn render_path_popup(popup: &PathPopup, cx: &mut Context<Self>) -> AnyElement {
        let body: AnyElement = match &popup.items {
            None => note(t!("info.loading").into_owned()),
            Some(items) if items.is_empty() => note(t!("path.no_folders").into_owned()),
            Some(items) => div()
                .id("path-popup-list")
                .flex()
                .flex_col()
                .max_h(POPUP_MAX_H)
                .overflow_y_scroll()
                .children(items.iter().enumerate().map(|(ix, (name, path))| {
                    let current = popup.current.as_ref() == Some(path);
                    folder_row(
                        ("path-popup-row", ix),
                        name.clone(),
                        popup.highlighted == Some(ix),
                        current,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.popup_go(ix, window, cx);
                        },
                    ))
                }))
                .into_any_element(),
        };
        deferred(
            anchored()
                .position(popup.position)
                .snap_to_window_with_margin(theme::S_8)
                .child(crate::motion::appear(
                    menu_panel("path-popup")
                        .key_context("PathPopup")
                        .track_focus(&popup.focus)
                        .on_action(cx.listener(Self::path_up))
                        .on_action(cx.listener(Self::path_down))
                        .on_action(cx.listener(Self::path_accept))
                        .on_action(cx.listener(Self::path_close))
                        .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.close_path_popup(window, cx);
                        }))
                        .w(POPUP_W)
                        .child(body),
                    "path-popup-appear",
                    0.,
                    -crate::motion::NUDGE,
                )),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> AnyElement {
        let bar = self.path_bounds.get();
        let rows = self
            .suggest
            .items
            .iter()
            .enumerate()
            .map(|(ix, (name, path))| {
                let path = path.clone();
                folder_row(
                    ("path-suggest", ix),
                    name.clone(),
                    self.suggest.highlighted == Some(ix),
                    false,
                )
                // Navigate on mouse down; on click the input has already lost focus and the list is
                // gone.
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.stop_edit_path(window, cx);
                        this.navigate(path.clone(), NavKind::New, cx);
                    }),
                )
            });
        deferred(
            anchored()
                .position(point(bar.left(), bar.bottom() + theme::S_4))
                .snap_to_window_with_margin(theme::S_8)
                .child(
                    menu_panel("path-suggest-panel")
                        .w(bar.size.width.max(POPUP_W))
                        .children(rows)
                        .child(note(t!("path.suggest_hint").into_owned())),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}

/// The collapsed "…": click to list the hidden levels and jump to one.
fn more_piece(i: usize, hidden: Vec<(String, PathBuf)>, cx: &mut Context<Explorer>) -> AnyElement {
    div()
        .id(("crumb-more", i))
        .a11y_button(t!("a11y.more_folders"))
        .flex_none()
        .px(theme::S_6)
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_color(theme::text_subtle())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child("…")
        .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.open_path_popup(
                Some(hidden.clone()),
                None,
                false,
                e.position().x,
                window,
                cx,
            );
        }))
        .into_any_element()
}

fn crumb_piece(
    i: usize,
    label: String,
    target: PathBuf,
    is_last: bool,
    cx: &mut Context<Explorer>,
) -> AnyElement {
    // Hovering here during a drag opens it after a delay (see `spring.rs`).
    let spring_key = format!("crumb:{}", target.display());
    let spring_bar = crate::spring::bar(&spring_key, cx);
    let me = cx.entity().downgrade();
    div()
        .id(("crumb", i))
        .a11y_lazy(gpui::Role::Link, || label.clone())
        .relative()
        .px(theme::S_6)
        .py(theme::S_2)
        .rounded(theme::R_4)
        .cursor_pointer()
        .whitespace_nowrap()
        // When space runs out earlier segments truncate first; the current folder truncates last.
        .when(!is_last, |d| d.min_w_0().truncate())
        .when(is_last, |d| {
            d.text_size(theme::FS_15)
                .font_weight(theme::FW_STRONG)
                .text_color(theme::text())
        })
        .when(!is_last, |d| d.text_color(theme::text_muted()))
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .can_drop({
            let check = target.clone();
            move |value, _, cx| super::accepts_drop(value, &check, cx)
        })
        .drag_over::<super::DraggedFiles>(|s, _, _, _| s.bg(theme::signal_wash()))
        .on_drag_move({
            let dest = target.clone();
            move |e: &gpui::DragMoveEvent<super::DraggedFiles>, window, cx| {
                super::report_drop_target(&dest, e, cx);
                // No need to open the current folder again.
                if !is_last {
                    crate::spring::hover(
                        spring_key.clone(),
                        crate::spring::Action::Open(dest.clone()),
                        crate::spring::Target::Explorer(me.clone()),
                        e,
                        window,
                        cx,
                    );
                }
            }
        })
        .on_drop({
            let dest = target.clone();
            move |dragged: &super::DraggedFiles, window, cx| {
                super::drop_into(dragged, dest.clone(), window, cx);
            }
        })
        .child(label)
        .children(spring_bar)
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            this.navigate(target.clone(), NavKind::New, cx);
        }))
        .into_any_element()
}

/// The › between segments: clickable (lists folders) and rotates to ⌄ while open.
fn chevron_button(id: impl Into<gpui::ElementId>, open: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_button(t!("a11y.subfolders"))
        .aria_expanded(open)
        .flex_none()
        .size(theme::S_20)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .cursor_pointer()
        .when(open, |d| d.bg(theme::wash_strong()))
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(Icon::ChevronRight.path())
                .size(theme::ICON_SM)
                .text_color(if open {
                    theme::text()
                } else {
                    theme::text_subtle()
                })
                .with_transformation(gpui::Transformation::rotate(gpui::radians(if open {
                    std::f32::consts::FRAC_PI_2
                } else {
                    0.
                }))),
        )
}

fn menu_panel(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Menu)
        .occlude()
        .flex()
        .flex_col()
        .py(theme::S_4)
        .rounded(theme::R_12)
        .bg(theme::overlay())
        .shadow(theme::elev_float())
        .ui_font()
        .text_size(theme::FS_14)
        .text_color(theme::text())
}

fn folder_row(
    id: impl Into<gpui::ElementId>,
    name: String,
    highlighted: bool,
    current: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_lazy(gpui::Role::MenuItem, || name.clone())
        .when(
            highlighted,
            gpui::StatefulInteractiveElement::aria_active_descendant,
        )
        .h(theme::S_32)
        .mx(theme::S_4)
        .px(theme::S_8)
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::S_8)
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(highlighted, |d| d.bg(theme::wash_strong()))
        .when(!highlighted, |d| d.hover(|s| s.bg(theme::wash())))
        .child(kind_icon(FileKind::Folder, theme::ICON))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(current, |d| d.font_weight(theme::FW_STRONG))
                .child(SharedString::from(name)),
        )
}

fn note(text: String) -> AnyElement {
    div()
        .px(theme::S_12)
        .py(theme::S_6)
        .text_size(theme::FS_12)
        .text_color(theme::text_subtle())
        .child(text)
        .into_any_element()
}

#[cfg(test)]
impl Explorer {
    /// Tests: current autocomplete entries.
    pub(crate) fn suggestion_names(&self) -> Vec<String> {
        self.suggest.items.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Tests: path input text.
    pub(crate) fn path_text(&self, cx: &App) -> String {
        self.path_input.read(cx).text().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_typed_paths() {
        let base = Path::new(r"C:\home");
        assert_eq!(
            split_input(base, r"C:\Users\al"),
            Some((PathBuf::from(r"C:\Users\"), "al".to_owned()))
        );
        assert_eq!(
            split_input(base, "D:"),
            Some((PathBuf::from(r"D:\"), String::new()))
        );
        assert_eq!(
            split_input(base, "docs"),
            Some((PathBuf::from(r"C:\home"), "docs".to_owned()))
        );
        assert_eq!(split_input(base, "  "), None);
    }

    #[test]
    fn prefix_matches_come_first() {
        let names = ["Downloads", "Documents", "My Docs", "docs"].map(|n| (n, false));
        assert_eq!(
            matching(names, "do"),
            ["Downloads", "Documents", "docs", "My Docs"]
        );
        // Exact matches are skipped (already typed).
        assert_eq!(matching(names, "docs"), ["My Docs"]);
        assert_eq!(matching(names, "").len(), 4);
    }

    #[test]
    fn hidden_folders_need_a_typed_hidden_prefix() {
        // (name, hidden under the list's current rules)
        let names = [
            ("$Recycle.Bin", true),
            ("$WinREAgent", true),
            ("$Visible", false),
            (".git", true),
            ("ProgramData", true),
            ("Program Files", false),
            ("Windows", false),
        ];
        // Nothing typed or an ordinary prefix: hidden folders stay out, as in the list.
        assert_eq!(
            matching(names, ""),
            ["$Visible", "Program Files", "Windows"]
        );
        assert_eq!(matching(names, "pro"), ["Program Files"]);
        // A substring match never reveals a hidden folder.
        assert!(matching(names, "data").is_empty());
        // Typing the start of a hidden name that begins with $ or . completes it.
        assert_eq!(matching(names, "$win"), ["$WinREAgent"]);
        assert_eq!(
            matching(names, "$"),
            ["$Recycle.Bin", "$WinREAgent", "$Visible"]
        );
        assert_eq!(matching(names, ".g"), [".git"]);
    }

    #[test]
    fn hidden_rules_follow_the_list() {
        use arke_core::entry::EntryKind;
        // Hidden, and Hidden + System (protected), as Windows reports them.
        let hidden = Entry::new("ProgramData", EntryKind::Dir, 0, None, 0x2);
        let protected = Entry::new("System Volume Information", EntryKind::Dir, 0, None, 0x6);
        let plain = Entry::new("Users", EntryKind::Dir, 0, None, 0);
        let names = |show_hidden: bool, show_protected: bool| {
            let flags: Vec<(&str, bool)> = [&hidden, &protected, &plain]
                .iter()
                .map(|e| {
                    (
                        &*e.name,
                        e.concealed_with(show_hidden, true, show_protected),
                    )
                })
                .collect();
            matching(flags, "")
        };
        assert_eq!(names(false, false), ["Users"]);
        // "Show hidden items" alone still keeps protected system folders out.
        assert_eq!(names(true, false), ["ProgramData", "Users"]);
        assert_eq!(
            names(true, true),
            ["ProgramData", "System Volume Information", "Users"]
        );
    }
}
