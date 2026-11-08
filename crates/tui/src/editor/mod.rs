//! The message editor: a gap buffer of UTF-8 text, a cursor that sits on
//! grapheme boundaries, a visual-row layout for drawing and the command history.
//!
//! Graphemes are a base character plus the zero-width and variation characters
//! after it, measured whole, the same rule the cell grid draws by, so a row
//! wraps where the renderer breaks it and an editor column is a screen column.
//!
//! A typed character allocates nothing once the buffer has grown, and so does an
//! erase once the undo log is full, when the step it drops hands the new step its
//! buffer; a kill spends its ring entry. The row list, the draft and the gap are
//! reused. The rows are wrapped lazily: an edit keeps every row
//! above its own and drops the wrap from there on, and a layout builds only the
//! rows the window shows and the cursor needs, so a key in a large buffer wraps
//! the few rows around the cursor, not the whole text. The row count is exact
//! once the wrap reaches the end of the text and an estimate before.
//!
//! Reading the text costs the bytes read, not the bytes held: a character is
//! checked from at most four of them, and a row slice only checks the row. The
//! whole buffer is checked only by [`Editor::chunks`], which the app calls once
//! a frame, and by [`Editor::take`] and the history.

use std::mem;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::slot::Component;

mod paste;
mod readline;

pub use paste::{CHIP, CHIP_MORE, IMAGE_LABEL, IMAGE_LABEL_END, MARK, MARK_BYTES, Paste};

use readline::Readline;

/// The case a word command sets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Case {
    /// Alt+U.
    Upper,
    /// Alt+L.
    Lower,
    /// Alt+C: the first character up and the rest down.
    Capitalize,
}

/// The room the gap grows by at least, in bytes.
const GAP: usize = 64;

/// The most bytes a grapheme the gap splits may have for its width to be
/// measured whole; a longer one falls back to the sum of its characters.
const GRAPHEME_BYTES: usize = 32;

/// A visual row: the byte range of the text that shows on it, without the
/// newline that ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    start: usize,
    end: usize,
}

impl Row {
    /// The first byte the row shows.
    pub fn start(&self) -> usize {
        self.start
    }

    /// One past the last byte the row shows.
    pub fn end(&self) -> usize {
        self.end
    }
}

/// The rows that show for a width and a height, and where the cursor is.
#[derive(Debug)]
pub struct Layout<'a> {
    rows: &'a [Row],
    first: usize,
    total: usize,
    cursor_row: u16,
    cursor_col: u16,
}

impl Layout<'_> {
    /// The visible rows, top to bottom.
    pub fn rows(&self) -> &[Row] {
        self.rows
    }

    /// The index of the first visible row in the whole text.
    pub fn first(&self) -> usize {
        self.first
    }

    /// How many rows the whole text has at this width: exact once the wrap
    /// reached the end of the text, an estimate of the rows still to wrap
    /// before.
    pub fn total(&self) -> usize {
        self.total
    }

    /// The cursor's row, counted from the first visible row.
    pub fn cursor_row(&self) -> u16 {
        self.cursor_row
    }

    /// The cursor's column, in cells.
    pub fn cursor_col(&self) -> u16 {
        self.cursor_col
    }
}

/// A folded paste chip as the layout measures it: the mark's byte position and
/// the columns the chip draws in its place.
///
/// One cell of text stands for a whole chip, so the wrap, the drawn row and the
/// cursor need the drawn width to agree; the mark keeps its own byte length.
#[derive(Debug, Clone, Copy)]
struct Mark {
    /// The first byte of the mark in the editor text.
    at: usize,
    /// The columns the chip's label takes there, clamped to the wrap width.
    width: u16,
}

/// The message editor.
#[derive(Debug)]
pub struct Editor {
    gap: Gap,
    /// The rows wrapped so far: every row from the start of the text up to
    /// [`Editor::wrapped_to`], which is the text length once all are built.
    rows: Vec<Row>,
    /// The width the rows are wrapped at; `None` when none is.
    rows_width: Option<u16>,
    /// The text position that starts the row after the built ones.
    wrapped_to: usize,
    scroll: usize,
    history: usize,
    draft: String,
    /// The lines of the Context panel, which the text box's footer reads the
    /// model and its settings from. A panel hands its text over as a shared
    /// `Arc`, so the buffer is kept and reused: reading the footer costs no
    /// copy and allocates nothing after the first frame.
    pub(crate) panel_lines: Vec<Component>,
    /// The paste chips, in text order: a long paste folded to a mark in the
    /// text, or opened to the editor's own text.
    pastes: Vec<Paste>,
    /// The folded chips as the layout measures them, in text order, rebuilt by
    /// [`Editor::refresh`]: the buffer keeps its room, so a layout allocates
    /// nothing however many chips the text holds.
    marks: Vec<Mark>,
    /// The readline state: the kill ring, the undo log, the argument, the mark
    /// and the key a quoted insert waits for.
    readline: Readline,
    /// The rows the last wrap built, for the cost test.
    #[cfg(test)]
    wrapped: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Editor::new()
    }
}

impl Editor {
    /// An empty editor with the cursor at the start.
    pub fn new() -> Self {
        Editor {
            gap: Gap::new(),
            rows: Vec::new(),
            rows_width: None,
            wrapped_to: 0,
            scroll: 0,
            history: 0,
            draft: String::new(),
            panel_lines: Vec::new(),
            pastes: Vec::new(),
            marks: Vec::new(),
            readline: Readline::default(),
            #[cfg(test)]
            wrapped: 0,
        }
    }

    /// The text as two slices: before the cursor, then after it. They
    /// concatenate to the whole text.
    pub fn chunks(&self) -> (&str, &str) {
        self.gap.chunks()
    }

    /// The text in `start..end` as two slices that concatenate to it, with the
    /// cut at the cursor. Both bounds must be character boundaries.
    pub fn slice(&self, start: usize, end: usize) -> (&str, &str) {
        self.gap.slice(start, end)
    }

    /// The length of the text in bytes.
    pub fn len(&self) -> usize {
        self.gap.len()
    }

    /// True when the text is empty.
    pub fn is_empty(&self) -> bool {
        self.gap.len() == 0
    }

    /// The cursor's byte offset.
    pub fn cursor(&self) -> usize {
        self.gap.cursor()
    }

    /// Insert text at the cursor and leave the cursor after it. A pasted block
    /// is one insert.
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let at = self.gap.cursor();
        self.place(text);
        self.readline.inserted(at, text.len());
    }

    /// Insert a line break.
    pub fn new_line(&mut self) {
        self.insert("\n");
    }

    /// Put `text` at the cursor, with the chips it moves and the rows it
    /// drops, and no undo step of its own.
    fn place(&mut self, text: &str) {
        let at = self.gap.cursor();
        self.gap.insert(text);
        self.moved(at, 0, text.len());
        self.snap();
        self.touch(at);
    }

    /// Move one grapheme left.
    pub fn left(&mut self) {
        self.readline.moved();
        let at = {
            let scan = Scan::new(&self.gap);
            scan.prev_boundary(self.gap.cursor())
        };
        self.gap.move_to(at);
    }

    /// Move one grapheme right.
    pub fn right(&mut self) {
        self.readline.moved();
        let at = {
            let scan = Scan::new(&self.gap);
            scan.next_boundary(self.gap.cursor())
        };
        self.gap.move_to(at);
    }

    /// Move up one visual line at `width`, keeping the column; false on the
    /// first row, where the history takes the key.
    pub fn up(&mut self, width: u16) -> bool {
        self.readline.moved();
        self.refresh(width);
        self.wrap_through(self.gap.cursor());
        let (row, col) = self.row_col();
        if row == 0 {
            return false;
        }
        let at = {
            let scan = Scan::new(&self.gap);
            position_in_row(&scan, &self.rows, row - 1, u32::from(col), &self.marks)
        };
        self.gap.move_to(at);
        true
    }

    /// Move down one visual line at `width`, keeping the column; false on the
    /// last row, where the history takes the key.
    pub fn down(&mut self, width: u16) -> bool {
        self.readline.moved();
        self.refresh(width);
        self.wrap_through(self.gap.cursor());
        let (row, col) = self.row_col();
        // The row below the cursor is the one the wrap may not have built yet.
        self.wrap_through(self.rows[row].end.saturating_add(1));
        if row + 1 >= self.rows.len() {
            return false;
        }
        let at = {
            let scan = Scan::new(&self.gap);
            position_in_row(&scan, &self.rows, row + 1, u32::from(col), &self.marks)
        };
        self.gap.move_to(at);
        true
    }

    /// Move to the start of the visual row at `width`.
    pub fn home(&mut self, width: u16) {
        self.readline.moved();
        self.refresh(width);
        let cursor = self.gap.cursor();
        self.wrap_through(cursor.saturating_add(1));
        let row = row_of_display(&self.rows, cursor);
        let mut at = self.rows[row].start;
        if at == cursor {
            // A wrap point shows at the end of the row above; Home there goes
            // to the start of the row the text continues on.
            let row = row_of_start(&self.rows, at);
            at = self.rows[row].start;
        }
        self.gap.move_to(at);
    }

    /// Move to the end of the visual row at `width`.
    pub fn end(&mut self, width: u16) {
        self.readline.moved();
        self.refresh(width);
        let cursor = self.gap.cursor();
        self.wrap_through(cursor.saturating_add(1));
        let row = row_of_display(&self.rows, cursor);
        let mut at = self.rows[row].end;
        if at == cursor {
            let row = row_of_start(&self.rows, at);
            at = self.rows[row].end;
        }
        self.gap.move_to(at);
    }

    /// Ctrl+A: move to the start of the line.
    pub fn line_start(&mut self) {
        self.readline.moved();
        let at = self.line_start_of(self.gap.cursor());
        self.gap.move_to(at);
    }

    /// Ctrl+E: move to the end of the line.
    pub fn line_end(&mut self) {
        self.readline.moved();
        let at = self.line_end_of(self.gap.cursor());
        self.gap.move_to(at);
    }

    /// Remove the grapheme before the cursor.
    pub fn backspace(&mut self) {
        let cursor = self.gap.cursor();
        if cursor == 0 {
            return;
        }
        let at = {
            let scan = Scan::new(&self.gap);
            scan.prev_boundary(cursor)
        };
        self.splice(at, cursor, "");
    }

    /// Remove the grapheme after the cursor.
    pub fn delete(&mut self) {
        let cursor = self.gap.cursor();
        let at = {
            let scan = Scan::new(&self.gap);
            scan.next_boundary(cursor)
        };
        self.splice(cursor, at, "");
    }

    /// Ctrl+K: kill to the end of the line.
    pub fn kill_to_end(&mut self) {
        let cursor = self.gap.cursor();
        let at = self.line_end_of(cursor);
        self.kill_range(cursor, at, false, false, false);
    }

    /// Ctrl+U: kill to the start of the line.
    pub fn kill_to_start(&mut self) {
        let cursor = self.gap.cursor();
        let at = self.line_start_of(cursor);
        self.kill_range(at, cursor, true, false, false);
    }

    /// Ctrl+W: kill the whitespace and the word before the cursor.
    pub fn kill_space_word(&mut self) {
        let cursor = self.gap.cursor();
        let at = self.space_word_start(cursor);
        self.kill_range(at, cursor, true, false, false);
    }

    /// Take the text and empty the editor, keeping the buffers.
    ///
    /// The message holds every chip's pasted bytes in place of its mark, built
    /// once into one string: a chip that is open has its bytes in the text
    /// already, and a folded one is copied here, its only copy into the send.
    pub fn take(&mut self) -> String {
        let mut out = String::with_capacity(self.expanded_len());
        self.write_text(&mut out);
        self.clear();
        out
    }

    /// Empty the text and the history browsing.
    pub fn clear(&mut self) {
        self.gap.clear();
        self.rows.clear();
        self.rows_width = None;
        self.wrapped_to = 0;
        self.scroll = 0;
        self.history = 0;
        self.draft.clear();
        self.pastes.clear();
        self.readline.clear();
    }

    /// The previous message of the session, newest last; false when the oldest
    /// entry is already shown. What was being typed is kept for
    /// [`Editor::history_down`].
    pub fn history_up<S: AsRef<str>>(&mut self, history: &[S]) -> bool {
        if history.is_empty() {
            return false;
        }
        if self.history == 0 {
            // The history keeps text: a chip's bytes join it as text now, so no
            // mark is left in a draft that has no chip.
            self.expand_pastes();
            let (before, after) = self.gap.chunks();
            self.draft.clear();
            self.draft.push_str(before);
            self.draft.push_str(after);
            self.history = 1;
        } else if self.history < history.len() {
            self.history += 1;
        } else {
            return false;
        }
        let entry = history[history.len() - self.history].as_ref();
        self.replace(entry);
        true
    }

    /// The next message of the session; false when the draft is already shown.
    /// Down past the newest message returns what was being typed.
    pub fn history_down<S: AsRef<str>>(&mut self, history: &[S]) -> bool {
        if self.history == 0 || self.history > history.len() {
            self.history = 0;
            return false;
        }
        self.history -= 1;
        if self.history == 0 {
            let draft = mem::take(&mut self.draft);
            self.replace(&draft);
            self.draft = draft;
        } else {
            let entry = history[history.len() - self.history].as_ref();
            self.replace(entry);
        }
        true
    }

    /// Alt+<: the oldest message of the session, keeping what is being typed for
    /// [`Editor::history_newest`]; false when no message was sent.
    pub fn history_oldest<S: AsRef<str>>(&mut self, history: &[S]) -> bool {
        let Some(oldest) = history.first() else {
            return false;
        };
        if self.history == 0 {
            self.expand_pastes();
            let (before, after) = self.gap.chunks();
            self.draft.clear();
            self.draft.push_str(before);
            self.draft.push_str(after);
        }
        self.history = history.len();
        self.replace(oldest.as_ref());
        true
    }

    /// Alt+>: the draft again, whatever message the box shows; false when it
    /// already shows the draft.
    pub fn history_newest(&mut self) -> bool {
        if self.history == 0 {
            return false;
        }
        self.history = 0;
        let draft = mem::take(&mut self.draft);
        self.replace(&draft);
        self.draft = draft;
        true
    }

    /// The rows that show for a width and a height, scrolled so the cursor row
    /// is visible, and the cursor's row and column among them. Only the rows the
    /// window can show are wrapped.
    pub fn layout(&mut self, width: u16, height: u16) -> Layout<'_> {
        self.refresh(width);
        let height = usize::from(height.max(1));
        let cursor = self.gap.cursor();
        // One past the cursor: a wrap point starts the row the cursor shows in,
        // so the wrap must not stop there.
        self.wrap_through(cursor.saturating_add(1));
        let (row, col) = self.row_col();
        // A scroll that follows the cursor never shows a row more than `height`
        // rows below it, so the wrap stops there.
        self.wrap_rows_to(row.saturating_add(height));
        let built = self.rows.len();
        let total = self.total_rows();
        if row < self.scroll {
            self.scroll = row;
        }
        if row >= self.scroll + height {
            self.scroll = row + 1 - height;
        }
        self.scroll = self.scroll.min(total.saturating_sub(height));
        let first = self.scroll;
        let last = (first + height).min(built);
        Layout {
            rows: &self.rows[first..last],
            first,
            total,
            cursor_row: (row - first) as u16,
            cursor_col: col,
        }
    }

    /// How many rows the whole text has at the current width: exact once the
    /// wrap reached the end of the text, and otherwise the rows built plus an
    /// estimate of the rows the rest of the text needs at this width.
    fn total_rows(&self) -> usize {
        if self.fully_wrapped() {
            return self.rows.len();
        }
        let width = usize::from(self.rows_width.unwrap_or(1).max(1));
        let rest = self.gap.len().saturating_sub(self.wrapped_to);
        self.rows.len() + rest.div_ceil(width)
    }

    /// Put the text of a history entry in place with the cursor at its end.
    ///
    /// The chips go with the text they were cut out of: an entry is its own
    /// text, and the draft the history hands back is the expanded text of the
    /// one it replaced, so a chip of either would point into bytes that are no
    /// longer there.
    fn replace(&mut self, text: &str) {
        self.gap.clear();
        self.gap.insert(text);
        self.gap.move_to(self.gap.len());
        self.pastes.clear();
        self.readline.clear();
        self.drop_rows(0);
    }

    /// The text changed at `at`: the rows below are stale and reading history
    /// is over.
    fn touch(&mut self, at: usize) {
        self.drop_rows(at);
        self.history = 0;
    }

    /// Drop the rows an edit at `at` made stale and keep the ones above: the
    /// wrap resumes at the start of the dropped run.
    fn drop_rows(&mut self, at: usize) {
        if self.rows_width.is_none() {
            return;
        }
        let keep = self.rows.partition_point(|row| row.end < at);
        if keep == self.rows.len() {
            return;
        }
        self.rows.truncate(keep);
        self.wrapped_to = match keep.checked_sub(1) {
            Some(last) => self.next_start(last),
            None => 0,
        };
    }

    /// The text position that starts the row after row `row`.
    fn next_start(&self, row: usize) -> usize {
        let end = self.rows[row].end;
        match Scan::new(&self.gap).char_at(end) {
            Some('\n') => end + 1,
            _ => end,
        }
    }

    /// Leave a cursor that follows a combining mark on the next boundary.
    fn snap(&mut self) {
        let at = {
            let scan = Scan::new(&self.gap);
            let mut at = self.gap.cursor();
            while let Some(c) = scan.char_at(at) {
                if !is_extend(c) {
                    break;
                }
                at += c.len_utf8();
            }
            at
        };
        self.gap.move_to(at);
    }

    fn line_start_of(&self, pos: usize) -> usize {
        let scan = Scan::new(&self.gap);
        let mut at = pos;
        while let Some(prev) = scan.prev_char(at) {
            if scan.char_at(prev) == Some('\n') {
                break;
            }
            at = prev;
        }
        at
    }

    fn line_end_of(&self, pos: usize) -> usize {
        let scan = Scan::new(&self.gap);
        let mut at = pos;
        while let Some(c) = scan.char_at(at) {
            if c == '\n' {
                break;
            }
            at += c.len_utf8();
        }
        at
    }

    /// A new width drops the rows of the old one, and the marks are measured
    /// again at it: a chip's width is the drawn one, cut at the row.
    fn refresh(&mut self, width: u16) {
        let width = width.max(1);
        self.refresh_marks(width);
        if self.rows_width == Some(width) {
            return;
        }
        self.rows.clear();
        self.rows_width = Some(width);
        self.wrapped_to = 0;
    }

    /// Measure the folded chips for the layout: the columns each one draws in
    /// place of its mark, clamped to `width`, the narrowest row it can fill.
    ///
    /// A chip that opens or folds changes the drawn width of its mark, so this
    /// runs on every layout, not only when the width changes. It touches the
    /// chips, never the text, and the buffer keeps its room.
    fn refresh_marks(&mut self, width: u16) {
        self.marks.clear();
        for paste in &self.pastes {
            if paste.is_open() {
                continue;
            }
            let (at, _) = paste.range();
            let width = paste.width().min(width);
            self.marks.push(Mark { at, width });
        }
    }

    /// True once every row is built: the wrap reached the end of the text and
    /// the last row ends there. A resume point at the text length is not
    /// enough, since the empty row a trailing newline shows starts there.
    fn fully_wrapped(&self) -> bool {
        self.wrapped_to == self.gap.len()
            && self
                .rows
                .last()
                .is_some_and(|row| row.end == self.gap.len())
    }

    /// Wrap until at least `count` rows are built or the text ends. Each step
    /// builds at least one row, so the loop ends at the end of the text.
    fn wrap_rows_to(&mut self, count: usize) {
        while self.rows.len() < count && !self.fully_wrapped() {
            let at = self.rows.last().map_or(0, |row| row.end.saturating_add(1));
            self.wrap_through(at);
        }
    }

    /// Wrap the rows up to the one that holds text position `at`.
    fn wrap_through(&mut self, at: usize) {
        if self.fully_wrapped() {
            return;
        }
        if self.rows.last().is_some_and(|row| at <= row.end) {
            return;
        }
        let width = self.rows_width.unwrap_or(1);
        let mut rows = mem::take(&mut self.rows);
        #[cfg(test)]
        let before = rows.len();
        {
            let scan = Scan::new(&self.gap);
            self.wrapped_to = wrap_rows(&scan, width, &mut rows, self.wrapped_to, at, &self.marks);
        }
        self.rows = rows;
        #[cfg(test)]
        {
            self.wrapped += self.rows.len() - before;
        }
    }

    fn row_col(&self) -> (usize, u16) {
        let pos = self.gap.cursor();
        let row = row_of_display(&self.rows, pos);
        let col = column_of(&Scan::new(&self.gap), self.rows[row], pos, &self.marks);
        (row, col)
    }

    /// The rows the wrap built since [`Editor::reset_rows_wrapped`].
    #[cfg(test)]
    fn rows_wrapped(&self) -> usize {
        self.wrapped
    }

    /// Start counting the rows the wrap builds.
    #[cfg(test)]
    fn reset_rows_wrapped(&mut self) {
        self.wrapped = 0;
    }
}

/// A gap buffer: the text before the cursor, an empty gap, the text after it.
/// The cursor is always at the start of the gap.
#[derive(Debug, Default)]
struct Gap {
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl Gap {
    fn new() -> Self {
        Gap::default()
    }

    fn len(&self) -> usize {
        self.buf.len() - (self.end - self.start)
    }

    fn cursor(&self) -> usize {
        self.start
    }

    /// Empty the text and keep the room it grew to.
    fn clear(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.end = 0;
    }

    /// The whole text as two slices, with the cut at the cursor. This is the
    /// one read that checks every byte; [`Gap::slice`] checks only its own.
    fn chunks(&self) -> (&str, &str) {
        let before =
            std::str::from_utf8(&self.buf[..self.start]).expect("the gap starts on a boundary");
        let after = std::str::from_utf8(&self.buf[self.end..]).expect("the gap ends on a boundary");
        (before, after)
    }

    /// The text in `start..end` as one or two slices, cut at the cursor, with
    /// only the bytes handed out checked.
    fn slice(&self, start: usize, end: usize) -> (&str, &str) {
        let len = self.len();
        let start = start.min(len);
        let end = end.clamp(start, len);
        let cursor = self.start;
        let gap = self.end - self.start;
        let first = if start < cursor {
            &self.buf[start..end.min(cursor)]
        } else {
            &[]
        };
        let second = if end > cursor {
            &self.buf[start.max(cursor) + gap..end + gap]
        } else {
            &[]
        };
        (
            std::str::from_utf8(first).expect("a row starts on a boundary"),
            std::str::from_utf8(second).expect("a row ends on a boundary"),
        )
    }

    fn insert(&mut self, text: &str) {
        let bytes = text.as_bytes();
        self.reserve(bytes.len());
        self.buf[self.start..self.start + bytes.len()].copy_from_slice(bytes);
        self.start += bytes.len();
        debug_assert!(self.start <= self.end && self.end <= self.buf.len());
    }

    /// Make room for `extra` bytes at the cursor: the gap grows to its need,
    /// never below [`GAP`], and the vector doubles its capacity, so a typed
    /// line reallocates a few times, not on every character.
    fn reserve(&mut self, extra: usize) {
        let gap = self.end - self.start;
        if gap >= extra {
            return;
        }
        let grow = gap.max(extra).max(GAP) - gap;
        let after = self.buf.len() - self.end;
        self.buf.resize(self.end + grow + after, 0);
        self.buf
            .copy_within(self.end..self.end + after, self.end + grow);
        self.buf[self.end..self.end + grow].fill(0);
        self.end += grow;
        debug_assert!(self.start <= self.end && self.end <= self.buf.len());
    }

    /// Move the gap so that text position `at` is the cursor.
    fn move_to(&mut self, at: usize) {
        let at = at.min(self.len());
        if at < self.start {
            let moved = self.start - at;
            let end = self.end - moved;
            self.buf.copy_within(at..self.start, end);
            self.start = at;
            self.end = end;
        } else if at > self.start {
            let moved = at - self.start;
            self.buf.copy_within(self.end..self.end + moved, self.start);
            self.start += moved;
            self.end += moved;
        }
        debug_assert!(self.start <= self.end && self.end <= self.buf.len());
    }

    /// Delete `start..end`, leaving the cursor at `start`.
    fn erase(&mut self, start: usize, end: usize) {
        if start == end {
            return;
        }
        self.move_to(end);
        self.start = start;
    }

    /// Capacity of the buffer, for the steady-state allocation test.
    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

/// The text seen as bytes: the part before the cursor, the gap, the part after
/// it. A character is read from the at most four bytes it takes, so reading one
/// never checks or scans the whole text.
struct Scan<'a> {
    buf: &'a [u8],
    /// The gap: `buf[..start]` then `buf[end..]` is the text.
    start: usize,
    end: usize,
}

impl<'a> Scan<'a> {
    fn new(gap: &'a Gap) -> Self {
        Scan {
            buf: &gap.buf,
            start: gap.start,
            end: gap.end,
        }
    }

    fn gap(&self) -> usize {
        self.end - self.start
    }

    fn len(&self) -> usize {
        self.buf.len() - self.gap()
    }

    /// The buffer index of text position `at`.
    fn index(&self, at: usize) -> usize {
        if at < self.start { at } else { at + self.gap() }
    }

    /// The text position of buffer index `index`.
    fn position(&self, index: usize) -> usize {
        if index < self.start {
            index
        } else {
            index - self.gap()
        }
    }

    /// The byte at text position `at`.
    fn byte_at(&self, at: usize) -> Option<u8> {
        self.buf.get(self.index(at)).copied()
    }

    fn char_at(&self, at: usize) -> Option<char> {
        let index = self.index(at);
        let first = *self.buf.get(index)?;
        // The common case, and the one that must not pay for a check.
        if first < 0x80 {
            return Some(first as char);
        }
        let len = utf8_len(first);
        let bytes = self.buf.get(index..index.checked_add(len)?)?;
        std::str::from_utf8(bytes).ok()?.chars().next()
    }

    /// The start of the character before `at`.
    fn prev_char(&self, at: usize) -> Option<usize> {
        if at == 0 {
            return None;
        }
        let mut index = self.index(at - 1);
        while index > 0 && self.buf.get(index).is_some_and(|byte| byte & 0xc0 == 0x80) {
            index -= 1;
        }
        Some(self.position(index))
    }

    /// The start of the grapheme before `at`.
    fn prev_boundary(&self, at: usize) -> usize {
        let Some(mut at) = self.prev_char(at) else {
            return 0;
        };
        while at > 0 && self.char_at(at).is_some_and(is_extend) {
            at = self.prev_char(at).unwrap_or(0);
        }
        at
    }

    /// The start of the grapheme after `at`.
    fn next_boundary(&self, at: usize) -> usize {
        let len = self.len();
        let Some(c) = self.char_at(at) else {
            return at;
        };
        let mut at = at + c.len_utf8();
        while let Some(c) = self.char_at(at) {
            if !is_extend(c) {
                break;
            }
            at += c.len_utf8();
        }
        at.min(len)
    }

    /// The text from `at` to `end` when the gap does not split it.
    fn contiguous(&self, at: usize, end: usize) -> Option<&'a str> {
        let bytes: &'a [u8] = self.buf;
        if at < self.start && end > self.start {
            return None;
        }
        let (first, last) = if end <= self.start {
            (at, end)
        } else {
            (at + self.gap(), end + self.gap())
        };
        std::str::from_utf8(bytes.get(first..last)?).ok()
    }
}

/// How many bytes the character that starts with `byte` takes; a byte that
/// cannot start one reads a length that fails the check.
fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// A combining mark or other zero-width character that extends the one before
/// it. The cell grid draws by the same rule.
fn is_extend(c: char) -> bool {
    matches!(UnicodeWidthChar::width(c), Some(0))
}

/// The end of the grapheme at `at` and the columns it takes: the base character
/// with its zero-width and variation characters, measured whole, as the cell
/// grid measures it.
fn grapheme_width(scan: &Scan<'_>, at: usize, marks: &[Mark]) -> (usize, u32) {
    let end = scan.next_boundary(at);
    // A folded chip draws its whole label where the text holds one mark, so the
    // layout measures the mark as the columns the chip takes.
    if let Ok(index) = marks.binary_search_by_key(&at, |mark| mark.at) {
        return (end, u32::from(marks[index].width));
    }
    // One printable ASCII character is one grapheme of one column.
    if end == at + 1
        && scan
            .byte_at(at)
            .is_some_and(|byte| (0x20..0x7f).contains(&byte))
    {
        return (end, 1);
    }
    // A tab takes the one column the screen draws for it; every other control
    // character takes none, since the screen writes nothing for it.
    if let Some(c) = scan.char_at(at)
        && c.is_control()
    {
        return (end, char_width(c));
    }
    let width = match scan.contiguous(at, end) {
        Some(text) => UnicodeWidthStr::width(text) as u32,
        None => split_width(scan, at, end),
    };
    (end, width)
}

/// The width of a grapheme the gap splits, measured from a copy of its bytes:
/// a base and a variation selector are worth more together than apart.
fn split_width(scan: &Scan<'_>, at: usize, end: usize) -> u32 {
    let mut buf = [0u8; GRAPHEME_BYTES];
    let mut len = 0;
    let mut step = at;
    while step < end {
        let Some(c) = scan.char_at(step) else {
            break;
        };
        step += c.len_utf8();
        match buf.get_mut(len..) {
            Some(rest) if rest.len() >= 4 => len += c.encode_utf8(rest).len(),
            _ => return sum_width(scan, at, end),
        }
    }
    match std::str::from_utf8(&buf[..len]) {
        Ok(text) => UnicodeWidthStr::width(text) as u32,
        Err(_) => sum_width(scan, at, end),
    }
}

/// The columns of a grapheme too long to measure whole, one character at a time.
fn sum_width(scan: &Scan<'_>, at: usize, end: usize) -> u32 {
    let mut width = 0;
    let mut step = at;
    while step < end {
        let Some(c) = scan.char_at(step) else {
            break;
        };
        width += char_width(c);
        step += c.len_utf8();
    }
    width
}

/// The columns one character takes: a tab is the one column the screen draws
/// for it, any other control character none.
fn char_width(c: char) -> u32 {
    if c.is_control() {
        return u32::from(c == '\t');
    }
    UnicodeWidthChar::width(c).unwrap_or(0) as u32
}

/// The visual rows of the text at `width`, wrapped from text position `from`
/// (a row start) until the row that holds `upto`, and the text position where
/// the wrap stopped, which is the text length once every row is built.
fn wrap_rows(
    scan: &Scan<'_>,
    width: u16,
    rows: &mut Vec<Row>,
    from: usize,
    upto: usize,
    marks: &[Mark],
) -> usize {
    let limit = u32::from(width.max(1));
    let len = scan.len();
    let mut start = from;
    let mut col = 0;
    let mut at = from;
    while at < len {
        let Some(c) = scan.char_at(at) else {
            break;
        };
        if c == '\n' {
            rows.push(Row { start, end: at });
            at += 1;
            start = at;
            col = 0;
            // At `upto` itself the row that holds it is still to build.
            if start > upto && start < len {
                return start;
            }
            continue;
        }
        let (next, w) = grapheme_width(scan, at, marks);
        if next <= at {
            break;
        }
        if col > 0 && col + w > limit {
            rows.push(Row { start, end: at });
            start = at;
            col = 0;
            if start >= upto {
                return start;
            }
        }
        col += w;
        at = next;
    }
    rows.push(Row { start, end: len });
    len
}

/// The row the cursor is drawn in: a wrap point shows at the end of the row
/// above, so pressing up from it keeps going up.
fn row_of_display(rows: &[Row], at: usize) -> usize {
    rows.partition_point(|row| row.end < at)
        .min(rows.len().saturating_sub(1))
}

/// The row the character at the cursor is on: the last row that starts at or
/// before it. It differs from [`row_of_display`] only at a wrap point.
fn row_of_start(rows: &[Row], at: usize) -> usize {
    rows.partition_point(|row| row.start <= at)
        .saturating_sub(1)
}

/// The column of text position `at` inside `row`, in cells.
fn column_of(scan: &Scan<'_>, row: Row, at: usize, marks: &[Mark]) -> u16 {
    let mut col = 0;
    let mut step = row.start;
    while step < at && step < row.end {
        let (next, width) = grapheme_width(scan, step, marks);
        if next <= step {
            break;
        }
        col += width;
        step = next;
    }
    col.min(u32::from(u16::MAX)) as u16
}

/// The text position that Up or Down lands on in row `index` at column `col`.
/// The start of a wrapped row draws at the end of the row above, so the cursor
/// takes the first grapheme too, or the key would not change the drawn row.
fn position_in_row(scan: &Scan<'_>, rows: &[Row], index: usize, col: u32, marks: &[Mark]) -> usize {
    let at = position_at(scan, rows[index], col, marks);
    let wrapped = index > 0 && rows[index - 1].end == at;
    if wrapped {
        return grapheme_width(scan, at, marks).0.max(at);
    }
    at
}

/// The text position in `row` that shows at column `col`, or the end of the
/// row when it is shorter.
fn position_at(scan: &Scan<'_>, row: Row, col: u32, marks: &[Mark]) -> usize {
    let mut used = 0;
    let mut at = row.start;
    while at < row.end {
        let (next, width) = grapheme_width(scan, at, marks);
        if next <= at || used + width > col {
            break;
        }
        used += width;
        at = next;
    }
    at
}

#[cfg(test)]
mod tests;

/// The word primitives, the transposes and the case commands of the readline
/// keys.
impl Editor {
    /// The word after `at`: its start and end. The characters between the words
    /// are skipped first, so the word is the next alphanumeric run.
    fn word_after(&self, at: usize) -> (usize, usize) {
        let scan = Scan::new(&self.gap);
        let mut start = at;
        while let Some(c) = scan.char_at(start) {
            if c.is_alphanumeric() {
                break;
            }
            start += c.len_utf8();
        }
        let mut end = start;
        while let Some(c) = scan.char_at(end) {
            if !c.is_alphanumeric() {
                break;
            }
            end += c.len_utf8();
        }
        (start, end)
    }

    /// The word before `at`: its start and end. The characters between the
    /// words are skipped first, so the word is the alphanumeric run before it.
    fn word_before(&self, at: usize) -> (usize, usize) {
        let scan = Scan::new(&self.gap);
        let mut end = at;
        while let Some(prev) = scan.prev_char(end) {
            if scan.char_at(prev).is_some_and(char::is_alphanumeric) {
                break;
            }
            end = prev;
        }
        let mut start = end;
        while let Some(prev) = scan.prev_char(start) {
            if !scan.char_at(prev).is_some_and(char::is_alphanumeric) {
                break;
            }
            start = prev;
        }
        (start, end)
    }

    /// The start of the whitespace and the word before `at`: Ctrl+W's rule,
    /// which counts any character that is not whitespace as part of the word.
    fn space_word_start(&self, at: usize) -> usize {
        let scan = Scan::new(&self.gap);
        let mut at = at;
        while let Some(prev) = scan.prev_char(at) {
            if !scan.char_at(prev).is_some_and(char::is_whitespace) {
                break;
            }
            at = prev;
        }
        while let Some(prev) = scan.prev_char(at) {
            if scan.char_at(prev).is_some_and(char::is_whitespace) {
                break;
            }
            at = prev;
        }
        at
    }

    /// The end of the word and the whitespace after `at`: Ctrl+W's forward rule.
    fn space_word_end(&self, at: usize) -> usize {
        let scan = Scan::new(&self.gap);
        let mut at = at;
        while let Some(c) = scan.char_at(at) {
            if c.is_whitespace() {
                break;
            }
            at += c.len_utf8();
        }
        while let Some(c) = scan.char_at(at) {
            if !c.is_whitespace() {
                break;
            }
            at += c.len_utf8();
        }
        at
    }

    /// The spaces and tabs around the cursor: where they start and end.
    fn spaces(&self) -> (usize, usize) {
        let scan = Scan::new(&self.gap);
        let cursor = self.gap.cursor();
        let space = |c: char| c == ' ' || c == '\t';
        let mut start = cursor;
        while let Some(prev) = scan.prev_char(start) {
            if !scan.char_at(prev).is_some_and(space) {
                break;
            }
            start = prev;
        }
        let mut end = cursor;
        while let Some(c) = scan.char_at(end) {
            if !space(c) {
                break;
            }
            end += c.len_utf8();
        }
        (start, end)
    }

    /// The end of the line `lines` lines forward from `at`, which is the end of
    /// the text when there are fewer.
    fn lines_forward(&self, at: usize, lines: usize) -> usize {
        let mut end = at;
        for _ in 0..lines {
            let stop = self.line_end_of(end);
            if stop == end {
                break;
            }
            end = if stop < self.gap.len() && Scan::new(&self.gap).char_at(stop) == Some('\n') {
                stop + 1
            } else {
                stop
            };
        }
        end
    }

    /// The start of the line `lines` lines back from `at`, which is the start of
    /// the text when there are fewer.
    fn lines_back(&self, at: usize, lines: usize) -> usize {
        let mut start = at;
        for _ in 0..lines {
            let stop = self.line_start_of(start);
            if stop == start {
                break;
            }
            start = if stop > 0 { stop - 1 } else { stop };
        }
        start
    }

    /// The text of `from..to` as one string: the copy an undo step keeps.
    fn copied(&self, from: usize, to: usize) -> String {
        let (head, tail) = self.gap.slice(from, to);
        let mut out = String::with_capacity(to - from);
        out.push_str(head);
        out.push_str(tail);
        out
    }

    /// The position of the next `c` after the cursor, or of the last one before
    /// it: readline's character search does not wrap.
    fn find_char(&self, c: char, forward: bool) -> Option<usize> {
        let scan = Scan::new(&self.gap);
        if forward {
            let mut at = scan.next_boundary(self.gap.cursor());
            while let Some(found) = scan.char_at(at) {
                if found == c {
                    return Some(at);
                }
                at += found.len_utf8();
            }
            return None;
        }
        let mut at = self.gap.cursor();
        while let Some(prev) = scan.prev_char(at) {
            if scan.char_at(prev) == Some(c) {
                return Some(prev);
            }
            at = prev;
        }
        None
    }

    /// Ctrl+T: swap the grapheme before the cursor with the one after it, or the
    /// two before it when the cursor ends the text.
    fn swap_graphemes(&mut self) {
        let cursor = self.gap.cursor();
        let (start, middle, end) = {
            let scan = Scan::new(&self.gap);
            if cursor == 0 {
                return;
            } else if cursor == scan.len() {
                let end = cursor;
                let middle = scan.prev_boundary(end);
                (scan.prev_boundary(middle), middle, end)
            } else {
                (
                    scan.prev_boundary(cursor),
                    cursor,
                    scan.next_boundary(cursor),
                )
            }
        };
        if start == middle || middle == end {
            return;
        }
        if self.covers_chip(start, end) {
            return;
        }
        let first = self.copied(start, middle);
        let second = self.copied(middle, end);
        let mut text = String::with_capacity(end - start);
        text.push_str(&second);
        text.push_str(&first);
        self.splice(start, end, &text);
        self.gap.move_to(end);
        self.readline.point(cursor);
    }

    /// Alt+T: swap the word before the cursor with the word at or after it, and
    /// leave the cursor after the word that moved forward.
    fn swap_words(&mut self) {
        let cursor = self.gap.cursor();
        let (start2, end2) = {
            let scan = Scan::new(&self.gap);
            let mut at = cursor;
            while let Some(c) = scan.char_at(at) {
                if c.is_alphanumeric() {
                    break;
                }
                at += c.len_utf8();
            }
            if at < scan.len() {
                let (start, end) = self.word_after(at);
                // A cursor inside a word takes the whole word, not its tail.
                let start = if at == cursor {
                    self.word_before(cursor).0
                } else {
                    start
                };
                (start, end)
            } else {
                self.word_before(cursor)
            }
        };
        if start2 == end2 {
            return;
        }
        let (start1, end1) = self.word_before(start2);
        if start1 == end1 {
            return;
        }
        if self.covers_chip(start1, end2) {
            return;
        }
        let first = self.copied(start1, end1);
        let second = self.copied(start2, end2);
        let between = self.copied(end1, start2);
        let mut text = String::with_capacity(second.len() + between.len() + first.len());
        text.push_str(&second);
        text.push_str(&between);
        text.push_str(&first);
        self.splice(start1, end2, &text);
        self.gap.move_to(end2);
    }

    /// Set the case of the word after the cursor and leave the cursor at its
    /// end; false when no word follows it.
    fn reword(&mut self, case: Case) -> bool {
        let (start, end) = self.word_after(self.gap.cursor());
        if start == end {
            return false;
        }
        let word = self.copied(start, end);
        let mut text = String::with_capacity(word.len());
        let mut first = true;
        for c in word.chars() {
            match case {
                Case::Upper => text.extend(c.to_uppercase()),
                Case::Lower => text.extend(c.to_lowercase()),
                Case::Capitalize if first => text.extend(c.to_uppercase()),
                Case::Capitalize => text.extend(c.to_lowercase()),
            }
            first = false;
        }
        self.splice(start, end, &text);
        true
    }

    /// Take the folded chips inside `from..to` out of the text, for the undo
    /// step that covers them: each is kept whole, at its offset from `from`.
    fn take_chips(&mut self, from: usize, to: usize) -> Vec<(usize, Paste)> {
        let mut taken = Vec::new();
        let mut index = 0;
        while index < self.pastes.len() {
            let (start, end) = self.pastes[index].range();
            if start >= from && end <= to && !self.pastes[index].is_open() {
                let chip = self.pastes.remove(index);
                taken.push((start - from, chip));
            } else {
                index += 1;
            }
        }
        taken
    }

    /// Whether a folded chip stands in `from..to`: a transposed range keeps its
    /// chips where they are, so a swap that covers one leaves the text as it
    /// was rather than swap a mark for a bare one.
    fn covers_chip(&self, from: usize, to: usize) -> bool {
        self.pastes.iter().any(|paste| {
            let (start, end) = paste.range();
            !paste.is_open() && start < to && from < end
        })
    }

    /// The start of the character the byte `at` is in.
    fn boundary(&self, at: usize) -> usize {
        let scan = Scan::new(&self.gap);
        let mut at = at.min(scan.len());
        while at > 0 && scan.byte_at(at).is_some_and(|byte| byte & 0xc0 == 0x80) {
            at -= 1;
        }
        at
    }
}
