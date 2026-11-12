//! Arke: a file explorer for Windows that never hangs.
//!
//! Usage:
//!   arke [path]
//!   arke <path> --bench startup|scroll|scroll-grid [--bench-out file]
//!   arke --bench index [--bench-out file]   build the search index headless, report time and
//!                                           memory (see `bench_index.rs`)
//!   arke --no-resident [path]   don't use or become the resident process
//!   arke --quit                 quit the resident process
//!   arke --home                 open Home (what Win+E runs while "Open folders in Arke" is on)
//!   arke --restore-explorer     undo the folder handler registration and turn the setting off
//!   arke --uninstall [--keep-settings] [--quiet] [--dry-run]
//!                               remove Arke from this PC (see `uninstall.rs`)
//!   arke --shell-host           (internal) hidden context menu process, see `shell_host.rs`
//!   arke --pick-file            (internal) one file dialog for Settings, see `win_picker.rs`
//!
//! By default the first Arke stays in the background after its last window closes, so later
//! launches only open a window without reinitializing the GPU. See `instance.rs`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Tests may unwrap and panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

// First, so the `tn!` macro is visible in every module below.
#[macro_use]
mod plural;

mod a11y;
mod assets;
mod bench;
mod bench_index;
mod brand;
mod build_info;
mod crashlog;
mod explorer;
mod folder_handler;
mod folder_views;
mod frame_probe;
mod global_search;
mod home;
mod instance;
mod media;
mod motion;
mod net_dialog;
mod op_center;
mod op_queue;
mod palette;
mod prefs;
mod recycle_bin;
mod remote_shell;
mod saved_workspaces;
mod scrollbar;
mod search_history;
mod settings;
mod shelf;
mod shell;
mod shell_host;
mod slider;
mod spring;
mod teracopy;
mod text_input;
mod theme;
mod tray;
mod uninstall;
mod update;
mod win;
mod win_cloud;
mod win_devnotify;
mod win_dragimage;
mod win_eject;
mod win_fileops;
mod win_http;
mod win_icons;
mod win_inuse;
mod win_jumplist;
mod win_menu;
mod win_net;
mod win_openinfolder;
mod win_pdf;
mod win_picker;
mod win_purge;
mod win_recycle;
mod win_registry;
mod win_takeover;
mod workspace;

#[cfg(test)]
mod tests;

// UI string tables (locales/*.yml, embedded at compile time); falls back to English
rust_i18n::i18n!("locales", fallback = "en");

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arke_core::RealFs;
use gpui::{
    App, AppContext, Bounds, QuitMode, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_platform::application;

use crate::assets::Assets;
use crate::bench::{Bench, BenchMode};
use crate::explorer::Services;
use crate::instance::{OpenRequest, Page, Request, SendOutcome};
use crate::win::WinShell;
use crate::workspace::Workspace;
use arke_core::takeover::Target;

/// What `--bench` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BenchArg {
    /// Opens a window and measures it (`bench.rs`)
    Window(BenchMode),
    /// `--bench index`: headless index benchmark, no window (`bench_index.rs`)
    Index,
}

// One bool per command line flag.
#[allow(clippy::struct_excessive_bools)]
struct Args {
    path: Option<PathBuf>,
    bench: Option<BenchArg>,
    bench_out: Option<PathBuf>,
    resident: bool,
    quit: bool,
    /// Stay in the background without opening a window (used for autostart at Windows sign-in)
    background: bool,
    /// Open a page instead of a folder (`--home`, or the folder handler got Home / the Recycle Bin)
    page: Option<Page>,
    /// Undo the folder handler registration, then exit
    restore_explorer: bool,
}

fn parse_args() -> Args {
    parse_args_from(std::env::args_os().skip(1))
}

fn parse_args_from(mut it: impl Iterator<Item = std::ffi::OsString>) -> Args {
    let mut args = Args {
        path: None,
        bench: None,
        bench_out: None,
        resident: true,
        quit: false,
        background: false,
        page: None,
        restore_explorer: false,
    };
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--bench") => {
                args.bench = Some(match it.next().as_ref().and_then(|s| s.to_str()) {
                    Some("index") => BenchArg::Index,
                    Some("scroll") => BenchArg::Window(BenchMode::Scroll),
                    Some("scroll-grid") => BenchArg::Window(BenchMode::ScrollGrid),
                    _ => BenchArg::Window(BenchMode::Startup),
                });
            }
            Some("--bench-out") => args.bench_out = it.next().map(PathBuf::from),
            Some("--no-resident") => args.resident = false,
            Some("--quit") => args.quit = true,
            Some("--background") => args.background = true,
            Some("--home") => args.page = Some(Page::Home),
            Some("--restore-explorer") => args.restore_explorer = true,
            // Just updated: the old process's pid follows (already waited for at the start of
            // app_main).
            Some(update::AFTER_UPDATE_ARG) => {
                let _ = it.next();
            }
            _ => args.path = Some(path_arg(&arg)),
        }
    }
    args
}

/// A folder argument as the shell passes it. The registered command is `"arke.exe" "%1"`, and for a
/// drive root `%1` ends in a backslash (`"C:\"`), which the command line rules read as an escaped
/// quote: the argument arrives as `C:"`. The quote is the leftover of that, so it goes and the
/// backslash comes back.
fn path_arg(arg: &std::ffi::OsStr) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let mut units: Vec<u16> = arg.encode_wide().collect();
    if units.last() == Some(&u16::from(b'"')) {
        units.pop();
        if units.last() != Some(&u16::from(b'\\')) {
            units.push(u16::from(b'\\'));
        }
    }
    PathBuf::from(std::ffi::OsString::from_wide(&units))
}

impl Args {
    /// The window benchmark to run, if any.
    const fn window_bench(&self) -> Option<BenchMode> {
        match self.bench {
            Some(BenchArg::Window(mode)) => Some(mode),
            _ => None,
        }
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Convert another process's launch time to an `Instant` here, so measurements start at launch.
fn instant_from_unix_ms(launched_unix_ms: u64) -> Instant {
    let ago = Duration::from_millis(unix_ms_now().saturating_sub(launched_unix_ms));
    let now = Instant::now();
    now.checked_sub(ago).unwrap_or(now)
}

/// Single binary: `arke.exe --indexd <command>` is the background service (arke-indexd) and opens
/// no window.
const INDEXD_ARG: &str = "--indexd";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(INDEXD_ARG) {
        return arke_indexd::cli::run(args[2..].to_vec(), Some(INDEXD_ARG));
    }
    // Before app_main: no log file (it would hold %LOCALAPPDATA%\Arke open), residency or window.
    if args.get(1).map(String::as_str) == Some(uninstall::ARG) {
        rust_i18n::set_locale(&win::ui_locale());
        return uninstall::run_cli(&args[2..]);
    }
    // A Browse button's file dialog, shown in its own process (see `win_picker.rs`).
    if args.get(1).map(String::as_str) == Some(win_picker::PICK_ARG) {
        win_picker::run();
        return std::process::ExitCode::SUCCESS;
    }
    let mut args = parse_args();
    // As the folder handler the shell hands over every folder, virtual ones included: what Arke
    // can't show goes to Explorer right away, before anything is initialized.
    if let Some(target) = args.path.as_deref().and_then(Path::to_str) {
        match arke_core::takeover::route_argument(target) {
            arke_core::takeover::Route::Explorer(target) => {
                win::open_in_explorer(&target);
                return std::process::ExitCode::SUCCESS;
            }
            arke_core::takeover::Route::Arke(Target::Home) => {
                args.path = None;
                args.page = Some(Page::Home);
            }
            arke_core::takeover::Route::Arke(Target::RecycleBin) => {
                args.path = None;
                args.page = Some(Page::RecycleBin);
            }
            arke_core::takeover::Route::Arke(Target::Folder(_)) => {}
        }
    }
    // Nothing else starts: no log file, tray, window or resident process.
    if args.restore_explorer {
        return if folder_handler::restore_cli() {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        };
    }
    // Before anything in app_main: no log file, residency handoff, tray or window.
    if args.bench == Some(BenchArg::Index) {
        return bench_index::run(args.bench_out.as_deref());
    }
    app_main(args);
    std::process::ExitCode::SUCCESS
}

// Startup order (args, handoff to the resident process, window, settings-driven pieces) reads best
// top to bottom in one place.
#[allow(clippy::too_many_lines)]
fn app_main(mut args: Args) {
    if run_shell_host() {
        return;
    }
    // Launched by the old version (auto-update): wait for it to exit before taking over residency.
    update::wait_for_previous(&std::env::args().collect::<Vec<_>>());
    let started_at = Instant::now();
    let launched_unix_ms = unix_ms_now();
    crashlog::init();
    motion::refresh_system_setting();
    rust_i18n::set_locale(&win::ui_locale());
    theme::set_mica_available(use_mica());
    // From here any filesystem call on this thread trips a debug assertion.
    arke_core::fs::mark_ui_thread();

    // Relative paths resolve against the launching process's cwd, so make them absolute before
    // handing off. std::path::absolute is string handling only (GetFullPathNameW) and never touches
    // the filesystem.
    if let Some(path) = &args.path
        && let Ok(abs) = std::path::absolute(path)
    {
        args.path = Some(abs);
    }

    let linger = shell_window(&args);
    let listener = if args.background {
        // Background launch: if a resident process exists we're not needed; otherwise become
        // resident without a window.
        match instance::listen() {
            Some(listener) => Some(listener),
            None => return,
        }
    } else if args.resident || args.quit {
        let request = if args.quit {
            Request::Quit
        } else {
            Request::Open(OpenRequest {
                path: args.path.clone(),
                bench: args.window_bench(),
                bench_out: args.bench_out.clone(),
                launched_unix_ms,
                page: args.page,
            })
        };
        match hand_off(&request) {
            HandOff::Done => {
                // A "Show in folder" caller may still be selecting: stay until the stand-in
                // shell window is done.
                if let Some(linger) = linger {
                    linger.wait();
                }
                return;
            }
            HandOff::BecomeResident(listener) => Some(listener),
            HandOff::Standalone => None,
        }
    } else {
        None
    };
    if args.quit {
        log::info!("no resident Arke running");
        return;
    }

    let background = args.background;
    let bench = args
        .window_bench()
        .map(|mode| Bench::new(mode, args.bench_out));
    let start_path = args.path;
    let start_page = args.page;
    let resident = listener.is_some();
    log::info!(
        "{}",
        if resident {
            "resident mode"
        } else {
            "standalone mode"
        }
    );

    let app = application().with_assets(Assets);
    // gpui on Windows quits when the last window closes; resident mode decides when to quit itself.
    let app = if resident {
        app.with_quit_mode(QuitMode::Explicit)
    } else {
        app
    };
    app.run(move |cx: &mut App| {
        motion::sync_app(cx);
        frame_probe::init();
        text_input::bind_keys(cx);
        explorer::bind_keys(cx);
        workspace::bind_keys(cx);
        palette::bind_keys(cx);
        // Settings load in the background; benchmarks neither read nor write, so test folders don't
        // land in Recent.
        let store = settings::SettingsStore::init(
            if bench.is_some() {
                None
            } else {
                settings::default_path()
            },
            cx,
        );
        // Per-folder views, next to the settings file; benchmarks keep them in memory only
        folder_views::FolderViewStore::init(
            if bench.is_some() {
                None
            } else {
                folder_views::default_path()
            },
            cx,
        );
        // What the user opens, for ranking "This PC" results; benchmarks keep it in memory only
        search_history::SearchHistory::init(
            if bench.is_some() {
                None
            } else {
                search_history::default_path()
            },
            cx,
        );
        // Saved workspaces, next to the settings file too
        saved_workspaces::WorkspaceStore::init(
            if bench.is_some() {
                None
            } else {
                saved_workspaces::default_path()
            },
            cx,
        );
        // The shelf, next to the settings file; benchmarks keep it in memory only
        shelf::ShelfStore::init(
            if bench.is_some() {
                None
            } else {
                shelf::default_path()
            },
            cx,
        );
        follow_language_setting(&store, cx);
        let remotes = remote_shell::start(&store, cx);
        let services = Services {
            fs: Arc::new(arke_remote::RoutingFs::new(
                Arc::new(RealFs),
                remotes.clone(),
            )),
            shell: Arc::new(remote_shell::RemoteShell::new(
                Arc::new(WinShell),
                remotes.clone(),
            )),
            remotes,
        };
        // "Open in VS Code/Zed": check in the background whether they're installed.
        if bench.is_none() {
            let shell = services.shell.clone();
            explorer::spawn_thread("editor-apps", move || {
                let apps = shell.editor_apps();
                // Icons come from each exe (icon only, no thumbnail provider).
                let icons = apps
                    .iter()
                    .map(|(_, exe)| shell.file_icon(exe, 32))
                    .collect();
                explorer::set_open_with(apps, icons);
            });
        }
        if bench.is_none() {
            follow_drive_removals(cx);
            teracopy::detect_in_background();
        }
        if resident && bench.is_none() {
            install_tray(cx, &services, &store);
            follow_autostart_setting(&store, &services, cx);
            follow_takeover_setting(&store, &services, cx);
        }
        // The shell registration follows the setting in every process that is running, resident
        // or not (a standalone Arke started through the folder verb keeps it repaired too).
        if bench.is_none()
            && let Some(handler) = folder_handler::Handler::real()
        {
            folder_handler::follow_setting(&store, handler, cx);
        }
        if bench.is_none() {
            follow_jump_list(&store, cx);
            follow_start_menu_setting(&store, &services, cx);
        }
        // Global search: index only when enabled; not during benchmarks.
        if bench.is_none() {
            global_search::GlobalSearch::init(
                services.fs.clone(),
                services.shell.clone(),
                global_search::Roots::LocalDrives,
                global_search::default_cache_path(),
                cx,
            );
        }
        // Auto-update: delete the old binary left from last time; check a bit after startup so it
        // doesn't compete.
        if bench.is_none() {
            explorer::spawn_thread("update-cleanup", update::clean_up_old);
            cx.spawn(async |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                cx.update(update::auto_check);
            })
            .detach();
        }
        let benchmarking = bench.is_some();
        let first_window_opened = if background {
            true
        } else if let Some(page) = start_page {
            open_target(cx, &services, page.target(), &[], None)
        } else {
            open_explorer(cx, &services, start_path, bench, started_at)
        };
        if !first_window_opened && !resident {
            cx.quit();
            return;
        }
        if let Some(listener) = listener {
            start_resident_service(cx, listener, services);
            quit_with_last_window_unless_resident(cx);
        }
        if !benchmarking {
            prewarm_shell_host();
        }
        if !background {
            cx.activate(true);
        }
    });
}

/// Drives removed, locked or dismounted from outside Arke: watchers on them were already closed
/// on the notification thread (`win_devnotify.rs`); here every window moves its tabs off the drive.
fn follow_drive_removals(cx: &mut App) {
    let events = win_devnotify::start();
    cx.spawn(async move |cx| {
        while let Ok(event) = events.recv_async().await {
            let win_devnotify::DriveEvent::Leaving(drive) = event;
            cx.update(|cx| workspace::leave_drive_everywhere(&drive, cx));
        }
    })
    .detach();
}

/// "Start at Windows sign-in": when on, put a shortcut in the Startup folder pointing at the
/// current exe (rewritten every launch so it follows a moved exe); when off, delete it. Runs in the
/// background, no registry.
fn follow_autostart_setting(
    store: &gpui::Entity<settings::SettingsStore>,
    services: &Services,
    cx: &mut App,
) {
    let shell = services.shell.clone();
    let mut applied: Option<bool> = None;
    cx.observe(store, move |store, cx| {
        let store = store.read(cx);
        // Before settings load the values are defaults; don't delete the shortcut based on them.
        if !store.loaded {
            return;
        }
        let wanted = store.settings.autostart;
        if applied == Some(wanted) {
            return;
        }
        // First time seeing "off" and never on before: no need to touch the filesystem.
        let first = applied.is_none();
        applied = Some(wanted);
        if first && !wanted {
            return;
        }
        let shell = shell.clone();
        explorer::spawn_thread("autostart", move || {
            if let Err(err) = shell.set_autostart(wanted) {
                log::warn!("autostart shortcut not updated: {err}");
            }
        });
    })
    .detach();
}

/// "Add to the Start menu": when on, write the Start menu shortcut at every start (so it follows a
/// moved exe) and whenever it is turned on; when turned off, delete it. Background thread, no
/// registry.
fn follow_start_menu_setting(
    store: &gpui::Entity<settings::SettingsStore>,
    services: &Services,
    cx: &mut App,
) {
    let shell = services.shell.clone();
    let mut applied: Option<bool> = None;
    let mut apply = move |store: &settings::SettingsStore| {
        if !store.loaded {
            return;
        }
        let wanted = store.settings.start_menu;
        if applied == Some(wanted) {
            return;
        }
        let first = applied.is_none();
        applied = Some(wanted);
        if first && !wanted {
            return;
        }
        let shell = shell.clone();
        explorer::spawn_thread("start-menu", move || {
            if let Err(err) = shell.set_start_menu(wanted) {
                log::warn!("Start menu shortcut not updated: {err}");
            }
        });
    };
    apply(store.read(cx));
    cx.observe(store, move |store, cx| apply(store.read(cx)))
        .detach();
}

/// "Open folders in Arke": while the setting is on (and Arke stays in the background), watch for new
/// File Explorer windows and show their folders here instead (`win_takeover.rs`). Turning the
/// setting off stops the watcher at once.
fn follow_takeover_setting(
    store: &gpui::Entity<settings::SettingsStore>,
    services: &Services,
    cx: &mut App,
) {
    let (tx, rx) = flume::unbounded::<win_takeover::OpenRequest>();
    let services = services.clone();
    cx.spawn(async move |cx| {
        while let Ok(request) = rx.recv_async().await {
            let opened = cx.update(|cx| open_taken_over(cx, &services, &request));
            let _ = request.reply.send(opened);
        }
    })
    .detach();
    let watcher: std::rc::Rc<std::cell::RefCell<Option<win_takeover::Watcher>>> =
        std::rc::Rc::default();
    let apply = move |store: &settings::SettingsStore| {
        // Before settings load the values are defaults.
        if !store.loaded {
            return;
        }
        let wanted = store.settings.open_folders_in_arke && store.settings.keep_running;
        tray::set_takeover(wanted);
        let mut watcher = watcher.borrow_mut();
        if wanted && watcher.is_none() {
            log::info!("takeover: starting");
            *watcher = win_takeover::Watcher::start(tx.clone());
        } else if !wanted {
            *watcher = None;
        }
    };
    // Settings may already be loaded; the observer only hears later changes.
    apply(store.read(cx));
    cx.observe(store, move |store, cx| apply(store.read(cx)))
        .detach();
}

/// Opens a window showing what an Explorer window showed. Answers whether Arke has a window.
fn open_taken_over(cx: &mut App, services: &Services, request: &win_takeover::OpenRequest) -> bool {
    open_target(
        cx,
        services,
        request.target.clone(),
        &request.select,
        Some(request.explorer_thread),
    )
}

/// Opens a window for a folder, Home or the Recycle Bin (with `select` selected) and brings it to
/// the front. `explorer_thread` is the thread of the Explorer window being replaced, if any.
fn open_target(
    cx: &mut App,
    services: &Services,
    target: Target,
    select: &[String],
    explorer_thread: Option<u32>,
) -> bool {
    let start = match &target {
        Target::Folder(path) => path.clone(),
        Target::Home | Target::RecycleBin => {
            std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("C:\\"), PathBuf::from)
        }
    };
    let Some(window) = open_explorer_window(cx, services, Some(start), None, Instant::now()) else {
        return false;
    };
    let select: Vec<Arc<str>> = select.iter().map(|n| Arc::from(n.as_str())).collect();
    let shown = window.update(cx, |workspace, window, cx| {
        workspace.show_taken_over(target, select, window, cx);
        bring_to_front(window, explorer_thread);
    });
    if shown.is_ok() {
        cx.activate(true);
    }
    shown.is_ok()
}

/// Brings Arke's window to the front, even when another program owns the foreground (a window
/// opened for a File Explorer window that was just replaced, or by a folder the shell handed over).
fn bring_to_front(window: &gpui::Window, explorer_thread: Option<u32>) {
    if let Some(hwnd) = explorer::Explorer::owner_from(window) {
        win::bring_to_front(hwnd, explorer_thread.filter(|&t| t != 0));
    }
}

/// "Show in folder" while Arke is the folder handler: a stand-in shell window for the folder that
/// collects what the caller selects (`win_openinfolder.rs`) and forwards it to the running Arke.
fn shell_window(args: &Args) -> Option<win_openinfolder::Linger> {
    let folder = args.path.as_ref().filter(|p| p.is_absolute())?;
    if args.background
        || args.quit
        || args.page.is_some()
        || args.bench.is_some()
        || std::env::var_os("ARKE_NO_SHELL_WINDOW").is_some()
    {
        return None;
    }
    win_openinfolder::start(folder.clone(), |folder, items| {
        let count = items.len();
        let outcome = instance::send(&Request::Select { folder, items }, instance::CLIENT_TIMEOUT);
        log::info!("shell window: {count} selected items forwarded: {outcome:?}");
    })
}

/// Selects `items` in the newest window that shows (or is loading) their folder, or opens one.
fn select_items(cx: &mut App, services: &Services, folder: &Path, items: &[PathBuf]) -> bool {
    let full: Vec<String> = items
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let names = arke_core::takeover::selected_names(folder, &full);
    if names.is_empty() {
        return false;
    }
    let shown: Vec<Arc<str>> = names.iter().map(|n| Arc::from(n.as_str())).collect();
    for handle in cx.windows().into_iter().rev() {
        let Some(workspace) = handle.downcast::<Workspace>() else {
            continue;
        };
        let done = workspace.update(cx, |workspace, _, cx| {
            workspace.active_view().update(cx, |view, cx| {
                view.select_in_folder(folder, shown.clone(), cx)
            })
        });
        if matches!(done, Ok(true)) {
            log::info!(
                "selected {} items in the window showing {}",
                shown.len(),
                folder.display()
            );
            return true;
        }
    }
    open_target(
        cx,
        services,
        Target::Folder(folder.to_path_buf()),
        &names,
        None,
    )
}

/// Keep the taskbar jump list in sync with pinned and recent folders. One background thread handles
/// only the latest request, so quick folder changes don't queue.
fn follow_jump_list(store: &gpui::Entity<settings::SettingsStore>, cx: &mut App) {
    type Lists = (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>);
    let (tx, rx) = flume::unbounded::<Lists>();
    explorer::spawn_thread("jump-list", move || {
        while let Ok(mut lists) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                lists = newer;
            }
            if let Err(err) = win_jumplist::update(&lists.0, &lists.1) {
                log::info!("jump list not updated: {err}");
            }
        }
    });
    let mut applied: Option<Lists> = None;
    cx.observe(store, move |store, cx| {
        let store = store.read(cx);
        if !store.loaded {
            return;
        }
        let s = &store.settings;
        let pinned: Vec<_> = s
            .favorites
            .iter()
            .flatten()
            .chain(&s.pinned)
            .cloned()
            .collect();
        let recent: Vec<_> = s.recent.iter().take(12).cloned().collect();
        let lists = (pinned, recent);
        if applied.as_ref() == Some(&lists) {
            return;
        }
        applied = Some(lists.clone());
        let _ = tx.send(lists);
    })
    .detach();
}

/// "Language" setting: switch when loaded or changed and redraw all windows (input placeholders
/// update in new tabs only).
fn follow_language_setting(store: &gpui::Entity<settings::SettingsStore>, cx: &mut App) {
    let mut applied: Option<settings::Language> = None;
    cx.observe(store, move |store, cx| {
        let language = store.read(cx).settings.language;
        if applied == Some(language) {
            return;
        }
        applied = Some(language);
        apply_language(&language.locale(), cx);
    })
    .detach();
    // Also apply what is set now: the observer only hears later changes.
    apply_language(&store.read(cx).settings.language.locale(), cx);
}

/// Switches strings and the CJK font fallback order to `locale`, repainting if either changed.
fn apply_language(locale: &str, cx: &mut App) {
    // Fonts follow the UI language so Simplified text gets Simplified glyphs on a Traditional
    // system and vice versa (see `theme::cjk_fallbacks`).
    let mut changed = theme::set_ui_language(locale, win::system_locale());
    if *rust_i18n::locale() != *locale {
        rust_i18n::set_locale(locale);
        changed = true;
    }
    if changed {
        cx.refresh_windows();
    }
}

/// In resident mode with "Keep running in background" off: quit when the last window closes.
fn quit_with_last_window_unless_resident(cx: &mut App) {
    cx.on_window_closed(|cx, _| {
        let keep = settings::SettingsStore::global(cx)
            .read(cx)
            .settings
            .keep_running;
        if !keep && cx.windows().is_empty() {
            log::info!("background residency off: last window closed, quitting");
            cx.quit();
        }
    })
    .detach();
}

/// Mica backdrop on Windows 11. Off when ARKE_THEME forces light or dark, since Mica follows the
/// system theme and a mismatch looks washed out. `ARKE_NO_MICA` disables it.
fn use_mica() -> bool {
    win::supports_mica()
        && std::env::var_os("ARKE_NO_MICA").is_none()
        && std::env::var_os("ARKE_THEME").is_none()
}

/// New window size: 75% of the usable screen (so large screens don't get a tiny window), clamped to
/// 1180×760 .. 2200×1400.
fn default_window_size(cx: &App) -> gpui::Size<gpui::Pixels> {
    let (min, max) = (size(px(1180.), px(760.)), size(px(2200.), px(1400.)));
    let Some(screen) = cx.primary_display().map(|d| d.visible_bounds().size) else {
        return min;
    };
    let fit = |want: gpui::Pixels, lo: gpui::Pixels, hi: gpui::Pixels, room: gpui::Pixels| {
        // Screens smaller than the minimum get 95% of the screen.
        (want.max(lo).min(hi)).min(room * 0.95)
    };
    size(
        fit(screen.width * 0.75, min.width, max.width, screen.width),
        fit(screen.height * 0.75, min.height, max.height, screen.height),
    )
}

/// Last window position, used only if at least a 240×120 patch is still on some screen (so it never
/// opens off-screen after unplugging a monitor).
fn restored_bounds(place: settings::WindowPlace, cx: &App) -> Option<WindowBounds> {
    let bounds = Bounds::new(
        gpui::point(px(place.x), px(place.y)),
        size(px(place.width.max(720.)), px(place.height.max(480.))),
    );
    let visible = cx.displays().iter().any(|d| {
        let overlap = d.bounds().intersect(&bounds);
        overlap.size.width >= px(240.) && overlap.size.height >= px(120.)
    });
    visible.then_some(if place.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    })
}

/// `arke.exe --shell-host`: the menu process. No window, no residency or settings; only serves menu
/// requests from Arke.
fn run_shell_host() -> bool {
    if std::env::args_os()
        .nth(1)
        .is_none_or(|a| a != shell_host::HOST_ARG)
    {
        return false;
    }
    rust_i18n::set_locale(&win::ui_locale());
    shell_host::run();
    true
}

/// Start the "More options" menu process early (it reads the menu once) so the first right-click
/// doesn't wait.
fn prewarm_shell_host() {
    let _ = std::thread::Builder::new()
        .name("shell-host-prewarm".into())
        .spawn(|| {
            std::thread::sleep(Duration::from_secs(1));
            shell_host::prewarm();
        });
}

enum HandOff {
    /// Resident process opened the window; this process can exit
    Done,
    /// No resident process; this one becomes resident
    BecomeResident(instance::Listener),
    /// Resident process unresponsive (or we lost the race and can't connect); open a window here
    /// without residency
    Standalone,
}

fn hand_off(request: &Request) -> HandOff {
    for _ in 0..2 {
        match instance::send(request, instance::CLIENT_TIMEOUT) {
            SendOutcome::Delivered => return HandOff::Done,
            SendOutcome::Failed => {
                log::warn!("resident process did not respond in time, running standalone");
                return HandOff::Standalone;
            }
            SendOutcome::NoServer => {
                if matches!(request, Request::Quit) {
                    return HandOff::Standalone;
                }
                if let Some(listener) = instance::listen() {
                    return HandOff::BecomeResident(listener);
                }
                // Two Arkes launched together and the other became resident first: retry the
                // handoff once.
            }
        }
    }
    HandOff::Standalone
}

fn start_resident_service(cx: &mut App, listener: instance::Listener, services: Services) {
    let (tx, rx) = flume::unbounded::<(Request, flume::Sender<bool>)>();
    instance::serve(listener, move |request| {
        let (ack_tx, ack_rx) = flume::bounded(1);
        if tx.send((request, ack_tx)).is_err() {
            return false;
        }
        // If the UI thread doesn't handle it in time, reply failure so the sender opens its own
        // window.
        ack_rx
            .recv_timeout(instance::SERVER_ACK_TIMEOUT)
            .unwrap_or(false)
    });
    cx.spawn(async move |cx| {
        while let Ok((request, ack)) = rx.recv_async().await {
            let handled = cx.update(|cx| match request {
                Request::Quit => {
                    cx.quit();
                    true
                }
                Request::Select { folder, items } => select_items(cx, &services, &folder, &items),
                Request::Open(open) => {
                    let started_at = instant_from_unix_ms(open.launched_unix_ms);
                    let bench = open.bench.map(|mode| Bench::new(mode, open.bench_out));
                    // The sender was just started by the user or the shell and passed on its
                    // foreground right (instance.rs), which `bring_to_front` makes use of.
                    if let Some(page) = open.page {
                        open_target(cx, &services, page.target(), &[], None)
                    } else {
                        match open_explorer_window(cx, &services, open.path, bench, started_at) {
                            Some(handle) => {
                                let _ =
                                    handle.update(cx, |_, window, _| bring_to_front(window, None));
                                cx.activate(true);
                                true
                            }
                            None => false,
                        }
                    }
                }
            });
            let _ = ack.send(handled);
        }
    })
    .detach();
}

/// Resident mode: a notification area icon so Arke is reachable with all windows closed (left click
/// opens a window, the menu can quit). `ARKE_NO_TRAY` skips it (temporary processes for automated
/// tests and screenshots).
fn install_tray(cx: &mut App, services: &Services, store: &gpui::Entity<settings::SettingsStore>) {
    if std::env::var_os("ARKE_NO_TRAY").is_some() {
        return;
    }
    let Some(commands) = tray::install() else {
        return;
    };
    // The setting can turn it off: add or remove when settings load or change. This runs on every
    // settings save (folder changes included), so do nothing and log nothing if the state is
    // unchanged.
    let last = std::cell::Cell::new(None);
    let show = move |on: bool| {
        if last.replace(Some(on)) == Some(on) {
            return;
        }
        if !tray::set_visible(on) {
            log::warn!("cannot add notification area icon");
        } else if on {
            log::info!("notification area icon added");
        }
    };
    show(store.read(cx).settings.tray_icon);
    cx.observe(store, move |store, cx| {
        show(store.read(cx).settings.tray_icon);
    })
    .detach();
    let services = services.clone();
    cx.spawn(async move |cx| {
        while let Ok(command) = commands.recv_async().await {
            cx.update(|cx| match command {
                tray::TrayCommand::Open => {
                    if open_explorer(cx, &services, None, None, Instant::now()) {
                        cx.activate(true);
                    }
                }
                tray::TrayCommand::ToggleTakeover => {
                    settings::SettingsStore::global(cx).update(cx, |store, cx| {
                        store.update(
                            |st| {
                                st.open_folders_in_arke = !st.open_folders_in_arke;
                                if st.open_folders_in_arke {
                                    st.keep_running = true;
                                }
                            },
                            cx,
                        );
                    });
                }
                tray::TrayCommand::Quit => {
                    tray::remove();
                    cx.quit();
                }
            });
        }
    })
    .detach();
    // Quitting from elsewhere (arke --quit, last window closed) also removes the icon, so no ghost
    // icon lingers until hovered.
    cx.on_app_quit(|_| async { tray::remove() }).detach();
}

fn open_explorer(
    cx: &mut App,
    services: &Services,
    path: Option<PathBuf>,
    bench: Option<Bench>,
    started_at: Instant,
) -> bool {
    open_explorer_window(cx, services, path, bench, started_at).is_some()
}

fn open_explorer_window(
    cx: &mut App,
    services: &Services,
    path: Option<PathBuf>,
    bench: Option<Bench>,
    started_at: Instant,
) -> Option<gpui::WindowHandle<Workspace>> {
    // The startup path is string handling only; existence isn't checked, since startup must not
    // touch locations that might block. No path given: if settings say "last location/last tabs",
    // open there (switching once settings finish loading).
    let first_window = cx.windows().is_empty();
    let (last, session, place) = {
        let store = settings::SettingsStore::global(cx).read(cx);
        let start = store.settings.start_location;
        let last = (path.is_none() && store.loaded && start == settings::StartLocation::LastFolder)
            .then(|| store.settings.recent.first().cloned())
            .flatten();
        // Restore last tabs only when no other window exists, otherwise each new window would
        // duplicate them.
        let session = (path.is_none()
            && store.loaded
            && first_window
            && start == settings::StartLocation::LastSession)
            .then(|| store.settings.session.clone())
            .filter(|s| s.tabs.first().is_some_and(|t| !t.panes.is_empty()));
        // Window position: the first window uses the last one; later windows are centered instead
        // of stacking.
        let place = (first_window && bench.is_none())
            .then_some(store.settings.window)
            .flatten();
        (last, session, place)
    };
    let start_default = path.is_none() && last.is_none() && session.is_none();
    let start_path = path
        .or(last)
        .or_else(|| session.as_ref().map(|s| s.tabs[0].panes[0].clone()))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("C:\\"));
    let services = services.clone();
    let window_bounds = place
        .and_then(|p| restored_bounds(p, cx))
        .unwrap_or_else(|| {
            WindowBounds::Windowed(Bounds::centered(None, default_window_size(cx), cx))
        });
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(window_bounds),
            // The title bar is drawn by us (the frame's top bar); the system keeps only the border
            // and snap layouts.
            titlebar: Some(TitlebarOptions {
                title: Some("Arke".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            window_min_size: Some(size(px(720.), px(480.))),
            // Windows 11: Mica shows through the frame (sidebar, top bar); the list is a layer on
            // top.
            window_background: if theme::mica() {
                gpui::WindowBackgroundAppearance::MicaBackdrop
            } else {
                gpui::WindowBackgroundAppearance::Opaque
            },
            ..Default::default()
        },
        move |window, cx| {
            cx.new(|cx| {
                Workspace::new(
                    services,
                    start_path,
                    start_default,
                    session,
                    bench,
                    started_at,
                    window,
                    cx,
                )
            })
        },
    );
    match opened {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::error!("cannot open window: {err:#}");
            None
        }
    }
}

#[cfg(test)]
mod arg_tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args_from(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn the_shells_drive_root_argument_gets_its_backslash_back() {
        // `"arke.exe" "C:\"` arrives as `C:"`.
        assert_eq!(parse(&["C:\""]).path, Some(PathBuf::from(r"C:\")));
        assert_eq!(
            parse(&[r#"D:\my docs""#]).path,
            Some(PathBuf::from(r"D:\my docs\"))
        );
        assert_eq!(
            parse(&[r"D:\my docs"]).path,
            Some(PathBuf::from(r"D:\my docs"))
        );
        assert_eq!(parse(&[r"C:\"]).path, Some(PathBuf::from(r"C:\")));
    }

    #[test]
    fn home_and_restore_flags_are_not_paths() {
        let args = parse(&["--home"]);
        assert!(args.page == Some(Page::Home) && args.path.is_none() && !args.restore_explorer);
        let args = parse(&["--restore-explorer"]);
        assert!(args.restore_explorer && args.path.is_none() && args.page.is_none());
    }

    #[test]
    fn bench_index_runs_without_a_window_bench() {
        let args = parse(&["--bench", "index", "--bench-out", r"C:\out.txt"]);
        assert_eq!(args.bench, Some(BenchArg::Index));
        assert_eq!(args.window_bench(), None);
        assert_eq!(args.bench_out, Some(PathBuf::from(r"C:\out.txt")));
        assert_eq!(args.path, None);
    }

    #[test]
    fn window_benches_still_parse() {
        let args = parse(&[r"C:\big", "--bench", "scroll"]);
        assert_eq!(args.window_bench(), Some(BenchMode::Scroll));
        assert_eq!(args.path, Some(PathBuf::from(r"C:\big")));
        assert_eq!(parse(&["--bench"]).window_bench(), Some(BenchMode::Startup));
    }
}
