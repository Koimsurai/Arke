//! "More options": reads the Windows context menu, including third-party shell extensions,
//! so Arke can draw it itself.
//!
//! Runs only inside the menu host (`arke.exe --shell-host`, see `shell_host.rs`), because
//! reading the menu loads extensions that must stay out of Arke's process. The menu is built
//! with `IContextMenu::QueryContextMenu` but never shown; each item's text, icon, submenu and
//! state is read and sent back. A chosen item is run with `InvokeCommand` on the same
//! `IContextMenu`.
//!
//! Everything runs on one long-lived STA thread that keeps pumping messages: some extensions
//! continue via messages after `InvokeCommand` returns (e.g. Send to > Compressed folder) and
//! break if the thread exits.
//!
//! A submenu still empty once asked to fill itself is sent empty with a number ([`Lazy`]) and
//! filled when the user opens it ([`fill_submenu`]), the way File Explorer's menu asks for it on
//! opening; the read doesn't wait for it.
//!
//! Only one menu (`Session`) is kept at a time; it is released on the next query, after
//! invoking, or when the menu closes without a choice.
//!
//! A single extension's menu ([`query_handler`], see `win_menu_parts.rs`) gets a thread of its
//! own, with the same loop, so several load at once. It keeps that thread until it is released
//! (the thread then ends) or invoked (the thread stays, for extensions that go on through
//! messages, until the process exits).

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, HBITMAP};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSIDFromString, CoCreateInstance, IDataObject,
};
use windows::Win32::System::Ole::{
    IObjectWithSite, OleFlushClipboard, OleInitialize, OleUninitialize,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO,
    CMINVOKECOMMANDINFOEX, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, ILClone,
    ILFindLastID, ILFree, ILRemoveLastID, IShellExtInit, IShellFolder, SHBindToParent,
    SHGetDesktopFolder, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetMenuItemCount, GetMenuItemInfoW, HMENU, IsMenu, MENUITEMINFOW, MFS_CHECKED, MFS_DEFAULT,
    MFS_DISABLED, MFT_BITMAP, MFT_SEPARATOR, MIIM_BITMAP, MIIM_CHECKMARKS, MIIM_FTYPE, MIIM_ID,
    MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
    PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW, SW_SHOWNORMAL, TranslateMessage,
    WINDOW_EX_STYLE, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
    WS_POPUP,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR, w};

use crate::shell::{ShellError, ShellMenu, ShellMenuEntry, tidy_separators};
use crate::win_fileops::wide;
use crate::win_menu_parts::{HandlerKind, MenuHandler};

/// Registry and packaged-app commands read on their own (#278).
#[path = "win_menu_single.rs"]
mod one;

const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
/// Submenu depth and total item limits; some extensions add very long lists.
const MAX_DEPTH: usize = 4;
const MAX_ITEMS: usize = 400;
/// Retries, 30 ms apart, while a submenu is still being filled.
const INIT_TRIES: u32 = 50;
/// Retries for a submenu filled as it opens that still has no items at all, not even a
/// placeholder: about 300 ms, since a command picked meanwhile waits behind it.
const EMPTY_TRIES: u32 = 10;

enum Request {
    Query {
        paths: Vec<PathBuf>,
        background: bool,
        extended: bool,
        done: flume::Sender<Result<ShellMenu, ShellError>>,
    },
    /// One extension's menu (on a thread of its own).
    QueryHandler {
        paths: Vec<PathBuf>,
        background: bool,
        extended: bool,
        handler: MenuHandler,
        done: flume::Sender<Result<ShellMenu, ShellError>>,
    },
    Invoke {
        session: u64,
        id: u32,
        /// Physical pixels in the Arke window's client area.
        client_point: (i32, i32),
        owner: Option<isize>,
        done: flume::Sender<Result<(), ShellError>>,
    },
    Release(u64),
    /// A submenu that was empty when the menu was read ([`Lazy`]): fill it now and read it.
    FillSubmenu {
        session: u64,
        index: u32,
        done: flume::Sender<Result<Vec<ShellMenuEntry>, ShellError>>,
    },
    /// Process is exiting: hand clipboard data set by extensions (e.g. Copy) to the system.
    FlushClipboard(flume::Sender<()>),
}

/// A submenu still empty when the menu was read. An extension may fill its submenu only when it
/// is really opened, so it is kept and asked again when the user opens it in Arke
/// ([`fill_submenu`]), the way File Explorer's menu asks for it on opening.
#[derive(Clone, Copy)]
struct Lazy {
    hmenu: HMENU,
    /// Position in its parent, for `WM_INITMENUPOPUP`.
    pos: u32,
    depth: usize,
}

/// A menu waiting for a choice. Frees the HMENU and PIDLs on drop.
struct Session {
    id: u64,
    menu: IContextMenu,
    hmenu: HMENU,
    owner: HWND,
    _pidls: Pidls,
    /// The folder the menu is for (the folder itself for its background, else the items'
    /// folder), NUL-terminated: passed when invoking, like File Explorer does. A command
    /// registered only for folder backgrounds (`Directory\Background\shell`, e.g. Everything's
    /// "Search Everything...") fails without it, as if nothing could open the folder (#184).
    directory: Vec<u16>,
    /// Submenus to fill when opened, by the index sent with the menu.
    lazy: Vec<Lazy>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The stand-in view given to an empty-space menu (`set_background_site`) goes first, as
        // File Explorer takes its site back before letting go of a menu.
        if let Ok(with_site) = self.menu.cast::<IObjectWithSite>() {
            // SAFETY: plain COM call on the thread that made the menu.
            let _ = unsafe { with_site.SetSite(None) };
        }
        // SAFETY: hmenu was created by CreatePopupMenu and not yet destroyed.
        let _ = unsafe { DestroyMenu(self.hmenu) };
    }
}

thread_local! {
    /// Current menu; menu messages (WM_INITMENUPOPUP etc.) are forwarded to it.
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// This menu thread's requests: its queue, and the ones taken off it but not handled yet.
    static INBOX: RefCell<Inbox> = RefCell::new(Inbox::default());
}

/// Requests waiting on a menu thread. Fills of submenus (`FillSubmenu`) go last: the pointer may
/// open several in a row, each taking a while, and the command the user then picks (or closing
/// the menu) shouldn't wait behind them.
#[derive(Default)]
struct Inbox {
    rx: Option<flume::Receiver<Request>>,
    waiting: std::collections::VecDeque<Request>,
}

/// What the menu thread does next.
enum Next {
    Handle(Request),
    /// Nothing to do: wait for a request or a message.
    Idle,
    /// The queue was dropped: the thread is done.
    Gone,
}

impl Inbox {
    /// Takes everything sent so far; whether the senders are gone.
    fn collect(&mut self) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return true;
        };
        loop {
            match rx.try_recv() {
                Ok(request) => self.waiting.push_back(request),
                Err(flume::TryRecvError::Empty) => return false,
                Err(flume::TryRecvError::Disconnected) => return true,
            }
        }
    }

    fn next(&mut self) -> Next {
        let gone = self.collect();
        match take_next(&mut self.waiting) {
            Some(request) => Next::Handle(request),
            None if gone => Next::Gone,
            None => Next::Idle,
        }
    }

    /// Whether a request other than a fill is waiting (a fill under way then stops waiting for
    /// its extension).
    fn urgent(&mut self) -> bool {
        self.collect();
        self.waiting.iter().any(|r| !r.is_fill())
    }
}

/// The request to handle next: the oldest one that isn't a fill, else the oldest fill.
fn take_next(waiting: &mut std::collections::VecDeque<Request>) -> Option<Request> {
    let at = waiting.iter().position(|r| !r.is_fill()).unwrap_or(0);
    waiting.remove(at)
}

impl Request {
    const fn is_fill(&self) -> bool {
        matches!(self, Self::FillSubmenu { .. })
    }
}

/// Whether this menu thread has a request waiting that a fill should give way to.
fn urgent_waiting() -> bool {
    INBOX.with(|inbox| inbox.try_borrow_mut().is_ok_and(|mut inbox| inbox.urgent()))
}

/// The menu thread's request queue, and the event that wakes it for each request.
struct MenuThread {
    tx: flume::Sender<Request>,
    /// Auto-reset event, set after every request. The thread sleeps on it and on its message
    /// queue together, so it wakes only when there is something to do.
    wake: isize,
}

impl MenuThread {
    fn send(&self, request: Request) -> Result<(), ShellError> {
        self.tx
            .send(request)
            .map_err(|_hung_up| ShellError::MenuThreadGone)?;
        // SAFETY: the event was created with the thread and is never closed.
        let _ = unsafe { SetEvent(HANDLE(self.wake as *mut c_void)) };
        Ok(())
    }
}

impl MenuThread {
    /// Starts a menu thread. A `part` thread holds one extension's menu and ends once that is
    /// released; it closes its event then, so nothing may be sent to it after the release.
    fn spawn(name: &str, part: bool) -> Option<Self> {
        // SAFETY: an unnamed auto-reset event; the whole-menu thread keeps it for the life of
        // the process, a part thread closes it when it ends.
        let wake = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.ok()?;
        let wake = wake.0 as isize;
        let (tx, rx) = flume::unbounded::<Request>();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                run(&rx, wake, part);
                if part {
                    // SAFETY: created above; the sender side was dropped before the release
                    // that ended the loop, so nothing sets it any more.
                    let _ = unsafe { CloseHandle(HANDLE(wake as *mut c_void)) };
                }
            })
            .ok()?;
        Some(Self { tx, wake })
    }
}

fn menu_thread() -> Option<&'static MenuThread> {
    static THREAD: OnceLock<Option<MenuThread>> = OnceLock::new();
    THREAD
        .get_or_init(|| MenuThread::spawn("shell-menu", false))
        .as_ref()
}

/// A single extension's menu, waiting on its own thread.
struct Part {
    session: u64,
    /// Arke's menu it belongs to; a newer one releases it.
    group: u64,
    thread: MenuThread,
}

static PARTS: parking_lot::Mutex<Vec<Part>> = parking_lot::Mutex::new(Vec::new());
/// Newest menu that asked for single extensions.
static LATEST_GROUP: AtomicU64 = AtomicU64::new(0);

/// Reads one extension's menu for `paths` on a thread of its own, so several load at once.
/// `group` is Arke's menu asking; the parts of older menus are released. Blocks, so call from a
/// background thread.
pub(crate) fn query_handler(
    paths: &[PathBuf],
    background: bool,
    extended: bool,
    handler: &MenuHandler,
    group: u64,
) -> Result<ShellMenu, ShellError> {
    LATEST_GROUP.fetch_max(group, Ordering::Relaxed);
    release_parts_before(group);
    let thread = MenuThread::spawn("shell-menu-part", true).ok_or(ShellError::MenuThreadStart)?;
    let (done, result) = flume::bounded(1);
    thread.send(Request::QueryHandler {
        paths: paths.to_vec(),
        background,
        extended,
        handler: handler.clone(),
        done,
    })?;
    let menu = result
        .recv()
        .map_err(|_hung_up| ShellError::MenuThreadNoReply)?;
    match menu {
        Ok(menu) if LATEST_GROUP.load(Ordering::Relaxed) <= group => {
            PARTS.lock().push(Part {
                session: menu.session,
                group,
                thread,
            });
            Ok(menu)
        }
        other => {
            // Failed, or Arke has moved on to another menu meanwhile: end the thread.
            let _ = thread.send(Request::Release(0));
            other.and(Err(ShellError::MenuExpired))
        }
    }
}

/// Reads "Send to" once for `path`, so its label, targets and icons are known before the first
/// menu asks (the first read in a process takes about 0.3 s, later ones a few ms).
pub(crate) fn warm_send_to(path: &Path) {
    if let Ok(menu) = query_handler(
        &[path.to_path_buf()],
        false,
        false,
        &MenuHandler::send_to(),
        0,
    ) && let Some(part) = take_part(menu.session)
    {
        let _ = part.thread.send(Request::Release(part.session));
    }
}

fn release_parts_before(group: u64) {
    let old: Vec<Part> = {
        let mut parts = PARTS.lock();
        let (old, keep) = std::mem::take(&mut *parts)
            .into_iter()
            .partition(|p| p.group < group);
        *parts = keep;
        old
    };
    for part in old {
        let _ = part.thread.send(Request::Release(part.session));
    }
}

/// The thread holding single-extension menu `session`, taken out of the list.
fn take_part(session: u64) -> Option<Part> {
    let mut parts = PARTS.lock();
    let at = parts.iter().position(|p| p.session == session)?;
    Some(parts.remove(at))
}

/// Reads the context menu for `paths`. Blocks on the STA thread, so call from a background thread.
pub(crate) fn query(
    paths: &[PathBuf],
    background: bool,
    extended: bool,
) -> Result<ShellMenu, ShellError> {
    let thread = menu_thread().ok_or(ShellError::MenuThreadStart)?;
    let (done, result) = flume::bounded(1);
    thread.send(Request::Query {
        paths: paths.to_vec(),
        background,
        extended,
        done,
    })?;
    result
        .recv()
        .map_err(|_hung_up| ShellError::MenuThreadNoReply)?
}

/// Invokes a menu item. Commands may open windows or run long, so call from a background thread.
pub(crate) fn invoke(
    session: u64,
    id: u32,
    client_point: (i32, i32),
    owner: Option<isize>,
) -> Result<(), ShellError> {
    let part = take_part(session);
    let thread = match &part {
        Some(part) => &part.thread,
        None => menu_thread().ok_or(ShellError::MenuThreadStart)?,
    };
    let (done, result) = flume::bounded(1);
    thread.send(Request::Invoke {
        session,
        id,
        client_point,
        owner,
        done,
    })?;
    result
        .recv()
        .map_err(|_hung_up| ShellError::MenuThreadNoReply)?
}

/// Fills and reads submenu `index` of menu `session`, one that was empty when the menu was read
/// (`ShellMenuEntry::Submenu::lazy`). Its items run like the rest of the menu's. Blocks on the
/// thread holding the menu, so call from a background thread.
pub(crate) fn fill_submenu(session: u64, index: u32) -> Result<Vec<ShellMenuEntry>, ShellError> {
    let (done, result) = flume::bounded(1);
    let request = Request::FillSubmenu {
        session,
        index,
        done,
    };
    let unsent = {
        let parts = PARTS.lock();
        // Sent while the list is locked: a part's thread ends once released, which takes it out
        // of the list first.
        if let Some(part) = parts.iter().find(|p| p.session == session) {
            part.thread.send(request)?;
            None
        } else {
            Some(request)
        }
    };
    if let Some(request) = unsent {
        menu_thread()
            .ok_or(ShellError::MenuThreadStart)?
            .send(request)?;
    }
    result
        .recv()
        .map_err(|_hung_up| ShellError::MenuThreadNoReply)?
}

/// Releases a menu without waiting.
pub(crate) fn release(session: u64) {
    if let Some(part) = take_part(session) {
        let _ = part.thread.send(Request::Release(session));
    } else if let Some(thread) = menu_thread() {
        let _ = thread.send(Request::Release(session));
    }
}

fn run(rx: &flume::Receiver<Request>, wake: isize, part: bool) {
    // OleInitialize rather than CoInitialize: Copy/Cut commands use the OLE clipboard.
    // SAFETY: per-thread init, paired with OleUninitialize via OleGuard.
    if unsafe { OleInitialize(None) }.is_err() {
        return;
    }
    let _ole = OleGuard;
    let Some(window) = create_owner_window() else {
        return;
    };
    let wake = HANDLE(wake as *mut c_void);
    INBOX.with(|inbox| inbox.borrow_mut().rx = Some(rx.clone()));
    loop {
        #[cfg(test)]
        WAKES.fetch_add(1, Ordering::Relaxed);
        pump_messages();
        loop {
            // Not borrowed while handling: a fill looks at the inbox while it waits.
            let next = INBOX.with(|inbox| inbox.borrow_mut().next());
            match next {
                Next::Handle(request) => {
                    let released = matches!(request, Request::Release(_));
                    handle(request, window);
                    // A single extension's menu was released: its thread is done.
                    if part && released {
                        return;
                    }
                }
                Next::Idle => break,
                Next::Gone => return,
            }
            pump_messages();
        }
        // Sleep until a request is sent or a message arrives (extensions may go on through
        // messages after a command), with no timeout. A request sent after the queue was found
        // empty has already set the event, so the wait returns at once.
        // SAFETY: `wake` is a valid event handle for the life of the process.
        let _ = unsafe {
            MsgWaitForMultipleObjectsEx(Some(&[wake]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
    }
}

fn handle(request: Request, window: HWND) {
    match request {
        Request::Query {
            paths,
            background,
            extended,
            done,
        } => {
            CURRENT.with(|c| c.borrow_mut().take());
            let _ = done.send(build(&paths, background, extended, window));
        }
        Request::QueryHandler {
            paths,
            background,
            extended,
            handler,
            done,
        } => {
            CURRENT.with(|c| c.borrow_mut().take());
            let _ = done.send(build_handler(
                &paths, background, extended, &handler, window,
            ));
        }
        Request::Invoke {
            session,
            id,
            client_point,
            owner,
            done,
        } => {
            let current = CURRENT.with(|c| c.borrow_mut().take());
            let result = match current {
                Some(s) if s.id == session => invoke_in(&s, id, client_point, owner),
                _ => Err(ShellError::MenuExpired),
            };
            let _ = done.send(result);
        }
        Request::FillSubmenu {
            session,
            index,
            done,
        } => {
            let _ = done.send(fill_lazy(session, index));
        }
        Request::FlushClipboard(done) => {
            // SAFETY: plain OLE call; it errors harmlessly if this thread set no clipboard data.
            let _ = unsafe { OleFlushClipboard() };
            let _ = done.send(());
        }
        Request::Release(session) => CURRENT.with(|c| {
            let mut c = c.borrow_mut();
            if c.as_ref().is_some_and(|s| s.id == session) {
                *c = None;
            }
        }),
    }
}

/// Turns of the menu thread's loop (tests check it sleeps while nothing is asked).
#[cfg(test)]
static WAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct OleGuard;

impl Drop for OleGuard {
    fn drop(&mut self) {
        // SAFETY: pairs with the successful OleInitialize at the start of run.
        unsafe { OleUninitialize() };
    }
}

/// Call before the process exits so clipboard data set by extensions survives it.
pub(crate) fn flush_clipboard() {
    if let Some(thread) = menu_thread() {
        let (done, wait) = flume::bounded(1);
        if thread.send(Request::FlushClipboard(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop; msg is a valid output buffer.
    unsafe {
        while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// Forwards menu messages to `IContextMenu3`/`IContextMenu2`; some extensions fill
/// submenus this way.
fn forward_menu_msg(
    menu: &IContextMenu,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        // SAFETY: menu3 is a valid interface on this thread; result is a valid out pointer.
        return unsafe { menu3.HandleMenuMsg2(msg, wparam, lparam, Some(&raw mut result)) }
            .ok()
            .map(|()| result);
    }
    let menu2 = menu.cast::<IContextMenu2>().ok()?;
    // SAFETY: menu2 is a valid interface on this thread.
    unsafe { menu2.HandleMenuMsg(msg, wparam, lparam) }
        .ok()
        .map(|()| LRESULT(0))
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let handled = CURRENT.with(|current| {
            let current = current.try_borrow().ok()?;
            forward_menu_msg(&current.as_ref()?.menu, msg, wparam, lparam)
        });
        if let Some(result) = handled {
            return result;
        }
    }
    // SAFETY: default handling for everything else, with the arguments we received.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Hidden zero-size owner window for extensions; only receives messages.
fn create_owner_window() -> Option<HWND> {
    let class = w!("ArkeShellMenuOwner");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: wc outlives the call; the class name is static; the window belongs to this thread.
    unsafe {
        RegisterClassW(&raw const wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
        .ok()
    }
}

/// Owned PIDLs, freed on drop.
struct Pidls(Vec<*mut ITEMIDLIST>);

impl Drop for Pidls {
    fn drop(&mut self) {
        for &pidl in &self.0 {
            // SAFETY: each pidl was allocated by SHParseDisplayName and not yet freed.
            unsafe { ILFree(Some(pidl.cast_const())) };
        }
    }
}

/// Session ids, shared by whole menus and single extensions' menus.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn parse_pidls(paths: &[PathBuf]) -> Result<Pidls, ShellError> {
    let mut pidls = Pidls(Vec::new());
    for path in paths {
        let w = wide(path.as_os_str());
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: w is NUL-terminated; pidl is a valid out pointer, owned by Pidls on success.
        unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &raw mut pidl, 0, None) }.map_err(
            |source| ShellError::Path {
                path: path.clone(),
                source,
            },
        )?;
        pidls.0.push(pidl);
    }
    Ok(pidls)
}

fn build(
    paths: &[PathBuf],
    background: bool,
    extended: bool,
    window: HWND,
) -> Result<ShellMenu, ShellError> {
    if paths.is_empty() {
        return Ok(ShellMenu::default());
    }
    let pidls = parse_pidls(paths)?;
    // Owner is this thread's hidden window, so messages extensions send while building the
    // menu (e.g. "Send to" finishing its list) are handled here. Arke's window becomes the
    // owner only when invoking, as the dialog parent.
    let owner = window;
    let menu: IContextMenu = if background {
        // Folder background: the folder's own view object menu (New, Open in Terminal,
        // third-party background items).
        // SAFETY: binds from the desktop down to the folder; pidls.0[0] is a valid absolute PIDL.
        let folder: IShellFolder = unsafe {
            SHGetDesktopFolder().and_then(|desktop| desktop.BindToObject(pidls.0[0], None))
        }?;
        // SAFETY: owner is this thread's window.
        let menu: IContextMenu = unsafe { folder.CreateViewObject(owner) }?;
        set_background_site(&menu, &paths[0]);
        menu
    } else {
        items_object(paths, &pidls.0, owner)?
    };
    fill(
        menu,
        owner,
        pidls,
        menu_directory(paths, background),
        extended,
    )
}

/// The items' own UI object (their `IContextMenu` or `IDataObject`). Items of one folder are
/// asked of that folder by their IDs in it, as File Explorer asks. Items from different folders
/// (search results, #413) have no one folder to ask: they are asked of the desktop by their full
/// IDs, where each absolute ID is an item of its own. That is the shape Arke's drags give such
/// items too (`win_fileops::drag_data_object`). The one menu then has the commands for all of
/// them, third-party ones included, as File Explorer's search results do. `pidls` are the
/// absolute IDs of `paths`.
fn items_object<T: Interface>(
    paths: &[PathBuf],
    pidls: &[*mut ITEMIDLIST],
    owner: HWND,
) -> windows::core::Result<T> {
    let one_folder = paths
        .windows(2)
        .all(|pair| pair[0].parent() == pair[1].parent());
    if one_folder && let Some(&first) = pidls.first() {
        // SAFETY: first is a valid absolute PIDL.
        let folder: IShellFolder = unsafe { SHBindToParent(first, None) }?;
        let children: Vec<*const ITEMIDLIST> = pidls
            .iter()
            // SAFETY: ILFindLastID points into the same allocation, valid until pidls is freed.
            .map(|&p| unsafe { ILFindLastID(p) }.cast_const())
            .collect();
        // SAFETY: the PIDLs in children are valid for the call.
        return unsafe { folder.GetUIObjectOf(owner, &children, None) };
    }
    // SAFETY: plain COM call on this STA thread.
    let desktop: IShellFolder = unsafe { SHGetDesktopFolder() }?;
    let full: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p.cast_const()).collect();
    // SAFETY: the absolute PIDLs are valid for the call; the desktop takes them as its items.
    unsafe { desktop.GetUIObjectOf(owner, &full, None) }
}

/// The folder a menu for `paths` is for: the folder itself for its background, else the items'
/// folder. NUL-terminated, for [`Session::directory`].
fn menu_directory(paths: &[PathBuf], background: bool) -> Vec<u16> {
    let first = paths
        .first()
        .map_or_else(|| Path::new(""), PathBuf::as_path);
    let folder = if background {
        first
    } else {
        first.parent().unwrap_or(first)
    };
    wide(folder.as_os_str())
}

/// A menu whose entries are already known (one command read on its own, `one.rs`) as this
/// thread's current session: `menu` runs its item like any other menu's.
fn fill_single(
    menu: IContextMenu,
    owner: HWND,
    pidls: Pidls,
    directory: Vec<u16>,
    entries: Vec<ShellMenuEntry>,
) -> Result<ShellMenu, ShellError> {
    // SAFETY: creates an empty popup menu; Session calls DestroyMenu.
    let hmenu = unsafe { CreatePopupMenu() }?;
    let session = Session {
        id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
        menu,
        hmenu,
        owner,
        _pidls: pidls,
        directory,
        lazy: Vec::new(),
    };
    let result = ShellMenu {
        session: session.id,
        entries,
        partial: false,
        filling: Vec::new(),
    };
    CURRENT.with(|c| *c.borrow_mut() = Some(session));
    Ok(result)
}

/// The `uFlags` of `QueryContextMenu`: what File Explorer passes always includes `CMF_EXPLORE`.
/// A bare `CMF_NORMAL` (0) is what shell code passes for a double-click or a probe, and some
/// extensions return at once on it, adding nothing (#439). `extended`: Shift was held.
const fn query_flags(extended: bool) -> u32 {
    let flags = CMF_NORMAL | CMF_EXPLORE;
    if extended {
        flags | CMF_EXTENDEDVERBS
    } else {
        flags
    }
}

/// Reads `menu` into a [`Session`] that becomes this thread's current menu. `extended`: Shift
/// was held, so commands registered to show only then are included, as in File Explorer (#272).
fn fill(
    menu: IContextMenu,
    owner: HWND,
    pidls: Pidls,
    directory: Vec<u16>,
    extended: bool,
) -> Result<ShellMenu, ShellError> {
    // SAFETY: creates an empty popup menu; Session calls DestroyMenu.
    let hmenu = unsafe { CreatePopupMenu() }?;
    let mut session = Session {
        id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
        menu,
        hmenu,
        owner,
        _pidls: pidls,
        directory,
        lazy: Vec::new(),
    };
    let flags = query_flags(extended);
    // SAFETY: hmenu is valid; command IDs range over 1..=0x7FFF.
    let hr = unsafe { session.menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, flags) };
    if hr.is_err() {
        return Err(windows::core::Error::from(hr).into());
    }
    let mut reader = Reader::new(&session.menu, 0);
    let entries = reader.read_menu(hmenu, 0);
    session.lazy = reader.lazy;
    let result = ShellMenu {
        session: session.id,
        entries,
        partial: reader.partial,
        filling: reader.filling,
    };
    CURRENT.with(|c| *c.borrow_mut() = Some(session));
    Ok(result)
}

/// One extension's menu: created and initialized the way File Explorer does it for its own
/// menu (the folder, the selected items as a data object, the key it is registered under), then
/// read like a whole menu.
fn build_handler(
    paths: &[PathBuf],
    background: bool,
    extended: bool,
    handler: &MenuHandler,
    window: HWND,
) -> Result<ShellMenu, ShellError> {
    if paths.is_empty() {
        return Ok(ShellMenu::default());
    }
    if handler.kind != HandlerKind::Extension {
        return one::build(paths, background, handler, window);
    }
    // Send To with its label known: no need to create Windows' extension at all (#278).
    if !background
        && handler.is_send_to()
        && let Some(label) = one::cached_send_to_label()
    {
        return one::send_to(paths, label, window);
    }
    let mut pidls = parse_pidls(paths)?;
    let owner = window;
    // The folder: the path itself for its background, otherwise the items' parent.
    // SAFETY: pidls.0[0] is a valid absolute PIDL; the clone is owned by Pidls.
    let folder_pidl = unsafe { ILClone(pidls.0[0]) };
    if folder_pidl.is_null() {
        return Err(ShellError::MenuThreadNoReply);
    }
    pidls.0.push(folder_pidl);
    let data: Option<IDataObject> = if background {
        None
    } else {
        // SAFETY: folder_pidl is our own copy; drop the last ID to get the parent (the first
        // item's, for items from several folders).
        let _ = unsafe { ILRemoveLastID(Some(folder_pidl)) };
        Some(items_object(paths, &pidls.0[..paths.len()], owner)?)
    };
    let clsid_text = wide(std::ffi::OsStr::new(&handler.clsid));
    // SAFETY: NUL-terminated.
    let clsid = unsafe { CLSIDFromString(PCWSTR(clsid_text.as_ptr())) }?;
    // SAFETY: plain COM creation on this STA thread.
    let init: IShellExtInit =
        unsafe { CoCreateInstance(&raw const clsid, None, CLSCTX_INPROC_SERVER) }?;
    let key_name = wide(std::ffi::OsStr::new(&handler.key));
    let mut key = HKEY::default();
    // SAFETY: key_name is NUL-terminated; the key is closed below.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(key_name.as_ptr()),
            None,
            KEY_READ,
            &raw mut key,
        )
    }
    .is_ok();
    // SAFETY: folder_pidl and data are valid for the call; the key only while it is open.
    let initialized = unsafe {
        init.Initialize(
            Some(folder_pidl.cast_const()),
            data.as_ref(),
            opened.then_some(key),
        )
    };
    if opened {
        // SAFETY: opened above; extensions that keep the key duplicate it.
        let _ = unsafe { RegCloseKey(key) };
    }
    initialized?;
    let menu: IContextMenu = init.cast()?;
    // Send To: listed from its folder, not filled by the extension (`one.rs`, #278).
    if !background && handler.is_send_to() {
        let label = one::send_to_label(&menu).unwrap_or_else(|| handler.name.clone());
        return one::send_to(paths, label, owner);
    }
    if background {
        set_background_site(&menu, &paths[0]);
    }
    fill(
        menu,
        owner,
        pidls,
        menu_directory(paths, background),
        extended,
    )
}

/// File Explorer's view is the site of its empty-space menu, and some extensions ask it what the
/// menu is for: Windows' Sharing fills "Give access to" from it and leaves it empty without one.
/// Gives the menu a stand-in view of `folder` (`win_viewsite.rs`).
fn set_background_site(menu: &IContextMenu, folder: &Path) {
    let Some(site) = crate::win_viewsite::background_site(folder) else {
        return;
    };
    if let Ok(with_site) = menu.cast::<IObjectWithSite>() {
        // SAFETY: plain COM call on this STA thread; the menu keeps its own reference.
        let _ = unsafe { with_site.SetSite(&site) };
    }
}

/// Submenu `index` of the current menu `session`, filled now: the extension gets
/// `WM_INITMENUPOPUP` as when File Explorer opens it, then it is read like the rest of the menu.
/// Submenus inside it that are still empty become lazy in turn.
fn fill_lazy(session: u64, index: u32) -> Result<Vec<ShellMenuEntry>, ShellError> {
    let found = CURRENT.with(|c| {
        let c = c.borrow();
        let s = c.as_ref().filter(|s| s.id == session)?;
        let lazy = *s.lazy.get(index as usize)?;
        Some((s.menu.clone(), lazy, s.lazy.len()))
    });
    let (menu, lazy, known) = found.ok_or(ShellError::MenuExpired)?;
    // SAFETY: plain check; the menu belongs to the session, which is still current.
    if !unsafe { IsMenu(lazy.hmenu) }.as_bool() {
        return Err(ShellError::MenuExpired);
    }
    // The current menu is not borrowed meanwhile: messages the extension sends while filling
    // reach it through the window procedure.
    init_submenu(&menu, lazy.hmenu, lazy.pos, true);
    let mut reader = Reader::new(&menu, known);
    let entries = reader.read_menu(lazy.hmenu, lazy.depth);
    CURRENT.with(|c| {
        if let Some(s) = c.borrow_mut().as_mut().filter(|s| s.id == session) {
            s.lazy.extend(reader.lazy);
        }
    });
    Ok(entries)
}

/// Reads a menu into entries, keeping submenus that are still empty for later ([`Lazy`]).
struct Reader<'a> {
    menu: &'a IContextMenu,
    budget: usize,
    /// Empty submenus found, numbered from `first_lazy`.
    lazy: Vec<Lazy>,
    first_lazy: usize,
    /// Something wasn't read in full ([`ShellMenu::partial`]).
    partial: bool,
    /// Submenus left still filling ([`ShellMenu::filling`]).
    filling: Vec<(u32, String)>,
}

impl<'a> Reader<'a> {
    const fn new(menu: &'a IContextMenu, first_lazy: usize) -> Self {
        Self {
            menu,
            budget: MAX_ITEMS,
            lazy: Vec::new(),
            first_lazy,
            partial: false,
            filling: Vec::new(),
        }
    }

    /// Reads one menu level. Submenus get `WM_INITMENUPOPUP` first, since "Send to",
    /// "Open with" and others fill their content only then.
    fn read_menu(&mut self, hmenu: HMENU, depth: usize) -> Vec<ShellMenuEntry> {
        // SAFETY: hmenu is a valid menu.
        let count = unsafe { GetMenuItemCount(Some(hmenu)) };
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for pos in 0..u32::try_from(count).unwrap_or(0) {
            if self.budget == 0 {
                self.partial = true;
                break;
            }
            let Some(entry) = self.read_item(hmenu, pos, depth) else {
                continue;
            };
            // Extensions that refill a submenu can add the same command twice; keep one.
            if let ShellMenuEntry::Item { id, .. } = &entry
                && !seen.insert(*id)
            {
                continue;
            }
            self.budget -= 1;
            out.push(entry);
        }
        tidy_separators(out)
    }

    fn read_item(&mut self, hmenu: HMENU, pos: u32, depth: usize) -> Option<ShellMenuEntry> {
        let menu = self.menu;
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE
                | MIIM_STATE
                | MIIM_ID
                | MIIM_SUBMENU
                | MIIM_STRING
                | MIIM_BITMAP
                | MIIM_CHECKMARKS,
            ..Default::default()
        };
        // First call only asks for the string length.
        // SAFETY: info.cbSize is set; with a null dwTypeData only cch is returned.
        unsafe { GetMenuItemInfoW(hmenu, pos, true, &raw mut info) }.ok()?;
        if info.fType.0 & MFT_SEPARATOR.0 != 0 {
            return Some(ShellMenuEntry::Separator);
        }
        if info.fType.0 & MFT_BITMAP.0 != 0 {
            return None;
        }
        let mut text = vec![0u16; info.cch as usize + 1];
        info.dwTypeData = PWSTR(text.as_mut_ptr());
        info.cch += 1;
        // SAFETY: text has room for cch u16s including the NUL.
        unsafe { GetMenuItemInfoW(hmenu, pos, true, &raw mut info) }.ok()?;
        let len = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        let (label, access) = read_label(&String::from_utf16_lossy(&text[..len]));
        // Owner-draw items without text can't be read; skip them.
        if label.is_empty() {
            return None;
        }
        let enabled = info.fState.0 & MFS_DISABLED.0 == 0;
        let icon = item_icon(info.hbmpItem).or_else(|| item_icon(info.hbmpUnchecked));
        if !info.hSubMenu.is_invalid() {
            if depth + 1 >= MAX_DEPTH {
                return None;
            }
            // "Send to" is listed by Arke itself, beside this menu (`one.rs`): not waited for
            // here, where it took 0.7 to 1.6 s to fill and was often cut short. Kept to fill
            // when it opens, should that list not be there (#278).
            let send_to = one::cached_send_to_label().is_some_and(|l| l == label);
            // Nor is any other submenu still filling: the menu comes now, and it is filled
            // right after (`filling`), so no extension holds the whole menu up (#278).
            let settled = send_to || init_submenu(menu, info.hSubMenu, pos, false);
            self.partial |= !settled;
            let children = if send_to || !settled {
                Vec::new()
            } else {
                self.read_menu(info.hSubMenu, depth + 1)
            };
            // Items there that can't be read (owner-drawn, bitmaps, submenus past the depth
            // limit) leave nothing to show, now or later: dropped. Only a submenu with no items
            // at all is one its extension fills on opening.
            if children.is_empty()
                && settled
                && !send_to
                && !keeps_for_opening(item_count(info.hSubMenu))
            {
                return None;
            }
            // Left to fill when it opens: not read yet (Send To aside, read on its own).
            self.partial |= children.is_empty() && !send_to;
            let lazy = children.is_empty().then(|| {
                self.lazy.push(Lazy {
                    hmenu: info.hSubMenu,
                    pos,
                    depth: depth + 1,
                });
                u32::try_from(self.first_lazy + self.lazy.len() - 1).unwrap_or(u32::MAX)
            });
            if let Some(index) = lazy.filter(|_| !settled && !send_to) {
                self.filling.push((index, label.clone()));
            }
            return Some(ShellMenuEntry::Submenu {
                label,
                access,
                enabled,
                icon,
                children,
                lazy,
            });
        }
        Some(ShellMenuEntry::Item {
            id: info.wID,
            verb: item_verb(menu, info.wID),
            label,
            access,
            enabled,
            checked: info.fState.0 & MFS_CHECKED.0 != 0,
            default: info.fState.0 & MFS_DEFAULT.0 != 0,
            icon,
        })
    }
}

/// Canonical verb ("open", "copy", "delete", ...) used to spot items that duplicate Arke's
/// own menu. Many extension items have none.
fn item_verb(menu: &IContextMenu, id: u32) -> Option<String> {
    let offset = (id as usize).checked_sub(1)?;
    let mut buf = [0u16; 64];
    // SAFETY: with GCS_VERBW pszname is a buffer of cchmax u16s; offset is the command ID
    // minus the idCmdFirst passed to QueryContextMenu.
    unsafe {
        menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr().cast()),
            buf.len() as u32,
        )
    }
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let verb = String::from_utf16_lossy(&buf[..len]);
    (!verb.is_empty()).then_some(verb)
}

/// Lets the extension fill a submenu (`WM_INITMENUPOPUP`: wParam is the submenu, low word
/// of lParam its position in the parent). "Send to" fills in the background and shows a
/// blank placeholder until ready, so pump messages and ask again. The first fill takes
/// about 0.7 s, later ones are near instant; the wait is capped at about 1.5 s.
///
/// While the whole menu is read (`opening` false), nothing is waited for: a submenu its extension
/// left with no items at all (not even a placeholder) is kept and asked again when the user opens
/// it ([`fill_lazy`], `opening` true); Windows' Sharing did that to "Give access to" on a
/// folder's empty space, which held that menu up for the whole 1.5 s. One still filling (a
/// placeholder, no text yet) is left too, and filled right after the menu is in (#278): any
/// extension doing that held the whole menu up the same way.
///
/// Returns whether the submenu settled: false when it is still filling (not waited for, the
/// wait ran out, or it gave way to a command).
fn init_submenu(menu: &IContextMenu, submenu: HMENU, pos: u32, opening: bool) -> bool {
    let lparam = LPARAM(isize::try_from(pos).unwrap_or_default());
    let init = || {
        let _ = forward_menu_msg(menu, WM_INITMENUPOPUP, WPARAM(submenu.0 as usize), lparam);
    };
    init();
    if has_text(submenu) {
        return true;
    }
    if !opening {
        return item_count(submenu) <= 0;
    }
    // Slow submenus like "Send to" fill gradually; done once the item count is stable for a
    // few rounds. Only these get the message again (they refill fully each time).
    let mut last = -1;
    let mut stable = 0;
    for round in 0..INIT_TRIES {
        // A command picked (or the menu closed) meanwhile goes first; this fill ends with what
        // is there.
        if opening && urgent_waiting() {
            return false;
        }
        pump_messages();
        std::thread::sleep(Duration::from_millis(30));
        init();
        if !has_text(submenu) {
            // Opened and still without a single item: not much longer.
            if opening && round + 1 >= EMPTY_TRIES && item_count(submenu) <= 0 {
                return false;
            }
            continue;
        }
        let count = item_count(submenu);
        if count == last {
            stable += 1;
            if stable >= 3 {
                return true;
            }
        } else {
            last = count;
            stable = 0;
        }
    }
    false
}

/// Items in `hmenu`, of any kind (-1 if it isn't a menu).
fn item_count(hmenu: HMENU) -> i32 {
    // SAFETY: plain query; an invalid handle answers -1.
    unsafe { GetMenuItemCount(Some(hmenu)) }
}

/// Whether a submenu holding `count` items, none of them readable, is one its extension fills
/// only when it opens: it has no items at all (`GetMenuItemCount` 0). One with items Arke can't
/// read (owner-drawn, bitmaps, deeper than it goes) stays that way.
const fn keeps_for_opening(count: i32) -> bool {
    count == 0
}

/// Whether the menu has any item with text, or a submenu.
fn has_text(hmenu: HMENU) -> bool {
    // SAFETY: hmenu is a valid menu.
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    (0..u32::try_from(count).unwrap_or(0)).any(|pos| {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STRING | MIIM_SUBMENU,
            ..Default::default()
        };
        // SAFETY: only queries the string length (dwTypeData is null).
        unsafe { GetMenuItemInfoW(hmenu, pos, true, &raw mut info) }.is_ok()
            && info.fType.0 & MFT_SEPARATOR.0 == 0
            && (info.cch > 0 || !info.hSubMenu.is_invalid())
    })
}

/// Item bitmap. `HBMMENU_*` values (1..=11, -1) are system icon IDs, not real bitmaps.
fn item_icon(bitmap: HBITMAP) -> Option<arke_core::thumbs::Thumbnail> {
    let raw = bitmap.0 as isize;
    if (-1..=11).contains(&raw) {
        return None;
    }
    crate::win_icons::bitmap_pixels(bitmap)
}

/// The access key of a menu label (`&O` in Windows' text): the letter that picks the item while
/// the menu is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Access {
    /// The key as written; compared without regard to case.
    pub(crate) key: char,
    /// Where it sits in the clean label (byte offset). `None` for the CJK form, `Open(&O)`,
    /// where the label is kept without the key and it is shown after the text, as `(O)`.
    pub(crate) at: Option<usize>,
}

impl Access {
    /// Whether typing `typed` picks this key.
    pub(crate) fn matches(self, typed: char) -> bool {
        self.key.to_lowercase().eq(typed.to_lowercase())
    }
}

/// The label and its access key from Windows' menu text: `&Open` is "Open" with key O,
/// `開啟(&O)` is "開啟" with key O written after it, `&&` is a literal `&`, and the shortcut text
/// after a tab goes. A marker on something that isn't a letter or digit gives no key.
pub(crate) fn parse_label(raw: &str) -> (String, Option<Access>) {
    let text = raw.split('\t').next().unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut key: Option<(char, usize)> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('&') => {
                out.push('&');
                chars.next();
            }
            // Only the first marker counts, as in Windows.
            Some(next) if key.is_none() && next.is_alphanumeric() => key = Some((next, out.len())),
            _ => {}
        }
    }
    let start = out.len() - out.trim_start().len();
    let body = out.trim();
    // CJK menus write the key after the text, as "Open(&O)": the key is the one in parentheses.
    if let Some((head, _)) = key_suffix(body)
        && let Some((c, at)) = key
        && at == start + head.len() + 1
    {
        let access = Access { key: c, at: None };
        return (head.trim_end().to_owned(), Some(access));
    }
    let access = key.and_then(|(key, at)| {
        let at = at.checked_sub(start).filter(|at| *at < body.len())?;
        Some(Access { key, at: Some(at) })
    });
    (body.to_owned(), access)
}

/// `Head(K)` with a single letter or digit between the parentheses.
fn key_suffix(text: &str) -> Option<(&str, char)> {
    let (head, key) = text.strip_suffix(')')?.rsplit_once('(')?;
    let mut chars = key.chars();
    let c = chars.next()?;
    (chars.next().is_none() && c.is_ascii_alphanumeric()).then_some((head, c))
}

/// [`parse_label`] for a label read from Windows, where a CJK menu may also write the key after
/// the text without marking it (`Open(O)`): that is dropped too, as the label is the text only.
pub(crate) fn read_label(raw: &str) -> (String, Option<Access>) {
    let (label, access) = parse_label(raw);
    if access.is_some_and(|a| a.at.is_none()) {
        return (label, access);
    }
    match key_suffix(&label) {
        Some((head, _)) => {
            let head = head.trim_end().to_owned();
            let access = access.filter(|a| a.at.is_some_and(|at| at < head.len()));
            (head, access)
        }
        None => (label, access),
    }
}

/// Menu text without access-key markers (`&O`, `(&O)`) or the shortcut text after a tab.
pub(crate) fn clean_label(raw: &str) -> String {
    read_label(raw).0
}

/// Whether Windows is set to always underline access keys (Settings > Accessibility > Keyboard,
/// `SPI_GETKEYBOARDCUES`). Reads one system parameter: asked once as a menu opens.
pub(crate) fn keyboard_cues_always() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETKEYBOARDCUES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    if cfg!(test) {
        return false;
    }
    let mut on = windows::core::BOOL::from(false);
    // SAFETY: on is a correctly sized out param.
    let asked = unsafe {
        SystemParametersInfoW(
            SPI_GETKEYBOARDCUES,
            0,
            Some((&raw mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    asked.is_ok() && on.as_bool()
}

fn invoke_in(
    session: &Session,
    id: u32,
    client_point: (i32, i32),
    owner: Option<isize>,
) -> Result<(), ShellError> {
    let owner = owner.map_or(session.owner, |h| HWND(h as *mut _));
    let mut pt = POINT {
        x: client_point.0,
        y: client_point.1,
    };
    // SAFETY: owner is the Arke window or this thread's hidden window; pt is a valid in/out pointer.
    unsafe {
        let _ = ClientToScreen(owner, &raw mut pt);
    }
    let Some(verb) = (id as usize).checked_sub(1) else {
        return Ok(());
    };
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        // MAKEINTRESOURCE: the low word is the command offset (IDs start at 1 in QueryContextMenu).
        lpVerb: PCSTR(verb as *const u8),
        lpVerbW: PCWSTR(verb as *const u16),
        lpDirectoryW: PCWSTR(session.directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    // SAFETY: info is a fully initialized CMINVOKECOMMANDINFOEX with correct cbSize, which may
    // be passed as CMINVOKECOMMANDINFO.
    unsafe {
        session
            .menu
            .InvokeCommand((&raw const info).cast::<CMINVOKECOMMANDINFO>())
    }
    .map_err(ShellError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extensions that return on a bare `CMF_NORMAL` (FastCopy's does) must still be asked for
    /// their items: the flags always carry what File Explorer's do (#439).
    #[test]
    fn query_flags_are_never_bare_normal() {
        assert_ne!(query_flags(false), CMF_NORMAL);
        assert_ne!(query_flags(true), CMF_NORMAL);
        assert_ne!(query_flags(false) & CMF_EXPLORE, 0);
        assert_eq!(query_flags(false) & CMF_EXTENDEDVERBS, 0);
        assert_ne!(query_flags(true) & CMF_EXTENDEDVERBS, 0);
    }

    /// #278: once the menu host knows "Send to" (it lists it itself), reading the whole Windows
    /// menu doesn't wait for Windows' own to fill: it is left to fill when opened. Prints how
    /// long the whole menu took both ways.
    #[test]
    #[ignore = "reads this machine's Windows menu; run by hand"]
    #[allow(
        clippy::print_stderr,
        reason = "a by-hand measurement prints what it found"
    )]
    fn the_whole_menu_doesnt_wait_for_send_to() {
        let tmp_dir = crate::win::long_tempdir("arke-whole-sendto-");
        let file = tmp_dir.path().join("sendto.txt");
        std::fs::write(&file, "x").unwrap();
        let read = || {
            let started = std::time::Instant::now();
            let menu = query(std::slice::from_ref(&file), false, false).unwrap();
            let took = started.elapsed();
            release(menu.session);
            (menu, took)
        };
        let (_, cold) = read();
        let (before, waited) = read();
        warm_send_to(&file);
        let label = one::cached_send_to_label().expect("Send To's label");
        let (after, took) = read();
        let send_to = |menu: &ShellMenu| {
            menu.entries.iter().find_map(|e| match e {
                ShellMenuEntry::Submenu {
                    label: l,
                    children,
                    lazy,
                    ..
                } if *l == label => Some((children.len(), lazy.is_some())),
                _ => None,
            })
        };
        eprintln!(
            "cold {cold:?}; waiting for Send To {waited:?} {:?}; not waiting {took:?} {:?}",
            send_to(&before),
            send_to(&after)
        );
        let names: Vec<String> = before
            .entries
            .iter()
            .find_map(|e| match e {
                ShellMenuEntry::Submenu {
                    label: l, children, ..
                } if *l == label => Some(
                    children
                        .iter()
                        .map(|c| match c {
                            ShellMenuEntry::Item { label, verb, .. } => {
                                format!("{label} [{verb:?}]")
                            }
                            ShellMenuEntry::Submenu { label, .. } => format!("{label} ›"),
                            ShellMenuEntry::Separator => "---".into(),
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        eprintln!("Windows' own: {names:?}");
        assert_eq!(send_to(&after), Some((0, true)));
    }

    /// A folder background's command registered only for backgrounds (`Directory\Background\shell`,
    /// like Everything's "Search Everything...") runs (#184). Needs such a command:
    /// `ARKE_BG_VERB=<part of its label> cargo nextest run -p arke-app --run-ignored only background_only`.
    #[test]
    #[ignore = "needs a background-only shell command installed"]
    fn background_only_commands_run() {
        fn find(entries: &[ShellMenuEntry], part: &str) -> Option<(u32, String)> {
            entries.iter().find_map(|e| match e {
                ShellMenuEntry::Item { id, label, .. } if label.contains(part) => {
                    Some((*id, label.clone()))
                }
                ShellMenuEntry::Submenu { children, .. } => find(children, part),
                _ => None,
            })
        }
        let part = std::env::var("ARKE_BG_VERB").unwrap_or_else(|_| "Everything".into());
        let tmp_dir = tempfile::tempdir().unwrap();
        let folder = tmp_dir.path().to_path_buf();
        let menu = query(std::slice::from_ref(&folder), true, false).unwrap();
        let (id, label) = find(&menu.entries, &part).expect("the command is in the menu");
        let result = invoke(menu.session, id, (0, 0), None);
        assert!(result.is_ok(), "{label}: {result:?}");
    }

    /// On this machine: Windows' Sharing submenu ("Give access to") lists its items on a folder
    /// and on the folder's empty space, as in File Explorer (on empty space it used to be dropped,
    /// after holding the menu up for 1.5 s). Submenus still empty when read are filled as if
    /// opened. Prints both menus; runs nothing. By hand:
    /// `cargo nextest run -p arke-app --run-ignored only --no-capture give_access_to_lists_its_items`.
    #[test]
    #[ignore = "loads this machine's shell extensions; run by hand"]
    #[allow(clippy::print_stdout, reason = "a report printed for a run by hand")]
    fn give_access_to_lists_its_items() {
        /// The entries as text, submenus with their items (filled now when filled on opening).
        fn tree(session: u64, entries: &[ShellMenuEntry]) -> Vec<String> {
            entries
                .iter()
                .map(|e| match e {
                    ShellMenuEntry::Item { label, .. } => label.clone(),
                    ShellMenuEntry::Submenu {
                        label,
                        children,
                        lazy,
                        ..
                    } => {
                        let (children, how) = match lazy {
                            Some(index) => (
                                fill_submenu(session, *index).unwrap_or_default(),
                                " (filled on opening)",
                            ),
                            None => (children.clone(), ""),
                        };
                        format!("{label} ›{how} {:?}", tree(session, &children))
                    }
                    ShellMenuEntry::Separator => "-".into(),
                })
                .collect()
        }
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        // The submenu's name, from the Sharing extension's own menu for the folder.
        let sharing = crate::win_menu_parts::handlers(std::slice::from_ref(&dir), false)
            .into_iter()
            .find(|h| {
                h.clsid
                    .eq_ignore_ascii_case("{f81e9010-6ea4-11ce-a7ff-00aa003ca9f6}")
            });
        let Some(sharing) = sharing else {
            println!("Sharing isn't registered here");
            return;
        };
        let own = query_handler(std::slice::from_ref(&dir), false, false, &sharing, 1).unwrap();
        let name = own
            .entries
            .iter()
            .find_map(|e| match e {
                ShellMenuEntry::Submenu { label, .. } => Some(label.clone()),
                _ => None,
            })
            .expect("Sharing's submenu");
        release(own.session);
        for (what, background) in [("folder", false), ("background", true)] {
            let started = std::time::Instant::now();
            let menu = query(std::slice::from_ref(&dir), background, false).unwrap();
            let read = started.elapsed().as_millis();
            let lines = tree(menu.session, &menu.entries);
            println!("{what}: read in {read} ms");
            for line in &lines {
                println!("  {line}");
            }
            let give = lines
                .iter()
                .find(|l| l.starts_with(&format!("{name} ›")))
                .unwrap_or_else(|| panic!("{what}: no {name}"));
            assert!(!give.ends_with("[]"), "{what}: {give}");
            release(menu.session);
        }
    }

    /// #413: items from two folders (search results) get one Windows menu with the commands for
    /// both, as File Explorer's search results do.
    #[test]
    #[ignore = "reads this machine's Windows menu; run by hand"]
    fn items_from_two_folders_get_one_menu() {
        let tmp_dir = crate::win::long_tempdir("arke-two-folders-");
        let base = tmp_dir.path().to_path_buf();
        let files: Vec<PathBuf> = ["a", "b"]
            .iter()
            .map(|dir| {
                let dir = base.join(dir);
                std::fs::create_dir_all(&dir).unwrap();
                // The same name in both, as search results often have.
                let file = dir.join("arke-item.txt");
                std::fs::write(&file, "x").unwrap();
                file
            })
            .collect();
        let menu = query(&files, false, false);
        let menu = menu.expect("a menu for both items");
        let verbs: Vec<String> = menu
            .entries
            .iter()
            .filter_map(|e| match e {
                ShellMenuEntry::Item { verb, .. } => verb.clone(),
                _ => None,
            })
            .collect();
        release(menu.session);
        assert!(
            verbs.iter().any(|v| v.eq_ignore_ascii_case("properties")),
            "{verbs:?}"
        );
    }

    #[test]
    fn cleans_menu_labels() {
        assert_eq!(clean_label("&Open"), "Open");
        // zh-TW labels put the access key in parentheses after the text.
        assert_eq!(clean_label("開啟(&O)"), "開啟");
        assert_eq!(clean_label("傳送到(&N)"), "傳送到");
        // zh-CN uses the same style.
        assert_eq!(clean_label("打开(&O)"), "打开");
        assert_eq!(clean_label("发送到(&N)"), "发送到");
        assert_eq!(clean_label("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(clean_label("Copy\tCtrl+C"), "Copy");
        assert_eq!(clean_label("7-Zip"), "7-Zip");
        assert_eq!(clean_label("Version (2)"), "Version");
        assert_eq!(clean_label("Size (MB)"), "Size (MB)");
    }

    /// The label and `(key, offset)`, the offset being `None` for the CJK form.
    fn parsed(raw: &str) -> (String, Option<(char, Option<usize>)>) {
        let (label, access) = parse_label(raw);
        (label, access.map(|a| (a.key, a.at)))
    }

    #[test]
    fn keeps_the_access_key_apart_from_the_label() {
        assert_eq!(parsed("&Open"), ("Open".into(), Some(('O', Some(0)))));
        assert_eq!(parsed("Cop&y"), ("Copy".into(), Some(('y', Some(3)))));
        assert_eq!(parsed("Save &1"), ("Save 1".into(), Some(('1', Some(5)))));
        // The CJK form keeps the key in parentheses after the text, as Windows draws it.
        assert_eq!(parsed("開啟(&O)"), ("開啟".into(), Some(('O', None))));
        assert_eq!(parsed("发送到(&N)"), ("发送到".into(), Some(('N', None))));
        // A key in the middle of a label that also ends in a parenthesized letter.
        assert_eq!(
            parsed("&Tools (x)"),
            ("Tools (x)".into(), Some(('T', Some(0))))
        );
    }

    #[test]
    fn a_literal_ampersand_or_no_marker_gives_no_key() {
        assert_eq!(parsed("Tom && Jerry"), ("Tom & Jerry".into(), None));
        assert_eq!(parsed("Open"), ("Open".into(), None));
        // A marker on something that isn't a letter or digit.
        assert_eq!(parsed("100&%"), ("100%".into(), None));
        assert_eq!(parsed("Ends with &"), ("Ends with".into(), None));
        // `&&` first, then the real marker.
        assert_eq!(parsed("A && &B"), ("A & B".into(), Some(('B', Some(4)))));
    }

    #[test]
    fn the_shortcut_after_a_tab_and_stray_spaces_leave_the_key_in_place() {
        assert_eq!(
            parsed("Cop&y\tCtrl+C"),
            ("Copy".into(), Some(('y', Some(3))))
        );
        assert_eq!(parsed("  &Open  "), ("Open".into(), Some(('O', Some(0)))));
        // Only the first marker counts.
        assert_eq!(
            parsed("&Open &With"),
            ("Open With".into(), Some(('O', Some(0))))
        );
        // Read from Windows, an unmarked trailing "(N)" is still dropped (as before), key or not.
        assert_eq!(read_label("開啟(O)"), ("開啟".into(), None));
        assert_eq!(read_label("Version (2)"), ("Version".into(), None));
        assert_eq!(
            read_label("&Open (2)"),
            ("Open".into(), parse_label("&Open").1)
        );
    }

    #[test]
    fn a_key_is_matched_without_regard_to_case() {
        let (_, access) = parse_label("Cu&t");
        let access = access.expect("a key");
        assert!(access.matches('t') && access.matches('T'));
        assert!(!access.matches('c'));
    }

    #[test]
    fn tidies_separators() {
        let item = |id| ShellMenuEntry::Item {
            id,
            verb: None,
            label: "x".into(),
            access: None,
            enabled: true,
            checked: false,
            default: false,
            icon: None,
        };
        let out = tidy_separators(vec![
            ShellMenuEntry::Separator,
            item(1),
            ShellMenuEntry::Separator,
            ShellMenuEntry::Separator,
            item(2),
            ShellMenuEntry::Separator,
        ]);
        let shape: Vec<bool> = out
            .iter()
            .map(|e| matches!(e, ShellMenuEntry::Separator))
            .collect();
        assert_eq!(shape, [false, true, false]);
    }

    /// Reads the context menu of the built-in notepad.exe without invoking anything.
    #[test]
    fn reads_a_real_shell_menu() {
        let windir = std::env::var("windir").unwrap_or_else(|_| r"C:\Windows".into());
        let path = PathBuf::from(windir).join(r"System32\notepad.exe");
        let menu = query(&[path], false, false).expect("read the menu");
        assert!(!menu.entries.is_empty());
        let labels: Vec<&str> = menu
            .entries
            .iter()
            .filter_map(|e| match e {
                ShellMenuEntry::Item { label, .. } | ShellMenuEntry::Submenu { label, .. } => {
                    Some(label.as_str())
                }
                ShellMenuEntry::Separator => None,
            })
            .collect();
        assert!(
            labels.iter().all(|l| !l.contains('&')),
            "access key markers must be removed: {labels:?}"
        );
        release(menu.session);
    }

    #[allow(
        clippy::inline_always,
        clippy::ref_as_ptr,
        reason = "code generated by windows' implement macro"
    )]
    mod stub {
        use windows::Win32::UI::Shell::{CMINVOKECOMMANDINFO, IContextMenu, IContextMenu_Impl};
        use windows::Win32::UI::WindowsAndMessaging::HMENU;
        use windows::core::{HRESULT, PSTR};

        /// A context menu that adds nothing, standing for an extension around hand-built menus.
        #[windows::core::implement(IContextMenu)]
        pub(super) struct Stub;

        impl IContextMenu_Impl for Stub_Impl {
            fn QueryContextMenu(&self, _: HMENU, _: u32, _: u32, _: u32, _: u32) -> HRESULT {
                HRESULT(0)
            }
            fn InvokeCommand(&self, _: *const CMINVOKECOMMANDINFO) -> windows::core::Result<()> {
                Ok(())
            }
            fn GetCommandString(
                &self,
                _: usize,
                _: u32,
                _: *const u32,
                _: PSTR,
                _: u32,
            ) -> windows::core::Result<()> {
                Err(windows::Win32::Foundation::E_NOTIMPL.into())
            }
        }
    }
    use stub::Stub;

    fn popup() -> HMENU {
        // SAFETY: an empty menu, destroyed with its parent (or leaked by the test).
        unsafe { CreatePopupMenu() }.unwrap()
    }

    fn add_submenu(parent: HMENU, label: PCWSTR) -> HMENU {
        use windows::Win32::UI::WindowsAndMessaging::{AppendMenuW, MF_POPUP};
        let sub = popup();
        // SAFETY: both menus are valid; the parent takes the submenu.
        unsafe { AppendMenuW(parent, MF_POPUP, sub.0 as usize, label) }.unwrap();
        sub
    }

    fn add_owner_drawn(menu: HMENU) {
        use windows::Win32::UI::WindowsAndMessaging::{AppendMenuW, MF_OWNERDRAW};
        // SAFETY: valid menu; an owner-drawn item has no text, only its item data (none here).
        unsafe { AppendMenuW(menu, MF_OWNERDRAW, 9, PCWSTR::null()) }.unwrap();
    }

    /// A submenu with no items at all is kept to fill on opening; one with items but none readable
    /// yet (a placeholder, or owner-drawn) isn't waited for, and is filled right after the menu is
    /// in (#278: it held the whole menu up 1.5 s). One whose items can never be read (submenus
    /// past the depth limit) is dropped.
    #[test]
    fn only_submenus_with_no_items_wait_for_opening() {
        let menu: IContextMenu = Stub.into();
        let top = popup();
        add_submenu(top, w!("Empty"));
        add_owner_drawn(add_submenu(top, w!("Owner drawn")));
        // Four levels down: past MAX_DEPTH, so the innermost can't be read.
        let mut deep = add_submenu(top, w!("Deep"));
        for _ in 0..MAX_DEPTH {
            deep = add_submenu(deep, w!("Deeper"));
        }
        let mut reader = Reader::new(&menu, 0);
        let entries = reader.read_menu(top, 0);
        let shape: Vec<(String, Option<u32>)> = entries
            .iter()
            .filter_map(|e| match e {
                ShellMenuEntry::Submenu { label, lazy, .. } => Some((label.clone(), *lazy)),
                _ => None,
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("Empty".to_owned(), Some(0)),
                ("Owner drawn".to_owned(), Some(1))
            ]
        );
        assert_eq!(reader.lazy.len(), 2);
        assert_eq!(reader.filling, [(1, "Owner drawn".to_owned())]);
        assert!(reader.partial);
        // SAFETY: made above; destroys its submenus too.
        let _ = unsafe { DestroyMenu(top) };
    }

    /// A submenu filled as it opens that stays without a single item isn't waited for long.
    #[test]
    fn an_opened_submenu_with_no_items_is_given_up_soon() {
        let menu: IContextMenu = Stub.into();
        let top = popup();
        let sub = add_submenu(top, w!("Empty"));
        let started = std::time::Instant::now();
        init_submenu(&menu, sub, 0, true);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(800), "{took:?}");
        // SAFETY: made above.
        let _ = unsafe { DestroyMenu(top) };
    }

    /// A command picked (or the menu closed) while a submenu is being filled goes first: the
    /// fill stops waiting for its extension, and the next request taken is the command, ahead of
    /// fills asked before it.
    #[test]
    fn commands_go_ahead_of_submenu_fills() {
        let (tx, rx) = flume::unbounded();
        INBOX.with(|inbox| inbox.borrow_mut().rx = Some(rx));
        let fill = |index| {
            let (done, _) = flume::bounded(1);
            Request::FillSubmenu {
                session: 1,
                index,
                done,
            }
        };
        tx.send(fill(0)).unwrap();
        tx.send(fill(1)).unwrap();
        assert!(!urgent_waiting(), "only fills");
        tx.send(Request::Release(1)).unwrap();
        tx.send(fill(2)).unwrap();
        assert!(urgent_waiting());

        // A fill under way (its extension keeps a placeholder, so it would wait 1.5 s) ends now.
        let menu: IContextMenu = Stub.into();
        let top = popup();
        let sub = add_submenu(top, w!("Slow"));
        add_owner_drawn(sub);
        let started = std::time::Instant::now();
        init_submenu(&menu, sub, 0, true);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(300), "{took:?}");
        // SAFETY: made above.
        let _ = unsafe { DestroyMenu(top) };

        let order: Vec<String> =
            std::iter::from_fn(|| match INBOX.with(|i| i.borrow_mut().next()) {
                Next::Handle(Request::Release(s)) => Some(format!("release {s}")),
                Next::Handle(Request::FillSubmenu { index, .. }) => Some(format!("fill {index}")),
                Next::Handle(_) => Some("other".to_owned()),
                Next::Idle | Next::Gone => None,
            })
            .collect();
        assert_eq!(order, ["release 1", "fill 0", "fill 1", "fill 2"]);
        drop(tx);
        assert!(matches!(INBOX.with(|i| i.borrow_mut().next()), Next::Gone));
    }

    /// With nothing asked, the menu thread waits without waking (it used to poll every 30 ms),
    /// and a request still gets an answer right away.
    #[test]
    fn the_menu_thread_sleeps_until_asked() {
        flush_clipboard();
        let before = WAKES.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(600));
        let idle = WAKES.load(Ordering::Relaxed) - before;
        assert!(idle <= 2, "woke {idle} times in 600 ms with nothing to do");
        let asked = std::time::Instant::now();
        flush_clipboard();
        assert!(
            asked.elapsed() < Duration::from_millis(500),
            "{:?}",
            asked.elapsed()
        );
    }
}
