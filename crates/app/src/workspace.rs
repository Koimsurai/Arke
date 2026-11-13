//! Window root view: the frame (title row and sidebar) plus the active tab's panes.
//!
//! Layout is a frame with one sheet of paper: the sidebar and top bar are drawn on the frame and the
//! file list is the only paper. The sidebar holds favorite tiles, This PC, then Arc-style vertical tabs.
//!
//! A tab has one or two panes (dual pane, like Arc's split tabs). Each pane is an independent `Explorer`
//! with its own directory service, history and filter. Only the active tab renders; background tabs keep watching.
//!
//! Ctrl+T opens the location palette (`palette.rs`): Enter opens a new tab, Shift+Enter opens a split.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use arke_core::NavKind;
use arke_core::filetype::FileKind;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, KeyBinding, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, SharedString, Subscription, Window,
    WindowAppearance, WindowControlArea, actions, div, prelude::*, px,
};
use rust_i18n::t;

use crate::a11y::A11yExt as _;
use crate::bench::Bench;
use crate::explorer::{Explorer, ExplorerEvent, PaneInfo, Services, kind_icon};
use crate::net_dialog::{NetDialog, NetDialogEvent, NetDialogKind};
use crate::palette::{Candidate, LocationPalette, PaletteEvent, Source, Target};
use crate::settings::{SettingsStore, TabLayout};
use crate::theme::{self, UiFont as _};

#[path = "workspace_backup.rs"]
mod backup;
#[path = "workspace_home.rs"]
mod home_page;
#[path = "workspace_menuprefs.rs"]
pub(crate) mod menuprefs;
#[path = "workspace_pages.rs"]
mod pages;
#[path = "workspace_recycle.rs"]
mod recycle_page;
#[path = "workspace_report.rs"]
mod report_page;
#[path = "workspace_saved.rs"]
mod saved;
#[path = "workspace_settings.rs"]
mod settings_panel;
#[path = "workspace_shelf.rs"]
mod shelf_view;
#[path = "workspace_sidebar.rs"]
mod sidebar;
#[path = "workspace_tabdrag.rs"]
mod tabdrag;
#[path = "workspace_uninstall.rs"]
mod uninstall_page;
#[path = "workspace_update.rs"]
mod update_page;
#[path = "workspace_welcome.rs"]
mod welcome;
use settings_panel::CloseSettings;
pub(crate) use settings_panel::{OpenSettings, switch};
pub(crate) use sidebar::{leave_drive_everywhere, place_icon};

/// The Arke window used last, where folders from other programs open as tabs (#11). gpui has no
/// window order on Windows (`window_stack` is `None` there), so each window records activation.
#[derive(Default)]
pub(crate) struct LastActiveWindow(pub(crate) Option<gpui::AnyWindowHandle>);

impl gpui::Global for LastActiveWindow {}

actions!(
    workspace,
    [
        NewTab,
        OpenPalette,
        CloseTab,
        NextTab,
        PrevTab,
        ReopenClosedTab,
        /// Dual pane: browsing in one side repeats in the other (Ctrl+Shift+S).
        ToggleSyncBrowsing,
        /// Dual pane: mark what differs between the two folders (Ctrl+Shift+D).
        CompareFolders,
        /// Dual pane: select the items the comparison marked, in both panes.
        SelectDifferences,
        /// The saved workspaces panel.
        OpenWorkspaces
    ]
);

/// Closed tabs remembered for Ctrl+Shift+T.
const CLOSED_TABS_MAX: usize = 20;

/// A closed tab: folder of each pane, its index, and which pane was active.
struct ClosedTab {
    paths: Vec<PathBuf>,
    at: usize,
    active_pane: usize,
    ratio: f32,
    pinned: bool,
    synced: bool,
}

/// Minimum width of each side in dual pane.
const PANE_MIN: gpui::Pixels = px(420.);

pub(crate) fn bind_keys(cx: &mut App) {
    settings_panel::bind_keys(cx);
    uninstall_page::bind_keys(cx);
    crate::net_dialog::bind_keys(cx);
    crate::recycle_bin::bind_keys(cx);
    let ctx = Some("Workspace");
    cx.bind_keys([
        KeyBinding::new("ctrl-t", OpenPalette, ctx),
        KeyBinding::new("ctrl-w", CloseTab, ctx),
        KeyBinding::new("ctrl-shift-t", ReopenClosedTab, ctx),
        KeyBinding::new("ctrl-f4", CloseTab, ctx),
        KeyBinding::new("ctrl-tab", NextTab, ctx),
        KeyBinding::new("ctrl-pagedown", NextTab, ctx),
        KeyBinding::new("ctrl-shift-tab", PrevTab, ctx),
        KeyBinding::new("ctrl-pageup", PrevTab, ctx),
        KeyBinding::new("ctrl-shift-s", ToggleSyncBrowsing, ctx),
        KeyBinding::new("ctrl-shift-d", CompareFolders, ctx),
    ]);
}

struct Pane {
    view: Entity<Explorer>,
    /// Folder this pane last finished opening, to tell what kind of move the next one was
    /// (synchronized browsing repeats it in the other pane).
    last: Option<PathBuf>,
    _subscriptions: [Subscription; 2],
}

struct Tab {
    /// Stable identity across reorders.
    id: u64,
    /// One pane normally, two in dual pane.
    panes: Vec<Pane>,
    active_pane: usize,
    /// Width fraction of the left pane.
    ratio: f32,
    /// Pinned tabs sit at the top of the list (always a prefix of `Workspace::tabs`), look
    /// compact, and middle click, "Close other tabs" and "Close all tabs" leave them open.
    pinned: bool,
    /// Dual pane: synchronized browsing.
    synced: bool,
    /// Dual pane: folder comparison is on (the marks live on each pane).
    compare: Option<saved::CompareState>,
}

impl Tab {
    fn view(&self) -> &Entity<Explorer> {
        &self.panes[self.active_pane].view
    }

    const fn is_split(&self) -> bool {
        self.panes.len() > 1
    }

    /// A tab with one pane.
    fn new(pane: Pane) -> Self {
        Self {
            id: tabdrag::next_tab_id(),
            panes: vec![pane],
            active_pane: 0,
            ratio: 0.5,
            pinned: false,
            synced: false,
            compare: None,
        }
    }
}

thread_local! {
    /// Folder just pinned to the list (tiles were full) and its serial, so the sidebar row flashes
    /// and shows where it went.
    static PIN_FLASH: std::cell::RefCell<Option<(PathBuf, u32)>> =
        const { std::cell::RefCell::new(None) };
    static PIN_FLASH_SERIAL: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Flashes this sidebar list row, e.g. after pinning overflowed from the tiles into the list.
pub(crate) fn flash_pin(path: PathBuf) {
    let serial = PIN_FLASH_SERIAL.with(|s| {
        s.set(s.get().wrapping_add(1));
        s.get()
    });
    PIN_FLASH.with(|f| *f.borrow_mut() = Some((path, serial)));
}

/// Animation serial if this row should flash.
pub(crate) fn pin_flash(path: &Path) -> Option<u32> {
    PIN_FLASH.with(|f| {
        f.borrow()
            .as_ref()
            .filter(|(p, _)| p == path)
            .map(|(_, serial)| *serial)
    })
}

/// Where the item at `ix` ends up after moving an item from `from` to `to`.
const fn index_after_move(ix: usize, from: usize, to: usize) -> usize {
    if ix == from {
        to
    } else if from < ix && ix <= to {
        ix - 1
    } else if to <= ix && ix < from {
        ix + 1
    } else {
        ix
    }
}

#[cfg(test)]
mod tab_order_tests {
    use super::index_after_move;

    // Check against an actual move: every item's computed index must match.
    #[test]
    fn indices_follow_the_moved_tab() {
        for len in 1..6 {
            for from in 0..len {
                for to in 0..len {
                    let mut order: Vec<usize> = (0..len).collect();
                    let moved = order.remove(from);
                    order.insert(to, moved);
                    for ix in 0..len {
                        assert_eq!(
                            order[index_after_move(ix, from, to)],
                            ix,
                            "len {len} from {from} to {to} ix {ix}"
                        );
                    }
                }
            }
        }
    }
}

/// A tab being dragged. Over another tab it reorders; over the right half of the content area it splits.
#[derive(Clone)]
struct DraggedTab {
    ix: usize,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(theme::H_ROW)
            .px(theme::S_8)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_8)
            .bg(theme::overlay())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .child(kind_icon(FileKind::Folder, theme::ICON))
            .child(self.title.clone())
    }
}

// UI flags are independent (resizing, dragging, shelf open, ...) and don't fit a state machine.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Workspace {
    services: Services,
    tabs: Vec<Tab>,
    pub(crate) active: usize,
    /// Recently closed tabs, newest last (Ctrl+Shift+T).
    closed: Vec<ClosedTab>,
    /// Sidebar tree: expanded folders and listed children.
    expanded: HashSet<PathBuf>,
    /// The tree node just expanded or collapsed: its chevron turns and its children fade in.
    tree_cue: (Option<PathBuf>, crate::motion::Cue),
    tree_children: HashMap<PathBuf, sidebar::TreeChildren>,
    /// Folders a background check found to have no subfolders (no chevron in the tree).
    tree_leaf: HashSet<PathBuf>,
    sidebar_menu: Option<sidebar::SidebarMenu>,
    pub(crate) palette: Option<(Entity<LocationPalette>, Subscription)>,
    /// The dismissed palette fading out (see `motion::Leaving`).
    closing_palette: Option<crate::motion::Leaving<Entity<LocationPalette>>>,
    /// Network login/connect dialog.
    net_dialog: Option<NetDialogSlot>,
    /// Saved workspaces panel.
    workspaces: Option<(Entity<saved::WorkspacesPanel>, Subscription)>,
    _saved_workspaces: Subscription,
    /// Open Settings page (holds focus).
    settings_panel: Option<settings_panel::SettingsPage>,
    /// Open Recycle Bin page.
    recycle_page: Option<recycle_page::RecyclePage>,
    home_page: Option<home_page::HomeView>,
    /// Window opened without a path: once settings load, switch to the last session if configured.
    start_default: bool,
    /// Dragging the divider between the panes.
    resizing: bool,
    /// Tab being dragged; dropping on the right half of the content area splits, like Arc.
    dragging_tab: Option<usize>,
    /// Animation and hit testing for tab and favorite tile reordering.
    tab_drag: tabdrag::TabDrag,
    /// Dragging a pinned folder; an empty list needs a drop target.
    dragging_pin: bool,
    /// The shelf's items are shown in the sidebar.
    shelf_open: bool,
    /// Repaint when the shelf changes (any window may change it).
    _shelf: Subscription,
    last_viewport_width: gpui::Pixels,
    /// The active tab's shared preview at the last render (split view with the preview on).
    shared_preview: Option<crate::explorer::SharedPreview>,
    _appearance: Subscription,
    /// Rechecks network folders when Arke regains focus.
    _activation: Subscription,
    _settings: Subscription,
    /// Last session saved to settings; only saved again when it changes.
    saved_session: Option<crate::settings::Session>,
    /// Window position is saved only after moving or resizing settles.
    _bounds: Subscription,
    bounds_task: Option<gpui::Task<()>>,
    /// Look version this window last rendered ([`theme::look`]).
    look_seen: u32,
}

/// Open network dialog, plus the tab and path to reopen after a successful login.
struct NetDialogSlot {
    dialog: Entity<NetDialog>,
    after: Option<(Entity<Explorer>, PathBuf)>,
    _events: Subscription,
}

impl Workspace {
    /// Opens Arke's network login/connect dialog.
    pub(crate) fn open_net_dialog(
        &mut self,
        kind: NetDialogKind,
        after: Option<(Entity<Explorer>, PathBuf)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shell = self.services.shell.clone();
        let remotes = self.services.remotes.clone();
        let dialog = cx.new(|cx| NetDialog::new(kind, shell, remotes, window, cx));
        let events = cx.subscribe_in(&dialog, window, Self::on_net_dialog);
        self.net_dialog = Some(NetDialogSlot {
            dialog,
            after,
            _events: events,
        });
        cx.notify();
    }

    fn on_net_dialog(
        &mut self,
        _: &Entity<NetDialog>,
        event: &NetDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.net_dialog.take() else {
            return;
        };
        self.focus_active(window, cx);
        match event {
            NetDialogEvent::Closed => {}
            NetDialogEvent::UseSystemDialog => self.system_drive_dialog(true, window, cx),
            NetDialogEvent::Connected => {
                // Reload the sidebar drives and reopen the path in the tab that asked to log in.
                self.reload_all_places(cx);
                if let Some((view, path)) = slot.after {
                    view.update(cx, |v, cx| {
                        v.navigate(path, arke_core::NavKind::New, cx);
                    });
                }
            }
            NetDialogEvent::RemoteConnected { place, save } => {
                let url = place.url.clone();
                if *save {
                    let place = place.clone();
                    SettingsStore::global(cx).update(cx, |s, cx| {
                        s.update(|st| st.save_remote_place(place), cx);
                    });
                } else {
                    // Signed in again: keep the saved location's key file up to date.
                    SettingsStore::global(cx).update(cx, |s, cx| {
                        s.update(
                            |st| {
                                if let Some(saved) =
                                    st.remote_places.iter_mut().find(|p| p.url == place.url)
                                {
                                    saved.key_file.clone_from(&place.key_file);
                                }
                            },
                            cx,
                        );
                    });
                }
                // Reopen where sign-in was asked for (that folder, not just the server), or open
                // the new location in the active tab.
                let (view, path) = slot
                    .after
                    .unwrap_or_else(|| (self.active_view().clone(), url));
                view.update(cx, |v, cx| v.navigate(path, arke_core::NavKind::New, cx));
            }
        }
        cx.notify();
    }

    /// Rechecks disk space in every tab's sidebar; each tab has its own place list.
    pub(crate) fn refresh_disk_space(&self, cx: &App) {
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.view.read(cx).refresh_disk_space();
            }
        }
    }

    pub(crate) fn reload_all_places(&self, cx: &App) {
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.view.read(cx).reload_places();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn net_dialog_for_test(&self) -> Option<Entity<NetDialog>> {
        self.net_dialog.as_ref().map(|slot| slot.dialog.clone())
    }
}

/// Actual accent color for the setting; `None` is the default Arke orange.
pub(crate) fn chosen_accent(accent: crate::settings::Accent) -> Option<u32> {
    match accent {
        crate::settings::Accent::Arke => None,
        // Fall back to the default if the system color cannot be read.
        crate::settings::Accent::Windows => crate::win::system_accent(),
        crate::settings::Accent::Color(color) => Some(color),
    }
}

/// Applies light/dark mode and accent. Mode priority: ARKE_THEME, then settings, then system.
/// With Mica the window frame follows too, so Mica picks up the same color.
fn apply_theme(window: &mut Window, cx: &mut App) {
    let forced = std::env::var("ARKE_THEME")
        .ok()
        .and_then(|v| match v.as_str() {
            "light" => Some(false),
            "dark" => Some(true),
            _ => None,
        });
    let look = SettingsStore::global(cx).read(cx).settings.clone();
    let accent = chosen_accent(look.accent);
    theme::set_accent(accent);
    theme::set_tint(match look.tint {
        crate::settings::Tint::None => None,
        crate::settings::Tint::Accent => Some(accent.unwrap_or(theme::ARKE_ACCENT)),
        crate::settings::Tint::Color(color) => Some(color),
    });
    crate::prefs::apply(&look);
    theme::set_font(&look.font);
    theme::set_backdrop(look.background_image.clone().map(|path| {
        (
            path,
            f32::from(look.background_opacity.clamp(5, 100)) / 100.,
        )
    }));
    // Material follows the setting; machines without Mica always get solid.
    theme::set_mica(look.material != crate::settings::Material::Solid);
    window.set_background_appearance(if !theme::mica() {
        gpui::WindowBackgroundAppearance::Opaque
    } else if look.material == crate::settings::Material::MicaAlt {
        gpui::WindowBackgroundAppearance::MicaAltBackdrop
    } else {
        gpui::WindowBackgroundAppearance::MicaBackdrop
    });
    let chosen = match SettingsStore::global(cx).read(cx).settings.theme {
        crate::settings::ThemeMode::Light => Some(false),
        crate::settings::ThemeMode::Dark => Some(true),
        crate::settings::ThemeMode::System => None,
    };
    let dark = forced
        .or(chosen)
        .unwrap_or_else(|| is_dark(window.appearance()));
    if dark != theme::is_dark() {
        theme::set_dark(dark);
        window.refresh();
    }
    if theme::mica()
        && let Some(hwnd) = Explorer::owner_from(window)
    {
        crate::win::set_dark_frame(hwnd, dark);
    }
}

const fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

impl Workspace {
    pub(crate) fn new(
        services: Services,
        start_path: PathBuf,
        start_default: bool,
        restore: Option<crate::settings::Session>,
        bench: Option<Bench>,
        started_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Mode priority: ARKE_THEME (for screenshots and tests), then settings, then system.
        apply_theme(window, cx);
        let appearance =
            cx.observe_window_appearance(window, move |this: &mut Self, window, cx| {
                this.sync_look(window, cx);
                window.refresh();
                cx.notify();
            });
        // On window activation, recheck visible network folders once. No polling; watchers cover the rest.
        let activation = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
            if !window.is_window_active() {
                return;
            }
            cx.set_global(LastActiveWindow(Some(window.window_handle())));
            crate::motion::sync_app(cx);
            // "Follow Windows" accent: the system color may have just changed.
            this.sync_look(window, cx);
            this.recycle_refresh(cx);
            this.home_refresh(cx);
            // Another program may have added or deleted files.
            this.refresh_disk_space(cx);
            // Shelf items may have been moved or deleted outside Arke.
            let fs = this.services.fs.clone();
            crate::shelf::ShelfStore::global(cx).update(cx, |s, cx| s.recheck(fs, cx));
            // Global search: probe network locations that were unreachable last time.
            if let Some(global) = crate::global_search::GlobalSearch::global(cx) {
                global.update(cx, super::global_search::GlobalSearch::recheck_network);
            }
            if let Some(tab) = this.tabs.get(this.active) {
                for pane in &tab.panes {
                    pane.view.read(cx).recheck_network();
                }
            }
        });
        // Repaint when settings change (pins, favorites, tab layout, ...).
        let store = SettingsStore::global(cx);
        let settings = cx.observe_in(&store, window, |this: &mut Self, store, window, cx| {
            this.sync_look(window, cx);
            this.sync_accent_input(window, cx);
            this.start_from_settings(&store, window, cx);
            cx.notify();
        });
        // Saved workspaces show in the sidebar and Ctrl+T.
        let saved_workspaces = crate::saved_workspaces::WorkspaceStore::global(cx);
        let saved_workspaces = cx.observe(&saved_workspaces, |_, _, cx| cx.notify());
        let shelf_store = crate::shelf::ShelfStore::global(cx);
        let shelf = cx.observe(&shelf_store, |_, _, cx| cx.notify());
        // Remember window position and size once moving or resizing settles.

        let bounds = cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
            this.remember_bounds(window, cx);
        });
        let mut this = Self {
            closed: Vec::new(),
            services,
            tabs: Vec::new(),
            active: 0,
            expanded: HashSet::new(),
            tree_cue: (None, crate::motion::Cue::default()),
            tree_children: HashMap::new(),
            tree_leaf: HashSet::new(),
            sidebar_menu: None,
            palette: None,
            closing_palette: None,
            net_dialog: None,
            workspaces: None,
            _saved_workspaces: saved_workspaces,
            settings_panel: None,
            recycle_page: None,
            home_page: None,
            start_default,
            resizing: false,
            dragging_tab: None,
            tab_drag: tabdrag::TabDrag::default(),
            dragging_pin: false,
            shelf_open: false,
            _shelf: shelf,
            last_viewport_width: px(1200.),
            shared_preview: None,
            _appearance: appearance,
            _activation: activation,
            _settings: settings,
            saved_session: None,
            _bounds: bounds,
            bounds_task: None,
            look_seen: theme::look(),
        };
        let pane = this.make_pane(start_path, bench, started_at, window, cx);
        this.tabs.push(Tab::new(pane));
        if let Some(session) = restore {
            this.restore_session(&session, window, cx);
        }
        this
    }

    /// Applies the configured mode and accent, and repaints everything if it differs from what this
    /// window last drew. The look is app-wide, so another window may have applied it first.
    fn sync_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        apply_theme(window, cx);
        let look = theme::look();
        if look != self.look_seen {
            self.look_seen = look;
            window.refresh();
        }
    }

    /// Restores the last session. The first tab is already open on the first pane of `session.tabs[0]`.
    fn restore_session(
        &mut self,
        session: &crate::settings::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (ix, saved) in session.tabs.iter().enumerate() {
            let Some(first) = saved.panes.first() else {
                continue;
            };
            let tab_ix = if ix == 0 {
                0
            } else {
                let at = self.tabs.len();
                self.open_tab(first.clone(), at, false, window, cx);
                at
            };
            if let Some(second) = saved.panes.get(1) {
                self.split(tab_ix, second.clone(), window, cx);
                if let Some(tab) = self.tabs.get_mut(tab_ix) {
                    tab.active_pane = saved.active_pane.min(1);
                    tab.ratio = saved.ratio.clamp(0.2, 0.8);
                }
            }
            if let Some(tab) = self.tabs.get_mut(tab_ix) {
                tab.pinned = saved.pinned;
                tab.synced = saved.synced && tab.is_split();
            }
        }
        self.active = session.active.min(self.tabs.len() - 1);
        self.keep_pins_first();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Snapshot of this window's tabs for the last session. Panes without a path are skipped.
    pub(crate) fn session_snapshot(&self, cx: &App) -> crate::settings::Session {
        let tabs: Vec<crate::settings::SessionTab> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let panes: Vec<PathBuf> = tab
                    .panes
                    .iter()
                    .filter_map(|p| p.view.read(cx).display_path().map(Path::to_path_buf))
                    .collect();
                (!panes.is_empty()).then(|| crate::settings::SessionTab {
                    active_pane: tab.active_pane.min(panes.len() - 1),
                    ratio: tab.ratio,
                    pinned: tab.pinned,
                    synced: tab.synced && panes.len() > 1,
                    panes,
                })
            })
            .collect();
        crate::settings::Session {
            active: self.active.min(tabs.len().saturating_sub(1)),
            tabs,
        }
    }

    /// Saves tabs whenever they change (open, close, reorder, navigate); the last window to change wins.
    /// Skipped until settings are loaded and the startup switch is done, so the previous session is not overwritten.
    fn save_session_if_changed(&mut self, cx: &mut Context<Self>) {
        let store = SettingsStore::global(cx);
        if self.start_default || !store.read(cx).loaded {
            return;
        }
        let snapshot = self.session_snapshot(cx);
        if snapshot.tabs.is_empty() || self.saved_session.as_ref() == Some(&snapshot) {
            return;
        }
        self.saved_session = Some(snapshot.clone());
        // Do not update other entities during paint; defer to the end of this cycle.
        cx.defer(move |cx| {
            store.update(cx, |s, cx| s.update(|st| st.session = snapshot, cx));
        });
    }

    /// Writes window position and size to settings 400 ms after it stops changing, so a drag does
    /// not notify every view on each frame.
    fn remember_bounds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (bounds, maximized) = match window.window_bounds() {
            gpui::WindowBounds::Windowed(b) => (b, false),
            gpui::WindowBounds::Maximized(b) | gpui::WindowBounds::Fullscreen(b) => (b, true),
        };
        let place = crate::settings::WindowPlace {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            maximized,
        };
        self.bounds_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            cx.update(|cx| {
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(|st| st.window = Some(place), cx);
                });
            });
        }));
    }

    /// Settings loaded: if no path was given and the start location is the last session, switch once.
    fn start_from_settings(
        &mut self,
        store: &Entity<SettingsStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.start_default {
            return;
        }
        let store = store.read(cx);
        if !store.loaded {
            return;
        }
        self.start_default = false;
        match store.settings.start_location {
            crate::settings::StartLocation::Home => {}
            crate::settings::StartLocation::HomePage => self.open_home(false, window, cx),
            crate::settings::StartLocation::LastFolder => {
                if let Some(last) = store.last_session_folder.clone() {
                    self.active_view()
                        .update(cx, |v, cx| v.navigate(last, NavKind::New, cx));
                }
            }
            crate::settings::StartLocation::LastSession => {
                // Settings loaded after the window: restore all last-session tabs only if this is the only window.
                let session = store.settings.session.clone();
                let Some(first) = session.tabs.first().and_then(|t| t.panes.first()).cloned()
                else {
                    return;
                };
                if cx.windows().len() > 1 {
                    return;
                }
                self.active_view()
                    .update(cx, |v, cx| v.navigate(first, NavKind::New, cx));
                self.restore_session(&session, window, cx);
            }
        }
    }

    /// Active pane of each tab.
    #[cfg(test)]
    pub(crate) fn tab_views(&self) -> impl Iterator<Item = &Entity<Explorer>> {
        self.tabs.iter().map(Tab::view)
    }

    /// Tests: the active tab's shared preview at the last render.
    #[cfg(test)]
    pub(crate) const fn shared_preview_for_test(&self) -> Option<crate::explorer::SharedPreview> {
        self.shared_preview
    }

    /// All panes of the active tab.
    #[cfg(test)]
    pub(crate) fn pane_views(&self) -> Vec<Entity<Explorer>> {
        self.tabs[self.active]
            .panes
            .iter()
            .map(|p| p.view.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn active_pane(&self) -> usize {
        self.tabs[self.active].active_pane
    }

    pub(crate) fn active_view(&self) -> &Entity<Explorer> {
        self.tabs[self.active].view()
    }

    /// Tab and pane index of this `Explorer`.
    fn locate(&self, view: &Entity<Explorer>) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(t, tab)| {
            tab.panes
                .iter()
                .position(|p| p.view == *view)
                .map(|pane| (t, pane))
        })
    }

    fn make_pane(
        &mut self,
        path: PathBuf,
        bench: Option<Bench>,
        started_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Pane {
        let services = self.services.clone();
        let view = cx.new(|cx| Explorer::new(services, path, bench, started_at, window, cx));
        let events = cx.subscribe_in(&view, window, Self::on_explorer_event);
        // Tab titles and the sidebar follow the current folder.
        let observe = cx.observe(&view, |_, _, cx| cx.notify());
        Pane {
            view,
            last: None,
            _subscriptions: [events, observe],
        }
    }

    /// Tabs are on top (setting).
    fn horizontal_tabs(cx: &App) -> bool {
        cx.try_global::<crate::settings::GlobalSettings>()
            .is_some_and(|g| g.0.read(cx).settings.tab_layout == TabLayout::Horizontal)
    }

    fn on_explorer_event(
        &mut self,
        source: &Entity<Explorer>,
        event: &ExplorerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((tab_ix, pane_ix)) = self.locate(source) else {
            return;
        };
        match event {
            ExplorerEvent::OpenInNewTab { path, activate } => {
                self.open_tab(path.clone(), tab_ix + 1, *activate, window, cx);
            }
            ExplorerEvent::ToggleSplit => {
                if self.tabs[tab_ix].is_split() {
                    self.unsplit(tab_ix, pane_ix, window, cx);
                } else {
                    let path = source
                        .read(cx)
                        .display_path()
                        .map_or_else(home, Path::to_path_buf);
                    self.split(tab_ix, path, window, cx);
                }
            }
            ExplorerEvent::Activated => self.activate_pane(tab_ix, pane_ix, window, cx),
            ExplorerEvent::NetLogin(path) if arke_core::remote::is_remote(path) => {
                let view = self
                    .tabs
                    .get(tab_ix)
                    .and_then(|t| t.panes.get(pane_ix))
                    .map(|p| p.view.clone());
                let key_file = arke_core::remote::RemoteUrl::from_path(path).and_then(|url| {
                    SettingsStore::global(cx)
                        .read(cx)
                        .settings
                        .remote_places
                        .iter()
                        .filter_map(|p| Some((arke_core::remote::RemoteUrl::from_path(&p.url)?, p)))
                        .find(|(saved, _)| saved.same_origin(&url))
                        .and_then(|(_, p)| p.key_file.clone())
                });
                self.open_net_dialog(
                    NetDialogKind::RemoteLogin {
                        url: path.clone(),
                        key_file,
                    },
                    view.map(|v| (v, path.clone())),
                    window,
                    cx,
                );
            }
            ExplorerEvent::NetLogin(path) => {
                let view = self
                    .tabs
                    .get(tab_ix)
                    .and_then(|t| t.panes.get(pane_ix))
                    .map(|p| p.view.clone());
                // Log in to the share root (\\server\share), not a folder below it.
                let share = arke_core::net::share_root(path).unwrap_or_else(|| path.clone());
                self.open_net_dialog(
                    NetDialogKind::Login { share },
                    view.map(|v| (v, path.clone())),
                    window,
                    cx,
                );
            }
            // Record once per real navigation. Doing this in observe would make two panes trigger each other via settings.
            ExplorerEvent::Navigated(path) => {
                crate::search_history::SearchHistory::record(path, true, cx);
                SettingsStore::global(cx).update(cx, |s, cx| {
                    s.update(
                        |st| {
                            st.remember(path);
                            st.remember_share(path);
                        },
                        cx,
                    );
                });
                self.pane_navigated(tab_ix, pane_ix, path, cx);
            }
            ExplorerEvent::ToggleSync => self.toggle_sync(tab_ix, cx),
            ExplorerEvent::ToggleCompare => self.toggle_compare(tab_ix, cx),
            ExplorerEvent::SelectDifferences => self.select_differences(tab_ix, cx),
            ExplorerEvent::RemoteChanged(path) => {
                let views: Vec<_> = self
                    .tabs
                    .iter()
                    .flat_map(|t| t.panes.iter().map(|p| p.view.clone()))
                    .collect();
                for view in views {
                    view.update(cx, |v, cx| v.reload_if_showing(path, cx));
                }
            }
            ExplorerEvent::WindowMenu(position) => self.open_window_menu(*position, window, cx),
            ExplorerEvent::ImportSettings(path) => {
                self.import_settings_from_menu(path.clone(), window, cx);
            }
            ExplorerEvent::FocusOtherPane => {
                self.activate_pane(tab_ix, 1 - pane_ix.min(1), window, cx);
            }
            ExplorerEvent::Arrived { dir, names } => {
                self.select_arrived_beside(tab_ix, pane_ix, dir, names, cx);
            }
        }
    }

    /// A copy or move from pane `pane_ix` landed in `dir`: if the other side of the split shows
    /// it, select the items there, so Copy to/Move to and drags across show their result.
    fn select_arrived_beside(
        &self,
        tab_ix: usize,
        pane_ix: usize,
        dir: &Path,
        names: &[std::sync::Arc<str>],
        cx: &mut Context<Self>,
    ) {
        let others: Vec<Entity<Explorer>> = self.tabs[tab_ix]
            .panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != pane_ix)
            .map(|(_, p)| p.view.clone())
            .collect();
        for view in others {
            view.update(cx, |e, cx| {
                if e.current_path() == Some(dir) {
                    e.select_arrived(names.to_vec(), cx);
                }
            });
        }
    }

    // --- Tabs ---

    /// Inserts a new tab at `at`. Background tabs do not take focus.
    pub(crate) fn open_tab(
        &mut self,
        path: PathBuf,
        at: usize,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_tab(path, at, activate, window, cx);
    }

    /// A folder or the Recycle Bin another program asked Arke to show, in this window (#11). A tab
    /// already showing the folder is reused (repeated "Show in folder" from a browser's downloads
    /// shouldn't pile up tabs); otherwise a new tab opens at the end. `select` is selected there.
    pub(crate) fn open_external(
        &mut self,
        target: arke_core::takeover::Target,
        select: Vec<std::sync::Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use arke_core::takeover::{Target, same_path};
        match target {
            Target::Folder(path) => {
                let showing = self.tabs.iter().position(|tab| {
                    let view = &tab.panes[tab.active_pane].view;
                    self.page_title(view.entity_id()).is_none()
                        && view
                            .read(cx)
                            .display_path()
                            .is_some_and(|p| same_path(p, &path))
                });
                if let Some(ix) = showing {
                    self.activate(ix, window, cx);
                    self.active_view()
                        .update(cx, |e, cx| e.select_in_folder(&path, select, cx));
                } else {
                    let at = self.tabs.len();
                    self.open_tab(path, at, true, window, cx);
                    self.active_view()
                        .update(cx, |e, _| e.select_on_arrival(select));
                }
            }
            Target::RecycleBin => self.open_recycle(true, window, cx),
            Target::Home => self.open_home(true, window, cx),
        }
    }

    /// [`Self::open_tab`], returning where the tab went: new tabs never go among the pinned ones.
    fn insert_tab(
        &mut self,
        path: PathBuf,
        at: usize,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let pane = self.make_pane(path, None, Instant::now(), window, cx);
        let at = at.clamp(self.pinned_count(), self.tabs.len());
        self.tabs.insert(at, Tab::new(pane));
        if activate {
            self.activate(at, window, cx);
        } else {
            if at <= self.active {
                self.active += 1;
            }
            // Explorer::new focuses its own list; give focus back to the active tab.
            self.focus_active(window, cx);
        }
        cx.notify();
        at
    }

    pub(crate) fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.active = ix;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn activate_pane(
        &mut self,
        tab_ix: usize,
        pane_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_ix) else {
            return;
        };
        if pane_ix < tab.panes.len() {
            tab.active_pane = pane_ix;
            self.active = tab_ix;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Focuses what the active tab shows: its page (Home, Settings, the Recycle Bin) when one
    /// covers the list, otherwise the list. A hidden list can't take keys, so with the list focused
    /// under a page, shortcuts such as Ctrl+W did nothing.
    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self
            .page_focus(cx)
            .unwrap_or_else(|| self.active_view().read(cx).focus_handle(cx));
        window.focus(&focus, cx);
    }

    /// Closes a tab (both panes in dual pane). Closing the last tab closes the window.
    pub(crate) fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            // The last tab: the window stays on Home; closing Home itself closes the window.
            if self.home_open() {
                window.remove_window();
            } else {
                self.remember_closed(ix, cx);
                self.open_home(false, window, cx);
            }
            return;
        }
        self.remember_closed(ix, cx);
        // Dropping the tab stops its directory service, watchers and searches.
        self.tabs.remove(ix);
        if self.active > ix || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Records a tab about to close, for Ctrl+Shift+T.
    fn remember_closed(&mut self, ix: usize, cx: &App) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let paths: Vec<PathBuf> = tab
            .panes
            .iter()
            .filter_map(|p| p.view.read(cx).display_path().map(Path::to_path_buf))
            .collect();
        if paths.is_empty() {
            return;
        }
        if self.closed.len() >= CLOSED_TABS_MAX {
            self.closed.remove(0);
        }
        self.closed.push(ClosedTab {
            active_pane: tab.active_pane.min(paths.len() - 1),
            ratio: tab.ratio,
            pinned: tab.pinned,
            synced: tab.synced,
            paths,
            at: ix,
        });
    }

    /// Reopens the most recently closed tab at its old index, with both panes if it was split.
    fn reopen_closed_tab(
        &mut self,
        _: &ReopenClosedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(closed) = self.closed.pop() else {
            return;
        };
        let mut paths = closed.paths.into_iter();
        let Some(first) = paths.next() else {
            return;
        };
        let mut at = self.insert_tab(first, closed.at, true, window, cx);
        if closed.pinned {
            // Back among the pinned tabs, where it was.
            self.tabs[at].pinned = true;
            at = self.move_tab(at, closed.at, cx);
            self.active = at;
        }
        if let Some(second) = paths.next() {
            self.split(at, second, window, cx);
            if let Some(tab) = self.tabs.get_mut(at) {
                tab.ratio = closed.ratio;
                tab.synced = closed.synced;
            }
            self.activate_pane(at, closed.active_pane, window, cx);
        }
    }

    /// Tab context menu "Duplicate": opens a tab on the same folder next to it.
    pub(crate) fn duplicate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .tabs
            .get(ix)
            .and_then(|t| t.view().read(cx).display_path().map(Path::to_path_buf))
        else {
            return;
        };
        self.open_tab(path, ix + 1, true, window, cx);
    }

    /// Tab context menu "Close other tabs": keeps only `keep`. Closed tabs can be restored with Ctrl+Shift+T.
    pub(crate) fn close_other_tabs(
        &mut self,
        keep: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.tabs.get(keep).map(|t| t.id) else {
            return;
        };
        self.close_unpinned_except(Some(id), window, cx);
    }

    /// Tab context menu "Close all tabs": closes every tab that isn't pinned. With nothing
    /// pinned, one tab on the home folder stays so the window doesn't close.
    pub(crate) fn close_all_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pinned_count() == 0 {
            let at = self.tabs.len();
            let at = self.insert_tab(home(), at, false, window, cx);
            let id = self.tabs[at].id;
            self.close_unpinned_except(Some(id), window, cx);
        } else {
            self.close_unpinned_except(None, window, cx);
        }
    }

    /// Closes every unpinned tab except the one with id `keep`. They can be restored with
    /// Ctrl+Shift+T. The active tab stays active if it's still open, else the kept one is.
    fn close_unpinned_except(
        &mut self,
        keep: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_id = self.tabs[self.active].id;
        // Close from the back so indices stay valid; the first closed is restored last.
        for ix in (0..self.tabs.len()).rev() {
            let tab = &self.tabs[ix];
            if !tab.pinned && Some(tab.id) != keep {
                self.remember_closed(ix, cx);
                self.tabs.remove(ix);
            }
        }
        self.active = self
            .tabs
            .iter()
            .position(|t| t.id == active_id)
            .or_else(|| self.tabs.iter().position(|t| Some(t.id) == keep))
            .unwrap_or(0);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// How many tabs are pinned (they are the first ones).
    fn pinned_count(&self) -> usize {
        self.tabs.iter().take_while(|t| t.pinned).count()
    }

    /// Pins or unpins a tab. Pinning moves it to the end of the pinned tabs, unpinning to just
    /// after them.
    pub(crate) fn toggle_pin(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        tab.pinned = !tab.pinned;
        // The flag is already flipped: the count now includes (or leaves out) this tab.
        let pinned = self.tabs.iter().filter(|t| t.pinned).count();
        let to = if self.tabs[ix].pinned {
            pinned - 1
        } else {
            pinned
        };
        self.move_tab(ix, to, cx);
        cx.notify();
    }

    /// Restores the rule that pinned tabs come first (after loading a session or a workspace
    /// that broke it, e.g. a hand-edited file). Keeps the active tab active.
    fn keep_pins_first(&mut self) {
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        self.tabs.sort_by_key(|t| !t.pinned);
        self.active = self
            .tabs
            .iter()
            .position(|t| Some(t.id) == active_id)
            .unwrap_or(0);
    }

    /// Opens a new tab on the current folder without going through Ctrl+T.
    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.current_dir(cx);
        let at = self.active + 1;
        self.open_tab(path, at, true, window, cx);
    }

    fn current_dir(&self, cx: &App) -> PathBuf {
        self.active_view()
            .read(cx)
            .display_path()
            .map_or_else(home, Path::to_path_buf)
    }

    /// Ctrl+W: in split view closes only the active side, the other staying on as the tab's
    /// one pane; otherwise closes the tab.
    fn close_active(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_side(self.active, window, cx);
    }

    /// Closes the active side of a split tab (Ctrl+Shift+T brings it back as a tab of its own),
    /// or the tab itself when it isn't split.
    pub(crate) fn close_side(
        &mut self,
        tab_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_ix) else {
            return;
        };
        if !tab.is_split() {
            self.close_tab(tab_ix, window, cx);
            return;
        }
        let side = tab.active_pane.min(1);
        if let Some(path) = tab.panes[side]
            .view
            .read(cx)
            .display_path()
            .map(Path::to_path_buf)
        {
            if self.closed.len() >= CLOSED_TABS_MAX {
                self.closed.remove(0);
            }
            // Reopens next to the tab, on the side it was on.
            self.closed.push(ClosedTab {
                paths: vec![path],
                at: tab_ix + side,
                active_pane: 0,
                ratio: 0.5,
                pinned: false,
                synced: false,
            });
        }
        let tab = &mut self.tabs[tab_ix];
        // Dropping the pane stops its directory service, watchers and searches.
        let closed = tab.panes.remove(side);
        tab.active_pane = 0;
        tab.synced = false;
        if tab.compare.take().is_some() {
            for pane in [&tab.panes[0], &closed] {
                pane.view.update(cx, |e, cx| e.set_compare(None, cx));
            }
        }
        drop(closed);
        self.active = tab_ix;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let ix = (self.active + 1) % self.tabs.len();
        self.activate(ix, window, cx);
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        let ix = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.activate(ix, window, cx);
    }

    /// Ctrl+1..8 switches to tab N, Ctrl+9 to the last tab, as in browsers.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        // Esc during a drag cancels it; spring-opened folders go back to where they were.
        if ks.key == "escape" && cx.has_active_drag() {
            cx.stop_active_drag(window);
            window.refresh();
            cx.stop_propagation();
            return;
        }
        if !ks.modifiers.control || ks.modifiers.alt || ks.modifiers.shift {
            return;
        }
        let Ok(n) = ks.key.parse::<usize>() else {
            return;
        };
        let ix = match n {
            1..=8 => n - 1,
            9 => self.tabs.len() - 1,
            _ => return,
        };
        self.activate(ix, window, cx);
        cx.stop_propagation();
    }

    fn navigate_active(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.active_view().clone();
        view.update(cx, |e, cx| e.navigate(path, NavKind::New, cx));
        self.focus_active(window, cx);
    }

    fn open_in_background(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.active + 1;
        self.open_tab(path, at, false, window, cx);
    }

    // --- Dual pane ---

    /// Splits a tab into two panes, opening `path` on the right as the active side.
    pub(crate) fn split(
        &mut self,
        tab_ix: usize,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(tab_ix).is_none_or(Tab::is_split) {
            return;
        }
        let pane = self.make_pane(path, None, Instant::now(), window, cx);
        let tab = &mut self.tabs[tab_ix];
        tab.panes.push(pane);
        tab.active_pane = 1;
        tab.ratio = 0.5;
        self.active = tab_ix;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Unsplits, keeping `keep`. The other side becomes its own tab next to it, so nothing is
    /// lost: the left side before the right one, the order they had on screen.
    fn unsplit(&mut self, tab_ix: usize, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(tab_ix) else {
            return;
        };
        if !tab.is_split() {
            return;
        }
        let keep = keep.min(1);
        let other = tab.panes.remove(1 - keep);
        tab.active_pane = 0;
        tab.synced = false;
        let pinned = tab.pinned;
        if tab.compare.take().is_some() {
            for pane in [&tab.panes[0], &other] {
                pane.view.update(cx, |e, cx| e.set_compare(None, cx));
            }
        }
        // The other side stays next to it, pinned too if this tab was.
        let mut split_off = Tab::new(other);
        split_off.pinned = pinned;
        if keep == 1 {
            self.tabs.insert(tab_ix, split_off);
            self.active = tab_ix + 1;
        } else {
            self.tabs.insert(tab_ix + 1, split_off);
            self.active = tab_ix;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Where tab `from` may go when asked to move to `to`: pinned tabs stay among the pinned
    /// ones and the others below them.
    fn clamp_move(&self, from: usize, to: usize) -> usize {
        let pinned = self.tabs.iter().filter(|t| t.pinned).count();
        let to = to.min(self.tabs.len().saturating_sub(1));
        if self.tabs.get(from).is_some_and(|t| t.pinned) {
            to.min(pinned.saturating_sub(1))
        } else {
            to.max(pinned)
        }
    }

    /// Moves tab `from` to position `to` (after it when dragging down, before it when dragging up),
    /// kept on its side of the pinned tabs. Returns where it went. The active tab stays the same
    /// tab regardless of reordering.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) -> usize {
        if from >= self.tabs.len() {
            return from;
        }
        let to = self.clamp_move(from, to);
        if from == to {
            return from;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = index_after_move(self.active, from, to);
        cx.notify();
        to
    }

    /// Drops tab `from` on the right half of the content area: merges it with tab `into` as a split, `from` on the right.
    fn merge_tabs(
        &mut self,
        from: usize,
        into: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if from == into
            || from >= self.tabs.len()
            || into >= self.tabs.len()
            || self.tabs[from].is_split()
            || self.tabs[into].is_split()
        {
            return;
        }
        let moved = self.tabs.remove(from);
        let into = if from < into { into - 1 } else { into };
        self.tabs[into].synced = false;
        let tab = &mut self.tabs[into];
        tab.panes.extend(moved.panes);
        tab.active_pane = 1;
        tab.ratio = 0.5;
        self.active = into;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Before paint, gives each pane its layout info (split, active, caption buttons, the other side's folder).
    fn sync_panes(&mut self, cx: &mut Context<Self>) {
        let horizontal = Self::horizontal_tabs(cx);
        let tab = &self.tabs[self.active];
        let split = tab.is_split();
        // Split view shows one preview for the tab, at its right end, following the active side.
        let shared = if split && tab.view().read(cx).preview_pane {
            let mins = [0, 1].map(|i| tab.panes[i].view.read(cx).bare_list_width());
            crate::explorer::shared_preview(
                self.last_viewport_width,
                self.panes_width_hint(),
                tab.ratio,
                mins,
                crate::prefs::preview_width(),
            )
        } else {
            None
        };
        let reserve = shared.map_or(px(0.), crate::explorer::SharedPreview::reserve);
        let paths: Vec<Option<PathBuf>> = tab
            .panes
            .iter()
            .map(|p| p.view.read(cx).current_path().map(Path::to_path_buf))
            .collect();
        let last = tab.panes.len() - 1;
        for (i, pane) in tab.panes.iter().enumerate() {
            let info = PaneInfo {
                split,
                active: i == tab.active_pane,
                // With horizontal tabs the caption buttons live on the tab strip.
                show_caption: i == last && !horizontal,
                show_ops: i == last,
                nav_in_sidebar: !horizontal,
                peer: if split {
                    paths[1 - i.min(1)].clone()
                } else {
                    None
                },
                synced: split && tab.synced,
                comparing: split && tab.compare.is_some(),
                pair_toggles: split && i == last,
                share: match (split, i) {
                    (false, _) => 1.,
                    (true, 0) => tab.ratio,
                    (true, _) => 1. - tab.ratio,
                },
                reserve,
                trailing: shared.is_some() && i == last,
                preview_host: shared.filter(|_| i == tab.active_pane),
            };
            pane.view.update(cx, |e, cx| e.set_pane_info(info, cx));
        }
        self.shared_preview = shared;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.resizing {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.resizing = false;
            return;
        }
        // The panes share what the tab's preview leaves.
        let reserve = self
            .shared_preview
            .map_or(px(0.), crate::explorer::SharedPreview::reserve);
        let width = window.viewport_size().width - theme::SIDEBAR_W - reserve;
        let ratio = (event.position.x - theme::SIDEBAR_W) / width;
        // Keep at least PANE_MIN per side so the top bar and columns do not get crushed.
        let min = (PANE_MIN / width).min(0.5);
        self.tabs[self.active].ratio = ratio.clamp(min, 1. - min);
        cx.notify();
    }

    // ───────────────────────── Ctrl+T ─────────────────────────

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let current = self.current_dir(cx);
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut push =
            |label: String, icon: Option<crate::assets::Icon>, target: Target, source: Source| {
                if !candidates.iter().any(|c| c.label == label) {
                    candidates.push(Candidate {
                        label,
                        icon,
                        target,
                        source,
                    });
                }
            };
        for (ix, tab) in self.tabs.iter().enumerate() {
            if ix == self.active {
                continue;
            }
            if let Some(path) = tab.view().read(cx).display_path() {
                push(
                    path.display().to_string(),
                    None,
                    Target::Tab(ix),
                    Source::Tab,
                );
            }
        }
        // The current folder is already the first entry ("New tab here"); do not list it again.
        let recent = SettingsStore::global(cx).read(cx).settings.recent.clone();
        for path in recent.iter().filter(|p| **p != current) {
            push(
                path.display().to_string(),
                None,
                Target::Path(path.clone()),
                Source::Recent,
            );
        }
        for place in &self.active_view().read(cx).places {
            push(
                place.path.display().to_string(),
                Some(sidebar::place_icon(place.kind)),
                Target::Path(place.path.clone()),
                Source::Place,
            );
        }
        // Saved workspaces last: they show once their name is typed (workspace_saved.rs).
        let saved: Vec<String> = crate::saved_workspaces::WorkspaceStore::global(cx)
            .read(cx)
            .workspaces()
            .list()
            .iter()
            .map(|w| w.name.clone())
            .collect();
        for name in saved {
            push(
                name.clone(),
                None,
                Target::Workspace(name),
                Source::Workspace,
            );
        }
        let shell = self.services.shell.clone();
        let palette = cx.new(|cx| LocationPalette::new(current, candidates, shell, window, cx));
        let subscription = cx.subscribe_in(&palette, window, Self::on_palette_event);
        self.palette = Some((palette, subscription));
        cx.notify();
    }

    /// Close the palette at once (a choice was made).
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Dismissed (Escape, clicking outside): the palette fades out.
    fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((palette, _)) = self.palette.take() {
            crate::motion::Leaving::start(self, palette, |this| &mut this.closing_palette, cx);
        }
        self.close_palette(window, cx);
    }

    fn on_palette_event(
        &mut self,
        _: &Entity<LocationPalette>,
        event: &PaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PaletteEvent::Chosen { target, split } = event else {
            self.dismiss_palette(window, cx);
            return;
        };
        self.close_palette(window, cx);
        let path = match target {
            Target::Tab(ix) if !split => {
                self.activate(*ix, window, cx);
                return;
            }
            Target::Tab(ix) => match self.tabs.get(*ix) {
                Some(tab) => tab
                    .view()
                    .read(cx)
                    .display_path()
                    .map_or_else(home, Path::to_path_buf),
                None => return,
            },
            Target::Path(path) => path.clone(),
            Target::Workspace(name) => {
                self.open_saved_workspace(name, window, cx);
                return;
            }
        };

        if *split {
            // Already split: open on the other side.
            let tab_ix = self.active;
            if self.tabs[tab_ix].is_split() {
                let other = 1 - self.tabs[tab_ix].active_pane.min(1);
                let view = self.tabs[tab_ix].panes[other].view.clone();
                view.update(cx, |e, cx| e.navigate(path, NavKind::New, cx));
                self.activate_pane(tab_ix, other, window, cx);
            } else {
                self.split(tab_ix, path, window, cx);
            }
        } else {
            let at = self.active + 1;
            self.open_tab(path, at, true, window, cx);
        }
    }

    // --- Pane area ---

    fn render_panes(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[self.active];
        if !tab.is_split() {
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(tab.panes[0].view.clone())
                .into_any_element();
        }
        // Zero-width divider; the hit area extends left into the gap between the two papers.
        let resizing = self.resizing;
        let gutter = div().relative().w(px(0.)).h_full().child(
            div()
                .id("pane-gutter")
                .group("pane-gutter")
                .absolute()
                .top(theme::H_BAR)
                .bottom(theme::S_8)
                .left(-theme::S_12 + px(2.))
                .w(theme::S_12)
                .flex()
                .items_center()
                .justify_center()
                .cursor_col_resize()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.resizing = true;
                        cx.stop_propagation();
                    }),
                )
                .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                    // Double click resets to 50/50.
                    if event.click_count() >= 2 {
                        this.tabs[this.active].ratio = 0.5;
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .w(px(3.))
                        .h(theme::S_32 + theme::S_8)
                        .rounded(theme::R_FULL)
                        .bg(if resizing {
                            theme::signal()
                        } else {
                            theme::line_strong()
                        })
                        .group_hover("pane-gutter", |s| s.bg(theme::signal_line())),
                ),
        );
        // The divider's ratio applies to what the shared preview leaves. The right pane's box runs
        // on under the preview (its top bar spans the tab), and the preview sits on top of it.
        let reserve = self
            .shared_preview
            .map_or(px(0.), crate::explorer::SharedPreview::reserve);
        let shared = self.shared_preview.and_then(|shared| {
            let host = tab.view().clone();
            let preview = host.update(cx, Explorer::render_shared_preview)?;
            Some(
                div()
                    .absolute()
                    .top(theme::H_BAR)
                    .bottom(theme::S_8)
                    .right(theme::S_8)
                    .w(shared.width)
                    .child(preview),
            )
        });
        div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                div()
                    .h_full()
                    .w((self.panes_width_hint() - reserve) * tab.ratio)
                    .flex_none()
                    .child(tab.panes[0].view.clone()),
            )
            .child(gutter)
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .child(tab.panes[1].view.clone()),
            )
            .children(shared)
            .into_any_element()
    }
}

impl Workspace {
    /// While dragging a tab over the content area, the right half is a drop zone for split view (Arc's split gesture).
    fn render_split_drop_zone(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let from = self.dragging_tab?;
        let into = self.active;
        if from == into
            || self.tabs.get(from).is_none_or(Tab::is_split)
            || self.tabs[into].is_split()
        {
            return None;
        }
        let title = self.tabs[from].view().read(cx).tab_title();
        Some(
            div()
                .id("split-drop-zone")
                .absolute()
                .top(if Self::horizontal_tabs(cx) {
                    theme::H_BAR + theme::H_BAR
                } else {
                    theme::H_BAR
                })
                .bottom(theme::S_8)
                .right(theme::S_8)
                .w((self.panes_width_hint() - theme::S_8) / 2.)
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::R_12)
                .border_2()
                .border_color(theme::signal_line())
                .bg(theme::signal_wash())
                .text_color(theme::text())
                .font_weight(theme::FW_STRONG)
                .child(t!("split.drop_here", name = title).to_string())
                .drag_over::<DraggedTab>(|s, _, _, _| s.border_color(theme::signal()))
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    // The tab may already have moved during the drag; use its current index.
                    let from = this.dragging_tab.take().unwrap_or(dragged.ix);
                    this.merge_tabs(from, into, window, cx);
                }))
                .into_any_element(),
        )
    }

    /// When dragged files leave the window, hand off from Arke's drag to Windows drag and drop so they can
    /// land in browsers, mail, the desktop and so on. Mouse moves are only tracked while a drag is active.
    fn render_drag_out_watch(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::explorer::PendingDragOut;
        if !cx.has_active_drag() {
            if cx.has_global::<PendingDragOut>() {
                cx.remove_global::<PendingDragOut>();
            }
            return None;
        }
        if !cx.has_global::<PendingDragOut>() {
            return None;
        }
        let shell = self.services.shell.clone();
        Some(
            gpui::canvas(
                |_, _, _| (),
                move |_, (), window, _| {
                    let shell = shell.clone();
                    window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, window, cx| {
                        if phase != gpui::DispatchPhase::Bubble
                            || e.pressed_button != Some(MouseButton::Left)
                        {
                            return;
                        }
                        let size = window.viewport_size();
                        let inside = e.position.x >= px(0.)
                            && e.position.y >= px(0.)
                            && e.position.x < size.width
                            && e.position.y < size.height;
                        if inside || !cx.has_global::<PendingDragOut>() {
                            return;
                        }
                        let pending = cx.remove_global::<PendingDragOut>();
                        // Keep Arke's drag alive (just hidden) so it can resume if the cursor comes back.
                        cx.set_global(crate::explorer::DraggingOutside);
                        window.refresh();
                        let shell = shell.clone();
                        let handle = window.window_handle();
                        // Enter the Windows drag loop only after this mouse event is handled, outside GPUI's event dispatch.
                        cx.spawn(async move |cx| {
                            let end = shell.drag_out(&pending.paths, pending.image.as_ref());
                            if let Err(err) = &end {
                                log::warn!("drag out to another program failed: {err}");
                            }
                            let _ = cx.update_window(handle, |_, window, cx| {
                                if cx.has_global::<crate::explorer::DraggingOutside>() {
                                    cx.remove_global::<crate::explorer::DraggingOutside>();
                                }
                                if matches!(end, Ok(crate::shell::DragOutEnd::Returned)) {
                                    // Dragged back in: Arke's drag continues, and leaving again hands off to Windows again.
                                    cx.set_global(pending);
                                } else {
                                    cx.stop_active_drag(window);
                                }
                                window.refresh();
                            });
                        })
                        .detach();
                    });
                },
            )
            .absolute()
            .size_0()
            .into_any_element(),
        )
    }

    /// Content area width (right of the sidebar), from the last painted viewport width.
    fn panes_width_hint(&self) -> gpui::Pixels {
        self.last_viewport_width - theme::SIDEBAR_W
    }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("C:\\"), PathBuf::from)
}

impl Workspace {
    /// Cleanup after a drag: clears drag state and returns spring-opened folders if nothing was dropped.
    fn after_drag(&mut self, cx: &mut Context<Self>) {
        if crate::spring::needs_settle(cx) {
            cx.defer(crate::spring::settle);
        }
        if !cx.has_active_drag() {
            self.dragging_pin = false;
            self.dragging_tab = None;
            self.tab_drag.end_tile();
        }
    }
}

impl Render for Workspace {
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::frame_probe::frame_started();
        crate::a11y::sync(window);
        self.last_viewport_width = window.viewport_size().width;
        self.after_drag(cx);
        self.tab_drag.frame();
        self.sync_panes(cx);
        self.settings_follow_navigation(cx);
        self.recycle_follow_navigation(cx);
        self.home_follow_navigation(cx);
        self.save_session_if_changed(cx);
        self.refresh_compare(cx);
        let horizontal = Self::horizontal_tabs(cx);
        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(
                // Left column: title row (window drag area) and sidebar.
                div()
                    .w(theme::SIDEBAR_W)
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_col()
                    .when(!horizontal, |d| d.child(self.render_brand_row(cx)))
                    .child(self.render_home_button(cx))
                    .child(self.render_sidebar(cx))
                    .child(self.render_settings_button(cx)),
            )
            // Settings and Recycle Bin pages replace the panes; the sidebar stays.
            .child(match self.render_settings_page(window, cx) {
                Some(page) => page,
                None => match self.render_recycle_page(window, cx) {
                    Some(page) => page,
                    None => match self.render_home_page(window, cx) {
                        Some(page) => page,
                        None => self.render_panes(cx),
                    },
                },
            });
        div()
            .id("workspace")
            .key_context("Workspace")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::frame())
            .text_color(theme::text())
            .ui_font()
            .text_size(theme::FS_14)
            .when(self.resizing, gpui::Styled::cursor_col_resize)
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::close_active))
            .on_action(cx.listener(Self::reopen_closed_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(|this, _: &ToggleSyncBrowsing, _, cx| {
                this.toggle_sync(this.active, cx);
            }))
            .on_action(cx.listener(|this, _: &CompareFolders, _, cx| {
                this.toggle_compare(this.active, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectDifferences, _, cx| {
                this.select_differences(this.active, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenWorkspaces, window, cx| {
                this.open_workspaces(window, cx);
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            // Mouse back/forward anywhere in the window (the sidebar, the tab list, Home and the
            // other pages) move the active tab; over a list the pane itself handles them (#8).
            .on_mouse_down(
                MouseButton::Navigate(gpui::NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    let view = this.active_view().clone();
                    view.update(cx, |e, cx| e.go_back(&crate::explorer::GoBack, window, cx));
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(gpui::NavigationDirection::Forward),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    let view = this.active_view().clone();
                    view.update(cx, |e, cx| {
                        e.go_forward(&crate::explorer::GoForward, window, cx);
                    });
                }),
            )
            // Pressing or releasing Ctrl/Shift/Alt during a drag updates the drop action shown on the drag card.
            .on_modifiers_changed(|_, window, cx| {
                if cx.has_active_drag() {
                    window.refresh();
                }
            })
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.resizing {
                        this.resizing = false;
                        cx.notify();
                    }
                }),
            )
            .when(horizontal, |d| d.child(self.render_tab_strip(window, cx)))
            .child(body)
            .children(self.render_split_drop_zone(cx))
            // Horizontal tabs: the pin zone floats under the start of the strip, where the pinned
            // tabs are, without moving the tabs.
            .when(horizontal, |d| {
                d.children(self.pin_drop_zone(cx).map(|zone| {
                    // An opaque card under the accent wash, since it covers the sidebar (and
                    // keeps the drop from reaching what is under it).
                    div()
                        .occlude()
                        .absolute()
                        .top(theme::H_BAR + theme::S_4)
                        .left(theme::S_8)
                        .w(theme::SIDEBAR_W - theme::S_16)
                        .rounded(theme::R_8)
                        .bg(theme::overlay())
                        .shadow(theme::elev_float())
                        .child(zone)
                }))
            })
            .children(self.render_drag_out_watch(cx))
            .children(self.render_sidebar_menu(cx))
            .children(welcome::welcome_card(cx))
            .children(self.net_dialog.as_ref().map(|slot| slot.dialog.clone()))
            .children(self.workspaces.as_ref().map(|(panel, _)| panel.clone()))
            .children(self.palette.as_ref().map(|(palette, _)| {
                // Ctrl+T: clicking the backdrop closes it.
                let veil = div()
                    .id("palette-veil")
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(96.))
                    // A light scrim over the whole window (sidebar and title bar too), like
                    // Quick Look; it occludes so the list underneath shows no hover.
                    .occlude()
                    .bg(theme::scrim())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.dismiss_palette(window, cx);
                        }),
                    )
                    // Ctrl+T panel slides down and fades in (with the veil).
                    .child(crate::motion::appear(
                        div().child(palette.clone()),
                        "palette-appear",
                        0.,
                        -crate::motion::SLIDE,
                    ));
                crate::motion::appear(veil, "palette-veil-appear", 0., 0.)
            }))
            // Dismissed: the veil and panel fade out together, inert.
            .children(
                self.closing_palette
                    .as_ref()
                    .filter(|_| self.palette.is_none())
                    .map(|ghost| {
                        gpui::AnimationExt::with_animation(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .flex_col()
                                .items_center()
                                .pt(px(96.))
                                .bg(theme::scrim())
                                .child(crate::motion::inert(div().child(ghost.value.clone()))),
                            ghost.id("palette-leave"),
                            crate::motion::exit(),
                            |el, t| crate::motion::slide_out(el, t, 0., 0.),
                        )
                    }),
            )
            .children(crate::frame_probe::marker())
    }
}

impl Workspace {
    /// Vertical tabs: the sidebar top row has the Arke mark, then back, forward and refresh for the
    /// active pane (like Arc); the rest is window drag area. The mark sits on the sidebar's icon
    /// column; no name, since the taskbar and Alt+Tab already show it.
    fn render_brand_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::assets::Icon;
        use crate::explorer::{GoBack, GoForward, Refresh, ghost_button};
        let (can_back, can_forward, can_refresh) = self.active_view().read(cx).nav_state();
        let view = self.active_view().clone();
        let nav = move |run: fn(&mut Explorer, &mut Window, &mut Context<Explorer>)| {
            let view = view.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |e, cx| run(e, window, cx));
            }
        };
        div()
            .w(theme::SIDEBAR_W)
            .h(theme::H_BAR)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_2)
            // Same left edge as the sidebar items below.
            .pl(theme::S_12)
            .child(crate::brand::sidebar_mark())
            .child(if self.settings_open() || self.recycle_open() || self.home_open() {
                // With Settings or Recycle Bin open, back returns to the previous folder.
                ghost_button(
                    "back",
                    Icon::ArrowLeft,
                    true,
                    t!("nav.back"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.close_settings(&settings_panel::CloseSettings, window, cx);
                        this.close_recycle(window, cx);
                        this.close_home(window, cx);
                    }),
                )
                .into_any_element()
            } else {
                // Hovering here during a drag goes back after a countdown, an escape hatch for a wrong spring-open.
                crate::spring::back_target(
                    "sidebar",
                    crate::spring::Target::Workspace(cx.entity().downgrade()),
                    can_back,
                    ghost_button(
                        "back",
                        Icon::ArrowLeft,
                        can_back,
                        t!("nav.back"),
                        nav(|e, w, cx| e.go_back(&GoBack, w, cx)),
                    ),
                    cx,
                )
            })
            .child(ghost_button(
                "forward",
                Icon::ArrowRight,
                can_forward,
                t!("nav.forward"),
                nav(|e, w, cx| e.go_forward(&GoForward, w, cx)),
            ))
            .child(ghost_button(
                "refresh",
                Icon::Refresh,
                can_refresh,
                t!("nav.refresh"),
                nav(|e, w, cx| e.refresh(&Refresh, w, cx)),
            ))
            .child(drag_area("brand", cx).h_full().flex_1().min_w_0())
    }

    /// Horizontal tabs: tabs on the top row like a browser, caption buttons on the far right.
    fn render_tab_strip(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs: Vec<AnyElement> = (0..self.tabs.len())
            .map(|ix| {
                // Pinned tabs are narrow and don't shrink, like a browser's.
                let pinned = self.tabs[ix].pinned;
                div()
                    .when(pinned, |d| d.w(px(132.)).flex_none())
                    .when(!pinned, |d| d.w(px(200.)).min_w(px(96.)).flex_shrink(1.))
                    .child(self.render_tab(ix, cx))
                    .into_any_element()
            })
            .collect();
        div()
            .id("tab-strip")
            .a11y(gpui::Role::TabList, t!("sidebar.tabs"))
            // Dropping a tab between tabs or on the empty space to the right moves it to the end.
            .on_drop(cx.listener(Self::drop_tab_at_end))
            .h(theme::H_BAR)
            .pr(theme::S_8)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_4)
            .pl(theme::S_8)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::S_4)
                    .min_w_0()
                    .overflow_hidden()
                    .children(tabs),
            )
            .child(
                div()
                    .id("strip-new-tab")
                    .a11y_button(t!("sidebar.new_tab"))
                    .aria_keyshortcuts("Ctrl+T")
                    .size(theme::H_CTL)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(theme::R_8)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::wash_strong()))
                    .child(
                        gpui::svg()
                            .path(crate::assets::Icon::Plus.path())
                            .size(theme::ICON)
                            .text_color(theme::text_muted()),
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_palette(&OpenPalette, window, cx);
                    })),
            )
            .child(
                drag_area("strip-drag", cx)
                    .flex_1()
                    .min_w(theme::S_32)
                    .h_full(),
            )
            .child(Self::workspaces_button(cx))
            .child(sidebar::tab_layout_button(true, cx))

            .child(crate::explorer::caption_buttons(window))
    }
}

/// Window drag area (the system handles double click to maximize); right click opens Arke's window menu.
fn drag_area(id: &'static str, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .window_control_area(WindowControlArea::Drag)
        // Clicking empty title bar space ends path editing without blocking the window drag.
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                let view = this.active_view().clone();
                view.update(cx, |v, cx| v.cancel_path_edit(window, cx));
            }),
        )
        .on_mouse_up(
            gpui::MouseButton::Right,
            cx.listener(|this, e: &gpui::MouseUpEvent, window, cx| {
                this.open_window_menu(e.position, window, cx);
            }),
        )
}

impl Focusable for Workspace {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.page_focus(cx)
            .unwrap_or_else(|| self.active_view().read(cx).focus_handle(cx))
    }
}

/// Plain text tooltip.
pub(crate) struct Tooltip(pub(crate) SharedString);

/// Space between a control and a tooltip placed below it.
const TOOLTIP_GAP: gpui::Pixels = theme::S_8;

/// Text tooltip that sits [`TOOLTIP_GAP`] below its control instead of at the mouse, so it never
/// covers the neighbouring buttons in a toolbar. `pad` is the empty space above the card, from
/// where gpui puts tooltips (just below the mouse) down to the control's bottom edge plus the gap.
pub(crate) struct TooltipBelow {
    text: SharedString,
    pad: gpui::Pixels,
}

impl Render for TooltipBelow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().pt(self.pad).child(tooltip_card(&self.text))
    }
}

/// `.tooltip_below(text)` for toolbar controls: see [`TooltipBelow`].
pub(crate) trait TooltipBelowExt:
    gpui::StatefulInteractiveElement + gpui::ParentElement + Sized
{
    /// Shows `text` 8 px below this control. Adds an invisible probe child that records the
    /// control's bounds each frame (the tooltip builder only knows the mouse position); without
    /// a measurement yet it falls back to gpui's spot under the mouse.
    #[must_use]
    fn tooltip_below(self, text: impl Into<SharedString>) -> Self {
        let text: SharedString = text.into();
        let anchor: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>> =
            std::rc::Rc::default();
        let probe = anchor.clone();
        self.child(
            gpui::canvas(
                move |bounds, _, _| probe.set(Some(bounds)),
                |_, (), _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .tooltip(move |window, cx| {
            // gpui places the tooltip 1 px below the mouse.
            let top = window.mouse_position().y + px(1.);
            let pad = anchor
                .get()
                .map_or(px(0.), |b| (b.bottom() + TOOLTIP_GAP - top).max(px(0.)));
            let text = text.clone();
            cx.new(|_| TooltipBelow { text, pad }).into()
        })
    }
}

impl<E: gpui::StatefulInteractiveElement + gpui::ParentElement> TooltipBelowExt for E {}

/// Text tooltip for `.tooltip(...)`.
pub(crate) fn tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text: SharedString = text.into();
    move |_, cx| cx.new(|_| Tooltip(text.clone())).into()
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        tooltip_card(&self.0)
    }
}

/// The tooltip bubble itself.
fn tooltip_card(text: &SharedString) -> gpui::Div {
    // Tooltips lay out at minimum size (max_w has no effect), so long text gets an explicit width to wrap.
    let long = text.chars().count() > 48;
    div()
        .px(theme::S_8)
        .py(theme::S_4)
        .when(long, |d| d.w(theme::TOOLTIP_MAX_W))
        .rounded(theme::R_8)
        .bg(theme::overlay())
        .shadow(theme::elev_float())
        .ui_font()
        .text_size(theme::FS_12)
        .text_color(theme::text())
        .child(text.clone())
}
