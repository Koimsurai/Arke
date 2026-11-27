//! Tells the shell which cloud folder an Arke window shows, the way File Explorer announces every
//! folder it opens (#319).
//!
//! Sync clients built on the Cloud Files API fill a folder with its online-only items in one of two
//! ways. With the `CF_POPULATION_POLICY_FULL` policy the cloud filter asks the client when any
//! program lists the folder, Arke included. With `CF_POPULATION_POLICY_ALWAYS_FULL` the filter
//! never asks, and some clients (WPS Cloud) only fill a folder once a shell window shows it: they
//! follow the shell's window list (`IShellWindows`) and each window's navigation events. Arke
//! isn't in that list, so such a folder stayed empty in Arke until it had been opened in File
//! Explorer once.
//!
//! So while a pane shows a folder inside a sync root (any provider; the explorer already knows
//! from its cloud scan, so nothing is read for other folders), this module registers the pane's
//! window in the shell's window list for that folder, like an Explorer tab:
//! - `IShellWindows::Register` + `OnNavigate` with the folder's item id list, which raises
//!   `DShellWindowsEvents::WindowRegistered` for whoever follows the list;
//! - an `IWebBrowser2` answering for the window (its `HWND`, `LocationURL`, `LocationName`, late
//!   bound calls through the `SHDocVw` type library), reaching an `IShellBrowser` and its view
//!   (`IShellView`, `IFolderView::GetFolder`, `IPersistIDList`) through `IServiceProvider`;
//! - the view as the window's `Document`, giving itself for `SID_SFolderView`: that is how
//!   `SHOpenFolderAndSelectItems` reaches a window it found in the list to select items, and it
//!   fails instead of opening File Explorer when the document is missing;
//! - `DWebBrowserEvents` and `DWebBrowserEvents2` connection points: every listener is told of
//!   the navigation as soon as it subscribes, once, with File Explorer's whole sequence
//!   ([`tell_navigation`]).
//!
//! One registration per pane, the way each File Explorer tab has its own registration under the
//! window's one `HWND`. It lasts exactly as long as the pane shows that folder: the pane going
//! elsewhere, closing (its window closing with it) or Arke quitting ([`end_all`]) revokes it.
//!
//! Nothing here waits on a clock. Each registration lives on its own thread with its own message
//! loop (the objects aren't agile, so every COM call arrives there as a message), which sleeps
//! until a message comes:
//! - a listener subscribed: tell it;
//! - something hints the folder's items changed ([`Trigger`], [`RELIST_ON`]): the explorer lists
//!   it again and reloads if items came or went, for placeholders that arrive without a watcher
//!   event. Hints that arrive together make one relist ([`Relist`]). The shell notifications are
//!   subscribed to before the window is registered, so a client that fills the folder and says
//!   so as soon as it hears of the visit can't be missed, and the folder is listed once more after
//!   the navigation was announced and after each later listener was told of it, for a client
//!   that fills it inside the navigation events;
//! - the folder's own entry in its parent changed ([`ParentWatch`]): a client that fills the
//!   folder seconds later (WPS Cloud while its sync process restarts) creates placeholders that
//!   raise no watcher event inside the folder and no shell notification, but the folder's entry
//!   (its last write time, its attributes) changes in the parent, which a watcher on the parent
//!   sees at the very moment the folder is filled;
//! - a "Show in folder" caller (`SHOpenFolderAndSelectItems`) that found this registration selects
//!   items: they are selected in the pane, as they would be in File Explorer.
//!
//! When a visit ends, one log line sums up what it saw ([`Journal`]): the shell notifications by
//! kind, the listeners' calls by method (every method of every object handed out), the events
//! fired at them, the hints and how many relists they made, and when the visit registered and
//! first heard from a listener.
//!
//! Nothing is downloaded: the folder is only named, never opened or read. Its parent is opened
//! only to be watched (like any folder Arke shows), never listed.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use arke_core::fs::{Change, ChangeEvent, Fs as _, RealFs};
use parking_lot::{Condvar, Mutex};
use windows::Win32::Foundation::{
    DISP_E_BADINDEX, DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_NOINTERFACE, E_NOTIMPL,
    E_POINTER, HANDLE, HWND, LPARAM, LRESULT, SHANDLE_PTR, VARIANT_BOOL, VARIANT_FALSE,
    VARIANT_TRUE, WPARAM,
};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, DISPATCH_FLAGS, DISPATCH_METHOD, DISPPARAMS,
    EXCEPINFO, IBindCtx, IConnectionPoint, IConnectionPoint_Impl, IConnectionPointContainer,
    IConnectionPointContainer_Impl, IDispatch, IDispatch_Impl, IEnumConnectionPoints,
    IEnumConnections, IPersist_Impl, IServiceProvider, IServiceProvider_Impl, IStream, ITypeInfo,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    CONNECT_E_CANNOTCONNECT, CONNECT_E_NOCONNECTION, DispGetIDsOfNames, DispInvoke,
    IOleWindow_Impl, LoadRegTypeLib, OLECMDEXECOPT, OLECMDF, OLECMDID, OLEMENUGROUPWIDTHS,
    READYSTATE, READYSTATE_COMPLETE,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{
    InitVariantFromBuffer, VARENUM, VARIANT, VT_BOOL, VT_BSTR, VT_BYREF, VT_DISPATCH, VT_I4,
    VT_VARIANT, VariantClear,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    _SVGIO, DWebBrowserEvents, DWebBrowserEvents2, FOLDERSETTINGS, IFolderView, IFolderView_Impl,
    ILCombine, ILGetSize, ILIsEqual, ILIsParent, IPersistIDList, IPersistIDList_Impl,
    IShellBrowser, IShellBrowser_Impl, IShellFolder, IShellItem, IShellView, IShellView_Impl,
    IShellWindows, IWebBrowser_Impl, IWebBrowser2, IWebBrowser2_Impl, IWebBrowserApp_Impl,
    SHBindToObject, SHCNE_CREATE, SHCNE_DELETE, SHCNE_ID, SHCNE_MKDIR, SHCNE_RENAMEFOLDER,
    SHCNE_RENAMEITEM, SHCNE_RMDIR, SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNRF_NewDelivery,
    SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock,
    SHChangeNotifyDeregister, SHChangeNotifyEntry, SHChangeNotifyRegister, SHCreateItemFromIDList,
    SHParseDisplayName, SIGDN_FILESYSPATH, SVSI_DESELECTOTHERS, SVSI_EDIT, SVSI_ENSUREVISIBLE,
    SVSI_FOCUSED, SVSI_SELECT, SWC_BROWSER, ShellWindows, UrlCreateFromPathW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HMENU,
    HWND_MESSAGE, IsWindowVisible, MSG, PM_NOREMOVE, PeekMessageW, PostMessageW,
    PostThreadMessageW, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_QUIT, WNDCLASSW,
};
use windows::core::{BSTR, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::shell::FolderVisit;
use crate::win_openinfolder::Pidl;

/// `SHDocVw`'s type library (`IWebBrowser2` and its events), for late-bound callers.
const LIBID_SHDOCVW: GUID = GUID::from_u128(0xeab2_2ac0_30c1_11cf_a7eb_0000_c05b_ae0b);
/// `DWebBrowserEvents::NavigateComplete(URL)`
const DISPID_NAVIGATECOMPLETE: i32 = 101;
/// `DownloadComplete()`, in both event interfaces
const DISPID_DOWNLOADCOMPLETE: i32 = 104;
/// `DWebBrowserEvents2::NavigateComplete2(pDisp, URL)`
const DISPID_NAVIGATECOMPLETE2: i32 = 252;
/// `DWebBrowserEvents2::DocumentComplete(pDisp, URL)`
const DISPID_DOCUMENTCOMPLETE: i32 = 259;
/// `LOCALE_USER_DEFAULT`
const LOCALE_USER_DEFAULT: u32 = 0x400;

/// To the visit's message window: a listener subscribed, tell it.
const WM_TELL: u32 = WM_APP + 1;
/// To the visit's message window: a shell change notification for the folder.
const WM_SHELL_CHANGE: u32 = WM_APP + 2;
/// To the visit's message window: `OnNavigate` returned (after the listeners already waiting were
/// told, since their `WM_TELL` is queued first).
const WM_NAVIGATED: u32 = WM_APP + 3;
/// To the visit's message window: hints are pending, list the folder again once for all of them.
const WM_RELIST: u32 = WM_APP + 4;
/// To the visit's message window: the folder's own entry in its parent changed ([`ParentWatch`]).
const WM_PARENT_ENTRY: u32 = WM_APP + 5;

/// What may hint that the folder's items changed without a watcher event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// `SHChangeNotify` for the folder or an item in it (items created, deleted, renamed, the
    /// whole folder updated, or `SHCNE_UPDATEITEM` for the folder itself), as a sync client that
    /// added placeholders may send so File Explorer's view refreshes. Shell-level only: plain file
    /// system changes are the watcher's.
    ShellNotify,
    /// A listener asked the window to refresh (`IWebBrowser2::Refresh`, `Refresh2`,
    /// `IShellView::Refresh`).
    ListenerRefresh,
    /// The navigation was announced (`OnNavigate` returned and the listeners already subscribed
    /// were told): a client may have filled the folder inside the navigation events, after Arke
    /// listed it. Once per visit.
    Navigated,
    /// A listener that subscribed later was told of the navigation and its calls returned: it may
    /// have filled the folder inside them (a client subscribes only once it hears the window was
    /// registered, after `OnNavigate`). Once per round of telling.
    Told,
    /// The folder's own entry in its parent changed (its last write time or attributes), or an
    /// item was created or renamed under its name, or the parent's watcher lost changes
    /// ([`ParentWatch`]). A client that fills the folder later than the navigation (WPS Cloud,
    /// seconds later while its sync process restarts) says nothing else: placeholders raise no
    /// watcher event inside the folder.
    ParentEntryChanged,
}

/// The hints the explorer acts on. Drop one here to stop acting on it.
const RELIST_ON: [Trigger; 5] = [
    Trigger::ShellNotify,
    Trigger::ListenerRefresh,
    Trigger::Navigated,
    Trigger::Told,
    Trigger::ParentEntryChanged,
];

/// Shell notifications that mean items came, went or were renamed. `SHCNE_UPDATEITEM` (an
/// item's state changed: sync clients send it all the time) isn't one of them; only the one for
/// the folder itself counts ([`relists_on_notification`]).
const NOTIFY_EVENTS: u32 = SHCNE_CREATE.0
    | SHCNE_MKDIR.0
    | SHCNE_DELETE.0
    | SHCNE_RMDIR.0
    | SHCNE_RENAMEITEM.0
    | SHCNE_RENAMEFOLDER.0
    | SHCNE_UPDATEDIR.0;

/// What the visit registers for: the above, and `SHCNE_UPDATEITEM` to see the folder's own.
const WATCHED_EVENTS: u32 = NOTIFY_EVENTS | SHCNE_UPDATEITEM.0;

/// Whether `trigger` makes the explorer list the folder again.
fn relists(trigger: Trigger) -> bool {
    RELIST_ON.contains(&trigger)
}

/// Whether the shell notification `event` (an `SHCNE_*` value) is one to act on.
const fn notifies_items(event: u32) -> bool {
    event & NOTIFY_EVENTS != 0
}

/// What an `SHCNE_UPDATEITEM` names, seen from the visited folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The folder itself: a sync client that filled it may say so this way (WPS Cloud, as far
    /// as known), and File Explorer relists on it.
    Folder,
    /// An item in it: sync progress, sent all the time.
    Inside,
    /// Anything else (or a name that isn't a file system path).
    Elsewhere,
}

/// Whether the shell notification `event`, about `target` when it is an `SHCNE_UPDATEITEM`,
/// makes the folder be listed again.
fn relists_on_notification(event: u32, target: Option<Target>) -> bool {
    notifies_items(event) || (event & SHCNE_UPDATEITEM.0 != 0 && target == Some(Target::Folder))
}

/// The file system path written the way it compares: no `\\?\`, no trailing separator,
/// lowercase. A drive root (`C:\`) becomes `c:`.
fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.trim_end_matches(['\\', '/']).to_lowercase()
}

/// Whether `item` (from a shell notification) is the visited `folder` itself, in it, or
/// elsewhere. Compares paths: the shell's item id lists for one folder may differ (see
/// `win_openinfolder::folder_locations`).
fn target_of(item: &Path, folder: &Path) -> Target {
    let folder = path_key(folder);
    if path_key(item) == folder {
        Target::Folder
    } else if item
        .parent()
        .is_some_and(|parent| path_key(parent) == folder)
    {
        Target::Inside
    } else {
        Target::Elsewhere
    }
}

/// The name a shell notification goes by in the visit's log.
fn event_name(event: u32, target: Option<Target>) -> &'static str {
    const NAMES: [(SHCNE_ID, &str); 7] = [
        (SHCNE_CREATE, "CREATE"),
        (SHCNE_MKDIR, "MKDIR"),
        (SHCNE_DELETE, "DELETE"),
        (SHCNE_RMDIR, "RMDIR"),
        (SHCNE_RENAMEITEM, "RENAMEITEM"),
        (SHCNE_RENAMEFOLDER, "RENAMEFOLDER"),
        (SHCNE_UPDATEDIR, "UPDATEDIR"),
    ];
    if event & SHCNE_UPDATEITEM.0 != 0 {
        return match target {
            Some(Target::Folder) => "UPDATEITEM(folder)",
            Some(Target::Inside) => "UPDATEITEM(item)",
            _ => "UPDATEITEM(elsewhere)",
        };
    }
    NAMES
        .iter()
        .find(|(id, _)| event & id.0 != 0)
        .map_or("other", |&(_, name)| name)
}

/// The folder's name the way the parent's watcher records are compared with it ([`names_folder`]):
/// lowercase, like [`path_key`].
fn name_key(name: &std::ffi::OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

/// Whether `change`, from the watcher on the folder's parent, is about the folder's own entry,
/// `folder` being its name ([`name_key`]): the entry changed (`FILE_ACTION_MODIFIED`), or an item
/// was created (`ADDED`) or renamed (`RENAMED_NEW_NAME`) under its name, in any case. A sibling's
/// change isn't, nor the folder going away (`REMOVED`, `RENAMED_OLD_NAME`): the pane sees that.
/// The watch is on one level, so the name is all a record has to match.
fn names_folder(change: &Change, folder: &str) -> bool {
    match change {
        Change::Removed(_) => false,
        Change::Unknown(path) | Change::Present(path, _) => path
            .file_name()
            .is_some_and(|name| name_key(name) == folder),
    }
}

/// Whether what one read of the parent's watcher brought hints the folder was filled: a change
/// of its entry ([`names_folder`]), or lost changes (the buffer overflowed, the drive's watchers
/// were reopened), which may have had one.
fn about_folder(event: &ChangeEvent, folder: &str) -> bool {
    match event {
        ChangeEvent::Rescan => true,
        ChangeEvent::Changed(changes) => changes.iter().any(|c| names_folder(c, folder)),
    }
}

/// Watches the visited folder's parent, one level, for changes of the folder's own entry, and
/// posts [`WM_PARENT_ENTRY`] to the visit's message window for each read that has one (hints that
/// arrive together make one relist, [`Relist`]). Placeholders a sync client creates in the folder
/// later than the navigation raise no event in the folder's own watcher and no shell notification,
/// but the folder's entry in its parent changes at that moment (#319).
///
/// Lives exactly as long as the visit's registration: started before the window is registered,
/// dropped once the loop ended, before the message window is destroyed. It goes through the
/// explorer's own watcher ([`arke_core::fs::Fs::watch_changes`] of [`RealFs`]): no thread of its
/// own and no clock, a handle that shares read, write and delete (the parent can still be renamed
/// or deleted), closed with the others when the drive is ejected.
struct ParentWatch {
    /// The visit's message window, as the watcher's thread posts to it; `None` once the visit
    /// ended. Taken for each post, so no post reaches the window after [`Drop`] cleared it.
    window: Arc<Mutex<Option<isize>>>,
    /// Dropping it stops the watcher; its handle is closed right after, on the watcher's thread.
    _watch: Box<dyn Send>,
}

impl ParentWatch {
    /// Starts watching the parent of `dir` for it, posting to `messages`. `None` for a drive root
    /// (no parent) or a parent that can't be watched: the other hints still work.
    fn start(messages: HWND, dir: &Path) -> Option<Self> {
        let parent = dir.parent()?;
        let folder = name_key(dir.file_name()?);
        let window = Arc::new(Mutex::new(Some(messages.0 as isize)));
        let posting = Arc::clone(&window);
        let watched = RealFs.watch_changes(
            parent,
            Box::new(move |event| {
                if about_folder(&event, &folder) {
                    post_parent_entry(&posting);
                }
            }),
        );
        match watched {
            Ok(watch) => Some(Self {
                window,
                _watch: watch,
            }),
            Err(err) => {
                log::debug!("folder visit: cannot watch {}: {err}", parent.display());
                None
            }
        }
    }
}

/// Posts [`WM_PARENT_ENTRY`] to the visit's message window, unless the visit ended. On the
/// watcher's thread, which mustn't wait: PostMessageW doesn't.
#[allow(
    clippy::significant_drop_tightening,
    reason = "held while posting: the visit clears the window under it before destroying it"
)]
fn post_parent_entry(window: &Mutex<Option<isize>>) {
    let window = window.lock();
    if let Some(hwnd) = *window {
        // SAFETY: posts to the visit's own message window, which exists while the value is set.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut c_void)),
                WM_PARENT_ENTRY,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
    drop(window);
}

impl Drop for ParentWatch {
    fn drop(&mut self) {
        // Before the watcher stops (the fields drop after this): a read it is handling now posts
        // nothing more.
        *self.window.lock() = None;
    }
}

/// Relisting after hints: one wake-up for any number of hints that arrive before the loop gets to
/// it (a burst of notifications is already queued), and the one relist after the navigation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Relist {
    /// A wake-up is queued.
    pending: bool,
    /// The relist after the navigation was asked for already.
    navigated: bool,
}

impl Relist {
    /// A hint came. Returns whether to queue a wake-up (none is queued yet).
    const fn hint(&mut self) -> bool {
        let wake = !self.pending;
        self.pending = true;
        wake
    }

    /// The navigation was announced. Returns whether this is the first time in this visit.
    const fn navigated(&mut self) -> bool {
        let first = !self.navigated;
        self.navigated = true;
        first
    }

    /// The wake-up arrived. Returns whether to list the folder again now.
    const fn flush(&mut self) -> bool {
        let due = self.pending;
        self.pending = false;
        due
    }
}

/// What a visit saw, for one compact log block when it ends: shell notifications by kind (count
/// and the first one's paths), listener calls by method, the interfaces and services asked of
/// the window's objects (answered or refused), hints and relists.
#[derive(Debug, Default)]
struct Journal {
    notifications: Vec<(&'static str, u32, String)>,
    calls: Vec<(String, u32)>,
    hints: Vec<(Trigger, u32)>,
    relists: u32,
    /// Events fired at listeners: the DISPID, whether the call succeeded.
    fired: Vec<(i32, bool, u32)>,
    /// When the visit started, for [`Self::mark`].
    started: Option<Instant>,
    /// The first time each step happened, after `started`.
    marks: Vec<(&'static str, Duration)>,
}

impl Journal {
    /// The visit starts now.
    fn start(&mut self) {
        self.started = Some(Instant::now());
    }

    /// `step` happened now: kept the first time only (`registered`, `first listener`, ...).
    fn mark(&mut self, step: &'static str) {
        if let Some(started) = self.started
            && !self.marks.iter().any(|(s, _)| *s == step)
        {
            self.marks.push((step, started.elapsed()));
        }
    }

    /// Whether no notification of `kind` was noted yet (its paths are worth reading).
    fn first_of(&self, kind: &str) -> bool {
        !self.notifications.iter().any(|(k, ..)| *k == kind)
    }

    fn note_notification(&mut self, kind: &'static str, paths: impl FnOnce() -> String) {
        if let Some(entry) = self.notifications.iter_mut().find(|(k, ..)| *k == kind) {
            entry.1 += 1;
        } else {
            self.notifications.push((kind, 1, paths()));
        }
    }

    fn note_call(&mut self, method: &str) {
        if let Some(entry) = self.calls.iter_mut().find(|(m, _)| m == method) {
            entry.1 += 1;
        } else {
            self.calls.push((method.to_owned(), 1));
        }
    }

    fn note_fired(&mut self, id: i32, ok: bool) {
        if let Some(entry) = self.fired.iter_mut().find(|f| f.0 == id && f.1 == ok) {
            entry.2 += 1;
        } else {
            self.fired.push((id, ok, 1));
        }
    }

    fn note_hint(&mut self, trigger: Trigger) {
        if let Some(entry) = self.hints.iter_mut().find(|(t, _)| *t == trigger) {
            entry.1 += 1;
        } else {
            self.hints.push((trigger, 1));
        }
    }

    /// One line: `notifications: ...; listener calls: ...; hints: ... -> relists N`.
    fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::from("notifications:");
        if self.notifications.is_empty() {
            out.push_str(" none");
        }
        for (i, (kind, count, paths)) in self.notifications.iter().enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            let _ = write!(out, "{sep}{kind} x{count} [{paths}]");
        }
        out.push_str("; listener calls:");
        if self.calls.is_empty() {
            out.push_str(" none");
        }
        for (i, (method, count)) in self.calls.iter().enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            let _ = write!(out, "{sep}{method} x{count}");
        }
        out.push_str("; hints:");
        if self.hints.is_empty() {
            out.push_str(" none");
        }
        for (i, (trigger, count)) in self.hints.iter().enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            let _ = write!(out, "{sep}{trigger:?} x{count}");
        }
        let _ = write!(out, " -> relists {}", self.relists);
        for (i, (id, ok, count)) in self.fired.iter().enumerate() {
            let sep = if i == 0 { "; fired:" } else { "," };
            let answer = if *ok { "ok" } else { "failed" };
            let _ = write!(out, "{sep} {id} {answer} x{count}");
        }
        for (i, (step, at)) in self.marks.iter().enumerate() {
            let sep = if i == 0 { "; timing: " } else { ", " };
            let _ = write!(out, "{sep}{step} +{} ms", at.as_millis());
        }
        out
    }
}

/// `SelectItem` flags that select (`SVSI_EDIT` selects and starts a rename).
const SELECTS: u32 = (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0 | SVSI_EDIT.0) as u32;

/// Adds `name` to what a "Show in folder" caller selected so far (`SVSI_DESELECTOTHERS` starts
/// over). Returns whether the selection changed and should be shown.
fn note_selected(selected: &mut Vec<Arc<str>>, name: Option<Arc<str>>, flags: u32) -> bool {
    let Some(name) = name.filter(|_| flags & SELECTS != 0) else {
        return false;
    };
    if flags & SVSI_DESELECTOTHERS.0 as u32 != 0 {
        selected.clear();
    }
    if !selected.contains(&name) {
        selected.push(name);
    }
    true
}

/// A running visit, as [`end`] and [`end_all`] see it.
#[derive(Default)]
struct Running {
    /// The visit's thread, once it has a message queue (0 before).
    thread: AtomicU32,
    /// Asked to end (possibly before the thread got going).
    quit: AtomicBool,
    /// Inside a call to a listener (another process, which may hang): [`end_all`] doesn't wait
    /// for it.
    calling: AtomicBool,
    /// The registration was revoked and the thread is done.
    ended: Mutex<bool>,
    changed: Condvar,
}

impl Running {
    fn set_calling(&self, calling: bool) {
        self.calling.store(calling, Ordering::SeqCst);
        // Taken once so a waiter between its check and its wait doesn't miss the change.
        drop(self.ended.lock());
        self.changed.notify_all();
    }

    fn finish(&self) {
        *self.ended.lock() = true;
        self.changed.notify_all();
    }

    /// Waits until the visit ended, unless it is (or gets) stuck calling a listener.
    fn wait(&self) {
        let mut ended = self.ended.lock();
        while !*ended && !self.calling.load(Ordering::SeqCst) {
            self.changed.wait(&mut ended);
        }
    }
}

/// Running visits by pane (`slot`).
static VISITS: LazyLock<Mutex<HashMap<u64, Arc<Running>>>> = LazyLock::new(Mutex::default);

/// See [`crate::shell::Shell::folder_visit`]. Returns at once: the work runs on its own thread.
pub(crate) fn visit(slot: u64, visit: Option<FolderVisit>) {
    end(slot);
    let Some(visit) = visit else {
        return;
    };
    let Some(window) = visit.window.filter(|&w| w != 0) else {
        return;
    };
    // In the list before the thread starts, so leaving right away ends it too.
    let running = Arc::new(Running::default());
    VISITS.lock().insert(slot, Arc::clone(&running));
    let thread_running = Arc::clone(&running);
    let spawned = std::thread::Builder::new()
        .name("folder-visit".into())
        .spawn(move || run(window, visit, &thread_running));
    if let Err(err) = spawned {
        log::error!("cannot spawn thread: {err}");
        VISITS.lock().retain(|_, r| !Arc::ptr_eq(r, &running));
    }
}

/// Ends the pane's visit, if one is running.
fn end(slot: u64) {
    let running = VISITS.lock().remove(&slot);
    if let Some(running) = running {
        quit(&running);
    }
}

fn quit(running: &Running) {
    running.quit.store(true, Ordering::SeqCst);
    let thread = running.thread.load(Ordering::SeqCst);
    if thread != 0 {
        // SAFETY: posts WM_QUIT to a thread of ours, which revokes its registration and returns.
        let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

/// Arke is quitting: revokes every registration, so the shell's window list doesn't keep windows
/// that are gone. Waits for each visit to finish, except one stuck in a listener's call.
pub(crate) fn end_all() {
    let running: Vec<Arc<Running>> = VISITS.lock().drain().map(|(_, r)| r).collect();
    for r in &running {
        quit(r);
    }
    for r in &running {
        r.wait();
    }
}

/// Which of the two browser event interfaces a listener subscribed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Events {
    /// `DWebBrowserEvents` (what WPS Cloud listens to)
    Old,
    /// `DWebBrowserEvents2`
    New,
}

impl Events {
    const fn iid(self) -> GUID {
        match self {
            Self::Old => DWebBrowserEvents::IID,
            Self::New => DWebBrowserEvents2::IID,
        }
    }

    fn of(iid: &GUID) -> Option<Self> {
        [Self::Old, Self::New].into_iter().find(|e| e.iid() == *iid)
    }

    /// `Advise` for these events, as the visit's log names it.
    const fn advise_name(self) -> &'static str {
        match self {
            Self::Old => "Advise(DWebBrowserEvents)",
            Self::New => "Advise(DWebBrowserEvents2)",
        }
    }
}

/// A subscribed listener.
struct Sink {
    cookie: u32,
    events: Events,
    dispatch: IDispatch,
    /// Told about the navigation already.
    told: bool,
}

/// What the browser, its view and its connection points share. Only ever used on the visit's
/// thread (the objects aren't agile).
struct State {
    /// The pane's window, as registered
    hwnd: HWND,
    /// The visit's own message-only window
    messages: HWND,
    /// The folder, as its path and its item id list.
    dir: PathBuf,
    folder: Pidl,
    /// `file:///...`
    url: String,
    /// The folder's name, as `LocationName` gives it.
    name: String,
    type_info: Option<ITypeInfo>,
    sinks: RefCell<Vec<Sink>>,
    next_cookie: Cell<u32>,
    /// What a "Show in folder" caller selected so far.
    selected: RefCell<Vec<Arc<str>>>,
    changed: Box<dyn Fn()>,
    select: Box<dyn Fn(Vec<Arc<str>>)>,
    running: Arc<Running>,
    relist: Cell<Relist>,
    journal: RefCell<Journal>,
}

impl State {
    /// Something hints the folder's items changed: one wake-up lists it again for all the hints
    /// that come before it.
    fn hint(&self, trigger: Trigger) {
        if !relists(trigger) {
            return;
        }
        self.journal.borrow_mut().note_hint(trigger);
        let mut relist = self.relist.get();
        let wake = relist.hint();
        self.relist.set(relist);
        if wake && !self.messages.0.is_null() {
            // SAFETY: posts to this visit's own window.
            let _ = unsafe { PostMessageW(Some(self.messages), WM_RELIST, WPARAM(0), LPARAM(0)) };
        }
    }

    /// The navigation was announced: list the folder once more, the first time.
    fn navigated(&self) {
        let mut relist = self.relist.get();
        let first = relist.navigated();
        self.relist.set(relist);
        if first {
            self.hint(Trigger::Navigated);
        }
    }

    /// The wake-up for pending hints arrived.
    fn flush_relist(&self) {
        let mut relist = self.relist.get();
        let due = relist.flush();
        self.relist.set(relist);
        if due {
            self.journal.borrow_mut().relists += 1;
            (self.changed)();
        }
    }

    /// A listener called `method`, for the log. Skipped if the journal is busy (a call coming in
    /// while it is being written to cannot be, but the log must never panic).
    fn note_call(&self, method: &str) {
        if let Ok(mut journal) = self.journal.try_borrow_mut() {
            journal.note_call(method);
        }
    }

    /// A shell notification for the folder or an item in it: `event` about `first` (and `second`,
    /// for a rename). Returns whether it hints the folder's items changed.
    fn notification(
        &self,
        event: u32,
        first: *const ITEMIDLIST,
        second: *const ITEMIDLIST,
    ) -> bool {
        let target = (event & SHCNE_UPDATEITEM.0 != 0).then(|| self.target(first));
        let kind = event_name(event, target);
        self.journal.borrow_mut().mark("first notification");
        // The paths are read outside the borrow, and only for the first of each kind.
        let first_of_kind = self.journal.borrow().first_of(kind);
        let paths = if first_of_kind {
            [first, second]
                .into_iter()
                .filter_map(pidl_path)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ")
        } else {
            String::new()
        };
        self.journal.borrow_mut().note_notification(kind, || paths);
        relists_on_notification(event, target)
    }

    /// What the notification item `item` is, seen from the folder. The item id lists are
    /// compared first (no path needed for the folder's own list or its children's).
    fn target(&self, item: *const ITEMIDLIST) -> Target {
        if item.is_null() {
            return Target::Elsewhere;
        }
        let folder = self.folder.0.cast_const();
        // SAFETY: both lists are valid for the calls.
        let (same, child) = unsafe {
            (
                ILIsEqual(folder, item).as_bool(),
                ILIsParent(folder, item, true).as_bool(),
            )
        };
        if same {
            return Target::Folder;
        }
        if child {
            return Target::Inside;
        }
        pidl_path(item).map_or(Target::Elsewhere, |path| target_of(&path, &self.dir))
    }

    /// Wakes the loop to tell a new listener (not from inside its `Advise` call, which is waiting).
    fn listener_added(&self) {
        // SAFETY: posts to this visit's own window.
        let _ = unsafe { PostMessageW(Some(self.messages), WM_TELL, WPARAM(0), LPARAM(0)) };
    }

    /// A "Show in folder" caller selected the child item `item` of the folder.
    fn select_item(&self, item: *const ITEMIDLIST, flags: u32) {
        if item.is_null() || flags & SELECTS == 0 {
            return;
        }
        let name = child_name(self.folder.0.cast_const(), item);
        let mut selected = self.selected.borrow_mut();
        if note_selected(&mut selected, name, flags) {
            let names = selected.clone();
            drop(selected);
            (self.select)(names);
        }
    }
}

/// The file name of the child item `item` of the folder `folder`.
fn child_name(folder: *const ITEMIDLIST, item: *const ITEMIDLIST) -> Option<Arc<str>> {
    // SAFETY: both lists are valid for the call; the combined list is freed here.
    let full = unsafe { ILCombine(Some(folder), Some(item)) };
    if full.is_null() {
        return None;
    }
    let path = pidl_path(full);
    // SAFETY: allocated by ILCombine above.
    unsafe { CoTaskMemFree(Some(full.cast_const().cast())) };
    path?
        .file_name()
        .map(|n| Arc::from(n.to_string_lossy().as_ref()))
}

/// The file system path of the absolute item id list `pidl`; `None` for null or an item that
/// isn't in the file system.
fn pidl_path(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    if pidl.is_null() {
        return None;
    }
    // SAFETY: `pidl` is a valid absolute list for the call; the name is freed here. Naming an item
    // opens nothing.
    unsafe {
        let item: IShellItem = SHCreateItemFromIDList(pidl).ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        path
    }
}

#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::wildcard_imports,
    clippy::cast_possible_wrap,
    clippy::unused_self,
    clippy::unnecessary_wraps,
    reason = "The macro-generated code and the shell's COM signatures trip several pedantic lints"
)]
mod com {
    use super::*;
    use windows::core::IUnknownImpl as _;

    type Res<T> = windows::core::Result<T>;

    /// The window as the shell's window list sees it.
    #[implement(
        IWebBrowser2,
        IServiceProvider,
        IShellBrowser,
        IConnectionPointContainer,
        Agile = false
    )]
    pub(super) struct Browser {
        pub(super) state: Rc<State>,
        pub(super) view: IShellView,
    }

    /// The window's folder view.
    #[implement(
        IShellView,
        IFolderView,
        IPersistIDList,
        IDispatch,
        IServiceProvider,
        Agile = false
    )]
    pub(super) struct View {
        pub(super) state: Rc<State>,
    }

    /// One of the browser's event sources.
    #[implement(IConnectionPoint, Agile = false)]
    pub(super) struct EventPoint {
        events: Events,
        container: IConnectionPointContainer,
        state: Rc<State>,
    }

    impl IDispatch_Impl for Browser_Impl {
        fn GetTypeInfoCount(&self) -> Res<u32> {
            self.state.note_call("Browser.GetTypeInfoCount");
            Ok(u32::from(self.state.type_info.is_some()))
        }
        fn GetTypeInfo(&self, index: u32, _: u32) -> Res<ITypeInfo> {
            self.state.note_call("Browser.GetTypeInfo");
            match (&self.state.type_info, index) {
                (Some(info), 0) => Ok(info.clone()),
                _ => Err(DISP_E_BADINDEX.into()),
            }
        }
        fn GetIDsOfNames(
            &self,
            _: *const GUID,
            names: *const PCWSTR,
            count: u32,
            _: u32,
            ids: *mut i32,
        ) -> Res<()> {
            self.state.note_call("Browser.GetIDsOfNames");
            let Some(info) = &self.state.type_info else {
                return Err(DISP_E_UNKNOWNNAME.into());
            };
            // SAFETY: the caller passes `count` names and room for `count` ids.
            unsafe { DispGetIDsOfNames(info, names, count, ids) }
        }
        fn Invoke(
            &self,
            id: i32,
            _: *const GUID,
            _: u32,
            flags: DISPATCH_FLAGS,
            params: *const DISPPARAMS,
            result: *mut VARIANT,
            exception: *mut EXCEPINFO,
            arg_error: *mut u32,
        ) -> Res<()> {
            self.state.note_call("Browser.Invoke");
            let Some(info) = &self.state.type_info else {
                return Err(DISP_E_MEMBERNOTFOUND.into());
            };
            let this: IWebBrowser2 = self.to_interface();
            // SAFETY: `this` is this object's IWebBrowser2, the dual interface `info` describes;
            // the other pointers come from the caller as they are.
            unsafe {
                DispInvoke(
                    this.as_raw(),
                    info,
                    id,
                    flags.0,
                    params.cast_mut(),
                    result,
                    exception,
                    arg_error,
                )
            }
        }
    }

    impl IWebBrowser_Impl for Browser_Impl {
        fn GoBack(&self) -> Res<()> {
            self.state.note_call("Browser.GoBack");
            Err(E_NOTIMPL.into())
        }
        fn GoForward(&self) -> Res<()> {
            self.state.note_call("Browser.GoForward");
            Err(E_NOTIMPL.into())
        }
        fn GoHome(&self) -> Res<()> {
            self.state.note_call("Browser.GoHome");
            Err(E_NOTIMPL.into())
        }
        fn GoSearch(&self) -> Res<()> {
            self.state.note_call("Browser.GoSearch");
            Err(E_NOTIMPL.into())
        }
        fn Navigate(
            &self,
            _: &BSTR,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
        ) -> Res<()> {
            self.state.note_call("Browser.Navigate");
            Err(E_NOTIMPL.into())
        }
        fn Refresh(&self) -> Res<()> {
            self.state.note_call("Browser.Refresh");
            self.state.hint(Trigger::ListenerRefresh);
            Ok(())
        }
        fn Refresh2(&self, _: *const VARIANT) -> Res<()> {
            self.state.note_call("Browser.Refresh2");
            self.state.hint(Trigger::ListenerRefresh);
            Ok(())
        }
        fn Stop(&self) -> Res<()> {
            self.state.note_call("Browser.Stop");
            Ok(())
        }
        fn Application(&self) -> Res<IDispatch> {
            self.state.note_call("Browser.Application");
            self.to_interface::<IWebBrowser2>().cast()
        }
        fn Parent(&self) -> Res<IDispatch> {
            self.state.note_call("Browser.Parent");
            self.Application()
        }
        fn Container(&self) -> Res<IDispatch> {
            self.state.note_call("Browser.Container");
            self.Application()
        }
        fn Document(&self) -> Res<IDispatch> {
            self.state.note_call("Browser.Document");
            self.view.cast()
        }
        fn TopLevelContainer(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.TopLevelContainer");
            Ok(VARIANT_TRUE)
        }
        fn Type(&self) -> Res<BSTR> {
            self.state.note_call("Browser.Type");
            Ok(BSTR::from("Arke"))
        }
        fn Left(&self) -> Res<i32> {
            self.state.note_call("Browser.Left");
            Ok(0)
        }
        fn SetLeft(&self, _: i32) -> Res<()> {
            self.state.note_call("Browser.SetLeft");
            Err(E_NOTIMPL.into())
        }
        fn Top(&self) -> Res<i32> {
            self.state.note_call("Browser.Top");
            Ok(0)
        }
        fn SetTop(&self, _: i32) -> Res<()> {
            self.state.note_call("Browser.SetTop");
            Err(E_NOTIMPL.into())
        }
        fn Width(&self) -> Res<i32> {
            self.state.note_call("Browser.Width");
            Ok(0)
        }
        fn SetWidth(&self, _: i32) -> Res<()> {
            self.state.note_call("Browser.SetWidth");
            Err(E_NOTIMPL.into())
        }
        fn Height(&self) -> Res<i32> {
            self.state.note_call("Browser.Height");
            Ok(0)
        }
        fn SetHeight(&self, _: i32) -> Res<()> {
            self.state.note_call("Browser.SetHeight");
            Err(E_NOTIMPL.into())
        }
        fn LocationName(&self) -> Res<BSTR> {
            self.state.note_call("Browser.LocationName");
            Ok(BSTR::from(self.state.name.as_str()))
        }
        fn LocationURL(&self) -> Res<BSTR> {
            self.state.note_call("Browser.LocationURL");
            Ok(BSTR::from(self.state.url.as_str()))
        }
        fn Busy(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.Busy");
            Ok(VARIANT_FALSE)
        }
    }

    impl IWebBrowserApp_Impl for Browser_Impl {
        fn Quit(&self) -> Res<()> {
            self.state.note_call("Browser.Quit");
            // Never closes Arke's window.
            Ok(())
        }
        fn ClientToWindow(&self, _: *mut i32, _: *mut i32) -> Res<()> {
            self.state.note_call("Browser.ClientToWindow");
            Ok(())
        }
        fn PutProperty(&self, _: &BSTR, _: &VARIANT) -> Res<()> {
            self.state.note_call("Browser.PutProperty");
            Err(E_NOTIMPL.into())
        }
        fn GetProperty(&self, _: &BSTR) -> Res<VARIANT> {
            self.state.note_call("Browser.GetProperty");
            Err(E_NOTIMPL.into())
        }
        fn Name(&self) -> Res<BSTR> {
            self.state.note_call("Browser.Name");
            Ok(BSTR::from("Arke"))
        }
        fn HWND(&self) -> Res<SHANDLE_PTR> {
            self.state.note_call("Browser.HWND");
            Ok(SHANDLE_PTR(self.state.hwnd.0 as isize))
        }
        fn FullName(&self) -> Res<BSTR> {
            self.state.note_call("Browser.FullName");
            let exe = std::env::current_exe().unwrap_or_default();
            Ok(BSTR::from(exe.to_string_lossy().as_ref()))
        }
        fn Path(&self) -> Res<BSTR> {
            self.state.note_call("Browser.Path");
            let exe = std::env::current_exe().unwrap_or_default();
            let dir = exe.parent().unwrap_or(&exe);
            Ok(BSTR::from(dir.to_string_lossy().as_ref()))
        }
        fn Visible(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.Visible");
            // SAFETY: reads a window's style; a closed window reads as invisible.
            let visible = unsafe { IsWindowVisible(self.state.hwnd) }.as_bool();
            Ok(if visible { VARIANT_TRUE } else { VARIANT_FALSE })
        }
        fn SetVisible(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetVisible");
            Err(E_NOTIMPL.into())
        }
        fn StatusBar(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.StatusBar");
            Ok(VARIANT_FALSE)
        }
        fn SetStatusBar(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetStatusBar");
            Err(E_NOTIMPL.into())
        }
        fn StatusText(&self) -> Res<BSTR> {
            self.state.note_call("Browser.StatusText");
            Ok(BSTR::new())
        }
        fn SetStatusText(&self, _: &BSTR) -> Res<()> {
            self.state.note_call("Browser.SetStatusText");
            Err(E_NOTIMPL.into())
        }
        fn ToolBar(&self) -> Res<i32> {
            self.state.note_call("Browser.ToolBar");
            Ok(0)
        }
        fn SetToolBar(&self, _: i32) -> Res<()> {
            self.state.note_call("Browser.SetToolBar");
            Err(E_NOTIMPL.into())
        }
        fn MenuBar(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.MenuBar");
            Ok(VARIANT_FALSE)
        }
        fn SetMenuBar(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetMenuBar");
            Err(E_NOTIMPL.into())
        }
        fn FullScreen(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.FullScreen");
            Ok(VARIANT_FALSE)
        }
        fn SetFullScreen(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetFullScreen");
            Err(E_NOTIMPL.into())
        }
    }

    impl IWebBrowser2_Impl for Browser_Impl {
        fn Navigate2(
            &self,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
        ) -> Res<()> {
            self.state.note_call("Browser.Navigate2");
            Err(E_NOTIMPL.into())
        }
        fn QueryStatusWB(&self, _: OLECMDID) -> Res<OLECMDF> {
            self.state.note_call("Browser.QueryStatusWB");
            Err(E_NOTIMPL.into())
        }
        fn ExecWB(
            &self,
            _: OLECMDID,
            _: OLECMDEXECOPT,
            _: *const VARIANT,
            _: *mut VARIANT,
        ) -> Res<()> {
            self.state.note_call("Browser.ExecWB");
            Err(E_NOTIMPL.into())
        }
        fn ShowBrowserBar(
            &self,
            _: *const VARIANT,
            _: *const VARIANT,
            _: *const VARIANT,
        ) -> Res<()> {
            self.state.note_call("Browser.ShowBrowserBar");
            Err(E_NOTIMPL.into())
        }
        fn ReadyState(&self) -> Res<READYSTATE> {
            self.state.note_call("Browser.ReadyState");
            Ok(READYSTATE_COMPLETE)
        }
        fn Offline(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.Offline");
            Ok(VARIANT_FALSE)
        }
        fn SetOffline(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetOffline");
            Err(E_NOTIMPL.into())
        }
        fn Silent(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.Silent");
            Ok(VARIANT_TRUE)
        }
        fn SetSilent(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetSilent");
            Err(E_NOTIMPL.into())
        }
        fn RegisterAsBrowser(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.RegisterAsBrowser");
            Ok(VARIANT_TRUE)
        }
        fn SetRegisterAsBrowser(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetRegisterAsBrowser");
            Err(E_NOTIMPL.into())
        }
        fn RegisterAsDropTarget(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.RegisterAsDropTarget");
            Ok(VARIANT_FALSE)
        }
        fn SetRegisterAsDropTarget(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetRegisterAsDropTarget");
            Err(E_NOTIMPL.into())
        }
        fn TheaterMode(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.TheaterMode");
            Ok(VARIANT_FALSE)
        }
        fn SetTheaterMode(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetTheaterMode");
            Err(E_NOTIMPL.into())
        }
        fn AddressBar(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.AddressBar");
            Ok(VARIANT_TRUE)
        }
        fn SetAddressBar(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetAddressBar");
            Err(E_NOTIMPL.into())
        }
        fn Resizable(&self) -> Res<VARIANT_BOOL> {
            self.state.note_call("Browser.Resizable");
            Ok(VARIANT_TRUE)
        }
        fn SetResizable(&self, _: VARIANT_BOOL) -> Res<()> {
            self.state.note_call("Browser.SetResizable");
            Err(E_NOTIMPL.into())
        }
    }

    impl IServiceProvider_Impl for Browser_Impl {
        fn QueryService(
            &self,
            _: *const GUID,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // Every service (SID_STopLevelBrowser, SID_SShellBrowser, ...) is this browser.
            let unknown: IUnknown = self.to_interface();
            self.state.note_call("Browser.QueryService");
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IOleWindow_Impl for Browser_Impl {
        fn GetWindow(&self) -> Res<HWND> {
            self.state.note_call("Browser.GetWindow");
            Ok(self.state.hwnd)
        }
        fn ContextSensitiveHelp(&self, _: windows::core::BOOL) -> Res<()> {
            self.state.note_call("Browser.ContextSensitiveHelp");
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellBrowser_Impl for Browser_Impl {
        fn InsertMenusSB(&self, _: HMENU, _: *mut OLEMENUGROUPWIDTHS) -> Res<()> {
            self.state.note_call("Browser.InsertMenusSB");
            Err(E_NOTIMPL.into())
        }
        fn SetMenuSB(&self, _: HMENU, _: isize, _: HWND) -> Res<()> {
            self.state.note_call("Browser.SetMenuSB");
            Err(E_NOTIMPL.into())
        }
        fn RemoveMenusSB(&self, _: HMENU) -> Res<()> {
            self.state.note_call("Browser.RemoveMenusSB");
            Err(E_NOTIMPL.into())
        }
        fn SetStatusTextSB(&self, _: &PCWSTR) -> Res<()> {
            self.state.note_call("Browser.SetStatusTextSB");
            Ok(())
        }
        fn EnableModelessSB(&self, _: windows::core::BOOL) -> Res<()> {
            self.state.note_call("Browser.EnableModelessSB");
            Ok(())
        }
        fn TranslateAcceleratorSB(
            &self,
            _: *const windows::Win32::UI::WindowsAndMessaging::MSG,
            _: u16,
        ) -> Res<()> {
            self.state.note_call("Browser.TranslateAcceleratorSB");
            Err(E_NOTIMPL.into())
        }
        fn BrowseObject(&self, _: *const ITEMIDLIST, _: u32) -> Res<()> {
            self.state.note_call("Browser.BrowseObject");
            Err(E_NOTIMPL.into())
        }
        fn GetViewStateStream(&self, _: u32) -> Res<IStream> {
            self.state.note_call("Browser.GetViewStateStream");
            Err(E_NOTIMPL.into())
        }
        fn GetControlWindow(&self, _: u32) -> Res<HWND> {
            self.state.note_call("Browser.GetControlWindow");
            Err(E_NOTIMPL.into())
        }
        fn SendControlMsg(&self, _: u32, _: u32, _: WPARAM, _: LPARAM, _: *mut LRESULT) -> Res<()> {
            self.state.note_call("Browser.SendControlMsg");
            Err(E_NOTIMPL.into())
        }
        fn QueryActiveShellView(&self) -> Res<IShellView> {
            self.state.note_call("Browser.QueryActiveShellView");
            Ok(self.view.clone())
        }
        fn OnViewWindowActive(&self, _: Ref<IShellView>) -> Res<()> {
            self.state.note_call("Browser.OnViewWindowActive");
            Ok(())
        }
        fn SetToolbarItems(
            &self,
            _: *const windows::Win32::UI::Controls::TBBUTTON,
            _: u32,
            _: u32,
        ) -> Res<()> {
            self.state.note_call("Browser.SetToolbarItems");
            Err(E_NOTIMPL.into())
        }
    }

    impl IConnectionPointContainer_Impl for Browser_Impl {
        fn EnumConnectionPoints(&self) -> Res<IEnumConnectionPoints> {
            self.state.note_call("Browser.EnumConnectionPoints");
            Err(E_NOTIMPL.into())
        }
        fn FindConnectionPoint(&self, riid: *const GUID) -> Res<IConnectionPoint> {
            self.state.note_call("Browser.FindConnectionPoint");
            // SAFETY: riid is null or a GUID supplied by the caller.
            let iid =
                unsafe { riid.as_ref() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
            let events = Events::of(iid)
                .ok_or_else(|| windows::core::Error::from(CONNECT_E_NOCONNECTION))?;
            Ok(EventPoint {
                events,
                container: self.to_interface(),
                state: Rc::clone(&self.state),
            }
            .into())
        }
    }

    impl IConnectionPoint_Impl for EventPoint_Impl {
        fn GetConnectionInterface(&self) -> Res<GUID> {
            self.state.note_call("Events.GetConnectionInterface");
            Ok(self.events.iid())
        }
        fn GetConnectionPointContainer(&self) -> Res<IConnectionPointContainer> {
            self.state.note_call("Events.GetConnectionPointContainer");
            Ok(self.container.clone())
        }
        fn Advise(&self, sink: Ref<IUnknown>) -> Res<u32> {
            let unknown = sink.ok()?;
            let iid = self.events.iid();
            let mut raw = std::ptr::null_mut();
            // SAFETY: valid interface; QueryInterface fills raw on success.
            let sink = if unsafe { unknown.query(&raw const iid, &raw mut raw) }.is_ok() {
                // SAFETY: an event dispinterface pointer is an IDispatch, owned now.
                unsafe { IDispatch::from_raw(raw) }
            } else {
                unknown
                    .cast::<IDispatch>()
                    .map_err(|_not_dispatch| windows::core::Error::from(CONNECT_E_CANNOTCONNECT))?
            };
            let cookie = self.state.next_cookie.get() + 1;
            self.state.next_cookie.set(cookie);
            self.state.sinks.borrow_mut().push(Sink {
                cookie,
                events: self.events,
                dispatch: sink,
                told: false,
            });
            self.state.note_call(self.events.advise_name());
            self.state.journal.borrow_mut().mark("first listener");
            self.state.listener_added();
            Ok(cookie)
        }
        fn Unadvise(&self, cookie: u32) -> Res<()> {
            self.state.note_call("Events.Unadvise");
            let mut sinks = self.state.sinks.borrow_mut();
            let before = sinks.len();
            sinks.retain(|s| s.cookie != cookie);
            let removed = sinks.len() != before;
            drop(sinks);
            if removed {
                Ok(())
            } else {
                Err(CONNECT_E_NOCONNECTION.into())
            }
        }
        fn EnumConnections(&self) -> Res<IEnumConnections> {
            self.state.note_call("Events.EnumConnections");
            Err(E_NOTIMPL.into())
        }
    }

    /// The view as a document: nothing to automate, only `IServiceProvider` behind it.
    impl IDispatch_Impl for View_Impl {
        fn GetTypeInfoCount(&self) -> Res<u32> {
            self.state.note_call("View.GetTypeInfoCount");
            Ok(0)
        }
        fn GetTypeInfo(&self, _: u32, _: u32) -> Res<ITypeInfo> {
            self.state.note_call("View.GetTypeInfo");
            Err(DISP_E_BADINDEX.into())
        }
        fn GetIDsOfNames(
            &self,
            _: *const GUID,
            _: *const PCWSTR,
            _: u32,
            _: u32,
            _: *mut i32,
        ) -> Res<()> {
            self.state.note_call("View.GetIDsOfNames");
            Err(DISP_E_UNKNOWNNAME.into())
        }
        fn Invoke(
            &self,
            _: i32,
            _: *const GUID,
            _: u32,
            _: DISPATCH_FLAGS,
            _: *const DISPPARAMS,
            _: *mut VARIANT,
            _: *mut EXCEPINFO,
            _: *mut u32,
        ) -> Res<()> {
            self.state.note_call("View.Invoke");
            Err(DISP_E_MEMBERNOTFOUND.into())
        }
    }

    impl IServiceProvider_Impl for View_Impl {
        fn QueryService(
            &self,
            _: *const GUID,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // Every service (SID_SFolderView, ...) is this view.
            let unknown: IUnknown = self.to_interface();
            self.state.note_call("View.QueryService");
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IOleWindow_Impl for View_Impl {
        fn GetWindow(&self) -> Res<HWND> {
            self.state.note_call("View.GetWindow");
            Ok(self.state.hwnd)
        }
        fn ContextSensitiveHelp(&self, _: windows::core::BOOL) -> Res<()> {
            self.state.note_call("View.ContextSensitiveHelp");
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellView_Impl for View_Impl {
        fn TranslateAccelerator(
            &self,
            _: *const windows::Win32::UI::WindowsAndMessaging::MSG,
        ) -> HRESULT {
            self.state.note_call("View.TranslateAccelerator");
            E_NOTIMPL
        }
        fn EnableModeless(&self, _: windows::core::BOOL) -> Res<()> {
            self.state.note_call("View.EnableModeless");
            Ok(())
        }
        fn UIActivate(&self, _: u32) -> Res<()> {
            self.state.note_call("View.UIActivate");
            Ok(())
        }
        fn Refresh(&self) -> Res<()> {
            self.state.note_call("View.Refresh");
            self.state.hint(Trigger::ListenerRefresh);
            Ok(())
        }
        fn CreateViewWindow(
            &self,
            _: Ref<IShellView>,
            _: *const FOLDERSETTINGS,
            _: Ref<IShellBrowser>,
            _: *const windows::Win32::Foundation::RECT,
        ) -> Res<HWND> {
            self.state.note_call("View.CreateViewWindow");
            Err(E_NOTIMPL.into())
        }
        fn DestroyViewWindow(&self) -> Res<()> {
            self.state.note_call("View.DestroyViewWindow");
            Ok(())
        }
        fn GetCurrentInfo(&self) -> Res<FOLDERSETTINGS> {
            self.state.note_call("View.GetCurrentInfo");
            Ok(FOLDERSETTINGS::default())
        }
        fn AddPropertySheetPages(
            &self,
            _: u32,
            _: windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE,
            _: LPARAM,
        ) -> Res<()> {
            self.state.note_call("View.AddPropertySheetPages");
            Err(E_NOTIMPL.into())
        }
        fn SaveViewState(&self) -> Res<()> {
            self.state.note_call("View.SaveViewState");
            Ok(())
        }
        fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> Res<()> {
            self.state.note_call("View.IShellView.SelectItem");
            self.state.select_item(item, flags);
            Ok(())
        }
        fn GetItemObject(&self, _: &_SVGIO, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            self.state.note_call("View.GetItemObject");
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            let unknown: IUnknown = self.to_interface();
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { unknown.query(riid, ppv) }.ok()
        }
    }

    impl IFolderView_Impl for View_Impl {
        fn GetCurrentViewMode(&self) -> Res<u32> {
            self.state.note_call("View.GetCurrentViewMode");
            Ok(1)
        }
        fn SetCurrentViewMode(&self, _: u32) -> Res<()> {
            self.state.note_call("View.SetCurrentViewMode");
            Ok(())
        }
        fn GetFolder(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            self.state.note_call("View.GetFolder");
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            let pidl = self.state.folder.0.cast_const();
            // SAFETY: riid was checked above; the folder's item id list is valid. Binding names
            // the folder only, nothing in it is opened.
            let folder: IUnknown = unsafe {
                if *riid == IShellItem::IID {
                    SHCreateItemFromIDList::<IShellItem>(pidl)?.cast()?
                } else {
                    SHBindToObject::<_, _, IShellFolder>(
                        None::<&IShellFolder>,
                        pidl,
                        None::<&IBindCtx>,
                    )?
                    .cast()?
                }
            };
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { folder.query(riid, ppv) }.ok()
        }
        fn Item(&self, _: i32) -> Res<*mut ITEMIDLIST> {
            self.state.note_call("View.Item");
            Err(E_NOTIMPL.into())
        }
        fn ItemCount(&self, _: &_SVGIO) -> Res<i32> {
            self.state.note_call("View.ItemCount");
            Ok(0)
        }
        fn Items(&self, _: &_SVGIO, _: *const GUID, _: *mut *mut c_void) -> Res<()> {
            self.state.note_call("View.Items");
            Err(E_NOINTERFACE.into())
        }
        fn GetSelectionMarkedItem(&self) -> Res<i32> {
            self.state.note_call("View.GetSelectionMarkedItem");
            Ok(-1)
        }
        fn GetFocusedItem(&self) -> Res<i32> {
            self.state.note_call("View.GetFocusedItem");
            Ok(-1)
        }
        fn GetItemPosition(&self, _: *const ITEMIDLIST) -> Res<windows::Win32::Foundation::POINT> {
            self.state.note_call("View.GetItemPosition");
            Err(E_NOTIMPL.into())
        }
        fn GetSpacing(&self, _: *mut windows::Win32::Foundation::POINT) -> Res<()> {
            self.state.note_call("View.GetSpacing");
            Err(E_NOTIMPL.into())
        }
        fn GetDefaultSpacing(&self) -> Res<windows::Win32::Foundation::POINT> {
            self.state.note_call("View.GetDefaultSpacing");
            Err(E_NOTIMPL.into())
        }
        fn GetAutoArrange(&self) -> Res<()> {
            self.state.note_call("View.GetAutoArrange");
            Err(E_NOTIMPL.into())
        }
        fn SelectItem(&self, _: i32, _: u32) -> Res<()> {
            self.state.note_call("View.IFolderView.SelectItem");
            Ok(())
        }
        fn SelectAndPositionItems(
            &self,
            count: u32,
            items: *const *const ITEMIDLIST,
            _: *const windows::Win32::Foundation::POINT,
            flags: u32,
        ) -> Res<()> {
            self.state.note_call("View.SelectAndPositionItems");
            if items.is_null() {
                return Err(E_POINTER.into());
            }
            for index in 0..count as usize {
                // SAFETY: the caller passes `count` item id list pointers.
                let item = unsafe { *items.add(index) };
                // Only the first one may deselect the others.
                let flags = if index == 0 {
                    flags
                } else {
                    flags & !(SVSI_DESELECTOTHERS.0 as u32)
                };
                self.state.select_item(item, flags);
            }
            Ok(())
        }
    }

    impl IPersist_Impl for View_Impl {
        fn GetClassID(&self) -> Res<GUID> {
            self.state.note_call("View.GetClassID");
            Err(E_NOTIMPL.into())
        }
    }

    impl IPersistIDList_Impl for View_Impl {
        fn SetIDList(&self, _: *const ITEMIDLIST) -> Res<()> {
            self.state.note_call("View.SetIDList");
            Err(E_NOTIMPL.into())
        }
        fn GetIDList(&self) -> Res<*mut ITEMIDLIST> {
            self.state.note_call("View.GetIDList");
            // SAFETY: duplicates a valid list with the COM allocator; the caller frees it.
            let copy = unsafe { ILCombine(Some(self.state.folder.0.cast_const()), None) };
            if copy.is_null() {
                return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
            }
            Ok(copy)
        }
    }
}

/// A VARIANT holding a copy of `text`; freed with `VariantClear`.
fn bstr_variant(text: &str) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a zeroed VARIANT is VT_EMPTY; it now owns a BSTR, which VariantClear frees.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BSTR;
        inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(text));
    }
    v
}

/// A VARIANT holding a reference to `dispatch`; freed with `VariantClear`.
fn dispatch_variant(dispatch: &IDispatch) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: as above, with an added reference VariantClear releases.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_DISPATCH;
        inner.Anonymous.pdispVal = ManuallyDrop::new(Some(dispatch.clone()));
    }
    v
}

/// A `VT_BYREF | VT_VARIANT` VARIANT pointing at `target`, which must outlive it. Owns nothing.
fn variant_ref(target: &mut VARIANT) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a by-reference VARIANT only stores the pointer.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_BYREF.0 | VT_VARIANT.0);
        inner.Anonymous.pvarVal = std::ptr::from_mut(target);
    }
    v
}

/// Calls the event `id` on `sink` with `args` (in the reverse order IDispatch wants them).
fn fire(sink: &IDispatch, id: i32, args: &mut [VARIANT]) -> bool {
    let params = DISPPARAMS {
        rgvarg: if args.is_empty() {
            std::ptr::null_mut()
        } else {
            args.as_mut_ptr()
        },
        rgdispidNamedArgs: std::ptr::null_mut(),
        cArgs: u32::try_from(args.len()).unwrap_or(0),
        cNamedArgs: 0,
    };
    // SAFETY: the arguments live for the call; the result isn't asked for.
    let result = unsafe {
        sink.Invoke(
            id,
            &GUID::zeroed(),
            LOCALE_USER_DEFAULT,
            DISPATCH_METHOD,
            &raw const params,
            None,
            None,
            None,
        )
    };
    if let Err(err) = &result {
        log::debug!("folder visit: event {id} failed: {err}");
    }
    result.is_ok()
}

/// Tells every listener that hasn't heard yet that the window finished navigating to the folder.
fn tell_listeners(state: &State, browser: &IDispatch) {
    // Marked first and called outside the borrow: a listener may subscribe or leave from inside
    // the call (COM dispatches incoming calls while waiting).
    let due: Vec<(Events, IDispatch)> = state
        .sinks
        .borrow_mut()
        .iter_mut()
        .filter(|s| !s.told)
        .map(|s| {
            s.told = true;
            (s.events, s.dispatch.clone())
        })
        .collect();
    if !due.is_empty() {
        state.running.set_calling(true);
    }
    // Each call noted in the log.
    let noted = |sink: &IDispatch, id: i32, args: &mut [VARIANT]| {
        let ok = fire(sink, id, args);
        if let Ok(mut journal) = state.journal.try_borrow_mut() {
            journal.note_fired(id, ok);
        }
    };
    for (events, sink) in &due {
        tell_navigation(state, *events, sink, browser, &noted);
    }
    if !due.is_empty() {
        state.running.set_calling(false);
        // Their event calls are back: what they filled inside them is there now.
        state.hint(Trigger::Told);
    }
}

/// `DWebBrowserEvents::BeforeNavigate`, `DownloadBegin` (both interfaces)
const DISPID_BEFORENAVIGATE: i32 = 100;
const DISPID_DOWNLOADBEGIN: i32 = 106;
/// `DWebBrowserEvents2::BeforeNavigate2`
const DISPID_BEFORENAVIGATE2: i32 = 250;

/// Tells a listener of the navigation the way File Explorer does, with the folder's path as the
/// URL. `DWebBrowserEvents`: BeforeNavigate, DownloadBegin, NavigateComplete, DownloadComplete.
/// `DWebBrowserEvents2`: BeforeNavigate2, DownloadBegin, NavigateComplete2, DownloadComplete,
/// DocumentComplete. Arguments go last to first, as `IDispatch` wants them. WPS Cloud tells a
/// window the folder it shows was filled (`SHCNE_UPDATEDIR`) only after the whole sequence; with
/// NavigateComplete alone it filled the folder and said nothing (#319).
fn tell_navigation(
    state: &State,
    events: Events,
    sink: &IDispatch,
    browser: &IDispatch,
    fire: &dyn Fn(&IDispatch, i32, &mut [VARIANT]),
) {
    let path = state.dir.to_string_lossy();
    let mut cancel = VARIANT_FALSE;
    match events {
        Events::Old => {
            // BeforeNavigate(URL, Flags, TargetFrameName, PostData, Headers, Cancel)
            let mut post_data = VARIANT::default();
            let mut before = [
                bool_ref(&mut cancel),
                bstr_variant(""),
                variant_ref(&mut post_data),
                bstr_variant(""),
                i4_variant(0),
                bstr_variant(&path),
            ];
            fire(sink, DISPID_BEFORENAVIGATE, &mut before);
            clear(&mut before);
            fire(sink, DISPID_DOWNLOADBEGIN, &mut []);
            let mut url = [bstr_variant(&path)];
            fire(sink, DISPID_NAVIGATECOMPLETE, &mut url);
            clear(&mut url);
            fire(sink, DISPID_DOWNLOADCOMPLETE, &mut []);
        }
        Events::New => {
            // BeforeNavigate2(pDisp, URL, Flags, TargetFrameName, PostData, Headers, Cancel)
            let mut url = bstr_variant(&path);
            let (mut flags, mut target, mut post_data, mut headers) = (
                i4_variant(0),
                VARIANT::default(),
                VARIANT::default(),
                VARIANT::default(),
            );
            let mut before = [
                bool_ref(&mut cancel),
                variant_ref(&mut headers),
                variant_ref(&mut post_data),
                variant_ref(&mut target),
                variant_ref(&mut flags),
                variant_ref(&mut url),
                dispatch_variant(browser),
            ];
            fire(sink, DISPID_BEFORENAVIGATE2, &mut before);
            clear(&mut before);
            fire(sink, DISPID_DOWNLOADBEGIN, &mut []);
            // NavigateComplete2 and DocumentComplete(pDisp, URL)
            let mut args = [variant_ref(&mut url), dispatch_variant(browser)];
            fire(sink, DISPID_NAVIGATECOMPLETE2, &mut args);
            fire(sink, DISPID_DOWNLOADCOMPLETE, &mut []);
            fire(sink, DISPID_DOCUMENTCOMPLETE, &mut args);
            clear(&mut args);
            clear(std::slice::from_mut(&mut url));
        }
    }
}

/// A `VT_BYREF | VT_BOOL` VARIANT pointing at `target`, which must outlive it. Owns nothing.
fn bool_ref(target: &mut VARIANT_BOOL) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a by-reference VARIANT only stores the pointer.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_BYREF.0 | VT_BOOL.0);
        inner.Anonymous.pboolVal = std::ptr::from_mut(target);
    }
    v
}

/// A `VT_I4` VARIANT.
fn i4_variant(value: i32) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a zeroed VARIANT is VT_EMPTY; it now holds an i32.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = value;
    }
    v
}

fn clear(variants: &mut [VARIANT]) {
    for v in variants {
        // SAFETY: each VARIANT was built above; by-reference ones own nothing.
        let _ = unsafe { VariantClear(v) };
    }
}

/// The folder's `file:` URL, as `LocationURL` gives it.
fn file_url(name: &[u16]) -> String {
    let mut buffer = vec![0_u16; 32_768];
    let mut len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    // SAFETY: `name` is NUL-terminated; the buffer holds `len` units.
    let made = unsafe {
        UrlCreateFromPathW(
            PCWSTR(name.as_ptr()),
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
            0,
        )
    };
    if made.is_err() {
        return String::new();
    }
    let len = (len as usize).min(buffer.len());
    let end = buffer[..len].iter().position(|&c| c == 0).unwrap_or(len);
    String::from_utf16_lossy(&buffer[..end])
}

/// The visit's loop state, for its window procedure (same thread).
#[derive(Clone)]
struct Current {
    state: Rc<State>,
    browser: IDispatch,
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

/// The visit's message-only window: listeners to tell, shell change notifications.
unsafe extern "system" fn visit_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // Cloned out, so a nested message (COM dispatches while a listener's call waits) can borrow
    // it again.
    let current = CURRENT.with(|c| c.borrow().clone());
    match (msg, current) {
        (WM_TELL, Some(current)) => {
            tell_listeners(&current.state, &current.browser);
            LRESULT(0)
        }
        (WM_SHELL_CHANGE, Some(current)) => {
            let mut event = 0_i32;
            let mut items: *mut *mut ITEMIDLIST = std::ptr::null_mut();
            // SAFETY: `w` and `l` are the notification's lock handle and process id, as
            // SHCNRF_NewDelivery hands them. The two item id lists it locks are only read until
            // the lock is released, right after.
            let hints = unsafe {
                let lock = SHChangeNotification_Lock(
                    HANDLE(w.0 as *mut c_void),
                    l.0 as u32,
                    Some(&raw mut items),
                    Some(&raw mut event),
                );
                if lock.is_invalid() {
                    false
                } else {
                    let (first, second) = if items.is_null() {
                        (std::ptr::null(), std::ptr::null())
                    } else {
                        (items.read().cast_const(), items.add(1).read().cast_const())
                    };
                    let hints = current
                        .state
                        .notification(event.cast_unsigned(), first, second);
                    let _ = SHChangeNotification_Unlock(lock);
                    hints
                }
            };
            if hints {
                current.state.hint(Trigger::ShellNotify);
            }
            LRESULT(0)
        }
        (WM_NAVIGATED, Some(current)) => {
            current.state.navigated();
            LRESULT(0)
        }
        (WM_RELIST, Some(current)) => {
            current.state.flush_relist();
            LRESULT(0)
        }
        (WM_PARENT_ENTRY, Some(current)) => {
            current
                .state
                .journal
                .borrow_mut()
                .mark("first parent change");
            current.state.hint(Trigger::ParentEntryChanged);
            LRESULT(0)
        }
        // SAFETY: forwards to the default procedure with the same arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// The visit's thread: registers, runs the message loop until asked to end, revokes.
fn run(window: isize, visit: FolderVisit, running: &Arc<Running>) {
    let mut msg = MSG::default();
    // SAFETY: creates this thread's message queue, so WM_QUIT can be posted to it from now on.
    let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: plain thread id query.
    running
        .thread
        .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    if !running.quit.load(Ordering::SeqCst) {
        // SAFETY: first COM call on this thread; paired with CoUninitialize below when it succeeds.
        let com =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        let dir = visit.dir.clone();
        if let Err(err) = announce(HWND(window as *mut c_void), visit, running) {
            log::info!("folder visit for {}: {err}", dir.display());
        }
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
    running.finish();
}

/// The visit's message-only window.
fn message_window() -> windows::core::Result<HWND> {
    let class = windows::core::w!("ArkeFolderVisit");
    // SAFETY: registers a window class of this module (twice fails harmlessly) and creates a
    // message-only window of it on this thread.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(visit_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&raw const wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn announce(hwnd: HWND, visit: FolderVisit, running: &Arc<Running>) -> windows::core::Result<()> {
    let FolderVisit {
        dir,
        changed,
        select,
        ..
    } = visit;
    if let Some((provider, population)) = crate::win_cloud::sync_root_provider(&dir) {
        log::info!(
            "folder visit for {} (sync provider {provider:?}, population policy {population})",
            dir.display()
        );
    }
    let name: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut raw = std::ptr::null_mut();
    // SAFETY: name is NUL-terminated; raw receives an allocated item id list. Parsing names the
    // folder only.
    unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }?;
    let folder = Pidl(raw);
    // SAFETY: reads the registered type library; nothing is written.
    let type_info = unsafe {
        LoadRegTypeLib(&LIBID_SHDOCVW, 1, 1, 0)
            .and_then(|lib| lib.GetTypeInfoOfGuid(&IWebBrowser2::IID))
    }
    .inspect_err(|err| log::debug!("folder visit: no SHDocVw type info: {err}"))
    .ok();
    let messages = message_window()?;
    let pidl = folder.0.cast_const();
    let state = Rc::new(State {
        hwnd,
        messages,
        dir: dir.clone(),
        folder,
        url: file_url(&name),
        name: dir
            .file_name()
            .unwrap_or(dir.as_os_str())
            .to_string_lossy()
            .into_owned(),
        type_info,
        sinks: RefCell::default(),
        next_cookie: Cell::new(0),
        selected: RefCell::default(),
        changed,
        select,
        running: Arc::clone(running),
        relist: Cell::default(),
        journal: RefCell::default(),
    });
    state.journal.borrow_mut().start();
    let result = register_and_wait(&state, pidl, &dir);
    state.journal.borrow_mut().mark("ended");
    if result.is_ok() {
        log::info!(
            "folder visit for {} ended: {}",
            dir.display(),
            state.journal.borrow().summary()
        );
    }
    CURRENT.with(|c| *c.borrow_mut() = None);
    state.sinks.borrow_mut().clear();
    // SAFETY: our own window, on this thread.
    let _ = unsafe { DestroyWindow(messages) };
    result
}

/// Registers the window, listens for shell notifications and runs the loop until WM_QUIT.
fn register_and_wait(
    state: &Rc<State>,
    pidl: *const ITEMIDLIST,
    dir: &Path,
) -> windows::core::Result<()> {
    let view: IShellView = com::View {
        state: Rc::clone(state),
    }
    .into();
    let browser: IWebBrowser2 = com::Browser {
        state: Rc::clone(state),
        view,
    }
    .into();
    let dispatch: IDispatch = browser.cast()?;
    CURRENT.with(|c| {
        *c.borrow_mut() = Some(Current {
            state: Rc::clone(state),
            browser: dispatch.clone(),
        });
    });
    // Shell-level notifications for the folder and its items (what SHChangeNotify sends), before
    // the window is in the list: a client may fill the folder and say so as soon as it hears.
    let entry = SHChangeNotifyEntry {
        pidl: pidl.cast_mut(),
        fRecursive: false.into(),
    };
    // SAFETY: our own window gets WM_SHELL_CHANGE; `entry` is read during the call.
    let notify = unsafe {
        SHChangeNotifyRegister(
            state.messages,
            SHCNRF_ShellLevel | SHCNRF_NewDelivery,
            WATCHED_EVENTS.cast_signed(),
            WM_SHELL_CHANGE,
            1,
            &raw const entry,
        )
    };
    if notify == 0 {
        log::debug!("folder visit: no shell notifications for {}", dir.display());
    }
    // The folder's entry in its parent, also before the window is in the list. Stopped once the
    // loop ended, while the message window it posts to still exists.
    let parent = ParentWatch::start(state.messages, dir);
    let result = navigate_and_wait(state, pidl, &dispatch);
    drop(parent);
    if notify != 0 {
        // SAFETY: undoes the registration made above.
        let _ = unsafe { SHChangeNotifyDeregister(notify) };
    }
    result
}

/// Puts the window in the shell's window list for the folder, runs the loop until WM_QUIT and
/// takes it out again.
fn navigate_and_wait(
    state: &State,
    pidl: *const ITEMIDLIST,
    dispatch: &IDispatch,
) -> windows::core::Result<()> {
    // SAFETY: standard COM activation of the shell's window list.
    let windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }?;
    // SAFETY: the list is valid for ILGetSize bytes; the variant owns a copy. `pidl` is the
    // state's own list, alive as long as `state`.
    let mut location = unsafe { InitVariantFromBuffer(pidl.cast(), ILGetSize(Some(pidl))) }?;
    // SAFETY: valid interface and variant for the calls.
    let registered = unsafe {
        windows
            .Register(dispatch, state.hwnd.0 as i32, SWC_BROWSER)
            .and_then(|cookie| {
                windows
                    .OnNavigate(cookie, &raw const location)
                    .inspect_err(|_| {
                        let _ = windows.Revoke(cookie);
                    })
                    .map(|()| cookie)
            })
    };
    // SAFETY: the variant was made by InitVariantFromBuffer above.
    let _ = unsafe { VariantClear(&raw mut location) };
    let cookie = registered?;
    state.journal.borrow_mut().mark("registered");
    // Queued behind the WM_TELL of the listeners that subscribed meanwhile, so they are told
    // first: then the folder is listed once more.
    // SAFETY: posts to this visit's own window.
    let _ = unsafe { PostMessageW(Some(state.messages), WM_NAVIGATED, WPARAM(0), LPARAM(0)) };
    log::debug!("folder visit registered for {}", state.dir.display());
    let mut msg = MSG::default();
    // SAFETY: msg is a valid out pointer; blocks until a message arrives. 0 is WM_QUIT: the pane
    // left the folder, closed, or Arke is quitting.
    while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: msg came from GetMessageW.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    // SAFETY: undoes the registration made above.
    let _ = unsafe { windows.Revoke(cookie) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_told_apart_by_their_interface() {
        assert_eq!(Events::of(&DWebBrowserEvents::IID), Some(Events::Old));
        assert_eq!(Events::of(&DWebBrowserEvents2::IID), Some(Events::New));
        assert_eq!(Events::of(&IDispatch::IID), None);
    }

    #[test]
    fn a_folder_location_is_a_file_url() {
        let name: Vec<u16> = r"C:\Cloud Drive\a b"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let url = file_url(&name);
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.to_lowercase().contains("cloud%20drive"), "{url}");
    }

    /// Items coming, going or renamed relist; an item's state changing (sync progress) doesn't.
    #[test]
    fn only_shell_notifications_about_items_relist() {
        for event in [
            SHCNE_CREATE,
            SHCNE_MKDIR,
            SHCNE_DELETE,
            SHCNE_RMDIR,
            SHCNE_RENAMEITEM,
            SHCNE_RENAMEFOLDER,
            SHCNE_UPDATEDIR,
        ] {
            assert!(notifies_items(event.0), "{event:?}");
        }
        assert!(!notifies_items(
            windows::Win32::UI::Shell::SHCNE_UPDATEITEM.0
        ));
        assert!(!notifies_items(
            windows::Win32::UI::Shell::SHCNE_ATTRIBUTES.0
        ));
        assert!(relists(Trigger::ShellNotify));
        assert!(relists(Trigger::ListenerRefresh));
    }

    /// What a "Show in folder" caller selects adds up; `SVSI_DESELECTOTHERS` starts over; flags
    /// that don't select change nothing.
    #[test]
    fn selections_from_the_shell_add_up() {
        let select = SVSI_SELECT.0 as u32;
        let first = select | SVSI_DESELECTOTHERS.0 as u32 | SVSI_ENSUREVISIBLE.0 as u32;
        let mut selected = Vec::new();
        assert!(note_selected(&mut selected, Some("a.txt".into()), first));
        assert!(note_selected(&mut selected, Some("b.txt".into()), select));
        assert!(note_selected(&mut selected, Some("b.txt".into()), select));
        assert_eq!(selected, [Arc::<str>::from("a.txt"), "b.txt".into()]);
        assert!(!note_selected(&mut selected, Some("c.txt".into()), 0));
        assert!(!note_selected(&mut selected, None, select));
        assert!(note_selected(&mut selected, Some("c.txt".into()), first));
        assert_eq!(selected, [Arc::<str>::from("c.txt")]);
    }

    /// Leaving a pane that has no visit is a no-op; a visit without a window starts nothing.
    #[test]
    fn nothing_runs_without_a_window() {
        end(u64::MAX);
        visit(
            u64::MAX - 1,
            Some(FolderVisit {
                window: None,
                dir: PathBuf::from(r"C:\nowhere"),
                changed: Box::new(|| panic!("a visit that never started has no hints")),
                select: Box::new(|_| panic!("a visit that never started has no selection")),
            }),
        );
        assert!(!VISITS.lock().contains_key(&(u64::MAX - 1)));
    }

    /// A "Show in folder" caller that found the window goes through its document: the view for
    /// `SID_SFolderView`, whose `SelectItem` selects in the pane.
    #[test]
    fn show_in_folder_reaches_the_view_through_the_document() {
        use windows::Win32::UI::Shell::ILFindLastID;
        const SID_SFOLDERVIEW: GUID = GUID::from_u128(0xcde7_25b0_ccc9_4519_917e_325d_72fa_b4ce);
        // SAFETY: this test thread's apartment.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let parse = |path: &Path| {
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut raw = std::ptr::null_mut();
            // SAFETY: NUL-terminated name; raw receives an allocated list.
            unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }
                .unwrap();
            Pidl(raw)
        };
        let picked: Rc<RefCell<Vec<Arc<str>>>> = Rc::default();
        let into = Rc::clone(&picked);
        let state = Rc::new(State {
            hwnd: HWND::default(),
            messages: HWND::default(),
            dir: dir.clone(),
            folder: parse(&dir),
            url: String::new(),
            name: String::new(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(|| {}),
            select: Box::new(move |names| *into.borrow_mut() = names),
            running: Arc::default(),
            relist: Cell::default(),
            journal: RefCell::default(),
        });
        let view: IShellView = com::View {
            state: Rc::clone(&state),
        }
        .into();
        let browser: IWebBrowser2 = com::Browser { state, view }.into();
        let file = parse(&dir.join("a.txt"));
        // SAFETY: the calls a "Show in folder" caller makes; `file` outlives them.
        unsafe {
            let document = browser.Document().unwrap();
            let provider: IServiceProvider = document.cast().unwrap();
            let view: IShellView = provider.QueryService(&SID_SFOLDERVIEW).unwrap();
            view.SelectItem(
                ILFindLastID(file.0.cast_const()),
                (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0) as u32,
            )
            .unwrap();
        }
        assert_eq!(*picked.borrow(), [Arc::<str>::from("a.txt")]);
    }

    /// Ending a visit before its thread got going still ends it: the thread sees the request.
    #[test]
    fn a_visit_ended_before_its_thread_started_ends() {
        let running = Arc::new(Running::default());
        quit(&running);
        assert!(running.quit.load(Ordering::SeqCst));
        running.finish();
        running.wait();
    }

    /// `SHCNE_UPDATEITEM` relists only for the folder itself (also a sync root's top folder and a
    /// drive root, however the path is written), never for an item in it or elsewhere.
    #[test]
    fn only_the_folders_own_update_relists() {
        let update = SHCNE_UPDATEITEM.0;
        let root = Path::new(r"C:\Users\me\WPS Cloud");
        for (item, folder, target) in [
            (r"C:\Users\me\WPS Cloud", root, Target::Folder),
            (r"c:\users\ME\wps cloud\", root, Target::Folder),
            (r"\\?\C:\Users\me\WPS Cloud", root, Target::Folder),
            (r"C:\Users\me\WPS Cloud\a.docx", root, Target::Inside),
            (r"C:\Users\me\WPS Cloud\Team", root, Target::Inside),
            (
                r"C:\Users\me\WPS Cloud\Team\b.docx",
                root,
                Target::Elsewhere,
            ),
            (r"C:\Users\me", root, Target::Elsewhere),
            (r"C:\Users\me\WPS Cloud 2", root, Target::Elsewhere),
            (r"C:\", Path::new(r"C:\"), Target::Folder),
            (r"C:\a.txt", Path::new(r"C:\"), Target::Inside),
        ] {
            assert_eq!(target_of(Path::new(item), folder), target, "{item}");
        }
        assert!(relists_on_notification(update, Some(Target::Folder)));
        assert!(!relists_on_notification(update, Some(Target::Inside)));
        assert!(!relists_on_notification(update, Some(Target::Elsewhere)));
        assert!(!relists_on_notification(update, None));
        assert!(relists_on_notification(SHCNE_CREATE.0, None));
        assert!(relists_on_notification(SHCNE_UPDATEDIR.0, None));
        let attributes = windows::Win32::UI::Shell::SHCNE_ATTRIBUTES.0;
        assert!(!relists_on_notification(attributes, None));
        assert_eq!(WATCHED_EVENTS & update, update, "registered for");
    }

    /// The navigation makes exactly one relist per visit, and hints that come together (a burst
    /// of notifications, the navigation) make one.
    #[test]
    fn one_relist_after_the_navigation_and_one_per_burst() {
        let mut relist = Relist::default();
        assert!(relist.navigated());
        assert!(relist.hint(), "wakes the loop");
        assert!(relist.flush(), "lists again");
        assert!(!relist.flush(), "once");
        assert!(!relist.navigated(), "only the first navigation of a visit");

        assert!(relist.hint());
        assert!(!relist.hint(), "already queued");
        assert!(!relist.hint());
        assert!(relist.flush());
        assert!(!relist.flush(), "one relist for the burst");
        assert!(relists(Trigger::Navigated));
    }

    /// The visit's state relists once after the navigation, however often it is told, and once
    /// for hints that come before the wake-up; the log counts them.
    #[test]
    fn the_visit_relists_once_after_the_navigation() {
        let count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&count);
        let state = State {
            hwnd: HWND::default(),
            messages: HWND::default(),
            dir: PathBuf::from(r"C:\cloud"),
            folder: Pidl(std::ptr::null_mut()),
            url: String::new(),
            name: String::new(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(move || counted.set(counted.get() + 1)),
            select: Box::new(|_| {}),
            running: Arc::default(),
            relist: Cell::default(),
            journal: RefCell::default(),
        };
        state.navigated();
        state.navigated();
        state.hint(Trigger::ShellNotify);
        state.flush_relist();
        state.flush_relist();
        assert_eq!(count.get(), 1);
        state.hint(Trigger::ListenerRefresh);
        state.flush_relist();
        assert_eq!(count.get(), 2);
        let summary = state.journal.borrow().summary();
        assert!(summary.contains("Navigated x1"), "{summary}");
        assert!(summary.contains("ShellNotify x1"), "{summary}");
        assert!(summary.ends_with("-> relists 2"), "{summary}");
    }

    /// A listener told after the navigation's relist (it subscribed later) makes one more; told
    /// together with a shell notification, one for both. Telling nobody hints nothing.
    #[test]
    fn a_listener_told_later_relists_again() {
        let count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&count);
        let state = State {
            hwnd: HWND::default(),
            messages: HWND::default(),
            dir: PathBuf::from(r"C:\cloud"),
            folder: Pidl(std::ptr::null_mut()),
            url: String::new(),
            name: String::new(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(move || counted.set(counted.get() + 1)),
            select: Box::new(|_| {}),
            running: Arc::default(),
            relist: Cell::default(),
            journal: RefCell::default(),
        };
        state.navigated();
        state.flush_relist();
        assert_eq!(count.get(), 1);
        state.hint(Trigger::Told);
        state.flush_relist();
        assert_eq!(count.get(), 2, "Navigated, then a later Told: two relists");

        state.hint(Trigger::Told);
        state.hint(Trigger::ShellNotify);
        state.flush_relist();
        state.flush_relist();
        assert_eq!(count.get(), 3, "Told and ShellNotify together: one relist");

        // SAFETY: this test thread's apartment, for the browser object below.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let state = Rc::new(state);
        let view: IShellView = com::View {
            state: Rc::clone(&state),
        }
        .into();
        let browser: IDispatch = view.cast().unwrap();
        tell_listeners(&state, &browser);
        state.flush_relist();
        assert_eq!(count.get(), 3, "no listener to tell");
        let summary = state.journal.borrow().summary();
        assert!(summary.contains("Told x2"), "{summary}");
        assert!(summary.ends_with("-> relists 3"), "{summary}");
    }

    /// The log names the events fired at listeners, and whether they went through.
    #[test]
    fn the_visit_log_has_the_events_fired() {
        let mut journal = Journal::default();
        journal.note_fired(101, true);
        journal.note_fired(101, true);
        journal.note_fired(104, false);
        let summary = journal.summary();
        assert!(
            summary.contains("; fired: 101 ok x2, 104 failed x1"),
            "{summary}"
        );
    }

    /// Each listener is told of the navigation once, with File Explorer's whole sequence and the
    /// folder's path as the URL, on both event interfaces (WPS Cloud waits for it, #319).
    #[test]
    fn listeners_get_explorers_navigation_sequence() {
        let state = Rc::new(State {
            hwnd: HWND::default(),
            messages: HWND::default(),
            dir: PathBuf::from(r"C:\cloud\docs"),
            folder: Pidl(std::ptr::null_mut()),
            url: "file:///C:/cloud/docs".into(),
            name: "docs".into(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(|| {}),
            select: Box::new(|_| {}),
            running: Arc::default(),
            relist: Cell::default(),
            journal: RefCell::default(),
        });
        let (old, new): (Seen, Seen) = Default::default();
        for (events, seen, cookie) in [(Events::Old, &old, 1), (Events::New, &new, 2)] {
            state.sinks.borrow_mut().push(Sink {
                cookie,
                events,
                dispatch: Recorder {
                    seen: Rc::clone(seen),
                }
                .into(),
                told: false,
            });
        }
        let view: IShellView = com::View {
            state: Rc::clone(&state),
        }
        .into();
        let browser: IDispatch = view.cast().unwrap();
        tell_listeners(&state, &browser);
        tell_listeners(&state, &browser);
        let path = Some(r"C:\cloud\docs".to_owned());
        assert_eq!(
            *old.borrow(),
            [
                (DISPID_BEFORENAVIGATE, 6, path.clone()),
                (DISPID_DOWNLOADBEGIN, 0, None),
                (DISPID_NAVIGATECOMPLETE, 1, path.clone()),
                (DISPID_DOWNLOADCOMPLETE, 0, None),
            ],
            "once, in order"
        );
        assert_eq!(
            *new.borrow(),
            [
                (DISPID_BEFORENAVIGATE2, 7, path.clone()),
                (DISPID_DOWNLOADBEGIN, 0, None),
                (DISPID_NAVIGATECOMPLETE2, 2, path.clone()),
                (DISPID_DOWNLOADCOMPLETE, 0, None),
                (DISPID_DOCUMENTCOMPLETE, 2, path),
            ]
        );
        let summary = state.journal.borrow().summary();
        assert!(
            summary.contains("; fired: 100 ok x1, 106 ok x2"),
            "{summary}"
        );
    }

    /// An event a listener got: DISPID, how many arguments, and the URL among them.
    type Fired = (i32, u32, Option<String>);

    /// The events one listener got.
    type Seen = Rc<RefCell<Vec<Fired>>>;

    #[allow(
        clippy::inline_always,
        clippy::ref_as_ptr,
        clippy::wildcard_imports,
        reason = "The macro-generated code of #[implement] trips them, as in `mod com`"
    )]
    mod recorder {
        use super::*;

        /// A listener that notes every event it gets.
        #[implement(IDispatch)]
        pub(super) struct Recorder {
            pub(super) seen: Seen,
        }

        impl IDispatch_Impl for Recorder_Impl {
            fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
                Ok(0)
            }
            fn GetTypeInfo(&self, _: u32, _: u32) -> windows::core::Result<ITypeInfo> {
                Err(E_NOTIMPL.into())
            }
            fn GetIDsOfNames(
                &self,
                _: *const GUID,
                _: *const PCWSTR,
                _: u32,
                _: u32,
                _: *mut i32,
            ) -> windows::core::Result<()> {
                Err(E_NOTIMPL.into())
            }
            fn Invoke(
                &self,
                id: i32,
                _: *const GUID,
                _: u32,
                _: DISPATCH_FLAGS,
                params: *const DISPPARAMS,
                _: *mut VARIANT,
                _: *mut EXCEPINFO,
                _: *mut u32,
            ) -> windows::core::Result<()> {
                // SAFETY: the caller's arguments, valid for the call.
                let params = unsafe { &*params };
                let args: &[VARIANT] = if params.cArgs == 0 {
                    &[]
                } else {
                    // SAFETY: `cArgs` VARIANT values at `rgvarg`.
                    unsafe { std::slice::from_raw_parts(params.rgvarg, params.cArgs as usize) }
                };
                let url = args.iter().find_map(text_of);
                self.seen.borrow_mut().push((id, params.cArgs, url));
                Ok(())
            }
        }
    }
    use recorder::Recorder;

    /// The text of a `VT_BSTR` argument, or of a by-reference VARIANT holding one.
    fn text_of(arg: &VARIANT) -> Option<String> {
        // SAFETY: reads the VARIANT by its type tag.
        unsafe {
            let inner = &*arg.Anonymous.Anonymous;
            if inner.vt == VT_BSTR {
                let text = (*inner.Anonymous.bstrVal).to_string();
                return (!text.is_empty()).then_some(text);
            }
            if inner.vt == VARENUM(VT_BYREF.0 | VT_VARIANT.0) && !inner.Anonymous.pvarVal.is_null()
            {
                return text_of(&*inner.Anonymous.pvarVal);
            }
        }
        None
    }

    /// The log says when the visit registered and first heard from a listener or the shell, the
    /// first time each, after the visit started.
    #[test]
    fn the_visit_log_has_its_timing() {
        let mut journal = Journal::default();
        journal.mark("registered");
        assert!(journal.marks.is_empty(), "nothing before the visit started");
        journal.start();
        journal.mark("registered");
        journal.mark("first listener");
        journal.mark("registered");
        journal.mark("ended");
        let steps: Vec<&str> = journal.marks.iter().map(|(s, _)| *s).collect();
        assert_eq!(steps, ["registered", "first listener", "ended"]);
        let summary = journal.summary();
        assert!(summary.contains("; timing: registered +"), "{summary}");
        assert!(summary.contains(" ms, first listener +"), "{summary}");
    }

    /// The log block: notifications by kind with the first one's paths, calls by method.
    #[test]
    fn the_visit_log_is_one_compact_line() {
        let mut journal = Journal::default();
        assert_eq!(
            journal.summary(),
            "notifications: none; listener calls: none; hints: none -> relists 0"
        );
        journal.note_notification("UPDATEITEM(folder)", || r"C:\cloud".into());
        for _ in 0..40 {
            let paths = || r"C:\cloud\a.docx".to_string();
            journal.note_notification("UPDATEITEM(item)", paths);
        }
        assert!(!journal.first_of("UPDATEITEM(item)"));
        journal.note_call("Advise(DWebBrowserEvents)");
        journal.note_call("LocationURL");
        journal.note_call("LocationURL");
        journal.note_hint(Trigger::Navigated);
        journal.relists = 1;
        assert_eq!(
            journal.summary(),
            r"notifications: UPDATEITEM(folder) x1 [C:\cloud], UPDATEITEM(item) x40 [C:\cloud\a.docx]; listener calls: Advise(DWebBrowserEvents) x1, LocationURL x2; hints: Navigated x1 -> relists 1"
        );
        assert_eq!(
            event_name(SHCNE_UPDATEITEM.0, Some(Target::Folder)),
            "UPDATEITEM(folder)"
        );
        assert_eq!(event_name(SHCNE_CREATE.0, None), "CREATE");
        assert_eq!(event_name(0, None), "other");
    }

    /// Live: a real `SHCNE_UPDATEITEM` for the folder itself hints, one for an item in it doesn't.
    /// Goes through the shell's change notifications (explorer.exe delivers them).
    #[test]
    #[ignore = "uses the shell's change notifications, delivered by explorer.exe"]
    fn a_real_update_of_the_folder_itself_hints() {
        use std::time::{Duration, Instant};
        use windows::Win32::UI::Shell::{
            SHCNF_FLAGS, SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify,
        };
        use windows::Win32::UI::WindowsAndMessaging::PM_REMOVE;
        // SAFETY: this test thread's apartment.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let tmp_dir = tempfile::tempdir().unwrap();
        let dir = tmp_dir.path().to_path_buf();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let name: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut raw = std::ptr::null_mut();
        // SAFETY: NUL-terminated name; raw receives an allocated list.
        unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }.unwrap();
        let count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&count);
        let state = Rc::new(State {
            hwnd: HWND::default(),
            messages: message_window().unwrap(),
            dir: dir.clone(),
            folder: Pidl(raw),
            url: String::new(),
            name: String::new(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(move || counted.set(counted.get() + 1)),
            select: Box::new(|_| {}),
            running: Arc::default(),
            relist: Cell::default(),
            journal: RefCell::default(),
        });
        CURRENT.with(|c| {
            *c.borrow_mut() = Some(Current {
                state: Rc::clone(&state),
                browser: com::View {
                    state: Rc::clone(&state),
                }
                .into(),
            });
        });
        let entry = SHChangeNotifyEntry {
            pidl: state.folder.0,
            fRecursive: false.into(),
        };
        // SAFETY: our own window; `entry` is read during the call.
        let notify = unsafe {
            SHChangeNotifyRegister(
                state.messages,
                SHCNRF_ShellLevel | SHCNRF_NewDelivery,
                WATCHED_EVENTS.cast_signed(),
                WM_SHELL_CHANGE,
                1,
                &raw const entry,
            )
        };
        assert_ne!(notify, 0);
        let send = |path: &Path| {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let flags = SHCNF_FLAGS(SHCNF_PATHW.0 | SHCNF_FLUSHNOWAIT.0);
            // SAFETY: a NUL-terminated path for the call.
            unsafe {
                SHChangeNotify(SHCNE_UPDATEITEM, flags, Some(wide.as_ptr().cast()), None);
            }
        };
        // Pumps this thread's messages until `kind` was seen (and everything queued after it).
        let pump_until = |kind: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let mut msg = MSG::default();
                // SAFETY: msg is a valid out pointer; this thread's own messages.
                while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    // SAFETY: msg came from PeekMessageW.
                    unsafe { DispatchMessageW(&raw const msg) };
                }
                if !state.journal.borrow().first_of(kind) {
                    return;
                }
                assert!(Instant::now() < deadline, "no {kind} notification came");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        send(&dir.join("a.txt"));
        pump_until("UPDATEITEM(item)");
        assert_eq!(count.get(), 0, "an item's update doesn't relist");
        send(&dir);
        pump_until("UPDATEITEM(folder)");
        assert_eq!(count.get(), 1, "the folder's own update relists once");
        // SAFETY: undoes the registration and the window made above.
        unsafe {
            let _ = SHChangeNotifyDeregister(notify);
            let _ = DestroyWindow(state.messages);
        }
        CURRENT.with(|c| *c.borrow_mut() = None);
    }

    /// The parent's watcher hints for the folder's own entry in any case, and for an item renamed
    /// or created under its name; not for a sibling, nor for the folder going away. A drive root
    /// has no parent to watch.
    #[test]
    fn only_the_folders_own_entry_in_its_parent_hints() {
        use arke_core::{Entry, EntryKind};
        use std::ffi::OsStr;
        let parent = Path::new(r"C:\Users\me\WPS Cloud");
        let at = |name: &str| parent.join(name);
        let present =
            |name: &str| Change::Present(at(name), Entry::new(name, EntryKind::Dir, 0, None, 0x10));
        let folder = name_key(OsStr::new("Team"));
        // FILE_ACTION_MODIFIED of the folder, named in another case than the pane's path.
        assert!(names_folder(&present("TEAM"), &folder));
        assert!(names_folder(&Change::Unknown(at("team")), &folder));
        // ADDED or RENAMED_NEW_NAME under its name.
        assert!(names_folder(&present("Team"), &folder));
        // Siblings.
        assert!(!names_folder(&Change::Unknown(at("Team 2")), &folder));
        assert!(!names_folder(&present("Tea"), &folder));
        assert!(!names_folder(&Change::Unknown(at("Team.docx")), &folder));
        // REMOVED or RENAMED_OLD_NAME.
        assert!(!names_folder(&Change::Removed(at("Team")), &folder));
        let accented = name_key(OsStr::new("Équipe"));
        assert!(names_folder(&Change::Unknown(at("ÉQUIPE")), &accented));

        let sibling = ChangeEvent::Changed(vec![Change::Unknown(at("Other"))]);
        assert!(!about_folder(&sibling, &folder));
        let renamed = ChangeEvent::Changed(vec![Change::Removed(at("Team.tmp")), present("Team")]);
        assert!(about_folder(&renamed, &folder));
        assert!(
            about_folder(&ChangeEvent::Rescan, &folder),
            "lost changes may have had it"
        );
        assert!(!about_folder(&ChangeEvent::Changed(Vec::new()), &folder));
        assert!(relists(Trigger::ParentEntryChanged));
        assert!(ParentWatch::start(HWND::default(), Path::new(r"C:\")).is_none());
    }

    /// Live: a change of the folder's own entry in its parent (its attributes, as a sync client
    /// filling it changes it) hints, however the folder's path is spelled; a sibling's change
    /// doesn't; the parent can still be renamed meanwhile; once the watch stopped, nothing more
    /// comes.
    #[test]
    fn a_change_of_the_folders_entry_in_its_parent_hints() {
        use std::time::{Duration, Instant};
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
            SetFileAttributesW,
        };
        use windows::Win32::UI::WindowsAndMessaging::PM_REMOVE;
        // SAFETY: this test thread's apartment.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let tmp_dir = tempfile::tempdir().unwrap();
        let parent = tmp_dir.path().join("parent");
        let dir = parent.join("Cloud Folder");
        let sibling = parent.join("Cloud Folder 2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        let count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&count);
        let state = Rc::new(State {
            hwnd: HWND::default(),
            messages: message_window().unwrap(),
            dir: dir.clone(),
            folder: Pidl(std::ptr::null_mut()),
            url: String::new(),
            name: String::new(),
            type_info: None,
            sinks: RefCell::default(),
            next_cookie: Cell::new(0),
            selected: RefCell::default(),
            changed: Box::new(move || counted.set(counted.get() + 1)),
            select: Box::new(|_| {}),
            running: Arc::default(),
            relist: Cell::default(),
            journal: RefCell::default(),
        });
        state.journal.borrow_mut().start();
        CURRENT.with(|c| {
            *c.borrow_mut() = Some(Current {
                state: Rc::clone(&state),
                browser: com::View {
                    state: Rc::clone(&state),
                }
                .into(),
            });
        });
        let set_attributes = |path: &Path, attributes: FILE_FLAGS_AND_ATTRIBUTES| {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated path for the call.
            unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), attributes) }.unwrap();
        };
        // Pumps this thread's messages until `done` or `wait` passed; returns whether `done`.
        let pump = |done: &dyn Fn() -> bool, wait: Duration| -> bool {
            let deadline = Instant::now() + wait;
            loop {
                let mut msg = MSG::default();
                // SAFETY: msg is a valid out pointer; this thread's own messages.
                while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    // SAFETY: msg came from PeekMessageW.
                    unsafe { DispatchMessageW(&raw const msg) };
                }
                if done() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let hinted = || count.get() > 0;

        // The pane's path in another case than on disk.
        let spelled = PathBuf::from(dir.to_string_lossy().to_uppercase());
        let watch = ParentWatch::start(state.messages, &spelled).expect("the parent is watched");
        set_attributes(&sibling, FILE_ATTRIBUTE_HIDDEN);
        assert!(
            !pump(&hinted, Duration::from_millis(500)),
            "a sibling's change doesn't hint"
        );
        set_attributes(&dir, FILE_ATTRIBUTE_HIDDEN);
        assert!(
            pump(&hinted, Duration::from_secs(10)),
            "the folder's own change hints"
        );
        let summary = state.journal.borrow().summary();
        assert!(summary.contains("hints: ParentEntryChanged x"), "{summary}");
        assert!(summary.contains("first parent change +"), "{summary}");

        // The watch doesn't hold the parent: it can be renamed (and back).
        let moved = tmp_dir.path().join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::rename(&moved, &parent).unwrap();

        drop(watch);
        let before = count.get();
        set_attributes(&dir, FILE_ATTRIBUTE_NORMAL);
        assert!(
            !pump(&|| count.get() > before, Duration::from_millis(500)),
            "nothing comes once the watch stopped"
        );
        // SAFETY: the window made above, on this thread.
        let _ = unsafe { DestroyWindow(state.messages) };
        CURRENT.with(|c| *c.borrow_mut() = None);
    }
}
