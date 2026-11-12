//! `arke-indexd`: standalone binary for the background service, for development. Release
//! builds run the service as `arke.exe --indexd` (see `cli.rs`).

use std::process::ExitCode;

fn main() -> ExitCode {
    arke_indexd::cli::run(std::env::args().skip(1).collect(), None)
}
