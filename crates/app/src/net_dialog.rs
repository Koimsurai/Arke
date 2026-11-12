//! Network location dialogs, drawn by Arke in place of the Windows credential prompt and Map
//! Network Drive wizard.
//!
//! Kinds:
//! - **Sign in**: asks for credentials when `\\server\share` won't open.
//! - **Add network location** (sidebar Network "+"): a shared folder with a drive letter (SMB),
//!   or an SFTP or WebDAV server, saved to the sidebar.
//! - **Remote sign-in**: an SFTP or WebDAV location that needs a password, a key passphrase or
//!   a host key confirmation.
//!
//! The password lives only in this dialog's input and the one call that submits it: never logged,
//! never in settings, not copyable. With "Remember" checked it goes to Windows Credential Manager
//! (same as the Windows prompt); Arke stores nothing. Connecting runs in the background and errors
//! are shown in the dialog for another try. SMB connections last for this sign-in only (no
//! "reconnect at sign-in"); the dialog can hand off to the Windows dialog for that.
//!
//! SSH host keys: on the first connection the dialog shows the server's fingerprint and connects
//! only after "Trust and connect". A key that changed since is shown as a warning in the danger
//! color; replacing it takes a separate, explicit click.

use std::path::PathBuf;
use std::sync::Arc;

use arke_core::remote::{RemoteUrl, Scheme};
use arke_remote::{HostKey, RemoteError, Remotes};
use gpui::{
    AnimationExt, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyBinding, SharedString, Subscription, Window, actions, div, prelude::*, px,
};
use rust_i18n::t;

use crate::a11y::A11yExt as _;
use crate::settings::RemotePlace;
use crate::shell::{NetConnect, Shell};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

actions!(net_dialog, [NetDialogCancel]);

const CONTEXT: &str = "NetDialog";

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", NetDialogCancel, Some(CONTEXT))]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NetDialogKind {
    /// Sign in to this share
    Login { share: PathBuf },
    /// Add a network location: map a drive (`letters` are the unused drive letters) or add an
    /// SFTP or WebDAV server
    Map { letters: Vec<char> },
    /// Sign in to an SFTP or WebDAV location (password, passphrase or host key)
    RemoteLogin {
        url: PathBuf,
        key_file: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NetDialogEvent {
    /// Connected
    Connected,
    /// Connected to an SFTP or WebDAV server; `save` adds it to the sidebar.
    RemoteConnected { place: RemotePlace, save: bool },
    /// Canceled or closed
    Closed,
    /// Use the Windows "Map network drive" dialog instead
    UseSystemDialog,
}

/// Which kind of location the add dialog is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Smb,
    Sftp,
    WebDav,
}

/// A host key waiting for the user's decision.
struct PendingKey {
    key: HostKey,
    /// Changed since last time (possible attack), not just new.
    changed: bool,
}

pub(crate) struct NetDialog {
    kind: NetDialogKind,
    shell: Arc<dyn Shell>,
    remotes: Arc<Remotes>,
    focus: FocusHandle,
    folder: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    path: Entity<TextInput>,
    key_file: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    protocol: Protocol,
    /// SFTP: sign in with a private key file instead of a password.
    use_key: bool,
    letter: Option<char>,
    remember: bool,
    busy: bool,
    error: Option<SharedString>,
    pending_key: Option<PendingKey>,
    _inputs: Vec<Subscription>,
}

impl EventEmitter<NetDialogEvent> for NetDialog {}

impl NetDialog {
    pub(crate) fn new(
        kind: NetDialogKind,
        shell: Arc<dyn Shell>,
        remotes: Arc<Remotes>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let folder = cx.new(|cx| TextInput::new(r"\\server\share", cx));
        let host = cx.new(|cx| TextInput::new(t!("net.host_hint").to_string(), cx));
        let port = cx.new(|cx| TextInput::new("22", cx));
        let path = cx.new(|cx| TextInput::new("/", cx));
        let key_file = cx.new(|cx| TextInput::new(r"C:\Users\me\.ssh\id_ed25519", cx));
        let user = cx.new(|cx| TextInput::new(t!("net.user_hint").to_string(), cx));
        let password = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.set_masked(true, cx);
            input
        });
        let inputs = [&folder, &host, &port, &path, &key_file, &user, &password]
            .into_iter()
            .map(|input| cx.subscribe_in(input, window, Self::on_input))
            .collect();
        let letter = match &kind {
            // Pick from the end (Z, Y, ...) like Windows does.
            NetDialogKind::Map { letters } => letters.last().copied(),
            _ => None,
        };
        let mut protocol = Protocol::Smb;
        let mut use_key = false;
        if let NetDialogKind::RemoteLogin { url, key_file: key } = &kind
            && let Some(url) = RemoteUrl::from_path(url)
        {
            protocol = if url.scheme == Scheme::Sftp {
                Protocol::Sftp
            } else {
                Protocol::WebDav
            };
            host.update(cx, |i, cx| i.set_text(url.host.clone(), cx));
            port.update(cx, |i, cx| i.set_text(url.port.to_string(), cx));
            path.update(cx, |i, cx| i.set_text(url.path.clone(), cx));
            if let Some(name) = &url.user {
                user.update(cx, |i, cx| i.set_text(name.clone(), cx));
            }
            if let Some(key) = key {
                use_key = true;
                key_file.update(cx, |i, cx| {
                    i.set_text(key.to_string_lossy().into_owned(), cx);
                });
            }
        }
        // Ready to type right away: sign-in focuses the user name (or the password when the user
        // is known), mapping focuses the folder.
        let first = match &kind {
            NetDialogKind::Login { .. } => user.focus_handle(cx),
            NetDialogKind::Map { .. } => folder.focus_handle(cx),
            NetDialogKind::RemoteLogin { .. } if user.read(cx).text().is_empty() => {
                user.focus_handle(cx)
            }
            NetDialogKind::RemoteLogin { .. } => password.focus_handle(cx),
        };
        window.focus(&first, cx);
        Self {
            kind,
            shell,
            remotes,
            focus: cx.focus_handle(),
            folder,
            host,
            port,
            path,
            key_file,
            user,
            password,
            protocol,
            use_key,
            letter,
            remember: false,
            busy: false,
            error: None,
            pending_key: None,
            _inputs: inputs,
        }
    }

    /// Enter submits and Esc cancels in the inputs.
    fn on_input(
        &mut self,
        input: &Entity<TextInput>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Confirm => self.submit(cx),
            InputEvent::Cancel => self.cancel(cx),
            InputEvent::Changed => {
                // Another server or account: a pending host key no longer applies.
                if (input == &self.host || input == &self.port) && self.pending_key.take().is_some()
                {
                    cx.notify();
                }
                if self.error.take().is_some() {
                    cx.notify();
                }
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.clear_password(cx);
        cx.emit(NetDialogEvent::Closed);
    }

    fn clear_password(&mut self, cx: &mut Context<Self>) {
        self.password.update(cx, |input, cx| input.set_text("", cx));
    }

    const fn is_remote(&self) -> bool {
        matches!(self.kind, NetDialogKind::RemoteLogin { .. })
            || matches!(self.protocol, Protocol::Sftp | Protocol::WebDav)
    }

    fn set_protocol(&mut self, protocol: Protocol, window: &mut Window, cx: &mut Context<Self>) {
        if self.protocol == protocol {
            return;
        }
        self.protocol = protocol;
        self.error = None;
        self.pending_key = None;
        let default_port = match protocol {
            Protocol::WebDav => "443",
            _ => "22",
        };
        // Follow the protocol's default port unless the user typed another one.
        let typed = self.port.read(cx).text().trim().to_owned();
        if typed.is_empty() || typed == "22" || typed == "443" {
            self.port
                .update(cx, |i, cx| i.set_text(default_port.to_owned(), cx));
        }
        let first = if protocol == Protocol::Smb {
            self.folder.focus_handle(cx)
        } else {
            self.host.focus_handle(cx)
        };
        window.focus(&first, cx);
        cx.notify();
    }

    /// Target share; `None` if empty or not shaped like a share.
    fn target(&self, cx: &App) -> Option<PathBuf> {
        match &self.kind {
            NetDialogKind::Login { share } => Some(share.clone()),
            NetDialogKind::Map { .. } | NetDialogKind::RemoteLogin { .. } => {
                let text = self.folder.read(cx).committed_text();
                let text = text.trim().trim_matches('"').trim().replace('/', "\\");
                // Needs both a host and a share name.
                let mut parts = text
                    .strip_prefix(r"\\")?
                    .split('\\')
                    .filter(|p| !p.is_empty());
                (parts.next().is_some() && parts.next().is_some()).then(|| PathBuf::from(text))
            }
        }
    }

    /// The remote location from the form, or the message to show.
    fn remote_url(&self, cx: &App) -> Result<RemoteUrl, String> {
        let host = self.host.read(cx).committed_text().trim().to_owned();
        let user = self.user.read(cx).committed_text().trim().to_owned();
        // A whole URL pasted into the host field wins (also the way to use plain http).
        if host.contains("://") {
            let mut url =
                RemoteUrl::parse_input(&host).ok_or_else(|| t!("net.need_host").into_owned())?;
            if url.user.is_none() && !user.is_empty() {
                url.user = Some(user);
            }
            return Ok(url);
        }
        if host.is_empty() {
            return Err(t!("net.need_host").into_owned());
        }
        if self.protocol == Protocol::Sftp && user.is_empty() {
            return Err(t!("net.need_user").into_owned());
        }
        let scheme = match &self.kind {
            // Signing in again keeps the location's own scheme (http on this computer).
            NetDialogKind::RemoteLogin { url, .. } => {
                RemoteUrl::from_path(url).map_or("https", |u| u.scheme.as_str())
            }
            _ if self.protocol == Protocol::Sftp => "sftp",
            _ => "https",
        };
        let port = self.port.read(cx).committed_text().trim().to_owned();
        let path = self.path.read(cx).committed_text().trim().to_owned();
        let authority = if user.is_empty() {
            host
        } else {
            format!("{user}@{host}")
        };
        let port = if port.is_empty() {
            String::new()
        } else {
            format!(":{port}")
        };
        let path = if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        // Built from parts, so names are literal (no %-decoding).
        RemoteUrl::from_path(std::path::Path::new(&format!(
            "{scheme}://{authority}{port}{path}"
        )))
        .ok_or_else(|| t!("net.bad_address").into_owned())
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.is_remote() {
            self.submit_remote(cx);
            return;
        }
        let Some(share) = self.target(cx) else {
            self.error = Some(t!("net.need_folder").into_owned().into());
            cx.notify();
            return;
        };
        let user = self.user.read(cx).committed_text().trim().to_owned();
        let password = self.password.read(cx).text().to_owned();
        if matches!(self.kind, NetDialogKind::Login { .. }) && user.is_empty() {
            self.error = Some(t!("net.need_user").into_owned().into());
            cx.notify();
            return;
        }
        let request = NetConnect {
            share,
            drive: self.letter,
            // No user name means the current Windows identity.
            credentials: (!user.is_empty()).then_some((user, password)),
            remember: self.remember,
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let shell = self.shell.clone();
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("net-connect", move || {
            let _ = tx.send(shell.net_connect(&request));
        });
        cx.spawn(async move |this, cx| {
            // The reason is shown in the dialog; no reply (thread gone) has none.
            let result = match rx.recv_async().await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err(String::new()),
            };
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.clear_password(cx);
                match result {
                    Ok(()) => cx.emit(NetDialogEvent::Connected),
                    Err(why) => {
                        this.error = Some(
                            if why.is_empty() {
                                t!("net.failed").into_owned()
                            } else {
                                why
                            }
                            .into(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// SFTP or WebDAV: trusts a confirmed host key, signs in and checks the folder opens, all in
    /// the background.
    fn submit_remote(&mut self, cx: &mut Context<Self>) {
        let url = match self.remote_url(cx) {
            Ok(url) => url,
            Err(why) => {
                self.error = Some(why.into());
                cx.notify();
                return;
            }
        };
        let key_file = (self.use_key && url.scheme == Scheme::Sftp)
            .then(|| {
                self.key_file
                    .read(cx)
                    .committed_text()
                    .trim()
                    .trim_matches('"')
                    .to_owned()
            })
            .filter(|k| !k.is_empty())
            .map(PathBuf::from);
        if self.use_key && url.scheme == Scheme::Sftp && key_file.is_none() {
            self.error = Some(t!("net.need_key").into_owned().into());
            cx.notify();
            return;
        }
        let secret = Some(self.password.read(cx).text().to_owned()).filter(|s| !s.is_empty());
        let trust = self
            .pending_key
            .take()
            .filter(|p| p.key.host == arke_remote::known_hosts::host_pattern(&url.host, url.port))
            .map(|p| p.key);
        let remember = self.remember;
        self.busy = true;
        self.error = None;
        cx.notify();
        let remotes = self.remotes.clone();
        let (tx, rx) = flume::bounded(1);
        let target = url.clone();
        let key = key_file.clone();
        crate::explorer::spawn_thread("remote-connect", move || {
            if let Some(key) = &trust
                && let Err(err) = remotes.trust(key)
            {
                let _ = tx.send(Err(RemoteError::Local(err)));
                return;
            }
            remotes.set_key_file(&target, key);
            let _ = tx.send(remotes.sign_in(&target, secret, remember));
        });
        let save = matches!(self.kind, NetDialogKind::Map { .. });
        cx.spawn(async move |this, cx| {
            let result = rx
                .recv_async()
                .await
                .unwrap_or_else(|_| Err(RemoteError::Server(t!("net.failed").into_owned())));
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(entry) => {
                        this.clear_password(cx);
                        // A file was named: open the folder it's in.
                        let folder = if entry.is_dir_like() {
                            url.clone()
                        } else {
                            url.parent().unwrap_or_else(|| url.clone())
                        };
                        cx.emit(NetDialogEvent::RemoteConnected {
                            place: RemotePlace {
                                url: folder.to_path(),
                                key_file,
                            },
                            save,
                        });
                    }
                    Err(RemoteError::HostKeyUnknown(key)) => {
                        this.pending_key = Some(PendingKey {
                            key,
                            changed: false,
                        });
                    }
                    Err(RemoteError::HostKeyChanged(key)) => {
                        this.pending_key = Some(PendingKey { key, changed: true });
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(test)]
    pub(crate) fn fill_for_test(
        &mut self,
        folder: &str,
        user: &str,
        password: &str,
        cx: &mut Context<Self>,
    ) {
        self.folder
            .update(cx, |i, cx| i.set_text(folder.to_owned(), cx));
        self.user
            .update(cx, |i, cx| i.set_text(user.to_owned(), cx));
        self.password
            .update(cx, |i, cx| i.set_text(password.to_owned(), cx));
        self.submit(cx);
    }

    /// Fills the remote form (switching to `protocol`) and submits.
    #[cfg(test)]
    pub(crate) fn fill_remote_for_test(
        &mut self,
        protocol: Protocol,
        host: &str,
        port: &str,
        user: &str,
        password: &str,
        cx: &mut Context<Self>,
    ) {
        self.protocol = protocol;
        for (input, text) in [
            (&self.host, host),
            (&self.port, port),
            (&self.user, user),
            (&self.password, password),
        ] {
            input.update(cx, |i, cx| i.set_text(text.to_owned(), cx));
        }
        self.submit(cx);
    }

    #[cfg(test)]
    pub(crate) fn submit_for_test(&mut self, cx: &mut Context<Self>) {
        self.submit(cx);
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<String> {
        self.error.as_ref().map(ToString::to_string)
    }

    /// Fingerprint waiting for confirmation and whether it changed.
    #[cfg(test)]
    pub(crate) fn pending_key_for_test(&self) -> Option<(String, bool)> {
        self.pending_key
            .as_ref()
            .map(|p| (p.key.fingerprint.clone(), p.changed))
    }
}

fn label(text: impl Into<SharedString>) -> impl IntoElement {
    let text = text.into();
    div()
        .text_size(theme::FS_12)
        .text_color(theme::text_muted())
        // Names the field below it for screen readers.
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::label_scope(text.clone()))
        })
        .child(text)
}

/// Input frame (accent border when focused).
fn field(input: &Entity<TextInput>, window: &Window, cx: &App) -> gpui::Div {
    let focused = input.focus_handle(cx).is_focused(window);
    div()
        .h(theme::H_CTL)
        .px(theme::S_8)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(theme::line())
        })
        .child(input.clone())
}

/// Label above a field.
fn labeled(text: impl Into<SharedString>, input: gpui::Div) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(theme::S_4)
        .child(label(text))
        .child(input)
}

fn button(
    id: &'static str,
    text: impl Into<SharedString>,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let text = text.into();
    div()
        .id(id)
        .a11y_button(text.clone())
        .h(theme::H_CTL)
        .px(theme::S_16)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .cursor_pointer()
        .when(primary, |d| {
            d.bg(theme::signal()).text_color(theme::on_signal())
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(text)
}

/// One choice in a row of chips (drive letters, protocol, sign-in method).
fn chip(
    id: impl Into<gpui::ElementId>,
    text: impl Into<SharedString>,
    on: bool,
) -> gpui::Stateful<gpui::Div> {
    let text = text.into();
    div()
        .id(id)
        .a11y(gpui::Role::RadioButton, text.clone())
        .aria_toggled(crate::a11y::toggled(on))
        .h(theme::S_24)
        .px(theme::S_8)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::R_4)
        .cursor_pointer()
        .text_size(theme::FS_12)
        .when(on, |d| {
            d.bg(theme::signal())
                .text_color(theme::on_signal())
                .font_weight(theme::FW_STRONG)
        })
        .when(!on, |d| {
            d.text_color(theme::text_muted())
                .hover(|s| s.bg(theme::wash_strong()))
        })
        .child(text)
}

impl NetDialog {
    /// Title, intro line and OK button text.
    fn texts(&self) -> (String, String, String) {
        match &self.kind {
            NetDialogKind::Login { share } => (
                t!("net.login_title").into_owned(),
                t!("net.login_intro", name = share.display()).into_owned(),
                t!("net.login_ok").into_owned(),
            ),
            NetDialogKind::RemoteLogin { url, .. } => {
                let name = RemoteUrl::from_path(url)
                    .map_or_else(|| url.display().to_string(), |u| u.host_port());
                (
                    t!("net.remote_login_title").into_owned(),
                    t!("net.login_intro", name = name).into_owned(),
                    t!("net.login_ok").into_owned(),
                )
            }
            NetDialogKind::Map { .. } if self.is_remote() => (
                t!("net.add_title").into_owned(),
                if self.protocol == Protocol::Sftp {
                    t!("net.sftp_intro").into_owned()
                } else {
                    t!("net.dav_intro").into_owned()
                },
                t!("net.map_ok").into_owned(),
            ),
            NetDialogKind::Map { .. } => (
                t!("net.add_title").into_owned(),
                t!("net.map_intro").into_owned(),
                t!("net.map_ok").into_owned(),
            ),
        }
    }

    fn render_protocols(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div().flex().gap(theme::S_4).children(
            [
                (Protocol::Smb, t!("net.proto_smb")),
                (Protocol::Sftp, t!("net.proto_sftp")),
                (Protocol::WebDav, t!("net.proto_dav")),
            ]
            .into_iter()
            .enumerate()
            .map(|(ix, (protocol, text))| {
                chip(
                    ("net-proto", ix),
                    text.into_owned(),
                    self.protocol == protocol,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_protocol(protocol, window, cx);
                }))
            }),
        )
    }

    fn render_smb_fields(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let letters = match &self.kind {
            NetDialogKind::Map { letters } => letters.clone(),
            _ => Vec::new(),
        };
        let is_map = matches!(self.kind, NetDialogKind::Map { .. });
        div()
            .flex()
            .flex_col()
            .gap(theme::S_12)
            .when(is_map, |d| {
                d.child(label(t!("net.folder").into_owned()))
                    .child(field(&self.folder, window, cx))
                    .child(label(t!("net.letter").into_owned()))
                    .child(div().flex().flex_wrap().gap(theme::S_4).children(
                        letters.into_iter().enumerate().map(|(ix, letter)| {
                            chip(
                                ("net-letter", ix),
                                format!("{letter}:"),
                                self.letter == Some(letter),
                            )
                            .w(px(34.))
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    this.letter = Some(letter);
                                    cx.notify();
                                },
                            ))
                        }),
                    ))
            })
            .child(label(if is_map {
                t!("net.user_optional").into_owned()
            } else {
                t!("net.user").into_owned()
            }))
            .child(field(&self.user, window, cx))
            .child(label(t!("net.password").into_owned()))
            .child(field(&self.password, window, cx))
    }

    fn render_remote_fields(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let sftp = self.protocol == Protocol::Sftp;
        let login = matches!(self.kind, NetDialogKind::RemoteLogin { .. });
        let use_key = self.use_key && sftp;
        div()
            .flex()
            .flex_col()
            .gap(theme::S_12)
            .when(!login, |d| {
                d.child(
                    div()
                        .flex()
                        .gap(theme::S_8)
                        .child(
                            labeled(t!("net.host").into_owned(), field(&self.host, window, cx))
                                .flex_1(),
                        )
                        .child(
                            labeled(t!("net.port").into_owned(), field(&self.port, window, cx))
                                .w(px(80.)),
                        ),
                )
                .child(labeled(
                    t!("net.remote_path").into_owned(),
                    field(&self.path, window, cx),
                ))
            })
            .child(labeled(
                if sftp {
                    t!("net.user").into_owned()
                } else {
                    t!("net.user_dav").into_owned()
                },
                field(&self.user, window, cx),
            ))
            .when(sftp, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::S_4)
                        .child(
                            div()
                                .mr(theme::S_4)
                                .child(label(t!("net.sign_in_with").into_owned())),
                        )
                        .child(
                            chip(
                                "net-auth-password",
                                t!("net.password").into_owned(),
                                !use_key,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| {
                                    this.use_key = false;
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            chip("net-auth-key", t!("net.key_file").into_owned(), use_key)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.use_key = true;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when(use_key, |d| {
                d.child(labeled(
                    t!("net.key_file").into_owned(),
                    field(&self.key_file, window, cx),
                ))
            })
            .child(labeled(
                if use_key {
                    t!("net.passphrase").into_owned()
                } else if sftp {
                    t!("net.password_or_agent").into_owned()
                } else {
                    t!("net.password").into_owned()
                },
                field(&self.password, window, cx),
            ))
    }

    /// First connection or changed key: the fingerprint to check.
    fn render_host_key(&self) -> Option<gpui::Div> {
        let pending = self.pending_key.as_ref()?;
        let (title, body) = if pending.changed {
            (
                t!("net.key_changed_title").into_owned(),
                t!("net.key_changed_body", host = pending.key.host.as_str()).into_owned(),
            )
        } else {
            (
                t!("net.key_new_title").into_owned(),
                t!("net.key_new_body").into_owned(),
            )
        };
        Some(
            div()
                .p(theme::S_12)
                .flex()
                .flex_col()
                .gap(theme::S_4)
                .rounded(theme::R_8)
                .when(pending.changed, |d| d.bg(theme::danger_surface()))
                .when(!pending.changed, |d| d.bg(theme::wash()))
                .child(
                    div()
                        .font_weight(theme::FW_STRONG)
                        .when(pending.changed, |d| d.text_color(theme::danger()))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_muted())
                        .child(body),
                )
                .child(
                    div()
                        .text_size(theme::FS_12)
                        .font_family("Cascadia Mono")
                        .child(format!(
                            "{} {}",
                            pending.key.algorithm, pending.key.fingerprint
                        )),
                ),
        )
    }
}

impl Render for NetDialog {
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, intro, ok) = self.texts();
        let is_map = matches!(self.kind, NetDialogKind::Map { .. });
        let remote = self.is_remote();
        let remember = self.remember;
        let (ok, ok_danger) = match &self.pending_key {
            Some(p) if p.changed => (t!("net.key_replace").into_owned(), true),
            Some(_) => (t!("net.key_trust").into_owned(), false),
            None => (ok, false),
        };
        let panel = div()
            .id("net-dialog")
            .a11y(gpui::Role::Dialog, title.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NetDialogCancel, _, cx| this.cancel(cx)))
            // Clicks on the panel don't close it.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(440.))
            .p(theme::S_20)
            .flex()
            .flex_col()
            .gap(theme::S_12)
            .rounded(theme::R_12)
            .bg(theme::overlay())
            .shadow(theme::elev_3())
            .ui_font()
            .text_size(theme::FS_14)
            .text_color(theme::text())
            .child(
                div()
                    .text_size(theme::FS_15)
                    .font_weight(theme::FW_STRONG)
                    .child(title),
            )
            .when(is_map, |d| d.child(self.render_protocols(cx)))
            .child(div().text_color(theme::text_muted()).child(intro))
            .when(!remote, |d| d.child(self.render_smb_fields(window, cx)))
            .when(remote, |d| d.child(self.render_remote_fields(window, cx)))
            .when(crate::a11y::active(), |d| {
                d.child(crate::a11y::end_label_scope())
            })
            .child(
                div()
                    .id("net-remember")
                    .a11y(gpui::Role::CheckBox, t!("net.remember"))
                    .aria_toggled(crate::a11y::toggled(remember))
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .cursor_pointer()
                    .child(
                        div()
                            .size(theme::ICON)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::R_4)
                            .border_1()
                            .when(remember, |d| {
                                d.bg(theme::signal()).border_color(theme::signal()).child(
                                    gpui::svg()
                                        .path(crate::assets::Icon::Check.path())
                                        .size(theme::ICON_SM)
                                        .text_color(theme::on_signal()),
                                )
                            })
                            .when(!remember, |d| d.border_color(theme::line_strong())),
                    )
                    .child(t!("net.remember").into_owned())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.remember = !this.remember;
                        cx.notify();
                    })),
            )
            .children(self.render_host_key())
            .children(self.error.clone().map(|error| {
                div()
                    .text_size(theme::FS_12)
                    .text_color(theme::danger())
                    .child(error)
            }))
            .child(
                div()
                    .pt(theme::S_4)
                    .flex()
                    .items_center()
                    .gap(theme::S_8)
                    .when(is_map && !remote, |d| {
                        d.child(
                            div()
                                .id("net-system-dialog")
                                .a11y(gpui::Role::Link, t!("net.use_system"))
                                .text_size(theme::FS_12)
                                .text_color(theme::text_subtle())
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme::text()))
                                .child(t!("net.use_system").into_owned())
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.clear_password(cx);
                                    cx.emit(NetDialogEvent::UseSystemDialog);
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        button("net-cancel", t!("confirm.cancel").into_owned(), false)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx))),
                    )
                    .child(
                        button(
                            "net-ok",
                            if self.busy {
                                t!("net.connecting").into_owned()
                            } else {
                                ok
                            },
                            true,
                        )
                        .when(ok_danger, |d| d.bg(theme::danger()))
                        .when(self.busy, |d| d.opacity(0.6))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit(cx))),
                    ),
            );
        // Backdrop: clicking outside cancels.
        div()
            .id("net-dialog-veil")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::veil())
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel(cx)),
            )
            .child(panel)
            .with_animation("net-dialog-appear", crate::motion::enter(), |el, t| {
                crate::motion::slide_in(el, t, 0., 0.)
            })
    }
}
