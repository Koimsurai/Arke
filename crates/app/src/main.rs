//! Arke: a file explorer for Windows that never hangs.
//!
//! Usage:
//!   arke [path]
//!   arke --new-window [path]    a window of its own even when Settings › "Single window" is on
//!   arke <path> --bench startup|scroll|scroll-grid [--bench-out file]
//!   arke --bench index [--bench-out file]   build the search index headless, report time and
//!                                           memory (see `bench_index.rs`)
//!   arke --no-resident [path]   don't use or become the resident process
//!   arke --quit                 quit the resident process
//!   arke --api <json>           an integration API request; prints the JSON reply (see `api.rs`
//!                               and `docs/integration.md`)
//!   arke --home                 what Win+E runs while "Open folders in Arke" is on: opens a window
//!                               like starting Arke (Settings › "New windows open to")
//!   arke --search <text>        a new window searching the whole computer for <text>
//!   arke --restore-explorer     undo the folder handler registration and turn the setting off
//!   arke --uninstall [--keep-settings] [--quiet] [--dry-run]
//!                               remove Arke from this PC (see `uninstall.rs`)
//!   arke --shell-host           (internal) hidden context menu process, see `shell_host.rs`
//!   arke --thumb-host           (internal) hidden video thumbnail process, see `thumb_host.rs`
//!   arke --pick-file            (internal) one file dialog for Settings, see `win_picker.rs`
//!   arke -Embedding             (internal) COM starts Arke for the folder verbs, see
//!                               `win_folder_com.rs`
//!   arke --com-check            (internal) whether COM reaches that class
//!
//! By default the first Arke stays in the background after its last window closes, so later
//! launches only open a window without reinitializing the GPU. See `instance.rs`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Tests may unwrap and panic.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic,
        reason = "a failing test should stop right there"
    )
)]

// First, so the `tn!` macro is visible in every module below.
#[macro_use]
mod plural;

mod a11y;
mod api;
mod archive_password;
mod assets;
mod bench;
mod bench_index;
mod brand;
mod build_info;
mod caption;
mod cloud_places;
mod commands;
mod crashlog;
mod explorer;
mod folder_handler;
mod folder_views;
mod frame_probe;
mod global_search;
mod home;
mod instance;
mod keynav;
mod localized;
mod media;
mod menu_order;
mod menu_pin_looks;
mod motion;
mod net_dialog;
mod op_center;
mod op_queue;
mod palette;
mod place_names;
mod popup;
mod prefs;
mod press_guard;
mod properties;
mod quick_access;
mod quicklook;
mod recycle_bin;
mod redirection_guard;
mod remote_shell;
mod resident;
mod saved_workspaces;
mod scrollbar;
mod search_history;
mod session;
mod settings;
mod shelf;
mod shell;
mod shell_host;
mod slider;
mod spring;
mod teracopy;
mod text_input;
mod theme;
mod thumb_host;
mod tray;
mod uninstall;
mod update;
mod update_quiet;
mod update_route;
mod update_sig;
mod whats_new;
mod win;
mod win_bin_evict;
mod win_bin_usage;
mod win_cloud;
mod win_devnotify;
mod win_dragimage;
mod win_e_hook;
mod win_eject;
mod win_fast_copy;
mod win_fast_recycle;
mod win_fileops;
mod win_folder_com;
mod win_folder_visit;
mod win_http;
mod win_icons;
mod win_inuse;
mod win_jumplist;
mod win_menu;
mod win_menu_parts;
mod win_net;
mod win_openinfolder;
mod win_packaged;
mod win_pdf;
mod win_picker;
mod win_popup;
mod win_power;
mod win_purge;
mod win_recycle;
mod win_registry;
mod win_takeover;
mod win_viewsite;
mod win_virtual_drop;
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
use crate::instance::{NEW_WINDOW_ARG, OpenRequest, Page, Request, SendOutcome};
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

#[allow(
    clippy::struct_excessive_bools,
    reason = "One bool per command line flag"
)]
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
    /// A window of its own, not a tab in the window used last (`--new-window`)
    new_window: bool,
    /// Search the whole computer for this text in the window that opens (`--search <text>`)
    search: Option<String>,
    /// Started by COM for the folder verbs (`-Embedding`, `win_folder_com.rs`): like
    /// `--background`, and when another Arke is resident, pass the calls on to it
    embedding: bool,
}

fn parse_args() -> Args {
    parse_args_from(std::env::args_os().skip(1))
}

/// The command line as text, for reading flags (`--indexd`, `--uninstall`, `--api`, the update
/// hand-off). `std::env::args` panics on an argument that isn't valid Unicode, such as a folder
/// whose name has a lone surrogate; here such an argument keeps U+FFFD in its place. The folder
/// argument itself is read as an `OsString` ([`parse_args`]).
fn text_args() -> Vec<String> {
    text_args_from(std::env::args_os())
}

fn text_args_from(it: impl Iterator<Item = std::ffi::OsString>) -> Vec<String> {
    it.map(|arg| arg.to_string_lossy().into_owned()).collect()
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
        new_window: false,
        search: None,
        embedding: false,
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
            // Win+E's registration (folder_handler.rs). It used to open Home; now Win+E starts
            // like Arke itself, last tabs included (#73). The name stays: it is in the registry.
            Some("--home") => {}
            Some("--restore-explorer") => args.restore_explorer = true,
            Some(NEW_WINDOW_ARG) => args.new_window = true,
            // Text to search for; a missing or empty one is ignored.
            Some("--search") => {
                args.search = it
                    .next()
                    .and_then(|text| text.into_string().ok())
                    .map(|text| text.trim().to_owned())
                    .filter(|text| !text.is_empty());
            }
            // Started again without RedirectionGuard (`redirection_guard.rs`): nothing to do.
            Some(redirection_guard::RELAUNCHED_ARG) => {}
            // Just updated: the old process's pid follows (already waited for at the start of
            // app_main).
            Some(update::AFTER_UPDATE_ARG) => {
                let _ = it.next();
                update::mark_just_updated();
            }
            Some(flag) if win_folder_com::is_embedding_arg(flag) => args.embedding = true,
            // An empty argument (`arke.exe ""`, a shortcut with an empty %1) names no folder.
            _ if arg.is_empty() => {}
            _ => args.path = Some(path_arg(&arg)),
        }
    }
    // Updated in the background with no window open: the next window opens where the settings
    // say, as after any background start; there were no tabs to bring back.
    if args.background {
        update::start_handled();
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
    let args = text_args();
    if args.get(1).map(String::as_str) == Some(INDEXD_ARG) {
        return arke_indexd::cli::run(args[2..].to_vec(), Some(INDEXD_ARG));
    }
    // Before app_main: no log file (it would hold %LOCALAPPDATA%\Arke open), residency or window.
    if args.get(1).map(String::as_str) == Some(uninstall::ARG) {
        rust_i18n::set_locale(&win::ui_locale());
        return uninstall::run_cli(&args[2..]);
    }
    // `arke.exe --api <json>`: asks the running Arke, prints its reply and exits (`api.rs`).
    // `--api -` reads the request from stdin: Windows PowerShell 5.1 drops the quotes inside an
    // argument it passes to a program, so piping the JSON is the way that works everywhere.
    if args.get(1).map(String::as_str) == Some(api::ARG) {
        let json = match args.get(2).map(String::as_str) {
            Some("-") => {
                let mut bytes = Vec::new();
                if let Err(err) = std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes) {
                    log::warn!("reading the API request from stdin failed: {err}");
                }
                api::stdin_text(&bytes)
            }
            other => {
                // SAFETY: GetCommandLineW returns this process's command line, valid for its life.
                let raw =
                    unsafe { windows::Win32::System::Environment::GetCommandLineW().to_string() }
                        .unwrap_or_default();
                api::command_line_request(other.unwrap_or_default(), &raw)
            }
        };
        return run_api_cli(json.trim());
    }
    // A Browse button's file dialog, shown in its own process (see `win_picker.rs`).
    if args.get(1).map(String::as_str) == Some(win_picker::PICK_ARG) {
        win_picker::run();
        return std::process::ExitCode::SUCCESS;
    }
    // Whether the shell would reach Arke's folder-open COM class (`win_folder_com.rs`).
    if args.get(1).map(String::as_str) == Some(win_folder_com::CHECK_ARG) {
        return win_folder_com::check_cli();
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
            // Home, This PC and Quick access are where File Explorer starts from Start or the
            // taskbar: File Explorer started on purpose stays File Explorer unless Win+E is meant to
            // open Arke too.
            arke_core::takeover::Route::Arke(Target::Home) if !settings::home_opens_arke() => {
                win::open_in_explorer(target);
                return std::process::ExitCode::SUCCESS;
            }
            // Win+E, Start or the taskbar meant "open the file explorer": start like Arke itself
            // does, where Settings › "New windows open to" says (last tabs included) (#73).
            arke_core::takeover::Route::Arke(Target::Home) => {
                args.path = None;
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

#[allow(
    clippy::too_many_lines,
    reason = "Startup order (args, handoff to the resident process, window, settings-driven pieces) reads best top to bottom in one place"
)]
fn app_main(mut args: Args) {
    if run_shell_host() || run_thumb_host() {
        return;
    }
    // Launched by the old version (auto-update): wait for it to exit before taking over residency.
    update::wait_for_previous(&text_args());
    let started_at = Instant::now();
    // A launch in the background opens no window; any it opens later is no cold start.
    if !args.background {
        explorer::mark_process_start(started_at);
    }
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

    // `arke --quit` (the installer before it replaces the exe): returns once the resident Arke is
    // gone, so its files are free.
    if args.quit {
        // Only this copy: the installer and uninstaller of one copy must not close another.
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        match instance::quit_resident_of(&exe, Duration::from_secs(10)) {
            instance::Quit::Gone => log::info!("--quit: resident Arke gone"),
            instance::Quit::StillRunning => log::info!("--quit: resident Arke still running"),
            instance::Quit::OtherCopy(other) => log::info!(
                "--quit: the resident Arke is another copy ({}), left running",
                other.display()
            ),
        }
        return;
    }
    // Started by Inno Setup with Windows' RedirectionGuard on (#406): start again through the
    // desktop shell without it, and exit. Here, after the one-shot commands the installer waits for
    // (`--quit` above, `--uninstall` in main), with the log open for why, and before this copy
    // opens a window or takes over residency, which the new copy does instead. Not when COM started
    // this copy (`-Embedding`): COM waits for this process to register its class, so a copy
    // started elsewhere would leave the caller waiting.
    if !args.embedding && redirection_guard::restart_unguarded() {
        crashlog::flush(Duration::from_millis(200));
        return;
    }
    let linger = shell_window(&args);
    let listener = if args.background || args.embedding {
        // Background launch: if a resident process exists we're not needed; otherwise become
        // resident without a window. Started by COM, the folders come through the class.
        match instance::listen() {
            Some(listener) => Some(listener),
            None if args.embedding => {
                win_folder_com::forward_until_idle();
                return;
            }
            None => return,
        }
    } else if args.resident || args.quit {
        let request = if args.quit {
            Request::Quit
        } else if let Some(text) = &args.search {
            Request::Search(text.clone())
        } else {
            Request::Open(OpenRequest {
                path: args.path.clone(),
                bench: args.window_bench(),
                bench_out: args.bench_out.clone(),
                launched_unix_ms,
                page: args.page,
                new_window: args.new_window,
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

    let background = args.background || args.embedding;
    let bench = args
        .window_bench()
        .map(|mode| Bench::new(mode, args.bench_out));
    let start_path = args.path;
    let start_page = args.page;
    let start_search = args.search;
    let resident = listener.is_some();
    // The folder verbs may call the resident Arke over COM (#340).
    if resident && bench.is_none() && folder_handler::shell_allowed() {
        let _ = win_folder_com::serve();
    }
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
        start_type_names(cx);
        motion::sync_app(cx);
        frame_probe::init();
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
        // Keys, with the shortcuts changed in Settings
        commands::init(&store, cx);
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
        follow_update_route(&store, cx);
        let remotes = remote_shell::start(&store, cx);
        let services = Services {
            fs: Arc::new(arke_remote::RoutingFs::new(
                Arc::new(RealFs),
                Arc::clone(&remotes),
            )),
            shell: Arc::new(remote_shell::RemoteShell::new(
                Arc::new(WinShell),
                Arc::clone(&remotes),
            )),
            remotes,
        };
        // "Open in VS Code/Zed": check in the background whether they're installed.
        if bench.is_none() {
            let shell = Arc::clone(&services.shell);
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
        // The integration API (`api.rs`): `WM_COPYDATA` to any window, and the resident pipe.
        let api_queue = start_api(cx, &services);
        if bench.is_none() {
            follow_drive_removals(cx, Arc::clone(&services.remotes));
            teracopy::detect_in_background();
            quicklook::detect_in_background();
        }
        if resident && bench.is_none() {
            install_tray(cx, &services, &store);
            follow_autostart_setting(&store, &services, cx);
            follow_takeover_setting(&store, &services, cx);
            follow_win_e_hook(&store, &services, cx);
        }
        // The shell registration follows the setting in every process that is running, resident
        // or not (a standalone Arke started through the folder verb keeps it repaired too).
        // Only the resident Arke answers COM, so only it decides whether the verbs use it.
        if bench.is_none()
            && let Some(handler) = folder_handler::Handler::real()
        {
            let handler = if resident {
                handler.with_com_check(Arc::new(win_folder_com::works))
            } else {
                handler
            };
            folder_handler::follow_setting(&store, handler, cx);
        }
        if bench.is_none() {
            follow_jump_list(&store, cx);
            follow_start_menu_setting(&store, &services, cx);
        }
        // Global search: index only when enabled; not during benchmarks.
        if bench.is_none() {
            global_search::GlobalSearch::init(
                Arc::clone(&services.fs),
                Arc::clone(&services.shell),
                global_search::Roots::LocalDrives,
                global_search::default_cache_path(),
                cx,
            );
        }
        // Auto-update: delete the old binary left from last time; check a bit after startup so it
        // doesn't compete, then every hour while Arke keeps running (it can stay in the
        // notification area for days). `auto_check` only checks when 20 hours have passed.
        // A download that is waiting (from before Windows restarted) waits again; with
        // automatic updates on it goes in when Arke quits (`update_quiet.rs`).
        if bench.is_none() {
            let (pending_tx, pending_rx) = flume::bounded(1);
            explorer::spawn_thread("update-cleanup", move || {
                let _ = pending_tx.send(update::clean_up_old());
            });
            update_quiet::mark_resident(resident, background);
            cx.on_app_quit(|cx| {
                update_quiet::on_quit(cx);
                // Arke's windows leave the shell's window list with it (#319).
                win_folder_visit::end_all();
                // The process ends right after: the last lines (why it quit) go in the file first.
                crashlog::flush(Duration::from_millis(200));
                async {}
            })
            .detach();
            if !resident {
                // gpui quits by itself when the last window closes here.
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        update_quiet::note_quit(update_quiet::QuitReason::User);
                    }
                })
                .detach();
            }
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                if let Ok(Some(file)) = pending_rx.recv_async().await {
                    cx.update(|cx| update_quiet::restore(file, cx));
                }
                loop {
                    cx.update(update::auto_check);
                    cx.background_executor()
                        .timer(Duration::from_secs(3600))
                        .await;
                }
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
        if let Some(text) = start_search.filter(|_| first_window_opened) {
            search_computer(cx, text);
        }
        if let Some(listener) = listener {
            start_resident_service(cx, listener, services, api_queue);
            quit_with_last_window_unless_resident(cx);
            resident::watch(cx);
            if background {
                resident::enter(cx);
            }
        }
        if !benchmarking {
            prewarm_shell_host();
            // What the system drive's Recycle Bin holds, for the first delete (#379).
            win_fast_recycle::warm_up();
            // How pinned Windows commands looked, for the first menu (#278).
            let pinned = settings::SettingsStore::global(cx)
                .read(cx)
                .settings
                .menu_pinned
                .clone();
            let _ = std::thread::Builder::new()
                .name("menu-icons-load".into())
                .spawn(move || menu_pin_looks::load(&pinned));
        }
        if !background {
            cx.activate(true);
        }
    });
}

/// Drives removed, locked or dismounted from outside Arke: watchers on them were already closed
/// on the notification thread (`win_devnotify.rs`); here every window moves its tabs off the drive.
/// A drive plugged in or pulled out also has every sidebar read its drives again.
fn follow_drive_removals(cx: &mut App, remotes: Arc<arke_remote::Remotes>) {
    let events = win_devnotify::start();
    // Phones already connected (#292); later ones arrive as `Phones` below.
    remote_shell::refresh_phones(Arc::clone(&remotes), cx);
    cx.spawn(async move |cx| {
        while let Ok(event) = events.recv_async().await {
            match event {
                win_devnotify::DriveEvent::Leaving(drive) => {
                    cx.update(|cx| workspace::leave_drive_everywhere(&drive, cx));
                }
                win_devnotify::DriveEvent::Changed => {
                    cx.update(workspace::reload_places_everywhere);
                }
                win_devnotify::DriveEvent::Phones => {
                    let remotes = Arc::clone(&remotes);
                    cx.update(|cx| remote_shell::refresh_phones(remotes, cx));
                }
            }
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
    let shell = Arc::clone(&services.shell);
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
        let shell = Arc::clone(&shell);
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
    // The installer made its own shortcut and removes it with Arke; the setting is hidden then.
    if update::installed_by_setup() {
        return;
    }
    let shell = Arc::clone(&services.shell);
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
        let shell = Arc::clone(&shell);
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

/// "Win+E opens Arke too" on PCs where Windows sends Win+E straight to File Explorer instead of
/// asking the registration (`win_e_hook.rs`, #34): while the setting is on and Arke stays in the
/// background, a keyboard hook takes Win+E there. Everywhere else the registration does the job and
/// no hook runs. Windows can switch that path on or off when Explorer restarts, so it is looked at
/// again every half minute (one registry read, off the UI thread).
fn follow_win_e_hook(
    store: &gpui::Entity<settings::SettingsStore>,
    services: &Services,
    cx: &mut App,
) {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    // `ARKE_WIN_E_HOOK` acts as if Windows sent Win+E straight to File Explorer, so the hook can be
    // checked end to end on a PC without that (live checks also set `ARKE_NO_FOLDER_HANDLER`).
    let forced = std::env::var_os("ARKE_WIN_E_HOOK").is_some();
    if !forced && !folder_handler::shell_allowed() {
        return;
    }
    let (tx, rx) = flume::bounded::<()>(4);
    let services = services.clone();
    cx.spawn(async move |cx| {
        while rx.recv_async().await.is_ok() {
            // Like starting Arke: where Settings › "New windows open to" says, last tabs
            // included (#73); in single window mode, a tab there.
            cx.update(|cx| open_fresh(cx, &services, Instant::now()));
        }
    })
    .detach();
    let hook: Rc<RefCell<Option<win_e_hook::Hook>>> = Rc::default();
    let direct_launch = Rc::new(Cell::new(false));
    let apply = {
        let direct_launch = Rc::clone(&direct_launch);
        move |store: &settings::SettingsStore| {
            // Before settings load the values are defaults.
            if !store.loaded {
                return;
            }
            let s = &store.settings;
            let wanted = s.open_folders_in_arke
                && s.win_e_opens_arke
                && s.keep_running
                && direct_launch.get();
            let mut hook = hook.borrow_mut();
            if wanted && hook.is_none() {
                *hook = win_e_hook::Hook::start(tx.clone());
            } else if !wanted && hook.is_some() {
                *hook = None;
            }
        }
    };
    let apply = Rc::new(apply);
    {
        let apply = Rc::clone(&apply);
        cx.observe(store, move |store, cx| apply(store.read(cx)))
            .detach();
    }
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        loop {
            let active = cx
                .background_executor()
                .spawn(async move { forced || win_e_hook::direct_launch_active() })
                .await;
            if direct_launch.replace(active) != active {
                log::info!(
                    "Win+E: Windows direct launch {}",
                    if active { "on" } else { "off" }
                );
            }
            let Some(store) = store.upgrade() else {
                return;
            };
            cx.update(|cx| apply(store.read(cx)));
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
        }
    })
    .detach();
}

/// Opens a window showing what an Explorer window showed. Answers whether Arke has a window.
///
/// The early path (#285) ends File Explorer's launcher before it opens a window, and the tab it
/// opens here is marked (`Workspace::mark_early`). If that launcher had handed the folder over
/// already, the window comes anyway and arrives here from the watcher like any other: while the
/// marked tab still shows that folder, it comes forward with the window's selection instead of a
/// second tab or window. The same happens for any other window on it then; nothing is closed
/// without Arke showing it.
fn open_taken_over(cx: &mut App, services: &Services, request: &win_takeover::OpenRequest) -> bool {
    let explorer_thread = Some(request.explorer_thread);
    if !request.early && merge_into_early_tab(cx, &request.target, &request.select, explorer_thread)
    {
        return true;
    }
    let Some(window) = open_target_in(
        cx,
        services,
        request.target.clone(),
        &request.select,
        explorer_thread,
        None,
    ) else {
        return false;
    };
    if request.early {
        let _ = window.update(cx, |workspace, _, cx| {
            workspace.mark_early(&request.target, cx);
        });
    }
    true
}

/// A File Explorer window on `target` while a tab the early path opened still shows it (see
/// [`open_taken_over`]): that tab comes forward, in whichever window it is, with `select` selected,
/// and its mark is used up. With several, the newest mark wins: two quick launches of the same
/// folder each get their own tab, and the window the second couldn't keep away goes to the
/// second's. False when there is no such tab (closed, gone elsewhere, or used already).
fn merge_into_early_tab(
    cx: &mut App,
    target: &Target,
    select: &[String],
    explorer_thread: Option<u32>,
) -> bool {
    let select: Vec<Arc<str>> = select.iter().map(|n| Arc::from(n.as_str())).collect();
    let newest = {
        let app: &App = cx;
        app.windows()
            .into_iter()
            .filter_map(|w| w.downcast::<Workspace>())
            .filter_map(|w| Some((w, w.read(app).ok()?.early_mark(target, app)?)))
            .max_by_key(|&(_, mark)| mark)
    };
    let Some((handle, _)) = newest else {
        return false;
    };
    let merged = handle.update(cx, |workspace, window, cx| {
        let merged = workspace.merge_early(target, select, window, cx);
        if merged {
            bring_to_front(window, explorer_thread);
        }
        merged
    });
    if !matches!(merged, Ok(true)) {
        return false;
    }
    log::info!("takeover: shown in the tab opened early for {target:?}");
    cx.activate(true);
    true
}

/// Shows a folder, Home or the Recycle Bin (with `select` selected) and brings it to the front: in
/// single window mode as a tab in the window used last (`tab_host`), otherwise in a new window.
/// `explorer_thread` is the thread of the Explorer window being replaced, if any.
fn open_target(
    cx: &mut App,
    services: &Services,
    target: Target,
    select: &[String],
    explorer_thread: Option<u32>,
) -> bool {
    open_target_timed(cx, services, target, select, explorer_thread, None)
}

/// [`open_target`]; a window it opens for a request that arrived at `received` is timed from then
/// in the log.
fn open_target_timed(
    cx: &mut App,
    services: &Services,
    target: Target,
    select: &[String],
    explorer_thread: Option<u32>,
    received: Option<Instant>,
) -> bool {
    open_target_in(cx, services, target, select, explorer_thread, received).is_some()
}

/// [`open_target_timed`], giving the window `target` now shows in (its active tab).
fn open_target_in(
    cx: &mut App,
    services: &Services,
    target: Target,
    select: &[String],
    explorer_thread: Option<u32>,
    received: Option<Instant>,
) -> Option<gpui::WindowHandle<Workspace>> {
    let select: Vec<Arc<str>> = select.iter().map(|n| Arc::from(n.as_str())).collect();
    if let Some(host) = tab_host(cx)
        && show_in_tab(cx, host, target.clone(), select.clone(), explorer_thread)
    {
        return Some(host);
    }
    // Win+E (File Explorer's Home) with no window open: Arke starting, as "New windows open to"
    // says (the last session, say).
    let asked =
        (target != Target::Home || last_window(cx).is_some()).then_some(Asked { target, select });
    let window = open_explorer_window(cx, services, asked, None, Instant::now(), received)?;
    let shown = window.update(cx, |_, window, _| {
        bring_to_front(window, explorer_thread);
    });
    if shown.is_ok() {
        cx.activate(true);
    }
    shown.is_ok().then_some(window)
}

/// The window a folder from another program, Win+E or Arke started again open in as a tab: the
/// one used last, else the newest. `None` (open a window) when no window is open or single window
/// mode is off.
fn tab_host(cx: &mut App) -> Option<gpui::WindowHandle<Workspace>> {
    if !settings::SettingsStore::global(cx)
        .read(cx)
        .settings
        .single_window
    {
        return None;
    }
    last_window(cx)
}

/// Shows `target` as a tab in `host` and brings it to the front; `false` if the window is gone.
fn show_in_tab(
    cx: &mut App,
    host: gpui::WindowHandle<Workspace>,
    target: Target,
    select: Vec<Arc<str>>,
    explorer_thread: Option<u32>,
) -> bool {
    let shown = host.update(cx, |workspace, window, cx| {
        workspace.open_external(target, select, window, cx);
        bring_to_front(window, explorer_thread);
    });
    if shown.is_ok() {
        cx.activate(true);
    }
    shown.is_ok()
}

/// Arke asked to open with no folder (Win+E, Arke started again): in single window mode a tab in
/// the window used last, on what "New windows open to" says ([`fresh_tab`]); otherwise a window
/// started like Arke itself.
fn open_fresh(cx: &mut App, services: &Services, received: Instant) -> bool {
    if let Some(host) = tab_host(cx) {
        let target = {
            let s = &settings::SettingsStore::global(cx).read(cx).settings;
            fresh_tab(s.start_location, s.recent.first().cloned(), home_folder())
        };
        if show_in_tab(cx, host, target, Vec::new(), None) {
            return true;
        }
    }
    open_in_front(cx, services, None, None, received, received)
}

/// What a tab opened with no folder shows, after "New windows open to": the last tabs are open
/// already, so that one shows Home, as a second window does (#178).
fn fresh_tab(start: settings::StartLocation, last: Option<PathBuf>, home: PathBuf) -> Target {
    use settings::StartLocation;
    match start {
        StartLocation::HomePage | StartLocation::LastSession => Target::Home,
        StartLocation::Home => Target::Folder(home),
        StartLocation::LastFolder => Target::Folder(last.unwrap_or(home)),
    }
}

/// The user profile folder (string handling only).
fn home_folder() -> PathBuf {
    std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("C:\\"), PathBuf::from)
}

/// A request from another `arke.exe` (or the shell through it) to open something.
fn handle_open(cx: &mut App, services: &Services, open: OpenRequest, received: Instant) -> bool {
    let started_at = instant_from_unix_ms(open.launched_unix_ms);
    let bench = open.bench.map(|mode| Bench::new(mode, open.bench_out));
    // The sender was just started by the user or the shell and passed on its foreground right
    // (instance.rs), which `bring_to_front` makes use of.
    if let Some(page) = open.page {
        open_target(cx, services, page.target(), &[], None)
    } else if bench.is_some() || open.new_window {
        open_in_front(cx, services, open.path, bench, started_at, received)
    } else if let Some(path) = open.path {
        // A folder handed over by the shell or another program (#11).
        open_target_timed(
            cx,
            services,
            Target::Folder(path),
            &[],
            None,
            Some(received),
        )
    } else {
        open_fresh(cx, services, received)
    }
}

/// `arke.exe --search <text>` reaching the resident Arke: a window of its own that searches the
/// whole computer, brought to the front.
fn handle_search(cx: &mut App, services: &Services, text: String, received: Instant) -> bool {
    let Some(handle) =
        open_explorer_window(cx, services, None, None, Instant::now(), Some(received))
    else {
        return false;
    };
    let _ = handle.update(cx, |workspace, window, cx| {
        bring_to_front(window, None);
        workspace.search_computer(text, window, cx);
    });
    cx.activate(true);
    true
}

/// Starts a whole computer search for `text` in the window used last.
fn search_computer(cx: &mut App, text: String) {
    if let Some(host) = last_window(cx) {
        let _ = host.update(cx, |workspace, window, cx| {
            workspace.search_computer(text, window, cx);
        });
    }
}

/// How long after a maximized window opened behind the others Windows may still activate it: it
/// shows such a window later (`ShowWindowAsync(SW_MAXIMIZE)`), and maximizing always activates.
const BEHIND_SETTLE: Duration = Duration::from_millis(250);

/// Opens the other windows of the last session behind `front` (the window that took the one used
/// last, or the window something was asked for in), each where it was, a turn apart: the request
/// that started the session, a second `arke.exe` waiting for its answer (`instance.rs`), is
/// answered once `front` is up. Then `front` comes forward again, ahead of any of them that
/// Windows activated on showing it.
pub(crate) fn open_session_windows(
    cx: &mut App,
    services: &Services,
    windows: Vec<settings::SessionWindow>,
    front: gpui::AnyWindowHandle,
) {
    let services = services.clone();
    cx.spawn(async move |cx| {
        let mut behind = Vec::new();
        // The one used last of these opens last, so after `front` it is the next in line.
        for saved in windows.into_iter().rev() {
            next_turn().await;
            let opened = cx.update(|cx| {
                let path = saved.first_folder().cloned()?;
                let restored = saved.place.and_then(|p| restored_bounds(p, cx));
                let start = WindowStart {
                    path,
                    default: false,
                    session: Some(saved.session),
                    bounds: restored.map(|r| r.bounds),
                    display: restored.and_then(|r| r.display),
                    focus: false,
                };
                open_workspace_window(cx, &services, start, None, Instant::now(), None)
            });
            behind.extend(opened.map(gpui::AnyWindowHandle::from));
        }
        cx.background_executor().timer(BEHIND_SETTLE).await;
        cx.update(|cx| {
            // Unless the user went on to another window meanwhile.
            let last = cx
                .try_global::<workspace::LastActiveWindow>()
                .and_then(|l| l.0);
            if last.is_some_and(|l| l != front && !behind.contains(&l)) {
                return;
            }
            if front
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                // First in the session even if Windows doesn't let it take the foreground.
                session::activated(front, cx);
            }
        });
    })
    .detach();
}

/// Lets the UI thread get to other work (an answer to send, a frame to draw) before going on.
async fn next_turn() {
    let mut yielded = false;
    std::future::poll_fn(|task| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            task.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

/// The Arke window used last, else the newest; `None` when no window is open.
fn last_window(cx: &App) -> Option<gpui::WindowHandle<Workspace>> {
    let windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .collect();
    let last = cx
        .try_global::<workspace::LastActiveWindow>()
        .and_then(|l| l.0)
        .and_then(|w| w.downcast::<Workspace>());
    pick_tab_host(&windows, last)
}

/// `last` if it is still open, otherwise the newest window.
fn pick_tab_host<H: Copy + PartialEq>(windows: &[H], last: Option<H>) -> Option<H> {
    last.filter(|l| windows.contains(l))
        .or_else(|| windows.last().copied())
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
    // `--new-window` is Arke's own "Open in new window": no caller selects anything.
    if args.background
        || args.quit
        || args.new_window
        || args.page.is_some()
        || args.bench.is_some()
        || std::env::var_os("ARKE_NO_SHELL_WINDOW").is_some()
    {
        return None;
    }
    // What the shell or the calling program passed, to tell "Show in folder" callers apart.
    log::info!(
        "opened with {:?}",
        std::env::args_os().skip(1).collect::<Vec<_>>()
    );
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
/// Updates go through the download route in Settings (`update_route.rs`).
fn follow_update_route(store: &gpui::Entity<settings::SettingsStore>, cx: &mut App) {
    let apply = |store: &settings::SettingsStore| {
        if store.loaded {
            update_route::set(&store.settings.update_route);
        }
    };
    apply(store.read(cx));
    cx.observe(store, move |store, cx| apply(store.read(cx)))
        .detach();
}

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

/// Starts looking up Windows' names for file types the built-in table doesn't have; windows
/// repaint (and re-sort by type) when names arrive.
fn start_type_names(cx: &mut App) {
    let (tx, rx) = flume::bounded::<()>(1);
    let notify = tx.clone();
    arke_core::format::start_type_names(win_dragimage::registered_type_name, move || {
        let _ = tx.try_send(());
    });
    media::start_icon_probe(win_dragimage::registered_default_icon, move || {
        let _ = notify.try_send(());
    });
    cx.spawn(async move |cx| {
        while rx.recv_async().await.is_ok() {
            cx.update(App::refresh_windows);
        }
    })
    .detach();
}

/// Switches strings and the CJK font fallback order to `locale`, repainting if either changed.
fn apply_language(locale: &str, cx: &mut App) {
    // Fonts follow the UI language so Simplified text gets Simplified glyphs on a Traditional
    // system and vice versa (see `theme::cjk_fallbacks`).
    let mut changed = theme::set_ui_language(locale, win::system_locale());
    if *rust_i18n::locale() != *locale {
        rust_i18n::set_locale(locale);
        arke_core::format::language_changed();
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
            update_quiet::quit(update_quiet::QuitReason::User, cx);
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

/// Where a saved window opens again.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Restored {
    bounds: WindowBounds,
    /// The screen it goes on; `None` for the main one. Windows checks the bounds against this
    /// screen, and would put a window meant for another one at the main screen's default spot.
    display: Option<gpui::DisplayId>,
}

/// Last window position, on the screen that shows most of it.
fn restored_bounds(place: settings::WindowPlace, cx: &App) -> Option<Restored> {
    let screens: Vec<_> = cx.displays().iter().map(|d| (d.id(), d.bounds())).collect();
    let main = cx.primary_display().map(|d| d.bounds());
    place_on_screens(place, &screens, main)
}

/// [`restored_bounds`]: the screen showing most of the window, if at least a 240×120 patch of it
/// is still on one (so it never opens off-screen after unplugging a monitor). A maximized window
/// whose screen is gone is maximized on the main screen (`main`) instead; a window that wasn't
/// gets the default spot (`None`).
fn place_on_screens(
    place: settings::WindowPlace,
    screens: &[(gpui::DisplayId, Bounds<gpui::Pixels>)],
    main: Option<Bounds<gpui::Pixels>>,
) -> Option<Restored> {
    let bounds = Bounds::new(
        gpui::point(px(place.x), px(place.y)),
        size(px(place.width.max(720.)), px(place.height.max(480.))),
    );
    let shown = |screen: &Bounds<gpui::Pixels>| {
        let overlap = screen.intersect(&bounds);
        (overlap.size.width >= px(240.) && overlap.size.height >= px(120.))
            .then(|| f32::from(overlap.size.width) * f32::from(overlap.size.height))
    };
    let screen = screens
        .iter()
        .filter_map(|(id, screen)| Some((*id, shown(screen)?)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id);
    match screen {
        Some(id) => Some(Restored {
            bounds: if place.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            },
            display: Some(id),
        }),
        None if place.maximized => main.map(|main| {
            let size = bounds.size.min(&main.size);
            let origin = gpui::point(
                main.origin.x + (main.size.width - size.width) / 2.,
                main.origin.y + (main.size.height - size.height) / 2.,
            );
            Restored {
                bounds: WindowBounds::Maximized(Bounds::new(origin, size)),
                display: None,
            }
        }),
        None => None,
    }
}

/// Settings read only after the first window opened (a slow disk): it moves to where the window
/// it takes the tabs of was.
pub(crate) fn move_to_place(window: &mut gpui::Window, place: settings::WindowPlace, cx: &App) {
    let Some(restored) = restored_bounds(place, cx) else {
        return;
    };
    let bounds = restored.bounds.get_bounds();
    // gpui can resize a window but not move it.
    if let Some(hwnd) = explorer::Explorer::owner_from(window) {
        let screen = restored
            .display
            .and_then(|id| cx.find_display(id))
            .or_else(|| cx.primary_display());
        if let Some(screen) = screen {
            win::move_window(hwnd, u64::from(screen.id()), screen.bounds(), bounds);
        }
    }
    if matches!(restored.bounds, WindowBounds::Maximized(_)) && !window.is_maximized() {
        window.zoom_window();
    }
}

/// `arke.exe --thumb-host`: the video thumbnail process (see `thumb_host.rs`). No window, no
/// residency or settings.
fn run_thumb_host() -> bool {
    if std::env::args_os()
        .nth(1)
        .is_none_or(|a| a != thumb_host::HOST_ARG)
    {
        return false;
    }
    thumb_host::run();
    true
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

fn start_resident_service(
    cx: &mut App,
    listener: instance::Listener,
    services: Services,
    api_queue: flume::Sender<api::Action>,
) {
    let (tx, rx) = flume::unbounded::<(Request, Instant, flume::Sender<bool>)>();
    instance::serve(listener, move |request| {
        // `arke.exe --api`: answered here, from the snapshot, without waiting for the UI thread.
        if let Request::Api(json) = &request {
            return instance::Reply::Data(api::serve(
                json.as_bytes(),
                api::Target::LastUsed,
                &api_queue,
            ));
        }
        let received = Instant::now();
        let (ack_tx, ack_rx) = flume::bounded(1);
        if tx.send((request, received, ack_tx)).is_err() {
            return instance::Reply::Status(false);
        }
        // If the UI thread doesn't handle it in time, reply failure so the sender opens its own
        // window.
        instance::Reply::Status(
            ack_rx
                .recv_timeout(instance::SERVER_ACK_TIMEOUT)
                .unwrap_or(false),
        )
    });
    cx.spawn(async move |cx| {
        while let Ok((request, received, ack)) = rx.recv_async().await {
            let handled = cx.update(|cx| match request {
                Request::Quit => {
                    update_quiet::quit(update_quiet::QuitReason::Other, cx);
                    true
                }
                Request::Select { folder, items } => select_items(cx, &services, &folder, &items),
                Request::Open(open) => handle_open(cx, &services, open, received),
                Request::Search(text) => handle_search(cx, &services, text, received),
                // Answered on the pipe thread.
                Request::Api(_) => false,
            });
            let _ = ack.send(handled);
        }
    })
    .detach();
}

/// The integration API (`api.rs`): requests sent to any window as `WM_COPYDATA` are answered in its
/// window procedure, and those from `arke.exe --api` on the resident pipe thread; both queue their
/// `open` and `run` here, for the UI thread. Returns the queue for the pipe.
fn start_api(cx: &mut App, services: &Services) -> flume::Sender<api::Action> {
    let (tx, rx) = flume::unbounded::<api::Action>();
    let queue = tx.clone();
    gpui_windows::set_copydata_handler(move |hwnd, request| {
        Some(api::serve(request, api::Target::Window(hwnd), &queue))
    });
    let services = services.clone();
    cx.spawn(async move |cx| {
        while let Ok(action) = rx.recv_async().await {
            cx.update(|cx| apply_api(cx, &services, action));
        }
    })
    .detach();
    cx.on_window_closed(|_, id| api::forget(id.as_u64()))
        .detach();
    tx
}

/// The window an API `open` or `run` is for: the one it was sent to when that is a workspace
/// window, else the one used last.
fn api_window(cx: &App, target: api::Target) -> Option<gpui::WindowHandle<Workspace>> {
    let sent_to = match target {
        api::Target::Window(hwnd) => api::window_of(hwnd).and_then(|id| {
            cx.windows()
                .into_iter()
                .find(|w| w.window_id().as_u64() == id)
                .and_then(|w| w.downcast::<Workspace>())
        }),
        api::Target::LastUsed => None,
    };
    sent_to.or_else(|| last_window(cx))
}

/// Does what an API request queued (`api.rs`).
fn apply_api(cx: &mut App, services: &Services, action: api::Action) {
    match action {
        api::Action::Open {
            target,
            path,
            new_tab,
            select,
        } => {
            if let Some(host) = api_window(cx, target) {
                let names: Vec<Arc<str>> = select.iter().map(|n| Arc::from(n.as_str())).collect();
                let shown = host.update(cx, |workspace, window, cx| {
                    workspace.api_open(path.clone(), new_tab, names, window, cx);
                    bring_to_front(window, None);
                });
                if shown.is_ok() {
                    cx.activate(true);
                    return;
                }
            }
            // No window open (only the notification area icon): one opens on it.
            open_target(cx, services, Target::Folder(path), &select, None);
        }
        api::Action::Run { target, id } => {
            let Some(command) = commands::find(id) else {
                return;
            };
            let host = api_window(cx, target).or_else(|| {
                open_fresh(cx, services, Instant::now())
                    .then(|| last_window(cx))
                    .flatten()
            });
            if let Some(host) = host {
                let front = commands::moves_focus(command.id);
                let _ = host.update(cx, |workspace, window, cx| {
                    if front {
                        bring_to_front(window, None);
                    }
                    workspace.api_run(command, window, cx);
                });
                if front {
                    cx.activate(true);
                }
            }
        }
    }
}

/// `arke.exe --api <json>`: asks the resident Arke and prints its reply. With no Arke running,
/// `state` and `commands` are answered here; `open` and `run` start one in the background first.
fn run_api_cli(json: &str) -> std::process::ExitCode {
    // A GUI program has no console of its own: write to the one it was started from.
    bench_index::console::attach();
    rust_i18n::set_locale(&win::ui_locale());
    api::print_reply(&api_cli_reply(json))
}

/// How long `arke.exe --api` waits for an Arke it started to answer.
const API_START_TIMEOUT: Duration = Duration::from_secs(20);

fn api_cli_reply(json: &str) -> Vec<u8> {
    let request = match api::parse(json.as_bytes()) {
        Ok(request) => request,
        Err(error) => return api::error_reply(&error),
    };
    let message = api::encode_request(&api::absolute(request));
    match instance::ask(&message, instance::CLIENT_TIMEOUT) {
        instance::Asked::Replied(reply) => return reply,
        instance::Asked::Failed => return api_unavailable(),
        instance::Asked::NoServer => {}
    }
    // Nothing is running: what needs no window is answered here, as are requests that fail.
    let local = api::answer(message.as_bytes(), api::Target::LastUsed, &api::NotRunning);
    if local.action.is_none() {
        return local.reply;
    }
    if !start_background_arke() {
        return api_unavailable();
    }
    let deadline = Instant::now() + API_START_TIMEOUT;
    while Instant::now() < deadline {
        match instance::ask(&message, Duration::from_secs(10)) {
            instance::Asked::Replied(reply) => return reply,
            instance::Asked::Failed => return api_unavailable(),
            instance::Asked::NoServer => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    api_unavailable()
}

fn api_unavailable() -> Vec<u8> {
    api::error_reply(&api::ApiError::new(
        api::ErrorCode::Unavailable,
        "Arke did not answer in time",
    ))
}

/// Starts Arke in the background (`--background`, no window) for an API request that needs one.
/// It gets no console handles, so a script reading this command's output isn't held open by it.
fn start_background_arke() -> bool {
    use std::os::windows::process::CommandExt as _;
    /// No console for the new process.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .arg("--background")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .is_ok()
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
                    // Brings back the window used last (restored if minimized); a new window only
                    // when none is open, so repeated or double clicks don't pile up windows.
                    if let Some(window) = last_window(cx) {
                        let _ = window.update(cx, |_, window, _| bring_to_front(window, None));
                        cx.activate(true);
                    } else if open_explorer(cx, &services, None, None, Instant::now()) {
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
                    update_quiet::quit(update_quiet::QuitReason::User, cx);
                }
                tray::TrayCommand::Update => {
                    // The window used last (or a new one) shows the update's notes, with Download.
                    let window = last_window(cx).or_else(|| {
                        open_explorer(cx, &services, None, None, Instant::now())
                            .then(|| last_window(cx))
                            .flatten()
                    });
                    if let (Some(window), update::UpdateState::Available(release)) =
                        (window, update::UpdateState::get(cx))
                    {
                        let _ = window.update(cx, |ws, window, cx| {
                            bring_to_front(window, None);
                            ws.open_whats_new(
                                workspace::whats_new_page::Mode::Available(release),
                                window,
                                cx,
                            );
                        });
                        cx.activate(true);
                    }
                }
            });
        }
    })
    .detach();
    // Quitting from elsewhere (arke --quit, last window closed) also removes the icon, so no ghost
    // icon lingers until hovered.
    cx.on_app_quit(|_| async { tray::remove() }).detach();
    // The menu offers an update that was found (and not yet installed).
    cx.observe_global::<update::UpdateState>(|cx| {
        tray::set_update(match update::UpdateState::get(cx) {
            update::UpdateState::Available(r) if !update_quiet::hidden(cx) => Some(r.version),
            _ => None,
        });
    })
    .detach();
}

fn open_explorer(
    cx: &mut App,
    services: &Services,
    path: Option<PathBuf>,
    bench: Option<Bench>,
    started_at: Instant,
) -> bool {
    let asked = path.map(Asked::folder);
    open_explorer_window(cx, services, asked, bench, started_at, None).is_some()
}

/// Opens a window (at `path`, or where the settings say) and brings it to the front. `received`:
/// when the request for it arrived, which its start is timed from in the log.
fn open_in_front(
    cx: &mut App,
    services: &Services,
    path: Option<PathBuf>,
    bench: Option<Bench>,
    started_at: Instant,
    received: Instant,
) -> bool {
    let asked = path.map(Asked::folder);
    let Some(handle) = open_explorer_window(cx, services, asked, bench, started_at, Some(received))
    else {
        return false;
    };
    let _ = handle.update(cx, |_, window, _| bring_to_front(window, None));
    cx.activate(true);
    true
}

/// What a window is opened for: a folder (from the command line, another program or a second
/// `arke.exe`), Home or the Recycle Bin, with the items to select in it.
struct Asked {
    target: Target,
    select: Vec<Arc<str>>,
}

impl Asked {
    const fn folder(path: PathBuf) -> Self {
        Self {
            target: Target::Folder(path),
            select: Vec::new(),
        }
    }
}

/// Opens a window on what was `asked` for, or where "New windows open to" says. The first window
/// with "Last tabs" brings back the last session, also when something was asked for: that opens
/// as a tab of the window used last in single window mode, else in a window of its own in front of
/// the session's.
fn open_explorer_window(
    cx: &mut App,
    services: &Services,
    asked: Option<Asked>,
    bench: Option<Bench>,
    started_at: Instant,
    received: Option<Instant>,
) -> Option<gpui::WindowHandle<Workspace>> {
    // The startup path is string handling only; existence isn't checked, since startup must not
    // touch locations that might block. No path given: if settings say "last location/last tabs",
    // open there (switching once settings finish loading).
    let first_window = cx.windows().is_empty();
    let (last, mut sessions, pinned, place, single_window, loaded) = {
        let store = settings::SettingsStore::global(cx).read(cx);
        let start = update::start_location(store.settings.start_location);
        let last =
            (asked.is_none() && store.loaded && start == settings::StartLocation::LastFolder)
                .then(|| store.settings.recent.first().cloned())
                .flatten();
        // Restore the last session only when no other window exists, otherwise each new window
        // would duplicate it. Read before any window opens and saves its own tabs over it.
        let sessions = if store.loaded
            && first_window
            && bench.is_none()
            && start == settings::StartLocation::LastSession
        {
            store.settings.last_windows()
        } else {
            Vec::new()
        };
        // Pinned tabs come back in front of a folder asked for whatever the start location is
        // (#298); with the last tabs they are part of the session.
        let pinned = (store.loaded
            && first_window
            && bench.is_none()
            && start != settings::StartLocation::LastSession)
            .then(|| store.settings.last_pinned_tabs())
            .filter(|s| !s.tabs.is_empty());
        // Window position: the first window uses the last one; later windows are centered instead
        // of stacking.
        let place = (first_window && bench.is_none())
            .then_some(store.settings.window)
            .flatten();
        if store.loaded {
            update::start_handled();
        }
        (
            last,
            sessions,
            pinned,
            place,
            store.settings.single_window,
            store.loaded,
        )
    };
    // In single window mode the session comes back as usual, and what was asked for opens as a tab
    // of its front window.
    let (asked, as_tab) = match asked {
        Some(asked) if single_window && !sessions.is_empty() => (None, Some(asked)),
        asked => (asked, None),
    };
    if let Some(asked) = asked {
        let path = match &asked.target {
            Target::Folder(path) => path.clone(),
            Target::Home | Target::RecycleBin => home_folder(),
        };
        let restored = place.and_then(|p| restored_bounds(p, cx));
        let start = WindowStart {
            path,
            default: false,
            session: None,
            bounds: restored.map(|r| r.bounds),
            display: restored.and_then(|r| r.display),
            focus: true,
        };
        let wait_for_session = first_window && !loaded && bench.is_none();
        let handle = open_workspace_window(cx, services, start, bench, started_at, received)?;
        let _ = handle.update(cx, |workspace, window, cx| {
            workspace.show_taken_over(asked.target, asked.select, window, cx);
            if let Some(pinned) = &pinned {
                workspace.restore_session_before(pinned, window, cx);
            }
            // Settings not read yet: the last session may still come back around this window.
            if wait_for_session {
                workspace.restore_session_when_read();
            }
        });
        if !sessions.is_empty() {
            // Single window mode off: the session's windows open behind this one.
            open_session_windows(cx, services, sessions, handle.into());
        }
        return Some(handle);
    }
    // This window takes the one used last; the others open behind it once it is up.
    let front = (!sessions.is_empty()).then(|| sessions.remove(0));
    let start_default = last.is_none() && front.is_none();
    let path = last
        .or_else(|| front.as_ref().and_then(|w| w.first_folder().cloned()))
        .unwrap_or_else(home_folder);
    let place = front.as_ref().and_then(|w| w.place).or(place);
    let restored = place.and_then(|p| restored_bounds(p, cx));
    let start = WindowStart {
        path,
        default: start_default,
        session: front.map(|w| w.session),
        bounds: restored.map(|r| r.bounds),
        display: restored.and_then(|r| r.display),
        focus: true,
    };
    let handle = open_workspace_window(cx, services, start, bench, started_at, received)?;
    if let Some(asked) = as_tab {
        let _ = handle.update(cx, |workspace, window, cx| {
            workspace.open_external(asked.target, asked.select, window, cx);
        });
    }
    if !sessions.is_empty() {
        open_session_windows(cx, services, sessions, handle.into());
    }
    Some(handle)
}

/// A window for a tab dragged out of another one, at `bounds` (screen coordinates), whatever
/// single window mode says: the drag asked for a window.
pub(crate) fn open_moved_tab_window(
    cx: &mut App,
    services: &Services,
    moved: workspace::MovedTab,
    bounds: Bounds<gpui::Pixels>,
) -> Option<gpui::WindowHandle<Workspace>> {
    let path = moved.first_folder()?.to_path_buf();
    let display = cx
        .displays()
        .iter()
        .find(|d| d.bounds().contains(&bounds.center()))
        .map(|d| d.id());
    let start = WindowStart {
        path,
        default: false,
        session: None,
        bounds: Some(WindowBounds::Windowed(bounds)),
        display,
        focus: true,
    };
    let handle = open_workspace_window(cx, services, start, None, Instant::now(), None)?;
    handle
        .update(cx, |ws, window, cx| ws.adopt_first(moved, window, cx))
        .ok()?;
    Some(handle)
}

/// What a new window starts on, and where.
struct WindowStart {
    path: PathBuf,
    /// No path was given: once settings load, go where "New windows open to" says.
    default: bool,
    /// Tabs to restore (the first opens on `path`).
    session: Option<settings::Session>,
    /// `None`: centered, at the default size.
    bounds: Option<WindowBounds>,
    /// The screen `bounds` are on; `None`: the main one.
    display: Option<gpui::DisplayId>,
    /// Takes the foreground; the other windows of a restored session open behind.
    focus: bool,
}

fn open_workspace_window(
    cx: &mut App,
    services: &Services,
    start: WindowStart,
    bench: Option<Bench>,
    started_at: Instant,
    received: Option<Instant>,
) -> Option<gpui::WindowHandle<Workspace>> {
    let services = services.clone();
    let window_bounds = start.bounds.unwrap_or_else(|| {
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
            focus: start.focus,
            display_id: start.display,
            window_min_size: Some(size(theme::scaled_px(720.), theme::scaled_px(480.))),
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
                let mut workspace = Workspace::new(
                    services,
                    start.path,
                    start.default,
                    start.session,
                    bench,
                    started_at,
                    window,
                    cx,
                );
                // Before the first frame, which `open_window` draws before it returns.
                if let Some(received) = received {
                    workspace.time_from_request(received);
                }
                workspace
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

    /// A folder whose name has a lone surrogate (valid on NTFS, not valid Unicode) no longer
    /// panics at startup: the flags are still read, the folder keeps its exact name.
    #[cfg(windows)]
    #[test]
    fn a_folder_name_that_isnt_unicode_doesnt_stop_startup() {
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
        let mut wide: Vec<u16> = r"C:\a".encode_utf16().collect();
        wide.push(0xD800);
        let folder = std::ffi::OsString::from_wide(&wide);
        let flags = text_args_from(["arke.exe".into(), "--api".into(), folder.clone()].into_iter());
        assert_eq!(flags[1], "--api");
        assert_eq!(flags[2], "C:\\a\u{FFFD}");
        let args = parse_args_from([folder].into_iter());
        let kept: Vec<u16> = args.path.unwrap().as_os_str().encode_wide().collect();
        assert_eq!(kept, wide, "the folder argument is read as it is");
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

    /// The copy started again without RedirectionGuard reads its arguments like the first one.
    #[test]
    fn the_relaunch_marker_is_not_a_path() {
        use crate::redirection_guard::RELAUNCHED_ARG;
        let args = parse(&[RELAUNCHED_ARG]);
        assert!(args.path.is_none() && args.page.is_none() && !args.new_window);
        let args = parse(&[RELAUNCHED_ARG, r"D:\work", "--new-window"]);
        assert_eq!(args.path, Some(PathBuf::from(r"D:\work")));
        assert!(args.new_window);
    }

    #[test]
    fn home_and_restore_flags_are_not_paths() {
        let args = parse(&["--home"]);
        assert!(args.page.is_none() && args.path.is_none() && !args.restore_explorer);
        let args = parse(&["--restore-explorer"]);
        assert!(args.restore_explorer && args.path.is_none() && args.page.is_none());
    }

    #[test]
    fn search_takes_the_text_after_it() {
        assert_eq!(
            parse(&["--search", "tax return"]).search.as_deref(),
            Some("tax return")
        );
        let args = parse(&["--search", "  notes ", r"C:\work"]);
        assert_eq!(args.search.as_deref(), Some("notes"));
        assert_eq!(args.path, Some(PathBuf::from(r"C:\work")));
        assert_eq!(parse(&["--search"]).search, None);
        assert_eq!(parse(&["--search", " "]).search, None);
        assert_eq!(parse(&[r"C:\work"]).search, None);
    }

    #[test]
    fn new_window_is_a_flag_not_a_path() {
        let args = parse(&["--new-window", r"C:\work"]);
        assert!(args.new_window);
        assert_eq!(args.path, Some(PathBuf::from(r"C:\work")));
        assert!(!parse(&[r"C:\work"]).new_window);
    }

    #[test]
    fn an_empty_argument_names_no_folder() {
        assert_eq!(parse(&[""]).path, None);
        assert_eq!(
            parse(&["", r"C:\work"]).path,
            Some(PathBuf::from(r"C:\work"))
        );
    }

    /// Win+E or Arke started again with a window open: the tab goes where "New windows open to"
    /// says, Home when that is the last tabs (they are open already).
    #[test]
    fn a_tab_with_no_folder_follows_the_startup_setting() {
        use settings::StartLocation;
        let home = || PathBuf::from(r"C:\Users\me");
        let last = Some(PathBuf::from(r"C:\last"));
        assert_eq!(
            fresh_tab(StartLocation::HomePage, last.clone(), home()),
            Target::Home
        );
        assert_eq!(
            fresh_tab(StartLocation::LastSession, last.clone(), home()),
            Target::Home
        );
        assert_eq!(
            fresh_tab(StartLocation::Home, last.clone(), home()),
            Target::Folder(home())
        );
        assert_eq!(
            fresh_tab(StartLocation::LastFolder, last, home()),
            Target::Folder(PathBuf::from(r"C:\last"))
        );
        assert_eq!(
            fresh_tab(StartLocation::LastFolder, None, home()),
            Target::Folder(home())
        );
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

    #[test]
    fn folders_from_other_apps_go_to_the_window_used_last() {
        assert_eq!(pick_tab_host(&[1, 2, 3], Some(1)), Some(1));
        // The window used last was closed, or none was active yet: the newest.
        assert_eq!(pick_tab_host(&[1, 2, 3], Some(9)), Some(3));
        assert_eq!(pick_tab_host(&[1, 2], None), Some(2));
        // No window (only the notification area icon): open one.
        assert_eq!(pick_tab_host::<u8>(&[], Some(1)), None);
    }

    fn screen(x: f32, y: f32, width: f32, height: f32) -> Bounds<gpui::Pixels> {
        Bounds::new(gpui::point(px(x), px(y)), size(px(width), px(height)))
    }

    fn place(x: f32, y: f32, maximized: bool) -> settings::WindowPlace {
        settings::WindowPlace {
            x,
            y,
            width: 1000.,
            height: 700.,
            maximized,
        }
    }

    /// A window saved on the second screen opens there: gpui is told which screen, or Windows
    /// puts it at the main screen's default spot.
    #[test]
    fn a_window_comes_back_on_the_screen_it_was_on() {
        let main = screen(0., 0., 2560., 1440.);
        let second = screen(2560., 0., 1920., 1080.);
        let (one, two) = (gpui::DisplayId::new(1), gpui::DisplayId::new(2));
        let screens = [(one, main), (two, second)];
        let on_second = place_on_screens(place(2700., 100., false), &screens, Some(main)).unwrap();
        assert_eq!(on_second.display, Some(two));
        assert_eq!(
            on_second.bounds,
            WindowBounds::Windowed(screen(2700., 100., 1000., 700.))
        );
        let maximized = place_on_screens(place(2700., 100., true), &screens, Some(main)).unwrap();
        assert_eq!(maximized.display, Some(two));
        assert!(matches!(maximized.bounds, WindowBounds::Maximized(_)));
        // Across the edge: the screen that shows more of it.
        let across = place_on_screens(place(2200., 100., false), &screens, Some(main)).unwrap();
        assert_eq!(across.display, Some(two));
        let across = place_on_screens(place(1800., 100., false), &screens, Some(main)).unwrap();
        assert_eq!(across.display, Some(one));
    }

    /// The screen a window was on is gone: a window gets the default spot, a maximized one is
    /// maximized on the main screen.
    #[test]
    fn a_window_whose_screen_is_gone_falls_back_to_the_main_one() {
        let main = screen(0., 0., 2560., 1440.);
        let screens = [(gpui::DisplayId::new(1), main)];
        assert_eq!(
            place_on_screens(place(3000., 100., false), &screens, Some(main)),
            None
        );
        let maximized = place_on_screens(place(3000., 100., true), &screens, Some(main)).unwrap();
        assert_eq!(maximized.display, None);
        assert_eq!(
            maximized.bounds,
            WindowBounds::Maximized(screen(780., 370., 1000., 700.))
        );
        // Only a sliver left on a screen is as good as gone.
        assert_eq!(
            place_on_screens(place(2500., 100., false), &screens, Some(main)),
            None
        );
        // Small saved sizes grow to the window's minimum.
        let small = settings::WindowPlace {
            width: 100.,
            height: 100.,
            ..place(10., 10., false)
        };
        assert_eq!(
            place_on_screens(small, &screens, Some(main)).map(|r| r.bounds),
            Some(WindowBounds::Windowed(screen(10., 10., 720., 480.)))
        );
    }
}
