//! The paste chips: a bracketed paste longer than three lines stays in one
//! buffer and shows as a chip the text box draws and opens.
//!
//! The pasted bytes are held once. A folded chip keeps them in its own
//! buffer and puts one [`MARK`] character in the editor text, at the paste
//! point; an open chip hands them to the editor's own buffer, so the text
//! is editable, and takes them back when it folds. Nothing copies them for a
//! frame: the folded chip draws its first line by range, and the send path
//! copies each folded chip once into the message it builds.
//!
//! The mark is what makes the rest cheap. The layout measures it as the columns
//! the chip draws in its place ([`Paste::width`]), so the rows, the cursor
//! column and the wrap agree with the drawn chip without a case of their own; an
//! edit moves it as it moves text, since `moved` shifts the chips by the bytes
//! the edit took and added; and an edit that covers it removes the chip whole,
//! which is what makes a backspace on a folded chip delete it in one key.

use std::sync::Arc;

use unicode_width::UnicodeWidthStr;

use super::Editor;

/// The line breaks a paste may hold and still insert as text: one more folds
/// the paste into a chip.
pub const LINES: usize = 3;

/// The glyph a folded paste chip opens with: three lines folded to one.
pub const CHIP: &str = "≡";

/// The glyph a folded chip puts before the count of the lines it holds back.
pub const CHIP_MORE: &str = "… +";

/// The one character a folded chip puts in the editor text: U+FFFC, the object
/// replacement character. No terminal draws it, since the chip draws over its
/// cells, and the layout measures it as the columns the chip takes.
pub const MARK: char = '\u{fffc}';

/// The bytes the mark takes in the editor text.
pub const MARK_BYTES: usize = MARK.len_utf8();

/// A paste: the pasted bytes and the ranges computed once, at paste.
///
/// A chip is copied by its handle: the kill ring holds the chips a kill took
/// beside the undo step that takes the kill back, and neither copies the bytes.
#[derive(Debug, Clone)]
pub struct Paste {
    /// The pasted bytes while the chip is folded; empty while it is open, when
    /// the editor's own buffer holds them.
    text: Arc<str>,
    /// The first line's byte range in `text`: all the folded chip draws.
    first: (usize, usize),
    /// How many lines the paste has after the first, for the chip's count.
    other: usize,
    /// The columns the folded chip's label takes: `≡ first line … +N`, measured
    /// once, so a frame that lays the text out measures no line of the paste.
    width: u16,
    /// The chip's range in the editor text: the mark while it is folded, the
    /// pasted bytes while it is open.
    range: (usize, usize),
    /// Whether the chip's bytes are the editor's text.
    open: bool,
}

impl Paste {
    /// The first line of the paste: what a folded chip draws.
    pub fn first_line(&self) -> &str {
        &self.text[self.first.0..self.first.1]
    }

    /// How many lines the paste has after the first.
    pub fn other(&self) -> usize {
        self.other
    }

    /// The columns the folded chip's label takes: `≡ <first line> … +N`.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// The chip's range in the editor text.
    pub fn range(&self) -> (usize, usize) {
        self.range
    }

    /// Whether the chip is open for editing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The pasted bytes the chip holds while it is folded, empty while it is
    /// open: the editor's text holds them then.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// True when the edit point at `at` is on the chip: at either end of the
    /// mark of a folded one, inside the text of an open one.
    fn holds(&self, at: usize) -> bool {
        self.range.0 <= at && at <= self.range.1
    }

    /// Put the chip back at `at` in the editor text, as the undo of the edit
    /// that took it out does.
    pub(super) fn put(&mut self, at: usize) {
        self.range = (at, at + MARK_BYTES);
    }
}

impl Editor {
    /// Insert a bracketed paste at the cursor. More than [`LINES`] line breaks
    /// fold into one chip at the paste point; a shorter paste is text.
    pub fn paste(&mut self, text: String) {
        let mut text = text;
        // The mark is the text's own: a paste that held one would draw a chip it
        // is not, so the bytes lose it.
        if text.contains(MARK) {
            text.retain(|c| c != MARK);
        }
        if text.is_empty() {
            return;
        }
        // A chip cannot hold a chip: an open one folds first, which puts the
        // text back to marks before this paste adds one.
        if let Some(index) = self.selected_paste()
            && self.pastes[index].open
        {
            self.fold(index);
        }
        let (first, other) = measure(&text);
        if other < LINES {
            self.insert(&text);
            return;
        }
        let at = self.gap.cursor();
        let width = chip_width(&text[first.0..first.1], other);
        self.insert(MARK_STR);
        let index = self.pastes.partition_point(|paste| paste.range.0 < at);
        self.pastes.insert(
            index,
            Paste {
                text: text.into(),
                first,
                other,
                width,
                range: (at, at + MARK_BYTES),
                open: false,
            },
        );
    }

    /// The chips, in text order.
    pub fn pastes(&self) -> &[Paste] {
        &self.pastes
    }

    /// The index of the chip the edit point is on, if any.
    pub fn selected_paste(&self) -> Option<usize> {
        let cursor = self.gap.cursor();
        self.pastes.iter().position(|paste| paste.holds(cursor))
    }

    /// Tab: open the chip at the edit point, or fold it back when it is open.
    /// False when no chip is there, so the key is the transcript's or the box's
    /// as it was.
    pub fn toggle_paste(&mut self) -> bool {
        let Some(index) = self.selected_paste() else {
            return false;
        };
        if self.pastes[index].open {
            self.fold(index);
        } else {
            self.open(index);
        }
        true
    }

    /// Open the chip at `index` in place: its bytes become the editor's text,
    /// so the edit point moves in them and every edit is kept.
    fn open(&mut self, index: usize) {
        let (start, end) = self.pastes[index].range;
        let text = std::mem::take(&mut self.pastes[index].text);
        self.gap.erase(start, end);
        self.gap.insert(&text);
        self.pastes[index].range = (start, start + text.len());
        self.pastes[index].open = true;
        self.gap.move_to(start);
        self.touch(start);
        // The chip moved the text under the undo log and the mark.
        self.readline.forget();
    }

    /// Fold the chip at `index` back: the mark takes its range in the text and
    /// the chip takes the text, edits and all.
    fn fold(&mut self, index: usize) {
        let (start, end) = self.pastes[index].range;
        let (head, tail) = self.gap.slice(start, end);
        let mut text = String::with_capacity(end - start);
        text.push_str(head);
        text.push_str(tail);
        let (first, other) = measure(&text);
        let width = chip_width(&text[first.0..first.1], other);
        self.gap.erase(start, end);
        self.gap.insert(MARK_STR);
        let paste = &mut self.pastes[index];
        paste.text = text.into();
        paste.first = first;
        paste.other = other;
        paste.width = width;
        paste.range = (start, start + MARK_BYTES);
        paste.open = false;
        self.gap.move_to(start);
        self.touch(start);
        self.readline.forget();
    }

    /// The text changed at `at`: `removed` bytes went and `added` arrived, so
    /// every chip after the change moves with the text behind it and a chip the
    /// change covered goes whole.
    pub(super) fn moved(&mut self, at: usize, removed: usize, added: usize) {
        if self.pastes.is_empty() {
            return;
        }
        let end = at + removed;
        let mut index = 0;
        while index < self.pastes.len() {
            let (start, stop) = self.pastes[index].range;
            let (start, stop) = if removed == 0 {
                // An insert: the bytes at `at` and after it move right, and one
                // that lands inside an open chip makes it longer.
                let start = if start < at { start } else { start + added };
                let stop = if stop <= at { stop } else { stop + added };
                (start, stop)
            } else {
                // An erase: what it covered is gone and what followed it moved
                // left by the bytes it took.
                let start = shift(start, at, end, removed);
                let stop = shift(stop, at, end, removed);
                (start, stop)
            };
            if stop <= start {
                self.pastes.remove(index);
            } else {
                self.pastes[index].range = (start, stop);
                index += 1;
            }
        }
    }

    /// Open every folded chip, so the text stands on its own.
    pub(super) fn expand_pastes(&mut self) {
        while let Some(index) = self.pastes.iter().position(|paste| !paste.open) {
            self.open(index);
        }
    }

    /// The text with every chip's pasted bytes in place, built into `out` once.
    pub(super) fn write_text(&self, out: &mut String) {
        let len = self.gap.len();
        let mut from = 0;
        for paste in &self.pastes {
            if paste.open {
                // Its bytes are the text already: nothing to splice in.
                continue;
            }
            let (start, _) = paste.range;
            let (head, tail) = self.gap.slice(from, start);
            out.push_str(head);
            out.push_str(tail);
            out.push_str(paste.text());
            from = start + MARK_BYTES;
        }
        let (head, tail) = self.gap.slice(from, len);
        out.push_str(head);
        out.push_str(tail);
    }

    /// The bytes the message takes: the text with every folded chip's paste in
    /// place of its mark.
    pub(super) fn expanded_len(&self) -> usize {
        let mut len = self.gap.len();
        for paste in &self.pastes {
            if !paste.open {
                len += paste.text.len() - MARK_BYTES;
            }
        }
        len
    }

    /// The bytes the editor's buffer holds, for the steady-state test.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.gap.capacity()
    }

    /// The room the chip list holds, for the steady-state test.
    #[cfg(test)]
    pub(crate) fn chip_capacity(&self) -> usize {
        self.pastes.capacity()
    }
}

/// The mark as the text writes it.
const MARK_STR: &str = "\u{fffc}";

/// Where a byte at `at` lands after the range `from..to` went: it stays, it
/// moves up to the start of the erase, or it moves left by the bytes erased.
fn shift(at: usize, from: usize, to: usize, removed: usize) -> usize {
    if at < from {
        at
    } else if at >= to {
        at - removed
    } else {
        from
    }
}

/// The first line's range in `text` and the count of the lines after it.
///
/// A trailing line break ends the last line instead of starting an empty one,
/// `\r\n` counts as one break and a lone `\r` as one too, so a paste counts the
/// lines its writer meant whatever it wrote them with.
fn measure(text: &str) -> ((usize, usize), usize) {
    let bytes = text.as_bytes();
    let mut first = bytes.len();
    let mut other = 0;
    let mut at = 0;
    while at < bytes.len() {
        // `\r\n` is one break, and a lone `\r` is one as well: a paste from a
        // Windows or an old-Mac editor has the lines its writer meant.
        let len = match bytes[at] {
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                at += 1;
                continue;
            }
        };
        if first == bytes.len() {
            first = at;
        }
        // A break at the very end ends the last line rather than starting an
        // empty one, so the paste has no line after it.
        if at + len < bytes.len() {
            other += 1;
        }
        at += len;
    }
    ((0, first), other)
}

/// The columns a folded chip's label takes: `≡ <first line> … +N`.
fn chip_width(first: &str, other: usize) -> u16 {
    let mut digits = 1;
    let mut held = other;
    while held >= 10 {
        digits += 1;
        held /= 10;
    }
    let width = UnicodeWidthStr::width(CHIP)
        + 1
        + UnicodeWidthStr::width(first)
        + 1
        + UnicodeWidthStr::width(CHIP_MORE)
        + digits;
    width as u16
}

#[cfg(test)]
mod tests {
    use super::super::Editor;
    use super::{MARK, MARK_BYTES};

    /// The first line the compiler prints for a type error.
    const FIRST: &str = "error[E0308]: mismatched types";

    /// A paste of `lines` lines whose first line is [`FIRST`].
    fn log(lines: usize) -> String {
        let mut text = String::from(FIRST);
        for line in 1..lines {
            text.push_str("\n  ");
            text.push_str(&line.to_string());
        }
        text
    }

    /// The editor's text as the editor holds it, a folded chip's mark and all.
    fn raw(editor: &Editor) -> String {
        let (before, after) = editor.chunks();
        let mut out = String::with_capacity(before.len() + after.len());
        out.push_str(before);
        out.push_str(after);
        out
    }

    #[test]
    fn a_142_line_paste_folds_into_one_chip() {
        let mut editor = Editor::new();
        editor.paste(log(142));
        assert_eq!(editor.pastes().len(), 1, "one paste, one chip");
        let chip = &editor.pastes()[0];
        assert_eq!(chip.first_line(), FIRST, "the chip draws the first line");
        assert_eq!(chip.other(), 141, "the count of the lines behind it");
        assert!(!chip.is_open(), "a paste folds");
        assert_eq!(
            chip.range().1 - chip.range().0,
            MARK_BYTES,
            "a folded chip is one mark in the text"
        );
        assert_eq!(raw(&editor), MARK.to_string(), "the text is the mark");
    }

    #[test]
    fn a_three_line_paste_inserts_as_text() {
        let mut editor = Editor::new();
        editor.paste("one\ntwo\nthree".to_owned());
        assert!(editor.pastes().is_empty(), "three lines is not a chip");
        assert_eq!(raw(&editor), "one\ntwo\nthree");
        editor.paste("one\ntwo\nthree\nfour".to_owned());
        assert_eq!(editor.pastes().len(), 1, "a fourth line folds it");
    }

    #[test]
    fn tab_opens_the_chip_and_folds_it_back() {
        let mut editor = Editor::new();
        editor.insert("here is the log: ");
        editor.paste(log(4));
        assert!(editor.toggle_paste(), "tab takes a chip at the edit point");
        assert_eq!(
            raw(&editor),
            format!("here is the log: {}", log(4)),
            "the chip's bytes are the text now"
        );
        assert!(editor.pastes()[0].is_open(), "the chip is open");
        assert!(editor.toggle_paste(), "tab folds it back");
        assert!(!editor.pastes()[0].is_open(), "the chip is folded");
        assert_eq!(
            raw(&editor),
            format!("here is the log: {MARK}"),
            "and the mark is back in the text"
        );
    }

    #[test]
    fn an_edit_inside_an_open_chip_is_kept_when_folded_and_sent() {
        let mut editor = Editor::new();
        let text = log(4);
        editor.paste(text.clone());
        assert!(editor.toggle_paste());
        editor.right();
        editor.insert("Z\n");
        assert!(editor.toggle_paste(), "the chip folds with the edit in it");
        assert_eq!(editor.pastes()[0].other(), 4, "the edited text has 5 lines");
        assert_eq!(
            editor.take(),
            format!("eZ\n{}", &text[1..]),
            "and the edited text is what it sends"
        );
    }

    #[test]
    fn backspace_removes_a_folded_chip_whole() {
        let mut editor = Editor::new();
        editor.paste(log(142));
        assert_eq!(editor.pastes().len(), 1);
        editor.backspace();
        assert!(editor.pastes().is_empty(), "the chip goes whole");
        assert!(editor.is_empty(), "and nothing of it is left");
    }

    #[test]
    fn the_sent_text_holds_the_paste_in_place() {
        let mut editor = Editor::new();
        let text = log(142);
        editor.insert("here is the log:\n");
        editor.paste(text.clone());
        editor.insert(" why does it fail?");
        assert_eq!(
            editor.take(),
            format!("here is the log:\n{text} why does it fail?"),
            "the message is the typed text with the paste in its place"
        );
    }

    #[test]
    fn a_three_line_paste_with_a_trailing_newline_inserts_as_text() {
        let mut editor = Editor::new();
        editor.paste("a\nb\nc\n".to_owned());
        assert!(
            editor.pastes().is_empty(),
            "the last line break ends the third line, it does not start a fourth"
        );
        assert_eq!(raw(&editor), "a\nb\nc\n");
        editor.take();
        // A fourth line folds it, and the trailing break of that one is the end
        // of the fourth line too.
        editor.paste("a\nb\nc\nd\n".to_owned());
        assert_eq!(editor.pastes().len(), 1, "a fourth line folds it");
        assert_eq!(
            editor.pastes()[0].other(),
            3,
            "three lines behind the first"
        );
    }

    #[test]
    fn a_paste_with_a_trailing_newline_counts_the_lines_behind_the_first() {
        let mut editor = Editor::new();
        editor.paste(format!("{}\n", log(142)));
        let chip = &editor.pastes()[0];
        assert_eq!(chip.first_line(), FIRST, "the chip draws the first line");
        assert_eq!(chip.other(), 141, "142 lines leave 141 behind the first");
    }

    #[test]
    fn a_paste_counts_a_carriage_return_as_one_line_break() {
        let mut editor = Editor::new();
        // Windows line endings: three lines, so the paste is text.
        editor.paste("a\r\nb\r\nc\r\n".to_owned());
        assert!(
            editor.pastes().is_empty(),
            "three Windows lines are three lines"
        );
        assert_eq!(raw(&editor), "a\r\nb\r\nc\r\n");
        editor.take();
        // Four of them fold, with the first line cut at the break and none of it
        // left over.
        editor.paste("e\r\nf\r\ng\r\nh\r\n".to_owned());
        let chip = &editor.pastes()[0];
        assert_eq!(chip.first_line(), "e", "the first line ends at the break");
        assert_eq!(chip.other(), 3, "three lines behind the first");
        // A lone carriage return is one break as well.
        editor.take();
        editor.paste("i\rj\rk\r".to_owned());
        assert!(editor.pastes().is_empty(), "three old-Mac lines are three");
        editor.take();
        editor.paste("i\rj\rk\rl".to_owned());
        let chip = &editor.pastes()[0];
        assert_eq!(
            chip.first_line(),
            "i",
            "the first line ends at the carriage"
        );
        assert_eq!(chip.other(), 3, "three lines behind the first");
    }

    #[test]
    fn history_recall_after_a_paste_leaves_no_chip() {
        let mut editor = Editor::new();
        editor.paste(log(142));
        assert_eq!(editor.pastes().len(), 1, "the log is one chip");
        assert!(editor.history_up(&["hi"]), "up takes the newest message");
        assert!(
            editor.pastes().is_empty(),
            "the recalled text holds no chip of the draft's"
        );
        assert!(!editor.toggle_paste(), "tab finds no chip in it");
        // Tab folds nothing, and the message is the history's own text: no panic
        // and no stray bytes of the paste.
        assert_eq!(editor.take(), "hi");
    }

    #[test]
    fn down_from_a_recalled_message_gives_the_draft_back_as_text() {
        let mut editor = Editor::new();
        let text = log(4);
        editor.paste(text.clone());
        assert!(editor.history_up(&["hi"]), "up takes the newest message");
        assert!(editor.history_down(&["hi"]), "down goes back to the draft");
        assert!(
            editor.pastes().is_empty(),
            "the draft is expanded text, so it holds no chip"
        );
        assert_eq!(editor.take(), text, "the draft keeps the paste's bytes");
    }

    #[test]
    fn the_paste_is_stored_once() {
        let mut editor = Editor::new();
        let text = log(142);
        let len = text.len();
        editor.paste(text);
        assert_eq!(
            editor.len(),
            MARK_BYTES,
            "the text holds the chip's mark and none of its bytes"
        );
        let stored: usize = editor.pastes().iter().map(|chip| chip.text().len()).sum();
        assert_eq!(stored, len, "the editor holds one copy of the paste");
    }

    #[test]
    fn a_stray_mark_in_a_paste_cannot_be_a_chip() {
        let mut editor = Editor::new();
        editor.paste(format!("a\nb\nc\nd{MARK}e"));
        assert_eq!(editor.pastes().len(), 1, "the paste is one chip");
        assert!(
            !editor.pastes()[0].text().contains(MARK),
            "the mark is stripped from the pasted bytes"
        );
        assert!(editor.toggle_paste());
        assert_eq!(editor.take(), "a\nb\nc\nde");
    }

    #[test]
    fn a_folded_chip_moves_with_the_text_before_it() {
        let mut editor = Editor::new();
        editor.paste(log(5));
        let at = editor.pastes()[0].range().0;
        editor.line_start();
        editor.insert("ab");
        assert_eq!(
            editor.pastes()[0].range().0,
            at + 2,
            "the chip moved with the text"
        );
    }

    #[test]
    fn a_folded_chip_changes_columns_with_the_text_before_it() {
        let mut editor = Editor::new();
        editor.paste(log(5));
        editor.line_start();
        editor.insert("ab");
        let layout = editor.layout(80, 4);
        assert_eq!(
            layout.cursor_col(),
            2,
            "the edit point sits after the two typed characters"
        );
    }
}
