//! A small WebDAV server for tests: plain HTTP on 127.0.0.1 over a folder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::HashMap;
use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::testing::{PASSWORD, USER};

/// Serves `root` as a small WebDAV server over plain HTTP on 127.0.0.1 (PROPFIND, GET with
/// Range, PUT, MKCOL, MOVE, DELETE), requiring Basic auth as [`USER`]/[`PASSWORD`]. Returns the
/// port; serves until the process exits.
pub fn serve_dav(root: PathBuf) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = connection(stream, &root);
            });
        }
    });
    port
}

fn connection(stream: std::net::TcpStream, root: &Path) -> std::io::Result<()> {
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return Ok(());
        };
        let (method, target) = (method.to_owned(), target.to_owned());
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            reader.read_line(&mut h)?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        reader.read_exact(&mut body)?;
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASSWORD}"))
        );
        let (status, reply) = if headers.get("authorization") == Some(&expected) {
            handle(&method, &target, &headers, &body, root)
        } else {
            (401, Vec::new())
        };
        let challenge = if status == 401 {
            "WWW-Authenticate: Basic realm=\"arke\"\r\n"
        } else {
            ""
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: application/xml\r\n{challenge}\r\n",
            reply.len(),
        );
        out.write_all(head.as_bytes())?;
        out.write_all(&reply)?;
        out.flush()?;
    }
}

fn server_path(target: &str) -> String {
    let path = target.find("://").map_or(target, |at| {
        let rest = &target[at + 3..];
        rest.find('/').map_or("/", |p| &rest[p..])
    });
    percent_encoding::percent_decode_str(path)
        .decode_utf8_lossy()
        .into_owned()
}

fn local(root: &Path, path: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        out.push(part);
    }
    out
}

fn response(href: &str, meta: &std::fs::Metadata) -> String {
    let kind = if meta.is_dir() {
        "<d:resourcetype><d:collection/></d:resourcetype>".to_owned()
    } else {
        format!(
            "<d:resourcetype/><d:getcontentlength>{}</d:getcontentlength>",
            meta.len()
        )
    };
    let href = percent_encoding::utf8_percent_encode(href, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        .replace("%2F", "/");
    format!(
        "<d:response><d:href>{href}</d:href><d:propstat><d:prop>{kind}\
         <d:getlastmodified>Tue, 10 Nov 2026 08:00:00 GMT</d:getlastmodified></d:prop>\
         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

fn handle(
    method: &str,
    target: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
    root: &Path,
) -> (u16, Vec<u8>) {
    let path = server_path(target);
    let file = local(root, &path);
    let status = |r: std::io::Result<()>, ok: u16| match r {
        Ok(()) => ok,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 404,
        Err(_) => 409,
    };
    match method {
        "PROPFIND" => {
            let Ok(meta) = std::fs::metadata(&file) else {
                return (404, Vec::new());
            };
            let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">"#);
            xml.push_str(&response(&path, &meta));
            if meta.is_dir() && headers.get("depth").map(String::as_str) == Some("1") {
                for item in std::fs::read_dir(&file).unwrap().flatten() {
                    let name = item.file_name().to_string_lossy().into_owned();
                    let href = format!("{}/{name}", path.trim_end_matches('/'));
                    xml.push_str(&response(&href, &item.metadata().unwrap()));
                }
            }
            xml.push_str("</d:multistatus>");
            (207, xml.into_bytes())
        }
        "GET" => match std::fs::read(&file) {
            Ok(mut bytes) => {
                if let Some(end) = headers
                    .get("range")
                    .and_then(|r| r.strip_prefix("bytes=0-"))
                    .and_then(|e| e.parse::<usize>().ok())
                {
                    bytes.truncate(end + 1);
                    return (206, bytes);
                }
                (200, bytes)
            }
            Err(_) => (404, Vec::new()),
        },
        "PUT" => (status(std::fs::write(&file, body), 201), Vec::new()),
        "MKCOL" if file.exists() => (405, Vec::new()),
        "MKCOL" => (status(std::fs::create_dir(&file), 201), Vec::new()),
        "DELETE" => {
            let r = if file.is_dir() {
                std::fs::remove_dir_all(&file)
            } else {
                std::fs::remove_file(&file)
            };
            (status(r, 204), Vec::new())
        }
        "MOVE" => {
            let Some(dest) = headers.get("destination") else {
                return (400, Vec::new());
            };
            let to = local(root, &server_path(dest));
            if to.exists() && headers.get("overwrite").map(String::as_str) == Some("F") {
                return (412, Vec::new());
            }
            (status(std::fs::rename(&file, &to), 201), Vec::new())
        }
        _ => (405, Vec::new()),
    }
}
