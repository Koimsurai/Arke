//! Small Windows security helpers: handles, SIDs, security descriptors.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// Owns a HANDLE and closes it on drop.
pub struct OwnedHandle(pub HANDLE);

// SAFETY: a HANDLE is just a kernel object reference and can be used from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; ReadFile, WriteFile and CancelIoEx on one handle may run on different threads.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 is a valid handle owned by this struct and not yet closed.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Accepts only purely numeric SID strings like `S-1-5-21-...`. The value goes into SDDL and a
/// command line, so no other characters may slip in (they could rewrite the ACL).
pub fn is_plain_sid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("S-1-") else {
        return false;
    };
    !rest.is_empty()
        && s.len() <= 184
        && rest
            .split('-')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The token's user SID as a string.
pub fn token_user_sid(token: HANDLE) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: the first call only asks for the required size.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &raw mut len) };
    if len == 0 || len > 64 * 1024 {
        return None;
    }
    // u64 alignment so TOKEN_USER can be read in place.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: buf holds at least len bytes, is suitably aligned and valid for the call.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &raw mut len,
        )
    }
    .ok()?;
    // SAFETY: on success buf starts with a TOKEN_USER whose pointers point into buf.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    // SAFETY: user.User.Sid is a valid SID; text is allocated by the system and freed below.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) }.ok()?;
    // SAFETY: text is a system-allocated NUL-terminated string.
    let sid = unsafe { text.to_string() }.ok();
    // SAFETY: text was allocated by ConvertSidToStringSidW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    sid.filter(|s| is_plain_sid(s))
}

/// The current process's user SID.
pub fn current_user_sid() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo handle; token is a valid out pointer.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
    let token = OwnedHandle(token);
    token_user_sid(token.0)
}

/// Security descriptor built from an SDDL string, freed on drop.
pub struct SecurityDescriptor(pub PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    /// Converts an SDDL string (e.g. `D:P(A;;GA;;;SY)`) to a self-relative descriptor.
    pub fn from_sddl(sddl: &str) -> windows::core::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: wide is NUL-terminated and valid for the call; sd is system-allocated and
        // LocalFree'd on drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &raw mut sd,
                None,
            )
        }?;
        Ok(Self(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: self.0 was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_sids_get_through() {
        assert!(is_plain_sid("S-1-5-21-1004336348-1177238915-682003330-512"));
        assert!(is_plain_sid("S-1-5-18"));
        assert!(!is_plain_sid(""));
        assert!(!is_plain_sid("S-1-"));
        assert!(!is_plain_sid("S-1-5--18"));
        assert!(!is_plain_sid("S-1-5-18)(A;;GA;;;WD"), "no SDDL injection");
        assert!(
            !is_plain_sid("S-1-5-18 --allow S-1-1-0"),
            "no command-line injection"
        );
        assert!(!is_plain_sid("WD"));
    }

    #[test]
    fn reads_the_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(is_plain_sid(&sid), "{sid}");
    }

    #[test]
    fn builds_descriptors_from_sddl() {
        assert!(SecurityDescriptor::from_sddl("D:P(A;;GA;;;SY)").is_ok());
        assert!(SecurityDescriptor::from_sddl("not sddl").is_err());
    }
}
