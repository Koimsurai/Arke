//! [`Portable`] over Windows Portable Devices (#292): the WPD API, Windows' own interface to
//! phones and cameras over MTP. See `mtp.rs` for what this version does and doesn't do.
//!
//! WPD is COM. Everything runs on one thread of its own in the multithreaded apartment: MTP
//! does one thing at a time anyway, and the opened devices and the object ids learnt so far live
//! there. Callers hand it jobs and wait with a timeout ([`STEP_TIMEOUT`]); a file comes back in
//! chunks, each with its own timeout ([`CHUNK_TIMEOUT`]), so a big video isn't cut short and a
//! stuck device doesn't hang the caller.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, SystemTime};

use windows::Win32::Devices::PortableDevices::{
    E_WPD_DEVICE_NOT_OPEN, IPortableDevice, IPortableDeviceContent, IPortableDeviceKeyCollection,
    IPortableDeviceManager, IPortableDeviceProperties, IPortableDeviceValues, PortableDeviceFTM,
    PortableDeviceKeyCollection, PortableDeviceManager, PortableDeviceValues, WPD_CLIENT_NAME,
    WPD_CONTENT_TYPE_FOLDER, WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_DEVICE_OBJECT_ID,
    WPD_FUNCTIONAL_CATEGORY_STORAGE, WPD_FUNCTIONAL_OBJECT_CATEGORY, WPD_OBJECT_CONTENT_TYPE,
    WPD_OBJECT_DATE_MODIFIED, WPD_OBJECT_NAME, WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE,
    WPD_RESOURCE_DEFAULT,
};
use windows::Win32::Foundation::{E_ACCESSDENIED, FILETIME, PROPERTYKEY, SYSTEMTIME};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IStream, STGM_READ,
};
use windows::Win32::System::Variant::VT_DATE;
use windows::core::{GUID, HRESULT, PCWSTR, PWSTR};

use crate::error::{RemoteError, RemoteResult};
use crate::hub::TransferSink;
use crate::mtp::{Device, DeviceInfo, Object, Portable, device_key, is_portable, segments};

/// One request to a device (listing a big folder over MTP can take a while).
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// One chunk of a file.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// Children asked for at a time while listing.
const BATCH: usize = 64;

/// Devices that went away: unplugged, or the phone left "File transfer".
const GONE: [HRESULT; 4] = [
    E_WPD_DEVICE_NOT_OPEN,
    // ERROR_DEVICE_NOT_CONNECTED, ERROR_GEN_FAILURE, ERROR_NOT_FOUND
    HRESULT(0x8007_048F_u32.cast_signed()),
    HRESULT(0x8007_001F_u32.cast_signed()),
    HRESULT(0x8007_0490_u32.cast_signed()),
];

type Job = Box<dyn FnOnce(&mut State) + Send>;

/// The WPD backend; cheap to share, the work happens on its thread.
pub struct Wpd {
    jobs: flume::Sender<Job>,
}

impl Wpd {
    /// Starts the WPD thread. Touches no device until asked.
    pub fn new() -> Self {
        let (jobs, rx) = flume::unbounded::<Job>();
        let spawned = std::thread::Builder::new()
            .name("wpd".into())
            .spawn(move || {
                // SAFETY: first COM call on this thread, which stays in the MTA for its life.
                let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if let Err(err) = joined.ok() {
                    log::warn!("portable devices: cannot join the MTA: {err}");
                    return;
                }
                let mut state = State::default();
                for job in rx {
                    job(&mut state);
                }
            });
        if let Err(err) = spawned {
            log::warn!("portable devices: cannot start the thread: {err}");
        }
        Self { jobs }
    }

    /// Runs `job` on the WPD thread and waits for it, [`STEP_TIMEOUT`] at most.
    fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut State) -> RemoteResult<T> + Send + 'static,
    ) -> RemoteResult<T> {
        let (tx, rx) = flume::bounded(1);
        let sent = self.jobs.send(Box::new(move |state| {
            let _ = tx.send(job(state));
        }));
        if sent.is_err() {
            // The WPD thread couldn't start.
            return Err(RemoteError::DeviceGone);
        }
        rx.recv_timeout(STEP_TIMEOUT)
            .unwrap_or(Err(RemoteError::Timeout))
    }
}

impl Default for Wpd {
    fn default() -> Self {
        Self::new()
    }
}

impl Portable for Wpd {
    fn devices(&self) -> RemoteResult<Vec<Device>> {
        self.run(State::devices)
    }

    fn list(&self, device: &str, path: &str) -> RemoteResult<Vec<Object>> {
        let (device, path) = (device.to_owned(), path.to_owned());
        self.run(move |state| {
            state.on(&device, |open| {
                let parent = open.resolve(&path)?;
                let at_root = segments(&path).is_empty();
                let children = open.children(&parent, &path)?;
                Ok(children
                    .into_iter()
                    // The device root also lists functional objects that hold no files.
                    .filter(|o| !at_root || o.storage)
                    .map(|o| o.object)
                    .collect())
            })
        })
    }

    fn stat(&self, device: &str, path: &str) -> RemoteResult<Object> {
        let (device, path) = (device.to_owned(), path.to_owned());
        self.run(move |state| {
            state.on(&device, |open| {
                let names = segments(&path);
                let Some((name, parent)) = names.split_last() else {
                    return Ok(Object {
                        name: String::new(),
                        is_folder: true,
                        size: 0,
                        modified: None,
                    });
                };
                let parent_path = format!("/{}", parent.join("/"));
                let parent_id = open.resolve(&parent_path)?;
                open.children(&parent_id, &parent_path)?
                    .into_iter()
                    .find(|o| o.object.name == *name)
                    .map(|o| o.object)
                    .ok_or(RemoteError::NotFound)
            })
        })
    }

    fn read(
        &self,
        device: &str,
        path: &str,
        out: &mut dyn Write,
        max: u64,
        sink: &dyn TransferSink,
    ) -> RemoteResult<u64> {
        // The WPD thread reads and hands chunks over; dropping the receiver (cancel, timeout)
        // stops it at its next chunk.
        let (tx, rx) = flume::bounded::<RemoteResult<Vec<u8>>>(4);
        let (device, path) = (device.to_owned(), path.to_owned());
        let sent = self.jobs.send(Box::new(move |state| {
            let streamed = state.on(&device, |open| {
                let id = open.resolve(&path)?;
                open.stream(&id, max, &mut |chunk| tx.send(Ok(chunk)).is_ok())
            });
            if let Err(err) = streamed {
                let _ = tx.send(Err(err));
            }
        }));
        if sent.is_err() {
            return Err(RemoteError::DeviceGone);
        }
        let mut copied = 0u64;
        loop {
            if sink.cancelled() {
                return Err(RemoteError::Cancelled);
            }
            match rx.recv_timeout(CHUNK_TIMEOUT) {
                Ok(Ok(chunk)) => {
                    out.write_all(&chunk)?;
                    copied += chunk.len() as u64;
                    sink.add_bytes(chunk.len() as u64);
                }
                Ok(Err(err)) => return Err(err),
                Err(flume::RecvTimeoutError::Disconnected) => return Ok(copied),
                Err(flume::RecvTimeoutError::Timeout) => return Err(RemoteError::Timeout),
            }
        }
    }
}

/// What lives on the WPD thread.
#[derive(Default)]
struct State {
    manager: Option<IPortableDeviceManager>,
    /// Device key -> PnP id, from the last device list.
    ids: HashMap<String, String>,
    /// Opened devices by key.
    open: HashMap<String, Open>,
}

impl State {
    fn manager(&mut self) -> RemoteResult<IPortableDeviceManager> {
        if let Some(manager) = &self.manager {
            return Ok(manager.clone());
        }
        // SAFETY: plain COM activation on this MTA thread.
        let manager: IPortableDeviceManager =
            unsafe { CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER) }
                .map_err(wpd_error)?;
        self.manager = Some(manager.clone());
        Ok(manager)
    }

    fn devices(&mut self) -> RemoteResult<Vec<Device>> {
        let manager = self.manager()?;
        let mut ids = Vec::new();
        // SAFETY: the two-call pattern: count, then that many ids, each freed after copying.
        unsafe {
            manager.RefreshDeviceList().map_err(wpd_error)?;
            let mut count = 0u32;
            manager
                .GetDevices(std::ptr::null_mut(), &raw mut count)
                .map_err(wpd_error)?;
            let mut raw = vec![PWSTR::null(); count as usize];
            manager
                .GetDevices(raw.as_mut_ptr(), &raw mut count)
                .map_err(wpd_error)?;
            for id in raw.into_iter().take(count as usize) {
                if let Ok(text) = id.to_string() {
                    ids.push(text);
                }
                CoTaskMemFree(Some(id.0.cast_const().cast()));
            }
        }
        let mut out = Vec::new();
        self.ids.clear();
        for id in ids {
            // Disk volumes WPD also lists are drives, shown under This PC already (#437).
            if !is_portable(&DeviceInfo { pnp_id: id.clone() }) {
                continue;
            }
            let name = friendly_name(&manager, &id).unwrap_or_else(|| id.clone());
            let key = device_key(&name, &id);
            self.ids.insert(key.clone(), id);
            out.push(Device { key, name });
        }
        // Devices gone since are closed.
        self.open.retain(|key, _| out.iter().any(|d| d.key == *key));
        Ok(out)
    }

    /// Runs `f` on the opened device `key`, opening it first; a device that went away is
    /// forgotten so the next request opens it afresh.
    fn on<T>(
        &mut self,
        key: &str,
        f: impl FnOnce(&mut Open) -> RemoteResult<T>,
    ) -> RemoteResult<T> {
        if !self.open.contains_key(key) {
            if !self.ids.contains_key(key) {
                self.devices()?;
            }
            let id = self.ids.get(key).ok_or(RemoteError::DeviceGone)?.clone();
            let opened = Open::new(&id)?;
            self.open.insert(key.to_owned(), opened);
        }
        let open = self.open.get_mut(key).ok_or(RemoteError::DeviceGone)?;
        let result = f(open);
        if matches!(result, Err(RemoteError::DeviceGone)) {
            self.open.remove(key);
        }
        result
    }
}

fn friendly_name(manager: &IPortableDeviceManager, id: &str) -> Option<String> {
    let id = wide(id);
    let mut len = 0u32;
    // SAFETY: the two-call pattern with a buffer of the size asked for.
    unsafe {
        manager
            .GetDeviceFriendlyName(PCWSTR(id.as_ptr()), PWSTR::null(), &raw mut len)
            .ok()?;
        let mut buf = vec![0u16; len as usize + 1];
        manager
            .GetDeviceFriendlyName(PCWSTR(id.as_ptr()), PWSTR(buf.as_mut_ptr()), &raw mut len)
            .ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let name = String::from_utf16_lossy(&buf[..end]);
        (!name.trim().is_empty()).then(|| name.trim().to_owned())
    }
}

/// A child object as listed.
struct Child {
    id: String,
    object: Object,
    /// A storage (what the device root holds).
    storage: bool,
}

/// An opened device.
struct Open {
    device: IPortableDevice,
    content: IPortableDeviceContent,
    properties: IPortableDeviceProperties,
    keys: IPortableDeviceKeyCollection,
    /// Object ids by path, learnt while listing.
    ids: HashMap<String, String>,
}

impl Open {
    fn new(pnp_id: &str) -> RemoteResult<Self> {
        let id = wide(pnp_id);
        // SAFETY: plain COM calls on the WPD thread; strings outlive them.
        unsafe {
            let device: IPortableDevice =
                CoCreateInstance(&PortableDeviceFTM, None, CLSCTX_INPROC_SERVER)
                    .map_err(wpd_error)?;
            let client: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                    .map_err(wpd_error)?;
            client
                .SetStringValue(&WPD_CLIENT_NAME, windows::core::w!("Arke"))
                .map_err(wpd_error)?;
            device
                .Open(PCWSTR(id.as_ptr()), &client)
                .map_err(wpd_error)?;
            let content = device.Content().map_err(wpd_error)?;
            let properties = content.Properties().map_err(wpd_error)?;
            let keys: IPortableDeviceKeyCollection =
                CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER)
                    .map_err(wpd_error)?;
            for key in [
                WPD_OBJECT_ORIGINAL_FILE_NAME,
                WPD_OBJECT_NAME,
                WPD_OBJECT_CONTENT_TYPE,
                WPD_OBJECT_SIZE,
                WPD_OBJECT_DATE_MODIFIED,
                WPD_FUNCTIONAL_OBJECT_CATEGORY,
            ] {
                keys.Add(&raw const key).map_err(wpd_error)?;
            }
            let root = WPD_DEVICE_OBJECT_ID.to_string().unwrap_or_default();
            log::info!("portable device opened");
            Ok(Self {
                device,
                content,
                properties,
                keys,
                ids: HashMap::from([("/".to_owned(), root)]),
            })
        }
    }

    /// The object id at `path`, listing the folders above it that weren't listed yet.
    fn resolve(&mut self, path: &str) -> RemoteResult<String> {
        let names = segments(path);
        let mut at = "/".to_owned();
        let mut id = self.ids.get(&at).cloned().ok_or(RemoteError::NotFound)?;
        for name in names {
            let next = if at == "/" {
                format!("/{name}")
            } else {
                format!("{at}/{name}")
            };
            if !self.ids.contains_key(&next) {
                self.children(&id, &at)?;
            }
            id = self.ids.get(&next).cloned().ok_or(RemoteError::NotFound)?;
            at = next;
        }
        Ok(id)
    }

    /// The children of the object `parent` (at `path`), remembering their ids.
    fn children(&mut self, parent: &str, path: &str) -> RemoteResult<Vec<Child>> {
        let parent_w = wide(parent);
        let mut ids = Vec::new();
        // SAFETY: COM calls on the WPD thread; every returned id is freed after copying.
        unsafe {
            let objects = self
                .content
                .EnumObjects(0, PCWSTR(parent_w.as_ptr()), None)
                .map_err(wpd_error)?;
            loop {
                let mut batch = [PWSTR::null(); BATCH];
                let mut fetched = 0u32;
                let hr = objects.Next(&mut batch, &raw mut fetched);
                for id in batch.iter().take(fetched as usize) {
                    if let Ok(text) = id.to_string() {
                        ids.push(text);
                    }
                    CoTaskMemFree(Some(id.0.cast_const().cast()));
                }
                hr.ok().map_err(wpd_error)?;
                if (fetched as usize) < BATCH {
                    break;
                }
            }
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(child) = self.child(&id)? else {
                continue;
            };
            let full = if path == "/" {
                format!("/{}", child.object.name)
            } else {
                format!("{path}/{}", child.object.name)
            };
            // Two objects of one name (MTP allows it): the first one is the one shown.
            self.ids.entry(full).or_insert_with(|| child.id.clone());
            out.push(child);
        }
        Ok(out)
    }

    /// One object's properties; `None` for what isn't a file, folder or storage.
    fn child(&self, id: &str) -> RemoteResult<Option<Child>> {
        let id_w = wide(id);
        // SAFETY: COM calls on the WPD thread; returned strings are freed after copying and
        // the date's PROPVARIANT is cleared.
        unsafe {
            let values = self
                .properties
                .GetValues(PCWSTR(id_w.as_ptr()), &self.keys)
                .map_err(wpd_error)?;
            let text = |key: &PROPERTYKEY| -> Option<String> {
                let raw = values.GetStringValue(key).ok()?;
                let text = raw.to_string().ok();
                CoTaskMemFree(Some(raw.0.cast_const().cast()));
                text.filter(|t| !t.is_empty())
            };
            let kind: GUID = values
                .GetGuidValue(&WPD_OBJECT_CONTENT_TYPE)
                .unwrap_or_default();
            let functional = kind == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT;
            let storage = functional
                && values
                    .GetGuidValue(&WPD_FUNCTIONAL_OBJECT_CATEGORY)
                    .is_ok_and(|c| c == WPD_FUNCTIONAL_CATEGORY_STORAGE);
            if functional && !storage {
                return Ok(None);
            }
            let Some(name) =
                text(&WPD_OBJECT_ORIGINAL_FILE_NAME).or_else(|| text(&WPD_OBJECT_NAME))
            else {
                return Ok(None);
            };
            let is_folder = storage || kind == WPD_CONTENT_TYPE_FOLDER;
            let size = if is_folder {
                0
            } else {
                values
                    .GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)
                    .unwrap_or(0)
            };
            let modified = values
                .GetValue(&WPD_OBJECT_DATE_MODIFIED)
                .ok()
                .and_then(|mut value| {
                    let date = (value.Anonymous.Anonymous.vt == VT_DATE)
                        .then(|| value.Anonymous.Anonymous.Anonymous.date);
                    let _ = PropVariantClear(&raw mut value);
                    date.and_then(ole_date)
                });
            Ok(Some(Child {
                id: id.to_owned(),
                object: Object {
                    name,
                    is_folder,
                    size,
                    modified,
                },
                storage,
            }))
        }
    }

    /// Reads the object `id` and hands it to `chunk` piece by piece, at most `max` bytes; stops
    /// when `chunk` returns `false`.
    fn stream(
        &self,
        id: &str,
        max: u64,
        chunk: &mut dyn FnMut(Vec<u8>) -> bool,
    ) -> RemoteResult<()> {
        let id_w = wide(id);
        // SAFETY: COM calls on the WPD thread into buffers of the size given.
        unsafe {
            let resources = self.content.Transfer().map_err(wpd_error)?;
            let mut optimal = 0u32;
            let mut stream: Option<IStream> = None;
            resources
                .GetStream(
                    PCWSTR(id_w.as_ptr()),
                    &WPD_RESOURCE_DEFAULT,
                    STGM_READ.0,
                    &raw mut optimal,
                    &raw mut stream,
                )
                .map_err(wpd_error)?;
            let stream = stream.ok_or(RemoteError::NotFound)?;
            let size = (optimal as usize).clamp(64 * 1024, 1024 * 1024);
            let mut left = max;
            while left > 0 {
                let mut buf = vec![0u8; size.min(usize::try_from(left).unwrap_or(size))];
                let mut read = 0u32;
                let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
                stream
                    .Read(buf.as_mut_ptr().cast(), len, Some(&raw mut read))
                    .ok()
                    .map_err(wpd_error)?;
                if read == 0 {
                    break;
                }
                buf.truncate(read as usize);
                left -= u64::from(read);
                if !chunk(buf) {
                    return Err(RemoteError::Cancelled);
                }
            }
        }
        Ok(())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: closes the device this struct opened.
        let _ = unsafe { self.device.Close() };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A WPD failure as what the user should know.
fn wpd_error(err: windows::core::Error) -> RemoteError {
    let code = err.code();
    if code == E_ACCESSDENIED {
        RemoteError::Locked
    } else if GONE.contains(&code) {
        RemoteError::DeviceGone
    } else {
        RemoteError::Server(err.message())
    }
}

/// An OLE date (days since 1899-12-30, local time, as WPD gives dates) as a point in time.
fn ole_date(days: f64) -> Option<SystemTime> {
    if !days.is_finite() || days < 1.0 {
        return None;
    }
    let local = ole_to_systemtime(days)?;
    let mut utc = SYSTEMTIME::default();
    let mut file = FILETIME::default();
    // SAFETY: plain conversions between stack values.
    unsafe {
        windows::Win32::System::Time::TzSpecificLocalTimeToSystemTime(
            None,
            &raw const local,
            &raw mut utc,
        )
        .ok()?;
        windows::Win32::System::Time::SystemTimeToFileTime(&raw const utc, &raw mut file).ok()?;
    }
    let ticks = (u64::from(file.dwHighDateTime) << 32) | u64::from(file.dwLowDateTime);
    // Before 1970 counts as unknown, as it always did.
    arke_core::time::system_time_from_filetime(ticks).filter(|t| *t >= SystemTime::UNIX_EPOCH)
}

/// The calendar date and time an OLE date stands for.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "days and seconds are checked to be positive and in range"
)]
fn ole_to_systemtime(days: f64) -> Option<SYSTEMTIME> {
    let whole = days.floor();
    let seconds = ((days - whole) * 86_400.0).round() as u32;
    // OLE day 0 is 1899-12-30.
    let date = jiff::civil::date(1899, 12, 30)
        .checked_add(jiff::Span::new().try_days(whole as i64).ok()?)
        .ok()?;
    Some(SYSTEMTIME {
        wYear: u16::try_from(date.year()).ok()?,
        wMonth: u16::from(date.month().cast_unsigned()),
        wDay: u16::from(date.day().cast_unsigned()),
        wHour: u16::try_from((seconds / 3600).min(23)).ok()?,
        wMinute: u16::try_from((seconds / 60) % 60).ok()?,
        wSecond: u16::try_from(seconds % 60).ok()?,
        ..SYSTEMTIME::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ole_dates_become_calendar_dates() {
        // 2026-11-23 12:00 local.
        let t = ole_to_systemtime(46_349.5).unwrap();
        assert_eq!(
            (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute),
            (2026, 11, 23, 12, 0)
        );
        let t = ole_to_systemtime(25_569.0).unwrap();
        assert_eq!((t.wYear, t.wMonth, t.wDay), (1970, 1, 1));
        assert!(ole_date(0.0).is_none());
        assert!(ole_date(f64::NAN).is_none());
    }

    #[test]
    fn a_locked_phone_and_a_pulled_cable_say_so() {
        assert!(matches!(
            wpd_error(windows::core::Error::from(E_ACCESSDENIED)),
            RemoteError::Locked
        ));
        assert!(matches!(
            wpd_error(windows::core::Error::from(E_WPD_DEVICE_NOT_OPEN)),
            RemoteError::DeviceGone
        ));
    }

    /// Lists whatever devices this PC has (none on CI); only checks the calls don't fail.
    #[test]
    fn lists_the_devices_on_this_pc() {
        let wpd = Wpd::new();
        let devices = wpd.devices().unwrap();
        for d in &devices {
            assert!(!d.key.is_empty() && !d.name.is_empty());
        }
    }
}
