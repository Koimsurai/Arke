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
//! (`EVENT_OBJECT_CREATE`) sees that window. Each process is judged once, at its first window, and
//! never again while it runs ([`takeover::Judged`]; a handle kept for each stops its id from being
//! reused meanwhile): the image is checked, and for `<Windows>\explorer.exe` started after the hook
//! was set its command line is read (`core::takeover::parse_explorer_args`, `launch_target`: the
//! same answers `classify` gives for the window). When the watcher would take that window, the
//! launcher is ended at once, before it hands over (ending it up to 200–300 ms in never left a
//! window), and Arke opens the folder in a tab it marks. If it had handed over already, the window
//! still comes; the watcher takes it as usual, and Arke shows it in the marked tab as long as that
//! tab still shows the folder (with the window's selection), instead of a second one. Never touched:
//! the shell itself, `explorer.exe` without arguments, File Explorer processes that were running
//! before the hook or were left alone once (they may show windows themselves), launchers Arke
//! started or gave a folder back to, launchers above Arke's integrity level, and everything the
//! watcher leaves alone. `ARKE_EARLY_TAKEOVER=0` turns it off.
//!
//! Only local drives are looked at on the takeover thread. A network folder (`\\server\share`,
//! `\\wsl$`, a mapped drive) could keep it waiting on the server for longer than the launcher takes
//! to hand over, so it is taken without looking ([`OnDisk::Network`]): the launcher is ended and the
//! request says it is unchecked. Arke then looks at it off the UI thread and opens it only if it is
//! a folder and the request is still awaited; anything else (missing, a file, an unreachable server,
//! credentials needed) is answered `false`, and the folder goes back to File Explorer, which shows
//! its own prompt or error. No answer within [`OPEN_TIMEOUT`] counts as a failure for these, too.
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
    CloseHandle, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, NTSTATUS, UNICODE_STRING,
    WPARAM,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IConnectionPointContainer, IDispatch_Impl,
    IServiceProvider,
};
use windows::Win32::System::Com::{DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, ITypeInfo};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_STREAM};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, GetExitCodeProcess, GetProcessTimes, INFINITE,
    OpenProcess, OpenProcessToken, PROCESS_BASIC_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, RegisterWaitForSingleObject, TerminateProcess, UnregisterWaitEx,
    WT_EXECUTEONLYONCE,
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
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_NULL, WM_QUIT, WM_TIMER,
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
    /// From the early path (#285): File Explorer's launcher was ended, no window to replace. The
    /// tab it opens is marked, and a File Explorer window on the same target that comes anyway
    /// (the launcher had handed it over already) shows in that tab while it still shows it.
    pub(crate) early: bool,
    /// From the early path, on the network: nobody looked whether the folder is there
    /// ([`OnDisk::Network`]). Arke checks it off the UI thread first, answers `false` when it is
    /// not a folder it can list (File Explorer then gets it back), and opens nothing once `reply`
    /// is no longer awaited (no answer in time: File Explorer has it already).
    pub(crate) unchecked: bool,
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
        } else if msg.message == WM_PROCESS_EXITED && msg.hwnd.0.is_null() {
            forget_exited(msg.wParam.0 as u32);
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
                    early: false,
                    unchecked: false,
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

/// `GetExitCodeProcess` for a process still running.
const STILL_ACTIVE: u32 = 259;
/// Thread message to the takeover thread: a judged process ended (`wParam`: its entry's serial).
const WM_PROCESS_EXITED: u32 = WM_APP + 0x285;
/// Command lines longer than this (bytes) are left to the watcher.
const COMMAND_LINE_MAX: u32 = 0x1_0000;
/// `GetDriveTypeW` answers for drives that answer at once.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_RAMDISK: u32 = 6;
/// `GetDriveTypeW` for a mapped network drive: taken without looking.
const DRIVE_REMOTE: u32 = 4;

thread_local! {
    /// The early path's state. It lives on the takeover thread, where the hook calls in.
    static EARLY: RefCell<Option<Early>> = const { RefCell::new(None) };
    /// Processes whose first window came while `EARLY` was in use: noted as left alone at the next
    /// chance, so none is judged at a later window instead of its first.
    static MISSED: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The early path: File Explorer's launcher ended before it opens a window (#285).
struct Early {
    tx: flume::Sender<OpenRequest>,
    test_root: Option<PathBuf>,
    /// `%SystemRoot%\` in lowercase; File Explorer is `explorer.exe` there (or in `SysWOW64`).
    windows_dir: Vec<u16>,
    /// Arke's integrity level.
    ours: Option<u32>,
    /// When the hook was set (FILETIME ticks): File Explorer processes started before are never
    /// ended.
    hooked: u64,
    /// Every running process judged so far, each with its handle and a wait that removes it when
    /// it ends ([`forget_exited`]).
    judged: takeover::Judged<Kept>,
    /// The takeover thread, where the exit waits post to.
    thread: u32,
    /// Serial of the next entry in `judged`, so an exit report finds its own entry only.
    next_serial: u32,
    /// What Arke could not open and gave back to File Explorer: that launcher is Arke's own and is
    /// left alone even when its parent can't be read, or ending it would only loop.
    given_back: takeover::GivenBack,
    /// Requests waiting for Arke's answer.
    pending: Vec<Pending>,
}

struct Pending {
    target: Target,
    select: Vec<String>,
    reply: flume::Receiver<bool>,
    since: Instant,
    /// A network folder nobody looked at ([`OpenRequest::unchecked`]): no answer in time gives it
    /// back to File Explorer instead of leaving it to Arke.
    unchecked: bool,
}

/// Unhooks and drops the state when the takeover thread ends: every exit wait is unregistered
/// (waiting for a running callback) before its handle closes. Reports still in the queue then find
/// nothing.
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

/// What is kept for a judged process. Fields drop in order: the wait is unregistered before the
/// handle it waits on closes.
struct Kept {
    /// Reported by `wait` when the process ends.
    serial: u32,
    /// `None` when it couldn't be registered (no `SYNCHRONIZE` access): the entry then goes when
    /// [`takeover::Judged`] reaches its bound.
    _wait: Option<ExitWait>,
    /// Keeps the id from going to another process while noted. `None`: it couldn't be opened, so
    /// it can't be ended either; dropped when the list reaches its bound.
    process: Option<Process>,
}

/// A thread pool wait on a process handle that posts [`WM_PROCESS_EXITED`] once it ends.
struct ExitWait(HANDLE);

impl Drop for ExitWait {
    fn drop(&mut self) {
        // SAFETY: the wait came from RegisterWaitForSingleObject and is unregistered once.
        // INVALID_HANDLE_VALUE waits for a callback that is running to return (it only posts a
        // message), so the handle can be closed after this. Never called from that callback.
        let _ = unsafe { UnregisterWaitEx(self.0, Some(INVALID_HANDLE_VALUE)) };
    }
}

/// Opens a process to keep while it is noted: with `SYNCHRONIZE` so its end can be waited for,
/// else query-only.
fn open_kept(pid: u32) -> Option<Process> {
    // SAFETY: OpenProcess only returns handles, each closed when its `Process` drops.
    unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
        .or_else(|_| OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid))
    }
    .ok()
    .map(Process)
}

/// Registers a wait on `handle` that posts [`WM_PROCESS_EXITED`] with `serial` to `thread` once,
/// when it is signaled (a process: when it ends).
fn watch_exit(handle: &Process, thread: u32, serial: u32) -> Option<ExitWait> {
    // The thread and the serial travel as the context value; nothing is allocated or shared.
    let context = usize::try_from((u64::from(thread) << 32) | u64::from(serial)).ok()?;
    let mut wait = HANDLE::default();
    // SAFETY: `wait` is a valid out pointer; `handle` stays open until the wait is unregistered
    // (`Kept` drops the wait first). The callback only reads the context value and posts.
    unsafe {
        RegisterWaitForSingleObject(
            &raw mut wait,
            handle.0,
            Some(on_process_exited),
            Some(std::ptr::without_provenance(context)),
            INFINITE,
            WT_EXECUTEONLYONCE,
        )
    }
    .ok()?;
    Some(ExitWait(wait))
}

/// Runs on a thread pool thread when a judged process ends: only tells the takeover thread, which
/// owns the list ([`forget_exited`]).
///
/// # Safety
///
/// Called by Windows for a wait set by [`watch_exit`], with its context value.
unsafe extern "system" fn on_process_exited(context: *mut std::ffi::c_void, _: bool) {
    let value = context.addr() as u64;
    let (thread, serial) = ((value >> 32) as u32, value as u32);
    // SAFETY: posts a message to the takeover thread's queue; fails harmlessly once it is gone.
    let _ = unsafe {
        PostThreadMessageW(
            thread,
            WM_PROCESS_EXITED,
            WPARAM(serial as usize),
            LPARAM(0),
        )
    };
}

/// [`WM_PROCESS_EXITED`] on the takeover thread: the process with that serial ended, so its entry
/// goes now (the wait is unregistered and the handle closed, releasing its id) rather than when
/// the list fills.
fn forget_exited(serial: u32) {
    let gone = EARLY.with(|early| {
        let mut early = early.try_borrow_mut().ok()?;
        early
            .as_mut()?
            .judged
            .forget(|_, kept| kept.serial == serial)
    });
    // Dropped outside the borrow: unregistering waits for the callback to return.
    drop(gone);
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
    // Read before the hook is set: a process started before this is older than the hook. The
    // precise clock, so one started in the same scheduler tick doesn't count as newer.
    // SAFETY: plain Win32 call.
    let hooked = ticks(unsafe { GetSystemTimePreciseAsFileTime() });
    EARLY.with(|early| {
        *early.borrow_mut() = Some(Early {
            tx: tx.clone(),
            test_root,
            windows_dir,
            ours,
            hooked,
            judged: takeover::Judged::default(),
            // SAFETY: plain Win32 call; this is the takeover thread.
            thread: unsafe { GetCurrentThreadId() },
            next_serial: 0,
            given_back: takeover::GivenBack::default(),
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
        let Ok(mut early) = early.try_borrow_mut() else {
            // Busy: called again while the state is in use. Nothing that holds it pumps messages,
            // so this shouldn't happen; if it does, the process is left alone (the watcher still
            // sees its window) rather than judged at a later window.
            MISSED.with(|missed| {
                if let Ok(mut missed) = missed.try_borrow_mut() {
                    missed.push(pid);
                }
            });
            return false;
        };
        early
            .as_mut()
            .is_some_and(|early| !early.judged.contains(pid) && early.examine(hwnd, pid))
    });
    if opened {
        // Wake the message loop so the fast timer waits for Arke's answer (see `sink`).
        // SAFETY: posts an empty message to this thread's queue.
        let _ = unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_NULL, WPARAM(0), LPARAM(0)) };
    }
}

impl Early {
    /// The first window of a process not judged yet: judges it, once, and notes it with its handle
    /// whatever the answer. True when its folder went to Arke.
    fn examine(&mut self, hwnd: HWND, pid: u32) -> bool {
        self.note_missed();
        if self.judged.contains(pid) {
            return false;
        }
        // Kept while the process is noted: that keeps its id from going to another process, so
        // the id alone finds it later.
        let Some(process) = open_kept(pid) else {
            // Can't be looked at, so can't be ended either.
            self.note(pid, None, None);
            return false;
        };
        let (taken, created) = self.judge(hwnd, pid, &process);
        self.note(pid, created, Some(process));
        taken
    }

    /// Notes a judged process, with a wait that removes it the moment it ends. When the list is
    /// full (ends nobody heard of), those that ended are dropped first.
    fn note(&mut self, pid: u32, created: Option<u64>, process: Option<Process>) {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        let wait = process
            .as_ref()
            .and_then(|p| watch_exit(p, self.thread, serial));
        let kept = Kept {
            serial,
            _wait: wait,
            process,
        };
        let key = takeover::ProcessKey { pid, created };
        self.judged.note(key, kept, |_, kept| {
            kept.process.as_ref().is_none_or(exited)
        });
    }

    /// Notes the processes whose first window came while the state was in use, as left alone.
    fn note_missed(&mut self) {
        let missed = MISSED.with(|missed| {
            missed
                .try_borrow_mut()
                .map_or_else(|_| Vec::new(), |mut missed| std::mem::take(&mut *missed))
        });
        for pid in missed {
            if !self.judged.contains(pid) {
                let process = open_kept(pid);
                self.note(pid, None, process);
            }
        }
    }

    /// Whether to end the process that made window `hwnd`, and does so. Also gives when a File
    /// Explorer process started (read for those only).
    fn judge(&mut self, hwnd: HWND, pid: u32, process: &Process) -> (bool, Option<u64>) {
        // The process may have ended between the event and `OpenProcess`, and its id gone to a new
        // one. A window ends with its process, so if the window still names `pid`, the handle is
        // its process; otherwise it is left alone.
        let mut owner = 0u32;
        // SAFETY: valid out pointer; a window already gone gives 0.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut owner)) };
        if owner != pid || !self.is_file_explorer(process) {
            return (false, None);
        }
        let Some(created) = process_created(process) else {
            return (false, None);
        };
        if !takeover::started_after(created, self.hooked) {
            log::debug!("takeover: early: explorer.exe {pid} predates the hook, leaving it");
            return (false, Some(created));
        }
        let Some((launched, select)) = self.wanted(pid, process) else {
            return (false, Some(created));
        };
        (self.take(pid, created, launched, select), Some(created))
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
    /// leaves it alone. Only cheap checks come before the command line is read, nothing touches
    /// the disk before the arguments are understood, and network folders aren't touched at all.
    fn wanted(&mut self, pid: u32, process: &Process) -> Option<(takeover::Launched, Vec<String>)> {
        // The shell itself; with no shell running, File Explorer may be starting as the shell.
        if shell_pid()? == pid {
            return None;
        }
        let ours = parent_pid(process) == Some(std::process::id());
        let line = command_line(process)?;
        let args = takeover::command_line_arguments(&line);
        let Some(launch) = takeover::parse_explorer_args(args) else {
            log::debug!("takeover: early: leaving explorer.exe {args}");
            return None;
        };
        if shift_down() {
            log::info!("takeover: early: Shift held, leaving explorer.exe {args}");
            return None;
        }
        if leaves_to_explorer(integrity_level(process.0), self.ours) {
            log::info!("takeover: early: launcher above Arke's integrity level, leaving it");
            return None;
        }
        let Some(launched) = takeover::launch_target(&launch, on_disk) else {
            log::debug!("takeover: early: leaving explorer.exe {args} (not a folder Arke takes)");
            return None;
        };
        let target = &launched.target;
        if !takeover::watcher_takes(target)
            || !takeover::allowed_by_test_root(target, self.test_root.as_deref())
        {
            return None;
        }
        // Before the parent check, so Arke's own launcher uses the mark up.
        if self.given_back.take(target) {
            log::info!("takeover: early: Arke gave {target:?} back to File Explorer, leaving it");
            return None;
        }
        // "Show in File Explorer": Arke started it.
        if ours {
            return None;
        }
        // Arke's own request that didn't come from Arke's process; the watcher uses up the mark.
        if let Target::Folder(folder) = target
            && BYPASS.lock().holds(folder, Instant::now())
        {
            return None;
        }
        Some((launched, launch.select.into_iter().collect()))
    }

    /// Ends the launcher and opens its folder in Arke, as soon as it is seen. Ending fails once the
    /// launcher is gone: then nothing more happens and the watcher sees the window. If it had
    /// handed the folder to the shell already, the window comes anyway and the watcher takes it as
    /// usual; Arke shows it in the tab opened here, if that tab still shows it (`main.rs`
    /// `open_taken_over`). The caller holds a handle to the launcher, so `pid` is still its id.
    fn take(
        &mut self,
        pid: u32,
        created: u64,
        launched: takeover::Launched,
        select: Vec<String>,
    ) -> bool {
        let takeover::Launched { target, unchecked } = launched;
        // SAFETY: a terminate-only handle, closed when it drops.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }) else {
            log::info!("takeover: early: cannot end File Explorer's launcher for {target:?}");
            return false;
        };
        let killer = Process(handle);
        // SAFETY: ends `explorer.exe <folder>`, the launcher looked at above (the caller's handle
        // keeps its id from being reused); the shell and every other File Explorer process are left
        // alone. Exit code 1 is what File Explorer's launcher returns on its own, too.
        if let Err(err) = unsafe { TerminateProcess(killer.0, 1) } {
            log::debug!("takeover: early: launcher gone ({err}), leaving it to the watcher");
            return false;
        }
        // Only for the log (measuring how much room is left): nothing depends on it.
        // SAFETY: plain Win32 call.
        let now_ticks = ticks(unsafe { GetSystemTimePreciseAsFileTime() });
        log::info!(
            "takeover: early: ended File Explorer's launcher {} ms after it started, opening \
             {target:?} in Arke ({} selected{})",
            now_ticks.saturating_sub(created) / 10_000,
            select.len(),
            if unchecked {
                ", on the network: Arke checks it first"
            } else {
                ""
            }
        );
        let (reply, answer) = flume::bounded(1);
        let request = OpenRequest {
            target: target.clone(),
            select: select.clone(),
            // No Explorer window; Arke comes to the front through the foreground thread.
            explorer_thread: 0,
            early: true,
            unchecked,
            reply,
        };
        // Arke shutting down drops the request; `poll` then sees no answer and gives the folder
        // back to File Explorer.
        let _ = self.tx.send(request);
        self.pending.push(Pending {
            target,
            select,
            reply: answer,
            since: Instant::now(),
            unchecked,
        });
        true
    }

    /// Takes Arke's answers; returns what Arke could not open. A request with no answer in time is
    /// dropped (Arke is only slow to open a local folder), unless it is an unchecked network
    /// folder: that goes back to File Explorer, and dropping its receiver tells Arke not to open
    /// it any more.
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
                if !waiting && p.unchecked {
                    log::info!(
                        "takeover: early: no answer from Arke in time for network folder {:?}, \
                         giving it back to File Explorer",
                        p.target
                    );
                    failed.push((p.target.clone(), p.select.clone()));
                } else if !waiting {
                    log::info!("takeover: early: no answer from Arke for {:?}", p.target);
                }
                waiting
            }
        });
        for (target, _) in &failed {
            // The launcher that reopens them must not be ended.
            self.given_back.add(target.clone());
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
            // SAFETY: on success `buf` starts with a UNICODE_STRING (it is large and aligned
            // enough); its text is only read after checking that it lies within `buf`, which
            // outlives the copy below.
            let text = unsafe {
                let string = &*buf.as_ptr().cast::<UNICODE_STRING>();
                let start = buf.as_ptr().addr() + size_of::<UNICODE_STRING>();
                let end = buf.as_ptr().addr() + buf.len() * 8;
                let text = string.Buffer.0.addr();
                let len = usize::from(string.Length);
                if string.Buffer.is_null()
                    || text < start
                    || text.saturating_add(len) > end
                    || !text.is_multiple_of(2)
                {
                    return None;
                }
                std::slice::from_raw_parts(string.Buffer.0, len / 2)
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

/// When `process` started, in FILETIME ticks (100 ns since 1601, UTC).
fn process_created(process: &Process) -> Option<u64> {
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
    Some(ticks(created))
}

/// A FILETIME as one number.
fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Whether the process `process` names has ended (its id can be reused once the handle closes).
fn exited(process: &Process) -> bool {
    let mut code = 0u32;
    // SAFETY: valid out pointer; the handle has query access.
    let read = unsafe { GetExitCodeProcess(process.0, &raw mut code) };
    // A process that ended with exit code 259 counts as running: kept a little longer, harmless.
    read.is_err() || code != STILL_ACTIVE
}

/// What `path` is, looked at only on local drives. A network path is not touched at all
/// ([`OnDisk::Network`]): its server could keep the takeover thread (and the launcher) waiting, so
/// Arke checks it after the launcher is ended. Other drives (CD drives, unassigned letters) are
/// [`OnDisk::Unknown`] and left to the watcher.
fn on_disk(path: &Path) -> OnDisk {
    match how_to_look(path, drive_type) {
        Look::Disk => match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => OnDisk::Folder,
            Ok(_) => OnDisk::File,
            Err(_) => OnDisk::Unknown,
        },
        Look::Network => OnDisk::Network,
        Look::Skip => OnDisk::Unknown,
    }
}

/// How [`on_disk`] treats a path, from its text and its drive's type alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    /// A local drive that answers at once: looked at.
    Disk,
    /// A share or a mapped network drive: taken without looking.
    Network,
    /// Neither: not looked at, not taken.
    Skip,
}

/// [`Look`] for `path`; `drive_type` gives `GetDriveTypeW`'s answer for a drive letter (asked only
/// for `X:\...` paths).
fn how_to_look(path: &Path, drive_type: impl FnOnce(u8) -> u32) -> Look {
    let bytes = path.as_os_str().as_encoded_bytes();
    if let Some(rest) = bytes.strip_prefix(br"\\") {
        // `\\?\` and `\\.\` are device paths, not shares (`classify` refuses them anyway).
        return if rest.starts_with(b"?") || rest.starts_with(b".") {
            Look::Skip
        } else {
            Look::Network
        };
    }
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return Look::Skip;
    }
    match drive_type(bytes[0]) {
        DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_RAMDISK => Look::Disk,
        DRIVE_REMOTE => Look::Network,
        _ => Look::Skip,
    }
}

/// `GetDriveTypeW` for the drive `letter`.
fn drive_type(letter: u8) -> u32 {
    let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: `root` is NUL-terminated. GetDriveTypeW reads local mount info only.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }
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

    fn early() -> (Early, flume::Receiver<OpenRequest>) {
        let (tx, rx) = flume::unbounded();
        let early = Early {
            tx,
            test_root: None,
            windows_dir: Vec::new(),
            ours: None,
            hooked: 0,
            judged: takeover::Judged::default(),
            thread: 0,
            next_serial: 0,
            given_back: takeover::GivenBack::default(),
            pending: Vec::new(),
        };
        (early, rx)
    }

    fn pending(target: &Target, since: Instant) -> (Pending, flume::Sender<bool>) {
        let (reply, answer) = flume::bounded(1);
        let pending = Pending {
            target: target.clone(),
            select: vec!["a.txt".to_owned()],
            reply: answer,
            since,
            unchecked: false,
        };
        (pending, reply)
    }

    /// A handle to Arke's own process, which stands in for one the hook saw.
    fn own_process() -> Process {
        // SAFETY: a query-only handle to this process, closed when it drops.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, std::process::id()) };
        Process(handle.expect("this process can be opened"))
    }

    #[test]
    fn what_arke_could_not_open_goes_back_to_explorer_once() {
        let (mut early, _rx) = early();
        let t0 = Instant::now();
        let target = Target::Folder(PathBuf::from(r"C:\Work"));
        let (p, reply) = pending(&target, t0);
        early.pending.push(p);
        assert!(early.poll(t0).is_empty(), "no answer yet");
        assert!(reply.send(false).is_ok());
        assert_eq!(
            early.poll(t0),
            vec![(target.clone(), vec!["a.txt".to_owned()])]
        );
        assert!(early.pending.is_empty());
        assert!(
            early.given_back.take(&target),
            "the launcher that reopens it is left alone"
        );
        assert!(!early.given_back.take(&target), "once");
    }

    #[test]
    fn opened_or_unanswered_requests_are_not_given_back() {
        let (mut early, _rx) = early();
        let t0 = Instant::now();
        let opened = Target::RecycleBin;
        let silent = Target::Folder(PathBuf::from(r"C:\Slow"));
        let gone = Target::Folder(PathBuf::from(r"C:\Gone"));
        let (p, opened_reply) = pending(&opened, t0);
        early.pending.push(p);
        let (p, _silent_reply) = pending(&silent, t0);
        early.pending.push(p);
        let (p, gone_reply) = pending(&gone, t0);
        early.pending.push(p);
        assert!(opened_reply.send(true).is_ok());
        drop(gone_reply);
        let failed = early.poll(t0 + OPEN_TIMEOUT + Duration::from_millis(1));
        assert_eq!(failed, vec![(gone.clone(), vec!["a.txt".to_owned()])]);
        assert!(early.pending.is_empty());
        assert!(!early.given_back.take(&opened));
        assert!(!early.given_back.take(&silent));
        assert!(early.given_back.take(&gone), "Arke dropped it: given back");
    }

    /// #285: a network folder nobody looked at that Arke doesn't answer in time goes back to File
    /// Explorer (a sleeping server is then File Explorer's to wait on); a local one is only
    /// dropped, as before. Dropping the pending request tells Arke not to open it any more.
    #[test]
    fn an_unchecked_request_without_an_answer_in_time_is_given_back() {
        let (mut early, _rx) = early();
        let t0 = Instant::now();
        let network = Target::Folder(PathBuf::from(r"\\nas\share\pic"));
        let local = Target::Folder(PathBuf::from(r"C:\Slow"));
        let (mut p, network_reply) = pending(&network, t0);
        p.unchecked = true;
        early.pending.push(p);
        let (p, local_reply) = pending(&local, t0);
        early.pending.push(p);
        assert!(early.poll(t0 + OPEN_TIMEOUT).is_empty(), "still waiting");
        let failed = early.poll(t0 + OPEN_TIMEOUT + Duration::from_millis(1));
        assert_eq!(failed, vec![(network.clone(), vec!["a.txt".to_owned()])]);
        assert!(early.pending.is_empty());
        assert!(early.given_back.take(&network), "given back");
        assert!(!early.given_back.take(&local), "only dropped");
        assert!(network_reply.is_disconnected(), "Arke sees nobody waits");
        assert!(local_reply.is_disconnected());
    }

    /// #285: shares and mapped network drives are taken without looking; local drives are looked
    /// at; device paths, CD drives and unassigned letters are neither.
    #[test]
    fn only_local_drives_are_looked_at() {
        let look = |path: &str, kind: u32| {
            how_to_look(Path::new(path), |letter| {
                assert!(letter.is_ascii_alphabetic(), "{path}");
                kind
            })
        };
        let no_drive = |path: &str| {
            how_to_look(Path::new(path), |_| -> u32 {
                panic!("{path}: no drive type for a share")
            })
        };
        assert_eq!(no_drive(r"\\nas\share"), Look::Network);
        assert_eq!(no_drive(r"\\192.168.0.56\pic\2026"), Look::Network);
        assert_eq!(no_drive(r"\\wsl$\Ubuntu\home"), Look::Network);
        assert_eq!(no_drive(r"\\wsl.localhost\Ubuntu"), Look::Network);
        assert_eq!(no_drive(r"\\?\C:\dir"), Look::Skip);
        assert_eq!(no_drive(r"\\.\PhysicalDrive0"), Look::Skip);
        assert_eq!(look(r"Z:\share", DRIVE_REMOTE), Look::Network);
        assert_eq!(look(r"Z:\", DRIVE_REMOTE), Look::Network);
        for kind in [DRIVE_FIXED, DRIVE_REMOVABLE, DRIVE_RAMDISK] {
            assert_eq!(look(r"C:\dir", kind), Look::Disk, "{kind}");
        }
        // DRIVE_UNKNOWN, DRIVE_NO_ROOT_DIR, DRIVE_CDROM.
        for kind in [0, 1, 5] {
            assert_eq!(look(r"E:\dir", kind), Look::Skip, "{kind}");
        }
        assert_eq!(no_drive(r"C:dir"), Look::Skip);
        assert_eq!(no_drive("dir"), Look::Skip);
    }

    /// A process whose first window came while the state was in use is noted as left alone, and
    /// a running process stays noted however many others come and go.
    #[test]
    fn a_missed_process_is_left_alone_for_good() {
        let (mut early, _rx) = early();
        let me = std::process::id();
        MISSED.with(|missed| missed.borrow_mut().push(me));
        early.note_missed();
        assert!(early.judged.contains(me));
        early.note_missed();
        assert_eq!(early.judged.len(), 1, "noted once");
        // Processes that couldn't be opened (no handle) are dropped when the list is pruned; this
        // one runs, and its handle is open, so it stays.
        let bound = u32::try_from(takeover::JUDGED_BOUND).unwrap_or(64);
        for pid in 0..3 * bound {
            early.note(0x7FFF_0001 + 2 * pid, None, None);
        }
        assert!(early.judged.contains(me));
        assert!(
            early.judged.len() < usize::try_from(3 * bound).unwrap_or(usize::MAX),
            "{}",
            early.judged.len()
        );
    }

    /// Arke's own process started before any hook it sets: such a File Explorer process is never
    /// ended. Its creation time is read from the same clock as the hook's.
    #[test]
    fn a_process_running_before_the_hook_is_older_than_it() {
        let process = own_process();
        let created = process_created(&process).expect("its start time can be read");
        // SAFETY: plain Win32 call.
        let hooked = ticks(unsafe { GetSystemTimePreciseAsFileTime() });
        assert!(!takeover::started_after(created, hooked));
        assert!(!exited(&process), "it runs");
    }

    /// A process is removed when its end is reported, by its own entry's serial: a report for an
    /// entry that is gone already removes nothing.
    #[test]
    fn an_ended_process_goes_when_its_report_comes() {
        let (mut early, _rx) = early();
        let me = std::process::id();
        early.note(me, None, open_kept(me));
        let serial = early.next_serial.wrapping_sub(1);
        let other = 0x7FFF_0001;
        early.note(other, None, None);
        EARLY.with(|state| *state.borrow_mut() = Some(early));
        let noted = |pid: u32| {
            EARLY.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .is_some_and(|e| e.judged.contains(pid))
            })
        };
        forget_exited(serial.wrapping_add(100));
        assert!(noted(me) && noted(other), "a stale report removes nothing");
        forget_exited(serial);
        assert!(!noted(me), "gone on its report, not when the list fills");
        assert!(noted(other));
        EARLY.with(|state| state.borrow_mut().take());
    }

    /// The exit wait posts its serial to the thread that registered it, once the handle is
    /// signaled. An event stands in for a process: both are waitable handles.
    #[test]
    fn the_exit_wait_posts_its_serial() {
        use windows::Win32::System::Threading::{CreateEventW, SetEvent};
        use windows::Win32::UI::WindowsAndMessaging::{PM_NOREMOVE, PM_REMOVE, PeekMessageW};
        let mut msg = MSG::default();
        // SAFETY: valid out pointer; creates this thread's message queue.
        let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
        // SAFETY: an unnamed manual-reset event, closed when `event` drops.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) };
        let event = Process(event.expect("an event"));
        // SAFETY: plain Win32 call.
        let thread = unsafe { GetCurrentThreadId() };
        let wait = watch_exit(&event, thread, 7).expect("the wait is registered");
        // SAFETY: `event` is a valid event handle.
        assert!(unsafe { SetEvent(event.0) }.is_ok());
        let deadline = Instant::now() + Duration::from_secs(10);
        let serial = loop {
            // SAFETY: valid out pointer; takes only the exit report.
            let got = unsafe {
                PeekMessageW(
                    &raw mut msg,
                    None,
                    WM_PROCESS_EXITED,
                    WM_PROCESS_EXITED,
                    PM_REMOVE,
                )
            };
            if got.as_bool() {
                break msg.wParam.0;
            }
            assert!(Instant::now() < deadline, "no report");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(serial, 7);
        // The wait goes before the handle it waited on.
        drop(wait);
        drop(event);
    }

    /// On a thread without the early path (or with it off) nothing is awaited.
    #[test]
    fn without_the_early_path_nothing_is_awaited() {
        assert!(!early_busy());
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
