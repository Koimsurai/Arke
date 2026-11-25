//! "Open folders in Arke": which File Explorer windows Arke takes over, and the bookkeeping for the
//! ones it must leave alone.
//!
//! The shell side (watching new Explorer windows, reading where they are, closing them) lives in the
//! app's `win_takeover.rs`; this module only decides from strings, so it can be tested anywhere:
//! from the folder a window shows ([`classify`]) and, before any window exists, from the command
//! line of File Explorer's launcher ([`parse_explorer_args`], [`launch_target`]), with the same
//! answers.
//!
//! Only plain file-system folders Arke can list are taken over (local, mapped, UNC shares,
//! `\\wsl$`), plus Home / This PC / Quick access (Arke's Home page) and the Recycle Bin. Everything
//! else stays in Explorer: Control Panel, network neighbourhood, phones, inside archives, search
//! results, libraries and any other shell namespace.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where a taken-over Explorer window should open in Arke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A file-system folder.
    Folder(PathBuf),
    /// Home, This PC or Quick access: Arke's Home page.
    Home,
    /// The Recycle Bin page.
    RecycleBin,
}

/// Shell attributes of the folder an Explorer window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShellAttrs {
    /// `SFGAO_FILESYSTEM`: a real file-system location.
    pub filesystem: bool,
    /// `SFGAO_STREAM`: backed by a file, e.g. a .zip shown as a folder.
    pub stream: bool,
}

/// Home (Windows 11).
const HOME: &str = "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}";
/// Quick access (Windows 10).
const QUICK_ACCESS: &str = "::{679F85CB-0220-4080-B29B-5540CC05AAB6}";
/// This PC.
const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
/// Recycle Bin.
const RECYCLE_BIN: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";

/// Folders Explorer shows from inside a file; Arke leaves these to Explorer.
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "cab"];

/// Decides what to do with an Explorer window showing `parsing_name` (the folder's
/// desktop-absolute parsing name). `None` leaves the window to Explorer.
pub fn classify(parsing_name: &str, attrs: ShellAttrs) -> Option<Target> {
    let name = parsing_name.trim();
    if name.starts_with("::") {
        // Only a bare namespace root; anything below one (devices, Control Panel pages) stays.
        let root = name.trim_end_matches('\\');
        return if root.eq_ignore_ascii_case(HOME)
            || root.eq_ignore_ascii_case(QUICK_ACCESS)
            || root.eq_ignore_ascii_case(THIS_PC)
        {
            Some(Target::Home)
        } else if root.eq_ignore_ascii_case(RECYCLE_BIN) {
            Some(Target::RecycleBin)
        } else {
            None
        };
    }
    if !attrs.filesystem || attrs.stream || !is_listable_path(name) || inside_archive(name) {
        return None;
    }
    Some(Target::Folder(PathBuf::from(name)))
}

/// What `arke.exe <argument>` does when the shell starts it as the folder handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Arke shows it.
    Arke(Target),
    /// Arke can't: `explorer.exe` is started with this argument (which bypasses the folder verb,
    /// so nothing loops).
    Explorer(String),
}

/// Decides, before anything is initialized, where a folder argument goes. The shell passes `%1` for
/// every folder it opens, virtual ones included (`::{CLSID}` for Control Panel, This PC, ...).
/// Home, This PC, Quick access, the Recycle Bin and listable paths stay; Control Panel, search
/// folders, URLs, device paths, archives and every other namespace go to Explorer. Plain relative
/// paths (typed on a command line) are left to Arke. Uses [`classify`], so the Explorer window the
/// watcher sees for such a target is one it leaves alone, too.
pub fn route_argument(arg: &str) -> Route {
    let arg = arg.trim().trim_matches('"');
    let lower = arg.to_ascii_lowercase();
    let parsing = if let Some(rest) = lower.strip_prefix("shell:") {
        match rest {
            "recyclebinfolder" => return Route::Arke(Target::RecycleBin),
            "mycomputerfolder" | "homefolder" => return Route::Arke(Target::Home),
            // `shell:::{CLSID}`
            _ if rest.starts_with("::{") => &arg["shell:".len()..],
            _ => return Route::Explorer(arg.to_owned()),
        }
    } else {
        arg
    };
    if parsing.starts_with("::") {
        return classify(parsing, ShellAttrs::default())
            .map_or_else(|| Route::Explorer(format!("shell:{parsing}")), Route::Arke);
    }
    let bytes = parsing.as_bytes();
    let absolute = (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || parsing.starts_with(r"\\");
    if absolute {
        let attrs = ShellAttrs {
            filesystem: true,
            stream: false,
        };
        return classify(parsing, attrs)
            .map_or_else(|| Route::Explorer(parsing.to_owned()), Route::Arke);
    }
    // `search-ms:...`, `ftp://...`, `ms-settings:`: a scheme, not a relative path.
    let scheme = parsing.split_once(':').is_some_and(|(s, _)| {
        s.len() > 1 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    if scheme {
        return Route::Explorer(parsing.to_owned());
    }
    Route::Arke(Target::Folder(PathBuf::from(parsing)))
}

/// `X:\...` or `\\server\share\...`; not device paths (`\\?\`, `\\.\`) or a bare `\\server`.
fn is_listable_path(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return bytes.len() == 2 || bytes[2] == b'\\';
    }
    let Some(rest) = name.strip_prefix(r"\\") else {
        return false;
    };
    if rest.starts_with('?') || rest.starts_with('.') {
        return false;
    }
    let mut parts = rest.split('\\');
    let server = parts.next().unwrap_or_default();
    let share = parts.next().unwrap_or_default();
    !server.is_empty() && !share.is_empty()
}

fn inside_archive(name: &str) -> bool {
    name.split('\\').any(|component| {
        Path::new(component)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ARCHIVE_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
    })
}

/// Whether `path` is `root` or inside it (case-insensitive, whole components only).
pub fn within(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy();
    let root = root.to_string_lossy();
    let root = root.trim_end_matches('\\');
    if root.is_empty() || path.len() < root.len() || !path.is_char_boundary(root.len()) {
        return false;
    }
    let (head, tail) = path.split_at(root.len());
    head.eq_ignore_ascii_case(root) && (tail.is_empty() || tail.starts_with('\\'))
}

/// Whether the window watcher moves an Explorer window showing `target` to Arke. A window that opens
/// at Home or This PC is File Explorer started on purpose (taskbar, Start, `explorer.exe`), so it
/// stays; Win+E reaches Arke through the folder handler instead. Folders and the Recycle Bin, which
/// some program asked Explorer to show, are moved.
pub const fn watcher_takes(target: &Target) -> bool {
    !matches!(target, Target::Home)
}

/// What `explorer.exe <arguments>` asks File Explorer to show (#285: read from the launcher process
/// before it hands the request to the shell).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerLaunch {
    /// The folder the window opens on: an absolute path or a `::{CLSID}` parsing name.
    pub location: String,
    /// The item `/select,` picks in that folder (a name directly in it).
    pub select: Option<String>,
}

/// The arguments of a command line: everything after the program name, which may be quoted (the
/// way `CommandLineToArgvW` reads the first argument).
pub fn command_line_arguments(command_line: &str) -> &str {
    let line = command_line.trim_start();
    let rest = if let Some(quoted) = line.strip_prefix('"') {
        quoted.find('"').map_or("", |end| &quoted[end + 1..])
    } else {
        line.find(char::is_whitespace)
            .map_or("", |end| &line[end..])
    };
    rest.trim()
}

/// Reads `explorer.exe`'s arguments as File Explorer does: switches and objects separated by commas
/// or spaces, quotes keeping both inside a path. Understood: one object (an absolute path,
/// `::{CLSID}`, `shell:::{CLSID}`, `shell:RecycleBinFolder`, `shell:MyComputerFolder`), given
/// plainly, after `/root,` or after `/select,` (which opens the parent and selects the item);
/// `/n`, `/e` and `/separate` change nothing about where. Anything else (no object, relative
/// paths, other `shell:` names, `/idlist`, `/factory`, `-Embedding`, more than one object) is
/// `None`: left to the window watcher.
pub fn parse_explorer_args(args: &str) -> Option<ExplorerLaunch> {
    let mut object: Option<String> = None;
    let mut select = false;
    let mut tokens = explorer_tokens(args)?.into_iter();
    while let Some(token) = tokens.next() {
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "/n" | "/e" | "/separate" => {}
            "/select" | "/root" => {
                let value = tokens.next()?;
                if object.is_some() {
                    return None;
                }
                select = lower == "/select";
                object = Some(value);
            }
            _ if token.starts_with(['/', '-']) => return None,
            _ => {
                if object.is_some() {
                    return None;
                }
                object = Some(token);
            }
        }
    }
    let location = explorer_location(&object?)?;
    if !select {
        return Some(ExplorerLaunch {
            location,
            select: None,
        });
    }
    if location.starts_with("::") {
        return None;
    }
    // Selecting a drive shows This PC, which the watcher leaves alone anyway.
    let (parent, name) = location.rsplit_once('\\')?;
    if name.is_empty() {
        return None;
    }
    let parent = if parent.len() == 2 && parent.ends_with(':') {
        format!("{parent}\\")
    } else {
        parent.to_owned()
    };
    Some(ExplorerLaunch {
        location: parent,
        select: Some(name.to_owned()),
    })
}

/// Splits at commas and whitespace outside quotes; quotes are dropped, empty tokens skipped.
/// `None` for an unclosed quote.
fn explorer_tokens(args: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in args.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => tokens.push(std::mem::take(&mut current)),
            c if c.is_whitespace() && !quoted => tokens.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    if quoted {
        return None;
    }
    tokens.push(current);
    tokens.retain(|t| !t.is_empty());
    Some(tokens)
}

/// The parsing name Explorer's window will show for one object, when that is certain from the text
/// alone: absolute paths (trailing separators dropped, `.`/`..` and `/` refused) and the shell
/// names Arke knows.
fn explorer_location(object: &str) -> Option<String> {
    let lower = object.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("shell:") {
        return match rest {
            "recyclebinfolder" => Some(RECYCLE_BIN.to_owned()),
            "mycomputerfolder" => Some(THIS_PC.to_owned()),
            _ if rest.starts_with("::{") => Some(object["shell:".len()..].to_owned()),
            _ => None,
        };
    }
    if object.starts_with("::") {
        return Some(object.to_owned());
    }
    let bytes = object.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if !(drive || object.starts_with(r"\\")) || object.contains('/') {
        return None;
    }
    if object.split('\\').any(|part| part == "." || part == "..") {
        return None;
    }
    let trimmed = object.trim_end_matches('\\');
    if drive && trimmed.len() == 2 {
        return Some(format!("{trimmed}\\"));
    }
    if trimmed.len() < 3 {
        return None;
    }
    // After `X:` or `\\`. Explorer would show a different spelling than the text for empty parts
    // (`C:\\a`), names Windows trims (`a.`, `a `), streams and device paths (`a::$DATA`,
    // `\\?\C:`); a `name.{CLSID}` folder shows a shell namespace instead of its files. The tab the
    // early path opens is matched to a late window by spelling, so these are left to the watcher,
    // which reads the real folder.
    let body = &trimmed[2..];
    // `C:dir` is relative to the launcher's current folder on C:.
    if (drive && !body.starts_with('\\'))
        || body.contains(r"\\")
        || body.contains(':')
        || body
            .split('\\')
            .any(|part| part.ends_with(['.', ' ']) || (part.ends_with('}') && part.contains(".{")))
    {
        return None;
    }
    Some(trimmed.to_owned())
}

/// What a path is, as far as the caller looked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnDisk {
    /// A folder.
    Folder,
    /// A file (Explorer opens it in its program, or shows a .zip as a folder).
    File,
    /// Missing, or not looked at (network paths, which could take long to answer).
    Unknown,
}

/// The target Arke would take from the window `launch` is about to open: the same answer
/// [`classify`] gives for that window. A folder must be one on disk (a file such as a .zip shown as
/// a folder is a stream, left alone); a `/select,` item must exist (Explorer opens its default
/// folder otherwise). `on_disk` is asked only for paths [`classify`] could take.
pub fn launch_target(launch: &ExplorerLaunch, on_disk: impl Fn(&Path) -> OnDisk) -> Option<Target> {
    if launch.location.starts_with("::") {
        return if launch.select.is_some() {
            None
        } else {
            classify(&launch.location, ShellAttrs::default())
        };
    }
    let fs = ShellAttrs {
        filesystem: true,
        stream: false,
    };
    let target = classify(&launch.location, fs)?;
    if on_disk(Path::new(&launch.location)) != OnDisk::Folder {
        return None;
    }
    if let Some(name) = &launch.select
        && on_disk(Path::new(&launch.location).join(name).as_path()) == OnDisk::Unknown
    {
        return None;
    }
    Some(target)
}

/// A process as the early path (#285) tells them apart: its id and when it started (100 ns ticks
/// since 1601, as `GetProcessTimes` gives it), since a later process may get the same id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessKey {
    /// The process id.
    pub pid: u32,
    /// When it started; `None` when that wasn't read (it isn't File Explorer, or can't be opened).
    pub created: Option<u64>,
}

/// Size at which [`Judged`] looks for processes that ended but were not removed with
/// [`Judged::forget`] (a safety net: normally each goes the moment it ends).
pub const JUDGED_BOUND: usize = 64;

/// The processes the early path has judged (#285). Each is judged once, at the first window of it
/// that shows up, and never again while it runs, however many windows it makes later: one that was
/// left alone (Shift held, Arke's own, a `/separate` window) stays alone. `P` is what the caller
/// keeps for each, normally an open handle, which also keeps the id from going to another process
/// while it is noted: so on the hot path a process is found by id alone. The caller removes a
/// process when it ends ([`Self::forget`]); one whose end it couldn't learn of is dropped when the
/// list reaches its bound.
#[derive(Debug)]
pub struct Judged<P> {
    entries: Vec<(ProcessKey, P)>,
    bound: usize,
}

impl<P> Default for Judged<P> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            bound: JUDGED_BOUND,
        }
    }
}

impl<P> Judged<P> {
    /// Whether the process with id `pid` was judged already. No allocation, no system call.
    pub fn contains(&self, pid: u32) -> bool {
        self.entries.iter().any(|(key, _)| key.pid == pid)
    }

    /// The key noted for `pid`.
    pub fn key(&self, pid: u32) -> Option<ProcessKey> {
        self.entries
            .iter()
            .find(|(key, _)| key.pid == pid)
            .map(|(key, _)| *key)
    }

    /// Notes a judged process. When the list has reached its bound, first drops the processes
    /// `ended` says are gone, and the bound grows with the ones still running. A process noted
    /// with the id of one still in the list (it ended and the id was reused) replaces it.
    pub fn note(&mut self, key: ProcessKey, process: P, ended: impl Fn(&ProcessKey, &P) -> bool) {
        if self.entries.len() >= self.bound {
            self.entries.retain(|(k, p)| !ended(k, p));
            self.bound = (self.entries.len() * 2).max(JUDGED_BOUND);
        }
        self.entries.retain(|(noted, _)| noted.pid != key.pid);
        self.entries.push((key, process));
    }

    /// Removes and returns the first process `which` picks (one that ended); `None` when it is no
    /// longer noted (dropped or replaced already).
    pub fn forget(&mut self, which: impl Fn(&ProcessKey, &P) -> bool) -> Option<(ProcessKey, P)> {
        let ix = self.entries.iter().position(|(k, p)| which(k, p))?;
        Some(self.entries.remove(ix))
    }

    /// Number of processes noted.
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether none are noted.
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Whether a process that started at `created` came after the early path's hook was set at
/// `hooked` (same clock): File Explorer processes that were running before are never ended, whatever
/// their command line says (#285).
pub const fn started_after(created: u64, hooked: u64) -> bool {
    created > hooked
}

/// What Arke could not open after the early path ended its launcher, and gave back to File Explorer
/// (#285). The launcher that reopens each is left alone once, so a folder Arke keeps failing on
/// can't go round in a loop; the next launcher for that target uses the mark up.
#[derive(Debug, Default)]
pub struct GivenBack {
    targets: Vec<Target>,
}

impl GivenBack {
    /// Notes that `target` went back to File Explorer.
    pub fn add(&mut self, target: Target) {
        self.targets.push(target);
    }

    /// Whether a launcher for `target` is one Arke started to give it back; uses up the mark.
    pub fn take(&mut self, target: &Target) -> bool {
        let Some(ix) = self.targets.iter().position(|t| same_target(t, target)) else {
            return false;
        };
        self.targets.remove(ix);
        true
    }
}

fn same_target(a: &Target, b: &Target) -> bool {
    match (a, b) {
        (Target::Folder(a), Target::Folder(b)) => same_path(a, b),
        (a, b) => a == b,
    }
}

/// Test-only restriction (`ARKE_TAKEOVER_TEST_ROOT`): with a root set, only folders inside it are
/// taken over, so live checks never touch the tester's own Explorer windows.
pub fn allowed_by_test_root(target: &Target, root: Option<&Path>) -> bool {
    match (root, target) {
        (None, _) => true,
        (Some(root), Target::Folder(path)) => within(path, root),
        (Some(_), _) => false,
    }
}

/// Names of the selected items that sit directly in `folder` (what Arke can select there).
/// `items` are full parsing names; anything elsewhere is dropped, duplicates once.
pub fn selected_names(folder: &Path, items: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in items {
        let item = Path::new(item);
        let (Some(parent), Some(name)) = (item.parent(), item.file_name()) else {
            continue;
        };
        if !same_path(parent, folder) {
            continue;
        }
        let name = name.to_string_lossy().into_owned();
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    names
}

/// Whether `a` and `b` name the same folder: case-insensitive (non-ASCII letters too, as NTFS
/// compares them), `/` taken as `\`, trailing separators ignored.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    let a = a.trim_end_matches(['\\', '/']);
    let b = b.trim_end_matches(['\\', '/']);
    let fold = |s: &str| {
        s.chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    a.eq_ignore_ascii_case(b) || fold(a) == fold(b)
}

/// How long a "leave this one to Explorer" mark stays valid.
pub const BYPASS_TTL: Duration = Duration::from_secs(10);

/// Folders Arke itself just handed to Explorer ("Show in File Explorer"): the next Explorer window
/// on each is left alone once, within [`BYPASS_TTL`].
#[derive(Debug, Default)]
pub struct Bypass {
    entries: Vec<(PathBuf, Instant)>,
}

impl Bypass {
    /// Marks the folder Explorer will show for `path`: the parent when Explorer selects `path`,
    /// otherwise `path` itself.
    pub fn add(&mut self, path: &Path, select: bool, now: Instant) {
        let folder = if select {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        self.prune(now);
        self.entries.push((folder.to_path_buf(), now + BYPASS_TTL));
    }

    /// Whether an Explorer window on `location` should be left alone; uses up the mark.
    pub fn take(&mut self, location: &Path, now: Instant) -> bool {
        self.prune(now);
        let Some(ix) = self
            .entries
            .iter()
            .position(|(folder, _)| same_path(folder, location))
        else {
            return false;
        };
        self.entries.remove(ix);
        true
    }

    /// Whether there is a live mark for `location`, without using it up (the early path looks; the
    /// watcher, which sees the window Arke asked for, takes it).
    pub fn holds(&self, location: &Path, now: Instant) -> bool {
        self.entries
            .iter()
            .any(|(folder, until)| *until > now && same_path(folder, location))
    }

    /// Number of live marks.
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no marks.
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn prune(&mut self, now: Instant) {
        self.entries.retain(|(_, until)| *until > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: ShellAttrs = ShellAttrs {
        filesystem: true,
        stream: false,
    };
    const VIRTUAL: ShellAttrs = ShellAttrs {
        filesystem: false,
        stream: false,
    };

    fn folder(p: &str) -> Target {
        Target::Folder(PathBuf::from(p))
    }

    #[test]
    fn takes_over_file_system_folders() {
        for p in [
            r"C:\",
            r"C:",
            r"D:\Projects\報告 2026",
            r"Z:\mapped\share",
            r"\\nas\share",
            r"\\nas\share\docs",
            r"\\wsl$\Ubuntu\home",
            r"\\wsl.localhost\Ubuntu",
            r"C:\Users\me\OneDrive",
        ] {
            assert_eq!(classify(p, FS), Some(folder(p)), "{p} should be taken over");
        }
    }

    #[test]
    fn leaves_remote_urls_alone() {
        for p in [
            "sftp://u@h/home",
            r"sftp:\\u@h\home",
            "https://dav.example/files",
        ] {
            assert_eq!(classify(p, FS), None, "{p} is not an Explorer folder");
        }
    }

    #[test]
    fn maps_home_this_pc_and_recycle_bin() {
        assert_eq!(classify(HOME, VIRTUAL), Some(Target::Home));
        assert_eq!(
            classify("::{f874310e-b6b7-47dc-bc84-b9e6b38f5903}", VIRTUAL),
            Some(Target::Home),
            "CLSIDs compare case-insensitively"
        );
        assert_eq!(classify(QUICK_ACCESS, VIRTUAL), Some(Target::Home));
        assert_eq!(classify(THIS_PC, VIRTUAL), Some(Target::Home));
        assert_eq!(classify(RECYCLE_BIN, VIRTUAL), Some(Target::RecycleBin));
    }

    #[test]
    fn leaves_everything_else_to_explorer() {
        let leave = [
            // Control Panel and one of its pages
            ("::{26EE0668-A00A-44D7-9371-BEB064C98683}", VIRTUAL),
            (
                r"::{26EE0668-A00A-44D7-9371-BEB064C98683}\0\::{BB06C0E4-D293-4F75-8A90-CB05B6477EEE}",
                VIRTUAL,
            ),
            // Network neighbourhood, a bare server, printers
            ("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", VIRTUAL),
            (r"\\nas", FS),
            ("::{2227A280-3AEA-1069-A2DE-08002B30309D}", VIRTUAL),
            // A phone under This PC (MTP)
            (
                r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\\?\usb#vid_04e8&pid_6860#r58m#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
                VIRTUAL,
            ),
            // Libraries, Gallery
            (
                r"::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\Documents.library-ms",
                VIRTUAL,
            ),
            ("::{E88865EA-0E1C-4E20-9AA6-EDCD0212C87C}", VIRTUAL),
            // Search results, FTP
            (
                "search-ms:displayname=Search%20Results&crumb=location:C%3A%5C",
                VIRTUAL,
            ),
            ("ftp://example.com/pub", VIRTUAL),
            // Inside archives
            (
                r"C:\Downloads\a.zip",
                ShellAttrs {
                    filesystem: true,
                    stream: true,
                },
            ),
            (r"C:\Downloads\a.zip\inner", VIRTUAL),
            (r"C:\Downloads\b.CAB\x", FS),
            // Device paths, odd strings
            (r"\\?\C:\x", FS),
            (r"\\.\PhysicalDrive0", FS),
            ("", FS),
            ("Desktop", FS),
        ];
        for (p, attrs) in leave {
            assert_eq!(classify(p, attrs), None, "{p} should stay in Explorer");
        }
    }

    #[test]
    fn routes_what_the_shell_passes_as_the_folder_handler() {
        let arke = Route::Arke;
        assert_eq!(route_argument(r"C:\Users\me"), arke(folder(r"C:\Users\me")));
        assert_eq!(route_argument(r#""D:\a b""#), arke(folder(r"D:\a b")));
        assert_eq!(route_argument(r"C:\"), arke(folder(r"C:\")));
        assert_eq!(
            route_argument(r"\\srv\share\x"),
            arke(folder(r"\\srv\share\x"))
        );
        assert_eq!(
            route_argument("docs"),
            arke(folder("docs")),
            "relative stays"
        );
        for home in [
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            "shell:::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}",
            "shell:MyComputerFolder",
        ] {
            assert_eq!(route_argument(home), arke(Target::Home), "{home}");
        }
        for bin in [
            "::{645FF040-5081-101B-9F08-00AA002F954E}",
            "shell:RecycleBinFolder",
        ] {
            assert_eq!(route_argument(bin), arke(Target::RecycleBin), "{bin}");
        }
    }

    #[test]
    fn routes_everything_arke_cannot_show_to_explorer() {
        let explorer = |s: &str| Route::Explorer(s.to_owned());
        // Control Panel (category view), God Mode, Network, libraries.
        assert_eq!(
            route_argument("::{26EE0668-A00A-44D7-9371-BEB064C98683}"),
            explorer("shell:::{26EE0668-A00A-44D7-9371-BEB064C98683}")
        );
        assert_eq!(
            route_argument("::{ED7BA470-8E54-465E-825C-99712043E01C}"),
            explorer("shell:::{ED7BA470-8E54-465E-825C-99712043E01C}")
        );
        assert_eq!(
            route_argument("shell:NetworkPlacesFolder"),
            explorer("shell:NetworkPlacesFolder")
        );
        assert_eq!(
            route_argument("shell:Libraries"),
            explorer("shell:Libraries")
        );
        assert_eq!(
            route_argument("search-ms:query=a&crumb=location:C%3A"),
            explorer("search-ms:query=a&crumb=location:C%3A")
        );
        assert_eq!(route_argument("ftp://host/x"), explorer("ftp://host/x"));
        assert_eq!(route_argument(r"\\?\C:\x"), explorer(r"\\?\C:\x"));
        assert_eq!(route_argument(r"\\server"), explorer(r"\\server"));
        assert_eq!(
            route_argument(r"C:\Temp\a.zip\inner"),
            explorer(r"C:\Temp\a.zip\inner")
        );
    }

    #[test]
    fn what_goes_to_explorer_is_never_taken_back_by_the_watcher() {
        // The Explorer window for these shows the same location; classify (the watcher's rule)
        // leaves it alone.
        for arg in [
            "::{26EE0668-A00A-44D7-9371-BEB064C98683}",
            "shell:NetworkPlacesFolder",
            r"C:\Temp\a.zip\inner",
            r"\\server",
        ] {
            let Route::Explorer(target) = route_argument(arg) else {
                panic!("{arg} should go to Explorer");
            };
            let parsing = target.strip_prefix("shell:").unwrap_or(&target);
            assert_eq!(classify(parsing, ShellAttrs::default()), None, "{arg}");
        }
    }

    #[test]
    fn within_matches_whole_components() {
        let root = Path::new(r"C:\Temp\arke-test");
        assert!(within(Path::new(r"C:\Temp\arke-test"), root));
        assert!(within(Path::new(r"c:\temp\ARKE-TEST\sub\x"), root));
        assert!(within(Path::new(r"C:\Temp\arke-test\"), root));
        assert!(!within(Path::new(r"C:\Temp\arke-test2"), root));
        assert!(!within(Path::new(r"C:\Temp"), root));
        assert!(!within(Path::new(r"D:\Temp\arke-test"), root));
        assert!(within(Path::new(r"C:\x"), Path::new(r"C:\")));
        assert!(!within(Path::new(r"C:\x"), Path::new("")));
    }

    #[test]
    fn explorer_started_on_purpose_is_left_alone() {
        assert!(!watcher_takes(&Target::Home));
        assert!(watcher_takes(&Target::RecycleBin));
        assert!(watcher_takes(&Target::Folder(PathBuf::from(r"C:\Users"))));
    }

    #[test]
    fn test_root_filter_limits_what_is_taken() {
        let root = Path::new(r"C:\Temp\arke-test");
        let inside = Target::Folder(PathBuf::from(r"C:\Temp\arke-test\sub"));
        let outside = Target::Folder(PathBuf::from(r"C:\Temp\arke-test2"));
        assert!(allowed_by_test_root(&inside, None));
        assert!(allowed_by_test_root(&outside, None));
        assert!(allowed_by_test_root(&Target::Home, None));
        assert!(allowed_by_test_root(&inside, Some(root)));
        assert!(!allowed_by_test_root(&outside, Some(root)));
        assert!(!allowed_by_test_root(&Target::Home, Some(root)));
        assert!(!allowed_by_test_root(&Target::RecycleBin, Some(root)));
    }

    #[test]
    fn selected_names_keep_items_of_the_folder() {
        let items = vec![
            r"C:\t\a.txt".to_string(),
            r"c:\T\b.txt".to_string(),
            r"C:\t\sub\deep.txt".to_string(),
            r"C:\t\A.TXT".to_string(),
            r"C:\other\c.txt".to_string(),
            "::{645FF040-5081-101B-9F08-00AA002F954E}".to_string(),
        ];
        assert_eq!(
            selected_names(Path::new(r"C:\t\"), &items),
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert!(selected_names(Path::new(r"C:\t"), &[]).is_empty());
    }

    #[test]
    fn bypass_is_used_once_and_expires() {
        let t0 = Instant::now();
        let mut bypass = Bypass::default();
        bypass.add(Path::new(r"C:\t\a.txt"), true, t0);
        bypass.add(Path::new(r"C:\x"), false, t0);
        assert_eq!(bypass.len(), 2);
        // Selecting a file opens its folder.
        assert!(!bypass.take(Path::new(r"C:\t\a.txt"), t0));
        assert!(bypass.take(Path::new(r"c:\T\"), t0));
        assert!(!bypass.take(Path::new(r"C:\t"), t0), "used up");
        // Expired marks don't count.
        assert!(!bypass.take(Path::new(r"C:\x"), t0 + BYPASS_TTL));
        assert!(bypass.is_empty());
    }

    #[test]
    fn same_path_ignores_case_separators_and_trailing_slashes() {
        assert!(same_path(Path::new(r"C:\Work\"), Path::new(r"c:\WORK")));
        assert!(same_path(
            Path::new("C:/Work/Docs"),
            Path::new(r"C:\work\docs\")
        ));
        assert!(same_path(
            Path::new("D:\\\u{c4}rger"),
            Path::new("d:\\\u{e4}RGER")
        ));
        assert!(!same_path(Path::new(r"C:\Work"), Path::new(r"C:\Work2")));
        assert!(!same_path(
            Path::new(r"C:\Work"),
            Path::new(r"C:\Work\Docs")
        ));
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "compared with what the parser gives, an Option"
    )]
    fn launch(location: &str, select: Option<&str>) -> Option<ExplorerLaunch> {
        Some(ExplorerLaunch {
            location: location.to_owned(),
            select: select.map(str::to_owned),
        })
    }

    #[test]
    fn command_line_arguments_follow_the_program_name() {
        assert_eq!(
            command_line_arguments(r#""C:\Windows\explorer.exe" /select,"C:\a b\x.txt""#),
            r#"/select,"C:\a b\x.txt""#
        );
        assert_eq!(command_line_arguments(r"explorer.exe  C:\dir "), r"C:\dir");
        assert_eq!(command_line_arguments(r"C:\Windows\explorer.exe"), "");
        assert_eq!(command_line_arguments(r#""C:\Windows\explorer.exe""#), "");
        assert_eq!(command_line_arguments(""), "");
    }

    #[test]
    fn parses_what_explorer_is_asked_to_show() {
        assert_eq!(parse_explorer_args(r"C:\dir"), launch(r"C:\dir", None));
        assert_eq!(parse_explorer_args(r"C:\dir\"), launch(r"C:\dir", None));
        assert_eq!(parse_explorer_args("C:"), launch(r"C:\", None));
        assert_eq!(parse_explorer_args(r"C:\"), launch(r"C:\", None));
        assert_eq!(
            parse_explorer_args(r#""D:\a b\報告""#),
            launch(r"D:\a b\報告", None)
        );
        assert_eq!(
            parse_explorer_args(r#""C:\a,b""#),
            launch(r"C:\a,b", None),
            "quotes keep commas"
        );
        assert_eq!(
            parse_explorer_args(r"\\nas\share\docs"),
            launch(r"\\nas\share\docs", None)
        );
        // Switches that don't change where.
        assert_eq!(parse_explorer_args(r"/e,C:\dir"), launch(r"C:\dir", None));
        assert_eq!(
            parse_explorer_args(r"/n, /e, C:\dir"),
            launch(r"C:\dir", None)
        );
        assert_eq!(
            parse_explorer_args(r#"/separate,"C:\a b""#),
            launch(r"C:\a b", None)
        );
        assert_eq!(
            parse_explorer_args(r"/root,C:\dir"),
            launch(r"C:\dir", None)
        );
        assert_eq!(
            parse_explorer_args(r"/E,/ROOT,C:\dir"),
            launch(r"C:\dir", None)
        );
    }

    #[test]
    fn parses_select_as_the_parent_folder_and_a_name() {
        assert_eq!(
            parse_explorer_args(r"/select,C:\dir\x.txt"),
            launch(r"C:\dir", Some("x.txt"))
        );
        assert_eq!(
            parse_explorer_args(r#"/select,"C:\a b\x y.txt""#),
            launch(r"C:\a b", Some("x y.txt"))
        );
        assert_eq!(
            parse_explorer_args(r#"/select, "C:\a b\x.txt""#),
            launch(r"C:\a b", Some("x.txt")),
            "a space after the comma"
        );
        assert_eq!(
            parse_explorer_args(r"/SELECT,C:\dir\sub\"),
            launch(r"C:\dir", Some("sub")),
            "selecting a folder opens its parent"
        );
        assert_eq!(
            parse_explorer_args(r"/select,C:\x.txt"),
            launch(r"C:\", Some("x.txt"))
        );
        assert_eq!(
            parse_explorer_args(r"/e,/select,\\nas\share\a.txt"),
            launch(r"\\nas\share", Some("a.txt"))
        );
        // A drive (This PC) or a namespace item: not understood.
        assert_eq!(parse_explorer_args(r"/select,C:\"), None);
        assert_eq!(
            parse_explorer_args("/select,::{645FF040-5081-101B-9F08-00AA002F954E}"),
            None
        );
        assert_eq!(parse_explorer_args("/select"), None);
    }

    #[test]
    fn parses_shell_names_arke_knows() {
        for bin in [
            "shell:RecycleBinFolder",
            r#""shell:recyclebinfolder""#,
            "::{645FF040-5081-101B-9F08-00AA002F954E}",
            "shell:::{645FF040-5081-101B-9F08-00AA002F954E}",
        ] {
            assert_eq!(parse_explorer_args(bin), launch(RECYCLE_BIN, None), "{bin}");
        }
        assert_eq!(
            parse_explorer_args("shell:MyComputerFolder"),
            launch(THIS_PC, None)
        );
        assert_eq!(parse_explorer_args(HOME), launch(HOME, None));
    }

    #[test]
    fn leaves_arguments_it_does_not_understand() {
        for args in [
            "",
            "/n",
            "/e,",
            "/factory,{75dff2b7-6936-4c06-a8bb-676a7b00b24b} -Embedding",
            "/idlist,:1234:5678",
            "/root,/idlist,:1234:5678",
            "docs",
            r".\docs",
            r"C:\Program Files",
            r"C:\a,b",
            r#""C:\unclosed"#,
            r"C:\a\..\b",
            "C:/dir",
            "shell:Downloads",
            "file:///C:/dir",
            r"C:\a C:\b",
            r"/root,C:\a,C:\b",
            r"/select,C:\a\x,/select,C:\b\y",
            // Switches alone, or with nothing after them.
            "/separate",
            "/root,",
            "/select,",
            ",",
            "-Embedding",
            r"/separate,/idlist,:1:2",
            // Unquoted spaces split the path.
            r"/select,C:\a b\x.txt",
            r#""/select,C:\dir\x.txt""#,
            // Relative, drive-relative and UNC forms Arke can't name for certain.
            ".",
            r"..\dir",
            r"C:dir",
            r"\dir",
            r"\\",
            r"\\?\C:\dir",
            r"\\.\C:\dir",
            // Spellings Explorer shows differently, streams, namespace junctions.
            r"C:\\dir",
            r"C:\a\\b",
            r#""C:\dir ""#,
            r"C:\dir.",
            r"C:\dir::$INDEX_ALLOCATION",
            r"C:\a:b",
            r"C:\GodMode.{ED7BA470-8E54-465E-825C-99712043E01C}",
            r"/select,C:\x.{645FF040-5081-101B-9F08-00AA002F954E}\a.txt",
            // Shell names: only the Recycle Bin and This PC; nothing below a namespace.
            "shell:RecycleBinFolder\\",
            "shell:Desktop",
            "shell:HomeFolder",
            "/select,shell:RecycleBinFolder",
            "/select,shell:::{645FF040-5081-101B-9F08-00AA002F954E}",
            "ms-settings:",
            "https://example.com",
        ] {
            assert_eq!(parse_explorer_args(args), None, "{args}");
        }
    }

    #[test]
    fn parses_other_spellings_explorer_accepts() {
        assert_eq!(
            parse_explorer_args(r#"/select,"C:\a,b\x,y.txt""#),
            launch(r"C:\a,b", Some("x,y.txt")),
            "quoted commas in a selection"
        );
        assert_eq!(
            parse_explorer_args(r#"/root,"shell:RecycleBinFolder""#),
            launch(RECYCLE_BIN, None)
        );
        assert_eq!(
            parse_explorer_args("::{645ff040-5081-101b-9f08-00aa002f954e}"),
            launch("::{645ff040-5081-101b-9f08-00aa002f954e}", None),
            "CLSIDs as given; classify compares them ignoring case"
        );
        assert_eq!(
            parse_explorer_args("\tC:\\dir\t/n"),
            launch(r"C:\dir", None),
            "tabs separate too; a switch after the object"
        );
        assert_eq!(
            parse_explorer_args(r#"C:\"a b"\c"#),
            launch(r"C:\a b\c", None),
            "quotes inside a token"
        );
        assert_eq!(
            parse_explorer_args(r"\\wsl$\Ubuntu\home"),
            launch(r"\\wsl$\Ubuntu\home", None)
        );
        assert_eq!(parse_explorer_args(r"C:\a {b}\c"), None, "unquoted space");
        assert_eq!(
            parse_explorer_args(r#""C:\a {b}""#),
            launch(r"C:\a {b}", None),
            "braces without a dot before them are a plain name"
        );
        assert_eq!(
            parse_explorer_args(r#""C:\v1.{x}""#),
            None,
            "anything shaped like a namespace junction is left alone"
        );
    }

    #[test]
    fn command_line_arguments_edge_cases() {
        assert_eq!(
            command_line_arguments(r#""C:\Windows\explorer.exe"C:\dir"#),
            r"C:\dir",
            "no space after the quoted program"
        );
        assert_eq!(
            command_line_arguments("explorer.exe\t/select,C:\\x.txt"),
            r"/select,C:\x.txt"
        );
        assert_eq!(command_line_arguments(r#""unclosed C:\dir"#), "");
        assert_eq!(command_line_arguments("   "), "");
    }

    /// Stands in for a process handle: whether that process still runs.
    #[derive(Debug)]
    struct Proc {
        running: std::rc::Rc<std::cell::Cell<bool>>,
    }

    fn spawn() -> (Proc, std::rc::Rc<std::cell::Cell<bool>>) {
        let running = std::rc::Rc::new(std::cell::Cell::new(true));
        let handle = Proc {
            running: std::rc::Rc::clone(&running),
        };
        (handle, running)
    }

    const fn key(pid: u32, created: u64) -> ProcessKey {
        ProcessKey {
            pid,
            created: Some(created),
        }
    }

    fn ended(_: &ProcessKey, p: &Proc) -> bool {
        !p.running.get()
    }

    #[test]
    fn each_process_is_judged_once() {
        let mut judged = Judged::default();
        assert!(!judged.contains(7));
        let (p, _running) = spawn();
        judged.note(key(7, 100), p, ended);
        assert!(judged.contains(7), "never judged again while it runs");
        // However many processes come and go after it, a running one stays noted.
        for pid in 1000..1000 + 10 * JUDGED_BOUND as u32 {
            let (p, running) = spawn();
            judged.note(key(pid, 200), p, ended);
            running.set(false);
        }
        assert!(judged.contains(7));
        assert!(judged.len() <= JUDGED_BOUND, "{}", judged.len());
    }

    #[test]
    fn a_reused_id_is_judged_again_once_its_process_ended() {
        let mut judged = Judged::default();
        let (p, running) = spawn();
        judged.note(key(7, 100), p, ended);
        running.set(false);
        // Still noted until the list is pruned: the handle keeps the id from being reused.
        assert_eq!(judged.key(7), Some(key(7, 100)));
        for pid in 1000..1000 + JUDGED_BOUND as u32 {
            let (p, _) = spawn();
            judged.note(key(pid, 200), p, ended);
        }
        assert!(!judged.contains(7), "pruned once it ended");
        // The id now names a new process, started later: judged on its own.
        let (p, _running) = spawn();
        judged.note(key(7, 300), p, ended);
        assert_eq!(judged.key(7), Some(key(7, 300)));
        // A key for an id still noted replaces it rather than doubling it.
        let (p, _running) = spawn();
        judged.note(key(7, 400), p, ended);
        assert_eq!(judged.key(7), Some(key(7, 400)));
        assert_eq!(judged.entries.iter().filter(|(k, _)| k.pid == 7).count(), 1);
    }

    #[test]
    fn an_ended_process_is_forgotten_at_once() {
        // `P` is the serial the caller's exit wait reports, as Arke's entries carry one.
        let mut judged = Judged::default();
        let never = |_: &ProcessKey, _: &u32| false;
        judged.note(key(7, 100), 1, never);
        judged.note(key(8, 100), 2, never);
        assert_eq!(
            judged.forget(|_, serial| *serial == 1),
            Some((key(7, 100), 1))
        );
        assert!(!judged.contains(7), "gone at once, not when the list fills");
        assert!(judged.contains(8));
        assert_eq!(
            judged.forget(|_, serial| *serial == 1),
            None,
            "gone already"
        );
        // A later process that got the id is judged on its own; a late report for the old one
        // doesn't remove it.
        judged.note(key(7, 300), 3, never);
        assert_eq!(judged.forget(|_, serial| *serial == 1), None);
        assert_eq!(judged.key(7), Some(key(7, 300)));
    }

    #[test]
    fn the_bound_grows_with_running_processes() {
        let mut judged = Judged::default();
        for pid in 0..3 * JUDGED_BOUND as u32 {
            let (p, _running) = spawn();
            judged.note(key(pid, 1), p, ended);
        }
        assert_eq!(judged.len(), 3 * JUDGED_BOUND, "none ended, none dropped");
        assert!((0..3 * JUDGED_BOUND as u32).all(|pid| judged.contains(pid)));
    }

    #[test]
    fn processes_from_before_the_hook_are_left_alone() {
        let hooked = 1_000;
        assert!(!started_after(999, hooked));
        assert!(!started_after(hooked, hooked), "same tick: not after");
        assert!(started_after(1_001, hooked));
    }

    #[test]
    fn a_target_goes_back_once_per_give_back() {
        let mut back = GivenBack::default();
        assert!(!back.take(&Target::RecycleBin));
        back.add(folder(r"C:\Work"));
        back.add(Target::RecycleBin);
        back.add(Target::RecycleBin);
        assert!(!back.take(&folder(r"C:\Other")));
        assert!(back.take(&folder(r"c:\work\")));
        assert!(!back.take(&folder(r"C:\Work")), "used up");
        assert!(back.take(&Target::RecycleBin));
        assert!(back.take(&Target::RecycleBin), "given back twice");
        assert!(!back.take(&Target::RecycleBin));
    }

    #[test]
    fn launch_target_gives_the_watchers_answer() {
        let disk = |p: &Path| match p.to_str().unwrap_or_default() {
            r"C:\dir" | r"C:\dir\sub" | r"C:\" | r"C:\t.zip\inner" => OnDisk::Folder,
            r"C:\dir\x.txt" | r"C:\t.zip" | r"C:\x.txt" => OnDisk::File,
            _ => OnDisk::Unknown,
        };
        let target = |args: &str| launch_target(&parse_explorer_args(args)?, disk);
        assert_eq!(target(r"C:\dir"), Some(folder(r"C:\dir")));
        assert_eq!(target(r"/select,C:\dir\x.txt"), Some(folder(r"C:\dir")));
        assert_eq!(target(r"/select,C:\dir\sub"), Some(folder(r"C:\dir")));
        assert_eq!(target(r"/select,C:\x.txt"), Some(folder(r"C:\")));
        assert_eq!(target("shell:RecycleBinFolder"), Some(Target::RecycleBin));
        // Home and This PC: what File Explorer started on purpose shows; the watcher leaves it.
        for home in [HOME, THIS_PC, "shell:MyComputerFolder"] {
            let got = target(home);
            assert_eq!(got, Some(Target::Home), "{home}");
            assert!(!got.as_ref().is_some_and(watcher_takes), "{home}");
        }
        // A file opens in its program; a .zip shows as a folder (a stream); missing paths and
        // missing selections make Explorer open its default folder.
        assert_eq!(target(r"C:\dir\x.txt"), None);
        assert_eq!(target(r"C:\t.zip"), None);
        assert_eq!(target(r"C:\t.zip\inner"), None);
        assert_eq!(target(r"C:\missing"), None);
        assert_eq!(target(r"/select,C:\dir\gone.txt"), None);
        assert_eq!(target(r"\\nas\share"), None, "not looked at");
        assert_eq!(target(r"\\nas"), None, "a bare server");
        // Control Panel stays in Explorer.
        assert_eq!(
            target("shell:::{26EE0668-A00A-44D7-9371-BEB064C98683}"),
            None
        );
    }

    #[test]
    fn bypass_holds_without_using_up() {
        let t0 = Instant::now();
        let mut bypass = Bypass::default();
        bypass.add(Path::new(r"C:\t\a.txt"), true, t0);
        assert!(bypass.holds(Path::new(r"C:\T"), t0));
        assert!(bypass.holds(Path::new(r"C:\t"), t0), "still there");
        assert!(!bypass.holds(Path::new(r"C:\t\a.txt"), t0));
        assert!(!bypass.holds(Path::new(r"C:\t"), t0 + BYPASS_TTL));
        assert!(bypass.take(Path::new(r"C:\t"), t0));
    }

    #[test]
    fn bypass_counts_repeated_reveals() {
        let t0 = Instant::now();
        let mut bypass = Bypass::default();
        bypass.add(Path::new(r"C:\t"), false, t0);
        bypass.add(Path::new(r"C:\t"), false, t0 + Duration::from_secs(1));
        assert!(bypass.take(Path::new(r"C:\t"), t0 + Duration::from_secs(2)));
        assert!(bypass.take(Path::new(r"C:\t"), t0 + Duration::from_secs(2)));
        assert!(!bypass.take(Path::new(r"C:\t"), t0 + Duration::from_secs(2)));
    }
}
