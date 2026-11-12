//! "More options": reads the Windows context menu, including third-party shell extensions,
//! so Arke can draw it itself.
//!
//! Runs only inside the menu host (`arke.exe --shell-host`, see `shell_host.rs`), because
//! reading the menu loads extensions that must stay out of Arke's process. The menu is built
//! with `IContextMenu::QueryContextMenu` but never shown; each item's text, icon, submenu and
//! state is read and sent back. A chosen item is run with `InvokeCommand` on the same
//! `IContextMenu`.
//!
//! Everything runs on one long-lived STA thread that keeps pumping messages: some extensions
//! continue via messages after `InvokeCommand` returns (e.g. Send to > Compressed folder) and
//! break if the thread exits.
//!
//! Only one menu (`Session`) is kept at a time; it is released on the next query, after
//! invoking, or when the menu closes without a choice.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, HBITMAP};
use windows::Win32::System::Ole::{OleFlushClipboard, OleInitialize, OleUninitialize};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW,
    IContextMenu, IContextMenu2, IContextMenu3, ILFindLastID, ILFree, IShellFolder, SHBindToParent,
    SHGetDesktopFolder, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW, MFS_CHECKED, MFS_DEFAULT,
    MFS_DISABLED, MFT_BITMAP, MFT_SEPARATOR, MIIM_BITMAP, MIIM_CHECKMARKS, MIIM_FTYPE, MIIM_ID,
    MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
    SW_SHOWNORMAL, TranslateMessage, WINDOW_EX_STYLE, WM_DRAWITEM, WM_INITMENUPOPUP,
    WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW, WS_POPUP,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR, w};

use crate::shell::{ShellError, ShellMenu, ShellMenuEntry, tidy_separators};
use crate::win_fileops::wide;

const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
/// Submenu depth and total item limits; some extensions add very long lists.
const MAX_DEPTH: usize = 4;
const MAX_ITEMS: usize = 400;
/// Retries, 30 ms apart, while a submenu is still being filled.
const INIT_TRIES: u32 = 50;

enum Request {
    Query {
        paths: Vec<PathBuf>,
        background: bool,
        done: flume::Sender<Result<ShellMenu, ShellError>>,
    },
    Invoke {
        session: u64,
        id: u32,
        /// Physical pixels in the Arke window's client area.
        client_point: (i32, i32),
        owner: Option<isize>,
        done: flume::Sender<Result<(), ShellError>>,
    },
    Release(u64),
    /// Process is exiting: hand clipboard data set by extensions (e.g. Copy) to the system.
    FlushClipboard(flume::Sender<()>),
}

/// A menu waiting for a choice. Frees the HMENU and PIDLs on drop.
struct Session {
    id: u64,
    menu: IContextMenu,
    hmenu: HMENU,
    owner: HWND,
    _pidls: Pidls,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: hmenu was created by CreatePopupMenu and not yet destroyed.
        let _ = unsafe { DestroyMenu(self.hmenu) };
    }
}

thread_local! {
    /// Current menu; menu messages (WM_INITMENUPOPUP etc.) are forwarded to it.
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
}

fn menu_thread() -> Option<&'static flume::Sender<Request>> {
    static SENDER: OnceLock<Option<flume::Sender<Request>>> = OnceLock::new();
    SENDER
        .get_or_init(|| {
            let (tx, rx) = flume::unbounded::<Request>();
            std::thread::Builder::new()
                .name("shell-menu".into())
                .spawn(move || run(&rx))
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

/// Reads the context menu for `paths`. Blocks on the STA thread, so call from a background thread.
pub(crate) fn query(paths: &[PathBuf], background: bool) -> Result<ShellMenu, ShellError> {
    let tx = menu_thread().ok_or(ShellError::MenuThreadStart)?;
    let (done, result) = flume::bounded(1);
    tx.send(Request::Query {
        paths: paths.to_vec(),
        background,
        done,
    })
    .map_err(|_| ShellError::MenuThreadGone)?;
    result.recv().map_err(|_| ShellError::MenuThreadNoReply)?
}

/// Invokes a menu item. Commands may open windows or run long, so call from a background thread.
pub(crate) fn invoke(
    session: u64,
    id: u32,
    client_point: (i32, i32),
    owner: Option<isize>,
) -> Result<(), ShellError> {
    let tx = menu_thread().ok_or(ShellError::MenuThreadStart)?;
    let (done, result) = flume::bounded(1);
    tx.send(Request::Invoke {
        session,
        id,
        client_point,
        owner,
        done,
    })
    .map_err(|_| ShellError::MenuThreadGone)?;
    result.recv().map_err(|_| ShellError::MenuThreadNoReply)?
}

/// Releases a menu without waiting.
pub(crate) fn release(session: u64) {
    if let Some(tx) = menu_thread() {
        let _ = tx.send(Request::Release(session));
    }
}

fn run(rx: &flume::Receiver<Request>) {
    // OleInitialize rather than CoInitialize: Copy/Cut commands use the OLE clipboard.
    // SAFETY: per-thread init, paired with OleUninitialize via OleGuard.
    if unsafe { OleInitialize(None) }.is_err() {
        return;
    }
    let _ole = OleGuard;
    let Some(window) = create_owner_window() else {
        return;
    };
    loop {
        pump_messages();
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(Request::Query {
                paths,
                background,
                done,
            }) => {
                CURRENT.with(|c| c.borrow_mut().take());
                let _ = done.send(build(&paths, background, window));
            }
            Ok(Request::Invoke {
                session,
                id,
                client_point,
                owner,
                done,
            }) => {
                let current = CURRENT.with(|c| c.borrow_mut().take());
                let result = match current {
                    Some(s) if s.id == session => invoke_in(&s, id, client_point, owner),
                    _ => Err(ShellError::MenuExpired),
                };
                let _ = done.send(result);
            }
            Ok(Request::FlushClipboard(done)) => {
                // SAFETY: plain OLE call; it errors harmlessly if this thread set no clipboard data.
                let _ = unsafe { OleFlushClipboard() };
                let _ = done.send(());
            }
            Ok(Request::Release(session)) => CURRENT.with(|c| {
                let mut c = c.borrow_mut();
                if c.as_ref().is_some_and(|s| s.id == session) {
                    *c = None;
                }
            }),
            Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct OleGuard;

impl Drop for OleGuard {
    fn drop(&mut self) {
        // SAFETY: pairs with the successful OleInitialize at the start of run.
        unsafe { OleUninitialize() };
    }
}

/// Call before the process exits so clipboard data set by extensions survives it.
pub(crate) fn flush_clipboard() {
    if let Some(tx) = menu_thread() {
        let (done, wait) = flume::bounded(1);
        if tx.send(Request::FlushClipboard(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop; msg is a valid output buffer.
    unsafe {
        while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// Forwards menu messages to `IContextMenu3`/`IContextMenu2`; some extensions fill
/// submenus this way.
fn forward_menu_msg(
    menu: &IContextMenu,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        // SAFETY: menu3 is a valid interface on this thread; result is a valid out pointer.
        return unsafe { menu3.HandleMenuMsg2(msg, wparam, lparam, Some(&raw mut result)) }
            .ok()
            .map(|()| result);
    }
    let menu2 = menu.cast::<IContextMenu2>().ok()?;
    // SAFETY: menu2 is a valid interface on this thread.
    unsafe { menu2.HandleMenuMsg(msg, wparam, lparam) }
        .ok()
        .map(|()| LRESULT(0))
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let handled = CURRENT.with(|current| {
            let current = current.try_borrow().ok()?;
            forward_menu_msg(&current.as_ref()?.menu, msg, wparam, lparam)
        });
        if let Some(result) = handled {
            return result;
        }
    }
    // SAFETY: default handling for everything else, with the arguments we received.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Hidden zero-size owner window for extensions; only receives messages.
fn create_owner_window() -> Option<HWND> {
    let class = w!("ArkeShellMenuOwner");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: wc outlives the call; the class name is static; the window belongs to this thread.
    unsafe {
        RegisterClassW(&raw const wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
        .ok()
    }
}

/// Owned PIDLs, freed on drop.
struct Pidls(Vec<*mut ITEMIDLIST>);

impl Drop for Pidls {
    fn drop(&mut self) {
        for &pidl in &self.0 {
            // SAFETY: each pidl was allocated by SHParseDisplayName and not yet freed.
            unsafe { ILFree(Some(pidl.cast_const())) };
        }
    }
}

fn build(paths: &[PathBuf], background: bool, window: HWND) -> Result<ShellMenu, ShellError> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    if paths.is_empty() {
        return Ok(ShellMenu::default());
    }
    let mut pidls = Pidls(Vec::new());
    for path in paths {
        let w = wide(path.as_os_str());
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: w is NUL-terminated; pidl is a valid out pointer, owned by Pidls on success.
        unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &raw mut pidl, 0, None) }.map_err(
            |source| ShellError::Path {
                path: path.clone(),
                source,
            },
        )?;
        pidls.0.push(pidl);
    }
    // Owner is this thread's hidden window, so messages extensions send while building the
    // menu (e.g. "Send to" finishing its list) are handled here. Arke's window becomes the
    // owner only when invoking, as the dialog parent.
    let owner = window;
    let menu: IContextMenu = if background {
        // Folder background: the folder's own view object menu (New, Open in Terminal,
        // third-party background items).
        // SAFETY: binds from the desktop down to the folder; pidls.0[0] is a valid absolute PIDL.
        let folder: IShellFolder = unsafe {
            SHGetDesktopFolder().and_then(|desktop| desktop.BindToObject(pidls.0[0], None))
        }?;
        // SAFETY: owner is this thread's window.
        unsafe { folder.CreateViewObject(owner) }?
    } else {
        // Arke's selection is always within one folder, so bind to the first item's parent.
        // SAFETY: pidls.0[0] is a valid absolute PIDL.
        let folder: IShellFolder = unsafe { SHBindToParent(pidls.0[0], None) }?;
        // SAFETY: ILFindLastID points into the same allocation, valid until pidls is freed.
        let children: Vec<*const ITEMIDLIST> = pidls
            .0
            .iter()
            .map(|&p| unsafe { ILFindLastID(p) }.cast_const())
            .collect();
        // SAFETY: the PIDLs in children are valid for the call.
        unsafe { folder.GetUIObjectOf(owner, &children, None) }?
    };
    // SAFETY: creates an empty popup menu; Session calls DestroyMenu.
    let hmenu = unsafe { CreatePopupMenu() }?;
    let session = Session {
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        menu,
        hmenu,
        owner,
        _pidls: pidls,
    };
    // SAFETY: hmenu is valid; command IDs range over 1..=0x7FFF.
    let hr = unsafe {
        session
            .menu
            .QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL)
    };
    if hr.is_err() {
        return Err(windows::core::Error::from(hr).into());
    }
    let mut budget = MAX_ITEMS;
    let entries = read_menu(&session.menu, hmenu, 0, &mut budget);
    let result = ShellMenu {
        session: session.id,
        entries,
    };
    CURRENT.with(|c| *c.borrow_mut() = Some(session));
    Ok(result)
}

/// Reads one menu level. Submenus get `WM_INITMENUPOPUP` first, since "Send to",
/// "Open with" and others fill their content only then.
fn read_menu(
    menu: &IContextMenu,
    hmenu: HMENU,
    depth: usize,
    budget: &mut usize,
) -> Vec<ShellMenuEntry> {
    // SAFETY: hmenu is a valid menu.
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for pos in 0..u32::try_from(count).unwrap_or(0) {
        if *budget == 0 {
            break;
        }
        let Some(entry) = read_item(menu, hmenu, pos, depth, budget) else {
            continue;
        };
        // Extensions that refill a submenu can add the same command twice; keep one.
        if let ShellMenuEntry::Item { id, .. } = &entry
            && !seen.insert(*id)
        {
            continue;
        }
        *budget -= 1;
        out.push(entry);
    }
    tidy_separators(out)
}

fn read_item(
    menu: &IContextMenu,
    hmenu: HMENU,
    pos: u32,
    depth: usize,
    budget: &mut usize,
) -> Option<ShellMenuEntry> {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE
            | MIIM_STATE
            | MIIM_ID
            | MIIM_SUBMENU
            | MIIM_STRING
            | MIIM_BITMAP
            | MIIM_CHECKMARKS,
        ..Default::default()
    };
    // First call only asks for the string length.
    // SAFETY: info.cbSize is set; with a null dwTypeData only cch is returned.
    unsafe { GetMenuItemInfoW(hmenu, pos, true, &raw mut info) }.ok()?;
    if info.fType.0 & MFT_SEPARATOR.0 != 0 {
        return Some(ShellMenuEntry::Separator);
    }
    if info.fType.0 & MFT_BITMAP.0 != 0 {
        return None;
    }
    let mut text = vec![0u16; info.cch as usize + 1];
    info.dwTypeData = PWSTR(text.as_mut_ptr());
    info.cch += 1;
    // SAFETY: text has room for cch u16s including the NUL.
    unsafe { GetMenuItemInfoW(hmenu, pos, true, &raw mut info) }.ok()?;
    let len = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    let label = clean_label(&String::from_utf16_lossy(&text[..len]));
    // Owner-draw items without text can't be read; skip them.
    if label.is_empty() {
        return None;
    }
    let enabled = info.fState.0 & MFS_DISABLED.0 == 0;
    let icon = item_icon(info.hbmpItem).or_else(|| item_icon(info.hbmpUnchecked));
    if !info.hSubMenu.is_invalid() {
        if depth + 1 >= MAX_DEPTH {
            return None;
        }
        init_submenu(menu, info.hSubMenu, pos);
        let children = read_menu(menu, info.hSubMenu, depth + 1, budget);
        if children.is_empty() {
            return None;
        }
        return Some(ShellMenuEntry::Submenu {
            label,
            enabled,
            icon,
            children,
        });
    }
    Some(ShellMenuEntry::Item {
        id: info.wID,
        verb: item_verb(menu, info.wID),
        label,
        enabled,
        checked: info.fState.0 & MFS_CHECKED.0 != 0,
        default: info.fState.0 & MFS_DEFAULT.0 != 0,
        icon,
    })
}

/// Canonical verb ("open", "copy", "delete", ...) used to spot items that duplicate Arke's
/// own menu. Many extension items have none.
fn item_verb(menu: &IContextMenu, id: u32) -> Option<String> {
    let offset = (id as usize).checked_sub(1)?;
    let mut buf = [0u16; 64];
    // SAFETY: with GCS_VERBW pszname is a buffer of cchmax u16s; offset is the command ID
    // minus the idCmdFirst passed to QueryContextMenu.
    unsafe {
        menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr().cast()),
            buf.len() as u32,
        )
    }
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let verb = String::from_utf16_lossy(&buf[..len]);
    (!verb.is_empty()).then_some(verb)
}

/// Lets the extension fill a submenu (`WM_INITMENUPOPUP`: wParam is the submenu, low word
/// of lParam its position in the parent). "Send to" fills in the background and shows a
/// blank placeholder until ready, so pump messages and ask again. The first fill takes
/// about 0.7 s, later ones are near instant; the wait is capped at about 1.5 s.
fn init_submenu(menu: &IContextMenu, submenu: HMENU, pos: u32) {
    let lparam = LPARAM(isize::try_from(pos).unwrap_or_default());
    let init = || {
        let _ = forward_menu_msg(menu, WM_INITMENUPOPUP, WPARAM(submenu.0 as usize), lparam);
    };
    init();
    if has_text(submenu) {
        return;
    }
    // Slow submenus like "Send to" fill gradually; done once the item count is stable for a
    // few rounds. Only these get the message again (they refill fully each time).
    let mut last = -1;
    let mut stable = 0;
    for _ in 0..INIT_TRIES {
        pump_messages();
        std::thread::sleep(Duration::from_millis(30));
        init();
        if !has_text(submenu) {
            continue;
        }
        // SAFETY: submenu is a valid menu.
        let count = unsafe { GetMenuItemCount(Some(submenu)) };
        if count == last {
            stable += 1;
            if stable >= 3 {
                return;
            }
        } else {
            last = count;
            stable = 0;
        }
    }
}

/// Whether the menu has any item with text, or a submenu.
fn has_text(hmenu: HMENU) -> bool {
    // SAFETY: hmenu is a valid menu.
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    (0..u32::try_from(count).unwrap_or(0)).any(|pos| {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STRING | MIIM_SUBMENU,
            ..Default::default()
        };
        // SAFETY: only queries the string length (dwTypeData is null).
        unsafe { GetMenuItemInfoW(hmenu, pos, true, &raw mut info) }.is_ok()
            && info.fType.0 & MFT_SEPARATOR.0 == 0
            && (info.cch > 0 || !info.hSubMenu.is_invalid())
    })
}

/// Item bitmap. `HBMMENU_*` values (1..=11, -1) are system icon IDs, not real bitmaps.
fn item_icon(bitmap: HBITMAP) -> Option<arke_core::thumbs::Thumbnail> {
    let raw = bitmap.0 as isize;
    if (-1..=11).contains(&raw) {
        return None;
    }
    crate::win_icons::bitmap_pixels(bitmap)
}

/// Menu text without access-key markers (`&O`, `(&O)`) or the shortcut text after a tab.
pub(crate) fn clean_label(raw: &str) -> String {
    let text = raw.split('\t').next().unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    // CJK menus append the access key as "Open(O)": drop a trailing single alphanumeric
    // in parentheses.
    let trimmed = out.trim_end();
    let stripped = trimmed
        .strip_suffix(')')
        .and_then(|s| s.rsplit_once('('))
        .filter(|(_, key)| {
            key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(|(head, _)| head.trim_end().to_owned());
    stripped
        .unwrap_or_else(|| trimmed.to_owned())
        .trim()
        .to_owned()
}

fn invoke_in(
    session: &Session,
    id: u32,
    client_point: (i32, i32),
    owner: Option<isize>,
) -> Result<(), ShellError> {
    let owner = owner.map_or(session.owner, |h| HWND(h as *mut _));
    let mut pt = POINT {
        x: client_point.0,
        y: client_point.1,
    };
    // SAFETY: owner is the Arke window or this thread's hidden window; pt is a valid in/out pointer.
    unsafe {
        let _ = ClientToScreen(owner, &raw mut pt);
    }
    let Some(verb) = (id as usize).checked_sub(1) else {
        return Ok(());
    };
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        // MAKEINTRESOURCE: the low word is the command offset (IDs start at 1 in QueryContextMenu).
        lpVerb: PCSTR(verb as *const u8),
        lpVerbW: PCWSTR(verb as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    // SAFETY: info is a fully initialized CMINVOKECOMMANDINFOEX with correct cbSize, which may
    // be passed as CMINVOKECOMMANDINFO.
    unsafe {
        session
            .menu
            .InvokeCommand((&raw const info).cast::<CMINVOKECOMMANDINFO>())
    }
    .map_err(ShellError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_menu_labels() {
        assert_eq!(clean_label("&Open"), "Open");
        // zh-TW labels put the access key in parentheses after the text.
        assert_eq!(clean_label("開啟(&O)"), "開啟");
        assert_eq!(clean_label("傳送到(&N)"), "傳送到");
        // zh-CN uses the same style.
        assert_eq!(clean_label("打开(&O)"), "打开");
        assert_eq!(clean_label("发送到(&N)"), "发送到");
        assert_eq!(clean_label("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(clean_label("Copy\tCtrl+C"), "Copy");
        assert_eq!(clean_label("7-Zip"), "7-Zip");
        assert_eq!(clean_label("Version (2)"), "Version");
        assert_eq!(clean_label("Size (MB)"), "Size (MB)");
    }

    #[test]
    fn tidies_separators() {
        let item = |id| ShellMenuEntry::Item {
            id,
            verb: None,
            label: "x".into(),
            enabled: true,
            checked: false,
            default: false,
            icon: None,
        };
        let out = tidy_separators(vec![
            ShellMenuEntry::Separator,
            item(1),
            ShellMenuEntry::Separator,
            ShellMenuEntry::Separator,
            item(2),
            ShellMenuEntry::Separator,
        ]);
        let shape: Vec<bool> = out
            .iter()
            .map(|e| matches!(e, ShellMenuEntry::Separator))
            .collect();
        assert_eq!(shape, [false, true, false]);
    }

    /// Reads the context menu of the built-in notepad.exe without invoking anything.
    #[test]
    fn reads_a_real_shell_menu() {
        let windir = std::env::var("windir").unwrap_or_else(|_| r"C:\Windows".into());
        let path = PathBuf::from(windir).join(r"System32\notepad.exe");
        let menu = query(&[path], false).expect("read the menu");
        assert!(!menu.entries.is_empty());
        let labels: Vec<&str> = menu
            .entries
            .iter()
            .filter_map(|e| match e {
                ShellMenuEntry::Item { label, .. } | ShellMenuEntry::Submenu { label, .. } => {
                    Some(label.as_str())
                }
                ShellMenuEntry::Separator => None,
            })
            .collect();
        assert!(
            labels.iter().all(|l| !l.contains('&')),
            "access key markers must be removed: {labels:?}"
        );
        release(menu.session);
    }
}
