//! Arke's optional background service `arke-indexd`, plus the parts Arke itself uses (protocol,
//! client).
//!
//! The service (journal reading, pipe server, install) also lives here, entered through
//! [`cli::run`]: release builds run it as `arke.exe --indexd <command>`, and development can use
//! the standalone `arke-indexd.exe`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cli;
pub mod client;
pub mod proto;
pub mod sec;

mod changes;
mod install;
mod logger;
mod mft;
mod server;
mod service;
mod usn;

pub use usn::journal_size;
