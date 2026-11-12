//! The Windows "Open" dialog behind the Browse buttons in Settings (background image, "Open
//! with" program, settings import).
//!
//! `IFileOpenDialog` loads third-party shell extensions (namespace extensions, thumbnail and
//! preview handlers, icon overlays) into the process that shows it, and Arke never loads those
//! itself (see `shell_host.rs`). So the dialog runs in a short-lived child, `arke.exe
//! --pick-file`: Arke writes one JSON request line to its stdin, the child shows the dialog
//! owned by Arke's window (modal to it, as an in-process dialog would be) and writes one JSON
//! reply line with the chosen path, then exits.
//!
//! Arke waits for the reply on a background thread; the UI thread never blocks on the dialog.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_DONTADDTORECENT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FileOpenDialog,
    IFileOpenDialog, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::PCWSTR;

use crate::shell::{PickRequest, ShellError};
use crate::win_fileops::{Com, wide};

/// Starts arke.exe as a one-shot file dialog: no window of its own, no tray, no settings.
pub(crate) const PICK_ARG: &str = "--pick-file";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    /// Arke's window, which owns the dialog.
    owner: Option<isize>,
    pick: PickRequest,
}

/// The chosen file (`None` on cancel), or why the dialog failed.
#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    result: Result<Option<PathBuf>, String>,
}

// ───────────────────────── Child side ─────────────────────────

/// Entry point of `arke.exe --pick-file`: one request in, one reply out.
pub(crate) fn run() {
    let mut line = String::new();
    let request = std::io::stdin()
        .read_line(&mut line)
        .ok()
        .and_then(|_| serde_json::from_str::<Request>(&line).ok());
    let reply = Reply {
        result: match request {
            Some(request) => show(&request.pick, request.owner).map_err(|e| e.to_string()),
            None => Err("bad request".into()),
        },
    };
    if let Ok(text) = serde_json::to_string(&reply) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{text}").and_then(|()| out.flush());
    }
}

/// Shows the dialog and waits for it to close.
fn show(pick: &PickRequest, owner: Option<isize>) -> Result<Option<PathBuf>, ShellError> {
    let _com = Com::init()?;
    let title = wide(std::ffi::OsStr::new(&pick.title));
    let filter_name = wide(std::ffi::OsStr::new(&pick.filter_name));
    let patterns = wide(std::ffi::OsStr::new(&pick.patterns));
    let filters = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(filter_name.as_ptr()),
        pszSpec: PCWSTR(patterns.as_ptr()),
    }];
    // SAFETY: COM is initialized on this thread for the whole block; every string pointer
    // handed to the dialog outlives the calls that use it; the display name is freed once.
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetFileTypes(&filters)?;
        dialog.SetTitle(PCWSTR(title.as_ptr()))?;
        let options = dialog.GetOptions()?;
        dialog.SetOptions(
            options
                | FOS_FILEMUSTEXIST
                | FOS_PATHMUSTEXIST
                | FOS_FORCEFILESYSTEM
                | FOS_DONTADDTORECENT,
        )?;
        if let Some(folder) = &pick.folder {
            let folder = wide(folder.as_os_str());
            if let Ok(item) = SHCreateItemFromParsingName::<
                _,
                _,
                windows::Win32::UI::Shell::IShellItem,
            >(PCWSTR(folder.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
        }
        let owner = owner.map(|h| HWND(h as *mut core::ffi::c_void));
        if let Err(e) = dialog.Show(owner) {
            return if e.code() == ERROR_CANCELLED.to_hresult() {
                Ok(None)
            } else {
                Err(e.into())
            };
        }
        let item = dialog.GetResult()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        // Wide string as is, so unpaired surrogates in names survive.
        let path = PathBuf::from(std::ffi::OsString::from_wide(name.as_wide()));
        CoTaskMemFree(Some(name.0 as *const _));
        Ok(Some(path))
    }
}

// ───────────────────────── Arke side ─────────────────────────

/// Shows the dialog in a child process owned by `owner` and returns the chosen file, or `None`
/// on cancel. Blocks until the dialog closes; background thread.
pub(crate) fn pick(
    pick: &PickRequest,
    owner: Option<isize>,
) -> Result<Option<PathBuf>, ShellError> {
    debug_assert!(!arke_core::fs::is_ui_thread());
    let request = serde_json::to_string(&Request {
        owner,
        pick: pick.clone(),
    })
    .map_err(ShellError::Encode)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg(PICK_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    // Arke has the foreground; let the child bring its dialog to the front.
    // SAFETY: plain Win32 call taking the child's PID.
    let _ = unsafe { AllowSetForegroundWindow(child.id()) };
    let mut line = String::new();
    let talked = (|| -> std::io::Result<()> {
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "{request}")?;
        }
        if let Some(stdout) = child.stdout.take() {
            BufReader::new(stdout).read_line(&mut line)?;
        }
        Ok(())
    })();
    let _ = child.wait();
    // A modal dialog disables its owner and enables it again when it closes. If the child died
    // with the dialog open that never happened; don't leave Arke's window disabled.
    if let Some(owner) = owner {
        let hwnd = HWND(owner as *mut core::ffi::c_void);
        // SAFETY: plain Win32 calls on Arke's own window handle.
        unsafe {
            if !IsWindowEnabled(hwnd).as_bool() {
                let _ = EnableWindow(hwnd, true);
            }
        }
    }
    talked?;
    let reply: Reply = serde_json::from_str(line.trim()).map_err(|_| ShellError::HostGone)?;
    reply.result.map_err(ShellError::Host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests and replies survive the pipe, including non-ASCII paths and a cancel.
    #[test]
    fn messages_cross_the_wire() {
        let request = Request {
            owner: Some(0x1234),
            pick: PickRequest {
                title: "背景".into(),
                filter_name: "Images".into(),
                patterns: "*.png;*.jpg".into(),
                folder: Some(PathBuf::from(r"C:\Users\me\圖片")),
            },
        };
        let back: Request =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(back.owner, Some(0x1234));
        assert_eq!(back.pick, request.pick);
        for result in [
            Ok(Some(PathBuf::from(r"D:\相片\a.png"))),
            Ok(None),
            Err("x".to_string()),
        ] {
            let reply = Reply {
                result: result.clone(),
            };
            let back: Reply =
                serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
            assert_eq!(back.result, result);
        }
    }
}
