//! Path bar: breadcrumbs, the › and "…" folder menus, and autocomplete while typing a path.
//!
//! - The bar sits in each pane's first row, between Up and the view buttons (see
//!   `explorer_topbar.rs`).
//! - A segment navigates; a click on the blank end of the bar starts editing (also Ctrl+L, Alt+D).
//! - A › between segments lists the folders in the level to its left (current one in bold) for
//!   jumping to siblings; the trailing › lists subfolders. The collapsed "…" lists the hidden
//!   levels. The › before the first segment lists Home, Documents, every drive with its free space
//!   and the network shares opened before (#354). The menu opens under the bar, or above it when
//!   there is no room below.
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
    SharedString, Window, actions, anchored, canvas, deferred, div, point, prelude::*, px, svg,
};
use rust_i18n::t;

use super::{AppEvent, Explorer, kind_icon, spawn_thread};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::theme::{self, UiFont as _};

actions!(explorer, [PathUp, PathDown, PathAccept, PathClose]);

/// Empty space kept at the end of the path bar so there's always somewhere to click to edit the
/// path (the breadcrumbs collapse before eating into it).
fn path_blank_min() -> gpui::Pixels {
    crate::theme::scaled_px(40.)
}
/// How much more readily the levels above the current folder shrink than the current folder
/// itself, once collapsing levels into "…" isn't enough (a single very long name).
const CRUMBS_GIVE_WAY: f32 = 1000.;
/// Max autocomplete entries
const MAX_SUGGESTIONS: usize = 8;
/// Folder menu width
fn popup_w() -> gpui::Pixels {
    crate::theme::scaled_px(260.)
}
/// The drive menu is wider: its rows also show free space.
fn roots_popup_w() -> gpui::Pixels {
    crate::theme::scaled_px(320.)
}
fn popup_max_h() -> gpui::Pixels {
    crate::theme::scaled_px(360.)
}

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

/// Which button opened the path bar's menu (that button draws as open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    /// A › between segments (the folders of a level), or "…" (the collapsed levels).
    Levels,
    /// The trailing › (subfolders of the current folder).
    Tail,
    /// The › before the first segment: Home, Documents, the drives and network shares (#354).
    Roots,
}

/// What a menu row opens.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Go {
    Folder(PathBuf),
    /// Arke's Home page (the workspace opens it).
    Home,
}

/// A row of the › and "…" menus.
#[derive(Clone, Debug)]
struct PopupItem {
    name: String,
    go: Go,
    /// `None`: the folder icon.
    icon: Option<Icon>,
    /// Muted text at the end of the row (a drive's free space).
    detail: Option<String>,
    /// Starts a group: a line above it.
    group: bool,
}

impl PopupItem {
    const fn folder(name: String, path: PathBuf) -> Self {
        Self {
            name,
            go: Go::Folder(path),
            icon: None,
            detail: None,
            group: false,
        }
    }

    fn path(&self) -> Option<&Path> {
        match &self.go {
            Go::Folder(path) => Some(path),
            Go::Home => None,
        }
    }
}

/// Folder menu opened from › or "…".
pub(crate) struct PathPopup {
    /// What it opens under (logical pixels): the bar's height, from just left of the › pressed,
    /// so it flips above the bar rather than over it when there is no room below.
    control: Bounds<Pixels>,
    /// None while reading
    items: Option<Vec<PopupItem>>,
    /// The entry on the current path (in bold)
    current: Option<PathBuf>,
    /// The keyboard row, on the current entry until an arrow key moves it.
    highlighted: Option<usize>,
    /// Whether the keyboard row is lit: once an arrow key moved it, or when opened with the keys.
    /// Opened with the mouse, the row under the pointer is the only one lit.
    keyboard: bool,
    menu: Menu,
    request: u64,
    focus: FocusHandle,
    /// The window it is drawn in (`popup.rs`).
    popup: crate::popup::Single,
}

/// The › or "…" menu a press outside it closed: which one (`menu`, `current`) and where the
/// press was. The release of that press on the button that opened it would open it again; it
/// stays closed instead, as a menu does when its button is pressed again.
pub(crate) struct Dismissed {
    menu: Menu,
    current: Option<PathBuf>,
    at: gpui::Point<Pixels>,
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
    if let Some((probe, server)) = probe.zip(arke_core::net::share_server(dir)) {
        return probe
            .shares(server)
            .map(|shares| arke_core::net::share_entries(&shares));
    }
    if let Some(Err(err)) = probe.and_then(|probe| probe.reconnect(dir)) {
        return Err(err);
    }
    fs.read_dir(dir)
        .map(|iter| iter.filter_map(Result::ok).collect())
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
        let fs = Arc::clone(&self.services.fs);
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

    #[allow(
        clippy::too_many_arguments,
        reason = "what the menu lists, where it opens and how it was opened"
    )]
    fn open_path_popup(
        &mut self,
        items: Option<Vec<PopupItem>>,
        current: Option<PathBuf>,
        menu: Menu,
        at: Pixels,
        keys: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let request = next_request();
        let bar = self.path_bounds.get();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let highlighted = items
            .as_deref()
            .and_then(|items| current_row(items, current.as_deref()));
        self.path_popup = Some(PathPopup {
            control: Bounds::from_corners(
                point(at - theme::s_8(), bar.top()),
                point(at + theme::s_8(), bar.bottom()),
            ),
            items,
            current,
            highlighted,
            keyboard: keys,
            menu,
            request,
            focus,
            popup: crate::popup::Single::new(),
        });
        cx.notify();
        request
    }

    /// › clicked (with the keys: `keys`): list folders in `dir` with `current` in bold.
    pub(crate) fn show_folders(
        &mut self,
        dir: PathBuf,
        current: Option<PathBuf>,
        at: Pixels,
        keys: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = if current.is_none() {
            Menu::Tail
        } else {
            Menu::Levels
        };
        let request = self.open_path_popup(None, current, menu, at, keys, window, cx);
        self.list_folders(dir, request);
    }

    /// The first segment of the current path (the drive, or `\\server`).
    fn current_root(&self) -> Option<PathBuf> {
        self.display_path()
            .map_or_default(arke_core::path::breadcrumbs)
            .into_iter()
            .next()
            .map(|(_, path)| path)
    }

    /// The › before the first segment clicked (with the keys: `keys`): Home, Documents, every
    /// drive with its free space, and the network shares opened before, the drive the path is on
    /// in bold. All of it is already loaded (the sidebar's places), so nothing is read here.
    pub(crate) fn show_roots(
        &mut self,
        at: Pixels,
        keys: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.root_items(cx);
        let current = self.current_root();
        self.open_path_popup(Some(items), current, Menu::Roots, at, keys, window, cx);
    }

    fn root_items(&self, cx: &mut App) -> Vec<PopupItem> {
        use crate::shell::{KnownFolder, PlaceKind};
        let mut items = vec![PopupItem {
            name: t!("home.title").into_owned(),
            go: Go::Home,
            icon: Some(Icon::KfHome),
            detail: None,
            group: false,
        }];
        if let Some(docs) = self
            .places
            .iter()
            .find(|p| p.kind == PlaceKind::Folder(KnownFolder::Documents))
        {
            items.push(PopupItem {
                icon: Some(Icon::KfDocuments),
                ..PopupItem::folder(docs.label.clone(), docs.path.clone())
            });
        }
        let mut group = true;
        for place in &self.places {
            let PlaceKind::Drive { remote, space } = place.kind else {
                continue;
            };
            let detail = space.filter(|&(_, total)| total > 0).map(|(free, total)| {
                t!(
                    "home.space",
                    free = arke_core::format::format_size(free),
                    total = arke_core::format::format_size(total)
                )
                .into_owned()
            });
            items.push(PopupItem {
                icon: Some(if remote { Icon::NetDrive } else { Icon::Drive }),
                detail,
                group: std::mem::take(&mut group),
                ..PopupItem::folder(
                    crate::place_names::place_label(&self.place_names, place),
                    place.path.clone(),
                )
            });
        }
        // Network: the shares opened before, by name, as the sidebar's Network lists them.
        let mut shares = crate::settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .net_places
            .clone();
        shares.sort_by_cached_key(|s| s.to_string_lossy().to_lowercase());
        let mut group = true;
        for share in shares {
            let name = crate::place_names::shown_name(&self.place_names, &share)
                .unwrap_or_else(|| crate::place_names::share_label(&share));
            items.push(PopupItem {
                icon: Some(Icon::NetDrive),
                group: std::mem::take(&mut group),
                ..PopupItem::folder(name, share)
            });
        }
        items
    }

    /// Whether click `e` on the button of the menu (`menu`, `current`) releases the press that
    /// just closed that same menu: then it stays closed.
    fn reopens_dismissed(&mut self, menu: Menu, current: Option<&Path>, e: &ClickEvent) -> bool {
        let dismissed = self.path_popup_dismissed.take();
        match (dismissed, e) {
            (Some(d), ClickEvent::Mouse(click)) => {
                d.menu == menu && d.current.as_deref() == current && d.at == click.down.position
            }
            _ => false,
        }
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
            let items: Vec<PopupItem> = folders
                .iter()
                .filter(|e| !e.concealed(show_hidden))
                .map(|e| {
                    let name = e.name.to_string();
                    let path = dir.join(&name);
                    PopupItem::folder(name, path)
                })
                .collect();
            popup.highlighted = current_row(&items, popup.current.as_deref());
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
            .and_then(|p| p.items.as_ref()?.get(ix).map(|item| item.go.clone()));
        self.close_path_popup(window, cx);
        match target {
            Some(Go::Folder(target)) => self.navigate(target, NavKind::New, cx),
            Some(Go::Home) => cx.emit(super::ExplorerEvent::OpenHome),
            None => {}
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
        // Completes what was typed: Ctrl+Z takes it back to that.
        self.path_input
            .update(cx, |input, cx| input.replace_text(text, cx));
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
            Some(p) => {
                p.highlighted = Some(next);
                p.keyboard = true;
            }
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
    #[allow(
        clippy::type_complexity,
        reason = "crumbs with their collapsed levels, taken apart right at the call"
    )]
    fn shown_crumbs(
        &self,
        compact: bool,
        window: &Window,
    ) -> (
        Vec<(usize, Option<(String, PathBuf)>)>,
        usize,
        Vec<(String, PathBuf)>,
    ) {
        let mut crumbs = self
            .display_path()
            .map_or_default(arke_core::path::breadcrumbs);
        // A renamed drive or network place shows its name.
        for (label, path) in &mut crumbs {
            if let Some(name) = crate::place_names::shown_name(&self.place_names, path) {
                *label = name;
            }
        }
        let total = crumbs.len();
        // Too narrow even for "… › current folder" (a split pane in a small window): just the
        // current folder. Up still goes up.
        let slot = self.path_slot.get();
        if compact && total > 1 && slot > px(0.) && slot < theme::scaled_px(140.) {
            let hidden = crumbs[..total - 1].to_vec();
            let current = crumbs.into_iter().last();
            return (vec![(total - 1, current)], total, hidden);
        }
        let Some(hide) = self.crumbs_to_hide(&crumbs, compact, window) else {
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

    /// Tests: the labels of the breadcrumbs as drawn.
    #[cfg(test)]
    pub(crate) fn crumb_labels_for_test(&self, window: &Window) -> Vec<String> {
        let (shown, ..) = self.shown_crumbs(false, window);
        shown
            .into_iter()
            .filter_map(|(_, crumb)| crumb.map(|(label, _)| label))
            .collect()
    }

    /// Levels to collapse so the breadcrumbs fit the path bar as laid out on the last frame
    /// (`None`: everything fits, or the bar hasn't been measured yet). `compact`: the bar is
    /// shrunk for the search box, so only the current folder shows.
    fn crumbs_to_hide(
        &self,
        crumbs: &[(String, PathBuf)],
        compact: bool,
        window: &Window,
    ) -> Option<std::ops::Range<usize>> {
        let slot = self.path_slot.get();
        if slot <= px(0.) {
            return None;
        }
        // Shrunk for the search box: just "… › current folder".
        if compact && crumbs.len() > 1 {
            return Some(0..crumbs.len() - 1);
        }
        // The bar's padding, the leading and trailing › and the blank area for clicking to edit.
        let avail = slot - theme::s_4() * 2. - theme::s_20() * 2. - path_blank_min();
        let text_w = |text: &str, size: Pixels, weight: gpui::FontWeight| {
            super::top_bar::text_width(text, size, weight, window)
        };
        let pad = theme::s_6() * 2.;
        let last = crumbs.len().saturating_sub(1);
        let widths: Vec<Pixels> = crumbs
            .iter()
            .enumerate()
            .map(|(i, (label, _))| {
                // Same sizes as `crumb_piece`: the current folder is larger and semibold.
                let (size, weight) = if i == last {
                    (theme::fs_15(), theme::FW_STRONG)
                } else {
                    (theme::fs_14(), gpui::FontWeight::NORMAL)
                };
                let chevron = if i > 0 { theme::s_20() } else { px(0.) };
                chevron + text_w(label, size, weight) + pad
            })
            .collect();
        let more = theme::s_20() + text_w("…", theme::fs_14(), gpui::FontWeight::NORMAL) + pad;
        super::layout::collapse_crumbs(&widths, avail, more)
    }

    /// The path bar's slot in the pane row: the bar fills it. `compact`: the slot is narrow, so it
    /// shows only the current folder.
    pub(super) fn render_path_slot(
        &mut self,
        compact: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = Rc::clone(&self.path_slot);
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
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .child(capture)
            .child(self.render_path_bar(compact, window, cx))
            .into_any_element()
    }

    /// The › before the first segment: Home, Documents, the drives and network shares (#354).
    fn roots_chevron(open: bool, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        chevron("crumb-roots", open, t!("a11y.roots"))
            .debug_selector(|| "crumb-roots".to_owned())
            .a11y_on_expand(crumb_roots(cx.entity().downgrade()))
            .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                cx.stop_propagation();
                let root = this.current_root();
                if this.reopens_dismissed(Menu::Roots, root.as_deref(), e) {
                    return;
                }
                this.show_roots(e.position().x, e.is_keyboard(), window, cx);
            }))
    }

    /// The › after the last segment: the subfolders of the folder shown.
    fn tail_chevron(dir: PathBuf, open: bool, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        chevron_button("crumb-tail", open, Some(&dir))
            .a11y_on_expand(crumb_folders(
                cx.entity().downgrade(),
                Some(dir.clone()),
                None,
            ))
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if this.reopens_dismissed(Menu::Tail, None, e) {
                    return;
                }
                this.show_folders(
                    dir.clone(),
                    None,
                    e.position().x,
                    e.is_keyboard(),
                    window,
                    cx,
                );
            }))
    }

    fn render_path_bar(
        &mut self,
        compact: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bounds = Rc::clone(&self.path_bounds);
        // Menus and autocomplete line up with the bar: draw once more when it moves or resizes.
        let capture = canvas(
            move |b, window, _| {
                if bounds.replace(b) != b {
                    window.refresh();
                }
            },
            |_, (), _, _| {},
        )
        // Pinned to the corner: without insets it starts inside the bar's padding, and the
        // suggestions under the bar sat that far right of it.
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let bar = div()
            .id("path-bar")
            .debug_selector(|| "path-bar".to_owned())
            .a11y(gpui::Role::Toolbar, t!("a11y.address"))
            .relative()
            .flex_1()
            .min_w_0()
            .h(theme::h_ctl())
            .flex()
            .items_center()
            .px(theme::s_4())
            .rounded(theme::r_8())
            .child(capture);
        if self.editing_path {
            return bar
                .key_context("PathBar")
                .on_action(cx.listener(Self::path_up))
                .on_action(cx.listener(Self::path_down))
                .on_action(cx.listener(Self::path_accept))
                // A press anywhere else ends editing, as clicking the list does, even on parts that
                // don't take focus (header rows, the sidebar, the title row). Not while the input's
                // own edit menu is open: that press is for the menu.
                .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    if this.editing_path && !this.path_input.read(cx).menu_open() {
                        this.stop_edit_path(window, cx);
                    }
                }))
                .px(theme::s_8())
                .bg(theme::paper())
                .border_1()
                .border_color(theme::signal_line())
                .child(div().flex_1().min_w_0().child(self.path_input.clone()))
                .into_any_element();
        }
        let crumbs = self.crumb_elements(compact, window, cx);
        let current_dir = self.display_path().map(Path::to_path_buf);
        let open_menu = self.path_popup.as_ref().map(|p| p.menu);
        // Leading ›: Home, Documents, the drives and network shares (left out while the bar is
        // shrunk).
        let lead = (current_dir.is_some() && !compact)
            .then(|| Self::roots_chevron(open_menu == Some(Menu::Roots), cx));
        // Trailing ›: subfolders of the current folder (left out while the bar is shrunk).
        let tail_open = open_menu == Some(Menu::Tail);
        let tail = current_dir
            .filter(|_| !compact)
            .map(|dir| Self::tail_chevron(dir, tail_open, cx));
        // The bar fills its slot, so its highlight covers all of it.
        bar.cursor_text()
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
            .children(lead)
            .child(div().flex().items_center().min_w_0().children(crumbs))
            .children(tail)
            // The blank end: a click edits the path. Tooltip only here, so it doesn't cover the
            // menus from segments and ›.
            .child(
                div()
                    .id("path-blank")
                    .debug_selector(|| "path-blank".to_owned())
                    .a11y_button(t!("path.edit_tip"))
                    // The bar's own press handling starts the edit; a screen reader's Enter
                    // comes here.
                    .a11y_on_click(edit_path(cx.entity().downgrade()))
                    .flex_1()
                    .min_w(if compact { theme::s_8() } else { path_blank_min() })
                    .h_full()
                    .tooltip(|_, cx| {
                        cx.new(|_| crate::workspace::Tooltip(t!("path.edit_tip").into_owned().into()))
                            .into()
                    }),
            )
            .into_any_element()
    }

    /// Path segments, each preceded by ›.
    fn crumb_elements(
        &self,
        compact: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (shown, total, hidden) = self.shown_crumbs(compact, window);
        let last = total.saturating_sub(1);
        let popup_parent = self
            .path_popup
            .as_ref()
            .filter(|p| p.menu == Menu::Levels)
            .and_then(|p| p.current.clone());
        shown
            .into_iter()
            .enumerate()
            .map(|(pos, (i, crumb))| {
                let is_last = i == last;
                // The › before a segment lists the folders of the level to its left, with this one
                // highlighted.
                let here = crumb
                    .as_ref()
                    .map(|(_, p)| p.clone())
                    .or_else(|| hidden.first().map(|(_, p)| p.clone()));
                // Not before the first piece shown (the root, "…", or the current folder alone).
                let chevron = (pos > 0).then(|| {
                    let open = here.is_some() && popup_parent == here;
                    chevron_button(
                        ("crumb-sep", i),
                        open,
                        here.as_deref().and_then(Path::parent),
                    )
                    .a11y_on_expand(crumb_folders(cx.entity().downgrade(), None, here.clone()))
                    .on_click(cx.listener({
                        let here = here.clone();
                        move |this, e: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            if let Some(here) = here.clone()
                                && !this.reopens_dismissed(Menu::Levels, Some(&here), e)
                                && let Some(parent) = here.parent().map(Path::to_path_buf)
                            {
                                this.show_folders(
                                    parent,
                                    Some(here),
                                    e.position().x,
                                    e.is_keyboard(),
                                    window,
                                    cx,
                                );
                            }
                        }
                    }))
                });
                // Every level can shrink, so a name too long for the bar truncates instead of
                // running over what's next to it; the current folder gives way last. The "…"
                // never shrinks: squeezed, it would leave a stray ›.
                let shrink = match crumb {
                    None => 0.,
                    Some(_) if is_last => 1.,
                    Some(_) => CRUMBS_GIVE_WAY,
                };
                let piece = match crumb {
                    // Collapsed levels: list them for direct jumps.
                    None => more_piece(i, hidden.clone(), cx),
                    Some((label, target)) => crumb_piece(i, label, target, is_last, cx),
                };
                div()
                    .debug_selector(move || format!("crumb-{i}"))
                    .flex()
                    .items_center()
                    .min_w_0()
                    .flex_shrink(shrink)
                    .children(chevron)
                    .child(piece)
                    .into_any_element()
            })
            .collect()
    }

    /// Overlays below the path bar: the › folder menu or autocomplete while typing.
    pub(super) fn render_path_overlays(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.path_popup.is_some() {
            return self.render_path_popup(window, cx);
        }
        if self.editing_path && !self.suggest.items.is_empty() {
            return Some(self.render_suggestions(cx));
        }
        None
    }

    /// The › folder menu is drawn in a window of its own (`popup.rs`); what stays here is its
    /// keys and focus, and closing it on a press anywhere in the window.
    fn render_path_popup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let popup = self.path_popup.as_mut()?;
        let control = crate::popup::rect_to_screen(window, popup.control);
        popup.popup.ensure(
            || crate::popup::Anchor::Below {
                control,
                align_right: false,
            },
            |this: &mut Self| this.path_popup.as_mut().map(|p| &mut p.popup),
            Self::path_popup_panel,
            Self::close_path_popup,
            window,
            cx,
        );
        Some(
            div()
                .absolute()
                .size_0()
                .key_context("PathPopup")
                .track_focus(&popup.focus)
                .on_action(cx.listener(Self::path_up))
                .on_action(cx.listener(Self::path_down))
                .on_action(cx.listener(Self::path_accept))
                .on_action(cx.listener(Self::path_close))
                .on_mouse_down_out(cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    this.path_popup_dismissed = this.path_popup.as_ref().map(|p| Dismissed {
                        menu: p.menu,
                        current: p.current.clone(),
                        at: e.position,
                    });
                    this.close_path_popup(window, cx);
                }))
                .into_any_element(),
        )
    }

    /// The › folder menu's panel, drawn in its own window.
    fn path_popup_panel(
        &mut self,
        _: Pixels,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let popup = self.path_popup.as_ref()?;
        let owner = popup.popup.owner();
        let body: AnyElement = match &popup.items {
            None => note(t!("info.loading").into_owned()),
            Some(items) if items.is_empty() => note(t!("path.no_folders").into_owned()),
            Some(items) => div()
                .id("path-popup-list")
                .flex()
                .flex_col()
                .max_h(popup_max_h())
                .overflow_y_scroll()
                .children(items.iter().enumerate().flat_map(|(ix, item)| {
                    let current =
                        popup.current.is_some() && item.path() == popup.current.as_deref();
                    let icon = item.icon.map_or_else(
                        || kind_icon(FileKind::Folder, theme::icon()),
                        |icon| crate::assets::tinted(icon, theme::icon(), theme::text_muted()),
                    );
                    let line = (item.group && ix > 0).then(|| {
                        div()
                            .h(px(1.))
                            .my(theme::s_4())
                            .mx(theme::s_8())
                            .bg(theme::line())
                            .into_any_element()
                    });
                    let row = menu_row(
                        ("path-popup-row", ix),
                        item.name.clone(),
                        icon,
                        item.detail.clone(),
                        popup.keyboard && popup.highlighted == Some(ix),
                        current,
                    )
                    .on_click(crate::popup::listener_in(
                        owner,
                        cx,
                        move |this, _: &ClickEvent, window, cx| {
                            this.popup_go(ix, window, cx);
                        },
                    ))
                    .into_any_element();
                    line.into_iter().chain([row])
                }))
                .into_any_element(),
        };
        let roots = popup.menu == Menu::Roots;
        Some(crate::motion::appear(
            menu_panel("path-popup")
                .aria_label(if roots {
                    t!("a11y.roots")
                } else {
                    t!("a11y.subfolders")
                })
                .w(if roots { roots_popup_w() } else { popup_w() })
                .child(body),
            "path-popup-appear",
            0.,
            -crate::motion::NUDGE,
        ))
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
                .position(point(bar.left(), bar.bottom() + theme::s_4()))
                .snap_to_window_with_margin(theme::s_8())
                .child(
                    menu_panel("path-suggest-panel")
                        .w(bar.size.width.max(popup_w()))
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
        .px(theme::s_6())
        .rounded(theme::r_4())
        .cursor_pointer()
        .text_color(theme::text_subtle())
        .hover(|s| s.bg(theme::wash_strong()).text_color(theme::text()))
        .child("…")
        .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
            cx.stop_propagation();
            if this.reopens_dismissed(Menu::Levels, None, e) {
                return;
            }
            let items = hidden
                .iter()
                .map(|(name, path)| PopupItem::folder(name.clone(), path.clone()))
                .collect();
            this.open_path_popup(
                Some(items),
                None,
                Menu::Levels,
                e.position().x,
                e.is_keyboard(),
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
        .px(theme::s_6())
        .py(theme::s_2())
        .rounded(theme::r_4())
        .cursor_pointer()
        .whitespace_nowrap()
        // When space runs out earlier segments truncate first; the current folder truncates last.
        .min_w_0()
        .truncate()
        .when(is_last, |d| {
            d.text_size(theme::fs_15())
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
        // The folder's own menu, not the bar's (copy address, edit, ...) (#257).
        .on_mouse_down(
            gpui::MouseButton::Right,
            cx.listener({
                let target = target.clone();
                move |_, e: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(super::ExplorerEvent::FolderMenu {
                        path: target.clone(),
                        position: e.position,
                    });
                }
            }),
        )
        .on_click(cx.listener({
            let target = target.clone();
            move |this, e: &ClickEvent, _, cx| {
                cx.stop_propagation();
                // Ctrl: a new tab (in front with Shift too), as a link in a browser (#291).
                if e.modifiers().control {
                    cx.emit(super::ExplorerEvent::OpenInNewTab {
                        path: target.clone(),
                        activate: e.modifiers().shift,
                    });
                } else {
                    this.navigate(target.clone(), NavKind::New, cx);
                }
            }
        }))
        .on_mouse_down(
            gpui::MouseButton::Middle,
            cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                cx.emit(super::ExplorerEvent::OpenInNewTab {
                    path: target.clone(),
                    activate: false,
                });
            }),
        )
        .into_any_element()
}

/// What a screen reader's Enter does on the bar's blank end: edit the path, like Ctrl+L.
fn edit_path(view: gpui::WeakEntity<super::Explorer>) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let _ = view.update(cx, |this, cx| {
            this.focus_path_bar(&crate::explorer::FocusPathBar, window, cx);
        });
    }
}

/// What a screen reader's expand and collapse do on a ›: list the folders in `dir` (the trailing
/// ›), or next to `here` (the › before it), or close the list.
fn crumb_folders(
    view: gpui::WeakEntity<super::Explorer>,
    dir: Option<PathBuf>,
    here: Option<PathBuf>,
) -> impl Fn(bool, &mut Window, &mut App) + 'static {
    move |want, window, cx| {
        let _ = view.update(cx, |this, cx| {
            if !want {
                this.close_path_popup(window, cx);
            } else if let Some(dir) = dir.clone() {
                this.show_folders(dir, None, px(0.), true, window, cx);
            } else if let Some(here) = here.clone()
                && let Some(parent) = here.parent().map(Path::to_path_buf)
            {
                this.show_folders(parent, Some(here), px(0.), true, window, cx);
            }
        });
    }
}

/// What a screen reader's expand and collapse do on the leading ›: list the drives and places,
/// or close the list.
fn crumb_roots(
    view: gpui::WeakEntity<super::Explorer>,
) -> impl Fn(bool, &mut Window, &mut App) + 'static {
    move |want, window, cx| {
        let _ = view.update(cx, |this, cx| {
            if want {
                this.show_roots(px(0.), true, window, cx);
            } else {
                this.close_path_popup(window, cx);
            }
        });
    }
}

/// The › between segments: clickable (lists folders) and rotates to ⌄ while open.
fn chevron_button(
    id: impl Into<gpui::ElementId>,
    open: bool,
    of: Option<&Path>,
) -> gpui::Stateful<gpui::Div> {
    // Every › reads differently: "Subfolders of Users" (#322).
    // A drive root has no name of its own: "C:".
    let name = of.map(|p| {
        p.file_name().map_or_else(
            || p.to_string_lossy().trim_end_matches('\\').to_owned(),
            |n| n.to_string_lossy().into_owned(),
        )
    });
    let label = if let Some(name) = name {
        t!("a11y.subfolders_of", name = name)
    } else {
        t!("a11y.subfolders")
    };
    chevron(id, open, label)
}

/// A › of the path bar: rotates to ⌄ while its menu is open.
fn chevron(
    id: impl Into<gpui::ElementId>,
    open: bool,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .a11y_button(label)
        .aria_expanded(open)
        .flex_none()
        .size(theme::s_20())
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::r_4())
        .cursor_pointer()
        .when(open, |d| d.bg(theme::wash_strong()))
        .hover(|s| s.bg(theme::wash_strong()))
        .child(
            svg()
                .path(Icon::ChevronRight.path())
                .size(theme::icon_sm())
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
        .py(theme::s_4())
        .rounded(theme::r_12())
        .bg(theme::overlay())
        .shadow(theme::elev_menu())
        .ui_font()
        .text_size(theme::fs_14())
        .text_color(theme::text())
}

fn folder_row(
    id: impl Into<gpui::ElementId>,
    name: String,
    highlighted: bool,
    current: bool,
) -> gpui::Stateful<gpui::Div> {
    menu_row(
        id,
        name,
        kind_icon(FileKind::Folder, theme::icon()),
        None,
        highlighted,
        current,
    )
}

/// A menu row: icon, name (in bold when `current`) and muted `detail` at the end.
fn menu_row(
    id: impl Into<gpui::ElementId>,
    name: String,
    icon: AnyElement,
    detail: Option<String>,
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
        .h(theme::s_32())
        .mx(theme::s_4())
        .px(theme::s_8())
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_8())
        .rounded(theme::r_8())
        .cursor_pointer()
        .when(highlighted, |d| d.bg(theme::wash_strong()))
        .when(!highlighted, |d| d.hover(|s| s.bg(theme::wash())))
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(current, |d| d.font_weight(theme::FW_STRONG))
                .child(SharedString::from(name)),
        )
        .children(detail.map(|detail| {
            div()
                .flex_none()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(detail)
        }))
}

/// The row of `current` in `items`, if it is there.
fn current_row(items: &[PopupItem], current: Option<&Path>) -> Option<usize> {
    let current = current?;
    items.iter().position(|item| item.path() == Some(current))
}

fn note(text: String) -> AnyElement {
    div()
        .px(theme::s_12())
        .py(theme::s_6())
        .text_size(theme::fs_12())
        .text_color(theme::text_subtle())
        .child(text)
        .into_any_element()
}

#[cfg(test)]
impl Explorer {
    /// Tests: the open › menu's place on the screen, its keyboard row and whether that is lit.
    pub(crate) fn path_popup_for_test(
        &self,
    ) -> Option<(Option<crate::popup::ScreenRect>, Option<usize>, bool)> {
        let popup = self.path_popup.as_ref()?;
        Some((popup.popup.placed(), popup.highlighted, popup.keyboard))
    }

    /// Tests: the open › menu's rows as (name, detail, in bold).
    pub(crate) fn path_popup_rows_for_test(&self) -> Option<Vec<(String, Option<String>, bool)>> {
        let popup = self.path_popup.as_ref()?;
        let current = popup.current.as_deref();
        Some(
            popup
                .items
                .as_ref()?
                .iter()
                .map(|item| {
                    let bold = current.is_some() && item.path() == current;
                    (item.name.clone(), item.detail.clone(), bold)
                })
                .collect(),
        )
    }

    /// Tests: where the path bar was drawn.
    pub(crate) fn path_bar_bounds_for_test(&self) -> Bounds<Pixels> {
        self.path_bounds.get()
    }

    /// Tests: current autocomplete entries.
    pub(crate) fn suggestion_names(&self) -> Vec<String> {
        self.suggest.items.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Tests: the search box's input.
    pub(crate) fn filter_input_for_test(&self) -> gpui::Entity<crate::text_input::TextInput> {
        self.filter_input.clone()
    }

    /// Tests: path input text.
    #[cfg(test)]
    pub(crate) fn path_input_for_test(&self) -> gpui::Entity<crate::text_input::TextInput> {
        self.path_input.clone()
    }

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
