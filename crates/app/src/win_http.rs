//! Minimal HTTPS GET over WinHTTP, used for update checks and downloads.
//!
//! WinHTTP instead of a bundled TLS stack: it follows Windows' certificate store and proxy
//! settings (including auto-proxy) and keeps the binary small. WinHTTP follows redirects
//! itself (GitHub downloads redirect to another domain). Call from a background thread.

use std::io::Write;

use arke_remote::http::{Session, Url, error_text, split_url};

/// Why an HTTPS GET failed. Display is the text shown in Settings > About.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpError {
    /// The URL is not `https://`.
    #[error("only https is supported: {url}")]
    NotHttps {
        /// The rejected URL.
        url: String,
    },
    /// The URL has no usable host or port.
    #[error("not a valid URL: {url}")]
    BadUrl {
        /// The rejected URL.
        url: String,
    },
    /// A WinHTTP call failed (no network, TLS, proxy, timeout, ...).
    #[error("{}", error_text(.0))]
    Windows(#[from] windows::core::Error),
    /// The server answered with a status other than 200.
    #[error("HTTP {0}")]
    Status(u32),
    /// The body could not be written to the destination.
    #[error("{0}")]
    Write(#[source] std::io::Error),
    /// The body is not UTF-8 text.
    #[error("{0}")]
    NotText(#[from] std::string::FromUtf8Error),
    /// The caller stopped the download.
    #[error("stopped")]
    Stopped,
}

/// `url` taken apart; only `https://` is accepted.
fn https_url(url: &str) -> Result<Url<'_>, HttpError> {
    match split_url(url) {
        Some(parts) if parts.secure => Ok(parts),
        Some(_) => Err(HttpError::NotHttps {
            url: url.to_owned(),
        }),
        None if url.starts_with("http://") => Err(HttpError::NotHttps {
            url: url.to_owned(),
        }),
        None => Err(HttpError::BadUrl {
            url: url.to_owned(),
        }),
    }
}

/// Where a response goes: told the status and length first, then given the body chunk by chunk.
trait Body {
    /// The status and `Content-Length` are in; an error stops before the body is read.
    fn start(&mut self, status: u32, length: Option<u64>) -> Result<(), HttpError>;
    fn write(&mut self, chunk: &[u8]) -> Result<(), HttpError>;
}

/// GETs `url` into `out`, calling `progress(read, total)` after each chunk. Any status
/// other than 200 is an error.
/// Through the HTTP proxy `proxy` (`host:port`) when given.
fn get_via(
    url: &str,
    headers: &[&str],
    out: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
    proxy: Option<&str>,
) -> Result<(), HttpError> {
    struct Plain<'a> {
        out: &'a mut dyn Write,
        progress: &'a mut dyn FnMut(u64, Option<u64>),
        done: u64,
        total: Option<u64>,
    }
    impl Body for Plain<'_> {
        fn start(&mut self, status: u32, length: Option<u64>) -> Result<(), HttpError> {
            if status != 200 {
                return Err(HttpError::Status(status));
            }
            self.total = length;
            Ok(())
        }
        fn write(&mut self, chunk: &[u8]) -> Result<(), HttpError> {
            self.out.write_all(chunk).map_err(HttpError::Write)?;
            self.done += chunk.len() as u64;
            (self.progress)(self.done, self.total);
            Ok(())
        }
    }
    fetch(
        url,
        headers,
        &mut Plain {
            out,
            progress,
            done: 0,
            total: None,
        },
        proxy,
    )
}

/// GETs `url` into `file`, continuing after what the file already holds (a download stopped
/// earlier): asks for the rest and appends it when the server sends only that (206), starts the
/// file over when it sends everything (200). `progress(done, total)` counts the whole file and
/// returns `false` to stop ([`HttpError::Stopped`]).
/// Through the HTTP proxy `proxy` (`host:port`) when given.
pub(crate) fn get_resumed_via(
    url: &str,
    file: &mut std::fs::File,
    progress: &mut dyn FnMut(u64, Option<u64>) -> bool,
    proxy: Option<&str>,
) -> Result<(), HttpError> {
    struct Resumed<'a> {
        file: &'a mut std::fs::File,
        progress: &'a mut dyn FnMut(u64, Option<u64>) -> bool,
        done: u64,
        total: Option<u64>,
        /// The server had nothing more to send (416): its error text isn't part of the file.
        complete: bool,
    }
    impl Body for Resumed<'_> {
        fn start(&mut self, status: u32, length: Option<u64>) -> Result<(), HttpError> {
            use std::io::{Seek, SeekFrom};
            match status {
                206 => {
                    self.total = length.map(|rest| rest + self.done);
                    self.file.seek(SeekFrom::End(0)).map_err(HttpError::Write)?;
                }
                200 => {
                    self.done = 0;
                    self.total = length;
                    self.file.set_len(0).map_err(HttpError::Write)?;
                    self.file
                        .seek(SeekFrom::Start(0))
                        .map_err(HttpError::Write)?;
                }
                // Nothing after what the file holds: it was complete already (the check of it
                // didn't run last time). The checksum tells whether it is right.
                416 if self.done > 0 => {
                    self.total = Some(self.done);
                    self.complete = true;
                }
                status => return Err(HttpError::Status(status)),
            }
            Ok(())
        }
        fn write(&mut self, chunk: &[u8]) -> Result<(), HttpError> {
            if self.complete {
                return Ok(());
            }
            self.file.write_all(chunk).map_err(HttpError::Write)?;
            self.done += chunk.len() as u64;
            if (self.progress)(self.done, self.total) {
                Ok(())
            } else {
                Err(HttpError::Stopped)
            }
        }
    }
    let have = file.metadata().map_err(HttpError::Write)?.len();
    let range = format!("Range: bytes={have}-");
    let headers: &[&str] = if have > 0 { &[&range] } else { &[] };
    fetch(
        url,
        headers,
        &mut Resumed {
            file,
            progress,
            done: have,
            total: None,
            complete: false,
        },
        proxy,
    )
}

/// GETs `url` into `body`, through the HTTP proxy `proxy` (`host:port`) when given, else the way
/// Windows is set up to connect (its proxy settings included).
fn fetch(
    url: &str,
    headers: &[&str],
    body: &mut dyn Body,
    proxy: Option<&str>,
) -> Result<(), HttpError> {
    let parts = https_url(url)?;
    // A stripped Windows can lack winhttp.dll; a delayed load of it would end the process.
    if !crate::win_dll::system_dll("winhttp.dll") {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::ERROR_MOD_NOT_FOUND.to_hresult(),
        )
        .into());
    }
    // Resolve 10 s, connect 15 s, send and receive 30 s each.
    let session = Session::open(
        &format!("Arke/{}", crate::build_info::VERSION),
        proxy,
        15_000,
    )?;
    let exchange = session.exchange(&parts, "GET")?;
    for header in headers {
        exchange.add_header(header)?;
    }
    exchange.send_empty()?;
    exchange.receive()?;
    let status = exchange.status()?;
    body.start(status, exchange.content_length())?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = exchange.read(&mut buf)?;
        if read == 0 {
            break;
        }
        body.write(&buf[..read])?;
    }
    Ok(())
}

/// GETs `url` and returns the body as text.
pub(crate) fn get_text(url: &str, headers: &[&str]) -> Result<String, HttpError> {
    get_text_via(url, headers, None)
}

/// [`get_text`] through the HTTP proxy `proxy` (`host:port`) when given.
pub(crate) fn get_text_via(
    url: &str,
    headers: &[&str],
    proxy: Option<&str>,
) -> Result<String, HttpError> {
    let mut body = Vec::new();
    get_via(url, headers, &mut body, &mut |_, _| {}, proxy)?;
    Ok(String::from_utf8(body)?)
}

#[cfg(test)]
mod tests {
    use super::{HttpError, https_url};

    /// Only `https://` URLs are fetched, and the ones that can't be taken apart are refused.
    #[test]
    fn only_https_urls_are_fetched() {
        assert!(https_url("https://api.github.com/repos/a/b/releases?per_page=30").is_ok());
        assert!(https_url("https://[::1]:8443/x").is_ok());
        assert!(matches!(
            https_url("http://example.com/x"),
            Err(HttpError::NotHttps { .. })
        ));
        assert!(matches!(
            https_url("https://example.com:port/x"),
            Err(HttpError::BadUrl { .. })
        ));
    }

    /// Without `winhttp.dll` a request fails with an error, before anything calls into the DLL.
    #[test]
    fn a_missing_winhttp_is_an_error() {
        crate::win_dll::assert_not_loaded(&["winhttp.dll"], "before the test");
        let _missing = crate::win_dll::pretend_missing(&["winhttp.dll"]);
        let error = super::get_text("https://example.invalid/", &[]).unwrap_err();
        assert!(matches!(error, super::HttpError::Windows(_)), "{error}");
        assert!(!error.to_string().is_empty());
        crate::win_dll::assert_not_loaded(&["winhttp.dll"], "after the gated call");
    }

    /// #343: a WinHTTP failure (here "name not resolved") says what went wrong instead of
    /// nothing; the system message table doesn't have WinHTTP's codes.
    #[test]
    fn a_winhttp_error_has_text() {
        let error =
            windows::core::Error::from(windows::core::HRESULT(0x8007_2EE7_u32.cast_signed()));
        // A real failure comes after a WinHTTP call, so winhttp.dll is loaded; same here.
        // SAFETY: takes no arguments.
        let _ = unsafe { windows::Win32::Networking::WinHttp::WinHttpCheckPlatform() };
        let text = super::HttpError::Windows(error).to_string();
        assert!(text.contains("12007"), "{text}");
        assert!(text.len() > "(12007)".len(), "{text}");
    }
}

/// Live GitHub request. Needs network, so ignored by default:
/// `cargo test -p arke-app -- --ignored reaches_github`.
#[cfg(test)]
mod network_tests {
    #[test]
    #[ignore = "needs network"]
    fn reaches_github() {
        let text = super::get_text(
            "https://api.github.com/repos/rust-lang/rust/releases?per_page=2",
            &["Accept: application/vnd.github+json"],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let list: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        assert!(list.as_array().is_some_and(|a| !a.is_empty()), "{text}");
    }
}
