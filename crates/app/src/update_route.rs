//! The update download route (#249): Settings › About › "Download route", for where GitHub is
//! slow or blocked. One field; what is typed decides what it is:
//!
//! - `https://mirror.example.com/`: a prefix put in front of each GitHub address
//!   (`https://mirror.example.com/https://github.com/...`), as GitHub mirror services take them.
//! - `127.0.0.1:7890` (a host and a port): an HTTP proxy, such as a local proxy app's HTTP or
//!   mixed port. WinHTTP has no SOCKS, so a SOCKS address is refused with that said.
//!
//! Only checking for and downloading updates use it; remote places, the API and everything else
//! connect as before. Arke ships no route of its own and never picks one (`arke-network-principles`):
//! it's empty until the user types one. Nothing is trusted because of the route: downloads and
//! notes are installed or shown only with Arke's signature (`update_sig.rs`).
//!
//! The route goes first; when it fails, the same request goes to GitHub directly, and Settings
//! says so ([`last`]), never silently.

use std::sync::LazyLock;

use parking_lot::RwLock;

/// A parsed route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// No route: GitHub directly.
    Direct,
    /// Put in front of the GitHub address; ends in `/`.
    Prefix(String),
    /// An HTTP proxy, `host:port`.
    Proxy(String),
}

/// Why a typed route can't be used. Display is the text under the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RouteError {
    /// `http://` and other schemes: a prefix must be HTTPS.
    #[error("{}", rust_i18n::t!("update.route_not_https"))]
    NotHttps,
    /// A SOCKS proxy, which WinHTTP can't use.
    #[error("{}", rust_i18n::t!("update.route_socks"))]
    Socks,
    /// Neither an https address nor a host and port.
    #[error("{}", rust_i18n::t!("update.route_bad"))]
    Unrecognized,
}

impl Route {
    /// What `text` (the field as typed) is.
    pub(crate) fn parse(text: &str) -> Result<Self, RouteError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self::Direct);
        }
        let lower = text.to_ascii_lowercase();
        if lower.starts_with("socks") {
            return Err(RouteError::Socks);
        }
        if let Some(rest) = lower.strip_prefix("https://") {
            let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
            if host.is_empty() || text.contains(char::is_whitespace) {
                return Err(RouteError::Unrecognized);
            }
            let mut prefix = text.to_owned();
            if !prefix.ends_with('/') {
                prefix.push('/');
            }
            return Ok(Self::Prefix(prefix));
        }
        if lower.contains("://") {
            return Err(RouteError::NotHttps);
        }
        if is_host_port(text) {
            return Ok(Self::Proxy(text.to_owned()));
        }
        Err(RouteError::Unrecognized)
    }

    /// The address to ask for `url` (a GitHub address).
    pub(crate) fn url(&self, url: &str) -> String {
        match self {
            Self::Prefix(prefix) => format!("{prefix}{url}"),
            Self::Direct | Self::Proxy(_) => url.to_owned(),
        }
    }

    /// The HTTP proxy to go through, if this route is one.
    pub(crate) fn proxy(&self) -> Option<&str> {
        match self {
            Self::Proxy(proxy) => Some(proxy),
            Self::Direct | Self::Prefix(_) => None,
        }
    }

    /// The host the route sends requests to, for Settings (`mirror.example.com`, `127.0.0.1:7890`).
    pub(crate) fn host(&self) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::Prefix(prefix) => prefix
                .strip_prefix("https://")
                .and_then(|rest| rest.split('/').next())
                .map(str::to_owned),
            Self::Proxy(proxy) => Some(proxy.clone()),
        }
    }
}

/// `host:port`: a name, an IPv4 address or a bracketed IPv6 address, and a port 1–65535.
fn is_host_port(text: &str) -> bool {
    let Some((host, port)) = text.rsplit_once(':') else {
        return false;
    };
    let port_ok = port.parse::<u16>().is_ok_and(|p| p > 0);
    let host_ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    };
    port_ok && host_ok
}

/// The route updates use now (Settings, kept in step by `main.rs`).
static CURRENT: LazyLock<RwLock<Route>> = LazyLock::new(|| RwLock::new(Route::Direct));

/// What the last update request went through, for Settings.
static LAST: LazyLock<RwLock<Option<Used>>> = LazyLock::new(|| RwLock::new(None));

/// Sets the route from the field's text; one that can't be parsed means GitHub directly (the
/// field shows why).
pub(crate) fn set(text: &str) {
    let route = Route::parse(text).unwrap_or(Route::Direct);
    let changed = {
        let mut current = CURRENT.write();
        let changed = *current != route;
        *current = route;
        changed
    };
    if changed {
        *LAST.write() = None;
    }
}

/// The route updates use now.
pub(crate) fn current() -> Route {
    CURRENT.read().clone()
}

/// How an update request went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Used {
    /// Through the route (its host).
    Route(String),
    /// The route (its host) failed with this error; GitHub directly answered instead.
    FellBack(String, String),
}

/// The way the last update request went, when a route is set.
pub(crate) fn last() -> Option<Used> {
    LAST.read().clone()
}

/// The route (`host`) gave something that didn't hold up (`reason`) and GitHub was asked
/// directly instead.
pub(crate) fn fell_back(host: &str, reason: &str) {
    note(Used::FellBack(host.to_owned(), reason.to_owned()));
}

fn note(used: Used) {
    *LAST.write() = Some(used);
}

/// Runs `request(address, proxy)` through the current route and, when that fails, once more to
/// GitHub directly; remembers which it was ([`last`]). Without a route, just GitHub.
pub(crate) fn fetch<T, E: std::fmt::Display>(
    url: &str,
    mut request: impl FnMut(&str, Option<&str>) -> Result<T, E>,
) -> Result<T, E> {
    let route = current();
    let Some(host) = route.host() else {
        return request(url, None);
    };
    match request(&route.url(url), route.proxy()) {
        Ok(value) => {
            note(Used::Route(host));
            Ok(value)
        }
        Err(err) => {
            log::info!("update route {host} failed ({err}), asking GitHub directly");
            let direct = request(url, None);
            if direct.is_ok() {
                note(Used::FellBack(host, err.to_string()));
            }
            direct
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_https_address_is_a_prefix() {
        assert_eq!(
            Route::parse(" https://mirror.example.com "),
            Ok(Route::Prefix("https://mirror.example.com/".into()))
        );
        let route = Route::parse("https://mirror.example.com/gh/").unwrap();
        assert_eq!(
            route.url("https://github.com/o/r/releases/download/v1/a.exe"),
            "https://mirror.example.com/gh/https://github.com/o/r/releases/download/v1/a.exe"
        );
        assert_eq!(route.host().as_deref(), Some("mirror.example.com"));
        assert_eq!(route.proxy(), None);
    }

    #[test]
    fn a_host_and_port_is_an_http_proxy() {
        for text in ["127.0.0.1:7890", "proxy.example.com:8080", "[::1]:7890"] {
            let route = Route::parse(text).unwrap();
            assert_eq!(route.proxy(), Some(text), "{text}");
            assert_eq!(route.url("https://github.com/x"), "https://github.com/x");
        }
    }

    #[test]
    fn empty_is_github_directly() {
        assert_eq!(Route::parse("  "), Ok(Route::Direct));
        assert_eq!(Route::Direct.host(), None);
    }

    /// What can't be used says why instead of being taken for something else.
    #[test]
    fn unusable_routes_say_why() {
        assert_eq!(
            Route::parse("http://mirror.example.com/"),
            Err(RouteError::NotHttps)
        );
        assert_eq!(
            Route::parse("socks5://127.0.0.1:7891"),
            Err(RouteError::Socks)
        );
        for text in [
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:99999",
            "mirror",
            "https://",
            "a b:80",
        ] {
            assert_eq!(Route::parse(text), Err(RouteError::Unrecognized), "{text}");
        }
    }

    /// The route first; when it fails, GitHub directly, and that is remembered for Settings.
    #[test]
    fn a_failing_route_falls_back_to_github_and_says_so() {
        set("127.0.0.1:7890");
        let mut asked = Vec::new();
        let got: Result<&str, String> = fetch("https://github.com/x", |url, proxy| {
            asked.push((url.to_owned(), proxy.map(str::to_owned)));
            if proxy.is_some() {
                Err("connection refused".into())
            } else {
                Ok("body")
            }
        });
        assert_eq!(got, Ok("body"));
        assert_eq!(
            asked,
            [
                ("https://github.com/x".into(), Some("127.0.0.1:7890".into())),
                ("https://github.com/x".into(), None)
            ]
        );
        assert_eq!(
            last(),
            Some(Used::FellBack(
                "127.0.0.1:7890".into(),
                "connection refused".into()
            ))
        );
        set("");
        assert_eq!(last(), None);
        let direct: Result<&str, String> = fetch("https://github.com/x", |_, proxy| {
            assert_eq!(proxy, None);
            Ok("body")
        });
        assert_eq!(direct, Ok("body"));
    }
}
