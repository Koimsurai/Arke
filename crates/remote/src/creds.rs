//! Saved passwords: Windows Credential Manager, generic credential `Arke:<host>`, written only
//! when the user ticks "Remember". Never in settings or any file of Arke's.

use arke_core::remote::RemoteUrl;

/// Where remembered secrets live.
pub trait SecretStore: Send + Sync + 'static {
    /// The secret saved for this server and user.
    fn load(&self, url: &RemoteUrl) -> Option<String>;
    /// Saves (replaces) the secret; failures are only logged.
    fn save(&self, url: &RemoteUrl, secret: &str);
    /// Removes a saved secret.
    fn forget(&self, url: &RemoteUrl);
}

/// Remembers nothing (tests, benchmarks).
pub struct NoSecrets;

impl SecretStore for NoSecrets {
    fn load(&self, _: &RemoteUrl) -> Option<String> {
        None
    }
    fn save(&self, _: &RemoteUrl, _: &str) {}
    fn forget(&self, _: &RemoteUrl) {}
}

/// Credential Manager target name.
pub fn target_name(url: &RemoteUrl) -> String {
    format!("Arke:{}", url.host)
}

/// Prefix of every target name Arke writes (`Arke:<host>`).
pub const TARGET_PREFIX: &str = "Arke:";

/// Whether a Credential Manager target name is one of Arke's (case as Windows compares them).
#[must_use]
pub fn is_arke_target(name: &str) -> bool {
    name.get(..TARGET_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(TARGET_PREFIX))
        && name.len() > TARGET_PREFIX.len()
}

/// Deletes every saved Arke password (uninstall). Returns how many were removed.
///
/// # Errors
/// Listing the saved credentials failed (other than there being none).
#[cfg(windows)]
pub fn forget_all() -> windows::core::Result<usize> {
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CRED_ENUMERATE_FLAGS, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree,
    };
    use windows::core::PCWSTR;
    let filter = wide(&format!("{TARGET_PREFIX}*"));
    let mut count = 0u32;
    let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: filter is NUL-terminated; on success `list` points to `count` entries freed below.
    let listed = unsafe {
        CredEnumerateW(
            PCWSTR(filter.as_ptr()),
            Some(CRED_ENUMERATE_FLAGS(0)),
            &raw mut count,
            &raw mut list,
        )
    };
    match listed {
        Ok(()) => {}
        Err(err) if err.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(0),
        Err(err) => return Err(err),
    }
    let mut targets = Vec::new();
    // SAFETY: CredEnumerateW succeeded: `list` holds `count` valid pointers until CredFree.
    unsafe {
        for i in 0..count as usize {
            let cred = &**list.add(i);
            if cred.Type == CRED_TYPE_GENERIC
                && let Ok(name) = cred.TargetName.to_string()
                && is_arke_target(&name)
            {
                targets.push(name);
            }
        }
        CredFree(list.cast());
    }
    let mut removed = 0;
    for name in targets {
        let target = wide(&name);
        // SAFETY: target is NUL-terminated.
        if unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) }.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Windows Credential Manager.
#[cfg(windows)]
pub struct CredentialManager;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
impl SecretStore for CredentialManager {
    fn load(&self, url: &RemoteUrl) -> Option<String> {
        use windows::Win32::Security::Credentials::{
            CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
        };
        use windows::core::PCWSTR;
        let target = wide(&target_name(url));
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: target is NUL-terminated; on success `found` points to a buffer we free below.
        unsafe {
            CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &raw mut found,
            )
        }
        .ok()?;
        // SAFETY: CredReadW succeeded, so `found` is a valid CREDENTIALW until CredFree.
        unsafe {
            let cred = &*found;
            let user = cred.UserName.to_string().unwrap_or_default();
            let blob =
                std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize);
            let units: Vec<u16> = blob
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let secret = String::from_utf16(&units).ok();
            CredFree(found.cast());
            // Another account on the same server: not ours to use.
            secret.filter(|_| url.user.as_deref() == Some(user.as_str()))
        }
    }

    fn save(&self, url: &RemoteUrl, secret: &str) {
        use windows::Win32::Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
        };
        use windows::core::PWSTR;
        let Some(user) = &url.user else { return };
        let mut target = wide(&target_name(url));
        let mut name = wide(user);
        let mut blob: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: u32::try_from(blob.len()).unwrap_or(0),
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(name.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: the buffers credential points to are valid for the call.
        if let Err(err) = unsafe { CredWriteW(&raw const credential, 0) } {
            log::warn!(
                "Credential Manager did not save credentials for {}: {err}",
                url.host
            );
        }
        blob.fill(0);
    }

    fn forget(&self, url: &RemoteUrl) {
        use windows::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};
        use windows::core::PCWSTR;
        if self.load(url).is_none() {
            return;
        }
        let target = wide(&target_name(url));
        // SAFETY: target is NUL-terminated.
        let _ = unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::is_arke_target;

    #[test]
    fn only_arke_targets_are_forgotten() {
        assert!(is_arke_target("Arke:nas.local"));
        assert!(is_arke_target("arke:files.example.com"));
        assert!(!is_arke_target("Arke:"));
        assert!(!is_arke_target("Arkey:host"));
        assert!(!is_arke_target("git:https://github.com"));
        assert!(!is_arke_target("Ark"));
    }
}
