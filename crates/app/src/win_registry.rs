//! The real per-user registry behind `arke_core::folder_handler::Registry`.
//!
//! Only `HKEY_CURRENT_USER` is ever opened, and every call is refused for keys outside
//! `Software\Classes` (the registration logic checks too; this is the second lock). Nothing here
//! runs unless the user turned on "Open folders in Arke" or ran `arke --restore-explorer`.

use arke_core::folder_handler::{CLASSES, Error, RegValue, Registry};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPEN_CREATE_OPTIONS, REG_VALUE_TYPE,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

/// `HKEY_CURRENT_USER`, through the Win32 API.
pub(crate) struct UserRegistry;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn allowed(key: &str) -> Result<(), Error> {
    let prefix = format!(r"{CLASSES}\");
    if key.len() > prefix.len()
        && key.is_char_boundary(prefix.len())
        && key[..prefix.len()].eq_ignore_ascii_case(&prefix)
        && !key.contains("..")
    {
        Ok(())
    } else {
        Err(Error::Outside(key.to_owned()))
    }
}

fn failed(what: &str, key: &str, code: windows::Win32::Foundation::WIN32_ERROR) -> Error {
    Error::Registry(format!("{what} {key}: error {}", code.0))
}

/// An open key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: self.0 is an open key owned by this struct.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Opens `key` under HKCU; `None` when it does not exist.
fn open(
    key: &str,
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> Result<Option<Key>, Error> {
    allowed(key)?;
    let name = wide(key);
    let mut handle = HKEY::default();
    // SAFETY: name is NUL-terminated and valid for the call; handle is a valid out pointer.
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(name.as_ptr()),
            None,
            access,
            &raw mut handle,
        )
    };
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
        Ok(None)
    } else if code == ERROR_SUCCESS {
        Ok(Some(Key(handle)))
    } else {
        Err(failed("open", key, code))
    }
}

impl Registry for UserRegistry {
    fn read(&self, key: &str, value: &str) -> Result<Option<RegValue>, Error> {
        let Some(open) = open(key, KEY_READ)? else {
            return Ok(None);
        };
        let name = wide(value);
        let mut kind = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        // SAFETY: first call only asks for the size; all pointers are valid for the call.
        let code = unsafe {
            RegQueryValueExW(
                open.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&raw mut kind),
                None,
                Some(&raw mut size),
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code != ERROR_SUCCESS {
            return Err(failed("query", key, code));
        }
        let mut data = vec![0u8; size as usize];
        // SAFETY: data has `size` writable bytes; the pointers are valid for the call.
        let code = unsafe {
            RegQueryValueExW(
                open.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&raw mut kind),
                Some(data.as_mut_ptr()),
                Some(&raw mut size),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(failed("read", key, code));
        }
        data.truncate(size as usize);
        Ok(Some(RegValue { kind: kind.0, data }))
    }

    fn key_exists(&self, key: &str) -> Result<bool, Error> {
        Ok(open(key, KEY_READ)?.is_some())
    }

    fn write(&self, key: &str, value: &str, data: &RegValue) -> Result<(), Error> {
        allowed(key)?;
        let name = wide(key);
        let mut handle = HKEY::default();
        // SAFETY: name is NUL-terminated and valid for the call; handle is a valid out pointer.
        let code = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(name.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPEN_CREATE_OPTIONS(0),
                KEY_WRITE | KEY_READ,
                None,
                &raw mut handle,
                None,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(failed("create", key, code));
        }
        let open = Key(handle);
        let value_name = wide(value);
        // SAFETY: the value name is NUL-terminated; the data slice is valid for the call.
        let code = unsafe {
            RegSetValueExW(
                open.0,
                PCWSTR(value_name.as_ptr()),
                None,
                REG_VALUE_TYPE(data.kind),
                Some(&data.data),
            )
        };
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(failed("write", key, code))
        }
    }

    fn delete_value(&self, key: &str, value: &str) -> Result<(), Error> {
        let Some(open) = open(key, KEY_WRITE)? else {
            return Ok(());
        };
        let name = wide(value);
        // SAFETY: the value name is NUL-terminated and valid for the call.
        let code = unsafe { RegDeleteValueW(open.0, PCWSTR(name.as_ptr())) };
        if code == ERROR_SUCCESS || code == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(failed("delete value", key, code))
        }
    }

    fn delete_key_if_empty(&self, key: &str) -> Result<bool, Error> {
        let Some(open) = open(key, KEY_READ)? else {
            return Ok(true);
        };
        let (mut subkeys, mut values) = (0u32, 0u32);
        // SAFETY: only the two counters are requested; the pointers are valid for the call.
        let code = unsafe {
            RegQueryInfoKeyW(
                open.0,
                None,
                None,
                None,
                Some(&raw mut subkeys),
                None,
                None,
                Some(&raw mut values),
                None,
                None,
                None,
                None,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(failed("inspect", key, code));
        }
        if subkeys != 0 || values != 0 {
            return Ok(false);
        }
        drop(open);
        let name = wide(key);
        // SAFETY: the key name is NUL-terminated; RegDeleteKeyW fails on keys with subkeys.
        let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(name.as_ptr())) };
        if code == ERROR_SUCCESS || code == ERROR_FILE_NOT_FOUND {
            Ok(true)
        } else {
            Err(failed("delete key", key, code))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard only; the real registry is never touched by tests.
    #[test]
    fn refuses_keys_outside_software_classes() {
        assert!(allowed(r"Software\Classes\Directory\shell").is_ok());
        assert!(allowed(r"Software\Microsoft\Windows\CurrentVersion\Run").is_err());
        assert!(allowed(r"Software\Classes").is_err());
        assert!(allowed(r"Software\Classes\..\Run").is_err());
        assert!(allowed("").is_err());
    }
}
