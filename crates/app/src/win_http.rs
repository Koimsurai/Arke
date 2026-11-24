//! Minimal HTTPS GET over WinHTTP, used for update checks and downloads.
//!
//! WinHTTP instead of a bundled TLS stack: it follows Windows' certificate store and proxy
//! settings (including auto-proxy) and keeps the binary small. WinHTTP follows redirects
//! itself (GitHub downloads redirect to another domain). Call from a background thread.

use std::io::Write;

use windows::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{PCWSTR, w};

/// Why an HTTPS GET failed. Display is the text shown in Settings > About.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpError {
    /// The URL is not `https://`.
    #[error("only https is supported: {url}")]
    NotHttps {
        /// The rejected URL.
        url: String,
    },
    /// A WinHTTP call failed (no network, TLS, proxy, timeout, ...).
    #[error("{}", describe(.0))]
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

/// The text for a failed WinHTTP call. WinHTTP's own codes (12001–12999: timeout, name not
/// resolved, can't connect, TLS) aren't in the system message table, so `message()` is empty for
/// them; their text is in winhttp.dll, in the Windows display language. The code if neither has it.
fn describe(error: &windows::core::Error) -> String {
    let system = error.message();
    if !system.trim().is_empty() {
        return system;
    }
    let code = error.code().0.cast_unsigned();
    if code >> 16 == 0x8007 && (12001..13000).contains(&(code & 0xFFFF)) {
        use windows::Win32::System::Diagnostics::Debug::{
            FORMAT_MESSAGE_FROM_HMODULE, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
        };
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        // SAFETY: winhttp.dll is loaded (the failed call came from it); the buffer outlives the
        // call and its length is passed.
        unsafe {
            if let Ok(module) = GetModuleHandleW(w!("winhttp.dll")) {
                let mut buf = [0u16; 512];
                let n = FormatMessageW(
                    FORMAT_MESSAGE_FROM_HMODULE | FORMAT_MESSAGE_IGNORE_INSERTS,
                    Some(module.0.cast_const()),
                    code & 0xFFFF,
                    0,
                    windows::core::PWSTR(buf.as_mut_ptr()),
                    buf.len() as u32,
                    None,
                ) as usize;
                let text = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
                if !text.trim().is_empty() {
                    return format!("{} ({})", text.trim(), code & 0xFFFF);
                }
            }
        }
    }
    format!("0x{code:08X}")
}

/// WinHTTP handle, closed on drop.
struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: returned by WinHttpOpen/Connect/OpenRequest and not yet closed.
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `https://host[:port]/path?q` -> (host, port, path?q). Only https is accepted.
fn split_url(url: &str) -> Result<(&str, u16, &str), HttpError> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| HttpError::NotHttps {
            url: url.to_owned(),
        })?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let default = INTERNET_DEFAULT_HTTPS_PORT;
    Ok(
        match authority
            .rsplit_once(':')
            .filter(|(host, _)| !host.ends_with(']') || authority.starts_with('['))
            .and_then(|(host, port)| port.parse::<u16>().ok().map(|p| (host, p)))
        {
            Some((host, port)) => (host, port, path),
            None => (authority, default, path),
        },
    )
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
    let (host, port, path) = split_url(url)?;
    let agent = wide(&format!("Arke/{}", crate::build_info::VERSION));
    let host_w = wide(host);
    let path_w = wide(path);
    let proxy_w = proxy.map(wide);
    // SAFETY: all strings are NUL-terminated and outlive the calls; handles are owned by
    // Handle and closed in reverse creation order.
    unsafe {
        let session = Handle(match &proxy_w {
            Some(proxy) => WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_NAMED_PROXY,
                PCWSTR(proxy.as_ptr()),
                w!("<local>"),
                0,
            ),
            None => WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
        });
        if session.0.is_null() {
            return Err(windows::core::Error::from_thread().into());
        }
        // Resolve 10 s, connect 15 s, send and receive 30 s each.
        let _ = WinHttpSetTimeouts(session.0, 10_000, 15_000, 30_000, 30_000);
        let connect = Handle(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), port, 0));
        if connect.0.is_null() {
            return Err(windows::core::Error::from_thread().into());
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(windows::core::Error::from_thread().into());
        }
        for header in headers {
            let h: Vec<u16> = header.encode_utf16().collect();
            WinHttpAddRequestHeaders(request.0, &h, WINHTTP_ADDREQ_FLAG_ADD)?;
        }
        WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;
        let mut status = 0u32;
        let mut size = u32::try_from(size_of::<u32>()).unwrap_or(4);
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &raw mut size,
            std::ptr::null_mut(),
        )?;
        // Content-Length, if present, for progress.
        let mut length = 0u32;
        let mut size = u32::try_from(size_of::<u32>()).unwrap_or(4);
        let total = WinHttpQueryHeaders(
            request.0,
            windows::Win32::Networking::WinHttp::WINHTTP_QUERY_CONTENT_LENGTH
                | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut length).cast()),
            &raw mut size,
            std::ptr::null_mut(),
        )
        .ok()
        .map(|()| u64::from(length));
        body.start(status, total)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                &raw mut read,
            )?;
            if read == 0 {
                break;
            }
            body.write(&buf[..read as usize])?;
        }
        Ok(())
    }
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
    use super::split_url;

    #[test]
    fn splits_https_urls() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b/releases?per_page=30").ok(),
            Some(("api.github.com", 443, "/repos/a/b/releases?per_page=30"))
        );
        assert_eq!(
            split_url("https://example.com").ok(),
            Some(("example.com", 443, "/"))
        );
        // A mirror on its own port (`update_route.rs`).
        assert_eq!(
            split_url("https://mirror.example.com:8443/https://github.com/x").ok(),
            Some(("mirror.example.com", 8443, "/https://github.com/x"))
        );
        assert!(matches!(
            split_url("http://example.com/x"),
            Err(super::HttpError::NotHttps { .. })
        ));
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
