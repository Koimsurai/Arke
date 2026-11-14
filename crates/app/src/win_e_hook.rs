//! Win+E where Windows no longer asks the registration (#34).
//!
//! "Win+E and the File Explorer icon open Arke too" registers Arke for the `opennewwindow` verb of
//! `{52205fd8-...}` (`folder_handler.rs`). Since late 2026 Windows rolls out a "direct launch"
//! path for Win+E: `explorer.exe` hands the key straight to a window File Explorer registered in
//! `HKCU\...\Explorer\SessionInfo\<session>\DirectLaunchHandlers` (`microsoft.windows.explorer`)
//! and never looks at the verb, so the registration is in place but File Explorer opens anyway.
//!
//! Only on those PCs, and only while Arke keeps running in the background, a low-level keyboard
//! hook takes Win+E before Explorer sees it and opens an Arke window instead. Nothing flashes:
//! Explorer never gets the key. The hook looks at nothing but the E key while a Windows key is
//! held; every other key goes straight on, and nothing is recorded. When Arke isn't running, or
//! the foreground window belongs to an elevated program (Windows doesn't pass its keys to the
//! hook), Win+E does what Windows does by itself.
//!
//! The hook runs on its own thread with a message loop (Windows calls it from there) and does
//! nothing slow: it posts the request to the UI and returns.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use windows::Win32::Foundation::{ERROR_SUCCESS, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_ANY, RegGetValueW};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};
use windows::core::PCWSTR;

const VK_E: u16 = 0x45;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
/// Unassigned virtual key: pressing it while Win is down tells the Start menu a shortcut was used,
/// so releasing Win doesn't open Start.
const VK_MASK: u16 = 0xE8;

/// Whether this PC sends Win+E through File Explorer's direct launch path (see the module docs).
/// Read only; the value is rewritten by Windows each time Explorer starts.
pub(crate) fn direct_launch_active() -> bool {
    let mut session = 0u32;
    // SAFETY: valid out pointer; on failure session stays 0 and the lookup just misses.
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) };
    let key = wide(&format!(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\SessionInfo\{session}\DirectLaunchHandlers"
    ));
    let value = wide("microsoft.windows.explorer");
    // SAFETY: both strings are NUL-terminated and outlive the call; no data is read back, only
    // whether the value exists.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_ANY,
            None,
            None,
            None,
        )
    };
    code == ERROR_SUCCESS
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What the hook does with one key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not ours: Windows and the foreground program get it as usual.
    Pass,
    /// Win+E: keep it from Explorer and open Arke.
    Open,
    /// The rest of a Win+E already handled (key repeat, the E release): keep it from Explorer.
    Swallow,
}

/// Decides one key event. `held` answers whether a key was down before this event; `e_taken`
/// remembers that the E press went to Arke, so its repeats and release don't reach Explorer.
pub(crate) fn decide(
    vk: u16,
    down: bool,
    held: impl Fn(u16) -> bool,
    e_taken: &mut bool,
) -> Verdict {
    if vk != VK_E {
        return Verdict::Pass;
    }
    if !down {
        return if std::mem::take(e_taken) {
            Verdict::Swallow
        } else {
            Verdict::Pass
        };
    }
    if *e_taken {
        return Verdict::Swallow;
    }
    // Plain Win+E only: Win+Shift+E, Ctrl+Win+E and the like belong to other programs.
    let win = held(VK_LWIN) || held(VK_RWIN);
    if win && !held(VK_SHIFT) && !held(VK_CONTROL) && !held(VK_MENU) {
        *e_taken = true;
        Verdict::Open
    } else {
        Verdict::Pass
    }
}

/// Where the hook sends Win+E presses; set while a hook runs.
static TARGET: OnceLock<Mutex<Option<flume::Sender<()>>>> = OnceLock::new();
static E_TAKEN: AtomicBool = AtomicBool::new(false);

fn target() -> &'static Mutex<Option<flume::Sender<()>>> {
    TARGET.get_or_init(Mutex::default)
}

/// The running hook; dropping it removes the hook and ends its thread.
pub(crate) struct Hook {
    thread_id: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
}

impl Hook {
    /// Installs the hook on a new thread; each Win+E arrives on `tx`.
    pub(crate) fn start(tx: flume::Sender<()>) -> Option<Self> {
        *target().lock() = Some(tx);
        let thread_id = Arc::new(AtomicU32::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let (id, flag) = (thread_id.clone(), stopped.clone());
        std::thread::Builder::new()
            .name("win-e-hook".into())
            .spawn(move || run(&id, &flag))
            .ok()?;
        Some(Self { thread_id, stopped })
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        *target().lock() = None;
        let id = self.thread_id.load(Ordering::SeqCst);
        if id != 0 {
            // SAFETY: posts WM_QUIT to our own hook thread, which unhooks and exits.
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }
}

fn run(thread_id: &AtomicU32, stopped: &AtomicBool) {
    // SAFETY: the module handle of our own exe; the procedure lives in it for the whole run.
    let module = unsafe { GetModuleHandleW(None) }.ok();
    // SAFETY: `on_key` matches HOOKPROC and stays valid; the hook is removed below on this thread.
    let hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), module.map(Into::into), 0) }
        {
            Ok(hook) => hook,
            Err(err) => {
                log::warn!("Win+E hook: cannot install: {err}");
                return;
            }
        };
    // SAFETY: plain Win32 call.
    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    log::info!("Win+E hook: on (Windows sends Win+E straight to File Explorer here)");
    if !stopped.load(Ordering::SeqCst) {
        let mut msg = MSG::default();
        // SAFETY: standard message loop on this thread; it ends with WM_QUIT from `Hook::drop`.
        while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {}
    }
    // SAFETY: the hook installed above, removed on the thread that installed it.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    E_TAKEN.store(false, Ordering::SeqCst);
    log::info!("Win+E hook: off");
}

fn key_held(vk: u16) -> bool {
    // SAFETY: plain Win32 call.
    unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
}

/// The low-level keyboard procedure: keep it short, Windows drops hooks that answer slowly.
unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION.cast_signed() {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
        let vk = u16::try_from(info.vkCode).unwrap_or(0);
        let mut taken = E_TAKEN.load(Ordering::SeqCst);
        let verdict = decide(vk, down, key_held, &mut taken);
        E_TAKEN.store(taken, Ordering::SeqCst);
        match verdict {
            Verdict::Pass => {}
            Verdict::Swallow => return LRESULT(1),
            Verdict::Open => {
                mask_start_menu();
                if let Some(tx) = target().lock().as_ref() {
                    let _ = tx.try_send(());
                }
                return LRESULT(1);
            }
        }
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Presses and releases an unassigned key so releasing Win after Win+E doesn't open Start.
fn mask_start_menu() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_MASK),
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS::default()), key(KEYEVENTF_KEYUP)];
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let size = std::mem::size_of::<INPUT>() as i32;
    // SAFETY: two fully initialised keyboard inputs.
    let _ = unsafe { SendInput(&inputs, size) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS::default()
                    },
                    ..Default::default()
                },
            },
        }
    }

    /// The real hook on this desktop: presses Win+E once and checks the hook took it. It sends a
    /// real key press, so it only runs on request:
    /// `cargo test -p arke-app live_hook_takes_win_e -- --ignored`.
    #[test]
    #[ignore = "presses Win+E on the real desktop"]
    fn live_hook_takes_win_e() {
        let (tx, rx) = flume::bounded(4);
        let hook = Hook::start(tx).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let keys = [
            press(VK_LWIN, false),
            press(VK_E, false),
            press(VK_E, true),
            press(VK_LWIN, true),
        ];
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let size = std::mem::size_of::<INPUT>() as i32;
        // SAFETY: four fully initialised keyboard inputs.
        let sent = unsafe { SendInput(&keys, size) };
        assert_eq!(sent, 4);
        let got = rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(hook);
        assert!(got.is_ok(), "the hook should have taken Win+E");
        assert!(rx.try_recv().is_err(), "once, not per repeat or release");
        assert!(!E_TAKEN.load(Ordering::SeqCst), "the E release was seen");
    }

    fn run_keys(events: &[(u16, bool)], held: &[u16]) -> Vec<Verdict> {
        let mut taken = false;
        events
            .iter()
            .map(|&(vk, down)| decide(vk, down, |k| held.contains(&k), &mut taken))
            .collect()
    }

    #[test]
    fn takes_win_e_and_its_repeats_and_release() {
        use Verdict::{Open, Swallow};
        let got = run_keys(&[(VK_E, true), (VK_E, true), (VK_E, false)], &[VK_LWIN]);
        assert_eq!(got, [Open, Swallow, Swallow]);
        let got = run_keys(&[(VK_E, true), (VK_E, false)], &[VK_RWIN]);
        assert_eq!(got, [Open, Swallow], "the right Windows key too");
    }

    #[test]
    fn leaves_every_other_key_alone() {
        use Verdict::Pass;
        assert_eq!(
            run_keys(&[(VK_E, true), (VK_E, false)], &[]),
            [Pass, Pass],
            "plain E"
        );
        assert_eq!(
            run_keys(&[(0x52, true), (0x52, false)], &[VK_LWIN]),
            [Pass, Pass],
            "Win+R"
        );
        for modifier in [VK_SHIFT, VK_CONTROL, VK_MENU] {
            assert_eq!(
                run_keys(&[(VK_E, true), (VK_E, false)], &[VK_LWIN, modifier]),
                [Pass, Pass],
                "Win+E with {modifier:#x} belongs to someone else"
            );
        }
        assert_eq!(
            run_keys(&[(VK_LWIN, true), (VK_LWIN, false)], &[]),
            [Pass, Pass]
        );
    }

    #[test]
    fn a_release_after_win_is_let_go_still_finishes_the_press() {
        // Win released before E: the E release must still not reach Explorer alone.
        let mut taken = false;
        assert_eq!(
            decide(VK_E, true, |k| k == VK_LWIN, &mut taken),
            Verdict::Open
        );
        assert_eq!(decide(VK_E, false, |_| false, &mut taken), Verdict::Swallow);
        assert_eq!(
            decide(VK_E, true, |_| false, &mut taken),
            Verdict::Pass,
            "next E is typing"
        );
    }
}
