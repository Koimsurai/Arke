//! The one WinHTTP layer in Arke: Windows' certificate store and proxy settings apply, and no TLS
//! stack is bundled. Blocking; background threads only.
//!
//! [`Session`], [`Exchange`], [`split_url`] and [`error_text`] are what both users build on:
//! WebDAV's [`WinHttp`] transport here, and the update check and download in the app.

use std::io::Write;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_ADDREQ_FLAG_ADD,
    WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_QUERY_CONTENT_LENGTH,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WinHttpWriteData,
};
use windows::core::{PCWSTR, w};

use crate::dav::{Body, Progress, Request, Transport};
use crate::error::{RemoteError, RemoteResult};

/// WinHTTP handle, closed on drop.
struct Handle(*mut core::ffi::c_void);

impl Handle {
    /// `None` for the null handle a failed call returns.
    fn new(raw: *mut core::ffi::c_void) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: returned by WinHttpOpen/Connect/OpenRequest and not yet closed.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A URL taken apart for WinHTTP.
#[derive(Debug, PartialEq, Eq)]
pub struct Url<'a> {
    /// `https` rather than `http`.
    pub secure: bool,
    /// Host name or address. An IPv6 address has no brackets, which is how WinHTTP wants it
    /// (it puts them into the `Host` header itself).
    pub host: &'a str,
    /// Port, the scheme's default when the URL has none.
    pub port: u16,
    /// Path and query, `/` when empty.
    pub path: &'a str,
}

/// `http(s)://host[:port]/path?query` taken apart; `None` for any other scheme or a bad port.
#[must_use]
pub fn split_url(url: &str) -> Option<Url<'_>> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
    let default = if secure { 443 } else { 80 };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, tail) = v6.split_once(']')?;
        (host, tail.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = port.map_or(Some(default), |p| p.parse().ok())?;
    Some(Url {
        secure,
        host,
        port,
        path,
    })
}

/// The text for a failed WinHTTP call. WinHTTP's own codes (12001–12999: timeout, name not
/// resolved, can't connect, TLS) aren't in the system message table, so `message()` is empty for
/// them; their text is in winhttp.dll, in the Windows display language. The code if neither has it.
#[must_use]
pub fn error_text(error: &windows::core::Error) -> String {
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

/// WinHTTP error -> what the user should hear.
fn describe(err: &windows::core::Error, host: &str) -> RemoteError {
    // HRESULT_FROM_WIN32: facility 7, code in the low word.
    let hr = err.code().0 as u32;
    let code = if (hr >> 16) & 0x1fff == 7 {
        hr & 0xffff
    } else {
        hr
    };
    match code {
        12007 => RemoteError::NoSuchHost(host.to_owned()),
        12029 => RemoteError::Unreachable(host.to_owned()),
        12002 => RemoteError::Timeout,
        12030 | 12031 | 12017 => RemoteError::Lost,
        12037 | 12038 | 12044 | 12045 | 12057 | 12169 | 12175 => RemoteError::Certificate,
        _ => RemoteError::Server(error_text(err)),
    }
}

/// A WinHTTP session: the proxy setting and the timeouts every request on it gets.
pub struct Session(Handle);

// SAFETY: a WinHTTP session handle may be used from any thread (each request gets its own
// connect and request handles).
unsafe impl Send for Session {}
// SAFETY: as above; the session handle is only read after creation.
unsafe impl Sync for Session {}

impl Session {
    /// Opens a session (no network access yet): through the HTTP proxy `proxy` (`host:port`) when
    /// given, else the way Windows is set up to connect (its proxy settings included). Resolve
    /// 10 s, `connect_ms` to connect, send and receive 30 s each.
    pub fn open(agent: &str, proxy: Option<&str>, connect_ms: i32) -> windows::core::Result<Self> {
        let agent = wide(agent);
        let proxy = proxy.map(wide);
        // SAFETY: the strings are NUL-terminated and outlive the call.
        let raw = unsafe {
            match &proxy {
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
            }
        };
        let session = Handle::new(raw).ok_or_else(windows::core::Error::from_thread)?;
        // SAFETY: session is a valid handle.
        let _ = unsafe { WinHttpSetTimeouts(session.0, 10_000, connect_ms, 30_000, 30_000) };
        Ok(Self(session))
    }

    /// Connects to `url`'s host and opens `method` for its path (nothing is sent yet).
    pub fn exchange(&self, url: &Url<'_>, method: &str) -> windows::core::Result<Exchange> {
        let host = wide(url.host);
        let path = wide(url.path);
        let method = wide(method);
        // SAFETY: the strings are NUL-terminated and outlive the calls; the handles are owned by
        // Handle and closed in reverse creation order (Exchange drops the request first).
        unsafe {
            let connect = Handle::new(WinHttpConnect(self.0.0, PCWSTR(host.as_ptr()), url.port, 0))
                .ok_or_else(windows::core::Error::from_thread)?;
            let flags = if url.secure {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            };
            let request = Handle::new(WinHttpOpenRequest(
                connect.0,
                PCWSTR(method.as_ptr()),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            ))
            .ok_or_else(windows::core::Error::from_thread)?;
            Ok(Exchange {
                request,
                _connect: connect,
            })
        }
    }
}

/// One request and its response on a connection.
pub struct Exchange {
    /// Declared first so it is closed before its connection.
    request: Handle,
    _connect: Handle,
}

impl Exchange {
    /// Adds a `Name: value` header line.
    pub fn add_header(&self, line: &str) -> windows::core::Result<()> {
        let line: Vec<u16> = line.encode_utf16().collect();
        // SAFETY: valid request handle; the buffer is passed with its length.
        unsafe { WinHttpAddRequestHeaders(self.request.0, &line, WINHTTP_ADDREQ_FLAG_ADD) }
    }

    /// Sends the request without a body.
    pub fn send_empty(&self) -> windows::core::Result<()> {
        // SAFETY: valid request handle; no data is passed.
        unsafe { WinHttpSendRequest(self.request.0, None, None, 0, 0, 0) }
    }

    /// Sends the request with `bytes` as its body.
    pub fn send_bytes(&self, bytes: &[u8]) -> windows::core::Result<()> {
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        // SAFETY: valid request handle; the buffer outlives the call (the send is synchronous).
        unsafe {
            WinHttpSendRequest(
                self.request.0,
                None,
                Some(bytes.as_ptr().cast()),
                len,
                len,
                0,
            )
        }
    }

    /// Sends the headers of a request whose `total` body bytes follow through [`Self::write`].
    pub fn send_streamed(&self, total: u32) -> windows::core::Result<()> {
        // SAFETY: valid request handle; no data is passed.
        unsafe { WinHttpSendRequest(self.request.0, None, None, 0, total, 0) }
    }

    /// Sends the next part of a streamed body.
    pub fn write(&self, chunk: &[u8]) -> windows::core::Result<()> {
        let mut written = 0u32;
        // SAFETY: valid request handle; the buffer is valid for the call.
        unsafe {
            WinHttpWriteData(
                self.request.0,
                Some(chunk.as_ptr().cast()),
                u32::try_from(chunk.len()).unwrap_or(u32::MAX),
                &raw mut written,
            )
        }
    }

    /// Waits for the response headers (redirects are followed by WinHTTP itself).
    pub fn receive(&self) -> windows::core::Result<()> {
        // SAFETY: valid request handle; the request was sent.
        unsafe { WinHttpReceiveResponse(self.request.0, std::ptr::null_mut()) }
    }

    /// A numeric header (`info` already has the number flag).
    fn number(&self, info: u32) -> windows::core::Result<u32> {
        let mut value = 0u32;
        let mut size = u32::try_from(size_of::<u32>()).unwrap_or(4);
        // SAFETY: valid request handle; `value` is the 4 bytes of buffer `size` says.
        unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                info,
                PCWSTR::null(),
                Some((&raw mut value).cast()),
                &raw mut size,
                std::ptr::null_mut(),
            )?;
        }
        Ok(value)
    }

    /// The HTTP status of the response.
    pub fn status(&self) -> windows::core::Result<u32> {
        self.number(WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER)
    }

    /// `Content-Length` of the response, when it has one.
    #[must_use]
    pub fn content_length(&self) -> Option<u64> {
        self.number(WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER)
            .ok()
            .map(u64::from)
    }

    /// Reads the next part of the body into `buf`; 0 at its end.
    pub fn read(&self, buf: &mut [u8]) -> windows::core::Result<usize> {
        let mut read = 0u32;
        // SAFETY: valid request handle; the buffer is valid for the call.
        unsafe {
            WinHttpReadData(
                self.request.0,
                buf.as_mut_ptr().cast(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                &raw mut read,
            )?;
        }
        Ok(read as usize)
    }
}

/// HTTP over WinHTTP for WebDAV, one session for the app's lifetime.
pub struct WinHttp {
    session: Session,
}

/// Chunk size for uploads and downloads.
const CHUNK: usize = 256 * 1024;

impl WinHttp {
    /// Opens the session (no network access yet).
    pub fn new() -> RemoteResult<Self> {
        let session =
            Session::open("Arke", None, 10_000).map_err(|e| RemoteError::Server(error_text(&e)))?;
        Ok(Self { session })
    }
}

impl Transport for WinHttp {
    fn send(
        &self,
        request: Request<'_>,
        out: &mut dyn Write,
        progress: Progress<'_>,
    ) -> RemoteResult<u16> {
        let url = split_url(&request.url)
            .ok_or_else(|| RemoteError::Server(format!("bad URL {}", request.url)))?;
        let fail = |e: windows::core::Error| describe(&e, url.host);
        let exchange = self.session.exchange(&url, request.method).map_err(fail)?;
        for (name, value) in &request.headers {
            exchange
                .add_header(&format!("{name}: {value}"))
                .map_err(fail)?;
        }
        match request.body {
            Body::Empty => exchange.send_empty().map_err(fail)?,
            Body::Bytes(bytes) => exchange.send_bytes(bytes).map_err(fail)?,
            Body::Stream { reader, len } => {
                let total = u32::try_from(len).map_err(|_too_big| {
                    RemoteError::Server("files over 4 GB can't be uploaded over WebDAV".into())
                })?;
                exchange.send_streamed(total).map_err(fail)?;
                let mut buf = vec![0u8; CHUNK];
                let mut sent = 0u64;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    exchange.write(&buf[..n]).map_err(fail)?;
                    sent += n as u64;
                    if !progress(sent) {
                        return Err(RemoteError::Cancelled);
                    }
                }
            }
        }
        exchange.receive().map_err(fail)?;
        let status = u16::try_from(exchange.status().map_err(fail)?).unwrap_or(0);
        // Error bodies are HTML pages: not worth reading.
        if !(200..300).contains(&status) {
            return Ok(status);
        }
        let mut buf = vec![0u8; CHUNK];
        let mut done = 0u64;
        loop {
            let read = exchange.read(&mut buf).map_err(fail)?;
            if read == 0 {
                break;
            }
            out.write_all(&buf[..read])?;
            done += read as u64;
            if !progress(done) {
                return Err(RemoteError::Cancelled);
            }
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(url: &str) -> Option<(bool, &str, u16, &str)> {
        split_url(url).map(|u| (u.secure, u.host, u.port, u.path))
    }

    #[test]
    fn splits_urls() {
        assert_eq!(parts("https://h/a%20b/"), Some((true, "h", 443, "/a%20b/")));
        assert_eq!(
            parts("http://127.0.0.1:8080"),
            Some((false, "127.0.0.1", 8080, "/"))
        );
        assert_eq!(
            parts("https://api.github.com/repos/a/b/releases?per_page=30"),
            Some((
                true,
                "api.github.com",
                443,
                "/repos/a/b/releases?per_page=30"
            ))
        );
        // A mirror on its own port (`update_route.rs`), with a URL in its path.
        assert_eq!(
            parts("https://mirror.example.com:8443/https://github.com/x"),
            Some((true, "mirror.example.com", 8443, "/https://github.com/x"))
        );
        assert_eq!(parts("ftp://h/"), None);
        assert_eq!(parts("http://h:notaport/"), None);
    }

    /// An IPv6 address reaches WinHTTP without brackets, with or without a port.
    #[test]
    fn ipv6_hosts_lose_their_brackets() {
        assert_eq!(parts("http://[::1]:81/x"), Some((false, "::1", 81, "/x")));
        assert_eq!(parts("https://[::1]:8443/"), Some((true, "::1", 8443, "/")));
        assert_eq!(
            parts("https://[fe80::1]"),
            Some((true, "fe80::1", 443, "/"))
        );
        assert_eq!(parts("https://[::1"), None);
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
        let text = error_text(&error);
        assert!(text.contains("12007"), "{text}");
        assert!(text.len() > "(12007)".len(), "{text}");
    }
}
