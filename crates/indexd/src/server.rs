//! The service's pipe server: accepts only allowed users, one thread per connection.
//!
//! - The pipe ACL grants only SYSTEM and the users named at install (read and write data, no
//!   creating instances). The first instance uses `FILE_FLAG_FIRST_PIPE_INSTANCE`, so startup
//!   fails if someone already squatted on the name.
//! - Local clients only. Each client is identified (identification only, never acting with its
//!   rights) to decide which profiles' changes it may receive ([`crate::changes::Privacy`]).
//! - Exits after a period without connections (the service is demand-started).

use parking_lot::Mutex;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::proto::{self, Reply, Request, VERSION};
use crate::sec::{OwnedHandle, SecurityDescriptor, is_plain_sid, token_user_sid};
use arke_core::journal::Cursor;
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::{RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
    FILE_SHARE_MODE, FILE_WRITE_DATA, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
use windows::Win32::UI::Shell::{GetProfilesDirectoryW, GetUserProfileDirectoryW};
use windows::core::{PCWSTR, PWSTR};

use crate::changes::{Cached, Privacy, to_changes};
use crate::usn::{ReadError, Volume};

/// Max concurrent connections (one per drive from Arke, plus short cursor queries).
const MAX_CLIENTS: usize = 16;
/// Journal poll interval when there are no new changes.
const POLL: Duration = Duration::from_millis(300);
/// Ping interval when there are no changes.
const PING: Duration = Duration::from_secs(10);
/// Buffer size for one journal read.
const READ_BUF: usize = 64 * 1024;
/// Max items and approximate bytes per listing batch (a message is capped at 16 MB).
const LIST_BATCH: usize = 50_000;
const LIST_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct Config {
    /// SIDs allowed to connect.
    pub(crate) allowed: Vec<String>,
    pub(crate) pipe_name: String,
    /// Exit after this long without connections; `None` never exits (console mode).
    pub(crate) idle: Option<Duration>,
    /// Also grant the account running the server full pipe access. Needed in console mode and
    /// tests, which don't run as SYSTEM and otherwise can't create the next pipe instance.
    pub(crate) owner_access: bool,
}

pub(crate) struct Server {
    config: Config,
    stop: AtomicBool,
    active: AtomicUsize,
    last_active: Mutex<Instant>,
    /// Only one MFT read at a time (heavy on disk and memory).
    listing: Mutex<()>,
    /// Tests only: profile folders of identified clients.
    #[cfg(test)]
    seen_profiles: Mutex<Vec<Option<PathBuf>>>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Server {
    pub(crate) fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            stop: AtomicBool::new(false),
            listing: Mutex::new(()),
            active: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            #[cfg(test)]
            seen_profiles: Mutex::new(Vec::new()),
        })
    }

    /// Asks the server to stop (from another thread) by briefly connecting to its own pipe to
    /// wake the accept loop.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let name = wide(&self.config.pipe_name);
        // SAFETY: name is NUL-terminated and valid for the call; the handle is closed right away.
        if let Ok(h) = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        } {
            drop(OwnedHandle(h));
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Pipe ACL: SYSTEM gets full access; allowed users only read and write data (they can't
    /// create instances of the pipe).
    fn sddl(&self) -> Option<String> {
        let mut sddl = String::from("D:P(A;;GA;;;SY)");
        if self.config.owner_access {
            let me = crate::sec::current_user_sid()?;
            let _ = write!(sddl, "(A;;GA;;;{me})");
        }
        let user_access = FILE_GENERIC_READ.0 | FILE_WRITE_DATA.0;
        for sid in &self.config.allowed {
            if !is_plain_sid(sid) {
                return None;
            }
            let _ = write!(sddl, "(A;;0x{user_access:x};;;{sid})");
        }
        Some(sddl)
    }

    fn create_instance(&self, sd: &SecurityDescriptor, first: bool) -> Option<OwnedHandle> {
        let name = wide(&self.config.pipe_name);
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: false.into(),
        };
        let mut mode = PIPE_ACCESS_DUPLEX;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: name is NUL-terminated; attrs and sd are valid for the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_CLIENTS as u32 + 1,
                READ_BUF as u32,
                READ_BUF as u32,
                0,
                Some(&raw const attrs),
            )
        };
        if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
            log::warn!(
                "cannot create the pipe: {}",
                windows::core::Error::from_thread()
            );
            return None;
        }
        Some(OwnedHandle(handle))
    }

    /// Accepts connections until [`Server::stop`] or the idle timeout.
    pub(crate) fn run(self: &Arc<Self>) -> io::Result<()> {
        let sddl = self
            .sddl()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad SID"))?;
        let sd = SecurityDescriptor::from_sddl(&sddl).map_err(io::Error::other)?;
        let mut current = self.create_instance(&sd, true).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "pipe already exists (another server is running?)",
            )
        })?;
        self.watch_idle();
        log::info!("listening on {}", self.config.pipe_name);
        loop {
            // SAFETY: current is a valid pipe handle in synchronous mode.
            let connected = unsafe { ConnectNamedPipe(current.0, None) };
            if self.stopped() {
                return Ok(());
            }
            if let Err(err) = connected
                && err.code() != ERROR_PIPE_CONNECTED.to_hresult()
            {
                log::warn!("connect failed: {err}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let next = self
                .create_instance(&sd, false)
                .ok_or_else(|| io::Error::other("cannot create the next pipe instance"))?;
            let pipe = std::mem::replace(&mut current, next);
            if self.active.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
                self.active.fetch_sub(1, Ordering::SeqCst);
                log::warn!("too many clients");
                continue;
            }
            let server = self.clone();
            let spawned = std::thread::Builder::new()
                .name("indexd-client".into())
                .spawn(move || {
                    if let Err(err) = server.serve(&pipe) {
                        log::debug!("client ended: {err}");
                    }
                    // SAFETY: pipe is a valid, connected pipe handle.
                    unsafe {
                        let _ = FlushFileBuffers(pipe.0);
                        let _ = DisconnectNamedPipe(pipe.0);
                    }
                    *server.last_active.lock() = Instant::now();
                    server.active.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    fn watch_idle(self: &Arc<Self>) {
        let Some(idle) = self.config.idle else {
            return;
        };
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("indexd-idle".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(15));
                    let Some(server) = weak.upgrade() else {
                        return;
                    };
                    if server.stopped() {
                        return;
                    }
                    let quiet = server.active.load(Ordering::SeqCst) == 0
                        && server.last_active.lock().elapsed() >= idle;
                    if quiet {
                        log::info!("idle, stopping");
                        server.stop();
                        return;
                    }
                }
            });
    }

    fn serve(&self, pipe: &OwnedHandle) -> io::Result<()> {
        let mut io = PipeIo(pipe);
        // The client can only be identified after data has been read from the pipe.
        let hello = read_request(&mut io)?;
        let client = identify(pipe).ok_or_else(|| io::Error::other("cannot identify client"))?;
        if !self.config.allowed.contains(&client.sid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not allowed",
            ));
        }
        #[cfg(test)]
        self.seen_profiles.lock().push(client.profile.clone());
        if client.profile.is_none() {
            log::warn!("cannot find the profile folder of {}", client.sid);
        }
        let privacy = Privacy {
            profiles: profiles_dir(),
            mine: client.profile,
        };
        // On a version mismatch still send our version so the client knows, then disconnect.
        write_reply(&mut io, &Reply::Hello { version: VERSION })?;
        if hello != (Request::Hello { version: VERSION }) {
            return Ok(());
        }
        loop {
            match read_request(&mut io)? {
                Request::Cursor { root } => {
                    let cursor = Volume::open(&root)
                        .and_then(|v| v.query())
                        .ok()
                        .map(|d| Cursor {
                            journal: d.id,
                            usn: d.next,
                        });
                    write_reply(&mut io, &Reply::Cursor(cursor))?;
                }
                Request::Follow { root, from } => {
                    return self.follow(&mut io, &root, from, &privacy);
                }
                Request::List { root } => return self.list(&mut io, &root, &privacy),
                Request::Hello { .. } => return Ok(()),
            }
        }
    }

    /// Reads the MFT and sends the whole root listing in batches, minus other users' profiles.
    fn list(&self, io: &mut PipeIo, root: &Path, privacy: &Privacy) -> io::Result<()> {
        let _one = self.listing.lock();
        let mut table = match crate::mft::read_volume(root, &|| self.stopped()) {
            Ok(table) => table,
            Err(why) => {
                log::info!("{}: cannot list the MFT: {why}", root.display());
                // The protocol carries the reason as text.
                return write_reply(io, &Reply::ListFailed(why.to_string()));
            }
        };
        let mut batch = Vec::with_capacity(LIST_BATCH);
        let mut bytes = 0usize;
        let mut failed = None;
        let complete = crate::mft::listing(&mut table, root, privacy, |item| {
            bytes += item.name.len() + 32;
            batch.push(item);
            if batch.len() >= LIST_BATCH || bytes >= LIST_BATCH_BYTES {
                bytes = 0;
                if let Err(err) = write_reply(io, &Reply::Listing(std::mem::take(&mut batch))) {
                    failed = Some(err);
                    return false;
                }
            }
            !self.stopped()
        });
        drop(table);
        if let Some(err) = failed {
            return Err(err);
        }
        if !complete {
            return Ok(());
        }
        if !batch.is_empty() {
            write_reply(io, &Reply::Listing(batch))?;
        }
        write_reply(io, &Reply::ListDone)
    }

    /// Sends changes from `from`; once caught up, polls for new ones.
    fn follow(
        &self,
        io: &mut PipeIo,
        root: &Path,
        from: Cursor,
        privacy: &Privacy,
    ) -> io::Result<()> {
        let Ok(volume) = Volume::open(root) else {
            return write_reply(io, &Reply::Lost);
        };
        let Ok(data) = volume.query() else {
            return write_reply(io, &Reply::Lost);
        };
        if from.journal != data.id || from.usn < data.first || from.usn > data.next {
            return write_reply(io, &Reply::Lost);
        }
        let mut usn = from.usn;
        let mut caught_up = false;
        let mut last_sent = Instant::now();
        let mut buf = vec![0u8; READ_BUF];
        let mut resolve = Cached::new(|frn| volume.path_of(frn));
        loop {
            if self.stopped() {
                return Ok(());
            }
            let (next, records) = match volume.read(data.id, usn, &mut buf) {
                Ok(batch) => batch,
                Err(ReadError::Lost) => return write_reply(io, &Reply::Lost),
                Err(ReadError::Io(err)) => {
                    write_reply(io, &Reply::Lost)?;
                    return Err(io::Error::other(err));
                }
            };
            if records.is_empty() || next <= usn {
                if !caught_up {
                    caught_up = true;
                    write_reply(io, &Reply::CaughtUp)?;
                    last_sent = Instant::now();
                } else if last_sent.elapsed() >= PING {
                    write_reply(io, &Reply::Ping)?;
                    last_sent = Instant::now();
                }
                std::thread::sleep(POLL);
                continue;
            }
            usn = next;
            let changes = to_changes(&records, &mut resolve, privacy);
            if !changes.is_empty() {
                write_reply(
                    io,
                    &Reply::Changes {
                        changes,
                        cursor: Cursor {
                            journal: data.id,
                            usn,
                        },
                    },
                )?;
                last_sent = Instant::now();
            }
        }
    }
}

struct Client {
    sid: String,
    profile: Option<PathBuf>,
}

/// Identifies the user on the other end of the pipe, reverting to the service's own identity
/// right away.
fn identify(pipe: &OwnedHandle) -> Option<Client> {
    // SAFETY: pipe is a valid, connected pipe handle.
    unsafe { ImpersonateNamedPipeClient(pipe.0) }.ok()?;
    let mut token = HANDLE::default();
    // SAFETY: this thread is impersonating the client; asks for query access only and opens
    // the token as the service itself (OpenAsSelf).
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &raw mut token) };
    // SAFETY: plain Win32 call. If reverting fails we must not continue, since the thread would
    // still carry the client's identity.
    if unsafe { RevertToSelf() }.is_err() {
        std::process::abort();
    }
    opened.ok()?;
    let token = OwnedHandle(token);
    let sid = token_user_sid(token.0)?;
    let profile = user_profile(token.0);
    Some(Client { sid, profile })
}

fn read_wide_path(f: impl Fn(PWSTR, *mut u32) -> bool) -> Option<PathBuf> {
    let mut len = 0u32;
    let _ = f(PWSTR::null(), &raw mut len);
    if len == 0 || len > 32 * 1024 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    if !f(PWSTR(buf.as_mut_ptr()), &raw mut len) {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
}

fn user_profile(token: HANDLE) -> Option<PathBuf> {
    read_wide_path(|buf, len| {
        // SAFETY: token is a valid token; buf is null or holds at least *len u16s; len is valid.
        unsafe { GetUserProfileDirectoryW(token, Some(buf), len) }.is_ok()
    })
}

fn profiles_dir() -> Option<PathBuf> {
    read_wide_path(|buf, len| {
        // SAFETY: buf is null or holds at least *len u16s; len is a valid pointer.
        unsafe { GetProfilesDirectoryW(Some(buf), len) }.is_ok()
    })
}

struct PipeIo<'a>(&'a OwnedHandle);

impl Read for PipeIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: buf is valid and writable for the call; read is a valid out pointer; sync handle.
        unsafe { ReadFile(self.0.0, Some(buf), Some(&raw mut read), None) }
            .map_err(io::Error::other)?;
        Ok(read as usize)
    }
}

impl Write for PipeIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: buf is valid for the call; written is a valid out pointer; sync handle.
        unsafe { WriteFile(self.0.0, Some(buf), Some(&raw mut written), None) }
            .map_err(io::Error::other)?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_request(io: &mut PipeIo) -> io::Result<Request> {
    let frame = proto::read_frame(io)?;
    Request::decode(&frame).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad request"))
}

fn write_reply(io: &mut PipeIo, reply: &Reply) -> io::Result<()> {
    proto::write_frame(io, &reply.encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ServiceJournal;
    use crate::sec::current_user_sid;
    use arke_core::journal::{Journal, JournalEvent};

    fn test_pipe(tag: &str) -> String {
        format!(r"\\.\pipe\arke-indexd-test.{tag}.{}", std::process::id())
    }

    fn start(tag: &str, allowed: Vec<String>, owner_access: bool) -> (Arc<Server>, String) {
        let _ = env_logger::builder().is_test(true).try_init();
        let pipe_name = test_pipe(tag);
        let server = Server::new(Config {
            allowed,
            pipe_name: pipe_name.clone(),
            idle: None,
            owner_access,
        });
        let running = server.clone();
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(running.run().map_err(|e| e.to_string()));
        });
        if let Ok(result) = rx.recv_timeout(Duration::from_millis(300)) {
            panic!("server did not start: {result:?}");
        }
        // Wait for the pipe to appear.
        let name = wide(&pipe_name);
        let started = Instant::now();
        // SAFETY: name is NUL-terminated and valid for the call.
        while !unsafe { windows::Win32::System::Pipes::WaitNamedPipeW(PCWSTR(name.as_ptr()), 50) }
            .as_bool()
        {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "pipe did not appear"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        (server, pipe_name)
    }

    /// Needs no admin rights: exercises the pipe, ACL, identification and protocol. Without
    /// access to the volume the server reports unsupported or lost.
    #[test]
    fn allowed_user_talks_to_the_server() {
        let me = current_user_sid().unwrap();
        let (server, pipe) = start("ok", vec![me], true);
        let journal = ServiceJournal::at(&pipe);
        let cursor = journal.cursor(Path::new(r"C:\"));
        let elevated = cursor.is_some();
        assert_eq!(
            journal.cursor(Path::new(r"\\server\share\")),
            None,
            "only drive roots are accepted"
        );
        // Identification alone must still yield the profile folder, otherwise the privacy filter
        // would drop the user's own changes.
        let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        let seen = server.seen_profiles.lock().clone();
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|p| p.as_deref() == Some(home.as_path())),
            "{seen:?}"
        );
        let privacy = Privacy {
            profiles: profiles_dir(),
            mine: seen[0].clone(),
        };
        assert!(privacy.allows(&home.join(r"Desktop\a.txt")));

        // MFT listing: without admin it's unsupported (Arke walks instead); with admin the
        // user's own profile shows up.
        let mut found_home = false;
        let listed = journal.list(Path::new(r"C:\"), &mut |batch| {
            found_home |= batch.iter().any(|l| l.name.eq_ignore_ascii_case("Users"));
            true
        });
        if elevated {
            assert!(listed.is_ok() && found_home, "{listed:?}");
        } else {
            assert_eq!(listed.unwrap_err().kind(), io::ErrorKind::Unsupported);
        }
        assert_eq!(
            journal
                .list(Path::new(r"\\server\share\"), &mut |_| true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );

        // Unknown journal position: reported as lost right away.
        let (tx, rx) = flume::unbounded();
        let _guard = journal
            .follow(
                Path::new(r"C:\"),
                Cursor { journal: 1, usn: 0 },
                Box::new(move |e| {
                    let _ = tx.send(e);
                }),
            )
            .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            JournalEvent::Lost
        );

        // When the tests run elevated, also check that following works.
        if let Some(cursor) = cursor.filter(|_| elevated) {
            let (tx, rx) = flume::unbounded();
            let _guard = journal
                .follow(
                    Path::new(r"C:\"),
                    cursor,
                    Box::new(move |e| {
                        let _ = tx.send(e);
                    }),
                )
                .unwrap();
            let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                first,
                JournalEvent::Changed { .. } | JournalEvent::CaughtUp
            ));
        }
        server.stop();
    }

    #[test]
    fn other_users_cannot_connect() {
        // Only SYSTEM allowed: the current user can't connect (the ACL rejects it before any
        // protocol).
        let (server, pipe) = start("deny", vec!["S-1-5-18".into()], false);
        let name = wide(&pipe);
        // SAFETY: name is NUL-terminated and valid for the call.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_WRITE_DATA.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        let err = opened
            .map(OwnedHandle)
            .err()
            .expect("connection should be refused");
        assert_eq!(
            err.code(),
            windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult()
        );
        assert_eq!(ServiceJournal::at(&pipe).cursor(Path::new(r"C:\")), None);
        // Nor can it squat on the same pipe name.
        assert!(
            Server::new(Config {
                allowed: vec![current_user_sid().unwrap()],
                pipe_name: pipe,
                idle: None,
                owner_access: true,
            })
            .run()
            .is_err()
        );
        server.stop();
    }

    #[test]
    fn rejects_sids_that_would_rewrite_the_rules() {
        let server = Server::new(Config {
            allowed: vec!["S-1-5-18)(A;;GA;;;WD".into()],
            pipe_name: test_pipe("sddl"),
            idle: None,
            owner_access: false,
        });
        assert!(server.run().is_err());
    }
}
