//! Tells the shell which cloud folder an Arke window shows, the way File Explorer announces every
//! folder it opens (#319).
//!
//! Sync clients built on the Cloud Files API fill a folder with its online-only items in one of two
//! ways. With the `CF_POPULATION_POLICY_FULL` policy the cloud filter asks the client when any
//! program lists the folder, Arke included. With `CF_POPULATION_POLICY_ALWAYS_FULL` the filter
//! never asks, and some clients (WPS Cloud) only fill a folder once a shell window shows it: they
//! follow the shell's window list (`IShellWindows`) and each window's navigation events. Arke
//! isn't in that list, so such a folder stayed empty in Arke until it had been opened in File
//! Explorer once.
//!
//! So while a pane shows a folder inside a sync root (any provider; the explorer already knows
//! from its cloud scan, so nothing is read for other folders), this module registers the pane's
//! window in the shell's window list for that folder, like an Explorer tab:
//! - `IShellWindows::Register` + `OnNavigate` with the folder's item id list, which raises
//!   `DShellWindowsEvents::WindowRegistered` for whoever follows the list;
//! - an `IWebBrowser2` answering for the window (its `HWND`, `LocationURL`, `LocationName`, late
//!   bound calls through the `SHDocVw` type library), reaching an `IShellBrowser` and its view
//!   (`IShellView`, `IFolderView::GetFolder`, `IPersistIDList`) through `IServiceProvider`;
//! - the view as the window's `Document`, giving itself for `SID_SFolderView`: that is how
//!   `SHOpenFolderAndSelectItems` reaches a window it found in the list to select items, and it
//!   fails instead of opening File Explorer when the document is missing;
//! - `DWebBrowserEvents` and `DWebBrowserEvents2` connection points: every listener is told the
//!   navigation completed (`NavigateComplete`, `NavigateComplete2`, `DownloadComplete`,
//!   `DocumentComplete`) as soon as it subscribes, once.
//!
//! One registration per pane, the way each File Explorer tab has its own registration under the
//! window's one `HWND`. It lasts exactly as long as the pane shows that folder: the pane going
//! elsewhere, closing (its window closing with it) or Arke quitting ([`end_all`]) revokes it.
//!
//! Nothing here waits on a clock. Each registration lives on its own thread with its own message
//! loop (the objects aren't agile, so every COM call arrives there as a message), which sleeps
//! until a message comes:
//! - a listener subscribed: tell it;
//! - something hints the folder's items changed ([`Trigger`], [`RELIST_ON`]): the explorer lists
//!   it again and reloads if items came or went, for placeholders that arrive without a watcher
//!   event;
//! - a "Show in folder" caller (`SHOpenFolderAndSelectItems`) that found this registration selects
//!   items: they are selected in the pane, as they would be in File Explorer.
//!
//! Nothing is downloaded: the folder is only named, never opened or read.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::{Condvar, Mutex};
use windows::Win32::Foundation::{
    DISP_E_BADINDEX, DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_NOINTERFACE, E_NOTIMPL,
    E_POINTER, HANDLE, HWND, LPARAM, LRESULT, SHANDLE_PTR, VARIANT_BOOL, VARIANT_FALSE,
    VARIANT_TRUE, WPARAM,
};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, DISPATCH_FLAGS, DISPATCH_METHOD, DISPPARAMS,
    EXCEPINFO, IBindCtx, IConnectionPoint, IConnectionPoint_Impl, IConnectionPointContainer,
    IConnectionPointContainer_Impl, IDispatch, IDispatch_Impl, IEnumConnectionPoints,
    IEnumConnections, IPersist_Impl, IServiceProvider, IServiceProvider_Impl, IStream, ITypeInfo,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    CONNECT_E_CANNOTCONNECT, CONNECT_E_NOCONNECTION, DispGetIDsOfNames, DispInvoke,
    IOleWindow_Impl, LoadRegTypeLib, OLECMDEXECOPT, OLECMDF, OLECMDID, OLEMENUGROUPWIDTHS,
    READYSTATE, READYSTATE_COMPLETE,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{
    InitVariantFromBuffer, VARENUM, VARIANT, VT_BSTR, VT_BYREF, VT_DISPATCH, VT_VARIANT,
    VariantClear,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    _SVGIO, DWebBrowserEvents, DWebBrowserEvents2, FOLDERSETTINGS, IFolderView, IFolderView_Impl,
    ILCombine, ILGetSize, IPersistIDList, IPersistIDList_Impl, IShellBrowser, IShellBrowser_Impl,
    IShellFolder, IShellItem, IShellView, IShellView_Impl, IShellWindows, IWebBrowser_Impl,
    IWebBrowser2, IWebBrowser2_Impl, IWebBrowserApp_Impl, SHBindToObject, SHCNE_CREATE,
    SHCNE_DELETE, SHCNE_MKDIR, SHCNE_RENAMEFOLDER, SHCNE_RENAMEITEM, SHCNE_RMDIR, SHCNE_UPDATEDIR,
    SHCNRF_NewDelivery, SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock,
    SHChangeNotifyDeregister, SHChangeNotifyEntry, SHChangeNotifyRegister, SHCreateItemFromIDList,
    SHParseDisplayName, SIGDN_FILESYSPATH, SVSI_DESELECTOTHERS, SVSI_EDIT, SVSI_ENSUREVISIBLE,
    SVSI_FOCUSED, SVSI_SELECT, SWC_BROWSER, ShellWindows, UrlCreateFromPathW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HMENU,
    HWND_MESSAGE, IsWindowVisible, MSG, PM_NOREMOVE, PeekMessageW, PostMessageW,
    PostThreadMessageW, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_QUIT, WNDCLASSW,
};
use windows::core::{BSTR, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::shell::FolderVisit;
use crate::win_openinfolder::Pidl;

/// `SHDocVw`'s type library (`IWebBrowser2` and its events), for late-bound callers.
const LIBID_SHDOCVW: GUID = GUID::from_u128(0xeab2_2ac0_30c1_11cf_a7eb_0000_c05b_ae0b);
/// `DWebBrowserEvents::NavigateComplete(URL)`
const DISPID_NAVIGATECOMPLETE: i32 = 101;
/// `DownloadComplete()`, in both event interfaces
const DISPID_DOWNLOADCOMPLETE: i32 = 104;
/// `DWebBrowserEvents2::NavigateComplete2(pDisp, URL)`
const DISPID_NAVIGATECOMPLETE2: i32 = 252;
/// `DWebBrowserEvents2::DocumentComplete(pDisp, URL)`
const DISPID_DOCUMENTCOMPLETE: i32 = 259;
/// `LOCALE_USER_DEFAULT`
const LOCALE_USER_DEFAULT: u32 = 0x400;

/// To the visit's message window: a listener subscribed, tell it.
const WM_TELL: u32 = WM_APP + 1;
/// To the visit's message window: a shell change notification for the folder.
const WM_SHELL_CHANGE: u32 = WM_APP + 2;

/// What may hint that the folder's items changed without a watcher event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// `SHChangeNotify` for the folder or an item in it (items created, deleted, renamed, or the
    /// whole folder updated), as a sync client that added placeholders may send so File
    /// Explorer's view refreshes. Shell-level only: plain file system changes are the watcher's.
    ShellNotify,
    /// A listener asked the window to refresh (`IWebBrowser2::Refresh`, `Refresh2`,
    /// `IShellView::Refresh`).
    ListenerRefresh,
}

/// The hints the explorer acts on. Which one WPS Cloud gives (if any: its placeholders may well
/// come with a watcher event) is to be confirmed on a machine with it; drop the others here.
const RELIST_ON: [Trigger; 2] = [Trigger::ShellNotify, Trigger::ListenerRefresh];

/// Shell notifications that mean items came, went or were renamed. `SHCNE_UPDATEITEM` (an
/// item's state changed: sync clients send it all the time) is left out.
const NOTIFY_EVENTS: u32 = SHCNE_CREATE.0
    | SHCNE_MKDIR.0
    | SHCNE_DELETE.0
    | SHCNE_RMDIR.0
    | SHCNE_RENAMEITEM.0
    | SHCNE_RENAMEFOLDER.0
    | SHCNE_UPDATEDIR.0;

/// Whether `trigger` makes the explorer list the folder again.
fn relists(trigger: Trigger) -> bool {
    RELIST_ON.contains(&trigger)
}

/// Whether the shell notification `event` (an `SHCNE_*` value) is one to act on.
const fn notifies_items(event: u32) -> bool {
    event & NOTIFY_EVENTS != 0
}

/// `SelectItem` flags that select (`SVSI_EDIT` selects and starts a rename).
const SELECTS: u32 = (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0 | SVSI_EDIT.0) as u32;

/// Adds `name` to what a "Show in folder" caller selected so far (`SVSI_DESELECTOTHERS` starts
/// over). Returns whether the selection changed and should be shown.
fn note_selected(selected: &mut Vec<Arc<str>>, name: Option<Arc<str>>, flags: u32) -> bool {
    let Some(name) = name.filter(|_| flags & SELECTS != 0) else {
        return false;
    };
    if flags & SVSI_DESELECTOTHERS.0 as u32 != 0 {
        selected.clear();
    }
    if !selected.contains(&name) {
        selected.push(name);
    }
    true
}

/// A running visit, as [`end`] and [`end_all`] see it.
#[derive(Default)]
struct Running {
    /// The visit's thread, once it has a message queue (0 before).
    thread: AtomicU32,
    /// Asked to end (possibly before the thread got going).
    quit: AtomicBool,
    /// Inside a call to a listener (another process, which may hang): [`end_all`] doesn't wait
    /// for it.
    calling: AtomicBool,
    /// The registration was revoked and the thread is done.
    ended: Mutex<bool>,
    changed: Condvar,
}

impl Running {
    fn set_calling(&self, calling: bool) {
        self.calling.store(calling, Ordering::SeqCst);
        // Taken once so a waiter between its check and its wait doesn't miss the change.
        drop(self.ended.lock());
        self.changed.notify_all();
    }

    fn finish(&self) {
        *self.ended.lock() = true;
        self.changed.notify_all();
    }

    /// Waits until the visit ended, unless it is (or gets) stuck calling a listener.
    fn wait(&self) {
        let mut ended = self.ended.lock();
        while !*ended && !self.calling.load(Ordering::SeqCst) {
            self.changed.wait(&mut ended);
        }
    }
}

/// Running visits by pane (`slot`).
static VISITS: LazyLock<Mutex<HashMap<u64, Arc<Running>>>> = LazyLock::new(Mutex::default);

/// See [`crate::shell::Shell::folder_visit`]. Returns at once: the work runs on its own thread.
pub(crate) fn visit(slot: u64, visit: Option<FolderVisit>) {
    end(slot);
    let Some(visit) = visit else {
        return;
    };
    let Some(window) = visit.window.filter(|&w| w != 0) else {
        return;
    };
    // In the list before the thread starts, so leaving right away ends it too.
    let running = Arc::new(Running::default());
    VISITS.lock().insert(slot, Arc::clone(&running));
    let thread_running = Arc::clone(&running);
    let spawned = std::thread::Builder::new()
        .name("folder-visit".into())
        .spawn(move || run(window, visit, &thread_running));
    if let Err(err) = spawned {
        log::error!("cannot spawn thread: {err}");
        VISITS.lock().retain(|_, r| !Arc::ptr_eq(r, &running));
    }
}

/// Ends the pane's visit, if one is running.
fn end(slot: u64) {
    let running = VISITS.lock().remove(&slot);
    if let Some(running) = running {
        quit(&running);
    }
}

fn quit(running: &Running) {
    running.quit.store(true, Ordering::SeqCst);
    let thread = running.thread.load(Ordering::SeqCst);
    if thread != 0 {
        // SAFETY: posts WM_QUIT to a thread of ours, which revokes its registration and returns.
        let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

/// Arke is quitting: revokes every registration, so the shell's window list doesn't keep windows
/// that are gone. Waits for each visit to finish, except one stuck in a listener's call.
pub(crate) fn end_all() {
    let running: Vec<Arc<Running>> = VISITS.lock().drain().map(|(_, r)| r).collect();
    for r in &running {
        quit(r);
    }
    for r in &running {
        r.wait();
    }
}

/// Which of the two browser event interfaces a listener subscribed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Events {
    /// `DWebBrowserEvents` (what WPS Cloud listens to)
    Old,
    /// `DWebBrowserEvents2`
    New,
}

impl Events {
    const fn iid(self) -> GUID {
        match self {
            Self::Old => DWebBrowserEvents::IID,
            Self::New => DWebBrowserEvents2::IID,
        }
    }

    fn of(iid: &GUID) -> Option<Self> {
        [Self::Old, Self::New].into_iter().find(|e| e.iid() == *iid)
    }
}

/// A subscribed listener.
struct Sink {
    cookie: u32,
    events: Events,
    dispatch: IDispatch,
    /// Told about the navigation already.
    told: bool,
}

/// What the browser, its view and its connection points share. Only ever used on the visit's
/// thread (the objects aren't agile).
struct State {
    /// The pane's window, as registered
    hwnd: HWND,
    /// The visit's own message-only window
    messages: HWND,
    folder: Pidl,
    /// `file:///...`
    url: String,
    /// The folder's name, as `LocationName` gives it.
    name: String,
    type_info: Option<ITypeInfo>,
    sinks: RefCell<Vec<Sink>>,
    next_cookie: Cell<u32>,
    /// What a "Show in folder" caller selected so far.
    selected: RefCell<Vec<Arc<str>>>,
    changed: Box<dyn Fn()>,
    select: Box<dyn Fn(Vec<Arc<str>>)>,
    running: Arc<Running>,
}

impl State {
    /// Something hints the folder's items changed.
    fn hint(&self, trigger: Trigger) {
        if relists(trigger) {
            log::debug!("folder visit: {trigger:?}, listing again");
            (self.changed)();
        }
    }

    /// Wakes the loop to tell a new listener (not from inside its `Advise` call, which is waiting).
    fn listener_added(&self) {
        // SAFETY: posts to this visit's own window.
        let _ = unsafe { PostMessageW(Some(self.messages), WM_TELL, WPARAM(0), LPARAM(0)) };
    }

    /// A "Show in folder" caller selected the child item `item` of the folder.
    fn select_item(&self, item: *const ITEMIDLIST, flags: u32) {
        if item.is_null() || flags & SELECTS == 0 {
            return;
        }
        let name = child_name(self.folder.0.cast_const(), item);
        let mut selected = self.selected.borrow_mut();
        if note_selected(&mut selected, name, flags) {
            let names = selected.clone();
            drop(selected);
            (self.select)(names);
        }
    }
}

/// The file name of the child item `item` of the folder `folder`.
fn child_name(folder: *const ITEMIDLIST, item: *const ITEMIDLIST) -> Option<Arc<str>> {
    // SAFETY: both lists are valid for the call; the combined list and the name are freed here.
    unsafe {
        let full = ILCombine(Some(folder), Some(item));
        if full.is_null() {
            return None;
        }
        let shell_item: windows::core::Result<IShellItem> = SHCreateItemFromIDList(full);
        CoTaskMemFree(Some(full.cast_const().cast()));
        let name = shell_item.ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        path?
            .file_name()
            .map(|n| Arc::from(n.to_string_lossy().as_ref()))
    }
}

#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::wildcard_imports,
    clippy::cast_possible_wrap,
    clippy::unused_self,
    clippy::unnecessary_wraps,
    reason = "The macro-generated code and the shell's COM signatures trip several pedantic lints"
)]
mod com {
    use super::*;
    use windows::core::HRESULT;
    use windows::core::IUnknownImpl as _;

    type Res<T> = windows::core::Result<T>;

    /// The window as the shell's window list sees it.
    #[implement(
        IWebBrowser2,
        IServiceProvider,
        IShellBrowser,
        IConnectionPointContainer,
        Agile = false
    )]
    pub(super) struct Browser {
        pub(super) state: Rc<State>,
        pub(super) view: IShellView,
    }

    /// The window's folder view.
    #[implement(
        IShellView,
        IFolderView,
        IPersistIDList,
        IDispatch,
        IServiceProvider,
        Agile = false
    )]
    pub(super) struct View {
        pub(super) state: Rc<State>,
    }

    /// One of the browser's event sources.
    #[implement(IConnectionPoint, Agile = false)]
    pub(super) struct EventPoint {
        events: Events,
        container: IConnectionPointContainer,
        state: Rc<State>,
    }

    impl IDispatch_Impl for Browser_Impl {
        fn GetTypeInfoCount(&self) -> Res<u32> {
            Ok(u32::from(self.state.type_info.is_some()))
        }
        fn GetTypeInfo(&self, index: u32, _: u32) -> Res<ITypeInfo> {
            match (&self.state.type_info, index) {
                (Some(info), 0) => Ok(info.clone()),
                _ => Err(DISP_E_BADINDEX.into()),
            }
        }
        fn GetIDsOfNames(
            &self,
            _: *const GUID,
            names: *const PCWSTR,
            count: u32,
            _: u32,
            ids: *mut i32,
        ) -> Res<()> {
            let Some(info) = &self.state.type_info else {
                return Err(DISP_E_UNKNOWNNAME.into());
            };
            // SAFETY: the caller passes `count` names and room for `count` ids.
            unsafe { DispGetIDsOfNames(info, names, count, ids) }
        }
        fn Invoke(
            &self,
            id: i32,
            _: *const GUID,
            _: u32,
            flags: DISPATCH_FLAGS,
            params: *const DISPPARAMS,
            result: *mut VARIANT,
            exception: *mut EXCEPINFO,
            arg_error: *mut u32,
        ) -> Res<()> {
            let Some(info) = &self.state.type_info else {
                return Err(DISP_E_MEMBERNOTFOUND.into());
            };
            let this: IWebBrowser2 = self.to_interface();
            // SAFETY: `this` is this object's IWebBrowser2, the dual interface `info` describes;
            // the other pointers come from the caller as they are.
            unsafe {
                DispInvoke(
                    this.as_raw(),
                    info,
                    id,
                    flags.0,
                    params.cast_mut(),
                    result,
                    exception,
                    arg_error,
                )
            }
        }
    }

    impl IWebBrowser_Impl for Browser_Impl {
        fn GoBack(&self) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GoForward(&self) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GoHome(&self) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GoSearch(&self) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Navigate(
            &self,
            _: &BSTR,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Refresh(&self) -> Res<()> {
            self.state.hint(Trigger::ListenerRefresh);
            Ok(())
        }
        fn Refresh2(&self, _: *const VARIANT) -> Res<()> {
            self.state.hint(Trigger::ListenerRefresh);
            Ok(())
        }
        fn Stop(&self) -> Res<()> {
            Ok(())
        }
        fn Application(&self) -> Res<IDispatch> {
            self.to_interface::<IWebBrowser2>().cast()
        }
        fn Parent(&self) -> Res<IDispatch> {
            self.Application()
        }
        fn Container(&self) -> Res<IDispatch> {
            self.Application()
        }
        fn Document(&self) -> Res<IDispatch> {
            self.view.cast()
        }
        fn TopLevelContainer(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }
        fn Type(&self) -> Res<BSTR> {
            Ok(BSTR::from("Arke"))
        }
        fn Left(&self) -> Res<i32> {
            Ok(0)
        }
        fn SetLeft(&self, _: i32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Top(&self) -> Res<i32> {
            Ok(0)
        }
        fn SetTop(&self, _: i32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Width(&self) -> Res<i32> {
            Ok(0)
        }
        fn SetWidth(&self, _: i32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Height(&self) -> Res<i32> {
            Ok(0)
        }
        fn SetHeight(&self, _: i32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn LocationName(&self) -> Res<BSTR> {
            Ok(BSTR::from(self.state.name.as_str()))
        }
        fn LocationURL(&self) -> Res<BSTR> {
            Ok(BSTR::from(self.state.url.as_str()))
        }
        fn Busy(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
    }

    impl IWebBrowserApp_Impl for Browser_Impl {
        fn Quit(&self) -> Res<()> {
            // Never closes Arke's window.
            Ok(())
        }
        fn ClientToWindow(&self, _: *mut i32, _: *mut i32) -> Res<()> {
            Ok(())
        }
        fn PutProperty(&self, _: &BSTR, _: &VARIANT) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetProperty(&self, _: &BSTR) -> Res<VARIANT> {
            Err(E_NOTIMPL.into())
        }
        fn Name(&self) -> Res<BSTR> {
            Ok(BSTR::from("Arke"))
        }
        fn HWND(&self) -> Res<SHANDLE_PTR> {
            Ok(SHANDLE_PTR(self.state.hwnd.0 as isize))
        }
        fn FullName(&self) -> Res<BSTR> {
            let exe = std::env::current_exe().unwrap_or_default();
            Ok(BSTR::from(exe.to_string_lossy().as_ref()))
        }
        fn Path(&self) -> Res<BSTR> {
            let exe = std::env::current_exe().unwrap_or_default();
            let dir = exe.parent().unwrap_or(&exe);
            Ok(BSTR::from(dir.to_string_lossy().as_ref()))
        }
        fn Visible(&self) -> Res<VARIANT_BOOL> {
            // SAFETY: reads a window's style; a closed window reads as invisible.
            let visible = unsafe { IsWindowVisible(self.state.hwnd) }.as_bool();
            Ok(if visible { VARIANT_TRUE } else { VARIANT_FALSE })
        }
        fn SetVisible(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn StatusBar(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
        fn SetStatusBar(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn StatusText(&self) -> Res<BSTR> {
            Ok(BSTR::new())
        }
        fn SetStatusText(&self, _: &BSTR) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn ToolBar(&self) -> Res<i32> {
            Ok(0)
        }
        fn SetToolBar(&self, _: i32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn MenuBar(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
        fn SetMenuBar(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn FullScreen(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
        fn SetFullScreen(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IWebBrowser2_Impl for Browser_Impl {
        fn Navigate2(
            &self,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn QueryStatusWB(&self, _: OLECMDID) -> Res<OLECMDF> {
            Err(E_NOTIMPL.into())
        }
        fn ExecWB(
            &self,
            _: OLECMDID,
            _: OLECMDEXECOPT,
            _: *const VARIANT,
            _: *mut VARIANT,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn ShowBrowserBar(
            &self,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn ReadyState(&self) -> Res<READYSTATE> {
            Ok(READYSTATE_COMPLETE)
        }
        fn Offline(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
        fn SetOffline(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Silent(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }
        fn SetSilent(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn RegisterAsBrowser(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }
        fn SetRegisterAsBrowser(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn RegisterAsDropTarget(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
        fn SetRegisterAsDropTarget(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn TheaterMode(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
        fn SetTheaterMode(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn AddressBar(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }
        fn SetAddressBar(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn Resizable(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }
        fn SetResizable(&self, _: VARIANT_BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IServiceProvider_Impl for Browser_Impl {
        fn QueryService(
            &self,
            _: *const GUID,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // Every service (SID_STopLevelBrowser, SID_SShellBrowser, ...) is this browser.
            let unknown: IUnknown = self.to_interface();
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IOleWindow_Impl for Browser_Impl {
        fn GetWindow(&self) -> Res<HWND> {
            Ok(self.state.hwnd)
        }
        fn ContextSensitiveHelp(&self, _: windows::core::BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellBrowser_Impl for Browser_Impl {
        fn InsertMenusSB(&self, _: HMENU, _: *mut OLEMENUGROUPWIDTHS) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SetMenuSB(&self, _: HMENU, _: isize, _: HWND) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn RemoveMenusSB(&self, _: HMENU) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SetStatusTextSB(&self, _: &PCWSTR) -> Res<()> {
            Ok(())
        }
        fn EnableModelessSB(&self, _: windows::core::BOOL) -> Res<()> {
            Ok(())
        }
        fn TranslateAcceleratorSB(
            &self,
            _: *const windows::Win32::UI::WindowsAndMessaging::MSG,
            _: u16,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn BrowseObject(&self, _: *const ITEMIDLIST, _: u32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetViewStateStream(&self, _: u32) -> Res<IStream> {
            Err(E_NOTIMPL.into())
        }
        fn GetControlWindow(&self, _: u32) -> Res<HWND> {
            Err(E_NOTIMPL.into())
        }
        fn SendControlMsg(&self, _: u32, _: u32, _: WPARAM, _: LPARAM, _: *mut LRESULT) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn QueryActiveShellView(&self) -> Res<IShellView> {
            Ok(self.view.clone())
        }
        fn OnViewWindowActive(&self, _: Ref<IShellView>) -> Res<()> {
            Ok(())
        }
        fn SetToolbarItems(
            &self,
            _: *const windows::Win32::UI::Controls::TBBUTTON,
            _: u32,
            _: u32,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IConnectionPointContainer_Impl for Browser_Impl {
        fn EnumConnectionPoints(&self) -> Res<IEnumConnectionPoints> {
            Err(E_NOTIMPL.into())
        }
        fn FindConnectionPoint(&self, riid: *const GUID) -> Res<IConnectionPoint> {
            // SAFETY: riid is null or a GUID supplied by the caller.
            let iid =
                unsafe { riid.as_ref() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
            let events = Events::of(iid)
                .ok_or_else(|| windows::core::Error::from(CONNECT_E_NOCONNECTION))?;
            Ok(EventPoint {
                events,
                container: self.to_interface(),
                state: Rc::clone(&self.state),
            }
            .into())
        }
    }

    impl IConnectionPoint_Impl for EventPoint_Impl {
        fn GetConnectionInterface(&self) -> Res<GUID> {
            Ok(self.events.iid())
        }
        fn GetConnectionPointContainer(&self) -> Res<IConnectionPointContainer> {
            Ok(self.container.clone())
        }
        fn Advise(&self, sink: Ref<IUnknown>) -> Res<u32> {
            let unknown = sink.ok()?;
            let iid = self.events.iid();
            let mut raw = std::ptr::null_mut();
            // SAFETY: valid interface; QueryInterface fills raw on success.
            let sink = if unsafe { unknown.query(&raw const iid, &raw mut raw) }.is_ok() {
                // SAFETY: an event dispinterface pointer is an IDispatch, owned now.
                unsafe { IDispatch::from_raw(raw) }
            } else {
                unknown
                    .cast::<IDispatch>()
                    .map_err(|_not_dispatch| windows::core::Error::from(CONNECT_E_CANNOTCONNECT))?
            };
            let cookie = self.state.next_cookie.get() + 1;
            self.state.next_cookie.set(cookie);
            self.state.sinks.borrow_mut().push(Sink {
                cookie,
                events: self.events,
                dispatch: sink,
                told: false,
            });
            log::info!("folder visit: a listener subscribed to {:?}", self.events);
            self.state.listener_added();
            Ok(cookie)
        }
        fn Unadvise(&self, cookie: u32) -> Res<()> {
            let mut sinks = self.state.sinks.borrow_mut();
            let before = sinks.len();
            sinks.retain(|s| s.cookie != cookie);
            let removed = sinks.len() != before;
            drop(sinks);
            if removed {
                Ok(())
            } else {
                Err(CONNECT_E_NOCONNECTION.into())
            }
        }
        fn EnumConnections(&self) -> Res<IEnumConnections> {
            Err(E_NOTIMPL.into())
        }
    }

    /// The view as a document: nothing to automate, only `IServiceProvider` behind it.
    impl IDispatch_Impl for View_Impl {
        fn GetTypeInfoCount(&self) -> Res<u32> {
            Ok(0)
        }
        fn GetTypeInfo(&self, _: u32, _: u32) -> Res<ITypeInfo> {
            Err(DISP_E_BADINDEX.into())
        }
        fn GetIDsOfNames(
            &self,
            _: *const GUID,
            _: *const PCWSTR,
            _: u32,
            _: u32,
            _: *mut i32,
        ) -> Res<()> {
            Err(DISP_E_UNKNOWNNAME.into())
        }
        fn Invoke(
            &self,
            _: i32,
            _: *const GUID,
            _: u32,
            _: DISPATCH_FLAGS,
            _: *const DISPPARAMS,
            _: *mut VARIANT,
            _: *mut EXCEPINFO,
            _: *mut u32,
        ) -> Res<()> {
            Err(DISP_E_MEMBERNOTFOUND.into())
        }
    }

    impl IServiceProvider_Impl for View_Impl {
        fn QueryService(
            &self,
            _: *const GUID,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // Every service (SID_SFolderView, ...) is this view.
            let unknown: IUnknown = self.to_interface();
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IOleWindow_Impl for View_Impl {
        fn GetWindow(&self) -> Res<HWND> {
            Ok(self.state.hwnd)
        }
        fn ContextSensitiveHelp(&self, _: windows::core::BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellView_Impl for View_Impl {
        fn TranslateAccelerator(
            &self,
            _: *const windows::Win32::UI::WindowsAndMessaging::MSG,
        ) -> HRESULT {
            E_NOTIMPL
        }
        fn EnableModeless(&self, _: windows::core::BOOL) -> Res<()> {
            Ok(())
        }
        fn UIActivate(&self, _: u32) -> Res<()> {
            Ok(())
        }
        fn Refresh(&self) -> Res<()> {
            self.state.hint(Trigger::ListenerRefresh);
            Ok(())
        }
        fn CreateViewWindow(
            &self,
            _: Ref<IShellView>,
            _: *const FOLDERSETTINGS,
            _: Ref<IShellBrowser>,
            _: *const windows::Win32::Foundation::RECT,
        ) -> Res<HWND> {
            Err(E_NOTIMPL.into())
        }
        fn DestroyViewWindow(&self) -> Res<()> {
            Ok(())
        }
        fn GetCurrentInfo(&self) -> Res<FOLDERSETTINGS> {
            Ok(FOLDERSETTINGS::default())
        }
        fn AddPropertySheetPages(
            &self,
            _: u32,
            _: windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE,
            _: LPARAM,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SaveViewState(&self) -> Res<()> {
            Ok(())
        }
        fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> Res<()> {
            self.state.select_item(item, flags);
            Ok(())
        }
        fn GetItemObject(&self, _: &_SVGIO, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            let unknown: IUnknown = self.to_interface();
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IFolderView_Impl for View_Impl {
        fn GetCurrentViewMode(&self) -> Res<u32> {
            Ok(1)
        }
        fn SetCurrentViewMode(&self, _: u32) -> Res<()> {
            Ok(())
        }
        fn GetFolder(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            let pidl = self.state.folder.0.cast_const();
            // SAFETY: riid was checked above; the folder's item id list is valid. Binding names
            // the folder only, nothing in it is opened.
            let folder: IUnknown = unsafe {
                if *riid == IShellItem::IID {
                    SHCreateItemFromIDList::<IShellItem>(pidl)?.cast()?
                } else {
                    SHBindToObject::<_, _, IShellFolder>(
                        None::<&IShellFolder>,
                        pidl,
                        None::<&IBindCtx>,
                    )?
                    .cast()?
                }
            };
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { folder.query(riid, ppv) }.ok()
        }
        fn Item(&self, _: i32) -> Res<*mut ITEMIDLIST> {
            Err(E_NOTIMPL.into())
        }
        fn ItemCount(&self, _: &_SVGIO) -> Res<i32> {
            Ok(0)
        }
        fn Items(&self, _: &_SVGIO, _: *const GUID, _: *mut *mut c_void) -> Res<()> {
            Err(E_NOINTERFACE.into())
        }
        fn GetSelectionMarkedItem(&self) -> Res<i32> {
            Ok(-1)
        }
        fn GetFocusedItem(&self) -> Res<i32> {
            Ok(-1)
        }
        fn GetItemPosition(&self, _: *const ITEMIDLIST) -> Res<windows::Win32::Foundation::POINT> {
            Err(E_NOTIMPL.into())
        }
        fn GetSpacing(&self, _: *mut windows::Win32::Foundation::POINT) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetDefaultSpacing(&self) -> Res<windows::Win32::Foundation::POINT> {
            Err(E_NOTIMPL.into())
        }
        fn GetAutoArrange(&self) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SelectItem(&self, _: i32, _: u32) -> Res<()> {
            Ok(())
        }
        fn SelectAndPositionItems(
            &self,
            count: u32,
            items: *const *const ITEMIDLIST,
            _: *const windows::Win32::Foundation::POINT,
            flags: u32,
        ) -> Res<()> {
            if items.is_null() {
                return Err(E_POINTER.into());
            }
            for index in 0..count as usize {
                // SAFETY: the caller passes `count` item id list pointers.
                let item = unsafe { *items.add(index) };
                // Only the first one may deselect the others.
                let flags = if index == 0 {
                    flags
                } else {
                    flags & !(SVSI_DESELECTOTHERS.0 as u32)
                };
                self.state.select_item(item, flags);
            }
            Ok(())
        }
    }

    impl IPersist_Impl for View_Impl {
        fn GetClassID(&self) -> Res<GUID> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IPersistIDList_Impl for View_Impl {
        fn SetIDList(&self, _: *const ITEMIDLIST) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetIDList(&self) -> Res<*mut ITEMIDLIST> {
            // SAFETY: duplicates a valid list with the COM allocator; the caller frees it.
            let copy = unsafe { ILCombine(Some(self.state.folder.0.cast_const()), None) };
            if copy.is_null() {
                return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
            }
            Ok(copy)
        }
    }
}

/// A VARIANT holding a copy of `text`; freed with `VariantClear`.
fn bstr_variant(text: &str) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a zeroed VARIANT is VT_EMPTY; it now owns a BSTR, which VariantClear frees.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BSTR;
        inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(text));
    }
    v
}

/// A VARIANT holding a reference to `dispatch`; freed with `VariantClear`.
fn dispatch_variant(dispatch: &IDispatch) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: as above, with an added reference VariantClear releases.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_DISPATCH;
        inner.Anonymous.pdispVal = ManuallyDrop::new(Some(dispatch.clone()));
    }
    v
}

/// A `VT_BYREF | VT_VARIANT` VARIANT pointing at `target`, which must outlive it. Owns nothing.
fn variant_ref(target: &mut VARIANT) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a by-reference VARIANT only stores the pointer.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_BYREF.0 | VT_VARIANT.0);
        inner.Anonymous.pvarVal = std::ptr::from_mut(target);
    }
    v
}

/// Calls the event `id` on `sink` with `args` (in the reverse order IDispatch wants them).
fn fire(sink: &IDispatch, id: i32, args: &mut [VARIANT]) -> bool {
    let params = DISPPARAMS {
        rgvarg: if args.is_empty() {
            std::ptr::null_mut()
        } else {
            args.as_mut_ptr()
        },
        rgdispidNamedArgs: std::ptr::null_mut(),
        cArgs: u32::try_from(args.len()).unwrap_or(0),
        cNamedArgs: 0,
    };
    // SAFETY: the arguments live for the call; the result isn't asked for.
    let result = unsafe {
        sink.Invoke(
            id,
            &GUID::zeroed(),
            LOCALE_USER_DEFAULT,
            DISPATCH_METHOD,
            &raw const params,
            None,
            None,
            None,
        )
    };
    if let Err(err) = &result {
        log::debug!("folder visit: event {id} failed: {err}");
    }
    result.is_ok()
}

/// Tells every listener that hasn't heard yet that the window finished navigating to the folder.
fn tell_listeners(state: &State, browser: &IDispatch) {
    // Marked first and called outside the borrow: a listener may subscribe or leave from inside
    // the call (COM dispatches incoming calls while waiting).
    let due: Vec<(Events, IDispatch)> = state
        .sinks
        .borrow_mut()
        .iter_mut()
        .filter(|s| !s.told)
        .map(|s| {
            s.told = true;
            (s.events, s.dispatch.clone())
        })
        .collect();
    if !due.is_empty() {
        state.running.set_calling(true);
    }
    for (events, sink) in &due {
        match events {
            Events::Old => {
                let mut url = [bstr_variant(&state.url)];
                fire(sink, DISPID_NAVIGATECOMPLETE, &mut url);
                fire(sink, DISPID_DOWNLOADCOMPLETE, &mut []);
                clear(&mut url);
            }
            Events::New => {
                let mut url = bstr_variant(&state.url);
                let mut args = [variant_ref(&mut url), dispatch_variant(browser)];
                fire(sink, DISPID_NAVIGATECOMPLETE2, &mut args);
                fire(sink, DISPID_DOWNLOADCOMPLETE, &mut []);
                fire(sink, DISPID_DOCUMENTCOMPLETE, &mut args);
                clear(&mut args);
                clear(std::slice::from_mut(&mut url));
            }
        }
    }
    if !due.is_empty() {
        state.running.set_calling(false);
    }
}

fn clear(variants: &mut [VARIANT]) {
    for v in variants {
        // SAFETY: each VARIANT was built above; by-reference ones own nothing.
        let _ = unsafe { VariantClear(v) };
    }
}

/// The folder's `file:` URL, as `LocationURL` gives it.
fn file_url(name: &[u16]) -> String {
    let mut buffer = vec![0_u16; 32_768];
    let mut len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    // SAFETY: `name` is NUL-terminated; the buffer holds `len` units.
    let made = unsafe {
        UrlCreateFromPathW(
            PCWSTR(name.as_ptr()),
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
            0,
        )
    };
    if made.is_err() {
        return String::new();
    }
    let len = (len as usize).min(buffer.len());
    let end = buffer[..len].iter().position(|&c| c == 0).unwrap_or(len);
    String::from_utf16_lossy(&buffer[..end])
}

/// The visit's loop state, for its window procedure (same thread).
#[derive(Clone)]
struct Current {
    state: Rc<State>,
    browser: IDispatch,
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

/// The visit's message-only window: listeners to tell, shell change notifications.
unsafe extern "system" fn visit_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // Cloned out, so a nested message (COM dispatches while a listener's call waits) can borrow
    // it again.
    let current = CURRENT.with(|c| c.borrow().clone());
    match (msg, current) {
        (WM_TELL, Some(current)) => {
            tell_listeners(&current.state, &current.browser);
            LRESULT(0)
        }
        (WM_SHELL_CHANGE, Some(current)) => {
            let mut event = 0_i32;
            // SAFETY: `w` and `l` are the notification's lock handle and process id, as
            // SHCNRF_NewDelivery hands them; the lock is released right away.
            unsafe {
                let lock = SHChangeNotification_Lock(
                    HANDLE(w.0 as *mut c_void),
                    l.0 as u32,
                    None,
                    Some(&raw mut event),
                );
                if !lock.is_invalid() {
                    let _ = SHChangeNotification_Unlock(lock);
                }
            }
            if notifies_items(event.cast_unsigned()) {
                current.state.hint(Trigger::ShellNotify);
            }
            LRESULT(0)
        }
        // SAFETY: forwards to the default procedure with the same arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// The visit's thread: registers, runs the message loop until asked to end, revokes.
fn run(window: isize, visit: FolderVisit, running: &Arc<Running>) {
    let mut msg = MSG::default();
    // SAFETY: creates this thread's message queue, so WM_QUIT can be posted to it from now on.
    let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: plain thread id query.
    running
        .thread
        .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    if !running.quit.load(Ordering::SeqCst) {
        // SAFETY: first COM call on this thread; paired with CoUninitialize below when it succeeds.
        let com =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        let dir = visit.dir.clone();
        if let Err(err) = announce(HWND(window as *mut c_void), visit, running) {
            log::info!("folder visit for {}: {err}", dir.display());
        }
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
    running.finish();
}

/// The visit's message-only window.
fn message_window() -> windows::core::Result<HWND> {
    let class = windows::core::w!("ArkeFolderVisit");
    // SAFETY: registers a window class of this module (twice fails harmlessly) and creates a
    // message-only window of it on this thread.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(visit_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&raw const wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn announce(hwnd: HWND, visit: FolderVisit, running: &Arc<Running>) -> windows::core::Result<()> {
    let FolderVisit {
        dir,
        changed,
        select,
        ..
    } = visit;
    if let Some((provider, population)) = crate::win_cloud::sync_root_provider(&dir) {
        log::info!(
            "folder visit for {} (sync provider {provider:?}, population policy {population})",
            dir.display()
        );
    }
    let name: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut raw = std::ptr::null_mut();
    // SAFETY: name is NUL-terminated; raw receives an allocated item id list. Parsing names the
    // folder only.
    unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }?;
    let folder = Pidl(raw);
    // SAFETY: reads the registered type library; nothing is written.
    let type_info = unsafe {
        LoadRegTypeLib(&LIBID_SHDOCVW, 1, 1, 0)
            .and_then(|lib| lib.GetTypeInfoOfGuid(&IWebBrowser2::IID))
    }
    .inspect_err(|err| log::debug!("folder visit: no SHDocVw type info: {err}"))
    .ok();
    let messages = message_window()?;
    let pidl = folder.0.cast_const();
    let state = Rc::new(State {
        hwnd,
        messages,
        folder,
        url: file_url(&name),
        name: dir
            .file_name()
            .unwrap_or(dir.as_os_str())
            .to_string_lossy()
            .into_owned(),
        type_info,
        sinks: RefCell::default(),
        next_cookie: Cell::new(0),
        selected: RefCell::default(),
        changed,
        select,
        running: Arc::clone(running),
    });
    let result = register_and_wait(&state, pidl, &dir);
    CURRENT.with(|c| *c.borrow_mut() = None);
    state.sinks.borrow_mut().clear();
    // SAFETY: our own window, on this thread.
    let _ = unsafe { DestroyWindow(messages) };
    result
}

/// Registers the window, listens for shell notifications and runs the loop until WM_QUIT.
fn register_and_wait(
    state: &Rc<State>,
    pidl: *const ITEMIDLIST,
    dir: &Path,
) -> windows::core::Result<()> {
    let view: IShellView = com::View {
        state: Rc::clone(state),
    }
    .into();
    let browser: IWebBrowser2 = com::Browser {
        state: Rc::clone(state),
        view,
    }
    .into();
    let dispatch: IDispatch = browser.cast()?;
    CURRENT.with(|c| {
        *c.borrow_mut() = Some(Current {
            state: Rc::clone(state),
            browser: dispatch.clone(),
        });
    });
    // SAFETY: standard COM activation of the shell's window list.
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }?;
    // SAFETY: the list is valid for ILGetSize bytes; the variant owns a copy. `pidl` is the
    // state's own list, alive as long as `state`.
    let mut location = unsafe { InitVariantFromBuffer(pidl.cast(), ILGetSize(Some(pidl))) }?;
    // SAFETY: valid interface and variant for the calls.
    let registered = unsafe {
        windows
            .Register(&dispatch, state.hwnd.0 as i32, SWC_BROWSER)
            .and_then(|cookie| {
                windows
                    .OnNavigate(cookie, &raw const location)
                    .inspect_err(|_| {
                        let _ = windows.Revoke(cookie);
                    })
                    .map(|()| cookie)
            })
    };
    // SAFETY: the variant was made by InitVariantFromBuffer above.
    let _ = unsafe { VariantClear(&raw mut location) };
    let cookie = registered?;
    // Shell-level notifications for the folder and its items (what SHChangeNotify sends).
    let entry = SHChangeNotifyEntry {
        pidl: pidl.cast_mut(),
        fRecursive: false.into(),
    };
    // SAFETY: our own window gets WM_SHELL_CHANGE; `entry` is read during the call.
    let notify = unsafe {
        SHChangeNotifyRegister(
            state.messages,
            SHCNRF_ShellLevel | SHCNRF_NewDelivery,
            NOTIFY_EVENTS.cast_signed(),
            WM_SHELL_CHANGE,
            1,
            &raw const entry,
        )
    };
    log::debug!("folder visit registered for {}", dir.display());
    let mut msg = MSG::default();
    // SAFETY: msg is a valid out pointer; blocks until a message arrives. 0 is WM_QUIT: the pane
    // left the folder, closed, or Arke is quitting.
    while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: msg came from GetMessageW.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    // SAFETY: undoes the registrations made above.
    unsafe {
        if notify != 0 {
            let _ = SHChangeNotifyDeregister(notify);
        }
        let _ = windows.Revoke(cookie);
    }
    log::debug!("folder visit for {} ended", dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_told_apart_by_their_interface() {
        assert_eq!(Events::of(&DWebBrowserEvents::IID), Some(Events::Old));
        assert_eq!(Events::of(&DWebBrowserEvents2::IID), Some(Events::New));
        assert_eq!(Events::of(&IDispatch::IID), None);
    }

    #[test]
    fn a_folder_location_is_a_file_url() {
        let name: Vec<u16> = r"C:\Cloud Drive\a b"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let url = file_url(&name);
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.to_lowercase().contains("cloud%20drive"), "{url}");
    }

    /// Items coming, going or renamed relist; an item's state changing (sync progress) doesn't.
    #[test]
    fn only_shell_notifications_about_items_relist() {
        for event in [
            SHCNE_CREATE,
            SHCNE_MKDIR,
            SHCNE_DELETE,
            SHCNE_RMDIR,
            SHCNE_RENAMEITEM,
            SHCNE_RENAMEFOLDER,
            SHCNE_UPDATEDIR,
        ] {
            assert!(notifies_items(event.0), "{event:?}");
        }
        assert!(!notifies_items(
            windows::Win32::UI::Shell::SHCNE_UPDATEITEM.0
        ));
        assert!(!notifies_items(
            windows::Win32::UI::Shell::SHCNE_ATTRIBUTES.0
        ));
        assert!(relists(Trigger::ShellNotify));
        assert!(relists(Trigger::ListenerRefresh));
    }

    /// What a "Show in folder" caller selects adds up; `SVSI_DESELECTOTHERS` starts over; flags
    /// that don't select change nothing.
    #[test]
    fn selections_from_the_shell_add_up() {
        let select = SVSI_SELECT.0 as u32;
        let first = select | SVSI_DESELECTOTHERS.0 as u32 | SVSI_ENSUREVISIBLE.0 as u32;
        let mut selected = Vec::new();
        assert!(note_selected(&mut selected, Some("a.txt".into()), first));
        assert!(note_selected(&mut selected, Some("b.txt".into()), select));
        assert!(note_selected(&mut selected, Some("b.txt".into()), select));
        assert_eq!(selected, [Arc::<str>::from("a.txt"), "b.txt".into()]);
        assert!(!note_selected(&mut selected, Some("c.txt".into()), 0));
        assert!(!note_selected(&mut selected, None, select));
        assert!(note_selected(&mut selected, Some("c.txt".into()), first));
        assert_eq!(selected, [Arc::<str>::from("c.txt")]);
    }

    /// Leaving a pane that has no visit is a no-op; a visit without a window starts nothing.
    #[test]
    fn nothing_runs_without_a_window() {
        end(u64::MAX);
        visit(
            u64::MAX - 1,
            Some(FolderVisit {
                window: None,
                dir: PathBuf::from(r"C:\nowhere"),
                changed: Box::new(|| panic!("a visit that never started has no hints")),
                select: Box::new(|_| panic!("a visit that never started has no selection")),
            }),
        );
        assert!(!VISITS.lock().contains_key(&(u64::MAX - 1)));
    }

    /// A "Show in folder" caller that found the window goes through its document: the view for
    /// `SID_SFolderView`, whose `SelectItem` selects in the pane.
    #[test]
    fn show_in_folder_reaches_the_view_through_the_document() {
        use windows::Win32::UI::Shell::ILFindLastID;
        const SID_SFOLDERVIEW: GUID = GUID::from_u128(0xcde7_25b0_ccc9_4519_917e_325d_72fa_b4ce);
        // SAFETY: this test thread's apartment.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let dir = std::env::temp_dir().join(format!("arke-visit-doc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let parse = |path: &Path| {
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut raw = std::ptr::null_mut();
            // SAFETY: NUL-terminated name; raw receives an allocated list.
            unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }
                .unwrap();
            Pidl(raw)
        };
        let picked: Rc<RefCell<Vec<Arc<str>>>> = Rc::default();
        let into = Rc::clone(&picked);
        let state = Rc::new(State {
            hwnd: HWND::default(),
            messages: HWND::default(),
            folder: parse(&dir),
            url: String::new(),
            name: String::new(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(|| {}),
            select: Box::new(move |names| *into.borrow_mut() = names),
            running: Arc::default(),
        });
        let view: IShellView = com::View {
            state: Rc::clone(&state),
        }
        .into();
        let browser: IWebBrowser2 = com::Browser { state, view }.into();
        let file = parse(&dir.join("a.txt"));
        // SAFETY: the calls a "Show in folder" caller makes; `file` outlives them.
        unsafe {
            let document = browser.Document().unwrap();
            let provider: IServiceProvider = document.cast().unwrap();
            let view: IShellView = provider.QueryService(&SID_SFOLDERVIEW).unwrap();
            view.SelectItem(
                ILFindLastID(file.0.cast_const()),
                (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0) as u32,
            )
            .unwrap();
        }
        assert_eq!(*picked.borrow(), [Arc::<str>::from("a.txt")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ending a visit before its thread got going still ends it: the thread sees the request.
    #[test]
    fn a_visit_ended_before_its_thread_started_ends() {
        let running = Arc::new(Running::default());
        quit(&running);
        assert!(running.quit.load(Ordering::SeqCst));
        running.finish();
        running.wait();
    }
}
