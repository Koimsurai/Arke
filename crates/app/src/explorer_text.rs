//! Text and code previews, virtualized: only the lines on screen (and a few around them) are laid
//! out, so a log of a million lines scrolls like a short file.
//!
//! - The load reads the head of the file (64 KB) as before: decoded, highlighted, split into
//!   lines ([`TextHead`]).
//! - A local file longer than its head is indexed ([`arke_core::text_lines`]) on a background
//!   thread once the view comes near the end of the head, so selecting a large file reads no more
//!   than before. The same thread then reads blocks of [`BLOCK`] lines as they come into view and
//!   decodes and highlights them. For code it also works out, block by block, whether each block
//!   starts inside a comment or a string: until then blocks are shown plain, then read again.
//! - Each surface (the pane, Quick Look) keeps its own [`TextLayout`]: how many rows each line
//!   takes, estimated from its length until it has been laid out. The first line on screen and
//!   the scroll range follow from it. When a line above the first one turns out taller or shorter
//!   than estimated, the next frame scrolls by the difference, so what is on screen stays put.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arke_core::Fs;
use arke_core::highlight::{Carry, Lang, Token, highlight_from};
use arke_core::text::TextEncoding;
use arke_core::text_lines::{LineIndex, read_lines};
use gpui::{AnyElement, Pixels, ScrollHandle, SharedString, canvas, div, point, prelude::*, px};

use super::{AppEvent, CodeLine, lines_with_marks, spawn_thread};
use crate::shell::Shell;
use crate::theme;

/// Lines per block read from the file.
const BLOCK: usize = 64;
/// Lines per run whose rows the layout model sums up.
const LAYOUT_RUN: usize = 256;
/// Most blocks kept decoded per document.
const CACHE_BLOCKS: usize = 32;
/// Most text kept decoded per document, in bytes (the blocks on screen are always kept).
const CACHE_BYTES: usize = 8 << 20;
/// Longest line shown, in bytes: the head's size, the most any line showed before.
const LINE_MAX: usize = 64 * 1024;
/// Rows laid out above and below the screen.
const OVERSCAN: u64 = 4;
/// Rows laid out while the view's height isn't known yet (its first frame).
const FIRST_ROWS: u64 = 60;
/// The file is indexed once the view comes this many lines near the end of the head.
const INDEX_AHEAD: usize = 64;
/// Lines whose laid-out height a view remembers; past that the ones far away are forgotten.
const MEASURED_MAX: usize = 1 << 16;
/// How far from the first line on screen remembered heights survive [`MEASURED_MAX`].
const MEASURED_KEEP: usize = 4096;
/// Block requests the reader serves when they pile up (fast scrolling): the newest.
const QUEUE_KEEP: usize = 8;
/// Line height in Quick Look, roomier like an editor (the pane keeps GPUI's default, phi).
const LOOK_LINE_HEIGHT: f32 = 1.6;
/// Advance of a Cascadia Mono character in ems, for estimating where lines wrap.
const CHAR_EM: f32 = 0.6;

#[cfg(test)]
thread_local! {
    /// Line rows built since the last [`take_built_rows_for_test`]: every one is shaped.
    static BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Tests: line rows built (and so shaped) since the last call.
#[cfg(test)]
pub(crate) fn take_built_rows_for_test() -> usize {
    BUILT.with(|built| built.replace(0))
}

/// A line's text and highlight ranges, cloned out of the document for a frame.
type Line = (SharedString, Vec<(Range<usize>, Token)>);

/// The head of a text file as loaded in the background, and how to read the rest.
pub(crate) struct TextHead {
    pub(crate) lines: Vec<CodeLine>,
    pub(crate) encoding: TextEncoding,
    /// Set when the file goes on past the head and can be read in pieces.
    pub(crate) source: Option<TextSource>,
}

/// Where the rest of a text file is.
#[derive(Clone)]
pub(crate) struct TextSource {
    pub(crate) path: PathBuf,
    /// First byte of the text, past any byte order mark
    pub(crate) first: u64,
    pub(crate) encoding: TextEncoding,
    pub(crate) lang: Option<&'static Lang>,
}

/// What the reader thread uses.
pub(crate) struct Reader {
    pub(crate) fs: Arc<dyn Fs>,
    pub(crate) shell: Arc<dyn Shell>,
    pub(crate) tx: flume::Sender<AppEvent>,
}

/// What a document's reader thread reports.
pub(crate) enum TextUpdate {
    /// The file is indexed.
    Index(Arc<LineIndex>),
    /// A block of lines, read; `done` unless it is shown plain until its highlighting is known.
    Block {
        block: usize,
        lines: Vec<CodeLine>,
        done: bool,
    },
    /// Requests dropped because newer ones piled up; asked again if still on screen.
    Skipped(Vec<usize>),
    /// Every block's highlighting can now be worked out: plain blocks are read again.
    Colored,
    /// The file couldn't be indexed: the preview stays with its head.
    Failed,
}

/// A text preview's lines: the head, and the rest of the file once indexed.
pub(crate) struct TextDoc {
    /// The preview generation this was loaded for; its reader's updates carry it.
    pub(crate) generation: u64,
    head: Vec<CodeLine>,
    /// Lines of `head` that are whole: the last one is cut when the file goes on past the head.
    head_whole: usize,
    source: Option<TextSource>,
    reader: Option<Reader>,
    file: RefCell<FileState>,
    pane: Rc<RefCell<TextLayout>>,
    look: Rc<RefCell<TextLayout>>,
    /// Stops the reader's index scan once the document is dropped.
    cancel: Arc<AtomicBool>,
}

enum FileState {
    /// Only the head so far
    Head,
    /// Indexing; the reader takes block requests here once done
    Indexing(flume::Sender<usize>),
    Indexed(Indexed),
    /// Couldn't be indexed: the head is all there is
    Failed,
}

struct Indexed {
    index: Arc<LineIndex>,
    requests: flume::Sender<usize>,
    blocks: HashMap<usize, Slot>,
    /// Blocks can be highlighted (or need not be): plain ones are read again.
    colored: bool,
    /// Counts the frames that asked for lines, for dropping the blocks least recently shown.
    tick: u64,
}

enum Slot {
    Pending,
    Ready {
        lines: Vec<CodeLine>,
        /// False while shown plain until its highlighting is known
        done: bool,
        /// Asked for again, colored
        again: bool,
        used: u64,
        bytes: usize,
    },
}

impl TextDoc {
    pub(crate) fn new(generation: u64, head: TextHead, reader: Option<Reader>) -> Self {
        let head_whole = if head.source.is_some() {
            head.lines.len().saturating_sub(1)
        } else {
            head.lines.len()
        };
        Self {
            generation,
            head: head.lines,
            head_whole,
            source: head.source.filter(|_| reader.is_some()),
            reader,
            file: RefCell::new(FileState::Head),
            pane: Rc::default(),
            look: Rc::default(),
            cancel: Arc::default(),
        }
    }

    /// Lines in the view: the head's until the file is indexed.
    pub(crate) fn count(&self) -> usize {
        match &*self.file.borrow() {
            FileState::Indexed(file) => file.index.len(),
            _ => self.head.len(),
        }
    }

    /// Tests: the head's text, lines joined with `\n`.
    #[cfg(test)]
    pub(crate) fn head_text(&self) -> String {
        self.head
            .iter()
            .map(|l| l.text.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const fn layout(&self, large: bool) -> &Rc<RefCell<TextLayout>> {
        if large { &self.look } else { &self.pane }
    }

    /// Bytes of line `ix` (about its characters), for estimating how it wraps.
    fn len_of(&self, ix: usize) -> usize {
        if ix >= self.head_whole
            && let FileState::Indexed(file) = &*self.file.borrow()
        {
            let len = file.index.line_len(ix).min(LINE_MAX);
            return match file.index.encoding() {
                TextEncoding::Utf16Le | TextEncoding::Utf16Be => len / 2,
                TextEncoding::Utf8 | TextEncoding::Legacy => len,
            };
        }
        self.head.get(ix).map_or(0, |l| l.text.len())
    }

    /// Line `ix` to draw; `None` while its block is being read.
    fn line(&self, ix: usize) -> Option<Line> {
        let copy = |l: &CodeLine| (l.text.clone(), l.marks.clone());
        if ix >= self.head_whole
            && let FileState::Indexed(file) = &*self.file.borrow()
        {
            return match file.blocks.get(&(ix / BLOCK)) {
                // A line the file no longer had when read: empty.
                Some(Slot::Ready { lines, .. }) => Some(
                    lines
                        .get(ix % BLOCK)
                        .map_or_else(|| (SharedString::default(), Vec::new()), copy),
                ),
                _ => None,
            };
        }
        self.head.get(ix).map(copy)
    }

    /// The view lays out `lines`: read the blocks it lacks, and index the file once the view
    /// nears the end of the head.
    fn want(&self, lines: Range<usize>) {
        let mut file = self.file.borrow_mut();
        if matches!(*file, FileState::Head) {
            if lines.end + INDEX_AHEAD >= self.head.len()
                && let Some((source, reader)) = self.source.as_ref().zip(self.reader.as_ref())
            {
                *file = FileState::Indexing(self.start_reader(source, reader));
            }
        } else if let FileState::Indexed(indexed) = &mut *file {
            indexed.want(lines.start.max(self.head_whole)..lines.end);
        }
    }

    /// Starts the reader thread; returns where to send it block requests.
    fn start_reader(&self, source: &TextSource, reader: &Reader) -> flume::Sender<usize> {
        let (requests, receiver) = flume::unbounded();
        let job = ReadJob {
            source: source.clone(),
            fs: Arc::clone(&reader.fs),
            shell: Arc::clone(&reader.shell),
            tx: reader.tx.clone(),
            generation: self.generation,
            cancel: Arc::clone(&self.cancel),
        };
        spawn_thread("preview-text", move || job.run(&receiver));
        requests
    }

    /// Takes in what the reader thread sent.
    pub(crate) fn apply(&self, update: TextUpdate) {
        let mut file = self.file.borrow_mut();
        let state = std::mem::replace(&mut *file, FileState::Failed);
        *file = match (state, update) {
            (FileState::Indexing(requests), TextUpdate::Index(index)) => {
                FileState::Indexed(Indexed {
                    index,
                    requests,
                    blocks: HashMap::new(),
                    colored: self.source.as_ref().is_none_or(|s| s.lang.is_none()),
                    tick: 0,
                })
            }
            (FileState::Indexing(_), TextUpdate::Failed) => FileState::Failed,
            (FileState::Indexed(mut indexed), update) => {
                indexed.apply(update);
                FileState::Indexed(indexed)
            }
            (state, _) => state,
        };
    }

    /// Tests: whether the whole file is indexed.
    #[cfg(test)]
    pub(crate) fn indexed_for_test(&self) -> bool {
        matches!(&*self.file.borrow(), FileState::Indexed(_))
    }

    /// Tests: line `ix`'s text, if it has been read.
    #[cfg(test)]
    pub(crate) fn line_for_test(&self, ix: usize) -> Option<String> {
        self.line(ix).map(|(text, _)| text.to_string())
    }
}

impl Drop for TextDoc {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Indexed {
    fn want(&mut self, lines: Range<usize>) {
        if lines.is_empty() {
            return;
        }
        self.tick += 1;
        let blocks = lines.start / BLOCK..=(lines.end - 1) / BLOCK;
        for block in blocks.clone() {
            match self.blocks.entry(block) {
                Entry::Vacant(slot) => {
                    slot.insert(Slot::Pending);
                    let _ = self.requests.send(block);
                }
                Entry::Occupied(mut slot) => {
                    if let Slot::Ready {
                        used, done, again, ..
                    } = slot.get_mut()
                    {
                        *used = self.tick;
                        if self.colored && !*done && !*again {
                            *again = true;
                            let _ = self.requests.send(block);
                        }
                    }
                }
            }
        }
        self.evict(&blocks);
    }

    /// Drops the blocks least recently shown while over the budget, never those on screen.
    fn evict(&mut self, keep: &std::ops::RangeInclusive<usize>) {
        loop {
            let bytes: usize = self
                .blocks
                .values()
                .map(|slot| match slot {
                    Slot::Ready { bytes, .. } => *bytes,
                    Slot::Pending => 0,
                })
                .sum();
            if self.blocks.len() <= CACHE_BLOCKS && bytes <= CACHE_BYTES {
                return;
            }
            let oldest = self
                .blocks
                .iter()
                .filter_map(|(block, slot)| match slot {
                    Slot::Ready { used, .. } if !keep.contains(block) => Some((*used, *block)),
                    _ => None,
                })
                .min()
                .map(|(_, block)| block);
            let Some(oldest) = oldest else { return };
            self.blocks.remove(&oldest);
        }
    }

    fn apply(&mut self, update: TextUpdate) {
        match update {
            TextUpdate::Block { block, lines, done } => {
                let bytes = lines.iter().map(|l| l.text.len()).sum();
                self.blocks.insert(
                    block,
                    Slot::Ready {
                        lines,
                        done,
                        again: false,
                        used: self.tick,
                        bytes,
                    },
                );
            }
            TextUpdate::Skipped(blocks) => {
                for block in blocks {
                    if matches!(self.blocks.get(&block), Some(Slot::Pending)) {
                        self.blocks.remove(&block);
                    }
                }
            }
            TextUpdate::Colored => self.colored = true,
            TextUpdate::Index(_) | TextUpdate::Failed => {}
        }
    }
}

/// A document's reader thread: indexes the file, then reads blocks as asked.
struct ReadJob {
    source: TextSource,
    fs: Arc<dyn Fs>,
    shell: Arc<dyn Shell>,
    tx: flume::Sender<AppEvent>,
    generation: u64,
    cancel: Arc<AtomicBool>,
}

impl ReadJob {
    fn send(&self, update: TextUpdate) {
        let _ = self.tx.send(AppEvent::PreviewText {
            generation: self.generation,
            update,
        });
    }

    fn run(&self, requests: &flume::Receiver<usize>) {
        let index = self.fs.open(&self.source.path).and_then(|mut file| {
            file.seek(SeekFrom::Start(self.source.first))?;
            LineIndex::build(
                &mut *file,
                self.source.first,
                self.source.encoding,
                &self.cancel,
            )
        });
        let index = match index {
            Ok(Some(index)) => Arc::new(index),
            Ok(None) => return,
            Err(err) => {
                log::warn!("preview: couldn't index the text: {err}");
                self.send(TextUpdate::Failed);
                return;
            }
        };
        log::debug!(
            "preview: indexed {} lines ({} KB of offsets)",
            index.len(),
            index.heap_bytes() / 1024
        );
        self.send(TextUpdate::Index(Arc::clone(&index)));
        let carries = if let Some(lang) = self.source.lang {
            let Some(carries) = self.carries(&index, lang, requests) else {
                return;
            };
            carries
        } else {
            Vec::new()
        };
        while let Ok(first) = requests.recv() {
            let mut batch = vec![first];
            batch.extend(requests.try_iter());
            self.serve(&index, batch, &carries);
        }
    }

    /// Where each block starts for the highlighter, worked out block by block from the top.
    /// Requests for the screen are served in between. `None` once the document is gone.
    fn carries(
        &self,
        index: &LineIndex,
        lang: &'static Lang,
        requests: &flume::Receiver<usize>,
    ) -> Option<Vec<Carry>> {
        let blocks = index.len().div_ceil(BLOCK);
        let mut carries = Vec::with_capacity(blocks);
        let mut carry = Carry::Code;
        let mut file = self.fs.open(&self.source.path).ok();
        for block in 0..blocks {
            if self.cancel.load(Ordering::Relaxed) || requests.is_disconnected() {
                return None;
            }
            let batch: Vec<usize> = requests.try_iter().collect();
            if !batch.is_empty() {
                self.serve(index, batch, &carries);
            }
            carries.push(carry);
            carry = file
                .as_mut()
                .and_then(|file| self.read(&mut **file, index, block).ok())
                .map_or(Carry::Code, |texts| {
                    highlight_from(&texts.join("\n"), lang, carry).1
                });
        }
        self.send(TextUpdate::Colored);
        Some(carries)
    }

    /// Reads the asked blocks, the newest first; when many piled up, only the newest few.
    fn serve(&self, index: &LineIndex, batch: Vec<usize>, carries: &[Carry]) {
        let mut newest: Vec<usize> = Vec::new();
        for block in batch.into_iter().rev() {
            if !newest.contains(&block) {
                newest.push(block);
            }
        }
        let skipped = newest.split_off(newest.len().min(QUEUE_KEEP));
        if !skipped.is_empty() {
            self.send(TextUpdate::Skipped(skipped));
        }
        let mut file = self.fs.open(&self.source.path).ok();
        for block in newest {
            let texts = file
                .as_mut()
                .and_then(|file| self.read(&mut **file, index, block).ok())
                .unwrap_or_default();
            let carry = self.source.lang.zip(carries.get(block).copied());
            self.send(TextUpdate::Block {
                block,
                lines: code_lines(texts, carry),
                done: self.source.lang.is_none() || carry.is_some(),
            });
        }
    }

    fn read<R: Read + Seek + ?Sized>(
        &self,
        file: &mut R,
        index: &LineIndex,
        block: usize,
    ) -> std::io::Result<Vec<String>> {
        let legacy = |bytes: &[u8]| self.shell.decode_ansi(bytes);
        read_lines(
            file,
            index,
            block * BLOCK..(block + 1) * BLOCK,
            LINE_MAX,
            &legacy,
        )
    }
}

/// Lines read from the file, highlighted from `color`'s carry when given.
fn code_lines(texts: Vec<String>, color: Option<(&Lang, Carry)>) -> Vec<CodeLine> {
    let Some((lang, carry)) = color else {
        return texts
            .into_iter()
            .map(|text| CodeLine {
                text: text.into(),
                marks: Vec::new(),
            })
            .collect();
    };
    let joined = texts.join("\n");
    let (marks, _) = highlight_from(&joined, lang, carry);
    let mut at = 0;
    let spans = texts.iter().map(|text| {
        let span = (at, at + text.len());
        at += text.len() + 1;
        span
    });
    lines_with_marks(&joined, spans, &marks)
}

/// What a layout model's estimates were made for; any change starts it over.
#[derive(Clone, Copy, PartialEq)]
struct LayoutKey {
    line_h: Pixels,
    text_w: Pixels,
    wrap: bool,
    count: usize,
}

/// A view's model of its lines' heights, in rows.
#[derive(Default)]
pub(crate) struct TextLayout {
    key: Option<LayoutKey>,
    /// Characters per row for the estimates; 0 while unknown (one row a line)
    per_row: usize,
    /// Rows of the lines laid out so far, where they differ from the estimate
    measured: HashMap<usize, u32>,
    /// Rows of each run of [`LAYOUT_RUN`] lines: estimates, corrected by `measured`
    runs: Vec<u64>,
    /// First line on screen at the last frame
    anchor: usize,
    /// Rows to scroll by at the next frame, so the first line on screen stays where it is
    shift: i64,
    /// Without wrapping: the widest text laid out, so the sideways range only grows
    widest: Pixels,
}

/// The lines a frame lays out.
struct Plan {
    /// First line laid out (above the screen)
    start: usize,
    /// First line on screen and the row it starts at
    anchor: usize,
    anchor_y: u64,
    /// After the last line laid out
    end: usize,
    /// Rows of the whole text
    total: u64,
}

impl TextLayout {
    /// Estimated rows of a line `len` bytes long.
    fn est(&self, len: usize) -> u32 {
        if self.per_row == 0 {
            1
        } else {
            len.div_ceil(self.per_row).max(1) as u32
        }
    }

    fn rows(&self, ix: usize, len: &dyn Fn(usize) -> usize) -> u32 {
        self.measured
            .get(&ix)
            .copied()
            .unwrap_or_else(|| self.est(len(ix)))
    }

    /// Row line `ix` starts at.
    fn y_of(&self, ix: usize, len: &dyn Fn(usize) -> usize) -> u64 {
        let run = ix / LAYOUT_RUN;
        let before: u64 = self.runs.iter().take(run).sum();
        before
            + (run * LAYOUT_RUN..ix)
                .map(|j| u64::from(self.rows(j, len)))
                .sum::<u64>()
    }

    /// Makes the model fit `key`, keeping the first line on screen where it is.
    fn prepare(&mut self, key: LayoutKey, per_row: usize, len: &dyn Fn(usize) -> usize) {
        if self.key == Some(key) {
            return;
        }
        let was = self.key.map(|_| self.y_of(self.anchor, len));
        // Text is as wide at any view width; only a new text size changes it.
        if self.key.is_none_or(|old| old.line_h != key.line_h) {
            self.widest = Pixels::ZERO;
        }
        self.key = Some(key);
        self.per_row = if key.wrap { per_row } else { 0 };
        self.measured.clear();
        // Unwrapped (or before the width is known) every line is one row: no need to look at
        // each line's length. Wrapped, this goes through every line once per width.
        self.runs = (0..key.count.div_ceil(LAYOUT_RUN))
            .map(|run| {
                let lines = run * LAYOUT_RUN..((run + 1) * LAYOUT_RUN).min(key.count);
                if self.per_row == 0 {
                    lines.len() as u64
                } else {
                    lines.map(|ix| u64::from(self.est(len(ix)))).sum()
                }
            })
            .collect();
        self.anchor = self.anchor.min(key.count.saturating_sub(1));
        if let Some(was) = was {
            self.shift += rows_between(was, self.y_of(self.anchor, len));
        }
    }

    /// Forgets the heights of lines far from the screen once too many are remembered.
    fn forget_far(&mut self, len: &dyn Fn(usize) -> usize) {
        if self.measured.len() <= MEASURED_MAX {
            return;
        }
        let anchor = self.anchor;
        let far: Vec<(usize, u32)> = self
            .measured
            .iter()
            .filter(|&(&ix, _)| ix.abs_diff(anchor) > MEASURED_KEEP)
            .map(|(&ix, &rows)| (ix, rows))
            .collect();
        for (ix, rows) in far {
            self.measured.remove(&ix);
            let delta = i64::from(self.est(len(ix))) - i64::from(rows);
            self.adjust(ix, delta);
        }
    }

    /// Line `ix` takes `delta` rows more than the model had.
    fn adjust(&mut self, ix: usize, delta: i64) {
        if let Some(run) = self.runs.get_mut(ix / LAYOUT_RUN) {
            *run = run.saturating_add_signed(delta);
        }
        if ix < self.anchor {
            self.shift += delta;
        }
    }

    /// Line `ix` was laid out `rows` rows tall; `est` is what the model had for it unmeasured.
    fn record(&mut self, ix: usize, rows: u32, est: u32) {
        let old = self.measured.get(&ix).copied().unwrap_or(est);
        if old == rows {
            return;
        }
        if rows == est {
            self.measured.remove(&ix);
        } else {
            self.measured.insert(ix, rows);
        }
        self.adjust(ix, i64::from(rows) - i64::from(old));
    }

    /// The lines to lay out for a view whose top is at row `row_at`, `view_rows` tall.
    fn plan(
        &mut self,
        count: usize,
        row_at: u64,
        view_rows: u64,
        len: &dyn Fn(usize) -> usize,
    ) -> Plan {
        let total: u64 = self.runs.iter().sum();
        // The line holding row `row_at`: whole runs first, then line by line.
        let mut y = 0;
        let mut anchor = 0;
        for (run, &rows) in self.runs.iter().enumerate() {
            if y + rows > row_at {
                anchor = run * LAYOUT_RUN;
                break;
            }
            y += rows;
            anchor = ((run + 1) * LAYOUT_RUN).min(count);
        }
        while anchor + 1 < count {
            let rows = u64::from(self.rows(anchor, len));
            if y + rows > row_at {
                break;
            }
            y += rows;
            anchor += 1;
        }
        if anchor >= count {
            anchor = count.saturating_sub(1);
            y = self.y_of(anchor, len);
        }
        self.anchor = anchor;
        // Down to the bottom of the screen, then a few rows more.
        let need = row_at.saturating_sub(y) + view_rows;
        let mut end = anchor;
        let mut rows = 0;
        while end < count && rows < need + OVERSCAN {
            rows += u64::from(self.rows(end, len));
            end += 1;
        }
        // A few rows above it, so a line scrolled into view from above is laid out already.
        let mut start = anchor;
        let mut above = 0;
        while start > 0 && above < OVERSCAN {
            start -= 1;
            above += u64::from(self.rows(start, len));
        }
        Plan {
            start,
            anchor,
            anchor_y: y,
            end,
            total,
        }
    }
}

/// Rows from `from` to `to`, negative going up.
fn rows_between(from: u64, to: u64) -> i64 {
    i64::try_from(i128::from(to) - i128::from(from)).unwrap_or(0)
}

/// What the rows of a view look like.
#[derive(Clone, Copy)]
struct RowLook {
    /// Quick Look: a ruled gutter and roomier lines
    large: bool,
    wrap: bool,
    /// Width of the line numbers
    numbers: Pixels,
    line_h: Pixels,
}

impl RowLook {
    /// Width of everything left of the text.
    fn gutter(self) -> Pixels {
        if self.large {
            self.numbers + theme::s_16() + theme::s_12()
        } else {
            self.numbers + theme::s_8()
        }
    }
}

/// One line's row: its number, and its text (empty while its block is read). `model_rows` is
/// what the model has for it, `est` its estimate.
fn line_row(
    ix: usize,
    line: Option<Line>,
    look: RowLook,
    layout: &Rc<RefCell<TextLayout>>,
    (model_rows, est): (u32, u32),
) -> AnyElement {
    #[cfg(test)]
    BUILT.with(|built| built.set(built.get() + 1));
    let pending = line.is_none();
    let number = div()
        .flex_none()
        .flex()
        .justify_end()
        .text_color(theme::text_disabled())
        .child((ix + 1).to_string());
    let mut text = div()
        .relative()
        .when(look.wrap, |d| d.flex_1().min_w_0())
        .when(!look.wrap, |d| d.flex_none().whitespace_nowrap());
    if let Some((line, marks)) = line {
        let marks = marks.into_iter().map(|(range, token)| {
            (
                range,
                gpui::HighlightStyle {
                    color: Some(theme::code_color(token).into()),
                    ..gpui::HighlightStyle::default()
                },
            )
        });
        text = text.child(gpui::StyledText::new(line).with_highlights(marks));
        if !look.wrap {
            text = text.child(widest_probe(Rc::clone(layout)));
        }
    }
    let cells = if look.large {
        div()
            .flex()
            .child(
                number
                    .w(look.gutter())
                    .pl(theme::s_16())
                    .pr(theme::s_12())
                    .border_r_1()
                    .border_color(theme::line()),
            )
            .child(
                text.pl(theme::s_16())
                    .pr(theme::s_24())
                    .text_color(theme::text()),
            )
    } else {
        div()
            .flex()
            .gap(theme::s_8())
            .child(number.w(look.numbers))
            .child(text.text_color(theme::text_muted()))
    };
    cells
        .relative()
        .debug_selector(move || format!("preview-line-{ix}"))
        // A line still being read keeps the room it will take.
        .when(pending, |d| d.min_h(look.line_h * model_rows as f32))
        .when(look.wrap && !pending, |d| {
            d.child(rows_probe(Rc::clone(layout), ix, est, look.line_h))
        })
        .into_any_element()
}

/// Records how many rows the row it fills was laid out in.
fn rows_probe(
    layout: Rc<RefCell<TextLayout>>,
    ix: usize,
    est: u32,
    line_h: Pixels,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            let rows = (bounds.size.height / line_h).round().max(1.) as u32;
            layout.borrow_mut().record(ix, rows, est);
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Records the width of the unwrapped text it fills.
fn widest_probe(layout: Rc<RefCell<TextLayout>>) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            let mut layout = layout.borrow_mut();
            layout.widest = layout.widest.max(bounds.size.width);
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Text and code: line numbers on the left; long lines wrap and the number stays aligned to the
/// first row. `zoom` is Quick Look's text scale (`None` in the pane, which is smaller and drawn on
/// a sheet). Without `wrap` (tables) lines stay whole and the view scrolls sideways.
pub(super) fn code_view(
    doc: &Rc<TextDoc>,
    zoom: Option<f32>,
    wrap: bool,
    scroll: &ScrollHandle,
) -> AnyElement {
    let large = zoom.is_some();
    let zoom = zoom.unwrap_or(1.);
    let font = if large {
        theme::fs_13() * zoom
    } else {
        theme::fs_12()
    };
    // In pixels the way GPUI works out a line height, so the rows are exactly this tall.
    let spacing = if large {
        gpui::relative(LOOK_LINE_HEIGHT)
    } else {
        gpui::phi()
    };
    let line_h = spacing.to_pixels(font.into(), font).round().max(px(1.));
    let count = doc.count();
    let digits = count.max(1).to_string().len();
    // Line numbers take about 8 px a digit at normal size.
    let look = RowLook {
        large,
        wrap,
        numbers: px((8. * digits as f32 + 8.) * zoom),
        line_h,
    };
    let layout = Rc::clone(doc.layout(large));
    let (plan, sizes, widest) = plan_frame(doc, &layout, look, font, scroll);
    doc.want(plan.start..plan.end);
    let rows = |lines: Range<usize>| -> Vec<AnyElement> {
        lines
            .map(|ix| {
                let size = sizes[ix - plan.start];
                line_row(ix, doc.line(ix), look, &layout, size)
            })
            .collect()
    };
    let content = if wrap {
        wrapped_lines(&plan, line_h, rows)
    } else {
        let below = line_h * count.saturating_sub(plan.end) as f32;
        unwrapped_lines(&plan, look, below, widest, rows)
    };
    // Both axes scroll for a table, and the plain wheel still goes up and down: GPUI only turns
    // the wheel sideways when x alone scrolls.
    let area = div()
        .id("preview-text")
        .debug_selector(|| "preview-text".into())
        .size_full();
    let area = if wrap {
        area.overflow_y_scroll()
    } else {
        area.overflow_scroll().flex().items_start()
    };
    area.track_scroll(scroll)
        .font_family("Cascadia Mono")
        .text_size(font)
        .line_height(line_h)
        .when(large, |d| d.py(theme::s_12()))
        .when(!large, |d| d.p(theme::s_8()).map(theme::preview_sheet))
        .child(content)
        .into_any_element()
}

/// Wrapped lines: those from the first on screen down are placed from its row, the ones above it
/// stacked up from there, so neither moves the line on screen when it turns out taller or shorter
/// than estimated.
fn wrapped_lines(
    plan: &Plan,
    line_h: Pixels,
    rows: impl Fn(Range<usize>) -> Vec<AnyElement>,
) -> gpui::Div {
    let total = line_h * plan.total as f32;
    let top = line_h * plan.anchor_y as f32;
    div()
        .relative()
        .w_full()
        .h(total)
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(total - top)
                .flex()
                .flex_col()
                .children(rows(plan.start..plan.anchor)),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(top)
                .flex()
                .flex_col()
                .children(rows(plan.anchor..plan.end)),
        )
}

/// Unwrapped lines, each one row: blank room above the lines laid out and `below` them. The lines
/// set the width, which may pass the frame's; it only grows while scrolling (`widest`).
fn unwrapped_lines(
    plan: &Plan,
    look: RowLook,
    below: Pixels,
    widest: Pixels,
    rows: impl Fn(Range<usize>) -> Vec<AnyElement>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .min_w_full()
        .child(div().h(look.line_h * plan.start as f32))
        .children(rows(plan.start..plan.end))
        .child(div().h(below))
        .child(div().h(px(0.)).w(look.gutter() + widest))
}

/// Brings `layout` up to date for this frame and picks the lines to lay out; with each line's
/// rows in the model and its estimate, and the widest unwrapped text so far.
fn plan_frame(
    doc: &TextDoc,
    layout: &RefCell<TextLayout>,
    look: RowLook,
    font: Pixels,
    scroll: &ScrollHandle,
) -> (Plan, Vec<(u32, u32)>, Pixels) {
    let count = doc.count();
    // The view's own padding, and everything beside the text across.
    let (pad, chrome) = if look.large {
        (theme::s_12(), look.gutter() + theme::s_16() + theme::s_24())
    } else {
        (theme::s_8(), look.gutter() + theme::s_8() * 2. + px(2.))
    };
    // From the last frame; unknown (zero) in the first.
    let view = scroll.bounds().size;
    let text_w = (view.width - chrome).max(Pixels::ZERO).round();
    let per_row = if view.width > Pixels::ZERO {
        (text_w / (font * CHAR_EM)).floor().max(1.) as usize
    } else {
        0
    };
    let len = |ix: usize| doc.len_of(ix);
    let mut lay = layout.borrow_mut();
    let key = LayoutKey {
        line_h: look.line_h,
        text_w,
        wrap: look.wrap,
        count,
    };
    lay.prepare(key, per_row, &len);
    lay.forget_far(&len);
    let mut top = -scroll.offset().y;
    if lay.shift != 0 {
        top = (top + look.line_h * lay.shift as f32).max(Pixels::ZERO);
        scroll.set_offset(point(scroll.offset().x, -top));
        lay.shift = 0;
    }
    let row_at = ((top - pad).max(Pixels::ZERO) / look.line_h).floor() as u64;
    let view_rows = if view.height > Pixels::ZERO {
        (view.height / look.line_h).ceil() as u64 + 1
    } else {
        FIRST_ROWS
    };
    let plan = lay.plan(count, row_at, view_rows, &len);
    let sizes = (plan.start..plan.end)
        .map(|ix| (lay.rows(ix, &len), lay.est(len(ix))))
        .collect();
    (plan, sizes, lay.widest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block read from the middle of a file is colored from where the block before left off.
    #[test]
    fn blocks_are_colored_from_their_carry() {
        let lang = arke_core::highlight::lang_for("rs").expect("rust");
        let lines = code_lines(
            vec!["still */ let x".into(), "/* open".into()],
            Some((lang, Carry::Comment)),
        );
        assert_eq!(
            lines[0].marks,
            [(0..8, Token::Comment), (9..12, Token::Keyword)]
        );
        assert_eq!(lines[1].marks, [(0..7, Token::Comment)]);
        assert!(code_lines(vec!["let".into()], None)[0].marks.is_empty());
    }

    /// A line above the screen laid out taller than estimated scrolls the next frame by the
    /// difference, so the screen doesn't move; one below changes nothing above it.
    #[test]
    fn heights_learned_above_the_screen_keep_it_still() {
        let len = |_: usize| 10;
        let mut layout = TextLayout::default();
        let key = LayoutKey {
            line_h: px(10.),
            text_w: px(100.),
            wrap: true,
            count: 1000,
        };
        layout.prepare(key, 20, &len);
        let plan = layout.plan(1000, 500, 30, &len);
        assert_eq!((plan.anchor, plan.anchor_y, plan.total), (500, 500, 1000));
        assert!(plan.start < 500 && plan.end > 530);
        layout.record(498, 3, 1);
        assert_eq!(layout.shift, 2);
        assert_eq!(layout.y_of(500, &len), 502);
        layout.record(510, 2, 1);
        assert_eq!(layout.shift, 2);
        // Back to its estimate.
        layout.record(498, 1, 1);
        assert_eq!(layout.shift, 0);
        assert_eq!(layout.y_of(500, &len), 500);
    }

    /// A narrower view wraps every line into more rows; the first line on screen stays first.
    /// Scrolled past the end, the last line is the one shown.
    #[test]
    fn a_new_width_keeps_the_first_line_on_screen() {
        let len = |_: usize| 50;
        let mut layout = TextLayout::default();
        let key = LayoutKey {
            line_h: px(10.),
            text_w: px(500.),
            wrap: true,
            count: 100,
        };
        layout.prepare(key, 100, &len);
        layout.plan(100, 40, 10, &len);
        assert_eq!(layout.anchor, 40);
        layout.prepare(
            LayoutKey {
                text_w: px(250.),
                ..key
            },
            25,
            &len,
        );
        assert_eq!(layout.shift, 40, "line 40 now starts at row 80");
        let plan = layout.plan(100, 10_000, 10, &len);
        assert_eq!((plan.anchor, plan.end), (99, 100));
    }
}
