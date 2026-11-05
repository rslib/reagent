//! Where full screen meets the app: entering the alternate screen with the
//! mouse captured, laying the transcript out over the terminal and scrolling
//! it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::term::{KEYS_ON, MOUSE_ON};
use super::{App, Block, Entry, Screen, Terminal};

impl App {
    /// Switches the terminal to the alternate screen and captures the mouse,
    /// when the mode draws full screen, and falls back to inline when the
    /// terminal refuses the sequence.
    ///
    /// The screen takes the id of this session first, so the line the exit
    /// leaves names it. The sequence, the mouse capture and the extended keys go
    /// out in one write, the first frame of the run. Inline mode asks only for
    /// the extended keys: it draws on the normal screen and keeps the mouse for
    /// the terminal's own selection.
    pub(super) async fn enter(&mut self, terminal: &mut impl Terminal) {
        self.screen.set_session(&self.session);
        self.screen.enter(&mut self.frame);
        let full = !self.frame.is_empty();
        if full {
            self.frame.extend_from_slice(MOUSE_ON);
        }
        self.frame.extend_from_slice(KEYS_ON);
        if let Err(error) = terminal.write_frame(&self.frame).await
            && full
        {
            // A terminal that cannot switch keeps the inline mode, which draws
            // only the rows under the last committed line, and says so. The
            // regions were laid out for the alternate screen, so they are laid
            // out again: inline mode has no sidebar and the whole width.
            let (columns, rows) = (self.screen.columns(), self.screen.term_rows());
            self.screen = Screen::inline(columns, rows, 1);
            self.reflow();
            self.note(format!("full screen: {error}"));
        }
    }

    /// Draws the rows of the transcript the viewport shows in the main area, the
    /// live entries under them, and answers the row the box, the editor and the
    /// status line follow at.
    ///
    /// The transcript gives up the rows the live entries, the box, the editor and
    /// the status line need at the bottom of the main area. One page of the ring
    /// is the rows it shows, so a page turn costs those rows and not the lines
    /// behind them.
    pub(super) fn paint_full(&mut self, editor_rows: u16, dim: u32) -> u16 {
        let box_rows = self.approval_rows();
        let main = self.main;
        let body = main.height.saturating_sub(box_rows + editor_rows + 1);
        let live = self.live_rows().min(body);
        let transcript = body - live;
        self.screen
            .set_transcript_area(main.x, main.y, main.width, transcript);
        self.screen.draw_transcript();
        self.paint_new_lines(main.y, transcript);
        self.paint_live(main.y + transcript, live, dim)
    }

    /// Draws the row that says how far the viewport is behind, on the last row
    /// of the transcript, while the viewport is not at the bottom.
    ///
    /// The row is the same block the transcript's rows draw, so it takes its
    /// fill, its glyph and its hint from the same place. Nothing else draws in
    /// full screen, so the count is the lines under the viewport's last row.
    fn paint_new_lines(&mut self, top: u16, rows: u16) {
        if rows == 0 {
            return;
        }
        let transcript = self.screen.transcript();
        if transcript.follow() {
            return;
        }
        let seen = transcript
            .top()
            .saturating_add(u64::from(transcript.height()));
        let count = transcript.len().saturating_sub(seen);
        let entry = Entry {
            id: 0,
            block: Block::NewLines { count },
        };
        // The fold state goes with the entries into the frame, and comes back
        // unchanged: the row has no block of its own to open.
        let folds = std::mem::take(&mut self.folds);
        let y = top.saturating_add(rows).saturating_sub(1);
        self.paint_blocks(y, 0, 1, &[entry], &folds);
        self.folds = folds;
    }

    /// Scrolls the transcript for the keys that page it, and answers whether the
    /// key was one of them.
    ///
    /// Only full screen scrolls: inline mode has no viewport of its own, its
    /// transcript is the terminal's scrollback. End goes to the bottom only with
    /// nothing typed, so it keeps moving the editor's cursor otherwise.
    pub(super) fn scrolls(&mut self, event: KeyEvent) -> bool {
        if !self.screen.is_full() {
            return false;
        }
        let shift = event.modifiers.contains(KeyModifiers::SHIFT);
        let idle = self.editor.is_empty();
        let transcript = self.screen.transcript_mut();
        match (event.code, shift) {
            (KeyCode::PageUp, _) => transcript.page_up(),
            (KeyCode::PageDown, _) => transcript.page_down(),
            (KeyCode::Up, true) => transcript.scroll_up(1),
            (KeyCode::Down, true) => transcript.scroll_down(1),
            (KeyCode::End, _) if idle => transcript.scroll_to_bottom(),
            _ => return false,
        }
        true
    }
}
