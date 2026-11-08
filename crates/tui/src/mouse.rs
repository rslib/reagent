//! Mouse events in full screen: the wheel, a drag selection and its copy.
//!
//! A drag selects whole screen cells. [`Selection`] turns them into text and
//! into the one OSC 52 sequence that puts that text in the terminal's
//! clipboard. The module holds no screen types: the caller gives the text of a
//! row, one char per glyph, so it can draw its own grid; a wide glyph is two
//! cells and one char, and the copy cuts by cells.
//!
//! A drag moves two cells and no heap block: [`Mouse`] is two [`Cell`]s and a
//! flag. A copy writes into the buffers the caller keeps for it.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use unicode_width::UnicodeWidthChar;

/// Lines one notch of the wheel scrolls.
pub const WHEEL_LINES: u16 = 3;

/// A cell of the screen: `column` from the left and `row` from the top, both
/// from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The column, from the left.
    pub column: u16,
    /// The row, from the top.
    pub row: u16,
}

impl Cell {
    /// The cell at `column` of `row`.
    pub const fn new(column: u16, row: u16) -> Cell {
        Cell { column, row }
    }

    /// How selections order cells: by row, then by column.
    const fn key(self) -> (u16, u16) {
        (self.row, self.column)
    }
}

/// What one mouse event asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// The wheel turned up: scroll the region under `at` up `lines` lines.
    ScrollUp { at: Cell, lines: u16 },
    /// The wheel turned down: scroll the region under `at` down `lines` lines.
    ScrollDown { at: Cell, lines: u16 },
    /// The left button went down at `at`: the start of a drag or a click.
    Press(Cell),
    /// The left button moved to `at` with the button down.
    Drag(Cell),
    /// The left button came up at `at` after a drag: copy the selection.
    Release(Cell),
    /// The left button went down and up on one cell: a click, no selection.
    Click(Cell),
    /// An event of change 2 does nothing with: another button, a horizontal
    /// wheel, or a Shift drag, which the terminal keeps for itself.
    Ignore,
}

/// The mouse in full screen: the press a drag may follow, and its selection.
#[derive(Debug, Default)]
pub struct Mouse {
    /// The cell the left button went down on, while it is down.
    anchor: Option<Cell>,
    /// The cells from the anchor to the pointer, once the pointer moved.
    selection: Option<Selection>,
}

impl Mouse {
    /// A mouse with nothing pressed.
    pub fn new() -> Mouse {
        Mouse::default()
    }

    /// Feeds one event and says what the app should do for it.
    pub fn event(&mut self, event: MouseEvent) -> MouseAction {
        let at = Cell::new(event.column, event.row);
        // A Shift drag is the terminal's own selection: it keeps the text and
        // reagent never sees it (TUI change 2 grill Q7).
        if event.modifiers.contains(KeyModifiers::SHIFT) {
            return MouseAction::Ignore;
        }
        match event.kind {
            MouseEventKind::ScrollUp => MouseAction::ScrollUp {
                at,
                lines: WHEEL_LINES,
            },
            MouseEventKind::ScrollDown => MouseAction::ScrollDown {
                at,
                lines: WHEEL_LINES,
            },
            MouseEventKind::Down(MouseButton::Left) => {
                self.anchor = Some(at);
                self.selection = None;
                MouseAction::Press(at)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.anchor else {
                    return MouseAction::Ignore;
                };
                self.selection = if anchor == at {
                    None
                } else {
                    Some(Selection::new(anchor, at))
                };
                MouseAction::Drag(at)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(anchor) = self.anchor.take() else {
                    return MouseAction::Ignore;
                };
                self.selection = None;
                if anchor == at {
                    return MouseAction::Click(at);
                }
                self.selection = Some(Selection::new(anchor, at));
                MouseAction::Release(at)
            }
            _ => MouseAction::Ignore,
        }
    }

    /// The selection the drag so far makes, for the highlight and the copy.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Drops the selection, after a copy.
    pub fn clear(&mut self) {
        self.anchor = None;
        self.selection = None;
    }
}

/// The cells between two screen cells, both included, in screen order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    start: Cell,
    end: Cell,
}

impl Selection {
    /// The cells between `anchor` and `head`, in screen order: `start` is the
    /// one above, or the one on the left of the same row.
    pub fn new(anchor: Cell, head: Cell) -> Selection {
        if anchor.key() <= head.key() {
            Selection {
                start: anchor,
                end: head,
            }
        } else {
            Selection {
                start: head,
                end: anchor,
            }
        }
    }

    /// The first cell.
    pub fn start(&self) -> Cell {
        self.start
    }

    /// The last cell, both included.
    pub fn end(&self) -> Cell {
        self.end
    }

    /// Writes the text of the selected cells into `out`, one line per row and
    /// no line break after the last.
    ///
    /// `row` gives the text of one row of the screen, one char per glyph, so a
    /// row padded with spaces to the width of the terminal is cut to the
    /// selected columns and its last spaces are left out. A wide glyph takes
    /// two cells on screen and one char here, so this copy cuts by cell: a wide
    /// glyph the selection covers half of counts whole.
    pub fn text<'a>(&self, row: impl Fn(u16) -> &'a str, out: &mut String) {
        out.clear();
        for line in self.start.row..=self.end.row {
            if line > self.start.row {
                out.push('\n');
            }
            let first = if line == self.start.row {
                self.start.column
            } else {
                0
            };
            let last = if line == self.end.row {
                self.end.column
            } else {
                u16::MAX
            };
            out.push_str(cut(row(line), first, last));
        }
    }

    /// Writes one OSC 52 sequence holding the selected text into `out`, with
    /// `text` as the scratch it leaves the plain text in, and says whether it
    /// wrote one.
    ///
    /// The sequence is `ESC ] 52 ; c ; <base64> BEL`: the terminal copies the
    /// text without a platform clipboard, which is the copy of change 2. A
    /// selection of blank cells has no text, and a sequence with an empty
    /// payload would clear the user's clipboard, so `out` is left empty and the
    /// answer is false.
    pub fn copy<'a>(
        &self,
        row: impl Fn(u16) -> &'a str,
        text: &mut String,
        out: &mut Vec<u8>,
    ) -> bool {
        self.text(row, text);
        let wrote = Self::copy_text(text, out);
        if !wrote {
            text.clear();
        }
        wrote
    }

    /// Writes one OSC 52 sequence holding `text` into `out`, and says whether it
    /// wrote one: the text a copy made of the rows a drag covers, which is not
    /// always their cells.
    pub fn copy_text(text: &str, out: &mut Vec<u8>) -> bool {
        out.clear();
        if text.trim().is_empty() {
            return false;
        }
        out.extend_from_slice(OSC_52);
        push_base64(text.as_bytes(), out);
        out.push(BEL);
        true
    }
}

/// The start of an OSC 52 sequence: put the text in the clipboard (`c`).
const OSC_52: &[u8] = b"\x1b]52;c;";

/// The byte that ends an OSC sequence.
const BEL: u8 = 0x07;

/// The chars of `line` whose cells the columns `first` and `last` cover, both
/// included, without the spaces a row padded to the width of the terminal ends
/// with.
pub(crate) fn cut(line: &str, first: u16, last: u16) -> &str {
    let (start, end) = cells(line, first, last);
    line.get(start..end)
        .unwrap_or_default()
        .trim_end_matches(' ')
}

/// The byte range of `line` that the cells `first..=last` cover.
///
/// One char is one glyph and takes [`UnicodeWidthChar::width`] cells, two for a
/// CJK glyph: a glyph the selection covers half of counts whole, and a
/// zero-width mark goes with the glyph before it.
fn cells(line: &str, first: u16, last: u16) -> (usize, usize) {
    let first = u32::from(first);
    let last = u32::from(last);
    let mut column = 0u32;
    let mut start = None;
    let mut end = 0;
    // Whether the glyph before this char is in the selection, which is what a
    // zero-width mark follows.
    let mut selected = false;
    for (at, ch) in line.char_indices() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0) as u32;
        let selected_now = if width == 0 {
            selected
        } else {
            column + width > first && column <= last
        };
        if selected_now {
            start.get_or_insert(at);
            end = at + ch.len_utf8();
        }
        selected = selected_now;
        column += width;
    }
    (start.unwrap_or(0), end)
}

/// Writes the base64 of `bytes` into `out`: RFC 4648 section 4, with the `=`
/// padding, which is what OSC 52 and the kitty image protocol take.
fn push_base64(bytes: &[u8], out: &mut Vec<u8>) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (groups, rest) = bytes.as_chunks::<3>();
    for group in groups {
        let bits = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        for shift in [18, 12, 6, 0] {
            out.push(ALPHABET[(bits >> shift) as usize & 63]);
        }
    }
    if let Some(&first) = rest.first() {
        let bits = u32::from(first) << 16 | u32::from(rest.get(1).copied().unwrap_or(0)) << 8;
        out.push(ALPHABET[(bits >> 18) as usize & 63]);
        out.push(ALPHABET[(bits >> 12) as usize & 63]);
        out.push(if rest.len() == 2 {
            ALPHABET[(bits >> 6) as usize & 63]
        } else {
            b'='
        });
        out.push(b'=');
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::{Cell, Mouse, MouseAction, Selection, WHEEL_LINES};

    const NONE: KeyModifiers = KeyModifiers::NONE;

    fn event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: NONE,
        }
    }

    fn row_of<'a>(lines: &'a [&'a str], row: u16) -> &'a str {
        lines.get(row as usize).copied().unwrap_or("")
    }

    #[test]
    fn the_wheel_scrolls_the_region_under_it() {
        let mut mouse = Mouse::new();
        assert_eq!(
            mouse.event(event(MouseEventKind::ScrollUp, 12, 7)),
            MouseAction::ScrollUp {
                at: Cell::new(12, 7),
                lines: WHEEL_LINES,
            },
            "the wheel up names the cell it turned on"
        );
        assert_eq!(
            mouse.event(event(MouseEventKind::ScrollDown, 3, 1)),
            MouseAction::ScrollDown {
                at: Cell::new(3, 1),
                lines: WHEEL_LINES,
            }
        );
        assert_eq!(
            mouse.event(event(MouseEventKind::ScrollLeft, 3, 1)),
            MouseAction::Ignore,
            "a horizontal wheel scrolls nothing"
        );
    }

    #[test]
    fn a_drag_over_two_rows_takes_the_selected_columns() {
        let lines = ["abcdefg     ", "hijklmn     "];
        let row = |row: u16| row_of(&lines, row);
        let mut mouse = Mouse::new();
        assert_eq!(
            mouse.event(event(MouseEventKind::Down(MouseButton::Left), 2, 0)),
            MouseAction::Press(Cell::new(2, 0))
        );
        assert_eq!(
            mouse.event(event(MouseEventKind::Drag(MouseButton::Left), 3, 1)),
            MouseAction::Drag(Cell::new(3, 1))
        );
        let selection = mouse.selection().expect("a drag selects");
        assert_eq!(selection.start(), Cell::new(2, 0));
        assert_eq!(selection.end(), Cell::new(3, 1));
        let mut text = String::new();
        selection.text(row, &mut text);
        assert_eq!(text, "cdefg\nhijk");
        assert_eq!(
            mouse.event(event(MouseEventKind::Up(MouseButton::Left), 3, 1)),
            MouseAction::Release(Cell::new(3, 1))
        );
        assert_eq!(
            mouse.selection(),
            Some(selection),
            "the release keeps the selection for the copy"
        );
    }

    #[test]
    fn a_drag_downwards_and_upwards_selects_the_same_cells() {
        let lines = ["abcdefg", "hijklmn", "opqrstu"];
        let row = |row: u16| row_of(&lines, row);
        let mut text = String::new();
        // From the bottom row up: the selection is normalized into screen order.
        Selection::new(Cell::new(4, 2), Cell::new(1, 0)).text(row, &mut text);
        assert_eq!(text, "bcdefg\nhijklmn\nopqrs");
        // The first and last rows are cut, the rows between them are whole.
        Selection::new(Cell::new(2, 0), Cell::new(2, 2)).text(row, &mut text);
        assert_eq!(text, "cdefg\nhijklmn\nopq");
    }

    #[test]
    fn the_copy_is_one_osc_52_sequence() {
        let lines = ["hi    ", "      "];
        let row = |row: u16| row_of(&lines, row);
        let mut text = String::new();
        let mut out = Vec::new();
        Selection::new(Cell::new(0, 0), Cell::new(1, 0)).copy(row, &mut text, &mut out);
        assert_eq!(out, b"\x1b]52;c;aGk=\x07");
        assert_eq!(text, "hi", "the scratch keeps the plain text");
    }

    /// A row of the screen gives one char per glyph, and a wide glyph takes two
    /// cells: the copy cuts by cell, not by char (CHG-j9kb5g review).
    #[test]
    fn a_wide_glyph_takes_two_cells_of_a_selection() {
        let lines = ["日本語abc"];
        let row = |row: u16| row_of(&lines, row);
        let mut text = String::new();
        Selection::new(Cell::new(6, 0), Cell::new(8, 0)).text(row, &mut text);
        assert_eq!(
            text, "abc",
            "the three glyphs before abc take six cells of the row"
        );
        Selection::new(Cell::new(1, 0), Cell::new(3, 0)).text(row, &mut text);
        assert_eq!(
            text, "日本",
            "a wide glyph that half the selection covers counts whole"
        );
        Selection::new(Cell::new(4, 0), Cell::new(5, 0)).text(row, &mut text);
        assert_eq!(text, "語", "the two cells of one wide glyph are that glyph");
    }

    #[test]
    fn an_ascii_row_is_cut_by_cell_as_before() {
        let lines = ["abcdefg     ", "hijklmn     "];
        let row = |row: u16| row_of(&lines, row);
        let mut text = String::new();
        Selection::new(Cell::new(2, 0), Cell::new(4, 1)).text(row, &mut text);
        assert_eq!(text, "cdefg\nhijkl");
        Selection::new(Cell::new(9, 0), Cell::new(12, 0)).text(row, &mut text);
        assert_eq!(text, "", "cells past the text of a row are blank");
    }

    /// Rows of blank cells hold only line ends, and a copy of those would
    /// replace the user's clipboard with blank text.
    #[test]
    fn native_copy_of_blank_rows_writes_nothing() {
        let lines = ["hi", "    ", "    "];
        let row = |row: u16| row_of(&lines, row);
        let mut text = String::new();
        let mut out = Vec::new();
        assert!(
            !Selection::new(Cell::new(0, 1), Cell::new(3, 2)).copy(row, &mut text, &mut out),
            "two blank rows copy nothing"
        );
        assert_eq!(text, "", "the scratch is the empty text");
        assert!(out.is_empty(), "no sequence, so the clipboard is kept");
    }

    /// An empty copy would send OSC 52 with no payload, which clears the user's
    /// clipboard: it writes nothing and says so (CHG-j9kb5g review).
    #[test]
    fn a_copy_of_blank_cells_writes_nothing() {
        let lines = ["hi    ", "      "];
        let row = |row: u16| row_of(&lines, row);
        let mut text = String::new();
        let mut out = Vec::new();
        assert!(
            !Selection::new(Cell::new(2, 0), Cell::new(5, 0)).copy(row, &mut text, &mut out),
            "a selection of blank cells copies nothing"
        );
        assert_eq!(text, "", "the scratch is the empty text");
        assert!(out.is_empty(), "no sequence, so the clipboard is kept");
        assert!(
            Selection::new(Cell::new(0, 0), Cell::new(1, 0)).copy(row, &mut text, &mut out),
            "a selection with text says it wrote"
        );
        assert_eq!(out, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn base64_matches_known_values() {
        for (bytes, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("hello", "aGVsbG8="),
            ("\u{00e9}", "w6k="),
        ] {
            let mut out = Vec::new();
            super::push_base64(bytes.as_bytes(), &mut out);
            assert_eq!(String::from_utf8(out).unwrap(), want, "{bytes:?}");
        }
    }

    #[test]
    fn a_click_without_a_drag_is_a_click() {
        let mut mouse = Mouse::new();
        assert_eq!(
            mouse.event(event(MouseEventKind::Down(MouseButton::Left), 5, 2)),
            MouseAction::Press(Cell::new(5, 2))
        );
        assert_eq!(
            mouse.event(event(MouseEventKind::Up(MouseButton::Left), 5, 2)),
            MouseAction::Click(Cell::new(5, 2)),
            "a press and a release on one cell are a click"
        );
        assert_eq!(mouse.selection(), None, "a click selects nothing");
        assert_eq!(
            mouse.event(event(MouseEventKind::Up(MouseButton::Left), 5, 2)),
            MouseAction::Ignore,
            "a release with no press is nothing"
        );
    }

    #[test]
    fn a_drag_back_to_the_press_cell_is_a_click() {
        let mut mouse = Mouse::new();
        mouse.event(event(MouseEventKind::Down(MouseButton::Left), 4, 1));
        mouse.event(event(MouseEventKind::Drag(MouseButton::Left), 9, 3));
        assert!(mouse.selection().is_some());
        assert_eq!(
            mouse.event(event(MouseEventKind::Up(MouseButton::Left), 4, 1)),
            MouseAction::Click(Cell::new(4, 1))
        );
        assert_eq!(mouse.selection(), None);
    }

    #[test]
    fn a_shift_drag_is_the_terminals_own() {
        let mut mouse = Mouse::new();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
        ] {
            let mut held = event(kind, 1, 1);
            held.modifiers = KeyModifiers::SHIFT;
            assert_eq!(
                mouse.event(held),
                MouseAction::Ignore,
                "{kind:?} with Shift is the terminal's"
            );
        }
        assert_eq!(mouse.selection(), None, "a Shift drag selects nothing");
    }

    #[test]
    fn another_button_is_ignored() {
        let mut mouse = Mouse::new();
        assert_eq!(
            mouse.event(event(MouseEventKind::Down(MouseButton::Right), 2, 2)),
            MouseAction::Ignore
        );
        assert_eq!(
            mouse.event(event(MouseEventKind::Moved, 2, 2)),
            MouseAction::Ignore
        );
    }
}
