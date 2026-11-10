//! The question card: what the `ask` tool asks, its numbered options and the
//! row that types an answer of its own.
//!
//! The card takes the approval card's place and its shape: every row carries the
//! accent bar in column 0, and in a rich theme the accent tint under the whole
//! width. A terminal mode has neither fill, so the bar, the bold header and the
//! reversed pick carry the card on their own, and no color is named here:
//! [`App::style`] answers an id.
//!
//! Drawing a row writes runs into the screen's cells and builds no string: the
//! option number goes through a stack buffer and the texts are the box's own.

use unicode_width::UnicodeWidthChar;

use crate::screen::{Cell, Screen};
use crate::theme::{Emphasis, Role};

use super::super::{App, Ask, Item, QuestionBox, cut_in_place};

/// The column the card's bar takes.
const BAR: u16 = 1;

/// The columns a row of the card's body indents its text by.
const INDENT: u16 = 2;

/// The blank columns between an option's label and its note.
const GAP: u16 = 2;

/// The most rows of a typed answer the card shows: the newest rows are the ones
/// the card keeps.
const TEXT_ROWS: u16 = 4;

/// The most rows the card takes. The approval card takes no more than its own
/// rows, and the frame keeps the text box and the status line under this one.
const MAX_ROWS: u16 = 18;

/// The digits of an option number, written without allocating.
const DIGITS: usize = 3;

/// The bar of the card, accent in every mode.
const MARK: &str = "\u{258e}";

/// The glyph before the header's words and the dot between a row and its note.
const QUESTION: &str = "?";

/// The marker before the picked row.
const PICK: &str = ">";

/// The placeholder of the answer field.
const OWN: &str = "type your own answer...";

/// The column the answer field ends before: one blank column at the card's
/// right edge.
fn field_right(width: u16) -> u16 {
    width.saturating_sub(1)
}

/// The column of the answer field's bar, under the option numbers.
const FIELD_BAR: u16 = BAR + INDENT;

/// The rows of the field's fill above and below its text, as the text box pads
/// its own.
const FIELD_PAD: u16 = 1;

/// The column of the field's `>`.
const FIELD_PROMPT: u16 = FIELD_BAR + 2;

/// The column the field's text starts at.
const FIELD_TEXT: u16 = FIELD_PROMPT + 2;

/// The columns the answer field wraps its text at, in a card `width` wide.
pub(crate) fn field_columns(width: u16) -> u16 {
    field_right(width).saturating_sub(FIELD_TEXT).max(1)
}

impl App {
    /// How many rows the card of the open question needs, none when no card is
    /// on screen.
    pub(crate) fn question_rows(&mut self) -> u16 {
        let width = self.text_width();
        let Some(open) = self.question.as_mut() else {
            return 0;
        };
        card_rows(open, width).min(MAX_ROWS)
    }

    /// Draws the question card from `y` down, and answers the row after it.
    pub(crate) fn paint_question(&mut self, y: u16) -> u16 {
        if self.question.is_none() {
            return 0;
        }
        let width = self.text_width();
        let card = Role::CardAccent;
        let inks = Inks {
            bar: self.style_on(Role::Accent, Emphasis::Bold, card),
            tint: self.style(card, Emphasis::Plain),
            text: self.style_on(Role::Text, Emphasis::Plain, card),
            bold: self.style_on(Role::Text, Emphasis::Bold, card),
            dim: self.style_on(Role::Dim, Emphasis::Plain, card),
            pick: self.style_on(Role::Accent, Emphasis::Bold, card),
            field: self.style_on(Role::Text, Emphasis::Plain, Role::InputFill),
            field_dim: self.style_on(Role::Dim, Emphasis::Plain, Role::InputFill),
            field_fill: self.style(Role::InputFill, Emphasis::Plain),
            field_bar: self.style_on(Role::Accent, Emphasis::Plain, Role::InputFill),
            field_pick: self.style_on(Role::Accent, Emphasis::Bold, Role::InputFill),
        };
        let Self {
            screen,
            question,
            keyfile,
            ..
        } = self;
        let keys = keyfile.as_ref().map(crate::keyfile::KeyFile::labels);
        let Some(open) = question.as_mut() else {
            return 0;
        };
        let (rows, cursor) = draw(screen, open, y, width, &inks, keys);
        self.card_cursor = cursor;
        rows
    }

    /// Adds the one line per question that the history keeps of an answer.
    pub(crate) fn question_answered(&mut self, open: &QuestionBox, answers: &[Vec<String>]) {
        let mut lines = Vec::with_capacity(open.asks.len());
        for (at, ask) in open.asks.iter().enumerate() {
            let answer = answers
                .get(at)
                .map(|list| list.join(", "))
                .unwrap_or_default();
            let line = match answer.is_empty() {
                true => format!("{QUESTION} {} -> (no answer)", ask.q),
                false => format!("{QUESTION} {} -> {answer}", ask.q),
            };
            lines.push(line);
        }
        self.question_lines(lines);
    }

    /// Adds the one line per question that the history keeps of a skipped
    /// request.
    pub(crate) fn question_skipped(&mut self, open: &QuestionBox) {
        let lines: Vec<String> = open
            .asks
            .iter()
            .map(|ask| format!("{QUESTION} {} -> skipped", ask.q))
            .collect();
        self.question_lines(lines);
    }

    /// Adds the lines of one answer to the transcript, cut to the text box's
    /// width, in the role the transcript gives a finished answer.
    fn question_lines(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let width = usize::from(self.text_width());
        self.freeze_text();
        for mut line in lines {
            cut_in_place(&mut line, width);
            self.pending.push(Item::Answer { line, ok: true });
        }
        self.dirty = true;
    }
}

/// The styles the card draws with, one interned id each.
#[derive(Debug)]
struct Inks {
    /// The bar, the header's glyph and the marker of a picked row.
    bar: u32,
    /// The accent tint under the card's rows, the screen's own style in a mode
    /// that draws no fills.
    tint: u32,
    /// Body text.
    text: u32,
    /// The header and the question.
    bold: u32,
    /// The notes, the header's count and the hint.
    dim: u32,
    /// The marker and the label of the picked row.
    pick: u32,
    /// The text typed into the answer field, on the text box's fill.
    field: u32,
    /// The field's placeholder.
    field_dim: u32,
    /// The blank cells of the field: the screen's own style in a mode that
    /// draws no fills.
    field_fill: u32,
    /// The field's bar.
    field_bar: u32,
    /// The field's `>` while the keys type into it.
    field_pick: u32,
}

/// The columns one option row writes in: its number, its label and the column
/// its note starts at, which is one column for the whole list.
#[derive(Clone, Copy, Debug)]
struct Columns {
    /// The column of the option's number or its checkbox.
    number: u16,
    /// The column the label starts at.
    label: u16,
    /// The column the note starts at.
    note: u16,
}

/// The columns the card writes in, for the widest label of the question.
fn columns(ask: &Ask, width: u16) -> Columns {
    let number = BAR + INDENT;
    let label = number + if ask.multi { CHECK_GAP } else { NUMBER_GAP };
    let widest = ask
        .options
        .iter()
        .map(|option| cells(&option.label))
        .max()
        .unwrap_or(0);
    // The note gets one column after the widest label, and never more than half
    // the card: a long label wraps in its own column instead.
    let room = widest.min(width.saturating_sub(label) / 2);
    Columns {
        number,
        label,
        note: (label + room + GAP).min(width.saturating_sub(1)),
    }
}

/// The columns between a row's number and its label: the number and a space, and
/// the checkbox of a question that takes several.
const NUMBER_GAP: u16 = 2;
const CHECK_GAP: u16 = 6;

/// The rows the card needs: a padding row, the header, the question, the
/// options, the answer field under a blank row, and the hint under another.
fn card_rows(open: &mut QuestionBox, width: u16) -> u16 {
    let mut rows = 7;
    let Some((ask, pick)) = open.current() else {
        return rows;
    };
    let columns = columns(ask, width);
    rows += wrapped_rows(&ask.q, BAR + INDENT, width);
    for option in &ask.options {
        rows += option_rows(option, &columns, width);
    }
    let _ = pick;
    rows + field_rows(open, width) + 2 * FIELD_PAD
}

/// The rows the answer field of the question on screen takes: one when it is
/// empty, and as many as its text wraps to, up to [`TEXT_ROWS`].
fn field_rows(open: &mut QuestionBox, width: u16) -> u16 {
    let at = open.at;
    let Some(field) = open
        .fields
        .get_mut(at)
        .and_then(Option::as_mut)
        .filter(|field| !field.is_empty())
    else {
        return 1;
    };
    let total = field.layout(field_columns(width), TEXT_ROWS).total();
    (total as u16).clamp(1, TEXT_ROWS)
}

/// The rows one option needs: its label in its own column, and its note beside
/// it, whichever is longer.
fn option_rows(option: &super::super::AskOption, columns: &Columns, width: u16) -> u16 {
    let label = wrapped_rows(&option.label, columns.label, columns.note);
    let note = option
        .note
        .as_deref()
        .map_or(1, |note| wrapped_rows(note, columns.note, width));
    label.max(note)
}

/// Draws the card from row `y` and answers the row after its last one, with the
/// edit point of a typed answer while one is open.
fn draw(
    screen: &mut Screen,
    open: &mut QuestionBox,
    y: u16,
    width: u16,
    inks: &Inks,
    keys: Option<&crate::keyfile::Labels>,
) -> (u16, Option<(u16, u16)>) {
    let rows = card_rows(open, width).min(MAX_ROWS);
    let last = (y + rows).min(screen.rows());
    let mut cursor = None;
    if inks.tint != screen.default_style() {
        for row in y..last {
            for x in 0..width {
                screen.set(x, row, Cell::from_char(' ', inks.tint));
            }
        }
    }
    for row in y..last {
        screen.put_str(0, row, MARK, inks.bar);
    }
    let Some((ask, pick)) = open.current() else {
        return (rows, cursor);
    };
    let columns = columns(ask, width);
    let mut row = y + 1;
    if row >= last {
        return (rows, cursor);
    }
    // The header: the glyph, the words, and which question of how many.
    let mut x = screen.put_str_until(BAR, row, QUESTION, inks.bar, width);
    x = screen.put_str_until(x, row, " ", inks.text, width);
    x = screen.put_str_until(x, row, "reagent asks", inks.bold, width);
    if open.asks.len() > 1 {
        let mut digits = [0u8; DIGITS];
        let mut after = [0u8; DIGITS];
        x = screen.put_str_until(x, row, "  ", inks.dim, width);
        x = screen.put_str_until(x, row, number(open.at + 1, &mut digits), inks.dim, width);
        x = screen.put_str_until(x, row, " of ", inks.dim, width);
        let _ = screen.put_str_until(x, row, number(open.asks.len(), &mut after), inks.dim, width);
    }
    // The question, then one row per option, its note in the column of the
    // widest label.
    row += 2;
    put_wrapped(screen, BAR + INDENT, row, width, &ask.q, inks.bold, 0, 0);
    row += wrapped_rows(&ask.q, BAR + INDENT, width);
    for (index, option) in ask.options.iter().enumerate() {
        if row >= last {
            break;
        }
        let picked = pick == index;
        let style = if picked { inks.bold } else { inks.text };
        if picked {
            screen.put_str_until(BAR, row, PICK, inks.pick, columns.number);
        }
        let mut digits = [0u8; DIGITS];
        let number = number(index + 1, &mut digits);
        let digit = if picked { inks.pick } else { inks.dim };
        screen.put_str_until(columns.number, row, number, digit, columns.label);
        if ask.multi {
            let checked = open
                .checked
                .get(open.at)
                .and_then(|set| set.get(index))
                .copied()
                .unwrap_or(false);
            let box_ = if checked { "[x]" } else { "[ ]" };
            screen.put_str_until(columns.number + 2, row, box_, style, columns.label);
        }
        let used = option_rows(option, &columns, width);
        put_wrapped(
            screen,
            columns.label,
            row,
            columns.note,
            &option.label,
            style,
            0,
            used,
        );
        if let Some(note) = option.note.as_deref() {
            put_wrapped(screen, columns.note, row, width, note, inks.dim, 0, used);
        }
        row += used;
    }
    // A blank row, then the answer field: a small text box on the text box's
    // fill, its own bar and a `>`, which grows with the answer typed into it.
    row += 1;
    if row < last {
        let typing = open.typing();
        let right = field_right(width);
        let shown = field_rows(open, width);
        let boxed = (row + shown + 2 * FIELD_PAD).min(last);
        if inks.field_fill != screen.default_style() {
            for at in row..boxed {
                for x in FIELD_BAR..right {
                    screen.set(x, at, Cell::from_char(' ', inks.field_fill));
                }
            }
        }
        for at in row..boxed {
            screen.put_str_until(FIELD_BAR, at, MARK, inks.field_bar, right);
        }
        row += FIELD_PAD;
        let prompt = if typing {
            inks.field_pick
        } else {
            inks.field_dim
        };
        screen.put_str_until(FIELD_PROMPT, row, PICK, prompt, right);
        let at = open.at;
        match open
            .fields
            .get_mut(at)
            .and_then(Option::as_mut)
            .filter(|field| !field.is_empty())
        {
            Some(field) => {
                let mut lines = [None; TEXT_ROWS as usize];
                let (cursor_row, cursor_col) = {
                    let layout = field.layout(field_columns(width), TEXT_ROWS);
                    for (slot, line) in lines.iter_mut().zip(layout.rows()) {
                        *slot = Some(*line);
                    }
                    (layout.cursor_row(), layout.cursor_col())
                };
                for (offset, line) in lines.iter().enumerate() {
                    let Some(line) = line else {
                        break;
                    };
                    let y = row + offset as u16;
                    if y >= last {
                        break;
                    }
                    let (first, second) = field.slice(line.start(), line.end());
                    let x = screen.put_str_until(FIELD_TEXT, y, first, inks.field, right);
                    screen.put_str_until(x, y, second, inks.field, right);
                }
                if typing {
                    cursor = Some((FIELD_TEXT + cursor_col, row + cursor_row));
                }
            }
            None if row < last => {
                screen.put_str_until(FIELD_TEXT, row, OWN, inks.field_dim, right);
                if typing {
                    cursor = Some((FIELD_TEXT, row));
                }
            }
            None => {}
        }
        row += shown + FIELD_PAD;
    }
    // The hint, under a blank row, and the padding row after it.
    row += 1;
    if row < last {
        let hint = match open.typing() {
            true => keys.map_or(FALLBACK_TEXT_HINT, |keys| keys.ask_text()),
            false => keys.map_or(FALLBACK_HINT, |keys| keys.ask()),
        };
        screen.put_str_until(BAR + INDENT, row, hint, inks.dim, width);
    }
    (rows, cursor)
}

/// The hint of the card's rows, used when no keys file was read.
const FALLBACK_HINT: &str = "1-9 or up down pick \u{b7} type to answer \u{b7} enter answers \u{b7} tab next question \u{b7} esc skips";

/// The hint of a typed answer, used when no keys file was read.
const FALLBACK_TEXT_HINT: &str = "enter sends your answer \u{b7} ctrl+j new line \u{b7} esc clears";

/// The width of `text` in cells.
fn cells(text: &str) -> u16 {
    text.chars()
        .map(|c| c.width().unwrap_or(0) as u16)
        .fold(0u16, u16::saturating_add)
}

/// The rows `text` takes when it is wrapped at `right` columns from column `x`.
fn wrapped_rows(text: &str, x: u16, right: u16) -> u16 {
    let mut rows = 0;
    wrap(text, x, right, |_, _| rows += 1);
    rows.max(1)
}

/// Walks `text` wrapped at `right` columns from column `x`, calling `row` with
/// each row's number and its run: a newline starts a row of its own, a row
/// breaks at its last space when it has one, the spaces at a break are dropped,
/// and a word longer than a row breaks where the row ends.
fn wrap<'a>(text: &'a str, x: u16, right: u16, mut row: impl FnMut(u16, &'a str)) {
    let room = right.saturating_sub(x).max(1);
    let mut count = 0u16;
    for line in text.split('\n') {
        let mut rest = line;
        loop {
            let mut used = 0u16;
            let mut cut = rest.len();
            let mut space = None;
            for (at, c) in rest.char_indices() {
                let width = c.width().unwrap_or(0) as u16;
                if used.saturating_add(width) > room && at > 0 {
                    cut = at;
                    break;
                }
                if c == ' ' {
                    space = Some(at);
                }
                used = used.saturating_add(width);
            }
            if cut == rest.len() {
                row(count, rest);
                count = count.saturating_add(1);
                break;
            }
            let end = match space {
                _ if rest[cut..].starts_with(' ') => cut,
                Some(at) if at > 0 => at,
                _ => cut,
            };
            row(count, rest[..end].trim_end_matches(' '));
            count = count.saturating_add(1);
            rest = rest[end..].trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
        }
    }
}

/// Draws `text` from (`x`, `y`) wrapped at `right`, skipping its first `skip`
/// rows and drawing at most `room` of the rest, and answers the column and row
/// its end lands on.
#[expect(
    clippy::too_many_arguments,
    reason = "the wrap's place, its rows and its inks"
)]
fn put_wrapped(
    screen: &mut Screen,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    style: u32,
    skip: u16,
    room: u16,
) -> (u16, u16) {
    let mut end = (x, y);
    wrap(text, x, right, |row, run| {
        if row < skip {
            return;
        }
        let at = row.saturating_sub(skip);
        if room > 0 && at >= room {
            return;
        }
        end = (screen.put_str_until(x, y + at, run, style, right), y + at);
    });
    end
}

/// The decimal digits of `n`, written into the end of `buf` and read as text: a
/// count costs no allocation.
fn number(mut n: usize, buf: &mut [u8; DIGITS]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || at == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("0")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::client::{Initialized, Question, QuestionOption};
    use crate::screen::Grid;
    use crate::theme::{Mode, Rgb};

    use super::super::super::{APPROVAL_ARMED, App, Command, Item, QuestionBox};

    /// The columns and the rows of the test terminal: wide enough for the full
    /// screen frame and for a card of every row the tests draw.
    const COLUMNS: u16 = 120;
    const ROWS: u16 = 30;

    /// The background a dark terminal answers with.
    const DARK: Rgb = Rgb::new(0x14, 0x14, 0x14);

    /// Nothing pressed.
    const NONE: KeyModifiers = KeyModifiers::NONE;

    /// An app in full screen, its theme the terminal mode the app starts in.
    fn new_app() -> App {
        let init = Initialized {
            session: "sess-1".to_owned(),
            ..Initialized::default()
        };
        let mut app = App::new(&init, (COLUMNS, ROWS), true);
        app.restyle(Mode::Terminal);
        let _ = DARK;
        app
    }

    /// The questions of a card: which database, with two options, the second of
    /// which carries a note.
    fn database() -> Vec<Question<'static>> {
        vec![Question {
            q: "Which database?".into(),
            options: vec![
                QuestionOption {
                    label: "sqlite".into(),
                    note: None,
                },
                QuestionOption {
                    label: "postgres".into(),
                    note: Some("on the server".into()),
                },
            ],
            multi: false,
        }]
    }

    /// The questions of a card that takes several picks: two stores.
    fn stores() -> Vec<Question<'static>> {
        vec![Question {
            q: "Which stores?".into(),
            options: vec![
                QuestionOption {
                    label: "redis".into(),
                    note: None,
                },
                QuestionOption {
                    label: "lmdb".into(),
                    note: None,
                },
            ],
            multi: true,
        }]
    }

    /// Two questions, the second of which takes several picks.
    fn two() -> Vec<Question<'static>> {
        let mut questions = database();
        questions.extend(stores());
        questions
    }

    /// Puts one card on screen and draws the frame that shows it.
    fn show(app: &mut App, id: u64, questions: &[Question<'_>]) {
        app.question = Some(QuestionBox::new(id, questions, Instant::now()));
        frame(app);
    }

    /// Puts a card on screen, draws it, and ages it past [`APPROVAL_ARMED`]: the
    /// card as its keys find it.
    fn open(app: &mut App, questions: &[Question<'_>]) {
        show(app, 1, questions);
        armed(app);
    }

    /// Ages the card past [`APPROVAL_ARMED`], so its keys answer it.
    fn armed(app: &mut App) {
        let older = APPROVAL_ARMED + Duration::from_millis(1);
        let shown = Instant::now()
            .checked_sub(older)
            .expect("a clock past the arming wait");
        if let Some(open) = app.question.as_mut() {
            open.shown = shown;
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Types `text` one key at a time.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(key(KeyCode::Char(c), NONE));
        }
    }

    /// One frame, laid out as the loop's draw lays it out.
    fn frame(app: &mut App) -> Vec<String> {
        app.content_rows();
        app.paint();
        rows(app)
    }

    /// The text of one painted row of the main area, trailing blanks trimmed.
    fn row(grid: &Grid, y: u16, limit: u16) -> String {
        let mut text = String::new();
        for x in 0..limit.min(grid.width()) {
            match grid.cell(x, y).char() {
                Some(c) => text.push(c),
                None => text.push(' '),
            }
        }
        let keep = text.trim_end_matches(' ').len();
        text.truncate(keep);
        text
    }

    /// The text of every painted row of the main area, top down.
    fn rows(app: &App) -> Vec<String> {
        let grid = app.screen.back();
        let limit = app.main.width;
        (0..app.screen.rows())
            .map(|y| row(grid, y, limit))
            .collect()
    }

    /// The row the drawn screen shows `needle` on, when it shows it at all.
    fn at(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    /// The answer the card asked the client to send, and none when it sent none.
    fn answer(app: &mut App) -> Option<(u64, Option<Vec<Vec<String>>>)> {
        while let Some(command) = app.commands.pop_front() {
            if let Command::Answer { id, answers } = command {
                return Some((id, answers));
            }
        }
        None
    }

    /// The line the last answer left in the transcript.
    fn lines(app: &App) -> Vec<String> {
        app.pending
            .iter()
            .filter_map(|item| match item {
                Item::Answer { line, .. } => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    /// A card of two options, the second with a note, draws the header, the
    /// question, both numbered options and the row that types an answer.
    #[test]
    fn question_the_card_draws_the_options_and_the_own_row() {
        let mut app = new_app();
        open(&mut app, &database());
        let rows = frame(&mut app);
        let header = at(&rows, "? reagent asks").expect("the header draws");
        assert!(
            !rows[header].contains(" of "),
            "one question names no count: {:?}",
            rows[header]
        );
        assert!(at(&rows, "Which database?").is_some(), "{rows:?}");
        let first = at(&rows, "1 sqlite").expect("the first option draws");
        let second = at(&rows, "2 postgres").expect("the second option draws");
        assert_eq!(second, first + 1, "the options are numbered in order");
        assert!(
            rows[second].contains("on the server"),
            "the note of an option draws beside it: {:?}",
            rows[second]
        );
        assert!(
            at(&rows, "type your own answer...").is_some(),
            "the row that types an answer draws: {rows:?}"
        );
    }

    /// Two questions name the one on screen, and the count of them.
    #[test]
    fn question_two_questions_name_which_one_shows() {
        let mut app = new_app();
        open(&mut app, &two());
        let rows = frame(&mut app);
        assert!(at(&rows, "1 of 2").is_some(), "{rows:?}");
    }

    /// Down moves the pick to the second option, and Enter sends it.
    #[test]
    fn question_down_and_enter_send_the_pick() {
        let mut app = new_app();
        open(&mut app, &database());
        app.key(key(KeyCode::Down, NONE));
        frame(&mut app);
        app.key(key(KeyCode::Enter, NONE));
        let (id, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(id, 1);
        assert_eq!(answers, Some(vec![vec!["postgres".to_owned()]]));
    }

    /// A digit picks that option at once.
    #[test]
    fn question_a_digit_picks_the_option_at_once() {
        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "2");
        let rows = frame(&mut app);
        assert!(
            at(&rows, "Which database?").is_some(),
            "the digit answers nothing: {rows:?}"
        );
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["postgres".to_owned()]]));
    }

    /// The row that types an answer takes text with a new line from Ctrl+J, and
    /// Enter sends the text.
    #[test]
    fn question_the_own_answer_takes_text_and_enter_sends_it() {
        let mut app = new_app();
        open(&mut app, &database());
        app.key(key(KeyCode::Down, NONE));
        app.key(key(KeyCode::Down, NONE));
        app.key(key(KeyCode::Enter, NONE));
        assert!(
            answer(&mut app).is_none(),
            "Enter on an empty field sends nothing"
        );
        type_text(&mut app, "mysql");
        app.key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_text(&mut app, "percona");
        let rows = frame(&mut app);
        assert!(at(&rows, "mysql").is_some(), "{rows:?}");
        assert!(
            at(&rows, "percona").is_some(),
            "the new line draws: {rows:?}"
        );
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["mysql\npercona".to_owned()]]));
    }

    /// A question that takes several picks checks with Space, and Enter sends
    /// every checked label.
    #[test]
    fn question_space_checks_rows_and_enter_sends_both() {
        let mut app = new_app();
        open(&mut app, &stores());
        let rows = frame(&mut app);
        assert!(at(&rows, "[ ] redis").is_some(), "{rows:?}");
        app.key(key(KeyCode::Char(' '), NONE));
        app.key(key(KeyCode::Down, NONE));
        app.key(key(KeyCode::Char(' '), NONE));
        let rows = frame(&mut app);
        assert!(at(&rows, "[x] redis").is_some(), "{rows:?}");
        assert!(at(&rows, "[x] lmdb").is_some(), "{rows:?}");
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(
            answers,
            Some(vec![vec!["redis".to_owned(), "lmdb".to_owned()]])
        );
    }

    /// Tab shows the next question, and the Enter on the last one sends one list
    /// per question.
    #[test]
    fn question_tab_moves_on_and_the_last_enter_sends_every_answer() {
        let mut app = new_app();
        open(&mut app, &two());
        app.key(key(KeyCode::Tab, NONE));
        let rows = frame(&mut app);
        assert!(at(&rows, "2 of 2").is_some(), "{rows:?}");
        assert!(at(&rows, "Which stores?").is_some(), "{rows:?}");
        app.key(key(KeyCode::Char(' '), NONE));
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(
            answers,
            Some(vec![vec!["sqlite".to_owned()], vec!["redis".to_owned()]]),
            "one list per question, in the order they were asked"
        );
    }

    /// Esc skips the whole request.
    #[test]
    fn question_esc_skips_the_request() {
        let mut app = new_app();
        open(&mut app, &two());
        app.key(key(KeyCode::Esc, NONE));
        let (id, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(id, 1);
        assert_eq!(answers, None, "a skip sends no answers");
        assert!(app.question.is_none(), "the card closes");
        frame(&mut app);
    }

    /// The history keeps one line per question, with the question and the
    /// answer, and the card goes.
    #[test]
    fn question_the_answer_leaves_one_line_per_question() {
        let mut app = new_app();
        open(&mut app, &two());
        app.key(key(KeyCode::Down, NONE));
        app.key(key(KeyCode::Enter, NONE));
        app.key(key(KeyCode::Enter, NONE));
        let _ = answer(&mut app);
        assert_eq!(
            lines(&app),
            vec![
                "? Which database? -> postgres".to_owned(),
                "? Which stores? -> (no answer)".to_owned(),
            ]
            .into_iter()
            .collect::<Vec<String>>(),
            "one line per question"
        );
        let _ = frame(&mut app);
    }

    /// A resolved question for the open id closes the card, wherever the answer
    /// came from.
    #[test]
    fn question_resolved_closes_the_open_card() {
        let mut app = new_app();
        open(&mut app, &database());
        assert!(app.question.is_some());
        app.question_resolved(1);
        assert!(app.question.is_none(), "the card closes");
        assert!(answer(&mut app).is_none(), "no answer is sent");
    }

    /// A resolved question for another id leaves the open card on screen.
    #[test]
    fn question_resolved_for_another_id_keeps_the_card() {
        let mut app = new_app();
        open(&mut app, &database());
        app.question_resolved(2);
        assert!(app.question.is_some(), "the card stays");
    }

    /// A question that arrives behind an approval card waits for it, and shows
    /// once the approval is answered.
    #[test]
    fn question_waits_behind_an_approval_card() {
        let mut app = new_app();
        let width = app.text_width();
        app.approval = Some(super::super::super::ApprovalBox::new(
            crate::client::Approval {
                id: 9,
                tool: "bash".into(),
                call: "make".into(),
                rule: "default".into(),
                save: false,
            },
            width,
        ));
        app.question_request(1, &database());
        assert!(app.question.is_none(), "the approval keeps the screen");
        assert_eq!(app.questions.len(), 1, "the question waits");
        app.resolved(9);
        assert!(app.question.is_some(), "the question shows after it");
    }

    /// The status line says the question waits for the person, with the time on
    /// it.
    #[test]
    fn question_the_status_line_asks_you() {
        let mut app = new_app();
        open(&mut app, &database());
        let rows = frame(&mut app);
        let status = rows.last().map(String::as_str).unwrap_or_default();
        assert!(
            status.contains("? reagent asks you"),
            "the status line names the question: {status:?}"
        );
        assert!(
            status.contains("paused"),
            "the status line says the run waits: {status:?}"
        );
    }

    /// The status line goes back to the text box once the card is answered.
    #[test]
    fn question_the_status_line_stops_asking() {
        let mut app = new_app();
        open(&mut app, &database());
        let _ = frame(&mut app);
        app.key(key(KeyCode::Esc, NONE));
        let rows = frame(&mut app);
        assert!(
            at(&rows, "? reagent asks you").is_none(),
            "the status line stops asking: {rows:?}"
        );
    }

    /// A steady frame of the card grows nothing it holds.
    #[test]
    fn question_the_frame_holds_its_buffers() {
        let mut app = new_app();
        show(&mut app, 1, &two());
        frame(&mut app);
        let cells = app.screen.back().cells_capacity();
        let (styles, graphemes) = (app.screen.style_count(), app.screen.grapheme_count());
        for _ in 0..8 {
            frame(&mut app);
        }
        let grid = app.screen.back();
        assert_eq!(grid.cells_capacity(), cells, "no cell is added");
        assert_eq!(app.screen.style_count(), styles, "no style is added");
        assert_eq!(
            app.screen.grapheme_count(),
            graphemes,
            "no grapheme is added"
        );
    }

    /// The picked row carries the `>` marker, and the other rows do not.
    #[test]
    fn question_the_picked_row_carries_the_marker() {
        let mut app = new_app();
        open(&mut app, &database());
        let rows = frame(&mut app);
        let first = at(&rows, "1 sqlite").expect("the first option draws");
        let second = at(&rows, "2 postgres").expect("the second option draws");
        assert!(rows[first].contains("> "), "{:?}", rows[first]);
        assert!(!rows[second].contains('>'), "{:?}", rows[second]);
        app.key(key(KeyCode::Down, NONE));
        let rows = frame(&mut app);
        assert!(!rows[first].contains('>'), "{:?}", rows[first]);
        assert!(rows[second].contains("> "), "{:?}", rows[second]);
    }

    /// In a rich theme the card's text sits on the card's tint, so the tint
    /// has no holes where the words are.
    #[test]
    fn question_the_text_keeps_the_tint_behind_it() {
        let mut app = new_app();
        app.restyle(Mode::Rich {
            background: DARK,
            dark: true,
        });
        let tint = app.style(
            crate::theme::Role::CardAccent,
            crate::theme::Emphasis::Plain,
        );
        if tint == app.screen.default_style() {
            // `NO_COLOR` in the test environment: no mode draws a fill.
            return;
        }
        open(&mut app, &database());
        let rows = frame(&mut app);
        let y = at(&rows, "Which database?").expect("the question draws");
        let x = rows[y].find("Which").expect("its column") as u16;
        let on = app.style_on(
            crate::theme::Role::Text,
            crate::theme::Emphasis::Bold,
            crate::theme::Role::CardAccent,
        );
        assert_ne!(
            on,
            app.style(crate::theme::Role::Text, crate::theme::Emphasis::Bold)
        );
        assert_eq!(app.screen.back().cell(x, y as u16).style(), on);
        let note = at(&rows, "on the server").expect("the note draws");
        let x = rows[note].find("on the").expect("its column") as u16;
        let dim = app.style_on(
            crate::theme::Role::Dim,
            crate::theme::Emphasis::Plain,
            crate::theme::Role::CardAccent,
        );
        assert_eq!(app.screen.back().cell(x, note as u16).style(), dim);
    }

    /// A letter jumps into the answer field and starts the answer, and Enter
    /// sends it.
    #[test]
    fn question_a_letter_starts_the_own_answer() {
        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "duckdb");
        let rows = frame(&mut app);
        assert!(at(&rows, "duckdb").is_some(), "{rows:?}");
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["duckdb".to_owned()]]));
    }

    /// Esc leaves the field and keeps the card; a second Esc skips it.
    #[test]
    fn question_esc_leaves_the_field_then_skips() {
        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "x");
        app.key(key(KeyCode::Esc, NONE));
        assert!(app.question.is_some(), "the first Esc keeps the card");
        assert!(answer(&mut app).is_none(), "nothing is sent yet");
        let rows = frame(&mut app);
        assert!(at(&rows, "type your own answer...").is_some(), "{rows:?}");
        app.key(key(KeyCode::Esc, NONE));
        let (_, answers) = answer(&mut app).expect("the second Esc skips");
        assert_eq!(answers, None);
    }

    /// A letter in a question that takes several answers checks the field, so
    /// the typed text joins the checked labels.
    #[test]
    fn question_a_letter_in_a_multi_question_joins_the_checked() {
        let mut app = new_app();
        open(&mut app, &stores());
        type_text(&mut app, " ");
        type_text(&mut app, "rocksdb");
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(
            answers,
            Some(vec![vec!["redis".to_owned(), "rocksdb".to_owned()]])
        );
    }

    /// In a rich theme the answer field carries the text box's fill to the
    /// card's right edge, before anything is typed.
    #[test]
    fn question_the_answer_field_has_the_text_box_fill() {
        let mut app = new_app();
        app.restyle(Mode::Rich {
            background: DARK,
            dark: true,
        });
        let fill = app.style(crate::theme::Role::InputFill, crate::theme::Emphasis::Plain);
        if fill == app.screen.default_style() {
            return;
        }
        open(&mut app, &database());
        let rows = frame(&mut app);
        let y = at(&rows, "type your own answer...").expect("the field draws") as u16;
        let right = app.text_width() - 2;
        assert_eq!(app.screen.back().cell(right, y).style(), fill);
    }

    /// The answer field reads the text box's readline keys: Ctrl+A goes to the
    /// start of the line and Ctrl+W cuts the word before the cursor.
    #[test]
    fn question_the_field_reads_readline_keys() {
        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "duck db");
        app.key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        type_text(&mut app, "lake");
        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        type_text(&mut app, "my ");
        app.key(key(KeyCode::Left, NONE));
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["my duck lake".to_owned()]]));
    }

    /// Up on the first row of the field moves the pick to the last option and
    /// keeps the typed text for when the field is picked again.
    #[test]
    fn question_up_leaves_the_field_and_keeps_its_text() {
        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "duckdb");
        app.key(key(KeyCode::Up, NONE));
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["postgres".to_owned()]]));

        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "duckdb");
        app.key(key(KeyCode::Up, NONE));
        app.key(key(KeyCode::Down, NONE));
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["duckdb".to_owned()]]));
    }

    /// A paste while the field is picked goes into the field, not the text box.
    #[test]
    fn question_a_paste_goes_into_the_field() {
        let mut app = new_app();
        open(&mut app, &database());
        type_text(&mut app, "x");
        app.paste("tidb".to_owned());
        assert!(app.editor.is_empty(), "the text box takes nothing");
        app.key(key(KeyCode::Enter, NONE));
        let (_, answers) = answer(&mut app).expect("the card answers");
        assert_eq!(answers, Some(vec![vec!["xtidb".to_owned()]]));
    }

    /// The field draws as a small text box: its bar and its `>` on the row of
    /// the placeholder, under a blank row.
    #[test]
    fn question_the_field_draws_as_a_text_box() {
        let mut app = new_app();
        open(&mut app, &database());
        let rows = frame(&mut app);
        let y = at(&rows, "type your own answer...").expect("the field draws");
        assert!(
            rows[y].contains("\u{258e} > type your own answer..."),
            "{:?}",
            rows[y]
        );
        for pad in [y - 1, y + 1] {
            assert_eq!(
                rows[pad].trim_end(),
                "\u{258e}  \u{258e}",
                "a pad row of the field"
            );
        }
        assert!(
            rows[y - 2].trim_start_matches('\u{258e}').trim().is_empty(),
            "a blank row above the field: {:?}",
            rows[y - 2]
        );
    }

    /// A long question wraps at a space, so no row starts with one, and the
    /// options start on the row after its last row, with no gap.
    #[test]
    fn question_a_long_question_wraps_at_words() {
        let mut app = new_app();
        let mut questions = database();
        questions[0].q = "Which language should the todo tool be written in, given that this folder already holds a Cargo workspace with many crates?".into();
        open(&mut app, &questions);
        let rows = frame(&mut app);
        let first = at(&rows, "Which language").expect("the question draws");
        let second = rows[first + 1].trim_start_matches('\u{258e}');
        assert!(
            second.starts_with("  ") && !second.starts_with("   "),
            "{second:?}"
        );
        let options = at(&rows, "1 sqlite").expect("the options draw");
        assert_eq!(
            options,
            first + 2,
            "the options follow the question's last row: {rows:#?}"
        );
    }
}
