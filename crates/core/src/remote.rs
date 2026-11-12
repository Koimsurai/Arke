//! Remote locations (SFTP, WebDAV) as paths.
//!
//! A remote folder travels through Arke as an ordinary `PathBuf` holding its URL, like
//! `sftp://alice@nas:2222/home/alice` or `https://dav.example.com/files`. History, tabs, the
//! sidebar and the directory service then work unchanged; only the few places that take paths
//! apart (path bar input, parent, breadcrumbs) ask this module first.
//!
//! The canonical form uses `/` and no trailing slash (the root is just `sftp://host`), omits
//! the default port, and keeps names as they are (no percent-encoding). `Path::join` adds `\`, so parsing
//! accepts both separators. Nothing here touches the network.

use std::fmt;
use std::path::{Path, PathBuf};

/// Protocol of a remote location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// SSH file transfer (`sftp://`).
    Sftp,
    /// WebDAV over plain HTTP (`http://`).
    Dav,
    /// WebDAV over HTTPS (`https://`).
    Davs,
}

impl Scheme {
    /// Port used when the URL names none.
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Sftp => 22,
            Self::Dav => 80,
            Self::Davs => 443,
        }
    }

    /// URL scheme as written in the canonical form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Dav => "http",
            Self::Davs => "https",
        }
    }

    /// WebDAV (either transport).
    pub const fn is_dav(self) -> bool {
        matches!(self, Self::Dav | Self::Davs)
    }

    fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "sftp" | "ssh" => Some(Self::Sftp),
            "http" | "dav" | "webdav" => Some(Self::Dav),
            "https" | "davs" | "webdavs" => Some(Self::Davs),
            _ => None,
        }
    }
}

/// A parsed remote location.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteUrl {
    /// Protocol.
    pub scheme: Scheme,
    /// User name, if the URL names one.
    pub user: Option<String>,
    /// Host name or address (IPv6 without brackets).
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Absolute path on the server, `/`-separated, never empty.
    pub path: String,
}

/// Whether the path is a remote location (cheap prefix check, no full parse).
pub fn is_remote(path: &Path) -> bool {
    path.to_str().is_some_and(|s| split_scheme(s).is_some())
}

/// `scheme://rest` -> (scheme, rest). Also accepts `scheme:\\rest`, which is what a URL becomes
/// after a round trip through Windows path normalization.
fn split_scheme(s: &str) -> Option<(Scheme, &str)> {
    let colon = s.find(':')?;
    let scheme = Scheme::parse(&s[..colon])?;
    let rest = &s[colon + 1..];
    let rest = rest
        .strip_prefix("//")
        .or_else(|| rest.strip_prefix("\\\\"))?;
    Some((scheme, rest))
}

/// Decodes `%XX` escapes; invalid escapes are kept as typed.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

const fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Resolves `.`, `..`, repeated and trailing separators into `/a/b`.
fn normalize_path(raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::with_capacity(raw.len() + 1);
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

impl RemoteUrl {
    /// Parses a path that holds a remote URL (canonical or joined with `\`). Names are taken
    /// literally; `None` for local paths and malformed URLs.
    pub fn from_path(path: &Path) -> Option<Self> {
        Self::parse(path.to_str()?, false)
    }

    /// Parses what the user typed or pasted: like [`Self::from_path`], but `%20`-style escapes
    /// in the path are decoded, since URLs copied from browsers carry them.
    pub fn parse_input(text: &str) -> Option<Self> {
        Self::parse(text.trim().trim_matches('"').trim(), true)
    }

    fn parse(s: &str, decode: bool) -> Option<Self> {
        let (scheme, rest) = split_scheme(s)?;
        let end = rest.find(['/', '\\']).unwrap_or(rest.len());
        let (authority, raw_path) = rest.split_at(end);
        let (user, host_port) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };
        let user = user
            .map(|u| {
                if decode {
                    percent_decode(u)
                } else {
                    u.to_owned()
                }
            })
            .filter(|u| !u.is_empty());
        let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
            let close = v6.find(']')?;
            let port = &v6[close + 1..];
            (&v6[..close], port.strip_prefix(':'))
        } else {
            match host_port.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (host_port, None),
            }
        };
        if host.is_empty() || host.contains(['/', '\\', '@', ' ']) {
            return None;
        }
        let port = match port {
            None | Some("") => scheme.default_port(),
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
        };
        let path = if decode {
            percent_decode(raw_path)
        } else {
            raw_path.to_owned()
        };
        Some(Self {
            scheme,
            user,
            host: host.to_ascii_lowercase(),
            port,
            path: normalize_path(&path),
        })
    }

    /// The canonical path form.
    pub fn to_path(&self) -> PathBuf {
        PathBuf::from(self.to_string())
    }

    /// `scheme://user@host:port`: identifies the server connection (no path).
    pub fn origin(&self) -> String {
        let mut s = format!("{}://", self.scheme.as_str());
        if let Some(user) = &self.user {
            s.push_str(user);
            s.push('@');
        }
        s.push_str(&self.host_port());
        s
    }

    /// `host`, `host:port` for a non-default port, IPv6 in brackets.
    pub fn host_port(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.scheme.default_port() {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// The server's root folder.
    pub fn is_root(&self) -> bool {
        self.path == "/"
    }

    /// Last path segment; `None` at the root.
    pub fn name(&self) -> Option<&str> {
        (!self.is_root()).then(|| self.path.rsplit('/').next().unwrap_or_default())
    }

    /// Parent folder; `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let cut = self.path.rfind('/').unwrap_or(0);
        Some(Self {
            path: if cut == 0 {
                "/".into()
            } else {
                self.path[..cut].to_owned()
            },
            ..self.clone()
        })
    }

    /// Child (or relative path, `/`-separated; `..` allowed).
    #[must_use]
    pub fn join(&self, rel: &str) -> Self {
        Self {
            path: normalize_path(&format!("{}/{rel}", self.path)),
            ..self.clone()
        }
    }

    /// Same server, another absolute path.
    #[must_use]
    pub fn with_path(&self, path: &str) -> Self {
        Self {
            path: normalize_path(path),
            ..self.clone()
        }
    }

    /// Whether both are on the same server connection (same scheme, user, host and port).
    pub fn same_origin(&self, other: &Self) -> bool {
        self.scheme == other.scheme
            && self.user == other.user
            && self.host == other.host
            && self.port == other.port
    }

    /// Path bar segments: the server first, then each folder.
    pub fn breadcrumbs(&self) -> Vec<(String, PathBuf)> {
        let mut out = vec![(self.host_port(), self.with_path("/").to_path())];
        let mut acc = self.with_path("/");
        for part in self.path.split('/').filter(|p| !p.is_empty()) {
            acc = acc.join(part);
            out.push((part.to_owned(), acc.to_path()));
        }
        out
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The root has no trailing slash, so `Path::parent` of a joined child gives back
        // exactly this form.
        if self.is_root() {
            write!(f, "{}", self.origin())
        } else {
            write!(f, "{}{}", self.origin(), self.path)
        }
    }
}

/// Canonical form of a remote path; local paths come back unchanged.
pub fn canonical(path: PathBuf) -> PathBuf {
    match RemoteUrl::from_path(&path) {
        Some(url) => url.to_path(),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> RemoteUrl {
        RemoteUrl::from_path(Path::new(s)).unwrap()
    }

    #[test]
    fn parses_sftp_urls() {
        let u = url("sftp://alice@nas.local:2222/home/alice/");
        assert_eq!(u.scheme, Scheme::Sftp);
        assert_eq!(u.user.as_deref(), Some("alice"));
        assert_eq!(u.host, "nas.local");
        assert_eq!(u.port, 2222);
        assert_eq!(u.path, "/home/alice");
        assert_eq!(u.to_string(), "sftp://alice@nas.local:2222/home/alice");

        let bare = url("sftp://NAS");
        assert_eq!(bare.user, None);
        assert_eq!(bare.port, 22);
        assert_eq!(bare.path, "/");
        assert_eq!(bare.to_string(), "sftp://nas", "default port left out");
    }

    #[test]
    fn parses_webdav_urls() {
        let u = url("https://dav.example.com/remote.php/dav/files/bob");
        assert_eq!(u.scheme, Scheme::Davs);
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/remote.php/dav/files/bob");
        assert_eq!(url("http://h:8080/x").port, 8080);
        assert_eq!(url("davs://h/x").scheme, Scheme::Davs);
        assert_eq!(url("webdav://h/x").to_string(), "http://h/x");
    }

    #[test]
    fn handles_ipv6_and_users_with_at_signs() {
        let u = url("sftp://[::1]:2200/srv");
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 2200);
        assert_eq!(u.to_string(), "sftp://[::1]:2200/srv");
        let u = url("sftp://me@corp.com@host/x");
        assert_eq!(u.user.as_deref(), Some("me@corp.com"));
        assert_eq!(u.host, "host");
    }

    #[test]
    fn rejects_non_urls() {
        for bad in [
            r"C:\Users",
            r"\\nas\share",
            "sftp:/one-slash",
            "ftp://h/x",
            "sftp://",
            "sftp://h:99999/x",
            "sftp://h:0/x",
            "sftp://bad host/x",
        ] {
            assert!(RemoteUrl::from_path(Path::new(bad)).is_none(), "{bad}");
        }
        assert!(is_remote(Path::new("sftp://h/x")));
        assert!(is_remote(Path::new("HTTPS://h")));
        assert!(!is_remote(Path::new(r"C:\sftp")));
    }

    #[test]
    fn joins_with_backslashes_like_path_join_does() {
        let base = url("sftp://h/home");
        let joined = base.to_path().join("docs").join("a b.txt");
        let back = RemoteUrl::from_path(&joined).unwrap();
        assert_eq!(back.path, "/home/docs/a b.txt");
        assert_eq!(
            canonical(joined),
            PathBuf::from("sftp://h/home/docs/a b.txt")
        );
        // Path::parent of a joined child is the canonical parent, at the root too.
        let root = url("sftp://h/").to_path();
        assert_eq!(root, PathBuf::from("sftp://h"));
        assert_eq!(root.join("x").parent(), Some(root.as_path()));
        let dir = url("sftp://h/a").to_path();
        assert_eq!(dir.join("x").parent(), Some(dir.as_path()));
        assert_eq!(dir.parent(), Some(root.as_path()));
        assert_eq!(
            canonical(PathBuf::from(r"C:\x")),
            PathBuf::from(r"C:\x"),
            "local paths unchanged"
        );
    }

    #[test]
    fn walks_up_to_the_root_and_no_further() {
        let u = url("sftp://h/a/b");
        assert_eq!(u.name(), Some("b"));
        let up = u.parent().unwrap();
        assert_eq!(up.path, "/a");
        let root = up.parent().unwrap();
        assert!(root.is_root());
        assert_eq!(root.name(), None);
        assert!(root.parent().is_none());
        assert_eq!(u.join("../c/./d").path, "/a/c/d");
        assert_eq!(u.join("../../../..").path, "/", "never above the root");
    }

    #[test]
    fn typed_urls_are_percent_decoded_but_paths_are_literal() {
        let typed = RemoteUrl::parse_input(" \"https://h/My%20Files/100%\" ").unwrap();
        assert_eq!(typed.path, "/My Files/100%");
        let literal = url("https://h/a%20b");
        assert_eq!(literal.path, "/a%20b", "a file may really be named a%20b");
    }

    #[test]
    fn breadcrumbs_start_at_the_server() {
        let crumbs = url("sftp://u@h:2222/srv/data").breadcrumbs();
        let labels: Vec<_> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["h:2222", "srv", "data"]);
        assert_eq!(crumbs[0].1, PathBuf::from("sftp://u@h:2222"));
        assert_eq!(crumbs[2].1, PathBuf::from("sftp://u@h:2222/srv/data"));
    }

    #[test]
    fn origin_identifies_the_connection() {
        let a = url("sftp://u@h/a");
        let b = url("sftp://u@h:22/b");
        assert!(a.same_origin(&b));
        assert_eq!(a.origin(), "sftp://u@h");
        assert!(!a.same_origin(&url("sftp://v@h/a")));
    }
}
