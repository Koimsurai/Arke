//! End to end over real SSH: the test server in [`crate::testing`] serves a temp folder and
//! `Remotes` browses and changes it through the public API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::EntryKind;
use arke_core::remote::RemoteUrl;
use russh::keys::PrivateKey;

use crate::testing::{PASSWORD, USER, serve_sftp as serve};
use crate::{HostKey, NoProgress, RemoteError, Remotes, RemotesConfig};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("arke-sftp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct NoHttp;

impl crate::dav::Transport for NoHttp {
    fn send(
        &self,
        _: crate::dav::Request<'_>,
        _: &mut dyn std::io::Write,
        _: crate::dav::Progress<'_>,
    ) -> crate::RemoteResult<u16> {
        Err(RemoteError::Timeout)
    }
}

fn remotes(cache: &Path) -> Arc<Remotes> {
    Remotes::new(RemotesConfig {
        known_hosts: None,
        cache_dir: cache.to_path_buf(),
        secrets: Arc::new(crate::creds::NoSecrets),
        transport: Arc::new(NoHttp),
    })
}

#[test]
fn browses_and_changes_a_real_sftp_server() {
    let root = temp_dir("root");
    let cache = temp_dir("cache");
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs").join("readme.txt"), b"hello over ssh").unwrap();
    std::fs::write(root.join("報告.txt"), vec![b'x'; 300_000]).unwrap();
    let port = serve(root.clone(), 7);
    let hub = remotes(&cache);
    let url = RemoteUrl::from_path(Path::new(&format!("sftp://{USER}@127.0.0.1:{port}/"))).unwrap();

    // First contact: the key must be confirmed.
    let key = match hub.list(&url) {
        Err(RemoteError::HostKeyUnknown(key)) => key,
        other => panic!("expected an unknown host key, got {other:?}"),
    };
    assert_eq!(key.host, format!("[127.0.0.1]:{port}"));
    assert!(key.fingerprint.starts_with("SHA256:"));
    hub.trust(&key).unwrap();

    // No password yet (and no agent key the test server accepts).
    assert!(matches!(hub.list(&url), Err(RemoteError::NeedsLogin)));
    assert!(matches!(
        hub.sign_in(&url, Some("wrong".into()), false),
        Err(RemoteError::LoginFailed)
    ));
    hub.sign_in(&url, Some(PASSWORD.into()), false).unwrap();

    let mut names: Vec<(String, EntryKind)> = hub
        .list(&url)
        .unwrap()
        .into_iter()
        .map(|e| (e.name.to_string(), e.kind))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("docs".to_owned(), EntryKind::Dir),
            ("報告.txt".to_owned(), EntryKind::File)
        ]
    );
    let report = url.join("報告.txt");
    assert_eq!(hub.stat(&report).unwrap().size, 300_000);
    assert_eq!(
        hub.read_head(&url.join("docs/readme.txt"), 5).unwrap(),
        b"hello"
    );
    assert!(matches!(
        hub.stat(&url.join("missing")),
        Err(RemoteError::NotFound)
    ));

    // Cached copy for previews: fetched once, reused while unchanged.
    let local = hub.fetch(&report, u64::MAX).unwrap();
    assert!(local.starts_with(&cache));
    assert_eq!(std::fs::metadata(&local).unwrap().len(), 300_000);
    assert!(matches!(
        hub.fetch(&report, 1000),
        Err(RemoteError::TooLarge)
    ));

    // Upload, new folder, rename (never over an existing name), delete.
    let mut data: &[u8] = b"uploaded bytes";
    hub.upload_file(
        Path::new("up.bin"),
        &url.join("docs/up.bin"),
        &mut data,
        14,
        &NoProgress,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(root.join("docs").join("up.bin")).unwrap(),
        b"uploaded bytes"
    );
    hub.mkdir(&url.join("new")).unwrap();
    assert!(root.join("new").is_dir());
    assert!(matches!(
        hub.rename(&url.join("docs/up.bin"), &url.join("docs/readme.txt")),
        Err(RemoteError::Exists)
    ));
    hub.rename(&url.join("docs/up.bin"), &url.join("new/moved.bin"))
        .unwrap();
    assert!(root.join("new").join("moved.bin").is_file());
    let down = cache.join("down.bin");
    hub.download_file(&url.join("new/moved.bin"), &down, &NoProgress)
        .unwrap();
    assert_eq!(std::fs::read(&down).unwrap(), b"uploaded bytes");
    hub.delete(&url.join("new"), &NoProgress).unwrap();
    assert!(
        !root.join("new").exists(),
        "folders are deleted with their contents"
    );

    // A dropped connection is reopened transparently.
    hub.disconnect(&url);
    assert_eq!(hub.list(&url).unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_changed_host_key_is_refused() {
    let root = temp_dir("changed");
    let cache = temp_dir("changed-cache");
    let port = serve(root.clone(), 9);
    let hub = remotes(&cache);
    let url = RemoteUrl::from_path(Path::new(&format!("sftp://{USER}@127.0.0.1:{port}/"))).unwrap();
    // Pretend another key was trusted for this server earlier.
    let other = PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &[1; 32],
    ));
    let openssh = other.public_key().to_openssh().unwrap();
    hub.trust(&HostKey {
        host: format!("[127.0.0.1]:{port}"),
        algorithm: "ssh-ed25519".into(),
        fingerprint: String::new(),
        key: openssh.split_whitespace().nth(1).unwrap().into(),
    })
    .unwrap();
    hub.sign_in(&url, Some(PASSWORD.into()), false).unwrap_err();
    assert!(matches!(
        hub.list(&url),
        Err(RemoteError::HostKeyChanged(_))
    ));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn unreachable_servers_fail_fast() {
    let cache = temp_dir("down");
    let hub = remotes(&cache);
    // Bind and drop: nothing listens on this port now.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = RemoteUrl::from_path(Path::new(&format!("sftp://u@127.0.0.1:{port}/"))).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(hub.list(&url), Err(RemoteError::Unreachable(_))));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let bad = RemoteUrl::from_path(Path::new("sftp://u@arke-test.invalid/")).unwrap();
    assert!(matches!(hub.list(&bad), Err(RemoteError::NoSuchHost(_))));
    let _ = std::fs::remove_dir_all(&cache);
}

/// WebDAV through the real WinHTTP transport against the local test server.
#[cfg(windows)]
#[test]
fn browses_and_changes_a_webdav_server_over_winhttp() {
    let root = temp_dir("dav-root");
    let cache = temp_dir("dav-cache");
    std::fs::create_dir(root.join("My Docs")).unwrap();
    std::fs::write(root.join("My Docs").join("報告 #1.txt"), b"dav says hi").unwrap();
    let port = crate::testing_dav::serve_dav(root.clone());
    let hub = Remotes::new(RemotesConfig {
        known_hosts: None,
        cache_dir: cache.clone(),
        secrets: Arc::new(crate::creds::NoSecrets),
        transport: Arc::new(crate::http::WinHttp::new().unwrap()),
    });
    let anonymous = RemoteUrl::from_path(Path::new(&format!("http://127.0.0.1:{port}/"))).unwrap();
    assert!(matches!(hub.list(&anonymous), Err(RemoteError::NeedsLogin)));
    let url = RemoteUrl::from_path(Path::new(&format!("http://{USER}@127.0.0.1:{port}/"))).unwrap();
    assert!(matches!(
        hub.sign_in(&url, Some("nope".into()), false),
        Err(RemoteError::LoginFailed)
    ));
    hub.sign_in(&url, Some(PASSWORD.into()), false).unwrap();
    let names: Vec<_> = hub
        .list(&url)
        .unwrap()
        .into_iter()
        .map(|e| (e.name.to_string(), e.kind))
        .collect();
    assert_eq!(names, [("My Docs".to_owned(), EntryKind::Dir)]);
    let docs = url.join("My Docs");
    let file = docs.join("報告 #1.txt");
    assert_eq!(hub.stat(&file).unwrap().size, 11);
    assert_eq!(hub.read_head(&file, 3).unwrap(), b"dav");
    let local = hub.fetch(&file, u64::MAX).unwrap();
    assert_eq!(std::fs::read(local).unwrap(), b"dav says hi");

    let mut data: &[u8] = b"put me";
    hub.upload_file(
        Path::new("x"),
        &docs.join("new.txt"),
        &mut data,
        6,
        &NoProgress,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(root.join("My Docs").join("new.txt")).unwrap(),
        b"put me"
    );
    hub.mkdir(&url.join("Sub")).unwrap();
    assert!(matches!(
        hub.mkdir(&url.join("Sub")),
        Err(RemoteError::Exists)
    ));
    assert!(matches!(
        hub.rename(&docs.join("new.txt"), &file),
        Err(RemoteError::Exists)
    ));
    hub.rename(&docs.join("new.txt"), &url.join("Sub/moved.txt"))
        .unwrap();
    assert!(root.join("Sub").join("moved.txt").is_file());
    hub.delete(&url.join("Sub"), &NoProgress).unwrap();
    assert!(!root.join("Sub").exists());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
}
