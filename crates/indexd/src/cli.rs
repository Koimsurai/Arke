//! Command line for the background service: `arke-indexd.exe <command>`, or
//! `arke.exe --indexd <command>` in the single-exe build. Reads the NTFS USN journal and hands
//! changes to Arke as paths. Installing it is opt-in.
//!
//! ```text
//! arke-indexd install --user <SID>   install, or add a user (admin)
//! arke-indexd uninstall              remove (admin)
//! arke-indexd run --allow <SID>...   started by the service control manager, not by hand
//! arke-indexd console --allow <SID> [--pipe <name>]   run in the foreground (development)
//! arke-indexd journal-size C:\ <MB>  set the change journal max size (admin; keeps records)
//! arke-indexd probe C:\ [seconds]    show journal state and print new changes (development, admin)
//! arke-indexd mft C:\                read the MFT, print entry count and timing (read-only; admin)
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::proto::PIPE_NAME;

use crate::server::{Config, Server};
use crate::{changes, install, logger, service, usn};

fn out(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn fail(msg: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "arke-indexd: {msg}");
    log::error!("{msg}");
    ExitCode::FAILURE
}

/// Values of `--name value` arguments.
fn values<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|w| w[0] == name)
        .map(|w| w[1].as_str())
        .collect()
}

/// Arguments placed before the command in the service command line for the single-exe build
/// (`arke.exe --indexd run ...`).
static PREFIX: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

pub(crate) fn prefix() -> Option<&'static str> {
    PREFIX.get().copied()
}

/// Runs one command. `prefix` is `--indexd` for the single exe (written into the service command
/// line on install), `None` for the standalone `arke-indexd.exe`.
pub fn run(args: Vec<String>, prefix: Option<&'static str>) -> ExitCode {
    if let Some(prefix) = prefix {
        let _ = PREFIX.set(prefix);
    }
    let Some(command) = args.first() else {
        return fail(
            "usage: arke-indexd install --user <SID> | uninstall | console | probe <drive>",
        );
    };
    match command.as_str() {
        "run" => {
            if let Some(dir) = install::install_path().as_deref().and_then(Path::parent) {
                logger::to_file(&dir.join("indexd.log"));
            }
            let config = Config {
                allowed: values(&args, "--allow")
                    .into_iter()
                    .map(String::from)
                    .collect(),
                pipe_name: PIPE_NAME.to_string(),
                idle: None,
                owner_access: false,
            };
            match service::run(config) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => fail(&format!("not started by the service manager: {err}")),
            }
        }
        "install" => {
            let Some(user) = values(&args, "--user").first().copied() else {
                return fail("install needs --user <SID>");
            };
            match install::install(user) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => fail(&format!("install failed: {err}")),
            }
        }
        "uninstall" => match install::uninstall() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => fail(&format!("uninstall failed: {err}")),
        },
        "console" => {
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                .init();
            let mut allowed: Vec<String> = values(&args, "--allow")
                .into_iter()
                .map(String::from)
                .collect();
            if allowed.is_empty() {
                allowed.extend(crate::sec::current_user_sid());
            }
            let pipe_name = values(&args, "--pipe")
                .first()
                .map_or_else(|| PIPE_NAME.to_string(), |s| (*s).to_string());
            let server = Server::new(Config {
                allowed,
                pipe_name,
                idle: None,
                owner_access: true,
            });
            match server.run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => fail(&err.to_string()),
            }
        }
        "probe" => probe(&args),
        "mft" => mft_probe(&args),
        "journal-size" => {
            let (Some(root), Some(mb)) =
                (args.get(1), args.get(2).and_then(|m| m.parse::<u64>().ok()))
            else {
                return fail("usage: journal-size <drive> <MB>");
            };
            // 16 MB to 8 GB: smaller is useless, larger wastes disk.
            if !(16..=8192).contains(&mb) {
                return fail("journal size must be 16-8192 MB");
            }
            match usn::set_journal_size(Path::new(root), mb * 1024 * 1024) {
                Ok(()) => {
                    log::info!("{root} change journal max size set to {mb} MB");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(&format!("journal-size {root}: {}", err.message())),
            }
        }
        other => fail(&format!("unknown command: {other}")),
    }
}

/// Reads the MFT once, builds the listing with Arke's rules and prints stats only (writes nothing).
fn mft_probe(args: &[String]) -> ExitCode {
    let root = PathBuf::from(args.get(1).map_or(r"C:\", String::as_str));
    let started = Instant::now();
    let mut table = match crate::mft::read_volume(&root, &|| false) {
        Ok(t) => t,
        Err(err) => return fail(&format!("{} (run as administrator): {err}", root.display())),
    };
    let read = started.elapsed();
    let privacy = changes::Privacy {
        profiles: None,
        mine: None,
    };
    let (mut items, mut dirs, mut bytes) = (0usize, 0usize, 0u64);
    crate::mft::listing(&mut table, &root, &privacy, |l| {
        items += 1;
        dirs += usize::from(l.attributes & 0x10 != 0);
        bytes += l.size;
        true
    });
    out(&format!(
        "{}: {} records, {items} items ({dirs} folders, {:.1} GB); read {:.2} s, total {:.2} s",
        root.display(),
        table.files.len(),
        bytes as f64 / 1e9,
        read.as_secs_f64(),
        started.elapsed().as_secs_f64()
    ));
    ExitCode::SUCCESS
}

/// Shows journal state, then prints changes for a few seconds to check reads and path resolution.
fn probe(args: &[String]) -> ExitCode {
    let root = PathBuf::from(args.get(1).map_or(r"C:\", String::as_str));
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let volume = match usn::Volume::open(&root) {
        Ok(v) => v,
        Err(err) => {
            return fail(&format!(
                "cannot open {} (run as administrator): {err}",
                root.display()
            ));
        }
    };
    let data = match volume.query() {
        Ok(d) => d,
        Err(err) => return fail(&format!("no change journal on {}: {err}", root.display())),
    };
    out(&format!(
        "journal {:#x}: first {} next {} ({:.1} MB of history)",
        data.id,
        data.first,
        data.next,
        (data.next - data.first) as f64 / 1_048_576.0
    ));
    // Can the old records be read in full, and how fast (catch-up after downtime does this).
    let started = Instant::now();
    let mut usn = data.first;
    let mut buf = vec![0u8; 64 * 1024];
    let mut count = 0usize;
    let mut resolve = changes::Cached::new(|frn| volume.path_of(frn));
    let privacy = changes::Privacy {
        profiles: None,
        mine: None,
    };
    let mut paths = 0usize;
    while usn < data.next {
        match volume.read(data.id, usn, &mut buf) {
            Ok((next, records)) if next > usn => {
                count += records.len();
                paths += changes::to_changes(&records, &mut resolve, &privacy).len();
                usn = next;
            }
            Ok(_) => break,
            Err(err) => return fail(&format!("read failed: {err:?}")),
        }
    }
    out(&format!(
        "read {count} old records -> {paths} paths in {:.2} s",
        started.elapsed().as_secs_f64()
    ));
    out(&format!(
        "watching {} for {seconds} s; change some files...",
        root.display()
    ));
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        match volume.read(data.id, usn, &mut buf) {
            Ok((next, records)) if next > usn => {
                for c in changes::to_changes(&records, &mut resolve, &privacy) {
                    let mark = if c.moved_in { " (folder moved in)" } else { "" };
                    out(&format!("  {}{mark}", c.path.display()));
                }
                usn = next;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(300)),
            Err(err) => return fail(&format!("read failed: {err:?}")),
        }
    }
    ExitCode::SUCCESS
}
