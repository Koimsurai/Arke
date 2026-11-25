//! "Open folders in Arke": shows folders opened from anywhere in Arke instead of File Explorer.
//!
//! This is the net for what the folder handler registration (`folder_handler.rs`) cannot reach:
//! `explorer.exe /select,...` and any program that opens a File Explorer window directly. Folder
//! opens through the shell (double click, Win+E, "Show in folder" of browsers and editors) go to Arke
//! through the registration and never create an Explorer window. Windows that Arke itself sent to
//! Explorer (`arke.exe` as the folder handler hands everything it can't show to `explorer.exe`) are
//! never taken back: both sides decide with the same `core::takeover::classify`.
//!
//! No registry and no injection: documented shell automation, plus one out-of-context WinEvent hook
//! (Windows posts the events to Arke's own thread; no code is loaded into other processes).
//! - `CLSID_ShellWindows` (`IShellWindows`) is the shell's list of open Explorer windows; its
//!   `DShellWindowsEvents::WindowRegistered` event says when a new one appears.
//! - A new top-level `CabinetWClass` window (its `HWND` wasn't seen before; Windows 11 tabs share
//!   their window's handle, so tabs in a window the user is already using are never touched) is
//!   read through `IShellBrowser` -> `IFolderView2`: the folder's parsing name and its selection.
//! - `core::takeover::classify` decides whether Arke can show that folder. If so the window is
//!   hidden, Arke opens the folder (with the selection) and only then the Explorer window is closed
//!   with `IWebBrowser2::Quit`. Anything else is never touched.
//!
//! The early path (#285) keeps that window from showing at all. `explorer.exe <folder>` is a short
//! launcher that hands the request to the shell and exits; the shell's window shows 0.6–0.8 s after
//! the launcher started, but the launcher makes its first window (COM's) about 60 ms in. The hook
//! (`EVENT_OBJECT_CREATE`) sees that window; once per process the image is checked, and for
//! `<Windows>\explorer.exe` its command line is read (`core::takeover::parse_explorer_args`,
//! `launch_target`: the same answers `classify` gives for the window). When the watcher would take
//! that window, the launcher is ended before it hands over (ending it up to 300 ms in never left a
//! window) and Arke opens the folder. If it had handed over already, the window still comes and the
//! watcher closes it without opening the folder again ([`takeover::Claims`]). Never touched: the
//! shell itself, `explorer.exe` without arguments, launchers Arke started, launchers above Arke's
//! integrity level, and everything the watcher leaves alone. `ARKE_EARLY_TAKEOVER=0` turns it off.
//!
//! Left alone: File Explorer opened on purpose (a window at Home or This PC: the taskbar, Start or a
//! bare `explorer.exe`), windows opened while Shift is held, windows of elevated processes, windows Arke
//! itself asked Explorer for ("Show in File Explorer", see [`note_reveal`]), and every folder Arke
//! cannot show (Control Panel, Network, phones, archives, search results, ...).
//!
//! Everything runs on one dedicated STA thread with a message loop (COM and WinEvent events arrive
//! through it). `ARKE_TAKEOVER_TEST_ROOT` restricts the takeover to folders inside that directory,
//! so live checks never touch the tester's own Explorer windows.

use std::cell::RefCell;
use std::collections::HashSet;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use arke_core::takeover::{self, Bypass, OnDisk, ShellAttrs, Target};
use parking_lot::Mutex;
use windows::Wdk::System::Threading::{
    NtQueryInformationProcess, ProcessBasicInformation, ProcessCommandLineInformation,
};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HWND, LPARAM, NTSTATUS, UNICODE_STRING, WPARAM,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IConnectionPointContainer, IDispatch_Impl,
    IServiceProvider,
};
use windows::Win32::System::Com::{DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, ITypeInfo};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_STREAM};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_BASIC_INFORMATION, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::{
    DShellWindowsEvents, DShellWindowsEvents_Impl, IFolderView2, IPersistFolder2, IShellBrowser,
    IShellItem, IShellWindows, IWebBrowser2, PathCreateFromUrlAlloc, SHCreateItemFromIDList,
    SID_STopLevelBrowser, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_CREATE, GetClassNameW, GetMessageW, GetShellWindow,
    GetWindowThreadProcessId, IsWindow, KillTimer, MSG, OBJID_WINDOW, PostThreadMessageW, SW_HIDE,
    SW_SHOWNA, SetTimer, ShowWindowAsync, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_NULL, WM_QUIT, WM_TIMER,
};
use windows::core::{GUID, Interface, PCWSTR, PWSTR, implement};

use crate::shell::Shell as _;

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
/// After `WindowRegistered`, how long to keep looking on the fast timer: the event comes before
/// the new window is in the shell's list, and waiting for the slow scan left it on screen for up
/// to two seconds (#285).
const REGISTERED_LOOK: Duration = Duration::from_secs(2);

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
        let (id, flag) = (Arc::clone(&thread_id), Arc::clone(&stopped));
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

/// Receives `WindowRegistered` and only notes it; the message loop does the work. COM calls it
/// from inside `GetMessageW`, which then keeps waiting, so it also posts a message to wake the
/// loop: otherwise the note was only seen at the next slow tick, up to two seconds later, with
/// the Explorer window on screen all that time (#285).
#[allow(
    clippy::ref_as_ptr,
    clippy::inline_always,
    reason = "code generated by #[implement]"
)]
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
        /// The watching thread, woken when a window is registered.
        pub(super) thread: u32,
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
                // SAFETY: posts an empty message to the watching thread's queue.
                let _ = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                        self.thread,
                        windows::Win32::UI::WindowsAndMessaging::WM_NULL,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    )
                };
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
    /// A window was registered and isn't in the list yet: scan on the fast timer until then.
    looking_until: Option<Instant>,
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
        looking_until: None,
    };
    watch.seed();
    let early = start_early(tx, watch.test_root.clone());
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
                watch.look();
                watch.step();
                poll_early();
            }
        } else {
            // SAFETY: msg came from GetMessageW.
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        if registered.swap(false, Ordering::SeqCst) {
            watch.looking_until = Some(Instant::now() + REGISTERED_LOOK);
            watch.look();
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
    drop(early);
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
            registered: Arc::clone(registered),
            // SAFETY: plain Win32 call; this is the watching thread.
            thread: unsafe { GetCurrentThreadId() },
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

/// Whether the window belongs to a process above Arke's integrity level (an elevated File Explorer
/// next to a normal Arke), or one that can't be inspected: not ours to touch. With UAC turned off
/// every program, Arke included, runs at the same high level, and those windows are Arke's (#343).
fn above_us(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    // SAFETY: opens a query-only handle, closed below.
    let theirs = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => {
            let level = integrity_level(process);
            // SAFETY: closes the process handle opened above.
            let _ = unsafe { CloseHandle(process) };
            level
        }
        Err(_) => None,
    };
    // SAFETY: the pseudo handle of this process needs no closing.
    let ours = integrity_level(unsafe { windows::Win32::System::Threading::GetCurrentProcess() });
    leaves_to_explorer(theirs, ours)
}

/// Unknown levels count as above.
const fn leaves_to_explorer(theirs: Option<u32>, ours: Option<u32>) -> bool {
    match (theirs, ours) {
        (Some(theirs), Some(ours)) => theirs > ours,
        _ => true,
    }
}

/// The mandatory integrity level (the RID: 0x2000 medium, 0x3000 high) of `process`'s token.
fn integrity_level(process: windows::Win32::Foundation::HANDLE) -> Option<u32> {
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, TOKEN_MANDATORY_LABEL, TokenIntegrityLevel,
    };
    let mut token = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: process is a valid handle; token is a valid out pointer.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }.ok()?;
    // TOKEN_MANDATORY_LABEL followed by its SID; 64 bytes is plenty, u64 keeps it aligned.
    let mut buf = [0u64; 8];
    let mut len = 0u32;
    // SAFETY: buf and len are valid for the call; token is valid.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            size_of_val(&buf) as u32,
            &raw mut len,
        )
    };
    // SAFETY: closes the token handle opened above.
    let _ = unsafe { CloseHandle(token) };
    ok.ok()?;
    // SAFETY: the call filled buf with a TOKEN_MANDATORY_LABEL whose SID lies within buf; the SID
    // has at least one sub-authority (the level).
    unsafe {
        let label = &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        (count > 0).then(|| *GetSidSubAuthority(sid, u32::from(count) - 1))
    }
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

    /// After `WindowRegistered`: scans until the new window is in the list, or the time is up.
    fn look(&mut self) {
        let Some(until) = self.looking_until else {
            return;
        };
        let found = self.scan().unwrap_or(false);
        if found || Instant::now() >= until {
            self.looking_until = None;
        }
    }

    /// Picks up windows with a handle not seen before; true when there was one. Errors mean the
    /// shell's list is gone.
    fn scan(&mut self) -> windows::core::Result<bool> {
        let shift = shift_down();
        // SAFETY: valid interface pointer.
        unsafe { self.windows.Count() }?;
        // Forget closed windows so a reused handle counts as new again.
        // SAFETY: IsWindow only inspects the handle.
        self.seen
            .retain(|&h| unsafe { IsWindow(Some(HWND(h as *mut _))) }.as_bool());
        let now = Instant::now();
        let mut found = false;
        for (hwnd, browser) in self.list() {
            if self.seen.insert(hwnd) {
                found = true;
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
        Ok(found)
    }

    /// Moves every candidate along; runs on the fast timer and right after a scan.
    fn step(&mut self) {
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.retain_mut(|c| !self.advance(c));
        // advance() never adds candidates; scan() is the only producer.
        self.candidates = candidates;
    }

    /// Returns `true` when the candidate is finished.
    #[allow(clippy::too_many_lines, reason = "one candidate's steps, in order")]
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
                if take_claim(&target) {
                    // The early path ended its launcher just after it had handed the folder to the
                    // shell: Arke shows it already, so only the window goes.
                    // SAFETY: asynchronous post to a window handle; then a valid interface pointer
                    // closes the Explorer window.
                    let closed = unsafe {
                        let _ = ShowWindowAsync(hwnd, SW_HIDE);
                        c.browser.Quit()
                    };
                    log::info!(
                        "takeover: early: Arke already shows {parsing}, closed Explorer's window \
                         {} ms after it appeared ({})",
                        now.duration_since(c.first_seen).as_millis(),
                        closed.map_or_else(|e| format!("Quit failed: {e}"), |()| "closed".into())
                    );
                    return true;
                }
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
        if !takeover::watcher_takes(&target) {
            log::info!("takeover: File Explorer opened at {parsing}, leaving it");
            return None;
        }
        if !takeover::allowed_by_test_root(&target, self.test_root.as_deref()) {
            return None;
        }
        if c.shift || shift_down() {
            log::info!("takeover: Shift held, leaving {parsing} to Explorer");
            return None;
        }
        if above_us(hwnd) {
            log::info!(
                "takeover: window above Arke's integrity level, leaving {parsing} to Explorer"
            );
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

    /// The fast timer runs only while a window is being examined or looked for.
    fn update_timer(&mut self) {
        let busy = !self.candidates.is_empty() || self.looking_until.is_some() || early_busy();
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

/// A process is looked at once; after this long a reused id counts as new again.
const PID_MEMORY: Duration = Duration::from_secs(5);
/// At most this many processes are remembered.
const PID_CAP: usize = 256;
/// Command lines longer than this (bytes) are left to the watcher.
const COMMAND_LINE_MAX: u32 = 0x1_0000;
/// `GetDriveTypeW` answers for drives that answer at once.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_RAMDISK: u32 = 6;

thread_local! {
    /// The early path's state. It lives on the takeover thread, where the hook calls in.
    static EARLY: RefCell<Option<Early>> = const { RefCell::new(None) };
}

/// The early path: File Explorer's launcher ended before it opens a window (#285).
struct Early {
    tx: flume::Sender<OpenRequest>,
    test_root: Option<PathBuf>,
    /// `%SystemRoot%\` in lowercase; File Explorer is `explorer.exe` there (or in `SysWOW64`).
    windows_dir: Vec<u16>,
    /// Arke's integrity level.
    ours: Option<u32>,
    /// Processes already looked at, and when.
    seen: Vec<(u32, Instant)>,
    /// What was opened in Arke, for the window that may come anyway.
    claims: takeover::Claims,
    /// Requests waiting for Arke's answer.
    pending: Vec<Pending>,
}

struct Pending {
    target: Target,
    select: Vec<String>,
    reply: flume::Receiver<bool>,
    since: Instant,
}

/// Unhooks (and drops the state) when the takeover thread ends.
struct EarlyHook(HWINEVENTHOOK);

impl Drop for EarlyHook {
    fn drop(&mut self) {
        // SAFETY: the hook was set on this thread by `start_early`.
        let _ = unsafe { UnhookWinEvent(self.0) };
        EARLY.with(|early| early.borrow_mut().take());
    }
}

/// A process handle, closed on drop.
struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Starts the early path on the takeover thread, unless `ARKE_EARLY_TAKEOVER=0` (for comparing
/// with the watcher alone).
fn start_early(tx: &flume::Sender<OpenRequest>, test_root: Option<PathBuf>) -> Option<EarlyHook> {
    if std::env::var_os("ARKE_EARLY_TAKEOVER").is_some_and(|v| v.to_str() == Some("0")) {
        log::info!("takeover: early path off (ARKE_EARLY_TAKEOVER=0)");
        return None;
    }
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let mut windows_dir: Vec<u16> = root.encode_wide().map(lower).collect();
    while windows_dir.last() == Some(&u16::from(b'\\')) {
        windows_dir.pop();
    }
    windows_dir.push(u16::from(b'\\'));
    // SAFETY: the pseudo handle of this process needs no closing.
    let ours = integrity_level(unsafe { GetCurrentProcess() });
    EARLY.with(|early| {
        *early.borrow_mut() = Some(Early {
            tx: tx.clone(),
            test_root,
            windows_dir,
            ours,
            seen: Vec::new(),
            claims: takeover::Claims::default(),
            pending: Vec::new(),
        });
    });
    // SAFETY: an out-of-context hook: Windows posts the events to this thread, which has a message
    // loop, and calls `on_window_created` from it; nothing is loaded into other processes. Arke's
    // own windows are skipped.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_CREATE,
            None,
            Some(on_window_created),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.is_invalid() {
        log::info!("takeover: cannot watch new windows, early path off");
        EARLY.with(|early| early.borrow_mut().take());
        return None;
    }
    log::info!("takeover: early path on");
    Some(EarlyHook(hook))
}

/// Every window created on the desktop. Cheap for processes already looked at: one id lookup, no
/// allocation or I/O.
///
/// # Safety
///
/// Called by Windows (the hook set in `start_early`) on the takeover thread.
unsafe extern "system" fn on_window_created(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    // Windows themselves (CHILDID_SELF is 0), not their parts.
    if hwnd.0.is_null() || object != OBJID_WINDOW.0 || child != 0 {
        return;
    }
    let mut pid = 0u32;
    // SAFETY: valid out pointer; a window already gone gives 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    if pid == 0 {
        return;
    }
    let opened = EARLY.with(|early| {
        // Busy: called again while the state is in use (COM calls pump messages). The watcher
        // still sees the window.
        let Ok(mut early) = early.try_borrow_mut() else {
            return false;
        };
        early
            .as_mut()
            .is_some_and(|early| early.first_sight(pid, Instant::now()) && early.examine(pid))
    });
    if opened {
        // Wake the message loop so the fast timer waits for Arke's answer (see `sink`).
        // SAFETY: posts an empty message to this thread's queue.
        let _ = unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_NULL, WPARAM(0), LPARAM(0)) };
    }
}

impl Early {
    /// Whether `pid` wasn't looked at recently; notes it.
    fn first_sight(&mut self, pid: u32, now: Instant) -> bool {
        let recent = |at: Instant| now.duration_since(at) < PID_MEMORY;
        if self.seen.iter().any(|&(p, at)| p == pid && recent(at)) {
            return false;
        }
        self.seen.retain(|&(p, at)| p != pid && recent(at));
        if self.seen.len() >= PID_CAP {
            self.seen.remove(0);
        }
        self.seen.push((pid, now));
        true
    }

    /// Looks at a process that made a window; true when its folder went to Arke.
    fn examine(&mut self, pid: u32) -> bool {
        // SAFETY: a query-only handle, closed when `process` drops. Holding it keeps the id from
        // being reused.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let process = Process(handle);
        if !self.is_file_explorer(&process) {
            return false;
        }
        let Some((target, select)) = self.wanted(pid, &process) else {
            return false;
        };
        self.take(pid, &process, target, select)
    }

    fn is_file_explorer(&self, process: &Process) -> bool {
        let mut buf = [0u16; 512];
        let mut len = u32::try_from(buf.len()).unwrap_or(0);
        // SAFETY: `buf` is writable for `len` characters; the handle has query access.
        let named = unsafe {
            QueryFullProcessImageNameW(
                process.0,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &raw mut len,
            )
        };
        named.is_ok() && is_explorer_image(&buf[..len as usize], &self.windows_dir)
    }

    /// What File Explorer's launcher would open, when the watcher would take that window; `None`
    /// leaves it alone.
    fn wanted(&self, pid: u32, process: &Process) -> Option<(Target, Vec<String>)> {
        // The shell itself; with no shell running, File Explorer may be starting as the shell.
        if shell_pid()? == pid {
            return None;
        }
        // "Show in File Explorer": Arke started it.
        if parent_pid(process) == Some(std::process::id()) {
            return None;
        }
        let line = command_line(process)?;
        let args = takeover::command_line_arguments(&line);
        let Some(launch) = takeover::parse_explorer_args(args) else {
            log::debug!("takeover: early: leaving explorer.exe {args}");
            return None;
        };
        let Some(target) = takeover::launch_target(&launch, on_disk) else {
            log::debug!("takeover: early: leaving explorer.exe {args} (not a folder Arke takes)");
            return None;
        };
        if !takeover::watcher_takes(&target)
            || !takeover::allowed_by_test_root(&target, self.test_root.as_deref())
        {
            return None;
        }
        if shift_down() {
            log::info!("takeover: early: Shift held, leaving explorer.exe {args}");
            return None;
        }
        if leaves_to_explorer(integrity_level(process.0), self.ours) {
            log::info!("takeover: early: launcher above Arke's integrity level, leaving it");
            return None;
        }
        // Arke's own request that didn't come from Arke's process; the watcher uses up the mark.
        if let Target::Folder(folder) = &target
            && BYPASS.lock().holds(folder, Instant::now())
        {
            return None;
        }
        Some((target, launch.select.into_iter().collect()))
    }

    /// Ends the launcher and opens its folder in Arke. Ending fails once the launcher is gone (it
    /// handed the folder to the shell): then nothing more happens and the watcher sees the window.
    fn take(&mut self, pid: u32, process: &Process, target: Target, select: Vec<String>) -> bool {
        let age = process_age_ms(process);
        // SAFETY: a terminate-only handle, closed when it drops.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }) else {
            log::info!("takeover: early: cannot end File Explorer's launcher for {target:?}");
            return false;
        };
        let killer = Process(handle);
        // SAFETY: ends `explorer.exe <folder>`, the launcher looked at above (`process` keeps its
        // id from being reused); the shell and every other File Explorer process are left alone.
        if let Err(err) = unsafe { TerminateProcess(killer.0, 1) } {
            log::debug!("takeover: early: launcher gone ({err}), leaving it to the watcher");
            return false;
        }
        let now = Instant::now();
        log::info!(
            "takeover: early: ended File Explorer's launcher {} ms after it started, opening \
             {target:?} in Arke ({} selected)",
            age.map_or_else(|| "?".to_owned(), |ms| ms.to_string()),
            select.len()
        );
        self.claims.add(target.clone(), now);
        let (reply, answer) = flume::bounded(1);
        let request = OpenRequest {
            target: target.clone(),
            select: select.clone(),
            // No Explorer window; Arke comes to the front through the foreground thread.
            explorer_thread: 0,
            reply,
        };
        // Arke shutting down drops the request; `poll` then sees no answer and gives the folder
        // back to File Explorer.
        let _ = self.tx.send(request);
        self.pending.push(Pending {
            target,
            select,
            reply: answer,
            since: now,
        });
        true
    }

    /// Takes Arke's answers; returns what Arke could not open.
    fn poll(&mut self, now: Instant) -> Vec<(Target, Vec<String>)> {
        let mut failed = Vec::new();
        self.pending.retain(|p| match p.reply.try_recv() {
            Ok(true) => false,
            Ok(false) | Err(flume::TryRecvError::Disconnected) => {
                failed.push((p.target.clone(), p.select.clone()));
                false
            }
            Err(flume::TryRecvError::Empty) => {
                let waiting = now.duration_since(p.since) <= OPEN_TIMEOUT;
                if !waiting {
                    log::info!("takeover: early: no answer from Arke for {:?}", p.target);
                }
                waiting
            }
        });
        // Explorer's window for those must not be closed.
        for (target, _) in &failed {
            self.claims.take(target, now);
        }
        failed
    }
}

/// Runs on the fast timer: a folder Arke could not open goes back to File Explorer, since its
/// launcher was ended.
fn poll_early() {
    let failed = EARLY.with(|early| {
        let Ok(mut early) = early.try_borrow_mut() else {
            return Vec::new();
        };
        early
            .as_mut()
            .map_or_else(Vec::new, |early| early.poll(Instant::now()))
    });
    for (target, select) in failed {
        reopen_in_explorer(&target, &select);
    }
}

/// Whether Arke's answer to an early request is awaited (the fast timer keeps running).
fn early_busy() -> bool {
    EARLY.with(|early| {
        early
            .try_borrow()
            .is_ok_and(|early| early.as_ref().is_some_and(|e| !e.pending.is_empty()))
    })
}

/// Whether an Explorer window on `target` is one the early path already opened in Arke.
fn take_claim(target: &Target) -> bool {
    EARLY.with(|early| {
        early.try_borrow_mut().is_ok_and(|mut early| {
            early
                .as_mut()
                .is_some_and(|e| e.claims.take(target, Instant::now()))
        })
    })
}

/// Shows in File Explorer what Arke could not open. Started by Arke, so neither path takes it back
/// (`reveal_in_explorer` notes it for the watcher). A Recycle Bin window goes to the watcher, which
/// asks Arke once more and shows the window if that fails too.
fn reopen_in_explorer(target: &Target, select: &[String]) {
    log::info!("takeover: early: Arke could not open {target:?}, opening it in File Explorer");
    let (path, select) = match target {
        Target::Folder(folder) => match select.first() {
            Some(name) => (folder.join(name), true),
            None => (folder.clone(), false),
        },
        Target::RecycleBin => (PathBuf::from("shell:RecycleBinFolder"), false),
        Target::Home => return,
    };
    if let Err(err) = crate::win::WinShell.reveal_in_explorer(&path, select) {
        log::info!("takeover: early: File Explorer did not start either: {err}");
    }
}

/// The process of the shell's desktop window; `None` when no shell runs.
fn shell_pid() -> Option<u32> {
    // SAFETY: plain Win32 calls; the out pointer is valid.
    unsafe {
        let shell = GetShellWindow();
        if shell.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&raw mut pid));
        (pid != 0).then_some(pid)
    }
}

/// The id of the process that started `process`.
fn parent_pid(process: &Process) -> Option<u32> {
    let mut info = PROCESS_BASIC_INFORMATION {
        ExitStatus: NTSTATUS(0),
        PebBaseAddress: std::ptr::null_mut(),
        AffinityMask: 0,
        BasePriority: 0,
        UniqueProcessId: 0,
        InheritedFromUniqueProcessId: 0,
    };
    // SAFETY: `info` is writable for its size; the handle has query access.
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            ProcessBasicInformation,
            (&raw mut info).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status.0 < 0 {
        return None;
    }
    u32::try_from(info.InheritedFromUniqueProcessId).ok()
}

/// The command line `process` was started with.
fn command_line(process: &Process) -> Option<String> {
    // A UNICODE_STRING followed by the text it points to; u64 keeps it aligned.
    let mut buf = vec![0u64; 512];
    for _ in 0..2 {
        let bytes = u32::try_from(buf.len() * 8).ok()?;
        let mut needed = 0u32;
        // SAFETY: `buf` is writable for `bytes` bytes; the handle has query access.
        let status = unsafe {
            NtQueryInformationProcess(
                process.0,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast(),
                bytes,
                &raw mut needed,
            )
        };
        if status.0 >= 0 {
            // SAFETY: on success `buf` starts with a UNICODE_STRING whose text (`Length` bytes)
            // lies within `buf`, which outlives the copy below.
            let text = unsafe {
                let string = &*buf.as_ptr().cast::<UNICODE_STRING>();
                if string.Buffer.is_null() {
                    return None;
                }
                std::slice::from_raw_parts(string.Buffer.0, usize::from(string.Length) / 2)
            };
            return Some(String::from_utf16_lossy(text));
        }
        if needed <= bytes || needed > COMMAND_LINE_MAX {
            return None;
        }
        buf.resize((needed as usize).div_ceil(8), 0);
    }
    None
}

/// How long ago `process` started, in milliseconds.
fn process_age_ms(process: &Process) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the out pointers are valid; the handle has query access.
    unsafe {
        GetProcessTimes(
            process.0,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .ok()?;
    // SAFETY: plain Win32 call.
    let now = unsafe { GetSystemTimeAsFileTime() };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Some(ticks(now).saturating_sub(ticks(created)) / 10_000)
}

/// What `path` is, looked at only on local drives: a network path could keep the takeover thread
/// (and the launcher) waiting; the watcher handles those.
fn on_disk(path: &Path) -> OnDisk {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return OnDisk::Unknown;
    }
    let root = [u16::from(bytes[0]), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: `root` is NUL-terminated. GetDriveTypeW reads local mount info only.
    let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    if !matches!(kind, DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_RAMDISK) {
        return OnDisk::Unknown;
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => OnDisk::Folder,
        Ok(_) => OnDisk::File,
        Err(_) => OnDisk::Unknown,
    }
}

/// `<windows_dir>explorer.exe` or `<windows_dir>SysWOW64\explorer.exe`, ignoring case;
/// `windows_dir` is lowercase and ends in `\`.
fn is_explorer_image(image: &[u16], windows_dir: &[u16]) -> bool {
    let (Some(dir), Some(rest)) = (
        image.get(..windows_dir.len()),
        image.get(windows_dir.len()..),
    ) else {
        return false;
    };
    dir.iter().zip(windows_dir).all(|(&a, &b)| lower(a) == b)
        && (eq_ignore_case(rest, "explorer.exe") || eq_ignore_case(rest, r"syswow64\explorer.exe"))
}

/// `text` is the lowercase ASCII `ascii`, ignoring case.
fn eq_ignore_case(text: &[u16], ascii: &str) -> bool {
    text.len() == ascii.len()
        && text
            .iter()
            .zip(ascii.bytes())
            .all(|(&c, b)| lower(c) == u16::from(b))
}

/// ASCII lowercase of one UTF-16 unit.
const fn lower(c: u16) -> u16 {
    if matches!(c, 0x41..=0x5A) {
        c + 0x20
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_file_explorer_in_the_windows_folder_counts() {
        let wide = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let dir = wide(r"c:\windows\");
        assert!(is_explorer_image(&wide(r"C:\Windows\explorer.exe"), &dir));
        assert!(is_explorer_image(&wide(r"C:\WINDOWS\EXPLORER.EXE"), &dir));
        assert!(is_explorer_image(
            &wide(r"C:\Windows\SysWOW64\explorer.exe"),
            &dir
        ));
        assert!(!is_explorer_image(&wide(r"C:\Tools\explorer.exe"), &dir));
        assert!(!is_explorer_image(
            &wide(r"C:\Windows\System32\explorer.exe.bak"),
            &dir
        ));
        assert!(!is_explorer_image(&wide(r"C:\Windows\notepad.exe"), &dir));
        assert!(!is_explorer_image(&wide(r"C:\Win"), &dir));
    }

    /// On a thread without the early path (or with it off) nothing is awaited or claimed.
    #[test]
    fn without_the_early_path_nothing_is_claimed() {
        assert!(!early_busy());
        assert!(!take_claim(&Target::RecycleBin));
    }

    /// #343: with UAC turned off File Explorer and Arke run at the same high level, and Arke takes
    /// the window. Only a level above Arke's, or one it can't read, is left to File Explorer.
    #[test]
    fn only_a_level_above_arkes_is_left_to_explorer() {
        assert!(!leaves_to_explorer(Some(0x3000), Some(0x3000)));
        assert!(!leaves_to_explorer(Some(0x2000), Some(0x2000)));
        assert!(!leaves_to_explorer(Some(0x2000), Some(0x3000)));
        assert!(leaves_to_explorer(Some(0x3000), Some(0x2000)));
        assert!(leaves_to_explorer(None, Some(0x2000)));
        // SAFETY: the pseudo handle of this process needs no closing.
        let ours =
            integrity_level(unsafe { windows::Win32::System::Threading::GetCurrentProcess() });
        assert!(ours.is_some_and(|level| level >= 0x1000), "{ours:?}");
    }
}
