//! Minimal HTTPS GET over WinHTTP, used for update checks and downloads.
//!
//! WinHTTP instead of a bundled TLS stack: it follows Windows' certificate store and proxy
//! settings (including auto-proxy) and keeps the binary small. WinHTTP follows redirects
//! itself (GitHub downloads redirect to another domain). Call from a background thread.

use std::io::Write;

use windows::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD,
    WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts,
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
    #[error("{}", .0.message())]
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

/// `https://host/path?q` -> (host, path?q). Only https is accepted.
fn split_url(url: &str) -> Result<(&str, &str), HttpError> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| HttpError::NotHttps {
            url: url.to_owned(),
        })?;
    Ok(match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    })
}

/// GETs `url` into `out`, calling `progress(read, total)` after each chunk. Any status
/// other than 200 is an error.
pub(crate) fn get(
    url: &str,
    headers: &[&str],
    out: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), HttpError> {
    let (host, path) = split_url(url)?;
    let agent = wide(&format!("Arke/{}", crate::build_info::VERSION));
    let host_w = wide(host);
    let path_w = wide(path);
    // SAFETY: all strings are NUL-terminated and outlive the calls; handles are owned by
    // Handle and closed in reverse creation order.
    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(windows::core::Error::from_thread().into());
        }
        // Resolve 10 s, connect 15 s, send and receive 30 s each.
        let _ = WinHttpSetTimeouts(session.0, 10_000, 15_000, 30_000, 30_000);
        let connect = Handle(WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
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
        if status != 200 {
            return Err(HttpError::Status(status));
        }
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
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
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
            out.write_all(&buf[..read as usize])
                .map_err(HttpError::Write)?;
            done += u64::from(read);
            progress(done, total);
        }
        Ok(())
    }
}

/// GETs `url` and returns the body as text.
pub(crate) fn get_text(url: &str, headers: &[&str]) -> Result<String, HttpError> {
    let mut body = Vec::new();
    get(url, headers, &mut body, &mut |_, _| {})?;
    Ok(String::from_utf8(body)?)
}

#[cfg(test)]
mod tests {
    use super::split_url;

    #[test]
    fn splits_https_urls() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b/releases?per_page=30").ok(),
            Some(("api.github.com", "/repos/a/b/releases?per_page=30"))
        );
        assert_eq!(
            split_url("https://example.com").ok(),
            Some(("example.com", "/"))
        );
        assert!(matches!(
            split_url("http://example.com/x"),
            Err(super::HttpError::NotHttps { .. })
        ));
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
