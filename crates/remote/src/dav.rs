//! WebDAV client (RFC 4918 subset): PROPFIND depth 1 for listing, GET, PUT, MKCOL, MOVE and
//! DELETE. HTTP itself goes through a [`Transport`] (WinHTTP in Arke, so Windows' certificate
//! store and proxy settings apply).
//!
//! Only Basic authentication, and only over HTTPS (or to this computer): Basic sends the password
//! as plain base64.

use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime};

use arke_core::remote::{RemoteUrl, Scheme};
use arke_core::{Entry, EntryKind};
use base64::Engine as _;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::error::{RemoteError, RemoteResult};

/// Request body.
pub enum Body<'a> {
    /// No body.
    Empty,
    /// Small body in memory (PROPFIND XML).
    Bytes(&'a [u8]),
    /// Streamed upload of exactly `len` bytes.
    Stream {
        /// Source of the bytes.
        reader: &'a mut dyn Read,
        /// Content-Length.
        len: u64,
    },
}

/// One HTTP request.
pub struct Request<'a> {
    /// `PROPFIND`, `GET`, ...
    pub method: &'static str,
    /// Absolute URL, already percent-encoded.
    pub url: String,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
    /// Body.
    pub body: Body<'a>,
}

/// Progress callback: bytes moved so far in this request (sent or received); return `false`
/// to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// Sends HTTP requests. Implementations block; call from background threads only.
pub trait Transport: Send + Sync + 'static {
    /// Sends `request`, writes the response body to `out` and returns the status code.
    fn send(
        &self,
        request: Request<'_>,
        out: &mut dyn Write,
        progress: Progress<'_>,
    ) -> RemoteResult<u16>;
}

/// Characters left as they are in a path segment (RFC 3986 unreserved).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Full request URL for a server path (each segment percent-encoded).
pub fn request_url(url: &RemoteUrl, path: &str, dir: bool) -> String {
    let mut out = format!("{}://{}", url.scheme.as_str(), url.host_port());
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.extend(utf8_percent_encode(segment, SEGMENT));
    }
    if dir || path.trim_matches('/').is_empty() {
        out.push('/');
    }
    out
}

/// Server path of an `href` (absolute URL or absolute path), decoded, without trailing `/`.
pub fn href_path(href: &str) -> String {
    let href = href.trim();
    let path = match href.find("://") {
        Some(at) => href[at + 3..]
            .find('/')
            .map_or("/", |p| &href[at + 3 + p..]),
        None => href,
    };
    let decoded = percent_decode_str(path).decode_utf8_lossy();
    let trimmed = decoded.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` (RFC 7231 IMF-fixdate).
pub fn parse_http_date(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let rest = text.split_once(',').map_or(text, |(_, r)| r);
    let mut it = rest.split_whitespace();
    let day: u32 = it.next()?.parse().ok()?;
    let month = match it.next()?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':').map(str::parse::<u64>);
    let (h, m, s) = (hms.next()?.ok()?, hms.next()?.ok()?, hms.next()?.ok()?);
    if day == 0 || day > 31 || h > 23 || m > 59 || s > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + m * 60 + s;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One `<response>` of a multistatus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DavItem {
    /// Server path, decoded.
    pub path: String,
    /// A collection (folder).
    pub is_dir: bool,
    /// `getcontentlength`.
    pub size: u64,
    /// `getlastmodified`.
    pub modified: Option<SystemTime>,
}

impl DavItem {
    /// As a directory entry named after the last path segment.
    pub fn to_entry(&self) -> Entry {
        let name = self.path.rsplit('/').next().unwrap_or_default();
        if self.is_dir {
            Entry::new(name, EntryKind::Dir, 0, self.modified, 0)
        } else {
            Entry::new(name, EntryKind::File, self.size, self.modified, 0)
        }
    }
}

const DAV: &str = "DAV:";

/// A `DAV:` element named `name`.
fn is(n: &roxmltree::Node<'_, '_>, name: &str) -> bool {
    n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(DAV)
}

/// First `DAV:` child element named `name`.
fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children().find(|c| is(c, name))
}

/// Parses a PROPFIND multistatus body. Responses without a successful propstat are skipped.
pub fn parse_multistatus(xml: &str) -> RemoteResult<Vec<DavItem>> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| RemoteError::Server(format!("invalid PROPFIND response: {e}")))?;
    let mut items = Vec::new();
    for response in doc.descendants().filter(|n| is(n, "response")) {
        let Some(href) = child(response, "href").and_then(|h| h.text()) else {
            continue;
        };
        let ok = response
            .children()
            .filter(|n| is(n, "propstat"))
            .find(|ps| {
                child(*ps, "status")
                    .and_then(|s| s.text())
                    .is_some_and(|s| s.split_whitespace().nth(1) == Some("200"))
            });
        let Some(prop) = ok.and_then(|ps| child(ps, "prop")) else {
            continue;
        };
        let text = |name: &str| child(prop, name).and_then(|n| n.text()).map(str::trim);
        items.push(DavItem {
            path: href_path(href),
            is_dir: child(prop, "resourcetype").is_some_and(|rt| child(rt, "collection").is_some()),
            size: text("getcontentlength")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            modified: text("getlastmodified").and_then(parse_http_date),
        });
    }
    Ok(items)
}

const PROPFIND_BODY: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

/// Whether `host` is this computer (Basic auth over http is allowed there).
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A WebDAV server, as one user.
pub struct DavClient {
    url: RemoteUrl,
    password: Option<String>,
    transport: std::sync::Arc<dyn Transport>,
}

/// Writes at most `left` bytes and then reports "full" by returning a short write of 0, which
/// the transport treats as "stop reading".
struct Limited<'a> {
    out: &'a mut Vec<u8>,
    left: usize,
}

impl Write for Limited<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(self.left);
        self.out.extend_from_slice(&buf[..n]);
        self.left -= n;
        if self.left == 0 {
            // Enough: stop the transfer.
            return Err(io::Error::new(io::ErrorKind::WriteZero, "limit reached"));
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl DavClient {
    /// Client for the server of `url`, signing in as `url.user` with `password`.
    pub fn new(
        url: &RemoteUrl,
        password: Option<String>,
        transport: std::sync::Arc<dyn Transport>,
    ) -> Self {
        Self {
            url: url.with_path("/"),
            password,
            transport,
        }
    }

    fn headers(&self) -> RemoteResult<Vec<(&'static str, String)>> {
        let Some(user) = &self.url.user else {
            return Ok(Vec::new());
        };
        if self.url.scheme == Scheme::Dav && !is_loopback(&self.url.host) {
            return Err(RemoteError::Insecure);
        }
        let pair = format!("{user}:{}", self.password.as_deref().unwrap_or_default());
        Ok(vec![(
            "Authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            ),
        )])
    }

    /// Maps a status code to an error (`Ok` for 2xx).
    fn status(&self, status: u16) -> RemoteResult<()> {
        match status {
            200..=299 => Ok(()),
            401 if self.url.user.is_some() => Err(RemoteError::LoginFailed),
            401 => Err(RemoteError::NeedsLogin),
            403 => Err(RemoteError::Denied),
            404 | 410 => Err(RemoteError::NotFound),
            405 | 412 => Err(RemoteError::Exists),
            507 => Err(RemoteError::Server("insufficient storage".into())),
            other => Err(RemoteError::Server(format!("HTTP {other}"))),
        }
    }

    fn send(
        &self,
        method: &'static str,
        path: &str,
        dir: bool,
        mut headers: Vec<(&'static str, String)>,
        body: Body<'_>,
        out: &mut dyn Write,
        progress: Progress<'_>,
    ) -> RemoteResult<u16> {
        let url = request_url(&self.url, path, dir);
        headers.extend(self.headers()?);
        self.transport.send(
            Request {
                method,
                url,
                headers,
                body,
            },
            out,
            progress,
        )
    }

    fn propfind(&self, path: &str, depth: &str) -> RemoteResult<Vec<DavItem>> {
        let mut body = Vec::new();
        let status = self.send(
            "PROPFIND",
            path,
            depth == "1",
            vec![
                ("Depth", depth.to_owned()),
                ("Content-Type", "application/xml; charset=utf-8".to_owned()),
            ],
            Body::Bytes(PROPFIND_BODY),
            &mut body,
            &mut |_| true,
        )?;
        self.status(status)?;
        parse_multistatus(&String::from_utf8_lossy(&body))
    }

    /// Lists a folder (PROPFIND depth 1), without the folder itself.
    pub fn list(&self, path: &str) -> RemoteResult<Vec<Entry>> {
        let own = href_path(path);
        Ok(self
            .propfind(path, "1")?
            .iter()
            .filter(|item| item.path != own)
            .map(DavItem::to_entry)
            .collect())
    }

    /// One item (PROPFIND depth 0).
    pub fn stat(&self, path: &str) -> RemoteResult<Entry> {
        self.propfind(path, "0")?
            .first()
            .map(DavItem::to_entry)
            .ok_or(RemoteError::NotFound)
    }

    /// Up to `len` bytes from the start (a Range request; servers that ignore it are cut off).
    pub fn read_head(&self, path: &str, len: u64) -> RemoteResult<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut sink = Limited {
            out: &mut bytes,
            left: usize::try_from(len).unwrap_or(usize::MAX).max(1),
        };
        let range = format!("bytes=0-{}", len.saturating_sub(1));
        let status = self.send(
            "GET",
            path,
            false,
            vec![("Range", range)],
            Body::Empty,
            &mut sink,
            &mut |_| true,
        );
        match status {
            Ok(status) => self.status(status)?,
            // The limit stopped the transfer: that's the head we wanted.
            Err(RemoteError::Local(e)) if e.kind() == io::ErrorKind::WriteZero => {}
            Err(e) => return Err(e),
        }
        bytes.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        Ok(bytes)
    }

    /// Downloads into `out`.
    pub fn download(
        &self,
        path: &str,
        out: &mut dyn Write,
        progress: Progress<'_>,
    ) -> RemoteResult<()> {
        let status = self.send("GET", path, false, Vec::new(), Body::Empty, out, progress)?;
        self.status(status)
    }

    /// Uploads `len` bytes from `reader`, replacing an existing file.
    pub fn upload(
        &self,
        path: &str,
        reader: &mut dyn Read,
        len: u64,
        progress: Progress<'_>,
    ) -> RemoteResult<()> {
        let status = self.send(
            "PUT",
            path,
            false,
            vec![("Content-Type", "application/octet-stream".to_owned())],
            Body::Stream { reader, len },
            &mut io::sink(),
            progress,
        )?;
        self.status(status)
    }

    /// Creates a folder.
    pub fn mkdir(&self, path: &str) -> RemoteResult<()> {
        let status = self.send(
            "MKCOL",
            path,
            true,
            Vec::new(),
            Body::Empty,
            &mut io::sink(),
            &mut |_| true,
        )?;
        self.status(status)
    }

    /// Moves or renames; never overwrites.
    pub fn rename(&self, from: &str, to: &str, is_dir: bool) -> RemoteResult<()> {
        let destination = request_url(&self.url, to, is_dir);
        let status = self.send(
            "MOVE",
            from,
            is_dir,
            vec![("Destination", destination), ("Overwrite", "F".to_owned())],
            Body::Empty,
            &mut io::sink(),
            &mut |_| true,
        )?;
        self.status(status)
    }

    /// Deletes a file, or a folder with everything in it.
    pub fn delete(&self, path: &str, is_dir: bool) -> RemoteResult<()> {
        let status = self.send(
            "DELETE",
            path,
            is_dir,
            Vec::new(),
            Body::Empty,
            &mut io::sink(),
            &mut |_| true,
        )?;
        self.status(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    const NEXTCLOUD: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/bob/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>
   <d:getlastmodified>Tue, 10 Nov 2026 08:00:00 GMT</d:getlastmodified></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/bob/My%20Photos/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>https://cloud.example.com/remote.php/dav/files/bob/%E5%A0%B1%E5%91%8A.txt</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>1234</d:getcontentlength>
   <d:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</d:getlastmodified></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/bob/gone.txt</d:href>
  <d:status>HTTP/1.1 404 Not Found</d:status>
 </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_multistatus_with_prefixes_and_partial_propstats() {
        let items = parse_multistatus(NEXTCLOUD).unwrap();
        assert_eq!(items.len(), 3, "the 404 response is skipped");
        assert_eq!(items[0].path, "/remote.php/dav/files/bob");
        assert!(items[0].is_dir);
        assert_eq!(items[1].path, "/remote.php/dav/files/bob/My Photos");
        assert!(items[1].is_dir);
        let file = &items[2];
        assert_eq!(file.path, "/remote.php/dav/files/bob/報告.txt");
        assert!(!file.is_dir);
        assert_eq!(file.size, 1234);
        assert_eq!(
            file.modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777))
        );
        let entry = file.to_entry();
        assert_eq!(&*entry.name, "報告.txt");
        assert_eq!(entry.kind, EntryKind::File);
    }

    #[test]
    fn parses_default_namespace_documents() {
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/a/b.bin</href>
<propstat><prop><getcontentlength>7</getcontentlength></prop><status>HTTP/1.1 200 OK</status></propstat>
</response></multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items[0].path, "/a/b.bin");
        assert_eq!(items[0].size, 7);
        assert!(parse_multistatus("<not xml").is_err());
    }

    #[test]
    fn http_dates() {
        assert_eq!(
            parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert!(
            parse_http_date("Sat, 29 Feb 2025 12:00:00 GMT").is_some(),
            "lenient about impossible dates, never panics"
        );
        assert_eq!(parse_http_date("yesterday"), None);
        assert_eq!(parse_http_date("Sun, 06 Foo 1994 08:49:37 GMT"), None);
    }

    #[test]
    fn maps_paths_to_urls_and_back() {
        let url = RemoteUrl::from_path(std::path::Path::new("https://h:8443/dav")).unwrap();
        assert_eq!(
            request_url(&url, "/dav/a b/報告#1.txt", false),
            "https://h:8443/dav/a%20b/%E5%A0%B1%E5%91%8A%231.txt"
        );
        assert_eq!(
            request_url(&url, "/dav/dir", true),
            "https://h:8443/dav/dir/"
        );
        assert_eq!(request_url(&url, "/", false), "https://h:8443/");
        assert_eq!(href_path("/dav/a%20b/"), "/dav/a b");
        assert_eq!(href_path("https://h:8443/dav/x"), "/dav/x");
        assert_eq!(href_path("https://h:8443"), "/");
        // Round trip
        let path = "/dav/100% sure/[x]";
        assert_eq!(href_path(&request_url(&url, path, false)), path);
    }

    /// Method, URL and headers of a request.
    type Seen = (String, String, Vec<(&'static str, String)>);

    /// Records requests and answers from a script.
    struct Script {
        seen: Mutex<Vec<Seen>>,
        reply: Mutex<Vec<(u16, Vec<u8>)>>,
    }

    impl Transport for Script {
        fn send(
            &self,
            request: Request<'_>,
            out: &mut dyn Write,
            _: Progress<'_>,
        ) -> RemoteResult<u16> {
            self.seen.lock().push((
                request.method.to_owned(),
                request.url.clone(),
                request.headers,
            ));
            let (status, body) = self.reply.lock().remove(0);
            out.write_all(&body)?;
            Ok(status)
        }
    }

    fn script(replies: Vec<(u16, &[u8])>) -> Arc<Script> {
        Arc::new(Script {
            seen: Mutex::default(),
            reply: Mutex::new(replies.into_iter().map(|(s, b)| (s, b.to_vec())).collect()),
        })
    }

    #[test]
    fn lists_without_the_folder_itself_and_maps_statuses() {
        let t = script(vec![(207, NEXTCLOUD.as_bytes()), (401, b""), (404, b"")]);
        let url =
            RemoteUrl::from_path(std::path::Path::new("https://bob@cloud.example.com/")).unwrap();
        let dav = DavClient::new(&url, Some("pw".into()), t.clone());
        let entries = dav.list("/remote.php/dav/files/bob/").unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, ["My Photos", "報告.txt"]);
        let seen = t.seen.lock()[0].clone();
        assert_eq!(seen.0, "PROPFIND");
        assert_eq!(
            seen.1,
            "https://cloud.example.com/remote.php/dav/files/bob/"
        );
        assert!(seen.2.contains(&("Depth", "1".to_owned())));
        assert!(
            seen.2
                .iter()
                .any(|(k, v)| *k == "Authorization" && v == "Basic Ym9iOnB3")
        );
        assert!(matches!(dav.list("/x"), Err(RemoteError::LoginFailed)));
        assert!(matches!(dav.stat("/x"), Err(RemoteError::NotFound)));
    }

    #[test]
    fn refuses_basic_auth_over_plain_http_except_locally() {
        let t = script(vec![(201, b"")]);
        let remote = RemoteUrl::from_path(std::path::Path::new("http://u@nas/")).unwrap();
        assert!(matches!(
            DavClient::new(&remote, None, t.clone()).mkdir("/a"),
            Err(RemoteError::Insecure)
        ));
        let local = RemoteUrl::from_path(std::path::Path::new("http://u@127.0.0.1:8080/")).unwrap();
        assert!(DavClient::new(&local, None, t).mkdir("/a").is_ok());
        let anonymous = RemoteUrl::from_path(std::path::Path::new("http://nas/")).unwrap();
        let t = script(vec![(401, b"")]);
        assert!(matches!(
            DavClient::new(&anonymous, None, t).mkdir("/a"),
            Err(RemoteError::NeedsLogin)
        ));
    }

    #[test]
    fn move_sends_an_encoded_destination_without_overwrite() {
        let t = script(vec![(201, b""), (412, b"")]);
        let url = RemoteUrl::from_path(std::path::Path::new("https://h/")).unwrap();
        let dav = DavClient::new(&url, None, t.clone());
        dav.rename("/a/old name.txt", "/a/new name.txt", false)
            .unwrap();
        let seen = t.seen.lock()[0].clone();
        assert_eq!(seen.0, "MOVE");
        assert_eq!(seen.1, "https://h/a/old%20name.txt");
        assert!(
            seen.2
                .contains(&("Destination", "https://h/a/new%20name.txt".to_owned()))
        );
        assert!(seen.2.contains(&("Overwrite", "F".to_owned())));
        assert!(matches!(
            dav.rename("/a/x", "/a/y", false),
            Err(RemoteError::Exists)
        ));
    }
}
