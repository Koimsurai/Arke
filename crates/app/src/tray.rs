//! Notification area icon (resident mode only).
//!
//! With its last window closed Arke keeps running in the background; without an icon there's no
//! sign of it and no way to bring it back or quit.
//! - Left click opens a new window. Right click shows a small menu (new window, quit Arke).
//! - The icon is the exe's own resource (id 1); no separate file.
//! - Re-added automatically when Explorer restarts (taskbar recreated).
//!
//! A hidden message-only window on the UI thread receives notifications; actions are queued for the
//! gpui side, since the window procedure has no gpui `cx`. Touches only its own window and the
//! notification area, no registry.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use rust_i18n::t;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetSystemMetrics, HICON, HWND_MESSAGE, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW,
    LoadImageW, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, RegisterClassW,
    RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_LBUTTONUP,
    WM_RBUTTONUP, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayCommand {
    /// Open a new window
    Open,
    /// Switch "Open folders in Arke" (the setting) on or off
    ToggleTakeover,
    /// Quit Arke
    Quit,
    /// Show the update that was found (its notes and Download)
    Update,
}

/// Message the notification area uses to report mouse actions
const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;
const MENU_TAKEOVER: usize = 3;
const MENU_UPDATE: usize = 4;

static COMMANDS: OnceLock<flume::Sender<TrayCommand>> = OnceLock::new();
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Whether "Open folders in Arke" is on, for the menu's check mark.
static TAKEOVER: AtomicBool = AtomicBool::new(false);
/// Version of an update that was found and isn't installed, for the menu.
static UPDATE: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
/// Message Windows broadcasts when the taskbar is recreated (id registered at runtime)
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Set up the command receiver (once). Show the icon separately with [`set_visible`]. Call on the
/// UI thread.
pub(crate) fn install() -> Option<flume::Receiver<TrayCommand>> {
    let (tx, rx) = flume::unbounded();
    COMMANDS.set(tx).ok()?;
    Some(rx)
}

/// Show or hide the icon (the setting can turn it off). No-op if already in that state. Call on the
/// UI thread.
pub(crate) fn set_visible(visible: bool) -> bool {
    let shown = WINDOW.load(Ordering::Relaxed) != 0;
    if visible == shown {
        return true;
    }
    if !visible {
        remove();
        return true;
    }
    // SAFETY: on the UI thread, register the window class (re-registration fails harmlessly) and
    // create a message-only window; the window procedure is a 'static fn.
    let hwnd = unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return false;
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            lpszClassName: w!("ArkeTray"),
            ..Default::default()
        };
        RegisterClassW(&raw const class);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );
        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ArkeTray"),
            w!("Arke"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        );
        let Ok(hwnd) = created else {
            return false;
        };
        hwnd
    };
    WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
    add_icon(hwnd)
}

/// Tells the menu whether "Open folders in Arke" is on (shown as a check mark).
pub(crate) fn set_takeover(on: bool) {
    TAKEOVER.store(on, Ordering::Relaxed);
}

/// Tells the menu about an update that was found (`None` once there is none to offer).
pub(crate) fn set_update(version: Option<String>) {
    *UPDATE.lock() = version;
}

/// Remove the icon (before quitting or when the setting is turned off). No-op if never added.
pub(crate) fn remove() {
    let raw = WINDOW.swap(0, Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut _);
    let data = icon_data(hwnd);
    // SAFETY: data describes the icon we added, on a window we created.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &raw const data);
        let _ = DestroyWindow(hwnd);
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

fn add_icon(hwnd: HWND) -> bool {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    // Request the notification area size directly (16px at 100%); LoadIconW returns 32px and system
    // downscaling blurs it.
    // SAFETY: resource id 1 is the icon arke.rc compiles into the exe; MAKEINTRESOURCE passes the
    // id as a pointer.
    data.hIcon = unsafe {
        let size = GetSystemMetrics(SM_CXSMICON);
        GetModuleHandleW(None)
            .ok()
            .and_then(|module| {
                LoadImageW(
                    Some(module.into()),
                    PCWSTR(std::ptr::without_provenance(1)),
                    IMAGE_ICON,
                    size,
                    size,
                    LR_DEFAULTCOLOR,
                )
                .ok()
                .map(|h| HICON(h.0))
                .or_else(|| {
                    LoadIconW(Some(module.into()), PCWSTR(std::ptr::without_provenance(1))).ok()
                })
            })
            .unwrap_or_default()
    };
    for (slot, unit) in data.szTip.iter_mut().zip("Arke".encode_utf16()) {
        *slot = unit;
    }
    // SAFETY: data's fields are valid for the call.
    unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool()
}

fn send(command: TrayCommand) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(command);
    }
}

/// Context menu: the system popup menu (the convention for tray icons); returns what was chosen.
fn show_menu(hwnd: HWND) {
    let open: Vec<u16> = t!("tray.open").encode_utf16().chain([0]).collect();
    let takeover: Vec<u16> = t!("tray.takeover").encode_utf16().chain([0]).collect();
    let quit: Vec<u16> = t!("tray.quit").encode_utf16().chain([0]).collect();
    let update: Option<Vec<u16>> = UPDATE.lock().as_ref().map(|version| {
        t!("tray.update", version = version)
            .encode_utf16()
            .chain([0])
            .collect()
    });
    // SAFETY: the menu is created and destroyed here; strings are valid for the call.
    // SetForegroundWindow makes the menu close on outside clicks.
    let chosen = unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        if let Some(update) = &update {
            let _ = AppendMenuW(menu, MF_STRING, MENU_UPDATE, PCWSTR(update.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, PCWSTR(open.as_ptr()));
        let checked = if TAKEOVER.load(Ordering::Relaxed) {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let _ = AppendMenuW(
            menu,
            MF_STRING | checked,
            MENU_TAKEOVER,
            PCWSTR(takeover.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, PCWSTR(quit.as_ptr()));
        let mut at = POINT::default();
        let _ = GetCursorPos(&raw mut at);
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            at.x,
            at.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        chosen.0 as usize
    };
    match chosen {
        MENU_OPEN => send(TrayCommand::Open),
        MENU_TAKEOVER => send(TrayCommand::ToggleTakeover),
        MENU_QUIT => send(TrayCommand::Quit),
        MENU_UPDATE => send(TrayCommand::Update),
        _ => {}
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_TRAY {
        // Low 16 bits are the mouse message.
        match (lparam.0 as u32) & 0xFFFF {
            WM_LBUTTONUP => send(TrayCommand::Open),
            WM_RBUTTONUP => show_menu(hwnd),
            _ => {}
        }
        return LRESULT(0);
    }
    if message != 0 && message == TASKBAR_CREATED.load(Ordering::Relaxed) {
        // Explorer restarted and the icon is gone: add it back.
        add_icon(hwnd);
        return LRESULT(0);
    }
    // SAFETY: other messages get default handling.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
