//! Trusted SSH host keys, in OpenSSH `known_hosts` format, kept in Arke's own file
//! (`%APPDATA%\Arke\known_hosts`), not the user's `~/.ssh`.
//!
//! Lines are `host[,host...] algorithm base64 [comment]`; non-default ports are written
//! `[host]:port`. Hashed (`|1|`) and marker (`@revoked`) lines from a copied OpenSSH file are
//! skipped, never rewritten away.

use std::io;
use std::path::PathBuf;

use parking_lot::Mutex;

use crate::error::HostKey;

/// What known_hosts says about a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// This exact key is trusted for the host.
    Trusted,
    /// The host has no key of this type yet.
    Unknown,
    /// The host has another key of the same type: refuse.
    Changed,
}

/// How the host is written in known_hosts.
pub fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Checks `algorithm`/`key` for `pattern` against known_hosts text.
pub fn check(text: &str, pattern: &str, algorithm: &str, key: &str) -> Verdict {
    let mut verdict = Verdict::Unknown;
    for (hosts, algo, data) in text.lines().filter_map(parse_line) {
        if algo != algorithm || !hosts.split(',').any(|h| h.eq_ignore_ascii_case(pattern)) {
            continue;
        }
        if data == key {
            return Verdict::Trusted;
        }
        verdict = Verdict::Changed;
    }
    verdict
}

/// `text` with the host's keys of this type replaced by `key`.
pub fn with_trusted(text: &str, host: &HostKey) -> String {
    let mut out = String::with_capacity(text.len() + 128);
    for line in text.lines() {
        let stale = parse_line(line).is_some_and(|(hosts, algo, _)| {
            algo == host.algorithm && hosts.split(',').any(|h| h.eq_ignore_ascii_case(&host.host))
        });
        if !stale {
            out.push_str(line);
            out.push('\n');
        }
    }
    {
        use std::fmt::Write as _;
        let _ = writeln!(out, "{} {} {}", host.host, host.algorithm, host.key);
    }
    out
}

fn parse_line(line: &str) -> Option<(&str, &str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(['#', '@', '|']) {
        return None;
    }
    let mut fields = line.split_whitespace();
    Some((fields.next()?, fields.next()?, fields.next()?))
}

/// The known_hosts file. Reads it fresh on every check (it is tiny and connects are rare), so
/// edits made while Arke runs take effect.
pub struct KnownHosts {
    path: Option<PathBuf>,
    /// In-memory content when there is no file (tests); also serializes writes.
    memory: Mutex<String>,
}

impl KnownHosts {
    /// Backed by `path` (created on first trust); `None` keeps keys in memory only.
    pub const fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            memory: Mutex::new(String::new()),
        }
    }

    fn read(&self) -> String {
        match &self.path {
            Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
            None => self.memory.lock().clone(),
        }
    }

    /// Looks up a key.
    pub fn verdict(&self, host: &HostKey) -> Verdict {
        check(&self.read(), &host.host, &host.algorithm, &host.key)
    }

    /// Trusts the key from now on, replacing an older key of the same type.
    // The lock is held on purpose until the file is written.
    #[allow(clippy::significant_drop_tightening)]
    pub fn trust(&self, host: &HostKey) -> io::Result<()> {
        let Some(path) = &self.path else {
            let mut memory = self.memory.lock();
            *memory = with_trusted(&memory, host);
            return Ok(());
        };
        // Held while writing: two trusts at once must not lose one of the keys.
        let _writing = self.memory.lock();
        let text = with_trusted(&std::fs::read_to_string(path).unwrap_or_default(), host);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write a sibling and rename, so a crash never leaves half a file.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(host: &str, data: &str) -> HostKey {
        HostKey {
            host: host.into(),
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:x".into(),
            key: data.into(),
        }
    }

    #[test]
    fn patterns_follow_openssh() {
        assert_eq!(host_pattern("nas", 22), "nas");
        assert_eq!(host_pattern("nas", 2222), "[nas]:2222");
    }

    #[test]
    fn known_unknown_and_changed_keys() {
        let text = "\
# comment
|1|hashed= ssh-ed25519 AAAA
nas,10.0.0.5 ssh-ed25519 KEY1 me@pc
[nas]:2222 ssh-ed25519 KEY2
other ecdsa-sha2-nistp256 KEY3
";
        assert_eq!(check(text, "nas", "ssh-ed25519", "KEY1"), Verdict::Trusted);
        assert_eq!(
            check(text, "10.0.0.5", "ssh-ed25519", "KEY1"),
            Verdict::Trusted
        );
        assert_eq!(check(text, "NAS", "ssh-ed25519", "KEY1"), Verdict::Trusted);
        assert_eq!(check(text, "nas", "ssh-ed25519", "EVIL"), Verdict::Changed);
        assert_eq!(
            check(text, "[nas]:2222", "ssh-ed25519", "KEY2"),
            Verdict::Trusted
        );
        assert_eq!(
            check(text, "[nas]:2223", "ssh-ed25519", "KEY2"),
            Verdict::Unknown
        );
        assert_eq!(
            check(text, "other", "ssh-ed25519", "KEY4"),
            Verdict::Unknown,
            "a key of another type is not a change"
        );
    }

    #[test]
    fn trusting_replaces_the_old_key_and_keeps_the_rest() {
        let text = "# mine\nnas ssh-ed25519 OLD\nnas ecdsa-sha2-nistp256 EC\nother ssh-ed25519 O\n";
        let updated = with_trusted(text, &key("nas", "NEW"));
        assert_eq!(
            updated,
            "# mine\nnas ecdsa-sha2-nistp256 EC\nother ssh-ed25519 O\nnas ssh-ed25519 NEW\n"
        );
        assert_eq!(
            check(&updated, "nas", "ssh-ed25519", "NEW"),
            Verdict::Trusted
        );
        assert_eq!(
            check(&updated, "nas", "ssh-ed25519", "OLD"),
            Verdict::Changed
        );
    }

    #[test]
    fn file_is_created_on_first_trust() {
        let dir = std::env::temp_dir().join(format!("arke-known-hosts-{}", std::process::id()));
        let path = dir.join("known_hosts");
        let _ = std::fs::remove_dir_all(&dir);
        let hosts = KnownHosts::new(Some(path.clone()));
        let k = key("[127.0.0.1]:2222", "DATA");
        assert_eq!(hosts.verdict(&k), Verdict::Unknown);
        hosts.trust(&k).unwrap();
        assert_eq!(hosts.verdict(&k), Verdict::Trusted);
        assert!(std::fs::read_to_string(&path).unwrap().contains("DATA"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
