//! The rendered lines a full-screen run keeps: a ring with a capped line
//! count, the viewport over it, and follow mode.
//!
//! A full-screen run has no terminal scrollback of its own, so the lines the
//! app commits live here. The ring drops the oldest line past
//! [`TRANSCRIPT_RING_LINES`] and keeps its count, so a long run never holds
//! unbounded memory, and a frame draws only the rows the viewport shows: the
//! work of a scroll frame does not grow with the ring.
//!
//! The text of every stored line sits in one reused byte buffer and a run is
//! a span of it, so a commit allocates only when the buffer or the run list is
//! out of room: a warm ring stores a line without a block of its own.
//!
//! A committed block a fold key can open keeps the source it was drawn from, so
//! Tab and Ctrl+O draw its lines again in full screen: the block takes the room
//! of the lines it drew before, the lines after it shift once on the key, and a
//! later frame draws the ring as it found it.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use super::cell::GraphemeArena;
use super::grid::{Grid, is_control};
use super::{Run, Source};

/// Most lines the ring keeps. Past this the oldest line leaves the ring and
/// only its count stays, so a long run never holds unbounded memory.
pub const TRANSCRIPT_RING_LINES: usize = 20_000;

/// The digits of a `u64`, in a buffer on the stack: the line that names the
/// lines that left the ring allocates nothing.
const DIGITS: usize = 20;

/// One run of a stored line: a span of the ring's text buffer, in one style.
#[derive(Debug, Clone, Copy)]
pub(super) struct StoredRun {
    offset: usize,
    len: usize,
    style: u32,
}

impl StoredRun {
    /// The text of the run, read from the ring's byte buffer. The ring stores
    /// only whole UTF-8 runs, so the span always decodes.
    fn text<'a>(&self, buffer: &'a [u8]) -> &'a str {
        buffer
            .get(self.offset..self.offset.saturating_add(self.len))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or_default()
    }
}

/// One stored line: where its runs sit in the ring's run vector, and how many
/// rows it takes at the width of the area.
#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    len: usize,
    /// The rows the line takes at the area's width, measured when the line
    /// arrives and again whenever the width changes.
    rows: u32,
}

/// One committed block the ring can draw again when its fold state changes:
/// the number the fold state knows it by, the lines it took, and the item the
/// live block drew from, held once so a re-render copies nothing.
pub struct Block {
    /// The block's number, from the fold state.
    fold: u64,
    /// The line number of its first stored line.
    line: u64,
    /// How many stored lines it takes.
    lines: u64,
    /// The fold state its lines were drawn with.
    open: bool,
    /// What its lines were drawn from, while the app holds it back to grow it.
    source: Option<Source>,
}

impl fmt::Debug for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Block")
            .field("fold", &self.fold)
            .field("line", &self.line)
            .field("lines", &self.lines)
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

/// A place in the ring: a stored line by number, and the row within it that
/// starts at the top of the viewport.
///
/// A stored line wider than the area draws as several rows, so a viewport that
/// only knew the line would lose the row it started on when the width changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Mark {
    line: u64,
    row: u32,
}

/// The rows of the grid the transcript draws into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

/// The stored lines of a full-screen run, the viewport over them and follow
/// mode.
#[derive(Debug)]
pub struct Transcript {
    /// The text of every stored line, oldest first.
    text: Vec<u8>,
    /// The runs of every stored line, in order.
    runs: Vec<StoredRun>,
    /// The stored lines, oldest first.
    lines: VecDeque<Line>,
    /// The committed blocks the ring still holds, oldest first, one entry per
    /// block whose fold state can change and draw its lines again.
    blocks: VecDeque<Block>,
    /// The index in `runs` of the first run a stored line still uses.
    head: usize,
    /// The lines that left the ring.
    dropped: u64,
    /// The first row the viewport shows, as a place in the ring: `dropped` is
    /// the first line the ring still holds.
    top: Mark,
    /// Stick to the bottom while lines arrive.
    follow: bool,
    /// The rows the transcript draws into.
    area: Area,
    /// The style id of the dim line that names the lines that left the ring.
    dim: u32,
    /// Lines the last draw visited, for the frame-cost test.
    #[cfg(test)]
    visited: usize,
}

impl Transcript {
    /// An empty transcript, following the bottom.
    pub fn new() -> Self {
        Transcript {
            text: Vec::new(),
            runs: Vec::new(),
            lines: VecDeque::new(),
            blocks: VecDeque::new(),
            head: 0,
            dropped: 0,
            top: Mark::default(),
            follow: true,
            area: Area {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            dim: 0,
            #[cfg(test)]
            visited: 0,
        }
    }

    /// The style id of the line that names the lines that left the ring.
    pub fn dim_style(&self) -> u32 {
        self.dim
    }

    /// Set the style id of the line that names the lines that left the ring.
    pub fn set_dim_style(&mut self, style: u32) {
        self.dim = style;
    }

    /// Store one rendered line, the lines and runs `commit` hands over. The
    /// oldest line leaves the ring when it is full.
    pub(super) fn push(&mut self, runs: &[Run<'_>]) {
        let start = self.runs.len();
        for run in runs {
            let offset = self.text.len();
            self.text.extend_from_slice(run.text.as_bytes());
            self.runs.push(StoredRun {
                offset,
                len: run.text.len(),
                style: run.style,
            });
        }
        let rows = line_rows(runs.iter().map(|run| (run.text, run.style)), self.width());
        self.lines.push_back(Line {
            start,
            len: runs.len(),
            rows,
        });
        self.cap();
    }

    /// Drop the oldest lines past the ring's cap, and the room of the blocks
    /// they took with them: a block that leaves the ring cannot be drawn again.
    fn cap(&mut self) {
        while self.lines.len() > TRANSCRIPT_RING_LINES {
            let Some(oldest) = self.lines.pop_front() else {
                break;
            };
            self.head = oldest.start + oldest.len;
            self.dropped += 1;
            self.trim_blocks();
            self.compact();
        }
        if self.follow {
            self.top = self.max_top();
        } else if self.top.line < self.dropped {
            self.top.line = self.dropped;
        }
    }

    /// Reclaim the runs and the bytes of the lines that left the ring, once
    /// half of either is dead: one shift, amortized over the lines that left
    /// it.
    fn compact(&mut self) {
        if self.head > self.runs.len() / 2 {
            self.runs.drain(..self.head);
            for line in &mut self.lines {
                line.start -= self.head;
            }
            self.head = 0;
        }
        // The runs come in order, so the bytes before the first live run are
        // the bytes of the lines that left.
        let dead = self
            .runs
            .get(self.head)
            .map_or(self.text.len(), |run| run.offset);
        if dead > self.text.len() / 2 {
            self.text.drain(..dead);
            for run in &mut self.runs[self.head..] {
                run.offset -= dead;
            }
        }
    }

    /// Store a committed block that can be drawn again: the number the fold
    /// state knows it by, the line its first line went to, how many lines it
    /// took, the fold state they were drawn with, and the source they came
    /// from. The lines a block lost to the ring leave it nothing to draw.
    pub(super) fn add_block(
        &mut self,
        fold: u64,
        line: u64,
        lines: usize,
        open: bool,
        source: Source,
    ) {
        let end = line.saturating_add(lines as u64);
        let line = line.max(self.dropped);
        if end <= line {
            return;
        }
        self.blocks.push_back(Block {
            fold,
            line,
            lines: end - line,
            open,
            source: Some(source),
        });
    }

    /// Drop the blocks whose lines all left the ring, and clip the one the
    /// dropped lines reached: a block that left the ring cannot be drawn
    /// again.
    fn trim_blocks(&mut self) {
        while let Some(block) = self.blocks.front_mut() {
            let end = block.line.saturating_add(block.lines);
            if end > self.dropped {
                if block.line < self.dropped {
                    block.line = self.dropped;
                    block.lines = end - self.dropped;
                }
                return;
            }
            self.blocks.pop_front();
        }
    }

    /// How many committed blocks the ring still holds.
    pub(crate) fn blocks(&self) -> usize {
        self.blocks.len()
    }

    /// The number of the committed block at `index`, oldest first.
    pub(crate) fn block_fold(&self, index: usize) -> Option<u64> {
        self.blocks.get(index).map(|block| block.fold)
    }

    /// The number of the newest committed block the ring holds.
    pub(crate) fn newest_block(&self) -> Option<u64> {
        self.blocks.back().map(|block| block.fold)
    }

    /// The number of the committed block that holds `line`, if one does.
    pub(crate) fn block_at(&self, line: u64) -> Option<u64> {
        self.blocks
            .iter()
            .rev()
            .find(|block| block.line <= line && line < block.line.saturating_add(block.lines))
            .map(|block| block.fold)
    }

    /// The source the committed block `fold` draws from again, while the ring
    /// still holds it.
    pub(crate) fn source(&self, fold: u64) -> Option<Source> {
        self.blocks
            .iter()
            .find(|block| block.fold == fold)
            .and_then(|block| block.source.clone())
    }

    /// Take the source of the committed block `fold` out of the ring, so the app
    /// can grow the block it drew: its lines stay until the app draws them
    /// again.
    pub(crate) fn take_source(&mut self, fold: u64) -> Option<Source> {
        self.blocks
            .iter_mut()
            .find(|block| block.fold == fold)
            .and_then(|block| block.source.take())
    }

    /// Give the committed block `fold` the source it draws from again, after a
    /// re-render that could not use it.
    pub(crate) fn set_source(&mut self, fold: u64, source: Source) {
        if let Some(block) = self.blocks.iter_mut().find(|block| block.fold == fold) {
            block.source = Some(source);
        }
    }

    /// The fold state the committed block `fold` was drawn with, while the ring
    /// still holds it.
    pub(crate) fn rendered_open(&self, fold: u64) -> Option<bool> {
        self.blocks
            .iter()
            .find(|block| block.fold == fold)
            .map(|block| block.open)
    }

    /// Draw the committed block `fold` again with the fold state `open`:
    /// `runs` and `lines` are the rows it draws now, and they take the room of
    /// the lines it drew before, so the lines after it move and the view keeps
    /// its top line, or stays at the bottom while it follows.
    ///
    /// The old runs and their bytes leave the ring's buffers: the runs after
    /// the block shift, and the bytes of the tail and of the new lines swap
    /// places, so a re-render copies neither the ring nor the block. The lines
    /// after it move once, on the key that reopened it, and a later frame draws
    /// the ring as it found it. Answers whether the ring still held the block.
    pub(super) fn replace(
        &mut self,
        fold: u64,
        open: bool,
        runs: &[Run<'_>],
        lines: &[Range<usize>],
    ) -> bool {
        let Some(index) = self
            .blocks
            .iter()
            .position(|block| block.fold == fold && block.lines > 0)
        else {
            return false;
        };
        if lines.is_empty() {
            return false;
        }
        let (line, old) = (self.blocks[index].line, self.blocks[index].lines as usize);
        let local = (line - self.dropped) as usize;
        // The block's runs and its bytes are one stretch of each buffer, so the
        // tail after them and the new lines can trade places.
        let Some((first, last)) = self
            .lines
            .get(local)
            .zip(self.lines.get(local + old - 1))
            .map(|(first, last)| (*first, *last))
        else {
            return false;
        };
        let (r0, r1) = (first.start, last.start + last.len);
        if r0 == r1 {
            // A block of empty lines takes no room and holds no source to draw.
            return false;
        }
        let Some((head, tail)) = self
            .runs
            .get(r0)
            .zip(self.runs.get(r1 - 1))
            .map(|(head, tail)| (*head, *tail))
        else {
            return false;
        };
        let (t0, t1) = (head.offset, tail.offset + tail.len);
        // The new runs and their text go after the ones the ring holds.
        let before = self.runs.len();
        let text_before = self.text.len();
        let mut fresh = Vec::with_capacity(lines.len());
        for span in lines {
            let start = self.runs.len();
            for run in &runs[span.clone()] {
                let offset = self.text.len();
                self.text.extend_from_slice(run.text.as_bytes());
                self.runs.push(StoredRun {
                    offset,
                    len: run.text.len(),
                    style: run.style,
                });
            }
            let rows = line_rows(
                runs[span.clone()].iter().map(|run| (run.text, run.style)),
                self.width(),
            );
            fresh.push(Line {
                start,
                len: span.len(),
                rows,
            });
        }
        let added = fresh.len();
        let new_runs = self.runs.len() - before;
        let old_runs = r1 - r0;
        let (new_bytes, tail_bytes) = (self.text.len() - text_before, text_before - t1);
        // The block's old runs leave the ring and its new ones take their room.
        self.runs.drain(r0..r1);
        self.runs[r0..].rotate_right(new_runs);
        let mut start = r0;
        for run in &mut fresh {
            run.start = start;
            start += run.len;
        }
        // The bytes of the tail and of the new lines swap over the old ones.
        let text_end = self.text.len();
        self.text.copy_within(t1..text_end, t0);
        self.text.truncate(t0 + tail_bytes + new_bytes);
        self.text[t0..].rotate_right(new_bytes);
        // The runs come in line order again: their bytes follow them.
        let mut cursor = if self.head < r0 {
            self.runs[self.head].offset
        } else {
            t0
        };
        for run in &mut self.runs[self.head..] {
            run.offset = cursor;
            cursor += run.len;
        }
        // The lines after the block move by the runs it gained or lost.
        for after in self.lines.iter_mut().skip(local + old) {
            after.start = after.start - old_runs + new_runs;
        }
        let _ = self.lines.drain(local..local + old);
        for line in &fresh {
            self.lines.push_back(*line);
        }
        let grown = added as i64 - old as i64;
        for after in self.blocks.iter_mut().skip(index + 1) {
            after.line = after.line.saturating_add_signed(grown);
        }
        let block = &mut self.blocks[index];
        block.lines = added as u64;
        block.open = open;
        let deque = self.lines.make_contiguous();
        deque[local..].rotate_right(added);
        self.place(line, old as u64, added as u64);
        self.cap();
        true
    }

    /// Keep the view where it was after the block that started at `line` and
    /// took `old` lines takes `new`: a following viewport stays at the bottom,
    /// and one the user scrolled keeps the line it started on.
    fn place(&mut self, line: u64, old: u64, new: u64) {
        let end = line.saturating_add(old);
        let grown = new as i64 - old as i64;
        if self.follow {
            self.top = self.max_top();
        } else if self.top.line >= end {
            self.top.line = self.top.line.saturating_add_signed(grown);
        } else if self.top.line >= line {
            self.top.line = self.top.line.min(line + new.saturating_sub(1));
        }
        self.top.line = self.top.line.max(self.dropped);
        let max = self.max_top();
        if self.top > max {
            self.top = max;
        }
        let rows = self.rows_of(self.top.line);
        self.top.row = self.top.row.min(rows.saturating_sub(1));
    }

    /// The bytes, the runs and the lines the ring stores, for the allocation
    /// guard: a re-render grows them by the lines it adds and nothing else.
    #[cfg(test)]
    pub fn lengths(&self) -> (usize, usize, usize) {
        (self.text.len(), self.runs.len(), self.lines.len())
    }

    /// Every line ever stored, the ones that left the ring included.
    pub fn len(&self) -> u64 {
        self.dropped + self.lines.len() as u64
    }

    /// True before a line is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The lines the ring still holds.
    pub fn ring_len(&self) -> usize {
        self.lines.len()
    }

    /// Drop every stored line, keeping the buffers: the lines of one
    /// full-screen run are printed once, and a second close prints nothing.
    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.runs.clear();
        self.lines.clear();
        self.blocks.clear();
        self.head = 0;
        self.dropped = 0;
        self.top = Mark::default();
        self.follow = true;
    }

    /// The lines that left the ring.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The line the viewport starts on, as a line number.
    pub fn top(&self) -> u64 {
        self.top.line
    }

    /// The rows the viewport shows.
    pub fn height(&self) -> u16 {
        self.area.height
    }

    /// Whether the viewport follows the lines that arrive.
    pub fn follow(&self) -> bool {
        self.follow
    }

    /// Set the rows of the grid the transcript draws into. A resize re-clamps
    /// the viewport and a following one stays at the bottom.
    ///
    /// A narrower or wider area re-measures the rows of every stored line, once
    /// per resize or sidebar toggle, so a line the ring already holds wraps at
    /// the new width instead of losing its right-hand cells. The viewport keeps
    /// the stored line it started on.
    pub fn set_area(&mut self, x: u16, y: u16, width: u16, height: u16) {
        let rewrapped = width != self.area.width;
        self.area = Area {
            x,
            y,
            width,
            height,
        };
        if rewrapped {
            let (runs, text, width) = (&self.runs, &self.text, self.width());
            for line in &mut self.lines {
                let live = runs
                    .get(line.start..line.start.saturating_add(line.len))
                    .unwrap_or_default();
                line.rows = line_rows(live.iter().map(|run| (run.text(text), run.style)), width);
            }
            let rows = self.rows_of(self.top.line);
            self.top.row = self.top.row.min(rows.saturating_sub(1));
        }
        self.top.line = self.top.line.max(self.dropped);
        let max = self.max_top();
        if self.follow || self.top > max {
            self.top = max;
        }
    }

    /// Move the viewport up by `rows`, ending follow mode unless it reaches the
    /// bottom.
    pub fn scroll_up(&mut self, rows: u64) {
        let mut mark = self.top;
        let mut left = rows;
        while left > 0 {
            if mark.row > 0 {
                let step = u64::from(mark.row).min(left);
                mark.row -= step as u32;
                left -= step;
                continue;
            }
            if mark.line <= self.dropped {
                break;
            }
            mark.line -= 1;
            mark.row = self.rows_of(mark.line).saturating_sub(1);
            left -= 1;
        }
        self.top = mark;
        self.follow = self.top >= self.max_top();
    }

    /// Move the viewport down by `rows`; at the bottom follow mode is back on.
    pub fn scroll_down(&mut self, rows: u64) {
        let mut mark = self.top;
        let mut left = rows;
        while left > 0 {
            let rows = self.rows_of(mark.line);
            if mark.row + 1 < rows {
                let step = u64::from(rows - 1 - mark.row).min(left);
                mark.row += step as u32;
                left -= step;
                continue;
            }
            if mark.line + 1 >= self.len() {
                break;
            }
            mark.line += 1;
            mark.row = 0;
            left -= 1;
        }
        let max = self.max_top();
        self.top = mark.min(max);
        self.follow = self.top >= max;
    }

    /// Move the viewport up by one page.
    pub fn page_up(&mut self) {
        self.scroll_up(u64::from(self.area.height.max(1)));
    }

    /// Move the viewport down by one page.
    pub fn page_down(&mut self) {
        self.scroll_down(u64::from(self.area.height.max(1)));
    }

    /// Go back to the bottom and follow the lines that arrive.
    pub fn scroll_to_bottom(&mut self) {
        self.top = self.max_top();
        self.follow = true;
    }

    /// Draw the rows the viewport shows from the ring, and blank the rows below
    /// them. At the top of the ring one dim line names the lines that left it.
    ///
    /// A stored line wider than the area takes the rows after it, one row per
    /// slice of it, so a line the ring wrapped at a wider area loses nothing
    /// when the area narrows. Only the rows of the area are touched and only the
    /// pieces of the lines they show, so the work does not grow with the ring.
    pub fn draw(&mut self, grid: &mut Grid, arena: &mut GraphemeArena) {
        #[cfg(test)]
        let mut visited = 0usize;
        let Area {
            x,
            y,
            width,
            height,
        } = self.area;
        if width == 0 || height == 0 {
            return;
        }
        let right = x.saturating_add(width);
        let bottom = y.saturating_add(height);
        let mut row = y;
        if let Some(dropped) = self.notice() {
            grid.clear_span(row, x, width);
            let mut digits = [0u8; DIGITS];
            let mut cx = grid.put_str_until(x, row, "... ", self.dim, arena, right);
            cx = grid.put_str_until(
                cx,
                row,
                digits_of(&mut digits, dropped),
                self.dim,
                arena,
                right,
            );
            grid.put_str_until(cx, row, " earlier lines", self.dim, arena, right);
            row += 1;
        }
        let mut index = self.top.line;
        let mut skip = self.top.row;
        let mut cx = x;
        while index < self.len() && row < bottom {
            let mut last = None;
            for (text, style, piece) in Pieces::new(self.runs_of(index), width) {
                if piece < skip {
                    continue;
                }
                if row >= bottom {
                    break;
                }
                if last != Some(piece) {
                    if last.is_some() {
                        row += 1;
                        if row >= bottom {
                            break;
                        }
                    }
                    cx = x;
                    grid.clear_span(row, x, width);
                    #[cfg(test)]
                    {
                        visited += 1;
                    }
                    last = Some(piece);
                }
                cx = grid.put_str_until(cx, row, text, style, arena, right);
            }
            if last.is_some() {
                row += 1;
                cx = x;
            }
            index += 1;
            skip = 0;
        }
        while row < bottom {
            grid.clear_span(row, x, width);
            row += 1;
        }
        #[cfg(test)]
        {
            self.visited = visited;
        }
    }

    /// The lines that left the ring, when the viewport has reached the top of
    /// it and the oldest line in the ring is the first row.
    fn notice(&self) -> Option<u64> {
        (self.dropped > 0 && self.top.line == self.dropped && self.top.row == 0)
            .then_some(self.dropped)
    }

    /// The width the rows of the stored lines are measured at.
    fn width(&self) -> u16 {
        self.area.width.max(1)
    }

    /// The rows one stored line takes at the area's width.
    fn rows_of(&self, index: u64) -> u32 {
        index
            .checked_sub(self.dropped)
            .and_then(|local| self.lines.get(local as usize))
            .map_or(1, |line| line.rows.max(1))
    }

    /// The bottom-most first row: the place that shows the last row of the last
    /// line at the last row of the area.
    fn max_top(&self) -> Mark {
        let mut left = u64::from(self.area.height.max(1));
        let mut index = self.len();
        while index > self.dropped {
            let line = index - 1;
            let rows = u64::from(self.rows_of(line));
            if rows >= left {
                return Mark {
                    line,
                    row: (rows - left) as u32,
                };
            }
            left -= rows;
            index = line;
        }
        Mark {
            line: self.dropped,
            row: 0,
        }
    }

    /// The runs of one line, by line number, with the text of each one; an
    /// unknown line has none.
    pub(crate) fn runs_of(&self, index: u64) -> impl Iterator<Item = (&str, u32)> + '_ {
        let line = index
            .checked_sub(self.dropped)
            .and_then(|local| self.lines.get(local as usize));
        let runs = line
            .and_then(|line| {
                self.runs
                    .get(line.start..line.start.saturating_add(line.len))
            })
            .unwrap_or_default();
        runs.iter().map(|run| (run.text(&self.text), run.style))
    }

    /// Lines the last draw visited, for the frame-cost test.
    #[cfg(test)]
    pub fn lines_visited(&self) -> usize {
        self.visited
    }

    /// The capacities of the text buffer, the run list and the line list, for
    /// the allocation guard.
    #[cfg(test)]
    pub fn capacity(&self) -> (usize, usize, usize) {
        (
            self.text.capacity(),
            self.runs.capacity(),
            self.lines.capacity(),
        )
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Transcript::new()
    }
}

/// Write `value` into `buf` as digits and return them.
fn digits_of(buf: &mut [u8; DIGITS], mut value: u64) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// The rows a line of these runs takes at `width`.
fn line_rows<'a>(runs: impl Iterator<Item = (&'a str, u32)>, width: u16) -> u32 {
    let mut rows = 1;
    for (_, _, row) in Pieces::new(runs, width) {
        rows = row + 1;
    }
    rows
}

/// The pieces one stored line breaks into at a width: each is a slice of one
/// run that a single row of the area holds, with the row it sits on.
///
/// A row breaks before a glyph that does not fit, so a wide glyph never splits,
/// and a line with no text is one empty piece, so it still takes a row.
struct Pieces<'a, I> {
    runs: I,
    text: &'a str,
    style: u32,
    offset: usize,
    width: usize,
    row: u32,
    used: usize,
    empty: bool,
}

impl<'a, I: Iterator<Item = (&'a str, u32)>> Pieces<'a, I> {
    fn new(runs: I, width: u16) -> Self {
        Pieces {
            runs,
            text: "",
            style: 0,
            offset: 0,
            width: usize::from(width.max(1)),
            row: 0,
            used: 0,
            empty: true,
        }
    }
}

impl<'a, I: Iterator<Item = (&'a str, u32)>> Iterator for Pieces<'a, I> {
    type Item = (&'a str, u32, u32);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.offset >= self.text.len() {
                match self.runs.next() {
                    Some((text, style)) => {
                        self.text = text;
                        self.style = style;
                        self.offset = 0;
                        if text.is_empty() {
                            continue;
                        }
                    }
                    None => {
                        return self.empty.then(|| {
                            self.empty = false;
                            ("", self.style, self.row)
                        });
                    }
                }
            }
            let rest = &self.text[self.offset..];
            let first = cell_width(rest.chars().next()?);
            if self.used > 0 && self.used + first > self.width {
                self.row += 1;
                self.used = 0;
            }
            let mut cells = self.used;
            let mut end = rest.len();
            for (at, c) in rest.char_indices() {
                let width = cell_width(c);
                if at > 0 && cells + width > self.width {
                    end = at;
                    break;
                }
                cells += width;
            }
            self.offset += end;
            self.used = cells;
            self.empty = false;
            return Some((&rest[..end], self.style, self.row));
        }
    }
}

/// The cells a char takes in a row, measured the way the grid writes it: a tab
/// is one space and the other control characters take none.
fn cell_width(c: char) -> usize {
    if is_control(c) {
        usize::from(c == '\t')
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}
