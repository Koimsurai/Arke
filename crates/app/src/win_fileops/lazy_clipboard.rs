//! Files on the clipboard with delayed rendering.
//!
//! Copy and cut used to build a shell data object, which parses every path
//! (`SHParseDisplayName`), and flush it so all formats were rendered at once. With thousands of
//! files that takes seconds. Now `CF_HDROP` and `Preferred DropEffect` are put on the clipboard
//! at once (cheap: one buffer of paths), and `Shell IDList Array` is promised and rendered only
//! when a program asks for it (`WM_RENDERFORMAT`), so a paste into Explorer, Arke or an editor,
//! which read `CF_HDROP`, never pays for it.
//!
//! The clipboard owner is a hidden window on a thread of its own that keeps pumping messages for
//! the life of the process, because Windows sends the render requests to it. Before Arke exits,
//! [`render_all_now`] renders what was not asked for yet, so the formats outlive the process.
//! Paths on a network share are rendered at once, as before: parsing them can block, and that
//! must not happen inside another program's paste.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    EmptyClipboard, GetClipboardOwner, GetClipboardSequenceNumber, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    PostMessageW, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use super::{
    Com, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, Global, OpenedClipboard,
    drop_effect_format, hdrop_bytes,
};
use crate::shell::{ClipboardFiles, ShellError};

/// A format this module puts on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    /// `CF_HDROP`: the paths.
    Hdrop,
    /// `Preferred DropEffect`: copy or move.
    DropEffect,
    /// `Shell IDList Array`: shell IDs, which take parsing every path.
    IdList,
    /// `FileNameW`: the path, only when there is exactly one (as Explorer offers it).
    FileNameW,
}

/// Builds the bytes of a Shell IDList Array for these paths; `None` if a path can't be parsed.
type IdListBuilder = Box<dyn Fn(&[PathBuf]) -> Option<Vec<u8>>>;

/// What goes on the clipboard for one copy or cut, and when each part is made.
pub(super) struct Payload {
    paths: Vec<PathBuf>,
    cut: bool,
    /// Render the ID list with the rest instead of when asked for.
    eager_ids: bool,
    build_ids: IdListBuilder,
    ids: Ids,
}

/// Whether the ID list was made yet.
enum Ids {
    NotMade,
    /// Made, or `None` if it could not be.
    Made(Option<Vec<u8>>),
}

impl Payload {
    pub(super) fn new(
        paths: Vec<PathBuf>,
        cut: bool,
        eager_ids: bool,
        build_ids: IdListBuilder,
    ) -> Self {
        Self {
            paths,
            cut,
            eager_ids,
            build_ids,
            ids: Ids::NotMade,
        }
    }

    /// The formats to put on the clipboard now, with their bytes. The ID list is among them only
    /// when it is eager.
    pub(super) fn immediate(&mut self) -> Vec<(Format, Vec<u8>)> {
        let mut formats = vec![
            (Format::Hdrop, hdrop_bytes(&self.paths)),
            (Format::DropEffect, drop_effect_bytes(self.cut)),
        ];
        if let [only] = self.paths.as_slice() {
            formats.push((Format::FileNameW, file_name_bytes(only)));
        }
        if self.eager_ids
            && let Some(ids) = self.render(Format::IdList)
        {
            formats.push((Format::IdList, ids));
        }
        formats
    }

    /// The formats promised and not made yet: asked for later through `WM_RENDERFORMAT`.
    pub(super) const fn lazy_formats(&self) -> &'static [Format] {
        if self.eager_ids {
            &[]
        } else {
            &[Format::IdList]
        }
    }

    /// The lazy formats nobody has asked for yet (what must still be rendered before exit).
    pub(super) fn pending(&self) -> Vec<Format> {
        self.lazy_formats()
            .iter()
            .copied()
            .filter(|f| *f != Format::IdList || matches!(self.ids, Ids::NotMade))
            .collect()
    }

    /// The bytes of a lazy format, made on the first call and kept.
    pub(super) fn render(&mut self, format: Format) -> Option<Vec<u8>> {
        match format {
            Format::Hdrop => Some(hdrop_bytes(&self.paths)),
            Format::DropEffect => Some(drop_effect_bytes(self.cut)),
            Format::FileNameW => self.paths.first().map(|p| file_name_bytes(p)),
            Format::IdList => {
                if matches!(self.ids, Ids::NotMade) {
                    self.ids = Ids::Made((self.build_ids)(&self.paths));
                }
                match &self.ids {
                    Ids::Made(ids) => ids.clone(),
                    Ids::NotMade => None,
                }
            }
        }
    }
}

/// `Preferred DropEffect`: Explorer uses copy and link for a copy, and move for a cut.
pub(super) fn drop_effect_bytes(cut: bool) -> Vec<u8> {
    let effect = if cut {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY | DROPEFFECT_LINK
    };
    effect.to_le_bytes().to_vec()
}

/// `FileNameW`: one NUL-terminated wide path.
pub(super) fn file_name_bytes(path: &std::path::Path) -> Vec<u8> {
    super::wide(path.as_os_str())
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .collect()
}

/// A Shell IDList Array (CIDA): the item count, then the offsets of the folder's ID list and each
/// item's from the start, then the lists themselves. IDs are given with their terminators.
pub(super) fn cida_bytes(folder: &[u8], items: &[Vec<u8>]) -> Vec<u8> {
    let header = 4 + 4 * (items.len() + 1);
    let mut bytes =
        Vec::with_capacity(header + folder.len() + items.iter().map(Vec::len).sum::<usize>());
    bytes.extend_from_slice(&(items.len() as u32).to_le_bytes());
    let mut offset = header;
    for list in std::iter::once(folder).chain(items.iter().map(Vec::as_slice)) {
        bytes.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += list.len();
    }
    bytes.extend_from_slice(folder);
    for item in items {
        bytes.extend_from_slice(item);
    }
    bytes
}

/// The Win32 clipboard format number of a format.
fn format_id(format: Format) -> u32 {
    match format {
        Format::Hdrop => u32::from(CF_HDROP.0),
        Format::DropEffect => drop_effect_format(),
        // SAFETY: the argument is a static wide string.
        Format::FileNameW => unsafe { RegisterClipboardFormatW(w!("FileNameW")) },
        // SAFETY: the argument is a static wide string.
        Format::IdList => unsafe { RegisterClipboardFormatW(w!("Shell IDList Array")) },
    }
}

/// Shell IDs for the paths as a CIDA, the way Explorer's folder hands out its items: the folder
/// plus one-level child IDs when all items share a folder, else the desktop plus full IDs. The
/// same shape as [`super::drag_data_object`] gives a drag.
fn ids_for_paths(paths: &[PathBuf]) -> Option<Vec<u8>> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        ILFindLastID, ILGetSize, IPersistFolder2, SHBindToParent, SHParseDisplayName,
    };
    struct Pidls(Vec<*mut ITEMIDLIST>);
    impl Drop for Pidls {
        fn drop(&mut self) {
            for pidl in &self.0 {
                // SAFETY: allocated with CoTaskMemAlloc (SHParseDisplayName, GetCurFolder);
                // freed once each.
                unsafe { CoTaskMemFree(Some(pidl.cast_const().cast())) };
            }
        }
    }
    let bytes_of = |pidl: *const ITEMIDLIST| {
        // SAFETY: pidl is a valid, zero-terminated list that stays alive (Pidls); ILGetSize
        // gives its length with the terminator.
        unsafe {
            let size = ILGetSize(Some(pidl)) as usize;
            std::slice::from_raw_parts(pidl.cast::<u8>(), size).to_vec()
        }
    };
    if paths.is_empty() {
        return None;
    }
    let mut pidls = Pidls(Vec::with_capacity(paths.len() + 1));
    for path in paths {
        let name = super::wide(path.as_os_str());
        let mut pidl = std::ptr::null_mut();
        // SAFETY: name is NUL-terminated; pidl is an out variable, owned by Pidls on success.
        unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut pidl, 0, None) }.ok()?;
        pidls.0.push(pidl);
    }
    let folder = super::common_parent(paths).and_then(|_| {
        // SAFETY: the first item's PIDL is valid; the folder's copy of its own ID is
        // CoTaskMemAlloc'ed and freed with the others (Pidls).
        let pidl = unsafe {
            SHBindToParent::<IPersistFolder2>(pidls.0[0].cast_const(), None)
                .and_then(|folder| folder.GetCurFolder())
        }
        .ok()?;
        pidls.0.push(pidl);
        Some(pidl)
    });
    Some(if let Some(folder) = folder {
        let items: Vec<Vec<u8>> = pidls.0[..paths.len()]
                .iter()
                // SAFETY: the item's PIDL is valid; its last ID points into it.
                .map(|p| bytes_of(unsafe { ILFindLastID(p.cast_const()) }))
                .collect();
        cida_bytes(&bytes_of(folder.cast_const()), &items)
    } else {
        // The desktop (just a terminator) as the folder, so full IDs are its children.
        let items: Vec<Vec<u8>> = pidls.0.iter().map(|p| bytes_of(p.cast_const())).collect();
        cida_bytes(&[0, 0], &items)
    })
}

// ───────────────────────── The clipboard owner ─────────────────────────

/// Work for the owner thread.
enum Request {
    /// Put these files on the clipboard.
    Set {
        paths: Vec<PathBuf>,
        cut: bool,
        reply: flume::Sender<Result<u32, ShellError>>,
    },
    /// Render every promised format that was not asked for yet.
    RenderAll { reply: flume::Sender<()> },
    /// Run this here: remote items' data object lives on this thread (`remote_clip`).
    Run(Box<dyn FnOnce() + Send>),
}

/// The owner thread, as the other threads see it.
struct Owner {
    /// The owner window (an address, to cross threads).
    hwnd: usize,
    requests: flume::Sender<Request>,
}

impl Owner {
    const fn window(&self) -> HWND {
        HWND(std::ptr::with_exposed_provenance_mut(self.hwnd))
    }

    fn send(&self, request: Request) -> Result<(), ShellError> {
        if let Err(err) = self.requests.send(request) {
            log::info!("clipboard owner thread is gone: {err}");
            return Err(ShellError::ClipboardBusy);
        }
        // SAFETY: posts a message to a window of the owner thread; failure means it is gone.
        unsafe { PostMessageW(Some(self.window()), WM_APP, WPARAM(0), LPARAM(0)) }.map_err(|err| {
            log::info!("clipboard owner window is gone: {err}");
            ShellError::ClipboardBusy
        })
    }
}

static OWNER: OnceLock<Option<Owner>> = OnceLock::new();

fn owner() -> Option<&'static Owner> {
    OWNER
        .get_or_init(|| {
            let (ready_tx, ready_rx) = flume::bounded(1);
            std::thread::Builder::new()
                .name("clipboard-owner".into())
                .spawn(move || owner_main(&ready_tx))
                .ok()?;
            ready_rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
        })
        .as_ref()
}

thread_local! {
    /// What this thread's clipboard window owns, while it owns the clipboard.
    static STATE: RefCell<Option<Rc<RefCell<Payload>>>> = const { RefCell::new(None) };
    /// The owner thread's work queue.
    static QUEUE: RefCell<Option<flume::Receiver<Request>>> = const { RefCell::new(None) };
}

fn current() -> Option<Rc<RefCell<Payload>>> {
    STATE.with(|s| s.borrow().clone())
}

/// The owner thread: COM, the window, and messages until the process ends.
fn owner_main(ready: &flume::Sender<Option<Owner>>) {
    let Ok(_com) = Com::init() else {
        let _ = ready.send(None);
        return;
    };
    let Some(hwnd) = create_window() else {
        let _ = ready.send(None);
        return;
    };
    let (requests, queue) = flume::unbounded();
    QUEUE.with(|q| *q.borrow_mut() = Some(queue));
    let _ = ready.send(Some(Owner {
        hwnd: hwnd.0.expose_provenance(),
        requests,
    }));
    let mut msg = MSG::default();
    // SAFETY: the standard message loop of the thread that owns the window; GetMessageW is
    // positive until WM_QUIT (never posted) and -1 on error, which also ends it.
    unsafe {
        while GetMessageW(&raw mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

fn create_window() -> Option<HWND> {
    // SAFETY: registers a window class with a window procedure of this module and creates a
    // message-only window of it on this thread.
    unsafe {
        let instance = HINSTANCE(GetModuleHandleW(None).ok()?.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: w!("ArkeClipboardOwner"),
            ..Default::default()
        };
        // 0 if it exists already, which is fine: the window is created either way.
        RegisterClassW(&raw const class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ArkeClipboardOwner"),
            w!("arke-clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_RENDERFORMAT => {
            on_render_format(wparam.0 as u32);
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            render_pending(hwnd);
            LRESULT(0)
        }
        WM_DESTROYCLIPBOARD => {
            STATE.with(|s| *s.borrow_mut() = None);
            LRESULT(0)
        }
        WM_APP => {
            drain(hwnd);
            LRESULT(0)
        }
        // SAFETY: the default handling of a message this window doesn't take.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn drain(hwnd: HWND) {
    let Some(queue) = QUEUE.with(|q| q.borrow().clone()) else {
        return;
    };
    while let Ok(request) = queue.try_recv() {
        match request {
            Request::Set { paths, cut, reply } => {
                let _ = reply.send(set_on_owner(hwnd, paths, cut));
            }
            Request::RenderAll { reply } => {
                render_pending(hwnd);
                let _ = reply.send(());
            }
            Request::Run(job) => job(),
        }
    }
}

/// Whether a `SetClipboardData` promise (a null handle) worked: it returns NULL by design, so
/// only a NULL together with a nonzero last error is a failure.
const fn promise_failed(returned_null: bool, last_error: u32) -> Option<u32> {
    if returned_null && last_error != 0 {
        Some(last_error)
    } else {
        None
    }
}

unsafe extern "system" {
    /// The raw call: the `windows` wrapper turns the NULL a promise returns into an error.
    #[link_name = "SetClipboardData"]
    fn set_clipboard_data_raw(format: u32, mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

/// Promises `format` (delayed rendering): the data is asked for with `WM_RENDERFORMAT`.
fn promise(format: u32) -> Result<(), ShellError> {
    use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
    // SAFETY: the clipboard is open for this window; a null handle is the promise. The last
    // error is cleared first so a NULL return can be told from a failure.
    let failed = unsafe {
        SetLastError(WIN32_ERROR(0));
        let returned = set_clipboard_data_raw(format, std::ptr::null_mut());
        promise_failed(returned.is_null(), GetLastError().0)
    };
    match failed {
        None => Ok(()),
        Some(code) => {
            Err(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)).into())
        }
    }
}

/// Hands `bytes` to the clipboard (open, or inside `WM_RENDERFORMAT`) as `format`.
fn put(format: Format, bytes: &[u8]) -> Result<(), ShellError> {
    let mut mem = Global::new(bytes)?;
    // SAFETY: the clipboard is open (or being rendered into); after SetClipboardData succeeds
    // the system owns the memory, hence release.
    unsafe { SetClipboardData(format_id(format), mem.handle()) }?;
    mem.release();
    Ok(())
}

fn set_on_owner(hwnd: HWND, paths: Vec<PathBuf>, cut: bool) -> Result<u32, ShellError> {
    if paths.is_empty() {
        return Err(ShellError::ClipboardEmpty);
    }
    let eager = paths
        .iter()
        .any(|p| crate::global_search::is_network_path(p));
    let mut payload = Payload::new(paths, cut, eager, Box::new(ids_for_paths));
    // Made before the clipboard is opened, so a slow one doesn't hold it.
    let immediate = payload.immediate();
    let lazy = payload.lazy_formats();
    let shared = Rc::new(RefCell::new(payload));
    let result = (|| -> Result<(), ShellError> {
        let _clipboard = OpenedClipboard::open(Some(hwnd))?;
        // SAFETY: the clipboard is open for this window. Emptying it tells the previous
        // owner (maybe this window: WM_DESTROYCLIPBOARD), so the state is set after it.
        unsafe { EmptyClipboard() }?;
        STATE.with(|s| *s.borrow_mut() = Some(Rc::clone(&shared)));
        for (format, bytes) in &immediate {
            put(*format, bytes)?;
        }
        for format in lazy {
            promise(format_id(*format))?;
        }
        Ok(())
    })();
    if result.is_err() {
        STATE.with(|s| *s.borrow_mut() = None);
    }
    result?;
    // SAFETY: only reads the sequence number.
    Ok(unsafe { GetClipboardSequenceNumber() })
}

/// A program asked for a promised format; the clipboard is already open for it.
fn on_render_format(cf: u32) {
    let Some(payload) = current() else {
        return;
    };
    // Parsing can pump messages; a request that arrives meanwhile gets nothing.
    let Ok(mut payload) = payload.try_borrow_mut() else {
        return;
    };
    for format in payload.lazy_formats() {
        if format_id(*format) == cf
            && let Some(bytes) = payload.render(*format)
        {
            let _ = put(*format, &bytes);
        }
    }
}

/// Renders the promised formats nobody asked for, while this window still owns the clipboard
/// (`WM_RENDERALLFORMATS`, or before exit).
fn render_pending(hwnd: HWND) {
    let Some(payload) = current() else {
        return;
    };
    let Ok(mut payload) = payload.try_borrow_mut() else {
        return;
    };
    let pending = payload.pending();
    if pending.is_empty() {
        return;
    }
    let Ok(_clipboard) = OpenedClipboard::open(Some(hwnd)) else {
        return;
    };
    // SAFETY: only asks who owns the open clipboard.
    if unsafe { GetClipboardOwner() }.ok() != Some(hwnd) {
        return;
    }
    for format in pending {
        if let Some(bytes) = payload.render(format) {
            let _ = put(format, &bytes);
        }
    }
}

/// Puts the files on the clipboard: paths and drop effect now, shell IDs when asked for.
/// Background thread.
pub(super) fn set(files: &ClipboardFiles) -> Result<u32, ShellError> {
    let owner = owner().ok_or(ShellError::ClipboardBusy)?;
    let (reply, answer) = flume::bounded(1);
    owner.send(Request::Set {
        paths: files.paths.clone(),
        cut: files.cut,
        reply,
    })?;
    answer.recv().map_err(|err| {
        log::info!("clipboard owner did not answer: {err}");
        ShellError::ClipboardBusy
    })?
}

/// Runs `job` on the owner thread, which pumps messages for the life of the process, and waits
/// for its answer, `wait` at most. `start`: start the thread if it isn't running yet (else this
/// fails). Background thread.
pub(super) fn on_owner<T: Send + 'static>(
    start: bool,
    wait: Duration,
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ShellError> {
    let owner = if start {
        owner()
    } else {
        OWNER.get().and_then(Option::as_ref)
    }
    .ok_or(ShellError::ClipboardBusy)?;
    let (reply, answer) = flume::bounded(1);
    owner.send(Request::Run(Box::new(move || {
        let _ = reply.send(job());
    })))?;
    answer.recv_timeout(wait).map_err(|err| {
        log::info!("clipboard owner did not answer: {err}");
        ShellError::ClipboardBusy
    })
}

/// Before Arke exits: renders what no program asked for yet, so every format outlives the process.
/// Waits at most a few seconds; does nothing if Arke never put files on the clipboard.
pub(super) fn render_all_now() {
    let Some(Some(owner)) = OWNER.get() else {
        return;
    };
    let (reply, answer) = flume::bounded(1);
    if owner.send(Request::RenderAll { reply }).is_ok() {
        let _ = answer.recv_timeout(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn paths() -> Vec<PathBuf> {
        vec![
            PathBuf::from(r"C:\a\x.txt"),
            PathBuf::from(r"C:\a\y 報告.txt"),
        ]
    }

    /// A payload whose ID list builder counts its calls.
    fn counted(eager: bool, ok: bool) -> (Payload, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        let seen = Rc::clone(&calls);
        let build: IdListBuilder = Box::new(move |_| {
            seen.set(seen.get() + 1);
            ok.then(|| vec![1, 2, 3])
        });
        (Payload::new(paths(), false, eager, build), calls)
    }

    #[test]
    fn paths_and_effect_go_on_at_once_and_ids_are_promised() {
        let (mut payload, calls) = counted(false, true);
        let immediate = payload.immediate();
        let formats: Vec<Format> = immediate.iter().map(|(f, _)| *f).collect();
        assert_eq!(formats, [Format::Hdrop, Format::DropEffect]);
        assert_eq!(payload.lazy_formats(), [Format::IdList]);
        assert_eq!(immediate[0].1, hdrop_bytes(&paths()));
        assert_eq!(calls.get(), 0, "nothing was parsed to put the files on");
    }

    #[test]
    fn the_ids_are_made_when_first_asked_for_and_only_once() {
        let (mut payload, calls) = counted(false, true);
        assert_eq!(payload.pending(), [Format::IdList]);
        assert_eq!(payload.render(Format::IdList), Some(vec![1, 2, 3]));
        assert_eq!(calls.get(), 1);
        assert_eq!(payload.render(Format::IdList), Some(vec![1, 2, 3]));
        assert_eq!(calls.get(), 1, "a second paste reuses them");
        assert!(
            payload.pending().is_empty(),
            "nothing is left to render at exit"
        );
    }

    #[test]
    fn what_was_never_asked_for_is_still_pending_at_exit() {
        let (mut payload, calls) = counted(false, true);
        let _ = payload.immediate();
        assert_eq!(payload.pending(), [Format::IdList]);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn eager_ids_are_made_with_the_rest_and_not_promised() {
        let (mut payload, calls) = counted(true, true);
        let formats: Vec<Format> = payload.immediate().iter().map(|(f, _)| *f).collect();
        assert_eq!(formats, [Format::Hdrop, Format::DropEffect, Format::IdList]);
        assert!(payload.lazy_formats().is_empty());
        assert!(payload.pending().is_empty());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn ids_that_cannot_be_made_leave_the_other_formats_alone() {
        let (mut payload, calls) = counted(true, false);
        let formats: Vec<Format> = payload.immediate().iter().map(|(f, _)| *f).collect();
        assert_eq!(formats, [Format::Hdrop, Format::DropEffect]);
        assert_eq!(payload.render(Format::IdList), None);
        assert_eq!(calls.get(), 1, "a failure isn't retried on every paste");
    }

    #[test]
    fn a_single_path_also_offers_its_file_name_at_once() {
        let build: IdListBuilder = Box::new(|_| None);
        let one = PathBuf::from("x.txt");
        let mut payload = Payload::new(vec![one.clone()], false, false, build);
        let immediate = payload.immediate();
        let formats: Vec<Format> = immediate.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            formats,
            [Format::Hdrop, Format::DropEffect, Format::FileNameW]
        );
        assert_eq!(immediate[2].1, file_name_bytes(&one));
        assert_eq!(&immediate[2].1[immediate[2].1.len() - 2..], [0, 0]);
    }

    #[test]
    fn a_null_from_a_promise_is_success_unless_an_error_is_set() {
        assert_eq!(promise_failed(true, 0), None);
        assert_eq!(promise_failed(false, 0), None);
        assert_eq!(promise_failed(false, 5), None, "a handle came back");
        assert_eq!(promise_failed(true, 5), Some(5));
    }

    #[test]
    fn the_drop_effect_follows_cut() {
        assert_eq!(drop_effect_bytes(true), DROPEFFECT_MOVE.to_le_bytes());
        assert_eq!(
            drop_effect_bytes(false),
            (DROPEFFECT_COPY | DROPEFFECT_LINK).to_le_bytes()
        );
        let mut payload = Payload::new(paths(), true, false, Box::new(|_| None));
        assert_eq!(payload.immediate()[1].1, drop_effect_bytes(true));
    }

    #[test]
    fn a_cida_lists_offsets_then_the_folder_then_the_items() {
        let folder = vec![4, 0, 7, 7, 0, 0];
        let items = vec![vec![3, 0, 1, 0, 0], vec![3, 0, 2, 0, 0]];
        let bytes = cida_bytes(&folder, &items);
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(word(0), 2, "count");
        let header = 4 + 4 * 3;
        assert_eq!(word(4), header, "folder");
        assert_eq!(word(8), header + folder.len(), "first item");
        assert_eq!(
            word(12),
            header + folder.len() + items[0].len(),
            "second item"
        );
        assert_eq!(&bytes[word(4)..word(8)], folder);
        assert_eq!(&bytes[word(12)..], items[1]);
    }

    /// The real builder against the shell: a folder and one-level IDs, like a drag's.
    #[test]
    fn the_real_ids_hold_the_folder_and_each_item() {
        let dir = crate::win::long_temp_dir().join(format!("arke-lazy-ids-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b 報告.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let _com = Com::init().unwrap();
        let bytes = ids_for_paths(&[a, b]).unwrap();
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(word(0), 2);
        assert!(word(4) < word(8) && word(8) < word(12));
        assert!((word(12) as usize) < bytes.len());
        assert_eq!(ids_for_paths(&[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
