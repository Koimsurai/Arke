//! WebDAV's HTTP transport over WinHTTP: Windows' certificate store and proxy settings apply,
//! and no TLS stack is bundled. Blocking; background threads only.

use std::io::Write;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
    WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts, WinHttpWriteData,
};
use windows::core::PCWSTR;

use crate::dav::{Body, Progress, Request, Transport};
use crate::error::{RemoteError, RemoteResult};

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

/// `https://host:port/path` -> (secure, host, port, path).
fn split(url: &str) -> Option<(bool, String, u16, &str)> {
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
    Some((secure, host.to_owned(), port, path))
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
        _ => RemoteError::Server(err.message()),
    }
}

/// HTTP over WinHTTP, one session for the app's lifetime.
pub struct WinHttp {
    session: Handle,
}

// SAFETY: a WinHTTP session handle may be used from any thread (each request gets its own
// connect and request handles).
unsafe impl Send for WinHttp {}
// SAFETY: as above; the session handle is only read after creation.
unsafe impl Sync for WinHttp {}

/// Chunk size for uploads and downloads.
const CHUNK: usize = 256 * 1024;

impl WinHttp {
    /// Opens the session (no network access yet).
    pub fn new() -> RemoteResult<Self> {
        let agent = wide("Arke");
        // SAFETY: agent is NUL-terminated and outlives the call.
        let session = Handle(unsafe {
            WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        });
        if session.0.is_null() {
            return Err(RemoteError::Server(
                windows::core::Error::from_thread().message(),
            ));
        }
        // Resolve 10 s, connect 10 s, send and receive 30 s each.
        // SAFETY: session is a valid handle.
        let _ = unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000) };
        Ok(Self { session })
    }
}

impl Transport for WinHttp {
    #[allow(clippy::too_many_lines)]
    fn send(
        &self,
        request: Request<'_>,
        out: &mut dyn Write,
        progress: Progress<'_>,
    ) -> RemoteResult<u16> {
        let (secure, host, port, path) = split(&request.url)
            .ok_or_else(|| RemoteError::Server(format!("bad URL {}", request.url)))?;
        let fail = |e: windows::core::Error| describe(&e, &host);
        let host_w = wide(&host);
        let path_w = wide(path);
        let method_w = wide(request.method);
        // SAFETY: all strings are NUL-terminated and outlive the calls; handles are owned by
        // Handle and closed in reverse creation order; buffers passed to WinHTTP are valid
        // for each call.
        unsafe {
            let connect = Handle(WinHttpConnect(
                self.session.0,
                PCWSTR(host_w.as_ptr()),
                port,
                0,
            ));
            if connect.0.is_null() {
                return Err(fail(windows::core::Error::from_thread()));
            }
            let flags = if secure {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            };
            let req = Handle(WinHttpOpenRequest(
                connect.0,
                PCWSTR(method_w.as_ptr()),
                PCWSTR(path_w.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            ));
            if req.0.is_null() {
                return Err(fail(windows::core::Error::from_thread()));
            }
            for (name, value) in &request.headers {
                let line: Vec<u16> = format!("{name}: {value}").encode_utf16().collect();
                WinHttpAddRequestHeaders(req.0, &line, WINHTTP_ADDREQ_FLAG_ADD).map_err(fail)?;
            }
            match request.body {
                Body::Empty => WinHttpSendRequest(req.0, None, None, 0, 0, 0).map_err(fail)?,
                Body::Bytes(bytes) => {
                    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                    WinHttpSendRequest(req.0, None, Some(bytes.as_ptr().cast()), len, len, 0)
                        .map_err(fail)?;
                }
                Body::Stream { reader, len } => {
                    let total = u32::try_from(len).map_err(|_| {
                        RemoteError::Server("files over 4 GB can't be uploaded over WebDAV".into())
                    })?;
                    WinHttpSendRequest(req.0, None, None, 0, total, 0).map_err(fail)?;
                    let mut buf = vec![0u8; CHUNK];
                    let mut sent = 0u64;
                    loop {
                        let n = reader.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        let mut written = 0u32;
                        WinHttpWriteData(
                            req.0,
                            Some(buf.as_ptr().cast()),
                            u32::try_from(n).unwrap_or(u32::MAX),
                            &raw mut written,
                        )
                        .map_err(fail)?;
                        sent += n as u64;
                        if !progress(sent) {
                            return Err(RemoteError::Cancelled);
                        }
                    }
                }
            }
            WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(fail)?;
            let mut status = 0u32;
            let mut size = u32::try_from(size_of::<u32>()).unwrap_or(4);
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut status).cast()),
                &raw mut size,
                std::ptr::null_mut(),
            )
            .map_err(fail)?;
            let status = u16::try_from(status).unwrap_or(0);
            // Error bodies are HTML pages: not worth reading.
            if !(200..300).contains(&status) {
                return Ok(status);
            }
            let mut buf = vec![0u8; CHUNK];
            let mut done = 0u64;
            loop {
                let mut read = 0u32;
                WinHttpReadData(
                    req.0,
                    buf.as_mut_ptr().cast(),
                    u32::try_from(buf.len()).unwrap_or(u32::MAX),
                    &raw mut read,
                )
                .map_err(fail)?;
                if read == 0 {
                    break;
                }
                out.write_all(&buf[..read as usize])?;
                done += u64::from(read);
                if !progress(done) {
                    return Err(RemoteError::Cancelled);
                }
            }
            Ok(status)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        assert_eq!(
            split("https://h/a%20b/"),
            Some((true, "h".into(), 443, "/a%20b/"))
        );
        assert_eq!(
            split("http://127.0.0.1:8080"),
            Some((false, "127.0.0.1".into(), 8080, "/"))
        );
        assert_eq!(
            split("http://[::1]:81/x"),
            Some((false, "::1".into(), 81, "/x"))
        );
        assert_eq!(split("ftp://h/"), None);
    }
}
