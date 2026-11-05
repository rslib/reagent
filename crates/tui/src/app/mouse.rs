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
use crate::mouse::{Cell, MouseAction};
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
            self.selecting = false;
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
    /// rows its first row is above `row`, while the live entries fit the rows
    /// under the viewport: a live area that had to cut them off names none.
    fn live_block(&mut self, row: u16) -> Option<(u64, u16)> {
        let height = self.screen.transcript().height();
        let start = self.main.y.saturating_add(height);
        if height == 0 || row < start {
            return None;
        }
        let mut offset = row - start;
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
    /// The rows are cut at the main area's right edge, so a drag that runs into
    /// a docked sidebar copies the transcript's text beside it and none of the
    /// sidebar's cells.
    fn copy_selection(&mut self) {
        self.selecting = false;
        let Some(selection) = self.mouse.selection() else {
            self.mouse.clear();
            return;
        };
        let first = selection.start().row;
        let width = usize::from(self.main.right());
        self.picked.clear();
        for row in selection.start().row..=selection.end().row {
            let mut text = self.screen.row_text(row);
            super::cut_in_place(&mut text, width);
            self.picked.push(text);
        }
        let picked = &self.picked;
        let wrote = selection.copy(
            |row| {
                picked
                    .get(usize::from(row.saturating_sub(first)))
                    .map_or("", String::as_str)
            },
            &mut self.clip,
            &mut self.clipboard,
        );
        if !wrote {
            self.clipboard.clear();
        }
        self.mouse.clear();
        self.touch();
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
