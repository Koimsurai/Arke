//! Video thumbnails come from Windows (`IShellItemImageFactory`, the thumbnails File Explorer
//! shows) and are asked for in a separate process, `arke.exe --thumb-host`.
//!
//! Video decoders crash and hang on damaged or unusual files, whoever made them, so they stay out
//! of Arke's process twice over: Windows already runs thumbnail providers in its own surrogate
//! process, and the call that waits on it runs in this host. A request past the timeout means the
//! host is stuck: it is killed and a new one starts with the next request. A host with nothing to
//! do for a while exits; the next request starts another.
//!
//! Transport is one JSON message per line over stdin/stdout; the picture comes back compressed
//! ([`arke_core::thumb_cache::encode`]) as hex. Only Arke's background loader thread talks to it.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use arke_core::thumbs::Thumbnail;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Starts arke.exe as a thumbnail host only: no window, no tray, no settings.
pub(crate) const HOST_ARG: &str = "--thumb-host";
/// Longest a thumbnail may take; a cold one of a large 4K video takes about a second.
const TIMEOUT: Duration = Duration::from_secs(8);
/// The host exits after this long without a request.
const IDLE: Duration = Duration::from_secs(60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    id: u64,
    path: PathBuf,
    max_side: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    id: u64,
    /// The compressed picture as hex, `None` when Windows has no thumbnail for the file.
    picture: Option<String>,
}

// ───────────────────────── Host side ─────────────────────────

/// Entry point of `arke.exe --thumb-host`: answers requests until Arke closes its input or none
/// comes for [`IDLE`].
pub(crate) fn run() {
    let (tx, rx) = flume::unbounded::<Request>();
    let _ = std::thread::Builder::new()
        .name("thumb-host-in".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if let Ok(request) = serde_json::from_str(&line)
                    && tx.send(request).is_err()
                {
                    break;
                }
            }
        });
    let mut out = std::io::stdout().lock();
    while let Ok(request) = rx.recv_timeout(IDLE) {
        let picture = crate::win_icons::video_thumbnail(&request.path, request.max_side)
            .and_then(|t| arke_core::thumb_cache::encode(&t))
            .map(|bytes| to_hex(&bytes));
        let reply = Reply {
            id: request.id,
            picture,
        };
        let Ok(line) = serde_json::to_string(&reply) else {
            continue;
        };
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            break;
        }
    }
}

// ───────────────────────── Arke side ─────────────────────────

struct Host {
    child: Child,
    stdin: ChildStdin,
    replies: flume::Receiver<Reply>,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The running host, if any, and the next request id.
static HOST: OnceLock<Mutex<(Option<Host>, u64)>> = OnceLock::new();

/// Windows' thumbnail for a video, fitted within `max_side`, from the host process. `None` when
/// there is none (unsupported format, no codec), or the host failed or took too long.
pub(crate) fn video_thumbnail(path: &Path, max_side: u32) -> Option<Thumbnail> {
    // One request at a time: the lock is held until the answer is in.
    ask(
        &mut HOST.get_or_init(|| Mutex::new((None, 0))).lock(),
        path,
        max_side,
    )
}

fn ask(state: &mut (Option<Host>, u64), path: &Path, max_side: u32) -> Option<Thumbnail> {
    let (host, next_id) = state;
    *next_id += 1;
    let id = *next_id;
    let line = serde_json::to_string(&Request {
        id,
        path: path.to_path_buf(),
        max_side,
    })
    .ok()?;
    // A host that exited while idle fails the write: start a new one and send once more.
    for attempt in 0..2 {
        if host.is_none() {
            *host = spawn()
                .inspect_err(|err| log::warn!("thumbnail host: {err}"))
                .ok();
        }
        let running = host.as_mut()?;
        if writeln!(running.stdin, "{line}")
            .and_then(|()| running.stdin.flush())
            .is_err()
        {
            *host = None;
            if attempt == 0 {
                continue;
            }
            return None;
        }
        break;
    }
    let running = host.as_mut()?;
    loop {
        match running.replies.recv_timeout(TIMEOUT) {
            Ok(reply) if reply.id == id => {
                return reply
                    .picture
                    .and_then(|hex| from_hex(&hex))
                    .and_then(|bytes| arke_core::thumb_cache::decode(&bytes));
            }
            // A late answer to a request given up on before.
            Ok(_) => {}
            Err(flume::RecvTimeoutError::Timeout) => {
                log::warn!(
                    "thumbnail host: no thumbnail of {} after {} s, restarting it",
                    path.display(),
                    TIMEOUT.as_secs()
                );
                *host = None;
                return None;
            }
            Err(flume::RecvTimeoutError::Disconnected) => {
                log::warn!("thumbnail host exited reading {}", path.display());
                *host = None;
                return None;
            }
        }
    }
}

fn spawn() -> std::io::Result<Host> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg(HOST_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .spawn()?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err(std::io::Error::other("no pipes to the thumbnail host"));
    };
    let (tx, replies) = flume::unbounded::<Reply>();
    std::thread::Builder::new()
        .name("thumb-host-read".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(reply) = serde_json::from_str::<Reply>(&line)
                    && tx.send(reply).is_err()
                {
                    break;
                }
            }
        })?;
    Ok(Host {
        child,
        stdin,
        replies,
    })
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 15, 16, 127, 128, 254, 255];
        assert_eq!(to_hex(&bytes), "00010f107f80feff");
        assert_eq!(from_hex(&to_hex(&bytes)).as_deref(), Some(&bytes[..]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
