//! Frame timing log, for checking that transitions stay smooth in release builds.
//!
//! `ARKE_FRAME_LOG=<file>` writes one line per drawn frame: milliseconds since startup, the
//! interval since the previous frame, and the CPU time from the start of the window's render to
//! the end of its paint. Animations draw back-to-back frames, so a transition shows up as a burst
//! of lines; a smooth one keeps the interval at the display's refresh period and the CPU time well
//! under it. Off unless the variable is set (then a thread-local check per frame).
//!
//! Lines go to a background thread that does the writing, so the UI thread never touches the disk.

use std::cell::RefCell;
use std::io::Write as _;
use std::sync::mpsc::Sender;
use std::time::Instant;

use gpui::{AnyElement, IntoElement, Styled, canvas, deferred, px};

struct Probe {
    tx: Sender<String>,
    origin: Instant,
    /// Start of the frame being drawn now.
    started: Option<Instant>,
    /// Start of the previous frame.
    previous: Option<Instant>,
}

thread_local! {
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

/// Start logging if `ARKE_FRAME_LOG` names a file. Call on the UI thread.
pub(crate) fn init() {
    let Some(path) = std::env::var_os("ARKE_FRAME_LOG") else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let spawned = std::thread::Builder::new()
        .name("frame-log".into())
        .spawn(move || {
            let Ok(file) = std::fs::File::create(&path) else {
                return;
            };
            let mut out = std::io::BufWriter::new(file);
            let _ = writeln!(out, "t_ms\tinterval_ms\tcpu_ms");
            while let Ok(line) = rx.recv() {
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
        });
    if spawned.is_ok() {
        PROBE.set(Some(Probe {
            tx,
            origin: Instant::now(),
            started: None,
            previous: None,
        }));
    }
}

/// The window's root render begins (a new frame).
pub(crate) fn frame_started() {
    PROBE.with_borrow_mut(|probe| {
        if let Some(probe) = probe {
            probe.started = Some(Instant::now());
        }
    });
}

fn painted() {
    PROBE.with_borrow_mut(|probe| {
        let Some(probe) = probe else {
            return;
        };
        let Some(started) = probe.started.take() else {
            return;
        };
        let now = Instant::now();
        let interval = probe
            .previous
            .replace(started)
            .map_or(0., |p| started.duration_since(p).as_secs_f64() * 1000.);
        let _ = probe.tx.send(format!(
            "{:.2}\t{interval:.2}\t{:.2}",
            started.duration_since(probe.origin).as_secs_f64() * 1000.,
            now.duration_since(started).as_secs_f64() * 1000.
        ));
    });
}

/// Painted after everything else in the window (deferred, top priority): marks the frame's end.
/// `None` when not logging.
pub(crate) fn marker() -> Option<AnyElement> {
    let on = PROBE.with_borrow(Option::is_some);
    on.then(|| {
        deferred(
            canvas(|_, _, _| (), |_, (), _, _| painted())
                .w(px(0.))
                .h(px(0.)),
        )
        .with_priority(usize::MAX)
        .into_any_element()
    })
}
