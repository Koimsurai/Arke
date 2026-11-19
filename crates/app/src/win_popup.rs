//! Windows side of menus in their own windows (`popup.rs`): the monitors' work areas and scales,
//! moving a popup window, and letting the mouse through one that is fading out.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
};
use windows::core::BOOL;

use crate::popup::{Monitor, ScreenRect};

#[cfg_attr(
    test,
    allow(dead_code, reason = "tests place menus on monitors of their own")
)]
const fn screen_rect(rect: RECT) -> ScreenRect {
    ScreenRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// Every monitor with its work area (the screen minus the taskbar and docked bars) and scale.
#[cfg_attr(
    test,
    allow(dead_code, reason = "tests place menus on monitors of their own")
)]
pub(crate) fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `Vec` passed to `EnumDisplayMonitors` below, alive for the call.
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a MONITORINFO with its size set; `monitor` comes from the enumeration.
        if unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
            let (mut dpi, mut dpi_y) = (96, 96);
            // SAFETY: plain out parameters.
            let _ = unsafe {
                GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut dpi_y)
            };
            list.push(Monitor {
                bounds: screen_rect(info.rcMonitor),
                work: screen_rect(info.rcWork),
                scale: dpi as f32 / 96.,
            });
        }
        BOOL(1)
    }
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only runs during the call and only touches `list`.
    let _ =
        unsafe { EnumDisplayMonitors(None, None, Some(each), LPARAM((&raw mut list) as isize)) };
    list
}

/// Puts popup window `hwnd` at `frame` (physical screen pixels) and shows it, without activating
/// it. Run from a task, not while GPUI is drawing or dispatching: the window answers the move
/// right away.
pub(crate) fn move_popup(hwnd: isize, frame: ScreenRect) {
    // SAFETY: a stale handle only makes the call fail.
    let _ = unsafe {
        SetWindowPos(
            HWND(hwnd as _),
            None,
            frame.left,
            frame.top,
            frame.width(),
            frame.height(),
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        )
    };
}

/// Lets the mouse through popup window `hwnd` to Arke's window below it (a menu fading out).
pub(crate) fn pass_mouse_through(hwnd: isize) {
    gpui_windows::set_popup_mouse_passthrough(hwnd, true);
}
