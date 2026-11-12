//! A small SFTP server for tests (russh's own server side) serving a folder on 127.0.0.1, so
//! remote locations can be checked end to end without installing anything.

// The server traits' methods are async; most handlers here do plain file I/O.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unused_async_trait_impl,
    missing_docs
)]

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::remote::RemoteUrl;
use russh::keys::PrivateKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

/// The only account.
pub const USER: &str = "arke";
/// Its password.
pub const PASSWORD: &str = "correct horse";

struct Ssh {
    root: PathBuf,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.remove(&id) {
            Some(channel) if name == "sftp" => {
                session.channel_success(id)?;
                let sftp = Sftp {
                    root: self.root.clone(),
                    handles: HashMap::new(),
                    next: 0,
                };
                russh_sftp::server::run(channel.into_stream(), sftp).await;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

enum Open {
    File(std::fs::File),
    Dir(Option<Vec<File>>),
}

struct Sftp {
    root: PathBuf,
    handles: HashMap<String, Open>,
    next: u32,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn code(err: &std::io::Error) -> StatusCode {
    match err.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl Sftp {
    fn local(&self, path: &str) -> PathBuf {
        let mut out = self.root.clone();
        for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
            out.push(part);
        }
        out
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let h = self.next.to_string();
        self.handles.insert(h.clone(), open);
        h
    }
}

fn attrs(path: &Path) -> Result<FileAttributes, StatusCode> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| code(&e))?;
    Ok(FileAttributes::from(&meta))
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.local(&filename);
        let file = std::fs::File::options()
            .read(flags.contains(OpenFlags::READ))
            .write(flags.contains(OpenFlags::WRITE))
            .create(flags.contains(OpenFlags::CREATE))
            .truncate(flags.contains(OpenFlags::TRUNCATE))
            .open(path)
            .map_err(|e| code(&e))?;
        Ok(Handle {
            id,
            handle: self.handle(Open::File(file)),
        })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(Open::File(file)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).map_err(|e| code(&e))?;
        let mut data = vec![0; len as usize];
        let n = file.read(&mut data).map_err(|e| code(&e))?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(n);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(Open::File(file)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).map_err(|e| code(&e))?;
        file.write_all(&data).map_err(|e| code(&e))?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs {
            id,
            attrs: attrs(&self.local(&path))?,
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(Open::File(file)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let meta = file.metadata().map_err(|e| code(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let dir = self.local(&path);
        let mut files = Vec::new();
        for item in std::fs::read_dir(&dir).map_err(|e| code(&e))? {
            let item = item.map_err(|e| code(&e))?;
            files.push(File::new(
                item.file_name().to_string_lossy(),
                attrs(&item.path())?,
            ));
        }
        Ok(Handle {
            id,
            handle: self.handle(Open::Dir(Some(files))),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(files)) => match files.take() {
                Some(files) if !files.is_empty() => Ok(Name { id, files }),
                _ => Err(StatusCode::Eof),
            },
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.local(&filename)).map_err(|e| code(&e))?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.local(&path)).map_err(|e| code(&e))?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.local(&path)).map_err(|e| code(&e))?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let url = RemoteUrl::from_path(Path::new(&format!("sftp://h/{path}"))).unwrap();
        Ok(Name {
            id,
            files: vec![File::dummy(url.path)],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        std::fs::rename(self.local(&oldpath), self.local(&newpath)).map_err(|e| code(&e))?;
        Ok(ok(id))
    }
}

/// Serves `root` over SFTP on 127.0.0.1 from its own runtime thread; returns the port. The
/// host key comes from `seed`.
pub fn serve_sftp(root: PathBuf, seed: u8) -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let key = PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
                &[seed; 32],
            ));
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: std::time::Duration::from_millis(10),
                auth_rejection_time_initial: Some(std::time::Duration::ZERO),
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let handler = Ssh {
                    root: root.clone(),
                    channels: HashMap::new(),
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
    });
    rx.recv().unwrap()
}
