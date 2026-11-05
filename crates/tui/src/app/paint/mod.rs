//! The frame: one module per region of the screen.
//!
//! `App::paint` is the frame order. It clears the rows, rebuilds the status
//! line when it changed, and then draws, top down: the transcript rows and the
//! live entries, the approval card, the text box, the status line; the sidebar
//! and the drag's selection go over all of them last. The header row takes its
//! place above the transcript with the task that draws it.
//!
//! Each region lives in its own module, so a restyle of one region is an edit
//! of one file:
//!
//! - [`transcript`]: the transcript rows of the ring, the live entries, and the
//!   commit of finished lines into the scrollback or the ring.
//! - [`approval`]: the approval card.
//! - [`editor`]: the text box.
//! - [`list`]: the list above the text box.
//! - [`sheet`]: a sheet in place of the text box.
//! - [`status`]: the status line.
//! - [`theme`]: the roles the regions draw with.

mod approval;
mod editor;
mod list;
mod sheet;
mod status;
mod theme;
mod transcript;

pub(crate) use self::transcript::rows::{
    Block, CompactedRow, Diff, DiffKind, Entry, GroupRow, Span, SpanRole, State, ToolRow, Usage,
};
pub(crate) use self::transcript::{LineSink, SPANS};

use super::{App, DIM, REVERSE};

impl App {
    /// Draws one frame: the live area in inline mode, the whole terminal in
    /// full screen.
    ///
    /// The regions go out in this order, top down, and each of them lives in the
    /// module named above it: the header row (with the task that draws it), the
    /// transcript rows and the live entries, the approval card, the text box,
    /// the status line. The sidebar and the drag's selection go over all of
    /// them last.
    pub(super) fn paint(&mut self) {
        let rows = self.screen.rows();
        for y in 0..rows {
            self.screen.clear_row(y);
        }
        let dim = self.screen.intern(DIM);
        let reverse = self.screen.intern(REVERSE);
        self.refresh_status();
        self.wants_checkpoint();
        let width = self.text_width();
        let editor_rows = self.editor_height();
        let list_rows = self.list_height();
        let full = self.screen.is_full();
        let mut y = if full {
            self.paint_full(editor_rows, dim)
        } else {
            let box_rows = self.approval_rows();
            let limit = rows.saturating_sub(box_rows + editor_rows + 1);
            self.paint_live(0, limit, dim)
        };
        // In full screen the list lies over the transcript's bottom rows, above
        // the card; inline it takes the rows the live area left it, right above
        // the text box.
        let mut list_y = (list_rows > 0 && full).then(|| y.saturating_sub(list_rows));
        y += self.paint_approval(y);
        if list_rows > 0 && !full {
            list_y = Some(y);
        }
        if let Some(at) = list_y {
            self.paint_list(at);
        }
        if list_rows > 0 && !full {
            y += list_rows;
        }
        y += self.paint_editor(y, editor_rows, reverse);
        // The card's note holds the frame's one cursor while it is open; the
        // box placed its own a moment ago, and the palette takes it back.
        self.paint_card_cursor();
        if let Some(at) = list_y {
            self.paint_list_cursor(at);
        }
        self.paint_status(y, rows, width, dim);
        // The sidebar goes over everything the transcript and the editor drew,
        // then the drag's selection goes over it.
        self.paint_sidebar(dim);
        self.paint_selection(reverse);
    }

    /// The rows the regions of the frame need for their content: the live
    /// entries, the approval card, the text box and the status line.
    pub(super) fn content_rows(&mut self) -> u16 {
        let mut rows = self.live_rows();
        rows += self.approval_rows();
        rows += self.editor_height();
        rows += self.list_height();
        (rows + 1).min(self.screen.term_rows())
    }
}
#[cfg(test)]
mod tests {
    use crate::client::{Approval, Initialized};
    use crate::screen::Grid;

    use super::super::{App, ApprovalBox, Block, Entry, Item, Usage};

    /// The columns and the rows of the test terminal: wide enough for the full
    /// screen frame, too narrow for the sidebar to dock.
    const COLUMNS: u16 = 80;
    const ROWS: u16 = 24;

    /// The text of one painted row of the screen, trailing blanks trimmed.
    fn row(grid: &Grid, y: u16) -> String {
        let mut text = String::new();
        for x in 0..grid.width() {
            match grid.cell(x, y).char() {
                Some(c) => text.push(c),
                None => text.push(' '),
            }
        }
        let keep = text.trim_end_matches(' ').len();
        text.truncate(keep);
        text
    }

    /// The text of every painted row of the screen, top down.
    fn rows(app: &App) -> Vec<String> {
        let grid = app.screen.back();
        (0..app.screen.rows()).map(|y| row(grid, y)).collect()
    }

    /// The row the drawn screen shows `needle` on, when it shows it at all.
    fn at(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    /// The app of the frame test: one live row, one approval card, one line of
    /// text in the box, and a session id on the status line.
    fn app() -> App {
        let init = Initialized {
            session: "sess-1".to_owned(),
            ..Initialized::default()
        };
        let mut app = App::new(&init, (COLUMNS, ROWS), true);
        app.live.push(Item::Plain {
            lines: vec!["read src/main.rs".to_owned(), "  +14 -3".to_owned()],
            dim: false,
        });
        app.approval = Some(ApprovalBox::new(
            Approval {
                id: 1,
                tool: "bash".into(),
                call: "rm -rf target".into(),
                rule: "outside the project".into(),
                save: true,
            },
            COLUMNS,
        ));
        app.editor.insert("typed text");
        app.editor_dirty = true;
        app
    }

    /// The rows of the new transcript draw in a real frame: a user turn, a
    /// thinking block, a steer row and the rule a turn ends with.
    #[test]
    fn a_frame_draws_the_transcript_rows_of_a_turn() {
        let mut app = app();
        app.live.clear();
        app.live.push(crate::app::Item::User {
            text: "the first question".into(),
        });
        app.live
            .push(crate::app::Item::Thought(Box::new(crate::app::Thinking {
                text: "checking the tree".to_owned(),
                since: std::time::Instant::now(),
                seconds: 4,
                fold: 1,
            })));
        app.live.push(crate::app::Item::Steer {
            text: "use nextest".into(),
            queued: false,
        });
        app.live.push(crate::app::Item::Rule(Usage {
            seconds: 12,
            tokens: 42_000,
            cents: 42,
        }));
        app.paint();
        let rows = rows(&app);
        let user = at(&rows, "› the first question").expect("the user turn draws");
        let thought = at(&rows, "thought for 4s").expect("the thinking row draws");
        let steer = at(&rows, "steer").expect("the steer row draws");
        let rule = at(&rows, "42.0k tokens · $0.42")
            .unwrap_or_else(|| panic!("the turn's rule draws:\n{}", rows.join("\n")));
        assert!(
            user < thought && thought < steer && steer < rule,
            "the rows draw in the order the turn made them:\n{}",
            rows.join("\n")
        );
    }

    /// A block's rows are the rows a frame counts: the count the layout takes
    /// from `blocks_rows` is the rows `paint_blocks` draws.
    #[test]
    fn the_rows_a_block_counts_are_the_rows_it_draws() {
        let mut app = app();
        let mut folds = crate::app::fold::Folds::new();
        folds.add();
        let entry = Entry {
            id: 1,
            block: Block::Thinking {
                seconds: 7,
                text: "a line of the reasoning",
            },
        };
        let counted = app.blocks_rows(&[entry], &folds);
        let drawn = app.paint_blocks(0, 0, 40, &[entry], &folds);
        assert_eq!(counted, drawn, "the layout and the drawing agree");
        assert!(
            row(app.screen.back(), 0).trim().is_empty(),
            "a blank row above"
        );
        let drawn = row(app.screen.back(), 1);
        assert_eq!(
            drawn, "  thought for 7s  ▸",
            "the folded row, its two margins blank, under a blank row"
        );
        folds.toggle_at(None);
        let open = app.blocks_rows(&[entry], &folds);
        assert!(open > counted, "the open block draws its line too: {open}");
    }

    /// A frame draws the live rows above the approval card, the card above the
    /// text box and the text box above the status line.
    ///
    /// The restyle tasks of the change draw into one region each: the row this
    /// test pins is the order they stay in, whatever they draw in it.
    #[test]
    fn a_frame_draws_the_live_rows_then_the_card_then_the_box_then_the_status() {
        let mut app = app();
        app.paint();
        let rows = rows(&app);
        let live = at(&rows, "read src/main.rs").expect("the live row draws");
        let card = at(&rows, "rm -rf target").expect("the approval card draws");
        let box_ = at(&rows, "typed text").expect("the text box draws");
        let status = at(&rows, "waits for you").expect("the status line draws");
        assert!(
            live < card && card < box_ && box_ < status,
            "the regions draw out of order: live {live}, card {card}, box {box_}, \
             status {status}\n{}",
            rows.join("\n")
        );
    }
}
