//! Service log: written to the install folder (admin-only, so it can't be swapped for a link);
//! console mode logs to stderr.

use parking_lot::Mutex;
use std::io::Write;
use std::path::Path;

struct FileLog(Mutex<std::fs::File>);

impl log::Log for FileLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = writeln!(self.0.lock(), "{secs} {} {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

/// Logs to `path`, starting over once the file exceeds 1 MB.
pub(crate) fn to_file(path: &Path) {
    let big = std::fs::metadata(path).is_ok_and(|m| m.len() > 1024 * 1024);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!big)
        .write(true)
        .truncate(big)
        .open(path);
    if let Ok(file) = file
        && log::set_boxed_logger(Box::new(FileLog(Mutex::new(file)))).is_ok()
    {
        log::set_max_level(log::LevelFilter::Info);
    }
}
