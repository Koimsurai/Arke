//! Window benchmark modes (`--bench startup|scroll|scroll-grid`). The headless search index
//! benchmark (`--bench index`) is in `bench_index.rs`.
//!
//! - startup: time from cold start to first frame and to the first fully listed directory, then
//!   exit.
//! - scroll: after the listing loads, scroll `SCROLL_STEP` rows per frame to the bottom, recording
//!   frame intervals and row build time. Every frame rebuilds the visible rows, the worst case for
//!   a virtualized list.
//!
//! Results go to the `--bench-out` file (release builds have no console).

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BenchMode {
    Startup,
    Scroll,
    /// Scroll the large-icon grid while thumbnails load in the background.
    ScrollGrid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    WaitingListing,
    Scrolling,
    Done,
}

pub(crate) const SCROLL_STEP: usize = 40;
/// A grid row is about 124px. Two rows per frame is roughly a hard wheel flick and lets thumbnails
/// load mid-scroll.
pub(crate) const GRID_SCROLL_STEP: usize = 2;

pub(crate) struct Bench {
    mode: BenchMode,
    out: Option<PathBuf>,
    phase: Phase,
    last_frame: Option<Instant>,
    /// Frame intervals (ms) while loading; shows the UI keeps rendering during listing.
    loading_frames: Vec<f64>,
    scroll_frames: Vec<f64>,
    rows_time: Vec<f64>,
    pub(crate) scroll_ix: usize,
}

pub(crate) enum BenchStep {
    Continue,
    ScrollTo(usize),
    Finish,
}

impl Bench {
    pub(crate) const fn new(mode: BenchMode, out: Option<PathBuf>) -> Self {
        Self {
            mode,
            out,
            phase: Phase::WaitingListing,
            last_frame: None,
            loading_frames: Vec::new(),
            scroll_frames: Vec::new(),
            rows_time: Vec::new(),
            scroll_ix: 0,
        }
    }

    pub(crate) const fn mode(&self) -> BenchMode {
        self.mode
    }

    pub(crate) fn record_rows(&mut self, elapsed: Duration) {
        if self.phase == Phase::Scrolling {
            self.rows_time.push(elapsed.as_secs_f64() * 1e3);
        }
    }

    /// Called once per frame.
    pub(crate) fn on_frame(&mut self, listing_ready: bool, entry_count: usize) -> BenchStep {
        let now = Instant::now();
        if let Some(last) = self.last_frame.replace(now) {
            let ms = (now - last).as_secs_f64() * 1e3;
            match self.phase {
                Phase::WaitingListing => self.loading_frames.push(ms),
                Phase::Scrolling => self.scroll_frames.push(ms),
                Phase::Done => {}
            }
        }
        match self.phase {
            Phase::WaitingListing if listing_ready => {
                if self.mode == BenchMode::Startup || entry_count == 0 {
                    self.phase = Phase::Done;
                    BenchStep::Finish
                } else {
                    self.phase = Phase::Scrolling;
                    self.scroll_ix = 0;
                    BenchStep::ScrollTo(0)
                }
            }
            Phase::Scrolling => {
                self.scroll_ix += if self.mode == BenchMode::ScrollGrid {
                    GRID_SCROLL_STEP
                } else {
                    SCROLL_STEP
                };
                if self.scroll_ix >= entry_count {
                    self.phase = Phase::Done;
                    BenchStep::Finish
                } else {
                    BenchStep::ScrollTo(self.scroll_ix)
                }
            }
            _ => BenchStep::Continue,
        }
    }

    pub(crate) fn report(&self, header: &str) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{header}");
        let _ = writeln!(s, "mode: {:?}", self.mode);
        let _ = writeln!(s, "loading_frames: {}", stats(&self.loading_frames));
        if self.mode != BenchMode::Startup {
            let _ = writeln!(s, "scroll_frames: {}", stats(&self.scroll_frames));
            let over = self.scroll_frames.iter().filter(|&&f| f > 33.4).count();
            let _ = writeln!(s, "scroll_frames_over_33ms: {over}");
            let _ = writeln!(s, "rows_build_ms: {}", stats(&self.rows_time));
        }
        s
    }

    pub(crate) fn write_report(&self, header: &str) {
        let report = self.report(header);
        log::info!("bench results:\n{report}");
        if let Some(out) = &self.out {
            use std::io::Write as _;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(out);
            match file {
                Ok(mut f) => {
                    let _ = writeln!(f, "{report}");
                }
                Err(err) => log::error!("cannot write {}: {err}", out.display()),
            }
        }
    }
}

fn stats(samples: &[f64]) -> String {
    if samples.is_empty() {
        return "n=0".into();
    }
    let mut v = samples.to_vec();
    v.sort_by(f64::total_cmp);
    let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    format!(
        "n={} avg={avg:.2} p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        v.len(),
        pct(0.5),
        pct(0.95),
        pct(0.99),
        v[v.len() - 1]
    )
}
