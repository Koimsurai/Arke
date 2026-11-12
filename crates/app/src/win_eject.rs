//! Ejects removable drives, same as "Safely Remove Hardware" in the taskbar.
//!
//! Drive letter -> disk device (matched by `IOCTL_STORAGE_GET_DEVICE_NUMBER` against the
//! devices SetupAPI lists) -> its parent (the USB mass storage device) ->
//! `CM_Request_Device_EjectW`. If a program still has files open, Windows vetoes and names
//! it; Arke reports that and never forces the eject.

use std::path::Path;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Parent, CM_Request_Device_EjectW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    PNP_VETO_TYPE, PNP_VetoTypeUnknown, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BusType1394, BusTypeMmc, BusTypeSd, BusTypeUsb, CreateFileW, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    GUID_DEVINTERFACE_CDROM, GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows::core::PCWSTR;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_CDROM: u32 = 5;
/// `STORAGE_DEVICE_NUMBER::DeviceType` for optical drives.
const FILE_DEVICE_CD_ROM: u32 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Query-only handle. Without read/write access it needs no admin rights and doesn't
/// block the eject.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: valid handle owned by this struct and not yet closed.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn open(path: &str) -> Option<Handle> {
    let name = wide(path);
    // SAFETY: name is NUL-terminated and outlives the call.
    unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()
    .map(Handle)
}

/// `E:\` → `E`
fn letter(root: &Path) -> Option<char> {
    let s = root.to_str()?;
    let mut chars = s.chars();
    let c = chars.next()?.to_ascii_uppercase();
    (c.is_ascii_uppercase() && matches!(chars.as_str(), ":" | ":\\")).then_some(c)
}

fn device_number(handle: &Handle) -> Option<STORAGE_DEVICE_NUMBER> {
    let mut number = STORAGE_DEVICE_NUMBER::default();
    let mut returned = 0u32;
    // SAFETY: the output is a correctly sized STORAGE_DEVICE_NUMBER; synchronous call.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some((&raw mut number).cast()),
            size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&raw mut returned),
            None,
        )
    }
    .ok()?;
    Some(number)
}

/// External bus (USB, 1394, SD/MMC). Explorer offers Eject for fixed disks on these too.
fn on_external_bus(handle: &Handle) -> bool {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        ..STORAGE_PROPERTY_QUERY::default()
    };
    // Vendor strings follow the descriptor, so the buffer is oversized; only the fixed
    // header is read.
    let mut buf = [0u64; 128];
    let mut returned = 0u32;
    // SAFETY: input and output buffers are valid for the call with correct sizes; synchronous.
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr().cast()),
            size_of_val(&buf) as u32,
            Some(&raw mut returned),
            None,
        )
    }
    .is_ok();
    if !ok || (returned as usize) < size_of::<STORAGE_DEVICE_DESCRIPTOR>() - 1 {
        return false;
    }
    // SAFETY: the buffer is 8-aligned and holds a full STORAGE_DEVICE_DESCRIPTOR (length
    // checked above).
    let desc = unsafe { &*buf.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>() };
    [BusTypeUsb, BusType1394, BusTypeSd, BusTypeMmc].contains(&desc.BusType)
}

/// Whether the drive can be ejected: removable, optical, or a disk on an external bus.
/// Cheap enough to call while opening a menu.
pub(crate) fn is_ejectable(root: &Path) -> bool {
    let Some(c) = letter(root) else {
        return false;
    };
    let path = wide(&format!("{c}:\\"));
    // SAFETY: path is NUL-terminated; only queries the drive type.
    match unsafe { GetDriveTypeW(PCWSTR(path.as_ptr())) } {
        DRIVE_REMOVABLE | DRIVE_CDROM => true,
        DRIVE_FIXED => open(&format!(r"\\.\{c}:")).is_some_and(|h| on_external_bus(&h)),
        _ => false,
    }
}

/// Which physical device holds the drive letter (`disk2`, `cdrom0`), so jobs on two partitions of
/// one disk count as the same drive. `None` when Windows doesn't say (spanned or virtual volumes).
/// Query-only, no admin rights; background thread.
pub(crate) fn physical_device(letter: char) -> Option<String> {
    let number =
        open(&format!(r"\\.\{}:", letter.to_ascii_uppercase())).and_then(|h| device_number(&h))?;
    let kind = if number.DeviceType == FILE_DEVICE_CD_ROM {
        "cdrom"
    } else {
        "disk"
    };
    Some(format!("{kind}{}", number.DeviceNumber))
}

/// Finds the disk device with the same device number and returns its devinst.
fn disk_devinst(number: &STORAGE_DEVICE_NUMBER) -> Option<u32> {
    let class = if number.DeviceType == FILE_DEVICE_CD_ROM {
        GUID_DEVINTERFACE_CDROM
    } else {
        GUID_DEVINTERFACE_DISK
    };
    // SAFETY: class outlives the call; the set is freed below with SetupDiDestroyDeviceInfoList.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&raw const class),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }
    .ok()?;
    let mut found = None;
    for index in 0.. {
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..SP_DEVICE_INTERFACE_DATA::default()
        };
        // SAFETY: set is valid; iface.cbSize is set.
        if unsafe {
            SetupDiEnumDeviceInterfaces(set, None, &raw const class, index, &raw mut iface)
        }
        .is_err()
        {
            break;
        }
        // Detail data: a 4-byte cbSize (always the struct size) followed by the UTF-16
        // device path.
        let mut buf = [0u64; 256];
        let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: buf is large enough and aligned; the API requires cbSize to be the struct size.
        unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..SP_DEVINFO_DATA::default()
        };
        // SAFETY: all pointers are valid for the call; the buffer size is accurate.
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &raw const iface,
                Some(detail),
                size_of_val(&buf) as u32,
                None,
                Some(&raw mut info),
            )
        }
        .is_ok();
        if !ok {
            continue;
        }
        // SAFETY: on success DevicePath is a NUL-terminated string entirely inside buf.
        let path = unsafe {
            let start = (&raw const (*detail).DevicePath).cast::<u16>();
            let max = (size_of_val(&buf) - 4) / 2;
            let units = std::slice::from_raw_parts(start, max);
            let end = units.iter().position(|&u| u == 0).unwrap_or(max);
            String::from_utf16_lossy(&units[..end])
        };
        let same = open(&path)
            .and_then(|h| device_number(&h))
            .is_some_and(|n| n.DeviceNumber == number.DeviceNumber);
        if same {
            found = Some(info.DevInst);
            break;
        }
    }
    // SAFETY: set was obtained above and not yet freed.
    let _ = unsafe { SetupDiDestroyDeviceInfoList(set) };
    found
}

/// Why a drive could not be ejected. Display is the localized reason for the notice.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EjectError {
    /// The path is not a drive root.
    #[error("{}", rust_i18n::t!("eject.not_drive"))]
    NotDrive,
    /// The drive's device could not be found.
    #[error("{}", rust_i18n::t!("eject.not_found"))]
    NotFound,
    /// Windows refused because the drive is in use, without naming who holds it.
    #[error("{}", rust_i18n::t!("eject.busy"))]
    Busy,
    /// Windows refused; `name` is the program or device it named as holding the drive.
    #[error("{}", rust_i18n::t!("eject.busy_by", name = .name))]
    BusyBy {
        /// What Windows reported as holding the drive.
        name: String,
    },
}

/// Asks Windows to eject the device holding `root`. When vetoed, returns the program or
/// device Windows names. Call from a background thread: Windows first asks every program
/// that has it open.
pub(crate) fn eject_drive(root: &Path) -> Result<(), EjectError> {
    let c = letter(root).ok_or(EjectError::NotDrive)?;
    let number = open(&format!(r"\\.\{c}:"))
        .and_then(|h| device_number(&h))
        .ok_or(EjectError::NotFound)?;
    let disk = disk_devinst(&number).ok_or(EjectError::NotFound)?;
    // Eject the parent (USB mass storage device); optical drives can eject themselves.
    let mut parent = 0u32;
    // SAFETY: parent is a valid out variable.
    let target = if unsafe { CM_Get_Parent(&raw mut parent, disk, 0) } == CR_SUCCESS {
        parent
    } else {
        disk
    };
    let mut last = String::new();
    // Recently closed file handles may not be released yet; try up to three times.
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let mut veto = PNP_VETO_TYPE::default();
        let mut name = [0u16; 260];
        // SAFETY: veto and name are valid output buffers.
        let result =
            unsafe { CM_Request_Device_EjectW(target, Some(&raw mut veto), Some(&mut name), 0) };
        if result == CR_SUCCESS && veto == PNP_VetoTypeUnknown {
            return Ok(());
        }
        let end = name.iter().position(|&u| u == 0).unwrap_or(name.len());
        last = String::from_utf16_lossy(&name[..end]);
    }
    Err(if last.is_empty() {
        EjectError::Busy
    } else {
        EjectError::BusyBy { name: last }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_drive_roots_and_never_the_system_drive() {
        assert_eq!(letter(Path::new(r"E:\")), Some('E'));
        assert_eq!(letter(Path::new("e:")), Some('E'));
        assert_eq!(letter(Path::new(r"E:\folder")), None);
        assert_eq!(letter(Path::new(r"\\server\share")), None);
        // The system drive is internal.
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(!is_ejectable(Path::new(&format!("{system}\\"))));
        assert!(!is_ejectable(Path::new(r"\\server\share")));
    }
}
