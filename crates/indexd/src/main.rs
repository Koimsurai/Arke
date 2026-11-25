//! `arke-indexd`: standalone binary for the background service, for development. Release
//! builds run the service as `arke.exe --indexd` (see `cli.rs`).

use std::process::ExitCode;

fn main() -> ExitCode {
    // `args_os`: `std::env::args` panics on an argument that isn't valid Unicode.
    arke_indexd::cli::run(
        std::env::args_os()
            .skip(1)
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        None,
    )
}
