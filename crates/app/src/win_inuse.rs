//! Which programs have a file or folder open, so a failed delete can say "open in Word (PID 1234)"
//! instead of "access denied", like PowerToys File Locksmith.
//!
//! - Files: the Restart Manager (`rstrtmgr.dll`), the API installers use to find what holds a
//!   file. It needs no admin rights for the user's own processes and gives friendly app names.
//! - Folders (open as another program's current directory, or holding a locked file): the
//!   Restart Manager only takes files, so the folder's own handles are found with
//!   `FileProcessIdsUsingFileInformation`, and files inside it (up to a limit) are checked too.
//!   Process ids found that way are registered with the same Restart Manager session to get
//!   their names.
//!
//! Only reads: nothing is closed or stopped. Can take a moment (it walks the folder), so call
//! from a background thread.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, FILETIME, HANDLE, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RM_UNIQUE_PROCESS, RmEndSession, RmGetList,
    RmRegisterResources, RmStartSession,
};
use windows::core::{PCWSTR, PWSTR};

use crate::shell::Holder;

/// Files inside a folder checked at most, so a huge tree doesn't stall the report.
const MAX_FILES: usize = 400;
/// Folders inside a folder whose own handles are checked at most.
const MAX_DIRS: usize = 100;

/// Restart Manager session, ended on drop.
struct Session(u32);

impl Session {
    fn start() -> Result<Self, windows::core::Error> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: handle and key are valid out buffers; key has room for the session key and NUL.
        unsafe { RmStartSession(&raw mut handle, None, PWSTR(key.as_mut_ptr())) }.ok()?;
        Ok(Self(handle))
    }

    /// Registers files (full paths) and processes to ask about.
    fn register(
        &self,
        files: &[PathBuf],
        processes: &[RM_UNIQUE_PROCESS],
    ) -> Result<(), windows::core::Error> {
        let wide: Vec<Vec<u16>> = files
            .iter()
            .map(|p| p.as_os_str().encode_wide().chain(Some(0)).collect())
            .collect();
        let names: Vec<PCWSTR> = wide.iter().map(|w| PCWSTR(w.as_ptr())).collect();
        // SAFETY: names point into `wide`, which outlives the call; slices carry their lengths.
        unsafe {
            RmRegisterResources(
                self.0,
                (!names.is_empty()).then_some(names.as_slice()),
                (!processes.is_empty()).then_some(processes),
                None,
            )
        }
        .ok()
    }

    /// Everything the registered files and processes map to.
    fn list(&self) -> Result<Vec<RM_PROCESS_INFO>, windows::core::Error> {
        let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
        // The list can grow between calls; a few tries are plenty.
        for _ in 0..4 {
            let mut needed = 0u32;
            let mut count = u32::try_from(infos.len()).unwrap_or(u32::MAX);
            let mut reasons = 0u32;
            // SAFETY: infos has room for `count` entries; the out pointers are valid.
            let result: WIN32_ERROR = unsafe {
                RmGetList(
                    self.0,
                    &raw mut needed,
                    &raw mut count,
                    (!infos.is_empty()).then_some(infos.as_mut_ptr()),
                    &raw mut reasons,
                )
            };
            if result == ERROR_MORE_DATA {
                infos.resize(needed as usize + 2, RM_PROCESS_INFO::default());
                continue;
            }
            result.ok()?;
            infos.truncate(count as usize);
            return Ok(infos);
        }
        Err(ERROR_MORE_DATA.into())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: ends the session started in `start`.
        let _ = unsafe { RmEndSession(self.0) };
    }
}

/// Programs that have `path` (a file, or a folder and what is inside it) open. Empty when nothing
/// is found or the lookup itself fails: the caller then says "open in another program".
pub(crate) fn holders(path: &Path) -> Vec<Holder> {
    match lookup(path) {
        Ok(found) => found,
        Err(err) => {
            log::info!("in-use lookup for {} failed: {err}", path.display());
            Vec::new()
        }
    }
}

fn lookup(path: &Path) -> Result<Vec<Holder>, windows::core::Error> {
    let meta = std::fs::symlink_metadata(path)?;
    let (files, pids) = if meta.is_dir() && !meta.file_type().is_symlink() {
        let (files, dirs) = walk(path);
        let pids: Vec<u32> = dirs.iter().flat_map(|d| pids_using(d)).collect();
        (files, pids)
    } else {
        (vec![path.to_path_buf()], Vec::new())
    };
    let session = Session::start()?;
    let processes: Vec<RM_UNIQUE_PROCESS> = pids.iter().filter_map(|&pid| unique(pid)).collect();
    session.register(&files, &processes)?;
    let mut found: Vec<Holder> = Vec::new();
    for info in session.list()? {
        let pid = info.Process.dwProcessId;
        if found.iter().any(|h| h.pid == pid) {
            continue;
        }
        found.push(Holder {
            app: app_name(&info).unwrap_or_else(|| exe_name(pid).unwrap_or_default()),
            pid,
        });
    }
    // Processes the session didn't name (e.g. they exited meanwhile, or are protected).
    for pid in pids {
        if !found.iter().any(|h| h.pid == pid)
            && let Some(app) = exe_name(pid)
        {
            found.push(Holder { app, pid });
        }
    }
    Ok(found)
}

/// The folder itself plus files and subfolders below it, depth first, up to the limits. Links
/// are not followed.
fn walk(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut next = 0;
    while next < dirs.len() && files.len() < MAX_FILES {
        let Ok(entries) = std::fs::read_dir(&dirs[next]) else {
            next += 1;
            continue;
        };
        next += 1;
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && !kind.is_symlink() {
                if dirs.len() < MAX_DIRS {
                    dirs.push(entry.path());
                }
            } else if files.len() < MAX_FILES {
                files.push(entry.path());
            }
        }
    }
    (files, dirs)
}

/// Process ids with an open handle to `path` (works for folders, which the Restart Manager can't
/// take).
fn pids_using(path: &Path) -> Vec<u32> {
    use windows::Wdk::Storage::FileSystem::{
        FileProcessIdsUsingFileInformation, NtQueryInformationFile,
    };
    use windows::Wdk::System::SystemServices::FILE_PROCESS_IDS_USING_FILE_INFORMATION;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32.cast_signed();

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: wide is NUL-terminated; attributes-only access with full sharing disturbs no one.
    let Ok(handle) = (unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }) else {
        return Vec::new();
    };
    let handle = OwnedHandle(handle);
    // usize-aligned buffer: a count, then one pointer-sized id per process.
    let mut buf: Vec<usize> = vec![0; 64];
    let offset = std::mem::offset_of!(FILE_PROCESS_IDS_USING_FILE_INFORMATION, ProcessIdList)
        / size_of::<usize>();
    for _ in 0..4 {
        let mut status = IO_STATUS_BLOCK::default();
        let bytes = u32::try_from(buf.len() * size_of::<usize>()).unwrap_or(u32::MAX);
        // SAFETY: buf is writable for `bytes` bytes and suitably aligned; the handle is open.
        let result = unsafe {
            NtQueryInformationFile(
                handle.0,
                &raw mut status,
                buf.as_mut_ptr().cast(),
                bytes,
                FileProcessIdsUsingFileInformation,
            )
        };
        if result.0 == STATUS_INFO_LENGTH_MISMATCH {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if result.is_err() {
            return Vec::new();
        }
        // SAFETY: on success the buffer starts with a FILE_PROCESS_IDS_USING_FILE_INFORMATION.
        let count = unsafe {
            buf.as_ptr()
                .cast::<FILE_PROCESS_IDS_USING_FILE_INFORMATION>()
                .read()
                .NumberOfProcessIdsInList
        } as usize;
        let own = std::process::id();
        return buf
            .iter()
            .skip(offset)
            .take(count)
            .filter_map(|&id| u32::try_from(id).ok())
            // This lookup's handle, and Arke's own folder watchers (they allow deleting).
            .filter(|&id| id != own)
            .collect();
    }
    Vec::new()
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: query-only access; the handle is closed by OwnedHandle.
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .ok()
        .map(OwnedHandle)
}

/// Process id plus start time, which the Restart Manager needs to tell reused ids apart.
fn unique(pid: u32) -> Option<RM_UNIQUE_PROCESS> {
    use windows::Win32::System::Threading::GetProcessTimes;
    let process = open_process(pid)?;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the handle is open with query access; the out pointers are valid.
    unsafe {
        GetProcessTimes(
            process.0,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .ok()?;
    Some(RM_UNIQUE_PROCESS {
        dwProcessId: pid,
        ProcessStartTime: created,
    })
}

/// The program's file name ("cmd.exe").
fn exe_name(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{PROCESS_NAME_WIN32, QueryFullProcessImageNameW};
    let process = open_process(pid)?;
    let mut buf = [0u16; 1024];
    let mut len = u32::try_from(buf.len()).unwrap_or(0);
    // SAFETY: buf is writable for `len` characters; the handle is open with query access.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
    }
    .ok()?;
    let full = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    full.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Friendly name from the Restart Manager: the app name, or the service name for services.
fn app_name(info: &RM_PROCESS_INFO) -> Option<String> {
    let text = |units: &[u16]| {
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let s = String::from_utf16_lossy(&units[..end]);
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_owned())
    };
    text(&info.strAppName).or_else(|| text(&info.strServiceShortName))
}

/// Test helpers: another process holding a file, as an editor would.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// A child process, killed on drop (only ever the one this test started).
    pub(crate) struct Helper(pub(crate) Child);

    impl Drop for Helper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// PowerShell holding `path` open with no sharing, like an editor that locks its file.
    pub(crate) fn lock_in_other_process(path: &Path) -> Helper {
        use std::io::Read;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = format!(
            "$f = [System.IO.File]::Open('{}', 'Open', 'Read', 'None'); 'ready'; Start-Sleep 60",
            path.display()
        );
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let mut ready = [0u8; 5];
        child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
        Helper(child)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Helper, lock_in_other_process};
    use super::*;
    use std::process::{Command, Stdio};

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = crate::win::long_temp_dir()
                .join(format!("arke-inuse-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            if self.0.starts_with(crate::win::long_temp_dir()) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn names_the_process_that_locks_a_file() {
        let sb = Sandbox::new("file");
        let locked = sb.0.join("locked.txt");
        let free = sb.0.join("free.txt");
        std::fs::write(&locked, "x").unwrap();
        std::fs::write(&free, "x").unwrap();
        let helper = lock_in_other_process(&locked);
        let pid = helper.0.id();

        let found = holders(&locked);
        assert!(
            found.iter().any(|h| h.pid == pid && !h.app.is_empty()),
            "{found:?} should include PID {pid}"
        );
        assert!(holders(&free).is_empty(), "nobody has free.txt open");
        // A folder with the locked file inside reports the same process.
        assert!(holders(&sb.0).iter().any(|h| h.pid == pid));

        drop(helper);
        // Once the program is gone, so is the lock.
        assert!(!holders(&locked).iter().any(|h| h.pid == pid));
    }

    #[test]
    fn names_the_process_whose_current_folder_it_is() {
        use std::os::windows::process::CommandExt;
        let sb = Sandbox::new("cwd");
        let held = sb.0.join("held");
        std::fs::create_dir_all(&held).unwrap();
        // One process (no grandchild left behind), its current folder the one to delete.
        let child = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep 60",
            ])
            .current_dir(&held)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let pid = child.id();
        let helper = Helper(child);
        // Give it a moment to start.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut found = Vec::new();
        while std::time::Instant::now() < deadline {
            found = holders(&held);
            if found.iter().any(|h| h.pid == pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(found.iter().any(|h| h.pid == pid), "{found:?}");
        drop(helper);
    }

    #[test]
    fn missing_items_have_no_holders() {
        let sb = Sandbox::new("missing");
        assert!(holders(&sb.0.join("nope.txt")).is_empty());
    }
}
