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
//! So while a tab shows a folder inside a sync root (any provider; the explorer already knows
//! from its cloud scan, so nothing is read for other folders), this module registers the tab's
//! window in the shell's window list for that folder, like an Explorer window:
//! - `IShellWindows::Register` + `OnNavigate` with the folder's item id list, which raises
//!   `DShellWindowsEvents::WindowRegistered` for whoever follows the list;
//! - an `IWebBrowser2` answering for the window (its `HWND`, `LocationURL`, `LocationName`, late
//!   bound calls through the `SHDocVw` type library), reaching an `IShellBrowser` and its view
//!   (`IShellView`, `IFolderView::GetFolder`, `IPersistIDList`) through `IServiceProvider`;
//! - `DWebBrowserEvents` and `DWebBrowserEvents2` connection points: every listener that
//!   subscribes is told the navigation completed (`NavigateComplete`, `NavigateComplete2`,
//!   `DownloadComplete`, `DocumentComplete`), once.
//!
//! Nothing is downloaded: the folder is only named, never opened or read. The registration lasts
//! [`LIFETIME`] at most, ends when the tab shows another folder or its window closes, and lives on
//! its own thread with its own message loop (COM calls arrive as messages; the object isn't
//! agile, so they are all on that thread). When something read the location or took the events,
//! `heard` is called a few times ([`HEARD_AFTER`]): placeholders a client adds don't always come
//! with a watcher event, so the explorer then lists the folder again and reloads if items came.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{
    DISP_E_BADINDEX, DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_NOINTERFACE, E_NOTIMPL,
    E_POINTER, HWND, LPARAM, LRESULT, SHANDLE_PTR, VARIANT_BOOL, VARIANT_FALSE, VARIANT_TRUE,
    WPARAM,
};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoUninitialize, DISPATCH_FLAGS, DISPATCH_METHOD, DISPPARAMS, EXCEPINFO,
    IBindCtx, IConnectionPoint, IConnectionPoint_Impl, IConnectionPointContainer,
    IConnectionPointContainer_Impl, IDispatch, IDispatch_Impl, IEnumConnectionPoints,
    IEnumConnections, IPersist_Impl, IServiceProvider, IServiceProvider_Impl, IStream, ITypeInfo,
};
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
    IWebBrowser2, IWebBrowser2_Impl, IWebBrowserApp_Impl, SHBindToObject, SHCreateItemFromIDList,
    SHParseDisplayName, SWC_BROWSER, ShellWindows, UrlCreateFromPathW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, HMENU, IsWindow, IsWindowVisible, KillTimer, MSG, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetTimer, TranslateMessage, WM_QUIT, WM_TIMER,
};
use windows::core::{BSTR, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::shell::FolderVisit;
use crate::win_openinfolder::Pidl;

/// How long a folder stays registered at most. Listeners subscribe some time after the window
/// appears in the list; WPS Cloud is known to work with this long.
const LIFETIME: Duration = Duration::from_secs(10);
/// After the first listener looked, when `heard` is called.
const HEARD_AFTER: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(8),
];
/// How often the loop looks for new listeners to tell.
const TICK_MS: u32 = 50;

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

/// Running announcements: the tab (`slot`) and the thread whose message loop holds it.
static VISITS: LazyLock<Mutex<HashMap<u64, u32>>> = LazyLock::new(Mutex::default);

/// See [`crate::shell::Shell::folder_visit`]. Returns at once: the work runs on its own thread.
pub(crate) fn visit(slot: u64, visit: Option<FolderVisit>) {
    let Some(visit) = visit else {
        end(slot);
        return;
    };
    let Some(window) = visit.window.filter(|&w| w != 0) else {
        end(slot);
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("folder-visit".into())
        .spawn(move || run(slot, window, &visit.dir, &*visit.heard));
    if let Err(err) = spawned {
        log::error!("cannot spawn thread: {err}");
    }
}

/// Ends the tab's announcement, if one is running.
fn end(slot: u64) {
    if let Some(thread) = VISITS.lock().remove(&slot) {
        // SAFETY: posts WM_QUIT to a thread of ours, which leaves its message loop.
        let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
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
    sink: IDispatch,
    /// Told about the navigation already.
    told: bool,
}

/// What the browser, its view and its connection points share. Only ever used on the visit's
/// thread (the objects aren't agile).
struct State {
    hwnd: HWND,
    folder: Pidl,
    /// `file:///...`
    url: String,
    /// The folder's name, as `LocationName` gives it.
    name: String,
    type_info: Option<ITypeInfo>,
    sinks: RefCell<Vec<Sink>>,
    next_cookie: Cell<u32>,
    /// Something read where the window is, or subscribed to its events.
    looked_at: Cell<bool>,
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
    #[implement(IShellView, IFolderView, IPersistIDList, Agile = false)]
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
            Ok(())
        }
        fn Refresh2(&self, _: *const VARIANT) -> Res<()> {
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
            Err(E_NOTIMPL.into())
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
            self.state.looked_at.set(true);
            Ok(BSTR::from(self.state.name.as_str()))
        }
        fn LocationURL(&self) -> Res<BSTR> {
            self.state.looked_at.set(true);
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
        fn SendControlMsg(
            &self,
            _: u32,
            _: u32,
            _: WPARAM,
            _: LPARAM,
            _: *mut LRESULT,
        ) -> Res<()> {
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
                    .map_err(|_| windows::core::Error::from(CONNECT_E_CANNOTCONNECT))?
            };
            let cookie = self.state.next_cookie.get() + 1;
            self.state.next_cookie.set(cookie);
            self.state.sinks.borrow_mut().push(Sink {
                cookie,
                events: self.events,
                sink,
                told: false,
            });
            log::debug!("folder visit: a listener subscribed to {:?}", self.events);
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
        fn SelectItem(&self, _: *const ITEMIDLIST, _: u32) -> Res<()> {
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
            self.state.looked_at.set(true);
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
            _: u32,
            _: *const *const ITEMIDLIST,
            _: *const windows::Win32::Foundation::POINT,
            _: u32,
        ) -> Res<()> {
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
            self.state.looked_at.set(true);
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
            (s.events, s.sink.clone())
        })
        .collect();
    for (events, sink) in due {
        let told = match events {
            Events::Old => {
                let mut url = [bstr_variant(&state.url)];
                let told = fire(&sink, DISPID_NAVIGATECOMPLETE, &mut url);
                fire(&sink, DISPID_DOWNLOADCOMPLETE, &mut []);
                clear(&mut url);
                told
            }
            Events::New => {
                let mut url = bstr_variant(&state.url);
                let mut args = [variant_ref(&mut url), dispatch_variant(browser)];
                let told = fire(&sink, DISPID_NAVIGATECOMPLETE2, &mut args);
                fire(&sink, DISPID_DOWNLOADCOMPLETE, &mut []);
                fire(&sink, DISPID_DOCUMENTCOMPLETE, &mut args);
                clear(&mut args);
                clear(std::slice::from_mut(&mut url));
                told
            }
        };
        if told {
            state.looked_at.set(true);
        }
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

/// The visit's thread: registers, runs the message loop, revokes.
fn run(slot: u64, window: isize, dir: &Path, heard: &dyn Fn()) {
    let hwnd = HWND(window as *mut c_void);
    let mut msg = MSG::default();
    // SAFETY: creates this thread's message queue, so WM_QUIT can be posted to it from now on.
    let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: plain thread id query.
    let me = unsafe { GetCurrentThreadId() };
    if let Some(previous) = VISITS.lock().insert(slot, me) {
        // SAFETY: posts WM_QUIT to the tab's previous visit thread.
        let _ = unsafe { PostThreadMessageW(previous, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
    // SAFETY: first COM call on this thread; paired with CoUninitialize below when it succeeds.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if let Err(err) = announce(hwnd, dir, heard) {
        log::info!("folder visit for {}: {err}", dir.display());
    }
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    // Unless a newer visit of the tab took the place already.
    VISITS
        .lock()
        .retain(|&tab, &mut thread| tab != slot || thread != me);
}

#[allow(
    clippy::too_many_lines,
    reason = "set-up, message loop and clean-up read best in one place"
)]
fn announce(hwnd: HWND, dir: &Path, heard: &dyn Fn()) -> windows::core::Result<()> {
    if let Some((provider, population)) = crate::win_cloud::sync_root_provider(dir) {
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
    let pidl = folder.0.cast_const();
    let state = Rc::new(State {
        hwnd,
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
        looked_at: Cell::new(false),
    });
    let view: IShellView = com::View {
        state: Rc::clone(&state),
    }
    .into();
    let browser: IWebBrowser2 = com::Browser {
        state: Rc::clone(&state),
        view,
    }
    .into();
    let dispatch: IDispatch = browser.cast()?;
    // SAFETY: standard COM activation of the shell's window list.
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }?;
    // SAFETY: the list is valid for ILGetSize bytes; the variant owns a copy.
    // `pidl` is the state's own list, alive as long as `state`.
    let mut location = unsafe { InitVariantFromBuffer(pidl.cast(), ILGetSize(Some(pidl))) }?;
    // SAFETY: valid interface and variant for the calls.
    let cookie = unsafe {
        let cookie = windows.Register(&dispatch, hwnd.0 as i32, SWC_BROWSER)?;
        if let Err(err) = windows.OnNavigate(cookie, &raw const location) {
            let _ = windows.Revoke(cookie);
            let _ = VariantClear(&raw mut location);
            return Err(err);
        }
        cookie
    };
    // SAFETY: the variant was made by InitVariantFromBuffer above.
    let _ = unsafe { VariantClear(&raw mut location) };
    let started = Instant::now();
    log::debug!("folder visit registered for {}", dir.display());

    // SAFETY: a thread timer, so WM_TIMER arrives in this thread's queue.
    let tick = unsafe { SetTimer(None, 0, TICK_MS, None) };
    if tick == 0 {
        // SAFETY: revokes our own registration.
        let _ = unsafe { windows.Revoke(cookie) };
        return Err(windows::core::Error::from_thread());
    }
    let mut registered = true;
    let mut first_look: Option<Instant> = None;
    let mut heard_calls = 0;
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out pointer; blocks until a message arrives. 0 is WM_QUIT: the
        // tab moved on.
        if unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        if msg.message == WM_TIMER && msg.hwnd.0.is_null() && msg.wParam.0 == tick {
            // SAFETY: only reads whether the handle still names a window.
            let open = unsafe { IsWindow(Some(hwnd)) }.as_bool();
            if registered {
                tell_listeners(&state, &dispatch);
            }
            if first_look.is_none() && state.looked_at.get() {
                first_look = Some(Instant::now());
                log::info!(
                    "folder visit for {}: a listener looked after {} ms",
                    dir.display(),
                    started.elapsed().as_millis()
                );
            }
            if let Some(at) = first_look {
                while heard_calls < HEARD_AFTER.len()
                    && at.elapsed() >= HEARD_AFTER[heard_calls]
                {
                    heard();
                    heard_calls += 1;
                }
            }
            if registered && (!open || started.elapsed() >= LIFETIME) {
                // SAFETY: revokes our own registration.
                let _ = unsafe { windows.Revoke(cookie) };
                registered = false;
            }
            let heard_all = first_look.is_none() || heard_calls == HEARD_AFTER.len();
            if !open || (!registered && heard_all) {
                break;
            }
            continue;
        }
        // SAFETY: msg came from GetMessageW.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    // SAFETY: undoes what was set up above; revoking twice is harmless.
    unsafe {
        let _ = KillTimer(None, tick);
        if registered {
            let _ = windows.Revoke(cookie);
        }
    }
    state.sinks.borrow_mut().clear();
    log::debug!(
        "folder visit for {} ended after {} ms",
        dir.display(),
        started.elapsed().as_millis()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    /// Ending a tab that has no announcement is a no-op; a visit without a window starts nothing.
    #[test]
    fn nothing_runs_without_a_window() {
        end(u64::MAX);
        visit(
            u64::MAX - 1,
            Some(FolderVisit {
                window: None,
                dir: PathBuf::from(r"C:\nowhere"),
                heard: Box::new(|| panic!("no listener can hear a visit that never started")),
            }),
        );
        assert!(!VISITS.lock().contains_key(&(u64::MAX - 1)));
    }
}
