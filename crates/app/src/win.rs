//! Windows implementation: known folders, drive letters, ShellExecuteExW, time formatting.

use rust_i18n::t;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, HINSTANCE, SYSTEMTIME};
use windows::Win32::Globalization::GetUserDefaultLocaleName;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, QueryDosDeviceW,
};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
};
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Profile, FOLDERID_Videos, KF_FLAG_DONT_VERIFY, SEE_MASK_FLAG_HINST_IS_SITE,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHGetKnownFolderPath, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{GUID, Interface, PCWSTR};

use crate::shell::{
    ClipboardFiles, CloudPin, FileOp, KnownFolder, OpOutcome, Place, PlaceKind, Shell, ShellError,
};
use crate::win_fileops;

/// Lowers this thread's I/O, CPU and memory priority so index builds don't compete for the disk.
pub(crate) fn lower_io_priority() {
    use windows::Win32::System::Threading::{
        SetThreadInformation, THREAD_POWER_THROTTLING_CURRENT_VERSION,
        THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
        THREAD_PRIORITY_LOWEST, ThreadPowerThrottling,
    };
    // SAFETY: GetCurrentThread returns a pseudo handle that needs no closing; only this thread
    // changes. Background mode lowers I/O and memory priority only, so CPU is lowered separately;
    // EcoQoS keeps the thread on efficiency cores.
    unsafe {
        if let Err(err) = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) {
            log::debug!("could not lower I/O priority: {err}");
        }
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw const state).cast(),
            u32::try_from(size_of::<THREAD_POWER_THROTTLING_STATE>()).unwrap_or(12),
        );
    }
}

/// Whether heavy background work (rebuilding the search index) is OK now: memory isn't low and
/// Windows isn't saving energy (`win_power.rs`). Cheap system query; called from a background
/// thread.
pub(crate) fn background_work_ok() -> bool {
    use windows::Win32::System::Memory::{
        CreateMemoryResourceNotification, LowMemoryResourceNotification,
        QueryMemoryResourceNotification,
    };
    // Created once and kept; the system picks the threshold.
    static LOW_MEMORY: std::sync::OnceLock<Option<isize>> = std::sync::OnceLock::new();
    let handle = *LOW_MEMORY.get_or_init(|| {
        // SAFETY: creates a system memory notification; the handle lives until the process exits.
        unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) }
            .ok()
            .map(|h| h.0 as isize)
    });
    if let Some(handle) = handle {
        let mut low = windows::core::BOOL::from(false);
        // SAFETY: handle is the notification created above and still open; low is an out param.
        let asked = unsafe {
            QueryMemoryResourceNotification(
                windows::Win32::Foundation::HANDLE(handle as *mut core::ffi::c_void),
                &raw mut low,
            )
        };
        if asked.is_ok() && low.as_bool() {
            return false;
        }
    }
    !crate::win_power::saving_energy()
}

/// Why [`run_elevated`] did not get an exit code.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ElevateError {
    /// The program could not be started, including a declined UAC prompt.
    #[error("{}", .0.message())]
    Launch(#[from] windows::core::Error),
    /// Windows started it but returned no process handle.
    #[error("no process")]
    NoProcess,
    /// It did not exit within two minutes.
    #[error("timed out")]
    TimedOut,
}

/// Runs `exe` elevated (UAC prompt) and waits for it to exit.
/// Returns the exit code; a cancelled prompt is `Err`. Call from a background thread.
pub(crate) fn run_elevated(exe: &Path, args: &str) -> Result<u32, ElevateError> {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::SEE_MASK_NOCLOSEPROCESS;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    debug_assert!(!arke_core::fs::is_ui_thread());
    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(std::ffi::OsStr::new(args));
    // SAFETY: initializes STA on this background thread; ShellExecuteEx needs COM.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: the string pointers in info stay valid for the call.
    let started = unsafe { ShellExecuteExW(&raw mut info) };
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    started?;
    let process = info.hProcess;
    if process.is_invalid() {
        return Err(ElevateError::NoProcess);
    }
    // SAFETY: process is a valid handle from ShellExecuteExW (NOCLOSEPROCESS), closed here.
    let code = unsafe {
        let waited = WaitForSingleObject(process, 120_000);
        let mut code = 1u32;
        let ok = waited == WAIT_OBJECT_0 && GetExitCodeProcess(process, &raw mut code).is_ok();
        let _ = CloseHandle(process);
        ok.then_some(code)
    };
    code.ok_or(ElevateError::TimedOut)
}

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;

#[derive(Default)]
pub(crate) struct WinShell;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS: the content is in the cloud.
const NOT_ON_DEVICE: u32 = 0x1000 | 0x4_0000 | 0x40_0000;
/// The attribute bits SetFileAttributes accepts; the rest (folder, reparse point, ...) are not
/// ours to set.
const SETTABLE: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x1000 | 0x2000;
/// `FILE_ATTRIBUTE_NORMAL`: what a file with no attribute set is given.
const NORMAL: u32 = 0x80;

/// The alternate data stream Windows puts on files from the internet.
fn zone_stream(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(":Zone.Identifier");
    PathBuf::from(name)
}

/// Bytes a file takes on disk (`GetCompressedFileSizeW`: asks the file system, opens no data).
fn compressed_size(path: &Path) -> Option<u64> {
    use windows::Win32::Foundation::{GetLastError, NO_ERROR};
    use windows::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let name = wide(path.as_os_str());
    let mut high = 0u32;
    // SAFETY: `name` is NUL-terminated; `high` is valid for the call.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(name.as_ptr()), Some(&raw mut high)) };
    // SAFETY: reads this thread's last error right after the call above.
    if low == u32::MAX && unsafe { GetLastError() } != NO_ERROR {
        return None;
    }
    Some((u64::from(high) << 32) | u64::from(low))
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: id points to a valid GUID constant. KF_FLAG_DONT_VERIFY skips the existence check,
    // so a folder redirected to a network share doesn't hit the network. Freed with CoTaskMemFree.
    unsafe {
        let pwstr = SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None).ok()?;
        let path = pwstr.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path
    }
}

fn volume_label(root: &[u16]) -> Option<String> {
    let mut name = [0u16; 261];
    // SAFETY: root is a NUL-terminated "X:\" wide string; name is sized correctly.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            Some(&mut name),
            None,
            None,
            None,
            None,
        )
        .ok()?;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let label = String::from_utf16_lossy(&name[..len]);
    (!label.is_empty()).then_some(label)
}

/// Common editors found at their default install paths. Only checks for the exe: no registry,
/// nothing launched.
fn editor_apps() -> Vec<(String, PathBuf)> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let programs = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let candidates: [(&str, Option<PathBuf>); 5] = [
        (
            "VS Code",
            local.as_ref().map(|d| {
                d.join("Programs")
                    .join("Microsoft VS Code")
                    .join("Code.exe")
            }),
        ),
        (
            "VS Code",
            programs
                .as_ref()
                .map(|d| d.join("Microsoft VS Code").join("Code.exe")),
        ),
        (
            "Zed",
            local
                .as_ref()
                .map(|d| d.join("Programs").join("Zed").join("Zed.exe")),
        ),
        (
            "Zed",
            programs.as_ref().map(|d| d.join("Zed").join("Zed.exe")),
        ),
        (
            "Cursor",
            local
                .as_ref()
                .map(|d| d.join("Programs").join("cursor").join("Cursor.exe")),
        ),
    ];
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for (name, path) in candidates {
        let Some(path) = path else { continue };
        if found.iter().all(|(seen, _)| seen != name) && path.is_file() {
            found.push((name.to_owned(), path));
        }
    }
    found
}

/// The user's Start menu programs folder (`%APPDATA%\Microsoft\Windows\Start Menu\Programs`).
/// Follows `%APPDATA%`, so tests with a temp profile stay in temp.
fn start_menu_programs(appdata: &std::path::Path) -> PathBuf {
    appdata
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
}

/// Path of Arke's shortcut in the Startup folder ("Start when I sign in to Windows").
pub(crate) fn autostart_shortcut(appdata: &std::path::Path) -> PathBuf {
    start_menu_programs(appdata)
        .join("Startup")
        .join("Arke.lnk")
}

/// Path of Arke's Start menu shortcut.
pub(crate) fn start_menu_shortcut(appdata: &std::path::Path) -> PathBuf {
    start_menu_programs(appdata).join("Arke.lnk")
}

fn appdata() -> Result<PathBuf, ShellError> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or(ShellError::MissingEnv("APPDATA"))
}

/// Creates or removes the Startup folder shortcut. It points at this exe with `--background`
/// (stay resident without a window). Just a file in the profile, no registry. Background thread.
fn set_autostart(on: bool) -> Result<(), ShellError> {
    set_shortcut(&autostart_shortcut(&appdata()?), on, "--background")
}

/// Creates or removes the Start menu shortcut to this exe. Rewritten on every start while on, so
/// it follows a moved exe. Just a file in the profile, no registry. Background thread.
fn set_start_menu(on: bool) -> Result<(), ShellError> {
    set_shortcut(&start_menu_shortcut(&appdata()?), on, "")
}

/// Removes a shortcut file; one that isn't there is fine.
pub(crate) fn remove_shortcut(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes (on) or deletes (off) a shortcut at `path` to this exe with `args`.
fn set_shortcut(path: &std::path::Path, on: bool, args: &str) -> Result<(), ShellError> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;
    if !on {
        return Ok(remove_shortcut(path)?);
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _com = crate::win_fileops::Com::init()?;
    let exe_w = wide(exe.as_os_str());
    let dir_w = exe.parent().map(|d| wide(d.as_os_str()));
    let path_w = wide(path.as_os_str());
    let args_w = wide(std::ffi::OsStr::new(args));
    // SAFETY: COM is initialized on this thread; the wide strings are NUL-terminated and outlive
    // the calls. ShellLink is a built-in Windows component, not a third-party extension.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(PCWSTR(exe_w.as_ptr()))?;
        link.SetArguments(PCWSTR(args_w.as_ptr()))?;
        if let Some(dir) = &dir_w {
            let _ = link.SetWorkingDirectory(PCWSTR(dir.as_ptr()));
        }
        let file: IPersistFile = link.cast()?;
        Ok(file.Save(PCWSTR(path_w.as_ptr()), true)?)
    }
}

/// Whether Windows animation effects are on (Settings > Accessibility > Visual effects).
/// Assumes on if the query fails. Reads one system parameter, fine on the UI thread.
pub(crate) fn animations_enabled() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut on = windows::core::BOOL::from(true);
    // SAFETY: on is a correctly sized out param.
    let asked = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    asked.is_err() || on.as_bool()
}

/// The system double-click time (Settings > Bluetooth & devices > Mouse); 500 ms by default.
pub(crate) fn double_click_time() -> std::time::Duration {
    // SAFETY: no arguments, no side effects.
    let ms = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
    std::time::Duration::from_millis(u64::from(if ms == 0 { 500 } else { ms }))
}

/// Windows accent color as 0xRRGGBB. Asks DWM only (no registry); cheap enough for the UI thread.
pub(crate) fn system_accent() -> Option<u32> {
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut color = 0u32;
    let mut opaque = windows::core::BOOL::default();
    // SAFETY: both are correctly sized out params.
    unsafe { DwmGetColorizationColor(&raw mut color, &raw mut opaque) }.ok()?;
    Some(color & 0x00ff_ffff)
}

/// Sets a dark or light window frame (title bar, Mica). Needed when the app theme differs from
/// the system's, otherwise Mica keeps the system color and looks washed out. Per window only.
pub(crate) fn set_dark_frame(hwnd: isize, dark: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let value = windows::core::BOOL::from(dark);
    // SAFETY: hwnd is Arke's own live window; value is valid and correctly sized for the call.
    let _ = unsafe {
        DwmSetWindowAttribute(
            HWND(hwnd as *mut _),
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const value).cast(),
            u32::try_from(size_of::<windows::core::BOOL>()).unwrap_or(4),
        )
    };
}

/// Windows version (major, minor, build). RtlGetVersion ignores the compatibility manifest.
pub(crate) fn windows_version() -> Option<(u32, u32, u32)> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(size_of::<OSVERSIONINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: info has its size set and is a valid out pointer.
    let status = unsafe { RtlGetVersion(&raw mut info) };
    status
        .is_ok()
        .then_some((info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber))
}

/// Mica needs Windows 11 (build 22000+). RtlGetVersion ignores the compatibility manifest.
pub(crate) fn supports_mica() -> bool {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(size_of::<OSVERSIONINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: info has its size set and is a valid out pointer.
    let status = unsafe { RtlGetVersion(&raw mut info) };
    status.is_ok() && info.dwMajorVersion >= 10 && info.dwBuildNumber >= 22000
}

/// UI language: `ARKE_LANG` wins, otherwise the user locale. Traditional Chinese locales
/// (TW, HK, MO, Hant) map to zh-TW, other Chinese ones (CN, SG, Hans, plain zh) to zh-CN,
/// everything else to English.
pub(crate) fn ui_locale() -> String {
    if let Ok(lang) = std::env::var("ARKE_LANG") {
        return lang;
    }
    system_locale().to_owned()
}

/// UI language Windows itself is set to (`zh-TW`, `zh-CN` or `en`), ignoring `ARKE_LANG` and the
/// Language setting. Decides which Chinese font English UI text falls back to.
pub(crate) fn system_locale() -> &'static str {
    let mut buf = [0u16; 85];
    // SAFETY: buf is a valid out buffer (LOCALE_NAME_MAX_LENGTH = 85).
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    let name = usize::try_from(len)
        .ok()
        .filter(|&n| n > 0)
        .map_or_default(|n| String::from_utf16_lossy(&buf[..n - 1]));
    locale_for(&name)
}

/// The temp folder under its long name. Some machines (CI runners) report it as 8.3 short names
/// (`C:\Users\RUNNER~1\...`) while the shell hands back long paths, so tests that compare paths
/// returned by the shell must start from this.
#[cfg(test)]
pub(crate) fn long_temp_dir() -> PathBuf {
    use windows::Win32::Storage::FileSystem::GetLongPathNameW;
    let short = std::env::temp_dir();
    let wide = windows::core::HSTRING::from(short.as_os_str());
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `wide` is a NUL-terminated path and `buf` is a writable buffer of its full length.
    let len = unsafe { GetLongPathNameW(&wide, Some(&mut buf)) } as usize;
    if len == 0 || len > buf.len() {
        return short;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
}

fn locale_for(system: &str) -> &'static str {
    let lower = system.to_ascii_lowercase();
    // Traditional first: "zh-Hant-CN" style names would otherwise look Simplified.
    if lower.starts_with("zh-tw")
        || lower.starts_with("zh-hk")
        || lower.starts_with("zh-mo")
        || lower.contains("hant")
    {
        "zh-TW"
    } else if lower == "zh" || lower.starts_with("zh-") || lower.starts_with("zh_") {
        // zh-CN, zh-SG, zh-Hans-*, and plain zh default to Simplified.
        "zh-CN"
    } else {
        "en"
    }
}

/// The device behind a drive letter (`\Device\Floppy0`, `\Device\HarddiskVolume3`...), from the
/// DOS device table only: no drive is touched. A `subst` drive maps to `\??\` plus its folder.
pub(crate) fn dos_device(letter: char) -> Option<String> {
    let name = wide(std::ffi::OsStr::new(&format!("{letter}:")));
    let mut buf = [0u16; 512];
    // SAFETY: name is NUL-terminated; buf outlives the call and its length is passed.
    let len = unsafe { QueryDosDeviceW(PCWSTR(name.as_ptr()), Some(&mut buf)) } as usize;
    // The result is a list of NUL-terminated strings; the first is the current mapping.
    let first = buf.get(..len)?.split(|&c| c == 0).next()?;
    (!first.is_empty()).then(|| String::from_utf16_lossy(first))
}

/// A removable drive is a floppy drive when its device is one (`\Device\Floppy0`, also USB
/// floppy drives), or, when that can't be told, when it has A: or B:, which Windows keeps for them.
fn looks_like_floppy(letter: char, device: Option<&str>) -> bool {
    device.map_or(matches!(letter, 'A' | 'B'), |device| {
        device.to_ascii_lowercase().starts_with(r"\device\floppy")
    })
}

/// Logs the removable drives left out for having no disk, once per change (the list reloads
/// often), so a missing drive in a report can be explained (#125).
fn log_hidden_drives(mask: u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST: AtomicU32 = AtomicU32::new(0);
    if LAST.swap(mask, Ordering::Relaxed) == mask || mask == 0 {
        return;
    }
    let letters: Vec<String> = (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:", char::from(b'A' + i)))
        .collect();
    log::info!(
        "removable drives without a disk, not listed: {}",
        letters.join(" ")
    );
}

/// Free and total bytes on removable media, if any is inserted, without the "insert a disk" prompt.
fn media_space(root: &[u16]) -> Option<(u64, u64)> {
    use windows::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SetThreadErrorMode, THREAD_ERROR_MODE,
    };
    let mut previous = THREAD_ERROR_MODE(0);
    // SAFETY: changes only this thread's error mode and restores it; previous is an out param.
    let changed =
        unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&raw mut previous)) }.is_ok();
    let space = disk_space(root);
    if changed {
        // SAFETY: restores the previous mode.
        let _ = unsafe { SetThreadErrorMode(previous, None) };
    }
    space
}

/// The user profile folder is labelled with its own name (the account name, as in Explorer's
/// navigation pane), so it isn't confused with the Home page. Falls back to "User folder".
pub(crate) fn profile_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| t!("place.profile").into_owned())
}

/// (free, total) bytes for a drive root. Meant for fixed drives; network and removable drives
/// can block.
fn disk_space(root: &[u16]) -> Option<(u64, u64)> {
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: root is a NUL-terminated "X:\" wide string; both out pointers are valid.
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(root.as_ptr()),
            Some(&raw mut free),
            Some(&raw mut total),
            None,
        )
        .ok()?;
    }
    (total > 0).then_some((free, total))
}

impl Shell for WinShell {
    fn load_places(&self) -> Vec<Place> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "load_places must not run on the UI thread"
        );
        let mut places = Vec::new();
        let folders: [(&GUID, &str, KnownFolder); 7] = [
            (&FOLDERID_Profile, "place.profile", KnownFolder::Profile),
            (&FOLDERID_Desktop, "place.desktop", KnownFolder::Desktop),
            (
                &FOLDERID_Downloads,
                "place.downloads",
                KnownFolder::Downloads,
            ),
            (
                &FOLDERID_Documents,
                "place.documents",
                KnownFolder::Documents,
            ),
            (&FOLDERID_Pictures, "place.pictures", KnownFolder::Pictures),
            (&FOLDERID_Music, "place.music", KnownFolder::Music),
            (&FOLDERID_Videos, "place.videos", KnownFolder::Videos),
        ];
        for (id, label, known) in folders {
            if let Some(path) = known_folder(id) {
                let label = if known == KnownFolder::Profile {
                    profile_label(&path)
                } else {
                    t!(label).into_owned()
                };
                places.push(Place {
                    label,
                    path,
                    kind: PlaceKind::Folder(known),
                });
            }
        }
        // Cloud folders such as OneDrive go before drives, as in Windows Explorer.
        let (roots, pinned) = (cloud_roots(), pinned_cloud_folders());
        let cloud = crate::cloud_places::pick(&roots, &pinned);
        log_cloud_folders(&roots, &pinned, &cloud);
        places.extend(cloud.into_iter().map(|(label, path)| Place {
            label,
            path,
            kind: PlaceKind::Cloud,
        }));

        // SAFETY: no arguments; reads the current drive letter bitmask.
        let mask = unsafe { GetLogicalDrives() };
        // Removable drives left out for having no disk, logged when that set changes.
        let mut hidden = 0u32;
        for i in 0..26u8 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = char::from(b'A' + i);
            let root = format!("{letter}:\\");
            let root_w = wide(std::ffi::OsStr::new(&root));
            // SAFETY: root_w is NUL-terminated. GetDriveTypeW reads local mount info only.
            let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
            // Floppy drives are listed like Explorer does, disk or not, and never read: asking a
            // floppy for its free space spins the drive (#125).
            let floppy = drive_type == DRIVE_REMOVABLE
                && looks_like_floppy(letter, dos_device(letter).as_deref());
            // Skip empty removable slots (card readers, USB): opening them only gives "device not
            // ready", and Explorer hides them too. Empty slots answer without a disk prompt.
            let removable_space = (drive_type == DRIVE_REMOVABLE && !floppy)
                .then(|| media_space(&root_w))
                .flatten();
            if drive_type == DRIVE_REMOVABLE && !floppy && removable_space.is_none() {
                hidden |= 1 << i;
                continue;
            }
            // Labels only for fixed drives; network or optical drives can block or ask for media.
            let (label, remote) = match drive_type {
                DRIVE_REMOVABLE if floppy => (t!("drive.floppy").into_owned(), false),
                DRIVE_FIXED => (
                    volume_label(&root_w).unwrap_or_else(|| t!("drive.local").into_owned()),
                    false,
                ),
                DRIVE_REMOTE => (
                    // Like File Explorer: custom label or "share (\\server)"; no network access
                    crate::win_net::mapped_drive_label(letter)
                        .unwrap_or_else(|| t!("drive.network").into_owned()),
                    true,
                ),
                DRIVE_REMOVABLE => (t!("drive.removable").into_owned(), false),
                DRIVE_CDROM => (t!("drive.cdrom").into_owned(), false),
                _ => (t!("drive.other").into_owned(), false),
            };
            let space = (drive_type == DRIVE_FIXED)
                .then(|| disk_space(&root_w))
                .flatten()
                .or(removable_space);
            places.push(Place {
                label: format!("{label} ({letter}:)"),
                path: PathBuf::from(root),
                kind: PlaceKind::Drive { remote, space },
            });
        }
        log_hidden_drives(hidden);
        places.extend(wsl_distros().into_iter().map(|name| Place {
            path: PathBuf::from(format!(r"\\wsl.localhost\{name}")),
            label: name,
            kind: PlaceKind::Wsl,
        }));
        places
    }

    fn open(&self, path: &Path, sort: Option<arke_core::SortSpec>) -> Result<(), ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "open must not run on the UI thread"
        );
        let file = wide(path.as_os_str());
        let dir = path.parent().map(|p| wide(p.as_os_str()));
        // SAFETY: initializes STA on this dedicated background thread; ShellExecuteEx needs COM.
        let com =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        // The folder view it is opened from, so Photos and the like can step through the folder
        // in this order (`win_viewsite.rs`).
        let site = sort.and_then(|sort| crate::win_viewsite::site(path, &sort));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            // NOASYNC: return only when done, since this thread exits right after.
            fMask: SEE_MASK_NOASYNC
                | if site.is_some() {
                    SEE_MASK_FLAG_HINST_IS_SITE
                } else {
                    0
                },
            lpFile: PCWSTR(file.as_ptr()),
            lpDirectory: dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            nShow: SW_SHOWNORMAL.0,
            hInstApp: site
                .as_ref()
                .map_or_else(HINSTANCE::default, |s| HINSTANCE(s.as_raw())),
            ..Default::default()
        };
        // SAFETY: the string pointers in info live until the end of the function, and so does
        // the site, which the shell only borrows for the call.
        let result = unsafe { ShellExecuteExW(&raw mut info) };
        drop(site);
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        result.map_err(|e| {
            log::warn!("could not open {}: {e:?}", path.display());
            // Access denied usually means antivirus or policy blocks launching other programs.
            if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
                ShellError::OpenBlocked(e)
            } else {
                ShellError::Windows(e)
            }
        })
    }

    fn expand_env(&self, input: &str) -> String {
        let trimmed = input.trim();
        let input = if let Some(rest) = trimmed.strip_prefix('~') {
            format!("%USERPROFILE%{rest}")
        } else {
            trimmed.to_owned()
        };
        if !input.contains('%') {
            return input;
        }
        let src = wide(std::ffi::OsStr::new(&input));
        let mut buf = vec![0u16; 1024];
        // SAFETY: src is NUL-terminated and buf is writable. Reads this process's environment only.
        let n = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 || n > buf.len() {
            return input;
        }
        String::from_utf16_lossy(&buf[..n.saturating_sub(1)])
    }

    fn format_time(&self, time: SystemTime) -> String {
        format_local_time(time).unwrap_or_default()
    }

    fn format_time_short(&self, time: SystemTime) -> String {
        let (Some(then), Some(now)) = (local_time(time), local_time(SystemTime::now())) else {
            return String::new();
        };
        let clock = format!("{:02}:{:02}", then.wHour, then.wMinute);
        let (m, d) = (then.wMonth.to_string(), then.wDay.to_string());
        match day_number(&now) - day_number(&then) {
            0 => t!("time.today", time = clock).into_owned(),
            1 => t!("time.yesterday", time = clock).into_owned(),
            _ if then.wYear == now.wYear => t!("time.this_year", month = m, day = d).into_owned(),
            _ => t!("time.date", year = then.wYear, month = m, day = d).into_owned(),
        }
    }

    fn file_op(&self, op: &FileOp, owner: Option<isize>) -> Result<OpOutcome, ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "file_op must not run on the UI thread"
        );
        win_fileops::perform(op, owner, false)
    }

    fn file_op_undoable(
        &self,
        op: &FileOp,
        owner: Option<isize>,
    ) -> (Result<OpOutcome, ShellError>, Option<crate::shell::Undo>) {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "file_op must not run on the UI thread"
        );
        win_fileops::perform_undoable(op, owner, false)
    }

    fn drive_key(&self, path: &Path) -> String {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "drive_key must not run on the UI thread"
        );
        let volume = arke_core::watches::volume_key(path).unwrap_or_default();
        match volume.strip_suffix(':').and_then(|l| l.chars().next()) {
            // Two partitions of one disk are one drive.
            Some(letter) if volume.len() == 2 => {
                crate::win_eject::physical_device(letter).unwrap_or(volume)
            }
            // Shares on one server share its disks.
            _ => volume
                .strip_prefix(r"\\")
                .and_then(|rest| rest.split('\\').next())
                .map_or_else(|| volume.clone(), |server| format!(r"\\{server}")),
        }
    }

    fn file_op_tracked(
        &self,
        op: &FileOp,
        owner: Option<isize>,
        progress: &std::sync::Arc<crate::shell::OpProgress>,
    ) -> (Result<OpOutcome, ShellError>, Option<crate::shell::Undo>) {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "file_op must not run on the UI thread"
        );
        // Copy or cross-drive move: total the size in the background (for speed) without blocking.
        let sources = match op {
            FileOp::Copy { sources, .. } => Some(sources.clone()),
            FileOp::Move { sources, dest } => {
                let drive = |p: &Path| {
                    p.components()
                        .next()
                        .map(|c| c.as_os_str().to_ascii_lowercase())
                };
                sources
                    .iter()
                    .any(|s| drive(s) != drive(dest))
                    .then(|| sources.clone())
            }
            _ => None,
        };
        if let Some(sources) = sources {
            let progress = Arc::clone(progress);
            crate::explorer::spawn_thread("op-size", move || {
                if let Some(total) = total_size(&sources) {
                    progress
                        .bytes_total
                        .store(total, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
        win_fileops::perform_tracked(op, owner, progress)
    }

    fn drag_out(
        &self,
        paths: &[PathBuf],
        image: Option<&crate::shell::DragImage>,
    ) -> Result<crate::shell::DragOutEnd, ShellError> {
        win_fileops::drag_out(paths, image).map(|outcome| match outcome {
            Some(win_fileops::DragOutcome::Returned) => crate::shell::DragOutEnd::Returned,
            _ => crate::shell::DragOutEnd::Finished,
        })
    }

    fn covered_at_cursor(&self, owner: Option<isize>) -> bool {
        owner.is_some_and(|h| {
            win_fileops::other_window_at_cursor(windows::Win32::Foundation::HWND(h as *mut _))
        })
    }

    fn window_at_cursor(&self) -> Option<(isize, (i32, i32))> {
        win_fileops::window_at_cursor()
    }
    fn drag_image(
        &self,
        base: Option<arke_core::thumbs::Thumbnail>,
        request: &crate::shell::DragImageRequest,
    ) -> Option<crate::shell::DragImage> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::win_dragimage::compose(
            base,
            &request.path,
            request.is_dir,
            request.own_icon,
            request.count,
            request.badge,
        )
    }

    fn undo(
        &self,
        undo: &crate::shell::Undo,
        owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "undo must not run on the UI thread"
        );
        win_fileops::undo(undo, owner, false)
    }

    fn undo_tracked(
        &self,
        undo: &crate::shell::Undo,
        owner: Option<isize>,
        progress: &std::sync::Arc<crate::shell::OpProgress>,
    ) -> Result<OpOutcome, ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "undo must not run on the UI thread"
        );
        win_fileops::undo_tracked(undo, owner, progress)
    }

    fn recycle_bin(&self) -> Result<Vec<crate::shell::RecycledItem>, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::win_recycle::list()
    }

    fn recycle_restore(
        &self,
        items: &[crate::shell::RecycledItem],
        owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        let back = items
            .iter()
            .map(|item| {
                // The original folder may be gone; recreate it, as Explorer's restore does.
                let _ = std::fs::create_dir_all(&item.from);
                crate::shell::PutBack {
                    now: item.now.clone(),
                    parent: item.from.clone(),
                    name: item.name.clone(),
                }
            })
            .collect();
        win_fileops::undo(&crate::shell::Undo::PutBack(back), owner, false)
    }

    fn recycle_purge(
        &self,
        items: &[crate::shell::RecycledItem],
        progress: &crate::shell::OpProgress,
    ) -> Result<OpOutcome, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        win_fileops::purge_recycled(items, progress)
    }

    fn set_clipboard_files(&self, files: &ClipboardFiles) -> Result<u32, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        win_fileops::set_clipboard_files(files)
    }

    fn clipboard_files(&self) -> Result<Option<(ClipboardFiles, u32)>, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        win_fileops::clipboard_files()
    }

    fn clipboard_virtual_files(
        &self,
    ) -> Result<Option<crate::win_virtual_drop::VirtualDrop>, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::win_virtual_drop::clipboard_virtual_files()
    }

    fn clear_clipboard_if(&self, seq: u32) {
        debug_assert!(!arke_core::fs::is_ui_thread());
        win_fileops::clear_clipboard_if(seq);
    }

    fn clipboard_seq(&self) -> u32 {
        win_fileops::clipboard_seq()
    }

    fn clipboard_has_files(&self) -> bool {
        win_fileops::clipboard_has_files()
    }

    fn is_local_disk(&self, path: &Path) -> bool {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::win_icons::is_local_disk(path)
    }

    fn has_recycle_bin(&self, path: &Path) -> bool {
        crate::win_recycle::has_recycle_bin(path)
    }

    fn file_icon(&self, path: &Path, size: u32) -> Option<arke_core::thumbs::Thumbnail> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::win_icons::file_icon(path, size)
    }

    fn type_icon(&self, ext: &str) -> Option<arke_core::thumbs::Thumbnail> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::win_dragimage::registered_type_icon(ext)
    }

    fn video_thumbnail(
        &self,
        path: &Path,
        max_side: u32,
    ) -> Result<Option<arke_core::thumbs::Thumbnail>, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::thumb_host::video_thumbnail(path, max_side)
            .map_err(|e| ShellError::Host(e.to_string()))
    }

    fn shell_menu(
        &self,
        paths: &[PathBuf],
        background: bool,
    ) -> Result<crate::shell::ShellMenu, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::shell_host::query(paths, background)
    }

    fn shell_menu_prefetch(&self) -> bool {
        crate::shell_host::isolated()
    }

    fn invoke_shell_menu(
        &self,
        session: u64,
        id: u32,
        client_point: (i32, i32),
        owner: Option<isize>,
    ) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::shell_host::invoke(session, id, client_point, owner)
    }

    fn release_shell_menu(&self, session: u64) {
        crate::shell_host::release(session);
    }

    fn fill_shell_submenu(
        &self,
        session: u64,
        index: u32,
    ) -> Result<Vec<crate::shell::ShellMenuEntry>, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::shell_host::fill_submenu(session, index)
    }

    fn shell_menu_handlers(
        &self,
        paths: &[PathBuf],
        background: bool,
    ) -> Result<Vec<crate::win_menu_parts::MenuHandler>, ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::shell_host::menu_handlers(paths, background)
    }

    fn shell_menu_handler(
        &self,
        paths: &[PathBuf],
        background: bool,
        handler: &crate::win_menu_parts::MenuHandler,
        group: u64,
    ) -> Result<(crate::shell::ShellMenu, std::time::Duration), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        crate::shell_host::query_handler(paths, background, handler, group)
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        win_fileops::set_clipboard_text(text)
    }

    fn local_parts(&self, time: SystemTime) -> Option<[u16; 6]> {
        let t = local_time(time)?;
        Some([t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond])
    }

    fn recent_files(&self, limit: usize) -> Vec<crate::home::RecentFile> {
        use windows::Win32::UI::Shell::FOLDERID_Recent;
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "recent_files must not run on the UI thread"
        );
        let Some(dir) = known_folder(&FOLDERID_Recent) else {
            return Vec::new();
        };
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        // Each Recent entry is a shortcut; its mtime is when the file was last used.
        let mut links: Vec<(PathBuf, std::time::SystemTime)> = read
            .filter_map(Result::ok)
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
            })
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        links.sort_by_key(|l| std::cmp::Reverse(l.1));
        let mut out: Vec<crate::home::RecentFile> = Vec::new();
        for (link, used) in links {
            if out.len() >= limit {
                break;
            }
            let Some((target, is_dir)) = self.shortcut_target(&link) else {
                continue;
            };
            if is_dir || out.iter().any(|r| r.path == target) {
                continue;
            }
            out.push(crate::home::RecentFile { path: target, used });
        }
        out
    }

    fn is_ejectable(&self, root: &Path) -> bool {
        crate::win_eject::is_ejectable(root)
    }

    fn eject_drive(&self, root: &Path) -> Result<(), ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "eject_drive must not run on the UI thread"
        );
        Ok(crate::win_eject::eject_drive(root)?)
    }

    fn choose_app(&self, path: &Path, owner: Option<isize>) -> Result<(), ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "choose_app must not run on the UI thread"
        );
        let owner = owner.map(|h| windows::Win32::Foundation::HWND(h as *mut _));
        // SAFETY: initializes STA on this dedicated background thread; ShellExecuteEx needs COM.
        let com =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        let result = open_as(path, owner).or_else(|e| {
            log::warn!(
                "openas failed for {}, falling back to SHOpenWithDialog: {e:?}",
                path.display()
            );
            open_with_dialog(path, owner)
        });
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        Ok(result?)
    }

    fn open_terminal(&self, dir: &Path) -> Result<(), ShellError> {
        use std::os::windows::process::CommandExt;
        // Console programs get their own window (debug builds of Arke have a console).
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "open_terminal must not run on the UI thread"
        );
        // wt.exe is an app execution alias; `;` is its command separator and must be escaped.
        let escaped = dir.to_string_lossy().replace(';', "\\;");
        let wt = std::process::Command::new("wt.exe")
            .arg("-d")
            .arg(&escaped)
            .spawn();
        if wt.is_ok() {
            return Ok(());
        }
        std::process::Command::new("powershell.exe")
            .arg("-NoExit")
            .current_dir(dir)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
            .map_err(ShellError::from)
    }

    fn run_in(&self, command_line: &str, dir: &Path) -> Result<(), ShellError> {
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "run_in must not run on the UI thread"
        );
        let (program, args) = split_command(command_line);
        // Start PowerShell and cmd through Windows Terminal: launched by a signed host, they avoid
        // antivirus rules that block unknown programs from starting a shell. Else run directly.
        if let Some(wt_args) = through_terminal(program, args, dir)
            && shell_execute("wt.exe", &wt_args, dir).is_ok()
        {
            return Ok(());
        }
        // Windows Terminal ignores the launch directory; pass it with -d.
        let args = if program.eq_ignore_ascii_case("wt") || program.eq_ignore_ascii_case("wt.exe") {
            if args.is_empty() {
                "-d .".to_string()
            } else {
                args.to_string()
            }
        } else {
            args.to_string()
        };
        shell_execute(program, &args, dir)
    }

    fn run_quicklook(&self, exe: Option<&Path>, item: &Path) -> Result<(), ShellError> {
        crate::quicklook::toggle(item, exe).map_err(ShellError::from)
    }

    fn disk_spaces(&self, roots: &[PathBuf]) -> Vec<(PathBuf, (u64, u64))> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        roots
            .iter()
            // Removed media doesn't raise the "insert a disk" prompt.
            .filter_map(|root| media_space(&wide(root.as_os_str())).map(|s| (root.clone(), s)))
            .collect()
    }

    fn shortcut_target(&self, path: &Path) -> Option<(PathBuf, bool)> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        read_shortcut(path)
    }

    fn file_details(&self, path: &Path) -> crate::shell::FileDetails {
        use std::os::windows::fs::MetadataExt;
        debug_assert!(!arke_core::fs::is_ui_thread());
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return crate::shell::FileDetails::default();
        };
        let size_on_disk = if meta.is_dir() {
            None
        } else if meta.file_attributes() & NOT_ON_DEVICE != 0 {
            Some(0)
        } else {
            compressed_size(path)
        };
        crate::shell::FileDetails {
            accessed: meta.accessed().ok(),
            size_on_disk,
        }
    }

    fn set_attributes(&self, paths: &[PathBuf], set: u32, clear: u32) -> Result<(), ShellError> {
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
            SetFileAttributesW,
        };
        debug_assert!(!arke_core::fs::is_ui_thread());
        // The bits SetFileAttributes accepts; the rest (folder, reparse point, ...) are not ours.
        for path in paths {
            let name = wide(path.as_os_str());
            // SAFETY: `name` is NUL-terminated and outlives both calls.
            unsafe {
                let current = GetFileAttributesW(PCWSTR(name.as_ptr()));
                if current == INVALID_FILE_ATTRIBUTES {
                    return Err(std::io::Error::last_os_error().into());
                }
                let old = current & SETTABLE;
                let new = (old | set) & !clear;
                if new == old {
                    continue;
                }
                let new = if new == 0 { NORMAL } else { new };
                SetFileAttributesW(PCWSTR(name.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(new))?;
            }
        }
        Ok(())
    }

    fn is_blocked(&self, path: &Path) -> bool {
        debug_assert!(!arke_core::fs::is_ui_thread());
        std::fs::metadata(zone_stream(path)).is_ok()
    }

    fn unblock(&self, path: &Path) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        Ok(std::fs::remove_file(zone_stream(path))?)
    }

    fn default_app(&self, path: &Path) -> Option<String> {
        use windows::Win32::UI::Shell::{ASSOCF_NONE, ASSOCSTR_FRIENDLYAPPNAME, AssocQueryStringW};
        use windows::core::PWSTR;
        debug_assert!(!arke_core::fs::is_ui_thread());
        let ext = path.extension()?;
        let assoc = wide(&std::ffi::OsString::from(format!(
            ".{}",
            ext.to_string_lossy()
        )));
        let mut buffer = [0u16; 260];
        let mut len = 260u32;
        // SAFETY: `assoc` is NUL-terminated; the buffer holds `len` characters.
        let asked = unsafe {
            AssocQueryStringW(
                ASSOCF_NONE,
                ASSOCSTR_FRIENDLYAPPNAME,
                PCWSTR(assoc.as_ptr()),
                PCWSTR::null(),
                Some(PWSTR(buffer.as_mut_ptr())),
                &raw mut len,
            )
        };
        if asked.is_err() {
            return None;
        }
        let end = (len as usize).saturating_sub(1).min(buffer.len());
        let name = String::from_utf16_lossy(&buffer[..end]);
        (!name.is_empty()).then_some(name)
    }

    fn volume_info(&self, root: &Path) -> Option<crate::shell::VolumeInfo> {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
        debug_assert!(!arke_core::fs::is_ui_thread());
        let root = wide(root.as_os_str());
        let mut label = [0u16; 261];
        let mut system = [0u16; 261];
        // SAFETY: `root` is a NUL-terminated wide string; the buffers are as large as stated.
        unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                Some(&mut system),
            )
            .ok()?;
        }
        let text = |buffer: &[u16]| {
            let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            String::from_utf16_lossy(&buffer[..len])
        };
        let (mut per_cluster, mut per_sector) = (0u32, 0u32);
        // SAFETY: the out parameters are valid for the call.
        let cluster = unsafe {
            GetDiskFreeSpaceW(
                PCWSTR(root.as_ptr()),
                Some(&raw mut per_cluster),
                Some(&raw mut per_sector),
                None,
                None,
            )
        }
        .map_or(0, |()| u64::from(per_cluster) * u64::from(per_sector));
        Some(crate::shell::VolumeInfo {
            label: text(&label),
            file_system: text(&system),
            cluster,
        })
    }

    fn create_shortcuts(
        &self,
        sources: &[PathBuf],
        dest: &Path,
    ) -> Result<Vec<PathBuf>, ShellError> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        use windows::core::Interface;
        debug_assert!(
            !arke_core::fs::is_ui_thread(),
            "create_shortcuts on the UI thread"
        );
        let _com = crate::win_fileops::Com::init()?;
        let mut made = Vec::new();
        for source in sources {
            let path = crate::shell::shortcut_path(source, dest, Path::exists);
            let target = wide(source.as_os_str());
            let link_path = wide(path.as_os_str());
            // SAFETY: COM is initialized on this thread; the wide strings are NUL-terminated and
            // outlive the calls. ShellLink is a built-in Windows component.
            unsafe {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
                link.SetPath(PCWSTR(target.as_ptr()))?;
                if let Some(dir) = source.parent() {
                    let dir = wide(dir.as_os_str());
                    let _ = link.SetWorkingDirectory(PCWSTR(dir.as_ptr()));
                }
                let file: IPersistFile = link.cast()?;
                file.Save(PCWSTR(link_path.as_ptr()), true)?;
            }
            made.push(path);
        }
        Ok(made)
    }

    fn set_cloud_pin(&self, paths: &[PathBuf], pin: CloudPin) -> Result<(), ShellError> {
        use windows::Win32::Foundation::{CloseHandle, GENERIC_ACCESS_RIGHTS};
        use windows::Win32::Storage::CloudFilters::{
            CF_PIN_STATE_PINNED, CF_PIN_STATE_UNPINNED, CF_PIN_STATE_UNSPECIFIED,
            CF_SET_PIN_FLAG_NONE, CF_SET_PIN_FLAG_RECURSE, CfSetPinState,
        };
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
        };
        debug_assert!(!arke_core::fs::is_ui_thread());
        let state = match pin {
            CloudPin::Keep => CF_PIN_STATE_PINNED,
            CloudPin::Free => CF_PIN_STATE_UNPINNED,
            CloudPin::Default => CF_PIN_STATE_UNSPECIFIED,
        };
        let mut failed = Vec::new();
        for path in paths {
            let name = wide(path.as_os_str());
            // Attribute rights only, so opening doesn't read content and won't trigger a download.
            // Directories need BACKUP_SEMANTICS.
            // SAFETY: name is NUL-terminated; the handle is closed below.
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    GENERIC_ACCESS_RIGHTS(FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    None,
                )
            };
            let result = handle.and_then(|handle| {
                let flags = if path.is_dir() {
                    CF_SET_PIN_FLAG_RECURSE
                } else {
                    CF_SET_PIN_FLAG_NONE
                };
                // SAFETY: handle was just opened and is valid; synchronous call (no OVERLAPPED).
                let r = unsafe { CfSetPinState(handle, state, flags, None) };
                // SAFETY: closes the handle opened above, once.
                let _ = unsafe { CloseHandle(handle) };
                r
            });
            if let Err(e) = result {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                failed.push((name, e));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(ShellError::CloudPin { failed })
        }
    }

    fn reveal_in_explorer(&self, path: &Path, select: bool) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        // This Explorer window is wanted; "Open folders in Arke" must leave it alone.
        crate::win_takeover::note_reveal(path, select);
        let quoted = format!("\"{}\"", path.display());
        let params = if select {
            format!("/select,{quoted}")
        } else {
            quoted
        };
        let file = wide(std::ffi::OsStr::new("explorer.exe"));
        let params = wide(std::ffi::OsStr::new(&params));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC,
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: the string pointers in info are valid for the call. Only launches explorer.exe.
        Ok(unsafe { ShellExecuteExW(&raw mut info) }?)
    }

    fn net_probe(&self) -> Option<std::sync::Arc<dyn arke_core::net::NetProbe>> {
        Some(std::sync::Arc::new(crate::win_net::WinNetProbe))
    }

    fn set_autostart(&self, on: bool) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        set_autostart(on)
    }

    fn set_start_menu(&self, on: bool) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        set_start_menu(on)
    }

    fn editor_apps(&self) -> Vec<(String, PathBuf)> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        editor_apps()
    }

    fn net_connect(&self, request: &crate::shell::NetConnect) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        Ok(crate::win_net::connect(request)?)
    }

    fn net_disconnect(&self, drive: &Path) -> Result<(), ShellError> {
        debug_assert!(!arke_core::fs::is_ui_thread());
        Ok(crate::win_net::disconnect(drive)?)
    }

    fn map_drive_dialog(&self, owner: Option<isize>) -> Result<bool, ShellError> {
        Ok(crate::win_net::drive_dialog(owner, true)?)
    }

    fn disconnect_drive_dialog(&self, owner: Option<isize>) -> Result<bool, ShellError> {
        Ok(crate::win_net::drive_dialog(owner, false)?)
    }

    fn pick_file(
        &self,
        request: &crate::shell::PickRequest,
        owner: Option<isize>,
    ) -> Result<Option<PathBuf>, ShellError> {
        crate::win_picker::pick(request, owner)
    }

    fn render_pdf(
        &self,
        path: &Path,
        page: u32,
        max: (u32, u32),
    ) -> Result<crate::shell::PdfRender, ShellError> {
        crate::win_pdf::render_page(path, page, max)
    }

    fn cloud_scan(&self, dir: &Path, items: &[std::sync::Arc<str>]) -> crate::shell::CloudScan {
        crate::win_cloud::scan(dir, items)
    }

    fn decode_ansi(&self, bytes: &[u8]) -> String {
        use windows::Win32::Globalization::{
            CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar,
        };
        if bytes.is_empty() {
            return String::new();
        }
        let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
        // SAFETY: pure conversion; the first call gets the length, the second fills a buffer of it.
        unsafe {
            let len = MultiByteToWideChar(CP_ACP, flags, bytes, None);
            let Ok(len) = usize::try_from(len) else {
                return String::from_utf8_lossy(bytes).into_owned();
            };
            let mut buf = vec![0u16; len];
            let n = MultiByteToWideChar(CP_ACP, flags, bytes, Some(&mut buf));
            String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0)])
        }
    }
}

/// 100 ns intervals from 1601-01-01 to 1970-01-01.
const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;

/// Current time and UTC offset, for date filters like `dm:today` in global search.
pub(crate) fn clock() -> arke_core::index_query::Clock {
    let now = SystemTime::now();
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    arke_core::index_query::Clock {
        now: secs,
        utc_offset: utc_offset(now).unwrap_or(0),
    }
}

/// Local offset from UTC in seconds, including DST.
fn utc_offset(now: SystemTime) -> Option<i64> {
    let since_unix = now.duration_since(UNIX_EPOCH).ok()?;
    let ticks = EPOCH_DIFF_100NS + (since_unix.as_nanos() / 100) as u64;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    let mut local_ft = FILETIME::default();
    // SAFETY: pure conversion on valid locals. Time zone data is cached by the system; no I/O.
    unsafe {
        FileTimeToSystemTime(&raw const ft, &raw mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &raw const utc, &raw mut local).ok()?;
        SystemTimeToFileTime(&raw const local, &raw mut local_ft).ok()?;
    }
    let local_ticks =
        (u64::from(local_ft.dwHighDateTime) << 32) | u64::from(local_ft.dwLowDateTime);
    // 100 ns to seconds. Local time loses sub-ms precision, so round to whole minutes.
    let diff = (i128::from(local_ticks) - i128::from(ticks)) / 10_000_000;
    let minutes = (diff as f64 / 60.0).round() as i64;
    Some(minutes * 60)
}

fn format_local_time(time: SystemTime) -> Option<String> {
    let local = local_time(time)?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    ))
}

/// Day number of a local date (proleptic Gregorian, arbitrary epoch; only for differences).
fn day_number(t: &SYSTEMTIME) -> i64 {
    // Howard Hinnant's days_from_civil.
    let (y, m, d) = (i64::from(t.wYear), i64::from(t.wMonth), i64::from(t.wDay));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe
}

fn local_time(time: SystemTime) -> Option<SYSTEMTIME> {
    let since_unix = time.duration_since(UNIX_EPOCH).ok()?;
    let ticks = EPOCH_DIFF_100NS + (since_unix.as_nanos() / 100) as u64;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: pure conversion on valid locals. Time zone data is cached by the system; no I/O.
    unsafe {
        FileTimeToSystemTime(&raw const ft, &raw mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &raw const utc, &raw mut local).ok()?;
    }
    Some(local)
}

/// Restores the window (GPUI only exposes maximize). `ShowWindowAsync` doesn't wait, so it's
/// fine on the UI thread.
pub(crate) fn restore_window(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindowAsync};
    // SAFETY: hwnd is Arke's own live window.
    let _ = unsafe { ShowWindowAsync(HWND(hwnd as *mut _), SW_RESTORE) };
}

/// Starts `explorer.exe` with `target` (a path or `shell:::{CLSID}`) for something Arke can't show.
/// Launching explorer.exe directly never goes through the folder verb, so nothing loops. Runs
/// before any other initialization (the folder handler path of `arke.exe`).
pub(crate) fn open_in_explorer(target: &str) -> bool {
    use windows::Win32::UI::Shell::SEE_MASK_FLAG_NO_UI;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let file = wide(std::ffi::OsStr::new("explorer.exe"));
    let params = wide(std::ffi::OsStr::new(&format!("\"{target}\"")));
    // SAFETY: initializes STA on this thread; ShellExecuteEx needs COM. The process ends soon after,
    // so there is no matching CoUninitialize.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the string pointers in info stay valid for the call.
    unsafe { ShellExecuteExW(&raw mut info) }.is_ok()
}

/// Brings Arke's window to the foreground when another program owns it, without synthesized key
/// presses or hooks: a short-lived thread briefly shares input state with the thread that holds the
/// foreground (and `also`, e.g. the File Explorer window just replaced), which makes
/// `SetForegroundWindow` allowed. It runs on its own thread because activating the window sends
/// messages to it, which must not happen while the UI thread is busy inside GPUI.
pub(crate) fn bring_to_front(hwnd: isize, also: Option<u32>) {
    let spawned = std::thread::Builder::new()
        .name("to-front".into())
        .spawn(move || {
            let front = raise(hwnd, also);
            log::info!("window to front: {}", if front { "yes" } else { "no" });
        });
    if let Err(err) = spawned {
        log::warn!("cannot start the to-front thread: {err}");
    }
}

/// Moves Arke's window `hwnd` so its content is at `bounds`, in the logical pixels gpui gives
/// windows on the monitor `monitor` (whose bounds in those pixels are `screen`). Neither activates
/// it nor changes which window is in front.
pub(crate) fn move_window(
    hwnd: isize,
    monitor: u64,
    screen: gpui::Bounds<gpui::Pixels>,
    bounds: gpui::Bounds<gpui::Pixels>,
) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    };
    let hwnd = HWND(hwnd as *mut _);
    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or_default(),
        ..Default::default()
    };
    let (mut frame, mut client) = (RECT::default(), RECT::default());
    // SAFETY: plain Win32 calls on Arke's own window and a monitor handle gpui got from Windows
    // (a monitor unplugged since fails harmlessly); every pointer is to a local.
    unsafe {
        if !GetMonitorInfoW(HMONITOR(monitor as *mut _), &raw mut info).as_bool()
            || GetWindowRect(hwnd, &raw mut frame).is_err()
            || GetClientRect(hwnd, &raw mut client).is_err()
        {
            return;
        }
    }
    let logical_width = f32::from(screen.size.width);
    if logical_width <= 0. {
        return;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "screen coordinates, far inside f32 and i32"
    )]
    let (x, y, width, height) = {
        let scale = (info.rcMonitor.right - info.rcMonitor.left) as f32 / logical_width;
        let device = |v: gpui::Pixels| (f32::from(v) * scale).round() as i32;
        // The frame around the content (invisible resize borders), split in half on each side
        // as gpui does when it places a window and reads its position back.
        let extra_width = (frame.right - frame.left) - (client.right - client.left);
        let extra_height = (frame.bottom - frame.top) - (client.bottom - client.top);
        let (left, top) = (extra_width / 2, extra_height / 2);
        (
            device(bounds.origin.x) - left,
            device(bounds.origin.y) - top,
            device(bounds.size.width) + extra_width,
            device(bounds.size.height) + extra_height,
        )
    };
    // SAFETY: as above.
    if let Err(err) = unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
    } {
        log::warn!("cannot move the window to its saved place: {err}");
    }
}

/// Returns whether the window ended up in front.
fn raise(hwnd: isize, also: Option<u32>) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE,
        SetForegroundWindow, ShowWindowAsync,
    };
    let target = HWND(hwnd as *mut _);
    // SAFETY: plain Win32 calls on Arke's own window and on thread ids that are only used for
    // AttachThreadInput (which fails harmlessly for a thread that is gone); every attach is
    // detached again below.
    unsafe {
        if IsIconic(target).as_bool() {
            let _ = ShowWindowAsync(target, SW_RESTORE);
        }
        let front = GetForegroundWindow();
        if front == target {
            return true;
        }
        let me = GetCurrentThreadId();
        let mut threads: Vec<u32> = Vec::new();
        if !front.0.is_null() {
            threads.push(GetWindowThreadProcessId(front, None));
        }
        threads.extend(also);
        threads.retain(|&t| t != 0 && t != me);
        threads.sort_unstable();
        threads.dedup();
        let attached: Vec<u32> = threads
            .into_iter()
            .filter(|&t| AttachThreadInput(me, t, true).as_bool())
            .collect();
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
        for thread in attached {
            let _ = AttachThreadInput(me, thread, false);
        }
        GetForegroundWindow() == target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Real shortcuts in a temp folder: the target reads back, and a name clash gets a number.
    #[test]
    fn creates_shortcuts_that_point_back() {
        let dir = crate::win::long_temp_dir().join(format!("arke-lnk-{}", std::process::id()));
        // Non-ASCII name: the target must read back intact through the shell link API.
        let target = dir.join("報告.txt");
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(&target, "x").unwrap();
        let shell = WinShell;
        let first = shell
            .create_shortcuts(std::slice::from_ref(&target), &dest)
            .unwrap();
        let second = shell
            .create_shortcuts(std::slice::from_ref(&target), &dest)
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_ne!(
            first, second,
            "a name clash gets a number, not an overwrite"
        );
        assert!(first[0].exists() && second[0].exists());
        assert_eq!(shell.shortcut_target(&first[0]), Some((target, false)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clock_has_a_sane_offset() {
        let c = clock();
        assert!(c.now > 1_700_000_000);
        assert!(
            c.utc_offset.abs() <= 14 * 3600 && c.utc_offset % 60 == 0,
            "{c:?}"
        );
    }

    #[test]
    fn formats_time() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s = WinShell.format_time(t);
        assert!(s.starts_with("2023-11-1"), "{s}");
    }

    #[test]
    fn expands_env() {
        let s = WinShell.expand_env("%SystemRoot%\\System32");
        assert!(!s.contains('%'), "{s}");
        assert!(WinShell.expand_env("~").len() > 3);
    }
}

/// Explorer's "Choose another app" for one file: the `openas` verb Windows registers on the
/// `Unknown` class (`OpenWith.exe "%1"`, a COM local server). This dialog has "Always", and
/// Windows itself saves that choice. `SHOpenWithDialog` ignores the registration flags since
/// Windows 10 and only offers "Just once". Nothing third-party loads into Arke.
fn open_as(
    path: &Path,
    owner: Option<windows::Win32::Foundation::HWND>,
) -> windows::core::Result<()> {
    use windows::Win32::UI::Shell::{SEE_MASK_CLASSNAME, SEE_MASK_FLAG_NO_UI};
    let verb = wide(std::ffi::OsStr::new("openas"));
    let class = wide(std::ffi::OsStr::new("Unknown"));
    let file = wide(path.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // CLASSNAME: use the Unknown class's verb whatever app the file type has.
        // NOASYNC: return only once OpenWith.exe is started, since this thread exits after.
        fMask: SEE_MASK_NOASYNC | SEE_MASK_CLASSNAME | SEE_MASK_FLAG_NO_UI,
        hwnd: owner.unwrap_or_default(),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpClass: PCWSTR(class.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the string pointers in info are valid for the call; owner is Arke's own window.
    unsafe { ShellExecuteExW(&raw mut info) }
}

/// `SHOpenWithDialog`, "Just once" only (see [`open_as`]). Blocks until the dialog closes;
/// Cancel is not an error.
fn open_with_dialog(
    path: &Path,
    owner: Option<windows::Win32::Foundation::HWND>,
) -> windows::core::Result<()> {
    use windows::Win32::UI::Shell::{OAIF_EXEC, OPENASINFO, SHOpenWithDialog};
    let file = wide(path.as_os_str());
    let info = OPENASINFO {
        pcszFile: PCWSTR(file.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC,
    };
    // SAFETY: info and file are valid for the call; owner is Arke's own window or none.
    // The dialog is shown by OpenWith.exe, so no third-party extensions load into Arke.
    match unsafe { SHOpenWithDialog(owner, &raw const info) } {
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() => Ok(()),
        other => other,
    }
}

/// Starts `program` in `dir` via ShellExecuteEx, which resolves PATH and app execution aliases.
fn shell_execute(program: &str, args: &str, dir: &Path) -> Result<(), ShellError> {
    let file = wide(std::ffi::OsStr::new(program));
    let params = wide(std::ffi::OsStr::new(args));
    let directory = wide(dir.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // No Windows error dialog for a missing program; Arke reports it itself.
        fMask: SEE_MASK_NOASYNC | windows::Win32::UI::Shell::SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the string pointers in info are valid for the call.
    Ok(unsafe { ShellExecuteExW(&raw mut info) }?)
}

/// For PowerShell or cmd, builds Windows Terminal arguments (`-d "dir" powershell ...`); `None`
/// otherwise. Windows Terminal splits commands on `;`, so it's escaped in the dir and args.
fn through_terminal(program: &str, args: &str, dir: &Path) -> Option<String> {
    let lower = program.to_ascii_lowercase();
    let name = lower.strip_suffix(".exe").unwrap_or(&lower);
    if !matches!(name, "powershell" | "pwsh" | "cmd") {
        return None;
    }
    let escape = |s: &str| s.replace(';', "\\;");
    let dir = escape(&dir.to_string_lossy());
    let mut out = format!("-d \"{dir}\" {name}");
    if !args.is_empty() {
        out.push(' ');
        out.push_str(&escape(args));
    }
    Some(out)
}

/// Splits a command line into program and arguments. The program may be quoted to contain
/// spaces; the rest is passed through as is.
fn split_command(line: &str) -> (&str, &str) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        return (&rest[..end], rest[end + 1..].trim_start());
    }
    match line.split_once(char::is_whitespace) {
        Some((program, args)) => (program, args.trim_start()),
        None => (line, ""),
    }
}

#[cfg(test)]
mod locale_tests {
    use super::locale_for;

    #[test]
    fn maps_system_locales_to_ui_languages() {
        for tw in [
            "zh-TW",
            "zh-HK",
            "zh-MO",
            "zh-Hant",
            "zh-Hant-TW",
            "zh-Hant-CN",
        ] {
            assert_eq!(locale_for(tw), "zh-TW", "{tw}");
        }
        for cn in [
            "zh-CN",
            "zh-SG",
            "zh-Hans",
            "zh-Hans-CN",
            "zh-Hans-HK",
            "zh",
            "ZH-cn",
        ] {
            assert_eq!(locale_for(cn), "zh-CN", "{cn}");
        }
        for en in ["en-US", "ja-JP", "", "zhx"] {
            assert_eq!(locale_for(en), "en", "{en}");
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::{split_command, through_terminal};

    /// PowerShell and cmd go through Windows Terminal with the current folder; others don't.
    #[test]
    fn shells_go_through_windows_terminal() {
        let dir = std::path::Path::new(r"C:\work;dir");
        assert_eq!(
            through_terminal("powershell", "", dir).as_deref(),
            Some(r#"-d "C:\work\;dir" powershell"#)
        );
        assert_eq!(
            through_terminal("CMD.exe", "/k echo hi; ver", dir).as_deref(),
            Some(r#"-d "C:\work\;dir" cmd /k echo hi\; ver"#)
        );
        assert_eq!(
            through_terminal("pwsh", "-NoExit", dir).as_deref(),
            Some(r#"-d "C:\work\;dir" pwsh -NoExit"#)
        );
        assert_eq!(through_terminal("code", ".", dir), None);
        assert_eq!(through_terminal("bash", "", dir), None);
    }

    #[test]
    fn splits_the_program_from_its_arguments() {
        assert_eq!(split_command("cmd"), ("cmd", ""));
        assert_eq!(split_command("  code .  "), ("code", "."));
        assert_eq!(
            split_command(r#""C:\My Tools\x.exe" -a b"#),
            (r"C:\My Tools\x.exe", "-a b")
        );
        assert_eq!(
            split_command("powershell -NoExit -c ls"),
            ("powershell", "-NoExit -c ls")
        );
    }
}

/// Total size of these files and folders (recursive, symlinks not followed). Gives up with `None`
/// past 300k entries or 5 seconds; speed then uses completed files. Call from a background thread.
fn total_size(sources: &[PathBuf]) -> Option<u64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut stack: Vec<PathBuf> = sources.to_vec();
    let mut total = 0u64;
    let mut seen = 0usize;
    while let Some(path) = stack.pop() {
        seen += 1;
        if seen > 300_000 || std::time::Instant::now() > deadline {
            return None;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    Some(total)
}

/// Fixed drive roots (`C:\`). Only asks for the drive type, no disk access; fine on the UI thread.
pub(crate) fn fixed_drive_roots() -> Vec<PathBuf> {
    // SAFETY: no arguments; reads the current drive letter bitmask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", char::from(b'A' + i)))
        .filter(|root| {
            let w = wide(std::ffi::OsStr::new(root));
            // SAFETY: w is a NUL-terminated "X:\" wide string; reads the recorded drive type only.
            (unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) }) == DRIVE_FIXED
        })
        .map(PathBuf::from)
        .collect()
}

/// Network drive roots (`X:\`). Only asks for the drive type, no network; fine on the UI thread.
pub(crate) fn network_drive_roots() -> Vec<PathBuf> {
    // SAFETY: no arguments; reads the current drive letter bitmask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", char::from(b'A' + i)))
        .filter(|root| {
            let w = wide(std::ffi::OsStr::new(root));
            // SAFETY: w is a NUL-terminated "X:\" wide string; reads the recorded drive type only.
            (unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) }) == 4
        })
        .map(PathBuf::from)
        .collect()
}

/// Cloud sync roots registered with Windows via the Cloud Files API, as (label, path). Only reads
/// the registration: no network, doesn't wake the sync client. Call from a background thread.
fn cloud_roots() -> Vec<crate::cloud_places::SyncRoot> {
    use windows::Storage::IStorageItem;
    use windows::Storage::Provider::StorageProviderSyncRootManager;
    use windows::Win32::System::Com::COINIT_MULTITHREADED;
    use windows::core::Interface;
    // SAFETY: this background thread's own COM init; CoUninitialize only on success.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let read = || -> windows::core::Result<Vec<crate::cloud_places::SyncRoot>> {
        let roots = StorageProviderSyncRootManager::GetCurrentSyncRoots()?;
        let mut out = Vec::new();
        for ix in 0..roots.Size()? {
            let root = roots.GetAt(ix)?;
            let path = PathBuf::from(root.Path()?.cast::<IStorageItem>()?.Path()?.to_string());
            let name = root.DisplayNameResource()?.to_string();
            // Business accounts get folders like "OneDrive - Contoso", clearer than "OneDrive".
            let folder = path
                .file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned());
            let label = if name.is_empty() || folder.starts_with(&name) {
                folder
            } else {
                name
            };
            let id = root.Id()?.to_string();
            let provider = id.split('!').next().unwrap_or_default().to_owned();
            if !out
                .iter()
                .any(|r: &crate::cloud_places::SyncRoot| r.path == path)
            {
                out.push(crate::cloud_places::SyncRoot {
                    provider,
                    label,
                    path,
                });
            }
        }
        Ok(out)
    };
    let roots = read().unwrap_or_default();
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    roots
}

/// Which cloud folders came from where, for reports like #93. Every tab loads the places, so only
/// when it changes.
fn log_cloud_folders(
    roots: &[crate::cloud_places::SyncRoot],
    pinned: &[crate::cloud_places::Pinned],
    shown: &[(String, PathBuf)],
) {
    static LOGGED: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());
    let line = format!(
        "cloud folders: sync roots {:?}, pinned {:?}, shown {:?}",
        roots
            .iter()
            .map(|r| (&r.provider, &r.path))
            .collect::<Vec<_>>(),
        pinned.iter().map(|p| &p.path).collect::<Vec<_>>(),
        shown.iter().map(|(_, p)| p).collect::<Vec<_>>()
    );
    let mut logged = LOGGED.lock();
    if *logged != line {
        log::info!("{line}");
        *logged = line;
    }
}

/// Folders pinned to File Explorer's navigation pane (`Desktop\NameSpace` items with
/// `System.IsPinnedToNameSpaceTree`), the way cloud providers add theirs: name and target folder
/// (`TargetFolderPath`, or `TargetKnownFolder` for OneDrive). Reads the registry only.
fn pinned_cloud_folders() -> Vec<crate::cloud_places::Pinned> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const NAMESPACE: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace";
    let mut out = Vec::new();
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for clsid in reg_subkeys(hive, NAMESPACE) {
            // Per-user classes win over machine ones, as in HKEY_CLASSES_ROOT.
            let class = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
                .into_iter()
                .map(|h| (h, format!(r"Software\Classes\CLSID\{clsid}")))
                .find(|(h, k)| reg_key_exists(*h, k));
            let Some((h, class)) = class else { continue };
            if read_reg_dword(h, &class, "System.IsPinnedToNameSpaceTree") != Some(1) {
                continue;
            }
            let bag = format!(r"{class}\Instance\InitPropertyBag");
            let target = read_reg_string(h, &bag, "TargetFolderPath")
                .map(|p| PathBuf::from(expand_env(&p)))
                .or_else(|| {
                    let id = read_reg_string(h, &bag, "TargetKnownFolder")?;
                    known_folder(&GUID::try_from(id.trim_matches(['{', '}'])).ok()?)
                });
            let Some(path) = target else { continue };
            let label = read_reg_string(h, &class, "").unwrap_or_else(|| {
                path.file_name()
                    .map_or_default(|n| n.to_string_lossy().into_owned())
            });
            out.push(crate::cloud_places::Pinned { label, path });
        }
    }
    out
}

/// Names of a registry key's subkeys (empty if it can't be opened). Read only.
pub(crate) fn reg_subkeys(root: windows::Win32::System::Registry::HKEY, key: &str) -> Vec<String> {
    use windows::Win32::System::Registry::{
        HKEY, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };
    let name = wide(std::ffi::OsStr::new(key));
    let mut handle = HKEY::default();
    // SAFETY: name is NUL-terminated; the handle is closed below.
    let opened =
        unsafe { RegOpenKeyExW(root, PCWSTR(name.as_ptr()), None, KEY_READ, &raw mut handle) };
    if opened.is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut buf = [0u16; 256];
    for index in 0.. {
        let mut len = buf.len() as u32;
        // SAFETY: buf holds len UTF-16 units; the other out params are optional.
        let result = unsafe {
            RegEnumKeyExW(
                handle,
                index,
                Some(windows::core::PWSTR(buf.as_mut_ptr())),
                &raw mut len,
                None,
                None,
                None,
                None,
            )
        };
        if result.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    // SAFETY: opened above.
    let _ = unsafe { RegCloseKey(handle) };
    out
}

/// Whether a registry key exists. Read only.
fn reg_key_exists(root: windows::Win32::System::Registry::HKEY, key: &str) -> bool {
    use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegOpenKeyExW};
    let name = wide(std::ffi::OsStr::new(key));
    let mut handle = HKEY::default();
    // SAFETY: name is NUL-terminated; the handle is closed right away.
    let opened =
        unsafe { RegOpenKeyExW(root, PCWSTR(name.as_ptr()), None, KEY_READ, &raw mut handle) };
    if opened.is_ok() {
        // SAFETY: opened above.
        let _ = unsafe { RegCloseKey(handle) };
    }
    opened.is_ok()
}

/// A `REG_DWORD` value, or `None`.
fn read_reg_dword(
    root: windows::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<u32> {
    use windows::Win32::System::Registry::{RRF_RT_REG_DWORD, RegGetValueW};
    let (key, value) = (
        wide(std::ffi::OsStr::new(key)),
        wide(std::ffi::OsStr::new(value)),
    );
    let mut data = 0u32;
    let mut bytes = 4u32;
    // SAFETY: names are NUL-terminated; data is the 4-byte buffer RRF_RT_REG_DWORD needs.
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&raw mut bytes),
        )
    }
    .is_ok()
    .then_some(data)
}

/// `%USERPROFILE%\x` and the like, expanded.
fn expand_env(text: &str) -> String {
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    let src = wide(std::ffi::OsStr::new(text));
    let mut buf = vec![0u16; 1024];
    // SAFETY: src is NUL-terminated; buf is writable for its length.
    let len = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) } as usize;
    if len == 0 || len > buf.len() {
        return text.to_owned();
    }
    String::from_utf16_lossy(&buf[..len.saturating_sub(1)])
}

/// Installed WSL distros. `wsl.exe --list --quiet` lists names without starting any distro.
/// Empty if WSL is missing or doesn't answer within 3 seconds. Called from a background thread.
fn wsl_distros() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(mut child) = Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return Vec::new(),
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                return Vec::new();
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let mut bytes = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_end(&mut bytes);
    }
    parse_wsl_list(&bytes)
}

/// `wsl --list --quiet` prints UTF-16LE, one name per line.
fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|l| !l.is_empty() && !l.contains(['\\', '/', ':']))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod wsl_tests {
    use super::parse_wsl_list;

    #[test]
    fn reads_distro_names_from_utf16() {
        let text = "Ubuntu-24.04\r\ndocker-desktop\r\n\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_wsl_list(&bytes), ["Ubuntu-24.04", "docker-desktop"]);
        assert!(parse_wsl_list(&[]).is_empty());
    }
}

/// What the shortcut at `path` points to, and whether that is a folder. Reads the `.lnk` only.
pub(crate) fn read_shortcut(path: &Path) -> Option<(PathBuf, bool)> {
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, WIN32_FIND_DATAW};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;
    let _com = crate::win_fileops::Com::init().ok()?;
    let path_w = wide(path.as_os_str());
    let mut buf = vec![0u16; 32_768];
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: COM is initialized on this thread; path_w is NUL-terminated; buf and data are
    // valid for the call. ShellLink is built in. Only the .lnk is read; no Resolve, so no UI.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(PCWSTR(path_w.as_ptr()), STGM_READ).ok()?;
        link.GetPath(&mut buf, &raw mut data, 0).ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    let target = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    Some((
        target,
        data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
    ))
}

/// A string value from the registry (`REG_SZ` or `REG_EXPAND_SZ`, unexpanded), or `None`.
pub(crate) fn read_reg_string(
    root: windows::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<String> {
    use windows::Win32::System::Registry::{
        RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    let (key, value) = (
        wide(std::ffi::OsStr::new(key)),
        wide(std::ffi::OsStr::new(value)),
    );
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut bytes = 0u32;
    // SAFETY: both names are NUL-terminated; this call only reads the size.
    let sized = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            flags,
            None,
            None,
            Some(&raw mut bytes),
        )
    };
    if sized.is_err() || bytes == 0 {
        return None;
    }
    let mut buf = vec![0u16; bytes as usize / 2 + 1];
    // SAFETY: `buf` holds at least `bytes` bytes; the names are NUL-terminated.
    let read = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut bytes),
        )
    };
    if read.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len])).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod floppy_tests {
    use super::*;

    #[test]
    fn floppy_drives_are_told_by_their_device() {
        assert!(looks_like_floppy('A', Some(r"\Device\Floppy0")));
        // A USB floppy drive can get any letter.
        assert!(looks_like_floppy('F', Some(r"\Device\Floppy1")));
        // A card reader or USB stick moved to A: is not one.
        assert!(!looks_like_floppy('A', Some(r"\Device\HarddiskVolume7")));
        assert!(!looks_like_floppy('E', Some(r"\Device\HarddiskVolume7")));
        // Unknown device: A: and B: are kept for floppy drives.
        assert!(looks_like_floppy('B', None));
        assert!(!looks_like_floppy('E', None));
    }

    #[test]
    fn the_dos_device_table_answers_for_the_system_drive() {
        let device = dos_device('C').expect("C: is mapped");
        assert!(device.starts_with(r"\Device\"), "{device}");
        assert!(!looks_like_floppy('C', Some(&device)));
    }
}
