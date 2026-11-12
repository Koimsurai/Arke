//! Global search ("this PC" scope): the index is built only when enabled in settings (off by
//! default).
//!
//! - Indexes local fixed drives (no network or removable drives), stored at
//!   `%LOCALAPPDATA%\Arke\index.bin`.
//! - The walk runs on a background thread at lowest I/O priority (`THREAD_MODE_BACKGROUND_BEGIN`).
//! - Core logic is in `arke_core::index_service`; this module handles toggling, status and
//!   dispatching queries.
//! - With the optional background service (`arke-indexd`) installed, NTFS drives update live from
//!   the USN journal and changes made while closed are caught up on next start. Install/remove goes
//!   through UAC.
//! - Network locations (SMB, mapped drives) aren't indexed by default, since walking a NAS keeps
//!   its disks and the network busy. Each one is opted in from the sidebar context menu
//!   (`Settings::search_network`) and gets its own index and cache file, so a slow or offline share
//!   never holds up local drives. Unreachable shares use the last cache (no rebuild, results stay)
//!   and are probed again when Arke regains focus. Full rewalks happen at most daily; otherwise the
//!   watcher keeps them current.

use std::path::PathBuf;
use std::sync::Arc;

use arke_core::Fs;
use arke_core::index::IndexHit;
use arke_core::index_service::{IndexOptions, IndexService, IndexStatus};
use arke_core::journal::Journal;
use gpui::{App, AppContext, Context, Entity, Global, Subscription, Task};

use crate::settings::SettingsStore;
use crate::shell::{PlaceKind, Shell};

/// Visited items at most added to one query's results.
const FRECENT_MAX: usize = 32;

/// Ranking signals: the visit history plus the profile and system folders (from the environment,
/// read once).
fn rank_signals(
    history: Option<Arc<arke_core::frecency::FrecencySnapshot>>,
) -> arke_core::rank::RankSignals {
    static FOLDERS: std::sync::OnceLock<(Option<PathBuf>, Vec<PathBuf>)> =
        std::sync::OnceLock::new();
    let (home, system) = FOLDERS.get_or_init(|| {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let home = var("USERPROFILE");
        let mut system: Vec<PathBuf> = [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramData",
        ]
        .into_iter()
        .filter_map(var)
        .collect();
        if let Some(home) = &home {
            system.push(home.join("AppData"));
        }
        (home, system)
    });
    arke_core::rank::RankSignals::new(history, home.as_deref(), system)
}

/// Which roots to index.
#[derive(Clone)]
pub(crate) enum Roots {
    /// Local fixed drives (discovered in the background)
    LocalDrives,
    /// Explicit roots (tests)
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Vec<PathBuf>),
}

/// Index of one network location.
pub(crate) struct NetIndex {
    pub(crate) root: PathBuf,
    service: Option<Arc<IndexService>>,
    pub(crate) status: IndexStatus,
    /// Unreachable at last probe (using the cache, not updating)
    pub(crate) offline: bool,
    /// Opened during this session (the NAS is awake); only then is it watched and updated,
    /// otherwise the cache is used
    pub(crate) awake: bool,
    worker: Option<Task<()>>,
}

pub(crate) struct GlobalSearch {
    /// Network locations added by the user (same order as settings)
    pub(crate) net: Vec<NetIndex>,
    /// Skip build and package cache folders (from settings)
    skip_build_dirs: bool,
    /// Local drives and folders excluded from the index (from settings)
    skipped_drives: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
    fs: Arc<dyn Fs>,
    shell: Arc<dyn Shell>,
    roots: Roots,
    cache: Option<PathBuf>,
    enabled: bool,
    service: Option<Arc<IndexService>>,
    pub(crate) status: IndexStatus,
    /// Whether the background service is installed (`None`: not asked yet)
    pub(crate) service_installed: Option<bool>,
    /// Install or remove in progress (waiting for UAC consent)
    pub(crate) service_busy: bool,
    _settings: Subscription,
    worker: Option<Task<()>>,
}

struct GlobalSearchHandle(Entity<GlobalSearch>);

impl Global for GlobalSearchHandle {}

impl GlobalSearch {
    /// Create (or reuse the existing one). On/off follows the `global_search` setting.
    pub(crate) fn init(
        fs: Arc<dyn Fs>,
        shell: Arc<dyn Shell>,
        roots: Roots,
        cache: Option<PathBuf>,
        cx: &mut App,
    ) -> Entity<Self> {
        if let Some(handle) = cx.try_global::<GlobalSearchHandle>() {
            return handle.0.clone();
        }
        let settings = SettingsStore::global(cx);
        let entity = cx.new(|cx| {
            let sub = cx.observe(&settings, |this: &mut Self, _, cx| this.sync(cx));
            let mut this = Self {
                net: Vec::new(),
                skip_build_dirs: SettingsStore::global(cx)
                    .read(cx)
                    .settings
                    .search_skip_build_dirs,
                skipped_drives: SettingsStore::global(cx)
                    .read(cx)
                    .settings
                    .search_skipped_drives
                    .clone(),
                excluded: SettingsStore::global(cx)
                    .read(cx)
                    .settings
                    .search_excluded
                    .clone(),
                fs,
                shell,
                roots,
                cache,
                enabled: false,
                service: None,
                status: IndexStatus::default(),
                service_installed: None,
                service_busy: false,
                _settings: sub,
                worker: None,
            };
            this.sync(cx);
            this
        });
        cx.set_global(GlobalSearchHandle(entity.clone()));
        entity
    }

    pub(crate) fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSearchHandle>().map(|h| h.0.clone())
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turn on global search (writes the setting; `sync` then starts it).
    pub(crate) fn enable(cx: &mut App) {
        SettingsStore::global(cx).update(cx, |s, cx| s.update(|st| st.global_search = true, cx));
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let settings = &SettingsStore::global(cx).read(cx).settings;
        let want = settings.global_search;
        let skip = settings.search_skip_build_dirs;
        let scope = (
            settings.search_skipped_drives.clone(),
            settings.search_excluded.clone(),
        );
        let want_net = if want {
            let drives = if matches!(self.roots, Roots::LocalDrives) {
                crate::win::network_drive_roots()
            } else {
                Vec::new()
            };
            settings.searched_network(&drives)
        } else {
            Vec::new()
        };
        // Build-folder skipping or excluded drives/folders changed: start over, the index scope is
        // different.
        if skip != self.skip_build_dirs
            || scope.0 != self.skipped_drives
            || scope.1 != self.excluded
        {
            self.skip_build_dirs = skip;
            (self.skipped_drives, self.excluded) = scope;
            self.net.clear();
            if self.enabled {
                self.service = None;
                self.worker = None;
                self.status = IndexStatus::default();
                self.start(cx);
            }
        }
        self.sync_net(&want_net, cx);
        if want == self.enabled {
            return;
        }
        self.enabled = want;
        // Off: dropping the service stops the walk and the watcher.
        self.service = None;
        self.worker = None;
        self.status = IndexStatus::default();
        if want {
            self.start(cx);
        }
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let shell = self.shell.clone();
        let roots = self.roots.clone();
        let real = matches!(roots, Roots::LocalDrives);
        let skip_build_dirs = self.skip_build_dirs;
        let skipped_drives = self.skipped_drives.clone();
        let excluded = self.excluded.clone();
        let cache = self.cache.clone();
        let (changed_tx, changed_rx) = flume::unbounded::<()>();
        let (service_tx, service_rx) = flume::bounded::<(Arc<IndexService>, bool)>(1);
        crate::explorer::spawn_thread("index-roots", move || {
            // The background service only applies to real drives, not the fake ones in tests.
            let installed = matches!(roots, Roots::LocalDrives) && arke_indexd::client::installed();
            let roots = match roots {
                Roots::Fixed(roots) => roots,
                Roots::LocalDrives => local_drives(shell.as_ref()),
            };
            let roots = without_skipped(roots, &skipped_drives);
            log::info!("global search index roots: {roots:?}");
            let options = local_index_options(
                roots,
                cache,
                installed.then(|| {
                    Arc::new(arke_indexd::client::ServiceJournal::new()) as Arc<dyn Journal>
                }),
                can_build(real),
                skip_build_dirs,
                excluded,
            );
            let service = IndexService::start(fs, options, move || {
                let _ = changed_tx.send(());
            });
            // Receiver already gone (turned off again): the service drops here and stops.
            let _ = service_tx.send((Arc::new(service), installed));
        });
        self.worker = Some(cx.spawn(async move |this, cx| {
            let Ok((service, installed)) = service_rx.recv_async().await else {
                return;
            };
            let stored = this.update(cx, |this, cx| {
                this.service = Some(service);
                this.service_installed = Some(installed);
                this.refresh_status(cx);
            });
            if stored.is_err() {
                return;
            }
            // The service notifies on status changes, and periodically while building.
            while changed_rx.recv_async().await.is_ok() {
                while changed_rx.try_recv().is_ok() {}
                if this.update(cx, Self::refresh_status).is_err() {
                    return;
                }
            }
        }));
    }

    /// Sync network locations with settings: stop removed ones (dropping the service stops
    /// watching), start new ones.
    fn sync_net(&mut self, want: &[PathBuf], cx: &mut Context<Self>) {
        let before = self.net.len();
        self.net.retain(|n| want.contains(&n.root));
        let mut changed = before != self.net.len();
        for root in want {
            if !self.net.iter().any(|n| n.root == *root) {
                self.net.push(NetIndex {
                    root: root.clone(),
                    service: None,
                    status: IndexStatus::default(),
                    offline: false,
                    // Fake test drives count as awake from the start.
                    awake: !matches!(self.roots, Roots::LocalDrives),
                    worker: None,
                });
                self.start_net(root.clone(), cx);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Start indexing a network location. If not opened yet this session (NAS may be asleep), read
    /// only the cache. If opened, probe the server (a few seconds max): reachable means watch and
    /// rewalk when stale (weekly, one connection); otherwise cache only.
    fn start_net(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let real = matches!(self.roots, Roots::LocalDrives);
        let awake = self.net.iter().any(|n| n.root == root && n.awake);
        let skip_build_dirs = self.skip_build_dirs;
        let cache = self
            .cache
            .as_ref()
            .and_then(|c| c.parent())
            .map(|dir| dir.join(format!("index-net-{:016x}.bin", fnv(&root))));
        let (changed_tx, changed_rx) = flume::unbounded::<()>();
        let (service_tx, service_rx) = flume::bounded::<(Arc<IndexService>, bool)>(1);
        let key = root.clone();
        crate::explorer::spawn_thread("index-net", move || {
            let online = awake && (!real || reachable(&root));
            log::info!(
                "global search: network location {} ({})",
                root.display(),
                match (awake, online) {
                    (false, _) => "not opened yet, using last cache",
                    (true, true) => "reachable",
                    (true, false) => "unreachable, using last cache",
                }
            );
            let options = IndexOptions {
                roots: vec![root],
                cache,
                // One connection over the network, walking slowly so the NAS isn't swamped.
                threads: 1,
                thread_init: Arc::new(crate::win::lower_io_priority),
                // The watcher keeps it current; full walk only weekly.
                refresh_after: std::time::Duration::from_hours(24 * 7),
                journal: None,
                can_build: can_build(real),
                skip_build_dirs,
                passive: !online,
                excluded: Vec::new(),
            };
            let service = IndexService::start(fs, options, move || {
                let _ = changed_tx.send(());
            });
            let _ = service_tx.send((Arc::new(service), awake && !online));
        });
        let slot = key.clone();
        let worker = cx.spawn(async move |this, cx| {
            let Ok((service, offline)) = service_rx.recv_async().await else {
                return;
            };
            let key2 = key.clone();
            let stored = this.update(cx, move |this, cx| {
                if let Some(net) = this.net.iter_mut().find(|n| n.root == key2) {
                    net.service = Some(service);
                    net.offline = offline;
                }
                this.refresh_status(cx);
            });
            if stored.is_err() {
                return;
            }
            while changed_rx.recv_async().await.is_ok() {
                while changed_rx.try_recv().is_ok() {}
                if this.update(cx, Self::refresh_status).is_err() {
                    return;
                }
            }
        });
        if let Some(net) = self.net.iter_mut().find(|n| n.root == slot) {
            net.worker = Some(worker);
        }
    }

    /// A path was opened: if it's inside a network location that isn't awake yet, wake its index
    /// (the NAS is up, so it can update).
    pub(crate) fn wake(path: &std::path::Path, cx: &mut App) {
        let Some(global) = Self::global(cx) else {
            return;
        };
        let lower = path.to_string_lossy().to_lowercase();
        let sleeping: Vec<PathBuf> = global
            .read(cx)
            .net
            .iter()
            .filter(|n| !n.awake && lower.starts_with(&n.root.to_string_lossy().to_lowercase()))
            .map(|n| n.root.clone())
            .collect();
        if sleeping.is_empty() {
            return;
        }
        global.update(cx, |this, cx| {
            for root in sleeping {
                if let Some(net) = this.net.iter_mut().find(|n| n.root == root) {
                    log::info!("global search: {} opened, updating index", root.display());
                    net.awake = true;
                    net.service = None;
                    net.worker = None;
                    net.offline = false;
                    this.start_net(root, cx);
                }
            }
            cx.notify();
        });
    }

    /// Arke regained focus: probe previously unreachable network locations and restart the ones
    /// that respond.
    pub(crate) fn recheck_network(&mut self, cx: &mut Context<Self>) {
        let offline: Vec<PathBuf> = self
            .net
            .iter()
            .filter(|n| n.offline && n.service.is_some())
            .map(|n| n.root.clone())
            .collect();
        if offline.is_empty() {
            return;
        }
        let (tx, rx) = flume::bounded::<Vec<PathBuf>>(1);
        crate::explorer::spawn_thread("index-net-recheck", move || {
            let _ = tx.send(offline.into_iter().filter(|r| reachable(r)).collect());
        });
        cx.spawn(async move |this, cx| {
            let Ok(back) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                for root in back {
                    if let Some(net) = this.net.iter_mut().find(|n| n.root == root) {
                        net.service = None;
                        net.worker = None;
                        net.offline = false;
                        this.start_net(root, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Item count of local plus network indexes; `None` until the local index is built.
    pub(crate) fn total_count(&self) -> Option<usize> {
        let net: usize = self.net.iter().filter_map(|n| n.status.count).sum();
        self.status.count.map(|n| n + net)
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        for net in &mut self.net {
            let status = net.service.as_ref().map(|s| s.status()).unwrap_or_default();
            if status != net.status {
                net.status = status;
                cx.notify();
            }
        }
        let status = self
            .service
            .as_ref()
            .map(|s| s.status())
            .unwrap_or_default();
        if status != self.status {
            self.status = status;
            cx.notify();
        }
    }

    /// The local drives' index service, for folder sizes (it answers only for volumes it follows
    /// live through the change journal).
    pub(crate) fn local_service(cx: &App) -> Option<Arc<IndexService>> {
        Self::global(cx).and_then(|g| g.read(cx).service.clone())
    }

    /// Query in the background; `None` when there's no index yet.
    pub(crate) fn search(
        &self,
        query: String,
        limit: usize,
        cx: &App,
    ) -> Option<Task<Vec<IndexHit>>> {
        let service = self.service.clone()?;
        let net: Vec<Arc<IndexService>> =
            self.net.iter().filter_map(|n| n.service.clone()).collect();
        let history = crate::search_history::SearchHistory::snapshot_for_search(cx);
        let signals = rank_signals(history.clone());
        // Its own thread rather than the executor's pool: reading visited items from disk blocks.
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("global-search", move || {
            let clock = crate::win::clock();
            // Top hits from each index, merged and ordered by match quality, then by what the
            // user opens and where it is (`arke_core::rank`).
            let mut hits = service.search(&query, clock, limit, false);
            for service in &net {
                hits.extend(service.search(&query, clock, limit, false));
            }
            // Visited items the local index matched but didn't rank high enough to return. Not
            // for network locations: reading an item there can hang on a sleeping NAS.
            let extra = history.map_or_else(Vec::new, |history| {
                service.frecent_hits(&query, clock, false, &history, FRECENT_MAX)
            });
            let parsed = arke_core::index_query::Query::parse(&query, clock);
            let _ = tx.send(arke_core::rank::merge(
                hits, extra, &parsed, &signals, limit,
            ));
        });
        Some(
            cx.background_executor()
                .spawn(async move { rx.recv_async().await.unwrap_or_default() }),
        )
    }

    /// Install or remove the background service (UAC prompt). Restarts indexing afterwards to use
    /// (or stop using) the journal.
    pub(crate) fn set_service(&mut self, install: bool, cx: &mut Context<Self>) {
        if self.service_busy {
            return;
        }
        // Single binary: the service is `arke.exe --indexd …` (copied to Program Files on install).
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        self.service_busy = true;
        cx.notify();
        let (tx, rx) = flume::bounded::<bool>(1);
        crate::explorer::spawn_thread("index-service-setup", move || {
            let args = if install {
                let Some(sid) = arke_indexd::sec::current_user_sid() else {
                    let _ = tx.send(false);
                    return;
                };
                format!("--indexd install --user {sid}")
            } else {
                "--indexd uninstall".to_string()
            };
            let ok = match crate::win::run_elevated(&exe, &args) {
                Ok(0) => true,
                Ok(code) => {
                    log::warn!(
                        "background service {} failed (exit code {code})",
                        if install { "install" } else { "removal" }
                    );
                    false
                }
                // e.g. the user declined the UAC prompt
                Err(err) => {
                    log::info!(
                        "background service {} not done: {err}",
                        if install { "install" } else { "removal" }
                    );
                    false
                }
            };
            let _ = tx.send(ok);
        });
        cx.spawn(async move |this, cx| {
            let ok = rx.recv_async().await.unwrap_or(false);
            let _ = this.update(cx, |this, cx| {
                this.service_busy = false;
                if ok && this.enabled {
                    // Restart: query the service and choose journal or watcher.
                    this.service = None;
                    this.worker = None;
                    this.status = IndexStatus::default();
                    this.start(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Rewalk everything ("Rebuild").
    pub(crate) fn rebuild(&self) {
        if let Some(service) = &self.service {
            service.rebuild();
        }
    }
}

/// Options for the local drives index. Shared with `--bench index` (see `bench_index.rs`) so the
/// benchmark builds exactly what the app builds.
pub(crate) fn local_index_options(
    roots: Vec<PathBuf>,
    cache: Option<PathBuf>,
    journal: Option<Arc<dyn Journal>>,
    can_build: Arc<dyn Fn() -> bool + Send + Sync>,
    skip_build_dirs: bool,
    excluded: Vec<PathBuf>,
) -> IndexOptions {
    // Half the cores (max 8) so indexing doesn't saturate small machines.
    let threads =
        (std::thread::available_parallelism().map_or(4, std::num::NonZero::get) / 2).clamp(1, 8);
    IndexOptions {
        roots,
        cache,
        threads,
        thread_init: Arc::new(crate::win::lower_io_priority),
        // An index under a day old is used as is, no rescan at startup (changes while closed wait
        // for the scheduled rebuild).
        refresh_after: std::time::Duration::from_hours(24),
        journal,
        can_build,
        skip_build_dirs,
        passive: false,
        excluded,
    }
}

/// Drops the drives the user excluded in settings.
pub(crate) fn without_skipped(roots: Vec<PathBuf>, skipped: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .into_iter()
        .filter(|r| !skipped.iter().any(|s| same_path(s, r)))
        .collect()
}

/// When a walk may run: real drives check system resources (defer under memory pressure or battery
/// saver); fake test drives always can.
fn can_build(real: bool) -> Arc<dyn Fn() -> bool + Send + Sync> {
    if real {
        Arc::new(crate::win::background_work_ok)
    } else {
        Arc::new(|| true)
    }
}

/// Whether the server is reachable (network locations; local is always reachable). Takes a few
/// seconds at most; call from a background thread.
fn reachable(root: &std::path::Path) -> bool {
    use arke_core::net::{NetProbe, Reach};
    let probe = crate::win_net::WinNetProbe;
    let Some(server) = probe
        .unc(root)
        .and_then(|unc| arke_core::net::server_of(&unc))
    else {
        return true;
    };
    !matches!(probe.reach(&server), Reach::Down | Reach::NoSuchHost)
}

/// Hash for network cache file names (FNV-1a, case-insensitive, stable across versions).
fn fnv(path: &std::path::Path) -> u64 {
    path.to_string_lossy()
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// Same path, ignoring case and trailing `\`
pub(crate) fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    let key = |p: &std::path::Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    key(a) == key(b)
}

/// Whether a local folder is in global search scope: `Some(true)` searched, `Some(false)` excluded
/// itself. `None` when not applicable (network, global search off, or a parent or whole drive
/// excluded; change it there or in settings). Reads settings only.
pub(crate) fn local_search_state(
    st: &crate::settings::Settings,
    path: &std::path::Path,
) -> Option<bool> {
    if !st.global_search || is_network_path(path) {
        return None;
    }
    let within = |root: &std::path::Path| {
        let (p, r) = (
            path.to_string_lossy().to_lowercase(),
            root.to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .to_lowercase(),
        );
        p == r
            || p.strip_prefix(&r)
                .is_some_and(|rest| rest.starts_with('\\'))
    };
    if st.search_excluded.iter().any(|e| same_path(e, path)) {
        return Some(false);
    }
    let skipped = st.search_skipped_drives.iter().any(|d| within(d))
        || st.search_excluded.iter().any(|e| within(e));
    (!skipped).then_some(true)
}

/// Folder context menu "Exclude from this PC search" / "Include in this PC search". Removes the
/// exclusion when it's on this folder; an excluded parent can't be overridden here (use settings).
pub(crate) fn toggle_local_exclusion(path: PathBuf, cx: &mut App) {
    SettingsStore::global(cx).update(cx, |s, cx| {
        s.update(
            |st| {
                let before = st.search_excluded.len();
                st.search_excluded.retain(|e| !same_path(e, &path));
                if st.search_excluded.len() == before {
                    st.search_excluded.push(path);
                }
            },
            cx,
        );
    });
}

/// Roots of local fixed drives (no network or removable drives). Call from a background thread.
pub(crate) fn local_drives(shell: &dyn Shell) -> Vec<PathBuf> {
    shell
        .load_places()
        .into_iter()
        .filter(|p| matches!(p.kind, PlaceKind::Drive { remote: false, .. }))
        .map(|p| p.path)
        .filter(|p| shell.is_local_disk(p))
        .collect()
}

/// `%LOCALAPPDATA%\Arke\index.bin` (string handling only).
pub(crate) fn default_cache_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Arke").join("index.bin"))
}

/// Whether a path is on the network (`\\server\share\…` or a mapped drive). Uses the path and the
/// drive type Windows records, no network access.
pub(crate) fn is_network_path(path: &std::path::Path) -> bool {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\") && !text.starts_with(r"\\?\") && !text.starts_with(r"\\.\") {
        return true;
    }
    path.components().next().is_some_and(|first| {
        let root = PathBuf::from(format!("{}\\", first.as_os_str().to_string_lossy()));
        crate::win::network_drive_roots().contains(&root)
    })
}

/// Whether a network location is in "this PC" search: added explicitly, or (in auto mode) a mapped
/// drive or recent share that isn't excluded.
pub(crate) fn is_searched(st: &crate::settings::Settings, path: &std::path::Path) -> bool {
    st.search_network.iter().any(|p| p == path)
        || (auto_covered(st, path) && !st.search_network_excluded.iter().any(|p| p == path))
}

fn auto_covered(st: &crate::settings::Settings, path: &std::path::Path) -> bool {
    st.search_network_auto
        && (st.net_places.iter().any(|p| p == path)
            || crate::win::network_drive_roots().iter().any(|p| p == path))
}

/// Add to or remove from "this PC" search (sidebar and folder context menus). Auto-added locations
/// use the exclusion list; adding also turns on global search.
pub(crate) fn toggle_network(path: PathBuf, cx: &mut App) {
    SettingsStore::global(cx).update(cx, |s, cx| {
        s.update(
            |st| {
                let auto = auto_covered(st, &path);
                if is_searched(st, &path) {
                    st.search_network.retain(|p| *p != path);
                    if auto && !st.search_network_excluded.contains(&path) {
                        st.search_network_excluded.push(path);
                    }
                } else {
                    st.search_network_excluded.retain(|p| *p != path);
                    if !auto && !st.search_network.contains(&path) {
                        st.search_network.push(path);
                    }
                    st.global_search = true;
                }
            },
            cx,
        );
    });
}

/// Per-drive change journal sizes (shown and adjusted on the settings page). Queried from the
/// drive; Arke keeps no record.
#[derive(Clone, Default)]
pub(crate) struct JournalSizes {
    /// (drive, max size, current size) in bytes; only drives with a journal
    pub(crate) drives: Vec<(PathBuf, u64, u64)>,
    /// Already queried (otherwise queried when the settings page opens)
    pub(crate) loaded: bool,
    /// Drive being resized (waiting for UAC consent)
    pub(crate) busy: Option<PathBuf>,
    /// Result of the last resize (failed or canceled)
    pub(crate) error: Option<String>,
}

impl Global for JournalSizes {}

/// Requery journal sizes for these drives (background, query only, no elevation needed).
pub(crate) fn refresh_journal_sizes(roots: Vec<PathBuf>, cx: &mut App) {
    let mut state = cx.try_global::<JournalSizes>().cloned().unwrap_or_default();
    state.loaded = true;
    cx.set_global(state);
    let (tx, rx) = flume::bounded::<Vec<(PathBuf, u64, u64)>>(1);
    crate::explorer::spawn_thread("journal-size", move || {
        let found = roots
            .into_iter()
            .filter_map(|root| {
                arke_indexd::journal_size(&root).map(|(max, used)| (root, max, used))
            })
            .collect();
        let _ = tx.send(found);
    });
    cx.spawn(async move |cx| {
        if let Ok(drives) = rx.recv_async().await {
            cx.update(|cx| {
                let mut state = cx.try_global::<JournalSizes>().cloned().unwrap_or_default();
                state.drives = drives;
                cx.set_global(state);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

/// Resize `root`'s change journal to at most `mb` MB via UAC and `arke.exe --indexd journal-size`
/// (no records deleted, no registry changes). Requeries sizes afterwards.
pub(crate) fn set_journal_size(root: PathBuf, mb: u64, roots: Vec<PathBuf>, cx: &mut App) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut state = cx.try_global::<JournalSizes>().cloned().unwrap_or_default();
    state.busy = Some(root.clone());
    state.error = None;
    cx.set_global(state);
    cx.refresh_windows();
    let (tx, rx) = flume::bounded(1);
    crate::explorer::spawn_thread("journal-resize", move || {
        let root = crate::win::quote_arg(&root.to_string_lossy());
        let args = format!("--indexd journal-size {root} {mb}");
        let _ = tx.send(crate::win::run_elevated(&exe, &args));
    });
    cx.spawn(async move |cx| {
        // Shown under the drive as is; no reply (thread gone) shows nothing.
        let error = match rx.recv_async().await {
            Ok(Ok(0)) | Err(_) => None,
            Ok(Ok(code)) => Some(format!("exit {code}")),
            Ok(Err(err)) => Some(err.to_string()),
        };
        cx.update(|cx| {
            let mut state = cx.try_global::<JournalSizes>().cloned().unwrap_or_default();
            state.busy = None;
            state.error = error.filter(|e| !e.is_empty());
            cx.set_global(state);
            refresh_journal_sizes(roots, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn local_folders_can_be_excluded_only_where_they_were_excluded() {
        let p = PathBuf::from;
        let mut st = crate::settings::Settings {
            global_search: true,
            search_excluded: vec![p(r"C:\Games")],
            search_skipped_drives: vec![p(r"D:\")],
            ..crate::settings::Settings::default()
        };
        assert_eq!(local_search_state(&st, Path::new(r"C:\Work")), Some(true));
        assert_eq!(local_search_state(&st, Path::new(r"c:\games")), Some(false));
        // Parent excluded or whole drive not indexed: can't change it here.
        assert_eq!(local_search_state(&st, Path::new(r"C:\Games\Steam")), None);
        assert_eq!(local_search_state(&st, Path::new(r"D:\Media")), None);
        assert_eq!(
            local_search_state(&st, Path::new(r"C:\GamesBackup")),
            Some(true)
        );
        st.global_search = false;
        assert_eq!(local_search_state(&st, Path::new(r"C:\Work")), None);
        assert!(same_path(Path::new(r"C:\Games\"), Path::new(r"c:\GAMES")));
    }
}
