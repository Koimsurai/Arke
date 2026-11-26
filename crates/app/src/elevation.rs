//! Arke never runs as administrator (#451).
//!
//! A program running as administrator that opens a folder while the folder verbs run the command
//! (not Arke's COM class: registrations from before 0.9.2, or a per-machine install where COM
//! refuses the class) starts `arke.exe` as administrator too. Such an Arke hands the folder to the
//! resident Arke as usual, but never becomes the resident Arke itself: with none running it starts
//! again through the desktop shell, whose programs run as the user, and exits
//! (`redirection_guard::relaunch`). Only when the shell can't give it a standard copy (no shell, the
//! shell itself runs as administrator, or the copy it started is elevated again) does it open its
//! window as administrator, without residency, and say so in the log.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The desktop shell, as an elevated Arke sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shell {
    /// No desktop shell window (Explorer isn't running, or another shell replaces it).
    Missing,
    /// The shell runs as administrator too (UAC off, the built-in Administrator account): what it
    /// starts is elevated as well.
    Elevated,
    /// The shell runs as the user: what it starts isn't elevated.
    Standard,
}

/// What an elevated Arke does when it would otherwise become the resident Arke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Start again through the desktop shell, then exit.
    Relaunch,
    /// A standard copy can't be had: open the window here, without residency.
    Standalone(Why),
}

/// Why [`Plan::Standalone`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// This is the copy the shell started, and it is elevated all the same.
    StillElevated,
    NoShell,
    ShellElevated,
}

/// The plan, from whether this copy is the one a relaunch started (`relaunched`) and the shell.
/// A relaunched copy never relaunches again, so nothing loops.
pub(crate) const fn decide(relaunched: bool, shell: Shell) -> Plan {
    match (relaunched, shell) {
        (true, _) => Plan::Standalone(Why::StillElevated),
        (false, Shell::Missing) => Plan::Standalone(Why::NoShell),
        (false, Shell::Elevated) => Plan::Standalone(Why::ShellElevated),
        (false, Shell::Standard) => Plan::Relaunch,
    }
}

/// An elevated Arke that would become resident: starts again through the desktop shell when that
/// gives a standard copy. `true`: the new copy is starting, this one exits now; `false`: it runs
/// here, without residency (the log says why).
pub(crate) fn start_unelevated() -> bool {
    match decide(crate::redirection_guard::relaunched(), shell()) {
        Plan::Relaunch => match crate::redirection_guard::relaunch() {
            Ok(()) => {
                log::info!("started as administrator: started again through the desktop shell");
                true
            }
            Err(err) => {
                log::warn!(
                    "started as administrator, and starting again through the desktop shell \
                     failed ({err}): running without residency"
                );
                false
            }
        },
        Plan::Standalone(why) => {
            log::warn!(
                "started as administrator and no standard copy can be started ({why:?}): running \
                 without residency"
            );
            false
        }
    }
}

/// The desktop shell's state: its window, and whether the process behind it is elevated. A shell
/// whose token can't be read counts as standard: the relaunched copy checks again.
fn shell() -> Shell {
    use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
    // SAFETY: no arguments; it only returns a window handle, or null.
    let window = unsafe { GetShellWindow() };
    if window.is_invalid() {
        return Shell::Missing;
    }
    let mut pid = 0u32;
    // SAFETY: a window handle (a stale one only makes the call fail); pid is a valid out pointer.
    if unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) } == 0 || pid == 0 {
        return Shell::Standard;
    }
    // SAFETY: plain call; the handle is closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return Shell::Standard;
    };
    let elevated = process_elevated(process);
    // SAFETY: closes the handle opened above.
    let _ = unsafe { CloseHandle(process) };
    if elevated == Some(true) {
        Shell::Elevated
    } else {
        Shell::Standard
    }
}

/// Whether this process runs as administrator (its token is elevated).
pub(crate) fn is_elevated() -> bool {
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    process_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)
}

/// Whether `process`'s token is elevated; `None` when it can't be read.
fn process_elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    // SAFETY: process is a valid handle; token is a valid out pointer.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }.ok()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: the buffer is the TOKEN_ELEVATION above and the length passed is its size.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    };
    // SAFETY: closes the token handle opened above.
    let _ = unsafe { CloseHandle(token) };
    read.ok()?;
    Some(elevation.TokenIsElevated != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_elevated_start_relaunches_only_when_the_shell_gives_a_standard_copy() {
        assert_eq!(decide(false, Shell::Standard), Plan::Relaunch);
        assert_eq!(
            decide(false, Shell::Missing),
            Plan::Standalone(Why::NoShell)
        );
        assert_eq!(
            decide(false, Shell::Elevated),
            Plan::Standalone(Why::ShellElevated)
        );
        // The copy the shell started is elevated again: never a second relaunch.
        for shell in [Shell::Standard, Shell::Missing, Shell::Elevated] {
            assert_eq!(decide(true, shell), Plan::Standalone(Why::StillElevated));
        }
    }
}
