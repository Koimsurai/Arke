//! Arke's folder-open COM class (#340): the folder verbs' `DelegateExecute` once it was checked to
//! work (`arke_core::folder_handler::ARKE_CLSID`).
//!
//! With an empty `DelegateExecute` the shell starts `"arke.exe" "%1"` from the calling program's
//! process. Programs that block their own child processes (WPS Office's "Open file location")
//! then fail. With Arke's class id the shell asks COM for the class instead: the resident Arke has
//! registered it ([`serve`]), so the call reaches it over RPC and no process is started; with no
//! Arke running, COM starts `arke.exe -Embedding`, which becomes the resident Arke.
//!
//! `Execute` runs on an MTA thread of COM's and only queues the folders; a worker thread routes
//! them the way `main.rs` routes a folder argument and hands them to the resident Arke through
//! its pipe, so the caller never waits for Arke's UI.
//!
//! Whether COM accepts the class is checked before the verbs rely on it ([`works`]): DCOM does not
//! accept a class registered per user (`HKCU`) whose exe is in a folder an administrator created
//! (a per-machine install under Program Files). The server starts and registers, but callers wait
//! about a minute and get `CO_E_SERVER_EXEC_FAILURE`. Then the verbs keep running the command.
#![allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    reason = "The code `windows::core::implement` generates trips these pedantic lints"
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_FAIL, POINT};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoRegisterClassObject, CoTaskMemFree, IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Shell::{
    IExecuteCommand, IExecuteCommand_Impl, IObjectWithSelection, IObjectWithSelection_Impl,
    IShellItemArray, SIGDN_DESKTOPABSOLUTEPARSING,
};
use windows::core::{BOOL, GUID, IUnknown, Interface, PCWSTR, Ref, implement};

use arke_core::takeover::{Route, Target};

use crate::instance::{self, OpenRequest, Page, Request, SendOutcome};

/// `arke.exe --com-check`: asks COM for Arke's class and exits 0 when it got it ([`works`]).
pub(crate) const CHECK_ARG: &str = "--com-check";

/// The class id; the same as `arke_core::folder_handler::ARKE_CLSID`.
const CLSID: GUID = GUID::from_u128(0x8F3B_6C21_4E7A_4D2B_9C5E_A1D0_4F7E_3340);

/// How long [`works`] waits for the check: a working class answers in milliseconds; a refused one
/// makes the caller wait about a minute.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a forwarding server ([`forward_until_idle`]) stays after its last call.
const IDLE_EXIT: Duration = Duration::from_secs(10);

/// How long a folder waits for the resident Arke's pipe (COM started Arke a moment ago).
const PIPE_WAIT: Duration = Duration::from_secs(15);

/// When COM last created or called an object here, in ms since [`START`].
static LAST_CALL_MS: AtomicU64 = AtomicU64::new(0);
static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn touch() {
    let ms = u64::try_from(START.elapsed().as_millis()).unwrap_or(u64::MAX);
    LAST_CALL_MS.store(ms, Ordering::Relaxed);
}

/// The worker that routes queued folders; started with the first [`serve`].
static QUEUE: std::sync::OnceLock<flume::Sender<Vec<String>>> = std::sync::OnceLock::new();

#[implement(IExecuteCommand, IObjectWithSelection)]
struct FolderOpen {
    selection: Mutex<Option<IShellItemArray>>,
}

impl IExecuteCommand_Impl for FolderOpen_Impl {
    fn SetKeyState(&self, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetParameters(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetPosition(&self, _: &POINT) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetShowWindow(&self, _: i32) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetNoShowUI(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetDirectory(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn Execute(&self) -> windows::core::Result<()> {
        touch();
        let Some(items) = self.selection.lock().clone() else {
            return Err(E_FAIL.into());
        };
        let names = parsing_names(&items);
        log::info!("folder COM: execute for {} items", names.len());
        if names.is_empty() {
            return Err(E_FAIL.into());
        }
        match QUEUE.get() {
            Some(queue) if queue.send(names).is_ok() => Ok(()),
            _ => Err(E_FAIL.into()),
        }
    }
}

impl IObjectWithSelection_Impl for FolderOpen_Impl {
    fn SetSelection(&self, items: Ref<IShellItemArray>) -> windows::core::Result<()> {
        *self.selection.lock() = items.cloned();
        Ok(())
    }
    fn GetSelection(
        &self,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        let items = self.selection.lock().clone();
        match items {
            // SAFETY: riid and ppv come from the caller as QueryInterface takes them.
            Some(items) => unsafe { items.query(riid, ppv) }.ok(),
            None => Err(E_FAIL.into()),
        }
    }
}

/// Each item's desktop parsing name: the path of a file-system folder, `::{CLSID}` for This PC,
/// the Recycle Bin and other virtual folders; what the shell passes as `%1`.
fn parsing_names(items: &IShellItemArray) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: plain calls on a live item array; each returned name is freed after copying.
    unsafe {
        for i in 0..items.GetCount().unwrap_or(0) {
            let Ok(item) = items.GetItemAt(i) else {
                continue;
            };
            let Ok(name) = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) else {
                continue;
            };
            if let Ok(text) = name.to_string() {
                out.push(text);
            }
            CoTaskMemFree(Some(name.0.cast_const().cast()));
        }
    }
    out
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        object: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        touch();
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let unknown: IUnknown = FolderOpen {
            selection: Mutex::new(None),
        }
        .into();
        // SAFETY: riid and object come from COM as QueryInterface takes them.
        unsafe { unknown.query(riid, object) }.ok()
    }
    fn LockServer(&self, _: BOOL) -> windows::core::Result<()> {
        touch();
        Ok(())
    }
}

/// Registers the class for this process's life, from a thread of COM's multithreaded apartment
/// that stays for that long. Doesn't wait: the receiver says whether it worked.
pub(crate) fn serve() -> flume::Receiver<bool> {
    let (done_tx, done) = flume::bounded::<bool>(1);
    let (queue, folders) = flume::unbounded::<Vec<String>>();
    if QUEUE.set(queue).is_err() {
        let _ = done_tx.send(true);
        return done;
    }
    crate::explorer::spawn_thread("folder-com-open", move || {
        while let Ok(names) = folders.recv() {
            for name in names {
                open(&name);
            }
        }
    });
    crate::explorer::spawn_thread("folder-com", move || {
        // SAFETY: first COM call on this thread, which stays in the MTA until the process ends.
        let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if let Err(err) = joined.ok() {
            log::warn!("folder COM: cannot join the MTA: {err}");
            let _ = done_tx.send(false);
            return;
        }
        let factory: IClassFactory = Factory.into();
        // SAFETY: the factory lives as long as the registration (until the process ends).
        let registered = unsafe {
            CoRegisterClassObject(&CLSID, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE)
        };
        match registered {
            Ok(_) => {
                log::info!("folder COM: class registered");
                let _ = done_tx.send(true);
                // Keeps the apartment, and with it the registration, alive.
                loop {
                    std::thread::park();
                }
            }
            Err(err) => {
                log::warn!("folder COM: cannot register the class: {err}");
                let _ = done_tx.send(false);
            }
        }
    });
    done
}

/// COM started this Arke (`-Embedding`) though another one is resident (it had not registered the
/// class yet, or an older Arke is running): passes each call on to it, then returns once nothing
/// called for a while.
pub(crate) fn forward_until_idle() {
    if !serve().recv().unwrap_or(false) {
        return;
    }
    touch();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let last = Duration::from_millis(LAST_CALL_MS.load(Ordering::Relaxed));
        if START.elapsed().saturating_sub(last) >= IDLE_EXIT {
            log::info!("folder COM: idle, the forwarding Arke exits");
            return;
        }
    }
}

/// Routes one folder from `Execute` like a folder argument (`main.rs`) and hands it to the
/// resident Arke.
fn open(name: &str) {
    let (path, page) = match arke_core::takeover::route_argument(name) {
        Route::Explorer(target) => {
            crate::win::open_in_explorer(&target);
            return;
        }
        Route::Arke(Target::Home) if !crate::settings::home_opens_arke() => {
            crate::win::open_in_explorer(name);
            return;
        }
        Route::Arke(Target::Home) => (None, None),
        Route::Arke(Target::RecycleBin) => (None, Some(Page::RecycleBin)),
        Route::Arke(Target::Folder(path)) => (Some(path), None),
    };
    // "Show in folder" looks the folder's window up next: the stand-in collects what it selects.
    if let Some(folder) = path.clone().filter(|p| p.is_absolute()) {
        let _ = crate::win_openinfolder::start(folder, |folder, items| {
            let count = items.len();
            let outcome =
                instance::send(&Request::Select { folder, items }, instance::CLIENT_TIMEOUT);
            log::info!("shell window: {count} selected items forwarded: {outcome:?}");
        });
    }
    let request = Request::Open(OpenRequest {
        path,
        bench: None,
        bench_out: None,
        launched_unix_ms: crate::unix_ms_now(),
        page,
        new_window: false,
    });
    let deadline = Instant::now() + PIPE_WAIT;
    loop {
        touch();
        match instance::send(&request, instance::CLIENT_TIMEOUT) {
            SendOutcome::NoServer if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            outcome => {
                log::info!("folder COM: open handed over: {outcome:?}");
                return;
            }
        }
    }
}

/// Whether the shell would reach Arke through COM: runs `arke.exe --com-check` and waits for it
/// for a few seconds at most. Blocking; call from a background thread.
pub(crate) fn works() -> bool {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let started = Instant::now();
    let mut child = match std::process::Command::new(exe)
        .arg(CHECK_ARG)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            log::warn!("folder COM: cannot start the check: {err}");
            return false;
        }
    };
    let passed = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < CHECK_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                log::info!("folder COM: the check timed out (COM does not accept the class here)");
                break false;
            }
            Err(err) => {
                log::warn!("folder COM: cannot wait for the check: {err}");
                break false;
            }
        }
    };
    log::info!(
        "folder COM: check {} in {} ms",
        if passed { "passed" } else { "failed" },
        started.elapsed().as_millis()
    );
    passed
}

/// `arke.exe --com-check`: what the shell does, short of calling `Execute`.
pub(crate) fn check_cli() -> std::process::ExitCode {
    // SAFETY: first COM call of this short-lived process.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: plain activation; the object is released when dropped.
    let made: windows::core::Result<IExecuteCommand> =
        unsafe { CoCreateInstance(&CLSID, None, CLSCTX_LOCAL_SERVER) };
    if made.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// `-Embedding`: how COM starts a local server.
pub(crate) const fn is_embedding_arg(arg: &str) -> bool {
    arg.eq_ignore_ascii_case("-Embedding") || arg.eq_ignore_ascii_case("/Embedding")
}

/// [`CLSID`] as the registry spells it.
#[cfg(test)]
fn clsid_text() -> String {
    format!("{{{CLSID:?}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_id_is_the_one_the_folder_verbs_name() {
        assert!(
            clsid_text().eq_ignore_ascii_case(arke_core::folder_handler::ARKE_CLSID),
            "{} vs {}",
            clsid_text(),
            arke_core::folder_handler::ARKE_CLSID
        );
    }

    #[test]
    fn com_starts_a_server_with_either_embedding_flag() {
        assert!(is_embedding_arg("-Embedding"));
        assert!(is_embedding_arg("/Embedding"));
        assert!(is_embedding_arg("-embedding"));
        assert!(!is_embedding_arg("--embedding"));
    }
}
