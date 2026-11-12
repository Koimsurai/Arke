//! "Open folders in Arke": shows folders opened from anywhere in Arke instead of File Explorer.
//!
//! This is the net for what the folder handler registration (`folder_handler.rs`) cannot reach:
//! `explorer.exe /select,...` and any program that opens a File Explorer window directly. Folder
//! opens through the shell (double click, Win+E, "Show in folder" of browsers and editors) go to Arke
//! through the registration and never create an Explorer window. Windows that Arke itself sent to
//! Explorer (`arke.exe` as the folder handler hands everything it can't show to `explorer.exe`) are
//! never taken back: both sides decide with the same `core::takeover::classify`.
//!
//! Only documented shell automation is used, no registry, hooks or injection:
//! - `CLSID_ShellWindows` (`IShellWindows`) is the shell's list of open Explorer windows; its
//!   `DShellWindowsEvents::WindowRegistered` event says when a new one appears.
//! - A new top-level `CabinetWClass` window (its `HWND` wasn't seen before; Windows 11 tabs share
//!   their window's handle, so tabs in a window the user is already using are never touched) is
//!   read through `IShellBrowser` -> `IFolderView2`: the folder's parsing name and its selection.
//! - `core::takeover::classify` decides whether Arke can show that folder. If so the window is
//!   hidden, Arke opens the folder (with the selection) and only then the Explorer window is closed
//!   with `IWebBrowser2::Quit`. Anything else is never touched.
//!
//! Left alone: windows opened while Shift is held, windows of elevated processes, windows Arke
//! itself asked Explorer for ("Show in File Explorer", see [`note_reveal`]), and every folder Arke
//! cannot show (Control Panel, Network, phones, archives, search results, ...).
//!
//! Everything runs on one dedicated STA thread with a message loop (COM events arrive as window
//! messages). `ARKE_TAKEOVER_TEST_ROOT` restricts the takeover to folders inside that directory, so
//! live checks never touch the tester's own Explorer windows.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use arke_core::takeover::{self, Bypass, ShellAttrs, Target};
use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IConnectionPointContainer, IDispatch_Impl,
    IServiceProvider,
};
use windows::Win32::System::Com::{DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, ITypeInfo};
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_STREAM};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::{
    DShellWindowsEvents, DShellWindowsEvents_Impl, IFolderView2, IPersistFolder2, IShellBrowser,
    IShellItem, IShellWindows, IWebBrowser2, PathCreateFromUrlAlloc, SHCreateItemFromIDList,
    SID_STopLevelBrowser, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClassNameW, GetMessageW, GetWindowThreadProcessId, IsWindow, KillTimer,
    MSG, PostThreadMessageW, SW_HIDE, SW_SHOWNA, SetTimer, ShowWindowAsync, TranslateMessage,
    WM_QUIT, WM_TIMER,
};
use windows::core::{GUID, Interface, PCWSTR, implement};

/// How long a new window may take to show its first folder before it is given up on.
const VIEW_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for a selection ("Show in folder" sets it right after navigating).
const SELECTION_WAIT: Duration = Duration::from_millis(300);
/// How long Arke has to open its window before Explorer's is shown again.
const OPEN_TIMEOUT: Duration = Duration::from_millis(2500);
/// Poll interval while a new window is being examined.
const FAST_TICK_MS: u32 = 10;
/// Safety net in case an event is missed or the shell's window list went away.
const SLOW_TICK_MS: u32 = 2000;

/// Folders Arke itself handed to Explorer.
static BYPASS: LazyLock<Mutex<Bypass>> = LazyLock::new(|| Mutex::new(Bypass::default()));

/// Arke is about to open `path` in Explorer ("Show in File Explorer"); that window is not taken
/// over.
pub(crate) fn note_reveal(path: &Path, select: bool) {
    // Servers never reach Explorer (the remote shell refuses them first); keep them out anyway.
    if arke_core::remote::is_remote(path) {
        return;
    }
    BYPASS.lock().add(path, select, Instant::now());
}

/// A folder to show in Arke.
pub(crate) struct OpenRequest {
    pub(crate) target: Target,
    /// File names to select (items directly in the folder).
    pub(crate) select: Vec<String>,
    /// Thread of the Explorer window being replaced; it owns the foreground until it goes.
    pub(crate) explorer_thread: u32,
    /// Answered `true` once Arke has a window for it; Explorer's window is closed only then.
    pub(crate) reply: flume::Sender<bool>,
}

/// The running watcher; dropping it (or [`Watcher::stop`]) ends the thread.
pub(crate) struct Watcher {
    thread_id: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
}

impl Watcher {
    /// Starts watching on a new thread. Requests arrive on `tx`.
    pub(crate) fn start(tx: flume::Sender<OpenRequest>) -> Option<Self> {
        let thread_id = Arc::new(AtomicU32::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let (id, flag) = (thread_id.clone(), stopped.clone());
        std::thread::Builder::new()
            .name("takeover".into())
            .spawn(move || run(&tx, &id, &flag))
            .ok()?;
        Some(Self { thread_id, stopped })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let id = self.thread_id.load(Ordering::SeqCst);
        if id != 0 {
            // SAFETY: posts WM_QUIT to our own watcher thread, which exits its message loop.
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }
}

/// Receives `WindowRegistered` and only notes it; the message loop does the work.
#[allow(clippy::ref_as_ptr, clippy::inline_always)]
mod sink {
    use super::{
        AtomicBool, DISPATCH_FLAGS, DISPPARAMS, DShellWindowsEvents, DShellWindowsEvents_Impl,
        EXCEPINFO, GUID, IDispatch_Impl, ITypeInfo, Ordering, PCWSTR, VARIANT, implement,
    };
    use std::sync::Arc;
    use windows::Win32::Foundation::E_NOTIMPL;

    /// `DISPID_WINDOWREGISTERED`.
    const WINDOW_REGISTERED: i32 = 200;

    #[implement(DShellWindowsEvents)]
    pub(super) struct Sink {
        pub(super) registered: Arc<AtomicBool>,
    }

    impl DShellWindowsEvents_Impl for Sink_Impl {}

    impl IDispatch_Impl for Sink_Impl {
        fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
            Ok(0)
        }

        fn GetTypeInfo(&self, _: u32, _: u32) -> windows::core::Result<ITypeInfo> {
            Err(E_NOTIMPL.into())
        }

        fn GetIDsOfNames(
            &self,
            _: *const GUID,
            _: *const PCWSTR,
            _: u32,
            _: u32,
            _: *mut i32,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Invoke(
            &self,
            dispid: i32,
            _: *const GUID,
            _: u32,
            _: DISPATCH_FLAGS,
            _: *const DISPPARAMS,
            _: *mut VARIANT,
            _: *mut EXCEPINFO,
            _: *mut u32,
        ) -> windows::core::Result<()> {
            if dispid == WINDOW_REGISTERED {
                self.registered.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }
}

/// A new Explorer window being examined.
struct Candidate {
    hwnd: isize,
    browser: IWebBrowser2,
    first_seen: Instant,
    /// Shift was down when the window appeared.
    shift: bool,
    stage: Stage,
}

enum Stage {
    /// Waiting for the first folder to show.
    Looking,
    /// Taken-over folder known; Explorer's window hidden; waiting for a selection.
    Selecting {
        target: Target,
        /// When the window's real folder showed up (the selection wait starts then); `None` while
        /// the decision rests on the navigation URL alone.
        view_ready: Option<Instant>,
        hidden_after: Duration,
    },
    /// Arke was asked to open it; waiting for the answer.
    Opening {
        reply: flume::Receiver<bool>,
        since: Instant,
        shown: Duration,
        selected: usize,
    },
}

struct Watch {
    windows: IShellWindows,
    seen: HashSet<isize>,
    candidates: Vec<Candidate>,
    tx: flume::Sender<OpenRequest>,
    test_root: Option<PathBuf>,
    fast_timer: usize,
}

fn run(tx: &flume::Sender<OpenRequest>, thread_id: &AtomicU32, stopped: &AtomicBool) {
    log::info!("takeover: thread started");
    // SAFETY: first COM call on this thread; paired with CoUninitialize below when it succeeds.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    thread_id.store(
        // SAFETY: plain Win32 call.
        unsafe { GetCurrentThreadId() },
        Ordering::SeqCst,
    );
    if stopped.load(Ordering::SeqCst) {
        // Stopped before the id was published; nobody will post WM_QUIT.
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        return;
    }
    let registered = Arc::new(AtomicBool::new(false));
    let test_root = std::env::var_os("ARKE_TAKEOVER_TEST_ROOT").map(PathBuf::from);
    let mut connection = connect(&registered);
    let Some((windows, _)) = connection.as_ref() else {
        log::warn!("takeover: cannot reach the shell's window list, not watching");
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        return;
    };
    let mut watch = Watch {
        windows: windows.clone(),
        seen: HashSet::new(),
        candidates: Vec::new(),
        tx: tx.clone(),
        test_root,
        fast_timer: 0,
    };
    watch.seed();
    // SAFETY: a thread timer (no window): WM_TIMER arrives in this thread's queue.
    let slow_timer = unsafe { SetTimer(None, 0, SLOW_TICK_MS, None) };
    log::info!(
        "takeover: watching ({} Explorer windows already open{})",
        watch.seen.len(),
        watch
            .test_root
            .as_ref()
            .map_or(String::new(), |r| format!(", test root {}", r.display()))
    );

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out pointer; blocks until a message arrives.
        let got = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break;
        }
        let is_slow = msg.message == WM_TIMER && msg.hwnd.0.is_null() && msg.wParam.0 == slow_timer;
        if msg.message == WM_TIMER && msg.hwnd.0.is_null() {
            if is_slow {
                if watch.scan().is_err() {
                    // The shell's list went away (Explorer restarted): connect again.
                    connection = connect(&registered);
                    if let Some((windows, _)) = connection.as_ref() {
                        watch.windows = windows.clone();
                    }
                }
            } else {
                watch.step();
            }
        } else {
            // SAFETY: msg came from GetMessageW.
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        if registered.swap(false, Ordering::SeqCst) {
            let _ = watch.scan();
        }
        watch.update_timer();
    }
    // Anything still hidden goes back to the user.
    for c in watch.candidates.drain(..) {
        if !matches!(c.stage, Stage::Looking) {
            // SAFETY: asynchronous post to a window handle; harmless if it is gone.
            let _ = unsafe { ShowWindowAsync(HWND(c.hwnd as *mut _), SW_SHOWNA) };
        }
    }
    // SAFETY: kills the timers created above; unadvising happens as `connection` drops.
    unsafe {
        let _ = KillTimer(None, slow_timer);
        if watch.fast_timer != 0 {
            let _ = KillTimer(None, watch.fast_timer);
        }
    }
    drop(connection);
    log::info!("takeover: stopped");
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// The shell's window list and the event subscription (unsubscribed on drop).
struct Subscription {
    point: windows::Win32::System::Com::IConnectionPoint,
    cookie: u32,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // SAFETY: cookie came from Advise on this connection point.
        let _ = unsafe { self.point.Unadvise(self.cookie) };
    }
}

fn connect(registered: &Arc<AtomicBool>) -> Option<(IShellWindows, Option<Subscription>)> {
    // SAFETY: standard COM activation of the shell's window list in the user's own session.
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }.ok()?;
    let subscription = (|| {
        let container: IConnectionPointContainer = windows.cast().ok()?;
        // SAFETY: valid interface pointers; the sink lives as long as the subscription.
        let point = unsafe { container.FindConnectionPoint(&DShellWindowsEvents::IID) }.ok()?;
        let sink: DShellWindowsEvents = sink::Sink {
            registered: registered.clone(),
        }
        .into();
        // SAFETY: see above.
        let cookie = unsafe { point.Advise(&sink) }.ok()?;
        Some(Subscription { point, cookie })
    })();
    if subscription.is_none() {
        log::warn!("takeover: no window events, falling back to polling");
    }
    Some((windows, subscription))
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: buf is valid and writable for the call.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..usize::try_from(len).unwrap_or(0)])
}

/// Whether the process owning `hwnd` runs elevated (or can't be inspected, which is the same
/// thing from here: not ours to touch).
fn elevated(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    // SAFETY: opens a query-only handle, closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return true;
    };
    let mut token = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: process is a valid handle; token is a valid out pointer.
    let elevated = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
        .ok()
        .is_none_or(|()| {
            let mut info = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            // SAFETY: info and len are valid for the call; token is valid.
            let ok = unsafe {
                GetTokenInformation(
                    token,
                    TokenElevation,
                    Some((&raw mut info).cast()),
                    size_of::<TOKEN_ELEVATION>() as u32,
                    &raw mut len,
                )
            };
            // SAFETY: closes the token handle opened above.
            let _ = unsafe { CloseHandle(token) };
            ok.is_err() || info.TokenIsElevated != 0
        });
    // SAFETY: closes the process handle opened above.
    let _ = unsafe { CloseHandle(process) };
    elevated
}

fn shift_down() -> bool {
    // SAFETY: plain Win32 call; reads the key state, installs nothing.
    unsafe { GetAsyncKeyState(i32::from(VK_SHIFT.0)) < 0 }
}

/// The folder an Explorer window shows: its desktop-absolute parsing name and attributes.
fn read_location(browser: &IWebBrowser2) -> Option<(String, ShellAttrs, IFolderView2)> {
    let provider: IServiceProvider = browser.cast().ok()?;
    // SAFETY: valid interface pointers throughout; results are owned COM objects.
    unsafe {
        let shell_browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser).ok()?;
        let view = shell_browser.QueryActiveShellView().ok()?;
        let folder_view: IFolderView2 = view.cast().ok()?;
        let folder: IPersistFolder2 = folder_view.GetFolder().ok()?;
        let pidl = folder.GetCurFolder().ok()?;
        let item: Result<IShellItem, _> = SHCreateItemFromIDList(pidl);
        CoTaskMemFree(Some(pidl as *const _));
        let item = item.ok()?;
        let name = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING).ok()?;
        let parsing = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        let flags = item
            .GetAttributes(SFGAO_FILESYSTEM | SFGAO_STREAM)
            .unwrap_or_default();
        let attrs = ShellAttrs {
            filesystem: flags.0 & SFGAO_FILESYSTEM.0 != 0,
            stream: flags.0 & SFGAO_STREAM.0 != 0,
        };
        Some((parsing?, attrs, folder_view))
    }
}

/// The folder a window is navigating to, from its URL (known before the view exists). Only
/// `file:` URLs count.
fn navigation_path(browser: &IWebBrowser2) -> Option<String> {
    // SAFETY: valid interface pointer; the returned string is freed after copying.
    unsafe {
        let url = browser.LocationURL().ok()?;
        if url.len() < 5 || !url.to_string().get(..5)?.eq_ignore_ascii_case("file:") {
            return None;
        }
        let mut out = windows::core::PWSTR::null();
        PathCreateFromUrlAlloc(PCWSTR(url.as_ptr()), &raw mut out, 0).ok()?;
        let path = out.to_string().ok();
        CoTaskMemFree(Some(out.0 as *const _));
        path
    }
}

/// Full paths of the selected items.
fn read_selection(browser: &IWebBrowser2) -> Vec<String> {
    let Some((_, _, view)) = read_location(browser) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // SAFETY: valid interface pointers; names are freed after copying.
    unsafe {
        let Ok(array) = view.GetSelection(false) else {
            return out;
        };
        for ix in 0..array.GetCount().unwrap_or(0) {
            let Ok(item) = array.GetItemAt(ix) else {
                continue;
            };
            if let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                if let Ok(path) = name.to_string() {
                    out.push(path);
                }
                CoTaskMemFree(Some(name.0 as *const _));
            }
        }
    }
    out
}

impl Watch {
    /// Notes every Explorer window that already exists, so only later ones are taken over.
    fn seed(&mut self) {
        for (hwnd, _) in self.list() {
            self.seen.insert(hwnd);
        }
    }

    /// Explorer windows in the shell's list (one entry per tab).
    fn list(&self) -> Vec<(isize, IWebBrowser2)> {
        let mut out = Vec::new();
        // SAFETY: valid interface pointers; each item is an owned COM object.
        unsafe {
            let count = self.windows.Count().unwrap_or(0);
            for ix in 0..count {
                let index = VARIANT::from(ix);
                let Ok(item) = self.windows.Item(&index) else {
                    continue;
                };
                let Ok(browser) = item.cast::<IWebBrowser2>() else {
                    continue;
                };
                let Ok(raw) = browser.HWND() else {
                    continue;
                };
                let hwnd = raw.0;
                if class_name(HWND(hwnd as *mut _)) == "CabinetWClass" {
                    out.push((hwnd, browser));
                }
            }
        }
        out
    }

    /// Picks up windows with a handle not seen before. Errors mean the shell's list is gone.
    fn scan(&mut self) -> windows::core::Result<()> {
        let shift = shift_down();
        // SAFETY: valid interface pointer.
        unsafe { self.windows.Count() }?;
        // Forget closed windows so a reused handle counts as new again.
        // SAFETY: IsWindow only inspects the handle.
        self.seen
            .retain(|&h| unsafe { IsWindow(Some(HWND(h as *mut _))) }.as_bool());
        let now = Instant::now();
        for (hwnd, browser) in self.list() {
            if self.seen.insert(hwnd) {
                log::info!("takeover: new Explorer window {hwnd:x}");
                self.candidates.push(Candidate {
                    hwnd,
                    browser,
                    first_seen: now,
                    shift,
                    stage: Stage::Looking,
                });
            }
        }
        self.step();
        Ok(())
    }

    /// Moves every candidate along; runs on the fast timer and right after a scan.
    fn step(&mut self) {
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.retain_mut(|c| !self.advance(c));
        // advance() never adds candidates; scan() is the only producer.
        self.candidates = candidates;
    }

    /// Returns `true` when the candidate is finished.
    #[allow(clippy::too_many_lines)]
    fn advance(&self, c: &mut Candidate) -> bool {
        let hwnd = HWND(c.hwnd as *mut _);
        // SAFETY: IsWindow only inspects the handle.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return true;
        }
        let now = Instant::now();
        match &c.stage {
            Stage::Looking => {
                // The window's view comes up late; the navigation URL is known earlier, which
                // lets the window disappear sooner. The real folder is checked afterwards.
                let located = read_location(&c.browser)
                    .map(|(parsing, attrs, _)| (parsing, attrs, Some(now)))
                    .or_else(|| {
                        navigation_path(&c.browser).map(|path| {
                            let attrs = ShellAttrs {
                                filesystem: true,
                                stream: false,
                            };
                            (path, attrs, None)
                        })
                    });
                let Some((parsing, attrs, view_ready)) = located else {
                    let gave_up = now.duration_since(c.first_seen) > VIEW_TIMEOUT;
                    if gave_up {
                        log::info!("takeover: window {:x} never showed a folder", c.hwnd);
                    }
                    return gave_up;
                };
                let Some(target) = self.decide(c, hwnd, &parsing, attrs) else {
                    return true;
                };
                // Hide right away so the window doesn't linger while Arke opens.
                // SAFETY: asynchronous post to a window handle.
                let _ = unsafe { ShowWindowAsync(hwnd, SW_HIDE) };
                c.stage = Stage::Selecting {
                    target,
                    view_ready,
                    hidden_after: now.duration_since(c.first_seen),
                };
                false
            }
            Stage::Selecting {
                target,
                view_ready,
                hidden_after,
            } => {
                let Some(ready) = *view_ready else {
                    // Decided from the URL: confirm with the real folder before going on.
                    match read_location(&c.browser) {
                        Some((parsing, attrs, _)) => {
                            if takeover::classify(&parsing, attrs).as_ref() == Some(target) {
                                c.stage = Stage::Selecting {
                                    target: target.clone(),
                                    view_ready: Some(now),
                                    hidden_after: *hidden_after,
                                };
                                return false;
                            }
                            log::info!("takeover: {parsing} is not what the URL said, restoring");
                        }
                        None if now.duration_since(c.first_seen) <= VIEW_TIMEOUT => return false,
                        None => {}
                    }
                    // SAFETY: asynchronous post to a window handle.
                    let _ = unsafe { ShowWindowAsync(hwnd, SW_SHOWNA) };
                    return true;
                };
                let folder_target = matches!(target, Target::Folder(_));
                let items = if folder_target {
                    read_selection(&c.browser)
                } else {
                    Vec::new()
                };
                if folder_target && items.is_empty() && now.duration_since(ready) < SELECTION_WAIT {
                    return false;
                }
                let select: Vec<String> = match target {
                    Target::Folder(folder) => takeover::selected_names(folder, &items),
                    _ => Vec::new(),
                };
                let selected = select.len();
                let (reply, answer) = flume::bounded(1);
                // SAFETY: only reads the window's thread id.
                let explorer_thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
                let request = OpenRequest {
                    target: target.clone(),
                    select,
                    explorer_thread,
                    reply,
                };
                if self.tx.send(request).is_err() {
                    // Arke is shutting down.
                    // SAFETY: asynchronous post to a window handle.
                    let _ = unsafe { ShowWindowAsync(hwnd, SW_SHOWNA) };
                    return true;
                }
                c.stage = Stage::Opening {
                    reply: answer,
                    since: now,
                    shown: *hidden_after,
                    selected,
                };
                false
            }
            Stage::Opening {
                reply,
                since,
                shown,
                selected,
            } => match reply.try_recv() {
                Ok(true) => {
                    // SAFETY: valid interface pointer; closes the Explorer window.
                    let closed = unsafe { c.browser.Quit() };
                    log::info!(
                        "takeover: Explorer window visible {} ms before hiding, Arke opened in {} ms, \
                         {} ms from the window appearing to closing it, {} selected ({})",
                        shown.as_millis(),
                        now.duration_since(*since).as_millis(),
                        now.duration_since(c.first_seen).as_millis(),
                        selected,
                        closed.map_or_else(|e| format!("Quit failed: {e}"), |()| "closed".into())
                    );
                    true
                }
                Ok(false) | Err(flume::TryRecvError::Disconnected) => {
                    log::warn!(
                        "takeover: Arke could not open the folder, leaving Explorer's window"
                    );
                    // SAFETY: asynchronous post to a window handle.
                    let _ = unsafe { ShowWindowAsync(hwnd, SW_SHOWNA) };
                    true
                }
                Err(flume::TryRecvError::Empty) => {
                    if now.duration_since(*since) > OPEN_TIMEOUT {
                        // SAFETY: asynchronous post to a window handle.
                        let _ = unsafe { ShowWindowAsync(hwnd, SW_SHOWNA) };
                        return true;
                    }
                    false
                }
            },
        }
    }

    /// What to do with a window showing `parsing`; `None` leaves it to Explorer.
    fn decide(
        &self,
        c: &Candidate,
        hwnd: HWND,
        parsing: &str,
        attrs: ShellAttrs,
    ) -> Option<Target> {
        let Some(target) = takeover::classify(parsing, attrs) else {
            log::info!("takeover: leaving {parsing} to Explorer");
            return None;
        };
        if !takeover::allowed_by_test_root(&target, self.test_root.as_deref()) {
            return None;
        }
        if c.shift || shift_down() {
            log::info!("takeover: Shift held, leaving {parsing} to Explorer");
            return None;
        }
        if elevated(hwnd) {
            log::info!("takeover: elevated window, leaving {parsing} to Explorer");
            return None;
        }
        if let Target::Folder(folder) = &target
            && BYPASS.lock().take(folder, Instant::now())
        {
            log::info!("takeover: Arke asked for this window, leaving {parsing} to Explorer");
            return None;
        }
        Some(target)
    }

    /// The fast timer runs only while a window is being examined.
    fn update_timer(&mut self) {
        let busy = !self.candidates.is_empty();
        if busy && self.fast_timer == 0 {
            // SAFETY: a thread timer; WM_TIMER arrives in this thread's queue.
            self.fast_timer = unsafe { SetTimer(None, 0, FAST_TICK_MS, None) };
        } else if !busy && self.fast_timer != 0 {
            // SAFETY: kills the timer created above.
            let _ = unsafe { KillTimer(None, self.fast_timer) };
            self.fast_timer = 0;
        }
    }
}
