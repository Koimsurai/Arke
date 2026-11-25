//! Remote locations (SFTP, WebDAV) in the shell layer: [`RemoteShell`] wraps the Windows shell
//! and handles every call that involves a remote URL, so the explorer runs its usual file
//! operations (copy, move, rename, new folder, delete, open, clipboard) on servers too.
//!
//! - Copying in uploads, copying out downloads, moves on one server are renames; nothing is
//!   ever overwritten (a taken name gets " (2)").
//! - Deleting on a server is permanent (the explorer confirms first); there is no undo.
//! - Opening downloads to the cache under `%TEMP%` and opens that copy. Changes to it are not
//!   uploaded back.
//! - Shell-only features (context menu extensions, Explorer, terminals, shortcuts, icons read
//!   from the file) answer "not available" for remote paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use arke_core::remote::RemoteUrl;
use arke_core::{Entry, EntryKind};
use arke_remote::{RemoteError, Remotes, TransferSink};
use parking_lot::Mutex;

use crate::shell::{
    ClipboardFiles, CloudPin, CloudScan, DragImage, DragImageRequest, DragOutEnd, FileOp,
    NetConnect, OpOutcome, OpProgress, PdfRender, PickRequest, Place, RecycledItem, Shell,
    ShellError, ShellMenu, Undo,
};

/// Largest file opened from a server (it is downloaded first).
const OPEN_MAX: u64 = 4 * 1024 * 1024 * 1024;
/// Most bytes downloaded to put remote items on the clipboard or drag them out.
pub(crate) const STAGE_MAX: u64 = 512 * 1024 * 1024;

fn remote(path: &Path) -> Option<RemoteUrl> {
    RemoteUrl::from_path(path)
}

/// Progress of a remote transfer, written into the explorer's [`OpProgress`].
struct Sink<'a>(&'a OpProgress);

impl TransferSink for Sink<'_> {
    fn cancelled(&self) -> bool {
        self.0.cancel.load(Ordering::Relaxed)
    }
    fn add_bytes(&self, n: u64) {
        self.0.bytes.fetch_add(n, Ordering::Relaxed);
        self.0.work_done.fetch_add(n, Ordering::Relaxed);
    }
}

impl Sink<'_> {
    fn item_done(&self) {
        self.0.done.fetch_add(1, Ordering::Relaxed);
    }
    fn add_total(&self, bytes: u64) {
        self.0.bytes_total.fetch_add(bytes, Ordering::Relaxed);
        self.0.work_total.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Name free in `taken` (lowercase names), Explorer style: `a (2).txt`, `folder (2)`.
fn free_name(name: &str, is_dir: bool, taken: &HashSet<String>) -> String {
    let exists = |n: &str| taken.contains(n);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !is_dir && !stem.is_empty() => {
            arke_core::path::unique_file_name(stem, ext, exists)
        }
        _ => arke_core::path::unique_name(name, exists),
    }
}

/// Local size of a file or folder tree (links are not followed into).
fn local_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        std::fs::read_dir(path).map_or(0, |rd| {
            rd.filter_map(Result::ok)
                .map(|e| local_size(&e.path()))
                .sum()
        })
    } else {
        std::fs::metadata(path).map_or(0, |m| m.len())
    }
}

/// Copies a local file or folder to `url` (which must not exist yet).
fn upload_tree(
    remotes: &Remotes,
    local: &Path,
    url: &RemoteUrl,
    sink: &Sink<'_>,
) -> Result<(), RemoteError> {
    if sink.cancelled() {
        return Err(RemoteError::Cancelled);
    }
    let meta = std::fs::symlink_metadata(local)?;
    if meta.is_dir() {
        match remotes.mkdir(url) {
            Ok(()) | Err(RemoteError::Exists) => {}
            Err(e) => return Err(e),
        }
        for item in std::fs::read_dir(local)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            upload_tree(remotes, &item.path(), &url.join(&name), sink)?;
        }
        sink.item_done();
        return Ok(());
    }
    if meta.is_symlink() && std::fs::metadata(local).is_ok_and(|m| m.is_dir()) {
        // A folder link: skipped rather than followed (it may loop).
        return Ok(());
    }
    let mut file = std::fs::File::open(local)?;
    let len = file.metadata()?.len();
    remotes.upload_file(local, url, &mut file, len, sink)?;
    sink.item_done();
    Ok(())
}

/// Copies a remote file or folder to `local` (which must not exist yet).
fn download_tree(
    remotes: &Remotes,
    url: &RemoteUrl,
    entry: &Entry,
    local: &Path,
    sink: &Sink<'_>,
) -> Result<(), RemoteError> {
    if sink.cancelled() {
        return Err(RemoteError::Cancelled);
    }
    match entry.kind {
        EntryKind::Dir => {
            std::fs::create_dir_all(local)?;
            for child in remotes.list(url)? {
                if child.kind == EntryKind::File {
                    sink.add_total(child.size);
                }
                let name = arke_remote::hub::local_name(&child.name);
                download_tree(
                    remotes,
                    &url.join(&child.name),
                    &child,
                    &local.join(name),
                    sink,
                )?;
            }
        }
        // A folder link on the server is not followed (it may loop).
        EntryKind::Link { is_dir: true } => return Ok(()),
        EntryKind::File | EntryKind::Link { is_dir: false } => {
            remotes.download_file(url, local, sink)?;
        }
    }
    sink.item_done();
    Ok(())
}

/// Lowercase names already in a local folder.
fn local_names(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir).map_or_default(|rd| {
        rd.filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_lowercase())
            .collect()
    })
}

/// A fresh folder under the cache for one staging (clipboard, drag out, copy between servers).
fn stage_dir(remotes: &Remotes) -> std::io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let dir = remotes
        .cache_dir()
        .join("staged")
        .join(format!("{stamp}-{n}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Downloads remote items into a new staging folder and returns their local copies, in order
/// (local paths pass through). Refuses past `max` bytes in total. Background thread.
pub(crate) fn stage_local_copies(
    remotes: &Remotes,
    paths: &[PathBuf],
    max: u64,
) -> Result<Vec<PathBuf>, RemoteError> {
    let progress = OpProgress::default();
    let sink = Sink(&progress);
    let mut dir: Option<PathBuf> = None;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(url) = remote(path) else {
            out.push(path.clone());
            continue;
        };
        let entry = remotes.stat(&url)?;
        if progress.bytes.load(Ordering::Relaxed) + entry.size > max {
            return Err(RemoteError::TooLarge);
        }
        let dir = if let Some(d) = &dir {
            d.clone()
        } else {
            let d = stage_dir(remotes)?;
            dir = Some(d.clone());
            d
        };
        let local = dir.join(arke_remote::hub::local_name(&entry.name));
        download_tree(remotes, &url, &entry, &local, &sink)?;
        if progress.bytes.load(Ordering::Relaxed) > max {
            return Err(RemoteError::TooLarge);
        }
        out.push(local);
    }
    Ok(out)
}

/// HTTP that always fails (WinHTTP unavailable, tests).
struct NoHttp;

impl arke_remote::dav::Transport for NoHttp {
    fn send(
        &self,
        _: arke_remote::dav::Request<'_>,
        _: &mut dyn std::io::Write,
        _: arke_remote::dav::Progress<'_>,
    ) -> Result<u16, RemoteError> {
        Err(RemoteError::Server("HTTP is unavailable".into()))
    }
}

/// The app's remote hub: host keys in `%APPDATA%\Arke\known_hosts`, downloads
/// under `%TEMP%\Arke\remote`, passwords in Credential Manager. Applies the saved locations' key
/// files. Touches no network.
pub(crate) fn start(
    store: &gpui::Entity<crate::settings::SettingsStore>,
    cx: &gpui::App,
) -> Arc<Remotes> {
    let known_hosts =
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Arke").join("known_hosts"));
    let transport: Arc<dyn arke_remote::dav::Transport> = match arke_remote::http::WinHttp::new() {
        Ok(http) => Arc::new(http),
        Err(err) => {
            log::error!("WinHTTP unavailable, WebDAV disabled: {err}");
            Arc::new(NoHttp)
        }
    };
    let remotes = Remotes::new(arke_remote::RemotesConfig {
        known_hosts,
        cache_dir: std::env::temp_dir().join("Arke").join("remote"),
        secrets: Arc::new(arke_remote::creds::CredentialManager),
        transport,
        portable: Arc::new(arke_remote::wpd::Wpd::new()),
    });
    for place in &store.read(cx).settings.remote_places {
        if let Some(url) = remote(&place.url) {
            remotes.set_key_file(&url, place.key_file.clone());
        }
    }
    for origin in &store.read(cx).settings.insecure_http {
        if let Some(url) = remote(Path::new(origin)) {
            remotes.set_insecure_allowed(&url, true);
        }
    }
    remotes
}

/// Whether the user allowed sending the password over plain http to the server of `path`.
/// Menus use this to offer [`revoke_insecure_http`] only where it applies.
pub(crate) fn insecure_http_allowed(remotes: &Remotes, path: &Path) -> bool {
    remote(path).is_some_and(|url| remotes.insecure_allowed(&url))
}

/// Takes back the plain-http allowance for the server of `path` (a saved location's menu
/// entry "Require a secure connection"). The next sign-in over http asks again.
pub(crate) fn revoke_insecure_http(remotes: &Remotes, path: &Path, cx: &mut gpui::App) {
    let Some(url) = remote(path) else {
        return;
    };
    remotes.set_insecure_allowed(&url, false);
    let origin = url.origin();
    crate::settings::SettingsStore::global(cx).update(cx, |s, cx| {
        s.update(|st| st.set_insecure_http(&origin, false), cx);
    });
}

/// A hub for tests: keys in memory, nothing remembered, no HTTP.
#[cfg(test)]
pub(crate) fn test_remotes() -> Arc<Remotes> {
    Remotes::new(arke_remote::RemotesConfig {
        known_hosts: None,
        cache_dir: std::env::temp_dir().join(format!("arke-test-remote-{}", std::process::id())),
        secrets: Arc::new(arke_remote::creds::NoSecrets),
        transport: Arc::new(NoHttp),
        portable: Arc::new(arke_remote::mtp::NoDevices),
    })
}

/// The Windows shell plus remote locations (see the module docs).
pub(crate) struct RemoteShell {
    inner: Arc<dyn Shell>,
    remotes: Arc<Remotes>,
    /// Remote items last cut to the clipboard (clipboard sequence number, originals): pasting
    /// them inside Arke moves the originals, not the staged copies.
    cut: Mutex<Option<(u32, Vec<PathBuf>)>>,
}

impl RemoteShell {
    pub(crate) fn new(inner: Arc<dyn Shell>, remotes: Arc<Remotes>) -> Self {
        Self {
            inner,
            remotes,
            cut: Mutex::new(None),
        }
    }

    fn touches_remote(op: &FileOp) -> bool {
        match op {
            FileOp::Copy { sources, dest } | FileOp::Move { sources, dest } => {
                remote(dest).is_some() || sources.iter().any(|p| remote(p).is_some())
            }
            FileOp::Delete { paths, .. } => paths.iter().any(|p| remote(p).is_some()),
            FileOp::Rename { path, .. } => remote(path).is_some(),
            FileOp::RenameMany { items } => items.iter().any(|(p, _)| remote(p).is_some()),
            FileOp::NewFolder { parent, .. } | FileOp::NewFile { parent, .. } => {
                remote(parent).is_some()
            }
        }
    }

    fn need(path: &Path) -> Result<RemoteUrl, RemoteError> {
        remote(path).ok_or(RemoteError::NotFound)
    }

    fn remote_op(
        &self,
        op: &FileOp,
        owner: Option<isize>,
        sink: &Sink<'_>,
    ) -> Result<(), ShellError> {
        let remotes = &*self.remotes;
        match op {
            FileOp::Copy { sources, dest } => self.transfer(sources, dest, false, owner, sink),
            FileOp::Move { sources, dest } => self.transfer(sources, dest, true, owner, sink),
            FileOp::Delete { paths, .. } => {
                for path in paths {
                    remotes.delete(&Self::need(path)?, sink)?;
                    sink.item_done();
                }
                Ok(())
            }
            FileOp::Rename { path, new_name } => {
                let from = Self::need(path)?;
                let to = from.parent().unwrap_or_else(|| from.clone()).join(new_name);
                Ok(remotes.rename(&from, &to)?)
            }
            FileOp::RenameMany { items } => {
                for (path, new_name) in items {
                    let from = Self::need(path)?;
                    let to = from.parent().unwrap_or_else(|| from.clone()).join(new_name);
                    remotes.rename(&from, &to)?;
                }
                Ok(())
            }
            FileOp::NewFolder { parent, name } => {
                Ok(remotes.mkdir(&Self::need(parent)?.join(name))?)
            }
            FileOp::NewFile { parent, name } => {
                Ok(remotes.create_empty(&Self::need(parent)?.join(name))?)
            }
        }
    }

    /// Copy or move between any mix of local and remote locations.
    fn transfer(
        &self,
        sources: &[PathBuf],
        dest: &Path,
        is_move: bool,
        owner: Option<isize>,
        sink: &Sink<'_>,
    ) -> Result<(), ShellError> {
        let remotes = &*self.remotes;
        let Some(to) = remote(dest) else {
            return self.download_into(sources, dest, is_move, owner, sink);
        };
        let mut taken: HashSet<String> = remotes
            .list(&to)?
            .iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        // Totals first, so the progress bar is right from the start.
        for source in sources {
            match remote(source) {
                None => sink.add_total(local_size(source)),
                Some(url) => {
                    if let Ok(e) = remotes.stat(&url)
                        && e.kind == EntryKind::File
                    {
                        sink.add_total(e.size);
                    }
                }
            }
        }
        let mut moved_local = Vec::new();
        for source in sources {
            match remote(source) {
                None => {
                    let name = source
                        .file_name()
                        .map_or_default(|n| n.to_string_lossy().into_owned());
                    let is_dir = source.is_dir();
                    let name = free_name(&name, is_dir, &taken);
                    taken.insert(name.to_lowercase());
                    upload_tree(remotes, source, &to.join(&name), sink)?;
                    if is_move {
                        moved_local.push(source.clone());
                    }
                }
                Some(from) => {
                    let entry = remotes.stat(&from)?;
                    let name = free_name(&entry.name, entry.is_dir_like(), &taken);
                    taken.insert(name.to_lowercase());
                    let target = to.join(&name);
                    if is_move && from.same_origin(&to) {
                        remotes.rename(&from, &target)?;
                        sink.item_done();
                        continue;
                    }
                    // Between servers (or a copy on one server: SFTP has no server-side copy):
                    // through a local staging folder.
                    let stage = stage_dir(remotes)?;
                    let local = stage.join(arke_remote::hub::local_name(&entry.name));
                    let result = download_tree(remotes, &from, &entry, &local, sink)
                        .and_then(|()| upload_tree(remotes, &local, &target, sink));
                    let _ = std::fs::remove_dir_all(&stage);
                    result?;
                    if is_move {
                        remotes.delete(&from, sink)?;
                    }
                }
            }
        }
        if !moved_local.is_empty() {
            // Moved to a server: the local originals go to the Recycle Bin, so a mistake is
            // recoverable.
            self.inner.file_op(
                &FileOp::Delete {
                    paths: moved_local,
                    recycle: true,
                },
                owner,
            )?;
        }
        Ok(())
    }

    /// Remote sources into a local folder (local sources go through the Windows shell).
    fn download_into(
        &self,
        sources: &[PathBuf],
        dest: &Path,
        is_move: bool,
        owner: Option<isize>,
        sink: &Sink<'_>,
    ) -> Result<(), ShellError> {
        let remotes = &*self.remotes;
        let (remote_sources, local_sources): (Vec<&PathBuf>, Vec<&PathBuf>) =
            sources.iter().partition(|p| remote(p).is_some());
        if !local_sources.is_empty() {
            let op = if is_move {
                FileOp::Move {
                    sources: local_sources.into_iter().cloned().collect(),
                    dest: dest.to_path_buf(),
                }
            } else {
                FileOp::Copy {
                    sources: local_sources.into_iter().cloned().collect(),
                    dest: dest.to_path_buf(),
                }
            };
            self.inner.file_op(&op, owner)?;
        }
        let mut taken = local_names(dest);
        let mut entries = Vec::new();
        for source in remote_sources {
            let url = Self::need(source)?;
            let entry = remotes.stat(&url)?;
            if entry.kind == EntryKind::File {
                sink.add_total(entry.size);
            }
            entries.push((url, entry));
        }
        for (url, entry) in entries {
            let name = free_name(
                &arke_remote::hub::local_name(&entry.name),
                entry.is_dir_like(),
                &taken,
            );
            taken.insert(name.to_lowercase());
            let local = dest.join(&name);
            if let Err(e) = download_tree(remotes, &url, &entry, &local, sink) {
                // Leave no half-copied file behind; a partly copied folder stays for the user to
                // inspect (its files are complete).
                if entry.kind == EntryKind::File {
                    let _ = std::fs::remove_file(&local);
                }
                return Err(e.into());
            }
            if is_move {
                remotes.delete(&url, sink)?;
            }
        }
        Ok(())
    }
}

fn outcome(result: Result<(), ShellError>) -> Result<OpOutcome, ShellError> {
    match result {
        Ok(()) => Ok(OpOutcome::Done),
        Err(ShellError::Remote(RemoteError::Cancelled)) => Ok(OpOutcome::Cancelled),
        Err(e) => Err(e),
    }
}

impl Shell for RemoteShell {
    fn load_places(&self) -> Vec<Place> {
        let mut places = self.inner.load_places();
        places.extend(phone_places());
        places
    }
    fn quick_access_pinned(&self) -> Option<Vec<PathBuf>> {
        self.inner.quick_access_pinned()
    }
    fn recent_files_enabled(&self) -> bool {
        self.inner.recent_files_enabled()
    }
    fn window_at_cursor(&self) -> Option<(isize, (i32, i32))> {
        self.inner.window_at_cursor()
    }
    fn disk_spaces(&self, roots: &[PathBuf]) -> Vec<(PathBuf, (u64, u64))> {
        self.inner.disk_spaces(roots)
    }
    fn open(&self, path: &Path, sort: Option<arke_core::SortSpec>) -> Result<(), ShellError> {
        match remote(path) {
            // A downloaded copy has no folder to step through.
            Some(url) => {
                let local = self.remotes.fetch(&url, OPEN_MAX)?;
                self.inner.open(&local, None)
            }
            None => self.inner.open(path, sort),
        }
    }
    fn expand_env(&self, input: &str) -> String {
        self.inner.expand_env(input)
    }
    fn format_time(&self, time: SystemTime) -> String {
        self.inner.format_time(time)
    }
    fn format_time_short(&self, time: SystemTime) -> String {
        self.inner.format_time_short(time)
    }
    fn file_op(&self, op: &FileOp, owner: Option<isize>) -> Result<OpOutcome, ShellError> {
        if Self::touches_remote(op) {
            let progress = OpProgress::default();
            return outcome(self.remote_op(op, owner, &Sink(&progress)));
        }
        self.inner.file_op(op, owner)
    }
    fn file_op_undoable(
        &self,
        op: &FileOp,
        owner: Option<isize>,
    ) -> (Result<OpOutcome, ShellError>, Option<Undo>) {
        if Self::touches_remote(op) {
            return (self.file_op(op, owner), None);
        }
        self.inner.file_op_undoable(op, owner)
    }
    fn file_op_tracked(
        &self,
        op: &FileOp,
        owner: Option<isize>,
        progress: &Arc<OpProgress>,
    ) -> (Result<OpOutcome, ShellError>, Option<Undo>) {
        if Self::touches_remote(op) {
            // No undo on servers: there is no recycle bin to put things back from.
            return (outcome(self.remote_op(op, owner, &Sink(progress))), None);
        }
        self.inner.file_op_tracked(op, owner, progress)
    }
    fn drag_out(
        &self,
        paths: &[PathBuf],
        image: Option<&DragImage>,
        after_lost_press: bool,
    ) -> Result<DragOutEnd, ShellError> {
        // Remote drags hand over staged local copies (see explorer_dnd), never URLs.
        if paths.iter().any(|p| remote(p).is_some()) {
            return Ok(DragOutEnd::Finished);
        }
        self.inner.drag_out(paths, image, after_lost_press)
    }
    fn covered_at_cursor(&self, owner: Option<isize>) -> bool {
        self.inner.covered_at_cursor(owner)
    }
    fn drag_image(
        &self,
        base: Option<arke_core::thumbs::Thumbnail>,
        request: &DragImageRequest,
    ) -> Option<DragImage> {
        if remote(&request.path).is_some() {
            let request = DragImageRequest {
                own_icon: false,
                ..request.clone()
            };
            return self.inner.drag_image(base, &request);
        }
        self.inner.drag_image(base, request)
    }
    fn undo(&self, undo: &Undo, owner: Option<isize>) -> Result<OpOutcome, ShellError> {
        self.inner.undo(undo, owner)
    }
    fn undo_tracked(
        &self,
        undo: &Undo,
        owner: Option<isize>,
        progress: &Arc<OpProgress>,
    ) -> Result<OpOutcome, ShellError> {
        self.inner.undo_tracked(undo, owner, progress)
    }
    fn recycle_bin(&self) -> Result<Vec<RecycledItem>, ShellError> {
        self.inner.recycle_bin()
    }
    fn recycle_restore(
        &self,
        items: &[RecycledItem],
        owner: Option<isize>,
    ) -> Result<OpOutcome, ShellError> {
        self.inner.recycle_restore(items, owner)
    }
    fn recycle_purge(
        &self,
        items: &[RecycledItem],
        progress: &OpProgress,
    ) -> Result<OpOutcome, ShellError> {
        self.inner.recycle_purge(items, progress)
    }
    fn set_clipboard_files(&self, files: &ClipboardFiles) -> Result<u32, ShellError> {
        if !files.paths.iter().any(|p| remote(p).is_some()) {
            *self.cut.lock() = None;
            return self.inner.set_clipboard_files(files);
        }
        // Other programs get local copies; a cut stays a move of the originals inside Arke.
        let staged = stage_local_copies(&self.remotes, &files.paths, STAGE_MAX)?;
        let seq = self.inner.set_clipboard_files(&ClipboardFiles {
            paths: staged,
            cut: false,
        })?;
        *self.cut.lock() = files.cut.then(|| (seq, files.paths.clone()));
        Ok(seq)
    }
    fn clipboard_files(&self) -> Result<Option<(ClipboardFiles, u32)>, ShellError> {
        let found = self.inner.clipboard_files()?;
        let cut = self.cut.lock().clone();
        Ok(match (found, cut) {
            (Some((_, seq)), Some((cut_seq, paths))) if seq == cut_seq => {
                Some((ClipboardFiles { paths, cut: true }, seq))
            }
            (found, _) => found,
        })
    }
    fn clipboard_virtual_files(
        &self,
    ) -> Result<Option<crate::win_virtual_drop::VirtualDrop>, ShellError> {
        self.inner.clipboard_virtual_files()
    }
    fn clear_clipboard_if(&self, seq: u32) {
        self.inner.clear_clipboard_if(seq);
    }
    fn clipboard_seq(&self) -> u32 {
        self.inner.clipboard_seq()
    }
    fn clipboard_has_files(&self) -> bool {
        self.inner.clipboard_has_files()
    }
    fn is_local_disk(&self, path: &Path) -> bool {
        remote(path).is_none() && self.inner.is_local_disk(path)
    }
    fn has_recycle_bin(&self, path: &Path) -> bool {
        remote(path).is_none() && self.inner.has_recycle_bin(path)
    }
    fn file_icon(&self, path: &Path, size: u32) -> Option<arke_core::thumbs::Thumbnail> {
        if remote(path).is_some() {
            return None;
        }
        self.inner.file_icon(path, size)
    }
    fn type_icon(&self, ext: &str) -> Option<arke_core::thumbs::Thumbnail> {
        self.inner.type_icon(ext)
    }
    fn video_thumbnail(
        &self,
        path: &Path,
        max_side: u32,
    ) -> Result<Option<arke_core::thumbs::Thumbnail>, ShellError> {
        if remote(path).is_some() {
            return Ok(None);
        }
        self.inner.video_thumbnail(path, max_side)
    }
    fn shell_menu(
        &self,
        paths: &[PathBuf],
        background: bool,
        extended: bool,
    ) -> Result<ShellMenu, ShellError> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.shell_menu(paths, background, extended)
    }
    fn shell_menu_prefetch(&self) -> bool {
        self.inner.shell_menu_prefetch()
    }
    fn invoke_shell_menu(
        &self,
        session: u64,
        id: u32,
        client_point: (i32, i32),
        owner: Option<isize>,
    ) -> Result<(), ShellError> {
        self.inner
            .invoke_shell_menu(session, id, client_point, owner)
    }
    fn release_shell_menu(&self, session: u64) {
        self.inner.release_shell_menu(session);
    }
    fn fill_shell_submenu(
        &self,
        session: u64,
        index: u32,
    ) -> Result<Vec<crate::shell::ShellMenuEntry>, ShellError> {
        self.inner.fill_shell_submenu(session, index)
    }
    fn shell_menu_kind(&self, paths: &[PathBuf], background: bool) -> Option<String> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return None;
        }
        self.inner.shell_menu_kind(paths, background)
    }
    fn packaged_item_types(&self, clsid: &str) -> Vec<String> {
        self.inner.packaged_item_types(clsid)
    }
    fn packaged_commands(&self, kind: &str) -> Vec<String> {
        self.inner.packaged_commands(kind)
    }
    fn shell_menu_handlers(
        &self,
        paths: &[PathBuf],
        background: bool,
    ) -> Result<Vec<crate::win_menu_parts::MenuHandler>, ShellError> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.shell_menu_handlers(paths, background)
    }
    fn shell_menu_handler(
        &self,
        paths: &[PathBuf],
        background: bool,
        extended: bool,
        handler: &crate::win_menu_parts::MenuHandler,
        group: u64,
    ) -> Result<(ShellMenu, std::time::Duration), ShellError> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner
            .shell_menu_handler(paths, background, extended, handler, group)
    }
    fn set_clipboard_text(&self, text: &str) -> Result<(), ShellError> {
        self.inner.set_clipboard_text(text)
    }
    fn reveal_in_explorer(&self, path: &Path, select: bool) -> Result<(), ShellError> {
        if remote(path).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.reveal_in_explorer(path, select)
    }
    fn file_details(&self, path: &Path) -> crate::shell::FileDetails {
        if remote(path).is_some() {
            return crate::shell::FileDetails::default();
        }
        self.inner.file_details(path)
    }
    fn set_attributes(&self, paths: &[PathBuf], set: u32, clear: u32) -> Result<(), ShellError> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.set_attributes(paths, set, clear)
    }
    fn is_blocked(&self, path: &Path) -> bool {
        remote(path).is_none() && self.inner.is_blocked(path)
    }
    fn unblock(&self, path: &Path) -> Result<(), ShellError> {
        if remote(path).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.unblock(path)
    }
    fn default_app(&self, path: &Path) -> Option<String> {
        if remote(path).is_some() {
            return None;
        }
        self.inner.default_app(path)
    }
    fn volume_info(&self, root: &Path) -> Option<crate::shell::VolumeInfo> {
        if remote(root).is_some() {
            return None;
        }
        self.inner.volume_info(root)
    }
    fn show_properties(&self, paths: &[PathBuf], owner: Option<isize>) -> Result<(), ShellError> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.show_properties(paths, owner)
    }
    fn shortcut_target(&self, path: &Path) -> Option<(PathBuf, bool)> {
        if remote(path).is_some() {
            return None;
        }
        self.inner.shortcut_target(path)
    }
    fn set_cloud_pin(&self, paths: &[PathBuf], pin: CloudPin) -> Result<(), ShellError> {
        if paths.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.set_cloud_pin(paths, pin)
    }
    fn create_shortcuts(
        &self,
        sources: &[PathBuf],
        dest: &Path,
    ) -> Result<Vec<PathBuf>, ShellError> {
        if remote(dest).is_some() || sources.iter().any(|p| remote(p).is_some()) {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.create_shortcuts(sources, dest)
    }
    fn recent_files(&self, limit: usize) -> Vec<crate::home::RecentFile> {
        self.inner.recent_files(limit)
    }
    fn is_ejectable(&self, root: &Path) -> bool {
        remote(root).is_none() && self.inner.is_ejectable(root)
    }
    fn eject_drive(&self, root: &Path) -> Result<(), ShellError> {
        self.inner.eject_drive(root)
    }
    fn format_drive(&self, root: &Path, owner: Option<isize>) -> Result<(), ShellError> {
        if remote(root).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.format_drive(root, owner)
    }
    fn disk_cleanup(&self, root: &Path) -> Result<(), ShellError> {
        if remote(root).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.disk_cleanup(root)
    }
    fn choose_app(&self, path: &Path, owner: Option<isize>) -> Result<(), ShellError> {
        if remote(path).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.choose_app(path, owner)
    }
    fn open_terminal(&self, dir: &Path) -> Result<(), ShellError> {
        if remote(dir).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.open_terminal(dir)
    }
    fn run_in(&self, command_line: &str, dir: &Path) -> Result<(), ShellError> {
        if remote(dir).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.run_in(command_line, dir)
    }
    fn run_quicklook(&self, exe: Option<&Path>, item: &Path) -> Result<(), ShellError> {
        if remote(item).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.run_quicklook(exe, item)
    }
    fn local_parts(&self, time: SystemTime) -> Option<[u16; 6]> {
        self.inner.local_parts(time)
    }
    fn decode_ansi(&self, bytes: &[u8]) -> String {
        self.inner.decode_ansi(bytes)
    }
    fn net_probe(&self) -> Option<Arc<dyn arke_core::net::NetProbe>> {
        self.inner.net_probe()
    }
    fn net_connect(&self, request: &NetConnect) -> Result<(), ShellError> {
        self.inner.net_connect(request)
    }
    fn net_disconnect(&self, drive: &Path) -> Result<(), ShellError> {
        self.inner.net_disconnect(drive)
    }
    fn editor_apps(&self) -> Vec<(String, PathBuf)> {
        self.inner.editor_apps()
    }
    fn set_autostart(&self, on: bool) -> Result<(), ShellError> {
        self.inner.set_autostart(on)
    }
    fn set_start_menu(&self, on: bool) -> Result<(), ShellError> {
        self.inner.set_start_menu(on)
    }
    fn map_drive_dialog(&self, owner: Option<isize>) -> Result<bool, ShellError> {
        self.inner.map_drive_dialog(owner)
    }
    fn disconnect_drive_dialog(&self, owner: Option<isize>) -> Result<bool, ShellError> {
        self.inner.disconnect_drive_dialog(owner)
    }
    fn pick_file(
        &self,
        request: &PickRequest,
        owner: Option<isize>,
    ) -> Result<Option<PathBuf>, ShellError> {
        self.inner.pick_file(request, owner)
    }
    fn drive_key(&self, path: &Path) -> String {
        // One queue per server connection: jobs on the same server take turns.
        remote(path).map_or_else(|| self.inner.drive_key(path), |url| url.origin())
    }
    fn render_pdf(&self, path: &Path, page: u32, max: (u32, u32)) -> Result<PdfRender, ShellError> {
        if remote(path).is_some() {
            return Err(ShellError::NotOnRemote);
        }
        self.inner.render_pdf(path, page, max)
    }
    fn cloud_scan(&self, dir: &Path, items: &[Arc<str>]) -> CloudScan {
        if remote(dir).is_some() {
            return CloudScan::default();
        }
        self.inner.cloud_scan(dir, items)
    }
    fn folder_visit(&self, slot: u64, visit: Option<crate::shell::FolderVisit>) {
        if visit.as_ref().is_some_and(|v| remote(&v.dir).is_some()) {
            return;
        }
        self.inner.folder_visit(slot, visit);
    }
}

/// The phones connected now, as last read ([`refresh_phones`]). Reading them takes WPD a moment
/// (the first time, while it starts), so the sidebar never waits for it: it takes this list and
/// is reloaded once a new one is in.
static PHONES: parking_lot::Mutex<Vec<arke_remote::mtp::Device>> =
    parking_lot::Mutex::new(Vec::new());

/// The phones in [`PHONES`] as places under This PC (#292).
fn phone_places() -> Vec<Place> {
    PHONES
        .lock()
        .iter()
        .map(|device| Place {
            label: device.name.clone(),
            path: PathBuf::from(format!("mtp://{}", device.key)),
            kind: crate::shell::PlaceKind::Phone,
        })
        .collect()
}

/// Reads which phones are connected (in the background) and, when that changed, has every
/// sidebar list its places again. At startup and whenever Windows says a portable device came
/// or went.
pub(crate) fn refresh_phones(remotes: Arc<Remotes>, cx: &mut gpui::App) {
    let read = cx
        .background_executor()
        .spawn(async move { remotes.portable_devices() });
    cx.spawn(async move |cx| {
        let devices = match read.await {
            Ok(devices) => devices,
            Err(err) => {
                log::info!("portable devices not listed: {err}");
                return;
            }
        };
        let changed = {
            let mut phones = PHONES.lock();
            let changed = *phones != devices;
            if changed {
                log::info!("portable devices: {}", devices.len());
                *phones = devices;
            }
            changed
        };
        if changed {
            cx.update(crate::workspace::reload_places_everywhere);
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connected phone shows under This PC, opening its `mtp://` location (#292).
    #[test]
    fn phones_are_places_under_this_pc() {
        *PHONES.lock() = vec![arke_remote::mtp::Device {
            key: "pixel-7-3fa2c1".into(),
            name: "Pixel 7".into(),
        }];
        let places = phone_places();
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].label, "Pixel 7");
        assert_eq!(places[0].path, PathBuf::from("mtp://pixel-7-3fa2c1"));
        assert_eq!(places[0].kind, crate::shell::PlaceKind::Phone);
        assert!(arke_core::remote::is_remote(&places[0].path));
        PHONES.lock().clear();
        assert!(phone_places().is_empty());
    }

    #[test]
    fn free_names_never_overwrite() {
        let taken: HashSet<String> = ["a.txt", "a (2).txt", "docs"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(free_name("a.txt", false, &taken), "a (3).txt");
        assert_eq!(free_name("A.TXT", false, &taken), "A (3).TXT");
        assert_eq!(free_name("docs", true, &taken), "docs (2)");
        assert_eq!(free_name("b.txt", false, &taken), "b.txt");
        assert_eq!(free_name(".env", false, &taken), ".env");
    }

    #[test]
    fn only_operations_on_servers_are_routed() {
        let local = PathBuf::from(r"C:\a");
        let server = PathBuf::from("sftp://u@h/home");
        assert!(!RemoteShell::touches_remote(&FileOp::Copy {
            sources: vec![local.clone()],
            dest: PathBuf::from(r"D:\"),
        }));
        assert!(RemoteShell::touches_remote(&FileOp::Copy {
            sources: vec![local.clone()],
            dest: server.clone(),
        }));
        assert!(RemoteShell::touches_remote(&FileOp::Delete {
            paths: vec![server.join("x")],
            recycle: true,
        }));
        assert!(RemoteShell::touches_remote(&FileOp::NewFolder {
            parent: server,
            name: "n".into(),
        }));
        assert!(!RemoteShell::touches_remote(&FileOp::Rename {
            path: local,
            new_name: "b".into(),
        }));
    }
}
