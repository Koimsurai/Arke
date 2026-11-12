//! arke-remote: remote locations (SFTP and WebDAV) behind Arke's [`arke_core::Fs`] trait.
//!
//! A remote folder is a path holding its URL (see [`arke_core::remote`]). [`RoutingFs`] sends
//! those to [`Remotes`], which keeps the connections; file operations (copy in and out,
//! rename, delete) are methods on [`Remotes`]. Everything blocks and must run on background
//! threads; every network step has a timeout.

// Tests may unwrap / panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

rust_i18n::i18n!("locales", fallback = "en");

pub mod creds;
pub mod dav;
pub mod error;
#[cfg(windows)]
pub mod http;
pub mod hub;
pub mod known_hosts;
#[cfg(test)]
mod live_tests;
pub mod routing;
mod sftp;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
#[cfg(any(test, feature = "test-support"))]
pub mod testing_dav;

pub use error::{HostKey, RemoteError, RemoteResult};
pub use hub::{NoProgress, Remotes, RemotesConfig, TransferSink};
pub use routing::RoutingFs;
