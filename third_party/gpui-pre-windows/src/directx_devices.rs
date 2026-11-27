use anyhow::{Context, Result};
use gpui_util::ResultExt;
use itertools::Itertools;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG, DXGI_CREATE_FACTORY_FLAGS,
            IDXGIAdapter1, IDXGIFactory6,
        },
    },
};
use windows::core::Interface;

// Arke: for `PendingDevices` and the per-device `ShaderCache`.
use std::{
    sync::{Arc, mpsc},
    time::Instant,
};

use crate::ShaderCache;

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
    /// Arke: the renderer's shaders for `device`, shared by every window (see [`ShaderCache`]).
    pub(crate) shaders: Arc<ShaderCache>,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        let (adapter, device, device_context, feature_level) =
            get_adapter(&dxgi_factory, debug_layer_available).context("Getting DXGI adapter")?;
        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created device with Direct3D 11.1 feature level.")
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created device with Direct3D 11.0 feature level.")
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created device with Direct3D 10.1 feature level.")
            }
            _ => unreachable!(),
        }

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
            shaders: Arc::new(ShaderCache::default()),
        })
    }
}

/// Arke: DirectX devices being made on a thread of their own.
///
/// Making the devices loads the graphics driver, and with the renderer's shaders that took about
/// 220 ms of a 370 ms cold start on the UI thread, before the app could do anything else. The
/// platform starts this as soon as it knows it will likely be windowed, so the work overlaps the
/// text system, the platform window and the app's own startup. The thread hands the devices over
/// as soon as they exist and then makes the shaders while holding the device's [`ShaderCache`],
/// so the first window waits only for whichever of the two it still lacks.
///
/// The devices are made exactly as [`DirectXDevices::new`] makes them on the UI thread (the same
/// adapter, the same flags), and device loss is handled as before once they are in use.
pub(crate) struct PendingDevices {
    receiver: mpsc::Receiver<Result<DirectXDevices>>,
    started: Instant,
}

impl PendingDevices {
    pub(crate) fn start() -> Self {
        let started = Instant::now();
        let (sender, receiver) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("DirectXDevices".to_owned())
            .spawn(move || {
                let devices = match DirectXDevices::new() {
                    Ok(devices) => devices,
                    Err(error) => {
                        sender.send(Err(error)).ok();
                        return;
                    }
                };
                log::info!(
                    "DirectX devices made in {:.1} ms on another thread",
                    started.elapsed().as_secs_f64() * 1000.0
                );
                let shaders = devices.shaders.clone();
                let device = devices.device.clone();
                // Taken before the devices are handed over, so a window made meanwhile waits for
                // these shaders instead of making its own.
                let mut cached = shaders.lock();
                if sender.send(Ok(devices)).is_err() {
                    // Not needed after all (the platform started headless).
                    return;
                }
                let shaders_started = Instant::now();
                if ShaderCache::fill(&mut cached, &device)
                    .context("Making the renderer's shaders ahead")
                    .log_err()
                    .is_some()
                {
                    log::info!(
                        "renderer shaders made in {:.1} ms on another thread",
                        shaders_started.elapsed().as_secs_f64() * 1000.0
                    );
                }
            });
        if let Err(error) = spawned {
            // The sender is gone with the closure, so `wait` makes the devices itself.
            log::warn!("can't make the DirectX devices on another thread: {error}");
        }
        Self { receiver, started }
    }

    /// Waits for the devices. If the thread couldn't start, or ended without sending them (it
    /// panicked), they are made here instead, as without this thread.
    pub(crate) fn wait(self) -> Result<DirectXDevices> {
        let waiting = Instant::now();
        let result = match self.receiver.recv() {
            Ok(result) => result,
            Err(mpsc::RecvError) => return DirectXDevices::new(),
        };
        log::info!(
            "waited {:.1} ms for the DirectX devices, {:.1} ms after they were started",
            waiting.elapsed().as_secs_f64() * 1000.0,
            self.started.elapsed().as_secs_f64() * 1000.0
        );
        result
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

#[inline]
fn get_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    for adapter_index in 0.. {
        let adapter: IDXGIAdapter1 = unsafe { dxgi_factory.EnumAdapters(adapter_index)?.cast()? };
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            log::info!("Using GPU: {}", gpu_name);
        }
        // Check to see whether the adapter supports Direct3D 11 and create
        // the device if it does.
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        if let Some(device) = get_device(
            &adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        )
        .log_err()
        {
            return Ok((adapter, device, context.unwrap(), feature_level));
        }
    }

    unreachable!()
}

#[inline]
fn get_device(
    adapter: &IDXGIAdapter1,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )?;
    }
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_made_ahead_bring_their_shaders() -> Result<()> {
        let devices = PendingDevices::start().wait()?;
        // The thread holds the cache from before it hands the devices over until the shaders
        // are made, so once the lock is free they are there.
        assert!(devices.shaders.lock().is_some());
        // Clones (every window's copy) share them.
        let window_copy = devices.clone();
        assert!(Arc::ptr_eq(&devices.shaders, &window_copy.shaders));
        Ok(())
    }
}
