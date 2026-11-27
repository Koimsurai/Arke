//! Installs and removes the service. Needs admin; Arke launches it through a UAC prompt.
//!
//! Safeguards:
//! - The binary is copied to `%ProgramFiles%\Arke` (admin-writable only) and the service points
//!   at that copy. A SYSTEM service running from a user-writable path could be swapped out for
//!   privilege escalation.
//! - Demand start only (not at boot); it exits on its own when idle.
//! - The token keeps only `SeChangeNotifyPrivilege`; every other privilege is dropped.
//! - Service ACL: only the users named at install can start it; everyone else can only query.
//! - Uninstall removes the registration and the copied binary.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::proto::SERVICE_NAME;
use crate::sec::{SecurityDescriptor, is_plain_sid};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE, HANDLE,
};
use windows::Win32::Security::DACL_SECURITY_INFORMATION;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, ENUM_SERVICE_TYPE, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfigW, QueryServiceStatus, QueryServiceStatusEx,
    SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SC_STATUS_PROCESS_INFO,
    SERVICE_ALL_ACCESS, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_REQUIRED_PRIVILEGES_INFOW, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOPPED,
    SERVICE_WIN32_OWN_PROCESS, SetServiceObjectSecurity,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
use windows::core::{PCWSTR, PWSTR};

const DISPLAY_NAME: &str = "Arke Index";
const DESCRIPTION: &str = "Lets Arke see file changes on NTFS drives instantly, including \
    changes made while Arke was closed (reads the change journal; never reads file contents). \
    Installed only when you choose it in Arke; remove it there.";

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: self.0 is a service handle owned by this struct and not yet closed.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Why installing or removing the service failed. Display is the text printed by the CLI.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstallError {
    /// `--user` is not a plain SID.
    #[error("not a SID: {user}")]
    NotSid {
        /// The rejected value.
        user: String,
    },
    /// The Program Files folder could not be located.
    #[error("cannot find Program Files")]
    NoProgramFiles,
    /// The service control manager could not be opened.
    #[error("{source}")]
    Manager {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The existing service could not be opened.
    #[error("{source}")]
    OpenService {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The stop request was refused.
    #[error("{source}")]
    Stop {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The service status could not be read while waiting for it to stop.
    #[error("{source}")]
    QueryStatus {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The service was still running after the stop timeout.
    #[error("the service did not stop")]
    StopTimeout,
    /// The running executable's path is unknown.
    #[error("{source}")]
    CurrentExe {
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The install folder could not be created.
    #[error("{}: {source}", .path.display())]
    CreateDir {
        /// The install folder.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The binary could not be copied to the install folder.
    #[error("{}: {source}", .path.display())]
    Copy {
        /// The copy's destination.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The service could not be registered.
    #[error("{source}")]
    Create {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The existing registration could not be updated.
    #[error("{source}")]
    ChangeConfig {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The description could not be set.
    #[error("{source}")]
    Describe {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The required privileges could not be set.
    #[error("{source}")]
    Privileges {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The service ACL could not be built.
    #[error("{source}")]
    Sddl {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The service ACL could not be applied.
    #[error("{source}")]
    SetSecurity {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The registration could not be deleted.
    #[error("{source}")]
    Delete {
        /// Underlying error.
        #[source]
        source: windows::core::Error,
    },
    /// The installed binary could not be removed.
    #[error("{}: {source}", .path.display())]
    RemoveBinary {
        /// The installed binary.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// `%ProgramFiles%\Arke\arke-indexd.exe`. The location comes from the system, not environment
/// variables: users can change those and point a SYSTEM service at their own folder.
pub(crate) fn install_path() -> Option<PathBuf> {
    // SAFETY: the string is allocated by the system and freed with CoTaskMemFree below.
    let raw =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG(0), None) }.ok()?;
    // SAFETY: raw is a valid NUL-terminated string.
    let dir = unsafe { raw.to_string() }.ok();
    // SAFETY: raw was allocated by SHGetKnownFolderPath.
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Some(PathBuf::from(dir?).join("Arke").join("arke-indexd.exe"))
}

fn manager(access: u32) -> Result<Handle, InstallError> {
    // SAFETY: the local service control manager; the handle is owned by Handle.
    unsafe { OpenSCManagerW(None, None, access) }
        .map(Handle)
        .map_err(|source| InstallError::Manager { source })
}

fn open(manager: &Handle) -> Result<Option<Handle>, InstallError> {
    let name = wide(SERVICE_NAME);
    // SAFETY: name is NUL-terminated and valid for the call.
    match unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) } {
        Ok(h) => Ok(Some(Handle(h))),
        Err(err) if err.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => Ok(None),
        Err(source) => Err(InstallError::OpenService { source }),
    }
}

/// How long stopping the service may take, including its process exiting.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// A process handle, closed when dropped.
struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: self.0 is a process handle owned by this struct and not yet closed.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The running service's process, to wait for it to exit.
fn service_process(service: &Handle) -> Option<Process> {
    let mut info = [0u8; size_of::<SERVICE_STATUS_PROCESS>()];
    let mut needed = 0u32;
    // SAFETY: info is a writable buffer of the size passed; needed is a valid out pointer.
    unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(&mut info),
            &raw mut needed,
        )
    }
    .ok()?;
    // SAFETY: the call filled info with a SERVICE_STATUS_PROCESS (read unaligned from bytes).
    let pid = unsafe {
        info.as_ptr()
            .cast::<SERVICE_STATUS_PROCESS>()
            .read_unaligned()
    }
    .dwProcessId;
    if pid == 0 {
        return None;
    }
    // SAFETY: plain Win32 call; the handle is owned by the returned Process.
    unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .ok()
        .map(Process)
}

/// Stops the service and waits until its process has exited. The service reports stopped before
/// that, while its process still has the program file open, so replacing the file right after
/// fails (in use).
fn stop(service: &Handle) -> Result<(), InstallError> {
    let process = service_process(service);
    let mut status = SERVICE_STATUS::default();
    // SAFETY: service is a valid handle; status is a valid out pointer.
    match unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &raw mut status) } {
        Ok(()) => {}
        Err(err) if err.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => return Ok(()),
        Err(source) => return Err(InstallError::Stop { source }),
    }
    let started = Instant::now();
    while started.elapsed() < STOP_TIMEOUT {
        // SAFETY: as above.
        unsafe { QueryServiceStatus(service.0, &raw mut status) }
            .map_err(|source| InstallError::QueryStatus { source })?;
        if status.dwCurrentState == SERVICE_STOPPED {
            if let Some(process) = process {
                let left = STOP_TIMEOUT.saturating_sub(started.elapsed());
                let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
                // SAFETY: the handle has synchronize access and is open.
                let _ = unsafe { WaitForSingleObject(process.0, ms) };
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(InstallError::StopTimeout)
}

/// Runs `op` until it succeeds, retrying for up to 5 seconds while the file is still in use (a
/// process that has just exited, or a virus scanner looking at the file).
fn retry_in_use(mut op: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let started = Instant::now();
    loop {
        match op() {
            Err(e)
                if e.kind() != std::io::ErrorKind::NotFound
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            done => return done,
        }
    }
}

/// Users already allowed, read from the registered command line.
fn allowed_users(service: &Handle) -> Vec<String> {
    let mut needed = 0u32;
    // SAFETY: the first call only asks for the required size.
    let _ = unsafe { QueryServiceConfigW(service.0, None, 0, &raw mut needed) };
    if needed == 0 || needed > 64 * 1024 {
        return Vec::new();
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    let config = buf.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    // SAFETY: buf holds at least `needed` bytes, is suitably aligned and valid for the call.
    if unsafe { QueryServiceConfigW(service.0, Some(config), needed, &raw mut needed) }.is_err() {
        return Vec::new();
    }
    // SAFETY: the call succeeded, so config points to a QUERY_SERVICE_CONFIGW in buf and its
    // string pointers also point into buf.
    let command = unsafe { (*config).lpBinaryPathName.to_string() }.unwrap_or_default();
    parse_allowed(&command)
}

/// The `--allow <SID>` values in a command line.
pub(crate) fn parse_allowed(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut words = command.split_whitespace();
    while let Some(w) = words.next() {
        if w == "--allow"
            && let Some(sid) = words.next()
            && is_plain_sid(sid)
            && !out.iter().any(|s: &String| s == sid)
        {
            out.push(sid.to_string());
        }
    }
    out
}

pub(crate) fn command_line(exe: &Path, allowed: &[String]) -> String {
    // Single exe: `"...\arke-indexd.exe" --indexd run ...` (that file is a copy of arke.exe).
    let mut cmd = match crate::cli::prefix() {
        Some(prefix) => format!("\"{}\" {prefix} run", exe.display()),
        None => format!("\"{}\" run", exe.display()),
    };
    for sid in allowed {
        cmd.push_str(" --allow ");
        cmd.push_str(sid);
    }
    cmd
}

/// Service ACL: full control for SYSTEM and admins, query only for interactive users, and
/// allowed users can also start it.
pub(crate) fn service_sddl(allowed: &[String]) -> String {
    let mut sddl = String::from(
        "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)",
    );
    for sid in allowed {
        // RP = start, LC = query status, CC = query config, LO = interrogate
        let _ = write!(sddl, "(A;;CCLCSWRPLOCRRC;;;{sid})");
    }
    sddl
}

pub(crate) fn install(user: &str) -> Result<(), InstallError> {
    if !is_plain_sid(user) {
        return Err(InstallError::NotSid {
            user: user.to_string(),
        });
    }
    let target = install_path().ok_or(InstallError::NoProgramFiles)?;
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let existing = open(&manager)?;
    let mut allowed = existing.as_ref().map_or_default(allowed_users);
    if !allowed.iter().any(|s| s == user) {
        allowed.push(user.to_string());
    }
    if let Some(service) = &existing {
        stop(service)?;
    }

    // Copy to an admin-only location.
    let me = std::env::current_exe().map_err(|source| InstallError::CurrentExe { source })?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|source| InstallError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    let same = me
        .canonicalize()
        .ok()
        .zip(target.canonicalize().ok())
        .is_some_and(|(a, b)| a == b);
    if !same {
        retry_in_use(|| std::fs::copy(&me, &target).map(|_| ())).map_err(|source| {
            InstallError::Copy {
                path: target.clone(),
                source,
            }
        })?;
    }

    let command = wide(&command_line(&target, &allowed));
    let name = wide(SERVICE_NAME);
    let display = wide(DISPLAY_NAME);
    let service = if let Some(service) = existing {
        {
            // SAFETY: the strings are NUL-terminated and valid for the call; other fields kept.
            unsafe {
                ChangeServiceConfigW(
                    service.0,
                    ENUM_SERVICE_TYPE(SERVICE_WIN32_OWN_PROCESS.0),
                    SERVICE_DEMAND_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(command.as_ptr()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    PCWSTR(display.as_ptr()),
                )
            }
            .map_err(|source| InstallError::ChangeConfig { source })?;
            service
        }
    } else {
        {
            // SAFETY: strings are NUL-terminated and valid for the call; no account = LocalSystem.
            let h = unsafe {
                CreateServiceW(
                    manager.0,
                    PCWSTR(name.as_ptr()),
                    PCWSTR(display.as_ptr()),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_DEMAND_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(command.as_ptr()),
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .map_err(|source| InstallError::Create { source })?;
            Handle(h)
        }
    };

    harden(&service, &allowed)
}

/// Description, privileges and ACL: the service gets only what it needs.
fn harden(service: &Handle, allowed: &[String]) -> Result<(), InstallError> {
    let mut description = wide(DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(description.as_mut_ptr()),
    };
    // SAFETY: info and the string it points to are valid for the call.
    unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some((&raw const info).cast()),
        )
    }
    .map_err(|source| InstallError::Describe { source })?;

    // Keep only the required privilege (the string list ends with two NULs).
    let mut privileges: Vec<u16> = "SeChangeNotifyPrivilege"
        .encode_utf16()
        .chain([0, 0])
        .collect();
    let info = SERVICE_REQUIRED_PRIVILEGES_INFOW {
        pmszRequiredPrivileges: PWSTR(privileges.as_mut_ptr()),
    };
    // SAFETY: info and the string it points to are valid for the call.
    unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
            Some((&raw const info).cast()),
        )
    }
    .map_err(|source| InstallError::Privileges { source })?;

    let sd = SecurityDescriptor::from_sddl(&service_sddl(allowed))
        .map_err(|source| InstallError::Sddl { source })?;
    // SAFETY: service has WRITE_DAC (SERVICE_ALL_ACCESS); sd is valid for the call.
    unsafe { SetServiceObjectSecurity(service.0, DACL_SECURITY_INFORMATION, sd.0) }
        .map_err(|source| InstallError::SetSecurity { source })?;
    Ok(())
}

pub(crate) fn uninstall() -> Result<(), InstallError> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    if let Some(service) = open(&manager)? {
        stop(&service)?;
        // SAFETY: service has DELETE access (SERVICE_ALL_ACCESS).
        unsafe { DeleteService(service.0) }.map_err(|source| InstallError::Delete { source })?;
    }
    if let Some(target) = install_path() {
        let me = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok());
        let running_from_target = me.is_some_and(|me| target.canonicalize().ok() == Some(me));
        if target.exists() && !running_from_target {
            match retry_in_use(|| std::fs::remove_file(&target)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(InstallError::RemoveBinary {
                        path: target,
                        source: e,
                    });
                }
                _ => {}
            }
        }
        // Remove the folder only if empty (the user may have put things there).
        if let Some(dir) = target.parent() {
            let _ = std::fs::remove_file(dir.join("indexd.log"));
            let _ = std::fs::remove_dir(dir);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The update copies over the service's program right after it stops; the file may stay in
    /// use for a moment.
    #[test]
    fn a_file_in_use_for_a_moment_is_still_replaced() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        let target = dir.join("arke-indexd.exe");
        let source = dir.join("new.exe");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&source, b"new").unwrap();
        // Open with no sharing, as a running program's image is.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        assert!(std::fs::copy(&source, &target).is_err(), "in use");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(held);
        });
        retry_in_use(|| std::fs::copy(&source, &target).map(|_| ())).unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        // A missing file is not waited for.
        let started = Instant::now();
        let gone = retry_in_use(|| std::fs::remove_file(dir.join("missing.exe")));
        assert_eq!(gone.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    const ME: &str = "S-1-5-21-1-2-3-1001";
    const YOU: &str = "S-1-5-21-1-2-3-1002";

    #[test]
    fn command_line_round_trips_the_allowed_users() {
        let exe = Path::new(r"C:\Program Files\Arke\arke-indexd.exe");
        let cmd = command_line(exe, &[ME.into(), YOU.into()]);
        assert_eq!(
            cmd,
            format!(r#""C:\Program Files\Arke\arke-indexd.exe" run --allow {ME} --allow {YOU}"#)
        );
        assert_eq!(parse_allowed(&cmd), [ME, YOU]);
        assert_eq!(
            parse_allowed(&format!("x --allow {ME} --allow {ME} --allow WD --allow")),
            [ME],
            "duplicates and non-SIDs are dropped"
        );
    }

    #[test]
    fn service_rules_only_let_allowed_users_start_it() {
        let sddl = service_sddl(&[ME.into()]);
        assert!(sddl.contains(&format!("(A;;CCLCSWRPLOCRRC;;;{ME})")));
        // Interactive users lack RP (start).
        assert!(sddl.contains("(A;;CCLCSWLOCRRC;;;IU)"));
        assert!(SecurityDescriptor::from_sddl(&sddl).is_ok());
    }

    #[test]
    fn installs_under_program_files() {
        let p = install_path().unwrap();
        assert!(p.ends_with(r"Arke\arke-indexd.exe"));
        assert!(p.starts_with(std::env::var("ProgramFiles").unwrap()));
    }
}
