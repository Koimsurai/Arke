#![cfg(target_os = "windows")]

mod clipboard;
mod destination_list;
mod dialog;
mod direct_manipulation;
mod direct_write;
mod directx_atlas;
mod directx_devices;
mod directx_renderer;
mod dispatcher;
mod display;
mod events;
mod frame_gate;
mod keyboard;
mod platform;
mod system_notifications;
mod system_settings;
mod util;
mod virtual_drop;
mod vsync;
mod window;
mod wrapper;

pub(crate) use clipboard::*;
pub(crate) use destination_list::*;
pub(crate) use direct_write::*;
pub(crate) use directx_atlas::*;
pub(crate) use directx_devices::*;
pub(crate) use directx_renderer::*;
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use events::*;
pub(crate) use frame_gate::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use system_notifications::*;
pub(crate) use system_settings::*;
pub(crate) use util::*;
pub(crate) use vsync::*;
pub(crate) use window::*;
pub(crate) use wrapper::*;

pub use platform::WindowsPlatform;
// Arke: drops of virtual files (see `virtual_drop.rs`).
pub use virtual_drop::{VIRTUAL_DROP_ROOT, drop_paths, is_virtual_drop, take_dropped_data_object};
// Arke: menus in popup windows (see `WindowKind::AnchoredPopup` in `window.rs`).
pub use window::set_popup_mouse_passthrough;
// Arke: Windows' modal drag loop runs inside event dispatch (see `begin_modal_drag`).
pub use window::{ModalDragGuard, begin_modal_drag};
// Arke: the integration API's `WM_COPYDATA` channel and window classes (see `window.rs`).
pub use window::{COPYDATA_ID, set_copydata_handler};

pub(crate) use windows::Win32::Foundation::HWND;
