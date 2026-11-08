//! The mouse in full screen: the wheel, a drag selection and its copy, and a
//! click that folds or opens the block or the panel it lands on.
//!
//! The selection's cells are drawn reversed over everything else, and the
//! release writes the one OSC 52 sequence of the copy into the same frame write
//! as the rows it clears. Inline mode leaves the mouse to the terminal, so it
//! takes no event here.

use crossterm::event::MouseEvent;
use unicode_width::UnicodeWidthChar;

use crate::host::Target;
use crate::layout::Rect;
use crate::mouse::{Cell, MouseAction, Selection};
use crate::screen::Color;
use crate::slot::Clicked;
use crate::theme;

use super::App;

impl App {
    /// Takes one mouse event in full screen.
    pub(super) fn mouse(&mut self, event: MouseEvent) {
        if !self.screen.is_full() {
            return;
        }
        match self.mouse.event(event) {
            MouseAction::ScrollUp { at, lines } => self.wheel(at, -i32::from(lines)),
            MouseAction::ScrollDown { at, lines } => self.wheel(at, i32::from(lines)),
            // A press in the sidebar is a click on a panel, not a selection.
            MouseAction::Press(at) => self.selecting = self.transcript(at),
            MouseAction::Drag(_) => {
                if self.selecting {
                    self.touch();
                }
            }
            MouseAction::Release(_) => {
                if self.selecting {
                    self.copy_selection();
                }
            }
            MouseAction::Click(at) => self.click(at),
            MouseAction::Ignore => {}
        }
    }

    /// A click on the cell `at` of the transcript, as the mouse takes it: the
    /// tests name the row of a block and click it.
    #[cfg(test)]
    pub(super) fn clicked(&mut self, at: Cell) {
        self.click(at);
    }

    /// Whether `at` is a cell of the transcript rather than of the sidebar.
    fn transcript(&self, at: Cell) -> bool {
        match self.aside.rect() {
            Some(rect) => !holds(rect, at),
            None => true,
        }
    }

    /// One notch of the wheel at `at`: the transcript scrolls, and the sidebar's
    /// focused panel scrolls the rows it has to show.
    fn wheel(&mut self, at: Cell, rows: i32) {
        if self.transcript(at) {
            let transcript = self.screen.transcript_mut();
            let lines = u64::from(rows.unsigned_abs());
            if rows < 0 {
                transcript.scroll_up(lines);
            } else {
                transcript.scroll_down(lines);
            }
            self.touch();
        } else if self.sidebar.scroll_by(rows) {
            self.touch();
        }
    }

    /// A click in the sidebar focuses the panel it landed on, and the sidebar
    /// takes the keyboard. A click on the panel's title row folds it or opens
    /// it, the same as Enter on the focused panel; a click on one of its rows
    /// is the panel's to answer, as a session row that opens that session.
    fn click(&mut self, at: Cell) {
        self.mouse.clear();
        // A press in the transcript held a selection: the click drops it, and the
        // frame that draws over its highlight is due even when nothing folds.
        if std::mem::replace(&mut self.selecting, false) {
            self.touch();
        }
        if self.transcript(at) {
            // A click on a folded paste chip opens it, as Tab on it does.
            if self.click_paste(at) {
                return;
            }
            // A click on a block's row folds it or opens it, as Tab on the
            // block does.
            if self.click_block(at) {
                return;
            }
            return;
        }
        let Some(panel) = self.panel_at(at.row) else {
            return;
        };
        self.sidebar.set_active(true);
        // The host moves the focus one panel at a time, so a click steps to the
        // panel it landed on.
        let focus = self.sidebar.focus();
        if panel != focus {
            for _ in 0..self.sidebar.len() {
                if self.sidebar.focus() == panel || !self.sidebar.focus_next() {
                    break;
                }
            }
        }
        if self.title_row(at.row) {
            self.sidebar.toggle_fold(panel);
        } else {
            self.click_content(panel, at);
        }
        self.touch();
    }

    /// Hands a click on the rows of the panel `panel` to the panel that drew
    /// them, which may change and ask for the frame again, or name a session
    /// the app opens the way the sessions sheet does.
    fn click_content(&mut self, panel: usize, at: Cell) {
        let Some(line) = self.sidebar_line(panel, at.row) else {
            return;
        };
        let column = self.sidebar_column(at.column);
        match self.sidebar.click(line, column) {
            Clicked::None => {}
            Clicked::Redraw => self.touch(),
            Clicked::Open(id) => {
                if id.as_ref() != self.session {
                    self.request_open(Target::Session(id.to_string()));
                }
            }
        }
    }

    /// The line of the sidebar's last draw the screen row `row` shows, for the
    /// panel `panel`: its title is on a row of the screen and on a line of the
    /// draw, and the two give the offset.
    fn sidebar_line(&self, panel: usize, row: u16) -> Option<usize> {
        let drawn = self
            .sidebar
            .titles()
            .iter()
            .find(|(_, at)| *at == panel)
            .map(|(line, _)| *line)?;
        let screen = self
            .panel_rows
            .iter()
            .find(|(_, at)| *at == panel)
            .map(|(row, _)| *row)?;
        Some(drawn + usize::from(row.checked_sub(screen)?))
    }

    /// The column of a click inside the lines the sidebar drew: the block's
    /// left edge, past the `│` the terminal mode draws, is column zero.
    fn sidebar_column(&self, column: u16) -> u16 {
        let Some(rect) = self.aside.rect() else {
            return 0;
        };
        let edge = u16::from(self.look.theme.fill(theme::Role::PanelFill) == Color::Default);
        column.saturating_sub(rect.x + edge)
    }

    /// A click on a row of a foldable block opens the block when it is folded
    /// and folds it when it is open, as Tab on the block does; a click on a
    /// call's own row inside an open run opens that call. Answers whether the
    /// row held one.
    fn click_block(&mut self, at: Cell) -> bool {
        let Some((fold, rows)) = self.block_row(at.row) else {
            return false;
        };
        if let Some(member) = self.member_at(fold, self.offset(at.row, rows)) {
            self.folds.toggle(member);
            if !self.refresh(fold) {
                // A live run has no lines in the ring yet.
                self.refold();
            }
            return true;
        }
        let all = self.folds.all();
        self.folds.toggle(fold);
        if all {
            // One block folds and the rest stay open: every block the ring
            // holds is drawn again.
            self.reopen_all();
        } else if !self.reopen(fold) {
            // A live block has no lines in the ring yet.
            self.refold();
        }
        true
    }

    /// The foldable block whose row holds the screen row `row`, once a frame
    /// drew: a block of the viewport, or a live one under it. The number comes
    /// with the rows the block's first row is above `row`.
    fn block_row(&mut self, row: u16) -> Option<(u64, u16)> {
        let top = self.main.y;
        let height = self.screen.transcript().height();
        if (top..top.saturating_add(height)).contains(&row) {
            let line = self.line_at(row);
            if let Some(fold) = self.screen.transcript().block_at(line) {
                let start = self.block_start(fold, line);
                return Some((fold, u16::try_from(line - start).unwrap_or(u16::MAX)));
            }
        }
        self.live_block(row)
    }

    /// The first line of the committed block `fold` that holds `line`: the lines
    /// above it belong to the blocks above it.
    fn block_start(&self, fold: u64, line: u64) -> u64 {
        let transcript = self.screen.transcript();
        let mut start = line;
        while start > 0 && transcript.block_at(start - 1) == Some(fold) {
            start -= 1;
        }
        start
    }

    /// The rows a click `rows` rows below the block's first row is into the
    /// block, counted from the blank row above the block: a block whose first row
    /// is its own draws that blank row already, so the count moves up one.
    fn offset(&self, row: u16, rows: u16) -> u16 {
        let first = row.saturating_sub(rows);
        if self.screen.row_text(first).trim().is_empty() {
            rows
        } else {
            rows.saturating_add(1)
        }
    }

    /// The line of the transcript the row `row` of the viewport shows: a line
    /// wider than the area takes several rows, so its rows are counted from its
    /// runs rather than assumed to be one.
    fn line_at(&self, row: u16) -> u64 {
        let transcript = self.screen.transcript();
        let offset = row.saturating_sub(self.main.y);
        if transcript.follow()
            && transcript.fills()
            && let Some(line) = self.line_from_bottom(transcript.height(), offset)
        {
            return line;
        }
        self.line_from_top(offset)
    }

    /// The line the row `offset` from the top of the area shows, counting down
    /// from the viewport's first line.
    fn line_from_top(&self, offset: u16) -> u64 {
        let transcript = self.screen.transcript();
        let width = self.main.width;
        let mut left = u64::from(offset);
        let mut line = transcript.top();
        let mut rows = self.line_rows(line, width);
        while left >= rows && line < transcript.len() {
            left -= rows;
            line += 1;
            rows = self.line_rows(line, width);
        }
        line
    }

    /// The line the row `offset` from the top of an area of `height` rows shows
    /// while the viewport is at the bottom of the ring, whose last row is the
    /// area's last row: `None` when the ring is shorter than the area, which
    /// draws from its first line.
    fn line_from_bottom(&self, height: u16, offset: u16) -> Option<u64> {
        let transcript = self.screen.transcript();
        let width = self.main.width;
        let mut left = u64::from(height).saturating_sub(1 + u64::from(offset));
        let mut line = transcript.len().checked_sub(1)?;
        let mut rows = self.line_rows(line, width);
        while left >= rows {
            line = line.checked_sub(1)?;
            left -= rows;
            rows = self.line_rows(line, width);
        }
        Some(line)
    }

    /// The rows the stored line `line` takes at `width` columns, the way the
    /// viewport draws it: a row breaks before a glyph that does not fit.
    fn line_rows(&self, line: u64, width: u16) -> u64 {
        let width = usize::from(width.max(1));
        let mut rows = 1;
        let mut used = 0;
        for (text, _) in self.screen.transcript().runs_of(line) {
            for c in text.chars() {
                let cells = if c.is_control() {
                    usize::from(c == '\t')
                } else {
                    UnicodeWidthChar::width(c).unwrap_or(0)
                };
                if used > 0 && used + cells > width {
                    rows += 1;
                    used = 0;
                }
                used += cells;
            }
        }
        rows
    }

    /// The fold of the live block whose row holds the screen row `row`, with the
    /// rows its first row is above `row`, while a frame drew the live entries:
    /// the count starts at the row the frame started the entries on, and skips
    /// the rows the last frame cut from their top.
    fn live_block(&mut self, row: u16) -> Option<(u64, u16)> {
        let height = self.screen.transcript().height();
        let start = self.main.y.saturating_add(height);
        if row < start {
            return None;
        }
        let mut offset = row - start + self.live_skip;
        for index in 0..self.live.len() {
            let rows = self.live_item_rows(index);
            if offset < rows {
                let fold = self.live[index].fold_id();
                return (fold != 0).then_some((fold, offset));
            }
            offset -= rows;
        }
        None
    }

    /// The rows the live entry at `index` takes at the width of the screen. The
    /// frame counts them from the same place, so the entry leaves the list for
    /// the count and goes back.
    fn live_item_rows(&mut self, index: usize) -> u16 {
        if index >= self.live.len() {
            return 0;
        }
        let total = self.live_rows();
        let item = self.live.remove(index);
        let rest = self.live_rows();
        self.live.insert(index, item);
        total - rest
    }

    /// Writes the OSC 52 sequence of the selection into the copy the next frame
    /// carries, and drops the highlight.
    ///
    /// A block a reply wrote is copied as the reply wrote it: a selection that
    /// touches two blocks, or covers one whole, takes the source bytes of the
    /// blocks it touches, and only a copy of part of one block takes its cells.
    /// The rows are cut at the main area's right edge, so a drag that runs into
    /// a docked sidebar copies the transcript's text beside it and none of the
    /// sidebar's cells.
    fn copy_selection(&mut self) {
        self.selecting = false;
        let Some(selection) = self.mouse.selection() else {
            self.mouse.clear();
            return;
        };
        let mut text = std::mem::take(&mut self.clip);
        self.pick_text(&selection, &mut text);
        let wrote = Selection::copy_text(&text, &mut self.clipboard);
        if !wrote {
            text.clear();
            self.clipboard.clear();
        } else {
            // The text the copy left in `clip` goes to the local clipboard too:
            // the terminal's OSC 52 sequence is for the terminal only.
            if let Some(host) = self.host.as_ref() {
                host.get().copy(&text);
            }
            self.copied = text.lines().count();
            self.copied_until = Some(std::time::Instant::now() + super::COPIED_FOR);
        }
        self.clip = text;
        self.mouse.clear();
        self.touch();
    }

    /// Writes the text a release copies into `out`: the source of each block the
    /// selection touches, as the reply wrote it, and the screen text of the rows
    /// a block does not answer for.
    fn pick_text(&self, selection: &Selection, out: &mut String) {
        out.clear();
        let (start, end) = (selection.start(), selection.end());
        let top = self.main.y;
        let bottom = top.saturating_add(self.screen.transcript().height());
        let left = self.main.x.saturating_add(self.margin());
        let stop = self.main.right().saturating_sub(1);
        let several = self.touches_several(start, end, top, bottom);
        // The source the piece before came from, while no row stands between
        // them: they join with a blank line.
        let mut previous: Option<crate::screen::Origin> = None;
        // Whether a row went into the copy, whose separator comes before it.
        let mut wrote = false;
        let mut row = start.row;
        while row <= end.row {
            let line = (top..bottom).contains(&row).then(|| self.line_at(row));
            let Some(origin) = line.and_then(|line| self.screen.origin(line)) else {
                let text = self.cells(row, start, end, left, stop);
                if wrote {
                    out.push('\n');
                }
                out.push_str(&text);
                wrote = true;
                previous = None;
                row += 1;
                continue;
            };
            // The rows this block took, which the selection may hold a part of.
            let mut last = row;
            while last < end.row {
                let next = last + 1;
                let line = (top..bottom).contains(&next).then(|| self.line_at(next));
                let held = line.is_some_and(|line| {
                    (origin.line..origin.line.saturating_add(origin.lines)).contains(&line)
                });
                if !held {
                    break;
                }
                last = next;
            }
            if several || self.whole(&origin, top, bottom, row, last, start, end, left) {
                let text = self.screen.origin_text(&origin);
                let text = text.trim_end_matches('\n');
                if wrote {
                    out.push_str(if previous.is_some() { "\n\n" } else { "\n" });
                }
                out.push_str(text);
                previous = Some(origin);
            } else {
                self.block_rows(&origin, row, last, start, end, left, stop, out, wrote);
                previous = None;
            }
            wrote = true;
            row = last + 1;
        }
        out.truncate(out.trim_end().len());
    }

    /// Whether the rows of the selection hold lines of two or more blocks.
    fn touches_several(&self, start: Cell, end: Cell, top: u16, bottom: u16) -> bool {
        let mut first = None;
        for row in start.row..=end.row {
            if !(top..bottom).contains(&row) {
                continue;
            }
            let Some(origin) = self.screen.origin(self.line_at(row)) else {
                continue;
            };
            match first {
                None => first = Some(origin.line),
                Some(line) if line != origin.line => return true,
                Some(_) => {}
            }
        }
        false
    }

    /// Whether the rows from `row` to `last` cover the whole of `origin`: every
    /// row of every line it took, from before the first text cell of the first
    /// row to after the last text cell of the last row.
    #[expect(clippy::too_many_arguments, reason = "one selection, one block")]
    fn whole(
        &self,
        origin: &crate::screen::Origin,
        top: u16,
        bottom: u16,
        row: u16,
        last: u16,
        start: Cell,
        end: Cell,
        left: u16,
    ) -> bool {
        let lines = origin.line.saturating_add(origin.lines);
        let viewport = |row: u16| (top..bottom).contains(&row);
        let above = row > top && viewport(row - 1) && self.line_at(row - 1) >= origin.line;
        let below = last < end.row && viewport(last + 1) && self.line_at(last + 1) < lines;
        if above || below || self.line_at(row) > origin.line || self.line_at(last) + 1 < lines {
            // The selection starts or ends inside a line the block wraps over,
            // or leaves out the block's first or last line, which may be
            // scrolled out of the view.
            return false;
        }
        let first = start.column.max(left);
        let stop = self.main.right().saturating_sub(1);
        let last_column = end.column.min(stop);
        let from = self.text_span(row, stop);
        if from.is_some_and(|(first_cell, _)| first > first_cell) {
            return false;
        }
        match (self.text_span(last, stop), last == end.row) {
            // A row of the block the selection starts past is not the whole one.
            (Some((_, last_cell)), true) => last_column >= last_cell,
            _ => true,
        }
    }

    /// The first and last screen column of the text of row `row`, while it has
    /// any: the first cell after the blank columns before it, and the last cell
    /// of its last glyph.
    fn text_span(&self, row: u16, stop: u16) -> Option<(u16, u16)> {
        let text = self.screen.row_text(row);
        let text = crate::mouse::cut(&text, 0, stop);
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        let lead = text.len() - text.trim_start().len();
        let first = usize::from(self.main.x) + super::columns(&text[..lead]);
        let last = first + super::columns(trimmed.trim_start());
        Some((
            u16::try_from(first).unwrap_or(u16::MAX),
            u16::try_from(last.saturating_sub(1)).unwrap_or(u16::MAX),
        ))
    }

    /// Writes the cells of the rows of one block into `out`: the rows of a code
    /// block keep their line breaks and lose its padding and its tag, and the
    /// wrapped rows of anything else join with one space.
    #[expect(clippy::too_many_arguments, reason = "one block, one selection")]
    fn block_rows(
        &self,
        origin: &crate::screen::Origin,
        row: u16,
        last: u16,
        start: Cell,
        end: Cell,
        left: u16,
        stop: u16,
        out: &mut String,
        mut wrote: bool,
    ) {
        let mut first = true;
        for row in row..=last {
            if origin.label && origin.code && self.line_at(row) == origin.line {
                // The fence's tag row holds none of the block's text.
                continue;
            }
            let text = self.cells(row, start, end, left, stop);
            let text = text.trim();
            if text.is_empty() && origin.code {
                continue;
            }
            if wrote {
                // The rows of one block keep its line breaks, and the rows a
                // wrap made of one line of anything else join with a space.
                out.push(if origin.code || first { '\n' } else { ' ' });
            }
            if origin.code {
                out.push_str(text);
            } else {
                squeeze(out, text);
            }
            first = false;
            wrote = true;
        }
    }

    /// The cells of row `row` the selection covers, the margin left out, and no
    /// cell of a sidebar beside the main area.
    fn cells(&self, row: u16, start: Cell, end: Cell, left: u16, stop: u16) -> String {
        let from = if row == start.row {
            start.column.max(left)
        } else {
            left
        };
        let to = if row == end.row {
            end.column.min(stop)
        } else {
            stop
        };
        let text = self.screen.row_text(row);
        if to < from {
            return String::new();
        }
        crate::mouse::cut(&text, from, to.max(from)).to_string()
    }

    /// Draws the cells a drag selects reversed, over everything else, so the
    /// release copies what the screen shows.
    ///
    /// A drag stops at the main area's right edge: the sidebar's cells keep
    /// their style, so no panel line shows reversed beside the selection.
    pub(super) fn paint_selection(&mut self, reverse: u32) {
        if !self.selecting {
            return;
        }
        let Some(selection) = self.mouse.selection() else {
            return;
        };
        let (start, end) = (selection.start(), selection.end());
        let (rows, right) = (self.screen.rows(), self.main.right());
        let stop = right.saturating_sub(1);
        for row in start.row..=end.row.min(rows.saturating_sub(1)) {
            let first = if row == start.row { start.column } else { 0 };
            let last = if row == end.row { end.column } else { stop };
            for column in first..=last.min(stop) {
                let cell = self.screen.back().cell(column, row);
                let text = (cell.bits() >> 32) as u32;
                self.screen
                    .set(column, row, crate::screen::Cell::new(text, reverse));
            }
        }
    }
}

/// Whether the block `rect` holds the cell `at`.
fn holds(rect: Rect, at: Cell) -> bool {
    (rect.x..rect.right()).contains(&at.column) && (rect.y..rect.bottom()).contains(&at.row)
}

/// Pushes the words of `text` into `out`: one space where it holds a run of
/// them, which is the fill a layout puts around inline code.
fn squeeze(out: &mut String, text: &str) {
    let mut space = false;
    for ch in text.chars() {
        if ch == ' ' {
            if space {
                continue;
            }
            space = true;
        } else {
            space = false;
        }
        out.push(ch);
    }
}
