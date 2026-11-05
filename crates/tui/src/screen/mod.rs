//! Cell grid, double buffer, diff, inline output and full-screen output.

mod cell;
mod grid;
mod terminal;
mod transcript;

pub use cell::{Cell, Color, GraphemeArena, Style, StyleTable};
pub use grid::Grid;
pub use terminal::Terminal;
pub use transcript::{TRANSCRIPT_RING_LINES, Transcript};

use std::any::Any;
use std::ops::Range;
use std::sync::Arc;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use grid::is_control;

/// DEC 2026 synchronized output, start.
pub const SYNC_START: &[u8] = b"\x1b[?2026h";
/// DEC 2026 synchronized output, end.
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// The bytes of a frame that draws nothing: the sync block of an empty diff.
pub const SYNC_PAIR: usize = SYNC_START.len() + SYNC_END.len();

const RESET: &[u8] = b"\x1b[0m";
/// DECTCEM: hide the cursor for the frame. The UI draws its own edit point as
/// the terminal's cursor, and no reversed cell stands in for it.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
/// DECTCEM: show the cursor again, at the end of the frame that placed it.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const ERASE_BELOW: &[u8] = b"\x1b[0J";
const ERASE_ALL: &[u8] = b"\x1b[2J";
/// DEC 1049: enter the alternate screen buffer, saving the cursor and the
/// normal screen's contents.
const ALT_SCREEN_ON: &[u8] = b"\x1b[?1049h";
/// DEC 1049: leave the alternate screen buffer and restore both.
const ALT_SCREEN_OFF: &[u8] = b"\x1b[?1049l";
/// The characters of the session id the exit line names, as the Context panel
/// shows them.
const SHORT_SESSION_ID: usize = 8;

/// Where the terminal cursor sits when a frame ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cursor {
    /// The screen does not place the cursor: a diff writes nothing about it and
    /// the terminal keeps whatever it shows.
    #[default]
    Unmanaged,
    /// Hidden: the frame draws no edit point, as while a card waits for an
    /// answer.
    Hidden,
    /// Hidden while the frame draws, then put back at this grid column and row.
    At(u16, u16),
}

/// What a committed block draws from again when its fold state changes: the
/// item the live block drew from, held once. A re-render borrows it, so it
/// copies neither a message nor a tool's diff.
pub(crate) type Source = Arc<dyn Any + Send + Sync>;

/// One block of a commit: the number the fold state knows it by, the lines of
/// the batch it takes, the fold state they are drawn with, and the source a
/// later key draws it from again.
#[derive(Clone)]
pub(crate) struct CommitBlock {
    /// The block's number, from the fold state.
    pub fold: u64,
    /// The lines of the batch the block takes.
    pub lines: Range<usize>,
    /// Whether the block's lines are drawn open.
    pub open: bool,
    /// What the block's lines are drawn from.
    pub source: Source,
}

impl std::fmt::Debug for CommitBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitBlock")
            .field("fold", &self.fold)
            .field("lines", &self.lines)
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

/// One run of a committed line: text in one interned style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run<'a> {
    /// The text of the run.
    pub text: &'a str,
    /// The style id, from [`Screen::intern`].
    pub style: u32,
}

impl<'a> Run<'a> {
    /// A run of `text` in the default style.
    pub fn plain(text: &'a str) -> Self {
        Run { text, style: 0 }
    }
}

/// How many graphemes the arena keeps before it is rebuilt from the two grids.
pub const GRAPHEME_ARENA_CAP: usize = 4096;

/// Front and back grids that swap, a reused output path inside DEC 2026, and
/// the live area of inline mode or the transcript ring of full screen.
#[derive(Debug)]
pub struct Screen {
    front: Grid,
    back: Grid,
    styles: StyleTable,
    graphemes: GraphemeArena,
    commits: Vec<u8>,
    origin: u16,
    /// The first row the transcript continues at: the row after its last
    /// committed line. It equals `origin` unless the live area gave rows back
    /// below the screen's top, which leaves room the next lines print into.
    tail: u16,
    columns: u16,
    term_rows: u16,
    live: u16,
    inline: bool,
    /// Full screen: the alternate screen buffer and a transcript ring.
    full: bool,
    /// Full screen holds a frame that is due: a committed line or a moved
    /// viewport the next diff has to draw.
    pending: bool,
    /// The alternate screen is on and `close` has to leave it.
    entered: bool,
    /// The rendered lines of full screen.
    transcript: Transcript,
    /// The id of the session this run shows, which the exit line names short.
    /// Empty until the app sets it.
    session: String,
    full_redraw: bool,
    pen: Option<Style>,
    /// Where the next frame ends with the terminal cursor.
    cursor: Cursor,
    /// Where the frame the terminal shows left it.
    drawn_cursor: Cursor,
    /// Rows the last diff compared, for the frame-cost test.
    #[cfg(test)]
    visited_rows: u16,
}

impl Screen {
    /// A full-screen buffer for a terminal of `columns` by `rows`.
    pub fn new(columns: u16, rows: u16) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        Screen {
            front: Grid::new(columns, rows),
            back: Grid::new(columns, rows),
            styles: StyleTable::new(),
            graphemes: GraphemeArena::new(),
            commits: Vec::new(),
            origin: 0,
            tail: 0,
            columns,
            term_rows: rows,
            live: rows,
            inline: false,
            full: false,
            pending: false,
            entered: false,
            transcript: Transcript::new(),
            session: String::new(),
            full_redraw: true,
            pen: None,
            cursor: Cursor::Unmanaged,
            drawn_cursor: Cursor::Unmanaged,
            #[cfg(test)]
            visited_rows: 0,
        }
    }

    /// A full screen: the whole terminal in the alternate screen buffer, and
    /// the committed lines in a ring the app scrolls itself.
    ///
    /// [`Screen::enter`] writes the bytes that switch to the alternate screen
    /// and [`Screen::close`] leaves it on every exit path, closing the run with
    /// one line that names the session. The transcript starts over the whole
    /// terminal; the app narrows it with [`Screen::set_transcript_area`] when
    /// it draws a sidebar.
    pub fn full(columns: u16, rows: u16) -> Self {
        let mut screen = Screen::new(columns, rows);
        screen.full = true;
        screen.transcript.set_dim_style(screen.styles.intern(Style {
            dim: true,
            ..Style::default()
        }));
        screen
            .transcript
            .set_area(0, 0, screen.columns, screen.live);
        screen
    }

    /// A screen whose grid is the bottom `live` rows of a `rows` tall terminal.
    pub fn inline(columns: u16, rows: u16, live: u16) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let live = live.clamp(1, rows);
        Screen {
            front: Grid::new(columns, live),
            back: Grid::new(columns, live),
            styles: StyleTable::new(),
            graphemes: GraphemeArena::new(),
            commits: Vec::new(),
            origin: rows - live,
            tail: rows - live,
            columns,
            term_rows: rows,
            live,
            inline: true,
            full: false,
            pending: false,
            entered: false,
            transcript: Transcript::new(),
            session: String::new(),
            full_redraw: true,
            pen: None,
            cursor: Cursor::Unmanaged,
            drawn_cursor: Cursor::Unmanaged,
            #[cfg(test)]
            visited_rows: 0,
        }
    }

    /// Whether this screen draws full screen.
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// Take the id of the session this run shows, which the exit line names
    /// short.
    pub fn set_session(&mut self, id: &str) {
        if self.session == id {
            return;
        }
        self.session.clear();
        self.session.push_str(id);
    }

    /// The transcript of full screen.
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The transcript of full screen, to scroll it.
    pub fn transcript_mut(&mut self) -> &mut Transcript {
        self.pending = true;
        &mut self.transcript
    }

    /// Draw the transcript into the rows of its area: the rows the viewport
    /// shows come from the ring, and the rows below them are blanked.
    pub fn draw_transcript(&mut self) {
        self.transcript.draw(&mut self.back, &mut self.graphemes);
    }

    /// Set the rows of the grid the transcript draws into.
    pub fn set_transcript_area(&mut self, x: u16, y: u16, width: u16, height: u16) {
        self.transcript.set_area(x, y, width, height);
        self.pending = true;
    }

    /// Columns.
    pub fn columns(&self) -> u16 {
        self.columns
    }

    /// Grid rows (the live area in inline mode).
    pub fn rows(&self) -> u16 {
        self.live
    }

    /// Terminal rows.
    pub fn term_rows(&self) -> u16 {
        self.term_rows
    }

    /// The back grid, the frame being drawn.
    pub fn back(&self) -> &Grid {
        &self.back
    }

    /// The style id of the default style.
    pub fn default_style(&self) -> u32 {
        0
    }

    /// Intern a style once and reuse its id.
    pub fn intern(&mut self, style: Style) -> u32 {
        self.styles.intern(style)
    }

    /// Draw every row again on the next diff, as Ctrl+L asks.
    pub fn redraw(&mut self) {
        self.full_redraw = true;
        self.back.mark_all_dirty();
    }

    /// Blank the back grid and mark every row dirty. The full repaint.
    pub fn clear(&mut self) {
        self.back.clear();
    }

    /// Blank one row of the back grid and mark it for the diff.
    pub fn clear_row(&mut self, y: u16) {
        self.back.clear_row(y);
    }

    /// Start the back grid as a copy of the front so only drawn cells differ.
    pub fn sync(&mut self) {
        self.back.copy_from(&self.front);
    }

    /// Write a string at `(x, y)` and return the column after it.
    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: u32) -> u16 {
        self.back.put_str(x, y, text, style, &mut self.graphemes)
    }

    /// Write a string at `(x, y)` and stop at column `limit`, so a line the app
    /// narrowed to the main area does not write into the sidebar's columns.
    pub fn put_str_until(&mut self, x: u16, y: u16, text: &str, style: u32, limit: u16) -> u16 {
        self.back
            .put_str_until(x, y, text, style, &mut self.graphemes, limit)
    }

    /// Write one cell at `(x, y)`.
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        self.back.set(x, y, cell);
    }

    /// End the frame with the terminal cursor at grid column `x` and row `y`:
    /// hidden while the frame draws, then moved there outside the cells, so no
    /// cell is reversed to stand in for it.
    ///
    /// The position is clamped to the grid: a cursor outside it would land on a
    /// row of the terminal that is not the frame's.
    pub fn set_cursor(&mut self, x: u16, y: u16) {
        let x = x.min(self.columns.saturating_sub(1));
        let y = y.min(self.back.height().saturating_sub(1));
        self.cursor = Cursor::At(x, y);
    }

    /// End the frame with no cursor, as while a card waits for an answer: it is
    /// hidden and stays hidden until a frame places it again.
    pub fn clear_cursor(&mut self) {
        self.cursor = Cursor::Hidden;
    }

    /// The style behind an interned id, for a frame test.
    #[cfg(test)]
    pub fn style_of(&self, id: u32) -> Style {
        self.styles.get(id)
    }

    /// The text of one row of the last drawn frame, trailing spaces trimmed.
    pub fn row_text(&self, y: u16) -> String {
        let mut text = String::new();
        if y >= self.front.height() {
            return text;
        }
        let width = self.front.width() as usize;
        let row = y as usize * width;
        for x in 0..width {
            let cell = self.front.cells()[row + x];
            if cell.is_continuation() {
                continue;
            }
            if cell.is_grapheme() {
                text.push_str(self.graphemes.get(cell.grapheme_index()));
            } else if let Some(c) = cell.char() {
                text.push(c);
            }
        }
        text.truncate(text.trim_end_matches(' ').len());
        text
    }

    /// Print finished lines once above the live area and force a full redraw of
    /// the live area on the next frame.
    ///
    /// `runs` holds the runs of every line, in order, and `lines` says which
    /// range of `runs` each line is. A run keeps its style, so a committed
    /// reply keeps its markdown styles, and every run goes through the same
    /// filter a grid row uses: a tab is one space and no other control
    /// character reaches the terminal, so one line stays one row.
    ///
    /// A batch of lines goes right under the transcript while the rows between
    /// its tail and the live area hold it; past that it is printed at the live
    /// area's first row and scrolled up into them, so the transcript keeps its
    /// order whatever the area holds.
    ///
    /// In full screen the lines go into the transcript ring instead, and the
    /// next frame draws the rows the viewport shows: follow mode keeps the
    /// viewport at the bottom, and a viewport the user scrolled up stays put.
    pub fn commit(&mut self, runs: &[Run<'_>], lines: &[Range<usize>]) {
        self.commit_blocks(runs, lines, &[]);
    }

    /// Commit the lines of a batch, and remember the blocks of it that a fold
    /// key can draw again.
    ///
    /// `blocks` names the blocks of the batch that still have a source: their
    /// number, the lines of the batch each takes and the fold state they were
    /// drawn with. Inline mode prints the batch once and keeps none of it, as
    /// the scrollback cannot change; full screen stores each block beside its
    /// lines.
    pub(crate) fn commit_blocks(
        &mut self,
        runs: &[Run<'_>],
        lines: &[Range<usize>],
        blocks: &[CommitBlock],
    ) {
        if lines.is_empty() {
            return;
        }
        if self.full {
            let mut blocks = blocks.iter().peekable();
            for (index, line) in lines.iter().enumerate() {
                let at = self.transcript.len();
                self.transcript.push(&runs[line.clone()]);
                if blocks
                    .peek()
                    .is_some_and(|block| block.lines.start == index)
                    && let Some(block) = blocks.next()
                {
                    let count = block.lines.len();
                    self.transcript.add_block(
                        block.fold,
                        at,
                        count,
                        block.open,
                        block.source.clone(),
                    );
                }
            }
            self.pending = true;
            return;
        }
        self.commits.extend_from_slice(RESET);
        self.pen = Some(Style::default());
        let columns = usize::from(self.columns);
        let chunk = self.live.max(1) as usize;
        for batch in lines.chunks(chunk) {
            let count = batch.len() as u16;
            let scroll = count > self.origin.saturating_sub(self.tail);
            let row = if scroll {
                self.tail = self.origin;
                self.origin
            } else {
                let row = self.tail;
                self.tail += count;
                row
            };
            write_cup(&mut self.commits, row, 0);
            self.commits.extend_from_slice(ERASE_BELOW);
            for (i, line) in batch.iter().enumerate() {
                if i > 0 {
                    self.commits.extend_from_slice(b"\r\n");
                }
                let mut used = 0;
                for run in &runs[line.clone()] {
                    let style = self.styles.get(run.style);
                    if self.pen != Some(style) {
                        let pen = self.pen.unwrap_or_default();
                        write_sgr(&mut self.commits, pen, style);
                        self.pen = Some(style);
                    }
                    write_text(&mut self.commits, run.text, &mut used, columns);
                }
            }
            if scroll {
                write_cup(&mut self.commits, self.term_rows.saturating_sub(1), 0);
                for _ in batch {
                    self.commits.push(b'\n');
                }
            }
        }
        self.full_redraw = true;
        self.back.mark_all_dirty();
    }

    /// Draw the committed block `fold` again with the fold state `open`, in the
    /// room of the lines it drew before. Answers whether the ring still held
    /// it: a block past the ring's cap cannot be drawn again.
    pub(crate) fn replace_block(
        &mut self,
        fold: u64,
        open: bool,
        runs: &[Run<'_>],
        lines: &[Range<usize>],
    ) -> bool {
        if !self.full || !self.transcript.replace(fold, open, runs, lines) {
            return false;
        }
        self.pending = true;
        true
    }

    /// Take the source of the committed block `fold` out of the ring, so the app
    /// can grow the block it drew and draw it again.
    pub(crate) fn take_source(&mut self, fold: u64) -> Option<Source> {
        self.transcript.take_source(fold)
    }

    /// Give the committed block `fold` back the source it draws from again.
    pub(crate) fn set_source(&mut self, fold: u64, source: Source) {
        self.transcript.set_source(fold, source);
    }

    /// Fit the live area to `rows` rows of content.
    ///
    /// The area starts at the row after the transcript and holds its content at
    /// the top, so a smaller area leaves the editor right under the last
    /// committed line instead of a blank band. Growing into the room below costs
    /// no scroll; growing past the bottom takes rows from the transcript, which
    /// moves up into the scrollback.
    ///
    /// An area that took the whole screen has no transcript row above it to give
    /// rows back to, and the lines it prints next would land above the screen
    /// and never be seen: shrinking that one puts it at the bottom, and the row
    /// the transcript continues at comes back to the top of the screen.
    pub fn fit(&mut self, rows: u16) {
        if !self.inline {
            return;
        }
        let rows = rows.clamp(1, self.term_rows);
        if rows > self.live {
            let room = self.term_rows - self.origin;
            if rows > room {
                // The rows the area needs are printed lines: clear the area,
                // scroll that many rows into the scrollback and take them.
                let grow = rows - room;
                write_cup(&mut self.commits, self.origin, 0);
                self.commits.extend_from_slice(ERASE_BELOW);
                write_cup(&mut self.commits, self.term_rows - 1, 0);
                for _ in 0..grow {
                    self.commits.push(b'\n');
                }
                self.origin -= grow;
                self.tail = self.tail.saturating_sub(grow);
            }
            self.live = rows;
        } else if rows < self.live {
            // Give the rows back: the content stays at the top of the area and
            // nothing is drawn where it was, so the editor follows the
            // transcript. An area at the top of the screen has no transcript row
            // above it to give them back to, so it goes to the bottom instead and
            // the lines it commits next start under the top of the screen.
            self.live = rows;
            let from = self.origin;
            if from == 0 {
                self.origin = self.term_rows - rows;
            }
            write_cup(&mut self.commits, from, 0);
            self.commits.extend_from_slice(ERASE_BELOW);
        } else {
            return;
        }
        self.front.resize(self.columns, self.live);
        self.back.resize(self.columns, self.live);
        self.full_redraw = true;
    }

    /// Erase the live area and leave the cursor at its first row and column, so
    /// the shell prompt starts on a clean line right under the transcript.
    ///
    /// The pending commits go out first: they are the last lines the transcript
    /// took.
    ///
    /// In full screen this leaves the alternate screen, so the run ends with
    /// one line that names the session in the shell's scrollback instead of a
    /// transcript that vanishes with it. Entering again after a close is the
    /// same as the first time.
    pub fn close(&mut self, out: &mut Vec<u8>) {
        if self.full {
            self.close_full(out);
            return;
        }
        out.clear();
        out.extend_from_slice(SYNC_START);
        out.extend_from_slice(RESET);
        self.pen = Some(Style::default());
        out.extend_from_slice(&self.commits);
        self.commits.clear();
        write_cup(out, self.origin, 0);
        out.extend_from_slice(ERASE_BELOW);
        out.extend_from_slice(SYNC_END);
        // The frame drew with the cursor hidden; the shell's prompt wants it.
        if self.drawn_cursor != Cursor::Unmanaged {
            out.extend_from_slice(SHOW_CURSOR);
            self.drawn_cursor = Cursor::Unmanaged;
        }
        self.cursor = Cursor::Unmanaged;
        self.front.clear();
        self.back.clear();
        self.back.clear_dirty();
        self.full_redraw = false;
    }

    /// Switch the terminal to the alternate screen and blank the grids, so the
    /// next diff draws the whole screen.
    ///
    /// The normal screen and its scrollback are saved by the terminal and come
    /// back on [`Screen::close`], whatever the run did in between. Entering
    /// twice in a row writes nothing, so an exit path that closes a screen it
    /// never entered leaves the terminal alone.
    pub fn enter(&mut self, out: &mut Vec<u8>) {
        out.clear();
        if !self.full || self.entered {
            return;
        }
        self.entered = true;
        out.extend_from_slice(ALT_SCREEN_ON);
        out.extend_from_slice(RESET);
        out.extend_from_slice(ERASE_ALL);
        write_cup(out, 0, 0);
        // The alternate screen is blank, so nothing of the last frame is on it.
        self.front.clear();
        self.back.clear();
        self.back.mark_all_dirty();
        self.full_redraw = true;
        self.pending = true;
    }

    /// Leave the alternate screen in one write, close the run with the line
    /// that names its session, and leave the cursor on a clean line under it.
    fn close_full(&mut self, out: &mut Vec<u8>) {
        out.clear();
        if self.entered {
            out.extend_from_slice(RESET);
            out.extend_from_slice(ALT_SCREEN_OFF);
        }
        // The frame drew with the cursor hidden; the shell's prompt wants it.
        if self.drawn_cursor != Cursor::Unmanaged {
            out.extend_from_slice(SHOW_CURSOR);
        }
        self.write_exit_line(out);
        self.drawn_cursor = Cursor::Unmanaged;
        self.cursor = Cursor::Unmanaged;
        self.transcript.clear();
        self.pen = Some(Style::default());
        self.commits.clear();
        self.front.clear();
        self.back.clear();
        self.back.clear_dirty();
        self.full_redraw = false;
        self.pending = false;
        self.entered = false;
    }

    /// Write the one line full screen leaves in the scrollback: the name
    /// `reagent:` dim, then the session it showed short, in capitals, and the
    /// flag that continues it.
    fn write_exit_line(&mut self, out: &mut Vec<u8>) {
        let dim = self.styles.get(self.transcript.dim_style());
        out.push(b'\r');
        write_sgr(out, Style::default(), dim);
        out.extend_from_slice(b"reagent:");
        write_sgr(out, dim, Style::default());
        out.extend_from_slice(b" session ");
        let mut buf = [0u8; 4];
        for c in self.session.chars().take(SHORT_SESSION_ID) {
            for up in c.to_uppercase() {
                out.extend_from_slice(up.encode_utf8(&mut buf).as_bytes());
            }
        }
        out.extend_from_slice(b" \xc2\xb7 reagent -c to continue\r\n");
    }

    /// Resize the terminal; the live area is redrawn in full.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        if columns == self.columns && rows == self.term_rows {
            return;
        }
        self.columns = columns;
        self.term_rows = rows;
        if self.inline {
            self.live = self.live.clamp(1, rows);
            self.origin = rows - self.live;
        } else {
            self.live = rows;
            self.origin = 0;
        }
        self.tail = self.origin;
        self.front.resize(columns, self.live);
        self.back.resize(columns, self.live);
        if self.full {
            // A resize lays the screen out again: the transcript starts over
            // the whole terminal until the app sets its area again.
            self.transcript.set_area(0, 0, columns, self.live);
            self.pending = true;
        }
        self.full_redraw = true;
    }

    /// Whether the back grid has anything to draw.
    pub fn is_dirty(&self) -> bool {
        self.full_redraw || self.pending || !self.commits.is_empty() || self.back.any_dirty()
    }

    /// Capacity of the pending-commit buffer, for the allocation guard.
    pub fn commit_capacity(&self) -> usize {
        self.commits.capacity()
    }

    /// Rows the last diff compared, for the frame-cost test.
    #[cfg(test)]
    pub fn diff_rows_visited(&self) -> u16 {
        self.visited_rows
    }

    /// Number of interned styles, for the allocation guard.
    pub fn style_count(&self) -> usize {
        self.styles.len()
    }

    /// Number of interned graphemes, for the allocation guard.
    pub fn grapheme_count(&self) -> usize {
        self.graphemes.len()
    }

    /// Draw the frame into the caller's reused buffer, all inside DEC 2026, and
    /// leave the drawn frame in both grids. Emits only the changed cells, the
    /// fewest cursor moves and the SGR changes between runs.
    ///
    /// The back grid equals the front, the frame the terminal shows, after this
    /// call, so the next frame starts from it: a frame can overwrite only the
    /// rows it changes (see [`Screen::clear_row`]) and cost one row instead of
    /// one screen; [`Screen::clear`] is the full repaint.
    ///
    /// A screen the app placed a cursor on (see [`Screen::set_cursor`]) writes
    /// the frame with that cursor hidden, then moves it to the edit point and
    /// shows it, all inside the same DEC 2026 block: the terminal paints the
    /// cells and the cursor in one step, so the cursor never trails the text it
    /// belongs to. A frame that draws no cell and leaves the cursor where it
    /// already is writes nothing but the block, so a steady frame still costs
    /// [`SYNC_PAIR`] bytes.
    pub fn diff(&mut self, out: &mut Vec<u8>) {
        out.clear();
        #[cfg(test)]
        {
            self.visited_rows = 0;
        }
        out.extend_from_slice(SYNC_START);
        let mark = out.len();
        if self.cursor != Cursor::Unmanaged {
            out.extend_from_slice(HIDE_CURSOR);
        }
        let body = out.len();
        let pen = self.pen;
        if !self.commits.is_empty() {
            out.extend_from_slice(&self.commits);
            self.commits.clear();
        }
        self.write_diff(out);
        // The frame touched the terminal unless it wrote no cell and left the
        // cursor where the terminal already shows it.
        if out.len() > body || self.cursor != self.drawn_cursor {
            if let Cursor::At(x, y) = self.cursor {
                write_cup(out, self.origin + y, x);
                out.extend_from_slice(SHOW_CURSOR);
            }
        } else {
            out.truncate(mark);
            self.pen = pen;
        }
        out.extend_from_slice(SYNC_END);
        self.drawn_cursor = self.cursor;
        // Both grids end up holding the frame the terminal now shows: the next
        // frame draws over it, and a row it clears falls back to the front text.
        // One memcpy of the cells, and the back grid's rows go clean.
        self.front.copy_from(&self.back);
        self.back.clear_dirty();
        self.full_redraw = false;
        self.pending = false;
        if self.graphemes.len() > GRAPHEME_ARENA_CAP {
            self.compact_graphemes();
        }
    }

    /// Rebuild the arena from the graphemes the two grids still use, renumbering
    /// their cells together so equal graphemes stay equal.
    fn compact_graphemes(&mut self) {
        let mut arena = GraphemeArena::new();
        let graphemes = &self.graphemes;
        self.front.remap_graphemes(graphemes, &mut arena);
        self.back.remap_graphemes(graphemes, &mut arena);
        self.graphemes = arena;
    }

    fn write_diff(&mut self, out: &mut Vec<u8>) {
        let width = self.back.width() as usize;
        let height = self.back.height() as usize;
        let mut cx: i32 = -1;
        let mut cy: i32 = -1;

        for y in 0..height {
            if !self.full_redraw && !self.back.dirty(y as u16) {
                continue;
            }
            #[cfg(test)]
            {
                self.visited_rows += 1;
            }
            let row = y * width;
            let mut x = 0;
            while x < width {
                if !self.cell_differs(row + x) {
                    x += 1;
                    continue;
                }
                if cx != x as i32 || cy != y as i32 {
                    write_cup(out, self.origin + y as u16, x as u16);
                    cx = x as i32;
                    cy = y as i32;
                }
                while x < width && self.cell_differs(row + x) {
                    let cell = self.back.cells()[row + x];
                    if cell.is_continuation() {
                        x += 1;
                        continue;
                    }
                    let style = self.styles.get(cell.style());
                    if self.pen.is_none() {
                        out.extend_from_slice(RESET);
                        self.pen = Some(Style::default());
                    }
                    let pen = self.pen.unwrap_or_default();
                    if pen != style {
                        write_sgr(out, pen, style);
                        self.pen = Some(style);
                    }
                    if cell.is_grapheme() {
                        let text = self.graphemes.get(cell.grapheme_index());
                        out.extend_from_slice(text.as_bytes());
                        cx += UnicodeWidthStr::width(text) as i32;
                    } else {
                        let c = cell.char().unwrap_or(' ');
                        let mut buf = [0u8; 4];
                        let text = c.encode_utf8(&mut buf);
                        out.extend_from_slice(text.as_bytes());
                        cx += UnicodeWidthChar::width(c).unwrap_or(0) as i32;
                    }
                    x += 1;
                }
            }
        }
    }

    fn cell_differs(&self, index: usize) -> bool {
        self.full_redraw || self.front.cells()[index].bits() != self.back.cells()[index].bits()
    }
}

/// Writes the text of one committed run the way a grid row shows it: a tab is
/// one space, C0, DEL and C1 are dropped, and the run stops at `columns` so the
/// terminal never wraps the line into two rows.
fn write_text(out: &mut Vec<u8>, text: &str, used: &mut usize, columns: usize) {
    for c in text.chars() {
        if *used >= columns {
            return;
        }
        if is_control(c) {
            if c == '\t' {
                out.push(b' ');
                *used += 1;
            }
            continue;
        }
        let width = UnicodeWidthChar::width(c).unwrap_or(0);
        if width == 0 {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        if *used + width > columns {
            return;
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        *used += width;
    }
}

fn write_cup(out: &mut Vec<u8>, row: u16, column: u16) {
    out.extend_from_slice(b"\x1b[");
    write_u16(out, row.saturating_add(1));
    out.push(b';');
    write_u16(out, column.saturating_add(1));
    out.push(b'H');
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    write_u64(out, u64::from(value));
}

fn write_u64(out: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
}

fn needs_reset(prev: Style, next: Style) -> bool {
    (prev.bold && !next.bold)
        || (prev.dim && !next.dim)
        || (prev.italic && !next.italic)
        || (prev.underline && !next.underline)
        || (prev.strikethrough && !next.strikethrough)
        || (prev.reverse && !next.reverse)
        || (prev.fg != next.fg && next.fg == Color::Default)
        || (prev.bg != next.bg && next.bg == Color::Default)
        || prev.link != next.link
}

fn write_sgr(out: &mut Vec<u8>, prev: Style, next: Style) {
    if prev == next {
        return;
    }
    let mut params = Params::new();
    if needs_reset(prev, next) {
        params.push(0);
        push_style(&mut params, next, Style::default(), true);
    } else {
        push_style(&mut params, next, prev, false);
    }
    out.extend_from_slice(b"\x1b[");
    for (i, param) in params.buf[..params.len].iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        write_u16(out, *param);
    }
    out.push(b'm');
}

fn push_style(params: &mut Params, next: Style, base: Style, full: bool) {
    if next.bold && (full || !base.bold) {
        params.push(1);
    }
    if next.dim && (full || !base.dim) {
        params.push(2);
    }
    if next.italic && (full || !base.italic) {
        params.push(3);
    }
    if next.underline && (full || !base.underline) {
        params.push(4);
    }
    if next.reverse && (full || !base.reverse) {
        params.push(7);
    }
    if next.strikethrough && (full || !base.strikethrough) {
        params.push(9);
    }
    if next.fg != Color::Default && (full || next.fg != base.fg) {
        push_color(params, 38, next.fg);
    }
    if next.bg != Color::Default && (full || next.bg != base.bg) {
        push_color(params, 48, next.bg);
    }
}

fn push_color(params: &mut Params, base: u16, color: Color) {
    match color {
        Color::Default => {}
        Color::Indexed(index) => {
            params.push(base);
            params.push(5);
            params.push(u16::from(index));
        }
        Color::Rgb(r, g, b) => {
            params.push(base);
            params.push(2);
            params.push(u16::from(r));
            params.push(u16::from(g));
            params.push(u16::from(b));
        }
    }
}

struct Params {
    buf: [u16; 24],
    len: usize,
}

impl Params {
    fn new() -> Self {
        Params {
            buf: [0; 24],
            len: 0,
        }
    }

    fn push(&mut self, value: u16) {
        if self.len < self.buf.len() {
            self.buf[self.len] = value;
            self.len += 1;
        }
    }
}

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
