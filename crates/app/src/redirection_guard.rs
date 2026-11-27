//! Windows' RedirectionGuard, inherited from Inno Setup (#406).
//!
//! Inno Setup 6.7.0 and later turn this mitigation on for Setup. 6.7.0 to 6.7.3 pass it on to the
//! programs its [Run] section starts, and 6.7.4 still does when Setup didn't have to ask for
//! approval, as in Arke's per-user install. A process with it can't follow junctions and symbolic
//! links a standard user made, such as Scoop's `current` folder (os error 448,
//! ERROR_UNTRUSTED_MOUNT_POINT), and every program it starts (a terminal, an editor) inherits it.
//!
//! So an Arke started that way, after an install or an update, starts again through the desktop
//! shell, whose processes don't have it, with the same arguments, and exits. The copy it starts
//! carries [`RELAUNCHED_ARG`] first, so it never does that again; when anything is in the way it
//! logs why and starts as usual. When that copy still has the mitigation, it is on for every
//! program (a policy): the settings remember it (`redirection_guard_by_policy`), so later starts
//! don't try again, until a start finds it off.

use std::sync::atomic::{AtomicU8, Ordering};

/// Marks the copy [`restart_unguarded`] starts, which only warns if it still has the mitigation.
/// It goes first: an argument left open at the end of the command line (a drive root, `"C:\"`)
/// would take in one added after it. `parse_args` drops it.
pub(crate) const RELAUNCHED_ARG: &str = "--relaunched";

/// What a starting Arke does about RedirectionGuard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Not on: start as usual.
    Start,
    /// On by a policy the settings remember: start as usual, without trying again.
    Known,
    /// Start again through the desktop shell, then exit.
    Relaunch,
    /// On, but starting again wouldn't help: say why, then start as usual.
    Warn(Why),
}

/// Why [`Action::Warn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// This copy already is the one started through the desktop shell: the mitigation comes from
    /// a policy, which the settings then remember.
    StillOn,
    /// No desktop shell to start from (Explorer isn't running, or another shell replaces it).
    NoShell,
}

/// The decision, from whether the mitigation is on (`enforced`), whether this copy was started by
/// a relaunch (`relaunched`), whether the settings say a policy turns it on (`by_policy`) and
/// whether there is a desktop shell (`shell`).
#[allow(
    clippy::fn_params_excessive_bools,
    reason = "four yes/no facts; a struct of four bools trips the struct lint instead"
)]
pub(crate) const fn decide(
    enforced: bool,
    relaunched: bool,
    by_policy: bool,
    shell: bool,
) -> Action {
    match (enforced, relaunched, by_policy, shell) {
        (false, ..) => Action::Start,
        (true, _, true, _) => Action::Known,
        (true, true, false, _) => Action::Warn(Why::StillOn),
        (true, false, false, false) => Action::Warn(Why::NoShell),
        (true, false, false, true) => Action::Relaunch,
    }
}

/// What a start found out, for the settings once they are read ([`remembered_policy`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Finding {
    /// Not checked (tests, and the commands that exit before the check).
    NotChecked = 0,
    /// Off: a policy the settings remember is gone.
    Off = 1,
    /// On, nothing new about a policy.
    On = 2,
    /// Still on in the copy started through the desktop shell: a policy.
    OnAfterRelaunch = 3,
}

/// The `redirection_guard_by_policy` setting after a start that found `found`, from what the file
/// says (`stored`).
pub(crate) const fn policy_setting(stored: bool, found: Finding) -> bool {
    match found {
        Finding::NotChecked | Finding::On => stored,
        Finding::Off => false,
        Finding::OnAfterRelaunch => true,
    }
}

/// This start's [`Finding`], as its `u8`.
static FOUND: AtomicU8 = AtomicU8::new(Finding::NotChecked as u8);

fn found(finding: Finding) {
    FOUND.store(finding as u8, Ordering::Relaxed);
}

/// [`policy_setting`] for this start: the settings store applies it to the file it reads.
pub(crate) fn remembered_policy(stored: bool) -> bool {
    let finding = match FOUND.load(Ordering::Relaxed) {
        1 => Finding::Off,
        2 => Finding::On,
        3 => Finding::OnAfterRelaunch,
        _ => Finding::NotChecked,
    };
    policy_setting(stored, finding)
}

/// The parameters for the copy the relaunch starts: [`RELAUNCHED_ARG`], then this copy's own
/// arguments exactly as they came. `command_line` is the whole command line (`GetCommandLineW`),
/// program name included.
pub(crate) fn relaunch_parameters(command_line: &[u16]) -> Vec<u16> {
    let mut parameters: Vec<u16> = RELAUNCHED_ARG.encode_utf16().collect();
    let tail = arguments(command_line);
    if !tail.is_empty() {
        parameters.push(u16::from(b' '));
        parameters.extend_from_slice(tail);
    }
    parameters
}

/// What follows the program name, read the way Windows reads it: a quoted name runs to the next
/// quote, any other to the first space or tab; the blanks after it go too.
fn arguments(command_line: &[u16]) -> &[u16] {
    let quote = u16::from(b'"');
    let blank = |c: &u16| *c == u16::from(b' ') || *c == u16::from(b'\t');
    let name_end = if command_line.first() == Some(&quote) {
        command_line[1..]
            .iter()
            .position(|&c| c == quote)
            .map_or(command_line.len(), |at| at + 2)
    } else {
        command_line
            .iter()
            .position(blank)
            .unwrap_or(command_line.len())
    };
    let rest = &command_line[name_end..];
    let start = rest.iter().position(|c| !blank(c)).unwrap_or(rest.len());
    &rest[start..]
}

/// Whether this copy was started by a relaunch (this one's, or an elevated start's, see
/// `elevation.rs`).
pub(crate) fn relaunched() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == RELAUNCHED_ARG)
}

/// Checks this process for RedirectionGuard and, when it's on, starts Arke again without it.
/// `true`: the new copy is starting, this one exits now. Without the mitigation (almost always)
/// this is a single system call; with it, the settings file is read for the one setting.
pub(crate) fn restart_unguarded() -> bool {
    if !enforced() {
        found(Finding::Off);
        return false;
    }
    found(Finding::On);
    let relaunched = relaunched();
    let by_policy = crate::settings::redirection_guard_by_policy();
    match decide(true, relaunched, by_policy, shell_available()) {
        Action::Start => false,
        Action::Known => {
            log::debug!("RedirectionGuard is on by a policy (remembered): not starting again");
            false
        }
        Action::Warn(Why::StillOn) => {
            found(Finding::OnAfterRelaunch);
            log::warn!(
                "RedirectionGuard is on, also after starting again through the desktop shell, so \
                 a policy turns it on for every program; later starts won't try again. Junctions \
                 and symbolic links a standard user made (Scoop's, for one) can't be opened (os \
                 error 448)"
            );
            false
        }
        Action::Warn(Why::NoShell) => {
            log::warn!(
                "RedirectionGuard is on (started by an installer?) and there is no desktop shell \
                 to start again from: junctions and symbolic links a standard user made can't be \
                 opened (os error 448) until Arke is started from the Start menu"
            );
            false
        }
        Action::Relaunch => match relaunch() {
            Ok(()) => {
                log::info!("RedirectionGuard is on: started again through the desktop shell");
                true
            }
            Err(err) => {
                log::warn!(
                    "RedirectionGuard is on and starting again through the desktop shell failed \
                     ({err}): junctions and symbolic links a standard user made can't be opened \
                     (os error 448) until Arke is started from the Start menu"
                );
                false
            }
        },
    }
}

/// Whether RedirectionGuard is on for this process. Windows before 10 22H2 doesn't know the
/// policy and fails the call: nothing is enforced there.
fn enforced() -> bool {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessMitigationPolicy, ProcessRedirectionTrustPolicy,
    };
    // PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY is one DWORD of flags: EnforceRedirectionTrust
    // is bit 0, AuditRedirectionTrust bit 1.
    let mut flags = 0u32;
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing; the buffer is the
    // u32 above, and the length passed is its size.
    let read = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessRedirectionTrustPolicy,
            (&raw mut flags).cast(),
            size_of::<u32>(),
        )
    };
    read.is_ok() && (flags & 1) != 0
}

/// Whether a desktop shell is running (its desktop window exists).
fn shell_available() -> bool {
    // SAFETY: no arguments; it only returns a window handle, or null.
    !unsafe { windows::Win32::UI::WindowsAndMessaging::GetShellWindow() }.is_invalid()
}

/// Starts this exe again through the desktop shell, with this copy's arguments (after
/// [`RELAUNCHED_ARG`]) and working folder. Returns once the shell has the request. The new copy is
/// the shell's child: without this one's mitigations, and not elevated when the shell isn't.
pub(crate) fn relaunch() -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let file: Vec<u16> = exe.as_os_str().encode_wide().collect();
    let dir: Vec<u16> = std::env::current_dir().map_or_else(
        |_| Vec::new(),
        |dir| dir.as_os_str().encode_wide().collect(),
    );
    // SAFETY: no arguments; it returns this process's command line, valid for its life.
    let command_line = unsafe { windows::Win32::System::Environment::GetCommandLineW() };
    // SAFETY: `command_line` is a valid NUL-terminated string (above), copied right away.
    let parameters = relaunch_parameters(unsafe { command_line.as_wide() });
    // The new copy is started by the shell: let it take the foreground this copy may have.
    // SAFETY: no pointers; at worst it fails, which only means its window may open behind.
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    // On a thread of its own, so COM on the main thread is left as gpui expects to find it.
    let thread = std::thread::Builder::new()
        .name("relaunch".into())
        .spawn(move || shell_execute(&file, &parameters, &dir).map_err(|err| err.to_string()))
        .map_err(|err| err.to_string())?;
    thread
        .join()
        .unwrap_or_else(|_| Err("the relaunch thread panicked".into()))
}

/// `ShellExecute` run by the desktop shell (Explorer) instead of this process, so the new process
/// is Explorer's child and has none of this one's mitigations: the desktop's folder view hands out
/// the shell's `IShellDispatch2`, whose `ShellExecute` runs in Explorer.
fn shell_execute(file: &[u16], parameters: &[u16], dir: &[u16]) -> windows_core::Result<()> {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
    };
    // SAFETY: initializes STA on this thread, which does nothing else; undone below.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    // Every COM object is released inside, before COM is uninitialized.
    let result = run_in_shell(file, parameters, dir);
    if com.is_ok() {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

/// The COM part of [`shell_execute`]: the desktop window, its browser, its view, the view's
/// background object (`IShellFolderViewDual`), and that one's `Application` (`IShellDispatch2`).
fn run_in_shell(file: &[u16], parameters: &[u16], dir: &[u16]) -> windows_core::Result<()> {
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, IDispatch, IServiceProvider};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{
        IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellView, IShellWindows,
        SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows_core::{BSTR, Interface};
    // SAFETY: COM calls on this thread (COM initialized by the caller) on interfaces it got here;
    // each result is checked with `?` before it is used. The variants, BSTR strings and `hwnd` outlive the
    // calls that read them, and the interfaces are released when they go out of scope.
    unsafe {
        let shell_windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        // The desktop (CSIDL_DESKTOP, 0) among the shell's windows.
        let location = VARIANT::from(0i32);
        let root = VARIANT::default();
        let mut hwnd = 0i32;
        let desktop: IDispatch = shell_windows.FindWindowSW(
            &raw const location,
            &raw const root,
            SWC_DESKTOP,
            &raw mut hwnd,
            SWFO_NEEDDISPATCH,
        )?;
        let provider: IServiceProvider = desktop.cast()?;
        let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
        let view: IShellView = browser.QueryActiveShellView()?;
        let item: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
        let background: IShellFolderViewDual = item.cast()?;
        let application: IDispatch = background.Application()?;
        let shell: IShellDispatch2 = application.cast()?;
        shell.ShellExecute(
            &BSTR::from_wide(file),
            &VARIANT::from(BSTR::from_wide(parameters)),
            &VARIANT::from(BSTR::from_wide(dir)),
            // The default verb (open).
            &VARIANT::from(""),
            &VARIANT::from(SW_SHOWNORMAL.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunches_only_a_guarded_first_copy_with_a_shell() {
        let both = [false, true];
        // Not guarded: whatever else, start as usual.
        for relaunched in both {
            for by_policy in both {
                for shell in both {
                    assert_eq!(decide(false, relaunched, by_policy, shell), Action::Start);
                }
            }
        }
        // A policy the settings remember: never try again.
        for relaunched in both {
            for shell in both {
                assert_eq!(decide(true, relaunched, true, shell), Action::Known);
            }
        }
        assert_eq!(decide(true, false, false, true), Action::Relaunch);
        // The relaunched copy never relaunches again.
        let still_on = Action::Warn(Why::StillOn);
        assert_eq!(decide(true, true, false, true), still_on);
        assert_eq!(decide(true, true, false, false), still_on);
        let no_shell = Action::Warn(Why::NoShell);
        assert_eq!(decide(true, false, false, false), no_shell);
    }

    #[test]
    fn a_policy_is_remembered_until_a_start_finds_the_guard_off() {
        for stored in [false, true] {
            assert_eq!(policy_setting(stored, Finding::NotChecked), stored);
            assert_eq!(policy_setting(stored, Finding::On), stored);
            assert!(policy_setting(stored, Finding::OnAfterRelaunch));
            assert!(!policy_setting(stored, Finding::Off), "policy removed");
        }
    }

    fn parameters(command_line: &str) -> String {
        let wide: Vec<u16> = command_line.encode_utf16().collect();
        String::from_utf16_lossy(&relaunch_parameters(&wide))
    }

    #[test]
    fn relaunch_keeps_the_arguments_as_they_came() {
        // What Setup's [Run] entries pass.
        assert_eq!(
            parameters(r#""C:\Users\me\AppData\Local\Programs\Arke\arke.exe" --after-update 0"#),
            "--relaunched --after-update 0"
        );
        assert_eq!(
            parameters(r#""C:\Program Files\Arke\arke.exe" --background"#),
            "--relaunched --background"
        );
        assert_eq!(
            parameters(r#""C:\Program Files\Arke\arke.exe""#),
            "--relaunched"
        );
        assert_eq!(
            parameters(r#""C:\Program Files\Arke\arke.exe"   "#),
            "--relaunched"
        );
        // Unquoted program name, tabs, quoting inside the arguments.
        assert_eq!(
            parameters("arke.exe\t\"D:\\My Folder\" --new-window"),
            "--relaunched \"D:\\My Folder\" --new-window"
        );
        assert_eq!(parameters("arke.exe"), "--relaunched");
        // A drive root as the shell passes it stays exactly as it came (`parse_args` reads it).
        assert_eq!(
            parameters(r#""C:\Arke\arke.exe" "C:\""#),
            r#"--relaunched "C:\""#
        );
        // A program name whose quote never closes is all name.
        assert_eq!(parameters(r#""C:\Arke\arke.exe --quit"#), "--relaunched");
        assert_eq!(parameters(""), "--relaunched");
    }
}
