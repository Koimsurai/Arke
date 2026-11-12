//! "Show in folder" while Arke is the folder handler: a stand-in for the Explorer window the caller
//! is waiting for.
//!
//! `SHOpenFolderAndSelectItems` (browsers' "Show in folder", VS Code's reveal, ...) starts the
//! folder's `open` verb and then looks the new window up in the shell's window list
//! (`IShellWindows`), asks it for its view and calls `IShellView::SelectItem` for every item to
//! select. Without a window to find it would wait, give up and start Explorer itself.
//!
//! While Arke handles a folder, this module registers a small invisible shell window for that
//! folder (`RegisterPending` + `Register`, like the Files app's launcher does), answers the
//! caller's `SelectItem` calls and hands the collected items back. The window lives for a few
//! seconds at most, on its own thread with its own message loop; nothing is shown.
//!
//! Interfaces: `IWebBrowserApp` (what the list stores), `IServiceProvider` (how the caller reaches
//! the view) and `IShellView` (`SelectItem`); everything else only has to answer politely.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{
    E_NOINTERFACE, E_NOTIMPL, E_POINTER, HWND, LPARAM, LRESULT, SHANDLE_PTR, VARIANT_BOOL,
    VARIANT_FALSE, VARIANT_TRUE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO,
    IDispatch, IDispatch_Impl, IServiceProvider, IServiceProvider_Impl, ITypeInfo,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::IOleWindow_Impl;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{InitVariantFromBuffer, VARIANT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    _SVGIO, FOLDERSETTINGS, IFolderView, IFolderView_Impl, ILCombine, ILGetSize, IShellBrowser,
    IShellItem, IShellView, IShellView_Impl, IShellWindows, IWebBrowser_Impl, IWebBrowserApp,
    IWebBrowserApp_Impl, SHCreateItemFromIDList, SHParseDisplayName, SIGDN_FILESYSPATH, SVSI_EDIT,
    SVSI_ENSUREVISIBLE, SVSI_FOCUSED, SVSI_SELECT, SWC_BROWSER, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG,
    RegisterClassW, SetTimer, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_TIMER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BSTR, GUID, IUnknown, Interface, PCWSTR, Ref, implement};

use self::com::StandIn;

/// How long the stand-in window stays registered at most.
const LIFETIME: Duration = Duration::from_millis(3000);
/// After the last `SelectItem`, how long to wait for more items of the same call series.
const SETTLE: Duration = Duration::from_millis(200);

/// State shared between the COM object and the thread that pumps its messages.
#[derive(Default)]
struct Shared {
    /// Full paths the caller asked to select, in order.
    items: Mutex<Vec<PathBuf>>,
    /// When the last item arrived.
    last_item: Mutex<Option<Instant>>,
}

/// A folder's absolute item id list, freed on drop.
struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: allocated by the shell with the COM task allocator.
        unsafe { CoTaskMemFree(Some(self.0.cast_const().cast())) };
    }
}

// The macro-generated code and the shell's COM signatures trip several pedantic lints.
#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::wildcard_imports,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unused_self,
    clippy::unnecessary_wraps
)]
mod com {
    use super::*;

    /// The stand-in window's COM object.
    #[implement(IWebBrowserApp, IServiceProvider, IShellView, IFolderView)]
    pub(super) struct StandIn {
        pub(super) hwnd: HWND,
        pub(super) folder: Pidl,
        pub(super) shared: Arc<Shared>,
    }
    use windows::core::HRESULT;
    use windows::core::IUnknownImpl as _;

    type Res<T> = windows::core::Result<T>;

    impl IDispatch_Impl for StandIn_Impl {
        fn GetTypeInfoCount(&self) -> Res<u32> {
            Ok(0)
        }
        fn GetTypeInfo(&self, _: u32, _: u32) -> Res<ITypeInfo> {
            Err(E_NOTIMPL.into())
        }
        fn GetIDsOfNames(
            &self,
            _: *const GUID,
            _: *const PCWSTR,
            _: u32,
            _: u32,
            _: *mut i32,
        ) -> Res<()> {
            // DISP_E_UNKNOWNNAME
            Err(HRESULT(0x8002_0006_u32 as i32).into())
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
            // DISP_E_MEMBERNOTFOUND
            Err(HRESULT(0x8002_0003_u32 as i32).into())
        }
    }

    impl IWebBrowser_Impl for StandIn_Impl {
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
            _: &windows::core::BSTR,
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
            self.this_dispatch()
        }
        fn Parent(&self) -> Res<IDispatch> {
            self.this_dispatch()
        }
        fn Container(&self) -> Res<IDispatch> {
            self.this_dispatch()
        }
        fn Document(&self) -> Res<IDispatch> {
            self.this_dispatch()
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
            Ok(BSTR::new())
        }
        fn LocationURL(&self) -> Res<BSTR> {
            Ok(BSTR::new())
        }
        fn Busy(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
    }

    impl IWebBrowserApp_Impl for StandIn_Impl {
        fn Quit(&self) -> Res<()> {
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
            Ok(SHANDLE_PTR(self.hwnd.0 as isize))
        }
        fn FullName(&self) -> Res<BSTR> {
            let exe = std::env::current_exe().unwrap_or_default();
            Ok(BSTR::from(exe.to_string_lossy().as_ref()))
        }
        fn Path(&self) -> Res<BSTR> {
            self.FullName()
        }
        fn Visible(&self) -> Res<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
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

    impl IServiceProvider_Impl for StandIn_Impl {
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
            let unknown: IUnknown = self.to_interface();
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IOleWindow_Impl for StandIn_Impl {
        fn GetWindow(&self) -> Res<HWND> {
            Ok(self.hwnd)
        }
        fn ContextSensitiveHelp(&self, _: windows::core::BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellView_Impl for StandIn_Impl {
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
        fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> Res<()> {
            let wanted =
                (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0 | SVSI_EDIT.0) as u32;
            if item.is_null() || flags & wanted == 0 {
                return Ok(());
            }
            self.record(item, flags)
        }
        fn GetItemObject(&self, _: &_SVGIO, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // The background object is the folder view (multi-item selection goes through it).
            let unknown: IUnknown = self.to_interface();
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IFolderView_Impl for StandIn_Impl {
        fn GetCurrentViewMode(&self) -> Res<u32> {
            Ok(1)
        }
        fn SetCurrentViewMode(&self, _: u32) -> Res<()> {
            Ok(())
        }
        fn GetFolder(&self, _: *const GUID, _: *mut *mut c_void) -> Res<()> {
            Err(E_NOINTERFACE.into())
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
            Err(E_NOTIMPL.into())
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
                self.record(item, flags)?;
            }
            Ok(())
        }
    }

    impl StandIn_Impl {
        /// Notes that the caller selected the child item `item` of the folder.
        fn record(&self, item: *const ITEMIDLIST, flags: u32) -> Res<()> {
            let wanted =
                (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0 | SVSI_EDIT.0) as u32;
            if item.is_null() || flags & wanted == 0 {
                return Ok(());
            }
            // SAFETY: item is a valid child item id list for the call; the combined list is
            // freed below.
            let path = unsafe {
                let full = ILCombine(Some(self.folder.0.cast_const()), Some(item));
                if full.is_null() {
                    return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
                }
                let shell_item: Res<IShellItem> = SHCreateItemFromIDList(full);
                CoTaskMemFree(Some(full.cast_const().cast()));
                let name = shell_item?.GetDisplayName(SIGDN_FILESYSPATH)?;
                let path = name.to_string().ok().map(PathBuf::from);
                CoTaskMemFree(Some(name.0.cast_const().cast()));
                path
            };
            log::info!("shell window: item selected (flags {flags:#x}): {path:?}");
            if let Some(path) = path {
                self.shared.items.lock().push(path);
                *self.shared.last_item.lock() = Some(Instant::now());
            }
            Ok(())
        }

        fn this_dispatch(&self) -> Res<IDispatch> {
            let unknown: IUnknown = self.to_interface();
            unknown.cast()
        }
    }
}

/// The thread's hidden window; messages are only needed so the window is a real one.
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: forwards to the default procedure with the same arguments.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// Handle to the running stand-in window.
pub(crate) struct Linger(JoinHandle<()>);

impl Linger {
    /// Waits until the window's lifetime is over (a forwarding process must stay alive that long to
    /// pass on a late selection).
    pub(crate) fn wait(self) {
        let _ = self.0.join();
    }
}

/// Registers the stand-in window for `folder` and, once the caller selected items in it, calls
/// `deliver(folder, items)` from the window's thread. `None` when no thread could be started.
pub(crate) fn start(
    folder: PathBuf,
    deliver: impl FnOnce(PathBuf, Vec<PathBuf>) + Send + 'static,
) -> Option<Linger> {
    std::thread::Builder::new()
        .name("shell-window".into())
        .spawn(move || {
            let items = run(&folder);
            if !items.is_empty() {
                deliver(folder, items);
            }
        })
        .ok()
        .map(Linger)
}

/// Runs the message loop; returns what the caller selected.
#[allow(clippy::too_many_lines)]
fn run(folder: &std::path::Path) -> Vec<PathBuf> {
    // SAFETY: first COM call on this thread; paired with CoUninitialize below when it succeeds.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let items = register_and_wait(folder).unwrap_or_else(|err| {
        log::info!("shell window for {}: {err}", folder.display());
        Vec::new()
    });
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    items
}

#[allow(clippy::too_many_lines)]
fn register_and_wait(folder: &std::path::Path) -> windows::core::Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<u16> = folder
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut raw = std::ptr::null_mut();
    // SAFETY: name is NUL-terminated; raw receives an allocated item id list.
    unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }?;
    let pidl = Pidl(raw);
    // SAFETY: standard COM activation of the shell's window list.
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }?;

    let class = windows::core::w!("ArkeShellWindow");
    // SAFETY: registers a window class of this module; the window procedure is defined above.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Registering twice (a second folder in this process) fails harmlessly.
        RegisterClassW(&raw const wc);
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WINDOW_EX_STYLE(0),
            class,
            windows::core::w!("Arke"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    let cloak: i32 = 1;
    // SAFETY: cloaks our own window; the attribute value outlives the call.
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            (&raw const cloak).cast(),
            size_of::<i32>() as u32,
        )
    };

    let shared = Arc::new(Shared::default());
    let stand_in: IWebBrowserApp = StandIn {
        hwnd,
        folder: Pidl({
            // The object keeps its own copy of the folder's item id list.
            // SAFETY: duplicates a valid list with the COM allocator.
            unsafe { ILCombine(Some(pidl.0.cast_const()), None) }
        }),
        shared: shared.clone(),
    }
    .into();
    let dispatch: IDispatch = stand_in.cast()?;

    // SAFETY: pidl is valid for ILGetSize bytes; the variant owns a copy.
    let location = unsafe {
        InitVariantFromBuffer(
            pidl.0.cast_const().cast(),
            ILGetSize(Some(pidl.0.cast_const())),
        )
    }?;
    let empty = VARIANT::default();
    // SAFETY: valid interface and variants for the calls.
    let (pending, cookie) = unsafe {
        let pending = windows.RegisterPending(
            GetCurrentThreadId().cast_signed(),
            &raw const location,
            &raw const empty,
            SWC_BROWSER,
        )?;
        let cookie = windows.Register(&dispatch, hwnd.0 as i32, SWC_BROWSER)?;
        windows.OnNavigate(cookie, &raw const location)?;
        (pending, cookie)
    };

    // SAFETY: thread timers, so WM_TIMER arrives in this thread's queue.
    let (lifetime, tick) = unsafe {
        (
            SetTimer(
                None,
                0,
                u32::try_from(LIFETIME.as_millis()).unwrap_or(3000),
                None,
            ),
            SetTimer(None, 0, 50, None),
        )
    };
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out pointer; blocks until a message arrives.
        if unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        if msg.message == WM_TIMER && msg.hwnd.0.is_null() {
            let settled = shared
                .last_item
                .lock()
                .is_some_and(|at| at.elapsed() >= SETTLE);
            if msg.wParam.0 == lifetime || (msg.wParam.0 == tick && settled) {
                break;
            }
        }
        // SAFETY: msg came from GetMessageW.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    // SAFETY: undoes what was set up above; revoking a cookie twice is harmless.
    unsafe {
        let _ = KillTimer(None, lifetime);
        let _ = KillTimer(None, tick);
        let _ = windows.Revoke(cookie);
        if pending != cookie {
            let _ = windows.Revoke(pending);
        }
        let _ = DestroyWindow(hwnd);
    }
    drop(stand_in);
    Ok(std::mem::take(&mut *shared.items.lock()))
}
