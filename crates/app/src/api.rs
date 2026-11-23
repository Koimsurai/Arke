//! The integration API: one JSON interface for launchers and scripts (ALTRun, Listary, AutoHotkey,
//! PowerShell) to read what Arke shows and ask it to open a folder or run a command. The format is
//! documented in `docs/integration.md`; Arke has no code for any single tool.
//!
//! Two channels carry the same JSON:
//! - `WM_COPYDATA` sent to an `ArkeWindow` (`gpui_windows::set_copydata_handler`). It is answered
//!   inside the window procedure, where GPUI may be in the middle of anything.
//! - `arke.exe --api <json>`, through the single instance pipe (`instance.rs`), answered on the
//!   pipe thread.
//!
//! Neither may touch GPUI, so both answer from a snapshot each window keeps fresh as it renders
//! ([`publish`]), and `open` and `run` are checked as far as they can be there and then queued
//! for the UI thread ([`Action`]), with `{"v":1,"ok":true}` as the reply at once.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::explorer::Explorer;
use crate::explorer::scope::Scope;

/// `arke.exe --api <json>`.
pub(crate) const ARG: &str = "--api";

/// The request given as `arke.exe --api <json>`: the parsed argument `arg`, or, when that isn't
/// JSON, what follows `--api` on the raw command line `raw` (`GetCommandLineW`).
pub(crate) fn command_line_request(arg: &str, raw: &str) -> String {
    if serde_json::from_str::<serde_json::Value>(arg).is_ok() {
        return arg.to_owned();
    }
    // Windows PowerShell 5.1 passes the JSON without escaping its quotes, and wraps it in one more
    // pair when it holds a space: the parsed argument lost them, the raw command line still has
    // the request as written.
    let Some(at) = raw
        .match_indices(ARG)
        .find(|&(i, _)| i == 0 || raw[..i].ends_with([' ', '\t', '"']))
        .map(|(i, _)| i + ARG.len())
    else {
        return arg.to_owned();
    };
    let rest = raw[at..].trim();
    let unwrapped = rest
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .filter(|inner| inner.trim_start().starts_with('{'))
        .unwrap_or(rest);
    if serde_json::from_str::<serde_json::Value>(unwrapped).is_ok() {
        unwrapped.to_owned()
    } else {
        arg.to_owned()
    }
}

/// A request piped to `arke.exe --api -` as text: UTF-8 with or without a byte order mark, or
/// UTF-16 with one (PowerShell pipes what its `$OutputEncoding` says, which varies).
pub(crate) fn stdin_text(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8], little: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| {
                if little {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The format version requests name and replies carry.
pub(crate) const VERSION: u64 = 1;

/// How long checking that an `open` path exists may take (a share that stopped answering).
/// Past that the path is taken as it is, and opening it shows what went wrong.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The class of Arke's normal windows (`third_party/gpui-pre-windows`).
const WINDOW_CLASS: &str = "ArkeWindow";

// ─── Requests

/// A parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    State,
    Commands,
    Open {
        path: PathBuf,
        new_tab: bool,
        /// Names of items in that folder to select once it is listed
        select: Vec<String>,
    },
    Run {
        id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    /// Not valid JSON, not an object, or a field missing or of the wrong type
    BadRequest,
    /// An unknown `cmd`, or a `run` id no command has
    UnknownCommand,
    /// `v` is not 1
    UnsupportedVersion,
    /// `run` of a command that changes files
    NotAllowed,
    /// `open` of a path that doesn't exist
    NotFound,
    /// `arke.exe --api` only: Arke is running but didn't answer in time
    Unavailable,
}

impl ErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::UnknownCommand => "unknown_command",
            Self::UnsupportedVersion => "unsupported_version",
            Self::NotAllowed => "not_allowed",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

impl ApiError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::BadRequest, message)
}

/// Reads a request. Fields it doesn't know are ignored, so tools can send newer requests.
pub(crate) fn parse(bytes: &[u8]) -> Result<Request, ApiError> {
    // `WM_COPYDATA` senders often count the string's terminating NUL.
    let end = bytes
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |last| last + 1);
    let bytes = &bytes[..end];
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| bad(format!("not valid JSON: {err}")))?;
    let Some(object) = value.as_object() else {
        return Err(bad("the request is not a JSON object"));
    };
    match object.get("v") {
        None => return Err(bad("\"v\" is missing")),
        Some(v) if v.as_u64() == Some(VERSION) => {}
        Some(v) if v.is_number() => {
            return Err(ApiError::new(
                ErrorCode::UnsupportedVersion,
                format!("version {v} is not supported, only {VERSION}"),
            ));
        }
        Some(_) => return Err(bad("\"v\" is not a number")),
    }
    let cmd = match object.get("cmd") {
        None => return Err(bad("\"cmd\" is missing")),
        Some(Value::String(cmd)) => cmd.as_str(),
        Some(_) => return Err(bad("\"cmd\" is not a string")),
    };
    match cmd {
        "state" => Ok(Request::State),
        "commands" => Ok(Request::Commands),
        "open" => parse_open(object),
        "run" => match object.get("id") {
            Some(Value::String(id)) => Ok(Request::Run { id: id.clone() }),
            None => Err(bad("\"id\" is missing")),
            Some(_) => Err(bad("\"id\" is not a string")),
        },
        other => Err(ApiError::new(
            ErrorCode::UnknownCommand,
            format!("unknown cmd \"{other}\""),
        )),
    }
}

fn parse_open(object: &Map<String, Value>) -> Result<Request, ApiError> {
    let path = match object.get("path") {
        Some(Value::String(path)) if !path.trim().is_empty() => PathBuf::from(path),
        Some(Value::String(_)) => return Err(bad("\"path\" is empty")),
        None => return Err(bad("\"path\" is missing")),
        Some(_) => return Err(bad("\"path\" is not a string")),
    };
    let new_tab = match object.get("new_tab") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(new_tab)) => *new_tab,
        Some(_) => return Err(bad("\"new_tab\" is not true or false")),
    };
    let select = match object.get("select") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                let Value::String(item) = item else {
                    return Err(bad("\"select\" holds something other than names"));
                };
                // A full path stands for its name.
                if let Some(name) = Path::new(item).file_name() {
                    names.push(name.to_string_lossy().into_owned());
                }
            }
            names
        }
        Some(_) => return Err(bad("\"select\" is not an array of names")),
    };
    Ok(Request::Open {
        path,
        new_tab,
        select,
    })
}

/// A request as JSON again, to pass on to the resident Arke.
pub(crate) fn encode_request(request: &Request) -> String {
    let value = match request {
        Request::State => json!({ "v": VERSION, "cmd": "state" }),
        Request::Commands => json!({ "v": VERSION, "cmd": "commands" }),
        Request::Open {
            path,
            new_tab,
            select,
        } => json!({
            "v": VERSION,
            "cmd": "open",
            "path": path.to_string_lossy(),
            "new_tab": new_tab,
            "select": select,
        }),
        Request::Run { id } => json!({ "v": VERSION, "cmd": "run", "id": id }),
    };
    value.to_string()
}

/// A request from the command line with its relative path made absolute: it means the folder the
/// command runs in, not the resident Arke's.
pub(crate) fn absolute(request: Request) -> Request {
    match request {
        Request::Open {
            path,
            new_tab,
            select,
        } if !path.is_absolute() && !arke_core::remote::is_remote(&path) => Request::Open {
            path: std::path::absolute(&path).unwrap_or(path),
            new_tab,
            select,
        },
        request => request,
    }
}

// ─── Replies

fn reply_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

pub(crate) fn error_reply(error: &ApiError) -> Vec<u8> {
    reply_bytes(&json!({
        "v": VERSION,
        "error": { "code": error.code.as_str(), "message": error.message },
    }))
}

fn ok_json() -> Value {
    json!({ "v": VERSION, "ok": true })
}

/// Whether a reply is an error reply (`arke.exe --api` exits with 1 then).
pub(crate) fn is_error(reply: &[u8]) -> bool {
    serde_json::from_slice::<Value>(reply)
        .ok()
        .is_none_or(|value| value.get("error").is_some())
}

/// The commands `run` takes, with their names in the current language.
fn commands_json() -> Value {
    let commands: Vec<Value> = crate::commands::runnable()
        .map(|c| json!({ "id": c.id, "name": c.label() }))
        .collect();
    json!({ "v": VERSION, "commands": commands })
}

fn hwnd_text(hwnd: isize) -> String {
    format!("0x{:X}", hwnd as usize)
}

fn location_json(location: &Location) -> Value {
    match location {
        Location::Folder(path) => json!({ "kind": "folder", "path": path.to_string_lossy() }),
        Location::Remote(url) => json!({ "kind": "remote", "url": url }),
        Location::Search { path, query } => {
            let mut search = json!({ "kind": "search", "query": query });
            if let (Some(path), Some(object)) = (path, search.as_object_mut()) {
                object.insert("path".into(), Value::from(path.to_string_lossy()));
            }
            search
        }
        Location::Home => json!({ "kind": "home" }),
        Location::RecycleBin => json!({ "kind": "recycle_bin" }),
        Location::Other => json!({ "kind": "other" }),
    }
}

/// The `state` reply: windows front to back, the first being the one used last.
pub(crate) fn state_json(snapshot: &Snapshot) -> Value {
    let windows: Vec<Value> = snapshot
        .windows
        .iter()
        .enumerate()
        .map(|(w, window)| {
            let tabs: Vec<Value> = window
                .tabs
                .iter()
                .enumerate()
                .map(|(t, tab)| {
                    let panes: Vec<Value> = tab
                        .panes
                        .iter()
                        .enumerate()
                        .map(|(p, pane)| {
                            json!({
                                "active": p == tab.active_pane,
                                "location": location_json(&pane.location),
                                "selection": pane.selection,
                            })
                        })
                        .collect();
                    json!({ "active": t == window.active_tab, "panes": panes })
                })
                .collect();
            json!({ "hwnd": hwnd_text(window.hwnd), "active": w == 0, "tabs": tabs })
        })
        .collect();
    json!({ "v": VERSION, "windows": windows })
}

// ─── Answering

/// Which window an `open` or `run` is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The window a `WM_COPYDATA` was sent to (its HWND); when that isn't a workspace window (a
    /// Properties window, a menu), the one used last
    Window(isize),
    /// The window used last (the command line)
    LastUsed,
}

/// Work for the UI thread, checked as far as it could be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Open {
        target: Target,
        /// A folder (an `open` of a file opens its folder with the file selected)
        path: PathBuf,
        new_tab: bool,
        select: Vec<String>,
    },
    Run {
        target: Target,
        id: &'static str,
    },
}

/// What a path is, as far as could be told in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    Folder,
    File,
    Missing,
    /// Not told in time, not readable, or a remote location: opening it tells
    Unknown,
}

/// What answering needs from the outside, so tests can stand in for it.
pub(crate) trait Env {
    fn snapshot(&self) -> Snapshot;
    fn probe(&self, path: &Path) -> Probe;
}

/// A reply and the work it promised.
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) reply: Vec<u8>,
    pub(crate) action: Option<Action>,
}

/// Answers one request without touching GPUI.
pub(crate) fn answer(request: &[u8], target: Target, env: &impl Env) -> Answer {
    match parse(request).and_then(|request| respond(request, target, env)) {
        Ok((value, action)) => Answer {
            reply: reply_bytes(&value),
            action,
        },
        Err(error) => Answer {
            reply: error_reply(&error),
            action: None,
        },
    }
}

fn respond(
    request: Request,
    target: Target,
    env: &impl Env,
) -> Result<(Value, Option<Action>), ApiError> {
    match request {
        Request::State => Ok((state_json(&env.snapshot()), None)),
        Request::Commands => Ok((commands_json(), None)),
        Request::Open {
            path,
            new_tab,
            mut select,
        } => {
            if !path.is_absolute() && !arke_core::remote::is_remote(&path) {
                return Err(bad("\"path\" is not a full path"));
            }
            let path = match env.probe(&path) {
                Probe::Missing => {
                    return Err(ApiError::new(
                        ErrorCode::NotFound,
                        format!("{} does not exist", path.display()),
                    ));
                }
                // A file: its folder, with it selected.
                Probe::File => {
                    let parts = path.parent().map(Path::to_path_buf).zip(
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned()),
                    );
                    match parts {
                        Some((folder, name)) => {
                            select.insert(0, name);
                            folder
                        }
                        None => path,
                    }
                }
                Probe::Folder | Probe::Unknown => path,
            };
            Ok((
                ok_json(),
                Some(Action::Open {
                    target,
                    path,
                    new_tab,
                    select,
                }),
            ))
        }
        Request::Run { id } => {
            let Some(command) = crate::commands::find(&id) else {
                return Err(ApiError::new(
                    ErrorCode::UnknownCommand,
                    format!("no command has the id \"{id}\""),
                ));
            };
            if command.changes_files {
                return Err(ApiError::new(
                    ErrorCode::NotAllowed,
                    format!("\"{id}\" changes files, which the API doesn't do"),
                ));
            }
            Ok((
                ok_json(),
                Some(Action::Run {
                    target,
                    id: command.id,
                }),
            ))
        }
    }
}

/// The live state: the windows this process published, in Z-order.
struct Live;

impl Env for Live {
    fn snapshot(&self) -> Snapshot {
        snapshot(Some(z_order().as_slice()))
    }

    fn probe(&self, path: &Path) -> Probe {
        probe(path)
    }
}

/// No Arke is running (`arke.exe --api` answering by itself): no windows.
pub(crate) struct NotRunning;

impl Env for NotRunning {
    fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }

    fn probe(&self, path: &Path) -> Probe {
        probe(path)
    }
}

/// Answers a request from either channel and queues its work for the UI thread.
pub(crate) fn serve(request: &[u8], target: Target, queue: &flume::Sender<Action>) -> Vec<u8> {
    let answer = answer(request, target, &Live);
    if let Some(action) = answer.action
        && queue.send(action).is_err()
    {
        return error_reply(&ApiError::new(ErrorCode::Unavailable, "Arke is quitting"));
    }
    answer.reply
}

/// What `path` is. Checked on a helper thread so a share that stopped answering can't hold the
/// caller (the window procedure) longer than [`PROBE_TIMEOUT`].
pub(crate) fn probe(path: &Path) -> Probe {
    if arke_core::remote::is_remote(path) {
        return Probe::Unknown;
    }
    let (tx, rx) = flume::bounded(1);
    let owned = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("api-probe".into())
        .spawn(move || {
            let found = match std::fs::metadata(&owned) {
                Ok(meta) if meta.is_dir() => Probe::Folder,
                Ok(_) => Probe::File,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Probe::Missing,
                Err(_) => Probe::Unknown,
            };
            let _ = tx.send(found);
        });
    if spawned.is_err() {
        return Probe::Unknown;
    }
    rx.recv_timeout(PROBE_TIMEOUT).unwrap_or(Probe::Unknown)
}

// ─── Snapshot

/// What a pane shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Location {
    Folder(PathBuf),
    /// SFTP or WebDAV
    Remote(String),
    Search {
        /// What is searched; `None` for the whole PC
        path: Option<PathBuf>,
        query: String,
    },
    Home,
    RecycleBin,
    /// Settings, What's new and other pages
    Other,
}

/// A page over a pane (`workspace_pages.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Home,
    RecycleBin,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneState {
    pub(crate) location: Location,
    /// Full paths (URLs on a server)
    pub(crate) selection: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabState {
    pub(crate) panes: Vec<PaneState>,
    pub(crate) active_pane: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowState {
    pub(crate) hwnd: isize,
    pub(crate) tabs: Vec<TabState>,
    pub(crate) active_tab: usize,
}

/// The windows a reply describes.
#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    pub(crate) windows: Vec<Arc<WindowState>>,
}

/// A pane as a window renders it, for [`publish`].
pub(crate) struct PaneInput<'a> {
    pub(crate) explorer: &'a Explorer,
    pub(crate) page: Option<Page>,
}

pub(crate) struct TabInput<'a> {
    pub(crate) panes: Vec<PaneInput<'a>>,
    pub(crate) active_pane: usize,
}

/// What a pane's state is made from, cheap to take every frame: when it is the same as last
/// time, the selection isn't listed again.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneKey {
    location: Location,
    selected: usize,
    cursor: Option<Arc<str>>,
    /// The listing (its order is the selection's order): its generation and version, which every
    /// change bumps (the list itself can change in place, at the same address).
    entries: (u64, u64),
    /// Which names are selected, for selections up to [`HASHED_MAX`] items
    names: usize,
    search: Option<(usize, Option<usize>)>,
}

/// Up to this many selected items, which ones they are is part of [`PaneKey`]; past it only
/// how many (and the cursor), so a frame never walks a huge selection.
const HASHED_MAX: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowKey {
    hwnd: isize,
    active_tab: usize,
    tabs: Vec<(usize, Vec<PaneKey>)>,
}

struct Published {
    key: WindowKey,
    state: Arc<WindowState>,
}

/// Each workspace window's state, by GPUI window id.
static WINDOWS: Mutex<BTreeMap<u64, Published>> = Mutex::new(BTreeMap::new());

/// The pane is showing search results.
fn searching(explorer: &Explorer) -> bool {
    explorer.search_mode && !explorer.query.trim().is_empty()
}

fn location_of(pane: &PaneInput<'_>) -> Location {
    match pane.page {
        Some(Page::Home) => return Location::Home,
        Some(Page::RecycleBin) => return Location::RecycleBin,
        Some(Page::Other) => return Location::Other,
        None => {}
    }
    let explorer = pane.explorer;
    if searching(explorer) {
        let path = match explorer.scope() {
            Scope::Computer => None,
            Scope::Here | Scope::Subfolders => explorer
                .search_results
                .as_ref()
                .map(|r| r.root.clone())
                .or_else(|| explorer.display_path().map(Path::to_path_buf)),
        };
        return Location::Search {
            path,
            query: explorer.query.clone(),
        };
    }
    match explorer.display_path() {
        Some(path) if arke_core::remote::is_remote(path) => Location::Remote(remote_text(path)),
        Some(path) => Location::Folder(path.to_path_buf()),
        None => Location::Other,
    }
}

/// A server location as its URL.
fn remote_text(path: &Path) -> String {
    arke_core::remote::RemoteUrl::from_path(path).map_or_else(
        || path.to_string_lossy().into_owned(),
        |url| url.to_string(),
    )
}

fn path_text(path: &Path) -> String {
    if arke_core::remote::is_remote(path) {
        remote_text(path)
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn pane_key(pane: &PaneInput<'_>) -> PaneKey {
    let explorer = pane.explorer;
    let selection = &explorer.selection;
    let names = if selection.len() <= HASHED_MAX {
        selection.names().fold(0usize, |hash, name| {
            hash ^ (Arc::as_ptr(name).cast::<u8>() as usize)
        })
    } else {
        0
    };
    PaneKey {
        location: location_of(pane),
        selected: selection.len(),
        cursor: selection.cursor().cloned(),
        entries: explorer
            .model
            .as_ref()
            .map_or((0, 0), |m| (m.generation, m.version)),
        names,
        search: explorer
            .search_results
            .as_ref()
            .filter(|_| searching(explorer))
            .map(|r| (Arc::as_ptr(&r.hits) as usize, explorer.search_cursor)),
    }
}

/// The selection as full paths: the items selected in list order, or in a search the result
/// under the cursor (as the menus take it).
fn selection_of(pane: &PaneInput<'_>) -> Vec<String> {
    if pane.page.is_some() {
        return Vec::new();
    }
    let explorer = pane.explorer;
    if searching(explorer) {
        return explorer
            .search_results
            .as_ref()
            .zip(explorer.search_cursor)
            .and_then(|(results, ix)| {
                let hit = results.hits.get(ix)?;
                Some(path_text(&results.root.join(&*hit.rel)))
            })
            .into_iter()
            .collect();
    }
    let selection = &explorer.selection;
    let Some(model) = explorer.model.as_ref().filter(|_| !selection.is_empty()) else {
        return Vec::new();
    };
    let mut names: Vec<&Arc<str>> = model
        .entries
        .iter()
        .filter(|e| selection.contains(&e.name))
        .map(|e| &e.name)
        .collect();
    if names.len() < selection.len() {
        // Some aren't in the listing (still loading, or in an expanded tree folder): those go
        // after, by name.
        let seen: HashSet<Arc<str>> = names.iter().map(|n| Arc::clone(*n)).collect();
        let mut rest: Vec<&Arc<str>> = selection.names().filter(|n| !seen.contains(*n)).collect();
        rest.sort_unstable();
        names.extend(rest);
    }
    names
        .into_iter()
        .map(|name| path_text(&model.path.join(&**name)))
        .collect()
}

/// Keeps the state of one window fresh; called as it renders. Cheap when nothing changed.
pub(crate) fn publish(window: u64, hwnd: isize, active_tab: usize, tabs: &[TabInput<'_>]) {
    let key = WindowKey {
        hwnd,
        active_tab,
        tabs: tabs
            .iter()
            .map(|tab| (tab.active_pane, tab.panes.iter().map(pane_key).collect()))
            .collect(),
    };
    let same = WINDOWS.lock().get(&window).is_some_and(|p| p.key == key);
    if same {
        return;
    }
    let state = Arc::new(window_state(hwnd, active_tab, tabs));
    WINDOWS.lock().insert(window, Published { key, state });
}

/// A window's state, selections listed.
pub(crate) fn window_state(hwnd: isize, active_tab: usize, tabs: &[TabInput<'_>]) -> WindowState {
    WindowState {
        hwnd,
        active_tab,
        tabs: tabs
            .iter()
            .map(|tab| TabState {
                active_pane: tab.active_pane,
                panes: tab
                    .panes
                    .iter()
                    .map(|pane| PaneState {
                        location: location_of(pane),
                        selection: selection_of(pane),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// A window closed.
pub(crate) fn forget(window: u64) {
    WINDOWS.lock().remove(&window);
}

/// The GPUI window id of the workspace window `hwnd`.
pub(crate) fn window_of(hwnd: isize) -> Option<u64> {
    WINDOWS
        .lock()
        .iter()
        .find(|(_, p)| p.state.hwnd == hwnd)
        .map(|(id, _)| *id)
}

/// The published windows: in `order` (HWNDs front to back), leaving out those not in it, or in
/// the order they opened.
pub(crate) fn snapshot(order: Option<&[isize]>) -> Snapshot {
    let mut windows: Vec<Arc<WindowState>> = WINDOWS
        .lock()
        .values()
        .map(|p| Arc::clone(&p.state))
        .collect();
    if let Some(order) = order {
        windows.retain(|w| order.contains(&w.hwnd));
        windows.sort_by_key(|w| order.iter().position(|h| *h == w.hwnd));
    }
    Snapshot { windows }
}

/// This process's `ArkeWindow`s, front to back.
fn z_order() -> Vec<isize> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId,
    };
    use windows::core::BOOL;

    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut pid = 0u32;
        // SAFETY: hwnd comes from EnumWindows; pid is a valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        if pid != std::process::id() {
            return BOOL(1);
        }
        let mut class = [0u16; 64];
        // SAFETY: hwnd comes from EnumWindows; class is writable for its length.
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        let ours = usize::try_from(len)
            .ok()
            .and_then(|len| class.get(..len))
            .is_some_and(|name| String::from_utf16_lossy(name) == WINDOW_CLASS);
        if ours {
            // SAFETY: lparam is the &mut Vec passed below, valid until EnumWindows returns.
            let found = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
            found.push(hwnd.0 as isize);
        }
        BOOL(1)
    }

    let mut found: Vec<isize> = Vec::new();
    // SAFETY: the callback only uses the address of `found` while EnumWindows runs.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM((&raw mut found) as isize));
    }
    found
}

// ─── Command line

/// Writes a reply to the launching terminal (or wherever stdout goes) and returns the exit code:
/// 1 for an error reply.
pub(crate) fn print_reply(reply: &[u8]) -> std::process::ExitCode {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(reply);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    if is_error(reply) {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_request_mangled_by_windows_powershell_is_read_from_the_raw_command_line() {
        // PowerShell 7.3+ and cmd: the argument is the JSON.
        let json = r#"{"v":1,"cmd":"state"}"#;
        assert_eq!(command_line_request(json, "ignored"), json);
        // Windows PowerShell 5.1: the quotes are gone from the argument, not from the line.
        let raw = r#""C:\Program Files\Arke\arke.exe" --api {"v":1,"cmd":"state"}"#;
        assert_eq!(command_line_request("{v:1,cmd:state}", raw), json);
        // ... and with a space in it, the whole request is wrapped in one more pair.
        let open = r#"{"v":1,"cmd":"open","path":"S:\\Studio\\Client work","new_tab":true}"#;
        let raw = format!(r#"arke.exe --api "{open}""#);
        assert_eq!(command_line_request("{v:1,cmd:open,path:S:", &raw), open);
        // Not JSON either way: the argument as given, for the bad_request reply.
        assert_eq!(
            command_line_request("state", "arke.exe --api state"),
            "state"
        );
        assert_eq!(command_line_request("", "arke.exe --api"), "");
    }

    #[test]
    fn piped_requests_are_read_in_any_encoding_powershell_uses() {
        let json = r#"{"v":1,"cmd":"state"}"#;
        assert_eq!(stdin_text(json.as_bytes()), json);
        let bom: Vec<u8> = [0xEF, 0xBB, 0xBF].into_iter().chain(json.bytes()).collect();
        assert_eq!(stdin_text(&bom), json);
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(json.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(stdin_text(&le), json);
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(json.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(stdin_text(&be), json);
    }

    use super::*;

    struct Fake {
        snapshot: Snapshot,
        probe: Probe,
    }

    impl Env for Fake {
        fn snapshot(&self) -> Snapshot {
            self.snapshot.clone()
        }

        fn probe(&self, _: &Path) -> Probe {
            self.probe
        }
    }

    fn env(probe: Probe) -> Fake {
        Fake {
            snapshot: Snapshot::default(),
            probe,
        }
    }

    fn reply(request: &str, probe: Probe) -> (Value, Option<Action>) {
        let answer = answer(request.as_bytes(), Target::LastUsed, &env(probe));
        (
            serde_json::from_slice(&answer.reply).unwrap(),
            answer.action,
        )
    }

    fn code(request: &str) -> String {
        let (value, action) = reply(request, Probe::Folder);
        assert!(action.is_none(), "an error queues nothing: {request}");
        assert_eq!(value["v"], 1);
        value["error"]["code"].as_str().unwrap().to_owned()
    }

    #[test]
    fn requests_parse() {
        assert_eq!(
            parse(br#"{"v":1,"cmd":"state","extra":[1,2]}"#),
            Ok(Request::State)
        );
        assert_eq!(parse(br#"{"cmd":"commands","v":1}"#), Ok(Request::Commands));
        // With the terminating NUL a `WM_COPYDATA` sender counted.
        assert_eq!(parse(b"{\"v\":1,\"cmd\":\"state\"}\0"), Ok(Request::State));
        assert_eq!(
            parse(br#"{"v":1,"cmd":"open","path":"C:\\work"}"#),
            Ok(Request::Open {
                path: PathBuf::from(r"C:\work"),
                new_tab: false,
                select: Vec::new(),
            })
        );
        assert_eq!(
            parse(
                r#"{"v":1,"cmd":"open","path":"C:\\測試","new_tab":true,"select":["a b.txt","C:\\測試\\c.txt"]}"#
                    .as_bytes()
            ),
            Ok(Request::Open {
                path: PathBuf::from(r"C:\測試"),
                new_tab: true,
                // A full path stands for its name.
                select: vec!["a b.txt".into(), "c.txt".into()],
            })
        );
        assert_eq!(
            parse(br#"{"v":1,"cmd":"run","id":"refresh"}"#),
            Ok(Request::Run {
                id: "refresh".into()
            })
        );
    }

    #[test]
    fn every_error_code() {
        for request in [
            "not json",
            "[1]",
            r#"{"cmd":"state"}"#,
            r#"{"v":"1","cmd":"state"}"#,
            r#"{"v":1}"#,
            r#"{"v":1,"cmd":5}"#,
            r#"{"v":1,"cmd":"open"}"#,
            r#"{"v":1,"cmd":"open","path":""}"#,
            r#"{"v":1,"cmd":"open","path":"C:\\x","new_tab":"yes"}"#,
            r#"{"v":1,"cmd":"open","path":"C:\\x","select":"a.txt"}"#,
            r#"{"v":1,"cmd":"open","path":"C:\\x","select":[1]}"#,
            r#"{"v":1,"cmd":"open","path":"relative\\x"}"#,
            r#"{"v":1,"cmd":"run"}"#,
        ] {
            assert_eq!(code(request), "bad_request", "{request}");
        }
        assert_eq!(code(r#"{"v":2,"cmd":"state"}"#), "unsupported_version");
        assert_eq!(code(r#"{"v":0}"#), "unsupported_version");
        assert_eq!(code(r#"{"v":1,"cmd":"explode"}"#), "unknown_command");
        assert_eq!(
            code(r#"{"v":1,"cmd":"run","id":"no_such_thing"}"#),
            "unknown_command"
        );
        assert_eq!(code(r#"{"v":1,"cmd":"run","id":"delete"}"#), "not_allowed");
        let (value, action) = reply(r#"{"v":1,"cmd":"open","path":"C:\\gone"}"#, Probe::Missing);
        assert_eq!(value["error"]["code"], "not_found");
        assert!(value["error"]["message"].as_str().is_some());
        assert!(action.is_none());
    }

    #[test]
    fn open_queues_the_folder_and_what_to_select() {
        let (value, action) = reply(
            r#"{"v":1,"cmd":"open","path":"C:\\work","new_tab":true,"select":["a.txt"]}"#,
            Probe::Folder,
        );
        assert_eq!(value, json!({ "v": 1, "ok": true }));
        assert_eq!(
            action,
            Some(Action::Open {
                target: Target::LastUsed,
                path: PathBuf::from(r"C:\work"),
                new_tab: true,
                select: vec!["a.txt".into()],
            })
        );
        // A file: its folder, with it selected.
        let (_, action) = reply(
            r#"{"v":1,"cmd":"open","path":"C:\\work\\report.docx"}"#,
            Probe::File,
        );
        assert_eq!(
            action,
            Some(Action::Open {
                target: Target::LastUsed,
                path: PathBuf::from(r"C:\work"),
                new_tab: false,
                select: vec!["report.docx".into()],
            })
        );
        // A share that doesn't answer in time is opened as it is.
        let (value, action) = reply(
            r#"{"v":1,"cmd":"open","path":"\\\\nas\\x"}"#,
            Probe::Unknown,
        );
        assert_eq!(value["ok"], true);
        assert!(matches!(action, Some(Action::Open { .. })));
    }

    #[test]
    fn run_queues_allowed_commands_only() {
        let answer = answer(
            br#"{"v":1,"cmd":"run","id":"refresh"}"#,
            Target::Window(0x1234),
            &env(Probe::Folder),
        );
        assert_eq!(
            answer.action,
            Some(Action::Run {
                target: Target::Window(0x1234),
                id: "refresh",
            })
        );
        for blocked in [
            "paste",
            "delete",
            "delete_permanently",
            "new_folder",
            "undo",
            "redo",
            "copy_to_peer",
            "move_to_peer",
        ] {
            let request = format!(r#"{{"v":1,"cmd":"run","id":"{blocked}"}}"#);
            assert_eq!(code(&request), "not_allowed", "{blocked}");
        }
    }

    #[test]
    fn commands_list_leaves_out_the_ones_that_change_files() {
        let (value, _) = reply(r#"{"v":1,"cmd":"commands"}"#, Probe::Folder);
        let ids: Vec<&str> = value["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"refresh") && ids.contains(&"new_tab"));
        for blocked in ["paste", "delete", "undo", "copy_to_peer", "move_to_peer"] {
            assert!(!ids.contains(&blocked), "{blocked}");
        }
        for command in value["commands"].as_array().unwrap() {
            assert!(!command["name"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn no_arke_running_has_no_windows() {
        let answer = answer(br#"{"v":1,"cmd":"state"}"#, Target::LastUsed, &NotRunning);
        assert_eq!(
            serde_json::from_slice::<Value>(&answer.reply).unwrap(),
            json!({ "v": 1, "windows": [] })
        );
        assert!(!is_error(&answer.reply));
        assert!(is_error(&error_reply(&bad("x"))));
        assert!(is_error(b"garbage"));
    }

    #[test]
    fn state_lists_windows_front_to_back() {
        let window = |hwnd: isize, location: Location| {
            Arc::new(WindowState {
                hwnd,
                tabs: vec![TabState {
                    panes: vec![PaneState {
                        location,
                        selection: Vec::new(),
                    }],
                    active_pane: 0,
                }],
                active_tab: 0,
            })
        };
        let snapshot = Snapshot {
            windows: vec![
                window(0x1A2B3C, Location::Home),
                window(
                    0x10,
                    Location::Search {
                        path: None,
                        query: "report".into(),
                    },
                ),
            ],
        };
        let value = state_json(&snapshot);
        assert_eq!(value["windows"][0]["hwnd"], "0x1A2B3C");
        assert_eq!(value["windows"][0]["active"], true);
        assert_eq!(value["windows"][1]["active"], false);
        assert_eq!(
            value["windows"][0]["tabs"][0]["panes"][0]["location"],
            json!({ "kind": "home" })
        );
        // The whole PC: no path.
        assert_eq!(
            value["windows"][1]["tabs"][0]["panes"][0]["location"],
            json!({ "kind": "search", "query": "report" })
        );
        assert_eq!(
            location_json(&Location::Search {
                path: Some(PathBuf::from(r"D:\Work")),
                query: "x".into(),
            }),
            json!({ "kind": "search", "path": r"D:\Work", "query": "x" })
        );
        assert_eq!(
            location_json(&Location::Remote("sftp://host/home".into())),
            json!({ "kind": "remote", "url": "sftp://host/home" })
        );
        assert_eq!(
            location_json(&Location::RecycleBin),
            json!({ "kind": "recycle_bin" })
        );
        assert_eq!(location_json(&Location::Other), json!({ "kind": "other" }));
    }

    #[test]
    fn requests_encode_back_to_the_same_request() {
        for request in [
            Request::State,
            Request::Commands,
            Request::Open {
                path: PathBuf::from(r"C:\測試\a b"),
                new_tab: true,
                select: vec!["x.txt".into()],
            },
            Request::Run {
                id: "refresh".into(),
            },
        ] {
            assert_eq!(parse(encode_request(&request).as_bytes()), Ok(request));
        }
    }

    #[test]
    fn relative_paths_become_absolute_on_the_command_line() {
        let Request::Open { path, .. } = absolute(Request::Open {
            path: PathBuf::from("sub"),
            new_tab: false,
            select: Vec::new(),
        }) else {
            panic!("still an open request");
        };
        assert!(
            path.is_absolute() && path.ends_with("sub"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn existing_and_missing_paths_probe() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(probe(&here), Probe::Folder);
        assert_eq!(probe(&here.join("arke-api-no-such-thing")), Probe::Missing);
        assert_eq!(probe(Path::new("sftp://host/x")), Probe::Unknown);
    }
}
